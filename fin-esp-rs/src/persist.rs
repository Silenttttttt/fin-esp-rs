use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs, NvsDefault};
use crate::config::Screen;

const NS: &str = "state";

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
}
