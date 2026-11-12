//! Order placement, cancellation, execution replay, and algo parsing.

use crate::api::types::ExecutionFilter;
use crate::api::wrapper::Wrapper;
use crate::client_core::{ClientCore, ModifyPlan};
use crate::types::*;

use super::{Contract, Order, TagValue, EClient};

impl EClient {
    // ── Orders ──

    /// Place an order. Matches `placeOrder` in C++.
    pub fn place_order(&self, order_id: i64, contract: &Contract, order: &Order) -> Result<(), String> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if self.core.requests_ignored() { return Ok(()); }
        if !ClientCore::ids_fit("place_order", &[order_id, contract.con_id]) { return Ok(()); }
        // The reference's other names for an order type, under ibx's name
        // for every check and for the tracked order (ibx#469).
        let order = &*ClientCore::with_canonical_order_type(order, self.shared.reference.form_10_40());
        // Validate order params and contract before registering instrument (fail fast).
        ClientCore::validate_order(order)?;
        // The refusals of the contract's security type (ibx#666).
        if let Some((code, message)) = ClientCore::order_contract_refusal(contract) {
            self.shared.orders.push_order_error(order_id, code, message);
            return Ok(());
        }
        // Transmit off: the order is held (ibx#509). A what-if with
        // transmit off is refused with 321 below.
        if !order.transmit && !order.what_if {
            return self.hold_order(order_id, contract, order);
        }
        let mut built = Vec::new();
        if order.what_if {
            self.send_order(order_id, contract, order, false, &mut built)?;
            return self.send_built(built);
        }
        // Transmit on: the order goes out with the held orders of its tree,
        // together (ibx#547). An order that fails ends the list; the ones
        // before it go.
        let mut result = Ok(());
        for (id, contract, order, was_held) in self.core.orders_to_transmit_checked(order_id, contract, order, &self.shared, &self.account_id) {
            result = self.send_order(id, &contract, &order, was_held, &mut built);
            if result.is_err() { break; }
        }
        self.send_built(built)?;
        result
    }

    /// Send the commands of one placeOrder: one as it is, several new
    /// orders as a group the engine sends together (ibx#547).
    fn send_built(&self, mut built: Vec<ControlCommand>) -> Result<(), String> {
        if built.len() < 2 {
            return built.pop().map_or(Ok(()), |cmd| self.send(cmd));
        }
        self.send(ClientCore::order_group(built))
    }

    /// An order placed with transmit off (ibx#509): nothing is sent but the
    /// lookup of a contract given without a conId, and nothing is answered.
    /// A working order placed again with transmit off stays as it is
    /// (paper 09/10/2026).
    fn hold_order(&self, oid: i64, contract: &Contract, order: &Order) -> Result<(), String> {
        if self.core.working_order(&self.shared, oid).is_some() {
            return Ok(());
        }
        // The order is read and checked now, as the reference does; one
        // that is refused is not held (ibx#547).
        let (notices, refusal, _) = self.core.order_read_checks(
            oid, contract.con_id, &contract.sec_type, &contract.exchange, &contract.last_trade_date_or_contract_month, order,
            &self.shared, &self.account_id, false);
        let combo = ClientCore::combo_order(contract, order, &self.shared.reference, &self.account_id);
        for (code, message) in notices {
            self.shared.orders.push_order_error(oid, code, message);
        }
        if let Some((code, message)) = refusal.or(combo.err()) {
            self.shared.orders.push_order_error(oid, code, message);
            return Ok(());
        }
        let instrument = if contract.sec_type.eq_ignore_ascii_case("BAG") { 0 } else {
            self.core.order_instrument(
                &self.control_tx, oid, false,
                contract.con_id, &contract.symbol, &contract.exchange, &contract.sec_type, &contract.currency,
                crate::types::SecDefFilters {
                    // The primary exchange goes into the order's lookup (ibx#710).
                    primary_exchange: contract.primary_exchange.clone(), local_symbol: contract.local_symbol.clone(),
                    last_trade_date_or_contract_month: contract.last_trade_date_or_contract_month.clone(),
                    strike: contract.strike, right: contract.right.clone(), multiplier: contract.multiplier.clone(),
                    trading_class: contract.trading_class.clone(), ..Default::default()
                },
            )?
        };
        // Not known as a parent until the engine has its lookups' answers
        // (ibx#718): noted before the engine can answer.
        self.core.note_held_placed(&self.shared, oid);
        self.send(ControlCommand::HoldOrder {
            order_id: oid, instrument,
            qty: (order.total_quantity * crate::types::QTY_SCALE as f64).round() as crate::types::Qty,
            parent_id: order.parent_id, oca_group: order.oca_group.clone(),
        })?;
        self.core.hold_order(oid, contract.clone(), order.clone(), instrument);
        Ok(())
    }

    /// Send one order: a new order, or the change of a working one.
    fn send_order(&self, order_id: i64, contract: &Contract, order: &Order, was_held: bool, built: &mut Vec<ControlCommand>) -> Result<(), String> {
        // The id as given: the reference refuses 0 with 10149 below.
        let oid = order_id;

        // The warnings and the refusal of the order as it is read; the
        // warnings of an order that was held were given when it was placed
        // with transmit off (ibx#547).
        // A contract without a conId is looked up first; what the reference
        // checks once it is known comes after (ibx#665).
        let looked_up = contract.con_id == 0 && !contract.sec_type.eq_ignore_ascii_case("BAG");
        let (notices, refusal, deferred) = self.core.order_read_checks(
            oid, contract.con_id, &contract.sec_type, &contract.exchange, &contract.last_trade_date_or_contract_month, order,
            &self.shared, &self.account_id, looked_up);
        if !was_held {
            for (code, message) in notices {
                self.shared.orders.push_order_error(oid, code, message);
            }
        }
        if let Some((code, message)) = refusal {
            self.shared.orders.push_order_error(oid, code, message);
            return Ok(());
        }
        // A combo (BAG) order, read and checked as the reference reads it
        // (ibx#470).
        let combo = match ClientCore::combo_order(contract, order, &self.shared.reference, &self.account_id) {
            Ok(combo) => combo,
            Err((code, message)) => {
                self.shared.orders.push_order_error(oid, code, message);
                return Ok(());
            }
        };
        // The condition times as the reference sends them (ibx#416); the
        // order is tracked as the caller placed it.
        let sent = self.core.hedge_sent(ClientCore::with_condition_times(order));

        // A smart combo goes out on its currency's smart combo conId.
        let con_id = combo.as_ref().map(|c| c.smart_con_id).filter(|&c| c > 0).unwrap_or(contract.con_id);
        let instrument = self.core.order_instrument(
            &self.control_tx, oid, order.what_if,
            con_id, &contract.symbol, &contract.exchange, &contract.sec_type, &contract.currency,
            crate::types::SecDefFilters {
                    primary_exchange: String::new(), local_symbol: contract.local_symbol.clone(),
                    last_trade_date_or_contract_month: contract.last_trade_date_or_contract_month.clone(),
                    strike: contract.strike, right: contract.right.clone(), multiplier: contract.multiplier.clone(),
                    trading_class: contract.trading_class.clone(), ..Default::default()
                },
        )?;
        self.core.note_currency(&self.control_tx, con_id, &contract.currency);

        // If orderId is already tracked, this is a modification: replace it
        // with the full wanted state (ibx#247). A what-if never modifies:
        // it previews a new order (ibx#462).
        let working = if order.what_if { None } else { self.core.working_order(&self.shared, oid) };
        if working.is_some() {
            let refusal = self.core.tracked_contract(oid)
                .and_then(|placed| ClientCore::combo_modify_refusal(contract, &placed)
                    .or_else(|| ClientCore::algo_destination_modify_refusal(contract, &placed,
                        |e| self.shared.reference.is_algo_provider(e))))
                // A beta hedge's max size below what it filled (ibx#602).
                .or_else(|| self.core.hedge_filled_refusal(oid, order));
            if let Some((code, message)) = refusal {
                self.shared.orders.push_order_error(oid, code, message);
                return Ok(());
            }
        }
        let cmd = if let Some(working) = working {
            match ClientCore::build_modify_request(&sent, oid, &working, self.shared.reference.form_10_40())? {
                ModifyPlan::Send(cmd) => cmd,
                ModifyPlan::Refused { code, message } => {
                    // Refused before sending, like the reference: the caller
                    // gets error() and the tracked order keeps its old state.
                    self.shared.orders.push_order_error(oid, code, message);
                    return Ok(());
                }
            }
        } else if let Some(combo) = combo {
            ClientCore::build_combo_order_request(&sent, oid, instrument, combo)?
        } else {
            ClientCore::build_order_request(&sent, oid, instrument)?
        };
        let mut cmd = cmd;
        if !self.core.carry_deferred_checks(oid, &mut cmd, deferred, &self.shared) {
            return Ok(());
        }
        // A rule checked once the contract is known that keeps the order
        // for its cancel (ibx#688).
        self.core.carry_kept_refusal(&mut cmd, order);
        // A scale order's profit taker (ibx#751).
        if !order.what_if {
            self.core.scale_profit_child(&mut cmd, contract, order, oid, instrument, self.shared.orders.lowest_child_id());
        }
        built.push(cmd);
        self.core.cache_contract(contract.con_id, contract.clone());
        if order.what_if {
            self.core.track_what_if(oid, contract.clone(), order.clone());
        } else {
            self.core.track_order(oid, contract.clone(), order.clone(), instrument);
        }
        Ok(())
    }

    /// Cancel an order. Matches `cancelOrder` in C++.
    pub fn cancel_order(&self, order_id: i64, _manual_order_cancel_time: &str) -> Result<(), String> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if self.core.requests_ignored() { return Ok(()); }
        if !ClientCore::ids_fit("cancel_order", &[order_id]) { return Ok(()); }
        self.core.drop_held_order(order_id);
        self.send(ControlCommand::Order(OrderRequest::Cancel {
            order_id,
        }))
    }

    /// Cancel an order identified by `permId` — stable across sessions.
    ///
    /// `permId` is the broker-assigned identifier returned in `order_status`
    /// callbacks and surfaced in account tools. Useful for cancelling an order
    /// placed in a prior session, where the local `order_id` is not retained.
    ///
    /// Per ib-agent#154 the CCP cancel frame is orderId-only, so ibx looks up
    /// the local `order_id` from `permId` in the open-order cache (populated by
    /// `place_order` callbacks or by the CCP session-recovery push hydrated in
    /// `handle_exec_report`). Fails if `perm_id` is not currently tracked.
    pub fn cancel_order_by_perm_id(&self, perm_id: i64) -> Result<(), String> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if self.core.requests_ignored() { return Ok(()); }
        if perm_id == 0 {
            return Err("cancel_order_by_perm_id: perm_id must be non-zero".into());
        }
        let order_id = self.core.collect_open_orders(&self.shared)
            .into_iter()
            .find(|(_, tracked)| tracked.order.perm_id == perm_id)
            .map(|(oid, _)| oid)
            .ok_or_else(|| format!("cancel_order_by_perm_id: permId {} not found in open orders", perm_id))?;
        self.cancel_order(order_id, "")
    }

    /// Cancel all orders. Matches `reqGlobalCancel` in C++: every order of
    /// the account the session knows, those of other clients and of
    /// earlier sessions too, as the reference cancels them.
    pub fn req_global_cancel(&self) -> Result<(), String> {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if self.core.requests_ignored() { return Ok(()); }
        self.core.drop_held_orders();
        self.send(ControlCommand::Order(OrderRequest::GlobalCancel))
    }

    /// Request next valid order ID. Matches `reqIds` in C++: the highest
    /// order id this client used + 1, as the reference computes it per
    /// client id (1 when none), a 32-bit id. The ids of the client's earlier
    /// sessions count as far as the server's replays of the logon show them
    /// (orders with the client's id); right after the connect the answer
    /// waits for the order replay of the logon. Nothing is reserved.
    pub fn req_ids(&self, _wrapper: &mut impl Wrapper) {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if self.core.requests_ignored() { return; }
        // Answered here: after the requests made before it (ibx#561).
        self.shared.command_clock().take_turn();
        ClientCore::wait_order_replay(&self.shared);
        let id = self.core.next_valid_id(&self.shared);
        // Given by `process_msgs` at its place among the answers (ibx#532).
        self.later(move |_, wrapper| wrapper.next_valid_id(id));
    }

    /// The next order id for a new order: the next valid id (see
    /// [`req_ids`](EClient::req_ids)), or above the ids this method gave
    /// before. Each call reserves the id it gives.
    pub fn next_order_id(&self) -> i64 {
        ClientCore::wait_order_replay(&self.shared);
        self.core.take_order_id(&self.shared)
    }

    // ── Open Orders ──

    /// Request open orders for this client. Matches `reqOpenOrders` in C++.
    ///
    /// Before the order replay of the logon has ended, and while the auth
    /// link is lost, the request is answered only after the order replay,
    /// from `process_msgs` (ibx#251).
    pub fn req_open_orders(&self, _wrapper: &mut impl Wrapper) {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if self.core.requests_ignored() { return; }
        // Answered here: after the requests made before it (ibx#561).
        self.shared.command_clock().take_turn();
        if self.core.hold_open_orders(crate::client_core::OpenOrdersRequest::Open, &self.shared) {
            return;
        }
        // Given by `process_msgs` at its place among the answers (ibx#532).
        self.later(|this, wrapper| this.answer_open_orders(wrapper, crate::client_core::OpenOrdersRequest::Open));
    }

    /// Request all open orders. Matches `reqAllOpenOrders` in C++.
    ///
    /// Held like [`req_open_orders`](Self::req_open_orders) until the order
    /// replay (ibx#251).
    pub fn req_all_open_orders(&self, _wrapper: &mut impl Wrapper) {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if self.core.requests_ignored() { return; }
        // Answered here: after the requests made before it (ibx#561).
        self.shared.command_clock().take_turn();
        if self.core.hold_open_orders(crate::client_core::OpenOrdersRequest::All, &self.shared) {
            return;
        }
        // Given by `process_msgs` at its place among the answers (ibx#532).
        self.later(|this, wrapper| this.answer_open_orders(wrapper, crate::client_core::OpenOrdersRequest::All));
    }

    /// The open orders, each with its status, then the end of the list
    /// (the reference sends OPEN_ORDER then ORDER_STATUS for each).
    pub(crate) fn answer_open_orders(&self, wrapper: &mut (impl Wrapper + ?Sized), request: crate::client_core::OpenOrdersRequest) {
        for (order_id, tracked, client_id) in self.core.open_orders_listing(&self.shared, request) {
            let state = crate::api::types::OrderState {
                status: tracked.status.clone(),
                ..Default::default()
            };
            // An order kept pending after a refusal: its status only
            // (ibx#682).
            if tracked.status != "ApiPending" {
                wrapper.open_order(order_id, &tracked.contract, &tracked.order, &state);
            }
            let why_held = self.core.why_held(&tracked.status, &tracked.order.order_type, tracked.order.parent_id);
            wrapper.order_status(
                order_id, &tracked.status, tracked.filled, tracked.remaining, 0.0,
                tracked.order.perm_id, tracked.order.parent_id, tracked.last_fill_price, client_id, &why_held, 0.0,
            );
        }
        wrapper.open_order_end();
    }

    // ── Completed Orders ──

    /// Request completed orders. Matches `reqCompletedOrders` in C++.
    /// Delivers all archived completed orders, then calls
    /// `completed_orders_end`, through `process_msgs` (ibx#532).
    pub fn req_completed_orders(&self, _wrapper: &mut impl Wrapper) {
        self.req_completed_orders_only(false);
    }

    /// reqCompletedOrders with its apiOnly flag (ibx#695): the server's
    /// finished orders of the trading day, as the reference asks them;
    /// given by `process_msgs`.
    pub fn req_completed_orders_only(&self, api_only: bool) {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if self.core.requests_ignored() { return; }
        let _ = self.send(ControlCommand::CompletedOrders { api_only });
    }

    // ── Executions ──

    /// Automatically bind future orders to this client. Matches `reqAutoOpenOrders` in C++.
    pub fn req_auto_open_orders(&self, _b_auto_bind: bool) {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if self.core.requests_ignored() { return; }
        // No-op: single-client engine, all orders are auto-bound.
    }

    /// Request execution reports. Matches `reqExecutions` in C++.
    /// Replays stored executions (optionally filtered), firing `exec_details` +
    /// `commission_and_fees_report` for each, then `exec_details_end`.
    pub fn req_executions(&self, req_id: i64, filter: &ExecutionFilter, _wrapper: &mut impl Wrapper) {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if self.core.requests_ignored() { return; }
        // Answered here: after the requests made before it (ibx#561).
        self.shared.command_clock().take_turn();
        if !crate::client_core::ClientCore::ids_fit("req_executions", &[req_id]) { return; }
        // No lock is held during the callbacks (ibx#265). As the reference:
        // every execution, then the commission reports, then the end.
        let execs = self.core.matching_executions(filter);
        // Given by `process_msgs` at its place among the answers (ibx#532).
        self.later(move |_, wrapper| {
        for se in &execs {
            wrapper.exec_details(req_id, &se.contract, &se.execution);
        }
        for report in execs.iter().filter_map(|se| se.commission_and_fees.as_ref()) {
            wrapper.commission_and_fees_report(report);
        }
        wrapper.exec_details_end(req_id);
        });
    }
}

/// Parse algo strategy and TagValue params into internal AlgoParams.
pub fn parse_algo_params(strategy: &str, params: &[TagValue]) -> Result<AlgoParams, String> {
    let get = |key: &str| -> String {
        params.iter()
            .find(|tv| tv.tag == key)
            .map(|tv| tv.value.clone())
            .unwrap_or_default()
    };
    let get_f64 = |key: &str| -> f64 { get(key).parse().unwrap_or(0.0) };
    let get_bool = |key: &str| -> bool {
        let v = get(key);
        v == "1" || v.eq_ignore_ascii_case("true")
    };

    match strategy.to_lowercase().as_str() {
        "vwap" => Ok(AlgoParams::Vwap {
            max_pct_vol: get_f64("maxPctVol"),
            no_take_liq: get_bool("noTakeLiq"),
            allow_past_end_time: get_bool("allowPastEndTime"),
            start_time: get("startTime"),
            end_time: get("endTime"),
        }),
        "twap" => Ok(AlgoParams::Twap {
            allow_past_end_time: get_bool("allowPastEndTime"),
            start_time: get("startTime"),
            end_time: get("endTime"),
        }),
        "arrivalpx" | "arrival_price" => {
            let risk = match get("riskAversion").to_lowercase().as_str() {
                "get_done" | "getdone" => RiskAversion::GetDone,
                "aggressive" => RiskAversion::Aggressive,
                "passive" => RiskAversion::Passive,
                _ => RiskAversion::Neutral,
            };
            Ok(AlgoParams::ArrivalPx {
                max_pct_vol: get_f64("maxPctVol"),
                risk_aversion: risk,
                allow_past_end_time: get_bool("allowPastEndTime"),
                force_completion: get_bool("forceCompletion"),
                start_time: get("startTime"),
                end_time: get("endTime"),
            })
        }
        "closepx" | "close_price" => {
            let risk = match get("riskAversion").to_lowercase().as_str() {
                "get_done" | "getdone" => RiskAversion::GetDone,
                "aggressive" => RiskAversion::Aggressive,
                "passive" => RiskAversion::Passive,
                _ => RiskAversion::Neutral,
            };
            Ok(AlgoParams::ClosePx {
                max_pct_vol: get_f64("maxPctVol"),
                risk_aversion: risk,
                force_completion: get_bool("forceCompletion"),
                start_time: get("startTime"),
            })
        }
        "darkice" | "dark_ice" => Ok(AlgoParams::DarkIce {
            allow_past_end_time: get_bool("allowPastEndTime"),
            display_size: get("displaySize").parse().unwrap_or(100),
            start_time: get("startTime"),
            end_time: get("endTime"),
        }),
        "pctvol" | "pct_vol" => Ok(AlgoParams::PctVol {
            pct_vol: get_f64("pctVol"),
            no_take_liq: get_bool("noTakeLiq"),
            start_time: get("startTime"),
            end_time: get("endTime"),
        }),
        // Another strategy goes out as given, as the reference sends it
        // (ibx#622, AccuDistr accepted on paper 12/10/2026); the
        // definitions check refused it before if the server does not
        // know it (439) or one of its parameters (443).
        _ => {
            let mut pairs: Vec<(String, String)> = params.iter()
                .filter(|tv| !tv.value.is_empty())
                .map(|tv| (tv.tag.clone(), tv.value.clone()))
                .collect();
            crate::types::reference_param_order(&mut pairs);
            Ok(AlgoParams::Other { strategy: strategy.to_string(), params: pairs })
        }
    }
}
