use crate::api::MarketData;
use crate::config::Screen;
use crate::lcd::Lcd;
use crate::tuya::LampState;

/// Row + CGRAM dirty-tracking cache.
///
/// CGRAM is always written before DDRAM so icons are already correct by the
/// time the row bytes land on the display — prevents wrong-glyph flashes.
pub struct RowCache {
    rows:  [[u8; 20]; 4],
    cgram: [[u8;  8]; 8], // last-written glyph per slot; 0xFF-filled = never written
}

impl RowCache {
    pub fn new() -> Self {
        Self {
            rows:  [[0xFF; 20]; 4],
            cgram: [[0xFF;  8]; 8],
        }
    }

    pub fn invalidate(&mut self) {
        self.rows = [[0xFF; 20]; 4];
        // cgram intentionally NOT invalidated — LCD hardware retains it, and the cache
        // tracks it accurately. Resetting forced a full 8-slot rewrite (~20 ms flicker)
        // on every screen transition.
    }

    /// Write a row to the LCD, skipping if content is unchanged (row 0 always writes).
    pub fn commit(&mut self, lcd: &mut Lcd, row: u8, data: &[u8; 20]) {
        let r = row as usize;
        if r > 0 && r < 4 && self.rows[r] == *data {
            return;
        }
        if r < 4 { self.rows[r] = *data; }
        lcd.set_cursor(0, row);
        lcd.write_raw(data);
    }

    /// Write a glyph to a CGRAM slot only if it changed since last write.
    pub(crate) fn write_glyph(&mut self, lcd: &mut Lcd, slot: u8, glyph: &[u8; 8]) {
        let s = slot as usize;
        if self.cgram[s] != *glyph {
            self.cgram[s] = *glyph;
            lcd.create_char(slot, glyph);
        }
    }
}

/// All UI state needed for rendering.
pub struct UiState {
    pub screen: Screen,
    pub data: MarketData,
    pub lamp: LampState,
    pub wifi_connected: bool,
    pub fetching: bool,
    pub loading_frame: u16,
    pub lamp_loading_frame: u16,
    pub fetch_completed_at: u64,
    pub lamp_anim_until: u64,
    pub pot_enabled: bool,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            screen: Screen::Btc,
            data: MarketData::default(),
            lamp: LampState::default(),
            wifi_connected: false,
            fetching: false,
            loading_frame: 0,
            lamp_loading_frame: 0,
            fetch_completed_at: 0,
            lamp_anim_until: 0,
            pot_enabled: true,
        }
    }
}
