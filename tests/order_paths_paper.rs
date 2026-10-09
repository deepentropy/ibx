//! Order-path tests against the real server on the paper account, through `EClient`.
//!
//! Covers the order paths fixed in ibx#240 ibx#225 ibx#247 ibx#324 ibx#334
//! ibx#339 ibx#349 ibx#313 ibx#318 ibx#405 ibx#325 ibx#327 ibx#250 ibx#328 ibx#263. Each case checks
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
    // "WIRE> seq=N 8=FIX..|..." or "WIRE< ccp/comp 8=FIX..|..."
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

/// The new orders sent for the API order id `id` (6121, ibx#466).
fn new_orders(id: i64) -> Vec<Frame> {
    let id = id.to_string();
    wire().lines.lock().unwrap().iter()
        .filter(|l| l.starts_with("WIRE>"))
        .map(|l| parse_frame(l))
        .filter(|f| field(f, 35) == Some("D") && field(f, 6121) == Some(id.as_str()))
        .collect()
}

/// The ClOrdID of the order of API order id `id` at `version`: the server
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

/// 399 is an informational warning (order held until the open), not a refusal.
fn refused(s: &State, id: i64) -> bool {
    s.errors.iter().any(|(r, c, _)| *r == id && *c != 399)
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
    Some(EClientConfig { username, password, host, paper: true, core_id: None })
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
        action: "SELL".into(), order_type: "STP".into(), total_quantity: 1.0, aux_price: aux, ..Default::default()
    };
    let stp_lmt = |l: f64, aux: f64| Order {
        action: "SELL".into(), order_type: "STP LMT".into(), total_quantity: 1.0,
        lmt_price: l, aux_price: aux, ..Default::default()
    };
    let trail = |aux: f64| Order {
        action: "SELL".into(), order_type: "TRAIL".into(), total_quantity: 1.0, aux_price: aux, ..Default::default()
    };
    let trail_lmt = |aux: f64| Order {
        action: "SELL".into(), order_type: "TRAIL LIMIT".into(), total_quantity: 1.0,
        aux_price: aux, lmt_price_offset: 0.50, trail_stop_price: 50.0, ..Default::default()
    };
    let trail_percent = |p: f64| Order {
        action: "SELL".into(), order_type: "TRAIL".into(), total_quantity: 1.0, trailing_percent: p, ..Default::default()
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
        let err = s.errors.iter().any(|(r, c, _)| *r == id && *c != 399);
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
        action: "BUY".into(), order_type: "LMT".into(), total_quantity: 1.5, lmt_price: 200.0, ..Default::default()
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
        ..Default::default()
    };
    let twap = Order {
        action: "SELL".into(), order_type: "LMT".into(), total_quantity: 1.0, lmt_price: 5000.0,
        algo_strategy: "Twap".into(),
        algo_params: vec![TagValue { tag: "allowPastEndTime".into(), value: "1".into() }],
        ..Default::default()
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
        ..Default::default()
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
            ..Default::default()
        };
        paper.place(id, &order);
        let up = paper.wait_working(&[id]);
        paper.check(up, &format!("{}: accepted", label));
        let ack = reply(&clord_of(id, 0), &["0", "A"]);
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
/// more, as the reference (every 39=8 of the four-leg recordings of 26/09
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
/// looked up by symbol first (35=c FixSecDefReqBySymbol before the 35=D),
/// the order goes out with the conId found, and the server takes it.
///
/// Only the frames after the place call are read, each parsed: the 35=D
/// is found by its API order id (6121; its ClOrdID is the server id of the
/// order, ibx#466), the lookup by its message type, request name and
/// symbol, its answer by the lookup's 320.
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

/// The wire checks of [`order_by_symbol`] on the frames logged after the
/// place call: one lookup by symbol, its answer, then the order's 35=D with
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
// (offline): the 35=D is found by its API order id (6121), its ClOrdID
// being the server id of the order (ibx#466).
#[test]
fn order_by_symbol_checks_read_the_logged_frames() {
    let id = 612_i64;
    let lookup = "WIRE> seq=7 8=FIX.4.1|9=0120|35=c|34=000007|52=x|320=FixSecDefReqBySymbol3221225472|321=2|6088=Socket|55=AAPL|167=CS|100=BEST|15=USD|10=250|";
    let answer = "WIRE< ccp/fix 8=FIX.4.1|9=0100|35=d|34=000900|320=FixSecDefReqBySymbol3221225472|322=*|323=4|55=AAPL|167=STK|6008=265598|10=000|";
    let order = format!("WIRE> seq=9 8=FIX.4.1|9=0215|35=D|34=000009|52=x|11=1288736441.0|44=100.00|1=DU1|6122=c|6121={id}|6119=0|38=1|40=2|55=AAPL|167=STK|54=1|59=0|100=BEST|6210=BEST|6008=265598|6088=Socket|15=USD|10=096|");
    let ok = |lines: Vec<String>| order_by_symbol_checks(&lines, id, "AAPL", "265598").iter().all(|(ok, _)| *ok);
    assert!(ok(vec!["log line".into(), lookup.into(), answer.into(), order.clone()]));
    // The 35=D first, no answer, no conId, two lookups: each fails.
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
