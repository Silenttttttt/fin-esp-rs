//! LG air conditioner control over IR -- Fin-ESP is the AC's ONLY controller (same model as
//! the Tuya lamp: the LG remote stays in a drawer, every change goes through here).
//!
//! Protocol: LG2, 28 bits MSB-first, header 3200/9900 us, bit mark 480 us, 0 = 550 us space,
//! 1 = 1600 us space (IRremoteESP8266 src/ir_LG.h/.cpp). Captured from the real remote
//! (AKB30086214) 2026-10-26, see esp32-tests/ir_receiver_test/ac_capture/.
//!   Sign:8 (0x88) | Power:2 (0 on, 3 off) | :3 | Mode:3 | Temp:4 (C-15) | Fan:4 | Sum:4
//!   Sum = sum of the 4 data nibbles & 0xF.
//!
//! IR itself is one-way, but the AC reports its real state over WiFi to Fin-ESP's own ac_cloud
//! (src/ac_cloud/). That readback closes the loop (2026-10-27, after the AC silently missed frames
//! while the dashboard and MCP kept reporting the requested temperature as done):
//!   - every main frame is VERIFIED against the AC's reported power/mode/setpoint and re-sent
//!     (it's an absolute frame, so a repeat is idempotent) until it matches or VERIFY_MAX_SENDS is
//!     reached; then the command is reported as failed, never as done;
//!   - when nothing is in flight and the AC reports something else (LG remote, a missed frame),
//!     Fin-ESP adopts the AC's real values, so the dashboard always shows what the AC really has.
//! Without a live readback (ac_cloud down) it degrades to the old single send, reported as
//! "unverified". Every main frame carries the complete state (power+mode+temp+fan), so a
//! send is idempotent and re-sending ("resend") re-normalizes the AC whatever it was doing.
//! Toggle-type codes (light toggle, swing toggle) are deliberately never sent -- a toggle can't
//! be normalized without feedback; swing uses LG's absolute position codes instead.
//!
//! Callers (web, blue button) only change the atomics below and mark them pending; one sender
//! thread owns the RMT driver, coalesces rapid changes (SETTLE_MS) into one frame, transmits,
//! then persists. No caller ever blocks on IR or NVS.
#![allow(deprecated)] // legacy RMT API, same as rust_bench_test's proven IR transmitter

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::time::Duration;

use esp_idf_hal::gpio::Gpio2;
use esp_idf_hal::rmt::config::{CarrierConfig, DutyPercent, TransmitConfig};
use esp_idf_hal::rmt::{PinState, Pulse, TxRmtDriver, VariableLengthSignal, CHANNEL0};
use esp_idf_hal::units::FromValueType;
use log::{info, warn};

use crate::persist::Persist;

pub const MODES: [&str; 5] = ["cool", "dry", "fan", "auto", "heat"];
/// (name, LG fan nibble)
pub const FANS: [(&str, u8); 6] = [("lowest", 0), ("low", 1), ("medium", 2), ("high", 10), ("max", 4), ("auto", 5)];
/// (name, absolute LG code)
pub const SWING_V: [(&str, u32); 8] = [
    ("off", 0x881315A), ("swing", 0x8813149), ("highest", 0x881309D), ("high", 0x881308C),
    ("upper-mid", 0x881307B), ("middle", 0x881306A), ("low", 0x8813059), ("lowest", 0x8813048),
];
pub const SWING_H: [(&str, u32); 2] = [("off", 0x881317C), ("auto", 0x881316B)];

pub const TEMP_MIN: u8 = 16;
pub const TEMP_MAX: u8 = 30;
/// Default setpoint before any state exists, and the "comfort" preset's temperature.
pub const BUTTON_ON_TEMP: u8 = 21;

const LG_OFF: u32 = 0x88C0051;

// Transmit timings calibrated 2026-10-26 to Muni's real AKB30086214 remote, not the generic
// IRremoteESP8266 LG2 values (3200/9900, 480, 550/1600): our looped-back frames measured
// header 3245/9871, mark 518, 0=510, 1=1563 vs the remote's 3097/9420, 495, 504, 1504 through
// the same receiver -- 4-5% off, which a weak LED leaves no margin for. Each value below is the
// old one shifted by (remote - ours) as measured.
// Runtime-tunable (GET /ac/txcfg, POST /action/ac/txcfg) so delivery can be measured against the
// AC's own readback without a reflash per variant. RAM only: a reboot restores these defaults.
static HDR_MARK: AtomicU32 = AtomicU32::new(3050);
static HDR_SPACE: AtomicU32 = AtomicU32::new(9450);
static BIT_MARK: AtomicU32 = AtomicU32::new(460);
static ZERO_SPACE: AtomicU32 = AtomicU32::new(540);
static ONE_SPACE: AtomicU32 = AtomicU32::new(1540);
/// Copies of each frame per transmission. Safe because every code Fin-ESP sends is absolute
/// (never a toggle): an AC that receives two copies just applies the same setting twice.
static COPIES: AtomicU8 = AtomicU8::new(1);
/// Silence between copies (LG2 minimum message gap is ~40 ms, IRremoteESP8266 kLgMinGap).
static COPY_GAP_MS: AtomicU32 = AtomicU32::new(60);
/// Carrier duty %. Applied by the sender thread via rmt_set_tx_carrier when it changes.
static DUTY_PCT: AtomicU8 = AtomicU8::new(50);
static DUTY_DIRTY: AtomicBool = AtomicBool::new(false);

/// "k=v&k=v" over the knobs above; unknown keys/out-of-range values are rejected (false).
pub fn set_txcfg(query: &str) -> bool {
    for kv in query.split('&').filter(|kv| !kv.is_empty()) {
        let Some((k, v)) = kv.split_once('=') else { return false };
        let Ok(n) = v.parse::<u32>() else { return false };
        let ok = match k {
            "hdr_mark" if (2000..=5000).contains(&n) => { HDR_MARK.store(n, Ordering::Relaxed); true }
            "hdr_space" if (6000..=12000).contains(&n) => { HDR_SPACE.store(n, Ordering::Relaxed); true }
            "bit_mark" if (300..=800).contains(&n) => { BIT_MARK.store(n, Ordering::Relaxed); true }
            "zero_space" if (300..=900).contains(&n) => { ZERO_SPACE.store(n, Ordering::Relaxed); true }
            "one_space" if (1100..=2000).contains(&n) => { ONE_SPACE.store(n, Ordering::Relaxed); true }
            "copies" if (1..=4).contains(&n) => { COPIES.store(n as u8, Ordering::Relaxed); true }
            "gap_ms" if (20..=500).contains(&n) => { COPY_GAP_MS.store(n, Ordering::Relaxed); true }
            "duty" if (10..=90).contains(&n) => { DUTY_PCT.store(n as u8, Ordering::Relaxed); DUTY_DIRTY.store(true, Ordering::Relaxed); true }
            _ => false,
        };
        if !ok { return false; }
    }
    true
}

pub fn txcfg_json() -> String {
    std::format!(
        r#"{{"hdr_mark":{},"hdr_space":{},"bit_mark":{},"zero_space":{},"one_space":{},"copies":{},"gap_ms":{},"duty":{},"verify_wait_ms":{}}}"#,
        HDR_MARK.load(Ordering::Relaxed), HDR_SPACE.load(Ordering::Relaxed), BIT_MARK.load(Ordering::Relaxed),
        ZERO_SPACE.load(Ordering::Relaxed), ONE_SPACE.load(Ordering::Relaxed), COPIES.load(Ordering::Relaxed),
        COPY_GAP_MS.load(Ordering::Relaxed), DUTY_PCT.load(Ordering::Relaxed), VERIFY_WAIT_MS,
    )
}

/// More GPIOs driving the SAME LED (jumpered onto the one KY-005's S pin). KY-005 has no
/// series resistor and no transistor (middle pin unconnected), so the LED's current is capped
/// only by the single pin feeding it. Each extra pin carries the identical RMT channel-0 signal
/// through the GPIO matrix -- same edges, same instant, never opposing -- so their currents add.
/// No extra parts: just a jumper from each pin to S. Unjumpered pins simply idle low.
const EXTRA_LED_PINS: [i32; 2] = [4, 25];
const SETTLE_MS: u64 = 300;

static POWER: AtomicBool = AtomicBool::new(false);
static MODE: AtomicU8 = AtomicU8::new(0);
static TEMP: AtomicU8 = AtomicU8::new(BUTTON_ON_TEMP);
static FAN: AtomicU8 = AtomicU8::new(5); // index into FANS: auto
static SWING_V_IDX: AtomicU8 = AtomicU8::new(0);
static SWING_H_IDX: AtomicU8 = AtomicU8::new(0);

// AC-side settings beyond the main frame (absolute codes only; see docs/LG_AC_IR.md). Tested on
// Muni's unit 2026-10-27: auto-clean ON = 1 beep (accepted), show-power = "0.0 kW" on the display;
// ion/plasma and outdoor-quiet = 4 beeps (unsupported on this unit, so not offered).
static AUTO_CLEAN: AtomicBool = AtomicBool::new(false);
static ENERGY_SAVING: AtomicBool = AtomicBool::new(false);
/// Index into POWER_LIMITS (LG "Energy Ctrl").
static POWER_LIMIT: AtomicU8 = AtomicU8::new(0);
static PENDING_AUTO_CLEAN: AtomicBool = AtomicBool::new(false);
static PENDING_ENERGY_SAVING: AtomicBool = AtomicBool::new(false);
static PENDING_POWER_LIMIT: AtomicBool = AtomicBool::new(false);
static PENDING_SHOW_POWER: AtomicBool = AtomicBool::new(false);
const AUTO_CLEAN_CODES: [u32; 2] = [0x88C00C8, 0x88C00B7]; // [off, on]
const ENERGY_SAVING_CODES: [u32; 2] = [0x8810056, 0x8810045]; // [off, on]
/// (percent, code). 80% (0x88C07D0) is the code Muni's own remote sends.
pub const POWER_LIMITS: [(u8, u32); 4] = [(100, 0x88C07F2), (80, 0x88C07D0), (60, 0x88C07E1), (40, 0x88C0804)];
const LG_SHOW_POWER: u32 = 0x88C0466;

static PENDING_MAIN: AtomicBool = AtomicBool::new(false);
static PENDING_SWING_V: AtomicBool = AtomicBool::new(false);
static PENDING_SWING_H: AtomicBool = AtomicBool::new(false);
static LAST_CHANGE_MS: AtomicU32 = AtomicU32::new(0);
static SENT: AtomicU32 = AtomicU32::new(0);
/// Power state carried by the last main frame actually transmitted -- decides power-on vs
/// while-running encoding (see main_frame). Initialised from the persisted state at boot.
static LAST_SENT_ON: AtomicBool = AtomicBool::new(false);
/// Jet (LG 0x8810089, command 0x1008): max cooling until the next settings frame cancels it.
static JET: AtomicBool = AtomicBool::new(false);
static PENDING_JET: AtomicBool = AtomicBool::new(false);
const LG_JET: u32 = 0x8810089;
/// Sleep timer: unix time at which Fin-ESP itself sends OFF (0 = none). Kept here rather than
/// using the AC's own sleep timer so Fin-ESP's state can never disagree with the AC's.
static OFF_AT: AtomicU32 = AtomicU32::new(0);
/// Fin-ESP's own IR receiver (GPIO32) sits cm from the LED and sees every frame we send. The
/// NEC remote reader checks this and ignores the line while we transmit (+ a short tail), so
/// our own AC frames never reach it as noise.
static TX_BLANK_UNTIL_US: AtomicU32 = AtomicU32::new(0);
const TX_BLANK_TAIL_US: u32 = 150_000;

pub fn tx_blanking() -> bool {
    let now = unsafe { esp_idf_sys::esp_timer_get_time() } as u32;
    (TX_BLANK_UNTIL_US.load(Ordering::Relaxed).wrapping_sub(now) as i32) > 0
}

fn blank_for(us: u32) {
    let now = unsafe { esp_idf_sys::esp_timer_get_time() } as u32;
    TX_BLANK_UNTIL_US.store(now.wrapping_add(us), Ordering::Relaxed);
}

fn now_ms() -> u32 {
    (unsafe { esp_idf_sys::esp_timer_get_time() } / 1000) as u32
}

fn checksum(v: u32) -> u32 {
    (((v >> 4) & 0xF) + ((v >> 8) & 0xF) + ((v >> 12) & 0xF) + ((v >> 16) & 0xF)) & 0xF
}

/// LG2 on/set frame for the current state, or the off frame.
fn main_frame() -> u32 {
    if !POWER.load(Ordering::Relaxed) {
        return LG_OFF;
    }
    let mode = MODE.load(Ordering::Relaxed) as u32;
    let temp = TEMP.load(Ordering::Relaxed) as u32;
    let fan = FANS[FAN.load(Ordering::Relaxed) as usize % FANS.len()].1 as u32;
    // LG sets this bit on every settings change while the AC is already running and clears
    // it only on the frame that powers it on (Arduino-IRremote ac_LG.h LG_SWITCH_ON_MASK,
    // confirmed against Muni's AKB30086214: power-on 0x8800347, mode change while on 0x88083A5).
    // The AC's own reported power wins over what we last sent: after a missed frame those differ,
    // and the bit must describe the AC's real state for the frame to be a normal settings change.
    let ac_on = crate::ac_cloud::readback().map(|r| r.power).unwrap_or(LAST_SENT_ON.load(Ordering::Relaxed));
    let running = if ac_on { 1 << 15 } else { 0 };
    let v = (0x88 << 20) | running | (mode << 12) | ((temp - 15) << 8) | (fan << 4);
    v | checksum(v)
}

fn mark_main() {
    LAST_CHANGE_MS.store(now_ms(), Ordering::Relaxed);
    PENDING_MAIN.store(true, Ordering::Relaxed);
}

// ── Closed loop: verify every main frame against the AC's own readback ──────────────────────

/// How long to wait for the AC to report a frame's settings before re-sending it. Measured
/// 2026-10-27: when the AC takes a frame its WiFi report arrives ~1 s later.
const VERIFY_WAIT_MS: u32 = 3_000;
/// Total transmissions of one main frame before giving up and reporting failure.
const VERIFY_MAX_SENDS: u8 = 4;
/// Adopt the AC's reported state only after this long without any local change.
const ADOPT_QUIET_MS: u32 = 8_000;

const V_IDLE: u8 = 0;
const V_WAITING: u8 = 1;
const V_CONFIRMED: u8 = 2;
const V_FAILED: u8 = 3;
const V_UNVERIFIED: u8 = 4;
static VERIFY: AtomicU8 = AtomicU8::new(V_IDLE);
static VERIFY_SENDS: AtomicU8 = AtomicU8::new(0);
static VERIFY_SINCE_MS: AtomicU32 = AtomicU32::new(0);
/// Set by the verifier when it re-queues PENDING_MAIN, so the send counts as a retry.
static RETRY_MAIN: AtomicBool = AtomicBool::new(false);
/// What the last FAILED command asked for (packed like pack()), for status reporting.
static FAILED_WANT: AtomicU32 = AtomicU32::new(0);
/// How many times Fin-ESP's state was corrected from the AC's own report.
static ADOPTED: AtomicU32 = AtomicU32::new(0);

const PATH_NONE: u8 = 0;
const PATH_WIFI: u8 = 1;
const PATH_IR: u8 = 2;
/// Which path carried the last main-state send.
static PATH: AtomicU8 = AtomicU8::new(PATH_NONE);

/// Send the requested power/mode/fan/target over the AC's WiFi link, exactly as rethink's
/// RAC_056905_WW driver does: off = [0x1f7=0]; on from off = [0x1f7=1, mode, fan, target];
/// change while on = [target, mode, fan]. False = no live link, caller uses IR.
fn wifi_write() -> bool {
    let Some(rb) = crate::ac_cloud::readback() else { return false };
    if !POWER.load(Ordering::Relaxed) {
        return crate::ac_cloud::write_climate(&[(0x1f7, 0)]);
    }
    let mode = mode_tlv(MODE.load(Ordering::Relaxed));
    let fan = FAN_TLV[FAN.load(Ordering::Relaxed) as usize % FAN_TLV.len()];
    let target = TEMP.load(Ordering::Relaxed) as u32 * 2;
    if rb.power {
        crate::ac_cloud::write_climate(&[(0x1fe, target), (0x1f9, mode), (0x1fa, fan)])
    } else {
        crate::ac_cloud::write_climate(&[(0x1f7, 1), (0x1f9, mode), (0x1fa, fan), (0x1fe, target)])
    }
}

fn state_json_short() -> String {
    std::format!("on={} mode={} temp={} fan={}", POWER.load(Ordering::Relaxed),
        MODES[MODE.load(Ordering::Relaxed) as usize % MODES.len()], TEMP.load(Ordering::Relaxed),
        FANS[FAN.load(Ordering::Relaxed) as usize % FANS.len()].0)
}

/// IR mode index (MODES) -> AC's TLV mode code (rethink RAC_056905_WW MODES).
fn mode_tlv(idx: u8) -> u32 {
    match idx { 0 => 0, 1 => 1, 2 => 2, 3 => 6, 4 => 4, _ => u32::MAX }
}
fn mode_from_tlv(t: u32) -> Option<u8> {
    match t { 0 => Some(0), 1 => Some(1), 2 => Some(2), 6 => Some(3), 4 => Some(4), _ => None }
}
/// TLV fan code for each FANS entry (same order: lowest, low, medium, high, max, auto).
const FAN_TLV: [u32; 6] = [2, 3, 4, 5, 6, 8];
/// Only cool and heat hold a user setpoint; in dry/fan/auto LG ignores or overrides it.
fn temp_applies(mode_idx: u8) -> bool { mode_idx == 0 || mode_idx == 4 }

/// Does the AC's report match Fin-ESP's power/mode/setpoint?
fn matches(rb: &crate::ac_cloud::Readback) -> bool {
    let on = POWER.load(Ordering::Relaxed);
    if rb.power != on { return false; }
    if !on { return true; }
    let mode = MODE.load(Ordering::Relaxed);
    if rb.mode != mode_tlv(mode) { return false; }
    !temp_applies(mode) || rb.target_half == Some(TEMP.load(Ordering::Relaxed) as u32 * 2)
}

/// Take the AC's reported power/mode/setpoint as Fin-ESP's state. Persists only on a real change.
fn adopt(rb: &crate::ac_cloud::Readback, persist: &Persist) {
    let before = pack();
    POWER.store(rb.power, Ordering::Relaxed);
    LAST_SENT_ON.store(rb.power, Ordering::Relaxed);
    if let Some(m) = mode_from_tlv(rb.mode) { MODE.store(m, Ordering::Relaxed); }
    if let Some(h) = rb.target_half {
        let t = (h / 2) as u8;
        if (TEMP_MIN..=TEMP_MAX).contains(&t) { TEMP.store(t, Ordering::Relaxed); }
    }
    if let Some(f) = rb.fan.and_then(|f| FAN_TLV.iter().position(|x| *x == f)) { FAN.store(f as u8, Ordering::Relaxed); }
    if !rb.power && OFF_AT.load(Ordering::Relaxed) != 0 { sleep_in(0); }
    if pack() != before {
        ADOPTED.fetch_add(1, Ordering::Relaxed);
        info!("[ac] state corrected from the AC's own report: {}", state_json());
        persist.save_ac_state(pack());
    }
}

/// Runs every sender-loop tick. Verifies the last main frame, re-sends it if the AC didn't take
/// it, reports failure after VERIFY_MAX_SENDS, and otherwise keeps Fin-ESP's state equal to the AC's.
fn closed_loop(persist: &Persist) {
    if PENDING_MAIN.load(Ordering::Relaxed) { return; } // a newer change goes out (and is verified) first
    let rb = crate::ac_cloud::readback();
    let now = now_ms();
    if VERIFY.load(Ordering::Relaxed) == V_WAITING {
        let waited = now.wrapping_sub(VERIFY_SINCE_MS.load(Ordering::Relaxed));
        let Some(rb) = rb else {
            if waited >= VERIFY_WAIT_MS {
                VERIFY.store(V_UNVERIFIED, Ordering::Relaxed);
                warn!("[ac] no readback from the AC (ac_cloud not connected): frame sent once, unconfirmed");
            }
            return;
        };
        if matches(&rb) {
            VERIFY.store(V_CONFIRMED, Ordering::Relaxed);
            info!("[ac] AC confirmed the change (send {})", VERIFY_SENDS.load(Ordering::Relaxed));
            return;
        }
        if waited < VERIFY_WAIT_MS { return; }
        let sends = VERIFY_SENDS.load(Ordering::Relaxed);
        if sends < VERIFY_MAX_SENDS {
            warn!("[ac] AC still reports power={} mode={} target={:?} after {} send {sends}: re-sending over IR",
                rb.power, rb.mode, rb.target_half, if PATH.load(Ordering::Relaxed) == PATH_WIFI { "WiFi" } else { "IR" });
            RETRY_MAIN.store(true, Ordering::Relaxed);
            // Back-date so the sender doesn't wait SETTLE_MS for a retry.
            LAST_CHANGE_MS.store(now.wrapping_sub(SETTLE_MS as u32), Ordering::Relaxed);
            PENDING_MAIN.store(true, Ordering::Relaxed);
        } else {
            FAILED_WANT.store(pack(), Ordering::Relaxed);
            VERIFY.store(V_FAILED, Ordering::Relaxed);
            warn!("[ac] AC did not take the change after {sends} sends: reporting failure, adopting the AC's real state");
            adopt(&rb, persist);
        }
        return;
    }
    let Some(rb) = rb else { return };
    if now.wrapping_sub(LAST_CHANGE_MS.load(Ordering::Relaxed)) < ADOPT_QUIET_MS { return; }
    if !matches(&rb) {
        adopt(&rb, persist);
    }
}

// ── State changes (any thread, never block) ──────────────────────────────────

pub fn set_power(on: bool) {
    POWER.store(on, Ordering::Relaxed);
    if !on && OFF_AT.load(Ordering::Relaxed) != 0 { sleep_in(0); }
    mark_main();
}

pub fn jet() {
    POWER.store(true, Ordering::Relaxed);
    LAST_CHANGE_MS.store(now_ms(), Ordering::Relaxed);
    PENDING_JET.store(true, Ordering::Relaxed);
}

fn unix_now() -> u32 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as u32).unwrap_or(0)
}

/// minutes = 0 cancels. Needs NTP time (it's synced at boot); refuses otherwise.
pub fn sleep_in(minutes: u32) -> bool {
    if minutes == 0 { OFF_AT.store(0, Ordering::Relaxed); SLEEP_DIRTY.store(true, Ordering::Relaxed); return true; }
    let now = unix_now();
    if now < 1_700_000_000 { return false; }
    OFF_AT.store(now + minutes.min(24 * 60) * 60, Ordering::Relaxed);
    SLEEP_DIRTY.store(true, Ordering::Relaxed);
    true
}
static SLEEP_DIRTY: AtomicBool = AtomicBool::new(false);

fn sleep_poll(persist: &Persist) {
    if SLEEP_DIRTY.swap(false, Ordering::Relaxed) {
        persist.save_ac_off_at(OFF_AT.load(Ordering::Relaxed));
    }
    let at = OFF_AT.load(Ordering::Relaxed);
    if at != 0 && unix_now() >= at && unix_now() >= 1_700_000_000 {
        OFF_AT.store(0, Ordering::Relaxed);
        persist.save_ac_off_at(0);
        if POWER.load(Ordering::Relaxed) {
            info!("[ac] sleep timer reached, turning off");
            set_power(false);
        }
    }
}

/// Blue button: off -> on in cool at BUTTON_ON_TEMP; on -> off. Returns the new power state.
///
/// 2026-10-27 (Muni: "it should be toggle on and off"): a pure power toggle now. It flips the AC's
/// REAL power as reported over WiFi (Fin-ESP's own flag could be stale after a missed frame, which
/// made a press meant as "off" come out as "on at 21C"), unless a change is still in flight, in
/// which case it flips that requested state. Turning on keeps the current mode/temp/fan.
pub fn button_toggle() -> bool {
    let in_flight = PENDING_MAIN.load(Ordering::Relaxed) || VERIFY.load(Ordering::Relaxed) == V_WAITING;
    let current = if in_flight {
        POWER.load(Ordering::Relaxed)
    } else {
        crate::ac_cloud::readback().map(|r| r.power).unwrap_or(POWER.load(Ordering::Relaxed))
    };
    let on = !current;
    set_power(on);
    on
}

/// Any of mode/temp/fan may be None (unchanged). Setting anything implies power on, matching
/// the real remote (an LG set frame always carries Power=on).
pub fn set(mode: Option<&str>, temp: Option<i32>, fan: Option<&str>) -> bool {
    let mode_idx = match mode {
        Some(m) => match MODES.iter().position(|x| *x == m) { Some(i) => Some(i as u8), None => return false },
        None => None,
    };
    let fan_idx = match fan {
        Some(f) => match FANS.iter().position(|(n, _)| *n == f) { Some(i) => Some(i as u8), None => return false },
        None => None,
    };
    if let Some(m) = mode_idx { MODE.store(m, Ordering::Relaxed); }
    if let Some(t) = temp { TEMP.store((t.clamp(TEMP_MIN as i32, TEMP_MAX as i32)) as u8, Ordering::Relaxed); }
    if let Some(f) = fan_idx { FAN.store(f, Ordering::Relaxed); }
    POWER.store(true, Ordering::Relaxed);
    mark_main();
    true
}

pub fn step_temp(delta: i32) {
    let t = TEMP.load(Ordering::Relaxed) as i32 + delta;
    set(None, Some(t), None);
}

pub fn set_swing_v(name: &str) -> bool {
    match SWING_V.iter().position(|(n, _)| *n == name) {
        Some(i) => { SWING_V_IDX.store(i as u8, Ordering::Relaxed); PENDING_SWING_V.store(true, Ordering::Relaxed); true }
        None => false,
    }
}

pub fn set_swing_h(name: &str) -> bool {
    match SWING_H.iter().position(|(n, _)| *n == name) {
        Some(i) => { SWING_H_IDX.store(i as u8, Ordering::Relaxed); PENDING_SWING_H.store(true, Ordering::Relaxed); true }
        None => false,
    }
}

/// (name, mode, temp, fan, swing_v) -- one tap sets everything. "comfort" == the blue button.
/// "jet" is also accepted by preset() -- it's LG's own Jet command, not a settings combination.
pub const PRESETS: [(&str, &str, i32, &str, Option<&str>); 3] = [
    ("comfort", "cool", BUTTON_ON_TEMP as i32, "auto", None),
    ("night", "cool", 24, "lowest", None),
    ("dry", "dry", 23, "auto", None),
];

pub fn preset(name: &str) -> bool {
    if name == "jet" { jet(); return true; }
    let Some(&(_, mode, temp, fan, swing_v)) = PRESETS.iter().find(|p| p.0 == name) else { return false };
    if let Some(sv) = swing_v { set_swing_v(sv); }
    set(Some(mode), Some(temp), Some(fan))
}

/// Any of the three may be None (unchanged). power_limit must be one of POWER_LIMITS' percents.
/// Sent while the AC is on; if it's off they wait and go out after the next power-on.
pub fn set_features(auto_clean: Option<bool>, energy_saving: Option<bool>, power_limit: Option<u8>) -> bool {
    let limit_idx = match power_limit {
        Some(p) => match POWER_LIMITS.iter().position(|(pct, _)| *pct == p) { Some(i) => Some(i as u8), None => return false },
        None => None,
    };
    if let Some(v) = auto_clean { AUTO_CLEAN.store(v, Ordering::Relaxed); PENDING_AUTO_CLEAN.store(true, Ordering::Relaxed); }
    if let Some(v) = energy_saving { ENERGY_SAVING.store(v, Ordering::Relaxed); PENDING_ENERGY_SAVING.store(true, Ordering::Relaxed); }
    if let Some(i) = limit_idx { POWER_LIMIT.store(i, Ordering::Relaxed); PENDING_POWER_LIMIT.store(true, Ordering::Relaxed); }
    LAST_CHANGE_MS.store(now_ms(), Ordering::Relaxed);
    true
}

/// Momentary: the AC shows its live power draw (kW) on its own display.
pub fn show_power() {
    LAST_CHANGE_MS.store(now_ms(), Ordering::Relaxed);
    PENDING_SHOW_POWER.store(true, Ordering::Relaxed);
}

/// Re-send the full state (main frame, airflow and AC settings) to re-normalize the AC.
pub fn resend() {
    PENDING_SWING_V.store(true, Ordering::Relaxed);
    PENDING_SWING_H.store(true, Ordering::Relaxed);
    PENDING_AUTO_CLEAN.store(true, Ordering::Relaxed);
    PENDING_ENERGY_SAVING.store(true, Ordering::Relaxed);
    PENDING_POWER_LIMIT.store(true, Ordering::Relaxed);
    mark_main();
}

/// JSON object for /status and /ac/state. Small, fixed-size format.
pub fn state_json() -> String {
    let verify = VERIFY.load(Ordering::Relaxed);
    let rb = crate::ac_cloud::readback();
    // What the AC itself reports right now (null = no live readback).
    let real = match &rb {
        Some(r) => std::format!(
            r#"{{"on":{},"mode":"{}","temp":{},"fan":"{}"}}"#,
            r.power,
            mode_from_tlv(r.mode).map(|m| MODES[m as usize]).unwrap_or("other"),
            r.target_half.map(|h| if h % 2 == 0 { (h / 2).to_string() } else { std::format!("{}.5", h / 2) }).unwrap_or_else(|| "null".into()),
            r.fan.and_then(|f| FAN_TLV.iter().position(|x| *x == f)).map(|i| FANS[i].0).unwrap_or("other"),
        ),
        None => "null".into(),
    };
    let in_sync = match &rb { Some(r) => if matches(r) { "true" } else { "false" }, None => "null" };
    let failed_want = match FAILED_WANT.load(Ordering::Relaxed) {
        v if verify == V_FAILED && v >> 31 != 0 => std::format!(
            r#"{{"on":{},"mode":"{}","temp":{}}}"#,
            v & 1 != 0, MODES[((v >> 1) & 0x7) as usize % MODES.len()], (v >> 4) & 0x1F),
        _ => "null".into(),
    };
    std::format!(
        r#"{{"on":{},"mode":"{}","temp":{},"fan":"{}","swing_v":"{}","swing_h":"{}","jet":{},"off_in_min":{},"auto_clean":{},"energy_saving":{},"power_limit":{},"sent":{},"pending":{},"verify":"{}","tries":{},"in_sync":{},"real":{},"failed_want":{},"corrected":{},"path":"{}","wifi_writes":{}}}"#,
        POWER.load(Ordering::Relaxed),
        MODES[MODE.load(Ordering::Relaxed) as usize % MODES.len()],
        TEMP.load(Ordering::Relaxed),
        FANS[FAN.load(Ordering::Relaxed) as usize % FANS.len()].0,
        SWING_V[SWING_V_IDX.load(Ordering::Relaxed) as usize % SWING_V.len()].0,
        SWING_H[SWING_H_IDX.load(Ordering::Relaxed) as usize % SWING_H.len()].0,
        JET.load(Ordering::Relaxed),
        match OFF_AT.load(Ordering::Relaxed) { 0 => "null".to_string(), at => ((at.saturating_sub(unix_now()) + 59) / 60).to_string() },
        AUTO_CLEAN.load(Ordering::Relaxed),
        ENERGY_SAVING.load(Ordering::Relaxed),
        POWER_LIMITS[POWER_LIMIT.load(Ordering::Relaxed) as usize % POWER_LIMITS.len()].0,
        SENT.load(Ordering::Relaxed),
        PENDING_MAIN.load(Ordering::Relaxed) || PENDING_SWING_V.load(Ordering::Relaxed) || PENDING_SWING_H.load(Ordering::Relaxed)
            || verify == V_WAITING,
        ["idle", "waiting", "confirmed", "failed", "unverified"][verify as usize % 5],
        VERIFY_SENDS.load(Ordering::Relaxed),
        in_sync,
        real,
        failed_want,
        ADOPTED.load(Ordering::Relaxed),
        ["none", "wifi", "ir"][PATH.load(Ordering::Relaxed) as usize % 3],
        crate::ac_cloud::writes_sent(),
    )
}

// ── Diagnostics: raw replay + loopback capture ──────────────────────────────
// Added 2026-10-26 to settle "protocol or LED power?" when the AC ignored our frames:
// /action/ac/raw replays the REAL remote's captured timings (known-good on this AC), and every
// send is looped back through Fin-ESP's own receiver (GPIO32, edge ISR armed only while we
// transmit) so what actually left the LED can be compared bit-for-bit with the remote's frames.

const RAW_MAX: usize = 160;

// ── IR self-test (2026-10-27): verify the NEC remote receiver's decode path end-to-end with no
// human needed. The AC's IR LED (GPIO2) sits cm from the remote receiver's photodiode (GPIO32) --
// that's the whole reason tx_blanking() exists, to make the receiver IGNORE our own transmissions.
// This deliberately bypasses blank_for() (unlike every other send path above) so the receiver
// thread is genuinely listening, letting us confirm the decode pipeline works without a real
// remote: fires a synthetic NEC frame (address 0x00, command 0xFF -- decoded and counted, but not
// mapped to any action in ir_remote.rs::dispatch, so this has zero side effects), and a real
// decode will bump ir_remote::decode_status()'s count and flash the same red LED a real button
// press would. Safe to remove once no longer needed for diagnosis.
const NEC_TEST_FRAME_US: [u32; 67] = [
    9000, 4500, // header
    // address 0x00 (8 bits, each mark+short-space = "0")
    562, 562, 562, 562, 562, 562, 562, 562, 562, 562, 562, 562, 562, 562, 562, 562,
    // ~address 0xFF (8 bits, each mark+long-space = "1")
    562, 1687, 562, 1687, 562, 1687, 562, 1687, 562, 1687, 562, 1687, 562, 1687, 562, 1687,
    // command 0xFF (8 bits = "1")
    562, 1687, 562, 1687, 562, 1687, 562, 1687, 562, 1687, 562, 1687, 562, 1687, 562, 1687,
    // ~command 0x00 (8 bits = "0")
    562, 562, 562, 562, 562, 562, 562, 562, 562, 562, 562, 562, 562, 562, 562, 562,
    // trailing stop mark: real NEC frames send one final ~562us mark after the last data bit so the
    // receiver can measure that bit's own space before the signal ends. Omitting it (the original bug
    // in this self-test) left the pin HIGH with nothing to end the final wait_while(pin, true, ..) on,
    // so every run timed out on exactly the last bit's space (confirmed live: last_timeout_stage=65,
    // elapsed=2202us, ~2us over the 2200us budget) -- a self-test artifact, not a receiver bug.
    562,
];
static PENDING_IR_SELFTEST: AtomicBool = AtomicBool::new(false);

/// Fires the synthetic NEC frame above. See its own comment for why/how. Diagnostic only.
pub fn ir_selftest() {
    PENDING_IR_SELFTEST.store(true, Ordering::Relaxed);
}
static RAW: std::sync::Mutex<([u32; RAW_MAX], usize)> = std::sync::Mutex::new(([0; RAW_MAX], 0));
static PENDING_RAW: AtomicBool = AtomicBool::new(false);

/// Comma-separated mark/space durations in us (first is a mark). Returns false if unusable.
pub fn queue_raw(csv: &str) -> bool {
    let mut buf = [0u32; RAW_MAX];
    let mut n = 0;
    for part in csv.split(',') {
        if n == RAW_MAX { return false; }
        match part.trim().parse::<u32>() { Ok(v) if v > 0 && v < 32000 => { buf[n] = v; n += 1; } _ => return false }
    }
    if n < 3 { return false; }
    if let Ok(mut r) = RAW.lock() { *r = (buf, n); } else { return false; }
    LAST_CHANGE_MS.store(now_ms().wrapping_sub(SETTLE_MS as u32), Ordering::Relaxed);
    PENDING_RAW.store(true, Ordering::Relaxed);
    true
}

const LOOP_MAX: usize = 200;
const LOOP_PIN: i32 = 32;
static LOOP_ARMED: AtomicBool = AtomicBool::new(false);
static LOOP_N: AtomicU32 = AtomicU32::new(0);
static mut LOOP_TS: [u32; LOOP_MAX] = [0; LOOP_MAX];
static LOOP_LAST: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

unsafe extern "C" fn loop_isr(_: *mut core::ffi::c_void) {
    if !LOOP_ARMED.load(Ordering::Relaxed) { return; }
    let n = LOOP_N.load(Ordering::Relaxed) as usize;
    if n < LOOP_MAX {
        LOOP_TS[n] = esp_idf_sys::esp_timer_get_time() as u32;
        LOOP_N.store(n as u32 + 1, Ordering::Relaxed);
    }
}

// DISABLED (2026-10-27, real bug found live): this raw esp_idf_sys GPIO interrupt claim on
// GPIO32 -- even with the interrupt immediately disabled right after installing it -- was
// permanently breaking ir_remote.rs's own PinDriver::input() level-reads on that SAME physical
// pin (the lamp remote receiver, GPIO32 is shared hardware: see WIRING.md). Confirmed live via
// a synthetic NEC self-test frame (ac::ir_selftest): loopback (this ISR mechanism) saw it fine,
// but ir_remote's own `driver.is_low()` never once returned true across 3.9M+ loop iterations
// while the thread was provably alive and unstarved -- i.e. remote decode was fully dead from
// the moment this ran at boot, every boot, whether or not a loopback capture was ever actually
// used. This loopback/sniff diagnostic is also now largely redundant: ac_cloud gives real WiFi
// telemetry straight from the AC's own sensors, which is strictly better than self-looping our
// own transmitted IR back through the receiver. Left as safe no-ops (never touch the GPIO/ISR
// registers) rather than deleted outright, so /ac/loopback and sniff mode degrade gracefully
// (report nothing) instead of needing every call site below reworked.
fn loop_init() {}

fn loop_arm() {
    LOOP_N.store(0, Ordering::Relaxed);
    LOOP_ARMED.store(true, Ordering::Relaxed);
}

/// Stop capturing and keep the result as "<label> <count> d1 d2 ..." for GET /ac/loopback.
fn loop_finish(label: &str) {
    LOOP_ARMED.store(false, Ordering::Relaxed);
    let n = LOOP_N.load(Ordering::Relaxed) as usize;
    let mut out = std::format!("{label} {}", n.saturating_sub(1));
    for i in 1..n {
        let d = unsafe { LOOP_TS[i].wrapping_sub(LOOP_TS[i - 1]) };
        out.push(' ');
        out.push_str(&d.to_string());
    }
    if let Ok(mut l) = LOOP_LAST.lock() { *l = out; }
    // Sniff (listen) mode still running: drop our own frame and keep listening for the remote.
    // Bug found live 2026-10-26: without this, any dashboard send during a listen window left
    // the receiver ISR disarmed and the session silently recorded nothing more.
    if LISTEN_UNTIL_MS.load(Ordering::Relaxed) != 0 {
        loop_arm();
    }
}

// Sniff mode: record the REAL remote's frames (POST /action/ac/listen?secs=N, GET /ac/sniff).
static LISTEN_UNTIL_MS: AtomicU32 = AtomicU32::new(0);
static SNIFFED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
const SNIFF_KEEP: usize = 12;
const SNIFF_GAP_US: u32 = 80_000; // AC remotes can send multi-part frames ~30 ms apart

pub fn listen(secs: u32) {
    if let Ok(mut v) = SNIFFED.lock() { v.clear(); }
    LISTEN_UNTIL_MS.store(now_ms().wrapping_add(secs.clamp(1, 600) * 1000), Ordering::Relaxed);
    loop_arm();
}

pub fn sniffed() -> String {
    let listening = (LISTEN_UNTIL_MS.load(Ordering::Relaxed).wrapping_sub(now_ms()) as i32) > 0;
    let mut out = std::format!("listening={listening}\n");
    if let Ok(v) = SNIFFED.lock() { for (i, f) in v.iter().enumerate() { out.push_str(&std::format!("{} {f}\n", i + 1)); } }
    out
}

/// Called from the sender loop: closes a sniffed frame after SNIFF_GAP_US of silence.
fn sniff_poll() {
    let until = LISTEN_UNTIL_MS.load(Ordering::Relaxed);
    if until == 0 { return; }
    let n = LOOP_N.load(Ordering::Relaxed) as usize;
    let now_us = unsafe { esp_idf_sys::esp_timer_get_time() } as u32;
    if n > 0 && now_us.wrapping_sub(unsafe { LOOP_TS[n - 1] }) > SNIFF_GAP_US {
        unsafe { esp_idf_sys::gpio_intr_disable(LOOP_PIN); }
        let n = LOOP_N.load(Ordering::Relaxed) as usize;
        let mut f = std::format!("F {}", n.saturating_sub(1));
        for i in 1..n {
            f.push(' ');
            f.push_str(&unsafe { LOOP_TS[i].wrapping_sub(LOOP_TS[i - 1]) }.to_string());
        }
        if let Ok(mut v) = SNIFFED.lock() { if v.len() >= SNIFF_KEEP { v.remove(0); } v.push(f); }
        LOOP_N.store(0, Ordering::Relaxed);
        unsafe { esp_idf_sys::gpio_intr_enable(LOOP_PIN); }
    }
    if (until.wrapping_sub(now_ms()) as i32) <= 0 {
        LISTEN_UNTIL_MS.store(0, Ordering::Relaxed);
        unsafe { esp_idf_sys::gpio_intr_disable(LOOP_PIN); }
        LOOP_ARMED.store(false, Ordering::Relaxed);
    }
}

pub fn loopback() -> String {
    LOOP_LAST.lock().map(|l| l.clone()).unwrap_or_default()
}

// ── Persistence: one packed u32 in NVS ──────────────────────────────────────

fn pack() -> u32 {
    (POWER.load(Ordering::Relaxed) as u32)
        | ((MODE.load(Ordering::Relaxed) as u32 & 0x7) << 1)
        | ((TEMP.load(Ordering::Relaxed) as u32 & 0x1F) << 4)
        | ((FAN.load(Ordering::Relaxed) as u32 & 0xF) << 9)
        | ((SWING_V_IDX.load(Ordering::Relaxed) as u32 & 0xF) << 13)
        | ((SWING_H_IDX.load(Ordering::Relaxed) as u32 & 0x3) << 17)
        | ((AUTO_CLEAN.load(Ordering::Relaxed) as u32) << 19)
        | ((ENERGY_SAVING.load(Ordering::Relaxed) as u32) << 20)
        | ((POWER_LIMIT.load(Ordering::Relaxed) as u32 & 0x3) << 21)
        | (1 << 31) // "valid" marker
}

fn unpack(v: u32) {
    if v >> 31 == 0 { return; }
    let mode = ((v >> 1) & 0x7) as u8;
    let temp = ((v >> 4) & 0x1F) as u8;
    let fan = ((v >> 9) & 0xF) as u8;
    let sv = ((v >> 13) & 0xF) as u8;
    let sh = ((v >> 17) & 0x3) as u8;
    POWER.store(v & 1 != 0, Ordering::Relaxed);
    if (mode as usize) < MODES.len() { MODE.store(mode, Ordering::Relaxed); }
    if (TEMP_MIN..=TEMP_MAX).contains(&temp) { TEMP.store(temp, Ordering::Relaxed); }
    if (fan as usize) < FANS.len() { FAN.store(fan, Ordering::Relaxed); }
    if (sv as usize) < SWING_V.len() { SWING_V_IDX.store(sv, Ordering::Relaxed); }
    if (sh as usize) < SWING_H.len() { SWING_H_IDX.store(sh, Ordering::Relaxed); }
    AUTO_CLEAN.store((v >> 19) & 1 != 0, Ordering::Relaxed);
    ENERGY_SAVING.store((v >> 20) & 1 != 0, Ordering::Relaxed);
    POWER_LIMIT.store(((v >> 21) & 0x3) as u8, Ordering::Relaxed);
}

// ── Sender thread ───────────────────────────────────────────────────────────

fn build_signal(ticks_hz: esp_idf_hal::units::Hertz, code: u32) -> VariableLengthSignal {
    let mark = |us: u64| Pulse::new_with_duration(ticks_hz, PinState::High, &Duration::from_micros(us)).unwrap();
    let space = |us: u64| Pulse::new_with_duration(ticks_hz, PinState::Low, &Duration::from_micros(us)).unwrap();
    let mut signal = VariableLengthSignal::with_capacity(2 + 28 * 2 + 1);
    let ld = |a: &AtomicU32| a.load(Ordering::Relaxed) as u64;
    let (bit_mark, zero, one) = (ld(&BIT_MARK), ld(&ZERO_SPACE), ld(&ONE_SPACE));
    let _ = signal.push([&mark(ld(&HDR_MARK)), &space(ld(&HDR_SPACE))]);
    for b in (0..28).rev() {
        let s = if (code >> b) & 1 != 0 { one } else { zero };
        let _ = signal.push([&mark(bit_mark), &space(s)]);
    }
    let _ = signal.push([&mark(bit_mark)]);
    signal
}

fn build_raw_signal(ticks_hz: esp_idf_hal::units::Hertz, d: &[u32]) -> VariableLengthSignal {
    let pulse = |level, us: u32| Pulse::new_with_duration(ticks_hz, level, &Duration::from_micros(us as u64)).unwrap();
    let mut signal = VariableLengthSignal::with_capacity(d.len() + 1);
    for pair in d.chunks(2) {
        if pair.len() == 2 {
            let _ = signal.push([&pulse(PinState::High, pair[0]), &pulse(PinState::Low, pair[1])]);
        } else {
            let _ = signal.push([&pulse(PinState::High, pair[0])]);
        }
    }
    signal
}

/// Loads persisted state and starts the sender. Does NOT transmit on boot: the AC keeps its own
/// state across a Fin-ESP reboot, and an unprompted frame would beep and could override it.
pub fn spawn(channel: CHANNEL0<'static>, pin: Gpio2<'static>, persist: Persist) {
    if let Some(v) = persist.load_ac_state() {
        unpack(v);
    }
    LAST_SENT_ON.store(POWER.load(Ordering::Relaxed), Ordering::Relaxed);
    OFF_AT.store(persist.load_ac_off_at().unwrap_or(0), Ordering::Relaxed);
    info!("[ac] loaded state {}", state_json());
    std::thread::Builder::new()
        .name("ac-ir".into())
        .stack_size(4096)
        .spawn(move || {
            let carrier = CarrierConfig::new()
                .frequency(38u32.kHz().into())
                .duty_percent(DutyPercent::new(50).unwrap()); // same as rust_bench_test's measured optimum
            let config = TransmitConfig::new().carrier(Some(carrier));
            let mut tx = match TxRmtDriver::new(channel, pin, &config) {
                Ok(t) => t,
                Err(e) => { warn!("[ac] RMT init failed: {e:?} -- AC control disabled"); return; }
            };
            // 2026-10-26: KY-005's middle (VCC) pin is NOT connected on most units -- the LED sits
            // straight between S and GND, so it's powered by this GPIO alone and was too weak to
            // reach the AC (only "sometimes" lit Fin-ESP's own receiver a few cm away). Max drive
            // strength (~40 mA, up from the ~20 mA default) for the short 50%-duty bursts.
            let drive = unsafe { esp_idf_sys::gpio_set_drive_capability(2, esp_idf_sys::gpio_drive_cap_t_GPIO_DRIVE_CAP_3) };
            info!("[ac] GPIO2 drive cap 3: {}", if drive == 0 { "ok" } else { "FAILED" });
            for extra in EXTRA_LED_PINS {
                unsafe {
                    esp_idf_sys::esp_rom_gpio_pad_select_gpio(extra as u32);
                    esp_idf_sys::gpio_set_direction(extra, esp_idf_sys::gpio_mode_t_GPIO_MODE_OUTPUT);
                    esp_idf_sys::esp_rom_gpio_connect_out_signal(extra as u32, esp_idf_sys::RMT_SIG_OUT0_IDX, false, false);
                    esp_idf_sys::gpio_set_drive_capability(extra, esp_idf_sys::gpio_drive_cap_t_GPIO_DRIVE_CAP_3);
                }
            }
            info!("[ac] IR LED driven by GPIO2 + {:?} (jumper any of them onto the LED's S pin)", EXTRA_LED_PINS);
            let ticks_hz = tx.counter_clock().unwrap();
            loop_init();
            loop {
                std::thread::sleep(Duration::from_millis(30));
                sniff_poll();
                sleep_poll(&persist);
                closed_loop(&persist);
                if PENDING_IR_SELFTEST.swap(false, Ordering::Relaxed) {
                    // Deliberately NO blank_for() AND no loop_arm/loop_finish here: the loopback
                    // ISR shares GPIO32 with ir_remote.rs's own polling PinDriver, and enabling it
                    // (as an earlier version of this test did) is a confound a real remote press
                    // never has -- that run's physical signal was independently confirmed clean
                    // (see /ac/loopback capture, 2026-10-27), so this run tests ONLY the decoder,
                    // under the exact conditions a real button press would see.
                    let signal = build_raw_signal(ticks_hz, &NEC_TEST_FRAME_US);
                    match tx.start_blocking(&signal) {
                        Ok(()) => info!("[ac] IR self-test frame sent (no blanking, no loopback ISR) -- check /ir/status"),
                        Err(e) => warn!("[ac] IR self-test send failed: {e:?}"),
                    }
                }
                if PENDING_RAW.swap(false, Ordering::Relaxed) {
                    let (buf, n) = RAW.lock().map(|r| *r).unwrap_or(([0; RAW_MAX], 0));
                    let signal = build_raw_signal(ticks_hz, &buf[..n]);
                    blank_for(1_000_000);
                    loop_arm();
                    let res = tx.start_blocking(&signal);
                    blank_for(TX_BLANK_TAIL_US);
                    std::thread::sleep(Duration::from_millis(60));
                    loop_finish("raw");
                    match res {
                        Ok(()) => { SENT.fetch_add(1, Ordering::Relaxed); info!("[ac] sent raw ({n} durations)"); }
                        Err(e) => warn!("[ac] raw send failed: {e:?}"),
                    }
                }
                let on_now = POWER.load(Ordering::Relaxed);
                let settings_pending = on_now && (PENDING_AUTO_CLEAN.load(Ordering::Relaxed)
                    || PENDING_ENERGY_SAVING.load(Ordering::Relaxed)
                    || PENDING_POWER_LIMIT.load(Ordering::Relaxed)
                    || PENDING_SHOW_POWER.load(Ordering::Relaxed));
                let any = settings_pending || PENDING_JET.load(Ordering::Relaxed) || PENDING_MAIN.load(Ordering::Relaxed)
                    || PENDING_SWING_V.load(Ordering::Relaxed)
                    || PENDING_SWING_H.load(Ordering::Relaxed);
                if !any || now_ms().wrapping_sub(LAST_CHANGE_MS.load(Ordering::Relaxed)) < SETTLE_MS as u32 {
                    continue;
                }
                let mut frames: heapless_frames::Frames = Default::default();
                let mut main_on: Option<bool> = None;
                let mut via_wifi = false;
                if PENDING_MAIN.swap(false, Ordering::Relaxed) {
                    let retry = RETRY_MAIN.swap(false, Ordering::Relaxed);
                    if retry {
                        VERIFY_SENDS.fetch_add(1, Ordering::Relaxed);
                    } else {
                        VERIFY_SENDS.store(1, Ordering::Relaxed);
                    }
                    // WiFi first (the AC's own cloud link, confirmed by its report ~1 s later);
                    // IR when there's no live link or a WiFi write wasn't confirmed (retry).
                    if !retry && wifi_write() {
                        via_wifi = true;
                        PATH.store(PATH_WIFI, Ordering::Relaxed);
                        info!("[ac] main state sent over WiFi: {}", state_json_short());
                    } else {
                        frames.push(main_frame());
                        main_on = Some(POWER.load(Ordering::Relaxed));
                        PATH.store(PATH_IR, Ordering::Relaxed);
                    }
                    JET.store(false, Ordering::Relaxed); // any settings frame ends Jet, like the remote
                }
                let mut jet_sent = false;
                if PENDING_JET.swap(false, Ordering::Relaxed) {
                    frames.push(LG_JET);
                    jet_sent = true;
                }
                let on = POWER.load(Ordering::Relaxed);
                if PENDING_SWING_V.swap(false, Ordering::Relaxed) && on {
                    frames.push(SWING_V[SWING_V_IDX.load(Ordering::Relaxed) as usize % SWING_V.len()].1);
                }
                if PENDING_SWING_H.swap(false, Ordering::Relaxed) && on {
                    frames.push(SWING_H[SWING_H_IDX.load(Ordering::Relaxed) as usize % SWING_H.len()].1);
                }
                // AC settings only go out while it's on; otherwise they stay pending for the next on.
                if on && PENDING_AUTO_CLEAN.swap(false, Ordering::Relaxed) {
                    frames.push(AUTO_CLEAN_CODES[AUTO_CLEAN.load(Ordering::Relaxed) as usize]);
                }
                if on && PENDING_ENERGY_SAVING.swap(false, Ordering::Relaxed) {
                    frames.push(ENERGY_SAVING_CODES[ENERGY_SAVING.load(Ordering::Relaxed) as usize]);
                }
                if on && PENDING_POWER_LIMIT.swap(false, Ordering::Relaxed) {
                    frames.push(POWER_LIMITS[POWER_LIMIT.load(Ordering::Relaxed) as usize % POWER_LIMITS.len()].1);
                }
                if PENDING_SHOW_POWER.swap(false, Ordering::Relaxed) && on {
                    frames.push(LG_SHOW_POWER);
                }
                if DUTY_DIRTY.swap(false, Ordering::Relaxed) {
                    // Legacy RMT carrier counts are in APB ticks (80 MHz): 38 kHz = 2105 ticks/period.
                    let period: u32 = 80_000_000 / 38_000;
                    let high = (period * DUTY_PCT.load(Ordering::Relaxed) as u32 / 100) as u16;
                    let low = (period as u16).saturating_sub(high);
                    let r = unsafe { esp_idf_sys::rmt_set_tx_carrier(0, true, high, low, esp_idf_sys::rmt_carrier_level_t_RMT_CARRIER_LEVEL_HIGH) };
                    info!("[ac] carrier duty {}% ({high}/{low}): {}", DUTY_PCT.load(Ordering::Relaxed), r);
                }
                for (i, code) in frames.iter().enumerate() {
                    if i > 0 { std::thread::sleep(Duration::from_millis(400)); } // let the AC finish its beep
                    let signal = build_signal(ticks_hz, code);
                    blank_for(1_000_000);
                    loop_arm();
                    let mut res = tx.start_blocking(&signal);
                    for _ in 1..COPIES.load(Ordering::Relaxed) {
                        if res.is_err() { break; }
                        blank_for(1_000_000);
                        std::thread::sleep(Duration::from_millis(COPY_GAP_MS.load(Ordering::Relaxed) as u64));
                        res = tx.start_blocking(&signal);
                    }
                    blank_for(TX_BLANK_TAIL_US);
                    std::thread::sleep(Duration::from_millis(60));
                    loop_finish(&std::format!("0x{code:07X}"));
                    match res {
                        Ok(()) => { SENT.fetch_add(1, Ordering::Relaxed); info!("[ac] sent 0x{code:07X}"); }
                        Err(e) => warn!("[ac] send 0x{code:07X} failed: {e:?}"),
                    }
                }
                if via_wifi {
                    VERIFY_SINCE_MS.store(now_ms(), Ordering::Relaxed);
                    VERIFY.store(V_WAITING, Ordering::Relaxed);
                }
                if let Some(on) = main_on {
                    LAST_SENT_ON.store(on, Ordering::Relaxed);
                    // Verification clock starts once the frame (and any extras) actually went out.
                    VERIFY_SINCE_MS.store(now_ms(), Ordering::Relaxed);
                    VERIFY.store(V_WAITING, Ordering::Relaxed);
                }
                if jet_sent { JET.store(true, Ordering::Relaxed); LAST_SENT_ON.store(true, Ordering::Relaxed); }
                persist.save_ac_state(pack());
            }
        })
        .ok();
}

/// Up to 8 frames per send cycle, no heap.
mod heapless_frames {
    #[derive(Default)]
    pub struct Frames { codes: [u32; 8], len: usize }
    impl Frames {
        pub fn push(&mut self, c: u32) { if self.len < 8 { self.codes[self.len] = c; self.len += 1; } }
        pub fn iter(&self) -> impl Iterator<Item = u32> + '_ { self.codes[..self.len].iter().copied() }
    }
}
