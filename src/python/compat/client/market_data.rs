//! Market data request/cancel methods.

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

use crate::types::*;
use super::EClient;
use super::super::contract::Contract;

#[pymethods]
impl EClient {
    /// Set news provider codes for per-contract news ticks (e.g. "BRFG*BRFUPDN").
    #[pyo3(signature = (providers))]
    fn set_news_providers(&self, providers: &str) {
        self.core.set_news_providers(providers);
    }

    /// Request market data for a contract.
    #[pyo3(signature = (req_id, contract, generic_tick_list="", snapshot=false, regulatory_snapshot=false, mkt_data_options=Vec::new()))]
    fn req_mkt_data(
        &self,
        py: Python<'_>,
        req_id: i64,
        contract: &Contract,
        generic_tick_list: &str,
        snapshot: bool,
        regulatory_snapshot: bool,
        mkt_data_options: Vec<Py<PyAny>>,
    ) -> PyResult<()> {
        let tx = self.tx()?;
        let shared = self.shared_state()?;

        // The engine can take up to REGISTRATION_TIMEOUT to reply; release
        // the GIL for the round trip so a slow reply stalls this call, not
        // every Python thread (ibx#271). Own the contract fields first —
        // `contract` itself must not cross the detach boundary.
        let con_id = contract.con_id;
        let symbol = contract.symbol.clone();
        let exchange = contract.exchange.clone();
        let sec_type = contract.sec_type.clone();
        let last_trade_date = contract.last_trade_date_or_contract_month.clone();
        let strike = contract.strike;
        let right = contract.right.clone();
        let multiplier = contract.multiplier.clone();
        let generic_tick_list = generic_tick_list.to_string();
        py.detach(|| self.core.register_mkt_data(
            &shared, &tx, req_id,
            con_id, &symbol, &exchange, &sec_type,
            &last_trade_date, strike, &right, &multiplier,
            snapshot, &generic_tick_list, 0,
        )).map_err(|e| PyRuntimeError::new_err(e))?;
        self.core.cache_contract(contract.con_id, crate::api::types::Contract {
            con_id: contract.con_id,
            symbol: contract.symbol.clone(),
            sec_type: contract.sec_type.clone(),
            exchange: contract.exchange.clone(),
            currency: contract.currency.clone(),
            last_trade_date_or_contract_month: contract.last_trade_date_or_contract_month.clone(),
            strike: contract.strike,
            right: contract.right.clone(),
            multiplier: contract.multiplier.clone(),
            ..Default::default()
        });

        let _ = (regulatory_snapshot, mkt_data_options);

        Ok(())
    }

    /// Cancel market data.
    pub fn cancel_mkt_data(&self, py: Python<'_>, req_id: i64) -> PyResult<()> {
        let (instrument, needs_news_unsub) = self.core.unregister_mkt_data(req_id);
        if let Some(instrument) = instrument {
            let tx = self.tx()?;
            Self::send_control(py, &tx, ControlCommand::Unsubscribe { instrument })?;
            if needs_news_unsub {
                let _ = Self::send_control(py, &tx, ControlCommand::UnsubscribeNews { instrument });
            }
        }
        Ok(())
    }

    /// Request tick-by-tick data.
    #[pyo3(signature = (req_id, contract, tick_type, number_of_ticks=0, ignore_size=false))]
    fn req_tick_by_tick_data(
        &self,
        py: Python<'_>,
        req_id: i64,
        contract: &Contract,
        tick_type: &str,
        number_of_ticks: i32,
        ignore_size: bool,
    ) -> PyResult<()> {
        let tx = self.tx()?;

        let tbt_type = match tick_type {
            "Last" | "AllLast" => TbtType::Last,
            "BidAsk" => TbtType::BidAsk,
            _ => return Err(PyRuntimeError::new_err(format!("Unknown tick type: '{}'", tick_type))),
        };

        let shared = self.shared_state()?;
        Self::send_control(py, &tx, ControlCommand::RegisterInstrument {
            con_id: contract.con_id,
            symbol: contract.symbol.clone(),
            sec_type: contract.sec_type.clone(),
            exchange: contract.exchange.clone(),
            reply_tx: None,
        })?;
        // Same registration-wait hazard as req_mkt_data: release the GIL for
        // the reply round trip (ibx#271).
        let con_id = contract.con_id;
        let symbol = contract.symbol.clone();
        py.detach(|| self.core.register_tbt(&shared, &tx, req_id, con_id, &symbol, tbt_type))
            .map_err(|e| PyRuntimeError::new_err(e))?;

        let _ = (number_of_ticks, ignore_size);
        Ok(())
    }

    /// Cancel tick-by-tick data.
    fn cancel_tick_by_tick_data(&self, py: Python<'_>, req_id: i64) -> PyResult<()> {
        if let Some(instrument) = self.core.req_to_instrument.lock().unwrap().remove(&req_id) {
            self.core.instrument_to_req.lock().unwrap().remove(&instrument);
            self.core.forget_instrument(instrument);
            let tx = self.tx()?;
            Self::send_control(py, &tx, ControlCommand::UnsubscribeTbt { instrument })?;
        }
        Ok(())
    }

    /// Request an auth-connection round-trip time sample (ibx#158): sends a
    /// lightweight liveness probe with no side effects on subscriptions,
    /// contract caches, or pacing budgets. Poll `last_rtt_ms()` after a
    /// moment for the result.
    fn req_ping(&self, py: Python<'_>) -> PyResult<()> {
        let tx = self.tx()?;
        Self::send_control(py, &tx, ControlCommand::Ping)?;
        Ok(())
    }

    /// Last measured auth-connection round-trip time in milliseconds, or
    /// None if never measured (ibx#158). A gauge, not a benchmark — see
    /// `req_ping`. Also sampled automatically by the engine's own liveness
    /// probes.
    fn last_rtt_ms(&self) -> PyResult<Option<f64>> {
        let shared = match self.shared.lock().unwrap().clone() {
            Some(s) => s,
            None => return Ok(None),
        };
        Ok(shared.last_ccp_rtt().map(|d| d.as_secs_f64() * 1_000.0))
    }

    /// NOT supported end to end (ibx#234): the requested type (1=live,
    /// 2=frozen, 3=delayed, 4=delayed-frozen) is stored locally but never
    /// sent to the gateway, so subscriptions always deliver realtime data
    /// and delayed tick variants never arrive. Requesting a non-realtime
    /// type logs a warning, and the `market_data_type` callback reports the
    /// DELIVERED type (realtime) rather than echoing the request.
    fn req_market_data_type(&self, market_data_type: i32) -> PyResult<()> {
        self.core.set_market_data_type(market_data_type);
        Ok(())
    }

    /// Request market depth (L2 order book).
    #[pyo3(signature = (req_id, contract, num_rows=5, is_smart_depth=false, mkt_depth_options=Vec::new()))]
    fn req_mkt_depth(
        &self,
        py: Python<'_>,
        req_id: i64,
        contract: &Contract,
        num_rows: i32,
        is_smart_depth: bool,
        mkt_depth_options: Vec<Py<PyAny>>,
    ) -> PyResult<()> {
        let _ = mkt_depth_options;
        let exchange = if contract.exchange.is_empty() { "SMART".to_string() } else { contract.exchange.clone() };
        let sec_type = if contract.sec_type.is_empty() { "STK".to_string() } else { contract.sec_type.clone() };
        let tx = self.tx()?;
        Self::send_control(py, &tx, ControlCommand::SubscribeDepth {
            req_id: req_id as u32,
            con_id: contract.con_id,
            exchange,
            sec_type,
            num_rows,
            is_smart_depth,
        })?;
        Ok(())
    }

    /// Cancel market depth.
    #[pyo3(signature = (req_id, is_smart_depth=false))]
    fn cancel_mkt_depth(&self, py: Python<'_>, req_id: i64, is_smart_depth: bool) -> PyResult<()> {
        let _ = is_smart_depth;
        let tx = self.tx()?;
        Self::send_control(py, &tx, ControlCommand::UnsubscribeDepth { req_id: req_id as u32 })?;
        Ok(())
    }

    /// Request real-time 5-second bars.
    #[pyo3(signature = (req_id, contract, bar_size=5, what_to_show="TRADES", use_rth=0, real_time_bars_options=Vec::new()))]
    fn req_real_time_bars(
        &self,
        py: Python<'_>,
        req_id: i64,
        contract: &Contract,
        bar_size: i32,
        what_to_show: &str,
        use_rth: i32,
        real_time_bars_options: Vec<Py<PyAny>>,
    ) -> PyResult<()> {
        let tx = self.tx()?;
        let _ = (bar_size, real_time_bars_options);
        Self::send_control(py, &tx, ControlCommand::SubscribeRealTimeBar {
            req_id: req_id as u32,
            con_id: contract.con_id,
            symbol: contract.symbol.clone(),
            what_to_show: what_to_show.to_string(),
            use_rth: use_rth != 0,
        })?;
        Ok(())
    }

    /// Cancel real-time bars.
    fn cancel_real_time_bars(&self, py: Python<'_>, req_id: i64) -> PyResult<()> {
        let tx = self.tx()?;
        Self::send_control(py, &tx, ControlCommand::CancelRealTimeBar { req_id: req_id as u32 })?;
        Ok(())
    }

    // ── Quote Access ──

    /// Zero-copy SeqLock quote read by req_id.
    /// Returns a dict with bid, ask, last, bid_size, ask_size, last_size, volume,
    /// high, low, open, close, or None if the req_id is not mapped.
    fn quote(&self, req_id: i64) -> PyResult<Option<Py<PyAny>>> {
        let shared = self.shared_state()?;
        let map = self.core.req_to_instrument.lock().unwrap();
        let iid = match map.get(&req_id) {
            Some(&iid) => iid,
            None => return Ok(None),
        };
        drop(map);
        let q = shared.market.quote(iid);
        Python::attach(|py| {
            let ps = super::super::super::types::PRICE_SCALE_F;
            let qs = crate::types::QTY_SCALE as f64;
            let dict = pyo3::types::PyDict::new(py);
            dict.set_item("bid", q.bid as f64 / ps)?;
            dict.set_item("ask", q.ask as f64 / ps)?;
            dict.set_item("last", q.last as f64 / ps)?;
            dict.set_item("bid_size", q.bid_size as f64 / qs)?;
            dict.set_item("ask_size", q.ask_size as f64 / qs)?;
            dict.set_item("last_size", q.last_size as f64 / qs)?;
            dict.set_item("volume", q.volume as f64 / qs)?;
            dict.set_item("high", q.high as f64 / ps)?;
            dict.set_item("low", q.low as f64 / ps)?;
            dict.set_item("open", q.open as f64 / ps)?;
            dict.set_item("close", q.close as f64 / ps)?;
            Ok(Some(dict.into_any().unbind()))
        })
    }

    /// Zero-copy SeqLock quote read by InstrumentId.
    /// Returns a dict with bid, ask, last, bid_size, ask_size, last_size, volume,
    /// high, low, open, close, or None if not connected.
    fn quote_by_instrument(&self, instrument: u32) -> PyResult<Option<Py<PyAny>>> {
        let shared = match self.shared.lock().unwrap().clone() {
            Some(s) => s,
            None => return Ok(None),
        };
        // Out-of-range id: None, not a cross-language panic (ibx#234).
        let Some(q) = shared.market.try_quote(instrument) else {
            return Ok(None);
        };
        Python::attach(|py| {
            let ps = super::super::super::types::PRICE_SCALE_F;
            let qs = crate::types::QTY_SCALE as f64;
            let dict = pyo3::types::PyDict::new(py);
            dict.set_item("bid", q.bid as f64 / ps)?;
            dict.set_item("ask", q.ask as f64 / ps)?;
            dict.set_item("last", q.last as f64 / ps)?;
            dict.set_item("bid_size", q.bid_size as f64 / qs)?;
            dict.set_item("ask_size", q.ask_size as f64 / qs)?;
            dict.set_item("last_size", q.last_size as f64 / qs)?;
            dict.set_item("volume", q.volume as f64 / qs)?;
            dict.set_item("high", q.high as f64 / ps)?;
            dict.set_item("low", q.low as f64 / ps)?;
            dict.set_item("open", q.open as f64 / ps)?;
            dict.set_item("close", q.close as f64 / ps)?;
            Ok(Some(dict.into_any().unbind()))
        })
    }
}
