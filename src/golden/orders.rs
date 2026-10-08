//! Orders: the recorded placeOrder and cancelOrder requests made again
//! (encode: ibx's new order, replace and cancel messages against the
//! reference's, normalised), and the server's reports sent back with ibx's
//! order ids (decode: ibx's order callbacks against the reference's).

use std::collections::HashMap;

use super::load;
use super::replay::request as md_request;
use crate::test_support::scenario::record::{canonical, rebuild_text};
use crate::test_support::scenario::runner::{at_their_effect, base, comparable};
use crate::test_support::scenario::{Rec, Scenario as Fixture, Session};
use crate::test_support::{to_pipe, Fields};

fn tag(f: &Fields, t: u32) -> Option<&str> {
    f.iter().find(|(k, _)| *k == t).map(|(_, v)| v.as_str())
}

fn msg_type(f: &Fields) -> &str {
    tag(f, 35).unwrap_or("")
}

fn is_order_msg(f: &Fields) -> bool {
    matches!(msg_type(f), "D" | "G" | "F")
}

/// The key of each order message of a list: the API order id (6121 of the
/// new order; a replace or cancel takes the id of the order its 41 names),
/// the message type, and its rank among the messages of that order and type.
fn keys(frames: &[Fields]) -> Vec<(i64, String, usize)> {
    let mut by_base: HashMap<String, i64> = HashMap::new();
    let mut count: HashMap<(i64, String), usize> = HashMap::new();
    frames.iter().map(|f| {
        let t = msg_type(f).to_string();
        let id = match t.as_str() {
            "D" => {
                let id = tag(f, 6121).and_then(|v| v.parse().ok()).unwrap_or(-1);
                if let Some(c) = tag(f, 11) { by_base.insert(base(c).to_string(), id); }
                id
            }
            _ => tag(f, 41).or(tag(f, 11)).and_then(|c| by_base.get(base(c)).copied()).unwrap_or(-1),
        };
        let n = count.entry((id, t.clone())).or_default();
        *n += 1;
        (id, t, *n)
    }).collect()
}

/// The key of an order message: API order id, message type, rank.
pub(crate) type Key = (i64, String, usize);

/// An order message of both sides by its key: ibx's, and the reference's
/// with its seq.
pub(crate) type Pair = (Key, Option<Fields>, Option<(u64, Fields)>);

/// The result of an order replay.
pub(crate) struct OrderReplay {
    /// The order messages: key, ibx's and the reference's (with its seq),
    /// as [`comparable`] gives them.
    pub pairs: Vec<Pair>,
    /// The order callbacks, one line each.
    pub cb_ours: Vec<String>,
    pub cb_theirs: Vec<String>,
    /// Recorded server reports not sent (no order of ibx matches them).
    pub unsent: Vec<u64>,
}

/// Replay the orders of a fixture. Each recorded request is made again;
/// a definition lookup of ibx is answered with the recorded reply for the
/// same contract; the server's reports are sent with ibx's order ids.
pub(crate) fn replay_orders(fx: &Fixture) -> OrderReplay {
    let recs = at_their_effect(&fx.recs);
    let fx = &Fixture { header: fx.header.clone(), recs };
    let mut s = Session::new().in_zone(&fx.header);
    if let Some(start) = fx.recs.iter().find(|r| r.msg == "START_API") {
        // The client id of the recorded session, on the wire (6119) and in
        // the callbacks, as a client connected with it.
        let id = start.request["clientId"].as_i64().unwrap_or(0);
        s.shared.reference.set_api_client_id(id);
        s.client.core.client_id.store(id, std::sync::atomic::Ordering::Relaxed);
    }
    let mut unsent = Vec::new();
    let mut cb_theirs = Vec::new();
    // The definition replies, to answer ibx's own lookups.
    let replies: Vec<&Rec> = fx.recs.iter().filter(|r| r.is("fix_in", "CCP", "d")).collect();
    let mut seen_ccp = 0usize;
    let mut theirs: Vec<(u64, Fields)> = Vec::new();
    let mut ours: Vec<Fields> = Vec::new();
    // The reference's order id bases and ibx's, by the same key, and the
    // whole ClOrdIDs of the new orders (a preview's version differs).
    let mut ids: HashMap<String, String> = HashMap::new();
    let mut clords: HashMap<String, String> = HashMap::new();
    for r in &fx.recs {
        match (r.leg.as_str(), r.msg.as_str()) {
            ("api_out", "PLACE_ORDER") => md_request(&mut s, r),
            ("api_out", "CANCEL_ORDER") => {
                let id = r.request["orderId"].as_i64().unwrap();
                let _ = s.call(move |c| c.cancel_order(id, ""));
            }
            ("api_out", _) => md_request(&mut s, r),
            ("api_in", _) => cb_theirs.extend(r.callbacks.as_array().into_iter().flatten().filter_map(canonical)),
            ("fix_out", "D" | "G" | "F") if r.conn == "CCP" => theirs.push((r.seq, r.fields())),
            ("fix_in", "8" | "9" | "U") if r.conn == "CCP" => {
                let mut f = r.fields();
                let mut known = true;
                for (t, v) in f.iter_mut() {
                    if matches!(t, 11 | 41) && let Some(ours) = clords.get(v.as_str()) {
                        *v = ours.clone();
                        continue;
                    }
                    if matches!(t, 11 | 41 | 6107 | 583) && !v.is_empty() {
                        match ids.get(base(v)) {
                            Some(ours) => *v = v.replacen(base(v), ours, 1),
                            None if matches!(t, 11 | 41) => known = false,
                            None => {}
                        }
                    }
                }
                if known { s.send_ccp(&rebuild_text(&f)); } else { unsent.push(r.seq); }
            }
            _ => {}
        }
        // ibx's new messages: its order messages kept, its definition
        // lookups answered.
        while seen_ccp < s.ccp_out.len() {
            let f = s.ccp_out[seen_ccp].clone();
            seen_ccp += 1;
            if is_order_msg(&f) {
                ours.push(f);
            } else if msg_type(&f) == "c" {
                let con_id = tag(&f, 6008).map(str::to_string);
                let symbol = tag(&f, 55).map(str::to_string);
                let reply = replies.iter().find(|d| {
                    let df = d.fields();
                    match (&con_id, &symbol) {
                        (Some(c), _) => tag(&df, 6008) == Some(c.as_str()),
                        (None, Some(sym)) => tag(&df, 55) == Some(sym.as_str()),
                        _ => false,
                    }
                });
                if let (Some(d), Some(rid)) = (reply, tag(&f, 320)) {
                    let mut df = d.fields();
                    for (t, v) in df.iter_mut() { if *t == 320 { *v = rid.to_string(); } }
                    s.send_ccp(&rebuild_text(&df));
                }
            }
        }
        // The order ids of the new orders both sides sent.
        let their_frames: Vec<Fields> = theirs.iter().map(|(_, f)| f.clone()).collect();
        let (kt, ko) = (keys(&their_frames), keys(&ours));
        for (i, key) in kt.iter().enumerate().filter(|(_, k)| k.1 == "D") {
            if let Some(j) = ko.iter().position(|o| o == key)
                && let (Some(a), Some(b)) = (tag(&their_frames[i], 11), tag(&ours[j], 11))
            {
                ids.entry(base(a).to_string()).or_insert_with(|| base(b).to_string());
                clords.entry(a.to_string()).or_insert_with(|| b.to_string());
            }
        }
    }
    s.settle();
    let their_frames: Vec<Fields> = theirs.iter().map(|(_, f)| f.clone()).collect();
    let (kt, ko) = (keys(&their_frames), keys(&ours));
    let mut pairs: Vec<Pair> = Vec::new();
    for (i, key) in kt.iter().enumerate() {
        let mine = ko.iter().position(|o| o == key).map(|j| comparable(&ours[j]));
        pairs.push((key.clone(), mine, Some((theirs[i].0, comparable(&theirs[i].1)))));
    }
    for (j, key) in ko.iter().enumerate() {
        if !kt.contains(key) {
            pairs.push((key.clone(), Some(comparable(&ours[j])), None));
        }
    }
    OrderReplay { pairs, cb_ours: s.callbacks.clone(), cb_theirs, unsent }
}

/// The differences between ibx's order messages and the reference's, one
/// entry per message that differs.
pub(crate) fn frame_differences(r: &OrderReplay) -> Vec<String> {
    r.pairs.iter().filter_map(|((id, t, n), mine, theirs)| match (mine, theirs) {
        (Some(a), Some((seq, b))) if a != b =>
            Some(format!("order {id} 35={t} #{n} (seq {seq})\n   ours {}\n   want {}", to_pipe(a), to_pipe(b))),
        (Some(a), None) => Some(format!("order {id} 35={t} #{n} extra: {}", to_pipe(a))),
        (None, Some((seq, b))) => Some(format!("order {id} 35={t} #{n} (seq {seq}) not sent: {}", to_pipe(b))),
        _ => None,
    }).collect()
}

/// Known differences the order tests leave out of a comparison, each
/// with its own ignored test.
#[derive(Clone, Copy, Default)]
struct Known {
    /// openOrder lmtPrice of a STP order: the reference shows a price
    /// (250.03 for a SELL stop at 250) on the first report, none later;
    /// where it comes from is not read yet.
    stp_lmt: bool,
}

/// The order callbacks of a replay (no session notices), with the known
/// differences masked on both sides.
fn order_lines(lines: &[String], known: Known) -> Vec<String> {
    lines.iter().filter(|l| !l.starts_with("error|-1|")).map(|l| {
        let mut f: Vec<String> = l.split('|').map(str::to_string).collect();
        if known.stp_lmt && f[0] == "openOrder" && f[5].contains("orderType=STP,") {
            f[5] = f[5].split(',').map(|kv| if kv.starts_with("lmtPrice=") { "lmtPrice=-" } else { kv })
                .collect::<Vec<_>>().join(",");
        }
        f.join("|")
    }).collect()
}

/// Replay the records of `name` that `keep` keeps (a request whose answer
/// the recording does not hold is left out); every order message the
/// same, and the order callbacks the same with the `known` differences
/// masked. The orders `skip_orders` are left out of both.
fn replay_and_compare(name: &str, keep_rec: impl Fn(&Rec) -> bool, known: Known, skip_orders: &[i64]) -> OrderReplay {
    let mut fx = load(name);
    fx.recs.retain(|r| keep_rec(r));
    let r = replay_orders(&fx);
    let diffs: Vec<String> = frame_differences(&r).into_iter()
        .filter(|d| !skip_orders.iter().any(|id| d.starts_with(&format!("order {id} "))))
        .collect();
    assert!(diffs.is_empty(), "{name}: order messages differ:\n{}", diffs.join("\n"));
    // The callbacks of the orders of the recording (not those of its
    // market data requests), the skipped orders left out.
    let orders: Vec<String> = fx.recs.iter().filter(|r| matches!(r.msg.as_str(), "PLACE_ORDER" | "CANCEL_ORDER"))
        .filter_map(|r| r.request["orderId"].as_i64()).filter(|id| !skip_orders.contains(id))
        .map(|id| id.to_string()).collect();
    let keep = |l: &String| orders.iter().any(|id| l.split('|').nth(1) == Some(id.as_str()));
    let ours: Vec<String> = order_lines(&r.cb_ours, known).into_iter().filter(keep).collect();
    let theirs: Vec<String> = order_lines(&r.cb_theirs, known).into_iter().filter(keep).collect();
    crate::test_support::scenario::assert_same_callbacks(&ours, &theirs);
    r
}

#[test]
fn dump_all_order_fixtures() {
    if std::env::var_os("IBX_GOLDEN_DUMP").is_none() { return; }
    for name in [
        "orders_lmt_cancel", "orders_modify_cancelled", "orders_bracket", "orders_bracket_b", "orders_oca_group",
        "orders_premarket_order_types", "orders_rth_order_types", "orders_i196_overnight",
        "orders_b1_416_time_condition", "orders_b1_416_time_near", "orders_b1_462_whatif", "orders_b1_263_algo_refusals",
    ] {
        let r = replay_orders(&load(name));
        eprintln!("== {name}: {} messages, unsent {:?}", r.pairs.len(), r.unsent);
        for d in frame_differences(&r) { eprintln!("{d}"); }
        if std::env::var_os("IBX_GOLDEN_CALLBACKS").is_some() {
            let keep = |l: &&String| !l.starts_with("error|-1|");
            let (a, b): (Vec<&String>, Vec<&String>) = (r.cb_ours.iter().filter(keep).collect(), r.cb_theirs.iter().filter(keep).collect());
            for k in 0..a.len().max(b.len()) {
                let (x, y) = (a.get(k).map_or("", |s| s.as_str()), b.get(k).map_or("", |s| s.as_str()));
                eprintln!("{k:3} {} {x}\n      {y}", if x == y { ' ' } else { '*' });
            }
        }
    }
}

const KNOWN: Known = Known { stp_lmt: true };

fn all(_: &Rec) -> bool { true }

// A LMT order before the open, then its cancel (26/09/2026, lmt_cancel):
// the order message; the order message warning 399 (three lines),
// openOrder and orderStatus for each report; the reference acted on the
// cancel after the next report (the replay does the same), then the
// Cancelled status and 202.
#[test]
fn lmt_order_before_the_open() {
    replay_and_compare("orders_lmt_cancel", all, KNOWN, &[]);
}

// A LMT order, its cancel and a modify after the cancel (26/09/2026,
// modify_cancelled): no callback at the cancel itself; the working
// reports after the cancel request give PendingCancel; the Cancelled
// status keeps 1 remaining (151=0 on the wire), then 202. The answer to
// the modify is not in the recording.
#[test]
fn cancel_gives_pending_cancel_then_cancelled_then_202() {
    let r = replay_and_compare("orders_modify_cancelled", |r| r.seq < 2441, KNOWN, &[]);
    assert!(r.cb_theirs.iter().any(|l| l.starts_with("orderStatus|2|PendingCancel")));
}

// Two orders in one OCA group (26/09/2026, oca_group): the 583 group and
// 6209 type; the cancel of the second after the first's cancel
// cancelled it: 10148 "cannot be cancelled, state: Cancelled".
#[test]
fn oca_group_orders_and_the_refused_second_cancel() {
    replay_and_compare("orders_oca_group", all, KNOWN, &[]);
}

// Stop, trailing, IOC, FOK, time in force values, a customer account
// refusal, OPG, a fill and a modify before the open (28/09/2026,
// premarket_order_types): 2109 when outside RTH is dropped (and
// openOrder shows it off); a server reject gives Inactive, 201, Inactive
// again; an IOC that a report routes outside RTH shows outsideRth true.
#[test]
fn premarket_order_types() {
    replay_and_compare("orders_premarket_order_types", all, KNOWN, &[]);
}

// Four time conditions at a near date (02/10/2026, b1_416_time_near): the
// condition times in UTC on the wire; each order cancelled: Cancelled,
// then 202.
#[test]
fn time_conditions_near_dates() {
    replay_and_compare("orders_b1_416_time_near", all, KNOWN, &[]);
}

// Time conditions (02/10/2026, b1_416_time_condition): the server refuses
// the far date with 201 "Invalid value in field # 6223": Inactive, 201,
// Inactive again; 2174 for a time without a zone. Up to the Asia/Tokyo
// one (see the ignored test below).
#[test]
fn time_conditions_refused_by_the_server() {
    replay_and_compare("orders_b1_416_time_condition", |r| r.seq < 1530, KNOWN, &[]);
}

// OVERNIGHT time in force (28/09/2026, i196_overnight): the order and the
// refused change of its price (462 for the time in force the modify
// restates), Cancelled then 202. Up to the order on the OVERNIGHT
// exchange (see `overnight_directed_order`).
#[test]
fn overnight_time_in_force_on_smart() {
    replay_and_compare("orders_i196_overnight", |r| r.seq < 1237, KNOWN, &[]);
}

// The session's order types (28/09/2026, rth_order_types): TRAIL MIT by
// amount (filled at once) and by percent with its replace, TRAIL LIT,
// PASSV REL and RPI refused with 387 (the SPY list has no key for them),
// after the lookup of the contract's order types,
// PEG BEST sent and rejected by the server (ibx#469); PEG BENCH, its
// replace and cancel (openOrder auxPrice = the starting price);
// OVERNIGHT, OVERNIGHT + DAY and includeOvernight in the session, the
// directed OVERNIGHT order discarded (10). Left out here, in an ignored
// test below: the option combos refused with 460 (13, 14).
#[test]
fn session_order_types() {
    replay_and_compare("orders_rth_order_types", all, KNOWN, &[13, 14]);
}

// What-if previews (02/10/2026, b1_462_whatif): LMT, MKT and a margin
// refusal (openOrder then 201); transmit off refused with 321; a real
// order after them. The combo preview is in the ignored test below.
#[test]
fn what_if_previews() {
    replay_and_compare("orders_b1_462_whatif", all, KNOWN, &[78]);
}

// An algo order and the algo refusals (02/10/2026, b1_263_algo_refusals):
// the STP order, its cancel; prices off the grid refused with 110.
#[test]
fn algo_scenario_orders() {
    replay_and_compare("orders_b1_263_algo_refusals", all, KNOWN, &[80, 82, 83, 84]);
}

// ── Differences found by the replays, each its own test ──

// The reference's 399 names the listing of the contract's definition
// (NASDAQ.NMS), which it looks up before each order placed without a
// conId; ibx too (ibx#486). Every order replay compares the 399 text.
#[test]
fn order_message_names_the_listing_exchange() {
    let r = replay_and_compare("orders_modify_cancelled", |r| r.seq < 2441, KNOWN, &[]);
    assert!(r.cb_ours.iter().any(|l| l.contains("|399|") && l.contains("BUY 1 AAPL NASDAQ.NMS")));
}

// orderStatus whyHeld "trigger" for a stop or trailing order before its
// trigger (jclient.pe.iK(): "child", "locate", "trigger"). Every replay
// compares whyHeld; this one has STP and TRAIL orders.
#[test]
fn why_held_is_trigger_for_a_stop() {
    replay_and_compare("orders_premarket_order_types", all, KNOWN, &[]);
}

// The first openOrder of a STP order shows lmtPrice 250.03 (SELL stop at
// 250, no 44 on the wire), the next ones none. Not reproduced on
// 07/10/2026: four STP orders showed no limit price (ibx#510).
#[test]
#[ignore = "not reproduced (ibx#510): the first openOrder of a STP order had a limit price the wire does not carry"]
fn stp_first_open_order_limit_price() {
    replay_and_compare("orders_premarket_order_types", all, Known { stp_lmt: false, ..KNOWN }, &[]);
}

// A bracket in the client library's form: parent and take-profit with
// transmit off, the stop with transmit on; the reference holds the first
// two and sends the three new orders together. ibx refuses transmit off
// (ibx#226, ibx#509).
#[test]
#[ignore = "ibx#509: transmit=false orders are refused instead of held for the group"]
fn bracket_with_transmit_off() {
    replay_and_compare("orders_bracket", all, KNOWN, &[]);
    replay_and_compare("orders_bracket_b", all, KNOWN, &[]);
}

// The reference gives a preview a ClOrdID of its own with version 0, and
// the preview's openOrder the permId of that order (ibx#486).
#[test]
fn what_if_clord_id_and_perm_id() {
    let r = replay_and_compare("orders_b1_462_whatif", all, KNOWN, &[78]);
    let previews: Vec<&str> = r.pairs.iter().filter_map(|(_, mine, _)| mine.as_ref())
        .filter(|f| tag(f, 6091) == Some("1")).filter_map(|f| tag(f, 11)).collect();
    assert_eq!(previews, ["{id}.0"; 3]);
    assert!(r.cb_ours.iter().filter(|l| l.contains("whatIf=true")).all(|l| l.contains("permId={perm}")));
}

// A combo preview (QQQ,SPY BAG, conId 0 in the request).
#[test]
#[ignore = "replay limit: the combo what-if of b1_462 is not sent by the replay"]
fn what_if_of_a_combo() {
    replay_and_compare("orders_b1_462_whatif", all, KNOWN, &[]);
}

// A condition time in Asia/Tokyo on a US stock: the reference refuses it
// with 10314 after the contract lookup (not the contract's zone). It read
// the zone from a trading schedule it had before the recording (no
// schedule request in it: `trader.order.ay.a(dy,OrderCreator,D)`, the
// schedule from `jextend.dX.a(dy, Consumer)`, asked only when not
// cached); ibx asks no schedule for an order, knows no zone, and sends the
// order.
#[test]
#[ignore = "ibx#510: the contract's zone comes from a schedule the reference had before the recording"]
fn condition_time_in_another_zone() {
    replay_and_compare("orders_b1_416_time_condition", all, KNOWN, &[]);
}

// An order routed to the OVERNIGHT exchange: the reference warns 10329
// "This order will be directly routed to OVERNIGHT." and discards it
// (Cancelled, then 201 "Order was discarded."); the same id placed again
// gets 103, its cancel 10147. It is the API precaution "Bypass Redirect
// Order warning for Stock API Orders" (off by default,
// `trader.order.confirm.OrderChecker$6.check(pe)`; ibx:
// `IBX_BYPASS_REDIRECT_ORDER_WARNING`). The Cancelled status has a permId
// here and none in the ISLAND capture of 25/09/2026: ibx gives the id the
// order would have gone out under.
#[test]
fn overnight_directed_order() {
    replay_and_compare("orders_i196_overnight", all, KNOWN, &[]);
    replay_and_compare("orders_rth_order_types", all, KNOWN, &[1, 2, 3, 4, 5, 6, 7, 13, 14]);
}

// SPY option combos (a call spread, SMART and CBOE): the reference
// refuses both with 460 "No trading permissions" before sending; ibx
// gives no callback. The raise sites (`trader.order.proc.aN.a(pe,...)
// @13670, @13743, @13933`) read positions, clearing and logon flags; no
// tag of the logon or of the contract replies of the recording names the
// option permission.
#[test]
#[ignore = "ibx#510: the data behind 460 is not in the logon or the contract replies"]
fn option_combo_without_permission() {
    replay_and_compare("orders_rth_order_types", all, KNOWN, &[1, 2, 3, 4, 5, 6, 7]);
}

// The algo refusals 441 / 443 need the algo definitions the reference
// read from the server; this replay does not load them.
#[test]
#[ignore = "replay limit: algo definitions are not part of the order replay"]
fn algo_refusals() {
    replay_and_compare("orders_b1_263_algo_refusals", all, KNOWN, &[]);
}
