//! Account-related methods: positions, PnL, account summary/updates.

use crate::api::types::PRICE_SCALE_F;
use crate::api::wrapper::Wrapper;
use crate::types::*;

use super::{Contract, EClient};

impl EClient {
    // ── Positions ──

    /// Request positions. Matches `reqPositions` in C++.
    /// Immediately delivers all positions via wrapper callbacks, then calls position_end.
    pub fn req_positions(&self, wrapper: &mut impl Wrapper) {
        let positions = self.shared.portfolio.position_infos();
        for pi in &positions {
            let c = self.core.get_contract(pi.con_id, &self.shared)
                .unwrap_or_else(|| Contract { con_id: pi.con_id, ..Default::default() });
            let avg_cost = pi.avg_cost as f64 / PRICE_SCALE_F;
            wrapper.position(&self.account_id, &c, pi.position as f64, avg_cost);
        }
        wrapper.position_end();
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
    pub fn req_account_summary(&self, req_id: i64, _group: &str, tags: &str) {
        self.core.subscribe_account_summary(req_id, tags);
    }

    /// Cancel account summary. Matches `cancelAccountSummary` in C++.
    pub fn cancel_account_summary(&self, req_id: i64) {
        self.core.unsubscribe_account_summary(req_id);
    }

    // ── Account Updates ──

    /// Subscribe to account updates. Matches `reqAccountUpdates` in C++.
    pub fn req_account_updates(&self, subscribe: bool, _acct_code: &str) {
        self.core.subscribe_account_updates(subscribe);
    }

    /// Read account state snapshot.
    pub fn account(&self) -> AccountState {
        self.shared.portfolio.account()
    }
}
