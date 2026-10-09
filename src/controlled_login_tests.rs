use super::*;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{IpAddr, TcpListener};
use std::thread;
use std::time::Instant;

fn scope(timeout: Duration) -> ConnectionControl {
    ConnectionControl::new(
        timeout,
        BTreeMap::from([("localhost".into(), vec![IpAddr::from([127, 0, 0, 1])])]),
        "offline-hardware".into(),
    )
    .unwrap()
}
fn pair(control: &ConnectionControl) -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let client = control
        .connect_tcp("localhost", listener.local_addr().unwrap().port())
        .unwrap();
    let (peer, _) = listener.accept().unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    peer.set_write_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    (client, peer)
}
fn xyz_frame(state: u32, fields: &[&str]) -> Vec<u8> {
    crate::protocol::xyz::xyz_wrap(&crate::protocol::xyz::xyz_build(777, state, "", fields))
}
fn drain_request(peer: &mut TcpStream) {
    session::recv_msg_limited(peer, LOGIN_FRAME_BYTES).unwrap();
}
#[derive(Clone, Copy, Debug)]
enum Stage {
    AuthStart,
    SrpParameters,
    SrpChallenge,
    SrpResult,
    MobilePush,
    PostAuth,
    FixAck,
    InitDrain,
    FarmKey,
    FarmAck,
}

/// These fixtures enter the actual controlled stage primitives on registered
/// sockets, then interrupt partial frames or silent peers. They prove disposal
/// and joins for stage failures, not broker acceptance or an end-to-end happy login.
#[test]
fn controlled_login_stage_cancellation_joins_owned_login_workers_and_watchdog() {
    for stage in [
        Stage::AuthStart,
        Stage::SrpParameters,
        Stage::SrpChallenge,
        Stage::SrpResult,
        Stage::MobilePush,
        Stage::PostAuth,
        Stage::FixAck,
        Stage::InitDrain,
        Stage::FarmKey,
        Stage::FarmAck,
    ] {
        let control = scope(Duration::from_secs(5));
        let (client, mut peer) = pair(&control);
        let active = control.clone();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            let mut io = ControlledIo::new(client, Some(&active));
            started_tx.send(()).unwrap();
            let result: io::Result<()> = match stage {
                Stage::AuthStart => controlled_ccp_login_start(
                    &mut io,
                    &mut SecureChannel::new(),
                    b"38;521;offline;",
                )
                .map(|_| ()),
                Stage::SrpParameters | Stage::SrpChallenge | Stage::SrpResult => {
                    session::do_srp_bounded(
                        &mut io,
                        "offline",
                        "offline-password",
                        LOGIN_FRAME_BYTES,
                        8192,
                    )
                    .map(|_| ())
                }
                Stage::MobilePush => session::do_ib_key_push_bounded(
                    &mut LoginPollIo(&mut io),
                    "2a",
                    None,
                    LOGIN_FRAME_BYTES,
                    LOGIN_BYTES,
                    128,
                )
                .map(|_| ()),
                Stage::PostAuth => recv_ns(&mut io, Some(&active)).map(|_| ()),
                Stage::FixAck => {
                    login_frames::read_frame(&mut io, &mut Vec::new(), LOGIN_BYTES).map(|_| ())
                }
                Stage::InitDrain => loop {
                    match io.read_poll(&mut [0; 32]) {
                        Err(error)
                            if matches!(
                                error.kind(),
                                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                            ) =>
                        {
                            continue
                        }
                        other => break other.map(|_| ()),
                    }
                },
                Stage::FarmKey => farm_session_inner(
                    LinkStream::Plain(io.stream),
                    "offlinefarm",
                    "offline",
                    "offline-password",
                    false,
                    "offline-session",
                    &BigUint::from(7u32),
                    "offline-hardware",
                    "offline-encoded",
                    18,
                    false,
                    true,
                    Some(&active),
                )
                .map(|_| ()),
                Stage::FarmAck => farm_logon_exchange_inner(
                    &mut LinkStream::Plain(io.stream),
                    &mut SecureChannel::new(),
                    &BigUint::from(7u32),
                    "offline",
                    "offline-password",
                    &[],
                    &[],
                    Some(&active),
                )
                .map(|_| ()),
            };
            done_tx.send(result).unwrap();
        });
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        match stage {
            Stage::AuthStart | Stage::SrpParameters | Stage::MobilePush | Stage::FarmKey => {
                drain_request(&mut peer)
            }
            Stage::SrpChallenge | Stage::SrpResult => {
                drain_request(&mut peer);
                peer.write_all(&xyz_frame(2, &["25", "02"])).unwrap();
                drain_request(&mut peer);
                if matches!(stage, Stage::SrpResult) {
                    peer.write_all(&xyz_frame(4, &["01", "03"])).unwrap();
                    drain_request(&mut peer);
                }
            }
            _ => {}
        }
        // Cancellation must release reads that have already retained a prefix.
        if !matches!(stage, Stage::InitDrain) {
            peer.write_all(if matches!(stage, Stage::FixAck | Stage::FarmAck) {
                b"8=F"
            } else {
                b"#%#"
            })
            .unwrap();
        }
        let stop = Instant::now();
        control.cancel();
        assert!(
            done_rx
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .is_err(),
            "{stage:?}"
        );
        worker.join().unwrap();
        control.join_workers(Duration::from_secs(1)).unwrap();
        assert!(stop.elapsed() < Duration::from_secs(1), "{stage:?}");
        assert!(
            matches!(peer.read(&mut [0]), Ok(0) | Err(_)),
            "live socket after {stage:?}"
        );
    }
}
#[test]
fn controlled_push_stall_approval_timeout_and_total_login_expiry() {
    for total_scope_expires in [false, true] {
        let control = scope(if total_scope_expires {
            Duration::from_millis(150)
        } else {
            Duration::from_secs(5)
        });
        let (client, mut peer) = pair(&control);
        let active = control.clone();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            let mut io = ControlledIo::new(client, Some(&active));
            let deadline =
                (!total_scope_expires).then(|| Instant::now() + Duration::from_millis(150));
            done_tx
                .send(session::do_ib_key_push_bounded(
                    &mut LoginPollIo(&mut io),
                    "2a",
                    deadline,
                    LOGIN_FRAME_BYTES,
                    LOGIN_BYTES,
                    128,
                ))
                .unwrap();
        });
        drain_request(&mut peer);
        peer.write_all(b"#%#").unwrap();
        let error = done_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap_err();
        assert_eq!(
            error.kind(),
            if total_scope_expires {
                io::ErrorKind::ConnectionAborted
            } else {
                io::ErrorKind::TimedOut
            }
        );
        worker.join().unwrap();
        control.cancel();
        control.join_workers(Duration::from_secs(1)).unwrap();
    }
}
#[test]
fn controlled_parallel_farm_failure_waits_for_all_scoped_workers() {
    let control = scope(Duration::from_secs(5));
    let mut clients = Vec::new();
    let mut peers = Vec::new();
    for _ in 0..3 {
        let (client, peer) = pair(&control);
        clients.push(client);
        peers.push(peer);
    }
    let active = control.clone();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let login = thread::spawn(move || {
        let results = thread::scope(|scope| {
            let handles: Vec<_> = clients
                .into_iter()
                .map(|client| {
                    let control = &active;
                    scope.spawn(move || {
                        farm_session_inner(
                            LinkStream::Plain(client),
                            "offlinefarm",
                            "offline",
                            "offline-password",
                            false,
                            "offline-session",
                            &BigUint::from(7u32),
                            "offline-hardware",
                            "offline-encoded",
                            18,
                            false,
                            false,
                            Some(control),
                        )
                        .map(|_| ())
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });
        done_tx.send(results).unwrap();
    });
    // Wait until every actual farm session has sent its logon and is waiting.
    for peer in &mut peers {
        let mut bytes = Vec::new();
        while !bytes.windows(4).any(|w| w == b"\x0110=") {
            let mut chunk = [0; 4096];
            let count = peer.read(&mut chunk).unwrap();
            assert!(count > 0);
            bytes.extend_from_slice(&chunk[..count]);
        }
    }
    control.cancel();
    let results = done_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(results.len(), 3);
    assert!(results.iter().all(Result::is_err));
    login.join().unwrap();
    control.join_workers(Duration::from_secs(1)).unwrap();
}
