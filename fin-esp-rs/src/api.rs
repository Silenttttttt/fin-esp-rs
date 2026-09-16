use esp_idf_svc::http::client::{Configuration as HttpConfig, EspHttpConnection};
use esp_idf_svc::http::Method;
use log::{error, info, warn};
use serde_json::Value;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use crate::config;

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
    https_get_fresh(url).map(|(body, _conn)| body)
}

/// For web.rs's "show more" range buttons AND the default "Live" chart (2026-08-31, moved off
/// its own raw-event endpoint -- see dht22.rs's own comment on why) -- fetches bucketed
/// temp/humidity history from event-dashboard's generic `/api/events/timeseries` endpoint (see
/// `config::url_dht_timeseries`'s own comment for why that's a separate, generic route rather
/// than a DHT-specific one). Same `https_get` machinery as every other fetch in this file.
pub fn fetch_dht_timeseries_json(hours: u32) -> Result<String, String> {
    https_get(&config::url_dht_timeseries(hours))
}

// Real coredump-confirmed crash (2026-08-30), traced to this exact function:
// `panic_abort -> ... -> alloc::raw_vec::handle_error -> Vec<u8>::spec_extend ->
// fin_esp_rs::api::read_body`. `Vec::with_capacity`/`extend_from_slice`'s internal growth has
// no fallible path -- a heap allocation failure aborts the ENTIRE DEVICE, same bug class
// already fixed in `ota.rs`/`web.rs` earlier the same session. This function is shared by
// every HTTP GET in this file (price fetches, and now `fetch_dht_history_json`) -- the dht
// chart fetch's bigger response (up to 200 events) hit this far more often than the small
// price-API responses ever did, which is how a pre-existing bug in shared code finally
// surfaced. `try_reserve`/`try_reserve_exact` turn every growth step into a graceful `Err`
// instead, matching this codebase's now-consistent pattern.
fn read_body(conn: &mut EspHttpConnection) -> Result<String, String> {
    let mut body = Vec::new();
    body.try_reserve_exact(2048).map_err(|e| format!("body buffer alloc failed: {e}"))?;
    let mut buf = [0u8; 512];
    loop {
        let n = conn.read(&mut buf).map_err(|e| format!("HTTP read: {e}"))?;
        if n == 0 {
            break;
        }
        body.try_reserve(n).map_err(|e| format!("body buffer grow failed at {} bytes: {e}", body.len()))?;
        body.extend_from_slice(&buf[..n]);
    }
    String::from_utf8(body).map_err(|e| format!("UTF-8: {e}"))
}

/// Same retry-with-a-fresh-connection-per-attempt behavior as the original `https_get`, but
/// also returns the successful connection so a caller with a second same-host request queued
/// up (see `https_get_reuse_or_fresh` / `fetch_gold_and_oil` below) can reuse it instead of
/// paying for a second fresh TLS handshake.
fn https_get_fresh(url: &str) -> Result<(String, EspHttpConnection), String> {
    let http_config = HttpConfig {
        timeout: Some(Duration::from_millis(config::HTTP_TIMEOUT_MS)),
        use_global_ca_store: true,
        crt_bundle_attach: Some(esp_idf_svc::sys::esp_crt_bundle_attach),
        ..Default::default()
    };

    // Fresh connection per attempt — reusing across retries without draining the
    // response body leaks internal HTTP client buffers on non-200 responses.
    for attempt in 0..config::HTTP_RETRIES {
        let mut conn = EspHttpConnection::new(&http_config)
            .map_err(|e| format!("HTTP conn: {e}"))?;

        conn.initiate_request(Method::Get, url, &[])
            .map_err(|e| format!("HTTP init: {e}"))?;
        conn.initiate_response()
            .map_err(|e| format!("HTTP resp: {e}"))?;

        let status = conn.status();
        if status == 200 {
            let body = read_body(&mut conn)?;
            return Ok((body, conn));
        }

        // conn dropped here — cleans up TLS session and HTTP client resources.
        drop(conn);

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

/// Tries `url` on an already-open connection first — same host, no fresh TLS handshake, real
/// ESP-IDF `esp_http_client` keep-alive behavior (confirmed by reading `esp-idf-svc`'s own
/// `EspHttpConnection::initiate_request` source: it's explicitly designed to be called more
/// than once on the same connection object, auto-draining any leftover response first). Falls
/// back to `https_get_fresh`'s own independent retry loop if the reuse attempt fails for ANY
/// reason -- this must never make the caller's second request less reliable than two fully
/// independent `https_get` calls would have been, only cheaper on the common happy path.
fn https_get_reuse_or_fresh(mut conn: EspHttpConnection, url: &str) -> Result<(String, EspHttpConnection), String> {
    if conn.initiate_request(Method::Get, url, &[]).is_ok()
        && conn.initiate_response().is_ok()
        && conn.status() == 200
    {
        if let Ok(body) = read_body(&mut conn) {
            return Ok((body, conn));
        }
    }
    https_get_fresh(url)
}

struct EventMsg {
    event_type: &'static str,
    severity: &'static str,
    message: String,
    // Was a plain `mechanism: &'static str` - widened to a real JSON value so callers with
    // actual structured data (e.g. dht22's temp_c/humidity_pct) can send it as real queryable
    // JSONB fields in Postgres, not just embedded in the free-text `message`. `report_event()`
    // below still exists unchanged for every existing caller (button_press/wifi/boot) and just
    // wraps its `mechanism` string into `{"mechanism": ...}` before calling this.
    metadata: serde_json::Value,
}

/// One persistent worker thread, not one thread per event. The original
/// design spawned a brand-new OS thread (16KB heap-allocated stack each)
/// for every single report_event() call - fine for the rare boot/wifi
/// events it started with, but once button presses started calling this
/// too, several of these could spawn in quick succession and the device
/// hit a real Guru Meditation panic (IllegalInstruction) shortly after
/// boot - confirmed via actual serial console output, not guessed. An
/// unbounded mpsc queue + one long-lived worker thread avoids repeated
/// thread-stack allocation entirely; queueing a message is just a cheap
/// heap allocation for the message itself, not a whole new stack.
fn event_worker(rx: std::sync::mpsc::Receiver<EventMsg>) {
    for msg in rx {
        // Same guard, same reason as report_event/dht22.rs's own -- this wraps EVERY event
        // (already-built `msg.metadata` included) into one more `serde_json::json!()` object,
        // on this one shared worker thread that every single event report passes through. No
        // fallible path here either; dropping this one message (it'll simply be missing from
        // Postgres, same as any other transient reporting failure this pipeline already
        // tolerates) beats aborting the whole device over a logging side-channel.
        //
        // Checks the LARGEST CONTIGUOUS free block (`heap_caps_get_largest_free_block`), not
        // total free heap (`esp_get_free_heap_size`) -- corrected 2026-08-30 after a real
        // coredump showed `web.rs`'s own `build_status` still aborting on this exact class of
        // bug even with a total-free-heap guard already in place: total free heap can look
        // perfectly healthy while badly fragmented, which is exactly this device's own
        // well-documented recurring failure mode (see project_esp32_heap_budget.md's
        // fragmentation-ceiling investigation) -- a single contiguous allocation only ever
        // cares about the biggest block it could land in, not the sum of many small ones.
        if unsafe { esp_idf_sys::heap_caps_get_largest_free_block(esp_idf_sys::MALLOC_CAP_8BIT) } < 2048 {
            warn!("[events] heap too low to safely send {}, dropping", msg.event_type);
            continue;
        }
        let payload = serde_json::json!({
            "queue": "device-events",
            "message": {
                "source": "fin-esp",
                "event_type": msg.event_type,
                "severity": msg.severity,
                "message": msg.message,
                "metadata": msg.metadata,
            },
        });
        let body = payload.to_string();

        let http_config = HttpConfig {
            timeout: Some(Duration::from_millis(config::EVENT_REPORT_TIMEOUT_MS)),
            ..Default::default()
        };

        let result = (|| -> Result<u16, String> {
            let mut conn = EspHttpConnection::new(&http_config)
                .map_err(|e| format!("conn: {e}"))?;
            conn.initiate_request(
                Method::Post,
                config::URL_RABBITMQ_SENDER,
                &[
                    ("Content-Type", "application/json"),
                    ("Content-Length", &body.len().to_string()),
                ],
            )
            .map_err(|e| format!("init: {e}"))?;
            conn.write_all(body.as_bytes()).map_err(|e| format!("write: {e}"))?;
            conn.initiate_response().map_err(|e| format!("resp: {e}"))?;
            let status = conn.status();
            // Drain the response body - matches https_get's own reasoning:
            // leaving it unread leaks internal HTTP client buffers.
            let mut buf = [0u8; 256];
            while conn.read(&mut buf).unwrap_or(0) > 0 {}
            Ok(status)
        })();

        match result {
            Ok(status) if (200..300).contains(&status) => {
                info!("[events] reported {}: {}", msg.event_type, status);
            }
            Ok(status) => warn!("[events] {} got HTTP {}", msg.event_type, status),
            Err(e) => warn!("[events] {} failed: {}", msg.event_type, e),
        }
    }
}

static EVENT_TX: std::sync::OnceLock<std::sync::mpsc::Sender<EventMsg>> = std::sync::OnceLock::new();

/// Fire-and-forget device-events report via rabbitmq-sender - queues onto
/// the single persistent worker thread (see event_worker above) so a
/// slow/cold/unreachable rabbitmq-sender, or a burst of several events in
/// quick succession, can never delay boot or the render loop, and can
/// never spawn more than the one worker thread this module ever creates.
pub fn report_event(event_type: &'static str, severity: &'static str, message: String, mechanism: &'static str) {
    // Same guard, same reason as dht22.rs's own report call -- see that file's comment for the
    // real coredump this bug class was confirmed with. `serde_json::json!()`'s `Map` has no
    // fallible allocation path, so this single-field object can abort the whole device under
    // heap pressure exactly like the bigger one did; every button-press/boot/wifi event in the
    // codebase funnels through this one function, so this is the highest-leverage place to
    // guard it. Checks the largest CONTIGUOUS free block, not total free heap -- see
    // event_worker's own comment above for why that distinction matters on this device.
    if unsafe { esp_idf_sys::heap_caps_get_largest_free_block(esp_idf_sys::MALLOC_CAP_8BIT) } >= 2048 {
        report_event_with_metadata(event_type, severity, message, serde_json::json!({ "mechanism": mechanism }));
    } else {
        warn!("[events] heap too low to safely report {event_type}, skipping");
    }
}

/// Same worker/channel as `report_event` above (one persistent thread, never one per event) --
/// for callers with real structured data to report (e.g. dht22's temp_c/humidity_pct) instead
/// of just a `mechanism` string, so it lands as real queryable JSONB fields in Postgres rather
/// than buried in free-text `message`.
pub fn report_event_with_metadata(event_type: &'static str, severity: &'static str, message: String, metadata: serde_json::Value) {
    let tx = EVENT_TX.get_or_init(|| {
        // Found via code review: this closure's own `.expect()` used to be unprotected, unlike
        // every other thread spawn in this codebase (`finFetch`/`lampBridge`/`fetchA`/`fetchB`/
        // `ota`, all retry-with-backoff). `get_or_init` runs this closure lazily on this
        // function's FIRST real call across the whole process -- in practice that's the
        // `report_event("boot", ...)` call shortly after WiFi connects, i.e. right in this
        // device's real, regularly-observed early heap-pressure window, not a safely-early
        // boot-only point. A fresh `tx`/`rx` pair is created on every retry attempt -- reusing
        // an `rx` already consumed by a failed `spawn()` call would leave the eventually-returned
        // `tx` talking to a channel nothing is listening on.
        loop {
            let (tx, rx) = std::sync::mpsc::channel::<EventMsg>();
            match std::thread::Builder::new()
                .stack_size(16384)
                .spawn(move || event_worker(rx))
            {
                Ok(_) => break tx,
                Err(e) => {
                    error!("[boot] device-events worker spawn failed: {e}, retrying in 500ms");
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
        }
    });
    if let Err(e) = tx.send(EventMsg { event_type, severity, message, metadata }) {
        warn!("[events] failed to queue {event_type}: {e}");
    }
}

// Dormant while `config::PRICE_FETCH_ENABLED` is false (2026-08-30, "we're not even using any
// of that") -- kept, not deleted, same "flip the flag to bring it back" pattern as
// EXPROTOCOL_ENABLED. Nothing in this section runs, spawns a thread, or touches the network
// unless that flag is true; see main.rs's finFetch thread, which is itself only spawned when
// the flag is on.
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

/// Single data line, no header: Symbol,Date,Time,Open,High,Low,Close,,
fn parse_stooq_body(body: &str) -> Result<(f64, f64, bool), String> {
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

/// Fetches gold then oil, reusing ONE TLS connection across both when gold's own fetch
/// succeeds -- both hit the same host (stooq.com, see `config::URL_STOOQ_GOLD`/`URL_STOOQ_OIL`),
/// so this cuts one whole fresh TLS handshake out of this back-to-back pair. If gold's fetch
/// fails outright (nothing to reuse) or the reuse attempt for oil fails for any reason,
/// `https_get_reuse_or_fresh`/`https_get_fresh` fall back to a fully independent fresh
/// connection for oil -- reliability must never regress relative to two fully independent
/// `https_get` calls, only cost should improve on the happy path.
pub fn fetch_gold_and_oil(data: &mut MarketData) {
    info!("[API] fetching gold");
    let gold_conn = match https_get_fresh(config::URL_STOOQ_GOLD) {
        Ok((body, conn)) => {
            match parse_stooq_body(&body) {
                Ok((price, chg, has)) => {
                    data.price_gold = price;
                    data.chg_gold_pct = chg;
                    data.has_chg_gold = has;
                    data.ok_gold = true;
                }
                Err(e) => warn!("[API] gold parse failed: {e}"),
            }
            Some(conn)
        }
        Err(e) => {
            warn!("[API] gold fetch failed: {e}");
            None
        }
    };

    info!("[API] fetching oil");
    let oil_result = match gold_conn {
        Some(conn) => https_get_reuse_or_fresh(conn, config::URL_STOOQ_OIL),
        None => https_get_fresh(config::URL_STOOQ_OIL),
    };
    match oil_result {
        Ok((body, _conn)) => match parse_stooq_body(&body) {
            Ok((price, chg, has)) => {
                data.price_oil = price;
                data.chg_oil_pct = chg;
                data.has_chg_oil = has;
                data.ok_oil = true;
            }
            Err(e) => warn!("[API] oil parse failed: {e}"),
        },
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

// Weather is always on now, on-demand (main.rs's `weatherFetch` thread, triggered from web.rs's
// "/" GET handler) -- unlike crypto/gold/oil above, it's NOT gated by PRICE_FETCH_ENABLED, since
// disabling it was never asked for ("only fetch when someone actually goes to the dashboard" is
// a cadence change, not a request to turn it off). First attempt at on-demand weather
// (2026-08-30) spawned a FRESH throwaway thread per dashboard load and was reverted the same day
// after real repeated crashes: a fresh 32KB TLS stack essentially never finds room on this
// device's real ~12-20KB free heap at an unpredictable moment. The fix that stuck: one
// ALREADY-alive, already-TLS-capable persistent thread (started once at boot, blocked on a
// trigger the rest of the time) picks up the request instead of a new thread being spawned for
// it -- same lesson the crypto/gold/oil workers below already relied on.


// ── Persistent weather worker ───────────────────────────────────────────────────
// One always-alive thread, blocked on a Condvar until triggered, instead of spawning a fresh
// TLS-capable thread per dashboard load -- see `fetch_weather`'s own comment above for why a
// fresh-thread-per-request design was tried and reverted here already.

pub(crate) struct FetchWorker {
    go:     (Mutex<bool>, Condvar),
    result: (Mutex<Option<MarketData>>, Condvar),
}

impl FetchWorker {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            go:     (Mutex::new(false), Condvar::new()),
            result: (Mutex::new(None),  Condvar::new()),
        })
    }

    pub(crate) fn trigger(&self) {
        *self.go.0.lock().unwrap() = true;
        self.go.1.notify_one();
    }

    fn wait_go(&self) {
        let mut g = self.go.0.lock().unwrap();
        loop {
            if *g { *g = false; return; }
            g = self.go.1.wait(g).unwrap();
        }
    }

    fn post(&self, d: MarketData) {
        *self.result.0.lock().unwrap() = Some(d);
        self.result.1.notify_one();
    }

    pub(crate) fn collect(&self) -> MarketData {
        let mut g = self.result.0.lock().unwrap();
        loop {
            if let Some(d) = g.take() { return d; }
            g = self.result.1.wait(g).unwrap();
        }
    }
}

/// Spawn the persistent weather worker. Call once at boot; main.rs's own `weatherFetch` thread
/// triggers it on-demand (dashboard page load, rate-limited by `config::WEATHER_MIN_REFRESH_MS`)
/// and blocks on `collect()` for the result -- that supervisor thread, not this one, owns the
/// network_lock acquisition around the actual fetch.
pub(crate) fn spawn_weather_worker() -> Arc<FetchWorker> {
    let w = FetchWorker::new();
    let inner = Arc::clone(&w);
    // Same retry-with-backoff reasoning as every other thread spawn in this codebase (see
    // fetchA/fetchB's old comment, preserved in git-adjacent history) -- a bare `.unwrap()` here
    // would abort the whole device if this spawn ever loses a boot-time heap race.
    loop {
        let inner = Arc::clone(&inner);
        match thread::Builder::new()
            .name("weatherWorker".into())
            .stack_size(32768)
            .spawn(move || loop {
                inner.wait_go();
                let mut d = MarketData::default();
                fetch_weather(&mut d);
                inner.post(d);
            }) {
            Ok(_) => break,
            Err(e) => {
                error!("[boot] weatherWorker spawn failed: {e}, retrying in 500ms");
                thread::sleep(Duration::from_millis(500));
            }
        }
    }
    w
}

/// Merge a partial price-fetch result — only overwrites fields that succeeded this cycle,
/// preserving last-known-good values for anything that failed. Dormant while
/// `config::PRICE_FETCH_ENABLED` is false -- see that flag's own comment.
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
}

/// Spawn the two persistent price-fetch workers (crypto+USD-BRL, gold+oil). Only ever called
/// from main.rs when `config::PRICE_FETCH_ENABLED` is true -- when it's false, neither this nor
/// any thread/stack/heap it would have used exists at all.
///
/// `network_lock` is taken and released AROUND EACH INDIVIDUAL HTTPS call, not once for the
/// whole worker run -- holding one guard across a whole multi-call cycle used to let it
/// monopolize network_lock for up to ~30s worst case, freezing the Tuya lamp bridge, mic/media
/// ownership claims, and the dht22 chart refresher for that whole window (confirmed live
/// 2026-08-31). Locking per-call instead of per-cycle keeps the same safety invariant (still
/// never two TLS/network ops truly concurrent -- this device's heap can't survive that, see
/// fetch_all's own comment below) while shrinking the window down to one HTTP call's worth.
pub(crate) fn spawn_price_fetch_workers(network_lock: Arc<Mutex<()>>) -> (Arc<FetchWorker>, Arc<FetchWorker>) {
    let wa = FetchWorker::new();
    let wb = FetchWorker::new();

    {
        let w = Arc::clone(&wa);
        // A bare `.unwrap()` here used to abort the whole device whenever this spawn failed
        // under boot-time heap pressure -- retrying gives concurrent allocations a chance to
        // free first instead of treating a transient resource shortage as fatal.
        loop {
            let w = Arc::clone(&w);
            let network_lock = Arc::clone(&network_lock);
            match thread::Builder::new()
                .name("fetchA".into())
                .stack_size(32768)
                .spawn(move || loop {
                    w.wait_go();
                    let mut d = MarketData::default();
                    { let _g = network_lock.lock().unwrap(); fetch_crypto(&mut d); }
                    { let _g = network_lock.lock().unwrap(); fetch_usd_brl(&mut d); }
                    let heap = unsafe { esp_idf_sys::esp_get_free_heap_size() };
                    info!("[fetchA] heap free: {} bytes", heap);
                    w.post(d);
                }) {
                Ok(_) => break,
                Err(e) => {
                    error!("[boot] fetchA spawn failed: {e}, retrying in 500ms");
                    thread::sleep(Duration::from_millis(500));
                }
            }
        }
    }
    {
        let w = Arc::clone(&wb);
        // Same fix, same reasoning as fetchA immediately above.
        loop {
            let w = Arc::clone(&w);
            let network_lock = Arc::clone(&network_lock);
            match thread::Builder::new()
                .name("fetchB".into())
                .stack_size(32768)
                .spawn(move || loop {
                    w.wait_go();
                    let mut d = MarketData::default();
                    { let _g = network_lock.lock().unwrap(); fetch_gold_and_oil(&mut d); }
                    let heap = unsafe { esp_idf_sys::esp_get_free_heap_size() };
                    info!("[fetchB] heap free: {} bytes", heap);
                    w.post(d);
                }) {
                Ok(_) => break,
                Err(e) => {
                    error!("[boot] fetchB spawn failed: {e}, retrying in 500ms");
                    thread::sleep(Duration::from_millis(500));
                }
            }
        }
    }

    (wa, wb)
}

/// Trigger each worker and wait for it before starting the next. This device's heap cannot
/// sustain 2 concurrent TLS sessions, full stop (confirmed via serial: "alloc(N bytes) failed"
/// heap exhaustion, mbedtls handshake errors, even cert-bundle verification failures when two
/// ran at once) -- `include_b` skips worker B (gold, oil) on most calls, since it's the priciest
/// (2 sequential HTTPS calls vs worker A's 2 as well, but historically the flakier pair under
/// fragmentation) and nothing here has a physical screen needing it fresh every cycle anyway.
pub fn fetch_all(data: &mut MarketData, wa: &FetchWorker, wb: &FetchWorker, include_b: bool) {
    wa.trigger();
    merge(data, wa.collect());
    if include_b {
        wb.trigger();
        merge(data, wb.collect());
    }
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
