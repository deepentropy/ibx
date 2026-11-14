//! Tests of the C entry layer that need no C++: the structures that cross
//! in both directions come back as written, the conditions too, and what
//! the layer refuses.

use super::generated::checks;
use super::generated::*;
use super::wire::{R, W};
use super::*;
use crate::api::types::{price_from_f64, Contract, Order};

// A session is used from several threads of the caller: the requests from
// one, the callbacks from another.
#[test]
fn a_session_is_shared_between_threads() {
    fn shared<T: Send + Sync>() {}
    shared::<EClient>();
}

// ibx#235: every plain field of the structures that cross both ways,
// filled with a value made from its name, is read back in its place.
#[test]
fn structures_that_cross_both_ways_come_back_as_written() {
    for kind in ["TagValue", "ComboLeg", "DeltaNeutralContract", "Contract", "Order"] {
        let bytes = checks::make(kind, 3).unwrap_or_else(|| panic!("{kind} is not made"));
        let failures = checks::check(kind, &bytes, 3).unwrap_or_else(|| panic!("{kind} is not checked"));
        assert!(failures.is_empty(), "{kind}: {failures:?}");
        // Another seed gives other values: the check sees them.
        assert!(!checks::check(kind, &bytes, 4).unwrap().is_empty(), "{kind}: the check sees nothing");
    }
    assert!(checks::make("Nothing", 1).is_none() && checks::check("Nothing", &[], 1).is_none());
}

// The fields with a rule of their own.
#[test]
fn order_fields_with_a_rule_of_their_own() {
    let order = Order {
        total_quantity: 0.123456789012,
        total_quantity_text: "0.123456789012".into(),
        route_marketable_to_bbo: true,
        seek_price_improvement: false,
        use_price_mgmt_algo: 1,
        origin: 2,
        soft_dollar_tier_name: "n".into(),
        soft_dollar_tier_val: "v".into(),
        soft_dollar_tier_display_name: "d".into(),
        order_combo_legs: vec![1.5, f64::MAX],
        conditions: vec![
            OrderCondition::Price { con_id: 11, exchange: "EX".into(), price: price_from_f64(101.5), is_more: true, trigger_method: 2 },
            OrderCondition::Time { time: "20260311-09:30:00".into(), is_more: false },
            OrderCondition::Margin { percent: 30, is_more: true },
            OrderCondition::Execution { symbol: "S".into(), exchange: "E".into(), sec_type: "STK".into() },
            OrderCondition::Volume { con_id: 12, exchange: "EX2".into(), volume: 5000, is_more: true },
            OrderCondition::PercentChange { con_id: 13, exchange: "EX3".into(), percent: 2.5, is_more: false },
        ],
        ..Default::default()
    };
    let mut buf = Vec::new();
    w_order(&mut W(&mut buf), &order);
    let mut r = R::new(&buf);
    let back = r_order(&mut r);
    assert_eq!(r.refused, None);
    assert_eq!((back.total_quantity, back.total_quantity_text.as_str()), (0.123456789012, "0.123456789012"));
    assert_eq!((back.route_marketable_to_bbo, back.seek_price_improvement, back.use_price_mgmt_algo, back.origin), (true, false, 1, 2));
    assert_eq!((back.soft_dollar_tier_name.as_str(), back.soft_dollar_tier_val.as_str(), back.soft_dollar_tier_display_name.as_str()), ("n", "v", "d"));
    assert_eq!(back.order_combo_legs, [1.5, f64::MAX]);
    assert_eq!(format!("{:?}", back.conditions), format!("{:?}", order.conditions));
}

// The official unset strike is the largest double; the client's is 0.
#[test]
fn an_unset_strike_is_zero_for_the_client() {
    let mut buf = Vec::new();
    w_contract(&mut W(&mut buf), &Contract { strike: f64::MAX, ..Default::default() });
    assert_eq!(r_contract(&mut R::new(&buf)).strike, 0.0);
    buf.clear();
    w_contract(&mut W(&mut buf), &Contract { strike: 12.5, ..Default::default() });
    assert_eq!(r_contract(&mut R::new(&buf)).strike, 12.5);
}

/// One condition as the caller writes it.
fn condition(w: &mut W, kind: i64, and: bool) {
    w.i(kind);
    w.b(and);
    w.b(true);
    w.i(7);
    w.s("EX");
    w.f(1.5);
    w.s("20260311-09:30:00");
    w.s("SYM");
    w.s("STK");
    w.i(0);
}

// What the client cannot send refuses the order: it must not go out
// without it.
#[test]
fn conditions_the_client_cannot_send_refuse_the_order() {
    // Joined by "or".
    let mut buf = Vec::new();
    let w = &mut W(&mut buf);
    w.n(2);
    condition(w, 1, false);
    condition(w, 3, false);
    let mut r = R::new(&buf);
    assert_eq!(read_conditions(&mut r).len(), 2);
    assert_eq!(r.refused.map(|(code, _)| code), Some(321));

    // The last one's link joins nothing: not refused.
    let mut buf = Vec::new();
    let w = &mut W(&mut buf);
    w.n(2);
    condition(w, 1, true);
    condition(w, 3, false);
    let mut r = R::new(&buf);
    assert_eq!(read_conditions(&mut r).len(), 2);
    assert_eq!(r.refused, None);

    // A kind that does not exist.
    let mut buf = Vec::new();
    let w = &mut W(&mut buf);
    w.n(1);
    condition(w, 2, true);
    let mut r = R::new(&buf);
    assert!(read_conditions(&mut r).is_empty());
    assert_eq!(r.refused.map(|(code, _)| code), Some(321));

    // An unset percent change reads as 0, as the Python client.
    let mut buf = Vec::new();
    let w = &mut W(&mut buf);
    w.n(1);
    w.i(7);
    w.b(true);
    w.b(false);
    w.i(7);
    w.s("EX");
    w.f(f64::MAX);
    w.s("");
    w.s("");
    w.s("");
    w.i(0);
    assert!(matches!(read_conditions(&mut R::new(&buf))[..], [OrderCondition::PercentChange { percent, .. }] if percent == 0.0));
}

// A text with bytes that are not UTF-8 is read with them replaced, not refused.
#[test]
fn a_text_that_is_not_utf8_is_read() {
    let bytes = [b'a', 0xff, b'b'];
    let text = unsafe { IbxStr::of_bytes(&bytes).text() };
    assert_eq!(text, "a\u{fffd}b");
    let empty = IbxStr { p: std::ptr::null(), n: 5 };
    assert_eq!(unsafe { empty.text() }, "");
}

// The masks of the tick attributes, as the C++ side reads them.
#[test]
fn tick_attribute_masks() {
    use crate::api::types::{TickAttrib, TickAttribBidAsk, TickAttribLast};
    assert_eq!(mask_tick_attrib(&TickAttrib { can_auto_execute: true, past_limit: false, pre_open: true }), 0b101);
    assert_eq!(mask_tick_attrib_last(&TickAttribLast { past_limit: false, unreported: true }), 0b10);
    assert_eq!(mask_tick_attrib_bid_ask(&TickAttribBidAsk { bid_past_low: true, ask_past_high: true }), 0b11);
}

// A null handle or table is refused, not followed.
#[test]
fn null_handles_are_refused() {
    unsafe {
        assert!(!ibx_is_connected(std::ptr::null()));
        assert_eq!(ibx_server_version(std::ptr::null()), 0);
        assert!(!ibx_wait(std::ptr::null(), 0));
        assert_eq!(ibx_process(std::ptr::null_mut(), std::ptr::null(), std::ptr::null_mut()), 2);
        assert_eq!(ibx_req_current_time(std::ptr::null_mut()), 1);
        ibx_notify(std::ptr::null());
        ibx_disconnect(std::ptr::null_mut());
        ibx_free(std::ptr::null_mut());
        let mut reason = [0u8; 32];
        assert!(ibx_connect(std::ptr::null(), reason.as_mut_ptr(), reason.len()).is_null());
        assert!(reason.starts_with(b"no configuration"));
    }
}

// The precisions of a request the session does not know are not known,
// and nothing is written.
#[test]
fn the_precisions_of_an_unknown_request_are_not_known() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let shared = std::sync::Arc::new(crate::bridge::SharedState::new());
    let client = EClient::from_parts(shared, tx, std::thread::spawn(|| {}), "A1".into());
    let session = IbxClient::new(client, None);
    let (mut price, mut size) = ([0u8; 8], [0u8; 8]);
    let (mut price_length, mut size_length) = (99usize, 99usize);
    unsafe {
        assert!(!ibx_precisions(&session, 7, price.as_mut_ptr(), price.len(), &mut price_length, size.as_mut_ptr(), size.len(), &mut size_length));
        assert!(!ibx_precisions(std::ptr::null(), 7, price.as_mut_ptr(), price.len(), &mut price_length, size.as_mut_ptr(), size.len(), &mut size_length));
    }
    assert_eq!((price_length, size_length), (99, 99));
}

// The first pass gives the managed accounts and the next valid id before
// anything else, as a session of the reference starts.
#[test]
fn the_first_pass_starts_with_the_accounts_and_the_next_id() {
    use crate::api::wrapper::tests::RecordingWrapper;
    let (tx, _rx) = crossbeam_channel::unbounded();
    let shared = std::sync::Arc::new(crate::bridge::SharedState::new());
    shared.orders.push_order_error(-1, 2104, "Market data farm connection is OK:usfarm".into());
    let client = EClient::from_parts(shared, tx, std::thread::spawn(|| {}), "DU1".into());
    let session = IbxClient::new(client, Some(("DU1".into(), 42)));
    let mut wrapper = RecordingWrapper::default();
    pass(&session, &mut wrapper);
    assert_eq!(wrapper.events[0], "next_valid_id:42");
    assert!(wrapper.events.iter().any(|e| e.starts_with("error:-1:2104:")), "{:?}", wrapper.events);
    // Once only.
    let mut again = RecordingWrapper::default();
    pass(&session, &mut again);
    assert!(again.events.is_empty(), "{:?}", again.events);
}
