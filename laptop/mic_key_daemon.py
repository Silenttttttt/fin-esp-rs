#!/usr/bin/env python3
"""
Evdev key daemon — handles keys that GNOME/Wayland can't grab as custom shortcuts.
Runs as a user service. Requires 'input' group membership.

Keys handled:
  Insert      → mic toggle (pactl + ESP32 LED)
  Scroll Lock → playerctl play-pause
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

ESP_IP   = os.environ.get('ESP_IP', '192.168.1.x')
ESP_PORT = 9877

WATCHED_KEYS = {ecodes.KEY_INSERT, ecodes.KEY_SCROLLLOCK}

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
        try:
            s = socket.create_connection((ESP_IP, ESP_PORT), timeout=2)
            s.sendall(f'{state}\n'.encode())
            s.close()
            logging.info('ESP32 <- %s', state)
        except Exception as e:
            logging.warning('ESP32 send failed: %s', e)

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

_mic_q  = queue.Queue()
_play_q = queue.Queue()

def _mic_action(count):
    global _muted
    _muted = not _muted
    subprocess.run(['pactl', 'set-source-mute', _src, '1' if _muted else '0'],
                   capture_output=True)
    logging.info('%s (burst=%d)', 'muted' if _muted else 'unmuted', count)
    _esp_event.set()   # signal sender — it reads current _muted, so always correct

def _play_action(count):
    logging.info('play-pause (burst=%d)', count)
    subprocess.run(['playerctl', 'play-pause'], capture_output=True)

threading.Thread(target=_burst_worker, args=(_mic_q,  _mic_action),  daemon=True, name='mic-worker').start()
threading.Thread(target=_burst_worker, args=(_play_q, _play_action), daemon=True, name='play-worker').start()

_key_queue = {
    ecodes.KEY_INSERT:     _mic_q,
    ecodes.KEY_SCROLLLOCK: _play_q,
}

# ── evdev listener — dedup across device nodes ────────────────────────────────
_last_seen:  dict[int, float] = {}
_dedup_lock = threading.Lock()
_DEDUP_S    = 0.05   # 50 ms: collapses same key from multiple HID nodes

def watch_device(dev):
    logging.info('watching %s (%s)', dev.path, dev.name)
    try:
        for event in dev.read_loop():
            if event.type != ecodes.EV_KEY or event.code not in _key_queue or event.value != 1:
                continue
            now = time.monotonic()
            with _dedup_lock:
                if now - _last_seen.get(event.code, 0) < _DEDUP_S:
                    continue
                _last_seen[event.code] = now
            _key_queue[event.code].put(1)
    except Exception as e:
        logging.warning('device %s lost: %s', dev.path, e)

def find_keyboards():
    devs = []
    for path in evdev.list_devices():
        try:
            d = evdev.InputDevice(path)
            caps = d.capabilities()
            if ecodes.EV_KEY in caps and any(k in caps[ecodes.EV_KEY] for k in WATCHED_KEYS):
                devs.append(d)
        except Exception:
            pass
    return devs

if __name__ == '__main__':
    keyboards = find_keyboards()
    if not keyboards:
        logging.error('no device with watched keys — check input group membership')
        sys.exit(1)
    threads = [threading.Thread(target=watch_device, args=(dev,), daemon=True) for dev in keyboards]
    for t in threads:
        t.start()
    logging.info('listening on %d device(s)', len(keyboards))
    for t in threads:
        t.join()
