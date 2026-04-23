// Hardware pins
pub const BUTTON_PIN: i32 = 26;
pub const LIGHT_BUTTON_PIN: i32 = 12;
pub const DISPLAY_BUTTON_PIN: i32 = 32;
pub const WARM_BUTTON_PIN: i32 = 13;
pub const BRIGHT_BUTTON_PIN: i32 = 4;
pub const MEDIA_BUTTON_PIN: i32 = 19;
pub const I2C_SDA: i32 = 14;
pub const I2C_SCL: i32 = 27;

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
pub const WIFI_LED_GREEN: i32 = 25; // green LED — WiFi connected
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
pub const FETCH_INTERVAL_MS: u64 = 45_000;
pub const AUTO_SCREEN_INTERVAL_MS: u64 = 30_000;
pub const LOADING_ANIM_MS: u64 = 200;
pub const LAMP_TOGGLE_ANIM_MS: u64 = 4_000;
pub const DEBOUNCE_MS: u64 = 220;
pub const CHART_DURATION_MS: u64 = 30_000;

// HTTP
pub const HTTP_RETRIES: u32 = 2;
pub const HTTP_RETRY_DELAY_MS: u64 = 800;
pub const HTTP_TIMEOUT_MS: u64 = 10_000;

// API endpoints
pub const URL_COINGECKO: &str =
    "https://api.coingecko.com/api/v3/simple/price?ids=bitcoin,solana&vs_currencies=usd&include_24hr_change=true";
pub const URL_USDBRL: &str = "https://economia.awesomeapi.com.br/json/last/USD-BRL";
pub const URL_STOOQ_GOLD: &str = "https://stooq.com/q/l/?s=gc.f&i=d";
pub const URL_STOOQ_OIL: &str = "https://stooq.com/q/l/?s=cl.f&i=d";

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
