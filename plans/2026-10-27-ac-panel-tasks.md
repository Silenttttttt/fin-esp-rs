# AC button + panel tasks (2026-10-27, OpenCloud card)

| ID | Task | Priority | Depends On | Status | Added | Notes |
|----|------|----------|------------|--------|-------|-------|
| A1 | Blue button: ignore a 2nd press within 1.5 s (one press was counted twice, 255 ms apart) | high | - | done (OTA'd) | 10-27 | AC_BTN_LOCKOUT_MS; presses at 20:39 were clean single toggles |
| A2 | Panel: OFF state obvious (badge, big OFF, dimmed) | high | - | done (OTA'd) | 10-27 | Muni's browser still showed the cached old page |
| A3 | Every AC control pressed while off turns the AC on (swing, auto-clean, energy saving, max power, auto-off timer) | high | - | done (OTA'd 20:47) | 10-27 | power_on_if_off(); IR-only extras now wait until the AC confirms it's on |
| A4 | Bigger, unmistakable power control (replace small switch) | high | A2 | done (OTA'd 20:47) | 10-27 | big green ON / grey OFF power button, green card border |
| A5 | Advanced rows show the AC's real settings, read over WiFi (0x321 swing v, 0x322 swing h, 0x20e auto-dry, 0x20d power limit) | high | - | done (OTA'd 20:47) | 10-27 | adopt_extras(); verified: swing_v corrected Up->Fixed to match the AC's 0x321=0 |
| A6 | Live watch of presses vs AC state for Muni | med | - | running | 10-27 | scratchpad acwatch.py |
| A8 | IR remote: 4/5/6 = old 7/8/9 colors; 7 = AC on/off, 8 = colder, 9 = warmer | high | - | done (OTA'd 20:47), untested with a real press | 10-27 | Muni: replace the middle row |
| A7 | Commit A1-A5 (Rubble 2 card committing A1/A2) | med | A3-A5 | done (8471538 + next commit) | 10-27 | |
