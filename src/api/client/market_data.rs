//! Market data request/cancel methods and quote accessors.

use crate::types::*;
use crate::error_codes::{NO_SUCH_SUBSCRIPTION, Refusal};

use super::{wire_req_id, Contract, EClient};

impl EClient {
    // ── Market Data ──

    /// Subscribe to market data. Matches `reqMktData` in C++.
    /// When `snapshot` is true, delivers the first available quote then calls
    /// `tick_snapshot_end` and auto-cancels the subscription. That is a
    /// subscription this client ends, not a request of its own: the venue's
    /// own one-shot snapshot is the chargeable one, asked for with
    /// `regulatory_snapshot` on [`req_mkt_data_ex`](EClient::req_mkt_data_ex).
    ///
    /// `generic_tick_list` goes out with the subscription, and what comes back
    /// reaches the caller on the callback the series belongs to. These are
    /// read:
    ///
    /// * `100`, `101`, `105` — option volume, open interest and the average
    ///   volume, calls before puts.
    /// * `104`, `106`, `411` — volatility: the historical figure, the implied
    ///   one, and the one the venue restrikes through the session.
    /// * `162`, `165` — an index's premium over its future; the extremes of
    ///   the last quarter, half-year and year with the ordinary day's volume.
    /// * `220`, `221`, `232`, `619` — the mark the venue keeps, which is not a
    ///   trade, under each of the numbers it is asked for by, and the slow one
    ///   beside it.
    /// * `225` — the auction: what is crossing, which way, at what price, and
    ///   the imbalance the venue must publish.
    /// * `233`, `375` — everything that traded, and what traded on a trade
    ///   report, each stated as a trade rather than as the totals it is read
    ///   from.
    /// * `236` — whether it can be borrowed, and how much of it.
    /// * `258` (or `47`) — the company ratios, as the venue writes them.
    /// * `292` — news for the contract.
    /// * `293`, `294`, `295` — how fast it is trading.
    /// * `318` — what last traded in the regular session.
    /// * `456` (or `59`) — what it pays out.
    /// * `460` — the factor a redemption changes.
    /// * `499` — what it costs to borrow.
    /// * `577`, `614`, `623` — a fund's value per share: last, the day's
    ///   extremes, and the frozen one.
    /// * `586` — what a share is expected to open at, and what it did.
    /// * `588` — a future's open interest.
    /// * `595` — what has traded over the last three, five and ten minutes.
    /// * `787` — the odd lot: the two prices nobody has to deal in round lots
    ///   at, their sizes, and where each is quoted.
    ///
    /// A code outside that list still goes to the venue, and a reading of it
    /// arrives and is recorded rather than delivered: the shape it is written
    /// in is the series' own, and nothing here can read one it has not been
    /// taught.
    ///
    /// `tick_generic` also fires for the halt the venue states on its own tick:
    /// tick 49, 0 while a contract is trading and 1 once it has stopped.
    ///
    /// Delayed and frozen data are requested, contrary to what this said: name
    /// the type on [`req_market_data_type`](EClient::req_market_data_type) and
    /// every subscription after it carries the mode, or state it per request
    /// with [`req_mkt_data_ex`](EClient::req_mkt_data_ex). The table there
    /// gives the wire shape of each.
    pub fn req_mkt_data(
        &self, req_id: i64, contract: &Contract,
        generic_tick_list: &str, snapshot: bool, regulatory_snapshot: bool,
    ) -> Result<(), Refusal> {
        // The mode the caller asked for on `req_market_data_type`, which names
        // the type once for every subscription that follows. `req_mkt_data_ex`
        // states it per request instead.
        let mode = self.core.subscription_mode();
        self.req_mkt_data_ex(req_id, contract, generic_tick_list, snapshot, regulatory_snapshot, mode)
    }

    /// Like [`req_mkt_data`](EClient::req_mkt_data), but names the market-data
    /// mode on the request itself, through FIX field 9887, rather than taking
    /// the one the session is set to:
    ///
    /// | `mode_9887` | mode             | wire shape |
    /// |-------------|------------------|---|
    /// | `0`         | REALTIME         | `264=442` (BID_ASK) + `264=443` (LAST), no 9887 |
    /// | `1`         | DELAYED          | `264=1` (TOP) + `9887=1` |
    /// | `2`         | FROZEN           | `264=1` (TOP) + `9887=2` |
    /// | `3`         | DELAYED_FROZEN   | `264=1` (TOP) + `9887=3` |
    ///
    /// The frozen mode keeps thinly-traded names quoting after-hours, when the
    /// realtime feed is silent.
    ///
    /// A contract holds one subscription at a time, so this states
    /// the mode for that subscription rather than adding a parallel one — to
    /// compare modes on one contract, cancel between them. To set the mode for
    /// every subscription instead of naming it per request, call
    /// `req_market_data_type`.
    ///
    /// `regulatory_snapshot` asks for the venue's own chargeable one-shot
    /// snapshot: a request type of its own rather than a mode on an ordinary
    /// quote, asked for under the snapshot action and with no feed named
    /// beside it. It needs the entitlement — an account without it is
    /// refused by the venue, which names the request type back. Whether it
    /// also costs something is between the account and the broker, and is not
    /// on this wire. It ends the way an ordinary snapshot does, so a caller hears
    /// `tick_snapshot_end` either way. Its default is false.
    pub fn req_mkt_data_ex(
        &self, req_id: i64, contract: &Contract,
        generic_tick_list: &str, snapshot: bool, regulatory_snapshot: bool,
        mode_9887: i32,
    ) -> Result<(), Refusal> {
        // A contract's news is asked for by the venue's id for the contract,
        // and the caller may have stated a description instead. Resolved only
        // when news is what was asked for: a quote on a description is asked
        // for by description and the venue names it itself.
        // The whole entry, not a number ending in it: 1292 is not 292. Matching on
        // the ending qualifies the contract, which is a request to the venue and a
        // wait on the caller's thread, while the core subscribes to no news.
        let wants_news = generic_tick_list.split(',').any(|t| t.trim() == "292");
        // Named by the venue where the caller named it by id alone, and where
        // headlines are wanted for a contract named by description.
        let named;
        let contract = if wants_news && contract.con_id == 0 && !contract.symbol.is_empty() {
            named = self.qualify_contract(contract)?;
            &named
        } else {
            &*self.named_by_the_venue(contract)?
        };

        self.core.register_mkt_data(
            &self.shared, &self.control_tx, req_id,
            contract.con_id, &contract.symbol, &contract.exchange, &contract.sec_type,
            &contract.currency, &contract.lookup_filters(),
            snapshot, regulatory_snapshot, generic_tick_list, mode_9887,
        )?;
        Ok(())
    }

    /// Cancel market data. Matches `cancelMktData` in C++.
    pub fn cancel_mkt_data(&self, req_id: i64) -> Result<(), Refusal> {
        // A stream opened by `watch` holds its id as this client's own for as
        // long as it runs. Withdrawing it is where that ends. An id the caller
        // chose was never held, and releasing one that was not held does
        // nothing.
        self.shared.reference.forget_ours(crate::bridge::RecordKind::Answer, req_id);
        // A number still taking its subscription on another thread has no
        // record here yet. Refused for that, the caller was told its
        // withdrawal had not happened and was left holding the subscription
        // anyway — the venue withdraws one that arrives early rather than
        // refusing it. Recorded against the registration, which reads it
        // before it publishes what it opened and takes it back down instead.
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
        // exactly like one that worked.
        if !self.core.holds_mkt_data(req_id) {
            return Err(Refusal::stated(
                NO_SUCH_SUBSCRIPTION,
                format!("no contract is being watched under request {req_id}"),
            ));
        }
        let withdrawn = self.core.unregister_mkt_data(&self.shared, req_id);
        // Asked separately, because the quotes stay up for another caller
        // while the headlines this one asked for stop. Withdrawn only
        // alongside the quotes, they carried on with nobody listening.
        if let Some(subject) = withdrawn.headlines {
            let _ = self.send(ControlCommand::UnsubscribeNews { subject });
        }
        // And the series this caller brought to a subscription that stays up
        // for somebody else. Left behind, the venue serves them for the life
        // of that subscription with nobody reading them. They ride with the
        // withdrawal of the whole subscription too, for the case where that
        // subscription has already been replaced by one this caller knows
        // nothing about.
        let series = withdrawn.series;
        if let Some(instrument) = withdrawn.subscription {
            self.send(ControlCommand::Unsubscribe {
                instrument,
                con_id: withdrawn.con_id,
                took_it: withdrawn.took_it,
                series: series.map(|(_, ticks)| ticks).unwrap_or_default(),
                issued: withdrawn.decided_at,
            })?;
        } else if let Some((instrument, generic_ticks)) = series {
            self.send(ControlCommand::StopAskingForSeries {
                instrument,
                con_id: withdrawn.con_id,
                took_it: withdrawn.took_it,
                generic_ticks,
                issued: withdrawn.decided_at,
            })?;
        }
        Ok(())
    }

    /// Subscribe to every trade or every quote change on a contract.
    ///
    /// The feed rides the historical farm, registered there under the name
    /// `TickByTick` beside the five-second bars. No separate service is
    /// involved. A missing entitlement arrives as the venue's refusal
    /// rather than as silence.
    ///
    /// `number_of_ticks` and `ignore_size` are sent as stated: a count of past
    /// ticks goes out as the length of the run the stream opens with, and the
    /// size filter as the query's filter term. Neither goes out at its default
    /// — no prelude, sizes included — which is what the venue does on its own.
    /// Whether the venue honours the size filter is the venue's: one session
    /// saw size-only changes still arrive on a stream that asked to leave
    /// them out.
    pub fn req_tick_by_tick_data(
        &self, req_id: i64, contract: &Contract, tick_type: &str,
        number_of_ticks: i32, ignore_size: bool,
    ) -> Result<(), Refusal> {
        // The only request surface that did not check the number it was given.
        // Unchecked, it was narrowed further down instead, so a caller
        // numbering its requests from the order counter — which the venue lets
        // run past what a request id can hold — had this stream's refusals
        // reported against somebody else's request.
        let _ = wire_req_id(req_id)?;
        // Named by the venue where the caller named it by id alone.
        let contract = &*self.named_by_the_venue(contract)?;
        let kind = TbtType::named(tick_type)?;

        // A stream is asked for by the venue's id for the contract. Sent
        // with none, the venue answers "Unknown contract" against a query this
        // client had not told anyone about, and the caller waited on a stream
        // that was refused before it began.
        let named;
        let contract = if contract.con_id == 0 && !contract.symbol.is_empty() {
            named = self.qualify_contract(contract)?;
            &named
        } else {
            contract
        };

        let opened = self.core
            .register_tbt(
                &self.shared,
                &self.control_tx,
                req_id,
                contract.con_id,
                &contract.symbol,
                &contract.sec_type,
                &contract.exchange,
                kind,
                number_of_ticks.max(0) as u32,
                ignore_size,
            )?;
        // Which stream the callback names when these trades arrive: 1 = Last,
        // 2 = AllLast. The trade record does not carry it; the subscription does.
        // Named only where there is still a stream: one withdrawn while the
        // registration was away has already been taken back down, and a kind
        // left behind for it outlives the stream it describes.
        if opened.is_some() {
            self.tbt_kinds.lock().unwrap().insert(req_id, kind);
        }
        Ok(())
    }


    /// Cancel tick-by-tick data. Matches `cancelTickByTickData` in C++.
    pub fn cancel_tick_by_tick_data(&self, req_id: i64) -> Result<(), Refusal> {
        // A number still taking its stream on another thread has no record
        // here yet, and is withdrawn the way the quote subscription above is:
        // recorded against the registration, which takes the stream back down
        // when it reads it. The stream is asked for on the historical farm and
        // the record is written when the farm names the contract, so the gap
        // is as long as that takes. Asked before the record below for the
        // reason given there.
        if self.core.withdraw_while_registering_tbt(req_id) {
            return Ok(());
        }
        // Taken out before the send rather than across it. The guard in an
        // `if let` scrutinee lives to the end of the body, and the send is
        // bounded — it waits when the engine's queue is full, which is
        // ordinary backpressure. Held across that wait, this lock stops
        // another thread opening or withdrawing a different stream, and stops
        // a disconnect resetting at all, for as long as the queue stays full.
        let instrument = self.core.tbt_to_instrument.lock().unwrap().remove(&req_id);
        // A caller withdrawing a stream this client does not hold branches on
        // being told so. Said nothing, the withdrawal reads exactly like one
        // that worked.
        let Some(instrument) = instrument else {
            return Err(Refusal::stated(
                NO_SUCH_SUBSCRIPTION,
                format!("no tick stream is held under request {req_id}"),
            ));
        };
        // Only what this request took out, and only once the record says there
        // is a stream to take out. Removing the contract's quote mapping here
        // took the quotes away from whoever was watching them; removing the
        // kind ahead of that record leaves a withdrawal that was refused
        // having changed something, and what it changes is how the stream
        // under that number reports the prints it has left — an AllLast stream
        // whose kind is gone reports each one as an exchange print.
        self.tbt_kinds.lock().unwrap().remove(&req_id);
        self.send(ControlCommand::UnsubscribeTbt { req_id, instrument })?;
        Ok(())
    }

    // ── Market Depth ──

    /// Subscribe to market depth (L2 order book). Matches `reqMktDepth` in C++.
    ///
    /// A contract that names no venue and no security type is sent as it
    /// stands. The engine reads an unnamed venue as the smart destination and
    /// checks a named security type against the venue's routing table.
    /// Substituting a stock here asks for a future's book as a stock's, which
    /// the venue refuses as a book it does not serve.
    pub fn req_mkt_depth(
        &self, req_id: i64, contract: &Contract,
        num_rows: i32, is_smart_depth: bool,
    ) -> Result<(), Refusal> {
        // The number is checked before the book slot is taken: a number the
        // wire cannot carry holds nothing, and taking the slot first left it
        // held against a request that was then refused.
        let wire = wire_req_id(req_id)?;
        // A book rides the quote feed, so a feed the engine has given up on
        // serves none. Accepted, the request took a book slot and reached a
        // sender with no connection to write it to, which is silent — and a
        // book that never arrives is what a market with nothing to say looks
        // like, so nothing distinguished the two.
        if let Some(why) = self.shared.market.market_data_over() {
            return Err(Refusal::not_connected(format!(
                "market data is unavailable for the rest of this session: {why}",
            )));
        }
        self.core.hold_the_book(req_id)?;
        self.send(ControlCommand::SubscribeDepth {
            contract: ContractRef {
                con_id: contract.con_id,
                symbol: contract.symbol.clone(),
                exchange: contract.exchange.clone(),
                sec_type: contract.sec_type.clone(),
                currency: contract.currency.clone(),
                ..Default::default()
            },
            req_id: wire,
            filters: contract.lookup_filters(),
            num_rows,
            is_smart_depth,
        })
    }

    /// Cancel market depth. Matches `cancelMktDepth` in C++.
    pub fn cancel_mkt_depth(&self, req_id: i64) -> Result<(), Refusal> {
        let wire = wire_req_id(req_id)?;
        // A caller withdrawing a book this client does not hold branches on
        // being told so, under the number the catalogue gives depth rather
        // than the one a quote subscription is withdrawn under.
        self.core.release_the_book(req_id)?;
        self.send(ControlCommand::UnsubscribeDepth { req_id: wire })
    }

    // ── Real-Time Bars ──

    /// Subscribe to real-time 5-second bars. Matches `reqRealTimeBars` in C++.
    ///
    /// `bar_size` is taken and not applied. The venue's real-time bar is five
    /// seconds and there is no field asking for another; the reference client
    /// takes the number and sends none either.
    pub fn req_real_time_bars(
        &self, req_id: i64, contract: &Contract,
        _bar_size: i32, what_to_show: &str, use_rth: bool,
    ) -> Result<(), Refusal> {
        // Refused here rather than turned into trades on the way out: a
        // misspelled "BID" answered with trade bars looks like data.
        crate::control::historical::BarDataType::from_api_str(what_to_show)?;
        let wire = wire_req_id(req_id)?;
        // A historical request that finished under this number left the number
        // marked as one whose bars are updates to it, and only a new or a
        // cancelled historical request cleared that mark. Backfill and then
        // stream on the same number — the ordinary way to write it — and every
        // bar of the stream arrived as an update to the finished request, so a
        // caller that overrode only the stream read it as dead.
        self.core.historical_request_is_new(wire);
        self.send(ControlCommand::SubscribeRealTimeBar {
            contract: contract.into(),
            req_id: wire,
            filters: contract.lookup_filters(),
            what_to_show: what_to_show.into(),
            use_rth,
        })
    }

    /// Cancel real-time bars. Matches `cancelRealTimeBars` in C++.
    pub fn cancel_real_time_bars(&self, req_id: i64) -> Result<(), Refusal> {
        self.send(ControlCommand::CancelRealTimeBar { req_id: wire_req_id(req_id)? })
    }

    /// Request an auth-connection round-trip time sample: sends a
    /// lightweight liveness probe with no side effects on subscriptions,
    /// contract caches, or pacing budgets. The result lands asynchronously —
    /// poll `last_rtt()` after a moment. No-op while a probe is already in
    /// flight or the connection is down.
    pub fn req_ping(&self) -> Result<(), Refusal> {
        self.send(ControlCommand::Ping)
    }

    /// Last measured auth-connection round-trip time, if any.
    /// A gauge, not a benchmark: the sample is the interval from a probe to
    /// the first inbound traffic that followed it, which on an active feed
    /// can undercount by racing data already in flight. Also sampled
    /// automatically whenever liveness sends its own probe.
    pub fn last_rtt(&self) -> Option<std::time::Duration> {
        self.shared.last_ccp_rtt()
    }

    /// Which feed the subscriptions after this one ask for: 1 live, 2 frozen,
    /// 3 delayed, 4 delayed and frozen.
    ///
    /// Sent with each subscription, in the field this protocol carries it in,
    /// and the `market_data_type` callback reports the type the subscription
    /// was made under. To state it for one request rather than for the ones
    /// that follow, [`req_mkt_data_ex`](EClient::req_mkt_data_ex) takes it.
    /// A number naming no type leaves subscriptions realtime, and says so.
    pub fn req_market_data_type(&self, market_data_type: i32) {
        if self.session_over() { return self.report_reason(-1, &Refusal::not_connected("Not connected")); }
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
    /// Returns `None` for an id outside the instrument table.
    #[inline]
    pub fn quote_by_instrument(&self, instrument: InstrumentId) -> Option<Quote> {
        self.shared.market.try_quote(instrument)
    }

    /// What the venue's own model last made of an option, whole.
    ///
    /// [`Wrapper::tick_option_computation`] carries eight figures, which is
    /// what the documented callback has room for; the venue states eighteen on
    /// the same tick. The ten it has no room for — the rate greek, the expected
    /// time to exercise and the price that triggers it, the forward
    /// coefficient, two yields, the time value, the days the model counted, the
    /// rate it discounted at, and which kind of volatility it priced on — were
    /// decoded and dropped. They are on the record this returns.
    ///
    /// A figure the venue did not state is `f64::MAX`, as everywhere else on
    /// this record; zero is a real greek.
    ///
    /// `None` where the request names no subscription, or the venue has not
    /// stated a model for it yet.
    ///
    /// [`Wrapper::tick_option_computation`]: crate::api::Wrapper::tick_option_computation
    pub fn option_model(&self, req_id: i64) -> Option<crate::types::OptionComputation> {
        let instrument = *self.core.req_to_instrument.lock().unwrap().get(&req_id)?;
        self.shared.market.option_model(instrument)
    }

    /// Whether the venue is restricting short sales in the contract a request
    /// is watching.
    ///
    /// The circuit breaker a venue puts on a contract that has fallen far
    /// enough in a day, which stops a short from resting below the bid. The
    /// venue states it on the same record as the halt, and it has no field
    /// anywhere in the documented API. Not the same question as whether the
    /// contract can be borrowed, which [`Wrapper::tick_generic`] already
    /// answers beside it: a contract can be freely borrowable and still
    /// restricted.
    ///
    /// `false` where the request names no subscription, as it is for a
    /// contract the venue has not restricted.
    ///
    /// [`Wrapper::tick_generic`]: crate::api::Wrapper::tick_generic
    pub fn short_sale_restricted(&self, req_id: i64) -> bool {
        let held = self.core.req_to_instrument.lock().unwrap();
        held.get(&req_id)
            .is_some_and(|&iid| self.shared.market.short_sale_restricted(iid))
    }

    /// The same, by InstrumentId, for callers who track them themselves.
    pub fn short_sale_restricted_by_instrument(&self, instrument: InstrumentId) -> bool {
        self.shared.market.short_sale_restricted(instrument)
    }

    /// What the venue says about the contract itself, beside its prices.
    ///
    /// How many shares the company has on issue — the multiplier that turns a
    /// price into a market capitalisation — and what the contract opened at a
    /// year ago, which gives a trailing return without asking for a year of
    /// history. Both arrive on the tick that carries the price extremes, on
    /// every subscription that asks for them, and both were read past.
    ///
    /// The documented API reaches neither from a quote: a share count is a
    /// fundamentals request of its own there, and a year-ago open has no call
    /// at all. So they are read here rather than sent as ticks — a tick number
    /// of this client's own choosing, where a caller reads the venue's, is not
    /// something this client invents.
    ///
    /// `None` where the request names no subscription, or the venue has stated
    /// neither figure for it yet; `f64::MAX` for a figure it has not stated.
    pub fn contract_figures(&self, req_id: i64) -> Option<crate::types::ContractFigures> {
        let instrument = *self.core.req_to_instrument.lock().unwrap().get(&req_id)?;
        self.shared.market.contract_figures(instrument)
    }

    /// The same, by InstrumentId, for callers who track them themselves.
    pub fn contract_figures_by_instrument(
        &self, instrument: InstrumentId,
    ) -> Option<crate::types::ContractFigures> {
        self.shared.market.contract_figures(instrument)
    }

    /// What one of the venue's own series last stated for a subscription, in
    /// the order the series states it.
    ///
    /// The venue runs dozens of series the documented API has no call for: a
    /// bond's analytics, an option's model volatility, the volume a contract
    /// usually opens on, what margin a future takes. Ask for one by its own
    /// number in the generic tick list, and read what it stated here.
    ///
    /// The figures come as the venue stated them — its order, its widths — and
    /// a figure it holds nothing for comes as the largest number its field
    /// carries. Empty where the request names no subscription, or that series
    /// has stated nothing for it.
    ///
    /// Read here rather than sent as ticks for the same reason the contract
    /// figures are: a tick number of this client's own choosing, where a caller
    /// reads the venue's, is not something this client invents.
    pub fn stated_figures(&self, req_id: i64, series: u32) -> Vec<f64> {
        let Some(instrument) = self.core.req_to_instrument.lock().unwrap().get(&req_id).copied()
        else {
            return Vec::new();
        };
        self.shared.market.stated_figures(instrument, series)
    }

    /// What one of the venue's numbered-table series last stated for a
    /// subscription: its figures under the venue's own numbering.
    ///
    /// Four series state their figures as two tables, whole numbers and
    /// fractional ones, each entry naming what it is before stating it. Two of
    /// those numbers have a documented call to arrive on and the rest have
    /// none; the rest are here, under the number the venue gave them.
    ///
    /// `fractional` picks the table. The venue numbers the two separately, so
    /// the same number in each is not the same figure.
    pub fn numbered_figures(
        &self, req_id: i64, series: u32, fractional: bool,
    ) -> Vec<(i32, f64)> {
        let Some(instrument) = self.core.req_to_instrument.lock().unwrap().get(&req_id).copied()
        else {
            return Vec::new();
        };
        self.shared.market.numbered_figures(instrument, series, fractional)
    }

    /// The run of paired figures one series last stated for a subscription.
    ///
    /// Two series state their figures as a count and then that many pairs: the
    /// volatility the venue's own model puts on each point of a curve, and the
    /// weight it puts on each price a contract might reach. Neither has a
    /// documented call to arrive on.
    pub fn paired_figures(&self, req_id: i64, series: u32) -> Vec<(f64, f64)> {
        let Some(instrument) = self.core.req_to_instrument.lock().unwrap().get(&req_id).copied()
        else {
            return Vec::new();
        };
        self.shared.market.paired_figures(instrument, series)
    }

    /// Which series have stated paired figures for a subscription, in order.
    pub fn paired_figures_series(&self, req_id: i64) -> Vec<u32> {
        let Some(instrument) = self.core.req_to_instrument.lock().unwrap().get(&req_id).copied()
        else {
            return Vec::new();
        };
        self.shared.market.paired_figures_series(instrument)
    }

    /// Which series have stated numbered figures for a subscription, in order.
    pub fn numbered_figures_series(&self, req_id: i64) -> Vec<u32> {
        let Some(instrument) = self.core.req_to_instrument.lock().unwrap().get(&req_id).copied()
        else {
            return Vec::new();
        };
        self.shared.market.numbered_figures_series(instrument)
    }

    /// Which series have stated figures for a subscription, in order.
    pub fn stated_figures_series(&self, req_id: i64) -> Vec<u32> {
        let Some(instrument) = self.core.req_to_instrument.lock().unwrap().get(&req_id).copied()
        else {
            return Vec::new();
        };
        self.shared.market.stated_figures_series(instrument)
    }

    /// What the venue's model made of an option as it closed.
    ///
    /// The same model as [`Self::option_model`] and in the same shape — every
    /// greek it states, the ones the documented API has no field for included —
    /// but worked out as the contract closed rather than as it stands. The
    /// documented API has no call for it at all. Ask for it by the venue's own
    /// number for the series in the generic tick list.
    pub fn closing_option_model(&self, req_id: i64) -> Option<crate::types::OptionComputation> {
        let instrument = *self.core.req_to_instrument.lock().unwrap().get(&req_id)?;
        self.shared.market.closing_option_model(instrument)
    }

    /// The same, by InstrumentId, for callers who track them themselves.
    pub fn closing_option_model_by_instrument(
        &self, instrument: InstrumentId,
    ) -> Option<crate::types::OptionComputation> {
        self.shared.market.closing_option_model(instrument)
    }

    /// The same, by InstrumentId, for callers who track them themselves.
    pub fn option_model_by_instrument(
        &self, instrument: InstrumentId,
    ) -> Option<crate::types::OptionComputation> {
        self.shared.market.option_model(instrument)
    }
}
