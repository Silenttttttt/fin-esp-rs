#!/bin/bash
# Guarded OTA to Fin-ESP ONLY: MAC + title identity check first, abort on any mismatch.
set -e
BIN="$1"; IP=192.168.1.240
[[ -f "$BIN" ]] || { echo "no image $BIN"; exit 2; }
ping -c1 -W1 $IP >/dev/null 2>&1 || true
if ! (ip neigh show $IP | grep -q "00:70:07:25:6d:d4" && curl -s -m 5 http://$IP/ | grep -q '<title>Fin-ESP'); then
  echo "IDENTITY CHECK FAILED - not flashing"; ip neigh show $IP; exit 3; fi
echo "identity OK (00:70:07:25:6d:d4, title Fin-ESP)"
python3 - "$BIN" "$IP" <<'PYEOF'
import socket, struct, sys, time
path, host = sys.argv[1], sys.argv[2]
data = open(path, 'rb').read(); size = len(data)
print(f"  Sending {size} bytes...")
s = socket.create_connection((host, 3232), timeout=120)
s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
s.sendall(struct.pack('>I', size))
sent = 0; t0 = time.time()
while sent < size:
    sent += s.send(data[sent:sent+16384])
print(f"  sent {sent} in {time.time()-t0:.1f}s")
s.shutdown(socket.SHUT_WR)
print("  Response:", s.recv(64).decode(errors='replace').strip())
PYEOF
