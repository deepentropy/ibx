//! A replay session: the engine on in-memory links to a scripted farm,
//! auth server and historical farm, driven one step at a time from the
//! test thread, so the callbacks of each recorded frame are known. The API
//! client on top is the Rust one ([`Session`]) or any other through
//! [`super::Driver`] (the Python one).

use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crossbeam_channel::Sender;

use crate::api::client::EClient;
use crate::api::types::{
    BarData, CommissionAndFeesReport, Contract, ContractDetails, Execution, Order, OrderState, TickAttrib,
};
use crate::api::wrapper::Wrapper;
use crate::bridge::SharedState;
use crate::engine::hot_loop::HotLoop;
use crate::test_support::{parse_fields, Fields, Peer};
use crate::types::ControlCommand;

use super::record::{attr_mask, n, order_price, perm, OPEN_ORDER_FIELDS};
use super::runner::Link;

pub const ACCOUNT: &str = "DUXXXXXXX";

/// The zone of the machine the recordings were made on, when a fixture
/// header does not name it (`machine_zone`): every capture so far.
pub const RECORDING_ZONE: &str = "Europe/Paris";

/// The machine zone a recording names (`machine_zone` in its header), else
/// [`RECORDING_ZONE`]: a replay runs in it, whatever the zone of the
/// machine that runs the test (a time without a zone is read in it).
pub fn zone_of(header: &serde_json::Value) -> &str {
    header["machine_zone"].as_str().unwrap_or(RECORDING_ZONE)
}

/// Makes bad copies of a server frame (the robustness tests, ibx#488).
pub type BadCopies = Box<dyn FnMut(&[u8]) -> Vec<Vec<u8>>>;

thread_local! {
    /// When set, each server frame a replay sends is preceded by the bad
    /// copies this makes of it, given straight to the engine's handler:
    /// the replay then runs on with the bad input in between.
    pub static BAD_COPIES: std::cell::RefCell<Option<BadCopies>> = const { std::cell::RefCell::new(None) };
}

/// Steps of the engine after each input: enough for a request to reach the
/// wire and for its reply to reach the queues.
const SETTLE_STEPS: usize = 4;

/// The engine and its three links, with every message it sent on each.
pub struct Links {
    pub engine: HotLoop,
    pub shared: Arc<SharedState>,
    /// The engine's command channel, for an API client to take.
    pub control_tx: Sender<ControlCommand>,
    pub farm: Peer,
    pub ccp: Peer,
    pub hmds: Peer,
    /// Every message the engine sent on each link, in order.
    pub farm_out: Vec<Fields>,
    pub ccp_out: Vec<Fields>,
    pub hmds_out: Vec<Fields>,
}

impl Links {
    pub fn new() -> Self {
        // The engine runs on this thread in the recording machine's zone; a
        // replay of another zone sets its own (`in_zone`).
        crate::gateway::set_machine_zone_for_test(Some(RECORDING_ZONE));
        let shared = Arc::new(SharedState::new());
        // Reads return at once: the test steps the engine itself.
        let pair = || {
            let (conn, mut peer) = Peer::pair();
            conn.set_mem_read_timeout(std::time::Duration::ZERO);
            peer.conn().set_mem_read_timeout(std::time::Duration::ZERO);
            (conn, peer)
        };
        let (farm_conn, farm) = pair();
        let (ccp_conn, ccp) = pair();
        let (hmds_conn, hmds) = pair();
        let (mut engine, control_tx) = HotLoop::with_connections(
            shared.clone(), None, ACCOUNT.into(), farm_conn, ccp_conn, Some(hmds_conn), None);
        // The paper logon of the recordings (captures/0928, 28/09/2026):
        // features PRICEMGMT and SCALEUSLOT, 6247=demo, 8146 exclusions.
        engine.set_scale_us_lots(true);
        engine.set_user_book(true);
        engine.set_price_mgmt(true, Some("*/CMDTY;*/CRYPTO;*/FUND;*/IOPT;*/SLB"));
        // Its smart combo conIds (6611).
        shared.reference.set_smart_combo_con_ids(concat!(
            "AUD:61227077,BRL:136000438,CAD:61227082,CHF:61227087,CNH:136000441,DKK:136000423,EUR:58666491,",
            "GBP:58666494,HKD:61227072,INR:136000444,JPY:61227069,KRW:136000424,MXN:136000449,NOK:136000452,",
            "NZD:136000435,SEK:136000429,USD:28812380",
        ));
        Self {
            engine, shared, control_tx, farm, ccp, hmds,
            farm_out: Vec::new(), ccp_out: Vec::new(), hmds_out: Vec::new(),
        }
    }

    /// Run the engine a few steps and read what it sent.
    pub fn step(&mut self) {
        for _ in 0..SETTLE_STEPS {
            self.engine.step_for_test();
        }
        self.farm_out.extend(self.farm.messages().iter().map(|m| parse_fields(m)));
        self.ccp_out.extend(self.ccp.messages().iter().map(|m| parse_fields(m)));
        self.hmds_out.extend(self.hmds.messages().iter().map(|m| parse_fields(m)));
    }

    /// The bad copies [`BAD_COPIES`] makes of a server frame, handed to the
    /// engine's message handler of `link` (a compressed copy opened first).
    /// A panic names the copy.
    pub fn give_bad_copies(&mut self, raw: &[u8], link: Link) {
        let copies = BAD_COPIES.with(|hook| hook.borrow_mut().as_mut().map(|make| make(raw))).unwrap_or_default();
        for copy in copies {
            let msgs = if copy.starts_with(b"8=FIXCOMP\x01") {
                crate::protocol::fixcomp::fixcomp_decompress(&copy).unwrap_or_default()
            } else {
                vec![copy]
            };
            for m in msgs {
                let engine = &mut self.engine;
                let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match link {
                    Link::Farm => engine.inject_farm_message(&m),
                    Link::Ccp => engine.inject_ccp_message(&m),
                    Link::Hmds => engine.inject_hmds_message(&m),
                }));
                if let Err(e) = run {
                    let text = e.downcast_ref::<String>().cloned()
                        .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default();
                    panic!("{link:?} handler panicked ({text}) on {}", crate::protocol::fix::fmt_pipe(&m));
                }
            }
        }
    }

    /// [`Self::give_bad_copies`] of a message for the historical link,
    /// when the hook is set.
    pub fn give_bad_hmds_copies(&mut self, fields: &Fields) {
        if BAD_COPIES.with(|hook| hook.borrow().is_some()) {
            let f: Vec<(u32, &str)> = fields.iter().filter(|(t, _)| !matches!(t, 8 | 9 | 10 | 34))
                .map(|(t, v)| (*t, v.as_str())).collect();
            self.give_bad_copies(&crate::protocol::fix::fix_build(&f, 0), Link::Hmds);
        }
    }

    /// Run in the machine zone of a recording ([`zone_of`]): this thread
    /// and the threads it lends the engine or the client to.
    pub fn in_zone(self, header: &serde_json::Value) -> Self {
        crate::gateway::set_machine_zone_for_test(Some(zone_of(header)));
        self
    }

    /// Run `f` on this thread while the engine runs on another (a client
    /// call may wait for the engine's answer); then the engine settles.
    /// The engine's thread reads the machine zone of this one.
    pub fn during<R>(&mut self, f: impl FnOnce() -> R) -> R {
        let stop = AtomicBool::new(false);
        let engine = &mut self.engine;
        let zone = crate::gateway::machine_zone_for_test();
        let out = std::thread::scope(|s| {
            s.spawn(|| {
                crate::gateway::set_machine_zone_for_test(zone.as_deref());
                while !stop.load(Ordering::Acquire) {
                    engine.step_for_test();
                    std::thread::yield_now();
                }
            });
            let out = f();
            stop.store(true, Ordering::Release);
            out
        });
        self.step();
        out
    }
}

impl Default for Links {
    fn default() -> Self {
        Self::new()
    }
}

/// The links with the Rust API client on top, its callbacks as lines.
pub struct Session {
    pub links: Links,
    pub client: EClient,
    /// The callbacks ibx gave, one line each (see [`Recorder`]).
    pub callbacks: Vec<String>,
    /// The client reads its callbacks after each input (the default); when
    /// false only at [`Session::read`], as a client that reads slower than
    /// the messages come (ibx#446).
    pub read_each: bool,
}

impl Deref for Session {
    type Target = Links;
    fn deref(&self) -> &Links {
        &self.links
    }
}

impl DerefMut for Session {
    fn deref_mut(&mut self) -> &mut Links {
        &mut self.links
    }
}

impl Session {
    pub fn new() -> Self {
        let links = Links::new();
        let client = EClient::from_parts(links.shared.clone(), links.control_tx.clone(), std::thread::spawn(|| {}), ACCOUNT.into());
        Self { links, client, callbacks: Vec::new(), read_each: true }
    }

    /// Run in the machine zone a fixture names (see [`Links::in_zone`]).
    pub fn in_zone(mut self, header: &serde_json::Value) -> Self {
        self.links = self.links.in_zone(header);
        self
    }

    /// Call the API client; the engine runs on this thread meanwhile (a
    /// call may wait for the engine's answer). Then the engine settles and
    /// the callbacks are taken.
    pub fn call<R: Send>(&mut self, f: impl FnOnce(&EClient) -> R + Send) -> R {
        let Self { links, client, .. } = self;
        let client = &*client;
        // The client's thread reads the machine zone of this one.
        let zone = crate::gateway::machine_zone_for_test();
        let out = std::thread::scope(|s| {
            let h = s.spawn(move || {
                crate::gateway::set_machine_zone_for_test(zone.as_deref());
                f(client)
            });
            while !h.is_finished() {
                links.engine.step_for_test();
                std::thread::yield_now();
            }
            h.join().unwrap()
        });
        self.settle();
        out
    }

    /// Run the engine a few steps, read what it sent, and take the callbacks
    /// (with `read_each` off, only the engine's steps).
    pub fn settle(&mut self) {
        self.links.step();
        if self.read_each {
            self.read();
        }
    }

    /// Take the callbacks: one dispatch of the client.
    pub fn read(&mut self) {
        let mut rec = Recorder::default();
        self.client.process_msgs(&mut rec);
        self.callbacks.extend(rec.lines);
        // What the dispatch sent to the engine (a snapshot's cancel).
        self.links.step();
    }

    pub fn send_farm(&mut self, raw: &[u8]) {
        self.links.give_bad_copies(raw, Link::Farm);
        self.links.farm.send_raw(raw);
        self.settle();
    }

    pub fn send_ccp(&mut self, raw: &[u8]) {
        self.links.give_bad_copies(raw, Link::Ccp);
        self.links.ccp.send_raw(raw);
        self.settle();
    }

    /// A message to the engine on the historical link, compressed as the
    /// farm sends it.
    pub fn send_hmds_message(&mut self, fields: &Fields) {
        self.links.give_bad_hmds_copies(fields);
        send_hmds(&mut self.links.hmds, fields);
        self.settle();
    }
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

/// A message on the historical link, compressed as the farm sends it.
pub fn send_hmds(peer: &mut Peer, fields: &Fields) {
    let f: Vec<(u32, &str)> = fields.iter().filter(|(t, _)| !matches!(t, 8 | 9 | 10 | 34))
        .map(|(t, v)| (*t, v.as_str())).collect();
    peer.send_fixcomp(&f);
}

/// The callbacks as one line each, in the form of
/// [`super::record::canonical`].
#[derive(Default)]
pub struct Recorder {
    pub lines: Vec<String>,
}

fn bar_line(name: &str, req_id: i64, b: &BarData) -> String {
    format!(
        "{name}|{req_id}|{}|{}|{}|{}|{}|{}|{}|{}",
        b.date, n(b.open), n(b.high), n(b.low), n(b.close), b.volume, n(b.wap), b.bar_count,
    )
}

impl Wrapper for Recorder {
    fn error(&mut self, req_id: i64, code: i64, text: &str, _: &str) {
        self.lines.push(format!("error|{req_id}|{code}|{text}"));
    }
    fn tick_price(&mut self, req_id: i64, tick_type: i32, price: f64, a: &TickAttrib) {
        self.lines.push(format!("tickPrice|{req_id}|{tick_type}|{}|{}", n(price), attr_mask(a.can_auto_execute, a.past_limit, a.pre_open)));
    }
    fn tick_size(&mut self, req_id: i64, tick_type: i32, size: f64) {
        self.lines.push(format!("tickSize|{req_id}|{tick_type}|{}", n(size)));
    }
    fn tick_string(&mut self, req_id: i64, tick_type: i32, value: &str) {
        self.lines.push(format!("tickString|{req_id}|{tick_type}|{value}"));
    }
    fn tick_generic(&mut self, req_id: i64, tick_type: i32, value: f64) {
        self.lines.push(format!("tickGeneric|{req_id}|{tick_type}|{}", n(value)));
    }
    fn tick_snapshot_end(&mut self, req_id: i64) {
        self.lines.push(format!("tickSnapshotEnd|{req_id}"));
    }
    fn market_data_type(&mut self, req_id: i64, t: i32) {
        self.lines.push(format!("marketDataType|{req_id}|{t}"));
    }
    fn tick_req_params(&mut self, req_id: i64, min_tick: f64, bbo: &str, perms: i64) {
        self.lines.push(format!("tickReqParams|{req_id}|{}|{bbo}|{perms}", n(min_tick)));
    }
    fn account_summary(&mut self, req_id: i64, account: &str, tag: &str, value: &str, currency: &str) {
        self.lines.push(format!("accountSummary|{req_id}|{account}|{tag}|{value}|{currency}"));
    }
    fn account_summary_end(&mut self, req_id: i64) {
        self.lines.push(format!("accountSummaryEnd|{req_id}"));
    }
    fn update_account_value(&mut self, key: &str, value: &str, currency: &str, account: &str) {
        self.lines.push(format!("updateAccountValue|{key}|{value}|{currency}|{account}"));
    }
    fn update_portfolio(
        &mut self, c: &Contract, position: f64, market_price: f64, market_value: f64, average_cost: f64,
        unrealized_pnl: f64, realized_pnl: f64, account: &str,
    ) {
        self.lines.push(format!(
            "updatePortfolio|{}|{}|{}|{}|{}|{}|{}|{}|{account}",
            c.con_id, c.symbol, n(position), n(market_price), n(market_value), n(average_cost), n(unrealized_pnl), n(realized_pnl),
        ));
    }
    fn update_account_time(&mut self, timestamp: &str) {
        self.lines.push(format!("updateAccountTime|{timestamp}"));
    }
    fn account_download_end(&mut self, account: &str) {
        self.lines.push(format!("accountDownloadEnd|{account}"));
    }
    fn position(&mut self, account: &str, c: &Contract, pos: f64, avg_cost: f64) {
        self.lines.push(format!("position|{account}|{}|{}|{}|{}", c.con_id, c.symbol, n(pos), n(avg_cost)));
    }
    fn position_end(&mut self) {
        self.lines.push("positionEnd".into());
    }
    fn pnl(&mut self, req_id: i64, daily: f64, unrealized: f64, realized: f64) {
        self.lines.push(format!("pnl|{req_id}|{}|{}|{}", n(daily), n(unrealized), n(realized)));
    }
    fn pnl_single(&mut self, req_id: i64, pos: f64, daily: f64, unrealized: f64, realized: f64, value: f64) {
        self.lines.push(format!("pnlSingle|{req_id}|{}|{}|{}|{}|{}", n(pos), n(daily), n(unrealized), n(realized), n(value)));
    }
    fn scanner_data(&mut self, req_id: i64, rank: i32, d: &ContractDetails, _: &str, _: &str, _: &str, _: &str) {
        self.lines.push(format!("scannerData|{req_id}|{rank}|{}|{}", d.contract.con_id, d.contract.symbol));
    }
    fn scanner_data_end(&mut self, req_id: i64) {
        self.lines.push(format!("scannerDataEnd|{req_id}"));
    }
    fn historical_data(&mut self, req_id: i64, b: &BarData) {
        self.lines.push(bar_line("historicalData", req_id, b));
    }
    fn historical_data_update(&mut self, req_id: i64, b: &BarData) {
        self.lines.push(bar_line("historicalDataUpdate", req_id, b));
    }
    fn historical_data_end(&mut self, req_id: i64, start: &str, end: &str) {
        self.lines.push(format!("historicalDataEnd|{req_id}|{start}|{end}"));
    }
    fn head_timestamp(&mut self, req_id: i64, ts: &str) {
        self.lines.push(format!("headTimestamp|{req_id}|{ts}"));
    }
    fn smart_components(&mut self, req_id: i64, components: &[crate::types::SmartComponent]) {
        let mut rows: Vec<&crate::types::SmartComponent> = components.iter().collect();
        rows.sort_by_key(|c| c.bit_number);
        let rows: Vec<String> = rows.iter().map(|c| format!("{}:{}:{}", c.bit_number, c.exchange, c.exchange_letter)).collect();
        self.lines.push(format!("smartComponents|{req_id}|{}", rows.join(",")));
    }
    fn order_status(
        &mut self, order_id: i64, status: &str, filled: f64, remaining: f64, avg_fill_price: f64, perm_id: i64,
        parent_id: i64, last_fill_price: f64, client_id: i64, why_held: &str, mkt_cap_price: f64,
    ) {
        self.lines.push(format!(
            "orderStatus|{order_id}|{status}|{}|{}|{}|{}|{parent_id}|{}|{client_id}|{why_held}|{}",
            n(filled), n(remaining), n(avg_fill_price), perm(perm_id), n(last_fill_price), n(mkt_cap_price),
        ));
    }
    fn open_order(&mut self, order_id: i64, c: &Contract, o: &Order, state: &OrderState) {
        let field = |k: &str| -> String {
            match k {
                "action" => o.action.clone(),
                "totalQuantity" => n(o.total_quantity),
                "orderType" => o.order_type.clone(),
                "lmtPrice" => order_price(o.lmt_price),
                "auxPrice" => order_price(o.aux_price),
                "tif" => o.tif.clone(),
                "ocaGroup" => o.oca_group.clone(),
                "orderRef" => o.order_ref.clone(),
                "parentId" => o.parent_id.to_string(),
                "outsideRth" => o.outside_rth.to_string(),
                "goodAfterTime" => o.good_after_time.clone(),
                "goodTillDate" => o.good_till_date.clone(),
                "account" => o.account.clone(),
                "trailingPercent" => order_price(o.trailing_percent),
                "trailStopPrice" => order_price(o.trail_stop_price),
                "whatIf" => o.what_if.to_string(),
                "permId" => perm(o.perm_id).to_string(),
                "clientId" => o.client_id.to_string(),
                _ => unreachable!("{k}"),
            }
        };
        let fields: Vec<String> = OPEN_ORDER_FIELDS.iter().filter(|k| super::record::compared_field(k, &o.order_type))
            .map(|k| format!("{k}={}", field(k))).collect();
        self.lines.push(format!(
            "openOrder|{order_id}|{}|{}|{}|{}|{}", c.con_id, c.symbol, c.sec_type, fields.join(","), state.status,
        ));
    }
    fn open_order_end(&mut self) {
        self.lines.push("openOrderEnd".into());
    }
    fn exec_details(&mut self, req_id: i64, c: &Contract, e: &Execution) {
        self.lines.push(format!(
            "execDetails|{req_id}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
            c.con_id, c.symbol, e.exchange, e.side, n(e.shares), n(e.price), n(e.cum_qty), n(e.avg_price),
            e.order_id, e.order_ref, e.last_liquidity,
        ));
    }
    fn exec_details_end(&mut self, req_id: i64) {
        self.lines.push(format!("execDetailsEnd|{req_id}"));
    }
    fn commission_and_fees_report(&mut self, r: &CommissionAndFeesReport) {
        self.lines.push(format!("commissionAndFeesReport|{}|{}", n(r.commission_and_fees), r.currency));
    }
    fn tick_by_tick_all_last(
        &mut self, req_id: i64, tick_type: i32, time: i64, price: f64, size: f64,
        a: &crate::api::types::TickAttribLast, exchange: &str, special: &str,
    ) {
        self.lines.push(format!(
            "tickByTickAllLast|{req_id}|{tick_type}|{time}|{}|{}|{}|{exchange}|{special}",
            n(price), n(size), attr_mask(false, a.past_limit, a.unreported),
        ));
    }
    fn tick_by_tick_bid_ask(
        &mut self, req_id: i64, time: i64, bid: f64, ask: f64, bid_size: f64, ask_size: f64,
        a: &crate::api::types::TickAttribBidAsk,
    ) {
        self.lines.push(format!(
            "tickByTickBidAsk|{req_id}|{time}|{}|{}|{}|{}|{}",
            n(bid), n(ask), n(bid_size), n(ask_size), attr_mask(false, a.bid_past_low, a.ask_past_high),
        ));
    }
    fn tick_by_tick_mid_point(&mut self, req_id: i64, time: i64, mid: f64) {
        self.lines.push(format!("tickByTickMidPoint|{req_id}|{time}|{}", n(mid)));
    }
    fn historical_ticks_last(&mut self, req_id: i64, ticks: &crate::types::HistoricalTickData, done: bool) {
        if let crate::types::HistoricalTickData::Last(rows) = ticks {
            let rows: Vec<String> = rows.iter().map(|t| format!(
                "{}:{}:{}:{}:{}:{}", t.time, n(t.price), n(t.size),
                attr_mask(false, t.tick_attrib_last.past_limit, t.tick_attrib_last.unreported), t.exchange, t.special_conditions,
            )).collect();
            self.lines.push(format!("historicalTicksLast|{req_id}|{}|{done}", rows.join(",")));
        }
    }
    fn real_time_bar(
        &mut self, req_id: i64, date: i64, open: f64, high: f64, low: f64, close: f64, volume: f64, wap: f64, count: i32,
    ) {
        self.lines.push(format!(
            "realtimeBar|{req_id}|{date}|{}|{}|{}|{}|{}|{}|{count}",
            n(open), n(high), n(low), n(close), n(volume), n(wap),
        ));
    }
}
