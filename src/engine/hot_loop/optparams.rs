//! Option chain parameters on the auth connection (ibx#440), as the
//! reference runs them (`feature.derivatives`): per underlying, the
//! derivative query (6040=5, kept per symbol until the auth link drops),
//! then its legs one at a time: a definition lookup per stock, future,
//! currency or index leg, a chain query (6040=138) per option leg. The
//! chain answers stay with the underlying, so a later request is answered
//! without the legs again. No timeout, as in the reference.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::bridge::SharedState;
use crate::config::chrono_free_timestamp;
use crate::control::optparams::{self, ChainAnswer, ChainKey, UnderlyingAnswer};
use crate::protocol::connection::Connection;
use crate::protocol::fix;
use crate::types::ReqId;

use super::HeartbeatState;

/// Request numbers of the leg lookups: a range of their own, below the
/// option calculation lookups.
pub(crate) const OPTPARAMS_LOOKUP_FIRST_ID: u32 = 0xB000_0000;
pub(crate) const OPTPARAMS_LOOKUP_IDS: u32 = 0x1000_0000;
/// Underlyings kept before the idle ones go (`jutils.thread.c`).
const KEPT_UNDERLYINGS: usize = 100;
/// An underlying idle this long can go once more than 100 are kept.
const IDLE_UNDERLYING: Duration = Duration::from_secs(60);

/// An underlying of the reference (`feature.derivatives.T`): conId,
/// symbol in upper case, type and the futures option exchange.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Underlying {
    pub con_id: i64,
    pub symbol: String,
    pub sec_type: String,
    pub exchange: String,
}

impl Underlying {
    /// The derivative type whose leg must have data (`T.g()`).
    fn derivative(&self) -> &'static str {
        if self.sec_type == "FUT" { "FOP" } else { "OPT" }
    }

    /// The exchange of the default chain query (`T.l()`): a future's only.
    fn query_exchange(&self) -> &str {
        if self.sec_type == "FUT" { &self.exchange } else { "" }
    }

    /// The chain key and query of a leg on an exchange (`T.a(String)`,
    /// `feature.derivatives.D.b()`/`c()`).
    fn chain_key(&self, leg: &str, exchange: &str) -> (ChainKey, ChainKey) {
        let fop_of_other = leg == "FOP" && self.sec_type != "FUT";
        let query = ChainKey {
            con_id: self.con_id,
            symbol: self.symbol.clone(),
            sec_type: if fop_of_other { "FUT".into() } else { self.sec_type.clone() },
            exchange: exchange.to_string(),
            by_6457: fop_of_other,
        };
        let key = if exchange.is_empty() {
            ChainKey { con_id: self.con_id, symbol: self.symbol.clone(), sec_type: self.sec_type.clone(), exchange: String::new(), by_6457: false }
        } else {
            ChainKey { con_id: self.con_id, symbol: self.symbol.clone(), sec_type: "FUT".into(), exchange: exchange.to_string(), by_6457: self.sec_type != "FUT" }
        };
        (key, query)
    }
}

#[derive(Debug, Clone, PartialEq)]
enum LegStatus {
    Init,
    Pending,
    Filled,
}

#[derive(Debug, Clone)]
struct Leg {
    kind: &'static str,
    status: LegStatus,
    /// Futures option exchanges of this run (6589).
    exchanges: Vec<String>,
}

#[derive(Debug)]
struct Entry {
    /// A run is going on: a new request waits for it.
    requested: bool,
    clients: Vec<ReqId>,
    legs: Vec<Leg>,
    /// Chain answers per option leg, kept.
    answers: HashMap<&'static str, Vec<ChainAnswer>>,
    /// Error text of a leg of this run.
    failure: Option<String>,
    last_used: Instant,
}

/// State of the option chain requests.
#[derive(Debug, Default)]
pub(crate) struct OptParams {
    entries: HashMap<Underlying, Entry>,
    /// Derivative answers by symbol, until the auth link drops
    /// (`jclient.mw`).
    by_symbol: HashMap<String, UnderlyingAnswer>,
    /// Derivative queries sent, by symbol, and the underlyings waiting.
    asked: HashMap<String, Vec<Underlying>>,
    /// Chain queries sent and the legs waiting (`jutils.bX`).
    chains: HashMap<ChainKey, Vec<(Underlying, &'static str)>>,
    lookups: HashMap<u32, (Underlying, &'static str)>,
    next_lookup: u32,
}

fn send_u(conn: &mut Option<Connection>, connected: bool, fields: &[(u32, String)], hb: &mut HeartbeatState) -> bool {
    let Some(c) = conn.as_mut().filter(|_| connected) else { return false };
    let ts = chrono_free_timestamp();
    let mut all: Vec<(u32, &str)> = vec![(fix::TAG_MSG_TYPE, "U"), (fix::TAG_SENDING_TIME, &ts)];
    all.extend(fields.iter().map(|(t, v)| (*t, v.as_str())));
    let ok = c.send_fix(&all).is_ok();
    hb.last_ccp_sent = Instant::now();
    ok
}

impl OptParams {
    /// A checked request. It joins a run in progress for the same
    /// underlying; else a run starts: the derivative query (none for a
    /// future, whose only leg is the futures options of its exchange).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn request(
        &mut self, req_id: ReqId, symbol: &str, fut_fop_exchange: &str, sec_type: &str, con_id: i64,
        conn: &mut Option<Connection>, connected: bool, hb: &mut HeartbeatState, shared: &SharedState,
    ) {
        let now = Instant::now();
        let under = Underlying {
            con_id,
            symbol: symbol.to_uppercase(),
            sec_type: sec_type.to_string(),
            exchange: fut_fop_exchange.trim().to_string(),
        };
        if !self.entries.contains_key(&under) {
            self.evict(now);
        }
        let entry = self.entries.entry(under.clone()).or_insert_with(|| Entry {
            requested: false, clients: Vec::new(), legs: Vec::new(), answers: HashMap::new(), failure: None, last_used: now,
        });
        entry.last_used = now;
        entry.clients.push(req_id);
        if entry.requested {
            log::info!("Option chain req_id={}: joins the request in progress for {:?}", req_id, under);
            return;
        }
        entry.requested = true;
        entry.failure = None;
        log::info!("Option chain req_id={}: {:?}", req_id, under);
        if under.sec_type == "FUT" {
            let exchanges = vec![under.exchange.clone()];
            Self::add_leg(entry, "FOP", exchanges);
            self.next_leg(&under, conn, connected, hb, shared);
            return;
        }
        if self.by_symbol.get(&under.symbol).is_some_and(|a| a.serves(con_id, &under.sec_type)) {
            let answer = self.by_symbol[&under.symbol].clone();
            self.underlying_known(&under, &answer, conn, connected, hb, shared);
            return;
        }
        if let Some(waiting) = self.asked.get_mut(&under.symbol) {
            waiting.push(under);
            return;
        }
        let query = optparams::underlying_query(&under.symbol, &under.sec_type, con_id);
        if send_u(conn, connected, &query, hb) {
            self.asked.insert(under.symbol.clone(), vec![under]);
        } else {
            log::warn!("Option chain req_id={}: derivative query not sent", req_id);
            self.fail(&under, optparams::SEND_FAILED, shared);
        }
    }

    /// The underlyings over 100 that are idle for 60 s go, the oldest
    /// first.
    fn evict(&mut self, now: Instant) {
        if self.entries.len() < KEPT_UNDERLYINGS {
            return;
        }
        let mut idle: Vec<(Instant, Underlying)> = self.entries.iter()
            .filter(|(_, e)| e.clients.is_empty() && now.duration_since(e.last_used) > IDLE_UNDERLYING)
            .map(|(k, e)| (e.last_used, k.clone()))
            .collect();
        idle.sort_by_key(|(t, _)| *t);
        for (_, key) in idle {
            if self.entries.len() < KEPT_UNDERLYINGS {
                break;
            }
            self.entries.remove(&key);
        }
    }

    /// A leg joins the underlying unless it is already filled
    /// (`feature.derivatives.a.b(eh)`).
    fn add_leg(entry: &mut Entry, kind: &'static str, exchanges: Vec<String>) {
        match entry.legs.iter_mut().find(|l| l.kind == kind) {
            Some(leg) => {
                if leg.status != LegStatus::Filled {
                    leg.status = LegStatus::Init;
                }
                leg.exchanges = exchanges;
            }
            None => entry.legs.push(Leg { kind, status: LegStatus::Init, exchanges }),
        }
    }

    /// A derivative answer (6040=5): kept by symbol; the underlyings that
    /// wait for it go on.
    pub(crate) fn underlying_reply(
        &mut self, msg: &[u8], conn: &mut Option<Connection>, connected: bool, hb: &mut HeartbeatState, shared: &SharedState,
    ) {
        let Some(answer) = optparams::parse_underlying_answer(msg) else { return };
        let waiting = self.asked.remove(&answer.symbol).unwrap_or_default();
        self.by_symbol.insert(answer.symbol.clone(), answer.clone());
        for under in waiting {
            self.underlying_known(&under, &answer, conn, connected, hb, shared);
        }
    }

    /// The legs of an underlying from the derivative answer; none gives
    /// "No derivatives found".
    fn underlying_known(
        &mut self, under: &Underlying, answer: &UnderlyingAnswer,
        conn: &mut Option<Connection>, connected: bool, hb: &mut HeartbeatState, shared: &SharedState,
    ) {
        let legs = answer.legs(under.con_id);
        let Some(entry) = self.entries.get_mut(under) else { return };
        if legs.is_empty() {
            self.fail(under, optparams::NO_DERIVATIVES_FOUND, shared);
            return;
        }
        for (kind, exchanges) in legs {
            Self::add_leg(entry, kind, exchanges);
        }
        self.next_leg(under, conn, connected, hb, shared);
    }

    /// The next leg of a run, one at a time; the answer once every leg
    /// is filled.
    fn next_leg(
        &mut self, under: &Underlying, conn: &mut Option<Connection>, connected: bool, hb: &mut HeartbeatState, shared: &SharedState,
    ) {
        let Some(entry) = self.entries.get_mut(under) else { return };
        if entry.legs.iter().any(|l| l.status == LegStatus::Pending) {
            return;
        }
        let Some(i) = entry.legs.iter().position(|l| l.status == LegStatus::Init) else {
            self.finish(under, shared);
            return;
        };
        entry.legs[i].status = LegStatus::Pending;
        let leg = entry.legs[i].clone();
        if leg.kind == "OPT" || leg.kind == "FOP" {
            self.send_chain_leg(under, &leg, conn, connected, hb, shared);
        } else {
            self.send_lookup_leg(under, leg.kind, conn, connected, hb);
        }
    }

    /// The definition lookup of a stock, future, currency or index leg
    /// (`jclient.ph`): symbol, conId, type, and the futures option
    /// exchange except for a future. Not sent: the leg waits, as in the
    /// reference.
    fn send_lookup_leg(
        &mut self, under: &Underlying, kind: &'static str, conn: &mut Option<Connection>, connected: bool, hb: &mut HeartbeatState,
    ) {
        let number = OPTPARAMS_LOOKUP_FIRST_ID + self.next_lookup % OPTPARAMS_LOOKUP_IDS;
        self.next_lookup = self.next_lookup.wrapping_add(1);
        self.lookups.insert(number, (under.clone(), kind));
        let Some(c) = conn.as_mut().filter(|_| connected) else {
            log::warn!("Option chain leg {} of {:?} not sent: auth connection down", kind, under);
            return;
        };
        let ts = chrono_free_timestamp();
        let id = format!("{}{}", crate::control::contracts::SECDEF_BY_SYMBOL_NAME, number);
        let con_id = under.con_id.to_string();
        let fix_type = if kind == "STK" { "CS" } else { kind };
        let exchange = if kind == "FUT" { "" } else if under.exchange == "SMART" { "BEST" } else { under.exchange.as_str() };
        let mut fields: Vec<(u32, &str)> = vec![
            (fix::TAG_MSG_TYPE, "c"),
            (fix::TAG_SENDING_TIME, &ts),
            (crate::control::contracts::TAG_SECURITY_REQ_ID, &id),
            (crate::control::contracts::TAG_SECURITY_REQ_TYPE, "2"),
            (55, &under.symbol),
            (6457, &con_id),
            (167, fix_type),
        ];
        if !exchange.is_empty() {
            fields.push((100, exchange));
        }
        let _ = c.send_fix(&fields);
        hb.last_ccp_sent = Instant::now();
    }

    /// The chain queries of an option leg: one, or one per futures option
    /// exchange (without FORECASTX). A query already sent for the same
    /// key is not sent again. The leg fails when no query went out.
    fn send_chain_leg(
        &mut self, under: &Underlying, leg: &Leg, conn: &mut Option<Connection>, connected: bool, hb: &mut HeartbeatState, shared: &SharedState,
    ) {
        let exchanges: Vec<String> = if leg.kind == "FOP" && !leg.exchanges.is_empty() {
            leg.exchanges.iter().filter(|e| *e != "FORECASTX").cloned().collect()
        } else {
            vec![under.query_exchange().to_string()]
        };
        let mut waiting = 0;
        let mut failures: Vec<&str> = Vec::new();
        for exchange in exchanges {
            let (key, query) = under.chain_key(leg.kind, &exchange);
            if let Some(w) = self.chains.get_mut(&key) {
                w.push((under.clone(), leg.kind));
                waiting += 1;
                continue;
            }
            if send_u(conn, connected, &optparams::chain_query(&query), hb) {
                self.chains.insert(key, vec![(under.clone(), leg.kind)]);
                waiting += 1;
            } else {
                failures.push(optparams::SEND_FAILED);
            }
        }
        if waiting == 0 {
            log::warn!("Option chain leg {} of {:?} not sent", leg.kind, under);
            if let Some(entry) = self.entries.get_mut(under) {
                if let Some(l) = entry.legs.iter_mut().find(|l| l.kind == leg.kind) {
                    l.status = LegStatus::Filled;
                }
                entry.failure = Some(failures.join("\n"));
            }
            self.next_leg(under, conn, connected, hb, shared);
        }
    }

    /// The definition answer of a leg lookup; false when the reply is for
    /// another lookup.
    pub(crate) fn leg_reply(
        &mut self, request_id: &str, conn: &mut Option<Connection>, connected: bool, hb: &mut HeartbeatState, shared: &SharedState,
    ) -> bool {
        let Some(number) = crate::control::contracts::secdef_request_number(request_id) else { return false };
        let Ok(number) = u32::try_from(number) else { return false };
        let Some((under, kind)) = self.lookups.remove(&number) else { return false };
        if let Some(l) = self.entries.get_mut(&under)
            .and_then(|e| e.legs.iter_mut().find(|l| l.kind == kind && l.status == LegStatus::Pending)) {
            l.status = LegStatus::Filled;
        }
        self.next_leg(&under, conn, connected, hb, shared);
        true
    }

    /// A chain answer (6040=139), matched to its query by key: kept with
    /// each underlying that waits for it; its leg is filled with the first
    /// answer (the rows are built from every answer kept).
    pub(crate) fn chain_reply(
        &mut self, msg: &[u8], conn: &mut Option<Connection>, connected: bool, hb: &mut HeartbeatState, shared: &SharedState,
    ) {
        let Some(answer) = optparams::parse_chain_answer(msg) else { return };
        let Some(waiting) = self.chains.remove(&answer.key) else {
            log::warn!("Option chain answer for {:?} matches no query", answer.key);
            return;
        };
        let mut go_on: Vec<Underlying> = Vec::new();
        for (under, kind) in waiting {
            let Some(entry) = self.entries.get_mut(&under) else { continue };
            entry.answers.entry(kind).or_default().push(answer.clone());
            if let Some(l) = entry.legs.iter_mut().find(|l| l.kind == kind && l.status == LegStatus::Pending) {
                l.status = LegStatus::Filled;
                if !go_on.contains(&under) {
                    go_on.push(under);
                }
            }
        }
        for under in go_on {
            self.next_leg(&under, conn, connected, hb, shared);
        }
    }

    /// Every leg filled: the rows of each waiting request, then its end;
    /// or 322 when the option leg (or a futures option leg) has no data
    /// (`feature.derivatives.l.a(boolean)`).
    fn finish(&mut self, under: &Underlying, shared: &SharedState) {
        let Some(entry) = self.entries.get_mut(under) else { return };
        let missing = |kind: &str| entry.legs.iter().any(|l| l.kind == kind) && !entry.answers.contains_key(kind);
        if missing(under.derivative()) || missing("FOP") {
            let text = entry.failure.clone().unwrap_or_else(|| optparams::NO_DERIVATIVES_RETURNED.to_string());
            self.fail(under, &text, shared);
            return;
        }
        // Option rows when the request has no futures option exchange,
        // else futures option rows (`jextend.cp.a(s)`).
        let kind = if under.exchange.is_empty() { "OPT" } else { "FOP" };
        let (no_magnifier_fix, island_to_nasdaq) = shared.reference.option_chain_features();
        let rows = optparams::chain_rows(entry.answers.get(kind).map(Vec::as_slice).unwrap_or(&[]), no_magnifier_fix, island_to_nasdaq);
        for req_id in entry.clients.drain(..) {
            log::info!("Option chain req_id={}: {} rows", req_id, rows.len());
            shared.reference.push_option_chains(req_id, rows.clone());
        }
        entry.requested = false;
        entry.failure = None;
        entry.last_used = Instant::now();
    }

    /// 322 with `text` to every request waiting on the underlying.
    fn fail(&mut self, under: &Underlying, text: &str, shared: &SharedState) {
        let Some(entry) = self.entries.get_mut(under) else { return };
        let (code, message) = optparams::processing_error(text);
        for req_id in entry.clients.drain(..) {
            shared.reference.push_historical_error(req_id, code as i32, message.clone());
        }
        entry.requested = false;
        entry.failure = None;
        entry.last_used = Instant::now();
    }

    /// The auth link dropped: the derivative answers are forgotten
    /// (`jclient.mw.a()`); the chain answers stay.
    pub(crate) fn connection_lost(&mut self) {
        self.by_symbol.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    fn socket_pair() -> (crate::protocol::connection::MemTransport, crate::protocol::connection::MemTransport) {
        let (client, server) = crate::protocol::connection::mem_pair();
        (client, server)
    }

    /// Every message written to `server`, without the framing, sequence
    /// and time fields.
    fn sent(server: &mut crate::protocol::connection::MemTransport) -> Vec<String> {
        use std::io::Read;
        server.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        while let Ok(n) = server.read(&mut chunk) {
            if n == 0 { break; }
            buf.extend_from_slice(&chunk[..n]);
        }
        strip(&String::from_utf8_lossy(&buf))
    }

    fn strip(text: &str) -> Vec<String> {
        text.split("8=FIX.4.1\x01").filter(|m| !m.is_empty()).map(|m| {
            m.split('\x01')
                .filter(|f| !f.is_empty())
                .filter(|f| !["9=", "34=", "43=", "52=", "10="].iter().any(|p| f.starts_with(p)))
                .collect::<Vec<_>>().join("|")
        }).collect()
    }

    struct Record {
        leg: String,
        conn: String,
        raw: Vec<u8>,
        msg_name: String,
        body: Vec<u8>,
    }

    fn scenario(name: &str) -> Vec<Record> {
        let path = format!("{}/tests/fixtures/gw1040/scenarios/{}", env!("CARGO_MANIFEST_DIR"), name);
        let text = std::fs::read_to_string(&path).unwrap();
        let b64 = |v: &serde_json::Value| base64::engine::general_purpose::STANDARD.decode(v.as_str().unwrap_or("")).unwrap();
        text.lines().skip(1).map(|line| {
            let rec: serde_json::Value = serde_json::from_str(line).unwrap();
            Record {
                leg: rec["leg"].as_str().unwrap_or("").into(),
                conn: rec["conn"].as_str().unwrap_or("").into(),
                raw: b64(&rec["raw_b64"]),
                msg_name: rec["msg_name"].as_str().unwrap_or("").into(),
                body: b64(&rec["body_b64"]),
            }
        }).collect()
    }

    fn varint(b: &[u8], i: &mut usize) -> u64 {
        let (mut r, mut s) = (0u64, 0);
        loop {
            let c = b[*i];
            *i += 1;
            r |= ((c & 0x7f) as u64) << s;
            s += 7;
            if c & 0x80 == 0 { return r; }
        }
    }

    /// The API rows of a recorded answer: the protobuf
    /// SecDefOptParameter messages (reqId 1, exchange 2, conId 3, trading
    /// class 4, multiplier 5, expirations 6, packed strikes 7).
    fn recorded_rows(records: &[Record]) -> (Vec<optparams::OptionChain>, usize) {
        let mut rows = Vec::new();
        let mut ends = 0;
        for r in records.iter().filter(|r| r.leg == "api_in") {
            match r.msg_name.as_str() {
                "SECURITY_DEFINITION_OPTION_PARAMETER" => {
                    let b = &r.body;
                    let mut row = optparams::OptionChain {
                        exchange: String::new(), underlying_con_id: 0, trading_class: String::new(),
                        multiplier: String::new(), expirations: Vec::new(), strikes: Vec::new(),
                    };
                    let mut i = 0;
                    while i < b.len() {
                        let key = varint(b, &mut i);
                        let (field, wire) = (key >> 3, key & 7);
                        if wire == 0 {
                            let v = varint(b, &mut i);
                            if field == 3 { row.underlying_con_id = v as i64; }
                            continue;
                        }
                        let n = varint(b, &mut i) as usize;
                        let bytes = &b[i..i + n];
                        i += n;
                        let s = || String::from_utf8_lossy(bytes).into_owned();
                        match field {
                            2 => row.exchange = s(),
                            4 => row.trading_class = s(),
                            5 => row.multiplier = s(),
                            6 => row.expirations.push(s()),
                            7 => row.strikes.extend(bytes.chunks(8).map(|c| f64::from_le_bytes(c.try_into().unwrap()))),
                            _ => {}
                        }
                    }
                    rows.push(row);
                }
                "SECURITY_DEFINITION_OPTION_PARAMETER_END" => ends += 1,
                _ => {}
            }
        }
        (rows, ends)
    }

    fn ccp_frames<'a>(records: &'a [Record], leg: &'a str) -> impl Iterator<Item = &'a Record> + 'a {
        records.iter().filter(move |r| r.leg == leg && r.conn == "CCP")
    }

    fn is_chain_frame(raw: &[u8]) -> bool {
        let t = String::from_utf8_lossy(raw);
        t.contains("\x016040=5\x01") || t.contains("\x016040=138\x01") || t.contains("\x016040=139\x01") || t.contains("\x0135=c\x01") || t.contains("\x0135=d\x01")
    }

    /// Replay a recorded request: the frames ibx sends must be the
    /// recorded ones (the lookup id aside), and the API rows the recorded
    /// rows, in the same order.
    fn replay(name: &str, symbol: &str, con_id: i64) {
        let all = scenario(name);
        let start = all.iter().position(|r| r.msg_name == "REQ_SEC_DEF_OPT_PARAMS").unwrap();
        let end = all.iter().position(|r| r.msg_name == "SECURITY_DEFINITION_OPTION_PARAMETER_END").unwrap();
        let records = &all[start..=end];
        let shared = SharedState::new();
        shared.reference.set_api_features(crate::control::logon::ApiFeatures::parse("ISLAND2NASDAQ,SECDEFTA"));
        let (client, mut server) = socket_pair();
        let mut conn = Some(Connection::new_mem(client));
        let mut hb = HeartbeatState::new();
        let mut op = OptParams::default();
        op.request(77, symbol, "", "STK", con_id, &mut conn, true, &mut hb, &shared);

        let outs: Vec<String> = ccp_frames(records, "fix_out").filter(|r| is_chain_frame(&r.raw))
            .map(|r| strip(&String::from_utf8_lossy(&r.raw)).remove(0)).collect();
        let ins: Vec<&Record> = ccp_frames(records, "fix_in").filter(|r| is_chain_frame(&r.raw)).collect();
        assert_eq!(outs.len(), 3, "{outs:?}");
        assert_eq!(sent(&mut server), [outs[0].clone()], "the derivative query");
        let recorded_id = outs[1].split('|').find_map(|f| f.strip_prefix("320=")).unwrap().to_string();
        let own_id = format!("FixSecDefReqBySymbol{}", OPTPARAMS_LOOKUP_FIRST_ID);
        for (k, rec) in ins.iter().enumerate() {
            let text = String::from_utf8_lossy(&rec.raw);
            if text.contains("\x0135=d\x01") {
                let reply = text.replace(&recorded_id, &own_id);
                let rid = crate::control::contracts::secdef_response_req_id(reply.as_bytes()).unwrap();
                assert!(op.leg_reply(&rid, &mut conn, true, &mut hb, &shared));
            } else if text.contains("\x016040=5\x01") {
                op.underlying_reply(&rec.raw, &mut conn, true, &mut hb, &shared);
            } else {
                op.chain_reply(&rec.raw, &mut conn, true, &mut hb, &shared);
            }
            let got = sent(&mut server);
            match k {
                0 => assert_eq!(got, [outs[1].replace(&recorded_id, &own_id)], "the stock leg lookup"),
                1 => assert_eq!(got, [outs[2].clone()], "the option leg query after the lookup answer"),
                _ => assert!(got.is_empty(), "{got:?}"),
            }
        }
        let (want, ends) = recorded_rows(records);
        assert_eq!(ends, 1);
        let got = shared.reference.drain_option_chains();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, 77);
        assert_eq!(got[0].1.len(), want.len());
        for (g, w) in got[0].1.iter().zip(&want) {
            assert_eq!(g, w);
        }

        // A second request on the same underlying, later in the session:
        // no query at all, the same rows in the same order (captured
        // 28/09/2026, i192_f4_hist_sectypes).
        op.request(78, symbol, "", "STK", con_id, &mut conn, true, &mut hb, &shared);
        assert!(sent(&mut server).is_empty());
        let again = shared.reference.drain_option_chains();
        assert_eq!(again, [(78, got[0].1.clone())]);
    }

    // ibx#440, captured 28/09/2026 (i192_f2_option_future_lookup): AAPL
    // STK sends the derivative query, the stock leg lookup after its
    // answer, the option leg query after the lookup answer; the answer
    // gives 41 rows: IBUSOPT, then SMART and the 19 exchanges of 6523 for
    // each of the trading classes AAPL and 2AAPL.
    #[test]
    fn aapl_chain_as_the_reference() {
        replay("20260928/option_chain_aapl.jsonl", "AAPL", 265598);
        let (rows, _) = recorded_rows(&scenario("20260928/option_chain_aapl.jsonl"));
        assert_eq!(rows.len(), 41);
        assert_eq!(rows.iter().filter(|r| r.trading_class == "2AAPL").count(), 20);
        let smart = rows.iter().find(|r| r.exchange == "SMART" && r.trading_class == "AAPL").unwrap();
        assert_eq!((smart.expirations.len(), smart.strikes.len()), (25, 127));
    }

    // ibx#440, captured 28/09/2026 (rth_order_types): SPY STK, 40 rows.
    #[test]
    fn spy_chain_as_the_reference() {
        replay("20260928/rth_order_types.jsonl", "SPY", 756733);
    }

    #[test]
    fn a_future_sends_its_futures_option_query_only() {
        let shared = SharedState::new();
        let (client, mut server) = socket_pair();
        let mut conn = Some(Connection::new_mem(client));
        let mut hb = HeartbeatState::new();
        let mut op = OptParams::default();
        op.request(5, "es", "CME", "FUT", 495512563, &mut conn, true, &mut hb, &shared);
        assert_eq!(sent(&mut server), ["35=U|6040=138|55=ES|310=FUT|6346=495512563|6320=1|6994=1|6995=CME"]);
        // A second request joins the one in progress.
        op.request(6, "ES", "CME", "FUT", 495512563, &mut conn, true, &mut hb, &shared);
        assert!(sent(&mut server).is_empty());
        let answer = "35=U|6040=139|55=ES|310=FUT|6346=495512563|6994=1|6995=CME|8009=1|100=CME|6996=1|6058=ES|231=50|6346=495512563|6775=20261218|6997=6000;6100|15=USD|6031=67";
        op.chain_reply(answer.replace('|', "\x01").as_bytes(), &mut conn, true, &mut hb, &shared);
        let got = shared.reference.drain_option_chains();
        assert_eq!(got.len(), 2);
        assert_eq!((got[0].0, got[1].0), (5, 6));
        assert_eq!(got[0].1, [optparams::OptionChain {
            exchange: "CME".into(), underlying_con_id: 495512563, trading_class: "ES".into(), multiplier: "50".into(),
            expirations: vec!["20261218".into()], strikes: vec![6000.0, 6100.0],
        }]);
    }

    // The answers reach the requests through the auth connection handler;
    // the leg lookup answer is not a contract details reply.
    #[test]
    fn answers_through_the_auth_connection_handler() {
        let shared = SharedState::new();
        let mut context = crate::engine::context::Context::new();
        let mut ccp = super::super::ccp::CcpState::new();
        let (client, mut server) = socket_pair();
        let mut conn = Some(Connection::new_mem(client));
        let mut hb = HeartbeatState::new();
        ccp.optparams.request(9, "XYZ", "", "STK", 99, &mut conn, true, &mut hb, &shared);
        assert_eq!(sent(&mut server), ["35=U|6040=5|55=XYZ|310=STK|6457=99|6320=1"]);
        let mut feed = |text: &str, conn: &mut Option<Connection>, hb: &mut HeartbeatState| {
            ccp.process_ccp_message(text.replace('|', "\x01").as_bytes(), conn, &mut context, &shared, &None, hb, "DU1");
        };
        feed("8=FIX.4.1|9=1|35=U|52=20260928-14:11:14|6040=5|55=XYZ|6457=99|6070=OPT;STK;|10=0|", &mut conn, &mut hb);
        let id = format!("FixSecDefReqBySymbol{}", OPTPARAMS_LOOKUP_FIRST_ID);
        assert_eq!(sent(&mut server), [format!("35=c|320={id}|321=2|55=XYZ|6457=99|167=CS")]);
        feed(&format!("8=FIX.4.1|9=1|35=d|52=20260928-14:11:14|320={id}|322=*|323=4|55=XYZ|167=STK|207=BEST|6008=99|15=USD|10=0|"), &mut conn, &mut hb);
        assert_eq!(sent(&mut server), ["35=U|6040=138|55=XYZ|310=STK|6346=99|6320=1|6994=1"]);
        assert!(shared.reference.drain_contract_details().is_empty() && shared.reference.drain_contract_details_end().is_empty());
        feed("8=FIX.4.1|9=1|35=U|52=20260928-14:11:14|6040=139|55=XYZ|310=STK|6346=99|6994=1|8009=1|100=CBOE|6996=1|6058=XYZ|231=100|6346=99|6775=20261016|6997=10|15=USD|6031=32|10=0|", &mut conn, &mut hb);
        let rows = shared.reference.drain_option_chains();
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].0, rows[0].1.len(), rows[0].1[0].exchange.as_str()), (9, 1, "CBOE"));
    }

    #[test]
    fn errors_after_the_checks() {
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let mut op = OptParams::default();
        // The auth connection is down: the derivative query cannot go.
        let mut none: Option<Connection> = None;
        op.request(1, "AAPL", "", "STK", 265598, &mut none, true, &mut hb, &shared);
        let errors = shared.reference.drain_historical_errors();
        assert_eq!(errors, [(1, 322, "Error processing request: Sending message failed".to_string())]);

        // The answer has no row for the conId.
        let (client, mut server) = socket_pair();
        let mut conn = Some(Connection::new_mem(client));
        op.request(2, "AAPL", "", "STK", 1234, &mut conn, true, &mut hb, &shared);
        assert_eq!(sent(&mut server).len(), 1);
        let answer = "35=U|6040=5|55=AAPL|6455=1|55=AAPL|310=STK|6455=1|55=AAPL|6457=265598|6070=OPT;STK;";
        op.underlying_reply(answer.replace('|', "\x01").as_bytes(), &mut conn, true, &mut hb, &shared);
        assert_eq!(shared.reference.drain_historical_errors(),
            [(2, 322, "Error processing request: No derivatives found".to_string())]);

        // A conId of the kept answer: no new query; no option leg listed:
        // the end only.
        let answer = "35=U|6040=5|55=XYZ|6457=99|6070=STK;";
        op.request(3, "XYZ", "", "STK", 99, &mut conn, true, &mut hb, &shared);
        assert_eq!(sent(&mut server), ["35=U|6040=5|55=XYZ|310=STK|6457=99|6320=1"]);
        op.underlying_reply(answer.replace('|', "\x01").as_bytes(), &mut conn, true, &mut hb, &shared);
        let lookup = sent(&mut server);
        assert_eq!(lookup, [format!("35=c|320=FixSecDefReqBySymbol{}|321=2|55=XYZ|6457=99|167=CS", OPTPARAMS_LOOKUP_FIRST_ID)]);
        assert!(op.leg_reply(&format!("FixSecDefReqBySymbol{}", OPTPARAMS_LOOKUP_FIRST_ID), &mut conn, true, &mut hb, &shared));
        assert_eq!(shared.reference.drain_option_chains(), [(3, vec![])]);
        // The answer is forgotten when the auth link drops.
        op.connection_lost();
        op.request(4, "XYZ", "", "STK", 99, &mut conn, true, &mut hb, &shared);
        assert_eq!(sent(&mut server), ["35=U|6040=5|55=XYZ|310=STK|6457=99|6320=1"]);
    }
}
