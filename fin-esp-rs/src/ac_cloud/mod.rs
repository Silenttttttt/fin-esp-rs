//! Fin-ESP as the LG AC's local cloud (replaces the desktop rethink). The AC connects to Fin-ESP
//! over WiFi; Fin-ESP serves the ThinQ /route endpoints over HTTPS, runs a 1-client MQTTS broker,
//! speaks the clip protocol, and decodes the TLV stream -- so the real two-way AC data lives on this
//! one board, no desktop. Protocol ground truth: rethink (anszom/rethink) cloud/thinq2/*.ts,
//! cloud/mqtt-broker.ts, cloud/devices/tlv_device.ts. See docs/AC_CLOUD_PLAN.md.
//!
//! Isolation (this board runs the lamp/IR/web/OTA live and has an OOM-crash history):
//! - compiled in always, spawned only when config::AC_CLOUD_ENABLED, after all other init; its worker
//!   stack is only allocated START_DELAY after boot (so boot-time heap peaks never stack with it, and
//!   OTA/web are reachable even if this ever misbehaved);
//! - pressure valve: drops the AC session if free heap < 8 KB (lamp/web/OTA come first);
//! - runtime kill switch persisted in NVS (POST /action/ac/cloud?on=0) -- no reflash needed;
//! - ONE thread, fixed stack, never panics: every socket/TLS/parse error is logged + counted and
//!   only drops that connection; bind failures retry with backoff;
//! - every TLS handshake is heap-gated (free + largest block) and serialized with the lamp/web
//!   network work via `network_lock`; at most one MQTT session + one short-lived HTTPS request.
//! - toward the AC: what rethink sends (completeProvisioning, resp_timesync, caps/values queries)
//!   plus climate writes of power/mode/fan/target only (`write_climate`, rethink RAC_056905_WW's
//!   exact frames), requested by ac.rs. No other register is ever written.

pub mod clip;
pub mod mqtt;
pub mod tls;
pub mod tlv;

use crate::persist::Persist;
use log::{info, warn};
use std::net::{TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tls::{heap_free, heap_largest, TlsConn};

/// The CA the AC already trusts (copied from rethink's data/ca.cert). Served on /route/certificate.
pub const AC_CA_CERT: &str = include_str!("../../certs/ac_ca.cert");

/// Worker stack. Measured peak use 5.6 KB (TLS server handshake incl. RSA-2048 sign + ECDHE, deploy
/// JSON parse, status updates) -> 10 KB keeps ~45% headroom; canary overflow check is on.
const STACK: usize = 10 * 1024;
/// The worker's stack is NOT allocated at boot: a 4 KB launcher sleeps this long (boot-time heap
/// peaks: WiFi, lamp session, first weather fetch) and only then spawns the worker and exits.
const START_DELAY: Duration = Duration::from_secs(45);
const LAUNCHER_STACK: usize = 4 * 1024;
/// Pressure valve: if free heap ever drops below this while the AC session is idle-open, drop the
/// session (~11 KB back) so lamp/web/OTA never starve because of us. The AC simply reconnects later
/// (and the reconnect handshake is itself gated by HS_MIN_FREE).
const PRESSURE_DROP_FREE: u32 = 8 * 1024;
const HTTPS_PORT: &str = "0.0.0.0:443";
const MQTTS_PORT: &str = "0.0.0.0:8883";
/// Handshake heap gate. A TLS server handshake here peaks ~20 KB (RSA-2048 key, 4 KB/2 KB dynamic
/// record buffers, ECDHE); refuse below this so lamp/web always keep their margin. The AC retries.
const HS_MIN_FREE: u32 = 32 * 1024;
const HS_MIN_BLOCK: u32 = 12 * 1024;
const HS_TIMEOUT: Duration = Duration::from_secs(8);
const SESSION_READ_TIMEOUT: Duration = Duration::from_secs(5);
const NO_CONNECT_MS: u64 = 10_000;
const IDLE_NO_KEEPALIVE_MS: u64 = 5 * 60_000; // rethink: stream.setTimeout(5 min)

static STOP: AtomicBool = AtomicBool::new(false);

#[derive(Default)]
pub struct Status {
    pub phase: &'static str,
    pub https_ok: u32,
    pub https_fail: u32,
    pub mqtt_sessions: u32,
    pub mqtt_fail: u32,
    pub rejected_heap: u32,
    pub last_err: String,
    pub last_route: String,
    pub last_hs_ms: u32,
    pub heap_after_hs: u32,
    pub min_heap_after_hs: u32,
    pub stack_hwm: u32,
    pub pressure_drops: u32,
    // session
    pub connected: bool,
    pub client_id: String,
    pub mqtt_level: u8,
    pub keepalive: u16,
    pub subs: String,
    pub since_ms: u64,
    pub last_rx_ms: u64,
    pub pub_in: u32,
    pub pub_out: u32,
    pub pub_dropped: u32,
    pub qos2_ignored: u32,
    pub bad_json: u32,
    pub other_cmds: u32,
    pub provisioning_acks: u32,
    // clip / device
    pub deploys: u32,
    pub did: String,
    pub model: String,
    pub soft_ver: String,
    pub proto_ver: String,
    pub device_type: String,
    pub device_ready: bool,
    pub caps: bool,
    pub values: bool,
    pub frames: u32,
    pub crc_bad: u32,
    pub frames_private: u32,
    pub frames_unknown: u32,
    pub last_frame_ms: u64,
    pub queries: u32,
    pub timesyncs: u32,
    pub timesync_skipped: u32,
}

pub struct Shared {
    pub st: Status,
    pub regs: tlv::Registers,
}

// Boxed so a disabled build costs 4 bytes of .bss, not ~1.2 KB of DRAM.
static SHARED: Mutex<Option<Box<Shared>>> = Mutex::new(None);

pub(crate) fn with_shared(f: impl FnOnce(&mut Shared)) {
    if let Ok(mut g) = SHARED.lock() {
        let sh = g.get_or_insert_with(|| Box::new(Shared { st: Status { phase: "init", ..Default::default() }, regs: tlv::Registers::new() }));
        f(sh);
    }
}
pub(crate) fn with_status(f: impl FnOnce(&mut Status)) { with_shared(|sh| f(&mut sh.st)) }

fn set_err(e: &str) {
    warn!("[ac_cloud] {e}");
    with_status(|s| { s.last_err.clear(); s.last_err.push_str(&e.chars().take(120).collect::<String>()); });
}

fn now_ms() -> u64 { (unsafe { esp_idf_sys::esp_timer_get_time() } / 1000) as u64 }

static PERSIST: Mutex<Option<Persist>> = Mutex::new(None);

/// Entry point, called once from main after every other subsystem is up. Does nothing (no thread,
/// no sockets, no heap beyond a tiny status struct) unless config::AC_CLOUD_ENABLED.
pub fn spawn(network_lock: Arc<Mutex<()>>, persist: Persist) {
    if let Ok(mut g) = PERSIST.lock() {
        *g = Some(persist.clone());
    }
    if !crate::config::AC_CLOUD_ENABLED {
        info!("[ac_cloud] compiled disabled (config::AC_CLOUD_ENABLED=false)");
        with_status(|s| s.phase = "disabled (compile flag)");
        return;
    }
    if persist.load_ac_cloud_off() {
        info!("[ac_cloud] disabled by runtime switch (NVS); POST /action/ac/cloud?on=1 + reboot to enable");
        with_status(|s| s.phase = "off (runtime switch)");
        return;
    }
    with_status(|s| s.phase = "starting (delayed)");
    let launched = std::thread::Builder::new()
        .name("acCloudL".into())
        .stack_size(LAUNCHER_STACK)
        .spawn(move || {
            std::thread::sleep(START_DELAY);
            // Pinned to Core0, same reasoning as ir_remote.rs's receiver pinning to Core1 (see
            // that function's own comment) -- this worker does TLS handshakes (~1.1s of real
            // crypto work) plus ongoing MQTT/clip processing, long and bursty enough that, left
            // unpinned, the scheduler can land it on Core1 and starve ir_remote's own Core1
            // decode loop (a tight, non-yielding busy-wait through an entire NEC frame) mid-read
            // -- exactly the class of bug dht22.rs and ir_remote.rs were already fixed for.
            // Core0 is also where WiFi/LWIP naturally runs on this chip, so this is the native
            // fit for networking work, not a forced compromise. Reset to default right after
            // spawning so no later, unrelated thread-spawn in this process inherits this pin.
            use esp_idf_hal::cpu::Core;
            use esp_idf_hal::task::thread::ThreadSpawnConfiguration;
            for attempt in 0..6 {
                let lock = Arc::clone(&network_lock);
                let _ = ThreadSpawnConfiguration { pin_to_core: Some(Core::Core0), ..Default::default() }.set();
                let spawned = std::thread::Builder::new().name("acCloud".into()).stack_size(STACK).spawn(move || run(lock));
                let _ = ThreadSpawnConfiguration::default().set();
                match spawned {
                    Ok(_) => return, // launcher exits, its 4 KB stack is freed
                    Err(e) => {
                        warn!("[ac_cloud] worker spawn failed ({e}), attempt {attempt}, retrying in 10s");
                        std::thread::sleep(Duration::from_secs(10));
                    }
                }
            }
            set_err("gave up spawning acCloud worker (heap)");
            with_status(|s| s.phase = "spawn failed");
        });
    if let Err(e) = launched {
        set_err(&format!("launcher spawn failed: {e}"));
        with_status(|s| s.phase = "spawn failed");
    }
}

/// Runtime kill switch (web route). Persisted; `off` also stops the running thread's sockets now.
/// `on` takes effect at the next boot if the thread was never started.
pub fn set_enabled(on: bool) -> bool {
    let Ok(g) = PERSIST.lock() else { return false };
    let Some(persist) = g.as_ref() else { return false };
    persist.save_ac_cloud_off(!on);
    STOP.store(!on, Ordering::Relaxed);
    if !on {
        with_status(|s| s.phase = "stopping (runtime switch)");
    }
    true
}

fn bind(addr: &str) -> TcpListener {
    let mut backoff = 2u64;
    loop {
        match TcpListener::bind(addr) {
            Ok(l) => {
                let _ = l.set_nonblocking(true);
                return l;
            }
            Err(e) => {
                set_err(&format!("bind {addr}: {e}"));
                std::thread::sleep(Duration::from_secs(backoff));
                backoff = (backoff * 2).min(60);
            }
        }
    }
}

fn run(network_lock: Arc<Mutex<()>>) {
    'outer: loop {
        while STOP.load(Ordering::Relaxed) {
            with_status(|s| { s.phase = "off (runtime switch)"; s.connected = false; });
            std::thread::sleep(Duration::from_secs(2));
        }
        let l443 = bind(HTTPS_PORT);
        let l8883 = bind(MQTTS_PORT);
        info!("[ac_cloud] listening on :443 (HTTPS /route) and :8883 (MQTTS)");
        with_status(|s| s.phase = "listening");
        let mut sess: Option<Session> = None;

        loop {
            if STOP.load(Ordering::Relaxed) {
                drop(sess);
                info!("[ac_cloud] stopped by runtime switch");
                continue 'outer; // listeners drop here
            }
            let mut fds = [l443.as_raw_fd(), l8883.as_raw_fd(), -1];
            let mut n = 2;
            let mut pending = false;
            if let Some(s) = &sess {
                fds[2] = s.tls.fd();
                n = 3;
                pending = s.tls.pending();
            }
            let ready = match tls::poll_in(&fds[..n], if pending { 0 } else { 500 }) {
                Some(r) => r,
                None => {
                    std::thread::sleep(Duration::from_millis(200));
                    [false; 4]
                }
            };

            if ready[0] {
                if let Ok((stream, _)) = l443.accept() {
                    https_request(stream, &network_lock);
                }
            }
            if ready[1] {
                if let Ok((stream, peer)) = l8883.accept() {
                    if sess.take().is_some() {
                        info!("[ac_cloud] new MQTT connection from {peer} replaces the old session");
                    }
                    with_status(|s| s.connected = false);
                    sess = Session::open(stream, &network_lock);
                }
            }
            let now = now_ms();
            if let Some(s) = sess.as_mut() {
                let mut res = Ok(());
                if (n == 3 && ready[2]) || pending {
                    res = s.on_readable(now);
                }
                if res.is_ok() {
                    res = s.tick(now);
                }
                if let Err(e) = res {
                    info!("[ac_cloud] MQTT session closed: {e}");
                    with_status(|st| {
                        st.connected = false;
                        st.last_err.clear();
                        st.last_err.push_str(e);
                    });
                    sess = None;
                }
            }
            if sess.is_some() && heap_free() < PRESSURE_DROP_FREE {
                set_err(&format!("heap pressure ({} B free): dropping AC session to protect the device", heap_free()));
                with_status(|s| { s.connected = false; s.pressure_drops += 1; });
                sess = None;
            }
            let hwm = unsafe { esp_idf_sys::uxTaskGetStackHighWaterMark(core::ptr::null_mut()) };
            with_status(|s| s.stack_hwm = hwm);
        }
    }
}

/// Gate + serialize a handshake with the lamp/web network work, then run it.
fn handshake(stream: TcpStream, network_lock: &Mutex<()>) -> Option<TlsConn> {
    let (free, block) = (heap_free(), heap_largest());
    if free < HS_MIN_FREE || block < HS_MIN_BLOCK {
        with_status(|s| s.rejected_heap += 1);
        set_err(&format!("handshake refused: heap free {free} / block {block}"));
        return None; // stream drops -> peer sees a reset, retries later
    }
    let _g = lock_briefly(network_lock);
    let t0 = now_ms();
    match TlsConn::accept(stream, HS_TIMEOUT) {
        Ok(c) => {
            let ms = (now_ms() - t0) as u32;
            let after = heap_free();
            with_status(|s| {
                s.last_hs_ms = ms;
                s.heap_after_hs = after;
                if s.min_heap_after_hs == 0 || after < s.min_heap_after_hs { s.min_heap_after_hs = after; }
            });
            Some(c)
        }
        Err(e) => {
            set_err(&format!("TLS handshake failed: {e} (-0x{:x})", -(e as i64)));
            None
        }
    }
}

/// Up to ~3 s for the shared network lock; proceed without it rather than stall the AC forever.
fn lock_briefly(m: &Mutex<()>) -> Option<MutexGuard<'_, ()>> {
    for _ in 0..60 {
        if let Ok(g) = m.try_lock() {
            return Some(g);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

// ── HTTPS: /route, /route/certificate (rethink cloud/thinq2/provisioning.ts routes()) ──────────

fn json_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 16);
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o
}

fn route_response(method: &str, path: &str, query: &str) -> (&'static str, &'static str, String) {
    const JSON: &str = "application/json; charset=utf-8";
    let host = crate::config::AC_CLOUD_HOSTNAME;
    match (method, path) {
        ("GET", "/route") => ("200 OK", JSON, format!(
            "{{\"resultCode\":\"0000\",\"result\":{{\"apiServer\":\"https://{host}:443\",\"mqttServer\":\"ssl://{host}:8883\"}}}}"
        )),
        ("GET", "/route/certificate") => {
            let named = query.split('&').any(|kv| kv.starts_with("name=") && kv.len() > 5);
            if named {
                ("200 OK", JSON, format!(
                    "{{\"resultCode\":\"0000\",\"result\":{{\"certificatePem\":\"{}\"}}}}",
                    json_escape(AC_CA_CERT)
                ))
            } else {
                ("200 OK", JSON, "{\"resultCode\":\"0000\",\"result\":[\"common-server\",\"aws-iot\"]}".into())
            }
        }
        // Device certificate signing (only at (re-)pairing) is not implemented on Fin-ESP: the AC is
        // already provisioned against rethink's CA. Answer like rethink's own failure path.
        ("POST", p) if p.starts_with("/device/") && p.ends_with("/certificate") => {
            ("500 Internal Server Error", JSON, "{\"resultCode\":\"9999\",\"result\":{}}".into())
        }
        // rethink fallback: empty text/xml
        _ => ("200 OK", "text/xml;charset=utf-8", String::new()),
    }
}

fn https_request(stream: TcpStream, network_lock: &Mutex<()>) {
    let Some(mut c) = handshake(stream, network_lock) else {
        with_status(|s| s.https_fail += 1);
        return;
    };
    // Read the request head (bounded size and time).
    let mut req: Vec<u8> = Vec::new();
    if req.try_reserve(1024).is_err() {
        with_status(|s| s.https_fail += 1);
        return;
    }
    let deadline = now_ms() + 5_000;
    let mut buf = [0u8; 256];
    loop {
        if req.windows(4).any(|w| w == b"\r\n\r\n") || req.len() >= 2048 || now_ms() > deadline {
            break;
        }
        if !c.wait_readable(300) {
            continue;
        }
        match c.read(&mut buf) {
            Ok(0) => {}
            Ok(n) => req.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }
    let head = String::from_utf8_lossy(&req);
    let first = head.lines().next().unwrap_or("");
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if method.is_empty() {
        with_status(|s| s.https_fail += 1);
        set_err("HTTPS: no request line");
        return;
    }
    let (status, ctype, body) = route_response(method, path, query);
    let hdr = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let ok = c.write_all(hdr.as_bytes()).is_ok() && c.write_all(body.as_bytes()).is_ok();
    let line: String = format!("{method} {target} -> {}", &status[..3]).chars().take(100).collect();
    info!("[ac_cloud] HTTPS {line}");
    with_status(|s| {
        if ok { s.https_ok += 1 } else { s.https_fail += 1 }
        s.last_route = line;
    });
}

// ── MQTTS: one-client broker (rethink cloud/mqtt-broker.ts) ─────────────────────────────────────

struct Session {
    tls: TlsConn,
    rx: Vec<u8>,
    subs: Vec<String>,
    connected: bool,
    keepalive: u16,
    opened: u64,
    last_rx: u64,
    clip: clip::Clip,
}

impl Session {
    fn open(stream: TcpStream, network_lock: &Mutex<()>) -> Option<Self> {
        let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_default();
        let Some(tls) = handshake(stream, network_lock) else {
            with_status(|s| s.mqtt_fail += 1);
            return None;
        };
        tls.set_read_timeout(SESSION_READ_TIMEOUT);
        info!("[ac_cloud] MQTTS session from {peer}");
        let now = now_ms();
        with_status(|s| {
            s.mqtt_sessions += 1;
            s.since_ms = now;
            s.last_rx_ms = now;
            s.client_id.clear();
            s.subs.clear();
            s.device_ready = false;
        });
        Some(Self { tls, rx: Vec::new(), subs: Vec::new(), connected: false, keepalive: 0, opened: now, last_rx: now, clip: clip::Clip::default() })
    }

    fn send(&mut self, bytes: &[u8]) -> Result<(), &'static str> {
        self.tls.write_all(bytes).map_err(|_| "write failed")
    }

    fn on_readable(&mut self, now: u64) -> Result<(), &'static str> {
        let mut buf = [0u8; 512];
        let n = self.tls.read(&mut buf).map_err(|_| "peer closed / read error")?;
        if n == 0 {
            return Ok(());
        }
        self.last_rx = now;
        with_status(|s| s.last_rx_ms = now);
        if self.rx.len() + n > mqtt::MAX_PACKET + buf.len() || self.rx.try_reserve(n).is_err() {
            return Err("rx buffer limit");
        }
        self.rx.extend_from_slice(&buf[..n]);

        let rx = std::mem::take(&mut self.rx);
        let mut off = 0usize;
        let mut res = Ok(());
        while off < rx.len() {
            match mqtt::frame(&rx[off..]) {
                Err(()) => { res = Err("malformed/oversized MQTT packet"); break; }
                Ok(None) => break,
                Ok(Some((hl, total))) => {
                    let first = rx[off];
                    let body = &rx[off + hl..off + total];
                    off += total;
                    if let Err(e) = self.on_packet(first, body, now) {
                        res = Err(e);
                        break;
                    }
                }
            }
        }
        if res.is_ok() {
            let mut rx = rx;
            rx.drain(..off);
            self.rx = rx;
        }
        res
    }

    fn on_packet(&mut self, first: u8, body: &[u8], now: u64) -> Result<(), &'static str> {
        let Some(p) = mqtt::parse(first, body) else { return Err("unparseable MQTT packet") };
        if !self.connected && !matches!(p, mqtt::Packet::Connect { .. }) {
            return Err("packet before CONNECT");
        }
        match p {
            mqtt::Packet::Connect { level, client_id, keepalive, has_user, has_will } => {
                if self.connected {
                    return Err("second CONNECT");
                }
                if level != 3 && level != 4 {
                    let _ = self.send(&mqtt::connack(1));
                    set_err(&format!("MQTT protocol level {level} unsupported"));
                    return Err("unsupported MQTT level");
                }
                self.connected = true;
                self.keepalive = keepalive;
                let cid: String = client_id.chars().take(64).collect();
                info!("[ac_cloud] CONNECT client={cid} level={level} keepalive={keepalive} user={has_user} will={has_will}");
                with_status(|s| { s.connected = true; s.client_id = cid; s.mqtt_level = level; s.keepalive = keepalive; });
                self.send(&mqtt::connack(0))?;
            }
            mqtt::Packet::Subscribe { pid, topics } => {
                for t in &topics {
                    if self.subs.len() < 16 && !self.subs.iter().any(|s| s == t) {
                        self.subs.push(t.chars().take(96).collect());
                    }
                }
                let list = self.subs.join(",");
                info!("[ac_cloud] SUBSCRIBE {list}");
                with_status(|s| s.subs = list.chars().take(300).collect());
                self.send(&mqtt::suback(pid, topics.len()))?;
            }
            mqtt::Packet::Unsubscribe { pid, topics } => {
                self.subs.retain(|s| !topics.iter().any(|t| t == s));
                self.send(&mqtt::unsuback(pid))?;
            }
            mqtt::Packet::PingReq => self.send(&mqtt::pingresp())?,
            mqtt::Packet::Disconnect => return Err("client DISCONNECT"),
            mqtt::Packet::Publish { topic, payload, qos, pid, retain: _ } => {
                with_status(|s| s.pub_in += 1);
                if qos == 1 {
                    self.send(&mqtt::puback(pid))?;
                } else if qos == 2 {
                    with_status(|s| s.qos2_ignored += 1);
                }
                let mut out: clip::Out = Vec::new();
                self.clip.on_publish(topic, payload, now, &mut out);
                self.deliver(&out)?;
                // rethink's broker also forwards the client's own publish to matching subscriptions.
                if self.subs.iter().any(|f| mqtt::topic_matches(f, topic)) {
                    let pkt = mqtt::publish(topic, payload);
                    self.send(&pkt)?;
                }
            }
            mqtt::Packet::Other(_) => {}
        }
        Ok(())
    }

    /// Broker -> client publishes, only to topics the client subscribed (rethink Client.try_publish).
    fn deliver(&mut self, out: &clip::Out) -> Result<(), &'static str> {
        for (topic, payload) in out {
            if self.subs.iter().any(|f| mqtt::topic_matches(f, topic)) {
                let pkt = mqtt::publish(topic, payload);
                self.send(&pkt)?;
                with_status(|s| s.pub_out += 1);
            } else {
                with_status(|s| s.pub_dropped += 1);
            }
        }
        Ok(())
    }

    fn tick(&mut self, now: u64) -> Result<(), &'static str> {
        if !self.connected {
            if now.saturating_sub(self.opened) > NO_CONNECT_MS {
                return Err("no CONNECT within 10 s");
            }
            return Ok(());
        }
        let idle = now.saturating_sub(self.last_rx);
        let limit = if self.keepalive > 0 { self.keepalive as u64 * 1500 } else { IDLE_NO_KEEPALIVE_MS };
        if idle > limit {
            return Err("keepalive timeout");
        }
        let mut power_on = false;
        with_shared(|sh| power_on = sh.regs.get(0x1f7) == Some(1));
        let mut out: clip::Out = Vec::new();
        self.clip.tick(now, power_on, &mut out);
        let write = WRITE_REQ.lock().ok().and_then(|mut w| w.take());
        if let Some((elems, n)) = write {
            if self.clip.write(&elems[..n], &mut out) {
                WRITES_SENT.fetch_add(1, Ordering::Relaxed);
            } else {
                WRITES_DROPPED.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.deliver(&out)
    }
}

// ── Climate writes requested by ac.rs (WiFi control path) ─────────────────────────────────────

/// One pending write, consumed by the session thread on its next tick (<= ~0.5 s).
static WRITE_REQ: Mutex<Option<([(u16, u32); 4], usize)>> = Mutex::new(None);
static WRITES_SENT: AtomicU32 = AtomicU32::new(0);
static WRITES_DROPPED: AtomicU32 = AtomicU32::new(0);

pub fn writes_sent() -> u32 { WRITES_SENT.load(Ordering::Relaxed) }

/// Ask the live session to write power/mode/fan/target. Only these four registers, with values
/// range-checked here (power 0/1, mode in rethink's MODES, fan in FAN_MODES, target 16-30 C in
/// half degrees). Returns false (caller falls back to IR) if there is no live readback/session.
pub fn write_climate(elems: &[(u16, u32)]) -> bool {
    if elems.is_empty() || elems.len() > 4 || readback().is_none() { return false; }
    for &(t, v) in elems {
        let ok = match t {
            0x1f7 => v <= 1,
            0x1f9 => matches!(v, 0 | 1 | 2 | 4 | 6),
            0x1fa => matches!(v, 2..=6 | 8),
            0x1fe => (32..=60).contains(&v),
            0x321 => matches!(v, 0..=6 | 100), // vertical vane: fixed, 1-6 top..bottom, swing
            _ => false,
        };
        if !ok { warn!("[ac_cloud] refused climate write {t:#x}={v}"); return false; }
    }
    let mut buf = [(0u16, 0u32); 4];
    buf[..elems.len()].copy_from_slice(elems);
    match WRITE_REQ.lock() { Ok(mut w) => { *w = Some((buf, elems.len())); true } Err(_) => false }
}

// ── Readback for ac.rs (closed-loop IR) ─────────────────────────────────────────────────────────

/// The AC's real main settings as it reports them over this session. `mode`/`fan` are raw TLV
/// codes (rethink RAC_056905_WW MODES / FAN_MODES), `target_half` is the setpoint in half-degrees C.
pub struct Readback {
    pub power: bool,
    pub mode: u32,
    pub fan: Option<u32>,
    pub target_half: Option<u32>,
    pub swing_v: Option<u32>,
    pub swing_h: Option<u32>,
    pub auto_dry: Option<u32>,
    pub power_limit: Option<u32>,
    /// Counts MQTT sessions since boot: a change means the AC (re)connected.
    pub session: u32,
}

/// A TLV frame must have arrived this recently for the readback to count as live. The AC pushes
/// on every change and Fin-ESP re-queries every QUERY_ACTIVE/IDLE interval, so a healthy session
/// is never close to this.
const READBACK_MAX_AGE_MS: u64 = 90_000;

/// Some(..) only while the AC is connected, has sent its full value set, and is still talking.
/// None means "no trustworthy readback" -- callers must then not claim anything was confirmed.
pub fn readback() -> Option<Readback> {
    let mut out = None;
    with_shared(|sh| {
        let st = &sh.st;
        if !st.connected || !st.values || st.last_frame_ms == 0 {
            return;
        }
        if now_ms().saturating_sub(st.last_frame_ms) > READBACK_MAX_AGE_MS {
            return;
        }
        let (Some(p), Some(m)) = (sh.regs.get(0x1f7), sh.regs.get(0x1f9)) else { return };
        out = Some(Readback {
            power: p == 1,
            mode: m,
            fan: sh.regs.get(0x1fa),
            target_half: sh.regs.get(0x1fe),
            swing_v: sh.regs.get(0x321),
            swing_h: sh.regs.get(0x322),
            auto_dry: sh.regs.get(0x20e),
            power_limit: sh.regs.get(0x20d),
            session: st.mqtt_sessions,
        });
    });
    out
}

// ── Status JSON (GET /ac/cloud) ─────────────────────────────────────────────────────────────────

pub fn status_json() -> String {
    let mut s = String::new();
    with_shared(|sh| {
        let st = &sh.st;
        let r = &sh.regs;
        let opt = |v: Option<u32>| v.map(|x| x.to_string()).unwrap_or_else(|| "null".into());
        let half = |v: Option<u32>| v.map(|x| format!("{:.1}", x as f32 / 2.0)).unwrap_or_else(|| "null".into());
        let tbl = |t: &[f32; 255], v: Option<u32>| v.and_then(|x| tlv::table_temp(t, x)).map(|x| format!("{x:.1}")).unwrap_or_else(|| "null".into());
        let now = now_ms();
        let ago = |t: u64| if t == 0 { "null".to_string() } else { (now.saturating_sub(t) / 1000).to_string() };
        let mut regs = String::with_capacity(r.len * 12);
        for i in 0..r.len {
            if i > 0 { regs.push(','); }
            regs.push_str(&format!("\"0x{:03x}\":{}", r.ids[i], r.vals[i]));
        }
        s = format!(
            concat!(
                "{{\"enabled\":{en},\"phase\":\"{ph}\",\"host\":\"{host}\",",
                "\"https\":{{\"ok\":{hok},\"fail\":{hf},\"last\":\"{hl}\"}},",
                "\"tls\":{{\"rejected_heap\":{rh},\"last_handshake_ms\":{hs},\"heap_after_handshake\":{hah},\"min_heap_after_handshake\":{mhah}}},",
                "\"mqtt\":{{\"connected\":{con},\"sessions\":{ms},\"fail\":{mf},\"client_id\":\"{cid}\",\"level\":{lv},\"keepalive\":{ka},\"subs\":\"{subs}\",",
                "\"connected_s\":{csec},\"last_rx_s_ago\":{lrx},\"pub_in\":{pin},\"pub_out\":{pout},\"pub_dropped\":{pdr},\"qos2_ignored\":{q2}}},",
                "\"clip\":{{\"did\":\"{did}\",\"model\":\"{model}\",\"soft_ver\":\"{sv}\",\"protocol_ver\":\"{pv}\",\"device_type\":\"{dt}\",",
                "\"deploys\":{dep},\"device_ready\":{dr},\"caps\":{caps},\"values\":{vals},\"frames\":{fr},\"crc_bad\":{cb},\"private\":{fp},\"unknown\":{fu},",
                "\"last_frame_s_ago\":{lf},\"queries\":{qu},\"timesyncs\":{ts},\"timesync_skipped\":{tss},\"bad_json\":{bj},\"other_cmds\":{oc},\"prov_acks\":{pa}}},",
                "\"ac\":{{\"power\":{pw},\"mode\":{md},\"fan\":{fan},\"room_c\":{rc},\"target_c\":{tc},\"outdoor_c\":{oc2},\"odu_coil_c\":{coil},",
                "\"compressor_hz\":{hz},\"eev\":{eev},\"fan_rpm_raw\":{rpm},\"error\":{err}}},",
                "\"regs\":{{{regs}}},",
                "\"last_err\":\"{le}\",\"stack_hwm\":{hwm},\"pressure_drops\":{pd},\"heap\":{{\"free\":{hfree},\"largest\":{hlarge}}}}}"
            ),
            en = crate::config::AC_CLOUD_ENABLED,
            ph = st.phase,
            host = crate::config::AC_CLOUD_HOSTNAME,
            hok = st.https_ok, hf = st.https_fail, hl = json_escape(&st.last_route),
            rh = st.rejected_heap, hs = st.last_hs_ms, hah = st.heap_after_hs, mhah = st.min_heap_after_hs,
            con = st.connected, ms = st.mqtt_sessions, mf = st.mqtt_fail, cid = json_escape(&st.client_id),
            lv = st.mqtt_level, ka = st.keepalive, subs = json_escape(&st.subs),
            csec = if st.connected { now.saturating_sub(st.since_ms) / 1000 } else { 0 },
            lrx = if st.connected { ago(st.last_rx_ms) } else { "null".into() },
            pin = st.pub_in, pout = st.pub_out, pdr = st.pub_dropped, q2 = st.qos2_ignored,
            did = json_escape(&st.did), model = json_escape(&st.model), sv = json_escape(&st.soft_ver),
            pv = json_escape(&st.proto_ver), dt = json_escape(&st.device_type),
            dep = st.deploys, dr = st.device_ready, caps = st.caps, vals = st.values, fr = st.frames,
            cb = st.crc_bad, fp = st.frames_private, fu = st.frames_unknown, lf = ago(st.last_frame_ms),
            qu = st.queries, ts = st.timesyncs, tss = st.timesync_skipped, bj = st.bad_json,
            oc = st.other_cmds, pa = st.provisioning_acks,
            pw = opt(r.get(0x1f7)), md = opt(r.get(0x1f9)), fan = opt(r.get(0x1fa)),
            rc = half(r.get(0x1fd)), tc = half(r.get(0x1fe)),
            oc2 = tbl(&tlv::RAC_AIR_TEMP, r.get(0x332)), coil = tbl(&tlv::RAC_PIPE_TEMP, r.get(0x32c)),
            hz = opt(r.get(0x22a)), eev = opt(r.get(0x330)), rpm = opt(r.get(0x331)), err = opt(r.get(0x221)),
            regs = regs,
            le = json_escape(&st.last_err), hwm = st.stack_hwm, pd = st.pressure_drops,
            hfree = heap_free(), hlarge = heap_largest(),
        );
    });
    s
}
