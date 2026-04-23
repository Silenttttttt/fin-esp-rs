use crate::config::Screen;
use crate::glyphs::Glyph;
use crate::lcd::Lcd;
use crate::screen::{RowCache, UiState};

const DATA_MAX:   usize = 8;             // max data points = max CGRAM slots
const LABEL_COLS: usize = 5;             // cols 0–4: min/max price labels
const CHART_COLS: usize = 20 - LABEL_COLS; // 15 cols available for chart data

pub fn render(lcd: &mut Lcd, cache: &mut RowCache, state: &UiState, prices: &[f64]) {
    let n = prices.len().min(DATA_MAX);
    let prices = &prices[prices.len().saturating_sub(n)..]; // last n samples
    let (r0, r1, r2, r3, cgram) = build_chart(state, prices);
    for (slot, g) in cgram.iter().enumerate() {
        cache.write_glyph(lcd, slot as u8, g);
    }
    cache.commit(lcd, 0, &r0);
    cache.commit(lcd, 1, &r1);
    cache.commit(lcd, 2, &r2);
    cache.commit(lcd, 3, &r3);
}

fn build_chart(
    state: &UiState,
    prices: &[f64],
) -> ([u8; 20], [u8; 20], [u8; 20], [u8; 20], [Glyph; 8]) {
    let mut r0 = [b' '; 20];
    let mut r1 = [b' '; 20];
    let mut r2 = [b' '; 20];
    let mut r3 = [b' '; 20];
    let mut cgram: [Glyph; 8] = [[0u8; 8]; 8];

    let n = prices.len();

    // ── Row 0: header — asset | % change (centred) | current price ─────────
    let name: &[u8] = match state.screen {
        Screen::Btc    => b"BTC",
        Screen::Sol    => b"SOL",
        Screen::Gold   => b"GOLD",
        Screen::Oil    => b"OIL",
        Screen::UsdBrl => b"BRL",
    };
    for (i, &b) in name.iter().enumerate() { r0[i] = b; }

    if n >= 2 && prices[0] > 1e-9 {
        let pct = (prices[n - 1] - prices[0]) / prices[0] * 100.0;
        let s = format!("{:+.1}%", pct);
        let bytes = s.as_bytes();
        let start = 10usize.saturating_sub(bytes.len() / 2);
        for (i, &b) in bytes.iter().enumerate() {
            if start + i < 15 { r0[start + i] = b; }
        }
    }

    let cur = current_price(state);
    if cur > 0.0 {
        let p = short_price(cur, state.screen);
        for (i, &b) in p.iter().enumerate() { r0[15 + i] = b; }
    }

    // ── Not enough data ───────────────────────────────────────────────────────
    if n < 2 {
        r2 = *b"  not enough data   ";
        return (r0, r1, r2, r3, cgram);
    }

    // ── Price range ───────────────────────────────────────────────────────────
    let min = prices.iter().cloned().fold(f64::MAX, f64::min);
    let max = prices.iter().cloned().fold(f64::MIN, f64::max);
    let range = max - min;

    // Min/max labels at left of top and bottom chart rows
    if range > 1e-9 {
        let hi = short_price(max, state.screen);
        let lo = short_price(min, state.screen);
        for (i, &b) in hi.iter().enumerate() { r1[i] = b; }
        for (i, &b) in lo.iter().enumerate() { r3[i] = b; }
    }

    // ── Scale to pixel heights 0–23 ───────────────────────────────────────────
    let mut heights = [0i32; DATA_MAX];
    for i in 0..n {
        heights[i] = if range < 1e-9 {
            11
        } else {
            ((prices[i] - min) / range * 23.0).round().clamp(0.0, 23.0) as i32
        };
    }

    // ── Draw connected line segments ──────────────────────────────────────────
    // Centre n columns in the 15-column chart area (cols LABEL_COLS..19)
    let col_start = LABEL_COLS + (CHART_COLS.saturating_sub(n)) / 2;
    let mut reg_count = 0usize;
    let empty: Glyph = [0u8; 8];

    for i in 0..n {
        let h = heights[i];
        let lh = if i > 0     { (heights[i - 1] + h) / 2 } else { h };
        let rh = if i < n - 1 { (h + heights[i + 1]) / 2 } else { h };
        let col = col_start + i;
        if col >= 20 { break; }

        // cell_base 16 → LCD row 1 (top), 8 → row 2, 0 → row 3 (bottom)
        let g1 = line_glyph(lh, rh, 16);
        let g2 = line_glyph(lh, rh, 8);
        let g3 = line_glyph(lh, rh, 0);

        if g1 != empty { r1[col] = slot_for(&mut cgram, &mut reg_count, g1); }
        if g2 != empty { r2[col] = slot_for(&mut cgram, &mut reg_count, g2); }
        if g3 != empty { r3[col] = slot_for(&mut cgram, &mut reg_count, g3); }
    }

    (r0, r1, r2, r3, cgram)
}

fn current_price(state: &UiState) -> f64 {
    match state.screen {
        Screen::Btc    => state.data.price_btc,
        Screen::Sol    => state.data.price_sol,
        Screen::Gold   => state.data.price_gold,
        Screen::Oil    => state.data.price_oil,
        Screen::UsdBrl => state.data.price_usd_brl,
    }
}

/// Format a price into exactly 5 bytes, right-aligned, space-padded.
fn short_price(price: f64, screen: Screen) -> [u8; 5] {
    let s = match screen {
        Screen::Btc => {
            if price >= 100_000.0 { format!("{:.0}k", price / 1000.0) }
            else                  { format!("{:.1}k", price / 1000.0) }
        }
        Screen::Sol    => format!("{:.1}", price),
        Screen::Gold   => format!("{:.0}", price),
        Screen::Oil    => format!("{:.2}", price),
        Screen::UsdBrl => format!("{:.3}", price),
    };
    let mut buf = [b' '; 5];
    let bytes = s.as_bytes();
    if bytes.len() <= 5 {
        let start = 5 - bytes.len();
        for (i, &b) in bytes.iter().enumerate() { buf[start + i] = b; }
    } else {
        // String too long: take rightmost 5 chars
        for (i, &b) in bytes[bytes.len() - 5..].iter().enumerate() { buf[i] = b; }
    }
    buf
}

/// Compute a 5×8 glyph for the diagonal line segment through one character cell.
///
/// X-pass: one pixel per character column — covers shallow spans.
/// Y-pass: one pixel per pixel row — fills gaps so steep segments stay connected.
/// Both passes combined guarantee no gaps regardless of slope.
fn line_glyph(left_h: i32, right_h: i32, cell_base: i32) -> Glyph {
    let mut g = [0u8; 8];
    let cell_top = cell_base + 7;
    if left_h.max(right_h) < cell_base || left_h.min(right_h) > cell_top {
        return g;
    }
    let delta = right_h - left_h;

    // X-pass
    for x in 0..5i32 {
        let h = left_h + delta * x / 4;
        if h < cell_base || h > cell_top { continue; }
        let row = (7 - (h - cell_base)) as usize;
        g[row] |= 1u8 << (4 - x as usize);
    }

    // Y-pass (steep connectivity)
    if delta != 0 {
        let y_lo = left_h.min(right_h).max(cell_base);
        let y_hi = left_h.max(right_h).min(cell_top);
        for h in y_lo..=y_hi {
            let x = ((h - left_h) * 4 / delta).clamp(0, 4);
            let row = (7 - (h - cell_base)) as usize;
            g[row] |= 1u8 << (4 - x as usize);
        }
    }
    g
}

/// Find or assign a CGRAM slot for this glyph.
/// Exact match → reuse slot.  Free slot → assign it.
/// CGRAM full → Hamming-closest if ≤10 bits differ, else ASCII '|'.
fn slot_for(reg: &mut [Glyph; 8], count: &mut usize, g: Glyph) -> u8 {
    for s in 0..*count {
        if reg[s] == g { return s as u8; }
    }
    if *count < 8 {
        reg[*count] = g;
        let s = *count as u8;
        *count += 1;
        return s;
    }
    let mut best = 0u8;
    let mut best_dist = u32::MAX;
    for s in 0..8usize {
        let dist: u32 = reg[s].iter().zip(g.iter()).map(|(&a, &b)| (a ^ b).count_ones()).sum();
        if dist < best_dist { best_dist = dist; best = s as u8; }
    }
    if best_dist > 10 { b'|' } else { best }
}
