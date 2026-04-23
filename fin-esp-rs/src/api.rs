use esp_idf_svc::http::client::{Configuration as HttpConfig, EspHttpConnection};
use esp_idf_svc::http::Method;
use log::{info, warn};
use serde_json::Value;
use std::time::Duration;

use crate::config::{self, Screen};

/// Market data collected from all API sources.
#[derive(Default, Clone, Debug)]
pub struct MarketData {
    pub price_btc: f64,
    pub chg_btc_pct: f64,
    pub price_sol: f64,
    pub chg_sol_pct: f64,
    pub ok_crypto: bool,

    pub price_usd_brl: f64,
    pub chg_usd_brl_pct: f64,
    pub ok_usd_brl: bool,

    pub price_gold: f64,
    pub chg_gold_pct: f64,
    pub has_chg_gold: bool,
    pub ok_gold: bool,

    pub price_oil: f64,
    pub chg_oil_pct: f64,
    pub has_chg_oil: bool,
    pub ok_oil: bool,

    pub weather_temp: Option<f64>,
    pub weather_code: Option<i32>,
    pub ok_weather: bool,
}

impl MarketData {
    pub fn all_markets_ok(&self) -> bool {
        self.ok_crypto && self.ok_usd_brl && self.ok_gold && self.ok_oil
    }
}

fn https_get(url: &str) -> Result<String, String> {
    let config = HttpConfig {
        timeout: Some(Duration::from_millis(config::HTTP_TIMEOUT_MS)),
        use_global_ca_store: true,
        crt_bundle_attach: Some(esp_idf_svc::sys::esp_crt_bundle_attach),
        ..Default::default()
    };
    let mut conn =
        EspHttpConnection::new(&config).map_err(|e| format!("HTTP conn: {e}"))?;

    for attempt in 0..config::HTTP_RETRIES {
        conn.initiate_request(Method::Get, url, &[])
            .map_err(|e| format!("HTTP init: {e}"))?;
        conn.initiate_response()
            .map_err(|e| format!("HTTP resp: {e}"))?;

        let status = conn.status();
        if status == 200 {
            let mut body = Vec::with_capacity(2048);
            let mut buf = [0u8; 512];
            loop {
                let n = conn.read(&mut buf).map_err(|e| format!("HTTP read: {e}"))?;
                if n == 0 {
                    break;
                }
                body.extend_from_slice(&buf[..n]);
            }
            return String::from_utf8(body).map_err(|e| format!("UTF-8: {e}"));
        }

        if status == 429 || status == 503 {
            warn!("[API] HTTP {status}, backing off 20s");
            std::thread::sleep(Duration::from_millis(20_000));
        } else if attempt < config::HTTP_RETRIES - 1 {
            let delay = config::HTTP_RETRY_DELAY_MS * (attempt as u64 + 1);
            warn!("[API] HTTP {status}, retry in {delay}ms");
            std::thread::sleep(Duration::from_millis(delay));
        }
    }
    Err(format!("HTTP failed after {0} retries", config::HTTP_RETRIES))
}

pub fn fetch_crypto(data: &mut MarketData) {
    info!("[API] fetching crypto");
    match https_get(config::URL_COINGECKO) {
        Ok(body) => {
            if let Ok(v) = serde_json::from_str::<Value>(&body) {
                data.price_btc = v["bitcoin"]["usd"].as_f64().unwrap_or(0.0);
                data.chg_btc_pct = v["bitcoin"]["usd_24h_change"].as_f64().unwrap_or(0.0);
                data.price_sol = v["solana"]["usd"].as_f64().unwrap_or(0.0);
                data.chg_sol_pct = v["solana"]["usd_24h_change"].as_f64().unwrap_or(0.0);
                data.ok_crypto = data.price_btc > 0.0 || data.price_sol > 0.0;
            } else {
                warn!("[API] crypto JSON parse failed");
            }
        }
        Err(e) => warn!("[API] crypto fetch failed: {e}"),
    }
}

pub fn fetch_usd_brl(data: &mut MarketData) {
    info!("[API] fetching USD/BRL");
    match https_get(config::URL_USDBRL) {
        Ok(body) => {
            if let Ok(v) = serde_json::from_str::<Value>(&body) {
                let bid = v["USDBRL"]["bid"]
                    .as_str()
                    .and_then(|s| s.parse::<f64>().ok())
                    .unwrap_or(0.0);
                let pct = v["USDBRL"]["pctChange"]
                    .as_str()
                    .and_then(|s| s.parse::<f64>().ok())
                    .unwrap_or(0.0);
                data.price_usd_brl = bid;
                data.chg_usd_brl_pct = pct;
                data.ok_usd_brl = bid > 0.0;
            } else {
                warn!("[API] USD/BRL JSON parse failed");
            }
        }
        Err(e) => warn!("[API] USD/BRL fetch failed: {e}"),
    }
}

fn fetch_stooq(url: &str) -> Result<(f64, f64, bool), String> {
    let body = https_get(url)?;
    // Single data line, no header: Symbol,Date,Time,Open,High,Low,Close,,
    let row = body.lines().next().ok_or("empty response")?;
    let cols: Vec<&str> = row.split(',').collect();
    if cols.len() < 7 {
        return Err("not enough columns".into());
    }
    let open: f64  = cols[3].parse().map_err(|_| "bad open")?;
    let close: f64 = cols[6].parse().map_err(|_| "bad close")?;
    if close <= 0.0 {
        return Err("close <= 0".into());
    }
    let chg = (close - open) / open * 100.0;
    Ok((close, chg, true))
}

pub fn fetch_gold(data: &mut MarketData) {
    info!("[API] fetching gold");
    match fetch_stooq(config::URL_STOOQ_GOLD) {
        Ok((price, chg, has)) => {
            data.price_gold = price;
            data.chg_gold_pct = chg;
            data.has_chg_gold = has;
            data.ok_gold = true;
        }
        Err(e) => warn!("[API] gold fetch failed: {e}"),
    }
}

pub fn fetch_oil(data: &mut MarketData) {
    info!("[API] fetching oil");
    match fetch_stooq(config::URL_STOOQ_OIL) {
        Ok((price, chg, has)) => {
            data.price_oil = price;
            data.chg_oil_pct = chg;
            data.has_chg_oil = has;
            data.ok_oil = true;
        }
        Err(e) => warn!("[API] oil fetch failed: {e}"),
    }
}

pub fn fetch_weather(data: &mut MarketData) {
    info!("[API] fetching weather");
    let url = config::url_weather();
    match https_get(&url) {
        Ok(body) => {
            if let Ok(v) = serde_json::from_str::<Value>(&body) {
                data.weather_temp = v["current"]["temperature_2m"].as_f64();
                data.weather_code = v["current"]["weather_code"].as_i64().map(|c| c as i32);
                data.ok_weather = data.weather_temp.is_some();
            } else {
                warn!("[API] weather JSON parse failed");
            }
        }
        Err(e) => warn!("[API] weather fetch failed: {e}"),
    }
}

/// Merge a partial result — only overwrites fields that succeeded this cycle,
/// preserving last-known-good values for anything that failed.
pub fn merge(data: &mut MarketData, r: MarketData) {
    if r.ok_crypto {
        data.price_btc   = r.price_btc;
        data.chg_btc_pct = r.chg_btc_pct;
        data.price_sol   = r.price_sol;
        data.chg_sol_pct = r.chg_sol_pct;
        data.ok_crypto   = true;
    }
    if r.ok_usd_brl {
        data.price_usd_brl   = r.price_usd_brl;
        data.chg_usd_brl_pct = r.chg_usd_brl_pct;
        data.ok_usd_brl      = true;
    }
    if r.ok_gold {
        data.price_gold   = r.price_gold;
        data.chg_gold_pct = r.chg_gold_pct;
        data.has_chg_gold = r.has_chg_gold;
        data.ok_gold      = true;
    }
    if r.ok_oil {
        data.price_oil   = r.price_oil;
        data.chg_oil_pct = r.chg_oil_pct;
        data.has_chg_oil = r.has_chg_oil;
        data.ok_oil      = true;
    }
    if r.ok_weather {
        data.weather_temp = r.weather_temp;
        data.weather_code = r.weather_code;
        data.ok_weather   = true;
    }
}

/// Fetch all market data using 2 workers, prioritising the current and next screen.
///
/// Worker A owns: crypto (BTC+SOL) and USD/BRL.
/// Worker B owns: gold, oil, and weather.
/// Within each worker the fetch that serves the current or next visible screen
/// runs first, so the user always sees fresh data for what they're looking at.
pub fn fetch_all(data: &mut MarketData, current: Screen, next: Screen) {
    use std::thread;

    // True if a screen is "priority" — current or one rotation ahead.
    let pri = |s: Screen| s == current || s == next;

    // Worker A: crypto vs USD/BRL — swap if USD/BRL is priority and crypto isn't.
    let usd_first = pri(Screen::UsdBrl) && !pri(Screen::Btc) && !pri(Screen::Sol);

    let ha = thread::Builder::new()
        .name("fetchA".into())
        .stack_size(10240)
        .spawn(move || {
            let mut d = MarketData::default();
            if usd_first {
                fetch_usd_brl(&mut d);
                fetch_crypto(&mut d);
            } else {
                fetch_crypto(&mut d);
                fetch_usd_brl(&mut d);
            }
            d
        })
        .unwrap();

    // Worker B: gold vs oil — swap if oil is priority and gold isn't.
    let oil_first = pri(Screen::Oil) && !pri(Screen::Gold);

    let hb = thread::Builder::new()
        .name("fetchB".into())
        .stack_size(10240)
        .spawn(move || {
            let mut d = MarketData::default();
            if oil_first {
                fetch_oil(&mut d);
                fetch_gold(&mut d);
            } else {
                fetch_gold(&mut d);
                fetch_oil(&mut d);
            }
            fetch_weather(&mut d);
            d
        })
        .unwrap();

    merge(data, ha.join().unwrap_or_default());
    merge(data, hb.join().unwrap_or_default());
}

/// Convert WMO weather code to a short label for the LCD.
pub fn wmo_label(code: i32) -> &'static str {
    match code {
        0 => "clear",
        1 => "fine",
        2 => "p.cld",
        3 => "cloud",
        45 | 48 => "fog",
        51 | 53 | 55 => "drzl",
        61 | 63 | 65 => "rain",
        71 | 73 | 75 => "snow",
        80 | 81 | 82 => "shwr",
        96 | 99 => "storm",
        _ => "wx",
    }
}

/// Returns true if the WMO code represents rain/precipitation.
pub fn wmo_is_rain(code: i32) -> bool {
    (51..=67).contains(&code) || (80..=86).contains(&code) || code >= 95
}
