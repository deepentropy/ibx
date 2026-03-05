//! NS protocol: `#%#%` magic + 4-byte BE length + semicolon-delimited ASCII payload.
//!
//! Used for initial connection handshake before upgrading to FIX messaging.

use std::io::{self, Read};

/// NS framing magic bytes.
pub const NS_MAGIC: &[u8; 4] = b"#%#%";

// NsMessageType enum values
pub const NS_ERROR_RESPONSE: u32 = 519;
pub const NS_AUTH_START: u32 = 520;
pub const NS_CONNECT_REQUEST: u32 = 521;
pub const NS_CONNECT_RESPONSE: u32 = 523;
pub const NS_REDIRECT: u32 = 524;
pub const NS_FIX_START: u32 = 525;
pub const NS_NEWCOMMPORTTYPE: u32 = 526;
pub const NS_BACKUP_HOST: u32 = 527;
pub const NS_MISC_URLS_REQUEST: u32 = 528;
pub const NS_MISC_URLS_RESPONSE: u32 = 529;
pub const NS_SECURE_CONNECT: u32 = 532;
pub const NS_SECURE_CONNECTION_START: u32 = 533;
pub const NS_SECURE_MESSAGE: u32 = 534;
pub const NS_SECURE_ERROR: u32 = 535;

/// Build an NS message with `#%#%` framing.
///
/// Format: `#%#%` + 4-byte-BE-length + payload
/// Payload: `[prefix]{version};{msg_type};{field1};{field2};...;`
pub fn ns_build(version: u32, msg_type: u32, fields: &[&str], prefix: &str) -> Vec<u8> {
    let mut payload = String::new();
    payload.push_str(prefix);
    payload.push_str(&format!("{};{};", version, msg_type));
    for f in fields {
        payload.push_str(f);
        payload.push(';');
    }
    let payload_bytes = payload.as_bytes();
    let mut msg = Vec::with_capacity(8 + payload_bytes.len());
    msg.extend_from_slice(NS_MAGIC);
    msg.extend_from_slice(&(payload_bytes.len() as u32).to_be_bytes());
    msg.extend_from_slice(payload_bytes);
    msg
}

/// Parse NS payload into (version, msg_type, remaining_fields).
pub fn ns_parse(payload: &[u8]) -> Option<(u32, u32, Vec<String>)> {
    let text = std::str::from_utf8(payload).ok()?;
    // Strip MISC prefix if present
    let text = if text.to_uppercase().starts_with("MISC") {
        &text[4..]
    } else {
        text
    };
    let parts: Vec<&str> = text.split(';').collect();
    if parts.len() < 2 {
        return None;
    }
    let version: u32 = parts[0].parse().ok()?;
    let msg_type: u32 = parts[1].parse().ok()?;
    let fields: Vec<String> = parts[2..].iter().filter(|p| !p.is_empty()).map(|s| s.to_string()).collect();
    Some((version, msg_type, fields))
}

/// Receive one `#%#%` framed message. Returns (payload_bytes, total_len).
pub fn ns_recv<R: Read>(reader: &mut R) -> io::Result<(Vec<u8>, usize)> {
    let mut header = [0u8; 8];
    reader.read_exact(&mut header)?;
    if &header[..4] != NS_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Expected #%#% magic, got {:?}", &header[..4]),
        ));
    }
    let payload_len = u32::from_be_bytes([header[4], header[5], header[6], header[7]]) as usize;
    let mut payload = vec![0u8; payload_len];
    reader.read_exact(&mut payload)?;
    Ok((payload, payload_len + 8))
}

/// Classify a `#%#%` framed payload as NS text or XYZ binary.
///
/// Returns `true` if the payload looks like NS text (starts with ASCII digit).
pub fn is_ns_text(payload: &[u8]) -> bool {
    payload.first().map_or(false, |b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_structure() {
        let msg = ns_build(50, 521, &["user", "1234"], "");
        assert_eq!(&msg[..4], NS_MAGIC);
        let len = u32::from_be_bytes([msg[4], msg[5], msg[6], msg[7]]) as usize;
        assert_eq!(msg.len(), 8 + len);
        let payload = std::str::from_utf8(&msg[8..]).unwrap();
        assert!(payload.starts_with("50;521;"));
    }

    #[test]
    fn build_with_prefix() {
        let msg = ns_build(38, 528, &[], "MISC");
        let payload = std::str::from_utf8(&msg[8..]).unwrap();
        assert!(payload.starts_with("MISC38;528;"));
    }

    #[test]
    fn parse_roundtrip() {
        let msg = ns_build(50, 521, &["user", "1234", "info"], "");
        let payload = &msg[8..];
        let (version, msg_type, fields) = ns_parse(payload).unwrap();
        assert_eq!(version, 50);
        assert_eq!(msg_type, 521);
        assert_eq!(fields, vec!["user", "1234", "info"]);
    }

    #[test]
    fn parse_misc_prefix() {
        let msg = ns_build(38, 529, &["key=val"], "MISC");
        let payload = &msg[8..];
        let (version, msg_type, fields) = ns_parse(payload).unwrap();
        assert_eq!(version, 38);
        assert_eq!(msg_type, 529);
        assert_eq!(fields, vec!["key=val"]);
    }

    #[test]
    fn recv_roundtrip() {
        let msg = ns_build(50, 534, &["data"], "");
        let mut cursor = std::io::Cursor::new(&msg);
        let (payload, total) = ns_recv(&mut cursor).unwrap();
        assert_eq!(total, msg.len());
        let (version, msg_type, _) = ns_parse(&payload).unwrap();
        assert_eq!(version, 50);
        assert_eq!(msg_type, 534);
    }

    #[test]
    fn is_ns_text_checks() {
        assert!(is_ns_text(b"50;521;user;"));
        assert!(!is_ns_text(b"\x00\x00\x00\x17")); // XYZ binary
        assert!(!is_ns_text(b""));
    }
}
