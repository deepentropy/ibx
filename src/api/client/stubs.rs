//! Gateway-local methods that read init data from shared state.
//! Data is populated during connection by Gateway::populate_init_data.
//! Methods that are not yet supported log a warning.

use crate::api::wrapper::Wrapper;
use crate::error_codes::{LOG_LEVEL_INVALID, Refusal};

use super::EClient;


impl EClient {
    // ── Smart Components ──

    /// Request smart routing components for a BBO exchange. Matches
    /// `reqSmartComponents` in C++.
    /// Gateway-local — returns component exchanges from init data.
    ///
    /// `bbo_exchange` is taken and not applied. The venue states one table of
    /// routing components at logon, for this session rather than per exchange,
    /// and that whole table is what comes back.
    pub fn req_smart_components(&self, req_id: i64, _bbo_exchange: &str, wrapper: &mut impl Wrapper) {
        if self.session_over() { return wrapper.error(-1, Refusal::NOT_CONNECTED as i64, "Not connected", ""); }
        let components = self.shared.reference.smart_components();
        wrapper.smart_components(req_id, &components);
    }

    // ── News Providers ──

    /// Request available news providers. Matches `reqNewsProviders` in C++.
    /// Gateway-local — returns provider list from init data.
    pub fn req_news_providers(&self, wrapper: &mut impl Wrapper) {
        if self.session_over() { return wrapper.error(-1, Refusal::NOT_CONNECTED as i64, "Not connected", ""); }
        let providers = self.shared.reference.news_providers();
        wrapper.news_providers(&providers);
    }

    // ── Server Time ──

    /// The venue's clock, as `reqCurrentTime` reports it.
    ///
    /// The venue is never asked. There is no request for this on the wire, so
    /// the answer is worked out here: this machine's clock, shifted by what
    /// the venue has stated about its own — on the logon it stamps, and in the
    /// clock it pushes afterwards. A session that has been told nothing is
    /// shifted by nothing and answers this machine's clock, which is what a
    /// caller who asks before the venue has said anything gets. This is a
    /// question that always has an answer, and never a refusal.
    pub fn req_current_time(&self, wrapper: &mut impl Wrapper) {
        if self.session_over() { return wrapper.error(-1, Refusal::NOT_CONNECTED as i64, "Not connected", ""); }
        wrapper.current_time(self.shared.market.venue_time_millis().div_euclid(1_000));
    }

    /// The venue's clock in milliseconds, as `reqCurrentTimeInMillis` reports it.
    ///
    /// The same clock [`req_current_time`](Self::req_current_time) reports and
    /// worked out the same way. What differs is the precision kept: asking in
    /// seconds throws away the fraction this one keeps.
    pub fn req_current_time_in_millis(&self, wrapper: &mut impl Wrapper) {
        if self.session_over() { return wrapper.error(-1, Refusal::NOT_CONNECTED as i64, "Not connected", ""); }
        wrapper.current_time_in_millis(self.shared.market.venue_time_millis());
    }

    // ── FA (Financial Advisor) ──

    /// Ask the venue for a partition of the advisor's own configuration.
    ///
    /// The reference client names the partition by a number — its aliases, its
    /// groups, its allocation profiles — and the venue names it by a word, so
    /// the number is turned into the word it stands for. A number that stands
    /// for nothing is refused rather than sent as an empty partition.
    ///
    /// The venue's answer reaches [`Wrapper::receive_fa`] under the same
    /// number the partition was asked for by.
    pub fn request_fa(&self, fa_data_type: i32) -> Result<(), Refusal> {
        let partition = advisor_partition(fa_data_type)
            .ok_or_else(|| format!("no advisor configuration is named by {fa_data_type}"))?;
        self.send(crate::types::ControlCommand::AdvisorConfig {
            // Nothing to carry back: the answer to a question about a
            // partition names the partition, not a request.
            req_id: -1,
            // Asking for it by name.
            command: 5,
            partition: partition.to_string(),
            fa_data_type,
            document: None,
        })
    }

    /// Replace a partition of the advisor's configuration with the one given.
    ///
    /// [`Wrapper::replace_fa_end`] fires with `req_id` once the venue has
    /// taken it, and a venue that refuses states why on [`Wrapper::error`]
    /// under the same number.
    pub fn replace_fa(&self, req_id: i64, fa_data_type: i32, cxml: &str) -> Result<(), Refusal> {
        let partition = advisor_partition(fa_data_type)
            .ok_or_else(|| format!("no advisor configuration is named by {fa_data_type}"))?;
        self.send(crate::types::ControlCommand::AdvisorConfig {
            req_id,
            // Replacing it with what is carried.
            command: 3,
            partition: partition.to_string(),
            fa_data_type,
            document: Some(cxml.to_string()),
        })
    }

    // ── Option calculations ──
    //
    // A volatility inverted from a price, and a price implied by a volatility.
    // This protocol carries no request for either: nothing it sends takes a
    // caller-supplied option price or volatility for the venue to work back
    // from. They exist so a caller written against the reference client finds
    // the call and is told why it cannot be served, rather than finding
    // nothing at all.

    /// What volatility a price implies, under the venue's model.
    ///
    /// This protocol carries no request for it, so the value is computed
    /// here, anchored to the venue's last stated model output for this
    /// contract. Where the venue has stated no model, nothing is answered rather
    /// than a number derived from an unstated rate.
    ///
    /// Answered over a year, which is the scale `tick_option_computation`
    /// reports the venue's own volatility on, so the two read against each
    /// other.
    pub fn calculate_implied_volatility(
        &self, req_id: i64, contract: &super::Contract,
        option_price: f64, under_price: f64,
    ) {
        match self.solve_option(contract, None, |terms, model, schedule| {
            crate::control::option_model::implied_volatility(
                terms, model, schedule, option_price, under_price,
            )
        }) {
            Ok(volatility) => self.shared.market.push_option_computation(
                crate::types::OptionComputation {
                    implied_vol: volatility,
                    opt_price: option_price,
                    und_price: under_price,
                    ..crate::types::OptionComputation::solved(req_id)
                },
            ),
            // The venue states a model for a contract that is watched. Asking
            // about one nobody is watching opens the watch and answers when
            // the model arrives, which is what the caller asked for — rather
            // than refusing the question for having been asked first.
            //
            // Only where that is the trouble. A model already stated and a
            // question it cannot answer is not something waiting will fix,
            // and kept anyway the caller was given neither an answer nor a
            // reason and waited on a model that had already arrived.
            Err(why) if why.message == crate::client_core::OPTION_MODEL_UNSTATED => {
                if let Err(why) = self.watch_for_option_model(
                    req_id, contract, true, option_price, under_price,
                ) {
                    self.report_reason(req_id, &why);
                }
            }
            Err(why) => self.report_reason(req_id, &why),
        }
    }

    /// What price a volatility implies, under that same model.
    pub fn calculate_option_price(
        &self, req_id: i64, contract: &super::Contract,
        volatility: f64, under_price: f64,
    ) {
        match self.solve_option(contract, None, |terms, model, schedule| {
            crate::control::option_model::option_price(
                terms, model, schedule, volatility, under_price,
            )
        }) {
            Ok(price) => self.shared.market.push_option_computation(
                crate::types::OptionComputation {
                    implied_vol: volatility,
                    opt_price: price,
                    und_price: under_price,
                    ..crate::types::OptionComputation::solved(req_id)
                },
            ),
            // As above: the watch is opened where the model has not been
            // stated, and the answer follows it. Where it has, and the
            // question still cannot be answered, that is said.
            Err(why) if why.message == crate::client_core::OPTION_MODEL_UNSTATED => {
                if let Err(why) = self.watch_for_option_model(
                    req_id, contract, false, volatility, under_price,
                ) {
                    self.report_reason(req_id, &why);
                }
            }
            Err(why) => self.report_reason(req_id, &why),
        }
    }

    /// Answer a kept implied-volatility question, if the venue has stated a
    /// model by now. Answers whether it did.
    pub(crate) fn solve_and_push_volatility(
        &self, req_id: i64, calc: &super::PendingOptionCalc,
    ) -> bool {
        let (opt, und) = (calc.option_price, calc.under_price);
        match self.solve_option(&calc.contract, Some(req_id), |terms, model, schedule| {
            crate::control::option_model::implied_volatility(terms, model, schedule, opt, und)
        }) {
            Ok(volatility) => {
                self.shared.market.push_option_computation(crate::types::OptionComputation {
                    implied_vol: volatility,
                    opt_price: opt,
                    und_price: und,
                    ..crate::types::OptionComputation::solved(req_id)
                });
                true
            }
            // Only one of the refusals here resolves by waiting: the one
            // saying the venue has not stated its model yet. The rest are
            // permanent — no expiry to measure from, a contract the venue
            // priced on a model this does not solve with, a price no
            // volatility reproduces — and read as "not yet" they leave the
            // question kept for the life of the session, re-solved on every
            // pass, with the caller told neither an answer nor a reason. The
            // first call already tells them apart; this is where it was not.
            Err(why) if why.message == crate::client_core::OPTION_MODEL_UNSTATED => false,
            Err(why) => {
                self.report_reason(req_id, &why);
                true
            }
        }
    }

    /// Answer a kept option-price question, if the venue has stated a model by
    /// now. Answers whether it did.
    pub(crate) fn solve_and_push_price(
        &self, req_id: i64, calc: &super::PendingOptionCalc,
    ) -> bool {
        let (vol, und) = (calc.option_price, calc.under_price);
        match self.solve_option(&calc.contract, Some(req_id), |terms, model, schedule| {
            crate::control::option_model::option_price(terms, model, schedule, vol, und)
        }) {
            Ok(price) => {
                self.shared.market.push_option_computation(crate::types::OptionComputation {
                    implied_vol: vol,
                    opt_price: price,
                    und_price: und,
                    ..crate::types::OptionComputation::solved(req_id)
                });
                true
            }
            // Only one of the refusals here resolves by waiting: the one
            // saying the venue has not stated its model yet. The rest are
            // permanent — no expiry to measure from, a contract the venue
            // priced on a model this does not solve with, a price no
            // volatility reproduces — and read as "not yet" they leave the
            // question kept for the life of the session, re-solved on every
            // pass, with the caller told neither an answer nor a reason. The
            // first call already tells them apart; this is where it was not.
            Err(why) if why.message == crate::client_core::OPTION_MODEL_UNSTATED => false,
            Err(why) => {
                self.report_reason(req_id, &why);
                true
            }
        }
    }

    /// Watch a contract so the venue states a model for it, and keep the
    /// question until it does.
    ///
    /// A watch that cannot be opened returns its refusal to the caller.
    fn watch_for_option_model(
        &self, req_id: i64, contract: &super::Contract,
        wants_volatility: bool, option_price: f64, under_price: f64,
    ) -> Result<(), Refusal> {
        // Each question holds the watch under its own request. Market data
        // shares an existing subscription and keeps it up until its last
        // watcher withdraws, including where the venue resolves a description.
        self.req_mkt_data(req_id, contract, "", false, false)?;
        self.pending_option_calcs.lock().unwrap().insert(req_id, super::PendingOptionCalc {
            contract: contract.clone(),
            wants_volatility,
            option_price,
            under_price,
            answered: false,
        });
        Ok(())
    }

    /// The contract's terms and the venue's model for it, or why neither
    /// question can be answered.
    fn solve_option(
        &self,
        contract: &super::Contract,
        watched_under: Option<i64>,
        solve: impl Fn(
            crate::control::option_model::OptionTerms,
            crate::control::option_model::VenueModel,
            &[(f64, f64)],
        ) -> Option<f64>,
    ) -> Result<f64, Refusal> {
        self.core.solve_option(&self.shared, contract, watched_under, solve)
    }

    /// Withdraw a question that was waiting on the venue to state a model.
    ///
    /// A question answered from a model already stated started nothing and
    /// stops nothing. One that opened a watch to get an answer withdraws it
    /// here, so a caller that changes its mind is not left watching a
    /// contract it no longer asks about.
    pub fn cancel_calculate_implied_volatility(&self, req_id: i64) {
        self.forget_option_calc(req_id);
    }

    /// As for [`cancel_calculate_implied_volatility`](Self::cancel_calculate_implied_volatility).
    pub fn cancel_calculate_option_price(&self, req_id: i64) {
        self.forget_option_calc(req_id);
    }

    /// Drop a kept question and the watch it opened.
    fn forget_option_calc(&self, req_id: i64) {
        let gone = self.pending_option_calcs.lock().unwrap().remove(&req_id);
        if gone.is_some() {
            let _ = self.cancel_mkt_data(req_id);
        }
    }

    // ── Display Groups ──

    /// Query display groups. Not yet implemented.
    /// The display groups on offer. Answered on `display_group_list`.
    ///
    /// A display group is a way for several callers on one session to agree on
    /// a contract. Nothing about one crosses this wire, so they are kept here
    /// and served to callers from here.
    pub fn query_display_groups(&self, req_id: i64) {
        if self.session_over() { return self.report_reason(-1, &Refusal::not_connected("Not connected")); }
        self.core.query_display_groups(req_id);
    }

    /// Follow a display group. Answered on `display_group_updated`, at once
    /// with what the group holds and again whenever it changes.
    pub fn subscribe_to_group_events(&self, req_id: i64, group_id: i32) {
        if self.session_over() { return self.report_reason(-1, &Refusal::not_connected("Not connected")); }
        self.core.subscribe_to_group_events(req_id, group_id);
    }

    /// Stop following a display group.
    pub fn unsubscribe_from_group_events(&self, req_id: i64) {
        if self.session_over() { return self.report_reason(-1, &Refusal::not_connected("Not connected")); }
        self.core.unsubscribe_from_group_events(req_id);
    }

    /// Put a contract in the group this request follows, stated as
    /// `conId@exchange`, or `none` to empty it. Every follower of that group is
    /// told, including this one.
    pub fn update_display_group(&self, req_id: i64, contract_info: &str) -> Result<(), Refusal> {
        self.core.update_display_group(req_id, contract_info)
            .map_err(Refusal::from)
    }

    // ── Soft Dollar Tiers ──

    /// Request soft dollar tiers. Matches `reqSoftDollarTiers` in C++.
    /// Gateway-local — returns tiers parsed from CCP logon tag 6560.
    pub fn req_soft_dollar_tiers(&self, req_id: i64, wrapper: &mut impl Wrapper) {
        if self.session_over() { return wrapper.error(-1, Refusal::NOT_CONNECTED as i64, "Not connected", ""); }
        let tiers = self.shared.reference.soft_dollar_tiers();
        wrapper.soft_dollar_tiers(req_id, &tiers);
    }

    // ── Family Codes ──

    /// Request family codes. Matches `reqFamilyCodes` in C++.
    /// Gateway-local — returns codes parsed from CCP logon tag 6823.
    pub fn req_family_codes(&self, wrapper: &mut impl Wrapper) {
        if self.session_over() { return wrapper.error(-1, Refusal::NOT_CONNECTED as i64, "Not connected", ""); }
        let codes = self.shared.reference.family_codes();
        wrapper.family_codes(&codes);
    }

    // ── Server Log Level ──

    /// Set server log level. Matches `setServerLogLevel` in C++.
    ///
    /// Taken and not applied. The session holds no log level of its own and this
    /// protocol carries no message asking the venue to change one, so what a
    /// caller states here is written to this client's log and nothing else.
    /// This client's own logging is set where the process sets it, through
    /// `IBX_LOG_LEVEL` or `RUST_LOG`.
    pub fn set_server_log_level(&self, log_level: i32) {
        let level = match log_level {
            1 => "error",
            2 => "warn",
            3 => "info",
            4 => "debug",
            5 => "trace",
            // Refused rather than substituted. Reading a level nobody asked
            // for as `warn` told the caller nothing and left them believing
            // they had set the level they named.
            _ => {
                return self.report_reason(crate::bridge::ReferenceState::NO_REQUEST as i64, &Refusal::stated(
                    LOG_LEVEL_INVALID,
                    format!("set_server_log_level: {log_level} is not a log level; it is 1 to 5"),
                ));
            }
        };
        // The level of the logger this client installed, moved where there is
        // one to move. Nothing goes to the venue: this protocol carries no
        // message asking one to change how loudly it talks, and a level a
        // caller states is about the thing serving that caller — which, on a
        // client that runs in the caller's own process, is this library.
        if crate::logging::set_level(level) {
            log::info!("set_server_log_level: logging at {level} (level {log_level})");
            return;
        }
        // A program that installed its own logger keeps it, and saying the
        // level moved when it did not is worse than saying it did not.
        self.report_reason(crate::bridge::ReferenceState::NO_REQUEST as i64, &Refusal::stated(
            LOG_LEVEL_INVALID,
            format!(
                "set_server_log_level: {level} was not applied because this session did \
                 not install the logger; whoever did holds the level"
            ),
        ));
    }

    // ── User Info ──

    /// Request user info. Matches `reqUserInfo` in C++.
    /// Gateway-local — returns whiteBrandingId from CCP logon.
    pub fn req_user_info(&self, req_id: i64, wrapper: &mut impl Wrapper) {
        if self.session_over() { return wrapper.error(-1, Refusal::NOT_CONNECTED as i64, "Not connected", ""); }
        let id = self.shared.reference.white_branding_id();
        wrapper.user_info(req_id, &id);
    }

    /// A request this client cannot serve is answered, not ignored. A caller
    /// waiting on a callback that will never come cannot tell that apart from
    /// a slow gateway, so it is told on the channel a venue uses to say it
    /// will not act on a request.
    pub(crate) fn report_reason(&self, req_id: i64, reason: &Refusal) {
        self.shared.reference.push_historical_error(
            super::carried_under(req_id), reason.code, reason.message.clone(),
        );
    }
}


/// The word the venue names an advisor's configuration partition by, from the
/// number the reference client names it by.
fn advisor_partition(fa_data_type: i32) -> Option<&'static str> {
    // The order the venue reads them in. Rotated by one here, every
    // advisor request asked for a different partition than the caller named:
    // a request for groups returned aliases, and one for aliases returned
    // nothing the caller could use.
    match fa_data_type {
        1 => Some("Group"),
        2 => Some("Profile"),
        3 => Some("Aliases"),
        _ => None,
    }
}




#[cfg(test)]
mod server_clock_tests {
    use crate::api::client::tests::test_client;
    use crate::api::Wrapper;

    #[derive(Default)]
    struct Heard { seconds: Vec<i64>, millis: Vec<i64>, errors: Vec<i64> }
    impl Wrapper for Heard {
        fn current_time(&mut self, t: i64) { self.seconds.push(t); }
        fn current_time_in_millis(&mut self, t: i64) { self.millis.push(t); }
        fn error(&mut self, _req_id: i64, code: i64, _msg: &str, _: &str) {
            self.errors.push(code);
        }
    }

    fn local_millis() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64
    }

    /// A session that has heard nothing from the venue about its clock still
    /// answers, from this machine's.
    ///
    /// The venue is never asked what time it is, so there is nothing this
    /// question waits on and nothing to refuse over. Refused, a caller on a
    /// live session was handed a failure for a call that cannot fail — and one
    /// carrying the code that means there is no session at all.
    #[test]
    fn a_clock_the_venue_has_not_stated_is_answered_not_refused() {
        let (client, _rx, _shared) = test_client();
        let mut heard = Heard::default();

        client.req_current_time(&mut heard);
        client.req_current_time_in_millis(&mut heard);

        assert!(heard.errors.is_empty(), "nothing is refused: {:?}", heard.errors);
        assert!(
            (heard.seconds[0] - local_millis().div_euclid(1_000)).abs() <= 1,
            "this machine's clock, unshifted",
        );
        assert!((heard.millis[0] - local_millis()).abs() < 1_000, "and the same in milliseconds");
    }

    /// What the venue has stated shifts the answer, and the answer goes on
    /// running while the venue says nothing more.
    ///
    /// Answered with the stamp on the last message that happened to arrive,
    /// it stood still on a quiet connection: two readings a moment apart named
    /// the same instant, so a caller measuring the difference between the
    /// clocks watched it grow by exactly the time it had waited.
    #[test]
    fn a_stated_clock_shifts_the_answer_and_keeps_running() {
        let (client, _rx, shared) = test_client();
        let mut heard = Heard::default();

        shared.market.note_venue_time("20260815-12:00:00");
        client.req_current_time(&mut heard);
        client.req_current_time_in_millis(&mut heard);
        assert!(
            (heard.millis[0] - 1_786_795_200_000).abs() < 2_000,
            "the venue's clock, days from this machine's: {:?}", heard.millis,
        );
        assert_eq!(heard.seconds[0], heard.millis[0].div_euclid(1_000), "the same clock");

        std::thread::sleep(std::time::Duration::from_millis(5));
        client.req_current_time_in_millis(&mut heard);
        assert!(heard.millis[1] > heard.millis[0], "and it ran on unasked");
    }
}

#[cfg(test)]
mod advisor_partition_tests {
    use super::advisor_partition;

    /// The reference client names a partition by a number and the venue names
    /// it by a word. Both clients here send the word, and a number that
    /// stands for nothing is refused rather than sent as an empty partition.
    ///
    /// The order the venue reads: one names the group, two the
    /// profile, three the aliases. Rotated by one, every advisor request asked
    /// for a partition the caller had not named, and this test agreed with it.
    #[test]
    fn each_number_names_the_partition_the_venue_knows() {
        assert_eq!(advisor_partition(1), Some("Group"));
        assert_eq!(advisor_partition(2), Some("Profile"));
        assert_eq!(advisor_partition(3), Some("Aliases"));
    }

    #[test]
    fn a_number_that_names_nothing_is_refused() {
        for unknown in [0, 4, -1, i32::MAX] {
            assert_eq!(advisor_partition(unknown), None, "{unknown} was taken");
        }
    }
}

#[cfg(test)]
mod expiry_tests {
    use crate::client_core::years_to_expiry;
    use crate::protocol::datetime::days_from_civil;

    /// A known date, against a known day count. Written out rather than pulled
    /// in, so it is checked rather than trusted.
    #[test]
    fn a_civil_date_counts_the_days_since_the_epoch() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11017);
        assert_eq!(days_from_civil(2026, 1, 1), 20454);
    }

    /// An expiry already past is no expiry to measure to.
    #[test]
    fn an_expiry_in_the_past_measures_nothing() {
        assert!(years_to_expiry("19990101").is_none());
        assert!(years_to_expiry("").is_none());
        assert!(years_to_expiry("2026").is_none());
    }

    /// One ahead measures the years between, and a longer one measures more.
    #[test]
    fn an_expiry_ahead_measures_the_years_between() {
        let near = years_to_expiry("20301231").expect("a date ahead");
        let far = years_to_expiry("20351231").expect("a date further ahead");
        assert!(near > 0.0 && far > near, "{near} then {far}");
    }

    /// An expiry that cannot exist measures nothing. The day count is
    /// arithmetic and would place a thirteenth month or a thirty-second day
    /// somewhere regardless, so a solve measuring from it would answer from a
    /// day the venue never stated.
    #[test]
    fn an_impossible_expiry_measures_nothing() {
        assert!(years_to_expiry("20301301").is_none(), "a thirteenth month");
        assert!(years_to_expiry("20300001").is_none(), "a zeroth month");
        assert!(years_to_expiry("20300132").is_none(), "a thirty-second day");
        assert!(years_to_expiry("20300100").is_none(), "a zeroth day");
        assert!(years_to_expiry("20310229").is_none(), "February the 29th on a common year");
        assert!(years_to_expiry("21000229").is_none(), "February the 29th on a century year");
        // The calendar edge that must not be refused: leap day where one is.
        assert!(years_to_expiry("20320229").is_some(), "February the 29th on a leap year");
    }

    fn option_contract(con_id: i64, symbol: &str) -> crate::types::model::Contract {
        crate::types::model::Contract {
            con_id,
            symbol: symbol.into(),
            sec_type: "OPT".into(),
            exchange: "SMART".into(),
            currency: "USD".into(),
            last_trade_date_or_contract_month: "20301220".into(),
            strike: 100.0,
            right: "C".into(),
            ..Default::default()
        }
    }

    fn answer_option_watch(
        rx: &std::sync::mpsc::Receiver<crate::types::ControlCommand>,
        instrument: crate::types::InstrumentId,
    ) {
        use crate::types::ControlCommand;
        let wait = std::time::Duration::from_secs(5);
        assert!(matches!(rx.recv_timeout(wait), Ok(ControlCommand::RegisterInstrument { .. })));
        match rx.recv_timeout(wait).expect("the subscription") {
            ControlCommand::Subscribe { reply_tx: Some(reply), .. } => {
                let _ = reply.send(Ok(instrument));
            }
            other => panic!("expected a subscription, got {other:?}"),
        }
    }

    /// Questions on one contract share its subscription, and either order of
    /// withdrawal takes it down only when the last question goes.
    #[test]
    fn the_last_option_calculation_withdraws_the_shared_watch() {
        use crate::api::client::tests::test_client;
        use crate::types::ControlCommand;

        for [first, last] in [[1, 2], [2, 1]] {
            let (client, rx, shared) = test_client();
            client.core.set_registration_timeout(std::time::Duration::from_secs(5));
            let option = option_contract(756733, "SPY");
            std::thread::scope(|scope| {
                let asking = scope.spawn(|| {
                    client.calculate_implied_volatility(1, &option, 5.0, 100.0);
                });
                answer_option_watch(&rx, 0);
                asking.join().unwrap();
            });
            client.calculate_implied_volatility(2, &option, 5.0, 100.0);
            assert_eq!(client.pending_option_calcs.lock().unwrap().len(), 2);
            assert!(shared.reference.drain_historical_errors().is_empty());
            assert!(rx.try_recv().is_err(), "the second question shares the watch");

            client.cancel_calculate_implied_volatility(first);
            assert!(rx.try_recv().is_err(), "the remaining question still needs the model");
            assert!(!client.core.instrument_to_req.lock().unwrap().is_empty());

            client.cancel_calculate_implied_volatility(last);
            assert!(
                matches!(rx.try_recv(), Ok(ControlCommand::Unsubscribe { instrument: 0, .. })),
                "the last withdrawal leaves the subscription running",
            );
            assert!(client.pending_option_calcs.lock().unwrap().is_empty());
            assert!(!client.core.holds_mkt_data(1));
            assert!(!client.core.holds_mkt_data(2));
            assert!(client.core.instrument_to_req.lock().unwrap().is_empty());
            assert!(rx.try_recv().is_err(), "the watch is withdrawn once");
        }
    }

    /// Descriptions carry no conId, so their zeroes say nothing about whether
    /// two questions share a watch. The engine's subscriptions say which goes.
    #[test]
    fn described_option_calculations_withdraw_their_own_watches() {
        use crate::api::client::tests::test_client;
        use crate::types::ControlCommand;

        let (client, rx, shared) = test_client();
        client.core.set_registration_timeout(std::time::Duration::from_secs(5));
        for (req_id, symbol, instrument) in [(1, "SPY", 0), (2, "QQQ", 1)] {
            let option = option_contract(0, symbol);
            std::thread::scope(|scope| {
                let asking = scope.spawn(|| {
                    client.calculate_option_price(req_id, &option, 0.2, 100.0);
                });
                answer_option_watch(&rx, instrument);
                asking.join().unwrap();
            });
        }
        assert_eq!(client.pending_option_calcs.lock().unwrap().len(), 2);
        assert!(shared.reference.drain_historical_errors().is_empty());

        client.cancel_calculate_option_price(1);
        assert!(
            matches!(rx.try_recv(), Ok(ControlCommand::Unsubscribe { instrument: 0, .. })),
            "another description's zero conId keeps this watch running",
        );
        assert!(!client.core.holds_mkt_data(1));
        assert!(client.core.holds_mkt_data(2), "the other description remains watched");

        client.cancel_calculate_option_price(2);
        assert!(matches!(rx.try_recv(), Ok(ControlCommand::Unsubscribe { instrument: 1, .. })));
        assert!(client.pending_option_calcs.lock().unwrap().is_empty());
        assert!(!client.core.holds_mkt_data(2));
        assert!(client.core.instrument_to_req.lock().unwrap().is_empty());
        assert!(rx.try_recv().is_err(), "each watch is withdrawn once");
    }

    /// A number already watching another contract needs a different number,
    /// and waiting for an option model cannot make that number available.
    #[test]
    fn option_calculations_report_the_duplicate_watch_refusal() {
        use crate::api::client::tests::test_client;
        use crate::error_codes::DUPLICATE_TICKER_ID;
        use crate::types::ControlCommand;

        for wants_volatility in [true, false] {
            let (client, rx, shared) = test_client();
            client.core.set_registration_timeout(std::time::Duration::from_secs(5));
            let watched = option_contract(756733, "SPY");
            std::thread::scope(|scope| {
                let asking = scope.spawn(|| client.req_mkt_data(7, &watched, "", false, false));
                answer_option_watch(&rx, 0);
                asking.join().unwrap().unwrap();
            });
            let other = option_contract(0, "QQQ");
            let refused = client.req_mkt_data(7, &other, "", false, false).unwrap_err();
            assert_eq!(refused.code, DUPLICATE_TICKER_ID);

            if wants_volatility {
                client.calculate_implied_volatility(7, &other, 5.0, 100.0);
            } else {
                client.calculate_option_price(7, &other, 0.2, 100.0);
            }
            assert_eq!(
                shared.reference.drain_historical_errors(),
                vec![(7, refused.code, refused.message)],
                "the caller needs the watch's refusal, not a reason to wait for a model",
            );
            assert!(client.pending_option_calcs.lock().unwrap().is_empty());
            client.cancel_calculate_implied_volatility(7);
            client.cancel_calculate_option_price(7);
            assert!(client.core.holds_mkt_data(7), "a refused question owns no watch to withdraw");
            assert!(rx.try_recv().is_err(), "the original watch stays up");
            client.cancel_mkt_data(7).unwrap();
            assert!(matches!(rx.try_recv(), Ok(ControlCommand::Unsubscribe { instrument: 0, .. })));
        }
    }

    fn answer_empty_contract_lookup(
        rx: &std::sync::mpsc::Receiver<crate::types::ControlCommand>,
        shared: &crate::bridge::SharedState,
    ) {
        match rx.recv_timeout(std::time::Duration::from_secs(5)).expect("the contract lookup") {
            crate::types::ControlCommand::FetchContractDetails { req_id, .. } => {
                shared.reference.push_contract_details_end(req_id);
            }
            other => panic!("expected a contract lookup, got {other:?}"),
        }
    }

    /// A contract the venue cannot name cannot be watched, so the question
    /// reports that refusal under its own number.
    #[test]
    fn option_calculations_report_the_qualification_refusal() {
        use crate::api::client::tests::test_client;
        use crate::error_codes::Refusal;

        for wants_volatility in [true, false] {
            let (client, rx, shared) = test_client();
            let mut option = option_contract(756734, "QQQ");
            option.sec_type.clear();
            let refused = std::thread::scope(|scope| {
                let asking = scope.spawn(|| client.req_mkt_data(7, &option, "", false, false));
                answer_empty_contract_lookup(&rx, &shared);
                asking.join().unwrap().unwrap_err()
            });
            assert_eq!(refused.code, Refusal::NO_DEFINITION);

            std::thread::scope(|scope| {
                let asking = scope.spawn(|| {
                    if wants_volatility {
                        client.calculate_implied_volatility(7, &option, 5.0, 100.0);
                    } else {
                        client.calculate_option_price(7, &option, 0.2, 100.0);
                    }
                });
                answer_empty_contract_lookup(&rx, &shared);
                asking.join().unwrap();
            });
            assert_eq!(
                shared.reference.drain_historical_errors(),
                vec![(7, refused.code, refused.message)],
                "the caller needs the qualification refusal, not a reason to wait for a model",
            );
            assert!(client.pending_option_calcs.lock().unwrap().is_empty());
            assert!(!client.core.holds_mkt_data(7));
            assert!(rx.try_recv().is_err(), "no subscription follows a refused qualification");
        }
    }
}
