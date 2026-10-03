//! A replay session: the engine on in-memory links to a scripted farm and
//! auth server, the API client on top, driven one step at a time from the
//! test thread, so the callbacks of each recorded frame are known.

use std::sync::Arc;

use crate::api::client::EClient;
use crate::api::types::{Contract, Order, OrderState, TickAttrib};
use crate::api::wrapper::Wrapper;
use crate::bridge::SharedState;
use crate::engine::hot_loop::HotLoop;
use crate::test_support::{parse_fields, Fields, Peer};

use super::fixture::{attr_mask, n, order_price, perm, OPEN_ORDER_FIELDS};

pub(crate) const ACCOUNT: &str = "DUXXXXXXX";

/// Makes bad copies of a server frame (the robustness tests, ibx#488).
pub(crate) type BadCopies = Box<dyn FnMut(&[u8]) -> Vec<Vec<u8>>>;

thread_local! {
    /// When set, each server frame a replay sends is preceded by the bad
    /// copies this makes of it, given straight to the engine's handler:
    /// the replay then runs on with the bad input in between.
    pub(crate) static BAD_COPIES: std::cell::RefCell<Option<BadCopies>> = const { std::cell::RefCell::new(None) };
}

/// A link of the session.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Link {
    Farm,
    Ccp,
    Hmds,
}

/// Steps of the engine after each input: enough for a request to reach the
/// wire and for its reply to reach the queues.
const SETTLE_STEPS: usize = 4;

pub(crate) struct Session {
    pub engine: HotLoop,
    pub client: EClient,
    pub shared: Arc<SharedState>,
    pub farm: Peer,
    pub ccp: Peer,
    pub hmds: Peer,
    /// Every message the engine sent on each link, in order.
    pub farm_out: Vec<Fields>,
    pub ccp_out: Vec<Fields>,
    pub hmds_out: Vec<Fields>,
    /// The callbacks ibx gave, one line each (see [`Recorder`]).
    pub callbacks: Vec<String>,
}

impl Session {
    pub fn new() -> Self {
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
        let client = EClient::from_parts(shared.clone(), control_tx, std::thread::spawn(|| {}), ACCOUNT.into());
        Self {
            engine, client, shared, farm, ccp, hmds,
            farm_out: Vec::new(), ccp_out: Vec::new(), hmds_out: Vec::new(), callbacks: Vec::new(),
        }
    }

    /// Call the API client; the engine runs on this thread meanwhile (a
    /// call may wait for the engine's answer). Then the engine settles and
    /// the callbacks are taken.
    pub fn call<R: Send>(&mut self, f: impl FnOnce(&EClient) -> R + Send) -> R {
        let Self { engine, client, .. } = self;
        let client = &*client;
        let out = std::thread::scope(|s| {
            let h = s.spawn(move || f(client));
            while !h.is_finished() {
                engine.step_for_test();
                std::thread::yield_now();
            }
            h.join().unwrap()
        });
        self.settle();
        out
    }

    /// Run the engine a few steps, read what it sent, and take the callbacks.
    pub fn settle(&mut self) {
        for _ in 0..SETTLE_STEPS {
            self.engine.step_for_test();
        }
        self.farm_out.extend(self.farm.messages().iter().map(|m| parse_fields(m)));
        self.ccp_out.extend(self.ccp.messages().iter().map(|m| parse_fields(m)));
        self.hmds_out.extend(self.hmds.messages().iter().map(|m| parse_fields(m)));
        let mut rec = Recorder::default();
        self.client.process_msgs(&mut rec);
        self.callbacks.extend(rec.lines);
        // What the dispatch sent to the engine (a snapshot's cancel).
        for _ in 0..SETTLE_STEPS {
            self.engine.step_for_test();
        }
        self.farm_out.extend(self.farm.messages().iter().map(|m| parse_fields(m)));
        self.ccp_out.extend(self.ccp.messages().iter().map(|m| parse_fields(m)));
        self.hmds_out.extend(self.hmds.messages().iter().map(|m| parse_fields(m)));
    }

    pub fn send_farm(&mut self, raw: &[u8]) {
        self.give_bad_copies(raw, Link::Farm);
        self.farm.send_raw(raw);
        self.settle();
    }

    pub fn send_ccp(&mut self, raw: &[u8]) {
        self.give_bad_copies(raw, Link::Ccp);
        self.ccp.send_raw(raw);
        self.settle();
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

    /// A message to the engine on the historical link, compressed as the
    /// farm sends it.
    pub fn send_hmds_message(&mut self, fields: &Fields) {
        let f: Vec<(u32, &str)> = fields.iter().filter(|(t, _)| !matches!(t, 8 | 9 | 10 | 34))
            .map(|(t, v)| (*t, v.as_str())).collect();
        if BAD_COPIES.with(|hook| hook.borrow().is_some()) {
            self.give_bad_copies(&crate::protocol::fix::fix_build(&f, 0), Link::Hmds);
        }
        self.hmds.send_fixcomp(&f);
        self.settle();
    }
}

/// The callbacks as one line each, in the form of
/// [`super::fixture::canonical`].
#[derive(Default)]
pub(crate) struct Recorder {
    pub lines: Vec<String>,
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
    fn historical_data(&mut self, req_id: i64, b: &crate::api::types::BarData) {
        self.lines.push(format!(
            "historicalData|{req_id}|{}|{}|{}|{}|{}|{}|{}|{}",
            b.date, n(b.open), n(b.high), n(b.low), n(b.close), b.volume, n(b.wap), b.bar_count,
        ));
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
        let fields: Vec<String> = OPEN_ORDER_FIELDS.iter().map(|k| format!("{k}={}", field(k))).collect();
        self.lines.push(format!(
            "openOrder|{order_id}|{}|{}|{}|{}|{}", c.con_id, c.symbol, c.sec_type, fields.join(","), state.status,
        ));
    }
}
