//! FIX 4.1 message framing for IB CCP.
//!
//! IB's FIX format (from decompiled a8.java):
//! - Body length: 4-digit zero-padded (e.g. "0199")
//! - MsgSeqNum: 6-digit zero-padded (e.g. "000001")
//! - No SenderCompID (49) or TargetCompID (56) — set at session level
//! - SOH (0x01) delimiter

use std::collections::HashMap;
use std::io::{self, Read};

use hmac::{Hmac, Mac};
use sha1::Sha1;

pub const SOH: u8 = 0x01;

// Standard FIX tags
pub const TAG_BEGIN_STRING: u32 = 8;
pub const TAG_BODY_LENGTH: u32 = 9;
pub const TAG_CHECKSUM: u32 = 10;
pub const TAG_MSG_SEQ_NUM: u32 = 34;
pub const TAG_MSG_TYPE: u32 = 35;
pub const TAG_SENDER_COMP_ID: u32 = 49;
pub const TAG_SENDING_TIME: u32 = 52;
pub const TAG_TARGET_COMP_ID: u32 = 56;
pub const TAG_TEXT: u32 = 58;
pub const TAG_URGENCY: u32 = 61;
pub const TAG_ENCRYPT_METHOD: u32 = 98;
pub const TAG_HEARTBEAT_INT: u32 = 108;
pub const TAG_TEST_REQ_ID: u32 = 112;
pub const TAG_RESET_SEQ_NUM: u32 = 141;
pub const TAG_HEADLINE: u32 = 148;
pub const TAG_SECURITY_EXCHANGE: u32 = 207;

// IB custom tags
pub const TAG_IB_BUILD: u32 = 6034;
pub const TAG_IB_COMM_TYPE: u32 = 6040;
pub const TAG_IB_COMP_VERSION: u32 = 6143;
pub const TAG_IB_VERSION: u32 = 6968;
pub const TAG_HMAC_SIGNATURE: u32 = 8349;

// Message types
pub const MSG_HEARTBEAT: &str = "0";
pub const MSG_TEST_REQUEST: &str = "1";
pub const MSG_REJECT: &str = "3";
pub const MSG_LOGOUT: &str = "5";
pub const MSG_LOGON: &str = "A";
pub const MSG_NEWS: &str = "B";
pub const MSG_IB_CUSTOM: &str = "U";
pub const MSG_EXEC_REPORT: &str = "8";
pub const MSG_CANCEL_REJECT: &str = "9";
pub const MSG_NEW_ORDER: &str = "D";
pub const MSG_ORDER_CANCEL: &str = "F";
pub const MSG_ORDER_REPLACE: &str = "G";
pub const MSG_MARKET_DATA_REQ: &str = "V";

/// Sum of all bytes mod 256, zero-padded to 3 digits.
pub fn fix_checksum(data: &[u8]) -> String {
    let sum: u32 = data.iter().map(|&b| b as u32).sum();
    format!("{:03}", sum % 256)
}

/// Build a complete FIX message matching IB Gateway format.
///
/// `fields` should NOT include tags 8, 9, 34, or 10 (auto-generated).
/// Body length is 4-digit zero-padded, MsgSeqNum is 6-digit zero-padded.
pub fn fix_build(fields: &[(u32, &str)], seq: u32) -> Vec<u8> {
    // Build body: first_field SOH 34=NNNNNN SOH [rest...] SOH
    let mut body = String::new();
    for (i, (tag, val)) in fields.iter().enumerate() {
        if i == 1 {
            // Insert seq after first field (35=MsgType)
            body.push_str(&format!("34={:06}\x01", seq));
        }
        body.push_str(&format!("{}={}\x01", tag, val));
    }
    if fields.len() == 1 {
        // Only one field, append seq after it
        body.push_str(&format!("34={:06}\x01", seq));
    }

    // Header: 8=FIX.4.1 SOH 9=NNNN SOH
    let header = format!("8=FIX.4.1\x019={:04}\x01", body.len());

    // Checksum covers header + body
    let pre_checksum = format!("{}{}", header, body);
    let cksum = fix_checksum(pre_checksum.as_bytes());
    let mut msg = pre_checksum.into_bytes();
    msg.extend_from_slice(format!("10={}\x01", cksum).as_bytes());
    msg
}

/// Parse a SOH-delimited FIX message into {tag: value}.
pub fn fix_parse(data: &[u8]) -> HashMap<u32, String> {
    let mut result = HashMap::new();
    for part in data.split(|&b| b == SOH) {
        if part.is_empty() {
            continue;
        }
        let text = String::from_utf8_lossy(part);
        if let Some((tag_str, val)) = text.split_once('=') {
            if let Ok(tag) = tag_str.parse::<u32>() {
                result.insert(tag, val.to_string());
            }
        }
    }
    result
}

/// Fold 20-byte HMAC digest to 4 bytes → 8-char uppercase hex.
pub fn xor_fold(h: &[u8]) -> String {
    assert!(h.len() >= 20);
    let mut r = [0u8; 4];
    for (i, &b) in h[..20].iter().enumerate() {
        r[i % 4] ^= b;
    }
    r.iter().map(|b| format!("{:02X}", b)).collect()
}

/// Find the byte position right after `tag=value\x01` in the message.
fn find_after_tag(data: &[u8], tag_num: u32) -> Option<usize> {
    let needle = format!("{}=", tag_num);
    let needle_bytes = needle.as_bytes();
    // Search for the needle
    let idx = data
        .windows(needle_bytes.len())
        .position(|w| w == needle_bytes)?;
    // Find SOH after the value
    let soh = data[idx + needle_bytes.len()..]
        .iter()
        .position(|&b| b == SOH)?;
    Some(idx + needle_bytes.len() + soh + 1)
}

/// Sign a FIX/FIXCOMP message with HMAC-SHA1 + XOR distortion (tag 8349).
///
/// Returns (signed_msg, new_iv). The IV chains across messages.
pub fn fix_sign(msg: &[u8], mac_key: &[u8], iv: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let is_fixcomp = msg.starts_with(b"8=FIXCOMP");
    let is_fix41 = msg.starts_with(b"8=FIX.4.1");

    // 1. Extract body (after tag 9 SOH)
    let after9 = find_after_tag(msg, 9).unwrap();
    let body_end = if is_fix41 {
        // FIX.4.1: body ends before tag 10 (include the SOH before "10=")
        msg.windows(4)
            .rposition(|w| w == b"\x0110=")
            .map(|p| p + 1)
            .unwrap()
    } else {
        msg.len()
    };
    let body = &msg[after9..body_end];

    // 2. HMAC-SHA1(iv + body)
    let mut mac = Hmac::<Sha1>::new_from_slice(mac_key).unwrap();
    mac.update(iv);
    mac.update(body);
    let hmac_res = mac.finalize().into_bytes();

    // 3. XOR-fold → tag 8349
    let sig = xor_fold(&hmac_res);

    // 4. Rebuild message with signature
    let mut signed_body = Vec::new();
    signed_body.extend_from_slice(body);
    signed_body.extend_from_slice(format!("8349={}\x01", sig).as_bytes());

    // Header: everything up to and including first SOH (8=... SOH)
    let hdr_end = msg.iter().position(|&b| b == SOH).unwrap() + 1;
    let header = &msg[..hdr_end];

    let mut new_msg = Vec::new();
    new_msg.extend_from_slice(header);
    new_msg.extend_from_slice(format!("9={}\x01", signed_body.len()).as_bytes());
    new_msg.extend_from_slice(&signed_body);

    if is_fix41 {
        // Add tag 10 checksum
        let cksum: u32 = new_msg.iter().map(|&b| b as u32).sum();
        new_msg.extend_from_slice(format!("10={:03}\x01", cksum % 256).as_bytes());
    }

    // 5. XOR distortion (8 positions using IV pairs)
    let start = if is_fixcomp {
        find_after_tag(&new_msg, 9).unwrap()
    } else {
        find_after_tag(&new_msg, 35).unwrap()
    };
    // End offset: skip trailing 8349=XXXXXXXX SOH (14 bytes) + SOH (1 byte)
    let mut end = new_msg.len() as isize - 14 - 2;
    if is_fix41 {
        end -= 7; // also skip 10=XXX SOH
    }
    if end >= start as isize {
        let rng = end as usize - start + 1;
        for i in 0..8 {
            let pos = (iv[i * 2] as usize) % rng;
            new_msg[start + pos] ^= iv[i * 2 + 1];
        }
    }

    // 6. IV chain
    let mut new_iv = iv.to_vec();
    for i in 0..16 {
        new_iv[i] ^= hmac_res[i];
    }

    (new_msg, new_iv)
}

/// Un-distort and verify a signed FIX message.
///
/// Returns (undistorted_msg, new_iv, signature_valid).
pub fn fix_unsign(msg: &[u8], mac_key: &[u8], iv: &[u8]) -> (Vec<u8>, Vec<u8>, bool) {
    let mut msg_bytes = msg.to_vec();

    let is_fix41 = msg.starts_with(b"8=FIX.4.1");
    let is_fixcomp = msg.starts_with(b"8=FIXCOMP");

    // 1. Un-distort (reverse XOR, loop backward)
    let start = if is_fixcomp {
        find_after_tag(&msg_bytes, 9).unwrap_or(0)
    } else {
        find_after_tag(&msg_bytes, 35).unwrap_or(0)
    };
    let mut end = msg_bytes.len() as isize - 14 - 2;
    if is_fix41 {
        end -= 7;
    }
    if end >= start as isize {
        let rng = end as usize - start + 1;
        for i in (0..8).rev() {
            let pos = (iv[i * 2] as usize) % rng;
            msg_bytes[start + pos] ^= iv[i * 2 + 1];
        }
    }

    // 2. Extract body + verify HMAC
    let after9 = match find_after_tag(&msg_bytes, 9) {
        Some(p) => p,
        None => return (msg_bytes, iv.to_vec(), false),
    };

    // Find 8349= tag
    let sig_needle = b"8349=";
    let t8349 = match msg_bytes
        .windows(sig_needle.len())
        .position(|w| w == sig_needle)
    {
        Some(p) => p,
        None => return (msg_bytes, iv.to_vec(), false),
    };

    let body = &msg_bytes[after9..t8349];

    let mut mac = Hmac::<Sha1>::new_from_slice(mac_key).unwrap();
    mac.update(iv);
    mac.update(body);
    let hmac_res = mac.finalize().into_bytes();
    let expected = xor_fold(&hmac_res);

    // Extract actual signature
    let sig_start = t8349 + sig_needle.len();
    let sig_end = msg_bytes[sig_start..]
        .iter()
        .position(|&b| b == SOH)
        .map(|p| sig_start + p)
        .unwrap_or(msg_bytes.len());
    let actual = String::from_utf8_lossy(&msg_bytes[sig_start..sig_end]).to_uppercase();
    let sig_valid = expected == actual;

    // 3. IV chain
    let mut new_iv = iv.to_vec();
    for i in 0..16 {
        new_iv[i] ^= hmac_res[i];
    }

    (msg_bytes, new_iv, sig_valid)
}

/// Read one complete FIX message from a reader.
///
/// Scans for `10=XXX\x01` (checksum tag) to detect message boundary.
pub fn fix_read<R: Read>(reader: &mut R) -> io::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(4096);
    let mut tmp = [0u8; 4096];
    loop {
        let n = reader.read(&mut tmp)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "socket closed while reading FIX message",
            ));
        }
        buf.extend_from_slice(&tmp[..n]);
        // Look for checksum tag ending: \x0110=XXX\x01
        if let Some(idx) = buf.windows(4).position(|w| w == b"\x0110=") {
            if let Some(end) = buf[idx + 4..].iter().position(|&b| b == SOH) {
                let total = idx + 4 + end + 1;
                return Ok(buf[..total].to_vec());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_abc() {
        assert_eq!(fix_checksum(b"abc"), format!("{:03}", (97 + 98 + 99) % 256));
    }

    #[test]
    fn checksum_zero_padded() {
        assert_eq!(fix_checksum(&[0x01]), "001");
    }

    #[test]
    fn build_structure() {
        let msg = fix_build(&[(35, "A"), (108, "10")], 1);
        let parsed = fix_parse(&msg);
        assert_eq!(parsed[&8], "FIX.4.1");
        assert!(parsed.contains_key(&9));
        assert_eq!(parsed[&34], "000001");
        assert!(parsed.contains_key(&10));
    }

    #[test]
    fn build_body_length() {
        let msg = fix_build(&[(35, "0")], 5);
        let text = String::from_utf8_lossy(&msg);
        let idx9 = text.find("9=").unwrap() + 2;
        let soh9 = text[idx9..].find('\x01').unwrap() + idx9;
        let body_len: usize = text[idx9..soh9].parse().unwrap();
        let body_start = soh9 + 1;
        let idx10 = text.rfind("\x0110=").unwrap();
        let actual_body = &msg[body_start..idx10 + 1];
        assert_eq!(body_len, actual_body.len());
    }

    #[test]
    fn build_parse_roundtrip() {
        let msg = fix_build(
            &[(35, "D"), (11, "12345"), (55, "AAPL"), (54, "1"), (38, "100")],
            42,
        );
        let parsed = fix_parse(&msg);
        assert_eq!(parsed[&35], "D");
        assert_eq!(parsed[&11], "12345");
        assert_eq!(parsed[&55], "AAPL");
        assert_eq!(parsed[&54], "1");
        assert_eq!(parsed[&38], "100");
        assert_eq!(parsed[&34], "000042");
    }

    #[test]
    fn parse_empty_value() {
        let msg = fix_build(&[(35, "A"), (58, "")], 1);
        let parsed = fix_parse(&msg);
        assert_eq!(parsed[&58], "");
    }

    #[test]
    fn xor_fold_sequential() {
        let data: Vec<u8> = (0u8..20).collect();
        let result = xor_fold(&data);
        assert_eq!(result.len(), 8);
        assert_eq!(result, result.to_uppercase());
        let mut r = [0u8; 4];
        for off in (0..20).step_by(4) {
            for i in 0..4 {
                r[i] ^= data[off + i];
            }
        }
        let expected: String = r.iter().map(|b| format!("{:02X}", b)).collect();
        assert_eq!(result, expected);
    }

    #[test]
    fn sign_unsign_roundtrip() {
        let mac_key: Vec<u8> = (0..20).collect();
        let iv: Vec<u8> = (0..16).collect();
        let msg = fix_build(
            &[(35, "D"), (55, "AAPL"), (54, "1"), (38, "100")],
            1,
        );
        let (signed, new_iv) = fix_sign(&msg, &mac_key, &iv);
        let (_, new_iv2, valid) = fix_unsign(&signed, &mac_key, &iv);
        assert!(valid);
        assert_eq!(new_iv, new_iv2);
    }

    #[test]
    fn sign_iv_chains() {
        let mac_key: Vec<u8> = (0..20).collect();
        let iv: Vec<u8> = (0..16).collect();
        let msg1 = fix_build(&[(35, "0")], 1);
        let (_, iv1) = fix_sign(&msg1, &mac_key, &iv);
        let msg2 = fix_build(&[(35, "0")], 2);
        let (_, iv2) = fix_sign(&msg2, &mac_key, &iv1);
        assert_ne!(iv, iv1);
        assert_ne!(iv1, iv2);
    }

    #[test]
    fn sign_wrong_key_fails() {
        let mac_key: Vec<u8> = (0..20).collect();
        let wrong_key: Vec<u8> = (20..40).collect();
        let iv: Vec<u8> = (0..16).collect();
        let msg = fix_build(&[(35, "0")], 1);
        let (signed, _) = fix_sign(&msg, &mac_key, &iv);
        let (_, _, valid) = fix_unsign(&signed, &wrong_key, &iv);
        assert!(!valid);
    }

    #[test]
    fn fix_parse_empty_value_raw() {
        // "35=\x01" should parse tag 35 with empty string
        let data = b"35=\x01";
        let parsed = fix_parse(data);
        assert_eq!(parsed.get(&35), Some(&String::new()));
    }

    #[test]
    fn fix_parse_non_fix_prefix() {
        // Random bytes before valid tag=value pairs should still parse
        let data = b"\xDE\xAD\xBE\xEF\x0155=AAPL\x0154=1\x01";
        let parsed = fix_parse(data);
        assert_eq!(parsed.get(&55), Some(&"AAPL".to_string()));
        assert_eq!(parsed.get(&54), Some(&"1".to_string()));
    }

    #[test]
    fn fix_build_many_tags_all_present() {
        let fields: Vec<(u32, &str)> = vec![
            (35, "D"),
            (11, "CLO001"),
            (55, "MSFT"),
            (54, "2"),
            (38, "500"),
            (40, "2"),
            (44, "350.50"),
            (59, "0"),
        ];
        let msg = fix_build(&fields, 10);
        let parsed = fix_parse(&msg);
        for (tag, val) in &fields {
            assert_eq!(
                parsed.get(tag),
                Some(&val.to_string()),
                "tag {} missing or wrong",
                tag
            );
        }
        // Auto-generated tags must also be present
        assert!(parsed.contains_key(&8));
        assert!(parsed.contains_key(&9));
        assert!(parsed.contains_key(&10));
        assert!(parsed.contains_key(&34));
    }

    #[test]
    fn fix_build_seq_zero_pads_to_6_digits() {
        let msg1 = fix_build(&[(35, "0")], 1);
        let parsed1 = fix_parse(&msg1);
        assert_eq!(parsed1[&34], "000001");

        let msg2 = fix_build(&[(35, "0")], 999999);
        let parsed2 = fix_parse(&msg2);
        assert_eq!(parsed2[&34], "999999");
    }

    #[test]
    fn fix_checksum_wraps_at_256() {
        // 256 bytes of value 1 → sum = 256, mod 256 = 0
        let data = vec![1u8; 256];
        assert_eq!(fix_checksum(&data), "000");

        // 255 bytes of value 1 → sum = 255, mod 256 = 255
        let data2 = vec![1u8; 255];
        assert_eq!(fix_checksum(&data2), "255");

        // 257 bytes of value 1 → sum = 257, mod 256 = 1
        let data3 = vec![1u8; 257];
        assert_eq!(fix_checksum(&data3), "001");
    }

    #[test]
    fn sign_unsign_iv_chaining_three_messages() {
        let mac_key: Vec<u8> = (0..20).collect();
        let iv: Vec<u8> = (0..16).collect();

        let msg1 = fix_build(&[(35, "D"), (55, "AAPL")], 1);
        let msg2 = fix_build(&[(35, "0")], 2);
        let msg3 = fix_build(&[(35, "A"), (108, "10")], 3);

        // Sign sequentially
        let (signed1, iv_after1) = fix_sign(&msg1, &mac_key, &iv);
        let (signed2, iv_after2) = fix_sign(&msg2, &mac_key, &iv_after1);
        let (signed3, iv_after3) = fix_sign(&msg3, &mac_key, &iv_after2);

        // Unsign in same order with same initial IV
        let (_, unsign_iv1, valid1) = fix_unsign(&signed1, &mac_key, &iv);
        assert!(valid1, "message 1 signature invalid");
        let (_, unsign_iv2, valid2) = fix_unsign(&signed2, &mac_key, &unsign_iv1);
        assert!(valid2, "message 2 signature invalid");
        let (_, unsign_iv3, valid3) = fix_unsign(&signed3, &mac_key, &unsign_iv2);
        assert!(valid3, "message 3 signature invalid");

        // IVs must match between sign and unsign chains
        assert_eq!(iv_after1, unsign_iv1);
        assert_eq!(iv_after2, unsign_iv2);
        assert_eq!(iv_after3, unsign_iv3);
    }

    #[test]
    fn unsign_wrong_iv_returns_invalid() {
        let mac_key: Vec<u8> = (0..20).collect();
        let iv: Vec<u8> = (0..16).collect();
        let wrong_iv: Vec<u8> = (16..32).collect();

        let msg = fix_build(&[(35, "D"), (55, "AAPL")], 1);
        let (signed, _) = fix_sign(&msg, &mac_key, &iv);

        let (_, _, valid) = fix_unsign(&signed, &mac_key, &wrong_iv);
        assert!(!valid);
    }

    #[test]
    fn unsign_empty_key_returns_original() {
        // When mac_key is empty, HMAC-SHA1 still works (empty key is valid).
        // The test verifies that sign → unsign with the same empty key is valid.
        let mac_key: Vec<u8> = vec![];
        let iv: Vec<u8> = (0..16).collect();

        let msg = fix_build(&[(35, "0")], 1);
        let (signed, _) = fix_sign(&msg, &mac_key, &iv);
        let (_, _, valid) = fix_unsign(&signed, &mac_key, &iv);
        assert!(valid);
    }

    // ── fix_read tests ─────────────────────────────────────────────────

    #[test]
    fn fix_read_single_message() {
        let msg = fix_build(&[(35, "0")], 1);
        let mut cursor = std::io::Cursor::new(msg.clone());
        let result = fix_read(&mut cursor).unwrap();
        assert_eq!(result, msg);
    }

    #[test]
    fn fix_read_logon_message() {
        let msg = fix_build(&[(35, "A"), (108, "30"), (98, "0")], 1);
        let mut cursor = std::io::Cursor::new(msg.clone());
        let result = fix_read(&mut cursor).unwrap();
        assert_eq!(result, msg);
        let parsed = fix_parse(&result);
        assert_eq!(parsed[&35], "A");
        assert_eq!(parsed[&108], "30");
    }

    #[test]
    fn fix_read_returns_first_message_from_buffer() {
        // fix_read is a single-message reader (used during handshake).
        // When two messages arrive together, it returns only the first.
        let msg1 = fix_build(&[(35, "0")], 1);
        let msg2 = fix_build(&[(35, "D"), (55, "AAPL")], 2);
        let mut buf = msg1.clone();
        buf.extend_from_slice(&msg2);
        let mut cursor = std::io::Cursor::new(buf);
        let r1 = fix_read(&mut cursor).unwrap();
        assert_eq!(r1, msg1);
    }

    #[test]
    fn fix_read_eof_returns_error() {
        let mut cursor = std::io::Cursor::new(Vec::<u8>::new());
        let err = fix_read(&mut cursor).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::ConnectionReset);
    }

    #[test]
    fn fix_read_incomplete_then_eof() {
        // Partial message (no checksum tag) then EOF
        let partial = b"8=FIX.4.1\x019=0010\x0135=0\x01".to_vec();
        let mut cursor = std::io::Cursor::new(partial);
        let err = fix_read(&mut cursor).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::ConnectionReset);
    }

    // ── fix_unsign edge case tests ────────────────────────────────────

    #[test]
    fn unsign_empty_data_no_panic() {
        let mac_key: Vec<u8> = (0..20).collect();
        let iv: Vec<u8> = (0..16).collect();
        let (_, _, valid) = fix_unsign(&[], &mac_key, &iv);
        assert!(!valid);
    }

    #[test]
    fn unsign_short_data_no_panic() {
        let mac_key: Vec<u8> = (0..20).collect();
        let iv: Vec<u8> = (0..16).collect();
        let (_, _, valid) = fix_unsign(b"8=FIX.4.1\x01", &mac_key, &iv);
        assert!(!valid);
    }

    #[test]
    fn unsign_tiny_fixcomp_no_panic() {
        let mac_key: Vec<u8> = (0..20).collect();
        let iv: Vec<u8> = (0..16).collect();
        let (_, _, valid) = fix_unsign(b"8=FIXCOMP\x01", &mac_key, &iv);
        assert!(!valid);
    }

    #[test]
    fn xor_fold_all_zero_digest() {
        let digest = [0u8; 20];
        let result = xor_fold(&digest);
        assert_eq!(result, "00000000");
    }

    #[test]
    fn xor_fold_known_values() {
        // Manually compute: 5 groups of 4 bytes each, XOR-folded
        // Group 0: [0x01, 0x02, 0x03, 0x04]
        // Group 1: [0x10, 0x20, 0x30, 0x40]
        // Group 2: [0x05, 0x06, 0x07, 0x08]
        // Group 3: [0xFF, 0x00, 0xFF, 0x00]
        // Group 4: [0xAA, 0xBB, 0xCC, 0xDD]
        let digest: [u8; 20] = [
            0x01, 0x02, 0x03, 0x04, // group 0
            0x10, 0x20, 0x30, 0x40, // group 1
            0x05, 0x06, 0x07, 0x08, // group 2
            0xFF, 0x00, 0xFF, 0x00, // group 3
            0xAA, 0xBB, 0xCC, 0xDD, // group 4
        ];
        let r0 = 0x01 ^ 0x10 ^ 0x05 ^ 0xFF ^ 0xAA;
        let r1 = 0x02 ^ 0x20 ^ 0x06 ^ 0x00 ^ 0xBB;
        let r2 = 0x03 ^ 0x30 ^ 0x07 ^ 0xFF ^ 0xCC;
        let r3 = 0x04 ^ 0x40 ^ 0x08 ^ 0x00 ^ 0xDD;
        let expected = format!("{:02X}{:02X}{:02X}{:02X}", r0, r1, r2, r3);
        assert_eq!(xor_fold(&digest), expected);
    }
}
