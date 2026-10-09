//! The engine against a scripted peer (ibx#484 layer 0): the hot loop runs
//! on its own thread with in-memory connections, the API client places an
//! order, and the test reads back the exact frame the engine wrote on the
//! auth connection: framing, sequence, checksum and signature checked, the
//! fields compared with the reference's frame through the shared
//! normaliser. No network, no credentials.

use std::sync::Arc;
use std::time::Duration;

use ibx::api::client::{Contract, EClient, Order};
use ibx::bridge::SharedState;
use ibx::engine::hot_loop::HotLoop;
use ibx::protocol::connection::Frame;
use ibx::protocol::fix;
use ibx::test_support::{assert_same_fields, parse_fields, Normaliser, Peer};
use ibx::types::ControlCommand;

/// A new limit order of the reference (gateway 1040, paper, account
/// masked; `tests/fixtures/gw1040/order_frames/frames.tsv`, from
/// api-logs/39_placeOrder_AAPL_STK): API order 5 of client 39.
const REFERENCE_LMT: &str = "35=D|11=944233825.0|44=262.35|1=DUXXXXXXX|6122=c|6121=5|6119=39|38=1|40=2|55=AAPL|167=STK|231=1.00|54=1|59=0|100=BEST|6210=BEST|6008=265598|6088=Socket|15=USD|6211=|6238=";

fn reference_frames() -> Vec<String> {
    let path = format!("{}/tests/fixtures/gw1040/order_frames/frames.tsv", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(path).unwrap()
        .lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| l.split_once('\t').map(|(_, frame)| frame.to_string()))
        .collect()
}

/// The 3 digit checksum of a frame is the sum of its bytes before the
/// trailer, and 9= counts the bytes between it and the trailer.
fn check_framing(msg: &[u8]) {
    let text = String::from_utf8_lossy(msg);
    let trailer = text.rfind("\x0110=").expect("a checksum trailer") + 1;
    assert_eq!(&text[trailer + 3..], format!("{}\x01", fix::fix_checksum(&msg[..trailer])), "checksum");
    let len_start = text.find("\x019=").unwrap() + 3;
    let len_end = len_start + text[len_start..].find('\x01').unwrap();
    let body_len: usize = text[len_start..len_end].parse().unwrap();
    assert_eq!(body_len, trailer - (len_end + 1), "body length");
}

#[test]
fn a_new_order_reaches_the_server_as_the_reference_frame() {
    assert!(reference_frames().iter().any(|f| f == REFERENCE_LMT), "the reference frame is in the fixture");

    let shared = Arc::new(SharedState::new());
    shared.reference.set_api_client_id(39);
    let (farm_conn, _farm) = Peer::pair();
    let (mut ccp_conn, mut ccp) = Peer::pair();
    // A signed auth link, as after the logon: the peer checks the
    // signature chain of every frame.
    let mac_key: Vec<u8> = (1..=20).collect();
    ccp_conn.set_keys(mac_key.clone(), (0..16).collect(), mac_key, (16..32).collect());
    ccp.sign_like(&ccp_conn);
    let (mut engine, control_tx) = HotLoop::with_connections(
        shared.clone(), None, "DUXXXXXXX".into(), farm_conn, ccp_conn, None, None);
    let stop_tx = control_tx.clone();
    let handle = std::thread::spawn(move || engine.run());
    let client = EClient::from_parts(shared, control_tx, std::thread::spawn(|| {}), "DUXXXXXXX".into());

    let contract = Contract {
        con_id: 265598, symbol: "AAPL".into(), sec_type: "STK".into(),
        exchange: "SMART".into(), currency: "USD".into(), ..Default::default()
    };
    let order = Order {
        action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(),
        lmt_price: 262.35, tif: "DAY".into(), ..Default::default()
    };
    client.place_order(5, &contract, &order).unwrap();

    let is_order = |m: &Vec<u8>| parse_fields(m).iter().any(|(t, v)| *t == 35 && v == "D");
    let sent = ccp.messages_until(Duration::from_secs(5), |ms| ms.iter().any(is_order));
    stop_tx.send(ControlCommand::Shutdown).unwrap();
    handle.join().unwrap();

    let order_msgs: Vec<&Vec<u8>> = sent.iter().filter(|m| is_order(m)).collect();
    assert_eq!(order_msgs.len(), 1, "one new order");
    let msg = order_msgs[0];
    check_framing(msg);
    // The engine's own order id and version: the reference's is its own.
    let fields = parse_fields(msg);
    assert!(fields.iter().any(|(t, v)| *t == 11 && v.ends_with(".0")), "a new order is version 0");
    let n = Normaliser::session();
    assert_same_fields(&n.msg(msg), &n.pipe(REFERENCE_LMT));
}

#[test]
fn the_peer_reads_the_exact_bytes_on_the_wire() {
    // The bytes the peer reads are the frame built and signed by the
    // builder and signer of the engine, byte for byte, and its signature
    // chain checks.
    let (mut conn, mut peer) = Peer::pair();
    let mac_key: Vec<u8> = (1..=20).collect();
    let iv: Vec<u8> = (0..16).collect();
    conn.set_keys(mac_key.clone(), iv.clone(), Vec::new(), Vec::new());
    peer.sign_like(&conn);
    conn.send_fix(&[(35, "0")]).unwrap();
    conn.send_fix(&[(35, "1"), (112, "T")]).unwrap();
    let (first, next_iv) = fix::fix_sign(&fix::fix_build(&[(35, "0")], 1), &mac_key, &iv);
    let (second, _) = fix::fix_sign(&fix::fix_build(&[(35, "1"), (112, "T")], 2), &mac_key, &next_iv);
    let frames = peer.frames();
    let [Frame::Fix(a), Frame::Fix(b)] = &frames[..] else { panic!("two frames: {frames:?}") };
    assert_eq!((a, b), (&first, &second));
    let mut chained = Peer::pair().1;
    chained.conn().set_keys(Vec::new(), Vec::new(), mac_key, iv);
    assert!(chained.conn().unsign(a).1 && chained.conn().unsign(b).1, "the signature chain checks");
    check_framing(&fix::fix_build(&[(35, "1"), (112, "T")], 2));
}

#[test]
fn compressed_messages_round_trip_through_the_peer() {
    let (mut conn, mut peer) = Peer::pair();
    conn.send_fixcomp(&[(35, "V"), (262, "7")]).unwrap();
    let msgs = peer.messages();
    assert_eq!(msgs.len(), 1);
    let fields = parse_fields(&msgs[0]);
    assert!(fields.contains(&(35, "V".into())) && fields.contains(&(262, "7".into())), "{fields:?}");

    // And the other way: what the peer sends, the engine side extracts.
    peer.send_fixcomp(&[(35, "P"), (262, "7")]);
    conn.try_recv().unwrap();
    let frames = conn.extract_frames();
    assert!(matches!(&frames[..], [Frame::FixComp(_)]), "{frames:?}");
}

/// The open orders a client lists: (order id, permId) of each openOrder,
/// and whether the end came.
#[derive(Default)]
struct Listing {
    open: Vec<(i64, i64)>,
    end: bool,
}

impl ibx::api::wrapper::Wrapper for Listing {
    fn open_order(&mut self, order_id: i64, _c: &Contract, order: &Order, _s: &ibx::api::types::OrderState) {
        self.open.push((order_id, order.perm_id));
    }
    fn open_order_end(&mut self) {
        self.end = true;
    }
}

/// The engine on a signed in-memory auth link, with a Rust client on top.
fn session(shared: &Arc<SharedState>) -> (EClient, Peer, ibx::engine::park::ControlSender, std::thread::JoinHandle<()>) {
    let (farm_conn, _farm) = Peer::pair();
    let (mut ccp_conn, mut ccp) = Peer::pair();
    let mac_key: Vec<u8> = (1..=20).collect();
    ccp_conn.set_keys(mac_key.clone(), (0..16).collect(), mac_key, (16..32).collect());
    ccp.sign_like(&ccp_conn);
    let (mut engine, control_tx) = HotLoop::with_connections(
        shared.clone(), None, "DUXXXXXXX".into(), farm_conn, ccp_conn, None, None);
    let stop_tx = control_tx.clone();
    let handle = std::thread::spawn(move || engine.run());
    let client = EClient::from_parts(shared.clone(), control_tx, std::thread::spawn(|| {}), "DUXXXXXXX".into());
    (client, ccp, stop_tx, handle)
}

// Paper 05/10/2026 (global_cancel_paper): an order a Rust client placed in
// one session was not found among the open orders of the next session by
// its order id, which was built from the clock, above the int range, so
// the new order carried no API order id (6121). The order ids are now the
// reference's (ibx#466): the next valid id is the highest order id the
// client used + 1, 1 for a client with none; the new order goes out under
// a server id of the order id generator, its API order id in 6121. The
// next session's logon replay gives the order with its 6121: it is listed
// with that order id and its permId (the server id), and the next valid id
// of that session is above it.
#[test]
fn an_order_of_an_earlier_session_keeps_its_order_id() {
    // Session 1: the new order as sent.
    let shared = Arc::new(SharedState::new());
    let (client, mut ccp, stop_tx, handle) = session(&shared);
    let mut ids = NextIds::default();
    client.req_ids(&mut ids);
    let id = client.next_order_id();
    assert_eq!((ids.0.as_slice(), id), ([1].as_slice(), 1), "a client with no orders starts at 1");
    let spy = Contract {
        con_id: 756733, symbol: "SPY".into(), sec_type: "STK".into(),
        exchange: "SMART".into(), currency: "USD".into(), ..Default::default()
    };
    let order = Order {
        action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(),
        lmt_price: 1.0, tif: "GTC".into(), ..Default::default()
    };
    client.place_order(id, &spy, &order).unwrap();
    let is_order = |m: &Vec<u8>| parse_fields(m).iter().any(|(t, v)| *t == 35 && v == "D");
    let sent = ccp.messages_until(Duration::from_secs(5), |ms| ms.iter().any(is_order));
    client.req_ids(&mut ids);
    assert_eq!(ids.0, [1, 2], "the placed id counts");
    stop_tx.send(ControlCommand::Shutdown).unwrap();
    handle.join().unwrap();
    let new_order = parse_fields(sent.iter().find(|m| is_order(m)).expect("the new order"));
    let tag = |t: u32| new_order.iter().find(|(k, _)| *k == t).map(|(_, v)| v.clone());
    let clord = tag(11).expect("a ClOrdID");
    let perm: i64 = clord.strip_suffix(".0").and_then(|p| p.parse().ok()).expect("a server id, version 0");
    assert!(perm > 0 && perm <= i64::from(i32::MAX), "a server id of the order id generator: {clord}");
    assert_eq!(tag(6121).as_deref(), Some("1"), "the API order id is sent");
    assert_eq!(tag(6119).as_deref(), Some("0"));

    // Session 2: the logon replay gives it back, working, then its end.
    let shared = Arc::new(SharedState::new());
    let (client, mut ccp, stop_tx, handle) = session(&shared);
    ccp.send_fix(&[
        (35, "8"), (11, clord.as_str()), (17, "140781.1791194600.0"), (150, "A"), (20, "3"), (39, "A"),
        (167, "CS"), (55, "SPY"), (6210, "BEST"), (38, "1"), (44, "1"), (32, "0"), (31, "0.00"), (14, "0"),
        (151, "1"), (6, "0"), (54, "1"), (37, "00cf16ed.000225ed.6abde767.0001"), (1, "DUXXXXXXX"),
        (40, "2"), (6121, "1"), (6119, "0"), (59, "1"), (6008, "756733"), (15, "USD"), (6088, "Socket"),
    ]);
    ccp.send_fix(&[(35, "8"), (11, "*"), (150, "0"), (20, "3"), (39, "0"), (55, "*"), (37, "*")]);
    let mut listing = Listing::default();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while shared.orders.get_order_info(id).is_none() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    // The replay's report reaches this client (client 0, the order's).
    client.process_msgs(&mut listing);
    assert_eq!(listing.open, [(id, perm)], "the replayed report: its order id and its permId");
    let mut ids = NextIds::default();
    client.req_ids(&mut ids);
    assert_eq!((ids.0.as_slice(), client.next_order_id()), ([2].as_slice(), 2), "above the replayed order's id");
    listing.open.clear();
    client.req_all_open_orders(&mut listing);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !listing.end && std::time::Instant::now() < deadline {
        client.process_msgs(&mut listing);
        std::thread::sleep(Duration::from_millis(10));
    }
    stop_tx.send(ControlCommand::Shutdown).unwrap();
    handle.join().unwrap();
    assert!(listing.end, "the listing ends");
    assert_eq!(listing.open, [(id, perm)], "listed with its order id and its permId");
}

/// The ids of each nextValidId.
#[derive(Default)]
struct NextIds(Vec<i64>);

impl ibx::api::wrapper::Wrapper for NextIds {
    fn next_valid_id(&mut self, order_id: i64) {
        self.0.push(order_id);
    }
}
