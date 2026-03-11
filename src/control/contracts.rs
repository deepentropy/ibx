//! Contract/security definition lookups via CCP (FIX 35=c request, 35=d response).
//!
//! IB routes secdef requests through CCP, not directly to secdefil farms.
//! Key tag mappings: STK→CS (SecurityType), SMART→BEST (Exchange).

use std::collections::HashMap;

use crate::protocol::fix::{self, TAG_MSG_TYPE};

// FIX tags for security definitions
pub const TAG_SECURITY_REQ_ID: u32 = 320;
pub const TAG_SECURITY_REQ_TYPE: u32 = 321;
pub const TAG_SECURITY_RESPONSE_TYPE: u32 = 323;
pub const TAG_SYMBOL: u32 = 55;
pub const TAG_SECURITY_TYPE: u32 = 167;
pub const TAG_EXCHANGE: u32 = 100;
pub const TAG_CURRENCY: u32 = 15;
pub const TAG_LAST_TRADE_DATE: u32 = 200;
pub const TAG_RIGHT: u32 = 201;
pub const TAG_STRIKE: u32 = 202;
pub const TAG_SECURITY_EXCHANGE: u32 = 207;
pub const TAG_MULTIPLIER: u32 = 231;
pub const TAG_LONG_NAME: u32 = 306;
pub const TAG_SECURITY_ID: u32 = 455;
pub const TAG_SECURITY_ID_SOURCE: u32 = 456;

// IB custom tags
pub const TAG_IB_CON_ID: u32 = 6008;
pub const TAG_IB_LOCAL_SYMBOL: u32 = 6035;
pub const TAG_IB_VALID_EXCHANGES: u32 = 6046;
pub const TAG_IB_TRADING_CLASS: u32 = 6058;
pub const TAG_IB_SOURCE: u32 = 6088;
pub const TAG_IB_PRIMARY_EXCHANGE: u32 = 6470;
pub const TAG_IB_MIN_TICK: u32 = 6019;
pub const TAG_IB_ORDER_TYPES: u32 = 6431;
pub const TAG_IB_STOCK_TYPE: u32 = 8077;

/// Security types (IB internal encoding).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecurityType {
    Stock,    // CS
    Option,   // OPT
    Future,   // FUT
    Forex,    // CASH
    Index,    // IND
    Bond,     // BOND
    Warrant,  // WAR
    Other,
}

impl SecurityType {
    /// Convert from TWS API string to FIX wire format.
    pub fn to_fix(&self) -> &'static str {
        match self {
            Self::Stock => "CS",
            Self::Option => "OPT",
            Self::Future => "FUT",
            Self::Forex => "CASH",
            Self::Index => "IND",
            Self::Bond => "BOND",
            Self::Warrant => "WAR",
            Self::Other => "CS",
        }
    }

    /// Parse from FIX wire format.
    pub fn from_fix(s: &str) -> Self {
        match s {
            "CS" | "STK" => Self::Stock,
            "OPT" => Self::Option,
            "FUT" => Self::Future,
            "CASH" => Self::Forex,
            "IND" => Self::Index,
            "BOND" => Self::Bond,
            "WAR" => Self::Warrant,
            _ => Self::Other,
        }
    }
}

/// Option right (call/put).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptionRight {
    Call,
    Put,
}

/// Full contract definition from a 35=d response.
#[derive(Debug, Clone)]
pub struct ContractDefinition {
    pub con_id: u32,
    pub symbol: String,
    pub sec_type: SecurityType,
    pub exchange: String,
    pub primary_exchange: String,
    pub currency: String,
    pub local_symbol: String,
    pub trading_class: String,
    pub long_name: String,
    pub min_tick: f64,
    pub multiplier: f64,
    pub valid_exchanges: Vec<String>,
    pub order_types: Vec<String>,
    // Options/futures specific
    pub last_trade_date: String,
    pub strike: f64,
    pub right: Option<OptionRight>,
}

impl Default for ContractDefinition {
    fn default() -> Self {
        Self {
            con_id: 0,
            symbol: String::new(),
            sec_type: SecurityType::Stock,
            exchange: String::new(),
            primary_exchange: String::new(),
            currency: String::new(),
            local_symbol: String::new(),
            trading_class: String::new(),
            long_name: String::new(),
            min_tick: 0.01,
            multiplier: 1.0,
            valid_exchanges: Vec::new(),
            order_types: Vec::new(),
            last_trade_date: String::new(),
            strike: 0.0,
            right: None,
        }
    }
}

/// Map TWS exchange name to FIX wire format.
pub fn exchange_to_fix(exchange: &str) -> &str {
    match exchange {
        "SMART" => "BEST",
        other => other,
    }
}

/// Map FIX exchange name back to TWS format.
pub fn exchange_from_fix(exchange: &str) -> &str {
    match exchange {
        "BEST" => "SMART",
        other => other,
    }
}

/// Build a FIX 35=c SecurityDefinitionRequest by conId.
pub fn build_secdef_request_by_conid(req_id: &str, con_id: u32, seq: u32) -> Vec<u8> {
    let con_id_str = con_id.to_string();
    fix::fix_build(
        &[
            (TAG_MSG_TYPE, "c"),
            (TAG_SECURITY_REQ_ID, req_id),
            (TAG_SECURITY_REQ_TYPE, "2"),
            (TAG_IB_CON_ID, &con_id_str),
            (TAG_IB_SOURCE, "Socket"),
        ],
        seq,
    )
}

/// Build a FIX 35=c SecurityDefinitionRequest by symbol.
pub fn build_secdef_request_by_symbol(
    req_id: &str,
    symbol: &str,
    sec_type: SecurityType,
    exchange: &str,
    currency: &str,
    seq: u32,
) -> Vec<u8> {
    fix::fix_build(
        &[
            (TAG_MSG_TYPE, "c"),
            (TAG_SECURITY_REQ_ID, req_id),
            (TAG_SECURITY_REQ_TYPE, "2"),
            (TAG_SYMBOL, symbol),
            (TAG_SECURITY_TYPE, sec_type.to_fix()),
            (TAG_EXCHANGE, exchange_to_fix(exchange)),
            (TAG_CURRENCY, currency),
            (TAG_IB_SOURCE, "Socket"),
        ],
        seq,
    )
}

/// Parse a FIX 35=d SecurityDefinition response into a ContractDefinition.
pub fn parse_secdef_response(data: &[u8]) -> Option<ContractDefinition> {
    let tags = fix::fix_parse(data);

    // Verify it's a 35=d message
    if tags.get(&TAG_MSG_TYPE).map(|s| s.as_str()) != Some("d") {
        return None;
    }

    let mut def = ContractDefinition::default();

    if let Some(v) = tags.get(&TAG_IB_CON_ID) {
        def.con_id = v.parse().unwrap_or(0);
    }
    if let Some(v) = tags.get(&TAG_SYMBOL) {
        def.symbol = v.clone();
    }
    if let Some(v) = tags.get(&TAG_SECURITY_TYPE) {
        def.sec_type = SecurityType::from_fix(v);
    }
    if let Some(v) = tags.get(&TAG_SECURITY_EXCHANGE) {
        def.exchange = exchange_from_fix(v).to_string();
    }
    if let Some(v) = tags.get(&TAG_IB_PRIMARY_EXCHANGE) {
        def.primary_exchange = exchange_from_fix(v).to_string();
    }
    if let Some(v) = tags.get(&TAG_CURRENCY) {
        def.currency = v.clone();
    }
    if let Some(v) = tags.get(&TAG_IB_LOCAL_SYMBOL) {
        def.local_symbol = v.clone();
    }
    if let Some(v) = tags.get(&TAG_IB_TRADING_CLASS) {
        def.trading_class = v.clone();
    }
    if let Some(v) = tags.get(&TAG_LONG_NAME) {
        def.long_name = v.clone();
    }
    if let Some(v) = tags.get(&TAG_IB_MIN_TICK) {
        def.min_tick = v.parse().unwrap_or(0.01);
    }
    if let Some(v) = tags.get(&TAG_MULTIPLIER) {
        def.multiplier = v.parse().unwrap_or(1.0);
    }
    if let Some(v) = tags.get(&TAG_IB_VALID_EXCHANGES) {
        def.valid_exchanges = v.split(',').map(|s| exchange_from_fix(s).to_string()).collect();
    }
    if let Some(v) = tags.get(&TAG_IB_ORDER_TYPES) {
        def.order_types = v.split(',').map(|s| s.to_string()).collect();
    }
    if let Some(v) = tags.get(&TAG_LAST_TRADE_DATE) {
        def.last_trade_date = v.clone();
    }
    if let Some(v) = tags.get(&TAG_STRIKE) {
        def.strike = v.parse().unwrap_or(0.0);
    }
    if let Some(v) = tags.get(&TAG_RIGHT) {
        def.right = match v.as_str() {
            "C" => Some(OptionRight::Call),
            "P" => Some(OptionRight::Put),
            _ => None,
        };
    }

    Some(def)
}

/// Extract the SecurityReqID from a 35=d response to match with the original request.
pub fn secdef_response_req_id(data: &[u8]) -> Option<String> {
    let tags = fix::fix_parse(data);
    tags.get(&TAG_SECURITY_REQ_ID).cloned()
}

/// Check if a 35=d response is the last one (response type 5 or 6).
pub fn secdef_response_is_last(data: &[u8]) -> bool {
    let tags = fix::fix_parse(data);
    matches!(
        tags.get(&TAG_SECURITY_RESPONSE_TYPE).map(|s| s.as_str()),
        Some("5") | Some("6")
    )
}

/// Cache of contract definitions by conId.
#[derive(Debug, Default)]
pub struct ContractStore {
    by_con_id: HashMap<u32, ContractDefinition>,
    by_symbol: HashMap<String, u32>,
}

impl ContractStore {
    pub fn insert(&mut self, def: ContractDefinition) {
        let key = format!("{}:{}:{}", def.symbol, def.sec_type.to_fix(), def.currency);
        self.by_symbol.insert(key, def.con_id);
        self.by_con_id.insert(def.con_id, def);
    }

    pub fn get(&self, con_id: u32) -> Option<&ContractDefinition> {
        self.by_con_id.get(&con_id)
    }

    pub fn find(&self, symbol: &str, sec_type: SecurityType, currency: &str) -> Option<&ContractDefinition> {
        let key = format!("{}:{}:{}", symbol, sec_type.to_fix(), currency);
        self.by_symbol.get(&key).and_then(|id| self.by_con_id.get(id))
    }

    pub fn len(&self) -> usize {
        self.by_con_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_con_id.is_empty()
    }
}

// ─── Schedule subscription (35=U|6040=107) ───

/// FIX tags for schedule subscription responses.
pub const TAG_SUB_PROTOCOL: u32 = 6040;
pub const TAG_SCHEDULE_TIMEZONE: u32 = 6734;
pub const TAG_SESSION_COUNT: u32 = 6840;
pub const TAG_SESSION_START: u32 = 6841;
pub const TAG_SESSION_END: u32 = 6842;
pub const TAG_TRADE_DATE: u32 = 75;
pub const TAG_IS_TRADING_HOURS: u32 = 6843;
pub const TAG_IS_LIQUID_HOURS: u32 = 6844;

/// A single trading/liquid hours session.
#[derive(Debug, Clone, PartialEq)]
pub struct ScheduleSession {
    pub start: String,
    pub end: String,
    pub trade_date: String,
}

/// Parsed schedule from a 35=U|6040=107 response.
#[derive(Debug, Clone)]
pub struct ContractSchedule {
    pub timezone: String,
    pub trading_hours: Vec<ScheduleSession>,
    pub liquid_hours: Vec<ScheduleSession>,
}

/// Parse a 35=U|6040=107 schedule response into trading/liquid hours.
///
/// Uses sequential tag parsing since sessions are a repeating group.
pub fn parse_schedule_response(data: &[u8]) -> Option<ContractSchedule> {
    use crate::protocol::fix::SOH;

    // Sequential parse: collect all tag-value pairs in order
    let mut tags: Vec<(u32, String)> = Vec::new();
    for part in data.split(|&b| b == SOH) {
        if part.is_empty() { continue; }
        let text = String::from_utf8_lossy(part);
        if let Some((tag_str, val)) = text.split_once('=') {
            if let Ok(tag) = tag_str.parse::<u32>() {
                tags.push((tag, val.to_string()));
            }
        }
    }

    // Verify this is 35=U with 6040=107
    let msg_type = tags.iter().find(|(t, _)| *t == fix::TAG_MSG_TYPE)?.1.as_str();
    if msg_type != "U" { return None; }
    let sub_protocol = tags.iter().find(|(t, _)| *t == TAG_SUB_PROTOCOL)?.1.as_str();
    if sub_protocol != "107" { return None; }

    let timezone = tags.iter()
        .find(|(t, _)| *t == TAG_SCHEDULE_TIMEZONE)
        .map(|(_, v)| v.clone())
        .unwrap_or_default();

    // Parse repeating session groups.
    // Each session starts with tag 6841 (start) and includes 6842 (end), 75 (date),
    // and either 6843 (trading) or 6844 (liquid).
    let mut trading_hours = Vec::new();
    let mut liquid_hours = Vec::new();

    let mut start = String::new();
    let mut end = String::new();
    let mut trade_date = String::new();
    let mut is_trading = false;
    let mut is_liquid = false;
    let mut in_session = false;

    for (tag, val) in &tags {
        match *tag {
            TAG_SESSION_START => {
                // Flush previous session if any
                if in_session {
                    let session = ScheduleSession {
                        start: start.clone(), end: end.clone(), trade_date: trade_date.clone(),
                    };
                    if is_trading { trading_hours.push(session); }
                    else if is_liquid { liquid_hours.push(session); }
                }
                start = val.clone();
                end.clear();
                trade_date.clear();
                is_trading = false;
                is_liquid = false;
                in_session = true;
            }
            TAG_SESSION_END => end = val.clone(),
            TAG_TRADE_DATE => trade_date = val.clone(),
            TAG_IS_TRADING_HOURS => is_trading = val == "1",
            TAG_IS_LIQUID_HOURS => is_liquid = val == "1",
            _ => {}
        }
    }
    // Flush last session
    if in_session {
        let session = ScheduleSession {
            start: start, end: end, trade_date: trade_date,
        };
        if is_trading { trading_hours.push(session); }
        else if is_liquid { liquid_hours.push(session); }
    }

    Some(ContractSchedule { timezone, trading_hours, liquid_hours })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn security_type_roundtrip() {
        for st in [
            SecurityType::Stock,
            SecurityType::Option,
            SecurityType::Future,
            SecurityType::Forex,
        ] {
            assert_eq!(SecurityType::from_fix(st.to_fix()), st);
        }
    }

    #[test]
    fn exchange_mapping() {
        assert_eq!(exchange_to_fix("SMART"), "BEST");
        assert_eq!(exchange_to_fix("NYSE"), "NYSE");
        assert_eq!(exchange_from_fix("BEST"), "SMART");
        assert_eq!(exchange_from_fix("ARCA"), "ARCA");
    }

    #[test]
    fn build_secdef_by_conid() {
        let msg = build_secdef_request_by_conid("R1", 265598, 1);
        let tags = fix::fix_parse(&msg);
        assert_eq!(tags[&TAG_MSG_TYPE], "c");
        assert_eq!(tags[&TAG_SECURITY_REQ_ID], "R1");
        assert_eq!(tags[&TAG_SECURITY_REQ_TYPE], "2");
        assert_eq!(tags[&TAG_IB_CON_ID], "265598");
        assert_eq!(tags[&TAG_IB_SOURCE], "Socket");
    }

    #[test]
    fn build_secdef_by_symbol() {
        let msg = build_secdef_request_by_symbol("R2", "AAPL", SecurityType::Stock, "SMART", "USD", 2);
        let tags = fix::fix_parse(&msg);
        assert_eq!(tags[&TAG_MSG_TYPE], "c");
        assert_eq!(tags[&TAG_SYMBOL], "AAPL");
        assert_eq!(tags[&TAG_SECURITY_TYPE], "CS");
        assert_eq!(tags[&TAG_EXCHANGE], "BEST"); // SMART→BEST
        assert_eq!(tags[&TAG_CURRENCY], "USD");
    }

    #[test]
    fn parse_secdef_response() {
        // Build a fake 35=d response
        let msg = fix::fix_build(
            &[
                (TAG_MSG_TYPE, "d"),
                (TAG_SECURITY_REQ_ID, "R1"),
                (TAG_SECURITY_RESPONSE_TYPE, "4"),
                (TAG_IB_CON_ID, "265598"),
                (TAG_SYMBOL, "AAPL"),
                (TAG_SECURITY_TYPE, "CS"),
                (TAG_SECURITY_EXCHANGE, "NASDAQ"),
                (TAG_CURRENCY, "USD"),
                (TAG_LONG_NAME, "APPLE INC"),
                (TAG_IB_MIN_TICK, "0.01"),
                (TAG_IB_VALID_EXCHANGES, "BEST,NYSE,ARCA"),
                (TAG_IB_PRIMARY_EXCHANGE, "NASDAQ"),
            ],
            1,
        );
        let def = super::parse_secdef_response(&msg).unwrap();
        assert_eq!(def.con_id, 265598);
        assert_eq!(def.symbol, "AAPL");
        assert_eq!(def.sec_type, SecurityType::Stock);
        assert_eq!(def.exchange, "NASDAQ");
        assert_eq!(def.currency, "USD");
        assert_eq!(def.long_name, "APPLE INC");
        assert_eq!(def.min_tick, 0.01);
        assert_eq!(def.valid_exchanges, vec!["SMART", "NYSE", "ARCA"]);
        assert_eq!(def.primary_exchange, "NASDAQ");
    }

    #[test]
    fn parse_rejects_non_secdef() {
        let msg = fix::fix_build(&[(TAG_MSG_TYPE, "A")], 1);
        assert!(super::parse_secdef_response(&msg).is_none());
    }

    #[test]
    fn secdef_response_last_check() {
        let msg5 = fix::fix_build(
            &[(TAG_MSG_TYPE, "d"), (TAG_SECURITY_RESPONSE_TYPE, "5")],
            1,
        );
        let msg4 = fix::fix_build(
            &[(TAG_MSG_TYPE, "d"), (TAG_SECURITY_RESPONSE_TYPE, "4")],
            2,
        );
        assert!(secdef_response_is_last(&msg5));
        assert!(!secdef_response_is_last(&msg4));
    }

    #[test]
    fn contract_store_insert_and_lookup() {
        let mut store = ContractStore::default();
        let def = ContractDefinition {
            con_id: 265598,
            symbol: "AAPL".to_string(),
            sec_type: SecurityType::Stock,
            currency: "USD".to_string(),
            exchange: "NASDAQ".to_string(),
            ..Default::default()
        };
        store.insert(def);

        assert_eq!(store.len(), 1);
        let found = store.get(265598).unwrap();
        assert_eq!(found.symbol, "AAPL");

        let by_sym = store.find("AAPL", SecurityType::Stock, "USD").unwrap();
        assert_eq!(by_sym.con_id, 265598);

        assert!(store.find("MSFT", SecurityType::Stock, "USD").is_none());
    }

    #[test]
    fn contract_store_update_replaces() {
        let mut store = ContractStore::default();
        store.insert(ContractDefinition {
            con_id: 265598,
            symbol: "AAPL".to_string(),
            long_name: "OLD".to_string(),
            ..Default::default()
        });
        store.insert(ContractDefinition {
            con_id: 265598,
            symbol: "AAPL".to_string(),
            long_name: "APPLE INC".to_string(),
            ..Default::default()
        });
        assert_eq!(store.len(), 1);
        assert_eq!(store.get(265598).unwrap().long_name, "APPLE INC");
    }

    #[test]
    fn option_contract_fields() {
        let msg = fix::fix_build(
            &[
                (TAG_MSG_TYPE, "d"),
                (TAG_IB_CON_ID, "12345"),
                (TAG_SYMBOL, "AAPL"),
                (TAG_SECURITY_TYPE, "OPT"),
                (TAG_LAST_TRADE_DATE, "20260321"),
                (TAG_STRIKE, "200.0"),
                (TAG_RIGHT, "C"),
                (TAG_MULTIPLIER, "100"),
            ],
            1,
        );
        let def = super::parse_secdef_response(&msg).unwrap();
        assert_eq!(def.sec_type, SecurityType::Option);
        assert_eq!(def.last_trade_date, "20260321");
        assert_eq!(def.strike, 200.0);
        assert_eq!(def.right, Some(OptionRight::Call));
        assert_eq!(def.multiplier, 100.0);
    }

    #[test]
    fn parse_schedule_response_basic() {
        // Build a fake 35=U|6040=107 with 2 trading + 2 liquid sessions
        let msg = fix::fix_build(
            &[
                (TAG_MSG_TYPE, "U"),
                (TAG_SUB_PROTOCOL, "107"),
                (TAG_SCHEDULE_TIMEZONE, "US/Eastern"),
                (TAG_SESSION_COUNT, "4"),
                // Trading session 1
                (TAG_SESSION_START, "20260311-08:00:00"),
                (TAG_SESSION_END, "20260312-00:00:00"),
                (TAG_TRADE_DATE, "20260311"),
                (TAG_IS_TRADING_HOURS, "1"),
                // Liquid session 1
                (TAG_SESSION_START, "20260311-13:30:00"),
                (TAG_SESSION_END, "20260311-20:00:00"),
                (TAG_TRADE_DATE, "20260311"),
                (TAG_IS_LIQUID_HOURS, "1"),
                // Trading session 2
                (TAG_SESSION_START, "20260312-08:00:00"),
                (TAG_SESSION_END, "20260313-00:00:00"),
                (TAG_TRADE_DATE, "20260312"),
                (TAG_IS_TRADING_HOURS, "1"),
                // Liquid session 2
                (TAG_SESSION_START, "20260312-13:30:00"),
                (TAG_SESSION_END, "20260312-20:00:00"),
                (TAG_TRADE_DATE, "20260312"),
                (TAG_IS_LIQUID_HOURS, "1"),
            ],
            1,
        );
        let sched = parse_schedule_response(&msg).unwrap();
        assert_eq!(sched.timezone, "US/Eastern");
        assert_eq!(sched.trading_hours.len(), 2);
        assert_eq!(sched.liquid_hours.len(), 2);

        assert_eq!(sched.trading_hours[0].start, "20260311-08:00:00");
        assert_eq!(sched.trading_hours[0].end, "20260312-00:00:00");
        assert_eq!(sched.trading_hours[0].trade_date, "20260311");

        assert_eq!(sched.liquid_hours[0].start, "20260311-13:30:00");
        assert_eq!(sched.liquid_hours[0].end, "20260311-20:00:00");
    }

    #[test]
    fn parse_schedule_rejects_non_schedule() {
        let msg = fix::fix_build(&[(TAG_MSG_TYPE, "d")], 1);
        assert!(parse_schedule_response(&msg).is_none());

        // Wrong sub-protocol
        let msg = fix::fix_build(
            &[(TAG_MSG_TYPE, "U"), (TAG_SUB_PROTOCOL, "100")],
            1,
        );
        assert!(parse_schedule_response(&msg).is_none());
    }
}
