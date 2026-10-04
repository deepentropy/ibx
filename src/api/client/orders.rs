//! Order placement, cancellation, execution replay, and algo parsing.

use std::sync::atomic::Ordering;

use crate::api::types::ExecutionFilter;
use crate::api::wrapper::Wrapper;
use crate::client_core::{ClientCore, ModifyPlan};
use crate::types::*;

use super::{Contract, Order, TagValue, EClient};

impl EClient {
    // ── Orders ──

    /// Place an order. Matches `placeOrder` in C++.
    pub fn place_order(&self, order_id: i64, contract: &Contract, order: &Order) -> Result<(), String> {
        // The reference's other names for an order type, under ibx's name
        // for every check and for the tracked order (ibx#469).
        let order = &*ClientCore::with_canonical_order_type(order);
        // Validate order params and contract before registering instrument (fail fast).
        ClientCore::validate_order(order)?;
        ClientCore::validate_order_contract(&contract.sec_type)?;

        // The id as given: the reference refuses 0 with 10149 below.
        let oid = order_id;

        // Warnings the reference sends while it reads the order (ibx#416).
        for (code, message) in ClientCore::implied_zone_warnings(order) {
            self.shared.orders.push_order_error(oid, code, message);
        }
        // Refused before sending, like the reference: error() only.
        let contract_zone = self.shared.reference.time_zone_id(contract.con_id);
        if let Some((code, message)) = ClientCore::refusal_before_sending(order, &contract.exchange)
            .or_else(|| ClientCore::algo_definition_refusal(order, &contract.exchange, &self.shared.reference))
            .or_else(|| ClientCore::account_config_refusal(
                order, self.shared.reference.account_features().as_deref(), &self.account_id))
            .or_else(|| ClientCore::good_till_date_refusal(order, contract_zone.as_deref()))
            .or_else(|| ClientCore::condition_time_zone_refusal(order, contract_zone.as_deref()))
            .or_else(|| ClientCore::price_refusal(order))
            .or_else(|| ClientCore::order_id_refusal(oid))
            .or_else(|| self.core.refusal_for_order_id(oid, order, &self.shared))
        {
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
        let sent = ClientCore::with_condition_times(order);

        // A smart combo goes out on its currency's smart combo conId.
        let con_id = combo.as_ref().map(|c| c.smart_con_id).filter(|&c| c > 0).unwrap_or(contract.con_id);
        let instrument = self.core.find_or_register_instrument(
            &self.control_tx,
            con_id, &contract.symbol, &contract.exchange, &contract.sec_type,
        )?;
        self.core.note_currency(&self.control_tx, con_id, &contract.currency);

        // If orderId is already tracked, this is a modification: replace it
        // with the full wanted state (ibx#247). A what-if never modifies:
        // it previews a new order (ibx#462).
        let working = if order.what_if { None } else { self.core.tracked_order(oid) };
        if working.is_some() {
            let refusal = self.core.tracked_contract(oid)
                .and_then(|placed| ClientCore::combo_modify_refusal(contract, &placed));
            if let Some((code, message)) = refusal {
                self.shared.orders.push_order_error(oid, code, message);
                return Ok(());
            }
        }
        let cmd = if let Some(working) = working {
            match ClientCore::build_modify_request(&sent, oid, &working)? {
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
        self.send(cmd)?;
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
        self.send(ControlCommand::Order(OrderRequest::GlobalCancel))
    }

    /// Request next valid order ID. Matches `reqIds` in C++.
    pub fn req_ids(&self, wrapper: &mut impl Wrapper) {
        let next_id = self.next_order_id.load(Ordering::Relaxed);
        wrapper.next_valid_id(next_id);
    }

    /// Get the next order ID (local counter).
    pub fn next_order_id(&self) -> i64 {
        self.next_order_id.fetch_add(1, Ordering::Relaxed)
    }

    // ── Open Orders ──

    /// Request open orders for this client. Matches `reqOpenOrders` in C++.
    ///
    /// Before the order replay of the logon has ended, and while the auth
    /// link is lost, the request is answered only after the order replay,
    /// from `process_msgs` (ibx#251).
    pub fn req_open_orders(&self, wrapper: &mut impl Wrapper) {
        if self.core.hold_open_orders(crate::client_core::OpenOrdersRequest::Open, &self.shared) {
            return;
        }
        self.answer_open_orders(wrapper);
    }

    /// Request all open orders. Matches `reqAllOpenOrders` in C++.
    ///
    /// Held like [`req_open_orders`](Self::req_open_orders) until the order
    /// replay (ibx#251).
    pub fn req_all_open_orders(&self, wrapper: &mut impl Wrapper) {
        if self.core.hold_open_orders(crate::client_core::OpenOrdersRequest::All, &self.shared) {
            return;
        }
        self.answer_open_orders(wrapper);
    }

    /// The open orders, then the end of the list.
    pub(crate) fn answer_open_orders(&self, wrapper: &mut impl Wrapper) {
        for (order_id, tracked) in self.core.collect_open_orders(&self.shared) {
            let state = crate::api::types::OrderState {
                status: tracked.status,
                ..Default::default()
            };
            wrapper.open_order(order_id, &tracked.contract, &tracked.order, &state);
        }
        wrapper.open_order_end();
    }

    // ── Completed Orders ──

    /// Request completed orders. Matches `reqCompletedOrders` in C++.
    /// Immediately delivers all archived completed orders, then calls `completed_orders_end`.
    pub fn req_completed_orders(&self, wrapper: &mut impl Wrapper) {
        for order in self.shared.orders.drain_completed_orders() {
            let status_str = crate::client_core::order_status_str(order.status);
            if let Some(info) = self.shared.orders.get_order_info(order.order_id) {
                let mut state = info.order_state;
                state.status = status_str.into();
                // Enrich contract with secdef cache at read time
                let contract = if info.contract.con_id != 0 {
                    self.core.get_contract(info.contract.con_id, &self.shared).unwrap_or(info.contract)
                } else {
                    info.contract
                };
                // The order as the reference shows it (its unset values).
                let mut order = info.order;
                crate::client_core::reported_unset_values(&mut order);
                wrapper.completed_order(&contract, &order, &state);
            } else {
                let contract = Contract::default();
                let api_order = Order { order_id: order.order_id, ..Default::default() };
                let state = crate::api::types::OrderState {
                    status: status_str.into(),
                    ..Default::default()
                };
                wrapper.completed_order(&contract, &api_order, &state);
            }
            // Bound `order_cache` growth: terminal entries are no longer needed
            // once delivered through `completed_order`.
            self.shared.orders.remove_order_info(order.order_id);
        }
        wrapper.completed_orders_end();
    }

    // ── Executions ──

    /// Automatically bind future orders to this client. Matches `reqAutoOpenOrders` in C++.
    pub fn req_auto_open_orders(&self, _b_auto_bind: bool) {
        // No-op: single-client engine, all orders are auto-bound.
    }

    /// Request execution reports. Matches `reqExecutions` in C++.
    /// Replays stored executions (optionally filtered), firing `exec_details` +
    /// `commission_and_fees_report` for each, then `exec_details_end`.
    pub fn req_executions(&self, req_id: i64, filter: &ExecutionFilter, wrapper: &mut impl Wrapper) {
        if !crate::client_core::ClientCore::ids_fit("req_executions", &[req_id]) { return; }
        // No lock is held during the callbacks (ibx#265). As the reference:
        // every execution, then the commission reports, then the end.
        let execs = self.core.matching_executions(filter);
        for se in &execs {
            wrapper.exec_details(req_id, &se.contract, &se.execution);
        }
        for report in execs.iter().filter_map(|se| se.commission_and_fees.as_ref()) {
            wrapper.commission_and_fees_report(report);
        }
        wrapper.exec_details_end(req_id);
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
        _ => Err(format!("Unsupported algo strategy: '{}'", strategy)),
    }
}
