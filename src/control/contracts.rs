//! Contract/security definition lookups via the auth connection.
//!
//! Key tag mappings: STK→CS (SecurityType), SMART→BEST (Exchange).

use std::collections::HashMap;

use crate::protocol::fix::{self, TAG_MSG_TYPE};

// Tags for security definitions
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
pub const TAG_IB_ORDER_TYPES: u32 = 6431;
pub const TAG_IB_MARKET_RULE_ID: u32 = 6031;
/// Stock type text of a definition (ibx#436).
pub const TAG_IB_STOCK_TYPE_TEXT: u32 = 8273;

// Market rule table (ibx#437).
/// Number of rules in the table; the table follows it.
pub const TAG_MARKET_RULE_COUNT: u32 = 6019;
/// Rule id: starts a rule and closes the previous one.
pub const TAG_MARKET_RULE_ID: u32 = 6031;
/// Low edge of an entry (the last one read is used by the next increment).
pub const TAG_LOW_EDGE: u32 = 6023;
/// Increment of an entry.
pub const TAG_INCREMENT: u32 = 6027;
/// Number of price increment entries of a rule (its first increment set).
pub const TAG_PRICE_INCREMENT_COUNT: u32 = 6026;
/// Number of size increment entries of a rule (its second set, ibx#287).
pub const TAG_SIZE_INCREMENT_COUNT: u32 = 6030;
/// First and last tag of a rule's fields.
const MARKET_RULE_FIELDS: std::ops::RangeInclusive<u32> = 6020..=6031;
/// Market type of the contract, for example USSTK.
pub const TAG_IB_MARKET_TYPE: u32 = 6523;

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
    /// Official API string ("STK", "OPT", ...). THE single mapping for
    /// everything user-visible — the callbacks previously reported a Debug
    /// derive ("Stock"), which no request path accepts, so a returned
    /// Contract could not be fed back into another call (ibx#230).
    /// `Other` maps to "" on purpose: an instrument the engine could not
    /// classify must not masquerade as a stock — the order path is
    /// STK-only and that one wrong guess would not be caught downstream.
    pub fn to_api_str(&self) -> &'static str {
        match self {
            Self::Stock => "STK",
            Self::Option => "OPT",
            Self::Future => "FUT",
            Self::Forex => "CASH",
            Self::Index => "IND",
            Self::Bond => "BOND",
            Self::Warrant => "WAR",
            Self::Other => "",
        }
    }

    /// Convert to the wire encoding.
    pub fn to_fix(&self) -> &'static str {
        match self {
            Self::Stock => "CS",
            Self::Option => "OPT",
            Self::Future => "FUT",
            Self::Forex => "CASH",
            Self::Index => "IND",
            Self::Bond => "BOND",
            Self::Warrant => "WAR",
            // An unrecognized security type must not be sent as a stock —
            // that misroutes the request silently (ibx#223). Empty draws a
            // visible gateway error instead, matching its own
            // unknown-to-none handling.
            Self::Other => "",
        }
    }

    /// Parse from wire format.
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

/// Full contract definition.
#[derive(Debug, Clone)]
pub struct ContractDefinition {
    pub con_id: i64,
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
    pub market_rule_id: Option<u32>,
    /// Market rule id of each valid exchange whose rule is known,
    /// comma-joined in valid-exchange order (the API marketRuleIds).
    pub market_rule_ids: String,
    // Options/futures specific
    pub last_trade_date: String,
    pub strike: f64,
    pub right: Option<OptionRight>,
    // Extended fields
    pub stock_type: String,
    pub category: String,
    pub country: String,
    pub market_name: String,
    pub isin: String,
    /// Trading session string. Populated by merging the paired schedule reply.
    pub trading_hours: Option<String>,
    /// Liquid (regular-session) hours string. Same source as trading_hours.
    pub liquid_hours: Option<String>,
    /// IANA timezone for session times (e.g. "US/Eastern").
    pub time_zone_id: Option<String>,
    /// Exchange-path join key (tag 6256) used to pair secdef ↔ schedule replies.
    /// Internal — not exposed on the public API surface.
    pub join_key: String,
    /// conId of the underlying (the API underConId), 0 when absent.
    pub under_con_id: i64,
    /// Exercise style of an option: 1 American, 2 European, 0 unknown.
    pub exercise_style: u8,
    /// Last trading time of a derivative, `HHMM`, empty when absent.
    pub last_trade_time: String,
    /// API security type of the underlying, empty when absent.
    pub under_sec_type: String,
    /// Security type as the wire names it (`CS`, `OPT`, `FOP`, ...).
    pub wire_sec_type: String,
    /// A record of a continuous futures lookup: the API security type is
    /// `CONTFUT` (ibx#438).
    pub continuous: bool,
    // ── ibx#436: the other ContractDetails sources ──
    /// Multiplier as received (tag 231), empty when absent.
    pub multiplier_text: String,
    /// Contract month: tag 200, else the month of tag 541.
    pub contract_month: String,
    /// Real expiration date (tag 6614).
    pub real_expiration_date: String,
    /// Symbol of the underlying (detail 6855).
    pub under_symbol: String,
    /// Aggregate group (tag 6178), None when absent.
    pub agg_group: Option<i32>,
    /// CUSIP of the alternative id group (455 with 456=1).
    pub cusip: String,
    /// Second and third parts of the industry text.
    pub subcategory: String,
    pub industry: String,
    /// Exchange suffix of the primary exchange (detail 8224).
    pub primary_suffix: String,
    /// EV rule as received (detail 6858, `name:value`).
    pub ev_rule: String,
    /// Fractional sizes allowed for the contract (detail 8193).
    pub fractional: bool,
    /// Fractional size increment (detail 8175), None when absent.
    pub fraction_size: Option<f64>,
    /// Size set and price magnifier of the record's market rule.
    pub size_increments: Vec<PriceIncrement>,
    pub price_magnifier: i32,
    /// Ineligibility reasons: (id, description) from detail 6765 and the
    /// reasons table (6767 / 6768).
    pub ineligibility: Vec<(String, String)>,
    pub fund: FundFields,
    pub bond: BondFields,
    /// Last trade date and time of the API row, from the schedule
    /// (`jclient.dy.c(lh.D)`); None when no schedule was merged.
    pub schedule_expiry: Option<(String, String)>,
    /// Logon features BONDAPI and EVAPI when the row was made.
    pub bond_api: bool,
    pub ev_api: bool,
    /// Symbol of the request when it reads as a CUSIP (the CUSIP of a bond
    /// row), else empty.
    pub lookup_cusip: String,
}

/// Fund fields of a definition (detail block, FUND only on the API).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FundFields {
    pub name: String,                      // 6481
    pub family: String,                    // 6472
    pub fund_type: String,                 // 6473
    pub front_load: String,                // 6474
    pub back_load: String,                 // 6475
    pub back_load_time_interval: String,   // 6482
    pub management_fee: String,            // 6476
    pub closed: bool,                      // 6477
    pub closed_for_new_investors: bool,    // 6511
    pub closed_for_new_money: bool,        // 6512
    pub notify_amount: String,             // 6478
    pub minimum_initial_purchase: String,  // 6479
    pub subsequent_minimum_purchase: String, // 8505
    pub blue_sky_states: String,           // 8150
    pub blue_sky_territories: String,      // 8151
    pub distribution_policy_indicator: String, // 8502
    pub asset_type: String,                // 8503
}

/// Bond fields of a definition (detail block).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BondFields {
    pub notes: String,              // 6493
    pub desc_append: String,        // 6494
    pub short_description: String,  // 6853
    pub bond_type: String,          // 6495
    pub coupon_type: String,        // 6496
    pub callable: bool,             // 6497
    pub putable: bool,              // 6498
    pub convertible: bool,          // 6499
    pub next_option_partial: bool,  // 6500
    pub next_option_date: String,   // 6501
    pub next_option_type: String,   // 6502
    pub ratings: String,            // 255
    pub coupon: String,             // 223
    pub issue_date: String,         // 225
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
            market_rule_id: None,
            market_rule_ids: String::new(),
            last_trade_date: String::new(),
            strike: 0.0,
            right: None,
            stock_type: String::new(),
            category: String::new(),
            country: String::new(),
            market_name: String::new(),
            isin: String::new(),
            trading_hours: None,
            liquid_hours: None,
            time_zone_id: None,
            join_key: String::new(),
            under_con_id: 0,
            exercise_style: 0,
            last_trade_time: String::new(),
            under_sec_type: String::new(),
            wire_sec_type: String::new(),
            continuous: false,
            multiplier_text: String::new(),
            contract_month: String::new(),
            real_expiration_date: String::new(),
            under_symbol: String::new(),
            agg_group: None,
            cusip: String::new(),
            subcategory: String::new(),
            industry: String::new(),
            primary_suffix: String::new(),
            ev_rule: String::new(),
            fractional: false,
            fraction_size: None,
            size_increments: Vec::new(),
            price_magnifier: 1,
            ineligibility: Vec::new(),
            fund: FundFields::default(),
            bond: BondFields::default(),
            schedule_expiry: None,
            bond_api: false,
            ev_api: false,
            lookup_cusip: String::new(),
        }
    }
}

/// A lookup symbol read as a CUSIP (`jfix.bh.b(String)`): 9 characters,
/// or the internal id prefix `IBCID` followed by more.
pub fn reads_as_cusip(symbol: &str) -> bool {
    let s = symbol.trim();
    s.chars().count() == 9 || (s.len() > 5 && s.starts_with("IBCID"))
}

impl ContractDefinition {
    /// API security type, as the reference names it (`jfix.eh.toString()`):
    /// `CONTFUT` for a continuous futures record, the stock code `CS` as
    /// `STK`, any other type as received (ibx#436).
    pub fn api_sec_type(&self) -> String {
        if self.continuous {
            return "CONTFUT".to_string();
        }
        self.api_type_name()
    }

    /// API name of the contract's own security type (a continuous futures
    /// record is a future).
    pub fn api_type_name(&self) -> String {
        match self.wire_sec_type.as_str() {
            "" => self.sec_type.to_api_str().to_string(),
            "CS" => "STK".to_string(),
            other => other.to_string(),
        }
    }

    /// A bond, bill or fixed income record (`jfix.eh.N()`): its rows are
    /// bond contract details.
    pub fn is_bond(&self) -> bool {
        self.sec_type == SecurityType::Bond || matches!(self.wire_sec_type.as_str(), "BOND" | "BILL" | "FIXED")
    }
}

/// Map exchange name.
pub fn exchange_to_fix(exchange: &str) -> &str {
    match exchange {
        "SMART" => "BEST",
        other => other,
    }
}

/// Map exchange name back from wire format.
pub fn exchange_from_fix(exchange: &str) -> &str {
    match exchange {
        "BEST" => "SMART",
        other => other,
    }
}

/// Build a SecurityDefinitionRequest by conId.
pub fn build_secdef_request_by_conid(req_id: &str, con_id: i64, seq: u32) -> Vec<u8> {
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

/// Build a SecurityDefinitionRequest by symbol.
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

/// Symbol as sent in a by-symbol lookup: every '/' is removed ("BRK/A"
/// becomes "BRKA"); '.' and spaces are kept as given (ibx#400).
pub fn lookup_symbol(symbol: &str) -> std::borrow::Cow<'_, str> {
    if symbol.contains('/') {
        std::borrow::Cow::Owned(symbol.replace('/', ""))
    } else {
        std::borrow::Cow::Borrowed(symbol)
    }
}

/// Parse a SecurityDefinition response into its first contract record.
///
/// `None` when the message is not a definition reply, or when it carries no
/// contract record (a "no such contract" answer): such a reply must give
/// "no security definition" to the caller, never a row with conId 0
/// (ibx#400). See [`parse_secdef_records`] for replies with several records.
pub fn parse_secdef_response(data: &[u8]) -> Option<ContractDefinition> {
    parse_secdef_records(data)?.into_iter().next()
}

/// Where a tag sits in a definition reply, read in wire order.
#[derive(Clone, Copy, PartialEq)]
enum SecdefSection {
    Header,
    Record,
    Rules,
    Details,
    OrderTypes,
    Industry,
    Ineligibility,
}

/// Parse a SecurityDefinition response into one definition per contract
/// record, in reply order (ibx#435).
///
/// Each record starts with its symbol; the detail blocks that follow the
/// records are joined to them by conId, and the order-type, industry and
/// ineligibility tables by their keys. `None` when the message is not a
/// definition reply; an empty list when it has no record.
pub fn parse_secdef_records(data: &[u8]) -> Option<Vec<ContractDefinition>> {
    use crate::protocol::fix::SOH;

    let mut is_secdef = false;
    let mut section = SecdefSection::Header;
    let mut records: Vec<Vec<(u32, &str)>> = Vec::new();
    let mut details: Vec<(i64, Vec<(u32, &str)>)> = Vec::new();
    let mut order_types: Vec<(&str, &str)> = Vec::new();
    let mut industries: Vec<(&str, &str)> = Vec::new();
    let mut reasons: Vec<(&str, &str)> = Vec::new();
    for part in data.split(|&b| b == SOH) {
        let Ok(text) = std::str::from_utf8(part) else { continue };
        let Some((tag, val)) = text.split_once('=') else { continue };
        let Ok(tag) = tag.parse::<u32>() else { continue };
        if tag == TAG_MSG_TYPE {
            is_secdef = val == "d";
            continue;
        }
        use SecdefSection::*;
        match (section, tag) {
            (Header | Record, TAG_SYMBOL) => {
                records.push(vec![(tag, val)]);
                section = Record;
            }
            (Header | Record, 146 | 6038 | TAG_MARKET_RULE_COUNT) => section = Rules,
            (Rules | Details, 6344) => section = Details,
            (Rules | Details, TAG_IB_CON_ID) => {
                details.push((val.parse().unwrap_or(0), Vec::new()));
                section = Details;
            }
            (Rules | Details | Industry | Ineligibility, 6432) => section = OrderTypes,
            (Rules | Details | OrderTypes | Ineligibility, 6622) => section = Industry,
            (Rules | Details | OrderTypes | Industry, 6766) => section = Ineligibility,
            (Record, _) => {
                if let Some(record) = records.last_mut() {
                    record.push((tag, val));
                }
            }
            (Details, _) => {
                if let Some((_, block)) = details.last_mut() {
                    block.push((tag, val));
                }
            }
            (OrderTypes, 6430) => order_types.push((val, "")),
            (OrderTypes, TAG_IB_ORDER_TYPES) => {
                if let Some(entry) = order_types.last_mut() {
                    entry.1 = val;
                }
            }
            (Industry, 6623) => industries.push((val, "")),
            (Industry, 6624) => {
                if let Some(entry) = industries.last_mut() {
                    entry.1 = val;
                }
            }
            (Ineligibility, 6767) => reasons.push((val, "")),
            (Ineligibility, 6768) => {
                if let Some(entry) = reasons.last_mut() {
                    entry.1 = val;
                }
            }
            _ => {}
        }
    }
    if !is_secdef {
        return None;
    }
    if records.is_empty() {
        return Some(Vec::new());
    }

    let rules = parse_market_rules(data);
    let mut defs = Vec::with_capacity(records.len());
    for record in &records {
        let mut def = ContractDefinition::default();
        let mut keys = RecordKeys::default();
        apply_secdef_fields(&mut def, &mut keys, record);
        for (con_id, block) in &details {
            if *con_id == def.con_id {
                apply_secdef_fields(&mut def, &mut keys, block);
            }
        }
        // Last trade date: 541, else 200; contract month: 200, else the
        // month of 541 (`jclient.dy.aV()`, `dy.aU()`).
        def.last_trade_date = if keys.expiry.is_empty() { keys.month } else { keys.expiry }.to_string();
        def.contract_month = if keys.month.is_empty() {
            keys.expiry.get(..6).unwrap_or(keys.expiry).to_string()
        } else {
            keys.month.to_string()
        };
        // The primary exchange carries its suffix when that full name is a
        // valid exchange of the contract (`jclient.dy.y()`).
        if !def.primary_suffix.is_empty() {
            let full = format!("{}.{}", def.primary_exchange, def.primary_suffix);
            if def.valid_exchanges.contains(&full) {
                def.primary_exchange = full;
            }
        }
        // Order types: the table entry of the record's key, else the last
        // entry, else what the record itself carries.
        let types = order_types.iter()
            .find(|(key, _)| !keys.order_types.is_empty() && *key == keys.order_types)
            .or(order_types.last())
            .map(|(_, list)| *list)
            .filter(|list| !list.is_empty());
        if let Some(list) = types {
            def.order_types = list.split(',').map(|s| s.to_string()).collect();
        }
        // Industry, category and subcategory: the industry text of the
        // record's key split on '|' (`jsecdef.l`, `jclient.b5.m/n/o()`).
        if !keys.industry.is_empty()
            && let Some((_, text)) = industries.iter().find(|(key, _)| *key == keys.industry)
        {
            let mut parts = text.split('|');
            def.industry = parts.next().unwrap_or("").to_string();
            def.category = parts.next().unwrap_or("").to_string();
            def.subcategory = parts.next().unwrap_or("").to_string();
        }
        // Ineligibility reasons: the ids of the record, each with its text.
        def.ineligibility = keys.reasons.split(',')
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(|id| {
                let text = reasons.iter().find(|(key, _)| *key == id).map(|(_, t)| *t).unwrap_or("");
                (id.to_string(), text.to_string())
            })
            .collect();
        // The rule-block start marker is NOT the tick increment (ibx#197).
        // min_tick is the smallest increment of the record's rule (of every
        // rule when the reply has none for it), iso ContractDetails.minTick.
        // Absent a rule block, the default (0.01) stands.
        let own: Vec<&MarketRule> = rules.iter()
            .filter(|rule| def.market_rule_id.is_some_and(|id| rule.rule_id == id as i32))
            .collect();
        let used: Vec<&MarketRule> = if own.is_empty() { rules.iter().collect() } else { own };
        if let Some(min_increment) = used.iter()
            .flat_map(|rule| rule.price_increments.iter())
            .map(|inc| inc.increment)
            .filter(|inc| *inc > 0.0)
            .min_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
        {
            def.min_tick = min_increment;
        }
        // Size set and magnifier of the record's own rule.
        if let Some(rule) = rules.iter().find(|rule| def.market_rule_id.is_some_and(|id| rule.rule_id == id as i32)) {
            def.size_increments = rule.size_increments.clone();
            def.price_magnifier = rule.price_magnifier;
        }
        defs.push(def);
    }
    // Records of one underlying share its name, symbol and type: a
    // detail block that leaves them out gets those of another record of
    // the same underlying (the underlying's data, `jclient.dy.bt()`,
    // `dy.ba()`, `dy.aB()`).
    for i in 0..defs.len() {
        let under = defs[i].under_con_id;
        if under <= 0 {
            continue;
        }
        let pick = |f: fn(&ContractDefinition) -> &String, defs: &[ContractDefinition]| {
            defs.iter().filter(|d| d.under_con_id == under).map(f).find(|v| !v.is_empty()).cloned()
        };
        if defs[i].long_name.is_empty() && let Some(v) = pick(|d| &d.long_name, &defs) {
            defs[i].long_name = v;
        }
        if defs[i].under_symbol.is_empty() && let Some(v) = pick(|d| &d.under_symbol, &defs) {
            defs[i].under_symbol = v;
        }
        if defs[i].under_sec_type.is_empty() && let Some(v) = pick(|d| &d.under_sec_type, &defs) {
            defs[i].under_sec_type = v;
        }
    }
    Some(defs)
}

/// Table keys and dates a record refers to.
#[derive(Default)]
struct RecordKeys<'a> {
    order_types: &'a str,
    industry: &'a str,
    reasons: &'a str,
    expiry: &'a str,
    month: &'a str,
}

/// A flag sent as text: true when its first character is `T` or `Y`
/// (`jutils.dN.j(String)`).
fn text_flag(v: &str) -> bool {
    matches!(v.as_bytes().first(), Some(b'T' | b'Y'))
}

/// A flag sent as a number: true when not 0 (`jfix.cN.e()`).
fn int_flag(v: &str) -> bool {
    v.trim().parse::<i64>().is_ok_and(|n| n != 0)
}

/// Apply one record block or detail block to a definition.
fn apply_secdef_fields<'a>(def: &mut ContractDefinition, keys: &mut RecordKeys<'a>, block: &[(u32, &'a str)]) {
    let mut last_alt_id = "";
    for &(tag, v) in block {
        match tag {
            TAG_IB_CON_ID => def.con_id = v.parse().unwrap_or(0),
            TAG_SYMBOL => def.symbol = v.to_string(),
            TAG_SECURITY_TYPE => {
                def.sec_type = SecurityType::from_fix(v);
                def.wire_sec_type = v.to_string();
            }
            // The record's own exchange is its first one.
            TAG_SECURITY_EXCHANGE => {
                if def.exchange.is_empty() {
                    def.exchange = exchange_from_fix(v).to_string();
                }
            }
            TAG_IB_PRIMARY_EXCHANGE => def.primary_exchange = exchange_from_fix(v).to_string(),
            8224 => def.primary_suffix = v.to_string(),
            TAG_CURRENCY => def.currency = v.to_string(),
            TAG_IB_LOCAL_SYMBOL => def.local_symbol = v.to_string(),
            TAG_IB_TRADING_CLASS => def.trading_class = v.to_string(),
            TAG_LONG_NAME => def.long_name = v.to_string(),
            TAG_MULTIPLIER => {
                def.multiplier = v.parse().unwrap_or(1.0);
                def.multiplier_text = v.to_string();
            }
            TAG_IB_VALID_EXCHANGES => {
                def.valid_exchanges = v.split(',')
                    .filter(|s| !s.is_empty())
                    .map(|s| exchange_from_fix(s).to_string())
                    .collect();
            }
            TAG_IB_ORDER_TYPES => def.order_types = v.split(',').map(|s| s.to_string()).collect(),
            6430 => keys.order_types = v,
            6623 => keys.industry = v,
            6765 => keys.reasons = v,
            TAG_IB_MARKET_RULE_ID => def.market_rule_id = v.parse().ok(),
            541 => keys.expiry = v,
            TAG_LAST_TRADE_DATE => keys.month = v,
            6614 => def.real_expiration_date = v.to_string(),
            TAG_STRIKE => def.strike = v.parse().unwrap_or(0.0),
            // The wire right is 1 for a call and 0 for a put (`jfix.eg`);
            // the letters are accepted too.
            TAG_RIGHT => {
                def.right = match v {
                    "1" | "C" => Some(OptionRight::Call),
                    "0" | "P" => Some(OptionRight::Put),
                    _ => None,
                };
            }
            // The API stock type is the text of 8273; 8077 fills an enum
            // the API does not send (`jsecdef.i.a@5330`).
            TAG_IB_STOCK_TYPE_TEXT => def.stock_type = v.to_string(),
            6911 => def.country = v.to_string(), // Country
            58 => def.market_name = v.to_string(), // MarketName
            TAG_SCHEDULE_JOIN_KEY => def.join_key = v.to_string(),
            // ISIN and CUSIP from the alternative id group
            TAG_SECURITY_ID => last_alt_id = v,
            TAG_SECURITY_ID_SOURCE => match v {
                "4" => def.isin = last_alt_id.to_string(),
                "1" => def.cusip = last_alt_id.to_string(),
                _ => {}
            },
            6346 => def.under_con_id = v.parse().unwrap_or(0),
            6855 => def.under_symbol = v.to_string(),
            6178 => def.agg_group = v.trim().parse().ok(),
            6659 => def.exercise_style = v.parse().unwrap_or(0),
            6850 => def.last_trade_time = v.to_string(),
            310 => def.under_sec_type = if v == "CS" { "STK".to_string() } else { v.to_string() },
            6858 => def.ev_rule = v.to_string(),
            8193 => def.fractional = int_flag(v),
            8175 => def.fraction_size = v.trim().parse().ok(),
            // Fund
            6481 => def.fund.name = v.to_string(),
            6472 => def.fund.family = v.to_string(),
            6473 => def.fund.fund_type = v.to_string(),
            6474 => def.fund.front_load = v.to_string(),
            6475 => def.fund.back_load = v.to_string(),
            6482 => def.fund.back_load_time_interval = v.to_string(),
            6476 => def.fund.management_fee = v.to_string(),
            6477 => def.fund.closed = int_flag(v),
            6511 => def.fund.closed_for_new_investors = int_flag(v),
            6512 => def.fund.closed_for_new_money = int_flag(v),
            6478 => def.fund.notify_amount = v.to_string(),
            6479 => def.fund.minimum_initial_purchase = v.to_string(),
            8505 => def.fund.subsequent_minimum_purchase = v.to_string(),
            8150 => def.fund.blue_sky_states = v.to_string(),
            8151 => def.fund.blue_sky_territories = v.to_string(),
            8502 => def.fund.distribution_policy_indicator = v.to_string(),
            8503 => def.fund.asset_type = v.to_string(),
            // Bond
            6493 => def.bond.notes = v.to_string(),
            6494 => def.bond.desc_append = v.to_string(),
            6853 => def.bond.short_description = v.to_string(),
            6495 => def.bond.bond_type = v.to_string(),
            6496 => def.bond.coupon_type = v.to_string(),
            6497 => def.bond.callable = text_flag(v),
            6498 => def.bond.putable = text_flag(v),
            6499 => def.bond.convertible = text_flag(v),
            6500 => def.bond.next_option_partial = text_flag(v),
            6501 => def.bond.next_option_date = v.to_string(),
            6502 => def.bond.next_option_type = v.to_string(),
            255 => def.bond.ratings = v.to_string(),
            223 => def.bond.coupon = v.to_string(),
            225 => def.bond.issue_date = v.to_string(),
            _ => {}
        }
    }
}

/// Request id names of a lookup by symbol and of a lookup by identifier,
/// as the reference names them; the request number follows (ibx#229).
pub const SECDEF_BY_SYMBOL_NAME: &str = "FixSecDefReqBySymbol";
pub const SECDEF_BY_IDENTIFIER_NAME: &str = "FixSecDefReqByIdTypeValue";
/// Name of the API lookup by conId on an exchange (ibx#438).
pub const SECDEF_BY_CONID_NAME: &str = "socket-reqContractDetailsReqByConid";
/// Name of the lookup of the preferred contract of a conId, the API lookup
/// by conId without an exchange (ibx#438).
pub const SECDEF_PREFERRED_NAME: &str = "PreferredReqByConid";
/// Name of the company lookup of an underlying (ibx#436).
pub const SECDEF_UNDERLYING_NAME: &str = "UnderlyingECNoDupsNDReqByConid";

/// Industry, category and subcategory of a company, set on a definition
/// that has none (ibx#436).
pub fn apply_company(def: &mut ContractDefinition, company: &(String, String, String)) {
    if def.industry.is_empty() {
        def.industry = company.0.clone();
        def.category = company.1.clone();
        def.subcategory = company.2.clone();
    }
}

/// The request number of a definition reply's request id: the id without
/// the name of its lookup (a bare number is taken as is).
pub fn secdef_request_number(req_id: &str) -> Option<crate::types::ReqId> {
    req_id.strip_prefix(SECDEF_BY_IDENTIFIER_NAME)
        .or_else(|| req_id.strip_prefix(SECDEF_BY_SYMBOL_NAME))
        .or_else(|| req_id.strip_prefix(SECDEF_BY_CONID_NAME))
        .or_else(|| req_id.strip_prefix(SECDEF_PREFERRED_NAME))
        .unwrap_or(req_id)
        .parse()
        .ok()
}

/// Extract the SecurityReqID from a response to match with the original request.
pub fn secdef_response_req_id(data: &[u8]) -> Option<String> {
    let tags = fix::fix_parse(data);
    tags.get(&TAG_SECURITY_REQ_ID).cloned()
}

/// Check if a response is the last one (response type 5 or 6).
pub fn secdef_response_is_last(data: &[u8]) -> bool {
    let tags = fix::fix_parse(data);
    matches!(
        tags.get(&TAG_SECURITY_RESPONSE_TYPE).map(|s| s.as_str()),
        Some("5") | Some("6")
    )
}

// ─── Market rules ───

/// A price increment rule defining tick sizes at different price levels.
#[derive(Debug, Clone)]
pub struct PriceIncrement {
    pub low_edge: f64,
    pub increment: f64,
}

/// A market rule containing a rule ID and its price increment table.
#[derive(Debug, Clone, Default)]
pub struct MarketRule {
    pub rule_id: i32,
    pub price_increments: Vec<PriceIncrement>,
    /// Size set of the rule (ibx#436): minimum size and size increments.
    pub size_increments: Vec<PriceIncrement>,
    /// Price magnifier: 10 to the power of the first tier set's exponent
    /// (`jmarketrules.o.<init>@...`: `round(10^l.b())`, l from tag 6021).
    pub price_magnifier: i32,
}

/// Parse the market rule table of a definition reply (ibx#437), as the
/// reference reads it: the table follows its rule count and ends at the
/// first field that is not a rule field; each rule id starts a rule; the
/// counts are counts, and only the entries after the price increment count
/// are the rule's price increments, the entries after the size count its
/// size increments (the tier sets are skipped). An increment's low edge is
/// the last low edge read.
pub fn parse_market_rules(data: &[u8]) -> Vec<MarketRule> {
    use crate::protocol::fix::SOH;

    /// Entries the last count opened.
    #[derive(Clone, Copy, PartialEq)]
    enum Set { None, Price, Size }
    let mut rules: Vec<MarketRule> = Vec::new();
    let mut in_table = false;
    let mut current: Option<MarketRule> = None;
    let mut set = Set::None;
    let mut entries_left = 0usize;
    let mut low_edge: Option<f64> = None;
    let mut exponent_read = false;
    for part in data.split(|&b| b == SOH) {
        let Ok(text) = std::str::from_utf8(part) else { continue };
        let Some((tag, val)) = text.split_once('=') else { continue };
        let Ok(tag) = tag.parse::<u32>() else { continue };
        if tag == TAG_MARKET_RULE_COUNT {
            rules.extend(current.take());
            in_table = true;
            set = Set::None;
            low_edge = None;
            continue;
        }
        if !in_table {
            continue;
        }
        match tag {
            TAG_MARKET_RULE_ID => {
                rules.extend(current.take());
                current = Some(MarketRule { rule_id: val.parse().unwrap_or(0), price_magnifier: 1, ..Default::default() });
                set = Set::None;
                exponent_read = false;
            }
            TAG_PRICE_INCREMENT_COUNT => {
                set = Set::Price;
                entries_left = val.parse().unwrap_or(0);
            }
            TAG_SIZE_INCREMENT_COUNT => {
                set = Set::Size;
                entries_left = val.parse().unwrap_or(0);
            }
            TAG_LOW_EDGE => low_edge = val.parse().ok(),
            TAG_INCREMENT => {
                if set != Set::None && entries_left > 0 {
                    entries_left -= 1;
                    if let (Some(rule), Some(low_edge), Ok(increment)) = (current.as_mut(), low_edge, val.parse::<f64>()) {
                        let entry = PriceIncrement { low_edge, increment };
                        if set == Set::Price {
                            rule.price_increments.push(entry);
                        } else {
                            rule.size_increments.push(entry);
                        }
                    }
                }
            }
            // The exponent of the first tier set.
            6021 => {
                if let (Some(rule), false) = (current.as_mut(), exponent_read) {
                    exponent_read = true;
                    let exp: i32 = val.parse().unwrap_or(0);
                    rule.price_magnifier = 10f64.powi(exp).round() as i32;
                }
            }
            // Any other count starts entries that are not increments.
            t if MARKET_RULE_FIELDS.contains(&t) => {
                if matches!(t, 6022 | 6029) {
                    set = Set::None;
                }
            }
            _ => {
                rules.extend(current.take());
                in_table = false;
            }
        }
    }
    rules.extend(current.take());
    rules
}

/// The size increments of a definition's market rules (ibx#287), in the
/// order they come.
pub fn parse_size_increments(data: &[u8]) -> Vec<PriceIncrement> {
    use crate::protocol::fix::SOH;

    let mut out = Vec::new();
    let mut remaining = 0usize;
    let mut low_edge: Option<f64> = None;
    for part in data.split(|&b| b == SOH) {
        let Some(eq) = part.iter().position(|&b| b == b'=') else { continue };
        let Ok(tag) = std::str::from_utf8(&part[..eq]).unwrap_or("").parse::<u32>() else { continue };
        let val = std::str::from_utf8(&part[eq + 1..]).unwrap_or("");
        match tag {
            TAG_SIZE_INCREMENT_COUNT => {
                remaining = val.parse().unwrap_or(0);
                low_edge = None;
            }
            // A new rule ends the entries of the previous one.
            TAG_MARKET_RULE_ID => remaining = 0,
            TAG_LOW_EDGE if remaining > 0 => low_edge = val.parse().ok(),
            TAG_INCREMENT if remaining > 0 => {
                if let (Some(low_edge), Ok(increment)) = (low_edge.take(), val.parse::<f64>()) {
                    out.push(PriceIncrement { low_edge, increment });
                }
                remaining -= 1;
            }
            _ => {}
        }
    }
    out
}

/// Round lot of a contract's market data sizes (ibx#287), from its
/// definition, as the reference: a US stock or US warrant counts in the
/// integer part of its smallest size increment, 100 when that is missing
/// or below 1; any other contract in 1.
pub fn round_lot_from_secdef(data: &[u8]) -> i64 {
    let tags = fix::fix_parse(data);
    let sec_type = tags.get(&TAG_SECURITY_TYPE).map(String::as_str).unwrap_or("");
    let market_type = tags.get(&TAG_IB_MARKET_TYPE).map(String::as_str).unwrap_or("");
    let us = matches!((sec_type, market_type), ("CS" | "STK", "USSTK") | ("WAR", "USWAR"));
    if !us {
        return 1;
    }
    parse_size_increments(data)
        .iter()
        .map(|inc| inc.increment)
        .filter(|inc| *inc > 0.0)
        .min_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
        .map(|inc| inc.trunc() as i64)
        .filter(|lot| *lot > 0)
        .unwrap_or(100)
}

/// Aggregate group of a contract (#445), from its definition: the routing
/// row of a SMART request is the one of this group. None when absent.
pub fn agg_group_from_secdef(data: &[u8]) -> Option<i32> {
    fix::fix_parse(data).get(&6178).and_then(|v| v.trim().parse().ok())
}

/// SMART component exchanges of a contract (#452), from its definition
/// (comma list, a trailing comma allowed). Empty when absent.
pub fn smart_components_from_secdef(data: &[u8]) -> Vec<String> {
    fix::fix_parse(data).get(&6177)
        .map(|v| v.split(',').map(str::trim).filter(|e| !e.is_empty()).map(String::from).collect())
        .unwrap_or_default()
}

/// Cache of contract definitions by conId.
#[derive(Debug, Default)]
pub struct ContractStore {
    by_con_id: HashMap<i64, ContractDefinition>,
    by_symbol: HashMap<String, i64>,
}

impl ContractStore {
    pub fn insert(&mut self, def: ContractDefinition) {
        let key = format!("{}:{}:{}", def.symbol, def.sec_type.to_fix(), def.currency);
        self.by_symbol.insert(key, def.con_id);
        self.by_con_id.insert(def.con_id, def);
    }

    pub fn get(&self, con_id: i64) -> Option<&ContractDefinition> {
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

// ─── Schedule subscription ───

/// Tags for schedule subscription responses.
pub const TAG_SUB_PROTOCOL: u32 = 6040;
pub const TAG_SCHEDULE_TIMEZONE: u32 = 6734;
pub const TAG_SESSION_COUNT: u32 = 6840;
pub const TAG_SESSION_START: u32 = 6841;
pub const TAG_SESSION_END: u32 = 6842;
pub const TAG_TRADE_DATE: u32 = 75;
pub const TAG_IS_TRADING_HOURS: u32 = 6843;
pub const TAG_IS_LIQUID_HOURS: u32 = 6844;
/// Exchange-path key shared by paired secdef and schedule replies.
pub const TAG_SCHEDULE_JOIN_KEY: u32 = 6256;
/// Subscribe protocol value for schedule subscription.
pub const SUB_PROTOCOL_SCHEDULE_SUBSCRIBE: &str = "106";
/// Subscribe protocol value for schedule reply.
pub const SUB_PROTOCOL_SCHEDULE_REPLY: &str = "107";

/// A single trading/liquid hours session.
#[derive(Debug, Clone, PartialEq)]
pub struct ScheduleSession {
    pub start: String,
    pub end: String,
    pub trade_date: String,
}

/// Parsed schedule response.
#[derive(Debug, Clone)]
pub struct ContractSchedule {
    pub timezone: String,
    pub trading_hours: Vec<ScheduleSession>,
    pub liquid_hours: Vec<ScheduleSession>,
}

/// Parse a schedule response into trading/liquid hours.
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

    // Verify this is a schedule response
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

    // 24h venues (e.g. FOREX) emit sessions with both 6843=1 AND 6844=1 — append
    // to both lists independently.
    for (tag, val) in &tags {
        match *tag {
            TAG_SESSION_START => {
                if in_session {
                    flush_session(&mut trading_hours, &mut liquid_hours,
                        start.clone(), end.clone(), trade_date.clone(), is_trading, is_liquid);
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
    if in_session {
        flush_session(&mut trading_hours, &mut liquid_hours,
            start, end, trade_date, is_trading, is_liquid);
    }

    Some(ContractSchedule { timezone, trading_hours, liquid_hours })
}

/// Append a parsed session to the hour lists. A session with NEITHER flag
/// set is a closed day; it used to be dropped, leaving "market closed"
/// indistinguishable from "data missing" (ibx#223). It is kept as a
/// zero-length session in both lists, which renders as `<date>:CLOSED`.
fn flush_session(
    trading_hours: &mut Vec<ScheduleSession>,
    liquid_hours: &mut Vec<ScheduleSession>,
    start: String,
    end: String,
    trade_date: String,
    is_trading: bool,
    is_liquid: bool,
) {
    let closed = !is_trading && !is_liquid;
    let session = ScheduleSession {
        end: if closed { start.clone() } else { end },
        start,
        trade_date,
    };
    if closed {
        trading_hours.push(session.clone());
        liquid_hours.push(session);
    } else {
        if is_trading { trading_hours.push(session.clone()); }
        if is_liquid { liquid_hours.push(session); }
    }
}

/// A session of a schedule day: start and end instants.
type Session = (jiff::Timestamp, jiff::Timestamp);

/// The days of a schedule from the current day on, as the reference builds
/// them (`feature.lh.ag`, `feature.lh.D.<init>`): one day per trade date,
/// the days between two trade dates and the current day and the next one
/// added closed; the days before the current day left out. A day's liquid
/// sessions are its trading sessions when it has no liquid session, and
/// none when it has no trading session.
struct ScheduleDays {
    tz: jiff::tz::TimeZone,
    trading: Vec<(jiff::civil::Date, Vec<Session>)>,
    liquid: Vec<(jiff::civil::Date, Vec<Session>)>,
}

fn utc_instant(s: &str) -> Option<jiff::Timestamp> {
    jiff::civil::DateTime::strptime("%Y%m%d-%H:%M:%S", s).ok()?
        .to_zoned(jiff::tz::TimeZone::UTC).ok()
        .map(|z| z.timestamp())
}

fn civil_date(s: &str) -> Option<jiff::civil::Date> {
    jiff::civil::Date::strptime("%Y%m%d", s.get(..8)?).ok()
}

/// The time zone of a schedule; UTC when the zone is not known.
pub fn schedule_zone(name: &str) -> jiff::tz::TimeZone {
    jiff::tz::TimeZone::get(name).unwrap_or(jiff::tz::TimeZone::UTC)
}

fn schedule_days(sched: &ContractSchedule, today: jiff::civil::Date) -> ScheduleDays {
    use std::collections::BTreeMap;
    let tz = schedule_zone(&sched.timezone);
    let mut days: BTreeMap<jiff::civil::Date, (Vec<Session>, Vec<Session>)> = BTreeMap::new();
    let mut add = |list: &[ScheduleSession], liquid: bool| {
        for s in list {
            let (Some(start), Some(end)) = (utc_instant(&s.start), utc_instant(&s.end)) else { continue };
            let date = civil_date(&s.trade_date).unwrap_or_else(|| start.to_zoned(tz.clone()).date());
            let day = days.entry(date).or_default();
            // A closed day is kept as a zero length session: no session.
            if start == end {
                continue;
            }
            let sessions = if liquid { &mut day.1 } else { &mut day.0 };
            if !sessions.contains(&(start, end)) {
                sessions.push((start, end));
            }
        }
    };
    add(&sched.trading_hours, false);
    add(&sched.liquid_hours, true);
    if let (Some(first), Some(last)) = (days.keys().next().copied(), days.keys().next_back().copied()) {
        let mut d = first;
        while d < last {
            days.entry(d).or_default();
            match d.tomorrow() { Ok(next) => d = next, Err(_) => break }
        }
    }
    days.entry(today).or_default();
    if let Ok(next) = today.tomorrow() {
        days.entry(next).or_default();
    }
    let mut trading = Vec::new();
    let mut liquid = Vec::new();
    for (date, (mut th, mut lh)) in days.into_iter().filter(|(d, _)| *d >= today) {
        th.sort();
        lh.sort();
        let lh = if th.is_empty() { Vec::new() } else if lh.is_empty() { th.clone() } else { lh };
        trading.push((date, th));
        liquid.push((date, lh));
    }
    ScheduleDays { tz, trading, liquid }
}

/// One day of an hours text (`feature.lh.f.a(TimeZone)`): its sessions
/// `yyyyMMdd:HHmm-yyyyMMdd:HHmm` joined with `;`, or `yyyyMMdd:CLOSED`.
fn day_text(date: jiff::civil::Date, sessions: &[Session], tz: &jiff::tz::TimeZone) -> String {
    if sessions.is_empty() {
        return format!("{}:CLOSED", date.strftime("%Y%m%d"));
    }
    sessions.iter()
        .map(|(start, end)| format!(
            "{}-{}",
            start.to_zoned(tz.clone()).strftime("%Y%m%d:%H%M"),
            end.to_zoned(tz.clone()).strftime("%Y%m%d:%H%M"),
        ))
        .collect::<Vec<_>>()
        .join(";")
}

/// A day with a session that starts or ends before its own date
/// (`feature.lh.f.a(false)`).
fn starts_before(date: jiff::civil::Date, sessions: &[Session], tz: &jiff::tz::TimeZone) -> bool {
    sessions.iter().any(|(start, end)| {
        start.to_zoned(tz.clone()).date() < date || end.to_zoned(tz.clone()).date() < date
    })
}

/// An hours text of the API row (`feature.lh.D.a(f[], boolean)`): the first
/// day always; a later closed day is left out when the next day is open
/// and starts on an earlier date (its session covers the closed day).
fn hours_text(days: &[(jiff::civil::Date, Vec<Session>)], tz: &jiff::tz::TimeZone) -> String {
    let mut out = String::new();
    for (i, (date, sessions)) in days.iter().enumerate() {
        if i > 0
            && sessions.is_empty()
            && let Some((next_date, next)) = days.get(i + 1)
            && !next.is_empty()
            && starts_before(*next_date, next, tz)
        {
            continue;
        }
        if !out.is_empty() {
            out.push(';');
        }
        out.push_str(&day_text(*date, sessions, tz));
    }
    out
}

/// The last trading instant of a contract with a schedule
/// (`jclient.dy.c(lh.D)`), and whether it has a time: with a last trading
/// time (6850) the date at that time in the zone; else, for a contract
/// that is not a bond, the end of the last session of an open liquid day
/// on that date; else the start of the date. None without a date.
fn last_trading(def: &ContractDefinition, days: &ScheduleDays) -> Option<(jiff::Zoned, bool)> {
    let expiry = def.last_trade_date.as_str();
    if expiry.is_empty() || expiry.eq_ignore_ascii_case("NOEXP") {
        return None;
    }
    let date = civil_date(expiry)?;
    let time = def.last_trade_time.as_str();
    if time.len() == 4 && time.bytes().all(|b| b.is_ascii_digit()) {
        let (h, m) = (time[..2].parse().ok()?, time[2..].parse().ok()?);
        return date.at(h, m, 0, 0).to_zoned(days.tz.clone()).ok().map(|z| (z, true));
    }
    if !def.is_bond()
        && let Some((_, sessions)) = days.liquid.iter().find(|(d, s)| *d == date && !s.is_empty())
        && let Some((_, end)) = sessions.last()
    {
        return Some((end.to_zoned(days.tz.clone()), true));
    }
    date.to_zoned(days.tz.clone()).ok().map(|z| (z, false))
}

/// The days of a schedule up to the last trading instant
/// (`feature.lh.D.b(d1)`): a closed day is kept, an open day is kept when
/// it starts before that instant, is its date, or holds it; the days after
/// the first other one are left out. The session that holds the instant
/// ends at it, the later sessions of that day are left out.
fn trim_days(days: &[(jiff::civil::Date, Vec<Session>)], end: jiff::Timestamp, tz: &jiff::tz::TimeZone) -> Vec<(jiff::civil::Date, Vec<Session>)> {
    let end_date = end.to_zoned(tz.clone()).date();
    let mut kept = Vec::new();
    for (date, sessions) in days {
        if !sessions.is_empty() {
            let day_start = date.to_zoned(tz.clone()).map(|z| z.timestamp()).unwrap_or(end);
            let first = sessions.first().map(|s| s.0).unwrap_or(end);
            let last = sessions.last().map(|s| s.1).unwrap_or(end);
            if !(day_start < end || *date == end_date || (first <= end && end <= last)) {
                break;
            }
        }
        kept.push((*date, sessions.clone()));
    }
    let mut cut = false;
    for (_, sessions) in kept.iter_mut() {
        let mut i = 0;
        while i < sessions.len() {
            if cut {
                sessions.remove(i);
                continue;
            }
            if sessions[i].0 <= end && end <= sessions[i].1 {
                sessions[i].1 = end;
                cut = true;
            }
            i += 1;
        }
    }
    kept
}

/// Merge a schedule into a definition, as the reference fills the API row
/// (ibx#436, `jextend.dz.a(dy, lh.D)`): a contract whose last trading
/// instant is past gets no schedule; else the zone, the trading and liquid
/// hours up to that instant, and the last trade date and time.
pub fn apply_schedule(def: &mut ContractDefinition, sched: &ContractSchedule, now: jiff::Timestamp) {
    let tz = schedule_zone(&sched.timezone);
    let mut days = schedule_days(sched, now.to_zoned(tz).date());
    let last = last_trading(def, &days);
    def.schedule_expiry = None;
    if let Some((at, _)) = &last {
        if at.timestamp() < now {
            def.time_zone_id = None;
            def.trading_hours = None;
            def.liquid_hours = None;
            return;
        }
        days.trading = trim_days(&days.trading, at.timestamp(), &days.tz);
        days.liquid = trim_days(&days.liquid, at.timestamp(), &days.tz);
    }
    def.time_zone_id = (!sched.timezone.is_empty()).then(|| sched.timezone.clone());
    def.trading_hours = Some(hours_text(&days.trading, &days.tz));
    def.liquid_hours = Some(hours_text(&days.liquid, &days.tz));
    def.schedule_expiry = last.map(|(at, timed)| {
        (at.strftime("%Y%m%d").to_string(), if timed { at.strftime("%H:%M:%S").to_string() } else { String::new() })
    });
}

/// Market rule id of each valid exchange of a definition whose rule is
/// known, comma-joined in valid-exchange order (ibx#435, `jclient.dy.fu()`).
pub fn market_rule_ids(def: &ContractDefinition, known: &HashMap<(i64, String), u32>) -> String {
    let mut ids = String::new();
    for exch in &def.valid_exchanges {
        if let Some(id) = known.get(&(def.con_id, exch.clone())) {
            if !ids.is_empty() {
                ids.push(',');
            }
            ids.push_str(&id.to_string());
        }
    }
    ids
}

/// The row of one record of a definition reply, made as the hot loop makes
/// it (ibx#436): the record of `con_id` on `exchange`, the company of its
/// underlying from the company lookup replies received before, its market
/// rule ids from the known rules, the schedule merged at `now`, and the
/// logon features. Used by the golden tests of the Rust and Python rows.
#[doc(hidden)]
#[allow(clippy::too_many_arguments)]
pub fn row_of_reply(
    secdef: &[u8],
    con_id: i64,
    exchange: &str,
    continuous: bool,
    known_rules: &HashMap<(i64, String), u32>,
    company_replies: &[&[u8]],
    schedule: Option<&[u8]>,
    now: jiff::Timestamp,
    bond_api: bool,
    ev_api: bool,
) -> Option<ContractDefinition> {
    let mut def = parse_secdef_records(secdef)?.into_iter()
        .find(|d| d.con_id == con_id && d.exchange == exchange)?;
    def.continuous = continuous;
    if def.under_con_id > 0 {
        let company = company_replies.iter()
            .filter_map(|reply| parse_secdef_records(reply))
            .flat_map(|records| records.into_iter())
            .rfind(|d| d.con_id == def.under_con_id)
            .map(|d| (d.industry, d.category, d.subcategory));
        if let Some(company) = company {
            apply_company(&mut def, &company);
        }
    }
    def.market_rule_ids = market_rule_ids(&def, known_rules);
    def.bond_api = bond_api;
    def.ev_api = ev_api;
    if let Some(sched) = schedule.and_then(parse_schedule_response) {
        apply_schedule(&mut def, &sched, now);
    }
    Some(def)
}

/// Format a list of sessions into a semicolon-delimited string.
///
/// Output: `"YYYYMMDD:HHMM-YYYYMMDD:HHMM;YYYYMMDD:CLOSED;..."`.
/// Times are in UTC as received from the upstream wire — consumers should
/// convert to local time using the paired timezone identifier when displaying.
/// A zero-length session is a closed day and renders as `<date>:CLOSED`,
/// the official-API convention (ibx#223).
/// Returns an empty string if `sessions` is empty.
pub fn format_sessions_string(sessions: &[ScheduleSession]) -> String {
    let mut out = String::with_capacity(sessions.len() * 32);
    for (i, s) in sessions.iter().enumerate() {
        if i > 0 { out.push(';'); }
        if s.start == s.end {
            // A short or non-ASCII field is kept whole rather than cut
            // inside a character (ibx#258).
            let date = s.trade_date.get(..8)
                .or_else(|| s.start.get(..8))
                .unwrap_or(s.start.as_str());
            out.push_str(date);
            out.push_str(":CLOSED");
        } else {
            out.push_str(&trim_session_endpoint(&s.start));
            out.push('-');
            out.push_str(&trim_session_endpoint(&s.end));
        }
    }
    out
}

/// Convert wire `YYYYMMDD-HH:MM:SS` to compact `YYYYMMDD:HHMM`.
/// Returns the input unchanged if the format does not match, including a
/// non-ASCII value, whose byte positions are not character positions
/// (ibx#258).
fn trim_session_endpoint(s: &str) -> String {
    let bytes = s.as_bytes();
    if s.is_ascii() && bytes.len() >= 14 && bytes[8] == b'-' && bytes[11] == b':' {
        let mut out = String::with_capacity(13);
        out.push_str(&s[..8]);
        out.push(':');
        out.push_str(&s[9..11]);
        out.push_str(&s[12..14]);
        out
    } else {
        s.to_string()
    }
}

// ─── Matching symbols search ───

/// Tags for matching symbols.
pub const TAG_MATCH_PATTERN: u32 = 58;
pub const TAG_MATCH_COUNT: u32 = 146;
pub const TAG_MATCH_PRIMARY_EXCHANGE: u32 = 6453;
pub const TAG_MATCH_DESCRIPTION: u32 = 306;
pub const TAG_MATCH_DERIVATIVE_TYPES: u32 = 6070;

pub const TAG_MATCH_ISSUER_ID: u32 = 6454;
/// Security type of a row that has no security type field (bond rows).
pub const TAG_MATCH_ALT_SECURITY_TYPE: u32 = 310;

/// A single matching symbol result (ibx#439).
#[derive(Debug, Clone)]
pub struct SymbolMatch {
    /// -1 when the row has no conId, as the reference.
    pub con_id: i64,
    pub symbol: String,
    /// The API text ("STK", "BOND", ...); a type the engine does not know
    /// is kept as received.
    pub sec_type: String,
    pub currency: String,
    pub primary_exchange: String,
    pub description: String,
    pub issuer_id: String,
    pub derivative_types: Vec<String>,
}

/// API text of a security type as received: the known types in their API
/// form, any other text as is.
fn api_sec_type(raw: &str) -> String {
    match SecurityType::from_fix(raw) {
        SecurityType::Other => raw.to_string(),
        known => known.to_api_str().to_string(),
    }
}

/// Build a matching symbols request.
pub fn build_matching_symbols_request(pattern: &str, req_id: &str, seq: u32) -> Vec<u8> {
    fix::fix_build(
        &[
            (fix::TAG_MSG_TYPE, "U"),
            (TAG_SUB_PROTOCOL, "185"),
            (TAG_SECURITY_REQ_ID, req_id),
            (TAG_MATCH_PATTERN, pattern),
        ],
        seq,
    )
}

/// Parse a matching symbols response.
///
/// Uses sequential tag parsing since matches are a repeating group.
pub fn parse_matching_symbols_response(data: &[u8]) -> Option<Vec<SymbolMatch>> {
    use crate::protocol::fix::SOH;

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

    // Verify this is a matching symbols response
    let msg_type = tags.iter().find(|(t, _)| *t == fix::TAG_MSG_TYPE)?.1.as_str();
    if msg_type != "U" { return None; }
    let sub_protocol = tags.iter().find(|(t, _)| *t == TAG_SUB_PROTOCOL)?.1.as_str();
    if sub_protocol != "186" { return None; }

    // Each row starts at its symbol, as the reference: rows without a
    // conId are kept with -1, the security type is the first type field
    // else the alternative one (ibx#439). A row whose conId is not a number
    // is skipped, as the reference skips a row it cannot convert.
    struct Row<'a> {
        m: SymbolMatch,
        sec_type: Option<&'a str>,
        alt_sec_type: Option<&'a str>,
        con_id: Option<&'a str>,
    }
    fn close(row: Row<'_>, out: &mut Vec<SymbolMatch>) {
        let mut m = row.m;
        m.sec_type = api_sec_type(row.sec_type.or(row.alt_sec_type).unwrap_or(""));
        m.con_id = match row.con_id {
            None => -1,
            Some(v) => match v.parse() {
                Ok(id) => id,
                Err(_) => {
                    log::warn!("matching symbols: row {} skipped, conId {:?}", m.symbol, v);
                    return;
                }
            },
        };
        out.push(m);
    }
    let mut matches = Vec::new();
    let mut current: Option<Row> = None;

    for (tag, val) in &tags {
        if *tag == TAG_SYMBOL {
            if let Some(row) = current.take() {
                close(row, &mut matches);
            }
            current = Some(Row {
                m: SymbolMatch {
                    con_id: -1,
                    symbol: val.clone(),
                    sec_type: String::new(),
                    currency: String::new(),
                    primary_exchange: String::new(),
                    description: String::new(),
                    issuer_id: String::new(),
                    derivative_types: Vec::new(),
                },
                sec_type: None,
                alt_sec_type: None,
                con_id: None,
            });
            continue;
        }
        let Some(row) = current.as_mut() else { continue };
        match *tag {
            TAG_SECURITY_TYPE => row.sec_type = Some(val),
            TAG_MATCH_ALT_SECURITY_TYPE => row.alt_sec_type = Some(val),
            TAG_CURRENCY => row.m.currency = val.clone(),
            TAG_IB_CON_ID => row.con_id = Some(val),
            TAG_MATCH_PRIMARY_EXCHANGE => row.m.primary_exchange = val.clone(),
            TAG_MATCH_DESCRIPTION => row.m.description = val.clone(),
            TAG_MATCH_ISSUER_ID => row.m.issuer_id = val.clone(),
            TAG_MATCH_DERIVATIVE_TYPES => {
                row.m.derivative_types = val.split(',').map(|s| s.to_string()).collect();
            }
            _ => {}
        }
    }
    if let Some(row) = current {
        close(row, &mut matches);
    }

    Some(matches)
}

#[cfg(test)]
pub(crate) mod tests {
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
        // Build a fake security definition response
        let msg = fix::fix_build(
            &[
                (TAG_MSG_TYPE, "d"),
                (TAG_SECURITY_REQ_ID, "R1"),
                (TAG_SECURITY_RESPONSE_TYPE, "4"),
                (TAG_SYMBOL, "AAPL"),
                (TAG_IB_CON_ID, "265598"),
                (TAG_SECURITY_TYPE, "CS"),
                (TAG_SECURITY_EXCHANGE, "NASDAQ"),
                (TAG_CURRENCY, "USD"),
                (TAG_LONG_NAME, "APPLE INC"),
                (TAG_IB_VALID_EXCHANGES, "BEST,NYSE,ARCA"),
                (TAG_IB_PRIMARY_EXCHANGE, "NASDAQ"),
                // Rule table: min_tick is the smallest price increment.
                (TAG_MARKET_RULE_COUNT, "1"),
                (TAG_MARKET_RULE_ID, "26"),
                (TAG_PRICE_INCREMENT_COUNT, "1"),
                (TAG_LOW_EDGE, "0"),
                (TAG_INCREMENT, "0.01"),
                (TAG_SIZE_INCREMENT_COUNT, "1"),
                (TAG_LOW_EDGE, "0"),
                (TAG_INCREMENT, "1"),
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

    // ibx#400: '/' is removed from the symbol, '.' and spaces are kept.
    #[test]
    fn lookup_symbol_removes_every_slash_only() {
        assert_eq!(lookup_symbol("BRK/A"), "BRKA");
        assert_eq!(lookup_symbol("A/B/C"), "ABC");
        assert_eq!(lookup_symbol("BRK.A"), "BRK.A");
        assert_eq!(lookup_symbol("BRK A"), "BRK A");
        assert!(matches!(lookup_symbol("AAPL"), std::borrow::Cow::Borrowed("AAPL")));
    }

    // ibx#400: a reply with no contract record is "not found", not a
    // definition with conId 0.
    #[test]
    fn parse_empty_reply_is_none() {
        let msg = fix::fix_build(
            &[
                (TAG_MSG_TYPE, "d"),
                (TAG_SECURITY_REQ_ID, "1005"),
                (322, "*"),
                (TAG_SECURITY_RESPONSE_TYPE, "4"),
                (6038, "Y"),
                (TAG_MARKET_RULE_COUNT, "0"),
                (6344, "0"),
            ],
            1,
        );
        assert!(super::parse_secdef_response(&msg).is_none());
    }

    /// A message from its fields written as `tag=value|tag=value`.
    pub(crate) fn pipe_msg(text: &str) -> Vec<u8> {
        let fields: Vec<(u32, &str)> = text.split('|')
            .filter(|f| !f.is_empty())
            .map(|f| {
                let (t, v) = f.split_once('=').unwrap();
                (t.parse().unwrap(), v)
            })
            .collect();
        fix::fix_build(&fields, 1)
    }

    /// A futures lookup with no month: five contracts in one reply, each
    /// record followed later by its detail block (layout of a captured
    /// reply, values trimmed).
    pub(crate) fn five_future_records(req_id: &str, join_key: &str) -> Vec<u8> {
        let months = [
            ("815824267", "MNQZ6", "20261218", "202612"),
            ("840227399", "MNQH7", "20270319", "202703"),
            ("866514785", "MNQM7", "20270618", "202706"),
            ("893091676", "MNQU7", "20270917", "202709"),
            ("925800444", "MNQZ7", "20271217", "202712"),
        ];
        let key = if join_key.is_empty() { String::new() } else { format!("6256={}|", join_key) };
        let mut text = format!("35=d|43=N|320={}|322=*|323=4|", req_id);
        for (id, local, date, month) in months {
            text += &format!(
                "55=MNQ|167=FUT|207=CME|6008={id}|8499=1|6411=CME/FUT/GLOBEX|{key}6031=67|15=USD|58=MNQ|\
                 6035={local}|6058=MNQ|541={date}|200={month}|6614={date}|231=2|6430=CME/FUT|"
            );
        }
        text += "146=5|6038=Y|6019=1|6031=67|6026=1|6023=0|6027=0.25|6030=1|6344=5|";
        for (id, _, _, _) in months {
            text += &format!("6008={id}|8499=1|6346=362687422|310=IND|306=Micro E-Mini Nasdaq-100 Index|6046=CME,|6523=USFUT|");
        }
        text += "6432=1|6430=CME/FUT|6431=LMT/3,MKT/1,STP/1|6599=IBALGO";
        pipe_msg(&text)
    }

    // ibx#435: one definition per record, not one merged definition.
    #[test]
    fn parse_records_splits_a_multi_contract_reply() {
        let defs = parse_secdef_records(&five_future_records("11", "")).unwrap();
        assert_eq!(defs.len(), 5);
        let ids: Vec<i64> = defs.iter().map(|d| d.con_id).collect();
        assert_eq!(ids, [815824267, 840227399, 866514785, 893091676, 925800444]);
        let locals: Vec<&str> = defs.iter().map(|d| d.local_symbol.as_str()).collect();
        assert_eq!(locals, ["MNQZ6", "MNQH7", "MNQM7", "MNQU7", "MNQZ7"]);
        // The last trade date is 541, the contract month 200 (ibx#436).
        let months: Vec<&str> = defs.iter().map(|d| d.contract_month.as_str()).collect();
        assert_eq!(months, ["202612", "202703", "202706", "202709", "202712"]);
        let dates: Vec<&str> = defs.iter().map(|d| d.last_trade_date.as_str()).collect();
        assert_eq!(dates, ["20261218", "20270319", "20270618", "20270917", "20271217"]);
        for d in &defs {
            assert_eq!(d.symbol, "MNQ");
            assert_eq!(d.sec_type, SecurityType::Future);
            assert_eq!(d.exchange, "CME");
            assert_eq!(d.currency, "USD");
            assert_eq!(d.multiplier, 2.0);
            assert_eq!(d.market_rule_id, Some(67));
            assert_eq!(d.min_tick, 0.25);
            // Detail block joined by conId, order types by the record key.
            assert_eq!(d.long_name, "Micro E-Mini Nasdaq-100 Index");
            assert_eq!(d.valid_exchanges, ["CME"]);
            assert_eq!(d.order_types, ["LMT/3", "MKT/1", "STP/1"]);
        }
        assert_eq!(super::parse_secdef_response(&five_future_records("11", "")).unwrap().con_id, 815824267);
    }

    // A detail block goes only to the record with its conId.
    #[test]
    fn parse_records_joins_details_by_con_id() {
        let msg = pipe_msg(
            "35=d|320=1|323=4|55=AAA|167=STK|207=BEST|6008=1|15=USD|55=BBB|167=STK|207=BEST|6008=2|15=USD|\
             146=0|6344=2|6008=2|306=BBB INC|6046=BEST,NYSE,|455=US0000000002|456=4|6623=0|6008=1|306=AAA INC|6046=BEST,ARCA,|\
             6622=1|6623=0|6624=Financial"
        );
        let defs = parse_secdef_records(&msg).unwrap();
        assert_eq!(defs.len(), 2);
        assert_eq!((defs[0].symbol.as_str(), defs[0].long_name.as_str()), ("AAA", "AAA INC"));
        assert_eq!(defs[0].valid_exchanges, ["SMART", "ARCA"]);
        assert_eq!(defs[0].isin, "");
        assert_eq!((defs[1].symbol.as_str(), defs[1].long_name.as_str()), ("BBB", "BBB INC"));
        assert_eq!(defs[1].valid_exchanges, ["SMART", "NYSE"]);
        assert_eq!(defs[1].isin, "US0000000002");
        assert_eq!(defs[0].exchange, "SMART");
        // The industry of the record's key; a record without a key has none.
        assert_eq!((defs[1].industry.as_str(), defs[1].category.as_str()), ("Financial", ""));
        assert_eq!(defs[0].industry, "");
    }

    #[test]
    fn parse_records_of_an_empty_reply_is_an_empty_list() {
        let msg = pipe_msg("35=d|43=N|320=5|322=*|323=4|6038=Y|6019=0|6344=0");
        assert_eq!(parse_secdef_records(&msg).map(|d| d.len()), Some(0));
        assert!(parse_secdef_records(&pipe_msg("35=A|98=0")).is_none());
    }

    #[test]
    fn parse_rejects_non_secdef() {
        let msg = fix::fix_build(&[(TAG_MSG_TYPE, "A")], 1);
        assert!(super::parse_secdef_response(&msg).is_none());
    }

    // Regression for ibx#197: a US equity secdef carries a rule table
    // after its rule count. The count must NOT be read as min_tick (it
    // would yield 1.0) — min_tick is the smallest parsed increment.
    #[test]
    fn secdef_min_tick_from_price_increments_not_rule_sentinel() {
        let msg = fix::fix_build(
            &[
                (TAG_MSG_TYPE, "d"),
                (TAG_SYMBOL, "AXTI"),
                (TAG_IB_CON_ID, "4726868"),
                (TAG_SECURITY_TYPE, "CS"),
                (TAG_CURRENCY, "USD"),
                // Rule table with one rule of two price increments.
                (TAG_MARKET_RULE_COUNT, "1"),
                (TAG_MARKET_RULE_ID, "26"),
                (TAG_PRICE_INCREMENT_COUNT, "2"),
                (TAG_LOW_EDGE, "0"),
                (TAG_INCREMENT, "0.0001"),
                (TAG_LOW_EDGE, "1"),
                (TAG_INCREMENT, "0.01"),
            ],
            1,
        );
        let def = super::parse_secdef_response(&msg).unwrap();
        // Smallest increment across bands, not the rule count.
        assert_eq!(def.min_tick, 0.0001);
    }

    // Absent any inline rule block, min_tick keeps its default.
    #[test]
    fn secdef_min_tick_defaults_without_rule_block() {
        let msg = fix::fix_build(
            &[
                (TAG_MSG_TYPE, "d"),
                (TAG_SYMBOL, "AAPL"),
                (TAG_IB_CON_ID, "265598"),
                (TAG_SECURITY_TYPE, "CS"),
            ],
            1,
        );
        let def = super::parse_secdef_response(&msg).unwrap();
        assert_eq!(def.min_tick, 0.01);
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
                (TAG_SYMBOL, "AAPL"),
                (TAG_IB_CON_ID, "12345"),
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
        // Build a fake schedule response with 2 trading + 2 liquid sessions
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
    fn parse_schedule_dual_flag_appends_to_both() {
        // 24h venues (FOREX) emit sessions with both 6843=1 AND 6844=1.
        let msg = fix::fix_build(
            &[
                (TAG_MSG_TYPE, "U"),
                (TAG_SUB_PROTOCOL, "107"),
                (TAG_SCHEDULE_TIMEZONE, "US/Eastern"),
                (TAG_SESSION_COUNT, "1"),
                (TAG_SESSION_START, "20260427-22:15:00"),
                (TAG_SESSION_END, "20260428-22:00:00"),
                (TAG_TRADE_DATE, "20260427"),
                (TAG_IS_TRADING_HOURS, "1"),
                (TAG_IS_LIQUID_HOURS, "1"),
            ],
            1,
        );
        let sched = parse_schedule_response(&msg).unwrap();
        assert_eq!(sched.trading_hours.len(), 1);
        assert_eq!(sched.liquid_hours.len(), 1);
        assert_eq!(sched.trading_hours[0].start, sched.liquid_hours[0].start);
    }

    #[test]
    fn format_sessions_string_basic() {
        let sessions = vec![
            ScheduleSession {
                start: "20260427-13:30:00".into(),
                end: "20260427-20:00:00".into(),
                trade_date: "20260427".into(),
            },
            ScheduleSession {
                start: "20260428-13:30:00".into(),
                end: "20260428-20:00:00".into(),
                trade_date: "20260428".into(),
            },
        ];
        let s = format_sessions_string(&sessions);
        assert_eq!(s, "20260427:1330-20260427:2000;20260428:1330-20260428:2000");
    }

    #[test]
    fn format_sessions_string_empty() {
        assert_eq!(format_sessions_string(&[]), "");
    }

    /// ibx#258: a non-ASCII or short date never panics; the value is kept.
    #[test]
    fn format_sessions_string_keeps_non_ascii_and_short_dates() {
        // A replacement character (3 bytes) puts byte 12 inside a character.
        let bad = "20260427-13:3\u{FFFD}0:00".to_string();
        assert_eq!(bad.as_bytes()[8], b'-');
        assert_eq!(bad.as_bytes()[11], b':');
        let open = ScheduleSession { start: bad.clone(), end: "20260427-20:00:00".into(), trade_date: "20260427".into() };
        assert_eq!(format_sessions_string(&[open]), format!("{}-20260427:2000", bad));

        // Closed day with a short trade date and a non-ASCII start.
        let closed = ScheduleSession { start: "2026\u{FFFD}".into(), end: "2026\u{FFFD}".into(), trade_date: "2026".into() };
        assert_eq!(format_sessions_string(&[closed]), "2026\u{FFFD}:CLOSED");
        // A non-ASCII trade date cut inside a character falls back to the start.
        let closed = ScheduleSession { start: "20260427".into(), end: "20260427".into(), trade_date: "2026042\u{FFFD}".into() };
        assert_eq!(format_sessions_string(&[closed]), "20260427:CLOSED");
        // Short values on both sides.
        let closed = ScheduleSession { start: "".into(), end: "".into(), trade_date: "".into() };
        assert_eq!(format_sessions_string(&[closed]), ":CLOSED");
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

    #[test]
    fn market_rule_id_parsed() {
        let msg = fix::fix_build(
            &[
                (TAG_MSG_TYPE, "d"),
                (TAG_SYMBOL, "SPY"),
                (TAG_IB_CON_ID, "756733"),
                (TAG_SECURITY_TYPE, "CS"),
                (TAG_IB_MARKET_RULE_ID, "4563"),
            ],
            1,
        );
        let def = super::parse_secdef_response(&msg).unwrap();
        assert_eq!(def.market_rule_id, Some(4563));
    }

    #[test]
    fn market_rule_id_absent() {
        let msg = fix::fix_build(
            &[
                (TAG_MSG_TYPE, "d"),
                (TAG_SYMBOL, "SPY"),
                (TAG_IB_CON_ID, "756733"),
            ],
            1,
        );
        let def = super::parse_secdef_response(&msg).unwrap();
        assert_eq!(def.market_rule_id, None);
    }

    #[test]
    fn build_matching_symbols_request_structure() {
        let msg = build_matching_symbols_request("APP", "R1", 1);
        let tags = fix::fix_parse(&msg);
        assert_eq!(tags[&fix::TAG_MSG_TYPE], "U");
        assert_eq!(tags[&TAG_SUB_PROTOCOL], "185");
        assert_eq!(tags[&TAG_SECURITY_REQ_ID], "R1");
        assert_eq!(tags[&TAG_MATCH_PATTERN], "APP");
    }

    #[test]
    fn parse_matching_symbols_response_basic() {
        let msg = fix::fix_build(
            &[
                (TAG_MSG_TYPE, "U"),
                (TAG_SUB_PROTOCOL, "186"),
                (TAG_SECURITY_REQ_ID, "R1"),
                (TAG_MATCH_COUNT, "2"),
                // Match 1
                (TAG_SYMBOL, "AAPL"),
                (TAG_SECURITY_TYPE, "CS"),
                (TAG_CURRENCY, "USD"),
                (TAG_IB_CON_ID, "265598"),
                (TAG_MATCH_PRIMARY_EXCHANGE, "NASDAQ"),
                (TAG_MATCH_DESCRIPTION, "APPLE INC"),
                (TAG_MATCH_DERIVATIVE_TYPES, "OPT,WAR"),
                // Match 2
                (TAG_SYMBOL, "APP"),
                (TAG_SECURITY_TYPE, "CS"),
                (TAG_CURRENCY, "USD"),
                (TAG_IB_CON_ID, "481863646"),
                (TAG_MATCH_PRIMARY_EXCHANGE, "NASDAQ"),
                (TAG_MATCH_DESCRIPTION, "APPLOVIN CORP"),
                (TAG_MATCH_DERIVATIVE_TYPES, "OPT"),
            ],
            1,
        );
        let matches = parse_matching_symbols_response(&msg).unwrap();
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].symbol, "AAPL");
        assert_eq!(matches[0].con_id, 265598);
        assert_eq!(matches[0].primary_exchange, "NASDAQ");
        assert_eq!(matches[0].description, "APPLE INC");
        assert_eq!(matches[0].derivative_types, vec!["OPT", "WAR"]);
        assert_eq!(matches[1].symbol, "APP");
        assert_eq!(matches[1].con_id, 481863646);
        assert_eq!(matches[0].sec_type, "STK");
    }

    // ibx#439: the captured MSFT reply, a bond row and a row without conId.
    #[test]
    fn matching_symbols_rows_as_the_reference() {
        let msg = pipe_msg("35=U|6040=186|320=10|146=4|\
            55=MSFT|167=STK|15=USD|6008=272093|6453=NASDAQ|6470=NASDAQ.NMS|306=MICROSOFT CORP|6070=BAG,CFD,IOPT,OPT,WAR|6739=WAR|8499=1|8077=COMMON|8273=COMMON|8179=US/STK|\
            55=|310=BOND|15=USD|6008=123456|306=MICROSOFT CORP|6454=e1393444|8450=Corp|\
            55=EUR|167=CASH|15=USD|\
            55=MSF|167=FUND|15=EUR|6008=1|8533=BOND:2,STK:23,IND:2");
        let m = parse_matching_symbols_response(&msg).unwrap();
        assert_eq!(m.len(), 4);
        assert_eq!((m[0].con_id, m[0].symbol.as_str(), m[0].sec_type.as_str()), (272093, "MSFT", "STK"));
        assert_eq!((m[0].primary_exchange.as_str(), m[0].currency.as_str()), ("NASDAQ", "USD"));
        assert_eq!(m[0].description, "MICROSOFT CORP");
        assert_eq!(m[0].derivative_types, ["BAG", "CFD", "IOPT", "OPT", "WAR"]);
        assert_eq!(m[0].issuer_id, "");
        // A bond row: empty symbol, type from the alternative field, issuer id.
        assert_eq!((m[1].symbol.as_str(), m[1].sec_type.as_str(), m[1].issuer_id.as_str()), ("", "BOND", "e1393444"));
        // No conId: kept with -1.
        assert_eq!((m[2].con_id, m[2].sec_type.as_str()), (-1, "CASH"));
        // A type the engine does not know is kept as received.
        assert_eq!(m[3].sec_type, "FUND");
        // The wire stock code is given in its API form.
        let msg = pipe_msg("35=U|6040=186|320=1|146=1|55=AAPL|167=CS|6008=265598");
        assert_eq!(parse_matching_symbols_response(&msg).unwrap()[0].sec_type, "STK");
        // A conId that is not a number: the row is skipped.
        let msg = pipe_msg("35=U|6040=186|320=1|146=2|55=AAPL|167=STK|6008=x|55=IBM|167=STK|6008=8314");
        let m = parse_matching_symbols_response(&msg).unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].symbol, "IBM");
    }

    // ibx#223: a closed day (neither hours flag set) must be represented,
    // not dropped — "market closed" and "data missing" were previously
    // indistinguishable.
    #[test]
    fn schedule_closed_day_is_kept_and_renders_closed() {
        let msg = fix::fix_build(
            &[
                (TAG_MSG_TYPE, "U"),
                (TAG_SUB_PROTOCOL, "107"),
                (TAG_SCHEDULE_TIMEZONE, "US/Eastern"),
                (TAG_SESSION_COUNT, "2"),
                // Saturday: closed — no 6843/6844 flags.
                (TAG_SESSION_START, "20260718-00:00:00"),
                (TAG_SESSION_END, "20260718-00:00:00"),
                (TAG_TRADE_DATE, "20260718"),
                // Monday: normal trading session.
                (TAG_SESSION_START, "20260720-13:30:00"),
                (TAG_SESSION_END, "20260720-20:00:00"),
                (TAG_TRADE_DATE, "20260720"),
                (TAG_IS_TRADING_HOURS, "1"),
                (TAG_IS_LIQUID_HOURS, "1"),
            ],
            1,
        );
        let sched = parse_schedule_response(&msg).unwrap();
        assert_eq!(sched.trading_hours.len(), 2, "closed day must appear");
        assert_eq!(sched.liquid_hours.len(), 2);
        let rendered = format_sessions_string(&sched.trading_hours);
        assert_eq!(rendered, "20260718:CLOSED;20260720:1330-20260720:2000");
    }

    // ibx#223: an unrecognized security type must not be encoded as a stock.
    #[test]
    fn to_fix_other_is_not_stock() {
        assert_eq!(SecurityType::Other.to_fix(), "");
        assert_eq!(SecurityType::from_fix(""), SecurityType::Other);
    }

    // ibx#230: user-visible sec_type must be the official API string, and
    // an unclassifiable instrument must not masquerade as a stock.
    #[test]
    fn sec_type_to_api_str_round_trips_and_other_is_empty() {
        assert_eq!(SecurityType::Stock.to_api_str(), "STK");
        assert_eq!(SecurityType::Forex.to_api_str(), "CASH");
        assert_eq!(SecurityType::Warrant.to_api_str(), "WAR");
        assert_eq!(SecurityType::Other.to_api_str(), "");
        // Every non-Other variant survives the round trip back through the
        // inbound parser (which accepts API strings too), so a reported
        // Contract can be fed into another request.
        for st in [SecurityType::Stock, SecurityType::Option, SecurityType::Future,
                   SecurityType::Forex, SecurityType::Index, SecurityType::Bond,
                   SecurityType::Warrant] {
            assert_eq!(SecurityType::from_fix(st.to_api_str()), st, "{:?}", st);
        }
    }

    #[test]
    fn parse_matching_symbols_rejects_non_match() {
        let msg = fix::fix_build(&[(TAG_MSG_TYPE, "d")], 1);
        assert!(parse_matching_symbols_response(&msg).is_none());

        let msg = fix::fix_build(
            &[(TAG_MSG_TYPE, "U"), (TAG_SUB_PROTOCOL, "107")],
            1,
        );
        assert!(parse_matching_symbols_response(&msg).is_none());
    }

    #[test]
    fn parse_market_rules_single_rule() {
        let msg = fix::fix_build(
            &[
                (TAG_MSG_TYPE, "d"),
                (TAG_SYMBOL, "AAPL"),
                (TAG_IB_CON_ID, "265598"),
                // Rule table
                (TAG_MARKET_RULE_COUNT, "1"),
                (TAG_MARKET_RULE_ID, "26"),
                (TAG_PRICE_INCREMENT_COUNT, "2"),
                (TAG_LOW_EDGE, "0"),
                (TAG_INCREMENT, "0.01"),
                (TAG_LOW_EDGE, "1"),
                (TAG_INCREMENT, "0.01"),
            ],
            1,
        );
        let rules = parse_market_rules(&msg);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].rule_id, 26);
        assert_eq!(rules[0].price_increments.len(), 2);
        assert_eq!(rules[0].price_increments[0].low_edge, 0.0);
        assert_eq!(rules[0].price_increments[0].increment, 0.01);
        assert_eq!(rules[0].price_increments[1].low_edge, 1.0);
        assert_eq!(rules[0].price_increments[1].increment, 0.01);
    }

    #[test]
    fn parse_market_rules_multiple_rules() {
        let msg = fix::fix_build(
            &[
                (TAG_MSG_TYPE, "d"),
                (TAG_MARKET_RULE_COUNT, "2"),
                // Rule 1: penny increments
                (TAG_MARKET_RULE_ID, "26"),
                (TAG_PRICE_INCREMENT_COUNT, "1"),
                (TAG_LOW_EDGE, "0"),
                (TAG_INCREMENT, "0.01"),
                // Rule 2: nickel increments above $1
                (TAG_MARKET_RULE_ID, "42"),
                (TAG_PRICE_INCREMENT_COUNT, "2"),
                (TAG_LOW_EDGE, "0"),
                (TAG_INCREMENT, "0.01"),
                (TAG_LOW_EDGE, "1"),
                (TAG_INCREMENT, "0.05"),
            ],
            1,
        );
        let rules = parse_market_rules(&msg);
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].rule_id, 26);
        assert_eq!(rules[0].price_increments.len(), 1);
        assert_eq!(rules[1].rule_id, 42);
        assert_eq!(rules[1].price_increments.len(), 2);
        assert_eq!(rules[1].price_increments[1].low_edge, 1.0);
        assert_eq!(rules[1].price_increments[1].increment, 0.05);
    }

    #[test]
    fn parse_market_rules_empty_when_none() {
        let msg = fix::fix_build(
            &[
                (TAG_MSG_TYPE, "d"),
                (TAG_SYMBOL, "AAPL"),
                (TAG_IB_CON_ID, "265598"),
            ],
            1,
        );
        let rules = parse_market_rules(&msg);
        assert!(rules.is_empty());
    }

    // ── ibx#287: size increments and the round lot ──

    /// An AAPL definition with the given market type, security type and
    /// size increments (low edge, increment).
    fn aapl_secdef(market_type: &str, sec_type: &str, size_set: &[(&str, &str)]) -> Vec<u8> {
        let count = size_set.len().to_string();
        let mut tags: Vec<(u32, &str)> = vec![
            (TAG_MSG_TYPE, "d"), (TAG_IB_CON_ID, "265598"), (TAG_SYMBOL, "AAPL"),
            (TAG_SECURITY_TYPE, sec_type), (TAG_CURRENCY, "USD"), (TAG_IB_MARKET_TYPE, market_type),
            (TAG_MARKET_RULE_COUNT, "1"), (TAG_MARKET_RULE_ID, "26"),
            (6020, "0"), (6021, "0"), (6022, "1"), (TAG_LOW_EDGE, "0"), (6024, "4"), (6025, "2"),
            (TAG_PRICE_INCREMENT_COUNT, "1"), (TAG_LOW_EDGE, "0"), (TAG_INCREMENT, "0.01"),
            (6028, "0"), (6029, "1"), (TAG_LOW_EDGE, "0"), (6024, "6"), (6025, "0"),
        ];
        tags.push((TAG_SIZE_INCREMENT_COUNT, &count));
        for (edge, inc) in size_set {
            tags.push((TAG_LOW_EDGE, edge));
            tags.push((TAG_INCREMENT, inc));
        }
        fix::fix_build(&tags, 1)
    }

    #[test]
    fn size_increments_are_the_second_set_only() {
        let msg = aapl_secdef("USSTK", "CS", &[("40", "40")]);
        let sizes = parse_size_increments(&msg);
        assert_eq!(sizes.len(), 1);
        assert_eq!((sizes[0].low_edge, sizes[0].increment), (40.0, 40.0));
        // The price increments are unchanged.
        let def = super::parse_secdef_response(&msg).unwrap();
        assert!((def.min_tick - 0.01).abs() < 1e-12);
    }

    #[test]
    fn round_lot_of_a_us_stock_is_its_size_increment() {
        assert_eq!(round_lot_from_secdef(&aapl_secdef("USSTK", "CS", &[("40", "40")])), 40);
        // The smallest increment, integer part.
        assert_eq!(round_lot_from_secdef(&aapl_secdef("USSTK", "CS", &[("0", "100.5"), ("1000", "10.9")])), 10);
        // No size rule, or an increment below 1: 100.
        assert_eq!(round_lot_from_secdef(&aapl_secdef("USSTK", "CS", &[])), 100);
        assert_eq!(round_lot_from_secdef(&aapl_secdef("USSTK", "CS", &[("0", "0.0001")])), 100);
        // A US warrant too.
        assert_eq!(round_lot_from_secdef(&aapl_secdef("USWAR", "WAR", &[("0", "10")])), 10);
    }

    #[test]
    fn round_lot_of_other_contracts_is_one() {
        // A non-US stock, a US option, a future, a currency pair.
        assert_eq!(round_lot_from_secdef(&aapl_secdef("DESTK", "CS", &[("0", "40")])), 1);
        assert_eq!(round_lot_from_secdef(&aapl_secdef("USSTK", "OPT", &[("0", "40")])), 1);
        assert_eq!(round_lot_from_secdef(&aapl_secdef("", "FUT", &[("0", "1")])), 1);
        assert_eq!(round_lot_from_secdef(&aapl_secdef("", "CASH", &[("0", "1")])), 1);
    }

    #[test]
    fn parse_market_rules_last_rule_at_end_of_message() {
        // The last rule is kept when the message ends inside the table.
        let msg = fix::fix_build(
            &[
                (TAG_MSG_TYPE, "d"),
                (TAG_MARKET_RULE_COUNT, "1"),
                (TAG_MARKET_RULE_ID, "10"),
                (TAG_PRICE_INCREMENT_COUNT, "1"),
                (TAG_LOW_EDGE, "0"),
                (TAG_INCREMENT, "0.005"),
            ],
            1,
        );
        let rules = parse_market_rules(&msg);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].rule_id, 10);
        assert_eq!(rules[0].price_increments.len(), 1);
        assert_eq!(rules[0].price_increments[0].increment, 0.005);
    }

    /// ibx#437: the captured two-rule table. Rule 32 has one price
    /// increment, rule 109 two; the tier entries and the size set are not
    /// price increments.
    const TWO_RULE_TABLE: &str = "6019=2|6031=32|6020=0|6021=0|6022=1|6023=0|6024=4|6025=2|6026=1|6023=0|6027=0.01|6028=0|6029=1|6023=0|6024=6|6025=0|6030=1|6023=1|6027=1|6031=109|6020=0|6021=0|6022=1|6023=0|6024=4|6025=2|6026=2|6023=0|6027=0.01|6023=3|6027=0.05|6028=0|6029=1|6023=0|6024=6|6025=0|6030=1|6023=1|6027=1";

    fn rule_pairs(rule: &MarketRule) -> Vec<(f64, f64)> {
        rule.price_increments.iter().map(|p| (p.low_edge, p.increment)).collect()
    }

    #[test]
    fn parse_market_rules_captured_two_rule_table() {
        let msg = pipe_msg(&format!("35=d|320=1|55=AAPL|6008=265598|6031=32|146=1|6038=Y|{}|6344=1|6008=265598", TWO_RULE_TABLE));
        let rules = parse_market_rules(&msg);
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].rule_id, 32);
        assert_eq!(rule_pairs(&rules[0]), vec![(0.0, 0.01)]);
        assert_eq!(rules[1].rule_id, 109);
        assert_eq!(rule_pairs(&rules[1]), vec![(0.0, 0.01), (3.0, 0.05)]);
        // The size set of each rule is still read by the size parser.
        let sizes = parse_size_increments(&msg);
        assert_eq!(sizes.len(), 2);
        assert!(sizes.iter().all(|p| (p.low_edge, p.increment) == (1.0, 1.0)));
    }

    #[test]
    fn parse_market_rules_ignores_rule_ids_outside_the_table() {
        // The record's own rule id and a rule id after the table are not
        // rules.
        let msg = pipe_msg("35=d|55=AAPL|6031=7|6019=1|6031=32|6026=1|6023=0|6027=0.01|6344=1|6031=8|6026=1|6023=0|6027=5");
        let rules = parse_market_rules(&msg);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].rule_id, 32);
        assert_eq!(rule_pairs(&rules[0]), vec![(0.0, 0.01)]);
    }

    #[test]
    fn min_tick_is_the_smallest_increment_of_the_record_rule() {
        // Record on rule 109 in a two-rule table: its smallest increment.
        let table = TWO_RULE_TABLE.replace("6026=2|6023=0|6027=0.01", "6026=2|6023=0|6027=0.02");
        let msg = pipe_msg(&format!("35=d|320=1|55=AAPL|6008=265598|6031=109|146=1|6038=Y|{}|6344=1|6008=265598", table));
        let def = super::parse_secdef_response(&msg).unwrap();
        assert_eq!(def.market_rule_id, Some(109));
        assert!((def.min_tick - 0.02).abs() < 1e-12, "{}", def.min_tick);
    }

    // ── ibx#436: the API row sources ──

    #[test]
    fn api_sec_type_is_the_wire_type_with_stk_and_contfut() {
        for wire in ["FOP", "FUND", "CFD", "CRYPTO", "CMDTY", "BOND", "OPT", "IOPT"] {
            let d = ContractDefinition { wire_sec_type: wire.into(), sec_type: SecurityType::from_fix(wire), ..Default::default() };
            assert_eq!(d.api_sec_type(), wire);
        }
        let d = ContractDefinition { wire_sec_type: "CS".into(), ..Default::default() };
        assert_eq!(d.api_sec_type(), "STK");
        let d = ContractDefinition { wire_sec_type: "FUT".into(), continuous: true, ..Default::default() };
        assert_eq!((d.api_sec_type().as_str(), d.api_type_name().as_str()), ("CONTFUT", "FUT"));
        let bill = ContractDefinition { wire_sec_type: "BILL".into(), ..Default::default() };
        assert!(bill.is_bond());
    }

    // The wire right of a definition is 1 for a call, 0 for a put.
    #[test]
    fn right_and_dates_of_an_option_record() {
        let msg = pipe_msg("35=d|320=1|55=AAPL|167=OPT|207=BEST|6008=9|541=20261005|200=202610|6614=20261005|201=1|202=342.5|231=100|\
            146=0|6344=1|6008=9|6346=265598|310=STK|6855=AAPL|6850=1600");
        let d = super::parse_secdef_response(&msg).unwrap();
        assert_eq!(d.right, Some(OptionRight::Call));
        assert_eq!((d.last_trade_date.as_str(), d.contract_month.as_str()), ("20261005", "202610"));
        assert_eq!(d.real_expiration_date, "20261005");
        assert_eq!((d.under_con_id, d.under_symbol.as_str(), d.under_sec_type.as_str()), (265598, "AAPL", "STK"));
        assert_eq!(d.multiplier_text, "100");
        let put = pipe_msg("35=d|55=AAPL|167=OPT|6008=9|201=0");
        assert_eq!(super::parse_secdef_response(&put).unwrap().right, Some(OptionRight::Put));
        // A bond has only 541: its month is the month of that date.
        let bond = pipe_msg("35=d|55=IBM|167=BOND|6008=7|146=0|6344=1|6008=7|541=20451030");
        let b = super::parse_secdef_response(&bond).unwrap();
        assert_eq!((b.last_trade_date.as_str(), b.contract_month.as_str()), ("20451030", "204510"));
    }

    // Stock type from 8273, ISIN and CUSIP from the id group, industry
    // split in three, the primary exchange suffix only when listed, the
    // ineligibility reasons with their texts.
    #[test]
    fn detail_fields_of_a_record() {
        let msg = pipe_msg("35=d|320=1|55=AAPL|167=STK|207=BEST|6008=1|6178=1|146=0|6344=1|6008=1|6470=NASDAQ|8224=NMS|\
            454=3|455=BBG000B9XRY4|456=S|455=US0378331005|456=4|455=037833100|456=1|6623=0|8077=X|8273=COMMON|6765=i1,i2|\
            6046=BEST,NASDAQ,|6622=1|6623=0|6624=Technology");
        let mut msg = msg;
        // The industry text holds '|' bytes, not separators.
        let pos = msg.windows(16).position(|w| w == b"6624=Technology\x01").unwrap() + 15;
        msg.splice(pos..pos, b"|Computers|Computer Services".iter().copied());
        msg.extend_from_slice(b"6766=2\x016767=i1\x016768=First\x016767=i2\x016768=Second\x01");
        let d = super::parse_secdef_response(&msg).unwrap();
        assert_eq!(d.stock_type, "COMMON");
        assert_eq!((d.isin.as_str(), d.cusip.as_str()), ("US0378331005", "037833100"));
        assert_eq!((d.industry.as_str(), d.category.as_str(), d.subcategory.as_str()), ("Technology", "Computers", "Computer Services"));
        assert_eq!(d.primary_exchange, "NASDAQ");
        assert_eq!(d.agg_group, Some(1));
        assert_eq!(d.ineligibility, vec![("i1".to_string(), "First".to_string()), ("i2".to_string(), "Second".to_string())]);
        let listed = pipe_msg("35=d|55=X|167=STK|6008=1|146=0|6344=1|6008=1|6470=NASDAQ|8224=NMS|6046=BEST,NASDAQ.NMS,");
        assert_eq!(super::parse_secdef_response(&listed).unwrap().primary_exchange, "NASDAQ.NMS");
    }

    // Records of one underlying share its name, symbol and type when a
    // detail block leaves them out (the captured five-month reply).
    #[test]
    fn records_of_one_underlying_share_its_data() {
        let msg = pipe_msg("35=d|320=1|55=MNQ|167=FUT|207=CME|6008=1|55=MNQ|167=FUT|207=CME|6008=2|146=0|6344=2|\
            6008=1|6346=9|310=IND|6855=MNQ|306=Micro Index|6008=2|6346=9");
        let defs = parse_secdef_records(&msg).unwrap();
        assert_eq!((defs[1].long_name.as_str(), defs[1].under_symbol.as_str(), defs[1].under_sec_type.as_str()),
            ("Micro Index", "MNQ", "IND"));
    }

    // Fund and bond fields: text flags (T/Y) for the bond, number flags
    // for the fund.
    #[test]
    fn fund_and_bond_fields() {
        let msg = pipe_msg("35=d|55=F|167=FUND|6008=1|8502=N|8503=001|146=0|6344=1|6008=1|6481=Name|6472=Family|6473=Type|\
            6474=1%|6475=2%|6482=90|6476=0.5|6477=1|6511=0|6512=1|6478=10|6479=100|8505=50|8150=NY|8151=PR");
        let f = super::parse_secdef_response(&msg).unwrap().fund;
        assert_eq!((f.name.as_str(), f.family.as_str(), f.fund_type.as_str()), ("Name", "Family", "Type"));
        assert_eq!((f.front_load.as_str(), f.back_load.as_str(), f.back_load_time_interval.as_str()), ("1%", "2%", "90"));
        assert_eq!((f.closed, f.closed_for_new_investors, f.closed_for_new_money), (true, false, true));
        assert_eq!((f.notify_amount.as_str(), f.minimum_initial_purchase.as_str(), f.subsequent_minimum_purchase.as_str()), ("10", "100", "50"));
        assert_eq!((f.blue_sky_states.as_str(), f.blue_sky_territories.as_str()), ("NY", "PR"));
        assert_eq!((f.distribution_policy_indicator.as_str(), f.asset_type.as_str()), ("N", "001"));
        let msg = pipe_msg("35=d|55=IBM|167=BOND|6008=7|146=0|6344=1|6008=7|225=19951030|6495=Bonds|6853=IBM 7 10/30/45|6494=Append|\
            6497=N|6498=T|6499=F|6500=Y|6501=20270801|6502=P|6493=Notes|6496=FIXED|255=A3|223=7");
        let b = super::parse_secdef_response(&msg).unwrap().bond;
        assert_eq!((b.callable, b.putable, b.convertible, b.next_option_partial), (false, true, false, true));
        assert_eq!((b.issue_date.as_str(), b.bond_type.as_str(), b.coupon_type.as_str(), b.coupon.as_str()), ("19951030", "Bonds", "FIXED", "7"));
        assert_eq!((b.desc_append.as_str(), b.short_description.as_str(), b.notes.as_str()), ("Append", "IBM 7 10/30/45", "Notes"));
        assert_eq!((b.next_option_date.as_str(), b.next_option_type.as_str(), b.ratings.as_str()), ("20270801", "P", "A3"));
    }

    // The size set and the magnifier of the record's rule.
    #[test]
    fn size_set_and_magnifier_of_the_record_rule() {
        let table = TWO_RULE_TABLE.replace("6031=109|6020=0|6021=0", "6031=109|6020=0|6021=2");
        let msg = pipe_msg(&format!("35=d|320=1|55=AAPL|6008=265598|6031=109|146=1|6038=Y|{}|6344=1|6008=265598", table));
        let d = super::parse_secdef_response(&msg).unwrap();
        assert_eq!(d.price_magnifier, 100);
        assert_eq!(d.size_increments.iter().map(|p| (p.low_edge, p.increment)).collect::<Vec<_>>(), vec![(1.0, 1.0)]);
        let rules = parse_market_rules(&msg);
        assert_eq!((rules[0].price_magnifier, rules[1].price_magnifier), (1, 100));
    }

    #[test]
    fn a_cusip_symbol() {
        assert!(reads_as_cusip("459200AN1"));
        assert!(reads_as_cusip("IBCID29105555"));
        assert!(!reads_as_cusip("IBM"));
        assert!(!reads_as_cusip("IBCID"));
    }

    /// A schedule reply with sessions `(start, end, trade date, flags)`,
    /// flags T for trading, L for liquid.
    fn schedule(zone: &str, sessions: &[(&str, &str, &str, &str)]) -> ContractSchedule {
        let mut text = format!("35=U|6040=107|6256=K|6734={}|6840={}|", zone, sessions.len());
        for (s, e, d, f) in sessions {
            text += &format!("6841={}|6842={}|75={}|", s, e, d);
            if f.contains('T') { text += "6843=1|"; }
            if f.contains('L') { text += "6844=1|"; }
        }
        parse_schedule_response(&pipe_msg(&text)).unwrap()
    }

    fn at(utc: &str) -> jiff::Timestamp {
        jiff::civil::DateTime::strptime("%Y%m%d-%H:%M:%S", utc).unwrap()
            .to_zoned(jiff::tz::TimeZone::UTC).unwrap().timestamp()
    }

    // The captured futures shape: hours in the zone, from the current day
    // on; a closed day left out when the next session starts on it; the
    // liquid hours of a day without any are its trading hours.
    #[test]
    fn hours_in_the_zone_from_the_current_day() {
        let sched = schedule("US/Central", &[
            ("20260930-22:00:00", "20261001-21:00:00", "20261001", "T"),
            ("20261001-22:00:00", "20261002-21:00:00", "20261002", "T"),
            ("20261004-22:00:00", "20261005-21:00:00", "20261005", "T"),
            ("20261005-22:00:00", "20261006-21:00:00", "20261006", "T"),
            ("20261002-13:30:00", "20261002-21:00:00", "20261002", "L"),
            ("20261005-13:30:00", "20261005-21:00:00", "20261005", "L"),
        ]);
        let mut def = ContractDefinition { wire_sec_type: "FUT".into(), ..Default::default() };
        apply_schedule(&mut def, &sched, at("20261002-09:00:00"));
        assert_eq!(def.time_zone_id.as_deref(), Some("US/Central"));
        assert_eq!(def.trading_hours.as_deref(), Some(
            "20261001:1700-20261002:1600;20261003:CLOSED;20261004:1700-20261005:1600;20261005:1700-20261006:1600"));
        assert_eq!(def.liquid_hours.as_deref(), Some(
            "20261002:0830-20261002:1600;20261003:CLOSED;20261004:CLOSED;20261005:0830-20261005:1600;20261005:1700-20261006:1600"));
        assert_eq!(def.schedule_expiry, None);
        // The current day and the next one are there even without sessions.
        let empty = schedule("US/Eastern", &[]);
        let mut def = ContractDefinition::default();
        apply_schedule(&mut def, &empty, at("20261002-15:00:00"));
        assert_eq!(def.trading_hours.as_deref(), Some("20261002:CLOSED;20261003:CLOSED"));
    }

    fn option_week() -> ContractSchedule {
        schedule("US/Eastern", &[
            ("20261002-13:30:00", "20261002-20:00:00", "20261002", "TL"),
            ("20261005-13:30:00", "20261005-20:00:00", "20261005", "TL"),
            ("20261006-13:30:00", "20261006-20:00:00", "20261006", "TL"),
        ])
    }

    // The hours end at the last trading instant: later days and sessions
    // left out, the closed days before them kept; the last trade time is
    // 6850 in the zone, else the end of the liquid session of that date.
    #[test]
    fn hours_end_at_the_last_trading_instant() {
        let mut def = ContractDefinition { wire_sec_type: "OPT".into(), last_trade_date: "20261005".into(), last_trade_time: "1200".into(), ..Default::default() };
        apply_schedule(&mut def, &option_week(), at("20261002-14:00:00"));
        assert_eq!(def.trading_hours.as_deref(), Some(
            "20261002:0930-20261002:1600;20261003:CLOSED;20261004:CLOSED;20261005:0930-20261005:1200"));
        assert_eq!(def.schedule_expiry, Some(("20261005".to_string(), "12:00:00".to_string())));
        let mut def = ContractDefinition { wire_sec_type: "OPT".into(), last_trade_date: "20261005".into(), ..Default::default() };
        apply_schedule(&mut def, &option_week(), at("20261002-14:00:00"));
        assert_eq!(def.schedule_expiry, Some(("20261005".to_string(), "16:00:00".to_string())));
        assert_eq!(def.liquid_hours.as_deref(), Some(
            "20261002:0930-20261002:1600;20261003:CLOSED;20261004:CLOSED;20261005:0930-20261005:1600"));
        // A bond has no session time: the date alone.
        let mut def = ContractDefinition { wire_sec_type: "BOND".into(), sec_type: SecurityType::Bond, last_trade_date: "20261005".into(), ..Default::default() };
        apply_schedule(&mut def, &option_week(), at("20261002-14:00:00"));
        assert_eq!(def.schedule_expiry, Some(("20261005".to_string(), String::new())));
    }

    // A contract past its last trading instant gets no schedule (the
    // captured matured bonds).
    #[test]
    fn a_past_contract_has_no_schedule() {
        let mut def = ContractDefinition { wire_sec_type: "BOND".into(), sec_type: SecurityType::Bond, last_trade_date: "20220801".into(), ..Default::default() };
        apply_schedule(&mut def, &option_week(), at("20261002-14:00:00"));
        assert_eq!((def.time_zone_id, def.trading_hours, def.liquid_hours, def.schedule_expiry), (None, None, None, None));
        // Past the same day, after its time.
        let mut def = ContractDefinition { wire_sec_type: "OPT".into(), last_trade_date: "20261002".into(), last_trade_time: "1600".into(), ..Default::default() };
        apply_schedule(&mut def, &option_week(), at("20261002-21:00:00"));
        assert!(def.trading_hours.is_none());
    }
}
