//! Replay of a codec fixture through the API client and the engine: the
//! recorded API requests are made again, the recorded server frames are
//! sent on the links with the request ids of the engine in place of the
//! reference's, and the callbacks are collected on both sides.

use std::collections::HashMap;

use serde_json::Value;

use crate::api::types::Contract;
use crate::test_support::Fields;

use super::fixture::{binary_body, canonical, rebuild_binary, rebuild_text, Fixture, Rec};
use super::session::{Recorder, Session};

/// The callbacks of a replay: ibx's and the reference's, one line each.
pub(crate) struct Replayed {
    pub ours: Vec<String>,
    pub theirs: Vec<String>,
    /// Recorded server frames not sent (seq, link): no request of ibx
    /// matches them.
    pub unsent: Vec<(u64, String)>,
    pub session: Session,
}

/// A contract of a request: the client library's object or the request's
/// protobuf fields (the names differ for the primary exchange).
pub(crate) fn contract_of(v: &Value) -> Contract {
    let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
    Contract {
        con_id: v["conId"].as_i64().unwrap_or(0),
        symbol: s("symbol"),
        sec_type: s("secType"),
        exchange: s("exchange"),
        currency: s("currency"),
        primary_exchange: if v["primaryExchange"].is_string() { s("primaryExchange") } else { s("primaryExch") },
        last_trade_date_or_contract_month: s("lastTradeDateOrContractMonth"),
        strike: v["strike"].as_f64().unwrap_or(0.0),
        right: s("right"),
        multiplier: s("multiplier"),
        local_symbol: s("localSymbol"),
        trading_class: s("tradingClass"),
        ..Default::default()
    }
}

/// The entries of a market data message (`35=V`): (262, 263, 6008, 207, 264).
fn entries(f: &Fields) -> Vec<(String, String, String, String, String)> {
    let action = f.iter().find(|(t, _)| *t == 263).map(|(_, v)| v.clone()).unwrap_or_default();
    let mut out = Vec::new();
    let mut cur: Option<[String; 4]> = None;
    for (t, v) in f {
        match t {
            262 => {
                if let Some([id, c, e, k]) = cur.take() { out.push((id, action.clone(), c, e, k)); }
                cur = Some([v.clone(), String::new(), String::new(), String::new()]);
            }
            6008 => if let Some(c) = cur.as_mut() { c[1] = v.clone() },
            207 => if let Some(c) = cur.as_mut() { c[2] = v.clone() },
            264 => if let Some(c) = cur.as_mut() { c[3] = v.clone() },
            _ => {}
        }
    }
    if let Some([id, c, e, k]) = cur { out.push((id, action, c, e, k)); }
    out
}

fn msg_type(f: &Fields) -> &str {
    f.iter().find(|(t, _)| *t == 35).map_or("", |(_, v)| v.as_str())
}

fn tag<'a>(f: &'a Fields, tag: u32) -> Option<&'a str> {
    f.iter().find(|(t, _)| *t == tag).map(|(_, v)| v.as_str())
}

/// The ids the engine and the reference gave the same farm entry and the
/// same contract lookup.
#[derive(Default)]
struct Ids {
    /// Reference farm id → engine farm id.
    farm: HashMap<String, String>,
    /// Reference lookup id (320) → engine lookup id.
    lookup: HashMap<String, String>,
    /// The engine's entries and lookups already given a reference id.
    farm_taken: Vec<String>,
    lookup_taken: Vec<String>,
}

impl Ids {
    /// Pair the reference's subscribe entries of `theirs` with the engine's
    /// entries of the same contract, exchange and type.
    fn pair_farm(&mut self, theirs: &Fields, ours: &[Fields]) {
        for (gw_id, action, con_id, exch, kind) in entries(theirs) {
            if action != "1" || self.farm.contains_key(&gw_id) { continue; }
            let found = ours.iter().filter(|f| msg_type(f) == "V").flat_map(entries)
                .find(|(id, a, c, e, k)| a == "1" && *c == con_id && *e == exch && *k == kind && !self.farm_taken.contains(id));
            if let Some((id, ..)) = found {
                self.farm_taken.push(id.clone());
                self.farm.insert(gw_id, id);
            }
        }
    }

    fn pair_lookup(&mut self, theirs: &Fields, ours: &[Fields]) {
        let Some(gw_id) = tag(theirs, 320) else { return };
        if self.lookup.contains_key(gw_id) { return; }
        let key = |f: &Fields| (tag(f, 55).map(str::to_string), tag(f, 167).map(str::to_string), tag(f, 6008).map(str::to_string));
        let found = ours.iter().filter(|f| msg_type(f) == "c")
            .find(|f| key(f) == key(theirs) && tag(f, 320).is_some_and(|id| !self.lookup_taken.iter().any(|t| t == id)));
        if let Some(id) = found.and_then(|f| tag(f, 320)) {
            self.lookup_taken.push(id.to_string());
            self.lookup.insert(gw_id.to_string(), id.to_string());
        }
    }
}

/// Make the recorded API request again. Requests the replay does not cover
/// are left out.
pub(crate) fn request(s: &mut Session, r: &Rec) {
    let q = &r.request;
    let id = q["reqId"].as_i64().unwrap_or(0);
    match r.msg.as_str() {
        "REQ_MKT_DATA" => {
            let contract = contract_of(&q["contract"]);
            let ticks = q["genericTickList"].as_str().unwrap_or("").to_string();
            let snapshot = q["snapshot"].as_bool().unwrap_or(false);
            let regulatory = q["regulatorySnapshot"].as_bool().unwrap_or(false);
            let _ = s.call(move |c| c.req_mkt_data(id, &contract, &ticks, snapshot, regulatory));
        }
        "CANCEL_MKT_DATA" => { let _ = s.call(move |c| c.cancel_mkt_data(id)); }
        "REQ_MARKET_DATA_TYPE" => {
            let t = q["marketDataType"].as_i64().unwrap_or(1) as i32;
            s.call(move |c| c.req_market_data_type(t));
        }
        "REQ_SMART_COMPONENTS" => {
            let bbo = q["bboExchange"].as_str().unwrap_or("").to_string();
            let lines = s.call(move |c| {
                let mut rec = Recorder::default();
                c.req_smart_components(id, &bbo, &mut rec);
                rec.lines
            });
            s.callbacks.extend(lines);
        }
        _ => {}
    }
}

/// Replay the market data part of a fixture: the requests, the lookups'
/// replies on the auth link, and the farm's frames of `farm_conn`.
/// `keep` chooses the callbacks compared (by name); the replay stops at
/// the record `until` (a request on another farm, for example).
pub(crate) fn replay_market_data(fx: &Fixture, farm_conn: &str, keep: &[&str], until: Option<u64>) -> Replayed {
    let mut s = Session::new();
    let mut ids = Ids::default();
    let mut theirs = Vec::new();
    let mut unsent = Vec::new();
    for r in fx.recs.iter().take_while(|r| until.is_none_or(|u| r.seq < u)) {
        match r.leg.as_str() {
            "api_out" => request(&mut s, r),
            "api_in" => theirs.extend(r.callbacks.as_array().into_iter().flatten().filter_map(canonical)),
            "fix_out" if r.conn == farm_conn && r.msg == "V" => ids.pair_farm(&r.fields(), &s.farm_out),
            "fix_out" if r.conn == "CCP" && r.msg == "c" => ids.pair_lookup(&r.fields(), &s.ccp_out),
            "fix_in" if r.conn == "CCP" && r.msg == "d" => {
                // Pair lookups the engine made after the reference's.
                for o in fx.recs.iter().filter(|o| o.is("fix_out", "CCP", "c") && o.seq < r.seq) {
                    ids.pair_lookup(&o.fields(), &s.ccp_out);
                }
                let mut f = r.fields();
                let ours = f.iter().find(|(t, _)| *t == 320).and_then(|(_, v)| ids.lookup.get(v).cloned());
                match ours {
                    Some(id) => {
                        for (t, v) in f.iter_mut() { if *t == 320 { *v = id.clone(); } }
                        s.send_ccp(&rebuild_text(&f));
                    }
                    None => unsent.push((r.seq, r.conn.clone())),
                }
            }
            "fix_in" if r.conn == farm_conn => {
                for o in fx.recs.iter().filter(|o| o.is("fix_out", farm_conn, "V") && o.seq < r.seq) {
                    ids.pair_farm(&o.fields(), &s.farm_out);
                }
                match r.msg.as_str() {
                    "Q" => {
                        let body = binary_body(&r.raw);
                        let mut parts: Vec<String> = body.split(',').map(str::to_string).collect();
                        match parts.get(1).and_then(|id| ids.farm.get(id)) {
                            Some(id) => {
                                parts[1] = id.clone();
                                s.send_farm(&rebuild_binary(&r.raw, &parts.join(",")));
                            }
                            None => unsent.push((r.seq, r.conn.clone())),
                        }
                    }
                    "3" => {
                        let mut f = r.fields();
                        let ours = f.iter().find(|(t, _)| *t == 262).and_then(|(_, v)| ids.farm.get(v).cloned());
                        match ours {
                            Some(id) => {
                                for (t, v) in f.iter_mut() { if *t == 262 { *v = id.clone(); } }
                                s.send_farm(&rebuild_text(&f));
                            }
                            None => unsent.push((r.seq, r.conn.clone())),
                        }
                    }
                    _ => s.send_farm(&r.raw),
                }
            }
            _ => {}
        }
    }
    s.settle();
    let wanted = |line: &String| keep.iter().any(|k| line.split('|').next() == Some(*k));
    let ours = s.callbacks.iter().filter(|l| wanted(l)).cloned().collect();
    let theirs = theirs.into_iter().filter(wanted).collect();
    Replayed { ours, theirs, unsent, session: s }
}

/// The two lists of callbacks are the same; on a difference, the first one
/// and the callbacks around it.
#[track_caller]
pub(crate) fn assert_same_callbacks(ours: &[String], theirs: &[String]) {
    if std::env::var_os("IBX_GOLDEN_DUMP").is_some() {
        for k in 0..ours.len().max(theirs.len()) {
            let (a, b) = (ours.get(k).map_or("", |s| s.as_str()), theirs.get(k).map_or("", |s| s.as_str()));
            eprintln!("{k:4} {} {a:<60} {b}", if a == b { ' ' } else { '*' });
        }
    }
    let first = ours.iter().zip(theirs).position(|(a, b)| a != b).unwrap_or(ours.len().min(theirs.len()));
    if first < ours.len().max(theirs.len()) {
        let around = |v: &[String]| v[first.saturating_sub(3)..(first + 6).min(v.len())].join("\n    ");
        panic!(
            "callback {first} differs ({} ours, {} reference)\n  ours:\n    {}\n  reference:\n    {}",
            ours.len(), theirs.len(), around(ours), around(theirs),
        );
    }
}
