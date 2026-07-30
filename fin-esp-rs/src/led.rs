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
            // Used to default to true ("red on until wifi connects"), back
            // when red was a pure wifi-status indicator. Red is now
            // repurposed as the "cloud message pending" signal (cleared by
            // pressing any button, set on by the web API) - defaulting it
            // on at every boot would misreport a pending message that
            // isn't there, so this now starts false like the others.
            red:   AtomicBool::new(false),
            blue:  AtomicBool::new(false),
        }
    }

    /// DISABLED - screen (LCD) is physically disconnected, and red is now
    /// the "cloud message pending" indicator (see WRITE_PROTECTION/red LED
    /// usage in main.rs's button handlers and the web API). This used to
    /// tie green/red to wifi status whenever the screen toggled on/off;
    /// left as a real no-op (not deleted) in case the screen and its
    /// original wifi-status-LED behavior are ever wanted back - every
    /// call site in main.rs is unchanged and still calls this.
    pub fn on_screen_on(&self, _wifi: bool) {}

    /// DISABLED - see on_screen_on.
    pub fn on_screen_off(&self) {}

    /// DISABLED - wifi connect/disconnect used to auto-drive green/red as
    /// a connectivity indicator; that conflicts with red's new "cloud
    /// message pending" meaning (a wifi blip would silently clear or set
    /// it regardless of any real pending message). Kept as a real no-op,
    /// not deleted, in case the original wifi-status-LED behavior is ever
    /// wanted back on a LED that isn't doing double duty anymore.
    pub fn on_wifi_connect(&self, _screen_on: bool) {}

    /// DISABLED - see on_wifi_connect.
    pub fn on_wifi_disconnect(&self, _screen_on: bool) {}

    pub fn set_green(&self, on: bool) { self.green.store(on, Ordering::Relaxed); }
    pub fn set_red  (&self, on: bool) { self.red  .store(on, Ordering::Relaxed); }
    pub fn set_blue (&self, on: bool) { self.blue .store(on, Ordering::Relaxed); }
}
