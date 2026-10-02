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
    /// `control_capacity` bounds the command channel (unbounded by default).
    #[doc(hidden)]
    #[pyo3(signature = (account_id="TEST123".to_string(), control_capacity=None))]
    fn _test_connect(&self, account_id: String, control_capacity: Option<usize>) -> PyResult<()> {
        if self.connected.load(Ordering::Relaxed) {
            return Err(PyRuntimeError::new_err("Already connected"));
        }
        let shared = Arc::new(SharedState::new());
        let (tx, rx) = match control_capacity {
            Some(n) => crossbeam_channel::bounded(n),
            None => crossbeam_channel::unbounded(),
        };
        let (event_tx, event_rx) = crossbeam_channel::bounded(256);
        *self.shared.lock().unwrap() = Some(shared);
        *self.control_tx.lock().unwrap() = Some(tx);
        *self.event_rx.lock().unwrap() = Some(event_rx);
        *self.account_id.lock().unwrap() = Some(account_id);
        // Store event_tx so _test_push_disconnect_event can use it.
        *self._test_event_tx.lock().unwrap() = Some(event_tx);
        // Keep the command receiver: commands sent to the absent engine
        // must not fail as "Engine stopped".
        *self._test_control_rx.lock().unwrap() = Some(rx);
        self.next_order_id.store(1000, Ordering::Relaxed);
        *self.connection_time.lock().unwrap() = Some(crate::client_core::connection_time_now());
        self.connected.store(true, Ordering::Release);
        Ok(())
    }

    /// Map a reqId to an instrument slot. The request takes every tick of
    /// the instrument, its headlines too.
    #[doc(hidden)]
    fn _test_map_instrument(&self, req_id: i64, instrument: u32) {
        self.core.req_to_instrument.lock().unwrap().insert(req_id, instrument);
        self.core.instrument_to_req.lock().unwrap().entry(instrument).or_default().push(req_id);
        self.core.md_news.lock().unwrap().insert(req_id, String::new());
    }

    /// Set instrument count on SharedState.
    #[doc(hidden)]
    fn _test_set_instrument_count(&self, count: u32) -> PyResult<()> {
        let shared = self.shared_state()?;
        shared.market.set_instrument_count(count);
        Ok(())
    }

    /// Push a quote into SharedState for a given instrument.
    /// `timestamp` is the last trade time in epoch seconds; without it the
    /// time is 1 ns.
    #[doc(hidden)]
    #[pyo3(signature = (instrument, bid=0.0, ask=0.0, last=0.0, bid_size=0, ask_size=0, last_size=0, volume=0, open=0.0, high=0.0, low=0.0, close=0.0, timestamp=None))]
    #[allow(clippy::too_many_arguments)]
    fn _test_push_quote(
        &self, instrument: u32,
        bid: f64, ask: f64, last: f64,
        bid_size: i64, ask_size: i64, last_size: i64,
        volume: i64, open: f64, high: f64, low: f64, close: f64, timestamp: Option<u64>,
    ) -> PyResult<()> {
        let shared = self.shared_state()?;
        let ps = PRICE_SCALE as f64;
        let q = Quote {
            bid: (bid * ps).round() as i64, ask: (ask * ps).round() as i64, last: (last * ps).round() as i64,
            bid_size: bid_size * QTY_SCALE, ask_size: ask_size * QTY_SCALE,
            last_size: last_size * QTY_SCALE,
            volume: volume * QTY_SCALE,
            open: (open * ps).round() as i64, high: (high * ps).round() as i64,
            low: (low * ps).round() as i64, close: (close * ps).round() as i64,
            bid_exch_mask: 0, ask_exch_mask: 0, last_exch_mask: 0,
            timestamp_ns: timestamp.map_or(1, |s| s * 1_000_000_000),
        };
        shared.market.push_quote(instrument, &q);
        Ok(())
    }

    /// Push the marks of a quote (test-only, ibx#446): the trade status,
    /// the bid and ask auto-execution bits, and whether the daily figures
    /// came before the trade. `steps` gives the updates of one message, in
    /// its order: "quote", "daily", "trade", "time", "exchange";
    /// `sizes_seen` marks every size and the volume as given by the farm.
    #[doc(hidden)]
    #[pyo3(signature = (instrument, halted=None, auto_bits=None, daily_first=false, steps=None, sizes_seen=false))]
    fn _test_push_marks(
        &self, instrument: u32, halted: Option<i64>, auto_bits: Option<i64>, daily_first: bool,
        steps: Option<String>, sizes_seen: bool,
    ) -> PyResult<()> {
        let mut marks = QuoteMarks::default();
        if let Some(status) = halted { marks.set_halted(status); }
        if let Some(bits) = auto_bits { marks.set_auto_bits(bits); }
        marks.set_daily_first(daily_first);
        if let Some(steps) = steps {
            marks.begin_message();
            for step in steps.split(',').map(str::trim) {
                match step {
                    "quote" => marks.note_quote_update(),
                    "daily" => marks.begin_daily(),
                    "trade" => marks.begin_trade(),
                    "time" => marks.trade_gives(false),
                    "exchange" => marks.trade_gives(true),
                    _ => return Err(PyRuntimeError::new_err(format!("unknown step {step}"))),
                }
            }
        }
        if sizes_seen {
            for kind in [SizeKind::Bid, SizeKind::Ask, SizeKind::Last, SizeKind::Volume] {
                marks.set_seen(kind);
            }
        }
        self.shared_state()?.market.push_marks(instrument, marks);
        Ok(())
    }

    /// Queue the request parameters of an acked subscription (test-only).
    #[doc(hidden)]
    fn _test_push_tick_req_params(&self, instrument: u32, min_tick: f64, bbo_exchange: &str, snapshot_permissions: i32) -> PyResult<()> {
        self.shared_state()?.market.push_tick_req_params(crate::bridge::TickReqParams {
            instrument, min_tick, bbo_exchange: bbo_exchange.to_string(), snapshot_permissions,
        });
        Ok(())
    }

    /// Push a fill into SharedState.
    #[doc(hidden)]
    #[pyo3(signature = (instrument, order_id, side, price, qty, remaining, commission=0.0))]
    fn _test_push_fill(
        &self, instrument: u32, order_id: OrderId, side: &str,
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
        // The tests pass whole shares; fills are fixed-point.
        shared.orders.push_fill(Fill {
            cum_qty_fixed: 0, avg_price: 0,
            instrument, order_id, side: s,
            price: (price * ps) as i64, qty_fixed: qty * QTY_SCALE, remaining_fixed: remaining * QTY_SCALE,
            commission: (commission * ps) as i64,
            timestamp_ns: 100,
        });
        Ok(())
    }

    /// Run the second-factor code provider built from `callable` the way the
    /// login runs it: on its own thread, with the interpreter lock released
    /// by the caller (ibx#208).
    #[doc(hidden)]
    fn _test_code_provider(&self, py: Python<'_>, callable: Py<PyAny>, display_id: String, avth_url: String) -> PyResult<String> {
        let provider = super::python_code_provider(callable);
        let challenge = crate::auth::session::IbKeyChallenge { display_id, avth_url };
        py.detach(|| std::thread::spawn(move || provider(challenge)).join())
            .map_err(|_| PyRuntimeError::new_err("code provider thread panicked"))?
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))
    }

    /// Hold or release the open-order requests, as a lost auth link and the
    /// end of the order replay do (ibx#251).
    #[doc(hidden)]
    fn _test_set_open_orders_held(&self, held: bool) -> PyResult<()> {
        self.shared_state()?.orders.set_open_orders_held(held);
        Ok(())
    }

    /// Push an order update into SharedState.
    #[doc(hidden)]
    fn _test_push_order_update(
        &self, order_id: OrderId, instrument: u32, status: &str,
        filled_qty: i64, remaining_qty: i64,
    ) -> PyResult<()> {
        let shared = self.shared_state()?;
        let st = match status {
            "PendingSubmit" => OrderStatus::PendingSubmit,
            "PreSubmitted" => OrderStatus::PreSubmitted,
            "Submitted" => OrderStatus::Submitted,
            "PendingCancel" => OrderStatus::PendingCancel,
            "PendingReplace" => OrderStatus::PendingReplace,
            "Filled" => OrderStatus::Filled,
            "PartiallyFilled" => OrderStatus::PartiallyFilled,
            "Cancelled" => OrderStatus::Cancelled,
            "Rejected" => OrderStatus::Rejected,
            "Inactive" => OrderStatus::Inactive,
            _ => return Err(PyRuntimeError::new_err(format!("Invalid status: {}", status))),
        };
        shared.orders.push_order_update(OrderUpdate {
            avg_fill_price: 0,
            order_id, instrument, status: st,
            filled_qty_fixed: filled_qty * QTY_SCALE, remaining_qty_fixed: remaining_qty * QTY_SCALE,
            perm_id: 0, parent_id: 0, timestamp_ns: 100,
        });
        Ok(())
    }

    /// Push a completed order + rich info into SharedState (for req_completed_orders regression tests).
    #[doc(hidden)]
    #[pyo3(signature = (
        order_id, instrument, status, filled_qty,
        symbol, action, total_quantity, lmt_price,
        completed_status, completed_time, commission_and_fees_currency, warning_text, commission_and_fees,
    ))]
    fn _test_push_completed_order(
        &self,
        order_id: OrderId, instrument: u32, status: &str, filled_qty: i64,
        symbol: &str, action: &str, total_quantity: f64, lmt_price: f64,
        completed_status: &str, completed_time: &str, commission_and_fees_currency: &str,
        warning_text: &str, commission_and_fees: f64,
    ) -> PyResult<()> {
        use crate::api::types::{Contract as ApiContract, Order as ApiOrder, Execution as ApiExecution, OrderState as ApiOrderState};
        let shared = self.shared_state()?;
        let st = match status {
            "Filled" => OrderStatus::Filled,
            "Cancelled" => OrderStatus::Cancelled,
            "Rejected" => OrderStatus::Rejected,
            _ => return Err(PyRuntimeError::new_err(format!("Invalid status: {}", status))),
        };
        shared.orders.push_completed_order(crate::types::CompletedOrder {
            order_id, instrument, status: st, filled_qty_fixed: filled_qty * QTY_SCALE, timestamp_ns: 100,
        });
        shared.orders.push_order_info(order_id, crate::bridge::RichOrderInfo {
            contract: ApiContract {
                symbol: symbol.to_string(),
                sec_type: "STK".into(),
                exchange: "SMART".into(),
                currency: "USD".into(),
                ..Default::default()
            },
            order: ApiOrder {
                order_id: order_id,
                action: action.to_string(),
                total_quantity,
                order_type: "LMT".into(),
                lmt_price,
                ..Default::default()
            },
            order_state: ApiOrderState {
                status: status.to_string(),
                completed_status: completed_status.to_string(),
                completed_time: completed_time.to_string(),
                commission_and_fees_currency: commission_and_fees_currency.to_string(),
                warning_text: warning_text.to_string(),
                commission_and_fees,
                ..Default::default()
            },
            last_exec: ApiExecution::default(),
        });
        Ok(())
    }

    /// Track an order locally (for req_open_orders regression tests).
    #[doc(hidden)]
    fn _test_track_order(
        &self, order_id: OrderId, instrument: u32,
        symbol: &str, action: &str, total_quantity: f64, lmt_price: f64,
    ) -> PyResult<()> {
        use crate::api::types::{Contract as ApiContract, Order as ApiOrder};
        let contract = ApiContract {
            symbol: symbol.to_string(),
            sec_type: "STK".into(),
            exchange: "SMART".into(),
            currency: "USD".into(),
            ..Default::default()
        };
        let order = ApiOrder {
            order_id: order_id,
            action: action.to_string(),
            total_quantity,
            order_type: "LMT".into(),
            lmt_price,
            ..Default::default()
        };
        self.core.track_order(order_id, contract, order, instrument);
        Ok(())
    }

    /// Push a what-if response into SharedState (for dispatch regression tests).
    #[doc(hidden)]
    #[pyo3(signature = (
        order_id, instrument,
        init_margin_before, maint_margin_before, equity_with_loan_before,
        init_margin_after, maint_margin_after, equity_with_loan_after,
        commission,
    ))]
    fn _test_push_what_if(
        &self,
        order_id: OrderId, instrument: u32,
        init_margin_before: f64, maint_margin_before: f64, equity_with_loan_before: f64,
        init_margin_after: f64, maint_margin_after: f64, equity_with_loan_after: f64,
        commission: f64,
    ) -> PyResult<()> {
        let shared = self.shared_state()?;
        let ps = PRICE_SCALE as f64;
        shared.orders.push_what_if(crate::types::WhatIfResponse {
            order_id, instrument,
            init_margin_before: (init_margin_before * ps) as i64,
            maint_margin_before: (maint_margin_before * ps) as i64,
            equity_with_loan_before: (equity_with_loan_before * ps) as i64,
            init_margin_after: (init_margin_after * ps) as i64,
            maint_margin_after: (maint_margin_after * ps) as i64,
            equity_with_loan_after: (equity_with_loan_after * ps) as i64,
            commission: (commission * ps) as i64,
            state: crate::types::WhatIfState {
                status: "PreSubmitted".into(),
                init_margin_before: Some(init_margin_before),
                maint_margin_before: Some(maint_margin_before),
                equity_with_loan_before: Some(equity_with_loan_before),
                init_margin_after: Some(init_margin_after),
                maint_margin_after: Some(maint_margin_after),
                equity_with_loan_after: Some(equity_with_loan_after),
                commission: Some(commission),
                ..Default::default()
            },
            final_reply: true,
        });
        Ok(())
    }

    /// Push a cancel reject into SharedState.
    #[doc(hidden)]
    fn _test_push_cancel_reject(&self, order_id: OrderId, instrument: u32, reason_code: i32) -> PyResult<()> {
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

    /// Push historical data into SharedState, with the start and end of
    /// the request for historicalDataEnd (ibx#431).
    #[doc(hidden)]
    #[pyo3(signature = (req_id, bars, is_complete, start="".to_string(), end="".to_string()))]
    fn _test_push_historical_data(
        &self, req_id: ReqId, bars: Vec<(String, f64, f64, f64, f64, i64)>, is_complete: bool,
        start: String, end: String,
    ) -> PyResult<()> {
        let shared = self.shared_state()?;
        let bar_list: Vec<HistoricalBar> = bars.into_iter().map(|(time, o, h, l, c, v)| {
            HistoricalBar { time, open: o, high: h, low: l, close: c, volume: v, wap: 0.0, count: 0 }
        }).collect();
        shared.reference.push_historical_data(req_id, HistoricalResponse {
            query_id: String::new(), timezone: String::new(), bars: bar_list, is_complete, start, end,
        });
        Ok(())
    }

    /// Push a keepUpToDate bar into SharedState (ibx#429).
    #[doc(hidden)]
    #[allow(clippy::too_many_arguments)]
    fn _test_push_historical_update(
        &self, req_id: ReqId, time: String, open: f64, high: f64, low: f64, close: f64, volume: i64, wap: f64, count: i32,
    ) -> PyResult<()> {
        self.shared_state()?.reference.push_historical_update(req_id, HistoricalBar {
            time, open, high, low, close, volume, wap, count,
        });
        Ok(())
    }

    /// Push one frame of trade ticks into SharedState (ibx#432): (time,
    /// past limit, unreported, price, size, exchange, conditions).
    #[doc(hidden)]
    #[allow(clippy::type_complexity)]
    fn _test_push_historical_ticks_last(
        &self, req_id: ReqId, ticks: Vec<(i64, bool, bool, f64, f64, String, String)>, done: bool,
    ) -> PyResult<()> {
        let ticks = ticks.into_iter().map(|(time, past_limit, unreported, price, size, exchange, special_conditions)| {
            crate::types::HistoricalTickLast {
                time, tick_attrib_last: crate::api::types::TickAttribLast { past_limit, unreported },
                price, size, exchange, special_conditions,
            }
        }).collect();
        self.shared_state()?.reference.push_historical_ticks(req_id, crate::types::HistoricalTickData::Last(ticks), "AllLast".into(), done);
        Ok(())
    }

    /// Push one frame of bid/ask ticks into SharedState (ibx#432): (time,
    /// bid past low, ask past high, bid, ask, bid size, ask size).
    #[doc(hidden)]
    #[allow(clippy::type_complexity)]
    fn _test_push_historical_ticks_bid_ask(
        &self, req_id: ReqId, ticks: Vec<(i64, bool, bool, f64, f64, f64, f64)>, done: bool,
    ) -> PyResult<()> {
        let ticks = ticks.into_iter().map(|(time, bid_past_low, ask_past_high, price_bid, price_ask, size_bid, size_ask)| {
            crate::types::HistoricalTickBidAsk {
                time, tick_attrib_bid_ask: crate::api::types::TickAttribBidAsk { bid_past_low, ask_past_high },
                price_bid, price_ask, size_bid, size_ask,
            }
        }).collect();
        self.shared_state()?.reference.push_historical_ticks(req_id, crate::types::HistoricalTickData::BidAsk(ticks), "BidAsk".into(), done);
        Ok(())
    }

    /// Push one frame of midpoint ticks into SharedState (ibx#432).
    #[doc(hidden)]
    fn _test_push_historical_ticks_midpoint(&self, req_id: ReqId, ticks: Vec<(i64, f64)>, done: bool) -> PyResult<()> {
        let ticks = ticks.into_iter().map(|(time, price)| crate::types::HistoricalTickMidpoint { time, price, size: 0.0 }).collect();
        self.shared_state()?.reference.push_historical_ticks(req_id, crate::types::HistoricalTickData::Midpoint(ticks), "MidPoint".into(), done);
        Ok(())
    }

    /// Push a head timestamp into SharedState.
    #[doc(hidden)]
    fn _test_push_head_timestamp(&self, req_id: ReqId, timestamp: &str) -> PyResult<()> {
        let shared = self.shared_state()?;
        shared.reference.push_head_timestamp(req_id, HeadTimestampResponse {
            head_timestamp: timestamp.to_string(), timezone: String::new(),
        });
        Ok(())
    }

    /// Push a contract details row and its end into SharedState (ibx#438).
    #[doc(hidden)]
    #[pyo3(signature = (req_id, con_id, sec_type, continuous=false))]
    fn _test_push_contract_row(&self, req_id: ReqId, con_id: i64, sec_type: &str, continuous: bool) -> PyResult<()> {
        let shared = self.shared_state()?;
        shared.reference.push_contract_details(req_id, crate::control::contracts::ContractDefinition {
            con_id,
            sec_type: crate::control::contracts::SecurityType::from_fix(sec_type),
            continuous,
            ..Default::default()
        });
        shared.reference.push_contract_details_end(req_id);
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

    /// Set an account row as the server sends it (ibx#475): the store the
    /// account updates, the P&L keys and the portfolio read.
    #[doc(hidden)]
    #[pyo3(signature = (key, value, currency=""))]
    fn _test_set_account_row(&self, key: &str, value: &str, currency: &str) -> PyResult<()> {
        let shared = self.shared_state()?;
        shared.portfolio.update_account_rows(|rows| {
            rows.set(key, currency, value);
            rows.image_complete = true;
        });
        Ok(())
    }

    /// The account download is complete, so position requests answer
    /// (ibx#477).
    #[doc(hidden)]
    fn _test_account_download_complete(&self) -> PyResult<()> {
        self.shared_state()?.portfolio.set_account_download_complete();
        Ok(())
    }

    /// Push an account summary batch as the server sends it, for the
    /// subscription id `SR.Socket.{n}` (ibx#479).
    #[doc(hidden)]
    #[pyo3(signature = (sr_id, rows, end=true))]
    fn _test_push_account_summary(&self, sr_id: &str, rows: Vec<(String, String, String)>, end: bool) -> PyResult<()> {
        let shared = self.shared_state()?;
        shared.portfolio.push_account_summary_event(crate::bridge::AccountSummaryEvent {
            sr_id: sr_id.to_string(),
            rows: rows.into_iter().map(|(key, value, currency)| crate::bridge::AccountRow { key, value, currency, ledger: false }).collect(),
            ledger: false,
            end,
        });
        Ok(())
    }

    /// Push a position into SharedState.
    #[doc(hidden)]
    fn _test_set_position(&self, con_id: i64, position: i64, avg_cost: f64) -> PyResult<()> {
        let shared = self.shared_state()?;
        let ps = PRICE_SCALE as f64;
        shared.portfolio.set_position_info(PositionInfo {
            con_id, position_fixed: position * QTY_SCALE, avg_cost: (avg_cost * ps) as i64, ..Default::default()
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

    /// Act as the engine after `delay_ms` (test-only): a background thread
    /// takes the queued commands and answers each registration with
    /// instrument 0, until no command comes for 500 ms.
    #[doc(hidden)]
    fn _test_serve_commands_after(&self, delay_ms: u64) -> PyResult<()> {
        let rx = self._test_control_rx.lock().unwrap().clone()
            .ok_or_else(|| PyRuntimeError::new_err("No test command channel"))?;
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(delay_ms));
            while let Ok(cmd) = rx.recv_timeout(std::time::Duration::from_millis(500)) {
                match cmd {
                    ControlCommand::RegisterInstrument { reply_tx: Some(r), .. }
                    | ControlCommand::Subscribe { reply_tx: Some(r), .. }
                    | ControlCommand::SubscribeTbt { reply_tx: Some(r), .. } => { let _ = r.send(Ok(0)); }
                    _ => {}
                }
            }
        });
        Ok(())
    }

    /// Queue a link status notice, as the engine does (test-only).
    #[doc(hidden)]
    fn _test_push_connection_notice(&self, code: i64, message: &str) -> PyResult<()> {
        self.shared_state()?.push_connection_notice(code, message.to_string());
        Ok(())
    }

    /// Apply the values of a first logon reply, as the connect does
    /// (ibx#421, test-only).
    #[doc(hidden)]
    #[pyo3(signature = (clock_offset_ms, features, max_backfill_years, version_cutoff=None, version_cutoff_date=None))]
    fn _test_apply_logon(
        &self,
        clock_offset_ms: Option<i64>,
        features: &str,
        max_backfill_years: i32,
        version_cutoff: Option<&str>,
        version_cutoff_date: Option<&str>,
    ) -> PyResult<()> {
        let logon = crate::gateway::LogonValues { clock_offset_ms, features: Some(features.to_string()), data_permissions: None };
        crate::gateway::apply_first_logon(&logon, version_cutoff, version_cutoff_date, max_backfill_years, &*self.shared_state()?);
        Ok(())
    }
}
