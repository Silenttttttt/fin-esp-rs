# Lean AC-cloud on the ESP32 — build plan

> **As built (2026-10-27):** Stages A (HTTPS /route), B (1-client MQTTS broker) and C (clip + TLV
> decode, read-only) are implemented in `src/ac_cloud/` and verified on the live Fin-ESP against a
> scripted fake AC replaying real captured frames. The real AC still talks to the desktop rethink;
> cutover (re-pointing the `rethink.internal` DNS rewrite to 192.168.1.240) awaits Muni's OK.
> Live status: `GET http://192.168.1.240/ac/cloud`. Kill switch: `POST /action/ac/cloud?on=0`.
> Progress log: ~/Documents/Computarias/lean-ac-cloud/esp-cloud/PROGRESS.md.

Goal: the AC's fake-ThinQ-cloud runs **entirely on an ESP32** (Fin-ESP is viable: 66 KB free / 52 KB
min measured; or a dedicated board). No desktop, no rethink. Path B (tuned TLS — the AC forces TLS,
confirmed 10-27). Zero AC risk: reimplements only the server; the AC firmware is never touched.

## What the ESP server must implement (extracted from rethink 2026-10-27)

### 1. HTTPS /route endpoints (provisioning + reconnect; the AC hits these each reconnect)
- `GET /route` -> `{ resultCode:'0000', result:{ apiServer:'https://<esp>:443', mqttServer:'ssl://<esp>:8883' } }`
- `GET /route/certificate` -> `{ resultCode:'0000', result:{ certificatePem:<CA cert> } }`
- `POST /device/:id/certificate` -> signs/returns the device cert (or reuse rethink's already-issued one)
- TLS server presents a cert chaining to the CA the AC already trusts (**reuse rethink's ca.key/ca.cert
  => AC connects with NO re-pairing**).

### 2. MQTTS broker (TLS, ONE client = the AC) — minimal subset
- CONNECT->CONNACK, SUBSCRIBE->SUBACK, PINGREQ->PINGRESP, PUBLISH (both ways), DISCONNECT.
- Retained-message handling (rethink's Subscription/retainMap) — can be trivial for one client.

### 3. clip protocol (the actual AC conversation) — from cloud/thinq2/{clip,device}.ts
- On connect the AC sends a `deploy`/`preDeploy` ClipMessage (appInfo: modelName, softVer, protocolVer,
  fwUpgradeInfo...). Reply to complete provisioning; publish to `lime/devices/<did>`.
- AC publishes `device_packet` (cmd) on `clip/message/devices/<did>` carrying the **TLV frame**
  (04 00 00 00 | 87 | ... | TLV | crc16) — decode with our existing parser (tlv-explorer.mjs / ac.rs).
- Handle `req_timesync` (give it the time).
- To COMMAND the AC: publish a `device_packet` with a TLV frame (0x65 dir) — same encode we use.

### 4. State + UI
- Keep decoded state (room/outdoor temp, compressor Hz, EEV, power, all registers).
- Serve a tiny web dashboard (like Fin-ESP's) + expose to the fin-esp MCP.

## Stages (each testable, read-only first)
- **Stage 0 (desktop prototype):** reimplement 1-3 as a LEAN standalone server (node, this dir),
  reuse rethink's ca.key/ca.cert, run on ALT ports, controlled-switch test vs rethink (like the
  plaintext test), prove the real AC connects + reports. This is the ESP's logic, portable, de-risked.
- **Stage 1 (ESP read-only):** port Stage 0 to Rust/esp-idf (reuse Fin-ESP patterns + ac.rs TLV),
  tuned mbedTLS (SSL_IN/OUT_CONTENT_LEN ~4 KB). AC connects to the ESP, ESP decodes + dashboards. No cmds.
- **Stage 2 (ESP control):** ESP sends TLV commands (documented safe set, absolute values).
- **Stage 3 (standalone):** AdGuard points the AC's cloud name at the ESP IP; desktop rethink retired.

## Board / lang
- Fin-ESP (66 KB free) viable but tight (already runs lamp/IR/web); dedicated ESP32 cleaner. S3/PSRAM ideal.
- Rust/esp-idf (reuse ac.rs TLV + Fin-ESP) vs C/ESP-IDF (more TLS-server examples). Lean toward Rust.

## Reuse map
- TLV decode/encode: tlv-explorer.mjs, fin-esp-rs/src/ac.rs, rethink util/tlv.ts
- clip/broker logic to port: rethink cloud/thinq2/{clip,device,provisioning}.ts + cloud/mqtt-broker.ts
- CA (so AC doesn't re-pair): rethink-deploy/data/ca.{key,cert}
- Device meaning (type-ids): rethink cloud/devices/RAC_056905_WW.ts + our ac-decoded-map.md
