mod api;
mod cache;
mod cgram;
mod chart;
mod config;
mod fmt;
mod glyphs;
mod history;
mod lcd;
mod led;
mod ota;
mod persist;
mod sand;
mod screen;
mod ticker;
mod tuya;
mod web;

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
use std::time::Duration;

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
    // Captured here (reset reason is only valid to read once, right at boot)
    // and reported to device-events once wifi is up further down - this is
    // the exact diagnostic signal the ESP32 crash/flap investigation was
    // missing (no way to tell "WiFi blip" from "firmware panicked and
    // rebooted" over HTTP). See where `boot_reason`/`boot_severity` are used
    // near `st.wifi_connected = true`.
    let (boot_reason, boot_severity): (&'static str, &'static str) =
        match unsafe { esp_idf_sys::esp_reset_reason() } {
            r if r == esp_idf_sys::esp_reset_reason_t_ESP_RST_POWERON => {
                info!("[boot] cold start — settling 1 s");
                FreeRtos::delay_ms(1000);
                ("cold_start", "info")
            }
            r if r == esp_idf_sys::esp_reset_reason_t_ESP_RST_BROWNOUT => {
                info!("[boot] brownout reset — settling 3 s");
                FreeRtos::delay_ms(3000);
                ("brownout", "critical")
            }
            r if r == esp_idf_sys::esp_reset_reason_t_ESP_RST_PANIC => {
                info!("[boot] reset reason: PANIC (stack overflow or abort)");
                ("panic", "critical")
            }
            r if r == esp_idf_sys::esp_reset_reason_t_ESP_RST_INT_WDT => {
                info!("[boot] reset reason: INT WATCHDOG");
                ("int_watchdog", "error")
            }
            r if r == esp_idf_sys::esp_reset_reason_t_ESP_RST_TASK_WDT => {
                info!("[boot] reset reason: TASK WATCHDOG");
                ("task_watchdog", "error")
            }
            r if r == esp_idf_sys::esp_reset_reason_t_ESP_RST_WDT => {
                info!("[boot] reset reason: OTHER WATCHDOG");
                ("other_watchdog", "error")
            }
            r if r == esp_idf_sys::esp_reset_reason_t_ESP_RST_SW => {
                info!("[boot] reset reason: software reset (OTA or esp_restart)");
                ("software_reset", "info")
            }
            r => {
                info!("[boot] reset reason: unknown ({})", r);
                ("unknown", "warning")
            }
        };

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
    let persist = persist::Persist::new(nvs_cache.clone());

    // Load persisted state before the sand animation starts.
    let screen_forced_off = Arc::new(AtomicBool::new(persist.load_screen_forced()));
    let initial_pot_enabled   = persist.load_pot_enabled();
    POT_ENABLED.store(initial_pot_enabled, Ordering::Relaxed);
    let web_triggers = Arc::new(web::WebTriggers::new());
    let mut last_btn_display:       bool = true;
    let mut last_debounce_display_ms: u64 = 0;
    if screen_forced_off.load(Ordering::Relaxed) {
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
                let sfo = !screen_forced_off.load(Ordering::Relaxed);
                screen_forced_off.store(sfo, Ordering::Relaxed);
                lcd.write_backlight(!sfo);
                persist.save_screen_forced(sfo);
            }
        }
        last_btn_display = disp;

        if !screen_forced_off.load(Ordering::Relaxed) {
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
                    let sfo = !screen_forced_off.load(Ordering::Relaxed);
                    screen_forced_off.store(sfo, Ordering::Relaxed);
                    lcd.write_backlight(!sfo);
                    persist.save_screen_forced(sfo);
                }
            }
            last_btn_display = disp;
            if !screen_forced_off.load(Ordering::Relaxed) {
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

    // Shared state
    let ui_state   = Arc::new(Mutex::new(screen::UiState::default()));
    let lamp_handle = Arc::new(tuya::LampHandle::new());
    let led_state   = Arc::new(led::LedState::new());

    // Set initial LED state now that wifi is connected.
    led_state.on_wifi_connect(!screen_forced_off.load(Ordering::Relaxed));

    // Mic/lamp server — accepts "m:0", "m:1" (mic LED), "l:t" (lamp toggle).
    spawn_mic_server(Arc::clone(&lamp_handle), Arc::clone(&ui_state), Arc::clone(&led_state));

    let auto_rotate = Arc::new(AtomicBool::new(true));

    // Web control server — browse to http://<ESP_IP>
    if config::WEB_SERVER_ENABLED {
        web::spawn(Arc::clone(&web_triggers), Arc::clone(&ui_state), Arc::clone(&screen_forced_off), Arc::clone(&lamp_handle), Arc::clone(&auto_rotate), Arc::clone(&led_state));
    }

    // Report boot/reset reason now that wifi is actually up and an HTTP
    // request can succeed - fire-and-forget, never blocks this thread.
    api::report_event("boot", boot_severity, format!("device booted, reset reason: {boot_reason}"));

    {
        let mut st = ui_state.lock().unwrap();
        st.wifi_connected = true;
        st.fetching = true; // show hourglass from first render — cleared when fetch completes
        st.pot_enabled = initial_pot_enabled;
        if let Some(screen) = persist.load_screen() {
            st.screen = screen;
        }
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
    let mut last_lcd_reinit_ms:     u64 = loop_start;
    let mut last_debounce_screen_ms:u64 = 0;
    let mut last_debounce_light_ms: u64 = 0;
    let mut last_loading_ms:      u64 = loop_start;
    let mut last_lamp_loading_ms: u64 = loop_start;
    let mut last_wifi_check_ms:     u64 = loop_start;
    let mut wifi_down_since_ms:     u64 = 0; // 0 = currently connected
    let mut last_btn_debug_ms:      u64 = 0; // first log fires immediately

    let mut last_vol_read_ms: u64 = 0;
    let mut vol_smoothed: u32 = u32::MAX;
    let mut vol_move_count: u32 = 0;

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

    let mut prev_lamp_anim    = false;
    let mut last_backlight    = true;

    // Track last value written to each LED pin so hardware is only touched on change.
    let mut last_hw_green = false;
    let mut last_hw_red   = true;  // matches boot: red=high until wifi connects
    let mut last_hw_blue  = false;

    loop {
        let now = millis();

        // ── Read state (brief lock) ───────────────────────────────────────────
        let (lamp_anim_active, is_fetching, wifi_connected) = {
            let st = ui_state.lock().unwrap();
            let anim = st.lamp_anim_until > 0 && now < st.lamp_anim_until;
            (anim, st.fetching, st.wifi_connected)
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

        // ── Periodic LCD re-init — combats contrast drift from thermal effects ──
        if last_lcd_reinit_ms > 0 && now - last_lcd_reinit_ms >= 30 * 60 * 1000 {
            last_lcd_reinit_ms = now;
            lcd.init();
            let st = ui_state.lock().unwrap();
            ticker::prime_cgram(&mut lcd, &mut row_cache, &st, now);
            row_cache.invalidate();
            ticker::render(&mut lcd, &mut row_cache, &st, now);
        }

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

        // ── Web volume trigger ────────────────────────────────────────────────
        let web_vol_raw = if config::WEB_SERVER_ENABLED { web_triggers.volume.swap(-1, Ordering::Relaxed) } else { -1i8 };
        if web_vol_raw >= 0 {
            // slider sends 0-100; map to pot's 0-153 scale
            let vol = (web_vol_raw as u32 * 153 / 100) as u8;
            VOLUME_PCT.store(vol, Ordering::Relaxed);
        }

        // ── Clock update every second (skip during chart) ─────────────────────
        if !chart_active && now - last_clock_ms >= 1000 {
            last_clock_ms = now;
            let mut st = ui_state.lock().unwrap();
            st.pot_enabled = POT_ENABLED.load(Ordering::Relaxed);
            st.volume_pct  = VOLUME_PCT.load(Ordering::Relaxed);
            ticker::paint_header(&mut lcd, &mut row_cache, &st, now);
        }

        // ── Auto screen rotation (skip during chart or when disabled by web) ────
        if !chart_active && auto_rotate.load(Ordering::Relaxed) && now - last_auto_screen_ms >= config::AUTO_SCREEN_INTERVAL_MS {
            last_auto_screen_ms = now;
            if let Ok(mut st) = ui_state.lock() { st.screen = st.screen.next(); }
            fetch_trigger.store(true, Ordering::Relaxed);
            row_cache.invalidate();
            let st = ui_state.lock().unwrap();
            ticker::render(&mut lcd, &mut row_cache, &st, now);
        }

        // ── Screen button (GPIO 26, active LOW) ──────────────────────────────
        let btn = btn_screen.is_high();
        let phys_screen = last_btn_screen && !btn && now - last_debounce_screen_ms >= config::DEBOUNCE_MS;
        let web_screen  = config::WEB_SERVER_ENABLED && web_triggers.screen.swap(false, Ordering::Relaxed);
        let web_select_raw = if config::WEB_SERVER_ENABLED { web_triggers.screen_select.swap(-1, Ordering::Relaxed) } else { -1i8 };
        let web_select = web_select_raw >= 0;
        if phys_screen || web_screen || web_select {
            last_debounce_screen_ms = now;
            last_auto_screen_ms     = now;
            let _ = led_red.set_high(); FreeRtos::delay_ms(80); let _ = led_red.set_low(); led_state.set_red(false); last_hw_red = false;
            chart_active = false;
            if web_select {
                if let Some(s) = config::Screen::from_u8(web_select_raw as u8) {
                    info!("[btn] screen select web -> {:?}", s);
                    if let Ok(mut st) = ui_state.lock() {
                        st.screen = s;
                        persist.save_screen(st.screen);
                    }
                }
            } else {
                info!("[btn] screen {}", if web_screen { "web" } else { "physical" });
                if let Ok(mut st) = ui_state.lock() {
                    st.screen = st.screen.next();
                    persist.save_screen(st.screen);
                }
            }
            fetch_trigger.store(true, Ordering::Relaxed);
            row_cache.invalidate();
            let st = ui_state.lock().unwrap();
            ticker::render(&mut lcd, &mut row_cache, &st, now);
        }
        last_btn_screen = btn;

        // ── Light button (GPIO 12, active LOW, pull-up) ──────────────────────
        let light = btn_light.is_high();
        let phys_lamp = last_btn_light && !light && now - last_debounce_light_ms >= config::DEBOUNCE_MS;
        if phys_lamp {
            last_debounce_light_ms = now;
            let _ = led_red.set_high(); FreeRtos::delay_ms(80); let _ = led_red.set_low(); led_state.set_red(false); last_hw_red = false;
            info!("[btn] lamp physical");
            let new_on = {
                let st = ui_state.lock().unwrap();
                lamp_handle.flip_target(st.lamp.on)
            };
            if let Ok(mut st) = ui_state.lock() {
                st.lamp.on    = new_on;
                st.lamp.known = true;
                st.lamp_anim_until    = now + config::LAMP_TOGGLE_ANIM_MS;
                st.lamp_loading_frame = 0;
            }
            last_lamp_loading_ms = now;
            last_loading_ms = now;
            row_cache.invalidate();
            let st = ui_state.lock().unwrap();
            ticker::render(&mut lcd, &mut row_cache, &st, now);
        }
        last_btn_light = light;

        // ── Display power button (GPIO 32, active LOW, pull-up) ──────────────
        let disp_btn = btn_display.is_high();
        let phys_display = last_btn_display && !disp_btn && now - last_debounce_display_ms >= config::DEBOUNCE_MS;
        let web_display_raw = if config::WEB_SERVER_ENABLED { web_triggers.display.swap(-1, Ordering::Relaxed) } else { -1i8 };
        let web_display = web_display_raw >= 0;
        if phys_display || web_display {
            last_debounce_display_ms = now;
            let _ = led_red.set_high(); FreeRtos::delay_ms(80); let _ = led_red.set_low(); led_state.set_red(false); last_hw_red = false;
            // DISABLED (config::DISPLAY_TOGGLE_ENABLED) - screen (LCD) is
            // physically disconnected, so toggling "is the screen forced
            // off" has no real effect. Left in place, not deleted, in case
            // the screen is ever reconnected - the button (and the web
            // toggle) still runs the red-flash/clear above either way.
            if config::DISPLAY_TOGGLE_ENABLED {
                let sfo = if web_display { web_display_raw == 0 } else { !screen_forced_off.load(Ordering::Relaxed) };
                screen_forced_off.store(sfo, Ordering::Relaxed);
                persist.save_screen_forced(sfo);
                if sfo { led_state.on_screen_off(); } else { led_state.on_screen_on(wifi_connected); }
                info!("[btn] display {} ({})", if sfo { "off" } else { "on" }, if web_display { "web" } else { "physical" });
            }
        }
        last_btn_display = disp_btn;

        // ── Warm dim button (GPIO 4, active LOW) ─────────────────────────────
        let warm_btn = btn_warm.is_high();
        let phys_warm = last_btn_warm && !warm_btn && now - last_debounce_warm_ms >= config::DEBOUNCE_MS;
        if phys_warm {
            last_debounce_warm_ms = now;
            let _ = led_red.set_high(); FreeRtos::delay_ms(80); let _ = led_red.set_low(); led_state.set_red(false); last_hw_red = false;
            info!("[btn] warm dim (physical)");
            lamp_handle.queue_warm_dim();
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
        last_btn_warm = warm_btn;

        // ── Bright white button (GPIO 5, active LOW) ─────────────────────────
        let bright_btn = btn_bright.is_high();
        let phys_bright = last_btn_bright && !bright_btn && now - last_debounce_bright_ms >= config::DEBOUNCE_MS;
        if phys_bright {
            last_debounce_bright_ms = now;
            let _ = led_red.set_high(); FreeRtos::delay_ms(80); let _ = led_red.set_low(); led_state.set_red(false); last_hw_red = false;
            info!("[btn] bright white (physical)");
            lamp_handle.queue_bright_white();
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
        last_btn_bright = bright_btn;

        // ── Pot toggle button (GPIO 18, active LOW) — chart code preserved below ──
        let chart_btn = btn_chart.is_high();
        let phys_pot = last_btn_chart && !chart_btn && now - last_debounce_chart_ms >= config::DEBOUNCE_MS;
        let web_pot_raw = if config::WEB_SERVER_ENABLED { web_triggers.pot.swap(-1, Ordering::Relaxed) } else { -1i8 };
        let web_pot = web_pot_raw >= 0;
        if phys_pot || web_pot {
            last_debounce_chart_ms = now;
            let _ = led_red.set_high(); FreeRtos::delay_ms(80); let _ = led_red.set_low(); led_state.set_red(false); last_hw_red = false;
            // DISABLED (config::POT_TOGGLE_ENABLED) - the potentiometer
            // itself is unplugged (too noisy, replaced by the new
            // keyboard's volume scroll wheel), so there's nothing left to
            // enable/disable here. Left in place, not deleted, in case the
            // pot is ever reconnected - the button (and the web toggle)
            // still runs the red-flash/clear above either way.
            if config::POT_TOGGLE_ENABLED {
                let enabled = if web_pot { web_pot_raw == 1 } else { !POT_ENABLED.load(Ordering::Relaxed) };
                POT_ENABLED.store(enabled, Ordering::Relaxed);
                persist.save_pot_enabled(enabled);
                let mut st = ui_state.lock().unwrap();
                st.pot_enabled = enabled;
                info!("[btn] pot {} ({})", if enabled { "on" } else { "off" }, if web_pot { "web" } else { "physical" });
                ticker::paint_header(&mut lcd, &mut row_cache, &st, now);
            }
        }
        last_btn_chart = chart_btn;

        // ── Media play/pause button (GPIO 19, active LOW) ────────────────────
        let media_btn = btn_media.is_high();
        let phys_media = last_btn_media && !media_btn && now - last_debounce_media_ms >= config::DEBOUNCE_MS;
        let web_media  = config::WEB_SERVER_ENABLED && web_triggers.media.swap(false, Ordering::Relaxed);
        if phys_media || web_media {
            last_debounce_media_ms = now;
            info!("[btn] media play/pause ({})", if web_media { "web" } else { "physical" });
            let _ = led_red.set_high(); FreeRtos::delay_ms(80); let _ = led_red.set_low(); led_state.set_red(false); last_hw_red = false;
            PLAY_PAUSE_READY.store(true, Ordering::Relaxed);
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
            // 15-sample trimmed mean of middle 7: rejects extreme ADC outliers.
            let mut s = [0u32; 15];
            for v in s.iter_mut() { *v = vol_pin.read_raw().unwrap_or(0) as u32; }
            s.sort_unstable();
            let raw = (s[4] + s[5] + s[6] + s[7] + s[8] + s[9] + s[10]) / 7;
// Remap pot's actual ADC range to [0, 4095], then sqrt curve → 0-100 output.
            let raw_cal = raw.clamp(config::POT_ADC_MIN, config::POT_ADC_MAX)
                .saturating_sub(config::POT_ADC_MIN) * 4095
                / (config::POT_ADC_MAX - config::POT_ADC_MIN);
            let raw_fp: u32 = if raw_cal < 10 {
                0
            } else {
                ((raw_cal as f32 / 4095.0_f32).sqrt() * 153.0 * 256.0) as u32
            };
            if vol_smoothed == u32::MAX { vol_smoothed = raw_fp; }
            let deviation = raw_fp.abs_diff(vol_smoothed);
            // Freeze-and-track: output is completely frozen when pot is still.
            // Only 2+ consecutive readings outside the freeze zone trigger tracking,
            // making single-sample transient spikes invisible to the output.
            if deviation >= 5 * 256 {
                vol_move_count = vol_move_count.saturating_add(1);
            } else {
                vol_move_count = 0;
            }
            if vol_move_count >= 2 {
                let alpha: u32 = if deviation >= 15 * 256 {
                    200
                } else {
                    80 + (deviation - 5 * 256) * 120 / (10 * 256)
                };
                vol_smoothed = (vol_smoothed * (256 - alpha) + raw_fp * alpha) / 256;
            }
            let vol = (vol_smoothed / 256) as u8;
            // config::POT_TOGGLE_ENABLED is the real, permanent gate now -
            // the pot is physically disconnected, so this block still reads
            // the (floating) ADC pin above for diagnostics, but must never
            // let that reading reach VOLUME_PCT. POT_ENABLED alone was NOT
            // sufficient: it defaults to `true` and, worse, gets restored
            // from NVS-persisted state at boot (whatever was saved back
            // when the pot was still connected) - with the toggle button
            // itself disabled, nothing could ever set it back to `false`
            // again. Confirmed live: this let real ADC noise off a
            // disconnected pin silently overwrite the real volume,
            // surfacing as "the volume randomly drops" with no user action.
            if config::POT_TOGGLE_ENABLED && POT_ENABLED.load(Ordering::Relaxed) {
                let prev = VOLUME_PCT.load(Ordering::Relaxed);
                if prev == 255 || (vol as i16 - prev as i16).abs() >= 5 {
                    VOLUME_PCT.store(vol, Ordering::Relaxed);
                }
            }
        }

        // ── Backlight — driven by display toggle only ─────────────────────────
        let want_backlight = !screen_forced_off.load(Ordering::Relaxed);
        if want_backlight != last_backlight { last_backlight = want_backlight; lcd.write_backlight(want_backlight); }
        // ── LEDs: apply LedState → hardware when changed ─────────────────────
        let g = led_state.green.load(Ordering::Relaxed);
        let r = led_state.red  .load(Ordering::Relaxed);
        let b = led_state.blue .load(Ordering::Relaxed);
        if g != last_hw_green { last_hw_green = g; if g { led_green.set_high().unwrap(); } else { led_green.set_low().unwrap(); } }
        if r != last_hw_red   { last_hw_red   = r; if r { led_red  .set_high().unwrap(); } else { led_red  .set_low().unwrap(); } }
        if b != last_hw_blue  { last_hw_blue  = b; if b { led_blue .set_high().unwrap(); } else { led_blue .set_low().unwrap(); } }

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

            if connected != wifi_connected {
                let screen_on = !screen_forced_off.load(Ordering::Relaxed);
                if connected {
                    led_state.on_wifi_connect(screen_on);
                    api::report_event("wifi_recovered", "info", "wifi reconnected".to_string());
                } else {
                    led_state.on_wifi_disconnect(screen_on);
                    api::report_event("wifi_lost", "warning", "wifi disconnected".to_string());
                }
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
                screen_forced_off.load(Ordering::Relaxed) as u8);
        }

        unsafe { let _ = esp_idf_sys::esp_task_wdt_reset(); }
        std::thread::sleep(Duration::from_millis(1));
    }
}

static PLAY_PAUSE_READY: AtomicBool = AtomicBool::new(false);
static VOLUME_PCT: AtomicU8 = AtomicU8::new(255); // 255 = not yet read
static POT_ENABLED: AtomicBool = AtomicBool::new(true);
// Which machine's mic last toggled - set by spawn_mic_server, read by every
// media connection to decide whether IT is the one that should act on
// PLAY_PAUSE_READY. None until the first mic message ever arrives.
static CURRENT_OWNER: Mutex<Option<String>> = Mutex::new(None);

// Explicit per-machine targets, set from the web UI's per-machine controls
// (see web.rs's /action/media/target and /action/volume/target) - these
// bypass CURRENT_OWNER entirely, the whole point being to control either
// machine from the web regardless of which one last toggled its mic. Plain
// Vec<(String, _)> rather than a HashMap: only ever 2-3 real entries, and
// this avoids pulling in a hasher/extra crate for something this small.
static MEDIA_TARGETS: Mutex<Vec<(String, bool)>> = Mutex::new(Vec::new());
static VOLUME_TARGETS: Mutex<Vec<(String, u8)>> = Mutex::new(Vec::new());

/// Called from web.rs's POST /action/media/target?machine=<id> handler.
pub fn queue_media_for_machine(machine: &str) {
    let mut targets = MEDIA_TARGETS.lock().unwrap();
    match targets.iter_mut().find(|(m, _)| m == machine) {
        Some(entry) => entry.1 = true,
        None => targets.push((machine.to_string(), true)),
    }
}

fn take_media_target(machine: &str) -> bool {
    let mut targets = MEDIA_TARGETS.lock().unwrap();
    match targets.iter_mut().find(|(m, _)| m == machine) {
        Some(entry) => std::mem::replace(&mut entry.1, false),
        None => false,
    }
}

/// Called from web.rs's POST /action/volume/target?machine=<id>&v=<N> handler.
/// `vol` is already in the same 0-153 internal range VOLUME_PCT uses.
pub fn queue_volume_for_machine(machine: &str, vol: u8) {
    let mut targets = VOLUME_TARGETS.lock().unwrap();
    match targets.iter_mut().find(|(m, _)| m == machine) {
        Some(entry) => entry.1 = vol,
        None => targets.push((machine.to_string(), vol)),
    }
}

fn peek_volume_target(machine: &str) -> Option<u8> {
    VOLUME_TARGETS.lock().unwrap().iter().find(|(m, _)| m == machine).map(|(_, v)| *v)
}

fn spawn_media_server() {
    std::thread::Builder::new()
        .name("media-srv".into())
        .stack_size(8192)
        .spawn(|| {
            use std::net::TcpListener;
            loop {
                let listener = match TcpListener::bind("0.0.0.0:9876") {
                    Ok(l) => l,
                    Err(e) => { warn!("[media-srv] bind err: {e}, retrying"); FreeRtos::delay_ms(2000); continue; }
                };
                info!("[media-srv] listening on :9876");
                // One thread per accepted connection - up to one per real
                // machine (desktop, laptop) can now stay connected at the
                // same time, instead of the old design where a second
                // machine's connection just sat unaccepted in the OS
                // backlog until the first one dropped.
                for stream in listener.incoming() {
                    match stream {
                        Ok(s) => {
                            std::thread::Builder::new()
                                .name("media-conn".into())
                                .stack_size(8192)
                                .spawn(move || handle_media_connection(s))
                                .ok();
                        }
                        Err(e) => { warn!("[media-srv] accept err: {e}"); break; }
                    }
                }
                FreeRtos::delay_ms(1000);
            }
        })
        .ok();
}

/// One connected machine's send loop. The client's first line must be
/// "id:<machine>\n" (mic_key_daemon.py/play_pause_server.py send this
/// immediately on connect) - everything after that is unchanged from the
/// old protocol. play/pause is only ever written to whichever connection's
/// machine id currently matches CURRENT_OWNER (last machine to toggle its
/// mic - see spawn_mic_server), so pressing the physical media button
/// always goes to whichever computer you were just actively using, not
/// whichever happened to connect first. Volume and the keepalive stay
/// broadcast to every connected machine, same as before - only play/pause
/// routing is owner-gated.
fn handle_media_connection(mut s: std::net::TcpStream) {
    use std::io::{Read, Write};
    // Reads the "id:<machine>\n" handshake byte-by-byte directly off the one
    // stream handle, deliberately NOT via try_clone()+BufReader - try_clone
    // is a dup() under the hood, and esp-idf-svc's std::net support for it on
    // this target turned out to be unreliable in practice (every connection
    // was reset within milliseconds, right at this point, until switched to
    // this single-handle read). A byte-at-a-time read is fine here: the
    // handshake line is short (a hostname) and sent once, immediately, never
    // a hot path.
    let mut id_line = Vec::new();
    loop {
        let mut byte = [0u8; 1];
        match s.read(&mut byte) {
            Ok(0) => { warn!("[media-srv] connection closed before identifying"); return; }
            Ok(_) => {
                if byte[0] == b'\n' { break; }
                id_line.push(byte[0]);
                if id_line.len() > 128 {
                    warn!("[media-srv] id: handshake too long, dropping connection");
                    return;
                }
            }
            Err(e) => { warn!("[media-srv] read err before identifying: {e}"); return; }
        }
    }
    let id_line = String::from_utf8_lossy(&id_line).trim().to_string();
    let machine_id = match id_line.strip_prefix("id:") {
        Some(id) if !id.is_empty() => id.to_string(),
        _ => { warn!("[media-srv] first line wasn't a valid id: handshake: {id_line:?}"); return; }
    };
    info!("[media-srv] {machine_id} connected");
    let mut last_vol: u8 = VOLUME_PCT.load(Ordering::Relaxed);
    let mut last_targeted_vol: Option<u8> = None;
    let mut keepalive: u32 = 0;
    loop {
        let is_owner = CURRENT_OWNER.lock()
            .map(|o| o.as_deref() == Some(machine_id.as_str()))
            .unwrap_or(false);
        if is_owner && PLAY_PAUSE_READY.swap(false, Ordering::Relaxed) {
            if s.write_all(b"p\n").is_err() { break; }
            info!("[media-srv] play/pause sent -> {machine_id} (mic-owner)");
        }
        // Explicit per-machine web target - independent of ownership, so
        // either machine can be controlled directly regardless of which
        // one last toggled its mic.
        if take_media_target(&machine_id) {
            if s.write_all(b"p\n").is_err() { break; }
            info!("[media-srv] play/pause sent -> {machine_id} (web target)");
        }
        let vol = VOLUME_PCT.load(Ordering::Relaxed);
        if vol != 255 && vol != last_vol {
            // VOLUME_PCT is internal 0-153 (pot's sqrt-curve range) - the wire
            // protocol and play_pause_server.py's `pactl set-sink-volume {v}%`
            // both expect a plain 0-100 percentage, so rescale before sending.
            // Sending the raw 0-153 value here was a real bug (e.g. a 100%
            // slider became "v:153" -> 153% volume client-side).
            let pct = (vol as u32 * 100 / 153).min(100) as u8;
            let msg = std::format!("v:{}\n", pct);
            if s.write_all(msg.as_bytes()).is_err() { break; }
            last_vol = vol;
        }
        if let Some(targeted_vol) = peek_volume_target(&machine_id) {
            if last_targeted_vol != Some(targeted_vol) {
                let pct = (targeted_vol as u32 * 100 / 153).min(100) as u8;
                let msg = std::format!("v:{}\n", pct);
                if s.write_all(msg.as_bytes()).is_err() { break; }
                last_targeted_vol = Some(targeted_vol);
            }
        }
        keepalive += 1;
        if keepalive >= 3000 {
            if s.write_all(b"k\n").is_err() { break; }
            keepalive = 0;
        }
        FreeRtos::delay_ms(10);
    }
    info!("[media-srv] {machine_id} disconnected");
}

fn spawn_mic_server(lamp_handle: Arc<tuya::LampHandle>, ui_state: Arc<Mutex<screen::UiState>>, led_state: Arc<led::LedState>) {
    std::thread::Builder::new()
        .name("mic-srv".into())
        .stack_size(4096)
        .spawn(move || {
            use std::net::TcpListener;
            loop {
                let listener = match TcpListener::bind("0.0.0.0:9877") {
                    Ok(l) => l,
                    Err(e) => { warn!("[mic-srv] bind err: {e}, retrying"); FreeRtos::delay_ms(2000); continue; }
                };
                info!("[mic-srv] listening on :9877");
                // One thread per accepted connection, each looping over
                // multiple lines - was previously one-shot (mic_key_daemon.py
                // dialed a brand new TCP connection for every single mic
                // toggle). That meant paying a full TCP handshake round-trip
                // on real wifi for every press (measured ~120ms end-to-end)
                // for what should feel instant. Now the client keeps one
                // connection open and this thread just keeps reading lines
                // off it. Receive-only (no reply ever written back), so no
                // try_clone() needed here unlike the media server.
                for stream in listener.incoming() {
                    match stream {
                        Ok(s) => {
                            let lamp_handle = Arc::clone(&lamp_handle);
                            let ui_state = Arc::clone(&ui_state);
                            let led_state = Arc::clone(&led_state);
                            std::thread::Builder::new()
                                .name("mic-conn".into())
                                .stack_size(6144)
                                .spawn(move || handle_mic_connection(s, lamp_handle, ui_state, led_state))
                                .ok();
                        }
                        Err(e) => { warn!("[mic-srv] accept err: {e}"); FreeRtos::delay_ms(1000); }
                    }
                }
            }
        })
        .ok();
}

fn handle_mic_connection(
    stream: std::net::TcpStream,
    lamp_handle: Arc<tuya::LampHandle>,
    ui_state: Arc<Mutex<screen::UiState>>,
    led_state: Arc<led::LedState>,
) {
    let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_else(|_| "?".into());
    info!("[mic-srv] {peer} connected");
    let reader = std::io::BufReader::new(stream);
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => { warn!("[mic-srv] {peer} read err: {e}"); break; }
        };
        let parts: Vec<&str> = line.trim().splitn(3, ':').collect();
        match parts.as_slice() {
            // Tagged: "m:<0|1>:<machine>" - mic_key_daemon.py always
            // sends this shape now, tagging which machine toggled so
            // the media server knows who to route play/pause to.
            ["m", state, machine] => {
                let unmuted = *state == "1";
                led_state.set_blue(unmuted);
                info!("[mic] {} ({machine})", if unmuted { "unmuted" } else { "muted" });
                if let Ok(mut owner) = CURRENT_OWNER.lock() {
                    *owner = Some(machine.to_string());
                }
            }
            // Defensive fallback for an untagged/old-format sender -
            // still reflects mic state, just can't claim ownership.
            ["m", state] => {
                let unmuted = *state == "1";
                led_state.set_blue(unmuted);
                info!("[mic] {} (no machine id)", if unmuted { "unmuted" } else { "muted" });
            }
            ["l", "t"] => {
                let new_on = {
                    let st = ui_state.lock().unwrap();
                    lamp_handle.flip_target(st.lamp.on)
                };
                if let Ok(mut st) = ui_state.lock() {
                    st.lamp.on    = new_on;
                    st.lamp.known = true;
                }
                info!("[lamp] toggled via laptop -> {}", if new_on { "on" } else { "off" });
            }
            _ => {}
        }
    }
    info!("[mic-srv] {peer} disconnected");
}

/// Milliseconds since boot via esp_timer (same source the clock uses).
fn millis() -> u64 {
    (unsafe { esp_idf_sys::esp_timer_get_time() } / 1000) as u64
}

