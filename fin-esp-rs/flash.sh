#!/bin/bash
# Flash Fin-ESP firmware.
# Usage:
#   ./flash.sh <ESP32_IP>          — OTA over network
#   ./flash.sh usb                 — USB via /dev/ttyUSB0 (default)
#   ./flash.sh usb /dev/ttyACM0   — USB, explicit port

set -e

ELF="target/xtensa-esp32-espidf/release/fin-esp-rs"
BIN="/tmp/fin-esp.bin"
ESPTOOL="$(find .embuild -name esptool.py 2>/dev/null | head -1)"
if [[ -z "$ESPTOOL" ]]; then ESPTOOL="esptool.py"; fi

echo "[1/3] Building..."
cargo build --release 2>&1 | tail -3

echo "[2/3] Converting ELF → ESP32 image..."
"$ESPTOOL" --chip esp32 elf2image \
    --flash_mode dio --flash_freq 80m --flash_size 4MB \
    "$ELF" -o "$BIN"
echo "      Binary: $(wc -c < "$BIN") bytes"

if [[ "$1" == "usb" ]]; then
    PORT="${2:-/dev/ttyACM0}"
    BOOTLOADER="target/xtensa-esp32-espidf/release/bootloader.bin"
    PARTITIONS="target/xtensa-esp32-espidf/release/partition-table.bin"
    echo "[3/3] Flashing via USB ($PORT)..."
    "$ESPTOOL" --chip esp32 --port "$PORT" --baud 460800 \
        write_flash --flash_mode dio --flash_freq 80m --flash_size 4MB \
        0x1000  "$BOOTLOADER" \
        0x8000  "$PARTITIONS" \
        0x20000 "$BIN"
    echo "Done."
else
    IP="${1:?Usage: $0 <ESP32_IP> | usb [port]}"
    echo "[3/3] OTA flashing to ${IP}:3232 ..."
    python3 - "$BIN" "$IP" <<'PYEOF'
import socket, struct, sys, time

path, host = sys.argv[1], sys.argv[2]
data = open(path, 'rb').read()
size = len(data)
print(f"  Sending {size} bytes ({size//1024} KB)...")

s = socket.create_connection((host, 3232), timeout=120)
s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
s.sendall(struct.pack('>I', size))

sent = 0
chunk = 16384
t0 = time.time()
while sent < size:
    n = s.send(data[sent:sent+chunk])
    sent += n
    pct = sent * 100 // size
    kb_s = sent / 1024 / max(time.time() - t0, 0.001)
    print(f"\r  {pct:3d}% ({sent//1024}/{size//1024} KB)  {kb_s:.0f} KB/s", end='', flush=True)

print()
s.shutdown(socket.SHUT_WR)
resp = s.recv(64).decode(errors='replace').strip()
print(f"  Response: {resp}")
PYEOF
fi
