//! Account summary (35=U|6040=55 request, 35=UM / 35=RL / 35=EB answer):
//! the request against the reference's, the rows against the callbacks
//! the reference gave.

use super::load;
use crate::test_support::scenario::record::{binary_body, canonical, rebuild_binary};
use crate::test_support::scenario::{assert_same_callbacks, Scenario as Fixture, Session};
use crate::test_support::{to_pipe, Fields, Normaliser};

fn tag(f: &Fields, t: u32) -> Option<&str> {
    f.iter().find(|(k, _)| *k == t).map(|(_, v)| v.as_str())
}

/// An account summary request as compared: the framing dropped, the
/// subscription id (6529) masked.
fn request_message(f: &Fields) -> String {
    let out: Fields = Normaliser::framing().apply(f).into_iter()
        .map(|(t, v)| if t == 6529 { (t, "{id}".to_string()) } else { (t, v) }).collect();
    to_pipe(&out)
}

struct AccountReplay {
    ours: Vec<String>,
    theirs: Vec<String>,
    requests_ours: Vec<String>,
    requests_theirs: Vec<String>,
}

/// Replay the account summary requests of a fixture: the server's frames
/// with ibx's subscription id in place of the reference's.
fn replay_account_summary(fx: &Fixture) -> AccountReplay {
    let mut s = Session::new().in_zone(&fx.header);
    let mut theirs = Vec::new();
    let mut requests_theirs = Vec::new();
    // The reference's subscription id and ibx's, in request order.
    let mut ids: Vec<(String, String)> = Vec::new();
    let summary = |f: &Fields| tag(f, 35) == Some("U") && tag(f, 6040) == Some("55");
    for r in &fx.recs {
        match (r.leg.as_str(), r.msg.as_str()) {
            ("api_out", "REQ_ACCOUNT_SUMMARY") => {
                let q = r.request.clone();
                let id = q["reqId"].as_i64().unwrap();
                let (group, tags) = (q["group"].as_str().unwrap_or("").to_string(), q["tags"].as_str().unwrap_or("").to_string());
                s.call(move |c| c.req_account_summary(id, &group, &tags));
            }
            ("api_out", "CANCEL_ACCOUNT_SUMMARY") => {
                let id = r.request["reqId"].as_i64().unwrap();
                s.call(move |c| c.cancel_account_summary(id));
            }
            ("api_in", _) => theirs.extend(r.callbacks.as_array().into_iter().flatten().filter_map(canonical)),
            ("fix_out", "U") if summary(&r.fields()) => {
                requests_theirs.push(request_message(&r.fields()));
                // Pair the subscribe with ibx's subscribe of the same rank.
                if let Some(gw) = tag(&r.fields(), 6529).filter(|_| tag(&r.fields(), 6036) == Some("1")) {
                    let rank = ids.len();
                    let ours = s.ccp_out.iter().filter(|f| summary(f) && tag(f, 6036) == Some("1"))
                        .nth(rank).and_then(|f| tag(f, 6529)).map(str::to_string);
                    if let Some(ours) = ours { ids.push((gw.to_string(), ours)); }
                }
            }
            ("fix_in", "UM" | "RL" | "EB" | "UT" | "UP") => {
                // The subscription id is a text field of the frame.
                let body = ids.iter().fold(binary_body(&r.raw), |b, (gw, ours)| {
                    b.replace(&format!("6529={gw}"), &format!("6529={ours}"))
                });
                s.send_ccp(&rebuild_binary(&r.raw, &body));
            }
            _ => {}
        }
    }
    s.settle();
    let keep = |l: &String| l.starts_with("accountSummary");
    AccountReplay {
        ours: s.callbacks.iter().filter(|l| keep(l)).cloned().collect(),
        theirs: theirs.into_iter().filter(keep).collect(),
        requests_ours: s.ccp_out.iter().filter(|f| summary(f)).map(request_message).collect(),
        requests_theirs,
    }
}

// reqAccountSummary of four tags and $LEDGER:ALL, then its cancel
// (26/09/2026, account_summary, market closed): the subscription and its
// cancel (with the group) as the reference's; the four tag rows of the
// 35=UM frame.
#[test]
fn account_summary_request_and_tag_rows() {
    let r = replay_account_summary(&load("account_summary"));
    assert_eq!(r.requests_ours, r.requests_theirs);
    assert_eq!(r.requests_theirs.len(), 2);
    assert_same_callbacks(&r.ours[..4], &r.theirs[..4]);
}

// The rest of the answer: the reference gives the tag rows twice, and the
// $LEDGER:ALL rows per currency (USD, then BASE) for the account "All"
// written as its account values (933115.0500 gives 933115.05), then the
// end, twice (ACCOUNT-SUMMARY 1.3 and 1.5: the ALL sum of ef.a(); the
// listener registered twice by b2.o()).
#[test]
fn account_summary_ledger_all_rows() {
    let r = replay_account_summary(&load("account_summary"));
    assert!(r.theirs.len() > 100, "{}", r.theirs.len());
    assert_same_callbacks(&r.ours, &r.theirs);
}
