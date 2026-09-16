// Hardware pins
// NOTE: these constants are documentation only - main.rs's actual PinDriver::input/output
// calls hardcode the real GPIO numbers directly (pre-existing in this codebase; these consts
// are flagged `never used` by cargo). Keep them in sync by hand when a pin moves.
pub const BUTTON_PIN: i32 = 26;
pub const LIGHT_BUTTON_PIN: i32 = 12;
pub const DISPLAY_BUTTON_PIN: i32 = 32;
pub const WARM_BUTTON_PIN: i32 = 13;
// Moved 2026-08-29: were 4/19 (opposite side of the ESP32 board from the other buttons) -
// now on the same side as BUTTON_PIN/LIGHT_BUTTON_PIN/WARM_BUTTON_PIN for a cleaner physical
// wiring layout. Freed up by moving the (physically disconnected, never removed) LCD's I2C
// bus off these two pins instead - see I2C_SDA/I2C_SCL below.
pub const BRIGHT_BUTTON_PIN: i32 = 14;
pub const MEDIA_BUTTON_PIN: i32 = 27;
// Moved 2026-08-29 from 14/27 (needed for BRIGHT_BUTTON_PIN/MEDIA_BUTTON_PIN above) to 22/23 -
// the LCD is still physically disconnected either way, so which exact pins its (otherwise
// unused) I2C bus claims doesn't matter functionally. Kept fully intact and enabled in code
// (not deleted, not feature-flagged off) per explicit instruction not to remove it for good.
pub const I2C_SDA: i32 = 22;
pub const I2C_SCL: i32 = 23;

// Laptop media bridge
pub const LAPTOP_PORT: u16 = 8765;
const fn laptop_ip_from_env() -> [u8; 4] {
    let b = env!("LAPTOP_IP").as_bytes();
    let mut ip = [0u8; 4];
    let mut octet = 0u32;
    let mut idx = 0usize;
    let mut i = 0usize;
    while i <= b.len() {
        let ch = if i < b.len() { b[i] } else { b'.' };
        if ch == b'.' {
            ip[idx] = octet as u8;
            octet = 0;
            idx += 1;
        } else {
            octet = octet * 10 + (ch - b'0') as u32;
        }
        i += 1;
    }
    ip
}
pub const LAPTOP_IP: [u8; 4] = laptop_ip_from_env();
pub const WIFI_LED_GREEN: i32 = 19; // green LED — WiFi connected (moved 2026-08-30, was 25)
pub const WIFI_LED_RED:   i32 = 33; // red LED   — WiFi down

// LCD
pub const LCD_ADDR: u8 = 0x27;
pub const LCD_COLS: usize = 20;
pub const LCD_ROWS: usize = 4;

// WiFi — set in .env, never commit those values
pub const WIFI_SSID: &str = env!("WIFI_SSID");
pub const WIFI_PASSWORD: &str = env!("WIFI_PASS");

// NTP
pub const NTP_SERVER: &str = "pool.ntp.org";
pub const GMT_OFFSET_SEC: i32 = -3 * 3600;

// Weather location (Florianopolis, Brazil)
pub const WEATHER_LAT: f32 = -27.5954;
pub const WEATHER_LON: f32 = -48.5480;

// Timing
// Kept only for ticker.rs's (dead-LCD, harmless) countdown-glyph math -- the periodic
// crypto/gold/oil/USD-BRL price-fetch cycle this originally paced was removed entirely
// 2026-08-31 (per explicit request: "we're not even using any of that" -- it was also the
// single biggest source of network_lock contention this whole session's lamp-responsiveness
// investigation kept running into). Weather moved to on-demand instead -- see
// WEATHER_MIN_REFRESH_MS below.
pub const FETCH_INTERVAL_MS: u64 = 300_000;
// Weather is now fetched on-demand when the web dashboard's page loads (see web.rs's "/" GET
// handler and main.rs's weatherFetch thread), not on a fixed background timer. This just stops
// a page reload (or several tabs open) from re-fetching on every single load -- weather doesn't
// change fast enough to need that anyway.
pub const WEATHER_MIN_REFRESH_MS: u64 = 600_000;
pub const AUTO_SCREEN_INTERVAL_MS: u64 = 30_000;
pub const LOADING_ANIM_MS: u64 = 200;
pub const LAMP_TOGGLE_ANIM_MS: u64 = 4_000;
pub const DEBOUNCE_MS: u64 = 50;
pub const CHART_DURATION_MS: u64 = 30_000;

// Volume potentiometer calibration (raw ADC 0-4095).
// Tune these if the pot doesn't reach 0% or 100% at its physical stops.
pub const POT_ADC_MIN: u32 = 285;  // ADC value at pot's fully-left stop
pub const POT_ADC_MAX: u32 = 3063; // ADC value at pot's fully-right stop

// HTTP
pub const HTTP_RETRIES: u32 = 1;
pub const HTTP_RETRY_DELAY_MS: u64 = 800;
pub const HTTP_TIMEOUT_MS: u64 = 6_000;

// Price-fetch API endpoints -- dormant while PRICE_FETCH_ENABLED is false, see that flag above.
pub const URL_COINGECKO: &str =
    "https://api.coingecko.com/api/v3/simple/price?ids=bitcoin,solana&vs_currencies=usd&include_24hr_change=true";
pub const URL_USDBRL: &str = "https://economia.awesomeapi.com.br/json/last/USD-BRL";
pub const URL_STOOQ_GOLD: &str = "https://stooq.com/q/l/?s=gc.f&i=d";
pub const URL_STOOQ_OIL: &str = "https://stooq.com/q/l/?s=cl.f&i=d";

// device-events reporting - plain HTTP (no TLS needed, it's on the LAN),
// fire-and-forget, short timeout so a slow/cold rabbitmq-sender never
// delays boot or the render loop.
pub const URL_RABBITMQ_SENDER: &str = "http://192.168.1.105:30080/send";
pub const EVENT_REPORT_TIMEOUT_MS: u64 = 3_000;

// Same `device-events` Postgres store as above, read back for the web UI's Climate chart --
// `event-dashboard`'s own REST API, already reading straight from the `events` table. No CORS
// headers on this API, so the browser can't fetch it cross-origin directly from the page this
// device serves -- this device fetches it server-side instead and relays the JSON same-origin
// (see web.rs's `/dht/chart` route), the same "device does the outbound HTTP" pattern already
// used for the price-fetch APIs below.
//
// Backs the Climate chart's default "Live" view AND its "1h/6h/24h" show-more buttons.
// Deliberately a SEPARATE, generic endpoint on event-dashboard (`/api/events/timeseries`, not
// a DHT/climate-specific route) - that app is a general logging tool for any event_type, and a
// route hardcoded to "climate"/"dht22_reading" would make it non-generic just because this one
// device happens to be the first consumer. Returns bucketed (time-averaged) points, bounded to
// a fixed small count regardless of `hours` -- "Live" originally used raw per-event rows
// instead (a SEPARATE, now-deleted endpoint/URL) capped at 20 events (10 minutes) specifically
// because of a real, measured heap-fragmentation ceiling (this device's largest contiguous
// free block, not its total free heap, consistently failed around 8192 bytes for raw JSON).
// Moved onto this bucketed endpoint 2026-08-31 (per request for "Live" to show more history)
// since it's strictly smaller AND covers more time: a 1-hour window buckets down to ~60 compact
// points (~900 bytes) versus the old 20-raw-event/~4.5KB response -- more visible history in a
// SMALLER, safer payload, with real fragmentation margin this device didn't have before.
pub fn url_dht_timeseries(hours: u32) -> String {
    std::format!(
        "http://192.168.1.105:30081/api/events/timeseries?event_type=dht22_reading&keys=temp_c,humidity_pct&hours={}",
        hours
    )
}

pub fn url_weather() -> String {
    format!(
        "https://api.open-meteo.com/v1/forecast?latitude={}&longitude={}&current=temperature_2m,weather_code&timezone=America%2FSao_Paulo",
        WEATHER_LAT, WEATHER_LON
    )
}

// Tuya lamp — key and IP from .env
const fn key_from_env() -> [u8; 16] {
    let b = env!("TUYA_KEY").as_bytes();
    assert!(b.len() == 16, "TUYA_KEY must be exactly 16 ASCII bytes");
    let mut arr = [0u8; 16];
    let mut i = 0;
    while i < 16 { arr[i] = b[i]; i += 1; }
    arr
}
const fn ip_from_env() -> [u8; 4] {
    let b = env!("TUYA_IP").as_bytes();
    let mut ip = [0u8; 4];
    let mut octet = 0u32;
    let mut idx = 0usize;
    let mut i = 0usize;
    while i <= b.len() {
        let ch = if i < b.len() { b[i] } else { b'.' };
        if ch == b'.' {
            ip[idx] = octet as u8;
            octet = 0;
            idx += 1;
        } else {
            octet = octet * 10 + (ch - b'0') as u32;
        }
        i += 1;
    }
    ip
}
pub const TUYA_DEVICE_KEY: [u8; 16] = key_from_env();
pub const TUYA_DEVICE_IP:  [u8; 4]  = ip_from_env();
pub const TUYA_DEVICE_PORT: u16 = 6668;
pub const TUYA_PROTOCOL_VERSION: u8 = 5; // 3.5

// Web control server — browse to http://<ESP_IP> when enabled
// Set WEB_SERVER=1 in .env to enable; omit or set to empty to disable.
pub const WEB_SERVER_ENABLED: bool = option_env!("WEB_SERVER").is_some();

// Hardware that's been physically removed/replaced - feature-flagged off
// rather than deleted, in case any of it is ever reconnected. The button
// (and its web toggle) still runs, it just no longer does anything beyond
// the shared red-flash/clear every button already does.
//
// Screen (LCD) unplugged - wasn't being used. Display-power button's
// "force screen off" toggle has nothing left to affect.
pub const DISPLAY_TOGGLE_ENABLED: bool = false;
// Volume potentiometer unplugged (too noisy) - replaced by the new
// keyboard's own volume scroll wheel. Nothing left for this on/off toggle
// to enable.
pub const POT_TOGGLE_ENABLED: bool = false;
// ExProtocol test endpoint not in active use right now (2026-08-30) - disabled per direct
// request to rule it out as a contributor to web/mic-srv reliability issues while it isn't
// actually needed, same "kept, not removed" pattern as the two flags above. Its dispatch
// thread was always one of the biggest single heap/CPU consumers on this device (real crypto
// work every handshake, its own network_lock contention) - flip back to true to re-enable.
pub const EXPROTOCOL_ENABLED: bool = false;
// Crypto/gold/oil/USD-BRL price fetching - disabled per direct request 2026-08-30
// ("we're not even using any of that"), same "kept, not removed" pattern as the flags above so
// it can come back with one flip if that changes. When false, main.rs never spawns the finFetch
// thread at all -- no worker threads, no stack/heap reserved for them, no network_lock
// contention, not just "fetches nothing." Weather is unaffected by this flag -- it moved to its
// own always-on, on-demand-per-dashboard-load path (see main.rs's `weatherFetch` thread) the
// same day, since it was a separate, valid ask ("only fetch when someone actually goes to the
// dashboard").
pub const PRICE_FETCH_ENABLED: bool = false;

// Screen identifiers
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Screen {
    Btc = 0,
    Sol = 1,
    Gold = 2,
    Oil = 3,
    UsdBrl = 4,
}

impl Screen {
    pub const COUNT: usize = 5;

    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Btc),
            1 => Some(Self::Sol),
            2 => Some(Self::Gold),
            3 => Some(Self::Oil),
            4 => Some(Self::UsdBrl),
            _ => None,
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Btc => Self::Sol,
            Self::Sol => Self::Gold,
            Self::Gold => Self::Oil,
            Self::Oil => Self::UsdBrl,
            Self::UsdBrl => Self::Btc,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Btc => " BTC / USD",
            Self::Sol => " SOL / USD",
            Self::Gold => " GOLD USD/oz",
            Self::Oil => " OIL WTI/bbl",
            Self::UsdBrl => " USD / BRL",
        }
    }

    pub fn decimals(self) -> u8 {
        match self {
            Self::Btc => 0,
            Self::Sol | Self::Gold | Self::Oil => 2,
            Self::UsdBrl => 4,
        }
    }
}
