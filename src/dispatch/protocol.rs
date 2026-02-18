//! Binary protocol for dispatch lobby handshake.
//!
//! Minimal binary encoding — no serde dependency.
//! Messages are exchanged over the lobby SharedServer ring buffers.

use crate::error::{Result, ShmError};

// ─── Constants ───────────────────────────────────────────────────────────────

const DISPATCH_MAGIC: u32 = 0x4449_5350; // 'DISP'
const DISPATCH_VERSION: u8 = 1;

const MSG_TYPE_REQUEST: u8 = 1;
const MSG_TYPE_RESPONSE: u8 = 2;

const MAX_NAME_LEN: usize = 64;
const MAX_CHANNEL_NAME_LEN: usize = 64;

/// Response status: success.
pub const STATUS_OK: u8 = 0;
/// Response status: server rejected the connection.
pub const STATUS_REJECTED: u8 = 1;

// ─── Registration request (client → server) ─────────────────────────────────

/// Data sent by client during lobby registration.
#[derive(Debug, Clone)]
pub struct RegistrationRequest {
    pub pid: u32,
    pub bits: u8,
    pub revision: u16,
    pub name: String,
}

/// Encode a registration request into bytes.
///
/// Layout:
/// ```text
/// [0..4]   magic: u32 LE
/// [4..5]   version: u8
/// [5..6]   msg_type: u8 = 1
/// [6..10]  pid: u32 LE
/// [10..11] bits: u8
/// [11..13] revision: u16 LE
/// [13..14] name_len: u8
/// [14..]   name: UTF-8 bytes
/// ```
pub fn encode_request(req: &RegistrationRequest) -> Vec<u8> {
    let name_bytes = req.name.as_bytes();
    let name_len = name_bytes.len().min(MAX_NAME_LEN) as u8;
    let total = 14 + name_len as usize;
    let mut buf = Vec::with_capacity(total);

    buf.extend_from_slice(&DISPATCH_MAGIC.to_le_bytes());
    buf.push(DISPATCH_VERSION);
    buf.push(MSG_TYPE_REQUEST);
    buf.extend_from_slice(&req.pid.to_le_bytes());
    buf.push(req.bits);
    buf.extend_from_slice(&req.revision.to_le_bytes());
    buf.push(name_len);
    buf.extend_from_slice(&name_bytes[..name_len as usize]);

    buf
}

/// Decode a registration request from bytes.
pub fn decode_request(data: &[u8]) -> Result<RegistrationRequest> {
    if data.len() < 14 {
        return Err(ShmError::MessageTooSmall);
    }

    let magic = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
    if magic != DISPATCH_MAGIC {
        return Err(ShmError::Corrupted);
    }

    let version = data[4];
    if version != DISPATCH_VERSION {
        return Err(ShmError::HandshakeFailed);
    }

    let msg_type = data[5];
    if msg_type != MSG_TYPE_REQUEST {
        return Err(ShmError::HandshakeFailed);
    }

    let pid = u32::from_le_bytes([data[6], data[7], data[8], data[9]]);
    let bits = data[10];
    let revision = u16::from_le_bytes([data[11], data[12]]);
    let name_len = data[13] as usize;

    if data.len() < 14 + name_len {
        return Err(ShmError::MessageTooSmall);
    }

    let name = String::from_utf8_lossy(&data[14..14 + name_len]).into_owned();

    Ok(RegistrationRequest {
        pid,
        bits,
        revision,
        name,
    })
}

// ─── Registration response (server → client) ────────────────────────────────

/// Data sent by server after processing registration.
#[derive(Debug, Clone)]
pub struct RegistrationResponse {
    pub status: u8,
    pub client_id: u32,
    pub channel_name: String,
}

/// Encode a registration response into bytes.
///
/// Layout:
/// ```text
/// [0..4]   magic: u32 LE
/// [4..5]   version: u8
/// [5..6]   msg_type: u8 = 2
/// [6..7]   status: u8
/// [7..11]  client_id: u32 LE
/// [11..12] channel_name_len: u8
/// [12..]   channel_name: UTF-8 bytes
/// ```
pub fn encode_response(resp: &RegistrationResponse) -> Vec<u8> {
    let name_bytes = resp.channel_name.as_bytes();
    let name_len = name_bytes.len().min(MAX_CHANNEL_NAME_LEN) as u8;
    let total = 12 + name_len as usize;
    let mut buf = Vec::with_capacity(total);

    buf.extend_from_slice(&DISPATCH_MAGIC.to_le_bytes());
    buf.push(DISPATCH_VERSION);
    buf.push(MSG_TYPE_RESPONSE);
    buf.push(resp.status);
    buf.extend_from_slice(&resp.client_id.to_le_bytes());
    buf.push(name_len);
    buf.extend_from_slice(&name_bytes[..name_len as usize]);

    buf
}

/// Decode a registration response from bytes.
pub fn decode_response(data: &[u8]) -> Result<RegistrationResponse> {
    if data.len() < 12 {
        return Err(ShmError::MessageTooSmall);
    }

    let magic = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
    if magic != DISPATCH_MAGIC {
        return Err(ShmError::Corrupted);
    }

    let version = data[4];
    if version != DISPATCH_VERSION {
        return Err(ShmError::HandshakeFailed);
    }

    let msg_type = data[5];
    if msg_type != MSG_TYPE_RESPONSE {
        return Err(ShmError::HandshakeFailed);
    }

    let status = data[6];
    let client_id = u32::from_le_bytes([data[7], data[8], data[9], data[10]]);
    let name_len = data[11] as usize;

    if data.len() < 12 + name_len {
        return Err(ShmError::MessageTooSmall);
    }

    let channel_name = String::from_utf8_lossy(&data[12..12 + name_len]).into_owned();

    Ok(RegistrationResponse {
        status,
        client_id,
        channel_name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_roundtrip() {
        let req = RegistrationRequest {
            pid: 12345,
            bits: 32,
            revision: 7,
            name: "l2.exe".to_string(),
        };
        let encoded = encode_request(&req);
        let decoded = decode_request(&encoded).unwrap();
        assert_eq!(decoded.pid, 12345);
        assert_eq!(decoded.bits, 32);
        assert_eq!(decoded.revision, 7);
        assert_eq!(decoded.name, "l2.exe");
    }

    #[test]
    fn response_roundtrip() {
        let resp = RegistrationResponse {
            status: STATUS_OK,
            client_id: 42,
            channel_name: "NxT_a7f3b2c1".to_string(),
        };
        let encoded = encode_response(&resp);
        let decoded = decode_response(&encoded).unwrap();
        assert_eq!(decoded.status, STATUS_OK);
        assert_eq!(decoded.client_id, 42);
        assert_eq!(decoded.channel_name, "NxT_a7f3b2c1");
    }

    #[test]
    fn request_too_short() {
        assert!(decode_request(&[0; 10]).is_err());
    }

    #[test]
    fn response_bad_magic() {
        let mut data = encode_response(&RegistrationResponse {
            status: 0,
            client_id: 1,
            channel_name: "x".into(),
        });
        data[0] = 0xFF;
        assert!(decode_response(&data).is_err());
    }
}
