use std::fmt::Write;

/// Insert thousand-separator commas into a digit string.
/// "12345678" → "12,345,678"
pub fn insert_commas(digits: &str) -> String {
    let len = digits.len();
    if len <= 3 {
        return digits.to_string();
    }
    let first = len % 3;
    let first = if first == 0 { 3 } else { first };
    let mut out = String::with_capacity(len + len / 3);
    out.push_str(&digits[..first]);
    let mut pos = first;
    while pos < len {
        out.push(',');
        out.push_str(&digits[pos..pos + 3]);
        pos += 3;
    }
    out
}

/// Format a float price with the given number of decimal places and thousand separators.
pub fn format_price(value: f64, decimals: u8) -> String {
    if value <= 0.0 {
        return "--".to_string();
    }
    let mult = 10f64.powi(decimals as i32);
    let iv = (value * mult).round() as i64;
    let whole = iv / mult as i64;
    let frac = (iv % mult as i64).unsigned_abs();

    let whole_str = insert_commas(&whole.to_string());
    if decimals == 0 {
        whole_str
    } else {
        format!("{}.{:0>width$}", whole_str, frac, width = decimals as usize)
    }
}

/// Format price row for LCD: "Price: $ XXX,XXX.DD" padded to 20 chars.
pub fn format_price_row(price: f64, decimals: u8) -> String {
    let formatted = format_price(price, decimals);
    let row = format!("Price: $ {}", formatted);
    pad_to_20(&row)
}

/// Format change percentage row for LCD: "^+2.34%  26C clear" padded to 20.
pub fn format_change_row(
    change_pct: f64,
    has_change: bool,
    temp: Option<f64>,
    weather_label: &str,
) -> String {
    let mut row = String::with_capacity(20);

    if has_change {
        let arrow = if change_pct >= 0.0 { '^' } else { 'v' };
        let sign = if change_pct >= 0.0 { '+' } else { '-' };
        let _ = write!(row, "{}{}{:.2}%", arrow, sign, change_pct.abs());
    }

    if let Some(t) = temp {
        // Pad to push weather to the right
        while row.len() < 10 {
            row.push(' ');
        }
        let _ = write!(row, "{:.0}\x05C {}", t, weather_label); // \x05 = degree glyph
    }

    pad_to_20(&row)
}

/// Pad or truncate a string to exactly 20 characters.
pub fn pad_to_20(s: &str) -> String {
    let mut out = String::with_capacity(20);
    for (i, ch) in s.chars().enumerate() {
        if i >= 20 {
            break;
        }
        out.push(ch);
    }
    while out.len() < 20 {
        out.push(' ');
    }
    out
}
