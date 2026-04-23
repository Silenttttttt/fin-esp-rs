mod crypto;
mod protocol;
mod session;

use log::{info, warn};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::config;

/// Lamp state visible to the rest of the app.
#[derive(Clone, Copy, Debug)]
pub struct LampState {
    pub on: bool,
    pub known: bool,
}

impl Default for LampState {
    fn default() -> Self {
        Self {
            on: false,
            known: false,
        }
    }
}

/// High-level Tuya lamp controller.
pub struct TuyaLamp {
    key: [u8; 16],
    ip: [u8; 4],
    port: u16,
    version: u8,
}

impl TuyaLamp {
    pub fn new() -> Self {
        Self {
            key: config::TUYA_DEVICE_KEY,
            ip: config::TUYA_DEVICE_IP,
            port: config::TUYA_DEVICE_PORT,
            version: config::TUYA_PROTOCOL_VERSION,
        }
    }

    /// Query the lamp's current DPS state.
    pub fn refresh_status(&self) -> Option<bool> {
        match session::Session::connect(self.ip, self.port, &self.key, self.version) {
            Ok(mut sess) => {
                if !sess.negotiate_key() {
                    warn!("[tuya] key negotiation failed");
                    return None;
                }
                match sess.query_status() {
                    Some(json) => parse_dps20(&json),
                    None => {
                        warn!("[tuya] status query failed");
                        None
                    }
                }
            }
            Err(e) => {
                warn!("[tuya] connect failed: {e}");
                None
            }
        }
    }

    /// Send an explicit on/off command without querying current state first.
    pub fn set_on(&self, on: bool) -> Option<bool> {
        let dps = if on { r#""20":true"# } else { r#""20":false"# };
        self.send_dps(dps).map(|_| on)
    }

    /// Warm white, minimum brightness (as yellow and dim as possible).
    pub fn set_warm_dim(&self) -> Option<bool> {
        // DPS 20=on, 21=white mode, 22=brightness(10-1000), 23=color_temp(0=warm, 1000=cool)
        self.send_dps(r#""20":true,"21":"white","22":10,"23":0"#).map(|_| true)
    }

    /// Cool white, maximum brightness.
    pub fn set_bright_white(&self) -> Option<bool> {
        self.send_dps(r#""20":true,"21":"white","22":1000,"23":1000"#).map(|_| true)
    }

    /// Set brightness only (DPS 22: 10–1000). Does not change color or mode.
    pub fn set_brightness(&self, level: u16) -> bool {
        let dps = std::format!(r#""22":{}"#, level.clamp(10, 1000));
        self.send_dps(&dps).is_some()
    }

    fn send_dps(&self, dps_inner: &str) -> Option<()> {
        match session::Session::connect(self.ip, self.port, &self.key, self.version) {
            Ok(mut sess) => {
                if !sess.negotiate_key() {
                    warn!("[tuya] key negotiation failed");
                    return None;
                }
                if sess.send_dps_command(dps_inner) { Some(()) } else {
                    warn!("[tuya] command failed");
                    None
                }
            }
            Err(e) => { warn!("[tuya] connect failed: {e}"); None }
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

/// Thread-safe lamp handle.
///
/// `target`: -1=OFF, 0=idle, 1=ON, 2=warm-dim, 3=bright-white.
/// Any positive target displays as ON. `flip_target` treats all positive targets
/// as "currently on" and flips to -1, and vice versa.
pub struct LampHandle {
    lamp: TuyaLamp,
    pub state: Mutex<LampState>,
    target: Mutex<i8>,
    suppress_until_ms: AtomicU32,
    /// Don't retry a failed command until this timestamp.
    retry_after_ms: AtomicU32,
}

impl LampHandle {
    pub fn new() -> Self {
        Self {
            lamp: TuyaLamp::new(),
            state: Mutex::new(LampState::default()),
            target: Mutex::new(0),
            suppress_until_ms: AtomicU32::new(0),
            retry_after_ms: AtomicU32::new(0),
        }
    }

    /// Toggle button: flip between off and on. Any pending mode (2/3) counts as ON.
    /// Returns the new desired on state for the optimistic display update.
    pub fn flip_target(&self, current_displayed_on: bool) -> bool {
        let mut t = self.target.lock().unwrap();
        *t = if *t == 0 {
            if current_displayed_on { -1 } else { 1 }
        } else if *t > 0 {
            -1 // any pending ON / mode → cancel to OFF
        } else {
            1  // pending OFF → ON
        };
        let want_on = *t > 0;
        drop(t);
        self.retry_after_ms.store(0, Ordering::Relaxed);
        self.suppress_until_ms.store(now_ms().wrapping_add(10_000), Ordering::Relaxed);
        want_on
    }

    /// Warm dim button: queue a warm-white minimum-brightness command.
    pub fn queue_warm_dim(&self) {
        *self.target.lock().unwrap() = 2;
        self.retry_after_ms.store(0, Ordering::Relaxed);
        self.suppress_until_ms.store(now_ms().wrapping_add(10_000), Ordering::Relaxed);
    }

    /// Bright white button: queue a cool-white maximum-brightness command.
    pub fn queue_bright_white(&self) {
        *self.target.lock().unwrap() = 3;
        self.retry_after_ms.store(0, Ordering::Relaxed);
        self.suppress_until_ms.store(now_ms().wrapping_add(10_000), Ordering::Relaxed);
    }

    /// Set brightness directly (bypasses the target queue). Returns true on success.
    pub fn apply_brightness(&self, level: u16) -> bool {
        self.lamp.set_brightness(level)
    }

    /// Called from the lamp bridge thread. Executes any pending target command.
    /// Returns true when a command completes and state changed.
    pub fn poll(&self) -> bool {
        let target = *self.target.lock().unwrap();
        if target == 0 { return false; }

        // Don't spam on failure — wait for retry cooldown.
        let now = now_ms();
        let retry_at = self.retry_after_ms.load(Ordering::Relaxed);
        if retry_at > 0 && now.wrapping_sub(retry_at) > u32::MAX / 2 {
            return false;
        }

        let result = match target {
            1  => self.lamp.set_on(true),
           -1  => self.lamp.set_on(false),
            2  => self.lamp.set_warm_dim(),
            3  => self.lamp.set_bright_white(),
            _  => return false,
        };
        if let Some(actual_on) = result {
            let mut st = self.state.lock().unwrap();
            st.on = actual_on;
            st.known = true;
            drop(st);

            // Clear target only if it hasn't changed during the TCP call.
            let mut t = self.target.lock().unwrap();
            if *t == target { *t = 0; }
            drop(t);

            self.retry_after_ms.store(0, Ordering::Relaxed);
            self.suppress_until_ms.store(now_ms().wrapping_add(5_000), Ordering::Relaxed);
            return true;
        }

        // Command failed — back off 5 s before retrying.
        self.retry_after_ms.store(now_ms().wrapping_add(5_000), Ordering::Relaxed);
        false
    }

    /// The state the LCD should display: target state if a command is pending,
    /// otherwise the last confirmed state.
    pub fn display_state(&self) -> LampState {
        let target = *self.target.lock().unwrap();
        if target != 0 {
            LampState { on: target > 0, known: true }
        } else {
            *self.state.lock().unwrap()
        }
    }

    /// Refresh lamp status over LAN. No-op within the suppress window.
    pub fn refresh(&self) {
        let deadline = self.suppress_until_ms.load(Ordering::Relaxed);
        if deadline > 0 && now_ms().wrapping_sub(deadline) > u32::MAX / 2 {
            return;
        }
        if let Some(on) = self.lamp.refresh_status() {
            let mut st = self.state.lock().unwrap();
            st.on = on;
            st.known = true;
        }
    }
}

fn now_ms() -> u32 {
    (unsafe { esp_idf_sys::esp_timer_get_time() } / 1000) as u32
}
