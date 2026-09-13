//! Event dispatch: drains SharedState queues and fires Wrapper callbacks.

use std::sync::atomic::Ordering;
use crate::types::qty_to_f64;
use crate::types::model::{
    BarData, CommissionAndFeesReport, ContractDetails, ContractDescription, Execution,
    Order as ApiOrder, OrderState, TickAttribLast, TickAttribBidAsk, PRICE_SCALE_F,
};
use crate::api::wrapper::Wrapper;
use crate::bridge::RecordKind;
use crate::types::order_status::order_status_str;
const QTY_SCALE_F: f64 = crate::types::QTY_SCALE as f64;

use crate::types::*;

use super::{Contract, EClient};

/// The tick type a model computation is reported under, matching the reference
/// client's own numbering.
const MODEL_OPTION_COMPUTATION: i32 = 13;

/// The same on a delayed feed, which the reference client numbers apart.
///
/// A program that asked for delayed data reads its model there; delivered
/// under 13 it arrived indistinguishable from a live reading, on a feed the
/// caller had been told was delayed.
const DELAYED_MODEL_OPTION_COMPUTATION: i32 = 83;

/// Tick type 53: a computation this client was asked for.
///
/// The stream and the answer are two different things, and the venue names
/// them apart: a caller watching a contract reads the model on 13, and a
/// caller who asked what a volatility implies reads their answer on 53. Sent
/// under 13, an answer arrived indistinguishable from the stream.
const ASKED_OPTION_COMPUTATION: i32 = 53;

/// What the reference client reports when a contract cannot be named.
const NO_SECURITY_DEFINITION: i64 = 200;

/// Reported against no request, the way the reference client reports anything
/// it cannot attribute to one.
pub(crate) const NO_REQUEST: i64 = -1;

/// The venue said something went wrong and stated no code for it. This one
/// says only that the venue is the one saying it.
const VENUE_REPORTED: i64 = 321;

/// What the venue sends every session when it has something to say that
/// belongs to no request. The number is the venue's own, not a stand-in: it
/// arrives on the session as a message of its own subtype and reaches every
/// client under this code.
const VENUE_MESSAGE: i64 = 2148;

/// What the reference client reports when data a caller asked for is not being
/// served. A book this client has given up on is not being served: the venue
/// goes on sending it and nothing further is kept, until the caller withdraws
/// it and asks again.
pub(crate) const DEPTH_NOT_SERVED: i64 = 354;

impl EClient {
    // ── Message Processing ──

    /// Drain all SharedState queues and dispatch to the Wrapper.
    /// Call this in a loop — it is the Rust equivalent of C++ `EReader::processMsgs()`.
    ///
    /// Reading takes the session's turn, because the queues empty as they are
    /// read. The calls that answer pump this loop themselves and keep what
    /// carries their own request id, so a second reader running beside one
    /// takes the answer first and hands it to a callback — and the question
    /// waits out its whole deadline for a reply that had already arrived. The
    /// turn is released before this returns, so a loop calling it holds
    /// nothing between reads.
    pub fn process_msgs(&self, wrapper: &mut impl Wrapper) {
        // A read from inside a read is served by the one it is inside. The
        // turn this thread holds is not re-entrant, so waiting for it here
        // ends the program; and what this would drain is the outer read's to
        // deliver, to the wrapper that read was given.
        if super::reading_now() == self.which_session() {
            return;
        }
        let _turn = self.asking.lock().unwrap_or_else(|e| e.into_inner());
        self.read_the_session(wrapper, true);
    }

    /// The same read, for a caller that already holds the turn.
    ///
    /// The turn is not re-entrant, and a question that took it before sending
    /// pumps this loop while it waits.
    /// `durable` is false for the pump an answering call runs into a
    /// collector with no session record behind it: the venue-data notices
    /// are left queued for the caller's own loop rather than drained into a
    /// collector that drops them and the quote baseline forgotten with
    /// nothing said.
    pub(crate) fn read_the_session(&self, wrapper: &mut impl Wrapper, durable: bool) {
        // Said for the length of the read, so a question asked from inside one
        // of the callbacks below is told why it cannot be answered rather than
        // left waiting on this thread's own turn.
        let _reading = super::Reading::begin(self.which_session());
        self.dispatch_venue_data(wrapper, durable);
        self.dispatch_positions(wrapper);
        self.dispatch_orders(wrapper);
        self.dispatch_quotes(wrapper);
        self.dispatch_data(wrapper);
        self.dispatch_connection(wrapper);
    }

    // ── Connection Dispatch ──

    /// Surface the end of the session: `connection_closed`, once, with no error
    /// callback. Covers both an engine-side loss and an explicit
    /// [`disconnect()`](EClient::disconnect).
    ///
    /// Queued data is dispatched before this fires, so a caller that stops
    /// polling on `connection_closed` still sees everything the engine had
    /// already produced.
    fn dispatch_connection(&self, wrapper: &mut impl Wrapper) {
        use std::sync::atomic::Ordering;
        let went = self.shared.take_connection_lost();
        let came_back = self.shared.take_connection_restored();
        // Applied in the order they were read, which is the order they
        // happened in: each setter clears the other, so both in hand means the
        // recovery landed between the two reads above and the loss is the
        // older of the two. Applied the other way round, the loss went on last
        // and a session that had come back read as disconnected for the rest
        // of its life, with the caller told it recovered before it was told it
        // had gone. The other surface reads and applies them in this order.
        if went {
            self.connected.store(false, Ordering::Release);
            // A loss the engine is still working to recover is said under
            // 1100, as the other surface says it and as this surface's own
            // contract states; `connection_closed` answered it instead, and a
            // program that stands down on that stood down on an outage the
            // engine recovered from. A stop the caller asked for says nothing
            // here: the reference client answers `disconnect()` with
            // `connection_closed` alone.
            if !self.shared.connection_lost_by_design() && !self.stopped_by_caller.load(Ordering::Acquire) {
                wrapper.error(-1, 1100, "Connectivity between client and server has been lost", "");
            }
        }
        // A recovered session is connected again, and `close_notified` has to
        // come back with it: left latched, the next loss would pass without
        // firing `connection_closed` at all.
        if came_back {
            let was_connected = self.connected.swap(true, Ordering::AcqRel);
            self.close_notified.store(false, Ordering::Release);
            // 1102 pairs with a 1100: it is a recovery from a loss the caller
            // was told about. Where a poll spanned a whole outage and recovery
            // the two flags collapse to the restore alone, and a 1102 with no
            // 1100 before it reads as a recovery from nothing — so it is said
            // only where this surface believed it was disconnected. 1102 rather
            // than 1101 because the reconnect re-establishes the subscriptions.
            if !was_connected {
                wrapper.error(-1, 1102, "Connectivity between client and server has been restored - data maintained", "");
            }
        }
        // The session over — ended by the caller, or given up on — is what
        // `connection_closed` says, once.
        let over = self.shared.reference.session_over().is_some()
            || self.shared.connection_lost_by_design()
            || self.stopped_by_caller.load(Ordering::Acquire);
        if over
            && !self.connected.load(Ordering::Acquire)
            && !self.close_notified.swap(true, Ordering::AcqRel)
        {
            wrapper.connection_closed();
        }
    }

    /// One of the connections the venue keeps data on went away or came back,
    /// said under the number the venue reports it under, and said first: a
    /// caller reading quotes has nothing else to tell it that the last price
    /// it holds stopped being a price. A market-data loss also forgets what
    /// each caller was last told of every quote — the engine zeroes the
    /// quotes at the drop, and diffed against what the caller had heard those
    /// noughts went out as prices; diffed against nothing, only what the venue
    /// restates goes out.
    fn dispatch_venue_data(&self, wrapper: &mut impl Wrapper, durable: bool) {
        // No session record behind the collector: leave the notices queued
        // so the caller's own loop delivers them, rather than drain them
        // into a collector that drops them and forget the quote baseline
        // with nothing said.
        if !durable {
            return;
        }
        for (which, up) in self.shared.drain_venue_data_notices() {
            if matches!(which, crate::bridge::VenueDataConnection::MarketData) && !up {
                self.core.forget_last_quotes();
            }
            let (broken, ok) = which.codes();
            wrapper.error(-1, if up { ok } else { broken }, which.says(up), "");
        }
    }

    // ── Order / Fill Dispatch ──

    /// A holding that has moved since the caller last heard, where the caller
    /// asked for positions and has not withdrawn the ask.
    ///
    /// The feed is real-time: a fill that changes what the account holds is
    /// followed by the holding it changed. Nothing is drained while no ask
    /// stands — [`EClient::req_positions`] clears what accumulated before it,
    /// and draining here as well discarded the moves that landed while it was
    /// still assembling its answer, which no later report would repeat.
    fn dispatch_positions(&self, wrapper: &mut impl Wrapper) {
        let on_position = self.positions_requested.load(Ordering::Acquire);
        let per_request: Vec<i64> = {
            let watching = self.positions_multi_requested.lock().unwrap();
            let mut ids: Vec<i64> = watching.iter().copied().collect();
            ids.sort_unstable();
            ids
        };
        if !on_position && per_request.is_empty() {
            return;
        }
        // Drained once and given to everyone watching. Drained per watcher,
        // the first would take the move and the rest would never hear of it.
        let moved = self.shared.portfolio.drain_position_changes();
        for pi in &moved {
            let contract = self.position_contract(pi);
            let avg_cost = pi.avg_cost as f64 / PRICE_SCALE_F;
            if on_position {
                wrapper.position(&self.account_id, &contract, pi.position, avg_cost);
            }
            // The account this session opened under, whatever the request
            // named, as the answer to the request itself states.
            for req_id in &per_request {
                wrapper.position_multi(
                    *req_id, &self.account_id, "", &contract, pi.position, avg_cost,
                );
            }
        }
    }

    /// Answer the calculations that were waiting on the venue to state a
    /// model, and forget them.
    ///
    /// A question the venue still cannot answer is kept: the watch is open, so
    /// the model may yet arrive. It is dropped when the caller withdraws it.
    fn answer_kept_option_calcs(&self) {
        let kept: Vec<(i64, crate::api::client::PendingOptionCalc)> = self
            .pending_option_calcs.lock().unwrap()
            .iter().map(|(k, v)| (*k, v.clone())).collect();
        for (req_id, calc) in kept {
            if calc.answered {
                continue;
            }
            let answered = if calc.wants_volatility {
                self.solve_and_push_volatility(req_id, &calc)
            } else {
                self.solve_and_push_price(req_id, &calc)
            };
            if answered {
                // Marked, not dropped. The watch this client opened to obtain
                // the model comes down where the caller withdraws the
                // calculation, and a question that has been dropped cannot be
                // withdrawn — the withdrawal finds nothing and cancels nothing,
                // leaving a subscription the caller cannot take down. Nothing
                // is sent here; the mark only stops it being solved again.
                if let Some(kept) = self.pending_option_calcs.lock().unwrap().get_mut(&req_id) {
                    kept.answered = true;
                }
            }
        }
    }

    fn dispatch_orders(&self, wrapper: &mut impl Wrapper) {
        // Fills → order_status + exec_details + commission_and_fees_report
        // One `order_status` per execution report: a report carrying both a fill
        // and a status emits them together.
        //
        // Held as a list in arrival order. Each status change is a separate
        // report, so two changes to one order in a single pass are two
        // callbacks.
        // Records held back while a fill for them was queued, freed once that
        // fill has been read. Freed here and nowhere else: this is the side
        // that reads the fills, so a record cannot be freed between a fill
        // being taken off the queue and the report that is built from it.
        if !self.deferred_evictions.lock().unwrap().is_empty() {
            self.deferred_evictions.lock().unwrap().retain(|oid| {
                if self.shared.orders.has_pending_fill(*oid) {
                    return true;
                }
                self.shared.orders.remove_completed_order_info(*oid);
                false
            });
        }
        let mut paired: Vec<crate::types::OrderUpdate> =
            self.shared.orders.drain_order_updates();
        // Taken before the fills below, and reported after them.
        //
        // The engine pushes a fill and then, off a message of its own, the
        // charge that names it. Taken after the fills, a charge whose fill was
        // pushed between the two drains arrives with nothing stored under its
        // execution: the charge is dropped by a caller that reads its fills
        // first, and the fill is filed for a replay with its cost unknown for
        // ever. Taken first, every charge in hand has its fill either already
        // stored by an earlier pass or in the batch below.
        let charges = self.shared.orders.drain_charges();
        for (fill, booked_off) in self.shared.orders.drain_fills() {
            let price_f = fill.price as f64 / PRICE_SCALE_F;
            // Paired on the report, not the order: one pass can carry both an
            // acknowledgement and a fill for the same order. The matching report
            // is the one whose filled and remaining quantities equal the fill's.
            let with_it = paired
                .iter()
                .position(|u| {
                    u.order_id == fill.order_id
                        && u.remaining_qty == qty_to_f64(fill.remaining)
                        && u.filled_qty == qty_to_f64(fill.cum_qty)
                })
                .map(|at| paired.remove(at));
            // Status as the report states it, derived from `remaining` only when
            // the report carries none.
            let status = with_it
                .map(|u| order_status_str(u.status))
                .unwrap_or(if fill.remaining == 0 { "Filled" } else { "Submitted" });
            if let Some(u) = with_it {
                self.core.update_order_status(
                    &self.shared, u.order_id, u.status, u.filled_qty, u.remaining_qty,
                    u.instrument,
                );
            }
            let (perm_id, parent_id) = self.core.perm_and_parent(&self.shared, fill.order_id);
            // `filled` and `avgFillPrice` describe the order so far;
            // `lastFillPrice` describes this print.
            let avg_price_f = fill.avg_price as f64 / PRICE_SCALE_F;

            let side_str = match fill.side {
                Side::Buy => "BOT",
                Side::Sell => "SLD",
                Side::ShortSell => "SLD",
            };
            // The report this fill was booked off, not whatever the order's
            // record says now: one pass can carry two prints of one order, and
            // the record holds only the later.
            let booked_off = booked_off.or_else(|| self.shared.orders.get_order_info(fill.order_id));
            let (c, exec) = if let Some(info) = booked_off {
                let mut ex = info.last_exec;
                ex.side = side_str.into();
                ex.shares = qty_to_f64(fill.qty);
                ex.price = price_f;
                ex.order_id = fill.order_id as i64;
                // The client that placed it, where the report names none. The
                // record knows which client the order went out under and the
                // venue's report may carry no client at all; taken as stated,
                // it filed this session's own fill under client zero while the
                // status above was announced under the placing client.
                if ex.client_id == 0 {
                    ex.client_id = i64::from(self.core.placing_client(&self.shared, fill.order_id));
                }
                let contract = if info.contract.con_id != 0 {
                    self.core.get_contract(info.contract.con_id, &self.shared).unwrap_or(info.contract)
                } else {
                    info.contract
                };
                (contract, ex)
            } else {
                (Contract::default(), Execution {
                    side: side_str.into(),
                    shares: qty_to_f64(fill.qty),
                    price: price_f,
                    order_id: fill.order_id as i64,
                    // The venue stated nothing about this one, so the client
                    // that placed the order is what names it. Left at nought,
                    // a request filtered by client matched none of them.
                    client_id: i64::from(
                        self.core.placing_client(&self.shared, fill.order_id),
                    ),
                    ..Default::default()
                })
            };
            // Unsolicited executions carry request id -1. A market-data
            // subscription id does not identify a `reqExecutions` request.
            let req_id = NO_REQUEST;
            // Stored before either callback about the print. A caller that
            // asks for its executions from inside the status callback or this
            // one -- which is ordinary -- was answered without the fill it was
            // being told about.
            //
            // What it cost is not stated here. It arrives on a record of its
            // own and is reported from the drain below. Stored unstated so a
            // replay of this execution says the charge is unknown rather than
            // that it was nothing.
            self.core.push_execution(c.clone(), exec.clone(), CommissionAndFeesReport::default());
            wrapper.order_status(
                fill.order_id as i64, status, qty_to_f64(fill.cum_qty), qty_to_f64(fill.remaining),
                avg_price_f, perm_id, parent_id, price_f,
                self.core.placing_client(&self.shared, fill.order_id) as i64, "", 0.0,
            );
            wrapper.exec_details(req_id, &c, &exec);

            // Update open order tracking
            self.core.update_order_fill(fill.order_id, status, qty_to_f64(fill.cum_qty), qty_to_f64(fill.remaining));
        }

        // Executions the venue restated rather than announced. Filed for
        // `req_executions` and reported to nobody: a caller that asks is
        // answered, and one that did not hears nothing.
        self.core.record_restated_executions(&self.shared);

        // What the venue says its fills cost, each naming the execution it
        // belongs to. Reported after the executions above, which is the order
        // they arrive in and the order a caller reads them in.
        for charge in charges {
            self.core.record_charge(&charge);
            wrapper.commission_and_fees_report(&charge);
        }

        // What is left: a status change with no fill on the same report.
        for update in paired {
            let status = order_status_str(update.status);
            // The engine reads no parent from the report, but this client
            // placed the order and was told. Prefer what it recorded; an order
            // it did not place keeps the engine's answer of none.
            let parent_id = self.core.tracked_parent_id(update.order_id)
                .unwrap_or(update.parent_id);
            // What the order has paid, as the report that changed its status
            // stated it. Reported as zero, a status arriving just after a fill
            // told the caller the order had filled at no price at all.
            let avg = update.avg_price as f64 / crate::types::PRICE_SCALE as f64;
            // The order as this client sent it, beside the status it is now
            // in. The reference client answers an order's every change with
            // both, from the order it holds — its own method for it sends the
            // pair — and a program that waits for the order to confirm what it
            // asked for waited on a callback that only arrived if it asked for
            // its open orders. The other surface sends the pair here too.
            //
            // Copied out before the callback rather than read across it: a
            // guard built in the scrutinee is held for the whole body, and the
            // body calls the caller's own code. A wrapper that reaches the
            // order cache from there would wait on a lock its own caller holds.
            let tracked = self.core.open_orders.lock().unwrap().get(&update.order_id).cloned();
            // Not for a preview. What the venue would do with an order is
            // asked for by placing one, so a preview is a tracked order like
            // any other and its statuses arrive here — but the answer to a
            // preview is the margin the venue states, and that arrives on its
            // own reply further down. The question waits on the first of these
            // carrying its number, so a pair sent from the status alone
            // answered it: no margin figures, and a cost of nought rather than
            // the number that means unstated, on the one call whose whole
            // purpose is to say what an order would cost.
            if let Some(tracked) = tracked.filter(|t| !t.order.what_if) {
                // What the venue said about the order, under the status this
                // client names it by. Built from the status alone, everything
                // beside it — what the order would cost, the margin figures and
                // the warning — came back at nought on every change, to the
                // caller this pair exists for.
                let state = OrderState {
                    status: status.to_string(),
                    ..self.shared.orders.get_order_info(update.order_id)
                        .map(|i| i.order_state)
                        .unwrap_or_default()
                };
                wrapper.open_order(
                    update.order_id as i64, &tracked.contract, &tracked.order, &state,
                );
            }
            wrapper.order_status(
                update.order_id as i64, status, update.filled_qty,
                update.remaining_qty, avg, update.perm_id, parent_id, 0.0,
                self.core.placing_client(&self.shared, update.order_id) as i64, "", 0.0,
            );
            self.core.update_order_status(&self.shared, update.order_id, update.status, update.filled_qty, update.remaining_qty, update.instrument);
        }

        // A replacement the venue has taken spends the terms kept against a
        // refusal of it. Before the refusals below, so an acceptance and a
        // stale refusal arriving in one pass leave the record on what the
        // venue holds.
        for order_id in self.shared.orders.drain_replacements_taken() {
            self.core.settle_replacement(order_id);
        }

        // Cancel rejects → error
        for reject in self.shared.orders.drain_cancel_rejects() {
            let (code, msg) = self.core.retire_rejected(&reject);
            wrapper.error(reject.order_id as i64, code, &msg, "");
        }

        // Inactive (39=I) order reasons → error. order_status above
        // already reported the "Inactive" string; this carries why.
        for (order_id, code, msg) in self.shared.orders.drain_order_inactive() {
            // A refusal is the end of a preview: it states what an order
            // would have cost, and nothing reached the book. Left
            // standing, the record read as a working order and its number
            // as spent.
            if self.core.tracked_order(order_id).is_some_and(|o| o.what_if) {
                self.core.untrack_order(order_id);
            }
            wrapper.error(order_id as i64, code as i64, &msg, "");
        }

        // A preview and nothing else, as the other surface already answers it.
        // The venue answers what an order would cost, on the order itself: it
        // states no status for it, nothing filled, no permanent number and no
        // client, so a status beside it was eleven fields composed here about
        // an order that was never placed. The reference client's own decode of
        // an open order calls one callback and this is it.
        for wi in self.shared.orders.drain_what_if_responses() {
            let state = OrderState::from(&wi);
            // Taken before the callback rather than after it. A preview is
            // finished the moment it is answered, and left recorded while its
            // own callback runs an order placed from inside that callback under
            // the same number read as a change to the preview and was refused
            // for being one. A callback that ends the read another way left it
            // recorded for the rest of the session, under a number nothing
            // could place again.
            let tracked = self.core.open_orders.lock().unwrap().remove(&wi.order_id);
            let (contract, order) = tracked
                .map(|t| (t.contract, t.order))
                .unwrap_or_else(|| (Contract::default(), ApiOrder::default()));
            wrapper.open_order(wi.order_id as i64, &contract, &order, &state);
        }
    }

    // ── Quote Dispatch ──

    fn dispatch_quotes(&self, wrapper: &mut impl Wrapper) {
        // The increment each subscription was acknowledged with, to everyone
        // watching the contract, once: the reference client delivers it on
        // `tick_req_params` ahead of the first tick.
        //
        // The exchange and the permission figure beside it are stated on the
        // acknowledgement only above a version this session does not negotiate
        // — every acknowledgement seen here carries five fields and neither of
        // them — so they go out empty and nought. That is what the venue said,
        // not a claim that it never says more.
        for (req_id, min_tick) in self.shared.market.drain_tick_req_params_direct() {
            wrapper.tick_req_params(req_id, min_tick, "", 0);
        }
        // A request that joined a contract the venue had already refused. The
        // refusal it joined was drained and told once, to whoever held the
        // contract then, so this one heard nothing and had nothing coming.
        for (req_id, reason) in self.shared.market.drain_subscription_failures_direct() {
            wrapper.error(req_id, NO_SECURITY_DEFINITION, &reason, "");
        }
        for (instrument, min_tick) in self.shared.market.drain_tick_req_params() {
            for req_id in self.core.watchers_of(instrument) {
                wrapper.tick_req_params(req_id, min_tick, "", 0);
            }
        }
        // Quote polling → tick_price / tick_size (via ClientCore)
        let instruments = self.core.snapshot_instruments();
        // What the venue says about a price rather than what it is. It states
        // both sides in one mask and a caller is owed an attribute beside each
        // price, so the side is read off the tick the attribute goes out with.
        let mut snapshot_done: Vec<(i64, Option<u64>)> = Vec::new();
        for (iid, req_id, watchers) in instruments {
            let result = self.core.poll_instrument_ticks(&self.shared, iid, req_id);
            // Ahead of everything this pass delivers, and to everyone it
            // delivers to. The type a caller is served under is stated before
            // the data it applies to, which is the order the reference client
            // keeps.
            //
            // It was stated for the holder alone, and only where the pass
            // carried a price or a size. A caller following the contract got
            // it from inside the price loop, so one whose first event was a
            // halt, an exchange letter or the last-trade time was handed data
            // first — and a pass carrying only those counts as nothing
            // delivered, so neither of them was told at all.
            let delivering = result.delivered
                || !result.generic_ticks.is_empty()
                || !result.string_ticks.is_empty()
                || result.timestamp.is_some();
            for id in std::iter::once(req_id).chain(watchers.iter().copied()) {
                if let Some(mdt) = self.core.check_mdt_needed(id, delivering) {
                    wrapper.market_data_type(id, mdt);
                }
            }
            // Which kinds the venue has stated, for anything waiting on a
            // snapshot of this contract.
            for tick in &result.ticks {
                for id in std::iter::once(tick.req_id).chain(watchers.iter().copied()) {
                    self.core.note_snapshot_tick(id, tick.tick_type);
                }
            }
            for tick in &result.ticks {
                for id in std::iter::once(tick.req_id).chain(watchers.iter().copied()) {
                    if tick.is_price {
                        let attrib = crate::types::quote_attributes(
                            tick.tick_type, result.eligible_mask, result.quote_state_mask,
                        );
                        wrapper.tick_price(id, tick.tick_type, tick.value, &attrib);
                    } else {
                        wrapper.tick_size(id, tick.tick_type, tick.value);
                    }
                }
            }
            for tick in &result.generic_ticks {
                for id in std::iter::once(tick.req_id).chain(watchers.iter().copied()) {
                    wrapper.tick_generic(id, tick.tick_type, tick.value);
                }
            }
            for st in &result.string_ticks {
                for id in std::iter::once(st.req_id).chain(watchers.iter().copied()) {
                    wrapper.tick_string(id, st.tick_type, &st.value);
                }
            }
            if let Some(ts) = &result.timestamp {
                let ts_secs = ts.timestamp_ns / 1_000_000_000;
                // Tick type 45 goes to every subscriber of the contract, as the
                // prices and strings above do.
                for id in std::iter::once(ts.req_id).chain(watchers.iter().copied()) {
                    wrapper.tick_string(id, if result.delayed { 88 } else { 45 }, &ts_secs.to_string());
                }
            }
            // The holder and everyone watching it. A caller that asked for a
            // snapshot of a contract somebody was already watching is recorded
            // as a follower, and only the holder was named here — so its
            // snapshot was never completed, never withdrawn, and a caller
            // waiting for the end of it waited for ever.
            for id in std::iter::once(req_id).chain(watchers.iter().copied()) {
                if self.core.check_snapshot_done(id) {
                    // What it was watching when the snapshot finished, so
                    // the withdrawal below can tell this subscription from
                    // whatever the callback leaves under the same number. A
                    // callback is free to withdraw what it has just been told
                    // about and ask for something else — "the snapshot is in,
                    // now stream it" is the obvious thing to write — and the
                    // withdrawal that followed took the number alone, so it
                    // cancelled the subscription the callback had just made.
                    let was_watching = self.core.registration_of(id);
                    wrapper.tick_snapshot_end(id);
                    snapshot_done.push((id, was_watching));
                }
            }
        }
        for (req_id, was_watching) in snapshot_done {
            // Only where the number still holds the subscription the
            // snapshot was for — not merely the same contract, which a
            // callback that re-asked would also show.
            if self.core.registration_of(req_id) == was_watching {
                let _ = self.cancel_mkt_data(req_id);
            }
        }

        // TBT trades → tick_by_tick_all_last
        let trades = self.shared.market.drain_tbt_trades();
        // Locked once per batch: this is the highest-rate feed here.
        let kinds = (!trades.is_empty()).then(|| self.tbt_kinds.lock().unwrap().clone());
        for trade in trades {
            // As the caller numbered it, from the record itself: a contract
            // can carry several streams and the contract alone does not say
            // which one this came from.
            let req_id = trade.req_id;
            // Tick type names the stream the request asked for: 1 = Last,
            // 2 = AllLast. The trade record does not carry it.
            let kind = match kinds.as_ref().and_then(|k| k.get(&req_id)) {
                Some(TbtType::AllLast) => 2,
                _ => 1,
            };
            // What the venue said about this print, not what a default says.
            let attrib_last = TickAttribLast {
                past_limit: trade.past_limit,
                unreported: trade.unreported,
            };
            wrapper.tick_by_tick_all_last(
                req_id, kind, trade.timestamp as i64,
                trade.price as f64 / PRICE_SCALE_F,
                trade.size as f64 / QTY_SCALE_F,
                &attrib_last, &trade.exchange, &trade.conditions,
            );
        }

        // TBT quotes → tick_by_tick_bid_ask
        for quote in self.shared.market.drain_tbt_quotes() {
            let req_id = quote.req_id;
            let attrib_ba = TickAttribBidAsk {
                bid_past_low: quote.bid_past_low,
                ask_past_high: quote.ask_past_high,
            };
            wrapper.tick_by_tick_bid_ask(
                req_id, quote.timestamp as i64,
                quote.bid as f64 / PRICE_SCALE_F, quote.ask as f64 / PRICE_SCALE_F,
                quote.bid_size as f64 / QTY_SCALE_F,
                quote.ask_size as f64 / QTY_SCALE_F,
                &attrib_ba,
            );
        }

        // The point between the two, each time it moved.
        for mid in self.shared.market.drain_tbt_mids() {
            wrapper.tick_by_tick_mid_point(
                mid.req_id, mid.timestamp as i64, mid.price as f64 / PRICE_SCALE_F,
            );
        }

        // The refusal queue, before the book levels below. A reset (317) is
        // queued and then the venue's first new levels land; levels drained
        // first, a pass that ran after both had arrived delivered the new book
        // and then the order to empty it. Still before historical_data, so a
        // QueryError that also queued an empty terminal HistoricalResponse
        // fires wrapper.error first, then wrapper.historical_data_end.
        for (req_id, code, msg) in self.shared.reference.drain_historical_errors_for_dispatch(
            |id| self.shared.reference.left_for_its_reader(id),
        ) {
            wrapper.error(
                crate::bridge::ReferenceState::request_id_reported(req_id),
                code as i64, &msg, "",
            );
        }

        // A book this client could not keep whole, on the request that asked
        // for it. Nothing further is kept for it, so a caller not told reads a
        // subscription that is up and a book that has stopped moving.
        for (req_id, reason) in self.shared.market.drain_depth_drops_for_dispatch(
            |id| self.shared.reference.is_ours(RecordKind::Depth, i64::from(id)),
        ) {
            wrapper.error(i64::from(req_id), DEPTH_NOT_SERVED, &reason, "");
        }

        // Depth updates → update_mkt_depth / update_mkt_depth_l2
        for du in self.shared.market.drain_depth_updates_for_dispatch(
            |id| self.shared.reference.is_ours(RecordKind::Depth, i64::from(id)),
        ) {
            if du.market_maker.is_empty() {
                wrapper.update_mkt_depth(du.req_id as i64, du.position, du.operation, du.side, du.price, du.size);
            } else {
                wrapper.update_mkt_depth_l2(du.req_id as i64, du.position, &du.market_maker, du.operation, du.side, du.price, du.size, du.is_smart_depth);
            }
        }
    }

    // ── Historical / News / Account Dispatch ──

    fn dispatch_data(&self, wrapper: &mut impl Wrapper) {
        // News → tick_news
        for news in self.shared.market.drain_tick_news() {
            // News goes to every subscriber of the contract, as its quotes do.
            for id in self.core.watchers_of(news.instrument) {
                wrapper.tick_news(
                    id, news.timestamp as i64,
                    &news.provider_code, &news.article_id, &news.headline, "",
                );
            }
        }

        // The venue's option model → tick_option_computation. Tick type 13 is
        // the model computation, and the attribute says the model is the
        // venue's rather than a price-based reading.
        // A calculation asked for before the venue had stated a model waited
        // on the watch that asking opened. The model has arrived, so answer
        // the question that was kept rather than leaving the caller with the
        // model it did not ask for.
        if !self.pending_option_calcs.lock().unwrap().is_empty() {
            self.answer_kept_option_calcs();
        }

        for comp in self.shared.market.drain_option_computations() {
            // A computation answering a specific request goes to that request.
            // One published for the contract goes to every subscriber of it.
            let (to, tick_type): (Vec<i64>, i32) = match comp.answers {
                Some(asked) => (vec![asked], ASKED_OPTION_COMPUTATION),
                None => {
                    (
                        self.core.watchers_of(comp.instrument),
                        if self.core.feed_is_delayed(comp.instrument) {
                            DELAYED_MODEL_OPTION_COMPUTATION
                        } else {
                            MODEL_OPTION_COMPUTATION
                        },
                    )
                }
            };
            for req_id in to {
                wrapper.tick_option_computation(
                    req_id, tick_type, 0,
                    comp.implied_vol, comp.delta, comp.opt_price, comp.pv_dividend,
                    comp.gamma, comp.vega, comp.theta, comp.und_price,
                );
            }
        }

        for event in self.core.drain_group_events() {
            match event {
                crate::client_core::GroupEvent::List(req_id, groups) => {
                    wrapper.display_group_list(req_id, &groups);
                }
                crate::client_core::GroupEvent::Updated(req_id, info) => {
                    wrapper.display_group_updated(req_id, &info);
                }
            }
        }

        // What the venue said went wrong. It attributes these to no request,
        // so neither does this.
        for text in self.shared.market.drain_venue_errors() {
            wrapper.error(NO_REQUEST, VENUE_MESSAGE, &text, "");
        }

        // A subscription the venue could not be asked for, because it never
        // named the contract. Reported on the request the caller holds.
        // A lookup that named a contract another slot already holds. One
        // subscription per contract exists on the wire, so the callers given
        // the second slot read the first — otherwise their quotes arrive on a
        // slot nothing is watching.
        for (from, into) in self.shared.market.drain_subscription_moves() {
            let nobody_arrived = self.core.move_watchers(&self.shared, from, into);
            // Said once the move is installed, because the subscription on the
            // slot moved onto is held up until then.
            self.shared.market.note_a_move_is_read(into);
            // And where nobody arrived — the caller this move was for withdrew
            // before it read the move — the subscription it was held up for is
            // nobody's. Left, it ran for the rest of the session against an
            // allowance that is counted, with no request able to withdraw it.
            if let Some(issued) = nobody_arrived {
                let _ = self.send(ControlCommand::Unsubscribe {
                    instrument: into,
                    series: Vec::new(),
                    issued,
                });
            }
        }
        // Everyone watching the contract, not only whoever asked first. A
        // refusal is a fact about the contract, and a caller sharing somebody
        // else's subscription holds no request of its own for the venue to
        // refuse — so it was told nothing and waited for ticks that could not
        // arrive. Where nobody holds it, there is nobody to tell.
        for (instrument, reason) in self.shared.market.drain_subscription_failures() {
            for req_id in self.core.watchers_of(instrument) {
                wrapper.error(req_id, NO_SECURITY_DEFINITION, &reason, "");
            }
        }

        // A news subscription the venue refused: the engine released its side,
        // so the client forgets whoever asked, leaving a later ask free to
        // send anew. The quote it rode beside is untouched and unreported.
        for con_id in self.shared.market.drain_news_rejections() {
            self.core.release_news_askers(con_id);
        }

        // News bulletins → update_news_bulletin (only when subscribed)
        if self.core.bulletins_subscribed() {
            for b in self.shared.market.drain_news_bulletins() {
                wrapper.update_news_bulletin(b.msg_id as i64, b.msg_type, &b.message, &b.exchange);
            }
        }

        // Historical data → historical_data + historical_data_end, and after
        // that end, historical_data_update. A keep-up-to-date request answers
        // once with the history and then keeps speaking; the reference client
        // separates the two, and a caller that overrode only the update
        // callback heard nothing from this surface.
        for (req_id, response) in self.shared.reference.drain_historical_data() {
            let is_update = self.core.hist_initial_complete.lock().unwrap().contains(&req_id);
            self.core.note_historical_zone(req_id as i64, &response.timezone);
            for bar in &response.bars {
                let bd = BarData {
                    date: self.core.bar_time_for(req_id as i64, &bar.time, &response.timezone),
                    open: bar.open,
                    high: bar.high,
                    low: bar.low,
                    close: bar.close,
                    volume: bar.volume,
                    wap: bar.wap,
                    bar_count: bar.count,
                    timezone: response.timezone.clone(),
                };
                if is_update {
                    wrapper.historical_data_update(req_id as i64, &bd);
                } else {
                    wrapper.historical_data(req_id as i64, &bd);
                }
            }
            if response.is_complete && !is_update {
                self.core.hist_initial_complete.lock().unwrap().insert(req_id);
                // As on the other surface: the range the request covered, which
                // is what a caller pages backwards with.
                let (from, to) =
                    self.core.historical_range_for(req_id as i64, &response.timezone);
                wrapper.historical_data_end(req_id as i64, &from, &to);
            }
        }

        // Head timestamps → head_timestamp
        for (req_id, response) in self.shared.reference.drain_head_timestamps() {
            // Returned in the form `format_date` asked for. The wire carries one
            // form; `bar_time_for` converts it. 2 = seconds since the epoch.
            let stated = self.core.bar_time_for(req_id as i64, &response.head_timestamp, "");
            wrapper.head_timestamp(req_id as i64, &stated);
        }

        // Contract details → contract_details + contract_details_end. The
        // ends are taken before the rows: a row and its end landing between
        // the two drains then deliver the row now and the end next pass,
        // where the other order delivered the end first and the row to a
        // caller that had already stopped listening.
        let ends = self.shared.reference.drain_contract_details_end();
        for (req_id, def) in self.shared.reference.drain_contract_details() {
            let details = ContractDetails::from_definition(&def);
            // Fixed income answers on its own callback. A bond, a bill and the
            // type the venue spells `FIXED` share it; every other type is
            // answered on the ordinary one. Answered on the ordinary one too,
            // a program written to wait for a bond waited through the answer.
            if def.sec_type.is_fixed_income() {
                wrapper.bond_contract_details(req_id as i64, &details);
            } else {
                wrapper.contract_details(req_id as i64, &details);
            }
        }
        for req_id in ends {
            wrapper.contract_details_end(req_id as i64);
        }

        // Depth exchanges → mkt_depth_exchanges
        //
        // The request was sent and the reply parsed, and then nothing carried
        // it to the caller: this side never drained it, so the callback the
        // Python surface fires had no counterpart here and the rows simply
        // accumulated.
        let depth_exchanges = self.shared.reference.drain_depth_exchanges();
        if !depth_exchanges.is_empty() {
            wrapper.mkt_depth_exchanges(&depth_exchanges);
        }

        // The calendar's answers, as the venue wrote them.
        for (req_id, json) in self.shared.reference.drain_calendar_meta_data() {
            wrapper.wsh_meta_data(req_id as i64, &json);
        }
        for (req_id, json) in self.shared.reference.drain_calendar_events() {
            wrapper.wsh_event_data(req_id as i64, &json);
        }

        // Matching symbols → symbol_samples
        for (req_id, matches) in self.shared.reference.drain_matching_symbols() {
            let descriptions: Vec<ContractDescription> =
                matches.iter().map(ContractDescription::from).collect();
            wrapper.symbol_samples(req_id as i64, &descriptions);
        }

        // Option chain → security_definition_option_parameter + _end
        // The plain drain, not the one that holds back what an answering call
        // is waiting for: on this client the answering calls receive *through*
        // this loop, so withholding from it withholds from them. `option_chain`
        // came back empty against a live venue while the Python client, whose
        // answering calls take from the queue themselves, was answered.
        for (req_id, underlying_con_id, scopes) in self.shared.reference.drain_option_params() {
            for scope in &scopes {
                wrapper.security_definition_option_parameter(
                    req_id as i64, &scope.exchange, underlying_con_id, &scope.trading_class,
                    &scope.multiplier, &scope.expirations, &scope.strikes,
                );
            }
            wrapper.security_definition_option_parameter_end(req_id as i64);
        }

        // Scanner params
        for xml in self.shared.reference.drain_scanner_params() {
            wrapper.scanner_parameters(&xml);
        }

        // The advisor's own configuration: a partition the caller asked for,
        // the end of one they replaced, and the venue's account of a
        // replacement it would not take.
        for (fa_data_type, xml) in self.shared.reference.drain_advisor_config() {
            wrapper.receive_fa(fa_data_type, &xml);
        }
        for (req_id, text) in self.shared.reference.drain_advisor_replaced() {
            wrapper.replace_fa_end(req_id, &text);
        }
        for (req_id, code, text) in self.shared.reference.drain_advisor_refused() {
            wrapper.error(req_id, i64::from(code), &text, "");
        }

        // Scanner data. Cache is populated by the engine before dispatch
        // (see `CcpState::start_scanner_enrichment`), so cold con_ids that
        // arrived in `<ScanResponse>` have already been resolved via 35=d.
        // The Some-arm fills the rich fields; the fallback covers deadline-
        // flushed partials where a secdef reply never arrived.
        for (req_id, result) in self.shared.reference.drain_scanner_data_for_dispatch(
            |id| self.shared.reference.is_ours(RecordKind::Scanner, i64::from(id)),
        ) {
            // A refused scan arrives in the shape of a completed one and carries
            // the reason. Reported against the requesting id, so a refusal is not
            // delivered as an empty result.
            if !result.error_text.is_empty() {
                wrapper.error(req_id as i64, VENUE_REPORTED, &result.error_text, "");
            }
            for (rank, entry) in result.entries.iter().enumerate() {
                let mut contract = Contract { con_id: entry.con_id as i64, ..Default::default() };
                if let Some(ac) = self.core.get_contract(entry.con_id as i64, &self.shared) {
                    contract.symbol = ac.symbol;
                    contract.sec_type = ac.sec_type;
                    contract.exchange = ac.exchange;
                    contract.currency = ac.currency;
                    contract.local_symbol = ac.local_symbol;
                    contract.primary_exchange = ac.primary_exchange;
                    contract.trading_class = ac.trading_class;
                }
                let details = ContractDetails { contract, ..Default::default() };
                wrapper.scanner_data(req_id as i64, rank as i32, &details, "", "", "", "");
            }
            wrapper.scanner_data_end(req_id as i64);
        }

        // Historical news
        for (req_id, headlines, has_more) in self.shared.reference.drain_historical_news() {
            for h in &headlines {
                wrapper.historical_news(req_id as i64, &h.time, &h.provider_code, &h.article_id, &h.headline);
            }
            wrapper.historical_news_end(req_id as i64, has_more);
        }

        // News articles
        for (req_id, article_type, text) in self.shared.reference.drain_news_articles() {
            wrapper.news_article(req_id as i64, article_type, &text);
        }

        // Fundamental data
        for (req_id, data) in self.shared.reference.drain_fundamental_data() {
            wrapper.fundamental_data(req_id as i64, &data);
        }

        // Histogram data
        for (req_id, entries) in self.shared.reference.drain_histogram_data() {
            let items: Vec<(f64, i64)> = entries.iter().map(|e| (e.price, e.count)).collect();
            wrapper.histogram_data(req_id as i64, &items);
        }

        // Historical ticks route to the variant-specific callback, as in ibapi.
        for (req_id, data, _query_id, done) in self.shared.reference.drain_historical_ticks() {
            match &data {
                HistoricalTickData::Midpoint(_) => wrapper.historical_ticks(req_id as i64, &data, done),
                HistoricalTickData::Last(_) => wrapper.historical_ticks_last(req_id as i64, &data, done),
                HistoricalTickData::BidAsk(_) => wrapper.historical_ticks_bid_ask(req_id as i64, &data, done),
            }
        }

        // Real-time bars, and the continued half of a keep-up-to-date request.
        // The two arrive on one feed and are told apart by whether the request
        // has already answered with its history.
        for (req_id, bar) in self.shared.market.drain_real_time_bars_for_dispatch(
            |id| self.shared.reference.is_ours(RecordKind::Bars, i64::from(id)),
        ) {
            if self.core.hist_initial_complete.lock().unwrap().contains(&req_id) {
                // A forming bar is stamped at its open, in seconds since the
                // epoch; dated as the history before it was, in the caller's
                // format on the zone the series was stated on.
                let bd = BarData {
                    date: self.core.bar_time_for_epoch(req_id as i64, i64::from(bar.timestamp)),
                    open: bar.open,
                    high: bar.high,
                    low: bar.low,
                    close: bar.close,
                    volume: bar.volume as i64,
                    wap: bar.wap,
                    bar_count: bar.count,
                    timezone: String::new(),
                };
                wrapper.historical_data_update(req_id as i64, &bd);
            } else {
                wrapper.real_time_bar(
                    req_id as i64, bar.timestamp as i64,
                    bar.open, bar.high, bar.low, bar.close,
                    bar.volume, bar.wap, bar.count,
                );
            }
        }

        // Historical schedules
        for (req_id, schedule) in self.shared.reference.drain_historical_schedules() {
            let sessions: Vec<(String, String, String)> = schedule.sessions.iter()
                .map(|s| (s.ref_date.clone(), s.open_time.clone(), s.close_time.clone()))
                .collect();
            wrapper.historical_schedule(
                req_id as i64, &schedule.start_date_time, &schedule.end_date_time,
                &schedule.timezone, &sessions,
            );
        }

        // PnL → pnl callback (change-detected via ClientCore)
        if let Some(update) = self.core.poll_pnl(&self.shared) {
            wrapper.pnl(update.req_id, update.daily_pnl, update.unrealized_pnl, update.realized_pnl);
        }

        // PnL single → pnl_single callback (via ClientCore)
        for update in self.core.poll_pnl_single(&self.shared) {
            wrapper.pnl_single(update.req_id, update.pos, update.daily_pnl, update.unrealized_pnl, update.realized_pnl, update.value);
        }

        // Account updates → update_account_value, update_portfolio and
        // account_download_end (via ClientCore)
        if let Some(batch) = self.core.prepare_account_updates(&self.shared) {
            for field in &batch.fields {
                wrapper.update_account_value(&field.key, &field.value, &field.currency, &self.account_id);
            }
            // What the account holds, beside what it is worth. The reference
            // client reports both on this subscription, and a caller watching
            // its positions through it heard only the values.
            for entry in self.core.prepare_portfolio_updates(&self.shared) {
                let contract = self.core
                    .get_contract(entry.con_id, &self.shared)
                    .unwrap_or_else(|| crate::types::model::Contract {
                        con_id: entry.con_id,
                        ..Default::default()
                    });
                wrapper.update_portfolio(
                    &contract, entry.position, entry.market_price, entry.market_value,
                    entry.avg_cost, entry.unrealized_pnl, entry.realized_pnl, &self.account_id,
                );
            }
            if batch.finished {
                wrapper.update_account_time("");
                wrapper.account_download_end(&self.account_id);
            }
        }

        // Account summary → account_summary + account_summary_end (one-shot via
        // ClientCore)
        if let Some(batch) = self.core.prepare_account_summary(&self.shared, &self.account_id) {
            for entry in &batch.entries {
                wrapper.account_summary(batch.req_id, &self.account_id, &entry.tag, &entry.value, &entry.currency);
            }
            wrapper.account_summary_end(batch.req_id);
        }
    }
}

#[cfg(test)]
mod fixed_income_tests {
    use crate::api::wrapper::Wrapper;
    use crate::control::contracts::{ContractDefinition, SecurityType};
    use crate::types::model::ContractDetails;

    /// A lookup for fixed income is answered on its own callback.
    ///
    /// A bond, a bill and the type the venue spells `FIXED` share it, and the
    /// venue answers them with a different set of fields from every other
    /// type's. Answered on the ordinary callback, a program written against
    /// the reference client waited through its own answer.
    #[test]
    fn fixed_income_answers_on_the_callback_written_for_it() {
        #[derive(Default)]
        struct Heard { plain: Vec<String>, fixed: Vec<String> }
        impl Wrapper for Heard {
            fn contract_details(&mut self, _: i64, d: &ContractDetails) {
                self.plain.push(d.contract.symbol.clone());
            }
            fn bond_contract_details(&mut self, _: i64, d: &ContractDetails) {
                self.fixed.push(d.contract.symbol.clone());
            }
        }
        let (client, _rx, shared) = crate::api::client::tests::test_client();
        for (symbol, sec_type) in [
            ("T 4 05/15/30", SecurityType::Bond),
            ("B 0 08/01/26", SecurityType::Bill),
            ("F 3 01/01/28", SecurityType::FixedIncome),
            ("SPY", SecurityType::Stock),
        ] {
            shared.reference.push_contract_details(1, ContractDefinition {
                symbol: symbol.to_string(), sec_type, ..Default::default()
            });
        }
        let mut heard = Heard::default();
        client.dispatch_data(&mut heard);
        assert_eq!(heard.fixed, ["T 4 05/15/30", "B 0 08/01/26", "F 3 01/01/28"]);
        assert_eq!(heard.plain, ["SPY"], "and nothing else moves");
    }
}

#[cfg(test)]
mod delivered_size_tests {
    use crate::api::wrapper::Wrapper;
    use crate::types::model::{TickAttribBidAsk, TickAttribLast};
    use crate::types::{PRICE_SCALE, QTY_SCALE, TbtQuote, TbtTrade};

    /// News and model publications belong to whoever still watches the
    /// contract. An explicit calculation answer keeps its own request id.
    #[test]
    fn news_and_models_are_delivered_only_to_current_watchers() {
        #[derive(Default)]
        struct Heard { news: Vec<i64>, models: Vec<(i64, i32)> }
        impl Wrapper for Heard {
            fn tick_news(&mut self, id: i64, _: i64, _: &str, _: &str, _: &str, _: &str) {
                self.news.push(id);
            }
            fn tick_option_computation(
                &mut self, id: i64, kind: i32, _: i32, _: f64, _: f64, _: f64,
                _: f64, _: f64, _: f64, _: f64, _: f64,
            ) {
                self.models.push((id, kind));
            }
        }
        let (client, _rx, shared) = crate::api::client::tests::test_client();
        client.core.req_to_instrument.lock().unwrap().extend([(1, 0), (2, 0)]);
        client.core.instrument_to_req.lock().unwrap().insert(0, 1);
        client.core.instrument_followers.lock().unwrap().insert(0, vec![2]);
        let publish = || {
            shared.market.push_tick_news(crate::types::TickNews {
                instrument: 0, timestamp: 0, provider_code: "BRFG".into(),
                article_id: "BRFG$1".into(), headline: "SPY headline".into(),
            });
            shared.market.push_option_computation(crate::types::OptionComputation {
                instrument: 0, ..Default::default()
            });
        };
        publish();
        let mut heard = Heard::default();
        client.dispatch_data(&mut heard);
        assert_eq!(heard.news, [1, 2]);
        assert_eq!(heard.models, [(1, 13), (2, 13)]);

        // On a delayed feed the reference client numbers the model apart, and
        // a program that asked for delayed data reads it there.
        client.core.mark_feed_delayed_for_test(0);
        publish();
        let mut delayed_heard = Heard::default();
        client.dispatch_data(&mut delayed_heard);
        assert_eq!(delayed_heard.models, [(1, 83), (2, 83)], "the delayed model, not the live one");

        publish();
        client.cancel_mkt_data(1).unwrap();
        client.cancel_mkt_data(2).unwrap();
        shared.market.push_option_computation(crate::types::OptionComputation {
            instrument: 0, answers: Some(7), ..Default::default()
        });
        let mut heard = Heard::default();
        client.dispatch_data(&mut heard);
        assert!(heard.news.is_empty(), "a withdrawn watch was sent news: {:?}", heard.news);
        assert_eq!(heard.models, [(7, 53)], "only the explicitly addressed answer is owed");
    }

    /// What the venue says on its own account reaches the caller under the
    /// venue's own code.
    ///
    /// It belongs to no request, so it is reported against none. The number is
    /// not a stand-in for "something went wrong": the venue sends this message
    /// on its own subtype and every client is told it under this code, so a
    /// caller matching on the number reads nothing where another number is
    /// reported in its place.
    #[test]
    fn a_message_the_venue_sends_on_its_own_account_keeps_the_venues_code() {
        #[derive(Default)]
        struct Heard(Vec<(i64, i64, String)>);
        impl Wrapper for Heard {
            fn error(&mut self, req_id: i64, code: i64, msg: &str, _: &str) {
                self.0.push((req_id, code, msg.to_string()));
            }
        }
        let (client, _rx, shared) = crate::api::client::tests::test_client();
        shared.market.push_venue_error("the venue is going down at 17:00".to_string());
        let mut heard = Heard::default();
        client.dispatch_data(&mut heard);
        assert_eq!(
            heard.0,
            [(-1, 2148, "the venue is going down at 17:00".to_string())],
            "against no request, under the venue's own code",
        );
    }

    #[derive(Default)]
    struct Sizes(Vec<f64>);

    impl Wrapper for Sizes {
        fn tick_by_tick_all_last(
            &mut self, _req_id: i64, _kind: i32, _time: i64, _price: f64, size: f64,
            _attrib: &TickAttribLast, _exchange: &str, _conditions: &str,
        ) {
            self.0.push(size);
        }
        fn tick_by_tick_bid_ask(
            &mut self, _req_id: i64, _time: i64, _bid: f64, _ask: f64,
            bid_size: f64, ask_size: f64, _attrib: &TickAttribBidAsk,
        ) {
            self.0.push(bid_size);
            self.0.push(ask_size);
        }
    }

    /// A size reaches a caller as the number of shares it is.
    ///
    /// Sizes cross the wire scaled by `QTY_SCALE` and are divided on delivery.
    /// Driven through `process_msgs` rather than checked as arithmetic, so this
    /// fails if delivery stops or the scaling is dropped.
    #[test]
    fn a_size_reaches_a_caller_as_itself() {
        let (client, _rx, shared) = crate::api::client::tests::test_client();
        shared.market.push_tbt_trade(TbtTrade {
            instrument: 0, req_id: 1, price: PRICE_SCALE, size: 100 * QTY_SCALE,
            timestamp: 0, exchange: "NYSE".into(), conditions: String::new(),
            past_limit: false, unreported: false,
        });
        shared.market.push_tbt_quote(TbtQuote {
            instrument: 0, req_id: 2, bid: PRICE_SCALE, ask: PRICE_SCALE,
            // The smallest representable size, beside an ordinary one.
            bid_size: 50 * QTY_SCALE, ask_size: 1,
            timestamp: 0, bid_past_low: false, ask_past_high: false,
        });

        let mut sizes = Sizes::default();
        client.process_msgs(&mut sizes);
        assert_eq!(sizes.0, vec![100.0, 50.0, 1e-8]);
    }

    /// A correction can reopen an order after its completion has armed cleanup
    /// and before anyone asks for completed orders again.
    #[test]
    fn a_deferred_completion_leaves_a_reopened_order_working() {
        use crate::engine::{context::Context, hot_loop::ccp::CcpState};

        let (client, _rx, shared) = crate::api::client::tests::test_client();
        let mut ccp = CcpState::new();
        let mut context = Context::new();
        let instrument = context.register_instrument(756733);
        context.set_symbol(instrument, "SPY".into());
        context.insert_order(crate::types::Order::new(
            7, instrument, crate::types::Side::Buy,
            100 * QTY_SCALE, 400 * PRICE_SCALE, b'2', b'0', 0,
        ));
        let report = |status, kind, exec_id, filled| {
            [(11, "7"), (39, status), (150, kind), (17, exec_id),
             (54, "1"), (6008, "756733"), (38, "100"), (14, filled),
             (32, filled), (31, "412.25")]
                .into_iter().map(|(tag, value)| (tag, value.to_string())).collect()
        };
        let mut wrapper = Sizes::default();
        ccp.handle_exec_report(
            &report("2", "F", "fill-1", "100"), b"", &mut context, &shared, &None, "",
        );
        client.process_msgs(&mut wrapper);
        client.req_completed_orders(false, &mut wrapper);
        assert!(client.deferred_evictions.lock().unwrap().contains(&7));

        ccp.handle_exec_report(
            &report("1", "G", "correction-1", "50"), b"", &mut context, &shared, &None, "",
        );
        assert!(!shared.orders.has_pending_fill(7));
        assert!(shared.orders.venue_is_working(7));
        client.process_msgs(&mut wrapper);
        let reopened = shared.orders.get_order_info(7).expect("the working row survives cleanup");
        assert_eq!(reopened.contract.con_id, 756733);
        assert_eq!(reopened.order.order_id, 7);
        assert!(shared.orders.venue_is_working(7));
        assert!(!client.deferred_evictions.lock().unwrap().contains(&7));

        // The venue saying the order is unknown still removes a working row.
        shared.orders.remove_order_info(7);
        assert!(shared.orders.get_order_info(7).is_none());
        shared.orders.push_order_correction(7, reopened);

        ccp.handle_exec_report(
            &report("2", "F", "fill-2", "100"), b"", &mut context, &shared, &None, "",
        );
        client.process_msgs(&mut wrapper);
        client.req_completed_orders(false, &mut wrapper);
        assert!(client.deferred_evictions.lock().unwrap().contains(&7));
        client.process_msgs(&mut wrapper);
        assert!(shared.orders.get_order_info(7).is_none(), "a terminal row is still reclaimed");
    }

    /// What each order callback was told, in the order it was told.
    #[derive(Default)]
    struct Pairs(Vec<(&'static str, i64, String, String)>);

    impl Wrapper for Pairs {
        fn open_order(
            &mut self, order_id: i64, contract: &crate::types::model::Contract,
            order: &crate::types::model::Order, state: &crate::types::model::OrderState,
        ) {
            self.0.push(("open_order", order_id, contract.symbol.clone(), state.status.clone()));
            assert_eq!(order.order_id, order_id, "the order it holds, not a blank one");
        }
        fn order_status(
            &mut self, order_id: i64, status: &str, _filled: f64, _remaining: f64,
            _avg_fill_price: f64, _perm_id: i64, _parent_id: i64,
            _last_fill_price: f64, _client_id: i64, _why_held: &str, _mkt_cap_price: f64,
        ) {
            self.0.push(("order_status", order_id, String::new(), status.into()));
        }
    }

    /// A status change with no fill on the report states the order beside the
    /// status, as the other surface does and as this one's contract says.
    #[test]
    fn a_status_change_states_the_order_beside_it() {
        let (client, _rx, shared) = crate::api::client::tests::test_client();
        client.core.track_order(
            7,
            crate::types::model::Contract {
                con_id: 756733, symbol: "SPY".into(), sec_type: "STK".into(),
                exchange: "SMART".into(), ..Default::default()
            },
            crate::types::model::Order {
                order_id: 7, action: "BUY".into(), total_quantity: 100.0,
                order_type: "LMT".into(), lmt_price: 400.0, ..Default::default()
            },
            0,
        );
        shared.orders.push_order_update(crate::types::OrderUpdate {
            order_id: 7, instrument: 0, status: crate::types::OrderStatus::Submitted,
            filled_qty: 0.0, remaining_qty: 100.0, avg_price: 0,
            perm_id: 0, parent_id: 0, timestamp_ns: 0,
        });

        let mut pairs = Pairs::default();
        client.process_msgs(&mut pairs);
        assert_eq!(
            pairs.0,
            vec![
                ("open_order", 7, "SPY".to_string(), "Submitted".to_string()),
                ("order_status", 7, String::new(), "Submitted".to_string()),
            ],
        );
    }

    /// And a preview is not stated that way, because a preview is answered by
    /// what the venue says it would cost.
    ///
    /// Asking what an order would do places one, so a preview is a tracked
    /// order like any other and its statuses arrive on this path. The question
    /// waits on the first statement carrying its number, so a pair sent from
    /// the status alone answered it — with no margin figures and a cost of
    /// nought rather than the number that means unstated, on the one call whose
    /// whole purpose is to say what an order would cost.
    #[test]
    fn a_preview_is_not_answered_by_the_status_beside_it() {
        let (client, _rx, shared) = crate::api::client::tests::test_client();
        client.core.track_order(
            8,
            crate::types::model::Contract {
                con_id: 756733, symbol: "SPY".into(), sec_type: "STK".into(),
                exchange: "SMART".into(), ..Default::default()
            },
            crate::types::model::Order {
                order_id: 8, action: "BUY".into(), total_quantity: 100.0,
                order_type: "LMT".into(), lmt_price: 400.0,
                what_if: true, ..Default::default()
            },
            0,
        );
        shared.orders.push_order_update(crate::types::OrderUpdate {
            order_id: 8, instrument: 0, status: crate::types::OrderStatus::PreSubmitted,
            filled_qty: 0.0, remaining_qty: 100.0, avg_price: 0,
            perm_id: 0, parent_id: 0, timestamp_ns: 0,
        });

        let mut pairs = Pairs::default();
        client.process_msgs(&mut pairs);
        assert!(
            pairs.0.iter().all(|(what, ..)| *what != "open_order"),
            "the status answered the preview in place of the venue's own reply: {:?}",
            pairs.0,
        );
    }
}
