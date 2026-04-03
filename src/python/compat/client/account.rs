//! Account-related methods: positions, PnL, account summary/updates.

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

use crate::types::*;
use super::EClient;
use super::super::contract::Contract;
use super::super::super::types::PRICE_SCALE_F;

#[pymethods]
impl EClient {
    /// Request P&L updates for the account.
    #[pyo3(signature = (req_id, account, model_code=""))]
    fn req_pnl(&self, req_id: i64, account: &str, model_code: &str) -> PyResult<()> {
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
        self.core.unsubscribe_pnl(req_id);
        let tx = self.tx()?;
        let _ = tx.send(ControlCommand::CancelPnl { req_id });
        Ok(())
    }

    /// Request P&L for a single position.
    #[pyo3(signature = (req_id, account, model_code, con_id))]
    fn req_pnl_single(&self, req_id: i64, account: &str, model_code: &str, con_id: i64) -> PyResult<()> {
        self.core.subscribe_pnl_single(req_id, con_id);
        let _ = (account, model_code);
        Ok(())
    }

    /// Cancel single-position P&L subscription.
    fn cancel_pnl_single(&self, req_id: i64) -> PyResult<()> {
        self.core.unsubscribe_pnl_single(req_id);
        Ok(())
    }

    /// Request account summary.
    #[pyo3(signature = (req_id, group_name, tags))]
    fn req_account_summary(&self, req_id: i64, group_name: &str, tags: &str) -> PyResult<()> {
        self.core.subscribe_account_summary(req_id, tags);
        let _ = group_name;
        Ok(())
    }

    /// Cancel account summary.
    fn cancel_account_summary(&self, req_id: i64) -> PyResult<()> {
        self.core.unsubscribe_account_summary(req_id);
        Ok(())
    }

    /// Request all positions.
    fn req_positions(&self, py: Python<'_>) -> PyResult<()> {
        let shared = self.shared_state()?;
        // Wait for CCP init burst to complete (up to 10s).
        for _ in 0..1000 {
            if shared.portfolio.account_download_complete() { break; }
            py.allow_threads(|| std::thread::sleep(std::time::Duration::from_millis(10)));
        }
        let positions = shared.portfolio.position_infos();
        for pi in &positions {
            let c = self.core.get_contract(pi.con_id, &shared).map(|ac| {
                let mut c = Contract::default();
                c.con_id = ac.con_id;
                c.symbol = ac.symbol;
                c.sec_type = ac.sec_type;
                c.exchange = ac.exchange;
                c.currency = ac.currency;
                c
            }).unwrap_or_else(|| {
                let mut c = Contract::default();
                c.con_id = pi.con_id;
                c
            });
            let c_py = Py::new(py, c)?.into_any();
            let avg_cost = pi.avg_cost as f64 / PRICE_SCALE_F;
            self.wrapper.call_method(
                py, "position",
                (self.account().as_str(), &c_py, pi.position as f64, avg_cost),
                None,
            )?;
        }
        self.wrapper.call_method0(py, "position_end")?;
        Ok(())
    }

    /// Cancel positions.
    fn cancel_positions(&self) -> PyResult<()> {
        Ok(())
    }

    /// Request account updates.
    #[pyo3(signature = (subscribe, _acct_code=""))]
    fn req_account_updates(&self, subscribe: bool, _acct_code: &str) -> PyResult<()> {
        self.core.subscribe_account_updates(subscribe);
        Ok(())
    }

    /// Request managed accounts list.
    fn req_managed_accts(&self, py: Python<'_>) -> PyResult<()> {
        self.wrapper.call_method1(py, "managed_accounts", (self.account().as_str(),))?;
        Ok(())
    }

    /// Request account updates for multiple accounts/models.
    #[pyo3(signature = (req_id, account, model_code, ledger_and_nlv=false))]
    fn req_account_updates_multi(
        &self, py: Python<'_>, req_id: i64, account: &str, model_code: &str, ledger_and_nlv: bool,
    ) -> PyResult<()> {
        let shared = self.shared_state()?;
        let _ = ledger_and_nlv;
        let acct = shared.portfolio.account();
        let acct_default = self.account();
        let acct_name = if !account.is_empty() { account } else { acct_default.as_str() };
        let tag_values: [(&str, f64); 8] = [
            ("NetLiquidation", acct.net_liquidation as f64 / PRICE_SCALE_F),
            ("TotalCashValue", acct.total_cash_value as f64 / PRICE_SCALE_F),
            ("BuyingPower", acct.buying_power as f64 / PRICE_SCALE_F),
            ("GrossPositionValue", acct.gross_position_value as f64 / PRICE_SCALE_F),
            ("UnrealizedPnL", acct.unrealized_pnl as f64 / PRICE_SCALE_F),
            ("RealizedPnL", acct.realized_pnl as f64 / PRICE_SCALE_F),
            ("InitMarginReq", acct.init_margin_req as f64 / PRICE_SCALE_F),
            ("MaintMarginReq", acct.maint_margin_req as f64 / PRICE_SCALE_F),
        ];
        for (key, val) in &tag_values {
            let val_str = format!("{:.2}", val);
            self.wrapper.call_method(
                py, "account_update_multi",
                (req_id, acct_name, model_code, *key, val_str.as_str(), "USD"),
                None,
            )?;
        }
        self.wrapper.call_method1(py, "account_update_multi_end", (req_id,))?;
        Ok(())
    }

    /// Cancel multi-account updates.
    fn cancel_account_updates_multi(&self, req_id: i64) -> PyResult<()> {
        let _ = req_id;
        Ok(())
    }

    /// Request positions across multiple accounts/models.
    #[pyo3(signature = (req_id, account, model_code))]
    fn req_positions_multi(&self, py: Python<'_>, req_id: i64, account: &str, model_code: &str) -> PyResult<()> {
        let shared = self.shared_state()?;
        for _ in 0..500 {
            if shared.portfolio.account_data_received() { break; }
            py.allow_threads(|| std::thread::sleep(std::time::Duration::from_millis(10)));
        }
        let positions = shared.portfolio.position_infos();
        for pi in &positions {
            let mut c = Contract::default();
            c.con_id = pi.con_id;
            let c_py = Py::new(py, c)?.into_any();
            let avg_cost = pi.avg_cost as f64 / PRICE_SCALE_F;
            self.wrapper.call_method(
                py, "position_multi",
                (req_id, account, model_code, &c_py, pi.position as f64, avg_cost),
                None,
            )?;
        }
        self.wrapper.call_method1(py, "position_multi_end", (req_id,))?;
        Ok(())
    }

    /// Cancel multi-account positions.
    fn cancel_positions_multi(&self, req_id: i64) -> PyResult<()> {
        let _ = req_id;
        Ok(())
    }

    /// Read account state snapshot. Returns a dict with all account values.
    fn account_snapshot(&self) -> PyResult<Option<PyObject>> {
        let shared = match self.shared.lock().unwrap().clone() {
            Some(s) => s,
            None => return Ok(None),
        };
        let acct = shared.portfolio.account();
        Python::with_gil(|py| {
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
