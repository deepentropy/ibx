//! Golden tests of combo (BAG) orders against the reference's captured
//! frames (ibx#470; ib-agent#105, captures of 26/09/2026 and 30/09/2026,
//! account masked). The engine runs the captured scenario: every frame it
//! sends is compared with the reference's, each server frame of the
//! capture is fed to it (with the engine's own request and order ids),
//! and the callbacks the API gets are compared with the reference's.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use super::*;
use crate::api::client::EClient;
use crate::api::types::{ComboLeg, Contract as ApiContract, Execution, Order as ApiOrder, OrderState as ApiOrderState, TagValue};
use crate::api::wrapper::Wrapper;
use crate::bridge::SharedState;

const ACCOUNT: &str = "DUXXXXXXX";
const SPY: i64 = 756733;
const QQQ: i64 = 320227571;
const SMART_COMBO: i64 = 28812380;

type Frame = Vec<(u32, String)>;

/// The CCP frames of a recorded scenario: (leg, fields without the
/// header, sequence, time and checksum).
fn fixture(name: &str) -> Vec<(String, Frame)> {
    let path = format!("{}/tests/fixtures/gw1040/scenarios/{}", env!("CARGO_MANIFEST_DIR"), name);
    let text = std::fs::read_to_string(&path).unwrap();
    let mut out = Vec::new();
    for line in text.lines().skip(1) {
        let rec: serde_json::Value = serde_json::from_str(line).unwrap();
        if rec["kind"] != "fix" || rec["conn"] != "CCP" { continue; }
        let Some(fields) = rec["fields"].as_array() else { continue };
        let frame: Frame = fields.iter().filter_map(|f| {
            let tag: u32 = f[0].as_str()?.parse().ok()?;
            (!matches!(tag, 8 | 9 | 34 | 52 | 10)).then(|| (tag, f[1].as_str().unwrap_or("").to_string()))
        }).collect();
        out.push((rec["leg"].as_str().unwrap().to_string(), frame));
    }
    out
}

fn tag(f: &[(u32, String)], t: u32) -> Option<&str> {
    f.iter().find(|(k, _)| *k == t).map(|(_, v)| v.as_str())
}

/// A request id without its number.
fn label(id: &str) -> &str {
    id.trim_end_matches(|c: char| c.is_ascii_digit())
}

/// A frame the combo flow sends: a set-up request or an order message.
fn combo_frame(f: &[(u32, String)]) -> bool {
    match tag(f, 35) {
        Some("c") => tag(f, 320).is_some_and(|id| matches!(label(id),
            "SecDefReqMsgReqByConid" | "Query ICS typeReqByConid" | "EComboReqByConid"
            | "SmartComboEComboValidationProcessorReqByConid" | "FixSecDefReqBySymbol")),
        Some("U") => matches!(tag(f, 6040), Some("36" | "7")),
        Some("D" | "G" | "F") => true,
        _ => false,
    }
}

/// Fields the reference writes in no fixed order (its order attributes).
fn is_attribute(t: u32) -> bool {
    (70..100).contains(&order_builder::reference_rank(t))
}

/// The same message, field for field: the other fields in order, the
/// attributes in any order, the request id by its label, the order ids
/// and the limit price by value.
fn assert_same(ours: &[(u32, String)], theirs: &[(u32, String)]) {
    let norm = |f: &[(u32, String)]| -> (Frame, Frame) {
        let mut plain = Vec::new();
        let mut attrs = Vec::new();
        for (t, v) in f {
            let v = match *t {
                320 => label(v).to_string(),
                11 | 41 => String::new(),
                44 => format!("{}", v.parse::<f64>().unwrap()),
                _ => v.clone(),
            };
            if is_attribute(*t) { attrs.push((*t, v)); } else { plain.push((*t, v)); }
        }
        attrs.sort();
        (plain, attrs)
    };
    assert_eq!(norm(ours), norm(theirs), "\nours:   {ours:?}\ntheirs: {theirs:?}");
}

fn socket_pair() -> (std::net::TcpStream, std::net::TcpStream) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (server, _) = listener.accept().unwrap();
    (client, server)
}

/// What the API client got, one line per callback.
#[derive(Default)]
struct Callbacks(Vec<String>);

impl Wrapper for Callbacks {
    fn error(&mut self, req_id: i64, code: i64, text: &str, _: &str) {
        if code != 2109 { self.0.push(format!("error {req_id} {code} {text}")); }
    }
    fn open_order(&mut self, order_id: i64, c: &ApiContract, o: &ApiOrder, s: &ApiOrderState) {
        let legs: Vec<String> = c.combo_legs.iter()
            .map(|l| format!("{}/{}/{}/{}/{}/{}/{}", l.con_id, l.ratio, l.action, l.exchange, l.open_close, l.short_sale_slot, l.exempt_code))
            .collect();
        let prices: Vec<String> = o.order_combo_legs.iter().map(|p| if *p == f64::MAX { "MAX".into() } else { p.to_string() }).collect();
        let lmt = if o.lmt_price == f64::MAX { "MAX".to_string() } else { o.lmt_price.to_string() };
        self.0.push(format!("openOrder {order_id} {} {} {} {} {} {} {} [{}] {} {} lmt={lmt} legs=[{}] {}",
            c.con_id, c.symbol, c.sec_type, c.exchange, c.currency, c.local_symbol, c.trading_class,
            legs.join(" "), c.combo_legs_descrip, o.action, prices.join(" "), s.status));
    }
    fn order_status(&mut self, order_id: i64, status: &str, filled: f64, remaining: f64, avg: f64,
        _: i64, _: i64, last: f64, _: i64, _: &str, _: f64) {
        self.0.push(format!("orderStatus {order_id} {status} {filled} {remaining} {avg} {last}"));
    }
    fn exec_details(&mut self, req_id: i64, c: &ApiContract, e: &Execution) {
        self.0.push(format!("execDetails {req_id} {} {} {} {} {} | {} {} {} {} {} {} {}",
            c.con_id, c.symbol, c.sec_type, c.exchange, c.local_symbol,
            e.exec_id, e.exchange, e.side, e.shares, e.price, e.cum_qty, e.avg_price));
    }
    fn commission_and_fees_report(&mut self, r: &crate::api::types::CommissionAndFeesReport) {
        self.0.push(format!("commission {} {}", r.exec_id, r.commission_and_fees));
    }
}

/// One engine and one API client on a captured gateway session.
struct Session {
    engine: HotLoop,
    server: std::net::TcpStream,
    client: EClient,
    /// Frames the engine sent that the test has not compared yet.
    queued: std::collections::VecDeque<Frame>,
    /// Reference request ids and order ids to the engine's.
    ids: HashMap<String, String>,
    callbacks: Callbacks,
}

impl Session {
    fn new() -> Self {
        let shared = Arc::new(SharedState::new());
        shared.reference.set_smart_combo_con_ids("AUD:61227077,EUR:58666491,USD:28812380");
        shared.reference.set_api_client_id(198);
        let mut engine = HotLoop::new(shared.clone(), None, None);
        let (c, server) = socket_pair();
        engine.ccp_conn = Some(Connection::new_raw(c).unwrap());
        let (tx, rx) = crossbeam_channel::unbounded();
        engine.set_control_rx(rx);
        let id = engine.context.market.register(SMART_COMBO);
        engine.context.market.set_routing(id, "BAG", "SMART");
        let client = EClient::from_parts(shared, tx, std::thread::spawn(|| {}), ACCOUNT.into());
        client.seed_instrument(SMART_COMBO, id);
        Session { engine, server, client, queued: Default::default(), ids: HashMap::new(), callbacks: Callbacks::default() }
    }

    /// Run the engine once: commands, then the orders.
    fn run(&mut self) {
        self.engine.poll_control_commands();
        order_builder::drain_and_send_orders(&mut self.engine.ccp_conn, &mut self.engine.context, ACCOUNT,
            &mut self.engine.hb, false, &self.engine.shared);
        self.client.process_msgs(&mut self.callbacks);
    }

    /// Every frame the engine wrote, waiting at most `wait` for the first.
    fn read(&mut self, wait: Duration) {
        use std::io::Read;
        self.server.set_read_timeout(Some(wait)).unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 16384];
        loop {
            match self.server.read(&mut chunk) {
                Ok(n) if n > 0 => {
                    buf.extend_from_slice(&chunk[..n]);
                    self.server.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
                }
                _ => break,
            }
        }
        let mut frames: Vec<Frame> = Vec::new();
        for field in buf.split(|&b| b == crate::protocol::fix::SOH) {
            let Ok(s) = std::str::from_utf8(field) else { continue };
            let Some((t, v)) = s.split_once('=') else { continue };
            let Ok(t) = t.parse::<u32>() else { continue };
            if t == 8 { frames.push(Vec::new()); continue; }
            if matches!(t, 9 | 34 | 52 | 10) { continue; }
            if let Some(f) = frames.last_mut() { f.push((t, v.to_string())); }
        }
        self.queued.extend(frames);
    }

    /// The next frame the engine sent.
    fn next_sent(&mut self) -> Frame {
        if self.queued.is_empty() { self.read(Duration::from_secs(2)); }
        self.queued.pop_front().expect("the engine sent nothing more")
    }

    fn assert_nothing_sent(&mut self) {
        self.read(Duration::from_millis(200));
        assert!(self.queued.is_empty(), "sent: {:?}", self.queued);
    }

    /// Play the captured frames from the first one `from` matches to the
    /// first one `to` matches (excluded): each combo frame the reference
    /// sent is the engine's next frame; each server answer to a request of
    /// the engine, and each report of its orders, goes to the engine.
    fn play(&mut self, frames: &[(String, Frame)], from: impl Fn(&Frame) -> bool, to: impl Fn(&Frame) -> bool) {
        let start = frames.iter().position(|(_, f)| from(f)).expect("no first frame");
        for (leg, f) in &frames[start..] {
            if f.as_slice() != frames[start].1.as_slice() && to(f) { break; }
            if leg == "fix_out" {
                if !combo_frame(f) { continue; }
                let ours = self.next_sent();
                assert_same(&ours, f);
                if let (Some(theirs), Some(mine)) = (tag(f, 320), tag(&ours, 320)) {
                    self.ids.insert(theirs.to_string(), mine.to_string());
                }
                if let (Some(theirs), Some(mine)) = (tag(f, 11), tag(&ours, 11)) {
                    let base = |s: &str| s.split('.').next().unwrap().to_string();
                    self.ids.insert(base(theirs), base(mine));
                }
                continue;
            }
            let answer = match tag(f, 35) {
                Some("d") => tag(f, 320).is_some_and(|id| self.ids.contains_key(id)),
                Some("U") => match tag(f, 6040) {
                    Some("36" | "7") => tag(f, 320).is_some_and(|id| self.ids.contains_key(id)),
                    Some("60") => true,
                    _ => false,
                },
                Some("8") => tag(f, 11).is_some_and(|c| self.ids.contains_key(c.split('.').next().unwrap())),
                _ => false,
            };
            if !answer { continue; }
            let mut fields = f.clone();
            for (t, v) in fields.iter_mut() {
                match *t {
                    320 => if let Some(mine) = self.ids.get(v.as_str()) { *v = mine.clone(); },
                    11 | 41 => {
                        let (base, ver) = v.split_once('.').unwrap_or((v.as_str(), ""));
                        if let Some(mine) = self.ids.get(base) { *v = format!("{mine}.{ver}"); }
                    }
                    _ => {}
                }
            }
            let refs: Vec<(u32, &str)> = fields.iter().map(|(t, v)| (*t, v.as_str())).collect();
            let msg = crate::protocol::fix::fix_build(&refs, 1);
            self.engine.inject_ccp_message(&msg);
            self.run();
        }
    }
}

fn leg(con_id: i64, action: &str) -> ComboLeg {
    ComboLeg { con_id, ratio: 1, action: action.into(), exchange: "SMART".into(), ..Default::default() }
}

/// The captured stock combo: BUY 1 SPY, SELL 1 QQQ on SMART.
fn spy_qqq(symbol: &str) -> ApiContract {
    ApiContract {
        symbol: symbol.into(), sec_type: "BAG".into(), exchange: "SMART".into(), currency: "USD".into(),
        combo_legs: vec![leg(SPY, "BUY"), leg(QQQ, "SELL")],
        ..Default::default()
    }
}

fn combo_lmt(action: &str, price: f64) -> ApiOrder {
    ApiOrder {
        action: action.into(), total_quantity: 1.0, order_type: "LMT".into(), lmt_price: price,
        order_ref: "fourleg".into(),
        smart_combo_routing_params: vec![TagValue { tag: "NonGuaranteed".into(), value: "1".into() }],
        ..Default::default()
    }
}

fn is(t35: &'static str, label_of: &'static str) -> impl Fn(&Frame) -> bool {
    move |f: &Frame| tag(f, 35) == Some(t35) && tag(f, 320).is_some_and(|id| label(id) == label_of)
}

fn order_msg(t35: &'static str) -> impl Fn(&Frame) -> bool {
    move |f: &Frame| tag(f, 35) == Some(t35)
}

const COMBO_CONTRACT: &str = "28812380 QQQ,SPY BAG SMART USD QQQ,SPY COMB [756733/1/BUY/SMART/0/0/-1 320227571/1/SELL/SMART/0/0/-1] 756733|1,320227571|-1";

// ibx#470 (captured 30/09/2026 in regular hours, i105_combo_fill): the
// first combo order of a session sends the reference's set-up requests,
// then its 35=D; the reports of its fill give the combo's execution, then
// one execution per leg on the leg's contract, each with openOrder and
// orderStatus at the combo's totals. The opposite combo on the same legs
// sends no set-up request.
#[test]
fn combo_fill_session_is_the_captured_one() {
    let frames = fixture("20260930/i105_combo_fill.jsonl");
    let mut s = Session::new();
    s.client.place_order(42, &spy_qqq("QQQ,SPY"), &combo_lmt("BUY", 23.84)).unwrap();
    s.run();
    s.play(&frames, is("c", "SecDefReqMsgReqByConid"), order_msg("D"));
    // The set-up ends with the leg confirmation; the order goes out with
    // the next run of the engine.
    s.play(&frames, order_msg("D"), |f| tag(f, 35) == Some("D") && tag(f, 54) == Some("2"));
    let buy = std::mem::take(&mut s.callbacks.0);
    let open = |status: &str| format!("openOrder 42 {COMBO_CONTRACT} BUY lmt=23.84 legs=[MAX MAX] {status}");
    let want = [
        open("PreSubmitted"),
        "orderStatus 42 PreSubmitted 0 1 0 0".to_string(),
        open("PreSubmitted"),
        "orderStatus 42 PreSubmitted 0 1 0 0".to_string(),
        "execDetails -1 28812380 QQQ,SPY BAG SMART QQQ,SPY | 000225ed.6abc8e73.01.01 SMART BOT 1 23.38 1 23.38".to_string(),
        open("Filled"),
        "orderStatus 42 Filled 1 0 23.38 23.38".to_string(),
        "execDetails -1 756733 SPY STK MEMX SPY | 00025b49.6abe16db.01.01.01 MEMX BOT 1 766.72 1 766.72".to_string(),
        open("Filled"),
        "orderStatus 42 Filled 1 0 23.38 23.38".to_string(),
        "execDetails -1 320227571 QQQ STK ARCA QQQ | 00025b49.6abe16da.01.01.01 ARCA SLD 1 743.34 1 743.34".to_string(),
        open("Filled"),
        "orderStatus 42 Filled 1 0 23.38 23.38".to_string(),
        "commission 00025b49.6abe16db.01.01.01 1.000003".to_string(),
        "commission 00025b49.6abe16da.01.01.01 1.015511".to_string(),
    ];
    assert_eq!(buy, want);
    // The positions are the legs', not the combo's.
    assert_eq!(s.engine.shared.portfolio.position_fixed(s.engine.context.market.instrument_by_con_id(SPY).unwrap()), crate::types::QTY_SCALE);
    assert_eq!(s.engine.context.position_fixed(s.engine.context.market.instrument_by_con_id(SMART_COMBO).unwrap()), 0);

    // The opposite combo: no set-up request, the order at once.
    s.client.place_order(43, &spy_qqq("QQQ,SPY"), &combo_lmt("SELL", 22.80)).unwrap();
    s.run();
    s.play(&frames, |f| tag(f, 35) == Some("D") && tag(f, 54) == Some("2"), |_| false);
    let sell = std::mem::take(&mut s.callbacks.0);
    assert!(sell.iter().any(|l| l == "execDetails -1 756733 SPY STK ARCA SPY | 00025b49.6abe16e8.01.01.01 ARCA SLD 1 766.68 1 766.68"), "{sell:#?}");
    assert!(sell.iter().any(|l| l == "orderStatus 43 Filled 1 0 23.32 23.32"), "{sell:#?}");
    assert_eq!(s.engine.shared.portfolio.position_fixed(s.engine.context.market.instrument_by_con_id(SPY).unwrap()), 0);
}

// ibx#470 (captured 26/09/2026, market closed, i105_combo_stock_smart of
// both runs): with the BAG symbol SPY,QQQ the set-up runs and the order
// is refused with 478, no 35=D; a second order on the same legs is
// refused at once. In the same session, with the symbol QQQ,SPY: no
// set-up request, the 35=D, the price change as a 35=G without the leg
// block, the cancel as a 35=F with the BAG conId, and the same for a
// second order.
#[test]
fn combo_symbol_refusal_then_order_modify_and_cancel_are_the_captured_ones() {
    let first = fixture("20260926/i105_combo_stock_smart.jsonl");
    let mut s = Session::new();
    s.client.place_order(23, &spy_qqq("SPY,QQQ"), &combo_lmt("BUY", -23.15)).unwrap();
    s.run();
    s.play(&first, is("c", "SecDefReqMsgReqByConid"), |_| false);
    s.run();
    let refusal = "error 23 478 Parameters in request conflicts with contract parameters received by contract id:Requested symbol SPY,QQQ, in legs QQQ,SPY";
    assert_eq!(s.callbacks.0, [refusal]);
    s.assert_nothing_sent();
    s.callbacks.0.clear();
    s.client.place_order(24, &spy_qqq("SPY,QQQ"), &combo_lmt("BUY", -24.15)).unwrap();
    s.run();
    s.assert_nothing_sent();
    assert_eq!(s.callbacks.0, [refusal.replace("error 23", "error 24")]);
    s.callbacks.0.clear();

    let second = fixture("20260926b/i105_combo_stock_smart.jsonl");
    let mut order = combo_lmt("BUY", -23.15);
    s.client.place_order(28, &spy_qqq("QQQ,SPY"), &order).unwrap();
    s.run();
    s.play(&second, order_msg("D"), order_msg("G"));
    order.lmt_price = -23.05;
    s.client.place_order(28, &spy_qqq("QQQ,SPY"), &order).unwrap();
    s.run();
    s.play(&second, order_msg("G"), order_msg("F"));
    s.client.cancel_order(28, "").unwrap();
    s.run();
    s.play(&second, order_msg("F"), order_msg("D"));
    let calls = std::mem::take(&mut s.callbacks.0);
    assert!(calls.iter().any(|l| l.starts_with("openOrder 28 28812380 QQQ,SPY BAG SMART USD QQQ,SPY COMB") && l.contains("lmt=-23.05")), "{calls:#?}");
    assert!(calls.iter().any(|l| l.starts_with("error 28 399 Order Message:")), "{calls:#?}");
    assert_eq!(calls.iter().filter(|l| l.starts_with("error 28 202 ")).count(), 1, "{calls:#?}");
    assert!(calls.last().unwrap().starts_with("orderStatus 28 Cancelled"), "{calls:#?}");
}

// ibx#470 (captured 26/09/2026, i105_combo_leg_prices): per-leg prices
// and no limit price give the combo price (721.35 - 794.50) and one 6879
// per leg after it; the price change of a leg keeps the placed prices on
// the 35=G, with the leg block. openOrder shows the reported leg prices
// and no limit price.
#[test]
fn combo_leg_prices_are_the_captured_ones() {
    let first = fixture("20260926/i105_combo_stock_smart.jsonl");
    let frames = fixture("20260926b/i105_combo_leg_prices.jsonl");
    let mut s = Session::new();
    // The session's set-up, as in the capture's session.
    s.client.place_order(23, &spy_qqq("SPY,QQQ"), &combo_lmt("BUY", -23.15)).unwrap();
    s.run();
    s.play(&first, is("c", "SecDefReqMsgReqByConid"), |_| false);
    s.run();
    s.callbacks.0.clear();

    let mut order = combo_lmt("BUY", f64::MAX);
    order.order_combo_legs = vec![721.35, 794.50];
    s.client.place_order(30, &spy_qqq("QQQ,SPY"), &order).unwrap();
    s.run();
    s.play(&frames, order_msg("D"), order_msg("G"));
    order.order_combo_legs[0] = 721.45;
    s.client.place_order(30, &spy_qqq("QQQ,SPY"), &order).unwrap();
    s.run();
    s.play(&frames, order_msg("G"), order_msg("F"));
    s.client.cancel_order(30, "").unwrap();
    s.run();
    s.play(&frames, order_msg("F"), |_| false);
    let calls = std::mem::take(&mut s.callbacks.0);
    let open: Vec<&String> = calls.iter().filter(|l| l.starts_with("openOrder 30 ")).collect();
    assert!(!open.is_empty(), "{calls:#?}");
    for l in open {
        assert!(l.contains("lmt=MAX legs=[721.35 794.5]"), "{l}");
    }
}

// ibx#470 (captured 26/09/2026, i105_combo_directed): a combo on ARCA asks
// the BAG by its symbol on ARCA; with no definition the order gets 200
// and nothing else is sent.
#[test]
fn directed_combo_without_definition_is_the_captured_one() {
    let frames = fixture("20260926b/i105_combo_directed.jsonl");
    let mut s = Session::new();
    let id = s.engine.context.market.register(0);
    s.client.seed_instrument(0, id);
    let mut contract = spy_qqq("QQQ,SPY");
    contract.exchange = "ARCA".into();
    for l in contract.combo_legs.iter_mut() { l.exchange = "ARCA".into(); }
    let mut order = combo_lmt("BUY", -23.15);
    order.smart_combo_routing_params.clear();
    s.client.place_order(32, &contract, &order).unwrap();
    s.run();
    s.play(&frames, is("c", "FixSecDefReqBySymbol"), |_| false);
    s.run();
    s.assert_nothing_sent();
    assert_eq!(s.callbacks.0, ["error 32 200 No security definition has been found for the request"]);
}

// ibx#470: the reference's refusals while it reads and checks a combo
// order (`jextend.bH.q(ee)`, `jextend.bH.S()`, `jextend.at.a(OrderCreator)`),
// before anything is sent.
#[test]
fn combo_refusals_before_sending() {
    let mut s = Session::new();
    let place = |s: &mut Session, contract: &ApiContract, order: &ApiOrder| -> Vec<String> {
        s.client.place_order(7, contract, order).unwrap();
        s.run();
        std::mem::take(&mut s.callbacks.0)
    };
    let lmt = combo_lmt("BUY", 1.0);
    let no_legs = ApiContract { combo_legs: Vec::new(), ..spy_qqq("QQQ,SPY") };
    assert_eq!(place(&mut s, &no_legs, &lmt), ["error 7 321 Error validating request.-'bH' : cause - Security type 'BAG' requires combo leg details."]);
    let chf = ApiContract { currency: "CHF".into(), ..spy_qqq("QQQ,SPY") };
    assert_eq!(place(&mut s, &chf, &lmt), ["error 7 321 Error validating request.-'bH' : cause - Currency CHF isn't supported for smart combo."]);
    let unknown = ApiOrder { smart_combo_routing_params: vec![TagValue { tag: "Foo".into(), value: "1".into() }], ..lmt.clone() };
    assert!(place(&mut s, &spy_qqq("QQQ,SPY"), &unknown)[0].starts_with("error 7 320 Error reading request:Invalid combo routing tag."));
    let three_prices = ApiOrder { order_combo_legs: vec![1.0, 2.0, 3.0], lmt_price: f64::MAX, ..lmt.clone() };
    assert_eq!(place(&mut s, &spy_qqq("QQQ,SPY"), &three_prices),
        ["error 7 320 Error reading request:Mismatch per-leg price number with combo leg specification."]);
    let priced_mkt = ApiOrder { order_type: "MKT".into(), order_combo_legs: vec![1.0, 2.0], ..lmt.clone() };
    assert_eq!(place(&mut s, &spy_qqq("QQQ,SPY"), &priced_mkt),
        ["error 7 321 Error validating request.-'bH' : cause - The combo details for leg '0' are invalid. - Only LMT or REL+LMT order allows using per-leg prices."]);
    let one_price = ApiOrder { order_combo_legs: vec![1.0, f64::MAX], lmt_price: f64::MAX, ..lmt.clone() };
    assert_eq!(place(&mut s, &spy_qqq("QQQ,SPY"), &one_price),
        ["error 7 321 Error validating request.-'bH' : cause - The combo details for leg '1' are invalid. - All leg prices are needed when specifying per-leg prices."]);
    let both = ApiOrder { order_combo_legs: vec![1.0, 2.0], ..lmt.clone() };
    assert_eq!(place(&mut s, &spy_qqq("QQQ,SPY"), &both),
        ["error 7 321 Error validating request.-'bH' : cause - Can't specify combo price when using per-leg prices."]);
    let mut bad_leg = spy_qqq("QQQ,SPY");
    bad_leg.combo_legs[1].action = "HOLD".into();
    assert_eq!(place(&mut s, &bad_leg, &lmt),
        ["error 7 321 Error validating request.-'bH' : cause - The combo details for leg '1' are invalid. -  conid, ratio, side: 320227571, 1, 0"]);
    let mut short_leg = spy_qqq("QQQ,SPY");
    short_leg.combo_legs[1].action = "SSHORT".into();
    assert_eq!(place(&mut s, &short_leg, &lmt),
        ["error 7 321 Error validating request.-'bH' : cause - The combo details for leg '1' are invalid. - Not an institutional account, or an away clearing order"]);
    let mut two_two = spy_qqq("QQQ,SPY");
    for l in two_two.combo_legs.iter_mut() { l.ratio = 2; }
    assert_eq!(place(&mut s, &two_two, &lmt), ["error 7 321 Error validating request.-'bH' : cause - Invalid leg ratio."]);
    s.assert_nothing_sent();
}

// ibx#470 (captured 28/09/2026 in regular hours, rth_order_types): the
// set-up of a SMART option vertical (SPY 06/10/2026 768 C BUY, 769 C
// SELL): the option legs give leg category 0, and the multiplier request
// and the leg confirmation carry it; no NonGuaranteed flag on a
// guaranteed combo. (The reference then looks up the BAG on each option
// exchange and refuses the order with 460 on this paper account, which
// has no combo permission: not part of this check.)
#[test]
fn option_vertical_set_up_is_the_captured_one() {
    let frames = fixture("20260928/rth_order_types.jsonl");
    let mut s = Session::new();
    let vertical = ApiContract {
        symbol: "SPY".into(), sec_type: "BAG".into(), exchange: "SMART".into(), currency: "USD".into(),
        combo_legs: vec![leg(927851902, "BUY"), leg(927851945, "SELL")],
        ..Default::default()
    };
    let order = ApiOrder { action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(), lmt_price: 0.05, ..Default::default() };
    s.client.place_order(60, &vertical, &order).unwrap();
    s.run();
    let bag = |f: &Frame| is("c", "SecDefReqMsgReqByConid")(f) && tag(f, 6008) == Some("28812380");
    s.play(&frames, bag, |f| tag(f, 35) == Some("U") && tag(f, 6040) == Some("193"));
    let combo = s.engine.context.combos.built_for_test().expect("built");
    assert_eq!((combo.category(), combo.multiplier, combo.symbol()), (0, 100.0, "SPY".to_string()));
}
