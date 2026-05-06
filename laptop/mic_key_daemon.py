#!/usr/bin/env python3
"""
Evdev key daemon — handles bare Insert on GNOME/Wayland (can't be grabbed as a
custom shortcut). Runs as a user service. Requires 'input' group membership.

Keys handled:
  Insert → mic toggle (pactl + ESP32 LED)
"""
import os
import queue
import socket
import subprocess
import threading
import logging
import sys
import time
import evdev
from evdev import ecodes

ESP_IP   = os.environ.get('ESP_IP', '192.168.1.240')
ESP_PORT = 9877

WATCHED_KEYS = {ecodes.KEY_INSERT}

logging.basicConfig(
    level=logging.INFO,
    format='%(asctime)s %(levelname)s %(message)s',
    stream=sys.stdout,
)

# ── Mic state ──────────────────────────────────────────────────────────────────
_src   = subprocess.check_output(['pactl', 'get-default-source']).decode().strip()
_muted = subprocess.check_output(['pactl', 'get-source-mute', _src]).decode().split()[1] == 'yes'

# ── ESP sender — latest-wins, never blocks the mic worker ─────────────────────
_esp_event = threading.Event()

def _esp_sender():
    while True:
        _esp_event.wait()
        _esp_event.clear()
        state = 'm:0' if _muted else 'm:1'
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

threading.Thread(target=_esp_sender, daemon=True, name='esp-sender').start()

# Sync LED to actual state at startup
_esp_event.set()
logging.info('startup: %s', 'muted' if _muted else 'unmuted')

# ── Burst-collapse worker ──────────────────────────────────────────────────────
def _burst_worker(q, action):
    while True:
        q.get()
        count = 1
        while True:
            try:
                q.get_nowait()
                count += 1
            except queue.Empty:
                break
        if count % 2 == 0:
            continue
        action(count)

_mic_q = queue.Queue()

def _mic_action(count):
    global _muted
    _muted = not _muted
    subprocess.Popen(['pactl', 'set-source-mute', _src, '1' if _muted else '0'],
                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    logging.info('%s (burst=%d)', 'muted' if _muted else 'unmuted', count)
    _esp_event.set()

threading.Thread(target=_burst_worker, args=(_mic_q, _mic_action), daemon=True, name='mic-worker').start()

_key_queue = {
    ecodes.KEY_INSERT: _mic_q,
}

# ── evdev listener ────────────────────────────────────────────────────────────
# _held: keys currently physically down (across all devices).
# A key can only fire once per press — key-up resets it.
# Also dedup across HID nodes with a 50 ms window on key-down.
_held      = set()
_last_down: dict[int, float] = {}
_state_lock = threading.Lock()

_watched_paths: set[str] = set()
_watched_lock = threading.Lock()

def watch_device(dev):
    logging.info('watching %s (%s)', dev.path, dev.name)
    try:
        for event in dev.read_loop():
            if event.type != ecodes.EV_KEY or event.code not in _key_queue:
                continue
            if event.value == 0:            # key-up: allow next press
                with _state_lock:
                    _held.discard(event.code)
                continue
            if event.value != 1:            # ignore standard repeat (value=2)
                continue
            now = time.monotonic()
            with _state_lock:
                if event.code in _held:
                    continue                # key still held — repeat, skip
                if now - _last_down.get(event.code, 0) < 0.05:
                    continue                # HID duplicate from another node
                _held.add(event.code)
                _last_down[event.code] = now
            _key_queue[event.code].put(1)
    except Exception as e:
        logging.warning('device %s lost: %s', dev.path, e)
    finally:
        with _watched_lock:
            _watched_paths.discard(dev.path)

def find_keyboards():
    devs = []
    for path in evdev.list_devices():
        try:
            d = evdev.InputDevice(path)
            if 'Consumer Control' in d.name:
                continue    # media-only interface — sends Insert with firmware delay, skip it
            caps = d.capabilities()
            if ecodes.EV_KEY in caps and any(k in caps[ecodes.EV_KEY] for k in WATCHED_KEYS):
                devs.append(d)
        except Exception:
            pass
    return devs

def _start_watching(dev):
    with _watched_lock:
        if dev.path in _watched_paths:
            return
        _watched_paths.add(dev.path)
    t = threading.Thread(target=watch_device, args=(dev,), daemon=True)
    t.start()

def _scanner():
    """Periodically rescan for new/reconnected keyboards."""
    while True:
        time.sleep(5)
        for dev in find_keyboards():
            with _watched_lock:
                already = dev.path in _watched_paths
            if not already:
                logging.info('new device found: %s (%s)', dev.path, dev.name)
                _start_watching(dev)

if __name__ == '__main__':
    keyboards = find_keyboards()
    if not keyboards:
        logging.error('no device with watched keys — check input group membership')
        sys.exit(1)
    for dev in keyboards:
        _start_watching(dev)
    logging.info('listening on %d device(s)', len(keyboards))
    threading.Thread(target=_scanner, daemon=True, name='scanner').start()
    # Keep main thread alive
    while True:
        time.sleep(60)
