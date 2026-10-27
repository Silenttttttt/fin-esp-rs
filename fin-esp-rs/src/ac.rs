//! LG air conditioner control over IR -- Fin-ESP is the AC's ONLY controller (same model as
//! the Tuya lamp: the LG remote stays in a drawer, every change goes through here).
//!
//! Protocol: LG2, 28 bits MSB-first, header 3200/9900 us, bit mark 480 us, 0 = 550 us space,
//! 1 = 1600 us space (IRremoteESP8266 src/ir_LG.h/.cpp). Captured from the real remote
//! (AKB30086214) 2026-10-26, see esp32-tests/ir_receiver_test/ac_capture/.
//!   Sign:8 (0x88) | Power:2 (0 on, 3 off) | :3 | Mode:3 | Temp:4 (C-15) | Fan:4 | Sum:4
//!   Sum = sum of the 4 data nibbles & 0xF.
//!
//! IR is one-way: the AC never reports its state, so the state here IS the source of truth and
//! is persisted to NVS. Every main frame carries the complete state (power+mode+temp+fan), so a
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
/// Blue button turns the AC on at this temperature (Muni's spec, 2026-10-26).
pub const BUTTON_ON_TEMP: u8 = 21;

const LG_OFF: u32 = 0x88C0051;

// Transmit timings calibrated 2026-10-26 to Muni's real AKB30086214 remote, not the generic
// IRremoteESP8266 LG2 values (3200/9900, 480, 550/1600): our looped-back frames measured
// header 3245/9871, mark 518, 0=510, 1=1563 vs the remote's 3097/9420, 495, 504, 1504 through
// the same receiver -- 4-5% off, which a weak LED leaves no margin for. Each value below is the
// old one shifted by (remote - ours) as measured.
const HDR_MARK: u64 = 3050;
const HDR_SPACE: u64 = 9450;
const BIT_MARK: u64 = 460;
const ZERO_SPACE: u64 = 540;
const ONE_SPACE: u64 = 1540;

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
    let running = if LAST_SENT_ON.load(Ordering::Relaxed) { 1 << 15 } else { 0 };
    let v = (0x88 << 20) | running | (mode << 12) | ((temp - 15) << 8) | (fan << 4);
    v | checksum(v)
}

fn mark_main() {
    LAST_CHANGE_MS.store(now_ms(), Ordering::Relaxed);
    PENDING_MAIN.store(true, Ordering::Relaxed);
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
pub fn button_toggle() -> bool {
    let on = !POWER.load(Ordering::Relaxed);
    if on {
        MODE.store(0, Ordering::Relaxed);
        TEMP.store(BUTTON_ON_TEMP, Ordering::Relaxed);
    }
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
    std::format!(
        r#"{{"on":{},"mode":"{}","temp":{},"fan":"{}","swing_v":"{}","swing_h":"{}","jet":{},"off_in_min":{},"auto_clean":{},"energy_saving":{},"power_limit":{},"sent":{},"pending":{}}}"#,
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
        PENDING_MAIN.load(Ordering::Relaxed) || PENDING_SWING_V.load(Ordering::Relaxed) || PENDING_SWING_H.load(Ordering::Relaxed),
    )
}

// ── Diagnostics: raw replay + loopback capture ──────────────────────────────
// Added 2026-10-26 to settle "protocol or LED power?" when the AC ignored our frames:
// /action/ac/raw replays the REAL remote's captured timings (known-good on this AC), and every
// send is looped back through Fin-ESP's own receiver (GPIO32, edge ISR armed only while we
// transmit) so what actually left the LED can be compared bit-for-bit with the remote's frames.

const RAW_MAX: usize = 160;
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

fn loop_init() {
    unsafe {
        let _ = esp_idf_sys::gpio_install_isr_service(0); // may already be installed: fine
        esp_idf_sys::gpio_set_intr_type(LOOP_PIN, esp_idf_sys::gpio_int_type_t_GPIO_INTR_ANYEDGE);
        esp_idf_sys::gpio_isr_handler_add(LOOP_PIN, Some(loop_isr), core::ptr::null_mut());
        esp_idf_sys::gpio_intr_disable(LOOP_PIN);
    }
}

fn loop_arm() {
    LOOP_N.store(0, Ordering::Relaxed);
    LOOP_ARMED.store(true, Ordering::Relaxed);
    unsafe { esp_idf_sys::gpio_intr_enable(LOOP_PIN); }
}

/// Stop capturing and keep the result as "<label> <count> d1 d2 ..." for GET /ac/loopback.
fn loop_finish(label: &str) {
    unsafe { esp_idf_sys::gpio_intr_disable(LOOP_PIN); }
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
    let _ = signal.push([&mark(HDR_MARK), &space(HDR_SPACE)]);
    for b in (0..28).rev() {
        let s = if (code >> b) & 1 != 0 { ONE_SPACE } else { ZERO_SPACE };
        let _ = signal.push([&mark(BIT_MARK), &space(s)]);
    }
    let _ = signal.push([&mark(BIT_MARK)]);
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
                if PENDING_MAIN.swap(false, Ordering::Relaxed) {
                    frames.push(main_frame());
                    main_on = Some(POWER.load(Ordering::Relaxed));
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
                for (i, code) in frames.iter().enumerate() {
                    if i > 0 { std::thread::sleep(Duration::from_millis(400)); } // let the AC finish its beep
                    let signal = build_signal(ticks_hz, code);
                    blank_for(1_000_000);
                    loop_arm();
                    let res = tx.start_blocking(&signal);
                    blank_for(TX_BLANK_TAIL_US);
                    std::thread::sleep(Duration::from_millis(60));
                    loop_finish(&std::format!("0x{code:07X}"));
                    match res {
                        Ok(()) => { SENT.fetch_add(1, Ordering::Relaxed); info!("[ac] sent 0x{code:07X}"); }
                        Err(e) => warn!("[ac] send 0x{code:07X} failed: {e:?}"),
                    }
                }
                if let Some(on) = main_on { LAST_SENT_ON.store(on, Ordering::Relaxed); }
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
