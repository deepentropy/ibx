//! The scenario runner (ibx#487): a recorded scenario replayed through an
//! API client and the engine, against a scripted peer that plays the
//! recorded servers.
//!
//! The records are taken in their recorded order:
//! - `api_out`: the API client makes the request again;
//! - `fix_out`: the frame the reference sent. A frame of a compared kind
//!   (see [`kind`]) must be the next frame of that kind ibx sent on that
//!   link, the same after normalisation; a missing or different frame ends
//!   the run there. Frames of the other kinds are only paired with ibx's,
//!   for their ids;
//! - `fix_in`: the frame the servers sent, with the ids of ibx in place of
//!   the reference's (order ids and permIds, farm request ids, lookup ids,
//!   subscription ids, historical query ids, sequence numbers), sent to
//!   ibx on the same link;
//! - `api_in`: the callbacks the reference gave.
//!
//! At the end, a frame of a compared kind that ibx sent and the reference
//! did not is unexpected; the callbacks ibx gave must be the reference's:
//! the same callbacks, fields, count and order.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::api::client::EClient;
use crate::test_support::{to_pipe, Fields, Normaliser};

use super::record::{binary_body, canonical, rebuild_binary, rebuild_text, Rec, Scenario};
use super::request::make;
use super::session::{send_hmds, Links, Recorder, ACCOUNT};

/// The API client side of a replay: the Rust client ([`RustDriver`]) or
/// another one (the Python client, `src/python`).
pub trait Driver {
    /// The session's client id (from the recorded startApi).
    fn start(&mut self, client_id: i64);
    /// Make the recorded request; the engine runs meanwhile (see
    /// [`Links::during`]). False when the driver does not make it.
    fn request(&mut self, links: &mut Links, r: &Rec) -> bool;
    /// The callbacks waiting, one line each in the form of
    /// [`super::record::canonical`].
    fn dispatch(&mut self) -> Vec<String>;
}

/// The Rust API client.
pub struct RustDriver {
    pub client: EClient,
    /// Callbacks a request gave at once.
    pending: Vec<String>,
}

impl RustDriver {
    pub fn new(links: &Links) -> Self {
        let client = EClient::from_parts(links.shared.clone(), links.control_tx.clone(), std::thread::spawn(|| {}), ACCOUNT.into());
        Self { client, pending: Vec::new() }
    }
}

impl Driver for RustDriver {
    fn start(&mut self, client_id: i64) {
        self.client.core.client_id.store(client_id, std::sync::atomic::Ordering::Relaxed);
    }

    fn request(&mut self, links: &mut Links, r: &Rec) -> bool {
        let client = &self.client;
        let mut rec = Recorder::default();
        let made = links.during(|| make(client, r, &mut rec));
        self.pending.extend(rec.lines);
        made
    }

    fn dispatch(&mut self) -> Vec<String> {
        let mut rec = Recorder { lines: std::mem::take(&mut self.pending) };
        self.client.process_msgs(&mut rec);
        rec.lines
    }
}

/// The kinds of frames ibx sends, as compared.
pub const ORDER: &str = "order";
pub const LOOKUP: &str = "lookup";
pub const MARKET_DATA: &str = "market_data";
pub const SUBSCRIPTION: &str = "account_subscription";
pub const HISTORICAL: &str = "historical";
pub const SCANNER: &str = "scanner";

/// The links of the replay.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Link {
    Farm,
    Ccp,
    Hmds,
}

impl Link {
    /// The link of a recorded connection; the other farms are not replayed
    /// (the engine opens them on demand, which the replay does not do).
    pub fn of(conn: &str) -> Option<Link> {
        match conn {
            "usfarm" => Some(Link::Farm),
            "CCP" => Some(Link::Ccp),
            "ushmds" => Some(Link::Hmds),
            _ => None,
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

fn tag(f: &Fields, t: u32) -> Option<&str> {
    f.iter().find(|(k, _)| *k == t).map(|(_, v)| v.as_str())
}

fn msg_type(f: &Fields) -> &str {
    tag(f, 35).unwrap_or("")
}

/// The kind of a frame sent on a link; `None` for session frames
/// (heartbeats, logons, client notices), never compared.
pub fn kind(link: Link, f: &Fields) -> Option<&'static str> {
    let svc = tag(f, 6040).unwrap_or("");
    Some(match (link, msg_type(f)) {
        (Link::Ccp, "D" | "G" | "F") => ORDER,
        (Link::Ccp, "c") => LOOKUP,
        // A definition query of another form (combo multiplier, leg
        // confirmation): answered by its 320 like a lookup.
        (Link::Ccp, "U") if tag(f, 320).is_some() => LOOKUP,
        (Link::Ccp, "U") if tag(f, 6529).is_some() => SUBSCRIPTION,
        (Link::Farm, "V") => MARKET_DATA,
        (Link::Hmds, "W" | "Z") => HISTORICAL,
        (Link::Hmds, "U") if matches!(svc, "10001" | "10003" | "10004") => SCANNER,
        _ => return None,
    })
}

/// The base of an order id (`1288736453.1` gives `1288736453`).
pub fn base(id: &str) -> &str {
    id.split('.').next().unwrap_or(id)
}

/// An order message as compared: the session fields normalised, and the
/// order attributes, which the reference writes in no fixed order, sorted
/// in their place.
pub fn comparable(f: &Fields) -> Fields {
    let mut out = Normaliser::session().apply(f);
    let attr = |t: u32| (70..100).contains(&crate::engine::hot_loop::order_builder::reference_rank(t));
    let mut k = 0;
    while k < out.len() {
        if attr(out[k].0) {
            let end = (k..out.len()).find(|&j| !attr(out[j].0)).unwrap_or(out.len());
            out[k..end].sort();
            k = end;
        } else {
            k += 1;
        }
    }
    out
}

/// A historical farm query (35=W) as compared: the framing dropped; in the
/// query, the id before its first `;;` (the reference's counter, cf76) and
/// the end time of a request with none (the time it was made) masked.
pub fn hmds_query(f: &Fields, mask_end: bool) -> String {
    let out: Fields = Normaliser::framing().apply(f).into_iter().map(|(t, mut v)| {
        if t == 6118 {
            if let (Some(a), Some(b)) = (v.find("<id>"), v.find("</id>")) {
                let end = v[a + 4..b].find(";;").map_or(b, |k| a + 4 + k);
                v.replace_range(a + 4..end, "{id}");
            }
            if mask_end && let (Some(a), Some(b)) = (v.find("<endTime>"), v.find("</endTime>")) {
                v.replace_range(a + 9..b, "{now}");
            }
        }
        (t, v)
    }).collect();
    to_pipe(&out)
}

/// The window id of a historical query or cancel: the `<id>` up to its
/// first `;;`.
fn query_id(f: &Fields) -> Option<String> {
    let q = tag(f, 6118)?;
    let a = q.find("<id>")? + 4;
    let b = a + q[a..].find("</id>")?;
    Some(q[a..b].split(";;").next()?.to_string())
}

/// A frame of a kind as compared, one line. The ids each side numbers its
/// own way are masked.
pub fn normalised(kind: &str, f: &Fields, mask_end: bool) -> String {
    let mask = |ids: &[u32]| -> String {
        let out: Fields = Normaliser::framing().apply(f).into_iter()
            .map(|(t, v)| if ids.contains(&t) { (t, "{id}".to_string()) } else { (t, v) }).collect();
        to_pipe(&out)
    };
    match kind {
        ORDER => to_pipe(&comparable(f)),
        LOOKUP => mask(&[320]),
        MARKET_DATA => mask(&[262]),
        SUBSCRIPTION => mask(&[6529]),
        HISTORICAL => hmds_query(f, mask_end),
        _ => mask(&[]),
    }
}

/// The entries of a market data message (`35=V`): (262, 263, 6008, 207, 264).
fn entries(f: &Fields) -> Vec<(String, String, String, String, String)> {
    let action = tag(f, 263).unwrap_or("").to_string();
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

/// A contract lookup (35=c) and its reply (35=d) name the same contract:
/// the same conId when both have one, else the same symbol and type.
/// A lookup names a stock `CS`, its reply `STK`.
fn same_contract(a: &Fields, b: &Fields) -> bool {
    fn sec_type(f: &Fields) -> Option<&str> {
        match tag(f, 167) {
            Some("CS") => Some("STK"),
            t => t,
        }
    }
    match (tag(a, 6008), tag(b, 6008)) {
        (Some(x), Some(y)) => x == y,
        _ => tag(a, 55).is_some() && tag(a, 55) == tag(b, 55) && sec_type(a) == sec_type(b),
    }
}

/// A lookup as matched with the reference's: its fields with the id
/// masked, and the label of the id (`EComboReqByConid` of
/// `EComboReqByConid83`).
fn lookup_key(f: &Fields) -> String {
    let label = tag(f, 320).unwrap_or("").trim_end_matches(|c: char| c.is_ascii_digit()).to_string();
    format!("{label}|{}", normalised(LOOKUP, f, false))
}

/// The ids ibx and the reference gave the same thing: reference id → ibx id.
#[derive(Default)]
struct Ids {
    /// Whole ClOrdIDs of the new orders (a preview's version differs).
    clord: HashMap<String, String>,
    /// Order id bases (ClOrdID, permId, OCA group).
    order: HashMap<String, String>,
    /// Farm request ids (262).
    farm: HashMap<String, String>,
    farm_taken: HashSet<String>,
    /// Lookup ids (320); ibx's lookups paired or answered.
    lookup: HashMap<String, String>,
    lookup_taken: HashSet<String>,
    /// The reference's lookups whose reply answered one of ibx's.
    lookup_used: HashSet<String>,
    /// Account subscription ids (6529).
    subscription: Vec<(String, String)>,
    /// Historical query ids.
    query: Vec<(String, String)>,
}

/// What a test leaves out of a replay, or masks.
#[derive(Clone)]
pub struct Options {
    /// The kinds of frames compared in order (see [`kind`]).
    pub compare: Vec<&'static str>,
    /// The records from this seq on are left out.
    pub until: Option<u64>,
    /// API orders left out: their requests, frames, reports and callbacks.
    pub skip_orders: Vec<i64>,
    /// Records left out by seq (a request whose answer the recording does
    /// not hold).
    pub skip_seqs: Vec<u64>,
    /// The callbacks compared (after the session notices are left out).
    pub keep: fn(&str) -> bool,
    /// A known difference masked on both sides of the callbacks.
    pub mask: fn(&str) -> String,
    /// A known difference masked on both sides of a compared frame.
    pub frame_mask: fn(&mut Fields),
    /// Frames left out of the comparison on both sides (a frame that
    /// depends on what the reference had before the recording).
    pub skip_frame: fn(&Fields) -> bool,
    /// Recorded lookups and replies of another scenario of the same day,
    /// for the definitions the reference had before this one (see
    /// [`Options::replies_from`]).
    pub replies: Vec<Rec>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            compare: vec![ORDER, MARKET_DATA, SUBSCRIPTION, HISTORICAL, SCANNER],
            until: None,
            skip_orders: Vec::new(),
            skip_seqs: Vec::new(),
            keep: |_| true,
            mask: |l| l.to_string(),
            frame_mask: |_| {},
            skip_frame: |_| false,
            replies: Vec::new(),
        }
    }
}

impl Options {
    pub fn compare(mut self, kinds: &[&'static str]) -> Self {
        self.compare = kinds.to_vec();
        self
    }

    pub fn until(mut self, seq: u64) -> Self {
        self.until = Some(seq);
        self
    }

    pub fn skip_orders(mut self, ids: &[i64]) -> Self {
        self.skip_orders = ids.to_vec();
        self
    }

    pub fn skip_seqs(mut self, seqs: &[u64]) -> Self {
        self.skip_seqs = seqs.to_vec();
        self
    }

    pub fn keep(mut self, keep: fn(&str) -> bool) -> Self {
        self.keep = keep;
        self
    }

    pub fn mask(mut self, mask: fn(&str) -> String) -> Self {
        self.mask = mask;
        self
    }

    /// Answer ibx's lookups also from the lookups of `sc` (definitions are
    /// server data: the same in every session).
    pub fn replies_from(mut self, sc: &Scenario) -> Self {
        self.replies.extend(sc.recs.iter().filter(|r| r.conn == "CCP" && r.raw.starts_with(b"8=FIX") && r.get(320).is_some()).cloned());
        self
    }

    pub fn skip_frame(mut self, skip: fn(&Fields) -> bool) -> Self {
        self.skip_frame = skip;
        self
    }

    pub fn frame_mask(mut self, mask: fn(&mut Fields)) -> Self {
        self.frame_mask = mask;
        self
    }
}

/// The result of a replay.
#[derive(Debug, Default)]
pub struct Outcome {
    /// The first frame of a compared kind ibx did not send as the
    /// reference did (missing, different or unexpected); the run stopped
    /// there.
    pub frame_error: Option<String>,
    /// The frames compared and found the same.
    pub frames_compared: usize,
    /// The callbacks compared: ibx's, and the reference's with their seq.
    pub ours: Vec<String>,
    pub theirs: Vec<(u64, String)>,
    /// Recorded server frames not sent: no request of ibx matches them, or
    /// their link is not replayed (seq, connection).
    pub unsent: Vec<(u64, String)>,
    /// Recorded requests the driver does not make (seq, name).
    pub not_made: Vec<(u64, String)>,
}

impl Outcome {
    /// No frame error, every request made, the same callbacks.
    #[track_caller]
    pub fn assert_same(&self) {
        if let Some(e) = &self.frame_error {
            panic!("{e}");
        }
        assert!(self.not_made.is_empty(), "requests the replay does not make: {:?}", self.not_made);
        let theirs: Vec<String> = self.theirs.iter().map(|(_, l)| l.clone()).collect();
        assert_same_callbacks(&self.ours, &theirs);
    }
}

/// The two lists of callbacks are the same; on a difference, the first one
/// and the callbacks around it.
#[track_caller]
pub fn assert_same_callbacks(ours: &[String], theirs: &[String]) {
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

/// A session notice of the reference (data farm status, id -1): ibx gives
/// its own at its own times.
fn session_notice(line: &str) -> bool {
    line.starts_with("error|-1|21") && line.contains(" farm ")
}

/// A callback line with its session values masked:
/// - the end of a historical request made with no end time is the time it
///   was made (`now` holds those requests);
/// - the class name a 321 "Error validating request" names is the class of
///   the request as the API client encoded it: a protobuf client (the
///   recordings) gets other letters than the one ibx gives
///   (src/control/generic_tick.rs, `CAPTURED_STK`).
pub fn session_values(line: &str, now: &HashSet<String>) -> String {
    let f: Vec<&str> = line.split('|').collect();
    if f[0] == "historicalDataEnd" && f.len() > 1 && now.contains(f[1]) {
        return format!("historicalDataEnd|{}|{{start}}|{{now}}", f[1]);
    }
    if f[0] == "error" && f.get(2) == Some(&"321")
        && let Some(a) = line.find("Error validating request.-'")
    {
        let start = a + "Error validating request.-'".len();
        if let Some(len) = line[start..].find('\'') {
            return format!("{}{{class}}{}", &line[..start], &line[start + len..]);
        }
    }
    line.to_string()
}

/// The records with each order request moved to where the reference acted
/// on it: just before its first effect, the order message it sent or the
/// error it gave at once. The reference reads a request on its API thread
/// and may act on it after reports that came in the meantime (lmt_cancel
/// of 26/09/2026: the cancel request, two reports, then the 35=F); the
/// replay makes ibx act at that same point.
pub fn at_their_effect(recs: &[Rec]) -> Vec<Rec> {
    let mut by_base: HashMap<String, i64> = HashMap::new();
    let order_of = |r: &Rec, by_base: &HashMap<String, i64>| -> Option<i64> {
        match r.msg.as_str() {
            "D" => r.get(6121).and_then(|v| v.parse().ok()),
            "G" | "F" => r.get(41).and_then(|c| by_base.get(base(&c)).copied()),
            _ => None,
        }
    };
    let mut effect_of: HashMap<usize, usize> = HashMap::new();
    for (i, r) in recs.iter().enumerate() {
        if r.leg == "fix_out" && r.msg == "D"
            && let (Some(c), Some(id)) = (r.get(11), r.get(6121).and_then(|v| v.parse::<i64>().ok()))
        {
            by_base.insert(base(&c).to_string(), id);
        }
        if !(r.leg == "api_out" && matches!(r.msg.as_str(), "PLACE_ORDER" | "CANCEL_ORDER")) { continue; }
        let Some(id) = r.request["orderId"].as_i64() else { continue };
        let mut scan = by_base.clone();
        for (j, e) in recs.iter().enumerate().skip(i + 1) {
            if e.leg == "api_out" && e.request["orderId"].as_i64() == Some(id) { break; }
            if e.leg == "fix_out" && e.msg == "D"
                && let (Some(c), Some(oid)) = (e.get(11), e.get(6121).and_then(|v| v.parse::<i64>().ok()))
            {
                scan.insert(base(&c).to_string(), oid);
            }
            let sent = e.leg == "fix_out" && order_of(e, &scan) == Some(id);
            let refused = e.leg == "api_in" && e.callbacks.as_array().into_iter().flatten().any(|c| {
                c[0] == "error" && c[1].as_i64() == Some(id) && !matches!(c[3].as_i64(), Some(399 | 201 | 202))
            });
            if sent || refused {
                effect_of.insert(i, j);
                break;
            }
        }
    }
    let mut out: Vec<Rec> = Vec::with_capacity(recs.len());
    let moved: Vec<usize> = effect_of.keys().copied().collect();
    for (j, r) in recs.iter().enumerate() {
        let mut before: Vec<usize> = effect_of.iter().filter(|(_, e)| **e == j).map(|(i, _)| *i).collect();
        before.sort();
        out.extend(before.into_iter().map(|i| recs[i].clone()));
        if !moved.contains(&j) {
            out.push(r.clone());
        }
    }
    out
}

/// The records a replay takes: up to `until`, without `skip_seqs` and the
/// skipped orders, each order request at the reference's effect.
fn prepare(sc: &Scenario, opts: &Options) -> Vec<Rec> {
    let recs: Vec<Rec> = sc.recs.iter()
        .filter(|r| opts.until.is_none_or(|u| r.seq < u) && !opts.skip_seqs.contains(&r.seq))
        .cloned().collect();
    // The order id bases of the skipped orders.
    let skipped: HashSet<String> = recs.iter()
        .filter(|r| r.leg == "fix_out" && r.msg == "D")
        .filter(|r| r.get(6121).and_then(|v| v.parse::<i64>().ok()).is_some_and(|id| opts.skip_orders.contains(&id)))
        .filter_map(|r| r.get(11).map(|c| base(&c).to_string())).collect();
    let of_skipped = |r: &Rec| -> bool {
        match r.leg.as_str() {
            "api_out" => matches!(r.msg.as_str(), "PLACE_ORDER" | "CANCEL_ORDER")
                && r.request["orderId"].as_i64().is_some_and(|id| opts.skip_orders.contains(&id)),
            "fix_out" | "fix_in" if r.conn == "CCP" && matches!(r.msg.as_str(), "D" | "G" | "F" | "8" | "9") =>
                [11, 41].iter().any(|t| r.get(*t).is_some_and(|c| skipped.contains(base(&c)))),
            _ => false,
        }
    };
    let recs: Vec<Rec> = recs.into_iter().filter(|r| !of_skipped(r)).collect();
    at_their_effect(&recs)
}

struct Run<'a> {
    links: &'a mut Links,
    driver: &'a mut dyn Driver,
    opts: &'a Options,
    recs: Vec<Rec>,
    /// The record being replayed.
    at: usize,
    /// ibx's messages of each link already looked at.
    seen: [usize; 3],
    /// ibx's frames of a compared kind not matched yet, per link.
    queue: [VecDeque<Fields>; 3],
    /// ibx's lookups not paired or answered yet (index in ccp_out).
    lookups: Vec<usize>,
    ids: Ids,
    /// Sequence number of the next frame on each link.
    seq_in: [u64; 3],
    /// Historical requests in order: made with no end time.
    no_end: VecDeque<bool>,
    /// New orders of the reference by API order id, and of ibx.
    gw_orders: HashMap<i64, usize>,
    pending_orders: Vec<(Fields, i64, usize)>,
    out: Outcome,
}

/// Replay a scenario through the Rust API client.
pub fn replay(sc: &Scenario, opts: &Options) -> Outcome {
    let mut links = Links::new();
    let mut driver = RustDriver::new(&links);
    run(sc, opts, &mut links, &mut driver)
}

/// Replay a scenario through `driver` on `links`, in the machine zone of
/// the recording (`session::zone_of`) on this thread.
pub fn run(sc: &Scenario, opts: &Options, links: &mut Links, driver: &mut dyn Driver) -> Outcome {
    crate::gateway::set_machine_zone_for_test(Some(super::session::zone_of(&sc.header)));
    let recs = prepare(sc, opts);
    let mut run = Run {
        links, driver, opts, recs, at: 0,
        seen: [0; 3], queue: Default::default(), lookups: Vec::new(), ids: Ids::default(),
        seq_in: [0; 3], no_end: VecDeque::new(), gw_orders: HashMap::new(), pending_orders: Vec::new(),
        out: Outcome::default(),
    };
    run.go();
    // The order id of a callback: its first field, the tenth of an
    // execution (after the request id and the contract).
    let skip = |l: &str| {
        let pos = if l.starts_with("execDetails|") { 10 } else { 1 };
        run.opts.skip_orders.iter().any(|id| l.split('|').nth(pos) == Some(&id.to_string()))
    };
    let keep = |l: &String| !session_notice(l) && !skip(l) && (run.opts.keep)(l);
    // Historical requests with no end time: their end is the time they
    // were made.
    let now: HashSet<String> = run.recs.iter().filter(|r| r.msg == "REQ_HISTORICAL_DATA")
        .filter(|r| r.request["endDateTime"].as_str().unwrap_or("").is_empty())
        .filter_map(|r| r.request["reqId"].as_i64()).map(|id| id.to_string()).collect();
    let line = |l: &String| (run.opts.mask)(&session_values(l, &now));
    run.out.ours = run.out.ours.iter().filter(|l| keep(l)).map(line).collect();
    run.out.theirs = run.out.theirs.iter().filter(|(_, l)| keep(l)).map(|(s, l)| (*s, line(l))).collect();
    run.out
}

impl Run<'_> {
    fn go(&mut self) {
        while self.at < self.recs.len() {
            let r = self.recs[self.at].clone();
            match r.leg.as_str() {
                "api_out" => self.request(&r),
                "api_in" => {
                    let lines = r.callbacks.as_array().into_iter().flatten().filter_map(canonical);
                    self.out.theirs.extend(lines.map(|l| (r.seq, l)));
                }
                "fix_out" => self.reference_sent(&r),
                "fix_in" => self.server_sent(&r),
                _ => {}
            }
            if self.out.frame_error.is_some() {
                return;
            }
            self.at += 1;
        }
        self.settle();
        for link in [Link::Farm, Link::Ccp, Link::Hmds] {
            if let Some(f) = self.queue[link.index()].front() {
                self.out.frame_error = Some(format!(
                    "unexpected frame on {link:?}: ibx sent {} ({} more), the reference did not",
                    to_pipe(f), self.queue[link.index()].len() - 1,
                ));
                return;
            }
        }
    }

    fn request(&mut self, r: &Rec) {
        match r.msg.as_str() {
            "API_PREFIX" | "CLIENT_VERSION" => return,
            "START_API" => {
                let id = r.request["clientId"].as_i64().unwrap_or(0);
                self.links.shared.reference.set_api_client_id(id);
                self.driver.start(id);
                return;
            }
            "REQ_HISTORICAL_DATA" => self.no_end.push_back(r.request["endDateTime"].as_str().unwrap_or("").is_empty()),
            "REQ_HEAD_TIMESTAMP" => self.no_end.push_back(false),
            _ => {}
        }
        if !self.driver.request(self.links, r) {
            self.out.not_made.push((r.seq, r.msg.clone()));
        }
        self.settle();
    }

    /// The engine settles, ibx's new frames are taken, the callbacks
    /// dispatched; ibx's own lookups are answered.
    fn settle(&mut self) {
        loop {
            self.links.step();
            let lines = self.driver.dispatch();
            self.out.ours.extend(lines);
            self.links.step();
            self.absorb();
            if !self.answer_lookups() {
                break;
            }
        }
    }

    fn sent(&self, link: Link) -> &[Fields] {
        match link {
            Link::Farm => &self.links.farm_out,
            Link::Ccp => &self.links.ccp_out,
            Link::Hmds => &self.links.hmds_out,
        }
    }

    /// ibx's new frames: those of a compared kind wait for the reference's.
    fn absorb(&mut self) {
        for link in [Link::Farm, Link::Ccp, Link::Hmds] {
            let k = link.index();
            let new: Vec<(usize, Fields)> = self.sent(link)[self.seen[k]..].iter().cloned()
                .enumerate().map(|(i, f)| (self.seen[k] + i, f)).collect();
            self.seen[k] += new.len();
            for (i, f) in new {
                let kd = kind(link, &f);
                if kd.is_some_and(|kd| self.opts.compare.contains(&kd)) && !(self.opts.skip_frame)(&f) {
                    self.queue[k].push_back(f.clone());
                }
                if kd == Some(LOOKUP) {
                    self.lookups.push(i);
                }
            }
        }
    }

    /// ibx's lookups not answered yet, answered at once: with the reply to
    /// the reference's lookup written the same way (the same fields and
    /// label, its id aside), else with a recorded reply for the same
    /// contract (ibx may ask what the reference had already, or ask it at
    /// another time). With neither, paired with a lookup of the reference
    /// for the same contract, whose answer is sent at its recorded point.
    /// True when a reply was sent.
    fn answer_lookups(&mut self) -> bool {
        let mut sent = false;
        let pending = std::mem::take(&mut self.lookups);
        for i in pending {
            let ours = self.links.ccp_out[i].clone();
            let Some(our_id) = tag(&ours, 320).map(str::to_string) else { continue };
            if self.ids.lookup_taken.contains(&our_id) { continue; }
            self.ids.lookup_taken.insert(our_id.clone());
            let key = lookup_key(&ours);
            let same = self.recs.iter().chain(&self.opts.replies)
                .filter(|r| r.leg == "fix_out" && r.conn == "CCP" && lookup_key(&r.fields()) == key)
                .filter_map(|r| r.get(320)).find(|id| !self.ids.lookup_used.contains(id));
            let reply_to = |id: &str| self.recs.iter().chain(&self.opts.replies)
                .find(|r| r.leg == "fix_in" && r.conn == "CCP" && r.get(320).as_deref() == Some(id)).cloned();
            let reply = match same.as_deref().and_then(reply_to) {
                Some(d) => {
                    self.ids.lookup_used.insert(same.unwrap());
                    Some(d)
                }
                None => self.recs.iter().chain(&self.opts.replies)
                    .find(|r| r.is("fix_in", "CCP", "d") && same_contract(&r.fields(), &ours)).cloned(),
            };
            match reply {
                Some(d) => {
                    let mut f = d.fields();
                    for (t, v) in f.iter_mut() { if *t == 320 { *v = our_id.clone(); } }
                    let f = self.renumber(Link::Ccp, f);
                    self.send(Link::Ccp, &rebuild_text(&f));
                    sent = true;
                }
                None => {
                    let gw = self.recs.iter().find(|r| r.is("fix_out", "CCP", "c") && same_contract(&r.fields(), &ours)
                        && r.get(320).is_some_and(|id| !self.ids.lookup.contains_key(&id)));
                    if let Some(gw_id) = gw.and_then(|r| r.get(320)) {
                        self.ids.lookup.insert(gw_id, our_id);
                    }
                }
            }
        }
        sent
    }

    /// A frame the reference sent.
    fn reference_sent(&mut self, r: &Rec) {
        let Some(link) = Link::of(&r.conn) else { return };
        let gw = r.fields();
        let Some(kd) = kind(link, &gw) else { return };
        if self.before_the_scenario(kd, &gw) {
            return;
        }
        if self.opts.compare.contains(&kd) && !(self.opts.skip_frame)(&gw) {
            let Some(ours) = self.queue[link.index()].pop_front() else {
                self.out.frame_error = Some(format!(
                    "seq {} {}: ibx did not send {}", r.seq, r.conn, normalised(kd, &gw, self.no_end.front() == Some(&true)),
                ));
                return;
            };
            let mask_end = kd == HISTORICAL && msg_type(&gw) == "W" && self.no_end.pop_front().unwrap_or(false);
            let (mut a, mut b) = (ours.clone(), gw.clone());
            (self.opts.frame_mask)(&mut a);
            (self.opts.frame_mask)(&mut b);
            let (a, b) = (normalised(kd, &a, mask_end), normalised(kd, &b, mask_end));
            if a != b {
                self.out.frame_error = Some(format!("seq {} {}: frame differs\n  ours:      {a}\n  reference: {b}", r.seq, r.conn));
                return;
            }
            self.out.frames_compared += 1;
            self.learn(kd, &gw, &ours);
        } else {
            self.pair(link, kd, &gw);
        }
    }

    /// A frame that ends what the reference made before the recording: an
    /// unsubscribe of farm entries or a cancel of an account subscription
    /// no frame of the scenario made.
    fn before_the_scenario(&self, kd: &str, gw: &Fields) -> bool {
        let made_here = |id: &str| self.recs.iter().any(|r| r.leg == "fix_out" && {
            let f = r.fields();
            match kd {
                MARKET_DATA => entries(&f).iter().any(|e| e.0 == id && e.1 == "1"),
                _ => tag(&f, 6529) == Some(id) && tag(&f, 6036) == Some("1"),
            }
        });
        match kd {
            MARKET_DATA => tag(gw, 263) == Some("2") && entries(gw).iter().all(|e| !made_here(&e.0)),
            SUBSCRIPTION => tag(gw, 6036) == Some("2") && tag(gw, 6529).is_some_and(|id| !made_here(id)),
            _ => false,
        }
    }

    /// The ids of a frame of ibx and the reference's same frame.
    fn learn(&mut self, kd: &str, gw: &Fields, ours: &Fields) {
        match kd {
            ORDER if msg_type(gw) == "D" => {
                if let (Some(a), Some(b)) = (tag(gw, 11), tag(ours, 11)) {
                    self.ids.clord.entry(a.to_string()).or_insert_with(|| b.to_string());
                    self.ids.order.entry(base(a).to_string()).or_insert_with(|| base(b).to_string());
                }
            }
            MARKET_DATA => {
                for ((g, ..), (o, ..)) in entries(gw).into_iter().zip(entries(ours)) {
                    self.ids.farm_taken.insert(o.clone());
                    self.ids.farm.entry(g).or_insert(o);
                }
            }
            LOOKUP => {
                if let (Some(a), Some(b)) = (tag(gw, 320), tag(ours, 320)) {
                    self.ids.lookup.insert(a.to_string(), b.to_string());
                    self.ids.lookup_taken.insert(b.to_string());
                }
            }
            SUBSCRIPTION => {
                if let (Some(a), Some(b)) = (tag(gw, 6529), tag(ours, 6529)) && !self.ids.subscription.iter().any(|(g, _)| g == a) {
                    self.ids.subscription.push((a.to_string(), b.to_string()));
                }
            }
            HISTORICAL => {
                if let (Some(a), Some(b)) = (query_id(gw), query_id(ours)) && !self.ids.query.iter().any(|(g, _)| *g == a) {
                    self.ids.query.push((a, b));
                }
            }
            _ => {}
        }
    }

    /// A frame of a kind not compared: paired with ibx's frame of the same
    /// thing, for its ids.
    fn pair(&mut self, link: Link, kd: &str, gw: &Fields) {
        match kd {
            ORDER if msg_type(gw) == "D" => {
                let Some(id) = tag(gw, 6121).and_then(|v| v.parse::<i64>().ok()) else { return };
                let rank = { let n = self.gw_orders.entry(id).or_default(); *n += 1; *n };
                self.pending_orders.push((gw.clone(), id, rank));
                self.pair_orders();
            }
            MARKET_DATA => self.pair_farm(gw),
            LOOKUP => {
                let Some(gw_id) = tag(gw, 320).map(str::to_string) else { return };
                if self.ids.lookup.contains_key(&gw_id) { return; }
                let found = self.links.ccp_out.iter().filter(|f| msg_type(f) == "c")
                    .find(|f| same_contract(f, gw) && tag(f, 320).is_some_and(|id| !self.ids.lookup_taken.contains(id)))
                    .and_then(|f| tag(f, 320)).map(str::to_string);
                if let Some(id) = found {
                    self.ids.lookup_taken.insert(id.clone());
                    self.ids.lookup.insert(gw_id, id);
                }
            }
            SUBSCRIPTION | HISTORICAL => {
                // The frame of ibx of the same rank.
                let same = |f: &Fields| msg_type(f) == msg_type(gw) && tag(f, 6040) == tag(gw, 6040) && tag(f, 6036) == tag(gw, 6036);
                let rank = self.recs[..self.at].iter().filter(|r| Link::of(&r.conn) == Some(link) && r.leg == "fix_out")
                    .filter(|r| same(&r.fields())).count();
                if let Some(ours) = self.sent(link).iter().filter(|f| same(f)).nth(rank.saturating_sub(1)).cloned() {
                    self.learn(kd, gw, &ours);
                }
            }
            _ => {}
        }
    }

    /// The reference's new orders paired with ibx's of the same API order
    /// id and rank, once ibx sent them.
    fn pair_orders(&mut self) {
        let pending = std::mem::take(&mut self.pending_orders);
        for (gw, id, rank) in pending {
            let ours = self.links.ccp_out.iter()
                .filter(|f| msg_type(f) == "D" && tag(f, 6121) == Some(id.to_string().as_str()))
                .nth(rank - 1).cloned();
            match ours {
                Some(o) => self.learn(ORDER, &gw, &o),
                None => self.pending_orders.push((gw, id, rank)),
            }
        }
    }

    /// The reference's subscribe entries paired with ibx's entries of the
    /// same contract, exchange and type.
    fn pair_farm(&mut self, theirs: &Fields) {
        for (gw_id, action, con_id, exch, kd) in entries(theirs) {
            if action != "1" || self.ids.farm.contains_key(&gw_id) { continue; }
            let found = self.links.farm_out.iter().filter(|f| msg_type(f) == "V").flat_map(entries)
                .find(|(id, a, c, e, k)| a == "1" && *c == con_id && *e == exch && *k == kd && !self.ids.farm_taken.contains(id));
            if let Some((id, ..)) = found {
                self.ids.farm_taken.insert(id.clone());
                self.ids.farm.insert(gw_id, id);
            }
        }
    }

    /// The sequence number of the link in place of the recorded one, so
    /// the frames ibx gets follow each other with no gap.
    fn renumber(&mut self, link: Link, mut f: Fields) -> Fields {
        let k = link.index();
        for (t, v) in f.iter_mut() {
            if *t == 34 {
                if self.seq_in[k] == 0 {
                    self.seq_in[k] = v.parse().unwrap_or(1);
                }
                *v = format!("{:0width$}", self.seq_in[k], width = v.len());
                self.seq_in[k] += 1;
            }
        }
        f
    }

    fn send(&mut self, link: Link, raw: &[u8]) {
        self.links.give_bad_copies(raw, link);
        match link {
            Link::Farm => self.links.farm.send_raw(raw),
            Link::Ccp => self.links.ccp.send_raw(raw),
            Link::Hmds => unreachable!("the historical link takes messages (send_hmds)"),
        }
        self.settle();
    }

    /// A frame the servers sent, with ibx's ids. The session frames
    /// (heartbeats, test requests) are left out: the replay does not run
    /// the links' liveness checks.
    fn server_sent(&mut self, r: &Rec) {
        if matches!(r.msg.as_str(), "0" | "1") {
            return;
        }
        let Some(link) = Link::of(&r.conn) else {
            self.out.unsent.push((r.seq, r.conn.clone()));
            return;
        };
        match link {
            Link::Ccp if r.raw.starts_with(b"8=FIX") => {
                self.pair_orders();
                let mut f = r.fields();
                let mut known = true;
                for (t, v) in f.iter_mut() {
                    if matches!(t, 11 | 41) && let Some(ours) = self.ids.clord.get(v.as_str()) {
                        *v = ours.clone();
                        continue;
                    }
                    if matches!(t, 11 | 41 | 6107 | 583) && !v.is_empty() {
                        match self.ids.order.get(base(v)) {
                            Some(ours) => *v = v.replacen(base(v), ours, 1),
                            None if matches!(t, 11 | 41) => known = false,
                            None => {}
                        }
                    }
                    if *t == 320 {
                        match self.ids.lookup.get(v.as_str()) {
                            Some(ours) => *v = ours.clone(),
                            None => known = false,
                        }
                    }
                }
                if known {
                    let f = self.renumber(link, f);
                    self.send(link, &rebuild_text(&f));
                } else {
                    self.out.unsent.push((r.seq, r.conn.clone()));
                }
            }
            Link::Ccp => {
                // A binary frame: the subscription id is a text field, the
                // last one of an end marker (35=EB|6529={id}).
                let body = self.ids.subscription.iter().fold(binary_body(&r.raw), |b, (gw, ours)| {
                    let b = b.replace(&format!("6529={gw}\x01"), &format!("6529={ours}\x01"));
                    match b.strip_suffix(&format!("6529={gw}")) {
                        Some(head) => format!("{head}6529={ours}"),
                        None => b,
                    }
                });
                self.send(link, &rebuild_binary(&r.raw, &body));
            }
            Link::Farm => {
                for o in self.recs[..self.at].iter().filter(|o| o.is("fix_out", "usfarm", "V")).cloned().collect::<Vec<_>>() {
                    self.pair_farm(&o.fields());
                }
                match r.msg.as_str() {
                    "Q" => {
                        let body = binary_body(&r.raw);
                        let mut parts: Vec<String> = body.split(',').map(str::to_string).collect();
                        match parts.get(1).and_then(|id| self.ids.farm.get(id)) {
                            Some(id) => {
                                parts[1] = id.clone();
                                self.send(link, &rebuild_binary(&r.raw, &parts.join(",")));
                            }
                            None => self.out.unsent.push((r.seq, r.conn.clone())),
                        }
                    }
                    "3" => {
                        let mut f = r.fields();
                        let ours = tag(&f, 262).and_then(|v| self.ids.farm.get(v).cloned());
                        match ours {
                            Some(id) => {
                                for (t, v) in f.iter_mut() { if *t == 262 { *v = id.clone(); } }
                                let f = self.renumber(link, f);
                                self.send(link, &rebuild_text(&f));
                            }
                            None => self.out.unsent.push((r.seq, r.conn.clone())),
                        }
                    }
                    _ => self.send(link, &r.raw),
                }
            }
            Link::Hmds if r.raw.starts_with(b"8=O") => {
                // A binary frame (live bar updates): the ids it holds are
                // the farm's own.
                self.links.give_bad_copies(&r.raw, Link::Hmds);
                self.links.hmds.send_raw(&r.raw);
                self.settle();
            }
            Link::Hmds => {
                let msgs = if r.raw.starts_with(b"8=FIXCOMP") {
                    crate::protocol::fixcomp::fixcomp_decompress(&r.raw).unwrap_or_default()
                } else {
                    vec![r.raw.clone()]
                };
                for m in msgs {
                    let f: Fields = crate::test_support::parse_fields(&m).into_iter().map(|(t, v)| {
                        (t, self.ids.query.iter().fold(v, |v, (gw, ours)| {
                            v.replace(&format!("<id>{gw};;"), &format!("<id>{ours};;"))
                                .replace(&format!("<id>{gw}</id>"), &format!("<id>{ours}</id>"))
                        }))
                    }).collect();
                    self.links.give_bad_hmds_copies(&f);
                    send_hmds(&mut self.links.hmds, &f);
                    self.settle();
                }
            }
        }
    }
}
