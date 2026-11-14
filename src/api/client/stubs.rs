//! Gateway-local methods that read init data from shared state.
//! Data is populated during connection by Gateway::populate_init_data().
//! Methods that are not yet supported log a warning.

use crate::api::wrapper::Wrapper;

use super::EClient;

impl EClient {
    /// The error of a refused display group request (ibx#424).
    fn display_group_refused(&self, wrapper: &mut (impl Wrapper + ?Sized), text: &str) {
        let (id, code) = crate::client_core::DISPLAY_GROUP_REFUSAL;
        self.local_error(wrapper, id, code, text);
    }

    // ── Smart Components ──

    /// Request smart routing components for a BBO exchange. Matches `reqSmartComponents` in C++.
    /// Gateway-local, as the reference (ibx#441): the exchange map of the
    /// BBO exchange that market data made known (the `bboExchange` of
    /// `tick_req_params`); an unknown one gives error 321. When the map has
    /// not come yet, the answer comes from `process_msgs`, within 2 s.
    pub fn req_smart_components(&self, req_id: i64, bbo_exchange: &str, _wrapper: &mut impl Wrapper) {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if self.core.requests_ignored() { return; }
        // Answered here: after the requests made before it (ibx#561).
        self.shared.command_clock().take_turn();
        if !crate::client_core::ClientCore::ids_fit("req_smart_components", &[req_id]) { return; }
        match self.core.req_smart_components(req_id, bbo_exchange, &self.shared) {
            // Given by `process_msgs` at its place among the answers (ibx#532).
            Some(Ok(components)) => self.later(move |_, wrapper| wrapper.smart_components(req_id, &components)),
            Some(Err((code, msg))) => self.later(move |this, wrapper| this.local_error(wrapper, req_id, code, &msg)),
            None => {}
        }
    }

    // ── News Providers ──

    /// Request available news providers. Matches `reqNewsProviders` in C++.
    /// Gateway-local — returns provider list from init data.
    pub fn req_news_providers(&self, _wrapper: &mut impl Wrapper) {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if self.core.requests_ignored() { return; }
        // Answered here: after the requests made before it (ibx#561).
        self.shared.command_clock().take_turn();
        // Given by `process_msgs` at its place among the answers (ibx#532).
        self.later(|this, wrapper| wrapper.news_providers(&this.shared.reference.news_providers()));
    }

    // ── Server Time ──

    /// Request current server time. Matches `reqCurrentTime` in C++.
    /// Answered locally, as the reference: the local clock plus the
    /// offset to the server clock of the logon (ibx#421).
    pub fn req_current_time(&self, _wrapper: &mut impl Wrapper) {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if self.core.requests_ignored() { return; }
        // Paced like every request: on the reference a current time
        // request made in a burst was answered at its turn (run of
        // 10/10/2026, ibx#561).
        self.shared.command_clock().take_turn();
        // Given by `process_msgs` at its place among the answers (ibx#532).
        self.later(|this, wrapper| wrapper.current_time(this.shared.reference.server_time_secs()));
    }

    /// Request current server time in milliseconds. Matches
    /// `reqCurrentTimeInMillis` in C++. Answered locally from the clock of
    /// `req_current_time` (ibx#516).
    pub fn req_current_time_in_millis(&self, _wrapper: &mut impl Wrapper) {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if self.core.requests_ignored() { return; }
        // As `req_current_time` (ibx#561).
        self.shared.command_clock().take_turn();
        // Given by `process_msgs` at its place among the answers (ibx#532).
        self.later(|this, wrapper| wrapper.current_time_in_millis(this.shared.reference.server_time_millis()));
    }

    // ── Requests a session of the reference does not have (ibx#516) ──

    /// Exercise or lapse an option. Matches `exerciseOptions` in C++.
    /// Nothing is sent yet: the request counts for the pacing and is
    /// logged, as in the Python client.
    pub fn exercise_options(
        &self, req_id: i64, contract: &crate::api::types::Contract, exercise_action: i32,
        exercise_quantity: i32, account: &str, override_: i32,
    ) {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if self.core.requests_ignored() { return; }
        let _ = (req_id, contract, exercise_action, exercise_quantity, account, override_);
        log::warn!("exercise_options: not yet implemented in engine");
    }

    /// Matches `cancelContractData` in C++. Below session level 215, error
    /// 503 for the request; from 215 nothing (ibx#557). A running contract
    /// details request goes on in both cases, as on the reference.
    pub fn cancel_contract_data(&self, req_id: i64) {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if self.core.requests_ignored() { return; }
        if let Some((code, text)) = crate::client_core::lookup_cancel_refusal(crate::client_core::session_level(), false) {
            self.shared.orders.push_order_error(req_id, code, text.to_string());
        }
    }

    /// Matches `cancelHistoricalTicks` in C++. Below session level 215,
    /// error 503 for the request; from 215 nothing (ibx#557). A running
    /// historical ticks request goes on in both cases, as on the reference.
    pub fn cancel_historical_ticks(&self, req_id: i64) {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if self.core.requests_ignored() { return; }
        if let Some((code, text)) = crate::client_core::lookup_cancel_refusal(crate::client_core::session_level(), true) {
            self.shared.orders.push_order_error(req_id, code, text.to_string());
        }
    }

    /// The intent to verify, as the official client's `extraAuth`: set
    /// before the connect, the verify requests are sent (ibx#556).
    pub fn set_extra_auth(&self, on: bool) {
        self.core.set_extra_auth(on);
    }

    /// Matches `verifyRequest` in C++. A session opened without the intent
    /// to verify: error 544, nothing sent. With it, sent: the session then
    /// answers no more request, as the reference 10.51 (ibx#556).
    pub fn verify_request(&self, _api_name: &str, _api_version: &str) {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if self.core.requests_ignored() { return; }
        if self.core.extra_auth() {
            self.core.verify_sent();
            return;
        }
        let (id, code, text) = crate::client_core::VERIFY_REQUEST_NO_INTENT;
        self.shared.orders.push_order_error(id, code, text.to_string());
    }

    /// Matches `verifyMessage` in C++. No answer, and the session answers
    /// no more request, as the reference 10.51 (ibx#556); error 10095 as
    /// the reference 10.40 in its replays.
    pub fn verify_message(&self, _api_data: &str) {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if self.core.requests_ignored() { return; }
        if self.shared.reference.form_10_40() {
            let (id, code, text) = crate::client_core::VERIFY_MESSAGE_OUT_OF_SEQUENCE;
            self.shared.orders.push_order_error(id, code, text.to_string());
            return;
        }
        self.core.verify_sent();
    }

    /// Matches `verifyAndAuthRequest` in C++. A session opened without the
    /// intent to verify: error 551, nothing sent. With it, sent: the session
    /// then answers no more request, as the reference 10.51 (ibx#556).
    pub fn verify_and_auth_request(&self, _api_name: &str, _api_version: &str, _opaque_isv_key: &str) {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if self.core.requests_ignored() { return; }
        if self.core.extra_auth() {
            self.core.verify_sent();
            return;
        }
        let (id, code, text) = crate::client_core::VERIFY_AND_AUTH_REQUEST_NO_INTENT;
        self.shared.orders.push_order_error(id, code, text.to_string());
    }

    /// Matches `verifyAndAuthMessage` in C++. No answer, and the session
    /// answers no more request, as the reference 10.51 (ibx#556).
    pub fn verify_and_auth_message(&self, _api_data: &str, _xyz_response: &str) {
        if self.core.requests_ignored() || self.shared.reference.form_10_40() { return; }
        self.core.verify_sent();
    }

    // ── Configuration ──

    /// Request the configuration. Matches `reqConfigProtoBuf` in C++
    /// (session level 219); the answer is `config_response`, the paper
    /// installation's configuration with the changes kept from
    /// `update_config` (ibx#610). Below level 219, error 503 for the
    /// request, as the official client library answers, and nothing sent.
    /// From 219 a logon that forbids configuration access gets 321.
    pub fn req_config(&self, request: &crate::api::config::ConfigRequest) {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if self.core.requests_ignored() { return; }
        self.req_config_at(crate::client_core::session_level(), request);
    }

    /// `req_config` on a session of level `level`.
    pub(crate) fn req_config_at(&self, level: i32, request: &crate::api::config::ConfigRequest) {
        match self.core.config_answer(level, request, &self.shared.reference.logon_features()) {
            Err((id, code, text)) => self.shared.orders.push_order_error(id, code, text.to_string()),
            Ok(response) => self.later(move |_, wrapper| wrapper.config_response(&response)),
        }
    }

    /// Update the configuration. Matches `updateConfigProtoBuf` in C++
    /// (session level 221); the answer is `update_config_response`. Below
    /// level 221, error 503 for the request, as the official client library
    /// answers, and nothing sent (ibx#612). From 221 a logon that forbids
    /// configuration access gets 321; otherwise the update is checked as
    /// the reference checks it, then a value ibx cannot follow is refused
    /// with the field named, and the rest is kept (decision of 18/10/2026
    /// on ibx#610).
    pub fn update_config(&self, request: &crate::api::config::UpdateConfigRequest) {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if self.core.requests_ignored() { return; }
        self.update_config_at(crate::client_core::session_level(), request);
    }

    /// `update_config` on a session of level `level`.
    pub(crate) fn update_config_at(&self, level: i32, request: &crate::api::config::UpdateConfigRequest) {
        match self.core.update_config_answer(level, request, &self.shared.reference.logon_features()) {
            Err((id, code, text)) => self.shared.orders.push_order_error(id, code, text.to_string()),
            Ok(response) => self.later(move |_, wrapper| wrapper.update_config_response(&response)),
        }
    }

    // ── FA (Financial Advisor) ──

    /// Request FA data. On a session that is not FA, error 321 as the
    /// reference (ibx#481). On an FA session, `receive_fa` from the
    /// session's FA data, or the reference's errors (ibx#507).
    pub fn request_fa(&self, fa_data_type: i32) {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if self.core.requests_ignored() { return; }
        if !self.shared.reference.fa_session() {
            let (id, code, text) = crate::client_core::REQUEST_FA_NOT_FA;
            self.shared.orders.push_order_error(id, code, text.to_string());
            return;
        }
        // Answered here: after the requests made before it (ibx#561).
        self.shared.command_clock().take_turn();
        match crate::client_core::fa_request(&self.shared, fa_data_type) {
            Ok(xml) => self.later(move |_, wrapper| wrapper.receive_fa(fa_data_type, &xml)),
            Err((id, code, text)) => self.shared.orders.push_order_error(id, code, text),
        }
    }

    /// Replace FA data. On a session that is not FA, error 321 for the
    /// request as the reference (ibx#481). On an FA session, the document
    /// is checked, the session's data replaced and saved to the server;
    /// `replace_fa_end` when the saves are answered, or the reference's
    /// errors (ibx#507).
    pub fn replace_fa(&self, req_id: i64, fa_data_type: i32, cxml: &str) {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if self.core.requests_ignored() { return; }
        if !crate::client_core::ClientCore::ids_fit("replace_fa", &[req_id]) { return; }
        if !self.shared.reference.fa_session() {
            let (code, text) = crate::client_core::REPLACE_FA_NOT_FA;
            self.shared.orders.push_order_error(req_id, code, text.to_string());
            return;
        }
        match crate::client_core::fa_replace(&self.shared, req_id, fa_data_type, cxml) {
            Ok(Some(save)) => { let _ = self.send(save); }
            Ok(None) => {}
            Err((code, text)) => self.shared.orders.push_order_error(req_id, code, text),
        }
    }

    // ── Display Groups ──

    /// Query display groups. Matches `queryDisplayGroups` in C++.
    /// Gateway-local, as the reference (ibx#424): the fixed list of groups.
    pub fn query_display_groups(&self, req_id: i64, _wrapper: &mut impl Wrapper) {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if self.core.requests_ignored() { return; }
        // Answered here: after the requests made before it (ibx#561).
        self.shared.command_clock().take_turn();
        if !crate::client_core::ClientCore::ids_fit("query_display_groups", &[req_id]) { return; }
        let answer = crate::client_core::ClientCore::query_display_groups(req_id);
        // Given by `process_msgs` at its place among the answers (ibx#532).
        self.later(move |this, wrapper| match answer {
            Ok(groups) => wrapper.display_group_list(req_id, groups),
            Err(text) => this.display_group_refused(wrapper, &text),
        });
    }

    /// Subscribe to display group events. Matches `subscribeToGroupEvents`
    /// in C++. Gateway-local, as the reference (ibx#424): the contract of
    /// the group at once, `none` since no group has one; error 321 for a
    /// group outside 1 to 7 or a request id already subscribed.
    pub fn subscribe_to_group_events(&self, req_id: i64, group_id: i32, _wrapper: &mut impl Wrapper) {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if self.core.requests_ignored() { return; }
        // Answered here: after the requests made before it (ibx#561).
        self.shared.command_clock().take_turn();
        if !crate::client_core::ClientCore::ids_fit("subscribe_to_group_events", &[req_id]) { return; }
        let answer = self.core.subscribe_to_group_events(req_id, group_id);
        // Given by `process_msgs` at its place among the answers (ibx#532).
        self.later(move |this, wrapper| match answer {
            Ok(contract_info) => wrapper.display_group_updated(req_id, contract_info),
            Err(text) => this.display_group_refused(wrapper, &text),
        });
    }

    /// Unsubscribe from display group events. Matches
    /// `unsubscribeFromGroupEvents` in C++. No answer, but error 321 for a
    /// request id that is not subscribed, as the reference (ibx#424).
    pub fn unsubscribe_from_group_events(&self, req_id: i64) {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if self.core.requests_ignored() { return; }
        if !crate::client_core::ClientCore::ids_fit("unsubscribe_from_group_events", &[req_id]) { return; }
        if let Some(text) = self.core.unsubscribe_from_group_events(req_id) {
            let (id, code) = crate::client_core::DISPLAY_GROUP_REFUSAL;
            self.shared.orders.push_order_error(id, code, text);
        }
    }

    /// Update display group. Matches `updateDisplayGroup` in C++. As the
    /// reference (ibx#424): error 321 for bad input or a request id that is
    /// not subscribed, error 473 for a conId that is not a contract, and
    /// no answer for a valid update, which changes no group.
    pub fn update_display_group(&self, req_id: i64, contract_info: &str) {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if self.core.requests_ignored() { return; }
        use crate::client_core::DisplayGroupUpdate;
        if !crate::client_core::ClientCore::ids_fit("update_display_group", &[req_id]) { return; }
        let known = |con_id| self.shared.reference.get_contract(con_id).is_some();
        match self.core.update_display_group(req_id, contract_info, known) {
            DisplayGroupUpdate::Nothing => {}
            DisplayGroupUpdate::Refused(text) => {
                let (id, code) = crate::client_core::DISPLAY_GROUP_REFUSAL;
                self.shared.orders.push_order_error(id, code, text);
            }
            DisplayGroupUpdate::Lookup(con_id) => {
                let _ = self.send(crate::types::ControlCommand::DisplayGroupLookup { req_id, con_id });
            }
        }
    }

    // ── Soft Dollar Tiers ──

    /// Request soft dollar tiers. Matches `reqSoftDollarTiers` in C++.
    /// Gateway-local — returns tiers parsed from the CCP logon's soft dollar tier field, none
    /// when the logon has no tiers (ibx#480).
    pub fn req_soft_dollar_tiers(&self, req_id: i64, _wrapper: &mut impl Wrapper) {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if self.core.requests_ignored() { return; }
        // Answered here: after the requests made before it (ibx#561).
        self.shared.command_clock().take_turn();
        if !crate::client_core::ClientCore::ids_fit("req_soft_dollar_tiers", &[req_id]) { return; }
        // Given by `process_msgs` at its place among the answers (ibx#532).
        self.later(move |this, wrapper| wrapper.soft_dollar_tiers(req_id, &this.shared.reference.soft_dollar_tiers()));
    }

    // ── Family Codes ──

    /// Request family codes. Matches `reqFamilyCodes` in C++.
    /// Gateway-local — returns codes parsed from the CCP logon's family code field.
    pub fn req_family_codes(&self, _wrapper: &mut impl Wrapper) {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if self.core.requests_ignored() { return; }
        // Answered here: after the requests made before it (ibx#561).
        self.shared.command_clock().take_turn();
        // Given by `process_msgs` at its place among the answers (ibx#532).
        self.later(|this, wrapper| wrapper.family_codes(&this.shared.reference.family_codes()));
    }

    // ── Server Log Level ──

    /// Set server log level. Matches `setServerLogLevel` in C++.
    pub fn set_server_log_level(&self, log_level: i32) {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if self.core.requests_ignored() { return; }
        let level = match log_level {
            1 => "error",
            2 => "warn",
            3 => "info",
            4 => "debug",
            5 => "trace",
            _ => "warn",
        };
        log::info!("set_server_log_level: {} (level {})", level, log_level);
    }

    // ── User Info ──

    /// Request user info. Matches `reqUserInfo` in C++.
    /// Gateway-local — returns whiteBrandingId from CCP logon.
    pub fn req_user_info(&self, req_id: i64, _wrapper: &mut impl Wrapper) {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if self.core.requests_ignored() { return; }
        // Answered here: after the requests made before it (ibx#561).
        self.shared.command_clock().take_turn();
        if !crate::client_core::ClientCore::ids_fit("req_user_info", &[req_id]) { return; }
        // Given by `process_msgs` at its place among the answers (ibx#532).
        self.later(move |this, wrapper| wrapper.user_info(req_id, &this.shared.reference.white_branding_id()));
    }

    // ── WSH ──

    /// Request WSH meta data. Matches `reqWshMetaData` in C++. As the
    /// reference (ibx#443): error 10276 when the session has no WSH news
    /// source, 10277 when it is not subscribed; the meta data held is given
    /// at once; 10278 while a request is on its way; else the request goes
    /// to the server and its answer comes as `wsh_meta_data`.
    pub fn req_wsh_meta_data(&self, req_id: i64) {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if self.core.requests_ignored() { return; }
        if !crate::client_core::ClientCore::ids_fit("req_wsh_meta_data", &[req_id]) { return; }
        let step = self.core.wsh_meta_request(&self.shared, req_id);
        self.wsh_step(req_id, true, step);
    }

    /// Cancel a WSH meta data request. Matches `cancelWshMetaData` in
    /// C++. The request on its way is let go; no answer, as the reference
    /// (ibx#443).
    pub fn cancel_wsh_meta_data(&self, _req_id: i64) {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        self.core.wsh_cancel(&self.shared);
    }

    /// Request WSH event data. Matches `reqWshEventData` in C++. As the
    /// reference (ibx#443): the permission errors; 10282 before the meta
    /// data is held; 10309 unless exactly one of a contract and a filter
    /// is given; 10283 for a filter that is not a JSON object; nothing at
    /// all for a date that is not `yyyyMMdd`; 10281 while a request is on
    /// its way; else the events come as `wsh_event_data`.
    pub fn req_wsh_event_data(&self, req_id: i64, wsh_event_data: &crate::api::types::WshEventData) {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if self.core.requests_ignored() { return; }
        if !crate::client_core::ClientCore::ids_fit("req_wsh_event_data", &[req_id]) { return; }
        let d = wsh_event_data;
        let request = crate::control::wsh::EventRequest {
            con_id: d.con_id, filter: d.filter.clone(), fill_watchlist: d.fill_watchlist,
            fill_portfolio: d.fill_portfolio, fill_competitors: d.fill_competitors,
            start_date: d.start_date.clone(), end_date: d.end_date.clone(), total_limit: d.total_limit,
        };
        let step = self.core.wsh_event_request(&self.shared, req_id, &request);
        self.wsh_step(req_id, false, step);
    }

    /// Cancel a WSH event data request. Matches `cancelWshEventData` in
    /// C++. As the reference (ibx#443): the meta data request on its way is
    /// let go, the event request goes on; no answer.
    pub fn cancel_wsh_event_data(&self, _req_id: i64) {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        self.core.wsh_cancel(&self.shared);
    }

    /// The outcome of a calendar data request (ibx#443).
    fn wsh_step(&self, req_id: i64, meta: bool, step: crate::client_core::WshStep) {
        use crate::client_core::WshStep;
        match step {
            WshStep::Refused(code, text) => self.shared.orders.push_order_error(req_id, code, text),
            WshStep::Answer(json) => self.later(move |_, wrapper| wrapper.wsh_meta_data(req_id, &json)),
            WshStep::Send(body) => {
                let _ = self.control_tx.send(crate::types::ControlCommand::WshRequest { req_id, meta, body });
            }
            WshStep::Nothing => {}
        }
    }
}
