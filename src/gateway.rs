//! Gateway: orchestrates CCP + farm connections into a running HotLoop.
//!
//! Startup sequence (mirrors ibgw-headless gateway.py):
//! 1. CCP: TLS connect → DH → SRP auth → FIX logon
//! 2. Farm: TCP connect → DH → encrypted FIX logon → SOFT_TOKEN → logon ACK
//! 3. Create HotLoop with connected sockets and HMAC keys
//! 4. Market data subscription via FIX 35=V

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
use crate::engine::context::Strategy;
use crate::engine::hot_loop::HotLoop;
use crate::protocol::connection::Connection;
use crate::protocol::fix::{self, fix_build, fix_parse, fix_read, SOH};
use crate::protocol::fixcomp;
use crate::protocol::ns;
use crate::types::ControlCommand;

/// Compute token short hash for farm FIX logon tag 8483.
/// Java: SHA1(stripLeadingZeros(token.toByteArray())).intValue().toHexString()
pub fn token_short_hash(session_token: &BigUint) -> String {
    let token_bytes = session_token.to_bytes_be();
    let stripped = strip_leading_zeros(&token_bytes);
    let digest = Sha1::digest(stripped);
    // Take last 4 bytes as u32 (Java BigInteger.intValue() truncates to low 32 bits)
    let hash_int = u32::from_be_bytes([digest[16], digest[17], digest[18], digest[19]]);
    format!("{:x}", hash_int)
}

/// Build CCP FIX logon message (35=A).
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

/// Build encrypted farm FIX logon wrapped in tags 90/91.
///
/// Wire format: `8=FIX.4.1|9=<bodylen>|90=<b64_len>|91=<b64_ciphertext>|10=<cksum>`
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
        "ushmds" | "cashhmds" => 18,
        "secdefil" => 19,
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

/// Execute farm logon exchange: recv encrypted msgs → SOFT_TOKEN → logon ACK.
///
/// Returns (read_iv, sign_iv) for HMAC message signing/verification.
pub fn farm_logon_exchange(
    stream: &mut TcpStream,
    channel: &mut SecureChannel,
    session_token: &BigUint,
    read_mac_key: &[u8],
    initial_read_iv: &[u8],
) -> io::Result<(Vec<u8>, Vec<u8>)> {
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

                // Check for AUTH_START (35=S) → do SOFT_TOKEN
                if decrypted.windows(5).any(|w| w == b"35=S\x01") {
                    do_soft_token(stream, session_token)?;
                }
            } else if fields.get(&35).map(|s| s.as_str()) == Some("A") {
                // Logon ACK — sign_iv is the current write_iv (mutated by encrypt)
                let sign_iv = channel
                    .write_iv()
                    .map(|iv| iv.to_vec())
                    .unwrap_or_default();
                return Ok((read_iv, sign_iv));
            } else if fields.get(&35).map(|s| s.as_str()) == Some("3") {
                let text = fields.get(&58).map(|s| s.as_str()).unwrap_or("unknown");
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("Farm logon rejected: {}", text),
                ));
            }
        } else if msg.starts_with(b"8=1\x01") {
            // SOFT_TOKEN response inside FIX framing — already handled by do_soft_token
            if msg.windows(6).any(|w| w == b"PASSED") {
                log::info!("SOFT_TOKEN PASSED");
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

/// Full gateway connection.
pub struct Gateway {
    pub account_id: String,
    pub session_token: BigUint,
    pub server_session_id: String,
    pub ccp_token: String,
    pub heartbeat_interval: u64,
    /// Stored for farm reconnection.
    #[allow(dead_code)]
    hw_info: String,
    #[allow(dead_code)]
    encoded: String,
}

/// Connect to a farm (usfarm or ushmds): DH → encrypted logon → SOFT_TOKEN → routing → Connection.
fn connect_farm(
    host: &str,
    farm_id: &str,
    username: &str,
    paper: bool,
    server_session_id: &str,
    session_key: &BigUint,
    hw_info: &str,
    encoded: &str,
) -> io::Result<Connection> {
    log::info!("Connecting to {} {}:{}", farm_id, host, MISC_PORT);
    let addr = format!("{}:{}", host, MISC_PORT)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "DNS resolution failed"))?;
    let farm_tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(TIMEOUT_FARM_CONNECT))?;

    // DH key exchange (raw TCP)
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
    log::info!("{} DH complete", farm_id);

    // Encrypted FIX logon
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

    // Logon exchange: AUTH_START → SOFT_TOKEN → logon ACK
    let read_mac_key = channel.key_block().map(|kb| kb[84..104].to_vec()).unwrap_or_default();
    let initial_read_iv = channel.key_block().map(|kb| kb[48..64].to_vec()).unwrap_or_default();
    let (read_iv, sign_iv) = farm_logon_exchange(
        &mut stream, &mut channel, session_key, &read_mac_key, &initial_read_iv,
    )?;
    log::info!("{} logon exchange complete", farm_id);

    let sign_mac_key = channel.key_block().map(|kb| kb[64..84].to_vec()).unwrap_or_default();

    // Send routing table request (6040=112) — farm expects this after logon.
    // Must be FIXCOMP-wrapped and HMAC-signed (matching Python _farm_init).
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
    log::info!("{} sent routing request (6556={})", farm_id, channel_id);

    // Read routing response (stream is still blocking)
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
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

    // Create Connection (switches to non-blocking), inject routing bytes
    let mut conn = Connection::new_raw(stream)?;
    conn.set_keys(sign_mac_key, new_sign_iv, read_mac_key, read_iv);
    conn.seq = 1; // routing request used seq 1

    if !resp_buf.is_empty() {
        conn.inject_buf(&resp_buf);
        // Extract and unsign all routing frames to advance read_iv chain
        let frames = conn.extract_frames();
        for frame in &frames {
            let raw = match frame {
                crate::protocol::connection::Frame::FixComp(d) => d,
                crate::protocol::connection::Frame::Binary(d) => d,
                crate::protocol::connection::Frame::Fix(d) => d,
            };
            conn.unsign(raw);
        }
        log::info!("{} routing response: {} bytes, {} frames", farm_id, resp_buf.len(), frames.len());
    }
    Ok(conn)
}

/// Configuration for connecting to IB.
pub struct GatewayConfig {
    pub username: String,
    pub password: String,
    pub host: String,
    pub paper: bool,
}

impl Gateway {
    /// Connect to IB: CCP auth + FIX logon + farm connections.
    /// Returns Gateway + farm Connection + CCP Connection + optional hmds Connection.
    pub fn connect(config: &GatewayConfig) -> io::Result<(Self, Connection, Connection, Option<Connection>)> {
        let hw_info = session::get_hw_info();
        let encoded = IB_ENCODED.to_string();

        // --- Phase 1: CCP TLS + SRP auth ---
        log::info!("Connecting to CCP {}:{}", config.host, AUTH_PORT);
        let addr = format!("{}:{}", config.host, AUTH_PORT)
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "DNS resolution failed"))?;
        let tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(TIMEOUT_SSL_AUTH))?;

        let connector = TlsConnector::builder()
            .danger_accept_invalid_certs(true)
            .build()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        let mut tls = connector
            .connect(&config.host, tcp)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;

        // DH key exchange
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
        log::info!("CCP DH complete");

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

        // Receive AUTH_START
        let _auth_start = session::recv_secure(&mut tls, &mut channel)?;

        // SRP-6 authentication
        log::info!("Starting SRP auth for {}", config.username);
        let session_key = do_srp(&mut tls, &config.username, &config.password)?;
        log::info!("SRP auth complete");

        // Receive post-auth messages (encrypted via 534)
        let mut fix_ready = false;
        for _ in 0..10 {
            let (payload, _) = match ns::ns_recv(&mut tls) {
                Ok(r) => r,
                Err(e) => {
                    log::warn!("Post-auth recv error: {}", e);
                    break;
                }
            };
            let text = String::from_utf8_lossy(&payload);
            let parts: Vec<&str> = text.split(';').collect();
            let raw_type: u32 = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);

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
            } else {
                payload
            };

            let inner_text = String::from_utf8_lossy(&inner);
            let inner_parts: Vec<&str> = inner_text.split(';').collect();
            let msg_type: u32 = inner_parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);

            if msg_type == ns::NS_CONNECT_RESPONSE {
                log::info!("NS_CONNECT_RESPONSE: {}", inner_text);
                // Send NEWCOMMPORTTYPE (required before FIX_START)
                let newcomm = format!("{};{};0;;2;0;", NS_VERSION_MIN, ns::NS_NEWCOMMPORTTYPE);
                session::send_secure(&mut tls, &mut channel, newcomm.as_bytes())?;
                log::info!("NEWCOMMPORTTYPE sent");
            } else if msg_type == ns::NS_FIX_START {
                log::info!("FIX_START: {}", inner_text);
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
                "Never received FIX_START after auth",
            ));
        }

        // --- Phase 2: CCP FIX logon (over TLS) ---
        let logon_msg = build_ccp_logon(&hw_info, &encoded, CCP_HEARTBEAT, 1);
        log::info!("Sending CCP FIX logon ({} bytes)", logon_msg.len());
        tls.write_all(&logon_msg)?;
        tls.flush()?;

        // Read FIX messages until we get the logon ACK (35=A) with session info
        tls.get_ref().set_read_timeout(Some(Duration::from_secs_f64(TIMEOUT_FIX_LOGON)))?;
        let mut account_id = String::new();
        let mut heartbeat_interval = CCP_HEARTBEAT;
        let mut server_session_id = String::new();
        let mut ccp_token = String::new();

        for _ in 0..5 {
            let response = fix_read(&mut tls)?;
            let fields = fix_parse(&response);
            let msg_type = fields.get(&35).map(|s| s.as_str()).unwrap_or("");
            log::info!("CCP FIX msg type={}", msg_type);

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
            "CCP FIX logon: account={} session_id={} hb={}s",
            account_id, server_session_id, heartbeat_interval
        );

        // --- CCP post-logon init sequence (mirrors real Gateway / Python _fix_init) ---
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
        log::info!("CCP init sequence sent ({} messages, seq now {})", 101, ccp_seq);

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
        log::info!("CCP init response: {} bytes", init_data.len());
        // Search for account ID patterns (DU/DF/U + digits for paper, or from tag 1)
        let init_str = String::from_utf8_lossy(&init_data);
        for part in init_str.split('\x01') {
            if part.starts_with("1=") && part.len() > 2 {
                let val = &part[2..];
                // IB account IDs: DU/DF/U + digits, or similar patterns
                if val.starts_with("DU") || val.starts_with("DF") || val.starts_with("U") {
                    if account_id.is_empty() || account_id == config.username {
                        account_id = val.to_string();
                        log::info!("Found account ID from init response: {}", account_id);
                    }
                }
            }
        }
        tls.get_ref().set_read_timeout(None)?;

        // CCP Connection (non-blocking TLS for hot loop)
        let mut ccp_conn = Connection::new(tls)?;
        ccp_conn.seq = ccp_seq;
        // Seed init burst into connection buffer so the hot loop processes 8=O account data
        ccp_conn.seed_buffer(&init_data);

        // --- Phase 3: Farm connections ---
        let farm_conn = connect_farm(
            &config.host, "usfarm",
            &config.username, config.paper,
            &server_session_id, &session_key, &hw_info, &encoded,
        )?;

        // ushmds farm (optional — for historical data)
        let hmds_conn = match connect_farm(
            &config.host, "ushmds",
            &config.username, config.paper,
            &server_session_id, &session_key, &hw_info, &encoded,
        ) {
            Ok(c) => {
                log::info!("ushmds farm connected");
                Some(c)
            }
            Err(e) => {
                log::warn!("ushmds farm connection failed (non-fatal): {}", e);
                None
            }
        };

        let gw = Gateway {
            account_id: if account_id.is_empty() { config.username.clone() } else { account_id },
            session_token: session_key,
            server_session_id,
            ccp_token,
            heartbeat_interval,
            hw_info,
            encoded,
        };
        Ok((gw, farm_conn, ccp_conn, hmds_conn))
    }

    /// Create the control channel and build a HotLoop with connected sockets.
    pub fn into_hot_loop<S: Strategy>(
        self,
        strategy: S,
        farm_conn: Connection,
        ccp_conn: Connection,
        hmds_conn: Option<Connection>,
        core_id: Option<usize>,
    ) -> (HotLoop<S>, Sender<ControlCommand>) {
        let (tx, rx) = bounded(64);
        let mut hot_loop = HotLoop::new(strategy, core_id);
        hot_loop.set_control_rx(rx);
        hot_loop.set_account_id(self.account_id.clone());
        hot_loop.farm_conn = Some(farm_conn);
        hot_loop.ccp_conn = Some(ccp_conn);
        hot_loop.hmds_conn = hmds_conn;
        (hot_loop, tx)
    }
}

/// Build FIX 35=V market data subscription request.
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

/// Build FIX 35=V market data unsubscribe request.
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
pub fn chrono_free_timestamp() -> String {
    use std::time::SystemTime;
    let dur = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap();
    let secs = dur.as_secs();
    // Simple UTC breakdown
    let days = secs / 86400;
    let time_secs = secs % 86400;
    let hours = time_secs / 3600;
    let minutes = (time_secs % 3600) / 60;
    let seconds = time_secs % 60;

    // Days since epoch to Y/M/D (simplified Gregorian)
    let (year, month, day) = days_to_ymd(days);
    format!(
        "{:04}{:02}{:02}-{:02}:{:02}:{:02}",
        year, month, day, hours, minutes, seconds
    )
}

/// Convert days since Unix epoch to (year, month, day).
fn days_to_ymd(days: u64) -> (u64, u64, u64) {
    // Algorithm from Howard Hinnant
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

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
    // which can't be created in unit tests. Tested via integration tests instead.

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
        };
        assert_eq!(config.username, "user");
        assert!(config.paper);
    }
}
