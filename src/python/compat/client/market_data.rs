//! Market data request/cancel methods.

use pyo3::prelude::*;
use crate::error_codes::{NO_SUCH_SUBSCRIPTION, Refusal};

use crate::types::*;
use super::{wire_req_id, EClient};
use super::super::contract::Contract;

#[pymethods]
impl EClient {
    /// Set news provider codes for per-contract news ticks (e.g. "BRFG*BRFUPDN").
    #[pyo3(signature = (providers))]
    fn set_news_providers(&self, providers: &str) {
        self.core.set_news_providers(providers);
    }

    /// Request market data for a contract.
    ///
    /// `mkt_data_options` is taken and not applied. This protocol's request
    /// carries no free-form option list, so what a caller puts in one cannot be
    /// sent. The reference client's own list is empty on every ordinary call.
    #[pyo3(signature = (req_id, contract, generic_tick_list="", snapshot=false, regulatory_snapshot=false, mkt_data_options=Vec::new()))]
    pub(crate) fn req_mkt_data(
        &self,
        py: Python<'_>,
        req_id: i64,
        contract: &Contract,
        generic_tick_list: &str,
        snapshot: bool,
        regulatory_snapshot: bool,
        mkt_data_options: Vec<Py<PyAny>>,
    ) -> PyResult<()> {
        let _ = mkt_data_options;
        // The mode set by `req_market_data_type`, which names the type once for
        // every subscription that follows. Passing zero here subscribes at
        // realtime regardless, which answers nothing on an account without the
        // realtime entitlement. `req_mkt_data_ex` states the mode per
        // request.
        let mode = self.core.subscription_mode();
        self.req_mkt_data_ex(py, req_id, contract, generic_tick_list, snapshot, regulatory_snapshot, mode)
    }

    /// Like `req_mkt_data`, but names the market-data mode on the request
    /// itself (0=realtime, 1=delayed, 2=frozen, 3=delayed-frozen) rather than
    /// taking the one the session is set to. The frozen one keeps thinly-traded
    /// names quoting after hours, when the realtime feed is silent.
    ///
    /// A contract holds one subscription at a time, so this states the mode for
    /// that subscription rather than adding a second alongside it: a later
    /// request for a contract already subscribed follows the one that is up and
    /// is handed its quotes. To compare two modes on one contract, withdraw
    /// between them.
    ///
    /// `regulatory_snapshot` asks for the venue's own chargeable one-shot
    /// snapshot: a request type of its own rather than a mode on an ordinary
    /// quote. It needs the entitlement, and an
    /// account without it is refused by the venue, which names the request
    /// type back through `error`. It ends the way an ordinary snapshot does,
    /// so `tickSnapshotEnd` fires either way.
    #[pyo3(signature = (req_id, contract, generic_tick_list="", snapshot=false, regulatory_snapshot=false, mode_9887=0))]
    fn req_mkt_data_ex(
        &self,
        py: Python<'_>,
        req_id: i64,
        contract: &Contract,
        generic_tick_list: &str,
        snapshot: bool,
        regulatory_snapshot: bool,
        mode_9887: i32,
    ) -> PyResult<()> {
        let Some(tx) = self.tx_or_report(req_id)? else { return Ok(()) };

        // A contract's news is asked for by the venue's id for the contract,
        // and the caller may have stated a description instead. Resolved only
        // when news is what was asked for: a quote on a description is asked
        // for by description and the venue names it itself.
        // The whole entry, not a number ending in it: 1292 is not 292. Matching on
        // the ending qualifies the contract, which is a request to the venue and a
        // wait on the caller's thread, while the core subscribes to no news.
        let wants_news = generic_tick_list.split(',').any(|t| t.trim() == "292");
        let named;
        let by_venue;
        let contract = if wants_news && contract.con_id == 0 && !contract.symbol.is_empty() {
            match self.qualify_contract_stated(py, contract) {
                Ok(found) => { named = found; &named }
                // Reported under the code for the cause. A session that ends
                // mid-lookup is not code 200, which names a contract the venue
                // does not hold and invites a retry.
                Err(why) => return self.report_refusal(py, req_id, why),
            }
        } else {
            // Named by the venue where the caller named it by id alone, as the
            // request surface names it. Sent as it stands, the engine took the
            // subscription, found no security type for it afterwards and gave
            // it up, so the caller was told nothing and read no quotes.
            let Some(found) = self.named_or_report(py, req_id, contract)? else { return Ok(()) };
            by_venue = found;
            &*by_venue
        };

        let shared = self.shared_state()?;

        // The engine can take up to REGISTRATION_TIMEOUT to reply; release
        // the GIL for the round trip so a slow reply stalls this call, not
        // every Python thread. Own the contract fields first —
        // `contract` itself must not cross the detach boundary.
        let con_id = contract.con_id;
        let symbol = contract.symbol.clone();
        let exchange = contract.exchange.clone();
        let sec_type = contract.sec_type.clone();
        let currency = contract.currency.clone();
        // What tells two listings of one symbol apart, taken whole. A caller
        // who names an option's class, its local name or where it is listed
        // states it here the way every other request that names a contract by
        // description does; dropped, the lookup behind the subscription asks a
        // wider question than the caller put and is answered with several
        // contracts, which names none.
        let filters = contract.lookup_filters();
        let generic_tick_list = generic_tick_list.to_string();
        if let Err(why) = py.detach(|| self.core.register_mkt_data(
            &shared, &tx, req_id,
            con_id, &symbol, &exchange, &sec_type, &currency, &filters,
            snapshot, regulatory_snapshot, &generic_tick_list, mode_9887,
        )) {
            return self.report_refusal(py, req_id, why);
        }
        self.core.cache_contract(contract.con_id, crate::types::model::Contract {
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

        Ok(())
    }

    /// Cancel market data.
    pub fn cancel_mkt_data(&self, py: Python<'_>, req_id: i64) -> PyResult<()> {
        let Some(tx) = self.tx_or_report(req_id)? else { return Ok(()) };
        // A number still taking its subscription on another thread has no
        // record here yet. Refused for that, the caller was told its
        // withdrawal had not happened and was left holding the subscription
        // anyway — the venue withdraws one that arrives early rather than
        // refusing it. Recorded against the registration, which reads it
        // before it publishes what it opened and takes it back down instead.
        // This surface registers with the interpreter lock released, which
        // makes a cancel from a timer thread an ordinary thing to write.
        //
        // Asked before the record below, not after: a registration finishing
        // between the two is seen by one of them either way, where asked the
        // other way round it could fall between both and be refused for a
        // subscription that is up.
        if self.core.withdraw_while_registering(req_id) {
            return Ok(());
        }
        // A caller withdrawing a subscription this client does not hold
        // branches on being told so. Said nothing, the withdrawal reads
        // exactly like one that worked. Reported here rather than in the body
        // below, which this client also calls for withdrawals nobody asked
        // for -- a snapshot that has ended, the watch behind an option
        // calculation -- and those must stay silent.
        if !self.core.holds_mkt_data(req_id) {
            return self.report_refusal(py, req_id, Refusal::stated(
                NO_SUCH_SUBSCRIPTION,
                format!("no contract is being watched under request {req_id}"),
            ));
        }
        self.withdraw_mkt_data(py, &tx, req_id)
    }

    /// Request tick-by-tick data.
    ///
    /// `number_of_ticks` and `ignore_size` are sent as stated: a count of past
    /// ticks goes out as the length of the run the stream opens with, and the
    /// size filter as the query's filter term. Neither goes out at its default
    /// — no prelude, sizes included — which is what the venue does on its own.
    /// Whether the venue honours the size filter is the venue's: one session
    /// saw size-only changes still arrive on a stream that asked to leave
    /// them out.
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
        // The number this request will answer under, checked before anything
        // reaches the venue. Unchecked, it was narrowed further down instead,
        // so a caller numbering its requests from the order counter — which the
        // venue lets run past what a request id can hold — had this stream's
        // refusals reported against somebody else's request.
        wire_req_id(req_id)?;
        let Some(tx) = self.tx_or_report(req_id)? else { return Ok(()) };

        let tbt_type = match TbtType::named(tick_type) {
            Ok(named) => named,
            // A tick type this client does not carry is a request it will not
            // send, which is what validation means.
            Err(why) => return self.report_refusal(py, req_id, Refusal::validation(why)),
        };

        // Named by the venue where the caller named it by id alone, as the
        // request surface names it.
        let Some(by_venue) = self.named_or_report(py, req_id, contract)? else { return Ok(()) };
        let contract = &*by_venue;

        // A stream is asked for by venue contract id. Sent
        // with none, the venue answers "Unknown contract" against a query this
        // client had not told anyone about, and the caller waited on a stream
        // that was refused before it began.
        let named;
        let contract = if contract.con_id == 0 && !contract.symbol.is_empty() {
            match self.qualify_contract_stated(py, contract) {
                Ok(found) => { named = found; &named }
                // Reported under the code for the cause. A session that ends
                // mid-lookup is not code 200, which names a contract the venue
                // does not hold and invites a retry.
                Err(why) => return self.report_refusal(py, req_id, why),
            }
        } else {
            contract
        };

        let shared = self.shared_state()?;
        if let Err(why) = Self::send_control(py, &tx, ControlCommand::RegisterInstrument {
                contract: ContractRef { con_id: contract.con_id, symbol: contract.symbol.clone(), sec_type: contract.sec_type.clone(), exchange: contract.exchange.clone(), ..Default::default() },
                identity: String::new(),
                reply_tx: None,
            }) {
            return self.report_refusal(py, req_id, Refusal::not_connected(why.to_string()));
        }
        // Same registration-wait hazard as req_mkt_data: release the GIL for
        // the reply round trip.
        let con_id = contract.con_id;
        let symbol = contract.symbol.clone();
        let (sec_type, exchange) = (contract.sec_type.clone(), contract.exchange.clone());
        let opened = match py.detach(|| self.core.register_tbt(
            &shared, &tx, req_id, con_id, &symbol, &sec_type, &exchange, tbt_type,
            number_of_ticks.max(0) as u32, ignore_size,
        )) {
            Ok(opened) => opened,
            Err(why) => return self.report_refusal(py, req_id, why),
        };
        // The kind this request asked for, kept so the callback can state it.
        // The record does not carry it, and every print was labelled as an
        // exchange print whichever stream it came from. Kept only where there
        // is still a stream: one withdrawn while the registration was away has
        // already been taken back down, and a kind left behind for it outlives
        // the stream it describes.
        if opened.is_some() && let TbtType::AllLast | TbtType::Last = tbt_type {
            let kind = if matches!(tbt_type, TbtType::AllLast) { 2 } else { 1 };
            self.tbt_kind.lock().unwrap().insert(req_id, kind);
        }

        Ok(())
    }

    /// Cancel tick-by-tick data.
    fn cancel_tick_by_tick_data(&self, py: Python<'_>, req_id: i64) -> PyResult<()> {
        let Some(tx) = self.tx_or_report(-1)? else { return Ok(()) };
        // A number still taking its stream on another thread has no record
        // here yet, and is withdrawn the way the quote subscription above is:
        // recorded against the registration, which takes the stream back down
        // when it reads it. Asked before the record below for the reason given
        // there.
        if self.core.withdraw_while_registering_tbt(req_id) {
            return Ok(());
        }
        // Only what this request took out. Removing the contract's quote
        // mapping here took the quotes away from whoever was watching them.
        // Removed before the send, not across it: the send is bounded and runs
        // detached from Python, so a guard spanning it blocks another thread
        // cancelling a different subscription.
        let instrument = self.core.tbt_to_instrument.lock().unwrap().remove(&req_id);
        // A caller withdrawing a stream this client does not hold branches on
        // being told so. Said nothing, the withdrawal reads exactly like one
        // that worked.
        let Some(instrument) = instrument else {
            return self.report_refusal(py, req_id, Refusal::stated(
                NO_SUCH_SUBSCRIPTION,
                format!("no tick stream is held under request {req_id}"),
            ));
        };
        // A refused withdrawal leaves the stream's callback kind in place.
        self.tbt_kind.lock().unwrap().remove(&req_id);
        if let Err(why) = Self::send_control(py, &tx, ControlCommand::UnsubscribeTbt { req_id, instrument }) {
            return self.report_refusal(py, req_id, Refusal::not_connected(why.to_string()));
        }
        Ok(())
    }

    /// Request an auth-connection round-trip time sample: sends a
    /// lightweight liveness probe with no side effects on subscriptions,
    /// contract caches, or pacing budgets. Poll `last_rtt_ms()` after a
    /// moment for the result.
    fn req_ping(&self, py: Python<'_>) -> PyResult<()> {
        let Some(tx) = self.tx_or_report(-1)? else { return Ok(()) };
        if let Err(why) = Self::send_control(py, &tx, ControlCommand::Ping) {
            return self.report_refusal(py, -1, Refusal::not_connected(why.to_string()));
        }
        Ok(())
    }

    /// Last measured auth-connection round-trip time in milliseconds, or
    /// None if never measured. A gauge, not a benchmark
    /// `req_ping`. Also sampled automatically by the engine's own liveness
    /// probes.
    fn last_rtt_ms(&self) -> PyResult<Option<f64>> {
        let shared = match self.shared.lock().unwrap().clone() {
            Some(s) => s,
            None => return Ok(None),
        };
        Ok(shared.last_ccp_rtt().map(|d| d.as_secs_f64() * 1_000.0))
    }

    /// Name the kind of data every subscription after this one asks for:
    /// 1 live, 2 frozen, 3 delayed, 4 delayed-frozen.
    ///
    /// The type is carried on each subscription that follows, and the
    /// `market_data_type` callback reports the type that subscription was
    /// made under. A type this client does not know is logged and leaves
    /// subscriptions live. `req_mkt_data_ex` states the type per request,
    /// which allows two feeds on one contract at once.
    fn req_market_data_type(&self, market_data_type: i32) -> PyResult<()> {
        // Answered under 504 with no session, as every request is, and the
        // type is then not kept. It used to be: set before `connect`, it
        // applied to the session that followed. The reference client's sends
        // and stores nothing, so a program written against it sets the type
        // after connecting, having never had another way; what a caller loses
        // here is only a setting the reference never let it make.
        let Some(_tx) = self.tx_or_report(-1)? else { return Ok(()) };
        self.core.set_market_data_type(market_data_type);
        Ok(())
    }

    /// Request market depth (L2 order book).
    ///
    /// `mkt_depth_options` is taken and not applied. This protocol's request
    /// carries no free-form option list, so what a caller puts in one cannot be
    /// sent. The reference client's own list is empty on every ordinary call.
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
        // As the caller stated it. The reference client sends a book request's
        // secType and exchange straight off the contract, so a contract naming
        // only an id was subscribed here to a US stock on SMART: a book for an
        // instrument nobody asked about, under their own request id.
        let Some(tx) = self.tx_or_report(req_id)? else { return Ok(()) };
        // The number is checked before the book slot is taken: a number the
        // wire cannot carry holds nothing, and taking the slot first left it
        // held against a request that was then refused.
        let wire = wire_req_id(req_id)?;
        // A book rides the quote feed, so a feed given up on serves none. The
        // other surface refuses this and this one did not: a book asked for
        // here took a slot, reached a sender with no connection to write it to,
        // and said nothing — and a book that never arrives is what a market
        // with nothing to say looks like.
        // Read out from under the lock before anything is refused. The refusal
        // below reaches the caller's own error handler, and a handler that
        // disconnects takes this same lock on its way out — held across the
        // call, the two are one thread waiting on a lock it is already holding,
        // with the interpreter stopped behind it.
        let feed_is_over = self.shared.lock().unwrap().as_ref()
            .and_then(|s| s.market.market_data_over());
        if let Some(why) = feed_is_over {
            return self.report_refusal(
                py, req_id,
                Refusal::not_connected(format!(
                    "market data is unavailable for the rest of this session: {why}",
                )),
            );
        }
        if let Err(why) = self.core.hold_the_book(req_id) {
            return self.report_refusal(py, req_id, why);
        }
        if let Err(why) = Self::send_control(py, &tx, ControlCommand::SubscribeDepth {
                contract: ContractRef { con_id: contract.con_id, symbol: contract.symbol.clone(), exchange: contract.exchange.clone(), sec_type: contract.sec_type.clone(), currency: contract.currency.clone(), ..Default::default() },
                req_id: wire,
                num_rows,
                is_smart_depth,
                filters: contract.lookup_filters(),
            }) {
            // The slot goes back with it. Kept, the number stayed held
            // against a request the venue never heard, and the caller's
            // retry under it was refused as a duplicate of that one until
            // the session was rebuilt.
            let _ = self.core.release_the_book(req_id);
            return self.report_refusal(py, req_id, Refusal::not_connected(why.to_string()));
        }
        Ok(())
    }

    /// Cancel market depth.
    ///
    /// `is_smart_depth` is taken and not applied. A book is withdrawn by the
    /// request that asked for it, and this client remembers which kind that
    /// was, so the caller restating it changes nothing.
    #[pyo3(signature = (req_id, is_smart_depth=false))]
    fn cancel_mkt_depth(&self, py: Python<'_>, req_id: i64, is_smart_depth: bool) -> PyResult<()> {
        let _ = is_smart_depth;
        let Some(tx) = self.tx_or_report(req_id)? else { return Ok(()) };
        // A caller withdrawing a book this client does not hold branches on
        // being told so, under the number the catalogue gives depth rather
        // than the one a quote subscription is withdrawn under.
        let wire = wire_req_id(req_id)?;
        if let Err(why) = self.core.release_the_book(req_id) {
            return self.report_refusal(py, req_id, why);
        }
        if let Err(why) = Self::send_control(py, &tx, ControlCommand::UnsubscribeDepth { req_id: wire }) {
            return self.report_refusal(py, req_id, Refusal::not_connected(why.to_string()));
        }
        Ok(())
    }

    /// Request real-time 5-second bars.
    ///
    /// `bar_size` and `real_time_bars_options` are taken and not applied. The
    /// venue's real-time bar is five seconds and there is no field asking for
    /// another, and this protocol's request carries no free-form option list.
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
        let Some(tx) = self.tx_or_report(req_id)? else { return Ok(()) };
        let _ = (bar_size, real_time_bars_options);
        if let Err(why) = crate::control::historical::BarDataType::from_api_str(what_to_show) {
            return self.report_refusal(py, req_id, why.into());
        }
        let wire = wire_req_id(req_id)?;
        // A historical request that finished under this number left the number
        // marked as one whose bars are updates to it, and only a new or a
        // cancelled historical request cleared that mark. Backfill and then
        // stream on the same number — the ordinary way to write it — and every
        // bar of the stream arrived as `historical_data_update`, so a caller
        // that overrode only `real_time_bar` read the stream as dead.
        self.core.historical_request_is_new(wire);
        if let Err(why) = Self::send_control(py, &tx, ControlCommand::SubscribeRealTimeBar {
                contract: contract.into(),
                req_id: wire,
                what_to_show: what_to_show.to_string(),
                use_rth: use_rth != 0,
                filters: contract.lookup_filters(),
            }) {
            return self.report_refusal(py, req_id, Refusal::not_connected(why.to_string()));
        }
        Ok(())
    }

    /// Cancel real-time bars.
    fn cancel_real_time_bars(&self, py: Python<'_>, req_id: i64) -> PyResult<()> {
        let Some(tx) = self.tx_or_report(req_id)? else { return Ok(()) };
        if let Err(why) = Self::send_control(py, &tx, ControlCommand::CancelRealTimeBar { req_id: wire_req_id(req_id)? }) {
            return self.report_refusal(py, req_id, Refusal::not_connected(why.to_string()));
        }
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
            // What the venue says about dealing in this contract at all, which
            // is what every price above means or does not mean. Left out, a
            // caller reading a quote through this surface read the last price
            // before a halt as a market it could deal on.
            dict.set_item("halted", q.halted)?;
            Ok(Some(dict.into_any().unbind()))
        })
    }

    /// Zero-copy SeqLock quote read by InstrumentId.
    /// Returns a dict with bid, ask, last, bid_size, ask_size, last_size,
    /// volume, high, low, open, close, and whether the venue has halted the
    /// contract and why — or None if not connected. Whether it is restricting
    /// short sales in it is `shortSaleRestrictedByInstrument`, which is stated
    /// on the same record and kept off this one.
    fn quote_by_instrument(&self, instrument: u32) -> PyResult<Option<Py<PyAny>>> {
        let shared = match self.shared.lock().unwrap().clone() {
            Some(s) => s,
            None => return Ok(None),
        };
        // Out-of-range id: None, not a cross-language panic.
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
            // What the venue says about dealing in this contract at all, which
            // is what every price above means or does not mean. Left out, a
            // caller reading a quote through this surface read the last price
            // before a halt as a market it could deal on.
            dict.set_item("halted", q.halted)?;
            Ok(Some(dict.into_any().unbind()))
        })
    }

    /// What the venue's own model last made of an option, whole.
    ///
    /// `tickOptionComputation` carries eight figures, which is what the
    /// documented callback has room for; the venue states eighteen on the same
    /// tick. The ten it has no room for are in this dict beside them. A figure
    /// the venue did not state is this API's own unset double; zero is a real
    /// greek.
    ///
    /// `None` where the request names no subscription, or the venue has not
    /// stated a model for it yet.
    #[pyo3(signature = (req_id))]
    fn option_model(&self, req_id: i64) -> PyResult<Option<Py<PyAny>>> {
        let Ok(shared) = self.shared_state() else { return Ok(None) };
        let Some(instrument) =
            self.core.req_to_instrument.lock().unwrap().get(&req_id).copied()
        else {
            return Ok(None);
        };
        self.option_model_dict(&shared, instrument)
    }

    /// Whether the venue is restricting short sales in the contract a request
    /// is watching.
    ///
    /// The circuit breaker a venue puts on a contract that has fallen far
    /// enough in a day, which stops a short from resting below the bid. Stated
    /// on the same record as the halt, and with no field anywhere in the
    /// documented API. Not the same question as whether the contract can be
    /// borrowed, which ticks 46 and 89 already answer beside it.
    #[pyo3(signature = (req_id))]
    fn short_sale_restricted(&self, req_id: i64) -> PyResult<bool> {
        let Ok(shared) = self.shared_state() else { return Ok(false) };
        let held = self.core.req_to_instrument.lock().unwrap();
        Ok(held.get(&req_id).is_some_and(|&iid| shared.market.short_sale_restricted(iid)))
    }

    /// The same, by InstrumentId, for callers who track them themselves.
    #[pyo3(signature = (instrument))]
    fn short_sale_restricted_by_instrument(&self, instrument: u32) -> PyResult<bool> {
        let Ok(shared) = self.shared_state() else { return Ok(false) };
        Ok(shared.market.short_sale_restricted(instrument))
    }

    /// What the venue says about the contract itself, beside its prices: how
    /// many shares are on issue, and what it opened at a year ago.
    ///
    /// Both arrive on the tick carrying the price extremes and neither has a
    /// tick of its own in the documented API — a share count is a fundamentals
    /// request there, and a year-ago open has no call at all.
    #[pyo3(signature = (req_id))]
    fn contract_figures(&self, req_id: i64) -> PyResult<Option<Py<PyAny>>> {
        let Ok(shared) = self.shared_state() else { return Ok(None) };
        let Some(instrument) = self.core.req_to_instrument.lock().unwrap().get(&req_id).copied()
        else {
            return Ok(None);
        };
        Self::contract_figures_dict(&shared, instrument)
    }

    /// The same, by InstrumentId, for callers who track them themselves.
    #[pyo3(signature = (instrument))]
    fn contract_figures_by_instrument(&self, instrument: u32) -> PyResult<Option<Py<PyAny>>> {
        let Ok(shared) = self.shared_state() else { return Ok(None) };
        Self::contract_figures_dict(&shared, instrument)
    }

    /// What one of the venue's own series last stated for a subscription, in
    /// the order the series states it.
    ///
    /// The venue runs dozens of series the documented API has no call for: a
    /// bond's analytics, an option's model volatility, the volume a contract
    /// usually opens on, what margin a future takes. Ask for one by its own
    /// number in the generic tick list, and read what it stated here. A figure
    /// the venue holds nothing for comes as the largest number its field
    /// carries.
    #[pyo3(signature = (req_id, series))]
    fn stated_figures(&self, req_id: i64, series: u32) -> PyResult<Vec<f64>> {
        let Ok(shared) = self.shared_state() else { return Ok(Vec::new()) };
        let Some(instrument) = self.core.req_to_instrument.lock().unwrap().get(&req_id).copied()
        else {
            return Ok(Vec::new());
        };
        Ok(shared.market.stated_figures(instrument, series))
    }

    /// What one of the venue's numbered-table series last stated for a
    /// subscription: its figures under the venue's own numbering.
    ///
    /// Four series state their figures as two tables, whole numbers and
    /// fractional ones, each entry naming what it is before stating it.
    /// `fractional` picks the table; the venue numbers the two separately, so
    /// the same number in each is not the same figure.
    #[pyo3(signature = (req_id, series, fractional=false))]
    fn numbered_figures(
        &self, req_id: i64, series: u32, fractional: bool,
    ) -> PyResult<Vec<(i32, f64)>> {
        let Ok(shared) = self.shared_state() else { return Ok(Vec::new()) };
        let Some(instrument) = self.core.req_to_instrument.lock().unwrap().get(&req_id).copied()
        else {
            return Ok(Vec::new());
        };
        Ok(shared.market.numbered_figures(instrument, series, fractional))
    }

    /// The run of paired figures one series last stated for a subscription.
    ///
    /// Two series state their figures as a count and then that many pairs: the
    /// volatility the venue's own model puts on each point of a curve, and the
    /// weight it puts on each price a contract might reach.
    #[pyo3(signature = (req_id, series))]
    fn paired_figures(&self, req_id: i64, series: u32) -> PyResult<Vec<(f64, f64)>> {
        let Ok(shared) = self.shared_state() else { return Ok(Vec::new()) };
        let Some(instrument) = self.core.req_to_instrument.lock().unwrap().get(&req_id).copied()
        else {
            return Ok(Vec::new());
        };
        Ok(shared.market.paired_figures(instrument, series))
    }

    /// The strategies a spread scan stated for a request, as the venue stated
    /// them.
    ///
    /// Each is a dict of its legs, which shape of strategy it is and how
    /// pressing, the thirteen figures the venue states about it, where it comes
    /// out even, and one figure behind those. Empty until a scan has been asked
    /// for and answered. The documented API has no call for this.
    #[pyo3(signature = (req_id))]
    fn scanned_strategies(&self, req_id: i64) -> PyResult<Vec<Py<PyAny>>> {
        let Ok(shared) = self.shared_state() else { return Ok(Vec::new()) };
        let Some(instrument) = self.core.req_to_instrument.lock().unwrap().get(&req_id).copied()
        else {
            return Ok(Vec::new());
        };
        let found = shared.market.scanned_strategies(instrument);
        Python::attach(|py| {
            let mut out = Vec::with_capacity(found.len());
            for s in found {
                let dict = pyo3::types::PyDict::new(py);
                dict.set_item("legs", s.legs.clone())?;
                dict.set_item("kind", s.kind)?;
                dict.set_item("aggression", s.aggression)?;
                dict.set_item("figures", s.figures.clone())?;
                dict.set_item("breakEvens", s.break_evens.clone())?;
                dict.set_item("lastFigure", s.last_figure)?;
                out.push(dict.into_any().unbind());
            }
            Ok(out)
        })
    }

    /// The rows of three figures one series last stated for a subscription.
    ///
    /// What the three are is the series' own:
    ///
    /// | Series | The three figures |
    /// | --- | --- |
    /// | 547 | A quantity, what it is offered at, and a second price where the form states one |
    /// | 491 | Which strategy the leg belongs to, the contract it names, and its size |
    /// | 320, 376, 530, 532 | The venue's number for a field of a packed quote, its figure, and how far that figure's decimal point moves |
    ///
    /// On the packed quotes the venue numbers its fields itself: nought and one
    /// are the two sides of the quote and four and five the size behind each,
    /// read off a live session. A side the venue is not standing behind reads
    /// as minus one hundred. Those figures are counted in the contract's own
    /// increments, as every packed figure is — 75815 against an increment of a
    /// hundredth is 758.15 — and no scale is put on them here, because which
    /// fields are prices and which are counts is the venue's to change.
    ///
    /// `f64::MAX` stands where a form states no third figure. The documented
    /// API has no call for any of these.
    #[pyo3(signature = (req_id, series))]
    fn stated_rows(&self, req_id: i64, series: u32) -> PyResult<Vec<(f64, f64, f64)>> {
        let Ok(shared) = self.shared_state() else { return Ok(Vec::new()) };
        let Some(instrument) = self.core.req_to_instrument.lock().unwrap().get(&req_id).copied()
        else {
            return Ok(Vec::new());
        };
        Ok(shared.market.stated_rows(instrument, series))
    }

    /// Which series have stated paired figures for a subscription, in order.
    #[pyo3(signature = (req_id))]
    fn paired_figures_series(&self, req_id: i64) -> PyResult<Vec<u32>> {
        let Ok(shared) = self.shared_state() else { return Ok(Vec::new()) };
        let Some(instrument) = self.core.req_to_instrument.lock().unwrap().get(&req_id).copied()
        else {
            return Ok(Vec::new());
        };
        Ok(shared.market.paired_figures_series(instrument))
    }

    /// Which series have stated numbered figures for a subscription, in order.
    #[pyo3(signature = (req_id))]
    fn numbered_figures_series(&self, req_id: i64) -> PyResult<Vec<u32>> {
        let Ok(shared) = self.shared_state() else { return Ok(Vec::new()) };
        let Some(instrument) = self.core.req_to_instrument.lock().unwrap().get(&req_id).copied()
        else {
            return Ok(Vec::new());
        };
        Ok(shared.market.numbered_figures_series(instrument))
    }

    /// Which series have stated figures for a subscription, in order.
    #[pyo3(signature = (req_id))]
    fn stated_figures_series(&self, req_id: i64) -> PyResult<Vec<u32>> {
        let Ok(shared) = self.shared_state() else { return Ok(Vec::new()) };
        let Some(instrument) = self.core.req_to_instrument.lock().unwrap().get(&req_id).copied()
        else {
            return Ok(Vec::new());
        };
        Ok(shared.market.stated_figures_series(instrument))
    }

    /// What the venue's model made of an option as it closed.
    ///
    /// The same model as `option_model` and in the same shape — every greek it
    /// states, the ones the documented API has no field for included — but
    /// worked out as the contract closed rather than as it stands. The
    /// documented API has no call for it at all. Ask for it by the venue's own
    /// number for the series in the generic tick list.
    #[pyo3(signature = (req_id))]
    fn closing_option_model(&self, req_id: i64) -> PyResult<Option<Py<PyAny>>> {
        let Ok(shared) = self.shared_state() else { return Ok(None) };
        let Some(instrument) = self.core.req_to_instrument.lock().unwrap().get(&req_id).copied()
        else {
            return Ok(None);
        };
        self.closing_option_model_dict(&shared, instrument)
    }

    /// The same, by InstrumentId, for callers who track them themselves.
    #[pyo3(signature = (instrument))]
    fn closing_option_model_by_instrument(&self, instrument: u32) -> PyResult<Option<Py<PyAny>>> {
        let Ok(shared) = self.shared_state() else { return Ok(None) };
        self.closing_option_model_dict(&shared, instrument)
    }

    /// The same, by InstrumentId, for callers who track them themselves.
    #[pyo3(signature = (instrument))]
    fn option_model_by_instrument(&self, instrument: u32) -> PyResult<Option<Py<PyAny>>> {
        let Ok(shared) = self.shared_state() else { return Ok(None) };
        self.option_model_dict(&shared, instrument)
    }
}

impl EClient {
    /// One statement of what the venue says about a contract, so the two
    /// readers above cannot publish different halves of it.
    fn contract_figures_dict(
        shared: &std::sync::Arc<crate::bridge::SharedState>,
        instrument: crate::types::InstrumentId,
    ) -> PyResult<Option<Py<PyAny>>> {
        let Some(f) = shared.market.contract_figures(instrument) else { return Ok(None) };
        Python::attach(|py| {
            let dict = pyo3::types::PyDict::new(py);
            dict.set_item("sharesOutstanding", f.shares_outstanding)?;
            dict.set_item("openAYearAgo", f.open_a_year_ago)?;
            Ok(Some(dict.into_any().unbind()))
        })
    }

    /// One statement of the record, so the two readers above cannot publish
    /// different halves of it.
    fn option_model_dict(
        &self,
        shared: &std::sync::Arc<crate::bridge::SharedState>,
        instrument: crate::types::InstrumentId,
    ) -> PyResult<Option<Py<PyAny>>> {
        let Some(m) = shared.market.option_model(instrument) else { return Ok(None) };
        Self::model_dict(m)
    }

    /// The same, for the model the venue worked out as the contract closed.
    fn closing_option_model_dict(
        &self,
        shared: &std::sync::Arc<crate::bridge::SharedState>,
        instrument: crate::types::InstrumentId,
    ) -> PyResult<Option<Py<PyAny>>> {
        let Some(m) = shared.market.closing_option_model(instrument) else { return Ok(None) };
        Self::model_dict(m)
    }

    /// One statement of a model's fields, so no two readers can publish
    /// different halves of the same record.
    fn model_dict(m: crate::types::OptionComputation) -> PyResult<Option<Py<PyAny>>> {
        Python::attach(|py| {
            let dict = pyo3::types::PyDict::new(py);
            for (name, value) in [
                ("impliedVol", m.implied_vol),
                ("delta", m.delta),
                ("optPrice", m.opt_price),
                ("pvDividend", m.pv_dividend),
                ("gamma", m.gamma),
                ("vega", m.vega),
                ("theta", m.theta),
                ("undPrice", m.und_price),
                ("rho", m.rho),
                ("fugit", m.fugit),
                ("exerciseBoundary", m.exercise_boundary),
                ("forwardCoeff", m.forward_coeff),
                ("modelYield", m.model_yield),
                ("bridgeYield", m.bridge_yield),
                ("timeValue", m.time_value),
                ("calDays", m.cal_days),
                ("rate", m.rate),
            ] {
                dict.set_item(name, value)?;
            }
            dict.set_item("priceBasedVol", m.price_based_vol)?;
            Ok(Some(dict.into_any().unbind()))
        })
    }
}

impl EClient {
    /// Withdraw a request's subscription, saying nothing about it.
    ///
    /// The body of `cancel_mkt_data`, for the withdrawals this client makes on
    /// its own: a snapshot that has ended, the watch opened behind an option
    /// calculation. Nobody asked for those, so nothing is reported against the
    /// request — made through the public cancel, a handler that disconnected
    /// on `tick_snapshot_end` was told 504 about a snapshot that had just
    /// completed.
    pub(crate) fn withdraw_mkt_data(
        &self,
        py: Python<'_>,
        tx: &std::sync::mpsc::SyncSender<ControlCommand>,
        req_id: i64,
    ) -> PyResult<()> {
        let shared = self.shared_state()?;
        let withdrawn = self.core.unregister_mkt_data(&shared, req_id);
        // Asked separately, because the quotes stay up for another caller
        // while the headlines this one asked for stop. Withdrawn only
        // alongside the quotes, they carried on with nobody listening.
        if let Some(subject) = withdrawn.headlines {
            let _ = Self::send_control(py, tx, ControlCommand::UnsubscribeNews { subject });
        }
        // And the series this caller brought to a subscription that stays up
        // for somebody else. Left behind, the venue serves them for the life
        // of that subscription with nobody reading them. They ride with the
        // withdrawal of the whole subscription too, for the case where that
        // subscription has already been replaced by one this caller knows
        // nothing about.
        let series = withdrawn.series;
        if let Some(instrument) = withdrawn.subscription {
            if let Err(why) = Self::send_control(
                py, tx,
                ControlCommand::Unsubscribe {
                    instrument,
                    con_id: withdrawn.con_id,
                    took_it: withdrawn.took_it,
                    series: series.map(|(_, ticks)| ticks).unwrap_or_default(),
                    issued: withdrawn.decided_at,
                },
            ) {
                return self.report_refusal(py, req_id, Refusal::not_connected(why.to_string()));
            }
        } else if let Some((instrument, generic_ticks)) = series
            && let Err(why) = Self::send_control(
                py, tx,
                ControlCommand::StopAskingForSeries {
                    instrument,
                    con_id: withdrawn.con_id,
                    took_it: withdrawn.took_it,
                    generic_ticks,
                    issued: withdrawn.decided_at,
                },
            )
        {
            return self.report_refusal(py, req_id, Refusal::not_connected(why.to_string()));
        }
        Ok(())
    }
}
