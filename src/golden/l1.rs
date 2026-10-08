//! Top of book (35=Q acknowledgements, 35=L definitions, 35=P ticks):
//! decode against the reference's callbacks.

use super::load;
use super::replay::replay_market_data;
use crate::test_support::scenario::assert_same_callbacks;

const MD_CALLBACKS: &[&str] = &[
    "tickPrice", "tickSize", "tickString", "tickGeneric", "marketDataType", "tickReqParams", "tickSnapshotEnd",
];

// AAPL then SPY before the open (02/10/2026, b1_441_smart_components, up
// to the EUR.USD request, which goes to another farm): the acknowledgements
// give the market data type and the request parameters; the first 35=P
// gives the last time, the last with its size, the volume, close, open,
// bid and ask with their sizes; no bid or ask exchange before the exchange
// map came (an empty text is not sent); the next frames give the volume,
// the ask size and the ask exchanges. The smart components requests: 321
// for an unknown code, the map once it came.
#[test]
fn aapl_and_spy_before_the_open() {
    let fx = load("l1_aapl_spy_preopen");
    let mut keep = MD_CALLBACKS.to_vec();
    keep.extend(["smartComponents", "error"]);
    let r = replay_market_data(&fx, "usfarm", &keep, Some(2633));
    assert!(r.unsent.is_empty(), "{:?}", r.unsent);
    // The farm notices of the session start are not part of the replay.
    let theirs: Vec<String> = r.theirs.into_iter().filter(|l| !l.starts_with("error|-1|")).collect();
    let ticks = theirs.iter().filter(|l| l.starts_with("tick")).count();
    assert_eq!((theirs.len(), ticks), (40, 33), "{theirs:#?}");
    assert_same_callbacks(&r.ours, &theirs);
}

/// Replay a whole market data fixture on the primary farm and compare the
/// callbacks of the requests `reqs` (all when empty).
fn replay_and_compare(name: &str, until: Option<u64>, reqs: &[i64]) -> usize {
    let fx = load(name);
    let mut keep = MD_CALLBACKS.to_vec();
    keep.push("error");
    let r = replay_market_data(&fx, "usfarm", &keep, until);
    // Definition replies of lookups ibx does not make (the reference's
    // own) are left out; every farm frame goes.
    assert!(r.unsent.iter().all(|(_, conn)| conn == "CCP"), "{name}: frames not sent {:?}", r.unsent);
    let of_reqs = |l: &String| !l.starts_with("error|-1|")
        && (reqs.is_empty() || reqs.iter().any(|id| l.split('|').nth(1) == Some(&id.to_string())));
    let theirs: Vec<String> = r.theirs.into_iter().filter(of_reqs).collect();
    let ours: Vec<String> = r.ours.into_iter().filter(of_reqs).collect();
    assert_same_callbacks(&ours, &theirs);
    theirs.len()
}

#[test]
fn spy_and_qqq_in_the_session() {
    // Up to the second SPY request: the reference then held a SPY record
    // of its own with data (the orders of the scenario), which the new
    // request joined; that record's frames are not in the recording.
    assert!(replay_and_compare("l1_spy_qqq_rth", Some(6581), &[]) > 40);
}

// AAPL before the open with delayed data asked (28/09/2026,
// premarket_order_types): the paper session has real-time data, so the
// request gets type 1; the ask exchanges follow each ask size change. BMW
// and 7203 of the same scenario are on other farms: AAPL only.
#[test]
fn aapl_with_delayed_data_asked_before_the_open() {
    assert!(replay_and_compare("l1_aapl_preopen_delayed", Some(22599), &[9001]) > 30);
}

// A new AAPL request 20 s after the first was cancelled (same scenario):
// the reference sends the bid and the ask with their sizes on the
// acknowledgement, before any 35=P: the values the contract's record kept
// from the cancelled request (bid 340.45 x 320, ask 340.52 x 40, the last
// ones of request 9001); the first 35=P then gives the trade and daily
// fields only. ibx frees the contract's quote at the cancel and sends
// everything at the first 35=P. Found by this test (ibx#508); the record
// keeps its values 45 s at least (capture of 07/10/2026), how long is not
// read yet.
#[test]
#[ignore = "ibx#508: the reference keeps a contract's quote after the cancel and gives it to the next request"]
fn a_new_request_gets_the_quote_kept_from_a_cancelled_one() {
    assert!(replay_and_compare("l1_aapl_preopen_delayed", None, &[9100]) > 10);
}

/// The tick callbacks of a market data fixture when the client reads
/// after each input and when it reads once after the last frame: both
/// are the reference's (ibx#446). The other callbacks come from other
/// queues, read in their own order: left out.
fn ticks_read_once_as_the_reference(name: &str, until: Option<u64>, reqs: &[i64]) -> usize {
    const TICKS: &[&str] = &["tickPrice", "tickSize", "tickString", "tickGeneric", "tickSnapshotEnd"];
    let fx = load(name);
    let of_reqs = |l: &String| reqs.is_empty() || reqs.iter().any(|id| l.split('|').nth(1) == Some(&id.to_string()));
    let mut out = Vec::new();
    for read_each in [true, false] {
        let r = super::replay::replay_market_data_read(&fx, "usfarm", TICKS, until, &[], read_each);
        let theirs: Vec<String> = r.theirs.into_iter().filter(of_reqs).collect();
        let ours: Vec<String> = r.ours.into_iter().filter(of_reqs).collect();
        assert_same_callbacks(&ours, &theirs);
        out.push(ours);
    }
    assert_eq!(out[0], out[1], "{name}: reading once changed the callbacks");
    out[0].len()
}

// ibx#446: the reference sends each farm message's ticks with that
// message's values: a client that reads once after every frame of the
// session gets the same ticks as one that reads after each frame.
#[test]
fn tick_callbacks_do_not_depend_on_when_the_client_reads() {
    assert!(ticks_read_once_as_the_reference("l1_aapl_spy_preopen", Some(2633), &[]) > 30);
    assert!(ticks_read_once_as_the_reference("l1_spy_qqq_rth", Some(6581), &[]) > 40);
    assert!(ticks_read_once_as_the_reference("l1_aapl_preopen_delayed", Some(22599), &[9001]) > 30);
}

/// The market data messages and contract lookups ibx wrote, against the
/// reference's: (ours, theirs) for 35=V, then for 35=c.
fn requests_of(name: &str, until: Option<u64>, skip: &[u64]) -> [(Vec<String>, Vec<String>); 2] {
    let fx = load(name);
    let r = super::replay::replay_market_data_without(&fx, "usfarm", MD_CALLBACKS, until, skip);
    let (v, c) = super::replay::request_messages(&r.session, &["V"], &["c"]);
    let theirs = |t: &str| r.requests_theirs.iter().filter(|m| m.starts_with(t)).cloned().collect::<Vec<_>>();
    // Market data of contracts the reference asked on this farm only.
    let farm_v = theirs("35=V");
    let on_farm = |m: &String| farm_v.iter().any(|t| t.split('|').find(|f| f.starts_with("6008=")) == m.split('|').find(|f| f.starts_with("6008=")));
    [(v.into_iter().filter(on_farm).collect(), farm_v.clone()), (c, theirs("35=c"))]
}

// The requests of the top of book replays (02/10 and 28/09/2026): the
// subscribe of bid/ask and last (442, 443, with 6088=Socket and 9830=1),
// the gateway's own exchange map entry (626) after the acknowledgement and
// its cancel once the map came, the cancel of the request (no 6088), and
// the symbol lookups: each message as the reference's, the farm ids and
// lookup ids masked. EUR.USD, BMW and 7203 go to other farms: left out.
#[test]
fn market_data_requests_as_the_reference() {
    for (name, until, skip) in [
        ("l1_aapl_spy_preopen", Some(2635), &[2633][..]),
        ("l1_spy_qqq_rth", Some(5410), &[][..]),
        ("l1_aapl_preopen_delayed", Some(22599), &[][..]),
    ] {
        for (ours, theirs) in requests_of(name, until, skip) {
            assert!(!theirs.is_empty(), "{name}");
            assert_eq!(ours, theirs, "{name}");
        }
    }
}
