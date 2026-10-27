use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs, NvsDefault};
use crate::config::Screen;

const NS: &str = "state";

#[derive(Clone)]
pub struct Persist {
    nvs: EspDefaultNvsPartition,
}

impl Persist {
    pub fn new(nvs: EspDefaultNvsPartition) -> Self {
        Self { nvs }
    }

    fn open(&self) -> Option<EspNvs<NvsDefault>> {
        EspNvs::new(self.nvs.clone(), NS, true).ok()
    }

    pub fn load_screen_forced(&self) -> bool {
        self.open()
            .and_then(|mut n| n.get_u8("forced_off").ok().flatten())
            .map(|v| v != 0)
            .unwrap_or(false)
    }

    pub fn save_screen_forced(&self, v: bool) {
        if let Some(mut n) = self.open() {
            let _ = n.set_u8("forced_off", v as u8);
        }
    }

    pub fn load_pot_enabled(&self) -> bool {
        self.open()
            .and_then(|mut n| n.get_u8("pot_on").ok().flatten())
            .map(|v| v != 0)
            .unwrap_or(true)
    }

    pub fn save_pot_enabled(&self, v: bool) {
        if let Some(mut n) = self.open() {
            let _ = n.set_u8("pot_on", v as u8);
        }
    }

    pub fn load_screen(&self) -> Option<Screen> {
        self.open()
            .and_then(|mut n| n.get_u8("screen").ok().flatten())
            .and_then(Screen::from_u8)
    }

    pub fn save_screen(&self, s: Screen) {
        if let Some(mut n) = self.open() {
            let _ = n.set_u8("screen", s as u8);
        }
    }

    // 2026-10-09, real bug found live: Muni reported the IR remote's brightness button
    // "sometimes" switching the lamp's colour back to white, which should be structurally
    // impossible (see tuya/mod.rs's own step_brightness -- brightness alone never touches DPS
    // 21/24 when in colour mode). Root cause: LampHandle's own colour/white mode tracking
    // (last_mode_is_colour) and its last-chosen colour/white values were ALL purely in-memory,
    // resetting to their hardcoded defaults (white mode, hue=30) on every Fin-ESP reboot --
    // including the already-known, already-documented background crash-reboot bug that
    // self-recovers in ~10s with no other visible effect. The real lamp doesn't forget its own
    // state on FIN-ESP's reboot, so after any such reboot while the lamp was showing a colour,
    // the very next brightness press would incorrectly take the white-mode branch (Fin-ESP
    // now THINKS it's in white mode) and force the lamp to white -- exactly matching "brightness
    // sometimes changes the colour", and exactly as intermittent as the underlying crash is.
    // u16 covers every one of these (hue 0-360, sat/val/temp 0-1000, brightness 10-1000).
    pub fn load_lamp_mode_is_colour(&self) -> bool {
        self.open()
            .and_then(|mut n| n.get_u8("lamp_is_colour").ok().flatten())
            .map(|v| v != 0)
            .unwrap_or(false)
    }

    pub fn save_lamp_mode_is_colour(&self, v: bool) {
        if let Some(mut n) = self.open() {
            let _ = n.set_u8("lamp_is_colour", v as u8);
        }
    }

    pub fn load_lamp_hue(&self) -> Option<u16> {
        self.open().and_then(|mut n| n.get_u16("lamp_hue").ok().flatten())
    }

    pub fn save_lamp_hue(&self, v: u16) {
        if let Some(mut n) = self.open() {
            let _ = n.set_u16("lamp_hue", v);
        }
    }

    pub fn load_lamp_sat(&self) -> Option<u16> {
        self.open().and_then(|mut n| n.get_u16("lamp_sat").ok().flatten())
    }

    pub fn save_lamp_sat(&self, v: u16) {
        if let Some(mut n) = self.open() {
            let _ = n.set_u16("lamp_sat", v);
        }
    }

    pub fn load_lamp_val(&self) -> Option<u16> {
        self.open().and_then(|mut n| n.get_u16("lamp_val").ok().flatten())
    }

    pub fn save_lamp_val(&self, v: u16) {
        if let Some(mut n) = self.open() {
            let _ = n.set_u16("lamp_val", v);
        }
    }

    pub fn load_lamp_brightness(&self) -> Option<u16> {
        self.open().and_then(|mut n| n.get_u16("lamp_bright").ok().flatten())
    }

    pub fn save_lamp_brightness(&self, v: u16) {
        if let Some(mut n) = self.open() {
            let _ = n.set_u16("lamp_bright", v);
        }
    }

    pub fn load_lamp_temp(&self) -> Option<u16> {
        self.open().and_then(|mut n| n.get_u16("lamp_temp").ok().flatten())
    }

    pub fn save_lamp_temp(&self, v: u16) {
        if let Some(mut n) = self.open() {
            let _ = n.set_u16("lamp_temp", v);
        }
    }

    /// LG AC state packed into one u32 by ac.rs (power/mode/temp/fan/swing). The AC can't
    /// report its own state over IR, so this is the only record of it across reboots.
    pub fn load_ac_state(&self) -> Option<u32> {
        self.open().and_then(|mut n| n.get_u32("ac_state").ok().flatten())
    }

    /// Unix time at which Fin-ESP sends the AC OFF (sleep timer), 0 = none.
    pub fn load_ac_off_at(&self) -> Option<u32> {
        self.open().and_then(|mut n| n.get_u32("ac_off_at").ok().flatten())
    }

    pub fn save_ac_off_at(&self, v: u32) {
        if let Some(mut n) = self.open() {
            let _ = n.set_u32("ac_off_at", v);
        }
    }

    pub fn save_ac_state(&self, v: u32) {
        if let Some(mut n) = self.open() {
            let _ = n.set_u32("ac_state", v);
        }
    }

    /// AC-cloud runtime kill switch (ac_cloud.rs). true = don't start it even if compiled enabled.
    pub fn load_ac_cloud_off(&self) -> bool {
        self.open()
            .and_then(|mut n| n.get_u8("acc_off").ok().flatten())
            .map(|v| v != 0)
            .unwrap_or(false)
    }

    pub fn save_ac_cloud_off(&self, v: bool) {
        if let Some(mut n) = self.open() {
            let _ = n.set_u8("acc_off", v as u8);
        }
    }
}
