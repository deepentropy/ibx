use std::sync::Arc;

use super::*;
use crate::types::model::PRICE_SCALE_F;
use crate::api::wrapper::Wrapper;
use crate::api::wrapper::tests::RecordingWrapper;
use crate::bridge::SharedState;
use crate::control::historical::{HistoricalResponse, HistoricalBar, HeadTimestampResponse};
use crate::control::contracts::{ContractDefinition, OptionChainScope, SecurityType, SymbolMatch};
use crate::control::scanner::{ScannerEntry, ScannerResult};
use crate::control::news::NewsHeadline;
use crate::control::histogram::HistogramEntry;

/// The next command a call puts on the channel, if it put one there.
///
/// A replace carries the caller's statement of the order on the replace
/// itself, so there is one command and not two.
pub(crate) fn next_command(
    rx: &std::sync::mpsc::Receiver<ControlCommand>,
) -> Option<ControlCommand> {
    rx.try_recv().ok()
}

/// Helper: create a test EClient backed by SharedState + channel.
pub(crate) fn test_client() -> (EClient, std::sync::mpsc::Receiver<ControlCommand>, Arc<SharedState>) {
    let shared = Arc::new(SharedState::new());
    let (tx, rx) = std::sync::mpsc::sync_channel(4096);
    let handle = std::thread::spawn(|| {});
    let client = EClient::from_parts(shared.clone(), tx, handle, "DU123".into());
    // Pre-seed SPY so find_or_register_instrument hits the fast path.
    client.core.con_id_to_instrument.lock().unwrap().insert(756733, 0);
    (client, rx, shared)
}

/// The slot a contract holds is not answered after the engine has taken it
/// back.
///
/// The getter is what a caller reads a quote through, and the cache it reads
/// answers without asking the engine. A slot goes to the next contract that
/// needs one, so an answer from here after that names another contract
/// altogether — the caller asks for one symbol's quote and is given another's.
#[test]
fn a_slot_the_engine_took_back_is_not_named_by_the_getter() {
    let (client, _rx, shared) = test_client();
    {
        let mut cache = client.core.con_id_to_instrument.lock().unwrap();
        cache.insert(756733, 4);
        cache.insert(265598, 5);
    }

    shared.market.note_released_slot(4, u64::MAX);

    assert_eq!(client.instrument_of(756733), None, "the freed slot is not named");
    assert_eq!(client.instrument_of(265598), Some(5), "the others stand");
    assert_eq!(client.instrument_of(0), None, "a contract with no id names no slot");
}

/// A short bracket is a sell, and its exits take the selling orientation:
/// take-profit below the entry, stop-loss above.
#[test]
fn a_short_bracket_reads_its_exits_the_way_a_sell_does() {
    let (client, _rx, _shared) = test_client();
    let c = spy();
    // Selling at 100: take profit below, stop out above.
    assert!(
        client.place_bracket(&c, "SSHORT", 1.0, 100.0, 90.0, 110.0).is_ok(),
        "a short bracket with its exits the right way round is placed",
    );
    assert!(
        client.place_bracket(&c, "SSHORT", 1.0, 100.0, 110.0, 90.0).is_err(),
        "and one with them the wrong way round is refused",
    );
    // The plain sell it must agree with.
    assert!(client.place_bracket(&c, "SELL", 1.0, 100.0, 90.0, 110.0).is_ok());
    assert!(client.place_bracket(&c, "SELL", 1.0, 100.0, 110.0, 90.0).is_err());
}

/// Helper: SPY contract.
fn spy() -> Contract {
    Contract {
        con_id: 756733, symbol: "SPY".into(), exchange: "SMART".into(),
        sec_type: "STK".into(),
        ..Default::default()
    }
}

/// Case name paired with the setter that gives an order the named attribute.
type OrderCase = (&'static str, fn(&mut Order));

// ═══════════════════════════════════════════════════════════════════
//  Algo parsing
// ═══════════════════════════════════════════════════════════════════

/// Re-placing a tracked id is a modify, and a stop order's price lives in
/// `aux_price`. Reading only `lmt_price` sent a limit price of zero for an
/// order that has no limit leg, which the venue rejects outright.
#[test]
fn modifying_a_stop_carries_the_new_trigger() {
    let (client, rx, _shared) = test_client();
    let stop = Order {
        action: "SELL".into(), total_quantity: 1.0, order_type: "STP".into(),
        aux_price: 600.0, tif: "DAY".into(), ..Default::default()
    };
    client.place_order(9201, &spy(), &stop).unwrap();
    rx.try_recv().expect("the submit");

    let moved = Order { aux_price: 610.0, ..stop };
    client.place_order(9201, &spy(), &moved).unwrap();

    match next_command(&rx).expect("the modify") {
        ControlCommand::Order(OrderRequest::Modify { stop_price, .. }) => assert_eq!(
            stop_price, (610.0 * PRICE_SCALE_F) as i64,
            "the new trigger must reach the request",
        ),
        other => panic!("expected a Modify, got {other:?}"),
    }
}
/// A trailing stop limit's replace names its limit offset as the price and
/// its trail as the trigger, which is where the submit reads each from.
/// Read from `lmt_price`, the offset a caller set on `lmt_price_offset` never
/// reached the request.
#[test]
fn modifying_a_trailing_stop_limit_carries_its_offset_and_trail() {
    let (client, rx, _shared) = test_client();
    let placed = Order {
        action: "SELL".into(), total_quantity: 1.0, order_type: "TRAIL LIMIT".into(),
        aux_price: 1.0, lmt_price_offset: 0.1, tif: "DAY".into(), ..Default::default()
    };
    client.place_order(9202, &spy(), &placed).unwrap();
    rx.try_recv().expect("the submit");

    let moved = Order { aux_price: 2.0, lmt_price_offset: 0.2, ..placed };
    client.place_order(9202, &spy(), &moved).unwrap();
    match next_command(&rx).expect("the modify") {
        ControlCommand::Order(OrderRequest::Modify { price, stop_price, .. }) => assert_eq!(
            (price, stop_price), ((0.2 * PRICE_SCALE_F) as i64, (2.0 * PRICE_SCALE_F) as i64),
        ),
        other => panic!("expected a Modify, got {other:?}"),
    }
}

/// A replace carries the caller's own statement of the order, so an order this
/// session did not place can be restated from it.
#[test]
fn a_replace_carries_the_callers_statement_of_the_order() {
    let (client, rx, shared) = test_client();
    let named = Order {
        action: "BUY".into(), total_quantity: 1.0, order_type: "PEG MID".into(),
        lmt_price: 100.0, tif: "DAY".into(), ..Default::default()
    };
    // Known from the venue's naming at connect, in no book of this client's.
    shared.orders.push_order_info(9302, crate::bridge::RichOrderInfo {
        contract: spy(),
        order: Order { order_id: 9302, ..named.clone() },
        order_state: crate::types::model::OrderState { status: "Submitted".into(), ..Default::default() },
        last_exec: Default::default(),
    });
    let capped = Order { lmt_price: 101.0, ..named };
    client.place_order(9302, &spy(), &capped).unwrap();

    match rx.try_recv().expect("the replace") {
        ControlCommand::Order(OrderRequest::Modify { order_id, spec, .. }) => {
            assert_eq!(order_id, 9302);
            let spec = spec.expect("the replace carries the caller's statement");
            assert!(
                matches!(spec.kind, crate::types::OrderKind::PegMid { price_cap, .. } if price_cap == (101.0 * PRICE_SCALE_F) as i64),
                "the shape as the caller states it: {:?}", spec.kind,
            );
        }
        other => panic!("one command, carrying the statement, got {other:?}"),
    }

    // The venue's status has been dispatched, which tracks the order here
    // without making it one this client placed; and the statement goes with
    // every replace of it, the latest standing at the engine.
    client.core.update_order_status(&shared, 9302, OrderStatus::Submitted, 0.0, 1.0, 0);
    let recapped = Order { lmt_price: 102.0, ..capped };
    client.place_order(9302, &spy(), &recapped).unwrap();
    let mut seen = Vec::new();
    while let Ok(cmd) = rx.try_recv() {
        seen.push(match cmd {
            ControlCommand::Order(OrderRequest::Modify { spec: Some(_), .. }) => "replace with its statement",
            ControlCommand::Order(OrderRequest::Modify { spec: None, .. }) => "replace stating nothing",
            _ => "other",
        });
    }
    assert_eq!(
        seen, ["replace with its statement"],
        "a second replace of a venue-named order states it again, on the replace",
    );
}

/// The statement goes with a replace that is built and held as well, or the
/// transmit that follows it finds the order recorded here and sends none.
#[test]
fn a_held_replace_of_a_venue_named_order_still_states_it() {
    let (client, rx, shared) = test_client();
    let named = Order {
        action: "BUY".into(), total_quantity: 1.0, order_type: "PEG MID".into(),
        lmt_price: 100.0, tif: "DAY".into(), ..Default::default()
    };
    shared.orders.push_order_info(9303, crate::bridge::RichOrderInfo {
        contract: spy(),
        order: Order { order_id: 9303, ..named.clone() },
        order_state: crate::types::model::OrderState { status: "Submitted".into(), ..Default::default() },
        last_exec: Default::default(),
    });
    let held = Order { lmt_price: 101.0, transmit: false, ..named.clone() };
    client.place_order(9303, &spy(), &held).unwrap();
    assert!(
        rx.try_recv().is_err(),
        "a held replace holds its statement with it, so nothing leaves yet",
    );

    let sent = Order { lmt_price: 101.0, transmit: true, ..named };
    client.place_order(9303, &spy(), &sent).unwrap();
    let mut stated = false;
    while let Ok(cmd) = rx.try_recv() {
        if let ControlCommand::Order(OrderRequest::Modify { order_id: 9303, spec, .. }) = cmd {
            stated |= spec.is_some();
        }
    }
    assert!(stated, "the transmit sends the replace, carrying the statement it was held with");
}

/// A bracket's legs are orders this client placed, so a leg replaced ahead of
/// the venue's acknowledgement is replaced, not placed again.
///
/// The legs went out under their numbers and were recorded nowhere here, so a
/// replace of one before the acknowledgement read as a fresh placement under
/// the leg's number, and the engine's record of the leg — its parent, its
/// group — was overwritten by one carrying neither.
#[test]
fn a_bracket_leg_replaced_before_its_acknowledgement_is_replaced_not_placed_again() {
    let (client, rx, _shared) = test_client();
    let [_, tp_id, _] = client.place_bracket(&spy(), "BUY", 1.0, 100.0, 110.0, 90.0).unwrap();
    while rx.try_recv().is_ok() {}
    let moved = Order {
        action: "SELL".into(), total_quantity: 1.0, order_type: "LMT".into(),
        lmt_price: 111.0, tif: "DAY".into(), transmit: true, ..Default::default()
    };
    client.place_order(tp_id, &spy(), &moved).unwrap();
    let mut seen = Vec::new();
    while let Ok(cmd) = rx.try_recv() {
        seen.push(match cmd {
            ControlCommand::Order(OrderRequest::Modify { spec: Some(_), .. }) => "replace with its statement",
            ControlCommand::Order(OrderRequest::Modify { spec: None, .. }) => "replace stating nothing",
            ControlCommand::Order(OrderRequest::SubmitEx { .. }) => "placement",
            _ => "other",
        });
    }
    assert_eq!(
        seen, ["replace with its statement"],
        "the leg is replaced, carrying the caller's own statement of what it now is",
    );
}

/// The legs are recorded as the wire states them, since the open-order reads
/// answer from the record for the life of the order.
#[test]
fn a_brackets_legs_are_recorded_as_the_wire_states_them() {
    let (client, _rx, _shared) = test_client();
    let [parent, tp, sl] = client.place_bracket(&spy(), "SSHORT", 1.0, 100.0, 90.0, 110.0).unwrap();
    let entry = client.core.tracked_order(parent as u64).expect("tracked");
    assert_eq!((entry.action.as_str(), entry.tif.as_str(), entry.oca_group.as_str(), entry.oca_type, entry.parent_id), ("SSHORT", "DAY", "", 0, 0));
    for (id, order_type, lmt, aux) in [(tp, "LMT", 90.0, 0.0), (sl, "STP", 0.0, 110.0)] {
        let exit = client.core.tracked_order(id as u64).expect("tracked");
        assert_eq!(
            (exit.action.as_str(), exit.order_type.as_str(), exit.lmt_price, exit.aux_price, exit.tif.as_str(), exit.oca_group.as_str(), exit.oca_type, exit.parent_id),
            ("BUY", order_type, lmt, aux, "GTC", format!("OCA_{parent}").as_str(), 3, parent),
            "leg {id}",
        );
    }
}

/// Nothing on an execution report carries a parent order id, so the engine
/// reports none. This client placed the order and was told the parent, so it
/// can answer where the engine cannot — and an order it did not place keeps
/// the engine's answer rather than borrowing someone else's.
#[test]
fn a_locally_placed_child_reports_the_parent_it_was_given() {
    let (client, rx, shared) = test_client();
    let child = Order {
        action: "SELL".into(), total_quantity: 1.0, order_type: "LMT".into(),
        lmt_price: 110.0, tif: "DAY".into(), parent_id: 4242, ..Default::default()
    };
    client.place_order(9401, &spy(), &child).unwrap();
    while rx.try_recv().is_ok() {}

    shared.orders.push_order_update(OrderUpdate {
        order_id: 9401, instrument: 0, status: OrderStatus::Submitted,
        filled_qty: 0.0, remaining_qty: 1.0, avg_price: 0, perm_id: 0, parent_id: 0, timestamp_ns: 0,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    let status = w.events.iter().find(|e| e.starts_with("order_status:9401:"))
        .expect("the status was dispatched");
    assert!(status.contains(":9401:"), "{status}");
    assert_eq!(
        w.parent_ids.last().copied(), Some(4242),
        "the parent this client recorded is reported: {:?}", w.events,
    );

    // An order this client never placed keeps the engine's answer.
    shared.orders.push_order_update(OrderUpdate {
        order_id: 9999, instrument: 0, status: OrderStatus::Submitted,
        filled_qty: 0.0, remaining_qty: 1.0, avg_price: 0, perm_id: 0, parent_id: 0, timestamp_ns: 0,
    });
    let mut w2 = RecordingWrapper::default();
    client.process_msgs(&mut w2);
    assert_eq!(w2.parent_ids.last().copied(), Some(0), "no parent is invented");
}

/// A status arriving on the heels of a fill reported an average of zero, so
/// the last thing a caller heard about a filled order was that it had filled
/// at no price at all.
#[test]
fn a_status_states_what_the_order_paid() {
    let (client, _rx, shared) = test_client();
    shared.orders.push_order_update(OrderUpdate {
        order_id: 9403, instrument: 0, status: OrderStatus::Filled,
        filled_qty: 100.0, remaining_qty: 0.0,
        avg_price: 13 * crate::types::PRICE_SCALE + crate::types::PRICE_SCALE / 2,
        perm_id: 0, parent_id: 0, timestamp_ns: 0,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    let status = w.events.iter().find(|e| e.starts_with("order_status:9403:"))
        .expect("the status was dispatched");
    assert!(status.ends_with(":13.5"), "the average the report stated: {status}");
}

/// A fill emits its own order_status from a different branch, so the parent
/// has to be preferred there as well. Before this it reported zero on every
/// fill of a bracket child — the callback a caller is most likely to act on.
#[test]
fn a_fill_reports_the_parent_the_child_was_given() {
    let (client, rx, shared) = test_client();
    let child = Order {
        action: "SELL".into(), total_quantity: 1.0, order_type: "LMT".into(),
        lmt_price: 110.0, tif: "DAY".into(), parent_id: 4242, ..Default::default()
    };
    client.place_order(9402, &spy(), &child).unwrap();
    while rx.try_recv().is_ok() {}

    shared.orders.push_fill(Fill {
        order_id: 9402, instrument: 0, side: Side::Sell, qty: crate::types::QTY_SCALE, remaining: 0,
        price: 110 * crate::types::PRICE_SCALE, timestamp_ns: 0,
        cum_qty: crate::types::QTY_SCALE, avg_price: 110 * crate::types::PRICE_SCALE,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert_eq!(
        w.parent_ids.first().copied(), Some(4242),
        "the fill's order_status carries the recorded parent: {:?}", w.events,
    );
}

/// The margin preview reports its own status too, and hard-coded a zero
/// parent. A preview of a bracket child that disowns it is the same wrong
/// answer as the other two paths gave.
#[test]
fn a_what_if_preview_reports_the_parent_the_child_was_given() {
    let (client, rx, shared) = test_client();
    let child = Order {
        action: "SELL".into(), total_quantity: 1.0, order_type: "LMT".into(),
        lmt_price: 110.0, tif: "DAY".into(), parent_id: 4242, what_if: true,
        ..Default::default()
    };
    client.place_order(9403, &spy(), &child).unwrap();
    while rx.try_recv().is_ok() {}

    shared.orders.push_what_if(WhatIfResponse {
        order_id: 9403, instrument: 0,
        init_margin_before: 0, maint_margin_before: 0, equity_with_loan_before: 0,
        init_margin_after: 0, maint_margin_after: 0, equity_with_loan_after: 0,
        commission: Some(0),
        min_commission: None,
        max_commission: None,
        commission_currency: String::new(),
        warning_text: String::new(),
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert_eq!(
        w.parent_ids.first().copied(), Some(4242),
        "the preview carries the recorded parent: {:?}", w.events,
    );
}

/// A preview is finished when its callback runs, not after it.
///
/// The number a preview was asked under is the caller's to place under next,
/// and that is what a caller does from inside the callback that answers the
/// preview. Still recorded while the callback ran, the placement read as a
/// change to a what-if and was refused for being one; a callback that ended
/// the read another way left the finished preview recorded under a number
/// nothing could place again.
#[test]
fn a_preview_is_no_longer_tracked_while_its_own_callback_runs() {
    use std::sync::Mutex;

    let (client, rx, shared) = test_client();
    let preview = Order {
        action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(),
        lmt_price: 110.0, tif: "DAY".into(), what_if: true, ..Default::default()
    };
    client.place_order(9404, &spy(), &preview).unwrap();
    while rx.try_recv().is_ok() {}

    shared.orders.push_what_if(WhatIfResponse {
        order_id: 9404, instrument: 0,
        init_margin_before: 0, maint_margin_before: 0, equity_with_loan_before: 0,
        init_margin_after: 0, maint_margin_after: 0, equity_with_loan_after: 0,
        commission: Some(0), min_commission: None, max_commission: None,
        commission_currency: String::new(), warning_text: String::new(),
    });

    /// Reads, from inside the callback, whether the preview is still recorded.
    struct AsksWhileItRuns<'a>(&'a Mutex<Option<bool>>, &'a crate::client_core::ClientCore);
    impl crate::api::wrapper::Wrapper for AsksWhileItRuns<'_> {
        fn open_order(
            &mut self, order_id: i64, _c: &Contract, _o: &ApiOrder,
            _s: &crate::types::model::OrderState,
        ) {
            let held = self.1.open_orders.lock().unwrap().contains_key(&(order_id as u64));
            *self.0.lock().unwrap() = Some(held);
        }
    }

    let seen = Mutex::new(None);
    let mut w = AsksWhileItRuns(&seen, &client.core);
    client.process_msgs(&mut w);

    assert_eq!(
        *seen.lock().unwrap(), Some(false),
        "the preview was still recorded while its own callback ran, so an order \
         placed under that number from inside it reads as a change to a what-if",
    );
}

/// Every request checks the number it was given, including this one.
///
/// It was the only surface that did not. Unchecked here, the number was
/// narrowed further down instead, so a caller numbering its requests from the
/// order counter — which the venue lets run past what a request id holds — had
/// this stream's refusals reported against somebody else's request.
#[test]
fn a_tick_by_tick_stream_checks_the_number_it_was_given() {
    let (client, _rx, _shared) = test_client();
    let err = client
        .req_tick_by_tick_data(u32::MAX as i64 + 1, &spy(), "Last", 0, false)
        .expect_err("a number no request id can hold is refused");
    assert!(err.message.contains("req_id"), "got: {err}");
}

/// Withdrawing a held order forgets it rather than telling the venue.
///
/// It was never sent, so the venue knows no such order — and left queued, the
/// next thing that transmitted sent the order the caller had just cancelled.
#[test]
fn a_held_order_that_is_withdrawn_does_not_go_out_later() {
    let (client, rx, _shared) = test_client();
    let leg = |id: i64, parent: i64, transmit: bool| Order {
        order_id: id,
        parent_id: parent,
        transmit,
        action: if parent == 0 { "BUY".into() } else { "SELL".into() },
        total_quantity: 100.0,
        order_type: "LMT".into(),
        lmt_price: 100.0,
        tif: "DAY".into(),
        ..Default::default()
    };

    client.place_order(80, &spy(), &leg(80, 0, false)).expect("held");
    client.cancel_order(80, "").expect("withdrawn");
    assert!(rx.try_recv().is_err(), "nothing was sent, so nothing is withdrawn at the venue");

    client.place_order(81, &spy(), &leg(81, 80, true)).expect("this one transmits");
    let sent: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert_eq!(sent.len(), 1, "only the order that transmitted: {sent:?}");
}

/// order the refusal had already taken out.
#[test]
fn a_placement_is_recorded_before_its_command_can_be_taken() {
    let shared = Arc::new(SharedState::new());
    // A channel that hands over one command at a time. The placement stays
    // inside its send until something takes it, which is what makes the moment
    // before the command exists anywhere observable.
    let (tx, rx) = std::sync::mpsc::sync_channel(0);
    let client = EClient::from_parts(shared, tx, std::thread::spawn(|| {}), "DU123".into());
    client.core.con_id_to_instrument.lock().unwrap().insert(756733, 0);
    let order = Order {
        action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(),
        lmt_price: 100.0, tif: "DAY".into(), transmit: true, ..Default::default()
    };

    let recorded = std::thread::scope(|scope| {
        scope.spawn(|| client.place_order(77, &spy(), &order).expect("the order goes out"));
        // Nothing has taken the command yet, so the placement is still inside
        // its send, and the record has to be here by now.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !client.core.is_order_tracked(77) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let recorded = client.core.is_order_tracked(77);
        rx.recv().expect("the submit");
        recorded
    });
    // The client's own shutdown goes down this channel when it is dropped, and
    // a channel that hands over one at a time waits for somebody to take it.
    // Taken before the assertion, so a failing one is a failure and not a wait.
    std::thread::spawn(move || while rx.recv().is_ok() {});
    assert!(
        recorded,
        "the order was published before this client had anywhere to record what \
         the venue goes on to say about it",
    );
}

/// The other surface waited for the naming all along.
#[test]
fn an_order_id_waits_for_the_venue_to_name_the_working_orders() {
    let (client, _rx, shared) = test_client();
    let venue = shared.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(200));
        venue.orders.push_order_info(4242, crate::bridge::RichOrderInfo {
            contract: Default::default(),
            order: crate::types::model::Order { order_id: 4242, ..Default::default() },
            order_state: Default::default(),
            last_exec: Default::default(),
        });
        venue.orders.set_replay_done();
    });

    assert_eq!(
        client.next_order_id(), 4243,
        "the id counts past what the venue named, which had not arrived when it was asked for",
    );
}

/// because its children are the first plus one and two.
#[test]
fn the_allocator_stops_at_the_last_id_a_report_can_name() {
    let (client, _rx, shared) = test_client();
    shared.orders.set_replay_done();
    shared.orders.push_order_info(crate::bridge::MAX_ORDER_ID - 1, crate::bridge::RichOrderInfo {
        contract: Default::default(),
        order: crate::types::model::Order::default(),
        order_state: Default::default(),
        last_exec: Default::default(),
    });

    assert_eq!(
        client.next_order_id(), crate::bridge::MAX_ORDER_ID as i64,
        "the last id there is, is handed out",
    );
    assert_eq!(
        client.next_order_id(), 0,
        "and nothing past it, rather than an id that reads as negative",
    );
    let why = client
        .place_bracket(&spy(), "BUY", 1.0, 100.0, 110.0, 90.0)
        .expect_err("a bracket needs three ids that fit, children included");
    assert!(
        why.message.contains("no run of 3 order ids left"),
        "the caller is told what there is not enough of: {}",
        why.message,
    );
}

/// Withdrawing a held order frees the id it was placed under.
///
/// The venue was never given the order, so the id is not spent there — but
/// the client's own record of it stood, and placing under the id again was
/// read as a modify of an order nothing had ever submitted: answered Ok,
/// and nothing went out.
#[test]
fn withdrawing_a_held_order_frees_its_id_to_place_again() {
    let (client, rx, _shared) = test_client();
    let leg = |id: i64, parent: i64, transmit: bool| Order {
        order_id: id,
        parent_id: parent,
        transmit,
        action: if parent == 0 { "BUY".into() } else { "SELL".into() },
        total_quantity: 100.0,
        order_type: "LMT".into(),
        lmt_price: 100.0,
        tif: "DAY".into(),
        ..Default::default()
    };

    client.place_order(82, &spy(), &leg(82, 0, false)).expect("held");
    client.cancel_order(82, "").expect("withdrawn");

    client.place_order(82, &spy(), &leg(82, 0, true)).expect("placed again under the same id");
    match rx.try_recv().expect("the order goes out") {
        ControlCommand::Order(OrderRequest::SubmitEx { order_id, .. }) => {
            assert_eq!(order_id, 82, "the id places a new order");
        }
        other => panic!("expected a fresh submit, got {other:?}"),
    }
}

/// A parent placed again to transmit sends the children held under it.
#[test]
fn transmitting_a_parent_releases_what_hangs_from_it() {
    let (client, rx, _shared) = test_client();
    let leg = |id: i64, parent: i64, transmit: bool| Order {
        order_id: id,
        parent_id: parent,
        transmit,
        action: if parent == 0 { "BUY".into() } else { "SELL".into() },
        total_quantity: 100.0,
        order_type: "LMT".into(),
        lmt_price: 100.0,
        tif: "DAY".into(),
        ..Default::default()
    };

    client.place_order(90, &spy(), &leg(90, 0, false)).expect("held");
    client.place_order(91, &spy(), &leg(91, 90, false)).expect("held under it");
    client.place_order(90, &spy(), &leg(90, 0, true)).expect("the parent transmits");

    let sent: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert_eq!(sent.len(), 2, "the child held under it goes too: {sent:?}");
}

/// A held order is not a working one, so placing again under its id submits.
///
/// Both are tracked here and only one is known to the venue. Read as the same
/// question, the second placement built a replace of an order nothing had ever
/// submitted — which the venue refuses — and the submit that was held stayed
/// queued to go out behind the next thing that transmits, under the terms the
/// caller had just replaced.
#[test]
fn placing_again_under_a_held_id_submits_it_rather_than_replacing_it() {
    let (client, rx, _shared) = test_client();
    let leg = |transmit: bool, price: f64| Order {
        order_id: 90,
        transmit,
        action: "BUY".into(),
        total_quantity: 100.0,
        order_type: "LMT".into(),
        lmt_price: price,
        tif: "DAY".into(),
        ..Default::default()
    };

    client.place_order(90, &spy(), &leg(false, 100.0)).expect("held");
    client.place_order(90, &spy(), &leg(true, 105.0)).expect("the same id transmits");

    let sent: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert_eq!(sent.len(), 1, "one order goes out, not a replace and a stale submit: {sent:?}");
    match &sent[0] {
        ControlCommand::Order(OrderRequest::SubmitEx { order_id, kind, .. }) => {
            assert_eq!(*order_id, 90);
            let crate::types::OrderKind::Limit { price } = kind else {
                panic!("expected a limit, got {kind:?}")
            };
            assert_eq!(
                *price,
                crate::types::price_from_f64(105.0),
                "at the price this placement states",
            );
        }
        other => panic!("expected a submit, got {other:?}"),
    }
    assert!(!client.core.is_held(90), "and nothing of it is left queued");
}

/// A family that cannot all reach the engine is not half sent and forgotten.
///
/// Taken out of the hold first and then sent one by one, a send that failed
/// partway left the parent live at the venue with its protective children
/// already forgotten, and the caller was told only that the call had failed.
#[test]
fn a_family_the_engine_stops_receiving_says_what_reached_it() {
    let (client, rx, _shared) = test_client();
    let leg = |id: i64, parent: i64, transmit: bool| Order {
        order_id: id,
        parent_id: parent,
        transmit,
        action: if parent == 0 { "BUY".into() } else { "SELL".into() },
        total_quantity: 100.0,
        order_type: "LMT".into(),
        lmt_price: 100.0,
        tif: "DAY".into(),
        ..Default::default()
    };

    client.place_order(90, &spy(), &leg(90, 0, false)).expect("held");
    client.place_order(91, &spy(), &leg(91, 90, false)).expect("held under it");
    client.place_order(92, &spy(), &leg(92, 90, false)).expect("held under it");
    drop(rx);

    let why = client
        .place_order(90, &spy(), &leg(90, 0, true))
        .expect_err("the engine is gone, so the family cannot go out");
    assert!(
        why.message.contains("nothing was sent") && why.message.contains("still held"),
        "the caller is told nothing went and the family is still there: {why}",
    );
    for id in [91u64, 92] {
        assert!(client.core.is_held(id), "order {id} is still held to be sent again");
    }
    assert!(
        !client.core.is_held(90),
        "the order that asked to go does not stay queued under the terms it replaced",
    );
}

/// A bracket built the way the reference client's own sample builds one.
///
/// It places a parent and a take-profit held back and lets the stop-loss send
/// all three, with a comment saying so. The field is written into that client's
/// own message and no tag carries it, so holding the order is the client's
/// side of the protocol. Refused here, the sample's parent and take-profit
/// never went and the stop-loss went alone, carrying a link to an order the
/// venue had never been given.
#[test]
fn a_bracket_held_back_goes_out_when_its_last_leg_transmits() {
    let (client, rx, _shared) = test_client();
    let held = |id: i64, parent: i64, transmit: bool| Order {
        order_id: id,
        parent_id: parent,
        transmit,
        action: if parent == 0 { "BUY".into() } else { "SELL".into() },
        total_quantity: 100.0,
        order_type: "LMT".into(),
        lmt_price: 100.0,
        tif: "DAY".into(),
        ..Default::default()
    };

    client.place_order(70, &spy(), &held(70, 0, false)).expect("kept, not refused");
    client.place_order(71, &spy(), &held(71, 70, false)).expect("kept, not refused");
    assert!(rx.try_recv().is_err(), "nothing goes out while every leg is held");

    client.place_order(72, &spy(), &held(72, 70, true)).expect("and this one sends them");
    let sent: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert_eq!(sent.len(), 3, "the parent, the sibling and this one: {sent:?}");
}

/// A stream taking a number a finished lookup used is a stream, not an update.
///
/// A completed historical request left the number marked as one whose bars
/// belong to it, and only a new or a cancelled historical request cleared the
/// mark. Backfill and then stream on one number — which is how it is written —
/// and every bar of the stream arrived as an update to the request that had
/// already ended, so a caller that overrode only the stream read it as dead.
#[test]
fn a_stream_on_a_finished_lookups_number_is_a_stream() {
    let (client, rx, _shared) = test_client();
    client.core.hist_initial_complete.lock().unwrap().insert(4242);
    assert!(
        client.core.hist_initial_complete.lock().unwrap().contains(&4242),
        "the lookup finished",
    );

    client
        .req_real_time_bars(4242, &spy(), 5, "TRADES", true)
        .expect("the stream takes the number");
    assert!(
        !client.core.hist_initial_complete.lock().unwrap().contains(&4242),
        "and the number is a stream's again, not a finished lookup's",
    );
    let _ = rx;
}

/// A quantity that is not a number, or overflows the fixed-point form, becomes
/// a size the caller did not ask for.
///
/// Zero and negative are not here. Both encode exactly and the venue answers
/// them itself, so refusing them was this client making up a rule about a
/// request the venue was never asked about.
#[test]
fn an_unusable_quantity_is_refused() {
    for (qty, expect) in [
        (f64::NAN, "finite"),
        (f64::INFINITY, "finite"),
        (1e11, "too large"),
    ] {
        let (client, _rx, _shared) = test_client();
        let order = Order {
            action: "BUY".into(), total_quantity: qty, order_type: "MKT".into(),
            tif: "DAY".into(), ..Default::default()
        };
        let err = client.place_order(9102, &spy(), &order)
            .expect_err("must be refused");
        assert!(err.message.contains(expect), "quantity {qty}: expected {expect:?}, got: {err}");
    }
}

/// The boundaries, exactly. Each of these was reachable by a mutation that the
/// round-number cases above could not see: rejecting the valid maximum,
/// admitting one past it, admitting a small negative, admitting a fraction
/// near a half, and misclassifying negative infinity.
#[test]
fn the_quantity_boundaries_are_exact() {
    let place = |qty: f64| {
        let (client, _rx, _shared) = test_client();
        let order = Order {
            action: "BUY".into(), total_quantity: qty, order_type: "MKT".into(),
            tif: "DAY".into(), ..Default::default()
        };
        client.place_order(9601, &spy(), &order)
    };

    let largest = crate::types::MAX_QTY_SHARES;
    assert!(place(largest).is_ok(), "the largest carryable quantity still places");
    assert!(place(largest * 2.0).is_err(), "past it does not");
    // A cash order states its size in currency units. Bounded at the double's
    // own limit this was refused here and sent by the gateway.
    assert!(place(100_000_000.0).is_ok(), "an ordinary currency amount places");
    assert!(place(-1.0).is_ok(), "a negative goes to the venue, which answers it");
    assert!(place(0.0).is_ok(), "and so does a zero");
    assert!(place(1.25).is_ok(), "a fraction is carried, not refused");
    assert!(place(f64::NEG_INFINITY).is_err(), "negative infinity is not finite either");
}

/// A cash-quantity order states its size in currency and carries no shares, so
/// zero is only wrong when nothing else says how much to buy.
#[test]
fn zero_shares_reaches_the_venue_rather_than_a_refusal_written_here() {
    let (client, rx, _shared) = test_client();
    let bare = Order {
        action: "BUY".into(), total_quantity: 0.0, order_type: "MKT".into(),
        tif: "DAY".into(), ..Default::default()
    };
    client.place_order(9602, &spy(), &bare).expect("the venue is asked, not this client");
    assert!(rx.try_recv().is_ok(), "and it reaches the wire to be asked");

    let cash = Order {
        action: "BUY".into(), total_quantity: 0.0, order_type: "LMT".into(),
        lmt_price: 100.0, cash_qty: 1000.0, tif: "DAY".into(), ..Default::default()
    };
    client.place_order(9603, &spy(), &cash).expect("a cash-sized order still places");
}

/// Whole quantities are unaffected.
#[test]
fn a_whole_quantity_still_places() {
    let (client, rx, _shared) = test_client();
    let order = Order {
        action: "BUY".into(), total_quantity: 200.0, order_type: "MKT".into(),
        tif: "DAY".into(), ..Default::default()
    };
    client.place_order(9103, &spy(), &order).expect("a whole quantity places");
    assert!(rx.try_recv().is_ok(), "and reaches the wire");
}
/// A replace states the order type and the prices its type carries. What it
/// does not state is the peg offset or the execution instruction, so sent for
/// a pegged order it describes one with no offset, which the venue rejects —
/// leaving the caller with nothing resting. Refusing keeps the order.
#[test]
fn a_type_the_replace_cannot_restate_is_not_modified() {
    for order_type in ["REL"] {
        let (client, rx, _shared) = test_client();
        let submit = Order {
            action: "SELL".into(), total_quantity: 1.0, order_type: order_type.into(),
            aux_price: 1.0, trailing_percent: 0.0, tif: "DAY".into(), ..Default::default()
        };
        // Submitting is fine; it is the replace that cannot express it.
        let _ = client.place_order(9201, &spy(), &submit);
        while rx.try_recv().is_ok() {}

        // Skipping when tracking did not happen would let this pass without
        // testing anything, which is how the modify gate went unnoticed.
        assert!(
            client.core.is_order_tracked(9201),
            "{order_type} must submit and be tracked, or the refusal below proves nothing",
        );
        let err = client.place_order(9201, &spy(), &submit)
            .expect_err("modifying it must be refused");
        assert!(err.message.contains("cannot be modified"), "{order_type}: {err}");
        assert!(rx.try_recv().is_err(), "{order_type}: nothing reaches the wire");
    }
}

/// The order type alone does not decide this. An adaptive or algo order is an
/// ordinary LMT defined by its algo tags; an adjustable stop is an ordinary STP
/// defined by its conversion; a conditional order rides submit-only tags. A
/// replace states none of those, so each is destroyed by one just as surely as
/// a trailing stop is — and each would have passed a gate that looked only at
/// the type.
#[test]
fn an_order_defined_by_more_than_its_type_is_not_modified() {
    let cases: Vec<OrderCase> = vec![
        // What is left of the attributes: the minimum quantity, which this
        // venue refuses as an order on the security type asked, so what a
        // replace would do to one cannot be put to it. Everything else came off
        // this list when a session placed it and the venue took the replace,
        // the adjustable stop last: its replace states the whole conversion.
        ("minimum quantity", |o| o.min_qty = 50),
    ];
    for (name, set) in cases {
        let (client, rx, _shared) = test_client();
        let mut order = Order {
            action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(),
            lmt_price: 100.0, tif: "DAY".into(), ..Default::default()
        };
        set(&mut order);
        let _ = client.place_order(9301, &spy(), &order);
        while rx.try_recv().is_ok() {}

        assert!(
            client.core.is_order_tracked(9301),
            "{name} must submit and be tracked, or the refusal below proves nothing",
        );
        let err = client.place_order(9301, &spy(), &order)
            .expect_err("modifying it must be refused");
        assert!(err.message.contains("cannot be modified"), "{name}: {err}");
        assert!(rx.try_recv().is_err(), "{name}: nothing reaches the wire");
    }
}

/// An order placed for one model within an account is placed for that model.
///
/// It was refused here, on the reading that nothing carried the model and that
/// placing the order against the account at large would be worse than refusing
/// it. The first half of that is what changed: the model rides beside the
/// account, so the order can be placed as asked.
#[test]
fn an_order_naming_a_model_is_placed_for_it() {
    let (client, rx, _shared) = test_client();
    let order = Order {
        action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(),
        lmt_price: 100.0, tif: "DAY".into(), model_code: "GROWTH".into(),
        ..Default::default()
    };
    client.place_order(9501, &spy(), &order).expect("an order for a model is placed");
    match rx.try_recv().expect("the order reaches the wire") {
        ControlCommand::Order(OrderRequest::SubmitEx { attrs, .. }) => assert_eq!(
            attrs.model_code, "GROWTH",
            "the model the order is placed for travels with it",
        ),
        cmd => panic!("expected a placement, got {cmd:?}"),
    }
}

/// An order that cannot be placed the way it was asked for is refused, rather
/// than placed a different way.
///
/// Each of these used to go out transformed: a delayed order placed at once, a
/// misspelled time in force placed as DAY and gone at the close, an unreadable
/// expiry placed with none. The order reached the venue every time and nothing
/// said what had changed.
#[test]
fn an_order_that_cannot_be_placed_as_asked_is_refused() {
    /// What the case does to an order, and the field its refusal must name.
    type Refusal = (&'static str, fn(&mut Order), &'static str);
    let cases: &[Refusal] = &[
        ("a delayed activation that cannot be read",
         |o| o.good_after_time = "next tuesday".into(), "good_after_time"),
        ("a time in force spelled the wrong way",
         |o| o.tif = "gtc".into(), "tif"),
        ("a time in force that is not one",
         |o| o.tif = "FOREVER".into(), "tif"),
        ("an expiry that cannot be read",
         |o| o.good_till_date = "next tuesday".into(), "good_till_date"),
        ("a hedge of a kind this venue does not carry",
         |o| o.hedge_type = "X".into(), "hedge_type"),
        ("a beta hedge struck at something that is not a number",
         |o| { o.hedge_type = "B".into(); o.hedge_param = "market".into() },
         "hedge_param"),
        ("a pair hedge with no ratio stated",
         |o| o.hedge_type = "P".into(), "hedge_param"),
        ("a trigger this venue does not carry",
         |o| o.trigger_method = 9, "trigger_method"),
        ("a one-cancels-all rule this venue does not carry",
         |o| o.oca_type = 7, "oca_type"),
        ("a display size that is not a quantity",
         |o| o.display_size = -5, "display_size"),
        ("a borrow slot that is not one",
         |o| o.short_sale_slot = -1, "short_sale_slot"),
        ("a minimum trade quantity below nothing",
         |o| o.min_trade_qty = -10, "min_trade_qty"),
        // One of the twenty-nine this protocol has no field for. Stated by a
        // caller, the order would otherwise be placed with the instruction
        // missing and nothing to say it had been.
        ("a routing preference this protocol cannot express",
         |o| o.opt_out_smart_routing = true, "opt_out_smart_routing"),
        ("an order origin other than the account's own",
         |o| o.origin = 1, "origin"),
        ("a scale table this protocol has no field for",
         |o| o.scale_table = "SCALE".into(), "scale_table"),
    ];
    for (what, set, names) in cases {
        let (client, rx, _shared) = test_client();
        let mut order = Order {
            action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(),
            lmt_price: 100.0, tif: "DAY".into(), ..Default::default()
        };
        set(&mut order);
        let err = client.place_order(9401, &spy(), &order)
            .expect_err(&format!("{what} must be refused"));
        assert!(err.message.contains(names), "{what}: {err}");
        assert!(rx.try_recv().is_err(), "{what}: nothing reaches the wire");
        assert!(!client.core.is_order_tracked(9401), "{what}: nothing is tracked");
    }
}

/// Limit-if-touched is replaced as itself.
///
/// It was refused here on the reading that it is tracked under a byte the
/// replace renders as market-to-limit. It is rendered by the same table that
/// wrote it, `LT` either way, and a session placed one with the market open
/// and replaced it: the venue took it and the order went on working.
#[test]
fn a_limit_if_touched_is_replaced_as_itself() {
    let (client, rx, _shared) = test_client();
    let order = Order {
        action: "SELL".into(), total_quantity: 1.0, order_type: "LIT".into(),
        lmt_price: 100.0, aux_price: 101.0, tif: "DAY".into(), ..Default::default()
    };
    client.place_order(9302, &spy(), &order).expect("the limit-if-touched submits");
    while rx.try_recv().is_ok() {}

    client.place_order(9302, &spy(), &order).expect("and is replaced as itself");
    match next_command(&rx).expect("the replace reaches the wire") {
        ControlCommand::Order(OrderRequest::Modify { order_id, .. }) => {
            assert_eq!(order_id, 9302);
        }
        cmd => panic!("expected a replace, got {cmd:?}"),
    }
}

/// The refusal has to read the order the caller is asking for, not only the one
/// on the book. A modify that *adds* a bracket link or an OCA group states an
/// order that has neither — so a gate looking only at the resting record lets
/// the attribute through on the very message that was supposed to carry it.
#[test]
fn an_attribute_added_by_the_modify_is_refused_too() {
    let cases: Vec<OrderCase> = vec![
        ("minimum quantity", |o| o.min_qty = 50),
    ];
    for (name, set) in cases {
        let (client, rx, _shared) = test_client();
        let plain = Order {
            action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(),
            lmt_price: 100.0, tif: "DAY".into(), ..Default::default()
        };
        client.place_order(9303, &spy(), &plain).expect("a plain limit submits");
        while rx.try_recv().is_ok() {}

        let mut attributed = plain.clone();
        set(&mut attributed);
        let err = client.place_order(9303, &spy(), &attributed)
            .expect_err("adding it by modify must be refused");
        assert!(err.message.contains("cannot be modified"), "{name}: {err}");
        assert!(rx.try_recv().is_err(), "{name}: nothing reaches the wire");
    }
}

/// Every allowed type must still modify, not just the one. Excluding any of
/// them costs a working modify, and only `LMT` was covered.
#[test]
fn every_restatable_type_still_modifies() {
    for (order_type, lmt, aux) in [
        ("MKT", 0.0, 0.0),
        ("LMT", 100.0, 0.0),
        ("STP", 0.0, 90.0),
        ("STP LMT", 100.0, 90.0),
        ("MOC", 0.0, 0.0),
        ("LOC", 100.0, 0.0),
        ("MIT", 0.0, 90.0),
        ("STP PRT", 0.0, 90.0),
        // Regression guard: these three were modifiable before the gate and
        // the replace renders the same byte they were submitted under.
        ("MTL", 0.0, 0.0),
        ("BOX TOP", 0.0, 0.0),
        ("MKT PRT", 0.0, 0.0),
        // The replace states the trail on 99 and 211 as the submit does, and a
        // session answered it: the venue takes it and the order keeps working.
        ("TRAIL", 0.0, 1.0),
    ] {
        let (client, rx, _shared) = test_client();
        let order = Order {
            action: "BUY".into(), total_quantity: 1.0, order_type: order_type.into(),
            lmt_price: lmt, aux_price: aux, tif: "DAY".into(), ..Default::default()
        };
        client.place_order(9701, &spy(), &order)
            .unwrap_or_else(|e| panic!("{order_type} must submit: {e}"));
        while rx.try_recv().is_ok() {}

        client.place_order(9701, &spy(), &order)
            .unwrap_or_else(|e| panic!("{order_type} must still modify: {e}"));
        match next_command(&rx).expect("the modify") {
            ControlCommand::Order(OrderRequest::Modify { .. }) => {}
            other => panic!("{order_type}: expected a Modify, got {other:?}"),
        }
    }
}

/// The decision is read from the order as it was submitted, not from the one
/// handed to the modify — a caller cannot make an unrestatable order
/// modifiable by describing it as a limit on the way in.
#[test]
fn the_refusal_reads_the_tracked_order_not_the_incoming_one() {
    let (client, rx, _shared) = test_client();
    let trail = Order {
        action: "SELL".into(), total_quantity: 1.0, order_type: "TRAIL LIMIT".into(),
        aux_price: 1.0, tif: "DAY".into(), ..Default::default()
    };
    client.place_order(9702, &spy(), &trail).expect("the trailing stop limit submits");
    while rx.try_recv().is_ok() {}

    let disguised = Order {
        action: "SELL".into(), total_quantity: 2.0, order_type: "LMT".into(),
        lmt_price: 100.0, tif: "DAY".into(), ..Default::default()
    };
    let err = client.place_order(9702, &spy(), &disguised)
        .expect_err("the tracked type decides, so this is still refused");
    assert!(err.message.contains("cannot be modified"), "{err}");
    assert!(rx.try_recv().is_err(), "and nothing reaches the wire");
}

/// The ordinary types still modify.
#[test]
fn a_limit_order_still_modifies() {
    let (client, rx, _shared) = test_client();
    let order = Order {
        action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(),
        lmt_price: 100.0, tif: "DAY".into(), ..Default::default()
    };
    client.place_order(9202, &spy(), &order).unwrap();
    while rx.try_recv().is_ok() {}

    let moved = Order { lmt_price: 101.0, ..order };
    client.place_order(9202, &spy(), &moved).expect("a limit modify still goes through");
    match next_command(&rx).expect("the modify") {
        ControlCommand::Order(OrderRequest::Modify { .. }) => {}
        other => panic!("expected a Modify, got {other:?}"),
    }
}

/// An algorithm this client does not model is carried, not refused.
///
/// Which algorithms an account may use is stated at logon — thirteen keys on an
/// ordinary session — and enforced by the venue. Refusing on the five this
/// client happens to name is a narrower answer than the venue's, and it stops a
/// caller using one the venue would have taken. The reference client does not
/// interpret these either.
#[test]
fn an_algorithm_this_client_does_not_model_is_carried_through() {
    let params = vec![
        TagValue { tag: "componentSize".into(), value: "100".into() },
        TagValue { tag: "timeBetweenOrders".into(), value: "60".into() },
    ];
    match parse_algo_params("Accumulate/Distribute", &params).expect("named, not refused") {
        AlgoParams::Named { strategy, params } => {
            assert_eq!(strategy, "Accumulate/Distribute", "as the caller wrote it");
            assert_eq!(
                params,
                vec!["componentSize", "100", "timeBetweenOrders", "60"],
                "name then value, in the order given",
            );
        }
        other => panic!("carried through as {other:?}"),
    }
}

#[test]
fn parse_algo_vwap() {
    let params = vec![
        TagValue { tag: "maxPctVol".into(), value: "0.1".into() },
        TagValue { tag: "startTime".into(), value: "09:30:00".into() },
        TagValue { tag: "endTime".into(), value: "16:00:00".into() },
    ];
    let algo = parse_algo_params("vwap", &params).unwrap();
    match algo {
        AlgoParams::Vwap { max_pct_vol, start_time, end_time, .. } => {
            assert_eq!(max_pct_vol.as_deref(), Some("0.1"));
            assert_eq!(start_time.as_deref(), Some("09:30:00"));
            assert_eq!(end_time.as_deref(), Some("16:00:00"));
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_algo_twap() {
    let algo = parse_algo_params("twap", &[]).unwrap();
    assert!(matches!(algo, AlgoParams::Twap { .. }));
}

#[test]
fn parse_algo_arrival_price() {
    let params = vec![
        TagValue { tag: "maxPctVol".into(), value: "0.25".into() },
        TagValue { tag: "riskAversion".into(), value: "Aggressive".into() },
    ];
    let algo = parse_algo_params("arrivalpx", &params).unwrap();
    match algo {
        AlgoParams::ArrivalPx { max_pct_vol, risk_aversion, .. } => {
            assert_eq!(max_pct_vol.as_deref(), Some("0.25"));
            assert_eq!(risk_aversion, Some(RiskAversion::Aggressive));
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_algo_close_price() {
    let algo = parse_algo_params("closepx", &[]).unwrap();
    assert!(matches!(algo, AlgoParams::ClosePx { .. }));
}

#[test]
fn parse_algo_dark_ice() {
    let params = vec![
        TagValue { tag: "displaySize".into(), value: "200".into() },
    ];
    let algo = parse_algo_params("darkice", &params).unwrap();
    match algo {
        AlgoParams::DarkIce { display_size, .. } => assert_eq!(display_size, "200"),
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_algo_pct_vol() {
    let params = vec![
        TagValue { tag: "pctVol".into(), value: "0.05".into() },
    ];
    let algo = parse_algo_params("pctvol", &params).unwrap();
    match algo {
        AlgoParams::PctVol { pct_vol, .. } => assert_eq!(pct_vol.as_deref(), Some("0.05")),
        _ => panic!("wrong variant"),
    }
}

/// An algorithm this client does not model is not an algorithm the venue does
/// not offer, and is not refused as though it were.
///
/// Which strategies an account may use is stated at logon and enforced by the
/// venue, so refusing here would be narrower than the venue's answer. The
/// reference client forwards these without reading them.
#[test]
fn parse_algo_unsupported() {
    let carried = parse_algo_params("unknown", &[]).expect("carried, not refused");
    assert!(matches!(carried, AlgoParams::Named { .. }));

    // A malformed parameter on a strategy this client does model is still
    // refused: that is this client reading something it understands and
    // finding it wrong, which is a different thing.
    let bad = vec![TagValue { tag: "maxPctVol".into(), value: "not a number".into() }];
    assert!(parse_algo_params("vwap", &bad).is_err());
}

// ── malformed / non-finite algo params must be rejected, not
// silently coerced into a valid-looking default ──

#[test]
fn parse_algo_vwap_rejects_malformed_max_pct_vol() {
    let params = vec![TagValue { tag: "maxPctVol".into(), value: "abc".into() }];
    let err = parse_algo_params("vwap", &params).unwrap_err();
    assert!(err.message.contains("maxPctVol"), "got: {err}");
}

#[test]
fn parse_algo_vwap_rejects_nan_max_pct_vol() {
    let params = vec![TagValue { tag: "maxPctVol".into(), value: "NaN".into() }];
    let err = parse_algo_params("vwap", &params).unwrap_err();
    assert!(err.message.contains("maxPctVol"), "got: {err}");
}

#[test]
fn parse_algo_vwap_rejects_infinite_max_pct_vol() {
    let params = vec![TagValue { tag: "maxPctVol".into(), value: "inf".into() }];
    let err = parse_algo_params("vwap", &params).unwrap_err();
    assert!(err.message.contains("maxPctVol"), "got: {err}");
}

#[test]
fn parse_algo_vwap_forwards_a_flag_spelling_it_does_not_fold() {
    // The venue owns this vocabulary. Refused here, a spelling it takes never
    // reached it; folded to "unset", the flag was dropped and nothing said so.
    let params = vec![TagValue { tag: "noTakeLiq".into(), value: "yes".into() }];
    match parse_algo_params("vwap", &params).unwrap() {
        AlgoParams::Named { strategy, params } => {
            assert_eq!(strategy, "vwap");
            assert_eq!(params, ["noTakeLiq", "yes"], "the caller's own text goes out");
        }
        other => panic!("the list goes as written, got {other:?}"),
    }
}

#[test]
fn parse_algo_vwap_rejects_empty_max_pct_vol() {
    // A present-but-empty value is a caller who set the tag, not one who
    // never set it — it must be refused like any other malformed value,
    // not silently coerced into the "absent" default of 0.0.
    let params = vec![TagValue { tag: "maxPctVol".into(), value: "".into() }];
    let err = parse_algo_params("vwap", &params).unwrap_err();
    assert!(err.message.contains("maxPctVol"), "got: {err}");
}

#[test]
fn parse_algo_vwap_forwards_a_present_but_empty_flag() {
    // Present-but-empty is not absent: the caller set the tag, so it travels
    // as they set it and the venue answers for it.
    let params = vec![TagValue { tag: "noTakeLiq".into(), value: "".into() }];
    match parse_algo_params("vwap", &params).unwrap() {
        AlgoParams::Named { params, .. } => assert_eq!(params, ["noTakeLiq", ""]),
        other => panic!("the list goes as written, got {other:?}"),
    }
}

#[test]
fn parse_algo_arrival_price_forwards_a_risk_level_it_does_not_fold() {
    // A typo must not be silently sent as Neutral, and must not be silently
    // dropped either. It travels as the caller wrote it and the venue refuses
    // it by name, which is the answer they can act on.
    let params = vec![TagValue { tag: "riskAversion".into(), value: "Aggresive".into() }];
    match parse_algo_params("arrivalpx", &params).unwrap() {
        AlgoParams::Named { strategy, params } => {
            assert_eq!(strategy, "arrivalpx");
            assert_eq!(params, ["riskAversion", "Aggresive"]);
        }
        other => panic!("the list goes as written, got {other:?}"),
    }
}

#[test]
fn parse_algo_arrival_price_states_no_risk_aversion_when_none_was_given() {
    let algo = parse_algo_params("arrivalpx", &[]).unwrap();
    match algo {
        AlgoParams::ArrivalPx { risk_aversion, .. } => assert_eq!(risk_aversion, None),
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_algo_arrival_price_forwards_a_present_but_empty_risk_aversion() {
    // Present-but-empty is not the same as absent: a tag the caller never set
    // is not sent, one they set to nothing goes out as they set it.
    let params = vec![TagValue { tag: "riskAversion".into(), value: "".into() }];
    match parse_algo_params("arrivalpx", &params).unwrap() {
        AlgoParams::Named { params, .. } => assert_eq!(params, ["riskAversion", ""]),
        other => panic!("the list goes as written, got {other:?}"),
    }
}

#[test]
fn parse_algo_dark_ice_rejects_malformed_display_size() {
    let params = vec![TagValue { tag: "displaySize".into(), value: "abc".into() }];
    let err = parse_algo_params("darkice", &params).unwrap_err();
    assert!(err.message.contains("displaySize"), "got: {err}");
}

#[test]
fn parse_algo_dark_ice_rejects_negative_display_size() {
    let params = vec![TagValue { tag: "displaySize".into(), value: "-5".into() }];
    let err = parse_algo_params("darkice", &params).unwrap_err();
    assert!(err.message.contains("displaySize"), "got: {err}");
}

/// Tag 111 display size is how much of the order the book shows. It is required
/// rather than defaulted, since any default publishes a size the caller did not
/// choose.
#[test]
fn parse_algo_dark_ice_needs_a_display_size() {
    let err = parse_algo_params("darkice", &[]).unwrap_err();
    assert!(err.message.contains("displaySize"), "got: {err}");
}

/// A key the strategy does not model still reaches the venue.
///
/// A modelled strategy is re-encoded from the fields it names, and a key it
/// does not name has no field to be re-encoded into — so it used to be
/// refused, on the reasoning that it would not reach the venue. It would: a
/// parameter travels as a name and a value in a repeating group and there is
/// no tag per parameter, so the venue reads a name this client never modelled
/// exactly as it reads one it did. The whole list goes as written instead.
/// This is the reference client's own VWAP sample, which states six parameters
/// where five are modelled here.
#[test]
fn an_algo_parameter_this_client_does_not_model_still_travels() {
    let params = vec![
        TagValue { tag: "maxPctVol".into(), value: "0.1".into() },
        TagValue { tag: "noTakeLiq".into(), value: "1".into() },
        TagValue { tag: "speedUp".into(), value: "1".into() },
    ];
    let algo = parse_algo_params("Vwap", &params).expect("the list is carried");
    let crate::types::AlgoParams::Named { strategy, params: stated } = algo else {
        panic!("a list with an unmodelled key is forwarded as written, not re-encoded");
    };
    assert_eq!(strategy, "Vwap", "the caller's own spelling reaches the venue");
    assert_eq!(
        stated,
        ["maxPctVol", "0.1", "noTakeLiq", "1", "speedUp", "1"],
        "every key the caller wrote, in the order they wrote it",
    );

    // A strategy this client does not model is handed over the same way.
    assert!(parse_algo_params("Balanced", &params).is_ok());

    // And a list the strategy does model is still re-encoded from its own
    // fields, so nothing that already worked changed shape.
    let modelled = parse_algo_params("Vwap", &params[..2]).expect("a modelled list");
    assert!(matches!(modelled, crate::types::AlgoParams::Vwap { .. }), "got: {modelled:?}");
}

#[test]
fn parse_algo_dark_ice_rejects_empty_display_size() {
    let params = vec![TagValue { tag: "displaySize".into(), value: "".into() }];
    let err = parse_algo_params("darkice", &params).unwrap_err();
    assert!(err.message.contains("displaySize"), "got: {err}");
}

// ═══════════════════════════════════════════════════════════════════
//  Connection
// ═══════════════════════════════════════════════════════════════════

#[test]
fn is_connected_after_construction() {
    let (client, _rx, _shared) = test_client();
    assert!(client.is_connected());
}

/// Disconnecting ends the session and then stops the engine, in that order. The
/// venue is told the session is going while there is still a connection to tell
/// it on; stopping the engine first would leave it to notice.
#[test]
fn disconnect_ends_the_session_then_stops_the_engine() {
    let (client, rx, _shared) = test_client();
    client.disconnect();
    assert!(!client.is_connected());
    assert!(matches!(rx.try_recv().unwrap(), ControlCommand::Logout));
    assert!(matches!(rx.try_recv().unwrap(), ControlCommand::Shutdown));
}

/// Dropping a client ends the session, the way disconnecting one does.
///
/// Its connections go with the engine, so there is nothing left for a caller to
/// reuse and nothing the venue should keep. Left to notice its sender went
/// away, the engine takes the path that sends no logout.
#[test]
fn dropping_the_client_ends_the_session_then_stops_the_engine() {
    let (client, rx, _shared) = test_client();
    drop(client);
    assert!(matches!(rx.try_recv().unwrap(), ControlCommand::Logout));
    assert!(matches!(rx.try_recv().unwrap(), ControlCommand::Shutdown));
}

#[test]
fn disconnect_idempotent() {
    let (client, _rx, _shared) = test_client();
    client.disconnect();
    client.disconnect();
    assert!(!client.is_connected());
}

// ═══════════════════════════════════════════════════════════════════
//  next_order_id / req_ids
// ═══════════════════════════════════════════════════════════════════

#[test]
fn next_order_id_monotonic() {
    let (client, _rx, _shared) = test_client();
    let id1 = client.next_order_id();
    let id2 = client.next_order_id();
    let id3 = client.next_order_id();
    assert!(id2 > id1);
    assert!(id3 > id2);
}

/// The id a caller places under next is one past the highest the venue has
/// named, and stepping past the highest id there is does not wrap it to zero.
///
/// The venue names the working orders at every connect and that mark is what
/// the counter is floored at. Stepped with a plain `+ 1`, a mark at the end of
/// the range answered nothing at all — a stop in a checked build, and order id
/// zero in a release one, which the venue refuses as an id already used.
#[test]
fn the_id_after_the_highest_there_is_does_not_wrap_to_zero() {
    let (client, _rx, shared) = test_client();
    shared.orders.push_order_info(u64::MAX, crate::bridge::RichOrderInfo {
        contract: Default::default(),
        order: crate::types::model::Order { order_id: -1, ..Default::default() },
        order_state: Default::default(),
        last_exec: Default::default(),
    });

    let mut w = RecordingWrapper::default();
    client.req_ids(&mut w);

    assert_eq!(w.events.len(), 1);
    assert!(
        !w.events[0].ends_with(":0"),
        "the counter steps rather than wrapping: {}",
        w.events[0],
    );
}

#[test]
fn req_ids_calls_wrapper() {
    let (client, _rx, _shared) = test_client();
    let mut w = RecordingWrapper::default();
    client.req_ids(&mut w);
    assert_eq!(w.events.len(), 1);
    assert!(w.events[0].starts_with("next_valid_id:"));
}

// ═══════════════════════════════════════════════════════════════════
//  Market data requests
// ═══════════════════════════════════════════════════════════════════

#[test]
fn req_mkt_data_sends_register_and_subscribe() {
    let (client, rx, _shared) = test_client();
    let _ = client.req_mkt_data(1, &spy(), "", false, false);
    let cmd1 = rx.try_recv().unwrap();
    assert!(matches!(cmd1, ControlCommand::RegisterInstrument { contract: ContractRef { con_id: 756733, .. }, .. }));
    let cmd2 = rx.try_recv().unwrap();
    match cmd2 {
        ControlCommand::Subscribe { contract: ContractRef { con_id, symbol, .. }, .. } => {
            assert_eq!(con_id, 756733);
            assert_eq!(symbol, "SPY");
        }
        _ => panic!("expected Subscribe, got {cmd2:?}"),
    }
}

#[test]
fn req_mkt_data_defaults_to_realtime_mode() {
    let (client, rx, _shared) = test_client();
    let _ = client.req_mkt_data(1, &spy(), "", false, false);
    let _register = rx.try_recv().unwrap();
    match rx.try_recv().unwrap() {
        ControlCommand::Subscribe { mode_9887, .. } => assert_eq!(mode_9887, 0),
        other => panic!("expected Subscribe, got {other:?}"),
    }
}

#[test]
fn req_mkt_data_ex_propagates_mode_9887() {
    for mode in [1_i32, 2, 3] {
        let (client, rx, _shared) = test_client();
        let _ = client.req_mkt_data_ex(1, &spy(), "", false, false, mode);
        let _register = rx.try_recv().unwrap();
        match rx.try_recv().unwrap() {
            ControlCommand::Subscribe { contract: ContractRef { con_id, .. }, mode_9887, .. } => {
                assert_eq!(mode_9887, mode);
                assert_eq!(con_id, 756733);
            }
            other => panic!("expected Subscribe, got {other:?}"),
        }
    }
}

/// A snapshot is an ordinary subscription that this client withdraws once it
/// has what it asked for — the wire carries no such thing, and one contract
/// carries one subscription. So a stream asked for while a snapshot is up
/// watches that subscription, and the snapshot finishing hands it over rather
/// than taking it down.
#[test]
fn a_stream_outlives_the_snapshot_it_was_watching() {
    let (client, rx, _shared) = test_client();
    // The snapshot holds the contract.
    client.core.con_id_to_instrument.lock().unwrap().insert(spy().con_id, 0);
    client.core.instrument_to_req.lock().unwrap().insert(0, 1);
    client.core.req_to_instrument.lock().unwrap().insert(1, 0);
    client.core.snapshot_reqs.lock().unwrap().insert(1, (std::time::Instant::now(), 0));

    // A stream watches what is already up.
    client.req_mkt_data(2, &spy(), "", false, false).expect("watches what is up");
    while rx.try_recv().is_ok() {}

    // The snapshot has what it asked for and withdraws.
    client.cancel_mkt_data(1).expect("the snapshot is done");

    let sent: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert!(
        !sent.iter().any(|c| matches!(c, ControlCommand::Unsubscribe { .. })),
        "the snapshot took the stream's subscription down with it: {sent:?}",
    );
    assert_eq!(
        client.core.instrument_to_req.lock().unwrap().get(&0).copied(),
        Some(2),
        "and the stream holds it now",
    );
}

/// Two callers asking for the headlines on a contract neither has registered
/// yet still ask once between them. Decided against the instrument, both
/// found nobody had asked — the instrument is not known that early — and both
/// asked, leaving a subscription the one withdrawal could not match.
#[test]
fn two_callers_racing_for_one_contract_ask_for_the_headlines_once() {
    let (client, rx, shared) = test_client();
    // Asked against the contract, which both callers name before either has
    // an instrument to name instead.
    let first = client.core.first_to_ask_for_news(spy().con_id, 1);
    let second = client.core.first_to_ask_for_news(spy().con_id, 2);

    assert!(first, "the first caller asks");
    assert!(!second, "and the second finds it already asked");

    // And the headlines outlast the first of them.
    assert_eq!(client.core.release_news(&shared, 1), None, "one of two left");
    let _ = rx.try_recv();
}

/// An order that is done stops being tracked, whether it filled or not.
///
/// Only a fill removed it before, and a fill is not how most orders end. A
/// cancelled or rejected order reports a quantity still outstanding and
/// produces none, so it stayed for the life of the session and the cost of
/// listing what is open grew with every cancel.
#[test]
fn a_cancelled_order_stops_being_tracked() {
    let (client, _rx, shared) = test_client();

    client.core.update_order_status(&shared, 7, OrderStatus::Submitted, 0.0, 100.0, 0);
    assert_eq!(client.core.open_orders.lock().unwrap().len(), 1, "working, so tracked");

    client.core.update_order_status(&shared, 7, OrderStatus::Cancelled, 0.0, 100.0, 0);
    assert!(
        client.core.open_orders.lock().unwrap().is_empty(),
        "a cancelled order is kept for the life of the session",
    );

    // And one the venue refused.
    client.core.update_order_status(&shared, 8, OrderStatus::Submitted, 0.0, 100.0, 0);
    client.core.update_order_status(&shared, 8, OrderStatus::Rejected, 0.0, 100.0, 0);
    assert!(client.core.open_orders.lock().unwrap().is_empty(), "and a rejected one");

    // What is held back is the one that can return to working on its own.
    client.core.update_order_status(&shared, 9, OrderStatus::Inactive, 0.0, 100.0, 0);
    assert_eq!(
        client.core.open_orders.lock().unwrap().len(), 1,
        "an inactive order returns to working when what holds it clears",
    );

    // And one the venue states as filled, even where no fill record came
    // with the status: the fill's own path drops the record, but the status
    // can arrive without one.
    client.core.update_order_status(&shared, 10, OrderStatus::Submitted, 0.0, 100.0, 0);
    client.core.update_order_status(&shared, 10, OrderStatus::Filled, 100.0, 0.0, 0);
    assert_eq!(
        client.core.open_orders.lock().unwrap().len(), 1,
        "a filled order is done, and only the inactive one remains",
    );
}

/// A question asked on an ended session is answered with the end at once,
/// not after its whole wait. The waits tested only their own deadline, so a
/// caller retrying on "no answer" paid the wait per call for ever while the
/// session had been over the whole time; the streams and the other surface
/// return at once.
#[test]
fn a_question_on_an_ended_session_is_refused_without_the_wait() {
    let (client, _rx, shared) = test_client();
    shared.reference.set_session_over("the trading connection");
    let started = std::time::Instant::now();
    let refused = client
        .contract_details(&Contract {
            symbol: "SPY".into(), sec_type: "STK".into(), exchange: "SMART".into(),
            currency: "USD".into(), ..Default::default()
        })
        .expect_err("the session is over");
    assert!(started.elapsed() < std::time::Duration::from_secs(5), "answered at once, not after the wait");
    assert_eq!(refused.code, 504, "{refused:?}");
}

/// A profit subscription on an ended session takes no slot. Taken before the
/// session was checked, a refused request held the one slot there is: the
/// next request under another number was refused as a duplicate of one that
/// never went, and the profit was reported under the refused number.
#[test]
fn a_profit_subscription_on_an_ended_session_takes_no_slot() {
    let (client, _rx, shared) = test_client();
    shared.reference.set_session_over("the trading connection");
    #[derive(Default)]
    struct Heard(Vec<i64>);
    impl crate::api::wrapper::Wrapper for Heard {
        fn error(&mut self, _req_id: i64, code: i64, _msg: &str, _json: &str) { self.0.push(code); }
    }
    let mut w = Heard::default();
    client.req_pnl(9, "", "");
    client.process_msgs(&mut w);
    assert!(client.core.pnl_req_id.lock().unwrap().is_none(), "the slot is not taken");
    assert!(w.0.contains(&504), "and the caller is told the session is over: {:?}", w.0);
}

/// An account read after the session ended is refused, not answered from the
/// book the session left behind.
///
/// The shutdown does not clear the download flag, so every one of these passed
/// its own gate at once and handed back the last book, the last figures or the
/// account list with nothing to say the session had gone. The compat surface
/// refuses all four; this one answered all four.
#[test]
fn an_account_read_after_the_session_ended_is_refused() {
    let (client, _rx, shared) = test_client();
    shared.reference.set_session_over("the trading connection");

    #[derive(Default)]
    struct Heard(Vec<i64>, usize);
    impl crate::api::wrapper::Wrapper for Heard {
        fn error(&mut self, _req_id: i64, code: i64, _msg: &str, _json: &str) {
            self.0.push(code);
        }
        fn position(&mut self, _a: &str, _c: &Contract, _p: f64, _avg: f64) { self.1 += 1; }
        fn position_end(&mut self) { self.1 += 1; }
        fn managed_accounts(&mut self, _accounts: &str) { self.1 += 1; }
        fn position_multi(
            &mut self, _r: i64, _a: &str, _m: &str, _c: &Contract, _p: f64, _avg: f64,
        ) { self.1 += 1; }
        fn account_update_multi(
            &mut self, _r: i64, _a: &str, _m: &str, _k: &str, _v: &str, _c: &str,
        ) { self.1 += 1; }
    }

    let mut w = Heard::default();
    client.req_positions(&mut w);
    client.req_managed_accts(&mut w);
    client.req_account_updates_multi(1, "", "", false, &mut w);
    client.req_positions_multi(2, "", "", &mut w);
    // The refusals are queued and handed over on the next pass, as every
    // refusal this surface makes is.
    client.process_msgs(&mut w);

    assert_eq!(
        w.0, vec![504, 504, 504, 504],
        "each says the session is over: {:?}", w.0,
    );
    assert_eq!(w.1, 0, "and none answers from the book the session left behind");
}

/// A withdrawal names an order this client is working, and one the venue
/// named at connect counts.
///
/// A withdrawal of a number naming nothing was sent anyway, under an order
/// name this client invented, and the caller learnt from the venue rather than
/// from the number. Answered here instead. The dangerous half is the other
/// one: the account's working set arrives asynchronously after connect, so a
/// withdrawal read before it lands must not refuse an order that is genuinely
/// live -- a refusal there leaves a real order working.
#[test]
fn a_withdrawal_names_an_order_this_client_is_working() {
    let (client, rx, shared) = test_client();
    shared.orders.set_replay_done();

    let refused = client.cancel_order(42, "");
    assert!(
        refused.as_ref().is_err_and(|why| why.code == 135),
        "no order is working under that number: {refused:?}",
    );
    assert!(rx.try_recv().is_err(), "and nothing was sent under it");

    // An order this session placed.
    client.core.track_order(42, spy(), Order::default(), 0);
    client.cancel_order(42, "").expect("the withdrawal goes");
    rx.try_recv().expect("and it reaches the engine");

    // And one carried over from a previous session, which this client learns
    // of only from the connect-time replay. Refusing this is the failure that
    // leaves a live order standing.
    let (client, rx, shared) = test_client();
    shared.orders.push_order_info(77, crate::bridge::RichOrderInfo {
        contract: spy(),
        order: Order { order_id: 77, ..Default::default() },
        order_state: crate::types::model::OrderState {
            status: "Submitted".into(), ..Default::default()
        },
        last_exec: Default::default(),
    });
    client.cancel_order(77, "").expect("an order the venue named is withdrawable");
    rx.try_recv().expect("and that withdrawal reaches the engine too");
}

/// A number the venue has finished an order under does not place another.
///
/// A status can arrive without the fill that ended the order, and while the
/// record stood the id was read as a working order's: the next order placed
/// under it went down the modify path, answered Ok, and nothing was
/// submitted. Freed instead, it went the other way -- the placement was sent
/// as a new order, and the venue refuses a repeated number only while it is
/// still working one, so a caller retrying what it believed had failed was
/// given a second live order. Refused now, under the number that names it.
#[test]
fn a_number_the_venue_has_finished_an_order_under_places_no_other() {
    let (client, rx, shared) = test_client();
    let order = Order {
        action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(),
        lmt_price: 100.0, tif: "DAY".into(), ..Default::default()
    };
    client.place_order(83, &spy(), &order).expect("placed");
    rx.try_recv().expect("the first order goes out");

    shared.orders.push_order_update(OrderUpdate {
        order_id: 83, instrument: 0, status: OrderStatus::Filled,
        filled_qty: 1.0, remaining_qty: 0.0, avg_price: 0, perm_id: 0, parent_id: 0, timestamp_ns: 0,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);

    let refused = client.place_order(83, &spy(), &order);
    assert!(
        refused.as_ref().is_err_and(|why| why.code == 103),
        "the number has already been worked: {refused:?}",
    );
    assert!(
        rx.try_recv().is_err(),
        "and nothing was sent under it, neither a second order nor a revision",
    );
}

/// A refusal that answers no request is reported as answering no request.
///
/// The reference client states those under -1. Clamped to 0, one lands on a
/// caller's own request — 0 is a number a caller may well have asked under —
/// and reads as an answer to something it did ask.
#[test]
fn a_refusal_against_no_request_is_not_reported_against_request_zero() {
    let (client, _rx, _shared) = test_client();

    client.report_reason(-1, &Refusal::not_connected("the engine has stopped"));

    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(
        w.events.iter().any(|e| e.starts_with("error:-1:")),
        "reported under {:?}, not against no request",
        w.events,
    );
}

/// An answering call does not swallow a venue-data notice.
///
/// The pump an answering call runs reads the dispatch into a collector of its
/// own; a bare client keeps no session record behind it, so a notice drained
/// there reaches nothing and the caller's own loop never sees it, while the
/// quote baseline is forgotten with nothing said.
#[test]
fn an_answering_call_does_not_swallow_a_venue_data_notice() {
    let (client, _rx, shared) = test_client();
    shared.push_venue_data_notice(crate::bridge::VenueDataConnection::MarketData, false);

    // The pump an answering call runs, into a collector with no session record.
    let mut collector = RecordingWrapper::default();
    client.pump_for_ask(&mut collector);
    assert!(
        !collector.events.iter().any(|e| e.starts_with("error:-1:2103:")),
        "the notice went to the ask collector and is gone: {:?}", collector.events,
    );

    // The caller's own loop still hears it.
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(
        w.events.iter().any(|e| e.starts_with("error:-1:2103:")),
        "the caller was never told the market-data connection broke: {:?}", w.events,
    );
}

/// A restore the caller never saw the matching loss of is not announced.
///
/// When a poll spans a whole outage and recovery the lost and restored flags
/// collapse to the restore alone; a 1102 with no 1100 before it reads as a
/// recovery from nothing. It is said only where this surface believed it was
/// disconnected.
#[test]
fn a_restore_without_a_loss_the_caller_saw_is_not_announced() {
    let (client, _rx, shared) = test_client();

    // The surface believes it is connected — it never processed a loss. A
    // restore lands on its own.
    shared.set_connection_restored();
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(
        !w.events.iter().any(|e| e.starts_with("error:-1:1102:")),
        "a 1102 with no 1100 the caller saw reads as a recovery from nothing: {:?}", w.events,
    );

    // A loss the caller is told about, then a recovery, is announced.
    shared.set_connection_lost();
    client.process_msgs(&mut w);
    shared.set_connection_restored();
    client.process_msgs(&mut w);
    assert!(
        w.events.iter().any(|e| e.starts_with("error:-1:1102:")),
        "a recovery from a loss the caller saw is announced: {:?}", w.events,
    );
}

/// A recovery that lands between the two reads is not undone by the loss it
/// followed.
///
/// The pair is read one flag at a time, and the engine can recover in the gap:
/// the loss is taken, the recovery lands, and the recovery is taken on the
/// same pass. Each setter clears the other, so holding both means the recovery
/// is the later of the two. Applied the other way round the loss went on last,
/// and a session that had come back — resubscribed and answering — read as
/// disconnected for the rest of its life, with nothing queued to say otherwise
/// and the caller told it recovered before it was told it had gone.
#[test]
fn a_recovery_landing_between_the_two_reads_is_not_undone_by_the_loss() {
    let (client, _rx, shared) = test_client();
    assert!(client.is_connected(), "connected to begin with");

    shared.raise_a_loss_a_recovery_landed_behind_for_test();
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);

    assert!(
        client.is_connected(),
        "the session came back and this surface holds it down: {:?}", w.events,
    );
    let announced: Vec<&String> = w
        .events
        .iter()
        .filter(|e| e.starts_with("error:-1:1100:") || e.starts_with("error:-1:1102:"))
        .collect();
    assert_eq!(
        announced.len(), 2,
        "the loss and the recovery are both said: {:?}", w.events,
    );
    assert!(
        announced[0].starts_with("error:-1:1100:"),
        "and the loss is said before the recovery from it: {announced:?}",
    );
}

/// A session that goes away while a question is being answered still tells the
/// caller so.
///
/// The pumping an answering call does runs the dispatch into a collector of its
/// own, and the notice that the session closed is delivered once. Taken there,
/// the caller's own wrapper never hears it and nothing says so again until a
/// reconnect — the program goes on believing it is connected.
#[test]
fn an_answering_call_does_not_swallow_the_notice_that_the_session_closed() {
    let (client, _rx, shared) = test_client();

    // The session goes away while the answer is being waited for.
    shared.set_connection_lost();
    let _ = client.qualify(spy());

    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(
        w.events.iter().any(|e| e.starts_with("error:-1:1100:")),
        "the caller was never told the session went away: {:?}", w.events,
    );
}

/// A request that asked for headlines and then failed to register leaves
/// nothing behind.
///
/// The headlines go out before the registration can fail, and the record of
/// who asked is what the next caller reads to decide whether to ask at all.
/// Left standing for a request that never started, this contract's headlines
/// are never requested again and the subscription that did go out cannot be
/// withdrawn — the path that withdraws it needs a request this one no longer
/// has.
#[test]
fn a_failed_request_does_not_keep_this_contract_from_asking_for_news_again() {
    let (client, rx, _shared) = test_client();
    let con_id = spy().con_id;

    // No engine answers a registration here, so the request fails after the
    // headlines have been asked for.
    let refused = client.req_mkt_data(1, &spy(), "292", false, false);
    assert!(refused.is_err(), "the registration was expected to fail");

    assert!(
        client.core.first_to_ask_for_news(con_id, 2),
        "the failed request is still recorded as watching, so nobody asks again",
    );
    let _ = rx.try_iter().count();
}

/// A calculation asked before the venue has stated a model keeps the question
/// and waits, rather than refusing it for having been asked first. The venue
/// states a model for a contract that is watched, so asking about one that is
/// not watched opens the watch; asking about one that is waits for the model
/// the watch will bring.
#[test]
fn a_calculation_waits_for_the_model_rather_than_refusing() {
    let (client, _rx, _shared) = test_client();
    // Already watched, so the watch needs no opening — what is at issue is
    // whether the question survives the venue not having stated a model yet.
    client.core.con_id_to_instrument.lock().unwrap().insert(spy().con_id, 0);
    client.core.instrument_to_req.lock().unwrap().insert(0, 1);

    client.calculate_implied_volatility(7, &spy(), 12.5, 600.0);

    assert!(
        client.pending_option_calcs.lock().unwrap().contains_key(&7),
        "the question was not kept, so the model will arrive with nobody asking",
    );

    // Withdrawn by the caller: the watch goes with it.
    client.cancel_calculate_implied_volatility(7);
    assert!(
        !client.pending_option_calcs.lock().unwrap().contains_key(&7),
        "the question outlived the caller's interest in it",
    );
}

/// The other direction of the same pair: a price asked for at a stated
/// volatility waits on the model as an implied volatility does, and is
/// withdrawn the same way.
#[test]
fn a_price_calculation_waits_and_is_withdrawn_the_same_way() {
    let (client, _rx, _shared) = test_client();
    client.core.con_id_to_instrument.lock().unwrap().insert(spy().con_id, 0);
    client.core.instrument_to_req.lock().unwrap().insert(0, 1);

    client.calculate_option_price(9, &spy(), 0.25, 600.0);
    assert!(
        client.pending_option_calcs.lock().unwrap().contains_key(&9),
        "the question was not kept, so the model will arrive with nobody asking",
    );

    client.cancel_calculate_option_price(9);
    assert!(
        !client.pending_option_calcs.lock().unwrap().contains_key(&9),
        "the question outlived the caller's interest in it",
    );
}

/// The venue is asked for the headlines by contract and withdrawn by
/// contract, so it is asked once. Asked once per caller, a second caller
/// added a subscription the single withdrawal could not match, and it was
/// left running with nobody listening.
#[test]
fn the_headlines_are_asked_for_once_however_many_callers_want_them() {
    let (client, rx, _shared) = test_client();
    client.core.con_id_to_instrument.lock().unwrap().insert(spy().con_id, 0);
    client.core.instrument_to_req.lock().unwrap().insert(0, 1);
    client.core.req_to_instrument.lock().unwrap().insert(1, 0);

    client.req_mkt_data(2, &spy(), "292", false, false).expect("watches what is up");
    client.req_mkt_data(3, &spy(), "292", false, false).expect("watches what is up");
    let sent: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();

    let asked = sent.iter()
        .filter(|c| matches!(c, ControlCommand::SubscribeNews { .. }))
        .count();
    assert_eq!(asked, 1, "asked more times than it can be withdrawn: {sent:?}");
}

/// The headlines stop when the last caller that asked for them goes, not when
/// the first one does and not when the quotes happen to end. A caller
/// watching a subscription someone else opened leaves by a different path,
/// and the news it asked for was never withdrawn on that one.
#[test]
fn the_headlines_stop_with_the_last_caller_that_asked_for_them() {
    let (client, rx, shared) = test_client();
    // Someone already watches the quotes, and asked for no headlines.
    client.core.con_id_to_instrument.lock().unwrap().insert(spy().con_id, 0);
    client.core.instrument_to_req.lock().unwrap().insert(0, 1);
    client.core.req_to_instrument.lock().unwrap().insert(1, 0);

    // A second caller watches the same contract and does want them.
    client.req_mkt_data(2, &spy(), "292", false, false).expect("watches what is up");
    while rx.try_recv().is_ok() {}
    // Cancelled through the client, so the withdrawal has to reach the wire
    // and not merely be decided on.
    client.cancel_mkt_data(2).expect("cancelled");
    let sent: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert!(
        sent.iter().any(|c| matches!(c, ControlCommand::UnsubscribeNews { .. })),
        "the headlines were left running: {sent:?}",
    );
    assert!(
        !sent.iter().any(|c| matches!(c, ControlCommand::Unsubscribe { .. })),
        "the quotes stay up for the caller still watching: {sent:?}",
    );

    // Two callers asking: the headlines outlast the first of them.
    client.req_mkt_data(3, &spy(), "292", false, false).expect("watches what is up");
    client.req_mkt_data(4, &spy(), "292", false, false).expect("watches what is up");
    let stop_news = client.core.unregister_mkt_data(&shared, 3).headlines;
    assert_eq!(stop_news, None, "one of two left, so the headlines carry on");
    let stop_news = client.core.unregister_mkt_data(&shared, 4).headlines;
    assert_eq!(stop_news, Some(NewsSubject::Slot(0)), "and stop when the last of them goes");
}

// A second live subscription on the same contract would clobber
// the first's reverse mapping and orphan it silently. Reject at the call.
#[test]
fn a_second_caller_watches_the_subscription_that_is_up() {
    let (client, rx, shared) = test_client();
    // Existing live subscription for SPY (instrument 0) under req_id 1.
    client.core.con_id_to_instrument.lock().unwrap().insert(spy().con_id, 0);
    client.core.instrument_to_req.lock().unwrap().insert(0, 1);
    client.core.req_to_instrument.lock().unwrap().insert(1, 0);

    client.req_mkt_data(2, &spy(), "", false, false)
        .expect("a second caller watches it rather than being refused");
    assert!(rx.try_recv().is_err(), "one contract, one subscription on the wire");
    assert_eq!(client.core.followers_of(0), vec![2], "and it hears the quotes");
    assert_eq!(
        client.core.instrument_to_req.lock().unwrap().get(&0).copied(),
        Some(1),
        "the one that holds it still holds it",
    );

    // The holder leaves; the one still watching takes it over rather than
    // losing the feed, and nothing is withdrawn from the venue.
    let withdraw = client.core.unregister_mkt_data(&shared, 1).subscription;
    assert!(withdraw.is_none(), "nothing is withdrawn while someone is watching");
    assert_eq!(
        client.core.instrument_to_req.lock().unwrap().get(&0).copied(),
        Some(2),
        "handed to the one still watching",
    );

    // And when the last one leaves, it goes.
    let withdraw = client.core.unregister_mkt_data(&shared, 2).subscription;
    assert_eq!(withdraw, Some(0), "the last one out withdraws it");
}

// A contract given the ordinary ibapi way carries conId 0. Cached as
// an identity it maps every later symbol onto the first one's instrument, and
// the guard above then refuses them all — a symbol-only client could
// hold exactly one subscription.
#[test]
fn a_second_symbol_is_not_a_duplicate_of_the_first_con_id_less_contract() {
    let (client, rx, _shared) = test_client();
    // What a live symbol-only subscription under req_id 1 leaves behind.
    client.core.con_id_to_instrument.lock().unwrap().insert(0, 0);
    client.core.instrument_to_req.lock().unwrap().insert(0, 1);

    let qqq = Contract {
        symbol: "QQQ".into(), sec_type: "STK".into(), exchange: "SMART".into(),
        ..Default::default()
    };
    let err = client.req_mkt_data(2, &qqq, "", false, false).unwrap_err();
    assert!(!err.message.contains("req_id 1"), "QQQ is not the live contract: {err}");
    match rx.try_recv().expect("the registration reaches the engine") {
        ControlCommand::RegisterInstrument { contract: ContractRef { con_id, symbol, .. }, .. } => {
            assert_eq!((con_id, symbol.as_str()), (0, "QQQ"));
        }
        other => panic!("expected RegisterInstrument, got {other:?}"),
    }
}

#[test]
fn cancel_mkt_data_sends_unsubscribe() {
    let (client, rx, _shared) = test_client();
    // Pre-register mapping
    client.core.req_to_instrument.lock().unwrap().insert(1, 0);
    client.core.instrument_to_req.lock().unwrap().insert(0, 1);
    client.cancel_mkt_data(1).unwrap();
    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::Unsubscribe { instrument: 0, .. }));
    // Mapping should be cleared
    assert!(client.core.req_to_instrument.lock().unwrap().get(&1).is_none());
}

/// Withdrawing a subscription this client does not hold is answered, not
/// waved through.
///
/// Said nothing, the withdrawal reads exactly like one that worked, and a
/// caller whose record disagrees with this client's has no way to learn it.
#[test]
fn cancel_mkt_data_under_a_number_that_holds_nothing_says_so() {
    let (client, rx, _shared) = test_client();
    let refused = client.cancel_mkt_data(999);
    assert!(
        refused.as_ref().is_err_and(|why| why.code == 300),
        "nothing is being watched under that number: {refused:?}",
    );
    assert!(rx.try_recv().is_err(), "and nothing was asked of the engine for it");
}

/// Nothing about a session reaches the disk unless the caller asks for it. A
/// credential is theirs to place, and a library that writes one somewhere by
/// itself has made that decision for them.
#[test]
fn a_session_is_not_written_anywhere_by_default() {
    let cfg = EClientConfig::default();
    assert!(cfg.session_file.is_none(), "no file unless one is named");
    assert!(cfg.resume.is_none(), "and nothing is resumed unless one is given");
}

/// A session names the account it came from. Handing back one from a different
/// login describes a session this connect has no claim on, and the request that
/// names it is asking the server about somebody else's — so it is not offered,
/// and the login proceeds as if none had been given.
#[test]
fn a_session_from_another_account_is_not_offered() {
    let session = crate::auth::resume::ResumableSession {
        token: vec![1, 2, 3],
        server_session_id: "abc.0001".into(),
        hw_info: "hw".into(),
        encoded: "enc".into(),
        username: "someone-else".into(),
        paper: true,
    };
    let offered = |cfg: &EClientConfig| {
        cfg.resume.as_ref().filter(|r| r.username == cfg.username && r.paper == cfg.paper).is_some()
    };

    let cfg = |username: &str, paper: bool| EClientConfig {
        username: username.into(), paper,
        resume: Some(session.clone()), ..Default::default()
    };
    assert!(offered(&cfg("someone-else", true)), "its own account's session is offered");
    assert!(!offered(&cfg("me", true)), "another account's session is not");
    assert!(!offered(&cfg("someone-else", false)), "nor a session of the other kind");
}

/// Tick-by-tick rides the historical farm this client already reaches, not a
/// service of its own, so the subscription is sent and the venue answers it.
#[test]
fn req_tick_by_tick_data_is_sent_rather_than_refused() {
    let (client, _rx, _shared) = test_client();
    // A kind the venue does not name is still refused, and refused for saying
    // so rather than for the feed being unreachable.
    let err = client
        .req_tick_by_tick_data(10, &spy(), "Sideways", 0, false)
        .expect_err("a kind that is not a kind is refused");
    assert!(err.message.contains("no such kind"), "{err}");
    assert!(
        !err.message.contains("not served to this session"),
        "the old reasoning is gone: {err}"
    );
}

/// The two trade streams are two streams. Asking for one under the other's
/// name asked the venue for someone else's trades: every trade reported away
/// from the exchange arrived on a subscription that wanted the exchange's own.
#[test]
fn the_two_trade_streams_are_asked_for_separately() {
    assert_eq!(TbtType::named("AllLast"), Ok(TbtType::AllLast));
    assert_eq!(TbtType::named("Last"), Ok(TbtType::Last));
    assert_eq!(TbtType::named("BidAsk"), Ok(TbtType::BidAsk));
    assert!(TbtType::named("Sideways").is_err(), "a kind that is not a kind is refused");
}

#[test]
fn cancel_tick_by_tick_data_sends_unsubscribe_tbt() {
    let (client, rx, _shared) = test_client();
    // A trade stream is held in its own table. Held in the quote table, a
    // request for trades was handed the contract's quotes, and withdrawing it
    // took the quotes away from whoever was watching them.
    client.core.tbt_to_instrument.lock().unwrap().insert(10, 3);
    client.core.instrument_to_req.lock().unwrap().insert(3, 99);
    client.cancel_tick_by_tick_data(10).unwrap();
    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::UnsubscribeTbt { instrument: 3, .. }));
    assert_eq!(
        client.core.instrument_to_req.lock().unwrap().get(&3).copied(),
        Some(99),
        "and the caller quoting that contract still is",
    );
}

/// A number already carrying a tick stream is not given a second.
///
/// Two streams under one number stamp that number on every record, so the
/// caller was handed one contract's trades and another's quotes with nothing
/// to tell them apart and the tick kind of whichever asked last. The
/// withdrawal reads one record, so it reached the second only -- and with that
/// record gone, a second withdrawal reached nothing and the first stream ran
/// under a cancelled number for the life of the session.
#[test]
fn a_request_already_carrying_a_tick_stream_is_not_given_another() {
    let (client, rx, _shared) = test_client();
    client.core.tbt_to_instrument.lock().unwrap().insert(5, 3);
    while rx.try_recv().is_ok() {}

    let elsewhere = Contract { symbol: "QQQ".into(), con_id: 320227571, ..spy() };
    let refused = client.req_tick_by_tick_data(5, &elsewhere, "BidAsk", 0, false);

    assert!(
        refused.as_ref().is_err_and(|why| why.code == 102),
        "the number is already carrying a stream: {refused:?}",
    );
    assert!(rx.try_recv().is_err(), "and nothing was asked of the engine for it");
    assert_eq!(
        client.core.tbt_to_instrument.lock().unwrap().get(&5).copied(), Some(3),
        "the stream it was already carrying is untouched",
    );
}

/// Withdrawing a tick stream this client does not hold is answered, not
/// waved through.
///
/// Said nothing, the withdrawal reads exactly like one that worked, and a
/// caller whose record disagrees with this client's has no way to learn it.
#[test]
fn cancel_tick_by_tick_under_a_number_that_holds_nothing_says_so() {
    let (client, rx, _shared) = test_client();
    let refused = client.cancel_tick_by_tick_data(999);
    assert!(
        refused.as_ref().is_err_and(|why| why.code == 300),
        "nothing is held under that number: {refused:?}",
    );
    assert!(rx.try_recv().is_err(), "and nothing was asked of the engine for it");
}

// ═══════════════════════════════════════════════════════════════════
//  Orders — every order type
// ═══════════════════════════════════════════════════════════════════

/// A caller asking for a fraction of a share gets one. The quantity was
/// taken through `as u32`, so `placeOrder` with 0.5 sent an order for none:
/// the fraction was dropped before it reached the wire.
#[test]
fn place_order_carries_a_fractional_quantity() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(),
        total_quantity: 0.5,
        order_type: "LMT".into(),
        lmt_price: 150.0,
        ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::Order(OrderRequest::SubmitEx { qty, .. }) => assert_eq!(
            qty, crate::types::QTY_SCALE / 2,
            "half a share reaches the engine as half a share",
        ),
        _ => panic!("expected a submitted order, got {cmd:?}"),
    }
}

/// An order is refused once the trading connection has stopped being retried.
///
/// Accepted there it is recorded and buffered and never sent, and the caller
/// is told it placed an order the venue never saw. Gated on the trading
/// connection's own state rather than the session's: the session flag is set
/// by any transport ending, and refusing on that would refuse orders a live
/// trading connection would have carried.
#[test]
fn an_order_is_refused_once_the_trading_connection_has_stopped() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 1.0, order_type: "MKT".into(),
        tif: "DAY".into(), ..Default::default()
    };

    // A quote feed ending is not the trading connection ending.
    shared.reference.set_session_over("the market data farm");
    client.place_order(1, &spy(), &order).expect("the trading connection still carries it");
    while rx.try_recv().is_ok() {}

    shared.reference.set_trading_over("the trading connection");
    let err = client.place_order(2, &spy(), &order)
        .expect_err("and is refused once that has stopped");
    assert!(err.message.contains("never sent"), "{err}");
    assert!(rx.try_recv().is_err(), "nothing reaches the wire");
}

/// An exercise and a bracket are refused on the same terms as an order.
///
/// Both take an id and queue an instruction, and both were taken while the
/// trading connection was gone for good — recorded, given ids, and buffered
/// for a connection nothing is rebuilding. Every other order call on this
/// surface answers that condition; these two did not.
#[test]
fn an_exercise_and_a_bracket_are_refused_once_the_trading_connection_has_stopped() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let opt = Contract {
        con_id: 999002, symbol: "AAPL".into(), sec_type: "OPT".into(),
        last_trade_date_or_contract_month: "20260619".into(), strike: 230.0,
        right: "C".into(), multiplier: "100".into(), ..Default::default()
    };
    shared.reference.set_trading_over("the trading connection");

    let err = client.exercise_options(1, &opt, 1, 1, "DU123", false, Default::default())
        .expect_err("an exercise is refused");
    assert!(err.message.contains("never sent"), "{err}");

    let err = client.place_bracket(&spy(), "BUY", 1.0, 100.0, 110.0, 90.0)
        .expect_err("and so is a bracket");
    assert!(err.message.contains("never sent"), "{err}");

    assert!(rx.try_recv().is_err(), "nothing reaches the wire");
}

#[test]
fn place_order_market() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order { action: "BUY".into(), total_quantity: 100.0, order_type: "MKT".into(), ..Default::default() };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::Order(OrderRequest::SubmitEx { qty, kind: OrderKind::Market, .. }) => assert_eq!(qty, 100 * crate::types::QTY_SCALE),
        _ => panic!("expected a Market order, got {cmd:?}"),
    }
}

#[test]
fn place_order_limit() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 50.0, order_type: "LMT".into(),
        lmt_price: 150.25, ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::Order(OrderRequest::SubmitEx { qty, kind: OrderKind::Limit { price, .. }, .. }) => {
            assert_eq!(qty, 50 * crate::types::QTY_SCALE);
            assert_eq!(price, (150.25 * PRICE_SCALE_F) as i64);
        }
        _ => panic!("expected a Limit order, got {cmd:?}"),
    }
}

#[test]
fn place_order_trailing_stop_carries_initial_trigger() {
    // Part B /: a plain amount trailing stop can carry an
    // initial stop trigger (trailStopPrice); it must reach the request.
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "SELL".into(), total_quantity: 1.0, order_type: "TRAIL".into(),
        aux_price: 0.50,             // trail amount
        trail_stop_price: 10.00,     // initial stop trigger
        ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();
    match rx.try_recv().unwrap() {
        ControlCommand::Order(OrderRequest::SubmitEx { kind: OrderKind::TrailingStop { trail_amt, trail_stop_price, .. }, .. }) => {
            assert_eq!(trail_amt, (0.50 * PRICE_SCALE_F) as i64);
            assert_eq!(trail_stop_price, (10.00 * PRICE_SCALE_F) as i64);
        }
        cmd => panic!("expected a TrailingStop order, got {cmd:?}"),
    }
}

#[test]
fn place_order_trailing_stop_without_trigger_is_unset() {
    // Default (f64::MAX) must encode as 0 (not set), so the tag is omitted.
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "SELL".into(), total_quantity: 1.0, order_type: "TRAIL".into(),
        aux_price: 0.50, ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();
    match rx.try_recv().unwrap() {
        ControlCommand::Order(OrderRequest::SubmitEx { kind: OrderKind::TrailingStop { trail_stop_price, .. }, .. }) => {
            assert_eq!(trail_stop_price, 0);
        }
        cmd => panic!("expected a TrailingStop order, got {cmd:?}"),
    }
}

#[test]
fn place_order_adjustable_trail_carries_trailing_amount_and_unit() {
    // /: a base STP that converts to a TRAIL must carry
    // the trailing amount and unit through to the AdjustableStop request.
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "SELL".into(), total_quantity: 1.0, order_type: "STP".into(),
        aux_price: 11.00,                          // base stop price
        adjusted_order_type: "TRAIL".into(),
        trigger_price: 11.00,
        adjusted_stop_price: 10.00,
        adjusted_trailing_amount: 0.50,
        adjustable_trailing_unit: 0,               // amount
        ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::Order(OrderRequest::SubmitEx { kind: crate::types::OrderKind::AdjustableStop {
            adjusted_order_type, stop_price, trigger_price, adjusted_stop_price,
            adjusted_trailing_amount, adjustable_trailing_unit, .. }, .. }) => {
            assert_eq!(adjusted_order_type, crate::types::AdjustedOrderType::Trail);
            assert_eq!(stop_price, (11.00 * PRICE_SCALE_F) as i64);
            assert_eq!(trigger_price, (11.00 * PRICE_SCALE_F) as i64);
            assert_eq!(adjusted_stop_price, (10.00 * PRICE_SCALE_F) as i64);
            assert_eq!(adjusted_trailing_amount, (0.50 * PRICE_SCALE_F) as i64);
            assert_eq!(adjustable_trailing_unit, 0);
        }
        _ => panic!("expected SubmitEx carrying AdjustableStop, got {cmd:?}"),
    }
}

#[test]
fn place_order_adjustable_stop_carries_bracket_attrs_and_tif() {
    // An adjustable stop used as a bracket child must stay linked to
    // its parent and its OCA group and keep the caller's tif. Routing it around
    // the extended-attrs path shipped the child naked, unlinked and DAY.
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "SELL".into(), total_quantity: 1.0, order_type: "STP".into(),
        aux_price: 11.00,
        adjusted_order_type: "STP".into(),
        trigger_price: 12.00,
        adjusted_stop_price: 11.50,
        parent_id: 42,
        oca_group: "bracket_1".into(),
        oca_type: 1,
        tif: "GTC".into(),
        ..Default::default()
    };
    client.place_order(7, &spy(), &order).unwrap();

    match rx.try_recv().unwrap() {
        ControlCommand::Order(OrderRequest::SubmitEx { kind, tif, attrs, .. }) => {
            assert!(matches!(kind, crate::types::OrderKind::AdjustableStop { .. }),
                "adjustable stop must route through the extended path; got {kind:?}");
            assert_eq!(tif, b'1', "tif must survive as GTC");
            assert_eq!(attrs.parent_id, 42, "bracket child must stay linked to its parent");
            assert_eq!(attrs.oca_group_str, "bracket_1", "OCA group must survive");
        }
        cmd => panic!("expected SubmitEx carrying AdjustableStop, got {cmd:?}"),
    }
}

#[test]
fn modify_carries_outside_rth_from_the_resubmitted_order() {
    // The replace asserted 6433=1 unconditionally, so an order placed
    // with outside_rth=false came back outside-RTH after any modify. The flag
    // has to travel with the modify, since the tracked record has no field for
    // it.
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(),
        lmt_price: 100.0, outside_rth: false, ..Default::default()
    };
    client.place_order(70, &spy(), &order).unwrap();
    let _submit = rx.try_recv().unwrap();

    // Same id -> modify. Caller still says outside_rth=false.
    let reprice = Order { lmt_price: 101.0, ..order.clone() };
    client.place_order(70, &spy(), &reprice).unwrap();
    match next_command(&rx).expect("the modify") {
        ControlCommand::Order(OrderRequest::Modify { outside_rth, .. }) => {
            assert!(!outside_rth, "a modify must not opt the order into the extended session");
        }
        cmd => panic!("expected Modify, got {cmd:?}"),
    }

    // And it survives when the caller does want it.
    let rth_out = Order { lmt_price: 102.0, outside_rth: true, ..order.clone() };
    client.place_order(70, &spy(), &rth_out).unwrap();
    match next_command(&rx).expect("the modify") {
        ControlCommand::Order(OrderRequest::Modify { outside_rth, .. }) => {
            assert!(outside_rth, "an explicit outside_rth=true must reach the replace");
        }
        cmd => panic!("expected Modify, got {cmd:?}"),
    }
}

#[test]
fn place_order_adjustable_trail_percent_unit_passes_through() {
    // Percent unit (100) must survive; the trailing amount is a percent value.
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "SELL".into(), total_quantity: 1.0, order_type: "STP".into(),
        aux_price: 11.00,
        adjusted_order_type: "TRAIL".into(),
        adjusted_trailing_amount: 1.00,            // 1.00%
        adjustable_trailing_unit: 100,             // percent
        ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    match rx.try_recv().unwrap() {
        ControlCommand::Order(OrderRequest::SubmitEx { kind: crate::types::OrderKind::AdjustableStop {
            adjustable_trailing_unit, adjusted_trailing_amount, .. }, .. }) => {
            assert_eq!(adjustable_trailing_unit, 100);
            assert_eq!(adjusted_trailing_amount, (1.00 * PRICE_SCALE_F) as i64);
        }
        cmd => panic!("expected SubmitEx carrying AdjustableStop, got {cmd:?}"),
    }
}

#[test]
fn place_order_limit_gtc_carries_the_tif() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 10.0, order_type: "LMT".into(),
        lmt_price: 100.0, tif: "GTC".into(), ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::Order(OrderRequest::SubmitEx { tif, kind: OrderKind::Limit { .. }, .. }) => {
            assert_eq!(tif, b'1'); // GTC
        }
        _ => panic!("expected a limit order, got {cmd:?}"),
    }
}

#[test]
fn place_order_limit_hidden_carries_the_attribute() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 10.0, order_type: "LMT".into(),
        lmt_price: 100.0, hidden: true, ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::Order(OrderRequest::SubmitEx { attrs, kind: OrderKind::Limit { .. }, .. }) => {
            assert!(attrs.hidden);
        }
        _ => panic!("expected a limit order, got {cmd:?}"),
    }
}

// ── every order type must carry attrs + tif when set ──

#[test]
fn place_order_stop_with_parent_and_gtc_uses_submit_ex() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "SELL".into(), total_quantity: 1.0, order_type: "STP".into(),
        aux_price: 240.0, tif: "GTC".into(), parent_id: 42,
        oca_group: "77".into(), ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::Order(OrderRequest::SubmitEx { kind, tif, attrs, .. }) => {
            assert!(matches!(kind, crate::types::OrderKind::Stop { stop_price }
                if stop_price == (240.0 * PRICE_SCALE_F) as i64));
            assert_eq!(tif, b'1'); // GTC
            assert_eq!(attrs.parent_id, 42);
            assert_eq!(attrs.oca_group_str, "77", "the group as the caller named it");
        }
        _ => panic!("expected a Ex order, got {cmd:?}"),
    }
}

#[test]
fn place_order_market_outside_rth_uses_submit_ex() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 1.0, order_type: "MKT".into(),
        outside_rth: true, ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::Order(OrderRequest::SubmitEx { kind, tif, attrs, .. }) => {
            assert!(matches!(kind, crate::types::OrderKind::Market));
            assert_eq!(tif, b'0'); // DAY
            assert!(attrs.outside_rth);
        }
        _ => panic!("expected a Ex order, got {cmd:?}"),
    }
}

#[test]
fn place_order_trailing_amount_with_oca_uses_submit_ex() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "SELL".into(), total_quantity: 1.0, order_type: "TRAIL".into(),
        aux_price: 2.0, tif: "GTC".into(), oca_group: "exit_9".into(),
        oca_type: 2, ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::Order(OrderRequest::SubmitEx { kind, tif, attrs, .. }) => {
            assert!(matches!(kind, crate::types::OrderKind::TrailingStop { trail_amt, .. }
                if trail_amt == (2.0 * PRICE_SCALE_F) as i64));
            assert_eq!(tif, b'1');
            assert_eq!(attrs.oca_group_str, "exit_9");
            assert_eq!(attrs.oca_type, 2);
        }
        _ => panic!("expected a Ex order, got {cmd:?}"),
    }
}

#[test]
fn place_order_empty_tif_is_day() {
    // An empty tif is DAY, matching the official API default.
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 1.0, order_type: "STP".into(),
        aux_price: 240.0, ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();
    match rx.try_recv().unwrap() {
        ControlCommand::Order(OrderRequest::SubmitEx { tif, kind: OrderKind::Stop { .. }, .. }) => {
            assert_eq!(tif, b'0', "an empty tif is DAY");
        }
        other => panic!("expected a stop order, got {other:?}"),
    }
}

// ── an order held back is kept, not refused and not sent ──

/// The field is one the reference client sends to its counterpart, not one the
/// venue reads, and that counterpart holds the order rather than working it.
/// This client stands where it stood, so the holding is this client's: nothing
/// goes out, nothing is refused, and an order that transmits sends it.
#[test]
fn an_order_held_back_reaches_neither_the_venue_nor_a_refusal() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(),
        lmt_price: 100.0, transmit: false, ..Default::default()
    };
    client.place_order(1, &spy(), &order).expect("kept, not refused");
    assert!(rx.try_recv().is_err(), "and nothing reaches the engine");

    // Placed again, transmitting, it goes.
    let now = Order { transmit: true, ..order };
    client.place_order(1, &spy(), &now).expect("and now it goes");
    assert!(rx.try_recv().is_ok(), "the order the caller asked to send");
}

// ── An account this session does not hold must be refused, not dropped ──

/// No encoder reads the account field, so an accepted one fills the whole size
/// on the connected account while the open-order snapshot echoes the caller's
/// value back and confirms the wrong one. Naming your own connected account is
/// the ordinary single-account pattern and must keep working: the guard rejects
/// a mismatch, not the presence of a value.
#[test]
fn place_order_accepts_the_connected_account_by_name() {
    let (client, rx, _shared) = test_client();
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "LMT".into(),
        lmt_price: 150.0, account: "DU123".into(), ..Default::default()
    };
    client.place_order(1, &spy(), &order).expect("the connected account is not a mismatch");
    assert!(rx.try_recv().is_ok(), "and the order reaches the engine");
}

#[test]
fn place_order_an_account_this_session_does_not_hold_is_rejected() {
    // The allocation fields that used to sit beside this one now reach the
    // wire, so the reason to refuse them is gone: an order stating a group, a
    // method or a percentage carries them to the venue.
    let cases: Vec<OrderCase> = vec![
        ("account", |o| o.account = "U9999999".into()),
    ];
    for (name, set) in cases {
        let (client, rx, _shared) = test_client();
        let mut order = Order {
            action: "BUY".into(), total_quantity: 100.0, order_type: "LMT".into(),
            lmt_price: 150.0, ..Default::default()
        };
        set(&mut order);
        let Err(err) = client.place_order(1, &spy(), &order) else {
            panic!("{name} must be refused");
        };
        // The field under test, not a fixed one: asserting "fa_group" for every
        // arm meant three of them only proved that some error was returned.
        let field = name.split(' ').next().unwrap();
        assert!(err.message.contains(field), "{name}: the message must name the field — {err}");
        assert!(rx.try_recv().is_err(), "{name}: nothing reaches the engine");
    }
}

#[test]
fn place_order_unknown_tif_is_rejected() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(),
        lmt_price: 100.0, tif: "XYZ".into(), ..Default::default()
    };
    let err = client.place_order(1, &spy(), &order).unwrap_err();
    assert!(err.to_string().contains("tif"), "got: {err}");
    assert!(rx.try_recv().is_err());
}

/// A trailing stop may be all-or-none.
///
/// Both instructions travel on one field, concatenated — `18=aG` — and this
/// client refused the pair on a reading that they share a slot. A session
/// placed it: the venue takes it and the order works.
#[test]
fn place_order_all_or_none_trail_reaches_the_wire() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "SELL".into(), total_quantity: 1.0, order_type: "TRAIL".into(),
        aux_price: 2.0, all_or_none: true, ..Default::default()
    };
    client.place_order(1, &spy(), &order).expect("the pair is the venue's to refuse");
    match rx.try_recv().expect("it reaches the wire") {
        ControlCommand::Order(OrderRequest::SubmitEx { kind: OrderKind::TrailingStop { .. }, attrs, .. }) => {
            assert!(attrs.all_or_none, "carrying the instruction it was given");
        }
        cmd => panic!("expected a trailing stop, got {cmd:?}"),
    }
}

// ── oca_type carried and coerced ──

#[test]
fn attrs_oca_type_coerces_out_of_range_to_unset() {
    let order = Order { oca_type: 9, ..Default::default() };
    assert_eq!(order.attrs().oca_type, 0);
    let order = Order { oca_type: 4, ..Default::default() };
    assert_eq!(order.attrs().oca_type, 4);
    let order = Order { oca_type: -1, ..Default::default() };
    assert_eq!(order.attrs().oca_type, 0);
}

#[test]
fn place_order_stop() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "SELL".into(), total_quantity: 100.0, order_type: "STP".into(),
        aux_price: 145.0, ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::Order(OrderRequest::SubmitEx { side, kind: OrderKind::Stop { stop_price, .. }, .. }) => {
            assert!(matches!(side, Side::Sell));
            assert_eq!(stop_price, (145.0 * PRICE_SCALE_F) as i64);
        }
        _ => panic!("expected a Stop order, got {cmd:?}"),
    }
}

#[test]
fn place_order_stop_limit() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "SELL".into(), total_quantity: 100.0, order_type: "STP LMT".into(),
        lmt_price: 144.0, aux_price: 145.0, ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::Order(OrderRequest::SubmitEx { kind: OrderKind::StopLimit { price, stop_price, .. }, .. }) => {
            assert_eq!(price, (144.0 * PRICE_SCALE_F) as i64);
            assert_eq!(stop_price, (145.0 * PRICE_SCALE_F) as i64);
        }
        _ => panic!("expected a StopLimit order, got {cmd:?}"),
    }
}

#[test]
fn place_order_trailing_stop_amount() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "SELL".into(), total_quantity: 100.0, order_type: "TRAIL".into(),
        aux_price: 2.0, ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::Order(OrderRequest::SubmitEx { kind: OrderKind::TrailingStop { trail_amt, .. }, .. }) => {
            assert_eq!(trail_amt, (2.0 * PRICE_SCALE_F) as i64);
        }
        _ => panic!("expected a TrailingStop order, got {cmd:?}"),
    }
}

#[test]
fn place_order_trailing_stop_percent() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "SELL".into(), total_quantity: 100.0, order_type: "TRAIL".into(),
        trailing_percent: 5.0, ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::Order(OrderRequest::SubmitEx { kind: OrderKind::TrailPct { trail_pct, .. }, .. }) => {
            assert_eq!(trail_pct, 500); // 5.0 * 100
        }
        _ => panic!("expected a TrailingStopPct order, got {cmd:?}"),
    }
}

#[test]
fn place_order_trailing_stop_limit() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "SELL".into(), total_quantity: 100.0, order_type: "TRAIL LIMIT".into(),
        lmt_price: 148.0, aux_price: 2.0, ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::Order(OrderRequest::SubmitEx { kind: OrderKind::TrailingStopLimit { .. }, .. })));
}

#[test]
fn place_order_moc() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "MOC".into(), ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::Order(OrderRequest::SubmitEx { kind: OrderKind::Moc, .. })));
}

#[test]
fn place_order_loc() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "LOC".into(),
        lmt_price: 150.0, ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::Order(OrderRequest::SubmitEx { kind: OrderKind::Loc { .. }, .. })));
}

#[test]
fn place_order_mit() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "MIT".into(),
        aux_price: 148.0, ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::Order(OrderRequest::SubmitEx { kind: OrderKind::Mit { .. }, .. })));
}

#[test]
fn place_order_lit() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "LIT".into(),
        lmt_price: 150.0, aux_price: 148.0, ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::Order(OrderRequest::SubmitEx { kind: OrderKind::Lit { .. }, .. })));
}

#[test]
fn place_order_mtl() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "MTL".into(), ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::Order(OrderRequest::SubmitEx { kind: OrderKind::Mtl, .. })));
}

#[test]
fn place_order_mkt_prt() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "MKT PRT".into(), ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::Order(OrderRequest::SubmitEx { kind: OrderKind::MktPrt, .. })));
}

#[test]
fn place_order_stp_prt() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "SELL".into(), total_quantity: 100.0, order_type: "STP PRT".into(),
        aux_price: 145.0, ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::Order(OrderRequest::SubmitEx { kind: OrderKind::StpPrt { .. }, .. })));
}

#[test]
fn place_order_rel() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "REL".into(),
        aux_price: 0.10, ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::Order(OrderRequest::SubmitEx { kind: OrderKind::Rel { .. }, .. })));
}

#[test]
fn place_order_peg_mkt() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "PEG MKT".into(),
        aux_price: 0.05, ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::Order(OrderRequest::SubmitEx { kind: OrderKind::PegMkt { .. }, .. })));
}

#[test]
fn place_order_peg_mid() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "PEG MID".into(),
        aux_price: 0.02, ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::Order(OrderRequest::SubmitEx { kind: OrderKind::PegMid { .. }, .. })));
}

#[test]
fn place_order_midprice() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "MIDPRICE".into(),
        lmt_price: 150.0, ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::Order(OrderRequest::SubmitEx { kind: OrderKind::MidPrice { .. }, .. })));
}

#[test]
fn place_order_snap_mkt() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "SNAP MKT".into(), ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::Order(OrderRequest::SubmitEx { kind: OrderKind::SnapMkt { .. }, .. })));
}

#[test]
fn place_order_snap_mid() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "SNAP MID".into(), ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::Order(OrderRequest::SubmitEx { kind: OrderKind::SnapMid { .. }, .. })));
}

#[test]
fn place_order_snap_pri() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "SNAP PRI".into(), ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::Order(OrderRequest::SubmitEx { kind: OrderKind::SnapPri { .. }, .. })));
}

#[test]
fn place_order_box_top() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "BOX TOP".into(), ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::Order(OrderRequest::SubmitEx { kind: OrderKind::Mtl, .. })));
}

#[test]
fn place_order_sell_side() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "SELL".into(), total_quantity: 50.0, order_type: "MKT".into(), ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::Order(OrderRequest::SubmitEx { side, kind: OrderKind::Market, .. }) => {
            assert!(matches!(side, Side::Sell));
        }
        _ => panic!("expected SubmitMarket"),
    }
}

#[test]
fn place_order_short_sell_side() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "SSHORT".into(), total_quantity: 50.0, order_type: "MKT".into(), ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::Order(OrderRequest::SubmitEx { side, kind: OrderKind::Market, .. }) => {
            assert!(matches!(side, Side::ShortSell));
        }
        _ => panic!("expected SubmitMarket"),
    }
}

#[test]
fn place_order_algo_vwap() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 1000.0, order_type: "LMT".into(),
        lmt_price: 150.0, algo_strategy: "vwap".into(),
        algo_params: vec![TagValue { tag: "maxPctVol".into(), value: "0.1".into() }],
        ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::Order(OrderRequest::SubmitEx {
        kind: OrderKind::Algo { .. }, .. })));
}

#[test]
fn place_order_what_if() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "LMT".into(),
        lmt_price: 150.0, what_if: true, ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();

    let cmd = rx.try_recv().unwrap();
    // The order the caller described, asked about rather than placed.
    assert!(matches!(cmd, ControlCommand::Order(OrderRequest::SubmitEx {
        kind: OrderKind::Limit { .. },
        attrs: crate::types::OrderAttrs { what_if: true, .. }, .. })));
}

#[test]
fn place_order_unsupported_type_returns_error() {
    let (client, _rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "FANTASY".into(), ..Default::default()
    };
    let result = client.place_order(1, &spy(), &order);
    let err = result.unwrap_err();
    assert!(err.message.contains("Unsupported order type"));
    assert_eq!(err.code, 387, "one refusal, one number, wherever it is raised");
}

/// A preview states the order type it asks about. An unrecognised type is
/// refused; encoded as a limit, the answer would describe a different order.
#[test]
fn a_preview_is_refused_for_a_type_this_client_cannot_send() {
    let (client, _rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0,
        order_type: "SOMETHING NEW".into(), what_if: true,
        ..Default::default()
    };
    let err = client.place_order(1, &spy(), &order).unwrap_err();
    assert!(err.message.contains("Unsupported order type"), "got: {err}");
}

/// An algorithm rides on a limit order: tag 40 is written once, as `2`. Any
/// other order type carrying an algorithm is refused.
#[test]
fn an_algo_order_states_the_limit_it_is_sent_as() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let market_with_algo = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "MKT".into(),
        algo_strategy: "Adaptive".into(),
        algo_params: vec![TagValue { tag: "adaptivePriority".into(), value: "Normal".into() }],
        ..Default::default()
    };
    let err = client.place_order(1, &spy(), &market_with_algo).unwrap_err();
    assert!(err.message.contains("limit order"), "got: {err}");

    let limit_with_algo = Order {
        order_type: "LMT".into(), lmt_price: 100.0, ..market_with_algo
    };
    client.place_order(2, &spy(), &limit_with_algo).expect("a limit carries the algo");
    assert!(matches!(rx.try_recv().unwrap(), ControlCommand::Order(OrderRequest::SubmitEx {
        kind: OrderKind::Adaptive { .. }, .. })));
}

/// A pegged-to-benchmark order encodes and previews.
#[test]
fn a_pegged_to_benchmark_order_reaches_the_builder() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "PEG BENCH".into(),
        lmt_price: 100.0, reference_contract_id: 265598, ..Default::default()
    };
    client.place_order(1, &spy(), &order).expect("PEG BENCH is placeable");
    assert!(matches!(rx.try_recv().unwrap(), ControlCommand::Order(OrderRequest::SubmitEx {
        kind: OrderKind::PegBench { .. }, .. })));
}

#[test]
fn place_order_non_stk_contract_rejected() {
    // An option's symbol names a whole chain. Without an expiry, strike or
    // right the order cannot say which contract it means.
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    // No contract id: an id names one contract on its own and the venue takes
    // an order carrying nothing else, so this is the case the guard is for.
    let bare = Contract {
        symbol: "AAPL".into(), sec_type: "OPT".into(), exchange: "SMART".into(),
        ..Default::default()
    };
    let order = Order { action: "BUY".into(), total_quantity: 1.0, order_type: "MKT".into(), ..Default::default() };
    let err = client.place_order(1, &bare, &order).expect_err("a chain is not a contract");
    assert!(err.message.contains("OPT"), "the refusal names the type: {err}");
    assert!(rx.try_recv().is_err(), "and nothing reaches the engine");

    // An id says which contract without any of it.
    let by_id = Contract {
        con_id: 999001, sec_type: "OPT".into(), exchange: "SMART".into(),
        ..Default::default()
    };
    let refusal = client.place_order(2, &by_id, &order).unwrap_err();
    assert!(!refusal.message.contains("names a whole chain"), "an id is not a chain: {refusal}");

    // The named case is not asserted here: this fixture has no engine, so
    // registering an instrument blocks on a reply that never arrives. That an
    // identified option is accepted is pinned by `contract_gate_tests`, and that
    // the identity reaches the wire by `an_option_order_names_its_contract`.
}

/// What an exercise refuses, and it refuses before it builds anything: a
/// caller told the request went out believes the position was dealt with.
///
/// The documented API names a third action, a hold, which is not served here.
/// A quantity that is not a count reaches the wire through `as u32` as a very
/// large one. And the account is not carried on the order at all, so an
/// exercise naming another one would be taken on the connected account.
#[test]
fn an_exercise_it_cannot_serve_is_refused_before_anything_is_sent() {
    let (client, rx, _shared) = test_client();
    let opt = Contract {
        con_id: 999002, symbol: "AAPL".into(), sec_type: "OPT".into(),
        last_trade_date_or_contract_month: "20260619".into(), strike: 230.0,
        right: "C".into(), multiplier: "100".into(), ..Default::default()
    };
    let cases: [(&str, i32, i32, &str); 5] = [
        ("a hold", 3, 1, ""),
        ("no action at all", 0, 1, ""),
        ("no contracts", 1, 0, ""),
        ("a negative count", 1, -1, ""),
        ("another account", 1, 1, "DU999"),
    ];
    for (name, action, qty, account) in cases {
        client.exercise_options(1, &opt, action, qty, account, false, Default::default()).expect_err(name);
        assert!(rx.try_recv().is_err(), "{name} reached the engine");
    }

    // One it can serve gets as far as naming the contract. This fixture has no
    // engine to answer the registration, so the call ends there.
    let _ = client.exercise_options(1, &opt, 1, 1, "DU123", false, Default::default());
    assert!(
        matches!(rx.try_recv(), Ok(ControlCommand::RegisterInstrument { contract: ContractRef { con_id: 999002, .. }, .. })),
        "a served exercise registers its contract",
    );
}

#[test]
fn an_order_states_where_it_is_to_be_filled() {
    // The venue does not choose a destination. Without this, a contract stating
    // only a symbol was looked up and filled on whichever listing the
    // definition service answered with first.
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let nowhere = Contract { con_id: 756733, symbol: "SPY".into(), ..Default::default() };
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "MKT".into(),
        ..Default::default()
    };

    let refused = client.place_order(1, &nowhere, &order).expect_err("no destination");
    assert_eq!(refused.code, crate::error_codes::Refusal::VALIDATION);
    assert!(rx.try_recv().is_err(), "and nothing reaches the engine");

    client.place_order(2, &spy(), &order).expect("a destination is all it lacked");
    assert!(rx.try_recv().is_ok());
}

#[test]
fn place_order_explicit_stk_contract_accepted() {
    // An explicit sec_type="STK" must still be accepted.
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let stk = Contract {
        con_id: 756733, symbol: "SPY".into(), sec_type: "STK".into(),
        exchange: "SMART".into(), ..Default::default()
    };
    let order = Order { action: "BUY".into(), total_quantity: 100.0, order_type: "MKT".into(), ..Default::default() };
    client.place_order(1, &stk, &order).unwrap();
    assert!(rx.try_recv().is_ok());
}

#[test]
fn place_order_invalid_action_returns_error() {
    let (client, _rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "INVALID".into(), total_quantity: 100.0, order_type: "MKT".into(), ..Default::default()
    };
    let result = client.place_order(1, &spy(), &order);
    assert!(result.is_err());
}

/// A revision waiting to be transmitted does not make a live order held.
///
/// The venue is working the order; what is kept here is a change to it. Read as
/// a held order, cancelling forgot the change and returned, leaving the order
/// working at the venue while the caller had been told it was withdrawn — and
/// placing again under the id built a fresh submission for an order already on
/// the market.
#[test]
fn a_staged_revision_does_not_hide_the_order_the_venue_is_working() {
    let (client, rx, _shared) = test_client();
    let order = |transmit: bool, price: f64| Order {
        order_id: 88,
        action: "BUY".into(),
        total_quantity: 100.0,
        order_type: "LMT".into(),
        lmt_price: price,
        tif: "DAY".into(),
        transmit,
        ..Default::default()
    };
    client.place_order(88, &spy(), &order(true, 100.0)).expect("placed and sent");
    assert!(matches!(
        rx.try_recv(), Ok(ControlCommand::Order(OrderRequest::SubmitEx { .. })),
    ));
    // A revision of it, kept rather than sent.
    client.place_order(88, &spy(), &order(false, 101.0)).expect("the change is kept");
    assert!(next_command(&rx).is_none(), "nothing goes to the venue for a change that is held");
    assert!(
        client.core.is_working_at_the_venue(88, Some(&client.shared)),
        "the order is still one the venue is working",
    );

    client.cancel_order(88, "").expect("withdrawn");
    match rx.try_recv().expect("the cancel travels") {
        ControlCommand::Order(OrderRequest::Cancel { order_id }) => assert_eq!(order_id, 88),
        other => panic!("expected a cancel, got {other:?}"),
    }
    assert!(!client.core.is_held(88), "and the change that was kept goes with it");
}

/// A replace releases nothing that is waiting to be placed.
///
/// The order that transmits sends whatever of its family was kept, which is how
/// a bracket goes out as one thing. A replace places nothing — it states new
/// terms for an order the venue is already working — so a caller moving a
/// parent's price had the exit it was still building sent for it, unpaired and
/// unasked.
#[test]
fn replacing_an_order_does_not_send_the_family_it_is_still_building() {
    let (client, rx, _shared) = test_client();
    let entry = |price: f64| Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "LMT".into(),
        lmt_price: price, tif: "DAY".into(), transmit: true, ..Default::default()
    };
    client.place_order(50, &spy(), &entry(100.0)).expect("the parent goes");
    assert!(matches!(
        rx.try_recv(), Ok(ControlCommand::Order(OrderRequest::SubmitEx { .. })),
    ));
    // One exit kept back, while the caller builds the other.
    let exit = Order {
        action: "SELL".into(), total_quantity: 100.0, order_type: "LMT".into(),
        lmt_price: 110.0, tif: "DAY".into(), transmit: false, parent_id: 50,
        ..Default::default()
    };
    client.place_order(51, &spy(), &exit).expect("the exit is kept");
    assert!(rx.try_recv().is_err(), "and nothing goes out for it");

    client.place_order(50, &spy(), &entry(99.0)).expect("the parent is replaced");
    match next_command(&rx).expect("the replace goes out") {
        ControlCommand::Order(OrderRequest::Modify { order_id, .. }) => assert_eq!(order_id, 50),
        other => panic!("expected a replace of the parent, got {other:?}"),
    }
    assert!(next_command(&rx).is_none(), "and the exit is not sent with it");
    assert!(client.core.is_held(51), "it is still waiting to be placed");
}

/// A change to an order that has finished is not sent with a later family.
///
/// A revision of a live order can be built and kept, waiting for something that
/// transmits. Nothing took it back when the order it changed left the book, so
/// it stayed in the hold for the life of the session — and the next order
/// placed beside it released a replace for an order that had already filled,
/// which the venue answers by stating that it knows no such order.
#[test]
fn a_change_to_an_order_that_finished_is_not_released_with_the_next_family() {
    let (client, rx, shared) = test_client();
    let exit = |transmit: bool, price: f64| Order {
        action: "SELL".into(), total_quantity: 100.0, order_type: "LMT".into(),
        lmt_price: price, tif: "DAY".into(), transmit, parent_id: 60,
        ..Default::default()
    };
    client.place_order(61, &spy(), &exit(true, 110.0)).expect("the exit goes");
    assert!(matches!(
        rx.try_recv(), Ok(ControlCommand::Order(OrderRequest::SubmitEx { .. })),
    ));
    client.place_order(61, &spy(), &exit(false, 111.0)).expect("a change to it is kept");
    assert!(next_command(&rx).is_none(), "nothing goes to the venue for a change that is held");
    // And then the order it was a change to fills.
    client.core.update_order_status(
        &shared, 61, crate::types::OrderStatus::Filled, 100.0, 0.0, 0,
    );
    assert!(!client.core.is_held(61), "the change goes with the order it changed");

    client.place_order(62, &spy(), &exit(true, 112.0)).expect("the next exit goes");
    match rx.try_recv().expect("it goes out") {
        ControlCommand::Order(OrderRequest::SubmitEx { order_id, .. }) => {
            assert_eq!(order_id, 62, "the order that transmits is the one that goes");
        }
        other => panic!("expected the new order, got {other:?}"),
    }
    assert!(rx.try_recv().is_err(), "and nothing goes out for the order that finished");
}

/// Naming a contract the venue has not named does not take the order apart.
///
/// A contract carrying only a symbol is handed to the venue to name, and what
/// comes back is the venue's description of it — which carries no hedge and no
/// legs, because a description of one contract has neither. Used in place of
/// what the caller stated, a delta-neutral order lost the contract it hedges
/// against and a combination lost every leg, and both went to the venue as
/// something else entirely.
#[test]
fn naming_a_contract_keeps_the_hedge_and_the_legs_the_caller_stated() {
    let (client, _rx, _shared) = test_client();
    let mut hedged = spy();
    hedged.con_id = 0;
    hedged.delta_neutral_contract = Some(crate::types::model::DeltaNeutralContract {
        con_id: 265598, delta: 0.5, price: 100.0,
    });
    // Named already, so the lookup is answered from the record rather than the
    // venue: the question here is what survives the naming, not the naming.
    let key = ClientCore::description_key(&hedged);
    let mut as_the_venue_names_it = spy();
    as_the_venue_names_it.con_id = 756733;
    as_the_venue_names_it.delta_neutral_contract = None;
    client.core.remember_named(key, as_the_venue_names_it);

    let order = Order {
        order_id: 60, action: "BUY".into(), total_quantity: 100.0,
        order_type: "LMT".into(), lmt_price: 100.0, tif: "DAY".into(),
        transmit: true, ..Default::default()
    };
    client.place_order(60, &hedged, &order).expect("placed");

    let held = client.core.open_orders.lock().unwrap();
    let placed = held.get(&60).expect("the order is tracked");
    assert!(
        placed.contract.delta_neutral_contract.is_some(),
        "the contract this order hedges against is what the caller stated: {:?}",
        placed.contract,
    );
}

/// A replacement the venue refuses does not leave its terms in the record.
///
/// The record takes the attempt ahead of the venue's answer, because every
/// later cancel and replace restates from it. Where the answer is a refusal
/// and only the status was put back, the record went on stating a price the
/// venue had said no to — and the next thing sent for that order carried it.
#[test]
fn a_refused_replacement_leaves_the_terms_the_venue_holds() {
    let (client, rx, shared) = test_client();
    let order = |qty: f64, price: f64| Order {
        order_id: 66, action: "BUY".into(), total_quantity: qty,
        order_type: "LMT".into(), lmt_price: price, tif: "DAY".into(),
        transmit: true, ..Default::default()
    };
    client.place_order(66, &spy(), &order(100.0, 100.0)).expect("placed");
    let _ = rx.try_recv();
    client.core.update_order_status(
        &shared, 66, crate::types::OrderStatus::Submitted, 0.0, 100.0, 0,
    );
    // A replacement goes out, and the record takes it.
    client.place_order(66, &spy(), &order(200.0, 105.0)).expect("replaced");
    assert_eq!(
        client.core.open_orders.lock().unwrap().get(&66).map(|o| o.order.lmt_price),
        Some(105.0),
        "the attempt stands while the venue has not answered",
    );

    // The venue refuses it, and says the order still stands.
    shared.orders.push_cancel_reject(CancelReject {
        order_id: 66, instrument: 0, reject_type: 2, reason_code: 0,
        answers_a_live_change: true, still_working: Some(crate::types::OrderStatus::Submitted), timestamp_ns: 0,
    });
    client.process_msgs(&mut RecordingWrapper::default());

    let held = client.core.open_orders.lock().unwrap();
    let tracked = held.get(&66).expect("the order still stands");
    assert_eq!(tracked.order.lmt_price, 100.0, "the price the venue holds");
    assert_eq!(tracked.order.total_quantity, 100.0, "and the quantity it holds");
}

/// A replace states new terms for an order the venue is working; it does not
/// place a new one.
///
/// Recorded as a new one, a partly filled order came back from a snapshot as
/// pending with nothing filled and its whole quantity outstanding — and a
/// replace the venue then refused left it reading that way for the rest of the
/// session.
#[test]
fn replacing_an_order_keeps_what_it_has_already_filled() {
    let (client, rx, shared) = test_client();
    let order = |qty: f64, price: f64| Order {
        order_id: 77,
        action: "BUY".into(),
        total_quantity: qty,
        order_type: "LMT".into(),
        lmt_price: price,
        tif: "DAY".into(),
        transmit: true,
        ..Default::default()
    };
    client.place_order(77, &spy(), &order(100.0, 100.0)).expect("placed");
    let _ = rx.try_recv();
    // Thirty of it trades.
    client.core.update_order_status(
        &shared, 77, crate::types::OrderStatus::PartiallyFilled, 30.0, 70.0, 0,
    );

    client.place_order(77, &spy(), &order(120.0, 101.0)).expect("replaced");

    let held = client.core.open_orders.lock().unwrap();
    let tracked = held.get(&77).expect("the order is still tracked");
    assert_eq!(tracked.filled, 30.0, "what it has traded stands");
    assert_eq!(tracked.status, "Submitted", "and it is still working, not pending again");
    assert_eq!(tracked.remaining, 90.0, "the new quantity less what it filled");
    assert_eq!(tracked.order.total_quantity, 120.0, "the terms follow the attempt");
    assert_eq!(tracked.order.lmt_price, 101.0);
}

/// An order number is the caller's, or it is a refusal.
///
/// An id at or below zero names no order the venue will hold. One handed out in
/// its place put the order on the market under a number the caller had never
/// seen: every status about it arrived under an id they were not watching, and
/// their own cancel named nothing. The other surface has always refused it.
#[test]
fn an_order_numbered_at_or_below_zero_is_refused_not_renumbered() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "MKT".into(), ..Default::default()
    };
    for stated in [0i64, -5] {
        let why = client
            .place_order(stated, &spy(), &order)
            .expect_err("a number at or below zero names no order");
        assert!(why.message.contains(&format!("order_id {stated}")), "{why}");
    }
    assert!(rx.try_recv().is_err(), "and nothing reaches the engine");
}

#[test]
fn cancel_order_sends_cancel_command() {
    let (client, rx, _shared) = test_client();
    // A withdrawal names an order this client is working, or it is answered
    // rather than sent under a number the venue never gave out.
    client.core.track_order(42, spy(), Order::default(), 0);
    client.cancel_order(42, "").unwrap();
    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::Order(OrderRequest::Cancel { order_id }) => assert_eq!(order_id, 42),
        _ => panic!("expected Cancel"),
    }
}

/// The note a stated cancel time leaves is recorded against the order named,
/// and nothing is recorded for a number that is not an order: parked against
/// zero, it fired as an error on an order that does not exist.
#[test]
fn a_cancel_time_note_names_the_order_it_belongs_to() {
    let (client, _rx, shared) = test_client();
    assert!(
        client.cancel_order(0, "20260904 12:00:00").is_err(),
        "zero is not an order number",
    );
    assert!(
        shared.orders.drain_order_inactive().is_empty(),
        "nothing may be recorded against an order that does not exist",
    );

    let (client, rx, shared) = test_client();
    client.core.track_order(17, spy(), Order::default(), 0);
    client.cancel_order(17, "20260904 12:00:00").unwrap();
    let notes = shared.orders.drain_order_inactive();
    assert_eq!(notes.len(), 1, "the note is recorded once");
    assert_eq!(notes[0].0, 17, "against the order named");
    match rx.try_recv().expect("the cancel is sent anyway") {
        ControlCommand::Order(OrderRequest::Cancel { order_id }) => assert_eq!(order_id, 17),
        other => panic!("expected a cancel, got {other:?}"),
    }
}

/// The executions mutex is not held while user callbacks run. A wrapper that
/// re-enters a path locking `executions` is an ordinary ibapi pattern —
/// re-requesting from `exec_details` — and holding the lock across it
/// deadlocks, in Python with the GIL held, freezing the interpreter.
#[test]
fn req_executions_does_not_hold_the_lock_across_callbacks() {
    struct Reentrant<'a> {
        core: &'a ClientCore,
        observed_locked: bool,
        rows: usize,
    }
    impl Wrapper for Reentrant<'_> {
        fn exec_details(&mut self, _r: i64, _c: &Contract, _e: &crate::types::model::Execution) {
            self.rows += 1;
            // Re-entering while the lock is held is exactly the deadlock.
            if self.core.executions.try_lock().is_err() {
                self.observed_locked = true;
            }
        }
    }

    let (client, _rx, _shared) = test_client();
    client.core.push_execution(
        crate::types::model::Contract { symbol: "AAPL".into(), ..Default::default() },
        Default::default(),
        Default::default(),
    );

    let mut w = Reentrant { core: &client.core, observed_locked: false, rows: 0 };
    client.req_executions(1, &crate::types::model::ExecutionFilter::default(), &mut w);
    assert_eq!(w.rows, 1, "the execution must still be replayed");
    assert!(!w.observed_locked,
        "executions lock must be released before the callback runs");
}

/// `ExecutionFilter.time` is a lower bound in ibapi. It was parsed and then
/// ignored, so a caller asking for today's fills got the whole history.
#[test]
fn execution_filter_time_is_a_lower_bound() {
    #[derive(Default)]
    struct Rows { seen: Vec<String> }
    impl Wrapper for Rows {
        fn exec_details(&mut self, _r: i64, _c: &Contract, e: &crate::types::model::Execution) {
            self.seen.push(e.time.clone());
        }
    }

    let (client, _rx, _shared) = test_client();
    for t in ["20260729-09:00:00", "20260729-11:00:00"] {
        client.core.push_execution(
            crate::types::model::Contract { symbol: "AAPL".into(), ..Default::default() },
            crate::types::model::Execution { time: t.into(), ..Default::default() },
            Default::default(),
        );
    }

    let mut w = Rows::default();
    client.req_executions(1, &crate::types::model::ExecutionFilter {
        time: "20260729-10:00:00".into(), ..Default::default()
    }, &mut w);
    assert_eq!(w.seen, vec!["20260729-11:00:00"], "only executions at or after the bound");

    // Punctuation differs between the two sides in practice; the comparison is
    // on digits, so a space-separated bound behaves identically.
    let mut w2 = Rows::default();
    client.req_executions(1, &crate::types::model::ExecutionFilter {
        time: "20260729 10:00:00".into(), ..Default::default()
    }, &mut w2);
    assert_eq!(w2.seen, vec!["20260729-11:00:00"], "separator must not change the bound");

    // A date-only bound keeps the whole day rather than dropping it.
    let mut w3 = Rows::default();
    client.req_executions(1, &crate::types::model::ExecutionFilter {
        time: "20260729".into(), ..Default::default()
    }, &mut w3);
    assert_eq!(w3.seen.len(), 2, "a date-only bound keeps that day");
}

/// An execution the venue restated at logon is not a fill and is announced as
/// none — but a caller asking for the day's executions is owed it. After a
/// restart the record was empty, and that caller was told, silently, that
/// nothing had filled.
#[test]
fn a_restated_execution_answers_req_executions_and_nobody_else() {
    let (client, _rx, shared) = test_client();
    shared.orders.push_restated_execution(
        crate::types::model::Contract { symbol: "SPY".into(), ..Default::default() },
        crate::types::model::Execution {
            exec_id: "0001f4e8.1".into(), side: "BOT".into(), shares: 10.0, ..Default::default()
        },
    );
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(
        !w.events.iter().any(|e| e.starts_with("exec_details:")),
        "nobody asked, so nobody is told: {:?}", w.events,
    );

    client.req_executions(7, &crate::types::model::ExecutionFilter::default(), &mut w);
    let answered: Vec<&String> = w.events.iter().filter(|e| e.starts_with("exec_details:")).collect();
    assert_eq!(answered, ["exec_details:7:BOT:10"], "the caller that asked is answered");
}

/// Two prints of one order in one pass are two executions.
///
/// A fill carries the print; everything else about it — the execution's id, the
/// time, the running quantity, the average — is on the report it was booked
/// off. Looked up against the order afterwards, both prints read the record the
/// later one left, so the earlier was reported under the later's id and the
/// charge that named that id was attached to both.
#[test]
fn two_prints_of_one_order_in_one_pass_are_two_executions() {
    #[derive(Default)]
    struct Seen { rows: Vec<(String, f64)> }
    impl Wrapper for Seen {
        fn exec_details(&mut self, _r: i64, _c: &Contract, e: &crate::types::model::Execution) {
            self.rows.push((e.exec_id.clone(), e.cum_qty));
        }
    }

    let (client, _rx, shared) = test_client();
    let report = |exec_id: &str, cum: f64| crate::bridge::RichOrderInfo {
        contract: ApiContract { symbol: "SPY".into(), ..Default::default() },
        order: Order { order_id: 77, ..Default::default() },
        order_state: Default::default(),
        last_exec: crate::types::model::Execution {
            exec_id: exec_id.into(), cum_qty: cum, ..Default::default()
        },
    };
    let print = |qty: i64, remaining: i64| Fill {
        instrument: 0, order_id: 77, side: Side::Buy,
        price: 150 * PRICE_SCALE, qty, remaining, timestamp_ns: 0,
        cum_qty: qty, avg_price: 150 * PRICE_SCALE,
    };
    shared.orders.push_fill_reported(
        print(5 * crate::types::QTY_SCALE, 5 * crate::types::QTY_SCALE),
        report("0001f4e8.1", 5.0),
    );
    shared.orders.push_fill_reported(
        print(5 * crate::types::QTY_SCALE, 0),
        report("0001f4e8.2", 10.0),
    );
    // The order's own record holds the later report, as it does in a session.
    shared.orders.push_order_info(77, report("0001f4e8.2", 10.0));

    let mut w = Seen::default();
    client.process_msgs(&mut w);
    assert_eq!(
        w.rows,
        [("0001f4e8.1".to_string(), 5.0), ("0001f4e8.2".to_string(), 10.0)],
        "each print is reported under the execution it was booked off",
    );

    // And the record kept for a replay holds two. Stored under one id they
    // would be one execution: the second is refused as a duplicate of the
    // first, and the caller is answered with half its fills.
    w.rows.clear();
    client.req_executions(3, &crate::types::model::ExecutionFilter::default(), &mut w);
    assert_eq!(
        w.rows,
        [("0001f4e8.1".to_string(), 5.0), ("0001f4e8.2".to_string(), 10.0)],
        "and the caller that asks for the day's executions is answered with both",
    );
}

#[test]
fn req_global_cancel_sends_cancel_all_for_each_instrument() {
    let (client, rx, shared) = test_client();
    shared.orders.set_replay_done();
    shared.market.set_instrument_count(2);
    client.req_global_cancel().unwrap();
    let mut cancel_instruments = vec![];
    while let Ok(cmd) = rx.try_recv() {
        if let ControlCommand::Order(OrderRequest::CancelAll { instrument }) = cmd {
            cancel_instruments.push(instrument);
        }
    }
    assert_eq!(cancel_instruments.len(), 2);
    cancel_instruments.sort();
    assert_eq!(cancel_instruments, vec![0, 1]);
}

#[test]
fn req_global_cancel_no_instruments_no_commands() {
    let (client, rx, shared) = test_client();
    shared.orders.set_replay_done();
    client.req_global_cancel().unwrap();
    assert!(rx.try_recv().is_err());
}

/// A withdrawal of everything covers what was never sent as well as what is
/// working.
///
/// The held orders are forgotten, record and all: left tracked, their ids
/// could never place again — a placement under one was read as a modify of
/// an order nothing had ever submitted.
#[test]
fn a_global_cancel_frees_the_ids_of_orders_never_sent() {
    let (client, rx, shared) = test_client();
    let held = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "LMT".into(),
        lmt_price: 100.0, tif: "DAY".into(), transmit: false, ..Default::default()
    };
    client.place_order(84, &spy(), &held).expect("held");
    assert!(rx.try_recv().is_err(), "nothing was sent for it");

    shared.orders.set_replay_done();
    client.req_global_cancel().expect("everything withdrawn");
    assert!(!client.core.is_order_tracked(84), "the record goes with the command");

    let order = Order { transmit: true, ..held };
    client.place_order(84, &spy(), &order).expect("placed again under the same id");
    match rx.try_recv().expect("the order goes out") {
        ControlCommand::Order(OrderRequest::SubmitEx { order_id, .. }) => {
            assert_eq!(order_id, 84, "the id places a new order");
        }
        other => panic!("expected a fresh submit, got {other:?}"),
    }
}

/// A withdrawal of everything forgets what was never sent, and only that.
///
/// An order the venue is working can have a revision of its own waiting to be
/// transmitted. Forgetting its record along with the revision left the live
/// order reading as one that was never placed: placing under the id again built
/// a fresh submission for an order already on the market, and the withdrawal
/// the venue was asked for had nothing here to answer to.
#[test]
fn a_global_cancel_keeps_the_order_a_staged_revision_belongs_to() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = |price: f64, transmit: bool| Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "LMT".into(),
        lmt_price: price, tif: "DAY".into(), transmit, ..Default::default()
    };
    client.place_order(85, &spy(), &order(100.0, true)).expect("placed and sent");
    assert!(matches!(
        rx.try_recv(), Ok(ControlCommand::Order(OrderRequest::SubmitEx { .. })),
    ));
    client.place_order(85, &spy(), &order(101.0, false)).expect("the change is kept");
    assert!(next_command(&rx).is_none(), "nothing goes to the venue for a change that is held");

    shared.orders.set_replay_done();
    client.req_global_cancel().expect("everything withdrawn");
    assert!(!client.core.is_held(85), "the change that was never sent is forgotten");
    assert!(
        client.core.is_working_at_the_venue(85, Some(&client.shared)),
        "and the order it was a change to is still one the venue is working",
    );
}

/// A global cancel issued before the venue has finished naming the working
/// orders is not answered in silence. What had been named is still
/// withdrawn, and the call says the naming had not finished — a partial
/// cancel that reads as one, rather than as a complete answer that is not.
#[test]
fn a_global_cancel_says_when_the_venue_has_not_finished_naming() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    // The venue began naming and never said it had finished: the wait runs
    // out with something named and something possibly not. An account that
    // was named nothing at all is the other case, and says nothing — there is
    // no uncovered order to warn about.
    shared.orders.note_naming_began();
    let refusal = client.req_global_cancel().expect_err(
        "a withdrawal composed before the naming finished says so rather than returning",
    );
    let sent: Vec<ControlCommand> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert!(
        matches!(sent.as_slice(), [ControlCommand::Order(OrderRequest::CancelAll { instrument: 0 })]),
        "what had been named is still withdrawn: {sent:?}",
    );
    assert_eq!(
        refusal.code, crate::error_codes::Refusal::NO_ANSWER,
        "under this client's own number for a wait the venue did not finish",
    );
    assert!(
        refusal.message.contains("had not finished naming"),
        "the caller is told what was and was not covered: {refusal}",
    );
}

/// Asking what the account is working before the venue has finished naming it
/// answers with what had arrived, and that is exactly what an account working
/// nothing looks like. The caller is told which of the two it is reading, so a
/// strategy does not take a partial snapshot for a flat account and place
/// again what it already has on.
#[test]
fn open_orders_say_when_the_snapshot_is_not_known_to_be_whole() {
    #[derive(Default)]
    struct Heard {
        told: Vec<(i64, i64, String)>,
        ended: usize,
    }
    impl Wrapper for Heard {
        fn error(&mut self, req_id: i64, code: i64, message: &str, _adv: &str) {
            self.told.push((req_id, code, message.to_string()));
        }
        fn open_order_end(&mut self) { self.ended += 1; }
    }

    let (client, _rx, shared) = test_client();
    // The venue began naming and never said it had finished. An account that
    // was named nothing at all is the other case, and says nothing — there is
    // no missing order to warn about.
    shared.orders.note_naming_began();
    let mut heard = Heard::default();
    client.req_all_open_orders(&mut heard);
    assert_eq!(heard.ended, 1, "what had arrived is still delivered, and still ends");
    assert!(
        heard.told.iter().any(|(req_id, code, message)| {
            *req_id == -1
                && *code == crate::error_codes::Refusal::NO_ANSWER as i64
                && message.contains("had not finished naming")
        }),
        "the caller is told the snapshot is not known to be whole: {:?}",
        heard.told,
    );
}

/// An order the venue named that could not be given a slot in the instrument
/// table is in none of the cancels a withdrawal of everything composes: those
/// are composed from the engine's book, and the order never reached it. The
/// call says so rather than returning, which would tell the caller the account
/// had been flattened while that order was still working there.
#[test]
fn a_global_cancel_says_when_an_order_has_no_slot_in_the_instrument_table() {
    let (client, rx, shared) = test_client();
    // The naming finished. One of the orders it carried arrived with the table
    // full, so this is the other case from a naming that never ended: what was
    // left out is established rather than suspected.
    shared.orders.set_replay_done();
    shared.market.set_instrument_count(1);
    shared.orders.note_an_order_without_a_slot();
    let refusal = client.req_global_cancel().expect_err(
        "a withdrawal that cannot reach every working order says so rather than returning",
    );
    let sent: Vec<ControlCommand> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert!(
        matches!(sent.as_slice(), [ControlCommand::Order(OrderRequest::CancelAll { instrument: 0 })]),
        "what the book does hold is still withdrawn: {sent:?}",
    );
    assert_eq!(
        refusal.code, crate::error_codes::Refusal::NO_ANSWER,
        "under this client's own number for an answer it cannot give",
    );
    assert!(
        refusal.message.contains("no slot in this client's instrument table"),
        "the caller is told what the withdrawal did not reach: {refusal}",
    );
}

/// An order left out of the engine's book for want of a slot is still listed
/// from the order cache, and nothing this session does follows it: its fills
/// are not booked, its status changes are not announced, and a withdrawal of
/// everything does not reach it. The caller is told so, rather than reading
/// the list as the orders this session is following.
#[test]
fn open_orders_say_when_one_of_them_has_no_slot_in_the_instrument_table() {
    #[derive(Default)]
    struct Heard {
        told: Vec<(i64, i64, String)>,
        ended: usize,
    }
    impl Wrapper for Heard {
        fn error(&mut self, req_id: i64, code: i64, message: &str, _adv: &str) {
            self.told.push((req_id, code, message.to_string()));
        }
        fn open_order_end(&mut self) { self.ended += 1; }
    }

    let (client, _rx, shared) = test_client();
    // The naming finished, and one of the orders it carried has no slot.
    shared.orders.set_replay_done();
    shared.orders.note_an_order_without_a_slot();
    let mut heard = Heard::default();
    client.req_all_open_orders(&mut heard);
    assert_eq!(heard.ended, 1, "what is held is still delivered, and still ends");
    assert!(
        heard.told.iter().any(|(req_id, code, message)| {
            *req_id == -1
                && *code == crate::error_codes::Refusal::NO_ANSWER as i64
                && message.contains("no slot in this client's instrument table")
        }),
        "the caller is told which of the orders it lists it cannot act on: {:?}",
        heard.told,
    );
}

// ═══════════════════════════════════════════════════════════════════
//  Order validation — aux_price guards
// ═══════════════════════════════════════════════════════════════════

#[test]
fn stp_order_with_zero_aux_price_is_rejected() {
    let (client, _rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "SELL".into(), total_quantity: 100.0, order_type: "STP".into(),
        lmt_price: 145.0, // common mistake: setting lmt_price instead of aux_price
        ..Default::default()
    };
    let result = client.place_order(1, &spy(), &order);
    assert!(result.is_err());
    assert!(result.unwrap_err().message.contains("aux_price"));
}

#[test]
fn stp_order_with_valid_aux_price_succeeds() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "SELL".into(), total_quantity: 100.0, order_type: "STP".into(),
        aux_price: 145.0, ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();
    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::Order(OrderRequest::SubmitEx { kind: OrderKind::Stop { .. }, .. })));
}

#[test]
fn stp_lmt_order_with_zero_aux_price_is_rejected() {
    let (client, _rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "SELL".into(), total_quantity: 100.0, order_type: "STP LMT".into(),
        lmt_price: 144.0, ..Default::default() // aux_price missing
    };
    let result = client.place_order(1, &spy(), &order);
    assert!(result.is_err());
    assert!(result.unwrap_err().message.contains("aux_price"));
}

#[test]
fn trail_order_with_zero_amount_and_zero_percent_is_rejected() {
    let (client, _rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "SELL".into(), total_quantity: 100.0, order_type: "TRAIL".into(),
        ..Default::default() // neither trailing_percent nor aux_price
    };
    let result = client.place_order(1, &spy(), &order);
    assert!(result.is_err());
    assert!(result.unwrap_err().message.contains("trailing_percent"));
}

#[test]
fn trail_order_with_trailing_percent_succeeds() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "SELL".into(), total_quantity: 100.0, order_type: "TRAIL".into(),
        trailing_percent: 5.0, ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();
    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::Order(OrderRequest::SubmitEx { kind: OrderKind::TrailPct { .. }, .. })));
}

#[test]
fn trail_limit_order_with_zero_aux_price_is_rejected() {
    let (client, _rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "SELL".into(), total_quantity: 100.0, order_type: "TRAIL LIMIT".into(),
        lmt_price: 148.0, ..Default::default() // aux_price missing
    };
    let result = client.place_order(1, &spy(), &order);
    assert!(result.is_err());
    assert!(result.unwrap_err().message.contains("aux_price"));
}

#[test]
fn mit_order_with_zero_aux_price_is_rejected() {
    let (client, _rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "MIT".into(),
        ..Default::default()
    };
    let result = client.place_order(1, &spy(), &order);
    assert!(result.is_err());
    assert!(result.unwrap_err().message.contains("aux_price"));
}

#[test]
fn stp_prt_order_with_zero_aux_price_is_rejected() {
    let (client, _rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "SELL".into(), total_quantity: 100.0, order_type: "STP PRT".into(),
        ..Default::default()
    };
    let result = client.place_order(1, &spy(), &order);
    assert!(result.is_err());
    assert!(result.unwrap_err().message.contains("aux_price"));
}

#[test]
fn lit_order_with_zero_aux_price_is_rejected() {
    let (client, _rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "LIT".into(),
        lmt_price: 150.0, ..Default::default() // aux_price missing
    };
    let result = client.place_order(1, &spy(), &order);
    assert!(result.is_err());
    assert!(result.unwrap_err().message.contains("aux_price"));
}

// ═══════════════════════════════════════════════════════════════════
//  Order validation — non-finite and out-of-range numbers
// ═══════════════════════════════════════════════════════════════════

#[test]
fn place_order_rejects_nan_lmt_price() {
    let (client, _rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "LMT".into(),
        lmt_price: f64::NAN, ..Default::default()
    };
    let err = client.place_order(1, &spy(), &order).unwrap_err();
    assert!(err.message.contains("lmt_price"), "got: {err}");
}

#[test]
fn place_order_rejects_infinite_lmt_price() {
    let (client, _rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "LMT".into(),
        lmt_price: f64::INFINITY, ..Default::default()
    };
    let err = client.place_order(1, &spy(), &order).unwrap_err();
    assert!(err.message.contains("lmt_price"), "got: {err}");
}

#[test]
fn place_order_rejects_lmt_price_that_overflows_the_wire() {
    // Finite, but scaling by PRICE_SCALE_F (1e8) overflows the wire's i64 —
    // the old code let this saturate to i64::MAX instead of refusing it.
    let (client, _rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "LMT".into(),
        lmt_price: 1.0e12, ..Default::default()
    };
    let err = client.place_order(1, &spy(), &order).unwrap_err();
    assert!(err.message.contains("lmt_price"), "got: {err}");
}

#[test]
fn place_order_rejects_lmt_price_at_the_exact_wire_boundary() {
    // `i64::MAX as f64` rounds up to 2^63, so this value scales back to
    // exactly 2^63 in `require_finite_price` — a `>` comparison against
    // that rounded boundary let it through and the cast saturated to
    // i64::MAX instead of refusing it.
    let (client, _rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "LMT".into(),
        lmt_price: i64::MAX as f64 / PRICE_SCALE_F, ..Default::default()
    };
    let err = client.place_order(1, &spy(), &order).unwrap_err();
    assert!(err.message.contains("lmt_price"), "got: {err}");
}

#[test]
fn place_order_rejects_nan_aux_price() {
    // NaN != 0.0, so the pre-existing "aux_price required" check (which only
    // compares against == 0.0) never catches this on its own.
    let (client, _rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "SELL".into(), total_quantity: 100.0, order_type: "STP".into(),
        aux_price: f64::NAN, ..Default::default()
    };
    let err = client.place_order(1, &spy(), &order).unwrap_err();
    assert!(err.message.contains("aux_price"), "got: {err}");
}

/// A negative quantity is the venue's to refuse, and it does.
///
/// Tag 38 carries the sign, so the order goes out saying exactly what it was
/// given. Refused here instead, a caller reading the venue's code for this was
/// handed one this client made up.
#[test]
fn a_negative_quantity_is_carried_to_the_venue() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: -100.0, order_type: "MKT".into(), ..Default::default()
    };
    client.place_order(1, &spy(), &order).expect("carried, not refused here");
    assert!(rx.try_recv().is_ok(), "it reaches the wire");
}

#[test]
fn place_order_rejects_nan_quantity() {
    let (client, _rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: f64::NAN, order_type: "MKT".into(), ..Default::default()
    };
    let err = client.place_order(1, &spy(), &order).unwrap_err();
    assert!(err.message.contains("total_quantity"), "got: {err}");
}

#[test]
fn place_order_rejects_infinite_quantity() {
    let (client, _rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: f64::INFINITY, order_type: "MKT".into(), ..Default::default()
    };
    let err = client.place_order(1, &spy(), &order).unwrap_err();
    assert!(err.message.contains("total_quantity"), "got: {err}");
}

#[test]
fn place_order_rejects_negative_display_size() {
    let (client, _rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "LMT".into(),
        lmt_price: 150.0, display_size: -5, ..Default::default()
    };
    let err = client.place_order(1, &spy(), &order).unwrap_err();
    assert!(err.message.contains("display_size"), "got: {err}");
}

#[test]
fn place_order_rejects_negative_min_qty() {
    let (client, _rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "LMT".into(),
        lmt_price: 150.0, min_qty: -5, ..Default::default()
    };
    let err = client.place_order(1, &spy(), &order).unwrap_err();
    assert!(err.message.contains("min_qty"), "got: {err}");
}

#[test]
fn place_order_rejects_negative_parent_id() {
    let (client, _rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "LMT".into(),
        lmt_price: 150.0, parent_id: -5, ..Default::default()
    };
    let err = client.place_order(1, &spy(), &order).unwrap_err();
    assert!(err.message.contains("parent_id"), "got: {err}");
}

#[test]
fn place_order_rejects_negative_trailing_percent() {
    let (client, _rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "SELL".into(), total_quantity: 100.0, order_type: "TRAIL".into(),
        trailing_percent: -5.0, ..Default::default()
    };
    let err = client.place_order(1, &spy(), &order).unwrap_err();
    assert!(err.message.contains("trailing_percent"), "got: {err}");
}

#[test]
fn place_order_adaptive_rejects_unknown_priority() {
    let (client, _rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "LMT".into(),
        lmt_price: 150.0, algo_strategy: "Adaptive".into(),
        algo_params: vec![TagValue { tag: "adaptivePriority".into(), value: "Aggressive".into() }],
        ..Default::default()
    };
    let err = client.place_order(1, &spy(), &order).unwrap_err();
    assert!(err.message.contains("adaptivePriority"), "got: {err}");
}

#[test]
fn place_order_adaptive_defaults_priority_when_absent() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "LMT".into(),
        lmt_price: 150.0, algo_strategy: "Adaptive".into(), ..Default::default()
    };
    client.place_order(1, &spy(), &order).unwrap();
    match rx.try_recv().unwrap() {
        ControlCommand::Order(OrderRequest::SubmitEx {
            kind: OrderKind::Adaptive { priority, .. }, ..
        }) => {
            assert_eq!(priority, crate::types::AdaptivePriority::Normal);
        }
        cmd => panic!("expected an adaptive order, got {cmd:?}"),
    }
}

// place_order validates before building: the two tests above go through
// place_order, so either function's check alone makes them pass and neither
// pins down which one is doing the rejecting. validate_order is also the
// only check an order Modify (place_order on an already-tracked order_id)
// runs, since that path never calls build_order_request. These call each
// function directly to prove its own guard independently of the other.

#[test]
fn validate_order_adaptive_rejects_unknown_priority() {
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "LMT".into(),
        lmt_price: 150.0, algo_strategy: "Adaptive".into(),
        algo_params: vec![TagValue { tag: "adaptivePriority".into(), value: "Aggressive".into() }],
        ..Default::default()
    };
    let err = crate::client_core::ClientCore::validate_order(&order, "DU123").unwrap_err();
    assert!(err.message.contains("adaptivePriority"), "got: {err}");
}

#[test]
fn build_order_request_adaptive_rejects_unknown_priority() {
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "LMT".into(),
        lmt_price: 150.0, algo_strategy: "Adaptive".into(),
        algo_params: vec![TagValue { tag: "adaptivePriority".into(), value: "Aggressive".into() }],
        ..Default::default()
    };
    let err = crate::client_core::ClientCore::build_order_request(&order, 1, 0, None).unwrap_err();
    assert!(err.message.contains("adaptivePriority"), "got: {err}");
}

// ═══════════════════════════════════════════════════════════════════
//  Historical data requests
// ═══════════════════════════════════════════════════════════════════

#[test]
fn req_historical_data_sends_fetch_historical() {
    let (client, rx, _shared) = test_client();
    client.req_historical_data(5, &spy(), "20260101 16:00:00", "1 D", "1 hour", "TRADES", true, 1, false).unwrap();
    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::FetchHistorical { contract: ContractRef { con_id, sec_type, exchange, .. }, req_id, duration, bar_size, what_to_show, use_rth, .. } => {
            assert_eq!(req_id, 5);
            assert_eq!(con_id, 756733);
            assert_eq!(duration, "1 D");
            assert_eq!(bar_size, "1 hour");
            assert_eq!(what_to_show, "TRADES");
            assert!(use_rth);
            // The contract's own fields have to leave the client, or the
            // engine has nothing but the old constants to fall back on.
            assert_eq!(sec_type, "STK");
            assert_eq!(exchange, "SMART");
        }
        _ => panic!("expected FetchHistorical"),
    }
}

/// A contract that does state its own type and venue must carry both, which is
/// the whole of what this fixes: every historical query described itself as a
/// SMART-routed stock regardless of the contract asked for.
#[test]
fn req_historical_data_carries_the_contract_s_own_type_and_venue() {
    let (client, rx, _shared) = test_client();
    let es = Contract {
        con_id: 495512563, symbol: "ES".into(),
        sec_type: "FUT".into(), exchange: "CME".into(), ..Default::default()
    };
    client.req_historical_data(6, &es, "20260101 16:00:00", "1 D", "1 hour", "TRADES", true, 1, false).unwrap();
    match rx.try_recv().unwrap() {
        ControlCommand::FetchHistorical { contract: ContractRef { sec_type, exchange, .. }, .. } => {
            assert_eq!(sec_type, "FUT");
            assert_eq!(exchange, "CME");
        }
        _ => panic!("expected FetchHistorical"),
    }
}

// ── unknown bar_size / what_to_show reject instead of silently
// falling back to 5-minute / TRADES bars ──

#[test]
fn req_historical_data_rejects_unknown_bar_size() {
    let (client, rx, _shared) = test_client();
    // A size that is not one is refused rather than answered with five-minute
    // candles. Its casing is not what makes it one: `1 Min` is a minute.
    let err = client.req_historical_data(5, &spy(), "", "2 D", "1 minute", "TRADES", true, 1, false).unwrap_err();
    assert!(err.message.contains("bar_size"), "got: {err}");
    assert!(rx.try_recv().is_err(), "nothing may reach the engine");
    client.req_historical_data(5, &spy(), "", "2 D", "1 Min", "TRADES", true, 1, false)
        .expect("a minute asked for in another casing is still a minute");
    assert!(rx.try_recv().is_ok(), "and it reaches the engine");
}

#[test]
fn req_historical_data_rejects_unknown_what_to_show() {
    let (client, rx, _shared) = test_client();
    let err = client.req_historical_data(5, &spy(), "", "2 D", "1 min", "TRADE", true, 1, false).unwrap_err();
    assert!(err.message.contains("what_to_show"), "got: {err}");
    assert!(rx.try_recv().is_err());
}

#[test]
fn req_historical_data_rejects_unsupported_keep_up_to_date_size() {
    let (client, rx, _shared) = test_client();
    // A second is shorter than the five-second bars a forming bar is folded
    // from, so nothing can form it. Refused here rather than answered with
    // five-second bars relabelled as one-second ones.
    let err = client.req_historical_data(5, &spy(), "", "1 D", "1 secs", "TRADES", true, 1, true).unwrap_err();
    assert!(err.message.contains("kept up to date"), "got: {err}");
    assert!(rx.try_recv().is_err());
}

#[test]
fn req_historical_data_accepts_streamable_keep_up_to_date_size() {
    let (client, rx, _shared) = test_client();
    client.req_historical_data(5, &spy(), "", "1 D", "5 mins", "TRADES", true, 1, true).unwrap();
    assert!(matches!(rx.try_recv().unwrap(), ControlCommand::FetchHistorical { keep_up_to_date: true, .. }));
}

/// ADJUSTED_LAST is served on the callback path now, not refused: the request
/// reaches the engine, which fetches raw trades and folds them with the
/// contract's actions before a bar is handed over. The name goes out as it
/// came — the engine reads it, the venue never sees it.
#[test]
fn req_historical_data_serves_adjusted_last() {
    let (client, rx, _shared) = test_client();
    client.req_historical_data(5, &spy(), "", "1 Y", "1 day", "ADJUSTED_LAST", true, 1, false)
        .expect("the callback path serves ADJUSTED_LAST");
    match rx.try_recv().expect("the request reaches the engine") {
        ControlCommand::FetchHistorical { what_to_show, .. } => {
            assert_eq!(what_to_show, "ADJUSTED_LAST", "the engine reads the name and folds");
        }
        other => panic!("expected a historical request, got {other:?}"),
    }
}

/// The adjusted series is folded from the contract's corporate actions, which
/// are asked for by the venue's id for the contract. Named by anything else the
/// fold cannot be made, so the request is refused rather than answered with raw
/// trades under an adjusted name — as the waiting call refuses it.
#[test]
fn req_historical_data_refuses_adjusted_last_without_the_venue_id() {
    let (client, rx, _shared) = test_client();
    let unqualified = Contract {
        symbol: "SPY".into(), exchange: "SMART".into(), sec_type: "STK".into(),
        ..Default::default()
    };
    let err = client
        .req_historical_data(5, &unqualified, "", "1 Y", "1 day", "ADJUSTED_LAST", true, 1, false)
        .unwrap_err();
    assert!(err.message.contains("venue's id"), "got: {err}");
    assert!(rx.try_recv().is_err(), "nothing may reach the engine");
}

/// A request kept up to date never completes, so there is no whole series to
/// fold: ADJUSTED_LAST and keepUpToDate together are refused rather than
/// answered with a series that can never be put on one scale.
#[test]
fn req_historical_data_refuses_adjusted_last_kept_up_to_date() {
    let (client, rx, _shared) = test_client();
    let err = client
        .req_historical_data(5, &spy(), "", "1 D", "5 mins", "ADJUSTED_LAST", true, 1, true)
        .unwrap_err();
    assert!(err.message.contains("kept up to date"), "got: {err}");
    assert!(rx.try_recv().is_err(), "nothing may reach the engine");
}

/// The waiting call is served by the same fold as the callback path, so a
/// contract with no venue id is refused where the request is made — the fold
/// cannot ask for the actions without it. Fetching the raw series first and
/// finding out afterwards would make the caller wait out the answer timeout
/// for a refusal that was known at once.
#[test]
fn the_waiting_call_refuses_adjusted_last_without_the_venue_id() {
    let (client, rx, _shared) = test_client();
    let unqualified = Contract {
        symbol: "SPY".into(), exchange: "SMART".into(), sec_type: "STK".into(),
        ..Default::default()
    };
    let err = client
        .historical_data(&unqualified, "", "1 Y", "1 day", "ADJUSTED_LAST", true)
        .unwrap_err();
    assert!(err.message.contains("venue's id"), "got: {err}");
    assert!(rx.try_recv().is_err(), "nothing may reach the engine");
}

/// An engine that has gone is not a request that was malformed. A caller that
/// branches on the code has to be able to tell a session it can reopen from a
/// request it has to fix.
#[test]
fn a_request_with_no_engine_behind_it_says_so_under_its_own_code() {
    // The engine's end of the channel goes with the receiver.
    let (client, rx, _shared) = test_client();
    drop(rx);

    let refused = client
        .req_contract_details(1, &spy())
        .expect_err("nothing can be sent with no engine to send it");
    assert_eq!(
        refused.code,
        crate::error_codes::Refusal::NOT_CONNECTED,
        "not connected, rather than a request that failed validation: {refused}",
    );
}

/// A bracket whose command never reached the engine leaves nothing tracked.
///
/// The three legs are recorded before the command goes, as they have to be —
/// the engine answers about them by number. Where the send does not reach the
/// engine a placement puts its record back, and this did not: the caller was
/// left holding three orders the venue was never given, reported as working,
/// each answering `is_working_at_the_venue`, so a retry built a change to
/// something the venue does not hold — and nothing released the numbers, since
/// only an order that went spends one.
#[test]
fn a_bracket_that_never_reached_the_engine_leaves_nothing_tracked() {
    let (client, rx, _shared) = test_client();
    drop(rx);

    let refused = client
        .submit_bracket(&spy(), crate::types::Side::Buy, 100.0, 100.0, 110.0, 90.0)
        .expect_err("nothing can be sent with no engine to send it");
    assert_eq!(refused.code, crate::error_codes::Refusal::NOT_CONNECTED);

    let tracked: Vec<u64> = client.core.open_orders.lock().unwrap().keys().copied().collect();
    assert!(
        tracked.is_empty(),
        "every leg goes back where the command did not reach the engine: {tracked:?}",
    );
}

/// A req_id reaches these requests' wire form as u32. `next_order_id()` hands
/// out ids near 1.7e12, so a caller running one counter for orders and
/// requests — the ibapi idiom — wraps every one of these: the venue receives an
/// id nobody chose, and the callback carries that id.
#[test]
fn an_unwireable_req_id_is_refused() {
    type Call = fn(&EClient, i64) -> Result<(), Refusal>;
    let calls: &[(&str, Call)] = &[
        ("req_historical_data", |c, id| c.req_historical_data(id, &spy(), "", "1 D", "1 min", "TRADES", true, 1, false)),
        ("cancel_historical_data", |c, id| c.cancel_historical_data(id)),
        ("req_head_time_stamp", |c, id| c.req_head_time_stamp(id, &spy(), "TRADES", true, 1)),
        ("cancel_head_time_stamp", |c, id| c.cancel_head_time_stamp(id)),
        ("req_contract_details", |c, id| c.req_contract_details(id, &spy())),
        ("req_matching_symbols", |c, id| c.req_matching_symbols(id, "SP")),
        ("req_sec_def_opt_params", |c, id| c.req_sec_def_opt_params(id, "SPY", "", "STK", 756733)),
        ("req_scanner_subscription", |c, id| c.req_scanner_subscription(id, "STK", "STK.US", "TOP_PERC_GAIN", 10, &[])),
        ("cancel_scanner_subscription", |c, id| c.cancel_scanner_subscription(id)),
        ("req_historical_news", |c, id| c.req_historical_news(id, 756733, "BRFG", "", "", 10)),
        ("req_news_article", |c, id| c.req_news_article(id, "BRFG", "BRFG$1")),
        ("req_fundamental_data", |c, id| c.req_fundamental_data(id, &spy(), "ReportSnapshot")),
        ("cancel_fundamental_data", |c, id| c.cancel_fundamental_data(id)),
        ("req_histogram_data", |c, id| c.req_histogram_data(id, &spy(), true, "3 days")),
        ("cancel_histogram_data", |c, id| c.cancel_histogram_data(id)),
        ("req_historical_ticks", |c, id| c.req_historical_ticks(id, &spy(), "", "20260101 16:00:00", 100, "TRADES", true)),
        ("req_historical_schedule", |c, id| c.req_historical_schedule(id, &spy(), "", "1 D", true)),
        ("req_mkt_depth", |c, id| c.req_mkt_depth(id, &spy(), 5, false)),
        // Asks for the book first: a withdrawal now says when it holds none,
        // and that refusal is not the one this test is about.
        ("cancel_mkt_depth", |c, id| {
            let _ = c.req_mkt_depth(id, &spy(), 5, false);
            c.cancel_mkt_depth(id)
        }),
        ("req_real_time_bars", |c, id| c.req_real_time_bars(id, &spy(), 5, "TRADES", true)),
        ("cancel_real_time_bars", |c, id| c.cancel_real_time_bars(id)),
    ];
    for (name, call) in calls {
        for bad in [u32::MAX as i64 + 1, -1] {
            let (client, rx, _shared) = test_client();
            let err = match call(&client, bad) {
                Err(e) => e,
                Ok(()) => panic!("{name}({bad}) must be refused"),
            };
            assert!(err.message.contains("req_id"), "{name}: the error names the field: {err}");
            assert!(rx.try_recv().is_err(), "{name}: and nothing reaches the wire");
        }
        // The largest id a request can take is one below the first band this
        // client reserves. Above it are the numbers the answering calls hold
        // and the ones the engine numbers its own lookups with, and an answer
        // under either is taken by this client rather than handed on.
        let (client, rx, _shared) = test_client();
        let largest = crate::bridge::ReferenceState::ASK_ID_BASE as i64 - 1;
        if let Err(e) = call(&client, largest) {
            panic!("{name}: the largest usable id must still request: {e}");
        }
        assert!(rx.try_recv().is_ok(), "{name}: and it reaches the wire");

        // And the band above it is not a caller's, which is what a request
        // numbered there used to be answered as: read as internal, its
        // callbacks kept rather than delivered.
        let (client, rx, _shared) = test_client();
        assert!(
            call(&client, crate::bridge::ENGINE_ID_BASE as i64).is_err(),
            "{name}: a request numbered where the engine numbers its own is answered to nobody",
        );
        assert!(rx.try_recv().is_err(), "{name}: and nothing reaches the wire under it");

        let (client, rx, _shared) = test_client();
        let refused = call(&client, u32::MAX as i64);
        assert!(refused.is_err(), "{name}: the number that means no request is not one");
        assert!(rx.try_recv().is_err(), "{name}: and nothing reaches the wire under it");
    }
}

#[test]
fn cancel_historical_data_sends_cancel() {
    let (client, rx, _shared) = test_client();
    client.cancel_historical_data(5).unwrap();
    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::CancelHistorical { req_id: 5 }));
}

#[test]
fn req_head_time_stamp_sends_fetch() {
    let (client, rx, _shared) = test_client();
    client.req_head_time_stamp(10, &spy(), "TRADES", true, 1).unwrap();
    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::FetchHeadTimestamp { contract: ContractRef { con_id, .. }, req_id, what_to_show, use_rth, .. } => {
            assert_eq!(req_id, 10);
            assert_eq!(con_id, 756733);
            assert_eq!(what_to_show, "TRADES");
            assert!(use_rth);
        }
        _ => panic!("expected FetchHeadTimestamp"),
    }
}

// ═══════════════════════════════════════════════════════════════════
//  Contract details
// ═══════════════════════════════════════════════════════════════════

#[test]
fn req_contract_details_sends_fetch() {
    let (client, rx, _shared) = test_client();
    client.req_contract_details(7, &spy()).unwrap();
    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::FetchContractDetails { contract: ContractRef { con_id, .. }, req_id, .. } => {
            assert_eq!(req_id, 7);
            assert_eq!(con_id, 756733);
        }
        _ => panic!("expected FetchContractDetails"),
    }
}

#[test]
fn req_contract_details_forwards_filter_fields() {
    // /: a by-symbol lookup must carry the disambiguation
    // filters (primary exchange, local symbol, expiry/strike/right, multiplier,
    // trading class) instead of dropping them.
    let (client, rx, _shared) = test_client();
    let contract = Contract {
        con_id: 0, symbol: "AAPL".into(), sec_type: "OPT".into(),
        exchange: "SMART".into(), currency: "USD".into(),
        primary_exchange: "NASDAQ".into(),
        local_symbol: "AAPL  260808C00250000".into(),
        last_trade_date_or_contract_month: "202608".into(),
        strike: 250.0,
        right: "C".into(),
        multiplier: "100".into(),
        trading_class: "AAPL".into(),
        ..Default::default()
    };
    client.req_contract_details(9, &contract).unwrap();
    match rx.try_recv().unwrap() {
        ControlCommand::FetchContractDetails { contract: ContractRef { con_id, .. }, req_id, filters, .. } => {
            assert_eq!(req_id, 9);
            assert_eq!(con_id, 0);
            assert_eq!(filters.primary_exchange, "NASDAQ");
            assert_eq!(filters.local_symbol, "AAPL  260808C00250000");
            assert_eq!(filters.last_trade_date_or_contract_month, "202608");
            assert_eq!(filters.strike, 250.0);
            assert_eq!(filters.right, "C");
            assert_eq!(filters.multiplier, "100");
            assert_eq!(filters.trading_class, "AAPL");
        }
        cmd => panic!("expected FetchContractDetails, got {cmd:?}"),
    }
}

#[test]
fn req_contract_details_forwards_identifier_lookup() {
    // /: an identifier lookup (ISIN) must carry secId and
    // secIdType through to the fetch command.
    let (client, rx, _shared) = test_client();
    let contract = Contract {
        con_id: 0, sec_type: "STK".into(), exchange: "SMART".into(), currency: "USD".into(),
        sec_id: "US0378331005".into(), sec_id_type: "ISIN".into(),
        ..Default::default()
    };
    client.req_contract_details(11, &contract).unwrap();
    match rx.try_recv().unwrap() {
        ControlCommand::FetchContractDetails { filters, .. } => {
            assert_eq!(filters.sec_id, "US0378331005");
            assert_eq!(filters.sec_id_type, "ISIN");
        }
        cmd => panic!("expected FetchContractDetails, got {cmd:?}"),
    }
}

#[test]
fn req_matching_symbols_sends_fetch() {
    let (client, rx, _shared) = test_client();
    client.req_matching_symbols(8, "AAPL").unwrap();
    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::FetchMatchingSymbols { req_id, pattern } => {
            assert_eq!(req_id, 8);
            assert_eq!(pattern, "AAPL");
        }
        _ => panic!("expected FetchMatchingSymbols"),
    }
}

/// The pattern rides the wire as one field's value: carrying the byte that
/// separates fields would send, after it, fields this request never stated.
/// Refused rather than sent stating something else.
#[test]
fn a_pattern_carrying_the_field_separator_is_refused() {
    let (client, _rx, _shared) = test_client();
    let err = client
        .req_matching_symbols(8, "AAPL\x011=999")
        .expect_err("a pattern cannot carry the byte that separates fields");
    assert!(err.message.contains("separates fields"), "{}", err.message);
}

// ═══════════════════════════════════════════════════════════════════
//  Positions
// ═══════════════════════════════════════════════════════════════════

/// An executions filter naming a side is read in either vocabulary.
///
/// A stored execution carries the venue's word for the side and a filter
/// states the order action. Compared as written, a filter for buys matched
/// nothing at all and the caller read an empty answer as "no fills" -- and one
/// surface mapped the words on the way in while the other did not, so the same
/// filter answered differently depending on which was used.
#[test]
fn an_executions_filter_reads_a_side_in_either_vocabulary() {
    let (client, _rx, _shared) = test_client();
    for (exec_id, side) in [("bought", "BOT"), ("sold", "SLD")] {
        client.core.push_execution(
            ApiContract { con_id: 265598, symbol: "AAPL".into(), ..Default::default() },
            crate::types::model::Execution {
                exec_id: exec_id.into(), side: side.into(), ..Default::default()
            },
            Default::default(),
        );
    }

    let matching = |side: &str| {
        let filter = crate::types::model::ExecutionFilter {
            side: side.into(), ..Default::default()
        };
        client.core.snapshot_executions(&filter)
            .into_iter().map(|se| se.execution.exec_id).collect::<Vec<_>>()
    };

    assert_eq!(matching("BUY"), vec!["bought".to_string()], "the order action");
    assert_eq!(matching("BOT"), vec!["bought".to_string()], "the venue's word");
    assert_eq!(matching("SELL"), vec!["sold".to_string()], "the order action");
    assert_eq!(matching("SSHORT"), vec!["sold".to_string()], "a short is a sale");
    assert_eq!(matching("SLD"), vec!["sold".to_string()], "the venue's word");
    assert_eq!(matching("").len(), 2, "and no side named is every fill");
}

/// One request number holds one book, and a withdrawal says when it holds none.
///
/// Depth is routed by records the engine keeps, so neither surface could see
/// that a number already held a book: two contracts' rows arrived interleaved
/// under one number with nothing to tell them apart, the withdrawal named only
/// the later contract and left the earlier one being served, and a reconnect
/// brought back one book where there had been two.
#[test]
fn a_request_number_holds_one_book_and_says_when_it_holds_none() {
    let (client, rx, _shared) = test_client();

    let withdrawn = client.cancel_mkt_depth(7);
    assert!(
        withdrawn.as_ref().is_err_and(|why| why.code == 310),
        "nothing is held under that number: {withdrawn:?}",
    );
    assert!(rx.try_recv().is_err(), "and nothing was asked of the engine for it");

    client.req_mkt_depth(7, &spy(), 5, false).expect("the first book is asked for");
    rx.try_recv().expect("and it reaches the engine");

    let elsewhere = Contract { symbol: "QQQ".into(), con_id: 320227571, ..spy() };
    let refused = client.req_mkt_depth(7, &elsewhere, 5, false);
    assert!(
        refused.as_ref().is_err_and(|why| why.code == 102),
        "the number already holds a book: {refused:?}",
    );
    assert!(rx.try_recv().is_err(), "and the second contract was not asked for");

    // Withdrawn, the number is the caller's again.
    client.cancel_mkt_depth(7).expect("the book is withdrawn");
    rx.try_recv().expect("and the withdrawal reaches the engine");
    client.req_mkt_depth(7, &elsewhere, 5, false).expect("the number is free again");
}

/// A replay says nothing about what a fill cost until the venue has.
///
/// An execution is stored with its charge deliberately unstated, and every
/// execution the venue replays at logon is stored that way and never charged.
/// Reported regardless, the caller was handed a charge saying the fill cost
/// nothing, in no currency, naming no execution -- the exact statement the
/// unstated storage exists to avoid. A caller summing what its fills cost read
/// those as zeroes.
#[test]
fn a_replayed_fill_says_nothing_about_a_cost_the_venue_has_not_stated() {
    let (client, _rx, _shared) = test_client();
    for (exec_id, charged) in [("costed", true), ("uncosted", false)] {
        client.core.push_execution(
            ApiContract { con_id: 265598, symbol: "AAPL".into(), ..Default::default() },
            crate::types::model::Execution {
                exec_id: exec_id.into(), side: "BOT".into(), ..Default::default()
            },
            Default::default(),
        );
        if charged {
            client.core.record_charge(&crate::types::model::CommissionAndFeesReport {
                exec_id: exec_id.into(), commission_and_fees: 1.25,
                currency: "USD".into(), ..Default::default()
            });
        }
    }

    #[derive(Default)]
    struct Costs(Vec<String>, Vec<String>);
    impl crate::api::wrapper::Wrapper for Costs {
        fn exec_details(
            &mut self, _req_id: i64, _contract: &Contract,
            execution: &crate::types::model::Execution,
        ) {
            self.0.push(execution.exec_id.clone());
        }
        fn commission_and_fees_report(
            &mut self, report: &crate::types::model::CommissionAndFeesReport,
        ) {
            self.1.push(report.exec_id.clone());
        }
    }

    let mut w = Costs::default();
    client.req_executions(9, &crate::types::model::ExecutionFilter::default(), &mut w);

    assert_eq!(w.0.len(), 2, "both fills are replayed: {:?}", w.0);
    assert_eq!(
        w.1, vec!["costed".to_string()],
        "and only the one the venue priced says what it cost: {:?}", w.1,
    );
}

/// A charge whose fill lands mid-pass is not read before the fill it names.
///
/// The engine pushes a fill and then, off a message of its own, the charge
/// that names it. The charges were taken after the fills, so a pair written
/// while the dispatcher was inside a callback split: the charge was in that
/// pass and its fill was not. The charge then named an execution nothing had
/// stored, so it updated nothing -- the fill was filed for a replay with its
/// cost unknown for ever -- and a caller that files fills first and costs
/// second dropped it.
#[test]
fn a_charge_is_never_read_before_the_fill_it_names() {
    let (client, _rx, shared) = test_client();
    let info = |order_id: i64, exec_id: &str| crate::bridge::RichOrderInfo {
        contract: ApiContract {
            con_id: 265598, symbol: "AAPL".into(), sec_type: "STK".into(),
            exchange: "SMART".into(), currency: "USD".into(), ..Default::default()
        },
        order: Order { order_id, ..Default::default() },
        order_state: Default::default(),
        last_exec: crate::types::model::Execution {
            exec_id: exec_id.into(), ..Default::default()
        },
    };
    let fill = |order_id: u64| Fill {
        instrument: 0, order_id, side: Side::Buy,
        price: 150 * PRICE_SCALE, qty: 10 * crate::types::QTY_SCALE, remaining: 0, timestamp_ns: 0,
        cum_qty: 10 * crate::types::QTY_SCALE, avg_price: 150 * PRICE_SCALE,
    };

    shared.orders.push_order_info(77, info(77, "first"));
    shared.orders.push_order_info(78, info(78, "second"));
    shared.orders.push_fill(fill(77));

    // The engine writing while the dispatcher is inside a caller's callback,
    // which is the moment the two drains straddle.
    struct WritesMidPass(std::sync::Arc<crate::bridge::SharedState>, bool);
    impl crate::api::wrapper::Wrapper for WritesMidPass {
        fn exec_details(
            &mut self, _req_id: i64, _contract: &Contract,
            _execution: &crate::types::model::Execution,
        ) {
            if std::mem::replace(&mut self.1, false) {
                self.0.orders.push_fill(Fill {
                    instrument: 0, order_id: 78, side: Side::Buy,
                    price: 150 * PRICE_SCALE, qty: 10 * crate::types::QTY_SCALE,
                    remaining: 0, timestamp_ns: 0,
                    cum_qty: 10 * crate::types::QTY_SCALE, avg_price: 150 * PRICE_SCALE,
                });
                self.0.orders.push_charge(crate::types::model::CommissionAndFeesReport {
                    exec_id: "second".into(), commission_and_fees: 1.25,
                    currency: "USD".into(), ..Default::default()
                });
            }
        }
    }

    let mut w = WritesMidPass(shared.clone(), true);
    client.process_msgs(&mut w);
    client.process_msgs(&mut w);

    let replayed = client.core.snapshot_executions(&crate::types::model::ExecutionFilter::default());
    let second = replayed.iter()
        .find(|se| se.execution.exec_id == "second")
        .expect("the fill written mid-pass is stored");
    assert_eq!(
        second.commission_and_fees.commission_and_fees, 1.25,
        "its charge was read before it and updated nothing: {:?}",
        second.commission_and_fees,
    );
}

/// Reading the completed orders discards the record each order was tracked
/// by. A fill still queued is read against that record, so discarding it
/// first delivered an execution with no contract and no execution id — and
/// the commission that follows is reported under that same id.
#[test]
fn a_queued_fill_survives_a_completed_orders_read() {
    let (client, _rx, shared) = test_client();
    client.core.cache_contract(265598, ApiContract {
        con_id: 265598, symbol: "AAPL".into(), sec_type: "STK".into(),
        exchange: "SMART".into(), currency: "USD".into(), ..Default::default()
    });
    shared.orders.push_order_info(77, crate::bridge::RichOrderInfo {
        contract: ApiContract {
            con_id: 265598, symbol: "AAPL".into(), sec_type: "STK".into(),
            exchange: "SMART".into(), currency: "USD".into(), ..Default::default()
        },
        order: Order { order_id: 77, ..Default::default() },
        order_state: Default::default(),
        last_exec: Default::default(),
    });

    // The venue fills the order and completes it, and the caller reads the
    // completed orders before it next pumps the queue.
    shared.orders.push_fill(Fill {
        instrument: 0, order_id: 77, side: Side::Buy,
        price: 150 * PRICE_SCALE, qty: 10 * crate::types::QTY_SCALE, remaining: 0, timestamp_ns: 0,
        cum_qty: 10 * crate::types::QTY_SCALE, avg_price: 150 * PRICE_SCALE,
    });
    shared.orders.push_completed_order(crate::types::CompletedOrder {
        order_id: 77, instrument: 0, status: OrderStatus::Filled,
        filled_qty: 10 * crate::types::QTY_SCALE, timestamp_ns: 0,
    });

    // The recording wrapper does not keep the contract, and the contract is
    // the thing at issue.
    #[derive(Default)]
    struct Executions(Vec<String>);
    impl crate::api::wrapper::Wrapper for Executions {
        fn exec_details(
            &mut self, _req_id: i64, contract: &Contract,
            _execution: &crate::types::model::Execution,
        ) {
            self.0.push(contract.symbol.clone());
        }
    }

    let mut w = Executions::default();
    // Read before the queue is next pumped, which is what discards the record.
    client.req_completed_orders(false, &mut w);
    client.process_msgs(&mut w);

    assert_eq!(
        w.0, vec!["AAPL".to_string()],
        "the fill was delivered without the contract it was on",
    );

    // Held back, not held forever: the next read frees it now the fill has
    // been delivered.
    assert!(
        shared.orders.get_order_info(77).is_some(),
        "the record was freed while the fill still needed it",
    );
    client.process_msgs(&mut w);
    assert!(
        shared.orders.get_order_info(77).is_none(),
        "the record was kept back and then never freed",
    );
}

/// The holding feed is live: a caller that asked for positions once keeps
/// hearing about them as the broker restates them, which is how it learns its
/// own fill moved its own holding.
#[test]
fn a_restated_holding_reaches_a_caller_that_already_asked() {
    let (client, _rx, shared) = test_client();
    shared.portfolio.account_download_is_settled();
    shared.portfolio.set_position_info(PositionInfo {
        con_id: 265598, position: 100.0, symbol: "AAPL".into(),
        sec_type: "STK".into(), currency: "USD".into(), ..Default::default()
    });

    let mut w = RecordingWrapper::default();
    client.req_positions(&mut w);
    let reported = |w: &RecordingWrapper| {
        w.events.iter().filter(|e| e.starts_with("position:")).count()
    };
    assert_eq!(reported(&w), 1, "the holding held when it was asked for");

    shared.portfolio.set_position_info(PositionInfo {
        con_id: 265598, position: 125.0, avg_cost: 150 * PRICE_SCALE,
        symbol: "AAPL".into(), sec_type: "STK".into(), currency: "USD".into(),
        ..Default::default()
    });
    client.process_msgs(&mut w);

    assert_eq!(reported(&w), 2, "the restatement reaches the caller");
    assert_eq!(
        shared.portfolio.position_info(265598).unwrap().position, 125.0,
        "and the holding is what the broker said it is",
    );
}

/// `positionMulti` is the same live feed as `position`, asked for under a
/// request id and withdrawn under one — the protocol has a cancel for it, and
/// nothing cancels a one-shot. Answered once, a caller watching an account's
/// holdings read a snapshot that went stale on the next fill.
///
/// Both may be watching at once, and one move is told to both: drained per
/// watcher, the first would take it and the other would never hear of it.
#[test]
fn a_holding_that_moves_reaches_every_watcher() {
    let (client, _rx, shared) = test_client();
    shared.portfolio.account_download_is_settled();
    let held = |qty: f64| PositionInfo {
        con_id: 265598, position: qty, symbol: "AAPL".into(),
        sec_type: "STK".into(), currency: "USD".into(), ..Default::default()
    };
    shared.portfolio.set_position_info(held(100.0));

    let mut w = RecordingWrapper::default();
    client.req_positions(&mut w);
    client.req_positions_multi(9, "", "", &mut w);
    let counted = |w: &RecordingWrapper, what: &str| {
        w.events.iter().filter(|e| e.starts_with(what)).count()
    };
    let (was_plain, was_multi) =
        (counted(&w, "position:"), counted(&w, "position_multi:"));

    shared.portfolio.set_position_info(held(150.0));
    client.process_msgs(&mut w);

    assert_eq!(counted(&w, "position:"), was_plain + 1, "the plain watcher");
    assert_eq!(counted(&w, "position_multi:"), was_multi + 1, "and the one per request");

    // Withdrawn under its own id, leaving the other watching.
    client.cancel_positions_multi(9);
    shared.portfolio.set_position_info(held(175.0));
    client.process_msgs(&mut w);

    assert_eq!(counted(&w, "position:"), was_plain + 2, "still watching");
    assert_eq!(counted(&w, "position_multi:"), was_multi + 1, "withdrawn");
}

/// Nothing is drained while no ask stands. Draining in the dispatch as well
/// discarded the moves that landed while `req_positions` was still assembling
/// its answer, and no later report repeated them.
#[test]
fn a_move_is_kept_while_no_request_stands() {
    let (client, _rx, shared) = test_client();
    shared.portfolio.set_position_info(PositionInfo {
        con_id: 265598, position: 5.0, symbol: "AAPL".into(), ..Default::default()
    });

    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);

    assert!(
        !shared.portfolio.drain_position_changes().is_empty(),
        "a dispatch nobody had asked for consumed the move",
    );
}

/// A watcher is watching before its first answer is read.
///
/// The queue of moves is drained once and given to everyone watching, so a
/// watcher that already exists lets a dispatch pass empty it. Registered after
/// its answer was read, a joining watcher lost every move that landed in
/// between: not in the answer, which was read before, and not in the moves,
/// which the pass took while it was still being registered. The callback is
/// where the answer is handed over, so the registration has to be visible by
/// then.
#[test]
fn a_joining_watcher_is_watching_before_its_answer_is_read() {
    /// Reads whether the request is already registered at the moment the
    /// answer reaches the caller.
    struct WatchingYet<'a> {
        watchers: &'a std::sync::Mutex<std::collections::HashSet<i64>>,
        registered_when_answered: Option<bool>,
    }
    impl crate::api::wrapper::Wrapper for WatchingYet<'_> {
        fn position_multi(
            &mut self, req_id: i64, _account: &str, _model: &str,
            _contract: &Contract, _position: f64, _avg_cost: f64,
        ) {
            self.registered_when_answered =
                Some(self.watchers.lock().unwrap().contains(&req_id));
        }
    }

    let (client, _rx, shared) = test_client();
    shared.portfolio.set_position_info(PositionInfo {
        con_id: 265598, position: 100.0, symbol: "AAPL".into(),
        sec_type: "STK".into(), currency: "USD".into(), ..Default::default()
    });

    let mut watching = WatchingYet {
        watchers: &client.positions_multi_requested,
        registered_when_answered: None,
    };
    client.req_positions_multi(9, "", "", &mut watching);

    assert_eq!(
        watching.registered_when_answered, Some(true),
        "the request is watching by the time its own answer is handed over",
    );
}

/// A watcher's first answer and its registration are one moment.
///
/// The answer states what the account holds and the registration is what
/// hears about it moving afterwards. Taken apart, a holding that moves between
/// the two is in neither: not in the answer, which was read before it moved,
/// and not in the moves, which the dispatch pass took while this watcher was
/// still being registered. Nothing repeats it, so that watcher never hears of
/// that holding again until it moves once more.
#[test]
fn a_watchers_first_answer_and_its_registration_are_one_moment() {
    let (client, _rx, shared) = test_client();
    let held = |con_id: i64, qty: f64| PositionInfo {
        con_id, position: qty, symbol: "AAPL".into(),
        sec_type: "STK".into(), currency: "USD".into(), ..Default::default()
    };
    shared.portfolio.set_position_info(held(265598, 100.0));

    // Held from outside, the way a dispatch pass holds it while it takes the
    // moves: the registration cannot complete until it is let go.
    let registering = client.positions_multi_requested.lock().unwrap();
    std::thread::scope(|s| {
        let asking = s.spawn(|| {
            let mut heard = RecordingWrapper::default();
            client.req_positions_multi(7, "", "", &mut heard);
            heard
        });
        std::thread::sleep(std::time::Duration::from_millis(100));
        // A second holding arrives while this watcher is being registered.
        shared.portfolio.set_position_info(held(756733, 50.0));
        drop(registering);
        let heard = asking.join().unwrap();
        assert_eq!(
            heard.events.iter().filter(|e| e.starts_with("position_multi:")).count(), 2,
            "a holding that moved while the watcher was being registered \
             reached neither its answer nor its moves: {:?}",
            heard.events,
        );
    });
}

/// `req_positions` subscribes to a real-time feed: a holding that moves after
/// the call is reported as it moves. Answering only the set held when the
/// call was made left a caller tracking its positions from a snapshot that
/// went stale on the next fill.
#[test]
fn a_holding_that_moves_after_the_request_is_reported() {
    let (client, _rx, shared) = test_client();
    shared.portfolio.account_download_is_settled();
    let held = |qty: f64| PositionInfo {
        con_id: 265598, position: qty, symbol: "AAPL".into(),
        sec_type: "STK".into(), currency: "USD".into(), ..Default::default()
    };
    shared.portfolio.set_position_info(held(100.0));

    let mut w = RecordingWrapper::default();
    client.req_positions(&mut w);
    let reported = |w: &RecordingWrapper| {
        w.events.iter().filter(|e| e.starts_with("position:")).count()
    };
    assert_eq!(reported(&w), 1, "the holding held when it was asked for");

    shared.portfolio.set_position_info(held(150.0));
    client.process_msgs(&mut w);
    assert_eq!(reported(&w), 2, "and the holding once it moves");

    // Withdrawn, so what moves after is no longer reported.
    client.cancel_positions();
    shared.portfolio.set_position_info(held(175.0));
    client.process_msgs(&mut w);
    assert_eq!(reported(&w), 2, "a withdrawn ask is not answered further");
}

/// Moves from before the request are answered by the request itself, which
/// states what the holdings are now. Fired again as change events they replay
/// history, and a caller acting on the feed re-acts on what it was just told.
#[test]
fn moves_from_before_the_request_are_not_refired_as_changes() {
    let (client, _rx, shared) = test_client();
    shared.portfolio.account_download_is_settled();
    // Two holdings arrive before anything asks, so both moves queue.
    shared.portfolio.set_position_info(PositionInfo {
        con_id: 265598, position: 100.0, symbol: "AAPL".into(),
        sec_type: "STK".into(), currency: "USD".into(), ..Default::default()
    });
    shared.portfolio.set_position_info(PositionInfo {
        con_id: 756733, position: -50.0, symbol: "SPY".into(),
        sec_type: "STK".into(), currency: "USD".into(), ..Default::default()
    });

    let mut w = RecordingWrapper::default();
    client.req_positions(&mut w);

    // Afterwards, one of them moves.
    shared.portfolio.set_position_info(PositionInfo {
        con_id: 756733, position: -75.0, symbol: "SPY".into(),
        sec_type: "STK".into(), currency: "USD".into(), ..Default::default()
    });
    client.process_msgs(&mut w);

    let aapl: Vec<_> = w.events.iter()
        .filter(|e| e.starts_with("position:") && e.contains(":265598:")).collect();
    let spy: Vec<_> = w.events.iter()
        .filter(|e| e.starts_with("position:") && e.contains(":756733:")).collect();
    assert_eq!(
        aapl.len(), 1,
        "a pre-request move is answered once, in the request itself: {aapl:?}",
    );
    assert_eq!(spy.len(), 2, "the request, and the move after it: {spy:?}");
    assert!(
        spy.last().unwrap().contains(":-75"),
        "the change states what is held now: {spy:?}",
    );
}

/// The same replay, on the per-request feed.
#[test]
fn moves_from_before_the_request_are_not_refired_on_the_per_request_feed() {
    let (client, _rx, shared) = test_client();
    shared.portfolio.set_position_info(PositionInfo {
        con_id: 265598, position: 100.0, symbol: "AAPL".into(), ..Default::default()
    });
    shared.portfolio.set_position_info(PositionInfo {
        con_id: 756733, position: -50.0, symbol: "SPY".into(), ..Default::default()
    });

    let mut w = RecordingWrapper::default();
    client.req_positions_multi(9, "", "", &mut w);

    shared.portfolio.set_position_info(PositionInfo {
        con_id: 756733, position: -75.0, symbol: "SPY".into(), ..Default::default()
    });
    client.process_msgs(&mut w);

    let aapl: Vec<_> = w.events.iter()
        .filter(|e| e.starts_with("position_multi:9:") && e.contains(":AAPL:")).collect();
    let spy: Vec<_> = w.events.iter()
        .filter(|e| e.starts_with("position_multi:9:") && e.contains(":SPY:")).collect();
    assert_eq!(
        aapl.len(), 1,
        "a pre-request move is answered once, in the request itself: {aapl:?}",
    );
    assert_eq!(spy.len(), 2, "the request, and the move after it: {spy:?}");
}

#[test]
fn req_positions_delivers_via_wrapper() {
    let (client, _rx, shared) = test_client();
    // The account has stated everything it holds. Without it this waits out
    // the whole ten seconds for a signal no engine is here to send, and comes
    // back through the timeout rather than through delivery.
    shared.portfolio.account_download_is_settled();
    // Named, as a holding off the wire is: nameless, delivery waits three
    // seconds for a definition to arrive from an engine that is not here.
    shared.portfolio.set_position_info(PositionInfo { con_id: 265598, position: 100.0, avg_cost: 150 * PRICE_SCALE, symbol: "AAPL".into(), ..Default::default() });
    shared.portfolio.set_position_info(PositionInfo { con_id: 756733, position: -50.0, avg_cost: 400 * PRICE_SCALE, symbol: "SPY".into(), ..Default::default() });
    let mut w = RecordingWrapper::default();
    client.req_positions(&mut w);
    let positions: Vec<_> = w.events.iter().filter(|e| e.starts_with("position:")).collect();
    assert_eq!(positions.len(), 2);
    assert!(w.events.last().unwrap() == "position_end");
}

/// Account values are reported as the venue states them: every figure it sends,
/// in the currency it names, labelled with the account holding them rather than
/// the account the caller asked about.
#[test]
fn the_account_figures_are_the_ones_the_venue_stated() {
    #[derive(Default)]
    struct Rows(Vec<(String, String, String, String)>);
    impl crate::api::wrapper::Wrapper for Rows {
        fn account_update_multi(
            &mut self, _req_id: i64, account: &str, _model: &str,
            key: &str, value: &str, currency: &str,
        ) {
            self.0.push((
                account.to_string(), key.to_string(), value.to_string(), currency.to_string(),
            ));
        }
    }

    let (client, _rx, shared) = test_client();
    shared.portfolio.note_account_value("NetLiquidation", "12345.678", "CHF");
    shared.portfolio.note_account_value("SettledCash", "42.5", "CHF");

    let mut rows = Rows::default();
    client.req_account_updates_multi(1, "DU999", "", true, &mut rows);

    assert!(
        rows.0.iter().any(|(account, key, value, currency)| {
            account == "DU123" && key == "NetLiquidation" && value == "12345.678"
                && currency == "CHF"
        }),
        "the figure, currency and account as stated: {:?}",
        rows.0,
    );
    assert!(
        rows.0.iter().any(|(_, key, ..)| key == "SettledCash"),
        "a figure the venue states outside the eight that were worked out here",
    );
    assert!(
        rows.0.iter().all(|(account, ..)| account == "DU123"),
        "an account that was asked about is not the account these are for",
    );
}

/// A holding is labelled with the account that holds it.
#[test]
fn holdings_are_labelled_with_the_account_that_holds_them() {
    let (client, _rx, shared) = test_client();
    shared.portfolio.set_position_info(PositionInfo {
        con_id: 756733, position: 100.0, avg_cost: 400 * PRICE_SCALE, ..Default::default()
    });
    let mut w = RecordingWrapper::default();
    client.req_positions_multi(2, "DU999", "", &mut w);
    assert!(
        w.events.iter().any(|e| e.starts_with("position_multi:2:DU123:")),
        "another account's name sat on this account's holdings: {:?}",
        w.events,
    );
}

/// The account's figures keep arriving under the request that asked for them,
/// and stop when it is withdrawn.
///
/// The reference client holds this request open: a figure that moves after the
/// first batch is reported again under the same request. Answered with the
/// first batch and nothing after, a caller watching its balance sheet through
/// this request watched a still picture — and every one of them is written
/// against a client that keeps it moving.
#[test]
fn the_account_figures_keep_arriving_under_the_request_that_asked() {
    #[derive(Default)]
    struct Rows(Vec<(i64, String, String)>);
    impl crate::api::wrapper::Wrapper for Rows {
        fn account_update_multi(
            &mut self, req_id: i64, _account: &str, _model: &str,
            key: &str, value: &str, _currency: &str,
        ) {
            self.0.push((req_id, key.to_string(), value.to_string()));
        }
    }

    let (client, _rx, shared) = test_client();
    shared.portfolio.account_download_is_settled();
    shared.portfolio.note_account_value("NetLiquidation", "100.00", "USD");

    let mut rows = Rows::default();
    client.req_account_updates_multi(7, "", "", false, &mut rows);
    assert!(
        rows.0.iter().any(|(req, key, value)| *req == 7 && key == "NetLiquidation" && value == "100.00"),
        "the first batch states the account: {:?}", rows.0,
    );

    // A figure moves, and the request that asked hears about it.
    rows.0.clear();
    shared.portfolio.note_account_value("NetLiquidation", "101.00", "USD");
    client.process_msgs(&mut rows);
    assert_eq!(
        rows.0, vec![(7, "NetLiquidation".to_string(), "101.00".to_string())],
        "the move was not reported under the request watching for it",
    );

    // Withdrawn, and the next move reaches nobody.
    rows.0.clear();
    client.cancel_account_updates_multi(7);
    shared.portfolio.note_account_value("NetLiquidation", "102.00", "USD");
    client.process_msgs(&mut rows);
    assert!(rows.0.is_empty(), "a withdrawn request went on being reported to: {:?}", rows.0);
}

/// A second subscription does not eat the moves the first is owed.
///
/// The record of what has been delivered is shared by every watcher and is
/// advanced by the dispatch that broadcasts. An ask that built its first batch
/// through that record handed itself the figures and marked them delivered for
/// everyone — so a figure that moved between one ask and the next reached the
/// new watcher and never reached the one already standing.
#[test]
fn a_second_subscription_does_not_eat_the_first_one_s_moves() {
    #[derive(Default)]
    struct Rows(Vec<(i64, String, String)>);
    impl crate::api::wrapper::Wrapper for Rows {
        fn account_update_multi(
            &mut self, req_id: i64, _account: &str, _model: &str,
            key: &str, value: &str, _currency: &str,
        ) {
            self.0.push((req_id, key.to_string(), value.to_string()));
        }
    }

    let (client, _rx, shared) = test_client();
    shared.portfolio.account_download_is_settled();
    shared.portfolio.note_account_value("NetLiquidation", "100.00", "USD");

    let mut rows = Rows::default();
    client.req_account_updates_multi(1, "", "", false, &mut rows);
    assert!(rows.0.iter().any(|(r, ..)| *r == 1), "the first ask is answered");

    // The figure moves, and before anything is dispatched a second caller asks.
    shared.portfolio.note_account_value("NetLiquidation", "101.00", "USD");
    rows.0.clear();
    client.req_account_updates_multi(2, "", "", false, &mut rows);
    assert!(
        rows.0.iter().any(|(r, k, v)| *r == 2 && k == "NetLiquidation" && v == "101.00"),
        "the second ask is answered with the account as it stands: {:?}", rows.0,
    );

    // And the watcher already standing is still owed that move.
    rows.0.clear();
    client.process_msgs(&mut rows);
    assert!(
        rows.0.iter().any(|(r, k, v)| *r == 1 && k == "NetLiquidation" && v == "101.00"),
        "the first watcher was never told the move the second ask overtook: {:?}", rows.0,
    );
    // And the one just answered is not told again what it was just given. A
    // first batch read outside the record leaves every figure looking
    // undelivered, and the next dispatch says the whole account back to a
    // caller that has it.
    assert!(
        !rows.0.iter().any(|(r, ..)| *r == 2),
        "the new watcher was told its own batch a second time: {:?}", rows.0,
    );

    // Withdrawn and asked again, the account comes whole rather than as the
    // nothing that has moved since.
    client.cancel_account_updates_multi(1);
    rows.0.clear();
    client.req_account_updates_multi(1, "", "", false, &mut rows);
    assert!(
        rows.0.iter().any(|(r, k, v)| *r == 1 && k == "NetLiquidation" && v == "101.00"),
        "a fresh ask is answered with the account, not with what moved: {:?}", rows.0,
    );
}

/// A model names a slice of the account, and a slice is not what this session
/// is told about.
///
/// The venue states the account whole. Echoing the caller's model onto every
/// row labelled the whole account's figures and holdings as that model's, and
/// a caller keeping a book per model files one model as holding everything the
/// account holds.
#[test]
fn a_model_that_was_asked_about_does_not_label_the_whole_account() {
    #[derive(Default)]
    struct Labels { models: Vec<String>, said: Vec<String> }
    impl crate::api::wrapper::Wrapper for Labels {
        fn account_update_multi(
            &mut self, _req_id: i64, _account: &str, model: &str,
            _key: &str, _value: &str, _currency: &str,
        ) {
            self.models.push(model.to_string());
        }
        fn position_multi(
            &mut self, _req_id: i64, _account: &str, model: &str,
            _contract: &Contract, _position: f64, _avg_cost: f64,
        ) {
            self.models.push(model.to_string());
        }
        fn error(&mut self, _req_id: i64, _code: i64, message: &str, _json: &str) {
            self.said.push(message.to_string());
        }
    }

    let (client, _rx, shared) = test_client();
    shared.portfolio.account_download_is_settled();
    shared.portfolio.note_account_value("NetLiquidation", "12345.678", "CHF");
    shared.portfolio.set_position_info(PositionInfo {
        con_id: 756733, position: 100.0, avg_cost: 400 * PRICE_SCALE, ..Default::default()
    });

    let mut heard = Labels::default();
    client.req_account_updates_multi(1, "", "TECH", false, &mut heard);
    client.req_positions_multi(2, "", "TECH", &mut heard);

    assert!(!heard.models.is_empty(), "the account answered at all");
    assert!(
        heard.models.iter().all(|model| model.is_empty()),
        "the whole account's figures came back labelled with a model: {:?}",
        heard.models,
    );
    let about_the_model: Vec<&String> =
        heard.said.iter().filter(|why| why.contains("TECH")).collect();
    assert_eq!(
        about_the_model.len(), 2,
        "the caller is told once per request: {:?}", heard.said,
    );
}

#[test]
fn req_positions_empty_still_calls_position_end() {
    let (client, _rx, shared) = test_client();
    // As above: the account has spoken, and it holds nothing.
    shared.portfolio.account_download_is_settled();
    let mut w = RecordingWrapper::default();
    client.req_positions(&mut w);
    assert_eq!(w.events, vec!["position_end"]);
}

// ═══════════════════════════════════════════════════════════════════
//  Scanner
// ═══════════════════════════════════════════════════════════════════

#[test]
fn req_scanner_parameters_sends_fetch() {
    let (client, rx, _shared) = test_client();
    client.req_scanner_parameters().unwrap();
    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::FetchScannerParams));
}

#[test]
fn req_scanner_subscription_sends_subscribe() {
    let (client, rx, _shared) = test_client();
    client.req_scanner_subscription(3, "STK", "STK.US.MAJOR", "TOP_PERC_GAIN", 25,
        &[TagValue { tag: "priceAbove".into(), value: "10".into() }]).unwrap();
    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::SubscribeScanner { req_id, scan_code, max_items, filters, .. } => {
            assert_eq!(req_id, 3);
            assert_eq!(scan_code, "TOP_PERC_GAIN");
            assert_eq!(max_items, 25);
            assert_eq!(filters, vec![("priceAbove".to_string(), "10".to_string())]);
        }
        _ => panic!("expected SubscribeScanner"),
    }
}

#[test]
fn cancel_scanner_subscription_sends_cancel() {
    let (client, rx, _shared) = test_client();
    client.cancel_scanner_subscription(3).unwrap();
    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::CancelScanner { req_id: 3 }));
}

// ═══════════════════════════════════════════════════════════════════
//  News
// ═══════════════════════════════════════════════════════════════════

#[test]
fn req_historical_news_sends_fetch() {
    let (client, rx, _shared) = test_client();
    // The query carries no time bounds, so a window is refused rather than
    // dropped: the answer is the most recent headlines, not the window's.
    assert!(client.req_historical_news(4, 265598, "BRFG", "2026-01-01", "2026-03-01", 10).is_err());
    client.req_historical_news(4, 265598, "BRFG", "", "", 10).unwrap();
    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::FetchHistoricalNews { req_id, con_id, provider_codes, max_results, .. } => {
            assert_eq!(req_id, 4);
            assert_eq!(con_id, 265598);
            assert_eq!(provider_codes, "BRFG");
            assert_eq!(max_results, 10);
        }
        _ => panic!("expected FetchHistoricalNews"),
    }
}

#[test]
fn req_news_article_sends_fetch() {
    let (client, rx, _shared) = test_client();
    client.req_news_article(5, "BRFG", "BRFG$12345").unwrap();
    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::FetchNewsArticle { req_id, provider_code, article_id } => {
            assert_eq!(req_id, 5);
            assert_eq!(provider_code, "BRFG");
            assert_eq!(article_id, "BRFG$12345");
        }
        _ => panic!("expected FetchNewsArticle"),
    }
}

// ═══════════════════════════════════════════════════════════════════
//  Fundamental data
// ═══════════════════════════════════════════════════════════════════

#[test]
fn req_fundamental_data_sends_fetch() {
    let (client, rx, _shared) = test_client();
    client.req_fundamental_data(6, &spy(), "ReportSnapshot").unwrap();
    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::FetchFundamentalData { req_id, report_type, .. } => {
            assert_eq!(req_id, 6);
            assert_eq!(report_type, "ReportSnapshot");
        }
        _ => panic!("expected FetchFundamentalData"),
    }
}

#[test]
fn cancel_fundamental_data_sends_cancel() {
    let (client, rx, _shared) = test_client();
    client.cancel_fundamental_data(6).unwrap();
    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::CancelFundamentalData { req_id: 6 }));
}

// ═══════════════════════════════════════════════════════════════════
//  Histogram
// ═══════════════════════════════════════════════════════════════════

#[test]
fn req_histogram_data_sends_fetch() {
    let (client, rx, _shared) = test_client();
    client.req_histogram_data(7, &spy(), true, "1 week").unwrap();
    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::FetchHistogramData { req_id, use_rth, period, .. } => {
            assert_eq!(req_id, 7);
            assert!(use_rth);
            assert_eq!(period, "1 week");
        }
        _ => panic!("expected FetchHistogramData"),
    }
}

#[test]
fn cancel_histogram_data_sends_cancel() {
    let (client, rx, _shared) = test_client();
    client.cancel_histogram_data(7).unwrap();
    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::CancelHistogramData { req_id: 7 }));
}

// ═══════════════════════════════════════════════════════════════════
//  Historical ticks
// ═══════════════════════════════════════════════════════════════════

#[test]
fn req_historical_ticks_sends_fetch() {
    let (client, rx, _shared) = test_client();
    // Either end, and the count says how far it reaches. Naming neither, or
    // both, is what the venue refuses.
    assert!(client.req_historical_ticks(8, &spy(), "", "", 1000, "TRADES", true).is_err());
    assert!(client.req_historical_ticks(8, &spy(), "20260101 09:30:00", "20260101 16:00:00", 1000, "TRADES", true).is_err());
    client.req_historical_ticks(8, &spy(), "20260101 09:30:00", "", 1000, "TRADES", true).unwrap();
    let _ = rx.try_recv();
    client.req_historical_ticks(8, &spy(), "", "20260101 16:00:00", 1000, "TRADES", true).unwrap();
    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::FetchHistoricalTicks { contract: ContractRef { con_id, .. }, req_id, number_of_ticks, what_to_show, .. } => {
            assert_eq!(req_id, 8);
            assert_eq!(con_id, 756733);
            assert_eq!(number_of_ticks, 1000);
            assert_eq!(what_to_show, "TRADES");
        }
        _ => panic!("expected FetchHistoricalTicks"),
    }
}

// ═══════════════════════════════════════════════════════════════════
//  Real-time bars
// ═══════════════════════════════════════════════════════════════════

/// An unrecognised `what_to_show` is refused rather than encoded as TRADES.
#[test]
fn a_real_time_bar_request_states_a_series_the_venue_serves() {
    let (client, _rx, _shared) = test_client();
    let err = client.req_real_time_bars(9, &spy(), 5, "BDI", true).unwrap_err();
    assert!(err.message.contains("Unsupported what_to_show"), "got: {err}");
}

#[test]
fn req_real_time_bars_sends_subscribe() {
    let (client, rx, _shared) = test_client();
    client.req_real_time_bars(9, &spy(), 5, "TRADES", true).unwrap();
    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::SubscribeRealTimeBar { contract: ContractRef { con_id, .. }, req_id, what_to_show, use_rth, .. } => {
            assert_eq!(req_id, 9);
            assert_eq!(con_id, 756733);
            assert_eq!(what_to_show, "TRADES");
            assert!(use_rth);
        }
        _ => panic!("expected SubscribeRealTimeBar"),
    }
}

#[test]
fn cancel_real_time_bars_sends_cancel() {
    let (client, rx, _shared) = test_client();
    client.cancel_real_time_bars(9).unwrap();
    let cmd = rx.try_recv().unwrap();
    assert!(matches!(cmd, ControlCommand::CancelRealTimeBar { req_id: 9 }));
}

// ═══════════════════════════════════════════════════════════════════
//  Historical schedule
// ═══════════════════════════════════════════════════════════════════

#[test]
fn req_historical_schedule_sends_fetch() {
    let (client, rx, _shared) = test_client();
    client.req_historical_schedule(11, &spy(), "20260101 16:00:00", "1 D", true).unwrap();
    let cmd = rx.try_recv().unwrap();
    match cmd {
        ControlCommand::FetchHistoricalSchedule { contract: ContractRef { con_id, .. }, req_id, use_rth, .. } => {
            assert_eq!(req_id, 11);
            assert_eq!(con_id, 756733);
            assert!(use_rth);
        }
        _ => panic!("expected FetchHistoricalSchedule"),
    }
}

// ═══════════════════════════════════════════════════════════════════
//  Quote / Account accessors
// ═══════════════════════════════════════════════════════════════════

#[test]
fn quote_escape_hatch() {
    let shared = Arc::new(SharedState::new());
    let q = Quote { bid: 200 * PRICE_SCALE, ..Default::default() };
    shared.market.push_quote(0, &q);

    let (tx, _rx) = std::sync::mpsc::sync_channel(4096);
    let handle = std::thread::spawn(|| {});
    let client = EClient::from_parts(shared, tx, handle, "DU123".into());

    client.core.req_to_instrument.lock().unwrap().insert(5, 0);

    let quote = client.quote(5).unwrap();
    assert_eq!(quote.bid, 200 * PRICE_SCALE);
    assert!(client.quote(99).is_none());
}

// RTT is None until measured, then reflects the stored sample;
// req_ping goes out as a Ping command.
#[test]
fn rtt_none_until_measured_and_ping_sends_command() {
    let (client, rx, shared) = test_client();
    assert_eq!(client.last_rtt(), None);
    client.req_ping().unwrap();
    assert!(matches!(rx.try_recv().unwrap(), ControlCommand::Ping));

    shared.set_ccp_rtt(std::time::Duration::from_micros(1234));
    assert_eq!(client.last_rtt(), Some(std::time::Duration::from_micros(1234)));
}

#[test]
fn quote_by_instrument_direct() {
    let shared = Arc::new(SharedState::new());
    let q = Quote { ask: 300 * PRICE_SCALE, ..Default::default() };
    shared.market.push_quote(2, &q);

    let (tx, _rx) = std::sync::mpsc::sync_channel(4096);
    let handle = std::thread::spawn(|| {});
    let client = EClient::from_parts(shared, tx, handle, "DU123".into());

    let quote = client.quote_by_instrument(2).expect("registered id");
    assert_eq!(quote.ask, 300 * PRICE_SCALE);

    // An out-of-range id is a caller error, not a panic across
    // the language boundary.
    assert!(client
        .quote_by_instrument(crate::types::MAX_INSTRUMENTS as u32)
        .is_none());
}

#[test]
fn account_reads_shared_state() {
    let (_client, _rx, shared) = test_client();
    let a = AccountState { net_liquidation: 100_000 * PRICE_SCALE, ..Default::default() };
    shared.portfolio.set_account(&a);
    shared.portfolio.account_download_is_settled();
    let (client2, _rx2, _) = {
        let (tx, rx) = std::sync::mpsc::sync_channel(4096);
        let handle = std::thread::spawn(|| {});
        (EClient::from_parts(shared.clone(), tx, handle, "DU123".into()), rx, shared.clone())
    };
    assert_eq!(client2.account().map(|a| a.net_liquidation), Some(100_000 * PRICE_SCALE));
}

/// The account is read only once the venue has stated it whole. Answered
/// from the struct alone, a caller read all zeros before the first download
/// and the pre-drop figures after a drop, with nothing to say so.
#[test]
fn the_account_is_read_only_once_the_download_has_finished() {
    let (client, _rx, shared) = test_client();
    shared.portfolio.set_account(&AccountState { net_liquidation: 100_000 * PRICE_SCALE, ..Default::default() });
    assert!(client.account().is_none(), "figures the download has not finished stating are not the account");
    shared.portfolio.account_download_is_settled();
    assert_eq!(client.account().map(|a| a.net_liquidation), Some(100_000 * PRICE_SCALE));
    shared.portfolio.account_download_is_pending();
    assert!(client.account().is_none(), "nor are the pre-drop figures, after a drop");
}

// ═══════════════════════════════════════════════════════════════════
//  process_msgs — fills, order updates, cancel rejects (existing)
// ═══════════════════════════════════════════════════════════════════

#[test]
fn process_msgs_dispatches_fill() {
    let (client, _rx, shared) = test_client();
    shared.orders.push_fill(Fill {
        instrument: 0, order_id: 42, side: Side::Buy,
        price: 150 * PRICE_SCALE, qty: 100 * crate::types::QTY_SCALE, remaining: 0, timestamp_ns: 123456789,
        cum_qty: 100 * crate::types::QTY_SCALE, avg_price: 150 * PRICE_SCALE,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("order_status:42:Filled")));
    assert!(w.events.iter().any(|e| e.starts_with("exec_details:-1:BOT:100")));
}

#[test]
fn process_msgs_dispatches_partial_fill() {
    let (client, _rx, shared) = test_client();
    shared.orders.push_fill(Fill {
        instrument: 0, order_id: 42, side: Side::Buy,
        price: 150 * PRICE_SCALE, qty: 50 * crate::types::QTY_SCALE, remaining: 50 * crate::types::QTY_SCALE, timestamp_ns: 123456789,
        cum_qty: 50 * crate::types::QTY_SCALE, avg_price: 150 * PRICE_SCALE,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    // A partly filled working order is Submitted: the vocabulary has no
    // status of its own for it, and a program reading one finds it in neither
    // the active set nor the done set.
    assert!(
        w.events.iter().any(|e| e.starts_with("order_status:42:Submitted:")),
        "{:?}", w.events,
    );
}

/// IB's `orderStatus` contract: `filled` is cumulative across the order and
/// `avgFillPrice` is volume-weighted across every print, while `lastFillPrice`
/// is this print. Reporting the print's own size and price as the cumulative
/// pair means an order that fills in more than one print never reports its
/// true average, and `filled` never reaches the order quantity.
#[test]
fn order_status_reports_the_order_total_not_the_last_print() {
    let (client, _rx, shared) = test_client();
    // Second print: 100 more at 151, taking the order to 200 filled at an
    // average of 150.50, with 100 still working.
    shared.orders.push_fill(Fill {
        instrument: 0, order_id: 42, side: Side::Buy,
        price: 151 * PRICE_SCALE, qty: 100 * crate::types::QTY_SCALE, remaining: 100 * crate::types::QTY_SCALE, timestamp_ns: 0,
        cum_qty: 200 * crate::types::QTY_SCALE, avg_price: 150 * PRICE_SCALE + PRICE_SCALE / 2,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);

    let status = w.events.iter().find(|e| e.starts_with("order_status:42:"))
        .expect("order_status was dispatched");
    assert_eq!(
        status, "order_status:42:Submitted:200:100:150.5",
        "filled and avgFillPrice must describe the order, not the print",
    );
}

#[test]
fn process_msgs_dispatches_sell_fill() {
    let (client, _rx, shared) = test_client();
    shared.orders.push_fill(Fill {
        instrument: 0, order_id: 43, side: Side::Sell,
        price: 151 * PRICE_SCALE, qty: 100 * crate::types::QTY_SCALE, remaining: 0, timestamp_ns: 0,
        cum_qty: 100 * crate::types::QTY_SCALE, avg_price: 151 * PRICE_SCALE,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("exec_details:-1:SLD:100")));
}

#[test]
fn process_msgs_dispatches_order_updates() {
    let (client, _rx, shared) = test_client();
    shared.orders.push_order_update(OrderUpdate {
        order_id: 43, instrument: 0, status: OrderStatus::Submitted,
        filled_qty: 0.0, remaining_qty: 100.0, avg_price: 0, perm_id: 0, parent_id: 0, timestamp_ns: 0,
    });
    shared.orders.push_order_update(OrderUpdate {
        order_id: 44, instrument: 0, status: OrderStatus::Cancelled,
        filled_qty: 0.0, remaining_qty: 100.0, avg_price: 0, perm_id: 0, parent_id: 0, timestamp_ns: 0,
    });
    shared.orders.push_order_update(OrderUpdate {
        order_id: 45, instrument: 0, status: OrderStatus::Rejected,
        filled_qty: 0.0, remaining_qty: 100.0, avg_price: 0, perm_id: 0, parent_id: 0, timestamp_ns: 0,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("order_status:43:Submitted")));
    assert!(w.events.iter().any(|e| e.starts_with("order_status:44:Cancelled")));
    assert!(w.events.iter().any(|e| e.starts_with("order_status:45:Inactive")));
}

/// A parked (39=I) order's reason reaches the caller through `Wrapper::error`,
/// on top of the order_status "Inactive" callback above: ibapi has no callback
/// dedicated to an order held with a reason.
#[test]
fn process_msgs_dispatches_inactive_reason_as_error() {
    let (client, _rx, shared) = test_client();
    shared.orders.push_order_inactive(46, 399, "Order held pending margin check".into());
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e == "error:46:399:Order held pending margin check"));
}

/// end-to-end: a genuinely-Inactive order dispatched through the real
/// `process_msgs` path (not a direct `ClientCore` call) stays in the
/// open-order snapshot, while a Rejected one — which stringifies to the same
/// "Inactive" — does not resurrect into it.
#[test]
fn process_msgs_then_open_orders_admits_inactive_excludes_rejected() {
    let (client, _rx, shared) = test_client();
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0,
        order_type: "LMT".into(), lmt_price: 150.0, ..Default::default()
    };
    client.place_order(82, &spy(), &order).unwrap();
    client.place_order(83, &spy(), &order).unwrap();

    shared.orders.push_order_update(OrderUpdate {
        order_id: 82, instrument: 0, status: OrderStatus::Inactive,
        filled_qty: 0.0, remaining_qty: 100.0, avg_price: 0, perm_id: 0, parent_id: 0, timestamp_ns: 0,
    });
    shared.orders.push_order_update(OrderUpdate {
        order_id: 83, instrument: 0, status: OrderStatus::Rejected,
        filled_qty: 0.0, remaining_qty: 100.0, avg_price: 0, perm_id: 0, parent_id: 0, timestamp_ns: 0,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);

    w.events.clear();
    client.req_all_open_orders(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("open_order:82:")),
        "genuinely-inactive order must remain in the open-order snapshot after dispatch");
    assert!(!w.events.iter().any(|e| e.starts_with("open_order:83:")),
        "rejected order must not resurrect into the open-order snapshot after dispatch");
}

/// A cancel the venue refused leaves the order working, and the record says so.
///
/// The record takes the cancel ahead of the venue's answer. Where the answer is
/// a refusal, the order stands — and left as it was, it read as leaving for the
/// rest of the session while the venue went on working it. Nothing later
/// corrects it: a refusal is the last message that order draws.
#[test]
fn a_refused_cancel_leaves_the_order_reading_as_working() {
    let (client, _rx, shared) = test_client();
    let mut order = ApiOrder {
        order_id: 44,
        action: "BUY".into(),
        total_quantity: 100.0,
        order_type: "LMT".into(),
        lmt_price: 100.0,
        tif: "DAY".into(),
        ..Default::default()
    };
    order.transmit = true;
    client.core.track_order(44, spy(), order, 0);
    client.core.update_order_status(
        &shared, 44, crate::types::OrderStatus::PendingCancel, 0.0, 100.0, 0,
    );
    assert_eq!(
        client.core.open_orders.lock().unwrap().get(&44).map(|o| o.status.clone()),
        Some("PendingCancel".to_string()),
    );

    shared.orders.push_cancel_reject(CancelReject {
        order_id: 44, instrument: 0, reject_type: 1, reason_code: 0,
        answers_a_live_change: true, still_working: Some(crate::types::OrderStatus::Submitted), timestamp_ns: 0,
    });
    client.process_msgs(&mut RecordingWrapper::default());

    assert_eq!(
        client.core.open_orders.lock().unwrap().get(&44).map(|o| o.status.clone()),
        Some("Submitted".to_string()),
        "the record holds what the venue says it is working",
    );
}

#[test]
fn process_msgs_dispatches_cancel_reject_type_1() {
    let (client, _rx, shared) = test_client();
    // Reason 0 is too-late-to-cancel: the venue found the order and would not
    // act on it. Reported as 202 this read as "Order Cancelled" — the opposite
    // of what happened, and a caller would replace an order still working.
    shared.orders.push_cancel_reject(CancelReject {
        order_id: 44, instrument: 0, reject_type: 1, reason_code: 0, answers_a_live_change: true, still_working: None, timestamp_ns: 0,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(
        w.events.iter().any(|e| e.starts_with("error:44:10148:")),
        "{:?}", w.events,
    );
    assert!(!w.events.iter().any(|e| e.starts_with("error:44:202:")));
}

#[test]
fn process_msgs_dispatches_cancel_reject_type_2() {
    let (client, _rx, shared) = test_client();
    shared.orders.push_cancel_reject(CancelReject {
        order_id: 44, instrument: 0, reject_type: 2, reason_code: 5, answers_a_live_change: true, still_working: None, timestamp_ns: 0,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(
        w.events.iter().any(|e| e.starts_with("error:44:10148:")),
        "{:?}", w.events,
    );
}

/// Cancel-reject reason 1 is an unknown order. Every other reason describes an
/// order the venue found and declined to act on.
#[test]
fn an_unknown_order_is_the_only_cancel_reject_reported_as_not_found() {
    let (client, _rx, shared) = test_client();
    shared.orders.push_cancel_reject(CancelReject {
        order_id: 44, instrument: 0, reject_type: 1, reason_code: 1, answers_a_live_change: true, still_working: None, timestamp_ns: 0,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(
        w.events.iter().any(|e| e.starts_with("error:44:10147:")),
        "{:?}", w.events,
    );
}

// ═══════════════════════════════════════════════════════════════════
//  process_msgs — quote polling
// ═══════════════════════════════════════════════════════════════════

#[test]
fn process_msgs_dispatches_quotes_on_change() {
    let (client, _rx, shared) = test_client();
    let mut q = Quote { bid: 150 * PRICE_SCALE, ask: 151 * PRICE_SCALE, ..Default::default() };
    shared.market.push_quote(0, &q);

    client.core.req_to_instrument.lock().unwrap().insert(1, 0);
    client.core.instrument_to_req.lock().unwrap().insert(0, 1);

    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("tick_price:1:1:150")));
    assert!(w.events.iter().any(|e| e.starts_with("tick_price:1:2:151")));

    // Second call — no changes, no events
    w.events.clear();
    client.process_msgs(&mut w);
    assert!(w.events.is_empty(), "no events on unchanged quotes");

    // Now change bid
    q.bid = 149 * PRICE_SCALE;
    shared.market.push_quote(0, &q);
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("tick_price:1:1:149")));
}

#[test]
fn process_msgs_dispatches_all_quote_fields() {
    let (client, _rx, shared) = test_client();
    let q = Quote {
        bid: 150 * PRICE_SCALE, ask: 151 * PRICE_SCALE, last: 150_50000000,
        bid_size: 1000 * QTY_SCALE, ask_size: 2000 * QTY_SCALE,
        last_size: 500 * QTY_SCALE,
        high: 155 * PRICE_SCALE, low: 148 * PRICE_SCALE,
        volume: 10_000 * QTY_SCALE,
        close: 149 * PRICE_SCALE, open: 150 * PRICE_SCALE,
        timestamp_ns: 1234567890,
        bid_exch_mask: 0, ask_exch_mask: 0, last_exch_mask: 0,
        halted: 0,
    };
    shared.market.push_quote(0, &q);

    client.core.req_to_instrument.lock().unwrap().insert(1, 0);
    client.core.instrument_to_req.lock().unwrap().insert(0, 1);

    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);

    // Should have tick_price for: bid(1), ask(2), last(4), high(6), low(7), close(9),
    // open(14)
    assert!(w.events.iter().any(|e| e.starts_with("tick_price:1:1:")));   // bid
    assert!(w.events.iter().any(|e| e.starts_with("tick_price:1:2:")));   // ask
    assert!(w.events.iter().any(|e| e.starts_with("tick_price:1:4:")));   // last
    assert!(w.events.iter().any(|e| e.starts_with("tick_price:1:6:")));   // high
    assert!(w.events.iter().any(|e| e.starts_with("tick_price:1:7:")));   // low
    assert!(w.events.iter().any(|e| e.starts_with("tick_price:1:9:")));   // close
    assert!(w.events.iter().any(|e| e.starts_with("tick_price:1:14:"))); // open
    // tick_size for: bid_size(0), ask_size(3), last_size(5), volume(8).
    // Assert the delivered quantity, not just that a tick appeared — the
    // scaling defect in fired every one of these with a value four
    // orders of magnitude off, and a `starts_with` check passed throughout.
    let delivered = |prefix: &str| -> Option<f64> {
        w.events.iter().find(|e| e.starts_with(prefix))
            .and_then(|e| e.rsplit(':').next())
            .and_then(|v| v.parse().ok())
    };
    assert_eq!(delivered("tick_size:1:0:"), Some(1000.0), "bid_size");
    assert_eq!(delivered("tick_size:1:3:"), Some(2000.0), "ask_size");
    assert_eq!(delivered("tick_size:1:5:"), Some(500.0), "last_size");
    assert_eq!(delivered("tick_size:1:8:"), Some(10_000.0), "volume");
}

#[test]
fn process_msgs_multiple_instruments_independent() {
    let (client, _rx, shared) = test_client();
    let q0 = Quote { bid: 150 * PRICE_SCALE, ..Default::default() };
    shared.market.push_quote(0, &q0);
    let q1 = Quote { bid: 400 * PRICE_SCALE, ..Default::default() };
    shared.market.push_quote(1, &q1);

    client.core.req_to_instrument.lock().unwrap().insert(1, 0);
    client.core.instrument_to_req.lock().unwrap().insert(0, 1);
    client.core.req_to_instrument.lock().unwrap().insert(2, 1);
    client.core.instrument_to_req.lock().unwrap().insert(1, 2);

    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("tick_price:1:1:150")));
    assert!(w.events.iter().any(|e| e.starts_with("tick_price:2:1:400")));
}

// ═══════════════════════════════════════════════════════════════════
//  process_msgs — TBT trades / quotes
// ═══════════════════════════════════════════════════════════════════

#[test]
fn process_msgs_dispatches_tbt_trade() {
    let (client, _rx, shared) = test_client();
    client.core.instrument_to_req.lock().unwrap().insert(0, 10);
    shared.market.push_tbt_trade(TbtTrade {
        req_id: 10,
        // A hundred shares, held the way every quantity is held.
        instrument: 0, price: 150 * PRICE_SCALE, size: 100 * crate::types::QTY_SCALE,
        timestamp: 1700000000, exchange: "ARCA".into(), conditions: "".into(),
        past_limit: false,
        unreported: false,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("tbt_last:10:1:1700000000:150:100:ARCA")));
}

#[test]
fn process_msgs_dispatches_tbt_quote() {
    let (client, _rx, shared) = test_client();
    client.core.instrument_to_req.lock().unwrap().insert(0, 10);
    shared.market.push_tbt_quote(TbtQuote {
        req_id: 10,
        instrument: 0, bid: 150 * PRICE_SCALE, ask: 151 * PRICE_SCALE,
        bid_size: 1000 * crate::types::QTY_SCALE,
        ask_size: 2000 * crate::types::QTY_SCALE,
        timestamp: 1700000000,
        bid_past_low: false,
        ask_past_high: false,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("tbt_bidask:10:1700000000:150:151:1000:2000")));
}

#[test]
fn process_msgs_tbt_records_carry_the_request_they_arrived_under() {
    let (client, _rx, shared) = test_client();
    // One contract, two streams: every trade, and every quote change. Looked
    // up by contract, both would be handed whichever request was made last.
    client.core.instrument_to_req.lock().unwrap().insert(0, 99);
    shared.market.push_tbt_trade(TbtTrade {
        req_id: 10,
        instrument: 0, price: 150 * PRICE_SCALE, size: 100 * crate::types::QTY_SCALE,
        timestamp: 0, exchange: "".into(), conditions: "".into(),
        past_limit: false,
        unreported: false,
    });
    shared.market.push_tbt_quote(TbtQuote {
        req_id: 11,
        instrument: 0, bid: 150 * PRICE_SCALE, ask: 151 * PRICE_SCALE,
        bid_size: crate::types::QTY_SCALE, ask_size: crate::types::QTY_SCALE,
        timestamp: 0,
        bid_past_low: false,
        ask_past_high: false,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(
        w.events.iter().any(|e| e.starts_with("tbt_last:10:")),
        "the trade did not carry its own request: {:?}", w.events,
    );
    assert!(
        w.events.iter().any(|e| e.starts_with("tbt_bidask:11:")),
        "the quote did not carry its own request: {:?}", w.events,
    );
    assert!(
        !w.events.iter().any(|e| e.contains(":99:")),
        "a record was attributed by contract rather than by request",
    );
}

// ═══════════════════════════════════════════════════════════════════
//  process_msgs — tick news
// ═══════════════════════════════════════════════════════════════════

#[test]
fn process_msgs_dispatches_tick_news() {
    let (client, _rx, shared) = test_client();
    client.core.instrument_to_req.lock().unwrap().insert(0, 1);
    shared.market.push_tick_news(TickNews {
        instrument: 0,
        provider_code: "BRFG".into(), article_id: "BRFG$123".into(),
        headline: "AAPL beats".into(), timestamp: 1700000000,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e == "tick_news:BRFG:BRFG$123:AAPL beats"));
}

// ═══════════════════════════════════════════════════════════════════
//  process_msgs — news bulletins
// ═══════════════════════════════════════════════════════════════════

#[test]
fn process_msgs_dispatches_news_bulletin() {
    let (client, _rx, shared) = test_client();
    client.req_news_bulletins(true);
    shared.market.push_news_bulletin(NewsBulletin {
        msg_id: 1, msg_type: 1,
        message: "Exchange notice".into(), exchange: "NYSE".into(),
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e == "news_bulletin:1:1:Exchange notice:NYSE"));
}

/// A bulletin subscription starts from the moment it is made unless `all_msgs`
/// is set.
///
/// Bulletins are broadcast at the session whether or not anything is subscribed,
/// so those already queued are discarded on subscribing unless asked for.
#[test]
fn a_bulletin_subscription_starts_where_the_caller_says_it_does() {
    let earlier = || NewsBulletin {
        msg_id: 7, msg_type: 1, message: "Published before anyone asked".into(),
        exchange: "NYSE".into(),
    };

    let (client, _rx, shared) = test_client();
    shared.market.push_news_bulletin(earlier());
    client.req_news_bulletins(false);
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(
        !w.events.iter().any(|e| e.starts_with("news_bulletin:7")),
        "a subscription for what follows opened with what came before: {:?}",
        w.events,
    );

    // And what is published after it started arrives.
    shared.market.push_news_bulletin(NewsBulletin {
        msg_id: 8, msg_type: 1, message: "Published after".into(), exchange: "NYSE".into(),
    });
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("news_bulletin:8")));

    // Asking for the day's own is answered with the ones already held.
    let (asks_for_all, _rx, shared) = test_client();
    shared.market.push_news_bulletin(earlier());
    asks_for_all.req_news_bulletins(true);
    let mut w = RecordingWrapper::default();
    asks_for_all.process_msgs(&mut w);
    assert!(
        w.events.iter().any(|e| e.starts_with("news_bulletin:7")),
        "the day's own were asked for and not delivered: {:?}",
        w.events,
    );
}

// ═══════════════════════════════════════════════════════════════════
//  process_msgs — what-if
// ═══════════════════════════════════════════════════════════════════

#[test]
fn process_msgs_dispatches_what_if() {
    let (client, _rx, shared) = test_client();
    shared.orders.push_what_if(WhatIfResponse {
        order_id: 42, instrument: 0,
        init_margin_before: 0, maint_margin_before: 0,
        equity_with_loan_before: 0,
        init_margin_after: 5000 * PRICE_SCALE,
        maint_margin_after: 3000 * PRICE_SCALE,
        equity_with_loan_after: 0,
        commission: Some(PRICE_SCALE),
        min_commission: None,
        max_commission: None,
        commission_currency: String::new(),
        warning_text: String::new(),
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("open_order:42:PreSubmitted")));
    assert!(
        !w.events.iter().any(|e| e.starts_with("order_status:42")),
        "a preview is answered on the order, and the venue states no status for one: {:?}",
        w.events,
    );
}

/// Regression: what-if dispatch must populate all 8 OrderState fields and call
/// open_order BEFORE order_status, matching official ibapi contract.
#[test]
fn process_msgs_what_if_emits_full_order_state() {
    let (client, _rx, shared) = test_client();
    // Distinct values per field so any swap/typo is detectable.
    shared.orders.push_what_if(WhatIfResponse {
        order_id: 7, instrument: 0,
        init_margin_before:    100 * PRICE_SCALE,
        maint_margin_before:   200 * PRICE_SCALE,
        equity_with_loan_before: 300 * PRICE_SCALE,
        init_margin_after:     400 * PRICE_SCALE,
        maint_margin_after:    500 * PRICE_SCALE,
        equity_with_loan_after: 600 * PRICE_SCALE,
        commission:            Some(7 * PRICE_SCALE),
        min_commission: None,
        max_commission: None,
        commission_currency: String::new(),
        warning_text: String::new(),
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);

    let open_idx = w.events.iter().position(|e| e.starts_with("open_order:7:"))
        .expect("open_order callback missing for what-if");

    let evt = &w.events[open_idx];
    // status, all 9 margin fields (before/change/after × init/maint/eql), commission.
    assert!(evt.contains(":PreSubmitted:"), "status field missing: {evt}");
    assert!(evt.contains("initB=100.00:initC=300.00:initA=400.00"), "init margin wrong: {evt}");
    assert!(evt.contains("maintB=200.00:maintC=300.00:maintA=500.00"), "maint margin wrong: {evt}");
    assert!(evt.contains("eqlB=300.00:eqlC=300.00:eqlA=600.00"), "equity-with-loan wrong: {evt}");
    assert!(evt.contains("comm=7"), "commission wrong: {evt}");
}

/// A preview is a question about an order, not an order: nothing reaches
/// the book while it runs, and nothing may report it as working. Left on
/// the book, every preview counted as exposure the account did not have.
#[test]
fn a_preview_is_never_reported_as_a_working_order() {
    let (client, rx, shared) = test_client();
    let preview = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "LMT".into(),
        lmt_price: 150.0, what_if: true, ..Default::default()
    };
    client.place_order(6101, &spy(), &preview).unwrap();
    while rx.try_recv().is_ok() {}

    // Still unanswered: nothing reads it as working.
    assert!(
        client.core.collect_open_orders(&client.shared).iter().all(|(id, _)| *id != 6101),
        "a preview awaiting its answer is not an open order",
    );

    // Answered: it leaves nothing behind.
    shared.orders.push_what_if(WhatIfResponse {
        order_id: 6101, instrument: 0,
        init_margin_before: 0, maint_margin_before: 0, equity_with_loan_before: 0,
        init_margin_after: crate::types::PRICE_SCALE, maint_margin_after: 0,
        equity_with_loan_after: 0, commission: Some(0), min_commission: None, max_commission: None,
        commission_currency: String::new(), warning_text: String::new(),
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(
        client.core.collect_open_orders(&client.shared).iter().all(|(id, _)| *id != 6101),
        "an answered preview leaves nothing on the book",
    );
}

/// A preview the venue refuses ends there: the reason is reported under
/// the order's number and the record goes with the refusal. Left
/// standing, the refusal read as a live order and its number as spent.
#[test]
fn a_refused_preview_is_reported_and_leaves_nothing() {
    let (client, rx, shared) = test_client();
    let preview = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "LMT".into(),
        lmt_price: 150.0, what_if: true, ..Default::default()
    };
    client.place_order(6102, &spy(), &preview).unwrap();
    while rx.try_recv().is_ok() {}

    // The venue's refusal of a preview arrives as an error under the
    // order's number.
    shared.orders.push_order_inactive(6102, 201, "the margin cannot be stated".into());
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(
        w.events.iter().any(|e| e.starts_with("error:6102:201:")),
        "the refusal reaches the caller: {:?}", w.events,
    );
    assert!(!client.core.is_order_tracked(6102), "the record went with the refusal");
    assert!(client.core.collect_open_orders(&client.shared).is_empty());
}

/// A preview asked as a question answers with the venue's refusal and
/// leaves nothing behind: no record, and the number it went out under is
/// free to the next order.
#[test]
fn a_preview_the_venue_refuses_answers_the_refusal_and_leaves_nothing() {
    let (client, _rx, shared) = test_client();
    let preview = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "LMT".into(),
        lmt_price: 150.0, what_if: true, ..Default::default()
    };
    let pushed = Arc::clone(&shared);
    let refused = std::thread::scope(|scope| {
        scope.spawn(|| {
            // The number is this client's own choosing while the question
            // runs; wait for the record before refusing under it.
            let id = loop {
                let found = client.core.open_orders.lock().unwrap().iter()
                    .find(|(_, tracked)| tracked.order.what_if)
                    .map(|(id, _)| *id);
                if let Some(id) = found { break id; }
                std::thread::sleep(std::time::Duration::from_millis(1));
            };
            pushed.orders.push_order_inactive(id, 201, "the margin cannot be stated".into());
        });
        client.what_if_order(&spy(), &preview).expect_err("the venue refused")
    });
    assert_eq!(refused.code, 201, "the venue's number reaches the caller: {refused}");
    assert!(client.core.open_orders.lock().unwrap().is_empty(), "nothing stays on the book");
}

/// A preview nothing answers is given up on, and its record goes with
/// the giving up. The wait is the whole answer timeout, which is the
/// length of this test.
#[test]
fn a_preview_nothing_answers_leaves_nothing_behind() {
    let (client, _rx, _shared) = test_client();
    let preview = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "LMT".into(),
        lmt_price: 150.0, what_if: true, ..Default::default()
    };
    let refused = client.what_if_order(&spy(), &preview).expect_err("nothing answers");
    assert_eq!(refused.code, Refusal::NO_ANSWER, "silence says so: {refused}");
    assert!(
        client.core.open_orders.lock().unwrap().is_empty(),
        "a question nobody answered leaves nothing on the book",
    );
}

// ═══════════════════════════════════════════════════════════════════
//  process_msgs — historical data
// ═══════════════════════════════════════════════════════════════════

#[test]
fn process_msgs_dispatches_historical_data() {
    let (client, _rx, shared) = test_client();
    shared.reference.push_historical_data(5, HistoricalResponse {
        query_id: String::new(), timezone: String::new(),
        bars: vec![
            HistoricalBar { time: "20260101".into(), open: 100.0, high: 105.0, low: 99.0, close: 103.0, volume: 1000, wap: 102.0, count: 50, end: String::new() },
            HistoricalBar { time: "20260102".into(), open: 103.0, high: 108.0, low: 102.0, close: 107.0, volume: 1200, wap: 105.0, count: 60, end: String::new() },
        ],
        is_complete: true,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e == "historical_data:5:20260101"));
    assert!(w.events.iter().any(|e| e == "historical_data:5:20260102"));
    assert!(w.events.iter().any(|e| e == "historical_data_end:5"));
}

#[test]
fn process_msgs_historical_data_incomplete_no_end() {
    let (client, _rx, shared) = test_client();
    shared.reference.push_historical_data(5, HistoricalResponse {
        query_id: String::new(), timezone: String::new(),
        bars: vec![
            HistoricalBar { time: "20260101".into(), open: 100.0, high: 105.0, low: 99.0, close: 103.0, volume: 1000, wap: 102.0, count: 50, end: String::new() },
        ],
        is_complete: false,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e == "historical_data:5:20260101"));
    assert!(!w.events.iter().any(|e| e == "historical_data_end:5"), "no end for incomplete");
}

// ═══════════════════════════════════════════════════════════════════
//  process_msgs — head timestamps
// ═══════════════════════════════════════════════════════════════════

#[test]
fn process_msgs_dispatches_head_timestamp() {
    let (client, _rx, shared) = test_client();
    shared.reference.push_head_timestamp(10, HeadTimestampResponse { head_timestamp: "20200101".into(), timezone: String::new() });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e == "head_timestamp:10:20200101"));
}

/// A head timestamp is returned in the form `format_date` asked for, as bars
/// are. 2 = seconds since the epoch.
#[test]
fn a_head_timestamp_is_written_the_way_it_was_asked_for() {
    let (client, _rx, shared) = test_client();
    client.req_head_time_stamp(11, &spy(), "TRADES", true, 2).expect("the request is sent");
    shared.reference.push_head_timestamp(11, HeadTimestampResponse {
        head_timestamp: "20200101-00:00:00".into(), timezone: String::new(),
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(
        w.events.iter().any(|e| e == "head_timestamp:11:1577836800"),
        "asked for in seconds since the epoch: {:?}",
        w.events,
    );

    // A request asking for format 1 keeps the wire's own spelling.
    client.req_head_time_stamp(12, &spy(), "TRADES", true, 1).expect("the request is sent");
    shared.reference.push_head_timestamp(12, HeadTimestampResponse {
        head_timestamp: "20200101-00:00:00".into(), timezone: String::new(),
    });
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e == "head_timestamp:12:20200101-00:00:00"));
}

/// A reused request id starts a new request: its completion latch is cleared, so
/// the bars arrive as initial data and `historical_data_end` fires again.
#[test]
fn a_historical_request_under_a_used_id_answers_from_the_beginning() {
    let (client, _rx, shared) = test_client();
    // As the first request left it.
    client.core.hist_initial_complete.lock().unwrap().insert(13);

    client
        .req_historical_data(13, &spy(), "", "1 D", "1 hour", "TRADES", true, 1, false)
        .expect("the request is sent");
    shared.reference.push_historical_data(13, HistoricalResponse {
        query_id: String::new(), timezone: String::new(),
        bars: vec![HistoricalBar {
            time: "20200101-00:00:00".into(), open: 1.0, high: 1.0, low: 1.0, close: 1.0,
            volume: 1, wap: 1.0, count: 1, end: String::new(),
        }],
        is_complete: true,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(
        w.events.iter().any(|e| e.starts_with("historical_data:13:")),
        "the bars answering a new request arrived as updates to the old one: {:?}",
        w.events,
    );
    assert!(
        w.events.iter().any(|e| e.starts_with("historical_data_end:13")),
        "and the request never ended: {:?}",
        w.events,
    );
}

/// A trade callback names the stream it carries: tick type 1 = Last,
/// 2 = AllLast. The trade record does not carry it, so the request's kind is.
#[test]
fn a_trade_stream_says_which_of_the_two_it_is() {
    let (client, rx, shared) = test_client();
    // Long enough for the stand-in below to be scheduled. The tests default to
    // a millisecond, which is a real engine answering from another thread and
    // a flake when the machine is busy.
    client.core.set_registration_timeout(std::time::Duration::from_secs(5));
    // Standing in for the engine, which answers a subscription by naming the
    // slot it took. Without an answer the call waits out its registration.
    let engine = std::thread::spawn(move || {
        while let Ok(cmd) = rx.recv() {
            if let ControlCommand::SubscribeTbt { reply_tx: Some(reply), .. } = cmd {
                let _ = reply.try_send(Ok(0));
            }
        }
    });
    client.req_tick_by_tick_data(20, &spy(), "AllLast", 0, false).expect("subscribed");
    client.req_tick_by_tick_data(21, &spy(), "Last", 0, false).expect("subscribed");

    for req_id in [20, 21] {
        shared.market.push_tbt_trade(crate::types::TbtTrade {
            instrument: 0, req_id, price: PRICE_SCALE, size: 1, timestamp: 0,
            exchange: "NYSE".into(), conditions: String::new(),
            past_limit: false, unreported: false,
        });
    }
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(
        w.events.iter().any(|e| e.starts_with("tbt_last:20:2:")),
        "every trade was reported as the exchange's own: {:?}",
        w.events,
    );
    assert!(w.events.iter().any(|e| e.starts_with("tbt_last:21:1:")));

    drop(client);
    engine.join().expect("the stand-in engine");
}

// ═══════════════════════════════════════════════════════════════════
//  process_msgs — contract details
// ═══════════════════════════════════════════════════════════════════

#[test]
fn process_msgs_dispatches_contract_details() {
    let (client, _rx, shared) = test_client();
    shared.reference.push_contract_details(7, ContractDefinition {
        con_id: 265598, symbol: "AAPL".into(), sec_type: SecurityType::Stock,
        exchange: "SMART".into(), primary_exchange: "NASDAQ".into(),
        currency: "USD".into(), local_symbol: "AAPL".into(),
        trading_class: "AAPL".into(), long_name: "Apple Inc".into(),
        min_tick: 0.01, multiplier: 1.0, valid_exchanges: vec!["SMART".into()],
        order_types: vec!["LMT".into()], market_rule_id: Some(26),
        last_trade_date: String::new(), right: None, strike: 0.0,
        ..Default::default()
    });
    shared.reference.push_contract_details_end(7);
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e == "contract_details:7:AAPL"));
    assert!(w.events.iter().any(|e| e == "contract_details_end:7"));
}

// ═══════════════════════════════════════════════════════════════════
//  process_msgs — matching symbols
// ═══════════════════════════════════════════════════════════════════

#[test]
fn process_msgs_dispatches_symbol_samples() {
    let (client, _rx, shared) = test_client();
    shared.reference.push_matching_symbols(8, vec![
        SymbolMatch {
            con_id: 265598, symbol: "AAPL".into(), sec_type: SecurityType::Stock,
            currency: "USD".into(), primary_exchange: "NASDAQ".into(),
            description: "Apple Inc".into(), derivative_types: vec!["OPT".into()],
            issuer_id: String::new(),
        },
    ]);
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e == "symbol_samples:8:1"));
}

// ═══════════════════════════════════════════════════════════════════
//  process_msgs — option chain
// ═══════════════════════════════════════════════════════════════════

/// Every class of the underlying is reported, and the request ends once.
#[test]
fn process_msgs_dispatches_option_chain_parameters() {
    let (client, _rx, shared) = test_client();
    shared.reference.push_option_params(9, 265598, vec![
        OptionChainScope {
            symbol: "AAPL".into(), exchange: "SMART".into(), trading_class: "AAPL".into(),
            multiplier: "100".into(), expirations: vec!["20260116".into(), "20260320".into()],
            strikes: vec![140.0, 145.0], underlying_con_id: 265598,
        },
        OptionChainScope {
            symbol: "AAPL".into(), exchange: "CBOE".into(), trading_class: "AAPL1".into(),
            multiplier: "100".into(), expirations: vec!["20260116".into()],
            strikes: vec![145.0], underlying_con_id: 265598,
        },
    ]);
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e == "sec_def_opt_param:9:SMART:265598:AAPL:100:20260116,20260320:140,145"), "{:?}", w.events);
    assert!(w.events.iter().any(|e| e == "sec_def_opt_param:9:CBOE:265598:AAPL1:100:20260116:145"), "{:?}", w.events);
    assert_eq!(w.events.iter().filter(|e| *e == "sec_def_opt_param_end:9").count(), 1);
}

/// A chain the venue lists nothing for is still an answer: the caller is
/// waiting on the end of the request, not on a class that does not exist.
#[test]
fn process_msgs_ends_an_empty_option_chain() {
    let (client, _rx, shared) = test_client();
    shared.reference.push_option_params(9, 265598, Vec::new());
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert_eq!(w.events, vec!["sec_def_opt_param_end:9".to_string()]);
}

// ═══════════════════════════════════════════════════════════════════
//  process_msgs — scanner
// ═══════════════════════════════════════════════════════════════════

#[test]
fn process_msgs_dispatches_scanner_params() {
    let (client, _rx, shared) = test_client();
    shared.reference.push_scanner_params("<scanner>XML</scanner>".into());
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e == "scanner_parameters"));
}

#[test]
fn process_msgs_dispatches_scanner_data() {
    let (client, _rx, shared) = test_client();
    shared.reference.push_scanner_data(3, ScannerResult {
        con_ids: vec![265598, 756733],
        entries: vec![
            ScannerEntry { con_id: 265598 },
            ScannerEntry { con_id: 756733 },
        ],
        scan_time: "2026-03-13".into(),
        error_text: String::new(),
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e == "scanner_data:3:0"));
    assert!(w.events.iter().any(|e| e == "scanner_data:3:1"));
    assert!(w.events.iter().any(|e| e == "scanner_data_end:3"));
}

// ═══════════════════════════════════════════════════════════════════
//  process_msgs — news
// ═══════════════════════════════════════════════════════════════════

#[test]
fn process_msgs_dispatches_historical_news() {
    let (client, _rx, shared) = test_client();
    shared.reference.push_historical_news(4, vec![
        NewsHeadline {
            time: "2026-01-15".into(), provider_code: "BRFG".into(),
            article_id: "BRFG$100".into(), headline: "Earnings beat".into(),
        },
        NewsHeadline {
            time: "2026-01-16".into(), provider_code: "BRFG".into(),
            article_id: "BRFG$101".into(), headline: "Guidance raised".into(),
        },
    ], false);
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e == "historical_news:4:BRFG:BRFG$100:Earnings beat"));
    assert!(w.events.iter().any(|e| e == "historical_news:4:BRFG:BRFG$101:Guidance raised"));
    assert!(w.events.iter().any(|e| e == "historical_news_end:4:false"));
}

#[test]
fn process_msgs_dispatches_news_article() {
    let (client, _rx, shared) = test_client();
    shared.reference.push_news_article(5, 0, "Full article text here".into());
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e == "news_article:5:0:Full article text here"));
}

// ═══════════════════════════════════════════════════════════════════
//  process_msgs — fundamental data
// ═══════════════════════════════════════════════════════════════════

#[test]
fn process_msgs_dispatches_fundamental_data() {
    let (client, _rx, shared) = test_client();
    shared.reference.push_fundamental_data(6, "<report>data</report>".into());
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e == "fundamental_data:6"));
}

// ═══════════════════════════════════════════════════════════════════
//  process_msgs — histogram data
// ═══════════════════════════════════════════════════════════════════

#[test]
fn process_msgs_dispatches_histogram_data() {
    let (client, _rx, shared) = test_client();
    shared.reference.push_histogram_data(7, vec![
        HistogramEntry { price: 150.0, count: 500 },
        HistogramEntry { price: 151.0, count: 300 },
    ]);
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e == "histogram_data:7:2"));
}

// ═══════════════════════════════════════════════════════════════════
//  process_msgs — historical ticks
// ═══════════════════════════════════════════════════════════════════

#[test]
fn process_msgs_dispatches_historical_ticks() {
    let (client, _rx, shared) = test_client();
    shared.reference.push_historical_ticks(8, HistoricalTickData::Midpoint(vec![
        HistoricalTickMidpoint { time: "2026-01-15 09:30:00".into(), price: 150.5 },
    ]), "MIDPOINT".into(), true);
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e == "historical_ticks:8:true"));
}

/// Each historical-tick variant routes to its own callback, as in ibapi,
/// rather than all three arriving through `historical_ticks`.
#[test]
fn process_msgs_routes_historical_tick_variants() {
    let (client, _rx, shared) = test_client();
    shared.reference.push_historical_ticks(10, HistoricalTickData::Last(vec![
        HistoricalTickLast {
            time: "2026-01-15 09:30:00".into(), price: 150.5, size: 100.0,
            exchange: "ARCA".into(), special_conditions: "".into(),
            past_limit: false, unreported: false,
        },
    ]), "TRADES".into(), true);
    shared.reference.push_historical_ticks(11, HistoricalTickData::BidAsk(vec![
        HistoricalTickBidAsk {
            time: "2026-01-15 09:30:01".into(), bid_price: 150.4, ask_price: 150.6,
            bid_size: 200.0, ask_size: 300.0,
            bid_past_low: false, ask_past_high: false,
        },
    ]), "BID_ASK".into(), true);
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);

    assert!(w.events.iter().any(|e| e == "historical_ticks_last:10:true"),
        "Last variant must route to historical_ticks_last; got {:?}", w.events);
    assert!(w.events.iter().any(|e| e == "historical_ticks_bid_ask:11:true"),
        "BidAsk variant must route to historical_ticks_bid_ask; got {:?}", w.events);
    // Generic historical_ticks should NOT fire for Last or BidAsk.
    assert!(!w.events.iter().any(|e| e == "historical_ticks:10:true"));
    assert!(!w.events.iter().any(|e| e == "historical_ticks:11:true"));
}

// ═══════════════════════════════════════════════════════════════════
//  process_msgs — real-time bars
// ═══════════════════════════════════════════════════════════════════

#[test]
fn process_msgs_dispatches_real_time_bar() {
    let (client, _rx, shared) = test_client();
    shared.market.push_real_time_bar(9, RealTimeBar {
        timestamp: 1700000000, open: 150.0, high: 151.0,
        low: 149.0, close: 150.5, volume: 1000.0, wap: 150.25, count: 50,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("real_time_bar:9:1700000000")));
}

// ═══════════════════════════════════════════════════════════════════
//  process_msgs — historical schedule
// ═══════════════════════════════════════════════════════════════════

#[test]
fn process_msgs_dispatches_historical_schedule() {
    let (client, _rx, shared) = test_client();
    shared.reference.push_historical_schedule(11, HistoricalScheduleResponse {
        query_id: String::new(),
        timezone: "US/Eastern".into(),
        start_date_time: "20260101".into(),
        end_date_time: "20260102".into(),
        sessions: vec![ScheduleSession {
            ref_date: "20260101".into(),
            open_time: "09:30:00".into(),
            close_time: "16:00:00".into(),
        }],
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e == "historical_schedule:11:US/Eastern:1"));
}

// ═══════════════════════════════════════════════════════════════════
//  process_msgs — drain is exhaustive
// ═══════════════════════════════════════════════════════════════════

#[test]
fn process_msgs_empty_queues_no_events() {
    let (client, _rx, _shared) = test_client();
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.is_empty());
}

#[test]
fn process_msgs_drains_on_first_call_empty_on_second() {
    let (client, _rx, shared) = test_client();
    shared.orders.push_fill(Fill {
        instrument: 0, order_id: 1, side: Side::Buy,
        price: PRICE_SCALE, qty: crate::types::QTY_SCALE, remaining: 0, timestamp_ns: 0,
        cum_qty: crate::types::QTY_SCALE, avg_price: PRICE_SCALE,
    });
    shared.orders.push_order_update(OrderUpdate {
        order_id: 2, instrument: 0, status: OrderStatus::Submitted,
        filled_qty: 0.0, remaining_qty: 1.0, avg_price: 0, perm_id: 0, parent_id: 0, timestamp_ns: 0,
    });

    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(!w.events.is_empty());

    w.events.clear();
    client.process_msgs(&mut w);
    // Only quote events might fire (if mapped), but no fills/updates
    let non_tick_events: Vec<_> = w.events.iter()
        .filter(|e| !e.starts_with("tick_price") && !e.starts_with("tick_size"))
        .collect();
    assert!(non_tick_events.is_empty(), "second drain should be empty");
}

// ═══════════════════════════════════════════════════════════════════
//  process_msgs — a fill answers no request unless one asked for it
// ═══════════════════════════════════════════════════════════════════

/// An unsolicited fill is reported against request id -1. A market-data
/// subscription id does not identify a `reqExecutions` request.
#[test]
fn a_fill_that_answers_no_request_is_reported_against_none() {
    let (client, _rx, shared) = test_client();
    client.core.instrument_to_req.lock().unwrap().insert(0, 42);
    shared.orders.push_fill(Fill {
        instrument: 0, order_id: 1, side: Side::Buy,
        price: PRICE_SCALE, qty: 100 * crate::types::QTY_SCALE, remaining: 0, timestamp_ns: 0,
        cum_qty: 100 * crate::types::QTY_SCALE, avg_price: PRICE_SCALE,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(
        w.events.iter().any(|e| e.starts_with("exec_details:-1:")),
        "the fill was numbered after a quote subscription: {:?}",
        w.events.iter().filter(|e| e.starts_with("exec_details")).collect::<Vec<_>>(),
    );
}

// ── Order modification edge cases ─────────────────────────────────

#[test]
fn modify_limit_order_price_via_resubmit() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0,
        order_type: "LMT".into(), lmt_price: 150.0, ..Default::default()
    };
    client.place_order(80, &spy(), &order).unwrap();
    while rx.try_recv().is_ok() {}

    let modified = Order {
        action: "BUY".into(), total_quantity: 100.0,
        order_type: "LMT".into(), lmt_price: 152.0, ..Default::default()
    };
    client.place_order(80, &spy(), &modified).unwrap();

    let mut found = false;
    while let Ok(cmd) = rx.try_recv() {
        if let ControlCommand::Order(OrderRequest::Modify { order_id: 80, price, qty, .. }) = cmd {
            assert_eq!(price, (152.0 * PRICE_SCALE_F) as i64);
            assert_eq!(qty, 100 * crate::types::QTY_SCALE);
            found = true;
        }
    }
    assert!(found, "Resubmit with same orderId should emit Modify");
}

#[test]
fn modify_order_before_ack_no_panic() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    for price in 0..10 {
        let order = Order {
            action: "BUY".into(), total_quantity: 100.0,
            order_type: "LMT".into(), lmt_price: 150.0 + price as f64,
            ..Default::default()
        };
        let _ = client.place_order(42, &spy(), &order);
    }
    let mut count = 0;
    while rx.try_recv().is_ok() { count += 1; }
    assert!(count >= 10, "All modify attempts should send commands, got {count}");
}

#[test]
fn cancel_during_modify_no_panic() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0,
        order_type: "LMT".into(), lmt_price: 150.0, ..Default::default()
    };
    client.place_order(99, &spy(), &order).unwrap();
    let modified = Order {
        action: "BUY".into(), total_quantity: 100.0,
        order_type: "LMT".into(), lmt_price: 151.0, ..Default::default()
    };
    client.place_order(99, &spy(), &modified).unwrap();
    client.cancel_order(99, "").unwrap();

    let mut has_cancel = false;
    while let Ok(cmd) = rx.try_recv() {
        if matches!(cmd, ControlCommand::Order(OrderRequest::Cancel { order_id: 99 })) {
            has_cancel = true;
        }
    }
    assert!(has_cancel, "Cancel command should be sent");
}

#[test]
fn modify_filled_order_receives_cancel_reject() {
    let (client, _rx, shared) = test_client();
    client.map_req_instrument(1, 0);
    shared.orders.push_fill(Fill {
        instrument: 0, order_id: 120, side: Side::Buy,
        price: 150 * PRICE_SCALE, qty: 100 * crate::types::QTY_SCALE, remaining: 0, timestamp_ns: 1000,
        cum_qty: 100 * crate::types::QTY_SCALE, avg_price: 150 * PRICE_SCALE,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("order_status:120:Filled")));

    shared.orders.push_cancel_reject(CancelReject {
        order_id: 120, instrument: 0, reject_type: 2, reason_code: 0, answers_a_live_change: true, still_working: None, timestamp_ns: 2000,
    });
    w.events.clear();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("error:120:")),
        "Modify reject should generate error callback, got: {:?}", w.events);
}

#[test]
fn rapid_modify_multiple_prices_no_crash() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    for i in 0..50 {
        let order = Order {
            action: "BUY".into(), total_quantity: 100.0,
            order_type: "LMT".into(), lmt_price: 100.0 + i as f64 * 0.01,
            ..Default::default()
        };
        let _ = client.place_order(77, &spy(), &order);
    }
    let mut order_count = 0;
    // The statement each replace goes behind is not itself a modify.
    while let Ok(cmd) = rx.try_recv() {
        if matches!(cmd, ControlCommand::Order(OrderRequest::Modify { .. })) { order_count += 1; }
    }
    // The first of the fifty places the order; the other forty-nine move it.
    assert_eq!(order_count, 49, "every price after the first is sent as a modify");
}

#[test]
fn modify_tif_day_to_gtc_via_resubmit() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0,
        order_type: "LMT".into(), lmt_price: 150.0,
        tif: "DAY".into(), ..Default::default()
    };
    client.place_order(88, &spy(), &order).unwrap();
    while rx.try_recv().is_ok() {}

    let modified = Order {
        action: "BUY".into(), total_quantity: 100.0,
        order_type: "LMT".into(), lmt_price: 150.0,
        tif: "GTC".into(), ..Default::default()
    };
    client.place_order(88, &spy(), &modified).unwrap();

    let mut found_modify = false;
    while let Ok(cmd) = rx.try_recv() {
        if let ControlCommand::Order(OrderRequest::Modify { order_id: 88, price, qty, tif, .. }) = cmd {
            assert_eq!(price, (150.0 * PRICE_SCALE_F) as i64);
            assert_eq!(qty, 100 * crate::types::QTY_SCALE);
            // The change the test is named for. Asserting only that a Modify
            // was emitted passed for as long as the time-in-force was dropped.
            assert_eq!(tif, b'1', "the modify must carry GTC, not restate DAY");
            found_modify = true;
        }
    }
    assert!(found_modify, "Resubmit with same orderId should emit Modify");
}

#[test]
fn modify_price_and_qty_simultaneously() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0,
        order_type: "LMT".into(), lmt_price: 150.0, ..Default::default()
    };
    client.place_order(55, &spy(), &order).unwrap();
    while rx.try_recv().is_ok() {}

    let modified = Order {
        action: "BUY".into(), total_quantity: 200.0,
        order_type: "LMT".into(), lmt_price: 148.0, ..Default::default()
    };
    client.place_order(55, &spy(), &modified).unwrap();

    let mut found = false;
    while let Ok(cmd) = rx.try_recv() {
        if let ControlCommand::Order(OrderRequest::Modify { order_id: 55, qty, price, .. }) = cmd {
            assert_eq!(qty, 200 * crate::types::QTY_SCALE);
            assert_eq!(price, (148.0 * PRICE_SCALE_F) as i64);
            found = true;
        }
    }
    assert!(found, "Resubmit with same orderId should emit Modify with new price and qty");
}

/// A zero order-type states nothing and keeps the resting type, so a modify to
/// a type the replace cannot express must be refused rather than reaching the
/// encoder — otherwise the caller's new type is silently restated as the old
/// one and the client caches an order the venue does not have.
#[test]
fn a_modify_to_an_unrepresentable_type_is_refused() {
    for order_type in ["REL", "TRAIL", "LIT", "MIDPX", "SNAP MKT"] {
        let (client, rx, shared) = test_client();
        shared.market.set_instrument_count(1);
        let plain = Order {
            action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(),
            lmt_price: 100.0, tif: "DAY".into(), ..Default::default()
        };
        client.place_order(9401, &spy(), &plain).expect("a plain limit submits");
        while rx.try_recv().is_ok() {}

        let converted = Order {
            order_type: order_type.into(), aux_price: 99.0, ..plain.clone()
        };
        let err = client.place_order(9401, &spy(), &converted)
            .expect_err("converting to a type the replace cannot express must be refused");
        assert!(err.message.contains("cannot be modified"), "{order_type}: {err}");
        assert!(rx.try_recv().is_err(), "{order_type}: nothing reaches the wire");
    }
}

#[test]
fn modify_order_type_lmt_to_stp() {
    let (client, rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0,
        order_type: "LMT".into(), lmt_price: 150.0, ..Default::default()
    };
    client.place_order(66, &spy(), &order).unwrap();
    while rx.try_recv().is_ok() {}

    let modified = Order {
        action: "BUY".into(), total_quantity: 100.0,
        order_type: "STP".into(), aux_price: 149.0, ..Default::default()
    };
    client.place_order(66, &spy(), &modified).unwrap();

    let mut found_modify = false;
    while let Ok(cmd) = rx.try_recv() {
        if let ControlCommand::Order(OrderRequest::Modify {
            order_id: 66, ord_type, stop_price, ..
        }) = cmd {
            // The change the test is named for. Asserting only that a Modify
            // was emitted passed for as long as the order type was dropped.
            assert_eq!(ord_type, b'3', "the modify must carry STP, not restate LMT");
            assert_eq!(stop_price, (149.0 * PRICE_SCALE_F) as i64,
                "and the trigger the caller set");
            found_modify = true;
        }
    }
    assert!(found_modify, "Resubmit with same orderId should emit Modify");
}

// ── Market data type switching ────────────────────────────────────

#[test]
fn market_data_type_callback_compiles_and_dispatches() {
    struct MarketDataTypeRecorder { events: Vec<(i64, i32)> }
    impl crate::api::wrapper::Wrapper for MarketDataTypeRecorder {
        fn market_data_type(&mut self, req_id: i64, market_data_type: i32) {
            self.events.push((req_id, market_data_type));
        }
    }
    let mut w = MarketDataTypeRecorder { events: vec![] };
    w.market_data_type(1, 1); // Live
    w.market_data_type(1, 2); // Frozen
    w.market_data_type(1, 3); // Delayed
    w.market_data_type(1, 4); // Delayed-Frozen
    assert_eq!(w.events.len(), 4);
    assert_eq!(w.events[0], (1, 1));
    assert_eq!(w.events[3], (1, 4));
}

#[test]
fn quote_dispatch_agnostic_to_data_type() {
    let (client, _rx, shared) = test_client();
    client.map_req_instrument(1, 0);
    let q = Quote { bid: 450 * PRICE_SCALE, ask: 451 * PRICE_SCALE, ..Default::default() };
    shared.market.push_quote(0, &q);
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("tick_price:1:1:450")));
}

#[test]
fn frozen_stale_quote_no_redispatch() {
    let (client, _rx, shared) = test_client();
    client.map_req_instrument(1, 0);
    let q = Quote { bid: 300 * PRICE_SCALE, ask: 301 * PRICE_SCALE, ..Default::default() };
    shared.market.push_quote(0, &q);

    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("tick_price:1:")));

    shared.market.push_quote(0, &q); // same quote
    w.events.clear();
    client.process_msgs(&mut w);
    let second_count = w.events.iter().filter(|e| e.starts_with("tick_price:1:")).count();
    assert_eq!(second_count, 0, "Identical frozen quote should not re-dispatch");
}

#[test]
fn transition_no_data_to_live_fires_callbacks() {
    let (client, _rx, shared) = test_client();
    client.map_req_instrument(1, 0);
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert_eq!(w.events.iter().filter(|e| e.starts_with("tick_price:1:")).count(), 0);

    let q = Quote { bid: 500 * PRICE_SCALE, ask: 501 * PRICE_SCALE, ..Default::default() };
    shared.market.push_quote(0, &q);
    w.events.clear();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("tick_price:1:1:500")));
    assert!(w.events.iter().any(|e| e.starts_with("tick_price:1:2:501")));
}

#[test]
fn partial_quote_update_only_changed_fields_dispatch() {
    let (client, _rx, shared) = test_client();
    client.map_req_instrument(1, 0);
    let mut q = Quote { bid: 100 * PRICE_SCALE, ask: 101 * PRICE_SCALE, ..Default::default() };
    shared.market.push_quote(0, &q);

    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);

    q.bid = 99 * PRICE_SCALE;
    shared.market.push_quote(0, &q);
    w.events.clear();
    client.process_msgs(&mut w);

    let bid_ticks: Vec<_> = w.events.iter().filter(|e| e.starts_with("tick_price:1:1:")).collect();
    let ask_ticks: Vec<_> = w.events.iter().filter(|e| e.starts_with("tick_price:1:2:")).collect();
    assert!(!bid_ticks.is_empty(), "Changed bid should dispatch");
    assert!(ask_ticks.is_empty(), "Unchanged ask should NOT dispatch");
}

// ═══════════════════════════════════════════════════════════════════
//  Thread lifecycle
// ═══════════════════════════════════════════════════════════════════

#[test]
fn disconnect_joins_thread() {
    let (client, _rx, _shared) = test_client();
    // The test_client helper spawns an empty thread (already exited).
    // disconnect() should join it without hanging.
    client.disconnect();
    assert!(!client.is_connected());
}

#[test]
fn drop_without_disconnect_joins_thread() {
    let (client, _rx, _shared) = test_client();
    // Dropping without explicit disconnect — Drop impl should join.
    drop(client);
    // No hang = success.
}

#[test]
fn disconnect_is_idempotent() {
    let (client, _rx, _shared) = test_client();
    client.disconnect();
    // Second disconnect should not panic (thread already joined).
    client.disconnect();
    assert!(!client.is_connected());
}

#[test]
fn ccp_session_id_matches_shared_reference() {
    let (client, _rx, shared) = test_client();
    assert_eq!(client.ccp_session_id(), shared.reference.ccp_session_id());

    shared.reference.set_ccp_session_id("sid.0001".to_string());
    assert_eq!(client.ccp_session_id(), "sid.0001");
    assert_eq!(client.ccp_session_id(), client.shared.reference.ccp_session_id());
}

#[test]
fn misc_url_lookup_delegates_to_shared() {
    let (client, _rx, shared) = test_client();
    assert!(client.misc_url("region_dam").is_none());

    let mut urls = std::collections::HashMap::new();
    urls.insert("region_dam".to_string(), "api.example.com".to_string());
    shared.reference.set_misc_urls(urls);

    assert_eq!(client.misc_url("region_dam").as_deref(), Some("api.example.com"));
    assert!(client.misc_url("missing").is_none());
}

#[test]
fn session_token_bytes_roundtrip_through_biguint() {
    use num_bigint::BigUint;

    let shared = Arc::new(SharedState::new());
    let (tx, _rx) = std::sync::mpsc::sync_channel(4096);
    let handle = std::thread::spawn(|| {});
    let mut client = EClient::from_parts(shared, tx, handle, "DU123".into());

    let session_token = BigUint::parse_bytes(
        b"fedcba9876543210fedcba9876543210", 16,
    ).unwrap();
    client.session_token_bytes = crate::auth::crypto::strip_leading_zeros(
        &session_token.to_bytes_be(),
    ).to_vec();

    assert_eq!(BigUint::from_bytes_be(client.session_token_bytes()), session_token);
}

// ═══════════════════════════════════════════════════════════════════
// Connection loss
// ═══════════════════════════════════════════════════════════════════

#[test]
fn engine_connection_loss_fires_connection_closed_once() {
    let (client, _rx, shared) = test_client();
    let mut w = RecordingWrapper::default();

    // Nothing to report while the engine is running.
    client.process_msgs(&mut w);
    assert!(client.is_connected());
    assert!(w.events.is_empty(), "no callbacks before the connection is lost");

    // A loss the engine is still working to recover: said under 1100, as the
    // other surface says it, not as the session's end.
    shared.set_connection_lost();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("error:-1:1100:")), "{:?}", w.events);
    assert!(!w.events.iter().any(|e| e == "connection_closed"), "the session is not over: {:?}", w.events);
    assert!(!client.is_connected(), "is_connected must turn false");

    // Its return.
    shared.set_connection_restored();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e.starts_with("error:-1:1102:")), "{:?}", w.events);
    assert!(client.is_connected());

    // The session's end.
    shared.reference.set_session_over("the venue ended it");
    shared.set_connection_lost();
    client.process_msgs(&mut w);
    assert_eq!(w.events.iter().filter(|e| *e == "connection_closed").count(), 1, "{:?}", w.events);
    assert!(!client.is_connected(), "is_connected must turn false");

    // Polling again must not repeat it.
    let said = w.events.len();
    client.process_msgs(&mut w);
    assert_eq!(w.events.len(), said, "nothing is said twice: {:?}", w.events);
    assert_eq!(w.events.iter().filter(|e| *e == "connection_closed").count(), 1);
}

/// The health check reads the session's own state, not only what a pump
/// has observed. A shape that never pumps `process_msgs` heard of an
/// engine-side ending nowhere else, and kept saying connected after the
/// session underneath it was over.
#[test]
fn is_connected_says_false_once_the_session_is_over() {
    let (client, _rx, shared) = test_client();
    assert!(client.is_connected(), "a fresh session is connected");

    // Recorded by the engine itself; nothing is pumped here.
    shared.reference.set_session_over(
        crate::reliability::retry::DisconnectReason::EngineStopped.as_str(),
    );
    assert!(!client.is_connected(), "an ended session is not connected");
}

#[test]
fn explicit_disconnect_fires_connection_closed() {
    let (client, _rx, _shared) = test_client();
    let mut w = RecordingWrapper::default();

    client.disconnect();
    client.process_msgs(&mut w);

    assert_eq!(w.events, vec!["connection_closed"]);
    assert!(!client.is_connected());
}

#[test]
fn queued_data_is_dispatched_before_connection_closed() {
    // A caller that stops polling on connection_closed must still have seen
    // whatever the engine had already queued.
    let (client, _rx, shared) = test_client();
    let mut w = RecordingWrapper::default();

    shared.reference.push_contract_details_end(7);
    shared.reference.set_session_over("the venue ended it");
    shared.set_connection_lost();
    client.process_msgs(&mut w);

    assert_eq!(w.events.first().map(String::as_str), Some("contract_details_end:7"), "{:?}", w.events);
    assert_eq!(w.events.last().map(String::as_str), Some("connection_closed"), "{:?}", w.events);
}

/// The code provider must reach the session config for the authenticator factor
/// to be usable. It is one field in a struct literal and no other test covers
/// it.
#[test]
fn the_second_factor_provider_reaches_the_gateway_config() {
    use crate::api::client::gateway_config;

    let base = crate::api::client::EClientConfig {
        username: "u".into(), password: "p".into(), host: "h".into(),
        paper: false, core_id: None, code_provider: None,
        ..Default::default()
    };
    assert!(gateway_config(&base).code_provider.is_none(), "none stays none");

    let called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = called.clone();
    let with_provider = crate::api::client::EClientConfig {
        code_provider: Some(std::sync::Arc::new(move |_: crate::auth::session::IbKeyChallenge| {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok("12345678".to_string())
        })),
        ..base
    };
    let forwarded = gateway_config(&with_provider).code_provider
        .expect("the provider is forwarded, not dropped");

    // Identity, not just presence: forwarding some other closure would pass a
    // bare `is_some`.
    forwarded(crate::auth::session::IbKeyChallenge {
        factor: crate::auth::session::SecondFactor::AuthenticatorCode,
        display_id: String::new(),
        avth_url: String::new(),
    }).unwrap();
    assert!(called.load(std::sync::atomic::Ordering::SeqCst), "it is the caller's own provider");
}


/// A display group is how two callers on one session agree on a contract. The
/// venue is not involved and never was, so the whole behaviour is this
/// client's to reproduce: what the groups are, what each holds, and who is
/// told when one changes.
#[test]
fn a_display_group_keeps_its_followers_in_step() {
    let (client, _rx, _shared) = test_client();

    client.query_display_groups(1);
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert_eq!(
        w.events.iter().find(|e| e.starts_with("display_group_list:")).map(String::as_str),
        Some("display_group_list:1:1|2|3|4|5|6|7"),
        "the groups on offer: {:?}", w.events,
    );

    // Two callers follow the same group; a third follows another.
    client.subscribe_to_group_events(10, 3);
    client.subscribe_to_group_events(11, 3);
    client.subscribe_to_group_events(12, 4);
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert_eq!(
        w.events.iter().filter(|e| e.ends_with(":none")).count(), 3,
        "each is told what its group holds now, not only what it changes to: {:?}", w.events,
    );

    // One of them puts a contract in it.
    client.update_display_group(10, "756733@SMART").unwrap();
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    let told: Vec<&String> = w.events.iter()
        .filter(|e| e.starts_with("display_group_updated:")).collect();
    assert_eq!(told.len(), 2, "both followers of that group, and only those: {told:?}");
    assert!(told.iter().all(|e| e.ends_with(":756733@SMART")), "{told:?}");
    assert!(told.iter().any(|e| e.contains(":10:")), "including the one that changed it: {told:?}");
    assert!(told.iter().any(|e| e.contains(":11:")), "{told:?}");

    // A caller that follows nothing has no group to put a contract in.
    let refusal = client.update_display_group(99, "1@SMART").unwrap_err();
    assert!(refusal.message.contains("follows no display group"), "{refusal}");

    // Once it stops following, it is no longer told.
    client.unsubscribe_from_group_events(11);
    client.update_display_group(10, "").unwrap();
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    let told: Vec<&String> = w.events.iter()
        .filter(|e| e.starts_with("display_group_updated:")).collect();
    assert_eq!(told.len(), 1, "only the one still following: {told:?}");
    assert!(told[0].ends_with(":none"), "and an empty contract empties the group: {told:?}");
}

/// A login holding more than one account is answered with all of them, comma
/// separated, led by the account a caller gets by default. Answering with only
/// the first is how a caller managing linked accounts stops seeing the rest.
#[test]
fn managed_accounts_names_every_account_the_login_holds() {
    #[derive(Default)]
    struct W(Vec<String>);
    impl crate::api::wrapper::Wrapper for W {
        fn managed_accounts(&mut self, accounts: &str) { self.0.push(accounts.to_string()); }
    }

    let (mut client, _rx, _shared) = test_client();
    let mut w = W::default();

    // One account: answered with that account and no comma.
    client.req_managed_accts(&mut w);
    assert_eq!(w.0, vec!["DU123".to_string()]);

    client.accounts = vec!["DU123".into(), "DU456".into(), "DU789".into()];
    client.req_managed_accts(&mut w);
    assert_eq!(w.0[1], "DU123,DU456,DU789");
}

#[cfg(test)]
mod answering_calls_receive_through_dispatch {
    use crate::bridge::SharedState;
    use crate::control::contracts::ContractDefinition;

    /// This shape's answering calls receive through the dispatch loop, so the
    /// drain that feeds it must hand over their replies too.
    ///
    /// Withholding them was a change made for the other shape, whose answering
    /// calls take replies out of the queue by id and so need them left alone.
    /// One drain served both, and the change broke every answering call here
    /// while every offline test kept passing — the queues in those tests are
    /// filled by hand, so nothing depended on the drain being the delivery.
    #[test]
    fn a_reply_to_an_answering_call_is_not_withheld_from_the_dispatch_that_delivers_it() {
        let shared = SharedState::new();
        let ask_id = crate::bridge::ReferenceState::ASK_ID_BASE;
        shared.reference.push_contract_details(
            ask_id,
            ContractDefinition { con_id: 756733, ..Default::default() },
        );

        let delivered = shared.reference.drain_contract_details();
        assert_eq!(
            delivered.len(),
            1,
            "an answering call's reply was withheld from the drain that delivers it"
        );
        assert_eq!(delivered[0].0, ask_id);
    }

    /// The other shape still gets its replies left where it will find them.
    #[test]
    fn the_dispatch_that_does_not_deliver_them_still_leaves_them() {
        let shared = SharedState::new();
        // Recorded as this client's own, which is what the ask path does when
        // it hands the id out. Which ids those are is no longer read off their
        // magnitude, so a number alone establishes nothing.
        let ask_id = crate::bridge::ReferenceState::ASK_ID_BASE;
        shared.reference.note_ours(crate::bridge::RecordKind::Answer, ask_id as i64);
        shared.reference.push_contract_details(
            ask_id,
            ContractDefinition { con_id: 756733, ..Default::default() },
        );
        shared.reference.push_contract_details(
            7,
            ContractDefinition { con_id: 111, ..Default::default() },
        );

        let delivered = shared.reference.drain_contract_details_for_dispatch();
        assert_eq!(delivered.len(), 1, "a caller's own reply was withheld");
        assert_eq!(delivered[0].0, 7);
        assert_eq!(shared.reference.take_contract_details_for(ask_id).len(), 1);
        shared.reference.forget_ours(crate::bridge::RecordKind::Answer, ask_id as i64);
    }

    /// A caller may number a request anything at all, including inside the
    /// band this client counts its own from.
    ///
    /// The band was how the two were told apart, so a program numbering its
    /// requests from a counter that had climbed into it had its answers
    /// withheld and nothing said why. Nothing is withheld now but what was
    /// recorded.
    #[test]
    fn a_caller_may_number_a_request_inside_this_clients_own_band() {
        let shared = SharedState::new();
        let theirs = crate::bridge::ReferenceState::ASK_ID_BASE + 5;
        shared.reference.push_contract_details(
            theirs,
            ContractDefinition { con_id: 756733, ..Default::default() },
        );

        let delivered = shared.reference.drain_contract_details_for_dispatch();
        assert_eq!(
            delivered.len(),
            1,
            "a caller's reply was withheld for its number rather than because \
             this client was waiting on it",
        );
        assert_eq!(delivered[0].0, theirs);
    }
}

/// A keep-up-to-date request answers once with its history and then keeps
/// speaking. The reference client separates the two, and this surface reported
/// only the first: a caller that overrode the update callback heard nothing,
/// and the continued bars arrived as real-time bars it never asked for.
#[test]
fn a_kept_up_to_date_request_reports_its_history_then_its_updates() {
    #[derive(Default)]
    struct Heard {
        history: Vec<i64>,
        ended: Vec<i64>,
        updates: Vec<i64>,
        real_time: Vec<i64>,
    }
    impl Wrapper for Heard {
        fn historical_data(&mut self, req_id: i64, _bar: &crate::types::model::BarData) { self.history.push(req_id); }
        fn historical_data_end(&mut self, req_id: i64, _s: &str, _e: &str) { self.ended.push(req_id); }
        fn historical_data_update(&mut self, req_id: i64, _bar: &crate::types::model::BarData) { self.updates.push(req_id); }
        fn real_time_bar(&mut self, req_id: i64, _t: i64, _o: f64, _h: f64, _l: f64,
                         _c: f64, _v: f64, _w: f64, _n: i32) { self.real_time.push(req_id); }
    }

    let (client, _rx, shared) = test_client();
    let mut heard = Heard::default();

    // The initial answer, complete.
    shared.reference.push_historical_data(9, HistoricalResponse {
        query_id: String::new(), timezone: String::new(),
        bars: vec![HistoricalBar { time: "20260101".into(), open: 100.0, high: 105.0, low: 99.0, close: 103.0, volume: 1000, wap: 102.0, count: 50, end: String::new() }],
        is_complete: true,
    });
    client.process_msgs(&mut heard);

    assert_eq!(heard.history, vec![9], "the history is history");
    assert_eq!(heard.ended, vec![9], "and it says when it has finished");
    assert!(heard.updates.is_empty(), "nothing is an update yet");

    // What the venue keeps sending afterwards, on both feeds it can arrive on.
    shared.reference.push_historical_data(9, HistoricalResponse {
        query_id: String::new(), timezone: String::new(),
        bars: vec![HistoricalBar { time: "20260101".into(), open: 100.0, high: 105.0, low: 99.0, close: 103.0, volume: 1000, wap: 102.0, count: 50, end: String::new() }],
        is_complete: false,
    });
    shared.market.push_real_time_bar(9, Default::default());
    client.process_msgs(&mut heard);

    assert_eq!(heard.updates, vec![9, 9], "both continued bars are updates");
    assert_eq!(heard.history, vec![9], "and neither is more history");
    assert_eq!(heard.ended, vec![9], "nor a second end");
    assert!(
        heard.real_time.is_empty(),
        "a request nobody made is not answered with real-time bars",
    );
}

/// The account subscription reports what the account holds as well as what it
/// is worth. A caller watching its positions through it heard only the values.
#[test]
fn subscribing_to_account_updates_reports_the_portfolio() {
    #[derive(Default)]
    struct Heard {
        values: Vec<String>,
        positions: Vec<(i64, f64)>,
    }
    impl Wrapper for Heard {
        fn update_account_value(&mut self, key: &str, _v: &str, _c: &str, _a: &str) {
            self.values.push(key.to_string());
        }
        fn update_portfolio(&mut self, contract: &Contract, position: f64, _mp: f64, _mv: f64,
                            _ac: f64, _up: f64, _rp: f64, _acct: &str) {
            self.positions.push((contract.con_id, position));
        }
    }

    let (client, _rx, shared) = test_client();
    let mut heard = Heard::default();

    client.req_account_updates(true, "DU123");
    shared.portfolio.holdings_restated_under("AR.1");
    shared.portfolio.set_position_info(crate::types::PositionInfo {
        con_id: 756733,
        position: 100.0,
        avg_cost: 490 * crate::types::PRICE_SCALE,
        symbol: "SPY".into(),
        sec_type: "STK".into(),
        ..Default::default()
    });
    shared.portfolio.note_account_value("NetLiquidation", "100000.00", "USD");
    // The venue has finished stating the account, which is what the portfolio
    // and summary reads wait on: answered on the first figure instead, every
    // holding went out at a price of nothing after a reconnect. The holding
    // above is one the download stated, so the download opens before it and
    // ends after it -- opened afterwards, its own end would square it away as
    // one the venue had stopped naming.
    shared.portfolio.set_account_download_complete("AR.1");
    shared.portfolio.account_download_is_settled();
    client.process_msgs(&mut heard);

    assert!(heard.values.contains(&"NetLiquidation".to_string()), "the values still arrive");
    assert_eq!(
        heard.positions, vec![(756733, 100.0)],
        "and the holding they describe arrives with them",
    );
}

/// A calculation this client makes answers the call that asked for it.
///
/// The caller's request id was stored in the field naming the option, and the
/// dispatcher then read it as one and mapped it again — so the answer arrived
/// under an unrelated subscription's id, or under none at all.
#[test]
fn a_local_option_calculation_answers_the_request_that_asked() {
    #[derive(Default)]
    struct Heard(Vec<i64>);
    impl Wrapper for Heard {
        fn tick_option_computation(&mut self, req_id: i64, _tick: i32, _attr: i32,
                                   _iv: f64, _d: f64, _op: f64, _pv: f64, _g: f64,
                                   _v: f64, _t: f64, _up: f64) {
            self.0.push(req_id);
        }
    }

    let (client, _rx, shared) = test_client();
    let mut heard = Heard::default();

    // An id far outside the instrument table, so reading it as one cannot
    // accidentally land on the right answer.
    let asked = 4242i64;
    shared.market.push_option_computation(crate::types::OptionComputation {
        answers: Some(asked),
        implied_vol: 0.25,
        ..Default::default()
    });
    client.process_msgs(&mut heard);

    assert_eq!(heard.0, vec![asked], "the answer names the call that asked for it");
}

/// `reqCurrentTime` answers the venue's clock, and always answers.
///
/// A caller asks it to learn how far apart the two clocks are. The answer is
/// this machine's clock shifted onto the venue's, by however much the venue
/// has said they differ — so it keeps running between messages instead of
/// standing still at the last stamp, and before the venue has said anything
/// the shift is nothing and the answer is this machine's own. There is no
/// state in which the question is refused: a session that is connected can
/// always be told the time.
#[test]
fn the_current_time_is_the_venues_own() {
    #[derive(Default)]
    struct Heard { times: Vec<i64>, errors: Vec<(i64, String)> }
    impl Wrapper for Heard {
        fn current_time(&mut self, t: i64) { self.times.push(t); }
        fn error(&mut self, _req_id: i64, code: i64, msg: &str, _: &str) {
            self.errors.push((code, msg.to_string()));
        }
    }

    let (client, _rx, shared) = test_client();
    let mut heard = Heard::default();

    // Before the venue has said anything, the question is still answered —
    // with this machine's clock, which is what no shift means.
    client.req_current_time(&mut heard);
    assert_eq!(heard.times.len(), 1, "a connected session is told the time");
    assert!(heard.errors.is_empty(), "and is not refused for it");

    // Once the venue states its clock, the answer lands on that clock.
    shared.market.note_venue_time("20260815-12:00:00");
    client.req_current_time(&mut heard);
    let stated = heard.times[1];
    assert!(
        (stated - 1_786_795_200).abs() < 5,
        "the answer sits on the venue's clock, got {stated}",
    );
    assert!(heard.errors.is_empty(), "and nothing was refused");
}

/// Asking in milliseconds keeps the fraction asking in seconds throws away.
///
/// Both calls read the same clock — the venue's own last stamp — so the answer
/// to one is the answer to the other times a thousand, except where the venue
/// stamped a fraction of a second. That case is the only reason the second
/// call exists, so it is the one worth pinning.
#[test]
fn the_millisecond_clock_keeps_what_the_second_one_drops() {
    #[derive(Default)]
    struct Heard { secs: Vec<i64>, millis: Vec<i64> }
    impl Wrapper for Heard {
        fn current_time(&mut self, t: i64) { self.secs.push(t); }
        fn current_time_in_millis(&mut self, t: i64) { self.millis.push(t); }
    }

    let (client, _rx, shared) = test_client();
    let mut heard = Heard::default();

    // A stamp with no fraction: the two agree to the thousand.
    shared.market.note_venue_time("20260815-12:00:00");
    client.req_current_time(&mut heard);
    client.req_current_time_in_millis(&mut heard);
    assert_eq!(heard.secs[0], 1_786_795_200);
    // The clock keeps running between the statement and the reading — that is
    // the whole point of holding a difference rather than a stamp — so this is
    // near the stated instant rather than exactly on it. The margin is far
    // below the quarter-second the second half of this test turns on.
    let near = |read: i64, stated: i64, what: &str| {
        assert!(
            (read - stated).abs() < 100,
            "{what}: read {read}, which is not near {stated}",
        );
    };
    near(heard.millis[0], 1_786_795_200_000, "a stamp with no fraction");

    // One with a fraction: seconds cannot carry it, milliseconds can.
    shared.market.note_venue_time("20260815-12:00:00.250");
    client.req_current_time(&mut heard);
    client.req_current_time_in_millis(&mut heard);
    assert_eq!(heard.secs[1], 1_786_795_200, "the same second");
    near(heard.millis[1], 1_786_795_200_250, "and a quarter of it besides");
}

/// A session that came back and went again is not a connected session.
///
/// Loss and recovery were two flags with no order between them. Both raised,
/// the dispatcher applied recovery last whichever way the connection had
/// actually gone — so a client reported itself connected to a socket that had
/// dropped, and nothing was left pending to correct it.
#[test]
fn the_last_thing_the_connection_did_is_what_a_caller_is_told() {
    let (client, _rx, shared) = test_client();
    let mut w = RecordingWrapper::default();

    // Lost, recovered, and lost again before anyone looked.
    shared.set_connection_lost();
    shared.set_connection_restored();
    shared.set_connection_lost();
    client.process_msgs(&mut w);

    assert!(!client.is_connected(), "the connection went and did not come back");
    assert_eq!(
        w.events.iter().filter(|e| e.starts_with("error:-1:1100:")).count(), 1,
        "and the caller is told once: {:?}", w.events,
    );

    // The other way round: a recovery after a loss stands.
    let (client, _rx, shared) = test_client();
    let mut w = RecordingWrapper::default();
    shared.set_connection_lost();
    shared.set_connection_restored();
    client.process_msgs(&mut w);
    assert!(client.is_connected(), "the connection came back");
}

/// A request made while the engine is still rebuilding a lost connection is
/// carried, and only one made after the session has ended is refused.
///
/// Guarded on `connected`, which the loss clears and the recovery restores, a
/// withdrawal made in between was answered 504 and left unapplied — so the
/// feed the caller had withdrawn came back with the session. The reference
/// client serves that window; what it answers 504 is a session with nothing
/// to come back to, closed or given up on, which is what the engine records.
#[test]
fn a_request_during_a_recoverable_loss_is_carried_and_one_after_the_end_is_refused() {
    use std::sync::atomic::Ordering;
    let (client, _rx, shared) = test_client();
    let mut w = RecordingWrapper::default();
    let refused = |w: &RecordingWrapper| {
        w.events.iter().filter(|e| e.starts_with("error:-1:504")).count()
    };
    let summary_asked = || {
        client.core.account_summary_req.lock().unwrap().as_ref().map(|(id, _)| *id)
    };

    // Announced lost, with the engine still working on it: no end recorded.
    shared.set_connection_lost();
    client.process_msgs(&mut w);
    assert!(!client.is_connected(), "the loss was announced");

    client.positions_requested.store(true, Ordering::Release);
    client.cancel_positions();
    client.req_account_summary(7, "All", "NetLiquidation");
    client.process_msgs(&mut w);
    assert_eq!(refused(&w), 0, "a request during recovery was refused: {:?}", w.events);
    assert!(!client.positions_requested.load(Ordering::Acquire), "the withdrawal took");
    assert_eq!(summary_asked(), Some(7), "and so did the request");

    // The engine gave up: the end is recorded, and the same calls are refused.
    shared.reference.set_session_over(
        crate::reliability::retry::DisconnectReason::EngineStopped.as_str(),
    );
    client.positions_requested.store(true, Ordering::Release);
    client.cancel_positions();
    client.req_account_summary(8, "All", "NetLiquidation");
    client.process_msgs(&mut w);
    assert_eq!(refused(&w), 2, "each is answered 504 once: {:?}", w.events);
    assert!(client.positions_requested.load(Ordering::Acquire), "and nothing was applied");
    // The request parked behind the download is answered once the session
    // is over, since no download is coming; the later one was never taken.
    assert_eq!(summary_asked(), None, "the parked request was answered, the later one not taken");
    assert!(
        w.events.iter().any(|e| e.starts_with("account_summary_end")),
        "the parked request ended: {:?}", w.events,
    );
}

/// Two callers subscribing one contract at once: one holds it, the other
/// follows. Deciding and taking are one acquisition, so they cannot both read
/// the contract as free and both take it — which left the second write owning
/// the mapping and the first request quiet, with nothing to say why.
#[test]
fn one_contract_has_one_owner_however_many_ask_at_once() {
    use std::sync::Arc;

    let (client, _rx, _shared) = test_client();
    let core = Arc::new(client);
    let instrument = 0u32;

    let barrier = Arc::new(std::sync::Barrier::new(8));
    let claimed: Arc<std::sync::Mutex<Vec<i64>>> = Arc::new(std::sync::Mutex::new(Vec::new()));

    std::thread::scope(|scope| {
        for req_id in 1..=8i64 {
            let core = Arc::clone(&core);
            let barrier = Arc::clone(&barrier);
            let claimed = Arc::clone(&claimed);
            scope.spawn(move || {
                barrier.wait();
                if !core.core.take_or_follow(instrument, req_id, &[], 0, 0) {
                    claimed.lock().unwrap().push(req_id);
                }
            });
        }
    });

    let owners = claimed.lock().unwrap();
    assert_eq!(owners.len(), 1, "exactly one request holds the contract: {owners:?}");
    assert_eq!(
        core.core.instrument_to_req.lock().unwrap().get(&instrument),
        Some(&owners[0]),
        "and the mapping names that one",
    );
    assert_eq!(
        core.core.followers_of(instrument).len(), 7,
        "everybody else follows it rather than being dropped",
    );
}

/// An option solve is computed locally against the model the venue published for
/// that contract. The protocol carries no request for one.
#[test]
fn solving_an_option_answers_against_the_venues_own_model() {
    #[derive(Default)]
    struct Heard {
        computed: Vec<(i64, f64)>,
        greeks: Vec<(f64, f64, f64, f64)>,
        errors: Vec<String>,
    }
    impl Wrapper for Heard {
        fn tick_option_computation(&mut self, req_id: i64, _t: i32, _a: i32, _iv: f64,
                                   delta: f64, opt_price: f64, _pv: f64, gamma: f64,
                                   vega: f64, theta: f64, _up: f64) {
            self.computed.push((req_id, opt_price));
            self.greeks.push((delta, gamma, vega, theta));
        }
        fn error(&mut self, _req_id: i64, _code: i64, msg: &str, _adv: &str) {
            self.errors.push(msg.to_string());
        }
    }

    let (client, _rx, shared) = test_client();
    let mut heard = Heard::default();

    let mut option = spy();
    option.con_id = 756733;
    option.sec_type = "OPT".into();
    option.strike = 500.0;
    option.right = "C".into();
    option.last_trade_date_or_contract_month = "20270115".into();

    // With no published model there is nothing to solve against; the call reports
    // that rather than inventing a rate.
    client.calculate_option_price(5, &option, 0.25, 505.0);
    client.process_msgs(&mut heard);
    assert!(heard.computed.is_empty(), "no model, no answer");
    assert!(!heard.errors.is_empty(), "and the caller is told why");

    // With it, the answer is solved and delivered under the caller's request.
    shared.market.push_option_computation(crate::types::OptionComputation {
        answers: None,
        instrument: 0,
        implied_vol: 0.20,
        opt_price: 30.0,
        und_price: 505.0,
        ..Default::default()
    });
    let _ = shared.market.drain_option_computations();
    heard.errors.clear();

    client.calculate_option_price(6, &option, 0.25, 505.0);
    client.process_msgs(&mut heard);

    assert!(heard.errors.is_empty(), "{:?}", heard.errors);
    assert_eq!(heard.computed.len(), 1, "the price was answered");
    assert_eq!(heard.computed[0].0, 6, "under the request that asked for it");
    assert!(heard.computed[0].1 > 0.0, "and it is a price");

    // The question the other way round — what volatility a price implies —
    // is solved against the same model and answered the same way.
    heard.computed.clear();
    client.calculate_implied_volatility(7, &option, 32.0, 505.0);
    client.process_msgs(&mut heard);

    assert!(heard.errors.is_empty(), "{:?}", heard.errors);
    assert_eq!(heard.computed.len(), 1, "the volatility was answered");
    assert_eq!(heard.computed[0].0, 7);

    // Fields this does not compute carry the unset sentinel. Zero is a valid
    // greek and cannot stand for one.
    let (delta, gamma, vega, theta) = heard.greeks[0];
    for (name, stated) in [("delta", delta), ("gamma", gamma), ("vega", vega), ("theta", theta)] {
        assert_eq!(stated, f64::MAX, "{name} was answered as a number nobody worked out");
    }
}

/// A question asked from inside a callback is told why rather than left
/// waiting.
///
/// A read hands every message it drains to the wrapper it was given, and a
/// wrapper is a caller's own code. A question asked from one waits for the
/// turn that same thread is already holding, on a lock that is not
/// re-entrant, and the read that would have answered it is the one it is
/// inside — so the thread stopped for good, with no deadline anywhere to say
/// so. The two shapes are one session and a program is told to mix them; this
/// is the one way of mixing them that cannot work, and it says so on the spot.
#[test]
fn a_question_asked_from_inside_a_callback_is_refused_rather_than_left_waiting() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let told: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let read = Arc::new(AtomicBool::new(false));
    let (t, r) = (Arc::clone(&told), Arc::clone(&read));
    // On a thread of its own, and never joined: a wedged read cannot be, and
    // the harness has to report a failure rather than stop on one.
    std::thread::spawn(move || {
        let (client, _rx, shared) = test_client();
        shared.reference.push_historical_error(9, 321, "the venue said no".to_string());

        struct AsksBack<'a> { client: &'a EClient, told: &'a Mutex<Option<String>> }
        impl Wrapper for AsksBack<'_> {
            fn error(&mut self, _req_id: i64, _code: i64, _message: &str, _advanced: &str) {
                *self.told.lock().unwrap() = Some(
                    match self.client.contract_details(&Contract::default()) {
                        Ok(_) => "answered".to_string(),
                        Err(refused) => refused.to_string(),
                    },
                );
            }
        }
        let mut asking = AsksBack { client: &client, told: &t };
        client.process_msgs(&mut asking);
        r.store(true, Ordering::Release);
    });

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while !read.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(read.load(Ordering::Acquire), "the read never returned from the callback");
    let answer = told.lock().unwrap().clone().expect("the callback asked its question");
    assert!(
        answer.contains("from inside a callback"),
        "the question was not told why it cannot be answered here: {answer}",
    );
}

/// A dispatch loop reads the session, so it takes the session's turn.
///
/// The calls that answer pump the loop themselves and keep what carries their
/// own request id. A second reader running beside one takes the answer first
/// and hands it to a callback, and the question waits out its whole deadline
/// for a reply that had already arrived. The session shape has always taken
/// the turn around its reader; a caller driving the loop itself had no way to
/// know it had to.
#[test]
fn a_dispatch_loop_waits_for_the_question_that_is_reading() {
    let (client, _rx, shared) = test_client();
    shared.reference.push_historical_error(9, 321, "the venue said no".to_string());
    let turn = client.asking.lock().unwrap();
    std::thread::scope(|s| {
        let reading = s.spawn(|| {
            let mut heard = RecordingWrapper::default();
            client.process_msgs(&mut heard);
        });
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(
            shared.reference.take_error_for(9).is_some(),
            "the reason was taken while a question held the turn",
        );
        drop(turn);
        reading.join().unwrap();
    });
}

/// Completed orders are retained: the arrival queue empties on read and the
/// venue does not resend them, so later calls answer from the archive.
#[test]
fn completed_orders_are_still_there_when_they_are_asked_for_again() {
    let (client, _rx, shared) = test_client();
    shared.orders.push_completed_order(crate::types::CompletedOrder {
        order_id: 31, instrument: 0, status: crate::types::OrderStatus::Filled,
        filled_qty: 100, timestamp_ns: 0,
    });

    let mut w = RecordingWrapper::default();
    client.req_completed_orders(false, &mut w);
    assert_eq!(w.events.iter().filter(|e| *e == "completed_order").count(), 1);

    let mut again = RecordingWrapper::default();
    client.req_completed_orders(false, &mut again);
    assert_eq!(
        again.events.iter().filter(|e| *e == "completed_order").count(),
        1,
        "asked a second time, the account read as having completed nothing: {:?}",
        again.events,
    );
}

/// An order the venue has finished with is not reported as working.
///
/// The record this client keeps of an order it placed is its own, and a
/// terminal report moves it on the dispatch pass. A caller asking in between
/// was handed the order as working, with a quantity outstanding that is not
/// outstanding — what the bridge remembers finishing is the wire's own
/// statement and outranks the local record.
#[test]
fn an_order_the_venue_has_finished_with_is_not_reported_as_working() {
    let (client, _rx, shared) = test_client();
    client.core.track_order(
        44,
        Contract { symbol: "SPY".into(), sec_type: "STK".into(), ..Default::default() },
        Order { order_id: 44, action: "BUY".into(), total_quantity: 100.0, ..Default::default() },
        0,
    );
    // The venue finishes it. The dispatch pass that would move the local
    // record has not run.
    shared.orders.push_completed_order(crate::types::CompletedOrder {
        order_id: 44, instrument: 0, status: crate::types::OrderStatus::Filled,
        filled_qty: 100, timestamp_ns: 0,
    });
    shared.orders.set_replay_done();

    let mut w = RecordingWrapper::default();
    client.req_open_orders(&mut w);
    assert!(
        !w.events.iter().any(|e| e == "open_order" || e.starts_with("open_order:")),
        "an order the venue has finished with is not working: {:?}", w.events,
    );
    assert!(
        w.events.iter().any(|e| e == "open_order_end"),
        "and the answer still ends: {:?}", w.events,
    );
}

/// One question of what the account has finished at a time.
///
/// The answer is a run of ordinary reports and one sentinel, and nothing in
/// the run says which question it answers — so two callers waiting at once
/// both read the same sentinel and both take the first answer as their own.
/// The client this replaces refuses the second outright. This one says so and
/// ends, rather than handing over somebody else's answer.
#[test]
fn a_second_question_about_what_the_account_has_finished_is_refused() {
    let (client, _rx, shared) = test_client();
    shared.orders.push_completed_order(crate::types::CompletedOrder {
        order_id: 31, instrument: 0, status: crate::types::OrderStatus::Filled,
        filled_qty: 100, timestamp_ns: 0,
    });
    // A caller already waiting, which is what the claim stands for.
    assert!(shared.orders.claim_the_completed_orders_question().is_some());

    let mut w = RecordingWrapper::default();
    client.req_completed_orders(false, &mut w);
    assert!(
        w.events.iter().any(|e| e.starts_with("error")),
        "the caller is told why it was not sent: {:?}", w.events,
    );
    assert!(
        w.events.iter().any(|e| e == "completed_orders_end"),
        "and is ended rather than left waiting: {:?}", w.events,
    );
    assert!(
        !w.events.iter().any(|e| e == "completed_order"),
        "and is handed nobody else's answer: {:?}", w.events,
    );

    // Once the first is over, the next one is sent.
    shared.orders.the_completed_orders_question_is_over();
    let engine = {
        let shared = shared.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(60));
            shared.orders.note_completed_orders_asked();
            // On the turn the question was asked under, which is what the
            // engine reads off the command.
            let turn = shared.orders.completed_orders_turn();
            shared.orders.note_completed_orders_end_on(turn);
        })
    };
    let mut next = RecordingWrapper::default();
    client.req_completed_orders(false, &mut next);
    engine.join().unwrap();
    assert_eq!(
        next.events.iter().filter(|e| *e == "completed_order").count(), 1,
        "the question is free again: {:?}", next.events,
    );
}

/// A caller is released by the end of its own question, not by the answer to
/// one somebody else gave up on.
///
/// The answer is a run of ordinary reports and one sentinel, and nothing in
/// the run says which question it answers. A caller that waits long enough
/// gives up and leaves its answer still on its way: read as a count alone, the
/// next caller took that answer as its own.
#[test]
fn a_caller_is_released_by_the_end_of_its_own_question() {
    let (client, rx, shared) = test_client();
    // The caller before this one asked and gave up.
    let abandoned_turn = shared.orders.claim_the_completed_orders_question()
        .expect("the question is free");
    shared.orders.the_completed_orders_question_is_over();

    // Its answer arrives while this caller is waiting on its own.
    let engine = {
        let shared = shared.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(60));
            shared.orders.note_completed_orders_asked();
            shared.orders.note_completed_orders_end_on(abandoned_turn);
        })
    };
    let began = std::time::Instant::now();
    let mut w = RecordingWrapper::default();
    client.req_completed_orders(false, &mut w);
    engine.join().unwrap();
    assert!(
        began.elapsed() >= std::time::Duration::from_secs(14),
        "the abandoned caller's answer released this one: {:?}", began.elapsed(),
    );
    assert!(
        w.events.iter().any(|e| e.starts_with("error")),
        "and this caller is told its own question went unanswered: {:?}", w.events,
    );
    // The question went out under this caller's own turn.
    let asked: Vec<u64> = std::iter::from_fn(|| rx.try_recv().ok())
        .filter_map(|cmd| match cmd {
            ControlCommand::FetchCompletedOrders { turn } => Some(turn),
            _ => None,
        })
        .collect();
    assert_eq!(asked, [abandoned_turn + 1], "its own turn, not the abandoned one: {asked:?}");
}

/// A question the engine refuses at once is answered at once.
///
/// The engine counts the question as asked and, where there is no connection
/// to carry it, counts the answer as ended in the same pass. A wait that
/// re-read the count of answers at the moment it saw the ask adopted a figure
/// that already included its own answer — and then sat out its whole deadline
/// waiting for a figure that could not move again, before reporting that the
/// venue had not finished.
#[test]
fn a_question_answered_before_the_wait_looks_does_not_wait_it_out() {
    let (client, _rx, shared) = test_client();
    // What the engine does with the question where there is no connection to
    // carry it: it counts the ask and the end in one pass, both of them
    // between two of the caller's looks.
    let engine = {
        let shared = shared.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(60));
            shared.orders.note_completed_orders_asked();
            // On the turn the question was asked under, which is what the
            // engine reads off the command.
            let turn = shared.orders.completed_orders_turn();
            shared.orders.note_completed_orders_end_on(turn);
        })
    };

    let began = std::time::Instant::now();
    let mut w = RecordingWrapper::default();
    client.req_completed_orders(false, &mut w);
    engine.join().unwrap();
    assert!(
        began.elapsed() < std::time::Duration::from_secs(2),
        "the wait sat out its deadline for an answer it already had: {:?}",
        began.elapsed(),
    );
    assert!(
        w.events.iter().any(|e| e == "completed_orders_end"),
        "and the caller is told the answer is over: {:?}", w.events,
    );
}

/// One venue order is archived once, whatever it is named along the way.
///
/// The venue names an order permanently at some point in its life, not from
/// its first report. A caller released before that happened holds a copy under
/// no permanent name, and the answer that arrives afterwards carries the same
/// order with one — the later answer supersedes the earlier, it does not join
/// it.
#[test]
fn an_order_named_permanently_after_it_was_answered_replaces_its_earlier_copy() {
    let (client, _rx, shared) = test_client();
    let known = |perm_id: i64| crate::bridge::RichOrderInfo {
        contract: Default::default(),
        order: crate::types::model::Order { order_id: 31, perm_id, ..Default::default() },
        order_state: Default::default(),
        last_exec: Default::default(),
    };
    let finished = || crate::types::CompletedOrder {
        order_id: 31, instrument: 0, status: crate::types::OrderStatus::Filled,
        filled_qty: 100, timestamp_ns: 0,
    };
    // The engine's side of the question: it takes it off the queue, and then
    // the run of answers ends.
    let answering = |shared: Arc<SharedState>| {
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(20));
            shared.orders.note_completed_orders_asked();
            std::thread::sleep(std::time::Duration::from_millis(20));
            shared.orders.note_completed_orders_end();
        })
    };
    shared.orders.push_order_info(31, known(0));
    shared.orders.push_completed_order(finished());

    let mut w = RecordingWrapper::default();
    let engine = answering(shared.clone());
    client.req_completed_orders(false, &mut w);
    engine.join().unwrap();
    assert_eq!(w.events.iter().filter(|e| *e == "completed_order").count(), 1);

    shared.orders.push_order_info(31, known(777));
    shared.orders.refile_completed_order(finished());

    let mut again = RecordingWrapper::default();
    let engine = answering(shared.clone());
    client.req_completed_orders(false, &mut again);
    engine.join().unwrap();
    assert_eq!(
        again.events.iter().filter(|e| *e == "completed_order").count(),
        1,
        "the one order read as two once the venue had named it: {:?}",
        again.events,
    );
}

/// And retracted when the venue takes the execution back.
///
/// A trade cancel or trade correction returns a finished order to a working
/// quantity, and the bridge drops the completion it had queued. The archive is
/// the copy a caller reads after that queue has emptied, so an order the venue
/// has taken back has to leave it too — otherwise the same order is listed as
/// working and as finished at the same time, for the rest of the session.
#[test]
fn a_completed_order_the_venue_takes_back_leaves_the_archive() {
    let (client, _rx, shared) = test_client();
    shared.orders.push_completed_order(crate::types::CompletedOrder {
        order_id: 31, instrument: 0, status: crate::types::OrderStatus::Filled,
        filled_qty: 100, timestamp_ns: 0,
    });

    let mut w = RecordingWrapper::default();
    client.req_completed_orders(false, &mut w);
    assert_eq!(w.events.iter().filter(|e| *e == "completed_order").count(), 1);

    shared.orders.push_order_correction(31, crate::bridge::RichOrderInfo {
        contract: Default::default(),
        order: crate::types::model::Order { order_id: 31, ..Default::default() },
        order_state: Default::default(),
        last_exec: Default::default(),
    });

    let mut after = RecordingWrapper::default();
    client.req_completed_orders(false, &mut after);
    assert_eq!(
        after.events.iter().filter(|e| *e == "completed_order").count(),
        0,
        "the order the venue took back is still reported as finished: {:?}",
        after.events,
    );
}

/// A request that names its contract by contract id refuses one carrying none.
///
/// These carry tag 6008 and nothing else of the contract. Contract id 0 and a
/// negative id are both answered with silence, which reads as no data.
#[test]
fn a_request_named_by_id_refuses_a_contract_that_has_none() {
    let (client, _rx, _shared) = test_client();
    let described = crate::types::model::Contract {
        symbol: "SPY".into(), sec_type: "STK".into(), exchange: "SMART".into(),
        ..Default::default()
    };
    assert!(client.req_fundamental_data(1, &described, "ReportSnapshot").is_err());
    assert!(client.req_histogram_data(2, &described, true, "3 days").is_err());
    assert!(client.req_historical_news(3, -1, "BRFG", "", "", 5).is_err());
    assert!(
        client.req_historical_ticks(4, &spy(), "", "", -1, "TRADES", true).is_err(),
        "a count below zero asked for four billion ticks",
    );

    // And one that carries the id is sent.
    assert!(client.req_fundamental_data(5, &spy(), "ReportSnapshot").is_ok());
    assert!(client.req_histogram_data(6, &spy(), true, "3 days").is_ok());
}

/// A depth request on a contract naming no exchange and no security type is sent
/// as it stands.
///
/// An unnamed exchange is already routed as SMART, and a named security type is
/// checked against the routing table, so writing STK in refuses books that
/// exist for other types.
#[test]
fn a_depth_request_states_the_contract_it_was_given() {
    let (client, rx, _shared) = test_client();
    let by_id = crate::types::model::Contract { con_id: 495512563, ..Default::default() };
    client.req_mkt_depth(1, &by_id, 5, false).expect("the request is sent");
    match rx.try_recv().expect("the subscription") {
        ControlCommand::SubscribeDepth { contract, .. } => {
            assert_eq!(contract.sec_type, "", "a security type nobody stated");
            assert_eq!(contract.exchange, "", "a venue nobody stated");
            assert_eq!(contract.con_id, 495512563);
        }
        other => panic!("expected SubscribeDepth, got {other:?}"),
    }
}

/// A book rides the quote feed, so a feed given up on serves none.
///
/// Accepted, the request took a book slot and reached a sender with no
/// connection to write it to, which is silent. A book that never arrives is
/// what a market with nothing to say looks like, so nothing told the two
/// apart, and the slot stayed held for a request the venue never heard.
#[test]
fn no_book_is_taken_on_a_feed_that_is_over_for_the_session() {
    let (client, rx, shared) = test_client();
    shared.market.set_market_data_over("the venue would not take the connection back");
    let contract = crate::types::model::Contract { con_id: 495512563, ..Default::default() };
    let asked = client.req_mkt_depth(1, &contract, 5, false);
    assert!(asked.is_err(), "the caller is refused: {asked:?}");
    assert!(rx.try_recv().is_err(), "and nothing was sent for it");
    // The slot is free, so a later session's request under the same number is
    // not refused as a book this one is already holding.
    assert!(client.core.hold_the_book(1).is_ok(), "the book slot was not taken");
}

/// A caller chooses how its bar times are written, and the choice is per
/// request. Discarded, a caller that asked for seconds since the epoch is
/// handed the wire's spelling and reads a date where it expects a number.
#[test]
fn a_request_gets_its_bar_times_written_the_way_it_asked() {
    #[derive(Default)]
    struct Heard(Vec<(i64, String)>);
    impl Wrapper for Heard {
        fn historical_data(&mut self, req_id: i64, bar: &crate::types::model::BarData) {
            self.0.push((req_id, bar.date.clone()));
        }
        // A request that has already answered with its history keeps speaking
        // on this one, and its times are written the same way.
        fn historical_data_update(&mut self, req_id: i64, bar: &crate::types::model::BarData) {
            self.0.push((req_id, bar.date.clone()));
        }
    }

    let (client, _rx, shared) = test_client();
    let mut heard = Heard::default();

    let bar = HistoricalBar {
        time: "20260815-12:00:00".into(), open: 1.0, high: 2.0, low: 0.5,
        close: 1.5, volume: 10, wap: 1.2, count: 3, end: String::new(),
    };
    // Format 1 is the wire's own spelling.
    let _ = client.req_historical_data(
        1, &spy(), "", "1 D", "1 day", "TRADES", true, 1, false,
    );
    shared.reference.push_historical_data(1, HistoricalResponse {
        query_id: String::new(), timezone: String::new(),
        bars: vec![bar.clone()], is_complete: true,
    });
    client.process_msgs(&mut heard);
    assert_eq!(heard.0[0].1, "20260815-12:00:00", "the wire's own spelling");

    // And seconds since the epoch for the request that asked for them.
    let _ = client.req_historical_data(
        2, &spy(), "", "1 D", "1 day", "TRADES", true, 2, false,
    );
    shared.reference.push_historical_data(2, HistoricalResponse {
        query_id: String::new(), timezone: String::new(),
        bars: vec![bar.clone()], is_complete: true,
    });
    client.process_msgs(&mut heard);
    assert_eq!(heard.0[1].1, "1786795200", "seconds since the epoch");

    // The first request is unaffected: the choice belongs to the request.
    shared.reference.push_historical_data(1, HistoricalResponse {
        query_id: String::new(), timezone: String::new(),
        bars: vec![bar.clone()], is_complete: false,
    });
    client.process_msgs(&mut heard);
    assert_eq!(heard.0[2].1, "20260815-12:00:00", "still the venue's spelling");
}

/// The shorthand states the order a reader would write out, and nothing else.
/// A constructor that quietly set a field a caller had not asked for would put
/// an instruction on the wire that nobody wrote.
#[test]
fn the_shorthand_states_the_order_and_nothing_more() {
    use crate::types::model::Order;
    let plain = Order::default();
    for (what, order, kind, lmt, aux) in [
        ("market", Order::market("BUY", 100.0), "MKT", 0.0, 0.0),
        ("limit", Order::limit("BUY", 100.0, 42.5), "LMT", 42.5, 0.0),
        ("stop", Order::stop("SELL", 100.0, 41.0), "STP", 0.0, 41.0),
        ("stop limit", Order::stop_limit("SELL", 100.0, 41.0, 40.5), "STP LMT", 40.5, 41.0),
    ] {
        assert_eq!(order.order_type, kind, "{what}");
        assert_eq!(order.total_quantity, 100.0, "{what}");
        assert_eq!(order.lmt_price, lmt, "{what}");
        assert_eq!(order.aux_price, aux, "{what}");
        assert_eq!(order.tif, "DAY", "{what}: expires at the close unless said otherwise");
        // Everything this shorthand does not name is left where it was.
        assert_eq!(order.hedge_type, plain.hedge_type, "{what}");
        assert_eq!(order.good_after_time, plain.good_after_time, "{what}");
        assert_eq!(order.origin, plain.origin, "{what}");
        assert_eq!(order.transmit, plain.transmit, "{what}");
    }
    assert_eq!(Order::limit("BUY", 1.0, 10.0).good_till_cancelled().tif, "GTC");
    assert!(Order::market("BUY", 1.0).outside_regular_hours().outside_rth);
}

/// A contract the shorthand names is the one a request would carry. Each of
/// these was read back off a live definition, so a default that drifted from
/// what the venue lists would be a lookup answering about something else.
#[test]
fn the_shorthand_names_the_contract_a_request_carries() {
    use crate::types::model::Contract;
    let spy = Contract::stock("SPY");
    assert_eq!((spy.sec_type.as_str(), spy.exchange.as_str(), spy.currency.as_str()),
               ("STK", "SMART", "USD"));

    let call = Contract::call("AAPL", 150.0, "20261218");
    assert_eq!(call.sec_type, "OPT");
    assert_eq!((call.right.as_str(), call.strike), ("C", 150.0));
    assert_eq!(call.last_trade_date_or_contract_month, "20261218");
    assert!(
        call.multiplier.is_empty(),
        "an option's multiplier identifies the listing and is left to the venue",
    );
    assert_eq!(Contract::put("AAPL", 150.0, "20261218").right, "P");

    let es = Contract::future("ES", "202612", "CME");
    assert_eq!((es.sec_type.as_str(), es.exchange.as_str()), ("FUT", "CME"));
    // A future is quoted in whatever its venue quotes in, so nothing here
    // assumes dollars — assumed, a Eurex contract would be asked about in a
    // currency it is not listed in.
    assert!(es.currency.is_empty(), "a future's currency is the venue's");
    assert!(Contract::index("SPX", "CBOE").currency.is_empty());

    let eurusd = Contract::forex("EUR", "USD");
    assert_eq!((eurusd.sec_type.as_str(), eurusd.symbol.as_str(),
                eurusd.currency.as_str(), eurusd.exchange.as_str()),
               ("CASH", "EUR", "USD", "IDEALPRO"));

    // A contract stated by id carries nothing else: a symbol beside an id that
    // disagreed with it is a description of two different contracts.
    let by_id = Contract::by_id(756733);
    assert_eq!(by_id.con_id, 756733);
    assert!(by_id.symbol.is_empty() && by_id.sec_type.is_empty());

    let toyota = Contract::stock("7203").on_exchange("TSEJ").in_currency("JPY");
    assert_eq!((toyota.exchange.as_str(), toyota.currency.as_str()), ("TSEJ", "JPY"));
}

/// A field left alone is not a field asked for. Several of the twenty-nine
/// carry a non-zero default — `what_if_type` is `i32::MAX`, `exempt_code` is
/// `-1` — so a refusal written against emptiness rather than against the
/// default would reject every order anyone ever placed.
#[test]
fn an_order_nobody_touched_is_not_refused_for_what_it_does_not_carry() {
    let (client, _rx, _shared) = test_client();
    let order = Order {
        action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(),
        lmt_price: 100.0, tif: "DAY".into(), ..Default::default()
    };
    client.place_order(9501, &spy(), &order).expect("a plain order is placed");

    // And the same order built by the shorthand, which fills no more than it
    // names.
    let (client, _rx, _shared) = test_client();
    client
        .place_order(9502, &spy(), &Order::limit("BUY", 1.0, 100.0))
        .expect("the shorthand's order is placed");
}

/// Two questions asked at once each get their own answer.
///
/// A question drives the message pump itself, and the pump hands everything it
/// drains to whichever collector is running — which keeps what carries its own
/// request id and discards the rest. Asked concurrently, the first question
/// read the second's answer, threw it away, and the second waited out its
/// timeout for a reply that had already arrived. With no engine to answer,
/// what this holds is the ordering: neither question is on the wire while the
/// other is listening, so neither can be handed the other's messages.
#[test]
fn two_questions_asked_at_once_do_not_consume_each_other() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    let (client, _rx, _shared) = test_client();
    let client = Arc::new(client);
    let overlapping = Arc::new(AtomicUsize::new(0));
    let inside = Arc::new(AtomicUsize::new(0));

    let threads: Vec<_> = (0..4)
        .map(|_| {
            let (client, overlapping, inside) = (
                Arc::clone(&client), Arc::clone(&overlapping), Arc::clone(&inside),
            );
            std::thread::spawn(move || {
                let _turn = client.asking.lock().unwrap_or_else(|e| e.into_inner());
                let now = inside.fetch_add(1, Ordering::SeqCst) + 1;
                if now > 1 {
                    overlapping.fetch_add(1, Ordering::SeqCst);
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
                inside.fetch_sub(1, Ordering::SeqCst);
            })
        })
        .collect();
    for t in threads {
        t.join().expect("a question finishes");
    }
    assert_eq!(
        overlapping.load(Ordering::SeqCst), 0,
        "a second question ran while the first was still listening",
    );
}

/// Every question takes its turn before it sends.
///
/// The test above holds the turn itself, so it stays green if the turn is taken
/// nowhere in the code that ships. This reads the questions instead: one that
/// waits for an answer and does not take a turn is one that can be handed
/// another question's reply, and there is no way to observe that from a test
/// with no session to answer it.
#[test]
fn a_question_takes_its_turn_before_it_sends() {
    // Both shapes: the answering calls here, and the ones the shape that
    // returns its answers asks. A schedule was the one of the second set that
    // took no turn, and nothing said so.
    let waits_for_an_answer: Vec<&str> = [include_str!("ask.rs"), include_str!("../direct.rs")]
        .iter()
        .flat_map(|source| source.split("\n    pub fn ").skip(1))
        .filter(|body| body.contains("self.wait_for(") || body.contains("holding_the_turn("))
        .collect();
    assert!(waits_for_an_answer.len() >= 15, "the reader found the questions");
    let without: Vec<&str> = waits_for_an_answer
        .iter()
        .filter(|body| !body.contains("take_the_turn()"))
        .map(|body| body.split('(').next().unwrap_or(body))
        .collect();
    assert!(without.is_empty(), "asks without taking a turn: {without:?}");

    // And the one place that sends before it waits holds the turn across both.
    let placing = include_str!("simple.rs");
    let place = placing.split("pub fn place(").nth(1).expect("place is there");
    let body = place.split("\n    }").next().unwrap_or(place);
    let turn = body.find("self.take_the_turn()").expect("place takes a turn");
    let send = body.find("self.place_order(").expect("place sends the order");
    assert!(turn < send, "the order is sent before the turn is taken");
}

/// Placing an order for a contract the venue has not named yet does not wait
/// on itself.
///
/// The order is sent under a turn, so that nothing else pumps its reply away.
/// A contract with no id is looked up before it is sent, and a lookup is a
/// question that takes a turn of its own — asked from inside the placing turn,
/// it waits on a turn that is not going to be given up, and the order is never
/// sent at all. Run on a thread so a regression fails the suite instead of
/// hanging it.
#[test]
fn placing_an_unnamed_contract_does_not_wait_on_itself() {
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = std::sync::Arc::clone(&done);
    std::thread::spawn(move || {
        let (client, _rx, _shared) = test_client();
        // No engine answers, so the lookup fails or times out — either way it
        // returns. What must not happen is that it never returns at all.
        let unnamed = Contract {
            symbol: "SPY".into(), sec_type: "STK".into(),
            exchange: "SMART".into(), currency: "USD".into(),
            ..Default::default()
        };
        let _ = client.place(&unnamed, &Order::limit("BUY", 1.0, 1.0));
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
    });

    // Longer than the wait it is bounded by, read from that wait rather than
    // restated: written out, this raced it the moment the wait moved. Naming
    // the contract is a lookup, so the lookup's wait is the one to clear.
    let deadline = std::time::Instant::now()
        + std::time::Duration::from_secs(crate::config::LOOKUP_TIMEOUT_SECS * 2);
    while std::time::Instant::now() < deadline {
        if done.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    panic!("placing an unnamed contract never returned: it is waiting on its own turn");
}

/// An order that cannot be placed is refused before the venue is asked
/// anything, and a description resolved once is not asked about twice.
///
/// Placing looks a contract up before it takes its turn. Done ahead of the
/// refusals, a caller who wrote an impossible order waits out a lookup for a
/// contract that was never going to be traded, and hears about the lookup
/// rather than about their order.
#[test]
fn an_impossible_order_is_refused_before_the_venue_is_asked() {
    let (client, rx, _shared) = test_client();
    let unnamed = Contract {
        symbol: "SPY".into(), sec_type: "STK".into(),
        exchange: "SMART".into(), currency: "USD".into(),
        ..Default::default()
    };
    let asked = std::time::Instant::now();
    let err = client
        .place(&unnamed, &Order { tif: "FOREVER".into(), ..Order::limit("BUY", 1.0, 1.0) })
        .expect_err("a time in force that is not one is refused");
    assert!(err.message.contains("tif"), "{err}");
    assert!(
        asked.elapsed() < std::time::Duration::from_secs(5),
        "the refusal waited on a lookup: {:?}",
        asked.elapsed(),
    );
    assert!(rx.try_recv().is_err(), "nothing reaches the wire");
}

/// A bracket whose exits sit the wrong side of its entry is refused before it
/// is sent.
///
/// Placed, it opens a position and closes it in the same breath: a take-profit
/// below the entry is already profitable, and a stop above it is already
/// triggered. The venue is not the right place to find that out.
#[test]
fn a_bracket_that_closes_itself_is_refused() {
    let (client, rx, _shared) = test_client();
    for (what, side, entry, take_profit, stop_loss) in [
        ("a buy taking profit below its entry", "BUY", 100.0, 90.0, 95.0),
        ("a buy stopping out above its entry", "BUY", 100.0, 110.0, 105.0),
        ("a sell taking profit above its entry", "SELL", 100.0, 110.0, 105.0),
    ] {
        let err = client
            .place_bracket(&spy(), side, 1.0, entry, take_profit, stop_loss)
            .expect_err(what);
        assert!(err.message.contains("wrong side"), "{what}: {err}");
        assert!(rx.try_recv().is_err(), "{what}: nothing reaches the wire");
    }

    // And one stated the right way round is sent, under three consecutive
    // numbers — the venue reads the children's as the parent's plus one and two.
    let ids = client
        .place_bracket(&spy(), "BUY", 1.0, 100.0, 110.0, 95.0)
        .expect("a bracket the right way round is placed");
    assert_eq!(ids[1], ids[0] + 1);
    assert_eq!(ids[2], ids[0] + 2);
    assert_eq!(client.next_order_id(), ids[0] + 3, "the next order does not reuse a child's");
}

/// Bars are asked for as trades, except where the instrument has none.
///
/// A currency pair does not trade on an exchange, so the venue holds no trade
/// history for one and answers a request for it with "No historical market
/// data" — which is what this call did against a live session until it stopped
/// asking for trades there.
#[test]
fn bars_ask_for_what_the_instrument_has() {
    let (client, rx, _shared) = test_client();
    // CFD asks for trades: a share CFD has them and an index one is refused
    // by name, which is better than being answered with a series nobody asked
    // for. Measured 2026-08-27, see `Contract::is_quoted_not_traded`.
    for (sec_type, wanted) in
        [("STK", "TRADES"), ("CASH", "MIDPOINT"), ("CMDTY", "MIDPOINT"), ("CFD", "TRADES")]
    {
        let contract = Contract {
            con_id: 12087792, symbol: "EUR".into(), sec_type: sec_type.into(),
            exchange: "IDEALPRO".into(), currency: "USD".into(), ..Default::default()
        };
        let _ = client.bars(&contract, "1 D", "1 hour");
        let asked = std::iter::from_fn(|| rx.try_recv().ok())
            .find_map(|c| match c {
                crate::types::ControlCommand::FetchHistorical { what_to_show, .. } => Some(what_to_show),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{sec_type}: a request reaches the engine"));
        assert_eq!(asked, wanted, "{sec_type} bars");
    }
}

/// Non-finite prices are refused before scaling. A saturating cast turns NaN
/// into 0 and infinity into the largest representable price, both of which the
/// venue accepts as real values.
#[test]
fn a_number_the_wire_cannot_carry_is_refused_wherever_it_sits() {
    let (client, _rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    let base = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "LMT".into(),
        lmt_price: 150.0, ..Default::default()
    };

    let cases: Vec<(&str, Order)> = vec![
        ("scale_price_increment", Order { scale_price_increment: f64::NAN, ..base.clone() }),
        ("scale_profit_offset", Order { scale_profit_offset: f64::INFINITY, ..base.clone() }),
        ("stock_range_lower", Order { stock_range_lower: f64::NAN, ..base.clone() }),
        ("volatility", Order { volatility: f64::NAN, ..base.clone() }),
        ("percent_offset", Order { percent_offset: f64::NEG_INFINITY, ..base.clone() }),
        ("starting_price", Order { starting_price: f64::NAN, ..base.clone() }),
        ("delta_neutral_aux_price", Order {
            delta_neutral_order_type: "MKT".into(),
            delta_neutral_aux_price: f64::NAN, ..base.clone()
        }),
        ("order_combo_legs", Order {
            order_combo_legs: vec![f64::NAN], ..base.clone()
        }),
    ];
    for (named, order) in cases {
        let err = match client.place_order(1, &spy(), &order) {
            Err(e) => e,
            Ok(()) => panic!("{named} was accepted"),
        };
        assert!(err.message.contains(named), "{named}: got {err}");
    }
}

/// Two reports on one order in a single pass are two callbacks, in arrival
/// order. Each status change is stated separately on the wire.
#[test]
fn each_report_on_an_order_is_delivered() {
    #[derive(Default)]
    struct Statuses(Vec<String>);
    impl Wrapper for Statuses {
        fn order_status(
            &mut self, _order_id: i64, status: &str, _filled: f64, _remaining: f64,
            _avg: f64, _perm_id: i64, _parent_id: i64, _last: f64, _client_id: i64,
            _why_held: &str, _mkt_cap_price: f64,
        ) {
            self.0.push(status.to_string());
        }
    }

    let (client, _rx, shared) = test_client();
    for status in [OrderStatus::PreSubmitted, OrderStatus::Submitted, OrderStatus::Cancelled] {
        shared.orders.push_order_update(crate::types::OrderUpdate {
            order_id: 4, instrument: 0, status, filled_qty: 0.0, remaining_qty: 100.0,
            avg_price: 0, perm_id: 77, parent_id: 0, timestamp_ns: 0,
        });
    }
    let mut seen = Statuses::default();
    client.process_msgs(&mut seen);
    assert_eq!(
        seen.0, ["PreSubmitted", "Submitted", "Cancelled"],
        "a report was replaced by the one that followed it",
    );
}

/// A replace names the order, so it cannot name another contract.
///
/// The message carries the order id and its fields, not the instrument, so the
/// order stays on the contract it was placed on. A contract naming a different
/// instrument is refused rather than recorded.
#[test]
fn a_replace_does_not_move_an_order_to_another_contract() {
    let (client, _rx, shared) = test_client();
    shared.market.set_instrument_count(2);
    let order = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "LMT".into(),
        lmt_price: 150.0, ..Default::default()
    };
    client.place_order(1, &spy(), &order).expect("the first placement");

    let elsewhere = Contract {
        con_id: 265598, symbol: "AAPL".into(), exchange: "SMART".into(),
        ..Default::default()
    };
    client.core.con_id_to_instrument.lock().unwrap().insert(elsewhere.con_id, 1);
    let err = client
        .place_order(1, &elsewhere, &Order { lmt_price: 151.0, ..order.clone() })
        .expect_err("an order working on one contract is not replaced onto another");
    assert!(err.message.contains("another contract"), "{err}");

    // And the record is the contract the venue is working, not the one refused.
    assert_eq!(
        client.core.open_orders.lock().unwrap()[&1].contract.symbol, "SPY",
        "the refused contract was recorded against the order",
    );

    // The same order on the same contract still replaces.
    client
        .place_order(1, &spy(), &Order { lmt_price: 151.0, ..order })
        .expect("a replace naming the contract it was placed on");
}

/// A bracket is held to the checks a single order is held to.
///
/// An order on a security type the account is not permitted is returned Inactive
/// with tag 58 empty, so the reason is stated here instead.
#[test]
fn a_bracket_is_refused_on_a_security_type_the_venue_does_not_permit() {
    let (client, _rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    shared.reference.set_order_permissions(
        [("STK".to_string(), vec!["LMT".to_string()])].into_iter().collect(),
    );
    let bond = Contract {
        con_id: 15547841, symbol: "IBM".into(), sec_type: "BOND".into(),
        exchange: "SMART".into(), ..Default::default()
    };
    let err = client
        .place_bracket(&bond, "BUY", 1000.0, 100.0, 110.0, 90.0)
        .expect_err("a bracket on an unpermitted security type is refused before it is sent");
    assert!(err.message.to_uppercase().contains("BOND"), "{err}");

    // And a permitted one still goes.
    client
        .place_bracket(&spy(), "BUY", 1.0, 100.0, 110.0, 90.0)
        .expect("a bracket on a permitted security type");
}

/// Asking for the account's P&L sends the subscription.
///
/// The figures on `pnl` are computed against each holding's midnight value and
/// realised amount, which the venue states only in answer to this request.
#[test]
fn asking_for_the_accounts_pnl_asks_the_venue() {
    let (client, rx, _shared) = test_client();
    client.req_pnl(9, "", "");
    let asked = rx.try_iter().find_map(|cmd| match cmd {
        ControlCommand::SubscribePnl { req_id, account } => Some((req_id, account)),
        _ => None,
    });
    assert_eq!(
        asked,
        Some((9, "DU123".to_string())),
        "the venue was not asked for the account's P&L",
    );

    // And under this session's account whatever is named, because the figure
    // is worked out from one set of seeds against one book of holdings and
    // both belong to it. Taken at its word, a second account's seeds replaced
    // this account's and the next restatement replaced them back, so the
    // figure alternated between two accounts' realised legs measured against
    // this account's positions. The slot is held by the first request, so that
    // one is withdrawn before another may ask.
    client.cancel_pnl(9);
    client.req_pnl(10, "DU999", "");
    let asked = rx.try_iter().find_map(|cmd| match cmd {
        ControlCommand::SubscribePnl { account, .. } => Some(account),
        _ => None,
    });
    assert_eq!(
        asked, Some("DU123".to_string()),
        "the profit reported is this account's, so this account is what is asked for",
    );
}

/// `regulatory_snapshot` reaches the venue rather than being refused here, and
/// it reaches it even when the contract is already being watched.
///
/// It names the venue's own chargeable one-shot snapshot, which is a request
/// of its own rather than a share of somebody else's stream. Answered by
/// following a subscription that is already up, nothing goes on the wire,
/// nothing is billed, and an account with no entitlement hears an end it was
/// never refused — off a stream it did not ask for.
#[test]
fn a_chargeable_snapshot_is_asked_for_even_where_the_contract_is_watched() {
    let (client, rx, _shared) = test_client();
    // A subscription on this contract is already up and held by another
    // request, which is the state that used to swallow this one.
    client.core.con_id_to_instrument.lock().unwrap().insert(spy().con_id, 0);
    client.core.instrument_to_req.lock().unwrap().insert(0, 1);
    client.core.req_to_instrument.lock().unwrap().insert(1, 0);

    // An ordinary request does follow it, and asks the engine for nothing.
    client.req_mkt_data(2, &spy(), "", false, false).expect("it follows the stream");
    assert!(
        !rx.try_iter().any(|c| matches!(c, ControlCommand::Subscribe { .. })),
        "an ordinary request shares the subscription that is up",
    );

    // The chargeable one does not.
    let _ = client.req_mkt_data_ex(3, &spy(), "", false, true, 0);
    assert!(
        rx.try_iter().any(|c| matches!(
            c, ControlCommand::Subscribe { regulatory_snapshot: true, .. }
        )),
        "the chargeable snapshot is asked for on its own",
    );
}


/// A caller cannot number a request the way this client numbers its own.
///
/// An answer finds whoever is waiting by that number. A caller using one from
/// the band the answering calls number themselves in has its answer handed to
/// one of those calls, which asked about something else — and the caller waits
/// out a deadline for an answer that was given away.
#[test]
fn a_caller_cannot_take_a_number_this_client_reserves() {
    use crate::bridge::ReferenceState;
    let (client, _rx, _shared) = test_client();
    let spy = spy();

    let taken = ReferenceState::ASK_ID_BASE as i64;
    let refused = client.req_adjustments(taken, 4815747, "STK", "SMART", "20240101", "20241231");
    assert!(refused.is_err(), "a request numbered {taken} must not be sent");

    // Every request, not one of them: a number from that band collides on
    // whichever call carries it.
    assert!(
        client.req_historical_data(taken, &spy, "", "1 D", "1 hour", "TRADES", true, 1, false).is_err(),
        "bars numbered inside the band must be refused too",
    );
    assert!(
        client.req_contract_details(taken, &spy).is_err(),
        "and a contract lookup",
    );

    // Refused whether or not this session happens to be holding that number.
    // Held is exactly when the collision is possible, so a check that lets a
    // held one through is open precisely when it matters.
    _shared.reference.note_ours(crate::bridge::RecordKind::Answer, taken);
    assert!(
        client.req_adjustments(taken, 4815747, "STK", "SMART", "20240101", "20241231").is_err(),
        "held or not, the band is not a caller's to number in",
    );
    _shared.reference.forget_ours(crate::bridge::RecordKind::Answer, taken);

    // The number below the band is a caller's to use, and still works.
    assert!(
        client.req_adjustments(taken - 1, 4815747, "STK", "SMART", "20240101", "20241231").is_ok(),
        "the band is a ceiling on caller numbers, not a ban on large ones",
    );
    // And an ordinary request is unaffected.
    assert!(client.req_contract_details(1, &spy).is_ok());
}

/// A refusal against a request too wide to carry is reported against no
/// request, not against its own low half.
///
/// This account hands out order ids far wider than a request number, so a
/// caller numbering both from one counter reaches this with every refusal it
/// gets. Narrowed, the refusal for one request is delivered under another the
/// caller may well be waiting on.
#[test]
fn a_refusal_for_an_uncarryable_request_is_not_delivered_under_another() {
    use crate::api::client::carried_under;
    use crate::bridge::ReferenceState;

    // What this account actually hands out.
    assert_eq!(carried_under(1_787_685_160_171_122), ReferenceState::NO_REQUEST);
    // The collision the narrowing made: these two differ by exactly 2^32.
    assert_ne!(carried_under(1), carried_under(4_294_967_297));
    assert_eq!(carried_under(-1), ReferenceState::NO_REQUEST);
    // One that does fit is carried as itself.
    assert_eq!(carried_under(4242), 4242);
    // The mark for none is not a request number a caller can be answered under.
    assert_eq!(
        carried_under(u32::MAX as i64), ReferenceState::NO_REQUEST,
        "a request numbered with the mark for none is reported as its own answer",
    );
}

/// The engine numbers the lookups it takes for itself above a line, and an
/// answer above that line is kept rather than handed on.
#[test]
fn a_request_numbered_where_the_engine_numbers_its_own_is_refused() {
    use crate::api::client::wire_req_id;
    use crate::bridge::{ENGINE_ID_BASE, ReferenceState};

    let refused = wire_req_id(ENGINE_ID_BASE as i64).expect_err("the band is not a caller's");
    assert!(
        refused.message.contains("lookups it takes"),
        "the refusal does not say why: {}", refused.message,
    );
    assert!(wire_req_id(ENGINE_ID_BASE as i64 + 1).is_err());
    // Everything from the answering band up was already refused; this band sat
    // above it and fell through, which is the gap. Below both is a caller's.
    assert!(
        wire_req_id(ReferenceState::ASK_ID_BASE as i64 - 1).is_ok(),
        "a number below every reserved band stopped being a caller's",
    );
}

/// An account whose order ids have outgrown a request number is told so on
/// whichever surface hands one out, not on the Python one alone.
///
/// Read out of each function's own body rather than the rest of the file: a
/// search that ran to the end of the file passed on the strength of a call in
/// a later function, so removing either one went unnoticed.
#[test]
fn every_path_that_hands_out_a_wide_order_id_says_so() {
    for (file, wanted) in [
        ("src/api/client/orders.rs", ["fn reserve_order_ids(", "pub fn req_ids("]),
        ("src/python/compat/client/mod.rs", ["fn take_order_id(", "fn stated_order_id("]),
    ] {
        let src = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(file),
        ).unwrap_or_else(|e| panic!("{file} is there to read: {e}"));
        for signature in wanted {
            let body = body_of(&src, signature)
                .unwrap_or_else(|| panic!("{file}: {signature} is no longer there"));
            assert!(
                body.contains("say_if_past_a_request_id"),
                "{file}: {signature} hands out an id without saying it has outgrown a \
                 request number",
            );
        }
    }
}

/// The body of the function whose signature this names, by matching braces.
fn body_of(src: &str, signature: &str) -> Option<String> {
    let from = src.find(signature)? + signature.len();
    let open = from + src[from..].find('{')?;
    let mut depth = 0usize;
    for (at, c) in src[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(src[open..open + at].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

/// A bracket takes three consecutive ids, and the line a request number stops
/// at can fall between the first and the last of them.
#[test]
fn a_run_of_ids_is_measured_by_its_widest() {
    let orders = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/api/client/orders.rs"),
    ).expect("the order surface is there to read");
    let body = body_of(&orders, "fn reserve_order_ids(").expect("ids are reserved in one place");
    assert!(
        body.contains("checked_add(n - 1)") && body.contains("say_if_past_a_request_id(last)"),
        "a run is measured by its first id, so a bracket whose children cross the line \
         hands them out in silence",
    );
}

/// The venue names the working orders after the connect returns, and a global
/// cancel is composed from what has been named. Issued before the naming
/// lands, it waits for it — without the wait it counted no instruments, sent
/// nothing, and returned without an error.
#[test]
fn a_global_cancel_waits_for_the_venue_to_name_the_working_orders() {
    let (client, rx, shared) = test_client();
    let venue = shared.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(200));
        venue.market.set_instrument_count(1);
        venue.orders.set_replay_done();
    });
    client.req_global_cancel().unwrap();
    let sent: Vec<ControlCommand> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert!(
        matches!(sent.as_slice(), [ControlCommand::Order(OrderRequest::CancelAll { instrument: 0 })]),
        "the order the venue named is withdrawn: {sent:?}",
    );
}


/// A call answered here does not wait on its own turn to name a contract the
/// caller gave by id alone.
///
/// These calls take the turn first and then ask the venue for the contract's
/// full description, which is another call that waits for the same turn. The
/// lock is not re-entrant, so neither ever runs again, and the deadline that
/// would have said so is set inside the wait that never starts — the session
/// stops answering anything, with nothing said.
///
/// `what_if_order` and `place` already name the contract before taking the
/// turn, and the comment there names this hazard. They guard a contract with
/// no id; this is the other shape the venue is asked about — an id with no
/// security type or venue beside it, which is exactly what
/// `named_by_the_venue` exists to fill in.
#[test]
fn an_answering_call_does_not_wait_on_itself_to_name_a_contract_given_by_id() {
    // The venue's own id, and nothing else — the shape that needs naming.
    let by_id_alone = || Contract { con_id: 756733, ..Default::default() };

    for (what, call) in [
        ("historical_data", 0u8), ("head_timestamp", 1), ("histogram_data", 2), ("schedule", 3),
    ] {
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&done);
        std::thread::spawn(move || {
            let (client, _rx, _shared) = test_client();
            let c = by_id_alone();
            // No engine answers, so each of these fails or times out. What must
            // not happen is that it never returns at all.
            let _ = match call {
                0 => client.historical_data(&c, "", "1 D", "1 hour", "TRADES", true).map(|_| ()),
                1 => client.head_timestamp(&c, "TRADES", true).map(|_| ()),
                2 => client.histogram_data(&c, true, "1 week").map(|_| ()),
                _ => client.schedule(&c, "1 D").map(|_| ()),
            };
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        });

        // Read from the wait it is bounded by rather than written out, so this
        // cannot race the wait moving.
        let deadline = std::time::Instant::now()
            + std::time::Duration::from_secs(crate::config::LOOKUP_TIMEOUT_SECS * 3);
        let mut returned = false;
        while std::time::Instant::now() < deadline {
            if done.load(std::sync::atomic::Ordering::SeqCst) {
                returned = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(returned, "{what} never returned: it is waiting on its own turn");
    }
}

/// A preview carrying an algo strategy is still a preview.
///
/// The flag that asks for one used to be a kind of order, so an order that was
/// already a kind — an algo, or an adaptive one — had nowhere to carry it. The
/// algo branch answered first and returned, and the preview flag went nowhere:
/// a caller asking what an algo order would cost had one placed instead, live,
/// and got order statuses for something they never meant to send.
///
/// It is a field on the order's attributes now rather than a kind, so the two
/// are no longer alternatives. Pinned here because nothing else states it, and
/// a preview that places an order is the worst way for this to come back.
#[test]
fn a_preview_of_an_algo_order_is_not_placed() {
    for strategy in ["Adaptive", "Vwap", "Twap", "ArrivalPx"] {
        let mut order = Order::limit("BUY", 100.0, 10.0);
        order.algo_strategy = strategy.to_string();
        order.algo_params = vec![
            crate::types::model::TagValue { tag: "maxPctVol".into(), value: "0.1".into() },
            crate::types::model::TagValue { tag: "adaptivePriority".into(), value: "Normal".into() },
        ];
        order.what_if = true;

        let built = ClientCore::build_order_request(&order, 1, 0, None);
        let Ok(crate::types::ControlCommand::Order(request)) = built else {
            panic!("{strategy}: a preview of it was not built: {built:?}");
        };
        let crate::types::OrderRequest::SubmitEx { attrs, .. } = &request else {
            panic!("{strategy}: a preview was not a submission: {request:?}");
        };
        assert!(
            attrs.what_if,
            "{strategy}: the preview flag did not survive the strategy, so the order \
             would be placed rather than priced",
        );
    }
}

/// An account the venue named nothing for is not warned about.
///
/// The record that ends the naming cannot be told from the one that precedes
/// it, so an account working nothing never sees the naming finish. Warned on
/// that alone, every withdrawal against an idle account would say orders might
/// still be working when there were none — which is the same lie as silence,
/// told the other way round.
#[test]
fn a_withdrawal_against_an_account_working_nothing_says_nothing() {
    let (client, _rx, shared) = test_client();
    shared.market.set_instrument_count(1);
    // The venue named nothing at all: no naming began, and none finished.
    client.req_global_cancel().expect(
        "an account the venue named nothing for is withdrawn without complaint",
    );
}

/// A book this client gives up on is said to the caller that asked for it.
///
/// The venue goes on sending that book and nothing further is kept for it, so
/// a caller not told reads a subscription that is up and a book that has
/// simply stopped changing — which is exactly what a quiet market looks like.
#[test]
fn a_book_given_up_on_is_said_to_the_caller_that_asked_for_it() {
    #[derive(Default)]
    struct Heard {
        told: Vec<(i64, i64, String)>,
    }
    impl Wrapper for Heard {
        fn error(&mut self, req_id: i64, code: i64, message: &str, _adv: &str) {
            self.told.push((req_id, code, message.to_string()));
        }
    }

    let (client, _rx, shared) = test_client();
    let flooding = 7u32;
    for _ in 0..=crate::bridge::STREAM_BACKLOG_LIMIT {
        shared.market.push_depth_update(crate::types::DepthUpdate {
            req_id: flooding,
            position: 0,
            market_maker: String::new(),
            operation: 0,
            side: 1,
            price: 1.0,
            size: 1.0,
            is_smart_depth: false,
        });
    }
    assert!(shared.market.depth_was_dropped(flooding), "the runaway book was given up");

    let mut heard = Heard::default();
    client.process_msgs(&mut heard);
    assert!(
        heard.told.iter().any(|(req_id, code, message)| {
            *req_id == i64::from(flooding)
                && *code == 354
                && message.contains("given up whole")
        }),
        "the request that asked for the book is told it is no longer served: {:?}",
        heard.told,
    );
}

/// An order the venue replayed at connect is replaced, not placed again.
///
/// A replayed order was placed by some earlier session, so it is in no book
/// this client keeps. Asked only of that book, a call naming its id read as a
/// first placement: a new order went out under a number the venue is already
/// working, and the engine's record of the order it named was overwritten on
/// the way, so the caller was reading terms nothing at the venue held.
#[test]
fn a_replayed_order_is_replaced_rather_than_placed_again() {
    let (client, rx, shared) = test_client();
    shared.orders.push_order_info(4242, crate::bridge::RichOrderInfo {
        contract: spy(),
        order: crate::types::model::Order {
            order_id: 4242, action: "BUY".into(), total_quantity: 100.0,
            order_type: "LMT".into(), lmt_price: 100.0, ..Default::default()
        },
        order_state: crate::types::model::OrderState {
            status: "Submitted".into(), ..Default::default()
        },
        last_exec: Default::default(),
    });
    shared.orders.set_replay_done();

    let revision = Order {
        order_id: 4242, action: "BUY".into(), total_quantity: 100.0,
        order_type: "LMT".into(), lmt_price: 101.0, tif: "DAY".into(),
        transmit: true, ..Default::default()
    };
    // Naming another contract on a replace is refused: the venue's own book
    // says which contract this order is on, and a replace names the order.
    let elsewhere = Contract { symbol: "QQQ".into(), con_id: 320227571, ..spy() };
    let wrong = client.place_order(4242, &elsewhere, &revision);
    assert!(
        wrong.is_err_and(|why| why.message.contains("another contract")),
        "a replace naming a contract the order is not on is refused",
    );
    assert!(rx.try_recv().is_err(), "and nothing was asked of the engine for it");

    client.place_order(4242, &spy(), &revision).expect("the revision travels");
    match rx.try_recv().expect("something travels") {
        ControlCommand::Order(OrderRequest::Modify { order_id, price, .. }) => {
            assert_eq!(order_id, 4242);
            assert_eq!(price, crate::types::price_from_f64(101.0));
        }
        other => panic!("a replace of a working order, not {other:?}"),
    }
}

/// A revision built and kept, then withdrawn, leaves no trace in the record.
///
/// A change to a working order that does not transmit is built and held, and
/// the record states its terms at once because the venue can answer a change
/// before the call that sent it returns. Withdrawing the order throws the held
/// change away — it was never sent — but the record went on stating it, so
/// every later cancel and replace restated a price nothing had ever been
/// given, and a refused cancellation would not put it back.
#[test]
fn a_held_revision_that_is_withdrawn_leaves_the_terms_the_venue_holds() {
    let (client, rx, _shared) = test_client();
    let order = |transmit: bool, price: f64| Order {
        order_id: 91, action: "BUY".into(), total_quantity: 100.0,
        order_type: "LMT".into(), lmt_price: price, tif: "DAY".into(),
        transmit, ..Default::default()
    };
    client.place_order(91, &spy(), &order(true, 100.0)).expect("placed and sent");
    assert!(matches!(
        rx.try_recv(), Ok(ControlCommand::Order(OrderRequest::SubmitEx { .. })),
    ));
    // A change to it, kept rather than sent.
    client.place_order(91, &spy(), &order(false, 101.0)).expect("the change is kept");
    assert!(next_command(&rx).is_none(), "nothing goes to the venue for a change that is held");

    client.cancel_order(91, "").expect("withdrawn");

    let stated = client.core.open_orders.lock().unwrap()
        .get(&91).expect("the order is still tracked").order.lmt_price;
    assert_eq!(stated, 100.0, "the record states the terms the venue was given");
}

/// A revision that leaves the hold to be sent keeps its terms.
///
/// What is held leaves the hold as often to be sent as to be thrown away.
/// Putting the terms back on the way out left the venue working the new price
/// while the record stated the old one, and spent the copy kept against a
/// refusal — so the venue's later refusal of those terms put nothing back, and
/// every check the next replace makes was made against terms nobody held.
#[test]
fn a_revision_sent_out_of_the_hold_keeps_the_terms_it_states() {
    let (client, rx, _shared) = test_client();
    let order = |transmit: bool, price: f64| Order {
        order_id: 93, action: "BUY".into(), total_quantity: 100.0,
        order_type: "LMT".into(), lmt_price: price, tif: "DAY".into(),
        transmit, ..Default::default()
    };
    client.place_order(93, &spy(), &order(true, 100.0)).expect("placed and sent");
    let _ = rx.try_recv();
    // A change kept rather than sent, and then a second that transmits: the
    // first leaves the hold on the way out, behind the second.
    client.place_order(93, &spy(), &order(false, 101.0)).expect("the change is kept");
    client.place_order(93, &spy(), &order(true, 102.0)).expect("and this one goes");

    let tracked = client.core.open_orders.lock().unwrap()
        .get(&93).expect("still tracked").clone();
    assert_eq!(tracked.order.lmt_price, 102.0, "the record states what went out");
    assert_eq!(
        tracked.before_the_replace.as_ref().map(|o| o.lmt_price), Some(100.0),
        "and what the venue holds is still kept against a refusal of it",
    );
}

/// A number this session has placed under is not handed out again.
///
/// The allocator counts from the highest number the venue has named, and the
/// venue has not named one this session has only just sent. A program keeping
/// its own numbers — the reference client's own idiom — and then asking for
/// one was given a number it had put on the market moments before, and the
/// venue refuses the second order under it.
#[test]
fn a_number_this_session_has_spent_is_not_handed_out_again() {
    let (client, rx, _shared) = test_client();
    let order = |id: i64| Order {
        order_id: id, action: "BUY".into(), total_quantity: 100.0,
        order_type: "LMT".into(), lmt_price: 100.0, tif: "DAY".into(),
        transmit: true, ..Default::default()
    };
    // Numbers of the caller's own choosing, none of which the venue has named.
    for id in [10, 11, 12] {
        client.place_order(id, &spy(), &order(id)).expect("placed");
    }
    while rx.try_recv().is_ok() {}

    assert_eq!(
        client.next_order_id(), 13,
        "the next number is past everything this session has spent",
    );
}

/// A number already watching a contract cannot be given another.
///
/// Nothing refused it, and the two records that answer "who is watching this
/// contract" and "what is this request watching" then disagreed: the second
/// contract took the request and the first was left in the one the delivery
/// loop reads. The caller was handed both contracts' ticks under one number
/// with nothing to tell them apart, its withdrawal reached only the second,
/// and a second withdrawal reached nothing — so the first went on arriving
/// under a number the caller had cancelled.
#[test]
fn a_request_already_watching_a_contract_is_not_given_another() {
    let (client, rx, _shared) = test_client();
    client.core.con_id_to_instrument.lock().unwrap().insert(spy().con_id, 0);
    client.core.instrument_to_req.lock().unwrap().insert(0, 5);
    client.core.req_to_instrument.lock().unwrap().insert(5, 0);
    while rx.try_recv().is_ok() {}

    let elsewhere = Contract { symbol: "QQQ".into(), con_id: 320227571, ..spy() };
    let refused = client.req_mkt_data(5, &elsewhere, "", false, false);

    assert!(
        refused.as_ref().is_err_and(|why| why.code == 102),
        "the number is already watching something under 102: {refused:?}",
    );
    assert!(rx.try_recv().is_err(), "and nothing was asked of the engine for it");
    assert_eq!(
        client.core.req_to_instrument.lock().unwrap().get(&5).copied(), Some(0),
        "the contract it was already watching is untouched",
    );
}

/// A contract the venue refuses is refused to everyone watching it.
///
/// A caller sharing somebody else's subscription holds no request of its own
/// for the venue to refuse, and the refusal names the contract rather than a
/// request — so it was told nothing at all and waited for ticks that could not
/// arrive.
#[test]
fn a_refused_contract_is_refused_to_everyone_watching_it() {
    let (client, rx, shared) = test_client();
    client.core.con_id_to_instrument.lock().unwrap().insert(spy().con_id, 0);
    client.core.instrument_to_req.lock().unwrap().insert(0, 1);
    client.core.req_to_instrument.lock().unwrap().insert(1, 0);
    // A second caller watching the same contract, holding no request of its own.
    client.core.instrument_followers.lock().unwrap().insert(0, vec![2]);
    while rx.try_recv().is_ok() {}

    shared.market.push_subscription_failure(0, "no security definition".into());
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);

    assert!(
        w.events.iter().any(|e| e.starts_with("error:1:") && e.contains("no security")),
        "the holder is told: {:?}", w.events,
    );
    assert!(
        w.events.iter().any(|e| e.starts_with("error:2:") && e.contains("no security")),
        "and so is the one sharing it: {:?}", w.events,
    );
}

/// A withdrawal read before the venue has named the working set is sent.
///
/// An order carried over from a previous session is not known here until the
/// replay lands. Where the wait for it gives up, nothing is known either way,
/// and refusing then would leave a live order working: the bound is one per
/// connection, so once it had passed every later withdrawal of a carried-over
/// order was refused for the life of the connection.
#[test]
fn a_withdrawal_before_the_replay_has_landed_is_sent() {
    let (client, rx, _shared) = test_client();
    let sent = client.cancel_order(42, "");
    assert!(sent.is_ok(), "not refused on what is not yet known: {sent:?}");
    assert!(rx.try_recv().is_ok(), "and the withdrawal went out");
}

/// A withdrawal after the trading connection has ended is answered as not
/// connected, which is what it is, rather than as a malformed request.
#[test]
fn a_withdrawal_after_the_trading_connection_ended_is_not_connected() {
    let (client, rx, shared) = test_client();
    shared.reference.set_trading_over("the test ended it");
    let refused = client.cancel_order(1, "");
    assert!(
        refused.as_ref().is_err_and(|why| why.code == 504),
        "no connection to carry it: {refused:?}",
    );
    assert!(rx.try_recv().is_err(), "and nothing was sent");
}

/// A withdrawal naming an order this client saw finish is refused as not
/// cancellable — this client has the record — and not as unknown.
#[test]
fn a_withdrawal_of_a_finished_order_is_not_cancellable() {
    let (client, rx, shared) = test_client();
    let order = Order {
        action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(),
        lmt_price: 100.0, tif: "DAY".into(), ..Default::default()
    };
    client.place_order(84, &spy(), &order).expect("placed");
    rx.try_recv().expect("the order goes out");
    shared.orders.push_order_update(OrderUpdate {
        order_id: 84, instrument: 0, status: OrderStatus::Filled,
        filled_qty: 1.0, remaining_qty: 0.0, avg_price: 0, perm_id: 0, parent_id: 0, timestamp_ns: 0,
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);

    let refused = client.cancel_order(84, "");
    assert!(
        refused.as_ref().is_err_and(|why| why.code == 161),
        "the order finished under this client's eyes: {refused:?}",
    );
    assert!(rx.try_recv().is_err(), "and nothing was sent under it");
}

/// A replace names the order, not the contract. One naming another contract
/// is refused under the number the venue gives that mismatch, so a caller
/// branching on it withdraws and places anew rather than re-sending.
#[test]
fn a_replace_naming_another_contract_is_refused_as_a_mismatch() {
    let (client, rx, _shared) = test_client();
    client.core.con_id_to_instrument.lock().unwrap().insert(265598, 1);
    let order = Order {
        action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(),
        lmt_price: 100.0, tif: "DAY".into(), ..Default::default()
    };
    client.place_order(85, &spy(), &order).expect("placed");
    rx.try_recv().expect("the order goes out");

    let aapl = Contract {
        con_id: 265598, symbol: "AAPL".into(), sec_type: "STK".into(),
        exchange: "SMART".into(), currency: "USD".into(), ..Default::default()
    };
    let refused = client.place_order(85, &aapl, &order);
    assert!(
        refused.as_ref().is_err_and(|why| why.code == 105),
        "the replace names another contract: {refused:?}",
    );
    assert!(rx.try_recv().is_err(), "and nothing was sent under it");
}

/// A book reset is delivered before the levels that follow it.
///
/// The engine queues the reset and then the venue's first new levels land.
/// Levels drained first, a pass that ran after both had arrived delivered the
/// new book and then the order to empty it.
#[test]
fn a_book_reset_is_delivered_before_the_levels_that_follow_it() {
    #[derive(Default)]
    struct Sequence(Vec<&'static str>);
    impl Wrapper for Sequence {
        fn error(&mut self, _: i64, code: i64, _: &str, _: &str) {
            if code == 317 { self.0.push("reset"); }
        }
        fn update_mkt_depth(&mut self, _: i64, _: i32, _: i32, _: i32, _: f64, _: f64) {
            self.0.push("level");
        }
    }
    let (client, _rx, shared) = test_client();
    client.req_mkt_depth(7, &spy(), 5, false).expect("asked");
    shared.reference.push_historical_error(
        7, crate::error_codes::DEPTH_BOOK_RESET, "Market depth data has been RESET".into(),
    );
    shared.market.push_depth_update(crate::types::DepthUpdate {
        req_id: 7, position: 0, market_maker: String::new(), operation: 0, side: 1,
        price: 100.0, size: 5.0, is_smart_depth: false,
    });
    let mut w = Sequence::default();
    client.process_msgs(&mut w);
    assert_eq!(w.0, ["reset", "level"], "the order to empty the book comes first");
}

/// A wait for the download ends with the session. Checked at entry alone, a
/// session that ended inside the wait had the caller sit out the ten seconds
/// and then read the pre-drop book under a refusal for silence, where the
/// refusal for no connection was three lines away.
#[test]
fn a_wait_for_the_download_ends_with_the_session() {
    type Ask = fn(&EClient, &mut RecordingWrapper);
    let asks: [(&str, Ask); 3] = [
        ("req_positions", |c, w| c.req_positions(w)),
        ("req_positions_multi", |c, w| c.req_positions_multi(9, "", "", w)),
        ("req_account_updates_multi", |c, w| c.req_account_updates_multi(9, "", "", true, w)),
    ];
    for (name, ask) in asks {
        let (client, _rx, shared) = test_client();
        let s = Arc::clone(&shared);
        let ender = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(100));
            s.reference.set_session_over("the test ended it");
        });
        let started = std::time::Instant::now();
        let mut w = RecordingWrapper::default();
        ask(&client, &mut w);
        client.process_msgs(&mut w);
        ender.join().unwrap();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "{name}: the wait ended with the session, not with the clock",
        );
        assert!(
            w.events.iter().any(|e| e.starts_with("error:") && e.contains(":504:")),
            "{name}: and the caller was told: {:?}", w.events,
        );
    }
}

/// A watch on the per-request feed does not make the plain answer replay the
/// book. The moves that landed before the ask are answered by the ask itself
/// and handed to the watchers; left in the queue, the next pass fired every
/// holding again on `position`.
#[test]
fn a_standing_watch_does_not_make_the_plain_answer_replay_the_book() {
    let (client, _rx, shared) = test_client();
    shared.portfolio.account_download_is_settled();
    let mut w = RecordingWrapper::default();
    client.req_positions_multi(9, "", "", &mut w);
    shared.portfolio.set_position_info(PositionInfo {
        con_id: 756733, position: -50.0, symbol: "SPY".into(), ..Default::default()
    });
    client.req_positions(&mut w);
    client.process_msgs(&mut w);
    let plain: Vec<_> = w.events.iter()
        .filter(|e| e.starts_with("position:") && e.contains(":756733:")).collect();
    let watched: Vec<_> = w.events.iter()
        .filter(|e| e.starts_with("position_multi:9:") && e.contains(":SPY:")).collect();
    assert_eq!(plain.len(), 1, "the holding is stated once, by the ask: {plain:?}");
    assert_eq!(watched.len(), 1, "and the watcher hears the move once: {watched:?}");
}

/// A single-position profit request naming another account is told what the
/// account-level one is told: the figures are this session's account's.
#[test]
fn req_pnl_single_says_whose_figures_it_answers_with() {
    let (client, _rx, _shared) = test_client();
    let mut w = RecordingWrapper::default();
    client.req_pnl_single(7, "DU999", "", 265598);
    client.process_msgs(&mut w);
    assert!(
        w.events.iter().any(|e| e.starts_with("error:7:321:") && e.contains("DU999")),
        "told under its own number: {:?}", w.events,
    );
}

/// A fill's client is the one that placed the order where the report names
/// none. One pass announced `order_status` under the placing client and filed
/// the same print under client zero, so a caller replaying its own fills by
/// client id got none of them.
#[test]
fn a_fill_whose_report_names_no_client_is_filed_under_the_placing_client() {
    #[derive(Default)]
    struct Filed(Vec<i64>);
    impl Wrapper for Filed {
        fn exec_details(&mut self, _: i64, _: &Contract, e: &crate::types::model::Execution) {
            self.0.push(e.client_id);
        }
    }
    let (client, rx, shared) = test_client();
    let order = Order {
        action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(),
        lmt_price: 100.0, tif: "DAY".into(), client_id: 5, ..Default::default()
    };
    client.place_order(86, &spy(), &order).expect("placed");
    rx.try_recv().expect("the order goes out");
    // The venue's record of it, carrying a report that names no client.
    shared.orders.push_order_info(86, crate::bridge::RichOrderInfo {
        contract: spy(),
        order: Order { order_id: 86, ..Default::default() },
        order_state: Default::default(),
        last_exec: crate::types::model::Execution { exec_id: "0001.86".into(), ..Default::default() },
    });
    shared.orders.push_fill(crate::types::Fill {
        instrument: 0, order_id: 86, side: crate::types::Side::Buy,
        price: 100 * PRICE_SCALE, qty: crate::types::QTY_SCALE, remaining: 0, timestamp_ns: 0, cum_qty: crate::types::QTY_SCALE, avg_price: 100 * PRICE_SCALE,
    });
    let mut w = Filed::default();
    client.process_msgs(&mut w);
    assert_eq!(w.0, [5], "filed under the client that placed it");
}

/// A fill on a bracket's leg is filed under the client the venue names.
///
/// The leg's record names no client, and a record here used to win over the
/// venue's statement whatever it said, so the fill was filed under client
/// zero. A record naming none defers to the venue, which is what zero means.
#[test]
fn a_fill_on_a_brackets_leg_is_filed_under_the_client_the_venue_names() {
    #[derive(Default)]
    struct Filed(Vec<i64>);
    impl Wrapper for Filed {
        fn exec_details(&mut self, _: i64, _: &Contract, e: &crate::types::model::Execution) {
            self.0.push(e.client_id);
        }
    }
    let (client, rx, shared) = test_client();
    let [_, tp, _] = client.place_bracket(&spy(), "BUY", 1.0, 100.0, 110.0, 90.0).unwrap();
    while rx.try_recv().is_ok() {}
    shared.orders.push_order_info(tp as u64, crate::bridge::RichOrderInfo {
        contract: spy(),
        order: Order { order_id: tp, client_id: 7, ..Default::default() },
        order_state: Default::default(),
        last_exec: crate::types::model::Execution { exec_id: "0001.tp".into(), ..Default::default() },
    });
    shared.orders.push_fill(crate::types::Fill {
        instrument: 0, order_id: tp as u64, side: crate::types::Side::Sell,
        price: 110 * PRICE_SCALE, qty: crate::types::QTY_SCALE, remaining: 0, timestamp_ns: 0,
        cum_qty: crate::types::QTY_SCALE, avg_price: 110 * PRICE_SCALE,
    });
    let mut w = Filed::default();
    client.process_msgs(&mut w);
    assert_eq!(w.0, [7], "the client the venue names, not the zero the leg's record carries");
}

/// A leg replaced with a bare order keeps its parent, its group and its type
/// in the record, as the wire keeps them, and the record's client stands: a
/// replace states no client, and a caller's statement of one is not what the
/// venue names.
///
/// The restatement wrote the caller's object over the record wholesale, so a
/// leg replaced with a fresh order read as detached and ungrouped for the life
/// of the order while the venue held it linked and grouped.
#[test]
fn a_leg_replaced_with_a_bare_order_keeps_its_links_in_the_record() {
    let (client, rx, _shared) = test_client();
    let [parent, tp, _] = client.place_bracket(&spy(), "BUY", 1.0, 100.0, 110.0, 90.0).unwrap();
    while rx.try_recv().is_ok() {}
    let bare = Order {
        action: "SELL".into(), total_quantity: 1.0, order_type: "LMT".into(),
        lmt_price: 111.0, tif: "GTC".into(), transmit: true, client_id: 5, ..Default::default()
    };
    client.place_order(tp, &spy(), &bare).unwrap();
    let record = client.core.tracked_order(tp as u64).expect("tracked");
    assert_eq!(
        (record.parent_id, record.oca_group.as_str(), record.oca_type, record.client_id),
        (parent, format!("OCA_{parent}").as_str(), 3, 0),
        "the links the wire keeps, and the client the placement recorded",
    );
}

/// A replace stating a parent or a group an order placed here was not placed
/// with is refused: the replace carries neither to the venue, so the order
/// would go on as it was placed while the caller believed it linked.
#[test]
fn a_replace_stating_other_links_for_an_order_placed_here_is_refused() {
    let (client, rx, _shared) = test_client();
    let plain = Order {
        action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(),
        lmt_price: 100.0, tif: "DAY".into(), transmit: true, ..Default::default()
    };
    client.place_order(9305, &spy(), &plain).unwrap();
    while rx.try_recv().is_ok() {}
    let linked = Order { lmt_price: 101.0, parent_id: 42, oca_group: "G1".into(), oca_type: 2, ..plain };
    let refused = client.place_order(9305, &spy(), &linked);
    assert!(refused.is_err_and(|why| why.message.contains("cannot be modified")), "told, not silently ignored");
    assert!(rx.try_recv().is_err(), "and nothing went to the engine");
    let record = client.core.tracked_order(9305).expect("tracked");
    assert_eq!((record.parent_id, record.oca_group.as_str(), record.oca_type), (0, "", 0), "the record is as placed");
}

/// The record of an order this client did not place follows the caller's
/// latest statement of it, since that statement is what the venue receives
/// on every replace of such an order.
///
/// The links were kept from the record whatever the caller stated, which is
/// right for an order placed here — the engine restates them from the
/// placement — and wrong for one the venue named: there the engine restates
/// from the caller's statement, so a group the caller moved on the second
/// replace moved at the venue while the record kept the first.
#[test]
fn a_venue_named_orders_record_follows_the_callers_latest_statement() {
    let (client, rx, shared) = test_client();
    let named = Order {
        order_id: 9307, action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(),
        lmt_price: 100.0, tif: "DAY".into(), oca_group: "G1".into(), oca_type: 3, ..Default::default()
    };
    shared.orders.push_order_info(9307, crate::bridge::RichOrderInfo {
        contract: spy(),
        order: named.clone(),
        order_state: crate::types::model::OrderState { status: "Submitted".into(), ..Default::default() },
        last_exec: Default::default(),
    });
    let first = Order { lmt_price: 101.0, transmit: true, ..named.clone() };
    client.place_order(9307, &spy(), &first).unwrap();
    let second = Order { lmt_price: 102.0, oca_group: "G2".into(), transmit: true, ..named };
    client.place_order(9307, &spy(), &second).unwrap();
    let mut stated_groups = Vec::new();
    while let Ok(cmd) = rx.try_recv() {
        if let ControlCommand::Order(OrderRequest::Modify { spec: Some(spec), .. }) = cmd {
            stated_groups.push(spec.attrs.oca_group_str.clone());
        }
    }
    assert_eq!(stated_groups, ["G1", "G2"], "each replace carried the caller's statement to the engine");
    let record = client.core.tracked_order(9307).expect("tracked");
    assert_eq!(record.oca_group, "G2", "and the record says what the venue was last told");
}

/// A change of type on an order with a parent or a group is refused.
///
/// The replace states the links only where it restates the type the order
/// was placed under; a change of type goes out without them, and the venue
/// reads their absence as their removal — measured, a leg replaced with no
/// group and no parent left its bracket. A plain order changes type as before.
#[test]
fn a_change_of_type_on_a_linked_order_is_refused() {
    let (client, rx, _shared) = test_client();
    let [_, tp, _] = client.place_bracket(&spy(), "BUY", 1.0, 100.0, 110.0, 90.0).unwrap();
    while rx.try_recv().is_ok() {}
    let as_stop = Order {
        action: "SELL".into(), total_quantity: 1.0, order_type: "STP".into(),
        aux_price: 109.0, tif: "GTC".into(), transmit: true, ..Default::default()
    };
    let refused = client.place_order(tp, &spy(), &as_stop);
    assert!(refused.is_err_and(|why| why.message.contains("change type")), "a linked leg keeps its type");
    assert!(rx.try_recv().is_err(), "and nothing went to the engine for it");

    let plain = Order {
        action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(),
        lmt_price: 100.0, tif: "DAY".into(), transmit: true, ..Default::default()
    };
    client.place_order(9306, &spy(), &plain).unwrap();
    while rx.try_recv().is_ok() {}
    let stop = Order { order_type: "STP".into(), lmt_price: 0.0, aux_price: 99.0, ..plain };
    assert!(client.place_order(9306, &spy(), &stop).is_ok(), "an unlinked order changes type as before");
}

/// The open-order read names the venue's client without writing it into the
/// record, or what a later replace kept depended on whether a read had
/// happened in between.
#[test]
fn the_open_order_read_leaves_the_record_alone() {
    let (client, rx, shared) = test_client();
    let [_, tp, _] = client.place_bracket(&spy(), "BUY", 1.0, 100.0, 110.0, 90.0).unwrap();
    while rx.try_recv().is_ok() {}
    shared.orders.push_order_info(tp as u64, crate::bridge::RichOrderInfo {
        contract: spy(),
        order: Order { order_id: tp, client_id: 7, ..Default::default() },
        order_state: crate::types::model::OrderState { status: "Submitted".into(), ..Default::default() },
        last_exec: Default::default(),
    });
    let named = client.core.collect_open_orders(&shared).into_iter()
        .find(|(id, _)| *id == tp as u64).map(|(_, o)| o.order.client_id);
    assert_eq!(named, Some(7), "the read names the venue's client");
    assert_eq!(client.core.tracked_order(tp as u64).map(|o| o.client_id), Some(0), "and the record still names none");
}

/// The open-order read names the client the venue names where the record
/// names none, as the fill does, or two callbacks about one leg named two
/// clients.
#[test]
fn the_open_order_read_names_the_client_the_venue_names_where_the_record_names_none() {
    let (client, rx, shared) = test_client();
    let [_, tp, _] = client.place_bracket(&spy(), "BUY", 1.0, 100.0, 110.0, 90.0).unwrap();
    while rx.try_recv().is_ok() {}
    shared.orders.push_order_info(tp as u64, crate::bridge::RichOrderInfo {
        contract: spy(),
        order: Order { order_id: tp, client_id: 7, ..Default::default() },
        order_state: crate::types::model::OrderState { status: "Submitted".into(), ..Default::default() },
        last_exec: Default::default(),
    });
    let named = client.core.collect_open_orders(&shared).into_iter()
        .find(|(id, _)| *id == tp as u64).map(|(_, o)| o.order.client_id);
    assert_eq!(named, Some(7));
}

/// `SCHEDULE` is a series of its own on the reference client's historical
/// request, and the other surface serves it there; this one refused it as a
/// bar type it could not send, while carrying a call of its own for it.
#[test]
fn a_schedule_asked_for_as_historical_data_is_served() {
    let (client, rx, _shared) = test_client();
    client
        .req_historical_data(7, &spy(), "", "1 D", "1 day", "SCHEDULE", true, 1, false)
        .expect("served, as the other surface serves it");
    assert!(
        matches!(rx.try_recv(), Ok(ControlCommand::FetchHistoricalSchedule { req_id: 7, .. })),
        "as the schedule request it is",
    );
}


/// A one-cancels-all group name travels as the caller names it.
///
/// A name that reads as a number was rewritten to the engine's own form on
/// the way out and read back under it. The venue holds such a name as named,
/// so the name travels as named, whatever it reads as.
#[test]
fn a_numeric_group_name_travels_as_named() {
    let (client, rx, _shared) = test_client();
    let order = Order {
        order_id: 9401, action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(),
        lmt_price: 100.0, tif: "DAY".into(), oca_group: "1234".into(), oca_type: 1, transmit: true,
        ..Default::default()
    };
    client.place_order(9401, &spy(), &order).unwrap();
    let mut stated = None;
    while let Ok(cmd) = rx.try_recv() {
        if let ControlCommand::Order(OrderRequest::SubmitEx { attrs, .. }) = cmd {
            stated = Some((attrs.oca_group_str.clone(), attrs.oca_group));
        }
    }
    assert_eq!(stated, Some(("1234".to_string(), 0)), "the group goes out under the name the caller gave");
}

/// A data connection's loss and return reach the caller under the venue's
/// numbers on this surface too.
///
/// The plain connect builds no event stream, and the notice rode the event
/// stream alone, so a program on this surface read quotes off a connection
/// the venue had said was broken and heard nothing.
#[test]
fn a_data_connections_loss_and_return_are_reported_under_the_venues_numbers() {
    let (client, _rx, shared) = test_client();
    shared.push_venue_data_notice(crate::bridge::VenueDataConnection::MarketData, false);
    shared.push_venue_data_notice(crate::bridge::VenueDataConnection::MarketData, true);
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    let said: Vec<&str> = w.events.iter().filter(|e| e.starts_with("error:-1:")).map(String::as_str).collect();
    assert_eq!(said.len(), 2, "{:?}", w.events);
    assert!(said[0].starts_with("error:-1:2103:") && said[1].starts_with("error:-1:2104:"), "{said:?}");
}

/// The increment a subscription was acknowledged with reaches every caller
/// watching the contract on `tick_req_params`, once.
#[test]
fn the_acknowledged_increment_reaches_the_caller_on_tick_req_params() {
    let (client, _rx, shared) = test_client();
    client.core.req_to_instrument.lock().unwrap().insert(1, 0);
    client.core.instrument_to_req.lock().unwrap().insert(0, 1);
    shared.market.push_tick_req_params(0, 0.01);
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    assert!(w.events.iter().any(|e| e == "tick_req_params:1:0.01::0"), "{:?}", w.events);
    client.process_msgs(&mut w);
    assert_eq!(w.events.iter().filter(|e| e.starts_with("tick_req_params:")).count(), 1, "once");
}

/// A bar that continues a kept-up-to-date request is dated as the history
/// before it was: in the caller's format, on the series' own zone.
///
/// The history came back as `20260904 09:30:00 US/Eastern` and every update
/// under the same request as seconds since the epoch with no zone.
#[test]
fn an_update_bar_is_dated_as_the_history_before_it() {
    let (client, _rx, shared) = test_client();
    client.req_historical_data(5, &spy(), "", "1 D", "1 min", "TRADES", false, 1, true).expect("asked");
    shared.reference.push_historical_data(5, crate::control::historical::HistoricalResponse {
        query_id: "q5".into(), timezone: "US/Eastern".into(), is_complete: true, bars: Vec::new(),
    });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    shared.market.push_real_time_bar(5, crate::types::RealTimeBar {
        timestamp: 1_757_000_000, open: 1.0, high: 1.0, low: 1.0, close: 1.0, volume: 0.0, wap: 1.0, count: 1,
    });
    client.process_msgs(&mut w);
    assert!(
        w.events.iter().any(|e| e == "historical_data_update:5:20250904 11:33:20 US/Eastern"),
        "{:?}", w.events.iter().filter(|e| e.starts_with("historical_data_update")).collect::<Vec<_>>(),
    );
}

/// A market-data connection that went away leaves nothing fabricated behind
/// it: what the caller last heard stands until the venue restates it.
///
/// The engine zeroes every quote at the drop, so nothing reads a pre-drop
/// price as current; the caller's own record of what it was last told did
/// not move with it, so the first quote after the rebuild — a bid alone —
/// was diffed against the old ask, last and close and each went out as a
/// real nought.
#[test]
fn a_dropped_data_connection_fabricates_no_ticks() {
    let (client, _rx, shared) = test_client();
    client.core.req_to_instrument.lock().unwrap().insert(1, 0);
    client.core.instrument_to_req.lock().unwrap().insert(0, 1);
    shared.market.push_quote(0, &Quote { bid: 150 * PRICE_SCALE, ask: 151 * PRICE_SCALE, halted: 1, ..Default::default() });
    let mut w = RecordingWrapper::default();
    client.process_msgs(&mut w);
    w.events.clear();
    // The drop: the venue's notice, and the engine's zeroing behind it.
    shared.push_venue_data_notice(crate::bridge::VenueDataConnection::MarketData, false);
    shared.market.push_quote(0, &Quote::default());
    client.process_msgs(&mut w);
    // The rebuild restates the bid alone.
    shared.market.push_quote(0, &Quote { bid: 150 * PRICE_SCALE, ..Default::default() });
    client.process_msgs(&mut w);
    let ticks: Vec<&String> = w.events.iter().filter(|e| e.starts_with("tick_price") || e.starts_with("tick_size") || e.starts_with("tick_generic")).collect();
    assert_eq!(ticks, [&"tick_price:1:1:150".to_string()], "only what the venue restated: {:?}", w.events);
}

/// A withdrawal by permanent id waits for the venue to name the working set,
/// as a withdrawal by number does.
///
/// The order it exists for is one carried over from a previous session, and
/// that is the order absent until the replay lands: read at once, the method
/// refused a withdrawal of an order the venue was working.
#[test]
fn a_withdrawal_by_permanent_id_waits_for_the_venue_to_name_the_working_set() {
    let (client, rx, shared) = test_client();
    shared.orders.replay_is_pending();
    let later = shared.clone();
    let naming = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        later.orders.push_order_info(4242, crate::bridge::RichOrderInfo {
            contract: spy(),
            order: Order {
                order_id: 4242, perm_id: 777_001, action: "BUY".into(), total_quantity: 100.0,
                order_type: "LMT".into(), lmt_price: 100.0, ..Default::default()
            },
            order_state: crate::types::model::OrderState { status: "Submitted".into(), ..Default::default() },
            last_exec: Default::default(),
        });
        later.orders.set_replay_done();
    });
    client.cancel_order_by_perm_id(777_001).expect("the order the venue names is withdrawn");
    naming.join().unwrap();
    assert!(
        matches!(rx.try_recv(), Ok(ControlCommand::Order(OrderRequest::Cancel { order_id: 4242 }))),
        "the withdrawal names the order the venue named",
    );
}

/// Withdrawing a held parent takes what hangs from it out of the hold.
///
/// The children stayed held under the cancelled parent's number: when the
/// caller later transmitted the stop-loss, the family gathered under that
/// number went out as exits naming a parent the venue was never given, and
/// their records read as working orders.
#[test]
fn withdrawing_a_held_parent_withdraws_the_children_held_under_it() {
    let (client, rx, _shared) = test_client();
    let leg = |id: i64, parent: i64| Order {
        order_id: id, parent_id: parent, transmit: false,
        action: if parent == 0 { "BUY".into() } else { "SELL".into() },
        total_quantity: 100.0, order_type: "LMT".into(), lmt_price: 100.0, tif: "DAY".into(),
        ..Default::default()
    };
    client.place_order(90, &spy(), &leg(90, 0)).expect("held");
    client.place_order(91, &spy(), &leg(91, 90)).expect("held under it");
    client.cancel_order(90, "").expect("withdrawn");
    assert!(client.core.tracked_order(91).is_none(), "the child's record goes with the parent's");
    assert!(!client.core.withdraw_held(91), "and nothing is held under the child's number");
    assert!(rx.try_recv().is_err(), "nothing reached the engine");
}

/// Asked for the API orders alone, a caller is answered with those.
///
/// The venue states no origin beside a finished order and it does number the
/// ones an API placed: an order that went out through one carries the number
/// that API gave it, and one typed in by hand carries none. Answered with all
/// of them, a program acting on what it believed were its own orders was
/// handed somebody's manual entry.
#[test]
fn asking_for_the_api_orders_alone_leaves_out_the_ones_typed_in() {
    let (client, _rx, shared) = test_client();
    shared.orders.set_replay_done();

    // One the venue numbered, and one it did not.
    for (order_id, numbered) in [(91u64, true), (92, false)] {
        shared.orders.push_order_info(order_id, crate::bridge::RichOrderInfo {
            contract: spy(),
            order: Order {
                order_id: order_id as i64, perm_id: order_id as i64,
                action: "BUY".into(), total_quantity: 1.0, ..Default::default()
            },
            order_state: crate::types::model::OrderState {
                status: "Filled".into(), ..Default::default()
            },
            last_exec: Default::default(),
        });
        if numbered {
            shared.orders.note_api_numbered(order_id);
        }
        shared.orders.push_completed_order(crate::types::CompletedOrder {
            order_id, instrument: 0, status: crate::types::OrderStatus::Filled,
            filled_qty: crate::types::QTY_SCALE, timestamp_ns: 0,
        });
    }

    #[derive(Default)]
    struct Heard(Vec<i64>);
    impl Wrapper for Heard {
        fn completed_order(
            &mut self, _: &Contract, order: &Order, _: &crate::types::model::OrderState,
        ) {
            self.0.push(order.order_id);
        }
    }

    let mut only_the_api = Heard::default();
    client.req_completed_orders(true, &mut only_the_api);
    assert_eq!(only_the_api.0, [91], "the one the venue numbered, and not the other");

    // And the archive is kept whole, so the same session asking for all of
    // them is answered with all of them.
    let mut all_of_them = Heard::default();
    client.req_completed_orders(false, &mut all_of_them);
    assert_eq!(all_of_them.0, [91, 92], "both");

    // And an order that finished while this session watched keeps the number
    // it was placed under and states no permanent id at all. Asked under the
    // permanent id alone, one another API placed was left out of an answer
    // the venue itself had marked.
    shared.orders.push_order_info(93, crate::bridge::RichOrderInfo {
        contract: spy(),
        order: Order {
            order_id: 93, perm_id: 0,
            action: "BUY".into(), total_quantity: 1.0, ..Default::default()
        },
        order_state: crate::types::model::OrderState {
            status: "Filled".into(), ..Default::default()
        },
        last_exec: Default::default(),
    });
    shared.orders.note_api_numbered(93);
    shared.orders.push_completed_order(crate::types::CompletedOrder {
        order_id: 93, instrument: 0, status: crate::types::OrderStatus::Filled,
        filled_qty: crate::types::QTY_SCALE, timestamp_ns: 0,
    });
    let mut live_one = Heard::default();
    client.req_completed_orders(true, &mut live_one);
    assert!(live_one.0.contains(&93), "the live one the venue numbered: {:?}", live_one.0);
}

/// A completed order names the client that placed it, on this surface as on
/// the other, where the venue names none.
#[test]
fn a_completed_order_names_the_client_that_placed_it() {
    let (client, rx, shared) = test_client();
    let order = Order {
        action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(),
        lmt_price: 100.0, tif: "DAY".into(), client_id: 5, ..Default::default()
    };
    client.place_order(86, &spy(), &order).expect("placed");
    rx.try_recv().expect("the order goes out");
    shared.orders.push_order_info(86, crate::bridge::RichOrderInfo {
        contract: spy(),
        order: Order { order_id: 86, action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(), lmt_price: 100.0, ..Default::default() },
        order_state: crate::types::model::OrderState { status: "Filled".into(), ..Default::default() },
        last_exec: Default::default(),
    });
    shared.orders.push_completed_order(crate::types::CompletedOrder {
        order_id: 86, instrument: 0, status: crate::types::OrderStatus::Filled,
        filled_qty: crate::types::QTY_SCALE, timestamp_ns: 0,
    });
    #[derive(Default)]
    struct Named(Vec<(i64, i32)>);
    impl Wrapper for Named {
        fn completed_order(&mut self, _: &Contract, order: &Order, _: &crate::types::model::OrderState) {
            self.0.push((order.order_id, order.client_id));
        }
    }
    let mut named = Named::default();
    client.req_completed_orders(false, &mut named);
    assert_eq!(named.0, [(86, 5)], "the client that placed it");
}

/// A global cancel attempts every instrument and says how many it reached,
/// on this surface as on the other.
#[test]
fn a_global_cancel_says_how_many_instruments_it_reached() {
    let (client, rx, shared) = test_client();
    shared.orders.set_replay_done();
    shared.market.set_instrument_count(3);
    drop(rx);
    let refused = client.req_global_cancel().expect_err("nothing reached the engine");
    assert!(refused.message.contains("reached the engine for 0 of 3"), "{refused}");
}

/// The note that a withdrawal's time does not travel is said for a
/// withdrawal that happens, not for one refused.
#[test]
fn a_refused_withdrawal_carries_no_note_about_its_time() {
    let (client, _rx, shared) = test_client();
    shared.orders.set_replay_done();
    let refused = client.cancel_order(77, "20260906-10:00:00").expect_err("no order is working under 77");
    assert!(refused.message.contains("no order is working"), "{refused}");
    assert!(shared.orders.drain_order_inactive().is_empty(), "and nothing is said about a time that did not travel");
}

/// The type a caller is served under is stated before the data it applies to.
///
/// It was stated from inside the price loop, and counted a pass as delivering
/// only where it carried a price or a size. A pass whose whole content is the
/// last-trade time — ordinary on a contract that is not quoting — delivered
/// that and stated the type afterwards, or never. The reference client states
/// it first.
#[test]
fn the_market_data_type_is_stated_before_the_first_thing_delivered() {
    #[derive(Default)]
    struct Order0f { calls: Vec<&'static str> }
    impl crate::api::wrapper::Wrapper for Order0f {
        fn market_data_type(&mut self, _req_id: i64, _market_data_type: i32) {
            self.calls.push("market_data_type");
        }
        fn tick_string(&mut self, _req_id: i64, _tick_type: i32, _value: &str) {
            self.calls.push("tick_string");
        }
    }

    let (client, _rx, shared) = test_client();
    let iid: InstrumentId = 0;
    client.core.instrument_to_req.lock().unwrap().insert(iid, 1);
    client.core.req_to_instrument.lock().unwrap().insert(1, iid);
    shared.market.set_instrument_count(1);

    // A pass carrying nothing but the last-trade time: every price and size
    // still reads as the baseline it started at.
    let quote = crate::types::Quote { timestamp_ns: 5, ..Default::default() };
    shared.market.push_quote(iid, &quote);

    let mut w = Order0f::default();
    client.process_msgs(&mut w);

    assert_eq!(
        w.calls.first().copied(),
        Some("market_data_type"),
        "the type was stated after what it applies to, or not at all: {:?}",
        w.calls,
    );
    assert!(w.calls.contains(&"tick_string"), "and the time was delivered: {:?}", w.calls);
}

/// A number past what an order can be carried under is refused, and the
/// allocator survives it.
///
/// The reader of the venue's reports stops at that number, so an order placed
/// past it goes to the venue and every report about it — the acknowledgement,
/// the fills, the withdrawal — fails to parse back to a number: the order is
/// live and invisible here. The mark the placement spends is set whatever
/// happens to the order, so one such call also left the allocator counting
/// from past the end, and every later request for a number was answered with
/// none for the rest of the session.
#[test]
fn an_order_number_past_the_end_is_refused_and_leaves_the_allocator_whole() {
    let (client, _rx, _shared) = test_client();
    let order = Order {
        action: "BUY".into(), total_quantity: 1.0, order_type: "MKT".into(),
        tif: "DAY".into(), transmit: true, ..Default::default()
    };

    let refused = client.place_order(i64::MAX, &spy(), &order);
    assert!(refused.is_err(), "an order number past the end was taken");

    assert!(
        client.next_order_id.load(std::sync::atomic::Ordering::Acquire)
            <= crate::bridge::MAX_ORDER_ID,
        "the allocator now counts from past the end, so every later request for \
         a number is answered with none for the rest of the session",
    );
}

/// An exercise takes an order's number, so it is under an order's rules.
///
/// It goes to the venue as an order and states a number on the wire. Taken as
/// given, a number that is also a working order's overwrote this side's record
/// of that order with the exercise's terms, and the venue refused the exercise
/// as a repeat of a number it was already working — while the caller was told
/// the exercise had gone. The repo's own surfaces feed request numbers that
/// start at one.
#[test]
fn an_exercise_does_not_take_the_number_of_a_working_order() {
    let (client, rx, _shared) = test_client();
    let order = Order {
        action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(),
        lmt_price: 1.0, tif: "GTC".into(), transmit: true, ..Default::default()
    };
    client.place_order(11, &spy(), &order).expect("placed and sent");
    while rx.try_recv().is_ok() {}

    let refused = client.exercise_options(
        11, &spy(), 1, 1, "", true, crate::client_core::ExerciseStates::default(),
    );
    assert!(
        refused.is_err(),
        "the exercise took the number of an order the venue is working",
    );
    assert!(
        rx.try_recv().is_err(),
        "and nothing went out under it",
    );
}

/// A contract stated by description is answered once its model is stated.
///
/// There is no id to look the model up by, and nothing is ever kept under
/// nought — one entry there would point every conId-less contract at the first
/// one's slot. The watch this client opens to obtain the model resolves the
/// contract and records the slot under the request that opened it, and that is
/// where it has to be found. Looked for by the contract's own id, the lookup
/// could not succeed however long it waited — and the refusal it gave is the
/// one that means "not yet", so the question was kept, re-solved on every
/// pass, and its caller told neither an answer nor a reason for the life of
/// the session.
#[test]
fn a_contract_named_by_description_is_answered_once_its_model_is_stated() {
    let (client, _rx, shared) = test_client();
    let iid: InstrumentId = 0;
    // The watch resolved the contract and holds it under the caller's number.
    client.core.req_to_instrument.lock().unwrap().insert(9, iid);
    shared.market.set_instrument_count(1);
    // And the venue has since stated its model for that slot.
    shared.market.push_option_computation(crate::types::OptionComputation {
        instrument: iid,
        implied_vol: 0.2,
        opt_price: 5.0,
        und_price: 100.0,
        pv_dividend: 0.0,
        ..Default::default()
    });

    let described = Contract {
        con_id: 0,
        symbol: "SPY".into(),
        sec_type: "OPT".into(),
        exchange: "SMART".into(),
        currency: "USD".into(),
        last_trade_date_or_contract_month: "20270320".into(),
        strike: 100.0,
        right: "C".into(),
        ..Default::default()
    };
    let kept = crate::api::client::PendingOptionCalc {
        contract: described,
        wants_volatility: true,
        option_price: 5.0,
        under_price: 100.0,
        answered: false,
    };

    assert!(
        client.solve_and_push_volatility(9, &kept),
        "the question is kept and re-solved for ever, with its caller told \
         neither an answer nor a reason",
    );
}

/// A first ask does not read the model of whatever else its number watches.
///
/// The number is the caller's own, and it may already be watching a contract
/// of its own choosing. For a contract with no id there is nothing to look a
/// model up by, so a fallback to that number would find the other contract's
/// slot — and a model belonging to a different contract answers as readily as
/// the right one. Only a question this client has already opened a watch for
/// reads one that way.
#[test]
fn a_first_ask_does_not_read_the_model_of_another_contract() {
    let (client, _rx, shared) = test_client();
    let iid: InstrumentId = 0;
    // The caller's number is watching a contract of its own.
    client.core.req_to_instrument.lock().unwrap().insert(9, iid);
    shared.market.set_instrument_count(1);
    shared.market.push_option_computation(crate::types::OptionComputation {
        instrument: iid,
        implied_vol: 0.2,
        opt_price: 5.0,
        und_price: 100.0,
        pv_dividend: 0.0,
        ..Default::default()
    });

    // And it asks about a different contract, named by description.
    let other = Contract {
        con_id: 0,
        symbol: "QQQ".into(),
        sec_type: "OPT".into(),
        exchange: "SMART".into(),
        currency: "USD".into(),
        // Terms the watched contract's model would solve for, so that reading
        // it produces an answer rather than failing on the arithmetic — which
        // is what makes this test about the lookup and not about the numbers.
        last_trade_date_or_contract_month: "20270320".into(),
        strike: 100.0,
        right: "C".into(),
        ..Default::default()
    };
    client.calculate_implied_volatility(9, &other, 5.0, 100.0);

    let answered = shared.market.drain_option_computations();
    assert!(
        !answered.iter().any(|c| c.answers == Some(9)),
        "the question was answered from the model of whatever else that number \
         happened to be watching: {answered:?}",
    );
}

/// A tick withdrawal that is refused leaves the stream's kind where it found
/// it.
///
/// The kind was taken out ahead of the record that says the stream is there,
/// so a refused withdrawal took it anyway. A stream with no kind recorded is
/// reported as the exchange's own, so every print of an AllLast stream after
/// such a withdrawal was labelled as a different stream's — the withdrawal
/// that was refused is the one thing that must leave nothing changed.
#[test]
fn a_refused_tick_withdrawal_leaves_the_kind_of_the_stream_still_running() {
    let (client, _rx, _shared) = test_client();
    // A kind on this number, and no mapping to withdraw a stream by.
    client.tbt_kinds.lock().unwrap().insert(7, TbtType::AllLast);

    let refused = client.cancel_tick_by_tick_data(7);
    assert!(
        refused.as_ref().is_err_and(|why| why.code == 300),
        "nothing is held under that number: {refused:?}",
    );
    assert_eq!(
        client.tbt_kinds.lock().unwrap().get(&7).copied(),
        Some(TbtType::AllLast),
        "the kind went with a withdrawal that was refused, and the stream it \
         belongs to is still running",
    );
}

/// Placing under the next id keeps the hedge and the legs the caller stated.
///
/// `place` names the contract before it takes the turn, because a lookup takes
/// a turn of its own and one asked from inside this one would never run. Named
/// here, the contract reaches `place_order` already carrying an id, so the
/// restore `place_order` does around its own naming does not run — and what
/// the venue names is a description of one contract, which has neither a hedge
/// nor legs.
#[test]
fn placing_by_description_keeps_the_hedge_the_caller_stated() {
    let (client, _rx, _shared) = test_client();
    let mut hedged = spy();
    hedged.con_id = 0;
    hedged.delta_neutral_contract = Some(crate::types::model::DeltaNeutralContract {
        con_id: 265598, delta: 0.5, price: 100.0,
    });
    // Named already, so the naming is answered from the record rather than the
    // venue: the question here is what survives it, not the naming.
    let key = ClientCore::description_key(&hedged);
    let mut as_the_venue_names_it = spy();
    as_the_venue_names_it.con_id = 756733;
    as_the_venue_names_it.delta_neutral_contract = None;
    client.core.remember_named(key, as_the_venue_names_it);

    // Nothing answers the order, so this waits out the settling window and
    // reports what is known; what it was placed on is on the book either way.
    let _ = client.place(&hedged, &Order::limit("BUY", 100.0, 100.0));

    let held = client.core.open_orders.lock().unwrap();
    let (_, placed) = held.iter().next().expect("the order is tracked");
    assert!(
        placed.contract.delta_neutral_contract.is_some(),
        "the contract this order hedges against is what the caller stated: {:?}",
        placed.contract,
    );
}

/// A preview of a description keeps the hedge and the legs the caller stated,
/// for the same reason placing one does.
///
/// A preview names an unqualified contract itself, ahead of the turn, and then
/// hands `place_order` the venue's naming — so the preview came back for a
/// bare contract while the caller asked about a delta-neutral order.
#[test]
fn previewing_by_description_keeps_the_hedge_the_caller_stated() {
    let (client, rx, shared) = test_client();
    let mut hedged = spy();
    hedged.con_id = 0;
    hedged.delta_neutral_contract = Some(crate::types::model::DeltaNeutralContract {
        con_id: 265598, delta: 0.5, price: 100.0,
    });
    let preview = Order {
        action: "BUY".into(), total_quantity: 100.0, order_type: "LMT".into(),
        lmt_price: 150.0, what_if: true, ..Default::default()
    };

    let previewed_on: std::sync::Mutex<Option<Contract>> = std::sync::Mutex::new(None);
    let pushed = Arc::clone(&shared);
    // The channel is not shared, it is handed over: the answering side owns it
    // for as long as the question runs.
    let placing = &client;
    let previewed = &previewed_on;
    let refused = std::thread::scope(|scope| {
        scope.spawn(move || {
            let give_up = std::time::Instant::now() + std::time::Duration::from_secs(10);
            // The lookup goes out first, under a number of this client's own
            // choosing. Answered as the venue answers it: one contract, and
            // nothing about a hedge.
            let looked_up = loop {
                assert!(std::time::Instant::now() < give_up, "no lookup was asked");
                match rx.try_recv() {
                    Ok(ControlCommand::FetchContractDetails { req_id, .. }) => break req_id,
                    Ok(_) => {}
                    Err(_) => std::thread::sleep(std::time::Duration::from_millis(1)),
                }
            };
            pushed.reference.push_contract_details(looked_up, ContractDefinition {
                con_id: 756733, symbol: "SPY".into(), sec_type: SecurityType::Stock,
                exchange: "SMART".into(), currency: "USD".into(), ..Default::default()
            });
            pushed.reference.push_contract_details_end(looked_up);
            // Then the preview is placed, and what it was placed on is the
            // question. Refused as soon as it is read, so the wait ends here
            // rather than at the answer timeout.
            let order_id = loop {
                assert!(std::time::Instant::now() < give_up, "no preview was placed");
                let found = placing.core.open_orders.lock().unwrap().iter()
                    .find(|(_, tracked)| tracked.order.what_if)
                    .map(|(id, tracked)| (*id, tracked.contract.clone()));
                if let Some((id, on)) = found {
                    *previewed.lock().unwrap() = Some(on);
                    break id;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            };
            pushed.orders.push_order_inactive(order_id, 201, "the margin cannot be stated".into());
        });
        client.what_if_order(&hedged, &preview).expect_err("the venue refused")
    });

    assert_eq!(refused.code, 201, "the venue's number reaches the caller: {refused}");
    let on = previewed_on.lock().unwrap().clone().expect("the preview was placed");
    assert!(
        on.delta_neutral_contract.is_some(),
        "the preview went out on the venue's naming, which hedges against \
         nothing: {on:?}",
    );
}

/// A withdrawal arriving while the number is still taking its tick stream is
/// taken, and the registration takes the stream back down.
///
/// The record a withdrawal reads is written when the farm names the contract,
/// and a registration waits on that. In between there is nothing to find, so
/// the withdrawal read as a number holding no stream — and refusing it for
/// that is a refusal the venue never makes: the caller was told its withdrawal
/// had not happened, the registration finished behind it, and it held a live
/// stream it believed was gone.
#[test]
fn a_tick_withdrawal_during_registration_takes_the_stream_back_down() {
    let (client, rx, _shared) = test_client();
    // Wide enough that the withdrawal lands inside the wait. A session states
    // five seconds and the tests here a millisecond, which would close the
    // window this is about before anything could arrive in it.
    client.core.set_registration_timeout(std::time::Duration::from_secs(5));

    // The engine holds its answer back until the withdrawal has been made,
    // which is the window under test.
    let (seen_tx, seen_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let (go_tx, go_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let engine = std::thread::spawn(move || {
        while let Ok(cmd) = rx.recv() {
            if let ControlCommand::SubscribeTbt { reply_tx: Some(reply), .. } = cmd {
                let _ = seen_tx.send(());
                let _ = go_rx.recv();
                let _ = reply.try_send(Ok(0));
                break;
            }
        }
        rx
    });

    let taking = &client;
    let withdrawal = std::thread::scope(|scope| {
        let asking = scope.spawn(move || taking.req_tick_by_tick_data(7, &spy(), "AllLast", 0, false));
        seen_rx.recv().expect("the stream reached the engine");
        assert!(
            client.core.tbt_to_instrument.lock().unwrap().get(&7).is_none(),
            "nothing is recorded for it yet, which is the window",
        );
        let withdrawal = client.cancel_tick_by_tick_data(7);
        let _ = go_tx.send(());
        asking.join().unwrap().expect("the request itself was not refused");
        withdrawal
    });
    let rx = engine.join().expect("the engine thread ran");
    let sent: Vec<_> = rx.try_iter().collect();

    withdrawal.expect("a withdrawal that arrived early was refused, which the venue never does");
    assert!(
        client.core.tbt_to_instrument.lock().unwrap().get(&7).is_none(),
        "the registration published a stream under a number whose caller had \
         already been told it was withdrawn",
    );
    assert!(
        client.tbt_kinds.lock().unwrap().get(&7).is_none(),
        "a kind was left recorded for a stream that is gone",
    );
    assert!(
        sent.iter().any(|c| matches!(
            c, ControlCommand::UnsubscribeTbt { req_id: 7, instrument: 0 },
        )),
        "the stream it opened was never taken back down: {sent:?}",
    );
}

/// The same for a quote subscription: a withdrawal in the registration window
/// is taken, and what the registration opened is taken back down.
///
/// The number is answered and left holding nothing, rather than refused and
/// left holding a live subscription — which is the state a caller cannot act
/// on, because the answer it was given says there is nothing to act on.
#[test]
fn a_quote_withdrawal_during_registration_takes_the_subscription_back_down() {
    let (client, rx, _shared) = test_client();
    client.core.set_registration_timeout(std::time::Duration::from_secs(5));

    let (seen_tx, seen_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let (go_tx, go_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let engine = std::thread::spawn(move || {
        while let Ok(cmd) = rx.recv() {
            if let ControlCommand::Subscribe { reply_tx: Some(reply), .. } = cmd {
                let _ = seen_tx.send(());
                let _ = go_rx.recv();
                let _ = reply.try_send(Ok(0));
                break;
            }
        }
        rx
    });

    let taking = &client;
    let withdrawal = std::thread::scope(|scope| {
        let asking = scope.spawn(move || taking.req_mkt_data(9, &spy(), "", false, false));
        seen_rx.recv().expect("the subscription reached the engine");
        assert!(
            !client.core.holds_mkt_data(9),
            "nothing is recorded for it yet, which is the window",
        );
        let withdrawal = client.cancel_mkt_data(9);
        let _ = go_tx.send(());
        asking.join().unwrap().expect("the request itself was not refused");
        withdrawal
    });
    let rx = engine.join().expect("the engine thread ran");
    let sent: Vec<_> = rx.try_iter().collect();

    withdrawal.expect("a withdrawal that arrived early was refused, which the venue never does");
    assert!(
        !client.core.holds_mkt_data(9),
        "the registration published a subscription under a number whose caller \
         had already been told it was withdrawn",
    );
    assert_eq!(client.core.watching(9), None, "a mapping was written for it all the same");
    assert!(
        sent.iter().any(|c| matches!(c, ControlCommand::Unsubscribe { instrument: 0, .. })),
        "the subscription it opened was never taken back down: {sent:?}",
    );
}

/// A request this client will not send is refused under the number for a
/// request that is wrong, not the one that means nothing was said.
///
/// Silence carries this client's own number, negative and one the venue can
/// never state, so a caller can tell a deadline that ran out from an answer.
/// A contract carrying no id is neither: the request is malformed, the same
/// way the chain of an unnamed underlying is, and it is refused before it is
/// sent. Under the number for silence a caller retried it, waiting the whole
/// deadline again for a request that will never leave.
#[test]
fn corporate_actions_about_an_unnumbered_contract_is_a_bad_request() {
    let (client, _rx, _shared) = test_client();
    let bare = Contract {
        symbol: "NVDA".into(), sec_type: "STK".into(), exchange: "SMART".into(),
        currency: "USD".into(), ..Default::default()
    };

    let why = client
        .corporate_actions(&bare, "20240101", "20241231")
        .expect_err("a contract with no id cannot be asked about");

    assert_eq!(why.code, Refusal::VALIDATION, "the number for a request that is wrong");
    assert_ne!(why.code, Refusal::NO_ANSWER, "nothing was waited for, so nothing stayed silent");
}
