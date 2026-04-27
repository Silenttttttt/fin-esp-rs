#!/usr/bin/env python3
"""
Connects to the ESP32's media server on port 9876.
  p   → playerctl play-pause
  v:N → pactl set-sink-volume @DEFAULT_SINK@ N%
Auto-reconnects if the ESP32 reboots or WiFi drops.
"""
import os
import socket
import subprocess
import threading
import logging
import sys
import time

ESP_IP   = os.environ.get('ESP_IP', '192.168.1.x')
ESP_PORT = 9876

# Volume worker: always applies the latest value, never queues up stale ones.
_vol_lock   = threading.Lock()
_vol_target = None
_vol_event  = threading.Event()

def _vol_worker():
    while True:
        _vol_event.wait()
        _vol_event.clear()
        with _vol_lock:
            val = _vol_target
        if val is not None:
            subprocess.run(
                ['pactl', 'set-sink-volume', '@DEFAULT_SINK@', f'{val}%'],
                capture_output=True,
            )

threading.Thread(target=_vol_worker, daemon=True).start()

def set_volume(vol: str):
    global _vol_target
    with _vol_lock:
        _vol_target = vol
    _vol_event.set()

def handle(line: str):
    line = line.strip()
    if line == 'p':
        logging.info('play-pause')
        subprocess.run(['playerctl', 'play-pause'], capture_output=True)
    elif line.startswith('v:'):
        vol = line[2:]
        logging.info('volume → %s%%', vol)
        set_volume(vol)

if __name__ == '__main__':
    logging.basicConfig(
        level=logging.INFO,
        format='%(asctime)s %(levelname)s %(message)s',
        stream=sys.stdout,
    )
    while True:
        try:
            logging.info('connecting to ESP32 %s:%d ...', ESP_IP, ESP_PORT)
            with socket.create_connection((ESP_IP, ESP_PORT), timeout=10) as s:
                logging.info('connected')
                s.setsockopt(socket.SOL_SOCKET, socket.SO_KEEPALIVE, 1)
                s.setsockopt(socket.IPPROTO_TCP, socket.TCP_KEEPIDLE, 10)
                s.setsockopt(socket.IPPROTO_TCP, socket.TCP_KEEPINTVL, 5)
                s.setsockopt(socket.IPPROTO_TCP, socket.TCP_KEEPCNT, 3)
                s.settimeout(None)
                buf = b''
                while True:
                    chunk = s.recv(256)
                    if not chunk:
                        break
                    buf += chunk
                    while b'\n' in buf:
                        line, buf = buf.split(b'\n', 1)
                        handle(line.decode('ascii', errors='ignore'))
        except Exception as e:
            logging.warning('disconnected: %s — retrying in 3s', e)
            time.sleep(3)
