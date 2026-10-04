//! Replay of a codec fixture through the API client and the engine: the
//! recorded API requests are made again, the recorded server frames are
//! sent on the links with the request ids of the engine in place of the
//! reference's, and the callbacks are collected on both sides.

use std::collections::HashMap;

use crate::test_support::scenario::record::{binary_body, canonical, rebuild_binary, rebuild_text};
use crate::test_support::scenario::{Rec, Recorder, Scenario as Fixture, Session};
use crate::test_support::Fields;

/// The callbacks of a replay: ibx's and the reference's, one line each.
pub(crate) struct Replayed {
    pub ours: Vec<String>,
    pub theirs: Vec<String>,
    /// Recorded server frames not sent (seq, link): no request of ibx
    /// matches them.
    pub unsent: Vec<(u64, String)>,
    /// The market data messages (35=V) and contract lookups (35=c) of the
    /// reference and of ibx, normalised (see [`request_messages`]).
    pub requests_theirs: Vec<String>,
    pub session: Session,
}

/// A market data message (35=V) or a contract lookup (35=c) as compared:
/// the framing dropped, the farm request ids (262) and the lookup id (320)
/// masked, as the reference's and ibx's numbering differ.
pub(crate) fn request_message(f: &Fields) -> String {
    let n = crate::test_support::Normaliser::framing();
    let out: Fields = n.apply(f).into_iter().map(|(t, v)| match t {
        262 => (t, "{id}".to_string()),
        320 => (t, "{lookup}".to_string()),
        _ => (t, v),
    }).collect();
    crate::test_support::to_pipe(&out)
}

/// ibx's market data messages and contract lookups, as compared.
pub(crate) fn request_messages(s: &Session, farm_types: &[&str], ccp_types: &[&str]) -> (Vec<String>, Vec<String>) {
    let pick = |msgs: &[Fields], types: &[&str]| msgs.iter().filter(|f| types.contains(&msg_type(f))).map(request_message).collect();
    (pick(&s.farm_out, farm_types), pick(&s.ccp_out, ccp_types))
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

fn tag(f: &Fields, tag: u32) -> Option<&str> {
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

/// Make the recorded API request again (`test_support::scenario::request`);
/// its callbacks given at once are kept.
pub(crate) fn request(s: &mut Session, r: &Rec) {
    let r = r.clone();
    let lines = s.call(move |c| {
        let mut rec = Recorder::default();
        crate::test_support::scenario::request::make(c, &r, &mut rec);
        rec.lines
    });
    s.callbacks.extend(lines);
}

/// Replay the market data part of a fixture: the requests, the lookups'
/// replies on the auth link, and the farm's frames of `farm_conn`.
/// `keep` chooses the callbacks compared (by name); the replay stops at
/// the record `until` (a request on another farm, for example).
pub(crate) fn replay_market_data(fx: &Fixture, farm_conn: &str, keep: &[&str], until: Option<u64>) -> Replayed {
    replay_market_data_without(fx, farm_conn, keep, until, &[])
}

/// [`replay_market_data`] without the records `skip` (requests that go to
/// another farm).
pub(crate) fn replay_market_data_without(fx: &Fixture, farm_conn: &str, keep: &[&str], until: Option<u64>, skip: &[u64]) -> Replayed {
    let mut s = Session::new().in_zone(&fx.header);
    let mut ids = Ids::default();
    let mut theirs = Vec::new();
    let mut unsent = Vec::new();
    let mut requests_theirs = Vec::new();
    for r in fx.recs.iter().take_while(|r| until.is_none_or(|u| r.seq < u)).filter(|r| !skip.contains(&r.seq)) {
        match r.leg.as_str() {
            "api_out" => request(&mut s, r),
            "api_in" => theirs.extend(r.callbacks.as_array().into_iter().flatten().filter_map(canonical)),
            "fix_out" if r.conn == farm_conn && r.msg == "V" => {
                requests_theirs.push(request_message(&r.fields()));
                ids.pair_farm(&r.fields(), &s.farm_out)
            }
            "fix_out" if r.conn == "CCP" && r.msg == "c" => {
                requests_theirs.push(request_message(&r.fields()));
                ids.pair_lookup(&r.fields(), &s.ccp_out)
            }
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
    Replayed { ours, theirs, unsent, requests_theirs, session: s }
}
