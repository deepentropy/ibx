//! Account-related methods: positions, PnL, account summary/updates.

use std::sync::atomic::Ordering;
use crate::types::model::PRICE_SCALE_F;
use crate::api::wrapper::Wrapper;
use crate::error_codes::Refusal;
use super::dispatch::NO_REQUEST;
use crate::types::*;

use super::{Contract, EClient};

impl EClient {
    // ── Positions ──


    /// The contract a holding is in, named as fully as this client can.
    ///
    /// Preferred from the definition cache, which carries the exchange,
    /// local symbol and trading class; the feed's own fields answer while the
    /// cache is cold. Shared with the real-time path so a holding is named
    /// the same way whether it is read at the request or as it moves.
    pub(crate) fn position_contract(&self, pi: &crate::types::PositionInfo) -> Contract {
        self.core.get_contract(pi.con_id, &self.shared).unwrap_or_else(|| Contract {
            con_id: pi.con_id,
            symbol: pi.symbol.clone(),
            sec_type: pi.sec_type.clone(),
            currency: pi.currency.clone(),
            multiplier: pi.multiplier.clone(),
            ..Default::default()
        })
    }

    /// Request positions. Matches `reqPositions` in C++.
    ///
    /// Waits for the account data the venue pushes as a session opens, then
    /// delivers what it holds and calls `position_end`. The wait is bounded, and
    /// an account that says nothing within it delivers nothing — which reads the
    /// same as an account holding nothing. Said in the log rather than left to
    /// be inferred, because the two are not the same answer.
    pub fn req_positions(&self, wrapper: &mut impl Wrapper) {
        // Answered from a session that has ended, this hands back the last
        // book with nothing to say it is stale: the shutdown does not clear
        // the download flag, so the gate below passes at once. The other
        // surface refuses, and this now does too.
        if self.session_over() {
            return self.report_reason(-1, &Refusal::not_connected("Not connected"));
        }
        // Waits for the batch-end signal, not for the first holding: an account
        // with several would otherwise answer with whichever arrived first. An
        // account holding nothing is complete when the batch ends, so this does
        // not wait on rows that are not coming.
        if !self.wait_for_the_download(NO_REQUEST) { return; }
        if !self.shared.portfolio.account_download_complete() {
            // Reported to the caller as well as the log. A caller reading
            // holdings has no other way to tell a truncated answer from a
            // complete one.
            let why = "the account had not finished stating its holdings within the wait, \
                       so what follows is what this session already held rather than what \
                       the account holds";
            log::warn!("{why}");
            wrapper.error(NO_REQUEST, Refusal::NO_ANSWER as i64, why, "");
        }
        // What moved before this asked is answered by the read below, which
        // states what the holdings are now; fired as change events as well it
        // would arrive twice. Taken whether or not a watch stands, as the other
        // surface takes it: left in the queue for a standing watcher, the next
        // pass fired every holding again on `position` as well.
        let already_stated = self.shared.portfolio.drain_position_changes();
        // Watching before reading: see `req_positions_multi`. Read first and
        // registered after, a holding that moved while the answer was being
        // assembled was taken by a watcher that already existed — the queue is
        // drained once for everyone — and reached this caller nowhere.
        self.positions_requested.store(true, Ordering::Release);
        // A holding arrives as a contract id and a quantity, and its definition
        // is fetched separately. Delivering before that lands names no
        // instrument at all, so give it a moment to arrive rather than handing
        // back a position in a contract the caller cannot identify.
        // The set is read inside the wait and delivered as read. Waiting on
        // one set and delivering another hands back a holding that arrives
        // between the two, which no lookup has named.
        let mut positions = self.shared.portfolio.position_infos();
        for _ in 0..150 {
            let unnamed = positions.iter().any(|pi| {
                pi.position != 0.0
                    && pi.symbol.is_empty()
                    && self.core.get_contract(pi.con_id, &self.shared).is_none()
            });
            if !unnamed { break; }
            std::thread::sleep(std::time::Duration::from_millis(20));
            positions = self.shared.portfolio.position_infos();
        }
        for pi in &positions {
            let c = self.position_contract(pi);
            let avg_cost = pi.avg_cost as f64 / PRICE_SCALE_F;
            wrapper.position(&self.account_id, &c, pi.position, avg_cost);
        }
        wrapper.position_end();
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
            let c = self.position_contract(pi);
            let avg_cost = pi.avg_cost as f64 / PRICE_SCALE_F;
            for req_id in &watching {
                wrapper.position_multi(*req_id, &self.account_id, "", &c, pi.position, avg_cost);
            }
        }
    }

    /// Wait for the account to finish stating itself, up to ten seconds, and
    /// say whether the session is still there to answer. The session may end
    /// inside the wait, and what this would answer from afterwards is the
    /// pre-drop book: checked at entry alone, ten seconds of that book went
    /// out under a refusal for silence.
    fn wait_for_the_download(&self, req_id: i64) -> bool {
        for _ in 0..1000 {
            if self.shared.portfolio.account_download_complete() { break; }
            if self.session_over() {
                self.report_reason(req_id, &Refusal::not_connected("Not connected"));
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        true
    }

    // ── PnL ──

    /// Subscribe to account PnL updates. Matches `reqPnL` in C++.
    ///
    /// The venue is asked, under `account` or under the one this session opened
    /// with where none is named. What comes back is what each holding was worth
    /// at midnight and what it has realised since, and the figures reported on
    /// `pnl` are worked out from those against the prices the session is being
    /// told. Without the subscription none of that arrives, and the figures
    /// reduce to the unrealised part with nothing realised on any position.
    ///
    /// `model_code` is taken and not applied: there is no model portfolio to
    /// name here.
    pub fn req_pnl(&self, req_id: i64, account: &str, _model_code: &str) {
        // Before the slot is taken, as the siblings check it: taken first, a
        // refused request held the one slot there is, so the next request
        // under another number was refused as a duplicate of one that never
        // went, and the profit was reported under the refused number.
        if self.session_over() { return self.report_reason(-1, &Refusal::not_connected("Not connected")); }
        // Refused while another request holds the subscription, and nothing
        // is asked of the venue for a request that will not be reported.
        if let Err(why) = self.core.subscribe_pnl(req_id) {
            return self.report_reason(req_id, &why);
        }
        // Always the account this session opened under, whatever was named.
        //
        // The figures are worked out from one set of midnight seeds against
        // one book of holdings, and both belong to this session's account. A
        // subscription taken out for another account replaced those seeds with
        // that account's and the next restatement replaced them back, so the
        // figure reported under this request alternated between two accounts'
        // realised legs measured against a third thing -- this account's
        // positions. Named and not applied, with the caller told, as the
        // holdings answer for another account already is.
        if !account.is_empty() && account != self.account_id {
            let why = format!(
                "account {account} was named and the profit that follows is {}'s, which \
                 is the account this session opened under",
                self.account_id,
            );
            log::warn!("{why}");
            self.report_reason(req_id, &Refusal::validation(why));
        }
        let account = self.account_id.clone();
        if let Err(why) = self.send(ControlCommand::SubscribePnl { req_id, account }) {
            self.report_reason(req_id, &why);
        }
    }

    /// Cancel PnL subscription. Matches `cancelPnL` in C++.
    ///
    /// Nothing withdraws one on this wire, and the engine says so: what stops
    /// is the reporting, not the venue.
    pub fn cancel_pnl(&self, req_id: i64) {
        self.core.unsubscribe_pnl(req_id);
        if let Err(why) = self.send(ControlCommand::CancelPnl { req_id }) {
            self.report_reason(req_id, &why);
        }
    }

    /// Subscribe to single-position PnL updates. Matches `reqPnLSingle` in C++.
    ///
    /// `account` and `model_code` are taken and not applied, as on
    /// [`req_pnl`](EClient::req_pnl): the figures are for the account this
    /// session opened under, and a caller naming another is told so.
    pub fn req_pnl_single(&self, req_id: i64, account: &str, _model_code: &str, con_id: i64) {
        if self.session_over() { return self.report_reason(-1, &Refusal::not_connected("Not connected")); }
        if !account.is_empty() && account != self.account_id {
            let why = format!(
                "account {account} was named and the profit that follows is {}'s, which \
                 is the account this session opened under",
                self.account_id,
            );
            log::warn!("{why}");
            self.report_reason(req_id, &Refusal::validation(why));
        }
        self.core.subscribe_pnl_single(req_id, con_id);
    }

    /// Cancel single-position PnL subscription. Matches `cancelPnLSingle` in C++.
    pub fn cancel_pnl_single(&self, req_id: i64) {
        if self.session_over() { return self.report_reason(-1, &Refusal::not_connected("Not connected")); }
        self.core.unsubscribe_pnl_single(req_id);
    }

    // ── Account Summary ──

    /// Request account summary. Matches `reqAccountSummary` in C++.
    ///
    /// `group` is taken and not applied. One session holds one account here,
    /// and the venue states its figures for that account without being asked
    /// which, so there is no second account or model portfolio to name.
    pub fn req_account_summary(&self, req_id: i64, _group: &str, tags: &str) {
        if self.session_over() { return self.report_reason(-1, &Refusal::not_connected("Not connected")); }
        // Refused while another request holds the subscription, as for
        // `req_pnl`, and said to the caller rather than silently taking it.
        if let Err(why) = self.core.subscribe_account_summary(req_id, tags) {
            self.report_reason(req_id, &why);
        }
    }

    /// Cancel account summary. Matches `cancelAccountSummary` in C++.
    pub fn cancel_account_summary(&self, req_id: i64) {
        if self.session_over() { return self.report_reason(-1, &Refusal::not_connected("Not connected")); }
        self.core.unsubscribe_account_summary(req_id);
    }

    // ── Account Updates ──

    /// Subscribe to account updates. Matches `reqAccountUpdates` in C++.
    ///
    /// `acct_code` is taken and not applied. One session holds one account
    /// here, and the venue states its figures for that account without being
    /// asked which, so there is no second account or model portfolio to name.
    ///
    /// Subscribing also asks the venue to state the figures now. It restates
    /// them on its own schedule otherwise, which is unhurried: a session that
    /// has just opened waits tens of seconds for its first set, and a caller
    /// that subscribed and then read the account got nothing.
    pub fn req_account_updates(&self, subscribe: bool, _acct_code: &str) {
        self.core.subscribe_account_updates(subscribe);
        if subscribe {
            let account = self.account_id.clone();
            if let Err(why) = self.send(ControlCommand::RefreshAccount { account }) {
                self.report_reason(-1, &why);
            }
        }
    }

    /// Cancel positions subscription. Matches `cancelPositions` in C++.
    ///
    /// Nothing is withdrawn from the venue: it pushes what the account holds
    /// as the session opens and keeps it current whether or not anyone is
    /// listening. What stops is the reporting — a holding that moves after
    /// this is no longer delivered on `position`.
    pub fn cancel_positions(&self) {
        if self.session_over() { return self.report_reason(-1, &Refusal::not_connected("Not connected")); }
        self.positions_requested.store(false, Ordering::Release);
    }

    /// Request managed accounts. Matches `reqManagedAccts` in C++.
    ///
    /// Answered with every account this login holds, comma separated, which is
    /// the shape the reference client answers in. A login with one account is
    /// answered with that one account and no comma.
    pub fn req_managed_accts(&self, wrapper: &mut impl Wrapper) {
        if self.session_over() {
            return self.report_reason(-1, &Refusal::not_connected("Not connected"));
        }
        wrapper.managed_accounts(&self.accounts.join(","));
    }

    /// Request account updates for multiple accounts/models. Matches
    /// `reqAccountUpdatesMulti` in C++.
    /// Account values for one account or model, answered on
    /// `account_update_multi`. The reference client answers this request on
    /// its own callbacks, not on the ones `req_account_updates` uses, and a
    /// caller written against it implements those and hears nothing otherwise.
    ///
    /// `ledger_and_nlv` is taken and not applied. The account figures arrive as
    /// the venue states them, and it states the ledger and the net liquidation
    /// among them without being asked.
    ///
    /// The figures are the ones the venue states for the account this session
    /// opened under, and they are labelled with that account. A login holding
    /// several is answered for that one; naming another here does not fetch
    /// the other's figures, and is said in the log rather than answered with
    /// this account's under the other's name.
    ///
    /// A model names a slice of the account, and the venue states the account
    /// whole. Naming one is said the same way and the figures are labelled
    /// with no model, rather than the account's whole balance sheet reaching a
    /// caller as one model's.
    ///
    /// The request is held open. A figure that moves after the batch below is
    /// reported again under the same number, until
    /// [`EClient::cancel_account_updates_multi`] withdraws it — which is what
    /// the reference client does, and what a caller watching a balance sheet
    /// through this request is written for.
    pub fn req_account_updates_multi(
        &self, req_id: i64, account: &str, model_code: &str, _ledger_and_nlv: bool,
        wrapper: &mut impl Wrapper,
    ) {
        if self.session_over() {
            return self.report_reason(req_id, &Refusal::not_connected("Not connected"));
        }
        if !account.is_empty() && account != self.account_id {
            let why = format!(
                "account {account} was named and the figures that follow are {}'s, which \
                 is the account this session opened under",
                self.account_id,
            );
            log::warn!("{why}");
            wrapper.error(req_id, Refusal::VALIDATION as i64, &why, "");
        }
        // The same wait the holdings answer beside this makes, and for the
        // same reason: an account that has said nothing since the connection
        // dropped reads exactly like one holding nothing, and this path
        // answered at once from the pre-drop figures without so much as a
        // warning.
        if !self.wait_for_the_download(req_id) { return; }
        if !self.shared.portfolio.account_download_complete() {
            let why = "the account had not finished stating its figures within the wait, \
                       so what follows is what this session already held rather than what \
                       the account holds";
            log::warn!("{why}");
            wrapper.error(req_id, Refusal::NO_ANSWER as i64, why, "");
        }
        // Held open from here. The reference client keeps this request alive
        // and reports each figure again as it moves, and a caller watching its
        // balance sheet through it was given one still picture and nothing
        // after. Registered before the batch below so a figure that moves
        // while it is being assembled is not lost between the two.
        self.account_updates_multi_requested.lock().unwrap().insert(req_id);
        self.core.forget_account_figures_stated();
        // A model is a slice of the account; the figures below are the whole
        // of it. Echoed onto the label, every one of them read as that model's
        // — and a caller keeping a book per model files the account's net
        // liquidation and buying power as one model's. Said, and labelled with
        // no model, as the account is labelled with the one this session
        // opened under rather than the one that was asked about.
        let model_code = if model_code.is_empty() { model_code } else {
            let why = format!(
                "model {model_code} was named and the figures that follow are the whole \
                 account's, which is what this session is told",
            );
            log::warn!("{why}");
            wrapper.error(req_id, Refusal::VALIDATION as i64, &why, "");
            ""
        };
        // As the venue stated them, in the currency it stated them in. Eight
        // of them were worked out here instead, rounded to two decimals and
        // labelled US dollars whatever the account is held in: an account in
        // another currency read as a dollar account, and every figure the
        // venue states beyond those eight was not reported at all.
        for field in self.core.account_figures_that_moved(&self.shared, true) {
            wrapper.account_update_multi(
                req_id, &self.account_id, model_code,
                &field.key, &field.value, &field.currency,
            );
        }
        wrapper.account_update_multi_end(req_id);
    }

    /// Cancel multi-account updates. Matches `cancelAccountUpdatesMulti` in C++.
    ///
    /// The request stops being reported to. The venue keeps the account
    /// current whether or not anyone is listening, as for
    /// `cancel_account_updates`; what stops is the reporting — a figure that
    /// moves after this is no longer delivered on `account_update_multi` for
    /// this request.
    pub fn cancel_account_updates_multi(&self, req_id: i64) {
        if self.session_over() { return self.report_reason(-1, &Refusal::not_connected("Not connected")); }
        self.account_updates_multi_requested.lock().unwrap().remove(&req_id);
    }

    /// Request positions for multiple accounts/models. Matches `reqPositionsMulti` in
    /// C++.
    /// Holdings for one account or model, answered on `position_multi`.
    ///
    /// Answered from the holdings this session already has, rather than by
    /// pumping for them: pumping here would drain every queued event into a
    /// collector that reports holdings and discards the rest, so a caller
    /// running its own loop would lose whatever had arrived since it last
    /// pumped.
    pub fn req_positions_multi(
        &self, req_id: i64, account: &str, model_code: &str,
        wrapper: &mut impl Wrapper,
    ) {
        if self.session_over() {
            return self.report_reason(req_id, &Refusal::not_connected("Not connected"));
        }
        // What moved before this asked is in the answer that follows, and
        // fired as change events as well it would arrive twice. Only where
        // nobody was watching: a queue somebody else owns is not this call's
        // to empty. The other surface does the same, in the same shape.
        if !self.positions_requested.load(Ordering::Acquire)
            && self.positions_multi_requested.lock().unwrap().is_empty()
        {
            self.shared.portfolio.drain_position_changes();
        }
        // Watching before reading, so nothing moves into the gap between the
        // two. Read first and registered after, a holding that moved while the
        // answer was being assembled was taken by a watcher that already
        // existed — the queue is drained once for everyone — and this caller
        // heard of it neither in its answer nor afterwards. Registered first,
        // the worst that happens is the same holding stated twice, and a
        // holding states what it is rather than what changed.
        // The same wait the plain answer makes, for the same reason: an
        // account that has said nothing since the connection dropped reads
        // exactly like one holding nothing, and this path answered from the
        // pre-drop book without so much as a warning.
        if !self.wait_for_the_download(req_id) { return; }
        if !self.shared.portfolio.account_download_complete() {
            let why = "the account had not finished stating its holdings within the wait, \
                       so what follows is what this session already held rather than what \
                       the account holds";
            log::warn!("{why}");
            wrapper.error(req_id, Refusal::NO_ANSWER as i64, why, "");
        }
        self.positions_multi_requested.lock().unwrap().insert(req_id);
        let held: Vec<_> = self.shared.portfolio.position_infos()
            .into_iter()
            .filter(|pi| pi.position != 0.0)
            .map(|pi| {
                let contract = self.position_contract(&pi);
                (contract, pi.position, pi.avg_cost as f64 / PRICE_SCALE_F)
            })
            .collect();
        if !account.is_empty() && account != self.account_id {
            let why = format!(
                "account {account} was named and the holdings that follow are {}'s, which \
                 is the account this session opened under",
                self.account_id,
            );
            log::warn!("{why}");
            wrapper.error(req_id, Refusal::VALIDATION as i64, &why, "");
        }
        // A model is a slice of the account, and these are the whole of what
        // it holds. Echoed onto the label, every holding read as that model's.
        let model_code = if model_code.is_empty() { model_code } else {
            let why = format!(
                "model {model_code} was named and the holdings that follow are the whole \
                 account's, which is what this session is told",
            );
            log::warn!("{why}");
            wrapper.error(req_id, Refusal::VALIDATION as i64, &why, "");
            ""
        };
        // Labelled with the account they are on, not with the one that was
        // asked about: these are the holdings of the account this session
        // opened under, and echoing the caller's own put another account's
        // name on them.
        for (contract, position, avg_cost) in held {
            wrapper.position_multi(
                req_id, &self.account_id, model_code, &contract, position, avg_cost,
            );
        }
        wrapper.position_multi_end(req_id);
    }

    /// Cancel multi-account positions. Matches `cancelPositionsMulti` in C++.
    /// Stop watching holdings under this request.
    ///
    // nothing to withdraw: the venue keeps the account current whether or not
    // anyone is listening, as for `cancel_positions`. What stops is the
    // reporting — a holding that moves after this is no longer delivered on
    // `position_multi` for this request.
    pub fn cancel_positions_multi(&self, req_id: i64) {
        if self.session_over() { return self.report_reason(-1, &Refusal::not_connected("Not connected")); }
        self.positions_multi_requested.lock().unwrap().remove(&req_id);
    }

    /// Holdings the venue reports that this broker does not hold itself:
    /// positions held away at another broker, and rows it marks as shown but
    /// not held.
    ///
    /// Kept apart from `positions`, which answers what the account itself
    /// holds. The reference client has no call for these — its own front end
    /// shows them in a separate table — so this is the only way to reach them.
    pub fn positions_elsewhere(&self) -> Vec<crate::types::PositionElsewhere> {
        self.shared.portfolio.positions_elsewhere()
    }

    /// The account figures describing one of the sets of holdings the account
    /// does not hold itself, as name, value and the currency each is stated
    /// in. A figure stated in two currencies is two figures.
    ///
    /// The venue states these the same way it states the account's own, and
    /// mixing them in would overstate what the account is worth, so they are
    /// kept where the holdings they describe are kept.
    pub fn values_elsewhere(&self, held: crate::types::HeldElsewhere) -> Vec<(String, String, String)> {
        self.shared.portfolio.values_elsewhere(held)
    }

    /// The account as the venue last stated it whole, or nothing while a
    /// download is running. Answered from the struct alone, a caller read all
    /// zeros before the first download and the pre-drop figures after a drop,
    /// with nothing to say so; the other surface answers `None` for both.
    pub fn account(&self) -> Option<AccountState> {
        self.shared.portfolio.account_download_complete()
            .then(|| self.shared.portfolio.account())
    }
}
