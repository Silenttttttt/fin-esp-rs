pub type Glyph = [u8; 8];

// Lamp
pub const GLYPH_LAMP_ON: Glyph = [0b01110, 0b11111, 0b11111, 0b11111, 0b11111, 0b01110, 0b00100, 0b00000];
pub const GLYPH_LAMP_OFF: Glyph = [0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110, 0b00100, 0b00000];
pub const GLYPH_LAMP_UNK: Glyph = [0b01110, 0b10001, 0b10101, 0b10001, 0b10101, 0b01110, 0b00100, 0b00000];

// WiFi
pub const GLYPH_WIFI_ON: Glyph = [0b00000, 0b00100, 0b01110, 0b11111, 0b00100, 0b00100, 0b00100, 0b00000];
pub const GLYPH_WIFI_OFF: Glyph = [0b10001, 0b01010, 0b00100, 0b01010, 0b10001, 0b00000, 0b00000, 0b00000];

// Price change arrows (slots 2 and 3 — freed from SUN/CLOUD, weather uses SUN_CLOUD for all conditions)
pub const GLYPH_UP_ARROW: Glyph   = [0x00, 0x04, 0x0E, 0x1F, 0x00, 0x00, 0x00, 0x00]; // ▲
pub const GLYPH_DOWN_ARROW: Glyph = [0x00, 0x1F, 0x0E, 0x04, 0x00, 0x00, 0x00, 0x00]; // ▼

// Weather (slot 4 only — SUN and CLOUD slots repurposed for arrows above)
pub const GLYPH_SUN_CLOUD: Glyph = [0b00100, 0b10101, 0b01110, 0b11111, 0b11111, 0b01110, 0b00000, 0b00000];
pub const GLYPH_RAIN: Glyph = [0b00000, 0b00000, 0b00100, 0b00100, 0b10101, 0b01010, 0b00000, 0b00000];

// Degree symbol
pub const GLYPH_DEGREE: Glyph = [0x06, 0x09, 0x09, 0x06, 0x00, 0x00, 0x00, 0x00];

// Asset icons
pub const GLYPH_ASSET_BTC: Glyph = [0b01110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b01110, 0b00000];
pub const GLYPH_ASSET_SOL: Glyph = [0x0F, 0x10, 0x0E, 0x01, 0x1E, 0x04, 0x0A, 0x04];
pub const GLYPH_ASSET_GOLD: Glyph = [0x00, 0x00, 0x0E, 0x1B, 0x11, 0x1F, 0x00, 0x00];
pub const GLYPH_ASSET_OIL: Glyph = [0b00100, 0b00100, 0b01010, 0b01010, 0b10001, 0b10001, 0b01110, 0b00000];

// Hourglass animation (17 frames)
pub const HOURGLASS: [Glyph; 17] = [
    [0x1F, 0x1F, 0x0E, 0x04, 0x0A, 0x11, 0x11, 0x0E],
    [0x1F, 0x1F, 0x0E, 0x04, 0x0E, 0x11, 0x11, 0x0E],
    [0x1F, 0x1F, 0x0E, 0x04, 0x0A, 0x15, 0x11, 0x0E],
    [0x1F, 0x1F, 0x0E, 0x04, 0x0A, 0x11, 0x15, 0x0E],
    [0x1F, 0x1F, 0x0E, 0x04, 0x0E, 0x11, 0x15, 0x0E],
    [0x1F, 0x1F, 0x0E, 0x04, 0x0A, 0x15, 0x15, 0x0E],
    [0x1F, 0x1F, 0x0E, 0x04, 0x0A, 0x11, 0x17, 0x0E],
    [0x1F, 0x1F, 0x0E, 0x04, 0x0E, 0x11, 0x17, 0x0E],
    [0x1F, 0x1F, 0x0E, 0x04, 0x0A, 0x15, 0x17, 0x0E],
    [0x1F, 0x1F, 0x0E, 0x04, 0x0A, 0x11, 0x1F, 0x0E],
    [0x1F, 0x1F, 0x0A, 0x04, 0x0E, 0x11, 0x1F, 0x0E],
    [0x1F, 0x1B, 0x0E, 0x04, 0x0A, 0x15, 0x1F, 0x0E],
    [0x1F, 0x1B, 0x0A, 0x04, 0x0E, 0x19, 0x1F, 0x0E],
    [0x1F, 0x19, 0x0E, 0x04, 0x0A, 0x1B, 0x1F, 0x0E],
    [0x1F, 0x11, 0x0E, 0x04, 0x0E, 0x1B, 0x1F, 0x0E],
    [0x1F, 0x11, 0x0E, 0x04, 0x0A, 0x1F, 0x1F, 0x0E],
    [0x1F, 0x11, 0x0A, 0x04, 0x0E, 0x1F, 0x1F, 0x0E],
];

// Chart glyphs are computed dynamically in chart.rs — no constants needed here.

use crate::config::Screen;

pub fn asset_glyph(screen: Screen) -> &'static Glyph {
    match screen {
        Screen::Btc => &GLYPH_ASSET_BTC,
        Screen::Sol => &GLYPH_ASSET_SOL,
        Screen::Gold => &GLYPH_ASSET_GOLD,
        Screen::Oil => &GLYPH_ASSET_OIL,
        Screen::UsdBrl => &GLYPH_ASSET_BTC, // unused — UsdBrl shows ASCII '$' directly
    }
}

/// Build a countdown glyph (5-wide, 6 rows of dots, removing one per elapsed second).
/// `dots_remaining` is 0..=30.  Matches C++ buildApiCountdownGlyph() exactly:
/// dots fill rows 1–6 top-to-bottom, left-to-right; removal starts from top-left.
pub fn countdown_glyph(dots_remaining: u8) -> Glyph {
    let mut g = [0u8; 8];
    let total = 30u8;
    let remaining = dots_remaining.min(total);
    let removed = total - remaining; // how many top-left dots are gone
    for dot_idx in 0u8..total {
        if dot_idx < removed {
            continue; // this dot has been removed (top-left first)
        }
        let row = 1 + (dot_idx / 5) as usize; // rows 1..=6, top to bottom
        let col = (dot_idx % 5) as usize;      // cols 0..=4, left to right
        g[row] |= 1 << (4 - col);
    }
    g
}
