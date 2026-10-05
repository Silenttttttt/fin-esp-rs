//! INMP441 I2S microphone capture -> continuous raw-PCM push to Jarvis over TCP.
//!
//! STATUS (2026-10-05): UNTESTED. Written before the physical mic is wired up, per Muni's own
//! explicit instruction to draft this now rather than wait idle -- every pin assignment, I2S
//! config detail, and the push-socket design below is a documented best-effort based on the
//! INMP441's published datasheet behavior and this codebase's own established patterns, NOT
//! verified against real hardware. Treat every claim here the way this project's own IR work
//! treated early duty-cycle/voltage assumptions before they were actually measured: plausible,
//! not proven. The first real test on actual hardware is expected to need adjustment.
//!
//! Wiring (PROPOSED, not yet physically connected -- pick 3 free, non-strapping, non-input-only
//! GPIOs; the ones below are free per a full grep of every pin already claimed elsewhere in this
//! codebase as of 2026-10-05, but re-verify against main.rs/config.rs before wiring in case that
//! changes):
//!   INMP441  SCK (bit clock)  -> GPIO16
//!   INMP441  WS  (word select) -> GPIO17
//!   INMP441  SD  (data out)   -> GPIO21
//!   INMP441  L/R              -> GND (selects the LEFT slot -- this driver reads in Mono mode,
//!                                which per esp-idf-hal's own doc reads only "the first slot";
//!                                if no audio comes through once wired, the first thing to try is
//!                                swapping L/R to 3.3V instead, in case "first slot" actually
//!                                means something different than assumed here)
//!   INMP441  VDD               -> 3.3V (this part is spec'd for exactly 3.3V, unlike the KY-005
//!                                IR LED module from this project's earlier work -- no voltage
//!                                mismatch risk here, don't repeat that investigation)
//!   INMP441  GND               -> GND
//!
//! Format: 16kHz, 16-bit, mono -- the standard choice for voice/STT pipelines (what Jarvis's own
//! Whisper-based STT expects), and a deliberately conservative choice against this device's own
//! documented heap tightness: ~32 KB/s raw, trivial for LAN WiFi, and avoids adding any on-device
//! encoding complexity (no codec, no framing beyond raw sample bytes) to a board that's already
//! tight on both heap and attention (dht22 + ir_remote + the web server + Tuya all compete for
//! the same budget already).
//!
//! Transport: PUSH, not pull -- Fin-ESP opens an outbound TCP connection to Jarvis's host and
//! writes raw PCM bytes continuously, auto-reconnecting if it drops. This is a deliberate choice,
//! not a default: `web.rs`'s own server is a single-threaded, non-blocking-accept-loop by design
//! (after a real past thread-spawn-storm bug there), so a long-lived PULL-style connection held
//! open by Jarvis would block that entire server for as long as the stream is open. Every other
//! continuous/dedicated feature in this codebase (media-srv:9876, the existing mic-mute-relay
//! also confusingly called "mic-srv" on 9877, ota:3232) already uses this same "separate
//! dedicated connection, not multiplexed with the shared web server" shape -- this follows that
//! established pattern rather than inventing a new one.
//!
//! Continuous, not button-gated: per Muni's explicit choice (passive listening + wake-word
//! detection on Jarvis's own GPU-backed host side, not push-to-talk), this streams whenever it
//! has a live connection, with no local gating at all -- no on-device VAD, no button requirement.
//! Wake-word detection and the "was this actually a command" decision both live entirely on
//! Jarvis's side; Fin-ESP's only job is "capture and forward," matching exactly what was asked.
//!
//! Open question, explicitly NOT resolved by code alone: which CPU core this capture thread
//! should run on. I2S capture is DMA-buffered (the peripheral fills a hardware buffer on its own;
//! software just needs to drain it often enough, not with GPIO-bit-bang-level microsecond
//! precision), so it's inherently much more tolerant of scheduling jitter than this project's own
//! IR transmit work was -- that argues it could reasonably share Core0 with WiFi. But this
//! session's own ir_remote.rs fix (2026-09-26) turned its idle-poll into a genuine tight busy-spin
//! specifically because it's pinned to Core1 alongside dht22's reader -- meaning Core1 is now
//! BUSIER than it used to be, not an automatically-safe place to add a third continuously-active
//! thread either. Deliberately NOT core-pinning this thread below, for exactly this reason: real
//! measurement on actual hardware (buffer-overrun/underrun counters, not guesswork) should decide
//! this, the same way real measurement -- not assumption -- is what actually fixed IR reliability
//! all session. Revisit once wired.

use esp_idf_hal::gpio::{AnyIOPin, Gpio16, Gpio17, Gpio21};
use esp_idf_hal::i2s::{
    config::{Config as I2sChannelConfig, DataBitWidth, StdClkConfig, StdConfig, StdSlotConfig},
    I2sDriver, I2sRx, I2S0,
};
use std::io::Write;
use std::net::TcpStream;
use std::time::Duration;

const SAMPLE_RATE_HZ: u32 = 16_000;
const BIT_WIDTH: DataBitWidth = DataBitWidth::Bits16;

// Placeholder pending Jarvis's own confirmation of where its receiver actually listens -- see
// this file's own header. 192.168.1.105 is Muni's desktop (where Jarvis runs), port 9878 is the
// next free slot after this codebase's existing 9876 (media-srv) / 9877 (mic-mute-relay), not
// yet confirmed as what Jarvis will actually bind to.
const JARVIS_HOST: &str = "192.168.1.105:9878";

/// One I2S read buffer's worth of samples per push -- large enough to amortize syscall/TCP-write
/// overhead, small enough to keep end-to-end latency low for a "passive listening" use case
/// where responsiveness matters. 1024 samples @ 16kHz = 64ms of audio per chunk -- a reasonable
/// starting point, not a measured-optimal one.
const SAMPLES_PER_CHUNK: usize = 1024;
const BYTES_PER_CHUNK: usize = SAMPLES_PER_CHUNK * 2; // 16-bit = 2 bytes/sample

/// Spawned once at boot. Deliberately NOT core-pinned (see this file's own header for why) --
/// unlike dht22.rs/ir_remote.rs, which both have a concrete, already-measured reason to be on
/// Core1 specifically.
pub fn spawn_mic_capture(i2s0: I2S0<'static>, sck: Gpio16<'static>, ws: Gpio17<'static>, sd: Gpio21<'static>) {
    let spawn_result = std::thread::Builder::new()
        .name("micCapture".into())
        .stack_size(8192) // generous relative to dht22/ir_remote's 4096 -- I2S + a TCP client
                           // stack together plausibly need more; untested, revisit if real
                           // coredumps show otherwise, same pattern this project already follows
                           // elsewhere for stack-size decisions
        .spawn(move || {
            let std_config = StdConfig::new(
                I2sChannelConfig::default(),
                StdClkConfig::from_sample_rate_hz(SAMPLE_RATE_HZ),
                StdSlotConfig::philips_slot_default(
                    BIT_WIDTH,
                    esp_idf_hal::i2s::config::SlotMode::Mono,
                ),
                Default::default(),
            );
            let mut i2s = match I2sDriver::<I2sRx>::new_std_rx(
                i2s0,
                &std_config,
                sck,
                sd,
                AnyIOPin::none(),
                ws,
            ) {
                Ok(d) => d,
                Err(e) => {
                    log::error!("[mic] I2S init failed: {e}, giving up on this thread");
                    return;
                }
            };
            if let Err(e) = i2s.rx_enable() {
                log::error!("[mic] I2S rx_enable failed: {e}, giving up on this thread");
                return;
            }
            log::info!("[mic] I2S capture ready, {SAMPLE_RATE_HZ}Hz/16-bit/mono, pushing to {JARVIS_HOST}");

            let mut buf = [0u8; BYTES_PER_CHUNK];
            let mut stream: Option<TcpStream> = None;

            loop {
                // Drain one chunk's worth of audio from the I2S DMA buffer regardless of
                // whether we currently have a live connection -- the DMA buffer is a fixed
                // hardware size and WILL overrun (silently dropping audio) if this thread stalls
                // waiting on the network instead of continuing to service it. Reconnecting is
                // handled below, never by skipping a read.
                let n = match i2s.read(&mut buf, 1000) {
                    Ok(n) => n,
                    Err(e) => {
                        log::warn!("[mic] I2S read failed: {e}");
                        std::thread::sleep(Duration::from_millis(50));
                        continue;
                    }
                };
                if n == 0 {
                    continue;
                }

                if stream.is_none() {
                    match TcpStream::connect(JARVIS_HOST) {
                        Ok(s) => {
                            let _ = s.set_write_timeout(Some(Duration::from_millis(500)));
                            log::info!("[mic] connected to Jarvis at {JARVIS_HOST}");
                            stream = Some(s);
                        }
                        Err(e) => {
                            log::warn!("[mic] connect to {JARVIS_HOST} failed: {e}, will retry");
                            std::thread::sleep(Duration::from_millis(2000));
                            continue; // drop this chunk -- no connection to push it to, and
                                       // buffering across a down link would just grow heap usage
                                       // on an already heap-tight device for stale audio nobody
                                       // will hear as "live" anyway
                        }
                    }
                }

                if let Some(s) = stream.as_mut() {
                    if let Err(e) = s.write_all(&buf[..n]) {
                        log::warn!("[mic] push failed: {e}, will reconnect");
                        stream = None;
                    }
                }
            }
        });
    if let Err(e) = spawn_result {
        log::error!("[mic] thread spawn failed: {e}");
    }
}
