# Fin-ESP32 Wiring Reference

Source of truth: `src/config.rs` pin constants + the actual `PinDriver::input/output` calls
in `src/main.rs`. This is what the firmware expects — match physical wiring to this table.

## Active buttons — all on the same side of the board (left column, standard ESP32 DevKitC)

All: input, internal pull-up (button pulls LOW when pressed) — other leg goes to GND, no
external resistor needed.

| Color  | Function            | GPIO |
|--------|----------------------|------|
| Red    | Lamp toggle (Tuya)   | 12   |
| Black  | Flashes and clears the red LED (its OTHER feature, LCD screen-cycling, is dead since the LCD is disconnected — but the LED clear itself is real, intentional functionality, not a no-op) | 26 |
| Blue   | Media play/pause     | 27   |
| White  | Bright White preset  | 14   |
| Yellow | Warm Dim preset      | 13   |

Moved 2026-08-29: Media and Bright were originally on GPIO 27/19 and 4 (right side of the
board) — moved to 27/14 so all 5 button wires land on the same side as the others. Freed by
moving the LCD's I2C bus off 14/27 onto GPIO 22/23 instead (see below) — nothing else used
those two pins.

## Other buttons that exist in firmware but are NOT being wired right now

| Function | GPIO | Why skipped |
|----------|------|-------------|
| Chart    | 18   | Toggles volume pot enable — no effect, pot disconnected |

Still works exactly like the "black" button above if ever wired (flash+clear the red
LED, nothing else) — skipped here only because the user doesn't need a 6th button.

Removed 2026-08-30: the "Display" button (was GPIO 32, toggled LCD backlight-force-off) was
never physically wired anyway — GPIO32 was repurposed for the DHT22 sensor's data line
instead (see below), and the button's PinDriver was deleted from `main.rs` along with it. The
web-triggered `/action/display/*` route still works unchanged (it never depended on this pin).

## DHT22/AM2302 temp+humidity sensor

| Function     | GPIO |
|--------------|------|
| Data (1-wire)| 32   |

Single bidirectional data line (bit-banged, `input_output_od` with internal pull-up) — VDD to
3.3V, GND to GND, no other wiring needed.

## LEDs (output)

| Function                      | GPIO |
|-------------------------------|------|
| Green — WiFi connected        | 19   |
| Red — WiFi down / flash       | 33   |
| Yellow — mic mute state       | 5    |

Moved 2026-08-30: Green was originally GPIO 25 — moved to 19 (freed 2026-08-29 when the Media
button moved off it) so it lands on the correct/preferred side of the board.

Yellow is the one remaining exception to "everything on one side" — GPIO 5 is on the right
side, and the only free left-side pins (34/35) are input-only and can't drive an LED output at
all. Confirmed acceptable: it's a single wire, not worth relocating another pin over.

## I2C (LCD) — physically disconnected, kept enabled in firmware, not removed

| Function | GPIO |
|----------|------|
| SDA      | 22   |
| SCL      | 23   |

The LCD itself is still unplugged (unused) — `DISPLAY_TOGGLE_ENABLED = false` in `config.rs`.
The I2C bus is still fully initialized and enabled in code (explicitly NOT deleted or
feature-flagged off, per instruction to keep it available for later) — it was just moved off
GPIO 14/27 onto GPIO 22/23 so 14/27 could be reused for the Bright/Media buttons above. Since
nothing is physically connected either way, which exact pins the I2C bus claims doesn't
matter functionally. Skip 22/23 when rewiring unless the LCD is coming back.

## Volume potentiometer (ADC) — physically disconnected, not currently wired

| Function     | GPIO |
|--------------|------|
| Pot wiper    | 34   |

Removed (too noisy) — `POT_TOGGLE_ENABLED = false` in `config.rs`, replaced by the
keyboard's own volume control. Skip this pin too unless the pot is coming back.

## DHT22 / AM2302 temp+humidity sensor (wiring slot ready, not yet wired)

4-pin module: `VDD`, `DATA` (silkscreened "SDA" on this module, but it's DHT's own
single-wire digital protocol, not real I2C), `NC`, `GND`.

| Module pin      | Connects to        |
|-----------------|---------------------|
| VDD             | 3.3V                |
| DATA ("SDA")    | GPIO 32             |
| NC              | leave unconnected   |
| GND             | GND                 |

Moved 2026-08-29 from GPIO 21 (right side of the board) to GPIO 32 (left side, same side as
every other active pin) — GPIO 21's own left-side alternatives (34/35/36/39) are input-only
with no internal pull-up, which doesn't work for DHT22's single-wire protocol (the MCU needs
to both drive AND read that line). GPIO 32 is the one free, full-I/O-capable left-side pin
left — currently claimed in firmware by the unwired "Display" button (harmless overlap for
now, since nothing is physically on that pin yet). **When the actual DHT22 driver code gets
written, `btn_display`'s `PinDriver::input(peripherals.pins.gpio32, ...)` in `main.rs` needs
to move or go first**, or the two will conflict at compile time over the same pin. Firmware
support to actually read this sensor is not yet written — this is just the physical wiring
slot. The web UI already has a "Climate" card (Temp/Humidity, showing `--`) waiting for it.

## Notes

- Tuya lamp relay is controlled entirely over WiFi (LAN protocol to the lamp's own IP,
  `TUYA_DEVICE_IP` in `.env`) — no direct GPIO wiring to the lamp itself.
- Both current boards (old: `192.168.1.240`, new: on serial as of 2026-08-29) run
  identical firmware and both point at the same physical lamp — don't power both at once
  once the old board's rewiring is done and it comes back online, to avoid them fighting
  over the same Tuya connection.
