//! Connection lifecycle: authentication and data connection management.

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

/// Result of authentication.
///
/// `session_token` is the SRP-derived shared secret K as a `BigUint`. For wire-byte uses
/// (e.g. SHA-1 challenge/response, token short hashes), prefer [`AuthResult::session_token_bytes`],
/// which returns the canonical big-endian form with leading zeros stripped — matching the
/// representation the server expects.
///
/// `token_type` is one of `"st"`, `"tst"`, or `"zenith"` and corresponds verbatim to the
/// `stoken_type` value used by SSO authenticators in the upstream Java auth flow.
pub struct AuthResult {
    /// SRP shared secret K. Use [`session_token_bytes`](Self::session_token_bytes) for the
    /// canonical big-endian wire form.
    pub session_token: BigUint,
    /// Token type discriminator: `"st"`, `"tst"`, or `"zenith"`. Matches the `stoken_type`
    /// field expected by the SSO `Authenticate-TWS` body.
    pub token_type: String,
    pub session_id: String,
    pub features: Vec<String>,
    pub authenticated: bool,
}

impl AuthResult {
    /// Canonical big-endian byte form of [`Self::session_token`], with leading zeros
    /// stripped (single `0x00` retained when the value is zero).
    ///
    /// This is the exact representation used as the second SHA-1 input for soft-token
    /// challenge/response and SSO `Authenticate-TWS` bodies. Round-trips through
    /// `BigUint::from_bytes_be`.
    pub fn session_token_bytes(&self) -> Vec<u8> {
        let raw = self.session_token.to_bytes_be();
        crate::auth::crypto::strip_leading_zeros(&raw).to_vec()
    }
}

/// Authenticated auth session.
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

/// Generate a session ID: hex(epoch_secs).hex(millis%1000).
pub fn get_session_id() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    let millis = now.as_millis() as u64;
    let secs = millis / 1000;
    let ms = millis % 1000;
    format!("{:x}.{:04x}", secs, ms)
}

/// Path of the persistent 8-hex machine_id file used in tag 6351.
///
/// Per ib-agent#132: the Java client reads/creates `%USERPROFILE%\hwid`
/// on Windows or `$HOME/.hwid` elsewhere, persists 8 hex chars there, and
/// reuses it across logons. IB binds that prefix to the IBKey enrollment
/// at first-login time; live farms silent-drop logons whose prefix isn't
/// in the registered set.
///
/// Override with the `IBX_HWID_PATH` env var to point elsewhere (containers,
/// CI, sharing one cookie across multiple machines, etc.).
fn hwid_path() -> std::path::PathBuf {
    if let Some(p) = std::env::var_os("IBX_HWID_PATH") {
        return std::path::PathBuf::from(p);
    }
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    if cfg!(windows) {
        home.join("hwid")
    } else {
        home.join(".hwid")
    }
}

/// Read the existing 8-hex machine_id, or generate+persist a fresh one.
///
/// `IBX_HWID` env var, if set to a hex string, short-circuits the file lookup
/// and is used verbatim (left-padded to 8 chars). Useful for one-shot scripts
/// or when injecting an already-enrolled cookie via secrets management.
fn read_or_create_hwid() -> String {
    if let Ok(v) = std::env::var("IBX_HWID") {
        let v = v.trim();
        if !v.is_empty() && v.chars().all(|c| c.is_ascii_hexdigit()) {
            return format!("{:0>8}", v);
        }
    }
    let path = hwid_path();
    if let Ok(s) = std::fs::read_to_string(&path) {
        let s = s.trim();
        if !s.is_empty() && s.chars().all(|c| c.is_ascii_hexdigit()) {
            return format!("{:0>8}", s);
        }
    }
    let mut buf = [0u8; 4];
    rand::rng().fill_bytes(&mut buf);
    let new_hwid = format!("{:08x}", u32::from_be_bytes(buf));
    let _ = std::fs::write(&path, &new_hwid);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444));
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("attrib")
            .args(["+H", "+R"])
            .arg(&path)
            .status();
    }
    new_hwid
}

/// Generate hardware info string: `{machine_id}|{MAC}`.
///
/// Live data farms validate the MAC field; an all-zero MAC causes the FIX
/// 35=A logon to be silently rejected (paper farms don't validate).
/// `machine_id` is the persistent 8-hex value from `~/hwid` (see #132).
pub fn get_hw_info() -> String {
    let machine_id = read_or_create_hwid();
    let mac = first_real_mac().unwrap_or_else(|| "00:00:00:00:00:00".to_string());
    format!("{}|{}", machine_id, mac)
}

/// Probe the OS for the first non-zero MAC address. Returns `None` if no NIC
/// has a usable MAC (e.g. no networking, all interfaces virtual).
fn first_real_mac() -> Option<String> {
    let all = mac_address::MacAddressIterator::new().ok()?;
    for mac in all {
        let bytes = mac.bytes();
        if bytes.iter().any(|&b| b != 0) {
            return Some(format!(
                "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
                bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5],
            ));
        }
    }
    None
}

/// Discover the local LAN IP that would route to the public internet.
/// Returns "127.0.0.1" if no external route is configured.
///
/// Uses the standard `UdpSocket::connect` trick: connecting a UDP socket to
/// a public address doesn't send any packets, but lets the OS pick the
/// outbound interface, exposed via `local_addr()`.
pub fn get_lan_ip() -> String {
    use std::net::UdpSocket;
    let sock = match UdpSocket::bind("0.0.0.0:0") {
        Ok(s) => s,
        Err(_) => return "127.0.0.1".into(),
    };
    if sock.connect("8.8.8.8:80").is_err() {
        return "127.0.0.1".into();
    }
    sock.local_addr()
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| "127.0.0.1".into())
}

/// Send an encrypted protocol message.
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

/// Receive an encrypted response and decrypt.
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

    if msg_type == NS_SECURE_ERROR || msg_type == ns::NS_ERROR_RESPONSE {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("Auth error: {}", parts[2..].join(";")),
        ));
    }
    if msg_type == NS_REDIRECT {
        let target = parts.get(2).unwrap_or(&"");
        return Err(io::Error::new(
            io::ErrorKind::ConnectionReset,
            format!("REDIRECT:{}", target),
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

/// Receive a framed message and classify as text or binary.
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
#[derive(Debug)]
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

/// Execute authentication protocol.
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
    let mut a_bytes = [0u8; 32];
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

/// Execute token authentication for farm connections.
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

/// Maximum size for a farm auth message (prevents unbounded allocation).
const MAX_FARM_MSG_SIZE: usize = 65536;

/// Read one framed `8=1` message from a farm stream.
///
/// `carry` holds bytes across calls: a read can return more than one message's
/// worth of data, and a high-latency regional gateway can coalesce the last
/// auth response and the farm logon ACK that follows it into a single read.
/// This returns exactly the framed message and leaves any surplus bytes in
/// `carry` so the caller can hand them to the next reader. Discarding that tail
/// dropped the logon ACK and stalled the exchange (ibx#237).
fn recv_8eq1(stream: &mut TcpStream, carry: &mut Vec<u8>) -> io::Result<Vec<u8>> {
    let mut tmp = [0u8; 4096];
    // Tolerate transient WouldBlock/TimedOut (os error 35 on macOS) from the
    // short poll timeout until an overall deadline; a slow segment from a
    // high-latency regional gateway must not fail the auth exchange (ibx#237).
    let deadline = std::time::Instant::now()
        + std::time::Duration::from_secs_f64(TIMEOUT_FARM_LOGON);
    loop {
        // A prior call may already have buffered a full message.
        if let Some(total) = try_frame_8eq1(carry)? {
            let msg = carry[..total].to_vec();
            carry.drain(..total);
            return Ok(msg);
        }
        let n = match stream.read(&mut tmp) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock
                || e.kind() == io::ErrorKind::TimedOut =>
            {
                if std::time::Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "farm auth timed out waiting for server response",
                    ));
                }
                continue;
            }
            Err(e) => return Err(e),
        };
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "farm connection closed during auth",
            ));
        }
        carry.extend_from_slice(&tmp[..n]);
    }
}

/// Locate one complete `8=1`-framed message at the front of `buf`.
///
/// Returns the message's total byte length when a full message is present,
/// `None` when more bytes are needed, or an error when the advertised body
/// length is implausibly large.
fn try_frame_8eq1(buf: &[u8]) -> io::Result<Option<usize>> {
    // Needs "8=1\x01" and a body length "9=NNNN\x01".
    if !buf.starts_with(b"8=1\x01") {
        return Ok(None);
    }
    let Some(nine_pos) = buf.windows(2).position(|w| w == b"9=") else {
        return Ok(None);
    };
    let val_start = nine_pos + 2;
    let Some(soh_off) = buf[val_start..].iter().position(|&b| b == 0x01) else {
        return Ok(None);
    };
    let body_len: usize = std::str::from_utf8(&buf[val_start..val_start + soh_off])
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if body_len > MAX_FARM_MSG_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("farm message body too large: {} bytes", body_len),
        ));
    }
    let total = val_start + soh_off + 1 + body_len;
    if buf.len() < total {
        return Ok(None);
    }
    Ok(Some(total))
}

/// Extract binary payload from a framed message.
fn extract_xyz(msg: &[u8]) -> &[u8] {
    let marker = b"35=X\x01";
    if let Some(idx) = msg.windows(marker.len()).position(|w| w == marker) {
        &msg[idx + marker.len()..]
    } else {
        msg
    }
}

/// Outcome of the per-session second-factor approval gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IbKeyOutcome {
    /// Server bypassed the second-factor gate (no second factor configured for
    /// this account, or the server short-circuited to PASSED on its own).
    Skipped,
    /// User approved on their device.
    Approved {
        /// URL the IBKey app posts to when the user approves; useful for
        /// "tap your phone (or open this URL)" prompts.
        approval_url: String,
        /// Per-session 6-digit identifier the user can verify visually.
        session_id: String,
        /// SOFT session token issued by `XYZ AUTH_FINISH(771) state=5 PASSED`,
        /// hex-encoded. This is the token that downstream farm logons must
        /// hash for tag 8483 (NOT the SRP-derived `session_key`). Empty if the
        /// AUTH_FINISH body didn't carry an extractable token.
        soft_token_hex: String,
    },
}

/// Errors specific to the second-factor approval gate. Wrapped into
/// `io::Error` so the call site can stay uniform.
fn ib_key_err(kind: io::ErrorKind, msg: impl Into<String>) -> io::Error {
    io::Error::new(kind, msg.into())
}

/// Challenge details surfaced to a [`CodeProvider`] callback.
///
/// Populated from the server's `XYZ 775` state=2 reply: the per-session
/// display id the user sees next to the 8-char code in the IBKey app, and
/// the `clientam.com/ibkr/ibkey/seamless?S=…` URL also used by the web
/// fallback. Either may be empty if the server omitted it in this run.
#[derive(Debug, Clone, Default)]
pub struct IbKeyChallenge {
    pub display_id: String,
    pub avth_url: String,
}

/// 8-character Challenge/Response code provider callback.
///
/// If supplied (via `GatewayConfig::code_provider`), the C/R variant of the
/// IBKey gate is used instead of waiting for a mobile push approval: after
/// the server delivers state=2, this callback is invoked once with the
/// parsed challenge and the returned 8-character code is submitted as
/// `XYZ 775` state=3. Per ib-agent#149, the server has no retry loop — one
/// wrong code returns state=4 FAILED and the socket is torn down. The
/// callback should pull the code from a deterministic source (stdin,
/// secrets vault, etc.) or return an `io::Error` to abort the login.
///
/// The callback runs on its own thread and may block for as long as the
/// operator needs — the gate keeps answering the server's keepalive probes
/// meanwhile. The code is submitted on the first inbound message after the
/// callback returns, so submission trails entry by up to one probe interval
/// for as long as the server keeps probing. The thread is not cancelled if
/// the gate returns early, so a callback holding a shared resource (stdin,
/// say) keeps holding it until it returns on its own.
pub type CodeProvider = std::sync::Arc<
    dyn Fn(IbKeyChallenge) -> io::Result<String> + Send + Sync,
>;

/// How long the gate waits inline for a freshly started `code_provider` before
/// falling back to polling it between inbound messages. Sized to cover a
/// provider that reads from a vault, an env var or a file, and to stay far
/// below the server's probe cadence so nothing is starved by the wait.
const IB_KEY_PROVIDER_FAST_PATH_GRACE: std::time::Duration =
    std::time::Duration::from_secs(1);

/// Put the 8-character Challenge/Response code on the wire as `XYZ 775` state=3.
fn submit_swcr_code<S: Write>(stream: &mut S, code: &str) -> io::Result<()> {
    let framed = xyz::xyz_wrap(&xyz::xyz_build_swcr_token_code_submission(code));
    stream.write_all(&framed)?;
    log::info!(
        "2FA gate: submitted SWCR_TOKEN state=3 code (len={}, {} bytes framed)",
        code.len(), framed.len(),
    );
    Ok(())
}

/// The provider thread dropped its sender without sending: it panicked.
fn provider_panicked() -> io::Error {
    ib_key_err(
        io::ErrorKind::Other,
        "2FA gate: code_provider panicked without returning a code",
    )
}

/// Compact hex dump for diagnostic logging.
fn hex_dump(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect::<Vec<_>>().join(" ")
}

/// Default deadline for the second-factor gate, matching the server-side
/// timeout measured in capture run B (~18 min).
pub const IB_KEY_DEFAULT_TIMEOUT_SECS: u64 = 1080;

/// Default IBKey token sub-type used in the SWCR_TOKEN state=1 body. Matches
/// the captured reference profile in ib-agent#123. Some accounts/SWCR
/// configurations require a different value — override via
/// [`crate::gateway::GatewayConfig::ib_key_token_sub_type`].
pub const IB_KEY_DEFAULT_TOKEN_SUB_TYPE: &str = "2a";

/// Execute the second-factor approval gate that follows SRP on a live login.
///
/// Sends `XYZ_MSG_SWCR_TOKEN` state=1 carrying the username, then loops over
/// inbound messages until one of:
///
/// 1. `XYZ_MSG_TOKEN_AUTH` (771) state=5 with `PASSED` arrives → user approved
/// 2. `XYZ_MSG_SWCR_TOKEN` (775) state=2 arrives → wait state, capture
///    `approval_url` / `session_id`, keep looping
/// 3. An `NS_TEST_REQUEST` (530) arrives → reply with `NS_HEART_BEAT` (531),
///    keep looping
/// 4. `deadline` expires → `TimedOut` error
/// 5. Underlying socket close → `ConnectionAborted` error (server's deadline)
///
/// If the server jumps straight to a non-XYZ NS message (e.g. CONNECT_RESPONSE),
/// returns `Skipped` and logs the path — the unread NS message is then handled
/// by the post-auth loop. (We can't `unread`, so this branch is reached only
/// when the very first reply is XYZ AUTH_FINISH PASSED with no preceding
/// state=2.)
pub fn do_ib_key_2fa<S: Read + Write>(
    stream: &mut S,
    token_sub_type: &str,
    deadline: std::time::Instant,
    code_provider: Option<&CodeProvider>,
) -> io::Result<IbKeyOutcome> {
    use std::time::Instant;

    // Send SWCR_TOKEN state=1. The username slot is empty in state=1; the
    // tokenSubType (account-specific, typically "2a") is the only non-empty
    // body field. See ib-agent#123 for the canonical wire layout.
    let init = xyz::xyz_build_swcr_token_init(token_sub_type);
    let framed = xyz::xyz_wrap(&init);
    stream.write_all(&framed)?;
    log::info!(
        "2FA gate: sent SWCR_TOKEN state=1 ({} bytes inner, {} bytes framed)",
        init.len(), framed.len(),
    );
    log::debug!("2FA gate: SWCR_TOKEN bytes (framed) = {}", hex_dump(&framed));

    let mut approval_url = String::new();
    let mut session_id = String::new();
    let mut announced_wait = false;
    let mut saw_challenge = false;
    let mut code_submitted = false;
    let mut pending_code: Option<std::sync::mpsc::Receiver<io::Result<String>>> = None;

    loop {
        if Instant::now() >= deadline {
            return Err(ib_key_err(
                io::ErrorKind::TimedOut,
                "2FA approval timed out (client deadline)",
            ));
        }

        let recv = match recv_msg(stream) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof
                || e.kind() == io::ErrorKind::ConnectionReset
                || e.kind() == io::ErrorKind::ConnectionAborted =>
            {
                // Server closed the socket. Two distinct cases:
                //   - We never received state=2: server rejected the SWCR_TOKEN
                //     init, the account doesn't have IBKey enabled, or the wire
                //     format is wrong. Fast (seconds).
                //   - We received state=2 then got socket-close: the real ~18 min
                //     server-side approval deadline fired (see ib-agent#76).
                let msg = if saw_challenge {
                    "2FA approval timed out (server closed socket; ~18 min server-side deadline)"
                } else {
                    "2FA gate: server closed socket before issuing a challenge — \
                     likely the account doesn't have IBKey 2FA enabled, or the \
                     server rejected the SWCR_TOKEN format. (Set RUST_LOG=info \
                     for stage-by-stage logs.)"
                };
                return Err(ib_key_err(io::ErrorKind::ConnectionAborted, msg));
            }
            Err(e) => return Err(e),
        };

        match recv {
            RecvMsg::Xyz { msg_id, state, fields, .. } if msg_id == xyz::XYZ_MSG_SWCR_TOKEN && state == 2 => {
                saw_challenge = true;
                let challenge = xyz::parse_swcr_token_challenge(&fields);
                approval_url = challenge.approval_url;
                session_id = challenge.session_id;
                if !announced_wait {
                    log::info!(
                        "2FA gate: awaiting approval (session_id={}, approval_url={})",
                        if session_id.is_empty() { "<unknown>" } else { &session_id },
                        if approval_url.is_empty() { "<not-provided>" } else { &approval_url },
                    );
                    announced_wait = true;
                }
                // Challenge/Response branch: if a code_provider is configured,
                // pull the 8-char code from the callback and submit state=3
                // instead of waiting for a phone tap. The callback runs off
                // this thread because it blocks for as long as the operator
                // needs to read the code off the IBKey app — this loop must
                // keep answering the server's NS_TEST_REQUEST probes (~20 s
                // cadence) in the meantime or the socket is torn down before
                // the code is ever submitted. Guarded so a repeated state=2
                // (server retransmission) doesn't spawn a second provider.
                if !code_submitted && pending_code.is_none() {
                    if let Some(provider) = code_provider {
                        let provider = provider.clone();
                        let challenge_info = IbKeyChallenge {
                            display_id: session_id.clone(),
                            avth_url: approval_url.clone(),
                        };
                        let (tx, rx) = std::sync::mpsc::channel();
                        std::thread::Builder::new()
                            .name("ibkey-code-provider".into())
                            .spawn(move || {
                                let _ = tx.send(provider(challenge_info));
                            })?;
                        // An unattended provider (vault, env, file) resolves at
                        // once and shouldn't have to wait for the next probe to
                        // get its code on the wire, so give it a brief inline
                        // grace period first. Far below the probe cadence, so
                        // nothing is starved; a provider waiting on a human
                        // falls through to the probe-driven path below.
                        match rx.recv_timeout(IB_KEY_PROVIDER_FAST_PATH_GRACE) {
                            Ok(result) => {
                                submit_swcr_code(stream, &result?)?;
                                code_submitted = true;
                            }
                            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                                pending_code = Some(rx);
                            }
                            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                                return Err(provider_panicked());
                            }
                        }
                    }
                }
            }
            RecvMsg::Xyz { msg_id, state, fields, .. } if msg_id == xyz::XYZ_MSG_SWCR_TOKEN && state == 4 => {
                // Challenge/Response result. Server responds PASSED → falls
                // through to AUTH_FINISH (state=3); FAILED → server skips
                // AUTH_FINISH and tears the socket down (no retry loop —
                // ib-agent#149).
                let result = fields.iter().rev().find(|s| !s.is_empty()).cloned().unwrap_or_default();
                if result.eq_ignore_ascii_case("PASSED") {
                    log::info!("2FA gate: C/R code accepted (state=4 PASSED)");
                } else {
                    return Err(ib_key_err(
                        io::ErrorKind::PermissionDenied,
                        format!("2FA gate: C/R code rejected (state=4 {})", result),
                    ));
                }
            }
            RecvMsg::Xyz { msg_id, state, fields, .. } if msg_id == xyz::XYZ_MSG_TOKEN_AUTH && (state == 3 || state == 5) => {
                // Look for "PASSED" sentinel and the SOFT token (long hex string).
                let mut passed = false;
                let mut soft_token_hex = String::new();
                for f in &fields {
                    if f.eq_ignore_ascii_case("PASSED") { passed = true; }
                    else if f.len() >= 32 && f.chars().all(|c| c.is_ascii_hexdigit())
                        && soft_token_hex.is_empty()
                    {
                        soft_token_hex = f.clone();
                    }
                }
                if passed {
                    log::info!("2FA gate: approved");
                    // Per ib-agent#125: AUTH_FINISH carries no token — body is
                    // just `["", "PASSED"]`. The SOFT token used for downstream
                    // farm logons (tag 8483) is the SRP-derived K_soft, which
                    // ibx already computes correctly via `srp_compute_k`
                    // (= SHA1(strip_leading_zeros(S))). No extraction needed.
                    if approval_url.is_empty() && session_id.is_empty()
                        && soft_token_hex.is_empty()
                    {
                        return Ok(IbKeyOutcome::Skipped);
                    }
                    return Ok(IbKeyOutcome::Approved {
                        approval_url, session_id, soft_token_hex,
                    });
                }
                let result = fields.iter().rev().find(|s| !s.is_empty()).map(|s| s.as_str()).unwrap_or("");
                return Err(ib_key_err(
                    io::ErrorKind::PermissionDenied,
                    format!("2FA approval rejected: {}", result),
                ));
            }
            RecvMsg::Ns { msg_type, fields, .. } if msg_type == NS_TEST_REQUEST => {
                let ts = fields.iter().find(|f| !f.is_empty()).cloned().unwrap_or_default();
                let reply = ns_build_heart_beat(NS_VERSION, &ts);
                stream.write_all(&reply)?;
                log::debug!("2FA gate: heartbeat {} -> 531", ts);
            }
            RecvMsg::Ns { msg_type, .. } if msg_type == NS_ERROR_RESPONSE
                || msg_type == NS_SECURE_ERROR =>
            {
                return Err(ib_key_err(
                    io::ErrorKind::Other,
                    format!("2FA gate: server error type={}", msg_type),
                ));
            }
            other => {
                // Unknown message during 2FA wait. Log and keep looping —
                // the server may send other informational frames.
                log::warn!("2FA gate: unexpected message {:?}", other);
            }
        }

        // A provider that outlived the grace period yields here instead. Polling
        // on inbound traffic keeps the socket single-threaded, at the cost of
        // trailing the operator's entry by up to one probe interval (~20 s); if
        // that ever matters, give the stream a read timeout and select on both.
        if let Some(rx) = pending_code.as_ref() {
            match rx.try_recv() {
                Ok(result) => {
                    pending_code = None;
                    // Don't burn a single-use code on a login we're about to
                    // abandon: the deadline check at the top of the loop would
                    // fire before the server's answer could be read.
                    if Instant::now() < deadline {
                        submit_swcr_code(stream, &result?)?;
                        code_submitted = true;
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    return Err(provider_panicked());
                }
            }
        }
    }
}

/// Result of soft token authentication attempt.
pub enum SoftTokenOutcome {
    /// Token accepted.
    Passed,
    /// Token not recognized (state 5 / "UNKNOWN") — SRP fallback needed.
    Unknown,
}

pub fn do_soft_token(
    stream: &mut TcpStream,
    session_token: &BigUint,
    carry: &mut Vec<u8>,
) -> io::Result<SoftTokenOutcome> {
    use sha1::{Digest, Sha1};

    // State 1: Send empty init (FIX-framed for farm)
    let msg1 = xyz::xyz_build_soft_token(1, "", "", "");
    stream.write_all(&wrap_xyz_fix(&msg1))?;

    // State 2: Receive challenge (FIX-framed)
    let recv2 = recv_8eq1(stream, carry)?;
    let xyz2 = extract_xyz(&recv2);
    let (_, _, state2, fields2) = xyz::xyz_parse_response(xyz2)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "SOFT_TOKEN: invalid XYZ state 2"))?;

    if state2 == 5 {
        // Farm rejected soft token — SRP fallback needed.
        // See: https://github.com/deepentropy/ibx/issues/123
        log::warn!("SOFT_TOKEN: farm returned state 5 (UNKNOWN) — SRP fallback needed");
        return Ok(SoftTokenOutcome::Unknown);
    }
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
    let recv4 = recv_8eq1(stream, carry)?;
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
        Ok(SoftTokenOutcome::Passed)
    } else if result == "UNKNOWN" {
        log::warn!("SOFT_TOKEN: farm returned UNKNOWN — SRP fallback needed");
        Ok(SoftTokenOutcome::Unknown)
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("SOFT_TOKEN auth failed: {}", result),
        ))
    }
}

/// SRP-6 authentication for farm connections using FIX framing (8=1).
/// Called as fallback when `do_soft_token` returns `SoftTokenOutcome::Unknown`.
/// Same SRP math as `do_srp`, different wire framing.
pub fn do_srp_farm(
    stream: &mut TcpStream,
    username: &str,
    password: &str,
    carry: &mut Vec<u8>,
) -> io::Result<()> {
    let n = srp::srp_n();
    let g = BigUint::from(srp::SRP_G);

    // State 1: Send AUTH_QUERY (FIX-framed)
    let msg1 = xyz::xyz_build_srp_v20(1, &[]);
    stream.write_all(&wrap_xyz_fix(&msg1))?;

    // State 2: Receive AUTH_PARAMS (FIX-framed)
    let recv2 = recv_8eq1(stream, carry)?;
    let xyz2 = extract_xyz(&recv2);
    let (_, _, state2, fields2) = xyz::xyz_parse_response(xyz2)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Farm SRP: invalid state 2"))?;

    if state2 != 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Farm SRP: expected state 2, got {}", state2),
        ));
    }

    let data_fields = extract_srp_data(&fields2, username);
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

    // Generate client keys with 32-byte private key
    let mut a_bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut a_bytes);
    let a_priv = BigUint::from_bytes_be(&a_bytes);
    let a_pub = g.modpow(&a_priv, &n);

    // State 3: Send client public key A (FIX-framed)
    let a_hex = format!("{:x}", a_pub);
    let msg3 = xyz::xyz_build_srp_v20(3, &[("L", &a_hex)]);
    stream.write_all(&wrap_xyz_fix(&msg3))?;

    // State 4: Receive salt + B (FIX-framed)
    let recv4 = recv_8eq1(stream, carry)?;
    let xyz4 = extract_xyz(&recv4);
    let (_, _, state4, fields4) = xyz::xyz_parse_response(xyz4)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Farm SRP: invalid state 4"))?;

    if state4 == 7 {
        let result = fields4.get(9).map(|s| s.as_str()).unwrap_or("FAILED");
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("Farm SRP early error (state 7): {}", result),
        ));
    }
    if state4 != 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Farm SRP: expected state 4, got {}", state4),
        ));
    }

    let data_fields = extract_srp_data(&fields4, username);
    if data_fields.len() < 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Farm SRP: missing salt/B in state 4",
        ));
    }

    let salt_hex = &data_fields[0];
    let b_hex = &data_fields[1];
    let salt_bytes = hex::decode(salt_hex)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    let b_pub = BigUint::parse_bytes(b_hex.as_bytes(), 16).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "Farm SRP: invalid B hex")
    })?;

    // Compute SRP values (same math as do_srp)
    let x = srp::srp_compute_x(strip_leading_zeros(&salt_bytes), username, password);
    let u = srp::srp_compute_u(&a_pub, &b_pub);
    let k_mult = BigUint::from(srp::SRP_K);
    let s = srp::srp_compute_s(&b_pub, &a_priv, &u, &x, &n, &g, &k_mult);
    let k = srp::srp_compute_k(&s);

    let salt_int = BigUint::parse_bytes(salt_hex.as_bytes(), 16).unwrap_or_default();
    let m1 = srp::srp_compute_m1(&n, &g, username, &salt_int, &a_pub, &b_pub, &k);

    // State 5: Send client proof M1 (FIX-framed)
    let m1_hex = format!("{:x}", m1);
    let msg5 = xyz::xyz_build_srp_v20(5, &[("N", &m1_hex)]);
    stream.write_all(&wrap_xyz_fix(&msg5))?;

    // State 6: Receive AUTH_RESULT (FIX-framed)
    let recv6 = recv_8eq1(stream, carry)?;
    let xyz6 = extract_xyz(&recv6);
    let (_, _, state6, fields6) = xyz::xyz_parse_response(xyz6)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Farm SRP: invalid state 6"))?;

    let result = fields6
        .get(9)
        .filter(|s| !s.is_empty())
        .or_else(|| fields6.iter().rev().find(|s| !s.is_empty()))
        .map(|s| s.as_str())
        .unwrap_or("");

    if state6 == 6 && result == "PASSED" {
        log::info!("Farm SRP auth PASSED");
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("Farm SRP FAILED (state={}): {}", state6, result),
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

    // ── IBKey Challenge/Response gate (ibx#176) ─────────────────────────────

    /// Timestamp echoed in the scripted server's keepalive probes.
    const PROBE_TS: &str = "20260430-22:58:25";

    /// Scripted gateway for the Challenge/Response gate. Serves `head`, then
    /// keeps issuing `NS_TEST_REQUEST` probes — as the live server does every
    /// ~20 s while the operator reads the code off the IBKey app — until the
    /// client submits `expect` (the state=3 frame), then serves `tail`.
    ///
    /// This is what makes the C/R tests deterministic: the result frames can't
    /// arrive before the code does. It models a server that keeps probing, not
    /// one that enforces the probe, so what a blocked gate loses here is the
    /// heartbeat ordering the tests assert on, not the frames themselves.
    struct ProbingGateway {
        pending: Vec<u8>,
        pos: usize,
        tail: Vec<u8>,
        expect: Vec<u8>,
        served_tail: bool,
        probes_left: u32,
        written: Vec<u8>,
    }

    impl ProbingGateway {
        fn new(head: Vec<u8>, expect: Vec<u8>, tail: Vec<u8>) -> Self {
            Self {
                pending: head,
                pos: 0,
                tail,
                expect,
                served_tail: false,
                // Bounded (~10 s at the pacing below) so a client that never
                // submits ends the test instead of probing forever.
                probes_left: 5_000,
                written: Vec::new(),
            }
        }

        fn code_submitted(&self) -> bool {
            !self.expect.is_empty()
                && self.written.windows(self.expect.len()).any(|f| f == self.expect)
        }
    }

    impl Read for ProbingGateway {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.pos >= self.pending.len() {
                if self.code_submitted() && !self.served_tail {
                    self.served_tail = true;
                    self.pending = std::mem::take(&mut self.tail);
                } else if self.probes_left > 0 && !self.served_tail {
                    self.probes_left -= 1;
                    // Paced so the probe stream stands in for the live server's
                    // ~20 s cadence without burning through the budget.
                    std::thread::sleep(std::time::Duration::from_millis(1));
                    self.pending = ns_build(NS_VERSION, NS_TEST_REQUEST, &[PROBE_TS], "MISC");
                } else {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "script exhausted"));
                }
                self.pos = 0;
            }
            let n = buf.len().min(self.pending.len() - self.pos);
            buf[..n].copy_from_slice(&self.pending[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    impl Write for ProbingGateway {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.written.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> { Ok(()) }
    }

    // ── recv_8eq1 framing / coalesced tail (ibx#237) ────────────────────────

    /// Build a framed `8=1` message with the given inner payload.
    fn framed_8eq1(payload: &[u8]) -> Vec<u8> {
        let body = {
            let mut b = b"35=X\x01".to_vec();
            b.extend_from_slice(payload);
            b
        };
        let mut msg = format!("8=1\x019={:04}\x01", body.len()).into_bytes();
        msg.extend_from_slice(&body);
        msg
    }

    #[test]
    fn try_frame_8eq1_partial_and_complete() {
        let msg = framed_8eq1(b"hello");
        // One byte short → not framed yet.
        assert_eq!(try_frame_8eq1(&msg[..msg.len() - 1]).unwrap(), None);
        // Exact message → framed at its own length.
        assert_eq!(try_frame_8eq1(&msg).unwrap(), Some(msg.len()));
        // Extra trailing bytes → framed length still stops at the message.
        let mut with_tail = msg.clone();
        with_tail.extend_from_slice(b"trailing-bytes");
        assert_eq!(try_frame_8eq1(&with_tail).unwrap(), Some(msg.len()));
    }

    #[test]
    fn try_frame_8eq1_rejects_oversized_body() {
        let hdr = format!("8=1\x019={}\x01", MAX_FARM_MSG_SIZE + 1).into_bytes();
        assert!(try_frame_8eq1(&hdr).is_err());
    }

    /// The exact ibx#237 regression: a gateway coalesces the final auth
    /// response and the farm logon ACK that follows it into one TCP read.
    /// `recv_8eq1` must return only the framed `8=1` message and preserve the
    /// trailing ACK bytes in the carry buffer, not discard them.
    #[test]
    fn recv_8eq1_preserves_coalesced_tail() {
        use std::io::Write;
        use std::net::TcpListener;

        let auth_msg = framed_8eq1(b"PASSED");
        // Stand-in for the farm logon ACK the gateway pipelines right after.
        let ack_tail = b"8=FIX.4.1\x019=0005\x0135=A\x0110=000\x01".to_vec();

        let mut wire = auth_msg.clone();
        wire.extend_from_slice(&ack_tail);

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            // Single write: the two messages arrive coalesced in one read.
            s.write_all(&wire).unwrap();
            // Hold the socket open so the client is not tricked by a close.
            std::thread::sleep(std::time::Duration::from_millis(100));
        });

        let mut client = TcpStream::connect(addr).unwrap();
        client
            .set_read_timeout(Some(std::time::Duration::from_millis(FARM_LOGON_POLL_MS)))
            .unwrap();

        let mut carry = Vec::new();
        let got = recv_8eq1(&mut client, &mut carry).unwrap();

        assert_eq!(got, auth_msg, "framed message must stop at the 8=1 boundary");
        assert_eq!(
            carry, ack_tail,
            "coalesced farm logon ACK bytes must survive in the carry buffer"
        );

        // A second call returns the buffered ACK without touching the socket
        // (it is not a valid 8=1 frame, so this would block on read if the tail
        // had been lost — proving the bytes are actually retained).
        assert_eq!(try_frame_8eq1(&carry).unwrap(), None);

        server.join().unwrap();
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

    // The machine_id is persistent (read from the hwid file / IBX_HWID env,
    // created once — see read_or_create_hwid, ib-agent#132), so repeated calls
    // must return the SAME id. This test previously asserted the pre-#132
    // behavior (random id per call) and failed once a hwid file existed.
    #[test]
    fn hw_info_machine_id_is_stable_across_calls() {
        let info1 = get_hw_info();
        let info2 = get_hw_info();
        let machine1 = info1.split('|').next().unwrap();
        let machine2 = info2.split('|').next().unwrap();
        assert_eq!(machine1, machine2, "Persistent machine ID must not change between calls");
        assert_eq!(machine1.len(), 8);
        assert!(machine1.chars().all(|c| c.is_ascii_hexdigit()),
            "machine ID must be 8 hex chars, got {machine1:?}");
    }

    #[test]
    fn hw_info_mac_format_is_six_hex_octets() {
        let info = get_hw_info();
        let mac = info.split('|').nth(1).unwrap();
        let octets: Vec<&str> = mac.split(':').collect();
        assert_eq!(octets.len(), 6, "MAC must be 6 colon-separated octets, got {mac:?}");
        for o in &octets {
            assert_eq!(o.len(), 2);
            assert!(o.chars().all(|c| c.is_ascii_hexdigit()), "non-hex octet {o:?}");
        }
        // Either a real NIC MAC, or the documented zero fallback when no NIC
        // exposes one (e.g. CI runners with no networking).
    }

    #[test]
    fn lan_ip_returns_a_valid_ipv4_or_loopback_fallback() {
        let ip = get_lan_ip();
        // Either a routable address or the documented loopback fallback.
        let parsed: Result<std::net::IpAddr, _> = ip.parse();
        assert!(parsed.is_ok(), "get_lan_ip returned non-parseable address {ip:?}");
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
    fn session_token_bytes_roundtrip_nonzero() {
        // 0x010203 → [0x01, 0x02, 0x03]; round-trip through BigUint::from_bytes_be.
        let token = BigUint::from(0x010203u32);
        let ar = AuthResult {
            session_token: token.clone(),
            token_type: "st".to_string(),
            session_id: String::new(),
            features: Vec::new(),
            authenticated: true,
        };
        let bytes = ar.session_token_bytes();
        assert_eq!(bytes, vec![0x01, 0x02, 0x03]);
        assert_eq!(BigUint::from_bytes_be(&bytes), token);
    }

    #[test]
    fn session_token_bytes_roundtrip_large() {
        let token = BigUint::parse_bytes(
            b"deadbeefcafebabe0123456789abcdef",
            16,
        ).unwrap();
        let ar = AuthResult {
            session_token: token.clone(),
            token_type: "tst".to_string(),
            session_id: String::new(),
            features: Vec::new(),
            authenticated: true,
        };
        let bytes = ar.session_token_bytes();
        assert_eq!(BigUint::from_bytes_be(&bytes), token);
    }

    #[test]
    fn session_token_bytes_zero_keeps_single_byte() {
        // BigUint::ZERO → to_bytes_be returns [0x00] (or empty); strip_leading_zeros
        // is documented to retain a single 0x00 byte for the all-zero case.
        let ar = AuthResult {
            session_token: BigUint::ZERO,
            token_type: String::new(),
            session_id: String::new(),
            features: Vec::new(),
            authenticated: false,
        };
        let bytes = ar.session_token_bytes();
        assert_eq!(BigUint::from_bytes_be(&bytes), BigUint::ZERO);
    }

    #[test]
    fn session_token_bytes_strips_leading_zero_high_bit() {
        // BigUint with high bit set in first byte should not have leading zero padding.
        let token = BigUint::parse_bytes(b"80ff", 16).unwrap();
        let ar = AuthResult {
            session_token: token.clone(),
            token_type: "zenith".to_string(),
            session_id: String::new(),
            features: Vec::new(),
            authenticated: true,
        };
        let bytes = ar.session_token_bytes();
        assert_eq!(bytes, vec![0x80, 0xff]);
        assert_eq!(BigUint::from_bytes_be(&bytes), token);
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

    // ── recv_secure ──────────────────────────────────────────────────────

    /// Build a fake NS frame with the given text payload.
    fn build_ns_frame(payload: &str) -> Vec<u8> {
        let bytes = payload.as_bytes();
        let mut frame = Vec::with_capacity(8 + bytes.len());
        frame.extend_from_slice(ns::NS_MAGIC);
        frame.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        frame.extend_from_slice(bytes);
        frame
    }

    #[test]
    fn recv_secure_redirect_returns_target() {
        let frame = build_ns_frame("50;524;ndc1.ibllc.com:4000;");
        let mut cursor = io::Cursor::new(frame);
        let mut channel = SecureChannel::new();
        let err = recv_secure(&mut cursor, &mut channel).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::ConnectionReset);
        assert!(err.to_string().starts_with("REDIRECT:"));
        assert!(err.to_string().contains("ndc1.ibllc.com:4000"));
    }

    #[test]
    fn recv_secure_error_still_works() {
        let frame = build_ns_frame("50;535;some error message;");
        let mut cursor = io::Cursor::new(frame);
        let mut channel = SecureChannel::new();
        let err = recv_secure(&mut cursor, &mut channel).unwrap_err();
        assert!(err.to_string().contains("Auth error"));
    }

    #[test]
    fn recv_ns_error_response_519() {
        let frame = build_ns_frame("50;519;1;malformed user name;");
        let mut cursor = io::Cursor::new(frame);
        let mut channel = SecureChannel::new();
        let err = recv_secure(&mut cursor, &mut channel).unwrap_err();
        assert!(err.to_string().contains("Auth error"));
        assert!(err.to_string().contains("malformed user name"));
    }

    #[test]
    fn recv_secure_unknown_type_returns_error() {
        let frame = build_ns_frame("50;999;payload;");
        let mut cursor = io::Cursor::new(frame);
        let mut channel = SecureChannel::new();
        let err = recv_secure(&mut cursor, &mut channel).unwrap_err();
        assert!(err.to_string().contains("Expected 534, got 999"));
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

    // ── do_ib_key_2fa ───────────────────────────────────────────────────

    /// Bidirectional in-memory stream for testing: scripted reads, captured writes.
    struct ScriptedStream {
        incoming: Vec<u8>,
        read_pos: usize,
        written: Vec<u8>,
    }

    impl ScriptedStream {
        fn new(incoming: Vec<u8>) -> Self {
            Self { incoming, read_pos: 0, written: Vec::new() }
        }
    }

    impl io::Read for ScriptedStream {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let remaining = self.incoming.len().saturating_sub(self.read_pos);
            if remaining == 0 {
                // Mirrors a server socket close.
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "scripted EOF"));
            }
            let n = remaining.min(buf.len());
            buf[..n].copy_from_slice(&self.incoming[self.read_pos..self.read_pos + n]);
            self.read_pos += n;
            Ok(n)
        }
    }

    impl io::Write for ScriptedStream {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.written.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> { Ok(()) }
    }

    /// Wrap an XYZ binary payload in `#%#%` framing.
    fn frame_xyz(payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + payload.len());
        out.extend_from_slice(ns::NS_MAGIC);
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        out.extend_from_slice(payload);
        out
    }

    fn far_future_deadline() -> std::time::Instant {
        std::time::Instant::now() + std::time::Duration::from_secs(60)
    }

    #[test]
    fn ib_key_2fa_skipped_when_server_passes_immediately() {
        // Server replies AUTH_FINISH(771) state=5 PASSED right after our init —
        // no SWCR_TOKEN(state=2) preceded it, so this is the no-2FA fast path.
        let auth_finish = xyz::xyz_build(xyz::XYZ_MSG_TOKEN_AUTH, 5, "user", &["PASSED"]);
        let mut stream = ScriptedStream::new(frame_xyz(&auth_finish));
        let outcome = do_ib_key_2fa(&mut stream, "2a", far_future_deadline(), None).unwrap();
        assert_eq!(outcome, IbKeyOutcome::Skipped);

        // Verify we sent the SWCR_TOKEN init carrying the tokenSubType.
        let written_payload = &stream.written[8..]; // skip NS frame header
        let (msg_id, _, state, fields) = xyz::xyz_parse_response(written_payload).unwrap();
        assert_eq!(msg_id, xyz::XYZ_MSG_SWCR_TOKEN);
        assert_eq!(state, 1);
        // Per the canonical layout (see ib-agent#123), tokenSubType is the
        // last (and only non-empty) string field; preceding slots are empty.
        assert_eq!(fields.last().map(|s| s.as_str()), Some("2a"),
            "tokenSubType must be the last field; got {:?}", fields);
    }

    #[test]
    fn ib_key_2fa_approved_after_state_2_and_passed() {
        // Server sends SWCR_TOKEN(state=2) carrying the approval URL, then
        // AUTH_FINISH(state=5) PASSED after the user "approves".
        let challenge = xyz::xyz_build(xyz::XYZ_MSG_SWCR_TOKEN, 2, "user", &[
            "e7429fde5b4c26f81fff956be6749908a8653558e7429fde5b4c26f81fff956b",
            "580 820",
            "https://www.example.com/seamless?S=YWJjZA==",
        ]);
        let auth_finish = xyz::xyz_build(xyz::XYZ_MSG_TOKEN_AUTH, 5, "user", &["PASSED"]);
        let mut incoming = frame_xyz(&challenge);
        incoming.extend_from_slice(&frame_xyz(&auth_finish));
        let mut stream = ScriptedStream::new(incoming);

        let outcome = do_ib_key_2fa(&mut stream, "2a", far_future_deadline(), None).unwrap();
        match outcome {
            IbKeyOutcome::Approved { approval_url, session_id, soft_token_hex } => {
                assert_eq!(approval_url, "https://www.example.com/seamless?S=YWJjZA==");
                assert_eq!(session_id, "580 820");
                let _ = soft_token_hex;
            }
            other => panic!("expected Approved, got {:?}", other),
        }
    }

    #[test]
    fn ib_key_2fa_echoes_test_request_timestamp() {
        // Mid-wait: server probes with NS_TEST_REQUEST. Client must echo the
        // timestamp in an NS_HEART_BEAT before the final AUTH_FINISH PASSED.
        let challenge = xyz::xyz_build(xyz::XYZ_MSG_SWCR_TOKEN, 2, "user", &[
            "e7429fde5b4c26f81fff956be6749908a8653558e7429fde5b4c26f81fff956b",
            "580 820",
            "https://x.example/u",
        ]);
        let test_req = ns::ns_build(NS_VERSION, ns::NS_TEST_REQUEST,
            &["20260430-22:58:25"], "MISC");
        let auth_finish = xyz::xyz_build(xyz::XYZ_MSG_TOKEN_AUTH, 5, "user", &["PASSED"]);
        let mut incoming = frame_xyz(&challenge);
        incoming.extend_from_slice(&test_req);
        incoming.extend_from_slice(&frame_xyz(&auth_finish));
        let mut stream = ScriptedStream::new(incoming);

        let outcome = do_ib_key_2fa(&mut stream, "2a", far_future_deadline(), None).unwrap();
        assert!(matches!(outcome, IbKeyOutcome::Approved { .. }));

        // The captured write stream contains: SWCR_TOKEN init, then HEART_BEAT.
        // Walk the frames and find the HEART_BEAT.
        let mut offset = 0;
        let mut saw_heartbeat = false;
        while offset + 8 <= stream.written.len() {
            assert_eq!(&stream.written[offset..offset + 4], ns::NS_MAGIC);
            let len = u32::from_be_bytes(
                stream.written[offset + 4..offset + 8].try_into().unwrap(),
            ) as usize;
            let payload = &stream.written[offset + 8..offset + 8 + len];
            if let Some((_, msg_type, fields)) = ns::ns_parse(payload) {
                if msg_type == ns::NS_HEART_BEAT {
                    assert_eq!(fields, vec!["20260430-22:58:25".to_string()]);
                    saw_heartbeat = true;
                }
            }
            offset += 8 + len;
        }
        assert!(saw_heartbeat, "client must echo the test-request timestamp in a HEART_BEAT");
    }

    #[test]
    fn ib_key_2fa_socket_close_during_wait_is_aborted() {
        // Server sends state=2 then closes the socket — the ~18 min server-side
        // deadline. The function must surface this as ConnectionAborted with
        // the long-deadline message (saw_challenge=true).
        let challenge = xyz::xyz_build(xyz::XYZ_MSG_SWCR_TOKEN, 2, "user", &[
            "e7429fde5b4c26f81fff956be6749908a8653558e7429fde5b4c26f81fff956b",
            "580 820",
            "https://x.example/u",
        ]);
        let mut stream = ScriptedStream::new(frame_xyz(&challenge));
        let err = do_ib_key_2fa(&mut stream, "2a", far_future_deadline(), None).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::ConnectionAborted);
        assert!(err.to_string().contains("18 min server-side deadline"),
            "expected the long-deadline message; got {}", err);
    }

    #[test]
    fn ib_key_2fa_socket_close_before_challenge_says_likely_rejection() {
        // Server closes the socket immediately after our SWCR_TOKEN init —
        // no challenge ever arrived. The diagnostic message must NOT blame
        // the 18 min deadline (which would mislead users into "approve faster"
        // when the real fix is "your account doesn't use IBKey").
        let mut stream = ScriptedStream::new(Vec::new());
        let err = do_ib_key_2fa(&mut stream, "2a", far_future_deadline(), None).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::ConnectionAborted);
        let msg = err.to_string();
        assert!(msg.contains("before issuing a challenge"),
            "expected rejection-style message; got {}", msg);
        assert!(!msg.contains("18 min"),
            "must not mention the 18 min deadline when no challenge was seen; got {}", msg);
    }

    #[test]
    fn ib_key_2fa_rejected_when_passed_string_is_failed() {
        // Server replies AUTH_FINISH state=5 but payload says FAILED — denial.
        let auth_finish = xyz::xyz_build(xyz::XYZ_MSG_TOKEN_AUTH, 5, "user", &["FAILED"]);
        let mut stream = ScriptedStream::new(frame_xyz(&auth_finish));
        let err = do_ib_key_2fa(&mut stream, "2a", far_future_deadline(), None).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(err.to_string().contains("rejected"));
    }

    // ── do_ib_key_2fa — Challenge/Response (ib-agent#149) ───────────────

    #[test]
    fn ib_key_2fa_cr_submits_code_then_passes_on_auth_finish_state_3() {
        // Replay of ib-agent#149 run A (success path). Captured fixtures:
        //   state=2: sessionId="399 830", challenge=10a447bc…0c9dc714 (20B / 40 hex),
        //            AVTH_URL=clientam.com/ibkr/ibkey/seamless?S=…
        //   user types "02226534" from the IBKey app
        //   state=4 PASSED  → AUTH_FINISH(771) state=3 PASSED  (note: push uses state=5)
        const RUN_A_CHALLENGE_HEX: &str = "10a447bc4f269b5161a6133b0265cf590c9dc714";
        const RUN_A_SESSION_ID: &str = "399 830";
        const RUN_A_AVTH_URL: &str =
            "https://www.clientam.com/ibkr/ibkey/seamless?S=eyJBVlRIX1VSTCI6Imh0dHBzOi8vemRjMS5pYmxsYy5jb206NDAwMS9zYS94RUI1c3hUVDcvSGpuTTlFQksifQ==";
        const RUN_A_CODE: &str = "02226534";

        let challenge = xyz::xyz_build(xyz::XYZ_MSG_SWCR_TOKEN, 2, "johnbegood", &[
            RUN_A_CHALLENGE_HEX,
            RUN_A_SESSION_ID,
            RUN_A_AVTH_URL,
        ]);
        let state4_passed = xyz::xyz_build(xyz::XYZ_MSG_SWCR_TOKEN, 4, "johnbegood", &["PASSED"]);
        let auth_finish = xyz::xyz_build(xyz::XYZ_MSG_TOKEN_AUTH, 3, "johnbegood", &["PASSED"]);
        let mut tail = frame_xyz(&state4_passed);
        tail.extend_from_slice(&frame_xyz(&auth_finish));
        let submission = frame_xyz(&xyz::xyz_build_swcr_token_code_submission(RUN_A_CODE));
        let mut stream = ProbingGateway::new(frame_xyz(&challenge), submission.clone(), tail);

        let seen_challenge = std::sync::Arc::new(std::sync::Mutex::new(IbKeyChallenge::default()));
        let seen_clone = seen_challenge.clone();
        // Unattended provider: resolves inside the fast-path grace, so the code
        // goes out without waiting on a probe.
        let provider: CodeProvider = std::sync::Arc::new(move |c: IbKeyChallenge| {
            *seen_clone.lock().unwrap() = c;
            Ok(RUN_A_CODE.to_string())
        });

        let outcome = do_ib_key_2fa(&mut stream, "2a", far_future_deadline(), Some(&provider)).unwrap();
        match outcome {
            IbKeyOutcome::Approved { approval_url, session_id, .. } => {
                assert_eq!(session_id, RUN_A_SESSION_ID);
                assert_eq!(approval_url, RUN_A_AVTH_URL);
            }
            other => panic!("expected Approved, got {:?}", other),
        }

        // Provider must have received the parsed display_id + avth_url.
        let seen = seen_challenge.lock().unwrap().clone();
        assert_eq!(seen.display_id, RUN_A_SESSION_ID);
        assert_eq!(seen.avth_url, RUN_A_AVTH_URL);

        // The state=3 frame must be byte-for-byte the 40-byte run-A capture.
        assert!(stream.written.windows(submission.len()).any(|f| f == submission),
            "state=3 submission must byte-match the ib-agent#149 run-A capture");
        // Nothing was waited on: an unattended login submits before any probe.
        let heartbeat = ns_build_heart_beat(NS_VERSION, PROBE_TS);
        assert!(!stream.written.windows(heartbeat.len()).any(|f| f == heartbeat),
            "an immediate provider must not have to wait for a probe");
    }

    /// A provider that waits on a human outlives the fast-path grace, so the
    /// code lands on the probe-driven path. The gate must answer the server's
    /// keepalives throughout: called inline, it answers none of them, which is
    /// what cost the C/R path its connection (ibx#244).
    #[test]
    fn ib_key_2fa_cr_slow_provider_keeps_heartbeats_flowing() {
        const CODE: &str = "02226534";
        let challenge = xyz::xyz_build(xyz::XYZ_MSG_SWCR_TOKEN, 2, "user", &[
            "10a447bc4f269b5161a6133b0265cf590c9dc714",
            "399 830",
            "https://x.example/u",
        ]);
        let state4_passed = xyz::xyz_build(xyz::XYZ_MSG_SWCR_TOKEN, 4, "user", &["PASSED"]);
        let auth_finish = xyz::xyz_build(xyz::XYZ_MSG_TOKEN_AUTH, 3, "user", &["PASSED"]);
        let mut tail = frame_xyz(&state4_passed);
        tail.extend_from_slice(&frame_xyz(&auth_finish));
        let submission = frame_xyz(&xyz::xyz_build_swcr_token_code_submission(CODE));
        let mut stream = ProbingGateway::new(frame_xyz(&challenge), submission.clone(), tail);

        let provider: CodeProvider = std::sync::Arc::new(|_| {
            std::thread::sleep(IB_KEY_PROVIDER_FAST_PATH_GRACE + std::time::Duration::from_millis(200));
            Ok(CODE.to_string())
        });

        let outcome = do_ib_key_2fa(&mut stream, "2a", far_future_deadline(), Some(&provider)).unwrap();
        assert!(matches!(outcome, IbKeyOutcome::Approved { .. }));

        let sent = &stream.written;
        let heartbeat = ns_build_heart_beat(NS_VERSION, PROBE_TS);
        let hb_at = sent.windows(heartbeat.len()).position(|f| f == heartbeat)
            .expect("heartbeat reply must be sent while the provider is running");
        let code_at = sent.windows(submission.len()).position(|f| f == submission)
            .expect("state=3 submission must reach the wire");
        assert!(hb_at < code_at, "heartbeat must be answered before the code is submitted");
    }

    #[test]
    fn ib_key_2fa_cr_code_rejected_on_state_4_failed() {
        // Server: state=2 → (client submits a wrong code) → state=4 FAILED. The
        // server tears the socket down after, with no AUTH_FINISH on the wire.
        // Client must surface PermissionDenied on FAILED, without waiting for
        // further frames. The gateway withholds FAILED until the code is on the
        // wire, so this also pins that the rejection follows a real submission.
        const WRONG_CODE: &str = "99999999";
        let challenge = xyz::xyz_build(xyz::XYZ_MSG_SWCR_TOKEN, 2, "user", &[
            "e7429fde5b4c26f81fff956be6749908a8653558e7429fde5b4c26f81fff956b",
            "399 830",
            "https://x.example/u",
        ]);
        let state4_failed = xyz::xyz_build(xyz::XYZ_MSG_SWCR_TOKEN, 4, "user", &["FAILED"]);
        let submission = frame_xyz(&xyz::xyz_build_swcr_token_code_submission(WRONG_CODE));
        let mut stream = ProbingGateway::new(
            frame_xyz(&challenge), submission.clone(), frame_xyz(&state4_failed),
        );

        let provider: CodeProvider = std::sync::Arc::new(|_| Ok(WRONG_CODE.to_string()));
        let err = do_ib_key_2fa(&mut stream, "2a", far_future_deadline(), Some(&provider)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(err.to_string().contains("C/R code rejected"),
            "expected C/R rejection message; got {}", err);
        assert!(stream.written.windows(submission.len()).any(|f| f == submission),
            "the rejected code must have been submitted as state=3");
    }

    #[test]
    fn ib_key_2fa_cr_provider_error_aborts_login() {
        // Provider returns an error (e.g. user cancelled): the function must
        // propagate it without sending state=3.
        let challenge = xyz::xyz_build(xyz::XYZ_MSG_SWCR_TOKEN, 2, "user", &[
            "e7429fde5b4c26f81fff956be6749908a8653558e7429fde5b4c26f81fff956b",
            "399 830",
            "https://x.example/u",
        ]);
        let mut stream = ProbingGateway::new(frame_xyz(&challenge), Vec::new(), Vec::new());
        let provider: CodeProvider = std::sync::Arc::new(|_| {
            Err(io::Error::new(io::ErrorKind::Interrupted, "user cancelled"))
        });
        let err = do_ib_key_2fa(&mut stream, "2a", far_future_deadline(), Some(&provider)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
    }
}
