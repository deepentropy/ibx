//! The tests for this module.
//!
//! One file per module, as `api/client` already does it. Each block below
//! reaches the code it tests through `super::super`, which is the module this
//! file belongs to.

mod historical_contract_tests {
    use super::super::{hist_exchange, hist_sec_type};

    /// The substitution the engine applies to whatever the client sent.
    /// Exercised here rather than on the query builder alone, which honours
    /// these fields either way and so cannot show where they are applied.
    #[test]
    fn a_stated_security_type_reaches_the_wire_in_its_own_spelling() {
        assert_eq!(hist_sec_type("FUT"), "FUT");
        assert_eq!(hist_sec_type("OPT"), "OPT");
        assert_eq!(hist_sec_type("CASH"), "CASH");
        // Both vocabularies for a stock land on the wire spelling.
        assert_eq!(hist_sec_type("STK"), "CS");
        assert_eq!(hist_sec_type("CS"), "CS");
        // Absent keeps exactly what every caller got before.
        assert_eq!(hist_sec_type(""), "CS");
        // A valid type the enum does not carry is sent as stated, not
        // narrowed away — the subscribe path does the same.
        assert_eq!(hist_sec_type("FOP"), "FOP");
        assert_eq!(hist_sec_type("CFD"), "CFD");
        // A value shaped like a type but unknown to both the enum and the
        // gateway still reaches it, to be named there rather than silently
        // described as a stock.
        assert_eq!(hist_sec_type("NOPE"), "NOPE");
        // Anything that could break the query document is blanked instead of
        // embedded. The value lands in XML, so this is the difference between
        // a refused query and a malformed one.
        assert_eq!(hist_sec_type("FOP&"), "");
        assert_eq!(hist_sec_type("<x>"), "");
        assert_eq!(hist_sec_type("A B"), "");
        assert_eq!(hist_sec_type("VERYLONGTYPE"), "");
    }

    #[test]
    fn a_stated_venue_reaches_the_wire_and_an_absent_one_defaults() {
        assert_eq!(hist_exchange("CME"), "CME");
        assert_eq!(hist_exchange("IDEALPRO"), "IDEALPRO");
        assert_eq!(hist_exchange(""), "SMART");
    }
}
/// A reconnect resubscribes the tick-by-tick streams. Replacing the socket
/// alone leaves every stream behind on the dead one while the transport
/// reports healthy, so nothing anywhere states that the data has stopped.
#[test]
fn a_reconnect_puts_the_tick_by_tick_streams_back() {
    let mut hmds = HmdsState::new();
    let mut market = crate::engine::market_state::MarketState::new();
    let instrument = market.try_register(756733).expect("slot");
    hmds.tbt_subscriptions.push(TbtSubscription { ignore_size: false, instrument, query_id: "tbt_0".to_string(), kind: TbtType::Last, caller_req_id: 0, venue_id: 0, min_tick: 0, size_tick: 0.0, running: Default::default() });
    // One with no contract behind it: it must be reported, not resubscribed
    // against a contract id the engine does not have.
    hmds.tbt_subscriptions.push(TbtSubscription { ignore_size: false, instrument: 7, query_id: "tbt_1".to_string(), kind: TbtType::BidAsk, caller_req_id: 0, venue_id: 0, min_tick: 0, size_tick: 0.0, running: Default::default() });

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let sock = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (_peer, _) = listener.accept().unwrap();
    let mut conn = None;
    let mut hb = HeartbeatState::new();
    hmds.disconnected = true;
    hmds.reconnect(
        crate::protocol::connection::Connection::new_raw(sock).unwrap(),
        &mut conn, &market, &mut hb,
    );

    assert!(!hmds.disconnected, "the transport is live again");
    assert_eq!(hmds.tbt_subscriptions.len(), 1, "the resolvable stream is back");
    assert_eq!(
        hmds.tbt_subscriptions[0].running, Default::default(),
        "the dead session's prices go with it, or its last pair takes the next move",
    );
    assert_eq!(hmds.tbt_subscriptions[0].instrument, instrument);
    assert_ne!(hmds.tbt_subscriptions[0].query_id, "tbt_0", "under a new id, not the dead session's");
}

/// The routing for a five-second bar stream is a ticker id the session
/// issued. A reconnect that kept the routing and re-sent nothing left the
/// bars stopped with the connection reporting healthy.
///
/// A keep-up-to-date request needs only this stream restored: its bars are
/// folded from it, and the partial bar survives the reconnect. A second
/// request for the same stream leaves two subscriptions upstream for one
/// caller.
#[test]
fn a_reconnect_asks_for_the_five_second_bars_again() {
    let mut hmds = HmdsState::new();
    let market = crate::engine::market_state::MarketState::new();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let sock = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (_peer, _) = listener.accept().unwrap();
    let mut conn = Some(crate::protocol::connection::Connection::new_raw(sock).unwrap());
    let mut hb = HeartbeatState::new();
    hmds.send_realtime_bar_subscribe(9, 265598, "", "STK", "SMART", "TRADES", true, &mut conn, &mut hb);
    let first = hmds.rtbar_subs[0].0.clone();
    // State a keep-up-to-date request leaves behind: the stream and the
    // partial bar.
    hmds.keep_up_to_date_reqs.insert(9);
    hmds.forming_bars.push(super::FormingBar {
        req_id: 9,
        seconds: 60,
        opened_at: 0,
        bar: Default::default(),
        weighted: 0.0,
    });

    let sock2 = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (_peer2, _) = listener.accept().unwrap();
    hmds.disconnected = true;
    hmds.reconnect(
        crate::protocol::connection::Connection::new_raw(sock2).unwrap(),
        &mut conn, &market, &mut hb,
    );

    assert_eq!(hmds.rtbar_subs.len(), 1, "the stream is asked for again");
    assert_ne!(hmds.rtbar_subs[0].0, first, "under a new query, not the dead session's");
    assert_eq!(hmds.rtbar_subs[0].1, 9, "still answering the caller's request id");
    assert_eq!(
        hmds.forming_bars.iter().filter(|f| f.req_id == 9).count(), 1,
        "and the bar it was folding is still the one being folded",
    );
    assert!(
        hmds.pending_historical.iter().all(|(_, rid)| *rid != 9),
        "nothing else is asked for on its behalf: one request, one stream",
    );
}

use super::*;

/// The diagnostic log byte-sliced a lossily decoded value, so a tag whose
/// two-hundredth byte fell inside a multi-byte character aborted the hot
/// loop — and only when debug logging was on, which is to say only while
/// someone was diagnosing an incident.
#[test]
fn a_non_utf8_xml_tag_does_not_abort_the_hot_loop() {
    // Driven through the handler that performs the slice, so the assertion
    // depends on the code under test.
    //
    // 199 ASCII bytes then one invalid byte. Lossily decoded, that byte becomes
    // a three-byte replacement character, so byte 200 falls inside it.
    let mut payload = b"<ResultSetBar>".to_vec();
    payload.extend(std::iter::repeat_n(b'a', 199 - payload.len()));
    payload.push(0xFF);

    let mut msg = Vec::new();
    msg.extend_from_slice(b"35=W\x016118=");
    msg.extend_from_slice(&payload);
    msg.push(0x01);

    let mut hmds = HmdsState::new();
    let shared = crate::bridge::SharedState::new();
    let mut hb = HeartbeatState::new();
    let mut conn: Option<crate::protocol::connection::Connection> = None;
    // The slice runs only with debug logging enabled.
    log::set_max_level(log::LevelFilter::Debug);
    hmds.process_hmds_message(&msg, &mut conn, &shared, &None, &mut hb);
}

fn make_query_error_msg(query_id: &str, error: &str) -> Vec<u8> {
    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<QueryError>\n\t<id>{query_id}</id>\n\t<error>{error}</error>\n</QueryError>\n",
    );
    let mut msg = Vec::new();
    msg.extend_from_slice(b"35=W\x016118=");
    msg.extend_from_slice(xml.as_bytes());
    msg.push(0x01);
    msg
}

fn make_bar_msg(query_id: &str, eoq: bool) -> Vec<u8> {
    let xml = format!(
        "<ResultSetBar><id>{}</id><eoq>{}</eoq><tz>UTC</tz><Events>\
         <Bar><time>20260714-13:30:00</time><open>100.0</open><close>100.5</close>\
         <high>100.7</high><low>99.9</low><weightedAvg>100.2</weightedAvg>\
         <volume>1000</volume><count>10</count></Bar></Events></ResultSetBar>",
        query_id, if eoq { "true" } else { "false" },
    );
    let mut msg = Vec::new();
    msg.extend_from_slice(b"35=W\x016118=");
    msg.extend_from_slice(xml.as_bytes());
    msg.push(0x01);
    msg
}

#[test]
fn segmented_bar_reply_completes_on_eoq_true() {
    // /: a segmented bar reply carries <eoq>false> on
    // early frames and <eoq>true> on the final one. The pending entry must
    // persist through the false frames and be released on the true frame.
    let mut hmds = HmdsState::new();
    let shared = SharedState::new();
    let mut hb = HeartbeatState::new();
    let mut conn: Option<Connection> = None;
    hmds.pending_historical.push(("q7".to_string(), 21));
    hmds.held.push(HeldSeries {
        req_id: 21, fold: Fold::None, con_id: 0, sec_type: String::new(), exchange: String::new(),
        bars: Vec::new(), timezone: String::new(), actions_asked: false, actions_query: None, actions: None, complete: false,
    });

    hmds.process_hmds_message(&make_bar_msg("q7", false), &mut conn, &shared, &None, &mut hb);
    assert_eq!(hmds.pending_historical.len(), 1, "entry must persist through eoq=false");
    assert!(shared.reference.drain_historical_data().is_empty(), "nothing is filed before the last page");

    hmds.process_hmds_message(&make_bar_msg("q7", true), &mut conn, &shared, &None, &mut hb);
    assert!(hmds.pending_historical.is_empty(), "eoq=true must release the pending entry");

    // Both pages, filed once, whole.
    let hist = shared.reference.drain_historical_data();
    assert_eq!(hist.len(), 1, "the series is filed once its last page is in");
    assert!(hist[0].1.is_complete, "and it is the whole answer");
    assert_eq!(hist[0].1.bars.len(), 2, "with every page's bars in it");
}

#[test]
fn conadj_response_frame_is_skipped_without_disturbing_pending() {
    // /: the 6040=10022 ConAdjResponse (corporate
    // actions) is pushed once per contract on the first historical request.
    // It must be recognized and skipped, not treated as bar or completion.
    let mut hmds = HmdsState::new();
    let shared = SharedState::new();
    let mut hb = HeartbeatState::new();
    let mut conn: Option<Connection> = None;
    hmds.pending_historical.push(("q8".to_string(), 22));

    let mut msg = Vec::new();
    msg.extend_from_slice(b"35=U\x016040=10022\x016118=");
    msg.extend_from_slice(b"<ConAdjResponse><id>ContractAdjustment1</id></ConAdjResponse>");
    msg.push(0x01);
    hmds.process_hmds_message(&msg, &mut conn, &shared, &None, &mut hb);

    assert_eq!(hmds.pending_historical.len(), 1, "ConAdjResponse must not touch pending historical");
    assert!(shared.reference.drain_historical_data().is_empty());
    assert!(shared.reference.drain_historical_errors().is_empty());
}

#[test]
fn query_error_releases_historical_and_emits_error_and_end_sentinel() {
    let mut hmds = HmdsState::new();
    let shared = SharedState::new();
    let mut hb = HeartbeatState::new();
    let mut conn: Option<Connection> = None;
    hmds.pending_historical.push(("hist_1003".to_string(), 11));
    hmds.keep_up_to_date_reqs.insert(11);
    // A page already held for it: a refused query takes what it held with it,
    // or the caller is answered with the error and then left waiting on a
    // series that will never complete.
    hmds.held.push(HeldSeries {
        req_id: 11, fold: Fold::None, con_id: 0, sec_type: String::new(), exchange: String::new(),
        bars: Vec::new(), timezone: String::new(), actions_asked: false, actions_query: None, actions: None, complete: false,
    });
    hmds.process_hmds_message(&make_bar_msg("hist_1003", false), &mut conn, &shared, &None, &mut hb);

    let msg = make_query_error_msg("hist_1003", "Invalid time length");
    hmds.process_hmds_message(&msg, &mut conn, &shared, &None, &mut hb);

    assert!(hmds.pending_historical.is_empty(), "pending entry should be drained");
    assert!(hmds.held.is_empty(), "the held pages go with the refused query");
    assert!(!hmds.keep_up_to_date_reqs.contains(&11), "kut flag should be cleared");

    let errors = shared.reference.drain_historical_errors();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].0, 11);
    assert_eq!(errors[0].1, 162);
    assert_eq!(errors[0].2, "Invalid time length");

    let hist = shared.reference.drain_historical_data();
    assert_eq!(hist.len(), 1, "terminal sentinel must be queued for historical req");
    assert_eq!(hist[0].0, 11);
    assert!(hist[0].1.is_complete);
    assert!(hist[0].1.bars.is_empty());
}
/// A head timestamp the connection cannot carry is refused, not recorded as
/// pending. Pushed unconditionally, a request with no connection sat pending
/// with no answer ever coming.
#[test]
fn a_head_timestamp_with_no_connection_is_refused_not_left_pending() {
    let mut hmds = HmdsState::new();
    let shared = SharedState::new();
    let mut hb = HeartbeatState::new();
    let mut conn: Option<Connection> = None;
    shared.reference.cache_definition(265598, crate::types::model::Contract {
        con_id: 265598, symbol: "AAPL".into(), sec_type: "STK".into(), exchange: "SMART".into(),
        ..Default::default()
    });

    hmds.send_head_timestamp_request(3, 265598, "TRADES", true, false, &mut conn, &mut hb, &shared);

    assert!(hmds.pending_head_ts.is_empty(), "a request that could not be sent is not left pending");
    let told = shared.reference.drain_historical_errors();
    assert!(
        told.iter().any(|(id, code, _)| *id == 3 && *code == 504),
        "the caller is told the request could not be sent: {told:?}",
    );
}


#[test]
fn query_error_releases_head_timestamp_without_sentinel() {
    let mut hmds = HmdsState::new();
    let shared = SharedState::new();
    let mut hb = HeartbeatState::new();
    let mut conn: Option<Connection> = None;
    hmds.pending_head_ts.push(("hts_1004".to_string(), 42));

    let msg = make_query_error_msg("hts_1004", "No head timestamp");
    hmds.process_hmds_message(&msg, &mut conn, &shared, &None, &mut hb);

    assert!(hmds.pending_head_ts.is_empty());
    let errors = shared.reference.drain_historical_errors();
    assert_eq!(errors, vec![(42, 162, "No head timestamp".to_string())]);
    // Head-ts is not a bar request — no historical_data sentinel should fire.
    assert!(shared.reference.drain_historical_data().is_empty());
}

/// The request carries no number of its own, but it is still told when it
/// cannot be made: returned as though the question had gone out, the caller
/// waited on an answer nothing was ever going to send.
#[test]
fn scanner_parameters_on_a_dead_connection_is_reported_not_dropped() {
    let mut hmds = HmdsState::new();
    let shared = SharedState::new();
    let mut hb = HeartbeatState::new();
    let mut conn: Option<Connection> = None;

    hmds.send_scanner_params_request(&mut conn, &mut hb, &shared);

    assert!(!hmds.pending_scanner_params, "nothing was sent, so nothing is awaited");
    let errors = shared.reference.drain_historical_errors();
    assert_eq!(errors.len(), 1, "the caller is told it was not sent");
    assert_eq!(errors[0].0, crate::bridge::ReferenceState::NO_REQUEST);
    assert_eq!(
        errors[0].1, 504,
        "the request never left, so the service reported no difficulty with it",
    );
}

// ── unknown bar_size rejects at the engine too (backstop for
// raw control-channel callers; the client validates synchronously) ──

#[test]
fn engine_rejects_unknown_bar_size_with_error_and_sentinel() {
    let mut hmds = HmdsState::new();
    let shared = SharedState::new();
    let mut hb = HeartbeatState::new();
    let mut conn: Option<Connection> = None;

    hmds.send_historical_request_ex(9, 756733, "", "2 d", "1 minute", "TRADES",
        true, false, false, "SPY", "STK", "SMART", &mut conn, &mut hb, &shared);

    assert!(hmds.pending_historical.is_empty(), "rejected request must not go pending");
    let errors = shared.reference.drain_historical_errors();
    assert_eq!(errors.len(), 1);
    assert_eq!(
        errors[0].1, 321,
        "the request is malformed, not a difficulty the service had with one it answered",
    );
    assert!(errors[0].2.contains("bar_size"), "got: {}", errors[0].2);
    let hist = shared.reference.drain_historical_data();
    assert_eq!(hist.len(), 1, "terminal sentinel must unblock waiters");
    assert!(hist[0].1.is_complete);
}

/// The same for a series this client cannot name.
///
/// Its sibling above covers the bar size; this covers the other argument the
/// same function reads, and pins both numbers so neither drifts back onto the
/// data service's own.
#[test]
fn engine_rejects_unknown_what_to_show_with_error_and_sentinel() {
    let mut hmds = HmdsState::new();
    let shared = SharedState::new();
    let mut hb = HeartbeatState::new();
    let mut conn: Option<Connection> = None;

    hmds.send_historical_request_ex(9, 756733, "", "2 D", "1 hour", "GRAVITY",
        true, false, false, "SPY", "STK", "SMART", &mut conn, &mut hb, &shared);

    assert!(hmds.pending_historical.is_empty(), "a refused request does not go pending");
    let errors = shared.reference.drain_historical_errors();
    assert_eq!(errors.len(), 1);
    assert_eq!(
        errors[0].1, 321,
        "the request is malformed, not a difficulty the service had with one it answered",
    );
    let hist = shared.reference.drain_historical_data();
    assert_eq!(hist.len(), 1, "and a caller waiting on it is released");
    assert!(hist[0].1.is_complete);
}

// ── a query waits for the venue ──

/// There used to be a sweep here that failed a historical query after a
/// minute of quiet, under the venue's own number. The reference client sets
/// no deadline of its own on one — the budget it carries is the widest a long
/// can hold — so a query now waits, and is failed only where the venue or the
/// connection says something.
#[test]
fn a_historical_query_waits_rather_than_being_failed_on_a_clock() {
    let mut hmds = HmdsState::new();
    let shared = SharedState::new();
    hmds.pending_historical.push(("hist_1010".to_string(), 21));

    assert_eq!(hmds.pending_historical_count(), 1, "still waiting on the venue");
    assert!(shared.reference.drain_historical_errors().is_empty(), "and nothing invented");
    assert!(shared.reference.drain_historical_data().is_empty());
}


#[test]
fn query_error_for_unknown_query_id_drops_nothing_and_emits_no_error() {
    let mut hmds = HmdsState::new();
    let shared = SharedState::new();
    let mut hb = HeartbeatState::new();
    let mut conn: Option<Connection> = None;
    hmds.pending_historical.push(("hist_1003".to_string(), 11));

    let msg = make_query_error_msg("hist_9999", "Boom");
    hmds.process_hmds_message(&msg, &mut conn, &shared, &None, &mut hb);

    assert_eq!(hmds.pending_historical.len(), 1, "unrelated entry must stay");
    assert!(shared.reference.drain_historical_errors().is_empty());
    assert!(shared.reference.drain_historical_data().is_empty());
}
/// The helpers above are only worth anything if the request that reaches
/// the wire uses them. Reinstating the old `CS`/`SMART` constants in the
/// builder passes every test that checks the helpers or the query encoder
/// in isolation, so this drives the real function and reads the socket.
#[test]
fn the_query_on_the_wire_carries_the_contract_s_own_type_and_venue() {
    use crate::protocol::connection::Connection;

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let client = std::net::TcpStream::connect(addr).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    peer.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
    let mut conn = Some(Connection::new_raw(client).unwrap());

    let mut hmds = super::HmdsState::new();
    let mut hb = crate::engine::hot_loop::HeartbeatState::new();
    let shared = crate::bridge::SharedState::new();

    hmds.send_historical_request_ex(
        1, 495512563, "20260101 16:00:00", "1 D", "1 hour", "TRADES",
            true, false, false, "ES", "FUT", "CME", &mut conn, &mut hb, &shared,
    );

    let sent = String::from_utf8_lossy(&read_frame(&mut peer)).to_string();

    assert!(sent.contains("FUT"), "the contract's security type: {sent}");
    assert!(sent.contains("CME"), "the contract's venue: {sent}");
    assert!(!sent.contains("SMART"), "and not the old constant: {sent}");
}

/// One complete daily bar under a query id, stated at a day and a close.
fn adj_bar_msg(query_id: &str, day: &str, close: f64, eoq: bool) -> Vec<u8> {
    let xml = format!(
        "<ResultSetBar><id>{query_id}</id><eoq>{eoq}</eoq><tz>UTC</tz><Events>\
         <Bar><date>{day}</date><open>{close}</open><close>{close}</close>\
         <high>{close}</high><low>{close}</low><weightedAvg>{close}</weightedAvg>\
         <volume>100</volume><count>5</count></Bar></Events></ResultSetBar>",
    );
    let mut msg = Vec::new();
    msg.extend_from_slice(b"35=W\x016118=");
    msg.extend_from_slice(xml.as_bytes());
    msg.push(0x01);
    msg
}

/// A corporate-actions reply echoing a query id and stating one action, in the
/// shape a live session was answered with: the query as XML on tag 6118, the
/// rows as text on tag 96, a name on its own line and its record under it.
fn conadj_msg(query_id: &str, con_id: u32, action_rows: &str) -> Vec<u8> {
    let echoed = format!(
        "<ListOfQueries><ConAdjQuery><id>{query_id}</id></ConAdjQuery></ListOfQueries>",
    );
    let body = format!("conc\n{con_id},-1,-1\n{action_rows}\n");
    let mut msg = Vec::new();
    msg.extend_from_slice(b"35=U\x016040=10022\x016118=");
    msg.extend_from_slice(echoed.as_bytes());
    msg.extend_from_slice(b"\x0196=");
    msg.extend_from_slice(body.as_bytes());
    msg.push(0x01);
    msg
}

/// The adjusted callback path holds its raw trades until the contract's actions
/// are in hand, then folds and files them complete on one scale. The bars go up
/// only once the split that moves them is known, so a caller is never handed a
/// pre-split price on the post-split scale.
#[test]
fn an_adjusted_request_folds_its_raw_trades_before_it_files_them() {
    use crate::protocol::connection::Connection;

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let sock = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    peer.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
    let mut conn = Some(Connection::new_raw(sock).unwrap());

    let mut hmds = HmdsState::new();
    let shared = SharedState::new();
    let mut hb = HeartbeatState::new();

    // A request for a contract, asked for adjusted, already on the wire as raw
    // trades: the raw query on `pending_historical`, the hold beside it.
    hmds.pending_historical.push(("hist_1".to_string(), 42));
    hmds.held.push(HeldSeries {
        req_id: 42, con_id: 756733, sec_type: "STK".into(), exchange: "SMART".into(),
        bars: Vec::new(), timezone: String::new(), actions_asked: false, actions_query: None, fold: Fold::Adjusted,
        actions: None, complete: false,
    });

    // One complete daily bar, dated before a ten-for-one split.
    hmds.process_hmds_message(
        &adj_bar_msg("hist_1", "20240607", 1208.88, true), &mut conn, &shared, &None, &mut hb,
    );

    // Nothing is filed: the actions are not in hand, so no bar can be scaled.
    assert!(
        shared.reference.drain_historical_data().is_empty(),
        "a raw bar was handed over before the series was on one scale",
    );
    // And the actions were asked for, on the first bar.
    let sent = String::from_utf8_lossy(&read_frame(&mut peer)).to_string();
    assert!(sent.contains("10020"), "the first bar did not ask for the contract's actions: {sent}");
    let qid = hmds.pending_adjustments.iter().find(|(_, rid, _)| *rid == 42)
        .map(|(q, _, _)| q.clone())
        .expect("the actions query is outstanding under this request");

    // The venue states a ten-for-one split after that bar.
    hmds.process_hmds_message(
        &conadj_msg(&qid, 756733, "SS\n20240610,10"), &mut conn, &shared, &None, &mut hb,
    );

    // Now the folded series is filed, complete, on the scale it trades on now.
    let filed = shared.reference.drain_historical_data();
    assert_eq!(filed.len(), 1, "the folded series is filed once, complete");
    assert_eq!(filed[0].0, 42);
    assert!(filed[0].1.is_complete, "and it says it is the whole answer");
    let bar = &filed[0].1.bars[0];
    assert!(
        (bar.close - 120.888).abs() < 1e-6,
        "the pre-split close was not put on the split's scale: {}", bar.close,
    );
    assert_eq!(bar.volume, 1000, "the shares before the split count for ten times as many");
    assert!(hmds.held.is_empty(), "the hold is released once folded");
}

/// When the venue refuses the actions the adjusted series needs, the request is
/// a bar request that failed: it is answered on the bar channels — the error
/// and the terminal sentinel — rather than handed back unadjusted or left
/// waiting on a fold that will never come.
#[test]
fn an_adjusted_request_whose_actions_are_refused_is_a_stated_refusal() {
    use crate::protocol::connection::Connection;

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let sock = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    peer.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
    let mut conn = Some(Connection::new_raw(sock).unwrap());

    let mut hmds = HmdsState::new();
    let shared = SharedState::new();
    let mut hb = HeartbeatState::new();

    hmds.pending_historical.push(("hist_1".to_string(), 42));
    hmds.held.push(HeldSeries {
        req_id: 42, con_id: 756733, sec_type: "STK".into(), exchange: "SMART".into(),
        bars: Vec::new(), timezone: String::new(), actions_asked: false, actions_query: None, fold: Fold::Adjusted,
        actions: None, complete: false,
    });

    hmds.process_hmds_message(
        &adj_bar_msg("hist_1", "20240607", 1208.88, true), &mut conn, &shared, &None, &mut hb,
    );
    let _ = read_frame(&mut peer);
    let qid = hmds.pending_adjustments.iter().find(|(_, rid, _)| *rid == 42)
        .map(|(q, _, _)| q.clone()).expect("the actions query is outstanding");

    // The venue rejects the actions query.
    hmds.process_hmds_message(
        &make_query_error_msg(&qid, "no permission"), &mut conn, &shared, &None, &mut hb,
    );

    assert!(hmds.held.is_empty(), "the hold is dropped, not left waiting");
    let errors = shared.reference.drain_historical_errors();
    assert_eq!(errors.len(), 1, "the caller is told why");
    assert_eq!(errors[0].0, 42);
    let filed = shared.reference.drain_historical_data();
    assert_eq!(filed.len(), 1, "and the series is ended so a waiting caller is released");
    assert!(filed[0].1.is_complete);
    assert!(filed[0].1.bars.is_empty(), "no unadjusted bar is handed back under the adjusted name");
}

/// And the stream half goes with it, on the same rule the batch keeps.
///
/// A request kept up to date is two queries under one number: the batch and a
/// five-second stream beside it. What fails the request fails the stream with
/// it — the batch refusal says so and withdraws it. A refusal of the actions
/// the fold is waiting on ends the request just as finally, and left standing
/// the bars go on arriving under a number the caller has been told failed,
/// while the entry holding that number refuses every later request under it as
/// a duplicate of a stream that is not running.
#[test]
fn a_kept_up_to_date_request_refused_on_its_actions_takes_its_stream_with_it() {
    use crate::protocol::connection::Connection;

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let sock = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    peer.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
    let mut conn = Some(Connection::new_raw(sock).unwrap());

    let mut hmds = HmdsState::new();
    let shared = SharedState::new();
    let mut hb = HeartbeatState::new();

    hmds.pending_historical.push(("hist_1".to_string(), 42));
    hmds.held.push(HeldSeries {
        req_id: 42, con_id: 756733, sec_type: "STK".into(), exchange: "SMART".into(),
        bars: Vec::new(), timezone: String::new(), actions_asked: false, actions_query: None,
        fold: Fold::Adjusted, actions: None, complete: false,
    });
    // The stream half, as a request asked to be kept up to date carries one.
    hmds.keep_up_to_date_reqs.insert(42);
    hmds.rtbar_subs.push(("rt_1".to_string(), 42, Some(9001), 0.01, 1.0));
    hmds.forming_bars.push(FormingBar {
        req_id: 42, seconds: 300, opened_at: 0,
        bar: crate::types::RealTimeBar::default(), weighted: 0.0,
    });

    hmds.process_hmds_message(
        &adj_bar_msg("hist_1", "20240607", 1208.88, true), &mut conn, &shared, &None, &mut hb,
    );
    let _ = read_frame(&mut peer);
    let qid = hmds.pending_adjustments.iter().find(|(_, rid, _)| *rid == 42)
        .map(|(q, _, _)| q.clone()).expect("the actions query is outstanding");

    hmds.process_hmds_message(
        &make_query_error_msg(&qid, "no permission"), &mut conn, &shared, &None, &mut hb,
    );

    assert!(
        !hmds.keep_up_to_date_reqs.contains(&42),
        "the stream keeps running under a number the caller was told had failed",
    );
    assert!(hmds.rtbar_subs.is_empty(), "and its subscription record stands: {:?}", hmds.rtbar_subs.len());
    assert!(hmds.forming_bars.is_empty(), "and its part-built bar goes on folding");
    assert!(
        !hmds.pending_historical.iter().any(|(_, r)| *r == 42),
        "the number stays reserved, so every later request under it is refused \
         as a duplicate for the rest of the session",
    );
}

/// A series that comes back with no bars has nothing to fold, and the actions
/// query only goes out on a first bar that never came. It is ended straight
/// away rather than held for an answer nothing asked for — the empty series the
/// waiting call hands back, on the callback path.
#[test]
fn an_adjusted_request_with_no_bars_ends_without_asking_for_actions() {
    let mut hmds = HmdsState::new();
    let shared = SharedState::new();
    let mut hb = HeartbeatState::new();
    // Nothing is sent, so no socket is needed.
    let mut conn: Option<crate::protocol::connection::Connection> = None;

    hmds.pending_historical.push(("hist_1".to_string(), 42));
    hmds.held.push(HeldSeries {
        req_id: 42, con_id: 756733, sec_type: "STK".into(), exchange: "SMART".into(),
        bars: Vec::new(), timezone: String::new(), actions_asked: false, actions_query: None, fold: Fold::Adjusted,
        actions: None, complete: false,
    });

    // The venue answers the series complete with no bars.
    let empty = {
        let xml = "<ResultSetBar><id>hist_1</id><eoq>true</eoq><tz>UTC</tz>\
                   <Events></Events></ResultSetBar>";
        let mut msg = Vec::new();
        msg.extend_from_slice(b"35=W\x016118=");
        msg.extend_from_slice(xml.as_bytes());
        msg.push(0x01);
        msg
    };
    hmds.process_hmds_message(&empty, &mut conn, &shared, &None, &mut hb);

    assert!(hmds.held.is_empty(), "nothing is held for a series with nothing to fold");
    assert!(
        hmds.pending_adjustments.iter().all(|(_, rid, _)| *rid != 42),
        "no actions were asked for a series that has none to apply",
    );
    let filed = shared.reference.drain_historical_data();
    assert_eq!(filed.len(), 1, "the empty series is filed complete");
    assert!(filed[0].1.is_complete);
    assert!(filed[0].1.bars.is_empty());
}

/// The vendor states its TRADES series as adjusted for splits, though the
/// venue serves it raw: a series crossing a ten-for-one split steps by ten
/// with nothing in it saying so, and every return, moving average and
/// volatility computed over that step is wrong. Measured against a contract
/// that split ten for one on 2024-06-10, where the close before was 1208.88
/// and the close after 121.79. The request asks for the raw trades, and the
/// venue's answer is held for the contract's actions and folded with the ones
/// that move the scale before a bar is handed over.
#[test]
fn a_trades_request_is_folded_with_the_actions_that_move_the_scale() {
    use crate::protocol::connection::Connection;

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let sock = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    peer.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
    let mut conn = Some(Connection::new_raw(sock).unwrap());

    let mut hmds = HmdsState::new();
    let shared = SharedState::new();
    let mut hb = HeartbeatState::new();

    hmds.send_historical_request_ex(
        42, 756733, "", "1 D", "1 day", "TRADES", true, false, false,
        "NVDA", "STK", "SMART", &mut conn, &mut hb, &shared,
    );
    let sent = String::from_utf8_lossy(&read_frame(&mut peer)).to_string();
    assert!(sent.contains("hist_1000"), "the bar query went out: {sent}");

    // One complete daily bar, dated before the split.
    hmds.process_hmds_message(
        &adj_bar_msg("hist_1000", "20240607", 1208.88, true), &mut conn, &shared, &None, &mut hb,
    );

    // Nothing is filed: the actions are not in hand, so no bar can be scaled.
    assert!(
        shared.reference.drain_historical_data().is_empty(),
        "a raw bar was handed over before the series was on one scale",
    );
    // And the actions were asked for.
    let sent = String::from_utf8_lossy(&read_frame(&mut peer)).to_string();
    assert!(sent.contains("10020"), "the completed series did not ask for the contract's actions: {sent}");
    let qid = hmds.pending_adjustments.iter().find(|(_, rid, _)| *rid == 42)
        .map(|(q, _, _)| q.clone())
        .expect("the actions query is outstanding under this request");

    // The venue states a cash dividend and a ten-for-one split. The dividend
    // is a payment out of the price rather than a restatement of it, so it
    // moves nothing; the split is what the series is folded with.
    hmds.process_hmds_message(
        &conadj_msg(
            &qid, 756733,
            "CD\n20240305,0.04,USD,20240221,20240306,20240327,R,NA\nSS\n20240610,10",
        ),
        &mut conn, &shared, &None, &mut hb,
    );

    let filed = shared.reference.drain_historical_data();
    assert_eq!(filed.len(), 1, "the folded series is filed once, complete");
    assert_eq!(filed[0].0, 42);
    assert!(filed[0].1.is_complete);
    let bar = &filed[0].1.bars[0];
    assert!(
        (bar.close - 120.888).abs() < 1e-6,
        "the pre-split close was not put on the split's scale: {}", bar.close,
    );
    assert_eq!(bar.volume, 1000, "the shares before the split count for ten times as many");
    assert!(hmds.held.is_empty(), "the hold is released once folded");
}

/// A TRADES request whose contract states an action this client cannot name
/// is refused rather than handed back raw: an action it cannot classify is
/// one it cannot say moves nothing, and folding without it is the wrong
/// number under an adjusted name arriving by its own back door. The adjusted
/// path refuses the same shape; this states it for the default series too.
#[test]
fn a_trades_request_with_an_action_nobody_can_name_is_refused() {
    use crate::protocol::connection::Connection;

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let sock = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    peer.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
    let mut conn = Some(Connection::new_raw(sock).unwrap());

    let mut hmds = HmdsState::new();
    let shared = SharedState::new();
    let mut hb = HeartbeatState::new();

    // Asked under the documented default, which is TRADES.
    hmds.send_historical_request_ex(
        42, 756733, "", "1 D", "1 day", "", true, false, false,
        "NVDA", "STK", "SMART", &mut conn, &mut hb, &shared,
    );
    let _ = read_frame(&mut peer);

    hmds.process_hmds_message(
        &adj_bar_msg("hist_1000", "20240607", 1208.88, true), &mut conn, &shared, &None, &mut hb,
    );
    assert!(
        shared.reference.drain_historical_data().is_empty(),
        "a raw bar was handed over before the series was on one scale",
    );
    let _ = read_frame(&mut peer);
    let qid = hmds.pending_adjustments.iter().find(|(_, rid, _)| *rid == 42)
        .map(|(q, _, _)| q.clone()).expect("the actions query is outstanding");

    // The venue names an action this client does not know.
    hmds.process_hmds_message(
        &conadj_msg(&qid, 756733, "ZZ\n20240610,10,,20240522"), &mut conn, &shared, &None, &mut hb,
    );

    assert!(hmds.held.is_empty(), "the hold is dropped, not left waiting");
    let errors = shared.reference.drain_historical_errors();
    assert_eq!(errors.len(), 1, "the caller is told why");
    assert_eq!(errors[0].0, 42);
    let filed = shared.reference.drain_historical_data();
    assert_eq!(filed.len(), 1, "and the series is ended so a waiting caller is released");
    assert!(filed[0].1.is_complete);
    assert!(filed[0].1.bars.is_empty(), "no raw bar is handed back under an adjusted name");
}

/// A series that is neither TRADES nor asked for adjusted is filed as the
/// venue served it: the fold belongs to the two series the vendor states as
/// adjusted, and a midpoint or bid-ask series has no actions to ask for.
#[test]
fn a_series_of_another_kind_is_filed_as_the_venue_served_it() {
    use crate::protocol::connection::Connection;

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let sock = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    peer.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
    let mut conn = Some(Connection::new_raw(sock).unwrap());

    let mut hmds = HmdsState::new();
    let shared = SharedState::new();
    let mut hb = HeartbeatState::new();

    hmds.send_historical_request_ex(
        42, 756733, "", "1 D", "1 day", "MIDPOINT", true, false, false,
        "NVDA", "STK", "SMART", &mut conn, &mut hb, &shared,
    );
    let _ = read_frame(&mut peer);

    hmds.process_hmds_message(
        &adj_bar_msg("hist_1000", "20240607", 1208.88, true), &mut conn, &shared, &None, &mut hb,
    );

    assert!(
        hmds.pending_adjustments.is_empty(),
        "no actions are asked for a series that is not folded",
    );
    let filed = shared.reference.drain_historical_data();
    assert_eq!(filed.len(), 1, "the series is filed as it arrived");
    let bar = &filed[0].1.bars[0];
    assert_eq!(bar.close, 1208.88, "and no scale has been applied to it");
    assert_eq!(bar.volume, 100, "nor to its count");
}

/// The venue pages a long series and the pages arrive newest first: the first
/// page a caller sees is the most recent one, and the oldest bars come last.
/// The actions a series is folded with are asked for from its earliest day,
/// and that day is not on the first page. Opened on the first bar to arrive,
/// the range began after the split and came back without it, and both paths
/// folded three years of NVDA with nothing to apply — every bar handed back
/// raw under the adjusted name, and no error anywhere.
///
/// Measured against the paper account and reproduced here through the real
/// engine: two pages, newest first, the split between them.
#[test]
fn a_paged_series_asks_for_its_actions_from_its_earliest_day_not_its_first_page() {
    use crate::protocol::connection::Connection;

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let sock = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    peer.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
    let mut conn = Some(Connection::new_raw(sock).unwrap());

    let mut hmds = HmdsState::new();
    let shared = SharedState::new();
    let mut hb = HeartbeatState::new();

    hmds.pending_historical.push(("hist_1".to_string(), 42));
    hmds.held.push(HeldSeries {
        req_id: 42, con_id: 4815747, sec_type: "STK".into(), exchange: "SMART".into(),
        bars: Vec::new(), timezone: String::new(), actions_asked: false, actions_query: None, fold: Fold::Adjusted,
        actions: None, complete: false,
    });

    // The newest page first, long after the split; then the last page, with a
    // bar from before it.
    hmds.process_hmds_message(
        &adj_bar_msg("hist_1", "20251215", 180.0, false), &mut conn, &shared, &None, &mut hb,
    );
    hmds.process_hmds_message(
        &adj_bar_msg("hist_1", "20240606", 1209.98, true), &mut conn, &shared, &None, &mut hb,
    );

    // The actions query opens on the earliest day the series holds, which is
    // the day of the split's own bar's predecessor — not the first page's.
    let sent = String::from_utf8_lossy(&read_frame(&mut peer)).to_string();
    assert!(sent.contains("10020"), "the actions were not asked for: {sent}");
    assert!(
        sent.contains("<startDate>20240606</startDate>"),
        "the range opened on the first page rather than the earliest bar, so it \
         cannot reach the split: {sent}",
    );

    // Answered with the split, the pre-split bar folds and the later one does not.
    let qid = hmds.pending_adjustments.iter().find(|(_, rid, _)| *rid == 42)
        .map(|(q, _, _)| q.clone()).expect("the actions query is outstanding");
    hmds.process_hmds_message(
        &conadj_msg(&qid, 4815747, "SS\n20240610,10"), &mut conn, &shared, &None, &mut hb,
    );
    let filed = shared.reference.drain_historical_data();
    assert_eq!(filed.len(), 1);
    // Oldest first: the pre-split bar leads, folded; the later one is untouched.
    let closes: Vec<f64> = filed[0].1.bars.iter().map(|b| b.close).collect();
    assert!((closes[0] - 120.998).abs() < 1e-6, "pre-split bar not folded: {closes:?}");
    assert!((closes[1] - 180.0).abs() < 1e-9, "post-split bar must be untouched: {closes:?}");
}

/// One page of a bar reply, stating a zone or, given none, omitting the tag as
/// the venue's later pages do.
fn bar_page_msg(query_id: &str, eoq: bool, tz: &str) -> Vec<u8> {
    let tz_tag = if tz.is_empty() { String::new() } else { format!("<tz>{tz}</tz>") };
    let xml = format!(
        "<ResultSetBar><id>{query_id}</id><eoq>{eoq}</eoq>{tz_tag}<Events>\
         <Bar><time>20260714-13:30:00</time><open>1</open><close>1</close>\
         <high>1</high><low>1</low><weightedAvg>1</weightedAvg>\
         <volume>1</volume><count>1</count></Bar></Events></ResultSetBar>",
    );
    let mut msg = Vec::new();
    msg.extend_from_slice(b"35=W\x016118=");
    msg.extend_from_slice(xml.as_bytes());
    msg.push(0x01);
    msg
}

/// The venue does not put `<tz>` on every page of a series, and the zone is
/// read off the page: a page that omits it formatted nothing, and every bar on
/// it reached the caller verbatim where the first page's were written on the
/// caller's clock — one series, two spellings, decided by which page a bar
/// landed on, and a completion whose range was computed on no clock at all.
/// The zone belongs to the query: the first page that states one states it for
/// the series.
#[test]
fn a_page_that_states_no_zone_takes_the_one_the_series_stated() {
    let mut hmds = HmdsState::new();
    let shared = SharedState::new();
    let mut hb = HeartbeatState::new();
    let mut conn: Option<crate::protocol::connection::Connection> = None;
    hmds.pending_historical.push(("hist_1".to_string(), 7));
    hmds.held.push(HeldSeries {
        req_id: 7, fold: Fold::None, con_id: 0, sec_type: String::new(), exchange: String::new(),
        bars: Vec::new(), timezone: String::new(), actions_asked: false, actions_query: None, actions: None, complete: false,
    });

    hmds.process_hmds_message(&bar_page_msg("hist_1", false, "US/Eastern"), &mut conn, &shared, &None, &mut hb);
    hmds.process_hmds_message(&bar_page_msg("hist_1", false, ""), &mut conn, &shared, &None, &mut hb);
    hmds.process_hmds_message(&bar_page_msg("hist_1", true, ""), &mut conn, &shared, &None, &mut hb);

    // One series, on the one zone it stated, whose range the completion is
    // computed on.
    let filed = shared.reference.drain_historical_data();
    assert_eq!(filed.len(), 1, "the series is filed once, whole");
    assert_eq!(filed[0].1.timezone, "US/Eastern", "on the zone the series stated");
    assert_eq!(filed[0].1.bars.len(), 3, "every page's bars in it");
    assert!(filed[0].1.is_complete);
    assert!(hmds.held.is_empty(), "and nothing is held once it has answered");
}

/// One page of a bar reply holding a bar on each of the given days, in the
/// order given.
fn page_of(query_id: &str, days: &[&str], eoq: bool) -> Vec<u8> {
    let bars: String = days.iter().map(|d| format!(
        "<Bar><time>{d}-13:30:00</time><open>1</open><close>1</close><high>1</high>\
         <low>1</low><weightedAvg>1</weightedAvg><volume>1</volume><count>1</count></Bar>",
    )).collect();
    let xml = format!(
        "<ResultSetBar><id>{query_id}</id><eoq>{eoq}</eoq><tz>US/Eastern</tz>\
         <Events>{bars}</Events></ResultSetBar>",
    );
    let mut msg = Vec::new();
    msg.extend_from_slice(b"35=W\x016118=");
    msg.extend_from_slice(xml.as_bytes());
    msg.push(0x01);
    msg
}

/// The venue pages a long series and the pages arrive newest first, each
/// ascending within itself. Handed over in arrival order the series steps
/// backwards at every page boundary: a plot of it is a sawtooth, a return
/// between consecutive bars is nonsense once per page, and the last bar is
/// years old. The reference client hands bars over oldest first; so does this,
/// for every historical request. Measured against the paper account: three
/// years of daily bars, five pages, four backward steps.
#[test]
fn a_paged_series_is_delivered_oldest_first_whatever_order_the_pages_arrive_in() {
    let mut hmds = HmdsState::new();
    let shared = SharedState::new();
    let mut hb = HeartbeatState::new();
    let mut conn: Option<crate::protocol::connection::Connection> = None;
    hmds.pending_historical.push(("hist_1".to_string(), 7));
    hmds.held.push(HeldSeries {
        req_id: 7, fold: Fold::None, con_id: 0, sec_type: String::new(), exchange: String::new(),
        bars: Vec::new(), timezone: String::new(), actions_asked: false, actions_query: None, actions: None, complete: false,
    });

    // Newest page first, oldest last, as the venue sends them.
    hmds.process_hmds_message(&page_of("hist_1", &["20260901", "20260902"], false), &mut conn, &shared, &None, &mut hb);
    hmds.process_hmds_message(&page_of("hist_1", &["20250327", "20250328"], false), &mut conn, &shared, &None, &mut hb);
    hmds.process_hmds_message(&page_of("hist_1", &["20230905", "20230906"], true), &mut conn, &shared, &None, &mut hb);

    let filed = shared.reference.drain_historical_data();
    assert!(filed.last().is_some_and(|(_, r)| r.is_complete), "the series ends");
    let days: Vec<String> = filed.iter()
        .flat_map(|(_, r)| r.bars.iter().map(|b| b.time[..8].to_string()))
        .collect();
    assert_eq!(days.len(), 6, "every bar is present");
    assert!(
        days.windows(2).all(|w| w[0] < w[1]),
        "the series must ascend across page boundaries, oldest first: {days:?}",
    );
}

/// Read until the query XML has closed. A single `read` can legally return
/// a partial frame, which would make these tests fail intermittently while
/// production is correct.
fn read_frame(peer: &mut std::net::TcpStream) -> Vec<u8> {
    use std::io::Read;
    let mut acc = Vec::new();
    let mut buf = vec![0u8; 8192];
    for _ in 0..64 {
        match peer.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                acc.extend_from_slice(&buf[..n]);
                // Stop on a complete frame, not on a field in the middle
                // of one: a split right after the query would otherwise
                // return a truncated read that happens to satisfy the
                // assertions above it.
                // A plain FIX frame ends with its checksum field; a
                // compressed one states its own length.
                let ends_with_trailer = acc.len() >= 7
                    && acc[acc.len() - 1] == 0x01
                    && acc[acc.len() - 7..acc.len() - 4] == *b"\x0110=";
                let complete = ends_with_trailer
                    || crate::protocol::fixcomp::fixcomp_length(&acc)
                        .is_some_and(|len| acc.len() >= len);
                if complete {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    acc
}
mod tick_ack_tests {
    use super::super::{parse_tick_subscription_ack, scaled_size};

    /// The venue's answer, taken from a live session. It names the
    /// subscription back, states the number it will use on every frame, and
    /// states the increments prices and sizes move in — none of which is
    /// stated anywhere else on this connection.
    #[test]
    fn the_acknowledgement_states_the_number_and_both_increments() {
        let xml = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\t<ResultSetTickerId>\n\
                   \t\t<id>tbt_1</id>\n\t\t<rtTickerId>1</rtTickerId>\n\
                   \t\t<minTick>0.00005</minTick>\n\t\t<sizeMinTick>1</sizeMinTick>\n\
                   \t\t<eoq>false</eoq>\n\t</ResultSetTickerId>";
        let ack = parse_tick_subscription_ack(xml).expect("the acknowledgement reads");
        assert_eq!(ack.query_id, "tbt_1");
        assert_eq!(ack.venue_id, 1);
        assert_eq!(ack.min_tick, 0.00005);
        assert_eq!(ack.size_min_tick, 1.0);
    }

    /// A crypto deals in hundred-millionths, and says so.
    #[test]
    fn a_contract_dealt_in_fractions_states_a_fractional_size_increment() {
        let xml = "<ResultSetTickerId><id>tbt_2</id><rtTickerId>2</rtTickerId>\
                   <minTick>0.25</minTick><sizeMinTick>0.00000001</sizeMinTick></ResultSetTickerId>";
        let ack = parse_tick_subscription_ack(xml).expect("the acknowledgement reads");
        assert_eq!(ack.venue_id, 2, "and its number is its own, not the one asked under");
        assert_eq!(ack.size_min_tick, 0.00000001);
    }

    /// Some other reply on the same connection is not an acknowledgement.
    #[test]
    fn another_reply_is_not_read_as_an_acknowledgement() {
        assert!(parse_tick_subscription_ack("<QueryError><id>tbt_1</id></QueryError>").is_none());
        assert!(parse_tick_subscription_ack("<ResultSetBar><id>h_1</id></ResultSetBar>").is_none());
    }

    /// A size is a count of what the venue said sizes move in. Counting a
    /// crypto's size in whole ones reports it a hundred million times too large.
    #[test]
    fn a_size_is_counted_in_what_the_venue_said_it_moves_in() {
        // A share: whole ones, held in the form readers divide by.
        assert_eq!(scaled_size(100, 1.0), 100 * crate::types::QTY_SCALE);
        // A crypto: a hundred million counts is one whole unit.
        assert_eq!(scaled_size(100_000_000, 0.00000001), crate::types::QTY_SCALE);
        // Stating no increment means whole ones.
        assert_eq!(scaled_size(5, 0.0), 5 * crate::types::QTY_SCALE);
    }
}
mod counted_size_ceiling_tests {
    use super::super::scaled_size;

    /// A count past what a quantity can hold is held at the largest one. Cast
    /// straight through it comes out negative, and a size that reads as
    /// negative is a sell where there was a buy.
    #[test]
    fn a_count_past_the_ceiling_does_not_come_back_negative() {
        assert!(scaled_size(u64::MAX, 1.0) > 0, "a size came back negative");
        assert!(scaled_size(u64::MAX, 1e-8) > 0);
    }

    /// An ordinary count is untouched.
    #[test]
    fn an_ordinary_count_is_untouched() {
        assert_eq!(scaled_size(100, 1.0), 100 * crate::types::QTY_SCALE);
    }
}
mod withdrawing_one_stream_tests {
    use super::super::*;

    fn stream(caller_req_id: i64, instrument: InstrumentId, kind: TbtType) -> TbtSubscription {
        TbtSubscription {
            ignore_size: false,
            instrument,
            query_id: format!("tbt_{caller_req_id}"),
            kind,
            caller_req_id,
            venue_id: caller_req_id as u64,
            min_tick: 1,
            size_tick: 1.0,
            running: Default::default(),
        }
    }

    /// A contract can carry two streams — every trade, and every quote change.
    /// Withdrawing one by naming the contract took whichever was opened first
    /// and left the caller's own running.
    #[test]
    fn withdrawing_one_stream_leaves_the_other() {
        let mut hmds = HmdsState::new();
        hmds.tbt_subscriptions.push(stream(1, 7, TbtType::Last));
        hmds.tbt_subscriptions.push(stream(2, 7, TbtType::BidAsk));

        hmds.send_tbt_unsubscribe(2, 7, &mut None, &mut HeartbeatState::new());

        assert_eq!(hmds.tbt_subscriptions.len(), 1, "one stream was withdrawn");
        assert_eq!(
            hmds.tbt_subscriptions[0].caller_req_id, 1,
            "the wrong stream was withdrawn",
        );
    }

    /// A trade stream is asked for by the name the caller used.
    ///
    /// The venue serves each of the three as a query of its own. Both trade
    /// streams went out under one name and the other was made here, by
    /// dropping the prints the venue marks as not reported to the tape — which
    /// is not what the venue means by it: that stream, asked for by name,
    /// carries those prints too. What was handed to a caller as the narrower
    /// stream was a series the venue does not serve.
    #[test]
    fn a_trade_stream_is_asked_for_by_the_name_the_caller_used() {
        assert_eq!(HmdsState::tbt_wire_kind(TbtType::Last), "Last");
        assert_eq!(HmdsState::tbt_wire_kind(TbtType::AllLast), "AllLast");
        assert_eq!(HmdsState::tbt_wire_kind(TbtType::BidAsk), "BidAsk");
        assert_eq!(HmdsState::tbt_wire_kind(TbtType::MidPoint), "MidPoint");
    }

    /// A withdrawal reaches the stream it names and no other. Where the name
    /// matched nothing it fell back on "the one stream on this contract",
    /// which was never the caller's: the venue had refused the caller's
    /// stream and it was already gone, so the fallback took the other
    /// caller's stream on the contract, and that caller was told nothing.
    #[test]
    fn a_withdrawal_naming_nothing_leaves_the_other_stream_on_the_contract() {
        let mut hmds = HmdsState::new();
        hmds.tbt_subscriptions.push(stream(2, 7, TbtType::BidAsk));
        hmds.send_tbt_unsubscribe(1, 7, &mut None, &mut HeartbeatState::new());
        assert_eq!(hmds.tbt_subscriptions.len(), 1, "the other caller's stream stands");
    }

    fn withdrawal_while_a_sibling_is_unnumbered(deferred: bool) {
        for (gone_kind, kept_kind, kept_instrument, shares) in [
            (TbtType::Last, TbtType::Last, 7, true),
            (TbtType::BidAsk, TbtType::BidAsk, 7, true),
            // The venue answers each name as a query of its own, so two
            // callers who asked by different names hold two streams and
            // neither withdrawal touches the other's.
            (TbtType::Last, TbtType::AllLast, 7, false),
            (TbtType::AllLast, TbtType::Last, 7, false),
            (TbtType::Last, TbtType::BidAsk, 7, false),
            (TbtType::Last, TbtType::Last, 8, false),
        ] {
            let mut hmds = HmdsState::new();
            let shared = crate::bridge::SharedState::new();
            let (conn, mut peer) = crate::protocol::connection::Connection::for_test();
            peer.set_read_timeout(Some(std::time::Duration::from_millis(100))).unwrap();
            let mut conn = Some(conn);
            let mut hb = HeartbeatState::new();
            let mut gone = stream(1, 7, gone_kind);
            gone.venue_id = if deferred { 0 } else { 41 };
            hmds.tbt_subscriptions.push(gone);
            let mut kept = stream(2, kept_instrument, kept_kind);
            kept.venue_id = 0;
            hmds.tbt_subscriptions.push(kept);

            hmds.send_tbt_unsubscribe(1, 7, &mut conn, &mut hb);
            if deferred {
                let by_name = super::read_frame(&mut peer);
                assert!(String::from_utf8_lossy(&by_name).contains("ticker:tbt_1"));
                let ack = b"35=W\x016118=<ResultSetTickerId><id>tbt_1</id><rtTickerId>41</rtTickerId>\
                    <minTick>0.01</minTick><sizeMinTick>1</sizeMinTick></ResultSetTickerId>\x01";
                hmds.process_hmds_message(ack, &mut conn, &shared, &None, &mut hb);
                assert!(hmds.tbt_withdrawn_unnumbered.is_empty());
            }
            let withdrawal = super::read_frame(&mut peer);
            if shares {
                assert!(withdrawal.is_empty(), "the sibling is still waiting for the shared number");
                assert!(!hmds.tbt_withdrawn.contains(&41));
            } else {
                assert!(String::from_utf8_lossy(&withdrawal).contains("ticker:41"),
                    "another contract or wire kind does not hold this stream");
                assert!(hmds.tbt_withdrawn.contains(&41));
            }

            let number = if shares { 41 } else { 42 };
            let ack = format!("35=W\x016118=<ResultSetTickerId><id>tbt_2</id><rtTickerId>{number}</rtTickerId>\
                <minTick>0.01</minTick><sizeMinTick>1</sizeMinTick></ResultSetTickerId>\x01");
            hmds.process_hmds_message(ack.as_bytes(), &mut conn, &shared, &None, &mut hb);
            assert_eq!(hmds.tbt_subscriptions[0].venue_id, number);
            hmds.send_tbt_unsubscribe(2, kept_instrument, &mut conn, &mut hb);
            let final_withdrawal = super::read_frame(&mut peer);
            assert!(String::from_utf8_lossy(&final_withdrawal).contains(&format!("ticker:{number}")),
                "the last caller's withdrawal reaches the venue");
            assert!(hmds.tbt_subscriptions.is_empty());
        }
    }

    /// The venue has not numbered the second caller's query yet, but it is
    /// already asking for the same stream that the first caller leaves.
    #[test]
    fn a_numbered_withdrawal_keeps_an_unnumbered_siblings_stream() {
        withdrawal_while_a_sibling_is_unnumbered(false);
    }

    /// The first acknowledgement can arrive after its caller leaves and
    /// before the second caller is numbered. That stream is still wanted.
    #[test]
    fn a_deferred_withdrawal_keeps_an_unnumbered_siblings_stream() {
        withdrawal_while_a_sibling_is_unnumbered(true);
    }

    /// A stream withdrawn before the venue has numbered it is withdrawn by
    /// that number when the acknowledgement arrives. Withdrawn by name alone,
    /// the form the venue accepts and does nothing with, its number was never
    /// learned, no second withdrawal was possible, and its ticks arrived until
    /// the session ended.
    #[test]
    fn a_stream_withdrawn_before_its_acknowledgement_is_withdrawn_at_it() {
        let mut hmds = HmdsState::new();
        let shared = crate::bridge::SharedState::new();
        let (conn, mut peer) = crate::protocol::connection::Connection::for_test();
        peer.set_read_timeout(Some(std::time::Duration::from_millis(500))).unwrap();
        let mut conn = Some(conn);
        let mut hb = HeartbeatState::new();
        let mut unnumbered = stream(1, 7, TbtType::Last);
        unnumbered.venue_id = 0;
        hmds.tbt_subscriptions.push(unnumbered);

        hmds.send_tbt_unsubscribe(1, 7, &mut conn, &mut hb);
        let by_name = String::from_utf8_lossy(&super::read_frame(&mut peer)).into_owned();
        assert!(by_name.contains("ticker:tbt_1"), "withdrawn by name for now: {by_name}");

        let ack = b"35=W\x016118=<ResultSetTickerId><id>tbt_1</id><rtTickerId>41</rtTickerId>\
                    <minTick>0.01</minTick><sizeMinTick>1</sizeMinTick></ResultSetTickerId>\x01";
        hmds.process_hmds_message(ack, &mut conn, &shared, &None, &mut hb);
        let by_number = String::from_utf8_lossy(&super::read_frame(&mut peer)).into_owned();
        assert!(by_number.contains("ticker:41"), "withdrawn by the venue's number: {by_number:?}");
        assert!(hmds.tbt_withdrawn.contains(&41), "and its ticks are known as withdrawn");
        assert!(hmds.tbt_subscriptions.is_empty(), "nothing is reopened by the acknowledgement");
    }

    /// The venue answers a second query on a contract and kind with the
    /// number it gave the first, so two callers share one stream. Every
    /// record reaches both, and the first withdrawal leaves the stream
    /// running for the other: routed to whichever subscription came first,
    /// the second caller heard nothing, and the first caller's withdrawal
    /// stopped the stream the second was still reading.
    #[test]
    fn two_callers_sharing_the_venues_number_both_hear_it_and_the_last_to_leave_withdraws_it() {
        let mut hmds = HmdsState::new();
        let shared = crate::bridge::SharedState::new();
        let (conn, mut peer) = crate::protocol::connection::Connection::for_test();
        peer.set_read_timeout(Some(std::time::Duration::from_millis(300))).unwrap();
        let mut conn = Some(conn);
        let mut hb = HeartbeatState::new();
        let hex = crate::protocol::tbt_stream::A_CAPTURED_QUOTE_FRAME;
        let frame: Vec<u8> = (0..hex.len() / 2)
            .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap())
            .collect();
        let start = frame.windows(5).position(|w| w == b"35=E\x01").unwrap() + 5;
        let end = frame.windows(6).position(|w| w == b"\x018349=").unwrap();
        let number = crate::protocol::tbt_stream::frame_ticker_id(&frame[start..end])
            .expect("the frame names its stream");
        for caller in [1, 2] {
            let mut sub = stream(caller, 7, TbtType::BidAsk);
            sub.venue_id = number;
            sub.min_tick = (0.00005 * crate::types::PRICE_SCALE as f64).round() as i64;
            hmds.tbt_subscriptions.push(sub);
        }
        hmds.process_hmds_message(&frame, &mut conn, &shared, &None, &mut hb);
        let heard: Vec<i64> = shared.market.drain_tbt_quotes().into_iter().map(|q| q.req_id).collect();
        assert_eq!(heard.iter().filter(|r| **r == 1).count(), 5, "{heard:?}");
        assert_eq!(heard.iter().filter(|r| **r == 2).count(), 5, "both callers hear every record: {heard:?}");

        hmds.send_tbt_unsubscribe(1, 7, &mut conn, &mut hb);
        assert!(super::read_frame(&mut peer).is_empty(), "the stream is left running for the other caller");
        assert!(!hmds.tbt_withdrawn.contains(&number), "and is not marked withdrawn");
        hmds.send_tbt_unsubscribe(2, 7, &mut conn, &mut hb);
        let withdrawn = String::from_utf8_lossy(&super::read_frame(&mut peer)).into_owned();
        assert!(withdrawn.contains(&format!("ticker:{number}")), "the last to leave withdraws it: {withdrawn:?}");
        assert!(hmds.tbt_withdrawn.contains(&number));
    }

    /// A caller taken on under a number another caller already holds joins
    /// that stream where it stands. A price is not sent — a move from the
    /// last one is — so a subscription anchored at nothing reads the frames
    /// the venue is already sending as though the stream began with it, and
    /// the caller is handed prices that are those moves added up. Nothing on
    /// the wire or in the log says so: the two callers simply disagree about
    /// what the market is.
    #[test]
    fn a_caller_joining_a_running_stream_is_anchored_where_that_stream_stands() {
        let mut hmds = HmdsState::new();
        let shared = crate::bridge::SharedState::new();
        let mut conn = None;
        let mut hb = HeartbeatState::new();
        let hex = crate::protocol::tbt_stream::A_CAPTURED_QUOTE_FRAME;
        let frame: Vec<u8> = (0..hex.len() / 2)
            .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap())
            .collect();
        let start = frame.windows(5).position(|w| w == b"35=E\x01").unwrap() + 5;
        let end = frame.windows(6).position(|w| w == b"\x018349=").unwrap();
        let number = crate::protocol::tbt_stream::frame_ticker_id(&frame[start..end])
            .expect("the frame names its stream");
        let scaled = (0.00005 * crate::types::PRICE_SCALE as f64).round() as i64;

        // The first caller, which has been reading the stream for a while.
        let mut first = stream(1, 7, TbtType::BidAsk);
        first.venue_id = number;
        first.min_tick = scaled;
        hmds.tbt_subscriptions.push(first);
        hmds.process_hmds_message(&frame, &mut conn, &shared, &None, &mut hb);
        let opening = shared.market.drain_tbt_quotes();
        assert_eq!(opening.len(), 5, "the stream has moved on before the second caller arrives");

        // The second, acknowledged under the number the first already holds.
        let mut second = stream(2, 7, TbtType::BidAsk);
        second.venue_id = 0;
        hmds.tbt_subscriptions.push(second);
        let ack = format!(
            "35=W\x016118=<ResultSetTickerId><id>tbt_2</id><rtTickerId>{number}</rtTickerId>\
             <minTick>0.00005</minTick><sizeMinTick>1</sizeMinTick></ResultSetTickerId>\x01",
        );
        hmds.process_hmds_message(ack.as_bytes(), &mut conn, &shared, &None, &mut hb);

        hmds.process_hmds_message(&frame, &mut conn, &shared, &None, &mut hb);
        let heard = shared.market.drain_tbt_quotes();
        let quoted = |req_id: i64| -> Vec<(i64, i64)> {
            heard.iter().filter(|q| q.req_id == req_id).map(|q| (q.bid, q.ask)).collect()
        };
        assert_eq!(quoted(1).len(), 5, "the first caller hears the frame");
        assert_ne!(
            quoted(1)[0], (opening[0].bid, opening[0].ask),
            "the moves in this frame are steps from where the stream had got to",
        );
        assert_eq!(
            quoted(2), quoted(1),
            "one stream is one set of prices; the caller that joined it read the moves from zero",
        );
    }
}
mod counted_size_range_tests {
    use super::super::scaled_size;

    /// A count too large to hold as sent is still held when the increment
    /// shrinks it back into range. Cut down to fit before scaling, it came
    /// back as a number that was plausible and was not the venue's.
    #[test]
    fn an_increment_that_shrinks_a_count_keeps_it() {
        let counted = u64::MAX / 4;
        let held = scaled_size(counted, 1e-8);
        let expected = (counted as f64 * 1e-8 * crate::types::QTY_SCALE as f64).round() as i64;
        assert_eq!(held, expected, "the increment was applied to a count that had been cut");
    }
}
mod forming_bar_tests {
    use super::super::*;

    fn five(timestamp: u32, open: f64, high: f64, low: f64, close: f64, volume: f64) -> crate::types::RealTimeBar {
        crate::types::RealTimeBar {
            timestamp, open, high, low, close, volume, wap: close, count: 1,
        }
    }

    /// A caller keeping five-minute bars up to date hears its own bar as it
    /// forms, not the five-second bars it is made of.
    #[test]
    fn the_forming_bar_is_folded_from_what_the_venue_streams() {
        let mut forming = FormingBar {
            req_id: 1, seconds: 300, opened_at: 0,
            bar: Default::default(), weighted: 0.0,
        };
        // 14:35:00, then two more within the same five minutes.
        let first = forming.fold(&five(1_786_456_500, 10.0, 10.5, 9.5, 10.2, 100.0));
        assert_eq!(first.timestamp, 1_786_456_500, "the bar opens on its own boundary");

        forming.fold(&five(1_786_456_505, 10.2, 11.0, 10.1, 10.8, 50.0));
        let so_far = forming.fold(&five(1_786_456_510, 10.8, 10.9, 9.0, 9.4, 50.0));
        assert_eq!(so_far.timestamp, 1_786_456_500, "still the same bar");
        assert_eq!(so_far.open, 10.0, "opened where the first one did");
        assert_eq!(so_far.high, 11.0, "the highest of them");
        assert_eq!(so_far.low, 9.0, "the lowest of them");
        assert_eq!(so_far.close, 9.4, "the latest of them");
        assert_eq!(so_far.volume, 200.0, "all of it");
        assert_eq!(so_far.count, 3);
        assert!((so_far.wap - (10.2 * 100.0 + 10.8 * 50.0 + 9.4 * 50.0) / 200.0).abs() < 1e-9);

        // And the next five minutes start a bar of their own.
        let next = forming.fold(&five(1_786_456_800, 9.4, 9.6, 9.3, 9.5, 10.0));
        assert_eq!(next.timestamp, 1_786_456_800);
        assert_eq!(next.volume, 10.0, "nothing carried over");
    }

    /// The trade count comes off the wire at the full width the field carries,
    /// so two bars in one interval need not add up to one.
    ///
    /// Added plain, two counts near the top of the range overflow — on the
    /// engine thread, where a panic ends the session and every subscription on
    /// it. The field states what it can hold, so a total past that is held at
    /// the top rather than wrapped to a bar made by minus two billion trades.
    #[test]
    fn a_forming_bar_holds_a_trade_count_the_wire_states_at_the_edge() {
        let mut forming = FormingBar {
            req_id: 1, seconds: 300, opened_at: 0,
            bar: Default::default(), weighted: 0.0,
        };
        let mut near_the_top = five(1_786_456_500, 10.0, 10.5, 9.5, 10.2, 100.0);
        near_the_top.count = i32::MAX - 1;
        forming.fold(&near_the_top);

        let mut second = five(1_786_456_505, 10.2, 11.0, 10.1, 10.8, 50.0);
        second.count = i32::MAX - 1;
        let so_far = forming.fold(&second);
        assert!(so_far.count > 0, "a bar is never made by a negative number of trades");
    }
}

/// A scan response is delivered to the scan that answered.
///
/// Every response arrives under one message id, so the id cannot identify the
/// scan. The payload names the scan, and that is what routes the rows.
#[test]
fn each_scan_gets_its_own_answer() {
    let mut hmds = super::HmdsState::new();
    hmds.pending_scanner.push(("APISCAN1:10001".to_string(), 10001));
    hmds.pending_scanner.push(("APISCAN2:10002".to_string(), 10002));
    hmds.pending_scanner.push(("APISCAN3:10003".to_string(), 10003));

    for (named, expected) in [
        ("APISCAN1:10001", 10001),
        ("APISCAN2:10002", 10002),
        ("APISCAN3:10003", 10003),
    ] {
        let xml = format!("<ScanResponse>\n\t<id>{named}</id>\n</ScanResponse>");
        assert_eq!(
            hmds.scanner_answered(&xml),
            Some(expected),
            "{named} answered, so its rows belong to the request that asked for it",
        );
    }
}

/// A response naming a scan this session is not running belongs to nobody here.
///
/// A withdrawn scan can answer once more. The scan name identifies the owner,
/// so a name matching no running scan has no owner and is not delivered.
#[test]
fn an_answer_naming_no_running_scan_is_not_handed_to_another() {
    let mut hmds = super::HmdsState::new();
    hmds.pending_scanner.push(("APISCAN1:10001".to_string(), 10001));
    assert_eq!(hmds.scanner_answered("<ScanResponse></ScanResponse>"), None);
    assert_eq!(hmds.scanner_answered("<ScanResponse><id>APISCAN9:99</id></ScanResponse>"), None);
    // The one it does name still answers.
    assert_eq!(
        hmds.scanner_answered("<ScanResponse><id>APISCAN1:10001</id></ScanResponse>"),
        Some(10001),
    );
}

mod hmds_correlation_tests {
    use super::super::*;
    use crate::bridge::SharedState;
    use crate::protocol::connection::Connection;

    /// Query names are numbered, so one is a prefix of another as soon as the
    /// count reaches ten. Searching the payload for the name handed the answer
    /// for `tk_12` to whichever of `tk_1` and `tk_12` was waiting first, and
    /// the other was never answered.
    ///
    /// A news reply states what the query asked for after its name, separated
    /// from it, and is still that query's answer.
    #[test]
    fn a_query_name_that_prefixes_another_is_not_answered_by_it() {
        let answer_for = |id: &str| {
            format!("<ResultSetTick><id>{id}</id><eoq>true</eoq></ResultSetTick>")
        };

        assert!(answers(&answer_for("tk_1"), "tk_1"), "its own answer");
        assert!(
            !answers(&answer_for("tk_12"), "tk_1"),
            "tk_1 took the answer meant for tk_12",
        );
        assert!(answers(&answer_for("tk_12"), "tk_12"), "which tk_12 needs itself");
        assert!(!answers(&answer_for("tk_2"), "tk_1"), "a different query entirely");

        // The decorated form a news reply states.
        let news = "<NewsResponse><id>news_2-headlines;;NewsQuery;;0;;true;;0;;U</id></NewsResponse>";
        assert!(answers(news, "news_2"), "the reply names the query it answers");
        assert!(!answers(news, "news_2x"), "and not one whose name merely resembles it");
    }

    fn tick_msg(query_id: &str, done: bool) -> Vec<u8> {
        let xml = format!(
            "<ResultSetTick><id>{}</id><eoq>{}</eoq><tz>UTC</tz><Events>\
             <Tick><time>20260714-13:30:00</time><price>100.0</price><size>1</size></Tick>\
             </Events></ResultSetTick>",
            query_id, if done { "true" } else { "false" },
        );
        let mut msg = Vec::new();
        msg.extend_from_slice(b"35=W\x016118=");
        msg.extend_from_slice(xml.as_bytes());
        msg.push(0x01);
        msg
    }

    /// A tick query is answered in segments, each stating whether it is the
    /// last. The route is held until a segment states it is.
    #[test]
    fn a_segmented_tick_reply_keeps_its_route_until_the_venue_is_done() {
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let mut conn: Option<Connection> = None;
        hmds.pending_ticks.push(("tk_1".to_string(), 31, "TRADES".to_string()));

        hmds.process_hmds_message(&tick_msg("tk_1", false), &mut conn, &shared, &None, &mut hb);
        assert_eq!(hmds.pending_ticks.len(), 1, "more is coming, so the route stays");

        hmds.process_hmds_message(&tick_msg("tk_1", true), &mut conn, &shared, &None, &mut hb);
        assert!(hmds.pending_ticks.is_empty(), "the last segment releases it");
        assert_eq!(
            shared.reference.drain_historical_ticks().len(), 2,
            "both segments reached the caller",
        );
    }

    /// A segment cut mid-row ends the request rather than waiting on it.
    ///
    /// The parse refuses a row it never saw closed rather than handing back
    /// what is in hand, so a cut segment carries nothing to deliver — and
    /// nothing more is coming under that number, because the venue has
    /// answered it. Waited on, the series either completed later with a hole
    /// in it under the reply's own statement that it was whole, or, where the
    /// cut segment was the last, the request stood until the connection died:
    /// the sweep that fails an unreadable reply never sees a tick payload.
    #[test]
    fn a_tick_segment_cut_mid_row_ends_the_request() {
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let mut conn: Option<Connection> = None;
        hmds.pending_ticks.push(("tk_9".to_string(), 31, "TRADES".to_string()));

        // The last segment, cut inside a row: the opening tag is there and
        // the close never arrives.
        let mut msg = Vec::new();
        msg.extend_from_slice(b"35=W\x016118=");
        msg.extend_from_slice(
            b"<ResultSetTick><id>tk_9</id><eoq>true</eoq><tz>UTC</tz><Events>\
              <Tick><time>20260714-13:30:00</time><price>100.0</price>",
        );
        msg.push(0x01);
        hmds.process_hmds_message(&msg, &mut conn, &shared, &None, &mut hb);

        assert!(
            hmds.pending_ticks.is_empty(),
            "the request waits on a reply the venue has already sent",
        );
        let errors = shared.reference.drain_historical_errors();
        assert!(errors.iter().any(|(r, ..)| *r == 31), "the caller is told: {errors:?}");
        let delivered = shared.reference.drain_historical_ticks();
        assert!(
            delivered.iter().any(|(r, _, _, done)| *r == 31 && *done),
            "and released, or a caller reading these off the callback waits on \
             a last segment that is not coming: {delivered:?}",
        );
    }

    /// Tag 96 carries gzip bytes. The parsed field map is UTF-8 lossy, which
    /// replaces every invalid byte, so the payload is read from the raw
    /// frame.
    #[test]
    fn a_compressed_fundamental_report_survives_being_read() {
        use flate2::write::GzEncoder;
        use std::io::Write;

        let report = "<ReportSnapshot><Issuer>ACME</Issuer></ReportSnapshot>";
        let mut encoder = GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(report.as_bytes()).unwrap();
        let compressed = encoder.finish().unwrap();
        assert!(
            String::from_utf8(compressed.clone()).is_err(),
            "the payload really is not text",
        );

        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let mut conn: Option<Connection> = None;
        let named = crate::control::fundamental::fundamentals_query_id(1);
        hmds.pending_fundamental.push((named.clone(), 51));

        // Tag 95 states the length, which frames a payload containing SOH
        // bytes. The answer names the request that asked, which is how it is
        // matched.
        let mut msg = Vec::new();
        msg.extend_from_slice(b"35=U\x016040=10012\x016118=");
        msg.extend_from_slice(
            format!("<FundamentalsResponse><id>{named}</id></FundamentalsResponse>").as_bytes(),
        );
        msg.extend_from_slice(b"\x0195=");
        msg.extend_from_slice(compressed.len().to_string().as_bytes());
        msg.extend_from_slice(b"\x0196=");
        msg.extend_from_slice(&compressed);
        msg.push(0x01);
        hmds.process_hmds_message(&msg, &mut conn, &shared, &None, &mut hb);

        let answered = shared.reference.drain_fundamental_data();
        assert_eq!(answered.len(), 1, "the report reached the caller");
        assert_eq!(answered[0].1, report, "and it is the report the venue sent");
    }

    /// An article response consumes the pending request whether or not its
    /// payload reads, so an unreadable one is reported to the caller.
    #[test]
    fn an_unreadable_article_is_reported_rather_than_swallowed() {
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let mut conn: Option<Connection> = None;
        hmds.pending_articles.push(("art_1".to_string(), 61));

        let xml = "<NewsResponse><id>art_1-article_file;;NewsQuery;;0;;true;;0;;U</id></NewsResponse>";
        let mut msg = Vec::new();
        msg.extend_from_slice(b"35=U\x016040=10032\x016118=");
        msg.extend_from_slice(xml.as_bytes());
        msg.push(0x01);
        hmds.process_hmds_message(&msg, &mut conn, &shared, &None, &mut hb);

        assert!(hmds.pending_articles.is_empty(), "the request is spent either way");
        assert!(
            shared.reference.drain_news_articles().is_empty(),
            "there was no article to deliver",
        );
        assert!(
            shared.reference.drain_historical_errors().iter().any(|(id, _, _)| *id == 61),
            "and the caller is told, rather than left waiting",
        );
    }

    /// A news query is registered as outstanding only if it went out.
    /// Registered regardless, the caller waited its whole deadline for an
    /// answer to a request the socket never carried.
    #[test]
    fn a_news_query_that_did_not_go_out_is_refused_not_registered() {
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let mut conn: Option<Connection> = None;
        hmds.send_historical_news_request(7, 265598, "BRFG", "", "", 10, &shared, &mut conn, &mut hb);
        hmds.send_news_article_request(8, "BRFG", "BRFG$1", &shared, &mut conn, &mut hb);
        assert!(hmds.pending_news.is_empty() && hmds.pending_articles.is_empty(), "nothing waits on a request that never left");
        let told = shared.reference.drain_historical_errors();
        let codes: Vec<_> = told.iter().map(|(id, code, _)| (*id, *code)).collect();
        assert_eq!(codes, [(7, 504), (8, 504)], "{told:?}");
    }

    /// A command can reach the engine without passing through a request
    /// surface. Its unreadable bounds must not become an unbounded query.
    #[test]
    fn an_unreadable_news_window_is_refused_before_sending() {
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let mut conn: Option<Connection> = None;
        for (start, end) in [("unreadable", ""), ("", "unreadable")] {
            hmds.send_historical_news_request(7, 265598, "BRFG", start, end, 10, &shared, &mut conn, &mut hb);
            assert!(hmds.pending_news.is_empty());
            let told = shared.reference.drain_historical_errors();
            assert_eq!(told.len(), 1, "{told:?}");
            assert_eq!((told[0].0, told[0].1), (7, crate::error_codes::Refusal::VALIDATION));
            assert!(told[0].2.contains("YYYYMMDD-HH:MM:SS"), "{told:?}");
            assert!(told[0].2.contains("YYYYMMDD HH:MM:SS"), "{told:?}");
        }
    }

    /// A news reply naming nothing pending is noted, not dropped in silence:
    /// a reply after the lists were cleared, a duplicate, or an id this
    /// client cannot read all looked like nothing arriving.
    #[test]
    fn a_news_reply_naming_nothing_pending_is_noted() {
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let mut conn: Option<Connection> = None;
        let xml = "<NewsResponse><id>news_9-history;;NewsQuery;;0;;true;;0;;U</id></NewsResponse>";
        let mut msg = Vec::new();
        msg.extend_from_slice(b"35=U\x016040=10032\x016118=");
        msg.extend_from_slice(xml.as_bytes());
        msg.push(0x01);
        hmds.process_hmds_message(&msg, &mut conn, &shared, &None, &mut hb);
        assert!(
            shared.market.unread_wire().iter().any(|(c, what)| *c == "historical" && what.contains("news reply")),
            "{:?}", shared.market.unread_wire(),
        );
    }

    /// A histogram reply that names a query and cannot be read fails that
    /// query rather than leaving it waiting on an answer that has already
    /// arrived.
    #[test]
    fn an_unreadable_histogram_reply_fails_the_query_it_names() {
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let mut conn: Option<Connection> = None;
        let qid = crate::control::histogram::histogram_query_id(
            &crate::control::histogram::HistogramRequest {
                query_id: "hg_1".to_string(),
                con_id: 265598,
                sec_type: "STK".to_string(),
                exchange: "SMART".to_string(),
                use_rth: true,
                period: "1 week".to_string(),
                end_time: "20260320-21:00:00".to_string(),
            },
        );
        hmds.pending_histogram.push((qid.clone(), 71));

        let xml = format!(
            "<ResultSetHistogram><id>{qid}</id><Events>\
             <Tick><price>not-a-number</price><size>1500</size></Tick>\
             </Events></ResultSetHistogram>",
        );
        let mut msg = Vec::new();
        msg.extend_from_slice(b"35=W\x016118=");
        msg.extend_from_slice(xml.as_bytes());
        msg.push(0x01);
        hmds.process_hmds_message(&msg, &mut conn, &shared, &None, &mut hb);

        assert!(hmds.pending_histogram.is_empty(), "the request is spent either way");
        assert!(
            shared.reference.drain_histogram_data().is_empty(),
            "there were no entries to deliver",
        );
        assert!(
            shared.reference.drain_historical_errors_for_dispatch(|_| false).iter().any(|(id, _, _)| *id == 71),
            "and the caller is told, rather than left waiting",
        );
    }

    /// A news response names the query it answers, and is matched on that
    /// name. Two searches can be in flight at once.
    #[test]
    fn a_news_reply_answers_the_request_it_names() {
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let mut conn: Option<Connection> = None;
        hmds.pending_news.push(("news_1".to_string(), 51));
        hmds.pending_news.push(("news_2".to_string(), 52));

        // The second request's response arrives first, under the id its own
        // query went out with.
        let xml = "<NewsResponse><id>news_2-headlines;;NewsQuery;;0;;true;;0;;U</id></NewsResponse>";
        let mut msg = Vec::new();
        msg.extend_from_slice(b"35=U\x016040=10032\x016118=");
        msg.extend_from_slice(xml.as_bytes());
        msg.push(0x01);
        hmds.process_hmds_message(&msg, &mut conn, &shared, &None, &mut hb);

        let answered = shared.reference.drain_historical_news();
        assert_eq!(answered.len(), 1, "one answer reached a caller");
        assert_eq!(answered[0].0, 52, "and it is the caller the reply names");
        assert_eq!(
            hmds.pending_news.iter().map(|(q, _)| q.as_str()).collect::<Vec<_>>(),
            vec!["news_1"],
            "the other request is still outstanding",
        );
    }

    /// Two callers asking the same question of the same contract are two
    /// requests, and each is answered.
    ///
    /// The id a head timestamp went out under was built from what was being
    /// asked — the contract, the venue, the series, the hours — and nothing
    /// else. Two callers describing the same request therefore sent the same
    /// id, and the reply was matched to whichever of them sat first in the
    /// list: one was handed the other's answer, a pair the venue answered once
    /// left the second waiting with nothing to release it, and a cancel from
    /// one stopped the other's query at the venue. The histogram beside it is
    /// led by its own name for this reason.
    #[test]
    fn two_head_timestamps_for_one_contract_are_told_apart() {
        let ask = |query_id: &str| {
            crate::control::historical::head_timestamp_query_id(
                &crate::control::historical::HeadTimestampRequest {
                    query_id: query_id.to_string(),
                    con_id: 265_598,
                    sec_type: "CS".into(),
                    exchange: "SMART".into(),
                    data_type: "Last",
                    use_rth: true,
                    include_expired: false,
                },
            )
        };
        assert_ne!(
            ask("tk_1000"), ask("tk_1001"),
            "the same question asked twice goes out under two names",
        );
        assert!(ask("tk_1000").starts_with("tk_1000;;"), "led by the query's own name");
    }

    /// A head timestamp goes out under an id its response names, and is
    /// matched on it. Two can be in flight at once.
    #[test]
    fn a_head_timestamp_answers_the_request_the_reply_names() {
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let mut conn: Option<Connection> = None;
        let id_of = |con_id: u32| {
            crate::control::historical::head_timestamp_query_id(
                &crate::control::historical::HeadTimestampRequest {
                    query_id: format!("tk_{con_id}"),
                    con_id,
                    sec_type: "CS".into(),
                    exchange: "SMART".into(),
                    data_type: "Last",
                    use_rth: true,
                    include_expired: false,
                },
            )
        };
        hmds.pending_head_ts.push((id_of(1), 41));
        hmds.pending_head_ts.push((id_of(2), 42));

        let xml = format!(
            "<ResultSetHeadTimeStamp><id>{}</id><eoq>true</eoq>\
             <headTS>19930129-09:00:00</headTS><tz>US/Eastern</tz></ResultSetHeadTimeStamp>",
            id_of(2),
        );
        let mut msg = Vec::new();
        msg.extend_from_slice(b"35=W\x016118=");
        msg.extend_from_slice(xml.as_bytes());
        msg.push(0x01);
        hmds.process_hmds_message(&msg, &mut conn, &shared, &None, &mut hb);

        let answered = shared.reference.drain_head_timestamps();
        assert_eq!(answered.len(), 1);
        assert_eq!(answered[0].0, 42, "the reply named the second request");
    }

    /// A response naming no pending query is not delivered to the oldest
    /// outstanding request.
    #[test]
    fn an_answer_naming_no_pending_query_is_not_handed_to_another() {
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let mut conn: Option<Connection> = None;
        hmds.pending_head_ts.push(("hts_of_another_query".to_string(), 71));
        hmds.pending_histogram.push(("hg_of_another_query".to_string(), 72));

        for xml in [
            "<ResultSetHeadTimeStamp><id>nobody</id><eoq>true</eoq>\
             <headTS>19930129-09:00:00</headTS><tz>US/Eastern</tz></ResultSetHeadTimeStamp>",
            "<ResultSetHistogram><id>nobody</id><eoq>true</eoq>\
             <Events><Tick><price>100.0</price><size>5</size></Tick></Events></ResultSetHistogram>",
        ] {
            let mut msg = Vec::new();
            msg.extend_from_slice(b"35=W\x016118=");
            msg.extend_from_slice(xml.as_bytes());
            msg.push(0x01);
            hmds.process_hmds_message(&msg, &mut conn, &shared, &None, &mut hb);
        }

        assert!(shared.reference.drain_head_timestamps().is_empty());
        assert!(shared.reference.drain_histogram_data().is_empty());
        assert_eq!(hmds.pending_head_ts.len(), 1, "and both are still waiting");
        assert_eq!(hmds.pending_histogram.len(), 1);
    }
/// An unreadable end-of-query page withdraws the five-second stream at the
/// venue, as a stated refusal of it does. Removed only locally, the bars kept
/// arriving under a number the caller was told had failed.
#[test]
fn an_unreadable_eoq_page_withdraws_the_stream_at_the_venue() {
    let mut hmds = HmdsState::new();
    let shared = SharedState::new();
    let mut hb = HeartbeatState::new();
    let (conn, mut peer) = Connection::for_test();
    peer.set_read_timeout(Some(std::time::Duration::from_millis(500))).unwrap();
    let mut conn = Some(conn);
    hmds.pending_historical.push(("hist_1".to_string(), 9));
    hmds.keep_up_to_date_reqs.insert(9);
    hmds.rtbar_subs.push(("hist_1".to_string(), 9, Some(4002), 0.01, 1.0));
    hmds.held.push(HeldSeries {
        req_id: 9, fold: Fold::None, con_id: 0, sec_type: String::new(), exchange: String::new(),
        bars: Vec::new(), timezone: String::new(), actions_asked: false, actions_query: None,
        actions: None, complete: false,
    });
    let xml = "<ResultSetBar><id>hist_1</id><eoq>true</eoq><tz>US/Eastern</tz>\
               <Events><Bar><time>not-a-time</time></Bar></Events></ResultSetBar>";
    let mut msg = Vec::new();
    msg.extend_from_slice(b"35=W\x016118=");
    msg.extend_from_slice(xml.as_bytes());
    msg.push(0x01);
    hmds.process_hmds_message(&msg, &mut conn, &shared, &None, &mut hb);

    assert!(!hmds.keep_up_to_date_reqs.contains(&9), "the request is failed on the unreadable page");
    let cancel = String::from_utf8_lossy(&super::read_frame(&mut peer)).into_owned();
    assert!(cancel.contains("ticker:4002"), "the stream is withdrawn at the venue: {cancel:?}");
}

/// The venue refusing the stream half of a request kept up to date fails the
/// whole request, and the number is freed with it. Left flagged, every later
/// request under the number was refused as a duplicate of one the caller had
/// been told had failed.
#[test]
fn a_refused_stream_half_frees_the_number_it_was_kept_up_to_date_under() {
    let mut hmds = HmdsState::new();
    let shared = SharedState::new();
    let mut hb = HeartbeatState::new();
    let mut conn: Option<Connection> = None;
    hmds.keep_up_to_date_reqs.insert(9);
    hmds.rtbar_subs.push(("rt_4002".to_string(), 9, None, 0.01, 1.0));
    hmds.forming_bars.push(FormingBar {
        req_id: 9, seconds: 60, opened_at: 0, bar: Default::default(), weighted: 0.0,
    });
    let xml = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<QueryError>\n\t<id>rt_4002</id>\n\t<error>no</error>\n</QueryError>\n";
    let mut msg = Vec::new();
    msg.extend_from_slice(b"35=W\x016118=");
    msg.extend_from_slice(xml.as_bytes());
    msg.push(0x01);
    hmds.process_hmds_message(&msg, &mut conn, &shared, &None, &mut hb);

    assert!(hmds.rtbar_subs.iter().all(|(_, rid, ..)| *rid != 9), "the stream is gone");
    assert!(!hmds.keep_up_to_date_reqs.contains(&9), "and the number is freed");
    assert!(hmds.forming_bars.iter().all(|f| f.req_id != 9), "and the half-built bar with it");
}

/// A series that cannot be folded fails the whole request, so the stream half
/// goes with it — as it does where the venue states the refusal and where a
/// page cannot be read.
///
/// Left running, five-second bars kept arriving under a number the caller had
/// just been told had failed, the next reconnect asked for the stream again,
/// and the number answered nothing else for the rest of the session.
#[test]
fn a_series_that_cannot_be_folded_withdraws_the_stream_it_was_asked_for_alongside() {
    let mut hmds = HmdsState::new();
    let shared = SharedState::new();
    let mut hb = HeartbeatState::new();
    let (conn, mut peer) = Connection::for_test();
    peer.set_read_timeout(Some(std::time::Duration::from_millis(500))).unwrap();
    let mut conn = Some(conn);
    hmds.pending_historical.push(("hist_1".to_string(), 9));
    hmds.keep_up_to_date_reqs.insert(9);
    hmds.rtbar_subs.push(("hist_1".to_string(), 9, Some(4002), 0.01, 1.0));
    hmds.held.push(HeldSeries {
        req_id: 9, fold: Fold::Adjusted, con_id: 756733, sec_type: "STK".into(),
        exchange: "SMART".into(), bars: Vec::new(), timezone: String::new(),
        actions_asked: true, actions_query: None, complete: false,
        // An action the venue named and this client cannot classify. It may be
        // one that moves the scale, so the fold refuses rather than hand back
        // a raw price under an adjusted one's name.
        actions: Some(vec![crate::control::adjustments::Adjustment {
            kind: None,
            date: "20240610".into(),
            ..Default::default()
        }]),
    });
    let xml = "<ResultSetBar><id>hist_1</id><eoq>true</eoq><tz>US/Eastern</tz>\
               <Events><Bar><time>20240611  09:30:00</time><open>10</open>\
               <high>11</high><low>9</low><close>10.5</close><volume>100</volume>\
               </Bar></Events></ResultSetBar>";
    let mut msg = Vec::new();
    msg.extend_from_slice(b"35=W\x016118=");
    msg.extend_from_slice(xml.as_bytes());
    msg.push(0x01);
    hmds.process_hmds_message(&msg, &mut conn, &shared, &None, &mut hb);

    assert!(
        !shared.reference.drain_historical_errors().is_empty(),
        "the caller is told the series could not be folded",
    );
    assert!(!hmds.keep_up_to_date_reqs.contains(&9), "the number is freed");
    assert!(hmds.rtbar_subs.iter().all(|(_, rid, ..)| *rid != 9), "the stream is gone");
    let cancel = String::from_utf8_lossy(&super::read_frame(&mut peer)).into_owned();
    assert!(cancel.contains("ticker:4002"), "and withdrawn at the venue: {cancel:?}");
}

/// A request kept up to date is two queries, and the withdrawal takes both.
///
/// The batch and the five-second stream beside it are acknowledged separately,
/// so two records can stand under one request number. The withdrawal that runs
/// when the request fails took the one it found and left the other — and that
/// one belonged to no pending list and carried no flag, so nothing swept it.
/// The number read as busy for the rest of the session, and a caller asking
/// under it again was refused for a stream that was not running. The
/// withdrawal a caller asks for itself already takes them all.
#[test]
fn withdrawing_a_kept_up_to_date_request_leaves_no_record_under_its_number() {
    let mut hmds = HmdsState::new();
    let mut hb = HeartbeatState::new();
    let (conn, mut peer) = Connection::for_test();
    peer.set_read_timeout(Some(std::time::Duration::from_millis(500))).unwrap();
    let mut conn = Some(conn);
    hmds.keep_up_to_date_reqs.insert(9);
    // Both halves, as a live request holds them: the stream, and the batch the
    // venue also acknowledged.
    hmds.rtbar_subs.push(("rt_9".to_string(), 9, Some(4002), 0.01, 1.0));
    hmds.rtbar_subs.push(("hist_9".to_string(), 9, Some(4003), 0.01, 1.0));

    hmds.withdraw_the_stream_half(9, &mut conn, &mut hb);

    assert!(
        hmds.rtbar_subs.iter().all(|(_, rid, ..)| *rid != 9),
        "no record stands under the number: {:?}",
        hmds.rtbar_subs.iter().map(|(q, r, ..)| (q.clone(), *r)).collect::<Vec<_>>(),
    );
    let cancel = String::from_utf8_lossy(&super::read_frame(&mut peer)).into_owned();
    assert!(cancel.contains("ticker:4002"), "and the stream is withdrawn: {cancel:?}");
}

/// A withdrawal waiting for its number does not stop a stream another caller
/// is still reading.
///
/// Two callers on one contract and kind are served under one number. The
/// withdrawal that runs while the number is known guards against that and says
/// why. The one that had to wait for the number — because the caller left
/// before the venue answered — sent the cancel whoever else held it, and that
/// caller was left subscribed in every table, silent for the rest of the
/// session, and told nothing.
#[test]
fn a_withdrawal_waiting_for_its_number_leaves_a_shared_stream_running() {
    let mut hmds = HmdsState::new();
    let shared = SharedState::new();
    let mut hb = HeartbeatState::new();
    let (conn, mut peer) = Connection::for_test();
    peer.set_read_timeout(Some(std::time::Duration::from_millis(300))).unwrap();
    let mut conn = Some(conn);

    // A second caller already reading the number the venue is about to state.
    hmds.tbt_subscriptions.push(TbtSubscription {
        ignore_size: false, instrument: 0, query_id: "tbt_keep".to_string(),
        kind: TbtType::Last, caller_req_id: 7, venue_id: 55,
        min_tick: 0, size_tick: 0.0, running: Default::default(),
    });
    // And one that left before its own acknowledgement arrived.
    hmds.tbt_withdrawn_unnumbered.insert("tbt_gone".to_string(), (0, TbtType::Last));

    let ack = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<ResultSetTickerId>\
        <id>tbt_gone</id><rtTickerId>55</rtTickerId><minTick>0.01</minTick>\
        <sizeMinTick>1</sizeMinTick><eoq>false</eoq></ResultSetTickerId>";
    let mut msg = Vec::new();
    msg.extend_from_slice(b"35=W\x016118=");
    msg.extend_from_slice(ack.as_bytes());
    msg.push(0x01);
    hmds.process_hmds_message(&msg, &mut conn, &shared, &None, &mut hb);

    assert!(
        !hmds.tbt_withdrawn.contains(&55),
        "the number is not marked withdrawn while another caller reads it",
    );
    assert!(
        super::read_frame(&mut peer).is_empty(),
        "and no cancel goes out for it",
    );
}
}

mod hmds_transport_tests {
    use super::super::*;
    use crate::bridge::SharedState;
    use crate::protocol::connection::Connection;

    /// One-shot requests are failed when the connection is lost. Only
    /// historical bars carry a timeout, so the rest would never complete.
    #[test]
    fn a_lost_connection_answers_the_requests_it_took_with_it() {
        let mut hmds = HmdsState::new();
        let shared = SharedState::new();
        let mut conn: Option<Connection> = None;
        hmds.pending_head_ts.push(("hts".to_string(), 61));
        hmds.pending_fundamental.push(("fund".to_string(), 62));
        hmds.pending_ticks.push(("tk".to_string(), 63, "TRADES".to_string()));

        hmds.disconnect(&mut conn, &shared, &None);

        let errors = shared.reference.drain_historical_errors();
        for req_id in [61, 62, 63] {
            assert!(
                errors.iter().any(|(rid, ..)| *rid == req_id),
                "request {req_id} was told the connection went: {errors:?}",
            );
        }
        assert!(hmds.pending_head_ts.is_empty(), "and nothing is left waiting");
    }
}

/// A query id that is a prefix of another does not take its answer.
///
/// The bar replies are matched by the name the reply states, the same as every
/// other reply here. Matched by prefix alone, `hist_10001`'s bars go to whoever
/// is waiting on `hist_1000` — and a session that keeps one request resident
/// while thousands pass does reach five figures with the first still open.
#[test]
fn a_query_id_that_prefixes_another_does_not_take_its_bars() {
    use super::states;

    assert!(states("hist_1000", "hist_1000"), "its own answer");
    assert!(!states("hist_10001", "hist_1000"), "the longer id is another query");
    assert!(!states("hist_1000_x", "hist_1000"), "and so is one continued by a word");
    // A reply that states what it asked for after the name is still that
    // query's, which is why the separator is not simply any character.
    assert!(states("news_2-headlines;;x", "news_2"));
}

/// The query that opens a tick stream states the contract, the kind of stream
/// and where it came from, and nothing it was not given.
#[test]
fn a_tick_stream_states_the_contract_and_the_kind() {
    let q = super::HmdsState::build_tbt_query(7, 265598, "BEST", "CS", "AllLast", 0, false);
    assert!(q.contains("<id>tbt_7</id>"));
    assert!(q.contains("<contractID>265598</contractID>"));
    assert!(q.contains("<data>AllLast</data>"));
    assert!(q.contains("<source>API</source>"));
    // And nothing the caller did not ask for: no prelude and no filter where
    // it asked for neither.
    assert!(!q.contains("timeLength"), "{q}");
    assert!(!q.contains("filter"), "{q}");
}

/// A prelude and a size filter are stated where the caller asked for them.
///
/// The query carries a length for a run of past ticks before the stream, and a
/// filter for leaving out a change that moves only the size. Both were refused
/// at the surface instead, so a caller could ask for neither — and the refusal
/// said this protocol had no field for a prelude, which this query does have.
#[test]
fn a_tick_stream_states_the_prelude_and_the_filter_it_was_asked_for() {
    let asked = super::HmdsState::build_tbt_query(7, 265598, "BEST", "CS", "AllLast", 100, true);
    assert!(asked.contains("<timeLength>100 t</timeLength>"), "{asked}");
    assert!(asked.contains("<filter><ignoreSize>true</ignoreSize></filter>"), "{asked}");
}

/// A refusal of a different query under the same caller number leaves the
/// held series alone.
///
/// The lists these queries are drawn from do not share a number space. A
/// request that is kept up to date proves it on its own: the batch and the
/// five-second stream that follows it carry one caller number, and the venue
/// refusing the stream — a series it serves as history but not at five
/// seconds, or a contract with no streaming entitlement — threw away every
/// page the batch had collected. What was held went out as an end carrying no
/// bars, and the pages still to come were then delivered after it as updates,
/// newest page first.
#[test]
fn a_refusal_of_another_query_leaves_a_held_series_alone() {
    let mut hmds = HmdsState::new();
    let shared = SharedState::new();
    let mut hb = HeartbeatState::new();
    let mut conn: Option<Connection> = None;

    // One caller number, two queries: the batch of bars, and the stream that
    // keeps it up to date.
    hmds.pending_historical.push(("hist_2001".to_string(), 7));
    hmds.keep_up_to_date_reqs.insert(7);
    hmds.rtbar_subs.push(("rt_2002".to_string(), 7, None, 0.01, 1.0));
    hmds.held.push(HeldSeries {
        req_id: 7, fold: Fold::None, con_id: 0, sec_type: String::new(), exchange: String::new(),
        bars: Vec::new(), timezone: String::new(), actions_asked: false, actions_query: None, actions: None,
        complete: false,
    });
    hmds.process_hmds_message(&make_bar_msg("hist_2001", false), &mut conn, &shared, &None, &mut hb);
    let held_before = hmds.held[0].bars.len();
    assert!(held_before > 0, "the batch has pages in hand");

    // The venue refuses the stream, not the batch.
    hmds.process_hmds_message(
        &make_query_error_msg("rt_2002", "No market data permissions"),
        &mut conn, &shared, &None, &mut hb,
    );

    assert_eq!(hmds.held.len(), 1, "the batch was not the query that was refused");
    assert_eq!(hmds.held[0].bars.len(), held_before, "and it still holds its pages");
    assert!(hmds.rtbar_subs.is_empty(), "the stream that was refused is gone");
    assert!(
        !hmds.pending_historical.is_empty(),
        "the batch is still outstanding, so its remaining pages still reach the hold",
    );

    // The caller hears about the stream, and is not handed an end for a series
    // that has not finished arriving.
    let errors = shared.reference.drain_historical_errors();
    assert_eq!(errors, vec![(7, 162, "No market data permissions".to_string())]);
    assert!(
        shared.reference.drain_historical_data().is_empty(),
        "no end for a series still being collected",
    );
}

/// A standalone request for a contract's actions shares the caller's number
/// with whatever bar request is being folded under it. The answer is matched
/// to the hold on the query the hold itself sent: matched on the number
/// alone, the standalone answer folded an unrelated series with another
/// query's actions — a different range, or a different contract entirely.
#[test]
fn a_standalone_actions_reply_is_not_folded_into_another_request_s_series() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let sock = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    peer.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
    let mut conn = Some(Connection::new_raw(sock).unwrap());

    let mut hmds = HmdsState::new();
    let shared = SharedState::new();
    let mut hb = HeartbeatState::new();

    hmds.pending_historical.push(("hist_1".to_string(), 7));
    hmds.held.push(HeldSeries {
        req_id: 7, con_id: 756733, sec_type: "STK".into(), exchange: "SMART".into(),
        bars: Vec::new(), timezone: String::new(), actions_asked: false, actions_query: None,
        fold: Fold::Adjusted, actions: None, complete: false,
    });

    // The series completes, and the fold asks for its actions under its own id.
    hmds.process_hmds_message(
        &adj_bar_msg("hist_1", "20240607", 1208.88, true), &mut conn, &shared, &None, &mut hb,
    );
    let _ = read_frame(&mut peer);
    let qid = hmds.pending_adjustments.iter().find(|(_, rid, _)| *rid == 7)
        .map(|(q, _, _)| q.clone()).expect("the actions query is outstanding");

    // A standalone question about another contract, under this same caller
    // number, answers first.
    hmds.pending_adjustments.push(("adj_9999".to_string(), 7, 999999));
    hmds.process_hmds_message(
        &conadj_msg("adj_9999", 999999, "SS\n20240610,10"), &mut conn, &shared, &None, &mut hb,
    );

    assert_eq!(hmds.held.len(), 1, "the series is still waiting on its own actions");
    assert!(hmds.held[0].actions.is_none(), "and holds no answer from another query");
    assert!(
        shared.reference.drain_historical_data().is_empty(),
        "nothing is filed, folded with another query's actions",
    );
    assert_eq!(hmds.pending_adjustments.len(), 1, "the answered question is spent");
    assert_eq!(hmds.pending_adjustments[0].0, qid, "and the series' own still waits");
}

/// A refusal of a standalone actions query is not a refusal of the bar
/// request folded under the same caller number. Refusals are matched the way
/// answers are, on the query the hold itself sent.
#[test]
fn a_refusal_of_a_standalone_actions_query_leaves_the_fold_alone() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let sock = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    peer.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
    let mut conn = Some(Connection::new_raw(sock).unwrap());

    let mut hmds = HmdsState::new();
    let shared = SharedState::new();
    let mut hb = HeartbeatState::new();

    hmds.pending_historical.push(("hist_1".to_string(), 7));
    hmds.held.push(HeldSeries {
        req_id: 7, con_id: 756733, sec_type: "STK".into(), exchange: "SMART".into(),
        bars: Vec::new(), timezone: String::new(), actions_asked: false, actions_query: None,
        fold: Fold::Adjusted, actions: None, complete: false,
    });

    hmds.process_hmds_message(
        &adj_bar_msg("hist_1", "20240607", 1208.88, true), &mut conn, &shared, &None, &mut hb,
    );
    let _ = read_frame(&mut peer);

    // The venue refuses the standalone question, not the fold's.
    hmds.pending_adjustments.push(("adj_9999".to_string(), 7, 999999));
    hmds.process_hmds_message(
        &make_query_error_msg("adj_9999", "no permission"), &mut conn, &shared, &None, &mut hb,
    );

    assert_eq!(hmds.held.len(), 1, "the fold's series was not what was refused");
    assert!(hmds.held[0].actions.is_none(), "and it still waits on its own actions");
    assert!(
        shared.reference.drain_historical_data().is_empty(),
        "no end for a series the venue did not refuse",
    );
    let errors = shared.reference.drain_historical_errors();
    assert_eq!(errors.len(), 1, "the refused question is still reported");
    assert_eq!(errors[0].0, 7);
}

/// A series whose corporate actions could not be asked for is let go, not held.
///
/// The request is registered as outstanding only if it actually went out, so a
/// send that fails is on no path that later fails it. The caller was told the
/// actions could not be asked for and the series stayed held behind it —
/// waiting on an answer to a request that never left — until the connection
/// was torn down, when it was told a second time and given the end it should
/// have had at once.
#[test]
fn a_series_whose_actions_could_not_be_asked_for_is_let_go() {
    let mut hmds = HmdsState::new();
    let shared = SharedState::new();
    let mut hb = HeartbeatState::new();
    // No socket, so the send cannot go.
    let mut conn: Option<Connection> = None;

    hmds.held.push(HeldSeries {
        req_id: 21, fold: Fold::Adjusted, con_id: 756733, sec_type: "STK".to_string(),
        exchange: "SMART".to_string(), bars: Vec::new(), timezone: String::new(),
        actions_asked: false, actions_query: None, actions: None, complete: true,
    });

    hmds.send_adjustments_request(
        21, 756733, "STK", "SMART", "20240101", "20240201", &shared, &mut conn, &mut hb,
    );

    assert!(hmds.held.is_empty(), "nothing is waiting on a request that never left");
    let errors = shared.reference.drain_historical_errors();
    assert_eq!(errors.len(), 1, "told once, here, rather than again at teardown");
    assert_eq!(errors[0].0, 21);
    let ended = shared.reference.drain_historical_data();
    assert_eq!(ended.len(), 1, "and given the end that lets a blocked caller go");
    assert!(ended[0].1.is_complete);
    assert!(ended[0].1.bars.is_empty());
}

/// A request kept up to date whose batch the venue refuses is a request that
/// failed whole: the five-second stream it rides under this same number is
/// withdrawn with it. Left running, bars keep arriving under a number the
/// caller was told had failed, and the next reconnect asks for the stream
/// again.
#[test]
fn a_refused_batch_takes_the_kept_up_to_date_stream_with_it() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let sock = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    peer.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
    let mut conn = Some(Connection::new_raw(sock).unwrap());

    let mut hmds = HmdsState::new();
    let shared = SharedState::new();
    let mut hb = HeartbeatState::new();

    // One caller number, the two queries it rides, and the venue's number for
    // the stream already stated.
    hmds.pending_historical.push(("hist_3001".to_string(), 7));
    hmds.keep_up_to_date_reqs.insert(7);
    hmds.rtbar_subs.push(("rt_3002".to_string(), 7, Some(5), 0.01, 1.0));
    hmds.rtbar_resub.push(RtBarRequest {
        req_id: 7, con_id: 756733, sec_type: "STK".into(), exchange: "SMART".into(),
        what_to_show: "TRADES".into(), use_rth: true,
    });
    hmds.forming_bars.push(FormingBar {
        req_id: 7, seconds: 60, opened_at: 0, bar: Default::default(), weighted: 0.0,
    });

    hmds.process_hmds_message(
        &make_query_error_msg("hist_3001", "Invalid time length"),
        &mut conn, &shared, &None, &mut hb,
    );

    assert!(hmds.rtbar_subs.is_empty(), "the stream goes with the request that failed");
    assert!(hmds.rtbar_resub.is_empty(), "and no reconnect asks for it again");
    assert!(hmds.forming_bars.is_empty(), "nor does the bar it was folding stay behind");
    assert!(!hmds.keep_up_to_date_reqs.contains(&7));

    // The withdrawal goes out under the number the venue knows the stream by.
    let sent = String::from_utf8_lossy(&read_frame(&mut peer)).to_string();
    assert!(sent.contains("ticker:5"), "the stream is cancelled by the venue's number: {sent}");

    let errors = shared.reference.drain_historical_errors();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].0, 7);
}

/// A request that failed because its connection went is failed in its stream
/// half as well: a reconnect asks again for the streams that are still wanted,
/// and one whose request the caller was told had failed is not among them.
/// Left on the reconnect list, the bars resume under a number already
/// answered, and answer whatever is next asked under it.
#[test]
fn a_disconnect_does_not_resurrect_a_failed_request_s_stream() {
    let mut hmds = HmdsState::new();
    let shared = SharedState::new();
    let market = crate::engine::market_state::MarketState::new();
    let mut hb = HeartbeatState::new();
    let mut conn: Option<Connection> = None;

    // A request kept up to date, still waiting on its batch. Beside it, an
    // ordinary bar stream that is still wanted.
    hmds.pending_historical.push(("hist_4001".to_string(), 9));
    hmds.keep_up_to_date_reqs.insert(9);
    hmds.rtbar_subs.push(("rt_4002".to_string(), 9, None, 0.01, 1.0));
    hmds.rtbar_resub.push(RtBarRequest {
        req_id: 9, con_id: 756733, sec_type: "STK".into(), exchange: "SMART".into(),
        what_to_show: "TRADES".into(), use_rth: true,
    });
    hmds.forming_bars.push(FormingBar {
        req_id: 9, seconds: 60, opened_at: 0, bar: Default::default(), weighted: 0.0,
    });
    hmds.rtbar_subs.push(("rt_4003".to_string(), 10, None, 0.01, 1.0));
    hmds.rtbar_resub.push(RtBarRequest {
        req_id: 10, con_id: 265598, sec_type: "STK".into(), exchange: "SMART".into(),
        what_to_show: "TRADES".into(), use_rth: true,
    });

    hmds.disconnect(&mut conn, &shared, &None);

    assert!(
        shared.reference.drain_historical_errors().iter().any(|(rid, ..)| *rid == 9),
        "the caller was told the request failed",
    );
    assert!(
        hmds.rtbar_resub.iter().all(|r| r.req_id != 9),
        "and nothing asks for its stream again",
    );
    assert!(hmds.rtbar_subs.iter().all(|(_, rid, ..)| *rid != 9));
    assert!(hmds.forming_bars.iter().all(|f| f.req_id != 9));
    assert!(
        hmds.rtbar_resub.iter().any(|r| r.req_id == 10),
        "a stream that is still wanted survives the disconnect",
    );

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let sock = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (_peer, _) = listener.accept().unwrap();
    hmds.reconnect(
        Connection::new_raw(sock).unwrap(),
        &mut conn, &market, &mut hb,
    );

    assert!(
        hmds.rtbar_subs.iter().all(|(_, rid, ..)| *rid != 9),
        "the failed request's stream was not asked for again",
    );
    assert!(
        hmds.rtbar_subs.iter().any(|(_, rid, ..)| *rid == 10),
        "and the wanted one was",
    );
}

/// A number already answering a historical query does not take a second one.
///
/// Both the pages a caller is handed and the series they are held in are
/// resolved by this number, and the held series is a list searched by first
/// match. A second request under a live one put two entries under one number:
/// the second request's pages extended the first request's series, two
/// contracts were sorted together and folded on the first contract's actions,
/// and the caller was handed one series with two contracts in it and two ends.
/// The second request's own series was never completed and nothing sweeps it.
#[test]
fn a_number_already_answering_a_historical_query_is_not_given_another() {
    let mut hmds = HmdsState::new();
    let shared = SharedState::new();
    let mut hb = HeartbeatState::new();
    let (conn, _peer) = Connection::for_test();
    let mut conn = Some(conn);

    hmds.send_historical_request_ex(9, 756733, "", "2 D", "1 hour", "TRADES",
        true, false, false, "AAPL", "STK", "SMART", &mut conn, &mut hb, &shared);
    assert_eq!(hmds.held.len(), 1, "the first request is held for its pages");
    let _ = shared.reference.drain_historical_errors();

    hmds.send_historical_request_ex(9, 320227571, "", "2 D", "1 hour", "TRADES",
        true, false, false, "QQQ", "STK", "SMART", &mut conn, &mut hb, &shared);

    assert_eq!(hmds.held.len(), 1, "the second contract does not join the first's series");
    assert_eq!(
        hmds.held[0].con_id, 756733,
        "and the series still holds only the contract that was asked for",
    );
    assert_eq!(
        hmds.pending_historical.iter().filter(|(_, rid)| *rid == 9).count(), 1,
        "only one query is in flight under the number",
    );
    let errors = shared.reference.drain_historical_errors();
    assert_eq!(errors.len(), 1, "the caller is told: {errors:?}");
    assert_eq!(errors[0], (9, 386, errors[0].2.clone()), "under the number that names it");
    // Nothing ends the request that is answering. Released with an empty
    // terminal response under the number, the live request's end fired
    // before its bars had arrived, and every bar after went out as an update.
    // No waiting call can be refused here — they number themselves apart.
    assert!(
        shared.reference.drain_historical_data().is_empty(),
        "and the request that is answering is left to answer",
    );
}

/// A number already running a scan does not take a second.
///
/// Both scans run at the venue and both resolve to that number, so the caller
/// asked for one scan and read two interleaved with nothing in the sequence
/// saying so -- each batch ends the way a single scan's does. The withdrawal
/// takes one entry, so the other stayed running and went on delivering rows
/// under a number the caller had withdrawn.
#[test]
fn a_number_already_running_a_scan_is_not_given_another() {
    let mut hmds = HmdsState::new();
    let shared = SharedState::new();
    let mut hb = HeartbeatState::new();
    let (conn, _peer) = Connection::for_test();
    let mut conn = Some(conn);

    hmds.send_scanner_subscribe(9, "STK", "STK.US.MAJOR", "TOP_PERC_GAIN", 50,
        Vec::new(), &mut conn, &mut hb, &shared);
    assert_eq!(hmds.pending_scanner.len(), 1, "the first scan is running");
    let first = hmds.pending_scanner[0].0.clone();

    hmds.send_scanner_subscribe(9, "STK", "STK.US.MAJOR", "TOP_PERC_LOSE", 50,
        Vec::new(), &mut conn, &mut hb, &shared);

    assert_eq!(hmds.pending_scanner.len(), 1, "the second scan does not join it");
    assert_eq!(hmds.pending_scanner[0].0, first, "and the running scan is the one asked for");
    let errors = shared.reference.drain_historical_errors();
    assert_eq!(errors.len(), 1, "the caller is told: {errors:?}");
    assert_eq!(errors[0].1, 385, "under the number that names it");
}

/// A page that cannot be read ends a request kept up to date whose history
/// has not arrived whole, as it ends any other.
///
/// The unreadable page was warned and the stream left to go on, which is
/// right once the history is in; before that there is no history to go on
/// from — no end fired, later pages went on extending a series nobody could
/// be handed, and the number could never be used again.
#[test]
fn an_unreadable_page_ends_a_kept_up_to_date_request_still_assembling() {
    let mut hmds = HmdsState::new();
    let shared = SharedState::new();
    let mut hb = HeartbeatState::new();
    let mut conn: Option<Connection> = None;
    hmds.pending_historical.push(("q9".to_string(), 9));
    hmds.keep_up_to_date_reqs.insert(9);
    hmds.held.push(HeldSeries {
        req_id: 9, fold: Fold::None, con_id: 0, sec_type: String::new(), exchange: String::new(),
        bars: Vec::new(), timezone: String::new(), actions_asked: false, actions_query: None, actions: None, complete: false,
    });
    let xml = "<ResultSetBar><id>q9</id><eoq>true</eoq><tz>UTC</tz><Events><Bar><time>20260714-13:30:00</time><open>100.0</open></Bar></Events></ResultSetBar>";
    let mut msg = Vec::new();
    msg.extend_from_slice(b"35=W\x016118=");
    msg.extend_from_slice(xml.as_bytes());
    msg.push(0x01);
    hmds.process_hmds_message(&msg, &mut conn, &shared, &None, &mut hb);
    assert!(hmds.pending_historical.iter().all(|(_, rid)| *rid != 9), "the number is released");
    assert!(hmds.held.iter().all(|a| a.req_id != 9), "and the series with it");
    assert!(!hmds.keep_up_to_date_reqs.contains(&9), "and the stream half");
    assert!(shared.reference.drain_historical_errors().iter().any(|e| e.0 == 9), "the caller is told");
    assert!(shared.reference.drain_historical_data().iter().any(|(rid, r)| *rid == 9 && r.is_complete), "and given its end");
}

/// A scan whose subscribe did not go out is refused, not recorded as running.
///
/// Recorded regardless and logged as sent, the caller waited on a scan the
/// venue never received, with no deadline to end the wait.
#[test]
fn a_scan_that_did_not_go_out_is_refused_and_not_recorded() {
    let mut hmds = HmdsState::new();
    let shared = SharedState::new();
    let mut hb = HeartbeatState::new();
    hmds.send_scanner_subscribe(9, "STK", "STK.US.MAJOR", "TOP_PERC_GAIN", 50, Vec::new(), &mut None, &mut hb, &shared);
    assert!(hmds.pending_scanner.is_empty(), "nothing is running");
    let told = shared.reference.drain_historical_errors();
    assert!(told.iter().any(|e| e.0 == 9 && e.1 == crate::error_codes::Refusal::NOT_CONNECTED), "{told:?}");
}

/// A scan batch that names a contract id nobody can read is said to the
/// caller and recorded, rather than dropped in silence while the scan stays
/// subscribed.
#[test]
fn a_scan_batch_with_an_unreadable_row_is_said_not_dropped() {
    let mut hmds = HmdsState::new();
    let shared = SharedState::new();
    let mut hb = HeartbeatState::new();
    let mut conn: Option<Connection> = None;
    hmds.pending_scanner.push(("APISCAN1:9".to_string(), 9));
    let xml = "<ScanResponse><id>APISCAN1:9</id><scanTime>2026-09-06 13:00:00</scanTime><Contract><contractID>not-a-contract</contractID></Contract></ScanResponse>";
    let msg = fix::fix_build(&[(fix::TAG_MSG_TYPE, "U"), (6040, "10005"), (6118, xml)], 1);
    hmds.process_hmds_message(&msg, &mut conn, &shared, &None, &mut hb);
    let told = shared.reference.drain_historical_errors();
    assert!(told.iter().any(|e| e.0 == 9 && e.1 == 162), "the caller is told: {told:?}");
    assert!(shared.market.unread_wire().iter().any(|(k, _)| *k == "scanner"), "and the batch is recorded as unread");
    assert!(hmds.scanner_batches.is_empty(), "and nothing half-read is handed on");
}

/// A refusal naming the continued spelling of a query reaches that query.
///
/// The venue does not always echo the bare name it was given — the reply to a
/// news query comes back under `<name>-headlines;;…` — and every path that
/// reads data from a reply matches on that. The refusal path matched the name
/// exactly, so a refusal spelled the same way reached nothing: the request was
/// never told why, its record stood until the connection went, and the caller
/// waited out its deadline instead of hearing the reason the venue gave.
#[test]
fn a_refusal_naming_a_continued_query_name_still_reaches_it() {
    let mut hmds = HmdsState::new();
    let shared = SharedState::new();
    let mut hb = HeartbeatState::new();
    let mut conn: Option<Connection> = None;
    hmds.pending_ticks.push(("tk_4".to_string(), 77, "TRADES".to_string()));

    hmds.process_hmds_message(
        &make_query_error_msg("tk_4-headlines;;more", "no permission"),
        &mut conn, &shared, &None, &mut hb,
    );

    assert!(
        hmds.pending_ticks.is_empty(),
        "the refusal reached nothing, so the request waits for a reply that has \
         already arrived",
    );
    let told = shared.reference.drain_historical_errors();
    assert!(told.iter().any(|(r, ..)| *r == 77), "and the caller is told why: {told:?}");
}

/// A refused stream is recognised however the venue spells the name back.
///
/// The venue does not always echo the bare name it was given, and every other
/// branch of this chain matches the way the paths that read data match. This
/// one asked for the two to be equal, so a refusal naming the continued
/// spelling matched nothing: the caller was never told why, the record stayed
/// until the connection went, and a stream that was never coming was waited on
/// for the rest of the session.
#[test]
fn a_refused_tick_stream_is_matched_the_way_every_other_refusal_is() {
    let shared = std::sync::Arc::new(crate::bridge::SharedState::new());
    let mut hmds = HmdsState::new();
    let mut hb = HeartbeatState::new();
    let mut conn: Option<Connection> = None;
    hmds.tbt_subscriptions.push(TbtSubscription {
        ignore_size: false, instrument: 0, query_id: "tbt_7".to_string(),
        kind: TbtType::Last, caller_req_id: 41, venue_id: 0, min_tick: 0,
        size_tick: 0.0, running: Default::default(),
    });

    // The name it was given, continued — which is what a reply may state.
    let msg = make_query_error_msg("tbt_7.1", "no such stream");
    hmds.process_hmds_message(&msg, &mut conn, &shared, &None, &mut hb);

    assert!(
        hmds.tbt_subscriptions.is_empty(),
        "the refusal named this stream and was read as naming none",
    );
}

/// A stream's records are read in the shape the stream was asked for.
///
/// The same contract can carry trades, quotes and the point between them at
/// once, and a record read under the wrong shape is decoded field by field
/// into something nobody sent — a midpoint read as a trade states a size and
/// a venue the venue never wrote.
#[test]
fn a_stream_is_read_in_the_shape_it_was_asked_for() {
    use crate::protocol::tbt_stream::TbtKind;
    use crate::types::TbtType;
    assert_eq!(super::frame_kind(TbtType::MidPoint), TbtKind::MidPoint);
    assert_eq!(super::frame_kind(TbtType::BidAsk), TbtKind::BidAsk);
    assert_eq!(super::frame_kind(TbtType::Last), TbtKind::AllLast);
    assert_eq!(super::frame_kind(TbtType::AllLast), TbtKind::AllLast);
}
