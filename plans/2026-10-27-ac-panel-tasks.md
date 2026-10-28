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
| A9 | Keep every setting across a Fin-ESP restart and an off/on from Fin-ESP | high | - | done (OTA'd 21:13), verified | 10-27 | restore_to_ac() on each new AC session; never adopt settings while off; re-apply after power-on; auto mode now counts the setpoint. Tests: restart pushed the saved 30 back (AC was at 24); off/on kept auto/30/quiet/swing middle/auto-clean/full; reboot kept every setting before the link came up |
| A10 | Vertical swing over WiFi (0x321) instead of IR only | high | A9 | done (OTA'd 21:13), verified | 10-27 | off->0, middle->4, off->0, each confirmed within 4 s |
| A11 | Dashboard reloads itself after a Fin-ESP restart (/status boot id) | med | - | done | 10-27 | Muni's tab kept showing the pre-update layout |
| A12 | Unexplained AC-side changes: 30->22 (adopted ~21:10) and swing middle->up across the 21:13 restart | med | - | open, asked Muni | 10-27 | Fin-ESP adopted both from the AC and didn't send them; LG remote? |
| A13 | Panel shows a click instantly (acPatch), then re-reads at 0.7/1.5/2.8 s; SETTLE_MS 300 -> 150 | high | - | done (OTA'd 21:20), verified | 10-27 | highlight changes 1 ms after the click (was: after the board answered plus up to a 3 s poll) |
| A14 | Main-state retries over WiFi while the link is up (were IR) | high | - | done (OTA'd 21:20) | 10-27 | 28C auto failed after IR retries. Also: in AUTO the AC seems to move its own setpoint (30->22 with no command), so exact temps hold in cool/heat |
| A15 | Commit A9-A14 (Rubble 2 card is gone) | med | - | pending, needs Muni's OK | 10-27 | uncommitted on top of cebeed7 |
| A16 | Blue button off/on keeps every setting (Muni saw 20C + "random settings") | high | A9 | done, verified with Muni 21:41 | 10-27 | His 21:40-21:41 presses: off confirmed, on came back auto/25/quiet/vane Up, and Muni confirmed the temp was right. Not reproduced: nothing Fin-ESP sent or saw contained 20C. Louver sweeping down then back up on power-on is the LG unit's own behaviour |
| A17 | Vane encoding (0x321) checked against the physical louver with Muni | med | A10 | done | 10-27 | 0 = fixed (down), 1 = fixed up, 100 = sweep. The report can be stale after a non-WiFi change (it read 1 while sweeping); the LG remote is suspected, unconfirmed |
