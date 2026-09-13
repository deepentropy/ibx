//! Connection lifecycle: authentication and data connection management.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};

use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use num_bigint::BigUint;
use rand::RngCore;

use crate::auth::crypto::strip_leading_zeros;
use crate::auth::dh::SecureChannel;
use crate::auth::srp;
use crate::config::*;
use crate::protocol::ns::{self, *};
use crate::protocol::xyz;

/// The message id the venue states an SRP verdict under.
///
/// The exchange runs on odd ids out and even ids back: 1 asks for the
/// parameters and 2 answers, 3 offers the public value and 4 answers, 5 sends
/// the client's proof and 6 carries the verdict. An id outside that set
/// belongs to something else the venue is saying.
const SRP_AUTH_RESULT: u32 = 6;

/// Result of authentication.
///
/// `session_token` is the SRP-derived shared secret K as a `BigUint`. For wire-byte
/// uses
/// (e.g. SHA-1 challenge/response, token short hashes), prefer
/// [`AuthResult::session_token_bytes`],
/// which returns the canonical big-endian form with leading zeros stripped — matching
/// the
/// representation the server expects.
///
/// `token_type` is one of `"st"`, `"tst"`, or `"zenith"` and corresponds verbatim to
/// the
/// `stoken_type` value used by SSO authenticators in the upstream Java auth flow.
pub struct AuthResult {
    /// SRP shared secret K. Use [`session_token_bytes`](Self::session_token_bytes) for
    /// the
    /// canonical big-endian wire form.
    pub session_token: BigUint,
    /// Token type discriminator: `"st"`, `"tst"`, or `"zenith"`. Matches the
    /// `stoken_type`
    /// field expected by the SSO `Authenticate-TWS` body.
    pub token_type: String,
    /// Session id assigned by the venue.
    pub session_id: String,
    /// Every capability it granted.
    pub features: Vec<String>,
    /// Whether the login succeeded.
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

// CONNECT_REQUEST flags
/// Farm flag 1: ok to redirect.
pub const FLAG_OK_TO_REDIRECT: u32 = 1;
/// Farm flag 2: is farm.
pub const FLAG_IS_FARM: u32 = 2;
/// Farm flag 4: version.
pub const FLAG_VERSION: u32 = 4;
/// Farm flag 8: version present.
pub const FLAG_VERSION_PRESENT: u32 = 8;
/// Farm flag 16: soft token.
pub const FLAG_SOFT_TOKEN: u32 = 16;
/// Farm flag 32: device info.
pub const FLAG_DEVICE_INFO: u32 = 32;
/// Farm flag 64: permanent token.
pub const FLAG_PERMANENT_TOKEN: u32 = 64;
/// Farm flag 4096: not established.
pub const FLAG_UNKNOWN_U: u32 = 4096;
/// Farm flag 8192: paper connect.
pub const FLAG_PAPER_CONNECT: u32 = 8192;
/// Farm flag 131072: farm name.
pub const FLAG_FARM_NAME: u32 = 131072;
/// Farm flag 524288: not established.
pub const FLAG_UNKNOWN_19: u32 = 524288;
/// Farm flag 1048576: not established.
pub const FLAG_UNKNOWN_20: u32 = 1048576;
/// Farm flag 1024: twsro token.
pub const FLAG_TWSRO_TOKEN: u32 = 1024;

/// Generate a session ID: hex(epoch_secs).hex(millis%1000).
pub fn get_session_id() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let millis = now.as_millis() as u64;
    let secs = millis / 1000;
    let ms = millis % 1000;
    format!("{secs:x}.{ms:04x}")
}

/// Where the machine id sent in tag 6351 is kept.
///
/// Eight hex characters, written on first use and reused by every logon
/// after it: `%USERPROFILE%\hwid` on Windows, `$HOME/.hwid` elsewhere. The
/// value has to stay the same between runs — a logon presenting one the
/// account has not used before is not answered, and nothing says why, so a
/// path that does not persist reads as a login that has stopped working.
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
            return format!("{v:0>8}");
        }
    }
    let path = hwid_path();
    if let Ok(s) = std::fs::read_to_string(&path) {
        let s = s.trim();
        if !s.is_empty() && s.chars().all(|c| c.is_ascii_hexdigit()) {
            return format!("{s:0>8}");
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
/// 35=A logon to be silently rejected (paper farms don't validate). So a host
/// that answers with no card at all is told so here rather than handed six
/// bytes of nothing: this client refuses that value when a caller states it,
/// and composing it here left the farm behind the logon refusing in silence
/// with nothing to say why. A caller on such a host states a card of its own.
/// Where the session actually opened from, once it has.
///
/// The venue's own client takes the local address off the socket it connected
/// with and builds the identity from that, once, for the life of the process.
/// Probed independently instead, this client could answer with the address of
/// a route to somewhere else — or with nothing, where the route it probes is
/// blocked — and could answer differently for the order connection and for
/// each farm, which is three identities for one machine in one session.
static FROM_THE_SOCKET: std::sync::OnceLock<std::net::IpAddr> = std::sync::OnceLock::new();

/// Note the local address of the socket this session opened on.
///
/// The first one wins, as it does in the client this replaces: a redirect
/// opens another socket, and the identity the venue already has is the one
/// from the first.
pub fn note_the_socket_we_opened(local: std::net::IpAddr) {
    if is_a_machine(&local) {
        let _ = FROM_THE_SOCKET.set(local);
    }
}

/// `machine_id` is the persistent 8-hex value from `~/hwid`, and
/// `stated_mac` the card to name where the machine's own is not the one to
/// name.
pub fn get_hw_info(stated: Option<&str>, stated_mac: Option<&str>) -> Option<String> {
    let machine_id = match stated.map(str::trim).filter(|v| {
        !v.is_empty() && v.chars().all(|c| c.is_ascii_hexdigit())
    }) {
        Some(stated) => format!("{stated:0>8}"),
        None => read_or_create_hwid(),
    };
    let mac = stated_mac
        .and_then(stated_card)
        .or_else(card_on_the_socket_we_opened)
        .or_else(first_real_mac)?;
    Some(format!("{machine_id}|{mac}"))
}

/// A card a caller named, in the shape this identity is written in.
///
/// Six bytes of hex, however they were separated — colons, dashes, or nothing
/// at all — written back out with colons and in upper case, because that is
/// the shape the machine's own is read in and the two have to be the same
/// string for the same machine. Anything that is not six bytes names no card
/// and is refused, which leaves the probe below to answer.
fn stated_card(stated: &str) -> Option<String> {
    let digits: Vec<char> = stated
        .chars()
        .filter(|c| !matches!(c, ':' | '-' | '.' | ' '))
        .collect();
    if digits.len() != 12 || !digits.iter().all(|c| c.is_ascii_hexdigit()) {
        log::warn!(
            "the network card stated for this machine is not six bytes of hex; \
             reading the machine's own instead",
        );
        return None;
    }
    // Six bytes of nothing is not a card either. The venue takes the identity
    // and refuses the farm logon behind it, silently, which reads from here as
    // a farm that would not talk to this session.
    if digits.iter().all(|c| *c == '0') {
        log::warn!(
            "the network card stated for this machine is all zeroes, which the venue \
             refuses; reading the machine's own instead",
        );
        return None;
    }
    let pairs: Vec<String> = digits
        .chunks(2)
        .map(|pair| pair.iter().collect::<String>().to_ascii_uppercase())
        .collect();
    Some(pairs.join(":"))
}

/// The card of the interface the session's own socket was opened on.
///
/// Which interface that is, is what the address says, and this is the pairing
/// the client this replaces makes: it asks the system for the interface
/// holding the address its socket reported, and reads that interface's card.
/// The first card a machine answers with is a different question and often a
/// different answer — a virtual interface on some systems — and the identity
/// then describes a machine the venue has never seen.
///
/// `None` before a socket has been opened, and on a system this cannot ask.
#[cfg(unix)]
fn card_on_the_socket_we_opened() -> Option<String> {
    let local = *FROM_THE_SOCKET.get()?;
    let interfaces = nix::ifaddrs::getifaddrs().ok()?;
    let mut named = None;
    for interface in interfaces {
        let Some(address) = interface.address else { continue };
        let holds = match local {
            std::net::IpAddr::V4(wanted) => address
                .as_sockaddr_in()
                .is_some_and(|stated| stated.ip() == wanted),
            std::net::IpAddr::V6(wanted) => {
                address.as_sockaddr_in6().is_some_and(|stated| stated.ip() == wanted)
            }
        };
        if holds {
            named = Some(interface.interface_name);
            break;
        }
    }
    let card = mac_address::mac_address_by_name(&named?).ok().flatten()?;
    let bytes = card.bytes();
    if bytes.iter().all(|b| *b == 0) {
        return None;
    }
    Some(format!(
        "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5],
    ))
}

/// Nothing here asks that question on a system with no `getifaddrs`, so the
/// card is the first the machine answers with, as it was.
#[cfg(not(unix))]
fn card_on_the_socket_we_opened() -> Option<String> {
    None
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

/// Whether an address is one a machine is reached at.
///
/// What the identity carries is where this machine is on its own network, and
/// three kinds of address are not that: the unspecified address, which names
/// every interface and none, a multicast group, which names a set of
/// listeners, and the IPv4 broadcast address.
fn is_a_machine(address: &std::net::IpAddr) -> bool {
    if address.is_unspecified() || address.is_multicast() {
        return false;
    }
    match address {
        std::net::IpAddr::V4(v4) => !v4.is_broadcast(),
        std::net::IpAddr::V6(_) => true,
    }
}

/// Discover the local LAN IP that would route to the public internet, or use
/// the one the caller stated. Returns "127.0.0.1" if no external route is
/// configured.
///
/// Uses the standard `UdpSocket::connect` trick: connecting a UDP socket to
/// a public address doesn't send any packets, but lets the OS pick the
/// outbound interface, exposed via `local_addr()`.
///
/// A stated address is what a session inside a container needs: the address
/// the probe finds there belongs to the container's own network and not to the
/// machine the venue holds the session under. It has to be an address — a word
/// that is not one names no machine, and is refused so the probe still answers.
pub fn get_lan_ip(stated: Option<&str>) -> String {
    use std::net::UdpSocket;
    // What the socket said, where a socket has been opened: that is the
    // address this machine is at, as far as the venue is concerned, and the
    // client this replaces uses nothing else.
    if stated.map(str::trim).is_none_or(str::is_empty)
        && let Some(local) = FROM_THE_SOCKET.get()
    {
        return local.to_string();
    }
    if let Some(stated) = stated.map(str::trim).filter(|v| !v.is_empty()) {
        match stated.parse::<std::net::IpAddr>() {
            // An address, and one a machine can be reached at. The unspecified
            // address, a multicast group and the broadcast address all parse
            // and none of them is where a machine is: the venue's own client
            // takes this off the socket it opened, which can only ever answer
            // with one address of one interface.
            Ok(address) if is_a_machine(&address) => return address.to_string(),
            Ok(_) => log::warn!(
                "the address stated for this machine on its local network is not an \
                 address a machine is reached at; reading the machine's own instead",
            ),
            Err(_) => log::warn!(
                "the address stated for this machine on its local network is not an \
                 address; reading the machine's own instead",
            ),
        }
    }
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
    let outer = format!("{NS_VERSION};{NS_SECURE_MESSAGE};{ct_b64};");
    let payload = outer.as_bytes();
    let mut msg = Vec::with_capacity(8 + payload.len());
    msg.extend_from_slice(NS_MAGIC);
    msg.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    msg.extend_from_slice(payload);
    stream.write_all(&msg)?;
    Ok(())
}

/// The number the venue puts first in an error response when the site the
/// login was routed to is down.
const SITE_DOWN: u32 = 4;

/// The same, for a site that is up but not yet serving logins.
const SITE_NOT_READY: u32 = 5;

/// An error frame the venue sent, read for which of them it is.
///
/// An error response numbers itself in its first field, and two of those
/// numbers are not refusals: the site being down and the site not being ready
/// both say come back, not no. Carried as a refusal they classify as the
/// credentials being wrong and the retry ladder stops there, so a maintenance
/// window ends the session for good where the venue asked only for a wait.
/// Stated as nothing having answered, they are waited out and retried, which
/// is what the venue asked for.
///
/// Every other number keeps the refusal it is — a locked-out account, a
/// refused address, a restriction — because the next attempt would carry
/// exactly the same credentials to exactly the same answer.
///
/// A secure error is not numbered. Its first field is the message text, so
/// nothing is read out of it and it is only ever a refusal here.
pub fn error_the_venue_stated(what: &str, msg_type: u32, fields: &[&str]) -> io::Error {
    let stated = fields.join(";");
    let numbered = (msg_type == ns::NS_ERROR_RESPONSE)
        .then(|| fields.first().and_then(|f| f.trim().parse::<u32>().ok()))
        .flatten();
    let waiting = match numbered {
        Some(SITE_DOWN) => "the venue states its site is down",
        Some(SITE_NOT_READY) => "the venue states its site is not ready",
        _ => return ns::refused_by_the_venue(what, stated),
    };
    io::Error::new(io::ErrorKind::TimedOut, format!("{what}: {waiting}: {stated}"))
}

/// Receive an encrypted response and decrypt.
pub fn recv_secure<R: Read>(
    stream: &mut R,
    channel: &mut SecureChannel,
) -> io::Result<Vec<u8>> {
    // A message the venue states while this one is awaited is read past, not
    // taken for a failure. It states a backup host without being asked, and
    // refusing the login over one would end a session the venue had no
    // complaint about.
    let secure_deadline = std::time::Instant::now()
        + std::time::Duration::from_secs(crate::config::TIMEOUT_SSL_AUTH);
    let body = loop {
        if std::time::Instant::now() >= secure_deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "no secure message before the auth timeout, with the venue still speaking",
            ));
        }
        let (payload, _) = ns::ns_recv(stream, secure_deadline)?;
        let text = String::from_utf8_lossy(&payload);
        let parts: Vec<&str> = text.split(';').collect();

        if parts.len() < 2 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "malformed NS response"));
        }

        let msg_type: u32 = parts[1]
            .parse()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid msg type"))?;

        if msg_type == NS_SECURE_ERROR || msg_type == ns::NS_ERROR_RESPONSE {
            return Err(error_the_venue_stated("Auth error", msg_type, &parts[2..]));
        }
        if msg_type == NS_REDIRECT {
            let target = parts.get(2).unwrap_or(&"");
            return Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                format!("REDIRECT:{target}"),
            ));
        }
        if msg_type == NS_SECURE_MESSAGE {
            // Read rather than indexed: the guard above establishes two fields,
            // and a secure message states three. A frame with the type and
            // nothing after it is malformed, which is a thing to report —
            // indexing it takes the login thread down instead.
            break parts
                .get(2)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "secure message carries no body",
                    )
                })?
                .to_string();
        }

        log::info!("received msg id {msg_type} while awaiting a secure message; read past");
    };

    let ct = B64
        .decode(&body)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    channel
        .decrypt(&ct)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// The field the server states its own proof on.
const SRP_SERVER_PROOF_FIELD: usize = 8;

/// The clock one wait of this handshake runs under, matching the one the
/// auth socket is given.
fn srp_wait_deadline() -> std::time::Instant {
    std::time::Instant::now()
        + std::time::Duration::from_secs(crate::config::TIMEOUT_SSL_AUTH)
}

/// Check the server's proof before its verdict is believed.
///
/// The verdict is a word the peer states about itself; the proof is
/// `SHA1(A || M1 || K)`, which only a party holding the verifier can produce.
/// Read without it, a logon succeeds against whoever answered the connect —
/// which on a farm connection is the whole of the peer authentication, there
/// being no transport underneath that has already done it.
fn check_server_proof(
    fields: &[String],
    a_pub: &BigUint,
    m1: &BigUint,
    k: &BigUint,
    what: &str,
) -> io::Result<()> {
    let stated = fields
        .get(SRP_SERVER_PROOF_FIELD)
        .map(String::as_str)
        .filter(|proof| !proof.is_empty())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{what}: the venue stated no proof of its own, so its verdict is unchecked"),
            )
        })?;
    // Compared as numbers rather than as text. This client writes its own
    // proof as unpadded hex and the venue writes back the same way, so one in
    // every two hundred and fifty-six proofs is a digit shorter than the rest
    // — and a comparison of the strings would refuse those logons.
    //
    // An ordinary comparison: the proof is public, the venue sends it in the
    // clear, and it is derived from a key that is new every handshake, so
    // there is no repeated secret for a timing difference to leak.
    let stated_int = BigUint::parse_bytes(stated.as_bytes(), 16).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{what}: the venue's proof is not a number"),
        )
    })?;
    if srp::srp_compute_m2(a_pub, m1, k) != stated_int {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{what}: the venue's proof does not match the session key, so the peer \
                 does not hold this account's verifier",
            ),
        ));
    }
    Ok(())
}

/// Read on until the venue states an SRP verdict, and answer with its fields.
///
/// A message carrying an id this exchange does not use is passed over rather
/// than read as the verdict. The venue puts its own between the client's proof
/// and the result, and taking the first thing to arrive for the answer refused
/// a logon over a message that states no answer at all.
fn srp_result_fields<R: Read>(stream: &mut R) -> io::Result<Vec<String>> {
    // Bounded by the same clock the auth socket is given, not by a count of
    // messages: the socket's own timeout ends a quiet venue, and this ends one
    // that keeps talking without ever stating a verdict.
    let deadline = std::time::Instant::now()
        + std::time::Duration::from_secs(crate::config::TIMEOUT_SSL_AUTH);
    loop {
        if std::time::Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "no SRP result before the auth timeout, with the venue still speaking",
            ));
        }
        match recv_msg(stream, deadline)? {
            RecvMsg::Xyz { state, fields, .. } if state == SRP_AUTH_RESULT => {
                return Ok(fields);
            }
            RecvMsg::Xyz { state, .. } => {
                log::info!("received unknown msg id {state} while awaiting the SRP result");
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Expected XYZ response for SRP state 6",
                ));
            }
        }
    }
}

/// Receive a framed message and classify as text or binary.
///
/// Bounded by the caller's deadline: the socket's own timeout ends a quiet
/// peer one read at a time, but a peer that keeps trickling bytes holds a
/// frame open past every deadline checked between frames.
pub fn recv_msg<R: Read>(stream: &mut R, deadline: std::time::Instant) -> io::Result<RecvMsg> {
    let (payload, _) = ns::ns_recv(stream, deadline)?;

    // Try NS text first
    if ns::is_ns_text(&payload)
        && let Some((version, msg_type, fields)) = ns::ns_parse(&payload) {
            return Ok(RecvMsg::Ns {
                version,
                msg_type,
                fields,
                raw: payload,
            });
        }

    // Try XYZ binary
    if payload.len() >= 16
        && let Some((msg_id, sub_id, state, fields)) = xyz::xyz_parse_response(&payload) {
            return Ok(RecvMsg::Xyz {
                msg_id,
                sub_id,
                state,
                fields,
            });
        }

    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        format!("Cannot parse message: {:?}", &payload[..payload.len().min(40)]),
    ))
}

/// Classified received message.
#[derive(Debug)]
pub enum RecvMsg {
    /// A name-service (NS) message.
    Ns {
        /// Which version of that message this is.
        version: u32,
        /// Which message it is.
        msg_type: u32,
        /// Its fields, in the order they arrived.
        fields: Vec<String>,
        /// The payload it was parsed from, kept so a reader that consumed a
        /// message meant for somebody else can hand it on rather than drop it.
        raw: Vec<u8>,
    },
    /// One of its binary session messages.
    Xyz {
        /// Which message this is.
        msg_id: u32,
        /// Which part of it.
        sub_id: u32,
        /// What state the venue reports.
        state: u32,
        /// Its fields, in the order they arrived.
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

/// The group a peer states, where it states the one this venue uses.
///
/// A peer names the modulus and generator before it has proved anything, and
/// every value after that is arithmetic in the group it named. A small modulus
/// collapses the shared secret to something anybody can compute, and the proof
/// that would catch a substituted peer is then one it can produce itself — so
/// the group is checked before it is used, and a logon on any other stops
/// here.
fn stated_group(stated: &[String]) -> io::Result<(BigUint, BigUint)> {
    let parsed = stated.first().zip(stated.get(1)).and_then(|(n, g)| {
        Some((
            BigUint::parse_bytes(n.as_bytes(), 16)?,
            BigUint::parse_bytes(g.as_bytes(), 16)?,
        ))
    });
    match parsed {
        Some((n, g)) if n == srp::srp_venue_n() && g == BigUint::from(srp::SRP_VENUE_G) => {
            Ok((n, g))
        }
        Some((n, _)) => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "the peer stated an SRP group this venue does not use ({} bits), so the \
                 logon would be checked on ground the peer chose",
                n.bits(),
            ),
        )),
        // The venue states its group on every logon. A peer that states none
        // leaves the client to pick one, which is the same choice by omission:
        // whatever it fell back to would not be the group that was checked.
        None => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the peer stated no SRP group, so there is none to check it on",
        )),
    }
}

/// Execute authentication protocol.
///
/// Returns the session key K as BigUint.
pub fn do_srp<S: Read + Write>(stream: &mut S, username: &str, password: &str) -> io::Result<BigUint> {

    // State 1: Send AUTH_QUERY
    let msg1 = xyz::xyz_build_srp_v20(1, &[]);
    stream.write_all(&xyz::xyz_wrap(&msg1))?;

    // State 2: Receive AUTH_PARAMS
    let recv2 = recv_msg(stream, srp_wait_deadline())?;
    let fields2 = match recv2 {
        RecvMsg::Xyz { state, fields, .. } => {
            if state != 2 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Expected SRP state 2, got {state}"),
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
    // The venue may state its own group; otherwise this client's applies.
    let (n, g) = stated_group(&data_fields)?;

    // Generate client keys: a (private), A = g^a mod N
    let mut a_bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut a_bytes);
    let a_priv = BigUint::from_bytes_be(&a_bytes);
    let a_pub = g.modpow(&a_priv, &n);

    // State 3: Send client public key A
    let a_hex = format!("{a_pub:x}");
    let msg3 = xyz::xyz_build_srp_v20(3, &[("L", &a_hex)]);
    stream.write_all(&xyz::xyz_wrap(&msg3))?;

    // State 4: Receive SERVER_PARAMS (salt, B)
    let recv4 = recv_msg(stream, srp_wait_deadline())?;
    let (state4, fields4) = match recv4 {
        RecvMsg::Xyz { state, fields, .. } => (state, fields),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Expected XYZ response for SRP state 4",
            ));
        }
    };

    // The venue's verdict, stated instead of the salt and B a logon it means
    // to check would get. It is the field the AUTH_RESULT carries, so it is
    // raised the way that one is: a refusal, not a door that did not open.
    if state4 == 7 {
        let result = fields4.get(9).map(|s| s.as_str()).unwrap_or("FAILED");
        return Err(ns::refused_by_the_venue("SRP early error (state 7)", result.to_string()));
    }
    if state4 != 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Expected SRP state 4, got {state4}"),
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
    let m1_hex = format!("{m1:x}");
    let msg5 = xyz::xyz_build_srp_v20(5, &[("N", &m1_hex)]);
    stream.write_all(&xyz::xyz_wrap(&msg5))?;

    // State 6: Receive AUTH_RESULT.
    //
    // A message carrying an id this exchange does not use is skipped, not read
    // as the verdict. The venue puts its own between the proof and the result,
    // and taking the first thing to arrive for the answer refused a logon over
    // a message that states no answer at all — the reading was `UNKNOWN`,
    // which was a field of something else entirely.
    //
    // Bounded so a venue that never states a result ends the attempt rather
    // than reading forever.
    let fields6 = srp_result_fields(stream)?;
    check_server_proof(&fields6, &a_pub, &m1, &k, "SRP")?;

    let result = fields6
        .get(9)
        .filter(|s| !s.is_empty())
        .or_else(|| fields6.iter().rev().find(|s| !s.is_empty()))
        .map(|s| s.as_str())
        .unwrap_or("");

    if result == "PASSED" {
        Ok(k)
    } else if result == "NEEDSSL" {
        Err(io::Error::other(
            "Server requires SSL upgrade (NEEDSSL)",
        ))
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("SRP Authentication FAILED: {result}"),
        ))
    }
}

/// Execute token authentication for farm connections.
fn wrap_xyz_fix(xyz_payload: &[u8]) -> Vec<u8> {
    let tag35 = b"35=X\x01";
    let body_len = tag35.len() + xyz_payload.len();
    let header = format!("8=1\x019={body_len:04}\x01");
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
/// dropped the logon ACK and stalled the exchange.
fn recv_8eq1(stream: &mut TcpStream, carry: &mut Vec<u8>) -> io::Result<Vec<u8>> {
    let mut tmp = [0u8; 4096];
    // Tolerate transient WouldBlock/TimedOut (os error 35 on macOS) from the
    // short poll timeout until an overall deadline; a slow segment from a
    // high-latency regional gateway must not fail the auth exchange.
    let deadline = std::time::Instant::now()
        + std::time::Duration::from_secs_f64(TIMEOUT_FARM_LOGON);
    loop {
        // Read here as well as on the quiet socket below: a peer sending bytes
        // that never complete a message keeps every read successful, and the
        // deadline down there is never reached.
        if std::time::Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "farm auth timed out waiting for server response",
            ));
        }
        // A prior call may already have buffered a full message.
        if let Some(total) = try_frame_8eq1(carry)? {
            let msg = carry[..total].to_vec();
            carry.drain(..total);
            return Ok(msg);
        }
        let n = match stream.read(&mut tmp) {
            Ok(n) => n,
            Err(e) if crate::protocol::connection::read_found_nothing(&e) => {
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
        crate::protocol::connection::hold_what_was_read(carry, &tmp[..n])?;
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
            format!("farm message body too large: {body_len} bytes"),
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
    Skipped {
        /// A message the gate read that belongs to what follows it, or `None`
        /// when the gate ended on its own answer.
        ///
        /// The server can move straight on to the connect response instead of
        /// finishing the gate. Read here and dropped, that message never
        /// reached the loop waiting for it, which then waited out its deadline
        /// for something already delivered.
        unread: Option<Vec<u8>>,
    },
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

/// Which second factor the callback is being asked to answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SecondFactor {
    /// IBKey Challenge/Response — answer with the code the app
    /// shows for `display_id`.
    #[default]
    IbKeyChallengeResponse,
    /// Authenticator code — answer with the account's current code, typically
    /// 6 digits. `display_id` and `avth_url` are empty; there is nothing to show.
    AuthenticatorCode,
}

/// Challenge details surfaced to a [`CodeProvider`] callback.
///
/// Populated from the server's `XYZ 775` state=2 reply: the per-session
/// display id the user sees next to the code in the IBKey app, and
/// the `clientam.com/ibkr/ibkey/seamless?S=…` URL also used by the web
/// fallback. Either may be empty if the server omitted it in this run.
#[derive(Debug, Clone, Default)]
pub struct IbKeyChallenge {
    /// Which factor is being asked for. The two want different codes, so
    /// branch on this before validating what the operator typed.
    pub factor: SecondFactor,
    /// The code shown on the phone, which the person confirms.
    pub display_id: String,
    /// Where the approval is confirmed.
    pub avth_url: String,
}

/// Second-factor code provider callback.
///
/// Invoked once per login, with [`IbKeyChallenge::factor`] naming what to
/// return:
///
/// - [`SecondFactor::IbKeyChallengeResponse`] — the code shown for
///   `display_id`, submitted as `XYZ 775` state=3. Supplying a provider selects
///   this over waiting for a mobile push.
/// - [`SecondFactor::AuthenticatorCode`] — the account's current authenticator
///   code, submitted as `XYZ 774` code=1. Not optional: these accounts have no
///   push to fall back to, so a missing provider fails the login.
///
/// Neither server retries, and what a wrong code costs has not been watched
/// from here, so pull the code from a deterministic source (stdin, a secrets
/// vault) or return an `io::Error` to abort.
///
/// Called on its own thread so the gate can keep answering the server's
/// keepalives while it waits. It is not cancelled if the gate gives up first,
/// so bound anything that can block indefinitely — a callback still parked on
/// stdin outlives the login and competes with the next one for the terminal.
pub type CodeProvider = std::sync::Arc<
    dyn Fn(IbKeyChallenge) -> io::Result<String> + Send + Sync,
>;

/// How long the gate waits inline for a freshly started `code_provider` before
/// falling back to polling it between inbound messages. Sized to cover a
/// provider that reads from a vault, an env var or a file, and to stay far
/// below the server's probe cadence so nothing is starved by the wait.
const IB_KEY_PROVIDER_FAST_PATH_GRACE: std::time::Duration =
    std::time::Duration::from_secs(1);

/// Put the Challenge/Response code on the wire as `XYZ 775` state=3.
///
/// Trimmed, and an empty one refused, because the code is single-use and the
/// server has no retry loop: whitespace a provider left around it — a file, a
/// command's output, a vault answer that ends in a newline — reaches the server
/// as part of the code, and spends the one attempt the operator has. The other
/// gate on the same callback already refuses an empty code and trims for this
/// reason; one provider serves both.
fn submit_swcr_code<S: Write>(stream: &mut S, code: &str) -> io::Result<()> {
    let code = code.trim();
    if code.is_empty() {
        return Err(ib_key_err(
            io::ErrorKind::InvalidInput,
            "security-code gate: the code provider answered with nothing, and an empty code \
             would spend the one attempt",
        ));
    }
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

/// Put a code the provider has finished on the wire, where one is waiting.
///
/// A code is single use, and one spent on a login the deadline is about to
/// abandon is gone for nothing, so a code that outlives the deadline is
/// dropped rather than sent.
fn send_a_waiting_code<S: Write>(
    stream: &mut S,
    pending_code: &mut Option<std::sync::mpsc::Receiver<io::Result<String>>>,
    code_submitted: &mut bool,
    deadline: std::time::Instant,
) -> io::Result<()> {
    let Some(rx) = pending_code.as_ref() else {
        return Ok(());
    };
    match rx.try_recv() {
        Ok(result) => {
            *pending_code = None;
            if std::time::Instant::now() < deadline {
                submit_swcr_code(stream, &result?)?;
                *code_submitted = true;
            }
        }
        Err(std::sync::mpsc::TryRecvError::Empty) => {}
        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
            return Err(provider_panicked());
        }
    }
    Ok(())
}

/// Default deadline for the second-factor gate, matching the server-side
/// timeout measured on a recorded session (~18 min).
pub const IB_KEY_DEFAULT_TIMEOUT_SECS: u64 = 1080;

/// Default IBKey token sub-type used in the second-factor state=1 body, and
/// what the account this was written against is answered with. Another
/// account may be answered with something else — override via
/// [`crate::gateway::GatewayConfig::ib_key_token_sub_type`].
pub const IB_KEY_DEFAULT_TOKEN_SUB_TYPE: &str = "2a";

/// Ceiling on a frame in the auth gate, where the traffic is 774/771 replies
/// and keepalives — none of which reach a fraction of this.
const MAX_GATE_FRAME: usize = 64 * 1024;

/// Frame reader that survives a read timeout mid-message.
///
/// The gate polls the socket so a time-limited code is not left unsent while
/// the loop blocks. `ns_recv` cannot be polled: it is two `read_exact` calls,
/// and a timeout between them discards the bytes already consumed, leaving the
/// next read starting mid-frame. This buffers instead, so a timeout is simply
/// "no complete frame yet".
#[derive(Default)]
struct GateReader {
    buf: Vec<u8>,
}

impl GateReader {
    /// One non-blocking-ish step: drain what is available, return a message
    /// once a whole frame is buffered. `Ok(None)` means "nothing complete yet".
    fn poll<S: Read>(&mut self, stream: &mut S) -> io::Result<Option<RecvMsg>> {
        if let Some(msg) = self.take()? {
            return Ok(Some(msg));
        }
        // Only ever ask for what the frame in progress still needs. Reading
        // further pulls in bytes belonging to the next frame, and this buffer
        // is dropped when the gate returns — the caller carries on reading the
        // raw stream, so an over-read either loses a whole frame or leaves the
        // stream mid-frame. The gate therefore always exits with an empty
        // buffer. `take` has already validated the magic and the length.
        let needed = if self.buf.len() < 8 {
            8 - self.buf.len()
        } else {
            let len = u32::from_be_bytes([self.buf[4], self.buf[5], self.buf[6], self.buf[7]]) as usize;
            (8 + len) - self.buf.len()
        };
        let mut tmp = [0u8; 4096];
        let want = needed.min(tmp.len());
        match stream.read(&mut tmp[..want]) {
            Ok(0) => Err(io::Error::new(io::ErrorKind::UnexpectedEof, "server closed the socket")),
            Ok(n) => {
                crate::protocol::connection::hold_what_was_read(&mut self.buf, &tmp[..n])?;
                self.take()
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock
                || e.kind() == io::ErrorKind::TimedOut
                // A signal is not the peer's doing. Every read on this wire
                // answers it the same way now — `read_exact` retries it, and the
                // raw reads ask the one predicate that says so.
                || e.kind() == io::ErrorKind::Interrupted => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Pull one complete `#%#%` frame out of the buffer, if there is one.
    fn take(&mut self) -> io::Result<Option<RecvMsg>> {
        if self.buf.len() < 8 {
            return Ok(None);
        }
        if &self.buf[..4] != NS_MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "auth gate: framing lost (no #%#% magic)",
            ));
        }
        let len = u32::from_be_bytes([self.buf[4], self.buf[5], self.buf[6], self.buf[7]]) as usize;
        // Auth frames are tens of bytes. A length this far out is a corrupt
        // header, and believing it means buffering toward 4 GiB while waiting
        // for a tail that is never coming.
        if len > MAX_GATE_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("auth gate: frame claims {len} bytes"),
            ));
        }
        if self.buf.len() < 8 + len {
            return Ok(None);
        }
        let payload: Vec<u8> = self.buf[8..8 + len].to_vec();
        self.buf.drain(..8 + len);

        if ns::is_ns_text(&payload)
            && let Some((version, msg_type, fields)) = ns::ns_parse(&payload) {
                return Ok(Some(RecvMsg::Ns { version, msg_type, fields, raw: payload }));
            }
        if payload.len() >= 16
            && let Some((msg_id, sub_id, state, fields)) = xyz::xyz_parse_response(&payload) {
                return Ok(Some(RecvMsg::Xyz { msg_id, sub_id, state, fields }));
            }
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "auth gate: unparseable frame",
        ))
    }
}

/// Second factor for accounts whose token is an authenticator code (msg 774).
///
/// `AUTH_START` names the token type; type 4 takes this path, type 5 the IBKey
/// push in [`do_ib_key_2fa`]. One message goes on the wire: the code as 774
/// message code 1, framed and written raw like the IBKey messages, straight
/// after SRP passes. The server answers with code 2 carrying `PASSED` or
/// `FAILED`. Nothing on the wire prompts for the code first, so a gate that
/// waits to be asked waits forever.
///
/// The provider runs off this thread. It typically waits on a human, and the
/// loop has to keep answering the server's keepalives meanwhile or the
/// connection is dropped for going quiet.
///
/// `stream` must carry a read timeout. The loop only gets to check for the
/// code between reads, so on a blocking stream a code that arrives mid-wait
/// sits unsent until the server next says something — up to a keepalive
/// interval, against a code that is good for about thirty seconds.
pub fn do_security_code_2fa<S: Read + Write>(
    stream: &mut S,
    deadline: std::time::Instant,
    code_provider: Option<&CodeProvider>,
    cancel: Option<&AtomicBool>,
) -> io::Result<IbKeyOutcome> {
    use std::time::Instant;

    let Some(provider) = code_provider else {
        // Raised as a kind the retry ladder reads as one it does not come
        // back from: no code_provider is a lack every attempt carries, and
        // reading it as transport retried the same unfinished handshake
        // without end.
        return Err(ib_key_err(
            io::ErrorKind::Unsupported,
            "this account's second factor is an authenticator code; \
             set code_provider to supply it",
        ));
    };

    let (tx, rx) = std::sync::mpsc::channel();
    {
        let provider = provider.clone();
        std::thread::Builder::new()
            .name("ib-security-code".into())
            .spawn(move || {
                let _ = tx.send(provider(IbKeyChallenge {
                    factor: SecondFactor::AuthenticatorCode,
                    ..Default::default()
                }));
            })?;
    }
    let mut sent = false;
    let mut reader = GateReader::default();

    loop {
        // The client that asked for this wait can take it back. Ended as
        // something the retry ladder reads as the client's own doing, not an
        // answer from the venue.
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            return Err(ib_key_err(
                io::ErrorKind::Interrupted,
                "security-code wait cancelled by the client",
            ));
        }
        if Instant::now() >= deadline {
            return Err(ib_key_err(
                io::ErrorKind::TimedOut,
                "security-code gate timed out (client deadline)",
            ));
        }

        let recv = match reader.poll(stream) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof
                || e.kind() == io::ErrorKind::ConnectionReset
                || e.kind() == io::ErrorKind::ConnectionAborted =>
            {
                return Err(ib_key_err(
                    io::ErrorKind::ConnectionAborted,
                    if sent {
                        "security-code gate: server closed the socket after the code was sent"
                    } else {
                        "security-code gate: server closed the socket before a code was sent"
                    },
                ));
            }
            Err(e) => return Err(e),
        };

        // A pending verdict can end the gate before the operator's code is needed.
        let quiet = recv.is_none() && reader.buf.is_empty();

        if !sent && quiet {
            match rx.try_recv() {
                Ok(result) => {
                    let code = result?;
                    if code.trim().is_empty() {
                        return Err(ib_key_err(
                            io::ErrorKind::InvalidInput,
                            "code_provider returned an empty code; the server has no retry loop, \
                             so sending it would end the attempt",
                        ));
                    }
                    // The read above can span the deadline. Sending after it
                    // has passed puts a code on the wire for a login that is
                    // about to be abandoned.
                    if Instant::now() >= deadline {
                        return Err(ib_key_err(
                            io::ErrorKind::TimedOut,
                            "security-code gate timed out (client deadline)",
                        ));
                    }
                    // Trimmed on the way out, not just when checking for
                    // empty: surrounding whitespace reaches the server as part
                    // of the code and burns the one attempt.
                    let code = code.trim();
                    stream.write_all(&xyz::xyz_wrap(&xyz::xyz_build_security_code(code)))?;
                    log::info!("security-code gate: sent code (len={})", code.len());
                    sent = true;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    return Err(ib_key_err(
                        io::ErrorKind::Other,
                        "security-code gate: code_provider panicked without returning a code",
                    ));
                }
            }
        }

        let Some(recv) = recv else { continue };
        match recv {
            RecvMsg::Xyz { msg_id, state, fields, .. }
                if msg_id == xyz::XYZ_MSG_SECURITY_CODE && state == xyz::SECURITY_CODE_RESULT =>
            {
                let status = fields.iter().rev().find(|f| !f.is_empty()).cloned().unwrap_or_default();
                if status.eq_ignore_ascii_case("PASSED") {
                    log::info!("security-code gate: accepted");
                    return Ok(IbKeyOutcome::Approved {
                        approval_url: String::new(),
                        session_id: String::new(),
                        soft_token_hex: String::new(),
                    });
                }
                // Only a status this client recognises is echoed. The reply's fields are
                // server-controlled and this one is adjacent to the slot the
                // code was sent in — never interpolate it blind.
                let reason = if status.eq_ignore_ascii_case("FAILED") { "FAILED" } else { "unrecognized status" };
                return Err(ib_key_err(
                    io::ErrorKind::PermissionDenied,
                    format!("security code rejected ({reason})"),
                ));
            }
            RecvMsg::Xyz { msg_id, state, fields, .. }
                if msg_id == xyz::XYZ_MSG_TOKEN_AUTH && (state == 3 || state == 5) =>
            {
                if fields.iter().any(|f| f.eq_ignore_ascii_case("PASSED")) {
                    log::info!("security-code gate: accepted via AUTH_FINISH");
                    return Ok(IbKeyOutcome::Approved {
                        approval_url: String::new(),
                        session_id: String::new(),
                        soft_token_hex: String::new(),
                    });
                }
                // A rejection arriving this way is still a rejection. Falling
                // through would spin until the client deadline and then report
                // a timeout, which hides the reason.
                return Err(ib_key_err(
                    io::ErrorKind::PermissionDenied,
                    "second factor rejected at AUTH_FINISH",
                ));
            }
            RecvMsg::Xyz { msg_id, state, .. } if msg_id == xyz::XYZ_MSG_SECURITY_CODE => {
                return Err(ib_key_err(
                    io::ErrorKind::PermissionDenied,
                    format!("security-code gate: server returned code {state}"),
                ));
            }
            RecvMsg::Ns { msg_type, .. } if msg_type == NS_ERROR_RESPONSE
                || msg_type == NS_SECURE_ERROR =>
            {
                return Err(ns::refused_by_the_venue(
                    "security-code gate",
                    format!("server error type={msg_type}"),
                ));
            }
            RecvMsg::Ns { msg_type, raw, .. } if msg_type == NS_TEST_REQUEST => {
                // The timestamp from its position, or nothing: a blank slot
                // followed by a populated field echoes a value the probe did
                // not ask for otherwise.
                let ts = ns::parse_test_request_timestamp(&raw).unwrap_or_default();
                stream.write_all(&ns_build_heart_beat(NS_VERSION, &ts))?;
            }
            // Identifiers only. The derived `Debug` prints every field, and an
            // echoed frame can carry the code itself.
            // As in the sibling gate: the server has moved on, and this is
            // the only reader that will ever see this message.
            RecvMsg::Ns { msg_type, raw, .. }
                if msg_type == NS_CONNECT_RESPONSE || msg_type == NS_FIX_START =>
            {
                log::info!("security-code gate: skipped, server sent ns type={msg_type}");
                return Ok(IbKeyOutcome::Skipped { unread: Some(raw) });
            }
            RecvMsg::Ns { msg_type, .. } => log::debug!("security-code gate: ns type={msg_type}"),
            RecvMsg::Xyz { msg_id, state, .. } => {
                log::debug!("security-code gate: xyz id={msg_id} state={state}")
            }
        }
    }
}

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
/// 6. Any other `XYZ_MSG_SWCR_TOKEN` state → `PermissionDenied` error: the
///    server refused in a shape this gate doesn't model
///
/// If the server jumps straight to the connect response or the FIX start,
/// returns `Skipped` carrying that message: the gate is over, and the loop
/// that waits for it cannot ask the socket for it twice.
pub fn do_ib_key_2fa<S: Read + Write>(
    stream: &mut S,
    token_sub_type: &str,
    deadline: std::time::Instant,
    code_provider: Option<&CodeProvider>,
    cancel: Option<&AtomicBool>,
) -> io::Result<IbKeyOutcome> {
    use std::time::Instant;

    // Send SWCR_TOKEN state=1. The username slot is empty in state=1; the
    // tokenSubType (account-specific, typically "2a") is the only non-empty
    // body field. See for the canonical wire layout.
    let init = xyz::xyz_build_swcr_token_init(token_sub_type);
    let framed = xyz::xyz_wrap(&init);
    stream.write_all(&framed)?;
    log::info!(
        "2FA gate: sent SWCR_TOKEN state=1 ({} bytes inner, {} bytes framed)",
        init.len(), framed.len(),
    );

    let mut approval_url = String::new();
    let mut session_id = String::new();
    let mut announced_wait = false;
    let mut saw_challenge = false;
    let mut code_submitted = false;
    let mut pending_code: Option<std::sync::mpsc::Receiver<io::Result<String>>> = None;
    // The gate polls, and a frame can arrive across polls. The buffered
    // reader holds the half that has arrived and asks again, where the raw
    // read it replaces was two reads with nothing keeping the half-arrived
    // bytes: a poll timeout in the middle discarded them, the next read
    // started mid-frame, and an approval already given was spent on a login
    // that then died there.
    let mut reader = GateReader::default();

    loop {
        // The client that asked for this wait can take it back. Ended as
        // something the retry ladder reads as the client's own doing, not an
        // answer from the venue.
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            return Err(ib_key_err(
                io::ErrorKind::Interrupted,
                "second-factor wait cancelled by the client",
            ));
        }
        if Instant::now() >= deadline {
            return Err(ib_key_err(
                io::ErrorKind::TimedOut,
                "2FA approval timed out (client deadline)",
            ));
        }

        let recv = match reader.poll(stream) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof
                || e.kind() == io::ErrorKind::ConnectionReset
                || e.kind() == io::ErrorKind::ConnectionAborted =>
            {
                // Server closed the socket. Two distinct cases:
                //   - No state=2 arrived: the server rejected the SWCR_TOKEN
                //     init, the account doesn't have IBKey enabled, or the wire
                //     format is wrong. Fast (seconds).
                //   - state=2 arrived and then the socket closed: the ~18 min
                // server-side approval deadline fired.
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
        // Nothing has arrived yet, or not a whole frame has. The approval is
        // a person reaching for a phone, so silence is the ordinary case and
        // the loop goes back to the deadline check at the top — which is the
        // only thing that can end this wait. Treating it as a failure both
        // ends the login while the operator is still deciding, and, without a
        // timeout on the socket at all, leaves the deadline unreachable while
        // a server that has stopped talking holds the wait open for ever.
        let Some(recv) = recv else {
            // The operator's code can arrive in that silence, and reading it
            // only when the venue next spoke left it sitting — on a socket
            // the venue keeps quiet, until this wait's own deadline.
            send_a_waiting_code(stream, &mut pending_code, &mut code_submitted, deadline)?;
            continue;
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
                // pull the code from the callback and submit state=3
                // instead of waiting for a phone tap. The callback runs off
                // this thread because it blocks for as long as the operator
                // needs to read the code off the IBKey app — this loop must
                // keep answering the server's NS_TEST_REQUEST probes (~20 s
                // cadence) in the meantime or the socket is torn down before
                // the code is ever submitted. Guarded so a repeated state=2
                // (server retransmission) doesn't spawn a second provider.
                if !code_submitted && pending_code.is_none()
                    && let Some(provider) = code_provider {
                        let provider = provider.clone();
                        let challenge_info = IbKeyChallenge {
                            factor: SecondFactor::IbKeyChallengeResponse,
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
                                // Same rule as the polled path below: the grace
                                // period can straddle the deadline, and a code
                                // is single use — submitting one on a login the
                                // next loop abandons spends it for nothing.
                                if Instant::now() < deadline {
                                    submit_swcr_code(stream, &result?)?;
                                    code_submitted = true;
                                }
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
            RecvMsg::Xyz { msg_id, state, fields, .. } if msg_id == xyz::XYZ_MSG_SWCR_TOKEN && state == 4 => {
                // Challenge/Response result. Server responds PASSED → falls
                // through to AUTH_FINISH (state=3); FAILED → server skips
                // AUTH_FINISH and tears the socket down (no retry loop —
 //).
                let result = fields.iter().rev().find(|s| !s.is_empty()).cloned().unwrap_or_default();
                if result.eq_ignore_ascii_case("PASSED") {
                    log::info!("2FA gate: C/R code accepted (state=4 PASSED)");
                } else {
                    // Only a status this client recognises is echoed. The reply's fields are
                    // server-controlled and this one is adjacent to the slot the
                    // code was sent in — never interpolate it blind.
                    let reason = if result.eq_ignore_ascii_case("FAILED") { "FAILED" } else { "unrecognized status" };
                    return Err(ib_key_err(
                        io::ErrorKind::PermissionDenied,
                        format!("2FA gate: C/R code rejected (state=4 {reason})"),
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
                    // AUTH_FINISH carries no token — body is
                    // just `["", "PASSED"]`. The SOFT token used for downstream
                    // farm logons (tag 8483) is the SRP-derived K_soft, which
                    // ibx already computes correctly via `srp_compute_k`
                    // (= SHA1(strip_leading_zeros(S))). No extraction needed.
                    if approval_url.is_empty() && session_id.is_empty()
                        && soft_token_hex.is_empty()
                    {
                        return Ok(IbKeyOutcome::Skipped { unread: None });
                    }
                    return Ok(IbKeyOutcome::Approved {
                        approval_url, session_id, soft_token_hex,
                    });
                }
                return Err(ib_key_err(
                    io::ErrorKind::PermissionDenied,
                    "2FA approval rejected at AUTH_FINISH",
                ));
            }
            RecvMsg::Ns { msg_type, raw, .. } if msg_type == NS_TEST_REQUEST => {
                // The timestamp from its position, or nothing: a blank slot
                // followed by a populated field echoes a value the probe did
                // not ask for otherwise.
                let ts = ns::parse_test_request_timestamp(&raw).unwrap_or_default();
                let reply = ns_build_heart_beat(NS_VERSION, &ts);
                stream.write_all(&reply)?;
                log::debug!("2FA gate: heartbeat {ts} -> 531");
            }
            RecvMsg::Ns { msg_type, .. } if msg_type == NS_ERROR_RESPONSE
                || msg_type == NS_SECURE_ERROR =>
            {
                return Err(ns::refused_by_the_venue(
                    "2FA gate",
                    format!("server error type={msg_type}"),
                ));
            }
            RecvMsg::Xyz { msg_id, state, .. } if msg_id == xyz::XYZ_MSG_SWCR_TOKEN => {
                // An unmodelled state is the server refusing in an unrecognised
                // shape. Looping answers it with keepalives until the
                // client deadline and then reports a timeout, which hides the
                // reason for the ~18 min the operator spends waiting.
                return Err(ib_key_err(
                    io::ErrorKind::PermissionDenied,
                    format!("2FA gate: unexpected SWCR_TOKEN state {state}"),
                ));
            }
            // The server has moved past the gate. Handed back rather than
            // logged and dropped: the loop that waits for these cannot ask for
            // one again, and waited out its deadline for a message this had
            // already taken off the socket.
            RecvMsg::Ns { msg_type, raw, .. }
                if msg_type == NS_CONNECT_RESPONSE || msg_type == NS_FIX_START =>
            {
                log::info!("2FA gate: skipped, server sent ns type={msg_type}");
                return Ok(IbKeyOutcome::Skipped { unread: Some(raw) });
            }
            // Anything else is informational; keep looping. Identifiers only —
            // the derived `Debug` prints every field, and an echoed frame can
            // carry the code itself.
            RecvMsg::Ns { msg_type, .. } => log::debug!("2FA gate: ns type={msg_type}"),
            RecvMsg::Xyz { msg_id, state, .. } => {
                log::debug!("2FA gate: xyz id={msg_id} state={state}")
            }
        }

        // A provider that outlived the grace period yields here instead.
        send_a_waiting_code(stream, &mut pending_code, &mut code_submitted, deadline)?;
    }
}

/// Result of soft token authentication attempt.
pub enum SoftTokenOutcome {
    /// Token accepted.
    Passed,
    /// Token not recognized (state 5 / "UNKNOWN") — SRP fallback needed.
    Unknown,
}

/// Exchange the login for a token this session can resume with.
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
        // The caller reports this; said twice it is two lines a farm connection
        // for a documented outcome with a defined recovery, and there are four
        // of those on a session.
        log::debug!("SOFT_TOKEN: farm returned state 5 (UNKNOWN) — SRP fallback needed");
        return Ok(SoftTokenOutcome::Unknown);
    }
    if state2 != 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("SOFT_TOKEN: expected state 2, got {state2}"),
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
    let response_hex = format!("{response_int:x}");

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
            format!("SOFT_TOKEN auth failed: {result}"),
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
            format!("Farm SRP: expected state 2, got {state2}"),
        ));
    }

    let data_fields = extract_srp_data(&fields2, username);
    // The venue may state its own group; otherwise this client's applies.
    let (n, g) = stated_group(&data_fields)?;

    // Generate client keys with 32-byte private key
    let mut a_bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut a_bytes);
    let a_priv = BigUint::from_bytes_be(&a_bytes);
    let a_pub = g.modpow(&a_priv, &n);

    // State 3: Send client public key A (FIX-framed)
    let a_hex = format!("{a_pub:x}");
    let msg3 = xyz::xyz_build_srp_v20(3, &[("L", &a_hex)]);
    stream.write_all(&wrap_xyz_fix(&msg3))?;

    // State 4: Receive salt + B (FIX-framed)
    let recv4 = recv_8eq1(stream, carry)?;
    let xyz4 = extract_xyz(&recv4);
    let (_, _, state4, fields4) = xyz::xyz_parse_response(xyz4)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Farm SRP: invalid state 4"))?;

    if state4 == 7 {
        let result = fields4.get(9).map(|s| s.as_str()).unwrap_or("FAILED");
        return Err(ns::refused_by_the_venue("Farm SRP early error (state 7)", result.to_string()));
    }
    if state4 != 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Farm SRP: expected state 4, got {state4}"),
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
    let m1_hex = format!("{m1:x}");
    let msg5 = xyz::xyz_build_srp_v20(5, &[("N", &m1_hex)]);
    stream.write_all(&wrap_xyz_fix(&msg5))?;

    // State 6: Receive AUTH_RESULT (FIX-framed).
    //
    // As on the session's own SRP: a message carrying an id this exchange does
    // not use is skipped rather than read as the verdict.
    let farm_deadline = std::time::Instant::now()
        + std::time::Duration::from_secs(crate::config::TIMEOUT_SSL_AUTH);
    let fields6 = loop {
        if std::time::Instant::now() >= farm_deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "no farm SRP result before the auth timeout, with the venue still speaking",
            ));
        }
        let frame = recv_8eq1(stream, carry)?;
        let xyz = extract_xyz(&frame);
        let (_, _, state, fields) = xyz::xyz_parse_response(xyz).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "Farm SRP: invalid state 6")
        })?;
        if state == SRP_AUTH_RESULT {
            break fields;
        }
        log::info!("received unknown msg id {state} while awaiting the farm SRP result");
    };
    check_server_proof(&fields6, &a_pub, &m1, &k, "farm SRP")?;

    let result = fields6
        .get(9)
        .filter(|s| !s.is_empty())
        .or_else(|| fields6.iter().rev().find(|s| !s.is_empty()))
        .map(|s| s.as_str())
        .unwrap_or("");

    if result == "PASSED" {
        log::info!("Farm SRP auth PASSED");
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("Farm SRP FAILED: {result}"),
        ))
    }
}

#[cfg(test)]
mod tests;
