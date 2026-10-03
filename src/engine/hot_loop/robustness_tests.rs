//! The hot loop against bad peers (ibx#488, tests layer D), run on its own
//! thread with in-memory links (ibx#484): a peer that stops reading, a
//! peer that closes in the middle of a frame, links that stay down, and
//! tables at their limit. The loop is never blocked: it stops on Shutdown
//! within a bound, and the links still up keep answering.

use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;

use super::HotLoop;
use crate::bridge::SharedState;
use crate::protocol::connection::{mem_pair, Connection};
use crate::test_support::{parse_fields, Peer};
use crate::types::ControlCommand;

/// A link whose engine end holds at most `capacity` bytes the peer has not
/// read, as a socket buffer.
fn link(capacity: Option<usize>) -> (Connection, Peer) {
    let (engine_end, peer_end) = mem_pair();
    engine_end.set_write_capacity(capacity);
    (Connection::new_mem(engine_end), Peer::new(peer_end))
}

/// An engine running `run()` on its own thread. No reconnect credentials:
/// a lost link is never dialled again.
struct Running {
    handle: JoinHandle<HotLoop>,
    control: Sender<ControlCommand>,
    shared: Arc<SharedState>,
}

fn start(farm: Connection, ccp: Connection, hmds: Option<Connection>) -> Running {
    let shared = Arc::new(SharedState::new());
    let (mut engine, control) = HotLoop::with_connections(
        shared.clone(), None, "DUXXXXXXX".into(), farm, ccp, hmds, None);
    let handle = std::thread::spawn(move || {
        engine.run();
        engine
    });
    Running { handle, control, shared }
}

impl Running {
    /// A command to the engine; the control channel never stays full.
    fn send(&self, cmd: ControlCommand) {
        self.control.send_timeout(cmd, Duration::from_secs(2)).expect("the loop takes its commands");
    }

    /// Shutdown, then the engine back. Fails when the loop does not stop
    /// within `limit`: it is blocked.
    fn stop(self, limit: Duration) -> HotLoop {
        self.send(ControlCommand::Shutdown);
        let deadline = Instant::now() + limit;
        while !self.handle.is_finished() {
            assert!(Instant::now() < deadline, "the loop did not stop within {limit:?}: it is blocked");
            std::thread::sleep(Duration::from_millis(2));
        }
        self.handle.join().expect("the loop panicked")
    }

    /// Connection notices until `code` is among them, or `limit`.
    fn wait_notice(&self, code: i64, limit: Duration) -> Vec<i64> {
        let deadline = Instant::now() + limit;
        let mut seen = Vec::new();
        while !seen.contains(&code) && Instant::now() < deadline {
            seen.extend(self.shared.drain_connection_notices().into_iter().map(|n| n.0));
            std::thread::sleep(Duration::from_millis(2));
        }
        seen
    }
}

fn subscribe(con_id: i64) -> ControlCommand {
    ControlCommand::Subscribe {
        con_id, symbol: format!("S{con_id}"), exchange: "SMART".into(), sec_type: "STK".into(),
        last_trade_date: String::new(), strike: 0.0, right: String::new(), multiplier: String::new(),
        mode_9887: 0, snapshot: false, reply_tx: None,
    }
}

/// A test request from `peer`, answered by the engine's heartbeat within
/// `limit`.
fn answers_test_request(peer: &mut Peer, id: &str, limit: Duration) -> bool {
    peer.send_fix(&[(35, "1"), (112, id)]);
    let msgs = peer.messages_until(limit, |ms| ms.iter().any(|m| is_heartbeat(m, id)));
    msgs.iter().any(|m| is_heartbeat(m, id))
}

fn is_heartbeat(m: &[u8], id: &str) -> bool {
    let f = parse_fields(m);
    f.contains(&(35, "0".to_string())) && f.contains(&(112, id.to_string()))
}

// ibx#254: the gateway has no write timeout; a link whose peer stops
// reading keeps its output waiting and stalls only itself. Here the farm
// peer reads nothing while 120 subscriptions go out: their bytes wait on
// the farm link, the loop keeps serving the auth link and stops at once.
#[test]
fn a_peer_that_stops_reading_stalls_only_its_link() {
    let (farm, _farm_peer) = link(Some(1024));
    let (ccp, mut ccp_peer) = link(None);
    let (hmds, mut hmds_peer) = link(None);
    let running = start(farm, ccp, Some(hmds));
    for con_id in 1..=120 {
        running.send(subscribe(con_id));
    }
    std::thread::sleep(Duration::from_millis(50));
    assert!(answers_test_request(&mut ccp_peer, "auth-while-farm-stalled", Duration::from_secs(2)),
        "the auth link is served while the farm peer reads nothing");
    assert!(answers_test_request(&mut hmds_peer, "hmds-while-farm-stalled", Duration::from_secs(2)),
        "the historical link too");
    let engine = running.stop(Duration::from_secs(1));
    let farm = engine.farm_conn.as_ref().unwrap();
    assert!(farm.has_queued_output(), "the farm output waits for its peer");
    assert!(farm.write_error().is_none() && !engine.farm.disconnected, "a slow peer is not an error");
}

// ibx#254: a write error drops the link it happened on (2103 for the
// farm), the frame is not sent again, and the other links go on.
#[test]
fn a_write_error_drops_only_its_link() {
    let (farm, farm_peer) = link(Some(1024));
    let (ccp, mut ccp_peer) = link(None);
    let running = start(farm, ccp, None);
    for con_id in 1..=60 {
        running.send(subscribe(con_id));
    }
    drop(farm_peer);
    for con_id in 61..=80 {
        running.send(subscribe(con_id));
    }
    let seen = running.wait_notice(2103, Duration::from_secs(2));
    assert!(seen.contains(&2103), "the farm link is reported lost: {seen:?}");
    assert!(!seen.contains(&1100), "not the auth link: {seen:?}");
    assert!(answers_test_request(&mut ccp_peer, "auth-after-farm-error", Duration::from_secs(2)));
    let engine = running.stop(Duration::from_secs(1));
    assert!(engine.farm.disconnected && !engine.ccp.disconnected);
}

/// The first half of a heartbeat frame of `peer`'s link.
fn half_frame() -> Vec<u8> {
    let frame = crate::protocol::fix::fix_build(&[(35, "1"), (112, "cut")], 7);
    frame[..frame.len() / 2].to_vec()
}

// A peer that closes in the middle of a frame: the link is lost (1100 for
// the auth link), the half frame is dropped with it, nothing panics, and
// the farm and historical links go on.
#[test]
fn an_auth_peer_closing_mid_frame_loses_only_its_link() {
    let (farm, mut farm_peer) = link(None);
    let (ccp, mut ccp_peer) = link(None);
    let (hmds, mut hmds_peer) = link(None);
    let running = start(farm, ccp, Some(hmds));
    assert!(answers_test_request(&mut farm_peer, "before", Duration::from_secs(2)));
    ccp_peer.send_raw(&half_frame());
    std::thread::sleep(Duration::from_millis(20));
    ccp_peer.close();
    let seen = running.wait_notice(1100, Duration::from_secs(2));
    assert!(seen.contains(&1100), "the auth link is reported lost: {seen:?}");
    assert!(answers_test_request(&mut farm_peer, "farm-after-auth-cut", Duration::from_secs(2)));
    assert!(answers_test_request(&mut hmds_peer, "hmds-after-auth-cut", Duration::from_secs(2)));
    let engine = running.stop(Duration::from_secs(1));
    assert!(engine.ccp.disconnected && !engine.farm.disconnected && !engine.hmds.disconnected);
}

#[test]
fn a_farm_peer_closing_mid_frame_loses_only_its_link() {
    let (farm, mut farm_peer) = link(None);
    let (ccp, mut ccp_peer) = link(None);
    let running = start(farm, ccp, None);
    farm_peer.send_raw(&half_frame());
    std::thread::sleep(Duration::from_millis(20));
    farm_peer.close();
    let seen = running.wait_notice(2103, Duration::from_secs(2));
    assert!(seen.contains(&2103), "the farm link is reported lost: {seen:?}");
    assert!(answers_test_request(&mut ccp_peer, "auth-after-farm-cut", Duration::from_secs(2)));
    let engine = running.stop(Duration::from_secs(1));
    assert!(engine.farm.disconnected && !engine.ccp.disconnected);
}

/// Passes of the loop in `window` on these links.
fn passes_in(window: Duration, farm: Connection, ccp: Connection, hmds: Option<Connection>) -> u64 {
    let running = start(farm, ccp, hmds);
    std::thread::sleep(window);
    running.stop(Duration::from_secs(1)).context.loop_iterations
}

// ibx#399: with every link down the loop spun at about a million passes a
// second for the whole outage. Now it rests 1 ms a pass, and with a link
// up it waits on that link's read (1 ms): no core is pinned either way.
#[test]
fn a_loop_with_links_down_does_not_spin() {
    let window = Duration::from_millis(300);

    let (farm, mut farm_peer) = link(None);
    let (ccp, mut ccp_peer) = link(None);
    farm_peer.close();
    ccp_peer.close();
    let all_down = passes_in(window, farm, ccp, None);
    assert!(all_down < 1_000, "{all_down} passes in {window:?} with every link down");

    let (farm, mut farm_peer) = link(None);
    let (ccp, _ccp_peer) = link(None);
    farm_peer.close();
    let auth_up = passes_in(window, farm, ccp, None);
    assert!(auth_up < 5_000, "{auth_up} passes in {window:?} with only the idle auth link up");
}

// ibx#399, ibx#218: a reconnect that fails is tried again on the backoff
// (5 to 15 s for the farm and the auth link, 3 s doubling for the
// historical farm), never at once.
#[test]
fn a_failed_reconnect_waits_for_the_backoff() {
    let fail = || {
        let (tx, rx) = crossbeam_channel::bounded(1);
        tx.send(Err(std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused"))).unwrap();
        rx
    };
    let mut engine = HotLoop::new(Arc::new(SharedState::new()), None, None);
    engine.farm.disconnected = true;
    engine.ccp.disconnected = true;
    for _ in 0..3 {
        engine.pending_farm_reconnect = Some(fail());
        engine.farm_reconnect_attempt += 1;
        engine.poll_farm_reconnect();
        let now = Instant::now();
        engine.maybe_spawn_farm_reconnect();
        assert!(engine.pending_farm_reconnect.is_none(), "no new attempt at once");
        assert!(engine.farm_next_attempt_at.is_some_and(|at| at >= now + Duration::from_secs(5)), "the next attempt waits 5 s or more");
        engine.farm_next_attempt_at = None;
    }

    let (tx, rx) = crossbeam_channel::bounded(1);
    tx.send(Err(std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused"))).unwrap();
    engine.pending_ccp_reconnect = Some(rx);
    engine.ccp_reconnect_attempt = 1;
    engine.poll_ccp_reconnect();
    let now = Instant::now();
    engine.maybe_spawn_ccp_reconnect();
    assert!(engine.pending_ccp_reconnect.is_none());
    assert!(engine.ccp_next_attempt_at.is_some_and(|at| at >= now + Duration::from_secs(5)));

    let (tx, rx) = crossbeam_channel::bounded(1);
    tx.send(Err(std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused"))).unwrap();
    engine.pending_hmds_reconnect = Some(rx);
    engine.hmds_reconnect_attempt = 1;
    let now = Instant::now();
    engine.poll_hmds_reconnect();
    assert!(engine.hmds_next_attempt_at.is_some_and(|at| at >= now + Duration::from_secs(5)), "the second historical attempt waits 6 s");

    // Without credentials an attempt is skipped once, then looked at again
    // a minute later, not on every pass.
    engine.farm_next_attempt_at = Some(Instant::now());
    engine.maybe_spawn_farm_reconnect();
    let next = engine.farm_next_attempt_at.expect("rescheduled");
    for _ in 0..1000 {
        engine.maybe_spawn_farm_reconnect();
    }
    assert_eq!(engine.farm_next_attempt_at, Some(next), "no attempt on the next passes");
    assert!(next >= Instant::now() + Duration::from_secs(50));
}

/// A 35=P message of one block: the server tag, then (type, value) ticks
/// of 4 bytes each.
fn tick_message(server_tag: u32, ticks: &[(u64, u64)]) -> Vec<u8> {
    let mut bits: Vec<bool> = Vec::new();
    let mut push = |v: u64, n: usize| (0..n).rev().for_each(|i| bits.push((v >> i) & 1 == 1));
    push(0, 1);
    push(server_tag as u64, 31);
    for (i, &(kind, value)) in ticks.iter().enumerate() {
        push(kind, 5);
        push((i + 1 < ticks.len()) as u64, 1);
        push(3, 2);
        push(0, 1);
        push(value, 31);
    }
    let mut body = (bits.len() as u16).to_be_bytes().to_vec();
    body.extend(bits.chunks(8).map(|c| c.iter().enumerate().fold(0u8, |b, (i, &on)| b | (on as u8) << (7 - i))));
    let mut msg = b"8=O\x019=0000\x0135=P\x01".to_vec();
    msg.extend(body);
    msg.extend_from_slice(b"\x018349=00000000\x01");
    msg
}

// ibx#281: live sessions start their server tags far above 65536 (274555
// was seen); the largest a 31-bit tag can be routes as well.
#[test]
fn server_tags_up_to_the_largest_route_their_ticks() {
    let shared = Arc::new(SharedState::new());
    let mut engine = HotLoop::new(shared.clone(), None, None);
    let tags = [274_555u32, 70_000_000, 0x7FFF_FFFF];
    for (k, &tag) in tags.iter().enumerate() {
        let id = engine.context.market.try_register(1_000 + k as i64).unwrap();
        engine.context.market.register_server_tag(tag, id, 0.01);
        engine.inject_farm_message(&tick_message(tag, &[(crate::protocol::tick_decoder::O_BID_PRICE, 15_000 + k as u64)]));
        assert_eq!(engine.context.quote(id).bid, (15_000 + k as i64) * crate::types::PRICE_SCALE / 100, "tag {tag}");
    }
}

// ibx#257, ibx#233: with every instrument slot taken, the recorded server
// frames that register contracts (order replays, positions, contract
// replies) find no slot and are dropped, a subscription is refused by its
// reply, and the links go on.
#[test]
fn a_full_instrument_table_refuses_and_goes_on() {
    let (farm, mut farm_peer) = link(None);
    let (ccp, mut ccp_peer) = link(None);
    let shared = Arc::new(SharedState::new());
    let (mut engine, control) = HotLoop::with_connections(
        shared.clone(), None, "DUXXXXXXX".into(), farm, ccp, None, None);
    for i in 0..crate::types::MAX_INSTRUMENTS as i64 {
        engine.context.market.try_register(5_000_000 + i).unwrap();
    }
    for s in crate::robustness::server_frames().iter().filter(|s| s.conn == "CCP") {
        engine.inject_ccp_message(&s.raw);
    }
    assert_eq!(engine.context.market.count() as usize, crate::types::MAX_INSTRUMENTS);

    let (reply_tx, reply_rx) = crossbeam_channel::bounded(1);
    let ControlCommand::Subscribe { con_id, symbol, exchange, sec_type, last_trade_date, strike, right, multiplier, mode_9887, snapshot, .. } = subscribe(265598) else { unreachable!() };
    control.send(ControlCommand::Subscribe {
        con_id, symbol, exchange, sec_type, last_trade_date, strike, right, multiplier, mode_9887, snapshot,
        reply_tx: Some(reply_tx),
    }).unwrap();
    engine.step_for_test();
    let reply = reply_rx.recv_timeout(Duration::from_secs(1)).expect("a reply");
    assert!(reply.is_err_and(|e| e.contains("instrument table full")));

    farm_peer.send_fix(&[(35, "1"), (112, "full-farm")]);
    ccp_peer.send_fix(&[(35, "1"), (112, "full-auth")]);
    for _ in 0..4 {
        engine.step_for_test();
    }
    assert!(farm_peer.messages().iter().any(|m| is_heartbeat(m, "full-farm")));
    assert!(ccp_peer.messages().iter().any(|m| is_heartbeat(m, "full-auth")));
}
