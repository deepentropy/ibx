//! Gateway-local methods that read init data from shared state.
//! Data is populated during connection by Gateway::populate_init_data().
//! Methods that are not yet supported log a warning.

use crate::api::wrapper::Wrapper;

use super::EClient;

impl EClient {
    /// The error of a refused display group request (ibx#424).
    fn display_group_refused(&self, wrapper: &mut impl Wrapper, text: &str) {
        let (id, code) = crate::client_core::DISPLAY_GROUP_REFUSAL;
        self.local_error(wrapper, id, code, text);
    }

    // ── Smart Components ──

    /// Request smart routing components for a BBO exchange. Matches `reqSmartComponents` in C++.
    /// Gateway-local, as the reference (ibx#441): the exchange map of the
    /// BBO exchange that market data made known (the `bboExchange` of
    /// `tick_req_params`); an unknown one gives error 321. When the map has
    /// not come yet, the answer comes from `process_msgs`, within 2 s.
    pub fn req_smart_components(&self, req_id: i64, bbo_exchange: &str, wrapper: &mut impl Wrapper) {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        // Answered here: after the requests made before it (ibx#561).
        self.shared.command_clock().take_turn();
        if !crate::client_core::ClientCore::ids_fit("req_smart_components", &[req_id]) { return; }
        match self.core.req_smart_components(req_id, bbo_exchange, &self.shared) {
            Some(Ok(components)) => wrapper.smart_components(req_id, &components),
            Some(Err((code, msg))) => self.local_error(wrapper, req_id, code, &msg),
            None => {}
        }
    }

    // ── News Providers ──

    /// Request available news providers. Matches `reqNewsProviders` in C++.
    /// Gateway-local — returns provider list from init data.
    pub fn req_news_providers(&self, wrapper: &mut impl Wrapper) {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        // Answered here: after the requests made before it (ibx#561).
        self.shared.command_clock().take_turn();
        let providers = self.shared.reference.news_providers();
        wrapper.news_providers(&providers);
    }

    // ── Server Time ──

    /// Request current server time. Matches `reqCurrentTime` in C++.
    /// Answered locally, as the reference: the local clock plus the
    /// offset to the server clock of the logon (ibx#421).
    pub fn req_current_time(&self, wrapper: &mut impl Wrapper) {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        // Paced like every request: on the reference a current time
        // request made in a burst was answered at its turn (run of
        // 10/10/2026, ibx#561).
        self.shared.command_clock().take_turn();
        wrapper.current_time(self.shared.reference.server_time_secs());
    }

    /// Request current server time in milliseconds. Matches
    /// `reqCurrentTimeInMillis` in C++. Answered locally from the clock of
    /// `req_current_time` (ibx#516).
    pub fn req_current_time_in_millis(&self, wrapper: &mut impl Wrapper) {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        // As `req_current_time` (ibx#561).
        self.shared.command_clock().take_turn();
        wrapper.current_time_in_millis(self.shared.reference.server_time_millis());
    }

    // ── Requests a session of the reference does not have (ibx#516) ──

    /// Matches `cancelContractData` in C++. Not supported by a session of
    /// the reference: error 503 for the request, and a running contract
    /// details request goes on.
    pub fn cancel_contract_data(&self, req_id: i64) {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        let (code, text) = crate::client_core::CANCEL_CONTRACT_DATA_UNSUPPORTED;
        self.shared.orders.push_order_error(req_id, code, text.to_string());
    }

    /// Matches `cancelHistoricalTicks` in C++. Not supported by a session
    /// of the reference: error 503 for the request, and a running
    /// historical ticks request goes on.
    pub fn cancel_historical_ticks(&self, req_id: i64) {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        let (code, text) = crate::client_core::CANCEL_HISTORICAL_TICKS_UNSUPPORTED;
        self.shared.orders.push_order_error(req_id, code, text.to_string());
    }

    /// Matches `verifyRequest` in C++. A session opened without the intent
    /// to verify: error 544, nothing sent.
    pub fn verify_request(&self, _api_name: &str, _api_version: &str) {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        let (id, code, text) = crate::client_core::VERIFY_REQUEST_NO_INTENT;
        self.shared.orders.push_order_error(id, code, text.to_string());
    }

    /// Matches `verifyMessage` in C++. No verification is in progress:
    /// error 10095 as the reference.
    pub fn verify_message(&self, _api_data: &str) {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        let (id, code, text) = crate::client_core::VERIFY_MESSAGE_OUT_OF_SEQUENCE;
        self.shared.orders.push_order_error(id, code, text.to_string());
    }

    /// Matches `verifyAndAuthRequest` in C++. A session opened without the
    /// intent to verify: error 551, nothing sent.
    pub fn verify_and_auth_request(&self, _api_name: &str, _api_version: &str, _opaque_isv_key: &str) {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        let (id, code, text) = crate::client_core::VERIFY_AND_AUTH_REQUEST_NO_INTENT;
        self.shared.orders.push_order_error(id, code, text.to_string());
    }

    /// Matches `verifyAndAuthMessage` in C++. The reference gives no
    /// answer: nothing happens.
    pub fn verify_and_auth_message(&self, _api_data: &str, _xyz_response: &str) {}

    // ── FA (Financial Advisor) ──

    /// Request FA data. On a session that is not FA, error 321 as the
    /// reference (ibx#481); the FA data exchange itself is not implemented.
    pub fn request_fa(&self, _fa_data_type: i32) {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if !self.shared.reference.fa_session() {
            let (id, code, text) = crate::client_core::REQUEST_FA_NOT_FA;
            self.shared.orders.push_order_error(id, code, text.to_string());
            return;
        }
        log::warn!("request_fa: not yet implemented — needs FIX capture");
    }

    /// Replace FA data. On a session that is not FA, error 321 for the
    /// request as the reference (ibx#481); the FA data exchange itself is
    /// not implemented.
    pub fn replace_fa(&self, req_id: i64, _fa_data_type: i32, _cxml: &str) {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if !crate::client_core::ClientCore::ids_fit("replace_fa", &[req_id]) { return; }
        if !self.shared.reference.fa_session() {
            let (code, text) = crate::client_core::REPLACE_FA_NOT_FA;
            self.shared.orders.push_order_error(req_id, code, text.to_string());
            return;
        }
        log::warn!("replace_fa: not yet implemented — needs FIX capture");
    }

    // ── Display Groups ──

    /// Query display groups. Matches `queryDisplayGroups` in C++.
    /// Gateway-local, as the reference (ibx#424): the fixed list of groups.
    pub fn query_display_groups(&self, req_id: i64, wrapper: &mut impl Wrapper) {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        // Answered here: after the requests made before it (ibx#561).
        self.shared.command_clock().take_turn();
        if !crate::client_core::ClientCore::ids_fit("query_display_groups", &[req_id]) { return; }
        match crate::client_core::ClientCore::query_display_groups(req_id) {
            Ok(groups) => wrapper.display_group_list(req_id, groups),
            Err(text) => self.display_group_refused(wrapper, &text),
        }
    }

    /// Subscribe to display group events. Matches `subscribeToGroupEvents`
    /// in C++. Gateway-local, as the reference (ibx#424): the contract of
    /// the group at once, `none` since no group has one; error 321 for a
    /// group outside 1 to 7 or a request id already subscribed.
    pub fn subscribe_to_group_events(&self, req_id: i64, group_id: i32, wrapper: &mut impl Wrapper) {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        // Answered here: after the requests made before it (ibx#561).
        self.shared.command_clock().take_turn();
        if !crate::client_core::ClientCore::ids_fit("subscribe_to_group_events", &[req_id]) { return; }
        match self.core.subscribe_to_group_events(req_id, group_id) {
            Ok(contract_info) => wrapper.display_group_updated(req_id, contract_info),
            Err(text) => self.display_group_refused(wrapper, &text),
        }
    }

    /// Unsubscribe from display group events. Matches
    /// `unsubscribeFromGroupEvents` in C++. No answer, but error 321 for a
    /// request id that is not subscribed, as the reference (ibx#424).
    pub fn unsubscribe_from_group_events(&self, req_id: i64) {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
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
    /// Gateway-local — returns tiers parsed from CCP logon tag 6522, none
    /// when the logon has no tiers (ibx#480).
    pub fn req_soft_dollar_tiers(&self, req_id: i64, wrapper: &mut impl Wrapper) {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        // Answered here: after the requests made before it (ibx#561).
        self.shared.command_clock().take_turn();
        if !crate::client_core::ClientCore::ids_fit("req_soft_dollar_tiers", &[req_id]) { return; }
        let tiers = self.shared.reference.soft_dollar_tiers();
        wrapper.soft_dollar_tiers(req_id, &tiers);
    }

    // ── Family Codes ──

    /// Request family codes. Matches `reqFamilyCodes` in C++.
    /// Gateway-local — returns codes parsed from CCP logon tag 6823.
    pub fn req_family_codes(&self, wrapper: &mut impl Wrapper) {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        // Answered here: after the requests made before it (ibx#561).
        self.shared.command_clock().take_turn();
        let codes = self.shared.reference.family_codes();
        wrapper.family_codes(&codes);
    }

    // ── Server Log Level ──

    /// Set server log level. Matches `setServerLogLevel` in C++.
    pub fn set_server_log_level(&self, log_level: i32) {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
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
    pub fn req_user_info(&self, req_id: i64, wrapper: &mut impl Wrapper) {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        // Answered here: after the requests made before it (ibx#561).
        self.shared.command_clock().take_turn();
        if !crate::client_core::ClientCore::ids_fit("req_user_info", &[req_id]) { return; }
        let id = self.shared.reference.white_branding_id();
        wrapper.user_info(req_id, &id);
    }

    // ── WSH ──

    /// Request WSH meta data. Matches `reqWshMetaData` in C++. The
    /// permission check of the reference (ibx#443): error 10276 when the
    /// session has no WSH news source, 10277 when it is not subscribed.
    /// The data request itself is not implemented: with the permission,
    /// error 10279.
    pub fn req_wsh_meta_data(&self, req_id: i64) {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if !crate::client_core::ClientCore::ids_fit("req_wsh_meta_data", &[req_id]) { return; }
        let (code, text) = crate::client_core::wsh_meta_data_error(&self.shared.reference);
        self.shared.orders.push_order_error(req_id, code, text.to_string());
    }

    /// Cancel a WSH meta data request. Matches `cancelWshMetaData` in
    /// C++. No answer, as the reference (ibx#443).
    pub fn cancel_wsh_meta_data(&self, _req_id: i64) {}

    /// Request WSH event data. Matches `reqWshEventData` in C++. The
    /// permission check of the reference (ibx#443): error 10276 when the
    /// session has no WSH news source, 10277 when it is not subscribed.
    /// The data request itself is not implemented: with the permission,
    /// error 10282, since no meta data is held.
    pub fn req_wsh_event_data(&self, req_id: i64, _wsh_event_data: &crate::api::types::WshEventData) {
        // One API request for the pacing, whatever it sends (ibx#565).
        let _request = crate::engine::park::one_request();
        if !crate::client_core::ClientCore::ids_fit("req_wsh_event_data", &[req_id]) { return; }
        let (code, text) = crate::client_core::wsh_event_data_error(&self.shared.reference);
        self.shared.orders.push_order_error(req_id, code, text.to_string());
    }

    /// Cancel a WSH event data request. Matches `cancelWshEventData` in
    /// C++. No answer, as the reference (ibx#443).
    pub fn cancel_wsh_event_data(&self, _req_id: i64) {}
}
