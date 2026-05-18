mod crypto;
mod protocol;
mod session;

use log::warn;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

/// 255 = unknown (never set by web)
pub const LAMP_UNKNOWN: u32 = 255;

use crate::config;

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
/// `target`: -1=OFF, 0=idle, 1=ON, 2=warm-dim, 3=bright-white.
pub struct LampHandle {
    ip:      [u8; 4],
    port:    u16,
    key:     [u8; 16],
    version: u8,

    pub state:         Mutex<LampState>,
    target:            Mutex<i8>,
    suppress_until_ms: AtomicU32,
    retry_after_ms:    AtomicU32,

    // Persistent TCP session — reused across polls and refreshes.
    // Set to None on any I/O failure; reconnected lazily on next use.
    sess: Mutex<Option<session::Session>>,

    // Custom brightness/temp for target==4 (web-set values, Tuya range).
    custom_brightness: AtomicU32, // 10–1000
    custom_temp:       AtomicU32, // 0–1000

    // Last successfully applied values; reported via web /status.
    // LAMP_UNKNOWN = never applied.
    pub last_brightness: AtomicU32,
    pub last_temp:       AtomicU32,
}

impl LampHandle {
    pub fn new() -> Self {
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
            custom_brightness: AtomicU32::new(500),
            custom_temp:       AtomicU32::new(500),
            last_brightness:   AtomicU32::new(LAMP_UNKNOWN),
            last_temp:         AtomicU32::new(LAMP_UNKNOWN),
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
        self.retry_after_ms.store(0, Ordering::Relaxed);
        self.suppress_until_ms.store(now_ms().wrapping_add(10_000), Ordering::Relaxed);
    }

    pub fn queue_bright_white(&self) {
        *self.target.lock().unwrap() = 3;
        self.retry_after_ms.store(0, Ordering::Relaxed);
        self.suppress_until_ms.store(now_ms().wrapping_add(10_000), Ordering::Relaxed);
    }

    /// Set brightness (10–1000 Tuya) and color temperature (0=warm … 1000=cool).
    pub fn queue_brightness_temp(&self, brightness: u16, temp: u16) {
        self.custom_brightness.store(brightness.clamp(10, 1000) as u32, Ordering::Relaxed);
        self.custom_temp.store(temp.clamp(0, 1000) as u32, Ordering::Relaxed);
        *self.target.lock().unwrap() = 4;
        self.retry_after_ms.store(0, Ordering::Relaxed);
        self.suppress_until_ms.store(now_ms().wrapping_add(10_000), Ordering::Relaxed);
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

        self.retry_after_ms.store(now_ms().wrapping_add(5_000), Ordering::Relaxed);
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

fn parse_dps20(json: &str) -> Option<bool> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let dps20 = v
        .get("dps")
        .and_then(|d| d.get("20"))
        .or_else(|| v.get("data").and_then(|d| d.get("dps")).and_then(|d| d.get("20")));
    dps20.and_then(|v| v.as_bool().or_else(|| v.as_i64().map(|i| i != 0)))
}

fn now_ms() -> u32 {
    (unsafe { esp_idf_sys::esp_timer_get_time() } / 1000) as u32
}
