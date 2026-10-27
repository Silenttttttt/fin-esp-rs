# Fin-ESP LG AC control - task queue (2026-10-26)

Goal: Fin-ESP is the ONLY controller of the LG AC (remote AKB30086214, LG2 IR protocol), same model as
the Tuya lamp. Full persistent state that mirrors the AC, blue button (was media play/pause) = AC
on/off at 21C, full web UI for everything else. Flash via OTA only (USB browns it out).

| ID | Task | Priority | Depends On | Status | Added | Notes |
|---|---|---|---|---|---|---|
| 1 | Revert Fin-ESP from capture fw (ota_1) to its own fw (ota_0) | high | Fin-ESP powered | done 23:1x | 10-26 | `curl 192.168.1.240/revert`; Fin-ESP offline since ~22:02 |
| 2 | Wiring: IR LED on GPIO2 (S), 5V, GND; doc in WIRING.md | high | - | done (doc); Muni wiring |  10-26 | moved to GPIO2 (D2) per Muni |
| 3 | ac.rs: LG2 encode + RMT 38kHz TX, AcState {power,mode,temp,fan,swing_v,swing_h}, NVS persist | high | - | done (builds) | 10-26 | no toggle codes ever sent (light/swing toggle) |
| 4 | Blue button (GPIO27): media play/pause -> AC power toggle, on = cool 21C | high | 3 | done (builds) | 10-26 | |
| 5 | Web UI: AC card with all controls + /ac/* routes + JSON state | high | 3 | done (builds, JS syntax ok) | 10-26 | |
| 6 | Build, OTA to Fin-ESP (after #1), verify live: web state, button, AC responds | high | 1,3,4,5 | done (fw); AC response blocked on #10 |
| 7 | Restore rust_bench_test firmware on bench board (overwritten by ac_capture/ac_send today) | medium | 6 | ready (built) | 10-26 | ask: keep ac_send there or restore | flash: bootloader 0x1000, partition-table 0x8000, erase otadata 0x10000, app 0x20000 @115200; wait until Fin-ESP AC verified
| 8 | Diagnose AC not responding | high | 6 | done 23:30 | 10-26 | sniffed remote off/on: fixed frames, no counter; our off == remote off bit-for-bit (loopback); replay of remote's exact raw frames also ignored -> NOT protocol. Phone camera: LED on but weak. KY-005 middle pin is NC, LED driven by GPIO only |
| 9 | Ignore own AC frames in NEC remote reader (TX blanking) | medium | - | done, deployed 23:30 | 10-26 | KY-022 onboard LED still blinks (hardware); tape shields it |
| 10 | IR LED drive power: NPN transistor (S8050/2N2222/BC547) + 220R base + parallel 220Rs from 5V, OR mount LED at the AC | high | Muni hardware | blocked | 10-26 | the real fix; protocol side verified |
| 11 | Calibrate TX timing to real remote + paralleled GPIO4/25 onto same LED S pin | high | - | done, deployed 23:4x | 10-26 | loopback now header 3113/9399 vs remote 3097/9420; awaiting Muni's AC result + jumpers |
| 12 | Simplified UI + presets (Comfort/Jet/Night/Dry) + "turn off in" timer | high | - | done, deployed 23:59 | 10-26 | sleep timer runs on Fin-ESP (persisted unix deadline), sends OFF itself |
| 13 | LG running flag (bit15 set while on, clear on power-on frame) + Jet 0x8810089 | high | - | done, deployed, loopback-verified 0x8808855 | 10-26 | source: Arduino-IRremote ac_LG.h LG_SWITCH_ON_MASK + Muni's captures |
| 14 | Map this remote's flap cycle (0x1304-06, 0x130B-0F seen) to plain positions, replace Vane pills | medium | Muni watching AC | todo | 10-26 | 0x1314/0x1315 = wall swing on/off per ac_LG.h |
| 15 | Optional: re-send full state when AC power returns after outage | low | Muni yes/no | todo | 10-26 | |
| 16 | AC tools in fin-esp MCP (Jarvis finEspTools.ts): status/power/set/preset/sleep/airflow | high | - | done, live 00:07 | 10-27 | verified over /mcp/native-bridge |
| 17 | Hidden AC features (codes the remote lacks) research | medium | - | in progress (background agent) | 10-27 | then test each on the AC with Muni watching |
| 18 | Jet auto-end on Fin-ESP | medium | jet duration | todo | 10-27 | |
| 19 | Fin-ESP's own MCP server (fin-esp-mcp/index.mjs, user scope, 16 tools incl. AC) | high | - | done 00:2x | 10-27 | tool names match Jarvis's in-process fin-esp server (drop-in) |
| 20 | Switch Jarvis to consume fin-esp-mcp instead of its own finEspTools.ts copy | medium | Muni yes/no | todo | 10-27 | avoids two drifting copies |
| 21 | Test hidden features on the real AC (ion, outdoor-quiet, auto-clean, energy saving, kW display) | medium | Muni watching | todo | 10-27 | codes from research agent, all absolute except kW (momentary) |
| 22 | Hidden-features: auto-clean (works), show power (works), ion + outdoor-quiet (4 beeps, unsupported); added auto-clean/energy saving/power limit/show power to fw + UI + fin-esp MCP; full doc fin-esp-rs/docs/LG_AC_IR.md | high | - | done, deployed 00:17 | 10-27 | |
| 23 | [HARD REQ: no AC hardware mods, no firmware] rethink (anszom/rethink, GPL-2.0): local fake ThinQ cloud -> real two-way state, room temp, energy | high | Muni: OK to re-pair AC WiFi, pairing-mode method, which machine joins SoftAP | blocked | 10-27 | Muni's rule: automation only, NO firmware updates (module or mainboard), no compressor-affecting/raw unknown commands; only app-level documented settings |
| 24 | Code-space beep scan for undocumented codes | low | - | dropped | 10-27 | Muni: don't risk anything that could break it |
