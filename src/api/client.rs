//! ibapi-compatible EClient — Rust equivalent of C++ `EClientSocket`.
//!
//! Connects to IB, provides ibapi-matching method signatures, and dispatches
//! events to a [`Wrapper`] via `process_msgs()`.
//!
//! ```no_run
//! use ibx::api::{EClient, EClientConfig, Wrapper, Contract, Order};
//! use ibx::api::types::TickAttrib;
//!
//! struct MyWrapper;
//! impl Wrapper for MyWrapper {
//!     fn tick_price(&mut self, req_id: i64, tick_type: i32, price: f64, attrib: &TickAttrib) {
//!         println!("tick_price: req_id={req_id} type={tick_type} price={price}");
//!     }
//! }
//!
//! let mut client = EClient::connect(&EClientConfig {
//!     username: "user".into(),
//!     password: "pass".into(),
//!     host: "cdc1.ibllc.com".into(),
//!     paper: true,
//!     core_id: None,
//! }).unwrap();
//!
//! client.req_mkt_data(1, &Contract { con_id: 756733, symbol: "SPY".into(), ..Default::default() },
//!     "", false, false);
//!
//! let mut wrapper = MyWrapper;
//! loop {
//!     client.process_msgs(&mut wrapper);
//! }
//! ```

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use crossbeam_channel::Sender;

use crate::api::types::{
    self, Contract as ApiContract, Order as ApiOrder, TagValue as ApiTagValue,
    BarData, ContractDetails, ContractDescription, Execution, OrderState,
    TickAttrib, TickAttribLast, TickAttribBidAsk, PriceIncrement, CommissionReport,
    PRICE_SCALE_F,
};
use crate::api::wrapper::Wrapper;
use crate::bridge::SharedState;
use crate::gateway::{Gateway, GatewayConfig};
use crate::types::*;

// Tick type constants matching ibapi.
const TICK_BID: i32 = 1;
const TICK_ASK: i32 = 2;
const TICK_LAST: i32 = 4;
const TICK_HIGH: i32 = 6;
const TICK_LOW: i32 = 7;
const TICK_CLOSE: i32 = 9;
const TICK_OPEN: i32 = 14;
const TICK_BID_SIZE: i32 = 0;
const TICK_ASK_SIZE: i32 = 3;
const TICK_LAST_SIZE: i32 = 5;
const TICK_VOLUME: i32 = 8;

// Re-export as public type names for the API surface
pub type Contract = ApiContract;
pub type Order = ApiOrder;
pub type TagValue = ApiTagValue;

/// Configuration for connecting to IB via EClient.
pub struct EClientConfig {
    pub username: String,
    pub password: String,
    pub host: String,
    pub paper: bool,
    pub core_id: Option<usize>,
}

/// ibapi-compatible EClient. Matches C++ `EClientSocket` method signatures.
pub struct EClient {
    shared: Arc<SharedState>,
    control_tx: Sender<ControlCommand>,
    _thread: thread::JoinHandle<()>,
    pub account_id: String,
    connected: AtomicBool,
    next_order_id: AtomicU64,

    // reqId <-> InstrumentId mapping
    req_to_instrument: Mutex<HashMap<i64, InstrumentId>>,
    instrument_to_req: Mutex<HashMap<InstrumentId, i64>>,
    // Change detection for quote polling
    last_quotes: Mutex<HashMap<InstrumentId, [i64; 12]>>,
}

impl EClient {
    /// Connect to IB and start the engine.
    pub fn connect(config: &EClientConfig) -> Result<Self, Box<dyn std::error::Error>> {
        let gw_config = GatewayConfig {
            username: config.username.clone(),
            password: config.password.clone(),
            host: config.host.clone(),
            paper: config.paper,
        };

        let (gw, farm_conn, ccp_conn, hmds_conn) = Gateway::connect(&gw_config)?;
        let account_id = gw.account_id.clone();
        let shared = Arc::new(SharedState::new());

        let (mut hot_loop, control_tx) = gw.into_hot_loop(
            shared.clone(), None, farm_conn, ccp_conn, hmds_conn, config.core_id,
        );

        let handle = thread::Builder::new()
            .name("ib-engine-hotloop".into())
            .spawn(move || { hot_loop.run(); })?;

        let start_id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() * 1000;

        Ok(Self {
            shared,
            control_tx,
            _thread: handle,
            account_id,
            connected: AtomicBool::new(true),
            next_order_id: AtomicU64::new(start_id),
            req_to_instrument: Mutex::new(HashMap::new()),
            instrument_to_req: Mutex::new(HashMap::new()),
            last_quotes: Mutex::new(HashMap::new()),
        })
    }

    /// Construct from pre-built components (for testing or custom setups).
    #[doc(hidden)]
    pub fn from_parts(
        shared: Arc<SharedState>,
        control_tx: Sender<ControlCommand>,
        thread: thread::JoinHandle<()>,
        account_id: String,
    ) -> Self {
        let start_id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() * 1000;
        Self {
            shared,
            control_tx,
            _thread: thread,
            account_id,
            connected: AtomicBool::new(true),
            next_order_id: AtomicU64::new(start_id),
            req_to_instrument: Mutex::new(HashMap::new()),
            instrument_to_req: Mutex::new(HashMap::new()),
            last_quotes: Mutex::new(HashMap::new()),
        }
    }

    // ── Connection ──

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    pub fn disconnect(&self) {
        let _ = self.control_tx.send(ControlCommand::Shutdown);
        self.connected.store(false, Ordering::Release);
    }

    // ── Market Data ──

    /// Subscribe to market data. Matches `reqMktData` in C++.
    pub fn req_mkt_data(
        &self, req_id: i64, contract: &Contract,
        _generic_tick_list: &str, _snapshot: bool, _regulatory_snapshot: bool,
    ) {
        let _ = self.control_tx.send(ControlCommand::RegisterInstrument { con_id: contract.con_id });
        let _ = self.control_tx.send(ControlCommand::Subscribe {
            con_id: contract.con_id,
            symbol: contract.symbol.clone(),
        });

        // Wait briefly for the engine to process the registration, then map the reqId.
        // TODO: Replace with acknowledgment-based approach to eliminate this delay.
        std::thread::sleep(std::time::Duration::from_millis(10));
        let instrument_id = self.shared.instrument_count().saturating_sub(1);

        self.req_to_instrument.lock().unwrap().insert(req_id, instrument_id);
        self.instrument_to_req.lock().unwrap().insert(instrument_id, req_id);
    }

    /// Cancel market data. Matches `cancelMktData` in C++.
    pub fn cancel_mkt_data(&self, req_id: i64) {
        if let Some(instrument) = self.req_to_instrument.lock().unwrap().remove(&req_id) {
            self.instrument_to_req.lock().unwrap().remove(&instrument);
            self.last_quotes.lock().unwrap().remove(&instrument);
            let _ = self.control_tx.send(ControlCommand::Unsubscribe { instrument });
        }
    }

    /// Subscribe to tick-by-tick data. Matches `reqTickByTickData` in C++.
    pub fn req_tick_by_tick_data(
        &self, req_id: i64, contract: &Contract, tick_type: &str,
        _number_of_ticks: i32, _ignore_size: bool,
    ) {
        let tbt_type = match tick_type {
            "BidAsk" => TbtType::BidAsk,
            _ => TbtType::Last,
        };
        let _ = self.control_tx.send(ControlCommand::SubscribeTbt {
            con_id: contract.con_id,
            symbol: contract.symbol.clone(),
            tbt_type,
        });

        // Map reqId to instrument
        std::thread::sleep(std::time::Duration::from_millis(10));
        let instrument_id = self.shared.instrument_count().saturating_sub(1);
        self.req_to_instrument.lock().unwrap().insert(req_id, instrument_id);
        self.instrument_to_req.lock().unwrap().insert(instrument_id, req_id);
    }

    /// Cancel tick-by-tick data. Matches `cancelTickByTickData` in C++.
    pub fn cancel_tick_by_tick_data(&self, req_id: i64) {
        if let Some(instrument) = self.req_to_instrument.lock().unwrap().remove(&req_id) {
            self.instrument_to_req.lock().unwrap().remove(&instrument);
            let _ = self.control_tx.send(ControlCommand::UnsubscribeTbt { instrument });
        }
    }

    // ── Orders ──

    /// Place an order. Matches `placeOrder` in C++.
    pub fn place_order(&self, order_id: i64, contract: &Contract, order: &Order) -> Result<(), String> {
        let oid = if order_id > 0 {
            order_id as u64
        } else {
            self.next_order_id.fetch_add(1, Ordering::Relaxed)
        };

        let instrument = self.find_or_register_instrument(contract)?;
        let side = order.side()?;
        let qty = order.total_quantity as u32;
        let order_type = order.order_type.to_uppercase();

        // Algo orders
        if !order.algo_strategy.is_empty() {
            let algo = parse_algo_params(&order.algo_strategy, &order.algo_params)?;
            let price = (order.lmt_price * PRICE_SCALE_F) as i64;
            let _ = self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitAlgo {
                order_id: oid, instrument, side, qty, price, algo,
            }));
            return Ok(());
        }

        // What-if orders
        if order.what_if {
            let price = (order.lmt_price * PRICE_SCALE_F) as i64;
            let _ = self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitWhatIf {
                order_id: oid, instrument, side, qty, price,
            }));
            return Ok(());
        }

        let req = match order_type.as_str() {
            "MKT" => OrderRequest::SubmitMarket { order_id: oid, instrument, side, qty },
            "LMT" => {
                let price = (order.lmt_price * PRICE_SCALE_F) as i64;
                if order.has_extended_attrs() || order.tif != "DAY" {
                    OrderRequest::SubmitLimitEx {
                        order_id: oid, instrument, side, qty, price,
                        tif: order.tif_byte(),
                        attrs: order.attrs(),
                    }
                } else {
                    OrderRequest::SubmitLimit { order_id: oid, instrument, side, qty, price }
                }
            }
            "STP" => {
                let stop = (order.aux_price * PRICE_SCALE_F) as i64;
                OrderRequest::SubmitStop { order_id: oid, instrument, side, qty, stop_price: stop }
            }
            "STP LMT" => {
                let price = (order.lmt_price * PRICE_SCALE_F) as i64;
                let stop = (order.aux_price * PRICE_SCALE_F) as i64;
                OrderRequest::SubmitStopLimit { order_id: oid, instrument, side, qty, price, stop_price: stop }
            }
            "TRAIL" => {
                if order.trailing_percent > 0.0 {
                    let pct = (order.trailing_percent * 100.0) as u32;
                    OrderRequest::SubmitTrailingStopPct { order_id: oid, instrument, side, qty, trail_pct: pct }
                } else {
                    let trail = (order.aux_price * PRICE_SCALE_F) as i64;
                    OrderRequest::SubmitTrailingStop { order_id: oid, instrument, side, qty, trail_amt: trail }
                }
            }
            "TRAIL LIMIT" => {
                let price = (order.lmt_price * PRICE_SCALE_F) as i64;
                let trail = (order.aux_price * PRICE_SCALE_F) as i64;
                OrderRequest::SubmitTrailingStopLimit { order_id: oid, instrument, side, qty, price, trail_amt: trail }
            }
            "MOC" => OrderRequest::SubmitMoc { order_id: oid, instrument, side, qty },
            "LOC" => {
                let price = (order.lmt_price * PRICE_SCALE_F) as i64;
                OrderRequest::SubmitLoc { order_id: oid, instrument, side, qty, price }
            }
            "MIT" => {
                let stop = (order.aux_price * PRICE_SCALE_F) as i64;
                OrderRequest::SubmitMit { order_id: oid, instrument, side, qty, stop_price: stop }
            }
            "LIT" => {
                let price = (order.lmt_price * PRICE_SCALE_F) as i64;
                let stop = (order.aux_price * PRICE_SCALE_F) as i64;
                OrderRequest::SubmitLit { order_id: oid, instrument, side, qty, price, stop_price: stop }
            }
            "MTL" => OrderRequest::SubmitMtl { order_id: oid, instrument, side, qty },
            "MKT PRT" => OrderRequest::SubmitMktPrt { order_id: oid, instrument, side, qty },
            "STP PRT" => {
                let stop = (order.aux_price * PRICE_SCALE_F) as i64;
                OrderRequest::SubmitStpPrt { order_id: oid, instrument, side, qty, stop_price: stop }
            }
            "REL" => {
                let offset = (order.aux_price * PRICE_SCALE_F) as i64;
                OrderRequest::SubmitRel { order_id: oid, instrument, side, qty, offset }
            }
            "PEG MKT" => {
                let offset = (order.aux_price * PRICE_SCALE_F) as i64;
                OrderRequest::SubmitPegMkt { order_id: oid, instrument, side, qty, offset }
            }
            "PEG MID" | "PEG MIDPT" => {
                let offset = (order.aux_price * PRICE_SCALE_F) as i64;
                OrderRequest::SubmitPegMid { order_id: oid, instrument, side, qty, offset }
            }
            "MIDPX" | "MIDPRICE" => {
                let cap = (order.lmt_price * PRICE_SCALE_F) as i64;
                OrderRequest::SubmitMidPrice { order_id: oid, instrument, side, qty, price_cap: cap }
            }
            "SNAP MKT" => OrderRequest::SubmitSnapMkt { order_id: oid, instrument, side, qty },
            "SNAP MID" | "SNAP MIDPT" => OrderRequest::SubmitSnapMid { order_id: oid, instrument, side, qty },
            "SNAP PRI" | "SNAP PRIM" => OrderRequest::SubmitSnapPri { order_id: oid, instrument, side, qty },
            "BOX TOP" => OrderRequest::SubmitMtl { order_id: oid, instrument, side, qty },
            _ => return Err(format!("Unsupported order type: '{}'", order.order_type)),
        };

        let _ = self.control_tx.send(ControlCommand::Order(req));
        Ok(())
    }

    /// Cancel an order. Matches `cancelOrder` in C++.
    pub fn cancel_order(&self, order_id: i64, _manual_order_cancel_time: &str) {
        let _ = self.control_tx.send(ControlCommand::Order(OrderRequest::Cancel {
            order_id: order_id as u64,
        }));
    }

    /// Cancel all orders. Matches `reqGlobalCancel` in C++.
    pub fn req_global_cancel(&self) {
        let map = self.req_to_instrument.lock().unwrap();
        for &instrument in map.values() {
            let _ = self.control_tx.send(ControlCommand::Order(OrderRequest::CancelAll { instrument }));
        }
    }

    /// Request next valid order ID. Matches `reqIds` in C++.
    pub fn req_ids(&self, wrapper: &mut impl Wrapper) {
        let next_id = self.next_order_id.load(Ordering::Relaxed) as i64;
        wrapper.next_valid_id(next_id);
    }

    /// Get the next order ID (local counter).
    pub fn next_order_id(&self) -> i64 {
        self.next_order_id.fetch_add(1, Ordering::Relaxed) as i64
    }

    // ── Historical Data ──

    /// Request historical data. Matches `reqHistoricalData` in C++.
    pub fn req_historical_data(
        &self, req_id: i64, contract: &Contract,
        end_date_time: &str, duration: &str, bar_size: &str,
        what_to_show: &str, use_rth: bool, _format_date: i32, _keep_up_to_date: bool,
    ) {
        let _ = self.control_tx.send(ControlCommand::FetchHistorical {
            req_id: req_id as u32,
            con_id: contract.con_id,
            symbol: contract.symbol.clone(),
            end_date_time: end_date_time.into(),
            duration: duration.into(),
            bar_size: bar_size.into(),
            what_to_show: what_to_show.into(),
            use_rth,
        });
    }

    /// Cancel historical data. Matches `cancelHistoricalData` in C++.
    pub fn cancel_historical_data(&self, req_id: i64) {
        let _ = self.control_tx.send(ControlCommand::CancelHistorical { req_id: req_id as u32 });
    }

    /// Request head timestamp. Matches `reqHeadTimestamp` in C++.
    pub fn req_head_timestamp(
        &self, req_id: i64, contract: &Contract, what_to_show: &str, use_rth: bool, _format_date: i32,
    ) {
        let _ = self.control_tx.send(ControlCommand::FetchHeadTimestamp {
            req_id: req_id as u32,
            con_id: contract.con_id,
            what_to_show: what_to_show.into(),
            use_rth,
        });
    }

    // ── Contract Details ──

    /// Request contract details. Matches `reqContractDetails` in C++.
    pub fn req_contract_details(&self, req_id: i64, contract: &Contract) {
        let _ = self.control_tx.send(ControlCommand::FetchContractDetails {
            req_id: req_id as u32,
            con_id: contract.con_id,
        });
    }

    /// Request matching symbols. Matches `reqMatchingSymbols` in C++.
    pub fn req_matching_symbols(&self, req_id: i64, pattern: &str) {
        let _ = self.control_tx.send(ControlCommand::FetchMatchingSymbols {
            req_id: req_id as u32,
            pattern: pattern.into(),
        });
    }

    // ── Positions ──

    /// Request positions. Matches `reqPositions` in C++.
    /// Immediately delivers all positions via wrapper callbacks, then calls position_end.
    pub fn req_positions(&self, wrapper: &mut impl Wrapper) {
        let positions = self.shared.position_infos();
        for pi in &positions {
            let c = Contract { con_id: pi.con_id, ..Default::default() };
            let avg_cost = pi.avg_cost as f64 / PRICE_SCALE_F;
            wrapper.position(&self.account_id, &c, pi.position as f64, avg_cost);
        }
        wrapper.position_end();
    }

    // ── Scanner ──

    pub fn req_scanner_parameters(&self) {
        let _ = self.control_tx.send(ControlCommand::FetchScannerParams);
    }

    pub fn req_scanner_subscription(
        &self, req_id: i64, instrument: &str, location_code: &str,
        scan_code: &str, max_items: u32,
    ) {
        let _ = self.control_tx.send(ControlCommand::SubscribeScanner {
            req_id: req_id as u32,
            instrument: instrument.into(),
            location_code: location_code.into(),
            scan_code: scan_code.into(),
            max_items,
        });
    }

    pub fn cancel_scanner_subscription(&self, req_id: i64) {
        let _ = self.control_tx.send(ControlCommand::CancelScanner { req_id: req_id as u32 });
    }

    // ── News ──

    pub fn req_historical_news(
        &self, req_id: i64, con_id: i64, provider_codes: &str,
        start_time: &str, end_time: &str, max_results: u32,
    ) {
        let _ = self.control_tx.send(ControlCommand::FetchHistoricalNews {
            req_id: req_id as u32,
            con_id: con_id as u32,
            provider_codes: provider_codes.into(),
            start_time: start_time.into(),
            end_time: end_time.into(),
            max_results,
        });
    }

    pub fn req_news_article(&self, req_id: i64, provider_code: &str, article_id: &str) {
        let _ = self.control_tx.send(ControlCommand::FetchNewsArticle {
            req_id: req_id as u32,
            provider_code: provider_code.into(),
            article_id: article_id.into(),
        });
    }

    // ── Fundamental Data ──

    pub fn req_fundamental_data(&self, req_id: i64, contract: &Contract, report_type: &str) {
        let _ = self.control_tx.send(ControlCommand::FetchFundamentalData {
            req_id: req_id as u32,
            con_id: contract.con_id as u32,
            report_type: report_type.into(),
        });
    }

    pub fn cancel_fundamental_data(&self, req_id: i64) {
        let _ = self.control_tx.send(ControlCommand::CancelFundamentalData { req_id: req_id as u32 });
    }

    // ── Histogram ──

    pub fn req_histogram_data(&self, req_id: i64, contract: &Contract, use_rth: bool, period: &str) {
        let _ = self.control_tx.send(ControlCommand::FetchHistogramData {
            req_id: req_id as u32,
            con_id: contract.con_id as u32,
            use_rth,
            period: period.into(),
        });
    }

    pub fn cancel_histogram_data(&self, req_id: i64) {
        let _ = self.control_tx.send(ControlCommand::CancelHistogramData { req_id: req_id as u32 });
    }

    // ── Historical Ticks ──

    pub fn req_historical_ticks(
        &self, req_id: i64, contract: &Contract,
        start_date_time: &str, end_date_time: &str,
        number_of_ticks: i32, what_to_show: &str, use_rth: bool,
    ) {
        let _ = self.control_tx.send(ControlCommand::FetchHistoricalTicks {
            req_id: req_id as u32,
            con_id: contract.con_id,
            start_date_time: start_date_time.into(),
            end_date_time: end_date_time.into(),
            number_of_ticks: number_of_ticks as u32,
            what_to_show: what_to_show.into(),
            use_rth,
        });
    }

    // ── Real-Time Bars ──

    pub fn req_real_time_bars(
        &self, req_id: i64, contract: &Contract,
        _bar_size: i32, what_to_show: &str, use_rth: bool,
    ) {
        let _ = self.control_tx.send(ControlCommand::SubscribeRealTimeBar {
            req_id: req_id as u32,
            con_id: contract.con_id,
            symbol: contract.symbol.clone(),
            what_to_show: what_to_show.into(),
            use_rth,
        });
    }

    pub fn cancel_real_time_bars(&self, req_id: i64) {
        let _ = self.control_tx.send(ControlCommand::CancelRealTimeBar { req_id: req_id as u32 });
    }

    // ── Historical Schedule ──

    pub fn req_historical_schedule(
        &self, req_id: i64, contract: &Contract,
        end_date_time: &str, duration: &str, use_rth: bool,
    ) {
        let _ = self.control_tx.send(ControlCommand::FetchHistoricalSchedule {
            req_id: req_id as u32,
            con_id: contract.con_id,
            end_date_time: end_date_time.into(),
            duration: duration.into(),
            use_rth,
        });
    }

    // ── Escape Hatch ──

    /// Zero-copy SeqLock quote read. Maps reqId → InstrumentId → SeqLock.
    /// Returns `None` if the reqId is not mapped to a subscription.
    #[inline]
    pub fn quote(&self, req_id: i64) -> Option<Quote> {
        let map = self.req_to_instrument.lock().unwrap();
        map.get(&req_id).map(|&iid| self.shared.quote(iid))
    }

    /// Direct SeqLock read by InstrumentId (for callers who track IDs themselves).
    #[inline]
    pub fn quote_by_instrument(&self, instrument: InstrumentId) -> Quote {
        self.shared.quote(instrument)
    }

    /// Read account state snapshot.
    pub fn account(&self) -> AccountState {
        self.shared.account()
    }

    // ── Message Processing ──

    /// Drain all SharedState queues and dispatch to the Wrapper.
    /// Call this in a loop — it is the Rust equivalent of C++ `EReader::processMsgs()`.
    pub fn process_msgs(&self, wrapper: &mut impl Wrapper) {
        // Fills → order_status + exec_details
        for fill in self.shared.drain_fills() {
            let price_f = fill.price as f64 / PRICE_SCALE_F;
            let status = if fill.remaining == 0 { "Filled" } else { "PartiallyFilled" };
            wrapper.order_status(
                fill.order_id as i64, status, fill.qty as f64, fill.remaining as f64,
                price_f, 0, 0, price_f, 0, "", 0.0,
            );

            // exec_details
            let side_str = match fill.side {
                Side::Buy => "BOT",
                Side::Sell => "SLD",
                Side::ShortSell => "SLD",
            };
            let exec = Execution {
                side: side_str.into(),
                shares: fill.qty as f64,
                price: price_f,
                order_id: fill.order_id as i64,
                ..Default::default()
            };
            let c = Contract::default(); // minimal — conId not available from Fill
            let req_id = self.instrument_to_req.lock().unwrap()
                .get(&fill.instrument).copied().unwrap_or(-1);
            wrapper.exec_details(req_id, &c, &exec);
        }

        // Order updates → order_status
        for update in self.shared.drain_order_updates() {
            let status = match update.status {
                OrderStatus::PendingSubmit => "PendingSubmit",
                OrderStatus::Submitted => "Submitted",
                OrderStatus::Filled => "Filled",
                OrderStatus::PartiallyFilled => "PreSubmitted",
                OrderStatus::Cancelled => "Cancelled",
                OrderStatus::Rejected => "Inactive",
                OrderStatus::Uncertain => "Unknown",
            };
            wrapper.order_status(
                update.order_id as i64, status, update.filled_qty as f64,
                update.remaining_qty as f64, 0.0, 0, 0, 0.0, 0, "", 0.0,
            );
        }

        // Cancel rejects → error
        for reject in self.shared.drain_cancel_rejects() {
            let code = if reject.reject_type == 1 { 202 } else { 10147 };
            let msg = format!("Order {} cancel/modify rejected (reason: {})", reject.order_id, reject.reason_code);
            wrapper.error(reject.order_id as i64, code, &msg, "");
        }

        // Quote polling → tick_price / tick_size
        let instruments: Vec<(InstrumentId, i64)> = {
            let map = self.instrument_to_req.lock().unwrap();
            map.iter().map(|(&iid, &req_id)| (iid, req_id)).collect()
        };

        let attrib = crate::api::types::TickAttrib::default();
        for (iid, req_id) in instruments {
            let q = self.shared.quote(iid);
            let fields = [
                q.bid, q.ask, q.last, q.bid_size, q.ask_size, q.last_size,
                q.high, q.low, q.volume, q.close, q.open, q.timestamp_ns as i64,
            ];

            let last = {
                let map = self.last_quotes.lock().unwrap();
                map.get(&iid).copied().unwrap_or([0i64; 12])
            };

            if fields[0] != last[0] { wrapper.tick_price(req_id, TICK_BID, fields[0] as f64 / PRICE_SCALE_F, &attrib); }
            if fields[1] != last[1] { wrapper.tick_price(req_id, TICK_ASK, fields[1] as f64 / PRICE_SCALE_F, &attrib); }
            if fields[2] != last[2] { wrapper.tick_price(req_id, TICK_LAST, fields[2] as f64 / PRICE_SCALE_F, &attrib); }
            if fields[3] != last[3] { wrapper.tick_size(req_id, TICK_BID_SIZE, fields[3] as f64 / QTY_SCALE as f64); }
            if fields[4] != last[4] { wrapper.tick_size(req_id, TICK_ASK_SIZE, fields[4] as f64 / QTY_SCALE as f64); }
            if fields[5] != last[5] { wrapper.tick_size(req_id, TICK_LAST_SIZE, fields[5] as f64 / QTY_SCALE as f64); }
            if fields[6] != last[6] { wrapper.tick_price(req_id, TICK_HIGH, fields[6] as f64 / PRICE_SCALE_F, &attrib); }
            if fields[7] != last[7] { wrapper.tick_price(req_id, TICK_LOW, fields[7] as f64 / PRICE_SCALE_F, &attrib); }
            if fields[8] != last[8] { wrapper.tick_size(req_id, TICK_VOLUME, fields[8] as f64 / QTY_SCALE as f64); }
            if fields[9] != last[9] { wrapper.tick_price(req_id, TICK_CLOSE, fields[9] as f64 / PRICE_SCALE_F, &attrib); }
            if fields[10] != last[10] { wrapper.tick_price(req_id, TICK_OPEN, fields[10] as f64 / PRICE_SCALE_F, &attrib); }

            self.last_quotes.lock().unwrap().insert(iid, fields);
        }

        // TBT trades → tick_by_tick_all_last
        for trade in self.shared.drain_tbt_trades() {
            let req_id = self.instrument_to_req.lock().unwrap()
                .get(&trade.instrument).copied().unwrap_or(-1);
            let attrib_last = TickAttribLast::default();
            wrapper.tick_by_tick_all_last(
                req_id, 1, trade.timestamp as i64,
                trade.price as f64 / PRICE_SCALE_F, trade.size as f64,
                &attrib_last, &trade.exchange, &trade.conditions,
            );
        }

        // TBT quotes → tick_by_tick_bid_ask
        for quote in self.shared.drain_tbt_quotes() {
            let req_id = self.instrument_to_req.lock().unwrap()
                .get(&quote.instrument).copied().unwrap_or(-1);
            let attrib_ba = TickAttribBidAsk::default();
            wrapper.tick_by_tick_bid_ask(
                req_id, quote.timestamp as i64,
                quote.bid as f64 / PRICE_SCALE_F, quote.ask as f64 / PRICE_SCALE_F,
                quote.bid_size as f64, quote.ask_size as f64, &attrib_ba,
            );
        }

        // News → tick_news
        for news in self.shared.drain_tick_news() {
            let first_req_id = self.instrument_to_req.lock().unwrap()
                .values().next().copied().unwrap_or(-1);
            wrapper.tick_news(
                first_req_id, news.timestamp as i64,
                &news.provider_code, &news.article_id, &news.headline, "",
            );
        }

        // What-if → order_status (with margin info in why_held)
        for wi in self.shared.drain_what_if_responses() {
            let msg = format!(
                "WhatIf: initMargin={:.2}, maintMargin={:.2}, commission={:.2}",
                wi.init_margin_after as f64 / PRICE_SCALE_F,
                wi.maint_margin_after as f64 / PRICE_SCALE_F,
                wi.commission as f64 / PRICE_SCALE_F,
            );
            wrapper.order_status(
                wi.order_id as i64, "PreSubmitted", 0.0, 0.0, 0.0, 0, 0, 0.0, 0, &msg, 0.0,
            );
        }

        // Historical data → historical_data + historical_data_end
        for (req_id, response) in self.shared.drain_historical_data() {
            for bar in &response.bars {
                let bd = BarData {
                    date: bar.time.clone(),
                    open: bar.open,
                    high: bar.high,
                    low: bar.low,
                    close: bar.close,
                    volume: bar.volume,
                    wap: bar.wap,
                    bar_count: bar.count as i32,
                };
                wrapper.historical_data(req_id as i64, &bd);
            }
            if response.is_complete {
                wrapper.historical_data_end(req_id as i64, "", "");
            }
        }

        // Head timestamps → head_timestamp
        for (req_id, response) in self.shared.drain_head_timestamps() {
            wrapper.head_timestamp(req_id as i64, &response.head_timestamp);
        }

        // Contract details → contract_details + contract_details_end
        for (req_id, def) in self.shared.drain_contract_details() {
            let details = ContractDetails::from_definition(&def);
            wrapper.contract_details(req_id as i64, &details);
        }
        for req_id in self.shared.drain_contract_details_end() {
            wrapper.contract_details_end(req_id as i64);
        }

        // Matching symbols → symbol_samples
        for (req_id, matches) in self.shared.drain_matching_symbols() {
            let descriptions: Vec<ContractDescription> = matches.iter().map(|m| {
                ContractDescription {
                    con_id: m.con_id as i64,
                    symbol: m.symbol.clone(),
                    sec_type: m.sec_type.to_fix().to_string(),
                    currency: m.currency.clone(),
                    primary_exchange: m.primary_exchange.clone(),
                    derivative_sec_types: m.derivative_types.clone(),
                }
            }).collect();
            wrapper.symbol_samples(req_id as i64, &descriptions);
        }

        // Scanner params
        for xml in self.shared.drain_scanner_params() {
            wrapper.scanner_parameters(&xml);
        }

        // Scanner data
        for (req_id, result) in self.shared.drain_scanner_data() {
            for (rank, con_id) in result.con_ids.iter().enumerate() {
                let details = ContractDetails {
                    contract: Contract {
                        con_id: *con_id as i64,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                wrapper.scanner_data(req_id as i64, rank as i32, &details, "", "", "", "");
            }
            wrapper.scanner_data_end(req_id as i64);
        }

        // Historical news
        for (req_id, headlines, has_more) in self.shared.drain_historical_news() {
            for h in &headlines {
                wrapper.historical_news(req_id as i64, &h.time, &h.provider_code, &h.article_id, &h.headline);
            }
            wrapper.historical_news_end(req_id as i64, has_more);
        }

        // News articles
        for (req_id, article_type, text) in self.shared.drain_news_articles() {
            wrapper.news_article(req_id as i64, article_type, &text);
        }

        // Fundamental data
        for (req_id, data) in self.shared.drain_fundamental_data() {
            wrapper.fundamental_data(req_id as i64, &data);
        }

        // Histogram data
        for (req_id, entries) in self.shared.drain_histogram_data() {
            let items: Vec<(f64, i64)> = entries.iter().map(|e| (e.price, e.count)).collect();
            wrapper.histogram_data(req_id as i64, &items);
        }

        // Historical ticks
        for (req_id, data, _query_id, done) in self.shared.drain_historical_ticks() {
            wrapper.historical_ticks(req_id as i64, &data, done);
        }

        // Real-time bars
        for (req_id, bar) in self.shared.drain_real_time_bars() {
            wrapper.real_time_bar(
                req_id as i64, bar.timestamp as i64,
                bar.open, bar.high, bar.low, bar.close,
                bar.volume, bar.wap, bar.count,
            );
        }

        // Historical schedules
        for (req_id, schedule) in self.shared.drain_historical_schedules() {
            let sessions: Vec<(String, String, String)> = schedule.sessions.iter()
                .map(|s| (s.ref_date.clone(), s.open_time.clone(), s.close_time.clone()))
                .collect();
            wrapper.historical_schedule(
                req_id as i64, &schedule.start_date_time, &schedule.end_date_time,
                &schedule.timezone, &sessions,
            );
        }

        // Market rules (snapshot, not drain — they accumulate)
        // Delivered on request via req_market_rule(), not here.

        // Position updates are delivered immediately via req_positions(), not polled.
    }

    // ── Internal helpers ──

    fn find_or_register_instrument(&self, contract: &Contract) -> Result<InstrumentId, String> {
        // Check if already mapped
        {
            let map = self.req_to_instrument.lock().unwrap();
            for (&_req_id, &iid) in map.iter() {
                return Ok(iid);
            }
        }

        // Register new
        self.control_tx.send(ControlCommand::RegisterInstrument { con_id: contract.con_id })
            .map_err(|e| format!("Engine stopped: {}", e))?;
        self.control_tx.send(ControlCommand::Subscribe {
            con_id: contract.con_id,
            symbol: contract.symbol.clone(),
        }).map_err(|e| format!("Engine stopped: {}", e))?;

        std::thread::sleep(std::time::Duration::from_millis(10));
        Ok(self.shared.instrument_count().saturating_sub(1))
    }
}

/// Parse algo strategy and TagValue params into internal AlgoParams.
pub fn parse_algo_params(strategy: &str, params: &[TagValue]) -> Result<AlgoParams, String> {
    let get = |key: &str| -> String {
        params.iter()
            .find(|tv| tv.tag == key)
            .map(|tv| tv.value.clone())
            .unwrap_or_default()
    };
    let get_f64 = |key: &str| -> f64 { get(key).parse().unwrap_or(0.0) };
    let get_bool = |key: &str| -> bool {
        let v = get(key);
        v == "1" || v.eq_ignore_ascii_case("true")
    };

    match strategy.to_lowercase().as_str() {
        "vwap" => Ok(AlgoParams::Vwap {
            max_pct_vol: get_f64("maxPctVol"),
            no_take_liq: get_bool("noTakeLiq"),
            allow_past_end_time: get_bool("allowPastEndTime"),
            start_time: get("startTime"),
            end_time: get("endTime"),
        }),
        "twap" => Ok(AlgoParams::Twap {
            allow_past_end_time: get_bool("allowPastEndTime"),
            start_time: get("startTime"),
            end_time: get("endTime"),
        }),
        "arrivalpx" | "arrival_price" => {
            let risk = match get("riskAversion").to_lowercase().as_str() {
                "get_done" | "getdone" => RiskAversion::GetDone,
                "aggressive" => RiskAversion::Aggressive,
                "passive" => RiskAversion::Passive,
                _ => RiskAversion::Neutral,
            };
            Ok(AlgoParams::ArrivalPx {
                max_pct_vol: get_f64("maxPctVol"),
                risk_aversion: risk,
                allow_past_end_time: get_bool("allowPastEndTime"),
                force_completion: get_bool("forceCompletion"),
                start_time: get("startTime"),
                end_time: get("endTime"),
            })
        }
        "closepx" | "close_price" => {
            let risk = match get("riskAversion").to_lowercase().as_str() {
                "get_done" | "getdone" => RiskAversion::GetDone,
                "aggressive" => RiskAversion::Aggressive,
                "passive" => RiskAversion::Passive,
                _ => RiskAversion::Neutral,
            };
            Ok(AlgoParams::ClosePx {
                max_pct_vol: get_f64("maxPctVol"),
                risk_aversion: risk,
                force_completion: get_bool("forceCompletion"),
                start_time: get("startTime"),
            })
        }
        "darkice" | "dark_ice" => Ok(AlgoParams::DarkIce {
            allow_past_end_time: get_bool("allowPastEndTime"),
            display_size: get("displaySize").parse().unwrap_or(100),
            start_time: get("startTime"),
            end_time: get("endTime"),
        }),
        "pctvol" | "pct_vol" => Ok(AlgoParams::PctVol {
            pct_vol: get_f64("pctVol"),
            no_take_liq: get_bool("noTakeLiq"),
            start_time: get("startTime"),
            end_time: get("endTime"),
        }),
        _ => Err(format!("Unsupported algo strategy: '{}'", strategy)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::wrapper::tests::RecordingWrapper;

    #[test]
    fn parse_algo_vwap() {
        let params = vec![
            TagValue { tag: "maxPctVol".into(), value: "0.1".into() },
            TagValue { tag: "startTime".into(), value: "09:30:00".into() },
            TagValue { tag: "endTime".into(), value: "16:00:00".into() },
        ];
        let algo = parse_algo_params("vwap", &params).unwrap();
        match algo {
            AlgoParams::Vwap { max_pct_vol, start_time, end_time, .. } => {
                assert!((max_pct_vol - 0.1).abs() < 1e-10);
                assert_eq!(start_time, "09:30:00");
                assert_eq!(end_time, "16:00:00");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn parse_algo_twap() {
        let algo = parse_algo_params("twap", &[]).unwrap();
        match algo {
            AlgoParams::Twap { .. } => {}
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn parse_algo_unsupported() {
        assert!(parse_algo_params("unknown", &[]).is_err());
    }

    #[test]
    fn process_msgs_dispatches_to_recording_wrapper() {
        // Create a SharedState and push some test data
        let shared = Arc::new(SharedState::new());

        // Push a fill
        shared.push_fill(Fill {
            instrument: 0,
            order_id: 42,
            side: Side::Buy,
            price: 150 * PRICE_SCALE,
            qty: 100,
            remaining: 0,
            commission: PRICE_SCALE,
            timestamp_ns: 123456789,
        });

        // Push an order update
        shared.push_order_update(OrderUpdate {
            order_id: 43,
            instrument: 0,
            status: OrderStatus::Submitted,
            filled_qty: 0,
            remaining_qty: 100,
            timestamp_ns: 123456790,
        });

        // Push a cancel reject
        shared.push_cancel_reject(CancelReject {
            order_id: 44,
            instrument: 0,
            reject_type: 1,
            reason_code: 0,
            timestamp_ns: 123456791,
        });

        // Create a mock EClient with shared state (no actual connection)
        let (tx, _rx) = crossbeam_channel::unbounded();
        let handle = std::thread::spawn(|| {});
        let client = EClient::from_parts(shared, tx, handle, "DU123".into());

        let mut wrapper = RecordingWrapper::default();
        client.process_msgs(&mut wrapper);

        // Check fill dispatched as order_status + exec_details
        assert!(wrapper.events.iter().any(|e| e.starts_with("order_status:42:Filled")));
        assert!(wrapper.events.iter().any(|e| e.starts_with("exec_details:-1:BOT:100")));

        // Check order update dispatched
        assert!(wrapper.events.iter().any(|e| e.starts_with("order_status:43:Submitted")));

        // Check cancel reject dispatched as error
        assert!(wrapper.events.iter().any(|e| e.starts_with("error:44:202:")));
    }

    #[test]
    fn process_msgs_dispatches_quotes_on_change() {
        let shared = Arc::new(SharedState::new());

        // Register an instrument and write a quote
        let mut q = Quote::default();
        q.bid = 150 * PRICE_SCALE;
        q.ask = 151 * PRICE_SCALE;
        shared.push_quote(0, &q);

        let (tx, _rx) = crossbeam_channel::unbounded();
        let handle = std::thread::spawn(|| {});
        let client = EClient::from_parts(shared.clone(), tx, handle, "DU123".into());

        // Map reqId 1 → instrument 0
        client.req_to_instrument.lock().unwrap().insert(1, 0);
        client.instrument_to_req.lock().unwrap().insert(0, 1);

        let mut wrapper = RecordingWrapper::default();
        client.process_msgs(&mut wrapper);

        // Should have tick_price for bid and ask
        assert!(wrapper.events.iter().any(|e| e.starts_with("tick_price:1:1:150")));
        assert!(wrapper.events.iter().any(|e| e.starts_with("tick_price:1:2:151")));

        // Second call should NOT dispatch (no changes)
        wrapper.events.clear();
        client.process_msgs(&mut wrapper);
        assert!(wrapper.events.is_empty(), "no events on unchanged quotes");

        // Now change the bid
        q.bid = 149 * PRICE_SCALE;
        shared.push_quote(0, &q);
        client.process_msgs(&mut wrapper);
        assert!(wrapper.events.iter().any(|e| e.starts_with("tick_price:1:1:149")));
    }

    #[test]
    fn quote_escape_hatch() {
        let shared = Arc::new(SharedState::new());
        let mut q = Quote::default();
        q.bid = 200 * PRICE_SCALE;
        shared.push_quote(0, &q);

        let (tx, _rx) = crossbeam_channel::unbounded();
        let handle = std::thread::spawn(|| {});
        let client = EClient::from_parts(shared, tx, handle, "DU123".into());

        // Map reqId 5 → instrument 0
        client.req_to_instrument.lock().unwrap().insert(5, 0);

        let quote = client.quote(5).unwrap();
        assert_eq!(quote.bid, 200 * PRICE_SCALE);

        // Unmapped reqId returns None
        assert!(client.quote(99).is_none());
    }

    #[test]
    fn quote_by_instrument_direct() {
        let shared = Arc::new(SharedState::new());
        let mut q = Quote::default();
        q.ask = 300 * PRICE_SCALE;
        shared.push_quote(2, &q);

        let (tx, _rx) = crossbeam_channel::unbounded();
        let handle = std::thread::spawn(|| {});
        let client = EClient::from_parts(shared, tx, handle, "DU123".into());

        let quote = client.quote_by_instrument(2);
        assert_eq!(quote.ask, 300 * PRICE_SCALE);
    }

    #[test]
    fn process_msgs_dispatches_historical_data() {
        use crate::control::historical::{HistoricalResponse, HistoricalBar};
        let shared = Arc::new(SharedState::new());

        shared.push_historical_data(5, HistoricalResponse {
            query_id: String::new(),
            timezone: String::new(),
            bars: vec![
                HistoricalBar {
                    time: "20260101".into(),
                    open: 100.0, high: 105.0, low: 99.0, close: 103.0,
                    volume: 1000, wap: 102.0, count: 50,
                },
            ],
            is_complete: true,
        });

        let (tx, _rx) = crossbeam_channel::unbounded();
        let handle = std::thread::spawn(|| {});
        let client = EClient::from_parts(shared, tx, handle, "DU123".into());

        let mut wrapper = RecordingWrapper::default();
        client.process_msgs(&mut wrapper);

        assert!(wrapper.events.iter().any(|e| e == "historical_data:5:20260101"));
        assert!(wrapper.events.iter().any(|e| e == "historical_data_end:5"));
    }
}
