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
/// own ignored test in src/golden/orders.rs (ibx#486):
/// - warning 399 names the listing exchange of the contract's definition
///   (NASDAQ.NMS), which the reference looked up; ibx has no definition of
///   a contract placed by conId and names the order's exchange
///   (`order_message_names_the_listing_exchange`);
/// - the first openOrder of a STP order shows a limit price the wire does
///   not carry (`stp_first_open_order_limit_price`).
fn known(line: &str) -> String {
    let mut f: Vec<String> = line.split('|').map(str::to_string).collect();
    if f[0] == "error" && f.get(2).map(String::as_str) == Some("399") {
        let mut rows: Vec<String> = f[3].split('\n').map(str::to_string).collect();
        if rows.len() == 3 {
            let mut words: Vec<&str> = rows[1].split(' ').collect();
            if let Some(w) = words.last_mut() { *w = "{exchange}"; }
            rows[1] = words.join(" ");
        }
        f[3] = rows.join("\n");
    }
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
// of the time in force (462), up to the order directed to the OVERNIGHT
// exchange (src/golden/orders.rs, `overnight_directed_order`, ibx#486).
#[test]
fn overnight_time_in_force() {
    check("20260928/overnight_tif", orders().until(1237));
}

// The session's order types (28/09/2026, RTH): SPY and QQQ top of book,
// contract details, PEG BENCH with its replace and cancel, the OVERNIGHT
// orders, up to the second SPY request (src/golden/l1.rs
// `spy_and_qqq_in_the_session`). Left out as in src/golden/orders.rs: TRAIL MIT, TRAIL LIT,
// PASSV REL, RPI and PEG BEST (ibx#469), the directed OVERNIGHT order
// (ibx#486). The cancel of an exchange map (264=626) carries 6088=Socket in
// some recordings and not in others (ibx#487, see
// `exchange_map_cancel_source`): left out of that frame. The commission
// reports (no order id) are those of the fills of the orders left out.
#[test]
fn rth_order_types() {
    check("20260928/rth_order_types", Options::default().mask(known).skip_orders(&[1, 2, 3, 4, 5, 6, 7, 10]).until(6581)
        .frame_mask(|f| if exchange_map(f) { f.retain(|(t, _)| *t != 6088) })
        .keep(|l| !l.starts_with("commissionAndFeesReport")));
}

// The exchange map cancel of QQQ (seq 5489) has no 6088; those of AAPL and
// SPY in other recordings have 6088=Socket (02/10/2026 seq 2461 and 2590,
// 28/09/2026 seq 5292 and 22088); ibx always writes it.
#[test]
#[ignore = "ibx#487: the 6088 of an exchange map cancel (264=626) follows a rule not read yet"]
fn exchange_map_cancel_source() {
    check("20260928/rth_order_types", Options::default().mask(known).skip_orders(&[1, 2, 3, 4, 5, 6, 7, 10]).until(6581));
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
#[ignore = "ibx#226: transmit=false orders are refused instead of held for the group"]
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

// reqAccountUpdates and accountDownloadEnd (26/09/2026, ibx#475, ibx#476):
// the reference answers from the account it keeps since its logon, with
// no frame; the recording does not hold that session start.
#[test]
#[ignore = "ibx#487: needs a recording with the session start (the account values come from the logon subscription)"]
fn account_updates_and_download_end() {
    check("20260926/account_updates", Options::default());
}

// reqPnL and reqPnLSingle (26/09/2026, ibx#478): the reference subscribes
// the market data of its positions, known since its logon.
#[test]
#[ignore = "ibx#487: needs a recording with the session start (positions)"]
fn pnl_and_pnl_single() {
    check("20260926b/pnl", Options::default());
}

// reqAllOpenOrders and reqPositions (02/10/2026, ibx#477): both answered
// from the reference's state (orders and positions of the session start).
#[test]
#[ignore = "ibx#487: needs a recording with the session start (open orders, positions)"]
fn open_orders_and_positions() {
    check("20261002/b1_cleanup", Options::default());
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
#[ignore = "ibx#429: the order of the updates of keepUpToDate requests that share a live bar stream"]
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
// the reference sends the subscriptions that waited for the scanner
// parameters in the reverse order of the requests, each with an XML
// declaration; ibx sends them in request order with none.
#[test]
#[ignore = "ibx#457: scanner subscriptions in reverse order, XML declaration"]
fn two_scanner_subscriptions() {
    check("20260926b/scanner_two", Options::default().compare(&[SCANNER, LOOKUP]));
}

// News ticks on a contract asked twice (02/10/2026, ibx#458): the
// reference's subscription of tick 292 lists the news providers (6472).
#[test]
#[ignore = "ibx#458: 292 subscription without the provider list 6472"]
fn news_ticks_twice() {
    check("20261002/b1_458_news_dup", Options::default().compare(&[MARKET_DATA, HISTORICAL]));
}

fn dump(name: &str, o: &Outcome) {
    eprintln!("== {name}: frames compared {}, error {:?}", o.frames_compared, o.frame_error);
    eprintln!("   unsent {:?}", o.unsent);
    eprintln!("   not made {:?}, conId given {:?}", o.not_made, o.con_id_given);
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
