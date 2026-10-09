use super::*;
use std::io::{Read, Write};

struct Wire {
    bytes: Vec<u8>,
    position: usize,
    output: Vec<u8>,
    timeout_after: Option<usize>,
}
impl Wire {
    fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            position: 0,
            output: Vec::new(),
            timeout_after: None,
        }
    }
}
impl Read for Wire {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.timeout_after == Some(self.position) {
            self.timeout_after = None;
            return Err(io::Error::from(io::ErrorKind::TimedOut));
        }
        let mut count = buffer
            .len()
            .min(self.bytes.len().saturating_sub(self.position));
        if let Some(after) = self.timeout_after {
            count = count.min(after.saturating_sub(self.position));
        }
        buffer[..count].copy_from_slice(&self.bytes[self.position..self.position + count]);
        self.position += count;
        Ok(count)
    }
}
impl Write for Wire {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.output.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn xyz_frame(message: u32, state: u32, fields: &[&str]) -> Vec<u8> {
    xyz::xyz_wrap(&xyz::xyz_build(message, state, "", fields))
}
fn passed() -> Vec<u8> {
    xyz_frame(xyz::XYZ_MSG_TOKEN_AUTH, 5, &["PASSED"])
}
fn run(wire: &mut Wire) -> io::Result<IbKeyOutcome> {
    do_ib_key_push_bounded(wire, "2a", None, 4096, 16384, 16)
}
#[test]
fn bounded_push_approval_heartbeat_partial_frame_and_post_auth_carry() {
    let mut bytes = xyz_frame(
        xyz::XYZ_MSG_SWCR_TOKEN,
        2,
        &["123456", "https://private.invalid/secret"],
    );
    bytes.extend(ns::ns_build(NS_VERSION, NS_TEST_REQUEST, &["42"], ""));
    bytes.extend(passed());
    let suffix = ns::ns_build(NS_VERSION, NS_CONNECT_RESPONSE, &["private-session"], "");
    bytes.extend_from_slice(&suffix);
    let mut wire = Wire::new(bytes);
    wire.timeout_after = Some(3);
    assert!(matches!(
        run(&mut wire).unwrap(),
        IbKeyOutcome::Approved { .. }
    ));
    assert_eq!(&wire.bytes[wire.position..], suffix);
    let mut written = io::Cursor::new(wire.output);
    let init = recv_msg_limited(&mut written, 4096).unwrap();
    assert!(matches!(
        init,
        RecvMsg::Xyz {
            msg_id: 775,
            state: 1,
            ..
        }
    ));
    match recv_msg_limited(&mut written, 4096).unwrap() {
        RecvMsg::Ns {
            msg_type: NS_HEART_BEAT,
            fields,
            ..
        } => assert!(fields.iter().any(|f| f == "42")),
        other => panic!("missing heartbeat: {other:?}"),
    }
    assert_eq!(written.position(), written.get_ref().len() as u64);
}
#[test]
fn bounded_push_broker_short_circuit_accepts_auth_finish_without_challenge() {
    assert_eq!(
        run(&mut Wire::new(passed())).unwrap(),
        IbKeyOutcome::Skipped
    );
}
#[test]
fn bounded_push_decline_and_unsupported_errors_never_echo_peer_material() {
    for (bytes, kind) in [
        (
            xyz_frame(771, 5, &["secret-session", "FAILED"]),
            io::ErrorKind::PermissionDenied,
        ),
        (
            xyz_frame(775, 4, &["private-approval-url", "FAILED"]),
            io::ErrorKind::PermissionDenied,
        ),
        (
            xyz_frame(999, 2, &["sensitive-peer-value"]),
            io::ErrorKind::Unsupported,
        ),
        (
            ns::ns_build(NS_VERSION, NS_ERROR_RESPONSE, &["secret-account"], ""),
            io::ErrorKind::PermissionDenied,
        ),
    ] {
        let error = run(&mut Wire::new(bytes)).unwrap_err();
        assert_eq!(error.kind(), kind);
        for secret in [
            "secret-session",
            "private-approval-url",
            "sensitive-peer-value",
            "secret-account",
        ] {
            assert!(!error.to_string().contains(secret));
        }
    }
}
#[test]
fn bounded_push_expired_deadline_rejects_before_writing_or_reading() {
    let mut wire = Wire::new(passed());
    let error = do_ib_key_push_bounded(
        &mut wire,
        "2a",
        Some(std::time::Instant::now()),
        4096,
        16384,
        16,
    )
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert!(wire.output.is_empty());
    assert_eq!(wire.position, 0);
}
#[test]
fn bounded_push_oversized_announcement_rejects_before_body_read() {
    let mut bytes = NS_MAGIC.to_vec();
    bytes.extend_from_slice(&5000u32.to_be_bytes());
    bytes.extend_from_slice(&[0; 32]);
    let mut wire = Wire::new(bytes);
    assert_eq!(
        run(&mut wire).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    assert_eq!(wire.position, 8);
}
#[test]
fn bounded_push_aggregate_and_frame_budgets_include_backup_notices() {
    let backup = ns::ns_build(NS_VERSION, NS_BACKUP_HOST, &["private-backup"], "");
    let mut wire = Wire::new([backup.clone(), backup.clone(), passed()].concat());
    assert_eq!(
        do_ib_key_push_bounded(&mut wire, "2a", None, 4096, 16384, 1)
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidData
    );
    assert_eq!(wire.position, backup.len());
    let mut wire = Wire::new([backup.clone(), backup.clone(), passed()].concat());
    assert_eq!(
        do_ib_key_push_bounded(&mut wire, "2a", None, 4096, backup.len() + 8, 16)
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidData
    );
    assert_eq!(wire.position, backup.len() + 8);
}
#[test]
fn bounded_push_oversized_token_is_rejected_before_big_integer_construction() {
    let token = "a".repeat(2049);
    let mut wire = Wire::new(xyz_frame(771, 5, &["PASSED", &token]));
    assert_eq!(
        run(&mut wire).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
}
#[test]
fn bounded_push_closed_or_malformed_peer_is_content_free() {
    for bytes in [Vec::new(), b"secret!!".to_vec()] {
        let error = run(&mut Wire::new(bytes)).unwrap_err();
        assert!(!error.to_string().contains("secret"));
        assert!(matches!(
            error.kind(),
            io::ErrorKind::ConnectionAborted | io::ErrorKind::InvalidData
        ));
    }
}
#[test]
fn bounded_auth_start_refuses_unparseable_second_factor_instead_of_bypassing_it() {
    let payload = ns::ns_build(50, NS_AUTH_START, &["1", "1", "not-a-factor", "0"], "");
    let error = recv_auth_start_limited(
        &mut io::Cursor::new(payload),
        &mut SecureChannel::new(),
        4096,
    )
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(!error.to_string().contains("not-a-factor"));
}
