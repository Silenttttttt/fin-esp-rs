mod api;
mod cache;
mod cgram;
mod chart;
mod config;
mod dht22;
mod exprotocol_task;
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
use log::{error, info, warn};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// LCD column where the 5-wide sand canvas starts (centered: (20-5)/2 = 7).
const SAND_COL: u8 = 7;

/// Diagnostic: logs only when `main_task`'s real stack high-water-mark sets a NEW all-time
/// low, tagged by where in the render loop it was checked -- see the comment at
/// `min_hwm_seen`'s declaration in `main()` for why this exists.
fn check_stack_watermark(tag: &str, min_hwm_seen: &mut u32) {
    let hwm = unsafe { esp_idf_sys::uxTaskGetStackHighWaterMark(std::ptr::null_mut()) };
    if hwm < *min_hwm_seen {
        *min_hwm_seen = hwm;
        info!("[stack] NEW all-time low at '{tag}': {hwm} bytes free");
    }
}

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
    // I2C moved off GPIO14/27 (2026-08-29) onto GPIO22/23 instead, to free 14/27 for
    // btn_bright/btn_media below - the LCD is still physically disconnected either way, so
    // which exact pins its otherwise-unused I2C bus claims doesn't matter functionally. Kept
    // fully intact and enabled (not deleted, not feature-flagged off).
    let i2c_config = I2cConfig::new().baudrate(200u32.kHz().into());
    let i2c_driver = I2cDriver::new(
        peripherals.i2c0,
        peripherals.pins.gpio22,
        peripherals.pins.gpio23,
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
    // Removed 2026-08-30: was the (never physically wired) Display power button, on GPIO32.
    // Freed for the DHT22 sensor's data line instead - see dht22.rs. The web-triggered
    // `/action/display/*` path (unrelated to this physical pin) still works below.
    let btn_warm    = PinDriver::input(peripherals.pins.gpio13, Pull::Up).unwrap();
    // Moved 2026-08-29 from gpio4/gpio19 (opposite side of the board from the other buttons)
    // to gpio14/gpio27 (freed from the LCD's I2C bus, which moved to gpio22/gpio23 instead) -
    // same side as btn_light/btn_screen/btn_warm now, for a cleaner physical wiring layout.
    let btn_bright  = PinDriver::input(peripherals.pins.gpio14, Pull::Up).unwrap();
    let btn_chart   = PinDriver::input(peripherals.pins.gpio18, Pull::Up).unwrap();
    let btn_media   = PinDriver::input(peripherals.pins.gpio27, Pull::Up).unwrap();
    // Moved 2026-08-30 from GPIO25 to GPIO19 (freed 2026-08-29 when MEDIA_BUTTON_PIN moved to 27).
    let mut led_green = esp_idf_hal::gpio::PinDriver::output(peripherals.pins.gpio19).unwrap();
    let mut led_red   = esp_idf_hal::gpio::PinDriver::output(peripherals.pins.gpio33).unwrap();
    let mut led_yellow  = esp_idf_hal::gpio::PinDriver::output(peripherals.pins.gpio5).unwrap();

    let adc = AdcDriver::new(peripherals.adc1).unwrap();
    let mut vol_pin = AdcChannelDriver::new(
        &adc,
        peripherals.pins.gpio34,
        &AdcChannelConfig { attenuation: attenuation::DB_11, ..Default::default() },
    ).unwrap();
    // Real hardware crash, root-caused via addr2line: ADC's first-ever `read_raw()` call
    // lazily creates a FreeRTOS semaphore deep inside ESP-IDF's newlib lock retargeting
    // (`components/newlib/locks.c::lock_init_generic`) -- and that file's own comment says
    // exactly what happens if that allocation fails: `abort(); /* No more semaphores
    // available or OOM */`. This is a hard, unconditional whole-device abort with no Rust
    // `Result` in the loop to catch -- `.unwrap_or(0)` on the read itself can't help, because
    // the C code never returns to the caller at all. The actual first `read_raw()` call used
    // to happen only in the render loop, well after WiFi connect + finFetch's TLS churn had
    // already started eating heap -- so this lazy init could land in the exact heap-starved
    // window this whole project has been fighting. After the FIRST successful read, the
    // semaphore is cached and every later read just re-acquires it (no more allocation) --
    // so doing one throwaway read right here, while heap is still at its post-boot peak
    // (before WiFi/fetch/lampBridge/exprotocol have touched anything), makes this permanently
    // safe for the rest of the boot instead of gambling on when the render loop happens to
    // make its first real call.
    let _ = vol_pin.read_raw();
    // Same class of bug, a SECOND confirmed real instance: `lock_init_generic`'s
    // "abort(); /* No more semaphores available or OOM */" isn't specific to the ADC driver --
    // it's ESP-IDF's GENERIC newlib lock-retargeting mechanism, used by many first-use lazy
    // locks independently. Real hardware crash #2, resolved via addr2line, hit the exact same
    // abort() but through `clock_gettime` -> `esp_time_impl_get_boot_time`'s own lazy lock,
    // triggered by `ticker::build_header`'s `SystemTime::now()` call during normal render-loop
    // operation deep into the heap-starved window. Same fix, same reasoning: warm this up here
    // too, while heap is still at its post-boot peak, rather than leaving its first real call
    // to chance. If a THIRD such lock is ever found the same way (a real, addr2line-resolved
    // abort() bottoming in `lock_init_generic` via some other ESP-IDF first-use facility),
    // add its own one-line warm-up call right here, next to these two -- don't rediscover this
    // whole investigation from scratch.
    let _ = std::time::SystemTime::now().duration_since(std::time::SystemTime::UNIX_EPOCH);
    // THIRD confirmed real instance, found exactly as the comment above anticipated: multiple
    // real hardware crashes, resolved via a full GDB session against a saved core dump (not
    // just addr2line -- see project_esp32_heap_budget.md for the exact register dump and
    // backtrace), landed on the same `lock_init_generic`-class abort(), this time through
    // `usleep()` -- the function `std::thread::sleep()` compiles down to on this target. The
    // main render loop's own first-ever `std::thread::sleep()` call (its per-iteration delay,
    // the very last line of the loop body) doesn't happen until AFTER WiFi connect + LCD init
    // have completed and finFetch/lampBridge/exprotocol are already running and consuming heap
    // -- every OTHER delay used during boot setup is `FreeRtos::delay_ms` (a plain FreeRTOS
    // primitive, no newlib lock involved), so `usleep`'s own lazy lock genuinely never got
    // exercised until deep into the heap-starved window, same as the two fixes above. Confirmed
    // recurring even with ZERO external HTTP load -- the periodic fetch cycle's own heap
    // pressure alone was enough to trigger it once.
    std::thread::sleep(Duration::from_millis(0));
    led_green.set_low().unwrap();
    led_red.set_high().unwrap(); // red on until WiFi connects
    led_yellow.set_low().unwrap();

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

    // Disable WiFi power save (modem sleep) entirely. ESP-IDF's default (confirmed via serial:
    // "wifi:pm start, type: 1" on every boot, never previously overridden anywhere in this
    // codebase) powers the radio down between beacon intervals and relies on a timer to wake
    // back up in time to receive the next one. Real hardware evidence this session: `bcn_timeout`
    // disassociations kept recurring despite consistently EXCELLENT RSSI (-17 to -27 dBm across
    // many serial captures) -- a signal-quality problem would show weak/marginal RSSI, not this.
    // A missed wake-up window under this device's own well-documented CPU/heap pressure (see
    // project_esp32_heap_budget.md) fits far better: the radio can be strong AND still miss a
    // beacon if the CPU doesn't service the wake-up interrupt in time. This is a DIFFERENT
    // mechanism than the TX-power-vs-brownout tuning below (kept as-is, real and unrelated) --
    // that one addressed a current-spike/brownout-margin problem; this one addresses beacon
    // *timing*. Zero downside on a device that's always mains-powered: power save exists purely
    // to save battery, which isn't a consideration here.
    unsafe { let _ = esp_idf_sys::esp_wifi_set_ps(esp_idf_sys::wifi_ps_type_t_WIFI_PS_NONE); }

    // Cap TX power below the ~20 dBm default, to reduce the current spike
    // that was causing brownout-adjacent instability (see sdkconfig.defaults
    // - brownout detection is back on now, at its lowest threshold). Was 40
    // (10 dBm, a 10 dB cut) - too aggressive: confirmed via serial a real
    // "wifi:bcn_timeout ... reset wifi status to disassoc" full
    // disassociation, which then cascades into every network-dependent
    // feature (mic-srv, Tuya) failing with connection errors that look
    // exactly like the heap/crash symptoms this session chased, but aren't.
    // RSSI (received signal AT the ESP32, from the AP) being strong doesn't
    // guarantee the REVERSE link has equal margin - cutting TX power only
    // helps the downlink's read, it can hurt the ESP32's own uplink being
    // heard by the AP. 60 = 15 dBm, a more moderate 5 dB cut - still real
    // current-spike reduction, less likely to push the uplink margin
    // negative.
    unsafe { esp_idf_sys::esp_wifi_set_max_tx_power(60); }

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

    // Temp/humidity sensor — GPIO32, freed from the (never-wired) Display button above.
    dht22::spawn_dht22_reader(peripherals.pins.gpio32);

    // Shared state
    let ui_state   = Arc::new(Mutex::new(screen::UiState::default()));
    let lamp_handle = Arc::new(tuya::LampHandle::new());
    let led_state   = Arc::new(led::LedState::new());

    // The price-fetch cycle and the Tuya lamp bridge's own independent 5s
    // poll/refresh run on completely separate threads with no coordination -
    // they drift in and out of phase and periodically land at the same
    // moment, each opening real TLS/TCP connections. Confirmed via serial
    // console: "alloc(N bytes) failed" heap exhaustion and at least one real
    // crash during exactly this kind of overlap. This lock just makes sure
    // the two subsystems' network-heavy sections never run concurrently.
    let network_lock = Arc::new(Mutex::new(()));

    // Chart-cache refresher's own outbound fetch joins this too now -- a real coredump showed
    // it colliding with finFetch's own allocations at boot before this was added; see dht22.rs.
    dht22::spawn_chart_cache_refresher(Arc::clone(&network_lock));
    // Note: the "show more" 1h/6h/24h range buttons and on-demand weather refresh do NOT get a
    // boot-time spawn here -- both are on-demand, throwaway-thread-per-request designs (see
    // dht22::request_range / api::request_weather_refresh's own comments for the real boot-time
    // thread-starvation bug a permanent standing thread for each caused here).

    // ExProtocol test endpoint — additive only, see exprotocol_task.rs. Joins the same
    // `network_lock` finFetch/lamp already use: real hardware testing showed its handshake
    // crypto is exactly as heap-hungry as their TLS/TCP work, and a static free-heap-threshold
    // gate couldn't protect against it (fetch cycle keeps heap pinned low for its whole run,
    // not just briefly). Must be created after `network_lock` above so there's a lock to pass.
    if config::EXPROTOCOL_ENABLED {
        exprotocol_task::spawn_exprotocol_server(Arc::clone(&network_lock));
    }

    // Set initial LED state now that wifi is connected.
    led_state.on_wifi_connect(!screen_forced_off.load(Ordering::Relaxed));

    // Mic/lamp server — accepts "m:0", "m:1" (mic LED), "l:t" (lamp toggle).
    spawn_mic_server(Arc::clone(&lamp_handle), Arc::clone(&ui_state), Arc::clone(&led_state));

    let auto_rotate = Arc::new(AtomicBool::new(true));

    // Web control server — browse to http://<ESP_IP>
    if config::WEB_SERVER_ENABLED {
        web::spawn(Arc::clone(&web_triggers), Arc::clone(&ui_state), Arc::clone(&screen_forced_off), Arc::clone(&lamp_handle), Arc::clone(&auto_rotate), Arc::clone(&led_state), Arc::clone(&network_lock));
    }

    // Report boot/reset reason now that wifi is actually up and an HTTP
    // request can succeed - fire-and-forget, never blocks this thread.
    api::report_event("boot", boot_severity, format!("device booted, reset reason: {boot_reason}"), "system");

    {
        let mut st = ui_state.lock().unwrap();
        st.wifi_connected = true;
        // Show hourglass from first render, cleared when the (gated) price-fetch cycle
        // completes -- only meaningful while that cycle actually runs, else it would spin
        // forever with nothing ever clearing it.
        st.fetching = config::PRICE_FETCH_ENABLED;
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

    // ── Weather fetch thread ─────────────────────────────────────────────────
    // Always on, on-demand: triggered by `web_triggers.weather` (set on every "/" dashboard
    // page load, see web.rs), rate-limited by WEATHER_MIN_REFRESH_MS so repeat page loads/tabs
    // don't refetch needlessly. Not gated by PRICE_FETCH_ENABLED below -- disabling weather was
    // never asked for, only its cadence (was a fixed background timer, now on-demand). The
    // DHT22 room sensor is unrelated to any of this and keeps its own independent read/store
    // cadence (see dht22.rs).
    loop {
        let ui_weather      = Arc::clone(&ui_state);
        let web_triggers_wx = Arc::clone(&web_triggers);
        let net_lock_wx     = Arc::clone(&network_lock);
        let nvs_weather     = nvs_cache.clone();
        let weather_worker  = api::spawn_weather_worker();
        let spawned = std::thread::Builder::new()
        .name("weatherFetch".into())
        .stack_size(4096)
        .spawn(move || {
            let mut last_fetch_ms: u64 = 0;
            loop {
                if web_triggers_wx.weather.swap(false, Ordering::Relaxed) {
                    let now = millis();
                    if last_fetch_ms == 0 || now.saturating_sub(last_fetch_ms) >= config::WEATHER_MIN_REFRESH_MS {
                        last_fetch_ms = now;
                        info!("[net] weather fetch (on-demand)");
                        // Lock spans trigger→collect, not just collect -- it must cover the
                        // actual `fetch_weather` call running on the worker thread, or Tuya/
                        // exprotocol/dht22 could open their own network op concurrently with
                        // it (the exact heap-unsafe overlap network_lock exists to prevent).
                        let data = {
                            let _net_guard = net_lock_wx.lock().unwrap();
                            weather_worker.trigger();
                            weather_worker.collect()
                        };
                        if data.ok_weather {
                            if let Ok(mut st) = ui_weather.lock() {
                                st.data.weather_temp = data.weather_temp;
                                st.data.weather_code = data.weather_code;
                                st.data.ok_weather   = true;
                            }
                            cache::save(&nvs_weather, &data);
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        });
        match spawned {
            Ok(_) => break,
            Err(e) => {
                error!("[boot] weatherFetch spawn failed: {e}, retrying in 500ms");
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }

    // ── Price fetch thread (crypto/gold/oil/USD-BRL) ─────────────────────────
    // Dormant while `config::PRICE_FETCH_ENABLED` is false (2026-08-30, "we're not even using
    // any of that") -- when disabled, NOTHING below spawns: no finFetch thread, no fetchA/
    // fetchB worker threads, no stack/heap reserved for any of them, no network_lock
    // contention. Flip the flag to bring it back exactly as it was.
    let fetch_trigger = Arc::new(AtomicBool::new(false));
    if config::PRICE_FETCH_ENABLED {
        // A bare `.unwrap()` here used to abort the whole device whenever `pthread_create` failed
        // under boot-time heap pressure (confirmed via serial: `Os { code: 12, kind: OutOfMemory }`
        // panicking main.rs, right as this thread raced other boot-time thread spawns for the same
        // tight heap) - retrying gives those concurrent allocations a chance to free first instead
        // of treating a transient resource shortage as fatal.
        loop {
            let ui_net        = Arc::clone(&ui_state);
            let trigger_net   = Arc::clone(&fetch_trigger);
            let nvs_fetch     = nvs_cache.clone(); // fetch thread gets its own clone
            let net_lock_fetch = Arc::clone(&network_lock);
            let spawned = std::thread::Builder::new()
            .name("finFetch".into())
            .stack_size(20480)
            .spawn(move || {
                let (wa, wb) = api::spawn_price_fetch_workers(Arc::clone(&net_lock_fetch));
                // Worker B (gold, oil - 2 sequential HTTPS calls) only runs every Nth cycle --
                // historically the flakier pair under heap fragmentation, and nothing here has
                // a physical screen to show it fresh every cycle anyway.
                const FETCH_B_EVERY_N_CYCLES: u32 = 4;
                let mut cycle: u32 = 0;
                loop {
                info!("[net] fetch cycle start");
                let mut data = api::MarketData::default();
                let include_b = cycle % FETCH_B_EVERY_N_CYCLES == 0;
                cycle = cycle.wrapping_add(1);

                if let Ok(mut st) = ui_net.lock() { st.fetching = true; st.loading_frame = 0; }

                api::fetch_all(&mut data, &wa, &wb, include_b);

                cache::save(&nvs_fetch, &data);

                if let Ok(mut st) = ui_net.lock() {
                    api::merge(&mut st.data, data);
                    st.fetching = false;
                    st.fetch_completed_at = millis();
                }

                // Discard any trigger that fired WHILE the cycle above was
                // already running - that data is already fresh, so honoring a
                // stale trigger here would just launch an immediate,
                // redundant second cycle (confirmed via serial: back-to-back
                // "fetch cycle start" ~100ms apart, itself enough concurrent
                // HTTPS churn to throw several "pthread: Failed to create
                // task!" errors on its own).
                trigger_net.store(false, Ordering::Relaxed);

                // Wait for next interval, but exit early if trigger fires.
                let wait_until = millis() + config::FETCH_INTERVAL_MS;
                loop {
                    if trigger_net.swap(false, Ordering::Relaxed) { break; }
                    let remaining = wait_until.saturating_sub(millis());
                    if remaining == 0 { break; }
                    std::thread::sleep(Duration::from_millis(remaining.min(100)));
                }
                } // loop
            });
            match spawned {
                Ok(_) => break,
                Err(e) => {
                    error!("[boot] finFetch spawn failed: {e}, retrying in 500ms");
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
        }
    }

    // ── Lamp bridge thread ───────────────────────────────────────────────────
    // Same retry-on-spawn-failure treatment as finFetch above, for the same reason.
    loop {
        let lamp_bridge = Arc::clone(&lamp_handle);
        let ui_lamp     = Arc::clone(&ui_state);
        let net_lock_lamp = Arc::clone(&network_lock);
        let spawned = std::thread::Builder::new()
        .name("lampBridge".into())
        // Was 16384 -- investigated the actual call chain (tuya session/crypto/protocol) and
        // found no large stack-local buffers or deep recursion: the Tuya AES-ECB/GCM helpers use
        // small fixed 16/32-byte arrays, and message buffers are heap-allocated `Vec`s, not stack
        // arrays. 16384 looks like a conservative round number (consistent with this file's other
        // untuned thread sizes), not something measured as required. Testing smaller to free real
        // margin for ExProtocol's own dispatch thread on this same heap-tight device -- watch for
        // any real crash/corruption before trusting this long-term.
        .stack_size(8192)
        .spawn(move || {
            let mut last_refresh_ms: u64 = 0;
            loop {
                // poll() almost always has nothing to do (no user-initiated toggle pending) and
                // returns false immediately without touching the network -- checking that first
                // avoids taking network_lock on this thread's 20ms cadence for no reason. That
                // constant relocking was starving ExProtocol's own dispatch thread out of ever
                // getting a real turn at network_lock (confirmed on real hardware). A pending
                // target that appears between this check and the lock just waits one more 20ms
                // cycle -- imperceptible for a lamp toggle.
                let toggled = if lamp_bridge.has_pending_target() {
                    let _net_guard = net_lock_lamp.lock().unwrap();
                    lamp_bridge.poll()
                } else {
                    false
                };

                let now = millis();
                let do_refresh = now - last_refresh_ms >= 5_000;
                if do_refresh {
                    last_refresh_ms = now;
                    let _net_guard = net_lock_lamp.lock().unwrap();
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
        });
        match spawned {
            Ok(_) => break,
            Err(e) => {
                error!("[boot] lampBridge spawn failed: {e}, retrying in 500ms");
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }

    // ── Watchdog: subscribe main task, trigger panic (→ reset) if loop stalls ──
    // 45s, not 30s: fetch_all's two workers now run sequentially (see
    // api.rs - required for heap safety), and their combined worst-case
    // (HTTP_RETRIES/HTTP_TIMEOUT_MS across 5 URLs total) sits right around
    // 30s on its own. That left zero margin against the watchdog this was
    // meant to protect against; 45s gives real headroom instead of trading
    // one crash mode for another.
    unsafe {
        let wdt_cfg = esp_idf_sys::esp_task_wdt_config_t {
            timeout_ms: 45_000,
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
    let mut last_manual_fetch_ms:u64 = 0;
    let mut last_debounce_light_ms: u64 = 0;
    let mut last_loading_ms:      u64 = loop_start;
    let mut last_lamp_loading_ms: u64 = loop_start;
    let mut last_wifi_check_ms:     u64 = loop_start;
    let mut last_chart_watchdog_ms: u64 = loop_start;
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
    // Real debounce (stable-state, not the shared blanking-period pattern the other
    // buttons use) -- found live (2026-09-21): the "fire on first edge, then ignore
    // everything for DEBOUNCE_MS" pattern the other buttons share only blocks a SECOND
    // trigger within that window; if this specific switch's contact bounce runs past
    // DEBOUNCE_MS (worn/flaky switch, exactly what was reported: the blue play/pause
    // button, not the others), the tail end of the same physical press's bounce reads
    // as a brand new falling edge once the window has already expired -- one press,
    // two play/pause toggles. This instead requires the raw pin to hold its new value
    // continuously for the full window before accepting it as real, which filters bounce
    // no matter how it's distributed in time, at the cost of DEBOUNCE_MS of added latency
    // that's imperceptible for a play/pause button.
    let mut media_raw_last      = true;
    let mut media_raw_since_ms: u64 = 0;
    let mut media_stable_state  = true;
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
    let mut last_hw_yellow  = false;

    // Diagnostic added 2026-08-29: `main_task`'s own stack-depth margin has caused a real,
    // repeatedly-observed `IllegalInstruction` crash (usleep -> fin_esp_rs::main, GDB-decoded
    // three separate times -- see project memory). A prior cycle's live capture campaign only
    // ever logged one AGGREGATE high-water-mark reading every 5s, which requires catching the
    // exact rare excursion live to attribute it to a specific branch -- tried repeatedly across
    // several cycles and never once succeeded. This tracks the all-time-lowest reading seen so
    // far and logs a NEW low immediately, tagged by WHERE in the loop it was taken -- passively
    // revealing which branch correlates with a deeper stack excursion over real uptime, without
    // needing to catch anything live. The strongest untested candidate found by code review so
    // far: the "periodic LCD re-init" block below runs only once every 30 minutes, combining
    // `lcd.init()` + `prime_cgram()` + `render()` in one burst -- every live capture in this
    // whole investigation has been minutes long at most, so that specific combination has almost
    // certainly never been exercised during any prior observation window.
    let mut min_hwm_seen: u32 = u32::MAX;

    loop {
        let now = millis();

        // ── Read state (brief lock) ───────────────────────────────────────────
        let (lamp_anim_active, is_fetching, wifi_connected) = {
            let st = ui_state.lock().unwrap();
            let anim = st.lamp_anim_until > 0 && now < st.lamp_anim_until;
            (anim, st.fetching, st.wifi_connected)
        };

        // ── Record price history after each fetch (no-op while PRICE_FETCH_ENABLED is
        // false, since fetch_completed_at never updates then) ─────────────────
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
            check_stack_watermark("after-lamp-anim-full-render", &mut min_hwm_seen);
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
            // Leading candidate for main_task's own rare stack-depth excursion (see the
            // diagnostic's own comment at min_hwm_seen's declaration): this whole block runs
            // only once every 30 minutes, and no live capture in this investigation's history
            // has ever run that long, so this specific call combination has likely never been
            // directly observed before.
            check_stack_watermark("after-30min-lcd-reinit", &mut min_hwm_seen);
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
        // No physical LCD anymore, so this only rotates the value a /status caller sees.
        if !chart_active && auto_rotate.load(Ordering::Relaxed) && now - last_auto_screen_ms >= config::AUTO_SCREEN_INTERVAL_MS {
            last_auto_screen_ms = now;
            if let Ok(mut st) = ui_state.lock() { st.screen = st.screen.next(); }
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
                // Physical button presses aren't reported - they're too
                // frequent to be useful signal, and this is a personal
                // device sitting right in front of the user anyway. Only
                // web-triggered presses (someone hitting the API/UI, not
                // the physical button) are worth logging.
                if web_screen {
                    let origin = web_triggers.last_origin_external.load(Ordering::Relaxed);
                    api::report_event("button_press", "info", "screen button (web)".to_string(),
                        if origin { "web-external" } else { "web-internal" });
                }
                if let Ok(mut st) = ui_state.lock() {
                    st.screen = st.screen.next();
                    persist.save_screen(st.screen);
                }
            }
            // Rate-limited independently of the 50ms UI debounce above - a
            // stuck/bouncing button (real GPIO26 noise, seen firing ~1
            // press/sec for minutes straight) must still be able to flip
            // screens freely (cheap, local), but forcing a fresh HTTPS
            // fetch cycle on every single one of those was hammering the
            // network layer continuously and starving the main task long
            // enough to trip the watchdog. One real button press is never
            // going to want a second forced re-fetch within 10s anyway.
            // No-op entirely while PRICE_FETCH_ENABLED is false -- nothing
            // is listening on fetch_trigger then.
            if config::PRICE_FETCH_ENABLED {
                const MIN_MANUAL_FETCH_INTERVAL_MS: u64 = 10_000;
                if now - last_manual_fetch_ms >= MIN_MANUAL_FETCH_INTERVAL_MS {
                    last_manual_fetch_ms = now;
                    fetch_trigger.store(true, Ordering::Relaxed);
                }
            }
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

        // ── Display power (web-triggered only - GPIO32 is now the DHT22's data line) ──
        let web_display_raw = if config::WEB_SERVER_ENABLED { web_triggers.display.swap(-1, Ordering::Relaxed) } else { -1i8 };
        let web_display = web_display_raw >= 0;
        if web_display {
            let _ = led_red.set_high(); FreeRtos::delay_ms(80); let _ = led_red.set_low(); led_state.set_red(false); last_hw_red = false;
            {
                let origin = web_triggers.last_origin_external.load(Ordering::Relaxed);
                api::report_event(
                    "button_press", "info",
                    "display button (web)".to_string(),
                    if origin { "web-external" } else { "web-internal" },
                );
            }
            // DISABLED (config::DISPLAY_TOGGLE_ENABLED) - screen (LCD) is
            // physically disconnected, so toggling "is the screen forced
            // off" has no real effect. Left in place, not deleted, in case
            // the screen is ever reconnected - the button (and the web
            // toggle) still runs the red-flash/clear above either way.
            if config::DISPLAY_TOGGLE_ENABLED {
                let sfo = web_display_raw == 0;
                screen_forced_off.store(sfo, Ordering::Relaxed);
                persist.save_screen_forced(sfo);
                if sfo { led_state.on_screen_off(); } else { led_state.on_screen_on(wifi_connected); }
                info!("[btn] display {} (web)", if sfo { "off" } else { "on" });
            }
        }

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
            // Only web-triggered presses are reported - see the display
            // button's identical reasoning above.
            if web_pot {
                let origin = web_triggers.last_origin_external.load(Ordering::Relaxed);
                api::report_event(
                    "button_press", "info",
                    "pot button (web)".to_string(),
                    if origin { "web-external" } else { "web-internal" },
                );
            }
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

        // ── Media play/pause button (GPIO 27, active LOW) ────────────────────
        let media_raw = btn_media.is_high();
        if media_raw != media_raw_last {
            media_raw_last = media_raw;
            media_raw_since_ms = now;
        }
        let mut phys_media = false;
        if media_raw != media_stable_state && now - media_raw_since_ms >= config::DEBOUNCE_MS {
            if media_stable_state && !media_raw {
                phys_media = true; // confirmed falling edge: button actually pressed
            }
            media_stable_state = media_raw;
        }
        let web_media  = config::WEB_SERVER_ENABLED && web_triggers.media.swap(false, Ordering::Relaxed);
        if phys_media || web_media {
            info!("[btn] media play/pause ({})", if web_media { "web" } else { "physical" });
            if web_media {
                let origin = web_triggers.last_origin_external.load(Ordering::Relaxed);
                api::report_event(
                    "button_press", "info",
                    "media button (web)".to_string(),
                    if origin { "web-external" } else { "web-internal" },
                );
            }
            let _ = led_red.set_high(); FreeRtos::delay_ms(80); let _ = led_red.set_low(); led_state.set_red(false); last_hw_red = false;
            PLAY_PAUSE_READY.store(true, Ordering::Relaxed);
        }

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
        let b = led_state.yellow .load(Ordering::Relaxed);
        if g != last_hw_green { last_hw_green = g; if g { led_green.set_high().unwrap(); } else { led_green.set_low().unwrap(); } }
        if r != last_hw_red   { last_hw_red   = r; if r { led_red  .set_high().unwrap(); } else { led_red  .set_low().unwrap(); } }
        if b != last_hw_yellow  { last_hw_yellow  = b; if b { led_yellow .set_high().unwrap(); } else { led_yellow .set_low().unwrap(); } }

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
                    api::report_event("wifi_recovered", "info", "wifi reconnected".to_string(), "system");
                } else {
                    led_state.on_wifi_disconnect(screen_on);
                    api::report_event("wifi_lost", "warning", "wifi disconnected".to_string(), "system");
                }
            }
            if let Ok(mut st) = ui_state.lock() { st.wifi_connected = connected; }
            check_stack_watermark("after-wifi-check-block", &mut min_hwm_seen);
        }

        // ── Chart-cache-refresher watchdog every 15 s ────────────────────────
        // Real live bug found 2026-08-30: that thread can get permanently stuck (not crash --
        // confirmed via direct HTTP observation of its own cache going linearly stale for 5+
        // minutes straight with zero successful refreshes), most likely an `esp_http_client`
        // call not honoring its own timeout under this device's real WiFi flakiness. No
        // coredump is possible for a hang (nothing panics) and there's no JTAG on this hardware
        // to catch the exact stuck call live -- see dht22::REFRESHER_HEARTBEAT_MS's own comment
        // for the full writeup. Rust can't forcibly cancel a stuck thread, so this main loop
        // (confirmed to keep running fine throughout the whole real stuck window observed)
        // reboots the WHOLE DEVICE if that thread's heartbeat goes stale -- turning "silently
        // wrong until someone notices and power-cycles it" into "recovers within ~2 minutes on
        // its own." 120s is comfortably above this thread's own worst normal cadence (a few
        // seconds per iteration even when servicing a range click) with real margin, while
        // still bounding the outage to something a person would tolerate.
        if now - last_chart_watchdog_ms >= 15_000 {
            last_chart_watchdog_ms = now;
            if let Some(stuck_ms) = dht22::refresher_stuck_for_ms() {
                if stuck_ms >= 120_000 {
                    error!("[watchdog] chart-cache-refresher stuck for {}ms — rebooting", stuck_ms);
                    api::report_event(
                        "chart_refresher_stuck", "critical",
                        std::format!("chart-cache-refresher heartbeat stale for {}ms, forcing reboot", stuck_ms),
                        "system",
                    );
                    std::thread::sleep(Duration::from_millis(300)); // let the report_event queue flush
                    unsafe { esp_idf_sys::esp_restart(); }
                }
            }
        }

        // ── Button GPIO debug: log raw pin state every 5 s ───────────────────
        // Prints 1=HIGH(released) 0=LOW(pressed). Helps diagnose wiring issues.
        if now - last_btn_debug_ms >= 5000 {
            last_btn_debug_ms = now;
            info!("[gpio] screen(26)={} lamp(12)={} warm(13)={} bright(14)={} media(27)={} forced_off={}",
                btn_screen.is_high() as u8,
                btn_light.is_high() as u8,
                btn_warm.is_high() as u8,
                btn_bright.is_high() as u8,
                btn_media.is_high() as u8,
                screen_forced_off.load(Ordering::Relaxed) as u8);
            // Diagnostic added 2026-08-29 while investigating a recurring `IllegalInstruction`
            // crash whose GDB-decoded backtrace bottoms in `usleep -> fin_esp_rs::main` with
            // A0 == FreeRTOS's 0xa5 stack-fill byte -- i.e. a Xtensa register-window fill reading
            // never-touched main_task stack memory as a return address. Disassembly confirmed
            // this loop's own 1ms `usleep()` call takes the lock-free `esp_rom_delay_us` fast
            // path (no newlib lock involved at all), ruling out the lazy-lock theory an earlier
            // fix was based on -- this looks like a genuine main_task (CONFIG_ESP_MAIN_TASK_STACK_SIZE)
            // stack-depth margin problem instead. Logging the real high-water-mark (words free,
            // never recovers until reboot) directly, rather than continuing to guess from crash
            // post-mortems alone.
            // NOTE: despite the upstream FreeRTOS doc comment saying "in words", ESP-IDF's own
            // Xtensa port returns this already in BYTES (confirmed empirically: an earlier
            // version of this line multiplied by 4 and printed 36512 bytes free against a
            // 16384-byte total stack -- an impossible value, proving the raw return is bytes
            // already, matching how CONFIG_ESP_MAIN_TASK_STACK_SIZE/Rust's stack_size() are
            // both specified in bytes on this platform).
            let hwm = unsafe { esp_idf_sys::uxTaskGetStackHighWaterMark(std::ptr::null_mut()) };
            info!("[stack] main_task high-water-mark: {} bytes free (all-time min)", hwm);
            // Keeps the new branch-tagged NEW-low tracker (see check_stack_watermark and
            // min_hwm_seen's own declaration comment) in sync with this pre-existing periodic
            // reading, so a later branch-tagged check doesn't misreport "new low" for a value
            // this unconditional 5s check already saw and reported first.
            if hwm < min_hwm_seen { min_hwm_seen = hwm; }
            // Diagnostic added 2026-08-29, same cycle: mic_key_daemon.py's own systemd journal
            // shows its persistent connection to mic-srv (port 9877) gets "[Errno 104] Connection
            // reset by peer" roughly every ~20s, indefinitely, reconnecting successfully each
            // time -- but a full 75s serial capture spanning several of these resets never once
            // printed mic-srv's own "{peer} disconnected"/"read err" log line (see
            // handle_mic_connection in this file). That means the reset happens BELOW the
            // BSD-socket read() this thread blocks on -- lwIP silently killing the underlying PCB
            // without ever waking the blocked Rust read -- which would leak that thread (and its
            // 8192-byte stack) forever, every ~20s per connected client, directly matching this
            // whole file's chronic heap exhaustion and the still-unexplained pthread_create
            // failure storm. Logging live FreeRTOS task count to test this directly: if it climbs
            // without bound over uptime, the leak is real and confirmed; if stable, this
            // hypothesis is wrong and something else explains the storm.
            let task_count = unsafe { esp_idf_sys::uxTaskGetNumberOfTasks() };
            info!("[tasks] live FreeRTOS task count: {}", task_count);
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

/// One connection slot's state, polled from `spawn_media_server`'s single thread instead of
/// living on its own spawned thread -- see that function's own comment for why.
struct MediaConn {
    stream: std::net::TcpStream,
    identified: bool,
    id_buf: [u8; 128],
    id_len: usize,
    machine_id: String,
    last_vol: u8,
    last_targeted_vol: Option<u8>,
    keepalive: u32,
}

const MAX_MEDIA_CONNS: usize = 3;

// Rewritten 2026-08-29, same fix and same reason as `spawn_mic_server`'s own rewrite just
// above: this used to spawn a "media-conn" thread per accepted connection, and that spawn
// itself was what kept failing under this device's routine heap pressure
// ("pthread: Failed to create task! / Not enough space (os error 12)") -- confirmed live,
// happening roughly every 3 seconds while `play_pause_server.py` sat in its own reconnect
// loop, and directly implicated in a real device-wide heap collapse the same session (even
// the unrelated Tuya lamp connection started failing with the identical ENOMEM error after
// several minutes of this running). No per-connection thread spawn at all now -- a small
// fixed pool of up to `MAX_MEDIA_CONNS` non-blocking streams, all polled from this one
// thread's own loop, the same pattern ExProtocol's `PolledTcpTransport` and `spawn_mic_server`
// already use. `desktop` + `laptop` was the original "up to one per real machine" intent this
// replaces -- 3 leaves one spare slot rather than exactly matching today's count.
fn spawn_media_server() {
    std::thread::Builder::new()
        .name("media-srv".into())
        .stack_size(8192)
        .spawn(|| {
            use std::io::{Read, Write};
            use std::net::TcpListener;
            loop {
                let listener = match TcpListener::bind("0.0.0.0:9876") {
                    Ok(l) => l,
                    Err(e) => { warn!("[media-srv] bind err: {e}, retrying"); FreeRtos::delay_ms(2000); continue; }
                };
                if let Err(e) = listener.set_nonblocking(true) {
                    warn!("[media-srv] set_nonblocking failed: {e}, retrying bind");
                    FreeRtos::delay_ms(2000);
                    continue;
                }
                info!("[media-srv] listening on :9876 (single polled thread, no per-connection spawn)");

                let mut conns: [Option<MediaConn>; MAX_MEDIA_CONNS] = [None, None, None];

                loop {
                    // Accept whatever's waiting -- if every slot is full, the accepted stream
                    // is just dropped (closes cleanly), which is fine: the client's own
                    // reconnect loop already retries on any failure/close.
                    match listener.accept() {
                        Ok((s, addr)) => {
                            if let Err(e) = s.set_nonblocking(true) {
                                warn!("[media-srv] new conn set_nonblocking failed: {e}");
                            } else if let Some(slot) = conns.iter_mut().find(|c| c.is_none()) {
                                info!("[media-srv] {addr} connected");
                                *slot = Some(MediaConn {
                                    stream: s,
                                    identified: false,
                                    id_buf: [0u8; 128],
                                    id_len: 0,
                                    machine_id: String::new(),
                                    last_vol: VOLUME_PCT.load(Ordering::Relaxed),
                                    last_targeted_vol: None,
                                    keepalive: 0,
                                });
                            } else {
                                warn!("[media-srv] {addr} connected but all {MAX_MEDIA_CONNS} slots full, dropping");
                            }
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                        Err(e) => warn!("[media-srv] accept err: {e}"),
                    }

                    for slot in conns.iter_mut() {
                        let Some(c) = slot else { continue };
                        let mut drop_conn = false;

                        // Same handshake this always was ("id:<machine>\n"), just accumulated
                        // into a fixed on-stack buffer across possibly-several non-blocking
                        // reads instead of one blocking byte-at-a-time loop.
                        if !c.identified {
                            let mut readbuf = [0u8; 64];
                            match c.stream.read(&mut readbuf) {
                                Ok(0) => drop_conn = true,
                                Ok(n) => {
                                    for &b in &readbuf[..n] {
                                        if b == b'\n' {
                                            let line = std::str::from_utf8(&c.id_buf[..c.id_len]).unwrap_or("").trim();
                                            match line.strip_prefix("id:") {
                                                // Same guard, same reason as handle_mic_line's own
                                                // `machine.to_string()` fix -- a plain `.to_string()`
                                                // has no fallible allocation path, and this one runs
                                                // on EVERY media-srv (re)connection, including any
                                                // boot-time/post-WiFi-blip reconnect burst (this
                                                // device's own well-documented worst heap-pressure
                                                // window). Dropping the connection on low heap (same
                                                // as the "bad handshake" case right below) is safe --
                                                // the client's own reconnect loop retries.
                                                Some(id) if !id.is_empty() => {
                                                    if unsafe { esp_idf_sys::heap_caps_get_largest_free_block(esp_idf_sys::MALLOC_CAP_8BIT) } >= 512 {
                                                        c.machine_id = id.to_string();
                                                        c.identified = true;
                                                        info!("[media-srv] {} identified", c.machine_id);
                                                    } else {
                                                        warn!("[media-srv] heap too fragmented to safely identify {id}, dropping");
                                                        drop_conn = true;
                                                    }
                                                }
                                                _ => {
                                                    warn!("[media-srv] bad id: handshake: {line:?}");
                                                    drop_conn = true;
                                                }
                                            }
                                        } else if c.id_len < c.id_buf.len() {
                                            c.id_buf[c.id_len] = b;
                                            c.id_len += 1;
                                        } else {
                                            warn!("[media-srv] id: handshake too long, dropping");
                                            drop_conn = true;
                                        }
                                    }
                                }
                                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                                Err(e) => { warn!("[media-srv] read err before identifying: {e}"); drop_conn = true; }
                            }
                        } else {
                            // Drain any incoming bytes (this protocol is receive-only from the
                            // ESP32's side after the handshake, but still needs to notice a
                            // clean close/reset instead of writing into a dead socket forever).
                            let mut readbuf = [0u8; 64];
                            match c.stream.read(&mut readbuf) {
                                Ok(0) => drop_conn = true,
                                Ok(_) => {}
                                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                                Err(e) => { warn!("[media-srv] {} read err: {e}", c.machine_id); drop_conn = true; }
                            }

                            if !drop_conn {
                                let is_owner = CURRENT_OWNER.lock()
                                    .map(|o| o.as_deref() == Some(c.machine_id.as_str()))
                                    .unwrap_or(false);
                                if is_owner && PLAY_PAUSE_READY.swap(false, Ordering::Relaxed) {
                                    if c.stream.write_all(b"p\n").is_err() { drop_conn = true; }
                                    else { info!("[media-srv] play/pause sent -> {} (mic-owner)", c.machine_id); }
                                }
                                if !drop_conn && take_media_target(&c.machine_id) {
                                    if c.stream.write_all(b"p\n").is_err() { drop_conn = true; }
                                    else { info!("[media-srv] play/pause sent -> {} (web target)", c.machine_id); }
                                }
                                let vol = VOLUME_PCT.load(Ordering::Relaxed);
                                if !drop_conn && vol != 255 && vol != c.last_vol {
                                    let pct = (vol as u32 * 100 / 153).min(100) as u8;
                                    let msg = std::format!("v:{}\n", pct);
                                    if c.stream.write_all(msg.as_bytes()).is_err() { drop_conn = true; }
                                    else { c.last_vol = vol; }
                                }
                                if !drop_conn {
                                    if let Some(targeted_vol) = peek_volume_target(&c.machine_id) {
                                        if c.last_targeted_vol != Some(targeted_vol) {
                                            let pct = (targeted_vol as u32 * 100 / 153).min(100) as u8;
                                            let msg = std::format!("v:{}\n", pct);
                                            if c.stream.write_all(msg.as_bytes()).is_err() { drop_conn = true; }
                                            else { c.last_targeted_vol = Some(targeted_vol); }
                                        }
                                    }
                                }
                                if !drop_conn {
                                    c.keepalive += 1;
                                    if c.keepalive >= 300 { // ~3s at this loop's 10ms tick, vs the old per-thread loop's own 3000 * 10ms
                                        if c.stream.write_all(b"k\n").is_err() { drop_conn = true; }
                                        c.keepalive = 0;
                                    }
                                }
                            }
                        }

                        if drop_conn {
                            info!("[media-srv] {} disconnected", if c.machine_id.is_empty() { "?" } else { &c.machine_id });
                            *slot = None;
                        }
                    }

                    FreeRtos::delay_ms(10);
                }
            }
        })
        .ok();
}

fn handle_mic_line(
    line: &str,
    lamp_handle: &Arc<tuya::LampHandle>,
    ui_state: &Arc<Mutex<screen::UiState>>,
    led_state: &Arc<led::LedState>,
) {
    // Real coredump-adjacent bug found live (2026-08-31): this used to collect into a
    // `Vec<&str>` -- no fallible allocation path, same bug class as every other fix this
    // whole project -- on EVERY single mic-srv/media-srv line, including rapid toggle bursts
    // right at boot (the exact heap-pressure window that's already aborted this device for
    // several OTHER unguarded allocations tonight, e.g. `Dynamic Impl: alloc(N bytes) failed`
    // during finFetch's own first cycle). A crash 18s after a real boot, during exactly such
    // a burst (confirmed via the mic daemon's own logs sending 4 messages in that window),
    // matches this bug's own fingerprint closely enough to fix proactively even without a
    // SHA-matching coredump for this exact crash. Iterator-based matching needs no heap
    // allocation at all -- strictly better than guarding the Vec, same "eliminate rather than
    // gate" fix already used for `with_cached_chart_json`.
    let mut it = line.trim().splitn(3, ':');
    match (it.next(), it.next(), it.next()) {
        // Tagged: "m:<0|1>:<machine>" - mic_key_daemon.py always
        // sends this shape now, tagging which machine toggled so
        // the media server knows who to route play/pause to.
        (Some("m"), Some(state), Some(machine)) => {
            let unmuted = state == "1";
            led_state.set_yellow(unmuted);
            info!("[mic] {} ({machine})", if unmuted { "unmuted" } else { "muted" });
            // `machine.to_string()` is a genuine allocation with no way around it (CURRENT_OWNER
            // must outlive this borrowed line) -- same guard as everywhere else in this
            // codebase for a real "no fallible API exists" allocation. Small (a hostname,
            // a few bytes) but tiny allocations have already been shown to fail on this
            // device during its worst real fragmentation moments -- skip gracefully rather
            // than gamble on it; a missed ownership-claim just means the NEXT mic toggle
            // (which will retry this) decides media routing instead, not a lost toggle.
            if unsafe { esp_idf_sys::heap_caps_get_largest_free_block(esp_idf_sys::MALLOC_CAP_8BIT) } >= 512 {
                if let Ok(mut owner) = CURRENT_OWNER.lock() {
                    *owner = Some(machine.to_string());
                }
            } else {
                warn!("[mic] heap too fragmented to safely claim ownership for {machine}, skipping");
            }
        }
        // Defensive fallback for an untagged/old-format sender -
        // still reflects mic state, just can't claim ownership.
        (Some("m"), Some(state), None) => {
            let unmuted = state == "1";
            led_state.set_yellow(unmuted);
            info!("[mic] {} (no machine id)", if unmuted { "unmuted" } else { "muted" });
        }
        (Some("l"), Some("t"), None) => {
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

// Rewritten 2026-08-29: was spawn-a-thread-per-connection (mic-srv accepts, spawns a
// "mic-conn" thread per client). Real hardware confirmed this thread-spawn itself is what
// was failing under this device's routine heap pressure ("pthread: Failed to create task! /
// Not enough space (os error 12)") -- happening roughly every 3 seconds under today's full
// feature load, meaning a real mic toggle sent while the spawn fails is silently dropped:
// the TCP connection is accepted (so the client sees "connected"), but no thread ever reads
// it, so `led_state.set_yellow()` never runs. This is the same failure class ExProtocol
// already solved (see `exprotocol_task.rs`'s `PolledTcpTransport` comment) by never spawning
// a per-connection thread at all -- one thread polls everything itself. Applying the same
// fix here: a single non-blocking listener plus at most one non-blocking client stream
// (still "latest connection wins", the same design the old per-connection version already
// had via its `current` slot), both polled from this one thread's own loop. Zero per-
// connection thread spawns means zero chance of THIS specific failure mode recurring,
// regardless of how tight heap gets.
fn spawn_mic_server(lamp_handle: Arc<tuya::LampHandle>, ui_state: Arc<Mutex<screen::UiState>>, led_state: Arc<led::LedState>) {
    std::thread::Builder::new()
        .name("mic-srv".into())
        .stack_size(6144)
        .spawn(move || {
            use std::io::Read;
            use std::net::TcpListener;
            loop {
                let listener = match TcpListener::bind("0.0.0.0:9877") {
                    Ok(l) => l,
                    Err(e) => { warn!("[mic-srv] bind err: {e}, retrying"); FreeRtos::delay_ms(2000); continue; }
                };
                if let Err(e) = listener.set_nonblocking(true) {
                    warn!("[mic-srv] set_nonblocking failed: {e}, retrying bind");
                    FreeRtos::delay_ms(2000);
                    continue;
                }
                info!("[mic-srv] listening on :9877 (single polled thread, no per-connection spawn)");

                let mut current: Option<std::net::TcpStream> = None;
                // On this thread's own stack, not the heap -- avoids the whole class of bug
                // fixed elsewhere tonight (ota.rs/web.rs's unguarded heap allocations). 256
                // bytes comfortably covers the short tagged lines ("m:0:machine") this
                // connection ever actually receives.
                let mut linebuf = [0u8; 256];
                let mut linelen: usize = 0;
                let mut readbuf = [0u8; 64];
                let mut since_hwm_log = Duration::ZERO;

                loop {
                    // Accept any newly-waiting connection - replaces the current one
                    // immediately (dropping it closes its fd), same "only one live client"
                    // design as before, just with no second thread to explicitly shut down.
                    match listener.accept() {
                        Ok((s, addr)) => {
                            if let Err(e) = s.set_nonblocking(true) {
                                warn!("[mic-srv] new conn set_nonblocking failed: {e}");
                            } else {
                                info!("[mic-srv] {addr} connected");
                                current = Some(s);
                                linelen = 0;
                            }
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                        Err(e) => warn!("[mic-srv] accept err: {e}"),
                    }

                    if let Some(stream) = current.as_mut() {
                        match stream.read(&mut readbuf) {
                            Ok(0) => {
                                info!("[mic-srv] disconnected");
                                current = None;
                            }
                            Ok(n) => {
                                for &b in &readbuf[..n] {
                                    if b == b'\n' {
                                        if let Ok(line) = std::str::from_utf8(&linebuf[..linelen]) {
                                            handle_mic_line(line, &lamp_handle, &ui_state, &led_state);
                                        }
                                        linelen = 0;
                                    } else if linelen < linebuf.len() {
                                        linebuf[linelen] = b;
                                        linelen += 1;
                                    }
                                }
                            }
                            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                            Err(e) => {
                                warn!("[mic-srv] read err: {e}");
                                current = None;
                            }
                        }
                    }

                    FreeRtos::delay_ms(20);
                    since_hwm_log += Duration::from_millis(20);
                    if since_hwm_log >= Duration::from_secs(60) {
                        since_hwm_log = Duration::ZERO;
                        let hwm = unsafe { esp_idf_sys::uxTaskGetStackHighWaterMark(std::ptr::null_mut()) };
                        info!("[mic-srv] stack high-water-mark: {hwm} bytes free");
                    }
                }
            }
        })
        .ok();
}

/// Milliseconds since boot via esp_timer (same source the clock uses).
fn millis() -> u64 {
    (unsafe { esp_idf_sys::esp_timer_get_time() } / 1000) as u64
}

