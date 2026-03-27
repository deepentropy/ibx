//! Methods that are either gateway-local (no FIX round-trip) or not yet
//! supported by the engine. These exist for API surface parity with the
//! Python bindings.

use crate::api::wrapper::Wrapper;
use crate::types::{SmartComponent, NewsProvider, SoftDollarTier, FamilyCode};

use super::EClient;

/// US equity exchanges used in SMART routing (from ib-agent#86 capture).
const US_EQUITY_EXCHANGES: &[(&str, &str)] = &[
    ("NASDAQ", "Q"), ("NYSE", "N"), ("ARCA", "P"), ("BATS", "Z"),
    ("IEX", "V"), ("BEX", "B"), ("BYX", "Y"), ("NYSENAT", "C"),
    ("DRCTEDGE", "J"), ("MEMX", "U"), ("PEARL", "H"), ("AMEX", "A"),
    ("CHX", "M"), ("LTSE", "L"), ("PSX", "X"), ("ISE", "I"), ("EDGEA", "K"),
];

/// Known news providers (matches Gateway-local list).
const NEWS_PROVIDERS: &[(&str, &str)] = &[
    ("BRFG", "Briefing.com General Market Columns"),
    ("BRFUPDN", "Briefing.com Analyst Actions"),
    ("DJ-N", "Dow Jones Global Equity Trader"),
    ("DJ-RTA", "Dow Jones Top Stories Asia Pacific"),
    ("DJ-RTE", "Dow Jones Top Stories Europe"),
    ("DJ-RTG", "Dow Jones Top Stories Global"),
    ("DJ-RTPRO", "Dow Jones Top Stories Pro"),
    ("DJNL", "Dow Jones Newsletters"),
];

impl EClient {
    // ── Smart Components ──

    /// Request smart routing components for a BBO exchange. Matches `reqSmartComponents` in C++.
    /// Gateway-local — returns the component exchanges that make up SMART routing.
    pub fn req_smart_components(&self, req_id: i64, _bbo_exchange: &str, wrapper: &mut impl Wrapper) {
        let components: Vec<SmartComponent> = US_EQUITY_EXCHANGES.iter().enumerate()
            .map(|(i, (exch, letter))| SmartComponent {
                bit_number: i as i32,
                exchange: exch.to_string(),
                exchange_letter: letter.to_string(),
            })
            .collect();
        wrapper.smart_components(req_id, &components);
    }

    // ── News Providers ──

    /// Request available news providers. Matches `reqNewsProviders` in C++.
    /// Gateway-local — returns the static provider list.
    pub fn req_news_providers(&self, wrapper: &mut impl Wrapper) {
        let providers: Vec<NewsProvider> = NEWS_PROVIDERS.iter()
            .map(|(code, name)| NewsProvider { code: code.to_string(), name: name.to_string() })
            .collect();
        wrapper.news_providers(&providers);
    }

    // ── Server Time ──

    /// Request current server time. Matches `reqCurrentTime` in C++.
    /// Returns local system time (no server round-trip).
    pub fn req_current_time(&self, wrapper: &mut impl Wrapper) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        wrapper.current_time(now);
    }

    // ── FA (Financial Advisor) ──

    /// Request FA data. Not yet implemented.
    pub fn request_fa(&self, _fa_data_type: i32) {
        log::warn!("request_fa: not yet implemented — needs FIX capture");
    }

    /// Replace FA data. Not yet implemented.
    pub fn replace_fa(&self, _req_id: i64, _fa_data_type: i32, _cxml: &str) {
        log::warn!("replace_fa: not yet implemented — needs FIX capture");
    }

    // ── Display Groups ──

    /// Query display groups. Not yet implemented.
    pub fn query_display_groups(&self, _req_id: i64) {}

    /// Subscribe to display group events. Not yet implemented.
    pub fn subscribe_to_group_events(&self, _req_id: i64, _group_id: i32) {}

    /// Unsubscribe from display group events. Not yet implemented.
    pub fn unsubscribe_from_group_events(&self, _req_id: i64) {}

    /// Update display group. Not yet implemented.
    pub fn update_display_group(&self, _req_id: i64, _contract_info: &str) {}

    // ── Soft Dollar Tiers ──

    /// Request soft dollar tiers. Matches `reqSoftDollarTiers` in C++.
    /// Gateway-local — returns tiers from CCP logon tag 6560.
    pub fn req_soft_dollar_tiers(&self, req_id: i64, wrapper: &mut impl Wrapper) {
        const TIERS: &[(&str, &str, &str)] = &[
            ("MaxRebate", "1", "Maximize Rebate"),
            ("PreferRebate", "9", "Prefer Rebate"),
            ("PreferFill", "11", "Prefer Fill"),
            ("MaxFill", "12", "Maximize Fill"),
            ("Primary", "2", "Primary Exchange"),
            ("VRebate", "3", "Highest Volume Exchange With Rebate"),
            ("VLowFee", "4", "High Volume Exchange With Lowest Fee"),
        ];
        let tiers: Vec<SoftDollarTier> = TIERS.iter()
            .map(|(name, val, display)| SoftDollarTier {
                name: name.to_string(),
                val: val.to_string(),
                display_name: display.to_string(),
            })
            .collect();
        wrapper.soft_dollar_tiers(req_id, &tiers);
    }

    // ── Family Codes ──

    /// Request family codes. Matches `reqFamilyCodes` in C++.
    /// Gateway-local — returns codes from CCP logon tag 6823.
    /// Empty for paper/single accounts.
    pub fn req_family_codes(&self, wrapper: &mut impl Wrapper) {
        // Tag 6823 is empty for paper/single accounts.
        // Multi-account setups populate with accountID/familyCodeStr tuples.
        let codes: Vec<FamilyCode> = Vec::new();
        wrapper.family_codes(&codes);
    }

    // ── Server Log Level ──

    /// Set server log level. Matches `setServerLogLevel` in C++.
    pub fn set_server_log_level(&self, log_level: i32) {
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
    /// Gateway-local — returns whiteBrandingId (empty for standard accounts).
    pub fn req_user_info(&self, req_id: i64, wrapper: &mut impl Wrapper) {
        // Empty for standard IB accounts. Non-empty only for white-labeled brokers.
        wrapper.user_info(req_id, "");
    }

    // ── WSH ──

    /// Request WSH metadata. Not yet implemented.
    pub fn req_wsh_meta_data(&self, _req_id: i64) {
        log::warn!("req_wsh_meta_data: not yet implemented — needs FIX capture");
    }

    /// Request WSH event data. Not yet implemented.
    pub fn req_wsh_event_data(&self, _req_id: i64) {
        log::warn!("req_wsh_event_data: not yet implemented — needs FIX capture");
    }
}
