#!/usr/bin/env bash
# Send lamp toggle to ESP32 via TCP (faster than direct Tuya from laptop).
printf 'l:t\n' | nc -w1 "${ESP_IP:-192.168.1.240}" 9877
