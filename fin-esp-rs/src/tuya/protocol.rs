/// Tuya LAN protocol constants and message framing.

// Headers
pub const HEAD_55: [u8; 4] = [0x00, 0x00, 0x55, 0xAA];
pub const HEAD_66: [u8; 4] = [0x00, 0x00, 0x66, 0x99];

// Suffixes
pub const SUF_34: [u8; 4] = [0x00, 0x00, 0xAA, 0x55];
pub const SUF_35: [u8; 4] = [0x00, 0x00, 0x99, 0x66];

// Fixed local nonce ("0123456789abcdef")
pub const LOCAL_NONCE: [u8; 16] = [
    0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x61, 0x62, 0x63, 0x64, 0x65,
    0x66,
];

// Commands
pub const CMD_NEGOTIATE: u8 = 0x03;
pub const CMD_NEGOTIATE_FINISH: u8 = 0x05;
pub const CMD_CONTROL: u8 = 0x0D;
pub const CMD_STATUS: u8 = 0x10;

/// Build a protocol 3.5 (6699) message frame.
/// Encrypts `plaintext` with AES-GCM using the header bytes [4..18] as AAD,
/// matching tinytuya's `pack_message` behaviour.
pub fn build_msg_35(
    seq: u32,
    cmd: u8,
    key: &[u8; 16],
    iv: &[u8; 12],
    plaintext: &[u8],
) -> Option<Vec<u8>> {
    // data_len = IV(12) + ciphertext(n) + tag(16); suffix not counted
    let data_len = 12 + plaintext.len() + 16;
    let total = 18 + 12 + plaintext.len() + 16 + 4;
    let mut msg = Vec::with_capacity(total);

    msg.extend_from_slice(&HEAD_66);
    msg.extend_from_slice(&[0x00, 0x00]);
    msg.extend_from_slice(&seq.to_be_bytes());
    msg.extend_from_slice(&[0x00, 0x00, 0x00, cmd]);
    msg.extend_from_slice(&(data_len as u32).to_be_bytes());

    // AAD = header bytes after the magic prefix (bytes 4..18)
    let aad = msg[4..18].to_vec();

    let encrypted = super::crypto::aes_gcm_encrypt(plaintext, key, iv, &aad)?;

    msg.extend_from_slice(iv);
    msg.extend_from_slice(&encrypted);
    msg.extend_from_slice(&SUF_35);

    Some(msg)
}

/// Parse a received packet, returning (command, decrypted_payload_json).
/// Handles both 55AA and 6699 framing.
pub fn parse_response(data: &[u8], session_key: &[u8; 16]) -> Option<(u8, String)> {
    if data.len() < 20 {
        return None;
    }

    // Find sync header
    let (offset, is_35) = find_header(data)?;
    let pkt = &data[offset..];

    if is_35 {
        parse_response_35(pkt, session_key)
    } else {
        parse_response_34(pkt, session_key)
    }
}

pub fn find_header(data: &[u8]) -> Option<(usize, bool)> {
    for i in 0..data.len().saturating_sub(3) {
        if data[i..i + 4] == HEAD_66 {
            return Some((i, true));
        }
        if data[i..i + 4] == HEAD_55 {
            return Some((i, false));
        }
    }
    None
}

fn parse_response_35(pkt: &[u8], session_key: &[u8; 16]) -> Option<(u8, String)> {
    if pkt.len() < 22 {
        return None;
    }
    let cmd = pkt[13];
    let data_len = u32::from_be_bytes([pkt[14], pkt[15], pkt[16], pkt[17]]) as usize;

    if pkt.len() < 18 + data_len {
        return None;
    }

    let iv_start = 18;
    if data_len < 12 {
        return Some((cmd, String::new()));
    }
    let mut iv = [0u8; 12];
    iv.copy_from_slice(&pkt[iv_start..iv_start + 12]);

    let aad = &pkt[4..18];
    let enc_start = iv_start + 12;
    let enc_end = 18 + data_len;
    if enc_end <= enc_start {
        return Some((cmd, String::new()));
    }
    let encrypted = &pkt[enc_start..enc_end];

    match super::crypto::aes_gcm_decrypt(encrypted, session_key, &iv, aad) {
        Some(plain) => {
            let json = extract_json(&plain);
            Some((cmd, json))
        }
        None => {
            // Decryption failed, try with device key? Return empty.
            Some((cmd, String::new()))
        }
    }
}

fn parse_response_34(pkt: &[u8], session_key: &[u8; 16]) -> Option<(u8, String)> {
    if pkt.len() < 20 {
        return None;
    }
    let cmd = pkt[11];
    let data_len = u32::from_be_bytes([pkt[12], pkt[13], pkt[14], pkt[15]]) as usize;

    // Return code might be at offset 16-19
    let rc_offset = 16;
    let payload_start = if data_len > 4 { rc_offset + 4 } else { rc_offset };
    let payload_end = 16 + data_len - 4; // exclude CRC
    if payload_end <= payload_start || payload_end > pkt.len() {
        return Some((cmd, String::new()));
    }

    let mut payload = pkt[payload_start..payload_end].to_vec();
    super::crypto::aes_ecb_decrypt(&mut payload, session_key);
    let json = extract_json(&payload);
    Some((cmd, json))
}

/// Strip padding and version prefixes to find the JSON payload.
fn extract_json(data: &[u8]) -> String {
    // Skip leading bytes < 0x20 (non-printable / padding)
    let start = data.iter().position(|&b| b == b'{').unwrap_or(data.len());
    let end = data.iter().rposition(|&b| b == b'}').map(|p| p + 1).unwrap_or(start);
    if start < end {
        String::from_utf8_lossy(&data[start..end]).to_string()
    } else {
        String::new()
    }
}
