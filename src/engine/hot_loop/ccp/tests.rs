//! The tests for this module.
//!
//! One file per module, as `api/client` already does it. Each block below
//! reaches the code it tests through `super::super`, which is the module this
//! file belongs to.

use super::*;
use crate::types::model as api;
use crate::bridge::RichOrderInfo;
use crate::types::{PositionInfo, Price, Side, PRICE_SCALE, QTY_SCALE};

/// A second sentinel report brings the reconciliation forward, never back.
///
/// The deadline is shortened when the push says it has finished, so orders it
/// left out can be judged without waiting out the whole grace. This arm is
/// reached by any report whose order id does not read, not by that terminator
/// alone, so assigning the deadline outright pushed it back each time one
/// arrived — and under a steady trickle the sweep that reports those orders
/// never ran at all.
#[test]
fn a_later_sentinel_does_not_push_the_reconciliation_back() {
    let mut ccp = CcpState::new();
    let mut context = Context::new();
    let shared = SharedState::new();

    let soon = Instant::now() + Duration::from_millis(1);
    ccp.recovery_sweep_at = Some(soon);

    // The push has named an order, so the record that follows is its end
    // and may bring the reconciliation forward.
    let order = exec_report_frame(&[
        (11, "7.0"), (150, "0"), (39, "0"), (54, "1"), (6008, "756733"), (38, "100"),
    ]);
    ccp.handle_exec_report(&order, b"", &mut context, &shared, &None, "DU111111");

    // A report whose order id does not read, which is what reaches that arm.
    let mut parsed = std::collections::HashMap::new();
    parsed.insert(11u32, "*".to_string());
    parsed.insert(35u32, "8".to_string());
    ccp.handle_exec_report(&parsed, b"", &mut context, &shared, &None, "DU111111");

    let after = ccp.recovery_sweep_at.expect("the deadline is still set");
    assert!(
        after <= soon,
        "a sentinel may bring the reconciliation forward and must not delay it",
    );
}

/// A record that parses to no order reaches the sentinel arm before the
/// push has named anything, and it is not the push's end — the same shape
/// arrives as a mass-status echo ahead of every order. Shortening the
/// sweep on it let the held cancels and modifies out before the push had
/// named the orders they name, carrying ids the venue refuses. The
/// deadline only moves once at least one order has come through.
#[test]
fn the_sweep_is_not_shortened_by_a_record_that_precedes_every_order() {
    let mut ccp = CcpState::new();
    let mut context = Context::new();
    let shared = SharedState::new();

    let far = Instant::now() + Duration::from_secs(30);
    ccp.recovery_sweep_at = Some(far);

    // A record whose order id does not read, arriving before the push has
    // named anything.
    let echo = exec_report_frame(&[(11, "*")]);
    ccp.handle_exec_report(&echo, b"", &mut context, &shared, &None, "DU111111");

    assert_eq!(
        ccp.recovery_sweep_at,
        Some(far),
        "nothing has been named yet, so this is not the push's end and the deadline stands",
    );

    // Once an order has come through, the same record is the push's end
    // and shortens the wait.
    let order = exec_report_frame(&[
        (11, "7.0"), (150, "0"), (39, "0"), (54, "1"), (6008, "756733"), (38, "100"),
    ]);
    ccp.handle_exec_report(&order, b"", &mut context, &shared, &None, "DU111111");
    ccp.handle_exec_report(&echo, b"", &mut context, &shared, &None, "DU111111");

    let after = ccp.recovery_sweep_at.expect("the deadline is still set");
    assert!(
        after < far,
        "once the push has named an order, its ending record brings the reconciliation forward",
    );
}

/// `Uncertain` promises the caller a reconciliation when the reconnect
/// completes. Nothing completed it, so an order the recovery push left out
/// waited on a message that was never coming.
#[test]
fn the_recovery_reports_the_orders_it_did_not_account_for() {
    let mut ccp = CcpState::new();
    let mut context = Context::new();
    let shared = SharedState::new();
    let instrument = context.register_instrument(756733);
    context.insert_order(crate::types::Order::new(
        7, instrument, crate::types::Side::Buy, 100 * crate::types::QTY_SCALE,
        150 * crate::types::PRICE_SCALE, b'2', b'0', 0,
    ));
    context.mark_orders_uncertain();

    // Still inside the grace: the push may yet speak for it.
    ccp.recovery_sweep_at = Some(Instant::now() + Duration::from_secs(30));
    ccp.sweep_recovery(&mut context, &shared, &None);
    assert!(shared.orders.drain_order_updates().is_empty(), "nothing is due yet");

    ccp.recovery_sweep_at = Some(Instant::now() - Duration::from_secs(1));
    ccp.sweep_recovery(&mut context, &shared, &None);
    let updates = shared.orders.drain_order_updates();
    assert_eq!(updates.len(), 1, "the stranded order is reported");
    assert_eq!(updates[0].order_id, 7);
    assert_eq!(
        updates[0].status, crate::types::OrderStatus::Uncertain,
        "and reported as what it is — unknown, not a fate the engine invented",
    );

    ccp.sweep_recovery(&mut context, &shared, &None);
    assert!(shared.orders.drain_order_updates().is_empty(), "one report per recovery");
}

fn position_frame(pairs: &[(u32, &str)]) -> std::collections::HashMap<u32, String> {
    let mut m = std::collections::HashMap::new();
    m.insert(6008u32, "265598".to_string());
    for (t, v) in pairs { m.insert(*t, v.to_string()); }
    m
}

/// A frame carrying marks but no quantity leaves the position alone.
/// Reading absent as zero reconciles a live position to flat and publishes
/// it to reqPositions and both P&L paths.
///
/// The average cost is written into a row that persists, so a frame that
/// omits the tag must not replace a real one with zero either — the same
/// rule the quantity follows, on the price side.
#[test]
fn a_frame_without_an_average_cost_keeps_the_stored_one() {
    let mut context = Context::new();
    let shared = SharedState::new();
    let frame = |pairs: &[(u32, &str)]| {
        let mut m = std::collections::HashMap::new();
        for (t, v) in pairs { m.insert(*t, v.to_string()); }
        m
    };

    // A frame stating both.
    positions::handle_position_update(
        &frame(&[(6008, "756733"), (6064, "100"), (6101, "150.00"), (6068, "SPY")]),
        &mut context, &shared, &None,
    );
    let stored = shared.portfolio.position_info(756733).expect("row").avg_cost;
    assert_eq!(stored, 150 * PRICE_SCALE);

    // A later frame stating the quantity but not the cost.
    positions::handle_position_update(
        &frame(&[(6008, "756733"), (6064, "120"), (6068, "SPY")]),
        &mut context, &shared, &None,
    );
    let after = shared.portfolio.position_info(756733).expect("row");
    assert_eq!(after.position, 120.0, "the quantity it did state is applied");
    assert_eq!(
        after.avg_cost, 150 * PRICE_SCALE,
        "and the cost it did not state is kept, not zeroed",
    );
}

/// The key a holding is filed under is not the contract's multiplier.
///
/// The position frame carries no multiplier at all, and the tag that was being
/// read for one is the row's second key. Read as a multiplier it reached a caller as the contract's own,
/// and made a plain share read as multiplied: the holding was then counted
/// unpriceable and dropped out of the account's totals altogether.
#[test]
fn the_key_a_holding_is_filed_under_is_not_its_multiplier() {
    let mut context = Context::new();
    let shared = SharedState::new();
    let mut frame = std::collections::HashMap::new();
    for (t, v) in [
        (6008u32, "756733"), (6064, "100"), (6101, "150.00"), (6068, "SPY"),
        (167, "STK"), (15, "USD"), (8002, "100"),
    ] {
        frame.insert(t, v.to_string());
    }

    positions::handle_position_update(&frame, &mut context, &shared, &None);

    let row = shared.portfolio.position_info(756733).expect("the holding is recorded");
    assert_eq!(row.multiplier, "", "nothing is claimed about a multiplier");
    assert_eq!(row.position, 100.0, "and the holding itself is what the venue stated");
}

#[test]
fn marks_only_frame_does_not_flatten_a_live_position() {
    let mut context = Context::new();
    let instrument = context.register_instrument(265598);
    let shared = SharedState::new();

    positions::handle_position_update(&position_frame(&[(6064, "100"), (6101, "150.0")]),
        &mut context, &shared, &None);
    assert_eq!(context.position(instrument), 100.0);

    // Marks move, no 6064 on the frame.
    positions::handle_position_update(&position_frame(&[(6065, "151.0"), (6100, "100.0")]),
        &mut context, &shared, &None);
    assert_eq!(context.position(instrument), 100.0,
        "a marks-only frame must not flatten the position");
    assert_eq!(shared.portfolio.position_infos().iter()
        .find(|p| p.con_id == 265598).map(|p| p.position), Some(100.0),
        "reqPositions must still report the held quantity");

    // The marks from that frame did land on the existing row.
    let row = shared.portfolio.position_infos().into_iter()
        .find(|p| p.con_id == 265598).expect("row still present");
    assert_eq!(row.market_price, (151.0 * PRICE_SCALE as f64) as Price,
        "a marks-only frame must still update the marks");

    // A frame that really does carry a flat quantity still flattens it.
    positions::handle_position_update(&position_frame(&[(6064, "0")]), &mut context, &shared, &None);
    assert_eq!(context.position(instrument), 0.0);
}

/// A marks-only frame for a contract never seen before must not conjure a
/// row: set_position_marks inserts a default PositionInfo, and that row
/// would report position 0 to reqPositions and both P&L paths.
/// Same class as the absent tag: `"NaN".parse::<f64>()` succeeds and
/// `NaN as i64` is 0, so a non-finite value reached the flatten path by
/// exactly the route closed.
#[test]
fn a_non_finite_quantity_is_treated_as_no_quantity() {
    for bad in ["NaN", "inf", "-inf"] {
        let mut context = Context::new();
        let shared = SharedState::new();
        positions::handle_position_update(
            &position_frame(&[(6064, "100"), (6101, "150.0")]), &mut context, &shared, &None);
        assert_eq!(
            shared.portfolio.position_info(265598).map(|p| p.position), Some(100.0),
            "seed must establish a live position");

        positions::handle_position_update(
            &position_frame(&[(6064, bad), (6101, "151.0")]), &mut context, &shared, &None);
        assert_eq!(
            shared.portfolio.position_info(265598).map(|p| p.position), Some(100.0),
            "{bad} must not flatten a live position");
    }
}

#[test]
fn marks_only_frame_for_an_unknown_contract_creates_no_row() {
    let mut context = Context::new();
    let shared = SharedState::new();
    positions::handle_position_update(&position_frame(&[(6065, "151.0"), (6100, "100.0")]),
        &mut context, &shared, &None);
    assert!(shared.portfolio.position_infos().iter().all(|p| p.con_id != 265598),
        "no position row may be fabricated from a marks-only frame");
}

// The fill-dedup set is not wiped wholesale when it reaches its cap: a
// recently-seen ExecID stays deduplicated, so a post-reconnect replay
// cannot double-count the fill.
#[test]
fn record_exec_id_dedupes_within_window() {
    let mut ccp = CcpState::new();
    assert!(ccp.record_exec_id("exec-A"), "first sighting is new");
    assert!(!ccp.record_exec_id("exec-A"), "immediate replay is a duplicate");
}

#[test]
fn record_exec_id_evicts_oldest_not_whole_set() {
    let mut ccp = CcpState::new();
    // The very first ExecID — the one a reconnect is most likely to replay.
    assert!(ccp.record_exec_id("exec-first"));
    // Push the window exactly to its cap. Together with "exec-first" this is
    // EXEC_ID_WINDOW + 1 inserts, which evicts exactly one entry: the oldest
    // ("exec-first"). Every other recent ID must remain deduplicated.
    for i in 0..EXEC_ID_WINDOW {
        assert!(ccp.record_exec_id(&format!("exec-{i}")));
    }
    assert_eq!(ccp.seen_exec_ids.len(), EXEC_ID_WINDOW);
    // Oldest was evicted, so a replay now reads as new (unavoidable past the
    // window) — but the most recent IDs are still caught as duplicates.
    assert!(!ccp.record_exec_id("exec-0"), "recent ID still deduped");
    assert!(!ccp.record_exec_id(&format!("exec-{}", EXEC_ID_WINDOW - 1)),
        "newest ID still deduped");
}

// A wholesale clear() would have made "exec-first" re-insertable as new
// after just one extra fill past the cap; assert the rolling window keeps
// the bound without that cliff.
#[test]
fn record_exec_id_window_is_bounded() {
    let mut ccp = CcpState::new();
    for i in 0..(EXEC_ID_WINDOW * 3) {
        ccp.record_exec_id(&format!("exec-{i}"));
    }
    assert_eq!(ccp.seen_exec_ids.len(), EXEC_ID_WINDOW);
    assert_eq!(ccp.exec_id_order.len(), EXEC_ID_WINDOW);
}

// Build a what-if (6091=1) ExecReport map for order 42. `margin_fields`
// holds (tag, literal wire value) pairs exactly as the gateway puts them
// on the wire.
fn what_if_frame(margin_fields: &[(u32, &str)]) -> std::collections::HashMap<u32, String> {
    let mut m = std::collections::HashMap::new();
    m.insert(11u32, "42".to_string()); // ClOrdID
    m.insert(6091u32, "1".to_string()); // what-if marker
    for (tag, val) in margin_fields {
        m.insert(*tag, val.to_string());
    }
    m
}

// The full six margin fields of the captured true-zero close preview
// ( scenario 2b).
const ZERO_CLOSE_FIELDS: [(u32, &str); 6] = [
    (6826, "976.07"), (6827, "887.34"), (6828, "945924.53"),
    (6092, "0"), (6093, "0"), (6094, "945923.47"),
];

fn what_if_test_state() -> (CcpState, Context, SharedState) {
    let mut context = Context::new();
    let instrument = context.register_instrument(756733);
    context.insert_order(crate::types::Order {
        order_id: 42,
        instrument,
        side: Side::Buy,
        price: 0,
        qty: 100 * QTY_SCALE,
        filled: 0,
        status: crate::types::OrderStatus::Submitted,
        ord_type: b'2',
        tif: b'0',
        stop_price: 0,
    });
    (CcpState::new(), context, SharedState::new())
}

/// A replace is acknowledged as 39=5, and the gateway sends 39=6 first.
/// Captured live against a paper account, a modify runs PendingCancel then
/// Replaced. The monotonic guard ranks PendingCancel above the working
/// states, so the acknowledgement looked like a stale frame: the caller was
/// told the order was cancelling and never told the replacement was live.
#[test]
fn a_replace_acknowledgement_is_not_dropped_as_a_stale_frame() {
    let (mut ccp, mut context, shared) = ord_status_test_state();

    // The order is working, then the replace puts a cancel in flight.
    ccp.handle_exec_report(&exec_report_frame(&[(150, "0"), (39, "0")]), b"",
        &mut context, &shared, &None, "");
    ccp.handle_exec_report(&exec_report_frame(&[(150, "6"), (39, "6")]), b"",
        &mut context, &shared, &None, "");
    assert_eq!(context.order(42).map(|o| o.status),
        Some(crate::types::OrderStatus::PendingCancel), "the cancel is in flight");
    let _ = shared.orders.drain_open_orders();

    ccp.handle_exec_report(&exec_report_frame(&[(150, "5"), (39, "5")]), b"",
        &mut context, &shared, &None, "");

    assert_eq!(
        context.order(42).map(|o| o.status),
        Some(crate::types::OrderStatus::Submitted),
        "the replacement is working, not still cancelling",
    );
    assert!(
        shared.orders.drain_open_orders().iter().any(|(id, _)| *id == 42),
        "and the caller is told, rather than the frame being dropped",
    );
}

/// The recovery-push terminator carries `11='*'`, which parses to the
/// reserved order id 0. It is dropped further down the handler, but the
/// recovery insert runs first — so without a guard there it registers the
/// frame's conId and inserts order 0 before the "discard".
#[test]
fn the_recovery_terminator_mutates_no_state_before_it_is_dropped() {
    // A clean context: the shared fixture pre-registers its conId, which
    // would mask exactly what this test is looking for.
    let mut ccp = CcpState::new();
    let mut context = Context::new();
    let shared = SharedState::new();
    let frame: std::collections::HashMap<u32, String> = [
        (11u32, "*"), (150, "0"), (39, "0"), (6008, "265598"), (38, "1"), (54, "1"),
    ].iter().map(|(k, v)| (*k, v.to_string())).collect();

    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");

    assert!(context.order(0).is_none(), "the reserved order id must not be inserted");
    assert!(
        context.market.instrument_by_con_id(265598).is_none(),
        "the terminator must not register an instrument",
    );
}

/// A report on a position the venue liquidated carries the order's id with
/// a leading 'L'. The prefix is taken off to find the order, and it must
/// stay out of the recorded ClOrdID: the next cancel names whatever is
/// recorded there, and the venue only knows the caller's number.
#[test]
fn a_liquidation_report_does_not_record_the_prefixed_id() {
    let (mut ccp, mut context, shared) = ord_status_test_state();

    // The order is acknowledged under the caller's number.
    let ack = exec_report_frame(&[(11, "42.0"), (150, "0"), (39, "0")]);
    ccp.handle_exec_report(&ack, b"", &mut context, &shared, &None, "");

    // A partial fill then arrives reported under the liquidation prefix.
    let fill = exec_report_frame(&[
        (11, "L42.0"), (150, "1"), (39, "1"), (32, "30"), (31, "150.00"),
        (14, "30"), (151, "70"), (6, "150.00"), (17, "E1"),
    ]);
    ccp.handle_exec_report(&fill, b"", &mut context, &shared, &None, "");

    assert_eq!(
        context.last_clord.get(&42).map(String::as_str),
        Some("42.0"),
        "the recorded ClOrdID stays the caller's number, so a cancel names one the venue knows",
    );
}
/// LeavesQty is still the remainder everywhere it was already right. The
/// two are complements, so a change that confuses them shows up here as
/// well as on the filled side.
#[test]
fn leaves_qty_is_still_reported_as_the_remainder() {
    let (mut ccp, mut context, shared) = ord_status_test_state();
    let frame = exec_report_frame(&[
        (150, "2"), (39, "1"), (32, "30"), (31, "150.00"),
        (14, "30"), (151, "70"), (6, "150.00"), (17, "E1"),
    ]);

    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");

    let fills = shared.orders.drain_fills();
    assert_eq!(fills.len(), 1);
    assert_eq!(fills[0].0.remaining, 70 * QTY_SCALE, "the fill reports what is still working");
}

/// `filled_quantity` was taken from tag 151 (LeavesQty), the *unfilled*
/// remainder, rather than tag 14 (CumQty). The two are complements, so a
/// partially filled order reported the wrong number and a completed one —
/// LeavesQty zero — reported as entirely unfilled.
#[test]
fn filled_quantity_is_the_filled_amount_not_the_remainder() {
    let mut ccp = CcpState::new();
    let mut context = Context::new();
    let shared = SharedState::new();
    let instrument = context.market.register(265598);
    context.insert_order(crate::types::Order {
        order_id: 77, instrument, side: Side::Buy, price: 0, qty: 100 * QTY_SCALE,
        filled: 0, status: crate::types::OrderStatus::Submitted,
        ord_type: b'2', tif: b'0', stop_price: 0,
    });

    // 100 ordered, 30 filled, 70 still working.
    let frame: std::collections::HashMap<u32, String> = [
        (11u32, "77"), (150, "1"), (39, "1"), (6008, "265598"),
        (38, "100"), (14, "30"), (151, "70"), (54, "1"), (6, "150.0"),
    ].iter().map(|(k, v)| (*k, v.to_string())).collect();
    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");

    let orders = shared.orders.drain_open_orders();
    let (_, info) = orders.iter().find(|(id, _)| *id == 77)
        .expect("the order must be reported");
    assert_eq!(info.order.filled_quantity, 30.0,
        "filled must be CumQty (30), not LeavesQty (70)");

    // On a consistent frame the complement `total - leaves` gives the same
    // number, so it has to be told apart on a frame without tag 151 —
    // where the complement would report the whole order as filled.
    let mut ccp = CcpState::new();
    let mut context = Context::new();
    let shared = SharedState::new();
    let instrument = context.market.register(265598);
    context.insert_order(crate::types::Order {
        order_id: 78, instrument, side: Side::Buy, price: 0, qty: 100 * QTY_SCALE,
        filled: 0, status: crate::types::OrderStatus::Submitted,
        ord_type: b'2', tif: b'0', stop_price: 0,
    });
    let frame: std::collections::HashMap<u32, String> = [
        (11u32, "78"), (150, "1"), (39, "1"), (6008, "265598"),
        (38, "100"), (14, "30"), (54, "1"), (6, "150.0"),
    ].iter().map(|(k, v)| (*k, v.to_string())).collect();
    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");

    let orders = shared.orders.drain_open_orders();
    let (_, info) = orders.iter().find(|(id, _)| *id == 78)
        .expect("the order must be reported");
    assert_eq!(
        info.order.filled_quantity, 30.0,
        "still CumQty with no LeavesQty on the frame, not the complement (100)",
    );

    // A later report that omits tag 14 must not wipe what was established.
    // A pending cancel is exactly that shape, and zeroing there would put
    // back the symptom this corrects.
    let later: std::collections::HashMap<u32, String> = [
        (11u32, "78"), (150, "6"), (39, "6"), (6008, "265598"),
        (38, "100"), (151, "70"), (54, "1"),
    ].iter().map(|(k, v)| (*k, v.to_string())).collect();
    ccp.handle_exec_report(&later, b"", &mut context, &shared, &None, "");

    let orders = shared.orders.drain_open_orders();
    let (_, info) = orders.iter().find(|(id, _)| *id == 78)
        .expect("the order must still be reported");
    assert_eq!(
        info.order.filled_quantity, 30.0,
        "a report without tag 14 keeps the filled quantity, it does not zero it",
    );

    // And the remainder is still the remainder, on the same reports.
    assert_eq!(info.order.total_quantity, 100.0);
}
/// The midnight seed carries the same quantity tag and had the same
/// defect: reading an absent one as zero makes the day's P&L look as
/// though the position were opened intraday, when it was held overnight.
///
/// The row is kept with an unknown quantity rather than dropped, because
/// dropping it says the same wrong thing — a position with no seed row *is*
/// the intraday case — and would discard the cash and realized figures the
/// row does state.
#[test]
fn a_midnight_seed_without_a_quantity_is_not_seeded_flat() {
    let shared = SharedState::new();
    // Two entries: one stating its quantity, one omitting it.
    let body = [
        "6008=756733", "6064=100", "6822=-50.0", "6099=7.5",
        "6008=265598", "6822=-10.0", "6099=2.5",
    ].join("\x01");
    positions::handle_pnl_response(body.as_bytes(), &shared);

    let mut seeds = shared.portfolio.midnight_seeds();
    seeds.sort_by_key(|s| s.con_id);
    assert_eq!(seeds.len(), 2, "both entries are seeded");

    let stated = seeds.iter().find(|s| s.con_id == 756733).expect("stated entry");
    assert_eq!(stated.qty_midnight, Some(100.0));

    let silent = seeds.iter().find(|s| s.con_id == 265598).expect("silent entry");
    assert_eq!(silent.qty_midnight, None, "absent is unknown, not flat");
    assert_eq!(silent.money_traded, -10.0, "the figures it did state survive");
    assert_eq!(silent.realized_pnl, 2.5);
}

/// A non-finite cash or realized figure is not a figure. `"NaN"` parses
/// successfully, and one such value folded into the daily and realized
/// totals poisons them, so a caller reads nonsense for its whole position
/// rather than for the one contract that stated it. An unparseable value
/// already lands as nothing; a non-finite one must land the same way.
#[test]
fn a_non_finite_seed_figure_is_taken_as_unstated() {
    let shared = SharedState::new();
    let body = [
        "6008=756733", "6064=100", "6822=NaN", "6099=NaN",
        "6008=265598", "6064=10", "6822=-10.0", "6099=2.5",
    ].join("\x01");
    positions::handle_pnl_response(body.as_bytes(), &shared);

    let seeds = shared.portfolio.midnight_seeds();
    let poisoned = seeds.iter().find(|s| s.con_id == 756733).expect("the row is kept");
    assert!(poisoned.money_traded.is_finite(), "a NaN cash figure is not booked");
    assert!(poisoned.realized_pnl.is_finite(), "a NaN realized figure is not booked");

    let stated = seeds.iter().find(|s| s.con_id == 265598).expect("the other row");
    assert_eq!(stated.money_traded, -10.0, "finite figures are unaffected");
    assert_eq!(stated.realized_pnl, 2.5);
}

/// A fractional overnight position is a position. Narrowing the midnight
/// quantity to a whole number reads half a share as flat, and the day's
/// baseline is then sized against nothing.
#[test]
fn a_fractional_midnight_quantity_survives_the_wire() {
    let shared = SharedState::new();
    let body = ["6008=756733", "6064=0.5", "6822=-1.0"].join("\x01");
    positions::handle_pnl_response(body.as_bytes(), &shared);

    let seeds = shared.portfolio.midnight_seeds();
    let seed = seeds.iter().find(|s| s.con_id == 756733).expect("the row");
    assert_eq!(seed.qty_midnight, Some(0.5), "half a share is not flat");
}


/// The venue states what each position was worth at midnight and what has
/// been traded against it since. Those are the figures the day's change is
/// measured from, so they have to arrive intact rather than be recomputed.
///
/// A combo bucket restates the same five fields against a label. Nothing
/// here is keyed by a label, so a bucket's figures must land nowhere at
/// all instead of on whichever contract happened to come before it.
#[test]
fn the_venue_states_what_a_position_was_worth_at_midnight() {
    let shared = SharedState::new();
    let body = [
        "146=2",
        "6008=756733", "6064=100", "8223=25", "8233=44000.5", "6822=-1250.0", "6099=7.5",
        "6008=265598", "6064=-3", "8223=0", "8233=-1200.0", "6822=0", "6099=0",
        "8058=1",
        "8020=SPY 26JUN CALENDAR", "6064=9", "8233=999999.0", "6822=888888.0", "6099=777777.0",
    ].join("\x01");
    positions::handle_pnl_response(body.as_bytes(), &shared);

    let seeds = shared.portfolio.midnight_seeds();
    assert_eq!(seeds.len(), 2, "the combo bucket is not a contract");

    let long = seeds.iter().find(|s| s.con_id == 756733).expect("first contract");
    assert_eq!(long.qty_midnight, Some(100.0));
    assert_eq!(long.qty_traded, Some(25.0));
    assert_eq!(long.cost_midnight, Some(44000.5), "taken as sent, unscaled");
    assert_eq!(long.money_traded, -1250.0);
    assert_eq!(long.realized_pnl, 7.5);

    let short = seeds.iter().find(|s| s.con_id == 265598).expect("second contract");
    assert_eq!(short.qty_midnight, Some(-3.0));
    assert_eq!(short.cost_midnight, Some(-1200.0), "a short is worth a negative amount");
    assert_eq!(
        short.realized_pnl, 0.0,
        "the combo bucket's figures did not fall through onto the last contract",
    );
}

/// The body says whether it is an answer. One that reports a problem is
/// reporting that instead of stating figures, so nothing in it is read.
#[test]
fn a_pnl_body_that_reports_a_problem_states_no_seeds() {
    let shared = SharedState::new();
    let body = [
        "58=No security definition has been found",
        "6008=756733", "6064=100", "8233=44000.5", "6099=7.5",
    ].join("\x01");
    positions::handle_pnl_response(body.as_bytes(), &shared);
    assert!(shared.portfolio.midnight_seeds().is_empty(), "a problem is not a figure");
}

/// The venue answers against the reference it was handed and falls back to
/// its own request id only when it has none.
#[test]
fn the_reference_id_names_the_request_the_seeds_answer() {
    let shared = SharedState::new();
    let both = ["6529=PLR.2", "8292=PLR.1", "6008=756733", "6064=1"].join("\x01");
    positions::handle_pnl_response(both.as_bytes(), &shared);
    assert_eq!(shared.portfolio.pnl_request_key(), "PLR.1");

    let neither = ["6529=PLR.2", "8292=", "6008=756733", "6064=1"].join("\x01");
    positions::handle_pnl_response(neither.as_bytes(), &shared);
    assert_eq!(shared.portfolio.pnl_request_key(), "PLR.2");
}

/// The price table is two lists paired by position. An unreadable contract
/// id has to hold its place, because dropping it slides every price after
/// it onto the wrong contract.
#[test]
fn the_price_table_pairs_each_contract_with_its_own_price() {
    let shared = SharedState::new();
    let body = [
        "146=3",
        "6008=756733", "6008=not-a-contract", "6008=265598",
        "8057=612.34", "8057=9.99", "8057=1.005",
    ].join("\x01");
    handle_pnl_prices(body.as_bytes(), &shared);

    assert_eq!(shared.portfolio.venue_price(756733).as_deref(), Some("612.34"));
    assert_eq!(
        shared.portfolio.venue_price(265598).as_deref(), Some("1.005"),
        "the third price belongs to the third contract",
    );

    // A later table restates what it names and leaves the rest standing.
    handle_pnl_prices(["6008=756733", "8057=615.00"].join("\x01").as_bytes(), &shared);
    assert_eq!(shared.portfolio.venue_price(756733).as_deref(), Some("615.00"));
    assert_eq!(shared.portfolio.venue_price(265598).as_deref(), Some("1.005"));
}

/// One lookup describes one contract. The venues answer separately and
/// each answer is the same contract with a different exchange, so reporting
/// every one of them returned a single stock as twenty-seven listings.
#[test]
fn a_contract_reaches_the_caller_once_per_request() {
    let mut ccp = CcpState::new();
    let mut seen = |req_id: u32, con_id: i64| {
        ccp.details_delivered.entry(req_id).or_default().insert(con_id)
    };
    assert!(seen(9, 756733), "the first answer is the caller's row");
    assert!(!seen(9, 756733), "and every later venue saying the same is not");
    assert!(seen(9, 885901989), "a different contract still comes through");
    assert!(seen(10, 756733), "as does the same one under another request");
}

/// An option is asked for by expiry date and a future by contract month.
/// Both went out on MaturityMonthYear, so the option lookup asked for a
/// month that does not exist and matched nothing.
#[test]
fn a_maturity_rides_the_tag_its_precision_belongs_to() {
    assert_eq!(maturity_tag("202609"), Some(200), "a contract month");
    assert_eq!(maturity_tag("20260918"), Some(541), "a full expiry date");
    assert_eq!(maturity_tag("20260918 14:30:00"), Some(541), "a date with a time on it");
    assert_eq!(maturity_tag(""), None, "nothing to state");
    assert_eq!(maturity_tag("2026"), None, "too short to be either, so it is not guessed");
}

/// A holding arrives as a contract id and a quantity. Reported before its
/// definition lands, it named no instrument at all — a position in a
/// contract the caller cannot identify.
#[test]
fn a_holding_takes_its_contract_from_the_definition_that_follows() {
    use crate::control::contracts::{ContractDefinition, SecurityType};
    let shared = SharedState::new();
    shared.portfolio.set_position_info(PositionInfo {
        con_id: 793356217, position: 1.0, avg_cost: 38270,
        ..Default::default()
    });
    assert_eq!(
        shared.portfolio.position_info(793356217).map(|p| p.symbol.clone()),
        Some(String::new()),
        "the feed states no symbol",
    );

    let def = ContractDefinition {
        con_id: 793356217,
        symbol: "MES".to_string(),
        sec_type: SecurityType::Future,
        currency: "USD".to_string(),
        ..ContractDefinition::default()
    };
    identify_position(&shared, &def);

    let row = shared.portfolio.position_info(793356217).unwrap();
    assert_eq!(row.symbol, "MES", "and the definition names it");
    assert_eq!(row.position, 1.0, "without disturbing the quantity");
    assert_eq!(row.avg_cost, 38270, "or the basis");
}

/// The lean feed names a holding and states no multiplier, and the definition
/// is the only thing on that path that carries one. Stopping at the first
/// message to name the contract left a future or an option on the row priced
/// a unit at a time.
#[test]
fn a_named_holding_still_takes_the_multiplier_from_its_definition() {
    use crate::control::contracts::{ContractDefinition, SecurityType};
    let mut ccp = CcpState::new();
    let mut context = Context::new();
    let shared = SharedState::new();
    let mut hb = HeartbeatState::new();

    ccp.handle_position_feed(
        "6008=793356217\x016068=MES\x01167=FUT\x016064=1\x016101=38270\x01".as_bytes(),
        &mut None, &mut context, &shared, &None, &mut hb,
    );
    let row = shared.portfolio.position_info(793356217).unwrap();
    assert_eq!(row.symbol, "MES", "the feed names the contract");
    assert!(row.multiplier.is_empty(), "and states no multiplier for it");

    identify_position(&shared, &ContractDefinition {
        con_id: 793356217,
        symbol: "MES".to_string(),
        sec_type: SecurityType::Future,
        currency: "USD".to_string(),
        multiplier: 5.0,
        ..ContractDefinition::default()
    });

    let row = shared.portfolio.position_info(793356217).unwrap();
    assert_eq!(row.multiplier, "5", "which the definition supplies");
    assert_eq!(row.currency, "USD", "along with what it is priced in");
    assert_eq!(row.position, 1.0, "and neither disturbs the quantity");
    assert_eq!(row.symbol, "MES", "nor the name already on the row");
}

/// The lean feed states a quantity and often no cost. Reading the absence
/// as a cost of zero erased the basis of a live holding, and the P&L path
/// reads a zero basis as having acquired it for nothing.
#[test]
fn a_row_without_a_cost_keeps_the_one_on_file() {
    let mut ccp = CcpState::new();
    let mut context = Context::new();
    let shared = SharedState::new();
    let mut hb = HeartbeatState::new();
    context.market.register(265598);

    ccp.handle_position_feed(
        "6008=265598\x016064=100\x016101=150.0\x01".as_bytes(),
        &mut None, &mut context, &shared, &None, &mut hb,
    );
    let basis = shared.portfolio.position_info(265598).map(|i| i.avg_cost);
    assert_eq!(basis, Some(150 * crate::types::PRICE_SCALE));

    // Same holding, stated without a cost.
    ccp.handle_position_feed(
        "6008=265598\x016064=100\x01".as_bytes(),
        &mut None, &mut context, &shared, &None, &mut hb,
    );
    assert_eq!(
        shared.portfolio.position_info(265598).map(|i| i.avg_cost), basis,
        "the basis on file stands where the row states none",
    );

    // A row that closes the holding takes the basis with it, whether or not
    // it states one, or the next position in this contract opens against
    // the last one's cost.
    ccp.handle_position_feed(
        "6008=265598\x016064=0\x016101=151.0\x01".as_bytes(),
        &mut None, &mut context, &shared, &None, &mut hb,
    );
    assert_eq!(
        shared.portfolio.position_info(265598).map(|i| i.avg_cost), Some(0),
        "a closed holding keeps no basis, not even one the row states",
    );
    ccp.handle_position_feed(
        "6008=265598\x016064=100\x016101=150.0\x01".as_bytes(),
        &mut None, &mut context, &shared, &None, &mut hb,
    );

    ccp.handle_position_feed(
        "6008=265598\x016064=0\x01".as_bytes(),
        &mut None, &mut context, &shared, &None, &mut hb,
    );
    assert_eq!(
        shared.portfolio.position_info(265598).map(|i| i.avg_cost), Some(0),
        "a closed holding leaves no basis behind",
    );
    ccp.handle_position_feed(
        "6008=265598\x016064=100\x016101=150.0\x01".as_bytes(),
        &mut None, &mut context, &shared, &None, &mut hb,
    );

    // Stated as zero, which is the broker saying zero.
    ccp.handle_position_feed(
        "6008=265598\x016064=100\x016101=0\x01".as_bytes(),
        &mut None, &mut context, &shared, &None, &mut hb,
    );
    assert_eq!(
        shared.portfolio.position_info(265598).map(|i| i.avg_cost), Some(0),
        "a stated zero is a value, not an absence",
    );
}

/// The feed is the account's own statement of what it holds. It reached the
/// portfolio and the event, and not the table the callback side reads — so
/// a process that restarted holding stock ran its first decisions against
/// flat, and a strategy sizing from `position()` bought what it already had.
#[test]
fn a_position_feed_is_adopted_by_the_engine_not_only_published() {
    let mut ccp = CcpState::new();
    let mut context = Context::new();
    let shared = SharedState::new();
    let mut hb = HeartbeatState::new();
    let instrument = context.market.register(265598);
    assert_eq!(context.position(instrument), 0.0, "the engine starts knowing nothing");

    ccp.handle_position_feed(
        "6008=265598\x016064=500\x016101=151.0\x01".as_bytes(),
        &mut None, &mut context, &shared, &None, &mut hb,
    );

    assert_eq!(context.position(instrument), 500.0, "the account holds 500 and so does the engine");
    assert_eq!(shared.portfolio.position(instrument), 500.0);

    // A later statement is adopted too, not accumulated on top.
    ccp.handle_position_feed(
        "6008=265598\x016064=300\x016101=151.0\x01".as_bytes(),
        &mut None, &mut context, &shared, &None, &mut hb,
    );
    assert_eq!(context.position(instrument), 300.0, "the server's number wins, it is not added");
}

/// The 75 feed leaves a position alone where its running quantity is
/// absent: an entry carrying a conId but no parseable 6064 would otherwise
/// flatten a live position and publish it, as on the account-update path.
#[test]
fn a_position_feed_entry_without_a_quantity_leaves_the_position_alone() {
    for body in [
        // no 6064 at all
        "6008=265598\x016101=151.0\x01",
        // present but not a number
        "6008=265598\x016064=abc\x016101=151.0\x01",
        // parses, but is not a quantity
        "6008=265598\x016064=NaN\x016101=151.0\x01",
        // the same entry flushed by the next conId rather than by the end
        // of the message — a repeating group publishes at both boundaries.
        "6008=265598\x016101=151.0\x016008=756733\x016064=5\x01",
        "6008=265598\x016064=abc\x016101=151.0\x016008=756733\x016064=5\x01",
    ] {
        let mut ccp = CcpState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let (tx, rx) = std::sync::mpsc::sync_channel(4096);
        let event_tx = Some(crate::engine::hot_loop::EventSink::new(tx, Default::default()));
        let instrument = context.market.register(265598);
        shared.portfolio.set_position_info(PositionInfo {
            con_id: 265598, position: 100.0, avg_cost: 0, ..Default::default()
        });
        shared.portfolio.set_position(instrument, 100.0);

        ccp.handle_position_feed(
            body.as_bytes(), &mut None, &mut context, &shared, &event_tx, &mut hb);

        // All three stores move together, so all three are asserted: the
        // row callers read, the atomic the engine reads, and the event.
        assert_eq!(
            shared.portfolio.position_info(265598).map(|p| p.position), Some(100.0),
            "{body:?} must not flatten the position row",
        );
        assert_eq!(
            shared.portfolio.position(instrument), 100.0,
            "{body:?} must not flatten the shared position",
        );
        let flattened = rx.try_iter().any(|e| matches!(
            e, Event::PositionUpdate { con_id: 265598, position: 0.0, .. }));
        assert!(!flattened, "{body:?} must not publish a flat");
    }
}

/// The positive control for the test above: an entry that does state a
/// quantity has to reach all three stores, and at the flush triggered by
/// the next conId rather than only at the end of the message. Without this
/// the absence assertions pass just as well against a feed that publishes
/// nothing at all.
#[test]
fn a_position_feed_entry_with_a_quantity_publishes_it_everywhere() {
    let mut ccp = CcpState::new();
    let mut context = Context::new();
    let shared = SharedState::new();
    let mut hb = HeartbeatState::new();
    let (tx, rx) = std::sync::mpsc::sync_channel(4096);
    let event_tx = Some(crate::engine::hot_loop::EventSink::new(tx, Default::default()));
    let instrument = context.market.register(265598);

    // Two entries, so the first is flushed by the second's conId.
    let body = "6008=265598\x016064=42\x016101=151.0\x016008=756733\x016064=5\x01";
    ccp.handle_position_feed(
        body.as_bytes(), &mut None, &mut context, &shared, &event_tx, &mut hb);

    assert_eq!(
        shared.portfolio.position_info(265598).map(|p| p.position), Some(42.0),
        "the position row",
    );
    assert_eq!(shared.portfolio.position(instrument), 42.0, "the shared position");
    assert!(
        rx.try_iter().any(|e| matches!(
            e, Event::PositionUpdate { con_id: 265598, position: 42.0, .. })),
        "the published event",
    );
}

/// An explicit zero is a genuine flat and must still be published.
#[test]
fn a_position_feed_entry_with_an_explicit_zero_still_flattens() {
    let mut ccp = CcpState::new();
    let mut context = Context::new();
    let shared = SharedState::new();
    let mut hb = HeartbeatState::new();
    let instrument = context.market.register(265598);
    shared.portfolio.set_position_info(PositionInfo {
        con_id: 265598, position: 100.0, avg_cost: 0, ..Default::default()
    });
    shared.portfolio.set_position(instrument, 100.0);

    ccp.handle_position_feed(
        b"6008=265598\x016064=0\x016101=151.0\x01",
        &mut None, &mut context, &shared, &None, &mut hb);

    assert_eq!(
        shared.portfolio.position_info(265598).map(|p| p.position), Some(0.0),
        "an explicit zero is a genuine flat",
    );
}

// A margin-reducing preview (close, cash-account sell) resolves to a
// post-trade init margin of exactly 0, which the gateway sends as numeric "0". The old
// `> 0.0` guard dropped it and the caller timed out.
#[test]
fn what_if_zero_init_margin_is_delivered() {
    let (mut ccp, mut context, shared) = what_if_test_state();
    let frame = what_if_frame(&ZERO_CLOSE_FIELDS);
    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");
    let responses = shared.orders.drain_what_if_responses();
    assert_eq!(responses.len(), 1, "zero-margin preview must be delivered");
    assert_eq!(responses[0].init_margin_after, 0);
    // The completed preview consumes the pending order.
    assert!(context.order(42).is_none());
}

// The not-ready ack carries the literal "n/a" in all six margin fields
 //; it must be skipped so only the real data frame surfaces.
#[test]
fn what_if_not_ready_ack_is_skipped() {
    let (mut ccp, mut context, shared) = what_if_test_state();
    let frame = what_if_frame(&[
        (6826, "n/a"), (6827, "n/a"), (6828, "n/a"),
        (6092, "n/a"), (6093, "n/a"), (6094, "n/a"),
    ]);
    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");
    assert!(shared.orders.drain_what_if_responses().is_empty(),
        "n/a ack must not surface as a response");
    // The order stays pending for the subsequent data frame.
    assert!(context.order(42).is_some());
}

// The gateway's real-frame test is "any of the six margin fields
// is set", not "6092 is set". A preview that omits 6092 but carries
// numeric siblings must be delivered, with the absent field read as 0.
#[test]
fn what_if_without_6092_but_numeric_siblings_is_delivered() {
    let (mut ccp, mut context, shared) = what_if_test_state();
    let frame = what_if_frame(&[(6093, "0"), (6094, "945923.47")]);
    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");
    let responses = shared.orders.drain_what_if_responses();
    assert_eq!(responses.len(), 1, "sibling-only preview must be delivered");
    assert_eq!(responses[0].init_margin_after, 0);
    assert_eq!(responses[0].equity_with_loan_after,
        (945923.47 * PRICE_SCALE as f64) as Price);
    assert!(context.order(42).is_none());
}

// "nan" parses as f64::NAN, so it passed the old parse-success
// gate and surfaced as a bogus zero-margin preview. The gateway treats
// nan as unset, so an all-nan frame is not a data frame.
#[test]
fn what_if_nan_sentinels_are_skipped() {
    let (mut ccp, mut context, shared) = what_if_test_state();
    let frame = what_if_frame(&[
        (6826, "nan"), (6827, "nan"), (6828, "nan"),
        (6092, "nan"), (6093, "nan"), (6094, "nan"),
    ]);
    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");
    assert!(shared.orders.drain_what_if_responses().is_empty(),
        "all-nan frame must not surface as a response");
    assert!(context.order(42).is_some());
}

// Mixed frame: a nan field is unset, but one finite sibling makes the
// frame real. The nan field itself must read as 0, not poison the price.
#[test]
fn what_if_nan_field_with_finite_sibling_is_delivered() {
    let (mut ccp, mut context, shared) = what_if_test_state();
    let frame = what_if_frame(&[(6092, "nan"), (6094, "945923.47")]);
    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");
    let responses = shared.orders.drain_what_if_responses();
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0].init_margin_after, 0, "nan field reads as unset/0");
    assert_eq!(responses[0].equity_with_loan_after,
        (945923.47 * PRICE_SCALE as f64) as Price);
}

// A working order carries wire 39=0 whether it is routed or not.
// The gateway reports PreSubmitted while it waits (e.g. placed pre-market)
// and Submitted only once routed to an exchange. The discriminator is the
// routing tags on the same exec report, not a distinct wire status.
fn ord_status_test_state() -> (CcpState, Context, SharedState) {
    let mut context = Context::new();
    let instrument = context.register_instrument(756733);
    context.insert_order(crate::types::Order::new(
        42, instrument, Side::Buy, crate::types::QTY_SCALE, 100 * PRICE_SCALE, b'2', b'0', 0,
    )); // starts at PendingSubmit
    (CcpState::new(), context, SharedState::new())
}

/// An order the venue replays is published under the state that report states.
///
/// Named as working regardless, a caller was told an order was at an exchange
/// the venue had just said it had not reached — and the book then stood above
/// every later unrouted echo of it, so nothing could put it right.
#[test]
fn a_replayed_order_is_published_under_the_state_its_report_states() {
    let mut context = Context::new();
    let mut ccp = CcpState::new();
    let shared = SharedState::new();

    // Accepted and not yet routed: no venue named on the report, and no
    // reason given for a rejection.
    let mut frame = std::collections::HashMap::new();
    for (tag, val) in [
        (11u32, "88"), (150, "0"), (39, "0"), (6008, "756733"),
        (38, "100"), (55, "SPY"), (54, "1"), (40, "2"), (44, "150.00"),
    ] {
        frame.insert(tag, val.to_string());
    }
    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");

    let recovered = shared.orders.drain_open_orders();
    let (_, row) = recovered.iter().find(|(id, _)| *id == 88).expect("the order is recovered");
    assert_eq!(
        row.order_state.status, "PreSubmitted",
        "what the report says, not what this client would like it to be",
    );
    assert_eq!(
        context.order(88).map(|o| o.status),
        Some(crate::types::OrderStatus::PreSubmitted),
        "and the book agrees with what was published",
    );
}

/// A finished option order's right is published as the letter every other path
/// publishes it as.
///
/// The wire states it as a number, and passed through as the code one venue
/// statement reached callers in two spellings — and a contract handed back for
/// a definition lookup lost the field altogether, because the lookup takes only
/// the letters.
#[test]
fn a_finished_option_order_names_its_right_as_a_letter() {
    let (mut ccp, mut context, shared) = ord_status_test_state();
    ccp.completed_orders_open = true;
    let mut frame = exec_report_frame(&[
        (39, "2"), (150, "F"), (32, "1"), (31, "5.00"), (14, "1"), (151, "0"),
        (54, "1"), (38, "1"), (55, "SPY"), (167, "OPT"), (15, "USD"), (6008, "9999"),
        (40, "2"), (44, "5.00"), (1, "DU111111"), (201, "1"), (202, "500"),
    ]);
    frame.insert(11, "555".to_string());
    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");
    ccp.deliver_finished_orders(&shared, super::Handover::Final);

    let filed = shared.orders.drain_completed_orders();
    assert!(filed.iter().any(|o| o.order_id == 555), "the order is filed as finished");
    let row = shared.orders.get_order_info(555).expect("the record a caller reads back");
    assert_eq!(row.contract.right, "C", "a call is published as one");
}

/// The price an adjustable stop converts at is read back.
///
/// This client writes the whole group and read every field of it but this one,
/// so an order read back from the venue and placed again converted at nought.
#[test]
fn the_price_an_adjustable_stop_converts_at_is_read_back() {
    let mut order = crate::types::model::Order::default();
    let mut parsed = std::collections::HashMap::new();
    parsed.insert(6258u32, "142.50".to_string());
    parsed.insert(6259u32, "141.00".to_string());
    super::executions::read_stated_attributes(&mut order, &parsed);
    assert_eq!(order.trigger_price, 142.50, "the price the adjustment hinges on");
    assert_eq!(order.adjusted_stop_price, 141.00, "beside the one it converts to");
}

/// A trail the venue states as a percentage is read back as one.
///
/// The trail rides on the tag every other order states its trigger on, and the
/// unit beside it is the whole of what says which of the two it is. Read
/// without it, a stop the venue said trails by one per cent came back trailing
/// by one dollar, and the percentage it was placed with came back as nothing.
#[test]
fn a_trail_the_venue_states_as_a_percentage_is_read_back_as_one() {
    let mut order = crate::types::model::Order { aux_price: 1.0, ..Default::default() };
    let mut parsed = std::collections::HashMap::new();
    parsed.insert(99u32, "1".to_string());
    parsed.insert(6268u32, "100".to_string());
    super::executions::read_stated_attributes(&mut order, &parsed);
    assert_eq!(order.trailing_percent, 1.0, "one per cent, as the venue said");
    assert_eq!(order.aux_price, f64::MAX, "and no trail amount, which it did not");

    // The same trail without the unit is an amount, and stays where it is.
    let mut amount = crate::types::model::Order { aux_price: 1.0, ..Default::default() };
    let mut stated = std::collections::HashMap::new();
    stated.insert(99u32, "1".to_string());
    super::executions::read_stated_attributes(&mut amount, &stated);
    assert_eq!(amount.aux_price, 1.0, "a dollar is a dollar");
    assert_eq!(amount.trailing_percent, 0.0, "and no percentage");
}

/// A recovery record arriving with the instrument table already full used
/// to take the engine down. A missing order beats a dead hot loop, and the
/// conversion to the fallible register is what makes that true — nothing
/// else in the suite fails if it is reverted.
#[test]
fn a_full_instrument_table_does_not_abort_the_recovery_path() {
    let mut context = Context::new();
    let mut ccp = CcpState::new();
    let shared = SharedState::new();

    // Fill every slot, so the next registration has nowhere to go.
    for con_id in 1..=(crate::types::MAX_INSTRUMENTS as i64) {
        assert!(context.try_register_instrument(con_id).is_some(), "slot {con_id}");
    }
    assert!(
        context.try_register_instrument(999_999).is_none(),
        "the table really is full",
    );

    let mut frame = std::collections::HashMap::new();
    for (tag, val) in [
        (11u32, "42"), (150, "0"), (39, "0"), (6008, "888888"),
        (38, "100"), (55, "SPY"), (54, "1"),
    ] {
        frame.insert(tag, val.to_string());
    }

    // The point of the test: this must return rather than panic.
    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");

    assert!(
        context.order(42).is_none(),
        "the order is not tracked, which is the acknowledged cost",
    );
    // And the cost is counted rather than only logged. The order is working at
    // the venue and absent from the book a withdrawal of everything composes
    // its cancels from; uncounted, that withdrawal returns as though the
    // account had been flattened.
    assert_eq!(
        shared.orders.orders_without_a_slot(), 1,
        "the order the book could not hold is counted for the calls that answer \
         for the whole account",
    );
}
/// A short sale the venue replays is published as a short sale.
///
/// The venue states which of the three it is, and read out as a plain sale a
/// caller asking what it already has on is answered with a different order from
/// the one resting at the venue — one it can neither recognise nor replace.
#[test]
fn a_replayed_short_sale_is_published_as_one() {
    let mut context = Context::new();
    let mut ccp = CcpState::new();
    let shared = SharedState::new();

    let mut frame = std::collections::HashMap::new();
    for (tag, val) in [
        (11u32, "77"), (150, "0"), (39, "0"), (6008, "756733"),
        (38, "100"), (55, "SPY"), (54, "5"), (40, "2"), (44, "150.00"),
    ] {
        frame.insert(tag, val.to_string());
    }
    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");

    let recovered = shared.orders.drain_open_orders();
    let (_, short) = recovered.iter().find(|(id, _)| *id == 77).expect("the order is recovered");
    assert_eq!(
        short.order.action, "SSHORT",
        "the venue said which of the three it is",
    );
}

/// Build a fill report for order 42. `extra` adds or overrides tags.
fn fill_frame(extra: &[(u32, &str)]) -> std::collections::HashMap<u32, String> {
    let mut m = std::collections::HashMap::new();
    for (tag, val) in [
        (11u32, "42"), (150u32, "F"), (39u32, "1"),
        (17u32, "EXEC-1"), (31u32, "100.0"), (32u32, "10"), (151u32, "90"),
        (14u32, "10"),
        (60u32, "20260101-16:00:00"),
    ] {
        m.insert(tag, val.to_string());
    }
    for (tag, val) in extra {
        m.insert(*tag, val.to_string());
    }
    m
}

fn tracked_order_state() -> (CcpState, Context, SharedState) {
    let mut context = Context::new();
    let instrument = context.register_instrument(756733);
    context.insert_order(crate::types::Order::new(
        42, instrument, Side::Buy, 100 * crate::types::QTY_SCALE, 100 * PRICE_SCALE, b'2', b'0', 0,
    ));
    (CcpState::new(), context, SharedState::new())
}

/// At session start the venue replays recent executions, each carrying its
/// original ExecID and a resend marker. A fresh process has never seen that
/// ID, so the dedup window cannot stop it, and the order is tracked by then
/// because the recovery insert runs first. The marker is what keeps it from
/// becoming a fill event and a position move for something that happened
/// before the process started.
#[test]
fn a_resent_execution_does_not_book_a_fill() {
    for marker in [(97u32, "Y"), (43u32, "Y")] {
        let (mut ccp, mut context, shared) = tracked_order_state();
        context.adjust_order_filled(42, 10 * crate::types::QTY_SCALE); // already counted
        let frame = fill_frame(&[marker]);
        ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");

        assert!(
            shared.orders.drain_fills().is_empty(),
            "tag {} = Y restates history and must not book", marker.0,
        );
        assert_eq!(context.position(0), 0.0, "and must not move the position");
    }

    // The positive control: the same report without a marker is a real
    // execution and still books, so the assertions above are not passing
    // against a handler that books nothing.
    let (mut ccp, mut context, shared) = tracked_order_state();
    ccp.handle_exec_report(&fill_frame(&[]), b"", &mut context, &shared, &None, "");
    assert_eq!(shared.orders.drain_fills().len(), 1, "a live execution books");
    assert_eq!(context.position(0), 10.0);
}

/// end to end, as a fresh process sees it: the gateway replays the
/// order as a recovery record and then replays its executions. The record
/// carries the cumulative quantity already filled, so the executions behind
/// it state nothing new. Treating that record as unfilled made every one of
/// them look like fresh quantity, and each emitted a fill for something
/// that happened before the process started.
#[test]
fn a_fresh_process_does_not_book_the_history_it_is_replayed() {
    let mut ccp = CcpState::new();
    let mut context = Context::new();
    let shared = SharedState::new();

    // 1. The recovery record: not tracked locally, ten of a hundred filled.
    let mut recovery = std::collections::HashMap::new();
    for (tag, val) in [
        (11u32, "78"), (150u32, "0"), (39u32, "0"), (6008u32, "756733"),
        (38u32, "100"), (14u32, "10"), (55u32, "SPY"), (54u32, "1"), (40u32, "2"),
    ] {
        recovery.insert(tag, val.to_string());
    }
    ccp.handle_exec_report(&recovery, b"", &mut context, &shared, &None, "");
    let _ = shared.orders.drain_fills();

    assert_eq!(
        context.order(78).expect("recovered").filled, 10 * QTY_SCALE,
        "the record's own cumulative quantity is the baseline",
    );

    // 2. Its replayed execution, carrying the same cumulative quantity.
    let mut replay = std::collections::HashMap::new();
    for (tag, val) in [
        (11u32, "78"), (150u32, "F"), (39u32, "1"), (97u32, "Y"),
        (17u32, "OLD-EXEC"), (14u32, "10"), (32u32, "10"), (31u32, "100.0"),
        (151u32, "90"), (60u32, "20260101-16:00:00"),
    ] {
        replay.insert(tag, val.to_string());
    }
    ccp.handle_exec_report(&replay, b"", &mut context, &shared, &None, "");

    assert!(
        shared.orders.drain_fills().is_empty(),
        "the replayed execution states nothing the record did not already carry",
    );
    assert_eq!(context.order(78).expect("tracked").filled, 10 * QTY_SCALE, "and nothing is double-counted");
}

/// The same replay, seen from the record a caller asking for executions is
/// answered from. A restated execution books nothing, so it never became a
/// fill — and the fill path was the only way into that record, so after a
/// restart a caller was told, silently, that nothing had filled. It is filed
/// on the venue's own terms and announced to nobody; a live execution is a
/// fill, and is not filed a second time here.
#[test]
fn a_restated_execution_is_filed_and_not_announced() {
    let mut ccp = CcpState::new();
    let mut context = Context::new();
    let shared = SharedState::new();

    // The recovery record, then the replayed execution behind it.
    let mut recovery = std::collections::HashMap::new();
    for (tag, val) in [
        (11u32, "78"), (150u32, "0"), (39u32, "0"), (6008u32, "756733"),
        (38u32, "100"), (14u32, "10"), (55u32, "SPY"), (54u32, "1"), (40u32, "2"),
    ] {
        recovery.insert(tag, val.to_string());
    }
    ccp.handle_exec_report(&recovery, b"", &mut context, &shared, &None, "");
    let mut replay = std::collections::HashMap::new();
    for (tag, val) in [
        (11u32, "78"), (150u32, "F"), (39u32, "1"), (97u32, "Y"), (54u32, "1"),
        (17u32, "OLD-EXEC"), (14u32, "10"), (32u32, "10"), (31u32, "100.0"),
        (151u32, "90"), (60u32, "20260101-16:00:00"), (37u32, "1234567.0"), (109u32, "the desk"),
    ] {
        replay.insert(tag, val.to_string());
    }
    ccp.handle_exec_report(&replay, b"", &mut context, &shared, &None, "");

    assert!(shared.orders.drain_fills().is_empty(), "nothing is announced");
    let filed = shared.orders.drain_restated_executions();
    assert_eq!(filed.len(), 1, "the execution is on record");
    let (contract, execution) = &filed[0];
    assert_eq!(contract.symbol, "SPY");
    assert_eq!(execution.exec_id, "OLD-EXEC");
    assert_eq!(execution.side, "BOT");
    assert_eq!(execution.shares, 10.0);
    assert_eq!(execution.submitter, "the desk", "and who entered the order");
    assert_ne!(execution.perm_id, 0, "and the order's permanent number");

    // An order finished before the restart has no recovery record. Its
    // executions are replayed all the same, and with no tracked order to read
    // the side off, the report is what states it.
    let mut finished = replay.clone();
    for (tag, val) in [(11u32, "77"), (17u32, "OLDER-EXEC"), (54u32, "2"), (39u32, "2"), (151u32, "0")] {
        finished.insert(tag, val.to_string());
    }
    ccp.handle_exec_report(&finished, b"", &mut context, &shared, &None, "");
    assert!(shared.orders.drain_fills().is_empty(), "nothing is announced for that one either");
    let filed = shared.orders.drain_restated_executions();
    assert_eq!(filed.len(), 1, "an execution on an order this session never tracked is on record too");
    assert_eq!(filed[0].1.exec_id, "OLDER-EXEC");
    assert_eq!(filed[0].1.side, "SLD", "the side the report states");

    // The positive control: a live execution is a fill, and the fill path is
    // what files it.
    let (mut ccp, mut context, shared) = tracked_order_state();
    ccp.handle_exec_report(&fill_frame(&[]), b"", &mut context, &shared, &None, "");
    assert_eq!(shared.orders.drain_fills().len(), 1, "a live execution books");
    assert!(shared.orders.drain_restated_executions().is_empty(), "and is not filed twice");
}

/// A correction the venue states no execution id for does not bring a finished
/// order back a second time.
///
/// The window that tells a repeat from a new execution is asked twice on one
/// report: once by the recovery, which decides whether an order this session
/// no longer holds comes back, and once by the booking, which decides whether
/// the quantity moves. The recovery used to ask only when the venue stated an
/// id, and an absent id is exactly the shape a replay takes — so a repeated
/// correction was a repeat to the booking, which refused it, and a new
/// execution to the recovery, which brought the finished order back anyway.
#[test]
fn a_repeated_correction_without_an_execution_id_recovers_an_order_only_once() {
    let (mut ccp, mut context, shared) = tracked_order_state();
    // The order is gone from this session's book: filled and retired, or
    // finished before the process started.
    context.retire_order(42);

    // A correction that restates a trade, stating no execution id of its own.
    let correction = fill_frame(&[
        (17u32, ""), (150u32, "G"), (39u32, "1"), (20u32, "1"),
        (32u32, "50"), (14u32, "50"), (151u32, "50"),
        (6008u32, "756733"), (55u32, "SPY"), (54u32, "1"), (40u32, "2"), (38u32, "100"),
    ]);
    ccp.handle_exec_report(&correction, b"", &mut context, &shared, &None, "");
    assert!(
        context.order(42).is_some(),
        "a correction on an order this session does not hold brings it back",
    );
    let _ = shared.orders.drain_fills();

    // It finishes again, and the very same correction arrives a second time.
    context.retire_order(42);
    ccp.handle_exec_report(&correction, b"", &mut context, &shared, &None, "");
    assert!(
        context.order(42).is_none(),
        "the same correction twice is one correction, so the order stays gone",
    );
}

/// Which model a fill and its order belong to reaches the caller.
///
/// An advisor places for several models inside one account, and a report says
/// which. Dropped, every fill and every order came back attributed to the
/// account at large, and a caller could not tell one model's position from
/// another's.
#[test]
fn the_model_a_report_names_reaches_the_caller() {
    let (mut ccp, mut context, shared) = tracked_order_state();
    let mut frame = fill_frame(&[]);
    frame.insert(6700, "GROWTH".to_string());
    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");

    let held = shared.orders.get_order_info(42).expect("the order is held");
    assert_eq!(held.order.model_code, "GROWTH", "the order names its model");
    assert_eq!(held.last_exec.model_code, "GROWTH", "and so does the fill");

    // A report naming an account-only specification states no model, whatever
    // else it carries.
    let (mut ccp, mut context, shared) = tracked_order_state();
    let mut only_account = fill_frame(&[]);
    only_account.insert(6700, "GROWTH".to_string());
    only_account.insert(8065, "DU1".to_string());
    ccp.handle_exec_report(&only_account, b"", &mut context, &shared, &None, "");
    let held = shared.orders.get_order_info(42).expect("the order is held");
    assert_eq!(held.order.model_code, "", "an account-only report names no model");
}

/// The whole order the venue states comes back, not the handful of terms that
/// identify it.
///
/// A report carries every term the order holds. Only a few were read, so an
/// order read back from the venue came back as the defaults for the rest — no
/// display size, no trigger method, not hidden, no discretionary amount —
/// whatever the venue was actually working.
#[test]
fn the_whole_order_the_venue_states_comes_back() {
    let (mut ccp, mut context, shared) = tracked_order_state();
    let mut frame = fill_frame(&[]);
    for (tag, value) in [
        (111u32, "25"), (6135u32, "1"), (6115u32, "2"), (9813u32, "0.05"),
        (440u32, "CLEAR1"), (6488u32, "1"), (8402u32, "300"), (6287u32, "1"),
    ] {
        frame.insert(tag, value.to_string());
    }
    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");

    let held = shared.orders.get_order_info(42).expect("the order is held").order;
    assert_eq!(held.display_size, 25, "what it shows on the book");
    assert!(held.hidden, "and that it does not show at all");
    assert_eq!(held.trigger_method, 2, "how its trigger is judged");
    assert_eq!(held.discretionary_amt, 0.05, "what it may pay past its limit");
    assert_eq!(held.clearing_account, "CLEAR1", "where it clears");
    assert!(held.solicited);
    assert_eq!(held.duration, 300);
    assert!(held.not_held);

    // A term the report does not mention is one the order does not carry, and
    // the default already says that.
    let (mut ccp, mut context, shared) = tracked_order_state();
    ccp.handle_exec_report(&fill_frame(&[]), b"", &mut context, &shared, &None, "");
    let held = shared.orders.get_order_info(42).expect("held").order;
    assert_eq!(held.display_size, 0, "unstated stays unstated");
    assert!(!held.hidden);
}

/// The per-currency figures are read off the bucket the venue states them in.
///
/// A ledger reply is not the name-and-value stream the other account messages
/// are: it opens a bucket, names the currency, and states each figure on a tag
/// of its own. Read as name-and-value it matched nothing, and the standard way
/// to read per-currency cash came back empty — which a caller cannot tell from
/// an account holding no cash at all.
#[test]
fn the_per_currency_figures_are_read_off_their_bucket() {
    let shared = SharedState::new();
    // Two buckets in one frame: euros, then the base currency.
    let frame = concat!(
        "8001=1\x0115=EUR\x019806=5000\x018174=250\x019820=1.08\x016099=12.5",
        "\x018001=2\x0115=BASE\x019806=7500\x019819=75425.51",
    );
    super::positions::handle_ledger_update(frame.as_bytes(), &shared);

    let held = shared.portfolio.stated_account_values();
    let read = |name: &str, currency: &str| {
        held.iter()
            .find(|(k, _, c)| k == name && c == currency)
            .map(|(_, v, _)| v.clone())
    };
    assert_eq!(
        read("CashBalance", "EUR").as_deref(), Some("5250.00"),
        "the cash balance with the insured deposit in it, which is where the venue puts it",
    );
    assert_eq!(
        read("Currency", "EUR").as_deref(), Some("EUR"),
        "the bucket's currency is one of the figures the venue keeps per currency",
    );
    assert_eq!(read("Currency", "BASE").as_deref(), Some("BASE"), "and so is the next one's");
    assert_eq!(read("ExchangeRate", "EUR").as_deref(), Some("1.08"));
    assert_eq!(read("RealizedPnL", "EUR").as_deref(), Some("12.50"), "at least two places");
    assert_eq!(read("CashBalance", "BASE").as_deref(), Some("7500.00"), "the second bucket");
    assert_eq!(read("NetLiquidationByCurrency", "BASE").as_deref(), Some("75425.51"));
    assert!(
        read("CashBalance", "EUR").is_some() && read("NetLiquidationByCurrency", "EUR").is_none(),
        "and a figure stated in one bucket does not leak into the other",
    );
}

/// The venue's exchange directory says which of its sections is which, and a
/// venue is handed over under the type that section carries.
///
/// The directory states the share venues first, then a count of index venues,
/// then a count of futures venues. Labelled shares, every index venue reached
/// a caller under a type it does not trade, and a caller asking which venues
/// carry depth for an index was told none do.
#[test]
fn an_exchange_is_handed_over_under_the_type_its_section_carries() {
    let (ccp, _context, shared) = ord_status_test_state();
    let msg: Vec<u8> = [
        "35=U", "6040=102",
        "100=NYSE", "6813=New York",
        "8128=1", "100=CBOE", "6813=Chicago Options",
        "8129=1", "100=CME", "6813=Chicago Mercantile",
    ].join("\u{1}").into_bytes();

    ccp.handle_exchange_list(&msg, &shared);

    // Read the way a caller reads it: the directory is answered to whoever
    // asked for it.
    shared.reference.notify_depth_exchanges();
    let said: Vec<(String, String)> = shared.reference.drain_depth_exchanges()
        .into_iter()
        .map(|d| (d.exchange, d.sec_type))
        .collect();
    assert_eq!(
        said,
        [
            ("NYSE".to_string(), "STK".to_string()),
            ("CBOE".to_string(), "IND".to_string()),
            ("CME".to_string(), "FUT".to_string()),
        ],
        "each venue under the type its own section carries",
    );
}

/// An order the venue is still stating is not part of an answer about what it
/// has finished, whichever handover carries that answer.
///
/// The answer is assembled report by report, so a record can say the order is
/// working until a later report says otherwise. Handed over whole because the
/// window reached its bound or the connection went away, such a record reached
/// the caller as a completed order the venue never said was complete.
#[test]
fn an_order_still_being_stated_is_not_part_of_a_finished_answer() {
    let (mut ccp, _context, shared) = ord_status_test_state();
    let mut hb = HeartbeatState::new();
    let mut conn = None;
    let mut context = Context::new();

    // A window open, and one finished order assembled beside one the venue is
    // still stating.
    ccp.completed_orders_open = true;
    ccp.hold_a_finished_order_for_test(11, crate::types::OrderStatus::Filled);
    ccp.hold_a_finished_order_for_test(12, crate::types::OrderStatus::Submitted);

    // The connection goes away, which hands over what is held.
    ccp.handle_disconnect(&mut conn, &mut context, &shared, &None);
    let _ = &mut hb;

    let finished: Vec<u64> = shared.orders.drain_completed_orders()
        .into_iter()
        .map(|order| order.order_id)
        .collect();
    assert_eq!(
        finished, [11],
        "only the order the venue said it had finished: {finished:?}",
    );
}

/// The venue's own names for recovered orders are held in a window, oldest
/// out first.
///
/// One is learned per order the venue replays, and a caller may ask what the
/// account has finished as often as it likes, so unbounded the map grew for as
/// long as the connection lasted. Every other window this state keeps is
/// bounded on purpose.
#[test]
fn the_names_learned_at_recovery_are_held_in_a_window() {
    let mut ccp = CcpState::new();
    let mut context = Context::new();
    // One of them is an order the session is still working, which is what the
    // name is for.
    let instrument = context.register_instrument(756733);
    context.insert_order(crate::types::Order {
        order_id: 1,
        instrument,
        side: Side::Buy,
        price: 0,
        qty: 100 * QTY_SCALE,
        filled: 0,
        status: crate::types::OrderStatus::Submitted,
        ord_type: b'2',
        tif: b'0',
        stop_price: 0,
    });
    for n in 0..(super::WIRE_NAME_WINDOW as u64 + 16) {
        ccp.remember_the_venues_name_for(1_000_000 + n, n + 1, &context);
    }
    assert_eq!(
        ccp.how_many_venue_names_are_held(), super::WIRE_NAME_WINDOW,
        "the window holds what it says it holds",
    );
    assert_eq!(
        ccp.the_order_named(1_000_000), Some(1),
        "the name of an order still working is not forgotten, however old it is",
    );
    assert!(
        ccp.the_order_named(1_000_001).is_none(),
        "the oldest name of an order that has finished went instead",
    );
    assert_eq!(
        ccp.the_order_named(1_000_000 + super::WIRE_NAME_WINDOW as u64 + 15),
        Some(super::WIRE_NAME_WINDOW as u64 + 16),
        "and the newest is held",
    );
}

/// An exchange the venue names nothing for is not published, and the marker
/// behind it is still read.
///
/// The name follows the code, and only a field carrying it is the name. Taken
/// as whatever followed, such an exchange went out under an empty name and the
/// field behind it was swallowed — so where that field opened the futures
/// section, every futures venue after it was labelled shares. The venue also
/// states no aggregation group here, and nought is a group of its own: a
/// caller read these venues as grouped together.
#[test]
fn an_exchange_with_no_name_is_left_out_and_the_marker_behind_it_still_read() {
    let (ccp, _context, shared) = ord_status_test_state();
    let msg: Vec<u8> = [
        "35=U", "6040=102",
        "100=NYSE", "6813=New York",
        // Named nothing, and the futures marker directly behind it.
        "100=PHLX",
        "8129=1", "100=CME", "6813=Chicago Mercantile",
    ].join("\u{1}").into_bytes();

    ccp.handle_exchange_list(&msg, &shared);
    shared.reference.notify_depth_exchanges();
    let said: Vec<(String, String, i32)> = shared.reference.drain_depth_exchanges()
        .into_iter()
        .map(|d| (d.exchange, d.sec_type, d.agg_group))
        .collect();
    assert_eq!(
        said,
        [
            ("NYSE".to_string(), "STK".to_string(), i32::MAX),
            ("CME".to_string(), "FUT".to_string(), i32::MAX),
        ],
        "the unnamed one is left out and the futures marker was read: {said:?}",
    );
}

/// The venue moving a working order reaches the caller.
///
/// It states only what it changed — where the order is working, what its limit
/// is, or both — against the order it names. Read by nothing, a caller's own
/// account of the order stayed at what it was placed with while the venue
/// worked a different one, and nothing said so.
#[test]
fn the_venue_revising_a_working_order_reaches_the_caller() {
    let shared = SharedState::new();
    let placed = crate::types::model::Order {
        order_id: 55, action: "BUY".into(), total_quantity: 100.0,
        order_type: "LMT".into(), lmt_price: 100.0, tif: "DAY".into(), ..Default::default()
    };
    let spy = crate::types::model::Contract {
        symbol: "SPY".into(), exchange: "SMART".into(), ..Default::default()
    };
    shared.orders.push_order_info(55, crate::bridge::RichOrderInfo {
        contract: spy,
        order: placed,
        order_state: crate::types::model::OrderState {
            status: "Submitted".into(), ..Default::default()
        },
        last_exec: Default::default(),
    });

    let (mut ccp, mut context, _unused) = ord_status_test_state();

    let revision: std::collections::HashMap<u32, String> = [
        (11u32, "55".to_string()), (30u32, "ARCA".to_string()), (44u32, "101.5".to_string()),
    ].into_iter().collect();
    ccp.handle_order_revision(&revision, &context, &shared);

    let held = shared.orders.get_order_info(55).expect("the order is still held");
    assert_eq!(held.order.lmt_price, 101.5, "the limit the venue is now working");
    assert_eq!(held.contract.exchange, "ARCA", "and where it is working it");

    // Stating nothing changes nothing, rather than blanking what was held.
    let quiet: std::collections::HashMap<u32, String> =
        [(11u32, "55".to_string())].into_iter().collect();
    ccp.handle_order_revision(&quiet, &context, &shared);
    let held = shared.orders.get_order_info(55).expect("still held");
    assert_eq!(held.order.lmt_price, 101.5, "what it did not state, it did not change");
    assert_eq!(held.contract.exchange, "ARCA");

    // And an order this session recovered is named by the venue's own
    // permanent name from then on, which is not the number a caller addresses
    // it under. Read as a plain number, the revision either reached nothing —
    // which is every order the account already had — or reached whichever
    // unrelated order happened to be numbered the venue's permanent name for
    // this one, and wrote this order's venue and limit onto that one.
    ccp.remember_the_venues_name_for(90_071_992_547, 55, &context);
    let named_the_venues_way: std::collections::HashMap<u32, String> = [
        (11u32, "90071992547.0".to_string()), (44u32, "102.25".to_string()),
    ].into_iter().collect();
    ccp.handle_order_revision(&named_the_venues_way, &context, &shared);
    let held = shared.orders.get_order_info(55).expect("still held");
    assert_eq!(
        held.order.lmt_price, 102.25,
        "a revision naming the order the venue's way reaches the order it means",
    );

    // Unless something is working under that number itself, in which case
    // that is the order the number means.
    shared.orders.push_order_info(90_071_992_547, crate::bridge::RichOrderInfo {
        contract: crate::types::model::Contract {
            symbol: "QQQ".into(), exchange: "SMART".into(), ..Default::default()
        },
        order: crate::types::model::Order {
            order_id: 90_071_992_547, lmt_price: 500.0, ..Default::default()
        },
        order_state: Default::default(),
        last_exec: Default::default(),
    });
    context.insert_order(crate::types::Order {
        order_id: 90_071_992_547,
        instrument: 0,
        side: Side::Buy,
        price: 0,
        qty: 100 * QTY_SCALE,
        filled: 0,
        status: crate::types::OrderStatus::Submitted,
        ord_type: b'2',
        tif: b'0',
        stop_price: 0,
    });
    let same_name: std::collections::HashMap<u32, String> = [
        (11u32, "90071992547.0".to_string()), (44u32, "501.0".to_string()),
    ].into_iter().collect();
    ccp.handle_order_revision(&same_name, &context, &shared);
    assert_eq!(
        shared.orders.get_order_info(55).expect("still held").order.lmt_price, 102.25,
        "the revision went to the order working under that number, not through the name",
    );
    assert_eq!(
        shared.orders.get_order_info(90_071_992_547).expect("held").order.lmt_price, 501.0,
    );
}

/// The case a blanket suppression of marked reports loses. A CCP reconnect
/// keeps this state — window and order book both survive — and the gateway
/// replays recent executions on the new session. A fill that executed
/// during the outage therefore arrives marked, with an ExecID this session
/// has never seen, and is the first news of it. Refusing it would leave the
/// order permanently short a real fill.
#[test]
fn a_resent_execution_carrying_new_quantity_is_still_booked() {
    let (mut ccp, mut context, shared) = tracked_order_state();

    // Five already booked before the outage.
    context.adjust_order_filled(42, 5 * crate::types::QTY_SCALE);

    // The replay carries eight cumulative — three of which are news.
    let frame = fill_frame(&[(97, "Y"), (14, "8"), (32, "3"), (151, "92")]);
    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");

    assert_eq!(
        shared.orders.drain_fills().len(), 1,
        "a marked report carrying quantity the order does not have is a real fill",
    );
    assert_eq!(context.position(0), 3.0);

    // And a second copy of that same replay states no more, so it is history.
    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");
    assert!(
        shared.orders.drain_fills().is_empty(),
        "restating the same cumulative quantity is not new",
    );
    assert_eq!(context.position(0), 3.0);
}

/// Two genuine slices of one order, same size and price inside one
/// timestamp tick — the ordinary shape of algo and iceberg execution. The
/// synthesised key must tell them apart, which the cumulative quantity does
/// because it advances with every execution on the order.
#[test]
fn two_same_priced_slices_in_one_tick_are_not_one_execution() {
    let (mut ccp, mut context, shared) = tracked_order_state();

    let mut first = fill_frame(&[(32, "10"), (151, "90"), (14, "10")]);
    first.remove(&17);
    let mut second = fill_frame(&[(32, "10"), (151, "80"), (14, "20")]);
    second.remove(&17);

    ccp.handle_exec_report(&first, b"", &mut context, &shared, &None, "");
    ccp.handle_exec_report(&second, b"", &mut context, &shared, &None, "");

    assert_eq!(shared.orders.drain_fills().len(), 2, "both slices book");
    assert_eq!(context.position(0), 20.0);
}

/// Without an ExecID the execution is keyed on the fields that identify
/// it, rather than skipping the dedup window. Absent tag 17 is a shape a
/// replay takes, so skipping it books the copy a second time and doubles
/// the position.
#[test]
fn an_execution_without_an_exec_id_is_still_deduplicated() {
    let (mut ccp, mut context, shared) = tracked_order_state();
    let mut frame = fill_frame(&[]);
    frame.remove(&17);

    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");
    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");

    assert_eq!(shared.orders.drain_fills().len(), 1, "booked once, not twice");
    assert_eq!(context.position(0), 10.0, "and the position moved once");

    // A genuinely different execution on the same order is not swallowed by
    // the synthesised key.
    let mut other = fill_frame(&[]);
    other.remove(&17);
    other.insert(32, "5".to_string());
    ccp.handle_exec_report(&other, b"", &mut context, &shared, &None, "");
    assert_eq!(shared.orders.drain_fills().len(), 1, "a distinct execution still books");
    assert_eq!(context.position(0), 15.0);
}

/// A long session rolls executions out of the ExecID window, and a replay
/// arrives unordered and without ExecIDs of its own. Summing what each
/// report says it executed counts quantity the order already holds; reading
/// the cumulative figure it reports settles on the true total whatever
/// order the copies arrive in.
#[test]
fn a_replay_of_booked_history_adds_nothing_to_the_order() {
    let (mut ccp, mut context, shared) = tracked_order_state();
    context.adjust_order_filled(42, 12 * crate::types::QTY_SCALE); // both executions already booked

    let mut later = fill_frame(&[(97, "Y"), (14, "12"), (32, "4"), (151, "88")]);
    later.remove(&17);
    let mut earlier = fill_frame(&[(97, "Y"), (14, "8"), (32, "3"), (151, "92")]);
    earlier.remove(&17);
    ccp.handle_exec_report(&later, b"", &mut context, &shared, &None, "");
    ccp.handle_exec_report(&earlier, b"", &mut context, &shared, &None, "");

    assert!(shared.orders.drain_fills().is_empty(), "history restated is not new quantity");
    assert_eq!(context.order(42).unwrap().filled, 12 * QTY_SCALE, "and the order is not overcounted");
    assert_eq!(context.position(0), 0.0);

    // A fill from the same replay that this session has not booked is news
    // and still reaches the caller.
    let mut fresh = fill_frame(&[(97, "Y"), (14, "15"), (32, "3"), (151, "85")]);
    fresh.remove(&17);
    ccp.handle_exec_report(&fresh, b"", &mut context, &shared, &None, "");
    assert_eq!(shared.orders.drain_fills().len(), 1, "quantity the order lacks still books");
    assert_eq!(context.position(0), 3.0);
}

/// The same execution delivered marked and then unmarked. The cumulative
/// figure decides the marked copy, but the unmarked one is an ordinary
/// report and the window is the only thing that can catch it — so a marked
/// report has to be remembered even though it was not judged by the window.
#[test]
fn a_marked_execution_is_remembered_for_its_unmarked_twin() {
    let (mut ccp, mut context, shared) = tracked_order_state();
    context.adjust_order_filled(42, 5 * crate::types::QTY_SCALE);

    let marked = fill_frame(&[(97, "Y"), (17, "E-9"), (14, "9"), (32, "4"), (151, "91")]);
    ccp.handle_exec_report(&marked, b"", &mut context, &shared, &None, "");
    assert_eq!(shared.orders.drain_fills().len(), 1, "the marked copy books what is new");
    assert_eq!(context.order(42).unwrap().filled, 9 * QTY_SCALE);

    // The same execution again, this time without its marker.
    let unmarked = fill_frame(&[(17, "E-9"), (14, "9"), (32, "4"), (151, "91")]);
    ccp.handle_exec_report(&unmarked, b"", &mut context, &shared, &None, "");

    assert!(
        shared.orders.drain_fills().is_empty(),
        "the window catches the copy the cumulative figure cannot judge",
    );
    assert_eq!(context.order(42).unwrap().filled, 9 * QTY_SCALE, "and nothing is double-booked");
}

/// The ExecID window evicts oldest-first, so a replay batch deeper than
/// the window no longer holds its own head and the duplicate would book a
/// second time. For an order this session tracks, that window is the only
/// guard the ID itself provides.
///
/// A replayed execution is marked, so it is booked on the cumulative
/// quantity it reports rather than on the increment — and a copy that
/// restates quantity the order already holds adds nothing whether or not
/// its ExecID is still in the window. The window stops being the guard.
#[test]
fn a_replay_deeper_than_the_exec_id_window_does_not_double_count() {
    let (mut ccp, mut context, shared) = tracked_order_state();
    context.adjust_order_filled(42, 12 * crate::types::QTY_SCALE);

    // The window has rolled past this execution, so its ID is unseen here —
    // which is the whole point: the dedup window cannot be what saves this.
    let replayed = fill_frame(&[(97, "Y"), (17, "EVICTED-1"), (14, "12"), (32, "4"), (151, "88")]);

    ccp.handle_exec_report(&replayed, b"", &mut context, &shared, &None, "");

    assert!(
        shared.orders.drain_fills().is_empty(),
        "a replay the window has forgotten still adds no quantity the order holds",
    );
    assert_eq!(context.order(42).unwrap().filled, 12 * QTY_SCALE);
    assert_eq!(context.position(0), 0.0);
}

/// The same marked execution delivered twice, both copies carrying more
/// cumulative quantity than the order held when the first arrived.
#[test]
fn a_marked_execution_delivered_twice_books_once() {
    let (mut ccp, mut context, shared) = tracked_order_state();
    context.adjust_order_filled(42, 5 * crate::types::QTY_SCALE);

    let frame = fill_frame(&[(97, "Y"), (14, "12"), (32, "4"), (151, "88")]);
    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");
    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");

    assert_eq!(shared.orders.drain_fills().len(), 1, "the second copy adds nothing");
    assert_eq!(context.order(42).unwrap().filled, 12 * QTY_SCALE);
    assert_eq!(context.position(0), 7.0);
}

/// A replacement that raises the total lets an order fill the same size at
/// the same price and leave the same quantity behind twice. Everything the
/// synthesised key had to work with repeats except the cumulative figure.
#[test]
fn a_raised_total_does_not_collapse_two_slices_into_one() {
    let (mut ccp, mut context, shared) = tracked_order_state();

    let mut first = fill_frame(&[(32, "10"), (151, "90"), (14, "10")]);
    first.remove(&17);
    // Total raised from 100 to 110; the next slice again leaves 90.
    let mut second = fill_frame(&[(32, "10"), (151, "90"), (14, "20")]);
    second.remove(&17);

    ccp.handle_exec_report(&first, b"", &mut context, &shared, &None, "");
    ccp.handle_exec_report(&second, b"", &mut context, &shared, &None, "");

    assert_eq!(shared.orders.drain_fills().len(), 2, "both slices book");
    assert_eq!(context.position(0), 20.0);
}

/// An execution with no ExecID that arrives ahead of the recovery record
/// for its order. The key must not be spent on the copy that had nothing to
/// book against, or the delivery that finally could is refused.
#[test]
fn a_key_is_not_spent_before_the_order_exists() {
    let mut ccp = CcpState::new();
    let mut context = Context::new();
    let shared = SharedState::new();
    let mut frame = fill_frame(&[]);
    frame.remove(&17);

    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");
    assert!(shared.orders.drain_fills().is_empty(), "nothing to book against yet");

    let instrument = context.register_instrument(756733);
    context.insert_order(crate::types::Order::new(
        42, instrument, Side::Buy, 100 * crate::types::QTY_SCALE, 100 * PRICE_SCALE, b'2', b'0', 0,
    ));
    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");

    assert_eq!(shared.orders.drain_fills().len(), 1, "the execution is still bookable");
    assert_eq!(context.position(0), 10.0);
}
/// A request is recorded as pending only where it went out. Recording it
/// regardless — discarding the send error, pushing outside the block that
/// needs a connection — queues a request issued while the transport is down
/// with nothing on the wire to answer it.
#[test]
fn a_matching_symbols_request_that_was_not_sent_is_not_recorded() {
    let mut ccp = CcpState::new();
    let mut hb = HeartbeatState::new();
    let shared = SharedState::new();

    // No transport at all.
    let mut no_conn: Option<Connection> = None;
    ccp.send_matching_symbols_request(7, "AAPL", &mut no_conn, &mut hb, &shared);
    assert!(
        ccp.pending_matching_symbols.is_empty(),
        "nothing was sent, so nothing is awaiting a reply",
    );

    // And with one, it is recorded.
    let listener = std::net::TcpListener::bind("127.0.1:0").unwrap();
    let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (_peer, _) = listener.accept().unwrap();
    let mut conn = Some(crate::protocol::connection::Connection::new_raw(stream).unwrap());
    ccp.send_matching_symbols_request(8, "AAPL", &mut conn, &mut hb, &shared);
    assert_eq!(ccp.pending_matching_symbols.len(), 1, "a sent request is awaited");
    assert_eq!(ccp.pending_matching_symbols[0].0, 8);
}

/// An advisor's configuration request names its partition on tag 6906.
///
/// Tag 6158 carries the request's own number. The number is stated first and
/// the partition second, which is the order asserted here.
#[test]
fn an_advisor_request_names_the_partition_on_the_tag_that_carries_it() {
    use std::io::Read;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    let mut conn = Some(crate::protocol::connection::Connection::new_raw(stream).unwrap());
    let mut ccp = CcpState::new();
    let mut hb = HeartbeatState::new();
    let mut buf = [0u8; 4096];
    let sent = |peer: &mut std::net::TcpStream, buf: &mut [u8]| -> Vec<(String, String)> {
        let n = peer.read(buf).unwrap();
        String::from_utf8_lossy(&buf[..n])
            .split('\u{1}')
            .filter_map(|f| f.split_once('=').map(|(t, v)| (t.to_string(), v.to_string())))
            .skip_while(|(t, _)| t != "6040")
            .take_while(|(t, _)| t != "10")
            .collect()
    };

    // Asking for one partition: the whole of it, under command five.
    ccp.send_advisor_config(-1, 5, "Profile", 2, None, &mut conn, &mut hb, &SharedState::new());
    let fields = sent(&mut peer, &mut buf);
    let names: Vec<&str> = fields.iter().map(|(t, _)| t.as_str()).collect();
    assert_eq!(names, ["6040", "6905", "6158", "6906"], "{fields:?}");
    assert_eq!(fields[0].1, "116");
    assert_eq!(fields[1].1, "5");
    assert_eq!(fields[2].1, "1", "the first request of the session states itself as one");
    assert_eq!(fields[3].1, "Profile", "the partition, on the tag that carries it");

    // Replacing one carries the document beside it, and the next number.
    ccp.send_advisor_config(77, 3, "Group", 1, Some("<xml/>"), &mut conn, &mut hb, &SharedState::new());
    let fields = sent(&mut peer, &mut buf);
    let names: Vec<&str> = fields.iter().map(|(t, _)| t.as_str()).collect();
    assert_eq!(names, ["6040", "6905", "6158", "6906", "6118"], "{fields:?}");
    assert_eq!(fields[2].1, "2", "each request states a number of its own");
    assert_eq!(fields[3].1, "Group");
    assert_eq!(fields[4].1, "<xml/>");
}

/// The venue's answer to an advisor request reaches the caller who asked.
///
/// It states the number the request went out under and nothing else about
/// what was asked, so a question is told from a replacement by what this
/// client remembered when it sent one. Read as neither, the configuration a
/// caller asked for arrived and went nowhere.
#[test]
fn an_advisor_answer_reaches_the_caller_who_asked_for_it() {
    use std::io::Read;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    let mut conn = Some(crate::protocol::connection::Connection::new_raw(stream).unwrap());
    let mut ccp = CcpState::new();
    let mut hb = HeartbeatState::new();
    let mut context = Context::new();
    let shared = SharedState::new();
    let mut buf = [0u8; 4096];

    // Two questions and a replacement, each under a number of its own.
    ccp.send_advisor_config(-1, 5, "Group", 1, None, &mut conn, &mut hb, &shared);
    ccp.send_advisor_config(-1, 5, "Profile", 2, None, &mut conn, &mut hb, &shared);
    ccp.send_advisor_config(88, 3, "Group", 1, Some("<Groups/>"), &mut conn, &mut hb, &shared);
    let _ = peer.read(&mut buf);

    let reply = |key: &str, text: &str, xml: &str| {
        crate::protocol::fix::fix_build(&[
            (fix::TAG_MSG_TYPE, "U"), (6040, "117"), (6158, key), (58, text), (6118, xml),
        ], 1)
    };
    let feed = |ccp: &mut CcpState, context: &mut Context, frame: &[u8]| {
        ccp.process_ccp_message(
            frame, &mut None, context, &shared, &None, &mut HeartbeatState::new(), "DU1",
        );
    };

    // Answered out of order, which the number is there to survive.
    feed(&mut ccp, &mut context, &reply("2", "", "<Profiles/>"));
    feed(&mut ccp, &mut context, &reply("1", "", "<Groups/>"));
    assert_eq!(
        shared.reference.drain_advisor_config(),
        vec![(2, "<Profiles/>".to_string()), (1, "<Groups/>".to_string())],
        "each partition under the number it was asked for by",
    );

    // The replacement ends rather than answering with a configuration.
    feed(&mut ccp, &mut context, &reply("3", "", ""));
    assert!(shared.reference.drain_advisor_config().is_empty());
    assert_eq!(
        shared.reference.drain_advisor_replaced(),
        vec![(88, "FA data saved".to_string())],
        "the caller's own number for it, and what the reference client states",
    );

    // A number nothing asked under belongs to nobody.
    feed(&mut ccp, &mut context, &reply("9", "", "<Groups/>"));
    assert!(shared.reference.drain_advisor_config().is_empty(), "and is not delivered");
}

/// A venue that will not take the replacement says why, and the caller hears
/// it as trouble rather than as a replacement that stood.
#[test]
fn an_advisor_replacement_the_venue_refuses_is_reported_as_trouble() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (_peer, _) = listener.accept().unwrap();
    let mut conn = Some(crate::protocol::connection::Connection::new_raw(stream).unwrap());
    let mut ccp = CcpState::new();
    let mut hb = HeartbeatState::new();
    let mut context = Context::new();
    let shared = SharedState::new();

    ccp.send_advisor_config(51, 3, "Group", 1, Some("<Groups/>"), &mut conn, &mut hb, &shared);
    let refused = crate::protocol::fix::fix_build(&[
        (fix::TAG_MSG_TYPE, "U"), (6040, "117"), (6158, "1"),
        (58, "group DU1 is not yours"), (6118, ""),
    ], 1);
    ccp.process_ccp_message(
        &refused, &mut None, &mut context, &shared, &None, &mut HeartbeatState::new(), "DU1",
    );

    assert!(shared.reference.drain_advisor_replaced().is_empty(), "it did not stand");
    assert_eq!(
        shared.reference.drain_advisor_refused(),
        vec![(51, 10229, "group DU1 is not yours".to_string())],
    );
}

/// An advisor request the connection outlives is refused, not left waiting.
///
/// Only the connection that was asked can answer: the one that replaces it is
/// asked nothing this one was. Left standing, a caller reading a partition of
/// the configuration waited for ever, and one replacing a partition never
/// learned whether the venue had taken its document — no end, and no refusal.
#[test]
fn an_advisor_request_the_connection_outlives_is_refused() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (_peer, _) = listener.accept().unwrap();
    let mut conn = Some(crate::protocol::connection::Connection::new_raw(stream).unwrap());
    let mut ccp = CcpState::new();
    let mut hb = HeartbeatState::new();
    let mut context = Context::new();
    let shared = SharedState::new();

    ccp.send_advisor_config(51, 3, "Group", 1, Some("<Groups/>"), &mut conn, &mut hb, &shared);
    ccp.handle_disconnect(&mut conn, &mut context, &shared, &None);

    let refused = shared.reference.drain_advisor_refused();
    assert_eq!(refused.len(), 1, "the caller is told, rather than waiting: {refused:?}");
    assert_eq!(refused[0].0, 51, "under the number it asked with");
    assert!(
        shared.reference.drain_advisor_replaced().is_empty(),
        "and not told its document stands",
    );
}

/// An advisor request made with no connection is refused, not dropped.
///
/// Every sibling request on this connection refuses at once in the same
/// state. This one sent nothing, recorded nothing and said nothing, and the
/// connection that replaces it is asked nothing this one was — so there was
/// no later moment at which an answer arrived. A caller reading a partition
/// waited for ever; one replacing a partition held a document it believed the
/// venue had taken.
#[test]
fn an_advisor_request_with_no_connection_is_refused_rather_than_dropped() {
    let mut ccp = CcpState::new();
    let mut hb = HeartbeatState::new();
    let shared = SharedState::new();

    ccp.send_advisor_config(51, 3, "Group", 1, Some("<Groups/>"), &mut None, &mut hb, &shared);

    let refused = shared.reference.drain_advisor_refused();
    assert_eq!(refused.len(), 1, "the caller is told, rather than waiting: {refused:?}");
    assert_eq!(refused[0].0, 51, "under the number it asked with");
    assert!(
        shared.reference.drain_advisor_replaced().is_empty(),
        "and not told its document stands",
    );
}

/// An advisor request the venue never answers is given up on by its own clock.
///
/// Nothing else releases it: the entry was cleared only when the connection
/// next went, which on a session that stays up never happens. A caller
/// reading a partition waited for as long as the process ran.
#[test]
fn an_advisor_request_the_venue_never_answers_is_given_up_on() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (_peer, _) = listener.accept().unwrap();
    let mut conn = Some(crate::protocol::connection::Connection::new_raw(stream).unwrap());
    let mut ccp = CcpState::new();
    let mut hb = HeartbeatState::new();
    let shared = SharedState::new();

    ccp.send_advisor_config(51, 3, "Group", 1, Some("<Groups/>"), &mut conn, &mut hb, &shared);
    ccp.sweep_pending_advisor(&shared);
    assert!(
        shared.reference.drain_advisor_refused().is_empty(),
        "while the wait is still inside its window, nothing is said",
    );

    // The window shut, which is the only thing that changes.
    for asked in ccp.pending_advisor.values_mut() {
        asked.deadline = std::time::Instant::now();
    }
    ccp.sweep_pending_advisor(&shared);

    let refused = shared.reference.drain_advisor_refused();
    assert_eq!(refused.len(), 1, "the caller is told: {refused:?}");
    assert_eq!(refused[0].0, 51, "under the number it asked with");
    assert!(ccp.pending_advisor.is_empty(), "and the question is not kept to match a late reply");
}

/// The venue reads a chain request positionally, so the tags have to be
/// stated in the order it expects them and the underlying has to be named
/// on the tag that suits the derivative being asked for.
#[test]
fn a_chain_request_states_its_tags_in_order() {
    use std::io::Read;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    let mut conn = Some(crate::protocol::connection::Connection::new_raw(stream).unwrap());
    let mut ccp = CcpState::new();
    let mut hb = HeartbeatState::new();
    let shared = SharedState::new();
    let mut buf = [0u8; 4096];
    // The tags a caller cannot see are the session's own; the request
    // itself starts at the sub-message type.
    let sent = |peer: &mut std::net::TcpStream, buf: &mut [u8]| -> Vec<(String, String)> {
        let n = peer.read(buf).unwrap();
        String::from_utf8_lossy(&buf[..n])
            .split('\u{1}')
            .filter_map(|f| f.split_once('=').map(|(t, v)| (t.to_string(), v.to_string())))
            .skip_while(|(t, _)| t != "6040")
            .take_while(|(t, _)| t != "10")
            .collect()
    };

    ccp.send_option_params_request(7, "aapl", "", "STK", 265598, &mut conn, &mut hb, &shared);
    let fields = sent(&mut peer, &mut buf);
    let names: Vec<&str> = fields.iter().map(|(t, _)| t.as_str()).collect();
    assert_eq!(names, ["6040", "55", "310", "6346", "6320", "6994"], "an equity chain: {fields:?}");
    assert_eq!(fields[0].1, "138");
    assert_eq!(fields[1].1, "AAPL", "the symbol is stated upper cased");
    // The underlying's own type. Naming the derivative here is answered
    // "Unknown contract": there is no option contract by that symbol.
    assert_eq!(fields[2].1, "STK");
    assert_eq!(fields[3].1, "265598");
    assert_eq!(ccp.pending_option_params.len(), 1, "and the request awaits its reply");

    ccp.send_option_params_request(8, "ES", "CME", "FUT", 495512563, &mut conn, &mut hb, &shared);
    let fields = sent(&mut peer, &mut buf);
    let names: Vec<&str> = fields.iter().map(|(t, _)| t.as_str()).collect();
    assert_eq!(names, ["6040", "55", "310", "6346", "6320", "6994", "6995"], "a futures chain: {fields:?}");
    assert_eq!(fields[2].1, "FUT", "a future names itself, not its options");
    assert_eq!(fields[6].1, "CME", "and the venue rides only for a future");

    ccp.send_option_params_request(9, "SPX", "CME", "IND", 416904, &mut conn, &mut hb, &shared);
    let fields = sent(&mut peer, &mut buf);
    let names: Vec<&str> = fields.iter().map(|(t, _)| t.as_str()).collect();
    assert_eq!(names, ["6040", "55", "310", "6457", "6320", "6994"], "a futures chain on an index: {fields:?}");
    assert_eq!(fields[2].1, "IND");

    // A caller who states no type claims nothing. Standing STK in asked for a
    // stock's chain on whatever that symbol is, which for an index or a future
    // is a different contract or none.
    ccp.send_option_params_request(10, "SPX", "", "", 416904, &mut conn, &mut hb, &shared);
    let fields = sent(&mut peer, &mut buf);
    let names: Vec<&str> = fields.iter().map(|(t, _)| t.as_str()).collect();
    assert_eq!(names, ["6040", "55", "6346", "6320", "6994"], "an unstated type: {fields:?}");
}

/// A caller is waiting for the end of a request that never reached the
/// wire. Nothing on the socket will ever end it, so the client does.
#[test]
fn a_chain_request_that_could_not_be_sent_is_refused_not_answered_empty() {
    let mut ccp = CcpState::new();
    let mut hb = HeartbeatState::new();
    let shared = SharedState::new();
    let mut no_conn: Option<Connection> = None;

    ccp.send_option_params_request(7, "AAPL", "", "STK", 265598, &mut no_conn, &mut hb, &shared);

    assert!(ccp.pending_option_params.is_empty(), "nothing was sent, so nothing is awaited");
    // An empty chain is one the venue enumerated and found nothing in; a
    // request that never went out is refused instead.
    assert!(shared.reference.drain_option_params().is_empty());
    let refused = shared.reference.drain_historical_errors();
    assert_eq!(refused.len(), 1, "the caller is told it was not sent");
    assert_eq!(refused[0].0, 7);
}

/// The reply states no request id, so the symbol it names is what ties it
/// back to the request, and the conId the caller asked under is what the
/// callback reports.
#[test]
fn a_chain_reply_answers_the_request_that_named_its_underlying() {
    let mut ccp = CcpState::new();
    let shared = SharedState::new();
    ccp.pending_option_params.push((3, "SPY".into(), 756733, Instant::now() + OPTION_CHAIN_TIMEOUT));
    ccp.pending_option_params.push((7, "AAPL".into(), 265598, Instant::now() + OPTION_CHAIN_TIMEOUT));
    let msg = fix::fix_build(
        &[
            (fix::TAG_MSG_TYPE, "U"),
            (6040, "139"),
            (55, "AAPL"),
            (6775, "20260116/20260320/EXPW=20260109"),
            (6346, "265598"),
            (100, "SMART"),
            (6058, "AAPL"),
            (231, "100"),
            (6997, "140.0;145.0"),
        ],
        1,
    );

    ccp.handle_option_chain(&msg, &shared);

    assert_eq!(ccp.pending_option_params.len(), 1, "only the request it answers is spent");
    assert_eq!(ccp.pending_option_params[0].0, 3);
    let answered = shared.reference.drain_option_params();
    assert_eq!(answered.len(), 1);
    let (req_id, con_id, scopes) = &answered[0];
    assert_eq!(*req_id, 7);
    assert_eq!(*con_id, 265598, "the underlying the caller asked about");
    assert_eq!(scopes.len(), 1);
    assert_eq!(scopes[0].exchange, "SMART");
    assert_eq!(scopes[0].trading_class, "AAPL");
    assert_eq!(scopes[0].multiplier, "100");
    assert_eq!(scopes[0].expirations, vec!["20260116", "20260320"]);
    assert_eq!(scopes[0].strikes, vec![140.0, 145.0]);
}

/// An entry left in the queue would both hang its caller and stand ready
/// to absorb the answer to a later request for the same underlying.
#[test]
fn an_unanswered_chain_request_is_given_up_on() {
    let mut ccp = CcpState::new();
    let shared = SharedState::new();
    ccp.pending_option_params.push((7, "AAPL".into(), 265598, Instant::now() - Duration::from_secs(1)));
    ccp.pending_option_params.push((8, "SPY".into(), 756733, Instant::now() + OPTION_CHAIN_TIMEOUT));

    ccp.sweep_pending_option_params(&shared);

    assert_eq!(ccp.pending_option_params.len(), 1, "the expired one is dropped");
    assert_eq!(ccp.pending_option_params[0].0, 8, "and the live one is kept");
    // Told it is over, and told as a refusal: an empty chain is one the
    // venue enumerated and found nothing in, and the venue never answered.
    assert!(shared.reference.drain_option_params().is_empty());
    let refused = shared.reference.drain_historical_errors();
    assert_eq!(refused.len(), 1, "the caller of the expired one is told it is over");
    assert_eq!(refused[0].0, 7);
    assert_eq!(refused[0].1, -1, "as no answer, not as an empty chain");
}

/// Nothing expired an unanswered request, so it stayed queued for the life
/// of the process — and the reply matcher falls back to the head of that
/// queue when a reply carries no echoed request id, so a stale entry could
/// absorb a later request's answer.
#[test]
fn an_unanswered_matching_symbols_request_is_given_up_on() {
    let mut ccp = CcpState::new();
    let shared = SharedState::new();
    ccp.pending_matching_symbols.push((7, Instant::now() - Duration::from_secs(1)));
    ccp.pending_matching_symbols.push((8, Instant::now() + MATCHING_SYMBOLS_TIMEOUT));

    ccp.sweep_pending_matching_symbols(&shared);

    assert_eq!(ccp.pending_matching_symbols.len(), 1, "the expired one is dropped");
    assert_eq!(ccp.pending_matching_symbols[0].0, 8, "and the live one is kept");
    let refused = shared.reference.drain_historical_errors();
    assert_eq!(refused.len(), 1, "the caller of the expired one is told");
    assert_eq!((refused[0].0, refused[0].1), (7, -1), "as no answer, not as an empty search");
}
/// Tag 583 is the link id the engine sends the OCA group on. Reading it
/// back as a parent produced a stable non-zero value shared by every order
/// in the group — none of which has a parent — and nothing told it apart
/// from a real link.
#[test]
fn an_oca_group_is_not_reported_as_a_parent() {
    let (mut ccp, mut context, shared) = ord_status_test_state();
    let frame = exec_report_frame(&[
        (39, "0"), (150, "0"), (100, "ARCA"), (198, "ARCA:1"),
        (583, "PROBE-OCA-1"),
    ]);

    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");

    let updates = shared.orders.drain_order_updates();
    assert_eq!(updates.len(), 1, "the status is still reported");
    assert_eq!(
        updates[0].parent_id, 0,
        "an order in an OCA group has no parent, so none is reported",
    );
}

/// Not just the one group name, and not just one status: any value on 583
/// is a link id rather than a parent, at every point in the order's life.
#[test]
fn no_group_name_or_status_produces_a_parent() {
    for group in ["PROBE-OCA-1", "G", "12345", "a name with spaces"] {
        for (ord_status, exec_type) in [("0", "0"), ("1", "2"), ("2", "2"), ("4", "4")] {
            let (mut ccp, mut context, shared) = ord_status_test_state();
            let frame = exec_report_frame(&[
                (39, ord_status), (150, exec_type), (100, "ARCA"), (198, "ARCA:1"),
                (583, group),
            ]);
            ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");
            let updates = shared.orders.drain_order_updates();
            // Without this a case that produced no update at all would
            // pass the loop below by never entering it.
            assert_eq!(
                updates.len(), 1,
                "group {group:?} at status {ord_status} must produce one update",
            );
            assert_eq!(
                updates[0].parent_id, 0,
                "group {group:?} at status {ord_status} must not become a parent",
            );
        }
    }
}

/// A report carrying no group reported no parent before this change too, so
/// that case alone cannot tell the fix from the bug.
#[test]
fn a_report_without_a_group_still_has_no_parent() {
    let (mut ccp, mut context, shared) = ord_status_test_state();
    let frame = exec_report_frame(&[(39, "0"), (150, "0"), (100, "ARCA"), (198, "ARCA:1")]);
    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");
    let updates = shared.orders.drain_order_updates();
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0].parent_id, 0);
}

/// Tag 6107 is what the bracket path sends a parent on, and a report states a
/// parent there in the same shape — a whole number with a fraction.
///
/// A bare number on the same tag is the other thing it has been seen carrying:
/// one value shared by every order in the account. Read as a parent, each of
/// them is given a parent that does not exist, and a caller re-placing a leg
/// from that record links it to an order that is not its own. A parent that is
/// missing is recoverable; one that is invented is not.
#[test]
fn a_parent_is_read_back_only_in_the_shape_a_parent_is_written_in() {
    let (mut ccp, mut context, shared) = ord_status_test_state();
    let stated = exec_report_frame(&[
        (39, "0"), (150, "0"), (100, "ARCA"), (198, "ARCA:1"), (6107, "4242.0"),
    ]);
    ccp.handle_exec_report(&stated, b"", &mut context, &shared, &None, "");
    let updates = shared.orders.drain_order_updates();
    assert_eq!(
        updates[0].parent_id, 4242,
        "the parent the venue restated reaches the caller",
    );

    let (mut ccp, mut context, shared) = ord_status_test_state();
    let shared_across_the_account = exec_report_frame(&[
        (39, "0"), (150, "0"), (100, "ARCA"), (198, "ARCA:1"), (6107, "4242"),
    ]);
    ccp.handle_exec_report(&shared_across_the_account, b"", &mut context, &shared, &None, "");
    let updates = shared.orders.drain_order_updates();
    assert_eq!(
        updates[0].parent_id, 0,
        "and a bare number is not made into one",
    );
}

/// A refused revision arrives on the same message as an accepted one and
/// was read as the acceptance, so a modify the gateway would not make was
/// reported to the caller as made.
#[test]
fn a_refused_revision_is_not_an_acknowledgement() {
    let (mut ccp, mut context, shared) = ord_status_test_state();
    let frame = exec_report_frame(&[
        (39, "5"), (150, "5"), (100, "ARCA"), (198, "ARCA:1"), (378, "102"),
    ]);
    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");
    let updates = shared.orders.drain_order_updates();
    assert!(
        !updates.iter().any(|u| u.status == crate::types::OrderStatus::Submitted),
        "a refused revision does not put the order back to working: {updates:?}",
    );
}

/// A busted trade arrives as an execution like any other. Its quantity
/// reconciles against the order's cumulative figure rather than adding to it,
/// and the reconciliation may be negative.
#[test]
fn a_busted_execution_reconciles_rather_than_adds() {
    let (mut ccp, mut context, shared) = ord_status_test_state();
    // The order has already filled fifty, and the position holds them.
    let booked = exec_report_frame(&[
        (39, "1"), (150, "F"), (100, "ARCA"), (198, "ARCA:1"),
        (17, "exec-1"), (32, "50"), (31, "412.25"), (14, "50"), (38, "100"),
    ]);
    ccp.handle_exec_report(&booked, b"", &mut context, &shared, &None, "");
    assert_eq!(context.order(42).unwrap().filled, 50 * QTY_SCALE, "the trade is booked");
    assert_eq!(context.position(0), 50.0);
    let _ = shared.orders.drain_fills();

    // The venue busts it: the cumulative quantity goes back to nothing.
    let bust = exec_report_frame(&[
        (39, "1"), (150, "F"), (100, "ARCA"), (198, "ARCA:1"),
        (17, "exec-2"), (20, "1"), (32, "50"), (31, "412.25"), (14, "0"), (38, "100"),
    ]);
    ccp.handle_exec_report(&bust, b"", &mut context, &shared, &None, "");

    assert_eq!(
        context.order(42).unwrap().filled, 0,
        "the order no longer holds a trade the venue undid",
    );
    assert_eq!(context.position(0), 0.0, "and neither does the position");
    let fills = shared.orders.drain_fills();
    assert_eq!(fills.len(), 1);
    let bust_fill = &fills[0];
    assert_eq!(bust_fill.0.qty, 50 * QTY_SCALE, "the execution states tag 32 even when the booking is negative");
    // The report states zero, and zero is what the caller reads. Filtering
    // the stated figure on being positive fell back to adding the print to
    // what was already booked, and the caller was told the order had twice
    // its quantity filled while everything was still remaining.
    assert_eq!(
        bust_fill.0.cum_qty, 0,
        "the order total goes back to nothing with the trade: {bust_fill:?}",
    );
    assert_eq!(
        bust_fill.0.remaining, 100 * QTY_SCALE,
        "the whole order is still working: {bust_fill:?}",
    );
}

/// A replay or correction reconciles what the order holds, but the execution
/// still reports its own quantity on tag 32.
#[test]
fn a_reconciled_execution_reports_tag_32_and_books_only_the_delta() {
    for (exec_type, tag, value) in [("F", 20, "2"), ("G", 20, "0"), ("F", 97, "Y"), ("F", 43, "Y")] {
        let (mut ccp, mut context, shared) = tracked_order_state();
        let first = fix::fix_build(&[
            (35, "8"), (11, "42"), (39, "1"), (150, "F"),
            (17, "exec-1"), (32, "50"), (31, "412.25"), (14, "50"), (38, "100"),
        ], 1);
        ccp.process_ccp_message(&first, &mut None, &mut context, &shared,
            &None, &mut HeartbeatState::new(), "");
        assert_eq!(shared.orders.drain_fills()[0].0.qty, 50 * QTY_SCALE);
        assert_eq!(context.order(42).unwrap().filled, 50 * QTY_SCALE);
        assert_eq!(context.position(0), 50.0);

        let restated = fix::fix_build(&[
            (35, "8"), (11, "42"), (39, "1"), (150, exec_type), (tag, value),
            (17, "exec-2"), (32, "60"), (31, "412.25"), (14, "60"), (38, "100"),
        ], 2);
        ccp.process_ccp_message(&restated, &mut None, &mut context, &shared,
            &None, &mut HeartbeatState::new(), "");
        let fills = shared.orders.drain_fills();
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].0.qty, 60 * QTY_SCALE, "the execution reports tag 32, not the booking delta");
        assert_eq!(fills[0].0.cum_qty, 60 * QTY_SCALE);
        assert_eq!(context.order(42).unwrap().filled, 60 * QTY_SCALE, "only ten more are booked");
        assert_eq!(context.position(0), 60.0, "only ten more are held");
    }
}

/// A repeated correction states the total before a later fill, which still
/// belongs to both the order and the position.
#[test]
fn a_replayed_correction_does_not_erase_a_later_fill() {
    for (exec_type, trans_type, cumulative) in [
        ("H", "0", 0), ("G", "0", 60), ("F", "1", 0), ("F", "2", 60),
    ] {
        for marker in [None, Some(97), Some(43)] {
            let (mut ccp, mut context, shared) = tracked_order_state();
            let first = fill_frame(&[(17, "E1"), (32, "50"), (14, "50"), (151, "50")]);
            ccp.handle_exec_report(&first, b"", &mut context, &shared, &None, "");
            assert_eq!(context.order(42).unwrap().filled, 50 * QTY_SCALE);
            assert_eq!(context.position(0), 50.0);
            let _ = shared.orders.drain_fills();

            let mut correction = fill_frame(&[
                (17, "E2"), (150, exec_type), (20, trans_type),
                (32, if cumulative == 0 { "50" } else { "60" }),
                (14, &cumulative.to_string()), (151, &(100 - cumulative).to_string()),
            ]);
            ccp.handle_exec_report(&correction, b"", &mut context, &shared, &None, "");
            let fills = shared.orders.drain_fills();
            assert_eq!(fills.len(), 1, "an unseen correction still reconciles");
            assert_eq!(fills[0].0.qty, if cumulative == 0 { 50 } else { 60 } * QTY_SCALE);
            assert_eq!(context.order(42).unwrap().filled, cumulative * QTY_SCALE);
            assert_eq!(context.position(0), cumulative as f64);

            let later = fill_frame(&[
                (17, "E3"), (32, "20"), (14, &(cumulative + 20).to_string()),
                (151, &(80 - cumulative).to_string()),
            ]);
            ccp.handle_exec_report(&later, b"", &mut context, &shared, &None, "");
            assert_eq!(shared.orders.drain_fills().len(), 1);
            if let Some(tag) = marker { correction.insert(tag, "Y".to_string()); }
            ccp.handle_exec_report(&correction, b"", &mut context, &shared, &None, "");

            assert_eq!(context.order(42).unwrap().filled, (cumulative + 20) * QTY_SCALE);
            assert_eq!(context.position(0), (cumulative + 20) as f64);
            assert!(shared.orders.drain_fills().is_empty(), "the correction books only once");
        }
    }
}

/// A live order was retired by this: `D` is not in the terminal's terminal
/// set, and reading it as cancelled told the caller an order was gone while
/// it was still working and still able to fill.
#[test]
fn a_pending_status_does_not_retire_the_order() {
    let (mut ccp, mut context, shared) = ord_status_test_state();
    let frame = exec_report_frame(&[(39, "D"), (150, "D"), (100, "ARCA"), (198, "ARCA:1")]);
    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");
    let updates = shared.orders.drain_order_updates();
    assert_eq!(updates[0].status, crate::types::OrderStatus::PendingCancel,
        "D is pending, not cancelled");
    assert_ne!(updates[0].status, crate::types::OrderStatus::Cancelled);
}

/// The venue treats 39=E as a pending cancel, so that is what reaches the
/// caller.
///
/// Published under a word of its own, it was in neither the working set nor the
/// finished set of a program written against the vocabulary this client answers
/// in.
#[test]
fn a_pending_change_is_reported_as_the_venue_treats_it() {
    let (mut ccp, mut context, shared) = ord_status_test_state();
    // The order is working before the change is asked of the venue.
    let working = exec_report_frame(&[(39, "0"), (150, "0"), (100, "ARCA"), (198, "ARCA:1")]);
    ccp.handle_exec_report(&working, b"", &mut context, &shared, &None, "");
    let _ = shared.orders.drain_order_updates();

    let pending = exec_report_frame(&[(39, "E"), (150, "E"), (100, "ARCA"), (198, "ARCA:1")]);
    ccp.handle_exec_report(&pending, b"", &mut context, &shared, &None, "");

    let updates = shared.orders.drain_order_updates();
    assert_eq!(
        updates[0].status, crate::types::OrderStatus::PendingCancel,
        "read as the venue treats it: {updates:?}",
    );
    let info = shared.orders.get_order_info(42).expect("the record a caller reads back");
    assert_eq!(
        info.order_state.status, "PendingCancel",
        "and the record a caller reads back says the same",
    );
    assert!(
        shared.orders.drain_open_orders().iter().any(|(id, _)| *id == 42),
        "and an order mid-modification is still on the open book",
    );
}

/// The fill was thrown away with the report: an unrecognised status returned
/// before anything read the execution, so a real fill on a status this did
/// not know about was silently lost.
#[test]
fn an_unknown_status_still_books_its_fill() {
    let (mut ccp, mut context, shared) = ord_status_test_state();
    let frame = exec_report_frame(&[
        (39, "\u{7}"), (150, "F"), (100, "ARCA"), (198, "ARCA:1"),
        (32, "50"), (31, "412.25"), (14, "50"), (6, "412.25"), (38, "100"),
    ]);
    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");
    let fills = shared.orders.drain_fills();
    assert_eq!(fills.len(), 1, "the fill survives a status this does not know");
    assert_eq!(fills[0].0.qty, 50 * QTY_SCALE);
}

/// Absent is not zero. Without 151 the caller was told nothing was left on an
/// order that was still working, which reads as done.
#[test]
fn a_missing_leaves_qty_falls_back_to_what_is_unfilled() {
    let (mut ccp, mut context, shared) = ord_status_test_state();
    let frame = exec_report_frame(&[
        (39, "1"), (150, "1"), (100, "ARCA"), (198, "ARCA:1"),
        (38, "100"), (14, "30"), (32, "30"), (31, "412.25"), (6, "412.25"),
    ]);
    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");
    let updates = shared.orders.drain_order_updates();
    assert_eq!(updates[0].remaining_qty, 70.0, "100 ordered less 30 filled, not 0");
}

/// A report is written down in full before any of it is announced.
///
/// A caller acts on a notification the moment it arrives — withdrawing the
/// order it names, reading the fill, listing what has finished — and each of
/// those asks this session for a record. Announcing first meant answering those
/// questions about a report still being applied: the caller was told, asked,
/// and was told no such thing had happened.
#[test]
fn a_finished_order_is_written_down_before_it_is_announced() {
    let (mut ccp, mut context, shared) = ord_status_test_state();
    let (tx, rx) = std::sync::mpsc::sync_channel(8);
    let sink = Some(crate::engine::hot_loop::EventSink::new(
        tx,
        std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
    ));
    let frame = exec_report_frame(&[
        (39, "2"), (150, "2"), (100, "ARCA"), (198, "ARCA:1"),
        (38, "100"), (14, "100"), (32, "100"), (31, "412.25"), (6, "412.25"),
        (151, "0"), (6008, "265598"),
    ]);
    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &sink, "");

    // Everything the report changed is readable.
    assert_eq!(shared.orders.drain_fills().len(), 1, "the fill is recorded");
    assert_eq!(
        shared.orders.drain_completed_orders().len(),
        1,
        "and so is the order having finished",
    );
    assert_eq!(shared.orders.drain_order_updates().len(), 1, "and the status it finished in");

    // And the fill was announced before the status that followed from it.
    let announced: Vec<_> = rx.try_iter().collect();
    let kinds: Vec<&str> = announced
        .iter()
        .map(|e| match e {
            crate::engine::hot_loop::Event::Fill(_) => "fill",
            crate::engine::hot_loop::Event::OrderUpdate(_) => "status",
            _ => "other",
        })
        .collect();
    assert_eq!(kinds, ["fill", "status"], "what traded, then where the order stands");
}

/// The order id hash on tag 37 is a separate concern and must keep working.
#[test]
fn the_order_id_still_produces_a_stable_perm_id() {
    let (mut ccp, mut context, shared) = ord_status_test_state();
    let frame = exec_report_frame(&[
        (39, "0"), (150, "0"), (100, "ARCA"), (198, "ARCA:1"),
        (37, "0256d0f1.0001417e.6a6982d2.0001"),
    ]);
    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");
    let updates = shared.orders.drain_order_updates();
    assert_ne!(updates[0].perm_id, 0, "the order id still yields a permId");
}

/// The recovered side is not confined to the recovered record: every later
/// fill for that order books through the tracked path and takes its side
/// from here, so a guess moves the position by twice the fill in the wrong
/// direction, and nothing afterwards distinguishes it from a stated side.
#[test]
fn a_recovery_record_without_a_side_is_not_tracked() {
    for missing in ["", "9", "X"] {
        let mut context = Context::new();
        let mut ccp = CcpState::new();
        let shared = SharedState::new();
        let mut frame = std::collections::HashMap::new();
        for (tag, val) in [
            (11u32, "77"), (150, "0"), (39, "0"), (6008, "756733"), (38, "100"), (55, "SPY"),
        ] {
            frame.insert(tag, val.to_string());
        }
        if !missing.is_empty() {
            frame.insert(54, missing.to_string());
        }

        ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");

        assert!(
            context.order(77).is_none(),
            "Side={missing:?} must not be guessed into a tracked order",
            );
    }
}

/// A stated side is still recovered.
#[test]
fn a_recovery_record_with_a_side_is_tracked() {
    for (tag54, expected) in [("1", Side::Buy), ("2", Side::Sell), ("5", Side::ShortSell)] {
        let mut context = Context::new();
        let mut ccp = CcpState::new();
        let shared = SharedState::new();
        let mut frame = std::collections::HashMap::new();
        for (tag, val) in [
            (11u32, "77"), (150, "0"), (39, "0"), (6008, "756733"), (38, "100"),
            (55, "SPY"), (54, tag54),
        ] {
            frame.insert(tag, val.to_string());
        }

        ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");

        let order = context.order(77).expect("a stated side is recovered");
        assert_eq!(order.side, expected, "Side={tag54}");
    }
}

/// An order is named to a caller as the reference client names it.
///
/// The venue states the type on tag 40 and, for the four that travel as `P`,
/// tells them apart on tag 18. Read from tag 40 alone, a relative order and
/// both pegs were answered as `TRAIL`, and every multi-letter name as the wire
/// spells it — `TSL`, `SMID`, `MIDPX` — which no program written against the
/// reference client knows. A trailing stop limit's limit offset rides tag 6370
/// and was read from nowhere.
#[test]
fn an_order_is_named_as_the_reference_client_names_it() {
    let cases: [(&[(u32, &str)], &str); 17] = [
        (&[(40, "P"), (18, "R")], "REL"), (&[(40, "P"), (18, "M")], "PEG MID"),
        (&[(40, "P"), (18, "P")], "PEG MKT"), (&[(40, "P"), (18, "a")], "TRAIL"),
        (&[(40, "TSL")], "TRAIL LIMIT"), (&[(40, "SMID")], "SNAP MID"), (&[(40, "SMKT")], "SNAP MKT"),
        (&[(40, "SREL")], "SNAP PRI"), (&[(40, "MIDPX")], "MIDPRICE"), (&[(40, "PSVR")], "PASSV REL"),
        (&[(40, "PB")], "PEG BENCH"), (&[(40, "E2M")], "PEG BEST"), (&[(40, "LT")], "LIT"),
        (&[(40, "SP")], "STP PRT"), (&[(40, "U")], "MKT PRT"), (&[(40, "K")], "MTL"), (&[(40, "PMID2")], "PEG MID"),
    ];
    for (typed, name) in cases {
        let (mut ccp, mut context, shared) = (CcpState::new(), Context::new(), SharedState::new());
        let mut pairs: Vec<(u32, &str)> = vec![
            (11, "77"), (150, "0"), (39, "0"), (6008, "756733"), (38, "1"), (55, "SPY"), (54, "1"), (6370, "0.1"),
        ];
        pairs.extend_from_slice(typed);
        let frame: std::collections::HashMap<u32, String> =
            pairs.iter().map(|(t, v)| (*t, v.to_string())).collect();
        ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "DU1");
        let order = shared.orders.get_order_info(77).expect("published").order;
        assert_eq!(order.order_type, name, "{typed:?}");
        assert_eq!(order.lmt_price_offset, 0.1, "the limit offset is read off its tag: {typed:?}");
    }
}

/// An order the replay names as replaced is recovered like one it names as
/// new, so the report that ends it reaches the caller.
///
/// Measured on a paper session: orders one session had placed and replaced
/// were named to the next as replaced, under the version the replace gave
/// them, and were not brought into the book — so when that session withdrew
/// them the venue's cancelled reports, under the cancel's own id, matched
/// nothing and no caller heard the orders were gone.
#[test]
fn an_order_the_replay_names_as_replaced_is_recovered_and_its_end_is_heard() {
    let (mut ccp, mut context, shared) = (CcpState::new(), Context::new(), SharedState::new());
    let (tx, rx) = std::sync::mpsc::sync_channel(64);
    let sink = Some(crate::engine::hot_loop::EventSink::new(tx, Default::default()));
    let frame = |pairs: &[(u32, &str)]| -> std::collections::HashMap<u32, String> {
        pairs.iter().map(|(t, v)| (*t, v.to_string())).collect()
    };
    let named = [(6008u32, "756733"), (38, "1"), (55, "SPY"), (54, "1"), (40, "P"), (18, "R"), (99, "0.05")];
    let mut replaced = vec![(11u32, "77.1"), (41, "77.0"), (150, "5"), (39, "5")];
    replaced.extend_from_slice(&named);
    ccp.handle_exec_report(&frame(&replaced), b"", &mut context, &shared, &sink, "DU1");
    assert!(context.order(77).is_some(), "a replaced order the venue holds is in the book");

    let mut cancelled = vec![(11u32, "C77"), (41, "77.1"), (150, "4"), (39, "4")];
    cancelled.extend_from_slice(&named);
    ccp.handle_exec_report(&frame(&cancelled), b"", &mut context, &shared, &sink, "DU1");
    let heard: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok())
        .filter_map(|e| match e {
            Event::OrderUpdate(u) if u.order_id == 77 => Some(u.status),
            _ => None,
        })
        .collect();
    assert!(heard.contains(&crate::types::OrderStatus::Cancelled), "the caller hears the order is gone: {heard:?}");
}

/// A replace of an order recovered from the venue's naming carries the shape
/// the caller states, in the frame a placement's replace carries.
///
/// Composed end to end: the replay's own record of a replaced midpoint peg,
/// the caller's statement of it, then a replace of the quantity alone. The
/// venue refused the first such replace as an unsupported type, which a
/// placement's replace of the same order is not.
#[test]
fn a_replayed_pegs_replace_carries_the_shape_a_placements_does() {
    use std::io::Read;
    use crate::types::{OrderKind as K, PRICE_SCALE as P};
    let (mut ccp, mut context, shared) = (CcpState::new(), Context::new(), SharedState::new());
    let frame: std::collections::HashMap<u32, String> = [
        (11u32, "77.2"), (41, "77.1"), (150, "5"), (39, "5"), (6008, "756733"), (38, "2"),
        (55, "SPY"), (54, "1"), (40, "P"), (18, "M"), (44, "101"), (99, "0.00"), (1, "DU1"),
    ].into_iter().map(|(t, v)| (t, v.to_string())).collect();
    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "DU1");
    let recovered = context.order(77).expect("recovered");
    let instrument = recovered.instrument;
    context.set_symbol(instrument, "SPY".to_string());

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    peer.set_read_timeout(Some(std::time::Duration::from_millis(300))).unwrap();
    let mut conn = Some(crate::protocol::connection::Connection::new_raw(stream).unwrap());
    let mut hb = crate::engine::hot_loop::HeartbeatState::new();
    let shared = std::sync::Arc::new(SharedState::new());
    context.pending_orders.push(crate::types::OrderRequest::Modify {
        order_id: 77, price: 0, qty: 3 * crate::types::QTY_SCALE, outside_rth: false,
        ord_type: 0, tif: 0, stop_price: 0,
        spec: Some(Box::new(crate::types::OrderSpec {
            kind: K::PegMid { offset: 0, price_cap: 101 * P },
            attrs: crate::types::OrderAttrs::default(),
        })),
    });
    crate::engine::hot_loop::order_builder::drain_and_send_orders(
        &mut conn, &mut context, "DU1", &mut hb, false, &shared, false, &None,
    );
    let mut buf = [0u8; 8192];
    let n = peer.read(&mut buf).unwrap_or(0);
    let msg = String::from_utf8_lossy(&buf[..n]).to_string();
    let tag = |t: &str| msg.split('\u{1}').filter_map(|f| f.strip_prefix(t)).collect::<Vec<_>>();
    assert_eq!(tag("35="), ["G"], "sent: {msg}");
    assert_eq!(tag("11="), ["77.3"], "named past the revision the venue holds: {msg}");
    assert_eq!(tag("41="), ["77.2"], "{msg}");
    assert_eq!((tag("40="), tag("18=")), (vec!["P"], vec!["M"]), "the type and its instruction: {msg}");
    assert_eq!((tag("44="), tag("211=")), (vec!["101"], vec!["0"]), "the cap and the offset: {msg}");
    assert_eq!(tag("38="), ["3"], "{msg}");
    assert!(shared.orders.drain_order_inactive().is_empty());
}

/// A report that states no type leaves a tracked peg named as a peg.
///
/// Both pegs travel as `P` and are told apart by the instruction beside it;
/// the fallback from this client's own byte supplied no instruction, so a
/// report without tag 40 renamed a midpoint peg `TRAIL` in the row a caller
/// reads, and a replace of it was then judged against that.
#[test]
fn a_report_stating_no_type_leaves_a_tracked_peg_named_as_a_peg() {
    for (byte, name) in [(crate::types::ORD_PEG_MID, "PEG MID"), (crate::types::ORD_PEG_MKT, "PEG MKT")] {
        let (mut ccp, mut context, shared) = ord_status_test_state();
        context.insert_order(crate::types::Order::new(
            42, 0, Side::Buy, crate::types::QTY_SCALE, 100 * PRICE_SCALE, byte, b'0', 0,
        ));
        let frame = exec_report_frame(&[(39, "0"), (150, "0"), (100, "ARCA")]);
        ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");
        let order = shared.orders.get_order_info(42).expect("published").order;
        assert_eq!(order.order_type, name, "byte {byte}");
    }
}

/// A correction that recovers an order does not book the corrected shares as
/// a fill, and a bust stated as a negative print does not raise the figure.
///
/// The seed that keeps a first fill from counting twice subtracts the
/// report's own shares from the cumulative figure, because the booking that
/// follows adds them. A correction or a bust is booked by reconciling the
/// cumulative figure against the record instead, where the seed is subtracted
/// — so the same subtraction there turned a forty-share correction into a
/// forty-share purchase, and a negative print into a higher figure.
#[test]
fn a_correction_that_recovers_an_order_books_no_purchase() {
    // An order the book holds as uncertain after a drop, filled a hundred.
    let (mut ccp, mut context, shared) = (CcpState::new(), Context::new(), SharedState::new());
    let instrument = context.register_instrument(756733);
    let mut held = crate::types::Order::new(42, instrument, Side::Buy, 100 * crate::types::QTY_SCALE, 500 * PRICE_SCALE, b'2', b'0', 0);
    held.filled = 100 * crate::types::QTY_SCALE;
    held.status = crate::types::OrderStatus::PartiallyFilled;
    context.insert_order(held);
    context.mark_orders_uncertain();
    let position_before = context.position(instrument);
    let correction: std::collections::HashMap<u32, String> = [
        (11u32, "42.0"), (150, "1"), (20, "1"), (39, "1"), (54, "1"), (6008, "756733"), (38, "100"),
        (32, "40"), (14, "60"), (31, "500.0"), (55, "SPY"), (17, "exec-c"),
    ].into_iter().map(|(t, v)| (t, v.to_string())).collect();
    ccp.handle_exec_report(&correction, b"", &mut context, &shared, &None, "DU1");
    assert_eq!(context.order(42).expect("kept").filled, 60 * crate::types::QTY_SCALE, "the venue's cumulative figure");
    assert!(context.position(instrument) <= position_before, "a correction is not a purchase");
    assert!(shared.orders.drain_fills().iter().all(|(f, _)| f.order_id != 42), "and no fill is announced for it");

    // A bust stated as a negative print, on an order the book lacks.
    let (mut ccp, mut context, shared) = (CcpState::new(), Context::new(), SharedState::new());
    let bust: std::collections::HashMap<u32, String> = [
        (11u32, "43.0"), (150, "F"), (39, "1"), (54, "1"), (6008, "756733"), (38, "100"),
        (32, "-40"), (14, "60"), (31, "500.0"), (55, "SPY"), (17, "exec-b"),
    ].into_iter().map(|(t, v)| (t, v.to_string())).collect();
    ccp.handle_exec_report(&bust, b"", &mut context, &shared, &None, "DU1");
    assert_eq!(context.order(43).expect("recovered").filled, 60 * crate::types::QTY_SCALE, "not raised by a negative print");

    // A correction by execution type rather than by transaction type, on an
    // order the book lacks: reconciled, so nothing is booked as a purchase.
    let (mut ccp, mut context, shared) = (CcpState::new(), Context::new(), SharedState::new());
    let typed: std::collections::HashMap<u32, String> = [
        (11u32, "44.0"), (150, "G"), (39, "1"), (54, "1"), (6008, "756733"), (38, "100"),
        (32, "40"), (14, "60"), (31, "500.0"), (55, "SPY"), (17, "exec-g"),
    ].into_iter().map(|(t, v)| (t, v.to_string())).collect();
    ccp.handle_exec_report(&typed, b"", &mut context, &shared, &None, "DU1");
    let order = context.order(44).expect("recovered");
    assert_eq!(order.filled, 60 * crate::types::QTY_SCALE);
    assert_eq!(context.position(order.instrument), 0.0, "a correction is not a purchase");
    assert!(shared.orders.drain_fills().iter().all(|(f, _)| f.order_id != 44));
}

/// A fill that is the first this session hears of an order books its shares
/// once.
///
/// Such a report recovers the order and books the fill in one pass. Recovery
/// took the filled quantity from the report's cumulative figure, which already
/// counts this report's own shares, and the booking then added them again: an
/// order that had filled forty read as eighty, while the position moved by
/// forty.
#[test]
fn a_fill_that_recovers_an_order_books_its_shares_once() {
    let (mut ccp, mut context, shared) = (CcpState::new(), Context::new(), SharedState::new());
    let frame: std::collections::HashMap<u32, String> = [
        (11u32, "77.0"), (150, "1"), (39, "1"), (6008, "756733"), (38, "100"), (55, "SPY"), (54, "1"),
        (40, "2"), (44, "100"), (32, "40"), (31, "100"), (14, "40"), (151, "60"), (17, "exec-1"),
    ].into_iter().map(|(t, v)| (t, v.to_string())).collect();
    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "DU1");
    let order = context.order(77).expect("recovered by its first fill");
    assert_eq!(order.filled, 40 * crate::types::QTY_SCALE, "filled once, not once by recovery and once by the booking");
    assert_eq!(context.position(order.instrument), 40.0, "and the position moved by the fill");
}

/// An unrecognised or absent tag 59 leaves the wire match with nothing to
/// report, so the fallback that knows what the caller submitted can run. An
/// arm producing `DAY` for those cases keeps the fallback from ever
/// running, and `DAY` is an ordinary value: a caller reconciling its own
/// orders gets a plausible answer that disagrees with what it sent.
#[test]
fn an_unknown_time_in_force_falls_back_to_the_one_that_was_submitted() {
    // A tracked order submitted GTC, so a wrong answer is visibly wrong.
    let tracked = |ccp: &mut CcpState, context: &mut Context, shared: &SharedState, tif59: Option<&str>| {
        context.insert_order(crate::types::Order::new(
            42, 0, Side::Buy, crate::types::QTY_SCALE, 100 * PRICE_SCALE, b'2', b'1', 0,
        ));
        let mut pairs = vec![(39u32, "0"), (150u32, "0"), (100u32, "ARCA"), (198u32, "ARCA:1")];
        if let Some(v) = tif59 {
            pairs.push((59, v));
        }
        let frame = exec_report_frame(&pairs);
        ccp.handle_exec_report(&frame, b"", context, shared, &None, "");
        shared.orders.get_order_info(42).expect("published").order.tif.clone()
    };

    // Absence is the only case the fallback answers: the report states no
    // time-in-force, and this client knows what it submitted.
    let (mut ccp, mut context, shared) = ord_status_test_state();
    assert_eq!(
        tracked(&mut ccp, &mut context, &shared, None), "GTC",
        "the submitted time-in-force, not a plausible default",
    );

    // A stated code is still taken from the wire, including one that
    // happens to differ from the tracked order — the gateway is
    // authoritative when it says anything at all.
    let (mut ccp, mut context, shared) = ord_status_test_state();
    assert_eq!(tracked(&mut ccp, &mut context, &shared, Some("0")), "DAY");
    let (mut ccp, mut context, shared) = ord_status_test_state();
    assert_eq!(tracked(&mut ccp, &mut context, &shared, Some("4")), "FOK");

    // Including a code this does not name: seen as stated rather than
    // silently replaced by the local order's unrelated value.
    let (mut ccp, mut context, shared) = ord_status_test_state();
    assert_eq!(tracked(&mut ccp, &mut context, &shared, Some("5")), "5");
}

/// An order the venue replayed is paired with the number it is reachable under
/// here, and the caller is told.
///
/// The venue replays what the account is working when a session opens, and this
/// client gives each of those a number of its own. It worked that pairing out
/// and never said it: a caller holding the permanent id the venue keeps had no
/// way to reach the order here, and the callback that carries the pairing was
/// one nothing fired.
///
/// Said once. The same orders come back on every reconnect, and a caller
/// counting them would be counting reconnects.
#[test]
fn a_replayed_order_is_paired_with_the_number_it_is_reachable_under() {
    let mut ccp = CcpState::new();
    let mut context = Context::new();
    let shared = SharedState::new();

    let mut frame = std::collections::HashMap::new();
    for (tag, val) in [
        (11u32, "91"), (150u32, "0"), (39u32, "0"), (6008u32, "756733"),
        (38u32, "100"), (55u32, "SPY"), (54u32, "1"), (40u32, "2"),
        (37u32, "1234567890"),
    ] {
        frame.insert(tag, val.to_string());
    }
    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");

    let bound = shared.reference.drain_orders_bound();
    assert_eq!(bound.len(), 1, "the pairing is stated: {bound:?}");
    let (perm_id, client_id, order_id) = bound[0];
    assert_eq!(order_id, 91, "the number this client reaches it under");
    assert_ne!(perm_id, 0, "and the permanent id the venue keeps");
    assert_eq!(client_id, 0, "claimed for no client in particular, as it is owned by none");

    // And the connection goes. Every working order is marked uncertain by
    // that, and an uncertain order is recovered again when the venue replays
    // it — which is the path that states the pairing a second time, and the
    // one a check against the queue alone cannot see, because the queue was
    // just emptied by the drain above.
    ccp.handle_disconnect(&mut None, &mut context, &shared, &None);
    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");
    assert!(
        shared.reference.drain_orders_bound().is_empty(),
        "one pairing, however many times the venue states it and however many \
         times the connection goes",
    );
}

/// The case the test above cannot reach: an order this session never
/// placed, arriving on the session-start recovery push with no tag 59.
///
/// There is nothing to recover the time-in-force from, and an invented one
/// would be read as though it were the caller's own. An invented GTC rests
/// until cancelled; an invented DAY expires with the session. Neither is
/// knowledge, so the safer of the two is the one that does not leave an
/// order resting.
#[test]
fn a_recovered_order_without_a_time_in_force_states_none() {
    let mut ccp = CcpState::new();
    let mut context = Context::new();
    let shared = SharedState::new();

    // A recovery record: not tracked locally, states a contract and size,
    // states no time-in-force.
    let mut frame = std::collections::HashMap::new();
    for (tag, val) in [
        (11u32, "78"), (150u32, "0"), (39u32, "0"), (6008u32, "756733"),
        (38u32, "100"), (55u32, "SPY"), (54u32, "1"), (40u32, "2"),
    ] {
        frame.insert(tag, val.to_string());
    }
    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");

    assert_eq!(
        context.order(78).expect("recovered").tif, crate::types::TIF_UNSTATED,
        "an absent time-in-force is recorded as unstated, not guessed",
    );
    assert_eq!(
        shared.orders.get_order_info(78).expect("published").order.tif, "",
        "and is reported as unstated rather than as an ordinary value",
    );

    // And a replace of it carries no tag 59, so the guess is never sent to
    // the gateway as an instruction — a fabricated DAY would expire an
    // order that is resting until cancelled.
    let listener = std::net::TcpListener::bind("127.0.1:0").unwrap();
    let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    let mut conn = Some(crate::protocol::connection::Connection::new_raw(stream).unwrap());
    let mut hb = crate::engine::hot_loop::HeartbeatState::new();
    let shared_arc = std::sync::Arc::new(SharedState::new());

    context.modify(78, 100 * PRICE_SCALE, 100, false);
    crate::engine::hot_loop::order_builder::drain_and_send_orders(
        &mut conn, &mut context, "DU1", &mut hb, false, &shared_arc, false, &None,
    );

    let mut buf = [0u8; 4096];
    let n = std::io::Read::read(&mut peer, &mut buf).unwrap();
    let msg = String::from_utf8_lossy(&buf[..n]);
    assert!(msg.contains("35=G"), "a replace was sent: {msg}");
    assert!(!msg.split('\u{1}').any(|f| f.starts_with("59=")),
        "a replace must not restate a time-in-force the order never had: {msg}");
}

/// The venue names at connect what it holds, not only what is working: the
/// shape for an order it holds is 150=0 with the status as it stands, 39=I
/// captured. A cancel-all iterates the engine's book, so a named order that
/// never reached the book is one the kill switch silently skips. It must
/// reach the book, and the cancel must cover it.
#[test]
fn a_naming_record_for_a_held_order_reaches_the_book_and_the_cancel() {
    let mut ccp = CcpState::new();
    let mut context = Context::new();
    let shared = SharedState::new();

    // A naming record in the venue's shape for a held order.
    let mut frame = std::collections::HashMap::new();
    for (tag, val) in [
        (11u32, "77"), (150u32, "0"), (39u32, "I"), (6008u32, "756733"),
        (38u32, "100"), (55u32, "SPY"), (54u32, "1"), (40u32, "2"),
        (44u32, "100.0"), (58u32, "Order held pending margin check"), (103u32, "0"),
    ] {
        frame.insert(tag, val.to_string());
    }
    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");

    let order = context.order(77).expect("the named order reaches the book");
    assert_eq!(
        order.status, crate::types::OrderStatus::Inactive,
        "and keeps the status the naming record stated",
    );

    // The set a cancel-all iterates, for the contract named on the record.
    let open: Vec<u64> = context.open_orders_for(order.instrument)
        .iter().map(|o| o.order_id).collect();
    assert_eq!(open, vec![77], "the cancel-all covers it");

    // The wire leg of the kill switch: the cancel names the order the venue
    // named, under the ClOrdID the naming record stated.
    context.cancel_all(order.instrument);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    peer.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
    let mut conn = Some(crate::protocol::connection::Connection::new_raw(stream).unwrap());
    let mut hb = crate::engine::hot_loop::HeartbeatState::new();
    let shared_arc = std::sync::Arc::new(SharedState::new());
    crate::engine::hot_loop::order_builder::drain_and_send_orders(
        &mut conn, &mut context, "DU1", &mut hb, false, &shared_arc, false, &None,
    );
    let mut buf = [0u8; 4096];
    let n = std::io::Read::read(&mut peer, &mut buf).unwrap();
    let msg = String::from_utf8_lossy(&buf[..n]).replace('\u{1}', "|");
    assert!(msg.contains("35=F"), "a cancel went out: {msg}");
    assert!(msg.contains("|41=77|"), "naming the order the venue named: {msg}");
}

/// A report the venue marks as restating history states a working status for
/// an order that finished — the naming at connect arrives unmarked, and the
/// marked reports are the past. One must not be recovered as working even
/// though its status is non-terminal.
#[test]
fn a_marked_restating_is_not_recovered_as_working() {
    let mut ccp = CcpState::new();
    let mut context = Context::new();
    let shared = SharedState::new();

    let mut frame = std::collections::HashMap::new();
    for (tag, val) in [
        (11u32, "77"), (150u32, "0"), (39u32, "0"), (97u32, "Y"), (6008u32, "756733"),
        (38u32, "100"), (55u32, "SPY"), (54u32, "1"), (40u32, "2"),
    ] {
        frame.insert(tag, val.to_string());
    }
    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");

    assert!(
        context.order(77).is_none(),
        "the past of a finished order is not brought back as working",
    );
}

fn cancel_reject_frame(reason_code: &str) -> std::collections::HashMap<u32, String> {
    let mut m = std::collections::HashMap::new();
    m.insert(41u32, "C42".to_string()); // OrigClOrdID
    m.insert(434u32, "1".to_string());
    m.insert(102u32, reason_code.to_string());
    m
}

fn tracked_for_cancel(context: &mut Context) {
    let instrument = context.register_instrument(756733);
    context.insert_order(crate::types::Order::new(
        42, instrument, Side::Buy, 100 * crate::types::QTY_SCALE, 100 * PRICE_SCALE, b'2', b'0', 0,
    ));
    context.update_order_status(42, crate::types::OrderStatus::PendingCancel, false);
}

/// A cancel answered with UnknownOrder says the order does not exist on
/// the venue's side. Forcing it back to working asserts the opposite of the
/// message being handled, and the engine's own view governs subsequent
/// cancels, modifies and reconnect bookkeeping, so a phantom order persists
/// there while the cache row that would surface it is removed.
#[test]
fn an_unknown_order_rejection_retires_the_order() {
    let mut ccp = CcpState::new();
    let mut context = Context::new();
    let shared = SharedState::new();
    tracked_for_cancel(&mut context);
    shared.orders.push_order_info(42, RichOrderInfo {
        contract: api::Contract::default(),
        order: api::Order::default(),
        order_state: api::OrderState::default(),
        last_exec: api::Execution::default(),
    });

    ccp.handle_cancel_reject(&cancel_reject_frame("1"), &mut context, &shared, &None);

    assert!(
        context.order(42).is_none(),
        "the engine must not keep asserting an order the gateway says is not there",
    );
    assert!(
        shared.orders.get_order_info(42).is_none(),
        "and the cache row goes with it",
    );
    // The rejection itself is the report. A synthetic status update queued
    // here would reach the caller behind a fill that raced it, because both
    // dispatchers drain fills ahead of order updates.
    assert!(shared.orders.drain_order_updates().is_empty());
    assert_eq!(shared.orders.drain_cancel_rejects().len(), 1);
}

/// A fill that raced the rejection is recoverable, on the terms the
/// untracked-fill path sets: the execution has to carry its
/// contract id, because nothing else says which instrument moved, and it
/// must not be resend-marked, because a replayed execution for an order
/// this session does not track is history rather than news. An execution
/// that carries neither is dropped — the same as it was before this change
/// for any order already removed from the book.
#[test]
fn an_execution_racing_an_unknown_order_rejection_still_books() {
    let mut ccp = CcpState::new();
    let mut context = Context::new();
    let shared = SharedState::new();
    tracked_for_cancel(&mut context);

    ccp.handle_cancel_reject(&cancel_reject_frame("1"), &mut context, &shared, &None);
    let frame = exec_report_frame(&[
        (39, "1"), (17, "e-1"), (150, "F"), (32, "40"), (31, "100.0"), (151, "60"),
        (6008, "756733"), (38, "100"), (54, "1"),
    ]);
    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");

    assert_eq!(shared.orders.drain_fills().len(), 1, "the fill books");
    assert_eq!(context.position(0), 40.0, "and the position moves");
}

/// Only a stated UnknownOrder retires the order. Every other stated reason
/// means it is still working and the cancel arrived at the wrong moment; an
/// absent or unparseable tag 102 states nothing at all and is synthesized
/// as -1, so it takes the same path rather than retiring on an absence.
#[test]
fn any_other_rejection_leaves_the_order_in_place() {
    for code in ["0", "2", "-1", ""] {
        let mut ccp = CcpState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        tracked_for_cancel(&mut context);

        ccp.handle_cancel_reject(&cancel_reject_frame(code), &mut context, &shared, &None);

        assert_eq!(
            context.order(42).expect("still tracked").status,
            crate::types::OrderStatus::Submitted,
            "reason {code:?} does not say the order is gone",
        );
    }
}

fn exec_report_frame(pairs: &[(u32, &str)]) -> std::collections::HashMap<u32, String> {
    let mut m = std::collections::HashMap::new();
    m.insert(11u32, "42".to_string()); // ClOrdID
    for (tag, val) in pairs {
        m.insert(*tag, val.to_string());
    }
    m
}

/// A report stating quantities at the end of the range is booked without the
/// sums that follow it running past their own.
///
/// The quantity a report states is scaled to hundred-millionths and held in
/// sixty-four bits. Taken as it stood, a magnitude past that saturated to the
/// largest quantity there is, and what remained on the order — the ordered
/// quantity less what has filled — was then a subtraction with no room left:
/// wrapped in a release build, and a stop in a checked one.
#[test]
fn quantities_at_the_end_of_the_range_do_not_run_the_order_arithmetic_past_it() {
    let s = |v: &str| v.to_string();
    assert_eq!(
        crate::engine::hot_loop::parse_qty_tag(Some(&s("1e30"))), None,
        "a magnitude the fixed-point form cannot hold is not a quantity",
    );

    let (mut ccp, mut context, shared) = ord_status_test_state();
    // Both figures inside what the form holds, and their difference outside
    // it. What is left on the order is worked out from the two.
    let frame = exec_report_frame(&[
        (39, "0"), (150, "0"), (100, "ARCA"), (198, "ARCA:1"),
        (38, "90000000000"), (14, "-90000000000"),
    ]);
    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");

    // And the sums the order itself keeps: a report booking a quantity at the
    // end of the range, then a second stating a print with no cumulative
    // figure beside it, which is added to what the order already holds.
    let booked = exec_report_frame(&[
        (39, "1"), (150, "F"), (17, "e1"), (31, "1.0"),
        (32, "90000000000"), (14, "90000000000"), (38, "90000000000"),
    ]);
    ccp.handle_exec_report(&booked, b"", &mut context, &shared, &None, "");
    let no_cum_qty = exec_report_frame(&[
        (39, "1"), (150, "F"), (17, "e2"), (31, "1.0"), (32, "90000000000"),
    ]);
    ccp.handle_exec_report(&no_cum_qty, b"", &mut context, &shared, &None, "");

    let told = shared.orders.drain_order_updates();
    assert!(
        told.iter().all(|u| u.remaining_qty >= 0.0 && u.filled_qty >= 0.0),
        "what is filled and what is left are quantities, never the wrap of one: {told:?}",
    );
}

/// An order id past what a caller's surface carries is not taken as one.
///
/// An id reaches a caller as a signed number, and the id to place under next
/// is one past the highest the venue has named. Taken as it stood, an id at
/// the top of an unsigned sixty-four bits named its order under `-1` and left
/// nothing to count past — the next id handed out was zero, which the venue
/// refuses as an id the account has already used.
#[test]
fn an_order_id_past_what_a_caller_can_hold_is_not_taken_as_one() {
    let (mut ccp, mut context, shared) = ord_status_test_state();
    let frame = exec_report_frame(&[
        (11, "18446744073709551615"), (39, "0"), (150, "0"), (100, "ARCA"), (198, "ARCA:1"),
    ]);
    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");

    assert_eq!(
        shared.orders.working_id_watermark(), 0,
        "an id nothing here can report is not the one every later id counts from",
    );
}

#[test]
fn ord_status_new_unrouted_is_presubmitted() {
    let (mut ccp, mut context, shared) = ord_status_test_state();
    // 39=0, no ExDestination, exec ref "NONE" — waiting, not yet routed.
    let frame = exec_report_frame(&[(39, "0"), (150, "0"), (198, "NONE")]);
    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");
    assert_eq!(context.order(42).unwrap().status,
        crate::types::OrderStatus::PreSubmitted);
}

#[test]
fn ord_status_new_routed_is_submitted() {
    let (mut ccp, mut context, shared) = ord_status_test_state();
    // 39=0 with the order routed to ARCA — working.
    let frame = exec_report_frame(&[(39, "0"), (150, "0"), (100, "ARCA"), (198, "ARCA:1")]);
    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");
    assert_eq!(context.order(42).unwrap().status,
        crate::types::OrderStatus::Submitted);
}

#[test]
fn ord_status_presubmitted_then_routed_advances_to_submitted() {
    let (mut ccp, mut context, shared) = ord_status_test_state();
    let waiting = exec_report_frame(&[(39, "0"), (150, "0"), (198, "NONE")]);
    ccp.handle_exec_report(&waiting, b"", &mut context, &shared, &None, "");
    assert_eq!(context.order(42).unwrap().status,
        crate::types::OrderStatus::PreSubmitted);
    let routed = exec_report_frame(&[(39, "0"), (150, "0"), (100, "ARCA"), (198, "ARCA:1")]);
    ccp.handle_exec_report(&routed, b"", &mut context, &shared, &None, "");
    assert_eq!(context.order(42).unwrap().status,
        crate::types::OrderStatus::Submitted);
}

// 39=I (Inactive) and 39=8 (Rejected) both stringify to
// "Inactive" downstream (types::order_status::order_status_str), but must not be
// treated the same here. A parked (39=I) order's reason is queued for
// delivery through Wrapper::error, and its completed_status stays empty
// (it is not completed and may reactivate). A rejected order's reason
// stays on the order snapshot, and nothing is queued for it — the
// engine still holds the order at this point, so context still knows it
// as Inactive/reactivatable while a Rejected order is retired below.
/// A schedule that arrives after the wait ran out is still filed.
///
/// Giving up says this session may ask again. It does not say the venue will
/// not answer, and the id this session asked under is the only thing that says
/// which contract an answer is about — forgotten with the entry, a schedule
/// that came a moment late was thrown away with nothing to attribute it by.
#[test]
fn a_schedule_that_comes_late_is_still_filed_against_its_contract() {
    let (mut ccp, _unused, shared) = ord_status_test_state();
    let mut hb = HeartbeatState::new();

    // Asked, and given up on before the answer came.
    let mut conn = None;
    ccp.give_up_on_a_dividend_query("div_1", 756_733);
    ccp.sweep_completed_orders_request(&mut conn, &mut hb, &shared);
    assert!(
        shared.reference.dividend_schedule(756_733).is_none(),
        "nothing has been filed yet",
    );

    // And then the venue answers, under the id it was asked with.
    let answer = crate::protocol::fix::fix_build(
        &[
            (35, "U"), (6040, "20"), (320, "div_1"),
            (6118, "<dividends><div><date>20260918</date><amt>1.8311</amt>\
                    <curr>USD</curr></div></dividends>"),
        ],
        1,
    );
    let mut context = Context::new();
    ccp.process_ccp_message(
        &answer, &mut None, &mut context, &shared, &None, &mut hb, "",
    );

    let filed = shared.reference.dividend_schedule(756_733).expect("filed against its contract");
    assert_eq!(filed.payments.len(), 1, "{:?}", filed.payments);
}

/// A question about what the venue has finished waits for the session's own
/// replay to be over.
///
/// Both answers are a run of ordinary reports ending in the same sentinel, and
/// nothing on the wire says which question a sentinel answers. Asked across
/// the replay, the replay's own ending shut the window: the caller was told
/// the answer was complete before it had begun, and the history that followed
/// took the live path, where a report that states a fill is a fill.
#[test]
fn the_question_waits_for_the_session_s_own_replay() {
    let (mut ccp, _context, shared) = ord_status_test_state();
    let mut hb = HeartbeatState::new();
    let mut conn = None;

    ccp.send_completed_orders_request(1, &mut conn, &mut hb, &shared);
    assert!(!ccp.completed_orders_open, "nothing was asked yet");
    assert!(
        shared.orders.completed_orders_ended() == 0,
        "and the caller was not told the answer is complete",
    );

    // Nor on a later pass, while the replay is still running.
    ccp.sweep_completed_orders_request(&mut conn, &mut hb, &shared);
    assert!(shared.orders.completed_orders_ended() == 0, "still waiting");

    // Nor while an earlier question is still being answered: two of them share
    // one window and one sentinel, so the first sentinel would shut the window
    // on both and only one caller would ever be released.
    shared.orders.set_replay_done();
    ccp.completed_orders_open = true;
    ccp.sweep_completed_orders_request(&mut conn, &mut hb, &shared);
    assert!(shared.orders.completed_orders_ended() == 0, "one at a time");
    ccp.completed_orders_open = false;

    // Once the way is clear the question goes out. There is no connection here
    // to carry it, so what the caller is told is that it cannot be answered —
    // which is the path a held question joins, not a path of its own.
    ccp.sweep_completed_orders_request(&mut conn, &mut hb, &shared);
    assert!(
        shared.orders.completed_orders_ended() > 0,
        "the held question was asked once the replay was over",
    );
}

/// An account with nothing working still gets an answer.
///
/// The opening replay ends by naming no order at all on such an account, and
/// naming one is what says the replay has begun — so a question held for the
/// replay to finish waited on something that was never going to happen, on
/// exactly the accounts where the question is most likely to be asked.
#[test]
fn a_question_held_for_a_replay_that_names_nothing_is_asked_anyway() {
    let (mut ccp, _context, shared) = ord_status_test_state();
    let mut hb = HeartbeatState::new();
    let mut conn = None;

    ccp.send_completed_orders_request(1, &mut conn, &mut hb, &shared);
    assert!(shared.orders.completed_orders_ended() == 0, "held, and the replay has not ended");

    // Held for as long as the replay could take, and no longer. There is no
    // connection here to carry it, so what the caller is told is that it
    // cannot be answered — which is an answer, and is what was missing.
    ccp.give_up_waiting_for_the_replay();
    ccp.sweep_completed_orders_request(&mut conn, &mut hb, &shared);
    assert!(
        shared.orders.completed_orders_ended() > 0,
        "the question went out rather than waiting on a replay that names nothing",
    );
}

/// The hold and the window fit inside one caller's wait, and the hold is taken
/// once per connection rather than once per question.
///
/// Both were built from the same constant and ran one behind the other: a
/// question waited twelve seconds for a replay that never names an order, the
/// window behind it another twelve, and the caller waits fifteen. So the
/// caller was handed an empty answer the venue never gave — and paid the hold
/// again on every call, on exactly the accounts that have nothing working and
/// are most likely to ask.
#[test]
fn the_hold_and_the_window_fit_inside_the_wait_the_caller_keeps() {
    let (mut ccp, _context, shared) = ord_status_test_state();
    let mut hb = HeartbeatState::new();
    let mut conn = None;

    // Held once, for the replay.
    ccp.send_completed_orders_request(1, &mut conn, &mut hb, &shared);
    assert_eq!(shared.orders.completed_orders_ended(), 0, "held while the replay could run");

    // The hold runs out and the question goes out. There is no connection, so
    // the answer is that it cannot be answered.
    ccp.give_up_waiting_for_the_replay();
    ccp.sweep_completed_orders_request(&mut conn, &mut hb, &shared);
    let after_the_first = shared.orders.completed_orders_ended();
    assert!(after_the_first > 0, "the question was asked");

    // A second question on the same connection is not held again: the replay
    // happens once, and it has already had its time.
    ccp.send_completed_orders_request(1, &mut conn, &mut hb, &shared);
    assert!(
        shared.orders.completed_orders_ended() > after_the_first,
        "the question was held for a replay that had already had its time",
    );
}

/// A caller who has waited long enough is told so, and the window is not shut
/// with them.
///
/// A clock here says when a caller has waited long enough. It says nothing
/// about where the venue's answer ends, and that answer is a run of ordinary
/// reports carrying no mark of which question they answer. Shut on the clock,
/// the rest of the answer goes to the live path, where a report that states a
/// fill is a fill and moves a position.
#[test]
fn a_caller_who_waited_long_enough_is_answered_without_shutting_the_window() {
    let (mut ccp, _context, shared) = ord_status_test_state();
    let mut hb = HeartbeatState::new();
    let mut conn = None;
    ccp.completed_orders_open = true;
    ccp.give_up_waiting_for_the_sentinel();

    ccp.sweep_completed_orders_request(&mut conn, &mut hb, &shared);

    assert!(
        shared.orders.completed_orders_ended() > 0,
        "the caller is answered with what arrived",
    );
    assert!(
        ccp.completed_orders_open,
        "and what is still coming is still read as history, because the venue has not \
         said it has finished",
    );
}

/// The question dies with the connection that carried it.
///
/// Its answer ends with a sentinel, so a drop mid-answer left the caller
/// waiting out its whole deadline for a sentinel that was never coming — and
/// left the window open across the reconnect, where the replay that follows is
/// filed as history instead of recovered into the book a withdrawal walks.
#[test]
fn a_drop_mid_answer_releases_the_caller_and_shuts_the_window() {
    let (mut ccp, mut context, shared) = ord_status_test_state();
    ccp.completed_orders_open = true;
    let mut conn = None;

    ccp.handle_disconnect(&mut conn, &mut context, &shared, &None);

    assert!(!ccp.completed_orders_open, "the window is shut");
    assert!(
        shared.orders.completed_orders_ended() > 0,
        "and the caller is released rather than left on a sentinel nobody will send",
    );
}

/// The wait for the replay is taken again on the connection that replays.
///
/// The wait belongs to the connection, and a drop with no question outstanding
/// walked past it: the next connection inherited a wait that had already run
/// out, and asked what the venue has finished while its own replay was still
/// arriving — where that replay is read as history and every order working in
/// it is filed as finished.
#[test]
fn the_wait_for_the_replay_is_taken_again_on_the_next_connection() {
    let (mut ccp, mut context, shared) = ord_status_test_state();
    let mut hb = HeartbeatState::new();
    let mut conn = None;

    // A question on this connection: held, then asked once the wait ran out.
    ccp.send_completed_orders_request(1, &mut conn, &mut hb, &shared);
    ccp.give_up_waiting_for_the_replay();
    ccp.sweep_completed_orders_request(&mut conn, &mut hb, &shared);
    let asked = shared.orders.completed_orders_ended();
    assert!(asked > 0, "the first question was asked");
    assert!(!ccp.completed_orders_open, "and nothing is outstanding when the connection goes");

    ccp.handle_disconnect(&mut conn, &mut context, &shared, &None);
    shared.orders.replay_is_pending();

    // The connection that replaces it replays again, so the question waits.
    ccp.send_completed_orders_request(2, &mut conn, &mut hb, &shared);
    assert_eq!(
        shared.orders.completed_orders_ended(),
        asked,
        "the question went out over the replay of the connection that had just come up",
    );
}

/// A correction for an order this session placed is never filed as history.
///
/// A fill retires an order from the book, so afterwards the book alone cannot
/// tell this session's order from a stranger's. Read that way, a bust or a
/// correction arriving while the window was open was filed as something that
/// happened days ago: it took back nothing, and the position it was undoing
/// stayed where it was.
#[test]
fn a_correction_for_this_session_s_own_order_is_not_history() {
    let (mut ccp, mut context, shared) = ord_status_test_state();
    ccp.completed_orders_open = true;
    // Placed here, and already gone from the book the way a filled order is.
    shared.orders.note_the_order_went_out(987_654_321);

    let mut frame = exec_report_frame(&[
        (39, "2"), (150, "F"), (32, "100"), (31, "150.00"), (14, "100"), (151, "0"),
        (54, "1"), (38, "100"), (55, "IBM"), (167, "CS"), (15, "USD"), (6008, "8314"),
        (40, "2"), (44, "150.00"), (1, "DU111111"),
    ]);
    frame.insert(11, "987654321".to_string());
    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");

    // The live path registers the contract the report names. Filing as history
    // registers nothing, which is how the two are told apart from outside.
    assert!(
        context.market.instrument_by_con_id(8314).is_some(),
        "the report took the live path, where what it says still counts",
    );
}

/// The bound on what an answer holds is a bound on orders that are still being
/// stated too.
///
/// The bound hands over what the venue has finished and lets go of it, and an
/// order still being stated is not part of that — it is kept, because every
/// later report about it is read against what is held. So an answer whose
/// records are all still being stated freed nothing at the bound: the next
/// order was held beside them, the one after that too, and a window the venue
/// never ends grew for the life of a connection that never drops, reading each
/// report against the whole of it.
#[test]
fn an_answer_of_orders_still_being_stated_is_bounded_too() {
    let (mut ccp, mut context, shared) = ord_status_test_state();
    ccp.completed_orders_open = true;
    for order_id in 0..super::FINISHED_ORDERS_HELD as u64 {
        ccp.hold_a_finished_order_for_test(order_id, crate::types::OrderStatus::Submitted);
    }

    // One more order the venue states, which nothing here holds a record of.
    let mut frame = exec_report_frame(&[
        (39, "0"), (150, "0"), (32, "0"), (14, "0"), (151, "100"),
        (54, "1"), (38, "100"), (55, "IBM"), (167, "CS"), (15, "USD"), (6008, "8314"),
        (40, "2"), (44, "150.00"), (1, "DU111111"),
    ]);
    frame.insert(11, "424242".to_string());
    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");

    assert_eq!(
        ccp.finished_orders.len(), super::FINISHED_ORDERS_HELD,
        "the answer holds what it holds and no more",
    );
    assert!(
        !ccp.finished_orders.iter().any(|held| held.order_id == 424_242),
        "and the order the venue stated past the bound is not in it",
    );
}

/// The bound is on the answer, not on what is waiting to be handed over.
///
/// A handover empties the records and lets go of them, so an answer measured
/// by what is waiting was never bounded at all: every batch handed over made
/// room for the next, and a window the venue never ends grew for the life of a
/// connection that never drops — the answer the caller is finally handed as
/// large as the venue cared to make it.
#[test]
fn the_bound_is_on_the_answer_not_on_what_waits_to_be_handed_over() {
    let (mut ccp, mut context, shared) = ord_status_test_state();
    ccp.completed_orders_open = true;
    // A full answer's worth, every one of them finished — so a handover takes
    // them all and leaves nothing waiting.
    for order_id in 0..super::FINISHED_ORDERS_HELD as u64 {
        ccp.hold_a_finished_order_for_test(order_id, crate::types::OrderStatus::Filled);
    }

    let mut frame = exec_report_frame(&[
        (39, "2"), (150, "F"), (32, "100"), (31, "150.00"), (14, "100"), (151, "0"),
        (54, "1"), (38, "100"), (55, "IBM"), (167, "CS"), (15, "USD"), (6008, "8314"),
        (40, "2"), (44, "150.00"), (1, "DU111111"),
    ]);
    frame.insert(11, "424242".to_string());
    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");

    assert!(
        !ccp.finished_orders.iter().any(|held| held.order_id == 424_242),
        "the answer has taken every order it can, so this one is not part of it",
    );
    let handed_over = shared.orders.drain_completed_orders();
    assert!(
        !handed_over.iter().any(|order| order.order_id == 424_242),
        "and it is not handed to the caller either",
    );
}

/// A new answer takes its own orders up to its own bound.
///
/// An answer keeps the records the venue never finished stating, because a
/// report states what changed and leaves the rest out — so those records are
/// what a later report about the same order is read against. They are not this
/// answer's orders though: counted against its bound, an account with enough of
/// them could not take a single order the venue stated.
#[test]
fn a_new_answer_takes_its_own_orders_up_to_its_own_bound() {
    let (mut ccp, _context, shared) = ord_status_test_state();
    let mut hb = HeartbeatState::new();
    let (conn, _peer) = Connection::for_test();
    let mut conn = Some(conn);
    // The replay is over, so the question is not held.
    shared.orders.set_replay_done();
    for order_id in 0..super::FINISHED_ORDERS_HELD as u64 {
        ccp.hold_a_finished_order_for_test(order_id, crate::types::OrderStatus::Submitted);
    }

    ccp.send_completed_orders_request(1, &mut conn, &mut hb, &shared);

    assert!(
        ccp.orders_in_this_answer.is_empty(),
        "this answer can take every order the venue states, up to its own bound",
    );
    assert_eq!(
        ccp.finished_orders.len(), super::FINISHED_ORDERS_HELD,
        "and the half-built records stay, because a report states only what changed",
    );
}

/// And the records an earlier answer left behind make room for it.
///
/// They belong to no answer now: this one has not taken those orders, and
/// nothing is finishing them. Kept all the same, they hold the room this answer
/// needs and it cannot take an order the venue states at all.
#[test]
fn the_records_an_earlier_answer_left_make_room_for_this_one() {
    let (mut ccp, mut context, shared) = ord_status_test_state();
    let mut hb = HeartbeatState::new();
    let (conn, _peer) = Connection::for_test();
    let mut conn = Some(conn);
    shared.orders.set_replay_done();
    // An answer's worth of orders the venue never finished stating.
    for order_id in 0..super::FINISHED_ORDERS_HELD as u64 {
        ccp.hold_a_finished_order_for_test(order_id, crate::types::OrderStatus::Submitted);
    }
    ccp.send_completed_orders_request(1, &mut conn, &mut hb, &shared);

    // And an order the venue states in this answer.
    let mut frame = exec_report_frame(&[
        (39, "2"), (150, "F"), (32, "100"), (31, "150.00"), (14, "100"), (151, "0"),
        (54, "1"), (38, "100"), (55, "IBM"), (167, "CS"), (15, "USD"), (6008, "8314"),
        (40, "2"), (44, "150.00"), (1, "DU111111"),
    ]);
    frame.insert(11, "424242".to_string());
    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");

    assert!(
        ccp.finished_orders.iter().any(|held| held.order_id == 424_242),
        "this answer takes the order the venue stated",
    );
    assert_eq!(
        ccp.finished_orders.len(), super::FINISHED_ORDERS_HELD,
        "and holds no more records than it can",
    );
}

/// An order the venue numbered is an API order whichever path its report took.
///
/// The number is what tells an order placed through an API from one typed in
/// by hand, and it is on every report the venue sends about the order — not
/// only the ones that arrive in answer to a question about finished orders.
/// Read on that path alone, an order another API placed and finished while
/// this session was watching was left out of the answer to a caller asking for
/// the API orders.
#[test]
fn an_order_the_venue_numbered_is_known_as_an_api_order_on_any_path() {
    let (mut ccp, mut context, shared) = ord_status_test_state();

    // A live report for an order this session did not place, with no window
    // open — so it takes the ordinary path — and the venue's own number on it.
    let mut numbered = exec_report_frame(&[
        (39, "2"), (150, "F"), (32, "100"), (31, "150.00"), (14, "100"), (151, "0"),
        (54, "1"), (38, "100"), (55, "IBM"), (167, "CS"), (15, "USD"), (6008, "8314"),
        (40, "2"), (44, "150.00"), (1, "DU111111"), (6121, "4471"),
    ]);
    numbered.insert(11, "987654321".to_string());
    ccp.handle_exec_report(&numbered, b"", &mut context, &shared, &None, "");
    assert!(
        shared.orders.was_entered_through_an_api(0, 987_654_321),
        "the venue numbered it, so it went through an API",
    );

    // And one it did not number, which is what a manual entry looks like.
    let mut typed_in = exec_report_frame(&[
        (39, "2"), (150, "F"), (32, "50"), (31, "150.00"), (14, "50"), (151, "0"),
        (54, "1"), (38, "50"), (55, "IBM"), (167, "CS"), (15, "USD"), (6008, "8314"),
        (40, "2"), (44, "150.00"), (1, "DU111111"),
    ]);
    typed_in.insert(11, "987654322".to_string());
    ccp.handle_exec_report(&typed_in, b"", &mut context, &shared, &None, "");
    assert!(
        !shared.orders.was_entered_through_an_api(0, 987_654_322),
        "nothing numbered it, so nothing says an API placed it",
    );
}

/// A later report about a recovered order reaches the order it is about.
///
/// The venue names a recovered order two ways: its recovery report states the
/// permanent name and the number an API gave it, and this session takes the
/// second, because that is the number a caller withdraws it by. Every later
/// report states only the first. Looked up under that, the order this session
/// is tracking was not found, so a fill on it was booked against nothing — and
/// while a finished-orders window was open it was filed as history instead.
#[test]
fn a_later_report_naming_a_recovered_order_the_venues_way_finds_it() {
    let mut ccp = CcpState::new();
    let mut context = Context::new();
    let shared = SharedState::new();

    // Recovered: the venue's permanent name on tag 11, the API's number beside
    // it, and this session tracks it under the second.
    let recovery = exec_report_frame(&[
        (11, "9000.0"), (6121, "42"), (150, "0"), (39, "0"),
        (6008, "756733"), (55, "SPY"), (54, "1"), (38, "100"),
        (40, "2"), (44, "100"), (59, "0"), (100, "ARCA"), (198, "ARCA:1"),
    ]);
    ccp.handle_exec_report(&recovery, b"", &mut context, &shared, &None, "DU1");
    assert!(context.order(42).is_some(), "tracked under the number an API gave it");

    // And a later report names it the venue's way only.
    let mut later = exec_report_frame(&[
        (39, "1"), (150, "F"), (32, "50"), (31, "100.00"), (14, "50"), (151, "50"),
        (54, "1"), (38, "100"), (55, "SPY"), (6008, "756733"), (198, "ARCA:2"),
    ]);
    later.insert(11, "9000".to_string());
    ccp.handle_exec_report(&later, b"", &mut context, &shared, &None, "DU1");

    assert_eq!(
        context.order(42).expect("still tracked").filled,
        50 * crate::types::QTY_SCALE,
        "the fill reached the order it was for",
    );
}

/// A caller released early is handed the orders the venue has finished, and
/// not the ones it is still describing.
///
/// A record still being built says the order is working, and a record saying
/// that is one this client reads as an order the venue is holding. Handed over
/// anyway, a question about finished orders answered with live ones, and a
/// withdrawal could be aimed at one of them.
#[test]
fn a_caller_released_early_is_not_handed_a_half_described_order() {
    let (mut ccp, mut context, shared) = ord_status_test_state();
    let mut hb = HeartbeatState::new();
    let mut conn = None;
    ccp.completed_orders_open = true;

    // One the venue has finished describing, and one it has not.
    for (id, status) in [(11_001u64, "2"), (11_002, "0")] {
        let mut report = exec_report_frame(&[
            (39, status), (150, "0"), (14, "0"), (151, "0"),
            (54, "1"), (38, "100"), (55, "IBM"), (167, "CS"), (15, "USD"), (6008, "8314"),
            (40, "2"), (44, "150.00"), (1, "DU111111"),
        ]);
        report.insert(11, id.to_string());
        ccp.handle_exec_report(&report, b"", &mut context, &shared, &None, "");
    }

    ccp.give_up_waiting_for_the_sentinel();
    ccp.sweep_completed_orders_request(&mut conn, &mut hb, &shared);

    let finished: Vec<u64> =
        shared.orders.drain_completed_orders().into_iter().map(|o| o.order_id).collect();
    assert_eq!(finished, [11_001], "only the one the venue has finished: {finished:?}");
    let open = shared.orders.drain_open_orders();
    assert!(
        !open.iter().any(|(id, _)| *id == 11_002),
        "the half-described one was answered as an order the venue is working: {open:?}",
    );
}

/// Two callers in turn are each answered for their own question.
///
/// The end of an answer was a flag: set by one answer and taken by whoever
/// polled next. A caller that gave up a moment before it was set left it
/// standing, and the next caller read it as the answer to a question it had
/// not yet asked — returning before its own request reached the venue.
#[test]
fn the_end_of_one_answer_is_not_the_end_of_the_next_question() {
    let (_ccp, _context, shared) = ord_status_test_state();

    // What a caller reads before it asks.
    let before = shared.orders.completed_orders_ended();

    // An answer completes while nobody is waiting.
    shared.orders.note_completed_orders_end();
    assert!(
        shared.orders.completed_orders_ended() > before,
        "the answer that completed moved the count",
    );

    // The next caller reads the count as it now stands, and is not satisfied
    // by the answer that completed before it asked.
    let before = shared.orders.completed_orders_ended();
    assert_eq!(
        shared.orders.completed_orders_ended(),
        before,
        "nothing has answered this caller yet",
    );
    shared.orders.note_completed_orders_end();
    assert!(shared.orders.completed_orders_ended() > before, "and then its own answer does");
}

/// A finished order the venue rejected is not one it is still holding.
///
/// A terminal order says so twice: as the status it is in, and as what became
/// of it. Left empty, a rejected order reads as one merely inactive, which
/// this client takes for an order the venue is parking and may bring back — so
/// a finished order was answered as open, and a withdrawal could be aimed at
/// it.
#[test]
fn a_rejected_order_the_venue_has_finished_is_not_read_as_still_open() {
    let (mut ccp, mut context, shared) = ord_status_test_state();
    ccp.completed_orders_open = true;

    let mut rejected = exec_report_frame(&[
        (39, "8"), (150, "8"), (14, "0"), (151, "0"),
        (54, "1"), (38, "100"), (55, "IBM"), (167, "CS"), (15, "USD"), (6008, "8314"),
        (40, "2"), (44, "150.00"), (1, "DU111111"),
    ]);
    rejected.insert(11, "987654321".to_string());
    ccp.handle_exec_report(&rejected, b"", &mut context, &shared, &None, "");

    let mut end = exec_report_frame(&[(39, "2"), (55, "*")]);
    end.insert(11, "0".to_string());
    ccp.handle_exec_report(&end, b"", &mut context, &shared, &None, "");

    let open = shared.orders.drain_open_orders();
    assert!(
        !open.iter().any(|(id, _)| *id == 987_654_321),
        "a finished order was answered as one the venue is still holding: {open:?}",
    );
}

/// One order's reports are one order, whichever number each of them carries.
///
/// The venue states an order under the number an API gave it on some reports
/// and under its own on others. Keyed by whichever the report carried, an
/// order's first report and its last were filed as two different orders: the
/// caller was told about it twice, once with the fields and no outcome and
/// once with the outcome and no fields.
#[test]
fn reports_naming_one_order_two_ways_are_still_one_order() {
    let (mut ccp, mut context, shared) = ord_status_test_state();
    ccp.completed_orders_open = true;

    // The first report states the order and carries the number an API gave it.
    let mut first = exec_report_frame(&[
        (39, "0"), (150, "0"), (14, "0"), (151, "0"),
        (54, "2"), (38, "100"), (55, "IBM"), (167, "CS"), (15, "USD"), (6008, "8314"),
        (40, "2"), (44, "150.00"), (1, "DU111111"), (6121, "4471"),
    ]);
    first.insert(11, "9000.0".to_string());
    ccp.handle_exec_report(&first, b"", &mut context, &shared, &None, "");

    // The last states the outcome and carries only the venue's own number.
    let mut last = exec_report_frame(&[
        (39, "2"), (150, "0"), (14, "100"), (151, "0"), (1, "DU111111"),
    ]);
    last.insert(11, "9000".to_string());
    ccp.handle_exec_report(&last, b"", &mut context, &shared, &None, "");

    let mut end = exec_report_frame(&[(39, "2"), (55, "*")]);
    end.insert(11, "0".to_string());
    ccp.handle_exec_report(&end, b"", &mut context, &shared, &None, "");

    let finished = shared.orders.drain_completed_orders();
    assert_eq!(finished.len(), 1, "one order, not one per number: {finished:?}");
    assert_eq!(finished[0].status, crate::types::OrderStatus::Filled, "with its outcome");
    // Filed under the number a caller addresses it by, not the permanent name
    // the venue files it under: that name belongs to no client, and published
    // as an order id it raises the mark this session issues its own above.
    assert_eq!(finished[0].order_id, 4471, "the number an API gave it");
    assert!(
        shared.orders.get_order_info(9000).is_none(),
        "and not under the venue's own permanent name",
    );
    let info = shared.orders.get_order_info(4471).expect("its fields, under that number");
    assert_eq!(info.order.action, "SELL");
    assert_eq!(info.contract.symbol, "IBM");
    assert_eq!(info.order.order_id, 4471);
}

/// The last event of a finished order's life is the one the caller is handed.
///
/// The answer states each order's whole life, one report per event, in the
/// order they happened. Filed as a first sighting each time, the second event
/// onwards was refused as a repeat of the first — so a filled order was
/// reported as submitted, short every fill that followed it.
#[test]
fn the_last_event_of_a_finished_order_is_the_one_that_stands() {
    let (mut ccp, mut context, shared) = ord_status_test_state();
    ccp.completed_orders_open = true;

    // Submitted, then filled — the two events of one order, as they happened.
    // The first states the whole order; the second states what changed, which
    // is what the venue does.
    let mut first = exec_report_frame(&[
        (39, "0"), (150, "0"), (14, "0"), (151, "0"),
        (54, "2"), (38, "100"), (55, "IBM"), (167, "CS"), (15, "USD"), (6008, "8314"),
        (100, "NYSE"), (40, "2"), (44, "150.00"), (1, "DU111111"),
        (59, "1"), (6433, "1"), (583, "grp-7"), (6010, "mine"),
        (168, "20260901-09:30:00"),
    ]);
    first.insert(11, "987654321".to_string());
    ccp.handle_exec_report(&first, b"", &mut context, &shared, &None, "");

    let mut last = exec_report_frame(&[
        (39, "2"), (150, "0"), (14, "100"), (151, "0"), (1, "DU111111"),
    ]);
    last.insert(11, "987654321".to_string());
    ccp.handle_exec_report(&last, b"", &mut context, &shared, &None, "");

    // The venue says it has finished, and the answer is handed over whole.
    let mut end = exec_report_frame(&[(39, "2"), (55, "*")]);
    end.insert(11, "0".to_string());
    ccp.handle_exec_report(&end, b"", &mut context, &shared, &None, "");

    // The later event said nothing about the side, the symbol, the venue, the
    // quantity, the price or any term the first event stated once. Rebuilt
    // from nothing, the record kept only what that event repeated — and a
    // side nobody stated is not a buy.
    let info = shared.orders.get_order_info(987_654_321).expect("the order is recorded");
    assert_eq!(info.order.total_quantity, 100.0, "the quantity the first event stated");
    assert_eq!(info.order.order_type, "LMT", "and its type, spelled the way a caller reads it");
    assert_eq!(info.order.lmt_price, 150.0, "and its price");
    assert_eq!(info.order.tif, "GTC", "and how long it stood, also spelled that way");
    assert!(info.order.outside_rth, "and that it ran outside regular hours");
    assert_eq!(info.order.oca_group, "grp-7", "and the group it cancelled with");
    assert_eq!(info.order.order_ref, "mine", "and the reference its caller gave it");
    assert_eq!(info.order.good_after_time, "20260901-09:30:00", "and when it became live");
    assert_eq!(info.order.action, "SELL", "the side the first event stated");
    assert_eq!(info.contract.symbol, "IBM", "and the symbol");
    assert_eq!(info.contract.exchange, "NYSE", "and where it traded");

    let finished = shared.orders.drain_completed_orders();
    assert_eq!(finished.len(), 1, "one order, not one per event: {finished:?}");
    assert_eq!(
        finished[0].status,
        crate::types::OrderStatus::Filled,
        "what became of it, not what it was doing first",
    );
    assert_eq!(
        finished[0].filled_qty,
        100 * crate::types::QTY_SCALE,
        "and everything it filled",
    );
}

/// What the venue has finished is filed as history, and moves nothing.
///
/// The answer to that question is a run of ordinary execution reports for
/// orders this session never placed. Through the path a live report takes,
/// each one is a fill, and a fill registers a contract, opens an order in the
/// engine's book and moves a position. None of that may happen for an order
/// that finished days ago.
///
/// The window is narrowed to what that path would otherwise have recovered,
/// so a report for an order this session is working is never diverted by it.
#[test]
fn what_the_venue_has_finished_is_filed_rather_than_worked() {
    let (mut ccp, mut context, shared) = ord_status_test_state();
    ccp.completed_orders_open = true;

    // An order this session never placed, finished, on a contract it has
    // never seen.
    let mut frame = exec_report_frame(&[
        (39, "2"), (150, "F"), (32, "100"), (31, "150.00"), (14, "100"), (151, "0"),
        (54, "1"), (38, "100"), (55, "IBM"), (167, "CS"), (15, "USD"), (6008, "8314"),
        (40, "2"), (44, "150.00"), (1, "DU111111"),
    ]);
    frame.insert(11, "987654321".to_string());
    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");

    assert!(
        context.market.instrument_by_con_id(8314).is_none(),
        "a finished order registers no contract",
    );
    assert!(
        context.order(987_654_321).is_none(),
        "and opens no order in the book a withdrawal walks",
    );
    // Nothing is handed over yet: an order's reports are not always adjacent,
    // so the answer is assembled and given whole when the venue says it has
    // finished.
    assert!(shared.orders.drain_completed_orders().is_empty(), "not until the venue is done");
    assert!(shared.orders.completed_orders_ended() == 0, "not yet");

    // And the sentinel says the venue has said everything.
    let mut end = exec_report_frame(&[(39, "2"), (55, "*")]);
    end.insert(11, "0".to_string());
    ccp.handle_exec_report(&end, b"", &mut context, &shared, &None, "");
    assert!(!ccp.completed_orders_open, "the window is shut");
    assert!(shared.orders.completed_orders_ended() > 0, "and the caller is released");

    let finished = shared.orders.drain_completed_orders();
    assert_eq!(finished.len(), 1, "it is filed as finished: {finished:?}");
    assert_eq!(finished[0].order_id, 987_654_321);
    let info = shared.orders.get_order_info(987_654_321).expect("with what it was");
    assert_eq!(info.contract.symbol, "IBM");
    assert_eq!(info.order.action, "BUY");
    assert_eq!(info.order.total_quantity, 100.0);
}

/// The report that fills an order states its new status on the same
/// report. Announcing the execution and withholding the status left a
/// caller watching order status believing the order was still working,
/// which is the one thing it most needed not to believe.
#[test]
fn a_report_that_fills_an_order_also_says_the_order_is_filled() {
    let (mut ccp, mut context, shared) = ord_status_test_state();
    let (tx, rx) = std::sync::mpsc::sync_channel(4096);
    // 39=2 filled, 150=F the execution, with a quantity and a price on it.
    let frame = exec_report_frame(&[
        (39, "2"), (150, "F"), (32, "100"), (31, "150.00"), (14, "100"), (151, "0"),
    ]);
    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &Some(crate::engine::hot_loop::EventSink::new(tx, Default::default())), "");

    let events: Vec<_> = rx.try_iter().collect();
    assert!(
        events.iter().any(|e| matches!(e, Event::Fill(_))),
        "the execution is reported: {events:?}",
    );
    assert!(
        events.iter().any(|e| matches!(
            e, Event::OrderUpdate(u) if u.status == crate::types::OrderStatus::Filled
        )),
        "and so is the status it left the order in: {events:?}",
    );
}

#[test]
fn ord_status_inactive_reason_reaches_inactive_queue() {
    let (mut ccp, mut context, shared) = ord_status_test_state();
    let frame = exec_report_frame(&[
        (39, "I"), (150, "0"),
        (58, "Order held pending margin check"), (103, "0"),
    ]);
    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");

    assert_eq!(context.order(42).unwrap().status, crate::types::OrderStatus::Inactive);

    let inactive = shared.orders.drain_order_inactive();
    assert_eq!(inactive.len(), 1);
    assert_eq!(inactive[0].0, 42);
    assert_eq!(inactive[0].2, "Order held pending margin check (reason code 0)");

    let info = shared.orders.get_order_info(42).unwrap();
    assert!(info.order_state.completed_status.is_empty());
}

#[test]
fn a_refused_order_tells_the_caller_why() {
    let (mut ccp, mut context, shared) = ord_status_test_state();
    let frame = exec_report_frame(&[
        (39, "8"), (150, "0"),
        (58, "No valid bid/ask"), (103, "1"),
    ]);
    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");

    // Rejected is terminal — the engine retires the order.
    assert!(context.order(42).is_none());

    // The venue said why. A caller that has to read a log to find out is a
    // caller that cannot act on it, so the reason goes out on the channel a
    // refusal is reported on.
    let reported = shared.orders.drain_order_inactive();
    assert_eq!(reported.len(), 1, "the refusal reaches the caller: {reported:?}");
    assert_eq!(reported[0].0, 42);
    assert!(reported[0].2.contains("No valid bid/ask"), "and says why: {reported:?}");

    // It stays on the order's own record too, which is where a caller that
    // asks after the fact looks.
    let info = shared.orders.get_order_info(42).unwrap();
    assert_eq!(info.order_state.completed_status, "No valid bid/ask");
}

/// Empty reason text does not make a refused order one the venue can resume.
#[test]
fn an_execution_rejection_with_empty_text_stays_out_of_open_orders() {
    let (mut ccp, mut context, shared) = ord_status_test_state();
    let core = crate::client_core::ClientCore::new();
    let refusal = exec_report_frame(&[
        (35, "8"), (11, "42.0"), (39, "8"), (150, "8"), (58, ""),
    ]);
    ccp.handle_exec_report(&refusal, b"", &mut context, &shared, &None, "");

    assert!(context.order(42).is_none(), "the engine retires the refused order");
    assert!(core.collect_open_orders(&shared).is_empty(), "the cache must not import it again");
    let info = shared.orders.get_order_info(42).expect("the refusal is kept for completed orders");
    assert_eq!(info.order_state.status, "Inactive");
    assert_eq!(info.order_state.completed_status, "Rejected");
    let completed = shared.orders.drain_completed_orders();
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].order_id, 42);
    assert_eq!(completed[0].status, crate::types::OrderStatus::Rejected);
}

/// `completed_status` carries the reject text alone, so a caller reading it
/// cannot tell a venue refusing an order type from a malformed request when
/// the text is generic. The reason code (tag 103) is what separates them.
#[test]
fn ord_status_rejected_records_the_reason_with_its_code() {
    let (mut ccp, mut context, shared) = ord_status_test_state();
    let frame = exec_report_frame(&[
        (39, "8"), (150, "0"),
        (58, "No valid bid/ask"), (103, "1"),
    ]);
    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");

    let info = shared.orders.get_order_info(42).unwrap();
    assert_eq!(info.order_state.reject_reason, "No valid bid/ask (reason code 1)");
}

/// A replace followed at once by a cancel races at the venue, and the venue
/// resolves the race in the cancel's favour with the rejections first and the
/// cancel's own acknowledgement last. The rejections answer the replace, and
/// the first of them used to retire the order, so the acknowledgement arrived
/// with nothing tracked to announce it against, and a caller that had
/// cancelled was told the order was rejected and never that it was cancelled.
#[test]
fn a_cancel_is_still_answered_when_rejections_cross_it() {
    let mut ccp = CcpState::new();
    let mut context = Context::new();
    let shared = SharedState::new();
    let instrument = context.register_instrument(756733);
    context.insert_order(crate::types::Order::new(
        42, instrument, Side::Buy, 100 * crate::types::QTY_SCALE, 100 * PRICE_SCALE, b'2', b'0', 0,
    ));
    // Working, then a cancel on the wire: that cancel is what the venue owes
    // an answer to.
    assert!(context.update_order_status(42, crate::types::OrderStatus::Submitted, false));
    // A replace went out and has not been answered — that is what makes the
    // rejections below answers to something other than the cancel. Without one
    // outstanding, a rejection behind a cancel is the venue's word on the
    // order itself and must stand, or the order waits for ever.
    let before = *context.order(42).expect("the order is tracked");
    context.pre_replace.insert((42, 1), (before, "42.0".to_string(), None));
    assert!(context.update_order_status(42, crate::types::OrderStatus::PendingCancel, false));

    let report = |exec_type: &str, ord_status: &str, text: &str| {
        crate::protocol::fix::fix_build(&[
            (fix::TAG_MSG_TYPE, fix::MSG_EXEC_REPORT),
            (11, "42"), (150, exec_type), (39, ord_status), (58, text),
        ], 1)
    };
    // The race, in the order the venue stated it.
    ccp.process_ccp_message(
        &report("8", "8", "Order has been cancelled already, too late to replace"),
        &mut None, &mut context, &shared, &None, &mut HeartbeatState::new(), "DU1",
    );
    assert!(
        context.order(42).is_some(),
        "the venue still owes the cancel its answer, so the order stays in the book",
    );
    ccp.process_ccp_message(
        &report("8", "8", "Prior submit/modify was rejected"),
        &mut None, &mut context, &shared, &None, &mut HeartbeatState::new(), "DU1",
    );
    ccp.process_ccp_message(
        &report("4", "4", "Revision rejected due to unapproved mod followed by cancel"),
        &mut None, &mut context, &shared, &None, &mut HeartbeatState::new(), "DU1",
    );

    let updates = shared.orders.drain_order_updates();
    assert!(
        updates.iter().any(|u| u.order_id == 42 && u.status == crate::types::OrderStatus::Cancelled),
        "the venue's final word on the order reaches the caller: {updates:?}",
    );
    assert!(
        !updates.iter().any(|u| u.order_id == 42 && u.status == crate::types::OrderStatus::Rejected),
        "the rejections answered the replace, not the cancel: {updates:?}",
    );
    assert!(
        context.order(42).is_none(),
        "and the book lets the order go once the venue has had its final word",
    );
}

/// And the completed record says the same thing the status did.
///
/// The completion was filed before the guard above decided whether the
/// rejection was this order's outcome, so a rejection answering the replace was
/// recorded as how the order finished. That record is kept and refuses a later
/// one, so the caller was told Cancelled on the status and read Rejected in the
/// completed orders — and while the memory stood, every status behind it was
/// dropped as well.
#[test]
fn a_rejection_that_answers_the_replace_is_not_filed_as_how_the_order_finished() {
    let mut ccp = CcpState::new();
    let mut context = Context::new();
    let shared = SharedState::new();
    let instrument = context.register_instrument(756733);
    context.insert_order(crate::types::Order::new(
        42, instrument, Side::Buy, 100 * crate::types::QTY_SCALE, 100 * PRICE_SCALE, b'2', b'0', 0,
    ));
    assert!(context.update_order_status(42, crate::types::OrderStatus::Submitted, false));
    let before = *context.order(42).expect("the order is tracked");
    context.pre_replace.insert((42, 1), (before, "42.0".to_string(), None));
    assert!(context.update_order_status(42, crate::types::OrderStatus::PendingCancel, false));

    let report = |exec_type: &str, ord_status: &str, text: &str| {
        crate::protocol::fix::fix_build(&[
            (fix::TAG_MSG_TYPE, fix::MSG_EXEC_REPORT),
            (11, "42"), (150, exec_type), (39, ord_status), (58, text),
        ], 1)
    };
    ccp.process_ccp_message(
        &report("8", "8", "Order has been cancelled already, too late to replace"),
        &mut None, &mut context, &shared, &None, &mut HeartbeatState::new(), "DU1",
    );
    assert!(
        shared.orders.drain_completed_orders().is_empty(),
        "an answer to the replace is not the order finishing",
    );

    ccp.process_ccp_message(
        &report("4", "4", "Revision rejected due to unapproved mod followed by cancel"),
        &mut None, &mut context, &shared, &None, &mut HeartbeatState::new(), "DU1",
    );
    let done = shared.orders.drain_completed_orders();
    assert_eq!(done.len(), 1, "the cancel's own verdict is filed: {done:?}");
    assert_eq!(done[0].order_id, 42);
    assert_eq!(
        done[0].status, crate::types::OrderStatus::Cancelled,
        "and it says what the status said, not what the rejection said",
    );
}

/// And so does the state cached under the order, which is what a caller asking
/// what it has working reads.
///
/// The cache was written before the guard decided whether the rejection was
/// this order's outcome, so the order stayed in the book — the guard says so —
/// while what was filed under it carried a rejection's status and its reason.
/// A caller polling in between was handed a rejected order the venue was still
/// working.
#[test]
fn a_rejection_that_answers_the_replace_is_not_cached_as_the_orders_state() {
    let mut ccp = CcpState::new();
    let mut context = Context::new();
    let shared = SharedState::new();
    let instrument = context.register_instrument(756733);
    context.insert_order(crate::types::Order::new(
        42, instrument, Side::Buy, 100 * crate::types::QTY_SCALE, 100 * PRICE_SCALE, b'2', b'0', 0,
    ));
    assert!(context.update_order_status(42, crate::types::OrderStatus::Submitted, false));
    let before = *context.order(42).expect("the order is tracked");
    context.pre_replace.insert((42, 1), (before, "42.0".to_string(), None));
    assert!(context.update_order_status(42, crate::types::OrderStatus::PendingCancel, false));

    let report = |exec_type: &str, ord_status: &str, text: &str| {
        crate::protocol::fix::fix_build(&[
            (fix::TAG_MSG_TYPE, fix::MSG_EXEC_REPORT),
            (11, "42"), (150, exec_type), (39, ord_status), (58, text),
        ], 1)
    };
    ccp.process_ccp_message(
        &report("8", "8", "Order has been cancelled already, too late to replace"),
        &mut None, &mut context, &shared, &None, &mut HeartbeatState::new(), "DU1",
    );
    let cached = shared.orders.get_order_info(42);
    assert!(
        cached.as_ref().is_none_or(|info| {
            info.order_state.reject_reason.is_empty() && info.order_state.status != "Inactive"
        }),
        "an answer to the replace is not the working order's state: {:?}",
        cached.map(|i| (i.order_state.status.clone(), i.order_state.reject_reason.clone())),
    );

    // And the cancel's own verdict is cached, as it is filed.
    ccp.process_ccp_message(
        &report("4", "4", "Revision rejected due to unapproved mod followed by cancel"),
        &mut None, &mut context, &shared, &None, &mut HeartbeatState::new(), "DU1",
    );
    assert_eq!(
        shared.orders.get_order_info(42).map(|i| i.order_state.status.clone()).as_deref(),
        Some("Cancelled"),
    );
}

/// And history replayed behind a cancel is not the order's state either.
///
/// A session opens by replaying recent activity, and a working report from
/// before a cancel was sent cannot move the order out of PendingCancel — the
/// status guard says so. The cache did not read that verdict, so it filed the
/// replayed status anyway, and a caller asking what it had working was told
/// Submitted about an order the engine was holding as pending cancel.
#[test]
fn a_working_report_the_status_guard_refused_is_not_cached_as_the_orders_state() {
    let mut ccp = CcpState::new();
    let mut context = Context::new();
    let shared = SharedState::new();
    let instrument = context.register_instrument(756733);
    context.insert_order(crate::types::Order::new(
        42, instrument, Side::Buy, 100 * crate::types::QTY_SCALE, 100 * PRICE_SCALE, b'2', b'0', 0,
    ));
    assert!(context.update_order_status(42, crate::types::OrderStatus::Submitted, false));
    assert!(context.update_order_status(42, crate::types::OrderStatus::PendingCancel, false));

    // 97=Y marks a report that restates history. 150=0/39=0 is a working
    // order, which the venue names PreSubmitted.
    let replayed = crate::protocol::fix::fix_build(&[
        (fix::TAG_MSG_TYPE, fix::MSG_EXEC_REPORT),
        (11, "42"), (150, "0"), (39, "0"), (97, "Y"),
    ], 1);
    ccp.process_ccp_message(
        &replayed, &mut None, &mut context, &shared, &None, &mut HeartbeatState::new(), "DU1",
    );

    assert_eq!(
        context.order(42).map(|o| o.status),
        Some(crate::types::OrderStatus::PendingCancel),
        "the guard kept the order where it was",
    );
    let cached = shared.orders.get_order_info(42);
    assert!(
        cached.as_ref().is_none_or(|i| i.order_state.status != "PreSubmitted"),
        "and the cache says the same thing: {:?}",
        cached.map(|i| i.order_state.status.clone()),
    );
}

// /: in the UP portfolio snapshot the average cost is
// tag 6101 and 6065 is the market price. The handler previously read 6065 as
// the average cost. Verify the mapping and that all marks are stored.
#[test]
fn position_update_maps_marks_and_avg_cost_from_correct_tags() {
    let mut context = Context::new();
    let shared = SharedState::new();
    let mut m = std::collections::HashMap::new();
    m.insert(6008u32, "756733".to_string());   // conId
    m.insert(6064u32, "10".to_string());        // position
    m.insert(6101u32, "100.50".to_string());    // averageCost
    m.insert(6065u32, "110.25".to_string());    // marketPrice
    m.insert(6067u32, "1102.50".to_string());   // marketValue
    m.insert(6100u32, "97.50".to_string());     // unrealizedPNL
    m.insert(6099u32, "5.00".to_string());      // realizedPNL
    positions::handle_position_update(&m, &mut context, &shared, &None);

    let pi = shared.portfolio.position_info(756733).expect("position stored");
    assert_eq!(pi.position, 10.0);
    assert_eq!(pi.avg_cost, (100.50 * PRICE_SCALE as f64) as Price);
    assert_eq!(pi.market_price, (110.25 * PRICE_SCALE as f64) as Price);
    assert_eq!(pi.market_value, (1102.50 * PRICE_SCALE as f64) as Price);
    assert_eq!(pi.unrealized_pnl, (97.50 * PRICE_SCALE as f64) as Price);
    assert_eq!(pi.realized_pnl, (5.00 * PRICE_SCALE as f64) as Price);
}

// The lean position feed carries no marks; it must not zero the marks the
// portfolio snapshot set.
#[test]
fn lean_position_feed_does_not_clobber_marks() {
    let shared = SharedState::new();
    shared.portfolio.set_position_info(PositionInfo {
        con_id: 1, position: 10.0, avg_cost: 100 * PRICE_SCALE, ..Default::default()
    });
    shared.portfolio.set_position_marks(1, Some(110 * PRICE_SCALE), Some(1100 * PRICE_SCALE), Some(100 * PRICE_SCALE), Some(5 * PRICE_SCALE));
    // Lean feed updates position + avg_cost only.
    shared.portfolio.set_position_info(PositionInfo {
        con_id: 1, position: 12.0, avg_cost: 101 * PRICE_SCALE, ..Default::default()
    });
    let pi = shared.portfolio.position_info(1).unwrap();
    assert_eq!(pi.position, 12.0);
    assert_eq!(pi.avg_cost, 101 * PRICE_SCALE);
    assert_eq!(pi.market_price, 110 * PRICE_SCALE, "marks survive the lean feed");
    assert_eq!(pi.market_value, 1100 * PRICE_SCALE);
    assert_eq!(pi.unrealized_pnl, 100 * PRICE_SCALE);
}

// The TIF decoder must be the exact inverse of the outbound
// encoder. The old map decoded '7' (never emitted) as OPG and dropped
// OPG and AUC to "".
#[test]
fn tif_round_trips_through_encoder_and_decoder() {
    for tif in ["DAY", "GTC", "OPG", "IOC", "FOK", "GTD", "GTX", "AUC"] {
        let order = api::Order { tif: tif.to_string(), ..Default::default() };
        assert_eq!(decode_tif(order.tif_byte()), tif,
            "TIF {tif} must survive encode->decode");
    }
    // Day-till-cancelled goes out as good-till-cancelled with the flag that
    // stands it down at the day's end beside it, so tag 59 alone decodes as
    // GTC. The two are separate lives with separate names and that tag does
    // not carry the difference — the flag does.
    let dtc = api::Order { tif: "DTC".to_string(), ..Default::default() };
    assert_eq!(decode_tif(dtc.tif_byte()), "GTC");
    // Unknown bytes decode to empty, not a wrong TIF.
    assert_eq!(decode_tif(b'7'), "");
}

/// A refusal naming a request of another kind does not answer a definition
/// lookup that was never refused.
///
/// Tag 320 is not this request's alone — a symbol search states its own number
/// on it. Falling back on the count even where the venue HAD named something
/// meant such a refusal took the one pending lookup: that caller was handed
/// somebody else's refusal and its own definitions later had nothing waiting,
/// while the caller actually refused was never told. The option-chain branch
/// beside it already guards on the venue having named nothing.
#[test]
fn a_refusal_naming_another_request_does_not_answer_a_definition_lookup() {
    let mut ccp = CcpState::new();
    let mut context = Context::new();
    let shared = SharedState::new();
    ccp.pending_secdef.push((7, true, Instant::now()));

    // The venue names request 5 — not this lookup, and not one of ours to find.
    let reject = crate::protocol::fix::fix_build(&[
        (fix::TAG_MSG_TYPE, "3"),
        (58, "Unknown contract"),
        (320, "5"),
    ], 1);
    ccp.process_ccp_message(
        &reject, &mut None, &mut context, &shared, &None, &mut HeartbeatState::new(), "DU1",
    );

    assert_eq!(
        ccp.pending_secdef.len(), 1,
        "the lookup the venue did not name is still waiting for its answer",
    );
    assert!(
        shared.reference.drain_historical_errors().is_empty(),
        "and nothing was refused on its behalf",
    );
}

// ── contract-details deadline sweep ──

#[test]
fn sweep_times_out_pending_secdef_with_error_and_end() {
    let mut ccp = CcpState::new();
    let shared = SharedState::new();
    let past = Instant::now() - std::time::Duration::from_secs(1);
    ccp.pending_secdef.push((7, true, past));

    ccp.sweep_contract_details(&shared, &None);

    assert!(ccp.pending_secdef.is_empty(), "expired entry must be reclaimed");
    let errors = shared.reference.drain_historical_errors();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].0, 7);
    // Silence is not the venue's empty answer: 200 says the search ran and
    // matched nothing, and a caller branching on it stops asking.
    assert_eq!(errors[0].1, -1, "no reply is reported as no answer");
    assert_eq!(shared.reference.drain_contract_details_end(), vec![7],
        "end must fire so a blocked wait unblocks");
}

#[test]
fn sweep_drops_internal_secdef_silently() {
    let mut ccp = CcpState::new();
    let shared = SharedState::new();
    let past = Instant::now() - std::time::Duration::from_secs(1);
    // Internal sentinel (cache auto-fetch): no user is waiting on it.
    ccp.pending_secdef.push((0xF000_0001, true, past));

    ccp.sweep_contract_details(&shared, &None);

    assert!(ccp.pending_secdef.is_empty());
    assert!(shared.reference.drain_historical_errors().is_empty());
    assert!(shared.reference.drain_contract_details_end().is_empty());
}

#[test]
fn sweep_times_out_incomplete_fanout() {
    let mut ccp = CcpState::new();
    let shared = SharedState::new();
    ccp.pending_fanout.push(PendingFanout {
        api_req_id: 9,
        fanout_req_ids: (0..27).map(|i| format!("ibxfan-9-{i}")).collect(),
        // one leg never answered — previously hung forever
        answered: (0..26).map(|i| format!("ibxfan-9-{i}")).collect(),
        deadline: Instant::now() - std::time::Duration::from_secs(1),
    });

    ccp.sweep_contract_details(&shared, &None);

    assert!(ccp.pending_fanout.is_empty());
    let errors = shared.reference.drain_historical_errors();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].0, 9);
    assert_eq!(errors[0].1, -1, "a leg that never answered is no answer, not an empty one");
    assert_eq!(shared.reference.drain_contract_details_end(), vec![9]);
}

// ── a con_id=0 secdef reply is "not found", not a contract ──

/// The gateway's "no security definition" answer: a `35=d` echoing the
/// request id and carrying con_id 0 — no symbol, no price-increment block.
fn secdef_not_found(req_id: &str) -> Vec<u8> {
    crate::protocol::fix::fix_build(&[
        (fix::TAG_MSG_TYPE, "d"),
        (crate::control::contracts::TAG_SECURITY_REQ_ID, req_id),
        (crate::control::contracts::TAG_SECURITY_RESPONSE_TYPE, "4"),
        (crate::control::contracts::TAG_IB_CON_ID, "0"),
    ], 1)
}

/// A request carrying a contract rather than an id waits for the venue to
/// name it, and goes out once it has. Sent as it stood, it went out under
/// id zero and the venue answered a complete series with nothing in it.
#[test]
fn a_request_naming_a_contract_waits_to_be_given_its_id() {
    let (mut ccp, _context, shared) = u186_test_state();
    let bars = crate::types::ControlCommand::FetchHistorical {
        contract: crate::types::ContractRef { con_id: 0, symbol: "SPY".into(), sec_type: "STK".into(), exchange: "SMART".into(), currency: "USD".into(), ..Default::default() },
        req_id: 7,
        end_date_time: String::new(),
        duration: "2 D".into(),
        bar_size: "1 hour".into(),
            what_to_show: "TRADES".into(),
        use_rth: true,
        keep_up_to_date: false,
        include_expired: false,
        filters: Default::default(),
    };

    assert!(
        ccp.hold_until_named(bars, &mut None, &mut HeartbeatState::new(), &shared).is_none(),
        "held rather than sent under no id",
    );
    assert_eq!(ccp.pending_named.len(), 1);

    // The venue names it.
    let lookup = ccp.pending_named[0].0;
    let (_, mut held, _) = ccp.pending_named.remove(0);
    name_the_contract(&mut held, 756_733);
    let _ = lookup;
    match ccp.hold_until_named(held, &mut None, &mut HeartbeatState::new(), &shared) {
        Some(crate::types::ControlCommand::FetchHistorical { contract: crate::types::ContractRef { con_id, .. }, req_id, .. }) => {
            assert_eq!((req_id, con_id), (7, 756_733), "sent under the id it was given");
        }
        other => panic!("a named request is handled, not held again: {other:?}"),
    }

    // And one the venue never names is reported rather than left waiting.
    let unnamed = crate::types::ControlCommand::FetchHistorical { contract: crate::types::ContractRef { con_id: 0, symbol: "NOSUCH".into(), sec_type: "STK".into(), exchange: "SMART".into(), currency: "USD".into(), ..Default::default() }, end_date_time: String::new(), req_id: 8, duration: "1 D".into(), bar_size: "1 hour".into(), what_to_show: "TRADES".into(), use_rth: true, keep_up_to_date: false, include_expired: false, filters: Default::default() };
    ccp.hold_until_named(unnamed, &mut None, &mut HeartbeatState::new(), &shared);
    ccp.pending_named[0].2 -= CcpState::NAMING_TIMEOUT + Duration::from_secs(1);
    ccp.sweep_pending_named(&shared);
    assert!(ccp.pending_named.is_empty());
    let errors = shared.reference.drain_historical_errors();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].0, 8);
}

/// A lookup the venue never answers must not leave the subscription
/// waiting in silence. That is the failure this whole path exists to
/// remove, and it would have reappeared one level down.
#[test]
fn a_subscription_the_venue_never_names_is_reported() {
    let (mut ccp, mut context, shared) = u186_test_state();
    let parked = PendingSubscribe {
        issued: 0,
        filters: Default::default(),
        con_id: 0,
        instrument: 4,
        symbol: "NOSUCH".into(),
        exchange: "SMART".into(),
        sec_type: "STK".into(),
        currency: "USD".into(),
        mode_9887: 0, regulatory_snapshot: false,
    };
    ccp.resolve_for_subscribe(parked, &mut None, &mut HeartbeatState::new(), &shared);

    ccp.sweep_pending_subscribes(&mut context, &shared);
    assert_eq!(ccp.pending_md_subscribe.len(), 1, "still within its wait");
    assert!(shared.market.drain_subscription_failures().is_empty());

    // Wind the clock back past the wait.
    let asked_at = &mut ccp.pending_md_subscribe[0].2;
    *asked_at -= CcpState::NAMING_TIMEOUT + Duration::from_secs(1);
    ccp.sweep_pending_subscribes(&mut context, &shared);

    assert!(ccp.pending_md_subscribe.is_empty(), "given up on");
    let failures = shared.market.drain_subscription_failures();
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].0, 4, "reported against the slot that asked");
    assert!(failures[0].1.contains("NOSUCH"), "and names it: {}", failures[0].1);
}

/// The venue answers a market data subscription only when it is named by
/// contract id, so a subscription for a contract named by symbol waits on
/// the lookup that names it. It is held until the definition arrives, and
/// released with the id the definition carried.
#[test]
fn a_subscription_waits_for_the_lookup_that_names_its_contract() {
    let (mut ccp, mut context, shared) = u186_test_state();
    let parked = PendingSubscribe {
        issued: 0,
        filters: Default::default(),
        con_id: 0,
        instrument: 3,
        symbol: "SPY".into(),
        exchange: "SMART".into(),
        sec_type: "STK".into(),
        currency: "USD".into(),
        mode_9887: 0, regulatory_snapshot: false,
    };
    ccp.resolve_for_subscribe(parked, &mut None, &mut HeartbeatState::new(), &shared);
    let req_id = ccp.pending_md_subscribe[0].0;
    assert!(req_id >= 0xF000_0000, "asked for on the engine's own account, not a caller's");
    assert!(ccp.resolved_md_subscribe.is_empty(), "nothing to send until it is named");

    let named = crate::protocol::fix::fix_build(&[
        (fix::TAG_MSG_TYPE, "d"),
        (crate::control::contracts::TAG_SECURITY_REQ_ID, &req_id.to_string()),
        (crate::control::contracts::TAG_SECURITY_RESPONSE_TYPE, "4"),
        (55, "SPY"),
        (crate::control::contracts::TAG_IB_CON_ID, "756733"),
    ], 1);
    ccp.process_ccp_message(&named, &mut None, &mut context, &shared,
        &None, &mut HeartbeatState::new(), "DU1");

    assert!(ccp.pending_md_subscribe.is_empty(), "no longer waiting");
    assert_eq!(ccp.resolved_md_subscribe.len(), 1);
    let (con_id, released) = &ccp.resolved_md_subscribe[0];
    assert_eq!(*con_id, 756733, "the id the venue gave it");
    assert_eq!(released.instrument, 3, "for the slot that asked");
    assert_eq!(released.symbol, "SPY");
}

#[test]
fn secdef_not_found_by_symbol_is_an_error_not_a_row() {
    let (mut ccp, mut context, shared) = u186_test_state();
    ccp.pending_secdef.push((7, false, Instant::now() + SECDEF_TIMEOUT));

    ccp.process_ccp_message(&secdef_not_found("7"), &mut None, &mut context, &shared,
        &None, &mut HeartbeatState::new(), "DU1");

    assert!(shared.reference.drain_contract_details().is_empty(),
        "con_id=0 is the gateway saying 'no definition' — emitting it as a row \
         hands the caller a fabricated min_tick that reads like a hit");
    let errors = shared.reference.drain_historical_errors();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].0, 7);
    assert_eq!(errors[0].1, 200);
    assert_eq!(shared.reference.drain_contract_details_end(), vec![7],
        "end must still fire so a blocked wait unblocks");
}

#[test]
fn secdef_not_found_by_conid_errors_and_ends() {
    let (mut ccp, mut context, shared) = u186_test_state();
    // Known-conId lookup: single record, is_last regardless of the wire flag.
    ccp.pending_secdef.push((7, true, Instant::now() + SECDEF_TIMEOUT));

    ccp.process_ccp_message(&secdef_not_found("7"), &mut None, &mut context, &shared,
        &None, &mut HeartbeatState::new(), "DU1");

    assert!(shared.reference.drain_contract_details().is_empty());
    let errors = shared.reference.drain_historical_errors();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].0, 7);
    assert_eq!(errors[0].1, 200);
    assert_eq!(shared.reference.drain_contract_details_end(), vec![7]);
    assert!(ccp.pending_secdef.is_empty(), "the request is finished");
}

#[test]
fn secdef_not_found_stays_silent_for_an_internal_fetch() {
    let (mut ccp, mut context, shared) = u186_test_state();
    // Cache auto-fetch sentinel: no user is waiting on it.
    ccp.pending_secdef.push((0xF000_0001, true, Instant::now() + SECDEF_TIMEOUT));

    ccp.process_ccp_message(&secdef_not_found("4026531841"), &mut None, &mut context,
        &shared, &None, &mut HeartbeatState::new(), "DU1");

    assert!(shared.reference.drain_contract_details().is_empty());
    assert!(shared.reference.drain_historical_errors().is_empty());
    assert!(shared.reference.drain_contract_details_end().is_empty());
}

#[test]
fn a_fanout_reply_without_a_con_id_is_not_a_row() {
    let (mut ccp, mut context, shared) = u186_test_state();
    ccp.pending_fanout.push(PendingFanout {
        api_req_id: 9,
        fanout_req_ids: vec!["ibxfan-9-0".to_string()],
        answered: Vec::new(),
        deadline: Instant::now() + SECDEF_TIMEOUT,
    });

    ccp.process_ccp_message(&secdef_not_found("ibxfan-9-0"), &mut None, &mut context,
        &shared, &None, &mut HeartbeatState::new(), "DU1");

    assert!(shared.reference.drain_contract_details().is_empty(),
        "a per-exchange leg with no con_id is not a contract either");
        assert_eq!(shared.reference.drain_contract_details_end(), vec![9],
        "the fan-out still completes");
    assert!(ccp.pending_fanout.is_empty());
}

// ── matching-symbols attribution ──

fn matching_symbols_msg(req_id: &str, symbols: &[(&str, &str)]) -> Vec<u8> {
    let count = symbols.len().to_string();
    let mut fields: Vec<(u32, &str)> = vec![
        (crate::protocol::fix::TAG_MSG_TYPE, "U"),
        (6040, "186"),
        (320, req_id),
        (146, &count), // match count — marks a data frame (even when 0)
    ];
    for (sym, con_id) in symbols {
        fields.push((55, sym));
        fields.push((167, "CS"));
        fields.push((15, "USD"));
        fields.push((6008, con_id));
    }
    crate::protocol::fix::fix_build(&fields, 1)
}

/// A 186 frame with no match-count tag: the not-ready ack that precedes
/// the data frame.
fn matching_symbols_ack(req_id: &str) -> Vec<u8> {
    crate::protocol::fix::fix_build(&[
        (crate::protocol::fix::TAG_MSG_TYPE, "U"),
        (6040, "186"),
        (320, req_id),
    ], 1)
}

fn u186_test_state() -> (CcpState, Context, SharedState) {
    (CcpState::new(), Context::new(), SharedState::new())
}

/// Both figures a recovered order's fill is worked out from come off the wire,
/// so their difference need not be one.
///
/// A report that both recovers an order and books a fill counts its own shares
/// in the cumulative figure, and this takes them back out. Taken plain, a
/// cumulative figure the venue states near the bottom of the range and a fill
/// near the top underflow the subtraction — on the engine thread, where a
/// panic ends the session and every subscription on it.
#[test]
fn a_recovered_order_survives_a_cumulative_quantity_the_wire_states_at_the_edge() {
    let (mut ccp, mut context, shared) = u186_test_state();
    // Under the largest quantity `parse_qty_tag` will carry, stated at both
    // ends of the range: the cumulative figure at the bottom, this report's
    // own fill at the top.
    let edge = (crate::types::Qty::MAX / crate::types::QTY_SCALE) as f64 * 0.9;
    let report = crate::protocol::fix::fix_build(&[
        (fix::TAG_MSG_TYPE, fix::MSG_EXEC_REPORT),
        (11, "42"), (150, "F"), (39, "1"), (6008, "756733"),
        // A side and a quantity, which is what makes this the recovery of an
        // order this session does not hold.
        (54, "1"), (38, "100"),
        (14, &format!("{:.0}", -edge)),
        (32, &format!("{edge:.0}")),
    ], 1);
    ccp.process_ccp_message(
        &report, &mut None, &mut context, &shared, &None, &mut HeartbeatState::new(), "DU1",
    );
    // What matters is that the engine is still here to be asked. A quantity
    // that cannot be worked out is nought filled, not a number below it.
    assert!(
        context.order(42).is_none_or(|o| o.filled >= 0),
        "a fill is never a negative quantity",
    );
}

/// Every deadline the engine keeps for a caller's request has to expire
/// before the caller stops waiting, or the caller is told nothing arrived
/// while the reason is still held here and reported to nobody.
#[test]
fn the_engine_answers_before_a_caller_gives_up() {
    let caller = Duration::from_secs(crate::config::ANSWER_TIMEOUT_SECS);
    let asking = Duration::from_secs(crate::config::LOOKUP_TIMEOUT_SECS);
    assert!(
        SECDEF_TIMEOUT < CcpState::NAMING_TIMEOUT,
        "a lookup's own answer is preferred to the fallback that covers it",
    );
    assert!(
        CcpState::NAMING_TIMEOUT < caller,
        "a held request is reported before the caller stops listening",
    );
    assert!(SECDEF_TIMEOUT < caller);
    // A lookup a caller asked for has nothing covering it, so the one deadline
    // it has to sit under is that caller's own.
    assert!(
        LOOKUP_TIMEOUT < asking,
        "a lookup is reported before the caller asking for it stops listening",
    );
    assert_eq!(unanswered_after(crate::bridge::ENGINE_ID_BASE), SECDEF_TIMEOUT);
    assert_eq!(unanswered_after(7), LOOKUP_TIMEOUT);
}

/// A lookup the venue never answers has to end anyway. A caller that
/// asked through a library holding a future is waiting on the end of this
/// request, and a request that simply stops existing leaves it waiting for
/// as long as the program runs.
#[test]
fn a_lookup_the_venue_never_answers_is_ended_rather_than_left() {
    let (mut ccp, _context, shared) = u186_test_state();
    // Asked for by the venue's id for the contract, which is the shape
    // a caller uses when it names nothing else.
    ccp.pending_secdef.push((4242, true, Instant::now() - Duration::from_secs(1)));

    ccp.sweep_contract_details(&shared, &None);

    let refused = shared.reference.drain_historical_errors();
    assert_eq!(refused.len(), 1, "the caller is told, once");
    assert_eq!(refused[0].0, 4242, "under the id it asked with");
    assert_eq!(
        shared.reference.drain_contract_details_end(),
        vec![4242],
        "and the request ends, which is what a waiting future is waiting for",
        );
    assert!(ccp.pending_secdef.is_empty(), "and nothing is left pending");
}

#[test]
fn matching_symbols_matched_by_echoed_req_id_not_fifo() {
    let (mut ccp, mut context, shared) = u186_test_state();
    ccp.pending_matching_symbols.push((1, Instant::now() + MATCHING_SYMBOLS_TIMEOUT));
    ccp.pending_matching_symbols.push((2, Instant::now() + MATCHING_SYMBOLS_TIMEOUT));

    // Request 2's reply arrives FIRST (out of order).
    let msg = matching_symbols_msg("2", &[("AAPL", "265598")]);
    ccp.process_ccp_message(&msg, &mut None, &mut context, &shared, &None, &mut HeartbeatState::new(), "DU1");

    let delivered = shared.reference.drain_matching_symbols();
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].0, 2, "reply must land on the echoed req_id, not the queue head");
    assert_eq!(delivered[0].1.len(), 1);
    assert_eq!(ccp.pending_matching_symbols.iter().map(|(r, _)| *r).collect::<Vec<_>>(), vec![1]);
}

#[test]
fn matching_symbols_empty_result_pops_and_delivers() {
    let (mut ccp, mut context, shared) = u186_test_state();
    ccp.pending_matching_symbols.push((1, Instant::now() + MATCHING_SYMBOLS_TIMEOUT));
    ccp.pending_matching_symbols.push((2, Instant::now() + MATCHING_SYMBOLS_TIMEOUT));

    // Unknown pattern: zero matches. Must still pop req 1 and deliver
    // the empty answer — previously this poisoned the queue head and
    // every later reply was off by one, forever.
    let msg = matching_symbols_msg("1", &[]);
    ccp.process_ccp_message(&msg, &mut None, &mut context, &shared, &None, &mut HeartbeatState::new(), "DU1");

    let delivered = shared.reference.drain_matching_symbols();
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].0, 1);
    assert!(delivered[0].1.is_empty(), "empty result is a legitimate answer");
        assert_eq!(ccp.pending_matching_symbols.iter().map(|(r, _)| *r).collect::<Vec<_>>(), vec![2],
        "queue must not be poisoned by an empty result");

    // The next reply attributes correctly.
    let msg = matching_symbols_msg("2", &[("MSFT", "272093")]);
    ccp.process_ccp_message(&msg, &mut None, &mut context, &shared, &None, &mut HeartbeatState::new(), "DU1");
    let delivered = shared.reference.drain_matching_symbols();
    assert_eq!(delivered[0].0, 2);
    assert!(ccp.pending_matching_symbols.is_empty());
}

/// A symbol search the venue refuses is answered with the refusal.
///
/// It states its own number on tag 320, the same tag the definition lookup and
/// the option chain state theirs on, and only those two were matched against
/// it. So a refused search matched nothing, stayed queued, and the caller
/// waited out the sweep's timeout to be told the venue had never replied —
/// when it had replied at once, and said why.
#[test]
fn a_refused_symbol_search_is_told_the_reason_rather_than_left_to_time_out() {
    let (mut ccp, mut context, shared) = u186_test_state();
    ccp.pending_matching_symbols.push((1, Instant::now() + MATCHING_SYMBOLS_TIMEOUT));
    ccp.pending_matching_symbols.push((2, Instant::now() + MATCHING_SYMBOLS_TIMEOUT));

    let msg = crate::protocol::fix::fix_build(&[
        (fix::TAG_MSG_TYPE, "3"),
        (320, "2"),
        (58, "Unknown contract"),
    ], 1);
    ccp.process_ccp_message(&msg, &mut None, &mut context, &shared, &None, &mut HeartbeatState::new(), "DU1");

    let refused = shared.reference.drain_historical_errors();
    assert_eq!(refused.len(), 1, "the caller is told: {refused:?}");
    assert_eq!(refused[0].0, 2, "the one the venue named, not the queue head");
    assert!(refused[0].2.contains("Unknown contract"), "and why: {}", refused[0].2);
    assert_eq!(
        ccp.pending_matching_symbols.iter().map(|(r, _)| *r).collect::<Vec<_>>(),
        vec![1],
        "the refused request is off the queue and the other still waits",
    );
}

/// And a refusal that names nothing is not handed to a definition lookup while
/// a symbol search is outstanding — the search may be the one refused, and that
/// caller would then be handed somebody else's refusal.
#[test]
fn a_nameless_refusal_is_not_attributed_while_a_symbol_search_is_outstanding() {
    let (mut ccp, mut context, shared) = u186_test_state();
    ccp.pending_secdef.push((4242, false, Instant::now() + MATCHING_SYMBOLS_TIMEOUT));
    ccp.pending_matching_symbols.push((1, Instant::now() + MATCHING_SYMBOLS_TIMEOUT));

    let msg = crate::protocol::fix::fix_build(&[
        (fix::TAG_MSG_TYPE, "3"),
        (58, "Unknown contract"),
    ], 1);
    ccp.process_ccp_message(&msg, &mut None, &mut context, &shared, &None, &mut HeartbeatState::new(), "DU1");

    assert!(
        shared.reference.drain_historical_errors().is_empty(),
        "neither is told, because which one was refused is not stated",
    );
    assert_eq!(ccp.pending_secdef.len(), 1, "and the lookup still waits");
}

#[test]
fn matching_symbols_ack_frame_does_not_consume_the_request() {
    let (mut ccp, mut context, shared) = u186_test_state();
    ccp.pending_matching_symbols.push((1, Instant::now() + MATCHING_SYMBOLS_TIMEOUT));

    // The not-ready ack (no tag 146) arrives first — it must not pop the
    // request; delivering it as an empty answer orphans the data frame
    // that follows (observed live).
    let msg = matching_symbols_ack("1");
    ccp.process_ccp_message(&msg, &mut None, &mut context, &shared, &None, &mut HeartbeatState::new(), "DU1");
    assert!(shared.reference.drain_matching_symbols().is_empty());
    assert_eq!(ccp.pending_matching_symbols.iter().map(|(r, _)| *r).collect::<Vec<_>>(), vec![1]);

    // The data frame then delivers.
    let msg = matching_symbols_msg("1", &[("AAPL", "265598")]);
    ccp.process_ccp_message(&msg, &mut None, &mut context, &shared, &None, &mut HeartbeatState::new(), "DU1");
    let delivered = shared.reference.drain_matching_symbols();
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].0, 1);
    assert_eq!(delivered[0].1.len(), 1);
    assert!(ccp.pending_matching_symbols.is_empty());
}

#[test]
fn matching_symbols_unattributable_reply_is_dropped_not_misattributed() {
    let (mut ccp, mut context, shared) = u186_test_state();
    ccp.pending_matching_symbols.push((1, Instant::now() + MATCHING_SYMBOLS_TIMEOUT));
    ccp.pending_matching_symbols.push((2, Instant::now() + MATCHING_SYMBOLS_TIMEOUT));

    // Echoed id matches nothing pending: with two in flight, guessing
    // would cross-attribute — drop with a warn instead.
    let msg = matching_symbols_msg("99", &[("AAPL", "265598")]);
    ccp.process_ccp_message(&msg, &mut None, &mut context, &shared, &None, &mut HeartbeatState::new(), "DU1");

    assert!(shared.reference.drain_matching_symbols().is_empty());
    assert_eq!(ccp.pending_matching_symbols.iter().map(|(r, _)| *r).collect::<Vec<_>>(), vec![1, 2]);
}

#[test]
fn sweep_spares_live_entries() {
    let mut ccp = CcpState::new();
    let shared = SharedState::new();
    let future = Instant::now() + SECDEF_TIMEOUT;
    ccp.pending_secdef.push((7, true, future));
    ccp.pending_fanout.push(PendingFanout {
        api_req_id: 9,
        fanout_req_ids: vec!["ibxfan-9-0".to_string()],
        answered: Vec::new(),
        deadline: future,
    });

    ccp.sweep_contract_details(&shared, &None);

    assert_eq!(ccp.pending_secdef.len(), 1);
    assert_eq!(ccp.pending_fanout.len(), 1);
    assert!(shared.reference.drain_historical_errors().is_empty());
    assert!(shared.reference.drain_contract_details_end().is_empty());
}

/// A fill whose ClOrdID this session never tracked. Every field the engine
/// needs to book it is on the report itself.
fn untracked_fill(pairs: &[(u32, &str)]) -> std::collections::HashMap<u32, String> {
    let mut m = std::collections::HashMap::new();
    for (tag, val) in [
        (11u32, "99"),      // ClOrdID the context does not know
        (150, "2"),         // ExecType: trade
        (39, "2"),          // OrdStatus: filled
        (32, "5"),          // LastShares
        (31, "100.00"),     // LastPx
        (54, "1"),          // Side: buy
        (6008, "888888"),   // ContractID
        (55, "ZZZ"),
        (17, "EXEC-1"),
    ] {
        m.insert(tag, val.to_string());
    }
    for (tag, val) in pairs {
        if val.is_empty() {
            m.remove(tag);
        } else {
            m.insert(*tag, val.to_string());
        }
    }
    m
}

/// A fill for an order this session does not track is still a position the
/// account holds. Dropping it leaves the engine short of the truth with
/// nothing to say so — the cancel/fill race reaches this every time.
#[test]
fn a_fill_for_an_untracked_order_is_still_booked() {
    let (mut ccp, mut context, shared) = ord_status_test_state();
    let frame = untracked_fill(&[]);

    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");

    let fills = shared.orders.drain_fills();
    assert_eq!(fills.len(), 1, "the fill must be reported");
    assert_eq!(fills[0].0.qty, 5 * QTY_SCALE);
    assert_eq!(fills[0].0.order_id, 99);
    assert_eq!(fills[0].0.side, Side::Buy);
    assert_eq!(
        context.position(fills[0].0.instrument), 5.0,
        "the position must move by the filled quantity",
    );
}

/// A sell books the other way. Taking the side from the report rather than
/// defaulting is the whole point: the wrong sign is worse than no fill.
#[test]
fn an_untracked_sell_moves_the_position_down() {
    let (mut ccp, mut context, shared) = ord_status_test_state();
    let frame = untracked_fill(&[(54, "2")]);

    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");

    let fills = shared.orders.drain_fills();
    assert_eq!(fills.len(), 1);
    assert_eq!(fills[0].0.side, Side::Sell);
    assert_eq!(context.position(fills[0].0.instrument), -5.0);
}

/// Without a contract or a side there is nothing to book against, and
/// guessing either one would move a real position the wrong way.
#[test]
fn an_untracked_fill_is_not_booked_on_a_guess() {
    for missing in [6008u32, 54] {
        let (mut ccp, mut context, shared) = ord_status_test_state();
        let frame = untracked_fill(&[(missing, "")]);

        ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");

        assert!(
            shared.orders.drain_fills().is_empty(),
            "tag {missing} missing: must not book a guessed fill",
        );
    }
}

/// On a fresh process the gateway resends prior executions with 97=Y and
/// their original ExecIDs, for orders no session tracks. Booking those
/// builds a position out of history on top of the one the position feed
/// already reports.
#[test]
fn a_replayed_execution_is_not_booked_as_a_new_position() {
    for (tag, name) in [(97u32, "PossResend"), (43, "PossDupFlag")] {
        let (mut ccp, mut context, shared) = ord_status_test_state();

        ccp.handle_exec_report(&untracked_fill(&[(tag, "Y")]), b"", &mut context, &shared, &None, "");

        assert!(
            shared.orders.drain_fills().is_empty(),
            "{name}=Y restates history and must not move the position",
        );
    }

    // The same report without the marker is booked, so the guard is the
    // marker and not something else about the frame.
    let (mut ccp, mut context, shared) = ord_status_test_state();
    ccp.handle_exec_report(&untracked_fill(&[(97, "N")]), b"", &mut context, &shared, &None, "");
    assert_eq!(shared.orders.drain_fills().len(), 1);
}

/// A completed order's replay carries a cumulative quantity and no local
/// record to reconcile against. The marker is what stops it, and it must
/// stop it before the cumulative figure is read.
#[test]
fn a_replay_with_a_cumulative_quantity_is_still_not_booked() {
    let (mut ccp, mut context, shared) = ord_status_test_state();

    ccp.handle_exec_report(
        &untracked_fill(&[(97, "Y"), (14, "100"), (32, "100")]),
        b"", &mut context, &shared, &None, "",
    );

    assert!(
        shared.orders.drain_fills().is_empty(),
        "a replayed history for an order this session never saw is not a fill",
    );
}

/// An execution that could not be booked must stay replayable. Consuming
/// the ExecID for a fill that was dropped makes the loss permanent: the
/// replay after a reconnect is then rejected as a duplicate.
#[test]
fn an_unbookable_fill_does_not_consume_its_exec_id() {
    let (mut ccp, mut context, shared) = ord_status_test_state();

    // Same execution, first seen without the contract that would let the
    // engine place it.
    ccp.handle_exec_report(&untracked_fill(&[(6008, "")]), b"", &mut context, &shared, &None, "");
    assert!(shared.orders.drain_fills().is_empty());

    // Replayed in full — it must not be rejected as already seen.
    ccp.handle_exec_report(&untracked_fill(&[]), b"", &mut context, &shared, &None, "");
    assert_eq!(
        shared.orders.drain_fills().len(), 1,
        "the replay must be booked, not dropped as a duplicate",
    );
}

/// A fractional order fills in fractions, and the venue states them as
/// decimals. Read as an integer, `32=0.5` parsed to nothing: the fill was
/// reported as zero shares and the position never moved, on a client that
/// accepts fractional orders.
#[test]
fn a_fractional_print_books_the_fraction_it_states() {
    let (mut ccp, mut context, shared) = ord_status_test_state();
    let before = context.position(0);
    let frame = exec_report_frame(&[
        (150, "F"), (17, "EXEC-FRAC"), (100, "ARCA"), (198, "ARCA:1"),
        (32, "0.5"), (31, "101.00"), (14, "0.5"), (6, "101.00"), (151, "0.25"), (39, "1"),
    ]);

    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");

    let fills = shared.orders.drain_fills();
    assert_eq!(fills.len(), 1, "the fill is reported");
    assert_eq!(fills[0].0.qty, QTY_SCALE / 2, "half a share books as half a share");
    assert_eq!(fills[0].0.cum_qty, QTY_SCALE / 2, "and the order total states the same");
    assert_eq!(fills[0].0.remaining, QTY_SCALE / 4, "as does what is still working");
    assert_eq!(
        context.position(0) - before, 0.5,
        "and the position moves by the fraction that filled",
    );
    assert_eq!(
        context.order(42).unwrap().filled, QTY_SCALE / 2,
        "and the order records the fraction as filled",
    );
}

/// The cumulative pair has to come off the wire. Tag 14 is the order's
/// filled total and tag 6 its volume-weighted average; 32 and 31 describe
/// only the print that triggered the report.
#[test]
fn the_fill_carries_the_orders_totals_not_the_prints() {
    let (mut ccp, mut context, shared) = ord_status_test_state();
    // Second print of 5 at 101, taking the order to 12 filled at 100.50.
    let frame = untracked_fill(&[
        (32, "5"), (31, "101.00"), (14, "12"), (6, "100.50"), (151, "3"), (39, "1"),
    ]);

    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");

    let fills = shared.orders.drain_fills();
    assert_eq!(fills.len(), 1);
    assert_eq!(fills[0].0.qty, 5 * QTY_SCALE, "qty stays the print");
    assert_eq!(fills[0].0.price, 101 * PRICE_SCALE, "price stays the print");
    assert_eq!(fills[0].0.cum_qty, 12 * QTY_SCALE, "cum_qty is the order total from tag 14");
    assert_eq!(
        fills[0].0.avg_price, 100 * PRICE_SCALE + PRICE_SCALE / 2,
        "avg_price is the volume-weighted average from tag 6",
    );
}

/// Without tag 14 the print alone is not a substitute: on a later fill it
/// is smaller than what was already reported, so `filled` would go
/// backwards. The order's own accumulated quantity carries it instead.
#[test]
fn a_missing_cumulative_quantity_does_not_walk_backwards() {
    let (mut ccp, mut context, shared) = ord_status_test_state();

    // Seven filled so far, stated.
    ccp.handle_exec_report(
        &exec_report_frame(&[
            (150, "2"), (39, "1"), (32, "7"), (31, "100.00"), (14, "7"), (6, "100.00"),
            (151, "3"), (17, "E1"),
        ]), b"",
        &mut context, &shared, &None, "",
    );
    let first = shared.orders.drain_fills();
    assert_eq!(first[0].0.cum_qty, 7 * QTY_SCALE);

    // One more, with the cumulative fields absent.
    ccp.handle_exec_report(
        &exec_report_frame(&[
            (150, "2"), (39, "1"), (32, "1"), (31, "101.00"), (151, "2"), (17, "E2"),
        ]), b"",
        &mut context, &shared, &None, "",
    );
    let second = shared.orders.drain_fills();
    assert_eq!(
        second[0].0.cum_qty, 8 * QTY_SCALE,
        "the order's own total carries it, rather than dropping back to the print",
    );
}

/// A negative average price is a real value for a spread quoted as a net
/// credit, so only an absent or unparseable tag falls back.
#[test]
fn a_negative_average_price_is_not_treated_as_absent() {
    let (mut ccp, mut context, shared) = ord_status_test_state();
    let frame = untracked_fill(&[(32, "5"), (31, "-2.00"), (14, "5"), (6, "-1.50")]);

    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");

    let fills = shared.orders.drain_fills();
    assert_eq!(fills[0].0.avg_price, -(PRICE_SCALE + PRICE_SCALE / 2), "-1.50 is kept");
}

/// With no order to accumulate against and no tags, the print is all there
/// is — which is what the callback reported before.
#[test]
fn the_fill_falls_back_to_the_print_when_the_totals_are_absent() {
    let (mut ccp, mut context, shared) = ord_status_test_state();
    let frame = untracked_fill(&[(32, "5"), (31, "101.00"), (14, ""), (6, "")]);

    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");

    let fills = shared.orders.drain_fills();
    assert_eq!(fills.len(), 1);
    assert_eq!(fills[0].0.cum_qty, 5 * QTY_SCALE);
    assert_eq!(fills[0].0.avg_price, 101 * PRICE_SCALE);
}

/// The side mapping is the whole sign of the position delta, so every arm
/// is pinned — a short sale booked as an ordinary sell is the same
/// direction, but a buy booked as a sell is twice the fill in the wrong one.
#[test]
fn every_side_maps_to_the_right_position_delta() {
    for (tag54, expected_side, expected_delta) in [
        ("1", Side::Buy, 5),
        ("2", Side::Sell, -5),
        ("5", Side::ShortSell, -5),
    ] {
        let (mut ccp, mut context, shared) = ord_status_test_state();
        ccp.handle_exec_report(
            &untracked_fill(&[(54, tag54)]), b"", &mut context, &shared, &None, "",
        );
        let fills = shared.orders.drain_fills();
        assert_eq!(fills.len(), 1, "Side={tag54} books");
        assert_eq!(fills[0].0.side, expected_side, "Side={tag54}");
        assert_eq!(
            context.position(fills[0].0.instrument), expected_delta as f64,
            "Side={tag54} moves the position {expected_delta}",
        );
    }
}

/// Deduplication exists to stop a fill being counted twice. Returning out
/// of the whole handler also skips the status and the terminal bookkeeping,
/// so a replayed final fill leaves the order in `open_orders` for good and
/// `req_open_orders` keeps reporting a filled order as working.
#[test]
fn a_duplicate_exec_id_suppresses_the_fill_and_nothing_else() {
    let (mut ccp, mut context, shared) = ord_status_test_state();
    let (event_tx, event_rx) = std::sync::mpsc::sync_channel(4096);
    let event_tx = Some(crate::engine::hot_loop::EventSink::new(event_tx, Default::default()));

    // Partial fill, booked normally.
    ccp.handle_exec_report(
        &exec_report_frame(&[
            (150, "2"), (39, "1"), (32, "1"), (31, "100.00"), (151, "9"), (17, "DUP-1"),
        ]), b"",
        &mut context, &shared, &event_tx, "",
    );
    assert_eq!(shared.orders.drain_fills().len(), 1, "the first delivery books");
    assert!(context.order(42).is_some(), "and the order is still working");

    // The same execution replayed, this time carrying the terminal status.
    ccp.handle_exec_report(
        &exec_report_frame(&[
            (150, "2"), (39, "2"), (32, "1"), (31, "100.00"), (151, "0"), (17, "DUP-1"),
        ]), b"",
        &mut context, &shared, &event_tx, "",
    );

    assert!(
        shared.orders.drain_fills().is_empty(),
        "the fill is not counted twice",
    );
    let position_after = context.position(0);
    assert!(
        context.order(42).is_none(),
        "but the order still reaches its terminal state and is removed",
    );
    let completed = shared.orders.drain_completed_orders();
    assert_eq!(completed.len(), 1, "and is reported completed");
    assert_eq!(completed[0].order_id, 42);
    assert_eq!(completed[0].status, crate::types::OrderStatus::Filled);

    // The terminal status still reaches the application. Treating the
    // duplicate as though it had booked a fill would swallow it, since the
    // status notification is suppressed when a fill was reported instead.
    let updates = shared.orders.drain_order_updates();
    assert_eq!(updates.len(), 1, "exactly one status notification, not none and not two");
    assert_eq!(updates[0].order_id, 42);
    assert_eq!(updates[0].status, crate::types::OrderStatus::Filled);

    // The position is what deduplication exists to protect. One share was
    // filled; the replay must not make it two.
    assert_eq!(position_after, 1.0, "the duplicate must not move the position again");
    assert_eq!(updates[0].filled_qty, 1.0, "nor inflate the filled quantity");
    assert_eq!(completed[0].filled_qty, QTY_SCALE);

    // The event channel is a second delivery path for the same fill, and
    // every other test here passes None for it, so it is checked once.
    let events: Vec<_> = event_rx.try_iter().collect();
    assert_eq!(
        events.iter().filter(|e| matches!(e, Event::Fill(_))).count(), 1,
        "exactly one Fill reaches the channel across both deliveries: {events:?}",
    );
}

/// The report restates the order, and a caller asking what its orders are
/// is answered from that. An order that came back naming neither the
/// reference the caller gave it nor who entered it is not the order they
/// placed.
#[test]
fn the_order_a_report_restates_carries_what_the_caller_gave_it() {
    let (mut ccp, mut context, shared) = ord_status_test_state();
    let mut frame = exec_report_frame(&[
        (150, "2"), (39, "2"), (32, "1"), (31, "100.00"), (151, "0"), (17, "E1"),
        (6008, "756733"), (55, "SPY"),
    ]);
    frame.insert(6010, "my-strategy".to_string());
    frame.insert(47, "A".to_string());
    frame.insert(432, "20260401-16:00:00".to_string());
    frame.insert(109, "the desk".to_string());
    frame.insert(6160, "GROUP1".to_string());
    frame.insert(6159, "PctChange".to_string());
    frame.insert(6164, "25".to_string());

    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");

    let info = shared.orders.get_order_info(42).expect("the order is recorded");
    assert_eq!(info.order.order_ref, "my-strategy", "the caller's own name for it");
    // And on the execution, which is where a program matching its fills by
    // that name reads it. Blank there, it matched none of them.
    assert_eq!(
        info.last_exec.order_ref, "my-strategy",
        "the report restates it on every fill, so the fill carries it too",
    );
    assert_eq!(info.order.rule80a, "A");
    assert_eq!(info.order.good_till_date, "20260401-16:00:00");
    assert_eq!(info.order.submitter, "the desk");
    assert_eq!(info.order.fa_group, "GROUP1");
    assert_eq!(info.order.fa_method, "PctChange");
    assert_eq!(info.order.fa_percentage, "25");
}

/// A broker liquidating a position says so by naming the order with a
/// leading L, not by setting a field. Read as a field it was never set, and
/// a caller could not tell a forced liquidation from any other fill.
#[test]
fn a_liquidation_is_told_apart_from_an_ordinary_fill() {
    let (mut ccp, mut context, shared) = ord_status_test_state();
    let mut frame = exec_report_frame(&[
        (150, "2"), (39, "2"), (32, "1"), (31, "100.00"), (151, "0"), (17, "E1"),
        (6008, "756733"), (55, "SPY"),
    ]);
    frame.insert(11, "L42".to_string());
    frame.insert(6858, "AVG_LEG_CLOSE_DIFF".to_string());
    frame.insert(6859, "2.5".to_string());
    frame.insert(8497, "1".to_string());

    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");

    let info = shared.orders.get_order_info(42).expect("the order is recorded");
    assert_eq!(info.last_exec.liquidation, 1, "the broker liquidated this");
    assert_eq!(info.last_exec.ev_rule, "AVG_LEG_CLOSE_DIFF");
    assert_eq!(info.last_exec.ev_multiplier, 2.5, "the multiplier is the number beside the rule");
    // Read off the text tag, it parsed to nothing and every fill carried a
    // multiplier of zero.
    assert_ne!(info.last_exec.ev_multiplier, 0.0);
    assert!(info.last_exec.pending_price_revision, "the price may still be revised");
}

/// An ordinary fill is not a liquidation, and states none of the rest.
#[test]
fn an_ordinary_fill_claims_none_of_it() {
    let (mut ccp, mut context, shared) = ord_status_test_state();
    let frame = exec_report_frame(&[
        (150, "2"), (39, "2"), (32, "1"), (31, "100.00"), (151, "0"), (17, "E1"),
        (6008, "756733"), (55, "SPY"),
    ]);
    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");

    let info = shared.orders.get_order_info(42).expect("the order is recorded");
    assert_eq!(info.last_exec.liquidation, 0);
    assert!(info.last_exec.ev_rule.is_empty());
    assert!(!info.last_exec.pending_price_revision);
}

/// A late duplicate of an earlier partial must not put a finished order
/// back on the open list. The cache is what `req_open_orders` reads.
#[test]
fn a_late_partial_does_not_reopen_a_completed_order() {
    let (mut ccp, mut context, shared) = ord_status_test_state();
    let partial = |exec: &str| exec_report_frame(&[
        (150, "2"), (39, "1"), (32, "1"), (31, "100.00"), (151, "9"), (17, exec),
        (6008, "756733"), (55, "SPY"),
    ]);

    ccp.handle_exec_report(&partial("E1"), b"", &mut context, &shared, &None, "");
    ccp.handle_exec_report(
        &exec_report_frame(&[
            (150, "2"), (39, "2"), (32, "9"), (31, "100.00"), (151, "0"), (17, "E2"),
            (6008, "756733"), (55, "SPY"),
        ]), b"",
        &mut context, &shared, &None, "",
    );
    let terminal = shared.orders.get_order_info(42).map(|i| i.order_state.status.clone());

    // The earlier partial arrives again.
    ccp.handle_exec_report(&partial("E1"), b"", &mut context, &shared, &None, "");

    assert_eq!(
        shared.orders.get_order_info(42).map(|i| i.order_state.status.clone()),
        terminal,
        "the completed order stays completed",
    );
}

/// A terminal report the venue resends is the one it already sent. The
/// order was retired when it finished, so the replay finds nothing tracked
/// and files the completion again — with no contract and nothing filled,
/// contradicting the terminal status the caller was already given. The
/// completion is remembered from the first time; the replay adds nothing.
#[test]
fn a_replayed_terminal_report_does_not_file_the_completion_again() {
    let (mut ccp, mut context, shared) = ord_status_test_state();
    ccp.handle_exec_report(
        &exec_report_frame(&[
            (150, "2"), (39, "2"), (32, "100"), (31, "100.00"), (151, "0"), (17, "E1"),
        ]), b"",
        &mut context, &shared, &None, "",
    );
    let first = shared.orders.drain_completed_orders();
    assert_eq!(first.len(), 1, "the fill finishes the order");
    assert_eq!(first[0].filled_qty, 100 * QTY_SCALE, "filed with what it filled");

    // The venue resends the same report, marked as a resend.
    ccp.handle_exec_report(
        &exec_report_frame(&[
            (150, "2"), (39, "2"), (32, "100"), (31, "100.00"), (151, "0"), (17, "E1"),
            (97, "Y"),
        ]), b"",
        &mut context, &shared, &None, "",
    );
    assert!(
        shared.orders.drain_completed_orders().is_empty(),
        "the completion is filed once, not once per delivery of it",
    );
}

// A fill with no ExecID is deduped on its content instead, and the key
// includes CumQty, which advances with every execution on an order. Two
// real fills therefore never collide. The case that asserted the opposite
// sent one frame twice with no CumQty tag at all, so both read as zero: a
// shape the gateway does not produce, and treating it as two fills would
// give back the replay double-booking the content key exists to stop.

/// The gateway's answer to a symbol it cannot resolve: a `35=d` echoing
/// the request id with no contract fields (live: "BRK.A").
fn secdef_no_match(req_id: &str, response_type: &str) -> Vec<u8> {
    crate::protocol::fix::fix_build(&[
        (crate::protocol::fix::TAG_MSG_TYPE, "d"),
        (crate::control::contracts::TAG_SECURITY_REQ_ID, req_id),
        (crate::control::contracts::TAG_SECURITY_RESPONSE_TYPE, response_type),
    ], 1)
}

#[test]
fn secdef_no_match_reports_error_200_not_a_zeroed_row() {
    let (mut ccp, mut context, shared) = u186_test_state();
    // By-symbol lookup: not single-shot.
    ccp.pending_secdef.push((1005, false, Instant::now() + SECDEF_TIMEOUT));

    let msg = secdef_no_match("1005", "6");
    ccp.process_ccp_message(&msg, &mut None, &mut context, &shared, &None, &mut HeartbeatState::new(), "DU1");

    assert!(shared.reference.drain_contract_details().is_empty(),
        "a contract-less reply must not surface as a ContractDetails row");
    let errors = shared.reference.drain_historical_errors();
    assert_eq!(errors.len(), 1, "the caller must be told the symbol did not resolve");
    assert_eq!(errors[0].0, 1005);
    assert_eq!(errors[0].1, 200);
    assert_eq!(shared.reference.drain_contract_details_end(), vec![1005]);
    assert!(ccp.pending_secdef.is_empty(), "the request must not outlive its answer");
    }

/// Same reply without the 323 terminator: the by-symbol path reached end
/// through the fan-out branch instead, and must not fire end twice.
#[test]
fn secdef_no_match_without_terminator_ends_once() {
    let (mut ccp, mut context, shared) = u186_test_state();
    ccp.pending_secdef.push((1005, false, Instant::now() + SECDEF_TIMEOUT));

    let msg = secdef_no_match("1005", "4");
    ccp.process_ccp_message(&msg, &mut None, &mut context, &shared, &None, &mut HeartbeatState::new(), "DU1");

    assert!(shared.reference.drain_contract_details().is_empty());
    assert_eq!(shared.reference.drain_historical_errors().len(), 1);
    assert_eq!(shared.reference.drain_contract_details_end(), vec![1005]);
    assert!(ccp.pending_secdef.is_empty());
}

#[test]
fn secdef_no_match_on_internal_req_id_stays_silent() {
    let (mut ccp, mut context, shared) = u186_test_state();
    ccp.pending_secdef.push((0xF000_0001, true, Instant::now() + SECDEF_TIMEOUT));

    let msg = secdef_no_match("4026531841", "6"); // 0xF0000001
    ccp.process_ccp_message(&msg, &mut None, &mut context, &shared, &None, &mut HeartbeatState::new(), "DU1");

    assert!(shared.reference.drain_contract_details().is_empty());
    assert!(shared.reference.drain_historical_errors().is_empty());
    assert!(shared.reference.drain_contract_details_end().is_empty());
    assert!(ccp.pending_secdef.is_empty());
}

/// The venue's answer, verbatim from a live session. Nothing read it,
/// so a caller could not know which algorithms the account may use.
#[test]
fn the_venue_states_which_algorithms_it_offers() {
    let offered = super::parse_algorithms(
        "FOXRIVER/STK:FOXRIVER-AE,FOXRIVER-AL-COMMON;IBALGO/BAG:IBALGO-AE,IBALGO-AL-BAG;\
         IBALGO/CASH:IBALGO-AE,IBALGO-AL-CASH;IBALGO/OPT:IBALGO-AE,IBALGO-AL-OPT"
    );
    assert_eq!(offered.len(), 4, "one entry per provider and security type: {offered:?}");
    assert_eq!(offered["FOXRIVER/STK"], ["FOXRIVER-AE", "FOXRIVER-AL-COMMON"]);
    assert_eq!(offered["IBALGO/OPT"], ["IBALGO-AE", "IBALGO-AL-OPT"]);
    assert!(super::parse_algorithms("").is_empty(), "a session that offered none");
    assert!(
        super::parse_algorithms("NOCOLON").is_empty(),
        "an entry naming no algorithms states nothing",
    );
}

/// The order defaults the account holds are read, not discarded.
///
/// This session asks for them at logon and threw the answer away. The venue
/// keeps a set of order defaults per security type and fills parts of an order
/// the caller left unstated from them, so which sets exist is a fact about
/// every order placed from here.
///
/// The answer repeats three fields per set, so it is read by walking the tags
/// in order. Read by looking each up, five sets would answer as one.
#[test]
fn the_order_defaults_the_account_holds_are_read() {
    let msg = crate::protocol::fix::fix_build(
        &[
            (35, "U"), (6040, "194"), (6556, "OPR.2"), (8166, "L"), (8176, "1"),
            (8167, "3"),
            (8168, "s=CASH"), (8169, "v=1&a=1"), (8170, "1782492079.182"),
            (8168, "s=FUT"), (8169, "v=1&a=1"), (8170, "1782488506.813"),
            (8168, "s=STK"), (8169, "v=2"), (8170, "1782488429.956"),
        ],
        1,
    );
    let held = super::parse_order_presets(&msg);
    assert_eq!(
        held,
        Some(vec![
            ("s=CASH".to_string(), "v=1&a=1".to_string()),
            ("s=FUT".to_string(), "v=1&a=1".to_string()),
            ("s=STK".to_string(), "v=2".to_string()),
        ]),
        "every set, in the order the venue states them",
    );

    // An account that holds none says so, and that is an answer.
    let none = crate::protocol::fix::fix_build(
        &[(35, "U"), (6040, "194"), (6556, "OPR.2"), (8166, "L"), (8167, "0")],
        1,
    );
    assert_eq!(super::parse_order_presets(&none), Some(Vec::new()), "none is a number of sets");

    // And where the venue's own count and what arrived disagree, the message
    // did not arrive whole: published anyway, however many pairs happened to
    // parse would stand as the account's defaults beside a count saying there
    // were more.
    let short = crate::protocol::fix::fix_build(
        &[
            (35, "U"), (6040, "194"), (6556, "OPR.2"), (8166, "L"),
            (8167, "3"),
            (8168, "s=CASH"), (8169, "v=1&a=1"), (8170, "1782492079.182"),
        ],
        1,
    );
    assert_eq!(super::parse_order_presets(&short), None, "one of three is not the answer");

    // And a message that states no count at all proves nothing whole. Read as
    // an answer, one carrying neither count nor pairs cleared the sets this
    // account holds as though the venue had said it holds none.
    let countless = crate::protocol::fix::fix_build(
        &[(35, "U"), (6040, "194"), (6556, "OPR.2"), (8166, "L")],
        1,
    );
    assert_eq!(super::parse_order_presets(&countless), None, "no count is not a count of none");
}

/// A message nobody has looked at and a message deliberately not read are
/// both discarded, but only one is a gap.
#[test]
fn a_message_not_read_on_purpose_is_told_apart_from_one_overlooked() {
    // 93 is excused on what it carries — the account, a request id and two
    // flags — read off a live session, not on an assumption about it.
    // It arrives on a real session, which is how it came to be named here:
    // the notes had it down as never sent.
    assert!(super::known_unread("18").is_none(), "the venue's clock is read, not excused");
    assert!(super::known_unread("93").is_some(), "an answer carrying nothing new");
    // The order presets were excused as a user interface's defaults. The venue
    // fills two fields of a pegged-best order from them where the caller
    // states neither, so an order from here is not the same order — unread and
    // counted as the gap that is, rather than excused.
    assert!(super::known_unread("194").is_none(), "the presets reach an order, so not excused");
    assert!(super::known_unread("81").is_none(), "the algorithms are read, not excused");
    // Excused for years as a fill already stated by the execution reports.
    // The fill is, and what it cost is not: those reports carry no charge at
    // all, so this is the only place a caller's commission comes from.
    assert!(super::known_unread("60").is_none(), "what a fill cost is read, not excused");
    assert!(super::known_unread("99999").is_none(), "and anything unexamined is a gap");
}

/// The venue states trouble as text, with no code and, for all but a
/// narrow family of requests, nothing saying which request it belongs to.
#[test]
fn what_the_venue_says_went_wrong_reaches_the_caller() {
    let (_ccp, _context, shared) = u186_test_state();

    let mut said = std::collections::HashMap::new();
    said.insert(58u32, "Order rejected for margin".to_string());
    super::handle_venue_error(&said, &shared);
    assert_eq!(shared.market.drain_venue_errors(), ["Order rejected for margin"]);

    // Where it names something code-like, that travels with the text
    // rather than being read as a number it never stated.
    said.insert(149u32, "MARGIN".to_string());
    super::handle_venue_error(&said, &shared);
    assert_eq!(shared.market.drain_venue_errors(), ["Order rejected for margin (MARGIN)"]);

    // Trouble it says nothing about leaves nothing to report.
    super::handle_venue_error(&std::collections::HashMap::new(), &shared);
    assert!(shared.market.drain_venue_errors().is_empty());
}

/// What the venue says to the account holder is said on a subtype of its own,
/// about no request. It reached nobody: nothing read it, so the one thing the
/// venue ever addressed to the account was noted as unread wire and dropped.
#[test]
fn what_the_venue_tells_the_account_holder_reaches_the_caller() {
    let (mut ccp, mut context, shared) = u186_test_state();
    let msg = fix::fix_build(&[
        (fix::TAG_MSG_TYPE, "U"), (6040, "42"), (1, "DU1"),
        (58, "Trading in this account is restricted from tomorrow"),
    ], 1);
    ccp.process_ccp_message(
        &msg, &mut None, &mut context, &shared, &None, &mut HeartbeatState::new(), "DU1",
    );
    assert_eq!(
        shared.market.drain_venue_errors(),
        ["Trading in this account is restricted from tomorrow"],
    );
    assert!(
        !shared.market.unread_wire().iter().any(|(_, what)| what == "user message 42"),
        "and it is no longer counted among the messages nothing reads",
    );
}

/// The venue keeps three sets of holdings and this client read one. The
/// others carry the same fields in the same tags, so they are read the
/// same way — and kept apart, because a caller asking what the account
/// holds does not mean what it holds somewhere else.
#[test]
fn a_holding_the_account_does_not_hold_is_kept_apart() {
    let (_ccp, _context, shared) = u186_test_state();
    let mut row = std::collections::HashMap::new();
    row.insert(6008u32, "265598".to_string());
    row.insert(6068u32, "AAPL  ".to_string());
    row.insert(167u32, "STK".to_string());
    row.insert(15u32, "USD".to_string());
    row.insert(6064u32, "100".to_string());
    row.insert(6101u32, "150.0".to_string());

    super::positions::handle_position_elsewhere(&row, &shared, crate::types::HeldElsewhere::Away);
    let held = shared.portfolio.positions_elsewhere();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].con_id, 265598);
    assert_eq!(held[0].symbol, "AAPL", "the venue pads a symbol out");
    assert_eq!(held[0].position, 100.0);
    assert_eq!(held[0].avg_cost, 150 * PRICE_SCALE);
    assert_eq!(held[0].held, crate::types::HeldElsewhere::Away);

    // It stays out of what the account itself holds.
    assert!(shared.portfolio.position_infos().is_empty(), "not one of the account's own");

    // The venue restates a row rather than withdrawing it.
    row.insert(6064u32, "50".to_string());
    super::positions::handle_position_elsewhere(&row, &shared, crate::types::HeldElsewhere::DisplayOnly);
    let held = shared.portfolio.positions_elsewhere();
    assert_eq!(held.len(), 1, "restated, not added again");
    assert_eq!(held[0].position, 50.0);
    assert_eq!(held[0].held, crate::types::HeldElsewhere::DisplayOnly);

    // A row naming no contract names nothing.
    super::positions::handle_position_elsewhere(
        &std::collections::HashMap::new(), &shared, crate::types::HeldElsewhere::Away,
    );
    assert_eq!(shared.portfolio.positions_elsewhere().len(), 1);
}

/// The venue states figures for the sets of holdings the account does not
/// hold itself the same way it states the account's own. Applied to the
/// account's own they would overstate what it is worth.
#[test]
fn figures_for_other_holdings_stay_out_of_the_account() {
    let (_ccp, context, shared) = u186_test_state();
    let msg = b"8=FIX.4.1\x0135=U\x018001=NetLiquidation\x018004=12345.67\x018001=GrossPositionValue\x018004=999.00\x01";

    super::handle_account_update_elsewhere(msg, &shared, crate::types::HeldElsewhere::Away);
    let mut stated = shared.portfolio.values_elsewhere(crate::types::HeldElsewhere::Away);
    stated.sort();
    assert_eq!(stated, [
        ("GrossPositionValue".to_string(), "999.00".to_string(), String::new()),
        ("NetLiquidation".to_string(), "12345.67".to_string(), String::new()),
    ]);
    assert!(
        shared.portfolio.values_elsewhere(crate::types::HeldElsewhere::Aside).is_empty(),
        "one set's figures do not describe another",
        );
    assert_eq!(
        context.account().net_liquidation, 0,
        "and none of it is what the account itself is worth",
    );
}
/// Figures for holdings held elsewhere are read in the layout the venue
/// states a figure in: the name opens a group, and the currency and the value
/// follow inside it, the currency first on every group the account's own
/// figures were measured to carry. Read as value-then-currency, the currency
/// closed the group before its value arrived, and every figure was dropped.
#[test]
fn figures_for_other_holdings_are_read_as_the_venue_lays_them_out() {
    let (_ccp, _context, shared) = u186_test_state();
    let msg = b"8=O\x0135=AL\x016529=AR.1\x018001=AccruedCash\x0115=CHF\x016066=1788679387\x016288=0\x018004=-748.20\x018001=NetLiquidation\x0115=USD\x016066=1788679387\x016288=0\x018004=12345.67\x01";
    super::handle_account_update_elsewhere(msg, &shared, crate::types::HeldElsewhere::Away);
    let mut stated = shared.portfolio.values_elsewhere(crate::types::HeldElsewhere::Away);
    stated.sort();
    assert_eq!(stated, [
        ("AccruedCash".to_string(), "-748.20".to_string(), "CHF".to_string()),
        ("NetLiquidation".to_string(), "12345.67".to_string(), "USD".to_string()),
    ]);
}

/// The maintenance margin is the plain spelling, as its three siblings are.
///
/// The full spelling is a different figure — the two diverge whenever
/// intraday margin relief applies — and it stays reachable by name among the
/// stated values. Written into the maintenance field, the account read the
/// full figure beside a plain initial one.
#[test]
fn the_maintenance_margin_is_the_plain_spelling() {
    let (_ccp, mut context, shared) = u186_test_state();
    let msg = b"8=O\x0135=UM\x018001=MaintMarginReq\x0115=USD\x018004=10.00\x018001=FullMaintMarginReq\x0115=USD\x018004=20.00\x018001=InitMarginReq\x0115=USD\x018004=5.00\x018001=FullInitMarginReq\x0115=USD\x018004=7.00\x01";
    super::positions::handle_account_update(msg, &mut context, &shared);
    let account = context.account();
    assert_eq!(account.maint_margin_req, crate::types::price_from_f64(10.0), "the plain figure");
    assert_eq!(account.init_margin_req, crate::types::price_from_f64(5.0), "as its sibling");
    assert!(
        shared.portfolio.stated_account_values().iter().any(|(k, v, c)| k == "FullMaintMarginReq" && v == "20.00" && c == "USD"),
        "and the full figure stays reachable by name",
    );
}

mod unnamed_execution_tests {

    /// A report carries far more than any one client reads. What is not read
    /// is kept, so a fact the venue stated about a fill remains reachable.
    #[test]
    fn a_field_a_report_states_and_nothing_names_is_kept() {
        let frame = b"35=8\x0117=E1\x0132=100\x019997=something\x019998=42\x01";
        let kept = super::executions::unnamed_execution_fields(frame);
        let tags: Vec<u32> = kept.iter().map(|(t, _)| *t).collect();
        assert!(tags.contains(&9997));
        assert!(tags.contains(&9998));
        assert_eq!(kept.iter().find(|(t, _)| *t == 9997).unwrap().1, "something");
    }

    /// A field the handler reads is read into its own place, not left as a
    /// number, and the message's own fields are not the fill's.
    #[test]
    fn what_is_read_and_what_belongs_to_the_message_are_both_excluded() {
        let frame = b"35=8\x0117=E1\x0152=20260101-00:00:00\x01";
        let tags: Vec<u32> = super::executions::unnamed_execution_fields(frame).iter().map(|(t, _)| *t).collect();
        assert!(!tags.contains(&17), "the execution id is read");
        assert!(!tags.contains(&35), "the message type belongs to the message");
        assert!(!tags.contains(&52), "the sending time belongs to the message");
    }

    /// The handler reads a good many tags, so the derived list is not empty or
    /// tiny — which would make everything look unread.
    #[test]
    fn the_tags_the_handler_reads_are_derived_from_the_handler() {
        let read = &*super::executions::READ_FROM_AN_EXECUTION;
        assert!(read.len() > 30, "only {} tags reported as read", read.len());
        assert!(read.contains(&17), "the execution id is read");
    }
}
mod stated_account_value_tests {
    use crate::bridge::SharedState;

    /// The venue states a great many more figures than any client names, and a
    /// figure nobody named is still a figure about the account. They are kept
    /// where they arrive rather than dropped.
    #[test]
    fn a_figure_nothing_names_is_still_kept() {
        let shared = SharedState::new();
        shared.portfolio.note_account_value("NetLiquidation", "12345.67", "USD");
        shared.portfolio.note_account_value("SomethingNobodyNames", "42", "EUR");

        let stated = shared.portfolio.stated_account_values();
        assert_eq!(stated.len(), 2);
        assert!(stated.iter().any(|(k, v, c)| k == "SomethingNobodyNames" && v == "42" && c == "EUR"));
    }

    /// The same figure in two currencies is two figures. Collapsing them would
    /// report one account's worth in a currency it is not held in.
    #[test]
    fn the_same_figure_in_two_currencies_is_two_figures() {
        let shared = SharedState::new();
        shared.portfolio.note_account_value("TotalCashValue", "100", "USD");
        shared.portfolio.note_account_value("TotalCashValue", "90", "EUR");
        assert_eq!(shared.portfolio.stated_account_values().len(), 2);
    }

    /// A figure restated in the same currency replaces the earlier statement
    /// rather than piling up beside it.
    #[test]
    fn a_figure_restated_replaces_what_it_restates() {
        let shared = SharedState::new();
        shared.portfolio.note_account_value("BuyingPower", "100", "USD");
        shared.portfolio.note_account_value("BuyingPower", "200", "USD");
        let stated = shared.portfolio.stated_account_values();
        assert_eq!(stated.len(), 1);
        assert_eq!(stated[0].1, "200");
    }
}

/// Every holding a position frame names is read, not only the last.
///
/// Captured from a session: one frame, five holdings. A flat parse keeps only
/// the last value of each tag and reports a single holding of zero.
#[test]
fn a_position_frame_names_every_holding() {
    use super::positions::split_position_entries;

    let frame = concat!(
        "35=UP\x016529=AR.1\x01",
        "6068=IWM\x016288=0\x018001=PositionList\x016064=-80\x0115=USD\x016008=9579970\x01",
        "6068=MES SEP2026\x016288=0\x018001=PositionList\x016064=1\x0115=USD\x016008=793356217\x01167=FUT\x01",
        "6068=QQQ\x016288=0\x018001=PositionList\x016064=100\x0115=USD\x016008=320227571\x01",
        "6068=SPY\x016288=0\x018001=PositionList\x016064=342\x0115=USD\x016008=756733\x01",
        "6068=VOD\x016288=0\x018001=PositionList\x016064=0\x0115=GBP\x016008=140148322\x01",
    );

    let held = split_position_entries(frame.as_bytes());
    assert_eq!(held.len(), 5, "five holdings were named, so five are read");

    let by_con_id: Vec<(i64, f64)> = held
        .iter()
        .map(|h| (
            h.get(&6008).unwrap().parse().unwrap(),
            h.get(&6064).unwrap().parse().unwrap(),
        ))
        .collect();
    assert_eq!(
        by_con_id,
        vec![(9579970, -80.0), (793356217, 1.0), (320227571, 100.0),
             (756733, 342.0), (140148322, 0.0)],
    );

    // What the frame says about itself belongs to each holding in it.
    for one in &held {
        assert_eq!(one.get(&6529).map(String::as_str), Some("AR.1"));
        assert_eq!(one.get(&35).map(String::as_str), Some("UP"));
    }

    // And the one that had just traded is present, which a flat parse lost.
    assert!(by_con_id.iter().any(|(con_id, qty)| *con_id == 756733 && *qty == 342.0));

    // A holding describes itself. Read flat, only the last holding's symbol
    // and security type survived, so every other one — a future among them —
    // reached a caller as an id and a quantity and nothing else, and looked
    // like a contract the definition service had refused to name.
    let future = held
        .iter()
        .find(|h| h.get(&6008).map(String::as_str) == Some("793356217"))
        .expect("the future is one of the holdings");
    assert_eq!(future.get(&6068).map(|s| s.trim_end()), Some("MES SEP2026"));
    assert_eq!(future.get(&167).map(String::as_str), Some("FUT"));
    assert_eq!(future.get(&15).map(String::as_str), Some("USD"));
}

/// A frame naming one holding still reads as one.
#[test]
fn a_single_holding_frame_is_unchanged() {
    use super::positions::split_position_entries;

    let frame = "35=UP\x016529=AR.1\x016068=SPY\x016064=342\x016008=756733\x01";
    let held = split_position_entries(frame.as_bytes());
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].get(&6008).map(String::as_str), Some("756733"));
    assert_eq!(held[0].get(&6064).map(String::as_str), Some("342"));
}

/// The conditions the venue states an order under are read back off it.
///
/// Captured from a session: the report for a resting order carrying one price
/// condition. Nothing read these, so an order this session did not place came
/// back stating none — and a program that read one back and placed it again
/// sent an order that went live at once where the original waited for its
/// price.
#[test]
fn an_order_states_the_conditions_it_waits_on() {
    use super::executions::decode_conditions;
    use crate::types::OrderCondition;

    let report = concat!(
        "35=8\x0111=1787087979010000.0\x016136=1\x01",
        "6222=1\x016123=756733\x016169=Invalid\x016168=0\x016166=nan\x016220=0\x01",
        "6124=BEST\x016126=<=\x016125=0.01\x018569=\x016223=\x016246=\x01",
        "6947=\x016245=\x016263=\x016137=n\x016128=0\x016151=0\x01",
    );

    let waits_on = decode_conditions(report.as_bytes());
    assert_eq!(waits_on.len(), 1, "the order states one condition");
    match &waits_on[0] {
        OrderCondition::Price { con_id, exchange, price, is_more, .. } => {
            assert_eq!(*con_id, 756733);
            assert_eq!(exchange, "BEST");
            assert_eq!(*price, crate::types::PRICE_SCALE / 100, "one cent");
            assert!(!*is_more, "`<=` is met below the price, not above it");
        }
        other => panic!("a price condition was stated, not {other:?}"),
    }
}

/// Two conditions on one order are both read.
///
/// They arrive as a group per condition, and a flat parse keeps the last value
/// of each tag — so an order waiting on two came back waiting on one.
#[test]
fn two_conditions_are_both_read() {
    use super::executions::decode_conditions;

    let report = concat!(
        "35=8\x0111=1\x016136=2\x01",
        "6222=1\x016123=756733\x016124=BEST\x016126=<=\x016125=0.01\x016137=a\x01",
        "6222=1\x016123=9579970\x016124=SMART\x016126=>=\x016125=999.00\x016137=n\x01",
    );

    let waits_on = decode_conditions(report.as_bytes());
    assert_eq!(waits_on.len(), 2, "both conditions are read, not only the last");
}

/// A condition reads back the way it was joined.
///
/// `a` joins a condition to the next with AND and `o` with OR. The reference
/// client reads any other spelling as not AND, which is what the last
/// condition's `n` reads as, and an order read back here reads the same, so
/// placed again it is joined the way it was.
#[test]
fn a_condition_reads_back_how_it_is_joined() {
    use super::executions::decode_conditions;

    let report = concat!(
        "35=8\x0111=1\x016136=3\x01",
        "6222=1\x016123=756733\x016124=BEST\x016126=<=\x016125=0.01\x016137=o\x01",
        "6222=1\x016123=9579970\x016124=SMART\x016126=>=\x016125=999.00\x016137=a\x01",
        "6222=1\x016123=9579970\x016124=SMART\x016126=>=\x016125=999.00\x016137=n\x01",
    );
    let joined: Vec<bool> =
        decode_conditions(report.as_bytes()).iter().map(|c| c.is_conjunction_connection()).collect();
    assert_eq!(joined, [false, true, false], "OR, AND, and the last joins nothing");
}

/// A report carrying no conditions states none, rather than one made up.
#[test]
fn an_unconditional_order_states_no_conditions() {
    use super::executions::decode_conditions;

    let report = "35=8\x0111=1\x0139=0\x0155=SPY\x01";
    assert!(decode_conditions(report.as_bytes()).is_empty());
}

/// A condition whose direction the venue states in terms this cannot read is
/// left out, the way every other unreadable field leaves its condition out.
/// Read as "at most" it stated a trigger the venue never described, and an
/// order read back and placed again waited for the opposite of what it had.
#[test]
fn a_condition_with_no_readable_direction_is_left_out() {
    use super::executions::decode_conditions;

    let report = concat!(
        "35=8\x0111=1\x016136=2\x01",
        "6222=1\x016123=756733\x016124=BEST\x016126=!!\x016125=0.01\x016137=a\x01",
        "6222=1\x016123=9579970\x016124=SMART\x016126=>=\x016125=999.00\x016137=n\x01",
    );

    let waits_on = decode_conditions(report.as_bytes());
    assert_eq!(waits_on.len(), 1, "only the condition that read is kept: {waits_on:?}");
    match &waits_on[0] {
        crate::types::OrderCondition::Price { con_id, is_more, .. } => {
            assert_eq!(*con_id, 9579970);
            assert!(*is_more, "the one that read is the `>=` one");
        }
        other => panic!("a price condition was stated, not {other:?}"),
    }
}

/// An order restated by an ordinary report keeps the group it cancels
/// together with. Read only on the recovery record, the first report about a
/// recovered order replaced the cached row with one saying the order stood
/// alone.
#[test]
fn a_restated_order_keeps_the_group_it_cancels_with() {
    let (mut ccp, mut context, shared) = ord_status_test_state();
    let frame = exec_report_frame(&[
        (39, "0"), (150, "0"), (55, "SPY"), (6008, "756733"), (583, "OCA_42"),
    ]);
    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");
    let info = shared.orders.get_order_info(42).expect("the order was restated");
    assert_eq!(info.order.oca_group, "OCA_42", "the group is on the report and was read");
}

/// The venue turning an order down and the venue saying something about one
/// are different things, and a caller classifies on the code.
#[test]
fn a_refused_order_is_reported_under_the_rejection_code() {
    let (mut ccp, mut context, shared) = ord_status_test_state();
    let frame = exec_report_frame(&[
        (39, "8"), (150, "8"), (58, "No trading permissions"),
    ]);
    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");
    let told = shared.orders.drain_order_inactive();
    assert!(
        told.iter().any(|(id, code, _)| *id == 42 && *code == 201),
        "the refusal is reported as one: {told:?}",
    );
}

/// A fan-out ends when every exchange it asked has answered. Counted per
/// frame instead, a leg answered with more than one row completes the request
/// twice over and drops the legs still outstanding.
#[test]
fn a_leg_that_answers_twice_does_not_end_the_fanout() {
    let (mut ccp, mut context, shared) = u186_test_state();
    ccp.pending_fanout.push(PendingFanout {
        api_req_id: 9,
        fanout_req_ids: vec!["ibxfan-9-0".to_string(), "ibxfan-9-1".to_string()],
        answered: Vec::new(),
        deadline: Instant::now() + SECDEF_TIMEOUT,
    });

    for _ in 0..2 {
        ccp.process_ccp_message(&secdef_not_found("ibxfan-9-0"), &mut None, &mut context,
            &shared, &None, &mut HeartbeatState::new(), "DU1");
    }

    assert!(
        shared.reference.drain_contract_details_end().is_empty(),
        "the second exchange has not answered, so the request has not ended",
    );
    assert_eq!(ccp.pending_fanout.len(), 1, "and it is still awaiting that leg");
}

/// A request the transport could not carry, and one the venue never answered,
/// are refused rather than answered empty: an empty answer is a search the
/// venue ran and nothing matched, which is not what happened in either case.
#[test]
fn a_matching_symbols_request_that_goes_nowhere_is_refused() {
    let mut ccp = CcpState::new();
    let mut hb = HeartbeatState::new();
    let shared = SharedState::new();

    let mut no_conn: Option<Connection> = None;
    ccp.send_matching_symbols_request(7, "AAPL", &mut no_conn, &mut hb, &shared);
    assert!(
        shared.reference.drain_matching_symbols().is_empty(),
        "a request that never went out is not a search that found nothing",
    );
    let refused = shared.reference.drain_historical_errors();
    assert_eq!(refused.len(), 1, "the caller is told it was not sent");
    assert_eq!(refused[0].0, 7);

    ccp.pending_matching_symbols.push((8, Instant::now() - Duration::from_secs(1)));
    ccp.sweep_pending_matching_symbols(&shared);
    assert!(
        shared.reference.drain_matching_symbols().is_empty(),
        "and neither is one the venue never answered",
    );
    let refused = shared.reference.drain_historical_errors();
    assert_eq!(refused.len(), 1, "the caller of the unanswered one is told");
    assert_eq!(refused[0].0, 8);
}

/// A bulletin whose urgency names no type here is still a message the venue
/// sent. Dropped in silence it left no callback, no log and nothing to say
/// data had arrived and gone nowhere.
#[test]
fn a_bulletin_with_an_unnamed_urgency_is_recorded_as_unread() {
    let (mut ccp, mut context, shared) = u186_test_state();
    let msg = crate::protocol::fix::fix_build(&[
        (fix::TAG_MSG_TYPE, "B"),
        (fix::TAG_URGENCY, "7"),
        (fix::TAG_HEADLINE, "something the venue said"),
    ], 1);

    ccp.process_ccp_message(&msg, &mut None, &mut context, &shared, &None,
        &mut HeartbeatState::new(), "DU1");

    assert!(
        shared.market.unread_wire().iter().any(|(_, what)| what.contains("urgency 7")),
        "the drop is recorded: {:?}", shared.market.unread_wire(),
    );
}

/// A by-symbol lookup answers with the row that carries the trading hours.
/// The master row waits for its schedule while the per-exchange legs answer
/// for the same contract, and whichever reached the dedup gate first won —
/// so a caller's hours were decided by which reply the venue sent faster.
#[test]
fn a_fanout_leg_does_not_displace_the_row_that_carries_the_hours() {
    let (mut ccp, mut context, shared) = u186_test_state();
    let def = crate::control::contracts::ContractDefinition {
        con_id: 265598,
        symbol: "AAPL".to_string(),
        ..Default::default()
    };
    ccp.pending_schedule_pair.push(PendingSchedulePair {
        api_req_id: 9,
        join_key: "AAPL".to_string(),
        def,
        is_last: true,
        deadline: Instant::now() + Duration::from_secs(3),
    });
    ccp.pending_fanout.push(PendingFanout {
        api_req_id: 9,
        fanout_req_ids: vec!["ibxfan-9-0".to_string()],
        answered: Vec::new(),
        deadline: Instant::now() + SECDEF_TIMEOUT,
    });

    let leg = crate::protocol::fix::fix_build(&[
        (fix::TAG_MSG_TYPE, "d"),
        (crate::control::contracts::TAG_SECURITY_REQ_ID, "ibxfan-9-0"),
        (crate::control::contracts::TAG_SECURITY_RESPONSE_TYPE, "2"),
        (crate::control::contracts::TAG_IB_CON_ID, "265598"),
        (55, "AAPL"),
    ], 1);
    ccp.process_ccp_message(&leg, &mut None, &mut context, &shared, &None,
        &mut HeartbeatState::new(), "DU1");

    assert!(
        shared.reference.drain_contract_details().is_empty(),
        "the row still waiting for its hours is the one that answers",
    );
    assert!(
        shared.reference.drain_contract_details_end().is_empty(),
        "and the end waits for it too",
    );
    assert!(
        ccp.pending_schedule_pair.iter().any(|p| p.api_req_id == 9),
        "the master row is still parked",
    );
}

/// A reconnect has named nothing yet, so the replay flag is cleared. Left set
/// from the previous connection, a caller asking what it has on is answered
/// from the pre-drop book before the new account arrives.
#[test]
fn a_reconnect_waits_for_the_new_account_of_what_is_working() {
    let mut ccp = CcpState::new();
    let shared = SharedState::new();
    let mut hb = HeartbeatState::new();
    ccp.hydrated_any = true;
    shared.orders.set_replay_done();

    let (conn, _peer) = crate::protocol::connection::Connection::for_test();
    let mut ccp_conn: Option<Connection> = None;
    ccp.reconnect(conn, &mut ccp_conn, &mut hb, "DU1", &shared);

    assert!(!shared.orders.replay_done(), "the new connection has named nothing yet");
    assert!(!ccp.hydrated_any, "and nothing has been hydrated from it");
}

/// A reconnect has not stated what the account holds either.
///
/// The flag that says the download finished belongs to the connection that
/// finished it. Left set, `req_positions` is answered at once from the
/// pre-drop snapshot while the venue's own statement is still on its way, so
/// a holding that moved or closed while the connection was down is handed
/// back as though it still stood.
#[test]
fn a_reconnect_waits_for_the_new_statement_of_what_the_account_holds() {
    let mut ccp = CcpState::new();
    let shared = SharedState::new();
    let mut hb = HeartbeatState::new();
    shared.portfolio.account_download_is_settled();

    let (conn, _peer) = crate::protocol::connection::Connection::for_test();
    let mut ccp_conn: Option<Connection> = None;
    ccp.reconnect(conn, &mut ccp_conn, &mut hb, "DU1", &shared);

    assert!(
        !shared.portfolio.account_download_complete(),
        "the new connection has stated nothing about the account yet",
    );
}

/// The venue says why it would not cancel an order, and the structured
/// rejection carries two numbers and no text. The reason went to a log no
/// caller reads, where "the order does not exist" and "it is too late" look
/// the same.
#[test]
fn a_refused_cancel_carries_the_reason_the_venue_gave() {
    let (mut ccp, mut context, shared) = ord_status_test_state();
    let mut frame = std::collections::HashMap::new();
    frame.insert(41u32, "42".to_string());
    frame.insert(434u32, "1".to_string());
    frame.insert(102u32, "0".to_string());
    frame.insert(58u32, "Too late to cancel".to_string());

    ccp.handle_cancel_reject(&frame, &mut context, &shared, &None);

    let told = shared.orders.drain_order_inactive();
    assert!(
        told.iter().any(|(id, _, text)| *id == 42 && text == "Too late to cancel"),
        "the caller is told what the venue said: {told:?}",
    );
}

/// Whether the venue manages an order's price for it is a field of its own,
/// beside the algo rather than part of it. Read off the algo, an adaptive
/// order gained price management it may not have and every other order lost
/// it.
#[test]
fn price_management_is_read_from_its_own_field() {
    for (adaptive, stated, wanted) in [
        ("Adaptive", None, None),
        ("Adaptive", Some("1"), Some(1)),
        ("", Some("1"), Some(1)),
        ("", None, None),
    ] {
        let (mut ccp, mut context, shared) = ord_status_test_state();
        let mut pairs = vec![("39", "0"), ("150", "0"), ("55", "SPY")];
        if !adaptive.is_empty() {
            pairs.push(("847", adaptive));
        }
        if let Some(v) = stated {
            pairs.push(("8339", v));
        }
        let frame = exec_report_frame(
            &pairs.iter().map(|(t, v)| (t.parse().unwrap(), *v)).collect::<Vec<_>>(),
        );
        ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");
        let info = shared.orders.get_order_info(42).expect("the order was restated");
        assert_eq!(
            info.order.use_price_mgmt_algo, wanted,
            "847={adaptive:?} 8339={stated:?}",
        );
    }
}

/// A lookup named by an identifier this client carries no wire source for is
/// refused. Asked by symbol instead, it answered a different question than
/// the one the caller put, under the caller's own number — whatever the
/// symbol matched.
#[test]
fn an_identifier_this_client_cannot_carry_refuses_the_lookup() {
    let mut ccp = CcpState::new();
    let shared = SharedState::new();
    let mut hb = HeartbeatState::new();
    let mut no_conn: Option<Connection> = None;
    let filters = crate::types::SecDefFilters {
        sec_id: "B04KRF9".to_string(),
        sec_id_type: "SEDOL".to_string(),
        ..Default::default()
    };
    let reason = ccp.send_secdef_request_by_symbol(
        9, "AAPL", "STK", "SMART", "USD", &filters, &mut no_conn, &mut hb, &shared,
    ).expect_err("a kind this client cannot carry is not looked up by symbol");
    assert!(reason.contains("SEDOL"), "the refusal names the kind: {reason}");
    assert!(ccp.pending_secdef.is_empty(), "nothing was queued to be answered");
}

/// Each public identifier rides the tags the venue reads it on. A CUSIP was
/// going out as `22=1|48=<id>`, which is the pair an ISIN uses, and a FIGI was
/// not going out at all — the lookup fell through to the symbol and answered
/// with whatever that matched.
#[test]
fn a_public_identifier_rides_the_tags_its_own_kind_uses() {
    use std::io::Read;
    for (kind, id, wanted, unwanted) in [
        ("CUSIP", "037833100", vec!["454=1", "455=037833100", "456=1"], vec!["22=", "48="]),
        ("ISIN", "US0378331005", vec!["22=4", "48=US0378331005"], vec!["454=", "455="]),
        ("FIGI", "BBG000B9XRY4", vec!["22=S", "48=BBG000B9XRY4"], vec!["454=", "455="]),
    ] {
        let (conn, mut peer) = crate::protocol::connection::Connection::for_test();
        let mut ccp = CcpState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let mut conn = Some(conn);
        let filters = crate::types::SecDefFilters {
            sec_id: id.to_string(),
            sec_id_type: kind.to_string(),
            ..Default::default()
        };
        let sent = ccp.send_secdef_request_by_symbol(
            9, "AAPL", "STK", "SMART", "USD", &filters, &mut conn, &mut hb, &shared,
        );
        sent.expect("a CUSIP lookup is one this client can ask");

        let mut buf = [0u8; 4096];
        let n = peer.read(&mut buf).unwrap();
        let msg = String::from_utf8_lossy(&buf[..n]).replace('\u{1}', "|");
        for field in wanted {
            assert!(msg.contains(&format!("|{field}|")), "{kind} states {field}: {msg}");
        }
        for field in unwanted {
            assert!(!msg.contains(&format!("|{field}")), "{kind} does not state {field}: {msg}");
        }
        // The identifier replaces the symbol, and asking by both is asking a
        // different question from the one the caller put.
        assert!(!msg.contains("|55=AAPL|"), "{kind} does not also ask by symbol: {msg}");
    }
}

/// A kind of identifier this client carries no source for is refused rather
/// than asked for by symbol: that is a different question, and its answer
/// would read as the one the caller put.
#[test]
fn an_identifier_of_an_unknown_kind_is_refused_not_asked_by_symbol() {
    use std::io::Read;
    let (conn, mut peer) = crate::protocol::connection::Connection::for_test();
    let mut ccp = CcpState::new();
    let shared = SharedState::new();
    let mut hb = HeartbeatState::new();
    let mut conn = Some(conn);
    let filters = crate::types::SecDefFilters {
        sec_id: "XS1234567890".to_string(),
        sec_id_type: "SEDOL".to_string(),
        ..Default::default()
    };
    let why = ccp.send_secdef_request_by_symbol(
        16, "AAPL", "STK", "SMART", "USD", &filters, &mut conn, &mut hb, &shared,
    ).expect_err("the lookup is refused, and says what it could not ask");
    assert!(why.contains("SEDOL"), "the refusal names what it could not carry: {why}");
    assert!(ccp.pending_secdef.is_empty(), "and nothing is queued for an answer");
    peer.set_nonblocking(true).unwrap();
    let mut buf = [0u8; 4096];
    assert!(peer.read(&mut buf).is_err(), "and nothing reached the wire");
}

/// A lookup states the symbol and the venue's local symbol as two separate
/// fields, because they are two separate statements about the contract. Sending
/// only the local symbol asked a narrower question than the caller put, and a
/// symbol that disagrees with it — which the venue would refuse — matched
/// whatever the local symbol named.
#[test]
fn a_lookup_states_both_the_symbol_and_the_local_symbol() {
    use std::io::Read;
    let (conn, mut peer) = crate::protocol::connection::Connection::for_test();
    let mut ccp = CcpState::new();
    let shared = SharedState::new();
    let mut hb = HeartbeatState::new();
    let mut conn = Some(conn);
    let filters = crate::types::SecDefFilters {
        local_symbol: "ESZ6".to_string(),
        ..Default::default()
    };
    ccp.send_secdef_request_by_symbol(
        11, "ES", "FUT", "CME", "USD", &filters, &mut conn, &mut hb, &shared,
    ).expect("a symbol lookup is one this client can ask");

    let mut buf = [0u8; 4096];
    let n = peer.read(&mut buf).unwrap();
    let msg = String::from_utf8_lossy(&buf[..n]).replace('\u{1}', "|");
    assert!(msg.contains("|55=ES|"), "the symbol: {msg}");
    assert!(msg.contains("|6035=ESZ6|"), "and the contract's own name: {msg}");
}

/// A news feed states its provider where every other contract states a venue,
/// and the protocol carries the provider under a field of its own. Sent as a
/// venue instead, the whole message was refused: the venue answered that the
/// field the provider rides was missing.
#[test]
fn a_news_feed_states_its_provider_not_a_venue() {
    use std::io::Read;
    // The exchange names the provider alone on one feed and provider and feed
    // together on another; the feed is the half the provider field wants.
    for (exchange, wanted) in [("BRFG", "BRFG"), ("BRF", "BRF"), ("BZ:BZ_ALL", "BZ_ALL")] {
        let (conn, mut peer) = crate::protocol::connection::Connection::for_test();
        let mut ccp = CcpState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let mut conn = Some(conn);
        let filters = crate::types::SecDefFilters {
            trading_class: "BRF".to_string(),
            ..Default::default()
        };
        ccp.send_secdef_request_by_symbol(
            13, "BRF:BRF_ALL", "NEWS", exchange, "USD", &filters, &mut conn, &mut hb, &shared,
        ).expect("a news lookup is one this client can ask");

        let mut buf = [0u8; 4096];
        let n = peer.read(&mut buf).unwrap();
        let msg = String::from_utf8_lossy(&buf[..n]).replace('\u{1}', "|");
        assert!(msg.contains(&format!("|6825={wanted}|")), "the provider: {msg}");
        // The provider having moved, no venue and no currency ride with it.
        assert!(!msg.contains("|100="), "and no venue: {msg}");
        assert!(!msg.contains("|15="), "and nothing it is priced in: {msg}");
        // A feed has no class within a chain, so one stated on it narrows
        // nothing and is not passed on.
        assert!(!msg.contains("|6058="), "and no class: {msg}");
    }
}

/// The protocol has no security type of its own for a continuous future. It is
/// asked for as a future with a field saying the current lead month is wanted
/// rather than a listed one; sent under its own name the message was refused
/// outright. Its class rides a field of its own, because it is not being looked
/// up by a listed month.
#[test]
fn a_continuous_future_is_a_future_that_names_its_lead_month() {
    use std::io::Read;
    for stated in ["CONTFUT", "FUT+CONTFUT"] {
        let (conn, mut peer) = crate::protocol::connection::Connection::for_test();
        let mut ccp = CcpState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let mut conn = Some(conn);
        let filters = crate::types::SecDefFilters {
            trading_class: "GBL".to_string(),
            local_symbol: "FGBL DEC 26".to_string(),
            ..Default::default()
        };
        ccp.send_secdef_request_by_symbol(
            14, "GBL", stated, "EUREX", "EUR", &filters, &mut conn, &mut hb, &shared,
        ).expect("a continuous future lookup is one this client can ask");

        let mut buf = [0u8; 4096];
        let n = peer.read(&mut buf).unwrap();
        let msg = String::from_utf8_lossy(&buf[..n]).replace('\u{1}', "|");
        assert!(msg.contains("|167=FUT|"), "{stated} is asked for as a future: {msg}");
        assert!(msg.contains("|6857=2|"), "{stated} states the lead month: {msg}");
        assert!(msg.contains("|8362=GBL|"), "{stated} states its class: {msg}");
        assert!(!msg.contains("|6058="), "{stated} states it once: {msg}");
        // It is not being asked for by a listed month, so the name one listed
        // month goes by does not narrow it.
        assert!(!msg.contains("|6035="), "{stated} names no listed month: {msg}");
    }
}

/// A contract named only by its issuer is answered as fixed income, whatever
/// type the caller stated. Sent with the caller's own type the message was
/// refused: the type a contract is looked up under is not the caller's to
/// choose here.
#[test]
fn a_contract_named_by_its_issuer_is_asked_for_as_fixed_income() {
    use std::io::Read;
    let (conn, mut peer) = crate::protocol::connection::Connection::for_test();
    let mut ccp = CcpState::new();
    let shared = SharedState::new();
    let mut hb = HeartbeatState::new();
    let mut conn = Some(conn);
    let filters = crate::types::SecDefFilters {
        issuer_id: "e1453318".to_string(),
        ..Default::default()
    };
    ccp.send_secdef_request_by_symbol(15, "", "", "", "", &filters, &mut conn, &mut hb, &shared)
        .expect("an issuer lookup is one this client can ask");

    let mut buf = [0u8; 4096];
    let n = peer.read(&mut buf).unwrap();
    let msg = String::from_utf8_lossy(&buf[..n]).replace('\u{1}', "|");
    assert!(msg.contains("|6454=e1453318|"), "the issuer: {msg}");
    assert!(msg.contains("|167=FIXED|"), "asked for as fixed income: {msg}");
}

/// A contract fetched without a caller asking is remembered so it is not
/// fetched twice. The record is dropped when the fetch times out, or one lost
/// request leaves that contract unnamed for the life of the session.
#[test]
fn a_fetch_that_is_never_answered_is_asked_again() {
    let (conn, _peer) = crate::protocol::connection::Connection::for_test();
    let mut ccp = CcpState::new();
    let mut hb = HeartbeatState::new();
    let shared = SharedState::new();
    let mut conn = Some(conn);

    ccp.auto_fetch_secdef_if_cold(756733, &mut conn, &shared, &mut hb);
    assert_eq!(ccp.pending_secdef.len(), 1, "the fetch went out");
    assert!(ccp.auto_fetched_conids.contains_key(&756733), "and is remembered while it is out");

    // Nothing answers it.
    for entry in &mut ccp.pending_secdef {
        entry.2 = Instant::now() - std::time::Duration::from_secs(1);
    }
    ccp.sweep_contract_details(&shared, &None);
    assert!(
        !ccp.auto_fetched_conids.contains_key(&756733),
        "a fetch that never came back is forgotten, so the next report asks again",
    );

    ccp.auto_fetch_secdef_if_cold(756733, &mut conn, &shared, &mut hb);
    assert_eq!(ccp.pending_secdef.len(), 1, "and it is asked again");
}

/// One frame names every holding in it, and the sets the account does not hold
/// itself are no different from its own. Handed the flat map instead, the
/// generic parser kept the last value of each repeated tag, so a frame naming
/// three holdings arrived as one and the other two were gone before anything
/// could see them.
#[test]
fn an_away_position_frame_names_every_holding_in_it() {
    let (mut ccp, mut context, shared) = u186_test_state();
    let mut conn: Option<Connection> = None;
    let mut hb = HeartbeatState::new();
    let mut fields: Vec<(u32, &str)> = vec![
        (crate::protocol::fix::TAG_MSG_TYPE, "AP"),
    ];
    for (symbol, con_id, qty, cost) in [
        ("AAPL  ", "265598", "100", "150.0"),
        ("MSFT  ", "272093", "25", "300.0"),
        ("SPY   ", "756733", "7", "500.0"),
    ] {
        fields.push((6068, symbol));
        fields.push((6008, con_id));
        fields.push((167, "STK"));
        fields.push((15, "USD"));
        fields.push((6064, qty));
        fields.push((6101, cost));
    }
    let frame = crate::protocol::fix::fix_build(&fields, 1);
    ccp.process_ccp_message(&frame, &mut conn, &mut context, &shared, &None, &mut hb, "DU1");

    let mut held = shared.portfolio.positions_elsewhere();
    held.sort_by_key(|row| row.con_id);
    assert_eq!(held.len(), 3, "every holding the frame names: {held:?}");
    assert_eq!(held[0].symbol, "AAPL", "the venue pads a symbol out");
    assert_eq!(held[0].position, 100.0);
    assert_eq!(held[1].symbol, "MSFT");
    assert_eq!(held[1].avg_cost, 300 * PRICE_SCALE);
    assert_eq!(held[2].position, 7.0, "and the last one is not the only one");
    assert!(
        held.iter().all(|row| row.held == crate::types::HeldElsewhere::Away),
        "and all of them in the set the frame belongs to: {held:?}",
    );
}

/// A frame restating a holding without its cost was replacing a real basis
/// with nothing. The account's own holdings already keep theirs.
#[test]
fn a_holding_elsewhere_keeps_the_basis_a_later_frame_leaves_out() {
    let (_ccp, _context, shared) = u186_test_state();
    let mut row = std::collections::HashMap::new();
    row.insert(6008u32, "265598".to_string());
    row.insert(6064u32, "100".to_string());
    row.insert(6101u32, "150.0".to_string());
    super::positions::handle_position_elsewhere(&row, &shared, crate::types::HeldElsewhere::Away);

    row.remove(&6101);
    row.insert(6064u32, "120".to_string());
    super::positions::handle_position_elsewhere(&row, &shared, crate::types::HeldElsewhere::Away);

    let held = shared.portfolio.positions_elsewhere();
    assert_eq!(held[0].position, 120.0, "the new quantity");
    assert_eq!(held[0].avg_cost, 150 * PRICE_SCALE, "and the basis it already had");
}

/// Subscribing to account updates asks the venue for the figures.
///
/// The venue restates them on its own schedule. Measured against a live
/// session: a subscription alone is answered after 39 seconds, and the same
/// subscription with this request after 750 milliseconds.
#[test]
fn an_account_refresh_asks_for_the_figures() {
    use std::io::Read;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    let mut conn = Some(crate::protocol::connection::Connection::new_raw(stream).unwrap());
    let mut ccp = CcpState::new();
    let mut hb = HeartbeatState::new();
    let mut buf = [0u8; 4096];

    ccp.send_account_refresh("DU123456", &mut conn, &mut hb, &SharedState::new());

    let n = peer.read(&mut buf).unwrap();
    let text = String::from_utf8_lossy(&buf[..n]).replace('\u{1}', "|");
    let key_of = |t: &str| -> String {
        t.split('|').find(|f| f.starts_with("6529=")).unwrap_or("").to_string()
    };

    // The display request carries the positions beside the figures.
    assert!(text.contains("|6040=91|"), "the display request is stated: {text}");
    assert!(text.contains("|6556=DR.1|"), "under its own key: {text}");
    // The keyed account request.
    assert!(text.contains("|6040=6|"), "the account request is stated: {text}");
    let first = key_of(&text);

    // A second request states a different key, and a second state does too:
    // the venue answers a key it is already serving with nothing, and a
    // connection outlives the loops that use it.
    ccp.send_account_refresh("DU123456", &mut conn, &mut hb, &SharedState::new());
    let n = peer.read(&mut buf).unwrap();
    let second = key_of(&String::from_utf8_lossy(&buf[..n]).replace('\u{1}', "|"));
    assert_ne!(first, second, "a second request states a key of its own");

    let mut fresh = CcpState::new();
    fresh.send_account_refresh("DU123456", &mut conn, &mut hb, &SharedState::new());
    let n = peer.read(&mut buf).unwrap();
    let third = key_of(&String::from_utf8_lossy(&buf[..n]).replace('\u{1}', "|"));
    assert_ne!(second, third, "and so does a request from a state built later");

    // The subscription is closed under the key it was opened with. Tag 6036
    // states which of the two the request is.
    fresh.send_account_unsubscribe("DU123456", &mut conn, &mut hb);
    let n = peer.read(&mut buf).unwrap();
    let closing = String::from_utf8_lossy(&buf[..n]).replace('\u{1}', "|");
    assert!(closing.contains("|6036=0|"), "the request closes rather than opens: {closing}");
    assert_eq!(key_of(&closing), third, "and names the subscription it opened: {closing}");
    assert!(text.contains("|6095=DU123456|"), "naming the account: {text}");
    // The account rides its own tag on the display request, not inside a key.
    assert!(text.contains("|1=DU123456|"), "and tag 1 names it too: {text}");
}

/// A holding names its own contract.
///
/// The feed states the symbol on 6068 and the security type on 167 beside the
/// quantity. Read only for the contract id, a holding reaches the caller
/// carrying an id and nothing else, and stays that way until a definition
/// lookup answers.
#[test]
fn a_holding_carries_the_contract_the_feed_names() {
    let shared = SharedState::new();
    let mut context = Context::new();
    let mut ccp = CcpState::new();
    let mut hb = HeartbeatState::new();
    let mut conn = None;

    let msg = ["6008=756733", "6068=SPY", "167=STK", "6064=100", "6101=768.5",
               "6008=265598", "6068=AAPL", "167=STK", "6064=50", "6101=316.2",
               "6008=0"].join("\u{1}");
    ccp.handle_position_feed(msg.as_bytes(), &mut conn, &mut context, &shared, &None, &mut hb);

    let held = shared.portfolio.position_infos();
    let spy = held.iter().find(|p| p.con_id == 756733).expect("the first holding");
    assert_eq!(spy.symbol, "SPY", "named as the feed names it");
    assert_eq!(spy.sec_type, "STK");
    let aapl = held.iter().find(|p| p.con_id == 265598).expect("the second holding");
    assert_eq!(aapl.symbol, "AAPL", "each entry carries its own, not the one before it");
    assert_eq!(aapl.sec_type, "STK");
}

/// Every ask for account and position data draws its own key, and the state
/// remembers the one it last asked under.
///
/// The venue answers a key it is already serving with nothing, so two asks
/// sharing a key means the second is not answered — and a reconnect that named
/// a fixed key was asking under one the refreshes had already spent, leaving
/// the position pushes not resuming after a drop. The recorded key matters for
/// the same reason: the unsubscribe closes what it names.
#[test]
fn every_account_request_draws_its_own_key() {
    let mut ccp = CcpState::new();

    let first = ccp.next_account_request_key();
    assert_eq!(ccp.account_request_key.as_deref(), Some(first.as_str()));

    let second = ccp.next_account_request_key();
    assert_ne!(first, second, "two asks shared a key, so one goes unanswered");
    assert_eq!(
        ccp.account_request_key.as_deref(),
        Some(second.as_str()),
        "the unsubscribe would close a key this connection is no longer served under",
    );
}

/// The venue's urgency and the caller's kind are two numberings, and the two
/// exchange kinds sit the other way round in each. Passed straight through, a
/// caller halting on an exchange that had stopped trading acted on one that
/// had started.
#[test]
fn a_bulletin_is_reported_as_the_kind_a_caller_reads_not_the_urgency_stated() {
    // Stated urgency -> the kind a caller is told, and what that kind means.
    let cases = [
        (1, 1, "ordinary news"),
        (2, 3, "an exchange that has stopped trading"),
        (3, 2, "an exchange that has started"),
        (8, 4, "plain text"),
        (9, 5, "a message meant to be shown"),
        (10, 6, "one written as markup"),
    ];
    for (urgency, kind, what) in cases {
        let mut ccp = CcpState::new();
        let shared = SharedState::new();
        let parsed = std::collections::HashMap::from([
            (crate::protocol::fix::TAG_URGENCY, urgency.to_string()),
            (crate::protocol::fix::TAG_HEADLINE, what.to_string()),
            (crate::protocol::fix::TAG_SECURITY_EXCHANGE, "NASDAQ".to_string()),
            (crate::protocol::fix::TAG_BULLETIN_ID, "4242".to_string()),
        ]);
        ccp.handle_news_bulletin(&parsed, &shared);
        let sent = shared.market.drain_news_bulletins();
        assert_eq!(sent.len(), 1, "urgency {urgency} was dropped");
        assert_eq!(sent[0].msg_type, kind, "urgency {urgency} names {what}");
        assert_eq!(sent[0].msg_id, 4242, "the venue numbers its own bulletins");
    }
}

/// A bulletin the venue did not number stands at the widest number one is
/// carried under, rather than at a count this session kept.
#[test]
fn an_unnumbered_bulletin_says_so() {
    let mut ccp = CcpState::new();
    let shared = SharedState::new();
    let parsed = std::collections::HashMap::from([
        (crate::protocol::fix::TAG_URGENCY, "1".to_string()),
        (crate::protocol::fix::TAG_HEADLINE, "something".to_string()),
    ]);
    ccp.handle_news_bulletin(&parsed, &shared);
    assert_eq!(shared.market.drain_news_bulletins()[0].msg_id, i32::MAX);
}

/// The charge is on a record of its own, and the execution report carries
/// none: captured against a real fill, the report has no commission tag at
/// all. Taken from there, every caller was told its fills were free.
#[test]
fn what_a_fill_cost_is_read_off_the_record_that_states_it() {
    let shared = SharedState::new();
    // The record as the venue sent it, from a captured session: the execution
    // it belongs to, what it cost, and the currency that is charged in.
    let parsed = std::collections::HashMap::from([
        (crate::protocol::fix::TAG_EXEC_ID, "00025b49.6a8880e4.01.01".to_string()),
        (crate::protocol::fix::TAG_TRADE_CHARGE, "1.000003".to_string()),
        (crate::protocol::fix::TAG_TRADE_CHARGE_CURRENCY, "USD".to_string()),
    ]);
    super::handle_trade_charge(&parsed, &shared);

    let charged = shared.orders.drain_charges();
    assert_eq!(charged.len(), 1);
    assert_eq!(charged[0].exec_id, "00025b49.6a8880e4.01.01");
    assert!((charged[0].commission_and_fees - 1.000003).abs() < 1e-9);
    assert_eq!(charged[0].currency, "USD", "as the venue charges it, not as the contract is priced");
    assert!(shared.orders.drain_charges().is_empty(), "read once");
}

/// A record naming no execution, or stating no charge, says nothing — and
/// nothing is what is reported, rather than a zero against some other fill.
#[test]
fn a_record_that_states_no_charge_reports_none() {
    for parsed in [
        // No execution named.
        std::collections::HashMap::from([
            (crate::protocol::fix::TAG_TRADE_CHARGE, "1.5".to_string()),
        ]),
        // Named, and no charge stated.
        std::collections::HashMap::from([
            (crate::protocol::fix::TAG_EXEC_ID, "abc.def.01.01".to_string()),
        ]),
    ] {
        let shared = SharedState::new();
        super::handle_trade_charge(&parsed, &shared);
        assert!(shared.orders.drain_charges().is_empty());
    }
}

/// A trade cancel stated on the report type alone reverses what it undoes.
///
/// The venue states a restatement two ways: on the transaction type, and on the
/// report type itself. Read only from the first, a trade cancel arriving under
/// the second booked nothing at all, so a quantity the venue had just undone
/// stayed on the account and on every exposure decision made from it.
#[test]
fn a_trade_cancel_stated_on_the_report_type_reverses_what_it_undoes() {
    let mut ccp = CcpState::new();
    let mut context = Context::new();
    let instrument = context.register_instrument(756733);
    context.set_symbol(instrument, "SPY".to_string());
    let shared = SharedState::new();
    context.insert_order(crate::types::Order::new(
        42, instrument, Side::Buy, 100 * QTY_SCALE, 400 * PRICE_SCALE, b'2', b'0', 0,
    ));

    // Fifty trade.
    let fill = exec_report_frame(&[
        (39, "1"), (150, "F"), (100, "ARCA"), (198, "ARCA:1"),
        (17, "exec-1"), (32, "50"), (31, "412.25"), (14, "50"), (38, "100"),
    ]);
    ccp.handle_exec_report(&fill, b"", &mut context, &shared, &None, "");
    assert_eq!(context.position(instrument), 50.0, "fifty are held");
    let _ = shared.orders.drain_fills();

    // The venue cancels the trade, stating it on the report type.
    let bust = exec_report_frame(&[
        (39, "1"), (150, "H"), (100, "ARCA"), (198, "ARCA:1"),
        (17, "exec-2"), (32, "50"), (31, "412.25"), (14, "0"), (38, "100"),
    ]);
    ccp.handle_exec_report(&bust, b"", &mut context, &shared, &None, "");

    assert_eq!(
        context.position(instrument), 0.0,
        "the account no longer holds what the venue undid",
    );
    let fills = shared.orders.drain_fills();
    let reversal = fills.last().expect("the reversal is reported to the caller");
    assert_eq!(reversal.0.cum_qty, 0, "the order total goes back: {reversal:?}");
}

/// A busted trade on an order this session does not track books nothing,
/// rather than booking the trade again as a purchase.
///
/// A bust restates the cumulative quantity downwards. The reconciliation
/// works out what to book by subtracting what the order has already filled
/// from what the report states — and an untracked order used to supply a
/// baseline of zero, so the subtraction returned the whole cumulative figure
/// and a hundred lots that had just been undone were booked as a hundred
/// bought. The position feed heals the account; the fill handed to the caller
/// was never healed.
#[test]
fn a_bust_on_an_untracked_order_books_nothing_rather_than_a_purchase() {
    let mut ccp = CcpState::new();
    let mut context = Context::new();
    let instrument = context.register_instrument(756733);
    context.set_symbol(instrument, "SPY".to_string());
    let shared = SharedState::new();

    // Nothing is tracked under this number: another client's order, or one
    // from a session that has since ended.
    assert!(context.order(42).is_none(), "the order is untracked to begin with");

    // A bust: the trade is undone, and the report restates the total.
    let bust = exec_report_frame(&[
        (150, "1"), (39, "1"), (20, "1"), (6008, "756733"),
        (32, "100"), (14, "100"), (31, "500.0"), (54, "1"), (55, "SPY"),
    ]);
    ccp.handle_exec_report(&bust, b"", &mut context, &shared, &None, "");

    // Summed rather than checked per fill, so no fills at all is an answer
    // and not a test that skipped its own assertion.
    let booked: i64 = shared.orders.drain_fills().iter().map(|f| f.0.qty).sum();
    assert!(
        booked <= 0,
        "a busted trade booked {booked} as bought on an order nobody here tracks",
    );
}

/// A contract the venue names an order on at connect is counted where the
/// API reads the instrument count. Only a subscription refreshed that count,
/// so a session that subscribed to nothing composed its global cancel over no
/// instruments, withdrew none of the orders the venue had named, and returned
/// without an error.
#[test]
fn a_recovered_orders_contract_is_counted_where_the_api_reads_the_count() {
    let mut ccp = CcpState::new();
    let mut context = Context::new();
    let shared = SharedState::new();
    let named = crate::protocol::fix::fix_build(&[
        (fix::TAG_MSG_TYPE, fix::MSG_EXEC_REPORT),
        (11, "1787685160171119.0"), (150, "0"), (39, "0"), (6008, "756733"),
        (38, "1"), (14, "0"), (55, "SPY"), (54, "1"), (40, "3"),
    ], 1);
    ccp.process_ccp_message(&named, &mut None, &mut context, &shared,
        &None, &mut HeartbeatState::new(), "DU1");
    assert!(context.order(1787685160171119).is_some(), "the venue named it, so the engine holds it");
    assert_eq!(shared.market.instrument_count(), 1, "and its contract counts for a global cancel");
}

/// The venue replays the executions behind the day's orders at connect and
/// marks each as restated. A partial fill among them, for an order this
/// session does not hold as working, is the past of an order that finished —
/// an immediate-or-cancel that filled part and lapsed — and the lapse is never
/// replayed. Read as the order's state, it listed an order the venue was not
/// working. A restated report that finishes an order is still filed, for the
/// caller asking what completed.
#[test]
fn a_restated_partial_fill_of_an_order_not_held_lists_no_working_order() {
    let mut ccp = CcpState::new();
    let mut context = Context::new();
    let shared = SharedState::new();
    let mut lapsed = std::collections::HashMap::new();
    for (tag, val) in [
        // Tag 20 as the venue states it: the measured replay burst carries it
        // on 148 of 149 frames, so a fixture without it is not the wire — and
        // a reading of the correction that does not exclude a replay routes
        // the whole burst through as corrections, which this test then misses.
        (11u32, "1788103664.0"), (39u32, "1"), (150u32, "1"), (97u32, "Y"), (20u32, "1"),
        (17u32, "00012dbb.6a93b90b.01.01"), (6008u32, "479624278"), (55u32, "BTC"),
        (54u32, "1"), (38u32, "0.001"), (14u32, "0.00018171"), (32u32, "0.00018171"),
        (31u32, "78882"), (151u32, "0.00081829"), (44u32, "78882"), (59u32, "3"),
        (40u32, "2"), (60u32, "20260830-15:27:45"),
    ] {
        lapsed.insert(tag, val.to_string());
    }
    ccp.handle_exec_report(&lapsed, b"", &mut context, &shared, &None, "");
    assert!(shared.orders.drain_open_orders().is_empty(), "the venue is not working it");
    assert_eq!(shared.orders.drain_restated_executions().len(), 1, "the execution is on record");

    let mut done = std::collections::HashMap::new();
    for (tag, val) in [
        (11u32, "1787923041.0"), (39u32, "2"), (150u32, "2"), (97u32, "Y"),
        (17u32, "00025b49.6a8880e4.01.01"), (6008u32, "756733"), (55u32, "SPY"),
        (54u32, "2"), (38u32, "1"), (14u32, "1"), (32u32, "1"), (31u32, "645.10"),
        (151u32, "0"), (40u32, "1"), (60u32, "20260830-13:30:00"),
    ] {
        done.insert(tag, val.to_string());
    }
    ccp.handle_exec_report(&done, b"", &mut context, &shared, &None, "");
    assert_eq!(
        shared.orders.get_order_info(1787923041).map(|o| o.order_state.status),
        Some("Filled".to_string()),
        "a restated report that finishes an order is filed as completed",
    );
}

/// A refused request is told which one it was, and told at once.
///
/// The venue names the request it is refusing on tag 320. Attributed by
/// counting instead — only a lone request in flight was ever told — five
/// contract lookups asked together drew five refusals inside a tenth of a
/// second and not one caller heard them: all five waited out the sweep and
/// were then told the request had timed out with no reply from the venue,
/// when the reply had come back at once and said exactly what was wrong.
///
/// Measured against the venue, before and after: 20.0s to 0.1s, and the
/// venue's own words instead of a timeout that did not happen.
#[test]
fn a_refusal_reaches_the_request_the_venue_named() {
    let (mut ccp, mut context, shared) = u186_test_state();
    for req_id in [11u32, 12, 13] {
        ccp.pending_secdef.push((req_id, false, Instant::now() + SECDEF_TIMEOUT));
    }

    let refused = crate::protocol::fix::fix_build(&[
        (fix::TAG_MSG_TYPE, "3"),
        (320, "12"),
        (58, "Unsupported type"),
    ], 1);
    ccp.process_ccp_message(&refused, &mut None, &mut context, &shared,
        &None, &mut HeartbeatState::new(), "DU1");

    let waiting: Vec<u32> = ccp.pending_secdef.iter().map(|(id, _, _)| *id).collect();
    assert_eq!(waiting, vec![11, 13], "the one the venue named, and only it");

    let errors = shared.reference.drain_historical_errors();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].0, 12, "told to the request that drew it");
    assert_eq!(errors[0].1, 200);
    assert!(errors[0].2.contains("Unsupported type"), "in the venue's own words: {}", errors[0].2);
    assert_eq!(shared.reference.drain_contract_details_end(), vec![12],
        "and ended, so a caller blocked on it is let go");
}

/// Where the venue names nothing, a lone request is still told.
#[test]
fn a_refusal_naming_no_request_still_reaches_a_lone_one() {
    let (mut ccp, mut context, shared) = u186_test_state();
    ccp.pending_secdef.push((7, false, Instant::now() + SECDEF_TIMEOUT));

    let refused = crate::protocol::fix::fix_build(&[
        (fix::TAG_MSG_TYPE, "3"),
        (58, "Unsupported type"),
    ], 1);
    ccp.process_ccp_message(&refused, &mut None, &mut context, &shared,
        &None, &mut HeartbeatState::new(), "DU1");

    assert!(ccp.pending_secdef.is_empty());
    assert_eq!(shared.reference.drain_historical_errors()[0].0, 7);
    assert_eq!(shared.reference.drain_contract_details_end(), vec![7]);
}

/// And a refusal naming a request nobody is waiting on takes nothing with it.
#[test]
fn a_refusal_naming_a_stranger_leaves_the_waiting_alone() {
    let (mut ccp, mut context, shared) = u186_test_state();
    for req_id in [11u32, 12] {
        ccp.pending_secdef.push((req_id, false, Instant::now() + SECDEF_TIMEOUT));
    }

    let refused = crate::protocol::fix::fix_build(&[
        (fix::TAG_MSG_TYPE, "3"),
        (320, "99"),
        (58, "Unsupported type"),
    ], 1);
    ccp.process_ccp_message(&refused, &mut None, &mut context, &shared,
        &None, &mut HeartbeatState::new(), "DU1");

    assert_eq!(ccp.pending_secdef.len(), 2, "neither was the one refused");
    assert!(shared.reference.drain_historical_errors().is_empty());
}
/// A refusal is cached in the shape of a parked order — status "Inactive"
/// with the reason on `completed_status` — and the venue can still send a
/// frame from before the refusal afterwards. Once the completion window has
/// passed there is nothing left to refuse it but the terminal-status guard,
/// so the replay must meet a finished order and leave the refusal as it is:
/// reported as live, a strategy would hedge a position it does not have.
#[test]
fn a_replayed_frame_does_not_reopen_a_refused_order() {
    let (mut ccp, mut context, shared) = ord_status_test_state();
    // The venue refuses the order.
    let refusal = exec_report_frame(&[
        (39, "8"), (150, "0"),
        (58, "No valid bid/ask"), (103, "1"),
    ]);
    ccp.handle_exec_report(&refusal, b"", &mut context, &shared, &None, "");
    assert!(context.order(42).is_none(), "a refusal retires the order");

    // Outlast the completion window the way a long session does: the memory
    // is held by age with a hard cap of 65_536, so a cap of other
    // completions evicts this one.
    for id in 1000u64..(1000 + 65_536) {
        shared.orders.push_completed_order(crate::types::CompletedOrder {
            order_id: id,
            instrument: 0,
            status: crate::types::OrderStatus::Filled,
            filled_qty: 0,
            timestamp_ns: 0,
        });
    }
    assert!(!shared.orders.recently_completed(42), "the window has passed");

    // The frame the venue already sent once, before the refusal: the order
    // parked. Replayed after the refusal, it must not stand in for it.
    let replayed = exec_report_frame(&[(39, "I"), (150, "0")]);
    ccp.handle_exec_report(&replayed, b"", &mut context, &shared, &None, "");

    let info = shared.orders.get_order_info(42).expect("the refusal is still cached");
    assert_eq!(info.order_state.status, "Inactive");
    assert_eq!(
        info.order_state.completed_status, "No valid bid/ask",
        "a replayed frame does not reopen a refused order",
    );
    assert!(
        shared.orders.drain_open_orders().is_empty(),
        "a refused order is not listed in the open-order book",
    );
    assert!(context.order(42).is_none(), "nor tracked again in the engine");
}


/// A working order with a record the modify path can restate from.
fn working_order_state() -> (Context, std::sync::Arc<SharedState>) {
    let mut context = Context::new();
    let instrument = context.register_instrument(756733);
    context.insert_order(crate::types::Order::new(
        42, instrument, Side::Buy, 100 * QTY_SCALE, 100 * PRICE_SCALE, b'2', b'0', 0,
    ));
    context.update_order_status(42, crate::types::OrderStatus::Submitted, false);
    (context, std::sync::Arc::new(SharedState::new()))
}

/// The name a later cancel gives as the original does not go backwards.
///
/// Reports do not have to arrive in the order the revisions were sent. One for
/// an earlier revision arriving behind a later one moved the recorded name back
/// to a revision the venue had already superseded, so the next cancel named
/// that superseded revision as the original and the venue answered that it knew
/// no such order — leaving the order working, out of reach of a withdrawal.
#[test]
fn a_report_for_an_earlier_revision_does_not_take_back_the_name() {
    let (mut context, shared) = working_order_state();
    let mut ccp = CcpState::new();

    for revision in ["42.1", "42.2"] {
        let ack = exec_report_frame(&[
            (11, revision), (150, "5"), (39, "5"), (100, "ARCA"), (198, "ARCA:1"),
        ]);
        ccp.handle_exec_report(&ack, b"", &mut context, &shared, &None, "");
    }
    assert_eq!(context.last_clord.get(&42).map(String::as_str), Some("42.2"));

    // The venue answers the first revision after the second.
    let late = exec_report_frame(&[
        (11, "42.1"), (150, "5"), (39, "5"), (100, "ARCA"), (198, "ARCA:1"),
    ]);
    ccp.handle_exec_report(&late, b"", &mut context, &shared, &None, "");

    assert_eq!(
        context.last_clord.get(&42).map(String::as_str),
        Some("42.2"),
        "the name stays on the latest revision the venue was sent",
    );
}

/// The venue's naming at connect settles where an order's revisions stand.
///
/// An order the venue names carries the revision it reached in an earlier
/// session. Counted from zero beside it, the next replace named a revision the
/// venue had already been given, and the cancel behind it named one the venue
/// had superseded — answered that no such order exists, which retires the
/// record here while the order goes on working there. The forward-only rule
/// does not hold against that naming either: it is the venue's account of what
/// it holds, not an answer that can arrive out of turn.
#[test]
fn the_naming_at_connect_settles_where_an_orders_revisions_stand() {
    let mut context = Context::new();
    let mut ccp = CcpState::new();
    let shared = SharedState::new();

    // An order this session never placed, named on its fifth revision.
    let named = exec_report_frame(&[
        (11, "42.5"), (150, "0"), (39, "0"), (54, "1"), (38, "1"),
        (44, "100.00"), (6008, "756733"), (55, "SPY"),
    ]);
    ccp.handle_exec_report(&named, b"", &mut context, &shared, &None, "");

    assert_eq!(
        context.last_clord.get(&42).map(String::as_str),
        Some("42.5"),
        "a cancel names as the original what the venue stated",
    );
    assert_eq!(
        context.modify_versions.get(&42),
        Some(&5),
        "and the next replace names the revision past it, not one the venue holds",
    );

    // A revision this client sent and never had answered — its name written
    // into the record ahead of the answer the dropped connection took with it
    // — and the naming at the next connect saying the earlier revision is
    // what the venue is working.
    let attempt = *context.order(42).expect("the order was recovered");
    context.last_clord.insert(42, "42.6".to_string());
    context.pre_replace.insert((42, 6), (attempt, "42.5".to_string(), None));
    context.modify_versions.insert(42, 6);
    context.mark_orders_uncertain();
    ccp.handle_exec_report(&named, b"", &mut context, &shared, &None, "");

    assert_eq!(
        context.last_clord.get(&42).map(String::as_str),
        Some("42.5"),
        "the venue's account of the order puts the unanswered name back",
    );
    assert_eq!(
        context.modify_versions.get(&42),
        Some(&6),
        "while the counter keeps the highest revision this client has issued",
    );
    assert!(
        !context.replace_is_outstanding(42),
        "and the fallback that name was written beside goes with it",
    );
}

/// A refused revision puts back what the venue held before that revision, not
/// the terms of an attempt the venue itself refused.
///
/// Revisions overlap: the venue takes a second before it has answered the
/// first, and a refusal of either takes the order out of the book. Kept one
/// per order, the fallback was overwritten by the later attempt, so the
/// refusal restored a price the venue had already refused — or found nothing
/// and left the record showing one it had never accepted.
#[test]
fn a_refused_revision_falls_back_to_the_revision_it_replaced() {
    let (mut context, shared) = working_order_state();
    let mut ccp = CcpState::new();

    // Two revisions out, neither answered. Each keeps what the record held
    // before it: the first the original price, the second the first's.
    let at_100 = *context.order(42).expect("the order is tracked");
    context.pre_replace.insert((42, 1), (at_100, "42.0".to_string(), None));
    let mut at_101 = at_100;
    at_101.price = 101 * PRICE_SCALE;
    context.insert_order(at_101);
    context.pre_replace.insert((42, 2), (at_101, "42.1".to_string(), None));
    let mut at_102 = at_100;
    at_102.price = 102 * PRICE_SCALE;
    context.insert_order(at_102);
    context.modify_versions.insert(42, 2);

    // The venue refuses the first revision, naming it; the order stands.
    let refusal = exec_report_frame(&[
        (11, "42.1"), (150, "5"), (39, "5"), (100, "ARCA"), (198, "ARCA:1"), (378, "102"),
    ]);
    ccp.handle_exec_report(&refusal, b"", &mut context, &shared, &None, "");

    assert_eq!(
        context.order(42).expect("the order still stands").price,
        100 * PRICE_SCALE,
        "the record holds what the venue had before the revision it refused",
    );
    assert!(
        !context.replace_is_outstanding(42),
        "and a fallback built on terms the venue never held goes with it",
    );
}

/// A replace writes its attempt into the record ahead of the venue's answer.
/// Where the venue refuses the attempt, the record must fall back to what the
/// venue is known to hold: every later action restates from the record, so a
/// modify naming only a price went out restating the time-in-force and the
/// quantity the venue had already refused, and nothing said so.
#[test]
fn a_refused_replace_puts_back_the_terms_the_venue_holds() {
    use std::io::Read;
    let (mut context, shared) = working_order_state();
    let mut ccp = CcpState::new();
    let (conn, mut peer) = crate::protocol::connection::Connection::for_test();
    let mut conn = Some(conn);
    let mut hb = HeartbeatState::new();
    let mut buf = [0u8; 4096];

    // The attempt states a new price, a new quantity and a new time-in-force.
    context.modify_ex(42, 105 * PRICE_SCALE, 200, false, 0, b'1', 0);
    crate::engine::hot_loop::order_builder::drain_and_send_orders(
        &mut conn, &mut context, "DU1", &mut hb, false, &shared, false, &None,
    );
    let n = peer.read(&mut buf).unwrap();
    let attempt = String::from_utf8_lossy(&buf[..n]).to_string();
    assert!(attempt.contains("35=G"), "the replace went out: {attempt}");
    assert!(attempt.split('\u{1}').any(|f| f == "59=1"), "stating the asked time-in-force: {attempt}");
    assert!(attempt.split('\u{1}').any(|f| f == "38=200"), "and the asked quantity: {attempt}");

    // The venue refuses the revision; the order stands as it was.
    let refusal = exec_report_frame(&[
        (11, "42.1"), (150, "5"), (39, "5"), (100, "ARCA"), (198, "ARCA:1"), (378, "102"),
    ]);
    ccp.handle_exec_report(&refusal, b"", &mut context, &shared, &None, "");

    let order = context.order(42).expect("the order still stands");
    assert_eq!(order.price, 100 * PRICE_SCALE, "the refused price does not stand");
    assert_eq!(order.qty, 100 * QTY_SCALE, "the refused quantity does not stand");
    assert_eq!(order.tif, b'0', "the refused time-in-force does not stand");
    assert_eq!(order.status, crate::types::OrderStatus::Submitted, "working, not awaiting a replace");

    // A modify naming only a price carries the terms the venue holds.
    context.modify(42, 106 * PRICE_SCALE, 100, false);
    crate::engine::hot_loop::order_builder::drain_and_send_orders(
        &mut conn, &mut context, "DU1", &mut hb, false, &shared, false, &None,
    );
    let n = peer.read(&mut buf).unwrap();
    let second = String::from_utf8_lossy(&buf[..n]).to_string();
    assert!(second.contains("35=G"), "a replace went out: {second}");
    assert!(
        second.split('\u{1}').any(|f| f == "59=0"),
        "it carries the time-in-force the venue holds, not the one it refused: {second}",
    );
}

/// A cancellation states the quantity off the record. After a refused replace
/// the record holds what the venue holds, so the cancellation names the
/// quantity that is working, not the one the venue refused.
#[test]
fn a_cancel_after_a_refused_replace_names_the_quantity_the_venue_holds() {
    use std::io::Read;
    let (mut context, shared) = working_order_state();
    let mut ccp = CcpState::new();
    let (conn, mut peer) = crate::protocol::connection::Connection::for_test();
    let mut conn = Some(conn);
    let mut hb = HeartbeatState::new();
    let mut buf = [0u8; 4096];

    context.modify_ex(42, 105 * PRICE_SCALE, 200, false, 0, b'1', 0);
    crate::engine::hot_loop::order_builder::drain_and_send_orders(
        &mut conn, &mut context, "DU1", &mut hb, false, &shared, false, &None,
    );
    let n = peer.read(&mut buf).unwrap();
    assert!(String::from_utf8_lossy(&buf[..n]).contains("35=G"), "the replace went out");

    let refusal = exec_report_frame(&[(11, "42.1"), (150, "5"), (39, "5"), (378, "102")]);
    ccp.handle_exec_report(&refusal, b"", &mut context, &shared, &None, "");

    context.cancel(42);
    crate::engine::hot_loop::order_builder::drain_and_send_orders(
        &mut conn, &mut context, "DU1", &mut hb, false, &shared, false, &None,
    );
    let n = peer.read(&mut buf).unwrap();
    let cancel = String::from_utf8_lossy(&buf[..n]).to_string();
    assert!(cancel.contains("35=F"), "a cancel went out: {cancel}");
    assert!(
        cancel.split('\u{1}').any(|f| f == "38=100"),
        "it names the quantity the venue holds, not the one it refused: {cancel}",
    );
}

/// The venue answers a refused replace on the reject message as well, naming
/// the attempt on tag 434. The record holds that attempt ahead of the answer,
/// so the answer puts back what the venue is known to hold — except where the
/// refusal says the order itself is gone, which the retirement below already
/// states.
#[test]
fn a_replace_refused_on_the_reject_message_puts_back_the_prior_terms() {
    use std::io::Read;
    let (mut context, shared) = working_order_state();
    let mut ccp = CcpState::new();
    let (conn, mut peer) = crate::protocol::connection::Connection::for_test();
    let mut conn = Some(conn);
    let mut hb = HeartbeatState::new();
    let mut buf = [0u8; 4096];

    context.modify_ex(42, 105 * PRICE_SCALE, 200, false, 0, b'1', 0);
    crate::engine::hot_loop::order_builder::drain_and_send_orders(
        &mut conn, &mut context, "DU1", &mut hb, false, &shared, false, &None,
    );
    let n = peer.read(&mut buf).unwrap();
    assert!(String::from_utf8_lossy(&buf[..n]).contains("35=G"), "the replace went out");

    // The refusal answers the replace itself, on a reason that leaves the
    // order working.
    let mut frame = std::collections::HashMap::new();
    frame.insert(41u32, "42.1".to_string());
    frame.insert(434u32, "2".to_string());
    frame.insert(102u32, "0".to_string());
    ccp.handle_cancel_reject(&frame, &mut context, &shared, &None);

    let order = context.order(42).expect("the order still stands");
    assert_eq!(order.price, 100 * PRICE_SCALE, "the refused price does not stand");
    assert_eq!(order.qty, 100 * QTY_SCALE, "the refused quantity does not stand");
    assert_eq!(order.tif, b'0', "the refused time-in-force does not stand");
    assert_eq!(order.status, crate::types::OrderStatus::Submitted, "working again");
}

/// A recovered order's replace answers to its local revision even when the
/// original wire name is built from a different number.
#[test]
fn a_recovered_orders_replace_rejection_restores_its_terms_and_name() {
    use std::io::Read;
    let mut ccp = CcpState::new();
    let mut context = Context::new();
    let shared = std::sync::Arc::new(SharedState::new());
    let recovery = exec_report_frame(&[
        (11, "9000.0"), (6121, "42"), (150, "0"), (39, "0"),
        (6008, "756733"), (55, "SPY"), (54, "1"), (38, "100"),
        (40, "2"), (44, "100"), (59, "0"), (100, "ARCA"), (198, "ARCA:1"),
    ]);
    ccp.handle_exec_report(&recovery, b"", &mut context, &shared, &None, "DU1");
    assert_eq!(context.last_clord.get(&42).map(String::as_str), Some("9000.0"));

    let (conn, mut peer) = crate::protocol::connection::Connection::for_test();
    let mut conn = Some(conn);
    let mut hb = HeartbeatState::new();
    let mut buf = [0u8; 4096];
    context.modify_ex(42, 105 * PRICE_SCALE, 200, false, 0, b'1', 0);
    crate::engine::hot_loop::order_builder::drain_and_send_orders(
        &mut conn, &mut context, "DU1", &mut hb, false, &shared, false, &None,
    );
    let n = peer.read(&mut buf).unwrap();
    let attempt = fix::fix_parse(&buf[..n]);
    assert_eq!(attempt.get(&35).map(String::as_str), Some("G"));
    assert_eq!(attempt.get(&11).map(String::as_str), Some("42.1"));
    assert_eq!(attempt.get(&41).map(String::as_str), Some("9000.0"));
    assert_eq!(context.last_clord.get(&42).map(String::as_str), Some("42.1"));
    assert!(context.pre_replace.contains_key(&(42, 1)));
    assert_eq!(context.order(42).unwrap().price, 105 * PRICE_SCALE);

    let refusal = exec_report_frame(&[
        (35, "9"), (434, "2"), (102, "0"),
        (11, attempt.get(&11).unwrap()), (41, attempt.get(&41).unwrap()),
    ]);
    ccp.handle_cancel_reject(&refusal, &mut context, &shared, &None);

    let order = context.order(42).expect("the recovered order still stands");
    assert_eq!(order.price, 100 * PRICE_SCALE, "the refused price does not stand");
    assert_eq!(order.qty, 100 * QTY_SCALE, "the refused quantity does not stand");
    assert_eq!(order.tif, b'0', "the refused time-in-force does not stand");
    assert_eq!(order.status, crate::types::OrderStatus::Submitted);
    assert_eq!(context.last_clord.get(&42).map(String::as_str), Some("9000.0"));
    assert!(!context.replace_is_outstanding(42));
    let rejects = shared.orders.drain_cancel_rejects();
    assert_eq!(rejects.len(), 1);
    assert_eq!(rejects[0].order_id, 42);
    assert!(rejects[0].answers_a_live_change);

    context.cancel(42);
    crate::engine::hot_loop::order_builder::drain_and_send_orders(
        &mut conn, &mut context, "DU1", &mut hb, false, &shared, false, &None,
    );
    let n = peer.read(&mut buf).unwrap();
    let cancel = fix::fix_parse(&buf[..n]);
    assert_eq!(cancel.get(&35).map(String::as_str), Some("F"));
    assert_eq!(cancel.get(&41).map(String::as_str), Some("9000.0"));
    assert_eq!(cancel.get(&38).map(String::as_str), Some("100"));
}

/// A second refusal for the same cancel does not reach past the order it was
/// sent for.
///
/// A recovered order answers to the permanent id the venue stated beside it,
/// so the cancel goes out naming that. The first refusal retires the order and
/// drops the record the name was resolved through — leaving the second with
/// only the digits, which name whichever live order happens to carry that
/// number. The cancel's own name on tag 11 is this client's own and still
/// says which order it was sent for.
#[test]
fn a_second_cancel_refusal_does_not_retire_an_unrelated_order() {
    use std::io::Read;
    let mut ccp = CcpState::new();
    let mut context = Context::new();
    let shared = std::sync::Arc::new(SharedState::new());

    let recovery = exec_report_frame(&[
        (11, "9000.0"), (6121, "42"), (150, "0"), (39, "0"),
        (6008, "756733"), (55, "SPY"), (54, "1"), (38, "100"),
        (40, "2"), (44, "100"), (59, "0"), (100, "ARCA"), (198, "ARCA:1"),
    ]);
    ccp.handle_exec_report(&recovery, b"", &mut context, &shared, &None, "DU1");
    // An unrelated order whose own number is what those digits read as.
    let other = exec_report_frame(&[
        (11, "9000"), (6121, "9000"), (150, "0"), (39, "0"),
        (6008, "756733"), (55, "SPY"), (54, "1"), (38, "50"),
        (40, "2"), (44, "100"), (59, "0"), (100, "ARCA"), (198, "ARCA:2"),
    ]);
    ccp.handle_exec_report(&other, b"", &mut context, &shared, &None, "DU1");
    assert!(context.order(9000).is_some(), "the unrelated order is live");

    let (conn, mut peer) = crate::protocol::connection::Connection::for_test();
    let mut conn = Some(conn);
    let mut hb = HeartbeatState::new();
    let mut buf = [0u8; 4096];
    context.cancel(42);
    crate::engine::hot_loop::order_builder::drain_and_send_orders(
        &mut conn, &mut context, "DU1", &mut hb, false, &shared, false, &None,
    );
    let n = peer.read(&mut buf).unwrap();
    let cancel = fix::fix_parse(&buf[..n]);
    assert_eq!(cancel.get(&41).map(String::as_str), Some("9000.0"));

    let refusal = exec_report_frame(&[
        (35, "9"), (434, "1"), (102, "1"),
        (11, cancel.get(&11).expect("the cancel carries its own name")),
        (41, "9000.0"),
    ]);
    ccp.handle_cancel_reject(&refusal, &mut context, &shared, &None);
    ccp.handle_cancel_reject(&refusal, &mut context, &shared, &None);

    assert!(
        context.order(9000).is_some(),
        "the unrelated order was retired in place of the one that was cancelled",
    );
}

/// A refused revision puts back the name the venue holds, not only its terms.
///
/// A replace records the name it is about to emit ahead of the venue's answer,
/// so a cancel sent before the acknowledgement still names the right version.
/// A refusal says the venue never took that name — and every later cancel and
/// replace states it as the original, so the venue answers that it knows no
/// such order and the order goes on working out of reach of a withdrawal. The
/// venue's own reports cannot correct it either: the recorded name only ever
/// moves forward.
#[test]
fn a_refused_revision_puts_back_the_name_the_venue_holds() {
    use std::io::Read;
    let (mut context, shared) = working_order_state();
    let mut ccp = CcpState::new();
    let (conn, mut peer) = crate::protocol::connection::Connection::for_test();
    let mut conn = Some(conn);
    let mut hb = HeartbeatState::new();
    let mut buf = [0u8; 4096];

    context.modify_ex(42, 105 * PRICE_SCALE, 200, false, 0, b'1', 0);
    crate::engine::hot_loop::order_builder::drain_and_send_orders(
        &mut conn, &mut context, "DU1", &mut hb, false, &shared, false, &None,
    );
    let n = peer.read(&mut buf).unwrap();
    let replace = String::from_utf8_lossy(&buf[..n]).to_string();
    assert!(
        replace.split('\u{1}').any(|f| f == "41=42.0"),
        "the replace names the order the venue holds: {replace}",
    );

    // Refused on the reject message: nothing was applied, so the order keeps
    // the name it had.
    let mut frame = std::collections::HashMap::new();
    frame.insert(11u32, "42.1".to_string());
    frame.insert(41u32, "42.0".to_string());
    frame.insert(434u32, "2".to_string());
    frame.insert(102u32, "0".to_string());
    ccp.handle_cancel_reject(&frame, &mut context, &shared, &None);

    context.cancel(42);
    crate::engine::hot_loop::order_builder::drain_and_send_orders(
        &mut conn, &mut context, "DU1", &mut hb, false, &shared, false, &None,
    );
    let n = peer.read(&mut buf).unwrap();
    let cancel = String::from_utf8_lossy(&buf[..n]).to_string();
    assert!(cancel.contains("35=F"), "a cancel went out: {cancel}");
    assert!(
        cancel.split('\u{1}').any(|f| f == "41=42.0"),
        "it names the order the venue holds, not the revision it refused: {cancel}",
    );
}

/// A refusal naming an order the venue does not hold retires it. The fallback
/// kept against the attempt must not bring the order back: this passes on the
/// unfixed code too — it guards the restore above against resurrecting.
#[test]
fn a_refusal_of_a_gone_order_resurrects_nothing() {
    use std::io::Read;
    let (mut context, shared) = working_order_state();
    let mut ccp = CcpState::new();
    let (conn, mut peer) = crate::protocol::connection::Connection::for_test();
    let mut conn = Some(conn);
    let mut hb = HeartbeatState::new();
    let mut buf = [0u8; 4096];

    context.modify_ex(42, 105 * PRICE_SCALE, 200, false, 0, b'1', 0);
    crate::engine::hot_loop::order_builder::drain_and_send_orders(
        &mut conn, &mut context, "DU1", &mut hb, false, &shared, false, &None,
    );
    let n = peer.read(&mut buf).unwrap();
    assert!(String::from_utf8_lossy(&buf[..n]).contains("35=G"), "the replace went out");

    let mut frame = std::collections::HashMap::new();
    frame.insert(41u32, "42.1".to_string());
    frame.insert(434u32, "2".to_string());
    frame.insert(102u32, "1".to_string()); // UnknownOrder: the venue holds no such order
    ccp.handle_cancel_reject(&frame, &mut context, &shared, &None);

    assert!(context.order(42).is_none(), "the order stays gone");
}

/// An accepted replace spends the fallback: a refusal arriving behind the
/// acceptance must not put the old terms back over what the venue accepted.
#[test]
fn an_acknowledged_replace_spends_the_fallback() {
    use std::io::Read;
    let (mut context, shared) = working_order_state();
    let mut ccp = CcpState::new();
    let (conn, mut peer) = crate::protocol::connection::Connection::for_test();
    let mut conn = Some(conn);
    let mut hb = HeartbeatState::new();
    let mut buf = [0u8; 4096];

    context.modify_ex(42, 105 * PRICE_SCALE, 200, false, 0, b'1', 0);
    crate::engine::hot_loop::order_builder::drain_and_send_orders(
        &mut conn, &mut context, "DU1", &mut hb, false, &shared, false, &None,
    );
    let n = peer.read(&mut buf).unwrap();
    assert!(String::from_utf8_lossy(&buf[..n]).contains("35=G"), "the replace went out");

    // The venue accepts, then a stale refusal of the same attempt arrives.
    let ack = exec_report_frame(&[(11, "42.1"), (150, "5"), (39, "5"), (100, "ARCA"), (198, "ARCA:1")]);
    ccp.handle_exec_report(&ack, b"", &mut context, &shared, &None, "");
    let mut frame = std::collections::HashMap::new();
    frame.insert(41u32, "42.1".to_string());
    frame.insert(434u32, "2".to_string());
    frame.insert(102u32, "0".to_string());
    ccp.handle_cancel_reject(&frame, &mut context, &shared, &None);

    let order = context.order(42).expect("the order still stands");
    assert_eq!(order.tif, b'1', "the accepted terms stand");
    assert_eq!(order.qty, 200 * QTY_SCALE, "the accepted quantity stands");
    assert_eq!(order.price, 105 * PRICE_SCALE, "the accepted price stands");
}

/// A rejection behind a cancel, with no replace outstanding, is the venue's
/// last word and finishes the order.
///
/// The guard beside it holds an order in flight while the venue still owes the
/// cancel an answer — but only where a replace is outstanding for the venue to
/// be answering. Held on the status alone, an order the venue refused the
/// cancel for waited for a verdict that was never coming: six paper phases
/// placed an order, cancelled it, and were told it had never been cancelled.
#[test]
fn a_rejection_with_no_replace_outstanding_finishes_the_order() {
    let mut ccp = CcpState::new();
    let mut context = Context::new();
    let shared = SharedState::new();
    let instrument = context.register_instrument(756733);
    context.insert_order(crate::types::Order::new(
        42, instrument, Side::Buy, 100 * crate::types::QTY_SCALE, 100 * PRICE_SCALE, b'2', b'0', 0,
    ));
    assert!(context.update_order_status(42, crate::types::OrderStatus::Submitted, false));
    assert!(context.update_order_status(42, crate::types::OrderStatus::PendingCancel, false));
    assert!(!context.replace_is_outstanding(42), "nothing was replaced");

    ccp.process_ccp_message(
        &crate::protocol::fix::fix_build(&[
            (fix::TAG_MSG_TYPE, fix::MSG_EXEC_REPORT),
            (11, "42"), (150, "8"), (39, "8"), (58, "the venue will not take this"),
        ], 1),
        &mut None, &mut context, &shared, &None, &mut HeartbeatState::new(), "DU1",
    );

    assert!(
        context.order(42).is_none(),
        "the venue answered about the order itself, so it is finished and let go",
    );
}

/// A holding closed while the connection was down.
///
/// The venue states every holding the account has while an account request is
/// open, so one it never names over the whole of that request is one the
/// account no longer has. The rows were only ever written over, never cleared,
/// so a holding that closed in the gap kept the size it had before the drop:
/// `req_positions` handed it back, the instrument slot behind `position` read
/// it, and every exposure decision was taken against stock nobody owned.
#[test]
fn a_holding_the_new_statement_never_names_is_closed() {
    let mut ccp = CcpState::new();
    let mut context = Context::new();
    let shared = SharedState::new();
    let mut hb = HeartbeatState::new();
    let kept = context.register_instrument(756733);
    let closed = context.register_instrument(140148322);

    // What the account held when the connection dropped.
    let holding = |con_id: i64, qty: f64| crate::types::PositionInfo {
        // With what it cost, so a close can be seen to take that with it.
        con_id, position: qty, avg_cost: 4 * PRICE_SCALE, unrealized_pnl: 9 * PRICE_SCALE,
        unrealized_stated: true, ..Default::default()
    };
    shared.portfolio.set_position_info(holding(756733, 300.0));
    shared.portfolio.set_position(kept, 300.0);
    context.update_position(kept, 300.0);
    shared.portfolio.set_position_info(holding(140148322, 3.0));
    shared.portfolio.set_position(closed, 3.0);
    context.update_position(closed, 3.0);
    // A third the venue will name without stating a quantity. An entry that
    // states no quantity states no quantity — it does not say the account is
    // flat — so naming it is what counts.
    let marks_only = context.register_instrument(479624278);
    shared.portfolio.set_position_info(holding(479624278, 7.0));
    shared.portfolio.set_position(marks_only, 7.0);
    context.update_position(marks_only, 7.0);
    shared.portfolio.drain_position_changes();

    let (conn, _peer) = crate::protocol::connection::Connection::for_test();
    let mut ccp_conn: Option<Connection> = None;
    ccp.reconnect(conn, &mut ccp_conn, &mut hb, "DU1", &shared);

    // The download a rebuilt connection carries arrives under the key its own
    // opening asked with, which the handshake sent before this loop saw the
    // connection.
    let asked_under = OPENING_ACCOUNT_REQUEST;

    // Another request's end says nothing about what this one has stated. A
    // caller can ask for a refresh at any moment, and reconciling against
    // whichever ended first would close every holding the account has.
    ccp.process_ccp_message(
        b"35=EB\x016529=AR.999\x01", &mut None, &mut context, &shared, &None, &mut hb, "DU1",
    );
    assert_eq!(
        shared.portfolio.position_info(140148322).map(|i| i.position), Some(3.0),
        "an end that is not this request's closes nothing",
    );

    // The new connection states one of the two, and then says it is done.
    let restated = format!(
        "35=UP\x016529={asked_under}\x016068=SPY\x016064=300\x016008=756733\x01",
    );
    ccp.process_ccp_message(
        restated.as_bytes(), &mut None, &mut context, &shared, &None, &mut hb, "DU1",
    );
    let named_without_a_quantity = format!(
        "35=UP\x016529={asked_under}\x016068=BTC\x016008=479624278\x016065=80533.4\x01",
    );
    ccp.process_ccp_message(
        named_without_a_quantity.as_bytes(),
        &mut None, &mut context, &shared, &None, &mut hb, "DU1",
    );

    let done = format!("35=EB\x016529={asked_under}\x01");
    ccp.process_ccp_message(
        done.as_bytes(), &mut None, &mut context, &shared, &None, &mut hb, "DU1",
    );

    let held = |con_id: i64| shared.portfolio.position_info(con_id)
        .map(|i| i.position)
        .unwrap_or_else(|| panic!("con {con_id} is still known"));
    assert_eq!(held(756733), 300.0, "the holding the venue restated stands");
    let gone = shared.portfolio.position_info(140148322).expect("still known");
    assert_eq!(gone.avg_cost, 0, "a row closing a holding takes its basis with it");
    assert_eq!(gone.unrealized_pnl, 0, "and what it was showing");
    assert!(!gone.unrealized_stated, "which is no longer a figure the venue stated");
    assert_eq!(held(140148322), 0.0, "the one it never named is gone");
    assert_eq!(shared.portfolio.position(closed), 0.0, "and so is its slot");
    assert_eq!(context.position(closed), 0.0, "and the book behind it");
    assert_eq!(context.position(kept), 300.0, "which the other one keeps");
    assert_eq!(
        held(479624278), 7.0,
        "the venue named this one and stated no quantity for it, which is not the \
         same as stating that it is flat",
    );
    let moves = shared.portfolio.drain_position_changes();
    assert!(
        moves.iter().any(|m| m.con_id == 140148322 && m.position == 0.0),
        "a caller watching holdings is told it went to nothing: {moves:?}",
    );
}

/// A revision the venue will not make reaches the surfaces as a refusal.
///
/// The venue refuses a revision on the report the order's own answers arrive
/// on, and the engine put its book back and said why in a message. The record
/// the surfaces read took the attempt ahead of that answer and had no way to
/// learn it had been turned down, so a caller was told the change did not
/// happen while their own book went on stating that it had.
#[test]
fn a_refused_revision_travels_on_the_channel_a_refusal_travels_on() {
    let mut ccp = CcpState::new();
    let mut context = Context::new();
    let shared = SharedState::new();
    let instrument = context.register_instrument(756733);
    context.insert_order(crate::types::Order::new(
        42, instrument, Side::Buy,
        100 * crate::types::QTY_SCALE, 150 * crate::types::PRICE_SCALE,
        b'2', b'0', 0,
    ));
    assert!(context.update_order_status(42, crate::types::OrderStatus::Submitted, false));
    let before = *context.order(42).expect("tracked");
    context.pre_replace.insert((42, 1), (before, "42.0".to_string(), None));
    let refused = exec_report_frame(&[
        (11, "42.1"), (150, "8"), (39, "0"), (378, "102"),
        (58, "the price is through the band"),
    ]);
    ccp.handle_exec_report(&refused, b"", &mut context, &shared, &None, "DU1");

    let refusals = shared.orders.drain_cancel_rejects();
    assert_eq!(refusals.len(), 1, "one refusal reaches the surfaces: {refusals:?}");
    assert_eq!(refusals[0].order_id, 42);
    assert_eq!(refusals[0].reject_type, 2, "it is the revision the venue refused");
    assert_eq!(
        refusals[0].still_working, Some(crate::types::OrderStatus::Submitted),
        "and the order stands as it was",
    );
    let said = shared.orders.drain_order_inactive();
    assert!(
        said.iter().any(|(id, _, why)| *id == 42 && why.contains("band")),
        "the venue's own words still reach the caller: {said:?}",
    );
}

/// A refusal that arrives while the order is uncertain still puts the terms
/// the venue holds back.
///
/// A drop marks every order uncertain, and a report on an uncertain order is
/// taken as the venue naming what it holds — which reconciles the revision
/// and drops the fallbacks kept against a refusal. A refusal of the revision
/// outstanding at the drop is not the venue naming anything: taken as one,
/// it wiped its own fallback before the handler that needed it ran, the
/// record kept the refused terms, the name moved to the refused revision, and
/// the caller was told the refusal answered no change of theirs.
#[test]
fn a_refusal_arriving_while_the_order_is_uncertain_still_puts_the_terms_back() {
    let mut ccp = CcpState::new();
    let mut context = Context::new();
    let shared = SharedState::new();
    let instrument = context.register_instrument(756733);
    context.insert_order(crate::types::Order::new(
        42, instrument, Side::Buy, 100 * crate::types::QTY_SCALE, 150 * crate::types::PRICE_SCALE, b'2', b'0', 0,
    ));
    assert!(context.update_order_status(42, crate::types::OrderStatus::Submitted, false));
    let before = *context.order(42).expect("tracked");
    // The attempt: revision 1 at 151, recorded ahead of the answer.
    context.pre_replace.insert((42, 1), (before, "42.0".to_string(), None));
    context.modify_versions.insert(42, 1);
    context.last_clord.insert(42, "42.1".to_string());
    let mut attempt = before;
    attempt.price = 151 * crate::types::PRICE_SCALE;
    attempt.status = crate::types::OrderStatus::PendingReplace;
    context.insert_order(attempt);
    // The connection goes, and every order with it.
    context.mark_orders_uncertain();

    let refused = exec_report_frame(&[
        (11, "42.1"), (150, "8"), (39, "0"), (378, "102"), (54, "1"), (38, "100"), (6008, "756733"),
        (58, "the price is through the band"),
    ]);
    ccp.handle_exec_report(&refused, b"", &mut context, &shared, &None, "DU1");

    let order = context.order(42).expect("still tracked");
    assert_eq!(order.price, 150 * crate::types::PRICE_SCALE, "the terms the venue holds are back");
    assert_eq!(context.last_clord.get(&42).map(String::as_str), Some("42.0"), "and so is the name it holds");
    assert!(context.pre_replace.is_empty(), "the fallback was spent on the refusal, not wiped ahead of it");
    let refusals = shared.orders.drain_cancel_rejects();
    assert!(refusals.iter().any(|r| r.order_id == 42 && r.answers_a_live_change), "{refusals:?}");
}

/// An accepted revision spends the fallbacks of every revision below it.
///
/// Two revisions can be outstanding at once. The venue accepting the second
/// spent the second's fallback alone, so a refusal of the first arriving
/// behind the acceptance put back the terms from before the first — over the
/// terms the venue had just accepted — and moved the name back two revisions,
/// so the next cancel named a revision the venue had superseded.
#[test]
fn an_accepted_revision_spends_the_fallbacks_below_it() {
    let mut ccp = CcpState::new();
    let mut context = Context::new();
    let shared = SharedState::new();
    let instrument = context.register_instrument(756733);
    let at = |price: i64| {
        let mut o = crate::types::Order::new(42, instrument, Side::Buy, 100 * crate::types::QTY_SCALE, price * crate::types::PRICE_SCALE, b'2', b'0', 0);
        o.status = crate::types::OrderStatus::Submitted;
        o
    };
    context.insert_order(at(150));
    context.pre_replace.insert((42, 1), (at(150), "42.0".to_string(), None));
    context.pre_replace.insert((42, 2), (at(151), "42.1".to_string(), None));
    context.modify_versions.insert(42, 2);
    context.last_clord.insert(42, "42.2".to_string());
    let mut pending = at(152);
    pending.status = crate::types::OrderStatus::PendingReplace;
    context.insert_order(pending);

    let accepted = exec_report_frame(&[(11, "42.2"), (41, "42.1"), (150, "5"), (39, "5"), (44, "152"), (54, "1"), (38, "100"), (6008, "756733")]);
    ccp.handle_exec_report(&accepted, b"", &mut context, &shared, &None, "DU1");
    let refused = exec_report_frame(&[(11, "42.1"), (150, "8"), (39, "0"), (378, "102"), (58, "through the band")]);
    ccp.handle_exec_report(&refused, b"", &mut context, &shared, &None, "DU1");

    let order = context.order(42).expect("tracked");
    assert_eq!(order.price, 152 * crate::types::PRICE_SCALE, "the accepted terms stand");
    assert_eq!(context.last_clord.get(&42).map(String::as_str), Some("42.2"), "and the accepted name");
    assert!(context.pre_replace.is_empty(), "nothing is left to put back: {:?}", context.pre_replace.keys().collect::<Vec<_>>());
}

/// The venue taking a replacement is said even when a fill took the status.
///
/// The surfaces keep the terms an order had before a replacement, to put back
/// where the venue refuses it. They read the acceptance off a status — the
/// venue working the order again — and the venue fills the order it holds
/// while it is still deciding: the fill takes the more advanced status, the
/// acknowledgement behind it is dropped as stale, and nothing announces it. So
/// the copy outlived the replacement the venue had taken, and the next refusal
/// put back terms from before it.
#[test]
fn the_venue_taking_a_replacement_is_said_behind_a_fill() {
    let mut ccp = CcpState::new();
    let mut context = Context::new();
    let shared = SharedState::new();
    let instrument = context.register_instrument(756733);
    context.insert_order(crate::types::Order::new(
        42, instrument, Side::Buy,
        100 * crate::types::QTY_SCALE, 150 * crate::types::PRICE_SCALE,
        b'2', b'0', 0,
    ));
    // Staged as the builder stages one: the terms the venue holds, under the
    // revision the change goes out as.
    let before = *context.order(42).expect("tracked");
    context.pre_replace.insert((42, 1), (before, "42.0".to_string(), None));
    assert!(context.update_order_status(42, crate::types::OrderStatus::PendingReplace, false));

    // Part of it fills while the venue is still deciding on the change.
    let filled = exec_report_frame(&[
        (11, "42.1"), (150, "1"), (39, "1"), (54, "1"), (38, "100"), (14, "10"), (151, "90"),
    ]);
    ccp.handle_exec_report(&filled, b"", &mut context, &shared, &None, "DU1");
    assert_eq!(
        context.order(42).map(|o| o.status),
        Some(crate::types::OrderStatus::PartiallyFilled),
        "the fill is the order's most advanced state",
    );

    // And then the venue takes the change.
    let took = exec_report_frame(&[(11, "42.1"), (150, "5"), (39, "5")]);
    ccp.handle_exec_report(&took, b"", &mut context, &shared, &None, "DU1");

    assert_eq!(
        shared.orders.drain_replacements_taken(), vec![42],
        "the surfaces are told the venue took it",
    );
    assert_eq!(
        context.order(42).map(|o| o.status),
        Some(crate::types::OrderStatus::PartiallyFilled),
        "and the order is still what the fill left it",
    );
}

/// A refusal of a revision the venue has already answered says nothing about
/// where the order stands now.
///
/// The venue takes a second revision before it has answered the first, and a
/// cancel can be sent over both. The refusal of a revision already answered
/// used to force the order back to working whatever had happened since: a
/// caller who had withdrawn the order was told it was working again, and the
/// cancel's own answer only arrived later.
#[test]
fn a_refused_revision_the_venue_has_answered_does_not_revive_the_order() {
    let mut ccp = CcpState::new();
    let mut context = Context::new();
    let shared = SharedState::new();
    let instrument = context.register_instrument(756733);
    context.insert_order(crate::types::Order::new(
        42, instrument, Side::Buy,
        100 * crate::types::QTY_SCALE, 100 * PRICE_SCALE, b'2', b'0', 0,
    ));
    // Withdrawn, and the venue has not answered the withdrawal yet.
    assert!(context.update_order_status(42, crate::types::OrderStatus::PendingCancel, false));

    // A refusal of a revision nothing is waiting on — the venue answered it
    // before the cancel went out.
    let mut refused = std::collections::HashMap::new();
    refused.insert(41u32, "42.1".to_string());
    refused.insert(11u32, "42.1".to_string());
    refused.insert(434u32, "2".to_string());
    refused.insert(102u32, "0".to_string());
    ccp.handle_cancel_reject(&refused, &mut context, &shared, &None);

    assert_eq!(
        context.order(42).map(|o| o.status),
        Some(crate::types::OrderStatus::PendingCancel),
        "the order is still the one the caller withdrew",
    );
    let updates = shared.orders.drain_order_updates();
    assert!(
        !updates.iter().any(|u| u.order_id == 42
            && matches!(u.status, crate::types::OrderStatus::Submitted)),
        "and nobody is told it is working again: {updates:?}",
    );
}

/// And a refusal of a revision the venue IS holding does not revive it either.
///
/// That is the interleaving the case above stops short of: a replace goes out,
/// the caller cancels over it — which is allowed, and the guard raises pending
/// cancel — and only then does the venue refuse the live revision. Restoring
/// the terms wrote the snapshot's status back into the book as well, which is
/// not a status the guard was asked about: it went straight past it, and the
/// caller was told its withdrawn order was working while the cancel was still
/// in flight.
#[test]
fn a_refused_revision_does_not_revive_an_order_cancelled_over_it() {
    let mut ccp = CcpState::new();
    let mut context = Context::new();
    let shared = SharedState::new();
    let instrument = context.register_instrument(756733);
    context.insert_order(crate::types::Order::new(
        42, instrument, Side::Buy,
        100 * crate::types::QTY_SCALE, 100 * PRICE_SCALE, b'2', b'0', 0,
    ));
    assert!(context.update_order_status(42, crate::types::OrderStatus::Submitted, false));
    // The replace goes out, and the terms before it are kept to fall back on.
    let before = *context.order(42).expect("the order is tracked");
    context.pre_replace.insert((42, 1), (before, "42.0".to_string(), None));
    // The caller withdraws over it.
    assert!(context.update_order_status(42, crate::types::OrderStatus::PendingCancel, false));

    // Now the venue refuses the revision it was holding.
    let mut refused = std::collections::HashMap::new();
    refused.insert(41u32, "42.1".to_string());
    refused.insert(11u32, "42.1".to_string());
    refused.insert(434u32, "2".to_string());
    refused.insert(102u32, "0".to_string());
    ccp.handle_cancel_reject(&refused, &mut context, &shared, &None);

    assert_eq!(
        context.order(42).map(|o| o.status),
        Some(crate::types::OrderStatus::PendingCancel),
        "the terms go back; what has happened to the order does not",
    );
}

/// And the same refusal carried on an execution report, which is the other
/// half of it.
///
/// The venue states a refused change two ways: as a cancel reject, and as an
/// execution report naming the refusal on its own tag. The first was fixed;
/// this is the second. The report carries the order's terms as they stand, and
/// the guard reads a working status as the order working again — because that
/// is what it means everywhere else — so the withdrawal the caller had been
/// told about was undone, and the flag that suppresses the announcement left
/// the book saying it anyway.
#[test]
fn a_refused_revision_on_an_execution_report_does_not_revive_a_cancelled_order() {
    let mut ccp = CcpState::new();
    let mut context = Context::new();
    let shared = SharedState::new();
    let instrument = context.register_instrument(756733);
    context.insert_order(crate::types::Order::new(
        42, instrument, Side::Buy,
        100 * crate::types::QTY_SCALE, 100 * PRICE_SCALE, b'2', b'0', 0,
    ));
    assert!(context.update_order_status(42, crate::types::OrderStatus::Submitted, false));
    let before = *context.order(42).expect("the order is tracked");
    context.pre_replace.insert((42, 1), (before, "42.0".to_string(), None));
    assert!(context.update_order_status(42, crate::types::OrderStatus::PendingCancel, false));

    // 378=102 is the venue refusing the change; 39=0 states the order's terms
    // as they stand, which reads as working.
    let refused = crate::protocol::fix::fix_build(&[
        (fix::TAG_MSG_TYPE, fix::MSG_EXEC_REPORT),
        (11, "42.1"), (150, "0"), (39, "0"), (378, "102"),
    ], 1);
    ccp.process_ccp_message(
        &refused, &mut None, &mut context, &shared, &None, &mut HeartbeatState::new(), "DU1",
    );

    assert_eq!(
        context.order(42).map(|o| o.status),
        Some(crate::types::OrderStatus::PendingCancel),
        "the refusal answers the change, not the cancel still in flight",
    );
}

/// A cancel the venue refuses because the order already filled does not put
/// the order back to working.
///
/// The reject states where the order stands, and a refused cancellation is
/// very often refused for exactly that reason. Read past it, the restore put a
/// finished order back to working and did none of the cleanup finishing does,
/// so the caller held a live order the venue had already filled — and a
/// withdrawal of everything would go on trying to cancel it.
#[test]
fn a_cancel_refused_because_the_order_filled_leaves_it_filled() {
    let mut ccp = CcpState::new();
    let mut context = Context::new();
    let shared = SharedState::new();
    let instrument = context.register_instrument(756733);
    context.insert_order(crate::types::Order::new(
        42, instrument, Side::Buy,
        100 * crate::types::QTY_SCALE, 100 * PRICE_SCALE, b'2', b'0', 0,
    ));
    assert!(context.update_order_status(42, crate::types::OrderStatus::Submitted, false));
    assert!(context.update_order_status(42, crate::types::OrderStatus::PendingCancel, false));

    // 434=1 refuses the cancellation; 102=0 is "too late"; 39=2 says why.
    let mut refused = std::collections::HashMap::new();
    refused.insert(41u32, "42".to_string());
    refused.insert(434u32, "1".to_string());
    refused.insert(102u32, "0".to_string());
    refused.insert(39u32, "2".to_string());
    ccp.handle_cancel_reject(&refused, &mut context, &shared, &None);

    assert_ne!(
        context.order(42).map(|o| o.status),
        Some(crate::types::OrderStatus::Submitted),
        "an order the venue says is filled is not reported working",
    );
}


/// An accepted replace on an order that filled before the answer landed is
/// still announced.
///
/// The venue reports a partly filled working order as submitted — the two
/// quantities carry the distinction — so an acknowledgement stating submitted,
/// on a book holding partly filled, is the same status stated twice. Compared
/// as enums it read as two, and the acknowledgement announced nothing: no
/// status reached either surface, and the terms cached under the order stayed
/// the ones from before the replace.
#[test]
fn a_replace_accepted_on_a_partly_filled_order_is_announced() {
    let mut ccp = CcpState::new();
    let mut context = Context::new();
    let shared = SharedState::new();
    let instrument = context.register_instrument(756733);
    context.insert_order(crate::types::Order::new(
        42, instrument, Side::Buy,
        100 * crate::types::QTY_SCALE, 100 * PRICE_SCALE, b'2', b'0', 0,
    ));
    assert!(context.update_order_status(42, crate::types::OrderStatus::Submitted, false));
    // A fill lands before the venue answers the replace.
    assert!(context.update_order_status(42, crate::types::OrderStatus::PartiallyFilled, false));
    let _ = shared.orders.drain_order_updates();

    // 150=5 / 39=5 is the venue accepting the replace.
    let ack = crate::protocol::fix::fix_build(&[
        (fix::TAG_MSG_TYPE, fix::MSG_EXEC_REPORT),
        (11, "42"), (150, "5"), (39, "5"), (6008, "756733"),
    ], 1);
    ccp.process_ccp_message(
        &ack, &mut None, &mut context, &shared, &None, &mut HeartbeatState::new(), "DU1",
    );

    assert!(
        shared.orders.drain_order_updates().iter().any(|u| u.order_id == 42),
        "the caller hears that the replace was taken",
    );
}

/// A caller waiting on the download is let through to a squared account.
///
/// The flag that says the download is over used to be the first thing the
/// completion did, and the squaring — closing every holding the download never
/// named — followed it. A caller spinning on that flag from its own thread woke
/// inside that window and was handed the holding the squaring was about to
/// close, which is the answer the squaring exists to prevent.
#[test]
fn the_download_reads_as_over_only_once_it_is_squared() {
    let shared = SharedState::new();
    shared.portfolio.set_position_info(crate::types::PositionInfo {
        con_id: 756733, position: 300.0, ..Default::default()
    });
    shared.portfolio.account_download_is_pending();
    shared.portfolio.holdings_restated_under("AR.4");

    let unstated = shared.portfolio.set_account_download_complete("AR.4");
    assert_eq!(unstated, Some(vec![756733]), "the venue named none of it");
    assert!(
        !shared.portfolio.account_download_complete(),
        "and the download does not read as over while the caller still has work",
    );

    shared.portfolio.account_download_is_settled();
    assert!(shared.portfolio.account_download_complete(), "now it does");
}

/// A connection that dies takes the account's download with it.
///
/// The download was marked pending when the next connection arrived, so
/// through the whole of a backoff — or for ever, where the retries ran out —
/// the flag still said the venue had finished stating what the account holds.
/// A caller asking was answered at once from the book as it stood before the
/// drop, with nothing to say the venue had not been heard from since.
#[test]
fn a_connection_that_dies_takes_the_download_with_it() {
    let mut ccp = CcpState::new();
    let mut context = Context::new();
    let shared = SharedState::new();
    shared.portfolio.holdings_restated_under("AR.1");
    shared.portfolio.set_account_download_complete("AR.1");
    shared.portfolio.account_download_is_settled();
    assert!(shared.portfolio.account_download_complete(), "the session had one");

    let mut ccp_conn: Option<Connection> = None;
    ccp.handle_disconnect(&mut ccp_conn, &mut context, &shared, &None);

    assert!(
        !shared.portfolio.account_download_complete(),
        "and nothing has been stated about the account since it died",
    );
}

/// A connection that dies takes every lookup waiting on it with it, and
/// says so now.
///
/// The connection that replaces it is asked nothing this one was asked, so
/// a lookup outstanding at the drop could only run its deadline out — and it
/// was then reported as a request the venue never answered, or a contract it
/// does not know, ten to twenty seconds after the connection went. The
/// historical connection has failed its own at once all along.
#[test]
fn a_connection_that_dies_takes_the_lookups_waiting_on_it_with_it() {
    let (mut ccp, mut context, shared) = u186_test_state();
    let later = Instant::now() + Duration::from_secs(30);
    ccp.pending_secdef.push((7, false, later));
    ccp.pending_fanout.push(PendingFanout {
        api_req_id: 8,
        fanout_req_ids: vec!["ibxfan-8-0".into(), "ibxfan-8-1".into()],
        answered: vec!["ibxfan-8-0".into()],
        deadline: later,
    });
    ccp.details_delivered.entry(8).or_default().insert(756_733);
    ccp.pending_matching_symbols.push((9, later));
    ccp.pending_option_params.push((10, "AAPL".into(), 265_598, later));
    ccp.pending_schedule_pair.push(PendingSchedulePair {
        api_req_id: 11,
        join_key: "AAPL-NASDAQ".into(),
        def: crate::control::contracts::ContractDefinition { con_id: 265_598, ..Default::default() },
        is_last: true,
        deadline: later,
    });
    // A lookup of the engine's own for a holding, and a subscription and a
    // request each waiting on the naming of their contract.
    ccp.pending_secdef.push((0xF000_0001, true, later));
    ccp.auto_fetched_conids.insert(4_762, 0xF000_0001);
    // A scan parked behind the naming of its rows, which those lookups were
    // asking for.
    ccp.pending_scanner_enrichment.push(PendingScannerEnrichment {
        api_req_id: 13,
        result: crate::control::scanner::ScannerResult {
            con_ids: vec![4_762], entries: Vec::new(), scan_time: String::new(), error_text: String::new(),
        },
        awaiting: [4_762i64].into_iter().collect(),
        deadline: later,
    });
    ccp.resolve_for_subscribe(PendingSubscribe {
        issued: 0,
        filters: Default::default(),
        con_id: 0, instrument: 4, symbol: "SPY".into(), exchange: "SMART".into(),
        sec_type: "STK".into(), currency: "USD".into(),
        mode_9887: 0, regulatory_snapshot: false,
    }, &mut None, &mut HeartbeatState::new(), &shared);
    let bars = crate::types::ControlCommand::FetchHistorical {
        contract: crate::types::ContractRef { con_id: 0, symbol: "SPY".into(), sec_type: "STK".into(), exchange: "SMART".into(), currency: "USD".into(), ..Default::default() },
        req_id: 12, end_date_time: String::new(), duration: "1 D".into(), bar_size: "1 hour".into(),
        what_to_show: "TRADES".into(), use_rth: true, keep_up_to_date: false, include_expired: false,
        filters: Default::default(),
    };
    assert!(ccp.hold_until_named(bars, &mut None, &mut HeartbeatState::new(), &shared).is_none());
    assert_eq!((ccp.pending_md_subscribe.len(), ccp.pending_named.len()), (1, 1));

    ccp.handle_disconnect(&mut None, &mut context, &shared, &None);

    assert!(
        ccp.pending_secdef.is_empty() && ccp.pending_fanout.is_empty()
            && ccp.pending_matching_symbols.is_empty() && ccp.pending_option_params.is_empty()
            && ccp.pending_schedule_pair.is_empty() && ccp.pending_md_subscribe.is_empty()
            && ccp.pending_named.is_empty() && ccp.auto_fetched_conids.is_empty()
            && ccp.details_delivered.is_empty() && ccp.pending_scanner_enrichment.is_empty(),
        "nothing waits on a connection that is gone",
    );
    let gone = crate::error_codes::Refusal::NOT_CONNECTED;
    let mut told: Vec<(u32, i32)> = shared.reference.drain_historical_errors().into_iter()
        .inspect(|(_, _, why)| assert!(why.contains("trading connection"), "{why}"))
        .map(|(rid, code, _)| (rid, code)).collect();
    told.sort_unstable();
    assert_eq!(
        told, [(7, gone), (8, gone), (9, gone), (10, gone), (12, gone)],
        "each caller told now, and the engine's own lookup told to nobody",
    );
    let mut ended = shared.reference.drain_contract_details_end();
    ended.sort_unstable();
    assert_eq!(ended, [7, 8, 11], "each details request ended");
    let paired: Vec<_> = shared.reference.drain_contract_details().into_iter()
        .map(|(rid, d)| (rid, d.con_id, d.trading_hours.is_none())).collect();
    assert_eq!(paired, [(11, 265_598, true)], "a contract the venue did name is delivered, without the hours it did not");
    let failed = shared.market.drain_subscription_failures();
    assert!(failed.len() == 1 && failed[0].0 == 4 && failed[0].1.contains("trading connection"), "{failed:?}");
    assert_eq!(context.slots_to_reconsider, [4], "and the slot goes back");
}

/// The end that squares the account is the end of the request that was asked
/// to state it.
///
/// A caller can ask for a refresh at any moment, and a later request taking
/// over left the earlier one's unstated holdings to be settled by an end that
/// was never asked to state them — so holdings the account still had, which
/// the first request had simply not reached yet, were closed on the strength
/// of the second.
#[test]
fn a_later_request_does_not_settle_an_earlier_one() {
    let shared = SharedState::new();
    let held = |con_id: i64| crate::types::PositionInfo {
        con_id, position: 5.0, ..Default::default()
    };
    shared.portfolio.set_position_info(held(756733));
    shared.portfolio.set_position_info(held(140148322));
    shared.portfolio.account_download_is_pending();
    shared.portfolio.holdings_restated_under("AR.5");
    // A caller asks for a refresh while the first request is still arriving.
    shared.portfolio.holdings_restated_under("AR.6");

    // The refresh's end says nothing about what the first request has stated.
    assert_eq!(
        shared.portfolio.set_account_download_complete("AR.6"), None,
        "an end that is not the asked request's squares nothing, and says so",
    );
    assert!(
        !shared.portfolio.account_download_complete(),
        "and the download is not over while the request that was asked is out",
    );

    // The venue names one of them, and then the asked request ends.
    shared.portfolio.note_restated(756733);
    assert_eq!(
        shared.portfolio.set_account_download_complete("AR.5"), Some(vec![140148322]),
        "only what the request that was asked never named",
    );
}

/// An end that squares nothing does not declare the download over.
///
/// The guard that decides which end belongs to the outstanding request lives
/// in one place, and the caller owns the rest of the squaring — the instrument
/// slots and the engine's own book. Declaring the download over beside that
/// loop rather than inside it undid the guard entirely: every end set the
/// flag, including the ones the guard had just refused to act on, and a caller
/// waiting on it read the holdings the squaring was about to close.
#[test]
fn an_end_that_squares_nothing_does_not_end_the_download() {
    let mut ccp = CcpState::new();
    let mut context = Context::new();
    let shared = SharedState::new();
    shared.portfolio.set_position_info(crate::types::PositionInfo {
        con_id: 756733, position: 300.0, ..Default::default()
    });
    shared.portfolio.account_download_is_pending();
    shared.portfolio.holdings_restated_under("AR.5");

    // Some other request's end arrives first.
    ccp.process_ccp_message(
        b"35=EB\x016529=AR.6\x01", &mut None, &mut context, &shared, &None,
        &mut HeartbeatState::new(), "DU1",
    );

    assert!(
        !shared.portfolio.account_download_complete(),
        "the download is not over on an end that squared nothing",
    );
    assert_eq!(
        shared.portfolio.position_info(756733).map(|i| i.position), Some(300.0),
        "and nothing was closed on the strength of it",
    );
}

/// A rebuilt connection's download arrives under the key its own opening asked
/// with, which is the key a first logon uses.
///
/// The handshake sends that opening before the loop is handed the connection,
/// so the reconnect drawing a key from its counter and recording that one left
/// the end that carries the account belonging to neither: the account was
/// never squared, and — where the venue ends the counter's key with nothing,
/// because the first is already serving — every holding the account has would
/// have been closed against a request that stated none of them.
#[test]
fn a_rebuilt_connection_settles_under_the_key_its_opening_asked_with() {
    let mut ccp = CcpState::new();
    let shared = SharedState::new();
    let mut hb = HeartbeatState::new();
    shared.portfolio.set_position_info(crate::types::PositionInfo {
        con_id: 756733, position: 300.0, ..Default::default()
    });

    let (conn, _peer) = crate::protocol::connection::Connection::for_test();
    let mut ccp_conn: Option<Connection> = None;
    ccp.reconnect(conn, &mut ccp_conn, &mut hb, "DU1", &shared);

    assert_eq!(
        shared.portfolio.set_account_download_complete(OPENING_ACCOUNT_REQUEST),
        Some(vec![756733]),
        "the end of the opening request is the one that squares the account",
    );
    // And the counter's second subscribe measures nothing, so its own end
    // cannot square an account it was never asked to state.
    let counter_key = ccp.account_request_key.clone().expect("a second key was drawn");
    assert_ne!(counter_key, OPENING_ACCOUNT_REQUEST);
}

/// Every id the venue names raises the mark the next number is counted from,
/// including one it only ever mentions in passing.
///
/// An order that partly filled and was then withdrawn is not in the working
/// set a session opens with; the venue names it by replaying its execution
/// behind everything else. That record is deliberately not kept — it is not an
/// open order and it is not a completion — and the mark used to rise only as a
/// consequence of keeping it. So the id that order spent was the one id the
/// next session did not count past, and the venue refuses an order under an id
/// a fill has spent.
#[test]
fn an_id_the_venue_only_mentions_still_counts() {
    let mut ccp = CcpState::new();
    let mut context = Context::new();
    let shared = SharedState::new();

    // The replayed history of an order this session never placed: a partial
    // fill, marked as a restatement, and no completion behind it.
    let history = exec_report_frame(&[
        (11, "7000"), (150, "1"), (39, "1"), (97, "Y"), (54, "1"),
        (6008, "756733"), (38, "100"), (14, "10"), (151, "90"),
    ]);
    ccp.handle_exec_report(&history, b"", &mut context, &shared, &None, "DU1");

    assert!(
        shared.orders.get_order_info(7000).is_none(),
        "the record itself is not kept: it is neither working nor completed",
    );
    assert_eq!(
        shared.orders.working_id_watermark(), 7000,
        "but the id it spent is counted past all the same",
    );
}

/// A withdrawal reaches a request still waiting to be named.
///
/// A request naming its contract by symbol is parked whole while the venue is
/// asked what that contract is, and in that window it is in neither the
/// in-flight record nor the held one. The withdrawal found nothing, sent
/// nothing and returned; the naming answer then arrived, the request was
/// re-injected and sent, and the caller was served a full answer -- bars and
/// an end -- to a request it had cancelled.
#[test]
fn a_request_waiting_to_be_named_is_withdrawn_with_the_rest() {
    let shared = SharedState::new();
    let mut ccp = CcpState::new();

    let named_by_symbol = |req_id: u32| crate::types::ControlCommand::FetchHistorical {
        contract: crate::types::ContractRef {
            con_id: 0,
            symbol: "AAPL".into(),
            sec_type: "STK".into(),
            exchange: "SMART".into(),
            currency: "USD".into(),
            ..Default::default()
        },
        req_id,
        end_date_time: String::new(),
        duration: "1 D".into(),
        bar_size: "1 hour".into(),
        what_to_show: "TRADES".into(),
        use_rth: true,
        keep_up_to_date: false,
        include_expired: false,
        filters: Default::default(),
    };

    ccp.hold_until_named(named_by_symbol(7), &mut None, &mut HeartbeatState::new(), &shared);
    ccp.hold_until_named(named_by_symbol(8), &mut None, &mut HeartbeatState::new(), &shared);
    assert_eq!(ccp.pending_named.len(), 2, "both are waiting on a name");
    // And one whose name already came back, waiting to be read this pass.
    ccp.resolved_named.push(named_by_symbol(7));

    ccp.withdraw_named(7);

    assert!(
        !ccp.pending_named.iter().any(|(_, cmd, _)| request_id(cmd) == Some(7)),
        "the withdrawn request does not go out once its contract is named",
    );
    assert!(
        !ccp.resolved_named.iter().any(|cmd| request_id(cmd) == Some(7)),
        "nor does one already named and waiting to be read",
    );
    assert_eq!(
        ccp.pending_named.len(), 1,
        "and the request the caller did not withdraw is still waiting",
    );
}

/// A holding the venue closes keeps no value and no profit.
///
/// The venue states a close as an explicit zero, and the frame that states it
/// carries no marks. The marks are owned by the writer that reads them, so the
/// row kept the last value and the last unrealised figure it had -- and a
/// caller reading its profit on a position, or its portfolio, was shown both
/// against stock nobody owns. Only what the venue stops naming altogether was
/// squared, at the end of a download; a close it names was not.
#[test]
fn a_holding_the_venue_closes_keeps_no_value_and_no_profit() {
    let mut ccp = CcpState::new();
    let mut context = Context::new();
    let shared = SharedState::new();
    let mut hb = HeartbeatState::new();
    context.market.register(265598);

    // Held, priced, and showing a profit.
    ccp.handle_position_feed(
        "6008=265598\x016064=100\x016101=150.0\x01".as_bytes(),
        &mut None, &mut context, &shared, &None, &mut hb,
    );
    shared.portfolio.set_position_marks(
        265598,
        Some(160 * crate::types::PRICE_SCALE),
        Some(16_000 * crate::types::PRICE_SCALE),
        Some(1_000 * crate::types::PRICE_SCALE),
        None,
    );
    let held = shared.portfolio.position_info(265598).expect("held");
    assert_eq!(held.market_value, 16_000 * crate::types::PRICE_SCALE);
    assert!(held.unrealized_stated);

    // The venue closes it, and says nothing about what it is worth.
    ccp.handle_position_feed(
        "6008=265598\x016064=0\x01".as_bytes(),
        &mut None, &mut context, &shared, &None, &mut hb,
    );

    let gone = shared.portfolio.position_info(265598).expect("still known");
    assert_eq!(gone.position, 0.0, "the holding is closed");
    assert_eq!(gone.market_value, 0, "and is worth nothing, not what it last was");
    assert_eq!(gone.market_price, 0, "with no price standing against it");
    assert_eq!(gone.unrealized_pnl, 0, "and no profit on stock nobody owns");
    assert!(!gone.unrealized_stated, "which is unstated rather than stated as zero");
}

// ---------------------------------------------------------------------------
// Lookups the engine makes on its own account, and lookups that did not
// reach the venue.

/// The lookup a subscription makes asks what the caller named the contract by.
///
/// A symbol, a month, a strike and a right name three contracts on an index
/// future's options and one of them once the class is stated. Every other
/// request that names a contract by description carries what narrows it; this
/// one dropped the class, the local name and the listing venue on the way to
/// the lookup, so the caller was told its contract matched several and no
/// stream could be opened for it.
#[test]
fn a_subscription_looks_up_the_listing_the_caller_named() {
    use std::io::Read;
    let (conn, mut peer) = crate::protocol::connection::Connection::for_test();
    let mut ccp = CcpState::new();
    let shared = SharedState::new();
    let mut hb = HeartbeatState::new();
    let mut conn = Some(conn);
    ccp.resolve_for_subscribe(
        PendingSubscribe {
            issued: 0,
            con_id: 0,
            instrument: 5,
            symbol: "ES".into(),
            exchange: "CME".into(),
            sec_type: "FOP".into(),
            currency: "USD".into(),
            filters: crate::types::SecDefFilters {
                last_trade_date_or_contract_month: "202612".into(),
                strike: 7700.0,
                right: "C".into(),
                trading_class: "ES".into(),
                ..Default::default()
            },
            mode_9887: 0,
            regulatory_snapshot: false,
        },
        &mut conn, &mut hb, &shared,
    );

    let mut buf = [0u8; 4096];
    let n = peer.read(&mut buf).unwrap();
    let msg = String::from_utf8_lossy(&buf[..n]).replace('\u{1}', "|");
    assert!(msg.contains("|6058=ES|"), "the class the caller named: {msg}");
    assert!(msg.contains("|201=1|"), "and that it is a call: {msg}");
    assert!(msg.contains("|202=7700"), "and what it may be exercised at: {msg}");
}

fn spy_by_symbol(instrument: crate::types::InstrumentId) -> PendingSubscribe {
    PendingSubscribe {
        issued: 0,
        filters: Default::default(),
        con_id: 0, instrument,
        symbol: "SPY".into(), exchange: "SMART".into(), sec_type: "STK".into(), currency: "USD".into(),
        mode_9887: 0, regulatory_snapshot: false,
    }
}

fn head_timestamp_by_symbol(req_id: u32) -> crate::types::ControlCommand {
    crate::types::ControlCommand::FetchHeadTimestamp {
        req_id,
        include_expired: false,
        contract: crate::types::ContractRef {
            symbol: "SPY".into(), sec_type: "STK".into(), exchange: "SMART".into(), currency: "USD".into(),
            ..Default::default()
        },
        what_to_show: "TRADES".into(), use_rth: true, filters: Default::default(),
    }
}

/// One definition, naming the venues the contract trades on: the reply a
/// lookup by symbol draws, which a caller's lookup follows with one lookup
/// per venue named.
fn named_on_many_venues(req_id: &str) -> Vec<u8> {
    crate::protocol::fix::fix_build(&[
        (fix::TAG_MSG_TYPE, "d"),
        (crate::control::contracts::TAG_SECURITY_REQ_ID, req_id),
        (crate::control::contracts::TAG_SECURITY_RESPONSE_TYPE, "4"),
        (55, "SPY"), (167, "CS"),
        (crate::control::contracts::TAG_IB_CON_ID, "756733"),
        (crate::control::contracts::TAG_IB_VALID_EXCHANGES, "ISLAND,ARCA,NYSE"),
    ], 1)
}

/// One reply naming two listings of a symbol: what a symbol stated without a
/// currency draws.
fn named_twice(req_id: &str) -> Vec<u8> {
    crate::protocol::fix::fix_build(&[
        (fix::TAG_MSG_TYPE, "d"),
        (crate::control::contracts::TAG_SECURITY_REQ_ID, req_id),
        (crate::control::contracts::TAG_SECURITY_RESPONSE_TYPE, "4"),
        (55, "SPY"), (167, "CS"), (crate::control::contracts::TAG_IB_CON_ID, "756733"), (15, "USD"),
        (55, "SPY"), (167, "CS"), (crate::control::contracts::TAG_IB_CON_ID, "90016213"), (15, "MXN"),
    ], 1)
}

fn named_by_id(req_id: &str) -> Vec<u8> {
    crate::protocol::fix::fix_build(&[
        (fix::TAG_MSG_TYPE, "d"),
        (crate::control::contracts::TAG_SECURITY_REQ_ID, req_id),
        (crate::control::contracts::TAG_SECURITY_RESPONSE_TYPE, "4"),
        (55, "SPY"), (167, "CS"),
        (crate::control::contracts::TAG_IB_CON_ID, "756733"),
    ], 1)
}

/// A naming lookup of the engine's own — for a subscription or a request
/// that stated its contract by symbol — needs the one definition that names
/// the contract, and nothing after it. Followed up as a caller's lookup is,
/// every venue the contract trades on was asked in turn, and the rows and
/// the end that came back reached the wrapper under a number nobody asked
/// with.
#[test]
fn a_naming_lookup_of_the_engines_own_neither_fans_out_nor_reaches_the_wrapper() {
    let (mut ccp, mut context, shared) = u186_test_state();
    let mut hb = HeartbeatState::new();
    ccp.resolve_for_subscribe(spy_by_symbol(3), &mut None, &mut hb, &shared);
    let many = ccp.pending_md_subscribe[0].0;
    ccp.resolve_for_subscribe(spy_by_symbol(4), &mut None, &mut hb, &shared);
    let one = ccp.pending_md_subscribe[1].0;

    ccp.process_ccp_message(&named_on_many_venues(&many.to_string()), &mut None, &mut context, &shared,
        &None, &mut hb, "DU1");
    ccp.process_ccp_message(&secdef_frame_by_symbol(&one.to_string()), &mut None, &mut context, &shared,
        &None, &mut hb, "DU1");

    assert_eq!(ccp.resolved_md_subscribe.len(), 2, "both subscriptions are named");
    assert!(ccp.pending_fanout.is_empty(), "no venue is asked in turn on the engine's account");
    assert!(ccp.pending_secdef.is_empty(), "and both lookups are over");
    assert!(shared.reference.drain_contract_details().is_empty(), "no row reaches the wrapper");
    let ended = shared.reference.drain_contract_details_end();
    assert!(ended.is_empty(), "and no end, under a number nobody asked with: {ended:?}");
}

fn secdef_frame_by_symbol(req_id: &str) -> Vec<u8> {
    crate::protocol::fix::fix_build(&[
        (fix::TAG_MSG_TYPE, "d"),
        (crate::control::contracts::TAG_SECURITY_REQ_ID, req_id),
        (crate::control::contracts::TAG_SECURITY_RESPONSE_TYPE, "4"),
        (55, "SPY"),
        (crate::control::contracts::TAG_IB_CON_ID, "756733"),
    ], 1)
}

/// A symbol stated without a currency is answered with every listing that
/// carries it. A subscription or request naming its contract that way named
/// several, and was sent for whichever listing the reply stated last, with
/// no word to the caller; a lookup of the same description is refused as
/// naming none on both surfaces.
#[test]
fn a_naming_that_matches_several_listings_is_refused_rather_than_sent_for_the_last() {
    let (mut ccp, mut context, shared) = u186_test_state();
    let mut hb = HeartbeatState::new();
    let mut parked = spy_by_symbol(3);
    parked.currency.clear();
    ccp.resolve_for_subscribe(parked, &mut None, &mut hb, &shared);
    let sub = ccp.pending_md_subscribe[0].0;
    assert!(ccp.hold_until_named(head_timestamp_by_symbol(7), &mut None, &mut hb, &shared).is_none());
    let held = ccp.pending_named[0].0;

    ccp.process_ccp_message(&named_twice(&sub.to_string()), &mut None, &mut context, &shared, &None, &mut hb, "DU1");
    ccp.process_ccp_message(&named_twice(&held.to_string()), &mut None, &mut context, &shared, &None, &mut hb, "DU1");

    assert!(ccp.pending_md_subscribe.is_empty() && ccp.resolved_md_subscribe.is_empty(),
        "the subscription is neither waiting nor sent");
    let failures = shared.market.drain_subscription_failures();
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert_eq!(failures[0].0, 3);
    assert!(failures[0].1.contains("2 contracts"), "told how many it named: {}", failures[0].1);
    assert_eq!(context.slots_to_reconsider, [3], "and the slot goes back");

    assert!(ccp.pending_named.is_empty() && ccp.resolved_named.is_empty(),
        "the request is neither waiting nor sent");
    let told = shared.reference.drain_historical_errors();
    assert_eq!(told.len(), 1, "{told:?}");
    assert_eq!((told[0].0, told[0].1), (7, crate::error_codes::Refusal::NO_DEFINITION));
    assert!(told[0].2.contains("2 contracts"), "{}", told[0].2);
}

/// A caller's lookup made while the venue is unreachable is told so now.
/// Queued as if sent, it was reported twenty seconds later as a request the
/// venue did not answer — which it never received — while the sibling
/// lookups, matching symbols and option chains, refuse at once.
#[test]
fn a_lookup_that_cannot_reach_the_venue_is_refused_now() {
    let (mut ccp, _context, shared) = u186_test_state();
    let mut hb = HeartbeatState::new();
    ccp.send_secdef_request(7, 756733, &mut None, &mut hb, &shared);
    ccp.send_secdef_request_by_symbol(8, "SPY", "STK", "SMART", "USD", &Default::default(), &mut None, &mut hb, &shared)
        .expect("nothing is wrong with the request itself");

    assert!(ccp.pending_secdef.is_empty(), "nothing waits on a reply that cannot come");
    let told = shared.reference.drain_historical_errors();
    assert_eq!(
        told.iter().map(|(rid, code, _)| (*rid, *code)).collect::<Vec<_>>(),
        [(7, crate::error_codes::Refusal::NOT_CONNECTED), (8, crate::error_codes::Refusal::NOT_CONNECTED)],
        "{told:?}",
    );
    assert_eq!(shared.reference.drain_contract_details_end(), [7, 8], "and each is ended");
}

/// A number reused for a contract it was already handed, while any other
/// lookup is in flight, found the row delivered and got neither the row nor
/// the end: the end sat inside the check that keeps a row single, and the
/// record of what a number was handed outlived the lookup that was sent
/// afresh under it.
#[test]
fn a_lookup_repeated_under_its_number_is_ended_and_sent_afresh() {
    let (mut ccp, mut context, shared) = u186_test_state();
    let mut hb = HeartbeatState::new();
    // Another lookup in flight keeps the record of what was handed over.
    ccp.pending_secdef.push((9, true, Instant::now() + SECDEF_TIMEOUT));
    for _ in 0..2 {
        ccp.pending_secdef.push((7, true, Instant::now() + SECDEF_TIMEOUT));
        ccp.process_ccp_message(&named_by_id("7"), &mut None, &mut context, &shared, &None, &mut hb, "DU1");
        ccp.sweep_contract_details(&shared, &None);
    }
    assert_eq!(shared.reference.drain_contract_details().len(), 1, "the row is handed over once");
    assert_eq!(shared.reference.drain_contract_details_end(), [7, 7], "and each lookup ends");

    let (conn, _peer) = crate::protocol::connection::Connection::for_test();
    ccp.send_secdef_request(7, 756733, &mut Some(conn), &mut hb, &shared);
    assert!(!ccp.details_delivered.contains_key(&7), "a lookup sent afresh forgets what its number was handed");
}

/// The venue's "no definition" for a naming lookup of the engine's own ends
/// what waited on it now. Dropped with the lookup alone, the subscription or
/// request learnt of it only when its own twelve-second wait ran out.
#[test]
fn the_venues_no_definition_ends_what_waited_on_the_naming_now() {
    let (mut ccp, mut context, shared) = u186_test_state();
    let mut hb = HeartbeatState::new();
    ccp.resolve_for_subscribe(spy_by_symbol(3), &mut None, &mut hb, &shared);
    let sub = ccp.pending_md_subscribe[0].0;
    assert!(ccp.hold_until_named(head_timestamp_by_symbol(7), &mut None, &mut hb, &shared).is_none());
    let held = ccp.pending_named[0].0;

    ccp.process_ccp_message(&secdef_not_found(&sub.to_string()), &mut None, &mut context, &shared, &None, &mut hb, "DU1");
    ccp.process_ccp_message(&secdef_not_found(&held.to_string()), &mut None, &mut context, &shared, &None, &mut hb, "DU1");

    assert!(ccp.pending_md_subscribe.is_empty(), "the subscription no longer waits");
    let failures = shared.market.drain_subscription_failures();
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert_eq!(failures[0].0, 3);
    assert_eq!(context.slots_to_reconsider, [3], "and its slot goes back");
    assert!(ccp.pending_named.is_empty(), "the request no longer waits");
    let told = shared.reference.drain_historical_errors();
    assert_eq!(told.len(), 1, "{told:?}");
    assert_eq!((told[0].0, told[0].1), (7, crate::error_codes::Refusal::NO_DEFINITION));
}

/// The venue's reject of a lookup the engine made for itself frees the
/// contract to be asked for again on the next report naming it. Held in the
/// record of what was asked, the contract stayed unnamed for the session:
/// the deadline sweep frees it, the reject did not.
#[test]
fn a_rejected_lookup_of_the_engines_own_leaves_the_contract_askable_again() {
    let (mut ccp, mut context, shared) = u186_test_state();
    let mut hb = HeartbeatState::new();
    ccp.auto_fetch_secdef_if_cold(756733, &mut None, &shared, &mut hb);
    let req_id = ccp.auto_fetched_conids[&756733].to_string();

    let reject = crate::protocol::fix::fix_build(&[
        (fix::TAG_MSG_TYPE, "3"), (320, &req_id), (58, "Invalid request"), (371, "6008"),
    ], 1);
    ccp.process_ccp_message(&reject, &mut None, &mut context, &shared, &None, &mut hb, "DU1");

    assert!(ccp.pending_secdef.is_empty(), "the lookup is over");
    assert!(!ccp.auto_fetched_conids.contains_key(&756733), "and the contract can be asked for again");
    assert!(shared.reference.drain_historical_errors().is_empty(), "nothing reaches a caller");
}

/// The venue names the exchanges that offer a book once, unprompted, after
/// logon. An ask for that list made before it lands was answered with
/// nothing and spent, so the directory landing later answered nobody.
#[test]
fn an_ask_for_the_book_venues_is_answered_when_the_directory_lands() {
    let shared = SharedState::new();
    shared.reference.notify_depth_exchanges();
    assert!(shared.reference.drain_depth_exchanges().is_empty(), "nothing to answer with yet");
    shared.reference.push_depth_exchanges(vec![crate::types::DepthMktDataDescription {
        exchange: "ISLAND".into(), sec_type: "STK".into(), listing_exch: "NASDAQ".into(),
        service_data_type: String::new(), agg_group: 0,
    }]);
    assert_eq!(shared.reference.drain_depth_exchanges().len(), 1, "the ask that waited is answered");
    assert!(shared.reference.drain_depth_exchanges().is_empty(), "once");
}

/// A scan's batches reach the caller in the order the venue sent them, and
/// one scan's wait holds up no other. A batch needing no enrichment went
/// straight through while an earlier one waited on a definition, so the
/// caller ended on the older list.
#[test]
fn a_scans_batches_are_handed_over_in_the_order_they_arrived() {
    let (mut ccp, _context, shared) = u186_test_state();
    let mut hb = HeartbeatState::new();
    let batch = |con_ids: &[u32]| crate::control::scanner::ScannerResult {
        con_ids: con_ids.to_vec(),
        entries: con_ids.iter().map(|&con_id| crate::control::scanner::ScannerEntry { con_id }).collect(),
        scan_time: String::new(),
        error_text: String::new(),
    };
    ccp.start_scanner_enrichment(7, batch(&[111]), &mut None, &shared, &mut hb);
    ccp.start_scanner_enrichment(7, batch(&[]), &mut None, &shared, &mut hb);
    ccp.start_scanner_enrichment(8, batch(&[]), &mut None, &shared, &mut hb);
    let first: Vec<u32> = shared.reference.drain_scanner_data().into_iter().map(|(rid, _)| rid).collect();
    assert_eq!(first, [8], "the other scan goes through; the later batch of the first waits behind its earlier one");

    ccp.try_release_scanner_enrichments(111, &shared);
    let got: Vec<(u32, Vec<u32>)> = shared.reference.drain_scanner_data().into_iter()
        .map(|(rid, r)| (rid, r.con_ids)).collect();
    assert_eq!(got, [(7, vec![111]), (7, vec![])], "both, in the order they arrived");
}

/// The venue refuses an option-chain request for an underlying it cannot
/// number with a session reject naming the request. Attributed to nothing,
/// the caller waited out the chain's deadline for an answer that had arrived
/// fifteen milliseconds after the request.
#[test]
fn the_venues_reject_of_an_option_chain_request_reaches_its_caller_now() {
    let (mut ccp, mut context, shared) = u186_test_state();
    let mut hb = HeartbeatState::new();
    ccp.pending_option_params.push((701, "SPY".into(), 0, Instant::now() + Duration::from_secs(12)));
    let reject = crate::protocol::fix::fix_build(&[
        (fix::TAG_MSG_TYPE, "3"), (320, "701"), (58, "Unknown contract"),
    ], 1);
    ccp.process_ccp_message(&reject, &mut None, &mut context, &shared, &None, &mut hb, "DU1");
    assert!(ccp.pending_option_params.is_empty(), "the request is over");
    let told = shared.reference.drain_historical_errors();
    assert_eq!(told.len(), 1, "{told:?}");
    assert_eq!((told[0].0, told[0].1), (701, crate::error_codes::Refusal::NO_DEFINITION));
    assert!(told[0].2.contains("Unknown contract"), "in the venue's words: {}", told[0].2);
}

/// The plain and the `Full` spellings of a margin figure are two figures, and
/// they diverge whenever intraday margin relief applies. Folded into one
/// field, the account read whichever the venue stated last.
#[test]
fn the_full_margin_spellings_do_not_overwrite_the_plain_ones() {
    let (_ccp, mut context, shared) = u186_test_state();
    let msg = b"8=FIX.4.1\x0135=U\x018001=InitMarginReq\x018004=100.00\x018001=FullInitMarginReq\x018004=200.00\x01\
                8001=AvailableFunds\x018004=300.00\x018001=FullAvailableFunds\x018004=400.00\x01\
                8001=ExcessLiquidity\x018004=500.00\x018001=FullExcessLiquidity\x018004=600.00\x01";
    super::positions::handle_account_update(msg, &mut context, &shared);
    let account = context.account();
    assert_eq!(account.init_margin_req, 100 * PRICE_SCALE, "the plain figure");
    assert_eq!(account.available_funds, 300 * PRICE_SCALE);
    assert_eq!(account.excess_liquidity, 500 * PRICE_SCALE);
    assert!(
        shared.portfolio.stated_account_values().iter().any(|(k, v, _)| k == "FullInitMarginReq" && v == "200.00"),
        "and the full spelling stays reachable by name",
    );
}

/// A figure for holdings held elsewhere is stated in a currency, as the
/// account's own figures are, and a figure stated in two currencies is two
/// figures. Keyed on the name alone, the second statement overwrote the
/// first and the caller read one number with nothing to say which currency.
#[test]
fn a_figure_for_holdings_elsewhere_keeps_its_currency() {
    let (_ccp, _context, shared) = u186_test_state();
    let msg = b"8=FIX.4.1\x0135=U\x018001=NetLiquidation\x018004=123.00\x0115=USD\x018001=NetLiquidation\x018004=100.00\x0115=EUR\x01";
    super::handle_account_update_elsewhere(msg, &shared, crate::types::HeldElsewhere::Away);
    let mut stated = shared.portfolio.values_elsewhere(crate::types::HeldElsewhere::Away);
    stated.sort();
    assert_eq!(stated, [
        ("NetLiquidation".to_string(), "100.00".to_string(), "EUR".to_string()),
        ("NetLiquidation".to_string(), "123.00".to_string(), "USD".to_string()),
    ]);
}

/// An execution report states no charge; what the fill cost arrives on a
/// record of its own, afterwards. Read off a tag the report does not carry,
/// every order stated that it cost exactly nothing, which a program written
/// against the reference records as a cost because it is not the unset value.
#[test]
fn an_execution_report_states_no_charge_so_the_order_states_none() {
    let (mut ccp, mut context, shared) = tracked_order_state();
    ccp.handle_exec_report(&fill_frame(&[]), b"", &mut context, &shared, &None, "");
    let state = shared.orders.get_order_info(42).expect("the order is reported").order_state;
    assert_eq!(state.commission_and_fees, f64::MAX, "unstated, not nothing");
    assert_eq!(state.min_commission_and_fees, f64::MAX);
    assert_eq!(state.max_commission_and_fees, f64::MAX);
}

/// A profit-and-loss subscription outlives the connection it was asked on.
///
/// The venue serves the subscription on the connection that asked for it,
/// and a rebuilt connection has not been asked. The position and account
/// requests were renewed on a reconnect; this one was not, so the marks a
/// caller reads stopped moving with nothing said. A subscription the caller
/// withdrew before the drop is not renewed.
#[test]
fn a_pnl_subscription_is_renewed_on_a_reconnect_unless_withdrawn() {
    use std::io::Read;
    let mut ccp = CcpState::new();
    let shared = SharedState::new();
    let mut hb = HeartbeatState::new();
    let (first, mut first_peer) = crate::protocol::connection::Connection::for_test();
    let mut ccp_conn: Option<Connection> = Some(first);
    ccp.send_pnl_subscribe(5, "DU1", &mut ccp_conn, &mut hb);
    ccp.send_pnl_subscribe(6, "DU1", &mut ccp_conn, &mut hb);
    let mut buf = [0u8; 8192];
    let _ = first_peer.read(&mut buf);
    ccp.withdraw_pnl_subscription(6);
    ccp_conn = None;

    let (second, mut second_peer) = crate::protocol::connection::Connection::for_test();
    second_peer.set_read_timeout(Some(std::time::Duration::from_millis(300))).unwrap();
    ccp.reconnect(second, &mut ccp_conn, &mut hb, "DU1", &shared);
    let mut sent = Vec::new();
    while let Ok(n) = second_peer.read(&mut buf) {
        if n == 0 { break; }
        sent.extend_from_slice(&buf[..n]);
    }
    let msg = String::from_utf8_lossy(&sent).replace('\u{1}', "|");
    assert!(msg.contains("|6040=142|6529=PLR.5|1=DU1|"), "the standing subscription is asked for again: {msg}");
    assert!(!msg.contains("PLR.6"), "and the withdrawn one is not: {msg}");
}

/// A preview the venue refuses is a refusal of the preview and nothing else.
///
/// Read through the ordinary report path it became a rejected order: a status
/// for an order never placed, and the number the caller previewed under read
/// as spent, so placing under it afterwards was refused as a number already
/// worked and finished.
#[test]
fn a_refused_preview_is_a_refusal_and_not_a_rejected_order() {
    let (mut ccp, mut context, shared) = what_if_test_state();
    let mut frame = what_if_frame(&[]);
    frame.insert(39, "8".to_string());
    frame.insert(150, "8".to_string());
    frame.insert(58, "no margin for a preview of this size".to_string());
    ccp.handle_exec_report(&frame, b"", &mut context, &shared, &None, "");
    let refused = shared.orders.drain_order_inactive();
    assert!(refused.iter().any(|(id, code, why)| *id == 42 && *code == 201 && why.contains("no margin")), "{refused:?}");
    assert!(
        shared.orders.drain_order_updates().iter().all(|u| u.order_id != 42),
        "no status is said for an order that was never placed",
    );
    assert!(context.order(42).is_none(), "and the preview is over");
}

/// A definition arriving over an entry a fill seeded keeps what it states.
///
/// The cache merged seven names and nothing else, so an option's strike, its
/// right, its expiry and its multiplier — all that tells two options on one
/// underlying apart — were dropped on the way in.
#[test]
fn a_definition_over_a_seeded_entry_keeps_what_it_states() {
    let shared = SharedState::new();
    shared.reference.cache_contract(1, crate::types::model::Contract {
        con_id: 1, symbol: "SPY".into(), sec_type: "OPT".into(), exchange: "SMART".into(), currency: "USD".into(),
        ..Default::default()
    });
    shared.reference.cache_definition(1, crate::types::model::Contract {
        con_id: 1, symbol: "SPY".into(), sec_type: "OPT".into(), strike: 500.0, right: "C".into(),
        last_trade_date_or_contract_month: "20261218".into(), multiplier: "100".into(), trading_class: "SPY".into(),
        ..Default::default()
    });
    let held = shared.reference.get_contract(1).expect("cached");
    assert_eq!((held.strike, held.right.as_str(), held.last_trade_date_or_contract_month.as_str(), held.multiplier.as_str()),
        (500.0, "C", "20261218", "100"), "{held:?}");
}

/// An entry a fill seeded is not a definition, and does not keep the
/// definition from being asked for.
#[test]
fn a_seeded_entry_does_not_keep_the_definition_from_being_fetched() {
    let mut ccp = CcpState::new();
    let shared = SharedState::new();
    let mut hb = HeartbeatState::new();
    shared.reference.cache_contract(2, crate::types::model::Contract {
        con_id: 2, symbol: "SPY".into(), sec_type: "OPT".into(), ..Default::default()
    });
    ccp.auto_fetch_secdef_if_cold(2, &mut None, &shared, &mut hb);
    assert!(ccp.auto_fetched_conids.contains_key(&2), "the definition is asked for");
    shared.reference.cache_definition(3, crate::types::model::Contract { con_id: 3, symbol: "SPY".into(), ..Default::default() });
    ccp.auto_fetch_secdef_if_cold(3, &mut None, &shared, &mut hb);
    assert!(!ccp.auto_fetched_conids.contains_key(&3), "and a defined one is not asked for again");
}

/// A chain asked for without the underlying's id is answered, under the id
/// the venue names.
///
/// Matched on the id alone, a request that stated none could never match a
/// reply that stated one, and the caller waited out the deadline; and the
/// callback carried the caller's id where the reference client carries the
/// venue's.
#[test]
fn a_chain_asked_for_without_the_underlyings_id_is_answered_under_the_venues() {
    let mut ccp = CcpState::new();
    let shared = SharedState::new();
    ccp.pending_option_params.push((9, "SPY".into(), 0, Instant::now() + OPTION_CHAIN_TIMEOUT));
    let msg = fix::fix_build(
        &[
            (fix::TAG_MSG_TYPE, "U"), (6040, "139"), (55, "SPY"),
            (6775, "20260116/20260320"), (6346, "756733"), (100, "SMART"), (6058, "SPY"), (231, "100"),
            (6997, "500.0;505.0"),
        ],
        1,
    );
    ccp.handle_option_chain(&msg, &shared);
    let answered = shared.reference.drain_option_params();
    assert_eq!(answered.len(), 1, "the request is answered");
    assert_eq!((answered[0].0, answered[0].1), (9, 756733), "under the underlying the venue names");
    assert!(ccp.pending_option_params.is_empty());
}

/// Of two listings in one reply, the one the trading hours are fetched for
/// is delivered with them, once.
///
/// The multi-listing path claimed the delivery slot for every listing, the
/// last included — the one the flat path pairs with the schedule — so that
/// row reached the caller without hours and the enriched row was dropped at
/// the gate when the pair resolved.
#[test]
fn the_listing_paired_with_its_hours_is_delivered_by_that_pairing_alone() {
    let (mut ccp, mut context, shared) = u186_test_state();
    // A caller's lookup by symbol, which is what draws two listings.
    ccp.pending_secdef.push((9, false, Instant::now() + SECDEF_TIMEOUT));
    let msg = crate::protocol::fix::fix_build(&[
        (fix::TAG_MSG_TYPE, "d"),
        (crate::control::contracts::TAG_SECURITY_REQ_ID, "9"),
        (crate::control::contracts::TAG_SECURITY_RESPONSE_TYPE, "4"),
        (55, "SPY"), (167, "CS"), (crate::control::contracts::TAG_IB_CON_ID, "756733"), (15, "USD"),
        (55, "SPY"), (167, "CS"), (crate::control::contracts::TAG_IB_CON_ID, "90016213"), (15, "MXN"),
        (6256, "SPY-MEXI"),
    ], 1);
    ccp.process_ccp_message(&msg, &mut None, &mut context, &shared, &None, &mut HeartbeatState::new(), "DU1");
    let rows: Vec<i64> = shared.reference.drain_contract_details().into_iter().map(|(_, d)| d.con_id as i64).collect();
    assert_eq!(rows, vec![756733], "the listing with no hours to wait for goes at once, and only that one");
    assert!(
        ccp.pending_schedule_pair.iter().any(|p| p.api_req_id == 9 && p.def.con_id == 90016213),
        "the other waits for its hours: {:?}", ccp.pending_schedule_pair.iter().map(|p| p.def.con_id).collect::<Vec<_>>(),
    );
}

/// A leg of a fan-out whose reply cannot be read still counts as answered.
///
/// The tally sat inside the parse, so an unreadable leg never completed the
/// fan-out and the request waited out its deadline for a reply already in.
#[test]
fn a_leg_whose_reply_cannot_be_read_still_counts() {
    let (mut ccp, mut context, shared) = u186_test_state();
    ccp.pending_fanout.push(PendingFanout {
        api_req_id: 9,
        fanout_req_ids: vec!["ibxfan-9-0".into(), "ibxfan-9-1".into()],
        answered: vec!["ibxfan-9-0".into()],
        deadline: Instant::now() + SECDEF_TIMEOUT,
    });
    let unreadable = crate::protocol::fix::fix_build(&[
        (fix::TAG_MSG_TYPE, "d"),
        (crate::control::contracts::TAG_SECURITY_REQ_ID, "ibxfan-9-1"),
        (crate::control::contracts::TAG_SECURITY_RESPONSE_TYPE, "4"),
        (55, "SPY"), (167, "CS"), (crate::control::contracts::TAG_IB_CON_ID, "not-a-contract"),
    ], 1);
    ccp.process_ccp_message(&unreadable, &mut None, &mut context, &shared, &None, &mut HeartbeatState::new(), "DU1");
    assert!(ccp.pending_fanout.is_empty(), "the fan-out is complete");
    assert_eq!(shared.reference.drain_contract_details_end(), vec![9], "and the caller has its end");
}

/// A refusal answers the order that was cancelled, not the number its name
/// happens to carry.
///
/// Tag 41 echoes the name this client put on the cancel, and that name is not
/// always built from the order's own number: an order recovered from a prior
/// session is keyed here by the id the venue stated beside it, while the name
/// it answers to comes from the permanent one. Read back as digits, the
/// refusal reached whatever order that number matched — so the order the
/// caller had cancelled stayed pending for good, and another was retired in
/// its place.
#[test]
fn a_refusal_reaches_the_order_whose_name_it_states() {
    let mut ccp = CcpState::new();
    let mut context = Context::new();
    let shared = SharedState::new();
    let instrument = context.register_instrument(756733);
    // The order the caller cancelled, recovered from a prior session and so
    // answering to a name built from the venue's permanent number.
    context.insert_order(crate::types::Order::new(
        42, instrument, Side::Buy, 100 * crate::types::QTY_SCALE, 100 * PRICE_SCALE, b'2', b'0', 0,
    ));
    context.update_order_status(42, crate::types::OrderStatus::PendingCancel, false);
    context.last_clord.insert(42, "9000.0".to_string());
    // And an unrelated order that happens to be numbered as that name reads.
    context.insert_order(crate::types::Order::new(
        9000, instrument, Side::Buy, 100 * crate::types::QTY_SCALE, 100 * PRICE_SCALE, b'2', b'0', 0,
    ));

    let mut frame = std::collections::HashMap::new();
    frame.insert(41u32, "9000.0".to_string());
    frame.insert(434u32, "1".to_string());
    frame.insert(102u32, "1".to_string());
    ccp.handle_cancel_reject(&frame, &mut context, &shared, &None);

    assert!(
        context.order(42).is_none(),
        "the refusal did not reach the order whose cancel it answers",
    );
    assert!(
        context.order(9000).is_some(),
        "and it retired the order whose number the name happened to read as",
    );
}

/// A cancel refused because the order finished ends the order.
///
/// The venue very often refuses a cancel precisely because there is nothing
/// left to cancel, and says which on tag 39. Taken as a status and nothing
/// more, the order kept its place in the book with a terminal status written
/// on it, no completion was filed, and the row a caller reads stayed the
/// working one — so `req_open_orders` went on listing an order the venue had
/// said was done. The refusal is the last message the order draws, so nothing
/// later corrected any of it.
///
/// A refusal is one of the two, and it is the one the shared vocabulary cannot
/// state: it reads as the same word as an order the venue merely holds, and
/// only the completed status beside it tells them apart. Asked with none to
/// hand it came back as not finished, and the order was forced to working
/// against a message that said it had been rejected.
#[test]
fn a_cancel_refused_because_the_order_is_over_ends_it() {
    for (stated, expected) in [
        ("2", crate::types::OrderStatus::Filled),
        ("8", crate::types::OrderStatus::Rejected),
    ] {
        let mut ccp = CcpState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        tracked_for_cancel(&mut context);
        shared.orders.push_order_info(42, RichOrderInfo {
            contract: api::Contract::default(),
            order: api::Order::default(),
            order_state: api::OrderState::default(),
            last_exec: api::Execution::default(),
        });

        let mut frame = std::collections::HashMap::new();
        frame.insert(41u32, "C42".to_string());
        frame.insert(434u32, "1".to_string());
        frame.insert(102u32, "0".to_string());
        frame.insert(39u32, stated.to_string());
        ccp.handle_cancel_reject(&frame, &mut context, &shared, &None);

        assert!(
            context.order(42).is_none(),
            "39={stated}: the order the venue says is over is still in the book, \
             where a cancel-all walks to it and a replace names it",
        );
        let row = shared.orders.get_order_info(42)
            .expect("39={stated}: the row is what the completed order is reported from");
        assert!(
            !crate::types::order_status::is_open_or_reactivatable(
                &row.order_state.status, &row.order_state.completed_status,
            ),
            "39={stated}: the row a caller reads still lists it as working: {:?}",
            row.order_state.status,
        );
        let completed = shared.orders.drain_completed_orders();
        assert!(
            completed.iter().any(|c| c.order_id == 42 && c.status == expected),
            "39={stated}: nothing was filed to say how it finished: {completed:?}",
        );
    }
}

/// A correction that reopens a finished order puts it back where a withdrawal
/// can reach it.
///
/// A trade cancel or correction restates an execution already reported, so it
/// is the one report that may legitimately return a completed order to a
/// working quantity — and the record a caller reads already accepted it. The
/// engine's own book did not, so the two disagreed: the caller was shown an
/// open order, a cancel-all walked the book and never reached it, and the
/// quantity the correction gave back had no cumulative baseline to be booked
/// against.
///
/// However the venue states it. A cancelled or corrected execution arrives on
/// the report type, 150=H and 150=G, or on the transaction type, 20=1 and 20=2
/// — and the wire states the transaction type on every report. Read from the
/// report type alone, the second shape took its quantity off the account and
/// left the order finished: out of the book, out of the name a withdrawal is
/// sent under, and out of the correction the caller reads.
#[test]
fn a_correction_that_reopens_an_order_puts_it_back_in_the_book() {
    for undone in [&[(150u32, "H")][..], &[(150, "F"), (20, "1")][..]] {
        let mut ccp = CcpState::new();
        let mut context = Context::new();
        let instrument = context.register_instrument(756733);
        context.set_symbol(instrument, "SPY".to_string());
        let shared = SharedState::new();
        context.insert_order(crate::types::Order::new(
            42, instrument, Side::Buy, 100 * QTY_SCALE, 400 * PRICE_SCALE, b'2', b'0', 0,
        ));

        // Filled whole, so the order finishes and is retired.
        let fill = exec_report_frame(&[
            (39, "2"), (150, "F"), (17, "exec-1"),
            (32, "100"), (31, "412.25"), (14, "100"), (38, "100"),
        ]);
        ccp.handle_exec_report(&fill, b"", &mut context, &shared, &None, "");
        assert!(context.order(42).is_none(), "a filled order is retired");
        assert!(!context.last_clord.contains_key(&42), "and its name goes with it");
        let _ = shared.orders.drain_fills();

        // The venue then undoes half of that trade, leaving the order working.
        // Stating its side, which is what putting an order back needs: a guessed
        // one books every later fill the wrong way, by twice the fill.
        let mut corrected = exec_report_frame(&[
            (39, "1"), (17, "exec-2"), (54, "1"), (6008, "756733"),
            (32, "50"), (31, "412.25"), (14, "50"), (38, "100"),
        ]);
        for (tag, val) in undone {
            corrected.insert(*tag, val.to_string());
        }
        ccp.handle_exec_report(&corrected, b"", &mut context, &shared, &None, "");

        assert!(
            context.order(42).is_some(),
            "{undone:?}: the order the correction reopened is not in the book a withdrawal walks",
        );
        assert!(
            context.last_clord.contains_key(&42),
            "{undone:?}: and a withdrawal has no name to send it under",
        );
        assert_eq!(
            shared.orders.drain_order_corrections(), vec![42],
            "{undone:?}: the caller is still told the order finished",
        );
    }
}

/// A correction the window has already seen is that same one again.
///
/// The booking refuses it on its key, but the recovery and the correction the
/// caller reads went by the report alone and took it as a second one: an order
/// this session had already finished came back working, holding the quantity
/// the first copy had given back and short every fill that followed it.
#[test]
fn a_repeated_correction_does_not_reopen_a_finished_order() {
    let mut ccp = CcpState::new();
    let mut context = Context::new();
    let instrument = context.register_instrument(756733);
    context.set_symbol(instrument, "SPY".to_string());
    let shared = SharedState::new();
    context.insert_order(crate::types::Order::new(
        42, instrument, Side::Buy, 100 * QTY_SCALE, 400 * PRICE_SCALE, b'2', b'0', 0,
    ));

    let fill = exec_report_frame(&[
        (39, "2"), (150, "F"), (17, "exec-1"),
        (32, "100"), (31, "412.25"), (14, "100"), (38, "100"),
    ]);
    ccp.handle_exec_report(&fill, b"", &mut context, &shared, &None, "");
    assert!(context.order(42).is_none(), "a filled order is retired");

    let corrected = exec_report_frame(&[
        (39, "1"), (150, "H"), (17, "exec-2"), (54, "1"), (6008, "756733"),
        (32, "50"), (31, "412.25"), (14, "50"), (38, "100"),
    ]);
    ccp.handle_exec_report(&corrected, b"", &mut context, &shared, &None, "");
    assert!(context.order(42).is_some(), "the correction reopened the order");
    assert_eq!(shared.orders.drain_order_corrections(), vec![42]);
    let _ = shared.orders.drain_fills();

    // The quantity it gave back is filled again, and the order finishes.
    let refill = exec_report_frame(&[
        (39, "2"), (150, "F"), (17, "exec-3"), (54, "1"), (6008, "756733"),
        (32, "50"), (31, "412.25"), (14, "100"), (38, "100"),
    ]);
    ccp.handle_exec_report(&refill, b"", &mut context, &shared, &None, "");
    assert!(context.order(42).is_none(), "filled whole again, so retired again");
    let _ = shared.orders.drain_fills();

    // The venue restates the correction while its key is still in the window.
    ccp.handle_exec_report(&corrected, b"", &mut context, &shared, &None, "");

    assert!(
        context.order(42).is_none(),
        "a correction already acted on put a finished order back in the book",
    );
    assert!(
        shared.orders.drain_order_corrections().is_empty(),
        "and told the caller its completion no longer stands",
    );
}

/// A report arriving behind a finished order leaves no name behind it.
///
/// Retiring an order drops the two name maps precisely because they only serve
/// orders that can still be cancelled or replaced. A late working echo — and
/// the venue sends one behind a fill — wrote the entry straight back, with
/// nothing left that would ever remove it: a process left running held one per
/// order it had ever placed.
#[test]
fn a_report_behind_a_finished_order_leaves_no_name_behind_it() {
    let mut ccp = CcpState::new();
    let mut context = Context::new();
    let instrument = context.register_instrument(756733);
    context.set_symbol(instrument, "SPY".to_string());
    let shared = SharedState::new();
    context.insert_order(crate::types::Order::new(
        42, instrument, Side::Buy, 100 * QTY_SCALE, 400 * PRICE_SCALE, b'2', b'0', 0,
    ));

    let fill = exec_report_frame(&[
        (11, "42.0"), (39, "2"), (150, "F"), (17, "exec-1"),
        (32, "100"), (31, "412.25"), (14, "100"), (38, "100"),
    ]);
    ccp.handle_exec_report(&fill, b"", &mut context, &shared, &None, "");
    assert!(context.order(42).is_none(), "a filled order is retired");
    assert!(!context.last_clord.contains_key(&42), "and its name goes with it");

    // The working status the venue echoes behind a fill.
    let echo = exec_report_frame(&[
        (11, "42.0"), (39, "0"), (150, "0"), (38, "100"),
    ]);
    ccp.handle_exec_report(&echo, b"", &mut context, &shared, &None, "");

    assert!(
        !context.last_clord.contains_key(&42),
        "the echo wrote the name back with nothing left that would remove it",
    );
}

/// An acceptance does not spend a fallback a later revision still needs.
///
/// The venue takes a second revision before it has answered the first. This
/// side keeps one set of terms per revision; the record a caller reads keeps
/// one for the order. Told the fallback was spent the moment the first was
/// accepted, the revision still in flight had nothing left to fall back to —
/// so when the venue refused it in turn, the record went on stating the terms
/// it had just turned down, and every later cancel and replace restated from
/// those.
#[test]
fn an_acceptance_does_not_spend_a_fallback_a_later_revision_needs() {
    let mut ccp = CcpState::new();
    let mut context = Context::new();
    let instrument = context.register_instrument(756733);
    context.set_symbol(instrument, "SPY".to_string());
    let shared = SharedState::new();
    let terms = |px: i64| crate::types::Order::new(
        42, instrument, Side::Buy, 100 * QTY_SCALE, px * PRICE_SCALE, b'2', b'0', 0,
    );
    context.insert_order(terms(102));
    // Two revisions out, neither answered.
    context.pre_replace.insert((42, 1), (terms(100), "42.0".to_string(), None));
    context.pre_replace.insert((42, 2), (terms(101), "42.1".to_string(), None));

    // The venue takes the first.
    let ack = exec_report_frame(&[
        (11, "42.1"), (39, "5"), (150, "5"), (54, "1"), (38, "100"),
    ]);
    ccp.handle_exec_report(&ack, b"", &mut context, &shared, &None, "");

    assert!(
        shared.orders.drain_replacements_taken().is_empty(),
        "the record was told to spend its only fallback while a revision it \
         would need it for is still outstanding",
    );
}

/// A refusal that states no words is still a refusal.
///
/// The reason travels as the completed status a refusal is told apart by: an
/// order whose status reads "Inactive" with nothing beside it is one the venue
/// is merely holding and can take up again. The venue sends the tag empty as
/// readily as it leaves it out, and only the second was defaulted — so an
/// order this side had retired and filed as finished came back out of the
/// working list, and every caller asking what it had on was told the order was
/// live.
#[test]
fn a_refusal_that_states_no_words_still_ends_the_order() {
    let mut ccp = CcpState::new();
    let mut context = Context::new();
    let shared = SharedState::new();
    tracked_for_cancel(&mut context);
    shared.orders.push_order_info(42, RichOrderInfo {
        contract: api::Contract::default(),
        order: api::Order::default(),
        order_state: api::OrderState::default(),
        last_exec: api::Execution::default(),
    });

    let mut frame = std::collections::HashMap::new();
    frame.insert(41u32, "C42".to_string());
    frame.insert(434u32, "1".to_string());
    frame.insert(102u32, "0".to_string());
    frame.insert(39u32, "8".to_string());
    // Stated, and empty.
    frame.insert(58u32, String::new());
    ccp.handle_cancel_reject(&frame, &mut context, &shared, &None);

    let row = shared.orders.get_order_info(42).expect("the row is kept to report from");
    assert!(
        !crate::types::order_status::is_open_or_reactivatable(
            &row.order_state.status, &row.order_state.completed_status,
        ),
        "an order the venue refused reads as one it is holding: {:?}/{:?}",
        row.order_state.status, row.order_state.completed_status,
    );
}

/// An order recovered for a contract the account holds takes that holding.
///
/// The account states what it holds before anything here asks for a slot, and
/// until there is one that statement lands on the contract's number alone. The
/// control path takes it onto the slot it makes; recovery made a slot without
/// it, so the book read the contract as flat while the account held it — and
/// the guard that keeps a slot resident for a holding read the same zero and
/// gave the slot to the next contract that needed one.
#[test]
fn an_order_recovered_on_a_held_contract_takes_the_holding() {
    let mut ccp = CcpState::new();
    let mut context = Context::new();
    let shared = SharedState::new();
    // The account named the holding before any slot existed for it.
    shared.portfolio.set_position_info(crate::types::PositionInfo {
        con_id: 756733,
        position: 100.0,
        ..Default::default()
    });

    // The venue names an order it is already working on that contract.
    let recovered = exec_report_frame(&[
        (11, "9000.0"), (150, "0"), (39, "0"), (54, "1"), (6008, "756733"),
        (55, "SPY"), (38, "100"), (44, "400.00"),
    ]);
    ccp.handle_exec_report(&recovered, b"", &mut context, &shared, &None, "");

    let instrument = context.market.instrument_by_con_id(756733)
        .expect("recovery gave the contract a slot");
    assert_eq!(
        context.position(instrument), 100.0,
        "the book reads the contract as flat, so the slot is handed away while \
         the account holds it and a withdrawal of everything never names it",
    );
}

/// The account can state a holding before a fill gives its contract a slot.
/// Later fills change that slot without restoring the older account row.
#[test]
fn an_untracked_fill_starts_from_the_holding_the_account_stated() {
    let mut ccp = CcpState::new();
    let mut context = Context::new();
    let shared = SharedState::new();
    shared.portfolio.set_position_info(PositionInfo {
        con_id: 756733, position: 100.0, ..Default::default()
    });
    for (exec_id, quantity, expected) in [("E1", "100", 0.0), ("E2", "25", -25.0)] {
        let fill = untracked_fill(&[
            (6008, "756733"), (55, "SPY"), (54, "2"), (32, quantity), (17, exec_id),
        ]);
        ccp.handle_exec_report(&fill, b"", &mut context, &shared, &None, "");
        let instrument = context.market.instrument_by_con_id(756733).unwrap();
        assert_eq!(context.position(instrument), expected, "{exec_id}: the engine's holding");
        assert_eq!(shared.portfolio.position(instrument), expected, "{exec_id}: the caller's holding");
        assert_eq!(shared.orders.drain_fills().len(), 1, "each execution is booked once");
    }
}

/// Both fields have places in the report's record, so neither is unread.
#[test]
fn price_management_and_ev_multiplier_are_read_execution_fields() {
    for frame in [b"8339=1\x019997=kept\x01".as_slice(), b"6859=50\x019997=kept\x01".as_slice()] {
        assert_eq!(executions::unnamed_execution_fields(frame), vec![(9997, "kept".into())]);
    }
}

/// A name on tag 320 attributes the refusal even when it is not a number.
#[test]
fn a_refusal_with_a_nonnumeric_name_leaves_unrelated_requests_waiting() {
    for chain in [true, false] {
        for name in ["ibxfan-5-1", "SchedSub.9", ""] {
            let (mut ccp, mut context, shared) = u186_test_state();
            if chain {
                ccp.pending_option_params.push((701, "SPY".into(), 756733, Instant::now() + OPTION_CHAIN_TIMEOUT));
                if name.starts_with("ibxfan") {
                    ccp.pending_fanout.push(PendingFanout {
                        api_req_id: 5, fanout_req_ids: vec![name.into()], answered: Vec::new(),
                        deadline: Instant::now() + SECDEF_TIMEOUT,
                    });
                }
            } else {
                ccp.pending_secdef.push((701, false, Instant::now() + SECDEF_TIMEOUT));
            }
            let refused = fix::fix_build(&[(35, "3"), (320, name), (58, "Invalid request")], 1);
            ccp.process_ccp_message(&refused, &mut None, &mut context, &shared,
                &None, &mut HeartbeatState::new(), "DU1");
            assert_eq!(ccp.pending_option_params.len() + ccp.pending_secdef.len(), 1,
                "chain={chain}, name={name:?}: the waiting request was not refused");
            assert!(shared.reference.drain_historical_errors().is_empty());
            assert!(shared.reference.drain_contract_details_end().is_empty());
        }
    }
}

/// The report supplies the side where it names one, and the tracked order
/// supplies it otherwise. Without either, no action is stated.
#[test]
fn an_execution_action_comes_from_the_report_or_the_tracked_order() {
    for (side, action) in [(Side::Buy, "BUY"), (Side::Sell, "SELL"), (Side::ShortSell, "SSHORT")] {
        for stated in [None, Some("?"), Some("1"), Some("2"), Some("5")] {
            let mut ccp = CcpState::new();
            let mut context = Context::new();
            let shared = SharedState::new();
            let instrument = context.register_instrument(756733);
            context.insert_order(crate::types::Order::new(
                42, instrument, side, 10 * QTY_SCALE, 100 * PRICE_SCALE, b'2', b'1', 0,
            ));
            let mut report = exec_report_frame(&[(150, "0"), (39, "0")]);
            report.remove(&54);
            report.remove(&40);
            report.remove(&59);
            if let Some(stated) = stated { report.insert(54, stated.into()); }
            ccp.handle_exec_report(&report, b"", &mut context, &shared, &None, "");
            let order = shared.orders.get_order_info(42).unwrap().order;
            let expected = match stated { Some("1") => "BUY", Some("2") => "SELL", Some("5") => "SSHORT", _ => action };
            assert_eq!(order.action, expected);
            assert_eq!(order.tif, "GTC");
            assert_eq!(order.order_type, "LMT");
        }
    }
    let mut ccp = CcpState::new();
    let mut context = Context::new();
    let shared = SharedState::new();
    let mut report = exec_report_frame(&[(150, "0"), (39, "0")]);
    report.remove(&54);
    ccp.handle_exec_report(&report, b"", &mut context, &shared, &None, "");
    assert!(shared.orders.get_order_info(42).unwrap().order.action.is_empty());
}
