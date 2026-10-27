//! Minimal MQTT 3.1 / 3.1.1 codec for a single trusted-ish client (the AC). Covers exactly what
//! rethink's broker (cloud/mqtt-broker.ts) handles: CONNECT/CONNACK, SUBSCRIBE/SUBACK,
//! UNSUBSCRIBE/UNSUBACK, PUBLISH (QoS 0/1 in, QoS 0 out) + PUBACK, PINGREQ/PINGRESP, DISCONNECT.
//! All parsing is bounds-checked; malformed input yields None/Err, never a panic.

/// Largest packet we accept (AC messages are < 2 KB; deploy JSON is the biggest).
pub const MAX_PACKET: usize = 4096;

pub enum Packet<'a> {
    Connect { level: u8, client_id: &'a str, keepalive: u16, has_user: bool, has_will: bool },
    Publish { topic: &'a str, payload: &'a [u8], qos: u8, pid: u16, retain: bool },
    Subscribe { pid: u16, topics: Vec<&'a str> },
    Unsubscribe { pid: u16, topics: Vec<&'a str> },
    PingReq,
    Disconnect,
    /// Anything else we don't act on (PUBACK from client, etc.) -- type nibble.
    Other(u8),
}

/// If `buf` starts with a complete packet: Ok(Some((header_len, total_len))).
/// Ok(None) = need more bytes. Err = malformed or over MAX_PACKET.
pub fn frame(buf: &[u8]) -> Result<Option<(usize, usize)>, ()> {
    if buf.len() < 2 {
        return Ok(None);
    }
    let mut rem: usize = 0;
    let mut mult: usize = 1;
    let mut i = 1usize;
    loop {
        let Some(&b) = buf.get(i) else { return Ok(None) };
        rem += (b & 0x7f) as usize * mult;
        i += 1;
        if b & 0x80 == 0 {
            break;
        }
        mult *= 128;
        if i > 4 {
            return Err(()); // > 4 length bytes is a protocol violation
        }
    }
    let total = i + rem;
    if total > MAX_PACKET {
        return Err(());
    }
    if buf.len() < total {
        return Ok(None);
    }
    Ok(Some((i, total)))
}

struct Cur<'a> { b: &'a [u8], i: usize }
impl<'a> Cur<'a> {
    fn u8(&mut self) -> Option<u8> { let v = *self.b.get(self.i)?; self.i += 1; Some(v) }
    fn u16(&mut self) -> Option<u16> { Some(((self.u8()? as u16) << 8) | self.u8()? as u16) }
    fn bytes(&mut self) -> Option<&'a [u8]> {
        let n = self.u16()? as usize;
        let s = self.b.get(self.i..self.i.checked_add(n)?)?;
        self.i += n;
        Some(s)
    }
    fn str(&mut self) -> Option<&'a str> { core::str::from_utf8(self.bytes()?).ok() }
    fn rest(&self) -> &'a [u8] { self.b.get(self.i..).unwrap_or(&[]) }
}

/// Parse one complete packet: `first` = fixed-header byte, `body` = variable header + payload.
pub fn parse(first: u8, body: &[u8]) -> Option<Packet<'_>> {
    let mut c = Cur { b: body, i: 0 };
    match first >> 4 {
        1 => {
            let _proto = c.str()?; // "MQTT" (3.1.1) or "MQIsdp" (3.1)
            let level = c.u8()?;
            let flags = c.u8()?;
            let keepalive = c.u16()?;
            if level != 3 && level != 4 {
                // e.g. MQTT 5 (properties follow): report the level so the broker can answer
                // CONNACK rc=1 (unacceptable protocol version) instead of a bare disconnect.
                return Some(Packet::Connect { level, client_id: "", keepalive, has_user: false, has_will: false });
            }
            let client_id = c.str()?;
            Some(Packet::Connect { level, client_id, keepalive, has_user: flags & 0x80 != 0, has_will: flags & 0x04 != 0 })
        }
        3 => {
            let qos = (first >> 1) & 3;
            let retain = first & 1 != 0;
            let topic = c.str()?;
            let pid = if qos > 0 { c.u16()? } else { 0 };
            Some(Packet::Publish { topic, payload: c.rest(), qos, pid, retain })
        }
        8 => {
            let pid = c.u16()?;
            let mut topics = Vec::new();
            while c.i < body.len() && topics.len() < 16 {
                let t = c.str()?;
                let _qos = c.u8()?;
                topics.push(t);
            }
            Some(Packet::Subscribe { pid, topics })
        }
        10 => {
            let pid = c.u16()?;
            let mut topics = Vec::new();
            while c.i < body.len() && topics.len() < 16 {
                topics.push(c.str()?);
            }
            Some(Packet::Unsubscribe { pid, topics })
        }
        12 => Some(Packet::PingReq),
        14 => Some(Packet::Disconnect),
        t => Some(Packet::Other(t)),
    }
}

fn push_len(out: &mut Vec<u8>, mut n: usize) {
    loop {
        let mut b = (n % 128) as u8;
        n /= 128;
        if n > 0 {
            b |= 0x80;
        }
        out.push(b);
        if n == 0 {
            break;
        }
    }
}

pub fn connack(rc: u8) -> [u8; 4] { [0x20, 2, 0, rc] }
pub fn puback(pid: u16) -> [u8; 4] { [0x40, 2, (pid >> 8) as u8, pid as u8] }
pub fn unsuback(pid: u16) -> [u8; 4] { [0xb0, 2, (pid >> 8) as u8, pid as u8] }
pub fn pingresp() -> [u8; 2] { [0xd0, 0] }

pub fn suback(pid: u16, n: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(4 + n);
    v.push(0x90);
    push_len(&mut v, 2 + n);
    v.push((pid >> 8) as u8);
    v.push(pid as u8);
    v.extend(core::iter::repeat(0u8).take(n)); // all granted at QoS 0, like rethink
    v
}

/// QoS 0, non-retained PUBLISH.
pub fn publish(topic: &str, payload: &[u8]) -> Vec<u8> {
    let rem = 2 + topic.len() + payload.len();
    let mut v = Vec::with_capacity(rem + 5);
    v.push(0x30);
    push_len(&mut v, rem);
    v.push((topic.len() >> 8) as u8);
    v.push(topic.len() as u8);
    v.extend_from_slice(topic.as_bytes());
    v.extend_from_slice(payload);
    v
}

/// MQTT topic-filter match (`+` one level, `#` the rest), same intent as rethink's Subscription.
pub fn topic_matches(filter: &str, topic: &str) -> bool {
    let mut f = filter.split('/');
    let mut t = topic.split('/');
    loop {
        match (f.next(), t.next()) {
            (Some("#"), _) => return true,
            (Some("+"), Some(_)) => {}
            (Some(a), Some(b)) if a == b => {}
            (None, None) => return true,
            _ => return false,
        }
    }
}
