//! Бинарный протокол для handshake в dispatch-лобби.
//!
//! Минимальное бинарное кодирование — без зависимости от serde.
//! Сообщения передаются через ring buffer'ы lobby SharedServer.

use crate::error::{Result, ShmError};

// ─── Константы ────────────────────────────────────────────────────────────────

const DISPATCH_MAGIC: u32 = 0x4449_5350; // 'DISP'
const DISPATCH_VERSION: u8 = 2; // v2: убрано поле bits

const MSG_TYPE_REQUEST: u8 = 1;
const MSG_TYPE_RESPONSE: u8 = 2;

const MAX_NAME_LEN: usize = 64;
pub(crate) const MAX_CHANNEL_NAME_LEN: usize = 64;

/// Статус ответа: успех.
pub const STATUS_OK: u8 = 0;
/// Статус ответа: сервер отклонил подключение.
pub const STATUS_REJECTED: u8 = 1;

// ─── Запрос регистрации (клиент → сервер) ───────────────────────────────────

/// Данные, отправляемые клиентом во время регистрации в лобби.
///
/// Поля: имя процесса, PID, ревизия.
#[derive(Debug, Clone)]
pub struct RegistrationRequest {
    pub pid: u32,
    pub revision: u16,
    pub name: String,
}

/// Обрезает строку до `max` БАЙТ, не разрывая UTF-8 последовательность.
///
/// Наивное `&bytes[..max]` рубило по байту и могло оставить хвост
/// многобайтового символа -- декодер (`from_utf8_lossy`) заменял его на U+FFFD,
/// т.е. усечение молча портило последний символ имени (аудит 2026-07-28).
fn truncate_utf8(value: &str, max: usize) -> &[u8] {
    if value.len() <= max {
        return value.as_bytes();
    }
    let mut end = max;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value.as_bytes()[..end]
}

/// Кодирует запрос регистрации в байты.
///
/// Layout (v2):
/// ```text
/// [0..4]   magic: u32 LE = 0x44495350
/// [4..5]   version: u8 = 2
/// [5..6]   msg_type: u8 = 1
/// [6..10]  pid: u32 LE
/// [10..12] revision: u16 LE
/// [12..13] name_len: u8
/// [13..]   name: UTF-8 байты (максимум 64)
/// ```
#[must_use]
pub fn encode_request(req: &RegistrationRequest) -> Vec<u8> {
    let name_bytes = truncate_utf8(&req.name, MAX_NAME_LEN);
    let name_len = name_bytes.len() as u8;
    let total = 13 + name_len as usize;
    let mut buf = Vec::with_capacity(total);

    buf.extend_from_slice(&DISPATCH_MAGIC.to_le_bytes());
    buf.push(DISPATCH_VERSION);
    buf.push(MSG_TYPE_REQUEST);
    buf.extend_from_slice(&req.pid.to_le_bytes());
    buf.extend_from_slice(&req.revision.to_le_bytes());
    buf.push(name_len);
    buf.extend_from_slice(name_bytes);

    buf
}

/// Декодирует запрос регистрации из байтов.
///
/// Данные приходят от пира: длины проверяются типом (`split_first_chunk`,
/// `get`), а не индексами -- короткий или испорченный буфер даёт ошибку,
/// а не панику.
pub fn decode_request(data: &[u8]) -> Result<RegistrationRequest> {
    let Some((header, rest)) = data.split_first_chunk::<13>() else {
        return Err(ShmError::MessageTooSmall);
    };
    let [
        m0,
        m1,
        m2,
        m3,
        version,
        msg_type,
        p0,
        p1,
        p2,
        p3,
        r0,
        r1,
        name_len,
    ] = *header;

    if u32::from_le_bytes([m0, m1, m2, m3]) != DISPATCH_MAGIC {
        return Err(ShmError::Corrupted);
    }
    if version != DISPATCH_VERSION {
        return Err(ShmError::HandshakeFailed);
    }
    if msg_type != MSG_TYPE_REQUEST {
        return Err(ShmError::HandshakeFailed);
    }

    let pid = u32::from_le_bytes([p0, p1, p2, p3]);
    let revision = u16::from_le_bytes([r0, r1]);
    let name = rest
        .get(..usize::from(name_len))
        .ok_or(ShmError::MessageTooSmall)?;
    let name = String::from_utf8_lossy(name).into_owned();

    Ok(RegistrationRequest {
        pid,
        revision,
        name,
    })
}

// ─── Ответ регистрации (сервер → клиент) ────────────────────────────────────

/// Данные, отправляемые сервером после обработки регистрации.
#[derive(Debug, Clone)]
pub struct RegistrationResponse {
    pub status: u8,
    pub client_id: u32,
    pub channel_name: String,
}

/// Кодирует ответ регистрации в байты.
///
/// Layout (v2):
/// ```text
/// [0..4]   magic: u32 LE = 0x44495350
/// [4..5]   version: u8 = 2
/// [5..6]   msg_type: u8 = 2
/// [6..7]   status: u8
/// [7..11]  client_id: u32 LE
/// [11..12] channel_name_len: u8
/// [12..]   channel_name: UTF-8 байты (максимум 64)
/// ```
#[must_use]
pub fn encode_response(resp: &RegistrationResponse) -> Vec<u8> {
    let name_bytes = truncate_utf8(&resp.channel_name, MAX_CHANNEL_NAME_LEN);
    let name_len = name_bytes.len() as u8;
    let total = 12 + name_len as usize;
    let mut buf = Vec::with_capacity(total);

    buf.extend_from_slice(&DISPATCH_MAGIC.to_le_bytes());
    buf.push(DISPATCH_VERSION);
    buf.push(MSG_TYPE_RESPONSE);
    buf.push(resp.status);
    buf.extend_from_slice(&resp.client_id.to_le_bytes());
    buf.push(name_len);
    buf.extend_from_slice(name_bytes);

    buf
}

/// Декодирует ответ регистрации из байтов.
///
/// Разбор без индексов -- как в [`decode_request`].
pub fn decode_response(data: &[u8]) -> Result<RegistrationResponse> {
    let Some((header, rest)) = data.split_first_chunk::<12>() else {
        return Err(ShmError::MessageTooSmall);
    };
    let [
        m0,
        m1,
        m2,
        m3,
        version,
        msg_type,
        status,
        c0,
        c1,
        c2,
        c3,
        name_len,
    ] = *header;

    if u32::from_le_bytes([m0, m1, m2, m3]) != DISPATCH_MAGIC {
        return Err(ShmError::Corrupted);
    }
    if version != DISPATCH_VERSION {
        return Err(ShmError::HandshakeFailed);
    }
    if msg_type != MSG_TYPE_RESPONSE {
        return Err(ShmError::HandshakeFailed);
    }

    let client_id = u32::from_le_bytes([c0, c1, c2, c3]);
    let channel_name = rest
        .get(..usize::from(name_len))
        .ok_or(ShmError::MessageTooSmall)?;
    let channel_name = String::from_utf8_lossy(channel_name).into_owned();

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
            revision: 7,
            name: "l2.exe".to_string(),
        };
        let encoded = encode_request(&req);
        let decoded = decode_request(&encoded).unwrap();
        assert_eq!(decoded.pid, 12345);
        assert_eq!(decoded.revision, 7);
        assert_eq!(decoded.name, "l2.exe");
    }

    #[test]
    fn request_empty_name() {
        let req = RegistrationRequest {
            pid: 1,
            revision: 0,
            name: String::new(),
        };
        let encoded = encode_request(&req);
        let decoded = decode_request(&encoded).unwrap();
        assert_eq!(decoded.name, "");
        assert_eq!(decoded.pid, 1);
    }

    #[test]
    fn request_max_name() {
        let long_name = "A".repeat(100); // длиннее MAX_NAME_LEN
        let req = RegistrationRequest {
            pid: 42,
            revision: 1,
            name: long_name,
        };
        let encoded = encode_request(&req);
        let decoded = decode_request(&encoded).unwrap();
        assert_eq!(decoded.name.len(), MAX_NAME_LEN);
    }

    /// Регрессия (аудит 2026-07-28): усечение до MAX_NAME_LEN байт не должно
    /// разрывать многобайтовый символ -- иначе имя приезжает с U+FFFD.
    #[test]
    fn request_name_truncated_on_char_boundary() {
        // 'Ж' -- 2 байта: 33 символа = 66 байт > MAX_NAME_LEN (64).
        let req = RegistrationRequest {
            pid: 1,
            revision: 0,
            name: "Ж".repeat(33),
        };
        let decoded = decode_request(&encode_request(&req)).unwrap();
        assert!(decoded.name.len() <= MAX_NAME_LEN);
        assert!(
            !decoded.name.contains('\u{FFFD}'),
            "усечение порвало UTF-8: {}",
            decoded.name
        );
        assert_eq!(decoded.name, "Ж".repeat(32));
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

    #[test]
    fn response_rejected() {
        let resp = RegistrationResponse {
            status: STATUS_REJECTED,
            client_id: 0,
            channel_name: String::new(),
        };
        let encoded = encode_response(&resp);
        let decoded = decode_response(&encoded).unwrap();
        assert_eq!(decoded.status, STATUS_REJECTED);
        assert_eq!(decoded.client_id, 0);
        assert_eq!(decoded.channel_name, "");
    }
}
