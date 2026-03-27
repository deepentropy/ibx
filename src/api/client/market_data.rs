//! Market data request/cancel methods and quote accessors.

use crate::types::*;

use super::{Contract, EClient};

impl EClient {
    // ── Market Data ──

    /// Subscribe to market data. Matches `reqMktData` in C++.
    /// When `snapshot` is true, delivers the first available quote then calls
    /// `tick_snapshot_end` and auto-cancels the subscription.
    pub fn req_mkt_data(
        &self, req_id: i64, contract: &Contract,
        generic_tick_list: &str, snapshot: bool, _regulatory_snapshot: bool,
    ) -> Result<(), String> {
        self.core.register_mkt_data(
            &self.shared, &self.control_tx, req_id,
            contract.con_id, &contract.symbol, &contract.exchange, &contract.sec_type,
            snapshot, generic_tick_list,
        )?;
        Ok(())
    }

    /// Cancel market data. Matches `cancelMktData` in C++.
    pub fn cancel_mkt_data(&self, req_id: i64) -> Result<(), String> {
        let (instrument, needs_news_unsub) = self.core.unregister_mkt_data(req_id);
        if let Some(instrument) = instrument {
            self.send(ControlCommand::Unsubscribe { instrument })?;
            if needs_news_unsub {
                let _ = self.send(ControlCommand::UnsubscribeNews { instrument });
            }
        }
        Ok(())
    }

    /// Subscribe to tick-by-tick data. Matches `reqTickByTickData` in C++.
    pub fn req_tick_by_tick_data(
        &self, req_id: i64, contract: &Contract, tick_type: &str,
        _number_of_ticks: i32, _ignore_size: bool,
    ) -> Result<(), String> {
        let tbt_type = match tick_type {
            "BidAsk" => TbtType::BidAsk,
            _ => TbtType::Last,
        };
        self.core.register_tbt(
            &self.shared, &self.control_tx, req_id,
            contract.con_id, &contract.symbol, tbt_type,
        )?;
        Ok(())
    }

    /// Cancel tick-by-tick data. Matches `cancelTickByTickData` in C++.
    pub fn cancel_tick_by_tick_data(&self, req_id: i64) -> Result<(), String> {
        if let Some(instrument) = self.core.req_to_instrument.lock().unwrap().remove(&req_id) {
            self.core.instrument_to_req.lock().unwrap().remove(&instrument);
            self.send(ControlCommand::UnsubscribeTbt { instrument })?;
        }
        Ok(())
    }

    // ── Market Depth ──

    /// Subscribe to market depth (L2 order book). Matches `reqMktDepth` in C++.
    pub fn req_mkt_depth(
        &self, req_id: i64, contract: &Contract,
        num_rows: i32, is_smart_depth: bool,
    ) -> Result<(), String> {
        let exchange = if contract.exchange.is_empty() { "SMART".to_string() } else { contract.exchange.clone() };
        let sec_type = if contract.sec_type.is_empty() { "STK".to_string() } else { contract.sec_type.clone() };
        self.send(ControlCommand::SubscribeDepth {
            req_id: req_id as u32,
            con_id: contract.con_id,
            exchange,
            sec_type,
            num_rows,
            is_smart_depth,
        })
    }

    /// Cancel market depth. Matches `cancelMktDepth` in C++.
    pub fn cancel_mkt_depth(&self, req_id: i64) -> Result<(), String> {
        self.send(ControlCommand::UnsubscribeDepth { req_id: req_id as u32 })
    }

    // ── Real-Time Bars ──

    pub fn req_real_time_bars(
        &self, req_id: i64, contract: &Contract,
        _bar_size: i32, what_to_show: &str, use_rth: bool,
    ) -> Result<(), String> {
        self.send(ControlCommand::SubscribeRealTimeBar {
            req_id: req_id as u32,
            con_id: contract.con_id,
            symbol: contract.symbol.clone(),
            what_to_show: what_to_show.into(),
            use_rth,
        })
    }

    pub fn cancel_real_time_bars(&self, req_id: i64) -> Result<(), String> {
        self.send(ControlCommand::CancelRealTimeBar { req_id: req_id as u32 })
    }

    /// Set market data type preference (1=live, 2=frozen, 3=delayed, 4=delayed-frozen).
    pub fn req_market_data_type(&self, market_data_type: i32) {
        self.core.set_market_data_type(market_data_type);
    }

    /// Set news provider codes for per-contract news ticks.
    pub fn set_news_providers(&self, providers: &str) {
        self.core.set_news_providers(providers);
    }

    // ── Escape Hatch ──

    /// Zero-copy SeqLock quote read. Maps reqId → InstrumentId → SeqLock.
    /// Returns `None` if the reqId is not mapped to a subscription.
    #[inline]
    pub fn quote(&self, req_id: i64) -> Option<Quote> {
        let map = self.core.req_to_instrument.lock().unwrap();
        map.get(&req_id).map(|&iid| self.shared.market.quote(iid))
    }

    /// Direct SeqLock read by InstrumentId (for callers who track IDs themselves).
    #[inline]
    pub fn quote_by_instrument(&self, instrument: InstrumentId) -> Quote {
        self.shared.market.quote(instrument)
    }
}
