//! The configuration a session gives to reqConfig and keeps across
//! updateConfig (session levels 219 and 221; ibx#610, ibx#612), as the
//! project manager decided on 18/10/2026 (option 3 of ibx#610):
//!
//! - reqConfig gives the configuration of the reference's paper
//!   installation observed on 15/10/2026 (paper, market closed, fresh
//!   process, the official client at level 225), with the values ibx
//!   follows where they differ (its two precaution switches of ibx#486 and
//!   ibx#608);
//! - an update that passes the reference's checks
//!   ([`update_config_check`]) is taken when it changes a setting that
//!   means nothing without the reference's own socket or installation
//!   (port, encoding, logs, bulk timeout, trusted addresses, the order
//!   reject display, the message switches), or sets a value to the one ibx
//!   already follows; the new values are kept and a later reqConfig shows
//!   them. A change to a value ibx cannot follow is answered "error" with
//!   the field named, and nothing is changed;
//! - acting on the settings ibx could follow is later work (ibx#747).

use super::update_config::{update_config_check, ConfigRules, MessageRule, STATUS_ERROR, VALIDATION_FAILED};
use crate::api::config::*;

pub const STATUS_SUCCESS: &str = "success";
pub const CONFIG_UPDATED: &str = "Configuration updated";

/// The messages of the paper installation, in the order reqConfig gives
/// them (15/10/2026): (title, text, default action, id, enabled, listed
/// while enabled, disabling needs its warning accepted). A message that is
/// not listed while enabled (118) is listed because it is disabled.
#[allow(clippy::type_complexity)]
const PAPER_MESSAGES: [(&str, Option<&str>, &str, i32, bool, bool, bool); 22] = [
    ("Confirm auto cancel date", None, "Hidden", 118, false, false, false),
    ("Confirm Mandatory Cap Price", None, "Hidden", 131, true, true, true),
    ("Cross side warning", Some("The closing order quantity is greater than your current position."), "Hidden", 432, true, true, false),
    ("Market Order Confirmation", Some("Market Order Confirmation"), "Yes", 444, true, true, true),
    ("Stop Variant Order Confirmation", Some("Stop Variant Order Confirmation"), "Yes", 445, true, true, true),
    ("Cash Quantity Order Warning", Some("Cash Quantity Order Warning"), "Yes", 449, true, true, true),
    ("Group Allocation Warning", Some("This order will be distributed over multiple accounts.<br>We strongly suggest you familiarize yourself with our allocation<br> facilities at the link below before submitting orders."), "Yes", 458, true, true, true),
    ("Cash Quantity Order Confirmation", Some("Cash Quantity Order Confirmation"), "Yes", 530, true, true, true),
    ("API does not support trading in fractions for CashQty orders.", Some("API does not support trading in fractions for CashQty orders."), "Hidden", 537, true, true, false),
    ("Fractions trading no support outside of regular trading hours.", Some("This instrument does not support trading in fractions on %s outside of regular trading hours."), "Hidden", 580, true, true, false),
    ("Order using account belonging to pair of linked hedge fund accounts", Some("Warning: Please select %s to trade using %s"), "Hidden", 586, true, true, false),
    ("Order using Group/Profile containing some linked hedge fund accounts", Some("Warning: %s contains accounts: %s that require all other accounts to be added from: %s.\nPlease use Advisor Setup to edit the list of accounts and try again.\nYou may select %s to trade using %s."), "Hidden", 589, true, true, false),
    ("Cryptocurrency order confirmation for API", Some("Cryptocurrency order confirmation"), "Yes", 605, false, true, true),
    ("Cryptocurrency Market Order Confirmation", Some("Cryptocurrency Market Order Confirmation"), "Yes", 606, true, true, true),
    ("Cryptocurrency volatility warning for API", Some("OSL Digital Securities Limited is a Hong Kong Securities and Futures Commission (SFC)-licensed digital asset platform.\nSpot cryptocurrencies are volatile and you may lose all your investments."), "Yes", 616, true, true, true),
    ("This order will be directly routed to %s. Direct routed orders may result in higher trade fees.", Some("This order will be directly routed to %s. Direct routed orders may result in higher trade fees."), "Hidden", 621, true, true, false),
    ("Prompt to preserve sorted column on Orders panel.", Some("<html>The Orders Page is now in sorted view, which sorts and displays our orders based on the order column you clicked.&nbsp<br>Do you want to preserve sorted column on restart?</html>"), "Yes", 645, true, true, true),
    ("This order will be directly routed to %s.", Some("This order will be directly routed to %s."), "Hidden", 675, true, true, false),
    ("Model Offsetting Position Warning", Some("Checking Model offsetting position for this order..."), "Hidden", 710, true, true, false),
    ("Allocation Preset Mismatch Warning", Some("<html>Please review Allowed Allocation as they may be different from Group Allocation because of your current Allocation Preferences - <br/> %s. <br/> You can adjust that in Advisor Setup - <a href='tws://preset'>Preset</a>. <a href='%s'>Click here</a> for more details."), "Hidden", 723, true, true, false),
    ("Allocation Order Tool Promotion", Some("<html>You may use <a href='tws://alloc_order_tool'>Allocation Order Tool</a> to customize per account quantity for this order.</html>"), "Hidden", 728, true, true, false),
    ("Max Hedging Size Warning", Some("Warning: Max Hedging Size value %s set on the beta hedge order is lower than the system estimate of %s."), "Hidden", 731, true, true, false),
];

/// One message of the configuration.
#[derive(Clone, Debug)]
struct Message {
    config: MessageConfig,
    /// Listed while enabled.
    exposed: bool,
    /// Disabling it needs its warning accepted.
    warning: bool,
}

/// The zone the times given to a client are written in, the session's
/// `sendInstrumentTimezone` (ibx#747; the reference 10.51, paper, regular
/// hours 13/11/2026, the same requests under each value): the zone of the
/// contract with its name (`20261113 09:43:55 US/Eastern`), UTC
/// (`20261113-14:43:55`, an execution's time with a final `Z`), or the
/// zone of the machine with its name (`20261113 15:43:55 Europe/Paris`).
/// It changes the times of executions, of completed orders, of the bars
/// shorter than a day asked in the text form, and the two times of a bar
/// request's end; not a bar's date, the Unix form, a head timestamp, the
/// trading hours of contract details, nor an order's own dates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeZoneShown {
    Instrument,
    Utc,
    Operator,
}

/// `text`, a time written `yyyyMMdd HH:mm:ss {zone}`, as `shown` writes
/// it; `z` for the final `Z` of an execution's time in UTC. None when the
/// text is kept: the instrument's zone, or a text of another form (a
/// date, Unix seconds, an empty time).
pub fn time_shown(text: &str, shown: TimeZoneShown, z: bool, machine_zone: &str) -> Option<String> {
    if shown == TimeZoneShown::Instrument {
        return None;
    }
    let (civil, zone) = (text.get(..17)?, text.get(18..)?);
    if text.as_bytes().get(17) != Some(&b' ') || zone.is_empty() {
        return None;
    }
    let civil = jiff::civil::DateTime::strptime("%Y%m%d %H:%M:%S", civil).ok()?;
    let at = civil.to_zoned(jiff::tz::TimeZone::get(zone).ok()?).ok()?.timestamp();
    Some(match shown {
        TimeZoneShown::Utc => format!("{}{}", at.to_zoned(jiff::tz::TimeZone::UTC).strftime("%Y%m%d-%H:%M:%S"), if z { "Z" } else { "" }),
        _ => {
            let tz = jiff::tz::TimeZone::get(machine_zone).ok()?;
            format!("{} {}", at.to_zoned(tz).strftime("%Y%m%d %H:%M:%S"), machine_zone)
        }
    })
}

/// An account value as the session gives it with
/// `roundAccountValuesToNearestWholeNumber` on (ibx#747; the reference
/// 10.51, paper, regular hours 13/11/2026: every value written with a
/// decimal point in the account summary and the account updates, 116 of
/// them, the exchange rates too: 915.63 gives 916, -29.98 gives -30,
/// 1.1033517 gives 1, 0.00 gives 0; a whole number, a text and a list of
/// dated amounts are kept). None when the value is kept.
pub fn account_value_rounded(value: &str) -> Option<String> {
    let digits = value.strip_prefix('-').unwrap_or(value);
    let (whole, fraction) = digits.split_once('.')?;
    if whole.is_empty() || fraction.is_empty() || !whole.bytes().chain(fraction.bytes()).all(|b| b.is_ascii_digit()) {
        return None;
    }
    let rounded = value.parse::<f64>().ok()?.round();
    Some(if rounded == 0.0 { "0".to_string() } else { format!("{rounded:.0}") })
}

/// A setting an update names, in the order the reference applies them
/// and names them among the changed fields.
struct Setting {
    name: &'static str,
    /// Set in the request (a list: not empty).
    present: bool,
    /// Set to a value ibx does not follow: not the current one, on a
    /// setting ibx follows. Always false on a setting that means nothing
    /// without the reference's installation.
    unfollowed: bool,
}

fn changes<T: PartialEq>(new: &Option<T>, current: &Option<T>) -> bool {
    new.is_some() && new != current
}

/// A text compared without case, as the reference reads these.
fn changes_text(new: &Option<String>, current: &Option<String>) -> bool {
    new.as_deref().is_some_and(|n| !current.as_deref().is_some_and(|c| c.eq_ignore_ascii_case(n)))
}

/// `value` in the spelling of `allowed`, as the reference gives it back.
fn canonical(value: &str, allowed: &[&str]) -> String {
    allowed.iter().find(|a| a.eq_ignore_ascii_case(value)).map_or_else(|| value.to_string(), |a| a.to_string())
}

/// The configuration of a session.
#[derive(Clone, Debug)]
pub struct SessionConfig {
    lock_and_exit: LockAndExitConfig,
    messages: Vec<Message>,
    precautions: ApiPrecautionsConfig,
    settings: ApiSettingsConfig,
    smart_routing: OrdersSmartRoutingConfig,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self::paper()
    }
}

impl SessionConfig {
    /// The paper installation's configuration observed on 15/10/2026. The
    /// fields it did not give are unset here too: masterClientId (no master
    /// client), letApiAccountRequestsSwitchSubscription,
    /// bypassFlaggedAccountsWarning and sendMarketDataInLotsForUsStocks.
    pub fn paper() -> Self {
        let s = |v: &str| Some(v.to_string());
        Self {
            lock_and_exit: LockAndExitConfig { auto_logoff_time: s("11:45"), auto_logoff_period: s("PM"), auto_logoff_type: s("logoff") },
            messages: PAPER_MESSAGES.iter()
                .map(|&(title, message, action, id, enabled, exposed, warning)| Message {
                    config: MessageConfig {
                        id: Some(id), title: s(title), message: message.map(Into::into), default_action: s(action), enabled: Some(enabled),
                    },
                    exposed,
                    warning,
                })
                .collect(),
            precautions: ApiPrecautionsConfig {
                bypass_order_precautions: Some(true),
                bypass_bond_warning: Some(true),
                bypass_negative_yield_confirmation: Some(true),
                bypass_called_bond_warning: Some(false),
                bypass_same_action_pair_trade_warning: Some(false),
                bypass_flagged_accounts_warning: None,
                bypass_price_based_volatility_warning: Some(false),
                bypass_redirect_order_warning: Some(false),
                bypass_no_overfill_protection: Some(false),
                bypass_route_marketable_to_bbo: Some(false),
            },
            settings: ApiSettingsConfig {
                read_only_api: Some(false),
                total_quantity_for_mutual_funds: Some(false),
                download_open_orders_on_connection: Some(true),
                include_virtual_fx_positions: Some(true),
                prepare_daily_pnl: Some(true),
                send_status_updates_for_volatility_orders: Some(true),
                encode_api_messages: s("ascii7"),
                socket_port: Some(4002),
                use_negative_auto_range: Some(true),
                create_api_message_log_file: Some(true),
                include_market_data_in_log_file: Some(true),
                expose_trading_schedule_to_api: Some(true),
                split_insured_deposit_from_cash_balance: Some(false),
                send_zero_positions_for_today_only: Some(false),
                let_api_account_requests_switch_subscription: None,
                use_account_groups_with_allocation_methods: Some(true),
                logging_level: s("system"),
                master_client_id: None,
                bulk_data_timeout: Some(30),
                component_exch_separator: s(""),
                show_forex_data_in_1_10_pips: Some(true),
                allow_forex_trading_in_1_10_pips: Some(true),
                round_account_values_to_nearest_whole_number: Some(false),
                send_market_data_in_lots_for_us_stocks: None,
                show_advanced_order_reject_in_ui: Some(true),
                reject_messages_above_max_rate: Some(false),
                maintain_connection_on_incorrect_fields: Some(true),
                compatibility_mode_nasdaq_stocks: Some(false),
                send_instrument_timezone: s("instrument"),
                send_forex_data_in_compatibility_mode: Some(false),
                maintain_and_resubmit_orders_on_reconnect: Some(true),
                historical_data_max_size: Some(16),
                auto_report_netting_event_contract_trades: Some(true),
                option_exercise_request_type: s("editable"),
                allow_localhost_only: Some(true),
                trusted_ips: vec!["127.0.0.1".to_string()],
            },
            smart_routing: OrdersSmartRoutingConfig {
                seek_price_improvement: Some(false),
                pre_open_reroute: Some(false),
                do_not_route_to_dark_pools: Some(false),
                default_algorithm: s("SMART"),
            },
        }
    }

    /// The messages reqConfig lists, and the only ones an update can name:
    /// those listed while enabled, and every disabled one.
    fn listed(&self) -> impl Iterator<Item = &Message> {
        self.messages.iter().filter(|m| m.exposed || m.config.enabled == Some(false))
    }

    /// The answer to reqConfig. Without a request id the answer has 0.
    pub fn response(&self, req_id: Option<i32>) -> ConfigResponse {
        let precautions = ApiPrecautionsConfig {
            // The precautions ibx follows, as set for ibx (ibx#486, ibx#608).
            bypass_redirect_order_warning: Some(crate::config::bypass_redirect_order_warning()),
            bypass_route_marketable_to_bbo: Some(crate::config::bypass_route_marketable_to_bbo_warning()),
            ..self.precautions.clone()
        };
        ConfigResponse {
            req_id: Some(req_id.unwrap_or(0)),
            lock_and_exit: Some(self.lock_and_exit.clone()),
            messages: self.listed().map(|m| m.config.clone()).collect(),
            api: Some(ApiConfig { precautions: Some(precautions), settings: Some(self.settings.clone()) }),
            orders: Some(OrdersConfig { smart_routing: Some(self.smart_routing.clone()) }),
        }
    }

    /// What the reference's checks read from this configuration.
    pub fn rules(&self) -> ConfigRules {
        ConfigRules {
            messages: self.listed()
                .map(|m| MessageRule {
                    id: m.config.id.unwrap_or_default(),
                    // A message with no text of its own has "Message" as
                    // its warning's text (ibx#610; the reference 10.51 at
                    // level 225, paper, 18/10/2026: message 131).
                    warning: m.warning.then(|| {
                        (m.config.title.clone().unwrap_or_default(), m.config.message.clone().unwrap_or_else(|| "Message".to_string()))
                    }),
                })
                .collect(),
            settings_off: vec!["letApiAccountRequestsSwitchSubscription"],
            algorithms: vec!["SMART".to_string()],
            pre_open_reroute: true,
            dark_pools: true,
        }
    }

    /// The settings `request` names, in the reference's order.
    fn settings_named(&self, request: &UpdateConfigRequest) -> Vec<Setting> {
        let cur = &self.settings;
        let Some(s) = request.api.as_ref().and_then(|a| a.settings.as_ref()) else { return Vec::new() };
        let free = |name: &'static str, present: bool| Setting { name, present, unfollowed: false };
        macro_rules! follows {
            ($name:literal, $field:ident) => {
                Setting { name: $name, present: s.$field.is_some(), unfollowed: changes(&s.$field, &cur.$field) }
            };
            ($name:literal, $field:ident, text) => {
                Setting { name: $name, present: s.$field.is_some(), unfollowed: changes_text(&s.$field, &cur.$field) }
            };
        }
        vec![
            follows!("totalQuantityForMutualFunds", total_quantity_for_mutual_funds),
            follows!("includeVirtualFxPositions", include_virtual_fx_positions),
            follows!("prepareDailyPnL", prepare_daily_pnl),
            free("encodeApiMessages", s.encode_api_messages.is_some()),
            free("socketPort", s.socket_port.is_some()),
            free("createApiMessageLogFile", s.create_api_message_log_file.is_some()),
            free("includeMarketDataInLogFile", s.include_market_data_in_log_file.is_some()),
            follows!("exposeTradingScheduleToApi", expose_trading_schedule_to_api),
            follows!("splitInsuredDepositFromCashBalance", split_insured_deposit_from_cash_balance),
            follows!("sendZeroPositionsForTodayOnly", send_zero_positions_for_today_only),
            free("loggingLevel", s.logging_level.is_some()),
            free("bulkDataTimeout", s.bulk_data_timeout.is_some()),
            follows!("componentExchSeparator", component_exch_separator),
            // Followed since ibx#747: the account values are rounded.
            free("roundAccountValuesToNearestWholeNumber", s.round_account_values_to_nearest_whole_number.is_some()),
            free("showAdvancedOrderRejectInUi", s.show_advanced_order_reject_in_ui.is_some()),
            follows!("rejectMessagesAboveMaxRate", reject_messages_above_max_rate),
            follows!("maintainConnectionOnIncorrectFields", maintain_connection_on_incorrect_fields),
            // Followed since ibx#747: the times are written in that zone.
            free("sendInstrumentTimezone", s.send_instrument_timezone.is_some()),
            follows!("historicalDataMaxSize", historical_data_max_size),
            follows!("autoReportNettingEventContractTrades", auto_report_netting_event_contract_trades),
            follows!("readOnlyApi", read_only_api),
            follows!("optionExerciseRequestType", option_exercise_request_type, text),
            follows!("maintainAndResubmitOrdersOnReconnect", maintain_and_resubmit_orders_on_reconnect),
            follows!("sendForexDataInCompatibilityMode", send_forex_data_in_compatibility_mode),
            follows!("compatibilityModeNasdaqStocks", compatibility_mode_nasdaq_stocks),
            follows!("allowForexTradingIn110Pips", allow_forex_trading_in_1_10_pips),
            follows!("showForexDataIn110Pips", show_forex_data_in_1_10_pips),
            follows!("useAccountGroupsWithAllocationMethods", use_account_groups_with_allocation_methods),
            follows!("letApiAccountRequestsSwitchSubscription", let_api_account_requests_switch_subscription),
            follows!("useNegativeAutoRange", use_negative_auto_range),
            follows!("sendStatusUpdatesForVolatilityOrders", send_status_updates_for_volatility_orders),
            follows!("downloadOpenOrdersOnConnection", download_open_orders_on_connection),
            follows!("masterClientId", master_client_id),
            // Kept only when the list is not empty: it cannot be cleared.
            free("trustedIPs", !s.trusted_ips.is_empty()),
        ]
    }

    /// The smart routing settings `request` names, in the reference's
    /// order. ibx follows each of them.
    fn routing_named(&self, request: &UpdateConfigRequest) -> Vec<Setting> {
        let cur = &self.smart_routing;
        let Some(r) = request.orders.as_ref().and_then(|o| o.smart_routing.as_ref()) else { return Vec::new() };
        macro_rules! follows {
            ($name:literal, $field:ident) => {
                Setting { name: $name, present: r.$field.is_some(), unfollowed: changes(&r.$field, &cur.$field) }
            };
        }
        vec![
            follows!("seekPriceImprovement", seek_price_improvement),
            follows!("preOpenReroute", pre_open_reroute),
            follows!("defaultAlgorithm", default_algorithm),
            follows!("doNotRouteToDarkPools", do_not_route_to_dark_pools),
        ]
    }

    /// The zone the times given to a client are written in (ibx#747).
    pub fn time_zone_shown(&self) -> TimeZoneShown {
        match self.settings.send_instrument_timezone.as_deref() {
            Some(v) if v.eq_ignore_ascii_case("utc") => TimeZoneShown::Utc,
            Some(v) if v.eq_ignore_ascii_case("operator") => TimeZoneShown::Operator,
            _ => TimeZoneShown::Instrument,
        }
    }

    /// Whether the account values are given as whole numbers (ibx#747).
    pub fn rounds_account_values(&self) -> bool {
        self.settings.round_account_values_to_nearest_whole_number == Some(true)
    }

    /// The answer to updateConfig: the reference's checks first, then the
    /// values ibx cannot follow; when neither refuses it, the update is
    /// kept and "success" names the changed fields: the settings, the
    /// smart routing settings, then the messages.
    pub fn update(&mut self, request: &UpdateConfigRequest) -> UpdateConfigResponse {
        if let Some(refusal) = update_config_check(request, &self.rules()) {
            return refusal;
        }
        let answer = |status: &str, message: &str| UpdateConfigResponse {
            // Without a request id the answer has the largest one.
            req_id: Some(request.req_id.unwrap_or(i32::MAX)),
            status: Some(status.to_string()),
            message: Some(message.to_string()),
            ..Default::default()
        };
        let settings = self.settings_named(request);
        let routing = self.routing_named(request);
        // Resetting the order id sequence is not followed: ibx keeps its
        // order ids (ibx#518).
        let mut unfollowed: Vec<String> = Vec::new();
        if request.reset_api_order_sequence == Some(true) {
            unfollowed.push("configuration.resetAPIOrderSequence".to_string());
        }
        unfollowed.extend(settings.iter().filter(|s| s.unfollowed).map(|s| format!("configuration.api.settings.{}", s.name)));
        unfollowed.extend(routing.iter().filter(|s| s.unfollowed).map(|s| format!("configuration.orders.smartRouting.{}", s.name)));
        if !unfollowed.is_empty() {
            return UpdateConfigResponse { errors: unfollowed, ..answer(STATUS_ERROR, VALIDATION_FAILED) };
        }

        let mut changed: Vec<String> = settings.iter().filter(|s| s.present)
            .map(|s| format!("configuration.api.settings.{}", s.name))
            .chain(routing.iter().filter(|s| s.present).map(|s| format!("configuration.orders.smartRouting.{}", s.name)))
            .collect();
        if let Some(s) = request.api.as_ref().and_then(|a| a.settings.as_ref()) {
            // Only the settings ibx does not follow can take a new value;
            // the others were checked equal to the current one.
            let cur = &mut self.settings;
            if let Some(v) = &s.encode_api_messages { cur.encode_api_messages = Some(canonical(v, &["ascii7", "osCodePage"])); }
            if let Some(v) = s.socket_port { cur.socket_port = Some(v); }
            if let Some(v) = s.create_api_message_log_file { cur.create_api_message_log_file = Some(v); }
            if let Some(v) = s.include_market_data_in_log_file { cur.include_market_data_in_log_file = Some(v); }
            if let Some(v) = &s.logging_level {
                cur.logging_level = Some(canonical(v, &["system", "error", "warning", "info", "detail"]));
            }
            if let Some(v) = s.bulk_data_timeout { cur.bulk_data_timeout = Some(v); }
            if let Some(v) = &s.send_instrument_timezone {
                cur.send_instrument_timezone = Some(canonical(v, &["instrument", "operator", "utc"]));
            }
            if let Some(v) = s.round_account_values_to_nearest_whole_number {
                cur.round_account_values_to_nearest_whole_number = Some(v);
            }
            if let Some(v) = s.show_advanced_order_reject_in_ui { cur.show_advanced_order_reject_in_ui = Some(v); }
            if !s.trusted_ips.is_empty() { cur.trusted_ips = s.trusted_ips.clone(); }
        }
        for m in &request.messages {
            let Some(id) = m.id else { continue };
            if let Some(entry) = self.messages.iter_mut().find(|e| e.config.id == Some(id)) {
                // A message given with its id only is disabled.
                entry.config.enabled = Some(m.enabled.unwrap_or(false));
                changed.push(format!("configuration.messages id: {id}"));
            }
        }
        UpdateConfigResponse { changed_fields: changed, ..answer(STATUS_SUCCESS, CONFIG_UPDATED) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(id: i32) -> UpdateConfigRequest {
        UpdateConfigRequest { req_id: Some(id), ..Default::default() }
    }

    fn settings(id: i32, s: ApiSettingsConfig) -> UpdateConfigRequest {
        UpdateConfigRequest { api: Some(ApiConfig { settings: Some(s), precautions: None }), ..req(id) }
    }

    fn routing(id: i32, r: OrdersSmartRoutingConfig) -> UpdateConfigRequest {
        UpdateConfigRequest { orders: Some(OrdersConfig { smart_routing: Some(r) }), ..req(id) }
    }

    fn msg(id: i32, enabled: Option<bool>) -> MessageConfig {
        MessageConfig { id: Some(id), enabled, ..Default::default() }
    }

    fn accept(id: i32) -> UpdateConfigWarning {
        UpdateConfigWarning { message_id: Some(id), ..Default::default() }
    }

    type Answer = (i32, String, String, Vec<String>, Vec<String>);

    /// (req id, status, message, changed fields, errors).
    fn answer(c: &mut SessionConfig, r: &UpdateConfigRequest) -> Answer {
        let a = c.update(r);
        (a.req_id.unwrap(), a.status.unwrap(), a.message.unwrap(), a.changed_fields, a.errors)
    }

    fn success(id: i32, changed: &[&str]) -> Answer {
        (id, "success".into(), "Configuration updated".into(), changed.iter().map(|s| s.to_string()).collect(), vec![])
    }

    fn unfollowed(id: i32, errors: &[&str]) -> Answer {
        (id, "error".into(), VALIDATION_FAILED.into(), vec![], errors.iter().map(|s| s.to_string()).collect())
    }

    fn settings_of(c: &SessionConfig) -> ApiSettingsConfig {
        c.response(None).api.unwrap().settings.unwrap()
    }

    // The configuration the reference's paper installation gave on
    // 15/10/2026 (paper, market closed, fresh process, level 225).
    #[test]
    fn the_configuration_is_the_paper_installations() {
        let r = SessionConfig::paper().response(Some(7301));
        assert_eq!(r.req_id, Some(7301));
        let l = r.lock_and_exit.unwrap();
        assert_eq!((l.auto_logoff_time.as_deref(), l.auto_logoff_period.as_deref(), l.auto_logoff_type.as_deref()),
                   (Some("11:45"), Some("PM"), Some("logoff")));
        let ids: Vec<i32> = r.messages.iter().map(|m| m.id.unwrap()).collect();
        assert_eq!(ids, [118, 131, 432, 444, 445, 449, 458, 530, 537, 580, 586, 589, 605, 606, 616, 621, 645, 675, 710, 723, 728, 731]);
        assert_eq!(r.messages[0], MessageConfig {
            id: Some(118), title: Some("Confirm auto cancel date".into()), message: None, default_action: Some("Hidden".into()), enabled: Some(false),
        });
        assert_eq!(r.messages[12].enabled, Some(false));
        let api = r.api.unwrap();
        let p = api.precautions.unwrap();
        assert_eq!((p.bypass_order_precautions, p.bypass_flagged_accounts_warning, p.bypass_called_bond_warning), (Some(true), None, Some(false)));
        let s = api.settings.unwrap();
        assert_eq!((s.socket_port, s.master_client_id, s.let_api_account_requests_switch_subscription, s.send_market_data_in_lots_for_us_stocks),
                   (Some(4002), None, None, None));
        assert_eq!((s.read_only_api, s.download_open_orders_on_connection, s.historical_data_max_size), (Some(false), Some(true), Some(16)));
        assert_eq!(s.trusted_ips, ["127.0.0.1"]);
        let sr = r.orders.unwrap().smart_routing.unwrap();
        assert_eq!((sr.seek_price_improvement, sr.default_algorithm.as_deref()), (Some(false), Some("SMART")));
        // Without a request id the answer has 0.
        assert_eq!(SessionConfig::paper().response(None).req_id, Some(0));
    }

    // The updates that succeeded on the reference 10.51 on 17/10/2026
    // (paper, market closed, level 225): same answers, and the
    // configuration read after them is the same as before.
    #[test]
    fn updates_the_reference_took_are_answered_as_it_answered_them() {
        let mut c = SessionConfig::paper();
        let before = c.response(Some(1));
        assert_eq!(answer(&mut c, &req(7410)), success(7410, &[]));
        assert_eq!(answer(&mut c, &UpdateConfigRequest::default()), success(i32::MAX, &[]));
        assert_eq!(answer(&mut c, &UpdateConfigRequest { lock_and_exit: Some(Default::default()), ..req(7413) }), success(7413, &[]));
        let c14 = UpdateConfigRequest { messages: vec![msg(605, Some(false))], accepted_warnings: vec![accept(605)], ..req(7423) };
        assert_eq!(answer(&mut c, &c14), success(7423, &["configuration.messages id: 605"]));
        let c15 = settings(7424, ApiSettingsConfig {
            logging_level: Some("system".into()), bulk_data_timeout: Some(30), socket_port: Some(4002),
            create_api_message_log_file: Some(true), ..Default::default()
        });
        assert_eq!(answer(&mut c, &c15), success(7424, &["configuration.api.settings.socketPort",
            "configuration.api.settings.createApiMessageLogFile", "configuration.api.settings.loggingLevel",
            "configuration.api.settings.bulkDataTimeout"]));
        let c16 = routing(7425, OrdersSmartRoutingConfig { seek_price_improvement: Some(false), ..Default::default() });
        assert_eq!(answer(&mut c, &c16), success(7425, &["configuration.orders.smartRouting.seekPriceImprovement"]));
        assert_eq!(answer(&mut c, &UpdateConfigRequest { messages: vec![msg(118, None)], ..req(7426) }),
                   success(7426, &["configuration.messages id: 118"]));
        let d03 = settings(7503, ApiSettingsConfig { send_forex_data_in_compatibility_mode: Some(false), ..Default::default() });
        assert_eq!(answer(&mut c, &d03), success(7503, &["configuration.api.settings.sendForexDataInCompatibilityMode"]));
        let d06 = settings(7506, ApiSettingsConfig { encode_api_messages: Some("ASCII7".into()), ..Default::default() });
        assert_eq!(answer(&mut c, &d06), success(7506, &["configuration.api.settings.encodeApiMessages"]));
        let d07 = settings(7507, ApiSettingsConfig { logging_level: Some("SYSTEM".into()), ..Default::default() });
        assert_eq!(answer(&mut c, &d07), success(7507, &["configuration.api.settings.loggingLevel"]));
        let d08 = settings(7508, ApiSettingsConfig { send_instrument_timezone: Some("INSTRUMENT".into()), ..Default::default() });
        assert_eq!(answer(&mut c, &d08), success(7508, &["configuration.api.settings.sendInstrumentTimezone"]));
        assert_eq!(answer(&mut c, &UpdateConfigRequest { reset_api_order_sequence: Some(false), ..req(7513) }), success(7513, &[]));
        assert_eq!(c.response(Some(1)), before);
        // The reference's checks still come first.
        let c13 = UpdateConfigRequest { messages: vec![msg(605, Some(false))], ..req(7422) };
        assert_eq!(c.update(&c13).status.as_deref(), Some("pending"));
        // A message with no text: its warning's text is "Message" (ibx#610;
        // the reference 10.51 at level 225, paper, 18/10/2026, message 131).
        let w131 = c.update(&UpdateConfigRequest { messages: vec![msg(131, Some(false))], ..req(7801) });
        assert_eq!(w131.status.as_deref(), Some("pending"));
        assert_eq!(w131.warnings.iter().map(|w| (w.message_id, w.title.as_deref(), w.message.as_deref())).collect::<Vec<_>>(),
            [(Some(131), Some("Confirm Mandatory Cap Price"), Some("Message"))]);
        let c08 = settings(7417, ApiSettingsConfig { logging_level: Some("bogus".into()), read_only_api: Some(true), ..Default::default() });
        assert_eq!(c.update(&c08).errors, ["configuration.api.settings.loggingLevel"]);
    }

    // Decision of 18/10/2026: a new value of a setting that means nothing
    // without the reference's installation is kept, and a later reqConfig
    // shows it.
    #[test]
    fn a_new_value_of_a_setting_ibx_does_not_act_on_is_kept() {
        let mut c = SessionConfig::paper();
        let r = settings(7801, ApiSettingsConfig {
            socket_port: Some(4003), logging_level: Some("DETAIL".into()), encode_api_messages: Some("oscodepage".into()),
            bulk_data_timeout: Some(60), create_api_message_log_file: Some(false), include_market_data_in_log_file: Some(false),
            show_advanced_order_reject_in_ui: Some(false), trusted_ips: vec!["127.0.0.1".into(), "10.0.0.2".into()],
            send_market_data_in_lots_for_us_stocks: Some(true), ..Default::default()
        });
        // sendMarketDataInLotsForUsStocks is taken and not named.
        assert_eq!(answer(&mut c, &r), success(7801, &["configuration.api.settings.encodeApiMessages",
            "configuration.api.settings.socketPort", "configuration.api.settings.createApiMessageLogFile",
            "configuration.api.settings.includeMarketDataInLogFile", "configuration.api.settings.loggingLevel",
            "configuration.api.settings.bulkDataTimeout", "configuration.api.settings.showAdvancedOrderRejectInUi",
            "configuration.api.settings.trustedIPs"]));
        let s = settings_of(&c);
        assert_eq!((s.socket_port, s.logging_level.as_deref(), s.encode_api_messages.as_deref(), s.bulk_data_timeout),
                   (Some(4003), Some("detail"), Some("osCodePage"), Some(60)));
        assert_eq!((s.create_api_message_log_file, s.include_market_data_in_log_file, s.show_advanced_order_reject_in_ui),
                   (Some(false), Some(false), Some(false)));
        assert_eq!(s.trusted_ips, ["127.0.0.1", "10.0.0.2"]);
        assert_eq!(s.send_market_data_in_lots_for_us_stocks, None);
        // An empty list of trusted addresses changes nothing and is not named.
        assert_eq!(answer(&mut c, &settings(7802, ApiSettingsConfig::default())), success(7802, &[]));
        assert_eq!(settings_of(&c).trusted_ips, ["127.0.0.1", "10.0.0.2"]);

        // Message switches: kept, with the warning accepted when needed.
        let r = UpdateConfigRequest {
            messages: vec![msg(444, Some(false)), msg(432, Some(false)), msg(605, Some(true))],
            accepted_warnings: vec![accept(444)],
            ..req(7803)
        };
        assert_eq!(answer(&mut c, &r), success(7803, &["configuration.messages id: 444", "configuration.messages id: 432",
            "configuration.messages id: 605"]));
        let enabled = |c: &SessionConfig, id: i32| c.response(None).messages.iter().find(|m| m.id == Some(id)).map(|m| m.enabled);
        assert_eq!((enabled(&c, 444), enabled(&c, 432), enabled(&c, 605)), (Some(Some(false)), Some(Some(false)), Some(Some(true))));
        // 118 is listed only while it is disabled: enabled, it leaves the
        // list, and an update can no longer name it.
        assert_eq!(answer(&mut c, &UpdateConfigRequest { messages: vec![msg(118, Some(true))], ..req(7804) }),
                   success(7804, &["configuration.messages id: 118"]));
        assert_eq!(enabled(&c, 118), None);
        assert_eq!(c.update(&UpdateConfigRequest { messages: vec![msg(118, Some(false))], ..req(7805) }).errors,
                   ["configuration.messages: Message with id 118 not found"]);
    }

    // Decision of 18/10/2026: a value ibx does not follow is answered
    // "error" with the field named, and nothing is changed; the value ibx
    // follows is taken.
    #[test]
    // ibx#747 (the reference 10.51, paper, regular hours 13/11/2026): the
    // zone of the times and the rounding of the account values are taken,
    // answered "success" with the field named, and given back by reqConfig.
    #[test]
    fn the_time_zone_and_the_rounding_are_taken() {
        let mut c = SessionConfig::paper();
        assert_eq!((c.time_zone_shown(), c.rounds_account_values()), (TimeZoneShown::Instrument, false));
        let r = settings(7701, ApiSettingsConfig { send_instrument_timezone: Some("UTC".into()), ..Default::default() });
        assert_eq!(answer(&mut c, &r), success(7701, &["configuration.api.settings.sendInstrumentTimezone"]));
        assert_eq!(c.time_zone_shown(), TimeZoneShown::Utc);
        assert_eq!(c.response(Some(1)).api.unwrap().settings.unwrap().send_instrument_timezone.as_deref(), Some("utc"));
        let r = settings(7702, ApiSettingsConfig { send_instrument_timezone: Some("operator".into()),
            round_account_values_to_nearest_whole_number: Some(true), ..Default::default() });
        assert_eq!(answer(&mut c, &r), success(7702, &["configuration.api.settings.roundAccountValuesToNearestWholeNumber",
            "configuration.api.settings.sendInstrumentTimezone"]));
        assert_eq!((c.time_zone_shown(), c.rounds_account_values()), (TimeZoneShown::Operator, true));
        let r = settings(7703, ApiSettingsConfig { send_instrument_timezone: Some("instrument".into()),
            round_account_values_to_nearest_whole_number: Some(false), ..Default::default() });
        assert_eq!(c.update(&r).status.as_deref(), Some("success"));
        assert_eq!((c.time_zone_shown(), c.rounds_account_values()), (TimeZoneShown::Instrument, false));
    }

    #[test]
    fn a_time_is_written_in_the_zone_of_the_setting() {
        let t = "20261113 09:43:55 US/Eastern";
        assert_eq!(time_shown(t, TimeZoneShown::Instrument, true, "Europe/Paris"), None);
        assert_eq!(time_shown(t, TimeZoneShown::Utc, true, "Europe/Paris").as_deref(), Some("20261113-14:43:55Z"));
        assert_eq!(time_shown(t, TimeZoneShown::Utc, false, "Europe/Paris").as_deref(), Some("20261113-14:43:55"));
        assert_eq!(time_shown(t, TimeZoneShown::Operator, true, "Europe/Paris").as_deref(), Some("20261113 15:43:55 Europe/Paris"));
        assert_eq!(time_shown("20261113 15:41:53 MET", TimeZoneShown::Utc, true, "Europe/Paris").as_deref(), Some("20261113-14:41:53Z"));
        assert_eq!(time_shown("20261113 15:41:53 MET", TimeZoneShown::Operator, false, "Europe/Paris").as_deref(), Some("20261113 15:41:53 Europe/Paris"));
        // A bar's date, Unix seconds, an empty time: kept.
        for kept in ["20261113", "1794580200", "", "20261113 09:43:55"] {
            assert_eq!(time_shown(kept, TimeZoneShown::Utc, false, "Europe/Paris"), None, "{kept}");
        }
    }

    #[test]
    fn an_account_value_is_rounded_to_a_whole_number() {
        for (value, rounded) in [("915.63", "916"), ("-29.98", "-30"), ("1.1033517", "1"), ("0.00", "0"), ("908762.8429", "908763"),
            ("-1685.51", "-1686"), ("-0.40", "0"), ("6328880.76", "6328881")]
        {
            assert_eq!(account_value_rounded(value).as_deref(), Some(rounded), "{value}");
        }
        for kept in ["1794603600", "INDIVIDUAL", "true", "", "20261113:910193.94401762;20261116:910195", "DU1", "1", "-30"] {
            assert_eq!(account_value_rounded(kept), None, "{kept}");
        }
    }

    #[test]
    fn a_value_ibx_cannot_follow_is_refused_with_the_field_named() {
        let mut c = SessionConfig::paper();
        let before = c.response(Some(1));
        let s = |n: &str| format!("configuration.api.settings.{n}");
        let r = settings(7901, ApiSettingsConfig { read_only_api: Some(true), socket_port: Some(4003), ..Default::default() });
        assert_eq!(answer(&mut c, &r), unfollowed(7901, &[&s("readOnlyApi")]));
        let r = settings(7902, ApiSettingsConfig {
            master_client_id: Some(0), download_open_orders_on_connection: Some(false), historical_data_max_size: Some(4),
            option_exercise_request_type: Some("final".into()),
            total_quantity_for_mutual_funds: Some(true), component_exch_separator: Some("-".into()),
            ..Default::default()
        });
        assert_eq!(answer(&mut c, &r), unfollowed(7902, &[&s("totalQuantityForMutualFunds"), &s("componentExchSeparator"),
            &s("historicalDataMaxSize"), &s("optionExerciseRequestType"),
            &s("downloadOpenOrdersOnConnection"), &s("masterClientId")]));
        let r = UpdateConfigRequest {
            reset_api_order_sequence: Some(true),
            messages: vec![msg(432, Some(false))],
            ..routing(7903, OrdersSmartRoutingConfig {
                seek_price_improvement: Some(true), pre_open_reroute: Some(false), do_not_route_to_dark_pools: Some(true),
                default_algorithm: Some("SMART".into()),
            })
        };
        assert_eq!(answer(&mut c, &r), unfollowed(7903, &["configuration.resetAPIOrderSequence",
            "configuration.orders.smartRouting.seekPriceImprovement", "configuration.orders.smartRouting.doNotRouteToDarkPools"]));
        assert_eq!(c.response(Some(1)), before, "nothing changed");
        // The values ibx follows are taken, and named.
        let r = UpdateConfigRequest {
            orders: Some(OrdersConfig { smart_routing: Some(OrdersSmartRoutingConfig {
                pre_open_reroute: Some(false), default_algorithm: Some("SMART".into()), ..Default::default()
            }) }),
            ..settings(7904, ApiSettingsConfig {
                read_only_api: Some(false), download_open_orders_on_connection: Some(true), send_instrument_timezone: Some("Instrument".into()),
                ..Default::default()
            })
        };
        assert_eq!(answer(&mut c, &r), success(7904, &[&s("sendInstrumentTimezone"), &s("readOnlyApi"),
            &s("downloadOpenOrdersOnConnection"), "configuration.orders.smartRouting.preOpenReroute",
            "configuration.orders.smartRouting.defaultAlgorithm"]));
        assert_eq!(c.response(Some(1)), before);
    }
}
