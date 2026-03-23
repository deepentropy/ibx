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
        _generic_tick_list: &str, snapshot: bool, _regulatory_snapshot: bool,
    ) {
        let _ = self.core.register_mkt_data(
            &self.shared, &self.control_tx, req_id,
            contract.con_id, &contract.symbol, &contract.exchange, &contract.sec_type,
            snapshot,
        );
    }

    /// Cancel market data. Matches `cancelMktData` in C++.
    pub fn cancel_mkt_data(&self, req_id: i64) {
        if let Some(instrument) = self.core.unregister_mkt_data(req_id) {
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
        let _ = self.core.register_tbt(
            &self.shared, &self.control_tx, req_id,
            contract.con_id, &contract.symbol, tbt_type,
        );
    }

    /// Cancel tick-by-tick data. Matches `cancelTickByTickData` in C++.
    pub fn cancel_tick_by_tick_data(&self, req_id: i64) {
        if let Some(instrument) = self.core.req_to_instrument.lock().unwrap().remove(&req_id) {
            self.core.instrument_to_req.lock().unwrap().remove(&instrument);
            let _ = self.control_tx.send(ControlCommand::UnsubscribeTbt { instrument });
        }
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
