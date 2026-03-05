//! Connection lifecycle: CCP authentication and farm connection management.
//!
//! Orchestrates the full auth flow:
//! 1. Phase 1: Misc URLs (plaintext TCP port 4000)
//! 2. Phase 2: SSL + DH + SRP-6 against CCP (port 4001)
//! 3. Farm connections: DH + SOFT_TOKEN auth

use std::io::{self, Read, Write};
use std::net::TcpStream;

use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use num_bigint::BigUint;
use rand::RngCore;

use crate::auth::crypto::strip_leading_zeros;
use crate::auth::dh::SecureChannel;
use crate::auth::srp;
use crate::config::*;
use crate::protocol::ns::{self, *};
use crate::protocol::xyz;

/// Result of CCP authentication.
pub struct AuthResult {
    pub session_token: BigUint,
    pub token_type: String,
    pub session_id: String,
    pub features: Vec<String>,
    pub authenticated: bool,
}

/// Authenticated CCP session.
pub struct AuthSession {
    pub stream: TcpStream,
    pub channel: SecureChannel,
    pub auth_result: AuthResult,
    pub hw_info: String,
    pub encoded: String,
}

/// Authenticated farm session.
pub struct FarmSession {
    pub stream: TcpStream,
    pub channel: SecureChannel,
    pub auth_result: AuthResult,
    pub hw_info: String,
    pub encoded: String,
    pub farm_name: String,
    pub server_ns_version: u32,
}

// CONNECT_REQUEST flags
pub const FLAG_OK_TO_REDIRECT: u32 = 1;
pub const FLAG_IS_FARM: u32 = 2;
pub const FLAG_VERSION: u32 = 4;
pub const FLAG_VERSION_PRESENT: u32 = 8;
pub const FLAG_SOFT_TOKEN: u32 = 16;
pub const FLAG_DEVICE_INFO: u32 = 32;
pub const FLAG_PERMANENT_TOKEN: u32 = 64;
pub const FLAG_UNKNOWN_U: u32 = 4096;
pub const FLAG_PAPER_CONNECT: u32 = 8192;
pub const FLAG_FARM_NAME: u32 = 131072;
pub const FLAG_UNKNOWN_19: u32 = 524288;
pub const FLAG_UNKNOWN_20: u32 = 1048576;
pub const FLAG_TWSRO_TOKEN: u32 = 1024;

/// Generate a session ID matching TWS format: hex(epoch_secs).hex(millis%1000).
pub fn get_session_id() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    let millis = now.as_millis() as u64;
    let secs = millis / 1000;
    let ms = millis % 1000;
    format!("{:x}.{:04x}", secs, ms)
}

/// Generate hardware info string: `{machine_id}|{MAC}`.
pub fn get_hw_info() -> String {
    let machine_id = {
        let mut buf = [0u8; 4];
        rand::rng().fill_bytes(&mut buf);
        hex::encode(buf)
    };
    // Use a deterministic fallback MAC — real implementation would probe NIC
    format!("{}|00:00:00:00:00:00", machine_id)
}

/// Send an NS message encrypted via the DH channel (534).
pub fn send_secure<W: Write>(
    stream: &mut W,
    channel: &mut SecureChannel,
    inner: &[u8],
) -> io::Result<()> {
    let ct = channel.encrypt(inner);
    let ct_b64 = B64.encode(&ct);
    let outer = format!("{};{};{};", NS_VERSION, NS_SECURE_MESSAGE, ct_b64);
    let payload = outer.as_bytes();
    let mut msg = Vec::with_capacity(8 + payload.len());
    msg.extend_from_slice(NS_MAGIC);
    msg.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    msg.extend_from_slice(payload);
    stream.write_all(&msg)?;
    Ok(())
}

/// Receive an encrypted 534 response and decrypt.
pub fn recv_secure<R: Read>(
    stream: &mut R,
    channel: &mut SecureChannel,
) -> io::Result<Vec<u8>> {
    let (payload, _) = ns::ns_recv(stream)?;
    let text = String::from_utf8_lossy(&payload);
    let parts: Vec<&str> = text.split(';').collect();

    if parts.len() < 2 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "malformed NS response"));
    }

    let msg_type: u32 = parts[1]
        .parse()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid msg type"))?;

    if msg_type == NS_SECURE_ERROR {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("Secure error: {}", parts[2..].join(";")),
        ));
    }
    if msg_type != NS_SECURE_MESSAGE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Expected 534, got {}: {}", msg_type, text),
        ));
    }

    let ct = B64
        .decode(parts[2])
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    channel
        .decrypt(&ct)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Receive a `#%#%` framed message and classify as NS text or XYZ binary.
pub fn recv_msg<R: Read>(stream: &mut R) -> io::Result<RecvMsg> {
    let (payload, _) = ns::ns_recv(stream)?;

    // Try NS text first
    if ns::is_ns_text(&payload) {
        if let Some((version, msg_type, fields)) = ns::ns_parse(&payload) {
            return Ok(RecvMsg::Ns {
                version,
                msg_type,
                fields,
            });
        }
    }

    // Try XYZ binary
    if payload.len() >= 16 {
        if let Some((msg_id, sub_id, state, fields)) = xyz::xyz_parse_response(&payload) {
            return Ok(RecvMsg::Xyz {
                msg_id,
                sub_id,
                state,
                fields,
            });
        }
    }

    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        format!("Cannot parse message: {:?}", &payload[..payload.len().min(40)]),
    ))
}

/// Classified received message.
pub enum RecvMsg {
    Ns {
        version: u32,
        msg_type: u32,
        fields: Vec<String>,
    },
    Xyz {
        msg_id: u32,
        sub_id: u32,
        state: u32,
        fields: Vec<String>,
    },
}

/// Extract non-empty data fields from SRP response, skipping username.
fn extract_srp_data(fields: &[String], username: &str) -> Vec<String> {
    fields
        .iter()
        .filter(|f| !f.is_empty() && f.as_str() != username)
        .cloned()
        .collect()
}

/// Execute SRP-6 protocol (XYZ msg 777, states 1-6) via raw binary.
///
/// Returns the session key K as BigUint.
pub fn do_srp<S: Read + Write>(stream: &mut S, username: &str, password: &str) -> io::Result<BigUint> {
    let n = srp::srp_n();
    let g = BigUint::from(srp::SRP_G);

    // State 1: Send AUTH_QUERY
    let msg1 = xyz::xyz_build_srp_v20(1, &[]);
    stream.write_all(&xyz::xyz_wrap(&msg1))?;

    // State 2: Receive AUTH_PARAMS
    let recv2 = recv_msg(stream)?;
    let fields2 = match recv2 {
        RecvMsg::Xyz { state, fields, .. } => {
            if state != 2 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Expected SRP state 2, got {}", state),
                ));
            }
            fields
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Expected XYZ response for SRP state 2",
            ));
        }
    };

    let data_fields = extract_srp_data(&fields2, username);
    // Server may provide N and g, or we use defaults
    let (n, g) = if data_fields.len() >= 2 {
        if let (Some(server_n), Some(server_g)) = (
            BigUint::parse_bytes(data_fields[0].as_bytes(), 16),
            BigUint::parse_bytes(data_fields[1].as_bytes(), 16),
        ) {
            (server_n, server_g)
        } else {
            (n, g)
        }
    } else {
        (n, g)
    };

    // Generate client keys: a (private), A = g^a mod N
    let mut a_bytes = [0u8; 4];
    rand::rng().fill_bytes(&mut a_bytes);
    let a_priv = BigUint::from_bytes_be(&a_bytes);
    let a_pub = g.modpow(&a_priv, &n);

    // State 3: Send client public key A
    let a_hex = format!("{:x}", a_pub);
    let msg3 = xyz::xyz_build_srp_v20(3, &[("L", &a_hex)]);
    stream.write_all(&xyz::xyz_wrap(&msg3))?;

    // State 4: Receive SERVER_PARAMS (salt, B)
    let recv4 = recv_msg(stream)?;
    let (state4, fields4) = match recv4 {
        RecvMsg::Xyz { state, fields, .. } => (state, fields),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Expected XYZ response for SRP state 4",
            ));
        }
    };

    if state4 == 7 {
        let result = fields4.get(9).map(|s| s.as_str()).unwrap_or("FAILED");
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("SRP early error (state 7): {}", result),
        ));
    }
    if state4 != 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Expected SRP state 4, got {}", state4),
        ));
    }

    let data_fields = extract_srp_data(&fields4, username);
    if data_fields.len() < 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Missing salt/B in SRP state 4",
        ));
    }

    let salt_hex = &data_fields[0];
    let b_hex = &data_fields[1];
    let salt_bytes = hex::decode(salt_hex)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    let b_pub = BigUint::parse_bytes(b_hex.as_bytes(), 16).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "Invalid B hex")
    })?;

    // Compute SRP values
    let x = srp::srp_compute_x(
        strip_leading_zeros(&salt_bytes),
        username,
        password,
    );
    let u = srp::srp_compute_u(&a_pub, &b_pub);
    let k_mult = BigUint::from(srp::SRP_K);
    let s = srp::srp_compute_s(&b_pub, &a_priv, &u, &x, &n, &g, &k_mult);
    let k = srp::srp_compute_k(&s);

    // Compute client proof M1
    let salt_int = BigUint::parse_bytes(salt_hex.as_bytes(), 16).unwrap_or_default();
    let m1 = srp::srp_compute_m1(&n, &g, username, &salt_int, &a_pub, &b_pub, &k);

    // State 5: Send client proof M1
    let m1_hex = format!("{:x}", m1);
    let msg5 = xyz::xyz_build_srp_v20(5, &[("N", &m1_hex)]);
    stream.write_all(&xyz::xyz_wrap(&msg5))?;

    // State 6: Receive AUTH_RESULT
    let recv6 = recv_msg(stream)?;
    let (state6, fields6) = match recv6 {
        RecvMsg::Xyz { state, fields, .. } => (state, fields),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Expected XYZ response for SRP state 6",
            ));
        }
    };

    let result = fields6
        .get(9)
        .filter(|s| !s.is_empty())
        .or_else(|| fields6.iter().rev().find(|s| !s.is_empty()))
        .map(|s| s.as_str())
        .unwrap_or("");

    if state6 == 6 && result == "PASSED" {
        Ok(k)
    } else if result == "NEEDSSL" {
        Err(io::Error::new(
            io::ErrorKind::Other,
            "Server requires SSL upgrade (NEEDSSL)",
        ))
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("SRP Authentication FAILED (state={}): {}", state6, result),
        ))
    }
}

/// Execute SOFT_TOKEN authentication (XYZ msg 772) for farm connections.
/// Wrap XYZ binary in `8=1|35=X` FIX framing (farm protocol).
fn wrap_xyz_fix(xyz_payload: &[u8]) -> Vec<u8> {
    let tag35 = b"35=X\x01";
    let body_len = tag35.len() + xyz_payload.len();
    let header = format!("8=1\x019={:04}\x01", body_len);
    let mut msg = Vec::with_capacity(header.len() + tag35.len() + xyz_payload.len());
    msg.extend_from_slice(header.as_bytes());
    msg.extend_from_slice(tag35);
    msg.extend_from_slice(xyz_payload);
    msg
}

/// Read one `8=1` FIX message from a farm stream.
fn recv_8eq1(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(4096);
    let mut tmp = [0u8; 4096];
    loop {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "farm connection closed during SOFT_TOKEN",
            ));
        }
        buf.extend_from_slice(&tmp[..n]);
        // Look for complete 8=1 message: starts with "8=1\x01" and has a body
        if buf.starts_with(b"8=1\x01") {
            // Parse body length from 9=NNNN
            if let Some(nine_pos) = buf.windows(2).position(|w| w == b"9=") {
                let val_start = nine_pos + 2;
                if let Some(soh_pos) = buf[val_start..].iter().position(|&b| b == 0x01) {
                    let body_len: usize = std::str::from_utf8(&buf[val_start..val_start + soh_pos])
                        .ok()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0);
                    let header_end = val_start + soh_pos + 1;
                    let total = header_end + body_len;
                    if buf.len() >= total {
                        return Ok(buf[..total].to_vec());
                    }
                }
            }
        }
    }
}

/// Extract XYZ binary from `8=1|35=X` FIX message.
fn extract_xyz(msg: &[u8]) -> &[u8] {
    let marker = b"35=X\x01";
    if let Some(idx) = msg.windows(marker.len()).position(|w| w == marker) {
        &msg[idx + marker.len()..]
    } else {
        msg
    }
}

pub fn do_soft_token(stream: &mut TcpStream, session_token: &BigUint) -> io::Result<()> {
    use sha1::{Digest, Sha1};

    // State 1: Send empty init (FIX-framed for farm)
    let msg1 = xyz::xyz_build_soft_token(1, "", "", "");
    stream.write_all(&wrap_xyz_fix(&msg1))?;

    // State 2: Receive challenge (FIX-framed)
    let recv2 = recv_8eq1(stream)?;
    let xyz2 = extract_xyz(&recv2);
    let (_, _, state2, fields2) = xyz::xyz_parse_response(xyz2)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "SOFT_TOKEN: invalid XYZ state 2"))?;

    if state2 != 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("SOFT_TOKEN: expected state 2, got {}", state2),
        ));
    }

    let challenge_hex = fields2
        .get(1)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "SOFT_TOKEN: empty challenge"))?;

    // SHA-1(strip_zeros(challenge_bytes) + strip_zeros(token_bytes))
    let challenge_int = BigUint::parse_bytes(challenge_hex.as_bytes(), 16)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Invalid challenge hex"))?;
    let challenge_be = challenge_int.to_bytes_be();
    let challenge_bytes = strip_leading_zeros(&challenge_be);
    let token_be = session_token.to_bytes_be();
    let token_bytes = strip_leading_zeros(&token_be);

    let mut hasher = Sha1::new();
    hasher.update(challenge_bytes);
    hasher.update(token_bytes);
    let digest = hasher.finalize();
    let response_int = BigUint::from_bytes_be(&digest);
    let response_hex = format!("{:x}", response_int);

    // State 3: Send hash response (FIX-framed)
    let msg3 = xyz::xyz_build_soft_token(3, "", &response_hex, "");
    stream.write_all(&wrap_xyz_fix(&msg3))?;

    // State 4: Receive result (FIX-framed)
    let recv4 = recv_8eq1(stream)?;
    let xyz4 = extract_xyz(&recv4);
    let (_, _, _, fields4) = xyz::xyz_parse_response(xyz4)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "SOFT_TOKEN: invalid XYZ state 4"))?;

    let result = fields4
        .get(3)
        .filter(|s| !s.is_empty())
        .or_else(|| fields4.iter().rev().find(|s| !s.is_empty()))
        .map(|s| s.as_str())
        .unwrap_or("");

    if result == "PASSED" {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("SOFT_TOKEN auth failed: {}", result),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── get_session_id ──────────────────────────────────────────────────

    #[test]
    fn session_id_format() {
        let id = get_session_id();
        assert!(id.contains('.'));
        let parts: Vec<&str> = id.split('.').collect();
        assert_eq!(parts.len(), 2);
        // Both parts should be valid hex
        assert!(u64::from_str_radix(parts[0], 16).is_ok());
        assert!(u64::from_str_radix(parts[1], 16).is_ok());
    }

    #[test]
    fn session_id_two_calls_differ() {
        // Time-based: successive calls should produce different IDs
        // (sleep 1ms to guarantee millisecond tick)
        let id1 = get_session_id();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let id2 = get_session_id();
        assert_ne!(id1, id2, "Two session IDs generated at different times must differ");
    }

    #[test]
    fn session_id_hex_lengths() {
        let id = get_session_id();
        let parts: Vec<&str> = id.split('.').collect();
        // Seconds part: lowercase hex, at least 1 char
        assert!(!parts[0].is_empty());
        // Millis part: always 4 hex chars (format {:04x}, range 0..999)
        assert_eq!(parts[1].len(), 4, "Millis part must be zero-padded to 4 hex chars");
    }

    // ── get_hw_info ─────────────────────────────────────────────────────

    #[test]
    fn hw_info_format() {
        let info = get_hw_info();
        assert!(info.contains('|'));
        let parts: Vec<&str> = info.split('|').collect();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].len(), 8); // 4-byte hex
    }

    #[test]
    fn hw_info_two_calls_differ() {
        let info1 = get_hw_info();
        let info2 = get_hw_info();
        let machine1 = info1.split('|').next().unwrap();
        let machine2 = info2.split('|').next().unwrap();
        // Random machine IDs should differ (8-hex-char random, collision negligible)
        assert_ne!(machine1, machine2, "Two random machine IDs should differ");
    }

    #[test]
    fn hw_info_mac_always_zeroed() {
        for _ in 0..5 {
            let info = get_hw_info();
            let mac = info.split('|').nth(1).unwrap();
            assert_eq!(mac, "00:00:00:00:00:00");
        }
    }

    // ── extract_srp_data ────────────────────────────────────────────────

    #[test]
    fn extract_srp_data_empty_fields() {
        let fields: Vec<String> = vec![];
        let result = extract_srp_data(&fields, "user");
        assert!(result.is_empty());
    }

    #[test]
    fn extract_srp_data_only_username() {
        let fields = vec!["user".to_string()];
        let result = extract_srp_data(&fields, "user");
        assert!(result.is_empty(), "Username should be filtered out");
    }

    #[test]
    fn extract_srp_data_username_and_empties() {
        let fields = vec![
            "".to_string(),
            "user".to_string(),
            "".to_string(),
        ];
        let result = extract_srp_data(&fields, "user");
        assert!(result.is_empty(), "Empty strings and username should all be filtered");
    }

    #[test]
    fn extract_srp_data_returns_non_empty_non_username() {
        let fields = vec![
            "".to_string(),
            "user".to_string(),
            "abc123".to_string(),
            "".to_string(),
            "def456".to_string(),
        ];
        let result = extract_srp_data(&fields, "user");
        assert_eq!(result, vec!["abc123", "def456"]);
    }

    // ── RecvMsg enum ────────────────────────────────────────────────────

    #[test]
    fn recv_msg_ns_variant() {
        let msg = RecvMsg::Ns {
            version: 534,
            msg_type: 99,
            fields: vec!["a".into(), "b".into()],
        };
        match msg {
            RecvMsg::Ns { version, msg_type, fields } => {
                assert_eq!(version, 534);
                assert_eq!(msg_type, 99);
                assert_eq!(fields.len(), 2);
            }
            _ => panic!("Expected Ns variant"),
        }
    }

    #[test]
    fn recv_msg_xyz_variant() {
        let msg = RecvMsg::Xyz {
            msg_id: 777,
            sub_id: 1,
            state: 6,
            fields: vec!["PASSED".into()],
        };
        match msg {
            RecvMsg::Xyz { msg_id, sub_id, state, fields } => {
                assert_eq!(msg_id, 777);
                assert_eq!(sub_id, 1);
                assert_eq!(state, 6);
                assert_eq!(fields, vec!["PASSED"]);
            }
            _ => panic!("Expected Xyz variant"),
        }
    }

    // ── AuthResult struct ───────────────────────────────────────────────

    #[test]
    fn auth_result_default_like_init() {
        let ar = AuthResult {
            session_token: BigUint::ZERO,
            token_type: String::new(),
            session_id: String::new(),
            features: Vec::new(),
            authenticated: false,
        };
        assert_eq!(ar.session_token, BigUint::ZERO);
        assert!(ar.token_type.is_empty());
        assert!(ar.session_id.is_empty());
        assert!(ar.features.is_empty());
        assert!(!ar.authenticated);
    }

    #[test]
    fn auth_result_all_fields_accessible() {
        let ar = AuthResult {
            session_token: BigUint::from(42u32),
            token_type: "SRP".to_string(),
            session_id: "abc.0001".to_string(),
            features: vec!["feat1".into(), "feat2".into()],
            authenticated: true,
        };
        assert_eq!(ar.session_token, BigUint::from(42u32));
        assert_eq!(ar.token_type, "SRP");
        assert_eq!(ar.session_id, "abc.0001");
        assert_eq!(ar.features.len(), 2);
        assert!(ar.authenticated);
    }

    // ── Constants ───────────────────────────────────────────────────────

    #[test]
    fn flag_ok_to_redirect_value() {
        assert_eq!(FLAG_OK_TO_REDIRECT, 1);
    }

    #[test]
    fn flag_paper_connect_value() {
        assert_eq!(FLAG_PAPER_CONNECT, 8192);
    }

    #[test]
    fn flag_soft_token_value() {
        assert_eq!(FLAG_SOFT_TOKEN, 16);
    }

    #[test]
    fn flags_can_be_ored() {
        let combined = FLAG_OK_TO_REDIRECT | FLAG_PAPER_CONNECT | FLAG_SOFT_TOKEN;
        assert_eq!(combined, 1 | 8192 | 16);
        assert_eq!(combined, 8209);
        // Each flag bit is independent
        assert_ne!(combined & FLAG_OK_TO_REDIRECT, 0);
        assert_ne!(combined & FLAG_PAPER_CONNECT, 0);
        assert_ne!(combined & FLAG_SOFT_TOKEN, 0);
        // A flag we did NOT set should be absent
        assert_eq!(combined & FLAG_IS_FARM, 0);
    }
}
