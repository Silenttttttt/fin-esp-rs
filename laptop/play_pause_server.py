#!/usr/bin/env python3
"""
Connects to the ESP32's media server on port 9876.
Runs `playerctl play-pause` whenever it receives "p\n".
Auto-reconnects if the ESP32 reboots or WiFi drops.
"""
import os
import socket
import subprocess
import logging
import sys
import time

ESP_IP   = os.environ.get('ESP_IP', '192.168.1.x')  # set ESP_IP env var or edit here
ESP_PORT = 9876

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
                # TCP keepalive — detects a rebooted ESP32 within ~30s
                s.setsockopt(socket.SOL_SOCKET, socket.SO_KEEPALIVE, 1)
                s.setsockopt(socket.IPPROTO_TCP, socket.TCP_KEEPIDLE, 10)
                s.setsockopt(socket.IPPROTO_TCP, socket.TCP_KEEPINTVL, 5)
                s.setsockopt(socket.IPPROTO_TCP, socket.TCP_KEEPCNT, 3)
                s.settimeout(None)  # block forever — keepalive handles dead connections
                while True:
                    data = s.recv(16)
                    if not data:
                        break
                    if b'p' in data:
                        logging.info('play-pause!')
                        result = subprocess.run(
                            ['playerctl', 'play-pause'],
                            capture_output=True, text=True
                        )
                        if result.returncode != 0:
                            logging.warning('playerctl: %s', result.stderr.strip())
        except Exception as e:
            logging.warning('disconnected: %s — retrying in 3s', e)
            time.sleep(3)
