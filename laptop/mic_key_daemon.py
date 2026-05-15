#!/usr/bin/env python3
"""
Evdev key daemon — handles keys on GNOME/Wayland (can't be grabbed as
custom shortcuts). Runs as a user service. Requires 'input' group membership.

Each keyboard is grabbed exclusively; all non-consumed events are forwarded
through a UInput virtual device so normal typing is unaffected.

Keys handled:
  PgUp          → mic toggle (pactl + ESP32 LED)
  PgDn          → play/pause (playerctl)
  Ctrl + PgDn   → lamp toggle (ESP32)

LED always reflects actual PulseAudio mic state — external changes (VM, GNOME,
other tools) are picked up via `pactl subscribe` and synced automatically.
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
from evdev import ecodes, UInput

ESP_IP   = os.environ.get('ESP_IP', '192.168.1.240')
ESP_PORT = 9877

# Keys we consume (suppress from applications) and act on.
_ACTION_KEYS = {ecodes.KEY_PAGEUP, ecodes.KEY_PAGEDOWN}
# Keys we need to detect on a device to bother watching it.
WATCHED_KEYS = _ACTION_KEYS

logging.basicConfig(
    level=logging.INFO,
    format='%(asctime)s %(levelname)s %(message)s',
    stream=sys.stdout,
)

# ── Ctrl held tracker ──────────────────────────────────────────────────────────
_ctrl_count = 0
_ctrl_lock  = threading.Lock()

def _ctrl_held() -> bool:
    with _ctrl_lock:
        return _ctrl_count > 0

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

# ── ESP sender — latest-wins, sends _muted immediately ────────────────────────
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

# ── Deferred resync — catches VM double-toggle drift ──────────────────────────
_resync_timer: 'threading.Timer | None' = None
_resync_timer_lock = threading.Lock()

def _schedule_resync(delay: float = 0.3):
    global _resync_timer
    with _resync_timer_lock:
        if _resync_timer is not None:
            _resync_timer.cancel()
        _resync_timer = threading.Timer(delay, _do_resync)
        _resync_timer.start()

def _do_resync():
    global _muted
    actual = _actual_muted()
    with _state_mutex:
        if actual == _muted:
            return
        _muted = actual
    logging.info('LED resynced after settle: %s', 'muted' if actual else 'unmuted')
    _esp_event.set()

threading.Thread(target=_esp_sender, daemon=True, name='esp-sender').start()
_esp_event.set()
logging.info('startup: %s', 'muted' if _muted else 'unmuted')

# ── PulseAudio subscriber — catches external mute changes ─────────────────────
def _pa_subscriber():
    global _muted
    while True:
        try:
            proc = subprocess.Popen(
                ['pactl', 'subscribe'],
                stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True,
            )
            for line in proc.stdout:
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
            time.sleep(2)

threading.Thread(target=_pa_subscriber, daemon=True, name='pa-sub').start()

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

# ── Actions ───────────────────────────────────────────────────────────────────
_mic_q   = queue.Queue()
_media_q = queue.Queue()
_lamp_q  = queue.Queue()

def _mic_action(count):
    global _muted
    with _state_mutex:
        _muted = not _muted
        target = _muted
    subprocess.Popen(['pactl', 'set-source-mute', _src, '1' if target else '0'],
                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    logging.info('mic: %s (burst=%d)', 'muted' if target else 'unmuted', count)
    _esp_event.set()
    _schedule_resync(0.3)

def _media_action(count):
    subprocess.Popen(['playerctl', 'play-pause'],
                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    logging.info('playerctl play-pause (burst=%d)', count)

def _lamp_action(count):
    for attempt in range(3):
        try:
            s = socket.create_connection((ESP_IP, ESP_PORT), timeout=3)
            s.sendall(b'l:t\n')
            s.close()
            logging.info('ESP32 <- l:t (lamp toggle)')
            break
        except Exception as e:
            logging.warning('lamp send failed (attempt %d): %s', attempt + 1, e)
            time.sleep(1)

for _q, _fn in [(_mic_q, _mic_action), (_media_q, _media_action), (_lamp_q, _lamp_action)]:
    threading.Thread(target=_burst_worker, args=(_q, _fn), daemon=True).start()

# ── evdev listener ────────────────────────────────────────────────────────────
_held       = set()
_last_down: dict[int, float] = {}
_dedup_lock = threading.Lock()

_watched_paths: set[str] = set()
_watched_lock  = threading.Lock()

def watch_device(dev):
    global _ctrl_count

    # Grab the device and mirror it through UInput so normal typing is unaffected.
    ui = None
    try:
        ui = UInput.from_device(dev, name=f'mic-key-fwd')
        dev.grab()
        logging.info('watching %s (%s) [grabbed]', dev.path, dev.name)
    except Exception as e:
        if ui:
            ui.close()
            ui = None
        logging.warning('grab failed for %s: %s — events will pass through', dev.path, e)
        logging.info('watching %s (%s)', dev.path, dev.name)

    try:
        for event in dev.read_loop():
            # ── Ctrl: forward to system AND track locally ─────────────────────
            if event.type == ecodes.EV_KEY and event.code in (ecodes.KEY_LEFTCTRL, ecodes.KEY_RIGHTCTRL):
                with _ctrl_lock:
                    if event.value == 1:
                        _ctrl_count += 1
                    elif event.value == 0:
                        _ctrl_count = max(0, _ctrl_count - 1)
                # fall through to forward

            # ── Consumed action keys: handle and suppress ─────────────────────
            elif event.type == ecodes.EV_KEY and event.code in _ACTION_KEYS:
                if event.value == 1:  # keydown
                    now = time.monotonic()
                    with _dedup_lock:
                        if event.code not in _held and now - _last_down.get(event.code, 0) >= 0.05:
                            _held.add(event.code)
                            _last_down[event.code] = now
                            if event.code == ecodes.KEY_PAGEUP:
                                _mic_q.put(1)
                            elif event.code == ecodes.KEY_PAGEDOWN:
                                if _ctrl_held():
                                    _lamp_q.put(1)
                                else:
                                    _media_q.put(1)
                elif event.value == 0:  # keyup
                    with _dedup_lock:
                        _held.discard(event.code)
                continue  # never forward action key events to applications

            # ── Everything else: forward unchanged ────────────────────────────
            if ui:
                ui.write_event(event)

    except Exception as e:
        logging.warning('device %s lost: %s', dev.path, e)
    finally:
        if ui:
            try:
                dev.ungrab()
            except Exception:
                pass
            ui.close()
        with _watched_lock:
            _watched_paths.discard(dev.path)

def find_keyboards():
    devs = []
    for path in evdev.list_devices():
        try:
            d = evdev.InputDevice(path)
            if 'Consumer Control' in d.name:
                continue
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
    threading.Thread(target=watch_device, args=(dev,), daemon=True).start()

def _scanner():
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
    while True:
        time.sleep(60)
