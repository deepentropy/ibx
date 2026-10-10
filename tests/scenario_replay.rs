//! Scenario replay (ibx#487, tests layer C): the scenarios recorded from
//! the reference gateway (tests/fixtures/gw1040/scenarios/, four legs in
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
    let o = replay(&load_scenario(name), &opts);
    if std::env::var_os("IBX_SCENARIO_DUMP").is_some() {
        dump(name, &o);
    }
    o.assert_same();
    o
}

fn exchange_map(f: &Fields) -> bool {
    f.iter().any(|(t, v)| *t == 264 && v == "626")
}

// ── Orders ──

// A LMT order before the open, then its cancel (26/09/2026, ibx#472,
// ibx#465): the new order and the cancel as the reference's; openOrder and
// orderStatus for each report (PreSubmitted three times), the cancel acted
// on after the next report, then Cancelled and 202.
#[test]
fn lmt_order_then_cancel() {
    let o = check("20260926/lmt_cancel", orders());
    assert_eq!(o.frames_compared, 2);
    assert_eq!(o.theirs.len(), 9);
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

// The cancel of an unknown order (26/09/2026, ibx#464): 10147, nothing on
// the wire.
#[test]
fn cancel_of_an_unknown_order() {
    let o = check("20260926/cancel_unknown", orders());
    assert_eq!(o.frames_compared, 0);
    assert_eq!(o.theirs.len(), 1);
    assert!(o.theirs[0].1.starts_with("error|"), "{:?}", o.theirs);
}

// Two orders in one OCA group, both cancelled (26/09/2026, ibx#311,
// ibx#329): 583 and 6209 on the wire; the second cancel, after the first
// cancelled the group, refused with 10148.
#[test]
fn oca_group_and_the_refused_second_cancel() {
    let o = check("20260926/oca_group", orders());
    assert_eq!(o.frames_compared, 3);
}

// Stop, trailing, IOC, FOK, time in force values, a customer account
// refusal, OPG, a fill and a modify of the filled order before the open
// (28/09/2026, ibx#473, ibx#463): the openOrder and orderStatus sequence of
// each order event, the execution and commission of the fill, 104 for the
// modify of the filled order. The market data requests between the orders
// are left out (see `top_of_book_after_orders_on_the_contract`).
#[test]
fn premarket_order_types_and_order_events() {
    let sc = load_scenario("20260928/premarket_order_types");
    let md: Vec<u64> = sc.recs.iter()
        .filter(|r| matches!(r.msg.as_str(), "REQ_MKT_DATA" | "CANCEL_MKT_DATA" | "REQ_MARKET_DATA_TYPE"))
        .map(|r| r.seq).collect();
    let o = check("20260928/premarket_order_types", orders().skip_seqs(&md)
        .keep(|l| !l.starts_with("marketDataType") && !l.starts_with("tick") && !l.starts_with("error|9003|")));
    assert!(o.frames_compared >= 25, "{}", o.frames_compared);
    assert!(o.theirs.iter().any(|(_, l)| l.contains("|104|Cannot modify a filled order")));
    assert!(o.theirs.iter().any(|(_, l)| l.starts_with("execDetails|")));
}

// The market data part of the same scenario: AAPL top of book after the
// orders on AAPL (BMW and 7203 go to other farms: left out): the first
// 35=P gives the trade fields (lastTimestamp, last, lastSize, volume,
// close) to the request, also when the orders registered the contract
// before (ibx#487: they went to the orders' slot).
#[test]
fn top_of_book_after_orders_on_the_contract() {
    check("20260928/premarket_order_types", Options::default().mask(known)
        .skip_seqs(&[21961, 21962, 22575, 22576]).until(22599)
        .keep(|l| !["9002", "9003"].contains(&l.split('|').nth(1).unwrap_or(""))));
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

// The session's order types (28/09/2026, RTH): SPY and QQQ top of book,
// contract details, PEG BENCH with its replace and cancel, the OVERNIGHT
// orders, up to the second SPY request (src/golden/l1.rs
// `spy_and_qqq_in_the_session`), the directed OVERNIGHT order discarded
// (ibx#486). Left out as in src/golden/orders.rs: TRAIL MIT, TRAIL LIT,
// PASSV REL, RPI and PEG BEST (ibx#469). The commission reports (no order
// id) are those of the fills of the orders left out.
// The exchange map cancel of QQQ (seq 5489) has no 6088: the QQQ request
// was cancelled before the map came; those of AAPL and SPY in other
// recordings have 6088=Socket, their requests still running (02/10/2026
// seq 2461 and 2590, 28/09/2026 seq 5292 and 22088) (ibx#487).
#[test]
fn rth_order_types() {
    let o = check("20260928/rth_order_types", Options::default().mask(known).skip_orders(&[1, 2, 3, 4, 5, 6, 7]).until(6581)
        .keep(|l| !l.starts_with("commissionAndFeesReport")));
    assert!(o.frames_compared > 10, "{}", o.frames_compared);
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
    let o = check("20260930/i105_combo_fill", Options::default().mask(known).skip_frame(exchange_map)
        .keep(|l| !l.starts_with("position") && l.split('|').nth(1) != Some("9501")));
    assert!(o.theirs.iter().any(|(_, l)| l.starts_with("execDetails|")));
    assert!(o.theirs.iter().any(|(_, l)| l.starts_with("commissionAndFeesReport|")));
}

// A SMART stock combo whose symbol does not match its legs (26/09/2026,
// ibx#470): the definition lookups of the combo and its legs, the
// multiplier and leg confirmation queries, then 478; the cancels give
// 10147.
#[test]
fn combo_symbol_conflict() {
    let o = check("20260926/i105_combo_stock_smart", Options::default());
    assert_eq!(o.theirs.iter().filter(|(_, l)| l.contains("|478|")).count(), 2);
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

// A combo directed to ARCA (26/09/2026): no definition, 200. The cancel
// that follows gives orderStatus ApiCancelled (the order id stays in the
// reference's API pending map, ORDER-CANCEL 1: permId 0, remaining 1, the
// prices unset), nothing on the wire.
#[test]
fn directed_combo_without_definition_then_cancel() {
    let o = check("20260926b/i105_combo_directed", Options::default());
    assert!(o.theirs.iter().any(|(_, l)| l.starts_with("orderStatus|31|ApiCancelled|")));
}

// A bracket in the client library's form (26/09/2026): parent and
// take-profit with transmit off, the stop with transmit on.
#[test]
fn bracket() {
    check("20260926/bracket", orders());
    check("20260926b/bracket", orders());
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

#[test]
fn account_summary_whole_answer() {
    check("20260926/account_summary", Options::default());
}

// reqPnL and reqPnLSingle (26/09/2026, ibx#478): the reference subscribes
// the market data of its positions, known since its logon.
#[test]
#[ignore = "ibx#511: the P&L of the reference: its quotes, the account request of its partition, its first values (see session_start_first_client)"]
fn pnl_and_pnl_single() {
    check("20260926b/pnl", Options::default());
}

// Orders of earlier sessions known from the logon replay (01/10/2026,
// paper): 8 orders of client 0 the replay gives as not routed yet (150=A
// 20=3 39=A), then client 193: reqAllOpenOrders lists them in the book's
// order, API order id 0 (no 6121), permId the ClOrdID's id, client 0;
// reqGlobalCancel sends the 8 cancels tagged ALL in the book's order, and
// client 193 gets nothing of their reports (the orders are client 0's);
// reqAllOpenOrders then lists none.
#[test]
fn global_cancel_of_orders_of_earlier_sessions() {
    let o = check("20261001/global_cancel_replayed", orders());
    assert_eq!(o.frames_compared, 8, "the 8 cancels");
    assert_eq!(o.theirs.len(), 18);
}

// ── Historical data, scanners, news ──

// keepUpToDate bars, then their cancel (26/09/2026, ibx#429, ibx#431): the
// query, the bars, the end (its times are those of the request), 162 at the
// cancel.
#[test]
fn historical_keep_up_to_date_then_cancel() {
    let o = check("20260926b/hist_keep_up_to_date", Options::default());
    assert_eq!(o.theirs.len(), 32);
}

// Four keepUpToDate requests (02/10/2026, ibx#429): their bars, ends and
// first updates, up to the second round of updates (see
// `keep_up_to_date_update_order`).
#[test]
fn keep_up_to_date_bars_and_first_updates() {
    let o = check("20261002/b1_429_keep_up_to_date", Options::default().until(3650));
    assert!(o.theirs.iter().any(|(_, l)| l.starts_with("historicalDataUpdate|9533|")));
}

// The later rounds of updates, the cancels and the refusals: the
// reference gives the requests that share one live bar stream (9530 1
// hour, 9531 1 min, 9533 2 hours, all TRADES) in the order 9530, 9533,
// 9531; ibx in request order. The rule of the reference's order is not
// read yet.
#[test]
#[ignore = "ibx#512: the order of the updates of keepUpToDate requests that share a live bar stream"]
fn keep_up_to_date_update_order() {
    check("20261002/b1_429_keep_up_to_date", Options::default());
}

// Bars with formatDate 1 and 2, head timestamps (02/10/2026, ibx#431); the
// second head timestamp answered from the store, with no query (ibx#486).
#[test]
fn historical_formats_and_head_timestamps() {
    let o = check("20261002/b1_431_hist_format", Options::default());
    assert!(o.theirs.iter().any(|(_, l)| l.starts_with("headTimestamp|9545|")));
}

// Historical ticks of AAPL: start, end, both, refusals, AGGTRADES
// (02/10/2026, ibx#432). The EUR.USD requests go to the cash farm, which
// the replay does not run: left out.
#[test]
fn historical_ticks() {
    let sc = load_scenario("20261002/b1_432_hist_ticks");
    let eur: Vec<u64> = sc.recs.iter().filter(|r| r.msg == "REQ_HISTORICAL_TICKS" && r.request["contract"]["symbol"] == "EUR")
        .map(|r| r.seq).collect();
    check("20261002/b1_432_hist_ticks", Options::default().skip_seqs(&eur));
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

// News ticks on a contract asked twice, then on a second contract with
// one provider (02/10/2026, ibx#458): the news subscription lists the
// session's news providers, or the one asked (ibx#514). Stops before the
// cancels, which the reference groups into one message per kind
// (ibx#555). The order of marketDataType and tickReqParams on a contract
// asked before is ibx#535: left out.
#[test]
fn news_ticks_twice() {
    let o = check("20261002/b1_458_news_dup", Options::default().compare(&[MARKET_DATA, HISTORICAL])
        .skip_frame(exchange_map).until(15737)
        .keep(|l| !l.starts_with("marketDataType|") && !l.starts_with("tickReqParams|")));
    assert_eq!(o.frames_compared, 4);
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
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/gw1040/scenarios");
    let mut names = Vec::new();
    for day in std::fs::read_dir(&root).unwrap() {
        let day = day.unwrap().path();
        for f in std::fs::read_dir(&day).unwrap() {
            let f = f.unwrap().path();
            let name = f.file_name().unwrap().to_string_lossy().to_string();
            if name.ends_with(".api.jsonl") || !name.ends_with(".jsonl") { continue; }
            names.push(format!("{}/{}", day.file_name().unwrap().to_string_lossy(), name.trim_end_matches(".jsonl")));
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
    let o = check("20261005/b2_generic", Options::default().farms(&["cashfarm", "usfuture"]));
    assert!(o.frames_compared > 10, "{}", o.frames_compared);
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
    check("20261005/b2_mkt_errors", Options::default().farms(&["jfarm"]).skip_frame(exchange_map).skip_seqs(&[18512])
        .keep(|l| !l.starts_with("tickString|9650|32|") && !l.starts_with("tickString|9650|33|")
            && !l.starts_with("tickString|9651|32|") && !l.starts_with("tickString|9651|33|")));
}

// ── Tick-by-tick data, real-time bars, trailing stops (05/10/2026) ──

// Tick-by-tick Last, AllLast, BidAsk, MidPoint and BidAsk with ignoreSize
// of AAPL, AllLast with ten past ticks, EUR.USD BidAsk and MidPoint (the
// reference's query on the cash farm), an unknown type (321) (ibx#404,
// ibx#455).
#[test]
fn tick_by_tick_types() {
    check("20261005/b2_tbt", Options::default().hmds_farms(&["cashfarm"]));
}

// Real-time bars of AAPL TRADES asked twice, MIDPOINT, AXTI TRADES (with
// empty bars), then the cancel of the first (ibx#454). The average price
// is compared to 12 decimals: one AXTI bar's differs by one unit in the
// last place (84.90555555555557 for 84.90555555555555), an order of the
// reference's arithmetic not read yet.
#[test]
fn real_time_bars_shared() {
    check("20261005/b2_rtbars", Options::default().mask(|l| {
        let mut f: Vec<String> = l.split('|').map(str::to_string).collect();
        if f[0] == "realtimeBar" && f.len() == 10 && let Ok(w) = f[8].parse::<f64>() {
            f[8] = format!("{w:.12}");
        }
        f.join("|")
    }));
}

// Plain TRAIL orders, by amount and by percent: openOrder shows the stop
// price each report gives, as the market moves (ibx#491). The six
// reqOpenOrders are left out: their order is the book's, by permId, and
// ibx's permIds are its own (the listing: src/api/client/tests.rs
// `open_orders_show_the_reported_contract_and_trail_stop`).
#[test]
fn plain_trail_follows_the_server() {
    let listings: Vec<u64> = [15585u64, 15768, 15964, 16182, 16349, 16522].iter().flat_map(|&s| s..=s + 5).collect();
    check("20261005/b2_trail", orders().skip_seqs(&listings));
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
// has not); the P&L callbacks and the account request of the P&L partition
// (the test below); the order of the position rows, which is the order of a
// hash set over the real account id: the rows are compared sorted.
#[test]
fn session_start_first_client() {
    let core = |f: &Fields| f.iter().any(|(t, v)| *t == 6700 && v == "Core");
    let keep = |l: &str| !(l.starts_with("error|-1|2172|") || l.starts_with("pnl|") || l.starts_with("pnlSingle|")
        || l.starts_with("error|9831|2150|"));
    let o = replay(&load_scenario("20261007/session_start"),
        &Options::default().compare(&[ORDER, SUBSCRIPTION]).skip_frame(core).keep(keep));
    if std::env::var_os("IBX_SCENARIO_DUMP").is_some() {
        dump("20261007/session_start", &o);
    }
    assert_eq!(o.frame_error, None);
    let sorted = |lines: Vec<String>| {
        let mut lines = lines;
        if let (Some(a), Some(b)) = (lines.iter().position(|l| l.starts_with("position|")), lines.iter().position(|l| l == "positionEnd")) {
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
    // Compared: the errors and the ticks of the generic entries. The top of
    // book is left out: a request on a contract asked just before gets the
    // quote the reference kept (ibx#508). The version notice comes from the
    // logon frame, which the replay has not.
    fn generic(l: &str) -> bool {
        let f: Vec<&str> = l.split('|').collect();
        match f[0] {
            "error" => f[2] != "2172",
            "tickPrice" => matches!(f[2], "37" | "78" | "79" | "96" | "97" | "98" | "99"),
            "tickSize" => matches!(f[2], "63" | "64" | "65"),
            "tickString" => f[2] == "47",
            "tickGeneric" => true,
            _ => false,
        }
    }
    // The option chain request of the recording is not made by the replay.
    let o = replay(&load_scenario("20261007/b4_generic_rest"), &Options::default().keep(generic));
    assert_eq!((o.frame_error.as_deref(), o.frames_compared), (None, 36));
    let theirs: Vec<String> = o.theirs.iter().map(|(_, l)| l.clone()).collect();
    ibx::test_support::scenario::assert_same_callbacks(&o.ours, &theirs);
    assert_eq!(o.ours.len(), 22);
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
    // reference had the map from earlier in its session): their groups of
    // fields, and as many less in the entry count.
    fn kept_before(f: &mut Fields) {
        while let Some(at) = f.iter().position(|(t, v)| *t == 264 && (v == "258" || v == "626")) {
            let start = f[..at].iter().rposition(|(t, _)| *t == 262).unwrap_or(at);
            let end = f[at + 1..].iter().position(|(t, _)| *t == 262).map_or(f.len(), |n| at + 1 + n);
            f.drain(start..end);
            if let Some(count) = f.iter_mut().find(|(t, _)| *t == 146) {
                count.1 = (count.1.parse::<usize>().unwrap_or(1) - 1).to_string();
            }
        }
    }
    let o = replay(&load_scenario("20261007/b4_generic_rest2"),
        &Options::default().until(9363).keep(generic).frame_mask(kept_before));
    if std::env::var_os("IBX_SCENARIO_DUMP").is_some() {
        dump("20261007/b4_generic_rest2", &o);
    }
    assert_eq!(o.frame_error, None);
    let theirs: Vec<String> = o.theirs.iter().map(|(_, l)| l.clone()).collect();
    ibx::test_support::scenario::assert_same_callbacks(&o.ours, &theirs);
    assert!(o.ours.len() >= 5, "{:?}", o.ours);
}
