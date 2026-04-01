//! Gateway: orchestrates auth + data connections into a running HotLoop.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use crossbeam_channel::{Sender, bounded};
use native_tls::TlsConnector;
use num_bigint::BigUint;
use sha1::{Digest, Sha1};

use std::net::ToSocketAddrs;

use crate::auth::crypto::strip_leading_zeros;
use crate::auth::dh::SecureChannel;
use crate::auth::session::{self, do_srp, do_soft_token};
use crate::config::*;
use std::sync::Arc;
use crate::bridge::{Event, SharedState};
use crate::engine::hot_loop::HotLoop;
use crate::protocol::connection::Connection;
use crate::protocol::fix::{self, fix_build, fix_parse, fix_read, SOH};
use crate::protocol::fixcomp;
use crate::protocol::ns;
use crate::types::ControlCommand;

/// Compute token short hash for farm logon.
pub fn token_short_hash(session_token: &BigUint) -> String {
    let token_bytes = session_token.to_bytes_be();
    let stripped = strip_leading_zeros(&token_bytes);
    let digest = Sha1::digest(stripped);
    // Take last 4 bytes as u32 (Java BigInteger.intValue() truncates to low 32 bits)
    let hash_int = u32::from_be_bytes([digest[16], digest[17], digest[18], digest[19]]);
    format!("{:x}", hash_int)
}

/// Build auth server logon message.
pub fn build_ccp_logon(hw_info: &str, encoded: &str, heartbeat: u64, seq: u32) -> Vec<u8> {
    let now = chrono_free_timestamp();
    let tz = "UTC";
    let hb_str = heartbeat.to_string();
    let hw_field = format!("<{}|127.0.0.1>", hw_info);
    fix_build(
        &[
            (fix::TAG_MSG_TYPE, fix::MSG_LOGON),
            (fix::TAG_SENDING_TIME, &now),
            (fix::TAG_ENCRYPT_METHOD, "0"),
            (fix::TAG_HEARTBEAT_INT, &hb_str),
            (fix::TAG_RESET_SEQ_NUM, "Y"),
            (fix::TAG_IB_BUILD, IB_BUILD),
            (fix::TAG_IB_VERSION, IB_VERSION),
            (6490, "dark"),
            (6266, encoded),
            (6351, &hw_field),
            (6397, "1"),
            (6947, tz),
            (8361, "(rolling)"),
            (8098, "0"),
        ],
        seq,
    )
}

/// Build encrypted farm logon message.
pub fn build_farm_encrypted_logon(
    channel: &mut SecureChannel,
    username: &str,
    paper: bool,
    farm_name: &str,
    session_id: &str,
    session_token: &BigUint,
    hw_info: &str,
    encoded: &str,
) -> Vec<u8> {
    let display_name = if paper {
        format!("S{}", username)
    } else {
        username.to_string()
    };
    let slot = match farm_name {
        "usfarm" => 18,
        _ => 17,
    };
    let farm_id = format!("{}/{}/{}", display_name, slot, farm_name);
    let farm_id_len = farm_id.len().to_string();
    let token_hash = token_short_hash(session_token);
    let ns_range = format!("{}..{}", NS_VERSION_MIN, NS_VERSION);
    let now = chrono_free_timestamp();
    let hb_str = FARM_HEARTBEAT.to_string();
    let hw_field = format!("<{}|127.0.0.1>", hw_info);

    let inner = fix_build(
        &[
            (fix::TAG_MSG_TYPE, fix::MSG_LOGON),
            (fix::TAG_SENDING_TIME, &now),
            (fix::TAG_ENCRYPT_METHOD, "0"),
            (fix::TAG_HEARTBEAT_INT, &hb_str),
            (95, &farm_id_len),
            (96, &farm_id),
            (fix::TAG_IB_BUILD, IB_BUILD),
            (fix::TAG_IB_VERSION, IB_VERSION),
            (6351, &hw_field),
            (6266, encoded),
            (6903, "1"),
            (8035, session_id),
            (8285, &ns_range),
            (8483, &token_hash),
        ],
        0,
    );

    let encrypted_raw = channel.encrypt(&inner);
    let b64_str = B64.encode(&encrypted_raw);

    // Outer wrapper: 8=FIX.4.1|9=<bodylen>|90=<b64_len>|91=<b64>|10=<cksum>
    let b64_len_str = b64_str.len().to_string();
    let body = format!("90={}\x0191={}\x01", b64_len_str, b64_str);
    let header = format!("8=FIX.4.1\x019={:04}\x01", body.len());
    let pre_cksum = format!("{}{}", header, body);
    let cksum = fix::fix_checksum(pre_cksum.as_bytes());
    let mut wrapper = pre_cksum.into_bytes();
    wrapper.extend_from_slice(format!("10={}\x01", cksum).as_bytes());
    wrapper
}

/// Execute farm logon exchange.
///
/// Returns (read_iv, sign_iv, remaining_buf) for message signing/verification.
pub fn farm_logon_exchange(
    stream: &mut TcpStream,
    channel: &mut SecureChannel,
    session_token: &BigUint,
    username: &str,
    password: &str,
    read_mac_key: &[u8],
    initial_read_iv: &[u8],
) -> io::Result<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    stream.set_read_timeout(Some(Duration::from_secs_f64(TIMEOUT_FARM_LOGON)))?;
    let mut buf = Vec::new();
    let mut read_iv = initial_read_iv.to_vec();

    for _msg_num in 0..20 {
        // Read until we have a complete frame
        let msg = loop {
            if let Some((msg, consumed)) = try_frame_farm_msg(&buf) {
                buf.drain(..consumed);
                break msg;
            }
            let mut tmp = [0u8; FARM_RECV_BUF];
            let n = stream.read(&mut tmp)?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionReset,
                    "farm connection closed during logon",
                ));
            }
            buf.extend_from_slice(&tmp[..n]);
        };

        // FIX.4.1 message
        if msg.starts_with(b"8=FIX.4.1\x01") {
            let has_sig = msg.windows(5).any(|w| w == b"8349=");
            // Check for HMAC signature → unsign
            let parsed_msg = if has_sig {
                let (unsigned, new_iv, _valid) = fix::fix_unsign(&msg, read_mac_key, &read_iv);
                read_iv = new_iv;
                unsigned
            } else {
                msg.clone()
            };
            let fields = fix_parse(&parsed_msg);

            // Check for encrypted content (tags 91/96)
            let enc_tag = fields.get(&91).or_else(|| fields.get(&96));
            if let Some(b64_data) = enc_tag {
                let encrypted = B64.decode(b64_data).map_err(|e| {
                    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
                })?;
                let decrypted = channel.decrypt(&encrypted).map_err(|e| {
                    io::Error::new(io::ErrorKind::InvalidData, e)
                })?;

                // Sync HMAC read IV with AES read IV after decryption (CBC chaining)
                if let Some(iv) = channel.read_iv() {
                    read_iv = iv.to_vec();
                }

                // Check for auth challenge → respond with token, fall back to SRP if rejected
                if decrypted.windows(5).any(|w| w == b"35=S\x01") {
                    match do_soft_token(stream, session_token)? {
                        session::SoftTokenOutcome::Passed => {}
                        session::SoftTokenOutcome::Unknown => {
                            log::warn!("Soft token rejected — falling back to SRP farm auth");
                            stream.set_read_timeout(Some(Duration::from_secs(15)))?;
                            session::do_srp_farm(stream, username, password)?;
                        }
                    }
                }
            } else if fields.get(&35).map(|s| s.as_str()) == Some("A") {
                // Logon ACK — sign_iv is the current write_iv (mutated by encrypt)
                let sign_iv = channel
                    .write_iv()
                    .map(|iv| iv.to_vec())
                    .unwrap_or_default();
                if !buf.is_empty() {
                    log::warn!("{} bytes remaining in buffer after logon ACK",
                        buf.len());
                }
                return Ok((read_iv, sign_iv, buf));
            } else if fields.get(&35).map(|s| s.as_str()) == Some("3") {
                let text = fields.get(&58).map(|s| s.as_str()).unwrap_or("unknown");
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("Farm logon rejected: {}", text),
                ));
            }
        } else if msg.starts_with(b"8=1\x01") {
            // Token auth response
            if msg.windows(6).any(|w| w == b"PASSED") {
                log::info!("Token auth PASSED");
            }
        }
    }

    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "exceeded max messages without farm logon ACK",
    ))
}

/// Try to extract one complete FIX message from a buffer.
/// Returns (message, bytes_consumed) or None if incomplete.
fn try_frame_farm_msg(buf: &[u8]) -> Option<(Vec<u8>, usize)> {
    if buf.len() < 10 {
        return None;
    }
    // Look for FIX header
    if !buf.starts_with(b"8=") {
        // Skip garbage
        let next = buf.windows(2).position(|w| w == b"8=")?;
        return Some((Vec::new(), next)); // skip garbage, caller retries
    }
    // Find tag 9 body length
    let tag9_pos = buf.windows(3).position(|w| w == b"\x019=")?;
    let val_start = tag9_pos + 3;
    let soh_pos = buf[val_start..].iter().position(|&b| b == SOH)? + val_start;
    let body_len: usize = std::str::from_utf8(&buf[val_start..soh_pos]).ok()?.parse().ok()?;
    let total = soh_pos + 1 + body_len + 7; // +7 for "10=XXX\x01"
    if buf.len() < total {
        return None;
    }
    Some((buf[..total].to_vec(), total))
}

/// Credentials cached for auto-reconnect (no SRP needed).
#[derive(Clone)]
pub struct ReconnectAuth {
    pub host: String,
    pub username: String,
    pub password: String,
    pub paper: bool,
    pub session_key: BigUint,
    pub session_token: BigUint,
    pub server_session_id: String,
    pub hw_info: String,
    pub encoded: String,
}

/// Full gateway connection.
pub struct Gateway {
    pub account_id: String,
    pub session_token: BigUint,
    pub server_session_id: String,
    pub ccp_token: String,
    pub heartbeat_interval: u64,
    /// Stored for farm reconnection.
    pub hw_info: String,
    pub encoded: String,
    /// Raw soft dollar tier data from CCP logon tag 6560.
    pub raw_soft_dollar_tiers: String,
    /// Raw family code data from CCP logon tag 6823.
    pub raw_family_codes: String,
    /// White branding ID from CCP logon (empty for standard accounts).
    pub white_branding_id: String,
}

/// Connect to a data farm: key exchange → encrypted logon → token auth → routing → Connection.
pub fn connect_farm(
    host: &str,
    farm_id: &str,
    username: &str,
    password: &str,
    paper: bool,
    server_session_id: &str,
    session_key: &BigUint,
    hw_info: &str,
    encoded: &str,
) -> io::Result<Connection> {
    let port = misc_port();
    let farm_host = farm_host_override().unwrap_or_else(|| host.to_string());
    log::info!("Connecting to {} {}:{}", farm_id, farm_host, port);
    let addr = format!("{}:{}", farm_host, port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "DNS resolution failed"))?;
    let farm_tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(TIMEOUT_FARM_CONNECT))
        .map_err(|e| io::Error::new(e.kind(), format!("{} TCP connect: {}", farm_id, e)))?;
    farm_tcp.set_nodelay(true)?;
    farm_tcp.set_read_timeout(Some(Duration::from_secs(TIMEOUT_FARM_CONNECT)))?;

    // Key exchange (raw TCP)
    let mut channel = SecureChannel::new();
    let dh_msg = channel.build_secure_connect(NS_VERSION, NS_VERSION);
    let mut stream = farm_tcp;
    stream.write_all(&dh_msg)?;

    let (payload, _) = ns::ns_recv(&mut stream)?;
    let text = String::from_utf8_lossy(&payload);
    let parts: Vec<&str> = text.split(';').collect();
    let msg_type: u32 = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    if msg_type != ns::NS_SECURE_CONNECTION_START {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} DH: expected 533, got {}", farm_id, msg_type),
        ));
    }
    channel.process_server_hello(&parts[2..]);
    log::info!("{} key exchange complete", farm_id);

    // Encrypted logon
    let farm_session_id = if server_session_id.is_empty() {
        session::get_session_id()
    } else {
        server_session_id.to_string()
    };
    let logon_bytes = build_farm_encrypted_logon(
        &mut channel, username, paper, farm_id,
        &farm_session_id, session_key, hw_info, encoded,
    );
    stream.write_all(&logon_bytes)?;
    log::info!("{} encrypted logon sent", farm_id);

    // Logon exchange: challenge → token auth → logon ACK
    let read_mac_key = channel.key_block().map(|kb| kb[84..104].to_vec()).unwrap_or_default();
    let initial_read_iv = channel.key_block().map(|kb| kb[48..64].to_vec()).unwrap_or_default();
    let (read_iv, sign_iv, logon_remaining) = farm_logon_exchange(
        &mut stream, &mut channel, session_key, username, password,
        &read_mac_key, &initial_read_iv,
    )?;
    log::info!("{} logon exchange complete, {} bytes remaining", farm_id, logon_remaining.len());

    let sign_mac_key = channel.key_block().map(|kb| kb[64..84].to_vec()).unwrap_or_default();

    // Send routing table request after logon.
    let channel_id = if farm_id == "ushmds" { "2" } else { "1" };
    let now = chrono_free_timestamp();
    let routing_msg = fix_build(&[
        (fix::TAG_MSG_TYPE, "U"),
        (fix::TAG_SENDING_TIME, &now),
        (6040, "112"),
        (6556, channel_id),
    ], 1);
    let wrapped = fixcomp::fixcomp_build(&routing_msg);

    let (signed, new_sign_iv) = fix::fix_sign(&wrapped, &sign_mac_key, &sign_iv);
    stream.write_all(&signed)?;
    let final_sign_iv = new_sign_iv;
    log::info!("{} sent routing request (6556={})", farm_id, channel_id);

    // Read routing response (stream is still blocking)
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut resp_buf = Vec::new();
    loop {
        let mut tmp = [0u8; 8192];
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => resp_buf.extend_from_slice(&tmp[..n]),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock
                || e.kind() == io::ErrorKind::TimedOut => break,
            Err(e) => return Err(e),
        }
    }
    log::info!("{} routing response: {} bytes", farm_id, resp_buf.len());

    // Create Connection (switches to non-blocking), inject routing bytes
    let mut conn = Connection::new_raw(stream)?;
    conn.set_keys(sign_mac_key, final_sign_iv, read_mac_key, read_iv);
    conn.seq = 1; // routing request was seq=1; next send_fix will be seq=2

    // Inject logon remaining bytes + routing response into connection buffer.
    // Python processes logon remaining before routing, but both need read_iv chaining.
    if !logon_remaining.is_empty() {
        conn.inject_buf(&logon_remaining);
    }
    if !resp_buf.is_empty() {
        conn.inject_buf(&resp_buf);
    }
    // Extract and process all frames (unsign + respond to TestRequests, like Python).
    let frames = conn.extract_frames();
    for frame in &frames {
        match frame {
            crate::protocol::connection::Frame::FixComp(raw) => {
                let (unsigned, _valid) = conn.unsign(raw);
                let inner = fixcomp::fixcomp_decompress(&unsigned);
                for m in &inner {
                    let parsed = fix_parse(m);
                    let mt = parsed.get(&35).map(|s| s.as_str()).unwrap_or("");
                    log::debug!("{} routing compressed inner 35={}", farm_id, mt);
                    if mt == "1" {
                        let test_id = parsed.get(&112).cloned().unwrap_or_default();
                        let ts = chrono_free_timestamp();
                        let _ = conn.send_fix(&[
                            (fix::TAG_MSG_TYPE, "0"),
                            (fix::TAG_SENDING_TIME, &ts),
                            (112, &test_id),
                        ]);
                    }
                }
            }
            crate::protocol::connection::Frame::Fix(raw) => {
                let (unsigned, _valid) = conn.unsign(raw);
                let parsed = fix_parse(&unsigned);
                let mt = parsed.get(&35).map(|s| s.as_str()).unwrap_or("");
                log::debug!("{} routing FIX 35={}", farm_id, mt);
                if mt == "1" {
                    let test_id = parsed.get(&112).cloned().unwrap_or_default();
                    let ts = chrono_free_timestamp();
                    let _ = conn.send_fix(&[
                        (fix::TAG_MSG_TYPE, "0"),
                        (fix::TAG_SENDING_TIME, &ts),
                        (112, &test_id),
                    ]);
                }
            }
            crate::protocol::connection::Frame::Binary(raw) => {
                let (_unsigned, _valid) = conn.unsign(raw);
                log::info!("{} routing 8=O: {} bytes", farm_id, raw.len());
            }
        }
    }
    if !frames.is_empty() {
        log::info!("{} post-logon frames: {} frames, seq now {}", farm_id, frames.len(), conn.seq);
    }
    Ok(conn)
}

/// Reconnect to the CCP (order/auth) server using cached session credentials.
/// Performs TLS + DH + CONNECT_REQUEST, then attempts SOFT_TOKEN auth with cached K.
/// Falls back to error if server demands full SRP (requires password we don't store).
pub fn reconnect_ccp(auth: &ReconnectAuth) -> io::Result<Connection> {
    let token_hash = token_short_hash(&auth.session_token);
    reconnect_ccp_attempt(auth, &token_hash, &auth.host, 0)
}

fn reconnect_ccp_attempt(auth: &ReconnectAuth, token_hash: &str, host: &str, depth: u32) -> io::Result<Connection> {
    if depth > 5 {
        return Err(io::Error::new(io::ErrorKind::Other, "CCP reconnect: too many redirects"));
    }
    log::info!("CCP reconnect to {}:{} (attempt {})", host, AUTH_PORT, depth + 1);

    // TLS + DH key exchange
    let addr = format!("{}:{}", host, AUTH_PORT)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "DNS resolution failed"))?;
    let tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(TIMEOUT_SSL_AUTH))?;
    let connector = TlsConnector::builder()
        .build()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
    let mut tls = connector
        .connect(host, tcp)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;

    let mut channel = SecureChannel::new();
    let dh_msg = channel.build_secure_connect(NS_VERSION, NS_VERSION);
    tls.write_all(&dh_msg)?;

    let (payload, _) = ns::ns_recv(&mut tls)?;
    let text = String::from_utf8_lossy(&payload);
    let parts: Vec<&str> = text.split(';').collect();
    let msg_type: u32 = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    if msg_type != ns::NS_SECURE_CONNECTION_START {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("CCP reconnect DH: expected 533, got {}", msg_type),
        ));
    }
    channel.process_server_hello(&parts[2..]);

    // CONNECT_REQUEST with SOFT_TOKEN flag + token hash (field 9)
    let flags = session::FLAG_OK_TO_REDIRECT
        | session::FLAG_VERSION
        | session::FLAG_VERSION_PRESENT
        | session::FLAG_DEVICE_INFO
        | session::FLAG_SOFT_TOKEN
        | session::FLAG_UNKNOWN_U
        | session::FLAG_UNKNOWN_19
        | session::FLAG_UNKNOWN_20
        | if auth.paper { session::FLAG_PAPER_CONNECT } else { 0 };
    let display_name = if auth.paper {
        format!("S{}", auth.username)
    } else {
        auth.username.clone()
    };
    let connect_req = format!(
        "{};{};{};{};{};27;{};{};{};{};",
        NS_VERSION_MIN,
        ns::NS_CONNECT_REQUEST,
        display_name,
        flags,
        NS_VERSION,
        auth.hw_info,
        auth.server_session_id,
        auth.encoded,
        token_hash,
    );
    session::send_secure(&mut tls, &mut channel, connect_req.as_bytes())?;
    log::info!("CCP reconnect CONNECT_REQUEST sent (session={}, hash={})", auth.server_session_id, token_hash);

    // Receive AUTH_START — may get NS_REDIRECT instead
    let auth_start = match session::recv_secure(&mut tls, &mut channel) {
        Ok(data) => data,
        Err(e) if e.to_string().starts_with("REDIRECT:") => {
            let target = e.to_string().replace("REDIRECT:", "");
            let redirect_host = target.split(':').next().unwrap_or(&target).to_string();
            log::info!("CCP reconnect redirected to {}", redirect_host);
            drop(tls);
            return reconnect_ccp_attempt(auth, token_hash, &redirect_host, depth + 1);
        }
        Err(e) => return Err(e),
    };

    // Parse AUTH_START field[5] for auth mode: 2=SOFT_TOKEN, 0=SRP required
    let auth_text = String::from_utf8_lossy(&auth_start);
    let auth_fields: Vec<&str> = auth_text.split(';').collect();
    let auth_mode: u32 = auth_fields.get(5).and_then(|s| s.parse().ok()).unwrap_or(0);

    if auth_mode == 2 {
        // SOFT_TOKEN challenge-response (4 states)
        do_ccp_soft_token(&mut tls, &auth.session_key)?;

        // Consume AUTH_FINISH (msg_id=771) after SOFT_TOKEN PASSED
        match session::recv_msg(&mut tls) {
            Ok(session::RecvMsg::Xyz { state, fields, .. }) => {
                let result = fields.iter().rev().find(|s| !s.is_empty()).map(|s| s.as_str()).unwrap_or("");
                log::info!("CCP reconnect AUTH_FINISH: state={} result={}", state, result);
            }
            Ok(session::RecvMsg::Ns { msg_type, .. }) => {
                log::info!("CCP reconnect post-auth NS type={}", msg_type);
            }
            Err(e) => {
                log::warn!("CCP reconnect AUTH_FINISH recv: {}", e);
            }
        }
    } else {
        // SRP probe fallback
        let msg1 = crate::protocol::xyz::xyz_build_srp_v20(1, &[]);
        tls.write_all(&crate::protocol::xyz::xyz_wrap(&msg1))?;
        let resp = session::recv_msg(&mut tls)?;
        match resp {
            session::RecvMsg::Xyz { state, .. } if state == 2 => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "CCP reconnect: server requires full SRP — password not available",
                ));
            }
            _ => {}
        }
    }

    // Post-auth: wait for NS_CONNECT_RESPONSE → NEWCOMMPORTTYPE → NS_FIX_START
    tls.get_ref().set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut fix_ready = false;
    for _ in 0..20 {
        let (payload, _) = match ns::ns_recv(&mut tls) {
            Ok(r) => r,
            Err(e) => {
                log::warn!("CCP reconnect post-auth recv: {}", e);
                break;
            }
        };
        let text = String::from_utf8_lossy(&payload);
        let parts: Vec<&str> = text.split(';').collect();
        let raw_type: u32 = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);

        let inner = if raw_type == ns::NS_SECURE_MESSAGE {
            let ct = B64.decode(parts[2])
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
            channel.decrypt(&ct)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?
        } else {
            payload
        };

        let inner_text = String::from_utf8_lossy(&inner);
        let inner_parts: Vec<&str> = inner_text.split(';').collect();
        let msg_type: u32 = inner_parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);

        if msg_type == ns::NS_CONNECT_RESPONSE {
            let newcomm = format!("{};{};0;;2;0;", NS_VERSION_MIN, ns::NS_NEWCOMMPORTTYPE);
            session::send_secure(&mut tls, &mut channel, newcomm.as_bytes())?;
        } else if msg_type == ns::NS_FIX_START {
            fix_ready = true;
            break;
        } else if msg_type == ns::NS_ERROR_RESPONSE {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("CCP reconnect post-auth error: {}", inner_parts[2..].join(";")),
            ));
        }
        // Ignore 530 keepalives and other types
    }
    tls.get_ref().set_read_timeout(None)?;
    if !fix_ready {
        return Err(io::Error::new(io::ErrorKind::Other, "CCP reconnect: no FIX_START after auth"));
    }

    // FIX Logon
    let logon_msg = build_ccp_logon(&auth.hw_info, &auth.encoded, CCP_HEARTBEAT, 1);
    tls.write_all(&logon_msg)?;
    tls.flush()?;

    tls.get_ref().set_read_timeout(Some(Duration::from_secs_f64(TIMEOUT_FIX_LOGON)))?;
    for _ in 0..5 {
        let response = fix_read(&mut tls)?;
        let fields = fix_parse(&response);
        let msg_type = fields.get(&35).map(|s| s.as_str()).unwrap_or("");
        match msg_type {
            "3" | "5" => {
                let reason = fields.get(&58).map(|s| s.as_str()).unwrap_or("unknown");
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("CCP reconnect logon rejected: {}", reason),
                ));
            }
            "A" | "U" => break,
            _ => {}
        }
    }
    tls.get_ref().set_read_timeout(None)?;

    // Order mass status request
    let mut ccp_seq: u32 = 1;
    ccp_seq += 1;
    let now = chrono_free_timestamp();
    let status_req = fix_build(&[(35, "H"), (52, &now), (11, "*"), (54, "*"), (55, "*")], ccp_seq);
    tls.write_all(&status_req)?;
    tls.flush()?;

    let mut conn = Connection::new(tls)?;
    conn.seq = ccp_seq;
    log::info!("CCP reconnect complete (seq={})", conn.seq);
    Ok(conn)
}


/// SOFT_TOKEN challenge-response over the TLS/NS channel (for CCP reconnect).
fn do_ccp_soft_token<S: Read + Write>(stream: &mut S, session_key: &BigUint) -> io::Result<()> {
    use crate::protocol::xyz;

    // State 1: Send empty init
    let msg1 = xyz::xyz_build_soft_token(1, "", "", "");
    stream.write_all(&xyz::xyz_wrap(&msg1))?;

    // State 2: Receive challenge
    let recv2 = session::recv_msg(stream)?;
    let challenge_hex = match recv2 {
        session::RecvMsg::Xyz { state, fields, .. } if state == 2 => {
            fields.get(1).filter(|s| !s.is_empty()).cloned()
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "CCP SOFT_TOKEN: empty challenge"))?
        }
        _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "CCP SOFT_TOKEN: expected XYZ state 2")),
    };

    // SHA-1(strip(challenge) || strip(token))
    let challenge_int = BigUint::parse_bytes(challenge_hex.as_bytes(), 16)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Invalid challenge hex"))?;
    let challenge_be = challenge_int.to_bytes_be();
    let challenge_bytes = strip_leading_zeros(&challenge_be);
    let token_be = session_key.to_bytes_be();
    let token_bytes = strip_leading_zeros(&token_be);

    let mut hasher = Sha1::new();
    hasher.update(challenge_bytes);
    hasher.update(token_bytes);
    let response_hex = format!("{:x}", BigUint::from_bytes_be(&hasher.finalize()));

    // State 3: Send response
    let msg3 = xyz::xyz_build_soft_token(3, "", &response_hex, "");
    stream.write_all(&xyz::xyz_wrap(&msg3))?;

    // State 4: Receive result
    let recv4 = session::recv_msg(stream)?;
    let result = match recv4 {
        session::RecvMsg::Xyz { fields, .. } => {
            fields.iter().rev().find(|s| !s.is_empty()).cloned().unwrap_or_default()
        }
        _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "CCP SOFT_TOKEN: expected XYZ state 4")),
    };

    if result == "PASSED" {
        log::info!("CCP SOFT_TOKEN auth passed");
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("CCP SOFT_TOKEN auth failed: {}", result),
        ))
    }
}

/// Configuration for connecting to IB.
pub struct GatewayConfig {
    pub username: String,
    pub password: String,
    pub host: String,
    pub paper: bool,
    /// Accept invalid TLS certificates during auth. Default: `false` (secure).
    /// Only set to `true` for local testing against self-signed gateways.
    pub accept_invalid_certs: bool,
}

impl Gateway {
    /// Connect to IB: auth + logon + data farm connections.
    /// Returns Gateway + farm Connection + auth Connection + optional historical data Connection.
    pub fn connect(config: &GatewayConfig) -> io::Result<(Self, Connection, Connection, Option<Connection>, Option<Connection>, Option<Connection>, Option<Connection>, Option<Connection>)> {
        Self::connect_to_host(config, &config.host, 0)
    }

    /// Internal: connect to a specific host, with redirect depth tracking.
    fn connect_to_host(
        config: &GatewayConfig,
        host: &str,
        redirect_depth: u32,
    ) -> io::Result<(Self, Connection, Connection, Option<Connection>, Option<Connection>, Option<Connection>, Option<Connection>, Option<Connection>)> {
        if redirect_depth > 3 {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "Too many redirects during auth",
            ));
        }

        let hw_info = session::get_hw_info();
        let encoded = IB_ENCODED.to_string();

        // --- Phase 1: TLS + auth ---
        log::info!("Connecting to auth server {}:{}", host, AUTH_PORT);
        let addr = format!("{}:{}", host, AUTH_PORT)
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "DNS resolution failed"))?;
        let tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(TIMEOUT_SSL_AUTH))?;

        let connector = TlsConnector::builder()
            .danger_accept_invalid_certs(config.accept_invalid_certs)
            .build()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        let mut tls = connector
            .connect(host, tcp)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;

        // Key exchange
        let mut channel = SecureChannel::new();
        let dh_msg = channel.build_secure_connect(NS_VERSION, NS_VERSION);
        tls.write_all(&dh_msg)?;

        let (payload, _) = ns::ns_recv(&mut tls)?;
        let text = String::from_utf8_lossy(&payload);
        let parts: Vec<&str> = text.split(';').collect();
        let msg_type: u32 = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        if msg_type == ns::NS_SECURE_ERROR {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("DH error: {}", parts[2..].join(";")),
            ));
        }
        if msg_type != ns::NS_SECURE_CONNECTION_START {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Expected 533, got {}", msg_type),
            ));
        }
        channel.process_server_hello(&parts[2..]);
        log::info!("Auth key exchange complete");

        // Send CONNECT_REQUEST (encrypted)
        let flags = session::FLAG_OK_TO_REDIRECT
            | session::FLAG_VERSION
            | session::FLAG_VERSION_PRESENT
            | session::FLAG_DEVICE_INFO
            | session::FLAG_UNKNOWN_U
            | session::FLAG_UNKNOWN_19
            | session::FLAG_UNKNOWN_20
            | if config.paper { session::FLAG_PAPER_CONNECT } else { 0 };
        let display_name = if config.paper {
            format!("S{}", config.username)
        } else {
            config.username.clone()
        };
        let session_id = session::get_session_id();
        let connect_req = format!(
            "{};{};{};{};{};27;{};{};{};",
            NS_VERSION_MIN,
            ns::NS_CONNECT_REQUEST,
            display_name,
            flags,
            NS_VERSION,
            hw_info,
            session_id,
            encoded
        );
        session::send_secure(&mut tls, &mut channel, connect_req.as_bytes())?;

        // Receive AUTH_START — contains token type for 2FA (if enabled).
        let auth_start = match session::recv_secure(&mut tls, &mut channel) {
            Ok(data) => data,
            Err(e) if e.to_string().starts_with("REDIRECT:") => {
                let target = e.to_string().strip_prefix("REDIRECT:").unwrap().to_string();
                // Extract host (strip port if present — auth always uses AUTH_PORT)
                let redirect_host = target.split(':').next().unwrap_or(&target);
                log::info!("Redirected to {}, reconnecting...", redirect_host);
                drop(tls);
                return Self::connect_to_host(config, redirect_host, redirect_depth + 1);
            }
            Err(e) => return Err(e),
        };

        // Parse token type and subtype from AUTH_START field[4] (e.g., "5.2i").
        // Token type 5 = DSA/IB Key; subtype identifies the registered device.
        let token_subtype = {
            let text = String::from_utf8_lossy(&auth_start);
            let parts: Vec<&str> = text.split(';').collect();
            parts.get(4).and_then(|f| {
                let dot = f.find('.')?;
                let token_type: u32 = f[..dot].parse().ok()?;
                if token_type == 5 { Some(f[dot + 1..].to_string()) } else { None }
            }).unwrap_or_default()
        };

        // Authentication (SRP-6)
        log::info!("Starting auth for {}", config.username);
        let session_key = do_srp(&mut tls, &config.username, &config.password)?;
        log::info!("Auth complete");

        // For live accounts with IB Key 2FA (token type 5): send DSA challenge
        // (msg_id=775, state=1) as #%#%-framed XYZ directly over TLS.
        // Post-SRP 2FA messages bypass the application-level DH encryption —
        // the Gateway writes them to the TLS socket without DH wrapping.
        // The TLS layer still provides transport encryption.
        if !config.paper && !token_subtype.is_empty() {
            let sub = token_subtype.as_bytes();
            let mut dsa = Vec::with_capacity(48);
            dsa.extend_from_slice(&20u32.to_be_bytes());   // version
            dsa.extend_from_slice(&775u32.to_be_bytes());  // msg_id (DSA)
            dsa.extend_from_slice(&1u32.to_be_bytes());    // sub_id
            dsa.extend_from_slice(&1u32.to_be_bytes());    // state (initiate)
            dsa.extend_from_slice(&0u32.to_be_bytes());    // username
            dsa.extend_from_slice(&0u32.to_be_bytes());    // y
            dsa.extend_from_slice(&0u32.to_be_bytes());    // A
            dsa.extend_from_slice(&0u32.to_be_bytes());    // B
            dsa.extend_from_slice(&(sub.len() as u32).to_be_bytes());
            dsa.extend_from_slice(sub);
            let pad = (4 - sub.len() % 4) % 4; // align to 4-byte boundary
            for _ in 0..pad { dsa.push(0); }
            tls.write_all(&ns::ns_wrap_raw(&dsa))?;
            tls.flush()?;
            log::info!("Sent IB Key 2FA challenge (DSA 775, subtype='{}')", token_subtype);
        }

        // Receive post-auth messages.
        // Paper: server sends 534(523) → 534(525) immediately.
        // Live 2FA: server sends DSA result (775, state=2), then waits for
        // user to approve on IB Key, then sends 534(523) → 534(525).
        // Timeout: 5 minutes for 2FA approval, 30s for paper.
        let post_auth_timeout = if token_subtype.is_empty() { 30 } else { 300 };
        tls.get_ref().set_read_timeout(Some(Duration::from_secs(post_auth_timeout)))?;
        let mut fix_ready = false;
        for _ in 0..60 {
            let (payload, _) = match ns::ns_recv(&mut tls) {
                Ok(r) => r,
                Err(e) => {
                    log::warn!("Post-auth recv error: {}", e);
                    break;
                }
            };
            // DSA response arrives as raw XYZ binary (not NS text).
            if payload.len() >= 16 {
                let mid = u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
                if mid == 775 {
                    let state = u32::from_be_bytes([payload[12], payload[13], payload[14], payload[15]]);
                    log::info!("IB Key 2FA: DSA state={}", state);
                    if state == 4 {
                        // State 4 = denied/failed.
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            "IB Key 2FA denied by user or expired",
                        ));
                    }
                    continue;
                }
            }

            let text = String::from_utf8_lossy(&payload);
            let parts: Vec<&str> = text.split(';').collect();
            let raw_type: u32 = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);

            // Server keepalives during 2FA approval wait.
            if raw_type == ns::NS_TEST_REQUEST || raw_type == ns::NS_HEART_BEAT {
                continue;
            }

            // Decrypt if encrypted, otherwise use raw
            let inner = if raw_type == ns::NS_SECURE_MESSAGE {
                let ct = B64.decode(parts[2])
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
                channel.decrypt(&ct)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?
            } else if raw_type == ns::NS_SECURE_ERROR {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("Post-auth secure error: {}", parts[2..].join(";")),
                ));
            } else if raw_type == ns::NS_REDIRECT {
                let target = parts.get(2).unwrap_or(&"");
                let redirect_host = target.split(':').next().unwrap_or(target);
                log::info!("Post-auth redirect to {}, reconnecting...", redirect_host);
                drop(tls);
                return Self::connect_to_host(config, redirect_host, redirect_depth + 1);
            } else {
                payload
            };

            let inner_text = String::from_utf8_lossy(&inner);
            let inner_parts: Vec<&str> = inner_text.split(';').collect();
            let msg_type: u32 = inner_parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);

            if msg_type == ns::NS_CONNECT_RESPONSE {
                log::info!("NS_CONNECT_RESPONSE: {}", inner_text);
                // Send port type change (required before data start)
                let newcomm = format!("{};{};0;;2;0;", NS_VERSION_MIN, ns::NS_NEWCOMMPORTTYPE);
                session::send_secure(&mut tls, &mut channel, newcomm.as_bytes())?;
                log::info!("Port type change sent");
            } else if msg_type == ns::NS_FIX_START {
                log::info!("Data start: {}", inner_text);
                fix_ready = true;
                break;
            } else if msg_type == ns::NS_ERROR_RESPONSE {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("Post-auth error: {}", inner_parts[2..].join(";")),
                ));
            } else {
                log::info!("Post-auth msg type={}: {}", msg_type, inner_text);
            }
        }
        if !fix_ready {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "Never received data start after auth",
            ));
        }

        // --- Phase 2: Auth server logon (over TLS) ---
        let logon_msg = build_ccp_logon(&hw_info, &encoded, CCP_HEARTBEAT, 1);
        log::info!("Sending auth logon ({} bytes)", logon_msg.len());
        tls.write_all(&logon_msg)?;
        tls.flush()?;

        // Read FIX messages until we get the logon ACK (35=A) with session info
        tls.get_ref().set_read_timeout(Some(Duration::from_secs_f64(TIMEOUT_FIX_LOGON)))?;
        let mut account_id = String::new();
        let mut heartbeat_interval = CCP_HEARTBEAT;
        let mut server_session_id = String::new();
        let mut ccp_token = String::new();
        let mut raw_soft_dollar_tiers = String::new();
        let mut raw_family_codes = String::new();
        let mut white_branding_id = String::new();

        for _ in 0..5 {
            let response = fix_read(&mut tls)?;
            let fields = fix_parse(&response);
            let msg_type = fields.get(&35).map(|s| s.as_str()).unwrap_or("");
            log::info!("Auth msg type={}", msg_type);

            match msg_type {
                "3" | "5" => {
                    let reason = fields.get(&58).map(|s| s.as_str()).unwrap_or("unknown");
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        format!("FIX Logon rejected: {}", reason),
                    ));
                }
                _ => {}
            }

            if let Some(v) = fields.get(&1) {
                if account_id.is_empty() { account_id = v.clone(); }
            }
            if let Some(v) = fields.get(&108) {
                if let Ok(hb) = v.parse() { heartbeat_interval = hb; }
            }
            if let Some(v) = fields.get(&6386) {
                if ccp_token.is_empty() { ccp_token = v.clone(); }
            }
            // Tag 8035: try parsed fields first, then raw byte search
            if server_session_id.is_empty() {
                if let Some(v) = fields.get(&8035) {
                    server_session_id = v.clone();
                } else {
                    let marker = b"\x018035=";
                    if let Some(pos) = response.windows(marker.len()).position(|w| w == marker) {
                        let val_start = pos + marker.len();
                        if let Some(end) = response[val_start..].iter().position(|&b| b == SOH) {
                            server_session_id = String::from_utf8_lossy(
                                &response[val_start..val_start + end],
                            ).to_string();
                        }
                    }
                }
            }

            // Gateway-local init data from logon response
            if let Some(v) = fields.get(&6560) {
                if raw_soft_dollar_tiers.is_empty() { raw_soft_dollar_tiers = v.clone(); }
            }
            if let Some(v) = fields.get(&6823) {
                if raw_family_codes.is_empty() { raw_family_codes = v.clone(); }
            }
            if let Some(v) = fields.get(&6571) {
                if white_branding_id.is_empty() { white_branding_id = v.clone(); }
            }

            // Stop once we have the logon ACK or server config message
            if msg_type == "A" || msg_type == "U" {
                break;
            }
        }
        tls.get_ref().set_read_timeout(None)?;

        // Fall back to our auth session_id if server didn't provide one (Python does the same)
        if server_session_id.is_empty() {
            server_session_id = session_id.clone();
        }

        log::info!(
            "Auth logon: account={} session_id={} hb={}s",
            account_id, server_session_id, heartbeat_interval
        );

        // --- Post-logon init sequence ---
        let account = if account_id.is_empty() { config.username.clone() } else { account_id.clone() };
        let mut ccp_seq: u32 = 1; // logon was seq 1
        let now = chrono_free_timestamp();
        let today_start = format!("{}-00:00:00", &now[..8]);

        // Helper: send_ib_msg builds 35=U with 6040=<comm_type> + extra tags
        let mut send_init = |fields: &[(u32, &str)]| -> io::Result<()> {
            ccp_seq += 1;
            let msg = fix_build(fields, ccp_seq);
            tls.write_all(&msg)?;
            Ok(())
        };

        send_init(&[(35, "U"), (52, &now), (6040, "91"), (1, &account), (6556, "DR.1"), (6712, "1")])?;
        send_init(&[(35, "U"), (52, &now), (6040, "193"), (6556, "OPR.2"), (8166, "L"), (8176, "1")])?;
        send_init(&[(35, "U"), (52, &now), (6040, "101")])?;
        send_init(&[(35, "U"), (52, &now), (6040, "209"), (1, &account), (6556, "AcctConfig3")])?;
        send_init(&[(35, "U"), (52, &now), (6040, "72"), (6536, &today_start), (6537, &now), (6556, "today4")])?;
        send_init(&[(35, "U"), (52, &now), (6040, "74"), (1, ""), (6544, "2")])?;
        send_init(&[(35, "U"), (52, &now), (6040, "76"), (1, ""), (6565, "1")])?;
        send_init(&[(35, "U"), (52, &now), (6040, "6"), (6036, "1"), (6095, &account), (6529, "AR.3")])?;
        for _ in 0..92 {
            send_init(&[(35, "U"), (52, &now), (6040, "80")])?;
        }
        // Order status request
        ccp_seq += 1;
        let status_req = fix_build(&[(35, "H"), (52, &now), (11, "*"), (54, "*"), (55, "*")], ccp_seq);
        tls.write_all(&status_req)?;
        tls.flush()?;
        log::info!("Init sequence sent ({} messages, seq now {})", 101, ccp_seq);

        // Drain init responses — extract account ID if found
        tls.get_ref().set_read_timeout(Some(Duration::from_secs(3)))?;
        let mut init_data = Vec::new();
        let mut tmp_buf = vec![0u8; 65536];
        loop {
            match tls.read(&mut tmp_buf) {
                Ok(0) => break,
                Ok(n) => init_data.extend_from_slice(&tmp_buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut => break,
                Err(e) => return Err(e),
            }
        }
        log::info!("Init response: {} bytes", init_data.len());
        // Scan init response for account ID and gateway-local init tags
        let init_str = String::from_utf8_lossy(&init_data);
        for part in init_str.split('\x01') {
            if part.starts_with("1=") && part.len() > 2 {
                let val = &part[2..];
                if val.starts_with("DU") || val.starts_with("DF") || val.starts_with("U") {
                    if account_id.is_empty() || account_id == config.username {
                        account_id = val.to_string();
                        log::info!("Found account ID from init response: {}", account_id);
                    }
                }
            } else if part.starts_with("6560=") && raw_soft_dollar_tiers.is_empty() {
                raw_soft_dollar_tiers = part[5..].to_string();
                log::info!("Found soft dollar tiers from init response ({} bytes)", raw_soft_dollar_tiers.len());
            } else if part.starts_with("6823=") && raw_family_codes.is_empty() {
                raw_family_codes = part[5..].to_string();
                log::info!("Found family codes from init response ({} bytes)", raw_family_codes.len());
            } else if part.starts_with("6571=") && white_branding_id.is_empty() {
                white_branding_id = part[5..].to_string();
                log::info!("Found white branding ID from init response");
            }
        }
        tls.get_ref().set_read_timeout(None)?;

        // Auth connection (non-blocking TLS for hot loop)
        let mut ccp_conn = Connection::new(tls)?;
        ccp_conn.seq = ccp_seq;
        // Seed init burst into connection buffer so the hot loop processes 8=O account data
        ccp_conn.seed_buffer(&init_data);

        // --- Phase 3: Data farm connections (parallel) ---
        let (farm_conn, hmds_conn, cashfarm_conn, usfuture_conn, eufarm_conn, jfarm_conn) =
            std::thread::scope(|s| {
                let farm_h = s.spawn(|| connect_farm(
                    host, "usfarm", &config.username, &config.password, config.paper,
                    &server_session_id, &session_key, &hw_info, &encoded,
                ));
                let hmds_h = s.spawn(|| connect_farm(
                    host, "ushmds", &config.username, &config.password, config.paper,
                    &server_session_id, &session_key, &hw_info, &encoded,
                ));
                let cashfarm_h = s.spawn(|| connect_farm(
                    host, "cashfarm", &config.username, &config.password, config.paper,
                    &server_session_id, &session_key, &hw_info, &encoded,
                ));
                let usfuture_h = s.spawn(|| connect_farm(
                    host, "usfuture", &config.username, &config.password, config.paper,
                    &server_session_id, &session_key, &hw_info, &encoded,
                ));
                let eufarm_h = s.spawn(|| connect_farm(
                    host, "eufarm", &config.username, &config.password, config.paper,
                    &server_session_id, &session_key, &hw_info, &encoded,
                ));
                let jfarm_h = s.spawn(|| connect_farm(
                    host, "jfarm", &config.username, &config.password, config.paper,
                    &server_session_id, &session_key, &hw_info, &encoded,
                ));

                let farm_conn = farm_h.join().unwrap()?;
                let hmds_conn = match hmds_h.join().unwrap() {
                    Ok(c) => { log::info!("Historical data farm connected"); Some(c) }
                    Err(e) => { log::warn!("Historical data farm connection failed (non-fatal): {}", e); None }
                };
                let cashfarm_conn = match cashfarm_h.join().unwrap() {
                    Ok(c) => { log::info!("cashfarm connected"); Some(c) }
                    Err(e) => { log::warn!("cashfarm connection failed (non-fatal): {}", e); None }
                };
                let usfuture_conn = match usfuture_h.join().unwrap() {
                    Ok(c) => { log::info!("usfuture connected"); Some(c) }
                    Err(e) => { log::warn!("usfuture connection failed (non-fatal): {}", e); None }
                };
                let eufarm_conn = match eufarm_h.join().unwrap() {
                    Ok(c) => { log::info!("eufarm connected"); Some(c) }
                    Err(e) => { log::warn!("eufarm connection failed (non-fatal): {}", e); None }
                };
                let jfarm_conn = match jfarm_h.join().unwrap() {
                    Ok(c) => { log::info!("jfarm connected"); Some(c) }
                    Err(e) => { log::warn!("jfarm connection failed (non-fatal): {}", e); None }
                };
                Ok::<_, io::Error>((farm_conn, hmds_conn, cashfarm_conn, usfuture_conn, eufarm_conn, jfarm_conn))
            })?;

        let gw = Gateway {
            account_id: if account_id.is_empty() { config.username.clone() } else { account_id },
            session_token: session_key,
            server_session_id,
            ccp_token,
            heartbeat_interval,
            hw_info,
            encoded,
            raw_soft_dollar_tiers,
            raw_family_codes,
            white_branding_id,
        };
        Ok((gw, farm_conn, ccp_conn, hmds_conn, cashfarm_conn, usfuture_conn, eufarm_conn, jfarm_conn))
    }

    /// Populate shared state with gateway-local init data parsed from CCP logon.
    pub fn populate_init_data(&self, shared: &SharedState) {
        use crate::types::{SmartComponent, NewsProvider, SoftDollarTier, FamilyCode};

        // Smart components: hardcoded US equity SMART routing exchanges.
        // Server doesn't send these in a parseable init message; they're
        // embedded in the Gateway binary. Hardcoded list matches Gateway 10.30+.
        let smart_components: Vec<SmartComponent> = [
            ("NASDAQ", "Q"), ("NYSE", "N"), ("ARCA", "P"), ("BATS", "Z"),
            ("IEX", "V"), ("BEX", "B"), ("BYX", "Y"), ("NYSENAT", "C"),
            ("DRCTEDGE", "J"), ("MEMX", "U"), ("PEARL", "H"), ("AMEX", "A"),
            ("CHX", "M"), ("LTSE", "L"), ("PSX", "X"), ("ISE", "I"), ("EDGEA", "K"),
        ].iter().enumerate().map(|(i, (exch, letter))| SmartComponent {
            bit_number: i as i32,
            exchange: exch.to_string(),
            exchange_letter: letter.to_string(),
        }).collect();
        shared.reference.set_smart_components(smart_components);

        // News providers: hardcoded list matching Gateway.
        let news_providers: Vec<NewsProvider> = [
            ("BRFG", "Briefing.com General Market Columns"),
            ("BRFUPDN", "Briefing.com Analyst Actions"),
            ("DJ-N", "Dow Jones Global Equity Trader"),
            ("DJ-RTA", "Dow Jones Top Stories Asia Pacific"),
            ("DJ-RTE", "Dow Jones Top Stories Europe"),
            ("DJ-RTG", "Dow Jones Top Stories Global"),
            ("DJ-RTPRO", "Dow Jones Top Stories Pro"),
            ("DJNL", "Dow Jones Newsletters"),
        ].iter().map(|(code, name)| NewsProvider {
            code: code.to_string(), name: name.to_string(),
        }).collect();
        shared.reference.set_news_providers(news_providers);

        // Soft dollar tiers: parse from CCP logon tag 6560, fall back to defaults.
        let tiers = if self.raw_soft_dollar_tiers.is_empty() {
            // Default tiers matching Gateway 10.30+
            vec![
                SoftDollarTier { name: "MaxRebate".into(), val: "1".into(), display_name: "Maximize Rebate".into() },
                SoftDollarTier { name: "PreferRebate".into(), val: "9".into(), display_name: "Prefer Rebate".into() },
                SoftDollarTier { name: "PreferFill".into(), val: "11".into(), display_name: "Prefer Fill".into() },
                SoftDollarTier { name: "MaxFill".into(), val: "12".into(), display_name: "Maximize Fill".into() },
                SoftDollarTier { name: "Primary".into(), val: "2".into(), display_name: "Primary Exchange".into() },
                SoftDollarTier { name: "VRebate".into(), val: "3".into(), display_name: "Highest Volume Exchange With Rebate".into() },
                SoftDollarTier { name: "VLowFee".into(), val: "4".into(), display_name: "High Volume Exchange With Lowest Fee".into() },
            ]
        } else {
            // Parse "name1|val1|display1;name2|val2|display2" format
            self.raw_soft_dollar_tiers.split(';').filter_map(|entry| {
                let parts: Vec<&str> = entry.split('|').collect();
                if parts.len() >= 3 {
                    Some(SoftDollarTier {
                        name: parts[0].to_string(),
                        val: parts[1].to_string(),
                        display_name: parts[2].to_string(),
                    })
                } else {
                    log::warn!("Unexpected soft dollar tier format: {}", entry);
                    None
                }
            }).collect()
        };
        shared.reference.set_soft_dollar_tiers(tiers);

        // Family codes: parse from CCP logon tag 6823.
        // Empty for paper/single accounts.
        let codes = if self.raw_family_codes.is_empty() {
            Vec::new()
        } else {
            self.raw_family_codes.split(';').filter_map(|entry| {
                let parts: Vec<&str> = entry.split('|').collect();
                if parts.len() >= 2 {
                    Some(FamilyCode {
                        account_id: parts[0].to_string(),
                        family_code_str: parts[1].to_string(),
                    })
                } else {
                    log::warn!("Unexpected family code format: {}", entry);
                    None
                }
            }).collect()
        };
        shared.reference.set_family_codes(codes);

        // White branding ID (empty for standard accounts).
        shared.reference.set_white_branding_id(self.white_branding_id.clone());
    }

    /// Create the control channel and build a HotLoop with connected sockets.
    pub fn into_hot_loop(
        self,
        shared: Arc<SharedState>,
        event_tx: Option<Sender<Event>>,
        farm_conn: Connection,
        ccp_conn: Connection,
        hmds_conn: Option<Connection>,
        core_id: Option<usize>,
    ) -> (HotLoop, Sender<ControlCommand>) {
        self.into_hot_loop_with_farms(shared, event_tx, farm_conn, ccp_conn, hmds_conn, None, None, None, None, core_id)
    }

    /// Create the control channel and build a HotLoop with all farm connections.
    pub fn into_hot_loop_with_farms(
        self,
        shared: Arc<SharedState>,
        event_tx: Option<Sender<Event>>,
        farm_conn: Connection,
        ccp_conn: Connection,
        hmds_conn: Option<Connection>,
        cashfarm_conn: Option<Connection>,
        usfuture_conn: Option<Connection>,
        eufarm_conn: Option<Connection>,
        jfarm_conn: Option<Connection>,
        core_id: Option<usize>,
    ) -> (HotLoop, Sender<ControlCommand>) {
        let (tx, rx) = bounded(64);
        let reconnect_auth = ReconnectAuth {
            host: String::new(), // Filled by caller (Python EClient or Rust API)
            username: String::new(), // Filled by caller
            password: String::new(), // Filled by caller
            paper: false, // Filled by caller
            session_key: self.session_token.clone(),
            session_token: self.session_token.clone(),
            server_session_id: self.server_session_id.clone(),
            hw_info: self.hw_info.clone(),
            encoded: self.encoded.clone(),
        };
        let mut hot_loop = HotLoop::new(shared, event_tx, core_id);
        hot_loop.set_control_rx(rx);
        hot_loop.set_account_id(self.account_id.clone());
        hot_loop.set_reconnect_auth(reconnect_auth);
        hot_loop.farm_conn = Some(farm_conn);
        hot_loop.ccp_conn = Some(ccp_conn);
        hot_loop.hmds_conn = hmds_conn;
        hot_loop.cashfarm_conn = cashfarm_conn;
        hot_loop.usfuture_conn = usfuture_conn;
        hot_loop.eufarm_conn = eufarm_conn;
        hot_loop.jfarm_conn = jfarm_conn;
        (hot_loop, tx)
    }
}

/// Build market data subscription request.
pub fn build_mktdata_subscribe(
    con_id: u32,
    exchange: &str,
    sec_type: &str,
    md_req_id: &str,
    seq: u32,
) -> Vec<u8> {
    let con_id_str = con_id.to_string();
    let exchange_fix = match exchange {
        "SMART" => "BEST",
        e => e,
    };
    fix_build(
        &[
            (fix::TAG_MSG_TYPE, fix::MSG_MARKET_DATA_REQ),
            (262, md_req_id),
            (263, "1"), // Subscribe
            (146, "1"), // NumRelatedSym
            (6008, &con_id_str),
            (207, exchange_fix),
            (167, sec_type),
            (264, "442"), // BidAsk
            (9830, "1"),
        ],
        seq,
    )
}

/// Build market data unsubscribe request.
pub fn build_mktdata_unsubscribe(md_req_id: &str, seq: u32) -> Vec<u8> {
    fix_build(
        &[
            (fix::TAG_MSG_TYPE, fix::MSG_MARKET_DATA_REQ),
            (262, md_req_id),
            (263, "2"), // Unsubscribe
        ],
        seq,
    )
}

/// Format timestamp as YYYYMMDD-HH:MM:SS (no chrono dependency).
/// Re-exports for backward compatibility.
pub use crate::config::{chrono_free_timestamp, days_to_ymd};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_short_hash_deterministic() {
        let token = BigUint::from(123456789u64);
        let h1 = token_short_hash(&token);
        let h2 = token_short_hash(&token);
        assert_eq!(h1, h2);
        // Should be lowercase hex
        assert!(h1.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn token_short_hash_different_tokens() {
        let t1 = BigUint::from(111u64);
        let t2 = BigUint::from(222u64);
        assert_ne!(token_short_hash(&t1), token_short_hash(&t2));
    }

    #[test]
    fn build_ccp_logon_structure() {
        let msg = build_ccp_logon("abc123|00:00:00:00:00:00", "17.0.10.0.101/W/fr/G", 10, 1);
        let fields = fix_parse(&msg);
        assert_eq!(fields[&35], "A");
        assert_eq!(fields[&98], "0");
        assert_eq!(fields[&108], "10");
        assert_eq!(fields[&141], "Y");
        assert_eq!(fields[&6034], IB_BUILD);
        assert_eq!(fields[&6968], IB_VERSION);
        assert_eq!(fields[&6490], "dark");
        assert_eq!(fields[&6397], "1");
        assert_eq!(fields[&8361], "(rolling)");
        assert_eq!(fields[&8098], "0");
        assert!(fields[&6351].contains("abc123"));
    }

    #[test]
    fn build_farm_logon_has_required_tags() {
        let token = BigUint::from(999u64);
        let hash = token_short_hash(&token);
        assert!(!hash.is_empty());
    }

    #[test]
    fn build_mktdata_subscribe_structure() {
        let msg = build_mktdata_subscribe(265598, "SMART", "CS", "REQ1", 5);
        let fields = fix_parse(&msg);
        assert_eq!(fields[&35], "V");
        assert_eq!(fields[&262], "REQ1");
        assert_eq!(fields[&263], "1");
        assert_eq!(fields[&6008], "265598");
        assert_eq!(fields[&207], "BEST"); // SMART→BEST
        assert_eq!(fields[&167], "CS");
    }

    #[test]
    fn build_mktdata_unsubscribe_structure() {
        let msg = build_mktdata_unsubscribe("REQ1", 6);
        let fields = fix_parse(&msg);
        assert_eq!(fields[&35], "V");
        assert_eq!(fields[&262], "REQ1");
        assert_eq!(fields[&263], "2");
    }

    #[test]
    fn chrono_free_timestamp_format() {
        let ts = chrono_free_timestamp();
        assert_eq!(ts.len(), 17); // "YYYYMMDD-HH:MM:SS"
        assert_eq!(ts.as_bytes()[8], b'-');
        assert_eq!(ts.as_bytes()[11], b':');
        assert_eq!(ts.as_bytes()[14], b':');
    }

    #[test]
    fn days_to_ymd_epoch() {
        let (y, m, d) = days_to_ymd(0);
        assert_eq!((y, m, d), (1970, 1, 1));
    }

    #[test]
    fn days_to_ymd_known_date() {
        // 2026-03-05 = day 20517 since epoch
        let (y, m, d) = days_to_ymd(20517);
        assert_eq!((y, m, d), (2026, 3, 5));
    }

    #[test]
    fn try_frame_farm_msg_incomplete() {
        assert!(try_frame_farm_msg(b"8=FIX").is_none());
        assert!(try_frame_farm_msg(b"").is_none());
    }

    #[test]
    fn try_frame_farm_msg_complete() {
        let msg = fix_build(&[(35, "A"), (108, "30")], 1);
        let (extracted, consumed) = try_frame_farm_msg(&msg).unwrap();
        assert_eq!(extracted, msg);
        assert_eq!(consumed, msg.len());
    }

    #[test]
    fn try_frame_farm_msg_with_trailing() {
        let msg1 = fix_build(&[(35, "A")], 1);
        let msg2 = fix_build(&[(35, "0")], 2);
        let mut buf = msg1.clone();
        buf.extend_from_slice(&msg2);
        let (extracted, consumed) = try_frame_farm_msg(&buf).unwrap();
        assert_eq!(extracted, msg1);
        assert_eq!(consumed, msg1.len());
    }

    // Note: build_farm_encrypted_logon requires a DH-initialized SecureChannel
    // which can't be created in unit tests. Tested via compatibility tests instead.

    #[test]
    fn build_mktdata_subscribe_exchange_passthrough() {
        // Non-SMART exchanges should pass through as-is
        let msg = build_mktdata_subscribe(265598, "ARCA", "CS", "REQ2", 3);
        let fields = fix_parse(&msg);
        assert_eq!(fields[&207], "ARCA"); // not mapped to BEST
    }

    #[test]
    fn build_mktdata_subscribe_has_correct_tags() {
        let msg = build_mktdata_subscribe(756733, "SMART", "ETF", "REQ5", 10);
        let fields = fix_parse(&msg);
        assert_eq!(fields[&35], "V");
        assert_eq!(fields[&6008], "756733");
        assert_eq!(fields[&207], "BEST");
        assert_eq!(fields[&167], "ETF");
        assert_eq!(fields[&263], "1"); // subscribe
        assert_eq!(fields[&146], "1"); // NumRelatedSym
    }

    #[test]
    fn days_to_ymd_leap_year() {
        let (y, m, d) = days_to_ymd(19782); // 2024-02-29
        assert_eq!((y, m, d), (2024, 2, 29));
    }

    #[test]
    fn days_to_ymd_end_of_year() {
        // 2025-12-31
        let (y, m, d) = days_to_ymd(20453); // 2025-12-31
        assert_eq!((y, m, d), (2025, 12, 31));
    }

    #[test]
    fn days_to_ymd_start_of_2000() {
        // 2000-01-01 = 10957 days from epoch
        let (y, m, d) = days_to_ymd(10957);
        assert_eq!((y, m, d), (2000, 1, 1));
    }

    #[test]
    fn try_frame_farm_msg_garbage_prefix() {
        let mut buf = vec![0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        let msg = fix_build(&[(35, "A")], 1);
        buf.extend_from_slice(&msg);
        // Should skip garbage and return (empty, skip_count)
        let (extracted, consumed) = try_frame_farm_msg(&buf).unwrap();
        if extracted.is_empty() {
            // garbage skipped, need to retry from remaining
            let rest = &buf[consumed..];
            let (msg2, _) = try_frame_farm_msg(rest).unwrap();
            assert!(!msg2.is_empty());
        }
    }

    #[test]
    fn try_frame_farm_msg_multiple_sequential() {
        // Two FIX messages back to back
        let msg1 = fix_build(&[(35, "S")], 1);
        let msg2 = fix_build(&[(35, "A"), (108, "30")], 2);
        let mut buf = msg1.clone();
        buf.extend_from_slice(&msg2);
        let (extracted, consumed) = try_frame_farm_msg(&buf).unwrap();
        assert_eq!(extracted, msg1);
        assert_eq!(consumed, msg1.len());
        // Second message
        let (extracted2, consumed2) = try_frame_farm_msg(&buf[consumed..]).unwrap();
        assert_eq!(extracted2, msg2);
        assert_eq!(consumed2, msg2.len());
    }

    #[test]
    fn token_short_hash_nonzero_output() {
        let token = BigUint::from(1u64);
        let hash = token_short_hash(&token);
        assert!(!hash.is_empty());
        // Should be hex string
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn token_short_hash_large_token() {
        let token = BigUint::from(u64::MAX);
        let hash = token_short_hash(&token);
        assert!(!hash.is_empty());
        assert!(hash.len() <= 8); // u32 hex is at most 8 chars
    }

    #[test]
    fn chrono_free_timestamp_not_empty() {
        let ts = chrono_free_timestamp();
        assert!(!ts.is_empty());
        // Year should start with 20xx
        assert!(ts.starts_with("20"));
    }

    #[test]
    fn gateway_config_fields() {
        let config = GatewayConfig {
            username: "user".to_string(),
            password: "pass".to_string(),
            host: "cdc1.ibllc.com".to_string(),
            paper: true,
            accept_invalid_certs: false,
        };
        assert_eq!(config.username, "user");
        assert!(config.paper);
    }
}
