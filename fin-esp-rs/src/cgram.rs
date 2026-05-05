/// CGRAM slot manager for the HD44780 LCD (8 custom-char slots, 0-7).
///
/// Two consumers share the 8 slots exclusively in time:
///   - `SandGrid` — claims slots dynamically as particles spread (WiFi connecting)
///   - Ticker screen — uses the 8 fixed named slots below (after WiFi)
///
/// `CgramPool` is an allocator used by the sand simulation.
/// When sand ends, call `pool.release(lcd)` to write blank glyphs and hand
/// the slots back cleanly before the screen renderer claims them.

use crate::lcd::Lcd;

// ── Fixed slot assignments for the ticker screen ──────────────────────────────
// These are used by screen.rs after sand is done.
pub const SLOT_LAMP:       u8 = 0;
pub const SLOT_WIFI:       u8 = 1;
pub const SLOT_UP_ARROW:   u8 = 2; // shared slot — glyph set to up or down each frame
pub const SLOT_POT:        u8 = 3; // pot enabled/disabled indicator
pub const SLOT_SUN_CLOUD:  u8 = 4;
pub const SLOT_DEGREE:     u8 = 5;
pub const SLOT_HOURGLASS:  u8 = 6;
pub const SLOT_ASSET:      u8 = 7;

pub const BLANK: [u8; 8] = [0u8; 8];

// ── Dynamic allocator (used by sand) ─────────────────────────────────────────

/// Tracks which of the 8 CGRAM slots are in use.
/// Slots are allocated sequentially (0 first), so the returned index doubles
/// as the position in `SandGrid::slot_to_char`.
pub struct CgramPool {
    free_mask: u8, // bit N = 1 → slot N is free
}

impl CgramPool {
    pub const fn new() -> Self {
        Self { free_mask: 0xFF }
    }

    /// Claim the next free slot. Returns its index (0–7), or `None` if all taken.
    pub fn alloc(&mut self) -> Option<u8> {
        if self.free_mask == 0 {
            return None;
        }
        let slot = self.free_mask.trailing_zeros() as u8;
        self.free_mask &= !(1 << slot);
        Some(slot)
    }

    /// True when all 8 slots have been claimed.
    pub fn is_full(&self) -> bool {
        self.free_mask == 0
    }

    /// True when this specific slot is currently allocated.
    pub fn is_used(&self, slot: u8) -> bool {
        (self.free_mask >> slot) & 1 == 0
    }

    /// Release a single slot back to the pool.
    pub fn free(&mut self, slot: u8) {
        self.free_mask |= 1 << slot;
    }

    /// Number of slots currently allocated.
    pub fn count_used(&self) -> usize {
        8 - self.free_mask.count_ones() as usize
    }

    /// Write blank glyphs to every allocated slot and mark them all free.
    /// Call this when handing CGRAM back to the screen renderer.
    pub fn release(&mut self, lcd: &mut Lcd) {
        let used = !self.free_mask;
        for i in 0u8..8 {
            if used & (1 << i) != 0 {
                lcd.create_char(i, &BLANK);
            }
        }
        self.free_mask = 0xFF;
    }
}
