use log::{info, warn};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpStream};
use std::time::Duration;

use esp_idf_sys;

use super::crypto;
use super::protocol::{self, *};

pub struct Session {
    stream: TcpStream,
    device_key: [u8; 16],
    session_key: [u8; 16],
    version: u8,
    seq: u32,
    key_established: bool,
}

impl Session {
    pub fn connect(
        ip: [u8; 4],
        port: u16,
        device_key: &[u8; 16],
        version: u8,
    ) -> Result<Self, String> {
        let addr = Ipv4Addr::new(ip[0], ip[1], ip[2], ip[3]);
        info!("[tuya] connecting to {}:{}", addr, port);

        let stream = TcpStream::connect_timeout(
            &(addr, port).into(),
            Duration::from_secs(4),
        )
        .map_err(|e| format!("TCP connect: {e}"))?;

        stream.set_read_timeout(Some(Duration::from_secs(4))).ok();
        stream.set_write_timeout(Some(Duration::from_secs(4))).ok();

        info!("[tuya] TCP connected");

        Ok(Self {
            stream,
            device_key: *device_key,
            session_key: [0u8; 16],
            version,
            seq: 1,
            key_established: false,
        })
    }

    /// Perform the two-step key negotiation.
    pub fn negotiate_key(&mut self) -> bool {
        info!("[tuya] key negotiation starting");

        let remote_nonce = match self.request_remote_nonce() {
            Some(n) => n,
            None => {
                warn!("[tuya] failed to get remote nonce");
                return false;
            }
        };

        self.finalize_negotiation(&remote_nonce);
        self.key_established = true;
        self.seq = 3;
        info!("[tuya] key negotiation complete");
        true
    }

    /// Query lamp status (command 0x10 with payload "{}").
    pub fn query_status(&mut self) -> Option<String> {
        if !self.key_established {
            return None;
        }

        let payload = b"{}";
        self.send_command(CMD_STATUS, payload)?;

        let raw = self.receive_raw();
        match &raw {
            Some(r) => info!("[tuya] status rx {} bytes: {:02x?}", r.len(), &r[..r.len().min(48)]),
            None    => info!("[tuya] status rx: timeout"),
        }
        let raw = raw?;
        match protocol::parse_response(&raw, &self.session_key) {
            Some((cmd, json)) => {
                info!("[tuya] status parsed cmd={:#04x} json={:?}", cmd, &json[..json.len().min(80)]);
                if json.is_empty() { None } else { Some(json) }
            }
            None => {
                info!("[tuya] status parse failed");
                None
            }
        }
    }

    /// Send a DPS control command (command 0x0D).
    pub fn send_dps_command(&mut self, dps_json: &str) -> bool {
        if !self.key_established {
            return false;
        }

        let ts = unsafe { esp_idf_sys::time(core::ptr::null_mut()) } as u64;
        let json = format!(
            r#"{{"protocol":5,"t":{},"data":{{"dps":{{{}}}}}}}"#,
            ts, dps_json
        );

        // Protocol 3.5 DPS payloads are prefixed with the version string "3.5" + 12 null bytes.
        let mut payload = Vec::with_capacity(15 + json.len());
        payload.extend_from_slice(b"3.5\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00");
        payload.extend_from_slice(json.as_bytes());

        if self.send_command(CMD_CONTROL, &payload).is_none() {
            return false;
        }

        let _ = self.receive_raw();
        let _ = self.receive_raw();

        true
    }

    fn request_remote_nonce(&mut self) -> Option<[u8; 16]> {
        if self.version >= 5 {
            // Protocol 3.5 client-initiated: we send CMD_NEGOTIATE with our local
            // nonce, device replies with its remote nonce.
            let mut iv = [0u8; 12];
            iv.copy_from_slice(&LOCAL_NONCE[..12]);
            let msg = protocol::build_msg_35(self.seq, CMD_NEGOTIATE, &self.device_key, &iv, &LOCAL_NONCE)?;
            self.seq += 1;
            info!("[tuya] tx {} bytes: {:02x?}", msg.len(), &msg[..msg.len().min(32)]);
            self.stream.write_all(&msg).ok()?;

            let response = self.receive_raw();
            match &response {
                Some(r) => info!("[tuya] rx {} bytes: {:02x?}", r.len(), &r[..r.len().min(32)]),
                None    => info!("[tuya] rx: timeout"),
            }
            let response = response?;

            self.parse_35_nonce(&response)
        } else {
            // Protocol 3.4: client-initiated
            let mut padded = LOCAL_NONCE;
            crypto::aes_ecb_encrypt(&mut padded, &self.device_key);
            let msg = self.build_msg_34(CMD_NEGOTIATE, &padded);
            self.seq += 1;
            self.stream.write_all(&msg).ok()?;

            let response = self.receive_raw()?;
            let offset = response.windows(4).position(|w| w == HEAD_55)?;
            let pkt = &response[offset..];
            let data_len = u32::from_be_bytes([pkt[12], pkt[13], pkt[14], pkt[15]]) as usize;
            let start = 20;
            let end = 16 + data_len - 4;
            if end <= start || end > pkt.len() {
                return None;
            }
            let mut payload = pkt[start..end].to_vec();
            crypto::aes_ecb_decrypt(&mut payload, &self.device_key);
            if payload.len() < 16 {
                return None;
            }
            let mut nonce = [0u8; 16];
            nonce.copy_from_slice(&payload[..16]);
            Some(nonce)
        }
    }

    /// Parse a 6699-framed packet and decrypt the 16-byte nonce inside.
    fn parse_35_nonce(&self, response: &[u8]) -> Option<[u8; 16]> {
        let offset = response.windows(4).position(|w| w == HEAD_66)?;
        let pkt = &response[offset..];
        if pkt.len() < 34 {
            return None;
        }
        let aad = &pkt[4..18];
        let mut iv = [0u8; 12];
        iv.copy_from_slice(&pkt[18..30]);
        let data_len = u32::from_be_bytes([pkt[14], pkt[15], pkt[16], pkt[17]]) as usize;
        let enc_start = 30;
        let enc_end = 18 + data_len;
        if enc_end <= enc_start || enc_end > pkt.len() {
            return None;
        }
        let decrypted =
            crypto::aes_gcm_decrypt(&pkt[enc_start..enc_end], &self.device_key, &iv, aad)?;
        // Device prepends a 4-byte return code before the 16-byte nonce.
        if decrypted.len() < 20 {
            return None;
        }
        let mut nonce = [0u8; 16];
        nonce.copy_from_slice(&decrypted[4..20]);
        Some(nonce)
    }

    fn finalize_negotiation(&mut self, remote_nonce: &[u8; 16]) {
        let hmac = crypto::hmac_sha256(&self.device_key, remote_nonce);

        if self.version >= 5 {
            let mut iv = [0u8; 12];
            iv.copy_from_slice(&LOCAL_NONCE[..12]);
            if let Some(msg) = protocol::build_msg_35(self.seq, CMD_NEGOTIATE_FINISH, &self.device_key, &iv, &hmac) {
                self.seq += 1;
                let _ = self.stream.write_all(&msg);
            }
        } else {
            let mut padded = [0x10u8; 48];
            padded[..32].copy_from_slice(&hmac);
            crypto::aes_ecb_encrypt(&mut padded, &self.device_key);
            let msg = self.build_msg_34(CMD_NEGOTIATE_FINISH, &padded);
            self.seq += 1;
            let _ = self.stream.write_all(&msg);
        }

        self.session_key =
            crypto::derive_session_key(&LOCAL_NONCE, remote_nonce, &self.device_key, self.version);
    }

    fn send_command(&mut self, cmd: u8, payload: &[u8]) -> Option<()> {
        if self.version >= 5 {
            let mut iv = [0u8; 12];
            iv.copy_from_slice(&LOCAL_NONCE[..12]);
            let msg = protocol::build_msg_35(self.seq, cmd, &self.session_key, &iv, payload)?;
            self.seq += 1;
            self.stream.write_all(&msg).ok()
        } else {
            let pad_len = (16 - (payload.len() % 16)) % 16;
            let mut padded = payload.to_vec();
            padded.resize(payload.len() + pad_len, pad_len as u8);
            crypto::aes_ecb_encrypt(&mut padded, &self.session_key);
            let msg = self.build_msg_34(cmd, &padded);
            self.seq += 1;
            self.stream.write_all(&msg).ok()
        }
    }

    fn receive_and_parse(&mut self) -> Option<(u8, String)> {
        let data = self.receive_raw()?;
        protocol::parse_response(&data, &self.session_key)
    }

    fn receive_raw(&mut self) -> Option<Vec<u8>> {
        let mut buf = [0u8; 512];
        let mut total = Vec::new();

        loop {
            match self.stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    total.extend_from_slice(&buf[..n]);
                    self.stream
                        .set_read_timeout(Some(Duration::from_millis(100)))
                        .ok();
                }
                Err(_) => break,
            }
        }

        self.stream
            .set_read_timeout(Some(Duration::from_secs(4)))
            .ok();

        if total.is_empty() {
            None
        } else {
            Some(total)
        }
    }

    fn build_msg_34(&self, cmd: u8, encrypted_payload: &[u8]) -> Vec<u8> {
        let data_len = encrypted_payload.len() + 4 + 4;
        let mut msg = Vec::with_capacity(16 + encrypted_payload.len() + 8);

        msg.extend_from_slice(&HEAD_55);
        msg.extend_from_slice(&self.seq.to_be_bytes());
        msg.extend_from_slice(&[0x00, 0x00, 0x00, cmd]);
        msg.extend_from_slice(&(data_len as u32).to_be_bytes());
        msg.extend_from_slice(encrypted_payload);

        let crc = crc32(&msg);
        msg.extend_from_slice(&crc.to_be_bytes());
        msg.extend_from_slice(&SUF_34);

        msg
    }
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFFFFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB88320;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}
