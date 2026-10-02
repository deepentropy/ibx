//! Gateway: orchestrates auth + data connections into a running HotLoop.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use crossbeam_channel::{Sender, bounded};
use native_tls::TlsConnector;
use num_bigint::BigUint;
use sha1::{Digest, Sha1};
use zeroize::Zeroizing;

use std::net::ToSocketAddrs;

use crate::auth::crypto::strip_leading_zeros;
use crate::auth::dh::SecureChannel;
use crate::auth::session::{self, do_srp, do_soft_token};
use crate::config::*;
use std::sync::Arc;
use crate::bridge::{Event, SharedState};
use crate::engine::hot_loop::HotLoop;
use crate::protocol::connection::Connection;
use crate::protocol::fix::{self, fix_build, fix_parse, fix_read_deadline, SOH};
use crate::protocol::fixcomp;
use crate::protocol::ns;
use crate::types::ControlCommand;

/// Parse the `PRIV_LAB_MISC_URLS` blob (FIX tag 6321) into a `{key: value}` map.
///
/// Wire format: pipe-delimited `k=v|k=v|…`, with `%7C` escaping a literal `|`
/// inside keys or values. Falls back to comma as the entry separator when the
/// payload contains no `|`. Empty input yields an empty map; entries without
/// `=` or with an empty key are dropped.
pub fn parse_misc_urls(s: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    if s.is_empty() {
        return out;
    }
    let sep = if s.contains('|') { '|' } else { ',' };
    for entry in s.split(sep) {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let Some((k, v)) = entry.split_once('=') else {
            continue;
        };
        let key = k.trim().replace("%7C", "|").replace("%7c", "|");
        let val = v.trim().replace("%7C", "|").replace("%7c", "|");
        if key.is_empty() {
            continue;
        }
        out.insert(key, val);
    }
    out
}

/// Parse a farm-route string from the auth-server's routing tags.
///
/// Three accepted shapes (per ib-agent#128):
///   "<host>/<farm>"             — tag 6145 (trading)
///   "<host>/<farm>/<port>"      — tags 6171 (mktdata) / 8008 (secdef)
///
/// Port is informational only — ibx routes all farm channels to the same
/// data-port discovered via `misc_port()`. We just need (host, farm).
/// Returns `None` for empty or malformed input.
pub fn parse_farm_route(route: &str) -> Option<(String, String)> {
    if route.is_empty() { return None; }
    let mut parts = route.splitn(3, '/');
    let host = parts.next()?.to_string();
    let farm = parts.next()?.to_string();
    if host.is_empty() || farm.is_empty() { return None; }
    Some((host, farm))
}

/// Returns true if `buf` contains at least one complete `8=O` (binary) or
/// `8=FIXCOMP` frame. Used to terminate read drains as soon as the expected
/// response is fully buffered.
fn has_complete_response_frame(buf: &[u8]) -> bool {
    if buf.starts_with(b"8=O\x01") {
        if let Some(tag9_off) = buf[4..].windows(2).position(|w| w == b"9=") {
            let tag9_pos = 4 + tag9_off;
            if let Some(soh_off) = buf[tag9_pos..].iter().position(|&b| b == b'\x01') {
                let soh_pos = tag9_pos + soh_off;
                if let Ok(s) = std::str::from_utf8(&buf[tag9_pos + 2..soh_pos]) {
                    if let Ok(body_len) = s.parse::<usize>() {
                        return soh_pos + 1 + body_len <= buf.len();
                    }
                }
            }
        }
        return false;
    }
    let mut cursor = 0usize;
    while cursor + 12 <= buf.len() {
        if buf[cursor..].starts_with(b"8=FIXCOMP\x01") {
            if let Some(total_len) = fixcomp::fixcomp_length(&buf[cursor..]) {
                return cursor + total_len <= buf.len();
            }
            return false;
        }
        cursor += 1;
    }
    false
}

/// Compute token short hash for farm logon (FIX tag 8483).
///
/// Per ib-agent#125: gateway always emits this as **8 hex chars padded with
/// leading zeros**. `format!("{:x}", n)` is wrong when `hash_int`'s high
/// nibble is zero — server silently rejects the FIX 35=A logon in that case.
pub fn token_short_hash(session_token: &BigUint) -> String {
    let token_bytes = session_token.to_bytes_be();
    let stripped = strip_leading_zeros(&token_bytes);
    let digest = Sha1::digest(stripped);
    // Take last 4 bytes as u32 (Java BigInteger.intValue() truncates to low 32 bits)
    let hash_int = u32::from_be_bytes([digest[16], digest[17], digest[18], digest[19]]);
    format!("{:08x}", hash_int)
}

/// Build auth server logon message.
///
/// Tag 6266 (`encoded`) carries `{jdkVer}/{platform}/{locale}/{dist}`.
/// The auth server requires the `{locale}` segment to be a canonical Java
/// `Locale.toString()` value — `en_US`, `fr`, `ja_JP`, etc. Bare `en` is
/// rejected as `invalid twsInfo`. Override via `IBX_LOCALE` (locale only)
/// or `IBX_ENCODED` (full string).
///
/// Tag 8361 = `"(rolling)"` is load-bearing: it marks the client as a
/// rolling-release build, which bypasses the server's IB_BUILD allow-list
/// check. Without it the server rejects with "The TWS build you are
/// currently running is no longer supported." Per ib-agent#141 the
/// official client also keeps 6397/6947/8098, so we leave them in.
///
/// The time zone field carries the machine's IANA time zone name (e.g.
/// `Europe/Paris`), as the reference client does; `IBX_TZ` overrides it and
/// `UTC` is the fallback when the system zone has no IANA name.
pub fn build_ccp_logon(hw_info: &str, encoded: &str, heartbeat: u64, seq: u32) -> Vec<u8> {
    ccp_logon(hw_info, encoded, heartbeat, seq, "")
}

/// Logon for a reconnect of the same server session: the fresh logon plus
/// the session epoch of the previous logon reply, in the reference order.
/// An empty epoch gives the fresh logon (ibx#422).
pub fn build_ccp_reconnect_logon(hw_info: &str, encoded: &str, heartbeat: u64, seq: u32, session_epoch: &str) -> Vec<u8> {
    ccp_logon(hw_info, encoded, heartbeat, seq, session_epoch)
}

fn ccp_logon(hw_info: &str, encoded: &str, heartbeat: u64, seq: u32, session_epoch: &str) -> Vec<u8> {
    let now = chrono_free_timestamp();
    let tz = machine_time_zone();
    let hb_str = heartbeat.to_string();
    let hw_field = format!("<{}|{}>", hw_info, session::get_lan_ip());
    let mut fields: Vec<(u32, &str)> = Vec::with_capacity(15);
    fields.extend_from_slice(&[
        (fix::TAG_MSG_TYPE, fix::MSG_LOGON),
        (fix::TAG_SENDING_TIME, &now),
        (fix::TAG_ENCRYPT_METHOD, "0"),
        (fix::TAG_HEARTBEAT_INT, &hb_str),
        (fix::TAG_RESET_SEQ_NUM, "Y"),
    ]);
    if !session_epoch.is_empty() {
        fields.push((TAG_SESSION_EPOCH, session_epoch));
    }
    fields.extend_from_slice(&[
        (fix::TAG_IB_BUILD, IB_BUILD),
        (fix::TAG_IB_VERSION, IB_VERSION),
        (6490, "dark"),
        (6266, encoded),
        (6351, &hw_field),
        (6397, "1"),
        (6947, &tz),
        (8361, "(rolling)"),
        (8098, "0"),
    ]);
    fix_build(&fields, seq)
}

/// Session epoch of the server session, echoed on a reconnect logon (ibx#422).
const TAG_SESSION_EPOCH: u32 = 6059;

/// Time zone sent at logon: `IBX_TZ` when set, else the machine zone. It
/// is also the machine zone of an order's expiry zone rule (ibx#335).
pub(crate) fn machine_time_zone() -> String {
    time_zone_or_system(std::env::var("IBX_TZ").ok())
}

fn time_zone_or_system(override_tz: Option<String>) -> String {
    if let Some(tz) = override_tz.filter(|s| !s.is_empty()) {
        return tz;
    }
    jiff::tz::TimeZone::try_system()
        .ok()
        .and_then(|tz| tz.iana_name().map(str::to_string))
        .unwrap_or_else(|| "UTC".to_string())
}

/// The session epoch in a logon reply (plain or compressed), if any.
fn logon_reply_epoch(response: &[u8]) -> Option<String> {
    let mut text = response.to_vec();
    if response.starts_with(b"8=FIXCOMP\x01") {
        text.clear();
        for inner in fixcomp::fixcomp_decompress(response).ok()? {
            text.extend_from_slice(&inner);
            text.push(SOH);
        }
    }
    fix_parse(&text).remove(&TAG_SESSION_EPOCH).filter(|v| !v.is_empty())
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
    slot: u32,
) -> Vec<u8> {
    let inner = build_farm_logon(username, paper, farm_name, session_id, session_token, hw_info, encoded, slot);
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

/// The farm logon message, not encrypted: what goes inside the encrypted
/// logon, and what is sent as it is when the farm session runs in clear
/// (ibx#423).
pub fn build_farm_logon(
    username: &str,
    _paper: bool,
    farm_name: &str,
    session_id: &str,
    session_token: &BigUint,
    hw_info: &str,
    encoded: &str,
    slot: u32,
) -> Vec<u8> {
    let display_name = format!("S{}", username);
    let farm_id = format!("{}/{}/{}", display_name, slot, farm_name);
    let farm_id_len = farm_id.len().to_string();
    let token_hash = token_short_hash(session_token);
    let ns_range = format!("{}..{}", NS_VERSION_MIN, NS_VERSION);
    let now = chrono_free_timestamp();
    let hb_str = FARM_HEARTBEAT.to_string();
    let hw_field = format!("<{}|{}>", hw_info, session::get_lan_ip());

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

    log::info!(
        "{} FIX 35=A pre-encrypt ({} bytes): {}",
        farm_name,
        inner.len(),
        String::from_utf8_lossy(&inner).replace('\x01', "|"),
    );
    inner
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
    // Poll on a short read timeout and tolerate transient WouldBlock/TimedOut
    // returns until an overall deadline. A single slow response segment from a
    // high-latency regional gateway must not tear down the connection (ibx#237).
    stream.set_read_timeout(Some(Duration::from_millis(FARM_LOGON_POLL_MS)))?;
    let deadline = std::time::Instant::now() + Duration::from_secs_f64(TIMEOUT_FARM_LOGON);
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
            let n = match stream.read(&mut tmp) {
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock
                    || e.kind() == io::ErrorKind::TimedOut =>
                {
                    if std::time::Instant::now() >= deadline {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "farm logon timed out waiting for server response",
                        ));
                    }
                    continue;
                }
                Err(e) => return Err(e),
            };
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
            // A signed frame is verified; on a mismatch the logon fails and
            // the IV is not advanced, as in the reference (ibx#275).
            // A farm session in clear has no signing key (ibx#423).
            let parsed_msg = if fix::is_signed(&msg) && !read_mac_key.is_empty() {
                let (unsigned, new_iv, valid) = fix::fix_unsign(&msg, read_mac_key, &read_iv);
                if !valid {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "farm logon: frame signature mismatch",
                    ));
                }
                read_iv = new_iv;
                unsigned
            } else {
                msg.clone()
            };
            let fields = fix_parse(&parsed_msg);

            // Check for encrypted content (tags 91/96). A farm session in
            // clear sends its auth start as it is (ibx#423).
            let enc_tag = fields.get(&91).or_else(|| fields.get(&96));
            let plain_auth_start = enc_tag.is_none() && fields.get(&35).map(|s| s.as_str()) == Some("S");
            if enc_tag.is_some() || plain_auth_start {
                let decrypted = match enc_tag {
                    Some(b64_data) => {
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
                        decrypted
                    }
                    None => parsed_msg.clone(),
                };

                // Check for auth challenge → respond with token, fall back to SRP if rejected.
                // Outcome asymmetry (ib-agent#153, ibx#187):
                //   PASSED  — token accepted, continue
                //   UNKNOWN — server cache miss, recover via SRP on this socket
                //   FAILED  — `do_soft_token` returns Err; the OUTER reconnect loop
                //             must drop this socket and retry from scratch with a
                //             fresh soft-token (NOT SRP — captured behavior).
                if decrypted.windows(5).any(|w| w == b"35=S\x01") {
                    // Pass the farm read buffer as the auth carry buffer: the
                    // auth exchange reads on the same socket, and a high-latency
                    // gateway can coalesce its final response with the farm logon
                    // ACK. Threading `buf` through keeps those trailing ACK bytes
                    // so the loop below re-frames them instead of stalling on a
                    // read for bytes already consumed (ibx#237).
                    match do_soft_token(stream, session_token, &mut buf)? {
                        session::SoftTokenOutcome::Passed => {}
                        session::SoftTokenOutcome::Unknown => {
                            log::warn!("Soft token rejected — falling back to SRP farm auth");
                            stream.set_read_timeout(Some(Duration::from_millis(FARM_LOGON_POLL_MS)))?;
                            session::do_srp_farm(stream, username, password, &mut buf)?;
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
    /// Wrapped in `Zeroizing` so the plaintext is wiped from memory on drop.
    pub password: Zeroizing<String>,
    pub paper: bool,
    pub session_key: BigUint,
    pub session_token: BigUint,
    pub server_session_id: String,
    pub hw_info: String,
    pub encoded: String,
    /// Historical-data farm routing parsed from the auth-server response.
    /// Used by HMDS reconnect (ibx#187) — empty when no HMDS route was parsed.
    pub hmds_host: String,
    pub hmds_farm: String,
    /// Market-data farm of the session (host and name from the logon
    /// routing tag), used by the farm reconnect, as the reference reuses
    /// the farm of the lost connection (ibx#295). Empty: the auth host and
    /// the default farm name.
    pub farm_host: String,
    pub farm_name: String,
    /// Session epoch of the last logon reply, sent back on a reconnect logon
    /// so the server can resume the same session (ibx#422). Empty when the
    /// server sent none.
    pub session_epoch: String,
    /// The server refused the encryption of the last auth login, which went
    /// on in clear: farms opened after it log on in clear, as in the
    /// reference (ibx#423).
    pub ns_secure_refused: bool,
}

/// A CCP reconnect: the new connection and the session epoch of its logon
/// reply, when the reply carried one.
pub struct CcpReconnect {
    pub conn: Connection,
    pub session_epoch: Option<String>,
    /// The server refused the encryption of this login, which went on in
    /// clear: the farms opened after it log on in clear (ibx#423).
    pub ns_secure_refused: bool,
}

/// Full gateway connection.
pub struct Gateway {
    pub account_id: String,
    pub session_token: BigUint,
    /// Session ID surfaced to webapp REST clients as `x-ccp-session-id`.
    /// Sourced from the post-auth FIX logon ACK, falling back to the locally generated
    /// session ID when the gateway does not echo one back.
    pub server_session_id: String,
    /// Logon tag 6386: in the reference, the object key of the settings
    /// download; not used by ibx (ibx#483).
    pub settings_object_key: String,
    pub heartbeat_interval: u64,
    /// Stored for farm reconnection.
    pub hw_info: String,
    pub encoded: String,
    /// Raw soft dollar tier data from CCP logon tag 6522 (ibx#480).
    pub raw_soft_dollar_tiers: String,
    /// Raw family code data from CCP logon tag 6823.
    pub raw_family_codes: String,
    /// Raw news provider data from CCP logon tag 6830.
    pub raw_news_providers: String,
    /// Raw API news source list from the CCP logon (ibx#460).
    pub raw_news_sources: String,
    /// Raw news source capabilities from the CCP logon (ibx#460).
    pub raw_news_capabilities: String,
    /// The logon feature list denies news: no API news source (ibx#460).
    pub deny_news: bool,
    /// White branding ID from CCP logon (empty for standard accounts).
    pub white_branding_id: String,
    /// FA session: CCP logon tag 6108 is "1" (ibx#481).
    pub fa_session: bool,
    /// The logon's super user and omnibus flags (ibx#417).
    pub super_user: bool,
    pub omnibus: bool,
    /// Account config (6040=210): feature list (6542) and MiFID config id
    /// (8234); None when the answer was not in the login burst (ibx#425).
    pub account_config: Option<(Vec<String>, String)>,
    /// The algo definition answers of the login burst (ibx#263).
    pub algo_definitions: Vec<String>,
    /// The logon feature list asks for US stock sizes in round lots
    /// (ibx#287).
    pub scale_us_lots: bool,
    /// Most contracts with tick-by-tick data at once, from the logon's
    /// limits (ibx#455).
    pub tick_by_tick_limit: usize,
    /// Most contracts with API depth at once, from the logon (#452).
    pub depth_limit: usize,
    /// The logon feature list turns tick-by-tick data off (ibx#455).
    pub tick_by_tick_off: bool,
    /// The logon feature list allows the price management flag (ibx#492).
    pub price_mgmt: bool,
    /// The logon's price management exclusion list, None when absent
    /// (ibx#492).
    pub price_mgmt_exclusions: Option<String>,
    /// Most real-time bar requests at once, from the logon (ibx#454).
    pub max_real_time_requests: u32,
    /// Logical-name → host URL map pushed by the gateway during logon. Empty when no
    /// URL set was pushed (callers should then fall back to a documented literal,
    /// e.g. `api.ibkr.com` for `region_dam`).
    pub misc_urls: std::collections::HashMap<String, String>,
    /// CCP HMAC signing key (kb[64..84]) for selective signing of XML messages.
    pub ccp_sign_key: Vec<u8>,
    /// CCP HMAC initial IV (kb[48..64]) for selective signing.
    pub ccp_sign_iv: Vec<u8>,
    /// Historical-data farm routing parsed from the auth-server response,
    /// retained for HMDS reconnect (ibx#187).
    pub hmds_host: String,
    pub hmds_farm: String,
    /// Session epoch of the logon reply, for the reconnect logon (ibx#422).
    pub session_epoch: String,
    /// Name of the market-data farm, for the farm status messages (ibx#399).
    pub farm_name: String,
    /// Host of the market-data farm, for the farm reconnect (ibx#295).
    pub farm_host: String,
    /// Rows of the routing tables of the two primary farms, when they came
    /// with the logon (#445). A table that comes later is read by the loop.
    pub md_routing: Option<String>,
    pub hmds_routing: Option<String>,
    /// The server refused the encryption of the auth login, which went on
    /// in clear (ibx#423).
    pub ns_secure_refused: bool,
}

/// Request ids of the routing-table requests: one process-wide counter
/// starting at 1, as in the reference, whatever the farm (ibx#253).
static ROUTING_REQUEST_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);

fn next_routing_request_id() -> u32 {
    ROUTING_REQUEST_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
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
    slot: u32,
) -> io::Result<Connection> {
    connect_farm_ex(host, farm_id, username, password, paper, server_session_id, session_key,
        hw_info, encoded, slot, true).map(|(conn, _)| conn)
}

/// [`connect_farm`], with the routing table request only when `routing`
/// is set: the reference asks for it on the two primary farms, never on a
/// farm opened on demand (#445). Also returns the rows of the table when
/// its answer came with the logon.
pub fn connect_farm_ex(
    host: &str,
    farm_id: &str,
    username: &str,
    password: &str,
    paper: bool,
    server_session_id: &str,
    session_key: &BigUint,
    hw_info: &str,
    encoded: &str,
    slot: u32,
    routing: bool,
) -> io::Result<(Connection, Option<String>)> {
    connect_farm_opts(host, farm_id, username, password, paper, server_session_id, session_key,
        hw_info, encoded, slot, routing, true)
}

/// [`connect_farm_ex`], with the key exchange only when `ns_secure` is set.
/// The reference does not ask a farm for the encryption after the server
/// refused it on the auth login, and goes on in clear when a farm refuses
/// it with the permission to go on (ibx#423); the logon is then sent in
/// clear and the session is not signed.
pub fn connect_farm_opts(
    host: &str,
    farm_id: &str,
    username: &str,
    password: &str,
    paper: bool,
    server_session_id: &str,
    session_key: &BigUint,
    hw_info: &str,
    encoded: &str,
    slot: u32,
    routing: bool,
    ns_secure: bool,
) -> io::Result<(Connection, Option<String>)> {
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
    farm_session(farm_tcp, farm_id, username, password, paper, server_session_id, session_key,
        hw_info, encoded, slot, routing, ns_secure)
}

/// The farm session on a connected socket: key exchange when `ns_secure`,
/// logon, token auth, routing table.
fn farm_session(
    farm_tcp: TcpStream,
    farm_id: &str,
    username: &str,
    password: &str,
    paper: bool,
    server_session_id: &str,
    session_key: &BigUint,
    hw_info: &str,
    encoded: &str,
    slot: u32,
    routing: bool,
    ns_secure: bool,
) -> io::Result<(Connection, Option<String>)> {
    // Key exchange (raw TCP). Any failure, an error answer included,
    // drops the socket and the farm is tried again, as in the reference.
    let mut channel = SecureChannel::new();
    let mut stream = farm_tcp;
    let secure = if ns_secure {
        let dh_msg = channel.build_secure_connect(NS_VERSION, NS_VERSION);
        stream.write_all(&dh_msg)?;
        let secure = session::read_key_exchange_answer(&mut stream, &mut channel)
            .map_err(|e| io::Error::new(e.kind(), format!("{} key exchange: {}", farm_id, e)))?;
        if secure {
            log::info!("{} key exchange complete", farm_id);
        }
        secure
    } else {
        log::info!("{}: no key exchange, the auth login runs in clear", farm_id);
        false
    };

    // Logon: encrypted, or in clear without a key exchange (ibx#423).
    let farm_session_id = if server_session_id.is_empty() {
        session::get_session_id()
    } else {
        server_session_id.to_string()
    };
    if secure {
        let logon_bytes = build_farm_encrypted_logon(
            &mut channel, username, paper, farm_id,
            &farm_session_id, session_key, hw_info, encoded, slot,
        );
        stream.write_all(&logon_bytes)?;
        log::info!("{} encrypted logon sent", farm_id);
    } else {
        let logon_bytes = build_farm_logon(
            username, paper, farm_id, &farm_session_id, session_key, hw_info, encoded, slot,
        );
        stream.write_all(&logon_bytes)?;
        log::info!("{} logon sent in clear", farm_id);
    }

    // Logon exchange: challenge → token auth → logon ACK
    let read_mac_key = channel.key_block().map(|kb| kb[84..104].to_vec()).unwrap_or_default();
    let initial_read_iv = channel.key_block().map(|kb| kb[48..64].to_vec()).unwrap_or_default();
    let (read_iv, sign_iv, logon_remaining) = farm_logon_exchange(
        &mut stream, &mut channel, session_key, username, password,
        &read_mac_key, &initial_read_iv,
    )?;
    log::info!("{} logon exchange complete, {} bytes remaining", farm_id, logon_remaining.len());

    let sign_mac_key = channel.key_block().map(|kb| kb[64..84].to_vec()).unwrap_or_default();

    // Send routing table request after logon. Its request id is a unique
    // counter, as in the reference; it is not derived from the farm name
    // (ibx#253). A farm opened on demand is not asked (#445).
    let mut final_sign_iv = sign_iv.clone();
    let mut resp_buf = Vec::new();
    if routing {
        let request_id = next_routing_request_id().to_string();
        let now = chrono_free_timestamp();
        let routing_msg = fix_build(&[
            (fix::TAG_MSG_TYPE, "U"),
            (fix::TAG_SENDING_TIME, &now),
            (6040, "112"),
            (6556, &request_id),
        ], 1);
        let wrapped = fixcomp::fixcomp_build(&routing_msg);

        let (signed, new_sign_iv) = fix::fix_sign(&wrapped, &sign_mac_key, &sign_iv);
        stream.write_all(&signed)?;
        final_sign_iv = new_sign_iv;
        log::info!("{} sent routing request (6556={})", farm_id, request_id);

        // Read routing response. Frame-based termination: poll with a short
        // timeout, break as soon as we have at least one complete FIXCOMP frame
        // buffered. The 5-s read timeout remains as the worst-case fallback.
        stream.set_read_timeout(Some(Duration::from_millis(100)))?;
        let routing_deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let mut tmp = [0u8; 8192];
            match stream.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => {
                    resp_buf.extend_from_slice(&tmp[..n]);
                    if has_complete_response_frame(&resp_buf) { break; }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock
                    || e.kind() == io::ErrorKind::TimedOut =>
                {
                    if has_complete_response_frame(&resp_buf) { break; }
                    if std::time::Instant::now() >= routing_deadline { break; }
                }
                Err(e) => return Err(e),
            }
        }
        log::info!("{} routing response: {} bytes", farm_id, resp_buf.len());
    }

    // Create Connection (switches to non-blocking), inject routing bytes
    let mut conn = Connection::new_raw(stream)?;
    conn.set_keys(sign_mac_key, final_sign_iv, read_mac_key, read_iv);
    // The routing request was seq=1; the next send_fix is seq=2.
    conn.seq = if routing { 1 } else { 0 };

    // Inject logon remaining bytes + routing response into connection buffer.
    // Python processes logon remaining before routing, but both need read_iv chaining.
    if !logon_remaining.is_empty() {
        conn.inject_buf(&logon_remaining);
    }
    if !resp_buf.is_empty() {
        conn.inject_buf(&resp_buf);
    }
    // Extract and process all frames (unsign + respond to TestRequests, like Python).
    let mut table = None;
    let frames = conn.extract_frames();
    for frame in &frames {
        match frame {
            crate::protocol::connection::Frame::FixComp(raw) => {
                let (unsigned, valid) = conn.unsign(raw);
                if !valid {
                    return Err(signature_mismatch(farm_id));
                }
                let inner = fixcomp::fixcomp_decompress(&unsigned).unwrap_or_else(|e| {
                    log::warn!("{}: dropping malformed FIXCOMP frame: {}", farm_id, e);
                    Vec::new()
                });
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
                let (unsigned, valid) = conn.unsign(raw);
                if !valid {
                    return Err(signature_mismatch(farm_id));
                }
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
                let (unsigned, valid) = conn.unsign(raw);
                if !valid {
                    return Err(signature_mismatch(farm_id));
                }
                log::info!("{} routing 8=O: {} bytes", farm_id, raw.len());
                if let Some(text) = crate::engine::routing::table_text(&unsigned) {
                    table = Some(text);
                }
            }
            crate::protocol::connection::Frame::Control(raw) => {
                // 8=1 / 8=X control state — extracted, not routed (ibx#185).
                log::debug!("{} ignoring control frame: {} bytes", farm_id, raw.len());
            }
        }
    }
    if !frames.is_empty() {
        log::info!("{} post-logon frames: {} frames, seq now {}", farm_id, frames.len(), conn.seq);
    }
    Ok((conn, table))
}

/// Error of a frame whose signature does not match on `farm_id` (ibx#275).
fn signature_mismatch(farm_id: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("{}: frame signature mismatch", farm_id))
}

/// Reconnect to the CCP (order/auth) server using cached session credentials.
/// Performs TLS + CONNECT_REQUEST (no key exchange, ibx#423), then attempts SOFT_TOKEN auth with cached K.
/// If the server signals at AUTH_START that it requires full SRP, transparently
/// falls back to a fresh SRP handshake using the cached `username`/`password`
/// in `ReconnectAuth` (the same path used by `Gateway::connect`).
/// The connection has the order status request already sent.
pub fn reconnect_ccp(auth: &ReconnectAuth) -> io::Result<Connection> {
    let mut conn = reconnect_ccp_session(auth)?.conn;
    let now = chrono_free_timestamp();
    conn.send_fix(&[(35, "H"), (52, &now), (11, "*"), (54, "*"), (55, "*")])?;
    Ok(conn)
}

/// [`reconnect_ccp`] without the order status request, also returning the
/// session epoch of the new logon reply so the caller can keep it for the
/// next reconnect (ibx#422). The caller sends the post-logon requests.
pub fn reconnect_ccp_session(auth: &ReconnectAuth) -> io::Result<CcpReconnect> {
    reconnect_ccp_via(auth, &auth.host)
}

/// [`reconnect_ccp_session`] to `host`, one of [`ccp_reconnect_hosts`].
pub fn reconnect_ccp_via(auth: &ReconnectAuth, host: &str) -> io::Result<CcpReconnect> {
    let token_hash = token_short_hash(&auth.session_token);
    reconnect_ccp_attempt(auth, &token_hash, host, 0)
}

/// Hosts a CCP reconnect cycles through, as the reference does (ibx#399):
/// the primary, then its backups `{first label}-hb1` and `{first label}-hb2`
/// in the same domain (`cdc1.example` gives `cdc1-hb1.example`). An IP
/// address or a single-label name has no backups.
pub fn ccp_reconnect_hosts(host: &str) -> Vec<String> {
    let mut hosts = vec![host.to_string()];
    if host.parse::<std::net::IpAddr>().is_err()
        && let Some((label, domain)) = host.split_once('.')
        && !label.is_empty()
        && !domain.is_empty()
    {
        hosts.push(format!("{}-hb1.{}", label, domain));
        hosts.push(format!("{}-hb2.{}", label, domain));
    }
    hosts
}

/// Host of reconnect attempt `attempt` (1 for the first attempt).
pub fn ccp_reconnect_host(host: &str, attempt: u32) -> String {
    let mut hosts = ccp_reconnect_hosts(host);
    let i = (attempt.max(1) as usize - 1) % hosts.len();
    hosts.swap_remove(i)
}

fn reconnect_ccp_attempt(auth: &ReconnectAuth, token_hash: &str, host: &str, depth: u32) -> io::Result<CcpReconnect> {
    if depth > 5 {
        return Err(io::Error::new(io::ErrorKind::Other, "CCP reconnect: too many redirects"));
    }
    log::info!("CCP reconnect to {}:{} (attempt {})", host, AUTH_PORT, depth + 1);

    // TLS, with no key exchange: the reference's SSL mode (ibx#423).
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
    log::info!("CCP reconnect CONNECT_REQUEST (session={}, hash={})", auth.server_session_id, token_hash);

    // Receive AUTH_START — may get NS_REDIRECT instead
    let (auth_start, refused) = match ccp_login_start(&mut tls, &mut channel, connect_req.as_bytes()) {
        Ok(start) => start,
        Err(e) if e.to_string().starts_with("REDIRECT:") => {
            let target = e.to_string().replace("REDIRECT:", "");
            let redirect_host = target.split(':').next().unwrap_or(&target).to_string();
            log::info!("CCP reconnect redirected to {}", redirect_host);
            drop(tls);
            // Floor before following (ibx#218): this runs on the background
            // reconnect thread, and an instant re-dial chain risks the same
            // rate limiting the backoff ladder exists for.
            std::thread::sleep(Duration::from_secs(2));
            return reconnect_ccp_attempt(auth, token_hash, &redirect_host, depth + 1);
        }
        Err(e) => return Err(e),
    };

    // The soft flag of the auth start selects the session-token step, as in
    // the reference; it is read only from a real auth start (ibx#353).
    if auth_start.soft_flag != 0 {
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
        // Server requires full SRP (soft flag 0). Re-run the SRP handshake
        // with the credentials cached on ReconnectAuth — the same path
        // Gateway::connect uses on first login.
        log::info!("CCP reconnect: server requires SRP, running handshake with cached credentials");
        do_srp(&mut tls, &auth.username, &auth.password)?;
    }

    // Post-auth: wait for NS_CONNECT_RESPONSE → NEWCOMMPORTTYPE → NS_FIX_START.
    // Per-iteration read timeout aligned with the initial-connect path.
    tls.get_ref().set_read_timeout(Some(Duration::from_secs_f64(TIMEOUT_FIX_LOGON)))?;
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
            let ct = B64.decode(parts.get(2).copied().unwrap_or(""))
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
            session::send_plain(&mut tls, newcomm.as_bytes())?;
        } else if msg_type == ns::NS_FIX_START {
            fix_ready = true;
            break;
        } else if msg_type == ns::NS_ERROR_RESPONSE || msg_type == ns::NS_SECURE_ERROR {
            return Err(session::ns_error(msg_type, &inner_parts[2..]));
        }
        // Ignore 530 keepalives and other types
    }
    tls.get_ref().set_read_timeout(None)?;
    if !fix_ready {
        return Err(io::Error::new(io::ErrorKind::Other, "CCP reconnect: no FIX_START after auth"));
    }

    // FIX Logon: a reconnect of the same session carries its epoch (ibx#422).
    let logon_msg = build_ccp_reconnect_logon(&auth.hw_info, &auth.encoded, CCP_HEARTBEAT, 1, &auth.session_epoch);
    tls.write_all(&logon_msg)?;
    tls.flush()?;

    // Short poll timeout + overall deadline so a slow response segment from a
    // high-latency gateway is retried, not treated as a fatal logon failure
    // (ibx#237, same tolerance as the farm-logon path).
    tls.get_ref().set_read_timeout(Some(Duration::from_millis(FARM_LOGON_POLL_MS)))?;
    let fix_deadline = std::time::Instant::now() + Duration::from_secs_f64(TIMEOUT_FARM_LOGON);
    let mut session_epoch = None;
    for _ in 0..5 {
        let response = fix_read_deadline(&mut tls, fix_deadline)?;
        if let Some(epoch) = logon_reply_epoch(&response) {
            log::info!("CCP reconnect: session epoch {} (sent {:?})", epoch, auth.session_epoch);
            session_epoch = Some(epoch);
        }
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

    let mut conn = Connection::new(tls)?;
    conn.seq = 1; // the logon
    log::info!("CCP reconnect complete (seq={})", conn.seq);
    Ok(CcpReconnect { conn, session_epoch, ns_secure_refused: refused })
}


/// Start of a login on the auth connection, in the reference's SSL mode
/// (jts.ini `[Logon] UseSSL=true`, the setting of the captured gateway):
/// the connect request `connect_req` goes in clear inside TLS, with no key
/// exchange before it, then the auth start is read (ibx#423). The second
/// value is set when the server refused the encryption with the permission
/// to go on: the connect request was sent again in clear, and the farms
/// opened after this login skip their key exchange. Every auth login
/// (first login, reconnect, redirect) starts here.
fn ccp_login_start<S: Read + Write>(
    stream: &mut S,
    channel: &mut SecureChannel,
    connect_req: &[u8],
) -> io::Result<(session::AuthStart, bool)> {
    session::send_plain(stream, connect_req)?;
    let mut refused = false;
    let start = session::recv_auth_start_ccp(stream, channel, &mut refused, connect_req)?;
    Ok((start, refused))
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
    /// Wrapped in `Zeroizing` so the plaintext is wiped from memory on drop.
    pub password: Zeroizing<String>,
    pub host: String,
    pub paper: bool,
    /// Accept invalid TLS certificates during auth. Default: `false` (secure).
    /// Only set to `true` for local testing against self-signed gateways.
    pub accept_invalid_certs: bool,
    /// Per-session second-factor approval timeout in seconds; `0` (the
    /// default, [`session::IB_KEY_DEFAULT_TIMEOUT_SECS`]) is no client
    /// timeout, as in the reference: the wait ends with the server's answer
    /// or its close of the socket after about 18 min (ibx#208). Set a value
    /// to fail fast for unattended logins. Only consulted on non-paper
    /// logins; paper logins skip the gate entirely.
    pub ib_key_timeout_secs: u64,
    /// Override of the second-factor token sub-type sent in the SWCR_TOKEN
    /// state=1 init body (`M.D` field). Empty (the default,
    /// [`session::IB_KEY_DEFAULT_TOKEN_SUB_TYPE`]): the value comes from the
    /// second-factor list of the session's auth start, as the reference
    /// does (ibx#279). Set it only to force another value.
    pub ib_key_token_sub_type: String,
    /// If set, the IBKey gate uses the **Challenge/Response** path instead
    /// of waiting for a mobile push approval. After the server delivers
    /// state=2, the callback is invoked once with the challenge details
    /// and the returned 8-character code is submitted as state=3. See
    /// [`session::CodeProvider`] for the contract; `None` leaves behavior
    /// unchanged (push approval).
    pub code_provider: Option<session::CodeProvider>,
}

impl Gateway {
    /// Connect to IB: auth + logon + data farm connections.
    /// Returns Gateway + farm Connection + auth Connection + optional historical data Connection.
    ///
    /// While the server answers "site down" or "site not ready" the login is
    /// retried after 5 to 15 s, as the reference retries those answers; any
    /// other login error answer (bad credentials, lockout, ...) stops at once
    /// (ibx#423).
    pub fn connect(config: &GatewayConfig) -> io::Result<(Self, Connection, Connection, Option<Connection>)> {
        loop {
            match Self::connect_to_host(config, &config.host, 0) {
                Err(e) if session::login_error(&e).is_some_and(|l| l.kind.is_retryable()) => {
                    let delay = crate::engine::hot_loop::reconnect_backoff();
                    log::warn!("{}; login retried in {:?}", e, delay);
                    std::thread::sleep(delay);
                }
                result => return result,
            }
        }
    }

    /// Internal: connect to a specific host, with redirect depth tracking.
    fn connect_to_host(
        config: &GatewayConfig,
        host: &str,
        redirect_depth: u32,
    ) -> io::Result<(Self, Connection, Connection, Option<Connection>)> {
        if redirect_depth > 3 {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "Too many redirects during auth",
            ));
        }

        let hw_info = session::get_hw_info();
        // Tag 6266 carries `{jdkVer}/{platform}/{locale}/{dist}`. The locale
        // segment must be a canonical Java `Locale.toString()` value (e.g.
        // `en_US`, `fr`, `ja_JP`); bare `en` is rejected as `invalid twsInfo`.
        // `IBX_LOCALE` overrides just the locale; `IBX_ENCODED` overrides
        // the whole string for full control.
        let encoded = std::env::var("IBX_ENCODED").unwrap_or_else(|_| {
            match std::env::var("IBX_LOCALE") {
                Ok(loc) if !loc.is_empty() => format!("17.0.10.0.101/W/{}/G", loc),
                _ => IB_ENCODED.to_string(),
            }
        });

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

        // No key exchange on the TLS socket: the reference's SSL mode
        // (ibx#423).
        let mut channel = SecureChannel::new();

        // CONNECT_REQUEST, in clear inside TLS
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

        // Receive AUTH_START (may get a redirect instead for paper accounts).
        // When the server refuses the encryption with the permission to go
        // on, the farms of the session log on in clear, as the reference
        // does (ibx#423).
        let (auth_start, refused) = match ccp_login_start(&mut tls, &mut channel, connect_req.as_bytes()) {
            Ok(start) => start,
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

        // Authentication
        log::info!("Starting auth for {}", config.username);
        let session_key = do_srp(&mut tls, &config.username, &config.password)?;
        log::info!("Auth complete");

        // Per-session second-factor approval gate (IBKey / seamless push).
        // Skipped on paper logins; live logins enter a wait state if the
        // account has a second factor configured server-side.
        // Captures the SOFT session token from AUTH_FINISH PASSED — this is
        // the token used for downstream farm logons (NOT the SRP session_key).
        let mut soft_token: Option<BigUint> = None;
        // The second factor and its token sub-type come from the auth start
        // of this session; the config value only overrides the sub-type
        // (ibx#279). An empty list means no second factor, as in the
        // reference.
        let second_factor = if config.paper {
            None
        } else {
            let token = auth_start.mobile_key_token(&config.ib_key_token_sub_type)?;
            if token.is_none() {
                log::info!("Auth start lists no second factor: none required");
            }
            token
        };
        if let Some(token_sub_type) = second_factor {
            // No client deadline unless one is set, as in the reference: the
            // wait ends with the server's answer or its close of the socket
            // (ibx#208).
            let deadline = session::ib_key_deadline(config.ib_key_timeout_secs);
            let bound = if deadline.is_some() {
                format!("up to {}s", config.ib_key_timeout_secs)
            } else {
                "until the server answers or closes (about 18 min)".to_string()
            };
            // Live logins enter a human-approval window here: connect() blocks
            // until the second factor is approved (mobile push) or the wait
            // ends. Announce it up front so a stalled connect() reads as
            // "waiting for approval" rather than a hang (ibx#203 / ibx#207).
            // Accounts with no second factor fall straight through (Skipped).
            if config.code_provider.is_none() {
                log::info!(
                    "Live login for {}: waiting for second-factor approval (mobile push);                      connect() blocks {}. Use paper=true, an ib_key_timeout_secs,                      or a code_provider to avoid this.",
                    config.username, bound,
                );
            } else {
                log::info!(
                    "Live login for {}: second-factor via code_provider (Challenge/Response);                      connect() blocks {} awaiting the challenge.",
                    config.username, bound,
                );
            }
            // Short read timeout: the wait checks the code provider and the
            // deadline between reads (ibx#244).
            tls.get_ref().set_read_timeout(Some(Duration::from_millis(FARM_LOGON_POLL_MS)))?;
            match session::do_ib_key_2fa(
                &mut tls,
                &token_sub_type,
                deadline,
                config.code_provider.as_ref(),
            )? {
                session::IbKeyOutcome::Skipped => {
                    log::info!("2FA gate: skipped (no second factor)");
                }
                session::IbKeyOutcome::Approved { approval_url, session_id, soft_token_hex } => {
                    log::info!(
                        "2FA gate: approved (session_id={}, approval_url={}, token_hex_len={})",
                        if session_id.is_empty() { "<none>" } else { &session_id },
                        if approval_url.is_empty() { "<none>" } else { &approval_url },
                        soft_token_hex.len(),
                    );
                    if !soft_token_hex.is_empty() {
                        if let Some(tok) = BigUint::parse_bytes(soft_token_hex.as_bytes(), 16) {
                            soft_token = Some(tok);
                        } else {
                            log::warn!("2FA gate: SOFT token hex did not parse — falling back to session_key");
                        }
                    }
                }
            }
        }

        // Receive post-auth messages (encrypted via 534) and wait for the
        // data-farm start (NS_FIX_START). A transient stall here must not be
        // fatal: a single read timeout used to `break` and bubble a hard error
        // even though the data start was still pending, and keepalive chatter
        // could exhaust a fixed iteration budget before it arrived (ibx#196).
        // Retry within an overall deadline and ignore intervening messages,
        // mirroring the CCP-reconnect path.
        tls.get_ref().set_read_timeout(Some(Duration::from_secs_f64(TIMEOUT_FIX_LOGON)))?;
        let fix_deadline = std::time::Instant::now()
            + std::time::Duration::from_secs_f64(TIMEOUT_FIX_LOGON * 2.0);
        let mut fix_ready = false;
        while std::time::Instant::now() < fix_deadline {
            let (payload, _) = match ns::ns_recv(&mut tls) {
                Ok(r) => r,
                Err(e)
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::TimedOut =>
                {
                    log::warn!("Post-auth recv timeout, retrying until deadline: {}", e);
                    continue;
                }
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
                let ct = B64.decode(parts.get(2).copied().unwrap_or(""))
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
                channel.decrypt(&ct)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?
            } else if raw_type == ns::NS_SECURE_ERROR || raw_type == ns::NS_ERROR_RESPONSE {
                return Err(session::ns_error(raw_type, &parts[2..]));
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
                // Type only: the connect response carries the session log
                // key, so its text is not logged (ibx#283).
                log::info!("Post-auth: connect response received");
                // Send port type change (required before data start)
                let newcomm = format!("{};{};0;;2;0;", NS_VERSION_MIN, ns::NS_NEWCOMMPORTTYPE);
                session::send_plain(&mut tls, newcomm.as_bytes())?;
                log::info!("Port type change sent");
            } else if msg_type == ns::NS_FIX_START {
                log::info!("Data start: {}", inner_text);
                fix_ready = true;
                break;
            } else if msg_type == ns::NS_ERROR_RESPONSE || msg_type == ns::NS_SECURE_ERROR {
                return Err(session::ns_error(msg_type, &inner_parts[2..]));
            } else if msg_type == ns::NS_BACKUP_HOST {
                log::info!("Backup host notice received (ignored)");
            } else {
                log::info!("Post-auth msg type={} (ignored)", msg_type);
            }
        }
        if !fix_ready {
            // TimedOut (not Other) so callers can distinguish a transient
            // post-auth handshake miss — which is retryable — from a genuine
            // auth failure (ibx#196).
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Never received data start after auth",
            ));
        }

        // --- Phase 2: Auth server logon (over TLS) ---
        let logon_msg = build_ccp_logon(&hw_info, &encoded, CCP_HEARTBEAT, 1);
        log::info!("Sending auth logon ({} bytes)", logon_msg.len());
        tls.write_all(&logon_msg)?;
        tls.flush()?;

        // Read FIX messages until we get the logon ACK (35=A) with session info.
        // Short poll timeout + overall deadline so a slow ACK segment from a
        // high-latency gateway is retried, not fatal (ibx#237).
        tls.get_ref().set_read_timeout(Some(Duration::from_millis(FARM_LOGON_POLL_MS)))?;
        let ack_deadline = std::time::Instant::now() + Duration::from_secs_f64(TIMEOUT_FARM_LOGON);
        let mut account_id = String::new();
        let mut heartbeat_interval = CCP_HEARTBEAT;
        let mut server_session_id = String::new();
        let mut settings_object_key = String::new();
        let mut session_epoch = String::new();
        let mut raw_soft_dollar_tiers = String::new();
        let mut raw_family_codes = String::new();
        let mut raw_news_providers = String::new();
        let mut raw_news_sources = String::new();
        let mut raw_news_capabilities = String::new();
        let mut deny_news = false;
        let mut white_branding_id = String::new();
        let mut fa_session = false;
        let mut super_user = false;
        let mut omnibus = false;
        let mut scale_us_lots = false;
        // Tick-by-tick limit fields, first value seen (ibx#455).
        let mut tbt_limit_fields: [Option<String>; 4] = Default::default();
        // Logon values of the depth limit (#452).
        let mut depth_limit_fields: [Option<String>; 5] = Default::default();
        let mut tick_by_tick_off = false;
        let mut price_mgmt = false;
        let mut price_mgmt_exclusions: Option<String> = None;
        // Logon values of the real-time bar limit (ibx#454).
        let mut ticker_limit_tags: std::collections::HashMap<u32, i64> = std::collections::HashMap::new();
        let mut raw_misc_urls = String::new();
        // Per ib-agent#128: the auth-logon ACK tells us which farms this
        // account is routed to. Hardcoding `usfarm`/`ushmds` only works for
        // US accounts; EU accounts need eufarm/euhmds/secdefeu, etc.
        // Format of 6145: "<host>/<farm>"; 6171/8008: "<host>/<farm>/<port>"
        let mut trading_route = String::new();    // tag 6145
        let mut mktdata_route = String::new();    // tag 6171
        let mut secdef_route  = String::new();    // tag 8008

        for _ in 0..5 {
            let raw_response = fix_read_deadline(&mut tls, ack_deadline)?;
            // The auth-logon ACK arrives as `8=FIXCOMP` with a DEFLATE-
            // compressed inner body containing the per-account routing tags
            // (6145/6171/8008) and other init data. Inflate before parsing.
            // (See ib-agent#128 + #129.)
            let mut response = raw_response.clone();
            if raw_response.starts_with(b"8=FIXCOMP\x01") {
                let inflated_msgs = fixcomp::fixcomp_decompress(&raw_response)?;
                let total: usize = inflated_msgs.iter().map(|m| m.len()).sum();
                log::info!("Auth FIXCOMP envelope: {} bytes compressed → {} inner messages, ~{} inflated bytes",
                    raw_response.len(), inflated_msgs.len(), total);
                // Concatenate all inner messages so a single fix_parse pass
                // sees every tag.
                response.clear();
                for inner in inflated_msgs {
                    response.extend_from_slice(&inner);
                    response.push(b'\x01');
                }
            }
            let fields = fix_parse(&response);
            let msg_type = fields.get(&35).map(|s| s.as_str()).unwrap_or("");
            log::info!("Auth msg type={} ({} bytes raw / {} bytes parsed)",
                msg_type, raw_response.len(), response.len());
            for tag in [6144u32, 6145, 6146, 6147, 6171, 6172, 8008, 8009, 6160, 6161] {
                if let Some(v) = fields.get(&tag) {
                    log::info!("Auth msg type={} tag={}: {:?}", msg_type, tag, v);
                }
            }

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
                if settings_object_key.is_empty() {
                    settings_object_key = v.clone();
                    log::info!("Auth: settings object key (6386, len={})", settings_object_key.len());
                }
            }
            if let Some(v) = fields.get(&TAG_SESSION_EPOCH).filter(|v| !v.is_empty())
                && session_epoch.is_empty()
            {
                session_epoch = v.clone();
                log::info!("Auth: session epoch {}", session_epoch);
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

            // Farm routing (per ib-agent#128) — server tells us which farms
            // this account is permissioned for. EU accounts get `eufarm`,
            // US get `usfarm`, etc. Read once from whichever auth msg has it.
            if let Some(v) = fields.get(&6145) {
                if trading_route.is_empty() {
                    trading_route = v.clone();
                    log::info!("Auth: trading farm route = {}", trading_route);
                }
            }
            if let Some(v) = fields.get(&6171) {
                if mktdata_route.is_empty() {
                    mktdata_route = v.clone();
                    log::info!("Auth: market-data farm route = {}", mktdata_route);
                }
            }
            if let Some(v) = fields.get(&8008) {
                if secdef_route.is_empty() {
                    secdef_route = v.clone();
                    log::info!("Auth: secdef farm route = {}", secdef_route);
                }
            }

            // Gateway-local init data from logon response
            if let Some(v) = fields.get(&6522) {
                if raw_soft_dollar_tiers.is_empty() { raw_soft_dollar_tiers = v.clone(); }
            }
            if let Some(v) = fields.get(&6823) {
                if raw_family_codes.is_empty() { raw_family_codes = v.clone(); }
            }
            if let Some(v) = fields.get(&6830) {
                if raw_news_providers.is_empty() { raw_news_providers = v.clone(); }
            }
            if let Some(v) = fields.get(&6988) {
                if raw_news_sources.is_empty() { raw_news_sources = v.clone(); }
            }
            if let Some(v) = fields.get(&6969) {
                if raw_news_capabilities.is_empty() { raw_news_capabilities = v.clone(); }
            }
            // whiteBrandingId: logon tag 6593, as the reference (ibx#483).
            if let Some(v) = fields.get(&6593) {
                if white_branding_id.is_empty() { white_branding_id = v.clone(); }
            }
            // FA session: true only for the single character "1", as the
            // reference reads FIX booleans (ibx#481).
            if let Some(v) = fields.get(&6108) {
                fa_session |= v == "1";
            }
            // Super user and omnibus, read the same way (ibx#417).
            super_user |= fields.get(&6130).is_some_and(|v| v == "1");
            omnibus |= fields.get(&9826).is_some_and(|v| v == "1");
            if let Some(v) = fields.get(&6542) {
                scale_us_lots |= features_scale_us_lots(v);
                tick_by_tick_off |= features_have(v, "NOTICKBYTICK");
                price_mgmt |= features_have(v, "PRICEMGMT");
                deny_news |= features_have(v, "DENYNEWS");
            }
            if price_mgmt_exclusions.is_none() {
                price_mgmt_exclusions = fields.get(&8146).cloned();
            }
            for (slot, tag) in tbt_limit_fields.iter_mut().zip([8421u32, 8422, 6594, 6848]) {
                if slot.is_none() { *slot = fields.get(&tag).cloned(); }
            }
            for (slot, tag) in depth_limit_fields.iter_mut().zip([6849u32, 6848, 8421, 8422, 6594]) {
                if slot.is_none() { *slot = fields.get(&tag).cloned(); }
            }
            for tag in [6847u32, 6846, 8421, 8422, 6083] {
                if let Some(n) = fields.get(&tag).and_then(|v| v.trim().parse::<i64>().ok()) {
                    ticker_limit_tags.entry(tag).or_insert(n);
                }
            }
            // Tag 6321: PRIV_LAB_MISC_URLS — try parsed fields first, then raw byte search.
            // Mirrors the 8035 defensive scan because the value can carry `|` separators
            // that confuse downstream parsers if a chunk is fragmented.
            if raw_misc_urls.is_empty() {
                if let Some(v) = fields.get(&6321) {
                    raw_misc_urls = v.clone();
                    log::info!("Found misc URLs from logon ACK ({} bytes)", raw_misc_urls.len());
                } else {
                    let marker = b"\x016321=";
                    if let Some(pos) = response.windows(marker.len()).position(|w| w == marker) {
                        let val_start = pos + marker.len();
                        if let Some(end) = response[val_start..].iter().position(|&b| b == SOH) {
                            raw_misc_urls = String::from_utf8_lossy(
                                &response[val_start..val_start + end],
                            ).to_string();
                            log::info!("Found misc URLs from logon ACK byte scan ({} bytes)", raw_misc_urls.len());
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

        let max_real_time_requests = max_real_time_requests(&ticker_limit_tags);
        log::info!(
            "Auth logon: account={} session_id={} hb={}s scale_us_lots={} max_real_time_requests={}",
            account_id, server_session_id, heartbeat_interval, scale_us_lots, max_real_time_requests
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
        for _ in 0..92 {
            send_init(&[(35, "U"), (52, &now), (6040, "80")])?;
        }
        // The algo definitions the reference asks for stock algos (ibx#263).
        for key in crate::control::algo::DEFINITION_KEYS {
            send_init(&[(35, "U"), (52, &now), (6040, "53"), (6364, key)])?;
        }
        tls.flush()?;
        log::info!("Init sequence sent ({} messages, seq now {})", ccp_seq - 1, ccp_seq);

        // Drain init responses — extract account ID + farm routing tags.
        // Per ib-agent#134 read-throughput investigation (2026-05-05):
        // the burst's bulk (~28 kB compressed) arrives in ~300 ms continuous,
        // after which the server emits 67-byte keep-alive trickles every ~10 s
        // until it FINs the socket at ~140 s. A 300 ms idle-gap is past any
        // intra-burst jitter (the burst is continuous) and well short of the
        // 10 s keep-alive trickle interval, so we exit promptly after burst-end.
        tls.get_ref().set_read_timeout(Some(Duration::from_millis(300)))?;
        let mut init_data: Vec<u8> = Vec::with_capacity(65536);
        let mut tmp_buf = vec![0u8; 65536];
        let read_start = std::time::Instant::now();
        loop {
            match tls.read(&mut tmp_buf) {
                Ok(0) => break,
                Ok(n) => init_data.extend_from_slice(&tmp_buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock
                    || e.kind() == io::ErrorKind::TimedOut =>
                {
                    // First 1-s idle gap = burst is done. Anything past
                    // this is the server's 10-s keep-alive trickle, which
                    // we don't want to drain (would push grace-window
                    // messages past the server-side deadline).
                    break;
                }
                Err(e) => return Err(e),
            }
        }
        log::info!(
            "Init response: {} bytes in {:?}",
            init_data.len(), read_start.elapsed(),
        );

        // The auth-server's logon ACK arrives DEFLATE-compressed inside one or
        // more `8=FIXCOMP` envelopes (per ib-agent#129); the routing tags are
        // in the inflated content. The scan reads a copy with that content
        // appended; `init_data` itself seeds the connection buffer below
        // unchanged (ibx#317).
        let scan_data = init_scan_buffer(&init_data);

        // Scan init response for account ID and gateway-local init tags
        let init_str = String::from_utf8_lossy(&scan_data);
        let account_config = parse_account_config(&init_str);
        let algo_definitions = parse_algo_definitions(&init_str);
        log::info!("Algo definitions in the login burst: {}", algo_definitions.len());
        match &account_config {
            Some((features, mifid)) => log::info!("Account config: features {:?}, MiFID config {:?}", features, mifid),
            None => log::warn!("No account config answer in the login burst"),
        }
        // TEMP diagnostic (ib-agent#128 follow-up): log every part containing
        // "farm" or "hmds" so we can locate the routing tags.
        for part in init_str.split('\x01') {
            if part.contains("farm") || part.contains("hmds") || part.contains("secdef") {
                log::info!("Init scan: routing-shaped part = {:?}", part);
            }
        }
        for part in init_str.split('\x01') {
            if part.starts_with("1=") && part.len() > 2 {
                let val = &part[2..];
                if val.starts_with("DU") || val.starts_with("DF") || val.starts_with("U") {
                    if account_id.is_empty() || account_id == config.username {
                        account_id = val.to_string();
                        log::info!("Found account ID from init response: {}", account_id);
                    }
                }
            } else if part.starts_with("6522=") && raw_soft_dollar_tiers.is_empty() {
                raw_soft_dollar_tiers = part[5..].to_string();
                log::info!("Found soft dollar tiers from init response ({} bytes)", raw_soft_dollar_tiers.len());
            } else if part.starts_with("6823=") && raw_family_codes.is_empty() {
                raw_family_codes = part[5..].to_string();
                log::info!("Found family codes from init response ({} bytes)", raw_family_codes.len());
            } else if part.starts_with("6830=") && raw_news_providers.is_empty() {
                raw_news_providers = part[5..].to_string();
                log::info!("Found news providers from init response ({} bytes)", raw_news_providers.len());
            } else if part.starts_with("6988=") && raw_news_sources.is_empty() {
                raw_news_sources = part[5..].to_string();
                log::info!("Found API news sources from init response ({} bytes)", raw_news_sources.len());
            } else if part.starts_with("6969=") && raw_news_capabilities.is_empty() {
                raw_news_capabilities = part[5..].to_string();
                log::info!("Found news capabilities from init response ({} bytes)", raw_news_capabilities.len());
            } else if part == "6108=1" {
                fa_session = true;
            } else if part == "6130=1" {
                super_user = true;
            } else if part == "9826=1" {
                omnibus = true;
            } else if let Some(id) = white_branding_part(part).filter(|_| white_branding_id.is_empty()) {
                white_branding_id = id.to_string();
                log::info!("Found white branding ID from init response");
            } else if part.starts_with("6321=") && raw_misc_urls.is_empty() {
                raw_misc_urls = part[5..].to_string();
                log::info!("Found misc URLs from init response ({} bytes)", raw_misc_urls.len());
            } else if part.starts_with("6145=") && trading_route.is_empty() {
                trading_route = part[5..].to_string();
                log::info!("Found trading farm route in init response: {}", trading_route);
            } else if part.starts_with("6171=") && mktdata_route.is_empty() {
                mktdata_route = part[5..].to_string();
                log::info!("Found market-data farm route in init response: {}", mktdata_route);
            } else if part.starts_with("8008=") && secdef_route.is_empty() {
                secdef_route = part[5..].to_string();
                log::info!("Found secdef farm route in init response: {}", secdef_route);
            }
        }

        // Per ib-agent#134: CCP server FINs the connection ~12s after the
        // init-burst response if no application-level traffic arrives in the
        // grace window — heartbeats alone do not satisfy "client alive".
        // Send Account-Register (35=U|6040=6, account in tag 6095) followed
        // by a wildcard OrderStatusRequest (35=H|11=*|55=*|54=*) right after
        // the inbound burst-end, before farm logons begin. Both are sent in
        // plain FIX over TLS (the CCP socket has no AES/HMAC envelope at
        // this stage; encryption is set up only after `Connection::new`).
        let post_burst_account = if account_id.is_empty() {
            config.username.clone()
        } else {
            account_id.clone()
        };
        let post_burst_now = chrono_free_timestamp();
        ccp_seq += 1;
        let ar_msg = fix_build(
            &[
                (35, "U"),
                (52, &post_burst_now),
                (6040, "6"),
                (6036, "1"),
                (6529, "AR.1"),
                (6095, &post_burst_account),
            ],
            ccp_seq,
        );
        tls.write_all(&ar_msg)?;
        ccp_seq += 1;
        let osr_msg = fix_build(
            &[
                (35, "H"),
                (52, &post_burst_now),
                (11, "*"),
                (55, "*"),
                (54, "*"),
            ],
            ccp_seq,
        );
        tls.write_all(&osr_msg)?;
        ccp_seq += 1;
        // PortfolioLoginRequest — third post-burst app message in the Java
        // capture (tag34=104). Account goes in tag 1 here, not 6095.
        let plr_msg = fix_build(
            &[
                (35, "U"),
                (52, &post_burst_now),
                (6040, "142"),
                (6529, "PLR.1"),
                (1, &post_burst_account),
            ],
            ccp_seq,
        );
        tls.write_all(&plr_msg)?;
        ccp_seq += 1;
        // DataRequest — Java tag34=105: `1={acc}|6712=1|6556=DR.{N}`
        let dr_msg = fix_build(
            &[
                (35, "U"),
                (52, &post_burst_now),
                (6040, "91"),
                (1, &post_burst_account),
                (6712, "1"),
                (6556, "DR.2"),
            ],
            ccp_seq,
        );
        tls.write_all(&dr_msg)?;
        ccp_seq += 1;
        // 6040=74 — Java tag34=106: `1={acc}|6700=Core|6544=2`
        let core_msg = fix_build(
            &[
                (35, "U"),
                (52, &post_burst_now),
                (6040, "74"),
                (1, &post_burst_account),
                (6700, "Core"),
                (6544, "2"),
            ],
            ccp_seq,
        );
        tls.write_all(&core_msg)?;
        tls.flush()?;
        log::info!(
            "CCP post-burst grace messages sent (AR+H+PLR+DR+74), seq now {}",
            ccp_seq
        );

        tls.get_ref().set_read_timeout(None)?;

        // Auth connection (non-blocking TLS for hot loop)
        let mut ccp_conn = Connection::new(tls)?;
        ccp_conn.seq = ccp_seq;
        // No key exchange on the auth connection, so no signing key: its
        // messages go unsigned, as the reference's (ibx#423).
        let ccp_sign_key = Vec::new();
        let ccp_sign_iv = Vec::new();
        // Seed init burst into connection buffer so the hot loop processes 8=O account data
        ccp_conn.seed_buffer(&init_data);

        // --- Phase 3: Data farm connections ---
        // Per ib-agent#143/#144/#145: the official Gateway opens exactly 3 authed TCP
        // sessions per login — MARKET_DATA (tag 6145), HISTORICAL_DATA (tag 6171), and
        // SECDEFARM (tag 8008, UI/telemetry only — not used by ibx). Per ib-agent#125/
        // #131/#133: the SOFT token is `SHA1(strip(S))` where S is the SRP shared
        // secret. `do_srp` returns exactly that via `srp_compute_k`, so `session_key`
        // IS the SOFT token — no further hashing. (Tag 8483's per-channel SHA1 is
        // added by `token_short_hash` at the build-logon site.) Tag 6386 is an S3
        // object key, not a token source.
        let farm_token: BigUint = soft_token.clone().unwrap_or_else(|| session_key.clone());
        // Per ib-agent#128: read the farm names from the auth-server's
        // routing tags rather than hardcoding `usfarm`/`ushmds`. EU accounts
        // are routed to `eufarm`/`euhmds`/`secdefeu`, US to `usfarm`/`ushmds`,
        // etc. Format of the route strings:
        //   trading (6145):  "<host>/<farm>"            (port from tag 6146, default 4000)
        //   mktdata (6171):  "<host>/<farm>/<port>"
        //   secdef  (8008):  "<host>/<farm>/<port>"
        let (trading_host, trading_farm) = parse_farm_route(&trading_route)
            .unwrap_or_else(|| (host.to_string(), "usfarm".to_string()));
        let (mktdata_host, mktdata_farm) = parse_farm_route(&mktdata_route)
            .map(|(h, f)| (h, f))
            .unwrap_or_else(|| (host.to_string(), "ushmds".to_string()));
        log::info!("Farm routing: trading={}/{}, mktdata={}/{}",
            trading_host, trading_farm, mktdata_host, mktdata_farm);

        // Retain HMDS routing for the reconnect loop (ibx#187) — the values
        // below are moved into the thread::scope closures.
        let hmds_host_for_gw = mktdata_host.clone();
        let hmds_farm_for_gw = mktdata_farm.clone();
        let farm_name = trading_farm.clone();
        let farm_host = trading_host.clone();

        // Parallel farm logons: validated against paper and live (each farm
        // logon is ~6 s sequentially; running them in parallel halves the
        // farm-logon phase). Both servers accept concurrent logons with the
        // same credentials — see examples/ex_parallel_farm_logon.rs.
        let (farm_conn, hmds_conn) = std::thread::scope(|scope| {
            let username = &config.username;
            let password = &*config.password;
            let paper = config.paper;
            let ssid = &server_session_id;
            let token = &farm_token;
            let hw = &hw_info;
            let enc = &encoded;
            let trading_handle = scope.spawn(move || {
                connect_farm_opts(&trading_host, &trading_farm, username, password,
                    paper, ssid, token, hw, enc, 18, true, !refused)
            });
            let mktdata_handle = scope.spawn(move || {
                connect_farm_opts(&mktdata_host, &mktdata_farm, username, password,
                    paper, ssid, token, hw, enc, 17, true, !refused)
            });
            let trading = trading_handle.join().expect("trading farm thread panicked");
            let mktdata = mktdata_handle.join().expect("mktdata farm thread panicked");
            (trading, mktdata)
        });
        let (farm_conn, md_routing) = farm_conn?;
        let (hmds_conn, hmds_routing) = match hmds_conn {
            Ok((c, table)) => { log::info!("Historical data farm connected"); (Some(c), table) }
            Err(e) => { log::warn!("Historical data farm connection failed (non-fatal): {}", e); (None, None) }
        };

        let gw = Gateway {
            account_id: if account_id.is_empty() { config.username.clone() } else { account_id },
            session_token: session_key,
            server_session_id,
            settings_object_key,
            heartbeat_interval,
            hw_info,
            encoded,
            raw_soft_dollar_tiers,
            raw_family_codes,
            raw_news_providers,
            raw_news_sources,
            raw_news_capabilities,
            deny_news,
            white_branding_id,
            fa_session,
            super_user,
            omnibus,
            account_config,
            algo_definitions,
            scale_us_lots,
            tick_by_tick_limit: tick_by_tick_limit(&tbt_limit_fields),
            depth_limit: depth_limit(&depth_limit_fields),
            tick_by_tick_off,
            price_mgmt,
            price_mgmt_exclusions,
            max_real_time_requests,
            misc_urls: parse_misc_urls(&raw_misc_urls),
            ccp_sign_key,
            ccp_sign_iv,
            hmds_host: hmds_host_for_gw,
            hmds_farm: hmds_farm_for_gw,
            session_epoch,
            farm_name,
            farm_host,
            md_routing,
            hmds_routing,
            ns_secure_refused: refused,
        };
        Ok((gw, farm_conn, ccp_conn, hmds_conn))
    }

    /// Populate shared state with gateway-local init data parsed from CCP logon.
    pub fn populate_init_data(&self, shared: &SharedState) {
        use crate::types::FamilyCode;

        // Smart components are not logon data: the exchange map of each BBO
        // exchange comes with market data (ibx#441).

        // News providers: the API source list of the logon (ibx#460).
        let sources = if self.deny_news {
            log::info!("News denied by the logon feature list: no news provider");
            Vec::new()
        } else {
            parse_news_sources(&self.raw_news_sources)
        };
        if sources.is_empty() && !self.deny_news {
            log::warn!("No API news source in the logon: the news provider list is empty");
        }
        let news_providers = news_providers_from_logon(&sources, &self.raw_news_providers, &self.raw_news_capabilities);
        shared.reference.set_news_sources(
            sources.iter().filter(|s| s.subscribed).map(|s| s.code.clone()).collect(),
        );
        shared.reference.set_news_providers(news_providers);

        // Soft dollar tiers: from CCP logon tag 6522, none when it is absent
        // (ibx#480).
        shared.reference.set_soft_dollar_tiers(parse_soft_dollar_tiers(&self.raw_soft_dollar_tiers));

        // Family codes: parse from CCP logon tag 6823, answered as the
        // reference (ibx#441).
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
        shared.reference.set_family_codes(family_codes_answer(codes));

        // White branding ID (empty for standard accounts).
        shared.reference.set_white_branding_id(self.white_branding_id.clone());
        shared.reference.set_fa_session(self.fa_session);
        shared.reference.set_short_sale_flags(self.super_user, self.omnibus);
        shared.reference.set_tick_by_tick_limits(self.tick_by_tick_limit, self.tick_by_tick_off);
        // The snapshot rate limit is the API ticker limit (ibx#446).
        shared.reference.set_snapshot_rate_limit(self.max_real_time_requests);
        for xml in &self.algo_definitions {
            shared.reference.add_algo_definitions(xml);
        }
        if let Some((features, mifid)) = &self.account_config {
            shared.reference.set_account_config(features.clone(), mifid.clone());
        }

        // Webapp-REST-facing fields from the FIX logon roundtrip.
        shared.reference.set_ccp_session_id(self.server_session_id.clone());
        shared.reference.set_misc_urls(self.misc_urls.clone());
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
        self.into_hot_loop_with_farms(shared, event_tx, farm_conn, ccp_conn, hmds_conn, core_id)
    }

    /// Create the control channel and build a HotLoop with farm connections.
    pub fn into_hot_loop_with_farms(
        self,
        shared: Arc<SharedState>,
        event_tx: Option<Sender<Event>>,
        farm_conn: Connection,
        ccp_conn: Connection,
        hmds_conn: Option<Connection>,
        core_id: Option<usize>,
    ) -> (HotLoop, Sender<ControlCommand>) {
        let (tx, rx) = bounded(64);
        let reconnect_auth = ReconnectAuth {
            host: String::new(), // Filled by caller (Python EClient or Rust API)
            username: String::new(), // Filled by caller
            password: Zeroizing::new(String::new()), // Filled by caller
            paper: false, // Filled by caller
            session_key: self.session_token.clone(),
            session_token: self.session_token.clone(),
            server_session_id: self.server_session_id.clone(),
            hw_info: self.hw_info.clone(),
            encoded: self.encoded.clone(),
            hmds_host: self.hmds_host.clone(),
            hmds_farm: self.hmds_farm.clone(),
            farm_host: self.farm_host.clone(),
            farm_name: self.farm_name.clone(),
            session_epoch: self.session_epoch.clone(),
            ns_secure_refused: self.ns_secure_refused,
        };
        if let Some(tx) = event_tx.as_ref() {
            let _ = tx.send(Event::GatewayLogon {
                ccp_session_id: self.server_session_id.clone(),
                misc_urls: self.misc_urls.clone(),
            });
        }
        let mut hot_loop = HotLoop::new(shared, event_tx, core_id);
        hot_loop.set_control_rx(rx);
        hot_loop.set_account_id(self.account_id.clone());
        hot_loop.set_scale_us_lots(self.scale_us_lots);
        hot_loop.set_price_mgmt(self.price_mgmt, self.price_mgmt_exclusions.as_deref());
        hot_loop.set_max_real_time_requests(self.max_real_time_requests);
        hot_loop.set_depth_limit(self.depth_limit);
        hot_loop.set_farm_name(self.farm_name.clone());
        hot_loop.set_reconnect_auth(reconnect_auth);
        hot_loop.farm_conn = Some(farm_conn);
        hot_loop.ccp_conn = Some(ccp_conn);
        hot_loop.ccp.ccp_sign_key = self.ccp_sign_key.clone();
        hot_loop.ccp.ccp_sign_iv = std::sync::Mutex::new(self.ccp_sign_iv.clone());
        hot_loop.hmds_conn = hmds_conn;
        if let Some(text) = &self.md_routing {
            hot_loop.set_routing_table(crate::engine::routing::TableKind::MarketData, text);
        }
        if let Some(text) = &self.hmds_routing {
            hot_loop.set_routing_table(crate::engine::routing::TableKind::Historical, text);
        }
        (hot_loop, tx)
    }
}

/// Build market data subscription request.
pub fn build_mktdata_subscribe(
    con_id: i64,
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

/// The algo definition answers of the login burst (ibx#263): the XML of
/// each.
fn parse_algo_definitions(init: &str) -> Vec<String> {
    init.split("8=FIX").filter(|frame| frame.contains("\x016040=54\x01")).filter_map(|frame| {
        frame.split('\x01').find_map(|p| p.strip_prefix("6118=")).map(String::from)
    }).collect()
}

/// The init burst as the logon tag scan reads it: the received bytes, then
/// the inflated content of every `8=FIXCOMP` frame in them (ib-agent#129).
/// The compressed body is ~30 kB on the wire and expands to ~48 kB plaintext
/// holding the routing tags 6145/6171/8008.
///
/// The received bytes also seed the connection buffer, where the hot loop
/// inflates the compressed frames itself. The copy must stay out of it: with
/// the inflated content appended there, every compressed message of the init
/// burst was handled twice, executions included (ibx#317).
/// The account config answer (35=U 6040=210) of the login burst: its
/// feature list (6542, comma separated) and MiFID config id (8234), as the
/// reference reads them (ibx#425). Paper answer, 15/06/2026:
/// `6040=210|6556=AcctConfig4|1=DU...|6542=OLP,EUCOSTCALC,EUILLS`.
fn parse_account_config(init: &str) -> Option<(Vec<String>, String)> {
    init.split("8=FIX").find(|frame| frame.contains("\x016040=210\x01")).map(|frame| {
        let field = |tag: &str| frame.split('\x01').find_map(|p| p.strip_prefix(tag)).unwrap_or("");
        let features = field("6542=").split(',').filter(|f| !f.is_empty()).map(String::from).collect();
        (features, field("8234=").to_string())
    })
}

/// Most real-time bar requests at once, from the logon values, in the
/// order of preference of the reference (ibx#454); 40 when the logon
/// gives none.
fn max_real_time_requests(tags: &std::collections::HashMap<u32, i64>) -> u32 {
    let positive = |t: u32| tags.get(&t).copied().filter(|n| *n > 0);
    let n = positive(6847)
        .or_else(|| positive(6846))
        .or_else(|| match (tags.get(&8421), tags.contains_key(&8422)) {
            (Some(n), true) => Some(*n),
            _ => None,
        })
        .or_else(|| positive(6083));
    match n {
        Some(n) => n.clamp(0, u32::MAX as i64) as u32,
        None => crate::engine::hot_loop::hmds::DEFAULT_MAX_REAL_TIME_REQUESTS,
    }
}

/// The logon feature list turns on US stock sizes in round lots
/// (ibx#287): one of its comma separated tokens is SCALEUSLOT.
fn features_scale_us_lots(features: &str) -> bool {
    features_have(features, "SCALEUSLOT")
}

/// One of the comma separated tokens of a feature list is `feature`.
fn features_have(features: &str, feature: &str) -> bool {
    features.split(',').any(|f| f == feature)
}

/// Most contracts with tick-by-tick data at once (ibx#455), as the
/// reference reads its logon limit fields, given in the order the loop
/// collects them: the second field when the first two are both present,
/// else the third; the fourth when that one is missing or negative; at
/// least 3, and 3 when none is given.
/// The API depth limit of the logon, as the reference reads it (#452): the
/// API value, else the total value; with neither, the deep slot count (the
/// second of the two slot values when both are given, else the older one),
/// at least 3.
fn depth_limit(fields: &[Option<String>; 5]) -> usize {
    let int = |v: &Option<String>| v.as_deref().and_then(|s| s.trim().parse::<i64>().ok());
    let value = match (int(&fields[0]), int(&fields[1])) {
        (Some(api), _) => Some(api),
        (None, Some(total)) => Some(total),
        (None, None) => {
            let slots = if fields[2].is_some() && fields[3].is_some() { int(&fields[3]) } else { int(&fields[4]) };
            return slots.map_or(3, |v| v.max(3) as usize);
        }
    };
    value.map_or(3, |v| v.max(0) as usize)
}

fn tick_by_tick_limit(fields: &[Option<String>; 4]) -> usize {
    let int = |v: &Option<String>| v.as_deref().and_then(|s| s.trim().parse::<i64>().ok());
    let mut value = if fields[0].is_some() && fields[1].is_some() { int(&fields[1]) } else { int(&fields[2]) };
    if value.is_none_or(|v| v < 0) {
        value = int(&fields[3]);
    }
    value.map_or(3, |v| v.max(3) as usize)
}

/// The whiteBrandingId in one field of the logon data: tag 6593, as the
/// reference (`jfix.d0.a(e3)@13-19`). Tag 6571 is an order attribute there,
/// never a logon one (ibx#483).
fn white_branding_part(part: &str) -> Option<&str> {
    part.strip_prefix("6593=")
}

fn init_scan_buffer(init_data: &[u8]) -> Vec<u8> {
    let mut inflated_extra: Vec<u8> = Vec::new();
    let mut cursor = 0usize;
    while cursor + 12 < init_data.len() {
        if init_data[cursor..].starts_with(b"8=FIXCOMP\x01") {
            if let Some(total_len) = fixcomp::fixcomp_length(&init_data[cursor..]) {
                let segment = &init_data[cursor..cursor + total_len.min(init_data.len() - cursor)];
                let inflated = fixcomp::fixcomp_decompress(segment).unwrap_or_else(|e| {
                    log::warn!("Init FIXCOMP segment at offset {}: dropping malformed frame: {}", cursor, e);
                    Vec::new()
                });
                let inflated_bytes: usize = inflated.iter().map(|m| m.len() + 1).sum();
                log::info!(
                    "Init FIXCOMP segment at offset {}: {} compressed → {} inner messages, ~{} inflated bytes",
                    cursor, total_len, inflated.len(), inflated_bytes,
                );
                for inner in inflated {
                    inflated_extra.extend_from_slice(&inner);
                    inflated_extra.push(b'\x01');
                }
                cursor += total_len;
                continue;
            }
        }
        cursor += 1;
    }
    let mut scan = init_data.to_vec();
    if !inflated_extra.is_empty() {
        log::info!("Inflated {} bytes of FIXCOMP content; appending to scan buffer", inflated_extra.len());
        scan.extend_from_slice(&inflated_extra);
    }
    scan
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An auth connection: reads the framed `frames`, records what is
    /// written.
    struct AuthWire {
        input: io::Cursor<Vec<u8>>,
        output: Vec<u8>,
    }

    impl AuthWire {
        fn new(frames: &[&str]) -> Self {
            let mut wire = Vec::new();
            for f in frames {
                wire.extend_from_slice(ns::NS_MAGIC);
                wire.extend_from_slice(&(f.len() as u32).to_be_bytes());
                wire.extend_from_slice(f.as_bytes());
            }
            Self { input: io::Cursor::new(wire), output: Vec::new() }
        }

        /// The texts of the frames written, in order.
        fn sent(&self) -> Vec<String> {
            let mut cursor = io::Cursor::new(self.output.clone());
            let mut out = Vec::new();
            while (cursor.position() as usize) < self.output.len() {
                out.push(String::from_utf8(ns::ns_recv(&mut cursor).unwrap().0).unwrap());
            }
            out
        }
    }

    impl Read for AuthWire {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> { self.input.read(buf) }
    }

    impl Write for AuthWire {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> { self.output.extend_from_slice(buf); Ok(buf.len()) }
        fn flush(&mut self) -> io::Result<()> { Ok(()) }
    }

    /// The connect request of the reference's login of 02/10/2026, masked.
    const CAPTURED_CONNECT: &str = "38;521;S{user};1585197;50;27;{hwid}|XX:XX:XX:XX:XX:XX;6abf4c1c.03bc;17.0.10.0.101/W/en_US/G;";
    /// The auth start the server answered it with.
    const CAPTURED_AUTH_START: &str = "50;520;1;1;;0;2311454642000;1;6840198100420923956;OTPWAY;;";

    // ibx#423: on the TLS auth connection the reference sends the connect
    // request in clear as its first message, with no key exchange before
    // it (gateway capture of 02/10/2026: TLS to port 4001, then 521, then
    // the auth start; no 532 on the auth connection in any archived login).
    #[test]
    fn auth_login_starts_with_the_connect_request_in_clear() {
        let mut wire = AuthWire::new(&[CAPTURED_AUTH_START]);
        let (start, refused) = ccp_login_start(&mut wire, &mut SecureChannel::new(), CAPTURED_CONNECT.as_bytes()).unwrap();
        assert_eq!(wire.sent(), vec![CAPTURED_CONNECT.to_string()]);
        assert!(!wire.sent().iter().any(|f| f.contains(";532;")), "no key exchange request");
        assert!(start.password_required);
        assert!(!refused);
    }

    // ibx#423: a refusal of the encryption with the permission to go on
    // sends the connect request again in clear; the farms of the session
    // then skip their key exchange.
    #[test]
    fn auth_login_refused_encryption_with_proceed_goes_on() {
        let mut wire = AuthWire::new(&["50;535;no crypto;1;", CAPTURED_AUTH_START]);
        let (_, refused) = ccp_login_start(&mut wire, &mut SecureChannel::new(), CAPTURED_CONNECT.as_bytes()).unwrap();
        assert_eq!(wire.sent(), vec![CAPTURED_CONNECT.to_string(), CAPTURED_CONNECT.to_string()]);
        assert!(refused);

        let mut wire = AuthWire::new(&["50;535;no crypto;0;"]);
        let err = ccp_login_start(&mut wire, &mut SecureChannel::new(), CAPTURED_CONNECT.as_bytes()).unwrap_err();
        assert_eq!(session::login_error(&err).unwrap().kind, session::LoginErrorKind::SecureConnectionRefused);
        assert_eq!(wire.sent(), vec![CAPTURED_CONNECT.to_string()]);
    }

    // ibx#423: a redirect ends this connection after the connect request;
    // the login to the new host starts the same way.
    #[test]
    fn auth_login_redirect_after_the_connect_request() {
        let mut wire = AuthWire::new(&["38;524;cdc1.example:4000;"]);
        let err = ccp_login_start(&mut wire, &mut SecureChannel::new(), CAPTURED_CONNECT.as_bytes()).unwrap_err();
        assert_eq!(err.to_string(), "REDIRECT:cdc1.example:4000");
        assert_eq!(wire.sent(), vec![CAPTURED_CONNECT.to_string()]);
    }

    // ibx#423: an encrypted message on the auth connection, which has no
    // key exchange, is an error, not a panic.
    #[test]
    fn auth_login_encrypted_message_without_key_exchange_is_an_error() {
        let mut wire = AuthWire::new(&["50;534;AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA;"]);
        let err = ccp_login_start(&mut wire, &mut SecureChannel::new(), CAPTURED_CONNECT.as_bytes()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("Decryptor is not valid"), "{err}");
    }

    /// A farm server on a local socket: answers the key exchange request
    /// with `key_answer` (none when no request is expected), then reads the
    /// logon and acknowledges it. Returns the bytes the client sent.
    fn clear_farm_server(key_answer: Option<&'static str>) -> (TcpStream, std::thread::JoinHandle<Vec<u8>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut got = Vec::new();
            if let Some(answer) = key_answer {
                let (request, _) = ns::ns_recv(&mut sock).unwrap();
                got.extend_from_slice(&request);
                let fields: Vec<&str> = answer.split(';').collect();
                sock.write_all(&ns::ns_build(50, fields[0].parse().unwrap(), &fields[1..], "")).unwrap();
            }
            let mut buf = [0u8; 4096];
            while !got.windows(4).any(|w| w == b"\x0110=") {
                let n = sock.read(&mut buf).unwrap();
                assert!(n > 0, "closed before the logon");
                got.extend_from_slice(&buf[..n]);
            }
            sock.write_all(&fix::fix_build(&[(fix::TAG_MSG_TYPE, "A"), (fix::TAG_SENDING_TIME, "20261001-10:00:00")], 1)).unwrap();
            std::thread::sleep(Duration::from_millis(200));
            got
        });
        (client, server)
    }

    fn logon_in_clear(sent: &[u8]) -> bool {
        let text = String::from_utf8_lossy(sent);
        text.contains("8=FIX.4.1\x01") && text.contains("\x0135=A\x01") && text.contains("\x0196=Suser/18/usfarm\x01")
    }

    // ibx#423: after the auth login went on in clear, a farm is not asked
    // for the encryption: the logon goes in clear and the session is not
    // signed.
    #[test]
    fn farm_after_a_refused_auth_encryption_logs_on_in_clear() {
        let (client, server) = clear_farm_server(None);
        let (conn, table) = farm_session(client, "usfarm", "user", "pass", true, "sid", &BigUint::from(7u32),
            "hw", "enc", 18, false, false).unwrap();
        assert!(table.is_none());
        drop(conn);
        let sent = server.join().unwrap();
        assert!(!sent.starts_with(ns::NS_MAGIC), "no key exchange request");
        assert!(logon_in_clear(&sent), "{}", String::from_utf8_lossy(&sent));
    }

    // ibx#423: a farm that refuses the encryption with the permission to go
    // on gets its logon in clear; any other refusal drops the farm.
    #[test]
    fn farm_refusing_the_encryption_with_proceed_goes_on_in_clear() {
        let (client, server) = clear_farm_server(Some("535;no crypto;1"));
        farm_session(client, "usfarm", "user", "pass", true, "sid", &BigUint::from(7u32),
            "hw", "enc", 18, false, true).unwrap();
        let sent = server.join().unwrap();
        assert!(String::from_utf8_lossy(&sent).contains(";532;"), "key exchange asked first");
        assert!(logon_in_clear(&sent[sent.windows(9).position(|w| w == b"8=FIX.4.1").unwrap()..]));

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let _ = ns::ns_recv(&mut sock).unwrap();
            sock.write_all(&ns::ns_build(50, ns::NS_SECURE_ERROR, &["no crypto", "0"], "")).unwrap();
        });
        let err = farm_session(client, "usfarm", "user", "pass", true, "sid", &BigUint::from(7u32),
            "hw", "enc", 18, false, true).err().expect("refused");
        assert!(err.to_string().contains("usfarm key exchange"), "{err}");
        server.join().unwrap();
    }

    #[test]
    fn encrypted_farm_logon_wraps_the_clear_one() {
        let clear = build_farm_logon("user", true, "usfarm", "sid", &BigUint::from(7u32), "hw", "enc", 18);
        assert!(logon_in_clear(&clear));
    }

    // ibx#263: the algo definition answers of the login burst are kept,
    // one XML each; other frames are not.
    #[test]
    fn algo_definitions_are_read_from_the_login_burst() {
        let burst = "8=FIX.4.1\x0135=U\x016040=54\x016364=IBALGO-AE\x016118=<AlgoExchange/>\x0110=1\x01"
            .to_string() + "8=FIX.4.1\x0135=U\x016040=210\x016118=x\x0110=2\x01"
            + "8=FIX.4.1\x0135=U\x016040=54\x016364=IBALGO-AL-STK\x016118=<AlgorithmsMap/>\x0110=3\x01";
        assert_eq!(parse_algo_definitions(&burst), ["<AlgoExchange/>", "<AlgorithmsMap/>"]);
    }

    // ibx#317: the tag scan sees the inflated init burst, and the bytes that
    // seed the connection buffer stay as received. The inflated copy used
    // to be appended to them, so every compressed message of the burst
    // reached the engine twice (seen on paper 25/09/2026: seq 4 to 119).
    #[test]
    fn init_scan_buffer_inflates_for_the_scan_only() {
        use crate::protocol::fix::fix_build;
        let plain = fix_build(&[(35, "U"), (6040, "93")], 3);
        let mut inner = fix_build(&[(35, "8"), (17, "e1"), (6145, "usfarm")], 4);
        inner.extend_from_slice(&fix_build(&[(35, "U"), (6040, "60"), (17, "e1")], 5));
        let mut init_data = plain.clone();
        init_data.extend_from_slice(&fixcomp::fixcomp_build(&inner));
        let received = init_data.clone();

        let scan = init_scan_buffer(&init_data);

        assert_eq!(init_data, received, "the seed bytes are unchanged");
        assert!(scan.starts_with(&received));
        let text = String::from_utf8_lossy(&scan[received.len()..]).into_owned();
        assert!(text.contains("6145=usfarm"), "the scan sees the inflated content");
        assert_eq!(text.matches("35=").count(), 2, "each inflated message once");
    }

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

    // ibx#253: each routing request gets a new id from one counter, not an
    // id chosen by the farm name.
    #[test]
    fn routing_request_ids_are_unique_and_increasing() {
        let ids: Vec<u32> = (0..5).map(|_| next_routing_request_id()).collect();
        assert!(ids[0] >= 1);
        for w in ids.windows(2) {
            assert!(w[1] > w[0], "{ids:?}");
        }
    }

    #[test]
    fn parse_farm_route_two_segments() {
        let parsed = parse_farm_route("zdc1.ibllc.com/eufarm").unwrap();
        assert_eq!(parsed, ("zdc1.ibllc.com".to_string(), "eufarm".to_string()));
    }

    #[test]
    fn parse_farm_route_three_segments_drops_port() {
        let parsed = parse_farm_route("zdc1.ibllc.com/euhmds/4000").unwrap();
        assert_eq!(parsed, ("zdc1.ibllc.com".to_string(), "euhmds".to_string()));
    }

    #[test]
    fn parse_farm_route_us_account() {
        let parsed = parse_farm_route("cdc1.ibllc.com/usfarm").unwrap();
        assert_eq!(parsed, ("cdc1.ibllc.com".to_string(), "usfarm".to_string()));
    }

    #[test]
    fn parse_farm_route_rejects_empty_and_malformed() {
        assert_eq!(parse_farm_route(""), None);
        assert_eq!(parse_farm_route("nofarm.example.com"), None);
        assert_eq!(parse_farm_route("/farm"), None);
        assert_eq!(parse_farm_route("host/"), None);
    }

    #[test]
    fn token_short_hash_always_8_chars() {
        // Per ib-agent#125: gateway pads to 8 hex chars. Brute-force search
        // over small inputs to find one whose SHA1 ends in a high-nibble
        // zero, then assert padding kicks in.
        for n in 0u64..10_000 {
            let token = BigUint::from(n);
            let h = token_short_hash(&token);
            assert_eq!(h.len(), 8,
                "token_short_hash must always be 8 chars; n={n} produced {h:?}");
            assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
        }
    }

    #[test]
    fn build_ccp_logon_structure() {
        let msg = build_ccp_logon("abc123|00:00:00:00:00:00", "17.0.10.0.101/W/en/G", 10, 1);
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

    fn tag_order(msg: &[u8]) -> Vec<u32> {
        msg.split(|&b| b == SOH)
            .filter_map(|f| f.iter().position(|&b| b == b'=').map(|i| &f[..i]))
            .filter_map(|t| std::str::from_utf8(t).ok()?.parse().ok())
            .collect()
    }

    // ibx#422: the reconnect logon is the fresh logon plus the session
    // epoch, in the reference order.
    #[test]
    fn reconnect_logon_sends_the_session_epoch_in_reference_order() {
        let msg = build_ccp_reconnect_logon("abc123|00:00:00:00:00:00", "17.0.10.0.101/W/en/G", 10, 1, "1790795127");
        assert_eq!(fix_parse(&msg)[&TAG_SESSION_EPOCH], "1790795127");
        assert_eq!(
            tag_order(&msg),
            [8, 9, 35, 34, 52, 98, 108, 141, 6059, 6034, 6968, 6490, 6266, 6351, 6397, 6947, 8361, 8098, 10],
        );
    }

    // ibx#399: reconnect attempts go to the primary host and its backups in
    // turn.
    #[test]
    fn reconnect_hosts_rotate_primary_and_backups() {
        assert_eq!(ccp_reconnect_hosts("cdc1.example.com"),
            ["cdc1.example.com", "cdc1-hb1.example.com", "cdc1-hb2.example.com"]);
        let hosts: Vec<String> = (1..=7).map(|a| ccp_reconnect_host("cdc1.example", a)).collect();
        assert_eq!(hosts, ["cdc1.example", "cdc1-hb1.example", "cdc1-hb2.example",
            "cdc1.example", "cdc1-hb1.example", "cdc1-hb2.example", "cdc1.example"]);
        assert_eq!(ccp_reconnect_host("cdc1.example", 0), "cdc1.example");
    }

    #[test]
    fn reconnect_hosts_without_backups() {
        assert_eq!(ccp_reconnect_hosts("127.0.0.1"), ["127.0.0.1"]);
        assert_eq!(ccp_reconnect_hosts("::1"), ["::1"]);
        assert_eq!(ccp_reconnect_hosts("localhost"), ["localhost"]);
        assert_eq!(ccp_reconnect_host("localhost", 2), "localhost");
    }

    #[test]
    fn fresh_logon_has_no_session_epoch() {
        let msg = build_ccp_logon("abc123|00:00:00:00:00:00", "17.0.10.0.101/W/en/G", 10, 1);
        assert!(!fix_parse(&msg).contains_key(&TAG_SESSION_EPOCH));
        let empty = build_ccp_reconnect_logon("abc123|00:00:00:00:00:00", "17.0.10.0.101/W/en/G", 10, 1, "");
        assert_eq!(tag_order(&empty), tag_order(&msg), "no epoch known: the fresh logon");
    }

    #[test]
    fn logon_reply_epoch_is_read_from_plain_and_compressed_replies() {
        let reply = fix_build(&[(35, "A"), (52, "20260930-19:05:24"), (98, "0"), (108, "10"), (141, "Y"), (6059, "1790795127")], 1);
        assert_eq!(logon_reply_epoch(&reply).as_deref(), Some("1790795127"));
        let without = fix_build(&[(35, "A"), (52, "20260930-19:05:24"), (98, "0")], 1);
        assert_eq!(logon_reply_epoch(&without), None);

        let comp = fixcomp::fixcomp_build(&fix_build(&[(35, "A"), (6059, "1790795128")], 1));
        assert_eq!(logon_reply_epoch(&comp).as_deref(), Some("1790795128"));
    }

    // ibx#422: the machine's IANA zone by default, the override when set.
    #[test]
    fn logon_time_zone_is_the_machine_zone_unless_overridden() {
        assert_eq!(time_zone_or_system(Some("America/New_York".into())), "America/New_York");
        let system = time_zone_or_system(None);
        assert!(!system.is_empty());
        assert_eq!(time_zone_or_system(Some(String::new())), system, "an empty override is ignored");
        if let Ok(tz) = jiff::tz::TimeZone::try_system()
            && let Some(name) = tz.iana_name()
        {
            assert_eq!(system, name);
        }
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
    fn parse_misc_urls_pipe_separated() {
        let m = parse_misc_urls("region_dam=ny5wwwdam1.ibllc.com|region_webserver=ny5wwwgw1.ibllc.com|nossl=0");
        assert_eq!(m.len(), 3);
        assert_eq!(m.get("region_dam").map(String::as_str), Some("ny5wwwdam1.ibllc.com"));
        assert_eq!(m.get("region_webserver").map(String::as_str), Some("ny5wwwgw1.ibllc.com"));
        assert_eq!(m.get("nossl").map(String::as_str), Some("0"));
    }

    #[test]
    fn parse_misc_urls_pct_encoded_pipe() {
        let m = parse_misc_urls("a=1|b=2|c%7Cd=3");
        assert_eq!(m.len(), 3);
        assert_eq!(m.get("a").map(String::as_str), Some("1"));
        assert_eq!(m.get("b").map(String::as_str), Some("2"));
        assert_eq!(m.get("c|d").map(String::as_str), Some("3"));
    }

    #[test]
    fn parse_misc_urls_pct_encoded_pipe_in_value() {
        let m = parse_misc_urls("a=x%7Cy");
        assert_eq!(m.get("a").map(String::as_str), Some("x|y"));
    }

    #[test]
    fn parse_misc_urls_pct_encoded_lowercase() {
        let m = parse_misc_urls("a=x%7cy");
        assert_eq!(m.get("a").map(String::as_str), Some("x|y"));
    }

    #[test]
    fn parse_misc_urls_empty_input() {
        assert!(parse_misc_urls("").is_empty());
    }

    #[test]
    fn parse_misc_urls_comma_fallback() {
        let m = parse_misc_urls("a=1,b=2,c=3");
        assert_eq!(m.len(), 3);
        assert_eq!(m.get("b").map(String::as_str), Some("2"));
    }

    #[test]
    fn parse_misc_urls_drops_malformed_entries() {
        let m = parse_misc_urls("a=1|nokv|=val|b=2");
        assert_eq!(m.len(), 2);
        assert_eq!(m.get("a").map(String::as_str), Some("1"));
        assert_eq!(m.get("b").map(String::as_str), Some("2"));
    }

    #[test]
    fn parse_misc_urls_value_with_equals() {
        // split_once stops at first `=`, so URLs with query strings round-trip.
        let m = parse_misc_urls("cookbook=https://x.example/path?a=1&b=2");
        assert_eq!(m.get("cookbook").map(String::as_str), Some("https://x.example/path?a=1&b=2"));
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
            password: Zeroizing::new("pass".to_string()),
            host: "cdc1.ibllc.com".to_string(),
            paper: true,
            accept_invalid_certs: false,
            ib_key_timeout_secs: session::IB_KEY_DEFAULT_TIMEOUT_SECS,
            ib_key_token_sub_type: session::IB_KEY_DEFAULT_TOKEN_SUB_TYPE.into(),
            code_provider: None,
        };
        assert_eq!(config.username, "user");
        assert!(config.paper);
    }
}

/// One API news source of the logon. It is subscribed only when it has
/// no service id (ibx#460).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NewsSource {
    pub(crate) code: String,
    pub(crate) subscribed: bool,
}

/// The API news sources of the logon, in logon order (ibx#460).
pub(crate) fn parse_news_sources(raw: &str) -> Vec<NewsSource> {
    raw.split(',').filter_map(|entry| {
        let entry = entry.trim();
        if entry.is_empty() { return None; }
        let (code, ids) = match entry.split_once(':') {
            Some((c, ids)) => (c, ids),
            None => (entry, ""),
        };
        if code.is_empty() { return None; }
        Some(NewsSource {
            code: code.to_string(),
            subscribed: ids.split(';').all(|id| id.trim().is_empty()),
        })
    }).collect()
}

/// Look up `code` in a logon list of code and value items (names,
/// capabilities).
fn news_list_value<'a>(raw: &'a str, code: &str) -> Option<&'a str> {
    raw.split(',').find_map(|item| {
        let (c, v) = item.split_once('/')?;
        c.trim().eq_ignore_ascii_case(code).then_some(v.trim())
    })
}

/// The reqNewsProviders answer built from the logon (ibx#460): the
/// subscribed API sources whose capabilities include news, in source
/// order, named from the logon name list or by their code. A source with
/// no capability entry is not used.
pub(crate) fn news_providers_from_logon(
    sources: &[NewsSource],
    raw_names: &str,
    raw_capabilities: &str,
) -> Vec<crate::types::NewsProvider> {
    sources.iter().filter(|s| s.subscribed).filter_map(|s| {
        let caps = news_list_value(raw_capabilities, &s.code).filter(|c| !c.is_empty());
        let Some(caps) = caps else {
            log::warn!("News source {} has empty capabilities and is not processed", s.code);
            return None;
        };
        if !caps.contains('N') { return None; }
        let name = news_list_value(raw_names, &s.code).filter(|n| !n.is_empty()).unwrap_or(&s.code);
        Some(crate::types::NewsProvider { code: s.code.clone(), name: name.to_string() })
    }).collect()
}

#[cfg(test)]
mod news_provider_tests {
    use super::*;

    // ibx#460: the captured paper logon gives the 8 providers the API
    // client received, in logon order, with their names.
    #[test]
    fn providers_from_the_paper_logon() {
        let sources = "BRFG,BRFUPDN,DJ-N,DJ-RTA,DJ-RTE,DJ-RTG,DJ-RTPRO,DJNL,BZ:706,DJTOP:557;558;559,FLY:698";
        let names = "ABSTR/Absolute Strategy Research,BRFG/Briefing.com General Market Columns,\
                     BRFUPDN/Briefing.com Analyst Actions,BZ/Benzinga,DJ-N/Dow Jones Global Equity Trader,\
                     DJ-RTA/Dow Jones Top Stories Asia Pacific,DJ-RTE/Dow Jones Top Stories Europe,\
                     DJ-RTG/Dow Jones Top Stories Global,DJ-RTPRO/Dow Jones Top Stories Pro,\
                     DJNL/Dow Jones Newsletters,DJTOP/Dow Jones,FLY/The Fly";
        let caps = "ABSTR/RNP,BRFG/RNM,BRFUPDN/RNPU,BZ/N,DJ-N/N,DJ-RTA/N,DJ-RTE/N,DJ-RTG/N,\
                    DJ-RTPRO/N,DJNL/N,DJTOP/T,FLY/N";
        let parsed = parse_news_sources(sources);
        assert_eq!(parsed.len(), 11);
        assert!(!parsed[8].subscribed && !parsed[9].subscribed && !parsed[10].subscribed);
        let p = news_providers_from_logon(&parsed, names, caps);
        let codes: Vec<&str> = p.iter().map(|p| p.code.as_str()).collect();
        assert_eq!(codes, ["BRFG", "BRFUPDN", "DJ-N", "DJ-RTA", "DJ-RTE", "DJ-RTG", "DJ-RTPRO", "DJNL"]);
        assert_eq!(p[0].name, "Briefing.com General Market Columns");
        assert_eq!(p[7].name, "Dow Jones Newsletters");
    }

    #[test]
    fn providers_filter_and_names() {
        let parsed = parse_news_sources("AAA,BBB,CCC,DDD:");
        assert!(parsed[3].subscribed, "an empty service id list is subscribed");
        // BBB has no news capability, CCC has no capability entry, DDD no name.
        let p = news_providers_from_logon(&parsed, "AAA/Alpha", "AAA/RN,BBB/T,DDD/N");
        let got: Vec<(&str, &str)> = p.iter().map(|p| (p.code.as_str(), p.name.as_str())).collect();
        assert_eq!(got, [("AAA", "Alpha"), ("DDD", "DDD")]);
        assert!(news_providers_from_logon(&parse_news_sources(""), "AAA/Alpha", "AAA/N").is_empty());
    }
}

/// Soft dollar tiers as the reference reads them from logon tag 6522
/// (ibx#480): groups `{KEY}:{tiers}` separated by `;`, tiers `{name}@{value}`
/// separated by `,`. A later group with the same key replaces the earlier
/// one; every tier of every key is returned. The display name is
/// `Tier {name} ({value})`, with name + 1 when the name is an integer.
pub(crate) fn parse_soft_dollar_tiers(raw: &str) -> Vec<crate::types::SoftDollarTier> {
    let mut groups: Vec<(String, Vec<crate::types::SoftDollarTier>)> = Vec::new();
    for group in raw.split(';').filter(|g| !g.is_empty()) {
        let Some((key, list)) = group.split_once(':') else {
            log::warn!("Unexpected soft dollar tiers format: {} in: {}", group, raw);
            continue;
        };
        let tiers: Vec<crate::types::SoftDollarTier> = list.split(',').filter(|t| !t.is_empty()).filter_map(|tier| {
            let Some((name, val)) = tier.split_once('@') else {
                log::warn!("Unexpected soft dollar tier format: {} in: {}", tier, raw);
                return None;
            };
            let shown = name.parse::<i32>().map_or_else(|_| name.to_string(), |n| n.wrapping_add(1).to_string());
            Some(crate::types::SoftDollarTier {
                name: name.to_string(),
                val: val.to_string(),
                display_name: format!("Tier {} ({})", shown, val),
            })
        }).collect();
        if tiers.is_empty() { continue; }
        let key = key.to_uppercase();
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some(existing) => existing.1 = tiers,
            None => groups.push((key, tiers)),
        }
    }
    groups.into_iter().flat_map(|(_, tiers)| tiers).collect()
}

/// The family codes answer of the reference (ibx#441): the list of
/// accounts with their code when the accounts do not all share one code;
/// else one entry for every account, `*`, with the shared code, empty when
/// no account has one.
pub(crate) fn family_codes_answer(codes: Vec<crate::types::FamilyCode>) -> Vec<crate::types::FamilyCode> {
    let first = codes.iter().find(|c| !c.family_code_str.is_empty()).map(|c| c.family_code_str.clone());
    if let Some(code) = &first {
        if codes.iter().any(|c| c.family_code_str != *code) {
            return codes;
        }
    }
    vec![crate::types::FamilyCode { account_id: "*".into(), family_code_str: first.unwrap_or_default() }]
}

#[cfg(test)]
mod family_code_tests {
    use super::family_codes_answer;
    use crate::types::FamilyCode;

    fn codes(list: &[(&str, &str)]) -> Vec<FamilyCode> {
        list.iter().map(|(a, c)| FamilyCode { account_id: a.to_string(), family_code_str: c.to_string() }).collect()
    }

    fn pairs(list: &[FamilyCode]) -> Vec<(&str, &str)> {
        list.iter().map(|c| (c.account_id.as_str(), c.family_code_str.as_str())).collect()
    }

    // ibx#441: one entry when the accounts share a code or none has one.
    #[test]
    fn family_codes_answer_as_the_reference() {
        // No data, or no account with a code: one entry with an empty code.
        assert_eq!(pairs(&family_codes_answer(vec![])), [("*", "")]);
        assert_eq!(pairs(&family_codes_answer(codes(&[("DU1", ""), ("DU2", "")]))), [("*", "")]);
        // Every account with the same code: one entry with that code.
        assert_eq!(pairs(&family_codes_answer(codes(&[("U1", "F1"), ("U2", "F1")]))), [("*", "F1")]);
        // Different codes, or a code and no code: the full list.
        assert_eq!(pairs(&family_codes_answer(codes(&[("U1", "F1"), ("U2", "F2")]))), [("U1", "F1"), ("U2", "F2")]);
        assert_eq!(pairs(&family_codes_answer(codes(&[("U1", ""), ("U2", "F2")]))), [("U1", ""), ("U2", "F2")]);
    }
}

#[cfg(test)]
mod soft_dollar_tests {
    use super::parse_soft_dollar_tiers;

    // ibx#480: tiers come from logon tag 6522, in the reference's format.
    #[test]
    fn soft_dollar_tiers_from_6522() {
        assert!(parse_soft_dollar_tiers("").is_empty());
        let t = parse_soft_dollar_tiers("USSTK:0@ABC");
        assert_eq!(t.len(), 1);
        assert_eq!((t[0].name.as_str(), t[0].val.as_str(), t[0].display_name.as_str()), ("0", "ABC", "Tier 1 (ABC)"));

        let t = parse_soft_dollar_tiers("usstk:1@X,Gold@Y;EUSTK:2@Z;BAD;USSTK:3@W;CASH:");
        let shown: Vec<&str> = t.iter().map(|t| t.display_name.as_str()).collect();
        assert_eq!(shown, ["Tier 4 (W)", "Tier 3 (Z)"], "a later group with the same key replaces the earlier one");

        let t = parse_soft_dollar_tiers("USSTK:Gold@Y,nope");
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].display_name, "Tier Gold (Y)");
    }
}

#[cfg(test)]
mod account_config_tests {
    use super::parse_account_config;

    // ibx#287: the logon feature list turns on US stock sizes in lots.
    #[test]
    // #452: the API depth limit of the logon (captured paper logon: API 3,
    // total 3).
    fn depth_limit_follows_the_reference_rule() {
        let f = |v: [Option<&str>; 5]| super::depth_limit(&v.map(|x| x.map(String::from)));
        assert_eq!(f([Some("3"), Some("3"), Some("100"), Some("5"), None]), 3);
        assert_eq!(f([None, Some("4"), None, None, None]), 4);
        assert_eq!(f([None, None, Some("100"), Some("5"), Some("9")]), 5);
        assert_eq!(f([None, None, None, None, Some("2")]), 3);
        assert_eq!(f([None, None, None, None, None]), 3);
    }

    #[test]
    fn tick_by_tick_limit_follows_the_reference_rule() {
        let f = |a: Option<&str>, b: Option<&str>, c: Option<&str>, d: Option<&str>| {
            super::tick_by_tick_limit(&[a.map(String::from), b.map(String::from), c.map(String::from), d.map(String::from)])
        };
        assert_eq!(f(Some("100"), Some("5"), None, Some("3")), 5, "captured paper logon");
        assert_eq!(f(None, Some("5"), Some("7"), Some("3")), 7);
        assert_eq!(f(None, None, None, Some("9")), 9);
        assert_eq!(f(Some("100"), Some("-1"), None, Some("4")), 4);
        assert_eq!(f(Some("100"), Some("0"), None, Some("9")), 3, "0 is kept, then at least 3");
        assert_eq!(f(None, None, None, None), 3);
        assert!(super::features_have("A,NOTICKBYTICK", "NOTICKBYTICK"));
        assert!(!super::features_have("NOTICKBYTICKS", "NOTICKBYTICK"));
    }

    #[test]
    fn max_real_time_requests_from_the_logon() {
        let tags = |pairs: &[(u32, i64)]| pairs.iter().copied().collect::<std::collections::HashMap<u32, i64>>();
        // The values of the paper logon.
        assert_eq!(super::max_real_time_requests(&tags(&[(6846, 100), (6847, 100)])), 100);
        assert_eq!(super::max_real_time_requests(&tags(&[(6846, 60), (6847, 0)])), 60);
        assert_eq!(super::max_real_time_requests(&tags(&[(8421, 100), (8422, 5), (6083, 30)])), 100);
        assert_eq!(super::max_real_time_requests(&tags(&[(8421, 100), (6083, 30)])), 30);
        assert_eq!(super::max_real_time_requests(&tags(&[])), 40);
    }

    #[test]
    fn scale_us_lots_is_a_feature_token() {
        assert!(super::features_scale_us_lots("SCALEFRAC,SCALEMOD,SCALEUSLOT,SCALEWHATIF"));
        assert!(super::features_scale_us_lots("SCALEUSLOT"));
        assert!(!super::features_scale_us_lots("SCALEUSLOTS,XSCALEUSLOT"));
        assert!(!super::features_scale_us_lots(""));
    }

    // ibx#483: whiteBrandingId is logon tag 6593, not 6571.
    #[test]
    fn white_branding_is_tag_6593() {
        assert_eq!(super::white_branding_part("6593=ABC"), Some("ABC"));
        assert_eq!(super::white_branding_part("6571=X"), None);
    }

    // ibx#425: the paper answer (15/06/2026) has no CUSTACCT and no 8234.
    #[test]
    fn account_config_from_the_login_burst() {
        let burst = "8=FIX.4.1\x019=10\x0135=U\x016040=75\x011=DU1\x0110=000\x01\
                     8=FIX.4.1\x019=10\x0135=U\x016040=210\x016556=AcctConfig4\x011=DU1\x016542=OLP,EUCOSTCALC,EUILLS\x0110=000\x01";
        let (features, mifid) = parse_account_config(burst).unwrap();
        assert_eq!(features, ["OLP", "EUCOSTCALC", "EUILLS"]);
        assert_eq!(mifid, "");
        assert!(parse_account_config("8=FIX.4.1\x0135=U\x016040=75\x01").is_none());
    }
}
