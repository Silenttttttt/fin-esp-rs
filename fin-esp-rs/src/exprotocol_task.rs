//! Additive-only ExProtocol test endpoint. Runs alongside everything else Fin-ESP already
//! does -- one more spawned thread listening on its own port, same shape as
//! `spawn_media_server`/`spawn_mic_server` in `main.rs`. Doesn't touch any existing
//! display/lamp/weather logic. Exists to let a peer on the LAN (the desktop this firmware was
//! built on) do a real ECDH+PoW handshake and exchange data with actual ESP32 hardware, as the
//! hardware-in-the-loop counterpart to `esp-protocol`'s desktop test suite.

use esp_idf_hal::delay::FreeRtos;
use log::{error, info, warn};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use exprotocol::node::{ExProtocolNode, NodeConfig};
use exprotocol::stream::StreamManager;
use exprotocol::transport::PolledTcpTransport;

const EXPROTOCOL_PORT: u16 = 9878;
// A real control test on this device proved `ExProtocolNode::new()`'s internal recv thread --
// even at just 4096 bytes -- was the difference between lampBridge's own 16 KiB stack request
// succeeding and it retrying forever (~27 KiB free heap without this thread vs ~15-16 KiB with
// it, right at lampBridge's own requirement). `new_unthreaded()` + `pump()` removes that second
// thread entirely: this one thread drives both its own loop AND the protocol's receive/dispatch
// work.
//
// Switched from `TcpTransport` (spawns a NEW OS thread per accepted connection) to
// `PolledTcpTransport` (zero extra threads -- `pump()`'s own call to `recv()` does the
// non-blocking accept+read polling itself). Confirmed on real hardware as the direct cause of
// "accepted then silently closed" connections: even with `TcpTransport`'s reader-thread spawn
// already failing gracefully (not panicking) under heap pressure, the ATTEMPT itself was a real
// stack-size heap allocation this device couldn't always spare at accept time, on top of
// whatever `DISPATCH_THREAD_STACK` was already using. `PolledTcpTransport` needs no separate
// reader-thread stack size at all -- one fewer parameter, one fewer point of contention.
//
// This thread's OWN stack must cover the real crypto call depth `pump()` runs on it (ECDH,
// PoW verification, AES-GCM key derivation via `dispatch()`) -- confirmed the hard way on real
// hardware: 4096, 12288, 16384, and 20480 all produced a genuine "stack overflow in task
// pthread" crash (not a graceful error) partway through a real handshake, every time
// (16384/20480 looked safe in an earlier, less rigorous pass that only checked "did it crash in
// this one narrow window" -- retested properly and both crashed). 26624 held with zero crashes
// across many real handshake attempts and was kept as the known-safe baseline for a long time.
//
// Reduced 26624 -> 22528 (2026-08-29, "how low can we go" pass), for the first time backed by a
// REAL live measurement rather than more size-guessing: added `uxTaskGetStackHighWaterMark`
// logging to this thread (see the periodic check inside the pump loop below) and ran 6 separate
// real handshakes across two boots -- every one bottomed out in the same tight range
// (19968-20056 bytes free of 26624), meaning real usage is a stable ~6.6 KB, not the "needs
// generous headroom, don't know exactly how much" situation this constant was set under
// originally. Also confirmed via `exprotocol-rs` source (`crypto.rs`/`session.rs`) that the
// ESP32's own role here only ever calls `verify_pow()` (a single, constant-time hash check) --
// the OTHER function, `solve_pow()` (an unbounded iterative search whose real depth could
// plausibly vary a lot run-to-run), runs on the connecting PEER, not on this device. That
// meaningfully de-risks trusting a small number of real measurements here, since the one
// operation most likely to have genuine run-to-run variance isn't even executing on this thread.
//
// Still being deliberately conservative rather than jumping straight to the ~6.6 KB measured
// floor: picked a value with real margin above BOTH the measured usage (3.4x) and the last
// known-bad value, 20480 (+2048 headroom), rather than retrying an exact previously-crashed
// number or assuming 6 handshakes on one build represent the true worst case for good -- this
// file has been burned before by declaring victory from too small an observation window (see
// the DISPATCH_THREAD_STACK 16384/20480 "looked safe, wasn't" episode this exact comment already
// describes, and the main_task equivalent lesson in project memory). Verify further, and only
// step lower again with more real evidence, rather than pushing straight to the measured limit.
const DISPATCH_THREAD_STACK: usize = 22528;
// Tried delaying this thread's start (to dodge the boot-time thread-spawning burst) -- measured
// worse on real hardware: heap is at its fullest (~170 KiB) in the first couple seconds after
// boot, before the first fetch cycle's TLS churn eats into it. A fixed delay reliably landed
// bind+node-creation mid-fetch-cycle instead, at 10-27 KiB free, and reset every single cycle.
// Starting immediately, unconditionally, wins the race for heap while it's still there.

fn log_heap(tag: &str) {
    unsafe {
        info!(
            "[exprotocol] heap@{tag}: free={} min_free={}",
            esp_idf_sys::esp_get_free_heap_size(),
            esp_idf_sys::esp_get_minimum_free_heap_size()
        );
    }
}

// `network_lock` is the same mutex `main.rs` already has finFetch (HTTPS/TLS) and the Tuya lamp
// bridge (TCP) hold for the full duration of their own network-heavy sections, specifically to
// stop those two from ever running concurrently -- a real, previously-confirmed crash cause on
// this device ("alloc(N bytes) failed" heap exhaustion when their TLS/TCP work overlapped). This
// thread's own crypto (ECDH/PoW/AES-GCM key derivation via `dispatch()`) is exactly the same
// class of heap-hungry work, so it joins the same lock rather than inventing a separate
// mechanism. A prior attempt at a free-heap-threshold gate here was real hardware-tested and
// rejected: fin-esp-rs's fetch cycle keeps free heap pinned around 10-18 KB for effectively the
// entire time it's running (not just briefly), and a concurrent TLS handshake can swing free heap
// by 100+ KB within tens of milliseconds -- any static threshold high enough to be protective
// blocked this thread from ever pumping at all, and any threshold low enough to let it run
// regularly gave no real protection against that swing. Locking the actual contended resource
// fixes both problems at once.
pub fn spawn_exprotocol_server(network_lock: Arc<Mutex<()>>) {
    // Found via code review: this outer spawn's own `.expect()` was unprotected, unlike every
    // other spawn in this codebase (`finFetch`/`lampBridge`/`fetchA`/`fetchB`/`ota`, all
    // retry-with-backoff). Boot-time only (lower risk than the OTA/events-worker fixes since
    // it runs once, early, while heap is still near its post-boot peak), but a real,
    // reachable-in-principle abort path all the same -- matching the established pattern rather
    // than leaving it as the one inconsistent spawn.
    loop {
        let network_lock = Arc::clone(&network_lock);
        match std::thread::Builder::new()
            .name("exprotocol".into())
            .stack_size(DISPATCH_THREAD_STACK)
            .spawn(move || {
            loop {
                log_heap("before-bind");
                let addr = SocketAddr::from(([0, 0, 0, 0], EXPROTOCOL_PORT));
                // No per-connection reader thread here at all -- PolledTcpTransport does
                // non-blocking accept+read polling entirely inside this task's own recv()/pump()
                // call, on this thread's existing DISPATCH_THREAD_STACK. Nothing else to size.
                let transport = match PolledTcpTransport::new(Some(addr)) {
                    Ok(t) => t,
                    Err(e) => {
                        error!("[exprotocol] bind err: {e}, retrying");
                        FreeRtos::delay_ms(2000);
                        continue;
                    }
                };
                info!("[exprotocol] listening on :{EXPROTOCOL_PORT}");
                // NodeConfig::embedded() bundles the ESP32-appropriate overrides this task used
                // to set one-off (smaller max_payload_offered, far fewer max_connections) into
                // one named preset in the crate itself, so this file doesn't have to rediscover
                // them -- recv_thread_stack_size in it is unused here since new_unthreaded()
                // never spawns that thread.
                let config = NodeConfig::embedded();
                log_heap("before-node-new");
                let node = ExProtocolNode::new_unthreaded(
                    transport,
                    config,
                    Arc::new(StreamManager::new(Default::default())),
                    Some(Box::new(|data: &[u8], session_id: u64| {
                        info!("[exprotocol] received {} bytes on session {session_id}", data.len());
                    })),
                );
                log_heap("after-node-new");
                // No internal recv thread in unthreaded mode -- this thread's own loop IS the
                // protocol's receive/dispatch loop. One heap check per pump round is too often
                // to log; sample roughly once a minute instead.
                let mut since_last_heap_log = Duration::ZERO;
                loop {
                    // Real hardware caught the actual crash mechanism here: "memory allocation
                    // of N bytes failed" -> abort(), NOT a stack overflow, despite ESP-IDF's
                    // reset-reason classifier lumping both under "PANIC (stack overflow or
                    // abort)" -- that ambiguity is what led earlier cycles to keep tuning
                    // DISPATCH_THREAD_STACK (a stack-size knob) against what was actually a
                    // heap-exhaustion race with finFetch's concurrent TLS churn. Holding
                    // `network_lock` for each short pump call keeps that crypto work from ever
                    // overlapping with finFetch/lamp's own network-heavy sections.
                    //
                    // Real bug found live (2026-09-16): this used to pump for 100ms then sleep
                    // only 20ms -- an ~83% duty cycle on a lock the Tuya lamp bridge also needs
                    // for every button press and its 5s status refresh. `std::sync::Mutex` has
                    // no fairness guarantee, so lampBridge (itself only sleeping 20ms between
                    // lock attempts) regularly lost the race back to this thread's near-constant
                    // re-locking, surfacing as the lamp toggle feeling laggy ALL the time, not
                    // occasionally -- this test endpoint has no real latency requirement (a
                    // human or script connecting to it won't notice a few hundred ms), so it can
                    // afford to poll far less aggressively and get out of the lamp's way: 15ms
                    // pump + 300ms sleep is roughly a 5% duty cycle instead of 83%.
                    {
                        let _net_guard = network_lock.lock().unwrap();
                        // `pump()` used to have dispatch errors silently discarded inside the
                        // crate itself -- fixed upstream (exprotocol-rs) specifically because a
                        // real handshake was observed connecting, retrying for 90+ seconds, and
                        // never completing with zero error output anywhere. Logging this is what
                        // will actually reveal why, instead of continuing to guess blind.
                        if let Err(e) = node.pump(Duration::from_millis(15)) {
                            warn!("[exprotocol] pump/dispatch error: {e}");
                        }
                    }
                    FreeRtos::delay_ms(300);
                    since_last_heap_log += Duration::from_millis(315);
                    if since_last_heap_log >= Duration::from_secs(60) {
                        since_last_heap_log = Duration::ZERO;
                        log_heap("keepalive");
                        // Real measurement, not another guess: DISPATCH_THREAD_STACK's current
                        // 26624 value came from a real binary search (4096/12288/16384/20480 all
                        // produced genuine stack-overflow crashes mid-handshake, even after this
                        // build's own opt-level=3 override for the exprotocol crate), but that
                        // search only ever asked "does this size crash," never "how much does the
                        // real call depth actually use." Logging the true high-water-mark here so
                        // a future right-sizing has real data instead of another blind guess.
                        let hwm = unsafe { esp_idf_sys::uxTaskGetStackHighWaterMark(std::ptr::null_mut()) };
                        info!("[exprotocol] dispatch thread stack high-water-mark: {hwm} bytes free (of {DISPATCH_THREAD_STACK})");
                    }
                }
            }
        }) {
            Ok(_) => break,
            Err(e) => {
                error!("[boot] exprotocol spawn failed: {e}, retrying in 500ms");
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }
}
