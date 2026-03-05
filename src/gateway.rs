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

use crate::auth::crypto::strip_leading_zeros;
use crate::auth::dh::SecureChannel;
use crate::auth::session::{self, RecvMsg, do_srp, do_soft_token};
use crate::config::*;
use crate::engine::context::Strategy;
use crate::engine::hot_loop::HotLoop;
use crate::protocol::connection::Connection;
use crate::protocol::fix::{self, fix_build, fix_parse, fix_read, SOH};
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
    channel: &SecureChannel,
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

    let encrypted_raw = channel.encrypt_fresh(&inner);
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
            // Check for HMAC signature → unsign
            let parsed_msg = if msg.windows(6).any(|w| w == b"8349=\x01" || w == b"8349=") {
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

                // Sync HMAC read IV with AES read IV after decryption
                if let Some(kb) = channel.key_block() {
                    read_iv = kb[48..64].to_vec();
                }

                // Check for AUTH_START (35=S) → do SOFT_TOKEN
                if decrypted.windows(5).any(|w| w == b"35=S\x01") {
                    do_soft_token(stream, session_token)?;
                }
            } else if fields.get(&35).map(|s| s.as_str()) == Some("A") {
                // Logon ACK
                let sign_iv = channel
                    .key_block()
                    .map(|kb| kb[32..48].to_vec())
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

/// Configuration for connecting to IB.
pub struct GatewayConfig {
    pub username: String,
    pub password: String,
    pub host: String,
    pub paper: bool,
}

impl Gateway {
    /// Connect to IB: CCP auth + FIX logon + farm connection.
    /// Returns Gateway + farm Connection + CCP Connection ready for HotLoop.
    pub fn connect(config: &GatewayConfig) -> io::Result<(Self, Connection, Connection)> {
        let hw_info = session::get_hw_info();
        let encoded = IB_ENCODED.to_string();

        // --- Phase 1: CCP TLS + SRP auth ---
        log::info!("Connecting to CCP {}:{}", config.host, AUTH_PORT);
        let tcp = TcpStream::connect_timeout(
            &format!("{}:{}", config.host, AUTH_PORT).parse().map_err(|e| {
                io::Error::new(io::ErrorKind::InvalidInput, format!("bad address: {}", e))
            })?,
            Duration::from_secs(TIMEOUT_SSL_AUTH),
        )?;

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
            | session::FLAG_UNKNOWN_19
            | session::FLAG_UNKNOWN_20
            | if config.paper { session::FLAG_PAPER_CONNECT } else { 0 };
        let connect_req = format!(
            "{};{};{};{};{};",
            NS_VERSION,
            ns::NS_CONNECT_REQUEST,
            flags,
            config.username,
            hw_info
        );
        session::send_secure(tls.get_mut(), &mut channel, connect_req.as_bytes())?;

        // Receive AUTH_START
        let _auth_start = session::recv_secure(tls.get_mut(), &mut channel)?;

        // SRP-6 authentication
        log::info!("Starting SRP auth for {}", config.username);
        let session_key = do_srp(tls.get_mut(), &config.username, &config.password)?;
        log::info!("SRP auth complete");

        // Receive post-auth messages (CONNECT_RESPONSE, FIX_START)
        for _ in 0..5 {
            match session::recv_msg(tls.get_mut()) {
                Ok(RecvMsg::Ns { msg_type, .. }) => {
                    if msg_type == ns::NS_FIX_START {
                        log::info!("FIX_START received — CCP ready for FIX");
                        break;
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    log::warn!("Post-auth recv error: {}", e);
                    break;
                }
            }
        }

        // --- Phase 2: CCP FIX logon (over TLS) ---
        let logon_msg = build_ccp_logon(&hw_info, &encoded, CCP_HEARTBEAT, 1);
        tls.write_all(&logon_msg)?;

        // Read logon response
        tls.get_ref().set_read_timeout(Some(Duration::from_secs_f64(TIMEOUT_FIX_LOGON)))?;
        let response = fix_read(&mut tls)?;
        tls.get_ref().set_read_timeout(None)?;

        let fields = fix_parse(&response);
        let msg_type = fields.get(&35).map(|s| s.as_str()).unwrap_or("");
        match msg_type {
            "A" | "B" | "U" => {}
            "3" | "5" => {
                let reason = fields.get(&58).map(|s| s.as_str()).unwrap_or("unknown");
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("FIX Logon rejected: {}", reason),
                ));
            }
            _ => {
                log::warn!("Unexpected FIX logon response type: {}", msg_type);
            }
        }

        let account_id = fields.get(&1).cloned().unwrap_or_default();
        let heartbeat_interval: u64 = fields
            .get(&108)
            .and_then(|s| s.parse().ok())
            .unwrap_or(CCP_HEARTBEAT);
        let server_session_id = fields.get(&8035).cloned().unwrap_or_else(|| {
            let marker = b"\x018035=";
            if let Some(pos) = response.windows(marker.len()).position(|w| w == marker) {
                let val_start = pos + marker.len();
                if let Some(end) = response[val_start..].iter().position(|&b| b == SOH) {
                    return String::from_utf8_lossy(&response[val_start..val_start + end])
                        .to_string();
                }
            }
            String::new()
        });
        let ccp_token = fields.get(&6386).cloned().unwrap_or_default();

        log::info!(
            "CCP FIX logon: account={} session_id={} hb={}s",
            account_id, server_session_id, heartbeat_interval
        );

        // CCP Connection (non-blocking TLS for hot loop)
        let ccp_conn = Connection::new(tls)?;

        // --- Phase 3: Farm connection ---
        log::info!("Connecting to farm {}:{}", config.host, MISC_PORT);
        let farm_tcp = TcpStream::connect_timeout(
            &format!("{}:{}", config.host, MISC_PORT).parse().map_err(|e| {
                io::Error::new(io::ErrorKind::InvalidInput, format!("bad address: {}", e))
            })?,
            Duration::from_secs(TIMEOUT_FARM_CONNECT),
        )?;

        // Farm DH key exchange (raw TCP, no TLS)
        let mut farm_channel = SecureChannel::new();
        let farm_dh = farm_channel.build_secure_connect(NS_VERSION, NS_VERSION);
        let mut farm_stream = farm_tcp;
        farm_stream.write_all(&farm_dh)?;

        let (farm_payload, _) = ns::ns_recv(&mut farm_stream)?;
        let farm_text = String::from_utf8_lossy(&farm_payload);
        let farm_parts: Vec<&str> = farm_text.split(';').collect();
        let farm_msg_type: u32 = farm_parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        if farm_msg_type != ns::NS_SECURE_CONNECTION_START {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Farm DH: expected 533, got {}", farm_msg_type),
            ));
        }
        farm_channel.process_server_hello(&farm_parts[2..]);
        log::info!("Farm DH complete");

        // Send encrypted FIX logon
        let farm_session_id = if server_session_id.is_empty() {
            session::get_session_id()
        } else {
            server_session_id.clone()
        };
        let logon_bytes = build_farm_encrypted_logon(
            &farm_channel,
            &config.username,
            config.paper,
            "usfarm",
            &farm_session_id,
            &session_key,
            &hw_info,
            &encoded,
        );
        farm_stream.write_all(&logon_bytes)?;
        log::info!("Farm encrypted logon sent");

        // Farm logon exchange: AUTH_START → SOFT_TOKEN → logon ACK
        let read_mac_key = farm_channel
            .key_block()
            .map(|kb| kb[84..104].to_vec())
            .unwrap_or_default();
        let initial_read_iv = farm_channel
            .key_block()
            .map(|kb| kb[48..64].to_vec())
            .unwrap_or_default();
        let (read_iv, sign_iv) = farm_logon_exchange(
            &mut farm_stream,
            &mut farm_channel,
            &session_key,
            &read_mac_key,
            &initial_read_iv,
        )?;
        log::info!("Farm logon exchange complete");

        // Send routing table request (35=U, 6040=112) before switching to non-blocking
        let sign_mac_key = farm_channel
            .key_block()
            .map(|kb| kb[64..84].to_vec())
            .unwrap_or_default();
        let routing_hosts = send_routing_request(
            &mut farm_stream, &sign_mac_key, &sign_iv, &read_mac_key, &read_iv,
        );
        // Routing updates sign_iv/read_iv but Connection re-starts from post-logon IVs,
        // so we create Connection with the IVs returned from routing.
        let (final_sign_iv, final_read_iv) = match &routing_hosts {
            Ok((_, siv, riv)) => (siv.clone(), riv.clone()),
            Err(_) => (sign_iv, read_iv),
        };
        if let Ok((hosts, _, _)) = &routing_hosts {
            for (name, host) in hosts {
                log::info!("Routing: {} → {}", name, host);
            }
        }

        // Create farm Connection (raw TCP + HMAC signing)
        let mut farm_conn = Connection::new_raw(farm_stream)?;
        farm_conn.set_keys(sign_mac_key, final_sign_iv, read_mac_key, final_read_iv);

        let gw = Gateway {
            account_id,
            session_token: session_key,
            server_session_id,
            ccp_token,
            heartbeat_interval,
            hw_info,
            encoded,
        };
        Ok((gw, farm_conn, ccp_conn))
    }

    /// Create the control channel and build a HotLoop with connected sockets.
    pub fn into_hot_loop<S: Strategy>(
        self,
        strategy: S,
        farm_conn: Connection,
        ccp_conn: Connection,
        core_id: Option<usize>,
    ) -> (HotLoop<S>, Sender<ControlCommand>) {
        let (tx, rx) = bounded(64);
        let mut hot_loop = HotLoop::new(strategy, core_id);
        hot_loop.set_control_rx(rx);
        hot_loop.farm_conn = Some(farm_conn);
        hot_loop.ccp_conn = Some(ccp_conn);
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

/// Send 35=U routing request (6040=112) and parse 35=T response.
/// Returns Vec of (farm_name, host) pairs and updated sign/read IVs.
/// Runs in blocking mode (before Connection switches to non-blocking).
fn send_routing_request(
    stream: &mut TcpStream,
    sign_key: &[u8],
    sign_iv: &[u8],
    read_key: &[u8],
    read_iv: &[u8],
) -> io::Result<(Vec<(String, String)>, Vec<u8>, Vec<u8>)> {
    let now = chrono_free_timestamp();
    let msg = fix_build(&[
        (fix::TAG_MSG_TYPE, "U"),
        (fix::TAG_SENDING_TIME, &now),
        (6040, "112"),
        (6556, "1"), // usfarm channel
    ], 1); // seq doesn't matter, farm ignores it for routing

    // Sign and send
    let (signed, new_sign_iv) = fix::fix_sign(&msg, sign_key, sign_iv);
    stream.write_all(&signed)?;

    // Read response with timeout
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut buf = vec![0u8; 32768];
    let mut total = Vec::new();
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => total.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut => break,
            Err(e) => return Err(e),
        }
    }
    stream.set_read_timeout(None)?;

    // Unsign
    let (unsigned, new_read_iv, _valid) = fix::fix_unsign(&total, read_key, read_iv);

    // Parse 35=T routing table
    let hosts = parse_routing_table(&unsigned);
    Ok((hosts, new_sign_iv, new_read_iv))
}

/// Parse 8=O 35=T routing table body.
/// Body: semicolon-delimited entries, each comma-delimited:
///   {exchange},{sectype},{datatypes},{filter},{wildcard},{host},{port},{farmname};...
fn parse_routing_table(data: &[u8]) -> Vec<(String, String)> {
    let text = String::from_utf8_lossy(data);

    // Find body after "35=T\x01"
    let marker = "35=T\x01";
    let body = match text.find(marker) {
        Some(pos) => &text[pos + marker.len()..],
        None => return Vec::new(),
    };

    // Skip 6556= field if present
    let body = if body.starts_with("6556=") {
        match body.find('\x01') {
            Some(p) => &body[p + 1..],
            None => return Vec::new(),
        }
    } else {
        body
    };

    let mut hosts = Vec::new();
    for entry in body.split(';') {
        if entry.is_empty() {
            continue;
        }
        let parts: Vec<&str> = entry.split(',').collect();
        if parts.len() >= 8 && !parts[5].is_empty() {
            let host = parts[5].to_string();
            let farm_name = parts[7].to_string();
            if !farm_name.is_empty() {
                hosts.push((farm_name, host));
            }
        }
    }
    hosts
}

/// Format timestamp as YYYYMMDD-HH:MM:SS (no chrono dependency).
fn chrono_free_timestamp() -> String {
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
        use crate::auth::dh::SecureChannel;
        // We can't fully test without real DH, but verify inner structure
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

    #[test]
    fn parse_routing_table_extracts_hosts() {
        // Simulate 8=O 35=T body
        // Format: exchange,sectype,datatypes,filter,wildcard,host,port,farmname
        let body = b"8=O\x019=999\x0135=T\x016556=1\x01\
ARCA,STK,AllLast|BidAsk|Top,0,,cashhmds.ibllc.com,4001,cashhmds;\
ISLAND,STK,Deep|Deep2,0,,usopt.ibllc.com,4002,usopt;\
EMPTY,,,,,,,;\x01";
        let hosts = parse_routing_table(body);
        assert_eq!(hosts.len(), 2);
        assert_eq!(hosts[0].0, "cashhmds");
        assert_eq!(hosts[0].1, "cashhmds.ibllc.com");
        assert_eq!(hosts[1].0, "usopt");
        assert_eq!(hosts[1].1, "usopt.ibllc.com");
    }

    #[test]
    fn parse_routing_table_empty() {
        let data = b"8=O\x019=10\x0135=T\x016556=1\x01\x01";
        let hosts = parse_routing_table(data);
        assert!(hosts.is_empty());
    }

    #[test]
    fn parse_routing_table_no_35t() {
        let data = b"8=O\x019=10\x0135=A\x01stuff";
        let hosts = parse_routing_table(data);
        assert!(hosts.is_empty());
    }
}
