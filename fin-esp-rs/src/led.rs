use std::sync::atomic::{AtomicBool, Ordering};

/// Persistent state for the three physical LEDs.
/// All writes from firmware and web go through this struct.
/// Hardware is driven by the main loop reading these atomics.
pub struct LedState {
    pub green: AtomicBool,
    pub red:   AtomicBool,
    pub blue:  AtomicBool,
}

impl LedState {
    pub fn new() -> Self {
        Self {
            green: AtomicBool::new(false),
            red:   AtomicBool::new(true), // red on at boot until wifi connects
            blue:  AtomicBool::new(false),
        }
    }

    /// Screen turned on: green reflects wifi, red reflects no-wifi.
    pub fn on_screen_on(&self, wifi: bool) {
        self.green.store(wifi,  Ordering::Relaxed);
        self.red  .store(!wifi, Ordering::Relaxed);
    }

    /// Screen turned off: both indicator LEDs off.
    pub fn on_screen_off(&self) {
        self.green.store(false, Ordering::Relaxed);
        self.red  .store(false, Ordering::Relaxed);
    }

    /// Wifi connected: green on (if screen is on), red off.
    pub fn on_wifi_connect(&self, screen_on: bool) {
        if screen_on { self.green.store(true, Ordering::Relaxed); }
        self.red.store(false, Ordering::Relaxed);
    }

    /// Wifi disconnected: green off, red on (if screen is on).
    pub fn on_wifi_disconnect(&self, screen_on: bool) {
        self.green.store(false, Ordering::Relaxed);
        if screen_on { self.red.store(true, Ordering::Relaxed); }
    }

    pub fn set_green(&self, on: bool) { self.green.store(on, Ordering::Relaxed); }
    pub fn set_red  (&self, on: bool) { self.red  .store(on, Ordering::Relaxed); }
    pub fn set_blue (&self, on: bool) { self.blue .store(on, Ordering::Relaxed); }
}
