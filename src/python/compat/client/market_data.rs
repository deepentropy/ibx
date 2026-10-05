//! Market data request/cancel methods.

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

use crate::types::*;
use super::{send_cmd, EClient};
use super::super::contract::Contract;

#[pymethods]
impl EClient {
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
        if let Some(r) = self.not_connected(req_id) { return r; }
        if !crate::client_core::ClientCore::ids_fit("req_mkt_data", &[req_id, contract.con_id]) { return Ok(()); }
        let tx = self.tx()?;
        let shared = self.shared_state()?;
        // A contract with no exchange: 321, the reference's first check.
        // An invalid generic tick list of a request that is no snapshot:
        // 321 (ibx#450).
        if let Some((code, text)) = crate::client_core::ClientCore::market_data_exchange_refusal(&contract.exchange, &contract.sec_type)
            .or_else(|| self.core.generic_tick_list_refusal(generic_tick_list, snapshot, &contract.sec_type))
        {
            shared.orders.push_order_error(req_id, code, text);
            return Ok(());
        }
        // The snapshot checks come before the duplicate check, as the
        // reference's (ibx#446).
        if snapshot
            && let Some((code, text)) = self.core.snapshot_refusal(&shared, generic_tick_list, &contract.sec_type)
        {
            shared.orders.push_order_error(req_id, code, text);
            return Ok(());
        }
        if let Some((code, text)) = self.core.duplicate_ticker_refusal(req_id) {
            shared.orders.push_order_error(req_id, code, text);
            return Ok(());
        }
        // A regulatory snapshot goes through its own fetcher (ibx#446).
        if regulatory_snapshot {
            let _ = mkt_data_options;
            return py.detach(|| self.core.start_regulatory_snapshot(
                &shared, &tx, req_id, contract.con_id, &contract.symbol, &contract.exchange, &contract.sec_type,
            )).map_err(PyRuntimeError::new_err);
        }

        // The news tick of a known contract is checked first (ibx#458).
        if let Some((code, text)) = self.core.news_tick_refusal(&shared, generic_tick_list, contract.con_id, &contract.sec_type) {
            shared.orders.push_order_error(req_id, code, text);
            return Ok(());
        }
        let filters = SecDefFilters {
            primary_exchange: contract.primary_exchange.clone(),
            local_symbol: contract.local_symbol.clone(),
            last_trade_date_or_contract_month: contract.last_trade_date_or_contract_month.clone(),
            strike: contract.strike,
            right: contract.right.clone(),
            multiplier: contract.multiplier.clone(),
            trading_class: contract.trading_class.clone(),
            sec_id: contract.sec_id.clone(),
            sec_id_type: contract.sec_id_type.clone(),
            include_expired: contract.include_expired,
            issuer_id: String::new(),
        };
        // The registration waits for the engine: interpreter lock released
        // (ibx#271).
        py.detach(|| self.core.register_mkt_data(
            &shared, &tx, req_id,
            contract.con_id, &contract.symbol, &contract.exchange, &contract.sec_type,
            &contract.currency, &filters, snapshot, generic_tick_list, 0,
        )).map_err(|e| PyRuntimeError::new_err(e))?;
        // A contract without a conId has no identity to cache (ibx#278).
        if contract.con_id != 0 {
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
        }

        let _ = mkt_data_options;

        Ok(())
    }

    /// Cancel market data.
    pub fn cancel_mkt_data(&self, py: Python<'_>, req_id: i64) -> PyResult<()> {
        if let Some(r) = self.not_connected(req_id) { return r; }
        if !crate::client_core::ClientCore::ids_fit("cancel_mkt_data", &[req_id]) { return Ok(()); }
        // A running regulatory snapshot stops silently (ibx#446).
        if let Ok(tx) = self.tx() {
            if self.core.cancel_regulatory_snapshot(req_id, &tx) {
                return Ok(());
            }
        }
        let shared = self.shared_state()?;
        if let Some(cancel) = self.core.unregister_mkt_data(&shared, req_id) {
            // The subscription ends with the last request of the contract;
            // its news entries go with it (ibx#458, ibx#444).
            for command in cancel.commands() {
                let tx = self.tx()?;
                send_cmd(py, &tx, command)?;
            }
        } else {
            // An unknown request id: error 300, as the reference (ibx#444).
            shared.orders.push_order_error(req_id, 300, format!("Can't find EId with tickerId:{}", req_id));
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
        if let Some(r) = self.not_connected(req_id) { return r; }
        if !crate::client_core::ClientCore::ids_fit("req_tick_by_tick_data", &[req_id, contract.con_id]) { return Ok(()); }
        let tx = self.tx()?;

        // The reference's checks: 321 for a bad type or a combo, 10189 when
        // the logon turns it off, 10190 past the contract limit (ibx#455).
        let shared = self.shared_state()?;
        let local_symbol = if contract.local_symbol.is_empty() { &contract.symbol } else { &contract.local_symbol };
        let tbt_type = match self.core.tbt_refusal(&shared, &contract.sec_type, tick_type, local_symbol) {
            Ok(t) => t,
            Err((code, text)) => {
                shared.orders.push_order_error(req_id, code, text);
                return Ok(());
            }
        };
        send_cmd(py, &tx, ControlCommand::RegisterInstrument {
            con_id: contract.con_id,
            symbol: contract.symbol.clone(),
            sec_type: contract.sec_type.clone(),
            exchange: contract.exchange.clone(),
            reply_tx: None,
        })?;
        // The registration waits for the engine: interpreter lock released
        // (ibx#271).
        py.detach(|| self.core.register_tbt(
            &shared, &tx, req_id,
            contract.con_id, &contract.symbol, &contract.exchange, &contract.sec_type,
            tbt_type, number_of_ticks, ignore_size,
        )).map_err(|e| PyRuntimeError::new_err(e))?;
        Ok(())
    }

    /// Cancel tick-by-tick data.
    fn cancel_tick_by_tick_data(&self, py: Python<'_>, req_id: i64) -> PyResult<()> {
        if let Some(r) = self.not_connected(-1) { return r; }
        if !crate::client_core::ClientCore::ids_fit("cancel_tick_by_tick_data", &[req_id]) { return Ok(()); }
        if self.core.unregister_tbt(req_id).is_some() {
            let tx = self.tx()?;
            send_cmd(py, &tx, ControlCommand::UnsubscribeTbt { req_id })?;
        }
        Ok(())
    }

    /// Request an auth-connection round-trip time sample (ibx#158): sends a
    /// lightweight liveness probe with no side effects on subscriptions,
    /// contract caches, or pacing budgets. Poll `last_rtt_ms()` after a
    /// moment for the result.
    fn req_ping(&self, py: Python<'_>) -> PyResult<()> {
        let tx = self.tx()?;
        send_cmd(py, &tx, ControlCommand::Ping)?;
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

    /// Set the market data type (ibx#447): 1=live, 2=frozen, 3=delayed,
    /// 4=delayed-frozen, as the Rust client. With delayed on, a
    /// subscription the server rejects goes on with delayed data (type 3,
    /// error 10167); the frozen modes are kept but send no frozen
    /// subscription. A value outside 1..=4 gives error 321 with id -1.
    fn req_market_data_type(&self, py: Python<'_>, market_data_type: i32) -> PyResult<()> {
        if let Some(r) = self.not_connected(-1) { return r; }
        let tx = self.tx()?;
        if let Some((code, text)) = py.detach(|| self.core.set_market_data_type(&tx, market_data_type)) {
            self.shared_state()?.orders.push_order_error(-1, code, text);
        }
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
        if let Some(r) = self.not_connected(req_id) { return r; }
        if !crate::client_core::ClientCore::ids_fit("req_mkt_depth", &[req_id, contract.con_id]) { return Ok(()); }
        let _ = mkt_depth_options;
        // An empty exchange is refused by the engine, as the reference
        // refuses it (#452).
        let exchange = contract.exchange.clone();
        let sec_type = if contract.sec_type.is_empty() { "STK".to_string() } else { contract.sec_type.clone() };
        let tx = self.tx()?;
        send_cmd(py, &tx, ControlCommand::SubscribeDepth {
            req_id,
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
        if let Some(r) = self.not_connected(-1) { return r; }
        if !crate::client_core::ClientCore::ids_fit("cancel_mkt_depth", &[req_id]) { return Ok(()); }
        let _ = is_smart_depth;
        let tx = self.tx()?;
        send_cmd(py, &tx, ControlCommand::UnsubscribeDepth { req_id })?;
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
        if let Some(r) = self.not_connected(req_id) { return r; }
        if !crate::client_core::ClientCore::ids_fit("req_real_time_bars", &[req_id, contract.con_id]) { return Ok(()); }
        let tx = self.tx()?;
        let _ = (bar_size, real_time_bars_options);
        send_cmd(py, &tx, ControlCommand::SubscribeRealTimeBar {
            req_id,
            con_id: contract.con_id,
            symbol: contract.symbol.clone(),
            sec_type: contract.sec_type.clone(),
            exchange: contract.exchange.clone(),
            what_to_show: what_to_show.to_string(),
            use_rth: use_rth != 0,
        })?;
        Ok(())
    }

    /// Cancel real-time bars.
    fn cancel_real_time_bars(&self, py: Python<'_>, req_id: i64) -> PyResult<()> {
        if let Some(r) = self.not_connected(req_id) { return r; }
        if !crate::client_core::ClientCore::ids_fit("cancel_real_time_bars", &[req_id]) { return Ok(()); }
        let tx = self.tx()?;
        send_cmd(py, &tx, ControlCommand::CancelRealTimeBar { req_id })?;
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
