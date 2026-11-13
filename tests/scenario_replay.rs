//! Scenario replay (ibx#487, tests layer C): the scenarios recorded from
//! the reference gateway (tests/fixtures/gw1040/scenarios/ for 10.40 at
//! level 214, tests/fixtures/gw1051/scenarios/ for 10.51, whose folders of
//! 18/10/2026 on are at the client's own level 225, ibx#604; four legs in
//! one order) replayed through the Rust API client and the engine against
//! a scripted peer that plays the recorded servers
//! (`ibx::test_support::scenario`). Two checks per scenario:
//! - outbound: ibx's frames of the compared kinds are the reference's,
//!   after normalisation, in the same order; the first missing, different
//!   or unexpected frame fails the test;
//! - API: ibx's callbacks are the reference's: same callbacks, fields,
//!   count and order (timing only as order).
//!
//! No network. A scenario recorded again (with its API side decoded by
//! scripts/codec_fixtures.py --scenarios) runs with no change here.

use ibx::test_support::scenario::runner::{HISTORICAL, LOOKUP, MARKET_DATA, ORDER, SCANNER, SUBSCRIPTION};
use ibx::test_support::scenario::{load_scenario, replay, Options, Outcome};
use ibx::test_support::scenario::runner::assert_same_callbacks;
use ibx::test_support::Fields;

/// Known differences the order scenarios mask on both sides, each with its
/// own ignored test in src/golden/orders.rs (ibx#510): the first openOrder
/// of a STP order shows a limit price the wire does not carry
/// (`stp_first_open_order_limit_price`).
fn known(line: &str) -> String {
    let mut f: Vec<String> = line.split('|').map(str::to_string).collect();
    if f[0] == "openOrder" && f[5].contains("orderType=STP,") {
        f[5] = f[5].split(',').map(|kv| if kv.starts_with("lmtPrice=") { "lmtPrice=-" } else { kv }).collect::<Vec<_>>().join(",");
    }
    f.join("|")
}

fn orders() -> Options {
    Options::default().compare(&[ORDER]).mask(known)
}

/// Replay a scenario and check both legs; the outcome for more checks.
#[track_caller]
fn check(name: &str, opts: Options) -> Outcome {
    let sc = load_scenario(name);
    let o = replay(&sc, &opts);
    if std::env::var_os("IBX_SCENARIO_DUMP").is_some() {
        dump(name, &o);
    }
    o.assert_same();
    // Once more as a session opened by a logon runs: the requests paced
    // and the market data sent in passes, by the times of the recording
    // (ibx#563). The same messages in the same order, the same callbacks.
    let paced = replay(&sc, &opts.clone().paced());
    if std::env::var_os("IBX_SCENARIO_DUMP").is_some() {
        dump(&format!("{name} (paced)"), &paced);
    }
    paced.assert_same();
    assert_eq!(paced.frames_compared, o.frames_compared, "{name}: frames compared with the pacing on");
    o
}

use ibx::test_support::scenario::filters::{core_model, dividends_cancel, exchange_map, exchange_map_or_aapl,
    without_fundamentals_and_map};

// ── Orders ──

// A LMT order before the open, then its cancel (26/09/2026, ibx#472,
// ibx#465): the new order and the cancel as the reference's; openOrder and
// orderStatus for each report (PreSubmitted three times), the cancel acted
// on after the next report, then Cancelled and 202. Recorded again on
// 10.51 with the official client at its own level, 225, market closed
// (18/10/2026, ibx#604): the same answers, 399 first.
#[test]
fn lmt_order_then_cancel() {
    for name in ["gw1051/20261018/lmt_cancel", "20260926/lmt_cancel"] {
        let o = check(name, orders());
        assert_eq!(o.frames_compared, 2, "{name}");
        assert_eq!(o.theirs.len(), 9, "{name}");
    }
}

// A LMT order, its cancel, a modify after the cancel (26/09/2026, ibx#463,
// ibx#472): PendingCancel from the working reports after the cancel
// request, Cancelled with 1 remaining, 202. The answer to the modify of the
// cancelled order is not in the recording (the client left): the request is
// left out. ibx gives 104 there (ORDER-MODIFY 104: a cancelled order is not
// in a modifiable state).
#[test]
fn cancel_gives_pending_cancel_then_cancelled() {
    let o = check("20260926/modify_cancelled", orders().skip_seqs(&[2441]));
    assert!(o.theirs.iter().any(|(_, l)| l.starts_with("orderStatus|2|PendingCancel")));
}

// The same on 10.51 at level 225, market closed (18/10/2026, ibx#604),
// with the answer to the modify: PendingCancel twice, Cancelled, 202, then
// 104 for the modify and nothing sent.
#[test]
fn cancel_gives_pending_cancel_then_cancelled_and_the_modify_104() {
    let o = check("gw1051/20261018/modify_cancelled", orders());
    assert_eq!(o.frames_compared, 2);
    let status: Vec<&str> = o.theirs.iter().filter(|(_, l)| l.starts_with("orderStatus|"))
        .map(|(_, l)| l.split('|').nth(2).unwrap_or("")).collect();
    assert_eq!(status, ["PreSubmitted", "PendingCancel", "PendingCancel", "Cancelled"]);
    let last = &o.theirs.last().expect("callbacks").1;
    assert!(last.starts_with("error|") && last.contains("|104|"), "{last}");
}

// The modify of a cancelled order, with its answer (14/10/2026 in RTH, the
// reference 10.51 with the client at level 214, ibx#515): a far LMT, its
// cancel 4 s later (Cancelled, 202), then the same order modified 2 s
// after: 104, and nothing sent to the server. Recorded twice: the server
// reported the order pending then working (PreSubmitted, Submitted), and in
// the other run working twice (Submitted twice).
#[test]
fn modify_of_a_cancelled_order() {
    for (name, first) in [("gw1051/20261014/modify_cancelled", "PreSubmitted"),
                          ("gw1051/20261014/modify_cancelled_no_pending", "Submitted")] {
        let o = check(name, orders());
        // The new order and the cancel; the modify sends nothing.
        assert_eq!(o.frames_compared, 2, "{name}");
        let status: Vec<&str> = o.theirs.iter().filter(|(_, l)| l.starts_with("orderStatus|"))
            .map(|(_, l)| l.split('|').nth(2).unwrap_or("")).collect();
        assert_eq!(status, [first, "Submitted", "Cancelled"], "{name}");
        let last = &o.theirs.last().expect("callbacks").1;
        assert!(last.starts_with("error|") && last.contains("|104|"), "{name}: {last}");
    }
}

// The cancel of an unknown order (26/09/2026, ibx#464): 10147, nothing on
// the wire.
#[test]
fn cancel_of_an_unknown_order() {
    for name in ["gw1051/20261018/cancel_unknown", "20260926/cancel_unknown"] {
        let o = check(name, orders());
        assert_eq!(o.frames_compared, 0, "{name}");
        assert_eq!(o.theirs.len(), 1, "{name}");
        assert!(o.theirs[0].1.starts_with("error|"), "{name}: {:?}", o.theirs);
    }
}

// Two orders in one OCA group, both cancelled (26/09/2026, ibx#311,
// ibx#329): 583 and 6209 on the wire; the second cancel, after the first
// cancelled the group, refused with 10148.
#[test]
fn oca_group_and_the_refused_second_cancel() {
    for name in ["gw1051/20261018/oca_group", "20260926/oca_group"] {
        let o = check(name, orders());
        assert_eq!(o.frames_compared, 3, "{name}");
    }
}

// Stop, trailing, IOC, FOK, time in force values, a customer account
// refusal, OPG, a fill and a modify of the filled order before the open
// (28/09/2026, ibx#473, ibx#463): the openOrder and orderStatus sequence of
// each order event, the execution and commission of the fill, 104 for the
// modify of the filled order. The market data requests between the orders
// are left out (see `top_of_book_after_orders_on_the_contract`).
// Recorded again on 10.51 at level 225 (19/10/2026, pre-market, ibx#604).
#[test]
fn premarket_order_types_and_order_events() {
    for name in ["gw1051/20261019pre/premarket_order_types", "20260928/premarket_order_types"] {
        let sc = load_scenario(name);
        let md: Vec<u64> = sc.recs.iter()
            .filter(|r| matches!(r.msg.as_str(), "REQ_MKT_DATA" | "CANCEL_MKT_DATA" | "REQ_MARKET_DATA_TYPE"))
            .map(|r| r.seq).collect();
        let o = check(name, orders().skip_seqs(&md)
            .keep(|l| !l.starts_with("marketDataType") && !l.starts_with("tick") && !l.starts_with("error|9003|")));
        // 10.51 refuses GTX and NMIN at once with 10052 (two orders fewer).
        assert!(o.frames_compared >= 23, "{name}: {}", o.frames_compared);
        if name.starts_with("gw1051/") {
            assert!(o.theirs.iter().any(|(_, l)| l.contains("|10052|Invalid time in force:GTX")), "{name}");
        } else {
            // The 10.51 run's marketable buy was not filled (04:49 New
            // York): its fill and modify are `premarket_fill_then_modify_on_10_51`.
            assert!(o.theirs.iter().any(|(_, l)| l.contains("|104|Cannot modify a filled order")), "{name}");
            assert!(o.theirs.iter().any(|(_, l)| l.starts_with("execDetails|")), "{name}");
        }
    }
}

// The fill and modify of the same scenario on 10.51 at level 225, recorded
// again at 07:48 New York (19/10/2026, ibx#604; the 04:49 run did not
// fill): the marketable buy filled, its execution, 104 for the modify of
// the filled order, the sell back filled. The market data request is left
// out.
#[test]
fn premarket_fill_then_modify_on_10_51() {
    let name = "gw1051/20261019preb/premarket_fill_modify";
    let sc = load_scenario(name);
    let md: Vec<u64> = sc.recs.iter()
        .filter(|r| matches!(r.msg.as_str(), "REQ_MKT_DATA" | "CANCEL_MKT_DATA" | "REQ_MARKET_DATA_TYPE"))
        .map(|r| r.seq).collect();
    let o = check(name, orders().skip_seqs(&md).keep(|l| !l.starts_with("marketDataType") && !l.starts_with("tick")));
    assert!(o.theirs.iter().any(|(_, l)| l.contains("|104|Cannot modify a filled order")));
    assert_eq!(o.theirs.iter().filter(|(_, l)| l.starts_with("execDetails|")).count(), 2);
}

// The market data part of the same scenario: AAPL top of book after the
// orders on AAPL (BMW and 7203 go to other farms: left out): the first
// quote message gives the trade fields (lastTimestamp, last, lastSize, volume,
// close) to the request, also when the orders registered the contract
// before (ibx#487: they went to the orders' slot).
#[test]
fn top_of_book_after_orders_on_the_contract() {
    for (name, skip, until) in [("gw1051/20261019pre/premarket_order_types", [2167, 2168, 2706, 2707], 2773),
        ("20260928/premarket_order_types", [21961, 21962, 22575, 22576], 22599)] {
        check(name, Options::default().mask(known).skip_seqs(&skip).until(until)
            .keep(|l| !["9002", "9003"].contains(&l.split('|').nth(1).unwrap_or(""))));
    }
}

// OVERNIGHT time in force (28/09/2026): the orders and the refused change
// of the time in force (462); the order directed to the OVERNIGHT exchange
// discarded by the redirect precaution: 10329, Cancelled, 201, then 103
// for its id placed again and 10147 for its cancel (ibx#486).
#[test]
fn overnight_time_in_force() {
    let o = check("20260928/overnight_tif", orders());
    assert!(o.theirs.iter().any(|(_, l)| l.starts_with("error|2|10329|")));
}

// The same orders on the reference 10.51, the official client at level 225
// (19/10/2026, overnight session, ibx#604): the OVERNIGHT and OVERNIGHT +
// DAY time in force texts refused with 10052 whatever the exchange; a DAY
// order with includeOvernight placed, changed and cancelled.
#[test]
fn overnight_time_in_force_on_10_51() {
    for name in ["i196_overnight_smart", "i196_overnight_exch", "i196_overnight_day"] {
        let o = check(&format!("gw1051/20261019ov/{name}"), orders());
        assert!(o.theirs.iter().any(|(_, l)| l.contains("|10052|")), "{name}");
    }
    check("gw1051/20261019ov/i196_day_include_overnight", orders());
}

// The session's order types (28/09/2026, RTH): SPY and QQQ top of book,
// contract details, PEG BENCH with its replace and cancel, the OVERNIGHT
// orders, up to the second SPY request (src/golden/l1.rs
// `spy_and_qqq_in_the_session`), the directed OVERNIGHT order discarded
// (ibx#486). Left out as in src/golden/orders.rs: TRAIL MIT, TRAIL LIT,
// PASSV REL, RPI and PEG BEST (ibx#469). The commission reports (no order
// id) are those of the fills of the orders left out.
// The exchange map cancel of QQQ (seq 5489) has no API source field: the QQQ request
// was cancelled before the map came; those of AAPL and SPY in other
// recordings have the API socket source, their requests still running (02/10/2026
// seq 2461 and 2590, 28/09/2026 seq 5292 and 22088) (ibx#487).
#[test]
fn rth_order_types() {
    // Recorded again on 10.51 at level 225 (19/10/2026, ibx#604): the
    // orders numbered 15 to 28, the second SPY request at 2217; the
    // gateway's own exchange map entries are left out (the reference 10.51
    // cancels SPY's once the map came, before the QQQ request; nothing the
    // client sees).
    for (name, first, until) in [("gw1051/20261019rth/rth_order_types", 15, 2217), ("20260928/rth_order_types", 1, 6581)] {
        let skip: Vec<i64> = (first..first + 7).collect();
        let mut opts = Options::default().mask(known).skip_orders(&skip).until(until)
            .keep(|l| !l.starts_with("commissionAndFeesReport"));
        if name.starts_with("gw1051/") {
            opts = opts.skip_frame(exchange_map);
        }
        let o = check(name, opts);
        assert!(o.frames_compared >= 10, "{name}: {}", o.frames_compared);
    }
}

// A SMART stock combo (SPY, QQQ) bought and sold in RTH (30/09/2026,
// ibx#474, ibx#471, ibx#315): positions, top of book, the combo orders,
// the fills of the combo and of each leg (execDetails, commission reports,
// orderStatus Filled). The reference had QQQ's exchange map from earlier in
// its session: the exchange map frames and the top of book of QQQ (request
// 9501, whose bid and ask exchanges come from that map) are left out; and
// its positions (reqPositions at the start): the position callbacks are
// left out.
#[test]
fn combo_fill_executions_and_commissions() {
    // Recorded again on 10.51 at level 225 (19/10/2026, ibx#604).
    for name in ["gw1051/20261019rth/i105_combo_fill", "20260930/i105_combo_fill"] {
        let o = check(name, Options::default().mask(known).skip_frame(exchange_map)
            .keep(|l| !l.starts_with("position") && l.split('|').nth(1) != Some("9501")));
        assert!(o.theirs.iter().any(|(_, l)| l.starts_with("execDetails|")), "{name}");
        assert!(o.theirs.iter().any(|(_, l)| l.starts_with("commissionAndFeesReport|")), "{name}");
    }
}

// Partial fills in RTH (14/10/2026, the reference 10.51 with the client
// at level 214, ibx#515): three rounds of a marketable limit buying 300 F
// and its sale; the second sale filled in two executions (200 and 100),
// each with its execution, openOrder and orderStatus, then two commission
// reports; reqExecutions after each buy lists every execution of the
// session, the split sale as two rows.
// The replay stops before the closing global cancel, whose answers depend
// on how long ago each order filled (ibx#551) and on the positions of the
// session's logon, which the recording does not hold. The kept bid and ask
// of each new request of F come from the farm's data that came after the
// earlier request's cancel (ibx#694).
#[test]
fn partial_fills_then_executions() {
    let o = check("gw1051/20261014/partial_fills", Options::default().mask(known).skip_frame(exchange_map).until(2137));
    // Seven executions as they came (the split sale twice), then 1, 3
    // and 6 rows of reqExecutions.
    assert_eq!(o.theirs.iter().filter(|(_, l)| l.starts_with("execDetails|")).count(), 17);
    assert_eq!(o.theirs.iter().filter(|(_, l)| l.starts_with("execDetails|-1|") && l.contains("|SLD|200|")).count(), 1);
}

// A SMART stock combo whose symbol does not match its legs (26/09/2026,
// ibx#470): the definition lookups of the combo and its legs, the
// multiplier and leg confirmation queries, then 478; the cancels give
// 10147. Recorded again on 10.51 at level 225, market closed, one order
// and its cancel (18/10/2026, ibx#604).
#[test]
fn combo_symbol_conflict() {
    let o = check("20260926/i105_combo_stock_smart", Options::default());
    assert_eq!(o.theirs.iter().filter(|(_, l)| l.contains("|478|")).count(), 2);
    let o = check("gw1051/20261018c/i105_combo_symbol_conflict", Options::default());
    assert_eq!(o.theirs.iter().filter(|(_, l)| l.contains("|478|")).count(), 1);
}

// The combo orders of the second session (26/09/2026, ibx#470): the
// reference had the combo's definitions from its first session; ibx's
// lookups are answered from that session's replies.
#[test]
fn combo_orders_price_change_and_cancel() {
    let first = load_scenario("20260926/i105_combo_stock_smart");
    check("20260926b/i105_combo_stock_smart", Options::default().mask(known).replies_from(&first));
}

#[test]
fn combo_per_leg_prices() {
    let first = load_scenario("20260926/i105_combo_stock_smart");
    check("20260926b/i105_combo_leg_prices", Options::default().mask(known).replies_from(&first));
}

// The combo orders on 10.51 at level 225, market closed (18/10/2026,
// ibx#604): SMART, SPY and QQQ with the symbol in the legs' order (no
// conflict: 399, PreSubmitted, the price change, the cancel) in the first
// session of the gateway's process, then in the second one with the
// definitions known, and per-leg prices.
#[test]
fn combo_orders_on_10_51() {
    let first = load_scenario("gw1051/20261018/i105_combo_stock_smart");
    let o = check("gw1051/20261018/i105_combo_stock_smart", Options::default().mask(known));
    assert!(o.theirs.iter().any(|(_, l)| l.contains("|399|")), "{:?}", o.theirs);
    for name in ["gw1051/20261018b/i105_combo_stock_smart", "gw1051/20261018b/i105_combo_leg_prices"] {
        let o = check(name, Options::default().mask(known).replies_from(&first));
        assert!(o.frames_compared >= 2, "{name}: {}", o.frames_compared);
    }
}

// A combo directed to ARCA (26/09/2026): no definition, 200. The cancel
// that follows gives orderStatus ApiCancelled (the order id stays in the
// reference's API pending map, ORDER-CANCEL 1: permId 0, remaining 1, the
// prices unset), nothing on the wire.
#[test]
fn directed_combo_without_definition_then_cancel() {
    let o = check("20260926b/i105_combo_directed", Options::default());
    assert!(o.theirs.iter().any(|(_, l)| l.starts_with("orderStatus|31|ApiCancelled|")));
    // 10.51 at level 225, market closed (18/10/2026, ibx#604).
    let o = check("gw1051/20261018b/i105_combo_directed", Options::default());
    assert!(o.theirs.iter().any(|(_, l)| l.contains("|200|")), "{:?}", o.theirs);
    assert!(o.theirs.iter().any(|(_, l)| l.contains("|ApiCancelled|")), "{:?}", o.theirs);
}

// A bracket in the client library's form (26/09/2026): parent and
// take-profit with transmit off, the stop with transmit on.
#[test]
fn bracket() {
    for name in ["gw1051/20261018/bracket", "gw1051/20261018b/bracket", "20260926/bracket", "20260926b/bracket"] {
        check(name, orders());
    }
}

// ── Account ──

// reqAccountSummary of four tags and $LEDGER:ALL, then its cancel
// (26/09/2026, ibx#479): the subscription and its cancel as the
// reference's, the four tag rows. The whole answer: the test below
// (ibx#486).
#[test]
fn account_summary_subscription_and_tag_rows() {
    let o = replay(&load_scenario("20260926/account_summary"), &Options::default().compare(&[SUBSCRIPTION]));
    assert_eq!((o.frame_error.as_deref(), o.frames_compared), (None, 2));
    let theirs: Vec<String> = o.theirs.iter().map(|(_, l)| l.clone()).collect();
    ibx::test_support::scenario::assert_same_callbacks(&o.ours[..4], &theirs[..4]);
}

// Recorded in a gateway process that had served account updates in an
// earlier session: each frame twice (ibx#752).
#[test]
fn account_summary_whole_answer() {
    check("20260926/account_summary", Options::default().after_account_updates());
}

// The same request on 10.51 at level 225, market closed (18/10/2026,
// ibx#604): the subscription; the cancel sends nothing (the server's
// cancel comes at the client's disconnect, which the replay has not); the
// four tag rows.
#[test]
fn account_summary_subscription_and_tag_rows_on_10_51() {
    let o = replay(&load_scenario("gw1051/20261018/account_summary"), &Options::default().compare(&[SUBSCRIPTION]).until(2717));
    assert_eq!((o.frame_error.as_deref(), o.frames_compared), (None, 1));
    let theirs: Vec<String> = o.theirs.iter().map(|(_, l)| l.clone()).collect();
    ibx::test_support::scenario::assert_same_callbacks(&o.ours[..4], &theirs[..4]);
}

// The whole answer on 10.51, in a gateway process where no session had
// asked the account updates (18/10/2026, 20261018c): each frame once, the
// tag rows, the $LEDGER:ALL rows of the account "All", the end.
#[test]
fn account_summary_whole_answer_on_10_51() {
    let o = check("gw1051/20261018c/account_summary", Options::default().until(1413));
    assert_eq!(o.theirs.iter().filter(|(_, l)| l.starts_with("accountSummaryEnd|")).count(), 1);
}

// The same request after an earlier session of the gateway's process had
// asked the account updates (20261018): each frame twice (ibx#752).
#[test]
fn account_summary_whole_answer_after_account_updates() {
    let o = check("gw1051/20261018/account_summary", Options::default().until(2717).after_account_updates());
    assert_eq!(o.theirs.iter().filter(|(_, l)| l.starts_with("accountSummaryEnd|")).count(), 2);
}

// reqPnL and reqPnLSingle (26/09/2026, ibx#478): the reference subscribes
// the market data of its positions, known since its logon. The recording
// starts after its session's logon: the positions, seeds and marks the P&L
// is made from are not in it. The P&L is replayed from the recording of
// 10.51 (pnl_first_request_of_the_session, ibx#511).
#[test]
#[ignore = "ibx#511: the recording has not the session state its P&L is made from; see pnl_first_request_of_the_session"]
fn pnl_and_pnl_single() {
    check("20260926b/pnl", Options::default());
}

// Orders of earlier sessions known from the logon replay (01/10/2026,
// paper): 8 orders of client 0 the replay gives as not routed yet (pending-new
// status-only reports, PreSubmitted), then client 193: reqAllOpenOrders lists them in the book's
// order, API order id 0 (no API order id field), permId the id part of the client id, client 0;
// reqGlobalCancel sends the 8 cancels tagged ALL in the book's order, and
// client 193 gets nothing of their reports (the orders are client 0's);
// reqAllOpenOrders then lists none.
#[test]
fn global_cancel_of_orders_of_earlier_sessions() {
    // Recorded again on 10.51 at level 225 (19/10/2026, regular hours,
    // ibx#604): eight SPY orders of client 0 left working, a restart.
    for name in ["gw1051/20261019rth/global_cancel_replayed", "20261001/global_cancel_replayed"] {
        let o = check(name, orders());
        assert_eq!(o.frames_compared, 8, "{name}: the 8 cancels");
        assert_eq!(o.theirs.len(), 18, "{name}");
    }
}

// ── Historical data, scanners, news ──

// keepUpToDate bars, then their cancel (26/09/2026, ibx#429, ibx#431): the
// query, the bars, the end (its times are those of the request), 162 at the
// cancel.
#[test]
fn historical_keep_up_to_date_then_cancel() {
    for name in ["gw1051/20261018/hist_keep_up_to_date", "gw1051/20261018b/hist_keep_up_to_date", "20260926b/hist_keep_up_to_date"] {
        let o = check(name, Options::default());
        assert_eq!(o.theirs.len(), 32, "{name}");
    }
}

// Four keepUpToDate requests (02/10/2026, ibx#429): their bars, ends and
// first updates, up to the second round of updates (see
// `keep_up_to_date_update_order`).
#[test]
fn keep_up_to_date_bars_and_first_updates() {
    for (name, until) in [("gw1051/20261019b1/b1_429_keep_up_to_date", 3770), ("20261002/b1_429_keep_up_to_date", 3650)] {
        let o = check(name, Options::default().until(until));
        assert!(o.theirs.iter().any(|(_, l)| l.starts_with("historicalDataUpdate|9533|")), "{name}");
    }
}

// The later rounds of updates, the cancels and the refusals: the
// reference gives the requests that share one live bar stream (9530 1
// hour, 9531 1 min, 9533 2 hours, all TRADES) in the order 9530, 9533,
// 9531, the order they joined the stream: 9533's query was built after
// the stream's number had come for 9530, 9531 still waited for its own
// (ibx#512). Replayed with each contract lookup answered at its recorded
// point: a query is built when its own lookup is answered, which decides
// that order. The recording ends before the answer to the last cancel,
// which the reference gives as for the cancel before it: left out on
// both sides.
#[test]
fn keep_up_to_date_update_order() {
    for name in ["gw1051/20261019b1/b1_429_keep_up_to_date", "20261002/b1_429_keep_up_to_date"] {
        let o = check(name, Options::default().lookups_at_their_point()
            .keep(|l| !l.starts_with("error|9536|366|")));
        assert!(o.theirs.iter().any(|(_, l)| l.starts_with("error|9535|366|")), "{name}");
    }
}

// Bars with formatDate 1 and 2, head timestamps (02/10/2026, ibx#431); the
// second head timestamp answered from the store, with no query (ibx#486).
#[test]
fn historical_formats_and_head_timestamps() {
    for name in ["gw1051/20261019b1/b1_431_hist_format", "20261002/b1_431_hist_format"] {
        let o = check(name, Options::default());
        assert!(o.theirs.iter().any(|(_, l)| l.starts_with("headTimestamp|9545|")), "{name}");
    }
}

// Historical ticks of AAPL: start, end, both, refusals, AGGTRADES
// (02/10/2026, ibx#432). The EUR.USD requests go to the cash farm, which
// the replay does not run: left out.
#[test]
fn historical_ticks() {
    for name in ["gw1051/20261019b1/b1_432_hist_ticks", "20261002/b1_432_hist_ticks"] {
        let sc = load_scenario(name);
        let eur: Vec<u64> = sc.recs.iter().filter(|r| r.msg == "REQ_HISTORICAL_TICKS" && r.request["contract"]["symbol"] == "EUR")
            .map(|r| r.seq).collect();
        check(name, Options::default().skip_seqs(&eur));
    }
}

// Two scanner subscriptions, then their cancels (26/09/2026, ibx#457):
// the subscriptions that waited for the scanner parameters go out in the
// reference's order, the second request first here, each with an XML
// declaration (ibx#513); the lookups of the rows' contracts in row order.
// The recording ends before the answer to the last cancel, which the
// reference gives (run of 10/10/2026): left out on both sides.
#[test]
fn two_scanner_subscriptions() {
    let o = check("20260926b/scanner_two", Options::default().compare(&[SCANNER, LOOKUP])
        .keep(|l| !l.starts_with("error|9006|162|")));
    assert_eq!(o.frames_compared, 24);
}

// The same on 10.51 at level 225, market closed, the first scanner request
// of the gateway's process (18/10/2026, ibx#604): the parameters asked
// first, the subscriptions without the manual flag of 10.40, the lookups
// of the rows' contracts. The second session of that process
// (20261018b) had the parameters already: not replayed, ibx asks for them
// in each new session.
#[test]
fn two_scanner_subscriptions_on_10_51() {
    let o = check("gw1051/20261018/scanner_two", Options::default().compare(&[SCANNER, LOOKUP])
        .keep(|l| !l.starts_with("error|9006|162|")));
    assert!(o.frames_compared > 20, "{}", o.frames_compared);
}

// News ticks on a contract asked twice, then on a second contract with
// one provider (02/10/2026, ibx#458): the news subscription lists the
// session's news providers, or the one asked (ibx#514). The order of
// marketDataType and tickReqParams on a contract asked before is ibx#535:
// left out. Up to the cancels in both replays; the cancels, grouped by the
// market data passes (ibx#560), in the paced replay of the 10.51 recording
// (ibx#605). The 10.40 recording grouped them in one message for both
// contracts: its own traffic, not compared.
#[test]
fn news_ticks_twice() {
    let opts = || Options::default().compare(&[MARKET_DATA, HISTORICAL]).skip_frame(exchange_map)
        .keep(|l| !l.starts_with("marketDataType|") && !l.starts_with("tickReqParams|"));
    for (name, until) in [("gw1051/20261019b1/b1_458_news_dup", 16439), ("20261002/b1_458_news_dup", 15737)] {
        let o = check(name, opts().until(until));
        assert_eq!(o.frames_compared, 4, "{name}");
    }
    let name = "gw1051/20261019b1/b1_458_news_dup";
    let o = replay(&load_scenario(name), &opts().paced());
    if std::env::var_os("IBX_SCENARIO_DUMP").is_some() {
        dump(name, &o);
    }
    o.assert_same();
    assert_eq!(o.frames_compared, 8, "{name}: the cancels too");
}

fn dump(name: &str, o: &Outcome) {
    eprintln!("== {name}: frames compared {}, error {:?}", o.frames_compared, o.frame_error);
    eprintln!("   unsent {:?}", o.unsent);
    eprintln!("   not made {:?}", o.not_made);
    let theirs: Vec<&String> = o.theirs.iter().map(|(_, l)| l).collect();
    let first = o.ours.iter().zip(&theirs).position(|(a, b)| a != *b).unwrap_or(o.ours.len().min(theirs.len()));
    eprintln!("   callbacks: {} ours, {} reference, first difference {}", o.ours.len(), theirs.len(),
        if first < o.ours.len().max(theirs.len()) { first.to_string() } else { "none".into() });
    if std::env::var_os("IBX_SCENARIO_QUIET").is_some() { return; }
    for k in 0..o.ours.len().max(theirs.len()) {
        let (a, b) = (o.ours.get(k).map_or("", |s| s.as_str()), theirs.get(k).map_or("", |s| s.as_str()));
        eprintln!("{k:4} {} {a}\n          {b}", if a == b { ' ' } else { '*' });
    }
}

#[test]
fn dump_all_scenarios() {
    let Some(only) = std::env::var_os("IBX_SCENARIO_DUMP") else { return };
    let only = only.to_string_lossy().to_string();
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut names = Vec::new();
    // The recordings of 10.40 by their folder, those of 10.51 under gw1051/.
    for (version, prefix) in [("gw1040", ""), ("gw1051", "gw1051/")] {
        for day in std::fs::read_dir(fixtures.join(version).join("scenarios")).unwrap() {
            let day = day.unwrap().path();
            for f in std::fs::read_dir(&day).unwrap() {
                let f = f.unwrap().path();
                let name = f.file_name().unwrap().to_string_lossy().to_string();
                if name.ends_with(".api.jsonl") || !name.ends_with(".jsonl") { continue; }
                names.push(format!("{prefix}{}/{}", day.file_name().unwrap().to_string_lossy(), name.trim_end_matches(".jsonl")));
            }
        }
    }
    names.sort();
    for name in names.iter().filter(|n| only == "all" || n.contains(&only)) {
        let o = replay(&load_scenario(name), &Options::default());
        dump(name, &o);
    }
}

// ── Generic ticks (ibx#450) ──

// AAPL with sixteen generic ticks, SPY with mdoff, 233 and 236, an
// invalid list, EUR.USD with 233 (not valid for a currency pair: nothing
// sent) on the cash farm, MNQ with 588 on the futures farm, in the
// session (05/10/2026): the entries of the ticks that go at once after the
// top of book, the others with the exchange map entry at the
// acknowledgement, their cancels; the API ticks of each block.
#[test]
fn generic_ticks() {
    for name in B2.map(|d| format!("{d}/b2_generic")) {
        let o = check(&name, Options::default().farms(&["cashfarm", "usfuture"]));
        assert!(o.frames_compared > 10, "{name}: {}", o.frames_compared);
    }
}

/// The regular-hours family b2: on 10.51 at level 225 (19/10/2026,
/// ibx#604), and on 10.40 (05/10/2026).
const B2: [&str; 2] = ["gw1051/20261019b2", "20261005"];

/// The seqs of a recording's requests of one kind.
fn seqs_of(name: &str, msg: &str) -> Vec<u64> {
    load_scenario(name).recs.iter().filter(|r| r.leg == "api_out" && r.msg == msg).map(|r| r.seq).collect()
}

// ── Market data errors (ibx#444) ──

// Two request ids on AAPL at once; 7203 on the Tokyo farm without its
// subscription: 354 with the contract named, with type 1, with 233 (its
// entry cancelled with the top of book), 10167 and delayed data with type
// 3, then type 1 again (05/10/2026). The reference had AAPL's exchange map
// from earlier in its session: the map's frames and the exchange letters
// are left out.
#[test]
fn market_data_errors() {
    // The cancel of the last request: the client left before its answer.
    let last_cancel = *seqs_of("20261005/b2_mkt_errors", "CANCEL_MKT_DATA").last().unwrap();
    check("20261005/b2_mkt_errors", Options::default().farms(&["jfarm"]).skip_frame(exchange_map).skip_seqs(&[last_cancel])
        .keep(|l| !l.starts_with("tickString|9650|32|") && !l.starts_with("tickString|9650|33|")
            && !l.starts_with("tickString|9651|32|") && !l.starts_with("tickString|9651|33|")));
    // On 10.51 at level 225 (19/10/2026, ibx#604) the 7203 requests: the
    // reference had kept AAPL's quote and trade from the earlier scenarios
    // of its process, which the replay has not: the AAPL requests and
    // frames are left out.
    let name = "gw1051/20261019b2/b2_mkt_errors";
    let sc = load_scenario(name);
    let skip: Vec<u64> = sc.recs.iter()
        .filter(|r| r.leg == "api_out" && matches!(r.msg.as_str(), "REQ_MKT_DATA" | "CANCEL_MKT_DATA")
            && matches!(r.request["reqId"].as_i64(), Some(9650 | 9651)))
        .map(|r| r.seq).collect();
    let o = check(name, Options::default().farms(&["jfarm"]).skip_frame(exchange_map_or_aapl).skip_seqs(&skip)
        .keep(|l| !matches!(l.split('|').nth(1), Some("9650" | "9651"))));
    assert!(o.theirs.iter().any(|(_, l)| l.contains("|354|")), "{:?}", o.theirs);
}

// ── Tick-by-tick data, real-time bars, trailing stops (05/10/2026) ──

// Tick-by-tick Last, AllLast, BidAsk, MidPoint and BidAsk with ignoreSize
// of AAPL, AllLast with ten past ticks, EUR.USD BidAsk and MidPoint (the
// reference's query on the cash farm), an unknown type (321) (ibx#404,
// ibx#455).
#[test]
fn tick_by_tick_types() {
    for name in B2.map(|d| format!("{d}/b2_tbt")) {
        check(&name, Options::default().hmds_farms(&["cashfarm"]));
    }
}

// Real-time bars of AAPL TRADES asked twice, MIDPOINT, AXTI TRADES (with
// empty bars), then the cancel of the first (ibx#454). The average price
// to its last digit: the reference computes it in 16-digit decimals
// (84.90555555555557, ibx#604).
#[test]
fn real_time_bars_shared() {
    for name in B2.map(|d| format!("{d}/b2_rtbars")) {
        check(&name, Options::default());
    }
}

// Plain TRAIL orders, by amount and by percent: openOrder shows the stop
// price each report gives, as the market moves (ibx#491). The six
// reqOpenOrders are left out: their order is the book's, by permId, and
// ibx's permIds are its own (the listing: src/api/client/tests.rs
// `open_orders_show_the_reported_contract_and_trail_stop`).
#[test]
fn plain_trail_follows_the_server() {
    for name in B2.map(|d| format!("{d}/b2_trail")) {
        let listings: Vec<u64> = seqs_of(&name, "REQ_OPEN_ORDERS").iter().flat_map(|&s| s..=s + 5).collect();
        assert_eq!(listings.len(), 36, "{name}");
        check(&name, orders().skip_seqs(&listings));
    }
}

// Session start (07/10/2026, ibx#487): the server frames of the logon
// burst (the account config, the execution replay of two fills of the
// morning, the working order, the account and portfolio frames), then the
// first API client: the working order with the end of the list (no
// execution and no commission report of the replayed fills), reqPositions,
// reqAccountUpdates (the values in the order of the reference's map, the
// ledger keys, the portfolio rows, the time in the machine's zone, the
// end), reqOpenOrders, reqAllOpenOrders, reqExecutions and the cancel of
// the working order.
// Left out: the version notice 2172 (from the logon frame, which the replay
// has not); the P&L callbacks and the account request of the P&L
// partition: replayed on the recording's time they differ (ibx#757,
// `pnl_callbacks_in_full`); the order of the
// position rows, which is the order of a
// hash set over the real account id: the rows are compared sorted; the
// rows of reqCompletedOrders, which the reference asks the server for
// (ibx#695; the test below), its end compared.
#[test]
fn session_start_first_client() {
    session_start(false);
}

// The completed orders as a set: the reference's order of its rows is not
// the server's, and not known yet (ibx#695).
#[test]
fn session_start_with_completed_orders() {
    session_start(true);
}

fn session_start(completed: bool) {
    let core: fn(&Fields) -> bool = core_model;
    fn left_out(l: &str) -> bool {
        l.starts_with("error|-1|2172|") || l.starts_with("pnl|") || l.starts_with("pnlSingle|") || l.starts_with("error|9831|2150|")
    }
    let keep: fn(&str) -> bool = if completed {
        |l| !left_out(l)
    } else {
        |l| !left_out(l) && !l.starts_with("completedOrder|")
    };
    // Recorded again on 10.51 at level 225 (19/10/2026, pre-market, after
    // a restart of the gateway, ibx#604).
    for name in ["gw1051/20261019b4s/session_start", "20261007/session_start"] {
        session_start_of(name, core, keep);
    }
}

fn session_start_of(name: &str, core: fn(&Fields) -> bool, keep: fn(&str) -> bool) {
    let o = replay(&load_scenario(name),
        &Options::default().compare(&[ORDER, SUBSCRIPTION]).skip_frame(core).keep(keep));
    if std::env::var_os("IBX_SCENARIO_DUMP").is_some() {
        dump(name, &o);
    }
    assert_eq!(o.frame_error, None, "{name}");
    let sorted = |lines: Vec<String>| {
        let mut lines = lines;
        if let (Some(a), Some(b)) = (lines.iter().position(|l| l.starts_with("position|")), lines.iter().position(|l| l == "positionEnd")) {
            lines[a..b].sort();
        }
        if let (Some(a), Some(b)) = (lines.iter().position(|l| l.starts_with("completedOrder|")), lines.iter().position(|l| l == "completedOrdersEnd")) {
            lines[a..b].sort();
        }
        lines
    };
    let theirs = sorted(o.theirs.iter().map(|(_, l)| l.clone()).collect());
    ibx::test_support::scenario::assert_same_callbacks(&sorted(o.ours.clone()), &theirs);
}

// One request per generic tick on AAPL before the open (07/10/2026): 162
// refused with 321; 221 and 232 as the mark price entry (tick 37); the
// fundamentals on their own exchange (tick 47); the short-term volumes
// after the acknowledgement (ticks 63 to 65); the slow mark price (tick
// 79); 460, 577, 587, 614 and 623 not sent for this stock.
#[test]
fn generic_ticks_one_by_one() {
    // Compared: the errors and every tick, the top of book included: a
    // request on a contract asked just before gets the quote the reference
    // kept at its acknowledgement (ibx#508). The version notice comes from
    // the logon frame, which the replay has not.
    fn generic(l: &str) -> bool {
        let f: Vec<&str> = l.split('|').collect();
        match f[0] {
            "error" => f[2] != "2172",
            "tickPrice" | "tickSize" | "tickString" | "tickGeneric" => true,
            _ => false,
        }
    }
    // The option chain request of the recording is not made by the replay.
    // Recorded again on 10.51 at level 225 (19/10/2026, pre-market, ibx#604).
    for name in ["gw1051/20261019b4/b4_generic_rest", "20261007/b4_generic_rest"] {
        let o = replay(&load_scenario(name), &Options::default().keep(generic));
        if std::env::var_os("IBX_SCENARIO_DUMP").is_some() {
            dump(name, &o);
        }
        assert_eq!(o.frame_error.as_deref(), None, "{name}");
        let theirs: Vec<String> = o.theirs.iter().map(|(_, l)| l.clone()).collect();
        ibx::test_support::scenario::assert_same_callbacks(&o.ours, &theirs);
        if name.starts_with("2026") {
            assert_eq!((o.frames_compared, o.ours.len()), (36, 190));
        }
    }
}

// The same ticks in one list on AAPL (07/10/2026): the mark price and the
// slow mark price with the request, the IPO prices and the short-term
// volumes after the acknowledgement, each message in request code order.
// The reference had AAPL's ratios from a request of the minute before: it
// sent no fundamentals entry and gave tick 47 from what it kept; both are
// left out, and so is tick 79: ibx's fundamentals entry takes the place of
// the slow mark price entry among the recorded acknowledgements (its tick
// is checked in `generic_ticks_one_by_one`). The rest of the recording (an
// ETF, an option, a future) is not replayed.
#[test]
fn generic_ticks_in_one_list() {
    fn generic(l: &str) -> bool {
        let f: Vec<&str> = l.split('|').collect();
        match f[0] {
            "error" => f[2] != "2172",
            "tickPrice" => f[2] == "37",
            "tickSize" => matches!(f[2], "63" | "64" | "65"),
            _ => false,
        }
    }
    // Without the fundamentals entry and the exchange map entry (the
    // reference had the map from earlier in its session).
    let kept_before = without_fundamentals_and_map;
    // Up to the SPY request. Recorded again on 10.51 at level 225
    // (19/10/2026, pre-market, ibx#604): 10.51 refuses the list at once
    // with 10358 (no fundamentals on the paper account), nothing sent.
    for (name, until) in [("gw1051/20261019b4/b4_generic_rest2", 8213), ("20261007/b4_generic_rest2", 9363)] {
        let o = replay(&load_scenario(name), &Options::default().until(until).keep(generic).frame_mask(kept_before));
        if std::env::var_os("IBX_SCENARIO_DUMP").is_some() {
            dump(name, &o);
        }
        assert_eq!(o.frame_error, None, "{name}");
        let theirs: Vec<String> = o.theirs.iter().map(|(_, l)| l.clone()).collect();
        ibx::test_support::scenario::assert_same_callbacks(&o.ours, &theirs);
        if name.starts_with("gw1051/") {
            assert!(o.ours.iter().any(|l| l.starts_with("error|9740|10358|")), "{name}: {:?}", o.ours);
        } else {
            assert!(o.ours.len() >= 5, "{name}: {:?}", o.ours);
        }
    }
}

// The same list without 258 on 10.51 at level 225 (19/10/2026, pre-market,
// ibx#604; with 258 the paper account refuses the list with 10358): AAPL,
// the first market data request of its process, up to the SPY request,
// then SPY. Every tick and error compared.
#[test]
fn generic_ticks_in_one_list_on_10_51() {
    fn generic(l: &str) -> bool {
        let f: Vec<&str> = l.split('|').collect();
        matches!(f[0], "error" | "tickPrice" | "tickSize" | "tickString" | "tickGeneric")
    }
    let o = check("gw1051/20261019b4l/b4_generic_rest3", Options::default().keep(generic));
    assert!(o.theirs.iter().any(|(_, l)| l.starts_with("tickPrice|9790|37|")), "{:?}", o.theirs);
}

// A market-to-limit BUY 1 SPY filled before its cancel, then a market SELL
// 1 back, with reqPositions running (15/10/2026 in RTH, the reference 10.51
// at level 214, ibx#515, ibx#707): the cancel refused at once with 10148
// and nothing sent; a position row at each fill before its execution, one
// after the buy's commission report, two after the sale's (it realized
// P&L). The account's positions are those of the reference's first
// answer; its rows are compared as a set, their order coming from the
// account id the recording masks.
#[test]
fn fill_then_positions() {
    let sc = load_scenario("gw1051/20261015/fill_then_positions");
    for opts in [orders().positions_of_the_recording(), orders().positions_of_the_recording().paced()] {
        let o = replay(&sc, &opts.mask(known));
        assert!(o.frame_error.is_none(), "{:?}", o.frame_error);
        assert_eq!(o.frames_compared, 2, "the buy and the sale");
        let snapshot = |lines: &[String]| -> (Vec<String>, Vec<String>) {
            let end = lines.iter().position(|l| l == "positionEnd").expect("the first answer");
            let mut first = lines[..end].to_vec();
            first.sort();
            (first, lines[end..].to_vec())
        };
        let theirs: Vec<String> = o.theirs.iter().map(|(_, l)| l.clone()).collect();
        let (ours_first, ours_rest) = snapshot(&o.ours);
        let (theirs_first, theirs_rest) = snapshot(&theirs);
        assert_eq!(ours_first, theirs_first);
        assert_same_callbacks(&ours_rest, &theirs_rest);
        assert_eq!(ours_rest.iter().filter(|l| l.starts_with("position|")).count(), 5);
    }
}

// A market-to-limit BUY 1 SPY cancelled 0.3 s after it was placed, during
// the lookups of the contract's first order of the session (15/10/2026 in
// RTH, the reference 10.51 at level 214, ibx#706): the symbol lookup, the
// cancel (ApiCancelled, nothing sent), the second lookup; then a market
// SELL 1 SPY goes out after the first lookup alone. reqPositions runs
// (ibx#707). The lookups are answered at their recorded points, so the
// cancel comes while the order waits, as on the reference.
#[test]
fn cancel_during_the_first_orders_lookups() {
    let sc = load_scenario("gw1051/20261015/cancel_during_lookup");
    for opts in [orders().positions_of_the_recording().lookups_at_their_point(),
                 orders().positions_of_the_recording().lookups_at_their_point().paced()] {
        let o = replay(&sc, &opts.mask(known));
        assert!(o.frame_error.is_none(), "{:?}", o.frame_error);
        assert_eq!(o.frames_compared, 1, "the sale only");
        let theirs: Vec<String> = o.theirs.iter().map(|(_, l)| l.clone()).collect();
        let end = |v: &[String]| v.iter().position(|l| l == "positionEnd").expect("the first answer");
        let (a, b) = (end(&o.ours), end(&theirs));
        let (mut ours_first, mut theirs_first) = (o.ours[..a].to_vec(), theirs[..b].to_vec());
        ours_first.sort();
        theirs_first.sort();
        assert_eq!(ours_first, theirs_first);
        assert_same_callbacks(&o.ours[a..], &theirs[b..]);
        assert!(o.ours.iter().any(|l| l.starts_with("orderStatus|") && l.contains("|ApiCancelled|")), "{:?}", o.ours);
    }
}

// The first P&L requests of a session (15/10/2026 after hours, a fresh
// reference 10.51 at level 214, ibx#511, case A of `.tmp/511`): reqPnL and
// reqPnLSingle on AAPL, closed today, their cancel, then a second reqPnL.
// The reference asks its P&L quotes of the five positions in one message
// at the first request, their exchange maps at the acknowledgements, drops
// the dividends at their first data and keeps the rest; nothing at the
// cancels or at the second request.
#[test]
fn pnl_first_request_of_the_session() {
    let sc = load_scenario("gw1051/20261015/pnl_first_request");
    // The dividends' cancels: the reference split the ones of one frame
    // into two messages there, a timing of its threads (one message for
    // them in the unit test of the engine): `dividends_cancel`.
    let o = replay(&sc, &Options::default().compare(&[MARKET_DATA]).skip_frame(dividends_cancel));
    if std::env::var_os("IBX_SCENARIO_DUMP").is_some() {
        dump("gw1051/20261015/pnl_first_request", &o);
    }
    assert_eq!(o.frame_error, None);
    assert_eq!(o.frames_compared, 5, "the quotes, two exchange maps and their cancels");
    // The first value: one row, the server's mark; its daily, unrealized
    // and realized P&L to the last digit (the unrealized one is the value
    // less the exact cost, ibx#713). The values of the next seconds need
    // the time of the recording, which the replay does not give the client.
    let fields = |l: &str| l.split('|').map(str::to_string).collect::<Vec<_>>();
    let (ours, theirs) = (fields(&o.ours[0]), fields(&o.theirs[0].1));
    assert_eq!(&ours[..5], &theirs[..5]);
    // Replayed on the recording's time, the client computing on it too
    // (ibx#515), its first computation as long after the request as the
    // reference's (0.16 s here), so that every price falls on the same side
    // of each computation (ibx#757): the seventeen callbacks of the two
    // requests are equal to the last digit, the values of the second
    // request included.
    // ibx gives one more: at the second computation, the first row alone
    // at its P&L price, before all rows (ibx#746). The reference gave that
    // value in 8 of 10 session starts; this recording is one of the two
    // without it.
    let o = replay(&sc, &Options::default().compare(&[MARKET_DATA]).skip_frame(dividends_cancel).paced());
    assert_eq!(o.frame_error, None);
    let theirs: Vec<String> = o.theirs.iter().map(|(_, l)| l.clone()).collect();
    assert_eq!(theirs.len(), 17);
    let mut ours = o.ours.clone();
    assert_eq!(ours.remove(1), "pnl|9003|-73.19997558593786|42.285724414062315|0", "the first row alone");
    assert_same_callbacks(&ours, &theirs);
}

// The P&L callbacks of the session start recordings (07/10/2026 and
// 19/10/2026) cannot be compared beyond the first value: those recordings
// hold the account link only, without the market data the later values are
// computed from (ibx#757).
