//! Historical data (35=W queries, compressed answers): the queries against
//! the reference's, the bars and head timestamps against the reference's
//! callbacks.

use super::load;
use crate::test_support::scenario::record::{canonical, rebuild_text};
use crate::test_support::scenario::request::contract_of;
use crate::test_support::scenario::runner::hmds_query;
use crate::test_support::scenario::{assert_same_callbacks, Scenario as Fixture, Session};
use crate::test_support::{parse_fields, Fields};

const HMDS_CALLBACKS: &[&str] = &["historicalData", "historicalDataEnd", "headTimestamp", "error"];

fn tag(f: &Fields, t: u32) -> Option<&str> {
    f.iter().find(|(k, _)| *k == t).map(|(_, v)| v.as_str())
}

/// The window id of a 35=W: the `<id>` up to its first `;;`.
fn query_id(f: &Fields) -> Option<String> {
    let q = tag(f, 6118)?;
    let a = q.find("<id>")? + 4;
    let b = a + q[a..].find("</id>")?;
    Some(q[a..b].split(";;").next()?.to_string())
}

/// A historical replay: callbacks and the queries of both sides.
struct HmdsReplayed {
    ours: Vec<String>,
    theirs: Vec<String>,
    queries_ours: Vec<String>,
    queries_theirs: Vec<String>,
    /// The 34 of ibx's queries.
    seq_ours: Vec<String>,
}

/// Replay the historical requests of a fixture: the queries ibx writes
/// against the reference's, the farm's answers (decompressed, ibx's query
/// ids in place of the reference's, compressed again) and the contract
/// lookups' replies (to ibx's lookup of the same symbol, in order).
fn replay_hmds(fx: &Fixture) -> HmdsReplayed {
    let mut s = Session::new().in_zone(&fx.header);
    let mut theirs = Vec::new();
    let mut queries_theirs: Vec<(Fields, bool)> = Vec::new();
    let mut no_end: Vec<bool> = Vec::new();
    let mut qids: Vec<(String, String)> = Vec::new();
    let mut lookups_answered = 0usize;
    for r in &fx.recs {
        let q = &r.request;
        let id = q["reqId"].as_i64().unwrap_or(0);
        match (r.leg.as_str(), r.msg.as_str()) {
            ("api_out", "REQ_HISTORICAL_DATA") => {
                let contract = contract_of(&q["contract"]);
                let st = |k: &str| q[k].as_str().unwrap_or("").to_string();
                let (end, duration, bar, what) = (st("endDateTime"), st("duration"), st("barSizeSetting"), st("whatToShow"));
                no_end.push(end.is_empty());
                let (rth, fmt, keep_up) = (q["useRTH"].as_bool().unwrap_or(false), q["formatDate"].as_i64().unwrap_or(1) as i32,
                    q["keepUpToDate"].as_bool().unwrap_or(false));
                let _ = s.call(move |c| c.req_historical_data(id, &contract, &end, &duration, &bar, &what, rth, fmt, keep_up));
            }
            ("api_out", "REQ_HEAD_TIMESTAMP") => {
                let contract = contract_of(&q["contract"]);
                let what = q["whatToShow"].as_str().unwrap_or("").to_string();
                let (rth, fmt) = (q["useRTH"].as_bool().unwrap_or(false), q["formatDate"].as_i64().unwrap_or(1) as i32);
                no_end.push(false);
                let _ = s.call(move |c| c.req_head_time_stamp(id, &contract, &what, rth, fmt));
            }
            ("api_out", "CANCEL_HEAD_TIMESTAMP") => { let _ = s.call(move |c| c.cancel_head_time_stamp(id)); }
            ("api_out", "CANCEL_HISTORICAL_DATA") => { let _ = s.call(move |c| c.cancel_historical_data(id)); }
            ("api_in", _) => theirs.extend(r.callbacks.as_array().into_iter().flatten().filter_map(canonical)),
            ("fix_in", "d") if r.conn == "CCP" => {
                // ibx's next lookup not answered yet gets this reply.
                let lookups: Vec<String> = s.ccp_out.iter().filter(|f| tag(f, 35) == Some("c"))
                    .filter_map(|f| tag(f, 320).map(str::to_string)).collect();
                if let Some(rid) = lookups.get(lookups_answered) {
                    let mut f = r.fields();
                    for (t, v) in f.iter_mut() { if *t == 320 { *v = rid.clone(); } }
                    lookups_answered += 1;
                    s.send_ccp(&rebuild_text(&f));
                }
            }
            ("fix_out", "W") if r.conn == "ushmds" => {
                let f = r.fields();
                let rank = queries_theirs.len();
                let ours: Vec<&Fields> = s.hmds_out.iter().filter(|o| tag(o, 35) == Some("W")).collect();
                if let (Some(gw), Some(oq)) = (query_id(&f), ours.get(rank).and_then(|o| query_id(o))) {
                    qids.push((gw, oq));
                }
                queries_theirs.push((f, no_end.last().copied().unwrap_or(false)));
            }
            ("fix_in", _) if r.conn == "ushmds" => {
                let msgs = if r.raw.starts_with(b"8=FIXCOMP") {
                    crate::protocol::fixcomp::fixcomp_decompress(&r.raw).unwrap_or_default()
                } else {
                    vec![r.raw.clone()]
                };
                for m in msgs {
                    let f: Fields = parse_fields(&m).into_iter().map(|(t, v)| {
                        (t, qids.iter().fold(v, |v, (gw, ours)| v.replace(&format!("<id>{gw};;"), &format!("<id>{ours};;"))
                            .replace(&format!("<id>{gw}</id>"), &format!("<id>{ours}</id>"))))
                    }).collect();
                    s.send_hmds_message(&f);
                }
            }
            _ => {}
        }
    }
    s.settle();
    let wanted = |line: &String| HMDS_CALLBACKS.iter().any(|k| line.split('|').next() == Some(*k))
        && !line.starts_with("error|-1|");
    let ours_q: Vec<String> = s.hmds_out.iter().filter(|o| tag(o, 35) == Some("W")).enumerate()
        .map(|(k, f)| hmds_query(f, queries_theirs.get(k).is_some_and(|q| q.1))).collect();
    // The start and end of a request with no end time come from the time
    // it was made.
    let mask_end = |l: &String| if l.starts_with("historicalDataEnd|") {
        let f: Vec<&str> = l.split('|').collect();
        format!("{}|{}|{{start}}|{{now}}", f[0], f[1])
    } else {
        l.clone()
    };
    HmdsReplayed {
        ours: s.callbacks.iter().filter(|l| wanted(l)).map(mask_end).collect(),
        theirs: theirs.iter().filter(|l| wanted(l)).map(mask_end).collect(),
        queries_ours: ours_q,
        seq_ours: s.hmds_out.iter().filter(|o| tag(o, 35) == Some("W")).filter_map(|o| tag(o, 34).map(str::to_string)).collect(),
        queries_theirs: queries_theirs.iter().map(|(f, mask)| hmds_query(f, *mask)).collect(),
    }
}

/// The layout of a query left out: the reference writes it with a new
/// line after each element and tabs (`jxmlable.XmlGenerator`), ibx on one
/// line.
fn flat(q: &str) -> String {
    q.replace(['\n', '\t'], "")
}

// Bars of AAPL before the open with formatDate 1 and 2, daily and hourly,
// and head timestamps (02/10/2026, b1_431_hist_format): each query as the
// reference's (the chart name in the id, no cutoff date, whole days for
// daily bars, the head timestamp's TRADES label), its layout and the
// times of a request without an end left out; the bars, ends and head
// timestamp as the reference's; the cancel of an unknown request gives
// 366. The second head timestamp is answered from the reference's cache
// (ignored test below).
#[test]
fn bars_and_head_timestamp_as_the_reference() {
    let r = replay_hmds(&load("hmds_bars_and_head_timestamp"));
    let theirs: Vec<String> = r.queries_theirs.iter().map(|q| flat(q)).collect();
    let ours: Vec<String> = r.queries_ours.iter().take(theirs.len()).map(|q| flat(q)).collect();
    assert_eq!(theirs.len(), 5);
    for (a, b) in ours.iter().zip(&theirs) {
        assert_eq!(a, b);
    }
    let not_cached = |l: &String| !l.starts_with("headTimestamp|9545|");
    let (ours, theirs): (Vec<String>, Vec<String>) =
        (r.ours.into_iter().filter(not_cached).collect(), r.theirs.into_iter().filter(not_cached).collect());
    assert!(theirs.len() > 60, "{}", theirs.len());
    assert_same_callbacks(&ours, &theirs);
}

// A second head timestamp of the same contract and data: the reference
// answers from its cache (`hmdscore.store.b`, HIST-BARS 4.3), no query,
// here with formatDate 2 (345479400); ibx asks the farm again.
#[test]
#[ignore = "ibx#486: head timestamps are not answered from a cache"]
fn second_head_timestamp_from_the_cache() {
    let r = replay_hmds(&load("hmds_bars_and_head_timestamp"));
    assert_eq!(r.queries_ours.len(), r.queries_theirs.len());
    assert_same_callbacks(&r.ours, &r.theirs);
}

// The query layout: a new line after the header and each element, tabs
// before the elements, as every captured 35=W and 35=Z; and 34=000000.
#[test]
fn query_layout_as_the_reference() {
    let r = replay_hmds(&load("hmds_bars_and_head_timestamp"));
    assert!(!r.queries_theirs.is_empty());
    assert!(r.seq_ours.len() >= r.queries_theirs.len() && r.seq_ours.iter().all(|s| s == "000000"), "{:?}", r.seq_ours);
    for (a, b) in r.queries_ours.iter().zip(&r.queries_theirs) {
        assert_eq!(a, b);
    }
}

#[test]
fn dump_hmds() {
    if std::env::var_os("IBX_GOLDEN_DUMP").is_none() { return; }
    let r = replay_hmds(&load("hmds_bars_and_head_timestamp"));
    for k in 0..r.queries_ours.len().max(r.queries_theirs.len()) {
        eprintln!("Q ours {}", r.queries_ours.get(k).map_or("", |s| s.as_str()));
        eprintln!("  want {}", r.queries_theirs.get(k).map_or("", |s| s.as_str()));
    }
    assert_same_callbacks(&r.ours, &r.theirs);
}
