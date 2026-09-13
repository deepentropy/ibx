//! The tests for this module.
//!
//! One file per module, as `api/client` already does it. Each block below
//! reaches the code it tests through `super::super`, which is the module this
//! file belongs to.

use super::*;
use crate::reliability::retry;

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

// ── IBKey Challenge/Response gate ─────────────────────────────

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

// ── recv_8eq1 framing / coalesced tail ────────────────────────

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

/// The exact regression: a gateway coalesces the final auth
/// response and the farm logon ACK that follows it into one TCP read.
/// `recv_8eq1` must return only the framed `8=1` message and preserve the
/// trailing ACK bytes in the carry buffer, not discard them.
#[test]
fn recv_8eq1_preserves_coalesced_tail() {
    use std::io::Write;
    use std::net::TcpListener;

    let auth_msg = framed_8eq1(b"PASSED");
    // Stand-in for the farm logon ACK that follows immediately.
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
    let info = get_hw_info(None, None).expect("this machine answers with a card");
    assert!(info.contains('|'));
    let parts: Vec<&str> = info.split('|').collect();
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0].len(), 8); // 4-byte hex
}

// The machine_id is persistent (read from the hwid file / IBX_HWID env,
// created once — see read_or_create_hwid), so repeated calls
// must return the SAME id. This test previously asserted the pre-#132
// behavior (random id per call) and failed once a hwid file existed.
#[test]
fn hw_info_machine_id_is_stable_across_calls() {
    let info1 = get_hw_info(None, None).expect("this machine answers with a card");
    let info2 = get_hw_info(None, None).expect("this machine answers with a card");
    let machine1 = info1.split('|').next().unwrap();
    let machine2 = info2.split('|').next().unwrap();
    assert_eq!(machine1, machine2, "Persistent machine ID must not change between calls");
    assert_eq!(machine1.len(), 8);
    assert!(machine1.chars().all(|c| c.is_ascii_hexdigit()),
        "machine ID must be 8 hex chars, got {machine1:?}");
}

/// The address the session opened on is the one it presents, and the card is
/// the one belonging to it.
///
/// The client this replaces takes the local address off the socket that
/// reached the venue and builds the identity from that — the card of the
/// interface holding that address, and the address itself — once, for the life
/// of the process. Probed separately, this client could answer with the route
/// to somewhere else, with nothing at all where that route is blocked, or with
/// a different answer for the order connection than for each farm.
#[test]
fn the_address_the_session_opened_on_is_the_one_it_presents() {
    // Whatever this process has already noted, if anything: the value is
    // settled once, as it is there, so this reads it rather than setting it.
    let noted = get_lan_ip(None);
    assert!(!noted.is_empty(), "an address is presented either way");
    // A stated address still wins over it, because a caller inside a
    // container is telling this client what the machine outside is.
    assert_eq!(get_lan_ip(Some("10.11.12.13")), "10.11.12.13");
    // And an address that names no machine does not, whichever it is.
    for spelling in ["0.0.0.0", "::", "224.0.0.1", "255.255.255.255"] {
        assert_eq!(
            get_lan_ip(Some(spelling)), noted,
            "{spelling} was presented as where this machine is",
        );
    }
    // Six bytes of nothing is not a card: the venue takes the identity and
    // then refuses the farm logon behind it.
    assert_eq!(
        get_hw_info(None, Some("00:00:00:00:00:00")), get_hw_info(None, None),
        "a card of all zeroes was presented",
    );
}

/// A card and an address the caller stated are the ones presented.
///
/// The identity the venue holds a session under is three things and only one
/// of them used to be the caller's to state. The other two are read off the
/// machine, which answers with a virtual card on some systems and with the
/// container's own address inside one — neither of which is the machine the
/// session belongs to.
#[test]
fn a_stated_card_and_address_are_the_ones_presented() {
    // However the caller separated the bytes, and in the case the machine's
    // own is read in: the same machine has to present the same string.
    for spelling in ["aa:bb:cc:dd:ee:ff", "AA-BB-CC-DD-EE-FF", "aabbccddeeff"] {
        assert_eq!(
            get_hw_info(Some("abc123"), Some(spelling)),
            Some("00abc123|AA:BB:CC:DD:EE:FF".to_string()),
            "{spelling} did not reach the identity as the card it names",
        );
    }
    // What is not six bytes names no card, and the machine's own stands.
    for spelling in ["", "aa:bb:cc", "aa:bb:cc:dd:ee:ff:00", "not a mac"] {
        assert_eq!(
            get_hw_info(None, Some(spelling)), get_hw_info(None, None),
            "{spelling} was presented as a card",
        );
    }

    assert_eq!(get_lan_ip(Some("10.0.0.9")), "10.0.0.9");
    assert_eq!(get_lan_ip(Some("  10.0.0.9  ")), "10.0.0.9", "read as an address, not as text");
    // And what is not an address leaves the machine's own to answer.
    for spelling in ["", "not.an.ip", "999.1.1.1"] {
        assert_eq!(
            get_lan_ip(Some(spelling)), get_lan_ip(None),
            "{spelling} was presented as an address",
        );
    }
}

/// A stated machine identity is the one presented, padded to the width the
/// field takes. It reaches the wire in tag 6351 and in the connection request,
/// and every other test here states none and reads back the machine's own.
#[test]
fn a_stated_machine_identity_is_the_one_presented() {
    assert!(
        get_hw_info(Some("abc123"), None)
            .expect("this machine answers with a card")
            .starts_with("00abc123|"),
        "got {:?}",
        get_hw_info(Some("abc123"), None),
    );
    // Anything the field cannot carry is not an identity to present, so the
    // machine's own stands rather than a malformed one going out.
    assert_eq!(get_hw_info(Some("not hex"), None), get_hw_info(None, None));
}

#[test]
fn hw_info_mac_format_is_six_hex_octets() {
    let info = get_hw_info(None, None).expect("this machine answers with a card");
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
    let ip = get_lan_ip(None);
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
        raw: Vec::new(),
    };
    match msg {
        RecvMsg::Ns { version, msg_type, fields, .. } => {
            assert_eq!(version, 534);
            assert_eq!(msg_type, 99);
            assert_eq!(fields.len(), 2);
        }
        _ => panic!("Expected Ns variant"),
    }
}

/// A message the venue states between the client's proof and the verdict is
/// passed over, not taken for the verdict.
///
/// The frames here are constructed: what they pin is that an id this exchange
/// does not use is read past rather than taken for the answer. The reason that
/// behaviour is wanted came from a session — a logon was refused because a
/// message arriving before the verdict was read as one, and the field taken
/// from it said `UNKNOWN` — and the session is what that says, not this.
#[test]
fn a_message_that_is_not_the_srp_verdict_is_read_past() {
    let mut wire = Vec::new();
    // Two of the venue's own, then the verdict.
    wire.extend_from_slice(&xyz::xyz_wrap(&xyz::xyz_build_srp_v20(7, &[("N", "0")])));
    wire.extend_from_slice(&xyz::xyz_wrap(&xyz::xyz_build_srp_v20(9, &[("N", "0")])));
    wire.extend_from_slice(&xyz::xyz_wrap(&xyz::xyz_build_srp_v20(6, &[("N", "PASSED")])));

    let mut cursor = io::Cursor::new(wire);
    let fields = super::srp_result_fields(&mut cursor).expect("the verdict was not reached");
    assert!(
        fields.iter().any(|f| f == "PASSED"),
        "read past the two and answered with something other than the verdict: {fields:?}",
    );
}

/// Nothing bounds the reading but the connection.
///
/// A count of its own would be this client's invention: a venue that states no
/// verdict is read until the socket ends, and that is where the attempt ends.
#[test]
fn reading_past_ends_when_the_connection_does() {
    let mut wire = Vec::new();
    for _ in 0..40 {
        wire.extend_from_slice(&xyz::xyz_wrap(&xyz::xyz_build_srp_v20(7, &[("N", "0")])));
    }
    let mut cursor = io::Cursor::new(wire);
    super::srp_result_fields(&mut cursor)
        .expect_err("a verdict was answered where the venue stated none");
}

/// The venue's own refusal, stated where the salt and B would be.
///
/// State 7 carries the verdict field the AUTH_RESULT carries, so it is the
/// account's answer and every host answers for the same account. Raised with
/// no kind on it the retry ladder read it as a connection that did not come
/// up and dialled again with the same credentials, spending the recovery
/// budget — and the account's attempts — on an answer that does not change.
#[test]
fn an_early_srp_refusal_is_not_dialled_again() {
    let n_hex = format!("{:x}", srp::srp_venue_n());
    let g_hex = format!("{:x}", srp::SRP_VENUE_G);
    let mut wire = xyz::xyz_wrap(&xyz::xyz_build_srp_v20(2, &[("H", &n_hex), ("I", &g_hex)]));
    wire.extend_from_slice(&xyz::xyz_wrap(&xyz::xyz_build_srp_v20(7, &[
        ("P", "1;invalid username or password;"),
    ])));

    let mut stream = ScriptedStream::new(wire);
    let err = super::do_srp(&mut stream, "user", "pw").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
    assert_eq!(
        retry::DisconnectReason::from_error(&err).recovery(), retry::Recovery::Stop,
        "the ladder re-dials a refusal the venue has already answered: {err}",
    );
}

/// The same refusal on the farm exchange, which states it in FIX framing.
#[test]
fn an_early_farm_srp_refusal_is_not_dialled_again() {
    use std::io::Write;
    use std::net::TcpListener;

    let n_hex = format!("{:x}", srp::srp_venue_n());
    let g_hex = format!("{:x}", srp::SRP_VENUE_G);
    let mut wire = wrap_xyz_fix(&xyz::xyz_build_srp_v20(2, &[("H", &n_hex), ("I", &g_hex)]));
    wire.extend_from_slice(&wrap_xyz_fix(&xyz::xyz_build_srp_v20(7, &[
        ("P", "1;invalid username or password;"),
    ])));

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        s.write_all(&wire).unwrap();
        // Held open so the refusal is the reason the client reports, not a close.
        std::thread::sleep(std::time::Duration::from_millis(200));
    });

    let mut client = TcpStream::connect(addr).unwrap();
    client
        .set_read_timeout(Some(std::time::Duration::from_millis(FARM_LOGON_POLL_MS)))
        .unwrap();

    let mut carry = Vec::new();
    let err = do_srp_farm(&mut client, "user", "pw", &mut carry).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
    assert_eq!(
        retry::DisconnectReason::from_error(&err).recovery(), retry::Recovery::Stop,
        "the ladder re-dials a refusal the venue has already answered: {err}",
    );

    server.join().unwrap();
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

/// A type this read is not waiting for is read past, and the secure message
/// behind it is the one answered.
///
/// The venue states messages of its own accord — a backup host among them —
/// and refusing the login over one ends a session the venue had no complaint
/// about.
#[test]
fn recv_secure_reads_past_a_type_it_is_not_waiting_for() {
    let mut wire = build_ns_frame("50;527;host.example;");
    wire.extend_from_slice(&build_ns_frame("50;999;payload;"));
    // Then the one being waited for. Its body is not decipherable by a channel
    // that never shook hands, so what is checked is which message was reached.
    wire.extend_from_slice(&build_ns_frame("50;534;bm90LWNpcGhlcnRleHQ=;"));

    let mut cursor = io::Cursor::new(wire);
    let mut channel = SecureChannel::new();
    let err = recv_secure(&mut cursor, &mut channel).unwrap_err();
    let said = err.to_string();
    assert!(
        !said.contains("527") && !said.contains("999"),
        "stopped on a message it was not waiting for: {said}",
    );
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
    // A flag that was not set is absent
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

/// A server that stays silent until the client has written something, then
/// answers `chunk` bytes at a time. Both halves matter: the silence keeps
/// these tests independent of when the provider thread happens to return,
/// and the chunking reproduces a polled socket handing back a frame in
/// pieces.
struct RepliesAfterWrite {
    /// Served before the client writes anything.
    preface: Vec<u8>,
    pre_pos: usize,
    /// Served once the client has written.
    reply: Vec<u8>,
    read_pos: usize,
    written: Vec<u8>,
    chunk: usize,
    /// When set, the reply is withheld until this appears in `written` —
    /// a server that answers the code, not merely the first thing sent.
    trigger: Option<Vec<u8>>,
    /// Alternates a timeout between chunks, which is what a polled socket
    /// actually does and what `read_exact` cannot survive.
    stall: bool,
}

impl RepliesAfterWrite {
    fn new(reply: Vec<u8>) -> Self {
        Self {
            preface: Vec::new(), pre_pos: 0,
            reply, read_pos: 0, written: Vec::new(),
            chunk: usize::MAX, trigger: None, stall: false,
        }
    }
    /// Hand the reply back `chunk` bytes at a time, timing out in between.
    fn chunked(reply: Vec<u8>, chunk: usize) -> Self {
        Self { chunk, ..Self::new(reply) }
    }
    /// Say something before the client has written, so the gate is
    /// observed mid-wait rather than after the exchange.
    fn with_preface(preface: Vec<u8>, trigger: Vec<u8>, reply: Vec<u8>) -> Self {
        Self { preface, trigger: Some(trigger), ..Self::new(reply) }
    }

    /// Has the client sent what the server is waiting for?
    fn ready(&self) -> bool {
        match &self.trigger {
            Some(t) => self.written.windows(t.len()).any(|w| w == t.as_slice()),
            None => !self.written.is_empty(),
        }
    }
}

impl io::Read for RepliesAfterWrite {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.chunk != usize::MAX {
            self.stall = !self.stall;
            if self.stall {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "poll timeout"));
            }
        }
        let ready = self.ready();
        let (src, pos) = if ready {
            (&self.reply, &mut self.read_pos)
        } else {
            (&self.preface, &mut self.pre_pos)
        };
        let remaining = src.len().saturating_sub(*pos);
        if remaining == 0 {
            return if ready {
                Err(io::Error::new(io::ErrorKind::UnexpectedEof, "scripted EOF"))
            } else {
                Err(io::Error::new(io::ErrorKind::WouldBlock, "quiet"))
            };
        }
        let n = remaining.min(buf.len()).min(self.chunk);
        buf[..n].copy_from_slice(&src[*pos..*pos + n]);
        *pos += n;
        Ok(n)
    }
}

impl io::Write for RepliesAfterWrite {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.written.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

fn code_provider_returning(code: &'static str) -> CodeProvider {
    std::sync::Arc::new(move |challenge| {
        // The callback has to be told which factor it is answering; the
        // two want different codes and the shipped example branches on it.
        assert_eq!(
            challenge.factor,
            SecondFactor::AuthenticatorCode,
            "this gate must ask for an authenticator code",
        );
        Ok(code.to_string())
    })
}

/// A provider that never answers, so the gate is observed with `sent=false`.
fn code_provider_that_never_answers() -> CodeProvider {
    std::sync::Arc::new(|_| {
        std::thread::sleep(std::time::Duration::from_secs(60));
        Ok(String::new())
    })
}

struct VerdictAfterCodeReady {
    incoming: Vec<u8>,
    pos: usize,
    ready: Arc<AtomicBool>,
    written: Vec<u8>,
}
impl io::Read for VerdictAfterCodeReady {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // The provider is ready while the venue's verdict is still arriving,
        // so both can be available to the gate on the same pass.
        while !self.ready.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
        let remaining = self.incoming.len().saturating_sub(self.pos);
        if remaining == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "scripted EOF"));
        }
        let n = remaining.min(buf.len());
        buf[..n].copy_from_slice(&self.incoming[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}
impl io::Write for VerdictAfterCodeReady {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.written.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}

fn security_code_result(fields: &[&str]) -> Vec<u8> {
    frame_xyz(&xyz::xyz_build(
        xyz::XYZ_MSG_SECURITY_CODE,
        xyz::SECURITY_CODE_RESULT,
        "",
        fields,
    ))
}

// ── do_security_code_2fa — authenticator code ─────────────

#[test]
fn security_code_gate_sends_the_code_and_accepts_passed() {
    let mut stream = RepliesAfterWrite::new(security_code_result(&["", "", "", "PASSED"]));
    let outcome = do_security_code_2fa(
        &mut stream,
        far_future_deadline(),
        Some(&code_provider_returning("123456")), None)
    .expect("PASSED must be accepted");
    assert!(matches!(outcome, IbKeyOutcome::Approved { .. }));
    // The code must actually reach the wire, in the security-token slot.
    assert_eq!(
        stream.written,
        xyz::xyz_wrap(&xyz::xyz_build_security_code("123456")),
        "the gate must send exactly the 774 code-1 frame"
    );
}

#[test]
fn security_code_gate_reassembles_a_reply_split_across_reads() {
    // The gate polls, so a read can return part of a frame. Byte-at-a-time
    // delivery is the worst case: anything that drops a partial read here
    // resumes mid-frame and dies on the next magic check.
    let mut stream = RepliesAfterWrite::chunked(security_code_result(&["", "", "", "PASSED"]), 1);
    let outcome = do_security_code_2fa(
        &mut stream,
        far_future_deadline(),
        Some(&code_provider_returning("123456")), None)
    .expect("a frame split across reads must still be accepted");
    assert!(matches!(outcome, IbKeyOutcome::Approved { .. }));
}

#[test]
fn security_code_gate_answers_keepalives_while_it_waits() {
    // Going quiet gets the connection dropped, so the wait loop has to keep
    // answering NS_TEST_REQUEST.
    let probe = ns::ns_build(NS_VERSION, NS_TEST_REQUEST, &["20260729-01:02:03"], "MISC");
    let mut stream = RepliesAfterWrite::with_preface(
        probe,
        xyz::xyz_wrap(&xyz::xyz_build_security_code("123456")),
        security_code_result(&["", "", "", "PASSED"]),
    );
    // Slow enough that the probe is answered while the gate is still
    // waiting on the provider — the situation the off-thread call exists
    // for. A provider that returns instantly never tests it.
    let provider: CodeProvider = std::sync::Arc::new(|_| {
        std::thread::sleep(std::time::Duration::from_millis(60));
        Ok("123456".to_string())
    });
    do_security_code_2fa(&mut stream, far_future_deadline(), Some(&provider), None)
        .expect("PASSED must be accepted");
    let heartbeat = ns_build_heart_beat(NS_VERSION, "20260729-01:02:03");
    let code_frame = xyz::xyz_wrap(&xyz::xyz_build_security_code("123456"));
    let hb_at = stream.written.windows(heartbeat.len()).position(|w| w == heartbeat);
    let code_at = stream.written.windows(code_frame.len()).position(|w| w == code_frame);
    assert!(hb_at.is_some(), "the gate must answer the keepalive");
    assert!(
        hb_at < code_at,
        "the keepalive must be answered while waiting, not after the code went out"
    );
}

#[test]
fn security_code_gate_echoes_the_timestamp_from_its_position_or_nothing() {
    // A probe with a blank timestamp slot and a populated later field is
    // echoed nothing at that position: taking the first non-empty field
    // answers with a value the probe did not ask for.
    let probe = ns::ns_build(NS_VERSION, NS_TEST_REQUEST, &["", "1750000000"], "MISC");
    let mut stream = RepliesAfterWrite::with_preface(
        probe,
        xyz::xyz_wrap(&xyz::xyz_build_security_code("123456")),
        security_code_result(&["", "", "", "PASSED"]),
    );
    let provider: CodeProvider = std::sync::Arc::new(|_| {
        std::thread::sleep(std::time::Duration::from_millis(60));
        Ok("123456".to_string())
    });
    do_security_code_2fa(&mut stream, far_future_deadline(), Some(&provider), None)
        .expect("PASSED must be accepted");
    let heartbeat = ns_build_heart_beat(NS_VERSION, "");
    assert!(
        stream.written.windows(heartbeat.len()).any(|w| w == heartbeat),
        "the heartbeat must echo the timestamp's position — here, nothing",
    );
    assert!(
        !stream.written.windows(10).any(|w| w == b"1750000000"),
        "the heartbeat echoed a field the probe did not ask for",
    );
}

#[test]
fn security_code_gate_reports_a_rejection_as_a_rejection_not_a_timeout() {
    // A rejection that falls through is answered with keepalives until the
    // deadline and then reported as a timeout, which hides the reason.
    let mut stream = RepliesAfterWrite::new(security_code_result(&["", "", "", "FAILED"]));
    let err = do_security_code_2fa(
        &mut stream,
        far_future_deadline(),
        Some(&code_provider_returning("123456")), None)
    .unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    assert!(!err.to_string().contains("timed out"), "got {err}");
}

#[test]
fn security_code_gate_reports_a_774_rejection_before_a_code_is_sent() {
    // A refusal can arrive while the operator is still reading the code.
    // Silence after it must not turn that refusal into a timeout.
    for chunk in [usize::MAX, 1] {
        let mut stream = RepliesAfterWrite::chunked(Vec::new(), chunk);
        stream.preface = security_code_result(&["", "", "", "FAILED"]);
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);
        let err = do_security_code_2fa(
            &mut stream,
            deadline,
            Some(&code_provider_that_never_answers()), None)
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "got {err}");
        assert!(err.to_string().contains("security code rejected (FAILED)"), "got {err}");
        assert!(stream.written.is_empty(), "nothing should have been sent");
    }
}

#[test]
fn security_code_gate_reports_a_rejection_delivered_as_auth_finish() {
    // The same rejection can arrive on 771 instead of 774.
    let mut stream = RepliesAfterWrite::new(frame_xyz(&xyz::xyz_build(
        xyz::XYZ_MSG_TOKEN_AUTH, 5, "", &["FAILED"],
    )));
    let err = do_security_code_2fa(
        &mut stream,
        far_future_deadline(),
        Some(&code_provider_returning("123456")), None)
    .unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    assert!(!err.to_string().contains("timed out"), "got {err}");
}

#[test]
fn security_code_gate_never_puts_the_code_in_an_error_message() {
    // The status slot is next to the one the code was sent in, and the
    // reply is server-controlled — an echo must not reach the log.
    let mut stream = RepliesAfterWrite::new(security_code_result(&["", "123456", "", ""]));
    let err = do_security_code_2fa(
        &mut stream,
        far_future_deadline(),
        Some(&code_provider_returning("123456")), None)
    .unwrap_err();
    assert!(!err.to_string().contains("123456"), "code leaked into: {err}");
}

#[test]
fn security_code_gate_accepts_passed_before_a_code_is_sent() {
    let mut stream = ScriptedStream::new(frame_xyz(&xyz::xyz_build(
        xyz::XYZ_MSG_TOKEN_AUTH, 5, "", &["PASSED"],
    )));
    let outcome = do_security_code_2fa(
        &mut stream,
        far_future_deadline(),
        Some(&code_provider_that_never_answers()), None)
    .expect("the venue's PASSED must be accepted before a code is sent");
    assert!(matches!(outcome, IbKeyOutcome::Approved { .. }));
    assert!(stream.written.is_empty(), "nothing should have been sent");
}

#[test]
fn security_code_gate_accepts_a_verdict_whose_read_began_before_the_code() {
    let ready = Arc::new(AtomicBool::new(false));
    let signal = ready.clone();
    let provider: CodeProvider = Arc::new(move |_| {
        signal.store(true, Ordering::SeqCst);
        Ok("123456".to_string())
    });
    let mut stream = VerdictAfterCodeReady {
        incoming: frame_xyz(&xyz::xyz_build(xyz::XYZ_MSG_TOKEN_AUTH, 5, "", &["PASSED"])),
        pos: 0,
        ready,
        written: Vec::new(),
    };
    let outcome = do_security_code_2fa(&mut stream, far_future_deadline(), Some(&provider), None)
        .expect("the venue's PASSED must be accepted before a code is sent");
    assert!(matches!(outcome, IbKeyOutcome::Approved { .. }));
    assert!(stream.written.is_empty(), "nothing should have been sent");
}

#[test]
fn security_code_gate_reports_a_774_rejection_whose_read_began_before_the_code() {
    let ready = Arc::new(AtomicBool::new(false));
    let signal = ready.clone();
    let provider: CodeProvider = Arc::new(move |_| {
        signal.store(true, Ordering::SeqCst);
        Ok("123456".to_string())
    });
    let mut stream = VerdictAfterCodeReady {
        incoming: security_code_result(&["", "", "", "FAILED"]),
        pos: 0,
        ready,
        written: Vec::new(),
    };
    let err = do_security_code_2fa(&mut stream, far_future_deadline(), Some(&provider), None)
        .expect_err("the venue's FAILED must reject the login before a code is sent");
    assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    assert!(err.to_string().contains("security code rejected (FAILED)"), "got {err}");
    assert!(stream.written.is_empty(), "nothing should have been sent");
}

/// A keepalive coalesced with the verdict still needs its own answer.
#[test]
fn security_code_gate_accepts_a_verdict_after_a_keepalive_before_the_code() {
    let ready = Arc::new(AtomicBool::new(false));
    let signal = ready.clone();
    let provider: CodeProvider = Arc::new(move |_| {
        signal.store(true, Ordering::SeqCst);
        Ok("123456".to_string())
    });
    let mut incoming = ns::ns_build(NS_VERSION, NS_TEST_REQUEST, &["20260730-01:02:03"], "MISC");
    incoming.extend_from_slice(&security_code_result(&["", "", "", "PASSED"]));
    let mut stream = VerdictAfterCodeReady { incoming, pos: 0, ready, written: Vec::new() };

    let outcome = do_security_code_2fa(&mut stream, far_future_deadline(), Some(&provider), None)
        .expect("the venue's PASSED must be accepted before a code is sent");
    assert!(matches!(outcome, IbKeyOutcome::Approved { .. }));
    let heartbeat = ns_build_heart_beat(NS_VERSION, "20260730-01:02:03");
    assert_eq!(
        stream.written, heartbeat,
        "the gate must answer the probe before accepting the verdict",
    );
}

#[test]
fn security_code_gate_surfaces_an_unexpected_774_code_before_a_code_is_sent() {
    let mut stream = ScriptedStream::new(frame_xyz(&xyz::xyz_build(
        xyz::XYZ_MSG_SECURITY_CODE, 4, "", &["whatever"],
        )));
    let err = do_security_code_2fa(
        &mut stream,
        far_future_deadline(),
        Some(&code_provider_that_never_answers()), None)
    .unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    assert!(err.to_string().contains("code 4"), "got {err}");
    assert!(stream.written.is_empty(), "nothing should have been sent");
}

#[test]
fn security_code_gate_survives_an_interrupted_read() {
    // Every other reader in this file goes through `read_exact`, which
    // retries a signal. The gate's raw read is the only one that would
    // die on it, taking a live login and the operator's code with it.
    struct InterruptsOnce {
        fired: bool,
        reply: Vec<u8>,
        pos: usize,
        written: Vec<u8>,
    }
    impl io::Read for InterruptsOnce {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if !self.fired {
                self.fired = true;
                return Err(io::Error::new(io::ErrorKind::Interrupted, "signal"));
            }
            if self.written.is_empty() {
                return Err(io::Error::new(io::ErrorKind::WouldBlock, "quiet"));
            }
            let remaining = self.reply.len().saturating_sub(self.pos);
            if remaining == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "scripted EOF"));
            }
            let n = remaining.min(buf.len());
            buf[..n].copy_from_slice(&self.reply[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }
    impl io::Write for InterruptsOnce {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.written.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> { Ok(()) }
    }

    let mut stream = InterruptsOnce {
        fired: false,
        reply: security_code_result(&["", "", "", "PASSED"]),
        pos: 0,
        written: Vec::new(),
    };
    let outcome = do_security_code_2fa(
        &mut stream,
        far_future_deadline(),
        Some(&code_provider_returning("123456")), None)
    .expect("a signal must not end the login");
    assert!(matches!(outcome, IbKeyOutcome::Approved { .. }));
}

#[test]
fn security_code_gate_rejects_an_empty_code_before_sending_it() {
    // A valid frame with an empty token slot is a guaranteed rejection,
    // and one wrong code ends the attempt.
    let mut stream = RepliesAfterWrite::new(Vec::new());
    let provider: CodeProvider = Arc::new(|_| Ok("  ".to_string()));
    let err = do_security_code_2fa(&mut stream, far_future_deadline(), Some(&provider), None)
        .unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    assert!(stream.written.is_empty(), "an empty code must not reach the wire");
}

#[test]
fn security_code_gate_treats_a_clean_close_as_a_close() {
    // A half-closed socket returns Ok(0) immediately and forever. Treating
    // that as "nothing yet" spins hot until the deadline instead of
    // reporting the close — the read timeout never fires to break it up.
    struct CleanClose {
        written: Vec<u8>,
    }
    impl io::Read for CleanClose {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            Ok(0)
        }
    }
    impl io::Write for CleanClose {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.written.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> { Ok(()) }
    }

    let mut stream = CleanClose { written: Vec::new() };
    let began = std::time::Instant::now();
    let err = do_security_code_2fa(
        &mut stream,
        far_future_deadline(),
        Some(&code_provider_returning("123456")), None)
    .unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::ConnectionAborted);
    assert!(began.elapsed() < std::time::Duration::from_secs(5), "must not spin on a closed socket");
}

#[test]
fn security_code_gate_leaves_the_next_frame_on_the_stream() {
    // The server can coalesce its reply with what follows. The gate reads
    // from the raw stream and the caller keeps reading it afterwards, so a
    // read that crosses the frame boundary either swallows the next frame
    // whole or strands the stream mid-frame — and on this path each retry
    // costs the operator a fresh code.
    let trailing = ns::ns_build(NS_VERSION, 24, &["next"], "MISC");
    let mut reply = security_code_result(&["", "", "", "PASSED"]);
    reply.extend_from_slice(&trailing);
    let mut stream = RepliesAfterWrite::new(reply);
    do_security_code_2fa(
        &mut stream,
        far_future_deadline(),
        Some(&code_provider_returning("123456")), None)
    .expect("PASSED must be accepted");
    let (payload, _) = ns::ns_recv(&mut stream, far_future_deadline())
        .expect("the next frame must survive the gate");
    assert_eq!(
        payload,
        trailing[8..],
        "the frame after the gate's must be readable, whole and unconsumed"
    );
}

/// A frame larger than one read buffer has to reassemble across reads
/// without over-running its own boundary. Every shipped test uses a frame
/// that fits in a single 4KiB read, which is the one size where the read
/// clamp does nothing — so the clamp was unpinned.
#[test]
fn security_code_gate_reassembles_a_frame_larger_than_one_read() {
    let filler = "x".repeat(5000);
    let trailing = ns::ns_build(NS_VERSION, 24, &["next"], "MISC");
    let mut reply = security_code_result(&["", &filler, "", "PASSED"]);
    assert!(reply.len() > 4096, "the frame must cross a read boundary: {}", reply.len());
    reply.extend_from_slice(&trailing);

    let mut stream = RepliesAfterWrite::new(reply);
    do_security_code_2fa(
        &mut stream,
        far_future_deadline(),
        Some(&code_provider_returning("123456")), None)
    .expect("a large PASSED frame must still be accepted");

    let (payload, _) = ns::ns_recv(&mut stream, far_future_deadline())
        .expect("the next frame must survive");
    assert_eq!(
        payload, trailing[8..],
        "reassembly must stop at its own frame boundary, not read past it",
    );
}

/// A code is trimmed on the way out, not merely when checking for empty:
/// surrounding whitespace reaches the server as part of the code and burns
/// the one attempt the operator gets.
#[test]
fn security_code_gate_trims_the_code_it_sends() {
    let mut stream = RepliesAfterWrite::new(security_code_result(&["", "", "", "PASSED"]));
    do_security_code_2fa(
        &mut stream,
        far_future_deadline(),
        Some(&code_provider_returning("  123456  ")), None)
    .expect("PASSED must be accepted");

    let sent = String::from_utf8_lossy(&stream.written).to_string();
    assert!(sent.contains("123456"), "the code is sent: {sent:?}");
    assert!(
        !sent.contains("  123456") && !sent.contains("123456  "),
        "and without the surrounding whitespace: {sent:?}",
    );
}

#[test]
fn security_code_gate_accepts_a_774_verdict_before_a_code_is_sent() {
    let mut stream = ScriptedStream::new(security_code_result(&["", "", "", "PASSED"]));
    let outcome = do_security_code_2fa(
        &mut stream,
        far_future_deadline(),
        Some(&code_provider_that_never_answers()), None)
    .expect("the venue's PASSED must be accepted before a code is sent");
    assert!(matches!(outcome, IbKeyOutcome::Approved { .. }));
    assert!(stream.written.is_empty(), "nothing should have been sent");
}

#[test]
fn security_code_gate_surfaces_an_ns_error_frame() {
    let mut stream = RepliesAfterWrite::new(
        ns::ns_build(NS_VERSION, NS_ERROR_RESPONSE, &["denied"], "MISC"),
    );
    let err = do_security_code_2fa(
        &mut stream,
        far_future_deadline(),
        Some(&code_provider_returning("123456")), None)
    .unwrap_err();
    // The venue answering with an error frame is a refusal, not a gate that
    // went quiet: raised with no kind on it the retry ladder read it as a
    // transport failure and went round again, which re-asks for a second
    // factor the venue has already refused.
    assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    assert!(!err.to_string().contains("timed out"), "got {err}");
}

#[test]
fn security_code_gate_surfaces_an_unexpected_774_code() {
    // Any 774 that is not the result carries a failure the caller must
    // rather than wait out.
    let mut stream = RepliesAfterWrite::new(frame_xyz(&xyz::xyz_build(
        xyz::XYZ_MSG_SECURITY_CODE, 4, "", &["whatever"],
        )));
    let err = do_security_code_2fa(
        &mut stream,
        far_future_deadline(),
        Some(&code_provider_returning("123456")), None)
    .unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    assert!(err.to_string().contains("code 4"), "got {err}");
}

#[test]
fn security_code_gate_rejects_an_absurd_frame_length_instead_of_buffering() {
    // A corrupt header must not be believed: waiting for a 4 GiB tail
    // means growing the buffer until the deadline or the allocator gives
    // out. Header only, no payload — a reader that trusts the length will
    // sit on it.
    let mut reply = ns::NS_MAGIC.to_vec();
    reply.extend_from_slice(&u32::MAX.to_be_bytes());
    let mut stream = RepliesAfterWrite::new(reply);
    let err = do_security_code_2fa(
        &mut stream,
        far_future_deadline(),
        Some(&code_provider_returning("123456")), None)
    .unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidData);
}

#[test]
fn security_code_gate_requires_a_provider() {
    // These accounts have no push to fall back to. Raised as a kind the
    // retry ladder reads as one it does not come back from: the lack is the
    // same on every attempt, and retrying it ran the same unfinished
    // handshake without end.
    let mut stream = ScriptedStream::new(Vec::new());
    let err = do_security_code_2fa(&mut stream, far_future_deadline(), None, None).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::Unsupported);
    let reason = crate::reliability::retry::DisconnectReason::from_error(&err);
    assert!(
        reason.is_terminal(),
        "a second factor nobody is configured to supply retried forever",
    );
}

#[test]
fn security_code_gate_does_not_send_a_code_after_the_deadline_passes() {
    // The read can span the deadline. A login that is about to be
    // abandoned must not put a code on the wire on its way out — the code
    // is spent either way, and the operator has to fetch another.
    struct SlowRead {
        written: Vec<u8>,
    }
    impl io::Read for SlowRead {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            std::thread::sleep(std::time::Duration::from_millis(60));
            Err(io::Error::new(io::ErrorKind::TimedOut, "poll timeout"))
        }
    }
    impl io::Write for SlowRead {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.written.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> { Ok(()) }
    }

    let mut stream = SlowRead { written: Vec::new() };
    let err = do_security_code_2fa(
        &mut stream,
        std::time::Instant::now() + std::time::Duration::from_millis(20),
        Some(&code_provider_returning("123456")), None)
    .unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    assert!(stream.written.is_empty(), "no code may go out after the deadline");
}

#[test]
fn security_code_gate_honours_the_deadline() {
    let mut stream = RepliesAfterWrite::new(Vec::new());
    let began = std::time::Instant::now();
    let err = do_security_code_2fa(
        &mut stream,
        std::time::Instant::now(),
        Some(&code_provider_that_never_answers()), None)
    .unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    // An expired deadline has to be caught before the loop does anything,
    // not eventually. Without the check at the top this still ends in
    // `TimedOut`, but only once the provider gives up — so the kind alone
    // proves nothing and the elapsed time is what pins it.
    assert!(
        began.elapsed() < std::time::Duration::from_secs(5),
        "an expired deadline must return at once, took {:?}",
        began.elapsed(),
    );
}

fn far_future_deadline() -> std::time::Instant {
    std::time::Instant::now() + std::time::Duration::from_secs(60)
}

#[test]
fn ib_key_2fa_skipped_when_server_passes_immediately() {
    // The server replies AUTH_FINISH(771) state=5 PASSED right after the init —
    // no SWCR_TOKEN(state=2) preceded it, so this is the no-2FA fast path.
    let auth_finish = xyz::xyz_build(xyz::XYZ_MSG_TOKEN_AUTH, 5, "user", &["PASSED"]);
        let mut stream = ScriptedStream::new(frame_xyz(&auth_finish));
    let outcome = do_ib_key_2fa(&mut stream, "2a", far_future_deadline(), None, None).unwrap();
    assert_eq!(outcome, IbKeyOutcome::Skipped { unread: None });


    // The SWCR_TOKEN init carries the tokenSubType.
    let written_payload = &stream.written[8..]; // skip NS frame header
    let (msg_id, _, state, fields) = xyz::xyz_parse_response(written_payload).unwrap();
    assert_eq!(msg_id, xyz::XYZ_MSG_SWCR_TOKEN);
    assert_eq!(state, 1);
    // Per the canonical layout, tokenSubType is the
    // last (and only non-empty) string field; preceding slots are empty.
    assert_eq!(fields.last().map(|s| s.as_str()), Some("2a"),
        "tokenSubType must be the last field; got {fields:?}");
}

    /// A quiet socket is what waiting for a phone tap looks like.
///
/// The gate polls, so most reads come back with nothing. Treating that as
/// a failure ends the login while the operator is still deciding; treating
/// it as a reason to block for ever leaves the client's own deadline
/// unreachable when a server stops talking without closing. Neither: the
/// wait goes round again, and the deadline is what ends it.
#[test]
fn a_quiet_socket_is_waited_through_and_the_deadline_ends_it() {
    /// Answers every read with a timeout, as a polled socket does while
    /// nobody has tapped anything.
    struct AlwaysQuiet {
        reads: std::cell::Cell<u32>,
        written: Vec<u8>,
    }
    impl io::Read for AlwaysQuiet {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            self.reads.set(self.reads.get() + 1);
            Err(io::Error::new(io::ErrorKind::WouldBlock, "nothing yet"))
        }
    }
    impl io::Write for AlwaysQuiet {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.written.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> { Ok(()) }
    }

    let mut stream = AlwaysQuiet { reads: std::cell::Cell::new(0), written: Vec::new() };
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);

    let started = std::time::Instant::now();
    let outcome = do_ib_key_2fa(&mut stream, "2a", deadline, None, None);

    let why = outcome.expect_err("a wait nobody answered ends as a timeout");
    assert_eq!(why.kind(), io::ErrorKind::TimedOut, "{why}");
    assert!(
        stream.reads.get() > 1,
        "the wait went round rather than giving up on the first quiet read",
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "and it ended on its own deadline",
    );
}

/// The wait is as long as a person takes to reach a phone — eighteen
/// minutes by default. A client that stops, or whose recovery budget is
/// spent, cannot be made to wait it out: the flag it set ends the gate on
/// its next pass, not an answer from the venue.
#[test]
fn a_second_factor_wait_ends_when_the_client_takes_it_back() {
    /// Answers every read with a timeout, as a polled socket does while the
    /// venue says nothing.
    struct AlwaysQuiet {
        written: Vec<u8>,
    }
    impl io::Read for AlwaysQuiet {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::WouldBlock, "nothing yet"))
        }
    }
    impl io::Write for AlwaysQuiet {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.written.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> { Ok(()) }
    }

    let cancel = std::sync::atomic::AtomicBool::new(true);
    let mut stream = AlwaysQuiet { written: Vec::new() };
    // Far away on purpose: the flag must end the wait, not the deadline.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);

    let started = std::time::Instant::now();
    let err = do_ib_key_2fa(&mut stream, "2a", deadline, None, Some(&cancel))
        .expect_err("a wait the client took back is not one the venue ended");
    assert_eq!(err.kind(), io::ErrorKind::Interrupted, "{err}");
    assert!(err.to_string().contains("cancelled"), "{err}");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "the flag ended the wait at once, not the deadline: {:?}",
        started.elapsed(),
    );
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

    let outcome = do_ib_key_2fa(&mut stream, "2a", far_future_deadline(), None, None).unwrap();
    match outcome {
        IbKeyOutcome::Approved { approval_url, session_id, soft_token_hex } => {
            assert_eq!(approval_url, "https://www.example.com/seamless?S=YWJjZA==");
            assert_eq!(session_id, "580 820");
            let _ = soft_token_hex;
        }
        other => panic!("expected Approved, got {other:?}"),
    }
}

#[test]
fn ib_key_gate_reassembles_a_reply_split_across_reads() {
    // The gate polls under a short socket timeout, so a frame can arrive
    // across polls with timeouts in between — byte-at-a-time delivery is the
    // worst case. A reader that let a timeout discard the half it held
    // resumed mid-frame, and an approval already given was spent on a login
    // that died there.
    let challenge = xyz::xyz_build(xyz::XYZ_MSG_SWCR_TOKEN, 2, "user", &[
            "e7429fde5b4c26f81fff956be6749908a8653558e7429fde5b4c26f81fff956b",
        "580 820",
        "https://www.example.com/seamless?S=YWJjZA==",
    ]);
    let auth_finish = xyz::xyz_build(xyz::XYZ_MSG_TOKEN_AUTH, 5, "user", &["PASSED"]);
    let mut incoming = frame_xyz(&challenge);
    incoming.extend_from_slice(&frame_xyz(&auth_finish));
    let mut stream = RepliesAfterWrite::chunked(incoming, 1);
    let outcome = do_ib_key_2fa(&mut stream, "2a", far_future_deadline(), None, None)
        .expect("a frame split across reads must still be read");
    assert!(matches!(outcome, IbKeyOutcome::Approved { .. }), "{outcome:?}");
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

    let outcome = do_ib_key_2fa(&mut stream, "2a", far_future_deadline(), None, None).unwrap();
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
        if let Some((_, msg_type, fields)) = ns::ns_parse(payload)
            && msg_type == ns::NS_HEART_BEAT {
                assert_eq!(fields, vec!["20260430-22:58:25".to_string()]);
                saw_heartbeat = true;
            }
        offset += 8 + len;
    }
    assert!(saw_heartbeat, "client must echo the test-request timestamp in a HEART_BEAT");
}

#[test]
fn ib_key_gate_echoes_the_timestamp_from_its_position_or_nothing() {
    // A probe with a blank timestamp slot and a populated later field is
    // echoed nothing at that position: taking the first non-empty field
    // answers with a value the probe did not ask for.
    let test_req = ns::ns_build(NS_VERSION, ns::NS_TEST_REQUEST, &["", "1750000000"], "MISC");
    let auth_finish = xyz::xyz_build(xyz::XYZ_MSG_TOKEN_AUTH, 5, "user", &["PASSED"]);
    let mut incoming = test_req;
    incoming.extend_from_slice(&frame_xyz(&auth_finish));
    let mut stream = ScriptedStream::new(incoming);

    let outcome = do_ib_key_2fa(&mut stream, "2a", far_future_deadline(), None, None).unwrap();
    assert!(matches!(outcome, IbKeyOutcome::Skipped { .. }), "{outcome:?}");

    let heartbeat = ns_build_heart_beat(NS_VERSION, "");
    assert!(
        stream.written.windows(heartbeat.len()).any(|w| w == heartbeat),
        "the heartbeat must echo the timestamp's position — here, nothing",
    );
    assert!(
        !stream.written.windows(10).any(|w| w == b"1750000000"),
        "the heartbeat echoed a field the probe did not ask for",
    );
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
    let err = do_ib_key_2fa(&mut stream, "2a", far_future_deadline(), None, None).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::ConnectionAborted);
    assert!(err.to_string().contains("18 min server-side deadline"),
        "expected the long-deadline message; got {err}");
}

#[test]
fn ib_key_2fa_socket_close_before_challenge_says_likely_rejection() {
    // The server closes the socket immediately after the SWCR_TOKEN init —
    // no challenge ever arrived. The diagnostic message must NOT blame
    // the 18 min deadline (which would mislead users into "approve faster"
        // when the real fix is "your account doesn't use IBKey").
    let mut stream = ScriptedStream::new(Vec::new());
    let err = do_ib_key_2fa(&mut stream, "2a", far_future_deadline(), None, None).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::ConnectionAborted);
    let msg = err.to_string();
    assert!(msg.contains("before issuing a challenge"),
        "expected rejection-style message; got {msg}");
    assert!(!msg.contains("18 min"),
        "must not mention the 18 min deadline when no challenge was seen; got {msg}");
}

#[test]
fn ib_key_2fa_rejected_when_passed_string_is_failed() {
    // Server replies AUTH_FINISH state=5 but payload says FAILED — denial.
    let auth_finish = xyz::xyz_build(xyz::XYZ_MSG_TOKEN_AUTH, 5, "user", &["FAILED"]);
        let mut stream = ScriptedStream::new(frame_xyz(&auth_finish));
    let err = do_ib_key_2fa(&mut stream, "2a", far_future_deadline(), None, None).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    assert!(err.to_string().contains("rejected"));
}

// ── do_ib_key_2fa — Challenge/Response ───────────────

#[test]
fn ib_key_2fa_cr_submits_code_then_passes_on_auth_finish_state_3() {
    // Replay of the recorded success path. Captured fixtures:
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

    let outcome = do_ib_key_2fa(&mut stream, "2a", far_future_deadline(), Some(&provider), None).unwrap();
    match outcome {
        IbKeyOutcome::Approved { approval_url, session_id, .. } => {
            assert_eq!(session_id, RUN_A_SESSION_ID);
            assert_eq!(approval_url, RUN_A_AVTH_URL);
        }
        other => panic!("expected Approved, got {other:?}"),
    }

    // Provider must have received the parsed display_id + avth_url, and be
    // told which factor it is answering: the two want different codes from
    // different apps, and the shipped example branches on it. Nothing
    // pinned it on this path, so labelling it as the authenticator went
    // unnoticed.
    let seen = seen_challenge.lock().unwrap().clone();
    assert_eq!(seen.factor, SecondFactor::IbKeyChallengeResponse);
    assert_eq!(seen.display_id, RUN_A_SESSION_ID);
    assert_eq!(seen.avth_url, RUN_A_AVTH_URL);

    // The state=3 frame must be byte-for-byte the recorded 40-byte capture.
    assert!(stream.written.windows(submission.len()).any(|f| f == submission),
        "state=3 submission must byte-match the recorded capture");
    // Nothing was waited on: an unattended login submits before any probe.
    let heartbeat = ns_build_heart_beat(NS_VERSION, PROBE_TS);
    assert!(!stream.written.windows(heartbeat.len()).any(|f| f == heartbeat),
        "an immediate provider must not have to wait for a probe");
}

/// A provider that waits on a human outlives the fast-path grace, so the
/// code lands on the probe-driven path. The gate must answer the server's
/// keepalives throughout: called inline, it answers none of them, which is
/// what cost the C/R path its connection.
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

    let outcome = do_ib_key_2fa(&mut stream, "2a", far_future_deadline(), Some(&provider), None).unwrap();
    assert!(matches!(outcome, IbKeyOutcome::Approved { .. }));

    let sent = &stream.written;
    let heartbeat = ns_build_heart_beat(NS_VERSION, PROBE_TS);
    let hb_at = sent.windows(heartbeat.len()).position(|f| f == heartbeat)
        .expect("heartbeat reply must be sent while the provider is running");
    let code_at = sent.windows(submission.len()).position(|f| f == submission)
        .expect("state=3 submission must reach the wire");
    assert!(hb_at < code_at, "heartbeat must be answered before the code is submitted");
}

/// A provider that outlives the fast-path grace is polled between inbound
/// frames — but it must not take an inbound frame to send it. A venue that
/// keeps the socket quiet while the operator decides never says another word,
/// and the code sat in its channel until this wait's own deadline.
#[test]
fn a_slow_provider_s_code_is_sent_on_a_quiet_socket() {
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
    // Answers the code, and nothing else: no probes, no keepalives. Until
    // the submission arrives there is only the challenge and then silence,
    // so a gate that waited for inbound traffic before reading the provider
    // never sent the code at all.
    let mut stream = RepliesAfterWrite::with_preface(
        frame_xyz(&challenge), submission.clone(), tail,
    );

    let provider: CodeProvider = std::sync::Arc::new(|_| {
        std::thread::sleep(IB_KEY_PROVIDER_FAST_PATH_GRACE + std::time::Duration::from_millis(200));
        Ok(CODE.to_string())
    });

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
    let outcome = do_ib_key_2fa(&mut stream, "2a", deadline, Some(&provider), None)
        .expect("the code reaches the wire on a quiet socket");
    assert!(matches!(outcome, IbKeyOutcome::Approved { .. }), "{outcome:?}");
    assert!(
        stream.written.windows(submission.len()).any(|f| f == submission),
        "state=3 submission must reach the wire without inbound traffic first",
    );
}

#[test]
fn ib_key_2fa_cr_code_rejected_on_state_4_failed() {
    // Server: state=2 → (client submits a wrong code) → state=4 FAILED. The
    // server tears the socket down after, with no AUTH_FINISH on the wire.
    // Client must surface PermissionDenied on FAILED, without waiting for
    // further frames. FAILED is not sent until a code has been submitted, so
    // this also pins that the rejection follows a real submission.
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
    let err = do_ib_key_2fa(&mut stream, "2a", far_future_deadline(), Some(&provider), None).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    assert!(err.to_string().contains("C/R code rejected"),
        "expected C/R rejection message; got {err}");
    assert!(stream.written.windows(submission.len()).any(|f| f == submission),
        "the rejected code must have been submitted as state=3");
}

/// A code is single use. If the deadline passes while the provider is being
/// consulted, the login is already lost — submitting the code spends it on
/// a connection the next loop abandons, and the operator's next attempt
/// needs a fresh one. Both routes to the submission are pinned: the inline
/// grace period, which can straddle the deadline, and the polled path.
#[test]
fn ib_key_2fa_cr_does_not_burn_a_code_after_the_deadline() {
    const CODE: &str = "12345678";
    let challenge = xyz::xyz_build(xyz::XYZ_MSG_SWCR_TOKEN, 2, "user", &[
            "e7429fde5b4c26f81fff956be6749908a8653558e7429fde5b4c26f81fff956b",
        "399 830",
        "https://x.example/u",
    ]);
    let submission = frame_xyz(&xyz::xyz_build_swcr_token_code_submission(CODE));

    // Fast path: the provider resolves inside the grace period, but the
    // deadline has already passed by the time it does.
    let mut stream = ProbingGateway::new(
        frame_xyz(&challenge), submission.clone(), Vec::new(),
    );
    let provider: CodeProvider = std::sync::Arc::new(|_| {
        std::thread::sleep(std::time::Duration::from_millis(60));
        Ok(CODE.to_string())
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(20);
    let err = do_ib_key_2fa(&mut stream, "2a", deadline, Some(&provider), None).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::TimedOut, "got {err}");
    assert!(
        !stream.written.windows(submission.len()).any(|f| f == submission),
        "no code may reach the wire once the login is lost",
    );

    // A provider that outlasts the inline grace period. This pins that no
    // code reaches the wire by the polled route once the deadline has
    // passed — but it does not exercise the guard on that route: the loop
    // aborts on the deadline before the poll ever returns the code, so
    // deleting that guard leaves this passing. Reaching it needs the
    // deadline to expire between the top-of-loop check and the poll within
    // one iteration, which is a race rather than a sequence.
    let mut stream = ProbingGateway::new(
        frame_xyz(&challenge), submission.clone(), Vec::new(),
    );
    let provider: CodeProvider = std::sync::Arc::new(|_| {
        std::thread::sleep(IB_KEY_PROVIDER_FAST_PATH_GRACE + std::time::Duration::from_millis(200));
        Ok(CODE.to_string())
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(20);
    let err = do_ib_key_2fa(&mut stream, "2a", deadline, Some(&provider), None).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::TimedOut, "got {err}");
    assert!(
        !stream.written.windows(submission.len()).any(|f| f == submission),
        "a code that arrives after the deadline is not sent either",
        );

    // And the same code does reach the wire when the deadline holds, so the
    // assertions above are not passing for want of a working path.
    let mut stream = ProbingGateway::new(
        frame_xyz(&challenge), submission.clone(),
        frame_xyz(&xyz::xyz_build(xyz::XYZ_MSG_SWCR_TOKEN, 4, "user", &["PASSED"])),
        );
    let provider: CodeProvider = std::sync::Arc::new(|_| Ok(CODE.to_string()));
    let _ = do_ib_key_2fa(&mut stream, "2a", far_future_deadline(), Some(&provider), None);
    assert!(
        stream.written.windows(submission.len()).any(|f| f == submission),
        "the positive control: a live deadline still submits",
    );
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
    let err = do_ib_key_2fa(&mut stream, "2a", far_future_deadline(), Some(&provider), None).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::Interrupted);
}

/// A SWCR_TOKEN state the gate does not model is the server refusing in an
/// unrecognised shape. Looping on it answers the refusal with keepalives
/// until the client deadline — 18 min by default — and then reports
/// TimedOut, so the operator retries against a server that already said no.
#[test]
fn ib_key_2fa_rejects_an_unexpected_swcr_token_state() {
    const ECHOED_CODE: &str = "02226534";
    let challenge = xyz::xyz_build(xyz::XYZ_MSG_SWCR_TOKEN, 2, "user", &[
            "e7429fde5b4c26f81fff956be6749908a8653558e7429fde5b4c26f81fff956b",
        "399 830",
        "https://x.example/u",
    ]);
    // State 3 is the slot the code goes out in, so an echo of one is the
    // frame whose fields must not reach the log or the error text.
    let echo = xyz::xyz_build(xyz::XYZ_MSG_SWCR_TOKEN, 3, "user", &[ECHOED_CODE]);
        let mut head = frame_xyz(&challenge);
    head.extend_from_slice(&frame_xyz(&echo));
    // Nothing is ever submitted, so this gateway probes until the deadline:
    // only a terminating arm ends the attempt sooner.
    let mut stream = ProbingGateway::new(head, Vec::new(), Vec::new());

    let began = std::time::Instant::now();
    let deadline = began + std::time::Duration::from_secs(1);
    let err = do_ib_key_2fa(&mut stream, "2a", deadline, None, None).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "got {err}");
    assert!(
        began.elapsed() < std::time::Duration::from_millis(500),
        "the refusal must end the attempt, not the deadline; took {:?}",
        began.elapsed(),
    );
    assert!(!err.to_string().contains(ECHOED_CODE),
        "the frame's fields must not reach the error text; got {err}");
}

/// Both rejection texts carry a field the server chose, next to the slot
/// the code was sent in. Echo only a status the protocol defines.
#[test]
fn ib_key_2fa_rejections_do_not_echo_server_supplied_text() {
    const SERVER_TEXT: &str = "02226534-not-a-status";

    let state4 = xyz::xyz_build(xyz::XYZ_MSG_SWCR_TOKEN, 4, "user", &[SERVER_TEXT]);
        let mut stream = ScriptedStream::new(frame_xyz(&state4));
    let err = do_ib_key_2fa(&mut stream, "2a", far_future_deadline(), None, None).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "got {err}");
    assert!(!err.to_string().contains(SERVER_TEXT),
        "state=4 rejection must not echo the server's field; got {err}");

    let auth_finish = xyz::xyz_build(xyz::XYZ_MSG_TOKEN_AUTH, 5, "user", &[SERVER_TEXT]);
        let mut stream = ScriptedStream::new(frame_xyz(&auth_finish));
    let err = do_ib_key_2fa(&mut stream, "2a", far_future_deadline(), None, None).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "got {err}");
    assert!(!err.to_string().contains(SERVER_TEXT),
        "AUTH_FINISH rejection must not echo the server's field; got {err}");
}

/// A group the venue states has to be one this arithmetic can use.
/// The group is the venue's own, or the logon does not happen.
///
/// A peer names the modulus and generator before it has proved anything. Name
/// a small one and the shared secret collapses to a value anybody can compute,
/// which lets a peer holding no verifier produce the proof that would catch
/// it — so the ground the logon is checked on is not the peer's to choose.
#[test]
fn an_srp_group_that_is_not_this_venue_s_stops_the_logon() {
    // What the venue states, which is what this client holds.
    let venue = vec![
        crate::auth::srp::SRP_VENUE_N_STR.to_string(),
        format!("{:x}", crate::auth::srp::SRP_VENUE_G),
    ];
    let (n, g) = stated_group(&venue).expect("the venue's own group is the one to use");
    assert_eq!(n, crate::auth::srp::srp_venue_n());
    assert_eq!(g, BigUint::from(crate::auth::srp::SRP_VENUE_G));

    for (why, stated) in [
        ("a modulus of three", vec!["3".to_string(), "2".to_string()]),
        ("a zero modulus", vec!["0".to_string(), "2".to_string()]),
        ("a modulus of one", vec!["1".to_string(), "2".to_string()]),
        ("another group entirely", vec!["1FFFF".to_string(), "5".to_string()]),
        (
            "this venue's modulus under another generator",
            vec![crate::auth::srp::SRP_VENUE_N_STR.to_string(), "5".to_string()],
        ),
        // Stating none is the same choice made by omission: whatever this
        // client fell back to would not be the group that was checked.
        ("nothing parseable", vec!["zz".to_string(), "2".to_string()]),
        ("only one field", vec!["1FFFF".to_string()]),
        ("no fields at all", vec![]),
    ] {
        assert!(
            stated_group(&stated).is_err(),
            "{why} is not ground to be checked on",
        );
    }
}

/// A server that answers the gate's init with the connect response hands the
/// gate a message meant for the loop behind it.
///
/// Logged and dropped, that message was gone: the loop cannot ask the socket
/// for it twice, so it waited out its whole deadline for something already
/// delivered and the login failed with no data start.
#[test]
fn a_connect_response_read_by_the_gate_is_handed_on() {
    let payload = format!("{NS_VERSION};{NS_CONNECT_RESPONSE};ok;");
    let mut stream = ScriptedStream::new(frame_xyz(payload.as_bytes()));
    let outcome = do_ib_key_2fa(&mut stream, "2a", far_future_deadline(), None, None).unwrap();
    match outcome {
        IbKeyOutcome::Skipped { unread: Some(raw) } => {
            assert_eq!(raw, payload.as_bytes(), "handed back as it arrived");
        }
        other => panic!("the gate kept a message it did not own: {other:?}"),
    }
}

/// The same for the sibling gate, which shares the outcome and had the same
/// catch-all.
#[test]
fn a_fix_start_read_by_the_security_code_gate_is_handed_on() {
    let payload = format!("{NS_VERSION};{NS_FIX_START};;");
    let mut stream = ScriptedStream::new(frame_xyz(payload.as_bytes()));
    let provider = code_provider_returning("123456");
    let outcome =
        do_security_code_2fa(&mut stream, far_future_deadline(), Some(&provider), None).unwrap();
    assert!(
        matches!(outcome, IbKeyOutcome::Skipped { unread: Some(ref raw) } if raw == payload.as_bytes()),
        "the gate kept a message it did not own: {outcome:?}",
    );
}

/// The venue numbers its error responses, and two of those numbers say wait
/// rather than no.
///
/// Read as a refusal, a site that is down or not yet serving classifies as
/// the credentials being wrong and the retry ladder stops there — so a
/// maintenance window ended an established session for good, where the venue
/// only asked for a wait and comes back on its own.
#[test]
fn a_site_that_is_down_is_not_the_credentials_being_refused() {
    let stated = |code: &str| {
        error_the_venue_stated("Auth error", ns::NS_ERROR_RESPONSE, &[code, "some words"])
    };

    for (code, why) in [("4", "the site is down"), ("5", "the site is not ready")] {
        let err = stated(code);
        assert_ne!(
            retry::DisconnectReason::from_error(&err).recovery(),
            retry::Recovery::Stop,
            "{why}: the venue asked for a wait, not for the session to end",
        );
        assert!(err.to_string().contains("some words"), "and it still says what the venue said");
    }

    // Every other number is the venue refusing what was sent, and the next
    // attempt would carry exactly the same credentials.
    for code in ["1", "10", "11", "12", "", "lockedout"] {
        assert_eq!(
            stated(code).kind(),
            std::io::ErrorKind::PermissionDenied,
            "error type {code:?} is a refusal",
        );
    }

    // A secure error is not numbered — its first field is the message — so a
    // message that happens to start with a 4 is not read as a site being down.
    assert_eq!(
        error_the_venue_stated("Auth error", NS_SECURE_ERROR, &["4", "words"]).kind(),
        std::io::ErrorKind::PermissionDenied,
    );
}
