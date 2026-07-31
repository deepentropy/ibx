//! Order placement, cancellation, execution replay, and algo parsing.

use std::sync::atomic::Ordering;

use crate::api::types::{ExecutionFilter, PRICE_SCALE_F};
use crate::api::wrapper::Wrapper;
use crate::client_core::ClientCore;
use crate::types::*;

use super::{Contract, Order, TagValue, EClient};

impl EClient {
    // ── Orders ──

    /// Place an order. Matches `placeOrder` in C++.
    pub fn place_order(&self, order_id: i64, contract: &Contract, order: &Order) -> Result<(), String> {
        // Validate order params and contract before registering instrument (fail fast).
        ClientCore::validate_order(order)?;
        ClientCore::validate_order_contract(&contract.sec_type)?;

        let oid = if order_id > 0 {
            order_id as u64
        } else {
            self.next_order_id.fetch_add(1, Ordering::Relaxed)
        };

        let instrument = self.core.find_or_register_instrument(
            &self.control_tx,
            contract.con_id, &contract.symbol, &contract.exchange, &contract.sec_type,
        )?;

        // If orderId is already tracked, this is a modification — emit Modify instead of Submit.
        let cmd = if self.core.is_order_tracked(oid) {
            let price = (order.lmt_price * PRICE_SCALE_F) as i64;
            let qty = order.total_quantity as u32;
            ControlCommand::Order(OrderRequest::Modify {
                new_order_id: oid,
                order_id: oid,
                price,
                qty,
            })
        } else {
            ClientCore::build_order_request(order, oid, instrument)?
        };
        self.send(cmd)?;
        self.core.cache_contract(contract.con_id, contract.clone());
        self.core.track_order(oid, contract.clone(), order.clone(), instrument);
        Ok(())
    }

    /// Cancel an order. Matches `cancelOrder` in C++.
    pub fn cancel_order(&self, order_id: i64, _manual_order_cancel_time: &str) -> Result<(), String> {
        self.send(ControlCommand::Order(OrderRequest::Cancel {
            order_id: order_id as u64,
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
        self.cancel_order(order_id as i64, "")
    }

    /// Cancel all orders. Matches `reqGlobalCancel` in C++.
    pub fn req_global_cancel(&self) -> Result<(), String> {
        // Use global instrument count (not just locally-tracked ones)
        let count = self.shared.market.instrument_count();
        for instrument in 0..count {
            self.send(ControlCommand::Order(OrderRequest::CancelAll { instrument }))?;
        }
        Ok(())
    }

    /// Request next valid order ID. Matches `reqIds` in C++.
    pub fn req_ids(&self, wrapper: &mut impl Wrapper) {
        let next_id = self.next_order_id.load(Ordering::Relaxed) as i64;
        wrapper.next_valid_id(next_id);
    }

    /// Get the next order ID (local counter).
    pub fn next_order_id(&self) -> i64 {
        self.next_order_id.fetch_add(1, Ordering::Relaxed) as i64
    }

    // ── Open Orders ──

    /// Request open orders for this client. Matches `reqOpenOrders` in C++.
    pub fn req_open_orders(&self, wrapper: &mut impl Wrapper) {
        self.req_all_open_orders(wrapper);
    }

    /// Request all open orders. Matches `reqAllOpenOrders` in C++.
    pub fn req_all_open_orders(&self, wrapper: &mut impl Wrapper) {
        for (order_id, tracked) in self.core.collect_open_orders(&self.shared) {
            let state = crate::api::types::OrderState {
                status: tracked.status,
                ..Default::default()
            };
            wrapper.open_order(order_id as i64, &tracked.contract, &tracked.order, &state);
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
                wrapper.completed_order(&contract, &info.order, &state);
            } else {
                let contract = Contract::default();
                let api_order = Order { order_id: order.order_id as i64, ..Default::default() };
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
        let indices = self.core.filter_executions(filter);
        let execs = self.core.executions.lock().unwrap();
        for i in indices {
            let se = &execs[i];
            wrapper.exec_details(req_id, &se.contract, &se.execution);
            wrapper.commission_and_fees_report(&se.commission_and_fees);
        }
        wrapper.exec_details_end(req_id);
    }
}

/// Parse algo strategy and TagValue params into internal AlgoParams.
///
/// A key the caller never set defaults the way IB's own algos do (0.0,
/// false, or the documented default enum value). A key the caller *did*
/// set — even to an empty string — is refused if it doesn't parse, instead
/// of silently taking that same default: a typo like `riskAversion="Aggresive"`
/// used to submit a Neutral algo with no error, and `maxPctVol=""` used to
/// submit 0.0. See ibx#263.
pub fn parse_algo_params(strategy: &str, params: &[TagValue]) -> Result<AlgoParams, String> {
    let get = |key: &str| -> Option<String> {
        params.iter().find(|tv| tv.tag == key).map(|tv| tv.value.clone())
    };
    let get_str = |key: &str| -> String { get(key).unwrap_or_default() };
    let get_f64 = |key: &str| -> Result<f64, String> {
        let raw = match get(key) {
            None => return Ok(0.0),
            Some(raw) => raw,
        };
        let v: f64 = raw.parse().map_err(|_| format!("Invalid {} '{}': expected a number", key, raw))?;
        if !v.is_finite() {
            return Err(format!("Invalid {} '{}': must be a finite number", key, raw));
        }
        Ok(v)
    };
    let get_bool = |key: &str| -> Result<bool, String> {
        let raw = match get(key) {
            None => return Ok(false),
            Some(raw) => raw,
        };
        match raw.to_lowercase().as_str() {
            "0" | "false" => Ok(false),
            "1" | "true" => Ok(true),
            _ => Err(format!("Invalid {} '{}': expected true/false or 1/0", key, raw)),
        }
    };

    match strategy.to_lowercase().as_str() {
        "vwap" => Ok(AlgoParams::Vwap {
            max_pct_vol: get_f64("maxPctVol")?,
            no_take_liq: get_bool("noTakeLiq")?,
            allow_past_end_time: get_bool("allowPastEndTime")?,
            start_time: get_str("startTime"),
            end_time: get_str("endTime"),
        }),
        "twap" => Ok(AlgoParams::Twap {
            allow_past_end_time: get_bool("allowPastEndTime")?,
            start_time: get_str("startTime"),
            end_time: get_str("endTime"),
        }),
        "arrivalpx" | "arrival_price" => Ok(AlgoParams::ArrivalPx {
            max_pct_vol: get_f64("maxPctVol")?,
            risk_aversion: parse_risk_aversion(get("riskAversion").as_deref())?,
            allow_past_end_time: get_bool("allowPastEndTime")?,
            force_completion: get_bool("forceCompletion")?,
            start_time: get_str("startTime"),
            end_time: get_str("endTime"),
        }),
        "closepx" | "close_price" => Ok(AlgoParams::ClosePx {
            max_pct_vol: get_f64("maxPctVol")?,
            risk_aversion: parse_risk_aversion(get("riskAversion").as_deref())?,
            force_completion: get_bool("forceCompletion")?,
            start_time: get_str("startTime"),
        }),
        "darkice" | "dark_ice" => {
            let display_size = match get("displaySize") {
                None => 100,
                Some(raw) => raw.parse().map_err(|_| format!("Invalid displaySize '{}': expected a non-negative integer", raw))?,
            };
            Ok(AlgoParams::DarkIce {
                allow_past_end_time: get_bool("allowPastEndTime")?,
                display_size,
                start_time: get_str("startTime"),
                end_time: get_str("endTime"),
            })
        }
        "pctvol" | "pct_vol" => Ok(AlgoParams::PctVol {
            pct_vol: get_f64("pctVol")?,
            no_take_liq: get_bool("noTakeLiq")?,
            start_time: get_str("startTime"),
            end_time: get_str("endTime"),
        }),
        _ => Err(format!("Unsupported algo strategy: '{}'", strategy)),
    }
}

/// Parse a `riskAversion` tag value (used by ArrivalPx and ClosePx). A
/// missing tag defaults to Neutral, matching IB's own algo default; a
/// present value — including an empty string — that isn't a recognized
/// member is refused rather than silently defaulting to Neutral. See ibx#263.
fn parse_risk_aversion(raw: Option<&str>) -> Result<RiskAversion, String> {
    let raw = match raw {
        None => return Ok(RiskAversion::Neutral),
        Some(raw) => raw,
    };
    match raw.to_lowercase().as_str() {
        "neutral" => Ok(RiskAversion::Neutral),
        "get_done" | "getdone" => Ok(RiskAversion::GetDone),
        "aggressive" => Ok(RiskAversion::Aggressive),
        "passive" => Ok(RiskAversion::Passive),
        _ => Err(format!(
            "Unknown riskAversion '{}': expected Get_Done, Aggressive, Neutral or Passive", raw
        )),
    }
}
