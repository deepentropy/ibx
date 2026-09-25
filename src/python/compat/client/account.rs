//! Account-related methods: positions, PnL, account summary/updates.

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

use crate::types::*;
use super::EClient;
use super::super::super::types::PRICE_SCALE_F;

#[pymethods]
impl EClient {
    /// Request P&L updates for the account.
    #[pyo3(signature = (req_id, account, model_code=""))]
    fn req_pnl(&self, req_id: i64, account: &str, model_code: &str) -> PyResult<()> {
        if let Some(r) = self.not_connected(-1) { return r; }
        self.core.subscribe_pnl(req_id);
        let tx = self.tx()?;
        let acct = if account.is_empty() { self.account() } else { account.to_string() };
        tx.send(ControlCommand::SubscribePnl { req_id, account: acct })
            .map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        let _ = model_code;
        Ok(())
    }

    /// Cancel P&L subscription.
    fn cancel_pnl(&self, req_id: i64) -> PyResult<()> {
        if let Some(r) = self.not_connected(-1) { return r; }
        self.core.unsubscribe_pnl(req_id);
        let tx = self.tx()?;
        let _ = tx.send(ControlCommand::CancelPnl { req_id });
        Ok(())
    }

    /// Request P&L for a single position.
    #[pyo3(signature = (req_id, account, model_code, con_id))]
    fn req_pnl_single(&self, req_id: i64, account: &str, model_code: &str, con_id: i64) -> PyResult<()> {
        if let Some(r) = self.not_connected(-1) { return r; }
        self.core.subscribe_pnl_single(req_id, con_id);
        let _ = (account, model_code);
        Ok(())
    }

    /// Cancel single-position P&L subscription.
    fn cancel_pnl_single(&self, req_id: i64) -> PyResult<()> {
        if let Some(r) = self.not_connected(-1) { return r; }
        self.core.unsubscribe_pnl_single(req_id);
        Ok(())
    }

    /// Request account summary.
    #[pyo3(signature = (req_id, group_name, tags))]
    fn req_account_summary(&self, req_id: i64, group_name: &str, tags: &str) -> PyResult<()> {
        if let Some(r) = self.not_connected(-1) { return r; }
        // A server subscription: the rows come as the server sends them, each
        // batch ends with account_summary_end, until the cancel (ibx#479).
        match self.core.subscribe_account_summary(req_id, group_name, tags) {
            Ok(plan) => {
                let tx = self.tx()?;
                if let Some(sr_id) = plan.cancel_sr_id {
                    let _ = tx.send(ControlCommand::CancelAccountSummary { sr_id });
                }
                let _ = tx.send(ControlCommand::SubscribeAccountSummary {
                    sr_id: plan.sr_id, tags: plan.wire_tags, group: plan.group,
                });
            }
            Err((code, message)) => self.shared_state()?.orders.push_order_error(req_id as u64, code, message),
        }
        Ok(())
    }

    /// Cancel account summary.
    fn cancel_account_summary(&self, req_id: i64) -> PyResult<()> {
        if let Some(r) = self.not_connected(-1) { return r; }
        if let Some(sr_id) = self.core.unsubscribe_account_summary(req_id) {
            let _ = self.tx()?.send(ControlCommand::CancelAccountSummary { sr_id });
        }
        Ok(())
    }

    /// Request all positions.
    fn req_positions(&self, py: Python<'_>) -> PyResult<()> {
        if let Some(r) = self.not_connected(-1) { return r; }
        // A subscription, as the reference (ibx#477): the snapshot and
        // position_end once the data is in, then a row on each change.
        self.core.subscribe_positions();
        let shared = self.shared_state()?;
        self.dispatch_positions(py, &shared)
    }

    /// Cancel positions.
    fn cancel_positions(&self) -> PyResult<()> {
        if let Some(r) = self.not_connected(-1) { return r; }
        self.core.unsubscribe_positions();
        Ok(())
    }

    /// Request account updates.
    #[pyo3(signature = (subscribe, _acct_code=""))]
    fn req_account_updates(&self, subscribe: bool, _acct_code: &str) -> PyResult<()> {
        if let Some(r) = self.not_connected(-1) { return r; }
        // An unsubscribe answers error 2100 with id -1 (ibx#475).
        if let Some((code, message)) = self.core.subscribe_account_updates(subscribe) {
            self.shared_state()?.orders.push_order_error(-1i64 as u64, code, message);
        }
        Ok(())
    }

    /// Request managed accounts list.
    fn req_managed_accts(&self, py: Python<'_>) -> PyResult<()> {
        if let Some(r) = self.not_connected(-1) { return r; }
        self.wrapper.call_method1(py, "managed_accounts", (self.account().as_str(),))?;
        Ok(())
    }

    /// Request account updates across accounts / models. A subscription,
    /// as the reference (ibx#476): the rows, account_update_multi_end, then
    /// the rows that change, until cancel_account_updates_multi.
    #[pyo3(signature = (req_id, account, model_code, ledger_and_nlv=false))]
    fn req_account_updates_multi(
        &self, py: Python<'_>, req_id: i64, account: &str, model_code: &str, ledger_and_nlv: bool,
    ) -> PyResult<()> {
        if let Some(r) = self.not_connected(req_id as i64) { return r; }
        let shared = self.shared_state()?;
        if let Err((code, message)) = self.core.subscribe_account_multi(req_id, account, model_code, ledger_and_nlv) {
            shared.orders.push_order_error(req_id as u64, code, message);
            return Ok(());
        }
        self.dispatch_multi(py, &shared)
    }

    /// Cancel multi-account updates.
    fn cancel_account_updates_multi(&self, req_id: i64) -> PyResult<()> {
        if let Some(r) = self.not_connected(-1) { return r; }
        self.core.unsubscribe_account_multi(req_id);
        Ok(())
    }

    /// Request positions across multiple accounts/models. A subscription, as
    /// the reference (ibx#476).
    #[pyo3(signature = (req_id, account, model_code))]
    fn req_positions_multi(&self, py: Python<'_>, req_id: i64, account: &str, model_code: &str) -> PyResult<()> {
        if let Some(r) = self.not_connected(-1) { return r; }
        self.core.subscribe_positions_multi(req_id, account, model_code);
        let shared = self.shared_state()?;
        self.dispatch_multi(py, &shared)
    }

    /// Cancel multi-account positions.
    fn cancel_positions_multi(&self, req_id: i64) -> PyResult<()> {
        if let Some(r) = self.not_connected(-1) { return r; }
        self.core.unsubscribe_positions_multi(req_id);
        Ok(())
    }

    /// Read account state snapshot. Returns a dict with all account values.
    fn account_snapshot(&self) -> PyResult<Option<Py<PyAny>>> {
        let shared = match self.shared.lock().unwrap().clone() {
            Some(s) => s,
            None => return Ok(None),
        };
        let acct = shared.portfolio.account();
        Python::attach(|py| {
            let ps = PRICE_SCALE_F;
            let dict = pyo3::types::PyDict::new(py);
            dict.set_item("net_liquidation", acct.net_liquidation as f64 / ps)?;
            dict.set_item("buying_power", acct.buying_power as f64 / ps)?;
            dict.set_item("total_cash_value", acct.total_cash_value as f64 / ps)?;
            dict.set_item("gross_position_value", acct.gross_position_value as f64 / ps)?;
            dict.set_item("unrealized_pnl", acct.unrealized_pnl as f64 / ps)?;
            dict.set_item("realized_pnl", acct.realized_pnl as f64 / ps)?;
            dict.set_item("daily_pnl", acct.daily_pnl as f64 / ps)?;
            dict.set_item("init_margin_req", acct.init_margin_req as f64 / ps)?;
            dict.set_item("maint_margin_req", acct.maint_margin_req as f64 / ps)?;
            dict.set_item("available_funds", acct.available_funds as f64 / ps)?;
            dict.set_item("excess_liquidity", acct.excess_liquidity as f64 / ps)?;
            dict.set_item("settled_cash", acct.settled_cash as f64 / ps)?;
            dict.set_item("equity_with_loan", acct.equity_with_loan as f64 / ps)?;
            dict.set_item("cushion", acct.cushion as f64 / ps)?;
            dict.set_item("leverage", acct.leverage as f64 / ps)?;
            dict.set_item("sma", acct.sma as f64 / ps)?;
            dict.set_item("day_trades_remaining", acct.day_trades_remaining)?;
            Ok(Some(dict.into_any().unbind()))
        })
    }
}
