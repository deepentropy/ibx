//! Account-related methods: positions, PnL, account summary/updates.

use pyo3::prelude::*;

use crate::bridge::SharedState;
use crate::error_codes::Refusal;
use crate::types::*;
use std::sync::atomic::Ordering;
use super::EClient;
use super::super::contract::Contract;
use super::super::super::types::PRICE_SCALE_F;

impl EClient {
    /// The contract a position is a position in. Prefers the secdef cache,
    /// which carries exchange/localSymbol/tradingClass, and falls back to the
    /// wire-derived `PositionInfo` fields when it is cold.
    pub(crate) fn position_contract(
        &self, py: Python<'_>, pi: &PositionInfo, shared: &crate::bridge::SharedState,
    ) -> PyResult<Contract> {
        match self.core.get_contract(pi.con_id, shared) {
            Some(ac) => Contract::from_api(py, &ac),
            None => Ok(Contract {
                con_id: pi.con_id,
                symbol: pi.symbol.clone(),
                sec_type: pi.sec_type.clone(),
                currency: pi.currency.clone(),
                multiplier: pi.multiplier.clone(),
                ..Default::default()
            }),
        }
    }
}

#[pymethods]
impl EClient {
    /// Request P&L updates for the account.
    ///
    /// `model_code` is taken and not applied. One session holds one account
    /// here, and the venue states its figures for that account without being
    /// asked which, so there is no model portfolio to name. Another account is
    /// refused rather than answered with this account's profit.
    #[pyo3(signature = (req_id, account, model_code=""))]
    fn req_pnl(&self, py: Python<'_>, req_id: i64, account: &str, model_code: &str) -> PyResult<()> {
        // The session before the slot: taken first, a refused request held
        // the one slot there is, and the next request under another number
        // was refused as a duplicate of one that never went.
        let Some(tx) = self.tx_or_report(req_id)? else { return Ok(()) };
        // Another account's profit is not something this session can state.
        // The figures are worked out from one set of midnight seeds against
        // one book of holdings, and both belong to the account this session
        // opened under; there is no second pair to answer another account
        // from. So the request ends here — the venue never reports one
        // account's profit under a request naming a different one, and
        // reporting this account's figures under the caller's own request
        // number is an answer it never gives.
        //
        // Refused before the slot for the reason above it: a request that will
        // not be reported must not hold the one subscription there is.
        if !account.is_empty() && account != self.account() {
            let why = format!(
                "account {account} was named and this session opened under {}, \
                 whose profit is not what was asked for",
                self.account(),
            );
            log::warn!("{why}");
            return self.report_refusal(py, req_id, Refusal::validation(why));
        }
        // Refused while another request holds the subscription, and nothing
        // is asked of the venue for a request that will not be reported.
        if let Err(why) = self.core.subscribe_pnl(req_id) {
            return self.report_refusal(py, req_id, why);
        }
        let acct = self.account();
        let _ = model_code;
        // Answered on the error callback and returned normally, as a request
        // made before connecting already is. Raising instead is a path a
        // caller written against the reference client does not take, so the
        // two clients answered the same failure differently.
        if let Err(why) =
            Self::send_control(py, &tx, ControlCommand::SubscribePnl { req_id, account: acct })
        {
            // And the one slot there is goes back with it. Held, the next
            // request under any number was refused as a duplicate of a
            // subscription the venue was never asked for.
            self.core.unsubscribe_pnl(req_id);
            return self.report_refusal(py, req_id, Refusal::not_connected(why.to_string()));
        }
        Ok(())
    }

    /// Cancel P&L subscription.
    fn cancel_pnl(&self, py: Python<'_>, req_id: i64) -> PyResult<()> {
        self.core.unsubscribe_pnl(req_id);
        let Some(tx) = self.tx_or_report(req_id)? else { return Ok(()) };
        // A failed send is reported. Discarded, the subscription stays up and
        // the caller is told the cancel succeeded.
        if let Err(why) = Self::send_control(py, &tx, ControlCommand::CancelPnl { req_id }) {
            return self.report_refusal(py, req_id, Refusal::not_connected(why.to_string()));
        }
        Ok(())
    }

    /// Request P&L for a single position.
    ///
    /// `model_code` is taken and not applied. One session holds one account
    /// here, and the venue states its figures for that account without being
    /// asked which, so there is no model portfolio to name; another account is
    /// refused here as `req_pnl` refuses it, and for the reason given there.
    #[pyo3(signature = (req_id, account, model_code, con_id))]
    fn req_pnl_single(&self, py: Python<'_>, req_id: i64, account: &str, model_code: &str, con_id: i64) -> PyResult<()> {
        let Some(_tx) = self.tx_or_report(-1)? else { return Ok(()) };
        if !account.is_empty() && account != self.account() {
            let why = format!(
                "account {account} was named and this session opened under {}, \
                 whose profit is not what was asked for",
                self.account(),
            );
            log::warn!("{why}");
            return self.report_refusal(py, req_id, Refusal::validation(why));
        }
        self.core.subscribe_pnl_single(req_id, con_id);
        let _ = model_code;
        Ok(())
    }


    /// Cancel single-position P&L subscription.
    fn cancel_pnl_single(&self, req_id: i64) -> PyResult<()> {
        let Some(_tx) = self.tx_or_report(-1)? else { return Ok(()) };
        self.core.unsubscribe_pnl_single(req_id);
        Ok(())
    }

    /// Request account summary.
    ///
    /// `group_name` is taken and not applied. One session holds one account
    /// here, and the venue states its figures for that account without being
    /// asked which, so there is no second account or model portfolio to name.
    #[pyo3(signature = (req_id, group_name, tags))]
    fn req_account_summary(&self, py: Python<'_>, req_id: i64, group_name: &str, tags: &str) -> PyResult<()> {
        let Some(_tx) = self.tx_or_report(-1)? else { return Ok(()) };
        // Refused while another request holds the subscription, as for
        // `req_pnl`, and said to the caller rather than silently taking it.
        if let Err(why) = self.core.subscribe_account_summary(req_id, tags) {
            return self.report_refusal(py, req_id, why);
        }
        let _ = group_name;
        Ok(())
    }

    /// Cancel account summary.
    fn cancel_account_summary(&self, req_id: i64) -> PyResult<()> {
        let Some(_tx) = self.tx_or_report(-1)? else { return Ok(()) };
        self.core.unsubscribe_account_summary(req_id);
        Ok(())
    }

    /// Request all positions.
    ///
    /// Before a session exists this is reported on the error callback and the
    /// call returns, as every other request made before connecting is. A
    /// program written against the reference client has no exception handling
    /// around a request, because that client does not raise there.
    fn req_positions(&self, py: Python<'_>) -> PyResult<()> {
        let Some(_connected) = self.tx_or_report(-1)? else { return Ok(()) };
        let shared = self.shared_state()?;
        // Wait for CCP init burst to complete (up to 10s).
        if !self.wait_for_the_download(py, &shared, -1)? { return Ok(()); }
        if !shared.portfolio.account_download_complete() {
            // Reported to the caller as well as the log, as on the other
            // surface. A caller reading holdings has no other way to tell a
            // truncated answer from a complete one, and an account that says
            // nothing within the wait reads exactly like one holding nothing.
            let why = "the account had not finished stating its holdings within the wait, \
                       so what follows is what this session already held rather than what \
                       the account holds";
            log::warn!("{why}");
            self.report_refusal(py, -1, Refusal::no_answer(why))?;
        }
        // A position feed names a holding by id and leaves the rest to the
        // definition, which the engine asks for as the feed is read. That
        // answer lands a moment after the account download is called complete,
        // so reading straight through hands back a holding with no symbol,
        // security type or currency on it — one a caller can neither price nor
        // close — and nothing reads it again later.
        //
        // Bounded, and only while something is still unnamed: a definition the
        // venue never sends must cost a caller a wait, not a hang.
        // The set is read inside the wait and delivered as read. Waiting on
        // one set and delivering another hands back a holding that arrives
        // between the two, which no lookup has named.
        // Watching before reading, as the other surface does and for the
        // reason its own comment gives.
        //
        // Read first and registered after, a holding that moved while the
        // answer was being assembled was taken by a watcher that already
        // existed -- the queue is drained once for everyone -- and reached
        // this caller nowhere: not in its answer, which had already been read,
        // and not afterwards, because nothing here was yet watching. The
        // window is real rather than theoretical: the naming wait below
        // releases the interpreter for up to two seconds, and every delivery
        // runs a caller's own code.
        //
        // The cost is the one that surface already accepts: the same holding
        // may be stated twice. A holding states what it is rather than what
        // changed, so a second statement of it is the same answer again.
        //
        // Not covered by a test: the window closes before this call returns,
        // so nothing a single thread can drive tells the two orderings apart.
        // Written here rather than pinned by a check that passes either way.
        self.positions_requested.store(true, Ordering::Release);
        // What moved before this answer is in the answer. Left standing, the
        // pass that hands the answer over replays every one of them as a move,
        // so a caller asking once is told about a holding twice — and the
        // comment on the dispatch beside it says this call clears them, which
        // it did not. Taken here, and given below to the per-request watchers,
        // which have not been answered and would otherwise never hear of them.
        let already_stated = shared.portfolio.drain_position_changes();
        let waited_from = std::time::Instant::now();
        let mut positions = shared.portfolio.position_infos();
        loop {
            let unnamed = positions.iter().any(|pi| {
                pi.position != 0.0
                    && pi.symbol.is_empty()
                    && self.core.get_contract(pi.con_id, &shared).is_none()
            });
            if !unnamed || waited_from.elapsed() > std::time::Duration::from_secs(2) {
                break;
            }
            py.detach(|| std::thread::sleep(std::time::Duration::from_millis(20)));
            positions = shared.portfolio.position_infos();
        }
        for pi in &positions {
            let c_py = Py::new(py, self.position_contract(py, pi, &shared)?)?.into_any();
            let avg_cost = pi.avg_cost as f64 / PRICE_SCALE_F;
            self.deliver(py, "position", (self.account().as_str(), &c_py, pi.position, avg_cost))?;
        }
        self.deliver(py, "position_end", ())?;
        // What was taken above belongs to the per-request watchers too, and
        // they were not answered here. Handed over now rather than dropped, or
        // a holding that moved before this ask would reach them never.
        let watching: Vec<i64> = {
            let asked = self.positions_multi_requested.lock().unwrap();
            let mut ids: Vec<i64> = asked.iter().copied().collect();
            ids.sort_unstable();
            ids
        };
        for pi in &already_stated {
            let c_py = Py::new(py, self.position_contract(py, pi, &shared)?)?.into_any();
            let avg_cost = pi.avg_cost as f64 / PRICE_SCALE_F;
            for req_id in &watching {
                self.deliver(
                    py,
                    "position_multi",
                    (*req_id, self.account().as_str(), "", &c_py, pi.position, avg_cost),
                )?;
            }
        }
        Ok(())
    }

    /// Cancel positions.
    // nothing to withdraw: the venue pushes what the account holds when the
    // session opens and keeps it current. Nothing was subscribed, so nothing
    // stops, and reporting an error for withdrawing a subscription that was
    // never made would be wrong.
    fn cancel_positions(&self) -> PyResult<()> {
        let Some(_tx) = self.tx_or_report(-1)? else { return Ok(()) };
        self.positions_requested.store(false, Ordering::Release);
        Ok(())
    }

    /// Request account updates.
    ///
    /// `acct_code` is taken and not applied. One session holds one account
    /// here, and the venue states its figures for that account without being
    /// asked which, so there is no second account or model portfolio to name.
    ///
    /// Subscribing also asks the venue to state the figures now. It restates
    /// them on its own schedule otherwise, which is unhurried: a session that
    /// has just opened waits tens of seconds for its first set, and a caller
    /// that subscribed and then read the account got nothing.
    #[pyo3(signature = (subscribe, _acct_code=""))]
    fn req_account_updates(&self, py: Python<'_>, subscribe: bool, _acct_code: &str) -> PyResult<()> {
        let Some(tx) = self.tx_or_report(-1)? else { return Ok(()) };
        self.core.subscribe_account_updates(subscribe);
        if subscribe {
            let account = self.account();
            if let Err(why) = Self::send_control(py, &tx, ControlCommand::RefreshAccount { account })
            {
                return self.report_refusal(py, -1, Refusal::not_connected(why.to_string()));
            }
        }
        Ok(())
    }

    /// Request managed accounts list. Answered with every account this login
    /// holds, comma separated, matching the reference client.
    ///
    /// Before a session exists there are no accounts to name, and an empty
    /// list reads as a login holding none rather than as a question asked too
    /// early.
    fn req_managed_accts(&self, py: Python<'_>) -> PyResult<()> {
        let Some(_connected) = self.tx_or_report(-1)? else { return Ok(()) };
        self.deliver(py, "managed_accounts", (self.accounts_csv().as_str(),))?;
        Ok(())
    }

    /// Request account updates for multiple accounts/models.
    ///
    /// `ledger_and_nlv` is taken and not applied. The account figures arrive as
    /// the venue states them, and it states the ledger and the net liquidation
    /// among them without being asked.
    /// The request is held open. A figure that moves after the first batch is
    /// reported again under the same number, until
    /// `cancelAccountUpdatesMulti` withdraws it — which is what the reference
    /// client does, and what a caller watching a balance sheet through this
    /// request is written for.
    #[pyo3(signature = (req_id, account, model_code, ledger_and_nlv=false))]
    fn req_account_updates_multi(
        &self, py: Python<'_>, req_id: i64, account: &str, model_code: &str, ledger_and_nlv: bool,
    ) -> PyResult<()> {
        // Reported and returned, as `req_positions` above and every other
        // request before connecting. Raising made this one request out of the
        // set the caller had to guard.
        let Some(_connected) = self.tx_or_report(req_id)? else { return Ok(()) };
        let shared = self.shared_state()?;
        let _ = ledger_and_nlv;
        // The same wait the plain answer makes, on the flag that a dropped
        // connection actually clears: the one below it was stored once at the
        // first account message of the session and cleared nowhere, so the
        // loop meant nothing after that and this path answered from the
        // pre-drop book without a warning.
        if !self.wait_for_the_download(py, &shared, req_id)? { return Ok(()); }
        if !shared.portfolio.account_download_complete() {
            let why = "the account had not finished stating its holdings within the wait, \
                       so what follows is what this session already held rather than what \
                       the account holds";
            log::warn!("{why}");
            self.report_refusal(py, req_id, Refusal::no_answer(why))?;
        }
        // A figure the venue stated about another account is not something
        // this session was told, so naming one is answered rather than taken.
        if !account.is_empty() && account != self.account() {
            let why = format!(
                "account {account} was named and the figures that follow are {}'s, which \
                 is the account this session opened under",
                self.account(),
            );
            log::warn!("{why}");
            self.report_refusal(py, req_id, Refusal::validation(why))?;
        }
        // Labelled with the account they belong to, not with the one that was
        // asked about. Echoing the caller's own put another account's name on
        // this account's net liquidation and buying power, which a caller
        // keeping a book per account files as that account's balance sheet.
        // The holdings answer beside this already says so.
        let acct_name = self.account();
        // Held open from here. The reference client keeps this request alive
        // and reports each figure again as it moves; answered with one batch
        // and nothing after, a caller watching its balance sheet through it
        // watched a still picture.
        self.account_updates_multi_requested.lock().unwrap().insert(req_id);
        self.core.forget_account_figures_stated();
        // A model is a slice of the account; the figures below are the whole
        // of it. Echoed onto the label, every one of them read as that model's
        // — and a caller keeping a book per model files the account's net
        // liquidation and buying power as one model's.
        let model_code = if model_code.is_empty() { model_code } else {
            let why = format!(
                "model {model_code} was named and the figures that follow are the whole \
                 account's, which is what this session is told",
            );
            log::warn!("{why}");
            self.report_refusal(py, req_id, Refusal::validation(why))?;
            ""
        };
        // Every figure the venue stated, in the currency it stated it in.
        // Rebuilding from this client's typed copy reports an account held in
        // any other currency as dollars, and drops every figure outside the
        // handful that copy carries.
        for field in self.core.account_figures_that_moved(&shared, true) {
            self.deliver(py, "account_update_multi",
                (req_id, acct_name.as_str(), model_code,
                 field.key.as_str(), field.value.as_str(), field.currency.as_str()))?;
        }
        self.deliver(py, "account_update_multi_end", (req_id,))?;
        Ok(())
    }

    /// Cancel multi-account updates.
    ///
    /// The request stops being reported to. The venue keeps the account current
    /// whether or not anyone is listening; what stops is the reporting — a
    /// figure that moves after this is no longer delivered on
    /// `accountUpdateMulti` for this request.
    fn cancel_account_updates_multi(&self, req_id: i64) -> PyResult<()> {
        let Some(_tx) = self.tx_or_report(-1)? else { return Ok(()) };
        self.account_updates_multi_requested.lock().unwrap().remove(&req_id);
        Ok(())
    }

    /// Request positions across multiple accounts/models.
    #[pyo3(signature = (req_id, account, model_code))]
    fn req_positions_multi(&self, py: Python<'_>, req_id: i64, account: &str, model_code: &str) -> PyResult<()> {
        // As above.
        let Some(_connected) = self.tx_or_report(req_id)? else { return Ok(()) };
        let shared = self.shared_state()?;
        // What moved before this asked is in the answer that follows, so it is
        // dropped from the queue rather than fired as a change as well. Left
        // standing, a caller whose first position call was this one was handed
        // every holding, and then handed every holding again as a move that
        // did not happen. Only where nobody was watching: a queue somebody
        // else owns is not this call's to empty. The other surface does the
        // same, in the same shape.
        if !self.positions_requested.load(Ordering::Acquire)
            && self.positions_multi_requested.lock().unwrap().is_empty()
        {
            shared.portfolio.drain_position_changes();
        }
        // The same wait and the same warning the plain answer makes, and on
        // the same flag. Waiting on whether anything had been heard at all,
        // this was satisfied by the first account figure of a new connection —
        // tens of rows before a holding was restated — so it answered from the
        // book as it stood before the drop, and said nothing about it.
        if !self.wait_for_the_download(py, &shared, req_id)? { return Ok(()); }
        if !shared.portfolio.account_download_complete() {
            let why = "the account had not finished stating its holdings within the wait, \
                       so what follows is what this session already held rather than what \
                       the account holds";
            log::warn!("{why}");
            self.report_refusal(py, req_id, Refusal::no_answer(why))?;
        }
        // Watching before reading, as on the other surface: the queue of moves
        // is drained once for everyone watching, so a holding that moves while
        // this answer is being assembled was taken by a watcher that already
        // existed and reached this one nowhere.
        self.positions_multi_requested.lock().unwrap().insert(req_id);
        let held: Vec<_> = shared.portfolio.position_infos()
            .into_iter()
            .filter(|pi| pi.position != 0.0)
            .collect();
        if !account.is_empty() && account != self.account() {
            let why = format!(
                "account {account} was named and the holdings that follow are {}'s, which \
                 is the account this session opened under",
                self.account(),
            );
            log::warn!("{why}");
            self.report_refusal(py, req_id, Refusal::validation(why))?;
        }
        // Labelled with the account they are on, not with the one that was
        // asked about: these are the holdings of the account this session
        // opened under, and echoing the caller's own put another account's
        // name on them.
        let on = self.account();
        // A model is a slice of the account, and these are the whole of what
        // it holds. Echoed onto the label, every holding read as that model's.
        let model_code = if model_code.is_empty() { model_code } else {
            let why = format!(
                "model {model_code} was named and the holdings that follow are the whole \
                 account's, which is what this session is told",
            );
            log::warn!("{why}");
            self.report_refusal(py, req_id, Refusal::validation(why))?;
            ""
        };
        for pi in &held {
            let c_py = Py::new(py, self.position_contract(py, pi, &shared)?)?.into_any();
            let avg_cost = pi.avg_cost as f64 / PRICE_SCALE_F;
            self.deliver(py, "position_multi",
                (req_id, on.as_str(), model_code, &c_py, pi.position, avg_cost))?;
        }
        self.deliver(py, "position_multi_end", (req_id,))?;
        Ok(())
    }

    /// Cancel multi-account positions.
    ///
    // nothing to withdraw: the venue pushes what the account holds and keeps
    // it current whether or not anyone is listening, as for
    // `cancel_positions`. What stops is the reporting — a holding that moves
    // after this is no longer delivered on `position_multi` for this request.
    fn cancel_positions_multi(&self, req_id: i64) -> PyResult<()> {
        let Some(_tx) = self.tx_or_report(-1)? else { return Ok(()) };
        self.positions_multi_requested.lock().unwrap().remove(&req_id);
        Ok(())
    }

    /// Read account state snapshot. Returns a dict with all account values.
    fn account_snapshot(&self) -> PyResult<Option<Py<PyAny>>> {
        let shared = match self.shared.lock().unwrap().clone() {
            Some(s) => s,
            None => return Ok(None),
        };
        // Nothing where the venue has not stated the account whole on this
        // connection. Answered from what it last said instead, a caller
        // sizing an order after a drop read the buying power and the excess
        // liquidity from before it, with nothing to say so — and gated on
        // whether anything had been heard, the first figure of the new
        // connection let the rest of the pre-drop struct through.
        if !shared.portfolio.account_download_complete() {
            return Ok(None);
        }
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
            dict.set_item("accrued_cash", acct.accrued_cash as f64 / ps)?;
            dict.set_item("margin_used", acct.margin_used as f64 / ps)?;
            dict.set_item("equity_with_loan", acct.equity_with_loan as f64 / ps)?;
            dict.set_item("cushion", acct.cushion as f64 / ps)?;
            dict.set_item("leverage", acct.leverage as f64 / ps)?;
            dict.set_item("sma", acct.sma as f64 / ps)?;
            dict.set_item("day_trades_remaining", acct.day_trades_remaining)?;
            Ok(Some(dict.into_any().unbind()))
        })
    }
}

// Not a Python method: a helper the calls above share.
impl EClient {
    /// Wait for the account to finish stating itself, up to ten seconds, and
    /// say whether the session is still there to answer — as on the other
    /// surface. The session may end inside the wait, and what this would
    /// answer from afterwards is the pre-drop book: checked at entry alone,
    /// ten seconds of that book went out under a refusal for silence.
    fn wait_for_the_download(&self, py: Python<'_>, shared: &SharedState, req_id: i64) -> PyResult<bool> {
        for _ in 0..1000 {
            if shared.portfolio.account_download_complete() { return Ok(true); }
            if shared.reference.session_over().is_some() {
                self.report_refusal(py, req_id, Refusal::not_connected("Not connected"))?;
                return Ok(false);
            }
            py.detach(|| std::thread::sleep(std::time::Duration::from_millis(10)));
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// A connected client whose engine is a channel the test reads, and a
    /// wrapper that keeps every callback it is handed.
    fn wired_client(
        py: Python<'_>,
    ) -> (EClient, std::sync::mpsc::Receiver<ControlCommand>, Py<PyAny>) {
        let client = EClient::__new__(&pyo3::types::PyTuple::empty(py), None);
        let ns = pyo3::types::PyDict::new(py);
        py.run(
            c"class W:
    def __init__(self): self.calls = []
    def __getattr__(self, name):
        return lambda *args: self.calls.append((name,) + args)
w = W()",
            None,
            Some(&ns),
        ).unwrap();
        let wrapper = ns.get_item("w").unwrap().unwrap().unbind();
        client.__init__(wrapper.clone_ref(py)).unwrap();
        let (tx, rx) = std::sync::mpsc::sync_channel(64);
        *client.shared.lock().unwrap() = Some(Arc::new(SharedState::new()));
        *client.control_tx.lock().unwrap() = Some(tx);
        *client.account_id.lock().unwrap() = Some("DU123".into());
        client.connected.store(true, Ordering::Release);
        (client, rx, wrapper)
    }

    /// A profit request naming an account this session did not open under is
    /// refused and ends there. Answered instead with this account's figures
    /// under the caller's own request number, a caller authorised on two
    /// accounts read one account's profit as the other's, with nothing on any
    /// callback to say so. Nothing is asked of the venue for a request that
    /// will not be reported, and the one subscription slot stays free for the
    /// request that comes next.
    #[test]
    fn a_profit_request_naming_another_account_is_refused_rather_than_answered() {
        Python::initialize();
        Python::attach(|py| {
            let (client, rx, wrapper) = wired_client(py);

            client.req_pnl(py, 7, "DU999", "").unwrap();
            assert!(rx.try_recv().is_err(), "the venue is asked nothing");
            assert_eq!(
                *client.core.pnl_req_id.lock().unwrap(), None,
                "the one slot there is was not taken by a request that was refused",
            );

            client.req_pnl_single(py, 8, "DU999", "", 265_598).unwrap();
            assert!(
                !client.core.pnl_single_reqs.lock().unwrap().contains_key(&8),
                "nothing is watched under a request that was refused",
            );

            // Both refusals reach the caller under the number it gave, naming
            // the account it asked about.
            let heard = wrapper.bind(py).getattr("calls").unwrap()
                .extract::<Vec<(String, i64, i64, i64, String, String)>>().unwrap();
            let refused: Vec<i64> = heard.iter()
                .filter(|(name, _, _, code, message, _)| {
                    name == "error"
                        && *code == crate::error_codes::Refusal::VALIDATION as i64
                        && message.contains("DU999")
                })
                .map(|(_, req_id, ..)| *req_id)
                .collect();
            assert_eq!(refused, vec![7, 8], "each refusal is reported once: {heard:?}");

            // And the account this session did open under is still answered,
            // in the slot the refusal did not take.
            client.req_pnl(py, 9, "DU123", "").unwrap();
            assert!(
                matches!(rx.try_recv(), Ok(ControlCommand::SubscribePnl { req_id: 9, .. })),
                "the session's own account is asked for as before",
            );
        });
    }
}
