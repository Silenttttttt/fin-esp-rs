use crate::api;
use crate::cgram::{
    SLOT_ASSET, SLOT_DEGREE, SLOT_DOWN_ARROW, SLOT_HOURGLASS,
    SLOT_LAMP, SLOT_SUN_CLOUD, SLOT_UP_ARROW, SLOT_WIFI,
};
use crate::config::{self, Screen};
use crate::fmt;
use crate::glyphs;
use crate::lcd::Lcd;
use crate::screen::{RowCache, UiState};

/// Render all four ticker rows to the LCD (CGRAM before DDRAM).
pub fn render(lcd: &mut Lcd, cache: &mut RowCache, state: &UiState, now_ms: u64) {
    let r0 = build_header(state, now_ms);
    let r1 = build_asset_row(state);
    let r2 = build_price_row(state);
    let r3 = build_change_row(state);

    let mut used = [false; 8];
    for row in [&r0, &r1, &r2, &r3] {
        for &b in row.iter() {
            if (b as usize) < 8 { used[b as usize] = true; }
        }
    }
    let g = all_glyphs(state, now_ms);
    for slot in 0u8..8 {
        if used[slot as usize] {
            cache.write_glyph(lcd, slot, &g[slot as usize]);
        }
    }

    cache.commit(lcd, 0, &r0);
    cache.commit(lcd, 1, &r1);
    cache.commit(lcd, 2, &r2);
    cache.commit(lcd, 3, &r3);
}

/// Update only the header row (clock tick or lamp animation).
pub fn paint_header(lcd: &mut Lcd, cache: &mut RowCache, state: &UiState, now_ms: u64) {
    let r0 = build_header(state, now_ms);

    let mut used = [false; 8];
    for &b in &r0 {
        if (b as usize) < 8 { used[b as usize] = true; }
    }
    let g = all_glyphs(state, now_ms);
    for slot in 0u8..8 {
        if used[slot as usize] {
            cache.write_glyph(lcd, slot, &g[slot as usize]);
        }
    }

    cache.commit(lcd, 0, &r0);
}

/// Pre-load all CGRAM slots with correct ticker glyphs before the first render.
/// Call once after the sand animation blanks CGRAM, to prevent a blank-icon flash.
pub fn prime_cgram(lcd: &mut Lcd, cache: &mut RowCache, state: &UiState, now_ms: u64) {
    let g = all_glyphs(state, now_ms);
    for slot in 0u8..8 {
        cache.write_glyph(lcd, slot, &g[slot as usize]);
    }
}

// ── Glyph computation ─────────────────────────────────────────────────────────

fn all_glyphs(state: &UiState, now_ms: u64) -> [[u8; 8]; 8] {
    let mut g = [[0u8; 8]; 8];
    let lamp_anim = state.lamp_anim_until > 0 && now_ms < state.lamp_anim_until;

    g[SLOT_LAMP as usize] = if lamp_anim {
        glyphs::HOURGLASS[(state.lamp_loading_frame as usize) % glyphs::HOURGLASS.len()]
    } else if !state.lamp.known {
        glyphs::GLYPH_LAMP_UNK
    } else if state.lamp.on {
        glyphs::GLYPH_LAMP_ON
    } else {
        glyphs::GLYPH_LAMP_OFF
    };

    g[SLOT_WIFI as usize] = if state.wifi_connected {
        glyphs::GLYPH_WIFI_ON
    } else {
        glyphs::GLYPH_WIFI_OFF
    };

    g[SLOT_UP_ARROW   as usize] = glyphs::GLYPH_UP_ARROW;
    g[SLOT_DOWN_ARROW as usize] = glyphs::GLYPH_DOWN_ARROW;

    g[SLOT_SUN_CLOUD as usize] = if state.data.weather_code
        .map(api::wmo_is_rain)
        .unwrap_or(false)
    {
        glyphs::GLYPH_RAIN
    } else {
        glyphs::GLYPH_SUN_CLOUD
    };

    g[SLOT_DEGREE as usize] = glyphs::GLYPH_DEGREE;

    g[SLOT_HOURGLASS as usize] = if state.fetching {
        glyphs::HOURGLASS[(state.loading_frame as usize) % glyphs::HOURGLASS.len()]
    } else {
        let interval_secs = (config::FETCH_INTERVAL_MS / 1000) as u8;
        let elapsed = if state.fetch_completed_at > 0 && now_ms >= state.fetch_completed_at {
            ((now_ms - state.fetch_completed_at) / 1000).min(interval_secs as u64) as u8
        } else {
            0
        };
        let dots = 30u8.saturating_sub(elapsed * 30 / interval_secs);
        glyphs::countdown_glyph(dots)
    };

    g[SLOT_ASSET as usize] = *glyphs::asset_glyph(state.screen);

    g
}

// ── Row builders ──────────────────────────────────────────────────────────────

fn build_header(state: &UiState, now_ms: u64) -> [u8; 20] {
    let _ = now_ms;
    let mut row = [b' '; 20];

    row[0] = SLOT_WIFI;
    row[3] = if state.fetching || state.fetch_completed_at > 0 {
        SLOT_HOURGLASS
    } else {
        b'!'
    };
    row[4] = SLOT_LAMP;

    let date_str = get_date_string();
    for (i, b) in date_str.bytes().enumerate() {
        row[6 + i] = b;
    }

    let time_str = get_time_string();
    let time_bytes = time_str.as_bytes();
    let start = 20 - time_bytes.len().min(8);
    for (i, &b) in time_bytes.iter().take(8).enumerate() {
        row[start + i] = b;
    }

    row
}

fn build_asset_row(state: &UiState) -> [u8; 20] {
    let mut row = [b' '; 20];
    row[0] = if state.screen == Screen::UsdBrl { b'$' } else { SLOT_ASSET };
    for (i, b) in state.screen.name().bytes().enumerate() {
        if i + 1 >= 20 { break; }
        row[i + 1] = b;
    }
    row
}

fn build_price_row(state: &UiState) -> [u8; 20] {
    let price = match state.screen {
        Screen::Btc    => state.data.price_btc,
        Screen::Sol    => state.data.price_sol,
        Screen::Gold   => state.data.price_gold,
        Screen::Oil    => state.data.price_oil,
        Screen::UsdBrl => state.data.price_usd_brl,
    };
    str_to_row(&fmt::format_price_row(price, state.screen.decimals()))
}

fn build_change_row(state: &UiState) -> [u8; 20] {
    let (chg, has_chg) = match state.screen {
        Screen::Btc    => (state.data.chg_btc_pct,     state.data.ok_crypto),
        Screen::Sol    => (state.data.chg_sol_pct,     state.data.ok_crypto),
        Screen::Gold   => (state.data.chg_gold_pct,    state.data.has_chg_gold),
        Screen::Oil    => (state.data.chg_oil_pct,     state.data.has_chg_oil),
        Screen::UsdBrl => (state.data.chg_usd_brl_pct, state.data.ok_usd_brl),
    };

    let mut row = [b' '; 20];
    let mut col = 0usize;

    if has_chg {
        row[col] = if chg >= 0.0 { SLOT_UP_ARROW } else { SLOT_DOWN_ARROW };
        col += 1;
        for b in format!("{:.1}%", chg).bytes() {
            if col >= 8 { break; }
            row[col] = b; col += 1;
        }
    } else {
        for b in b"--%".iter() {
            if col >= 8 { break; }
            row[col] = *b; col += 1;
        }
    }

    if state.data.ok_weather {
        if let (Some(temp), Some(code)) = (state.data.weather_temp, state.data.weather_code) {
            let temp_s = format!("{:.0}", temp);
            let label  = api::wmo_label(code);
            let wlen   = 5 + temp_s.len() + label.len();
            let wstart = if wlen <= 20 { 20 - wlen } else { 9 };
            let mut wc = wstart;
            row[wc] = SLOT_SUN_CLOUD; wc += 1;
            row[wc] = b' '; wc += 1;
            for b in temp_s.bytes() { if wc < 20 { row[wc] = b; wc += 1; } }
            row[wc] = SLOT_DEGREE; wc += 1;
            row[wc] = b'C'; wc += 1;
            row[wc] = b' '; wc += 1;
            for b in label.bytes() { if wc < 20 { row[wc] = b; wc += 1; } }
        }
    }

    row
}

fn str_to_row(s: &str) -> [u8; 20] {
    let mut row = [b' '; 20];
    for (i, b) in s.bytes().enumerate() {
        if i >= 20 { break; }
        row[i] = b;
    }
    row
}

fn get_date_string() -> String {
    use std::time::SystemTime;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let adjusted = now + config::GMT_OFFSET_SEC as i64;
    let days = (adjusted / 86400) as u64;
    let z   = days + 719468;
    let era = z / 146097;
    let doe = z % 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp  = (5 * doy + 2) / 153;
    let d   = doy - (153 * mp + 2) / 5 + 1;
    let m   = if mp < 10 { mp + 3 } else { mp - 9 };
    format!("{:02}/{:02}", d, m)
}

fn get_time_string() -> String {
    use std::time::SystemTime;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let adjusted = now + config::GMT_OFFSET_SEC as i64;
    let secs = ((adjusted % 86400) + 86400) % 86400;
    let h24 = secs / 3600;
    let m   = (secs % 3600) / 60;
    let s   = secs % 60;
    let h12 = match h24 % 12 { 0 => 12, h => h };
    format!("{:2}:{:02}:{:02}{}", h12, m, s, if h24 < 12 { "a" } else { "p" })
}
