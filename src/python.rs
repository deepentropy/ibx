//! PyO3 bindings for ibx. Feature-gated behind `python`.
//!
//! Exposes `IbEngine` as the main entry point for Python:
//! ```python
//! import ibx
//! engine = ibx.connect(username="user", password="pass", paper=True)
//! spy = engine.subscribe(conid=756733, symbol="SPY")
//! quote = engine.quote(spy)
//! order_id = engine.submit_limit(spy, "BUY", qty=1, price=680.50)
//! fills = engine.fills()
//! engine.shutdown()
//! ```

use std::sync::Arc;
use std::thread;

use crossbeam_channel::Sender;
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

use crate::bridge::{BridgeStrategy, SharedState};
use crate::gateway::{Gateway, GatewayConfig};
use crate::types::*;

const PRICE_SCALE_F: f64 = PRICE_SCALE as f64;

// ── Python-visible types ──

#[pyclass]
#[derive(Clone)]
struct PyQuote {
    #[pyo3(get)]
    bid: f64,
    #[pyo3(get)]
    ask: f64,
    #[pyo3(get)]
    last: f64,
    #[pyo3(get)]
    bid_size: f64,
    #[pyo3(get)]
    ask_size: f64,
    #[pyo3(get)]
    last_size: f64,
    #[pyo3(get)]
    volume: f64,
    #[pyo3(get)]
    open: f64,
    #[pyo3(get)]
    high: f64,
    #[pyo3(get)]
    low: f64,
    #[pyo3(get)]
    close: f64,
    #[pyo3(get)]
    timestamp_ns: u64,
}

#[pymethods]
impl PyQuote {
    fn __repr__(&self) -> String {
        format!("Quote(bid={}, ask={}, last={})", self.bid, self.ask, self.last)
    }
}

impl From<Quote> for PyQuote {
    fn from(q: Quote) -> Self {
        Self {
            bid: q.bid as f64 / PRICE_SCALE_F,
            ask: q.ask as f64 / PRICE_SCALE_F,
            last: q.last as f64 / PRICE_SCALE_F,
            bid_size: q.bid_size as f64 / QTY_SCALE as f64,
            ask_size: q.ask_size as f64 / QTY_SCALE as f64,
            last_size: q.last_size as f64 / QTY_SCALE as f64,
            volume: q.volume as f64 / QTY_SCALE as f64,
            open: q.open as f64 / PRICE_SCALE_F,
            high: q.high as f64 / PRICE_SCALE_F,
            low: q.low as f64 / PRICE_SCALE_F,
            close: q.close as f64 / PRICE_SCALE_F,
            timestamp_ns: q.timestamp_ns,
        }
    }
}

#[pyclass]
#[derive(Clone)]
struct PyFill {
    #[pyo3(get)]
    instrument: u32,
    #[pyo3(get)]
    order_id: u64,
    #[pyo3(get)]
    side: String,
    #[pyo3(get)]
    price: f64,
    #[pyo3(get)]
    qty: i64,
    #[pyo3(get)]
    remaining: i64,
    #[pyo3(get)]
    commission: f64,
    #[pyo3(get)]
    timestamp_ns: u64,
}

#[pymethods]
impl PyFill {
    fn __repr__(&self) -> String {
        format!("Fill(order_id={}, side={}, price={}, qty={})", self.order_id, self.side, self.price, self.qty)
    }
}

impl From<Fill> for PyFill {
    fn from(f: Fill) -> Self {
        Self {
            instrument: f.instrument,
            order_id: f.order_id,
            side: match f.side { Side::Buy => "BUY".into(), Side::Sell => "SELL".into(), Side::ShortSell => "SSHORT".into() },
            price: f.price as f64 / PRICE_SCALE_F,
            qty: f.qty,
            remaining: f.remaining,
            commission: f.commission as f64 / PRICE_SCALE_F,
            timestamp_ns: f.timestamp_ns,
        }
    }
}

#[pyclass]
#[derive(Clone)]
struct PyOrderUpdate {
    #[pyo3(get)]
    order_id: u64,
    #[pyo3(get)]
    instrument: u32,
    #[pyo3(get)]
    status: String,
    #[pyo3(get)]
    filled_qty: i64,
    #[pyo3(get)]
    remaining_qty: i64,
    #[pyo3(get)]
    timestamp_ns: u64,
}

#[pymethods]
impl PyOrderUpdate {
    fn __repr__(&self) -> String {
        format!("OrderUpdate(order_id={}, status={})", self.order_id, self.status)
    }
}

impl From<OrderUpdate> for PyOrderUpdate {
    fn from(u: OrderUpdate) -> Self {
        Self {
            order_id: u.order_id,
            instrument: u.instrument,
            status: format!("{:?}", u.status),
            filled_qty: u.filled_qty,
            remaining_qty: u.remaining_qty,
            timestamp_ns: u.timestamp_ns,
        }
    }
}

#[pyclass]
#[derive(Clone)]
struct PyAccountState {
    #[pyo3(get)]
    net_liquidation: f64,
    #[pyo3(get)]
    buying_power: f64,
    #[pyo3(get)]
    margin_used: f64,
    #[pyo3(get)]
    unrealized_pnl: f64,
    #[pyo3(get)]
    realized_pnl: f64,
}

#[pymethods]
impl PyAccountState {
    fn __repr__(&self) -> String {
        format!("AccountState(net_liq={}, buying_power={})", self.net_liquidation, self.buying_power)
    }
}

impl From<AccountState> for PyAccountState {
    fn from(a: AccountState) -> Self {
        Self {
            net_liquidation: a.net_liquidation as f64 / PRICE_SCALE_F,
            buying_power: a.buying_power as f64 / PRICE_SCALE_F,
            margin_used: a.margin_used as f64 / PRICE_SCALE_F,
            unrealized_pnl: a.unrealized_pnl as f64 / PRICE_SCALE_F,
            realized_pnl: a.realized_pnl as f64 / PRICE_SCALE_F,
        }
    }
}

// ── Main engine class ──

#[pyclass]
struct IbEngine {
    shared: Arc<SharedState>,
    control_tx: Sender<ControlCommand>,
    next_order_id: std::sync::atomic::AtomicU64,
    /// Handle to the hot loop thread (for join on shutdown).
    _thread: Option<thread::JoinHandle<()>>,
    account_id: String,
}

#[pymethods]
impl IbEngine {
    /// Subscribe to market data for an instrument.
    /// Returns the InstrumentId used for subsequent calls.
    fn subscribe(&self, conid: i64, symbol: String) -> PyResult<u32> {
        // Register instrument in the hot loop
        self.control_tx.send(ControlCommand::RegisterInstrument { con_id: conid })
            .map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        self.control_tx.send(ControlCommand::Subscribe { con_id: conid, symbol })
            .map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        // Return instrument count - 1 as the ID (matches MarketState::register behavior)
        // Wait briefly for the hot loop to process the register command
        std::thread::sleep(std::time::Duration::from_millis(10));
        Ok(self.shared.instrument_count().saturating_sub(1))
    }

    /// Unsubscribe from market data.
    fn unsubscribe(&self, instrument: u32) -> PyResult<()> {
        self.control_tx.send(ControlCommand::Unsubscribe { instrument })
            .map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(())
    }

    /// Read the latest quote for an instrument (lock-free SeqLock read).
    fn quote(&self, instrument: u32) -> PyQuote {
        self.shared.quote(instrument).into()
    }

    /// Read the current position for an instrument.
    fn position(&self, instrument: u32) -> i64 {
        self.shared.position(instrument)
    }

    /// Read the account state.
    fn account(&self) -> PyAccountState {
        self.shared.account().into()
    }

    /// Drain all pending fills since last call.
    fn fills(&self) -> Vec<PyFill> {
        self.shared.drain_fills().into_iter().map(|f| f.into()).collect()
    }

    /// Drain all pending order updates since last call.
    fn order_updates(&self) -> Vec<PyOrderUpdate> {
        self.shared.drain_order_updates().into_iter().map(|u| u.into()).collect()
    }

    /// Submit a limit order. Returns OrderId.
    fn submit_limit(&self, instrument: u32, side: &str, qty: u32, price: f64) -> PyResult<u64> {
        let order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let side = parse_side(side)?;
        let price_fixed = (price * PRICE_SCALE_F) as i64;
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimit {
            order_id, instrument, side, qty, price: price_fixed,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(order_id)
    }

    /// Submit a market order. Returns OrderId.
    fn submit_market(&self, instrument: u32, side: &str, qty: u32) -> PyResult<u64> {
        let order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let side = parse_side(side)?;
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitMarket {
            order_id, instrument, side, qty,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(order_id)
    }

    /// Submit a stop order. Returns OrderId.
    fn submit_stop(&self, instrument: u32, side: &str, qty: u32, stop_price: f64) -> PyResult<u64> {
        let order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let side = parse_side(side)?;
        let stop_fixed = (stop_price * PRICE_SCALE_F) as i64;
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitStop {
            order_id, instrument, side, qty, stop_price: stop_fixed,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(order_id)
    }

    /// Submit a stop-limit order. Returns OrderId.
    fn submit_stop_limit(&self, instrument: u32, side: &str, qty: u32, price: f64, stop_price: f64) -> PyResult<u64> {
        let order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let side = parse_side(side)?;
        let price_fixed = (price * PRICE_SCALE_F) as i64;
        let stop_fixed = (stop_price * PRICE_SCALE_F) as i64;
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitStopLimit {
            order_id, instrument, side, qty, price: price_fixed, stop_price: stop_fixed,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(order_id)
    }

    /// Submit a GTC limit order. Returns OrderId.
    fn submit_limit_gtc(&self, instrument: u32, side: &str, qty: u32, price: f64, outside_rth: bool) -> PyResult<u64> {
        let order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let side = parse_side(side)?;
        let price_fixed = (price * PRICE_SCALE_F) as i64;
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimitGtc {
            order_id, instrument, side, qty, price: price_fixed, outside_rth,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(order_id)
    }

    /// Submit a GTC stop order. Returns OrderId.
    fn submit_stop_gtc(&self, instrument: u32, side: &str, qty: u32, stop_price: f64, outside_rth: bool) -> PyResult<u64> {
        let order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let side = parse_side(side)?;
        let stop_fixed = (stop_price * PRICE_SCALE_F) as i64;
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitStopGtc {
            order_id, instrument, side, qty, stop_price: stop_fixed, outside_rth,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(order_id)
    }

    /// Submit a GTC stop-limit order. Returns OrderId.
    fn submit_stop_limit_gtc(&self, instrument: u32, side: &str, qty: u32, price: f64, stop_price: f64, outside_rth: bool) -> PyResult<u64> {
        let order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let side = parse_side(side)?;
        let price_fixed = (price * PRICE_SCALE_F) as i64;
        let stop_fixed = (stop_price * PRICE_SCALE_F) as i64;
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitStopLimitGtc {
            order_id, instrument, side, qty, price: price_fixed, stop_price: stop_fixed, outside_rth,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(order_id)
    }

    /// Submit an IOC limit order. Returns OrderId.
    fn submit_limit_ioc(&self, instrument: u32, side: &str, qty: u32, price: f64) -> PyResult<u64> {
        let order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let side = parse_side(side)?;
        let price_fixed = (price * PRICE_SCALE_F) as i64;
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimitIoc {
            order_id, instrument, side, qty, price: price_fixed,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(order_id)
    }

    /// Submit a FOK limit order. Returns OrderId.
    fn submit_limit_fok(&self, instrument: u32, side: &str, qty: u32, price: f64) -> PyResult<u64> {
        let order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let side = parse_side(side)?;
        let price_fixed = (price * PRICE_SCALE_F) as i64;
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimitFok {
            order_id, instrument, side, qty, price: price_fixed,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(order_id)
    }

    /// Submit a trailing stop order. Returns OrderId.
    fn submit_trailing_stop(&self, instrument: u32, side: &str, qty: u32, trail_amt: f64) -> PyResult<u64> {
        let order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let side = parse_side(side)?;
        let trail_fixed = (trail_amt * PRICE_SCALE_F) as i64;
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitTrailingStop {
            order_id, instrument, side, qty, trail_amt: trail_fixed,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(order_id)
    }

    /// Submit a trailing stop-limit order. Returns OrderId.
    fn submit_trailing_stop_limit(&self, instrument: u32, side: &str, qty: u32, price: f64, trail_amt: f64) -> PyResult<u64> {
        let order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let side = parse_side(side)?;
        let price_fixed = (price * PRICE_SCALE_F) as i64;
        let trail_fixed = (trail_amt * PRICE_SCALE_F) as i64;
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitTrailingStopLimit {
            order_id, instrument, side, qty, price: price_fixed, trail_amt: trail_fixed,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(order_id)
    }

    /// Submit a Trailing Stop by percentage. trail_pct is in basis points (100 = 1%).
    fn submit_trailing_stop_pct(&self, instrument: u32, side: &str, qty: u32, trail_pct: u32) -> PyResult<u64> {
        let order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let side = parse_side(side)?;
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitTrailingStopPct {
            order_id, instrument, side, qty, trail_pct,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(order_id)
    }

    /// Submit a Market on Close order. Returns OrderId.
    fn submit_moc(&self, instrument: u32, side: &str, qty: u32) -> PyResult<u64> {
        let order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let side = parse_side(side)?;
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitMoc {
            order_id, instrument, side, qty,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(order_id)
    }

    /// Submit a Limit on Close order. Returns OrderId.
    fn submit_loc(&self, instrument: u32, side: &str, qty: u32, price: f64) -> PyResult<u64> {
        let order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let side = parse_side(side)?;
        let price_fixed = (price * PRICE_SCALE_F) as i64;
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitLoc {
            order_id, instrument, side, qty, price: price_fixed,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(order_id)
    }

    /// Submit a Market if Touched order. Returns OrderId.
    fn submit_mit(&self, instrument: u32, side: &str, qty: u32, stop_price: f64) -> PyResult<u64> {
        let order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let side = parse_side(side)?;
        let stop_fixed = (stop_price * PRICE_SCALE_F) as i64;
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitMit {
            order_id, instrument, side, qty, stop_price: stop_fixed,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(order_id)
    }

    /// Submit a Limit if Touched order. Returns OrderId.
    fn submit_lit(&self, instrument: u32, side: &str, qty: u32, price: f64, stop_price: f64) -> PyResult<u64> {
        let order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let side = parse_side(side)?;
        let price_fixed = (price * PRICE_SCALE_F) as i64;
        let stop_fixed = (stop_price * PRICE_SCALE_F) as i64;
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitLit {
            order_id, instrument, side, qty, price: price_fixed, stop_price: stop_fixed,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(order_id)
    }

    /// Submit a bracket order (entry + take-profit + stop-loss). Returns (parent_id, tp_id, sl_id).
    fn submit_bracket(&self, instrument: u32, side: &str, qty: u32, entry_price: f64, take_profit: f64, stop_loss: f64) -> PyResult<(u64, u64, u64)> {
        let parent_id = self.next_order_id.fetch_add(3, std::sync::atomic::Ordering::Relaxed);
        let tp_id = parent_id + 1;
        let sl_id = parent_id + 2;
        let side = parse_side(side)?;
        let entry_fixed = (entry_price * PRICE_SCALE_F) as i64;
        let tp_fixed = (take_profit * PRICE_SCALE_F) as i64;
        let sl_fixed = (stop_loss * PRICE_SCALE_F) as i64;
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitBracket {
            parent_id, tp_id, sl_id, instrument, side, qty,
            entry_price: entry_fixed, take_profit: tp_fixed, stop_loss: sl_fixed,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok((parent_id, tp_id, sl_id))
    }

    /// Submit a limit order with extended attributes.
    /// tif: "DAY", "GTC", or "GTD". Optional: display_size (iceberg), hidden, outside_rth,
    /// good_after (unix secs), good_till (unix secs).
    #[pyo3(signature = (instrument, side, qty, price, tif="DAY", display_size=0, min_qty=0, hidden=false, outside_rth=false, good_after=0, good_till=0, oca_group=0))]
    fn submit_limit_ex(
        &self, instrument: u32, side: &str, qty: u32, price: f64,
        tif: &str, display_size: u32, min_qty: u32, hidden: bool, outside_rth: bool,
        good_after: i64, good_till: i64, oca_group: u64,
    ) -> PyResult<u64> {
        let order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let side = parse_side(side)?;
        let price_fixed = (price * PRICE_SCALE_F) as i64;
        let tif_byte = match tif {
            "DAY" => b'0',
            "GTC" => b'1',
            "GTD" => b'6',
            "DTC" => b'6',
            _ => return Err(PyRuntimeError::new_err(format!("Invalid TIF: {}. Use DAY, GTC, GTD, or DTC", tif))),
        };
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimitEx {
            order_id, instrument, side, qty, price: price_fixed, tif: tif_byte,
            attrs: OrderAttrs { display_size, min_qty, hidden, outside_rth, good_after, good_till, oca_group },
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(order_id)
    }

    /// Submit a Relative / Pegged-to-Primary order. Offset is the peg offset from NBBO.
    fn submit_rel(&self, instrument: u32, side: &str, qty: u32, offset: f64) -> PyResult<u64> {
        let order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let side = parse_side(side)?;
        let offset_fixed = (offset * PRICE_SCALE_F) as i64;
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitRel {
            order_id, instrument, side, qty, offset: offset_fixed,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(order_id)
    }

    /// Submit a Limit order for the opening auction (TIF=OPG).
    fn submit_limit_opg(&self, instrument: u32, side: &str, qty: u32, price: f64) -> PyResult<u64> {
        let order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let side = parse_side(side)?;
        let price_fixed = (price * PRICE_SCALE_F) as i64;
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimitOpg {
            order_id, instrument, side, qty, price: price_fixed,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(order_id)
    }

    /// Submit an adaptive algo limit order. Priority: "Patient", "Normal", or "Urgent".
    fn submit_adaptive(&self, instrument: u32, side: &str, qty: u32, price: f64, priority: &str) -> PyResult<u64> {
        let order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let side = parse_side(side)?;
        let price_fixed = (price * PRICE_SCALE_F) as i64;
        let priority = match priority {
            "Patient" => AdaptivePriority::Patient,
            "Normal" => AdaptivePriority::Normal,
            "Urgent" => AdaptivePriority::Urgent,
            _ => return Err(PyRuntimeError::new_err(format!("Invalid priority: {}. Use Patient, Normal, or Urgent", priority))),
        };
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitAdaptive {
            order_id, instrument, side, qty, price: price_fixed, priority,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(order_id)
    }

    /// Cancel an order by OrderId.
    fn cancel(&self, order_id: u64) -> PyResult<()> {
        self.control_tx.send(ControlCommand::Order(OrderRequest::Cancel { order_id }))
            .map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(())
    }

    /// Cancel all orders for an instrument.
    fn cancel_all(&self, instrument: u32) -> PyResult<()> {
        self.control_tx.send(ControlCommand::Order(OrderRequest::CancelAll { instrument }))
            .map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(())
    }

    /// Modify an existing order. Returns new OrderId.
    fn modify(&self, order_id: u64, price: f64, qty: u32) -> PyResult<u64> {
        let new_order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let price_fixed = (price * PRICE_SCALE_F) as i64;
        self.control_tx.send(ControlCommand::Order(OrderRequest::Modify {
            new_order_id, order_id, price: price_fixed, qty,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(new_order_id)
    }

    /// Gracefully shutdown the engine.
    fn shutdown(&self) -> PyResult<()> {
        self.control_tx.send(ControlCommand::Shutdown)
            .map_err(|e| PyRuntimeError::new_err(format!("Engine already stopped: {}", e)))?;
        Ok(())
    }

    /// Get the IB account ID.
    #[getter]
    fn account_id(&self) -> &str {
        &self.account_id
    }
}

fn parse_side(s: &str) -> PyResult<Side> {
    match s.to_uppercase().as_str() {
        "BUY" | "B" => Ok(Side::Buy),
        "SELL" | "S" => Ok(Side::Sell),
        "SSHORT" | "SS" => Ok(Side::ShortSell),
        _ => Err(PyRuntimeError::new_err(format!("Invalid side '{}': use 'BUY' or 'SELL'", s))),
    }
}

// ── Module-level connect function ──

/// Connect to IB and start the engine. Returns an IbEngine handle.
#[pyfunction]
#[pyo3(signature = (username, password, paper=true, host="cdc1.ibllc.com".to_string(), core_id=None))]
fn connect(
    py: Python<'_>,
    username: String,
    password: String,
    paper: bool,
    host: String,
    core_id: Option<usize>,
) -> PyResult<IbEngine> {
    let config = GatewayConfig { username, password, host, paper };

    // Release GIL during the blocking connect (~21s)
    let result = py.allow_threads(|| Gateway::connect(&config));
    let (gw, farm_conn, ccp_conn, hmds_conn) = result
        .map_err(|e| PyRuntimeError::new_err(format!("Connection failed: {}", e)))?;

    let account_id = gw.account_id.clone();
    let shared = Arc::new(SharedState::new());
    let shared_clone = shared.clone();
    let strategy = BridgeStrategy::new(shared_clone);

    let (mut hot_loop, control_tx) = gw.into_hot_loop(strategy, farm_conn, ccp_conn, hmds_conn, core_id);

    // Start order ID counter from epoch-based value (matches Context behavior)
    let start_id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() * 1000;

    // Spawn hot loop on a dedicated thread
    let handle = thread::Builder::new()
        .name("ib-engine-hotloop".into())
        .spawn(move || {
            hot_loop.run();
        })
        .map_err(|e| PyRuntimeError::new_err(format!("Failed to spawn hot loop thread: {}", e)))?;

    Ok(IbEngine {
        shared,
        control_tx,
        next_order_id: std::sync::atomic::AtomicU64::new(start_id),
        _thread: Some(handle),
        account_id,
    })
}

/// Python module definition.
#[pymodule]
fn ibx(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(connect, m)?)?;
    m.add_class::<IbEngine>()?;
    m.add_class::<PyQuote>()?;
    m.add_class::<PyFill>()?;
    m.add_class::<PyOrderUpdate>()?;
    m.add_class::<PyAccountState>()?;
    m.add("PRICE_SCALE", PRICE_SCALE)?;
    m.add("QTY_SCALE", QTY_SCALE)?;
    Ok(())
}
