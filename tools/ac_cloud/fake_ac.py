#!/usr/bin/env python3
"""Scripted fake LG AC (clip protocol over MQTTS) against Fin-ESP's ac_cloud -- NEVER the real AC.

Speaks what the real AC speaks per rethink's source (cloud/thinq2/device.ts, clip.ts,
provisioning.ts, tlv_device.ts): MQTT 3.1.1 over TLS (verifying Fin-ESP's cert against rethink's CA
with hostname rethink.internal, exactly like the AC), deploy on clip/provisioning/devices/<did>,
device_packet / req_timesync on clip/message/devices/<did>, listening on lime/devices/<did>.
Replays REAL frames captured passively from the real AC (frames.json) where available.

Usage: fake_ac.py [frames.json]
"""
import base64, datetime, json, os, socket, ssl, struct, sys, time, urllib.request

HOST, PORT, SNI = "192.168.1.240", 8883, "rethink.internal"
CA = os.environ.get("RETHINK_CA", "./ca.cert")  # the CA cert Fin-ESP's TLS cert chains to
DID = "fa4e0000-0000-4000-8000-0000000fa4e0"  # fake did, not the real AC's
FAILS = []

def check(cond, what):
    print(("  PASS " if cond else "  FAIL ") + what)
    if not cond:
        FAILS.append(what)

# ---- TLV / frames (rethink util/tlv.ts + crc16) ----
def crc16(data):
    crc = 0
    for b in data:
        crc ^= b << 8
        for _ in range(8):
            crc = ((crc << 1) ^ 0x1021) if crc & 0x8000 else (crc << 1)
            crc &= 0xffff
    return crc

def tlv_parse(buf):
    out, i = [], 0
    while i + 2 <= len(buf):
        t = (buf[i] << 2) | (buf[i + 1] >> 6); l = (buf[i + 1] >> 4) & 3; v = buf[i + 1] & 15
        if i + 2 + l > len(buf): break
        if l:
            v = 0
            for j in range(l): v = (v << 8) | buf[i + 2 + j]
        out.append((t, v)); i += 2 + l
    return out

def tlv_build(elems):
    out = []
    for t, v in elems:
        out.append((t >> 2) & 255); tl = (t & 3) << 6
        if v < 0x10: out.append(tl | v)
        elif v < 0x100: out += [tl | 0x10, v]
        elif v < 0x10000: out += [tl | 0x20, v >> 8 & 255, v & 255]
        else: out += [tl | 0x30, v >> 16 & 255, v >> 8 & 255, v & 255]
    return bytes(out)

def ac_frame(elems, b0=0x01, b1=0x01, seq=0x01):
    """A frame as the AC sends it (dir 0x87, type 0x02)."""
    tlv = tlv_build(elems)
    body = bytes([0x04, 0, 0, 0, 0x87, 0x02, 0x01, seq, len(tlv)]) + tlv
    c = crc16(body)
    return bytes([b0, b1]) + body + bytes([c >> 8, c & 255])

# ---- MQTT ----
def enc_len(n):
    out = bytearray()
    while True:
        b = n % 128; n //= 128
        out.append(b | (0x80 if n else 0))
        if not n: return bytes(out)

def s16(s):
    b = s.encode() if isinstance(s, str) else s
    return struct.pack(">H", len(b)) + b

def pkt(first, body): return bytes([first]) + enc_len(len(body)) + body

def connect(cid, keepalive=60):
    return pkt(0x10, s16("MQTT") + bytes([4, 0x02]) + struct.pack(">H", keepalive) + s16(cid))

def subscribe(pid, topics):
    return pkt(0x82, struct.pack(">H", pid) + b"".join(s16(t) + b"\x00" for t in topics))

def publish(topic, payload, qos=0, pid=1):
    body = s16(topic) + (struct.pack(">H", pid) if qos else b"") + payload
    return pkt(0x30 | (qos << 1), body)

class Conn:
    def __init__(self, verify=True):
        ctx = ssl.create_default_context(cafile=CA) if verify else ssl._create_unverified_context()
        # rethink's CA has no keyUsage ext (by design, see rethink util/pki.ts); Python 3.13+'s
        # default X509_STRICT rejects that, the AC/OpenSSL/curl don't. Chain + hostname still verified.
        ctx.verify_flags &= ~ssl.VERIFY_X509_STRICT
        raw = socket.create_connection((HOST, PORT), timeout=15)
        self.t0 = time.time()
        self.s = ctx.wrap_socket(raw, server_hostname=SNI)
        self.hs = time.time() - self.t0
        self.buf = b""
    def send(self, b): self.s.sendall(b)
    def read_pkt(self, timeout=20):
        self.s.settimeout(timeout); end = time.time() + timeout
        while True:
            if len(self.buf) >= 2:
                n, mult, i = 0, 1, 1
                while i < len(self.buf):
                    b = self.buf[i]; n += (b & 127) * mult; i += 1
                    if not b & 0x80: break
                    mult *= 128
                else:
                    i = None
                if i and len(self.buf) >= i + n:
                    p = self.buf[:i + n]; self.buf = self.buf[i + n:]
                    return p[0], p[i:]
            if time.time() > end: return None
            try:
                d = self.s.recv(4096)
            except socket.timeout:
                return None
            if not d: return ("closed", b"")
            self.buf += d
    def wait_publish(self, pred, timeout=25):
        end = time.time() + timeout
        while time.time() < end:
            r = self.read_pkt(max(0.1, end - time.time()))
            if r is None: return None
            if r[0] == "closed": return "closed"
            first, body = r
            if first >> 4 == 3:
                tl = struct.unpack(">H", body[:2])[0]; topic = body[2:2 + tl].decode()
                off = 2 + tl + (2 if (first >> 1) & 3 else 0)
                try: j = json.loads(body[off:])
                except Exception: j = None
                if pred(topic, j): return topic, j
        return None
    def close(self):
        try: self.s.close()
        except Exception: pass

def status():
    return json.load(urllib.request.urlopen(f"http://{HOST}/ac/cloud", timeout=8))

def mem():
    return json.load(urllib.request.urlopen(f"http://{HOST}/status", timeout=8))["mem"]

def clip(cmd, data, typ=1, kind="RAC_056905_WW"):
    return json.dumps({"mid": int(time.time() * 1000), "did": DID, "kind": kind, "cmd": cmd, "type": typ, "data": data}).encode()

def main():
    frames_path = sys.argv[1] if len(sys.argv) > 1 else None
    real = []
    if frames_path:
        for f in json.load(open(frames_path)):
            if "rx" in f: real.append(bytes.fromhex(f["rx"]))
    print(f"loaded {len(real)} real AC->cloud frames")
    lime = f"lime/devices/{DID}"

    print("== 1. connect (TLS verified against rethink CA, SNI rethink.internal) + CONNECT/SUBSCRIBE")
    c = Conn()
    print(f"  handshake {c.hs:.2f}s")
    c.send(connect(DID, 60))
    r = c.read_pkt(); check(r and r[0] == 0x20 and r[1][1] == 0, "CONNACK rc=0")
    c.send(subscribe(1, [lime]))
    r = c.read_pkt(); check(r and r[0] == 0x90, "SUBACK")

    print("== 2. deploy -> completeProvisioning, then caps query")
    deploy = {"appInfo": {"modelName": "RAC_0B0001_WW", "modelLanguage": "", "softVer": "clip_bkn_v1.9.226",
              "ruleVer": "", "countryCode": "DE", "subCountryCode": "", "appVersion": "clip_bkn_v1.9.226",
              "modemType": "BEKEN_BK7234", "regionalCode": "", "timezone": "", "svcCode": "", "HomeApSsid": "x",
              "DeviceType": "401", "protocolVer": "4.9", "size": 3, "fwUpgradeInfo": {"x": [1, 2, {"y": None}]}},
              "platformInfo": {"provisioningKey": "abc", "version": "1"}}
    c.send(publish(f"clip/provisioning/devices/{DID}", clip("deploy", deploy, 0), qos=1, pid=7))
    got_ack = c.read_pkt(); check(got_ack and got_ack[0] == 0x40 and got_ack[1] == b"\x00\x07", "PUBACK for QoS1 deploy (pid 7)")
    r = c.wait_publish(lambda t, j: j and j.get("cmd") == "completeProvisioning")
    check(r is not None and r not in ("closed",), "completeProvisioning received")
    if r and r != "closed":
        t, j = r
        d = j["data"]
        check(t == lime and j["did"] == DID and j["type"] == 0, "topic/did/type")
        check(d == {"result": 0, "host": "message", "appInfo": {"host": "message", "publication": {
            "message": f"clip/message/devices/{DID}", "provisioning": f"clip/provisioning/devices/{DID}"}},
            "provisioningType": "deploy", "deployInterval": 600}, "data identical to rethink generateDeployResponse")
    r = c.wait_publish(lambda t, j: j and j.get("cmd") == "packet")
    check(r is not None and r != "closed", "caps query packet received")
    if r and r != "closed":
        fr = bytes.fromhex(r[1]["data"])
        check(fr[2:7] == bytes([4, 0, 0, 0, 0x65]) and crc16(fr[2:-2]) == (fr[-2] << 8 | fr[-1]), "query frame header + CRC valid")
        check(tlv_parse(fr[11:-2]) == [(0x1f5, 1)], "query TLV == 0x1f5=1 (caps)")
        print("  caps query frame:", fr.hex())

    print("== 3. AC answers caps (0x2da) -> values query")
    caps_real = [f for f in real if any(t == 0x2da for t, _ in tlv_parse(f[11:-2]))]
    caps = caps_real[0] if caps_real else ac_frame([(0x2da, 0x1234), (0x2c1, 0x53), (0x2e1, 32), (0x2e2, 60)])
    print(f"  using {'REAL' if caps_real else 'synthesized'} caps frame ({len(caps)} B)")
    c.send(publish(f"clip/message/devices/{DID}", clip("device_packet", caps.hex().upper())))
    r = c.wait_publish(lambda t, j: j and j.get("cmd") == "packet")
    if r and r != "closed":
        fr = bytes.fromhex(r[1]["data"])
        check(tlv_parse(fr[11:-2]) == [(0x1f5, 2)], "values query (0x1f5=2) after caps")
        rethink_tx = [f["tx"] for f in (json.load(open(frames_path)) if frames_path else []) if isinstance(f.get("tx"), str)]
        if rethink_tx:
            check(fr.hex() in rethink_tx, f"values query {fr.hex()} byte-identical to rethink's real query to the AC {rethink_tx[0]}")
    else:
        check(False, "values query after caps")

    print("== 4. AC sends values (real frames) -> decoded on Fin-ESP")
    vals_real = [f for f in real if len(tlv_parse(f[11:-2])) >= 10 and any(t == 0x1f7 for t, _ in tlv_parse(f[11:-2]))]
    if vals_real:
        vf = vals_real[-1]
        print(f"  using REAL values frame ({len(vf)} B, {len(tlv_parse(vf[11:-2]))} TLVs)")
    else:
        vf = ac_frame([(0x1f7, 0), (0x1f9, 0), (0x1fa, 2), (0x1fd, 47), (0x1fe, 42), (0x221, 0), (0x330, 4),
                       (0x332, 118), (0x32c, 120), (0x22a, 0), (0x331, 0)])
        print("  using synthesized values frame")
    expect = dict(tlv_parse(vf[11:-2]))
    sent = [vf] + [f for f in real if f is not vf][:40]  # plus a burst of other real notifies
    for f in sent:
        c.send(publish(f"clip/message/devices/{DID}", clip("device_packet", f.hex().upper())))
    # final state: replay vf last so expected values are the final ones
    c.send(publish(f"clip/message/devices/{DID}", clip("device_packet", vf.hex().upper())))
    time.sleep(2)
    st = status()
    regs = {int(k, 16): v for k, v in st["regs"].items()}
    check(st["clip"]["caps"] and st["clip"]["values"], "Fin-ESP caps+values flags set")
    check(all(regs.get(t) == v for t, v in expect.items()), f"all {len(expect)} TLVs of the values frame decoded identically")
    check(st["clip"]["crc_bad"] == 0, "no CRC mismatches on real frames")
    print("  decoded ac:", st["ac"])
    check(st["clip"]["model"] == "RAC_0B0001_WW" and st["clip"]["soft_ver"] == "clip_bkn_v1.9.226" and st["clip"]["protocol_ver"] == "4.9", "deploy appInfo parsed")

    print("== 5. req_timesync -> resp_timesync (7 bytes, month 0-based like rethink)")
    c.send(publish(f"clip/message/devices/{DID}", clip("req_timesync", "")))
    r = c.wait_publish(lambda t, j: j and j.get("cmd") == "resp_timesync")
    if r and r != "closed":
        b = base64.b64decode(r[1]["data"]); now = datetime.datetime.now(datetime.timezone.utc)
        exp = [now.year % 100, now.month - 1, now.day, now.hour]
        check(len(b) == 7 and list(b[:4]) == exp and abs(b[4] * 60 + b[5] - (now.minute * 60 + now.second)) < 30 and b[6] == (now.isoweekday() % 7),
              f"timesync bytes {list(b)} vs now {now:%y-%m-%d %H:%M:%S} wd{now.isoweekday()%7}")
    else:
        check(False, "resp_timesync received")

    print("== 6. keepalive PING + bad payloads must not kill the session")
    c.send(pkt(0xc0, b"")); r = c.read_pkt(); check(r and r[0] == 0xd0, "PINGRESP")
    for junk in [b"not json", b"{\"did\":\"" + b"A" * 300 + b"\"}", b"\x00\xff\xfe", b"{}", b"[1,2,3]"]:
        c.send(publish(f"clip/message/devices/{DID}", junk))
    c.send(publish(f"clip/message/devices/{DID}", clip("device_packet", "ZZ")))
    c.send(publish(f"clip/message/devices/{DID}", clip("device_packet", "0102")))
    c.send(pkt(0xc0, b"")); r = c.read_pkt(); check(r and r[0] == 0xd0, "session alive after junk payloads")
    print("  status:", {k: status()["clip"][k] for k in ("bad_json", "unknown", "frames")})
    c.close()

    print("== 7. hostile/broken clients (device must survive, each only drops itself)")
    m0 = mem()
    # raw non-TLS garbage on both ports
    for port in (8883, 443):
        s = socket.create_connection((HOST, port), timeout=10); s.sendall(b"GET / HTTP/1.0\r\n\r\n" * 3)
        try: s.recv(100)
        except Exception: pass
        s.close()
    # TLS ok but malformed MQTT remaining-length / oversized packet / no CONNECT
    for bad in [b"\x10\xff\xff\xff\xff\x7f", b"\x30" + enc_len(200000) + b"x" * 100, b"\x82\x02\x00"]:
        c2 = Conn(); c2.send(bad); r = c2.read_pkt(8); check(r is None or r[0] == "closed", f"bad MQTT {bad[:6].hex()}.. -> dropped"); c2.close()
    c3 = Conn(); t0 = time.time(); r = c3.read_pkt(15); check(r and r[0] == "closed" and 9 < time.time() - t0 < 14, f"no-CONNECT session closed after {time.time()-t0:.1f}s"); c3.close()
    # session replacement: 2nd connection replaces the 1st
    a = Conn(); a.send(connect("a")); a.read_pkt()
    b = Conn(); b.send(connect("b")); r = b.read_pkt(); check(r and r[0] == 0x20, "2nd client connects (replaces 1st)")
    r = a.read_pkt(5); check(r is None or r[0] == "closed", "1st client dropped")
    b.send(pkt(0xe0, b"")); a.close(); b.close()
    time.sleep(3)
    st = status(); m1 = mem()
    print("  heap before/after:", m0["free"], m1["free"], "min_free", m1["min_free"], "stack_hwm", st["stack_hwm"], "last_err", st["last_err"])
    check(st["phase"] == "listening", "ac_cloud still listening")
    check(m1["free"] > m0["free"] - 3000, "no heap leak across hostile clients")

    print("\nRESULT:", "ALL PASS" if not FAILS else f"{len(FAILS)} FAIL: {FAILS}")
    return 0 if not FAILS else 1

if __name__ == "__main__":
    sys.exit(main())
