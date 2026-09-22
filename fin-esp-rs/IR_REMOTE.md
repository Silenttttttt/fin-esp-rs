# IR remote (KY-022) — button mapping

Receiver wired to GPIO4 (see `src/ir_remote.rs` for why that pin specifically — GPIO2/5 are
ESP32 boot-strapping pins and were ruled out live). Full bench-test discovery history
(wiring, pinout capture/verification, the real root cause behind an earlier debugging
saga) lives in `esp32-tests/ir_receiver_test/` in the sibling Fin-ESP repo.

| Button | Action |
|---|---|
| **ok** | Toggle lamp on/off |
| **up** | Brightness up (or color brightness, if a color is active) |
| **down** | Brightness down (or color brightness, if a color is active) |
| **left** | Desktop volume down (~7%/press) |
| **right** | Desktop volume up (~7%/press) |
| **\*** | Warm-dim preset |
| **#** | Bright-white preset |
| **0** | Media play/pause |
| **1** | Mirrors the physical black button (GPIO14) — no lamp action, just the flash |
| **2** | Lamp color: red |
| **3** | Lamp color: orange |
| **4** | Lamp color: yellow-green |
| **5** | Lamp color: green |
| **6** | Lamp color: teal |
| **7** | Lamp color: cyan-blue |
| **8** | Lamp color: blue |
| **9** | Lamp color: purple |

Every successfully decoded button press gives an 80ms red LED flash, the same
acknowledgment the physical buttons already give — fires even for button 1, which has no
other effect.

## Known real bug already fixed here

`LampHandle::queue_brightness_temp()` always forces white mode (`"21":"white"` — see
`tuya/mod.rs`'s `poll()`, target 4), so stepping brightness while a color was active used
to silently discard the color back to white. Fixed by tracking whether a color or white
mode was last picked (via this same remote) and stepping the color's own V (brightness in
HSV) instead when a color is active — see `IN_COLOUR_MODE` in `src/ir_remote.rs`. Not
authoritative if something else (physical buttons, web UI) changes the mode in between,
but correct for the actual reported case: picking a color and adjusting its brightness
with this same remote.

Desktop volume is tracked locally on the ESP (`DESKTOP_VOLUME`, starts at ~50%) since
`queue_volume_for_machine()` takes an absolute value, not a delta — machine ID is this
desktop's hostname (`silent-ms7e56`, from `socket.gethostname()` in the laptop bridge
protocol).
