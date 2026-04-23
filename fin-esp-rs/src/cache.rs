/// NVS-backed price cache.
///
/// Stores the last known market prices to flash so that the ticker shows real
/// data immediately on boot rather than dashes until the first API fetch
/// completes (~10–30 s after WiFi connects).
///
/// Only fields guarded by an ok_* flag are written, so a partial fetch result
/// never overwrites the cached value for a market that failed.

use crate::api::MarketData;
use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs};
use log::info;

const NS: &str = "fin";

pub fn save(partition: &EspDefaultNvsPartition, data: &MarketData) {
    let Ok(mut nvs) = EspNvs::new(partition.clone(), NS, true) else {
        info!("[cache] NVS open failed — skip save");
        return;
    };
    if data.ok_crypto {
        w(&mut nvs, "p_btc",  data.price_btc);
        w(&mut nvs, "p_sol",  data.price_sol);
        w(&mut nvs, "c_btc",  data.chg_btc_pct);
        w(&mut nvs, "c_sol",  data.chg_sol_pct);
    }
    if data.ok_usd_brl {
        w(&mut nvs, "p_usd",  data.price_usd_brl);
        w(&mut nvs, "c_usd",  data.chg_usd_brl_pct);
    }
    if data.ok_gold {
        w(&mut nvs, "p_gold", data.price_gold);
        w(&mut nvs, "c_gold", data.chg_gold_pct);
    }
    if data.ok_oil {
        w(&mut nvs, "p_oil",  data.price_oil);
        w(&mut nvs, "c_oil",  data.chg_oil_pct);
    }
    if data.ok_weather {
        if let Some(temp) = data.weather_temp { w(&mut nvs, "w_temp", temp); }
        if let Some(code) = data.weather_code { w(&mut nvs, "w_code", code as f64); }
    }
    info!("[cache] saved");
}

pub fn load(partition: &EspDefaultNvsPartition) -> Option<MarketData> {
    let Ok(mut nvs) = EspNvs::new(partition.clone(), NS, true) else {
        return None;
    };
    let mut data = MarketData::default();
    let mut any = false;

    if let Some(v) = r(&mut nvs, "p_btc").filter(|&v| v > 0.0) {
        data.price_btc   = v;
        data.price_sol   = r(&mut nvs, "p_sol").unwrap_or(0.0);
        data.chg_btc_pct = r(&mut nvs, "c_btc").unwrap_or(0.0);
        data.chg_sol_pct = r(&mut nvs, "c_sol").unwrap_or(0.0);
        data.ok_crypto   = true;
        any = true;
    }
    if let Some(v) = r(&mut nvs, "p_usd").filter(|&v| v > 0.0) {
        data.price_usd_brl    = v;
        data.chg_usd_brl_pct  = r(&mut nvs, "c_usd").unwrap_or(0.0);
        data.ok_usd_brl       = true;
        any = true;
    }
    if let Some(v) = r(&mut nvs, "p_gold").filter(|&v| v > 0.0) {
        data.price_gold   = v;
        data.chg_gold_pct = r(&mut nvs, "c_gold").unwrap_or(0.0);
        data.ok_gold      = true;
        data.has_chg_gold = true;
        any = true;
    }
    if let Some(v) = r(&mut nvs, "p_oil").filter(|&v| v > 0.0) {
        data.price_oil   = v;
        data.chg_oil_pct = r(&mut nvs, "c_oil").unwrap_or(0.0);
        data.ok_oil      = true;
        data.has_chg_oil = true;
        any = true;
    }
    if let (Some(temp), Some(code)) = (r(&mut nvs, "w_temp"), r(&mut nvs, "w_code")) {
        data.weather_temp = Some(temp);
        data.weather_code = Some(code as i32);
        data.ok_weather   = true;
    }

    if any {
        info!("[cache] loaded: BTC={:.0} SOL={:.2} USD={:.4} Gold={:.2} Oil={:.2}",
            data.price_btc, data.price_sol, data.price_usd_brl,
            data.price_gold, data.price_oil);
        Some(data)
    } else {
        info!("[cache] empty");
        None
    }
}

// ── raw NVS helpers (f64 stored as 8 LE bytes) ────────────────────────────────

fn w(nvs: &mut EspNvs<esp_idf_svc::nvs::NvsDefault>, key: &str, v: f64) {
    let _ = nvs.set_blob(key, &v.to_bits().to_le_bytes());
}

fn r(nvs: &mut EspNvs<esp_idf_svc::nvs::NvsDefault>, key: &str) -> Option<f64> {
    let mut buf = [0u8; 8];
    // Borrow of buf ends after map() extracts the length.
    let len = nvs.get_blob(key, &mut buf)
        .ok()
        .flatten()
        .map(|s| s.len())
        .unwrap_or(0);
    if len == 8 { Some(f64::from_bits(u64::from_le_bytes(buf))) } else { None }
}
