//! Top of book (35=Q acknowledgements, 35=L definitions, 35=P ticks):
//! decode against the reference's callbacks.

use super::fixture::load;
use super::replay::{assert_same_callbacks, replay_market_data};

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
// everything at the first 35=P. Found by this test (ibx#486); the rule for
// how long the record keeps its values is not read yet.
#[test]
#[ignore = "ibx#486: the reference keeps a contract's quote after the cancel and gives it to the next request"]
fn a_new_request_gets_the_quote_kept_from_a_cancelled_one() {
    assert!(replay_and_compare("l1_aapl_preopen_delayed", None, &[9100]) > 10);
}
