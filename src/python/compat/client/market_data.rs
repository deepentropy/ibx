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
        *self.news_providers.lock().unwrap() = providers.to_string();
    }

    /// Request market data for a contract.
    #[pyo3(signature = (req_id, contract, generic_tick_list="", snapshot=false, regulatory_snapshot=false, mkt_data_options=Vec::new()))]
    fn req_mkt_data(
        &self,
        req_id: i64,
        contract: &Contract,
        generic_tick_list: &str,
        snapshot: bool,
        regulatory_snapshot: bool,
        mkt_data_options: Vec<PyObject>,
    ) -> PyResult<()> {
        let tx = self.tx()?;
        let shared = self.shared_state()?;

        // If generic_tick_list contains 292 (news), also subscribe via CCP
        let wants_news = generic_tick_list.split(',')
            .any(|t| t.trim() == "292" || t.trim() == "mdoff,292" || t.trim().ends_with("292"));
        if wants_news {
            let providers = self.news_providers.lock().unwrap().clone();
            tx.send(ControlCommand::SubscribeNews {
                con_id: contract.con_id,
                symbol: contract.symbol.clone(),
                providers,
                reply_tx: None,
            }).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        }

        self.core.register_mkt_data(
            &shared, &tx, req_id,
            contract.con_id, &contract.symbol, &contract.exchange, &contract.sec_type,
            snapshot,
        ).map_err(|e| PyRuntimeError::new_err(e))?;
        self.contract_cache.lock().unwrap().insert(contract.con_id, contract.clone());

        let _ = (regulatory_snapshot, mkt_data_options);

        Ok(())
    }

    /// Cancel market data.
    fn cancel_mkt_data(&self, req_id: i64) -> PyResult<()> {
        if let Some(instrument) = self.core.unregister_mkt_data(req_id) {
            self.mdt_sent.lock().unwrap().remove(&req_id);
            let tx = self.tx()?;
            tx.send(ControlCommand::Unsubscribe { instrument })
                .map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
            let _ = tx.send(ControlCommand::UnsubscribeNews { instrument });
        }
        Ok(())
    }

    /// Request tick-by-tick data.
    #[pyo3(signature = (req_id, contract, tick_type, number_of_ticks=0, ignore_size=false))]
    fn req_tick_by_tick_data(
        &self,
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
        tx.send(ControlCommand::RegisterInstrument { con_id: contract.con_id, reply_tx: None })
            .map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        self.core.register_tbt(
            &shared, &tx, req_id,
            contract.con_id, &contract.symbol, tbt_type,
        ).map_err(|e| PyRuntimeError::new_err(e))?;

        let _ = (number_of_ticks, ignore_size);
        Ok(())
    }

    /// Cancel tick-by-tick data.
    fn cancel_tick_by_tick_data(&self, req_id: i64) -> PyResult<()> {
        if let Some(instrument) = self.core.req_to_instrument.lock().unwrap().remove(&req_id) {
            self.core.instrument_to_req.lock().unwrap().remove(&instrument);
            let tx = self.tx()?;
            tx.send(ControlCommand::UnsubscribeTbt { instrument })
                .map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        }
        Ok(())
    }

    /// Set market data type (1=live, 2=frozen, 3=delayed, 4=delayed-frozen).
    fn req_market_data_type(&self, market_data_type: i32) -> PyResult<()> {
        self.market_data_type.store(market_data_type, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    /// Request market depth (L2 order book).
    #[pyo3(signature = (req_id, contract, num_rows=5, is_smart_depth=false, mkt_depth_options=Vec::new()))]
    fn req_mkt_depth(
        &self,
        req_id: i64,
        contract: &Contract,
        num_rows: i32,
        is_smart_depth: bool,
        mkt_depth_options: Vec<PyObject>,
    ) -> PyResult<()> {
        let _ = mkt_depth_options;
        let exchange = if contract.exchange.is_empty() { "SMART".to_string() } else { contract.exchange.clone() };
        let sec_type = if contract.sec_type.is_empty() { "STK".to_string() } else { contract.sec_type.clone() };
        let tx = self.tx()?;
        tx.send(ControlCommand::SubscribeDepth {
            req_id: req_id as u32,
            con_id: contract.con_id,
            exchange,
            sec_type,
            num_rows,
            is_smart_depth,
        }).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(())
    }

    /// Cancel market depth.
    #[pyo3(signature = (req_id, is_smart_depth=false))]
    fn cancel_mkt_depth(&self, req_id: i64, is_smart_depth: bool) -> PyResult<()> {
        let _ = is_smart_depth;
        let tx = self.tx()?;
        tx.send(ControlCommand::UnsubscribeDepth { req_id: req_id as u32 })
            .map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(())
    }

    /// Request real-time 5-second bars.
    #[pyo3(signature = (req_id, contract, bar_size=5, what_to_show="TRADES", use_rth=0, real_time_bars_options=Vec::new()))]
    fn req_real_time_bars(
        &self,
        req_id: i64,
        contract: &Contract,
        bar_size: i32,
        what_to_show: &str,
        use_rth: i32,
        real_time_bars_options: Vec<PyObject>,
    ) -> PyResult<()> {
        let tx = self.tx()?;
        let _ = (bar_size, real_time_bars_options);
        tx.send(ControlCommand::SubscribeRealTimeBar {
            req_id: req_id as u32,
            con_id: contract.con_id,
            symbol: contract.symbol.clone(),
            what_to_show: what_to_show.to_string(),
            use_rth: use_rth != 0,
        }).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(())
    }

    /// Cancel real-time bars.
    fn cancel_real_time_bars(&self, req_id: i64) -> PyResult<()> {
        let tx = self.tx()?;
        tx.send(ControlCommand::CancelRealTimeBar { req_id: req_id as u32 })
            .map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(())
    }
}
