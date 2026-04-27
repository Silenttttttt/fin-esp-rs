#!/bin/sh
# Toggle mic mute and update the ESP32 blue LED (GPIO 5).
# Bound to Insert in GNOME via gsettings custom-keybindings.

ESP_IP="${ESP_IP:-192.168.1.x}"

DEFAULT_SOURCE=$(pactl get-default-source)
MUTED=$(pactl get-source-mute "$DEFAULT_SOURCE" | awk '{print $2}')

send_mic_state() {
    python3 -c "import socket; s=socket.create_connection(('$ESP_IP',9877),2); s.sendall(b'$1\n'); s.close()" 2>/dev/null &
}

if [ "$MUTED" = "yes" ]; then
    pactl set-source-mute "$DEFAULT_SOURCE" 0
    send_mic_state "m:1"
else
    pactl set-source-mute "$DEFAULT_SOURCE" 1
    send_mic_state "m:0"
fi
