//! Order placement, cancellation, execution replay, and algo parsing.

use std::sync::atomic::Ordering;

use crate::error_codes::{DUPLICATE_ORDER_ID, NOT_CANCELLABLE, NO_SUCH_ORDER, ORDER_DOES_NOT_MATCH, Refusal};
use crate::types::model::ExecutionFilter;
use crate::api::wrapper::Wrapper;
use crate::client_core::ClientCore;
use crate::types::*;

use super::{Contract, Order, EClient};

impl EClient {
    // ── Orders ──

    /// Refuse a security type the venue does not permit this account to trade.
    ///
    /// The venue states its permissions at logon, keyed by security type, and it
    /// refuses an order on an unpermitted type by returning it Inactive with no
    /// text at all — so without this check the caller is told nothing. Silence
    /// here is not permission: when the venue stated no permissions, there is
    /// nothing to enforce and the order goes.
    fn check_sec_type_permitted(&self, sec_type: &str) -> Result<(), Refusal> {
        // The validator's own words under its own number, not flattened onto
        // the general one: a caller branches on which refusal it got.
        ClientCore::refuse_unpermitted_sec_type(
            &self.shared.reference.order_permissions(), sec_type,
        )
    }

    /// Security type → the order types the venue permits for it, as stated at
    /// logon. Empty until the session is up.
    pub fn order_permissions(&self) -> std::collections::HashMap<String, Vec<String>> {
        self.shared.reference.order_permissions()
    }

    /// The order types permitted for one security type, or `None` when the type
    /// is not permitted at all. A combination is named `COMB`.
    pub fn permitted_order_types(&self, sec_type: &str) -> Option<Vec<String>> {
        self.shared.reference.permitted_order_types(&sec_type.to_ascii_uppercase())
    }

    /// Feature tokens the venue enables for this account: those stated at
    /// logon, and those the account configuration adds afterwards.
    pub fn enabled_features(&self) -> Vec<String> {
        self.shared.reference.enabled_features()
    }

    /// Which algorithms the venue offers, keyed `PROVIDER/SECTYPE`.
    ///
    /// Stated on the session rather than per contract. An algorithm absent
    /// here is one this account may not use, and an order naming it is
    /// refused by the venue.
    pub fn algorithms(&self) -> std::collections::HashMap<String, Vec<String>> {
        self.shared.reference.algorithms()
    }

    /// The algorithms offered for one security type, across every provider.
    pub fn algorithms_for(&self, sec_type: &str) -> Vec<String> {
        self.shared.reference.algorithms_for(sec_type)
    }

    /// The sets of order defaults this account holds, as
    /// `(key, version, when it last changed)`.
    ///
    /// The venue keeps one per security type and fills parts of an order the
    /// caller left unstated from them: the size a compete order competes with
    /// and the offset it competes by, where neither was named. So the same
    /// call on two accounts is not the same order, and the reference client's
    /// surface has no way to say which sets are in force.
    ///
    /// The key is the venue's own — `s=STK`, or `s=CASH&tc=EUR` where a
    /// currency splits it — and the version is what that set is on. The values
    /// in a set are asked for separately and are not carried here.
    ///
    /// The version says *that* a set changed; the moment says *when*, which is
    /// what tells a caller whether an order it sent at a given time was filled
    /// in from the old defaults or the new. Empty where the venue stated
    /// none.
    pub fn order_presets(&self) -> Vec<(String, String, String)> {
        self.shared.reference.order_presets()
    }

    /// Refuse where the trading connection has stopped being retried.
    ///
    /// Gated on that connection's own state rather than the session's: the
    /// session flag is set by any transport ending, the market-data farm
    /// included, and refusing on it would refuse what a live trading
    /// connection would have carried. Where the trading connection itself has
    /// stopped, anything given here is recorded and buffered and never sent,
    /// and the caller is told it did something the venue never saw.
    fn refuse_if_trading_is_over(&self, what: &str) -> Result<(), Refusal> {
        match self.shared.reference.trading_over() {
            Some(why) => Err(Refusal::not_connected(format!(
                "the trading connection has ended and is not being retried ({why}), so \
                 {what} given here would be recorded and never sent: open a session again",
            ))),
            None => Ok(()),
        }
    }

    /// Place an order. Matches `placeOrder` in C++.
    ///
    /// An order names its contract by the venue's id. A caller who states
    /// a description instead of an id — which every example written against
    /// the reference client does — has it resolved here, once the order itself
    /// is known to be one the venue would take: an order that names no
    /// contract is one the venue has nothing to match, and answers with
    /// nothing at all.
    ///
    /// Resolving it costs a request and an answer the first time, so this call
    /// does not return until the venue has named the contract — up to the
    /// answer timeout. Once per description: the answer is kept, and later
    /// orders on the same contract are sent without asking again. The reference client
    /// never waits here, because a gateway
    /// resolved the contract before the order reached it; this client is the
    /// gateway, so the work happens somewhere, and today it happens on the
    /// caller's thread. A caller placing orders from inside a callback stalls
    /// its own dispatch loop for that time. Pass a contract carrying `con_id`
    /// — from `qualify_contract`, or from any contract-details answer — and
    /// nothing is resolved and nothing waits.
    pub fn place_order(&self, order_id: i64, contract: &Contract, order: &Order) -> Result<(), Refusal> {
        // Gated on the trading connection's own state rather than the
        // session's. The session flag is set by any transport ending, the
        // market-data farm included, and refusing an order on that takes the
        // trading connection down with the quote feed — on a connection that
        // would have carried it. This one is set only where the trading
        // connection itself has stopped being retried, which is where an order
        // accepted here would join a buffer nothing drains and be reported as
        // sent while never reaching the venue.
        self.refuse_if_trading_is_over("an order")?;
        self.core.refuse_if_readonly("an order").map_err(Refusal::validation)?;
        ClientCore::validate_order_destination(&contract.exchange)?;

        // Validate order params and contract before registering instrument (fail fast).
        ClientCore::validate_order(order, &self.account_id)?;
        ClientCore::validate_supported_instructions(order)?;
        ClientCore::validate_combo_legs(&contract.sec_type, contract.combo_legs.len())?;
        for (at, leg) in contract.combo_legs.iter().enumerate() {
            ClientCore::validate_leg(at, leg)?;
        }
        ClientCore::validate_order_contract(
            contract.con_id,
            &contract.sec_type,
            &crate::types::model::contract_identity(
                &contract.last_trade_date_or_contract_month, contract.strike,
                &contract.right, &contract.multiplier, &contract.currency,
            ),
        )?;
        self.check_sec_type_permitted(&contract.sec_type)?;

        let mut named;
        let contract = if contract.con_id == 0 && !contract.symbol.is_empty() {
            let key = ClientCore::description_key(contract);
            named = match self.core.named_for(&key) {
                Some(already) => already,
                None => {
                    // Under the code the lookup failed with. Rewritten to
                    // "no security definition" whatever happened, an order
                    // refused because the session ended reads as an order for
                    // a contract that does not exist, and a caller that
                    // branches on the code retries the description for ever.
                    let answer = self.qualify_contract(contract)?;
                    self.core.remember_named(key, answer.clone());
                    answer
                }
            };
            // What the venue names is a description of one contract, and a
            // description carries no hedge and no legs. Put in place of what
            // the caller stated, a delta-neutral order lost the contract it
            // hedges against and a combination lost every leg, and each went
            // to the venue as something else entirely. The naming supplies
            // what the caller left out; it does not take away what they said.
            named.delta_neutral_contract = contract.delta_neutral_contract.clone();
            if !contract.combo_legs.is_empty() {
                named.combo_legs = contract.combo_legs.clone();
            }
            &named
        } else {
            contract
        };

        // An id at or below zero names no order the venue will hold. One handed
        // out in its place put the order on the market under a number the
        // caller had never seen: every status about it arrived under an id they
        // were not watching, and their own cancel named nothing. Refused, as
        // the other surface refuses it.
        let Some(oid) = u64::try_from(order_id).ok().filter(|id| *id > 0) else {
            return Err(Refusal::validation(format!(
                "place_order: order_id {order_id} is not an order number; \
                 ask for one with next_order_id()",
            )));
        };
        // And within the range this client can carry one in, which is where
        // the reader of the venue's reports stops. Past it, the order goes to
        // the venue and every report about it — the acknowledgement, the
        // fills, the withdrawal — fails to parse back to a number, so the
        // order is live and invisible here. The mark below is spent whatever
        // happens to the placement, so one such call also left the allocator
        // counting from past the end and every later request for a number
        // answered with none, for the rest of the session.
        if oid > crate::bridge::MAX_ORDER_ID {
            return Err(Refusal::validation(format!(
                "place_order: order_id {order_id} is past the highest this client can \
                 carry an order under ({}); ask for one with next_order_id()",
                crate::bridge::MAX_ORDER_ID,
            )));
        }
        // Said once, as the paths that hand out numbers say it: a program
        // numbering its orders and its requests out of one counter has a
        // number here that a request cannot carry.
        crate::bridge::say_if_past_a_request_id(oid);

        // The number this call is spending, so the allocator does not hand it
        // out again. It counts from the highest the venue has named, and the
        // venue has not named this one yet — a program keeping its own numbers
        // off `next_valid_id`, which is the reference client's own idiom, then
        // asked for one and was given a number it had put on the market
        // moments before.
        self.next_order_id.fetch_max(oid + 1, Ordering::AcqRel);

        // Before the slot a tracked order holds is compared with the one this
        // contract is cached under: the engine may have given that slot back.
        self.core.forget_released_slots(&self.shared);
        let replacing = self.core.is_working_at_the_venue(oid, Some(&self.shared));
        // A number the venue has already worked an order under names nothing
        // now, so this placement is not a revision -- and the venue refuses a
        // repeated number only while it is still working one, so after a fill
        // it takes it as a new order. A caller retrying what it believed had
        // failed was given a second live order.
        if !replacing && self.core.the_number_is_spent(oid) {
            return Err(Refusal::stated(
                DUPLICATE_ORDER_ID,
                format!(
                    "order {oid} has already been worked and finished: place a new \
                     order under a number of its own",
                ),
            ));
        }
        // Where a replace names a contract other than the one the order is
        // working on, that is settled before anything is registered: asking
        // for a slot first spent one of the table's on a contract this call
        // then refused, and the table does not grow.
        let placed_on = if replacing { self.core.tracked_instrument(oid) } else { None };
        let wrong_contract = || Refusal::stated(ORDER_DOES_NOT_MATCH, format!(
            "order {oid} is working on another contract, and a replace names \
             the order rather than the contract: withdraw it and place a new \
             order to trade {}",
            contract.symbol,
        ));
        let instrument = match placed_on {
            Some(placed_on) if contract.con_id != 0 => {
                if self.core.cached_instrument(&self.shared, contract.con_id) != Some(placed_on) {
                    return Err(wrong_contract());
                }
                placed_on
            }
            _ => {
                // An order the venue replayed holds no slot here, and the
                // check above has nothing to compare. The venue's own book
                // names the contract it is on, and a replace names the order
                // rather than the contract — so one naming another contract is
                // refused rather than recorded against it, and rather than
                // spending a slot on a contract nothing needs.
                if replacing
                    && let Some(known) = self.shared.orders.get_order_info(oid)
                    && !ClientCore::names_the_same_contract(&known.contract, contract)
                {
                    return Err(wrong_contract());
                }
                self.core.find_or_register_instrument(
                    &self.shared,
                    &self.control_tx,
                    contract.con_id, &contract.symbol, &contract.exchange,
                    &contract.sec_type,
                    &crate::types::model::contract_identity(
                        &contract.last_trade_date_or_contract_month, contract.strike,
                        &contract.right, &contract.multiplier, &contract.currency,
                    ),
                )?
            }
        };

        // If orderId is already tracked, this is a modification — emit Modify instead
        // of Submit.
        let cmd = if replacing {
            // A replace carries the order id and its fields, not the contract, so
            // the order stays on the instrument it was placed on. A contract naming
            // a different instrument is refused rather than recorded.
            if placed_on.is_some_and(|placed_on| placed_on != instrument) {
                return Err(wrong_contract());
            }
            // A replace states the order type, the limit price and the trigger.
            // An order defined by anything else cannot survive one, so refuse
            // rather than send a message that destroys it.
            if let Some(refusal) = self.core.modify_refusal(oid, order, Some(&self.shared)) {
                return Err(refusal);
            }
            // Each read from the field the submit reads it from: a stop's
            // trigger rides on aux_price, and a trailing stop limit's price is
            // its limit offset. Reading only lmt_price left a stop order
            // modifying itself to a limit price of zero.
            let price = ClientCore::replace_price(order);
            let qty = crate::types::qty_from_f64(order.total_quantity);
            let stop_price = ClientCore::replace_trigger(order);
            // Built here, with the replace it belongs to, so that a statement
            // which cannot be built refuses the replace before anything moves,
            // and so that the two travel as one command: the venue merges what
            // a replace states onto the order it is working, and a statement
            // sent on its own arrived out of order beside a second replace of
            // the same order and left each carrying the other's terms.
            let spec = match ClientCore::build_order_request(order, oid, instrument, Some(contract))? {
                ControlCommand::Order(OrderRequest::SubmitEx { kind, attrs, .. }) => {
                    Some(Box::new(crate::types::OrderSpec { kind, attrs }))
                }
                _ => None,
            };
            ControlCommand::Order(OrderRequest::Modify {
                order_id: oid,
                price,
                qty,
                outside_rth: order.outside_rth,
                ord_type: order.ord_type_byte(),
                tif: order.tif_byte(),
                stop_price,
                spec,
            })
        } else {
            ClientCore::build_order_request(order, oid, instrument, Some(contract))?
        };
        self.core.cache_contract(contract.con_id, contract.clone());
        // The record carries the number the order went out under. Cached from
        // the caller's object unchanged, an order placed without one read back
        // from `open_order` naming no order at all.
        let mut placed = order.clone();
        placed.order_id = oid as i64;
        // An order that does not transmit is built and kept, not sent and not
        // refused. One that does sends whatever of its family was kept, in the
        // order it was placed, and then itself.
        //
        // The record of a placement goes down before either. Written
        // afterwards, the engine could acknowledge the order — or refuse and
        // retire it — while there was nothing here to record it against, and
        // the insertion behind that put a fresh PendingSubmit over the venue's
        // own word, or brought back an order the refusal had already taken
        // out. A held placement takes its record in the same step as the hold:
        // the two apart were read between, and a withdrawal that found the
        // hold, took it and found no record reported the order gone while the
        // record was written behind it.
        //
        // A replace restates an order the venue is already working, so it
        // states new terms and not a new order: recorded as one, a partly
        // filled order came back as pending with nothing filled and its whole
        // quantity outstanding, and a refused replace left it reading that
        // way. Its record goes down first for the same reason a placement's
        // does — the venue can answer the replace before this call returns,
        // and a restatement written behind that answer put the attempted
        // terms over a refusal that had already put back the real ones.
        // Every replace goes behind the caller's own statement of the order,
        // which the engine keeps as the record the replace restates its shape
        // from. A replace states the terms it changes and nothing else, so the
        // shape comes from that record — and a record left at what was first
        // placed sent the venue the first display size, the first discretionary
        // amount, the first of everything the caller had since changed, while
        // this client's own answer to "what is working" already read back the
        // new ones. The change never left the process and nothing said so.
        //
        // The statement itself rides on the replace, built above with it. A
        // held replace therefore leaves the hold carrying its own terms, and a
        // replace that never goes leaves nothing behind to be taken for the
        // record of an order.
        if replacing {
            self.core.restate_order(Some(&self.shared), oid, contract.clone(), placed.clone(), instrument);
        }
        if order.transmit {
            if !replacing {
                self.core.track_order(oid, contract.clone(), placed.clone(), instrument);
            }
            let tx = self.control_tx.clone();
            if let Err(why) = self.core.transmit_family(oid, order.parent_id, cmd, |c| {
                tx.send(c).is_ok()
            }) {
                // Nothing left, so nothing was restated.
                if replacing { self.core.undo_restatement(oid); }
                return Err(Refusal::not_connected(why));
            }
        } else if replacing {
            self.core.hold_until_transmitted(oid, order.parent_id, cmd);
        } else {
            self.core.hold_and_track(
                oid, order.parent_id, cmd, contract.clone(), placed.clone(), instrument,
            );
        }
        Ok(())
    }

    /// Exercise or lapse a long option position. Matches `exerciseOptions` in C++.
    ///
    /// `exercise_action` is 1 to exercise and 2 to lapse; anything else is
    /// refused.
    ///
    /// `override_` is taken and not sent, because there is no tag for it: it
    /// names a check made before the order is built, not one the venue makes.
    /// The check it names is a real one — it is what stops an exercise of an
    /// option that is out of the money and a lapse of one that is in it — and
    /// this client does not make it, because what it rests on is the venue's
    /// word on where the option stands, which this client does not ask for.
    /// So an instruction is sent as given, and `override_ = false` buys no
    /// protection here. Passing `true` is the honest description of what
    /// happens either way; passing `false` says so in the log.
    pub fn exercise_options(
        &self, req_id: i64, contract: &Contract, exercise_action: i32,
        exercise_quantity: i32, account: &str, override_: bool,
        stated: crate::client_core::ExerciseStates,
    ) -> Result<(), Refusal> {
        self.refuse_if_trading_is_over("an exercise")?;
        self.core.refuse_if_readonly("an exercise").map_err(Refusal::validation)?;
        if !override_ {
            log::warn!(
                "exercise of {} asked to stop short of an option out of the money, and \
                 this client does not know where the option stands: the instruction is \
                 sent as given",
                contract.symbol,
            );
        }
        let (action, qty) = ClientCore::validate_exercise(
            exercise_action, exercise_quantity, account, &self.account_id,
        )?;
        let identity = crate::types::model::contract_identity(
            &contract.last_trade_date_or_contract_month, contract.strike,
            &contract.right, &contract.multiplier, &contract.currency,
        );
        ClientCore::validate_order_contract(contract.con_id, &contract.sec_type, &identity)?;

        let oid = if req_id > 0 {
            let oid = req_id as u64;
            // An exercise goes to the venue as an order and takes an order's
            // number on the wire, so the number a caller states here is under
            // the same rules a placement's is. Taken as given, a number that
            // is also a working order's overwrote this side's record of that
            // order with the exercise's terms, and the venue refused the
            // exercise as a repeat of a number it was already working — the
            // caller was told the exercise had gone.
            if oid > crate::bridge::MAX_ORDER_ID {
                return Err(Refusal::validation(format!(
                    "exercise_options: {req_id} is past the highest number this client \
                     can carry an order under ({}); pass 0 to be given one",
                    crate::bridge::MAX_ORDER_ID,
                )));
            }
            if self.core.is_working_at_the_venue(oid, Some(&self.shared)) {
                return Err(Refusal::stated(
                    DUPLICATE_ORDER_ID,
                    format!(
                        "{req_id} is the number of an order the venue is working: an \
                         exercise takes an order's number, so pass 0 to be given one",
                    ),
                ));
            }
            // Spent, as a placement spends it: the allocator counts from the
            // highest the venue has named and has not named this one, so
            // without this it hands the same number out again while the venue
            // works the exercise under it.
            self.next_order_id.fetch_max(oid + 1, Ordering::AcqRel);
            oid
        } else {
            // Written down, as on `place_order` above.
            self.reserve_order_ids(1)? as u64
        };
        let instrument = self.core.find_or_register_instrument(
                &self.shared,
            &self.control_tx,
            contract.con_id, &contract.symbol, &contract.exchange, &contract.sec_type,
            &identity,
        )?;
        self.send(ControlCommand::Order(
            ClientCore::build_exercise_request(
                oid, instrument, action, crate::types::qty_from_wire(qty as i64), stated,
            ),
        ))
    }

    /// Cancel an order. Matches `cancelOrder` in C++.
    ///
    /// A cancel names five fields on this wire and no time among them, so a
    /// stated `manual_order_cancel_time` cannot travel. The cancel goes anyway
    /// and the caller is told the record did not: a live order left standing
    /// because a regulatory annotation has nowhere to go is the worse of the
    /// two, and the client this one stands in for withdraws it — it states the
    /// time on every cancel it sends. Taken in silence, as this did, the order
    /// came back under nobody's name while the caller had given one.
    pub fn cancel_order(&self, order_id: i64, manual_order_cancel_time: &str) -> Result<(), Refusal> {
        self.refuse_if_trading_is_over("a withdrawal")?;
        self.core.refuse_if_readonly("a cancel").map_err(Refusal::validation)?;
        // Tag 11 order ids start at 1. A negative id cast unchecked becomes a
        // large unsigned one, which the venue answers "no such order".
        let order_id = u64::try_from(order_id).ok().filter(|id| *id > 0).ok_or_else(|| {
            Refusal::validation(format!(
                "order_id {order_id} is not an order number: they start at one",
            ))
        })?;
        // Said for a withdrawal that happens — after a held order is
        // forgotten, or before the cancel goes — and not for one refused
        // below as spent or unknown, which withdrew nothing.
        let annotate = || {
            if manual_order_cancel_time.is_empty() {
                return;
            }
                // Recorded against the order named, which is known to be one by
                // now: parked against an id that names no order, the note fired
                // as an error on nothing.
                self.shared.orders.push_order_inactive(
                    order_id,
                    Refusal::VALIDATION,
                    format!(
                        "a withdrawal states a time, and this protocol carries no field for \
                         it: the cancel names five and none of them is that one, so the order \
                         is withdrawn without it. State it where the order was placed to have \
                         it recorded. (stated: {manual_order_cancel_time})",
                    ),
                );
        };
        // An order still held never reached the venue, so withdrawing it is
        // forgetting a command rather than sending one. Sent, the venue
        // answers that it knows no such order and the command stays queued to
        // go out behind the next thing that transmits. The record goes with
        // the command: left standing, the id reads as a working order's, and
        // placing under it again becomes a modify of an order nothing has
        // ever submitted.
        // Only where what is held would have placed the order. A revision of
        // an order the venue is already working can also be waiting to be
        // transmitted, and forgetting that and returning left the live order
        // working while the caller had been told it was withdrawn: the staged
        // revision goes, and the cancel still travels.
        if self.core.withdraw_held_placement(order_id) {
            annotate();
            return Ok(());
        }
        // A withdrawal naming an order this client is not working is answered
        // rather than sent under a number the venue never gave out.
        //
        // Read after the replay of the account's working set, which is what
        // makes it safe: an order carried over from a previous session is not
        // known here until that lands, and refusing a withdrawal of a live
        // order is worse than the silence this replaces. One bounded wait per
        // connection, as the global withdrawal beside this already makes.
        //
        // An order this client saw finish is not one it has never heard of:
        // the record is here, and the venue's own answer for it is that it is
        // no longer cancellable.
        if self.core.the_number_is_spent(order_id) {
            return Err(Refusal::stated(
                NOT_CANCELLABLE,
                format!(
                    "Cancel attempted when order is not in a cancellable state. Order permId = {}",
                    self.shared.orders.get_order_info(order_id).map_or(0, |info| info.order.perm_id),
                ),
            ));
        }
        // Bound, and read: where the wait gave up nothing is known either way,
        // and a withdrawal sent for the venue to answer is the safe side.
        let named = self.shared.orders.wait_for_replay();
        if named && !self.core.a_withdrawal_names_something(order_id, &self.shared) {
            return Err(Refusal::stated(
                NO_SUCH_ORDER,
                format!("no order is working under {order_id}"),
            ));
        }
        annotate();
        self.send(ControlCommand::Order(OrderRequest::Cancel { order_id }))
    }

    /// Cancel an order identified by `permId` — stable across sessions.
    ///
    /// `permId` is the broker-assigned identifier returned in `order_status`
    /// callbacks and surfaced in account tools. Useful for cancelling an order
    /// placed in a prior session, where the local `order_id` is not retained.
    ///
    /// the CCP cancel frame is orderId-only, so ibx looks up
    /// the local `order_id` from `permId` in the open-order cache (populated by
    /// `place_order` callbacks or by the CCP session-recovery push hydrated in
    /// `handle_exec_report`). Fails if `perm_id` is not currently tracked.
    pub fn cancel_order_by_perm_id(&self, perm_id: i64) -> Result<(), Refusal> {
        self.refuse_if_trading_is_over("a withdrawal")?;
        self.core.refuse_if_readonly("a cancel").map_err(Refusal::validation)?;
        if perm_id == 0 {
            return Err("cancel_order_by_perm_id: perm_id must be non-zero".into());
        }
        // Read after the venue has named the working set, as the withdrawal
        // by number reads: the order this exists for is one carried over from
        // a previous session, and that is the order absent until the naming
        // lands.
        let _ = self.shared.orders.wait_for_replay();
        let order_id = self.core.collect_open_orders(&self.shared)
            .into_iter()
            .find(|(_, tracked)| tracked.order.perm_id == perm_id)
            .map(|(oid, _)| oid)
            .ok_or_else(|| Refusal::stated(
                NO_SUCH_ORDER,
                format!("cancel_order_by_perm_id: permId {perm_id} not found in open orders"),
            ))?;
        self.cancel_order(order_id as i64, "")
    }

    /// Cancel every order the account is working. Matches `reqGlobalCancel`
    /// in C++.
    ///
    /// This wire carries no request to withdraw everything, so it is composed
    /// here: one cancel for each order held, which is what a caller asking for
    /// everything back is asking for. What is held is what the venue named as working at connect and
    /// what this session placed since. The venue names the former after the
    /// connect returns, so a global cancel issued straight away waits for that
    /// naming, as asking for the open orders does, and covers what was named.
    /// Where the naming does not finish within the wait, what had been named
    /// is still withdrawn and the call says so rather than returning as
    /// though every order were covered: a partial cancel that reads as one
    /// beats the same cancel in silence, which reads as a complete answer.
    /// The same where the naming did finish and an order it named could not be
    /// given a slot in this client's instrument table — the engine holds no
    /// record of such an order, so no cancel here names it and it goes on
    /// working at the venue.
    pub fn req_global_cancel(&self) -> Result<(), Refusal> {
        self.refuse_if_trading_is_over("a withdrawal of every order")?;
        self.core.refuse_if_readonly("a global cancel").map_err(Refusal::validation)?;
        // Everything held goes with everything working: an order the venue was
        // never given is withdrawn by forgetting it, and left queued it would
        // go out behind the next thing that transmits — after the caller had
        // asked for every order to be taken back.
        self.core.withdraw_all_held();
        let named = self.shared.orders.wait_for_replay();
        // One request per instrument the engine holds an order on. The count
        // is the engine's, mirrored: a contract the venue named an order on
        // counts whether or not this session ever subscribed to it.
        let count = self.shared.market.instrument_count();
        // Every failed send is counted and reported, as on the other surface:
        // stopped at the first, the rest were never attempted and the caller
        // was not told how many went.
        let mut unsent = 0usize;
        for instrument in 0..count {
            if self.send(ControlCommand::Order(OrderRequest::CancelAll { instrument })).is_err() {
                unsent += 1;
            }
        }
        if unsent > 0 {
            return Err(Refusal::no_answer(format!(
                "a global cancel reached the engine for {} of {count} instruments; \
                 the rest were not sent, so orders on them are still working",
                count as usize - unsent,
            )));
        }
        // An order the venue named that could not be given a slot in this
        // client's instrument table is in none of those requests: the engine
        // holds no record of it to compose a cancel from, and it is still
        // working there. Said before the naming is judged, because this is an
        // omission that happened rather than one that may have.
        let unheld = self.shared.orders.orders_without_a_slot();
        if unheld > 0 {
            return Err(Refusal::no_answer(format!(
                "{unheld} of this account's working orders have no slot in this client's \
                 instrument table, so no cancel was composed for them and they are still \
                 working; the {count} that were sent cover the rest",
            )));
        }
        // Only where the venue had begun naming and not finished. An account
        // working nothing is named with nothing, and the record that ends the
        // naming cannot be told from the one that precedes it, so an empty
        // account never sees it finish — warning there would cry wolf on every
        // withdrawal against an idle account.
        if !named && self.shared.orders.naming_began() {
            // Said to the caller rather than the log: what had been named
            // went, and what had not been named is not covered. A silent
            // partial cancel is the worst thing this call can do.
            return Err(Refusal::no_answer(format!(
                "the venue had not finished naming this account's working orders within \
                 the wait: {count} cancels were sent for what had been named, and what had \
                 not been named is not covered and may still be working",
            )));
        }
        Ok(())
    }

    /// Request next valid order ID. Matches `reqIds` in C++.
    pub fn req_ids(&self, wrapper: &mut impl Wrapper) {
        // Stated without being taken, as the reference client states it: the
        // caller places under it, and the reservation happens then.
        // Held under the end of the range the venue's reports can name back,
        // as the reservation is: an id past it names an order this client
        // could never reconcile against the answers to it.
        let stated = self.next_order_id.load(Ordering::Acquire)
            .max(self.next_id_base())
            .min(crate::bridge::MAX_ORDER_ID);
        crate::bridge::say_if_past_a_request_id(stated);
        wrapper.next_valid_id(stated as i64);
    }

    /// One past the highest id the account is working an order under.
    ///
    /// The venue refuses an order that names an id it is still working, and
    /// takes one whose order has been withdrawn or filled: an id is spent only
    /// while its order is live. So what an id has to clear is the working set,
    /// which the venue names unprompted at every connect — from every session,
    /// not just this one — and not every id the account has ever used.
    ///
    /// Read on each reservation rather than settled once, so an order the
    /// venue names later still raises the floor.
    ///
    /// The naming is waited for, as the other surface waits for it. The venue
    /// names what it is working after the connect returns, and an id asked for
    /// in the same breath was floored at nothing: a caller that connected and
    /// asked at once was handed one, and the venue refused the order under it
    /// as a duplicate of one it is still working. An account with nothing
    /// working is named with nothing and the wait is over as soon as the venue
    /// says so, so counting from one costs it nothing.
    fn next_id_base(&self) -> u64 {
        if !self.shared.orders.wait_for_replay() && self.shared.orders.naming_began() {
            // No error travels with an id, so this is said where it can be
            // said. The floor is whatever had been named by now, which is not
            // the whole of what the account is working.
            log::warn!(
                "the venue had not finished naming this account's working orders within \
                 the wait, so the next order id is counted from what it had named and the \
                 venue may refuse an order under it as one it is already working",
            );
        }
        self.shared.orders.working_id_watermark().saturating_add(1)
    }

    /// Get the next order ID (local counter).
    ///
    /// Zero where there is no id to give, which every placement path refuses.
    /// A number carries no reason with it, so the reason goes out on the
    /// channel a caller already watches as well as to the log: told only in
    /// the log, a caller reads a zero and has nowhere to learn why.
    pub fn next_order_id(&self) -> i64 {
        self.reserve_order_ids(1).unwrap_or_else(|why| {
            log::error!("{}", why.message);
            self.shared.reference.push_historical_error(
                crate::bridge::ReferenceState::NO_REQUEST,
                Refusal::VALIDATION,
                why.message.clone(),
            );
            0
        })
    }

    /// Take `n` consecutive ids in one step.
    ///
    /// A bracket occupies three consecutive ids: parent, parent+1, parent+2.
    /// Reserving them in one step keeps a concurrent placement from taking a
    /// child's id or moving the counter back over ids already handed out.
    ///
    /// The whole run has to fit under [`crate::bridge::MAX_ORDER_ID`],
    /// children included. Handed out unchecked, the run crossed the end of the
    /// range the venue's reports can name back: the id itself could never be
    /// reconciled, a bracket's children were an addition past the end of the
    /// signed range, and the counter left behind gave every caller after it a
    /// negative id — which the paths that carry one unsigned turned into an
    /// order number above nine quintillion.
    fn reserve_order_ids(&self, n: u64) -> Result<i64, Refusal> {
        let floor = self.next_id_base();
        let mut held = self.next_order_id.load(Ordering::Acquire);
        loop {
            let first = held.max(floor);
            let last = first
                .checked_add(n - 1)
                .filter(|last| *last <= crate::bridge::MAX_ORDER_ID)
                .ok_or_else(|| Refusal::validation(format!(
                    "this account has no run of {n} order ids left: the ids in use reach \
                     {first}, and an order above {} cannot be named back by the venue's \
                     own reports",
                    crate::bridge::MAX_ORDER_ID,
                )))?;
            match self.next_order_id.compare_exchange_weak(
                held, last + 1, Ordering::AcqRel, Ordering::Acquire,
            ) {
                Ok(_) => {
                    // Said where the ids are handed out rather than only where
                    // one is stated, so a caller that never asks what the next
                    // one is still hears it — and against the widest of the
                    // run, because a bracket's children are the first plus one
                    // and two, and the run can cross the line between them.
                    crate::bridge::say_if_past_a_request_id(last);
                    return Ok(first as i64);
                }
                Err(seen) => held = seen,
            }
        }
    }

    // ── Open Orders ──

    /// Request open orders for this client. Matches `reqOpenOrders` in C++.
    ///
    /// Answers with every order working on the account, as
    /// [`req_all_open_orders`](EClient::req_all_open_orders) does. The protocol
    /// carries no client number on an order, so this session cannot tell which
    /// orders it placed; reporting fewer would omit working orders.
    pub fn req_open_orders(&self, wrapper: &mut impl Wrapper) {
        self.req_all_open_orders(wrapper);
    }

    /// Request all open orders. Matches `reqAllOpenOrders` in C++.
    pub fn req_all_open_orders(&self, wrapper: &mut impl Wrapper) {
        if self.session_over() { return wrapper.error(-1, Refusal::NOT_CONNECTED as i64, "Not connected", ""); }
        // The orders already working are named by the server unprompted after a
        // connect, and answering before that lands reports none of them. A
        // strategy asking what it already has on, at the moment it starts, is
        // exactly who asks this first, and telling it "nothing" is how the same
        // order gets placed twice.
        // Only where the venue had begun naming and not finished. An account
        // working nothing is named with nothing, and the record that ends the
        // naming cannot be told from the one that precedes it, so an empty
        // account never sees it finish — reporting there would cry wolf on
        // every reading of an idle account.
        if !self.shared.orders.wait_for_replay() && self.shared.orders.naming_began() {
            // Said to the caller rather than only to the log, and said ahead of
            // the orders: what follows is what had arrived, which is otherwise
            // indistinguishable from an account with nothing working, and a
            // caller reading it as the whole set places what it already has on.
            wrapper.error(
                super::dispatch::NO_REQUEST,
                Refusal::NO_ANSWER as i64,
                "the venue had not finished naming this account's working orders within \
                 the wait, so what follows is what had arrived rather than what is working",
                "",
            );
        }
        // And where an order the venue named could not be given a slot in the
        // instrument table. It is listed below, from the order cache, and the
        // engine holds no record of it: no fill on it is booked, no status
        // change on it is announced, and a withdrawal of everything does not
        // reach it. Listed without that said, it reads as an order this
        // session is following.
        let unheld = self.shared.orders.orders_without_a_slot();
        if unheld > 0 {
            wrapper.error(
                super::dispatch::NO_REQUEST,
                Refusal::NO_ANSWER as i64,
                &format!(
                    "{unheld} of the orders below have no slot in this client's instrument \
                     table and are absent from the engine's book: their fills are not \
                     booked, their status changes are not announced, and a withdrawal of \
                     every order does not reach them",
                ),
                "",
            );
        }
        for (order_id, tracked) in self.core.collect_open_orders(&self.shared) {
            let state = crate::types::model::OrderState {
                status: tracked.status,
                ..Default::default()
            };
            wrapper.open_order(order_id as i64, &tracked.contract, &tracked.order, &state);
        }
        wrapper.open_order_end();
    }

    // ── Completed Orders ──

    /// Request completed orders. Matches `reqCompletedOrders` in C++.
    ///
    /// Immediately delivers every completed order this session archived, then
    /// calls `completed_orders_end`.
    ///
    /// `api_only` asks for the orders entered through an API rather than by
    /// hand. The venue states no origin beside a finished order, and it does
    /// number the ones an API placed: an order that went out through one
    /// carries the number that API gave it, and one typed in carries none. So
    /// `true` is answered with the orders the venue numbered.
    pub fn req_completed_orders(&self, api_only: bool, wrapper: &mut impl Wrapper) {
        if self.session_over() { return wrapper.error(-1, Refusal::NOT_CONNECTED as i64, "Not connected", ""); }
        // One at a time. The answer is a run of ordinary reports and one
        // sentinel, and nothing in the run says which question it answers, so
        // two callers waiting at once both take the first answer as their own.
        // The client this replaces refuses the second outright; this one says
        // so and ends, rather than handing over somebody else's answer or
        // leaving the caller waiting on a sentinel that has already been spent.
        let Some(mut held) = self.shared.claim_the_completed_orders_question() else {
            wrapper.error(
                -1, Refusal::NO_ANSWER as i64,
                "another request for what the account has finished is already waiting; \
                 this one was not sent",
                "",
            );
            return wrapper.completed_orders_end();
        };
        // Asked of the venue, not only of this session. What finished while
        // this program was watching is a fraction of what the account has
        // done. The answer is a run of ordinary reports ending in a sentinel,
        // so the wait is on that end rather than on a clock.
        // The question goes out under the turn this caller holds it on, and
        // the end that answers it comes back under the same turn. Waited on a
        // count instead, a caller that gave up left its answer on its way and
        // the next caller read it as the answer to its own question.
        if self.control_tx
            .send(crate::types::ControlCommand::FetchCompletedOrders { turn: held.turn })
            .is_ok()
        {
            let until = std::time::Instant::now()
                + std::time::Duration::from_secs(crate::config::ANSWER_TIMEOUT_SECS);
            // Two things in turn, and in that order. First the engine takes
            // this question off the queue; only then is the count of answers
            // read again, because one hot-loop pass can take a sentinel off
            // the socket and this question off the queue in that order — so an
            // answer that completed before the engine had even seen this
            // question satisfied it, and the caller returned before its own
            // request had reached the venue.
            // The end of this caller's own question is what releases it. The
            // turn travels out with the question and comes back on the end, so
            // an answer is matched to the question that asked for it — counted
            // instead, a caller that gave up left its answer on its way and
            // the next caller took it as its own, and an answer that arrived
            // before the engine had seen the question satisfied nobody.
            loop {
                if self.shared.orders.completed_orders_ended_on() >= held.turn {
                    break;
                }
                if std::time::Instant::now() >= until {
                    // Said, not only logged: what follows is what had arrived,
                    // which is not the venue's account of what the account has
                    // finished. Reported as an ordinary answer, an empty one
                    // reads as an account that has finished nothing.
                    log::warn!(
                        "the venue did not finish stating what it has finished; answering with \
                         what arrived",
                    );
                    wrapper.error(
                        -1, Refusal::NO_ANSWER as i64,
                        "the venue did not finish stating what the account has finished; \
                         what follows is what had arrived",
                        "",
                    );
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        // Drained once and retained. The queue empties on read and the venue does
        // not resend completed orders, so later calls answer from this archive.
        //
        // Taken while this caller still holds the question, so what the venue
        // stated for it lands in this caller's archive. Released first, the
        // next caller could ask, and whichever of the two drained first took
        // both answers.
        {
            let mut archive = self.completed.lock().unwrap();
            // What the venue has taken back goes first. A trade cancel or
            // correction returns a finished order to a working quantity, and
            // the bridge can only drop the completion it still holds — this is
            // the copy it cannot reach. Applied before the arrivals below, an
            // order taken back and then finished again keeps the new record
            // and loses the superseded one.
            for order_id in self.shared.orders.drain_order_corrections() {
                archive.retain(|(_, order, _)| order.order_id != order_id as i64);
                // And the eviction armed for it when it finished. A bust or a
                // correction puts the order back to a working quantity, and an
                // eviction still standing took its record away on the next pass —
                // after which the order reads as one the venue is not working.
                self.deferred_evictions.lock().unwrap().remove(&order_id);
            }
            for order in self.shared.orders.drain_completed_orders() {
                let status_str = crate::types::order_status::order_status_str(order.status);
                let entry = if let Some(info) = self.shared.orders.get_order_info(order.order_id) {
                    let mut state = info.order_state;
                    state.status = status_str.into();
                    // The contract as the venue was told it where the order
                    // was placed here — the record carries the legs and the
                    // hedge the caller stated, which no definition of one
                    // contract carries — and the venue's own, enriched from
                    // the definition cache, where it was not.
                    let placed_on = self.core.open_orders.lock().unwrap()
                        .get(&order.order_id).map(|placed| placed.contract.clone());
                    let contract = match placed_on {
                        Some(contract) => contract,
                        None if info.contract.con_id != 0 => self.core
                            .get_contract(info.contract.con_id, &self.shared)
                            .unwrap_or(info.contract),
                        None => info.contract,
                    };
                    // The client that placed it, where the venue names none:
                    // the venue states no client on this wire, and the record
                    // of a placement made here knows whose it was.
                    let mut order = info.order;
                    if order.client_id == 0 {
                        order.client_id = self.core.placing_client(&self.shared, order.order_id as u64);
                    }
                    (contract, order, state)
                } else {
                    (
                        Contract::default(),
                        Order { order_id: order.order_id as i64, ..Default::default() },
                        crate::types::model::OrderState {
                            status: status_str.into(),
                            ..Default::default()
                        },
                    )
                };
                // Replaced where this order is already in the archive, not
                // added beside it. The venue restates an order it has already
                // stated once the memory of it has aged out, and pushed again
                // the caller was handed the same order twice — and the archive
                // grew by one row every time it happened.
                // The caller's own number for it decides, as it does in the
                // queue this was read off. The venue names an order
                // permanently at some point in its life and not from the
                // first report, so an answer released before it had and the
                // one after carry the same order under a name only the second
                // one states — matched on both, the second was filed beside
                // the first instead of over it.
                match archive.iter().position(|(_, held, _)| held.order_id == entry.1.order_id
                    && (held.perm_id == entry.1.perm_id
                        || held.perm_id == 0
                        || entry.1.perm_id == 0))
                {
                    Some(at) => archive[at] = entry,
                    None => archive.push(entry),
                }
                // Bound `order_cache` growth: terminal entries are no longer
                // needed once what they carried has been read out of them.
                // Handed to the side that reads the fills rather than freed
                // here. That side is the only one that can tell when a record
                // is finished with: a fill taken off the queue but not yet
                // reported still needs it, and from here looks like no fill at
                // all. Freed here, the report it belonged to arrived with no
                // contract and no execution id — which is also the id its
                // commission is reported under.
                self.deferred_evictions.lock().unwrap().insert(order.order_id);
            }
        }
        // Copied before anything is called back: a callback may ask for these
        // again, and the lock is not re-entrant.
        //
        // And copied before the question is given back: released first, the
        // next caller could ask, be answered, and archive its answer while
        // this caller was still to read the archive — so this caller
        // published orders that were not in the answer to its own question.
        let completed = self.completed.lock().unwrap().clone();
        // The answer is this caller's now, so the question is free for the
        // next one — before the callbacks, which may take a while and may ask
        // for other things while they run. Every path that does not reach
        // here gives it back when the claim is dropped.
        held.give_it_back();
        for (contract, order, state) in &completed {
            // Kept whole in the archive and filtered on the way out, so the
            // same session can ask for all of them and for the numbered ones
            // and be answered correctly either way.
            if api_only && !self.shared.orders.was_entered_through_an_api(
                order.order_id.max(0) as u64, order.perm_id.max(0) as u64,
            ) {
                continue;
            }
            wrapper.completed_order(contract, order, state);
        }
        wrapper.completed_orders_end();
    }

    // ── Executions ──

    /// Automatically bind future orders to this client. Matches `reqAutoOpenOrders` in
    /// C++.
    /// Bind orders entered elsewhere to this client.
    ///
    /// Nothing goes to the venue; this is answered locally, setting
    /// a property of its own and refusing it for any client but the one those
    /// orders bind to. What that property gates does not arise here — this
    /// session is told about every order on the account, whether it placed
    /// them or not — and this surface names no client, so there is nothing to
    /// refuse and nothing left to do.
    ///
    /// [`Wrapper::order_bound`] is never fired here, and not because of this
    /// call: it follows asking for the open orders, not asking to bind them.
    /// The reference architecture partitions an account's orders by the client
    /// that placed them, and on being asked for the open ones it claims those
    /// that belong to no client for client nought — a control message to the
    /// venue, whose answer is what that callback carries. There is no such
    /// partition here: every session is told about every order on the account,
    /// so there is nothing to claim, and claiming it would change who owns an
    /// order at the venue to no end. The permanent id the callback pairs with
    /// arrives on the order's status and on its fills.
    ///
    /// `b_auto_bind` is taken and not applied. Whether it asks to bind or to
    /// stop binding, the answer is the same: this session hears about every
    /// order on the account either way.
    pub fn req_auto_open_orders(&self, _b_auto_bind: bool) {}

    /// Request execution reports. Matches `reqExecutions` in C++.
    /// Replays stored executions (optionally filtered), firing `exec_details` +
    /// `commission_and_fees_report` for each, then `exec_details_end`.
    pub fn req_executions(&self, req_id: i64, filter: &ExecutionFilter, wrapper: &mut impl Wrapper) {
        if let Some(why) = self.shared.reference.session_over() {
            return wrapper.error(req_id, Refusal::NOT_CONNECTED as i64, why, "");
        }
        // Snapshot first: a callback may re-enter a path that locks
        // `executions`, and the dispatch thread pushes fills through the same
        // mutex — holding it across user code deadlocks one and stalls the
        // other.
        for se in self.core.snapshot_executions(filter) {
            wrapper.exec_details(req_id, &se.contract, &se.execution);
            // Only where the venue has said what it cost. An execution is
            // stored with its charge deliberately unstated -- and every
            // execution the venue replays at logon is stored that way and
            // never charged -- so reporting it regardless said the fill cost
            // nothing, in no currency, naming no execution. The empty name is
            // the discriminator: a charge with one is refused where it is read
            // off the wire, so an empty one can only mean nobody has said.
            if !se.commission_and_fees.exec_id.is_empty() {
                wrapper.commission_and_fees_report(&se.commission_and_fees);
            }
        }
        wrapper.exec_details_end(req_id);
    }
}

impl EClient {
    /// Send a bracket as the one instruction the engine has for it.
    ///
    /// Three orders under three numbers, linked by the venue: the children are
    /// held until the parent has a position, and whichever fills withdraws the
    /// other. `place_bracket` is the call that states this in a caller's terms;
    /// this is the part that reaches the engine.
    pub(crate) fn submit_bracket(
        &self, contract: &Contract, side: crate::types::Side, quantity: f64,
        entry: f64, take_profit: f64, stop_loss: f64,
    ) -> Result<[i64; 3], Refusal> {
        // Checked here rather than in `place_bracket`: every surface that places a
        // bracket routes through this.
        self.refuse_if_trading_is_over("a bracket")?;
        self.core.refuse_if_readonly("a bracket").map_err(Refusal::validation)?;
        // The checks `place_order` applies. An order on a security type the account
        // is not permitted is returned Inactive with tag 58 empty, so the reason is
        // stated here instead.
        ClientCore::validate_order_destination(&contract.exchange)?;
        ClientCore::validate_order_contract(
            contract.con_id,
            &contract.sec_type,
            &crate::types::model::contract_identity(
                &contract.last_trade_date_or_contract_month, contract.strike,
                &contract.right, &contract.multiplier, &contract.currency,
            ),
        )?;
        self.check_sec_type_permitted(&contract.sec_type)?;
        let instrument = self.core.find_or_register_instrument(
                &self.shared,
            &self.control_tx,
            contract.con_id, &contract.symbol, &contract.exchange, &contract.sec_type,
            &crate::types::model::contract_identity(
                &contract.last_trade_date_or_contract_month, contract.strike,
                &contract.right, &contract.multiplier, &contract.currency,
            ),
        )?;
        // Consecutive, because the venue reads the children's numbers as the
        // parent's plus one and two. Taken apart, a bracket links to whatever
        // happened to be placed in between.
        let parent_id = self.reserve_order_ids(3)?;
        let (tp_id, sl_id) = (parent_id + 1, parent_id + 2);

        // Each leg is recorded as placed here, under its own number, and
        // before it is sent, as a placement is: recorded behind the send, a
        // refusal or a fill arriving in the window found nothing to record
        // against and the insert put a fresh PendingSubmit over the venue's
        // own word. A leg replaced ahead of the venue's acknowledgement was
        // otherwise a fresh placement under its number, which overwrote the
        // engine's record of the leg with one carrying no parent and no
        // group, and the next replace restated the leg detached from its
        // bracket.
        let (action, exit_action) = match side {
            crate::types::Side::Buy => ("BUY", "SELL"),
            crate::types::Side::Sell => ("SELL", "BUY"),
            crate::types::Side::ShortSell => ("SSHORT", "BUY"),
        };
        let oca_group = format!("OCA_{parent_id}");
        // Each record as the wire states the leg: the entry lives a day and
        // stands alone; each exit is good till cancelled, in the group, and
        // reduces the other on a fill. Recorded otherwise, the two open-order
        // reads described the exits as day orders for their whole life.
        let leg = |order_id: i64, action: &str, order_type: &str, lmt_price: f64, aux_price: f64, parent: i64| {
            let exit = parent != 0;
            crate::types::model::Order {
                order_id, action: action.into(), total_quantity: quantity, order_type: order_type.into(),
                lmt_price, aux_price, tif: if exit { "GTC" } else { "DAY" }.into(), parent_id: parent,
                oca_group: if exit { oca_group.clone() } else { String::new() },
                oca_type: if exit { 3 } else { 0 },
                transmit: true, ..Default::default()
            }
        };
        self.core.track_order(parent_id as u64, contract.clone(), leg(parent_id, action, "LMT", entry, 0.0, 0), instrument);
        self.core.track_order(tp_id as u64, contract.clone(), leg(tp_id, exit_action, "LMT", take_profit, 0.0, parent_id), instrument);
        self.core.track_order(sl_id as u64, contract.clone(), leg(sl_id, exit_action, "STP", 0.0, stop_loss, parent_id), instrument);
        let scaled = |price: f64| crate::types::price_from_f64(price);
        // The three records go back where the command did not reach the engine,
        // as `transmit_family` puts back the ones a placement could not send.
        // Kept, the caller held three orders the venue was never given: they
        // were reported as working, a replace of one was built as a change to
        // something the venue does not hold, and nothing ever released the
        // numbers, because the number is only spent by an order that went.
        if let Err(refused) = self.send(ControlCommand::Order(OrderRequest::SubmitBracket {
            con_id: contract.con_id,
            parent_id: parent_id as u64,
            tp_id: tp_id as u64,
            sl_id: sl_id as u64,
            instrument,
            side,
            qty: crate::types::qty_from_f64(quantity),
            entry_price: scaled(entry),
            take_profit: scaled(take_profit),
            stop_loss: scaled(stop_loss),
        })) {
            for id in [parent_id, tp_id, sl_id] {
                self.core.untrack_order(id as u64);
            }
            return Err(refused);
        }
        Ok([parent_id, tp_id, sl_id])
    }
}
