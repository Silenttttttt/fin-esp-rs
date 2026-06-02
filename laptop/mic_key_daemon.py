#!/usr/bin/env python3
"""
Mic LED sync daemon — keeps the ESP32 mic LED in sync with the actual
PulseAudio mic mute state. Runs as a user service.

Key actions (PgUp, PgDn, Ctrl+PgDn) are handled by GNOME keybindings:
  Page_Up         → mic toggle  (org.gnome.settings-daemon.plugins.media-keys mic-mute)
  Page_Down       → play/pause  (custom keybinding → playerctl play-pause)
  Ctrl+Page_Down  → lamp toggle (custom keybinding → fin_esp_lamp_toggle.sh)
"""
import os
import socket
import subprocess
import threading
import logging
import sys
import time

ESP_IP   = os.environ.get('ESP_IP', '192.168.1.240')
ESP_PORT = 9877

# Kill and restart pactl subscribe if no events arrive within this many seconds.
PA_WATCHDOG_SECS = 600

logging.basicConfig(
    level=logging.INFO,
    format='%(asctime)s %(levelname)s %(message)s',
    stream=sys.stdout,
)

# ── Mic state ──────────────────────────────────────────────────────────────────
_src   = subprocess.check_output(['pactl', 'get-default-source']).decode().strip()
_muted = subprocess.check_output(['pactl', 'get-source-mute', _src]).decode().split()[1] == 'yes'
_state_mutex = threading.Lock()

def _actual_muted() -> bool:
    try:
        out = subprocess.check_output(
            ['pactl', 'get-source-mute', _src],
            stderr=subprocess.DEVNULL, timeout=2,
        ).decode()
        return out.split()[1] == 'yes'
    except Exception:
        return _muted

# ── ESP sender — latest-wins ───────────────────────────────────────────────────
_esp_event = threading.Event()

def _esp_sender():
    while True:
        _esp_event.wait()
        _esp_event.clear()
        with _state_mutex:
            actual = _muted
        state = 'm:0' if actual else 'm:1'
        for attempt in range(4):
            try:
                s = socket.create_connection((ESP_IP, ESP_PORT), timeout=3)
                s.sendall(f'{state}\n'.encode())
                s.close()
                logging.info('ESP32 <- %s', state)
                break
            except Exception as e:
                logging.warning('ESP32 send failed (attempt %d): %s', attempt + 1, e)
                time.sleep(1)

# ── PulseAudio subscriber — catches all mic state changes ─────────────────────
_last_pa_event = time.time()   # updated on every line from pactl subscribe
_pa_proc_lock  = threading.Lock()
_pa_proc       = None          # current pactl subscribe Popen object

def _pa_subscriber():
    global _muted, _pa_proc, _last_pa_event
    while True:
        try:
            proc = subprocess.Popen(
                ['pactl', 'subscribe'],
                stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True,
            )
            with _pa_proc_lock:
                _pa_proc = proc
            _last_pa_event = time.time()
            for line in proc.stdout:
                _last_pa_event = time.time()
                if 'source' not in line or 'change' not in line:
                    continue
                time.sleep(0.05)  # let PulseAudio settle the change
                actual = _actual_muted()
                with _state_mutex:
                    if actual == _muted:
                        continue
                    _muted = actual
                logging.info('mic state synced from PA: %s', 'muted' if actual else 'unmuted')
                _esp_event.set()
        except Exception as e:
            logging.warning('pactl subscribe error: %s — restarting', e)
        finally:
            with _pa_proc_lock:
                _pa_proc = None
        time.sleep(2)

# ── Poller — catches mic changes even when pactl subscribe misses them ─────────
def _poller():
    global _muted
    while True:
        time.sleep(1)
        try:
            actual = _actual_muted()
            changed = False
            with _state_mutex:
                if actual != _muted:
                    _muted = actual
                    changed = True
            if changed:
                logging.info('mic state polled: %s', 'muted' if actual else 'unmuted')
                _esp_event.set()
        except Exception as e:
            logging.warning('poll error: %s', e)

# ── Watchdog — restarts pactl subscribe if it goes stale ──────────────────────
def _watchdog():
    while True:
        time.sleep(60)
        if time.time() - _last_pa_event > PA_WATCHDOG_SECS:
            with _pa_proc_lock:
                proc = _pa_proc
            if proc is not None:
                logging.warning('pactl subscribe stale (>%ds) — restarting', PA_WATCHDOG_SECS)
                proc.kill()

if __name__ == '__main__':
    threading.Thread(target=_esp_sender,    daemon=True, name='esp-sender').start()
    threading.Thread(target=_pa_subscriber, daemon=True, name='pa-sub').start()
    threading.Thread(target=_watchdog,      daemon=True, name='watchdog').start()
    threading.Thread(target=_poller,        daemon=True, name='poller').start()
    _esp_event.set()
    logging.info('startup: %s', 'muted' if _muted else 'unmuted')
    while True:
        time.sleep(60)
