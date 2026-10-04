//! Tag order of the new order (35=D), replace (35=G) and cancel (35=F)
//! messages against the gateway's writers (ibx#485 items 3 and 4).
//!
//! The writer table (`order_writers.csv`) lists, in bytecode order, every
//! tag the gateway writes, with its condition; the sub-writers are
//! flattened, a tag listed twice is two branches, and a block (the order
//! attributes, the algo parameters, the conditions, the combo legs) is one
//! place whose tags come in any order. For each `OrderRequest` variant the
//! engine's frame, read from the in-memory server, must:
//! - hold its tags in an order the writer can produce (a subsequence of the
//!   places, a block taking any number of its tags);
//! - hold no tag the writer never writes, and no tag whose condition is
//!   false for the order;
//! - hold every tag whose condition is true for the order;
//! - write each order attribute with the value kind of the attribute table.
//!
//! The conditions are read on the API order of each case ("facts" below,
//! from the gateway rules of `ORDER-SUBMIT` 2-3), never on ibx's frame.

use std::collections::HashMap;

use ibx::test_support::{parse_pipe, to_pipe, Fields};
use ibx::types::{
    AdaptivePriority, AdjustedOrderType, AlgoParams, OrderAttrs, OrderCondition, OrderKind, OrderRequest,
    PRICE_SCALE, QTY_SCALE, Side,
};

use super::catalog::{self, WriterRow};
use super::harness::{aapl, Engine};

const P: i64 = PRICE_SCALE;

/// Fields of the frame header and trailer, not written by the order
/// writers.
const FRAMING: &[u32] = &[8, 9, 35, 34, 49, 56, 52, 10, 8349, 43, 122, 97];

/// One expected order message: its type, the API order type and the facts
/// of the API order that decide the writer conditions.
#[derive(Clone)]
struct Expect {
    msg: &'static str,
    order_type: &'static str,
    facts: Vec<&'static str>,
}

/// A new order. Every new stock order of these cases has a currency and
/// the stock multiplier, and the origin attribute (`6122=c` on every
/// captured new order and replace of an API order).
fn d(order_type: &'static str, facts: &[&'static str]) -> Expect {
    let mut all = vec!["currency", "multiplier", "attr:6122"];
    all.extend_from_slice(facts);
    Expect { msg: "D", order_type, facts: all }
}

fn g(order_type: &'static str, facts: &[&'static str]) -> Expect {
    let mut all = vec!["attr:6122"];
    all.extend_from_slice(facts);
    Expect { msg: "G", order_type, facts: all }
}

fn f() -> Expect {
    Expect { msg: "F", order_type: "", facts: Vec::new() }
}

struct Case {
    name: &'static str,
    requests: Vec<OrderRequest>,
    expect: Vec<Expect>,
    /// A known difference from the gateway, with its open issue.
    open: Option<&'static str>,
}

/// Evaluate one `when` term on the facts: `None` when the writer table
/// cannot decide it (an attribute's own rule, a condition not read).
fn term(term: &str, e: &Expect) -> Option<bool> {
    let types = |list: &str| list.split(',').any(|t| t == e.order_type);
    match term.split_once(':') {
        _ if term == "always" => Some(true),
        _ if term == "never" => Some(false),
        Some(("flag", fact)) => Some(e.facts.contains(&fact)),
        Some(("type_in", list)) => Some(types(list)),
        Some(("type_not_in", list)) => Some(!types(list)),
        _ => None,
    }
}

fn when(row: &WriterRow, e: &Expect) -> Option<bool> {
    let mut out = Some(true);
    for t in row.when.split(" & ") {
        match term(t, e) {
            Some(false) => return Some(false),
            Some(true) => {}
            None => out = None,
        }
    }
    out
}

/// The differences between a frame and the writer table, empty when it
/// matches.
fn check(frame: &Fields, e: &Expect, rows: &[WriterRow], attrs: &HashMap<u32, Vec<catalog::Attribute>>) -> Vec<String> {
    let mut problems = Vec::new();
    let body: Vec<&(u32, String)> = frame.iter().filter(|(t, _)| !FRAMING.contains(t)).collect();
    // Order: the earliest place at or after the current one (the same
    // place again only inside a block).
    let mut cur = 0;
    for (tag, value) in &body {
        let next = rows.iter()
            .filter(|r| r.tag == *tag && (r.pos > cur || (r.pos == cur && !r.block.is_empty())))
            .map(|r| r.pos)
            .min();
        match next {
            Some(p) => cur = p,
            None if rows.iter().any(|r| r.tag == *tag) => {
                problems.push(format!("{tag}={value} is out of the writer's order"));
            }
            None => problems.push(format!("{tag}={value}: the gateway never writes it in a 35={}", e.msg)),
        }
    }
    // Presence: a tag is expected when one of its places is true for the
    // order, refused when all are false. A block that applies needs one of
    // its tags, not each.
    let mut tags: Vec<u32> = rows.iter().map(|r| r.tag).collect();
    tags.sort();
    tags.dedup();
    for tag in tags {
        let decisions: Vec<Option<bool>> = rows.iter().filter(|r| r.tag == tag)
            .map(|r| match when(r, e) {
                Some(true) if !r.block.is_empty() => None,
                d => d,
            })
            .collect();
        let present = body.iter().any(|(t, _)| *t == tag);
        if decisions.contains(&Some(true)) && !present {
            problems.push(format!("{tag} missing"));
        }
        if decisions.iter().all(|d| *d == Some(false)) && present {
            problems.push(format!("{tag} written although its condition is false for this order"));
        }
    }
    // The attributes the API order sets that the gateway was captured
    // writing for it ("attr:<tag>" facts).
    for fact in &e.facts {
        let Some(tag) = fact.strip_prefix("attr:") else { continue };
        let tag: u32 = tag.parse().unwrap();
        if !body.iter().any(|(t, _)| *t == tag) {
            problems.push(format!("attribute {tag} missing"));
        }
    }
    let mut blocks: Vec<(u32, &str)> = rows.iter().filter(|r| !r.block.is_empty() && when(r, e) == Some(true))
        .map(|r| (r.pos, r.block.as_str())).collect();
    blocks.dedup();
    for (pos, block) in blocks {
        if !body.iter().any(|(t, _)| rows.iter().any(|r| r.pos == pos && r.tag == *t)) {
            problems.push(format!("the {block} block is missing"));
        }
    }
    // Attributes: the value kind of the attribute table.
    for (tag, value) in &body {
        let Some(list) = attrs.get(tag) else { continue };
        let kinds: Vec<&str> = list.iter().filter(|a| a.kind != "holder").map(|a| a.kind.as_str()).collect();
        if kinds.is_empty() {
            continue;
        }
        if !kinds.iter().any(|k| value_fits(k, value)) {
            let names: Vec<&str> = list.iter().map(|a| a.name.as_str()).collect();
            problems.push(format!("{tag}={value}: not a {kinds:?} value ({names:?})"));
        }
    }
    problems
}

/// Whether a value is one the gateway writes for an attribute of this kind
/// (`jattrib` writers: a boolean only as `1`, an integer, a double with 2
/// to 8 decimals, a date and time `yyyyMMdd-HH:mm:ss` or with a space).
fn value_fits(kind: &str, value: &str) -> bool {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let signed = |s: &str| digits(s.strip_prefix('-').unwrap_or(s));
    match kind {
        "boolean" => value == "1",
        "integer" => signed(value),
        "double" => value.split_once('.').is_some_and(|(i, f)| signed(i) && digits(f) && (2..=8).contains(&f.len())),
        "decimal" => value.split_once('.').map_or(signed(value), |(i, f)| signed(i) && digits(f)),
        "datetime" => value.len() >= 17 && digits(&value[..8]) && matches!(&value[8..9], "-" | " "),
        "character" => value.chars().count() == 1,
        "string" | "enum" => !value.is_empty(),
        _ => true,
    }
}

fn attrs() -> OrderAttrs {
    OrderAttrs::default()
}

fn cases() -> Vec<Case> {
    use OrderRequest::*;
    let (i, s, q) = (0, Side::Buy, 1);
    let case = |name, requests, expect| Case { name, requests, expect, open: None };
    let vwap = AlgoParams::Vwap {
        max_pct_vol: 0.1, no_take_liq: false, allow_past_end_time: true,
        start_time: String::new(), end_time: String::new(),
    };
    let mut out = vec![
        case("SubmitLimit", vec![SubmitLimit { order_id: 101, instrument: i, side: s, qty: q, price: 100 * P }],
            vec![d("LMT", &["lmt_price"])]),
        case("SubmitMarket", vec![SubmitMarket { order_id: 102, instrument: i, side: s, qty: q }],
            vec![d("MKT", &[])]),
        case("SubmitStop", vec![SubmitStop { order_id: 103, instrument: i, side: Side::Sell, qty: q, stop_price: 90 * P }],
            vec![d("STP", &["aux_price", "stop_price", "attr:6115"])]),
        case("SubmitStopLimit", vec![SubmitStopLimit {
            order_id: 104, instrument: i, side: Side::Sell, qty: q, price: 89 * P, stop_price: 90 * P }],
            vec![d("STP LMT", &["lmt_price", "aux_price", "stop_price", "attr:6115"])]),
        case("SubmitLimitGtc", vec![SubmitLimitGtc {
            order_id: 105, instrument: i, side: s, qty: q, price: 100 * P, outside_rth: false }],
            vec![d("LMT", &["lmt_price"])]),
        case("SubmitStopGtc", vec![SubmitStopGtc {
            order_id: 106, instrument: i, side: Side::Sell, qty: q, stop_price: 90 * P, outside_rth: false }],
            vec![d("STP", &["aux_price", "stop_price", "attr:6115"])]),
        case("SubmitStopLimitGtc", vec![SubmitStopLimitGtc {
            order_id: 107, instrument: i, side: Side::Sell, qty: q, price: 89 * P, stop_price: 90 * P, outside_rth: false }],
            vec![d("STP LMT", &["lmt_price", "aux_price", "stop_price", "attr:6115"])]),
        case("SubmitLimitIoc", vec![SubmitLimitIoc { order_id: 108, instrument: i, side: s, qty: q, price: 100 * P }],
            vec![d("LMT", &["lmt_price"])]),
        case("SubmitLimitFok", vec![SubmitLimitFok { order_id: 109, instrument: i, side: s, qty: q, price: 100 * P }],
            vec![d("LMT", &["lmt_price"])]),
        case("SubmitTrailingStop", vec![SubmitTrailingStop {
            order_id: 110, instrument: i, side: Side::Sell, qty: q, trail_amt: P, trail_stop_price: 0 }],
            vec![d("TRAIL", &["aux_price", "exec_inst", "peg_difference", "attr:6115", "attr:6268"])]),
        case("SubmitTrailingStopLimit", vec![SubmitTrailingStopLimit {
            order_id: 111, instrument: i, side: Side::Sell, qty: q, lmt_offset: P / 2, lmt_price: None,
            trail_amt: P, trail_stop_price: 99 * P }],
            vec![d("TRAIL LIMIT", &["aux_price", "stop_price", "peg_difference", "attr:6115", "attr:6268", "attr:6370"])]),
        case("SubmitTrailingStopPct", vec![SubmitTrailingStopPct {
            order_id: 112, instrument: i, side: Side::Sell, qty: q, trail_percent: P, trail_stop_price: 0 }],
            vec![d("TRAIL", &["aux_price", "exec_inst", "peg_difference", "attr:6115", "attr:6268"])]),
        case("SubmitTrailingStopPctEx", vec![SubmitTrailingStopPctEx {
            order_id: 113, instrument: i, side: Side::Sell, qty: q, trail_percent: 2 * P, tif: b'1',
            attrs: attrs(), trail_stop_price: 0 }],
            vec![d("TRAIL", &["aux_price", "exec_inst", "peg_difference", "attr:6115", "attr:6268"])]),
        case("SubmitMoc", vec![SubmitMoc { order_id: 114, instrument: i, side: s, qty: q }], vec![d("MOC", &[])]),
        case("SubmitLoc", vec![SubmitLoc { order_id: 115, instrument: i, side: s, qty: q, price: 100 * P }],
            vec![d("LOC", &["lmt_price"])]),
        case("SubmitMit", vec![SubmitMit { order_id: 116, instrument: i, side: s, qty: q, stop_price: 95 * P }],
            vec![d("MIT", &["aux_price", "attr:6115", "attr:6117"])]),
        case("SubmitLit", vec![SubmitLit {
            order_id: 117, instrument: i, side: s, qty: q, price: 96 * P, stop_price: 95 * P }],
            vec![d("LIT", &["lmt_price", "aux_price", "attr:6115", "attr:6117"])]),
        case("SubmitBracket", vec![SubmitBracket {
            parent_id: 118, tp_id: 119, sl_id: 120, instrument: i, side: s, qty: q,
            entry_price: 100 * P, take_profit: 110 * P, stop_loss: 90 * P }],
            vec![d("LMT", &["lmt_price", "attr:6531"]), d("LMT", &["lmt_price", "oca", "parent", "attr:583", "attr:6531"]),
                 d("STP", &["aux_price", "stop_price", "oca", "parent", "attr:6115", "attr:583", "attr:6531"])]),
        case("SubmitLimitEx", vec![SubmitLimitEx {
            order_id: 121, instrument: i, side: s, qty: q, price: 100 * P, tif: b'0',
            attrs: OrderAttrs { order_ref: "layer-a".into(), display_size: 10, hidden: false, ..attrs() } }],
            vec![d("LMT", &["lmt_price", "attr:6010", "attr:111"])]),
        case("SubmitEx", vec![SubmitEx {
            order_id: 122, instrument: i, side: s, qty: q, kind: OrderKind::Market, tif: b'1', attrs: attrs() }],
            vec![d("MKT", &[])]),
        case("SubmitRel", vec![SubmitRel { order_id: 123, instrument: i, side: s, qty: q, offset: P / 100 }],
            vec![d("REL", &["exec_inst", "peg_difference"])]),
        case("SubmitLimitOpg", vec![SubmitLimitOpg { order_id: 124, instrument: i, side: s, qty: q, price: 100 * P }],
            vec![d("LMT", &["lmt_price"])]),
        case("SubmitAdaptive", vec![SubmitAdaptive {
            order_id: 125, instrument: i, side: s, qty: q, price: 100 * P, priority: AdaptivePriority::Normal,
            tif: b'0', attrs: attrs() }],
            vec![d("LMT", &["lmt_price", "algo", "exec_inst"])]),
        case("SubmitMtl", vec![SubmitMtl { order_id: 126, instrument: i, side: s, qty: q }], vec![d("MTL", &[])]),
        case("SubmitMktPrt", vec![SubmitMktPrt { order_id: 127, instrument: i, side: s, qty: q }],
            vec![d("MKT PRT", &[])]),
        case("SubmitStpPrt", vec![SubmitStpPrt { order_id: 128, instrument: i, side: Side::Sell, qty: q, stop_price: 90 * P }],
            vec![d("STP PRT", &["aux_price", "stop_price"])]),
        case("SubmitMidPrice", vec![SubmitMidPrice { order_id: 129, instrument: i, side: s, qty: q, price_cap: 0 }],
            vec![d("MIDPRICE", &[])]),
        case("SubmitSnapMkt", vec![SubmitSnapMkt { order_id: 130, instrument: i, side: s, qty: q, offset: 5 * P / 100 }],
            vec![d("SNAP MKT", &["aux_price", "peg_difference"])]),
        case("SubmitSnapMid", vec![SubmitSnapMid { order_id: 131, instrument: i, side: s, qty: q, offset: 5 * P / 100 }],
            vec![d("SNAP MID", &["aux_price", "peg_difference"])]),
        case("SubmitSnapPri", vec![SubmitSnapPri { order_id: 132, instrument: i, side: s, qty: q, offset: 5 * P / 100 }],
            vec![d("SNAP PRIM", &["aux_price", "peg_difference"])]),
        case("SubmitPegMkt", vec![SubmitPegMkt {
            order_id: 133, instrument: i, side: s, qty: q, price: 100 * P, offset: 5 * P / 100 }],
            vec![d("PEG MKT", &["lmt_price", "aux_price", "exec_inst", "peg_difference"])]),
        case("SubmitPegMid", vec![SubmitPegMid {
            order_id: 134, instrument: i, side: s, qty: q, price: 100 * P, offset: 0 }],
            vec![d("PEG MID", &["lmt_price", "exec_inst", "peg_difference"])]),
        case("SubmitAlgo", vec![SubmitAlgo {
            order_id: 135, instrument: i, side: s, qty: q, price: 100 * P, algo: vwap.clone(), tif: b'0', attrs: attrs() }],
            vec![d("LMT", &["lmt_price", "algo", "exec_inst"])]),
        case("SubmitPegBench", vec![SubmitPegBench {
            order_id: 136, instrument: i, side: s, qty: q, price: 100 * P, ref_con_id: 756733, is_peg_decrease: false,
            pegged_change_amount: P / 10, ref_change_amount: P / 10, stock_ref_price: 700 * P,
            ref_exchange: String::new() }],
            vec![d("PEG BENCH", &["aux_price", "exec_inst", "attr:6941", "attr:6938", "attr:6939", "attr:6580"])]),
        case("SubmitLimitAuc", vec![SubmitLimitAuc { order_id: 137, instrument: i, side: s, qty: q, price: 100 * P }],
            vec![d("LMT", &["lmt_price"])]),
        case("SubmitMtlAuc", vec![SubmitMtlAuc { order_id: 138, instrument: i, side: s, qty: q }], vec![d("MTL", &[])]),
        case("SubmitWhatIf", vec![SubmitWhatIf { request: Box::new(SubmitLimit {
            order_id: 139, instrument: i, side: s, qty: q, price: 100 * P }) }],
            vec![d("LMT", &["lmt_price", "what_if"])]),
        case("SubmitLimitFractional", vec![SubmitLimitFractional {
            order_id: 140, instrument: i, side: s, qty: 2 * QTY_SCALE, price: 100 * P }],
            vec![d("LMT", &["lmt_price"])]),
        case("SubmitAdjustableStop", vec![SubmitAdjustableStop {
            order_id: 141, instrument: i, side: Side::Sell, qty: q, stop_price: 90 * P, trigger_price: 95 * P,
            adjusted_order_type: AdjustedOrderType::Stop, adjusted_stop_price: 94 * P, adjusted_stop_limit_price: 0,
            adjusted_trailing_amount: 0, adjustable_trailing_unit: 0 }],
            vec![d("STP", &["aux_price", "stop_price", "adjustable", "attr:6115"])]),
        case("Cancel", vec![
            SubmitLimit { order_id: 142, instrument: i, side: s, qty: q, price: 100 * P },
            Cancel { order_id: 142 }],
            vec![d("LMT", &["lmt_price"]), f()]),
        case("CancelAll", vec![
            SubmitLimit { order_id: 143, instrument: i, side: s, qty: q, price: 100 * P },
            CancelAll { instrument: i }],
            vec![d("LMT", &["lmt_price"]), f()]),
        case("GlobalCancel", vec![
            SubmitLimit { order_id: 146, instrument: i, side: s, qty: q, price: 100 * P },
            GlobalCancel],
            vec![d("LMT", &["lmt_price"]), f()]),
        case("Modify", vec![
            SubmitLimit { order_id: 144, instrument: i, side: s, qty: q, price: 100 * P },
            Modify { new_order_id: 144, order_id: 144, qty: 2, kind: OrderKind::Limit { price: 101 * P }, tif: b'0', attrs: attrs() }],
            vec![d("LMT", &["lmt_price"]), g("LMT", &["lmt_price"])]),
        // Extended attributes and the other replaces.
        case("SubmitEx with OCA, GTD and conditions", vec![SubmitEx {
            order_id: 145, instrument: i, side: s, qty: q, kind: OrderKind::Limit { price: 100 * P }, tif: b'6',
            attrs: OrderAttrs {
                oca_group_str: "layer-a-oca".into(), good_till_date_ymd: 20991231,
                conditions: vec![OrderCondition::Margin { percent: 10, is_more: true }],
                ..attrs()
            } }],
            vec![d("LMT", &["lmt_price", "oca", "gtd_date", "conditions", "attr:583"])]),
        // The order attributes ibx writes: each with the value kind of the
        // attribute table.
        case("SubmitLimitEx with attributes", vec![SubmitLimitEx {
            order_id: 149, instrument: i, side: s, qty: 10, price: 100 * P, tif: b'0',
            attrs: OrderAttrs {
                order_ref: "layer-a".into(), display_size: 5, min_qty: 5, hidden: true, sweep_to_fill: true,
                discretionary_amt: P / 10, good_after: 4_102_444_800, include_overnight: true,
                use_price_mgmt_algo: Some(true), customer_account: "CUST1".into(), professional_customer: true,
                ..attrs()
            } }],
            vec![d("LMT", &["lmt_price", "attr:6010", "attr:111", "attr:110", "attr:6135", "attr:6102", "attr:9813",
                "attr:168", "attr:8534"])]),
        case("SubmitEx GTD with a time", vec![SubmitEx {
            order_id: 146, instrument: i, side: s, qty: q, kind: OrderKind::Limit { price: 100 * P }, tif: b'6',
            attrs: OrderAttrs { good_till: 4_102_444_800, ..attrs() } }],
            vec![d("LMT", &["lmt_price", "gtd_time"])]),
        case("Modify STP", vec![
            SubmitStop { order_id: 147, instrument: i, side: Side::Sell, qty: q, stop_price: 90 * P },
            Modify { new_order_id: 147, order_id: 147, qty: 1, kind: OrderKind::Stop { stop_price: 89 * P }, tif: b'0', attrs: attrs() }],
            vec![d("STP", &["aux_price", "stop_price", "attr:6115"]), g("STP", &["aux_price", "stop_price"])]),
        case("Modify TRAIL", vec![
            SubmitTrailingStop { order_id: 148, instrument: i, side: Side::Sell, qty: q, trail_amt: P, trail_stop_price: 0 },
            Modify { new_order_id: 148, order_id: 148, qty: 1,
                kind: OrderKind::TrailingStop { trail_amt: 2 * P, trail_stop_price: 0 }, tif: b'0', attrs: attrs() }],
            vec![d("TRAIL", &["aux_price", "exec_inst", "peg_difference", "attr:6115", "attr:6268"]),
                 g("TRAIL", &["aux_price", "exec_inst", "peg_difference", "attr:6268"])]),
    ];
    for c in &mut out {
        c.open = OPEN.iter().find(|(name, _)| *name == c.name).map(|(_, why)| *why);
    }
    out
}

/// Cases with a known difference from the gateway, and the open issue.
const OPEN: &[(&str, &str)] = &[];

/// The order id of each expected message of a case's requests: the ids
/// of a bracket, the order a cancel-all or a cancel or replace is for.
fn frame_ids(requests: &[OrderRequest]) -> Vec<i64> {
    let mut out = Vec::new();
    let mut last = 0;
    for req in requests {
        match req {
            OrderRequest::SubmitBracket { parent_id, tp_id, sl_id, .. } => out.extend([*parent_id, *tp_id, *sl_id]),
            OrderRequest::CancelAll { .. } | OrderRequest::GlobalCancel => out.push(last),
            other => {
                last = other.order_id();
                out.push(last);
            }
        }
    }
    out
}

/// The order id of a message: the id part of its ClOrdID.
fn frame_id(frame: &Fields) -> Option<i64> {
    super::harness::field(frame, 11)?.split('.').next()?.parse().ok()
}

/// Run every case on one engine; the problems of each case.
fn run() -> Vec<(&'static str, Option<&'static str>, Vec<String>)> {
    let attrs = catalog::attributes();
    let writers: HashMap<&str, Vec<WriterRow>> = ["D", "G", "F"].into_iter().map(|m| (m, catalog::writer(m))).collect();
    let mut engine = Engine::start();
    let instrument = engine.register(&aapl());
    assert_eq!(instrument, 0, "the cases use instrument 0");
    let mut used = std::collections::HashSet::new();
    let mut out = Vec::new();
    for case in cases() {
        let ids = frame_ids(&case.requests);
        assert_eq!(ids.len(), case.expect.len(), "{}", case.name);
        for req in case.requests {
            engine.send(req);
        }
        // Each expected message: the first one not taken yet with its
        // type and order id (the engine may hold an order back, and a
        // cancel-all cancels the earlier orders too).
        let find = |frames: &[Fields], used: &std::collections::HashSet<usize>, id: i64, msg: &str| {
            frames.iter().enumerate().position(|(n, f)| !used.contains(&n)
                && frame_id(f) == Some(id) && super::harness::field(f, 35) == Some(msg))
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let frames = loop {
            let frames = engine.orders(0);
            let all = ids.iter().zip(&case.expect).all(|(id, e)| find(&frames, &used, *id, e.msg).is_some());
            if all || std::time::Instant::now() >= deadline {
                break frames;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        let mut problems = Vec::new();
        for (id, e) in ids.iter().zip(&case.expect) {
            let Some(n) = find(&frames, &used, *id, e.msg) else {
                problems.push(format!("no 35={} for order {id}", e.msg));
                continue;
            };
            used.insert(n);
            println!("{}: {}", case.name, to_pipe(&frames[n]));
            for p in check(&frames[n], e, &writers[e.msg], &attrs) {
                problems.push(format!("35={} {p}: {}", e.msg, to_pipe(&frames[n])));
            }
        }
        out.push((case.name, case.open, problems));
    }
    out
}

#[test]
fn every_order_request_variant_follows_the_gateway_writers() {
    let results = run();
    let mut failures = Vec::new();
    for (name, open, problems) in &results {
        match (open, problems.is_empty()) {
            (None, false) => failures.push(format!("{name}:\n  {}", problems.join("\n  "))),
            (Some(why), true) => failures.push(format!("{name}: passes now, remove its open mark ({why})")),
            (Some(why), false) => println!("{name}: known difference ({why}):\n  {}", problems.join("\n  ")),
            (None, true) => {}
        }
    }
    println!("{} cases, {} with a known difference", results.len(), results.iter().filter(|r| r.1.is_some()).count());
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The writer table itself orders every order frame the gateway was
/// captured sending (`tests/fixtures/gw1040/order_frames/frames.tsv`).
#[test]
fn the_writer_table_orders_every_captured_gateway_frame() {
    let path = format!("{}/tests/fixtures/gw1040/order_frames/frames.tsv", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(path).unwrap();
    let writers: HashMap<&str, Vec<WriterRow>> = ["D", "G", "F"].into_iter().map(|m| (m, catalog::writer(m))).collect();
    let mut n = 0;
    let mut problems = Vec::new();
    for line in text.lines().filter(|l| !l.starts_with('#')) {
        let (source, frame) = line.split_once('\t').unwrap();
        let fields = parse_pipe(frame);
        let msg = super::harness::field(&fields, 35).unwrap().to_string();
        // Order only: the conditions need the API order, which these
        // frames do not carry.
        let e = Expect { msg: if msg == "D" { "D" } else { "G" }, order_type: "?", facts: Vec::new() };
        let order: Vec<String> = check(&fields, &e, &writers[e.msg], &HashMap::new()).into_iter()
            .filter(|p| p.contains("out of the writer's order") || p.contains("never writes")).collect();
        if !order.is_empty() {
            problems.push(format!("{source}: {order:?}: {frame}"));
        }
        n += 1;
    }
    assert!(n > 200, "{n} frames");
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn attribute_values_follow_the_attribute_table_kinds() {
    // The kinds as the gateway writes them (captured frames).
    assert!(value_fits("boolean", "1") && !value_fits("boolean", "0"));
    assert!(value_fits("double", "0.10") && value_fits("double", "-0.30") && !value_fits("double", "0.1"));
    assert!(value_fits("integer", "10") && !value_fits("integer", "10.0"));
    assert!(value_fits("datetime", "20991231-23:59:59"));
    // Every attribute tag of the captured gateway frames fits its kind.
    let attrs = catalog::attributes();
    let path = format!("{}/tests/fixtures/gw1040/order_frames/frames.tsv", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(path).unwrap();
    let mut problems = Vec::new();
    for line in text.lines().filter(|l| !l.starts_with('#')) {
        let (_, frame) = line.split_once('\t').unwrap();
        for (tag, value) in parse_pipe(frame) {
            let Some(list) = attrs.get(&tag) else { continue };
            let kinds: Vec<&str> = list.iter().filter(|a| a.kind != "holder").map(|a| a.kind.as_str()).collect();
            if !kinds.is_empty() && !kinds.iter().any(|k| value_fits(k, &value)) {
                problems.push(format!("{tag}={value} {kinds:?}"));
            }
        }
    }
    problems.sort();
    problems.dedup();
    assert!(problems.is_empty(), "{problems:?}");
}

/// The checks find each kind of difference: a tag the gateway never
/// writes (21, ibx#466), a tag out of order, a missing attribute
/// (orderRef, ibx#466), a tag whose condition is false (44 on a MKT).
#[test]
fn the_checks_find_each_kind_of_difference() {
    let rows = catalog::writer("D");
    let attrs = catalog::attributes();
    let reference = "35=D|11=1.0|44=262.35|1=DUXXXXXXX|6122=c|6010=x|6121=5|6119=39|38=1|40=2|55=AAPL|167=STK|\
        231=1.00|54=1|59=0|100=BEST|6210=BEST|6008=265598|6088=Socket|15=USD|6211=|6238=";
    let lmt = d("LMT", &["lmt_price", "attr:6010"]);
    assert_eq!(check(&parse_pipe(reference), &lmt, &rows, &attrs), Vec::<String>::new());
    let problems = |frame: &str, e: &Expect| check(&parse_pipe(frame), e, &rows, &attrs);
    let with_21 = reference.replace("|59=0|", "|59=0|21=2|");
    assert_eq!(problems(&with_21, &lmt), ["21=2: the gateway never writes it in a 35=D"]);
    let swapped = reference.replace("38=1|40=2", "40=2|38=1");
    assert_eq!(problems(&swapped, &lmt), ["38=1 is out of the writer's order"]);
    let no_ref = reference.replace("|6010=x", "");
    assert_eq!(problems(&no_ref, &lmt), ["attribute 6010 missing"]);
    let mkt = reference.replace("|6010=x", "").replace("40=2", "40=1");
    assert_eq!(problems(&mkt, &d("MKT", &[])), ["44 written although its condition is false for this order"]);
    let no_qty = reference.replace("|38=1", "");
    assert_eq!(problems(&no_qty, &lmt), ["38 missing"]);
    let bad_kind = reference.replace("|6010=x", "|6010=x|6433=true");
    assert_eq!(problems(&bad_kind, &lmt).len(), 1, "{:?}", problems(&bad_kind, &lmt));
}

/// Every `OrderRequest` variant (src/types.rs) has a case above (its name
/// is the case name's first word).
#[test]
fn every_order_request_variant_has_a_case() {
    let types = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/types.rs")).unwrap()
        .replace("\r\n", "\n");
    let block = &types[types.find("pub enum OrderRequest {").unwrap()..];
    let block = &block[..block.find("\n}\n").unwrap()];
    let variants: Vec<&str> = block.lines()
        .filter_map(|l| l.strip_prefix("    "))
        .filter(|l| l.starts_with(|c: char| c.is_ascii_uppercase()))
        .map(|l| l.split(|c: char| !c.is_alphanumeric()).next().unwrap())
        .collect();
    assert!(variants.len() > 30, "{variants:?}");
    let cases = cases();
    let missing: Vec<&&str> = variants.iter()
        .filter(|v| !cases.iter().any(|c| c.name.split(' ').next() == Some(**v)))
        .collect();
    assert!(missing.is_empty(), "OrderRequest variants with no case: {missing:?}");
}
