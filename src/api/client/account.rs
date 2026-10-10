//! Account-related methods: positions, PnL, account summary/updates.

use crate::api::types::{PRICE_SCALE_F, QTY_SCALE_F};
use crate::api::wrapper::Wrapper;
use crate::types::*;

use super::{Contract, EClient};

impl EClient {
    // ── Positions ──

    /// Request positions. Matches `reqPositions` in C++.
    ///
    /// A subscription, as the reference (ibx#477): the snapshot and
    /// `position_end` once the position data is in, then one `position` row
    /// each time a position or its average cost changes, until
    /// `cancel_positions`. What is ready now is sent through `wrapper`; the
    /// rest comes through `process_msgs`. No wait in the caller's thread.
    pub fn req_positions(&self, wrapper: &mut impl Wrapper) {
        // Answered here: after the requests made before it (ibx#561).
        self.shared.command_clock().take_turn();
        // Accounts whose application is not approved (ibx#421).
        match crate::client_core::ClientCore::positions_pending_check(&self.shared.reference) {
            Err((code, message)) => {
                self.shared.orders.push_order_error(-1, code, message);
                return;
            }
            Ok(Some((code, message))) => self.shared.orders.push_order_error(-1, code, message),
            Ok(None) => {}
        }
        self.core.subscribe_positions();
        self.dispatch_positions(wrapper);
    }

    /// Position rows of a running req_positions (ibx#477).
    pub(crate) fn dispatch_positions(&self, wrapper: &mut impl Wrapper) {
        let Some(batch) = self.core.prepare_positions(&self.shared, &self.account_id) else { return };
        for pi in &batch.rows {
            let ac = self.core.position_contract(pi.con_id, &self.shared);
            let c = Contract {
                con_id: ac.con_id, symbol: ac.symbol, sec_type: ac.sec_type,
                exchange: ac.exchange, primary_exchange: ac.primary_exchange,
                currency: ac.currency, local_symbol: ac.local_symbol,
                trading_class: ac.trading_class, multiplier: ac.multiplier,
                ..Default::default()
            };
            wrapper.position(&self.account_id, &c, pi.position_fixed as f64 / QTY_SCALE_F, pi.avg_cost as f64 / PRICE_SCALE_F);
        }
        if batch.end {
            wrapper.position_end();
        }
        if let Some((code, message)) = batch.error {
            wrapper.error(-1, code, &message, "");
        }
    }

    // ── PnL ──

    /// Subscribe to account PnL updates. Matches `reqPnL` in C++.
    /// Several requests can run; an empty or unknown account gives 321, a
    /// request id already running gives 102 (ibx#478).
    pub fn req_pnl(&self, req_id: i64, account: &str, _model_code: &str) {
        if !crate::client_core::ClientCore::ids_fit("req_pnl", &[req_id]) { return; }
        if let Err((code, message)) = self.core.request_pnl(req_id, account, &self.account_id) {
            self.shared.orders.push_order_error(req_id, code, message);
        }
    }

    /// Cancel PnL subscription. Matches `cancelPnL` in C++.
    /// A request id not running gives 10185 (ibx#478).
    pub fn cancel_pnl(&self, req_id: i64) {
        if !crate::client_core::ClientCore::ids_fit("cancel_pnl", &[req_id]) { return; }
        if let Some((code, message)) = self.core.cancel_pnl_request(req_id) {
            self.shared.orders.push_order_error(req_id, code, message);
        }
    }

    /// Subscribe to single-position PnL updates. Matches `reqPnLSingle` in C++.
    /// Same checks as `req_pnl` (ibx#478).
    pub fn req_pnl_single(&self, req_id: i64, account: &str, _model_code: &str, con_id: i64) {
        if !crate::client_core::ClientCore::ids_fit("req_pnl_single", &[req_id, con_id]) { return; }
        if let Err((code, message)) = self.core.request_pnl_single(req_id, account, &self.account_id, con_id) {
            self.shared.orders.push_order_error(req_id, code, message);
        }
    }

    /// Cancel single-position PnL subscription. Matches `cancelPnLSingle` in C++.
    /// A request id not running gives 10186 (ibx#478).
    pub fn cancel_pnl_single(&self, req_id: i64) {
        if !crate::client_core::ClientCore::ids_fit("cancel_pnl_single", &[req_id]) { return; }
        if let Some((code, message)) = self.core.cancel_pnl_single_request(req_id) {
            self.shared.orders.push_order_error(req_id, code, message);
        }
    }

    // ── Account Summary ──

    /// Request account summary. Matches `reqAccountSummary` in C++.
    /// A server subscription: the rows come as the server sends them, each
    /// batch ends with account_summary_end, until the cancel (ibx#479).
    pub fn req_account_summary(&self, req_id: i64, group: &str, tags: &str) {
        // One API request for the pacing, whatever it sends (ibx#555).
        let _one = crate::engine::park::one_request();
        if !crate::client_core::ClientCore::ids_fit("req_account_summary", &[req_id]) { return; }
        match self.core.subscribe_account_summary(req_id, group, tags) {
            Ok(plan) => {
                if let Some(sr_id) = plan.cancel_sr_id {
                    let _ = self.control_tx.send(ControlCommand::CancelAccountSummary { sr_id });
                }
                let _ = self.control_tx.send(ControlCommand::SubscribeAccountSummary {
                    sr_id: plan.sr_id, tags: plan.wire_tags, group: plan.group,
                });
            }
            Err((code, message)) => self.shared.orders.push_order_error(req_id, code, message),
        }
    }

    /// Cancel account summary. Matches `cancelAccountSummary` in C++.
    pub fn cancel_account_summary(&self, req_id: i64) {
        if !crate::client_core::ClientCore::ids_fit("cancel_account_summary", &[req_id]) { return; }
        if let Some(sr_id) = self.core.unsubscribe_account_summary(req_id) {
            let _ = self.control_tx.send(ControlCommand::CancelAccountSummary { sr_id });
        }
    }

    // ── Account Updates ──

    /// Subscribe to account updates. Matches `reqAccountUpdates` in C++.
    pub fn req_account_updates(&self, subscribe: bool, acct_code: &str) {
        // An account whose application is not approved: a warning, the
        // request goes on (ibx#421).
        if let Some((code, message)) = crate::client_core::ClientCore::account_updates_pending_warning(
            acct_code, &self.shared.reference, &self.account_id)
        {
            self.shared.orders.push_order_error(-1, code, message);
        }
        // An unsubscribe answers error 2100 with id -1 (ibx#475).
        if let Some((code, message)) = self.core.subscribe_account_updates(subscribe) {
            self.shared.orders.push_order_error(-1, code, message);
        }
    }

    /// Cancel positions subscription. Matches `cancelPositions` in C++.
    pub fn cancel_positions(&self) {
        self.core.unsubscribe_positions();
    }

    /// Request managed accounts. Matches `reqManagedAccts` in C++.
    /// Every account of the logon's account list, in logon order, comma
    /// separated, as the reference (ibx#420); the logon account when the
    /// logon had no list.
    pub fn req_managed_accts(&self, wrapper: &mut impl Wrapper) {
        // Answered here: after the requests made before it (ibx#561).
        self.shared.command_clock().take_turn();
        wrapper.managed_accounts(&self.shared.reference.managed_accounts_text(&self.account_id));
    }

    /// Request account updates for multiple accounts/models. Matches `reqAccountUpdatesMulti` in C++.
    ///
    /// A subscription, as the reference (ibx#476): the account values
    /// (unless `ledger_and_nlv`) and the ledger rows as the server sent them,
    /// each through `account_update_multi` with the request id and model
    /// code, then `account_update_multi_end`; then the rows that change,
    /// until `cancel_account_updates_multi`. A request id already running
    /// gives error 322. What is ready is sent through `wrapper` at once; the
    /// rest comes through `process_msgs`.
    pub fn req_account_updates_multi(
        &self, req_id: i64, account: &str, model_code: &str, ledger_and_nlv: bool,
        wrapper: &mut impl Wrapper,
    ) {
        if !crate::client_core::ClientCore::ids_fit("req_account_updates_multi", &[req_id]) { return; }
        if let Err((code, message)) = self.core.subscribe_account_multi(req_id, account, model_code, ledger_and_nlv) {
            wrapper.error(req_id, code, &message, "");
            return;
        }
        self.dispatch_multi(wrapper);
    }

    /// Cancel multi-account updates. Matches `cancelAccountUpdatesMulti` in C++.
    pub fn cancel_account_updates_multi(&self, req_id: i64) {
        if !crate::client_core::ClientCore::ids_fit("cancel_account_updates_multi", &[req_id]) { return; }
        self.core.unsubscribe_account_multi(req_id);
    }

    /// Request positions for multiple accounts/models. Matches `reqPositionsMulti` in C++.
    ///
    /// A subscription, as the reference (ibx#476): `position_multi` rows
    /// with the request id and model code, `position_multi_end`, then a row
    /// each time a position or its average cost changes, until
    /// `cancel_positions_multi`.
    pub fn req_positions_multi(
        &self, req_id: i64, account: &str, model_code: &str,
        wrapper: &mut impl Wrapper,
    ) {
        if !crate::client_core::ClientCore::ids_fit("req_positions_multi", &[req_id]) { return; }
        // An account whose application is not approved (ibx#421).
        if let Some((code, message)) = crate::client_core::ClientCore::positions_multi_pending_refusal(
            account, &self.shared.reference, &self.account_id)
        {
            self.shared.orders.push_order_error(req_id, code, message);
            return;
        }
        self.core.subscribe_positions_multi(req_id, account, model_code);
        self.dispatch_multi(wrapper);
    }

    /// Cancel multi-account positions. Matches `cancelPositionsMulti` in C++.
    pub fn cancel_positions_multi(&self, req_id: i64) {
        if !crate::client_core::ClientCore::ids_fit("cancel_positions_multi", &[req_id]) { return; }
        self.core.unsubscribe_positions_multi(req_id);
    }

    /// Rows of the running multi-account requests (ibx#476).
    pub(crate) fn dispatch_multi(&self, wrapper: &mut impl Wrapper) {
        for batch in self.core.prepare_account_multi(&self.shared) {
            let account = if batch.account.is_empty() { self.account_id.as_str() } else { batch.account.as_str() };
            for row in &batch.rows {
                wrapper.account_update_multi(batch.req_id, account, &batch.model_code, &row.key, &row.value, &row.currency);
            }
            if batch.end {
                wrapper.account_update_multi_end(batch.req_id);
            }
        }
        for (req_id, account, model_code, batch) in self.core.prepare_positions_multi(&self.shared) {
            let account = if account.is_empty() { self.account_id.clone() } else { account };
            for pi in &batch.rows {
                let ac = self.core.position_contract(pi.con_id, &self.shared);
                let c = Contract {
                    con_id: ac.con_id, symbol: ac.symbol, sec_type: ac.sec_type,
                    exchange: ac.exchange, primary_exchange: ac.primary_exchange,
                    currency: ac.currency, local_symbol: ac.local_symbol,
                    trading_class: ac.trading_class, multiplier: ac.multiplier,
                    ..Default::default()
                };
                wrapper.position_multi(req_id, &account, &model_code, &c,
                    pi.position_fixed as f64 / QTY_SCALE_F, pi.avg_cost as f64 / PRICE_SCALE_F);
            }
            if batch.end {
                wrapper.position_multi_end(req_id);
            }
            if let Some((code, message)) = batch.error {
                wrapper.error(req_id, code, &message, "");
            }
        }
    }

    /// Read account state snapshot.
    pub fn account(&self) -> AccountState {
        self.shared.portfolio.account()
    }
}
