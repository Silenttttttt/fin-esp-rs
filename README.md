# Fin-ESP

ESP32 firmware in Rust that drives a 20×4 HD44780 LCD (live financial data, weather, a falling-sand
simulation), controls a Tuya LAN smart bulb, and fully controls an LG air conditioner — both over
IR and, more interestingly, over WiFi by acting as the AC's own local "cloud" (no PC, no third-party
server, no internet). Built with `esp-idf-svc`.

## Features

- **Live prices** — BTC, SOL, Gold (WTI), Oil, USD/BRL fetched every 45 seconds
- **Line chart** — 8-point price history rendered via custom CGRAM glyphs with diagonal pixel drawing
- **Weather** — current temperature and condition from Open-Meteo
- **Tuya lamp control** — toggle/dim a Tuya LAN smart bulb (warm, bright white, on/off)
- **LG air conditioner control** — full IR remote replacement (mode, temp, fan, swing, presets,
  timers, jet, auto-clean, energy limit) *and* genuine two-way WiFi control/telemetry with **zero
  cloud or PC dependency** — see [AC control](#lg-air-conditioner-control) below
- **Sand animation** — falling-sand physics sim on idle
- **Media button** — physical button sends play/pause signal to a laptop over the local network
- **OTA updates** — flash new firmware over WiFi without USB
- **MCP server** — control the lamp and AC from any MCP-aware tool (Claude, etc.), see `fin-esp-mcp/`

## LG air conditioner control

Fin-ESP replaces the AC's remote **and** its cloud connection entirely — two independent paths:

### 1. IR (`src/ac.rs`)
Full state machine mirroring everything the real remote does: power, mode, temperature, fan speed,
vertical/horizontal swing, presets (Comfort/Jet/Night/Dry), a Fin-ESP-side sleep timer, auto-clean,
energy saving, and power-limit. State persists across reboots. Protocol details, every known IR code,
and how they were reverse-engineered: [`docs/LG_AC_IR.md`](fin-esp-rs/docs/LG_AC_IR.md).

### 2. WiFi — Fin-ESP *is* the AC's cloud (`src/ac_cloud/`)
LG's AC WiFi module normally phones home to LG's ThinQ cloud for app control and telemetry. Instead,
Fin-ESP's `ac_cloud` module runs its own TLS server, a minimal single-client MQTT broker, and LG's
"clip" protocol handler — the AC connects to Fin-ESP directly, believing it's talking to its real
cloud (same CA the device already trusts, so **no re-pairing needed**). This gives real two-way
telemetry the remote never exposes: room temperature, outdoor temperature, outdoor coil temperature,
live compressor frequency, EEV (expansion valve) opening, and error codes — all read at
`GET /ac/cloud`. Runs in its own heap-gated thread, pinned off the core the IR receiver needs, with a
kill switch (`POST /action/ac/cloud?on=0/1`) that survives reboots. Design notes:
[`docs/AC_CLOUD_PLAN.md`](fin-esp-rs/docs/AC_CLOUD_PLAN.md). Bench-test harness (a scripted fake AC
replaying real captured protocol frames, used to verify this before ever touching a real AC's live
connection) lives in `tools/ac_cloud/`.

Both paths write to the same persisted state, so the dashboard and MCP server always reflect what's
actually true.

## Hardware

| Component | Details |
|-----------|---------|
| MCU | ESP32 DevKit V1 |
| Display | 20×4 HD44780 LCD via PCF8574 I2C backpack |
| I2C | SDA → GPIO 14, SCL → GPIO 27 |
| Buttons | GPIO 26 (screen), 12 (lamp), 32 (display on/off), 13 (warm), 4 (bright), 18 (chart), 19 (media) |
| LEDs | GPIO 25 (green = WiFi OK), GPIO 33 (red = WiFi down / IR remote decode flash) |
| IR receiver (NEC remote) | GPIO 32 |
| IR transmitter (AC, RMT peripheral) | GPIO 2 |

## Project structure

```
fin-esp-rs/      Rust firmware (ESP-IDF)
  src/
    main.rs      Main loop, button handling, OTA server, media server
    config.rs    Pin assignments, timing constants, API URLs, feature flags
    api.rs       Market data fetching (HTTP)
    chart.rs     Line chart renderer (CGRAM glyph engine)
    cgram.rs     CGRAM slot manager with Hamming-distance dedup
    lcd.rs       HD44780 driver via PCF8574 I2C
    ticker.rs    Price ticker screen
    tuya/        Tuya LAN protocol 3.5 (AES-128, TCP)
    sand.rs      Falling-sand simulation
    ota.rs       OTA update server (TCP :3232)
    ac.rs        LG AC state machine + IR transmit (RMT)
    ir_remote.rs NEC IR receiver for the physical lamp remote
    ac_cloud/    Fin-ESP as the AC's own local "cloud": TLS server, 1-client MQTT
                 broker, LG "clip" protocol, TLV codec (mod.rs, tls.rs, mqtt.rs,
                 clip.rs, tlv.rs)
    web.rs       HTTP dashboard + JSON API
    ...
  docs/
    LG_AC_IR.md       IR protocol, every known code, reverse-engineering notes
    AC_CLOUD_PLAN.md  ac_cloud design/architecture
  flash.sh       Build + flash (USB or OTA)
  .env.example   Environment variable template

fin-esp-mcp/     Standalone MCP server — control the lamp and AC from any MCP client
  index.mjs

tools/ac_cloud/  Bench-test harness for ac_cloud (scripted fake AC, regression script,
                 guarded OTA helper) — used to verify the WiFi path without ever
                 touching a real AC's live connection

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

# OTA — the normal way, always use this
./flash.sh <ESP32_IP>

# USB — only for a first flash / unbrick
./flash.sh usb /dev/ttyUSB0
```

**Flash over OTA, not USB.** This board browns out on USB serial at high baud (any esptool
operation at 460800/921600, including a read-only `read-flash` dump). If USB is truly
unavoidable, use `--baud 115200`.

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

### MCP server

`fin-esp-mcp/` is a small standalone [Model Context Protocol](https://modelcontextprotocol.io) server
that exposes lamp and AC control as tools for any MCP-aware client (Claude Code, etc.):

```bash
cd fin-esp-mcp
npm install
# Point it at your board (defaults to 192.168.1.240)
FIN_ESP_URL=http://<ESP32_IP> node index.mjs
```

## Configuration

All hardware pins, API URLs, and timing constants are in [fin-esp-rs/src/config.rs](fin-esp-rs/src/config.rs). Secrets (WiFi, Tuya key, IPs) are injected at compile time via `.env` using `env!()` — they never appear in source.

## License

MIT
