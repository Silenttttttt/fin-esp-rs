mod crypto;
mod protocol;
mod session;

use log::warn;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// 255 = unknown (never set by web)
pub const LAMP_UNKNOWN: u32 = 255;

use crate::config;
use crate::persist::Persist;

#[derive(Clone, Copy, Debug)]
pub struct LampState {
    pub on: bool,
    pub known: bool,
}

impl Default for LampState {
    fn default() -> Self { Self { on: false, known: false } }
}

/// Thread-safe lamp handle with a persistent Tuya LAN session.
///
/// `target`: -1=OFF, 0=idle, 1=ON, 2=warm-dim, 3=bright-white, 4=custom white, 5=colour.
pub struct LampHandle {
    ip:      [u8; 4],
    port:    u16,
    key:     [u8; 16],
    version: u8,

    pub state:         Mutex<LampState>,
    target:            Mutex<i8>,
    suppress_until_ms: AtomicU32,
    retry_after_ms:    AtomicU32,

    sess: Mutex<Option<session::Session>>,

    // target==4: white mode with custom values (Tuya range).
    custom_brightness: AtomicU32, // 10–1000
    custom_temp:       AtomicU32, // 0–1000

    // target==5: colour mode (Tuya range).
    custom_hue: AtomicU32, // 0–360
    custom_sat: AtomicU32, // 0–1000
    custom_val: AtomicU32, // 0–1000

    pub last_brightness: AtomicU32,
    pub last_temp:       AtomicU32,

    // Centralized 2026-10-07 -- previously only ir_remote.rs tracked "is the lamp currently in
    // colour mode" (its own local IN_COLOUR_MODE static), discovered as a real bug 2026-09-22:
    // nudging "brightness" while a color was active used to silently force the bulb back to
    // white mode, since queue_brightness_temp() always sends "21":"white". That local tracking
    // was explicitly "not perfectly authoritative if something else (physical buttons, web UI)
    // changes the mode in between" -- moving it here makes it correct for ANY caller (IR
    // remote, web UI, Jarvis, anything else), not just the one that happened to set it last.
    // 2026-10-09, real bug found live: this (and the custom_* values below) used to be purely
    // in-memory, resetting to hardcoded defaults on every Fin-ESP reboot -- including the
    // already-known background crash-reboot bug -- while the REAL lamp doesn't forget its own
    // state on FIN-ESP's reboot. The next brightness press after any such reboot would
    // incorrectly take the white-mode branch and force the lamp to white, looking exactly like
    // "brightness sometimes changes the colour". See persist.rs's own comment on these fields.
    // Persisted now, loaded in `new()`, saved on every queue_colour/queue_brightness_temp call.
    last_mode_is_colour: AtomicBool,
    persist: Persist,
}

impl LampHandle {
    pub fn new(persist: Persist) -> Self {
        Self {
            ip:      config::TUYA_DEVICE_IP,
            port:    config::TUYA_DEVICE_PORT,
            key:     config::TUYA_DEVICE_KEY,
            version: config::TUYA_PROTOCOL_VERSION,
            state:             Mutex::new(LampState::default()),
            target:            Mutex::new(0),
            suppress_until_ms: AtomicU32::new(0),
            retry_after_ms:    AtomicU32::new(0),
            sess:              Mutex::new(None),
            custom_brightness: AtomicU32::new(persist.load_lamp_brightness().unwrap_or(500) as u32),
            custom_temp:       AtomicU32::new(persist.load_lamp_temp().unwrap_or(500) as u32),
            custom_hue:        AtomicU32::new(persist.load_lamp_hue().unwrap_or(30) as u32),
            custom_sat:        AtomicU32::new(persist.load_lamp_sat().unwrap_or(1000) as u32),
            custom_val:        AtomicU32::new(persist.load_lamp_val().unwrap_or(500) as u32),
            last_brightness:   AtomicU32::new(LAMP_UNKNOWN),
            last_temp:         AtomicU32::new(LAMP_UNKNOWN),
            last_mode_is_colour: AtomicBool::new(persist.load_lamp_mode_is_colour()),
            persist,
        }
    }

    // ── Session management ─────────────────────────────────────────────────────

    /// Connect and negotiate a session key if none is live. Returns false on failure.
    fn connect_if_needed(&self, guard: &mut Option<session::Session>) -> bool {
        if guard.is_some() {
            return true;
        }
        match session::Session::connect(self.ip, self.port, &self.key, self.version) {
            Ok(mut s) => {
                if s.negotiate_key() {
                    *guard = Some(s);
                    true
                } else {
                    warn!("[tuya] key negotiation failed");
                    false
                }
            }
            Err(e) => {
                warn!("[tuya] connect failed: {e}");
                false
            }
        }
    }

    fn do_query_status(&self) -> Option<bool> {
        let mut guard = self.sess.lock().unwrap();
        if !self.connect_if_needed(&mut guard) {
            return None;
        }
        match guard.as_mut().unwrap().query_status() {
            Some(json) => parse_dps20(&json),
            None => {
                warn!("[tuya] status query failed — resetting session");
                *guard = None;
                None
            }
        }
    }

    fn do_send_dps(&self, dps: &str) -> Option<()> {
        let mut guard = self.sess.lock().unwrap();
        if !self.connect_if_needed(&mut guard) {
            return None;
        }
        if guard.as_mut().unwrap().send_dps_command(dps) {
            Some(())
        } else {
            warn!("[tuya] dps send failed — resetting session");
            *guard = None;
            None
        }
    }

    // ── Public interface ───────────────────────────────────────────────────────

    /// Cheap check for whether `poll()` currently has any real work to do -- lets a caller
    /// avoid taking a shared network lock around `poll()` on the (overwhelming majority of)
    /// calls where it would just return `false` immediately anyway.
    pub fn has_pending_target(&self) -> bool {
        *self.target.lock().unwrap() != 0
    }

    /// Toggle button: flip between off and on.
    pub fn flip_target(&self, current_displayed_on: bool) -> bool {
        let mut t = self.target.lock().unwrap();
        *t = if *t == 0 {
            if current_displayed_on { -1 } else { 1 }
        } else if *t > 0 {
            -1
        } else {
            1
        };
        let want_on = *t > 0;
        drop(t);
        self.retry_after_ms.store(0, Ordering::Relaxed);
        self.suppress_until_ms.store(now_ms().wrapping_add(10_000), Ordering::Relaxed);
        want_on
    }

    pub fn queue_on(&self) {
        *self.target.lock().unwrap() = 1;
        self.retry_after_ms.store(0, Ordering::Relaxed);
        self.suppress_until_ms.store(now_ms().wrapping_add(10_000), Ordering::Relaxed);
    }

    pub fn queue_off(&self) {
        *self.target.lock().unwrap() = -1;
        self.retry_after_ms.store(0, Ordering::Relaxed);
        self.suppress_until_ms.store(now_ms().wrapping_add(10_000), Ordering::Relaxed);
    }

    pub fn queue_warm_dim(&self) {
        *self.target.lock().unwrap() = 2;
        self.last_mode_is_colour.store(false, Ordering::Relaxed);
        self.retry_after_ms.store(0, Ordering::Relaxed);
        self.suppress_until_ms.store(now_ms().wrapping_add(10_000), Ordering::Relaxed);
    }

    pub fn queue_bright_white(&self) {
        *self.target.lock().unwrap() = 3;
        self.last_mode_is_colour.store(false, Ordering::Relaxed);
        self.retry_after_ms.store(0, Ordering::Relaxed);
        self.suppress_until_ms.store(now_ms().wrapping_add(10_000), Ordering::Relaxed);
    }

    /// Set brightness (10–1000 Tuya) and color temperature (0=warm … 1000=cool).
    pub fn queue_brightness_temp(&self, brightness: u16, temp: u16) {
        let brightness = brightness.clamp(10, 1000);
        let temp = temp.clamp(0, 1000);
        self.custom_brightness.store(brightness as u32, Ordering::Relaxed);
        self.custom_temp.store(temp as u32, Ordering::Relaxed);
        *self.target.lock().unwrap() = 4;
        self.last_mode_is_colour.store(false, Ordering::Relaxed);
        self.retry_after_ms.store(0, Ordering::Relaxed);
        self.suppress_until_ms.store(now_ms().wrapping_add(10_000), Ordering::Relaxed);
        self.persist.save_lamp_brightness(brightness);
        self.persist.save_lamp_temp(temp);
        self.persist.save_lamp_mode_is_colour(false);
    }

    /// Set colour mode with HSV in Tuya units (hue 0–360, sat 0–1000, val 0–1000).
    pub fn queue_colour(&self, hue: u16, sat: u16, val: u16) {
        let hue = hue.clamp(0, 360);
        let sat = sat.clamp(0, 1000);
        let val = val.clamp(0, 1000);
        self.custom_hue.store(hue as u32, Ordering::Relaxed);
        self.custom_sat.store(sat as u32, Ordering::Relaxed);
        self.custom_val.store(val as u32, Ordering::Relaxed);
        *self.target.lock().unwrap() = 5;
        self.last_mode_is_colour.store(true, Ordering::Relaxed);
        self.retry_after_ms.store(0, Ordering::Relaxed);
        self.suppress_until_ms.store(now_ms().wrapping_add(10_000), Ordering::Relaxed);
        self.persist.save_lamp_hue(hue);
        self.persist.save_lamp_sat(sat);
        self.persist.save_lamp_val(val);
        self.persist.save_lamp_mode_is_colour(true);
    }

    /// Nudge brightness by `delta` (Tuya 10-1000 range) without needing to know the lamp's
    /// current color -- the single authoritative replacement for what used to be ir_remote.rs's
    /// own local, non-authoritative IN_COLOUR_MODE/LAST_HUE/LAST_VAL tracking (see
    /// `last_mode_is_colour`'s own comment). Works correctly from ANY caller: in colour mode,
    /// nudges V (brightness-in-HSV) while keeping the current hue/saturation exactly as last
    /// set; in white mode, nudges brightness while keeping the current color temp exactly as
    /// last applied (reading `last_brightness`/`last_temp`, not the `custom_*` staging fields,
    /// so this is correct even right after a warm_dim/bright_white preset, which bypass
    /// `custom_brightness`/`custom_temp` entirely).
    pub fn step_brightness(&self, delta: i32) {
        if self.last_mode_is_colour.load(Ordering::Relaxed) {
            let hue = self.custom_hue.load(Ordering::Relaxed) as u16;
            let sat = self.custom_sat.load(Ordering::Relaxed) as u16;
            let cur_val = self.custom_val.load(Ordering::Relaxed) as i32;
            let new_val = (cur_val + delta).clamp(10, 1000) as u16;
            self.queue_colour(hue, sat, new_val);
        } else {
            let cur_b = self.last_brightness.load(Ordering::Relaxed);
            let cur_b = if cur_b == LAMP_UNKNOWN { 500 } else { cur_b as i32 };
            let cur_t = self.last_temp.load(Ordering::Relaxed);
            let cur_t = if cur_t == LAMP_UNKNOWN { 500 } else { cur_t } as u16;
            let new_b = (cur_b + delta).clamp(10, 1000) as u16;
            self.queue_brightness_temp(new_b, cur_t);
        }
    }

    /// Last applied brightness as 0–100 percentage (LAMP_UNKNOWN if never set).
    pub fn brightness_pct(&self) -> u32 {
        let b = self.last_brightness.load(Ordering::Relaxed);
        if b == LAMP_UNKNOWN { return LAMP_UNKNOWN; }
        (b.saturating_sub(10)) * 100 / 990
    }

    /// Last applied color temp as 0–100 (0=warm, 100=cool; LAMP_UNKNOWN if never set).
    pub fn temp_pct(&self) -> u32 {
        let t = self.last_temp.load(Ordering::Relaxed);
        if t == LAMP_UNKNOWN { return LAMP_UNKNOWN; }
        t * 100 / 1000
    }

    /// Execute any pending target command. Returns true when state changes.
    pub fn poll(&self) -> bool {
        let target = *self.target.lock().unwrap();
        if target == 0 { return false; }

        let now = now_ms();
        let retry_at = self.retry_after_ms.load(Ordering::Relaxed);
        if retry_at > 0 && now.wrapping_sub(retry_at) > u32::MAX / 2 {
            return false;
        }

        let result: Option<bool> = match target {
             1 => self.do_send_dps(r#""20":true"#).map(|_| true),
            -1 => self.do_send_dps(r#""20":false"#).map(|_| false),
             2 => self.do_send_dps(r#""20":true,"21":"white","22":10,"23":0"#).inspect(|_| {
                    self.last_brightness.store(10, Ordering::Relaxed);
                    self.last_temp.store(0, Ordering::Relaxed);
                 }).map(|_| true),
             3 => self.do_send_dps(r#""20":true,"21":"white","22":1000,"23":1000"#).inspect(|_| {
                    self.last_brightness.store(1000, Ordering::Relaxed);
                    self.last_temp.store(1000, Ordering::Relaxed);
                 }).map(|_| true),
             4 => {
                let b = self.custom_brightness.load(Ordering::Relaxed).clamp(10, 1000);
                let t = self.custom_temp.load(Ordering::Relaxed).clamp(0, 1000);
                let dps = std::format!(r#""20":true,"21":"white","22":{},"23":{}"#, b, t);
                self.do_send_dps(&dps).inspect(|_| {
                    self.last_brightness.store(b, Ordering::Relaxed);
                    self.last_temp.store(t, Ordering::Relaxed);
                }).map(|_| true)
             }
             5 => {
                let h  = self.custom_hue.load(Ordering::Relaxed).clamp(0, 360);
                let sv = self.custom_sat.load(Ordering::Relaxed).clamp(0, 1000);
                let v  = self.custom_val.load(Ordering::Relaxed).clamp(10, 1000);
                // Power on first — bundling "20":true with colour DPS can reset the
                // bulb to white mode before the colour DPS lands on some firmware.
                self.do_send_dps(r#""20":true"#);
                // DPS 24 format: HHHHSSSSVVVV (12 hex chars, Tuya colour mode).
                let colour = std::format!("{:04x}{:04x}{:04x}", h, sv, v);
                warn!("[tuya] colour cmd h={} s={} v={} hex={}", h, sv, v, colour);
                let dps = std::format!(r#""21":"colour","24":"{}""#, colour);
                self.do_send_dps(&dps).inspect(|_| {
                    self.last_brightness.store(LAMP_UNKNOWN, Ordering::Relaxed);
                    self.last_temp.store(LAMP_UNKNOWN, Ordering::Relaxed);
                }).map(|_| true)
             }
             _ => return false,
        };

        if let Some(actual_on) = result {
            let mut st = self.state.lock().unwrap();
            st.on = actual_on;
            st.known = true;
            drop(st);

            let mut t = self.target.lock().unwrap();
            if *t == target { *t = 0; }
            drop(t);

            self.retry_after_ms.store(0, Ordering::Relaxed);
            self.suppress_until_ms.store(now_ms().wrapping_add(5_000), Ordering::Relaxed);
            return true;
        }

        // Was 5s -- combined with session.rs's old 4s connect_timeout, a cold session
        // needing even 1-2 failed reconnect attempts before succeeding compounded into the
        // real ~15-20s felt delay reported live 2026-08-31 ("press once, 15s, press again,
        // instant"). Tightened alongside session.rs's connect_timeout (now 1s, see that
        // file's own comment) so a bad cycle costs ~1-1.5s instead of ~5-9s.
        self.retry_after_ms.store(now_ms().wrapping_add(1_500), Ordering::Relaxed);
        false
    }

    /// State to display: pending target if a command is queued, else last confirmed.
    pub fn display_state(&self) -> LampState {
        let target = *self.target.lock().unwrap();
        if target != 0 {
            LampState { on: target > 0, known: true }
        } else {
            *self.state.lock().unwrap()
        }
    }

    /// Confirm lamp state over LAN. No-op within the suppress window.
    pub fn refresh(&self) {
        let deadline = self.suppress_until_ms.load(Ordering::Relaxed);
        if deadline > 0 && now_ms().wrapping_sub(deadline) > u32::MAX / 2 {
            return;
        }
        if let Some(on) = self.do_query_status() {
            let mut st = self.state.lock().unwrap();
            st.on = on;
            st.known = true;
        }
    }
}

/// Finds DPS key 20 (on/off) directly in the raw JSON string instead of building a full
/// `serde_json::Value` parse tree just to read one boolean -- ported from the same fix in the
/// standalone `tuya-lan-rs` crate (this module is a vendored copy of it). Matters here
/// specifically because this runs on every 5-second lamp poll on an already heap-tight device --
/// avoiding a parse-tree allocation on that hot path is a real, not just theoretical, saving.
fn parse_dps20(json: &str) -> Option<bool> {
    let after = json.find("\"20\":")?;
    let rest = json[after + 5..].trim_start();
    if rest.starts_with("true") {
        Some(true)
    } else if rest.starts_with("false") {
        Some(false)
    } else {
        let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
        rest.get(..end)?.parse::<i64>().ok().map(|n| n != 0)
    }
}

fn now_ms() -> u32 {
    (unsafe { esp_idf_sys::esp_timer_get_time() } / 1000) as u32
}
