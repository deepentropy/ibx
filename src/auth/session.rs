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

/// Kind of a login error answer, read from its code as the reference does
/// (ibx#423).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginErrorKind {
    /// Login from this IP address is not authorized.
    IpNotAuthorized,
    /// The user is locked out.
    LockedOut,
    /// Invalid user name or password.
    BadCredentials,
    /// The site is down. Retried with backoff.
    SiteDown,
    /// The site is not ready. Retried with backoff.
    SiteNotReady,
    /// Login restricted (sanctions, anonymous proxy, blocked address).
    Restricted,
    /// No demo user allocated.
    DemoUserNotAllocated,
    /// Live mode selected with a paper user.
    PaperUserInLiveMode,
    /// The user has no paper user.
    NoPaperMapping,
    /// The user has several paper users.
    SeveralPaperUsers,
    /// Paper logons are not allowed for this user.
    PaperLogonNotAllowed,
    /// The preview needs the paper mode.
    PreviewNeedsPaper,
    /// The password was rejected.
    PasswordRejected,
    /// Secure-error answer: the server refused the encrypted session.
    SecureConnectionRefused,
    /// Any other code, or a code that is not a number.
    Other,
}

impl LoginErrorKind {
    /// Kind of an error answer with `code` and server `text`.
    pub fn from_code(code: Option<i64>, text: &str) -> Self {
        match code {
            Some(1) if text.contains("IP address") => Self::IpNotAuthorized,
            Some(1) if text.contains("lockedout") => Self::LockedOut,
            Some(1) => Self::BadCredentials,
            Some(4) => Self::SiteDown,
            Some(5) => Self::SiteNotReady,
            Some(10..=12) => Self::Restricted,
            Some(13) => Self::DemoUserNotAllocated,
            Some(14) => Self::PaperUserInLiveMode,
            Some(15) => Self::NoPaperMapping,
            Some(16) => Self::SeveralPaperUsers,
            Some(17..=19) => Self::PaperLogonNotAllowed,
            Some(20) => Self::PreviewNeedsPaper,
            Some(22 | 23) => Self::PasswordRejected,
            _ => Self::Other,
        }
    }

    /// Site down and site not ready are retried with backoff, as the
    /// reference does; every other kind stops the login.
    pub fn is_retryable(self) -> bool {
        matches!(self, Self::SiteDown | Self::SiteNotReady)
    }

    fn describe(self) -> &'static str {
        match self {
            Self::IpNotAuthorized => "login from this IP address is not authorized",
            Self::LockedOut => "user locked out",
            Self::BadCredentials => "invalid user name or password",
            Self::SiteDown => "site down",
            Self::SiteNotReady => "site not ready",
            Self::Restricted => "login restricted",
            Self::DemoUserNotAllocated => "demo user not allocated",
            Self::PaperUserInLiveMode => "live mode selected, but the user is a paper user",
            Self::NoPaperMapping => "no paper user for this user",
            Self::SeveralPaperUsers => "several paper users for this user",
            Self::PaperLogonNotAllowed => "paper logons for this user are not allowed",
            Self::PreviewNeedsPaper => "the preview needs the paper mode",
            Self::PasswordRejected => "password rejected",
            Self::SecureConnectionRefused => "secure connection refused",
            Self::Other => "server error",
        }
    }
}

/// A login error answer from the server, carried inside the `io::Error` of
/// the login (ibx#423). Read it back with [`login_error`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginError {
    pub kind: LoginErrorKind,
    /// Code of the answer; `None` when it is not a number or the answer
    /// has no code.
    pub code: Option<i64>,
    /// Server text.
    pub text: String,
}

impl std::fmt::Display for LoginError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Auth error: {}", self.kind.describe())?;
        if let Some(code) = self.code {
            write!(f, " (code {})", code)?;
        }
        if !self.text.is_empty() {
            write!(f, ": {}", self.text)?;
        }
        Ok(())
    }
}

impl std::error::Error for LoginError {}

impl From<LoginError> for io::Error {
    fn from(e: LoginError) -> Self {
        let kind = match e.kind {
            LoginErrorKind::SiteDown | LoginErrorKind::SiteNotReady => io::ErrorKind::ConnectionRefused,
            LoginErrorKind::Other => io::ErrorKind::Other,
            _ => io::ErrorKind::PermissionDenied,
        };
        io::Error::new(kind, e)
    }
}

/// The login error answer carried by `e`, if any.
pub fn login_error(e: &io::Error) -> Option<&LoginError> {
    e.get_ref()?.downcast_ref::<LoginError>()
}

/// Error for an error answer (`NS_ERROR_RESPONSE`) or a secure-error answer
/// (`NS_SECURE_ERROR`); `fields` are the fields after the message type.
///
/// A secure-error answer can let the reference go on without the
/// encryption; that fallback is not supported here, so it always refuses.
pub fn ns_error(msg_type: u32, fields: &[&str]) -> io::Error {
    let field = |i: usize| fields.get(i).copied().unwrap_or("").to_string();
    if msg_type == NS_SECURE_ERROR {
        let proceed = field(1) == "1";
        let mut text = field(0);
        if proceed {
            text.push_str(" (the server allows an unencrypted login, which is not supported)");
        }
        return LoginError { kind: LoginErrorKind::SecureConnectionRefused, code: None, text }.into();
    }
    let code = fields.first().and_then(|c| c.trim().parse::<i64>().ok());
    let text = field(1);
    LoginError { kind: LoginErrorKind::from_code(code, &text), code, text }.into()
}

/// True for a backup-host notice, which the reference only logs during the
/// login (ibx#423); the caller then reads the next message.
pub fn is_backup_host_notice(text: &str) -> bool {
    let notice = text.split(';').nth(1).and_then(|t| t.parse::<u32>().ok()) == Some(NS_BACKUP_HOST);
    if notice {
        log::info!("Backup host notice received (ignored)");
    }
    notice
}

/// Receive the auth start (`NS_AUTH_START`) of a login, decrypting secure
/// messages.
///
/// As in the reference, a secure message (`NS_SECURE_MESSAGE`) is decrypted and its plain
/// text is handled by its own message type, not trusted as the auth start
/// (ibx#353): an inner auth start is returned, an inner redirect gives the
/// `REDIRECT:` error, an inner error answer gives its login error, a
/// backup-host notice is skipped, and any other type is refused. An auth
/// start sent without encryption is accepted, as the reference does.
///
/// Returns the plain text of the auth start.
pub fn recv_secure<R: Read>(
    stream: &mut R,
    channel: &mut SecureChannel,
) -> io::Result<Vec<u8>> {
    let mut inner: Option<Vec<u8>> = None;
    loop {
        let bytes = match inner.take() {
            Some(plain) => plain,
            None => ns::ns_recv(stream)?.0,
        };
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let parts: Vec<&str> = text.split(';').collect();
        if parts.len() < 2 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "malformed NS response"));
        }
        let msg_type: u32 = parts[1]
            .parse()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid msg type"))?;
        match msg_type {
            NS_AUTH_START => return Ok(bytes),
            NS_SECURE_ERROR | NS_ERROR_RESPONSE => return Err(ns_error(msg_type, &parts[2..])),
            NS_REDIRECT => {
                let target = parts.get(2).unwrap_or(&"");
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionReset,
                    format!("REDIRECT:{}", target),
                ));
            }
            NS_BACKUP_HOST => {
                log::info!("Backup host notice received (ignored)");
            }
            NS_SECURE_MESSAGE => {
                let ct = B64
                    .decode(parts.get(2).copied().unwrap_or(""))
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
                let plain = channel
                    .decrypt(&ct)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                inner = Some(plain);
            }
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Expected the auth start, got message type {}", other),
                ));
            }
        }
    }
}

/// Second factor of the mobile-key kind, the one this crate drives
/// (`XYZ_MSG_SWCR_TOKEN`).
pub const SECOND_FACTOR_MOBILE_KEY: u32 = 5;

/// One entry of the second-factor list of the auth start: a type with an
/// optional sub-type and suffix (ibx#279).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecondFactor {
    pub kind: u32,
    pub subtype: u32,
    pub suffix: String,
}

impl SecondFactor {
    /// Parse one entry the way the reference does. `None` when the type or
    /// the sub-type is not a number.
    pub fn parse(entry: &str) -> Option<Self> {
        let entry = entry.trim();
        let (kind, rest) = match entry.split_once('.') {
            Some((k, r)) => (k, r),
            None => (entry, ""),
        };
        let kind = kind.parse().ok()?;
        let digits = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
        let (subtype, suffix) = rest.split_at(digits);
        let subtype = if subtype.is_empty() { 0 } else { subtype.parse().ok()? };
        Some(Self { kind, subtype, suffix: suffix.to_string() })
    }
}

/// Fields of an auth start that the login uses, read where the reference
/// reads them (ibx#353, ibx#279).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthStart {
    /// Message version.
    pub version: u32,
    /// A password step is required.
    pub password_required: bool,
    /// Second factors of the session, in server order; empty when the
    /// session has none.
    pub second_factors: Vec<SecondFactor>,
    /// Non-zero selects the session-token step.
    pub soft_flag: u32,
}

/// Lowest auth start version whose first mobile-key message carries the
/// token sub-type (reference rule).
const SUB_TYPE_MIN_VERSION: u32 = 15;

impl AuthStart {
    /// Parse the plain text of an auth start. Refuses any other message
    /// type. Empty fields keep their position.
    pub fn parse(plain: &[u8]) -> io::Result<Self> {
        let text = String::from_utf8_lossy(plain);
        let text = text.strip_prefix("MISC").unwrap_or(&text);
        let fields: Vec<&str> = text.split(';').collect();
        let msg_type = fields.get(1).and_then(|t| t.parse::<u32>().ok());
        if msg_type != Some(NS_AUTH_START) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Expected the auth start, got message type {:?}", fields.get(1).copied().unwrap_or("")),
            ));
        }
        let number = |i: usize| fields.get(i).and_then(|f| f.trim().parse::<u32>().ok()).unwrap_or(0);
        let second_factors = fields.get(4).copied().unwrap_or("")
            .split(',')
            .filter(|e| !e.trim().is_empty())
            .filter_map(|e| {
                let factor = SecondFactor::parse(e);
                if factor.is_none() {
                    log::warn!("Auth start: second-factor entry {:?} not understood", e);
                }
                factor
            })
            .collect();
        Ok(Self {
            version: number(0),
            password_required: number(3) != 0,
            second_factors,
            soft_flag: number(5),
        })
    }

    /// The first mobile-key entry of the list, if any.
    pub fn mobile_key(&self) -> Option<&SecondFactor> {
        self.second_factors.iter().find(|f| f.kind == SECOND_FACTOR_MOBILE_KEY)
    }

    /// Second-factor step after the password step of a live login:
    /// `Ok(None)` when the list is empty (no second factor, as in the
    /// reference), `Ok(Some(sub_type))` for the first mobile-key entry, with
    /// `override_sub_type` used instead when it is not empty, and an
    /// `Unsupported` error when the list only has other kinds of factor.
    ///
    /// With several entries the reference preselects the factor used last
    /// time or asks the user; here the first mobile-key entry is used.
    pub fn mobile_key_token(&self, override_sub_type: &str) -> io::Result<Option<String>> {
        if self.second_factors.is_empty() {
            return Ok(None);
        }
        let factor = self.mobile_key().ok_or_else(|| {
            let kinds: Vec<String> = self.second_factors.iter().map(|f| f.kind.to_string()).collect();
            io::Error::new(
                io::ErrorKind::Unsupported,
                format!("second factor of type {} is not supported (only type {})",
                    kinds.join(", "), SECOND_FACTOR_MOBILE_KEY),
            )
        })?;
        Ok(Some(if override_sub_type.is_empty() {
            self.token_sub_type(factor)
        } else {
            override_sub_type.to_string()
        }))
    }

    /// Token sub-type sent in the first mobile-key message for `factor`:
    /// sub-type then suffix, only from `SUB_TYPE_MIN_VERSION` and with a
    /// non-zero sub-type, else empty (reference rule).
    pub fn token_sub_type(&self, factor: &SecondFactor) -> String {
        if self.version >= SUB_TYPE_MIN_VERSION && factor.subtype > 0 {
            format!("{}{}", factor.subtype, factor.suffix)
        } else {
            String::new()
        }
    }
}

/// [`recv_secure`] then [`AuthStart::parse`].
pub fn recv_auth_start<R: Read>(stream: &mut R, channel: &mut SecureChannel) -> io::Result<AuthStart> {
    AuthStart::parse(&recv_secure(stream, channel)?)
}

/// Receive a framed message and classify as text or binary.
pub fn recv_msg<R: Read>(stream: &mut R) -> io::Result<RecvMsg> {
    loop {
        let (payload, _) = ns::ns_recv(stream)?;
        if ns::is_ns_text(&payload) && is_backup_host_notice(&String::from_utf8_lossy(&payload)) {
            continue;
        }
        return classify_payload(&payload);
    }
}

/// Classify one framed payload as NS text or XYZ binary.
fn classify_payload(payload: &[u8]) -> io::Result<RecvMsg> {
    // Try NS text first
    if ns::is_ns_text(payload) {
        if let Some((version, msg_type, fields)) = ns::ns_parse(payload) {
            return Ok(RecvMsg::Ns {
                version,
                msg_type,
                fields,
            });
        }
    }

    // Try XYZ binary
    if payload.len() >= 16 {
        if let Some((msg_id, sub_id, state, fields)) = xyz::xyz_parse_response(payload) {
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
pub type CodeProvider = std::sync::Arc<
    dyn Fn(IbKeyChallenge) -> io::Result<String> + Send + Sync,
>;

/// Compact hex dump for diagnostic logging.
fn hex_dump(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect::<Vec<_>>().join(" ")
}

/// Default deadline for the second-factor gate, matching the server-side
/// timeout measured in capture run B (~18 min).
pub const IB_KEY_DEFAULT_TIMEOUT_SECS: u64 = 1080;

/// Default of [`crate::gateway::GatewayConfig::ib_key_token_sub_type`]:
/// empty, so the token sub-type sent in the SWCR_TOKEN state=1 body comes
/// from the second-factor list of the session's auth start, as the
/// reference does (ibx#279). A non-empty config value overrides it.
pub const IB_KEY_DEFAULT_TOKEN_SUB_TYPE: &str = "";

/// Cadence at which the server probes during the wait window.
const IB_KEY_HEARTBEAT_CADENCE_SECS: u64 = 20;

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
                // instead of waiting for a phone tap. Guarded so a repeated
                // state=2 (server retransmission) doesn't double-submit.
                if !code_submitted {
                    if let Some(provider) = code_provider {
                        let challenge_info = IbKeyChallenge {
                            display_id: session_id.clone(),
                            avth_url: approval_url.clone(),
                        };
                        let code = provider(challenge_info)?;
                        let submission = xyz::xyz_build_swcr_token_code_submission(&code);
                        let framed = xyz::xyz_wrap(&submission);
                        stream.write_all(&framed)?;
                        log::info!(
                            "2FA gate: submitted SWCR_TOKEN state=3 code (len={}, {} bytes framed)",
                            code.len(), framed.len(),
                        );
                        code_submitted = true;
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
            RecvMsg::Ns { msg_type, fields, .. } if msg_type == NS_ERROR_RESPONSE
                || msg_type == NS_SECURE_ERROR =>
            {
                let fields: Vec<&str> = fields.iter().map(String::as_str).collect();
                return Err(ns_error(msg_type, &fields));
            }
            other => {
                // Unknown message during 2FA wait. Log and keep looping —
                // the server may send other informational frames. Only the
                // type and state are logged, at debug level, as in the
                // reference: the fields can carry session material (ibx#283).
                match other {
                    RecvMsg::Xyz { msg_id, state, .. } =>
                        log::debug!("2FA gate: unexpected message id={} state={} (ignored)", msg_id, state),
                    RecvMsg::Ns { msg_type, .. } =>
                        log::debug!("2FA gate: unexpected message type={} (ignored)", msg_type),
                }
                let _ = IB_KEY_HEARTBEAT_CADENCE_SECS;  // referenced for docs
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

    fn recv_secure_login_error(frames: &[&str]) -> (io::Error, Option<LoginError>) {
        let mut wire = Vec::new();
        for f in frames {
            wire.extend_from_slice(&build_ns_frame(f));
        }
        let mut cursor = io::Cursor::new(wire);
        let mut channel = SecureChannel::new();
        let err = recv_secure(&mut cursor, &mut channel).unwrap_err();
        let login = login_error(&err).cloned();
        (err, login)
    }

    // ibx#423: the error answer code gives the kind; site down / not ready
    // are the only retryable kinds.
    #[test]
    fn recv_secure_error_codes_give_kinds() {
        let cases = [
            ("50;519;4;site down;", LoginErrorKind::SiteDown, true, io::ErrorKind::ConnectionRefused),
            ("50;519;5;site not ready;", LoginErrorKind::SiteNotReady, true, io::ErrorKind::ConnectionRefused),
            ("50;519;1;lockedout;", LoginErrorKind::LockedOut, false, io::ErrorKind::PermissionDenied),
            ("50;519;1;IP address 1.2.3.4;", LoginErrorKind::IpNotAuthorized, false, io::ErrorKind::PermissionDenied),
            ("50;519;1;bad;", LoginErrorKind::BadCredentials, false, io::ErrorKind::PermissionDenied),
            ("50;519;11;proxy;", LoginErrorKind::Restricted, false, io::ErrorKind::PermissionDenied),
            ("50;519;14;mode;", LoginErrorKind::PaperUserInLiveMode, false, io::ErrorKind::PermissionDenied),
            ("50;519;18;user;", LoginErrorKind::PaperLogonNotAllowed, false, io::ErrorKind::PermissionDenied),
            ("50;519;23;pwd;", LoginErrorKind::PasswordRejected, false, io::ErrorKind::PermissionDenied),
            ("50;519;99;other;", LoginErrorKind::Other, false, io::ErrorKind::Other),
            ("50;519;x;not a number;", LoginErrorKind::Other, false, io::ErrorKind::Other),
        ];
        for (frame, kind, retryable, io_kind) in cases {
            let (err, login) = recv_secure_login_error(&[frame]);
            let login = login.unwrap_or_else(|| panic!("{frame}: no login error in {err}"));
            assert_eq!(login.kind, kind, "{frame}");
            assert_eq!(login.kind.is_retryable(), retryable, "{frame}");
            assert_eq!(err.kind(), io_kind, "{frame}");
        }
        let (_, login) = recv_secure_login_error(&["50;519;4;site down;"]);
        assert_eq!(login.unwrap(), LoginError { kind: LoginErrorKind::SiteDown, code: Some(4), text: "site down".into() });
    }

    // ibx#423: a secure-error answer refuses with either proceed flag; the
    // unencrypted fallback is not supported.
    #[test]
    fn recv_secure_secure_error_refuses_with_either_flag() {
        for (frame, says_fallback) in [("50;535;text;1;", true), ("50;535;text;0;", false)] {
            let (err, login) = recv_secure_login_error(&[frame]);
            let login = login.unwrap();
            assert_eq!(login.kind, LoginErrorKind::SecureConnectionRefused);
            assert!(!login.kind.is_retryable());
            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
            assert_eq!(err.to_string().contains("not supported"), says_fallback, "{err}");
        }
    }

    // ibx#423: a backup-host notice is skipped; the next message is read.
    #[test]
    fn recv_secure_skips_backup_host_notice() {
        let (err, _) = recv_secure_login_error(&["50;527;x;", "50;524;ndc1.example:4000;"]);
        assert_eq!(err.kind(), io::ErrorKind::ConnectionReset);
        assert!(err.to_string().starts_with("REDIRECT:"), "{err}");
    }

    #[test]
    fn recv_msg_skips_backup_host_notice() {
        let mut wire = build_ns_frame("50;527;x;");
        wire.extend_from_slice(&frame_xyz(&xyz::xyz_build(xyz::XYZ_MSG_TOKEN_AUTH, 5, "user", &["PASSED"])));
        let msg = recv_msg(&mut io::Cursor::new(wire)).unwrap();
        assert!(matches!(msg, RecvMsg::Xyz { state: 5, .. }), "{msg:?}");
    }

    #[test]
    fn recv_secure_unknown_type_returns_error() {
        let frame = build_ns_frame("50;999;payload;");
        let mut cursor = io::Cursor::new(frame);
        let mut channel = SecureChannel::new();
        let err = recv_secure(&mut cursor, &mut channel).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("got message type 999"), "{err}");
    }

    /// A secure message (`NS_SECURE_MESSAGE`) carrying `inner`, encrypted for a zero-key
    /// channel.
    fn secure_frame(inner: &str) -> Vec<u8> {
        secure_frames(&[inner]).remove(0)
    }

    /// Secure messages in order, one sender channel (chained IVs).
    fn secure_frames(inners: &[&str]) -> Vec<Vec<u8>> {
        let mut server = SecureChannel::zero_keys_for_test();
        inners.iter().map(|inner| {
            let ct = B64.encode(server.encrypt(inner.as_bytes()));
            build_ns_frame(&format!("50;534;{};", ct))
        }).collect()
    }

    fn recv_secure_frames(frames: Vec<Vec<u8>>) -> io::Result<Vec<u8>> {
        let mut cursor = io::Cursor::new(frames.concat());
        let mut channel = SecureChannel::zero_keys_for_test();
        recv_secure(&mut cursor, &mut channel)
    }

    const LIVE_AUTH_START: &str = "50;520;1;1;5.2a;0;1234567890123;1;1234567890123456789;OTPWAY;;";

    // ibx#353: the decrypted text is handled by its own type.
    #[test]
    fn recv_secure_returns_only_a_real_auth_start() {
        let plain = recv_secure_frames(vec![secure_frame(LIVE_AUTH_START)]).unwrap();
        assert_eq!(plain, LIVE_AUTH_START.as_bytes());

        // Inner redirect: followed as a redirect.
        let err = recv_secure_frames(vec![secure_frame("50;524;ndc1.example:4001;")]).unwrap_err();
        assert!(err.to_string().starts_with("REDIRECT:ndc1.example:4001"), "{err}");

        // Inner error answer: its login error.
        let err = recv_secure_frames(vec![secure_frame("50;519;5;not ready;")]).unwrap_err();
        assert_eq!(login_error(&err).unwrap().kind, LoginErrorKind::SiteNotReady);

        // Inner connect response: not an auth start, refused.
        let err = recv_secure_frames(vec![secure_frame("50;523;host:4000;0;TST;")]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("got message type 523"), "{err}");

        // Inner backup-host notice: skipped, the next frame is read.
        let plain = recv_secure_frames(secure_frames(&["50;527;x;", LIVE_AUTH_START])).unwrap();
        assert_eq!(plain, LIVE_AUTH_START.as_bytes());

        // An auth start sent in clear is accepted, as the reference does.
        let plain = recv_secure_frames(vec![build_ns_frame(LIVE_AUTH_START)]).unwrap();
        assert_eq!(plain, LIVE_AUTH_START.as_bytes());

        // A secure message with no ciphertext field is an error, not a panic.
        assert!(recv_secure_frames(vec![build_ns_frame("50;534")]).is_err());
    }

    // ibx#279: the second-factor list and the soft flag at their fixed
    // positions, empty fields kept.
    #[test]
    fn auth_start_fields_at_fixed_positions() {
        let live = AuthStart::parse(LIVE_AUTH_START.as_bytes()).unwrap();
        assert_eq!(live.version, 50);
        assert!(live.password_required);
        assert_eq!(live.second_factors, vec![SecondFactor { kind: 5, subtype: 2, suffix: "a".into() }]);
        assert_eq!(live.soft_flag, 0);

        let paper = AuthStart::parse(b"50;520;1;1;;0;1234567890123;1;1234567890123456789;OTPWAY;;").unwrap();
        assert!(paper.second_factors.is_empty());
        assert_eq!(paper.soft_flag, 0);

        let reconnect = AuthStart::parse(b"50;520;1;0;;2;-1;0;0;;;").unwrap();
        assert!(!reconnect.password_required);
        assert_eq!(reconnect.soft_flag, 2);

        // Not an auth start: refused, its fields are not read.
        let err = AuthStart::parse(b"50;523;a;b;c;2;").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn second_factor_entries() {
        let p = |s: &str| SecondFactor::parse(s);
        assert_eq!(p("5.2a"), Some(SecondFactor { kind: 5, subtype: 2, suffix: "a".into() }));
        assert_eq!(p("5.2i"), Some(SecondFactor { kind: 5, subtype: 2, suffix: "i".into() }));
        assert_eq!(p("4.1"), Some(SecondFactor { kind: 4, subtype: 1, suffix: String::new() }));
        assert_eq!(p("3"), Some(SecondFactor { kind: 3, subtype: 0, suffix: String::new() }));
        assert_eq!(p("x.2a"), None);
        let list = AuthStart::parse(b"50;520;1;1;4.2,5.2i;0;").unwrap();
        assert_eq!(list.second_factors.len(), 2);
        assert_eq!(list.mobile_key(), Some(&SecondFactor { kind: 5, subtype: 2, suffix: "i".into() }));
    }

    // ibx#279: the sub-type sent comes from the session's list; the config
    // value only overrides it.
    #[test]
    fn mobile_key_token_from_the_auth_start() {
        let start = |s: &str| AuthStart::parse(s.as_bytes()).unwrap();
        assert_eq!(start(LIVE_AUTH_START).mobile_key_token("").unwrap(), Some("2a".into()));
        assert_eq!(start("50;520;1;1;5.2i;0;").mobile_key_token("").unwrap(), Some("2i".into()));
        assert_eq!(start("50;520;1;1;5.2i;0;").mobile_key_token("7z").unwrap(), Some("7z".into()));
        // No sub-type, or a version below 15: empty sub-type.
        assert_eq!(start("50;520;1;1;5;0;").mobile_key_token("").unwrap(), Some(String::new()));
        assert_eq!(start("14;520;1;1;5.2a;0;").mobile_key_token("").unwrap(), Some(String::new()));
        // Empty list: no second factor.
        assert_eq!(start("50;520;1;1;;0;").mobile_key_token("").unwrap(), None);
        // Only other kinds of factor: not supported.
        let err = start("50;520;1;1;4.1;0;").mobile_key_token("").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
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

    // ibx#283: an unknown state is ignored (logged by type and state only)
    // and the wait goes on, as in the reference.
    #[test]
    fn ib_key_2fa_ignores_an_unknown_state() {
        let unknown = xyz::xyz_build(xyz::XYZ_MSG_SWCR_TOKEN, 9, "user", &["secret-material"]);
        let auth_finish = xyz::xyz_build(xyz::XYZ_MSG_TOKEN_AUTH, 5, "user", &["PASSED"]);
        let mut incoming = frame_xyz(&unknown);
        incoming.extend_from_slice(&frame_xyz(&auth_finish));
        let mut stream = ScriptedStream::new(incoming);
        let outcome = do_ib_key_2fa(&mut stream, "2a", far_future_deadline(), None).unwrap();
        assert_eq!(outcome, IbKeyOutcome::Skipped);
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
        let mut incoming = frame_xyz(&challenge);
        incoming.extend_from_slice(&frame_xyz(&state4_passed));
        incoming.extend_from_slice(&frame_xyz(&auth_finish));
        let mut stream = ScriptedStream::new(incoming);

        let seen_challenge = std::sync::Arc::new(std::sync::Mutex::new(IbKeyChallenge::default()));
        let seen_clone = seen_challenge.clone();
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

        // Walk written frames: 1st = SWCR_TOKEN state=1 init, 2nd = state=3 submission.
        // The 2nd frame must be byte-for-byte the 40-byte capture from run A.
        let mut frames: Vec<Vec<u8>> = Vec::new();
        let mut offset = 0;
        while offset + 8 <= stream.written.len() {
            let len = u32::from_be_bytes(
                stream.written[offset + 4..offset + 8].try_into().unwrap(),
            ) as usize;
            frames.push(stream.written[offset + 8..offset + 8 + len].to_vec());
            offset += 8 + len;
        }
        assert!(frames.len() >= 2, "expected at least 2 frames (init + submission); got {}", frames.len());
        let expected_state3 = xyz::xyz_build_swcr_token_code_submission(RUN_A_CODE);
        assert_eq!(frames[1], expected_state3,
            "state=3 submission must byte-match the ib-agent#149 run-A capture");
    }

    #[test]
    fn ib_key_2fa_cr_code_rejected_on_state_4_failed() {
        // Server: state=2 → state=4 FAILED. Server tears the socket down after
        // (no AUTH_FINISH on the wire). Client must surface PermissionDenied
        // immediately on FAILED, without waiting for further frames.
        let challenge = xyz::xyz_build(xyz::XYZ_MSG_SWCR_TOKEN, 2, "user", &[
            "e7429fde5b4c26f81fff956be6749908a8653558e7429fde5b4c26f81fff956b",
            "399 830",
            "https://x.example/u",
        ]);
        let state4_failed = xyz::xyz_build(xyz::XYZ_MSG_SWCR_TOKEN, 4, "user", &["FAILED"]);
        let mut incoming = frame_xyz(&challenge);
        incoming.extend_from_slice(&frame_xyz(&state4_failed));
        let mut stream = ScriptedStream::new(incoming);

        let provider: CodeProvider = std::sync::Arc::new(|_| Ok("99999999".to_string()));
        let err = do_ib_key_2fa(&mut stream, "2a", far_future_deadline(), Some(&provider)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(err.to_string().contains("C/R code rejected"),
            "expected C/R rejection message; got {}", err);
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
        let mut stream = ScriptedStream::new(frame_xyz(&challenge));
        let provider: CodeProvider = std::sync::Arc::new(|_| {
            Err(io::Error::new(io::ErrorKind::Interrupted, "user cancelled"))
        });
        let err = do_ib_key_2fa(&mut stream, "2a", far_future_deadline(), Some(&provider)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
    }
}
