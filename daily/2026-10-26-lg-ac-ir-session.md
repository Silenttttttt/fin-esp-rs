# Session log — Fin-ESP LG AC over IR (2026-10-26 → 2026-10-27)

Controlling Muni's LG Dual Inverter split AC (remote **AKB30086214**) from the ESP32, end to end:
capture → decode → transmit → full control → MCP + docs. Chronological summary of the live session.

## 1. Capture rig

- Goal: record every **unique** AC button press, in order.
- Bench ESP32 (`/dev/ttyUSB0`, MAC `00:70:07:84:c1:64`, confirmed via `esptool read-mac`): flashed a
  raw-edge capture sketch (`esp32-tests/ir_receiver_test/ac_capture/ac_capture.ino`, KY-022 on GPIO4),
  PC-side `capture.py` keeping only unique frames (symbol-class dedup) → `captures/unique_codes.{md,jsonl}`.
- Bench receiver read LOW (stuck) → wiring/board issues; pivoted to Fin-ESP (already wired, receiver on
  **GPIO32**).

## 2. Fin-ESP brownout rule (learned the hard way)

- **Fin-ESP browns out on USB serial at high baud.** A `read-flash` dump at 921600 then 460800 dropped it
  off USB mid-read, twice. Muni: "you browned out it again dude... ur supposed to use OTA."
- Rule, now in README + memory: **flash Fin-ESP over WiFi OTA only** (`./flash.sh 192.168.1.240` / raw
  4-byte-length + image on TCP 3232); 115200 if USB ever unavoidable. Full 4 MB backup saved to
  `Fin-ESP/backups/fin-esp-full-2026-10-26-a.bin` (single read).

## 3. WiFi capture firmware on Fin-ESP

- Temporary capture firmware (`ac_capture_wifi/`) installed via Fin-ESP's **own OTA** into slot `ota_1`;
  Fin-ESP's real firmware stayed intact in `ota_0`; `GET /revert` switches back. Everything over WiFi, nothing
  over USB. `capture.py --url` polls `/frames` over HTTP.

## 4. Decode — it's LG2

- Protocol: **LG2**, 28-bit, header ~3200/9900 µs, pulse-distance. Frame = `0x88` sig + 16-bit cmd + 4-bit
  checksum (sum of the 4 cmd nibbles & 0xF). Source: IRremoteESP8266 `ir_LG.h` + Arduino-IRremote `ac_LG.h`.
- Example: `0x8800347` = cool 18 °C fan max. Fields: Sign:8 | Power:2 | flags | **Running:1** | Mode:3 |
  Temp:4(C-15) | Fan:4 | Sum:4.
- "Fire and forget" — IR is **one-way**, AC has no transmitter, only a beep as ack.

## 5. Transmit + the two real bugs

- Bench board → sender firmware (RMT 38 kHz, KY-005 IR LED on GPIO33), then moved to Fin-ESP (GPIO2/D2,
  mirrored onto GPIO4 + GPIO25).
- **AC ignored every frame at first.** Ruled out protocol by replaying the remote's *exact* captured
  timings (AC still ignored) and by looping Fin-ESP's own sends back through its receiver (frames left the
  LED bit-perfect). Two stacked causes:
  1. **Timing** — generic library values were 4–5% off this remote. Calibrated to the remote's measured
     frames: header **3050/9450**, mark 460, 0-space 540, 1-space 1540. **This was the fix that made it work.**
  2. **Weak LED** — KY-005 has no driver transistor (its middle/VCC pin is unconnected), so the LED only
     gets one GPIO's current, ~10× dimmer than the real remote. Mitigated: max GPIO drive + 3 pins paralleled
     onto the one LED. Phone-camera test confirmed "on but weak."
- After calibration: **AC responded.** Confirmed by Muni (off/on/temp all work).

## 6. Full control on Fin-ESP (`src/ac.rs`)

- State machine: power/mode/temp/fan/swing_v/swing_h, persisted to NVS (IR can't read back, so Fin-ESP's
  saved state **is** the AC's state). One sender thread, coalesces changes (SETTLE_MS), no retries.
- **Design rules** (why state stays in sync by construction): absolute codes only (never toggles); every
  settings frame carries the full state; timers run on Fin-ESP, not the AC; nothing transmitted at boot;
  LG running-flag handled (power-on vs while-running).
- Blue button (GPIO27, was media play/pause) → AC on (cool 21 °C) / off.
- Web dashboard card: on/off, −/+ temp, presets (Comfort/Jet/Night/Dry), "turn off in" timer (runs on
  Fin-ESP), and under "More settings": mode, fan, airflow, auto-clean, energy saving, power limit, show power.
- Self-reception noise fixed: NEC remote reader ignores Fin-ESP's own AC frames while transmitting.

## 7. Hidden features (tested live on the unit)

| Feature | Code | Result on this unit |
|---|---|---|
| Auto-clean on | `0x88C00B7` | ✅ 1 beep, accepted (dries coil after off) |
| Show power draw | `0x88C0466` | ✅ display showed "0.0 kW" (compressor idle) |
| Ion / plasma | `0x88C000C` | ❌ 4 beeps — unsupported on this unit |
| Outdoor-unit quiet | `0x88C0A6C` | ❌ 4 beeps — unsupported |
| Energy saving / power limit 100/80/60/40% | `0x8810045/56`, `0x88C07F2/7D0/7E1/0804` | added (remote's own Energy Ctrl = `0x88C07D0`) |

- Decoded one of the remote's unknown codes: `0xC07D` = Energy Ctrl 80%.
- Full reference written: **`fin-esp-rs/docs/LG_AC_IR.md`** (protocol, timing, every code + test result,
  remote button map, unknowns, sources). Linked from README.

## 8. MCP — Fin-ESP got its own

- Fin-ESP's MCP previously lived *inside Jarvis* (`Jarvis/src/tools/finEspTools.ts`). Added AC tools there
  first (live-verified over `/mcp/native-bridge`, 1332/1332 tests), then created **Fin-ESP's own standalone
  MCP server**: `fin-esp-rs-work/fin-esp-mcp/index.mjs`, registered user-scope as `fin-esp` (like `hivemind`).
  18 tools (lamp, LEDs, AC: status/power/set/preset/sleep/airflow/features/show_power/resend). Tool names
  match Jarvis's copy = drop-in. Runs on the PC, not the chip (ESP heap too small for TLS/MCP).

## 9. Going further — rethink (discussed, not started)

- "Can we go two-way / automate for real?" → **[anszom/rethink](https://github.com/anszom/rethink)**
  (GPL-2.0): a fully local fake ThinQ cloud. Re-pairs the AC's WiFi via its normal SoftAP (same as the LG
  app), module gets its cert from our server, talks MQTT locally, no LG contact. Gives **real state readback,
  room temp (0.5 °C), energy counter** — on LG wall splits like this one. The WiFi module just forwards
  cloud→device payloads to the AC mainboard UART (TLV protocol), so it reaches the mainboard's documented
  settings.
- **Firmware "update" question:** the FOTA path exists (`startDirectFota`/`modemFotaStart`/`fotaPgmSend`,
  `mdFota:Y`, `fwUpgradeInfo`) but is undocumented and signature-gated — only the holder of LG's signing key
  can push firmware. Not available to third parties.

## Muni's hard requirements (standing)

- **NO hardware modifications to the AC, ever** — no opening the unit, no flash dumps, no wired-controller port.
- **NO firmware updates** of any kind (WiFi module or mainboard); nothing that could touch the compressor or
  break the AC; no raw/unknown-code exploration. **Automation only.**
- Firmware (the IR work) "doesn't help" him directly — the value is automation via readback. rethink is the
  path for that, within the no-hardware-mods rule, pending his go-ahead.

## Open / pending

- rethink setup — needs: Muni's OK to re-pair AC WiFi, how the AC enters pairing mode, desktop joins its
  SoftAP (desktop is the always-on host; Fin-ESP stays the priority interface).
- Restore `rust_bench_test` firmware on the bench board (overwritten by capture/sender firmware today; rebuilt,
  ready to flash).
- Commit the Fin-ESP (`ac.rs`, `web.rs`, `persist.rs`, docs) + Jarvis (`finEspTools.ts`) + new `fin-esp-mcp`
  changes — not yet committed.
- Map this remote's extra flap positions; decide power-cut resend; Jet auto-end.

Task queue: `fin-esp-rs-work/plans/fin-esp-ac-tasks.md`. Memory: `project_lg_ac_ir_control.md`,
`feedback_fin_esp_flash_via_ota_not_usb.md`.
