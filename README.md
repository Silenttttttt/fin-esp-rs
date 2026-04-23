# Fin-ESP

ESP32 firmware that displays live financial data (BTC, SOL, Gold, Oil, USD/BRL) and local weather on a 20×4 HD44780 LCD. Written in Rust using `esp-idf-svc`.

## Features

- **Live prices** — BTC, SOL, Gold (WTI), Oil, USD/BRL fetched every 45 seconds
- **Line chart** — 8-point price history rendered via custom CGRAM glyphs with diagonal pixel drawing
- **Weather** — current temperature and condition from Open-Meteo
- **Tuya lamp control** — toggle/dim a Tuya LAN smart bulb (warm, bright white, on/off)
- **Sand animation** — falling-sand physics sim on idle
- **Media button** — physical button sends play/pause signal to a laptop over the local network
- **OTA updates** — flash new firmware over WiFi without USB

## Hardware

| Component | Details |
|-----------|---------|
| MCU | ESP32 DevKit V1 |
| Display | 20×4 HD44780 LCD via PCF8574 I2C backpack |
| I2C | SDA → GPIO 14, SCL → GPIO 27 |
| Buttons | GPIO 26 (screen), 12 (lamp), 32 (display on/off), 13 (warm), 4 (bright), 18 (chart), 19 (media) |
| LEDs | GPIO 25 (green = WiFi OK), GPIO 33 (red = WiFi down) |

## Project structure

```
fin-esp-rs/      Rust firmware (ESP-IDF)
  src/
    main.rs      Main loop, button handling, OTA server, media server
    config.rs    Pin assignments, timing constants, API URLs
    api.rs       Market data fetching (HTTP)
    chart.rs     Line chart renderer (CGRAM glyph engine)
    cgram.rs     CGRAM slot manager with Hamming-distance dedup
    lcd.rs       HD44780 driver via PCF8574 I2C
    ticker.rs    Price ticker screen
    tuya/        Tuya LAN protocol 3.5 (AES-128, TCP)
    sand.rs      Falling-sand simulation
    ota.rs       OTA update server (TCP :3232)
    ...
  flash.sh       Build + flash (USB or OTA)
  .env.example   Environment variable template

laptop/          Host-side media bridge
  play_pause_server.py   Connects to ESP32:9876, runs playerctl on signal
  play-pause.service     systemd user service

tools/           Dev utilities
  lcd_glyph_visualize.py   Preview 5×8 CGRAM glyphs in terminal
  image_to_5x8_grid.py     Convert image to 5×8 pixel grid
  png_to_lcd_glyphs.py     Convert PNG to CGRAM glyph bytes
```

## Getting started

### Prerequisites

- [Rust + esp-rs toolchain](https://github.com/esp-rs/esp-idf-template)
- `cargo`, `esptool.py`

### Configure

```bash
cp fin-esp-rs/.env.example fin-esp-rs/.env
# Edit .env — fill in WiFi credentials, Tuya device key/IP, laptop IP
```

`.env` fields:

```
WIFI_SSID=YourNetwork
WIFI_PASS=YourPassword

TUYA_KEY=<16 ASCII chars from Tuya developer portal>
TUYA_IP=<LAN IP of your Tuya bulb>

LAPTOP_IP=<LAN IP of your laptop, for media button>
```

### Build and flash

```bash
cd fin-esp-rs

# USB
./flash.sh usb /dev/ttyUSB0

# OTA (after first USB flash)
./flash.sh <ESP32_IP>
```

### Media bridge (laptop side)

The media button on GPIO 19 signals the laptop to run `playerctl play-pause`. The laptop connects **to** the ESP32 (not the other way around), so no firewall rules are needed on either side.

```bash
# Install playerctl (Arch/Manjaro)
sudo pacman -S playerctl

# Install the systemd user service
cp laptop/play-pause.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now play-pause
```

Set your ESP32's IP via an env file (no need to edit any source):

```bash
mkdir -p ~/.config/fin-esp
echo 'ESP_IP=192.168.1.x' > ~/.config/fin-esp/env
```

The service file uses `%h` for your home directory and loads `~/.config/fin-esp/env` automatically (the `-` prefix means it's optional — omitting it just uses the default).

## Configuration

All hardware pins, API URLs, and timing constants are in [fin-esp-rs/src/config.rs](fin-esp-rs/src/config.rs). Secrets (WiFi, Tuya key, IPs) are injected at compile time via `.env` using `env!()` — they never appear in source.

## License

MIT
