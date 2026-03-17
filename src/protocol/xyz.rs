//! XYZ binary protocol: length-prefixed string fields over `#%#%` framing.
//!
//! Format: `[4B version][4B msg_id][4B sub_id=1][4B state][len-prefixed strings...]`
//! Strings are ASCII, 4-byte length prefix, padded to 4-byte alignment.

use super::ns::NS_MAGIC;

/// XYZ protocol version (from MITM capture, real Gateway uses 0x17 = 23).
pub const XYZ_PROTOCOL_VERSION: u32 = 23;

/// XYZ message types.
pub const XYZ_MSG_SRP: u32 = 777;
pub const XYZ_MSG_TOKEN_AUTH: u32 = 771;
pub const XYZ_MSG_SOFT_TOKEN: u32 = 772;

/// Build an XYZ binary message.
pub fn xyz_build(msg_id: u32, state: u32, username: &str, fields: &[&str]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&XYZ_PROTOCOL_VERSION.to_be_bytes());
    buf.extend_from_slice(&msg_id.to_be_bytes());
    buf.extend_from_slice(&1u32.to_be_bytes()); // sub_id constant
    buf.extend_from_slice(&state.to_be_bytes());
    xyz_write_string(&mut buf, username);
    for f in fields {
        xyz_write_string(&mut buf, f);
    }
    buf
}

/// Wrap XYZ binary payload in `#%#%` envelope.
pub fn xyz_wrap(payload: &[u8]) -> Vec<u8> {
    let mut msg = Vec::with_capacity(8 + payload.len());
    msg.extend_from_slice(NS_MAGIC);
    msg.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    msg.extend_from_slice(payload);
    msg
}

/// Parse XYZ binary response → (msg_id, sub_id, state, string_fields).
pub fn xyz_parse_response(payload: &[u8]) -> Option<(u32, u32, u32, Vec<String>)> {
    if payload.len() < 16 {
        return None;
    }
    let msg_id = u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
    let sub_id = u32::from_be_bytes([payload[8], payload[9], payload[10], payload[11]]);
    let state = u32::from_be_bytes([payload[12], payload[13], payload[14], payload[15]]);

    let mut fields = Vec::new();
    let mut offset = 16;
    while offset + 4 <= payload.len() {
        let slen =
            u32::from_be_bytes([payload[offset], payload[offset + 1], payload[offset + 2], payload[offset + 3]])
                as usize;
        offset += 4;
        if slen > 0 && offset + slen <= payload.len() {
            fields.push(
                String::from_utf8_lossy(&payload[offset..offset + slen]).to_string(),
            );
        } else {
            fields.push(String::new());
        }
        offset += slen;
        // Pad to 4-byte alignment
        let remainder = slen % 4;
        if remainder > 0 {
            offset += 4 - remainder;
        }
    }
    Some((msg_id, sub_id, state, fields))
}

/// Build SRP XYZ message using v20 format with named fields H-P.
pub fn xyz_build_srp_v20(state: u32, named_fields: &[(&str, &str)]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&20u32.to_be_bytes()); // v20 format
    buf.extend_from_slice(&XYZ_MSG_SRP.to_be_bytes());
    buf.extend_from_slice(&1u32.to_be_bytes());
    buf.extend_from_slice(&state.to_be_bytes());
    xyz_write_string(&mut buf, ""); // empty username field

    // Named fields H through P (9 fields)
    let field_names = ['H', 'I', 'J', 'K', 'L', 'M', 'N', 'O', 'P'];
    for name in &field_names {
        let val = named_fields
            .iter()
            .find(|(k, _)| k.len() == 1 && k.chars().next() == Some(*name))
            .map(|(_, v)| *v)
            .unwrap_or("");
        xyz_write_string(&mut buf, val);
    }
    buf
}

/// Build token authentication message.
pub fn xyz_build_soft_token(state: u32, x: &str, y: &str, z: &str) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&XYZ_PROTOCOL_VERSION.to_be_bytes());
    buf.extend_from_slice(&XYZ_MSG_SOFT_TOKEN.to_be_bytes());
    buf.extend_from_slice(&1u32.to_be_bytes());
    buf.extend_from_slice(&state.to_be_bytes());
    xyz_write_string(&mut buf, ""); // empty username
    xyz_write_string(&mut buf, x);
    xyz_write_string(&mut buf, y);
    xyz_write_string(&mut buf, z);
    buf
}

/// Write a length-prefixed, 4-byte-aligned string.
pub fn xyz_write_string(buf: &mut Vec<u8>, s: &str) {
    let data = s.as_bytes();
    buf.extend_from_slice(&(data.len() as u32).to_be_bytes());
    buf.extend_from_slice(data);
    let remainder = data.len() % 4;
    if remainder > 0 {
        buf.extend(std::iter::repeat(0u8).take(4 - remainder));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Existing tests ──────────────────────────────────────────────

    #[test]
    fn build_parse_roundtrip() {
        let payload = xyz_build(777, 1, "user", &["field1", "field2"]);
        let (msg_id, sub_id, state, fields) = xyz_parse_response(&payload).unwrap();
        assert_eq!(msg_id, 777);
        assert_eq!(sub_id, 1);
        assert_eq!(state, 1);
        assert_eq!(fields[0], "user");
        assert_eq!(fields[1], "field1");
        assert_eq!(fields[2], "field2");
    }

    #[test]
    fn wrap_envelope() {
        let payload = xyz_build(772, 1, "", &[]);
        let wrapped = xyz_wrap(&payload);
        assert_eq!(&wrapped[..4], NS_MAGIC);
        let len = u32::from_be_bytes([wrapped[4], wrapped[5], wrapped[6], wrapped[7]]) as usize;
        assert_eq!(len, payload.len());
        assert_eq!(&wrapped[8..], &payload[..]);
    }

    #[test]
    fn string_alignment() {
        let mut buf = Vec::new();
        xyz_write_string(&mut buf, "abc"); // 3 bytes → padded to 4
        assert_eq!(buf.len(), 4 + 4); // 4-byte length + 4 bytes (3 data + 1 padding)
        xyz_write_string(&mut buf, "abcd"); // 4 bytes → no padding
        assert_eq!(buf.len(), 8 + 4 + 4); // previous + 4-byte length + 4 data
    }

    #[test]
    fn empty_string() {
        let mut buf = Vec::new();
        xyz_write_string(&mut buf, "");
        assert_eq!(buf.len(), 4); // just the length prefix (0)
        assert_eq!(u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]), 0);
    }

    #[test]
    fn srp_v20_fields() {
        let payload = xyz_build_srp_v20(3, &[("L", "deadbeef")]);
        let (msg_id, _, state, fields) = xyz_parse_response(&payload).unwrap();
        assert_eq!(msg_id, XYZ_MSG_SRP);
        assert_eq!(state, 3);
        // Field L is at index 4 in named fields (H=0, I=1, J=2, K=3, L=4)
        // Plus username at index 0, so L = fields[5]
        assert_eq!(fields[5], "deadbeef");
    }

    #[test]
    fn soft_token_fields() {
        let payload = xyz_build_soft_token(3, "", "response_hex", "");
        let (msg_id, _, state, fields) = xyz_parse_response(&payload).unwrap();
        assert_eq!(msg_id, XYZ_MSG_SOFT_TOKEN);
        assert_eq!(state, 3);
        assert_eq!(fields[2], "response_hex"); // y field
    }

    // ── New tests ───────────────────────────────────────────────────

    #[test]
    fn build_empty_username_no_fields() {
        let payload = xyz_build(777, 5, "", &[]);
        let (msg_id, sub_id, state, fields) = xyz_parse_response(&payload).unwrap();
        assert_eq!(msg_id, 777);
        assert_eq!(sub_id, 1);
        assert_eq!(state, 5);
        // Only the empty username string
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0], "");
    }

    #[test]
    fn build_long_field_padding() {
        // 7-byte string → 7 % 4 = 3 remainder → 1 byte padding
        let mut buf = Vec::new();
        xyz_write_string(&mut buf, "1234567");
        // 4 (len prefix) + 7 (data) + 1 (padding) = 12
        assert_eq!(buf.len(), 12);
        assert_eq!(buf[11], 0); // padding byte is zero
    }

    #[test]
    fn parse_response_exactly_16_bytes() {
        // 16 bytes = header only, no string fields
        let mut data = Vec::new();
        data.extend_from_slice(&23u32.to_be_bytes()); // version
        data.extend_from_slice(&100u32.to_be_bytes()); // msg_id
        data.extend_from_slice(&1u32.to_be_bytes()); // sub_id
        data.extend_from_slice(&42u32.to_be_bytes()); // state
        let (msg_id, sub_id, state, fields) = xyz_parse_response(&data).unwrap();
        assert_eq!(msg_id, 100);
        assert_eq!(sub_id, 1);
        assert_eq!(state, 42);
        assert!(fields.is_empty());
    }

    #[test]
    fn parse_response_less_than_16_bytes() {
        assert!(xyz_parse_response(b"").is_none());
        assert!(xyz_parse_response(&[0u8; 15]).is_none());
        assert!(xyz_parse_response(&[0u8; 1]).is_none());
    }

    #[test]
    fn parse_response_truncated_field() {
        // Header says string length = 10, but only 5 data bytes follow
        let mut data = Vec::new();
        data.extend_from_slice(&23u32.to_be_bytes()); // version
        data.extend_from_slice(&50u32.to_be_bytes()); // msg_id
        data.extend_from_slice(&1u32.to_be_bytes()); // sub_id
        data.extend_from_slice(&0u32.to_be_bytes()); // state
        data.extend_from_slice(&10u32.to_be_bytes()); // slen = 10
        data.extend_from_slice(b"ABCDE"); // only 5 bytes available
        let (msg_id, _, _, fields) = xyz_parse_response(&data).unwrap();
        assert_eq!(msg_id, 50);
        // slen > 0 but offset + slen > payload.len(), so branch pushes empty string
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0], "");
    }

    #[test]
    fn write_string_alignment_lengths() {
        // len=1 → 1%4=1 → 3 padding → total = 4+4 = 8
        let mut buf = Vec::new();
        xyz_write_string(&mut buf, "a");
        assert_eq!(buf.len(), 8);

        // len=2 → 2%4=2 → 2 padding → total = 4+4 = 8
        buf.clear();
        xyz_write_string(&mut buf, "ab");
        assert_eq!(buf.len(), 8);

        // len=3 → 3%4=3 → 1 padding → total = 4+4 = 8
        buf.clear();
        xyz_write_string(&mut buf, "abc");
        assert_eq!(buf.len(), 8);

        // len=4 → 4%4=0 → 0 padding → total = 4+4 = 8
        buf.clear();
        xyz_write_string(&mut buf, "abcd");
        assert_eq!(buf.len(), 8);

        // len=5 → 5%4=1 → 3 padding → total = 4+8 = 12
        buf.clear();
        xyz_write_string(&mut buf, "abcde");
        assert_eq!(buf.len(), 12);

        // len=8 → 8%4=0 → 0 padding → total = 4+8 = 12
        buf.clear();
        xyz_write_string(&mut buf, "abcdefgh");
        assert_eq!(buf.len(), 12);
    }

    #[test]
    fn srp_v20_multiple_named_fields() {
        let payload = xyz_build_srp_v20(7, &[("H", "alpha"), ("J", "beta"), ("P", "gamma")]);
        let (msg_id, _, state, fields) = xyz_parse_response(&payload).unwrap();
        assert_eq!(msg_id, XYZ_MSG_SRP);
        assert_eq!(state, 7);
        // fields[0] = username (empty), then H..P at indices 1..9
        assert_eq!(fields[1], "alpha"); // H
        assert_eq!(fields[3], "beta"); // J
        assert_eq!(fields[9], "gamma"); // P
    }

    #[test]
    fn srp_v20_unknown_field_name_ignored() {
        let payload = xyz_build_srp_v20(1, &[("Z", "should_be_ignored")]);
        let (_, _, _, fields) = xyz_parse_response(&payload).unwrap();
        // All named fields H-P should be empty since "Z" matches none
        for f in &fields[1..] {
            assert_eq!(f, "");
        }
    }

    #[test]
    fn soft_token_roundtrip_all_populated() {
        let payload = xyz_build_soft_token(9, "xxx", "yyy", "zzz");
        let (msg_id, sub_id, state, fields) = xyz_parse_response(&payload).unwrap();
        assert_eq!(msg_id, XYZ_MSG_SOFT_TOKEN);
        assert_eq!(sub_id, 1);
        assert_eq!(state, 9);
        // fields: [username="", x, y, z]
        assert_eq!(fields[0], ""); // empty username
        assert_eq!(fields[1], "xxx");
        assert_eq!(fields[2], "yyy");
        assert_eq!(fields[3], "zzz");
    }

    #[test]
    fn protocol_version_is_23() {
        assert_eq!(XYZ_PROTOCOL_VERSION, 23);
    }

    #[test]
    fn msg_srp_is_777() {
        assert_eq!(XYZ_MSG_SRP, 777);
    }
}
