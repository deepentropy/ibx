//! Test helper methods (hidden from public API).

use std::sync::Arc;
use std::sync::atomic::Ordering;

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

use crate::bridge::{Event, SharedState};
use crate::control::historical::{HistoricalBar, HistoricalResponse, HeadTimestampResponse};
use crate::types::*;

use super::EClient;

#[pymethods]
impl EClient {
    /// Create a fake "connected" EClient backed by a SharedState + crossbeam channel.
    #[doc(hidden)]
    #[pyo3(signature = (account_id="TEST123".to_string()))]
    fn _test_connect(&self, account_id: String) -> PyResult<()> {
        if self.connected.load(Ordering::Relaxed) {
            return Err(PyRuntimeError::new_err("Already connected"));
        }
        let shared = Arc::new(SharedState::new());
        let (tx, _rx) = crossbeam_channel::unbounded();
        let (event_tx, event_rx) = crossbeam_channel::bounded(256);
        *self.shared.lock().unwrap() = Some(shared);
        *self.control_tx.lock().unwrap() = Some(tx);
        *self.event_rx.lock().unwrap() = Some(event_rx);
        *self.account_id.lock().unwrap() = Some(account_id);
        // Store event_tx so _test_push_disconnect_event can use it.
        *self._test_event_tx.lock().unwrap() = Some(event_tx);
        self.next_order_id.store(1000, Ordering::Relaxed);
        self.connected.store(true, Ordering::Release);
        Ok(())
    }

    /// Map a reqId to an instrument slot.
    #[doc(hidden)]
    fn _test_map_instrument(&self, req_id: i64, instrument: u32) {
        self.core.req_to_instrument.lock().unwrap().insert(req_id, instrument);
        self.core.instrument_to_req.lock().unwrap().insert(instrument, req_id);
    }

    /// Set instrument count on SharedState.
    #[doc(hidden)]
    fn _test_set_instrument_count(&self, count: u32) -> PyResult<()> {
        let shared = self.shared_state()?;
        shared.market.set_instrument_count(count);
        Ok(())
    }

    /// Push a quote into SharedState for a given instrument.
    #[doc(hidden)]
    #[pyo3(signature = (instrument, bid=0.0, ask=0.0, last=0.0, bid_size=0, ask_size=0, last_size=0, volume=0, open=0.0, high=0.0, low=0.0, close=0.0))]
    fn _test_push_quote(
        &self, instrument: u32,
        bid: f64, ask: f64, last: f64,
        bid_size: i64, ask_size: i64, last_size: i64,
        volume: i64, open: f64, high: f64, low: f64, close: f64,
    ) -> PyResult<()> {
        let shared = self.shared_state()?;
        let ps = PRICE_SCALE as f64;
        let q = Quote {
            bid: (bid * ps) as i64, ask: (ask * ps) as i64, last: (last * ps) as i64,
            bid_size: bid_size * QTY_SCALE, ask_size: ask_size * QTY_SCALE,
            last_size: last_size * QTY_SCALE,
            volume: volume * QTY_SCALE,
            open: (open * ps) as i64, high: (high * ps) as i64,
            low: (low * ps) as i64, close: (close * ps) as i64,
            timestamp_ns: 1,
        };
        shared.market.push_quote(instrument, &q);
        Ok(())
    }

    /// Push a fill into SharedState.
    #[doc(hidden)]
    #[pyo3(signature = (instrument, order_id, side, price, qty, remaining, commission=0.0))]
    fn _test_push_fill(
        &self, instrument: u32, order_id: u64, side: &str,
        price: f64, qty: i64, remaining: i64, commission: f64,
    ) -> PyResult<()> {
        let shared = self.shared_state()?;
        let s = match side {
            "BUY" => Side::Buy,
            "SELL" => Side::Sell,
            "SSHORT" => Side::ShortSell,
            _ => return Err(PyRuntimeError::new_err(format!("Invalid side: {}", side))),
        };
        let ps = PRICE_SCALE as f64;
        shared.orders.push_fill(Fill {
            instrument, order_id, side: s,
            price: (price * ps) as i64, qty, remaining,
            commission: (commission * ps) as i64,
            timestamp_ns: 100,
        });
        Ok(())
    }

    /// Push an order update into SharedState.
    #[doc(hidden)]
    fn _test_push_order_update(
        &self, order_id: u64, instrument: u32, status: &str,
        filled_qty: i64, remaining_qty: i64,
    ) -> PyResult<()> {
        let shared = self.shared_state()?;
        let st = match status {
            "PendingSubmit" => OrderStatus::PendingSubmit,
            "Submitted" => OrderStatus::Submitted,
            "Filled" => OrderStatus::Filled,
            "Cancelled" => OrderStatus::Cancelled,
            "Rejected" => OrderStatus::Rejected,
            _ => return Err(PyRuntimeError::new_err(format!("Invalid status: {}", status))),
        };
        shared.orders.push_order_update(OrderUpdate {
            order_id, instrument, status: st, filled_qty, remaining_qty, timestamp_ns: 100,
        });
        Ok(())
    }

    /// Push a cancel reject into SharedState.
    #[doc(hidden)]
    fn _test_push_cancel_reject(&self, order_id: u64, instrument: u32, reason_code: i32) -> PyResult<()> {
        let shared = self.shared_state()?;
        shared.orders.push_cancel_reject(CancelReject {
            order_id, instrument, reject_type: 1, reason_code, timestamp_ns: 100,
        });
        Ok(())
    }

    /// Push a TBT trade into SharedState.
    #[doc(hidden)]
    fn _test_push_tbt_trade(
        &self, instrument: u32, price: f64, size: i64, exchange: &str,
    ) -> PyResult<()> {
        let shared = self.shared_state()?;
        let ps = PRICE_SCALE as f64;
        shared.market.push_tbt_trade(TbtTrade {
            instrument, price: (price * ps) as i64, size,
            exchange: exchange.to_string(), conditions: String::new(), timestamp: 12345,
        });
        Ok(())
    }

    /// Push a TBT quote into SharedState.
    #[doc(hidden)]
    fn _test_push_tbt_quote(
        &self, instrument: u32, bid: f64, ask: f64, bid_size: i64, ask_size: i64,
    ) -> PyResult<()> {
        let shared = self.shared_state()?;
        let ps = PRICE_SCALE as f64;
        shared.market.push_tbt_quote(TbtQuote {
            instrument,
            bid: (bid * ps) as i64, ask: (ask * ps) as i64,
            bid_size, ask_size, timestamp: 12345,
        });
        Ok(())
    }

    /// Push historical data into SharedState.
    #[doc(hidden)]
    fn _test_push_historical_data(
        &self, req_id: u32, bars: Vec<(String, f64, f64, f64, f64, i64)>, is_complete: bool,
    ) -> PyResult<()> {
        let shared = self.shared_state()?;
        let bar_list: Vec<HistoricalBar> = bars.into_iter().map(|(time, o, h, l, c, v)| {
            HistoricalBar { time, open: o, high: h, low: l, close: c, volume: v, wap: 0.0, count: 0 }
        }).collect();
        shared.reference.push_historical_data(req_id, HistoricalResponse {
            query_id: String::new(), timezone: String::new(), bars: bar_list, is_complete,
        });
        Ok(())
    }

    /// Push a head timestamp into SharedState.
    #[doc(hidden)]
    fn _test_push_head_timestamp(&self, req_id: u32, timestamp: &str) -> PyResult<()> {
        let shared = self.shared_state()?;
        shared.reference.push_head_timestamp(req_id, HeadTimestampResponse {
            head_timestamp: timestamp.to_string(), timezone: String::new(),
        });
        Ok(())
    }

    /// Push account state into SharedState.
    #[doc(hidden)]
    #[pyo3(signature = (net_liquidation=0.0, buying_power=0.0, daily_pnl=0.0, unrealized_pnl=0.0, realized_pnl=0.0))]
    fn _test_set_account(
        &self, net_liquidation: f64, buying_power: f64,
        daily_pnl: f64, unrealized_pnl: f64, realized_pnl: f64,
    ) -> PyResult<()> {
        let shared = self.shared_state()?;
        let ps = PRICE_SCALE as f64;
        let mut acct = shared.portfolio.account();
        acct.net_liquidation = (net_liquidation * ps) as i64;
        acct.buying_power = (buying_power * ps) as i64;
        acct.daily_pnl = (daily_pnl * ps) as i64;
        acct.unrealized_pnl = (unrealized_pnl * ps) as i64;
        acct.realized_pnl = (realized_pnl * ps) as i64;
        shared.portfolio.set_account(&acct);
        Ok(())
    }

    /// Push a position into SharedState.
    #[doc(hidden)]
    fn _test_set_position(&self, con_id: i64, position: i64, avg_cost: f64) -> PyResult<()> {
        let shared = self.shared_state()?;
        let ps = PRICE_SCALE as f64;
        shared.portfolio.set_position_info(PositionInfo {
            con_id, position, avg_cost: (avg_cost * ps) as i64,
        });
        Ok(())
    }

    /// Run ONE iteration of the event dispatch loop.
    #[doc(hidden)]
    fn _test_dispatch_once(&self, py: Python<'_>) -> PyResult<()> {
        if !self.connected.load(std::sync::atomic::Ordering::Acquire) {
            return Err(PyRuntimeError::new_err("Not connected"));
        }
        let shared = self.shared_state()?;
        self.dispatch_once(py, &shared)
    }

    /// Inject an Event::Disconnected into the event channel (test-only).
    #[doc(hidden)]
    fn _test_push_disconnect_event(&self) -> PyResult<()> {
        let tx = self._test_event_tx.lock().unwrap();
        let tx = tx.as_ref().ok_or_else(|| PyRuntimeError::new_err("No event channel"))?;
        tx.send(Event::Disconnected).map_err(|e| PyRuntimeError::new_err(format!("{}", e)))
    }
}
