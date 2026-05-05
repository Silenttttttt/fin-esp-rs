mod api;
mod cache;
mod cgram;
mod chart;
mod config;
mod fmt;
mod glyphs;
mod history;
mod lcd;
mod ota;
mod sand;
mod screen;
mod ticker;
mod tuya;

use esp_idf_hal::adc::attenuation;
use esp_idf_hal::adc::oneshot::{AdcChannelDriver, AdcDriver};
use esp_idf_hal::adc::oneshot::config::AdcChannelConfig;
use esp_idf_hal::delay::FreeRtos;
use esp_idf_hal::gpio::{PinDriver, Pull};
use esp_idf_hal::i2c::{I2cConfig, I2cDriver};
use esp_idf_hal::peripherals::Peripherals;
use esp_idf_hal::units::FromValueType;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::sntp::EspSntp;
use esp_idf_svc::wifi::{BlockingWifi, ClientConfiguration, Configuration, EspWifi};
use log::{info, warn};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::io::BufRead;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// LCD column where the 5-wide sand canvas starts (centered: (20-5)/2 = 7).
const SAND_COL: u8 = 7;

fn main() {
    esp_idf_svc::log::EspLogger::initialize_default();
    info!("Fin-ESP-RS: boot");

    let peripherals = Peripherals::take().unwrap();

    // ── I2C bus recovery ─────────────────────────────────────────────────────
    // After a soft reset (OTA, panic), the PCF8574 may hold SDA low mid-byte.
    // A power cycle releases it; a soft reset does not. Nine SCL pulses clock
    // out any in-flight byte so the bus is clean before I2cDriver takes over.
    unsafe {
        const SCL: i32 = config::I2C_SCL;
        const SDA: i32 = config::I2C_SDA;
        esp_idf_sys::gpio_set_direction(SCL, esp_idf_sys::gpio_mode_t_GPIO_MODE_OUTPUT);
        esp_idf_sys::gpio_set_direction(SDA, esp_idf_sys::gpio_mode_t_GPIO_MODE_OUTPUT);
        esp_idf_sys::gpio_set_level(SDA, 1);
        for _ in 0..9 {
            esp_idf_sys::gpio_set_level(SCL, 0);
            FreeRtos::delay_ms(1);
            esp_idf_sys::gpio_set_level(SCL, 1);
            FreeRtos::delay_ms(1);
        }
        // STOP condition: SDA low → high while SCL is high
        esp_idf_sys::gpio_set_level(SDA, 0);
        FreeRtos::delay_ms(1);
        esp_idf_sys::gpio_set_level(SDA, 1);
        FreeRtos::delay_ms(1);
    }

    // ── LCD first: user sees feedback before heavy WiFi init ─────────────────
    let i2c_config = I2cConfig::new().baudrate(200u32.kHz().into());
    let i2c_driver = I2cDriver::new(
        peripherals.i2c0,
        peripherals.pins.gpio14,
        peripherals.pins.gpio27,
        &i2c_config,
    ).unwrap();
    let i2c = Arc::new(Mutex::new(i2c_driver));
    let mut lcd = lcd::Lcd::new(
        unsafe { &*(Arc::as_ptr(&i2c) as *const Mutex<I2cDriver>) },
        config::LCD_ADDR,
    );
    lcd.init();

    // Let power rails settle before the WiFi radio starts.
    // Cold power-on (POWERON): capacitors are charging, supply is soft → 1 s.
    // Brownout reset: supply couldn't handle the radio spike last time → 3 s.
    // Software resets (OTA, watchdog, panic) need no delay — supply is stable.
    match unsafe { esp_idf_sys::esp_reset_reason() } {
        r if r == esp_idf_sys::esp_reset_reason_t_ESP_RST_POWERON => {
            info!("[boot] cold start — settling 1 s");
            FreeRtos::delay_ms(1000);
        }
        r if r == esp_idf_sys::esp_reset_reason_t_ESP_RST_BROWNOUT => {
            info!("[boot] brownout reset — settling 3 s");
            FreeRtos::delay_ms(3000);
        }
        r if r == esp_idf_sys::esp_reset_reason_t_ESP_RST_PANIC => {
            info!("[boot] reset reason: PANIC (stack overflow or abort)");
        }
        r if r == esp_idf_sys::esp_reset_reason_t_ESP_RST_INT_WDT => {
            info!("[boot] reset reason: INT WATCHDOG");
        }
        r if r == esp_idf_sys::esp_reset_reason_t_ESP_RST_TASK_WDT => {
            info!("[boot] reset reason: TASK WATCHDOG");
        }
        r if r == esp_idf_sys::esp_reset_reason_t_ESP_RST_WDT => {
            info!("[boot] reset reason: OTHER WATCHDOG");
        }
        r if r == esp_idf_sys::esp_reset_reason_t_ESP_RST_SW => {
            info!("[boot] reset reason: software reset (OTA or esp_restart)");
        }
        r => {
            info!("[boot] reset reason: unknown ({})", r);
        }
    }

    let btn_screen  = PinDriver::input(peripherals.pins.gpio26, Pull::Up).unwrap();
    let btn_light   = PinDriver::input(peripherals.pins.gpio12, Pull::Up).unwrap();
    let btn_display = PinDriver::input(peripherals.pins.gpio32, Pull::Up).unwrap();
    let btn_warm    = PinDriver::input(peripherals.pins.gpio13, Pull::Up).unwrap();
    let btn_bright  = PinDriver::input(peripherals.pins.gpio4,  Pull::Up).unwrap();
    let btn_chart   = PinDriver::input(peripherals.pins.gpio18, Pull::Up).unwrap();
    let btn_media   = PinDriver::input(peripherals.pins.gpio19, Pull::Up).unwrap();
    let mut led_green = esp_idf_hal::gpio::PinDriver::output(peripherals.pins.gpio25).unwrap();
    let mut led_red   = esp_idf_hal::gpio::PinDriver::output(peripherals.pins.gpio33).unwrap();
    let mut led_blue  = esp_idf_hal::gpio::PinDriver::output(peripherals.pins.gpio5).unwrap();

    let adc = AdcDriver::new(peripherals.adc1).unwrap();
    let mut vol_pin = AdcChannelDriver::new(
        &adc,
        peripherals.pins.gpio34,
        &AdcChannelConfig { attenuation: attenuation::DB_11, ..Default::default() },
    ).unwrap();
    led_green.set_low().unwrap();
    led_red.set_high().unwrap(); // red on until WiFi connects
    led_blue.set_low().unwrap();

    // ── Particle loading screen: full-screen sand/water on all 4 rows ──────────
    // Clear all 8 CGRAM slots — LCD CGRAM persists across soft resets (OTA), so
    // old main-screen glyphs would otherwise show through during the animation.
    for s in 0u8..8 { lcd.create_char(s, &cgram::BLANK); }
    let mut sand = sand::SandGrid::new(sand::rand_particle());
    for r in 0u8..4 { lcd.set_cursor(0, r); lcd.write_raw(&[b' '; 20]); }

    // ── WiFi init (sand is static during this ~1 s blocking call) ────────────
    info!("Connecting WiFi...");
    let sysloop = EspSystemEventLoop::take().unwrap();
    // NVS is required for RF calibration data — without it the radio does a
    // full recalibration every boot, causing a brownout spike and reboot loop.
    let nvs = EspDefaultNvsPartition::take().unwrap();
    // Keep a clone for the price cache; WiFi consumes the original.
    let nvs_cache = nvs.clone();

    // Load persisted screen state before the sand animation starts.
    let mut screen_forced_off = load_screen_forced(&nvs_cache);
    let mut last_btn_display:       bool = true;
    let mut last_debounce_display_ms: u64 = 0;
    if screen_forced_off {
        lcd.write_backlight(false);
        led_green.set_low().unwrap();
        led_red.set_low().unwrap();
    }

    // Force-clean any WiFi state left from a previous soft reset (e.g. OTA restart).
    // stop()+deinit() are no-ops if WiFi was never started; errors are safe to ignore.
    unsafe {
        esp_idf_sys::esp_wifi_stop();
        esp_idf_sys::esp_wifi_deinit();
    }

    let mut wifi = BlockingWifi::wrap(
        EspWifi::new(peripherals.modem, sysloop.clone(), Some(nvs)).unwrap(),
        sysloop,
    )
    .unwrap();

    wifi.set_configuration(&Configuration::Client(ClientConfiguration {
        ssid: config::WIFI_SSID.try_into().unwrap(),
        password: config::WIFI_PASSWORD.try_into().unwrap(),
        ..Default::default()
    }))
    .unwrap();

    for attempt in 1u8..=5 {
        match wifi.start() {
            Ok(_) => break,
            Err(e) => {
                info!("[wifi] start failed (attempt {}): {:?}", attempt, e);
                if attempt == 5 {
                    info!("[wifi] giving up — restarting");
                    unsafe { esp_idf_sys::esp_restart(); }
                }
                FreeRtos::delay_ms(1000);
            }
        }
    }

    // TX power: use ESP-IDF default (~20 dBm). Brownout detector is disabled
    // so there is no reason to cap TX power anymore.

    // Non-blocking connect: poll for association + DHCP with falling sand.
    let _ = unsafe { esp_idf_sys::esp_wifi_connect() }; // first attempt
    let mut wifi_t0 = millis();
    let mut last_retry_ms = wifi_t0;

    'wifi_loop: loop {
        let now = millis();
        let associated = wifi.is_connected().unwrap_or(false);

        // Exit as soon as we have a real IP.
        if associated {
            if let Ok(info) = wifi.wifi().sta_netif().get_ip_info() {
                if info.ip.to_string() != "0.0.0.0" {
                    break 'wifi_loop;
                }
            }
        }

        // Retry connect every 5 s while not associated.
        if !associated && now - last_retry_ms >= 5_000 {
            last_retry_ms = now;
            let _ = unsafe { esp_idf_sys::esp_wifi_connect() };
        }

        // After 30 s with no IP: reset WiFi and sand.
        if now - wifi_t0 >= 30_000 {
            wifi_t0 = now;
            last_retry_ms = now;
            info!("[wifi] 30 s timeout — resetting connection");
            let _ = unsafe { esp_idf_sys::esp_wifi_disconnect() };
            std::thread::sleep(Duration::from_millis(300));
            let _ = unsafe { esp_idf_sys::esp_wifi_connect() };
            sand = sand::SandGrid::new(sand::rand_particle());
            for r in 0u8..4 { lcd.set_cursor(0, r); lcd.write_raw(&[b' '; 20]); }
        }

        // Poll display button during loading so screen-off persists through boot.
        let disp = btn_display.is_high();
        if last_btn_display && !disp {
            let t = millis();
            if t - last_debounce_display_ms >= config::DEBOUNCE_MS {
                last_debounce_display_ms = t;
                screen_forced_off = !screen_forced_off;
                lcd.write_backlight(!screen_forced_off);
                save_screen_forced(&nvs_cache, screen_forced_off);
            }
        }
        last_btn_display = disp;

        if !screen_forced_off {
            for _ in 0..2 { sand.step(); }
            sand.render(&mut lcd, SAND_COL);
        }
    }

    let ip = wifi.wifi().sta_netif().get_ip_info().unwrap().ip;
    info!("WiFi connected: {}", ip);

    // Sand keeps falling for ~1 s after WiFi connects before switching to ticker.
    {
        let ok_start = millis();
        loop {
            if millis() - ok_start >= 1_000 { break; }
            let disp = btn_display.is_high();
            if last_btn_display && !disp {
                let t = millis();
                if t - last_debounce_display_ms >= config::DEBOUNCE_MS {
                    last_debounce_display_ms = t;
                    screen_forced_off = !screen_forced_off;
                    lcd.write_backlight(!screen_forced_off);
                    save_screen_forced(&nvs_cache, screen_forced_off);
                }
            }
            last_btn_display = disp;
            if !screen_forced_off {
                for _ in 0..5 { sand.step(); }
                sand.render(&mut lcd, SAND_COL);
            }
        }
    }

    // NTP — start immediately, no blocking sleep
    info!("Starting NTP sync...");
    // NTP — failure is non-fatal; clock just won't sync.  Don't panic here.
    let _sntp = EspSntp::new_default().ok();

    // OTA update server — listens on TCP :3232.
    // Flash via: flash_net.sh <ESP32_IP>
    ota::spawn_ota_server();

    // Media server — laptop connects here and receives "p\n" on button press.
    spawn_media_server();
    // Mic LED server — any machine connects briefly to send "m:0\n" or "m:1\n".
    spawn_mic_server();

    // Shared state
    let ui_state   = Arc::new(Mutex::new(screen::UiState::default()));
    let lamp_handle = Arc::new(tuya::LampHandle::new());

    {
        let mut st = ui_state.lock().unwrap();
        st.wifi_connected = true;
        st.fetching = true; // show hourglass from first render — cleared when fetch completes
        // Preload last known prices so the ticker shows real data immediately
        // instead of dashes until the first network fetch completes.
        if let Some(cached) = cache::load(&nvs_cache) {
            st.data = cached;
        }
    }

    let mut row_cache = screen::RowCache::new();

    // ── Network fetch thread ─────────────────────────────────────────────────
    // Pressing the screen button sets fetch_trigger so the thread wakes early
    // and starts a fresh cycle with the new screen's priority at the top.
    let fetch_trigger = Arc::new(AtomicBool::new(false));
    let ui_net        = Arc::clone(&ui_state);
    let trigger_net   = Arc::clone(&fetch_trigger);
    let nvs_fetch     = nvs_cache.clone(); // fetch thread gets its own clone
    std::thread::Builder::new()
        .name("finFetch".into())
        .stack_size(20480)
        .spawn(move || {
            let (wa, wb) = api::spawn_fetch_workers();
            loop {
            info!("[net] fetch cycle start");
            let mut data = api::MarketData::default();

            if let Ok(mut st) = ui_net.lock() { st.fetching = true; st.loading_frame = 0; }

            api::fetch_all(&mut data, &wa, &wb);

            cache::save(&nvs_fetch, &data);

            if let Ok(mut st) = ui_net.lock() {
                api::merge(&mut st.data, data);
                st.fetching = false;
                st.fetch_completed_at = millis();
            }

            // Wait for next interval, but exit early if trigger fires.
            let wait_until = millis() + config::FETCH_INTERVAL_MS;
            loop {
                if trigger_net.swap(false, Ordering::Relaxed) { break; }
                let remaining = wait_until.saturating_sub(millis());
                if remaining == 0 { break; }
                std::thread::sleep(Duration::from_millis(remaining.min(100)));
            }
            } // loop
        })
        .unwrap();

    // ── Lamp bridge thread ───────────────────────────────────────────────────
    let lamp_bridge = Arc::clone(&lamp_handle);
    let ui_lamp     = Arc::clone(&ui_state);
    std::thread::Builder::new()
        .name("lampBridge".into())
        .stack_size(16384)
        .spawn(move || {
            let mut last_refresh_ms: u64 = 0;
            loop {
                let toggled = lamp_bridge.poll();

                let now = millis();
                let do_refresh = now - last_refresh_ms >= 5_000;
                if do_refresh {
                    last_refresh_ms = now;
                    lamp_bridge.refresh();
                }

                // Sync ui_state immediately after a toggle or periodic refresh.
                // Always use display_state() so a pending target is never overwritten.
                if toggled || do_refresh {
                    if let Ok(mut st) = ui_lamp.lock() {
                        st.lamp = lamp_bridge.display_state();
                    }
                }

                std::thread::sleep(Duration::from_millis(20));
            }
        })
        .unwrap();

    // ── Watchdog: subscribe main task, trigger panic (→ reset) if loop stalls ──
    unsafe {
        let wdt_cfg = esp_idf_sys::esp_task_wdt_config_t {
            timeout_ms: 30_000,
            idle_core_mask: 0,
            trigger_panic: true,
        };
        let _ = esp_idf_sys::esp_task_wdt_reconfigure(&wdt_cfg);
        let _ = esp_idf_sys::esp_task_wdt_add(core::ptr::null_mut());
    }

    // Release sand's CGRAM slots (writes blanks), then prime CGRAM with ticker
    // glyphs before the first render.  Priming ensures CGRAM holds correct data
    // before DDRAM references any slot, preventing a blank-icon flash on the
    // ticker's first frame.
    sand.release(&mut lcd);
    // Blank DDRAM immediately: sand rows still hold slot indices 0-7 and would
    // briefly show ticker glyphs in wrong positions while prime_cgram writes.
    for r in 0u8..4 { lcd.set_cursor(0, r); lcd.write_raw(&[b' '; 20]); }
    {
        let st = ui_state.lock().unwrap();
        let now = millis();
        ticker::prime_cgram(&mut lcd, &mut row_cache, &st, now);
        ticker::render(&mut lcd, &mut row_cache, &st, now);
    }

    // ── Main loop ─────────────────────────────────────────────────────────────
    // All timers use raw millis() — avoids potential Instant bugs in this target.
    // Initialize timers to now so no interval fires immediately on first iteration.
    let loop_start = millis();

    let mut last_clock_ms:          u64 = loop_start;
    let mut last_auto_screen_ms:    u64 = loop_start;
    let mut last_debounce_screen_ms:u64 = 0;
    let mut last_debounce_light_ms: u64 = 0;
    let mut last_loading_ms:      u64 = loop_start;
    let mut last_lamp_loading_ms: u64 = loop_start;
    let mut last_wifi_check_ms:     u64 = loop_start;
    let mut wifi_down_since_ms:     u64 = 0; // 0 = currently connected
    let mut last_btn_debug_ms:      u64 = 0; // first log fires immediately

    let mut last_vol_read_ms: u64 = 0;
    let mut vol_smoothed: u32 = u32::MAX;

    let mut last_btn_screen = true;
    let mut last_btn_light  = true;
    let mut last_btn_warm   = true;
    let mut last_btn_bright = true;
    let mut last_btn_chart  = true;
    let mut last_btn_media  = true;
    let mut last_debounce_media_ms: u64 = 0;
    // last_btn_display and last_debounce_display_ms declared earlier (used during loading)
    // screen_forced_off declared earlier (loaded from NVS)

    let mut last_debounce_warm_ms:   u64 = 0;
    let mut last_debounce_bright_ms: u64 = 0;
    let mut last_debounce_chart_ms:  u64 = 0;
    let mut chart_active  = false;
    let mut chart_until:  u64 = 0;

    let mut history           = history::PriceHistory::new();
    let mut last_history_fetch: u64 = 0;

    let mut prev_lamp_anim  = false;
    let mut last_backlight  = true;

    loop {
        let now = millis();

        // ── Read state (brief lock) ───────────────────────────────────────────
        let (lamp_anim_active, lamp_known_off, is_fetching, wifi_connected) = {
            let st = ui_state.lock().unwrap();
            let anim     = st.lamp_anim_until > 0 && now < st.lamp_anim_until;
            let lamp_off = st.lamp.known && !st.lamp.on;
            (anim, lamp_off, st.fetching, st.wifi_connected)
        };

        // ── Record price history after each fetch ─────────────────────────────
        {
            let st = ui_state.lock().unwrap();
            if st.fetch_completed_at > last_history_fetch {
                last_history_fetch = st.fetch_completed_at;
                history.push(config::Screen::Btc,    st.data.price_btc);
                history.push(config::Screen::Sol,    st.data.price_sol);
                history.push(config::Screen::Gold,   st.data.price_gold);
                history.push(config::Screen::Oil,    st.data.price_oil);
                history.push(config::Screen::UsdBrl, st.data.price_usd_brl);
            }
        }

        // ── Lamp animation ended → full redraw (skip during chart) ───────────
        if prev_lamp_anim && !lamp_anim_active && !chart_active {
            row_cache.invalidate();
            let st = ui_state.lock().unwrap();
            ticker::render(&mut lcd, &mut row_cache, &st, now);
        }
        prev_lamp_anim = lamp_anim_active;

        // ── Hourglass animations (skip during chart — CGRAM slots occupied) ──
        let mut anim_header_dirty = false;
        if !chart_active {
            if lamp_anim_active && now - last_lamp_loading_ms >= config::LOADING_ANIM_MS {
                last_lamp_loading_ms = now;
                if let Ok(mut st) = ui_state.lock() {
                    st.lamp_loading_frame = st.lamp_loading_frame.wrapping_add(1);
                }
                anim_header_dirty = true;
            }
            if is_fetching && now - last_loading_ms >= config::LOADING_ANIM_MS {
                last_loading_ms = now;
                if let Ok(mut st) = ui_state.lock() {
                    st.loading_frame = st.loading_frame.wrapping_add(1);
                }
                anim_header_dirty = true;
            }
            if anim_header_dirty {
                let st = ui_state.lock().unwrap();
                ticker::paint_header(&mut lcd, &mut row_cache, &st, now);
            }
        }

        // ── Clock update every second (skip during chart) ─────────────────────
        if !chart_active && now - last_clock_ms >= 1000 {
            last_clock_ms = now;
            let st = ui_state.lock().unwrap();
            ticker::paint_header(&mut lcd, &mut row_cache, &st, now);
        }

        // ── Auto screen rotation (skip during chart) ──────────────────────────
        if !chart_active && now - last_auto_screen_ms >= config::AUTO_SCREEN_INTERVAL_MS {
            last_auto_screen_ms = now;
            if let Ok(mut st) = ui_state.lock() { st.screen = st.screen.next(); }
            fetch_trigger.store(true, Ordering::Relaxed);
            row_cache.invalidate();
            let st = ui_state.lock().unwrap();
            ticker::render(&mut lcd, &mut row_cache, &st, now);
        }

        // ── Screen button (GPIO 26, active LOW) ──────────────────────────────
        let btn = btn_screen.is_high();
        if last_btn_screen && !btn {
            if now - last_debounce_screen_ms >= config::DEBOUNCE_MS {
                last_debounce_screen_ms = now;
                last_auto_screen_ms     = now;
                let _ = led_red.set_high(); FreeRtos::delay_ms(80); let _ = led_red.set_low();
                chart_active = false; // pressing screen button exits chart mode
                info!("[btn] screen button pressed");
                if let Ok(mut st) = ui_state.lock() { st.screen = st.screen.next(); }
                fetch_trigger.store(true, Ordering::Relaxed);
                row_cache.invalidate();
                let st = ui_state.lock().unwrap();
                ticker::render(&mut lcd, &mut row_cache, &st, now);
            }
        }
        last_btn_screen = btn;

        // ── Light button (GPIO 12, active LOW, pull-up) ──────────────────────
        let light = btn_light.is_high();
        if last_btn_light && !light {
            if now - last_debounce_light_ms >= config::DEBOUNCE_MS {
                last_debounce_light_ms = now;
                let _ = led_red.set_high(); FreeRtos::delay_ms(80); let _ = led_red.set_low();
                info!("[btn] light button pressed");
                let (new_on, wifi_conn) = {
                    let st = ui_state.lock().unwrap();
                    (lamp_handle.flip_target(st.lamp.on), st.wifi_connected)
                };
                if let Ok(mut st) = ui_state.lock() {
                    st.lamp.on    = new_on;
                    st.lamp.known = true;
                    st.lamp_anim_until    = now + config::LAMP_TOGGLE_ANIM_MS;
                    st.lamp_loading_frame = 0;
                }
                last_lamp_loading_ms = now;
                // Turning lamp ON clears the screen-off override.
                if new_on && screen_forced_off { screen_forced_off = false; save_screen_forced(&nvs_cache, false); }
                last_loading_ms = now;
                row_cache.invalidate();
                let st = ui_state.lock().unwrap();
                ticker::render(&mut lcd, &mut row_cache, &st, now);
            }
        }
        last_btn_light = light;

        // ── Display power button (GPIO 32, active LOW, pull-up) ──────────────
        let disp_btn = btn_display.is_high();
        if last_btn_display && !disp_btn {
            if now - last_debounce_display_ms >= config::DEBOUNCE_MS {
                last_debounce_display_ms = now;
                let _ = led_red.set_high(); FreeRtos::delay_ms(80); let _ = led_red.set_low();
                screen_forced_off = !screen_forced_off;
                save_screen_forced(&nvs_cache, screen_forced_off);
                info!("[btn] display: {}", if screen_forced_off { "off" } else { "on" });
            }
        }
        last_btn_display = disp_btn;

        // ── Warm dim button (GPIO 4, active LOW) ─────────────────────────────
        let warm_btn = btn_warm.is_high();
        if last_btn_warm && !warm_btn {
            if now - last_debounce_warm_ms >= config::DEBOUNCE_MS {
                last_debounce_warm_ms = now;
                let _ = led_red.set_high(); FreeRtos::delay_ms(80); let _ = led_red.set_low();
                info!("[btn] warm dim");
                lamp_handle.queue_warm_dim();
                if screen_forced_off { screen_forced_off = false; save_screen_forced(&nvs_cache, false); }
                if let Ok(mut st) = ui_state.lock() {
                    st.lamp.on    = true;
                    st.lamp.known = true;
                    st.lamp_anim_until    = now + config::LAMP_TOGGLE_ANIM_MS;
                    st.lamp_loading_frame = 0;
                }
                last_lamp_loading_ms = now;
                row_cache.invalidate();
                let st = ui_state.lock().unwrap();
                ticker::render(&mut lcd, &mut row_cache, &st, now);
            }
        }
        last_btn_warm = warm_btn;

        // ── Bright white button (GPIO 5, active LOW) ─────────────────────────
        let bright_btn = btn_bright.is_high();
        if last_btn_bright && !bright_btn {
            if now - last_debounce_bright_ms >= config::DEBOUNCE_MS {
                last_debounce_bright_ms = now;
                let _ = led_red.set_high(); FreeRtos::delay_ms(80); let _ = led_red.set_low();
                info!("[btn] bright white");
                lamp_handle.queue_bright_white();
                if screen_forced_off { screen_forced_off = false; save_screen_forced(&nvs_cache, false); }
                if let Ok(mut st) = ui_state.lock() {
                    st.lamp.on    = true;
                    st.lamp.known = true;
                    st.lamp_anim_until    = now + config::LAMP_TOGGLE_ANIM_MS;
                    st.lamp_loading_frame = 0;
                }
                last_lamp_loading_ms = now;
                row_cache.invalidate();
                let st = ui_state.lock().unwrap();
                ticker::render(&mut lcd, &mut row_cache, &st, now);
            }
        }
        last_btn_bright = bright_btn;

        // ── Chart button (GPIO 16, active LOW) ───────────────────────────────
        let chart_btn = btn_chart.is_high();
        if last_btn_chart && !chart_btn {
            if now - last_debounce_chart_ms >= config::DEBOUNCE_MS {
                last_debounce_chart_ms = now;
                let _ = led_red.set_high(); FreeRtos::delay_ms(80); let _ = led_red.set_low();
                if chart_active {
                    chart_active = false;
                    row_cache.invalidate();
                    let st = ui_state.lock().unwrap();
                    ticker::render(&mut lcd, &mut row_cache, &st, now);
                } else {
                    chart_active = true;
                    chart_until = now + config::CHART_DURATION_MS;
                    let st = ui_state.lock().unwrap();
                    let mut prices = [0f64; 60];
                    let n = history.get(st.screen, &mut prices);
                    row_cache.invalidate();
                    chart::render(&mut lcd, &mut row_cache, &st, &prices[..n]);
                }
            }
        }
        last_btn_chart = chart_btn;

        // ── Media play/pause button (GPIO 19, active LOW) ────────────────────
        let media_btn = btn_media.is_high();
        if last_btn_media && !media_btn {
            if now - last_debounce_media_ms >= config::DEBOUNCE_MS {
                last_debounce_media_ms = now;
                info!("[btn] media play/pause");
                let _ = led_red.set_high();
                FreeRtos::delay_ms(80);
                let _ = led_red.set_low();
                PLAY_PAUSE_READY.store(true, Ordering::Relaxed);
            }
        }
        last_btn_media = media_btn;

        // ── Chart auto-exit after 30 s ────────────────────────────────────────
        if chart_active && now >= chart_until {
            chart_active = false;
            row_cache.invalidate();
            let st = ui_state.lock().unwrap();
            ticker::render(&mut lcd, &mut row_cache, &st, now);
        }

        // ── Volume potentiometer (GPIO 34, ADC1) ─────────────────────────────
        if now - last_vol_read_ms >= 10 {
            last_vol_read_ms = now;
            // Median of 5 rapid samples: rejects single-sample spikes and cuts
            // gaussian noise variance by ~2.2× before anything else touches it.
            let mut s = [0u32; 5];
            for v in s.iter_mut() { *v = vol_pin.read_raw().unwrap_or(0) as u32; }
            s.sort_unstable();
            let raw = s[2];
            // Apply sqrt curve in vol×256 fixed-point. Tiny dead zone avoids singularity.
            let raw_fp: u32 = if raw < 30 {
                0
            } else {
                ((raw as f32 / 4095.0_f32).sqrt() * 153.0 * 256.0) as u32
            };
            if vol_smoothed == u32::MAX { vol_smoothed = raw_fp; }
            // Per-step clamp: even if 3+ of 5 median samples are corrupted, the output
            // can move at most 20 vol-units per 10 ms toward the bad value.
            // Full range (153 units) takes ≥ 77 ms — covers any legitimate fast turn.
            let clamped_fp = raw_fp.clamp(
                vol_smoothed.saturating_sub(20 * 256),
                vol_smoothed + 20 * 256,
            );
            // EMA on the clamped median.
            let error = clamped_fp.abs_diff(vol_smoothed);
            let alpha: u32 = if error <= 2 * 256 {
                32   // at rest — heavy smoothing
            } else if error >= 12 * 256 {
                256  // clearly moving — instant track
            } else {
                32 + (error - 2 * 256) * 224 / (10 * 256)
            };
            vol_smoothed = (vol_smoothed * (256 - alpha) + clamped_fp * alpha) / 256;
            let vol = (vol_smoothed / 256) as u8;
            let prev = VOLUME_PCT.load(Ordering::Relaxed);
            if prev == 255 || (vol as i16 - prev as i16).abs() >= 3 {
                VOLUME_PCT.store(vol, Ordering::Relaxed);
            }
        }

        // ── Backlight + WiFi LEDs — single source of truth ───────────────────
        // Both are driven here every loop iteration. Nothing else sets LEDs.
        let is_day = {
            let utc = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            if utc > 1_600_000_000 {
                let local = utc + config::GMT_OFFSET_SEC as i64;
                let hour = (local.rem_euclid(86400) / 3600) as u8;
                hour >= 6 && hour < 18
            } else {
                true
            }
        };
        let want_backlight = !screen_forced_off && (is_day || !lamp_known_off);
        if want_backlight != last_backlight { last_backlight = want_backlight; lcd.write_backlight(want_backlight); }
        // LEDs always written directly — no dirty tracking to avoid init-state bugs.
        // Coupled to want_backlight so LEDs and backlight are always in sync.
        if want_backlight && wifi_connected { led_green.set_high().unwrap(); led_red.set_low().unwrap(); }
        else if want_backlight             { led_green.set_low().unwrap();  led_red.set_high().unwrap(); }
        else                               { led_green.set_low().unwrap();  led_red.set_low().unwrap(); }
        // Blue LED: mic unmuted = on.
        if MIC_UNMUTED.load(Ordering::Relaxed) { led_blue.set_high().unwrap(); }
        else                                    { led_blue.set_low().unwrap(); }

        // ── WiFi status + auto-reconnect every 15 s ──────────────────────────────
        if now - last_wifi_check_ms >= 15_000 {
            last_wifi_check_ms = now;
            let connected = wifi.is_connected().unwrap_or(false);

            if connected {
                wifi_down_since_ms = 0;
            } else {
                if wifi_down_since_ms == 0 { wifi_down_since_ms = now; }
                let secs_down = now.saturating_sub(wifi_down_since_ms) / 1000;
                if secs_down >= 60 {
                    // Full disconnect before reconnect after 60 s to clear stale state.
                    info!("[wifi] down {}s — full reconnect", secs_down);
                    let _ = unsafe { esp_idf_sys::esp_wifi_disconnect() };
                    std::thread::sleep(Duration::from_millis(300));
                    wifi_down_since_ms = now;
                } else {
                    info!("[wifi] disconnected — reconnect attempt ({}s)", secs_down);
                }
                let _ = unsafe { esp_idf_sys::esp_wifi_connect() };
            }

            if let Ok(mut st) = ui_state.lock() { st.wifi_connected = connected; }
        }

        // ── Button GPIO debug: log raw pin state every 5 s ───────────────────
        // Prints 1=HIGH(released) 0=LOW(pressed). Helps diagnose wiring issues.
        if now - last_btn_debug_ms >= 5000 {
            last_btn_debug_ms = now;
            info!("[gpio] screen(26)={} lamp(12)={} display(32)={} forced_off={}",
                btn_screen.is_high() as u8,
                btn_light.is_high() as u8,
                btn_display.is_high() as u8,
                screen_forced_off as u8);
        }

        unsafe { let _ = esp_idf_sys::esp_task_wdt_reset(); }
        std::thread::sleep(Duration::from_millis(1));
    }
}

static PLAY_PAUSE_READY: AtomicBool = AtomicBool::new(false);
static VOLUME_PCT: AtomicU8 = AtomicU8::new(255); // 255 = not yet read
static MIC_UNMUTED: AtomicBool = AtomicBool::new(false);


fn spawn_media_server() {
    std::thread::Builder::new()
        .name("media-srv".into())
        .stack_size(8192)
        .spawn(|| {
            use std::io::Write;
            use std::net::TcpListener;
            loop {
                let listener = match TcpListener::bind("0.0.0.0:9876") {
                    Ok(l) => l,
                    Err(e) => { warn!("[media-srv] bind err: {e}, retrying"); FreeRtos::delay_ms(2000); continue; }
                };
                info!("[media-srv] listening on :9876");
                for stream in listener.incoming() {
                    match stream {
                        Ok(mut s) => {
                            info!("[media-srv] laptop connected");
                            let mut last_vol: u8 = VOLUME_PCT.load(Ordering::Relaxed);
                            let mut keepalive: u32 = 0;
                            loop {
                                if PLAY_PAUSE_READY.swap(false, Ordering::Relaxed) {
                                    if s.write_all(b"p\n").is_err() { break; }
                                    info!("[media-srv] play/pause sent");
                                }
                                let vol = VOLUME_PCT.load(Ordering::Relaxed);
                                if vol != 255 && vol != last_vol {
                                    let msg = std::format!("v:{}\n", vol);
                                    if s.write_all(msg.as_bytes()).is_err() { break; }
                                    last_vol = vol;
                                }
                                keepalive += 1;
                                if keepalive >= 3000 {
                                    if s.write_all(b"k\n").is_err() { break; }
                                    keepalive = 0;
                                }
                                FreeRtos::delay_ms(10);
                            }
                            info!("[media-srv] laptop disconnected");
                        }
                        Err(e) => { warn!("[media-srv] accept err: {e}"); break; }
                    }
                }
                FreeRtos::delay_ms(1000);
            }
        })
        .ok();
}

fn spawn_mic_server() {
    std::thread::Builder::new()
        .name("mic-srv".into())
        .stack_size(6144)
        .spawn(|| {
            use std::net::TcpListener;
            loop {
                let listener = match TcpListener::bind("0.0.0.0:9877") {
                    Ok(l) => l,
                    Err(e) => { warn!("[mic-srv] bind err: {e}, retrying"); FreeRtos::delay_ms(2000); continue; }
                };
                info!("[mic-srv] listening on :9877");
                let mut err = false;
                for stream in listener.incoming() {
                    match stream {
                        Ok(s) => {
                            let mut line = String::new();
                            if std::io::BufReader::new(s).read_line(&mut line).is_ok() {
                                match line.trim() {
                                    "m:1" => { MIC_UNMUTED.store(true,  Ordering::Relaxed); info!("[mic] unmuted"); }
                                    "m:0" => { MIC_UNMUTED.store(false, Ordering::Relaxed); info!("[mic] muted");   }
                                    _ => {}
                                }
                            }
                        }
                        Err(e) => { warn!("[mic-srv] accept err: {e}"); err = true; break; }
                    }
                }
                if err { FreeRtos::delay_ms(1000); }
            }
        })
        .ok();
}

/// Milliseconds since boot via esp_timer (same source the clock uses).
fn millis() -> u64 {
    (unsafe { esp_idf_sys::esp_timer_get_time() } / 1000) as u64
}

fn load_screen_forced(nvs: &esp_idf_svc::nvs::EspDefaultNvsPartition) -> bool {
    use esp_idf_svc::nvs::EspNvs;
    EspNvs::new(nvs.clone(), "disp", true)
        .ok()
        .and_then(|mut n| n.get_u8("forced_off").ok().flatten())
        .map(|v| v != 0)
        .unwrap_or(false)
}

fn save_screen_forced(nvs: &esp_idf_svc::nvs::EspDefaultNvsPartition, v: bool) {
    use esp_idf_svc::nvs::EspNvs;
    if let Ok(mut n) = EspNvs::new(nvs.clone(), "disp", true) {
        let _ = n.set_u8("forced_off", v as u8);
    }
}
