//! The configuration requests across the C entry layer (ibx#235): the
//! structures of `crate::api::config` as flat text records, one per set
//! field.
//!
//! A record is `path \x1f value \x1e`. The path is made of field names,
//! with an index after the name of a repeated field, whose count is the
//! record `path.#`. A set sub-message, and each item of a repeated one,
//! has a record of its own with an empty value, so that one with no field
//! set is still there. A flag is `true` or `false`.
//!
//! A name is compared without case and underscores: the other side writes
//! the names of its own message classes.

use std::collections::HashMap;

use crate::api::config::*;

use super::{IbxClient, IbxStr};

/// The records of a request, by path.
pub(crate) struct Fields<'a>(HashMap<String, &'a str>);

/// A name as it is compared: without case and underscores.
fn plain(name: &str) -> String {
    name.chars().filter(|c| *c != '_').flat_map(char::to_lowercase).collect()
}

fn key(path: &str, name: &str) -> String {
    if path.is_empty() { plain(name) } else { format!("{path}.{}", plain(name)) }
}

impl<'a> Fields<'a> {
    pub(crate) fn parse(text: &'a str) -> Self {
        let mut map = HashMap::new();
        for record in text.split('\x1e') {
            if let Some((path, value)) = record.split_once('\x1f') {
                map.insert(path.split('.').map(plain).collect::<Vec<_>>().join("."), value);
            }
        }
        Self(map)
    }

    fn count(&self, path: &str) -> usize {
        self.0.get(&format!("{path}.#")).and_then(|v| v.parse().ok()).unwrap_or(0)
    }
}

fn record(out: &mut String, path: &str, value: &str) {
    out.push_str(path);
    out.push('\x1f');
    out.push_str(value);
    out.push('\x1e');
}

/// A structure that crosses as records.
pub(crate) trait Records: Sized {
    /// The records of the set fields, under `path`.
    fn write(&self, path: &str, out: &mut String);
    /// The structure whose records are under `path`.
    fn read(path: &str, fields: &Fields) -> Self;
    /// Every field set, to a value made from its name (for the tests).
    #[cfg(any(test, feature = "test-support"))]
    fn filled(seed: i64) -> Self;
}

/// How each kind of field is written.
mod put {
    use super::{key, record, Records};

    pub(super) fn text(v: &Option<String>, path: &str, name: &str, out: &mut String) {
        if let Some(v) = v {
            record(out, &key(path, name), v);
        }
    }

    pub(super) fn int(v: &Option<i32>, path: &str, name: &str, out: &mut String) {
        if let Some(v) = v {
            record(out, &key(path, name), &v.to_string());
        }
    }

    pub(super) fn flag(v: &Option<bool>, path: &str, name: &str, out: &mut String) {
        if let Some(v) = v {
            record(out, &key(path, name), if *v { "true" } else { "false" });
        }
    }

    pub(super) fn msg<T: Records>(v: &Option<T>, path: &str, name: &str, out: &mut String) {
        if let Some(v) = v {
            let at = key(path, name);
            record(out, &at, "");
            v.write(&at, out);
        }
    }

    pub(super) fn list<T: Records>(v: &[T], path: &str, name: &str, out: &mut String) {
        if v.is_empty() {
            return;
        }
        let at = key(path, name);
        record(out, &format!("{at}.#"), &v.len().to_string());
        for (k, item) in v.iter().enumerate() {
            let at = format!("{at}.{k}");
            record(out, &at, "");
            item.write(&at, out);
        }
    }

    pub(super) fn texts(v: &[String], path: &str, name: &str, out: &mut String) {
        if v.is_empty() {
            return;
        }
        let at = key(path, name);
        record(out, &format!("{at}.#"), &v.len().to_string());
        for (k, item) in v.iter().enumerate() {
            record(out, &format!("{at}.{k}"), item);
        }
    }
}

/// How each kind of field is read. A number or a flag that does not read
/// is not set.
mod get {
    use super::{key, Fields, Records};

    pub(super) fn text(path: &str, name: &str, f: &Fields) -> Option<String> {
        f.0.get(&key(path, name)).map(|v| v.to_string())
    }

    pub(super) fn int(path: &str, name: &str, f: &Fields) -> Option<i32> {
        f.0.get(&key(path, name)).and_then(|v| v.parse().ok())
    }

    pub(super) fn flag(path: &str, name: &str, f: &Fields) -> Option<bool> {
        f.0.get(&key(path, name)).and_then(|v| match *v {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        })
    }

    pub(super) fn msg<T: Records>(path: &str, name: &str, f: &Fields) -> Option<T> {
        let at = key(path, name);
        f.0.contains_key(&at).then(|| T::read(&at, f))
    }

    pub(super) fn list<T: Records>(path: &str, name: &str, f: &Fields) -> Vec<T> {
        let at = key(path, name);
        (0..f.count(&at)).map(|k| T::read(&format!("{at}.{k}"), f)).collect()
    }

    pub(super) fn texts(path: &str, name: &str, f: &Fields) -> Vec<String> {
        let at = key(path, name);
        (0..f.count(&at)).map(|k| f.0.get(&format!("{at}.{k}")).map(|v| v.to_string()).unwrap_or_default()).collect()
    }
}

/// A value for each kind of field, made from the field's name without
/// case and underscores: the C++ tests make the same from the names of the
/// message classes.
#[cfg(any(test, feature = "test-support"))]
mod fill {
    use super::{plain, Records};

    // The rule of the tests' own values (`testing.rs`, `test_support.h`).
    fn hash(name: &str) -> u32 {
        name.bytes().fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(u32::from(b)))
    }

    fn tv_i(name: &str, seed: i64) -> i64 {
        i64::from(hash(name) % 997) + seed
    }

    fn tv_b(name: &str, seed: i64) -> bool {
        (i64::from(hash(name)) + seed) % 2 == 1
    }

    fn tv_s(name: &str, seed: i64) -> String {
        format!("{name}{seed}")
    }

    pub(super) fn text(name: &str, seed: i64) -> Option<String> {
        Some(tv_s(&plain(name), seed))
    }

    pub(super) fn int(name: &str, seed: i64) -> Option<i32> {
        Some(tv_i(&plain(name), seed) as i32)
    }

    pub(super) fn flag(name: &str, seed: i64) -> Option<bool> {
        Some(tv_b(&plain(name), seed))
    }

    pub(super) fn msg<T: Records>(_name: &str, seed: i64) -> Option<T> {
        Some(T::filled(seed + 1))
    }

    pub(super) fn list<T: Records>(_name: &str, seed: i64) -> Vec<T> {
        vec![T::filled(seed + 1), T::filled(seed + 2)]
    }

    pub(super) fn texts(name: &str, seed: i64) -> Vec<String> {
        vec![tv_s(&plain(name), seed + 1), tv_s(&plain(name), seed + 2)]
    }
}

/// The fields of a structure, each with its kind: `text`, `int`, `flag`,
/// `msg` (a sub-message), `list` (a repeated sub-message), `texts` (a
/// repeated text). Every field of the structure is named, or it does not
/// compile.
macro_rules! records {
    ($ty:ty { $($field:ident: $kind:ident),* $(,)? }) => {
        impl Records for $ty {
            fn write(&self, path: &str, out: &mut String) {
                $( put::$kind(&self.$field, path, stringify!($field), out); )*
            }

            fn read(path: &str, fields: &Fields) -> Self {
                Self { $( $field: get::$kind(path, stringify!($field), fields), )* }
            }

            #[cfg(any(test, feature = "test-support"))]
            fn filled(seed: i64) -> Self {
                Self { $( $field: fill::$kind(stringify!($field), seed), )* }
            }
        }
    };
}

records!(LockAndExitConfig { auto_logoff_time: text, auto_logoff_period: text, auto_logoff_type: text });

records!(MessageConfig { id: int, title: text, message: text, default_action: text, enabled: flag });

records!(ApiPrecautionsConfig {
    bypass_order_precautions: flag,
    bypass_bond_warning: flag,
    bypass_negative_yield_confirmation: flag,
    bypass_called_bond_warning: flag,
    bypass_same_action_pair_trade_warning: flag,
    bypass_flagged_accounts_warning: flag,
    bypass_price_based_volatility_warning: flag,
    bypass_redirect_order_warning: flag,
    bypass_no_overfill_protection: flag,
    bypass_route_marketable_to_bbo: flag,
});

records!(ApiSettingsConfig {
    read_only_api: flag,
    total_quantity_for_mutual_funds: flag,
    download_open_orders_on_connection: flag,
    include_virtual_fx_positions: flag,
    prepare_daily_pnl: flag,
    send_status_updates_for_volatility_orders: flag,
    encode_api_messages: text,
    socket_port: int,
    use_negative_auto_range: flag,
    create_api_message_log_file: flag,
    include_market_data_in_log_file: flag,
    expose_trading_schedule_to_api: flag,
    split_insured_deposit_from_cash_balance: flag,
    send_zero_positions_for_today_only: flag,
    let_api_account_requests_switch_subscription: flag,
    use_account_groups_with_allocation_methods: flag,
    logging_level: text,
    master_client_id: int,
    bulk_data_timeout: int,
    component_exch_separator: text,
    show_forex_data_in_1_10_pips: flag,
    allow_forex_trading_in_1_10_pips: flag,
    round_account_values_to_nearest_whole_number: flag,
    send_market_data_in_lots_for_us_stocks: flag,
    show_advanced_order_reject_in_ui: flag,
    reject_messages_above_max_rate: flag,
    maintain_connection_on_incorrect_fields: flag,
    compatibility_mode_nasdaq_stocks: flag,
    send_instrument_timezone: text,
    send_forex_data_in_compatibility_mode: flag,
    maintain_and_resubmit_orders_on_reconnect: flag,
    historical_data_max_size: int,
    auto_report_netting_event_contract_trades: flag,
    option_exercise_request_type: text,
    allow_localhost_only: flag,
    trusted_ips: texts,
});

records!(ApiConfig { precautions: msg, settings: msg });

records!(OrdersSmartRoutingConfig { seek_price_improvement: flag, pre_open_reroute: flag, do_not_route_to_dark_pools: flag, default_algorithm: text });

records!(OrdersConfig { smart_routing: msg });

records!(ConfigRequest { req_id: int });

records!(ConfigResponse { req_id: int, lock_and_exit: msg, messages: list, api: msg, orders: msg });

records!(UpdateConfigWarning { message_id: int, title: text, message: text });

records!(UpdateConfigRequest {
    req_id: int,
    lock_and_exit: msg,
    messages: list,
    api: msg,
    orders: msg,
    accepted_warnings: list,
    reset_api_order_sequence: flag,
});

records!(UpdateConfigResponse { req_id: int, status: text, message: text, changed_fields: texts, errors: texts, warnings: list });

/// The records of a structure.
pub(crate) fn records<T: Records>(x: &T) -> String {
    let mut out = String::new();
    x.write("", &mut out);
    out
}

/// The structure of the records a caller gives.
pub(crate) fn from_records<T: Records>(text: &str) -> T {
    T::read("", &Fields::parse(text))
}

/// Request the configuration: the fields of the request as records. The
/// answer is the `config_response` entry of the callback table, or an
/// error callback.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ibx_req_config(h: *mut IbxClient, fields: IbxStr) -> i32 {
    let request: ConfigRequest = from_records(&unsafe { fields.text() });
    super::request(h, |c, _| {
        c.req_config(&request);
        0
    })
}

/// Update the configuration: the fields of the request as records. The
/// answer is the `update_config_response` entry of the callback table, or
/// an error callback.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ibx_update_config(h: *mut IbxClient, fields: IbxStr) -> i32 {
    let request: UpdateConfigRequest = from_records(&unsafe { fields.text() });
    super::request(h, |c, _| {
        c.update_config(&request);
        0
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every field of every structure, written and read back.
    #[test]
    fn a_structure_with_every_field_set_comes_back_as_written() {
        let response = ConfigResponse::filled(3);
        assert_eq!(from_records::<ConfigResponse>(&records(&response)), response);
        let update = UpdateConfigRequest::filled(4);
        assert_eq!(from_records::<UpdateConfigRequest>(&records(&update)), update);
        let answer = UpdateConfigResponse::filled(5);
        assert_eq!(from_records::<UpdateConfigResponse>(&records(&answer)), answer);
        let request = ConfigRequest::filled(6);
        assert_eq!(from_records::<ConfigRequest>(&records(&request)), request);
        // Filled means filled: nothing is left as it starts.
        let settings = response.api.as_ref().and_then(|a| a.settings.as_ref()).expect("the settings");
        assert!(settings.socket_port.is_some() && settings.logging_level.is_some() && settings.read_only_api.is_some());
        assert_eq!(settings.trusted_ips.len(), 2);
        assert_eq!((response.messages.len(), update.accepted_warnings.len(), answer.warnings.len()), (2, 2, 2));
    }

    // Only what is set crosses, and a sub-message with no field set is
    // still a sub-message.
    #[test]
    fn only_the_set_fields_are_written() {
        assert_eq!(records(&ConfigResponse::default()), "");
        assert_eq!(from_records::<ConfigResponse>(""), ConfigResponse::default());
        let response = ConfigResponse {
            req_id: Some(7),
            api: Some(ApiConfig { precautions: Some(ApiPrecautionsConfig::default()), settings: None }),
            messages: vec![MessageConfig::default(), MessageConfig { enabled: Some(false), ..Default::default() }],
            ..Default::default()
        };
        let text = records(&response);
        assert_eq!(text, "reqid\x1f7\x1emessages.#\x1f2\x1emessages.0\x1f\x1emessages.1\x1f\x1emessages.1.enabled\x1ffalse\x1eapi\x1f\x1eapi.precautions\x1f\x1e");
        assert_eq!(from_records::<ConfigResponse>(&text), response);
    }

    // The other side writes the names of its message classes: they are
    // read without case and underscores.
    #[test]
    fn the_names_of_the_message_classes_are_read() {
        let text = "reqId\x1f9\x1eapi\x1f\x1eapi.settings\x1f\x1eapi.settings.showForexDataIn1_10Pips\x1ftrue\x1eapi.settings.socketPort\x1f4002\x1e\
                    api.settings.trustedIPs.#\x1f2\x1eapi.settings.trustedIPs.0\x1f127.0.0.1\x1eapi.settings.trustedIPs.1\x1f10.0.0.1\x1e\
                    acceptedWarnings.#\x1f1\x1eacceptedWarnings.0\x1f\x1eacceptedWarnings.0.messageId\x1f131\x1eresetAPIOrderSequence\x1ffalse\x1e\
                    lockAndExit\x1f\x1elockAndExit.autoLogoffTime\x1f23:45\x1e";
        let request: UpdateConfigRequest = from_records(text);
        let settings = request.api.as_ref().and_then(|a| a.settings.as_ref()).expect("the settings");
        assert_eq!(request.req_id, Some(9));
        assert_eq!(settings.show_forex_data_in_1_10_pips, Some(true));
        assert_eq!(settings.socket_port, Some(4002));
        assert_eq!(settings.trusted_ips, vec!["127.0.0.1".to_string(), "10.0.0.1".to_string()]);
        assert_eq!(request.accepted_warnings.len(), 1);
        assert_eq!(request.accepted_warnings[0].message_id, Some(131));
        assert_eq!(request.reset_api_order_sequence, Some(false));
        assert_eq!(request.lock_and_exit.as_ref().and_then(|l| l.auto_logoff_time.as_deref()), Some("23:45"));
        assert!(request.orders.is_none() && request.messages.is_empty());
        // A number or a flag that does not read is not set.
        let odd: ConfigRequest = from_records("reqId\x1fseven\x1e");
        assert_eq!(odd.req_id, None);
    }

    // A request made through the C entry is answered by the pass: here
    // with the error of a session whose level has no configuration.
    #[test]
    fn a_request_through_the_c_entry_is_answered() {
        use crate::api::wrapper::tests::RecordingWrapper;
        let level_before = crate::client_core::session_level_for_test();
        crate::client_core::set_session_level_for_test(Some(200));
        let (tx, _rx) = crossbeam_channel::unbounded();
        let shared = std::sync::Arc::new(crate::bridge::SharedState::new());
        let client = crate::api::EClient::from_parts(shared, tx, std::thread::spawn(|| {}), "A1".into());
        let mut session = IbxClient::new(client, None);
        let text = records(&ConfigRequest { req_id: Some(31) });
        assert_eq!(unsafe { ibx_req_config(&mut session, IbxStr::of(&text)) }, 0);
        let text = records(&UpdateConfigRequest { req_id: Some(32), ..Default::default() });
        assert_eq!(unsafe { ibx_update_config(&mut session, IbxStr::of(&text)) }, 0);
        assert_eq!(unsafe { ibx_req_config(std::ptr::null_mut(), IbxStr::of("")) }, 1);
        let mut wrapper = RecordingWrapper::default();
        super::super::pass(&session, &mut wrapper);
        crate::client_core::set_session_level_for_test(level_before);
        assert!(wrapper.events.iter().any(|e| e.starts_with("error:31:503:")), "{:?}", wrapper.events);
        assert!(wrapper.events.iter().any(|e| e.starts_with("error:32:503:")), "{:?}", wrapper.events);
    }
}
