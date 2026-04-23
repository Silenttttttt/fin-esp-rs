/// Particle simulation (sand or water) for loading screens.
///
/// Canvas: 5 chars wide × 4 chars tall (25 px × 32 px), centered on the LCD.
/// Particles are emitted from the center top (pixel 12, 0).
///
/// Sand: falls and piles up in heaps.
/// Water: falls, then spreads horizontally — fills flat and flows around obstacles.
///
/// CGRAM slots are claimed dynamically via `CgramPool` as particles spread into
/// new char cells. After all 8 slots are claimed, unclaimed cells become walls.
///
/// When sand is done, call `release(lcd)` — this writes blank glyphs to every
/// claimed slot, cleanly handing CGRAM back to the screen renderer.

use crate::cgram::CgramPool;
use crate::lcd::Lcd;

const COLS: usize = 5;
const ROWS: usize = 4;
const W: usize = COLS * 5; // 25 px wide
const H: usize = ROWS * 8; // 32 px tall
const SOURCE_X: usize = W / 2;  // = 12
const SOURCE_CX: usize = SOURCE_X / 5; // = 2
const SOURCE_CY: usize = 0;

/// Ticks the source pixel must stay blocked before the grid resets.
const BLOCKED_RESET_TICKS: u8 = 250;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Particle { Sand, Water }

pub fn rand_particle() -> Particle {
    if unsafe { esp_idf_sys::esp_random() } & 1 == 0 { Particle::Sand } else { Particle::Water }
}

pub struct SandGrid {
    cells: [[bool; H]; W],
    tick: u8,
    prev_glyphs: [[u8; 8]; 8],
    pub particle: Particle,
    pool: CgramPool,
    slot_to_char: [(usize, usize); 8], // slot index → (cx, cy)
    char_has_slot: [[bool; ROWS]; COLS],
    blocked_ticks: u8,
}

impl SandGrid {
    pub fn new(particle: Particle) -> Self {
        Self {
            cells: [[false; H]; W],
            tick: 0,
            prev_glyphs: [[0u8; 8]; 8],
            particle,
            pool: CgramPool::new(),
            slot_to_char: [(0, 0); 8],
            char_has_slot: [[false; ROWS]; COLS],
            blocked_ticks: 0,
        }
    }

    /// Write blank glyphs to every claimed CGRAM slot and reset the grid.
    /// Call this when handing CGRAM back to the screen renderer.
    pub fn release(&mut self, lcd: &mut Lcd) {
        self.pool.release(lcd);
        *self = Self::new(rand_particle());
    }

    pub fn step(&mut self) {
        self.tick = self.tick.wrapping_add(1);

        if !self.cells[SOURCE_X][0] {
            self.cells[SOURCE_X][0] = true;
            self.blocked_ticks = 0;
        } else {
            self.blocked_ticks = self.blocked_ticks.saturating_add(1);
            if self.blocked_ticks >= BLOCKED_RESET_TICKS {
                // Internal reset: no LCD access here, so just drop the old pool.
                // New sand will immediately overwrite those CGRAM slots with fresh glyphs.
                let new_particle = rand_particle();
                *self = Self::new(new_particle);
            }
        }

        for y in (0..H - 1).rev() {
            if self.tick & 1 == 0 {
                for x in 0..W { self.try_fall(x, y); }
            } else {
                for x in (0..W).rev() { self.try_fall(x, y); }
            }
        }

        // Unclaim slots whose cells are now completely empty so they can be reused.
        for slot in 0..8usize {
            if !self.pool.is_used(slot as u8) { continue; }
            let (cx, cy) = self.slot_to_char[slot];
            let px0 = cx * 5;
            let py0 = cy * 8;
            let occupied = (0..8).any(|r| (0..5).any(|c| self.cells[px0 + c][py0 + r]));
            if !occupied {
                self.pool.free(slot as u8);
                self.char_has_slot[cx][cy] = false;
            }
        }

        'alloc: for cy in 0..ROWS {
            for cx in 0..COLS {
                if cx == SOURCE_CX && cy == SOURCE_CY { continue; }
                if self.char_has_slot[cx][cy] { continue; }
                if self.pool.is_full() { break 'alloc; }
                let px0 = cx * 5;
                let py0 = cy * 8;
                let mut occupied = false;
                'scan: for row in 0..8 {
                    for col in 0..5 {
                        if self.cells[px0 + col][py0 + row] {
                            occupied = true;
                            break 'scan;
                        }
                    }
                }
                if occupied {
                    let slot = self.pool.alloc().unwrap();
                    self.slot_to_char[slot as usize] = (cx, cy);
                    self.char_has_slot[cx][cy] = true;
                }
            }
        }
    }

    fn can_enter(&self, x: usize, y: usize) -> bool {
        if x >= W || y >= H { return false; }
        if self.pool.is_full() {
            let cx = x / 5;
            let cy = y / 8;
            if !(cx == SOURCE_CX && cy == SOURCE_CY) && !self.char_has_slot[cx][cy] {
                return false;
            }
        }
        !self.cells[x][y]
    }

    fn try_fall(&mut self, x: usize, y: usize) {
        if !self.cells[x][y] { return; }
        let by = y + 1;

        // 1. Fall (with slight mid-air drift)
        if self.can_enter(x, by) {
            // ~1/16 chance: nudge one pixel left or right while falling.
            let rnd = unsafe { esp_idf_sys::esp_random() };
            if rnd & 0x0F == 0 {
                let nx = if rnd & 0x10 == 0 { x.wrapping_sub(1) } else { x + 1 };
                if nx < W && self.can_enter(nx, by) {
                    self.cells[nx][by] = true;
                    self.cells[x][y] = false;
                    return;
                }
            }
            self.cells[x][by] = true;
            self.cells[x][y] = false;
            return;
        }

        // 2. Diagonal slide
        let xl = x.wrapping_sub(1);
        let xr = x + 1;
        let can_l = xl < W && self.can_enter(xl, by);
        let can_r = xr < W && self.can_enter(xr, by);

        let dest = match (can_l, can_r) {
            (true, false) => Some(xl),
            (false, true) => Some(xr),
            (true, true) => Some(if unsafe { esp_idf_sys::esp_random() } & 1 == 0 { xl } else { xr }),
            (false, false) => None,
        };

        if let Some(dx) = dest {
            self.cells[dx][by] = true;
            self.cells[x][y] = false;
            return;
        }

        // 3. Water only: spread horizontally when unable to fall.
        // Probe both directions independently; move to the farthest open cell.
        // On equal reach, use hardware RNG for a true 50/50 tiebreak.
        if self.particle == Particle::Water {
            let mut best_l: Option<usize> = None;
            let mut best_r: Option<usize> = None;
            for step in 1..=3usize {
                let nx = x.wrapping_sub(step);
                if nx >= W || !self.can_enter(nx, y) { break; }
                best_l = Some(nx);
            }
            for step in 1..=3usize {
                let nx = x + step;
                if nx >= W || !self.can_enter(nx, y) { break; }
                best_r = Some(nx);
            }
            let nx = match (best_l, best_r) {
                (Some(l), Some(r)) => {
                    let ld = x - l;
                    let rd = r - x;
                    if ld > rd { Some(l) }
                    else if rd > ld { Some(r) }
                    else if unsafe { esp_idf_sys::esp_random() } & 1 == 0 { Some(l) } else { Some(r) }
                }
                (Some(l), None) => Some(l),
                (None, Some(r)) => Some(r),
                (None, None)    => None,
            };
            if let Some(nx) = nx {
                self.cells[nx][y] = true;
                self.cells[x][y] = false;
                return;
            }
        }
    }

    pub fn glyphs(&self) -> [[u8; 8]; 8] {
        let mut out = [[0u8; 8]; 8];
        for slot in 0..8usize {
            if !self.pool.is_used(slot as u8) { continue; }
            let (cx, cy) = self.slot_to_char[slot];
            let px0 = cx * 5;
            let py0 = cy * 8;
            let mut g = [0u8; 8];
            for row in 0..8usize {
                let mut byte = 0u8;
                for col in 0..5usize {
                    if self.cells[px0 + col][py0 + row] {
                        byte |= 1 << (4 - col);
                    }
                }
                g[row] = byte;
            }
            out[slot] = g;
        }
        out
    }

    pub fn diff_and_update(&mut self, new_glyphs: &[[u8; 8]; 8]) -> [bool; 8] {
        let mut dirty = [false; 8];
        for i in 0..8 {
            if new_glyphs[i] != self.prev_glyphs[i] {
                dirty[i] = true;
                self.prev_glyphs[i] = new_glyphs[i];
            }
        }
        dirty
    }

    pub fn display_chars(&self) -> [[u8; COLS]; ROWS] {
        let mut out = [[b' '; COLS]; ROWS];
        for slot in 0..8usize {
            if !self.pool.is_used(slot as u8) { continue; }
            let (cx, cy) = self.slot_to_char[slot];
            out[cy][cx] = slot as u8;
        }
        out
    }

    pub fn render(&mut self, lcd: &mut Lcd, sand_col: u8) {
        let g = self.glyphs();
        let dirty = self.diff_and_update(&g);
        let dc = self.display_chars();

        for s in 0..8usize {
            if dirty[s] { lcd.create_char(s as u8, &g[s]); }
        }
        for row in 0..ROWS {
            lcd.set_cursor(sand_col, row as u8);
            lcd.write_raw(&dc[row]);
        }
    }
}
