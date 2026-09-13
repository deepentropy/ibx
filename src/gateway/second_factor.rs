//! The approval a login waits for after the password.
//!
//! A live account with a second factor configured does not finish logging on
//! when the handshake does: the venue sends a challenge and waits, sometimes
//! for as long as a person takes to reach for a phone. What arrives back
//! decides whether the session exists.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use num_bigint::BigUint;
use sha1::{Digest, Sha1};

use super::*;

/// The clock one wait of these exchanges runs under, matching the one the
/// auth socket is given.
fn wait_deadline() -> std::time::Instant {
    std::time::Instant::now() + std::time::Duration::from_secs(TIMEOUT_SSL_AUTH)
}

/// SOFT_TOKEN challenge-response over the TLS/NS channel (for CCP reconnect).
pub(super) fn do_ccp_soft_token<S: Read + Write>(stream: &mut S, session_key: &BigUint) -> io::Result<()> {
    use crate::protocol::xyz;

    // State 1: Send empty init
    let msg1 = xyz::xyz_build_soft_token(1, "", "", "");
    stream.write_all(&xyz::xyz_wrap(&msg1))?;

    // State 2: Receive challenge
    let recv2 = session::recv_msg(stream, wait_deadline())?;
    let challenge_hex = match recv2 {
        session::RecvMsg::Xyz { state: 2, fields, .. } => {
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
    let recv4 = session::recv_msg(stream, wait_deadline())?;
    let result = match recv4 {
        // The state is checked as well as the shape. Taken from any XYZ
        // frame, an unrelated message ending in the right word reads as the
        // answer to this challenge.
        session::RecvMsg::Xyz { state: 4, fields, .. } => {
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
            format!("CCP SOFT_TOKEN auth failed: {result}"),
        ))
    }
}

/// Which second-factor exchange an `AUTH_START` token type selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SecondFactorRoute {
    /// No gate at all. Paper sessions never present a second factor.
    None,
    /// `XYZ 775` — the IBKey push, or its Challenge/Response variant.
    IbKey,
    /// `XYZ 774` — an authenticator code.
    SecurityCode,
    /// A type this client has no exchange for. Better to say so than to send
    /// the wrong message and report whatever the server does about it.
    Unsupported,
}

/// The text of a decrypted frame that is `AUTH_START`, and an error for one
/// that is not.
///
/// `recv_secure` clears the outer envelope only. Everything taken out of the
/// frame after it is taken by position — the second-factor type and sub-type,
/// the auth mode — so another message type at that point in the handshake
/// supplies those from fields that mean something else, and the login fails
/// later as a rejected token or a closed socket rather than as the wrong frame
/// it was.
pub(super) fn auth_start_text(payload: &[u8]) -> io::Result<String> {
    let text = String::from_utf8_lossy(payload).into_owned();
    let msg_type: u32 = text.split(';').nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    if msg_type != ns::NS_AUTH_START {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Expected AUTH_START ({}), got {}", ns::NS_AUTH_START, msg_type),
        ));
    }
    Ok(text)
}

/// The second-factor token AUTH_START names, as `(type, sub-type)`.
///
/// Field 4 carries the type, optionally followed by a per-session sub-type
/// after a `.`, and carries a comma-separated list when the account has more
/// than one factor enabled. The list is split first, so a sub-type on one
/// entry cannot swallow the entries after it — `"5.2i,4"` is type `5`
/// sub-type `2i` and a second entry, not a sub-type of `"2i,4"`.
///
/// The sub-type returned is the one belonging to the type the gate will route
/// to, and the type string keeps the whole list for that routing decision.
pub(super) fn parse_auth_start_token(auth_start: &str) -> (String, Option<String>) {
    let fields: Vec<&str> = auth_start.split(';').collect();
    let token = fields.get(4).map(|f| f.trim()).unwrap_or("");

    let mut entries: Vec<(String, Option<String>)> = Vec::new();
    for entry in token.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let (ty, sub) = match entry.split_once('.') {
            Some((t, s)) => (t.trim(), s.trim()),
            None => (entry, ""),
        };
        entries.push((ty.to_string(), (!sub.is_empty()).then(|| sub.to_string())));
    }

    // The sub-type has to belong to the type the gate routes to, so which type
    // that is has to be decided first — exactly as `second_factor_route`
    // decides it, IBKey when present and the authenticator otherwise. Searching
    // for a sub-type across types instead lets an entry the gate is not using
    // supply one: `4.auth,5` routes to IBKey and would send `auth`.
    let routed = ["5", "4"]
        .into_iter()
        .find(|want| entries.iter().any(|(ty, _)| ty == want))
        .or_else(|| entries.first().map(|(ty, _)| ty.as_str()));
    let sub_type = routed.and_then(|routed| {
        entries.iter()
            .find(|(ty, sub)| ty == routed && sub.is_some())
            .and_then(|(_, sub)| sub.clone())
    });

    let types: Vec<&str> = entries.iter().map(|(ty, _)| ty.as_str()).collect();
    (types.join(","), sub_type)
}

/// Run the per-session second-factor gate after SRP. Returns the SOFT token
/// when the gate issued one.
///
/// A reconnect runs this too: the server drops a session across its own
/// maintenance windows, answers the next soft-token connect with SRP, and then
/// asks for the second factor again. Skipping it there left an unattended
/// client retrying a handshake it could never finish.
pub(super) fn run_second_factor(
    tls: &mut native_tls::TlsStream<TcpStream>,
    sf: SecondFactor<'_>,
) -> io::Result<SecondFactorOutcome> {
    let mut unread: Option<Vec<u8>> = None;
    // An advertised type this client cannot perform is worth saying out
    // loud: sending 775 at it gets the socket closed before any challenge,
    // and skipping the gate leaves the server waiting until the connect
    // dies with "Never received data start after auth". Neither names the
    // cause. See `second_factor_route` for why an absent type is
    // the one case that is not an error.
    let route = second_factor_route(sf.paper, &sf.token_type);
    if !sf.paper {
        log::debug!(
            "second factor: AUTH_START type {:?} sub {:?} -> {route:?}",
            sf.token_type, sf.token_sub_type,
        );
    }
    if route == SecondFactorRoute::SecurityCode {
        // Authenticator-code accounts take the 774 exchange rather than the
        // IBKey push. The same code_provider supplies the code.
        let deadline = std::time::Instant::now()
            + std::time::Duration::from_secs(sf.timeout_secs);
        log::info!(
            "Live login for {}: second factor is an authenticator code; awaiting code_provider",
            sf.username,
        );
        // The code is written raw, with no DH encryption — the same
        // transport the IBKey gate uses. The encrypted variant gets the
        // connection reset on receipt.
        // Poll the socket so the gate can submit as soon as the code is
        // available instead of waiting for the server's next keepalive: an
        // authenticator code is only valid for ~30s, and a 20s wait spends
        // most of it. Restored afterwards.
        tls.get_ref().set_read_timeout(Some(Duration::from_millis(500)))?;
        let gate = session::do_security_code_2fa(
            tls, deadline, sf.code_provider, sf.cancel,
        );
        let restore = tls.get_ref().set_read_timeout(None);
        if let session::IbKeyOutcome::Skipped { unread: carried } = gate? {
            unread = carried;
        }
        restore?;
        log::info!("security-code gate: passed")
    } else if route == SecondFactorRoute::Unsupported {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "second-factor token type {:?} is not supported; AUTH_START advertised it for {}",
                sf.token_type, sf.username,
            ),
        ));
    }
    if route == SecondFactorRoute::IbKey {
        let deadline = std::time::Instant::now()
            + std::time::Duration::from_secs(sf.timeout_secs);
        // Live logins enter a human-approval window here: connect() blocks
        // until the second factor is approved (mobile push) or this deadline
        // fires. Announce it up front so a stalled connect() reads as
        // "waiting for approval" rather than a hang.
        // Accounts with no second factor fall straight through (Skipped).
        if sf.code_provider.is_none() {
            log::info!(
                "Live login for {}: waiting for second-factor approval (mobile push); \
                 connect() blocks up to {}s. Use paper=true, a lower ib_key_timeout_secs, \
                 or a code_provider to avoid this.",
                sf.username, sf.timeout_secs,
            );
        } else {
            log::info!(
                "Live login for {}: second-factor via code_provider (Challenge/Response); \
                 connect() blocks up to {}s awaiting the challenge.",
                sf.username, sf.timeout_secs,
            );
        }
        // The server's per-session value wins. `ib_key_token_sub_type` is
        // the fallback for an AUTH_START that states none, not an override
        // — a fixed value cannot be right for a session it predates.
        let token_sub_type = sf.token_sub_type
            .as_deref()
            .unwrap_or(sf.default_sub_type);
        log::info!(
            "2FA gate: token sub-type {:?} ({})",
            token_sub_type,
            if sf.token_sub_type.is_some() { "from AUTH_START" } else { "configured default" },
        );
        // Polled, as the security-code gate beside this one is. The wait is a
        // person reaching for a phone, so the socket is quiet for most of it —
        // and a wait with no timeout on the socket cannot reach its own
        // deadline if the server stops talking rather than closing.
        tls.get_ref().set_read_timeout(Some(Duration::from_millis(500)))?;
        let gate = session::do_ib_key_2fa(
            tls,
            token_sub_type,
            deadline,
            sf.code_provider,
            sf.cancel,
        );
        tls.get_ref().set_read_timeout(None)?;
        match gate? {
            session::IbKeyOutcome::Skipped { unread: carried } => {
                log::info!("2FA gate: skipped (no second factor)");
                unread = carried;
            }
            session::IbKeyOutcome::Approved { approval_url, session_id } => {
                log::info!(
                    "2FA gate: approved (session_id={}, approval_url={})",
                    if session_id.is_empty() { "<none>" } else { &session_id },
                    if approval_url.is_empty() { "<none>" } else { &approval_url },
                );
            }
        }
    }
    Ok(SecondFactorOutcome { unread })
}

/// What the second factor left behind: any message the gate read that belongs
/// to what follows it.
///
/// Not a token. The gate issues none — the body that would carry one is
/// `["", "PASSED"]` — and the token a farm logon hashes is the SRP-derived
/// key, the same one every reconnect hashes.
pub(super) struct SecondFactorOutcome {
    /// A message already taken off the socket for the post-auth loop, which
    /// cannot ask for it again.
    pub unread: Option<Vec<u8>>,
}

/// An absent token type routes to the IBKey gate rather than skipping the
/// second factor. That gate opens by sending its init and reports `Skipped`
/// when the server answers `AUTH_FINISH PASSED`, which is how an account with
/// no second factor completes — so skipping it would leave the server waiting
/// on an init that never comes.
///
/// The field carries a comma-separated list when the account has more than one
/// factor enabled — `AUTH_START` advertised `4,5` for an account with both an
/// authenticator and IBKey. Reading the whole field as one type refused the
/// login outright, so each entry is considered and IBKey is preferred: it is
/// the only one that completes without a `code_provider`, and it still serves
/// a configured one through Challenge/Response.
pub(super) fn second_factor_route(paper: bool, token_type: &str) -> SecondFactorRoute {
    if paper {
        return SecondFactorRoute::None;
    }
    if token_type.is_empty() {
        return SecondFactorRoute::IbKey;
    }
    let mut saw_security_code = false;
    for entry in token_type.split(',') {
        match entry.trim() {
            "5" => return SecondFactorRoute::IbKey,
            "4" => saw_security_code = true,
            _ => {}
        }
    }
    if saw_security_code {
        SecondFactorRoute::SecurityCode
    } else {
        SecondFactorRoute::Unsupported
    }
}

/// Read the frame that finishes authentication, and refuse anything else.
///
/// AUTH_FINISH is XYZ message 771 in state 3 or 5, with a body of exactly
/// `PASSED`. Any other state, message id or body is a refusal, including a
/// state-3 frame carrying other text.
///
/// A receive error is also a refusal: the farm logons that follow authenticate
/// with the session key, so anything short of `PASSED` must not reach them. An
/// NS message here means authentication is already past, so it is handed on
/// rather than refused — and rather than dropped: it belongs to the post-auth
/// exchange, which cannot ask the socket for it a second time. Dropping it
/// left a reconnect waiting on a frame it was already holding until its own
/// deadline, failing on its own success.
pub(super) fn expect_auth_finish<S: Read>(stream: &mut S, whose: &str) -> io::Result<Option<Vec<u8>>> {
    use crate::protocol::xyz::XYZ_MSG_TOKEN_AUTH;

    match session::recv_msg(stream, wait_deadline()) {
        Ok(session::RecvMsg::Xyz { msg_id, state, fields, .. }) => {
            let passed = fields.iter().any(|f| f == "PASSED");
            if msg_id == XYZ_MSG_TOKEN_AUTH && matches!(state, 3 | 5) && passed {
                log::info!("{whose} AUTH_FINISH: state={state} PASSED");
                Ok(None)
            } else {
                let stated = fields.iter().rev().find(|s| !s.is_empty()).map(String::as_str).unwrap_or("");
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "{whose}: authentication was not finished — message {msg_id} state {state}                          stating {stated:?}",
                    ),
                ))
            }
        }
        Ok(session::RecvMsg::Ns { msg_type, raw, .. }) => {
            log::info!("{whose} post-auth NS type={msg_type}");
            Ok(Some(raw))
        }
        Err(e) => Err(io::Error::new(
            e.kind(),
            format!("{whose}: nothing said the authentication finished: {e}"),
        )),
    }
}
