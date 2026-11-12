//! Order-path tests against the real server on the paper account, through `EClient`.
//!
//! Covers the order paths fixed in ibx#240 ibx#225 ibx#247 ibx#324 ibx#334
//! ibx#339 ibx#349 ibx#313 ibx#318 ibx#405 ibx#325 ibx#327 ibx#250 ibx#328 ibx#263 ibx#717. Each case checks
//! the server's own reply (captured from the engine's wire trace), not only
//! that `place_order` returned: the replace confirmation must carry the new
//! values, a bracket child must be cancelled by the server with its parent
//! (proof that the parent link arrived), the condition flags must come back
//! as sent. Every order is placed far from the market and cancelled at the
//! end.
//!
//! Requires IB_USERNAME and IB_PASSWORD (paper account) in the environment.
//! Run with: cargo test --test order_paths_paper -- --nocapture

use std::env;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use ibx::api::client::{Contract, EClient, EClientConfig, Order, TagValue};
use ibx::api::wrapper::Wrapper;
use ibx::types::{OrderCondition, PRICE_SCALE};

// ── Wire capture ──
//
// The engine logs every frame it sends ("WIRE>") and receives ("WIRE<") at
// trace level. This logger keeps those lines so the test can read the
// server's replies, and prints warnings (for example the reason of a
// rejected order).

struct WireLog {
    lines: Mutex<Vec<String>>,
}

impl log::Log for WireLog {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }
    fn log(&self, record: &log::Record) {
        let msg = record.args().to_string();
        if msg.starts_with("WIRE") {
            self.lines.lock().unwrap().push(msg);
        } else if record.level() <= log::Level::Warn {
            eprintln!("[{}] {}", record.level(), msg);
        }
    }
    fn flush(&self) {}
}

fn wire() -> &'static WireLog {
    static WIRE: OnceLock<&'static WireLog> = OnceLock::new();
    WIRE.get_or_init(|| {
        let w: &'static WireLog = Box::leak(Box::new(WireLog { lines: Mutex::new(Vec::new()) }));
        let _ = log::set_logger(w);
        log::set_max_level(log::LevelFilter::Trace);
        w
    })
}

type Frame = Vec<(u32, String)>;

fn parse_frame(line: &str) -> Frame {
    // "WIRE> seq=N <text frame>" or "WIRE< ccp/comp <text frame>"
    let body = line.find("8=").map(|i| &line[i..]).unwrap_or("");
    body.split('|')
        .filter_map(|f| {
            let (t, v) = f.split_once('=')?;
            Some((t.parse().ok()?, v.to_string()))
        })
        .collect()
}

fn field<'a>(f: &'a Frame, tag: u32) -> Option<&'a str> {
    f.iter().find(|(t, _)| *t == tag).map(|(_, v)| v.as_str())
}

/// Frames sent (`outbound`) or received for `clord` with the given message type.
fn frames(outbound: bool, msg_type: &str, clord: &str) -> Vec<Frame> {
    let prefix = if outbound { "WIRE>" } else { "WIRE<" };
    wire().lines.lock().unwrap().iter()
        .filter(|l| l.starts_with(prefix))
        .map(|l| parse_frame(l))
        .filter(|f| field(f, 35) == Some(msg_type) && field(f, 11) == Some(clord))
        .collect()
}

/// The new orders sent for the API order id `id` (ibx#466).
fn new_orders(id: i64) -> Vec<Frame> {
    let id = id.to_string();
    wire().lines.lock().unwrap().iter()
        .filter(|l| l.starts_with("WIRE>"))
        .map(|l| parse_frame(l))
        .filter(|f| field(f, 35) == Some("D") && field(f, 6121) == Some(id.as_str()))
        .collect()
}

/// The client id of the order of API order id `id` at `version`: the server
/// id its new order went out under, from the order id generator (ibx#466).
fn clord_of(id: i64, version: u32) -> String {
    let server = new_orders(id).first()
        .and_then(|f| field(f, 11).and_then(|c| c.split('.').next()).map(str::to_string))
        .unwrap_or_else(|| format!("no-new-order-{id}"));
    format!("{server}.{version}")
}

/// The server's reply to `clord` with the given ExecType, if any.
fn reply(clord: &str, exec_types: &[&str]) -> Option<Frame> {
    frames(false, "8", clord).into_iter()
        .find(|f| field(f, 150).is_some_and(|e| exec_types.contains(&e)))
}

fn same_number(a: Option<&str>, b: f64) -> bool {
    a.and_then(|s| s.parse::<f64>().ok()).is_some_and(|x| (x - b).abs() < 1e-9)
}

// ── Client plumbing ──

#[derive(Default)]
struct State {
    statuses: Vec<(i64, String)>,
    errors: Vec<(i64, i64, String)>,
    /// Every order callback in arrival order ("status:<id>:<status>",
    /// "error:<id>:<code>").
    sequence: Vec<String>,
    /// The order ids of the openOrder rows, and the openOrderEnd count.
    open_rows: Vec<i64>,
    open_ends: usize,
}

struct Probe {
    state: Arc<Mutex<State>>,
}

impl Wrapper for Probe {
    fn order_status(
        &mut self, order_id: i64, status: &str, _filled: f64, _remaining: f64,
        _avg_fill_price: f64, _perm_id: i64, _parent_id: i64, _last_fill_price: f64,
        _client_id: i64, _why_held: &str, _mkt_cap_price: f64,
    ) {
        let mut s = self.state.lock().unwrap();
        s.statuses.push((order_id, status.into()));
        s.sequence.push(format!("status:{}:{}", order_id, status));
    }
    fn error(&mut self, req_id: i64, code: i64, msg: &str, _adv: &str) {
        eprintln!("  [error] id={} code={} {}", req_id, code, msg);
        let mut s = self.state.lock().unwrap();
        s.errors.push((req_id, code, msg.into()));
        s.sequence.push(format!("error:{}:{}", req_id, code));
    }
    fn open_order(&mut self, order_id: i64, _contract: &Contract, _order: &Order, _state: &ibx::api::types::OrderState) {
        self.state.lock().unwrap().open_rows.push(order_id);
    }
    fn open_order_end(&mut self) {
        self.state.lock().unwrap().open_ends += 1;
    }
}

struct Paper {
    client: EClient,
    probe: Probe,
    state: Arc<Mutex<State>>,
    placed: Vec<i64>,
    failures: Vec<String>,
}

impl Paper {
    fn pump(&mut self, secs: u64, done: impl Fn(&State) -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            self.client.process_msgs(&mut self.probe);
            if done(&self.state.lock().unwrap()) { return true; }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }
    fn place(&mut self, id: i64, order: &Order) {
        self.placed.push(id);
        if let Err(e) = self.client.place_order(id, &aapl(), order) {
            self.fail(&format!("order {}: place_order returned {}", id, e));
        }
    }
    fn last_status(&self, id: i64) -> Option<String> {
        last_status(&self.state.lock().unwrap(), id)
    }
    fn fail(&mut self, what: &str) {
        println!("    FAIL: {}", what);
        self.failures.push(what.to_string());
    }
    fn check(&mut self, ok: bool, what: &str) {
        if ok { println!("    ok: {}", what) } else { self.fail(what) }
    }
    /// Wait until every order is working or refused; true when all work.
    fn wait_working(&mut self, ids: &[i64]) -> bool {
        let ids = ids.to_vec();
        self.pump(20, |s| ids.iter().all(|&i| working(s, i) || refused(s, i)));
        let s = self.state.lock().unwrap();
        ids.iter().all(|&i| working(&s, i))
    }
}

fn last_status(s: &State, id: i64) -> Option<String> {
    s.statuses.iter().rev().find(|(o, _)| *o == id).map(|(_, st)| st.clone())
}

fn working(s: &State, id: i64) -> bool {
    matches!(last_status(s, id).as_deref(), Some("PreSubmitted" | "Submitted"))
}

/// 399 is an informational warning (order held until the open), not a
/// refusal; nor is 10349, the time in force taken from the order preset
/// for an order without one (ibx#660), after which the order goes on.
fn refused(s: &State, id: i64) -> bool {
    s.errors.iter().any(|(r, c, _)| *r == id && *c != 399 && *c != 10349)
        || last_status(s, id).as_deref() == Some("Inactive")
}

fn aapl() -> Contract {
    Contract {
        con_id: 265598, symbol: "AAPL".into(), sec_type: "STK".into(),
        exchange: "SMART".into(), currency: "USD".into(), ..Default::default()
    }
}

fn get_config() -> Option<EClientConfig> {
    let username = env::var("IB_USERNAME").ok()?;
    let password = env::var("IB_PASSWORD").ok()?;
    let host = env::var("IB_HOST").unwrap_or_else(|_| "cdc1.ibllc.com".to_string());
    Some(EClientConfig { username, password, host, paper: true, core_id: None, client_id: 0, copied_waits: false, code_provider: None })
}

// ── Scenarios ──

/// ibx#240 ibx#225: an adjustable stop as a bracket child keeps its parent
/// link, OCA group and tif, for each conversion type, with the reference's
/// type code.
fn adjustable_stop_brackets(paper: &mut Paper, base: i64) {
    for (i, (label, adjusted, code)) in [
        ("STP -> STP", "STP", "3"),
        ("STP -> TRAIL", "TRAIL", "T"),
        ("STP -> TRAIL LIMIT", "TRAIL LIMIT", "TSL"),
    ].into_iter().enumerate() {
        let parent_id = base + 10 * i as i64;
        let (stop_id, tp_id) = (parent_id + 1, parent_id + 2);
        println!("  adjustable stop bracket, {} (parent {})", label, parent_id);
        let oca = format!("ibxpaper_adj_{}", parent_id);
        let parent = Order {
            action: "BUY".into(), order_type: "LMT".into(), total_quantity: 1.0,
            lmt_price: 1.00, tif: "GTC".into(), ..Default::default()
        };
        let mut stop = Order {
            action: "SELL".into(), order_type: "STP".into(), total_quantity: 1.0, aux_price: 0.50,
            adjusted_order_type: adjusted.into(), trigger_price: 900.00, adjusted_stop_price: 0.60,
            parent_id, oca_group: oca.clone(), tif: "GTC".into(), ..Default::default()
        };
        if adjusted != "STP" {
            stop.adjusted_trailing_amount = 0.10;
            stop.adjustable_trailing_unit = 0;
        }
        if adjusted == "TRAIL LIMIT" {
            stop.adjusted_stop_limit_price = 0.55;
        }
        let tp = Order {
            action: "SELL".into(), order_type: "LMT".into(), total_quantity: 1.0, lmt_price: 5000.00,
            parent_id, oca_group: oca, tif: "GTC".into(), ..Default::default()
        };
        paper.place(parent_id, &parent);
        paper.place(stop_id, &stop);
        paper.place(tp_id, &tp);
        let up = paper.wait_working(&[parent_id, stop_id, tp_id]);
        paper.check(up, &format!("{}: parent and both children accepted", label));
        let sent = frames(true, "D", &clord_of(stop_id, 0));
        paper.check(sent.first().and_then(|f| field(f, 6261)) == Some(code),
            &format!("{}: adjusted type code {} sent", label, code));
        if !up { continue; }
        paper.client.cancel_order(parent_id, "").ok();
        let ids = [parent_id, stop_id, tp_id];
        let cascaded = paper.pump(20, |s| ids.iter().all(|&i| last_status(s, i).as_deref() == Some("Cancelled")));
        paper.check(cascaded, &format!("{}: children cancelled by the server with the parent", label));
    }
}

/// ibx#247 ibx#324 ibx#334 ibx#339 ibx#349: each replace is accepted and the
/// server's confirmation carries the new values.
fn modifies(paper: &mut Paper, base: i64) {
    let lmt = |px: f64, rth: bool, tif: &str| Order {
        action: "BUY".into(), order_type: "LMT".into(), total_quantity: 1.0,
        lmt_price: px, outside_rth: rth, tif: tif.into(), ..Default::default()
    };
    let stp = |aux: f64| Order {
        action: "SELL".into(), order_type: "STP".into(), total_quantity: 1.0, aux_price: aux, tif: "DAY".into(), ..Default::default()
    };
    let stp_lmt = |l: f64, aux: f64| Order {
        action: "SELL".into(), order_type: "STP LMT".into(), total_quantity: 1.0,
        lmt_price: l, aux_price: aux, tif: "DAY".into(), ..Default::default()
    };
    let trail = |aux: f64| Order {
        action: "SELL".into(), order_type: "TRAIL".into(), total_quantity: 1.0, aux_price: aux, tif: "DAY".into(), ..Default::default()
    };
    let trail_lmt = |aux: f64| Order {
        action: "SELL".into(), order_type: "TRAIL LIMIT".into(), total_quantity: 1.0,
        aux_price: aux, lmt_price_offset: 0.50, trail_stop_price: 50.0, tif: "DAY".into(), ..Default::default()
    };
    let trail_percent = |p: f64| Order {
        action: "SELL".into(), order_type: "TRAIL".into(), total_quantity: 1.0, trailing_percent: p, tif: "DAY".into(), ..Default::default()
    };
    let gtd_stp = |aux: f64| Order {
        action: "SELL".into(), order_type: "STP".into(), total_quantity: 1.0, aux_price: aux,
        tif: "GTD".into(), good_till_date: "20261230 16:00:00 US/Eastern".into(), ..Default::default()
    };

    // (label, first, second, expected fields in the server's replace confirmation)
    let cases: Vec<(&str, Order, Order, Vec<(u32, &str)>)> = vec![
        ("LMT price, outside-RTH off", lmt(200.0, false, "DAY"), lmt(201.0, false, "DAY"), vec![(44, "201")]),
        ("LMT price, outside-RTH on", lmt(200.0, true, "DAY"), lmt(201.0, true, "DAY"), vec![(44, "201")]),
        ("STP trigger", stp(200.0), stp(195.0), vec![(99, "195")]),
        ("STP LMT both prices", stp_lmt(194.0, 195.0), stp_lmt(189.0, 190.0), vec![(44, "189"), (99, "190")]),
        ("TRAIL amount", trail(100.0), trail(110.0), vec![(40, "P"), (99, "110")]),
        ("TRAIL LIMIT amount", trail_lmt(100.0), trail_lmt(110.0), vec![(40, "TSL"), (99, "110")]),
        ("TRAIL percent (not 1%)", trail_percent(5.25), trail_percent(6.0), vec![(99, "6"), (6268, "100")]),
        ("LMT DAY -> GTC", lmt(200.0, false, "DAY"), lmt(200.0, false, "GTC"), vec![(59, "1")]),
        ("GTD STP trigger", gtd_stp(200.0), gtd_stp(195.0), vec![(99, "195"), (59, "6")]),
    ];
    for (i, (label, first, second, want)) in cases.into_iter().enumerate() {
        let id = base + i as i64;
        println!("  modify, {} (order {})", label, id);
        paper.place(id, &first);
        if !paper.wait_working(&[id]) {
            paper.fail(&format!("modify {}: original order not accepted ({:?})", label, paper.last_status(id)));
            continue;
        }
        std::thread::sleep(Duration::from_millis(300));
        let rth = second.outside_rth;
        paper.place(id, &second);
        let clord = clord_of(id, 1);
        let c = clord.clone();
        paper.pump(15, |_| reply(&c, &["5"]).is_some());
        match reply(&clord, &["5"]) {
            None => paper.fail(&format!("modify {}: no replace confirmation", label)),
            Some(ack) => {
                for (tag, value) in &want {
                    let ok = match value.parse::<f64>() {
                        Ok(v) => same_number(field(&ack, *tag), v),
                        Err(_) => field(&ack, *tag) == Some(*value),
                    };
                    paper.check(ok, &format!("modify {}: server confirms {}={} (got {:?})",
                        label, tag, value, field(&ack, *tag)));
                }
            }
        }
        // Outside-RTH is not repeated in the confirmation: check what was sent.
        let sent = frames(true, "G", &clord);
        let flag = sent.first().and_then(|f| field(f, 6433)).is_some();
        paper.check(!sent.is_empty() && flag == rth,
            &format!("modify {}: outside-RTH sent only when set ({})", label, rth));
        let s = paper.state.lock().unwrap();
        let err = s.errors.iter().any(|(r, c, _)| *r == id && *c != 399 && *c != 10349);
        drop(s);
        paper.check(!err, &format!("modify {}: no error", label));
    }

    // A change of order type is refused before sending, with error 329.
    // Same side as the placed order, as the reference capture
    // (ib-agent#192 A4b): a side change is refused first, with 105.
    let id = base + 20;
    println!("  modify, LMT -> STP refused (order {})", id);
    paper.place(id, &lmt(200.0, false, "DAY"));
    if paper.wait_working(&[id]) {
        paper.place(id, &Order { action: "BUY".into(), ..stp(195.0) });
        let got = paper.pump(5, |s| s.errors.iter().any(|(r, c, _)| *r == id && *c == 329));
        paper.check(got, "modify LMT -> STP: error 329 on the right order id");
        paper.check(frames(true, "G", &clord_of(id, 1)).is_empty(), "modify LMT -> STP: nothing sent");
    } else {
        paper.fail("modify LMT -> STP: original order not accepted");
    }
}

/// ibx#313: a fractional quantity is refused before sending, with error 10243.
fn fractional(paper: &mut Paper, id: i64) {
    println!("  fractional quantity (order {})", id);
    let order = Order {
        action: "BUY".into(), order_type: "LMT".into(), total_quantity: 1.5, lmt_price: 200.0, tif: "DAY".into(), ..Default::default()
    };
    paper.place(id, &order);
    let got = paper.pump(5, |s| s.errors.iter().any(|(r, c, _)| *r == id && *c == 10243));
    paper.check(got, "fractional: error 10243 on the right order id");
    paper.check(new_orders(id).is_empty(), "fractional: nothing sent");
}

/// ibx#318 ibx#405: algo children keep parent link, OCA group and tif; every
/// algo type is accepted.
fn algos(paper: &mut Paper, base: i64) {
    let adaptive = Order {
        action: "SELL".into(), order_type: "LMT".into(), total_quantity: 1.0, lmt_price: 5000.0,
        algo_strategy: "Adaptive".into(),
        algo_params: vec![TagValue { tag: "adaptivePriority".into(), value: "Normal".into() }],
        tif: "DAY".into(), ..Default::default()
    };
    let twap = Order {
        action: "SELL".into(), order_type: "LMT".into(), total_quantity: 1.0, lmt_price: 5000.0,
        algo_strategy: "Twap".into(),
        algo_params: vec![TagValue { tag: "allowPastEndTime".into(), value: "1".into() }],
        tif: "DAY".into(), ..Default::default()
    };
    // The server refuses GTC for TWAP (and VWAP, ArrivalPx) itself.
    for (i, (label, child, tif)) in [("Adaptive child, GTC", adaptive, "GTC"), ("TWAP child, DAY", twap, "DAY")]
        .into_iter().enumerate()
    {
        let parent_id = base + 10 * i as i64;
        let (algo_id, stop_id) = (parent_id + 1, parent_id + 2);
        println!("  algo bracket, {} (parent {})", label, parent_id);
        let oca = format!("ibxpaper_algo_{}", parent_id);
        let parent = Order {
            action: "BUY".into(), order_type: "LMT".into(), total_quantity: 1.0,
            lmt_price: 1.00, tif: tif.into(), ..Default::default()
        };
        let child = Order { parent_id, oca_group: oca.clone(), oca_type: 1, tif: tif.into(), ..child };
        let stop = Order {
            action: "SELL".into(), order_type: "STP".into(), total_quantity: 1.0, aux_price: 0.50,
            parent_id, oca_group: oca, oca_type: 1, tif: tif.into(), ..Default::default()
        };
        paper.place(parent_id, &parent);
        paper.place(algo_id, &child);
        paper.place(stop_id, &stop);
        let up = paper.wait_working(&[parent_id, algo_id, stop_id]);
        paper.check(up, &format!("{}: parent and both children accepted", label));
        if !up { continue; }
        let tif_code = if tif == "GTC" { "1" } else { "0" };
        let ack = reply(&clord_of(algo_id, 0), &["0", "A"]);
        paper.check(ack.as_ref().and_then(|f| field(f, 59)) == Some(tif_code),
            &format!("{}: server confirms the time-in-force", label));
        paper.client.cancel_order(parent_id, "").ok();
        let ids = [parent_id, algo_id, stop_id];
        let cascaded = paper.pump(20, |s| ids.iter().all(|&i| last_status(s, i).as_deref() == Some("Cancelled")));
        paper.check(cascaded, &format!("{}: children cancelled by the server with the parent", label));
    }

    let id = base + 20;
    println!("  standalone VWAP (order {})", id);
    let vwap = Order {
        action: "BUY".into(), order_type: "LMT".into(), total_quantity: 1.0, lmt_price: 200.0,
        algo_strategy: "Vwap".into(),
        algo_params: vec![
            TagValue { tag: "maxPctVol".into(), value: "0.1".into() },
            TagValue { tag: "noTakeLiq".into(), value: "0".into() },
            TagValue { tag: "allowPastEndTime".into(), value: "1".into() },
        ],
        // DAY: the server refuses GTC for VWAP, and an order without a
        // time in force takes the preset's, GTC on the paper user's
        // stock preset (seen 18/10/2026; ibx#660).
        tif: "DAY".into(),
        ..Default::default()
    };
    paper.place(id, &vwap);
    let up = paper.wait_working(&[id]);
    paper.check(up, "standalone VWAP accepted");

    // ibx#263: an Adaptive stop goes out as a stop with its stop price
    // and the Adaptive block, as the reference sends it, and the server
    // keeps it as a stop (PreSubmitted outside the market hours).
    let id = base + 21;
    println!("  Adaptive STP (order {})", id);
    let adaptive_stop = Order {
        action: "BUY".into(), order_type: "STP".into(), total_quantity: 1.0, aux_price: 5000.0,
        algo_strategy: "Adaptive".into(),
        algo_params: vec![TagValue { tag: "adaptivePriority".into(), value: "Normal".into() }],
        tif: "DAY".into(), ..Default::default()
    };
    paper.place(id, &adaptive_stop);
    let up = paper.wait_working(&[id]);
    paper.check(up, "Adaptive STP accepted");
    let clord = clord_of(id, 0);
    let sent = frames(true, "D", &clord).into_iter().next();
    paper.check(sent.as_ref().is_some_and(|f| field(f, 40) == Some("3") && same_number(field(f, 99), 5000.0)
        && field(f, 44).is_none() && field(f, 18) == Some("e") && field(f, 847) == Some("Adaptive")),
        "Adaptive STP sent as a stop with its stop price and the Adaptive block");
    let ack = reply(&clord, &["0", "A"]);
    paper.check(ack.as_ref().is_some_and(|f| field(f, 40) == Some("3") && same_number(field(f, 99), 5000.0)),
        "server holds the Adaptive STP as a stop at its stop price");
}

/// ibx#325 ibx#327: an order whose only extra is a condition keeps it, with
/// the flags on the right fields.
fn conditions(paper: &mut Paper, base: i64) {
    for (i, (label, cancel_order, ignore_rth)) in [
        ("condition only", false, false),
        ("condition + cancel-order", true, false),
        ("condition + ignore-RTH", false, true),
    ].into_iter().enumerate() {
        let id = base + i as i64;
        println!("  {} (order {})", label, id);
        let order = Order {
            action: "BUY".into(), order_type: "LMT".into(), total_quantity: 1.0, lmt_price: 200.0,
            conditions: vec![OrderCondition::Price {
                con_id: 265598, exchange: "SMART".into(), price: 1000 * PRICE_SCALE,
                is_more: true, trigger_method: 0,
            }],
            conditions_cancel_order: cancel_order,
            conditions_ignore_rth: ignore_rth,
            tif: "DAY".into(), ..Default::default()
        };
        paper.place(id, &order);
        let up = paper.wait_working(&[id]);
        paper.check(up, &format!("{}: accepted", label));
        // The reply that carries the condition: a GTC order (the time in
        // force of the paper user's stock preset, ibx#660) gets a first
        // report without it, then a second with it (paper, 18/10/2026).
        let ack = frames(false, "8", &clord_of(id, 0)).into_iter()
            .filter(|f| field(f, 150).is_some_and(|e| e == "0" || e == "A"))
            .find(|f| field(f, 6136).is_some());
        let flag = |on: bool| if on { "1" } else { "0" };
        paper.check(ack.as_ref().and_then(|f| field(f, 6136)) == Some("1"),
            &format!("{}: server confirms the condition", label));
        paper.check(ack.as_ref().and_then(|f| field(f, 6151)) == Some(flag(cancel_order))
                && ack.as_ref().and_then(|f| field(f, 6128)) == Some(flag(ignore_rth)),
            &format!("{}: server confirms both flags as sent", label));
    }
}

/// ibx#250, ibx#486: a server reject reaches the caller as the Inactive
/// status, then error 201 with the server's reason, then the status once
/// more, as the reference (every Rejected report of the four-leg recordings of 26/09
/// to 02/10/2026, ib-agent captures/four-leg). The server refuses FOK on
/// this route (ib-agent#192 C7).
fn server_reject(paper: &mut Paper, id: i64) {
    println!("  server reject (order {})", id);
    let order = Order {
        action: "BUY".into(), order_type: "LMT".into(), total_quantity: 1.0, lmt_price: 200.0,
        tif: "FOK".into(), ..Default::default()
    };
    paper.place(id, &order);
    let error_key = format!("error:{}:201", id);
    let done = paper.pump(20, |s| last_status(s, id).as_deref() == Some("Inactive")
        && s.sequence.iter().any(|e| *e == error_key));
    let s = paper.state.lock().unwrap();
    let error = s.sequence.iter().position(|e| *e == error_key);
    let status = s.sequence.iter().position(|e| *e == format!("status:{}:Inactive", id));
    let reason_ok = s.errors.iter().any(|(r, c, m)| *r == id && *c == 201 && m.starts_with("Order rejected - reason:") && m.len() > 24);
    drop(s);
    paper.check(done, "server reject: status Inactive and error 201");
    paper.check(reason_ok, "server reject: error 201 with the server's reason");
    paper.check(status.is_some() && status < error, "server reject: the status before the error");
}

/// ibx#486: an order on a contract given without a conId: its contract is
/// looked up by symbol first (a contract lookup by symbol before the new order),
/// the order goes out with the conId found, and the server takes it.
///
/// Only the frames after the place call are read, each parsed: the new order
/// is found by its API order id (its client id is the server id of the
/// order, ibx#466), the lookup by its message type, request name and
/// symbol, its answer by the lookup's request id.
fn order_by_symbol(paper: &mut Paper, id: i64) {
    println!("  order by symbol (order {})", id);
    let by_symbol = Contract { con_id: 0, ..aapl() };
    let order = Order {
        action: "BUY".into(), order_type: "LMT".into(), total_quantity: 1.0, lmt_price: 100.0,
        tif: "DAY".into(), ..Default::default()
    };
    let start = wire().lines.lock().unwrap().len();
    paper.placed.push(id);
    if let Err(e) = paper.client.place_order(id, &by_symbol, &order) {
        paper.fail(&format!("order {}: place_order returned {}", id, e));
    }
    let done = paper.pump(20, |s| working(s, id));
    paper.check(done, "order by symbol: working");
    let lines = wire().lines.lock().unwrap()[start..].to_vec();
    for (ok, what) in order_by_symbol_checks(&lines, id, "AAPL", "265598") {
        paper.check(ok, &format!("order by symbol: {}", what));
    }
}

/// ibx#738: an order whose contract's lookup finds nothing gets 200, is
/// not listed by reqOpenOrders, and its cancel gives 10147, as the
/// reference 10.51 (paper, 17/10/2026, market closed).
fn refused_at_lookup(paper: &mut Paper, id: i64) {
    println!("  refused at its lookup (order {})", id);
    let nowhere = Contract { con_id: 0, symbol: "ZZQXJV".into(), ..aapl() };
    let order = Order {
        action: "BUY".into(), order_type: "LMT".into(), total_quantity: 1.0, lmt_price: 50.0,
        tif: "DAY".into(), ..Default::default()
    };
    paper.placed.push(id);
    if let Err(e) = paper.client.place_order(id, &nowhere, &order) {
        paper.fail(&format!("order {}: place_order returned {}", id, e));
    }
    let got = paper.pump(20, |s| s.errors.iter().any(|(r, c, _)| *r == id && *c == 200));
    paper.check(got, "refused at its lookup: error 200");
    let ends = paper.state.lock().unwrap().open_ends;
    paper.client.req_open_orders(&mut paper.probe);
    paper.pump(10, |s| s.open_ends > ends);
    let listed = paper.state.lock().unwrap().open_rows.contains(&id);
    paper.check(!listed, "refused at its lookup: not listed by reqOpenOrders");
    let _ = paper.client.cancel_order(id, "");
    let gone = paper.pump(10, |s| s.errors.iter().any(|(r, c, _)| *r == id && *c == 10147));
    paper.check(gone, "refused at its lookup: its cancel gives 10147");
}

/// The wire checks of [`order_by_symbol`] on the frames logged after the
/// place call: one lookup by symbol, its answer, then the order's new order message with
/// the conId found.
fn order_by_symbol_checks(lines: &[String], id: i64, symbol: &str, con_id: &str) -> Vec<(bool, String)> {
    let lines: Vec<(bool, Frame)> = lines.iter()
        .filter(|l| l.starts_with("WIRE"))
        .map(|l| (l.starts_with("WIRE>"), parse_frame(l))).collect();
    let lookups: Vec<usize> = lines.iter().enumerate()
        .filter(|(_, (out, f))| *out && field(f, 35) == Some("c") && field(f, 55) == Some(symbol)
            && field(f, 320).is_some_and(|r| r.starts_with("FixSecDefReqBySymbol")))
        .map(|(n, _)| n).collect();
    let api_id = |f: &Frame| field(f, 6121).and_then(|v| v.parse::<i64>().ok());
    let new_order = lines.iter().position(|(out, f)| *out && field(f, 35) == Some("D") && api_id(f) == Some(id));
    let answer = lookups.first().and_then(|&n| field(&lines[n].1, 320).map(str::to_string)).and_then(|rid| {
        lines.iter().position(|(out, f)| !*out && field(f, 35) == Some("d") && field(f, 320) == Some(rid.as_str()))
    });
    vec![
        (lookups.len() == 1, format!("one lookup by symbol ({} sent)", lookups.len())),
        (new_order.is_some(), "the 35=D sent".to_string()),
        (lookups.first().is_some_and(|&l| Some(l) < new_order) && answer.is_some() && answer < new_order,
            "the lookup and its answer before the 35=D".to_string()),
        (new_order.is_some_and(|n| field(&lines[n].1, 6008) == Some(con_id)), "the conId found on the 35=D".to_string()),
    ]
}

// The checks of the order by symbol on the frame shapes the engine logs
// (offline): the new order is found by its API order id, its client id
// being the server id of the order (ibx#466).
#[test]
fn order_by_symbol_checks_read_the_logged_frames() {
    let id = 612_i64;
    let lookup = "WIRE> seq=7 8=FIX.4.1|9=0120|35=c|34=000007|52=x|320=FixSecDefReqBySymbol3221225472|321=2|6088=Socket|55=AAPL|167=CS|100=BEST|15=USD|10=250|";
    let answer = "WIRE< ccp/fix 8=FIX.4.1|9=0100|35=d|34=000900|320=FixSecDefReqBySymbol3221225472|322=*|323=4|55=AAPL|167=STK|6008=265598|10=000|";
    let order = format!("WIRE> seq=9 8=FIX.4.1|9=0215|35=D|34=000009|52=x|11=1288736441.0|44=100.00|1=DU1|6122=c|6121={id}|6119=0|38=1|40=2|55=AAPL|167=STK|54=1|59=0|100=BEST|6210=BEST|6008=265598|6088=Socket|15=USD|10=096|");
    let ok = |lines: Vec<String>| order_by_symbol_checks(&lines, id, "AAPL", "265598").iter().all(|(ok, _)| *ok);
    assert!(ok(vec!["log line".into(), lookup.into(), answer.into(), order.clone()]));
    // The new order first, no answer, no conId, two lookups: each fails.
    assert!(!ok(vec![order.clone(), lookup.into(), answer.into()]));
    assert!(!ok(vec![lookup.into(), order.clone()]));
    assert!(!ok(vec![lookup.into(), answer.into(), order.replace("|6008=265598", "")]));
    assert!(!ok(vec![lookup.into(), lookup.into(), answer.into(), order.clone()]));
    assert!(!ok(vec![lookup.into(), answer.into(), order.replace(&format!("6121={id}"), "6121=5")]));
}

/// ibx#509: a bracket in the client library's usual form, on a contract
/// given by symbol: the parent and the take-profit with transmit off send
/// no order and get no answer; the stop with transmit on sends the three
/// together, the parent first, and the server takes them (the children
/// name their parent).
fn bracket_with_transmit_off(paper: &mut Paper, id: i64) {
    println!("  bracket with transmit off (orders {}..{})", id, id + 2);
    let by_symbol = Contract { con_id: 0, ..aapl() };
    let order = |action: &str, order_type: &str, lmt: f64, aux: f64, transmit: bool, parent_id: i64| Order {
        action: action.into(), order_type: order_type.into(), total_quantity: 1.0, lmt_price: lmt, aux_price: aux,
        tif: "DAY".into(), transmit, parent_id, ..Default::default()
    };
    let new_orders = |from: usize| -> Vec<i64> {
        wire().lines.lock().unwrap()[from..].iter().filter(|l| l.starts_with("WIRE>")).map(|l| parse_frame(l))
            .filter(|f| field(f, 35) == Some("D"))
            .filter_map(|f| field(&f, 6121).and_then(|v| v.parse().ok())).collect()
    };
    let start = wire().lines.lock().unwrap().len();
    for (oid, held) in [(id, order("BUY", "LMT", 100.0, 0.0, false, 0)), (id + 1, order("SELL", "LMT", 400.0, 0.0, false, id))] {
        paper.placed.push(oid);
        if let Err(e) = paper.client.place_order(oid, &by_symbol, &held) {
            paper.fail(&format!("order {}: place_order returned {}", oid, e));
        }
    }
    paper.pump(4, |_| false);
    let answered = {
        let s = paper.state.lock().unwrap();
        s.statuses.iter().any(|(i, _)| (id..id + 2).contains(i)) || s.errors.iter().any(|(i, ..)| (id..id + 2).contains(i))
    };
    paper.check(new_orders(start).is_empty(), "bracket with transmit off: the held orders are not sent");
    paper.check(!answered, "bracket with transmit off: the held orders get no answer");
    paper.placed.push(id + 2);
    if let Err(e) = paper.client.place_order(id + 2, &by_symbol, &order("SELL", "STP", 0.0, 50.0, true, id)) {
        paper.fail(&format!("order {}: place_order returned {}", id + 2, e));
    }
    let all = paper.wait_working(&[id, id + 1, id + 2]);
    paper.check(all, "bracket with transmit off: the three orders are working");
    paper.check(new_orders(start) == [id, id + 1, id + 2], "bracket with transmit off: sent together, the parent first");
    // The parent's cancel ends the three.
    let _ = paper.client.cancel_order(id, "");
    let ended = paper.pump(15, |s| (id..id + 3).all(|i| last_status(s, i).as_deref() == Some("Cancelled")));
    paper.check(ended, "bracket with transmit off: the parent's cancel ends the three");
}

/// ibx#545 (the reference on paper, 09/10/2026): a global cancel answers
/// 161 for an order cancelled a few seconds before, and for an order that
/// was never sent each time; once its time in the reference's book is
/// over, the cancelled order gets none.
fn global_cancel_after_ended_orders(paper: &mut Paper, id: i64) {
    println!("  global cancel after ended orders (orders {}, {})", id, id + 1);
    // An order id below the highest one used is refused: the held order first.
    let (held, sent) = (id, id + 1);
    let order = |transmit: bool| Order {
        action: "BUY".into(), order_type: "LMT".into(), total_quantity: 1.0, lmt_price: 100.0,
        tif: "DAY".into(), transmit, ..Default::default()
    };
    let refusals = |s: &State, from: usize| -> Vec<i64> {
        // The orders of this step only: the global cancel also ends the
        // orders the steps before left working.
        s.errors[from..].iter().filter(|(i, c, _)| *c == 161 && (id..id + 2).contains(i)).map(|(i, ..)| *i).collect()
    };
    // Held, then cancelled: an order that was never sent.
    paper.place(held, &order(false));
    paper.pump(2, |_| false);
    let _ = paper.client.cancel_order(held, "");
    paper.pump(10, |s| refusals(s, 0).contains(&held));
    // Sent, working, cancelled.
    paper.place(sent, &order(true));
    let is_working = paper.wait_working(&[sent]);
    paper.check(is_working, "global cancel after ended orders: the order is working");
    let _ = paper.client.cancel_order(sent, "");
    let cancelled = paper.pump(15, |s| last_status(s, sent).as_deref() == Some("Cancelled"));
    let at = Instant::now();
    paper.check(cancelled, "global cancel after ended orders: the order is cancelled");

    paper.pump(3, |_| false);
    let from = paper.state.lock().unwrap().errors.len();
    let _ = paper.client.req_global_cancel();
    paper.pump(3, |_| false);
    let mut got = refusals(&paper.state.lock().unwrap(), from);
    got.sort_unstable();
    paper.check(got == [held, sent], &format!("global cancel 3 s after: 161 for both ended orders ({:?})", got));

    while at.elapsed() < Duration::from_secs(12) {
        paper.pump(1, |_| false);
    }
    let from = paper.state.lock().unwrap().errors.len();
    let _ = paper.client.req_global_cancel();
    paper.pump(3, |_| false);
    let got = refusals(&paper.state.lock().unwrap(), from);
    paper.check(got == [held], &format!("global cancel 12 s after: 161 for the order that was never sent only ({:?})", got));
}

/// ibx#328: every new order carries the contract id after the secondary
/// routing field, as the reference does (ib-agent#192 B4).
fn contract_id_on_new_orders(paper: &mut Paper) {
    let sent: Vec<Frame> = wire().lines.lock().unwrap().iter()
        .filter(|l| l.starts_with("WIRE>"))
        .map(|l| parse_frame(l))
        .filter(|f| field(f, 35) == Some("D"))
        .collect();
    let missing = sent.iter().filter(|f| {
        let i = f.iter().position(|(t, _)| *t == 6210);
        !i.is_some_and(|i| f.get(i + 1) == Some(&(6008, "265598".to_string())))
    }).count();
    paper.check(!sent.is_empty() && missing == 0,
        &format!("contract id after the routing field on all {} new orders ({} missing)", sent.len(), missing));
}

/// ibx#609 ibx#717 (the reference 10.51, paper, 16/10/2026, market
/// closed): a stop loss and a profit taker asked from the order preset,
/// when the paper user's stock preset does not auto-attach them: 10355
/// naming the profit taker, the status Cancelled, then 202; nothing is
/// sent to the server. Should this step start to fail with a working
/// parent, the stock preset now auto-attaches them: that is the case
/// ibx#717 waits for.
fn attached_from_preset(paper: &mut Paper, id: i64) {
    println!("  attached orders from the preset (order {})", id);
    let order = Order {
        action: "BUY".into(), order_type: "LMT".into(), total_quantity: 1.0, lmt_price: 100.0,
        tif: "DAY".into(), sl_order_id: (id + 1) as i32, sl_order_type: "PRESET".into(),
        pt_order_id: (id + 2) as i32, pt_order_type: "PRESET".into(), ..Default::default()
    };
    let start = wire().lines.lock().unwrap().len();
    paper.place(id, &order);
    let notice = format!("error:{}:202", id);
    let done = paper.pump(20, |s| s.sequence.iter().any(|e| *e == notice) || working(s, id));
    let s = paper.state.lock().unwrap();
    let seq: Vec<String> = s.sequence.iter().filter(|e| e.split(':').nth(1).and_then(|v| v.parse::<i64>().ok())
        .is_some_and(|i| (id..id + 3).contains(&i))).cloned().collect();
    let text_ok = s.errors.iter().any(|(r, c, m)| *r == id && *c == 10355 && m == "Cannot auto-attach Profit Taker. Preset is not defined.");
    drop(s);
    let sent = wire().lines.lock().unwrap()[start..].iter().filter(|l| l.starts_with("WIRE>")).map(|l| parse_frame(l))
        .any(|f| field(&f, 35) == Some("D"));
    paper.check(done, "attached orders from the preset: answered");
    paper.check(seq == [format!("error:{id}:10355"), format!("status:{id}:Cancelled"), notice.clone()],
        &format!("attached orders from the preset: 10355, Cancelled, 202 ({seq:?})"));
    paper.check(text_ok, "attached orders from the preset: 10355 names the profit taker");
    paper.check(!sent, "attached orders from the preset: nothing sent");
}

/// ibx#743, ibx#744, ibx#745 (the reference 10.51, paper, 18/10/2026,
/// market closed): AAPL by conId on a made-up exchange gets 200 and is
/// gone (not listed, its cancel 10147); a stock combo with outside RTH
/// gets 2109 and is sent; PEG STK on AAPL gets 387 and is kept (its cancel
/// gives Cancelled, 202 and 161).
fn refusals_of_10_51(paper: &mut Paper, id: i64) {
    println!("  refusals of 10.51 (orders {}..{})", id, id + 2);
    let lmt = Order {
        action: "BUY".into(), order_type: "LMT".into(), total_quantity: 1.0, lmt_price: 50.0,
        tif: "DAY".into(), ..Default::default()
    };
    let nowhere = Contract { exchange: "XYZ".into(), ..aapl() };
    paper.placed.push(id);
    if let Err(e) = paper.client.place_order(id, &nowhere, &lmt) {
        paper.fail(&format!("order {}: place_order returned {}", id, e));
    }
    let got = paper.pump(20, |s| s.errors.iter().any(|(r, c, _)| *r == id && *c == 200));
    paper.check(got, "an exchange without a definition: error 200");
    let ends = paper.state.lock().unwrap().open_ends;
    paper.client.req_open_orders(&mut paper.probe);
    paper.pump(10, |s| s.open_ends > ends);
    let listed = paper.state.lock().unwrap().open_rows.contains(&id);
    paper.check(!listed, "an exchange without a definition: not listed by reqOpenOrders");
    let _ = paper.client.cancel_order(id, "");
    let gone = paper.pump(10, |s| s.errors.iter().any(|(r, c, _)| *r == id && *c == 10147));
    paper.check(gone, "an exchange without a definition: its cancel gives 10147");

    let leg = |con_id, action: &str| ibx::api::types::ComboLeg {
        con_id, ratio: 1, action: action.into(), exchange: "SMART".into(), ..Default::default()
    };
    let combo = Contract {
        symbol: "AAPL,MSFT".into(), sec_type: "BAG".into(), exchange: "SMART".into(), currency: "USD".into(),
        combo_legs: vec![leg(265598, "BUY"), leg(272093, "SELL")], ..Default::default()
    };
    let combo_order = Order {
        lmt_price: -500.0, outside_rth: true,
        smart_combo_routing_params: vec![TagValue { tag: "NonGuaranteed".into(), value: "1".into() }],
        ..lmt.clone()
    };
    paper.placed.push(id + 1);
    if let Err(e) = paper.client.place_order(id + 1, &combo, &combo_order) {
        paper.fail(&format!("order {}: place_order returned {}", id + 1, e));
    }
    let warned = paper.pump(20, |s| s.errors.iter().any(|(r, c, _)| *r == id + 1 && *c == 2109));
    paper.check(warned, "a combo with outside RTH: warning 2109");
    // The warning is no refusal: the order goes on.
    let sent = paper.pump(20, |s| working(s, id + 1));
    paper.check(sent, "a combo with outside RTH: sent and working");

    let peg_stk = Order { order_type: "PEG STK".into(), lmt_price: 0.0, ..lmt };
    paper.place(id + 2, &peg_stk);
    let got = paper.pump(20, |s| s.errors.iter().any(|(r, c, _)| *r == id + 2 && *c == 387));
    paper.check(got, "PEG STK on a stock: error 387");
    let _ = paper.client.cancel_order(id + 2, "");
    let ended = paper.pump(10, |s| s.errors.iter().any(|(r, c, _)| *r == id + 2 && *c == 161));
    let s = paper.state.lock().unwrap();
    let seq: Vec<String> = s.sequence.iter().filter(|e| e.split(':').nth(1) == Some((id + 2).to_string().as_str())).cloned().collect();
    drop(s);
    paper.check(ended && seq == [format!("error:{}:387", id + 2), format!("status:{}:Cancelled", id + 2),
        format!("error:{}:202", id + 2), format!("error:{}:161", id + 2)],
        &format!("PEG STK on a stock: kept, its cancel gives Cancelled, 202, 161 ({seq:?})"));
}

/// ibx#673 (the reference 10.51, paper, regular hours 12/11/2026): a
/// volatility order on a stock goes out as a limit order priced from the
/// quote the session holds; with no quote held, without a price, and the
/// server refuses it: Inactive, 201 naming the missing price, shown as a
/// limit order.
fn volatility_on_a_stock(paper: &mut Paper, id: i64) {
    println!("  volatility order on a stock with no quote held (order {})", id);
    let order = Order {
        action: "BUY".into(), order_type: "VOL".into(), total_quantity: 1.0, tif: "DAY".into(),
        volatility: 30.0, volatility_type: 2, ..Default::default()
    };
    let msft = Contract { con_id: 272093, exchange: "SMART".into(), ..Default::default() };
    paper.placed.push(id);
    if let Err(e) = paper.client.place_order(id, &msft, &order) {
        paper.fail(&format!("order {}: place_order returned {}", id, e));
    }
    let error_key = format!("error:{}:201", id);
    let done = paper.pump(20, |s| last_status(s, id).as_deref() == Some("Inactive") && s.sequence.iter().any(|e| *e == error_key));
    let reason_ok = paper.state.lock().unwrap().errors.iter()
        .any(|(r, c, m)| *r == id && *c == 201 && m == "Order rejected - reason:Message must contain field # 44");
    let sent = new_orders(id);
    paper.check(done, "volatility order on a stock: Inactive and error 201");
    paper.check(reason_ok, "volatility order on a stock: the server names the missing price");
    paper.check(sent.len() == 1 && field(&sent[0], 40) == Some("2") && field(&sent[0], 44).is_none(),
        "volatility order on a stock: sent as a limit order without a price");
}

/// ibx#751 (the reference 10.51, paper, 18/10/2026 market closed and
/// 12/11/2026 regular hours): a scale order with a profit offset goes out
/// with its profit taker in one write; the profit taker is a child under a
/// negative order id, PreSubmitted, and the order's cancel ends the order.
fn scale_with_profit_taker(paper: &mut Paper, id: i64) {
    println!("  scale order with a profit taker (order {})", id);
    let order = Order {
        action: "BUY".into(), order_type: "LMT".into(), total_quantity: 2.0, lmt_price: 50.0, tif: "DAY".into(),
        scale_init_level_size: 1, scale_subs_level_size: 1, scale_price_increment: 0.5, scale_profit_offset: 0.5,
        ..Default::default()
    };
    let start = wire().lines.lock().unwrap().len();
    paper.place(id, &order);
    let up = paper.pump(20, |s| working(s, id) && s.statuses.iter().any(|(i, st)| *i < 0 && st == "PreSubmitted"));
    paper.check(up, "scale order: working, its profit taker PreSubmitted under a negative id");
    let child = paper.state.lock().unwrap().statuses.iter().rev().find(|(i, _)| *i < 0).map(|(i, _)| *i);
    let pair: Vec<Frame> = wire().lines.lock().unwrap()[start..].iter().filter(|l| l.starts_with("WIRE>")).map(|l| parse_frame(l))
        .filter(|f| field(f, 35) == Some("D")).collect();
    paper.check(pair.len() == 2 && field(&pair[1], 6121) == child.map(|c| c.to_string()).as_deref(),
        &format!("scale order: the order then its profit taker {child:?} sent"));
    let _ = paper.client.cancel_order(id, "");
    let ended = paper.pump(20, |s| last_status(s, id).as_deref() == Some("Cancelled"));
    paper.check(ended, "scale order: cancelled");
    if let Some(child) = child {
        // The profit taker of an order that never traded ends with it.
        paper.placed.push(child);
        let gone = paper.pump(10, |s| last_status(s, child).as_deref() == Some("Cancelled"));
        if !gone {
            let _ = paper.client.cancel_order(child, "");
            paper.pump(10, |s| last_status(s, child).as_deref() == Some("Cancelled"));
        }
    }
}

#[test]
#[ignore = "live: logs in to the paper account (IB_USERNAME / IB_PASSWORD)"]
fn order_paths_paper() {
    wire();
    let config = get_config().expect("IB_USERNAME / IB_PASSWORD not set: a live test fails without credentials");
    println!("=== Order paths (paper account) ===");
    let client = EClient::connect(&config).expect("connect to paper failed");
    // Place no order unless this is a paper account (id starts with DU).
    if !client.account_id.starts_with("DU") {
        client.disconnect();
        panic!("refusing to run: the logged-in account is not a paper account (its id does not start with DU)");
    }
    let state = Arc::new(Mutex::new(State::default()));
    let mut paper = Paper {
        client, probe: Probe { state: state.clone() }, state, placed: Vec::new(), failures: Vec::new(),
    };
    // The client's next valid id: the ids above it are new (ibx#466).
    let base = paper.client.next_order_id();

    adjustable_stop_brackets(&mut paper, base);
    modifies(&mut paper, base + 100);
    fractional(&mut paper, base + 200);
    algos(&mut paper, base + 300);
    conditions(&mut paper, base + 400);
    server_reject(&mut paper, base + 500);
    contract_id_on_new_orders(&mut paper);
    order_by_symbol(&mut paper, base + 600);
    bracket_with_transmit_off(&mut paper, base + 700);
    global_cancel_after_ended_orders(&mut paper, base + 800);
    attached_from_preset(&mut paper, base + 900);
    refused_at_lookup(&mut paper, base + 1000);
    refusals_of_10_51(&mut paper, base + 1100);
    volatility_on_a_stock(&mut paper, base + 1200);
    scale_with_profit_taker(&mut paper, base + 1300);

    println!("  cleanup: cancelling every order still working");
    // A modified order is placed twice under one id: cancel it once.
    paper.placed.sort_unstable();
    paper.placed.dedup();
    let open: Vec<i64> = {
        let s = paper.state.lock().unwrap();
        paper.placed.iter().copied().filter(|&i| working(&s, i)).collect()
    };
    for id in open {
        let _ = paper.client.cancel_order(id, "");
    }
    paper.pump(8, |_| false);
    paper.client.disconnect();

    // IBX_WIRE_DUMP=<file> writes every captured frame, to look into a failure.
    if let Ok(path) = env::var("IBX_WIRE_DUMP") {
        let _ = std::fs::write(&path, wire().lines.lock().unwrap().join("\n"));
        println!("  wire frames written to {}", path);
    }

    println!("\n=== {} failure(s) ===", paper.failures.len());
    for f in &paper.failures {
        println!("  - {}", f);
    }
    assert!(paper.failures.is_empty(), "{} order-path check(s) failed", paper.failures.len());
}
