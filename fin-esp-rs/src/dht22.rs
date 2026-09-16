//! DHT22/AM2302 temperature+humidity sensor driver -- single-wire, bit-banged, no external
//! crate. Written by hand rather than pulling in a generic embedded-hal DHT driver so the
//! precise timing loops run directly against this project's own delay/timer primitives
//! (`Ets::delay_us`, `esp_timer_get_time`), already proven reliable elsewhere in this
//! codebase, instead of trusting an unfamiliar crate's timing on this exact hardware.

use esp_idf_hal::delay::Ets;
use esp_idf_hal::gpio::{InputOutput, PinDriver, Pull};
use std::time::Duration;

#[derive(Debug)]
pub enum Dht22Error {
    Timeout,
    ChecksumMismatch,
    Gpio(esp_idf_sys::EspError),
}

pub struct Dht22Reading {
    pub temp_c: f32,
    pub humidity_pct: f32,
}

fn now_us() -> i64 {
    unsafe { esp_idf_sys::esp_timer_get_time() }
}

/// Busy-waits while the pin reads `high`, returning the elapsed microseconds once it flips,
/// or `Timeout` if it never does within `timeout_us`. Used both to consume a phase whose exact
/// length doesn't matter (the response low/high pulses) and to measure one that does (each
/// bit's high-phase length, which is what actually encodes the bit's value).
fn wait_while(
    pin: &PinDriver<'_, InputOutput>,
    high: bool,
    timeout_us: i64,
) -> Result<i64, Dht22Error> {
    let start = now_us();
    loop {
        if pin.is_high() != high {
            return Ok(now_us() - start);
        }
        if now_us() - start > timeout_us {
            return Err(Dht22Error::Timeout);
        }
    }
}

/// One full read cycle. Real hardware timing (AM2302 datasheet): host pulls low >=1ms then
/// releases, sensor acks with an 80us low + 80us high pulse, then sends 40 bits, each a ~50us
/// low phase (ignored) followed by a high phase whose length IS the bit -- ~26-28us for a 0,
/// ~70us for a 1. Every timeout below is well above the spec max for its phase, generous
/// enough to absorb normal FreeRTOS scheduling jitter on a thread that isn't real-time
/// priority, while still catching a genuinely disconnected/dead sensor quickly.
pub fn read(pin: &mut PinDriver<'_, InputOutput>) -> Result<Dht22Reading, Dht22Error> {
    pin.set_low().map_err(Dht22Error::Gpio)?;
    Ets::delay_us(2000);
    pin.set_high().map_err(Dht22Error::Gpio)?;
    Ets::delay_us(30);

    wait_while(pin, true, 100)?; // sensor pulls low to start its ack
    wait_while(pin, false, 100)?; // ack low phase ends
    wait_while(pin, true, 100)?; // ack high phase ends -> first bit's low phase starts

    let mut bytes = [0u8; 5];
    for i in 0..40 {
        wait_while(pin, false, 100)?; // consume the ~50us low phase
        let high_us = wait_while(pin, true, 100)?; // measure the bit-encoding high phase
        let bit = if high_us > 40 { 1u8 } else { 0u8 };
        bytes[i / 8] = (bytes[i / 8] << 1) | bit;
    }

    let checksum = bytes[0]
        .wrapping_add(bytes[1])
        .wrapping_add(bytes[2])
        .wrapping_add(bytes[3]);
    if checksum != bytes[4] {
        return Err(Dht22Error::ChecksumMismatch);
    }

    let humidity_pct = (((bytes[0] as u16) << 8) | bytes[1] as u16) as f32 / 10.0;
    let temp_raw = (((bytes[2] & 0x7F) as u16) << 8) | bytes[3] as u16;
    // Written as a single division + a separate negation (not two divisions differing only in
    // sign) -- the latter tripped a real Xtensa LLVM backend codegen bug (a `[2 x float]`
    // constant-pool lowering it couldn't handle, crashing rustc with SIGSEGV/SIGILL, confirmed
    // via a real build on this exact toolchain). This form compiles clean.
    let mut temp_c = temp_raw as f32 / 10.0;
    if bytes[2] & 0x80 != 0 {
        temp_c = -temp_c;
    }

    Ok(Dht22Reading { temp_c, humidity_pct })
}

/// Last-known-good reading, shared with the web status handler. `f32::NAN` sentinel means "no
/// successful read yet" -- matches this codebase's existing sentinel-value convention (e.g.
/// `VOLUME_PCT`'s 255) rather than introducing an `Option`/lock for two floats.
use std::sync::atomic::{AtomicU32, Ordering};
pub static LAST_TEMP_C: AtomicU32 = AtomicU32::new(u32::MAX);
pub static LAST_HUMIDITY_PCT: AtomicU32 = AtomicU32::new(u32::MAX);

pub fn last_reading() -> Option<(f32, f32)> {
    let t = LAST_TEMP_C.load(Ordering::Relaxed);
    let h = LAST_HUMIDITY_PCT.load(Ordering::Relaxed);
    if t == u32::MAX || h == u32::MAX {
        None
    } else {
        Some((f32::from_bits(t), f32::from_bits(h)))
    }
}

// Read cadence and storage cadence are DELIBERATELY decoupled (2026-08-30): storage (the
// Postgres/device-events write below) always stays at 30s, matching the PC tray app's own
// `~/temp_log.csv` cadence so the two remain point-for-point correlatable -- but the LIVE
// value `last_reading()` exposes to `/status` can update much faster while someone's actually
// watching the web dashboard, without that faster sampling ever touching storage cadence.
const FAST_READ_INTERVAL_MS: u32 = 2000; // AM2302 datasheet's own recommended minimum sampling
                                          // interval -- reading faster than this risks
                                          // self-heating/inaccurate readings, so this is the
                                          // fastest "live" cadence gets, not an arbitrary choice.
const SLOW_READ_INTERVAL_MS: u32 = 30_000; // no point reading faster than storage cadence when
                                            // nobody's watching -- nothing would ever see it.
const STORE_INTERVAL_MS: u32 = 30_000;
// Page polls `/status` every 3s while open (see web.rs's own `setInterval(refresh,3000)`) --
// 6s (2x that) is a comfortable margin against one missed poll before falling back to the slow
// cadence, without lingering "fast" for long after a tab is actually closed.
const ACTIVE_WINDOW_MS: u32 = 6000;

// AtomicU64 isn't available on this target (Xtensa has no native 64-bit atomic instructions) --
// AtomicU32 millis wraps after ~49 days of continuous uptime, which this device has never come
// close to (its own well-documented WiFi/reboot flakiness alone guarantees more frequent
// reboots than that).
static LAST_ACTIVE_MS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn now_ms() -> u32 {
    (now_us() / 1000) as u32
}

// Set by `spawn_chart_cache_refresher`'s own loop, at the top of every iteration, before any
// network I/O -- see that function's own comment for the real live hang this exists to bound.
static REFRESHER_HEARTBEAT_MS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// How long since the chart-cache-refresher thread last completed a loop iteration -- called
/// from main.rs's own loop, which stays healthy independent of this thread, to decide whether
/// to force a full reboot.
pub fn refresher_stuck_for_ms() -> Option<u32> {
    let last = REFRESHER_HEARTBEAT_MS.load(Ordering::Relaxed);
    let now = now_ms();
    if last == 0 {
        // Never reported in yet -- either a normal brief startup window, OR the thread's own
        // `Builder::spawn` failed outright and it never exists at all for the rest of this
        // boot (a real, confirmed risk on this device under heap pressure, same as every
        // other thread spawn here -- and until this fix, that ALSO meant this exact watchdog
        // could never catch it, since `0` looked identical to "hasn't started yet"). 30s after
        // boot is a generous startup allowance -- a real successful spawn populates the cache
        // in well under that -- so still-zero past that point means treat it as stuck too.
        return if now >= 30_000 { Some(now) } else { None };
    }
    Some(now.wrapping_sub(last))
}

/// Called from web.rs's `/status` handler -- the one endpoint that's ONLY ever polled while
/// the dashboard page is actually open (every 3s, via its own JS). Lets the reader loop below
/// switch to `FAST_READ_INTERVAL_MS` for as long as that keeps happening, falling back to the
/// slow cadence the moment it stops (tab closed, page not open).
pub fn mark_dashboard_active() {
    LAST_ACTIVE_MS.store(now_ms(), std::sync::atomic::Ordering::Relaxed);
}

/// Spawned once at boot. Deliberately NOT sharing a
/// thread with anything else, since this driver spends its whole time in tight busy-wait
/// polling loops (worst case a bit under 5ms per successful read, longer on a timeout) that
/// would otherwise delay unrelated work sharing the same thread. A failed read (checksum
/// mismatch, timeout) just logs and retries next cycle -- same "transient failures are normal,
/// don't escalate" approach as every other periodic task in this codebase.
pub fn spawn_dht22_reader(pin: esp_idf_hal::gpio::Gpio32<'static>) {
    use esp_idf_hal::cpu::Core;
    use esp_idf_hal::task::thread::ThreadSpawnConfiguration;
    // Pinned to Core1 (APP CPU), away from Core0 where WiFi's own driver task and main_task
    // both run. Real hardware evidence this matters: WiFi beacon-timeout disassociations (a
    // pre-existing, already-documented characteristic of this device/AP) got noticeably more
    // frequent in the same session this driver was added, and this thread's `read()` spends up
    // to ~5ms per attempt in a tight, non-yielding busy-wait loop (required for the DHT22
    // protocol's sub-100us timing measurements) -- exactly the kind of thing that can delay
    // Core0's own beacon processing just enough to matter, even with strong RSSI ruling out a
    // signal-strength explanation. Reset back to the default config immediately after spawning
    // so this doesn't leak into any other thread spawned later in the program.
    let _ = ThreadSpawnConfiguration {
        pin_to_core: Some(Core::Core1),
        ..Default::default()
    }
    .set();
    let spawn_result = std::thread::Builder::new()
        .name("dht22".into())
        .stack_size(4096)
        .spawn(move || {
            // Internal pull-up as a safety net in addition to whichever pull-up (onboard the
            // AM2302 breakout module, or a discrete external resistor for a bare sensor) is
            // already on the physical line -- doesn't hurt either way.
            let mut driver = match PinDriver::input_output_od(pin, Pull::Up) {
                Ok(d) => d,
                Err(e) => {
                    log::error!("[dht22] failed to init GPIO32 as input-output-od: {e}");
                    return;
                }
            };
            let mut last_store_ms: u32 = 0;
            loop {
                match read(&mut driver) {
                    Ok(r) => {
                        log::info!(
                            "[dht22] temp={:.1}C humidity={:.1}%",
                            r.temp_c,
                            r.humidity_pct
                        );
                        LAST_TEMP_C.store(r.temp_c.to_bits(), Ordering::Relaxed);
                        LAST_HUMIDITY_PCT.store(r.humidity_pct.to_bits(), Ordering::Relaxed);
                        // Storage stays on its own fixed 30s cadence regardless of how often
                        // the read loop itself runs above (see this fn's own doc comment) --
                        // durable history lives in Postgres via the same `device-events`
                        // pipeline every other event in this firmware already uses (one
                        // persistent worker thread, not a new one per reading) -- NOT a local
                        // RAM ring buffer, which would be lost on every reboot and cost real
                        // static memory on an already heap-tight device. `event-consumer`'s
                        // activator keeps it scaled up (rather than spawning per-message) as
                        // long as readings keep arriving steadily, which they do every 30s.
                        let now = now_ms();
                        if now.saturating_sub(last_store_ms) >= STORE_INTERVAL_MS {
                            last_store_ms = now;
                            // Real coredump-confirmed whole-device abort (2026-08-30, caught
                            // while OTA-flashing over the network): `serde_json::json!()`'s
                            // underlying `Map` (a `BTreeMap`) allocates each entry via a plain
                            // `Box::new`, with NO fallible path -- unlike `Vec`'s `try_reserve`
                            // used everywhere else in this codebase. Full GDB backtrace bottomed
                            // exactly here: `alloc::collections::btree::map::entry::VacantEntry::
                            // insert_entry -> handle_alloc_error -> abort` -- this thread's own
                            // tiny 2-field object aborted the WHOLE DEVICE simply because OTA's
                            // concurrent flash-write heap pressure happened to peak at the same
                            // moment. Same mitigation as `web.rs`'s `BufReader::with_capacity`
                            // guard (that file's own established pattern for exactly this "no
                            // fallible API exists" situation): a same-instant free-heap check
                            // right before the allocation, skipping this one report (retried
                            // automatically next cycle) instead of gambling on it.
                            //
                            // Checks the largest CONTIGUOUS free block, not total free heap --
                            // corrected 2026-08-30 after a second real coredump showed a sibling
                            // guard (web.rs's build_status) still aborting even with a
                            // total-free-heap check in place: total free heap can look healthy
                            // while badly fragmented, this device's own well-documented
                            // recurring failure mode (see project_esp32_heap_budget.md).
                            if unsafe { esp_idf_sys::heap_caps_get_largest_free_block(esp_idf_sys::MALLOC_CAP_8BIT) } >= 2048 {
                                crate::api::report_event_with_metadata(
                                    "dht22_reading",
                                    "info",
                                    std::format!("{:.1}C, {:.1}%", r.temp_c, r.humidity_pct),
                                    serde_json::json!({ "temp_c": r.temp_c, "humidity_pct": r.humidity_pct }),
                                );
                            } else {
                                log::warn!("[dht22] heap too low to safely report reading, skipping this cycle");
                            }
                        }
                    }
                    Err(e) => log::warn!("[dht22] read failed: {e:?}"),
                }
                let active = now_ms().saturating_sub(LAST_ACTIVE_MS.load(Ordering::Relaxed)) <= ACTIVE_WINDOW_MS;
                let interval_ms = if active { FAST_READ_INTERVAL_MS } else { SLOW_READ_INTERVAL_MS };
                std::thread::sleep(Duration::from_millis(interval_ms as u64));
            }
        });
    let _ = ThreadSpawnConfiguration::default().set();
    if let Err(e) = spawn_result {
        log::error!("[dht22] thread spawn failed: {e}");
    }
}

/// Cached copy of the dashboard's chart JSON, refreshed by `spawn_chart_cache_refresher` below.
/// Real bug found live (2026-08-30): `web.rs`'s `/dht/chart` route used to call
/// `api::fetch_dht_history_json()` -- a real outbound HTTP request, with this project's own
/// 6s timeout and a retry -- DIRECTLY inside the request handler. `handle()` runs on `web.rs`'s
/// one and only accept-loop thread (deliberately single-threaded, to avoid the thread-spawn-
/// storm class of bug fixed earlier this session) -- so a slow or briefly-unreachable dashboard
/// meant `/dht/chart` blocked that ONE thread for its whole timeout+retry window, which blocked
/// EVERY other request too, including `/status`'s own 3s poll. Confirmed live: a bare `curl` to
/// `/dht/chart` alone hung with zero response, explaining the user's real "page stops loading
/// most things" report. Fixed by moving the fetch to its own thread -- the request handler now
/// only ever reads this cache, never blocks on network I/O.
static CHART_CACHE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Real coredump-confirmed whole-device abort, caught live (2026-08-30) while the device was
/// crash-looping every few minutes with no obvious trigger -- traced to a plain `.clone()` this
/// function used to do. `String`/`Vec<u8>`'s `Clone` impl has no fallible allocation path
/// either, same bug class as every other fix this session -- and it ran on EVERY `/dht/chart`
/// request (every 30s for as long as the dashboard page stays open), cloning a real ~4.4KB
/// string. A same-instant largest-contiguous-free-block guard was tried first (matching every
/// other fix this session) and made things WORSE, not better: real serial capture showed this
/// device's largest free block sitting at 2304-6144 bytes during completely normal operation
/// -- routinely SMALLER than the ~4.4KB string itself -- so the guard just made the chart
/// permanently empty instead of occasionally crashing.
///
/// The real fix needs no allocation at all: hold the mutex for the caller's whole write instead
/// of cloning out of it. `write_response` already streams the body in 1KB chunks over real
/// wall-clock time regardless, so the lock is held a bit longer than before -- but
/// `spawn_chart_cache_refresher`'s own writes only happen once every 30s and just block
/// briefly waiting their turn, which is harmless (single mutex, no lock-ordering risk, no
/// deadlock possible).
pub fn with_cached_chart_json<R>(f: impl FnOnce(Option<&str>) -> R) -> R {
    let guard = CHART_CACHE.lock().unwrap();
    f(guard.as_deref())
}

// ── On-demand "show more" range fetch (1h/6h/24h buttons) ──────────────────────────────
//
// Deliberately NOT its own periodic poller: per explicit instruction, these wider ranges must
// only ever be fetched when a human actually clicks a button, never proactively.
//
// Went through TWO wrong designs before landing here, both confirmed wrong via real live
// evidence (2026-08-30), worth keeping so this isn't rediscovered from scratch:
//   1. One ALWAYS-ALIVE standing thread with its own permanent 6144-byte stack -- combined
//      with a second new standing thread added the same session for weather, this pushed the
//      device's boot-time stack budget over the edge (confirmed via serial: `lampBridge spawn
//      failed: Not enough space`, repeating for several seconds on every boot).
//   2. A fresh, throwaway thread spawned PER CLICK instead -- reasonable in principle (a click
//      is rare and human-paced, unlike the 10s dht22 reading cadence), but ALSO confirmed
//      failing live, well past any boot-time window: `[dht22] 1h range-fetch thread spawn
//      failed: Not enough space (os error 12)`, 193 seconds into a steady, otherwise-healthy
//      boot. This device's real free-heap ceiling is tight enough, even in normal steady
//      state, that finding room for a BRAND NEW thread stack -- even a small 6144-byte one --
//      on demand, at an unpredictable moment, isn't reliable.
// Fixed by reusing the chart-cache-refresher thread below instead of spawning anything new --
// its stack is ALREADY paid for and already alive; checking 3 more atomic flags on its
// existing loop costs nothing extra.
const RANGE_HOURS: [u32; 3] = [1, 6, 24];

struct RangeSlot {
    cache: std::sync::Mutex<Option<String>>,
    wanted: std::sync::atomic::AtomicBool,
}

static RANGE_SLOTS: [RangeSlot; 3] = [
    RangeSlot { cache: std::sync::Mutex::new(None), wanted: std::sync::atomic::AtomicBool::new(false) },
    RangeSlot { cache: std::sync::Mutex::new(None), wanted: std::sync::atomic::AtomicBool::new(false) },
    RangeSlot { cache: std::sync::Mutex::new(None), wanted: std::sync::atomic::AtomicBool::new(false) },
];

fn slot_index(hours: u32) -> Option<usize> {
    RANGE_HOURS.iter().position(|&h| h == hours)
}

/// Flags a range as wanted -- O(1), no thread spawn, no network I/O. Serviced by
/// `spawn_chart_cache_refresher`'s own already-alive loop below, not a dedicated thread (see
/// this module's own comment above for why). Returns `false` for an hours value that isn't
/// one of the fixed presets.
pub fn request_range(hours: u32) -> bool {
    match slot_index(hours) {
        Some(i) => {
            RANGE_SLOTS[i].wanted.store(true, Ordering::Relaxed);
            true
        }
        None => false,
    }
}

/// `None` means "not fetched yet" (button never clicked this boot, or a fetch is still in
/// flight) -- the frontend polls this after clicking a range button until it sees real data.
/// Same no-clone pattern as `with_cached_chart_json` -- see that function's own comment for
/// why a heap-size guard was tried first and made things worse, not better, on this device.
pub fn with_cached_range_json<R>(hours: u32, f: impl FnOnce(Option<&str>) -> R) -> R {
    match slot_index(hours) {
        Some(i) => {
            let guard = RANGE_SLOTS[i].cache.lock().unwrap();
            f(guard.as_deref())
        }
        None => f(None),
    }
}

// Joins `network_lock` (2026-08-30) for the same reason `web.rs`'s own handler already does --
// a real coredump showed this fetch's own allocation failing right at boot, at the exact
// moment finFetch's own heavy fetch-cycle allocations were also in flight. Waiting for a turn
// on the same lock finFetch/lampBridge/exprotocol/web already coordinate on means this runs
// when heap actually has room, instead of racing the worst possible moment every time.
//
// ALSO services the 1h/6h/24h range buttons now (2026-08-30) -- checked every ~1s so a click
// resolves promptly, instead of a separate thread (see request_range's own comment for why
// that was tried twice and confirmed unsafe on this device both times). Logged on spawn
// failure now instead of silently discarded via `.ok()` -- that silent-discard class of bug is
// exactly what made the original range-fetch failures invisible until a live serial capture
// caught them directly.
pub fn spawn_chart_cache_refresher(network_lock: std::sync::Arc<std::sync::Mutex<()>>) {
  loop {
    let network_lock = std::sync::Arc::clone(&network_lock);
    let result = std::thread::Builder::new()
        .name("dht-chart-cache".into())
        .stack_size(6144)
        .spawn(move || {
            let mut tick: u32 = 0;
            // When the next default-chart refresh is due, in ticks -- NOT a flat `tick % 30`.
            // Real live evidence (2026-08-30): the very first attempt, right at boot, failed
            // with `ESP_ERR_HTTP_CONNECT` (network/event-dashboard not fully ready yet) -- and
            // a flat 30-tick modulus meant the NEXT attempt didn't fire until a full 30s later,
            // leaving the dashboard showing "not enough data yet" for that whole window. A
            // failure usually means something transient still settling, likely to resolve much
            // sooner than 30s -- retrying at +5 ticks instead of +30 after a failure (still +30
            // after a success, unchanged) fixes that without hammering the network on a genuine
            // outage either.
            let mut next_chart_refresh: u32 = 0;
            loop {
                // Heartbeat, updated BEFORE any of this iteration's own network I/O -- see
                // `REFRESHER_HEARTBEAT_MS`'s own comment for why this exists: this thread was
                // found live (2026-08-30) to be able to get permanently stuck -- not crash,
                // just silently stop making progress -- confirmed via direct HTTP observation
                // showing the cache's age climbing linearly, +30s every 30s, for 5+ straight
                // minutes with zero successful refreshes. No coredump was possible (nothing
                // panicked) and no JTAG is available on this hardware to catch it live, so the
                // exact stuck call was never pinned down -- most likely an `esp_http_client`
                // connect/read call not honoring its own configured timeout under some real
                // network condition (this device's own well-documented WiFi flakiness makes
                // that plausible: a dropped SYN or a stalled read past whatever phase the
                // client's `timeout_ms` doesn't actually cover). Rust has no way to forcibly
                // cancel a stuck thread from outside, so detecting the exact cause isn't even
                // actionable without one -- this bounds the BLAST RADIUS instead: main.rs's own
                // loop (confirmed to keep running throughout the whole stuck window) watches
                // this heartbeat and reboots the device if it ever goes stale, turning
                // "silently stuck forever" into "recovers on its own within ~2 minutes."
                REFRESHER_HEARTBEAT_MS.store(now_ms(), Ordering::Relaxed);
                for (i, &hours) in RANGE_HOURS.iter().enumerate() {
                    if RANGE_SLOTS[i].wanted.swap(false, Ordering::Relaxed) {
                        let result = {
                            let _guard = network_lock.lock().unwrap();
                            crate::api::fetch_dht_timeseries_json(hours)
                        };
                        match result {
                            Ok(json) => {
                                log::info!("[dht22] {hours}h range cache refreshed ({} bytes)", json.len());
                                *RANGE_SLOTS[i].cache.lock().unwrap() = Some(json);
                            }
                            Err(e) => log::warn!("[dht22] {hours}h range fetch failed: {e}"),
                        }
                    }
                }
                // Default "live" chart -- ~30s between refreshes on success, matching the
                // page's own refreshChart() poll interval; fires on tick 0 too, so the first
                // cache populates immediately at boot rather than after a full 30s wait.
                //
                // Uses the SAME bucketed `/api/events/timeseries` endpoint the 1h/6h/24h
                // buttons use (2026-08-31), not raw event rows -- per request to show more
                // history on "Live" without growing the response. A 1-hour window buckets down
                // to 60 points at this endpoint's own 60s floor (see event-dashboard's own
                // `target_points`/floor logic), a real ~900-byte payload -- smaller than the
                // OLD 20-raw-event/~4.5KB response while covering 6x the time window and
                // showing 3x the points. Safer too: this device's real fragmentation ceiling
                // has only gotten tighter over tonight's testing, and a compact bucketed
                // payload has much more headroom under it than raw per-event JSON ever did.
                if tick >= next_chart_refresh {
                    let result = {
                        let _guard = network_lock.lock().unwrap();
                        crate::api::fetch_dht_timeseries_json(1)
                    };
                    match result {
                        Ok(json) => {
                            log::info!("[dht22] chart cache refreshed ({} bytes)", json.len());
                            *CHART_CACHE.lock().unwrap() = Some(json);
                            next_chart_refresh = tick + 30;
                        }
                        Err(e) => {
                            log::warn!("[dht22] chart cache refresh failed: {e}");
                            next_chart_refresh = tick + 5;
                        }
                    }
                }
                tick = tick.wrapping_add(1);
                std::thread::sleep(Duration::from_secs(1));
            }
        });
    match result {
        Ok(_) => break,
        Err(e) => {
            // Retry-with-backoff, matching finFetch/lampBridge/fetchA/fetchB's own established
            // pattern -- a real, confirmed risk on this device under heap pressure. Found live
            // (2026-08-30) that this spawn used to be a ONE-SHOT attempt with no retry at all:
            // if it ever failed, this thread never existed for the rest of that boot, silently
            // disabling both the chart feature AND (until a matching fix) the watchdog meant
            // to catch exactly that.
            log::error!("[dht22] chart-cache-refresher thread spawn failed: {e}, retrying in 500ms");
            std::thread::sleep(Duration::from_millis(500));
        }
    }
  }
}
