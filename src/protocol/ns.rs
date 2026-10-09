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
/// Server keepalive probe sent during second-factor approval wait.
/// Payload: `MISC{ns_version};530;{server_timestamp};` — the client must echo
/// `server_timestamp` verbatim in an `NS_HEART_BEAT` reply.
pub const NS_TEST_REQUEST: u32 = 530;
/// Client keepalive reply matching a prior `NS_TEST_REQUEST`.
/// Payload: `MISC{ns_version};531;{server_timestamp};`.
pub const NS_HEART_BEAT: u32 = 531;

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
    // Strip the text prefix from the original text, case-sensitive, as the
    // reference does. Slicing at the length of an upper-cased copy panicked
    // when upper-casing changed the byte length (ibx#365).
    let text = text.strip_prefix("MISC").unwrap_or(text);
    let parts: Vec<&str> = text.split(';').collect();
    if parts.len() < 2 {
        return None;
    }
    let version: u32 = parts[0].parse().ok()?;
    let msg_type: u32 = parts[1].parse().ok()?;
    let fields: Vec<String> = parts[2..].iter().filter(|p| !p.is_empty()).map(|s| s.to_string()).collect();
    Some((version, msg_type, fields))
}

/// Build an `NS_HEART_BEAT` reply that echoes the timestamp from a paired
/// `NS_TEST_REQUEST`. Uses the `MISC` prefix variant the gateway client uses.
pub fn ns_build_heart_beat(ns_version: u32, test_req_timestamp: &str) -> Vec<u8> {
    ns_build(ns_version, NS_HEART_BEAT, &[test_req_timestamp], "MISC")
}

/// Extract the timestamp from an inbound `NS_TEST_REQUEST` payload. Returns
/// `None` if the payload doesn't parse or carries the wrong message type.
pub fn parse_test_request_timestamp(payload: &[u8]) -> Option<String> {
    let (_, msg_type, fields) = ns_parse(payload)?;
    if msg_type != NS_TEST_REQUEST { return None; }
    fields.into_iter().find(|f| !f.is_empty())
}

/// Buffer reserved at once for an NS payload; a longer one grows as it comes.
const NS_READ_CHUNK: usize = 64 * 1024;

/// Receive one `#%#%` framed message. Returns (payload_bytes, total_len).
pub fn ns_recv<R: Read>(reader: &mut R) -> io::Result<(Vec<u8>, usize)> {
    ns_recv_impl(reader, i32::MAX as usize, true)
}

/// Receive one NS frame, refusing its announced payload size before allocation.
/// The caller separately owns socket cancellation/deadline enforcement.
pub fn ns_recv_limited<R: Read>(reader: &mut R, max_payload_bytes: usize) -> io::Result<(Vec<u8>, usize)> {
    ns_recv_impl(reader, max_payload_bytes, false)
}

fn ns_recv_impl<R: Read>(reader: &mut R, max_payload_bytes: usize, peer_diagnostics: bool) -> io::Result<(Vec<u8>, usize)> {
    let mut header = [0u8; 8];
    reader.read_exact(&mut header)?;
    if &header[..4] != NS_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            if peer_diagnostics { format!("Expected #%#% magic, got {:?}", &header[..4]) } else { "invalid bounded NS frame magic".to_string() },
        ));
    }
    let raw_len = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
    // The reference reads the length as a signed int: with the high bit set
    // it is negative and the frame fails, so the connection is dropped. Here
    // it allocated up to 4 GB and waited for that many bytes (ibx#423).
    if raw_len & 0x8000_0000 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            if peer_diagnostics { format!("NS frame length {:#010x} is negative", raw_len) } else { "invalid bounded NS frame length".to_string() },
        ));
    }
    // The payload grows as its bytes come: a length the peer does not send
    // costs nothing. A buffer of the stated length was allocated at once,
    // up to 2 GB from one 8-byte header (ibx#488).
    let payload_len = raw_len as usize;
    if payload_len > max_payload_bytes {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "NS frame exceeds payload byte limit"));
    }
    let mut payload = Vec::with_capacity(payload_len.min(NS_READ_CHUNK));
    reader.by_ref().take(raw_len as u64).read_to_end(&mut payload)?;
    if payload.len() < payload_len {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "NS frame cut short"));
    }
    Ok((payload, payload_len + 8))
}

/// Classify a `#%#%` framed payload as NS text or XYZ binary.
///
/// Returns `true` if the payload looks like NS text — either an ASCII digit
/// (no prefix) or the `MISC` prefix the gateway uses for some message types
/// (e.g. `NS_MISC_URLS_RESPONSE`, `NS_TEST_REQUEST`, `NS_HEART_BEAT`).
pub fn is_ns_text(payload: &[u8]) -> bool {
    match payload.first() {
        Some(b) if b.is_ascii_digit() => true,
        _ => payload.starts_with(b"MISC"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn bounded_receive_rejects_header_before_body_read() {
        let mut header = NS_MAGIC.to_vec();
        header.extend_from_slice(&0x7fff_ffff_u32.to_be_bytes());
        let mut input = std::io::Cursor::new(header);
        let error = ns_recv_limited(&mut input, 1024).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(input.position(), 8);
    }

    #[test]
    fn bounded_receive_diagnostics_do_not_echo_header_bytes() {
        let frame = b"pass\x00\x00\x00\x00";
        let error = ns_recv_limited(&mut std::io::Cursor::new(frame), 1024).unwrap_err();
        assert_eq!(error.to_string(), "invalid bounded NS frame magic");
        assert!(ns_recv(&mut std::io::Cursor::new(frame)).unwrap_err().to_string().contains("[112, 97, 115, 115]"));
    }

    #[test]
    fn bounded_receive_accepts_exact_limit_and_empty_payload() {
        let frame = ns_build(50, 520, &["x"], "");
        let limit = frame.len() - 8;
        assert_eq!(ns_recv_limited(&mut std::io::Cursor::new(&frame), limit).unwrap(), ns_recv(&mut std::io::Cursor::new(&frame)).unwrap());
        assert!(ns_recv_limited(&mut std::io::Cursor::new(&frame), limit - 1).is_err());
        let mut empty = NS_MAGIC.to_vec();
        empty.extend_from_slice(&0u32.to_be_bytes());
        assert!(ns_recv_limited(&mut std::io::Cursor::new(empty), 0).unwrap().0.is_empty());
    }

    // ── Existing tests ──────────────────────────────────────────────

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
    fn heart_beat_echoes_timestamp() {
        let msg = ns_build_heart_beat(50, "20260430-22:58:25");
        let payload = std::str::from_utf8(&msg[8..]).unwrap();
        // Must use MISC prefix, type 531, and carry the original timestamp.
        assert!(payload.starts_with("MISC50;531;20260430-22:58:25;"), "got {payload}");
    }

    #[test]
    fn parse_test_request_extracts_timestamp() {
        let msg = ns_build(50, NS_TEST_REQUEST, &["20260430-22:58:25"], "MISC");
        let payload = &msg[8..];
        assert_eq!(
            parse_test_request_timestamp(payload),
            Some("20260430-22:58:25".to_string())
        );
    }

    #[test]
    fn parse_test_request_rejects_wrong_type() {
        let msg = ns_build(50, NS_HEART_BEAT, &["20260430-22:58:25"], "MISC");
        let payload = &msg[8..];
        assert_eq!(parse_test_request_timestamp(payload), None);
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

    // ── New tests ───────────────────────────────────────────────────

    #[test]
    fn build_empty_fields() {
        let msg = ns_build(1, 2, &[], "");
        let payload = std::str::from_utf8(&msg[8..]).unwrap();
        assert_eq!(payload, "1;2;");
    }

    #[test]
    fn build_many_fields() {
        let fields: Vec<&str> = (0..20).map(|_| "x").collect();
        let msg = ns_build(10, 99, &fields, "");
        let payload = std::str::from_utf8(&msg[8..]).unwrap();
        // version;msg_type; plus 20 "x;" entries
        assert!(payload.starts_with("10;99;"));
        assert_eq!(payload.matches("x;").count(), 20);
    }

    #[test]
    fn parse_empty_payload_returns_none() {
        assert!(ns_parse(b"").is_none());
    }

    #[test]
    fn parse_single_field_no_trailing_semicolon_returns_none() {
        // "50" with no semicolons → split yields ["50"], len < 2 → None
        assert!(ns_parse(b"50").is_none());
    }

    #[test]
    fn parse_non_numeric_version_returns_none() {
        assert!(ns_parse(b"abc;521;field;").is_none());
    }

    #[test]
    fn parse_misc_prefix_lowercase_is_kept() {
        // The prefix is stripped case-sensitively, as the reference does: a
        // lower-case "misc" stays in the version field, which is then not a
        // number (ibx#365).
        assert!(ns_parse(b"misc38;529;val;").is_none());
    }

    #[test]
    fn parse_prefix_that_changes_length_when_upper_cased_does_not_panic() {
        // Upper-cased, these characters give "MISC" with a different byte
        // length; slicing the original at byte 4 panicked (ibx#365).
        let payload = "mıſc;1;2".as_bytes();
        assert!(ns_parse(payload).is_none());
        let payload = "MISCı;1;2".as_bytes();
        assert!(ns_parse(payload).is_none());
    }

    #[test]
    fn recv_bad_magic_returns_error() {
        let bad = b"AAAA\x00\x00\x00\x02ok";
        let mut cursor = std::io::Cursor::new(&bad[..]);
        let result = ns_recv(&mut cursor);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("#%#%"));
    }

    #[test]
    fn recv_negative_length_fails_at_once() {
        // High bit set: refused before any allocation or read (ibx#423).
        let mut msg = Vec::new();
        msg.extend_from_slice(NS_MAGIC);
        msg.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xF0]);
        let mut cursor = std::io::Cursor::new(&msg);
        let err = ns_recv(&mut cursor).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("negative"), "{err}");
    }

    #[test]
    fn recv_zero_length_payload() {
        let mut msg = Vec::new();
        msg.extend_from_slice(NS_MAGIC);
        msg.extend_from_slice(&0u32.to_be_bytes());
        let mut cursor = std::io::Cursor::new(&msg);
        let (payload, total) = ns_recv(&mut cursor).unwrap();
        assert!(payload.is_empty());
        assert_eq!(total, 8);
    }

    #[test]
    fn is_ns_text_digit() {
        assert!(is_ns_text(b"0rest"));
        assert!(is_ns_text(b"9rest"));
    }

    #[test]
    fn is_ns_text_letter() {
        assert!(!is_ns_text(b"Atext"));
        assert!(!is_ns_text(b"z"));
    }

    #[test]
    fn is_ns_text_null_byte() {
        assert!(!is_ns_text(b"\x00"));
    }

    #[test]
    fn is_ns_text_space() {
        assert!(!is_ns_text(b" "));
    }

    #[test]
    fn ns_magic_value() {
        assert_eq!(NS_MAGIC, b"#%#%");
        assert_eq!(NS_MAGIC.len(), 4);
    }

    #[test]
    fn all_ns_constants_unique() {
        let values: Vec<u32> = vec![
            NS_ERROR_RESPONSE,
            NS_AUTH_START,
            NS_CONNECT_REQUEST,
            NS_CONNECT_RESPONSE,
            NS_REDIRECT,
            NS_FIX_START,
            NS_NEWCOMMPORTTYPE,
            NS_BACKUP_HOST,
            NS_MISC_URLS_REQUEST,
            NS_MISC_URLS_RESPONSE,
            NS_SECURE_CONNECT,
            NS_SECURE_CONNECTION_START,
            NS_SECURE_MESSAGE,
            NS_SECURE_ERROR,
        ];
        let set: HashSet<u32> = values.iter().copied().collect();
        assert_eq!(
            values.len(),
            set.len(),
            "Duplicate values found among NS_* constants"
        );
    }
}
