//! LG "clip" TLV frames, ported from rethink (util/tlv.ts, util/crc16.ts, cloud/devices/tlv_device.ts).
//! Pure functions, no allocation beyond the returned Vec, never panics (all indexing is checked).
//!
//! Frame (as carried hex-encoded in a clip `device_packet` / `packet` message):
//!   b0 b1 | 04 00 00 00 | dir (0x87/0xa7 from AC, 0x65 to AC) | 02 | 01/04 | seq | len | TLV.. | crc16(be)
//! where len = total - 13 and the CRC (XMODEM: poly 0x1021, init 0) covers everything from byte 2 to
//! the end of the TLV payload.
//! TLV element: 12-bit type, 2-bit value-length (0 => 4-bit value inline), value big-endian.

/// CRC-16/XMODEM, bitwise (no 512-byte table in DRAM; frames are ~100 bytes so speed is irrelevant).
pub fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &b in data {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
        }
    }
    crc
}

/// Parse a TLV payload. Truncated trailing elements are dropped (same as rethink).
pub fn parse(buf: &[u8], mut f: impl FnMut(u16, u32)) -> usize {
    let mut i = 0usize;
    let mut n = 0usize;
    while i + 2 <= buf.len() {
        let (b0, b1) = (buf[i], buf[i + 1]);
        let t = ((b0 as u16) << 2) | ((b1 as u16) >> 6);
        let l = ((b1 >> 4) & 3) as usize;
        let mut v = (b1 & 0x0f) as u32;
        if i + 2 + l > buf.len() {
            break;
        }
        if l > 0 {
            v = 0;
            for j in 0..l {
                v = (v << 8) | buf[i + 2 + j] as u32;
            }
        }
        f(t, v);
        n += 1;
        i += 2 + l;
    }
    n
}

/// Encode TLV elements (rethink TLV.build).
pub fn build(elems: &[(u16, u32)], out: &mut Vec<u8>) {
    for &(t, v) in elems {
        out.push(((t >> 2) & 0xff) as u8);
        let tl = ((t & 3) << 6) as u8;
        if v < 0x10 {
            out.push(tl | v as u8);
        } else if v < 0x100 {
            out.push(tl | 0x10);
            out.push(v as u8);
        } else if v < 0x10000 {
            out.push(tl | 0x20);
            out.push((v >> 8) as u8);
            out.push(v as u8);
        } else {
            out.push(tl | 0x30);
            out.push((v >> 16) as u8);
            out.push((v >> 8) as u8);
            out.push(v as u8);
        }
    }
}

/// rethink TLVDevice.send(header, tlv): header = [b0,b1,b2,b3,b4].
pub fn build_frame(header: [u8; 5], elems: &[(u16, u32)]) -> Vec<u8> {
    let mut tlv = Vec::with_capacity(16);
    build(elems, &mut tlv);
    let mut body = Vec::with_capacity(13 + tlv.len());
    body.extend_from_slice(&[0x04, 0x00, 0x00, 0x00, 0x65, header[2], header[3], header[4], tlv.len() as u8]);
    body.extend_from_slice(&tlv);
    let crc = crc16(&body);
    let mut out = Vec::with_capacity(body.len() + 4);
    out.push(header[0]);
    out.push(header[1]);
    out.extend_from_slice(&body);
    out.push((crc >> 8) as u8);
    out.push(crc as u8);
    out
}

/// Capabilities query (rethink TLVDevice.queryCaps).
pub fn caps_query() -> Vec<u8> { build_frame([1, 1, 2, 2, 1], &[(0x1f5, 1)]) }
/// Values query (rethink TLVDevice.query).
pub fn values_query() -> Vec<u8> { build_frame([1, 1, 2, 2, 1], &[(0x1f5, 2)]) }

pub enum FrameKind<'a> {
    /// Standard TLV report from the appliance; payload slice + whether the CRC matched.
    Tlv(&'a [u8], bool),
    /// 0xfd private/extended frame (filter data etc.) -- recognised, not decoded.
    Private,
    Unknown,
}

/// Classify a frame received from the AC (rethink TLVDevice.processData's checks).
pub fn classify(buf: &[u8]) -> FrameKind<'_> {
    let len = buf.len();
    if len < 13 || buf[2..6] != [0x04, 0x00, 0x00, 0x00] {
        return FrameKind::Unknown;
    }
    let from_ac = buf[6] == 0x87 || buf[6] == 0xa7;
    if from_ac && buf[7] == 0x02 && (buf[8] == 0x01 || buf[8] == 0x04) && buf[10] as usize == len - 13 {
        let crc_ok = crc16(&buf[2..len - 2]) == (((buf[len - 2] as u16) << 8) | buf[len - 1] as u16);
        return FrameKind::Tlv(&buf[11..len - 2], crc_ok);
    }
    if buf[6] == 0x87 && buf[7] == 0xfd {
        return FrameKind::Private;
    }
    FrameKind::Unknown
}

/// Hex decode (upper or lower case). None on odd length / bad digit / over `max` bytes.
pub fn hex_decode(s: &str, max: usize) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    if b.len() % 2 != 0 || b.len() / 2 > max {
        return None;
    }
    fn nib(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(b.len() / 2);
    for pair in b.chunks(2) {
        out.push((nib(pair[0])? << 4) | nib(pair[1])?);
    }
    Some(out)
}

pub fn hex_upper(buf: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789ABCDEF";
    let mut s = String::with_capacity(buf.len() * 2);
    for &b in buf {
        s.push(H[(b >> 4) as usize] as char);
        s.push(H[(b & 15) as usize] as char);
    }
    s
}

/// Decoded register table: type-id -> last value. Fixed capacity (no growth), 79 ids seen live.
pub const REG_CAP: usize = 128;

pub struct Registers {
    pub ids: [u16; REG_CAP],
    pub vals: [u32; REG_CAP],
    pub len: usize,
}

impl Registers {
    pub const fn new() -> Self {
        Self { ids: [0; REG_CAP], vals: [0; REG_CAP], len: 0 }
    }
    pub fn clear(&mut self) { self.len = 0; }
    pub fn set(&mut self, t: u16, v: u32) {
        for i in 0..self.len {
            if self.ids[i] == t {
                self.vals[i] = v;
                return;
            }
        }
        if self.len < REG_CAP {
            self.ids[self.len] = t;
            self.vals[self.len] = v;
            self.len += 1;
        }
    }
    pub fn get(&self, t: u16) -> Option<u32> {
        (0..self.len).find(|&i| self.ids[i] == t).map(|i| self.vals[i])
    }
}

/// rethink util/ac_tables.ts racAirTemp (index = 255 - raw; raw 0 = index 255 undefined)
pub static RAC_AIR_TEMP: [f32; 255] = [300.00,214.42,177.12,157.75,144.95,135.49,128.04,121.93,116.77,112.31,108.38,104.89,101.74,98.88,96.26,93.84,91.60,89.51,87.56,85.72,83.98,82.34,80.78,79.30,77.89,76.54,75.24,74.00,72.81,71.66,70.55,69.48,68.45,67.45,66.48,65.54,64.63,63.74,62.88,62.04,61.22,60.43,59.65,58.89,58.14,57.42,56.71,56.01,55.33,54.66,54.01,53.36,52.73,52.11,51.50,50.91,50.32,49.74,49.17,48.61,48.06,47.51,46.98,46.45,45.93,45.41,44.91,44.40,43.91,43.42,42.94,42.46,41.99,41.53,41.07,40.61,40.16,39.71,39.27,38.84,38.40,37.97,37.55,37.13,36.71,36.30,35.89,35.48,35.08,34.68,34.28,33.89,33.50,33.11,32.72,32.34,31.96,31.58,31.21,30.83,30.46,30.09,29.73,29.36,29.00,28.64,28.28,27.93,27.57,27.22,26.87,26.52,26.17,25.82,25.48,25.13,24.79,24.45,24.11,23.77,23.43,23.09,22.76,22.42,22.09,21.76,21.42,21.09,20.76,20.43,20.10,19.78,19.45,19.12,18.79,18.47,18.14,17.82,17.49,17.16,16.84,16.52,16.19,15.87,15.54,15.22,14.90,14.57,14.25,13.92,13.60,13.27,12.95,12.63,12.30,11.97,11.65,11.32,10.99,10.67,10.34,10.01,9.68,9.35,9.02,8.69,8.35,8.02,7.69,7.35,7.01,6.68,6.34,6.00,5.65,5.31,4.97,4.62,4.27,3.92,3.57,3.22,2.86,2.50,2.14,1.78,1.42,1.05,0.68,0.31,-0.06,-0.44,-0.82,-1.20,-1.58,-1.97,-2.37,-2.76,-3.16,-3.56,-3.97,-4.38,-4.80,-5.22,-5.64,-6.07,-6.51,-6.95,-7.39,-7.85,-8.30,-8.77,-9.24,-9.72,-10.20,-10.70,-11.20,-11.71,-12.23,-12.76,-13.30,-13.85,-14.41,-14.98,-15.57,-16.17,-16.79,-17.42,-18.06,-18.73,-19.42,-20.12,-20.85,-21.61,-22.39,-23.20,-24.04,-24.92,-25.83,-26.80,-27.81,-28.88,-30.01,-31.21,-32.51,-33.90,-35.42,-37.08,-38.94,-41.03,-43.45,-46.33,-49.93,-54.79,-62.60];
/// rethink util/ac_tables.ts racPipeTemp (index = 255 - raw)
pub static RAC_PIPE_TEMP: [f32; 255] = [300.00,224.93,184.71,163.98,150.32,140.26,132.36,125.88,120.42,115.70,111.56,107.87,104.56,101.54,98.79,96.24,93.89,91.69,89.64,87.71,85.89,84.16,82.53,80.98,79.50,78.08,76.73,75.43,74.18,72.98,71.82,70.70,69.62,68.58,67.57,66.58,65.63,64.71,63.81,62.93,62.08,61.24,60.43,59.64,58.86,58.11,57.37,56.64,55.93,55.23,54.55,53.88,53.23,52.58,51.95,51.32,50.71,50.11,49.52,48.93,48.36,47.79,47.24,46.69,46.15,45.61,45.09,44.57,44.05,43.54,43.04,42.55,42.06,41.58,41.10,40.62,40.16,39.69,39.24,38.78,38.33,37.89,37.45,37.01,36.58,36.15,35.72,35.30,34.88,34.47,34.06,33.65,33.24,32.84,32.44,32.05,31.65,31.26,30.87,30.49,30.10,29.72,29.34,28.96,28.59,28.22,27.85,27.48,27.11,26.74,26.38,26.02,25.66,25.30,24.94,24.59,24.23,23.88,23.53,23.18,22.83,22.48,22.13,21.79,21.44,21.10,20.75,20.41,20.07,19.73,19.39,19.05,18.71,18.37,18.04,17.70,17.36,17.03,16.69,16.36,16.02,15.69,15.35,15.02,14.68,14.35,14.01,13.68,13.35,13.01,12.68,12.34,12.01,11.67,11.34,11.00,10.66,10.33,9.99,9.65,9.32,8.98,8.64,8.30,7.96,7.61,7.27,6.93,6.58,6.24,5.89,5.54,5.19,4.84,4.49,4.14,3.78,3.43,3.07,2.71,2.35,1.98,1.62,1.25,0.88,0.51,0.13,-0.24,-0.62,-1.00,-1.39,-1.77,-2.16,-2.56,-2.95,-3.35,-3.76,-4.16,-4.57,-4.99,-5.41,-5.83,-6.25,-6.69,-7.12,-7.56,-8.01,-8.46,-8.92,-9.38,-9.85,-10.33,-10.81,-11.30,-11.80,-12.31,-12.82,-13.34,-13.88,-14.42,-14.97,-15.54,-16.11,-16.70,-17.30,-17.92,-18.55,-19.19,-19.85,-20.54,-21.24,-21.96,-22.71,-23.48,-24.27,-25.10,-25.96,-26.86,-27.80,-28.78,-29.81,-30.90,-32.06,-33.29,-34.61,-36.03,-37.58,-39.27,-41.16,-43.29,-45.76,-48.69,-52.34,-57.28,-65.19];

/// Temperature via one of the tables above; None for the undefined/sentinel raw value 0.
pub fn table_temp(table: &[f32; 255], raw: u32) -> Option<f32> {
    if raw == 0 || raw > 255 { return None; }
    table.get(255 - raw as usize).copied()
}
