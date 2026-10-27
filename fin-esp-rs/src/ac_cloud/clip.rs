//! The clip protocol (ThinQ2 device <-> cloud over MQTT), ported from rethink:
//!   cloud/thinq2/device.ts      DeviceAcceptor: deploy -> completeProvisioning, req_timesync, device_packet
//!   cloud/thinq2/provisioning.ts generateDeployResponse
//!   cloud/devices/tlv_device.ts  caps query -> values query -> periodic values query
//!
//! READ-ONLY toward the AC except for the exact same caps/values *queries* rethink already sends it
//! every 28 s / 15 min today (0x1f5 = 1 / 2). No setting is ever written from here.

use super::tlv;
use serde::Deserialize;

const LIME: &str = "lime/devices/";
const RETRY_MS: u64 = 15_000; // rethink: caps/values retry every 15 s until answered
const QUERY_ACTIVE_MS: u64 = 28_000; // rethink: (30-2) s while running
const QUERY_IDLE_MS: u64 = 15 * 60_000; // rethink default

#[derive(Deserialize)]
struct Head {
    #[serde(default)]
    did: String,
    #[serde(default)]
    cmd: String,
}

#[derive(Deserialize)]
struct StrData {
    data: String,
}

#[derive(Deserialize)]
struct Deploy {
    data: DeployData,
}
#[derive(Deserialize)]
struct DeployData {
    #[serde(rename = "appInfo")]
    app_info: Option<AppInfo>,
}
// serde_json::Value for each, not String: if the module ever sends one as a number, a String
// field would make the whole deploy fail to parse.
#[derive(Deserialize)]
struct AppInfo {
    #[serde(rename = "modelName")]
    model_name: Option<serde_json::Value>,
    #[serde(rename = "softVer")]
    soft_ver: Option<serde_json::Value>,
    #[serde(rename = "appVersion")]
    app_version: Option<serde_json::Value>,
    #[serde(rename = "protocolVer")]
    protocol_ver: Option<serde_json::Value>,
    #[serde(rename = "DeviceType")]
    device_type: Option<serde_json::Value>,
}

fn val_str(v: &Option<serde_json::Value>) -> String {
    match v {
        Some(serde_json::Value::String(s)) => s.chars().take(40).collect(),
        Some(serde_json::Value::Null) | None => String::new(),
        Some(o) => o.to_string().chars().take(40).collect(),
    }
}

/// Per-MQTT-session clip state (rethink: ClientWithExtra.deployMsg / deviceObj).
#[derive(Default)]
pub struct Clip {
    did: Option<String>,
    device: bool,
    caps: bool,
    values: bool,
    last_caps_q: u64,
    last_values_q: u64,
}

/// What the broker should publish (topic, payload) as a result of a message/tick.
pub type Out = Vec<(String, Vec<u8>)>;

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn packet_msg(did: &str, frame: &[u8]) -> Vec<u8> {
    // rethink Device.send('packet', 1, HEX)
    format!(
        "{{\"did\":\"{}\",\"mid\":{},\"cmd\":\"packet\",\"type\":1,\"data\":\"{}\"}}",
        did, unix_ms(), tlv::hex_upper(frame)
    )
    .into_bytes()
}

fn json_safe(s: &str) -> bool {
    s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
}

impl Clip {
    pub fn did(&self) -> Option<&str> { self.did.as_deref() }

    /// rethink DeviceAcceptor.mqtt(): one PUBLISH from the AC.
    pub fn on_publish(&mut self, topic: &str, payload: &[u8], now: u64, out: &mut Out) {
        if !topic.contains("clip/") {
            return;
        }
        // rethink: topic.replace(/^.*\/clip/, 'clip')
        let topic = match topic.rfind("/clip") {
            Some(p) => &topic[p + 1..],
            None => topic,
        };
        // trimNull
        let payload = match payload.last() {
            Some(0) => &payload[..payload.len() - 1],
            _ => payload,
        };
        let head: Head = match serde_json::from_slice(payload) {
            Ok(h) => h,
            Err(_) => {
                super::with_status(|s| s.bad_json += 1);
                return;
            }
        };
        if !json_safe(&head.did) {
            super::with_status(|s| s.bad_json += 1);
            return;
        }

        let msg_topic = topic.strip_prefix("clip/message/devices/");
        let prov_topic = topic.strip_prefix("clip/provisioning/devices/");

        if msg_topic == Some(head.did.as_str()) {
            let is_ours = self.did.as_deref() == Some(head.did.as_str());
            if head.cmd == "device_packet" && is_ours && self.device {
                if let Ok(d) = serde_json::from_slice::<StrData>(payload) {
                    self.on_frame(&d.data, now, out);
                }
            } else if head.cmd == "req_timesync" && is_ours {
                self.timesync(out);
            } else if head.cmd == "completeProvisioning_ack" {
                super::with_status(|s| s.provisioning_acks += 1);
            } else {
                super::with_status(|s| s.other_cmds += 1);
            }
        }

        if prov_topic == Some(head.did.as_str()) && (head.cmd == "preDeploy" || head.cmd == "deploy") {
            self.on_deploy(&head, payload, now, out);
        }
    }

    fn on_deploy(&mut self, head: &Head, payload: &[u8], now: u64, out: &mut Out) {
        let did = head.did.clone();
        // rethink generateDeployResponse(), verbatim shape.
        let resp = format!(
            "{{\"did\":\"{did}\",\"mid\":{mid},\"cmd\":\"completeProvisioning\",\"type\":0,\"data\":{{\"result\":0,\"host\":\"message\",\"appInfo\":{{\"host\":\"message\",\"publication\":{{\"message\":\"clip/message/devices/{did}\",\"provisioning\":\"clip/provisioning/devices/{did}\"}}}},\"provisioningType\":\"{cmd}\",\"deployInterval\":600}}}}",
            did = did,
            mid = unix_ms(),
            cmd = head.cmd,
        );
        out.push((format!("{LIME}{did}"), resp.into_bytes()));

        let info = serde_json::from_slice::<Deploy>(payload).ok().and_then(|d| d.data.app_info);
        super::with_status(|s| {
            s.deploys += 1;
            s.did = did.clone();
            if let Some(a) = &info {
                s.model = val_str(&a.model_name);
                let sv = val_str(&a.soft_ver);
                s.soft_ver = if sv.is_empty() { val_str(&a.app_version) } else { sv };
                s.proto_ver = val_str(&a.protocol_ver);
                s.device_type = val_str(&a.device_type);
            }
        });

        self.did = Some(did);
        if self.device {
            log::warn!("[ac_cloud] device already set up, deploy repeated");
            return;
        }
        // completeProvisioning(): new device -> TLVDevice constructor -> queryCaps() immediately.
        self.device = true;
        self.caps = false;
        self.values = false;
        super::with_status(|s| { s.device_ready = true; s.caps = false; s.values = false; });
        self.send_query(true, now, out);
    }

    fn send_query(&mut self, caps: bool, now: u64, out: &mut Out) {
        let Some(did) = self.did.as_deref() else { return };
        let frame = if caps { tlv::caps_query() } else { tlv::values_query() };
        out.push((format!("{LIME}{did}"), packet_msg(did, &frame)));
        if caps { self.last_caps_q = now } else { self.last_values_q = now }
        super::with_status(|s| s.queries += 1);
    }

    fn on_frame(&mut self, hex: &str, now: u64, out: &mut Out) {
        let Some(buf) = tlv::hex_decode(hex, 512) else {
            super::with_status(|s| s.frames_unknown += 1);
            return;
        };
        match tlv::classify(&buf) {
            tlv::FrameKind::Tlv(payload, crc_ok) => {
                let mut has_caps = false;
                let mut has_power = false;
                let mut n = 0usize;
                super::with_shared(|sh| {
                    n = tlv::parse(payload, |t, v| {
                        sh.regs.set(t, v);
                        if t == 0x2da { has_caps = true; }
                        if t == 0x1f7 { has_power = true; }
                    });
                    sh.st.frames += 1;
                    if !crc_ok { sh.st.crc_bad += 1; }
                    sh.st.last_frame_ms = now;
                });
                // rethink TLVDevice.processTLV: caps -> values query; values only after caps.
                if !self.caps && has_caps {
                    self.caps = true;
                    super::with_status(|s| s.caps = true);
                    self.send_query(false, now, out);
                } else if self.caps && !self.values && n >= 10 && has_power {
                    self.values = true;
                    super::with_status(|s| s.values = true);
                }
            }
            tlv::FrameKind::Private => super::with_status(|s| s.frames_private += 1),
            tlv::FrameKind::Unknown => super::with_status(|s| s.frames_unknown += 1),
        }
    }

    /// rethink DeviceAcceptor.timeSyncRequest(). Month is 0-based (JS getUTCMonth) -- kept as-is
    /// because that is what the AC has been receiving from rethink.
    fn timesync(&mut self, out: &mut Out) {
        let Some(did) = self.did.as_deref() else { return };
        let secs = unix_ms() / 1000;
        if secs < 1_700_000_000 {
            // SNTP not synced yet: answering would hand the AC a 1970 clock. It retries.
            super::with_status(|s| s.timesync_skipped += 1);
            return;
        }
        let days = (secs / 86_400) as i64;
        let sod = secs % 86_400;
        let (y, m, d) = civil_from_days(days);
        let wday = ((days + 4).rem_euclid(7)) as u8; // 1970-01-01 was a Thursday; 0 = Sunday
        let b = [
            (y % 100) as u8,
            (m - 1) as u8,
            d as u8,
            (sod / 3600) as u8,
            ((sod / 60) % 60) as u8,
            (sod % 60) as u8,
            wday,
        ];
        let msg = format!(
            "{{\"did\":\"{}\",\"mid\":{},\"cmd\":\"resp_timesync\",\"type\":1,\"data\":\"{}\"}}",
            did, unix_ms(), base64(&b)
        );
        out.push((format!("{LIME}{did}"), msg.into_bytes()));
        super::with_status(|s| s.timesyncs += 1);
    }

    /// Periodic work (rethink's query timers).
    pub fn tick(&mut self, now: u64, power_on: bool, out: &mut Out) {
        if !self.device {
            return;
        }
        if !self.caps {
            if now.saturating_sub(self.last_caps_q) >= RETRY_MS {
                self.send_query(true, now, out);
            }
        } else if !self.values {
            if now.saturating_sub(self.last_values_q) >= RETRY_MS {
                self.send_query(false, now, out);
            }
        } else {
            let every = if power_on { QUERY_ACTIVE_MS } else { QUERY_IDLE_MS };
            if now.saturating_sub(self.last_values_q) >= every {
                self.send_query(false, now, out);
            }
        }
    }
}

/// Howard Hinnant's days -> (year, month 1-12, day 1-31).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn base64(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity((input.len() + 2) / 3 * 4);
    for c in input.chunks(3) {
        let b0 = c[0] as u32;
        let b1 = c.get(1).copied().unwrap_or(0) as u32;
        let b2 = c.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        s.push(T[(n >> 18) as usize & 63] as char);
        s.push(T[(n >> 12) as usize & 63] as char);
        s.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        s.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    s
}
