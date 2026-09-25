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
        self.core.subscribe_positions();
        self.dispatch_positions(wrapper);
    }

    /// Position rows of a running req_positions (ibx#477).
    pub(crate) fn dispatch_positions(&self, wrapper: &mut impl Wrapper) {
        let Some(batch) = self.core.prepare_positions(&self.shared) else { return };
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
    pub fn req_pnl(&self, req_id: i64, _account: &str, _model_code: &str) {
        self.core.subscribe_pnl(req_id);
    }

    /// Cancel PnL subscription. Matches `cancelPnL` in C++.
    pub fn cancel_pnl(&self, req_id: i64) {
        self.core.unsubscribe_pnl(req_id);
    }

    /// Subscribe to single-position PnL updates. Matches `reqPnLSingle` in C++.
    pub fn req_pnl_single(&self, req_id: i64, _account: &str, _model_code: &str, con_id: i64) {
        self.core.subscribe_pnl_single(req_id, con_id);
    }

    /// Cancel single-position PnL subscription. Matches `cancelPnLSingle` in C++.
    pub fn cancel_pnl_single(&self, req_id: i64) {
        self.core.unsubscribe_pnl_single(req_id);
    }

    // ── Account Summary ──

    /// Request account summary. Matches `reqAccountSummary` in C++.
    /// A server subscription: the rows come as the server sends them, each
    /// batch ends with account_summary_end, until the cancel (ibx#479).
    pub fn req_account_summary(&self, req_id: i64, group: &str, tags: &str) {
        match self.core.subscribe_account_summary(req_id, group, tags) {
            Ok(plan) => {
                if let Some(sr_id) = plan.cancel_sr_id {
                    let _ = self.control_tx.send(ControlCommand::CancelAccountSummary { sr_id });
                }
                let _ = self.control_tx.send(ControlCommand::SubscribeAccountSummary {
                    sr_id: plan.sr_id, tags: plan.wire_tags, group: plan.group,
                });
            }
            Err((code, message)) => self.shared.orders.push_order_error(req_id as u64, code, message),
        }
    }

    /// Cancel account summary. Matches `cancelAccountSummary` in C++.
    pub fn cancel_account_summary(&self, req_id: i64) {
        if let Some(sr_id) = self.core.unsubscribe_account_summary(req_id) {
            let _ = self.control_tx.send(ControlCommand::CancelAccountSummary { sr_id });
        }
    }

    // ── Account Updates ──

    /// Subscribe to account updates. Matches `reqAccountUpdates` in C++.
    pub fn req_account_updates(&self, subscribe: bool, _acct_code: &str) {
        // An unsubscribe answers error 2100 with id -1 (ibx#475).
        if let Some((code, message)) = self.core.subscribe_account_updates(subscribe) {
            self.shared.orders.push_order_error(-1i64 as u64, code, message);
        }
    }

    /// Cancel positions subscription. Matches `cancelPositions` in C++.
    pub fn cancel_positions(&self) {
        self.core.unsubscribe_positions();
    }

    /// Request managed accounts. Matches `reqManagedAccts` in C++.
    pub fn req_managed_accts(&self, wrapper: &mut impl Wrapper) {
        wrapper.managed_accounts(&self.account_id);
    }

    /// Request account updates for multiple accounts/models. Matches `reqAccountUpdatesMulti` in C++.
    pub fn req_account_updates_multi(
        &self, _req_id: i64, _account: &str, _model_code: &str, _ledger_and_nlv: bool,
        wrapper: &mut impl Wrapper,
    ) {
        let acct = self.shared.portfolio.account();
        let fields: &[(&str, f64)] = &[
            ("NetLiquidation", acct.net_liquidation as f64 / PRICE_SCALE_F),
            ("TotalCashValue", acct.total_cash_value as f64 / PRICE_SCALE_F),
            ("BuyingPower", acct.buying_power as f64 / PRICE_SCALE_F),
            ("GrossPositionValue", acct.gross_position_value as f64 / PRICE_SCALE_F),
            ("UnrealizedPnL", acct.unrealized_pnl as f64 / PRICE_SCALE_F),
            ("RealizedPnL", acct.realized_pnl as f64 / PRICE_SCALE_F),
            ("InitMarginReq", acct.init_margin_req as f64 / PRICE_SCALE_F),
            ("MaintMarginReq", acct.maint_margin_req as f64 / PRICE_SCALE_F),
        ];
        for (key, val) in fields {
            let val_str = format!("{:.2}", val);
            wrapper.update_account_value(key, &val_str, "USD", &self.account_id);
        }
        wrapper.account_download_end(&self.account_id);
    }

    /// Cancel multi-account updates. Matches `cancelAccountUpdatesMulti` in C++.
    pub fn cancel_account_updates_multi(&self, _req_id: i64) {
        // No-op: delivered immediately.
    }

    /// Request positions for multiple accounts/models. Matches `reqPositionsMulti` in C++.
    pub fn req_positions_multi(
        &self, _req_id: i64, _account: &str, _model_code: &str,
        wrapper: &mut impl Wrapper,
    ) {
        self.req_positions(wrapper);
    }

    /// Cancel multi-account positions. Matches `cancelPositionsMulti` in C++.
    pub fn cancel_positions_multi(&self, _req_id: i64) {
        // No-op: delivered immediately.
    }

    /// Read account state snapshot.
    pub fn account(&self) -> AccountState {
        self.shared.portfolio.account()
    }
}
