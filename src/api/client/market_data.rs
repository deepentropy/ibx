//! Market data request/cancel methods and quote accessors.

use crate::types::*;

use super::{Contract, EClient};

impl EClient {
    // ── Market Data ──

    /// Subscribe to market data. Matches `reqMktData` in C++.
    /// When `snapshot` is true, delivers the first available quote then calls
    /// `tick_snapshot_end` and auto-cancels the subscription.
    ///
    /// `generic_tick_list` is NOT transmitted to the gateway, with one
    /// exception: "292" additionally subscribes per-contract news. Other
    /// generic tick types (RTVolume and friends) have no emission path, and
    /// `tick_generic` never fires (ibx#234). Delayed data cannot be
    /// requested either — see `req_market_data_type`.
    pub fn req_mkt_data(
        &self, req_id: i64, contract: &Contract,
        generic_tick_list: &str, snapshot: bool, regulatory_snapshot: bool,
    ) -> Result<(), String> {
        self.req_mkt_data_ex(req_id, contract, generic_tick_list, snapshot, regulatory_snapshot, 0)
    }

    /// Like [`req_mkt_data`], but sends a market-data mode with the request,
    /// allowing parallel realtime + frozen subscriptions for the same
    /// contract. The request is always the bid/ask and last pair; the mode
    /// rides on each of its entries:
    ///
    /// | `mode_9887` | mode             |
    /// |-------------|------------------|
    /// | `0`         | REALTIME (no mode sent) |
    /// | `1`         | DELAYED          |
    /// | `2`         | FROZEN           |
    /// | `3`         | DELAYED_FROZEN   |
    ///
    /// The frozen sub keeps thinly-traded names streaming after-hours when the
    /// realtime feed is silent. Issue 3-4 parallel calls per contract with
    /// different modes and pick whichever feed has data.
    pub fn req_mkt_data_ex(
        &self, req_id: i64, contract: &Contract,
        generic_tick_list: &str, snapshot: bool, _regulatory_snapshot: bool,
        mode_9887: i32,
    ) -> Result<(), String> {
        if let Some((code, text)) = self.core.duplicate_ticker_refusal(req_id) {
            self.shared.orders.push_order_error(req_id as u64, code, text);
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
        };
        self.core.register_mkt_data(
            &self.shared, &self.control_tx, req_id,
            contract.con_id, &contract.symbol, &contract.exchange, &contract.sec_type,
            &contract.currency, &filters, snapshot, generic_tick_list, mode_9887,
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
        } else {
            // An unknown request id: error 300, as the reference (ibx#444).
            self.shared.orders.push_order_error(req_id as u64, 300, format!("Can't find EId with tickerId:{}", req_id));
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
            self.core.forget_instrument(instrument);
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

    /// Subscribe to real-time 5-second bars. Matches `reqRealTimeBars` in C++.
    pub fn req_real_time_bars(
        &self, req_id: i64, contract: &Contract,
        _bar_size: i32, what_to_show: &str, use_rth: bool,
    ) -> Result<(), String> {
        self.send(ControlCommand::SubscribeRealTimeBar {
            req_id: req_id as u32,
            con_id: contract.con_id,
            symbol: contract.symbol.clone(),
            sec_type: contract.sec_type.clone(),
            exchange: contract.exchange.clone(),
            what_to_show: what_to_show.into(),
            use_rth,
        })
    }

    /// Cancel real-time bars. Matches `cancelRealTimeBars` in C++.
    pub fn cancel_real_time_bars(&self, req_id: i64) -> Result<(), String> {
        self.send(ControlCommand::CancelRealTimeBar { req_id: req_id as u32 })
    }

    /// Set market data type preference (1=live, 2=frozen, 3=delayed, 4=delayed-frozen).
    /// Request an auth-connection round-trip time sample (ibx#158): sends a
    /// lightweight liveness probe with no side effects on subscriptions,
    /// contract caches, or pacing budgets. The result lands asynchronously —
    /// poll `last_rtt()` after a moment. No-op while a probe is already in
    /// flight or the connection is down.
    pub fn req_ping(&self) -> Result<(), String> {
        self.send(ControlCommand::Ping)
    }

    /// Last measured auth-connection round-trip time, if any (ibx#158).
    /// A gauge, not a benchmark: the sample is the interval from a probe to
    /// the first inbound traffic that followed it, which on an active feed
    /// can undercount by racing data already in flight. Also sampled
    /// automatically whenever liveness sends its own probe.
    pub fn last_rtt(&self) -> Option<std::time::Duration> {
        self.shared.last_ccp_rtt()
    }

    /// Set the market data type (ibx#447), as the reference sets its modes:
    /// 1 all off, 2 frozen on, 3 delayed on, 4 delayed and delayed-frozen
    /// on (2 keeps the delayed modes, 3 keeps frozen). With delayed on, a
    /// subscription the server rejects goes on with delayed data when the
    /// server has it: `market_data_type(reqId, 3)` then error 10167, as the
    /// reference. The frozen modes are kept but no frozen subscription is
    /// sent: when the reference asks for frozen data is not known. A value
    /// outside 1..=4 gives error 321 with id -1.
    pub fn req_market_data_type(&self, market_data_type: i32) {
        if let Some((code, text)) = self.core.set_market_data_type(&self.control_tx, market_data_type) {
            self.shared.orders.push_order_error(-1i64 as u64, code, text);
        }
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
    /// Returns None for an out-of-range id — this used to panic (ibx#234).
    #[inline]
    pub fn quote_by_instrument(&self, instrument: InstrumentId) -> Option<Quote> {
        self.shared.market.try_quote(instrument)
    }
}
