//! Shared dispatch core for Rust and Python EClient implementations.
//!
//! `ClientCore` owns all subscription tracking state (reqId maps, change-detection
//! snapshots, PnL/account subscriptions) and exposes "prepare" methods that return
//! intermediate structs. Language-specific EClient adapters convert these into their
//! respective callback formats (Rust `Wrapper` trait calls or PyO3 `call_method`).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::engine::park::ControlSender;

use crate::api::types::{
    Contract as ApiContract, CommissionAndFeesReport as ApiCommissionAndFeesReport,
    Execution as ApiExecution, ExecutionFilter,
    Order as ApiOrder, OrderState as ApiOrderState,
    PRICE_SCALE_F, QTY_SCALE_F,
};
use crate::bridge::SharedState;
use crate::types::*;

/// The longest wait for the order replay of the logon before a next
/// valid order id is given (`ClientCore::wait_order_replay`); the replay
/// ends well within a second of the logon.
pub const ORDER_REPLAY_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// The market data type of real-time data (ibx#234).
const MDT_REALTIME: i32 = 1;
/// The market data type of frozen data (ibx#447).
const MDT_FROZEN: i32 = 2;

// ── Tick type constants matching ibapi ──

pub const TICK_BID: i32 = 1;
pub const TICK_ASK: i32 = 2;
pub const TICK_LAST: i32 = 4;
pub const TICK_HIGH: i32 = 6;
pub const TICK_LOW: i32 = 7;
pub const TICK_CLOSE: i32 = 9;
pub const TICK_OPEN: i32 = 14;
pub const TICK_BID_SIZE: i32 = 0;
pub const TICK_ASK_SIZE: i32 = 3;
pub const TICK_LAST_SIZE: i32 = 5;
pub const TICK_VOLUME: i32 = 8;
pub const TICK_LAST_TIMESTAMP: i32 = 45;
pub const TICK_BID_EXCHANGE: i32 = 32;
pub const TICK_ASK_EXCHANGE: i32 = 33;
pub const TICK_LAST_EXCHANGE: i32 = 84;
pub const TICK_HALTED: i32 = 49;

// ── Shared account field definitions ──



/// Render an exchange bitmask of a contract to its letters, as the
/// reference: each set bit picks the letter of that bit in the exchange
/// map of the contract's BBO exchange (ibx#441); no map yet gives an empty
/// text.
pub fn render_exchange_mask(mask: i64, instrument: InstrumentId, shared: &SharedState) -> String {
    render_exchange_mask_at(mask, instrument, shared, u64::MAX)
}

/// [`render_exchange_mask`] for a step of the market data queue: with the
/// map as known when the engine wrote it (ibx#446).
pub fn render_exchange_mask_at(mask: i64, instrument: InstrumentId, shared: &SharedState, seq: u64) -> String {
    if mask == 0 {
        return String::new();
    }
    let Some(components) = shared.reference.instrument_exchange_map_at(instrument, seq) else { return String::new() };
    let mut out = String::with_capacity(8);
    let mut bits = mask as u64;
    while bits != 0 {
        let bit = bits.trailing_zeros() as i32;
        bits &= bits - 1;
        if let Some(c) = components.iter().find(|c| c.bit_number == bit) {
            out.push_str(&c.exchange_letter);
        }
    }
    out
}

/// How long a reqSmartComponents waits for the exchange map of a known BBO
/// exchange, as the reference (ibx#441).
pub const SMART_COMPONENTS_WAIT: std::time::Duration = std::time::Duration::from_millis(2000);

/// A reqSmartComponents waiting for its exchange map: (reqId, code,
/// security type id, deadline).
type SmartComponentsWait = (i64, String, Option<u8>, std::time::Instant);

/// The answer to a reqSmartComponents: its components, or an error.
pub type SmartComponentsAnswer = Result<Vec<SmartComponent>, (i64, String)>;

/// The BBO exchange text of reqSmartComponents split as the reference
/// splits it (ibx#441): with 4 to 8 characters, the last 4 are the
/// security type id in hex (`9c0001` = code `9c`, STK); a text whose id is
/// no known security type, or of another length, is the code itself.
pub fn split_bbo_exchange(bbo: &str) -> (String, Option<u8>) {
    let chars: Vec<char> = bbo.chars().collect();
    if !(4..=8).contains(&chars.len()) {
        return (bbo.to_string(), None);
    }
    let code: String = chars[..chars.len() - 4].iter().collect();
    let id: String = chars[chars.len() - 4..].iter().collect();
    // The id is read as an int and kept as a byte, as the reference.
    match i32::from_str_radix(&id, 16).ok().map(|v| v as u8).filter(|v| sec_type_by_id(*v).is_some()) {
        Some(sec_type_id) => (code, Some(sec_type_id)),
        None => (bbo.to_string(), None),
    }
}

/// Error text of a reqSmartComponents whose exchange map did not come in
/// time (ibx#441).
fn smart_components_timeout(code: &str, sec_type_id: Option<u8>) -> (i64, String) {
    let sec_type = sec_type_id.and_then(sec_type_by_id).unwrap_or("null");
    (2147483647, format!("Unable to retrieve smart components for BBO exchange {} and security type {}", code, sec_type))
}

// ── Intermediate dispatch structs ──

/// One market data callback of a request.
#[derive(Debug, Clone, PartialEq)]
pub enum MdTick {
    /// `tick_price`; only a bid or an ask can execute automatically.
    Price { tick_type: i32, value: f64, can_auto_execute: bool },
    /// `tick_size`.
    Size { tick_type: i32, value: f64 },
    /// `tick_string`: exchange letters (32, 33).
    Text { tick_type: i32, value: String },
    /// `tick_string` of the last trade time (45, 88): epoch seconds, as
    /// text (`epoch_text`).
    Time { tick_type: i32, secs: i64 },
    /// `tick_generic`: the halted state (49, 90).
    Generic { tick_type: i32, value: f64 },
}

/// The text of a whole number, written in `buf` (no allocation).
pub fn epoch_text(secs: i64, buf: &mut [u8; 24]) -> &str {
    let mut n = secs.unsigned_abs();
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 { break; }
    }
    if secs < 0 {
        i -= 1;
        buf[i] = b'-';
    }
    std::str::from_utf8(&buf[i..]).unwrap_or("")
}

mod md_stream;
pub use md_stream::{MdOut, StreamState};

/// Security types and exchanges whose bid and ask auto-execution comes
/// from the farm on a stream; for every other contract the reference sets
/// both (ibx#446).
fn auto_execution_from_farm(sec_type: &str, exchange: &str) -> bool {
    let option = ["OPT", "FOP", "WAR", "IOPT"].iter().any(|t| t.eq_ignore_ascii_case(sec_type));
    let exchange = if exchange.is_empty() { "SMART" } else { exchange };
    option && ["SMART", "CBOE", "AMEX", "PHLX", "PSE", "ISE"].iter().any(|e| e.eq_ignore_ascii_case(exchange))
}

/// Net cash traded today on a position, for the daily P&L.
/// moneyTradedSinceMidnight (wire 6822) is signed net cash: SELL positive,
/// BUY negative (ib-agent#163). The seed's value, plus the cash of this
/// session's fills after it (a fill moved the daily P&L by the whole trade
/// value: +770 for 1 SPY bought, seen on paper 25/09/2026). With no seed row:
/// the cash of this session's fills when there are some, else the opening
/// trade's cash synthesized from the average cost (-qty * avgCost).
fn money_traded_today(seed: Option<&MidnightSeed>, since_seed: Option<f64>, qty_now: f64, avg_cost: Price) -> f64 {
    match (seed, since_seed) {
        (Some(s), since) => s.money_traded + since.unwrap_or(0.0),
        (None, Some(since)) => since,
        (None, None) => -(qty_now * avg_cost as f64 / PRICE_SCALE_F),
    }
}

/// Quotes ibx subscribes to by itself while a P&L request runs: the P&L
/// needs the last price and the previous close of each position, and a
/// client that did not subscribe had none (daily P&L unset). They never
/// reach the tick callbacks.
#[derive(Default)]
pub struct PnlQuotes {
    /// conId → instrument of a running internal subscription.
    active: HashMap<i64, InstrumentId>,
    /// conId → registration reply not received yet.
    pending: HashMap<i64, crossbeam_channel::Receiver<Result<InstrumentId, String>>>,
    /// Inputs of the last check: position generation, P&L requests, seeds.
    key: (u64, usize, usize, usize),
    checked_at: Option<std::time::Instant>,
}

impl PnlQuotes {
    /// Time of the last check (tests move it back to force a check).
    #[doc(hidden)]
    pub fn checked_at_mut(&mut self) -> Option<&mut std::time::Instant> {
        self.checked_at.as_mut()
    }
}

/// How often the internal P&L quotes are checked when nothing else changed.
const PNL_QUOTES_CHECK: std::time::Duration = std::time::Duration::from_secs(1);

/// Last values of a P&L request before its first callback.
const PNL_NOT_SENT: i64 = i64::MIN;

/// A provider code is a subscribed news source of the session; the lookup
/// ignores case, as the reference's (ibx#459).
fn news_source_subscribed(code: &str, sources: &[String]) -> bool {
    !code.is_empty() && sources.iter().any(|s| s.eq_ignore_ascii_case(code))
}

/// The news tick (292) of a generic tick list, as the reference's parser
/// reads it (ibx#458): tokens split on `,` and trimmed, `mdoff` is no tick,
/// `292:CODES` names the provider codes. `292:` with nothing after the
/// colon makes the reference refuse the whole list (321).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NewsTick {
    /// No news tick in the list.
    None,
    /// `292`: the default providers.
    Default,
    /// `292:CODES`: the codes, `+` separated.
    Codes(String),
    /// `292:`: the list is not valid.
    Invalid,
}

pub(crate) fn news_tick(generic_tick_list: &str) -> NewsTick {
    for token in generic_tick_list.split(',').map(str::trim) {
        let (id, param) = match token.split_once(':') {
            Some((id, param)) => (id.trim(), Some(param)),
            None => (token, None),
        };
        if id.parse::<i32>() != Ok(292) {
            continue;
        }
        return match param {
            None => NewsTick::Default,
            Some("") => NewsTick::Invalid,
            Some(codes) => NewsTick::Codes(codes.to_string()),
        };
    }
    NewsTick::None
}

/// Security types the reference calls derivatives: no news for them.
const NEWS_DERIVATIVES: [&str; 9] = ["FUT", "OPT", "FOP", "WAR", "IOPT", "CFD", "FWD", "SLB", "ICS"];

/// The provider key of a news tick (ibx#458), or the text of the
/// reference's 10094: a derivative contract is refused; explicit `codes`
/// must each be a subscribed API news source (an unknown or unchecked code
/// gets the "unchecked" text, the failures joined with `,`); without codes,
/// every subscribed source. The key is the codes sorted, without repeats,
/// in the sources' own spelling.
pub(crate) fn news_providers(sec_type: &str, codes: Option<&str>, sources: &[String]) -> Result<Vec<String>, String> {
    if NEWS_DERIVATIVES.contains(&sec_type.to_ascii_uppercase().as_str()) {
        return Err("API News error:Derivative contracts cannot be used to subscribe to news, please use the underlying \
            (Stocks, Cash, News Topics, and certain Indexes are supported).".to_string());
    }
    let mut key: Vec<String> = match codes {
        None => sources.to_vec(),
        Some(codes) => {
            // A Java split on `+`: trailing empty codes are dropped.
            let mut list: Vec<&str> = codes.split('+').collect();
            while list.len() > 1 && list.last() == Some(&"") { list.pop(); }
            let mut key = Vec::new();
            let mut failed: Vec<String> = Vec::new();
            for code in list {
                match sources.iter().find(|s| news_source_subscribed(code, std::slice::from_ref(s))) {
                    Some(source) => key.push(source.clone()),
                    None => failed.push(format!("Source code unchecked in API news Settings: {}", code)),
                }
            }
            if !failed.is_empty() {
                return Err(format!("API News error:{}", failed.join(",")));
            }
            key
        }
    };
    key.sort();
    key.dedup();
    Ok(key)
}

/// The reference's account checks of a P&L request (ibx#478): 321 for an
/// empty account or one this session is not logged in to.
fn pnl_account_refusal(class: &str, account: &str, own_account: &str) -> Result<(), (i64, String)> {
    let cause = if account.is_empty() {
        "Account must not be empty"
    } else if account != own_account {
        "Invalid account code"
    } else {
        return Ok(());
    };
    Err((321, format!("Error validating request.-'{}' : cause - {}", class, cause)))
}

/// API level of a session (ibx#426). The reference speaks levels 100 to
/// 214 and answers the lower of 214 and the client's highest level; the
/// current clients offer more than 214, so they get 214. Requests of higher
/// levels are not implemented.
pub const SERVER_VERSION: i32 = 214;

/// Connection time of a session in the reference's form
/// `yyyyMMdd HH:mm:ss {zone}`, local time of the machine (ibx#426). The zone
/// is the machine zone of the session (`IBX_TZ` when set).
pub fn connection_time_now() -> String {
    let zone = crate::gateway::machine_time_zone();
    connection_time(&jiff::Timestamp::now().to_zoned(crate::gateway::machine_tz()), &zone)
}

/// `connection_time_now` for a given time and zone name.
pub fn connection_time(at: &jiff::Zoned, zone: &str) -> String {
    format!("{} {}", at.strftime("%Y%m%d %H:%M:%S"), zone)
}

/// Pattern of a matching symbols request as the reference checks and
/// sends it (ibx#439): an empty or blank pattern, or one with a character
/// that is neither a printable ASCII character nor a space, gives 321; the
/// pattern sent is trimmed, with runs of spaces made one space.
///
/// A session whose logon feature list has no SECDEFTA (`allowed` false)
/// is refused first, with the reference's cause (ibx#421).
pub fn matching_symbols_pattern(pattern: &str, allowed: bool) -> Result<String, (i64, String)> {
    let refuse = |cause: String| Err((321, format!("Error validating request.-'ce' : cause - {}", cause)));
    if !allowed {
        log::info!("Not allowed (SECDEFTA feature not set).");
        return refuse("Failed to request matching symbols".into());
    }
    if pattern.trim_matches(|c: char| c <= ' ').is_empty() {
        return refuse("Pattern must not be empty".into());
    }
    if !pattern.chars().all(|c| c == ' ' || c.is_ascii_graphic()) {
        return refuse(format!("Invalid pattern: '{}'", pattern));
    }
    Ok(pattern.split(' ').filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" "))
}

/// Answer of a market rule request (ibx#437), as the reference: the
/// rule's price increments, or 322 when the id was not received in a
/// definition reply, or when its rule has no price increments.
pub fn market_rule_answer(
    rule: Option<crate::control::contracts::MarketRule>,
    market_rule_id: i32,
) -> Result<Vec<crate::control::contracts::PriceIncrement>, (i64, String)> {
    let refuse = |cause: String| Err((322, format!("Error processing request.-'cd' : cause - {}", cause)));
    match rule {
        None => refuse(format!("Market rule with id = {} is missing", market_rule_id)),
        Some(rule) if rule.price_increments.is_empty() => {
            refuse(format!("Price increment rule for market rule with id = {} is missing", market_rule_id))
        }
        Some(rule) => Ok(rule.price_increments),
    }
}

/// PnL update (account-level).
pub struct PnlUpdate {
    pub req_id: i64,
    pub daily_pnl: f64,
    pub unrealized_pnl: f64,
    pub realized_pnl: f64,
}

/// PnL single update (per-position).
pub struct PnlSingleUpdate {
    pub req_id: i64,
    pub pos: f64,
    pub daily_pnl: f64,
    pub unrealized_pnl: f64,
    pub realized_pnl: f64,
    pub value: f64,
}

/// A single changed account field.
pub struct AccountFieldUpdate {
    pub key: String,
    pub value: String,
    pub currency: String,
}

/// Batch of account update results (ibx#475).
pub struct AccountUpdateBatch {
    /// Account values to send: every value for the first image, then only
    /// the ones that changed.
    pub fields: Vec<AccountFieldUpdate>,
    /// The latest row time as `HH:mm`, for update_account_time.
    pub time: String,
    /// This batch is the first image: account_download_end follows it,
    /// once per subscription.
    pub download_end: bool,
}

/// This client's account stream (ibx#475).
#[derive(Default)]
pub struct AccountStream {
    /// Subscribed, and the first image not sent yet.
    image_pending: bool,
    /// Last value sent per (key, currency).
    sent: HashMap<(String, String), String>,
    /// Row store generation last read.
    generation: u64,
}

/// The reference's answer to an unsubscribe from account updates (ibx#475).
pub const ACCOUNT_UNSUBSCRIBED: (i64, &str) = (2100, "API client has been unsubscribed from account data.");

/// A row time as update_account_time carries it: `HH:mm` in the zone of
/// the machine, as the reference reads it with a calendar of its default
/// zone (`jfix.l.a()@25-30`, `jutils.d1.e(int)`; captured 07/10/2026 on a
/// machine in Europe/Paris: 11:49 for a row of 09:49 UTC). Empty before any
/// row time.
pub fn format_account_time(unix_secs: i64) -> String {
    if unix_secs <= 0 {
        return String::new();
    }
    let Ok(ts) = jiff::Timestamp::from_second(unix_secs) else {
        return String::new();
    };
    ts.to_zoned(crate::gateway::machine_tz()).strftime("%H:%M").to_string()
}

/// The keys of a ledger row in an account summary and the tag of each
/// value, in the reference's order (`jaccount.X.i()`, `X.a(int, aN, ...)`;
/// ACCOUNT-SUMMARY 1.3). `Currency`, `AccountOrGroup` and `RealCurrency`
/// are texts; InsuredDeposit is a key only with an API setting off by
/// default, so 8174 is added to CashBalance.
const LEDGER_SUMMARY_KEYS: &[(&str, u32)] = &[
    ("Currency", 15), ("CashBalance", 9806), ("TotalCashBalance", 9818), ("AccruedCash", 6242),
    ("StockMarketValue", 9807), ("OptionMarketValue", 9808), ("FutureOptionValue", 9809), ("FuturesPNL", 9810),
    ("NetLiquidationByCurrency", 9819), ("UnrealizedPnL", 6100), ("RealizedPnL", 6099), ("ExchangeRate", 9820),
    ("FundValue", 6483), ("NetDividend", 6681), ("MutualFundValue", 6682), ("MoneyMarketFundValue", 6683),
    ("CorporateBondValue", 6684), ("TBondValue", 6685), ("TBillValue", 6686), ("WarrantValue", 6687),
    ("FxCashBalance", 6711), ("AccountOrGroup", 0), ("RealCurrency", 0), ("IssuerOptionValue", 6924),
    ("Cryptocurrency", 8406),
];

/// The numeric tags of a ledger row the sums add up (`jextend.ef.a()`).
const LEDGER_SUM_TAGS: &[u32] = &[9806, 9818, 6242, 9807, 9808, 9809, 9810, 9819, 6100, 6099, 9820, 6483, 6681,
    6682, 6683, 6684, 6685, 6686, 6687, 6711, 6924, 8406, 8174, 6925, 6926, 8007, 8398];

/// The account summary rows of a ledger row, for `account`.
fn ledger_summary_rows(r: &crate::bridge::LedgerRow, account: &str) -> Vec<crate::bridge::AccountRow> {
    let currency = if r.real_currency.is_empty() { r.currency.clone() } else { r.real_currency.clone() };
    LEDGER_SUMMARY_KEYS.iter().map(|&(key, tag)| {
        let value = match key {
            "Currency" | "RealCurrency" => currency.clone(),
            "AccountOrGroup" => account.to_string(),
            "CashBalance" => {
                let cash = r.value(9806).map(|c| c + r.value(8174).unwrap_or(0.0));
                cash.map(java_account_value).unwrap_or_default()
            }
            _ => r.value(tag).map(java_account_value).unwrap_or_default(),
        };
        crate::bridge::AccountRow { key: key.to_string(), value, currency: r.currency.clone(), ledger: true }
    }).collect()
}

/// The accounts of the kept ledger rows, in the order first seen.
fn accounts_of(rows: &[crate::bridge::LedgerRow]) -> Vec<String> {
    let mut accounts: Vec<String> = Vec::new();
    for r in rows {
        if !accounts.contains(&r.account) {
            accounts.push(r.account.clone());
        }
    }
    accounts
}

/// One ledger row per currency summed over the accounts, in the order of
/// the reference's map (`jextend.ef.a()`): accounts and currencies are
/// visited in their hash map order; a value missing in one row counts as
/// missing (`ef.a(double, double)`).
fn ledger_sums(rows: &[crate::bridge::LedgerRow]) -> Vec<crate::bridge::LedgerRow> {
    let mut sums: Vec<crate::bridge::LedgerRow> = Vec::new();
    for account in java_hash_order(&accounts_of(rows)) {
        let currencies: Vec<String> = rows.iter().filter(|r| r.account == account).map(|r| r.currency.clone()).collect();
        for currency in java_hash_order(&currencies) {
            let Some(row) = rows.iter().find(|r| r.account == account && r.currency == currency) else { continue };
            let k = match sums.iter().position(|s| s.currency == currency) {
                Some(k) => k,
                None => {
                    sums.push(crate::bridge::LedgerRow {
                        account: "All".into(), currency: currency.clone(), real_currency: row.real_currency.clone(),
                        values: Vec::new(),
                    });
                    sums.len() - 1
                }
            };
            for &tag in LEDGER_SUM_TAGS {
                if let Some(v) = row.value(tag) {
                    match sums[k].values.iter_mut().find(|(t, _)| *t == tag) {
                        Some((_, s)) => *s += v,
                        None => sums[k].values.push((tag, v)),
                    }
                }
            }
        }
    }
    let order = java_hash_order(&sums.iter().map(|s| s.currency.clone()).collect::<Vec<_>>());
    order.iter().filter_map(|c| sums.iter().find(|s| &s.currency == c).cloned()).collect()
}

/// Java's `String.hashCode()`.
fn java_string_hash(s: &str) -> i32 {
    s.encode_utf16().fold(0i32, |h, c| h.wrapping_mul(31).wrapping_add(c as i32))
}

/// The iteration order of a Java `HashMap` / `HashSet` that got `keys` in
/// this order (the reference keeps the ledger currencies and accounts in
/// such maps): buckets by the spread hash, 16 to start, doubled past three
/// quarters full, a bucket in insertion order.
pub(crate) fn java_hash_order(keys: &[String]) -> Vec<String> {
    let spread = |k: &str| { let h = java_string_hash(k) as u32; h ^ (h >> 16) };
    let mut cap = 16usize;
    let mut table: Vec<Vec<(u32, String)>> = vec![Vec::new(); cap];
    let mut size = 0usize;
    for k in keys {
        if table.iter().flatten().any(|(_, e)| e == k) {
            continue;
        }
        let h = spread(k);
        table[h as usize & (cap - 1)].push((h, k.clone()));
        size += 1;
        if size > cap * 3 / 4 {
            let mut grown: Vec<Vec<(u32, String)>> = vec![Vec::new(); cap * 2];
            for bucket in table {
                for (h, e) in bucket {
                    grown[h as usize & (cap * 2 - 1)].push((h, e));
                }
            }
            table = grown;
            cap *= 2;
        }
    }
    table.into_iter().flatten().map(|(_, k)| k).collect()
}

/// An account value as the reference writes it (`jaccount.X.l`: a
/// `DecimalFormat` with two to seven decimals, half-even, no grouping):
/// 933115.0500 gives 933115.05, 953925.6599 stays, 1 gives 1.00.
pub(crate) fn java_account_value(v: f64) -> String {
    let text = format!("{}", v);
    let (sign, digits) = match text.strip_prefix('-') {
        Some(d) => ("-", d),
        None => ("", text.as_str()),
    };
    let (int, frac) = digits.split_once('.').unwrap_or((digits, ""));
    let mut int: Vec<u8> = int.bytes().collect();
    let mut frac: Vec<u8> = frac.bytes().collect();
    if frac.len() > 7 {
        let rest = frac.split_off(7);
        let up = rest[0] > b'5' || (rest[0] == b'5' && (rest[1..].iter().any(|&d| d != b'0')
            || frac.last().is_some_and(|d| (d - b'0') % 2 == 1)));
        if up {
            // One more in the last place, carried into the integer part.
            let mut carry = true;
            for d in frac.iter_mut().rev().chain(int.iter_mut().rev()) {
                if !carry { break; }
                if *d == b'9' { *d = b'0'; } else { *d += 1; carry = false; }
            }
            if carry { int.insert(0, b'1'); }
        }
    }
    while frac.len() > 2 && frac.last() == Some(&b'0') {
        frac.pop();
    }
    while frac.len() < 2 {
        frac.push(b'0');
    }
    format!("{}{}.{}", sign, String::from_utf8(int).unwrap_or_default(), String::from_utf8(frac).unwrap_or_default())
}

/// Account summary rows to send for one request (ibx#479).
#[derive(Clone, Debug)]
pub struct AccountSummaryBatch {
    pub req_id: i64,
    /// The account the rows name: None for this client's account, else
    /// the account of a ledger row, or `All` for the sums of `$LEDGER:ALL`
    /// (ibx#486).
    pub account: Option<String>,
    pub rows: Vec<crate::bridge::AccountRow>,
    /// The server's batch ended: account_summary_end follows the rows.
    pub end: bool,
}

/// The currency of `$LEDGER:ALL` in the set of ledger currencies (ibx#479,
/// ibx#486), as the reference keeps it (`jextend.b2.o()@202-285`):
/// `$LEDGER` adds `BASE`, `$LEDGER:{CCY}` adds `{CCY}`, `$LEDGER:ALL` adds
/// `ALL`. They are one `$LEDGER` item on the wire; the choice stays in the
/// client.
pub const LEDGER_ALL: &str = "ALL";

/// A running account summary request (ibx#479).
#[derive(Clone, Debug)]
pub struct AccountSummaryRequest {
    pub req_id: i64,
    /// Subscription id the server echoes on the rows.
    pub sr_id: String,
    /// The ledger currencies asked (see [`LEDGER_ALL`]), in request order.
    pub ledger: Vec<String>,
    /// The latest ledger row of each account and currency (`jextend.ef`).
    pub ledger_rows: Vec<crate::bridge::LedgerRow>,
}

/// What `req_account_summary` sends: the subscription, and the one it
/// replaces when the request id was already running.
pub struct AccountSummaryPlan {
    pub cancel_sr_id: Option<String>,
    pub sr_id: String,
    pub wire_tags: String,
    pub group: String,
}

/// A running req_positions (ibx#477).
pub struct PositionsSubscription {
    requested_at: std::time::Instant,
    snapshot_sent: bool,
    /// Position and average cost last sent, by conId.
    sent: HashMap<i64, (Qty, Price)>,
    generation: u64,
}

impl PositionsSubscription {
    /// Move the request time back (tests of the 30 s wait).
    #[doc(hidden)]
    pub fn backdate(&mut self, by: std::time::Duration) {
        self.requested_at -= by;
    }
}

impl PositionsSubscription {
    fn new() -> Self {
        Self { requested_at: std::time::Instant::now(), snapshot_sent: false, sent: HashMap::new(), generation: 0 }
    }
}

/// The rows of a reqPositions snapshot in the reference's order (ibx#487):
/// it collects the positions in a hash set and sends them as the set gives
/// them (`jextend.Y.b`, a `HashSet<ia.ca>`; `Y.b()`), so the order is the
/// set's buckets:
/// - hash of a row: `conId ^ Objects.hash(null, spec)` = `conId ^ (961 +
///   spec)`, the spec of a plain row being `account.hashCode()`
///   (`ia.ca.hashCode()@0-26`, `AccountSpec.hashCode()`);
/// - bucket: `(h ^ h >>> 16) & (capacity - 1)`, the capacity 16 doubled
///   while the rows are more than three quarters of it;
/// - the set also holds the rows of the account's model (`Core`), dropped
///   when sent: they count for the capacity. They are taken as one per
///   held position, as the logon's position frames of 07/10/2026 (four
///   plain rows, three model rows).
///
/// Rows of one bucket keep their order here (by conId), where the set has
/// its insertion order. Captured 07/10/2026: AXTI, MSFT, AAPL, SPY.
fn java_position_set_order(rows: &mut [PositionInfo], account: &str) {
    let spec = java_string_hash(account) as u32;
    let entries = rows.len() + rows.iter().filter(|p| p.position_fixed != 0).count();
    let mut capacity: usize = 16;
    while entries * 4 > capacity * 3 {
        capacity *= 2;
    }
    let bucket = |con_id: i64| {
        let h = (con_id as u32) ^ 961u32.wrapping_add(spec);
        ((h ^ (h >> 16)) as usize) & (capacity - 1)
    };
    rows.sort_by_key(|p| bucket(p.con_id));
}

#[cfg(test)]
mod position_order_tests {
    use super::*;

    // ibx#487: the positions of the capture of 07/10/2026 (AAPL 0, MSFT,
    // SPY, AXTI held) in the order of the reference's hash set. With the
    // account of the capture the rule gives the captured order (AXTI, MSFT,
    // AAPL, SPY; checked on the recording machine, the id is not kept
    // here); with this account it gives SPY, MSFT, AAPL, AXTI.
    #[test]
    fn positions_go_in_the_order_of_the_reference_set() {
        let row = |con_id: i64, qty: i64| PositionInfo { con_id, position_fixed: qty * QTY_SCALE, ..Default::default() };
        let mut rows = vec![row(265598, 0), row(272093, -10), row(756733, 81), row(4726868, 1)];
        java_position_set_order(&mut rows, "DU0000001");
        assert_eq!(rows.iter().map(|p| p.con_id).collect::<Vec<_>>(), [756733, 272093, 265598, 4726868]);
    }
}

/// The next rows of a positions subscription (ibx#477 ibx#476): the snapshot
/// and the end once the position data is in; then one row per change of a
/// position or its average cost. Error 2151 when the data is not in after
/// 30 s.
fn advance_positions(sub: &mut PositionsSubscription, shared: &SharedState, account: Option<&str>) -> Option<PositionsBatch> {
    if !sub.snapshot_sent {
        if !shared.portfolio.account_download_complete() {
            if sub.requested_at.elapsed() >= POSITIONS_WAIT {
                return Some(PositionsBatch {
                    rows: Vec::new(), end: false,
                    error: Some((2151, "Positions info is not available yet".into())),
                });
            }
            return None;
        }
        sub.generation = shared.portfolio.position_generation();
        let mut rows = shared.portfolio.position_infos();
        rows.sort_by_key(|p| p.con_id);
        if let Some(account) = account {
            java_position_set_order(&mut rows, account);
        }
        sub.sent = rows.iter().map(|p| (p.con_id, (p.position_fixed, p.avg_cost))).collect();
        sub.snapshot_sent = true;
        return Some(PositionsBatch { rows, end: true, error: None });
    }
    let generation = shared.portfolio.position_generation();
    if generation == sub.generation {
        return None;
    }
    sub.generation = generation;
    let mut rows: Vec<PositionInfo> = shared.portfolio.position_infos().into_iter()
        .filter(|p| sub.sent.get(&p.con_id) != Some(&(p.position_fixed, p.avg_cost)))
        .collect();
    rows.sort_by_key(|p| p.con_id);
    for p in &rows {
        sub.sent.insert(p.con_id, (p.position_fixed, p.avg_cost));
    }
    (!rows.is_empty()).then_some(PositionsBatch { rows, end: false, error: None })
}

/// A running req_positions_multi (ibx#476).
pub struct PositionsMultiSubscription {
    pub req_id: i64,
    pub account: String,
    pub model_code: String,
    sub: PositionsSubscription,
}

/// A running req_account_updates_multi (ibx#476).
pub struct AccountMultiSubscription {
    pub req_id: i64,
    pub account: String,
    pub model_code: String,
    /// ledgerAndNLV: the ledger rows only.
    ledger_only: bool,
    image_sent: bool,
    sent: HashMap<(String, String), String>,
    generation: u64,
}

/// Rows of one req_account_updates_multi (ibx#476).
pub struct AccountMultiBatch {
    pub req_id: i64,
    pub account: String,
    pub model_code: String,
    pub rows: Vec<crate::bridge::AccountRow>,
    /// The snapshot: account_update_multi_end follows the rows.
    pub end: bool,
}

/// Position rows to send for req_positions (ibx#477).
pub struct PositionsBatch {
    pub rows: Vec<PositionInfo>,
    /// The snapshot: position_end follows the rows.
    pub end: bool,
    /// The position data never came: error(-1, code, message), no end.
    pub error: Option<(i64, String)>,
}

/// How long req_positions waits for the position data before error 2151,
/// as the reference (ibx#477).
pub const POSITIONS_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// Most account summary requests per client (ibx#479).
pub const ACCOUNT_SUMMARY_MAX: usize = 2;

/// The reference's refusals of an account summary request (ibx#479).
fn summary_refusal(cause: &str) -> (i64, String) {
    (321, format!("Error validating request.-'b2' : cause - {}", cause))
}

/// A single portfolio position update.
pub struct PortfolioUpdateEntry {
    pub con_id: i64,
    pub position: f64,
    pub avg_cost: f64,
    pub market_price: f64,
    pub market_value: f64,
    pub unrealized_pnl: f64,
    pub realized_pnl: f64,
}

/// True when `status` names an IB order state that is still working on the broker.
/// Whitelist (rather than blacklist) so non-canonical or empty strings — and
/// any future terminal states added by IB — are treated as "not open".
#[inline]
pub fn is_open_status(status: &str) -> bool {
    matches!(
        status,
        "ApiPending"
            | "PendingSubmit"
            | "PendingCancel"
            | "PreSubmitted"
            | "Submitted"
            | "PartiallyFilled"
    )
}

/// The average, last fill and market cap prices of an orderStatus: an
/// order that never left (ApiCancelled) has them unset, as the reference
/// writes them (`jextend.dL.a(Collection, String)`; scenario
/// i105_combo_directed of 26/09/2026: MAX, MAX, MAX).
pub fn status_prices(r: &OrderReport) -> (f64, f64, f64) {
    if r.status == "ApiCancelled" {
        (f64::MAX, f64::MAX, f64::MAX)
    } else {
        (r.avg_fill_price, r.last_fill_price, 0.0)
    }
}

/// Convert OrderStatus enum to ibapi-compatible string.
#[inline]
pub fn order_status_str(status: OrderStatus) -> &'static str {
    match status {
        OrderStatus::PendingSubmit => "PendingSubmit",
        OrderStatus::PreSubmitted => "PreSubmitted",
        OrderStatus::Submitted => "Submitted",
        OrderStatus::PendingCancel => "PendingCancel",
        OrderStatus::PendingReplace => "PendingCancel", // IB API has no PendingReplace string
        OrderStatus::Filled => "Filled",
        // The reference has no partially-filled status: an order with part
        // of it filled is still working (ib-agent#192 C8).
        OrderStatus::PartiallyFilled => "Submitted",
        OrderStatus::Cancelled => "Cancelled",
        // ibapi has no "Rejected" status string — rejected orders surface as "Inactive"
        // with the rejection reason carried separately on OrderState.completedStatus.
        OrderStatus::Rejected => "Inactive",
        OrderStatus::Inactive => "Inactive",
        OrderStatus::ApiCancelled => "ApiCancelled",
    }
}

// ── Execution storage ──

/// Buy or sell from an API side string: `BUY` / `BOT` buy, `SELL` / `SLD` /
/// `SSHORT` sell. `None` for anything else.
fn side_is_buy(side: &str) -> Option<bool> {
    match side.to_ascii_uppercase().as_str() {
        "BUY" | "BOT" => Some(true),
        "SELL" | "SLD" | "SSHORT" => Some(false),
        _ => None,
    }
}

/// An execution time as the reference writes it in execDetails:
/// `yyyyMMdd HH:mm:ss US/Eastern` (ibx#474; the reference uses the zone of
/// its API time setting, US/Eastern in the captures). UTC when the zone
/// database has no US/Eastern.
pub fn format_exec_time(unix_secs: i64) -> String {
    let Ok(ts) = jiff::Timestamp::from_second(unix_secs) else {
        return String::new();
    };
    match jiff::tz::TimeZone::get("US/Eastern") {
        Ok(tz) => format!("{} US/Eastern", ts.to_zoned(tz).strftime("%Y%m%d %H:%M:%S")),
        Err(_) => format!("{} UTC", ts.to_zoned(jiff::tz::TimeZone::UTC).strftime("%Y%m%d %H:%M:%S")),
    }
}

/// A stored execution and its commission report for `req_executions` replay.
/// Shared between Rust and Python adapters via `ClientCore`. The report
/// comes from its own server frame after the fill; `None` until then (ibx#471).
#[derive(Clone)]
pub struct StoredExecution {
    pub req_id: i64,
    pub contract: ApiContract,
    pub execution: ApiExecution,
    /// Execution time, Unix seconds, for the time filter of `req_executions`.
    pub time_secs: Option<i64>,
    pub commission_and_fees: Option<ApiCommissionAndFeesReport>,
}

// ── Order tracking ──

/// An open-order request: `req_open_orders` or `req_all_open_orders`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenOrdersRequest {
    Open,
    All,
}

/// The last openOrder and orderStatus given for an order: the reference
/// gives them again when the commission of one of its executions comes
/// (ibx#486).
#[derive(Clone)]
pub struct OrderReport {
    /// None: no openOrder (a cancel).
    pub view: Option<OrderView>,
    pub status: String,
    pub filled: f64,
    pub remaining: f64,
    pub avg_fill_price: f64,
    pub perm_id: i64,
    pub parent_id: i64,
    pub last_fill_price: f64,
    pub client_id: i64,
    /// orderStatus whyHeld ([`ClientCore::why_held`]).
    pub why_held: String,
}

/// A locally tracked order for `req_open_orders` / dispatch status updates.
#[derive(Clone)]
pub struct TrackedOrder {
    pub contract: ApiContract,
    pub order: ApiOrder,
    pub status: String,
    pub filled: f64,
    pub remaining: f64,
    pub instrument: InstrumentId,
    /// Price of the order's last print; 0 before any fill (ibx#473).
    pub last_fill_price: f64,
}

/// What the client reports for an order in `open_order` / `order_status`
/// after a server report (ibx#473).
#[derive(Clone)]
pub struct OrderView {
    pub contract: ApiContract,
    pub order: ApiOrder,
    pub state: ApiOrderState,
    pub last_fill_price: f64,
    /// This client's id for an order it placed; 0 otherwise.
    pub client_id: i64,
}

/// Text of error 10027, a smartComboRoutingParams name the reference does
/// not know (ibx#470; one space after each comma, as the message table
/// `Invalid_combo_routing_tag_api`, ibx#485).
pub const INVALID_COMBO_ROUTING_TAG: &str = "Invalid combo routing tag. Valid tags are LeginPrio, MaxSegSize, \
    DontLeginNext, ChangeToMktTime1, ChangeToMktTime2, ChangeToMktOffset, DiscretionaryPct, NonGuaranteed, \
    CondPriceMin, CondPriceMax, and PriceCondConid.";

/// The order attribute tag of a smartComboRoutingParams name
/// (`jattrib.Attributes.f`, ibx#470); None for a name the reference does
/// not know.
fn combo_routing_tag(name: &str) -> Option<u32> {
    Some(match name {
        "LeginPrio" => 6851,
        "MaxSegSize" => 6852,
        "DontLeginNext" => 6867,
        "ChangeToMktTime1" => 6860,
        "ChangeToMktTime2" => 6861,
        "ChangeToMktOffset" => 6866,
        "DiscretionaryPct" => 6862,
        "PriceCondConid" => 6876,
        "CondPriceMax" => 6878,
        "CondPriceMin" => 6877,
        "NonGuaranteed" => 6248,
        _ => return None,
    })
}

fn gcd(a: u32, b: u32) -> u32 {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// The reference's answer to a place or modify on an order id that is no
/// longer working: filled, cancelled, or with a cancel pending. Nothing is
/// sent (ibx#463; captured 25/09/2026).
pub const MODIFY_OF_FINISHED_ORDER: (i64, &str) = (104, "Cannot modify a filled order.");

/// A new order under the id of an order of this client that is working (ibx#538).
pub const DUPLICATE_ORDER_ID: (i64, &str) = (103, "Duplicate order id");

/// The reference's refusal of a fractional quantity (ib-agent#192 B3).
pub const FRACTIONAL_VIA_API: (i64, &str) = (10243,
    "Fractional-sized order cannot be placed via API. Please use desktop version to place this order.");

/// Most commission reports kept while waiting for their execution.
pub const PENDING_COMMISSIONS_MAX: usize = 1024;

/// Commission reports waiting for their execution, oldest dropped first
/// past `PENDING_COMMISSIONS_MAX` (ibx#471).
#[derive(Default)]
pub struct PendingCommissions {
    reports: HashMap<String, (u64, ApiCommissionAndFeesReport)>,
    /// (insertion number, key), oldest at the front. An entry whose number
    /// no longer matches `reports` was removed or replaced, and is skipped.
    order: std::collections::VecDeque<(u64, String)>,
    next: u64,
}

impl PendingCommissions {
    pub fn insert(&mut self, key: String, report: ApiCommissionAndFeesReport) {
        let seq = self.next;
        self.next += 1;
        self.order.push_back((seq, key.clone()));
        self.reports.insert(key, (seq, report));
        while self.order.len() > PENDING_COMMISSIONS_MAX {
            if let Some((old_seq, old_key)) = self.order.pop_front() {
                if self.reports.get(&old_key).is_some_and(|(s, _)| *s == old_seq) {
                    self.reports.remove(&old_key);
                }
            }
        }
    }

    pub fn remove(&mut self, key: &str) -> Option<ApiCommissionAndFeesReport> {
        self.reports.remove(key).map(|(_, report)| report)
    }

    pub fn len(&self) -> usize {
        self.reports.len()
    }

    pub fn is_empty(&self) -> bool {
        self.reports.is_empty()
    }

    pub fn clear(&mut self) {
        self.reports.clear();
        self.order.clear();
    }
}

/// What `place_order` does with an order id that is already working (ibx#247).
pub enum ModifyPlan {
    /// Send this replace.
    Send(ControlCommand),
    /// Refused before sending, as the reference does; report it through `error()`.
    Refused { code: i64, message: String },
}

/// A market data request joins a contract that runs on delayed data while
/// this client has not enabled delayed data (ibx#444,
/// `jextend.s.a(dy,ec,Set)@174`).
pub const MD_DELAYED_NOT_ENABLED: (i64, &str) =
    (10168, "Requested market data is not subscribed. Delayed market data is not enabled");

/// The top of book of a request with the news tick was rejected; its news
/// goes on (ibx#444, `jextend.ba.a(String,List,boolean,long)@234-292`):
/// the ticks still observed, then "; ". Whatever the reference appends
/// after that from the reject's text is not known.
pub const MD_TOP_REJECTED: (i64, &str) = (2117,
    "Requested top market data is not subscribed. Subscription-independent ticks are still active.292; ");

/// Headlines kept per instrument for the requests that join it, and how
/// many of the latest a joining request gets, as the reference replays
/// them (ibx#444, `jextend.s.a(dy,generictick.b,ArString)@163-208`).
const NEWS_KEPT: usize = 64;
const NEWS_REPLAYED: usize = 5;

/// A news provider key (codes, comma separated; empty for none named)
/// has this provider.
fn news_key_covers(key: &str, provider: &str) -> bool {
    key.is_empty() || key.split(',').any(|code| code.eq_ignore_ascii_case(provider))
}

/// The slot of a tick-by-tick request whose contract is being looked up.
pub const NO_SLOT: InstrumentId = InstrumentId::MAX;

/// The generic ticks of a market data request (ibx#450): request codes,
/// and the exchange and security type it was asked with.
#[derive(Debug, Clone)]
pub struct MdGeneric {
    pub codes: Vec<i32>,
    pub exchange: String,
    pub sec_type: String,
}

/// What the engine is told when a market data request ends (ibx#444).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MdCancel {
    /// The last request of the instrument: its subscription ends.
    Instrument(InstrumentId),
    /// Other requests still use the instrument: only this request's share
    /// of its news entry goes, when it had the news tick, and of its
    /// generic tick entries (ibx#450).
    Shared { instrument: InstrumentId, news: Option<String>, generic: Vec<i32> },
    /// A request waiting for a market data line (101): nothing was sent.
    Waiting,
}

impl MdCancel {
    /// The commands for the engine.
    pub fn commands(self) -> Vec<ControlCommand> {
        match self {
            MdCancel::Instrument(instrument) => vec![ControlCommand::Unsubscribe { instrument }],
            MdCancel::Shared { instrument, news, generic } => {
                let mut out: Vec<ControlCommand> = Vec::new();
                if !generic.is_empty() {
                    out.push(ControlCommand::UnsubscribeGeneric { instrument, codes: generic });
                }
                if let Some(providers) = news {
                    out.push(ControlCommand::UnsubscribeNews { instrument, providers });
                }
                out
            }
            MdCancel::Waiting => Vec::new(),
        }
    }
}

/// A streaming market data request past the API ticker limit (101): it
/// waits for a line, as the reference's subscriber stays on a record the
/// line manager leaves unsubscribed (ibx#444).
#[derive(Debug, Clone)]
pub struct MdWaiting {
    pub req_id: i64,
    pub con_id: i64,
    pub symbol: String,
    pub exchange: String,
    pub sec_type: String,
    pub currency: String,
    pub filters: crate::types::SecDefFilters,
    pub generic_tick_list: String,
    pub mode_9887: i32,
}

/// Error 101 of the reference (`jextend.d7.e`).
pub const MD_MAX_TICKERS: (i64, &str) = (101, "Max number of tickers has been reached");

/// The text of 354 for a refused top of book (captured 05/10/2026), before
/// what is appended to it.
pub const MD_NOT_SUBSCRIBED: &str = "Requested market data is not subscribed. Check API status by selecting the Account menu \
    then under Management choose Market Data Subscription Manager and/or availability of delayed data.";

/// Error 10197 of the reference, as captured (02/10/2026, 07/10/2026): on
/// a paper session, a streaming request that ended with a refused news
/// tick (10094) gets it `MD_NO_DATA_WAIT` later, unless a request of its
/// contract runs then.
pub const MD_NO_DATA: (i64, &str) = (10197, "No market data during competing live session");
pub const MD_NO_DATA_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// A callback for a market data request beside its ticks (ibx#444).
#[derive(Debug, Clone, PartialEq)]
pub enum MdNotice {
    MarketDataType { req_id: i64, market_data_type: i32 },
    TickReqParams { req_id: i64, min_tick: f64, bbo_exchange: String, permissions: i64 },
    Error { req_id: i64, code: i64, text: String },
    News { req_id: i64, news: crate::types::TickNews },
}

// ── ClientCore ──

/// Shared subscription tracking and dispatch preparation logic.
///
/// Both Rust and Python EClient own a `ClientCore` and delegate state tracking
/// and data preparation to it. Only the final callback invocation is language-specific.
pub struct ClientCore {
    // reqId <-> InstrumentId mapping. Several requests may share one
    // instrument, in the order they came, as the reference's API
    // subscribers share a contract's record (ibx#444).
    pub req_to_instrument: Mutex<HashMap<i64, InstrumentId>>,
    pub instrument_to_req: Mutex<HashMap<InstrumentId, Vec<i64>>>,
    // con_id → InstrumentId for find_or_register_instrument lookup
    pub con_id_to_instrument: Mutex<HashMap<i64, InstrumentId>>,
    /// What each market data request sent last and where it reads the
    /// market data queue, by request id (ibx#446).
    pub last_quotes: Mutex<HashMap<i64, StreamState>>,
    /// The instruments as the queued steps have them (ibx#446).
    md_reader: Mutex<md_stream::MdReader>,
    /// Requests that joined a quote and wait for their first step.
    md_joins_waiting: std::sync::atomic::AtomicUsize,
    /// The dispatch's list of market data callbacks, kept between calls.
    pub md_out: Mutex<Vec<MdOut>>,
    /// The news provider key of each market data request with the news
    /// tick (ibx#444, ibx#458).
    pub md_news: Mutex<HashMap<i64, String>>,
    /// The market data requests whose generic tick list has `mdoff`: no
    /// top of book ticks (ibx#444).
    pub md_top_off: Mutex<HashSet<i64>>,
    /// The generic ticks of each market data request (ibx#450): a request
    /// gets the API ticks of those only.
    pub md_generic: Mutex<HashMap<i64, MdGeneric>>,
    /// Streaming requests that got 101 and wait for a line, oldest first
    /// (ibx#444).
    pub md_waiting: Mutex<Vec<MdWaiting>>,
    /// Contracts the server refused with "delayed market data available"
    /// (354): the reference's record keeps "not subscribed, delayed
    /// available", so a new request gets 10168 or goes delayed at once
    /// (ibx#444).
    pub md_delayed_known: Mutex<HashSet<i64>>,
    /// The contracts whose data is frozen (ibx#447): real-time slot ->
    /// frozen slot, where their requests are while it lasts.
    pub md_frozen: Mutex<HashMap<InstrumentId, InstrumentId>>,
    /// The last request parameters of each instrument (minimum tick, BBO
    /// exchange, snapshot permissions): a request that joins gets them at
    /// once (ibx#444).
    pub instrument_params: Mutex<HashMap<InstrumentId, (f64, String, i64)>>,
    /// Headlines given for each instrument, oldest first, at most
    /// `NEWS_KEPT`: a request that joins gets the last ones (ibx#444).
    pub instrument_news: Mutex<HashMap<InstrumentId, Vec<crate::types::TickNews>>>,
    /// Requests that joined a running subscription, and whether on delayed
    /// data, to be answered at the next dispatch (ibx#444).
    pub md_joins: Mutex<Vec<(i64, bool)>>,
    /// Requests of a paper session that ended with a refused news tick:
    /// (reqId, when its contract is looked at, conId), oldest first
    /// (10197, ibx#444); their count is read without the lock.
    md_no_data: Mutex<Vec<(i64, std::time::Instant, i64)>>,
    /// The conId the engine found for the slots of requests sent without
    /// one (ibx#444).
    md_slot_con_ids: Mutex<HashMap<InstrumentId, i64>>,
    md_no_data_count: std::sync::atomic::AtomicUsize,
    /// The client's market data modes from reqMarketDataType (ibx#444).
    pub md_modes: Mutex<crate::types::MarketDataModes>,
    /// The "Legal ones" text of the generic tick list refusal: computed
    /// once, for the first refused list, as the reference caches it; never
    /// reset (ibx#450).
    generic_legal: Mutex<Option<String>>,
    /// Running plain snapshots by request id (ibx#446), their count read
    /// without the lock, and the per-second snapshot limiter.
    snapshot_reqs: Mutex<HashMap<i64, crate::control::snapshot::PlainSnapshot>>,
    snapshot_count: std::sync::atomic::AtomicUsize,
    snapshot_rate: Mutex<crate::control::snapshot::RateLimiter>,
    /// Running regulatory snapshots (ibx#446) and the acknowledgements of
    /// their instruments (permission, BBO exchange code).
    pub reg_snapshots: Mutex<Vec<crate::control::regsnapshot::Fetch>>,
    pub reg_snapshot_acks: Mutex<HashMap<InstrumentId, (i32, String)>>,
    /// reqSmartComponents waiting for the exchange map of their BBO
    /// exchange (ibx#441): (reqId, code, security type id, deadline).
    smart_components_waiting: Mutex<Vec<SmartComponentsWait>>,

    // PnL subscription state
    /// Running req_pnl requests, with the last values sent (ibx#478).
    pub pnl_reqs: Mutex<HashMap<i64, [i64; 3]>>,
    pub pnl_quotes: Mutex<PnlQuotes>,
    /// Contract currency already sent to the engine, by conId (ibx#466).
    pub currency_sent: Mutex<HashMap<i64, String>>,
    pub pnl_single_reqs: Mutex<HashMap<i64, i64>>, // req_id → con_id
    // Per-req_id change detection for pnl_single: [pos, daily, unrealized, realized, value] scaled.
    pub last_pnl_single: Mutex<HashMap<i64, [i64; 5]>>,

    // Account summary subscription state (req_id, tags)
    pub account_summaries: Mutex<Vec<AccountSummaryRequest>>,
    pub positions_sub: Mutex<Option<PositionsSubscription>>,
    pub positions_multi: Mutex<Vec<PositionsMultiSubscription>>,
    pub account_multi: Mutex<Vec<AccountMultiSubscription>>,
    pub next_account_summary: AtomicU64,

    // News bulletin subscription
    pub bulletin_subscribed: AtomicBool,
    /// The stored bulletins are to be replayed at the next dispatch
    /// (ibx#461).
    pub bulletin_replay: AtomicBool,

    /// Request ids subscribed to display group events (ibx#424).
    pub display_group_subs: Mutex<HashSet<i64>>,

    // Account updates subscription
    pub account_updates_subscribed: AtomicBool,
    pub account_stream: Mutex<AccountStream>,
    pub last_portfolio: Mutex<Option<Vec<PositionInfo>>>,

    // Execution replay store
    pub executions: Mutex<Vec<StoredExecution>>,
    // The API client id given at connect (0 in the Rust client, which has
    // none): the clientId of this client's executions (ibx#474).
    pub client_id: AtomicI64,
    /// Orders that went out without outside RTH (warning 2109): their
    /// openOrder shows it off, as the reference's order record (ibx#486).
    rth_dropped: Mutex<HashSet<OrderId>>,
    /// Orders the redirect precaution discarded (10311, 10329), until
    /// their Cancelled status (ibx#486).
    discarded: Mutex<HashSet<OrderId>>,
    /// The last report given for each order (ibx#486).
    last_reports: Mutex<HashMap<OrderId, OrderReport>>,
    // Commission reports that came before their execution, by execution id
    // without its revision (ibx#471). Bounded: reports for executions this
    // client never sees (earlier sessions, other clients) are dropped oldest
    // first.
    pub pending_commissions: Mutex<PendingCommissions>,

    // Open order tracking
    pub open_orders: Mutex<HashMap<OrderId, TrackedOrder>>,
    /// Open-order requests made while the auth link was lost, answered at
    /// the end of the order replay; one per kind, as the reference keeps
    /// one per request kind and client (ibx#251).
    pub held_open_orders: Mutex<Vec<OpenOrdersRequest>>,
    /// What-if previews waiting for their answer, by order id, oldest
    /// first: the contract and order the answer reports (ibx#462). Kept
    /// apart from the open orders.
    pub what_if_orders: Mutex<HashMap<OrderId, std::collections::VecDeque<(ApiContract, ApiOrder)>>>,
    // Ids of tracked orders that were filled or cancelled: never sent again (ibx#463).
    pub finished_orders: Mutex<HashSet<OrderId>>,
    /// Ids of tracked orders placed again under the same id (a modify):
    /// their openOrder shows no derived trail stop price (ibx#521).
    pub modified_orders: Mutex<HashSet<OrderId>>,
    /// Executions of other clients' orders, by execution id without its
    /// revision: their commission reports are not given.
    pub silent_executions: Mutex<HashSet<String>>,
    /// The highest order id this client placed (orders and what-ifs), as
    /// the reference records it for the client when an order goes on
    /// (`jextend.bH.Z()@78`, `jextend.H.c(int)`): a new order at or below
    /// it is refused with 103 (ibx#462). Shared with the thread that keeps
    /// it on disk (ibx#518).
    pub highest_order_id: Arc<AtomicI64>,
    /// Keeps `highest_order_id` on disk while the client is connected, as
    /// the reference keeps it in its saved settings (ibx#518).
    pub order_id_saver: Mutex<Option<crate::order_ids::Saver>>,
    /// The next order id `take_order_id` hands out, at least the next
    /// valid id.
    pub reserved_order_id: AtomicI64,

    // Market data type callback tracking
    pub market_data_type: AtomicI32,
    pub mdt_sent: Mutex<HashSet<i64>>,
    /// Market data requests that switched to delayed data (ibx#447).
    pub delayed_reqs: Mutex<HashSet<i64>>,
    /// Market data requests whose tickReqParams was sent (ibx#449).
    pub tick_req_params_sent: Mutex<HashSet<i64>>,
    /// Tick-by-tick requests: reqId -> (instrument, conId, type). Kept apart
    /// from the market data maps, so both can run on one contract (ibx#455).
    pub tbt_reqs: Mutex<HashMap<i64, (InstrumentId, i64, TbtType)>>,


    // Contract cache for enrichment
    pub contract_cache: Mutex<HashMap<i64, ApiContract>>,
}

/// The delayed tick type of a real-time one, from the API tick type table
/// (bid 1 -> 66, ask 2 -> 67, last 4 -> 68, sizes 0/3/5 -> 69/70/71, high
/// 6 -> 72, low 7 -> 73, volume 8 -> 74, close 9 -> 75, open 14 -> 76, last
/// time 45 -> 88, halted 49 -> 90); others unchanged (ibx#447). The
/// reference's delayed sender writes 88 and 90 where the real-time one
/// writes 45 and 49 (ibx#446, `jextend.dL.b(List, s, int, pa, dy, o,
/// SnapshotPreference, Set)@2165-2524`).
pub fn delayed_tick_type(tick_type: i32) -> i32 {
    match tick_type {
        1 => 66, 2 => 67, 4 => 68, 0 => 69, 3 => 70, 5 => 71,
        6 => 72, 7 => 73, 8 => 74, 9 => 75, 14 => 76,
        45 => 88, 49 => 90,
        other => other,
    }
}

/// The news source of the WSH requests (ibx#443).
const WSH_SOURCE: &str = "WSHE";

/// A WSH request when its news source is not in the logon's list: the
/// reference's error (ibx#443).
pub const WSH_NOT_ALLOWED: (i64, &str) = (10276, "News feed is not allowed.");

/// A WSH request when its news source is listed but not subscribed: the
/// reference's error (ibx#443).
pub const WSH_NOT_SUBSCRIBED: (i64, &str) =
    (10277, "News Feed requires permissions. Please login to Portal to subscribe.");

/// reqWshEventData while no meta data is held: the reference's error
/// (ibx#443).
pub const WSH_META_NOT_REQUESTED: (i64, &str) = (10282, "WSH meta data not requested.");

/// reqWshMetaData that cannot be served: the reference's error for a
/// failed request, with the reason after it (ibx#443).
pub const WSH_META_FAILED: (i64, &str) =
    (10279, "Failed to request WSH meta data.The request is not supported.");

/// The permission check of the WSH requests, as the reference (ibx#443):
/// the error of a session whose logon does not list the WSH news source,
/// or lists it without a subscription; `None` when it is subscribed.
pub(crate) fn wsh_refusal(subscribed: &[String], unsubscribed: &[String]) -> Option<(i64, &'static str)> {
    let has = |codes: &[String]| codes.iter().any(|c| c.eq_ignore_ascii_case(WSH_SOURCE));
    if has(subscribed) {
        None
    } else if has(unsubscribed) {
        Some(WSH_NOT_SUBSCRIBED)
    } else {
        Some(WSH_NOT_ALLOWED)
    }
}

/// The answer of reqWshMetaData (ibx#443): the permission error, as the
/// reference; with the permission, the error of a failed request, since
/// the data request itself is not implemented.
pub(crate) fn wsh_meta_data_error(reference: &crate::bridge::ReferenceState) -> (i64, &'static str) {
    wsh_refusal(&reference.news_sources(), &reference.news_sources_unsubscribed()).unwrap_or(WSH_META_FAILED)
}

/// The answer of reqWshEventData (ibx#443): the permission error, as the
/// reference; with the permission, the error of a request made before
/// any meta data is held, which is always the case here.
pub(crate) fn wsh_event_data_error(reference: &crate::bridge::ReferenceState) -> (i64, &'static str) {
    wsh_refusal(&reference.news_sources(), &reference.news_sources_unsubscribed()).unwrap_or(WSH_META_NOT_REQUESTED)
}

/// requestFA on a session that is not FA: the reference's error, with its
/// request id for a request that has none (ibx#481).
pub const REQUEST_FA_NOT_FA: (i64, i64, &str) =
    (2147483647, 321, "Error validating request.-'b9' : cause - FA data operations ignored for non FA customers.");

/// replaceFA on a session that is not FA: the reference's error code and
/// text; the id is the request's (ibx#481).
pub const REPLACE_FA_NOT_FA: (i64, &str) =
    (321, "Error validating request.-'b1' : cause - FA data operations ignored for non FA customers.");

/// The display groups, as the reference lists them with no window open
/// (ibx#424).
pub const DISPLAY_GROUP_LIST: &str = "1|2|3|4|5|6|7";

/// The contract of a display group: no group has one (ibx#424).
pub const DISPLAY_GROUP_NO_CONTRACT: &str = "none";

/// Id and code of a refused display group request: the reference gives
/// the request id in the text only (ibx#424).
pub const DISPLAY_GROUP_REFUSAL: (i64, i64) = (-1, 321);

/// Error of a display group update whose conId is not a contract
/// (ibx#424).
pub const DISPLAY_GROUP_NO_INSTRUMENT: (i32, &str) = (473, "No Financial Instrument defined");

/// What a display group update asks of the caller (ibx#424).
#[derive(Debug, Clone, PartialEq)]
pub enum DisplayGroupUpdate {
    /// Accepted; the reference changes nothing and sends nothing.
    Nothing,
    /// Refused: the error text, for `DISPLAY_GROUP_REFUSAL`.
    Refused(String),
    /// Accepted, with a conId not seen yet: it is looked up, and one that
    /// is not a contract gets `DISPLAY_GROUP_NO_INSTRUMENT`.
    Lookup(i64),
}

fn display_group_refusal(class: &str, cause: String) -> String {
    format!("Error validating request.-'{class}' : cause - {cause}")
}

/// The reference's other names for order types ibx supports, and the name
/// ibx uses (ibx#469, from the reference's order-type map).
const ORDER_TYPE_ALIASES: [(&str, &str); 15] = [
    ("LIMIT", "LMT"),
    ("MARKET", "MKT"),
    ("STOP", "STP"),
    ("STPLMT", "STP LMT"),
    ("STOP LIMIT", "STP LMT"),
    ("MKT CLS", "MOC"),
    ("LMT CLS", "LOC"),
    ("MKT TO LMT", "MTL"),
    ("RELATIVE", "REL"),
    ("PEG PRIM", "REL"),
    ("PEGMKT", "PEG MKT"),
    ("PEGMID", "PEG MID"),
    ("PEGBENCH", "PEG BENCH"),
    ("TRAILING STOP", "TRAIL"),
    ("TRAILLMT", "TRAIL LIMIT"),
];

/// The reference's text for an invalid date or time (errors 337 and 343);
/// %s is the field's label.
pub(crate) const INVALID_DATE_TIME: &str = "%s: The date, time, or time-zone entered is invalid.\n\
The correct format is yyyymmdd hh:mm:ss xx/xxxx\n\
where yyyymmdd and xx/xxxx are optional.\n\
E.g.: 20031126 15:59:00 US/Eastern\n\
\n\
Note that there is a space between the date and time,\n\
and between the time and time-zone.\n\
\n\
If no date is specified, current date is assumed.\n\
If no time-zone is specified, local time-zone is assumed(deprecated).\n\
\n\
You can also provide yyyymmddd-hh:mm:ss time is in UTC.\n\
Note that there is a dash between the date and time in UTC notation.";

/// The reference's warning 2174: a date and time given with no zone, read
/// in the zone of the machine (ibx#416, captured 02/10/2026).
pub const IMPLIED_TIME_ZONE: &str = "Warning: You submitted request with date-time attributes without explicit time zone. \
Please switch to use yyyymmdd-hh:mm:ss in UTC or use instrument time zone, like US/Eastern. \
Implied time zone functionality will be removed in the next API release";

/// The time of a time condition as the reference reads it (ibx#416).
#[derive(Debug, Clone, PartialEq)]
pub struct ConditionTime {
    /// The time the condition carries: the API text unchanged for the UTC
    /// form, else the instant in UTC as `yyyyMMdd-HH:mm:ss`.
    pub wire: String,
    /// The zone id the time was read in: UTC for the UTC form, the zone
    /// as written, or the machine's zone when none is written.
    pub zone: String,
    /// No zone was written: the reference warns with 2174.
    pub implied_zone: bool,
}

/// The API's UTC form `yyyyMMdd-HH:mm:ss`, as the reference's strict
/// parse takes it (`jextend.dX.a(String)`): the text unchanged, no space.
fn is_utc_dash_form(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 17 || b[8] != b'-' || b[11] != b':' || b[14] != b':' {
        return false;
    }
    let num = |from: usize, to: usize| -> Option<u32> {
        b[from..to].iter().all(u8::is_ascii_digit).then(|| s[from..to].parse().unwrap_or(u32::MAX))
    };
    match (num(0, 4), num(4, 6), num(6, 8), num(9, 11), num(12, 14), num(15, 17)) {
        (Some(_), Some(mo), Some(d), Some(h), Some(mi), Some(se)) =>
            (1..=12).contains(&mo) && (1..=31).contains(&d) && h <= 23 && mi <= 59 && se <= 59,
        _ => false,
    }
}

/// A time condition's time as the reference reads it (ibx#416,
/// `jextend.dX.b(String)`): the UTC form `yyyyMMdd-HH:mm:ss` is kept as
/// given; `[yyyyMMdd ]HH:mm:ss[ zone]` is read in its zone (the machine's
/// when none is written, today's date when none is given) and sent as the
/// instant in UTC. A day past the end of its month runs into the next
/// month, as the reference's calendar does. None when the reference
/// cannot read it (error 10314).
pub fn parse_condition_time(s: &str, machine_zone: &str) -> Option<ConditionTime> {
    if is_utc_dash_form(s) {
        return Some(ConditionTime { wire: s.to_string(), zone: "UTC".into(), implied_zone: false });
    }
    if s.trim().is_empty() || !crate::control::historical::is_valid_end_date(s) {
        return None;
    }
    let words: Vec<&str> = s.split(' ').filter(|w| !w.is_empty()).collect();
    let (date, time, zone_words) = if words.first()?.contains(':') {
        (None, words[0], &words[1..])
    } else {
        (Some(words[0]), *words.get(1)?, &words[2..])
    };
    let implied_zone = zone_words.is_empty();
    let zone = if implied_zone { machine_zone.to_string() } else { zone_words.join(" ") };
    let tz = jiff::tz::TimeZone::get(crate::config::canonical_zone(&zone)).ok()?;
    let day = match date {
        Some(d) => {
            let (y, m, day): (i16, i8, i64) = (d[0..4].parse().ok()?, d[4..6].parse().ok()?, d[6..8].parse().ok()?);
            jiff::civil::Date::new(y, m, 1).ok()?.checked_add(jiff::Span::new().days(day - 1)).ok()?
        }
        None => jiff::Zoned::now().with_time_zone(tz.clone()).date(),
    };
    let hms: Vec<i8> = time.split(':').map(|p| p.parse().ok()).collect::<Option<_>>()?;
    let at = day.to_datetime(jiff::civil::Time::new(hms[0], hms[1], *hms.get(2).unwrap_or(&0), 0).ok()?);
    let instant = at.to_zoned(tz).ok()?.timestamp();
    let wire = instant.to_zoned(jiff::tz::TimeZone::UTC).strftime("%Y%m%d-%H:%M:%S").to_string();
    Some(ConditionTime { wire, zone, implied_zone })
}

/// Error 110 and its text: a price off the contract's price grid, or not
/// a number (`jextend.d7.l`, ibx#263).
pub(crate) const PRICE_VARIATION: (i64, &str) = (110, "The price does not conform to the minimum price variation for this contract.");

/// The API's overnight time-in-force values (ibx#467).
const TIF_OVERNIGHT: &str = "OVERNIGHT";
const TIF_OVERNIGHT_DAY: &str = "OVERNIGHT + DAY";

/// An API price field with its unset value (the maximum double) read as 0.
fn aux_or_zero(v: f64) -> f64 {
    if v == f64::MAX { 0.0 } else { v }
}

/// An API price as the engine's fixed-point price, unset (the maximum
/// double, the official API's default) read as 0.
fn price_or_zero(v: f64) -> Price {
    crate::api::types::price_from_f64(aux_or_zero(v))
}

/// Whether an API double is given: neither 0 nor the official API's unset
/// value (the maximum double).
fn is_given(v: f64) -> bool {
    v != 0.0 && v != f64::MAX
}

/// A TRAIL LIMIT's limit price, offset and stop price are what the server
/// reports (44, 6370, 6117), as the reference's openOrder (ib-agent#194,
/// ibx#491).
/// usePriceMgmtAlgo as openOrder reports it (ibx#492): 0 or 1, never
/// unset, as the reference; the value the server's report gives, else the
/// caller's, unset read as 0.
fn reported_price_mgmt(order: &mut ApiOrder, reported: Option<&ApiOrder>) {
    order.use_price_mgmt_algo = match reported {
        Some(r) => r.use_price_mgmt_algo,
        None => i32::from(order.use_price_mgmt_algo != i32::MAX && order.use_price_mgmt_algo != 0),
    };
}

/// The values the reference's openOrder shows for order fields the API
/// leaves unset (captured 26/09/2026 to 02/10/2026, the four-leg events:
/// the 67 openOrder of orders placed with the official API's defaults):
/// lmtPrice and auxPrice 0, volatilityType and referencePriceType 0,
/// dontUseAutoPriceForHedge true, filledQuantity 0. The other unset values
/// (minQty, trailingPercent, cashQty, triggerPrice, ...) are shown unset.
///
/// And the values it fills by itself, the same on all 198 openOrder and
/// completedOrder of the recordings (ibx#519): ocaType 3 unless the order
/// has one of its own (1 to 4), clearingIntent IB, the shareholder text,
/// and `None` for an adjusted or delta neutral order type that is not set.
pub fn reported_unset_values(order: &mut ApiOrder) {
    if !(1..=4).contains(&order.oca_type) { order.oca_type = 3; }
    if order.clearing_intent.is_empty() { order.clearing_intent = "IB".into(); }
    if order.shareholder.is_empty() { order.shareholder = NOT_A_SHAREHOLDER.into(); }
    if order.adjusted_order_type.is_empty() { order.adjusted_order_type = "None".into(); }
    if order.delta_neutral_order_type.is_empty() { order.delta_neutral_order_type = "None".into(); }
    order.lmt_price = aux_or_zero(order.lmt_price);
    order.aux_price = aux_or_zero(order.aux_price);
    if order.volatility_type == i32::MAX { order.volatility_type = 0; }
    if order.reference_price_type == i32::MAX { order.reference_price_type = 0; }
    order.dont_use_auto_price_for_hedge = true;
    order.filled_quantity = aux_or_zero(order.filled_quantity);
}

/// The shareholder text of every order the reference shows.
pub const NOT_A_SHAREHOLDER: &str = "Not an insider or substantial shareholder";

/// What the reference's openOrder shows for an order placed in this
/// session, beyond the order as placed (ibx#519, the recordings of
/// 26/09/2026 to 07/10/2026):
/// - the submitter is the user of the session (an order of an earlier
///   session has none);
/// - trailStopPrice of a LMT or PEG BEST order, a combo too: the limit
///   price + 1 from its placement, whatever the caller gave, and no value
///   once the order was modified (ibx#521, the reference on 08/10/2026:
///   272.34 gives 273.34, also for an order placed with 277.34; unset after
///   a modify, on its callbacks and on a later listing). PEG BEST after a
///   modify is not recorded and follows LMT;
/// - trailStopPrice of a STP order: its stop price, the new one after a
///   modify.
fn session_order_fields(order: &mut ApiOrder, user: &str, modified: bool) {
    if order.submitter.is_empty() {
        order.submitter = user.to_string();
    }
    let unset = |v: f64| v == f64::MAX || v == 0.0;
    let is = |t: &str| order.order_type.eq_ignore_ascii_case(t);
    if is("LMT") || is("PEG BEST") {
        order.trail_stop_price = if modified || unset(order.lmt_price) { f64::MAX } else { order.lmt_price + 1.0 };
    } else if is("STP") && !unset(order.aux_price) {
        order.trail_stop_price = order.aux_price;
    }
}

/// The contract of an openOrder: the one the contract data gave, with the
/// exchange the order was placed on and no primary exchange, as the
/// reference shows it (ibx#519: SMART on every report of an order placed
/// on SMART).
fn placed_contract(contract: &mut ApiContract, placed_exchange: &str) {
    if contract.sec_type.eq_ignore_ascii_case("BAG") {
        return;
    }
    if !placed_exchange.is_empty() {
        contract.exchange = placed_exchange.to_string();
    }
    contract.primary_exchange.clear();
}

/// The order and the contract of a preview as the reference's openOrder
/// shows them: those of an order of the session (ibx#519).
pub fn preview_view(contract: &mut ApiContract, order: &mut ApiOrder, placed_exchange: &str, shared: &SharedState) {
    session_order_fields(order, &shared.reference.user_name(), false);
    placed_contract(contract, placed_exchange);
}

fn reported_trail_limit(order: &mut ApiOrder, reported: &ApiOrder) {
    // A plain TRAIL shows the stop price the server reports too (captured
    // 05/10/2026: 6117 775.06, 775.05, ... as the market moved).
    if order.order_type.eq_ignore_ascii_case("TRAIL") && reported.trail_stop_price != f64::MAX {
        order.trail_stop_price = reported.trail_stop_price;
    }
    // A TRAIL LIT as a TRAIL LIMIT: the server reports its limit price,
    // offset and trigger (captured 26/09/2026, ibx#469).
    if !["TRAIL LIMIT", "TRAIL LIT"].iter().any(|t| order.order_type.eq_ignore_ascii_case(t)) { return; }
    if reported.lmt_price != 0.0 { order.lmt_price = reported.lmt_price; }
    if reported.lmt_price_offset != f64::MAX { order.lmt_price_offset = reported.lmt_price_offset; }
    if reported.trail_stop_price != f64::MAX { order.trail_stop_price = reported.trail_stop_price; }
}

impl ClientCore {
    /// The reference reads request ids, ticker ids, order ids and conIds
    /// as 32-bit ints: a request with one outside that range does not
    /// decode there and is dropped, with a log line and no error (ibx#285).
    /// False for such a request.
    pub fn ids_fit(request: &str, ids: &[i64]) -> bool {
        match ids.iter().find(|&&id| i32::try_from(id).is_err()) {
            None => true,
            Some(id) => {
                log::warn!("{request}: id {id} is outside the 32-bit range of the reference, request dropped");
                false
            }
        }
    }

    pub fn new() -> Self {
        Self {
            req_to_instrument: Mutex::new(HashMap::new()),
            instrument_to_req: Mutex::new(HashMap::new()),
            con_id_to_instrument: Mutex::new(HashMap::new()),
            last_quotes: Mutex::new(HashMap::new()),
            md_reader: Mutex::new(md_stream::MdReader::new()),
            md_joins_waiting: std::sync::atomic::AtomicUsize::new(0),
            md_out: Mutex::new(Vec::with_capacity(64)),
            md_news: Mutex::new(HashMap::new()),
            md_top_off: Mutex::new(HashSet::new()),
            md_generic: Mutex::new(HashMap::new()),
            md_waiting: Mutex::new(Vec::new()),
            md_delayed_known: Mutex::new(HashSet::new()),
            md_frozen: Mutex::new(HashMap::new()),
            instrument_params: Mutex::new(HashMap::new()),
            instrument_news: Mutex::new(HashMap::new()),
            md_joins: Mutex::new(Vec::new()),
            md_no_data: Mutex::new(Vec::new()),
            md_slot_con_ids: Mutex::new(HashMap::new()),
            md_no_data_count: std::sync::atomic::AtomicUsize::new(0),
            md_modes: Mutex::new(Default::default()),
            generic_legal: Mutex::new(None),
            snapshot_reqs: Mutex::new(HashMap::new()),
            snapshot_count: std::sync::atomic::AtomicUsize::new(0),
            snapshot_rate: Mutex::new(Default::default()),
            reg_snapshots: Mutex::new(Vec::new()),
            reg_snapshot_acks: Mutex::new(HashMap::new()),
            smart_components_waiting: Mutex::new(Vec::new()),
            pnl_reqs: Mutex::new(HashMap::new()),
            pnl_quotes: Mutex::new(PnlQuotes::default()),
            currency_sent: Mutex::new(HashMap::new()),
            pnl_single_reqs: Mutex::new(HashMap::new()),
            last_pnl_single: Mutex::new(HashMap::new()),
            account_summaries: Mutex::new(Vec::new()),
            positions_sub: Mutex::new(None),
            positions_multi: Mutex::new(Vec::new()),
            account_multi: Mutex::new(Vec::new()),
            next_account_summary: AtomicU64::new(1),
            bulletin_subscribed: AtomicBool::new(false),
            bulletin_replay: AtomicBool::new(false),
            display_group_subs: Mutex::new(HashSet::new()),
            account_updates_subscribed: AtomicBool::new(false),
            account_stream: Mutex::new(AccountStream::default()),
            last_portfolio: Mutex::new(None),
            executions: Mutex::new(Vec::new()),
            client_id: AtomicI64::new(0),
            rth_dropped: Mutex::new(HashSet::new()),
            discarded: Mutex::new(HashSet::new()),
            last_reports: Mutex::new(HashMap::new()),
            pending_commissions: Mutex::new(PendingCommissions::default()),
            open_orders: Mutex::new(HashMap::new()),
            held_open_orders: Mutex::new(Vec::new()),
            what_if_orders: Mutex::new(HashMap::new()),
            finished_orders: Mutex::new(HashSet::new()),
            modified_orders: Mutex::new(HashSet::new()),
            silent_executions: Mutex::new(HashSet::new()),
            highest_order_id: Arc::new(AtomicI64::new(0)),
            order_id_saver: Mutex::new(None),
            reserved_order_id: AtomicI64::new(0),
            market_data_type: AtomicI32::new(1),
            mdt_sent: Mutex::new(HashSet::new()),
            delayed_reqs: Mutex::new(HashSet::new()),
            tick_req_params_sent: Mutex::new(HashSet::new()),
            tbt_reqs: Mutex::new(HashMap::new()),
            contract_cache: Mutex::new(HashMap::new()),
        }
    }

    /// Clear all per-session state so the owning client can reconnect.
    pub fn reset(&self) {
        self.req_to_instrument.lock().unwrap().clear();
        self.instrument_to_req.lock().unwrap().clear();
        self.con_id_to_instrument.lock().unwrap().clear();
        self.last_quotes.lock().unwrap().clear();
        *self.md_reader.lock().unwrap() = md_stream::MdReader::new();
        self.md_joins_waiting.store(0, Ordering::Release);
        self.md_news.lock().unwrap().clear();
        self.md_generic.lock().unwrap().clear();
        self.instrument_params.lock().unwrap().clear();
        self.instrument_news.lock().unwrap().clear();
        self.md_joins.lock().unwrap().clear();
        self.md_no_data.lock().unwrap().clear();
        self.md_slot_con_ids.lock().unwrap().clear();
        self.md_no_data_count.store(0, Ordering::Release);
        *self.md_modes.lock().unwrap() = Default::default();
        self.md_frozen.lock().unwrap().clear();
        self.snapshot_reqs.lock().unwrap().clear();
        self.snapshot_count.store(0, Ordering::Release);
        self.reg_snapshots.lock().unwrap().clear();
        self.reg_snapshot_acks.lock().unwrap().clear();
        self.smart_components_waiting.lock().unwrap().clear();
        self.pnl_reqs.lock().unwrap().clear();
        *self.pnl_quotes.lock().unwrap() = PnlQuotes::default();
        self.currency_sent.lock().unwrap().clear();
        self.pnl_single_reqs.lock().unwrap().clear();
        self.last_pnl_single.lock().unwrap().clear();
        self.account_summaries.lock().unwrap().clear();
        *self.positions_sub.lock().unwrap() = None;
        self.positions_multi.lock().unwrap().clear();
        self.account_multi.lock().unwrap().clear();
        self.bulletin_subscribed.store(false, Ordering::Relaxed);
        self.bulletin_replay.store(false, Ordering::Relaxed);
        self.display_group_subs.lock().unwrap().clear();
        self.account_updates_subscribed.store(false, Ordering::Relaxed);
        *self.account_stream.lock().unwrap() = AccountStream::default();
        *self.last_portfolio.lock().unwrap() = None;
        self.executions.lock().unwrap().clear();
        self.pending_commissions.lock().unwrap().clear();
        self.open_orders.lock().unwrap().clear();
        self.what_if_orders.lock().unwrap().clear();
        // `finished_orders` and `highest_order_id` are kept: the server
        // still knows those orders after a reconnect, so their ids must not
        // be sent as new orders (the reference keeps the highest id of a
        // client in its settings).
        self.market_data_type.store(1, Ordering::Relaxed);
        self.mdt_sent.lock().unwrap().clear();
        self.delayed_reqs.lock().unwrap().clear();
        self.tick_req_params_sent.lock().unwrap().clear();
        self.tbt_reqs.lock().unwrap().clear();
        self.contract_cache.lock().unwrap().clear();
    }

    // ── Registration helpers ──

    /// Registration reply timeout.
    #[cfg(not(test))]
    const REGISTRATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
    /// In the unit tests: short, for the tests with no engine to answer,
    /// and long enough for a stand-in engine thread to answer on a loaded
    /// machine (1 ms failed at random, "Registration timed out").
    #[cfg(test)]
    const REGISTRATION_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

    /// Wait for the hot loop to process a registration command and return the
    /// assigned ID. The engine replies Err when the instrument table is full
    /// (ibx#233) — previously that condition killed the hot loop.
    fn recv_registration(reply_rx: crossbeam_channel::Receiver<Result<InstrumentId, String>>) -> Result<InstrumentId, String> {
        reply_rx.recv_timeout(Self::REGISTRATION_TIMEOUT)
            .map_err(|_| "Registration timed out".to_string())?
    }

    /// The instrument of an order (ibx#486): by conId as any contract; a
    /// contract given without a conId gets a slot of its own for each new
    /// order, and the engine looks it up before the order goes out, as the
    /// reference does for each API order (every four-leg recording of 26/09
    /// to 02/10/2026: `FixSecDefReqBySymbol` before each 35=D). A modify
    /// keeps the slot of its order.
    #[allow(clippy::too_many_arguments)]
    pub fn order_instrument(
        &self, control_tx: &ControlSender, order_id: OrderId, what_if: bool,
        con_id: i64, symbol: &str, exchange: &str, sec_type: &str, currency: &str,
    ) -> Result<InstrumentId, String> {
        if con_id != 0 || sec_type.eq_ignore_ascii_case("BAG") {
            return self.find_or_register_instrument(control_tx, con_id, symbol, exchange, sec_type);
        }
        if !what_if && let Some(t) = self.open_orders.lock().unwrap().get(&order_id) {
            return Ok(t.instrument);
        }
        let (reply_tx, reply_rx) = crossbeam_channel::bounded(1);
        control_tx.send(ControlCommand::RegisterOrderContract {
            symbol: symbol.to_string(), sec_type: sec_type.to_string(), exchange: exchange.to_string(),
            currency: currency.to_string(), reply_tx: Some(reply_tx),
        }).map_err(|e| format!("Engine stopped: {}", e))?;
        Self::recv_registration(reply_rx)
    }

    /// Find instrument ID for a contract, registering if needed.
    /// Returns `Err` if the control channel is closed.
    /// Tell the engine the currency of a contract before an order on it,
    /// when it does not have it yet (tag 15, ibx#466).
    pub fn note_currency(&self, control_tx: &ControlSender, con_id: i64, currency: &str) {
        if currency.is_empty() || con_id == 0 {
            return;
        }
        let mut sent = self.currency_sent.lock().unwrap();
        if sent.get(&con_id).map(String::as_str) == Some(currency) {
            return;
        }
        if control_tx.send(ControlCommand::SetInstrumentCurrency { con_id, currency: currency.to_string() }).is_ok() {
            sent.insert(con_id, currency.to_string());
        }
    }

    /// True when `note_currency` has nothing to send for this contract.
    pub fn currency_noted(&self, con_id: i64, currency: &str) -> bool {
        currency.is_empty() || con_id == 0
            || self.currency_sent.lock().unwrap().get(&con_id).map(String::as_str) == Some(currency)
    }

    pub fn find_or_register_instrument(
        &self,
        control_tx: &ControlSender,
        con_id: i64,
        symbol: &str,
        exchange: &str,
        sec_type: &str,
    ) -> Result<InstrumentId, String> {
        // Check if already mapped by con_id
        {
            let map = self.con_id_to_instrument.lock().unwrap();
            if let Some(&iid) = map.get(&con_id) {
                return Ok(iid);
            }
        }

        // Register new — only allocates an InstrumentId slot, does not subscribe to market data.
        let (reply_tx, reply_rx) = crossbeam_channel::bounded(1);
        control_tx.send(ControlCommand::RegisterInstrument {
            con_id, symbol: symbol.to_string(),
            sec_type: sec_type.to_string(), exchange: exchange.to_string(),
            reply_tx: Some(reply_tx),
        }).map_err(|e| format!("Engine stopped: {}", e))?;

        let id = Self::recv_registration(reply_rx)?;
        self.con_id_to_instrument.lock().unwrap().insert(con_id, id);
        Ok(id)
    }

    // ── Subscription management ──

    /// Register a market data subscription mapping.
    /// With the news tick (292) in `generic_tick_list`, the request's news
    /// entry follows its top of book (ibx#458); the caller made the checks
    /// of `news_tick_refusal` for a contract with a conId.
    /// A contract without a conId is looked up by the engine first, as the
    /// reference does, with the currency and `filters` (ibx#278).
    /// A contract another request of this client already subscribed is
    /// shared, as the reference shares the contract's record among its API
    /// subscribers (ibx#444): nothing new goes to the farm for its top of
    /// book, and the request gets at once what the others have.
    pub fn register_mkt_data(
        &self,
        shared: &SharedState,
        control_tx: &ControlSender,
        req_id: i64,
        con_id: i64,
        symbol: &str,
        exchange: &str,
        sec_type: &str,
        currency: &str,
        filters: &crate::types::SecDefFilters,
        snapshot: bool,
        generic_tick_list: &str,
        mode_9887: i32,
    ) -> Result<Option<InstrumentId>, String> {
        let (last_trade_date, strike, right, multiplier) = (
            filters.last_trade_date_or_contract_month.as_str(), filters.strike,
            filters.right.as_str(), filters.multiplier.as_str(),
        );
        // The news tick of the request (ibx#458): its provider key, or the
        // 10094 it ends with once the contract is known.
        let news = match news_tick(generic_tick_list) {
            NewsTick::Default => Some(None),
            NewsTick::Codes(codes) => Some(Some(codes)),
            NewsTick::None | NewsTick::Invalid => None,
        }.map(|codes| match news_providers(sec_type, codes.as_deref(), &shared.reference.news_sources()) {
            Ok(providers) => (providers.join(","), None),
            Err(text) => (String::new(), Some(text)),
        });
        let news_key = news.as_ref().filter(|(_, refusal)| refusal.is_none()).map(|(providers, _)| providers.clone());
        let send_news = |instrument: InstrumentId| {
            if let Some((providers, refusal)) = &news {
                let _ = control_tx.send(ControlCommand::SubscribeNews {
                    instrument, con_id, exchange: exchange.to_string(), sec_type: sec_type.to_string(),
                    providers: providers.clone(), refusal: refusal.clone(),
                });
            }
        };
        // `mdoff` anywhere in the list, in any case, turns the top of book
        // off for the request, as the reference's `generictick.bq.a(String)`
        // (ibx#444).
        let top_off = generic_tick_list.to_ascii_lowercase().contains("mdoff");
        // Its other generic ticks (ibx#450), request codes; the news tick
        // has its own entry.
        let generic: Vec<i32> = if snapshot {
            Vec::new()
        } else {
            crate::control::generic_tick::parse(generic_tick_list, sec_type).unwrap_or_default()
                .into_iter().map(|t| t.code).filter(|&c| c != 292).collect()
        };
        let send_generic = |instrument: InstrumentId| {
            if !generic.is_empty() {
                self.md_generic.lock().unwrap().insert(req_id, MdGeneric {
                    codes: generic.clone(), exchange: exchange.to_string(), sec_type: sec_type.to_string(),
                });
                let _ = control_tx.send(ControlCommand::SubscribeGeneric {
                    instrument, con_id, exchange: exchange.to_string(), sec_type: sec_type.to_string(), codes: generic.clone(),
                });
            }
        };
        let attach = |instrument: InstrumentId, had_data: bool| {
            if self.attach_md_request(shared, req_id, instrument, snapshot, sec_type, exchange, news_key.clone(), had_data, top_off) {
                send_news(instrument);
                send_generic(instrument);
            }
            Ok(Some(instrument))
        };

        // A quote ibx subscribed to for the P&L becomes the caller's
        // subscription: no second subscription to the server.
        if let Some(instrument_id) = self.pnl_quotes.lock().unwrap().active.remove(&con_id) {
            self.con_id_to_instrument.lock().unwrap().insert(con_id, instrument_id);
            return attach(instrument_id, true);
        }

        // No conId: not an identity. The engine gives the request its own
        // slot and resolves the conId before it subscribes (ibx#278); the
        // only duplicate check is the one on the request id. When the
        // contract it finds is subscribed already, the request joins that
        // subscription (`take_md_rejects`).
        if con_id == 0 {
            let (reply_tx, reply_rx) = crossbeam_channel::bounded(1);
            control_tx.send(ControlCommand::SubscribeBySymbol {
                symbol: symbol.to_string(),
                sec_type: sec_type.to_string(),
                exchange: exchange.to_string(),
                currency: currency.to_string(),
                filters: filters.clone(),
                mode_9887, snapshot,
                reply_tx: Some(reply_tx),
            }).map_err(|e| format!("Engine stopped: {}", e))?;
            let instrument_id = Self::recv_registration(reply_rx)?;
            return attach(instrument_id, false);
        }

        // A contract the server refused with delayed data available: the
        // reference's new subscriber on that record gets 10168 when its
        // client has not enabled delayed data, and is removed; else it
        // observes delayed data at once (marketDataType 3, no 10167), with
        // delayed entries (`jextend.s.a(dy,ec,Set)@104-191`, ibx#444).
        let delayed_known = self.md_delayed_known.lock().unwrap().contains(&con_id);
        if delayed_known && !self.md_modes.lock().unwrap().delayed {
            shared.orders.push_order_error(req_id, MD_DELAYED_NOT_ENABLED.0, MD_DELAYED_NOT_ENABLED.1.to_string());
            return Ok(None);
        }

        // A contract this client already streams: the request joins at
        // once. A stream asked where only snapshots run goes to the engine,
        // which adds the streaming entries; so does a request with a mode
        // of `req_mkt_data_ex`, an extension the reference has not.
        let running = self.con_id_to_instrument.lock().unwrap().get(&con_id).copied().filter(|iid| {
            if mode_9887 != 0 {
                return false;
            }
            let observers = self.instrument_to_req.lock().unwrap();
            let snaps = self.snapshot_reqs.lock().unwrap();
            observers.get(iid).is_some_and(|reqs| !reqs.is_empty() && (snapshot || reqs.iter().any(|r| !snaps.contains_key(r))))
        });
        if let Some(instrument_id) = running {
            return attach(instrument_id, false);
        }
        let mode_9887 = if delayed_known { crate::types::MarketDataModes::entry_mode(false, true) } else { mode_9887 };

        // The API ticker limit: a stream that needs a contract no stream
        // uses yet, with every line taken, gets 101 and waits for a line,
        // as the reference's line manager (`jclient.k_.a(boolean)`, maxApi
        // = the logon's API max tickers; error from
        // `jextend.a4.a(jclient.record.dU)@136`). Snapshots take no line.
        if !snapshot && self.md_lines_in_use() >= shared.reference.snapshot_rate_limit() as usize {
            shared.orders.push_order_error(req_id, MD_MAX_TICKERS.0, MD_MAX_TICKERS.1.to_string());
            self.md_waiting.lock().unwrap().push(MdWaiting {
                req_id, con_id, symbol: symbol.to_string(), exchange: exchange.to_string(),
                sec_type: sec_type.to_string(), currency: currency.to_string(), filters: filters.clone(),
                generic_tick_list: generic_tick_list.to_string(), mode_9887,
            });
            return Ok(None);
        }

        let (reply_tx, reply_rx) = crossbeam_channel::bounded(1);
        control_tx.send(ControlCommand::RegisterInstrument {
            con_id, symbol: symbol.to_string(),
            sec_type: sec_type.to_string(), exchange: exchange.to_string(),
            reply_tx: None,
        }).map_err(|e| format!("Engine stopped: {}", e))?;
        control_tx.send(ControlCommand::Subscribe {
            con_id,
            symbol: symbol.to_string(),
            exchange: exchange.to_string(),
            sec_type: sec_type.to_string(),
            last_trade_date: last_trade_date.to_string(),
            strike,
            right: right.to_string(),
            multiplier: multiplier.to_string(),
            mode_9887, snapshot,
            reply_tx: Some(reply_tx),
        }).map_err(|e| format!("Engine stopped: {}", e))?;

        let instrument_id = Self::recv_registration(reply_rx)?;
        self.con_id_to_instrument.lock().unwrap().insert(con_id, instrument_id);
        let attached = attach(instrument_id, false);
        if delayed_known && self.req_to_instrument.lock().unwrap().contains_key(&req_id) {
            self.set_delayed(req_id);
            self.md_joins.lock().unwrap().push((req_id, true));
        }
        attached
    }

    /// The market data lines in use: the contracts with a streaming
    /// request (ibx#444).
    pub fn md_lines_in_use(&self) -> usize {
        let observers = self.instrument_to_req.lock().unwrap();
        let snaps = self.snapshot_reqs.lock().unwrap();
        observers.values().filter(|reqs| reqs.iter().any(|r| !snaps.contains_key(r))).count()
    }

    /// The requests waiting for a line (101) that a free line lets through,
    /// oldest first, as the reference's line manager subscribes the
    /// records it kept waiting once the count is under the limit
    /// (ibx#444). They get no second 101.
    pub fn promote_waiting_md(&self, shared: &SharedState, control_tx: &ControlSender) {
        loop {
            if self.md_waiting.lock().unwrap().is_empty()
                || self.md_lines_in_use() >= shared.reference.snapshot_rate_limit() as usize
            {
                return;
            }
            let w = self.md_waiting.lock().unwrap().remove(0);
            if let Err(e) = self.register_mkt_data(
                shared, control_tx, w.req_id, w.con_id, &w.symbol, &w.exchange, &w.sec_type, &w.currency,
                &w.filters, false, &w.generic_tick_list, w.mode_9887,
            ) {
                log::warn!("Market data request {} waiting for a line: {}", w.req_id, e);
            }
        }
    }

    /// Add a new market data request to the requests of an instrument
    /// (ibx#444); false when refused (`join_md_observers`).
    #[allow(clippy::too_many_arguments)]
    fn attach_md_request(
        &self, shared: &SharedState, req_id: i64, instrument: InstrumentId, snapshot: bool, sec_type: &str,
        exchange: &str, news: Option<String>, had_data: bool, top_off: bool,
    ) -> bool {
        if let Some(key) = news.clone() {
            self.md_news.lock().unwrap().insert(req_id, key);
        }
        if top_off {
            self.md_top_off.lock().unwrap().insert(req_id);
        }
        let kind = md_stream::NewRequest {
            snapshot: snapshot.then_some(sec_type), top_off, farm_auto: auto_execution_from_farm(sec_type, exchange),
        };
        if !self.join_md_observers(shared, req_id, instrument, had_data, Some(kind), None) {
            self.md_news.lock().unwrap().remove(&req_id);
            self.md_top_off.lock().unwrap().remove(&req_id);
            return false;
        }
        true
    }

    /// The requests ended by a refused news tick whose wait is over at
    /// `now`: 10197 for each whose contract has no running request, as
    /// captured (ibx#444).
    pub fn take_md_no_data(&self, now: std::time::Instant) -> Vec<MdNotice> {
        if self.md_no_data_count.load(Ordering::Acquire) == 0 {
            return Vec::new();
        }
        let mut waits = self.md_no_data.lock().unwrap();
        let due = waits.iter().take_while(|(_, at, _)| *at <= now).count();
        if due == 0 {
            return Vec::new();
        }
        let instruments = self.con_id_to_instrument.lock().unwrap();
        let slots = self.md_slot_con_ids.lock().unwrap();
        let observers = self.instrument_to_req.lock().unwrap();
        let runs = |slot: &InstrumentId| observers.get(slot).is_some_and(|reqs| !reqs.is_empty());
        let notices = waits.drain(..due)
            .filter(|(_, _, con_id)| {
                !instruments.get(con_id).is_some_and(runs) && !slots.iter().any(|(slot, c)| c == con_id && runs(slot))
            })
            .map(|(req_id, ..)| MdNotice::Error { req_id, code: MD_NO_DATA.0, text: MD_NO_DATA.1.to_string() })
            .collect();
        self.md_no_data_count.store(waits.len(), Ordering::Release);
        notices
    }

    /// A streaming request ended with a refused news tick (10094): on a
    /// paper session its contract (`con_id`) is looked at
    /// `MD_NO_DATA_WAIT` later (`take_md_no_data`).
    pub fn note_news_refused(&self, shared: &SharedState, req_id: i64, con_id: i64) {
        if !shared.reference.paper_session() {
            return;
        }
        let mut waits = self.md_no_data.lock().unwrap();
        waits.push((req_id, std::time::Instant::now() + MD_NO_DATA_WAIT, con_id));
        self.md_no_data_count.store(waits.len(), Ordering::Release);
    }

    /// Put a market data request among the requests of an instrument
    /// (ibx#444). The first request of a fresh instrument starts its
    /// stream; a request on an instrument other requests use joins it, as
    /// the reference's subscriber joins the contract's record: when that
    /// record runs on delayed data and this client has not enabled delayed
    /// data, error 10168 and the request is not kept (false); else it gets
    /// at once the market data type (3 on delayed data), the request
    /// parameters, the latest headlines of its news key, and every field
    /// the quote has (`take_md_joins`). `had_data`: the quote already has
    /// data although no request uses it (the internal P&L quote).
    /// `kind`: what a new request is (None: a request that moves from
    /// another slot keeps its kind); `at`: its place in the market data
    /// queue when not the current end. Its stream state is in place before
    /// the dispatch can see the request (ibx#446).
    fn join_md_observers(
        &self, shared: &SharedState, req_id: i64, instrument: InstrumentId, had_data: bool,
        kind: Option<md_stream::NewRequest>, at: Option<u64>,
    ) -> bool {
        let mut observers = self.instrument_to_req.lock().unwrap();
        let joining = observers.get(&instrument).is_some_and(|reqs| !reqs.is_empty());
        let delayed = joining && {
            let delayed_reqs = self.delayed_reqs.lock().unwrap();
            observers.get(&instrument).is_some_and(|reqs| reqs.iter().any(|r| delayed_reqs.contains(r)))
        };
        if delayed && !self.md_modes.lock().unwrap().delayed {
            drop(observers);
            shared.orders.push_order_error(req_id, MD_DELAYED_NOT_ENABLED.0, MD_DELAYED_NOT_ENABLED.1.to_string());
            return false;
        }
        self.start_md_stream(shared, req_id, instrument, joining, had_data, kind, at);
        observers.entry(instrument).or_default().push(req_id);
        drop(observers);
        self.req_to_instrument.lock().unwrap().insert(req_id, instrument);
        if joining || had_data {
            if delayed {
                self.set_delayed(req_id);
            }
            self.md_joins.lock().unwrap().push((req_id, delayed));
        }
        true
    }

    /// Start a regulatory snapshot (ibx#446): one fetch per contract; the
    /// request goes to the farm as a snapshot request. A second fetch of a
    /// contract being fetched gets 10169.
    #[allow(clippy::too_many_arguments)]
    pub fn start_regulatory_snapshot(
        &self, shared: &SharedState, control_tx: &ControlSender, req_id: i64,
        con_id: i64, symbol: &str, exchange: &str, sec_type: &str,
    ) -> Result<(), String> {
        if con_id == 0 {
            return Err("regulatory snapshot: a contract without conId is not supported".into());
        }
        let running = self.reg_snapshots.lock().unwrap().iter().find(|f| f.con_id == con_id).map(|f| f.req_id);
        if let Some(other) = running {
            shared.orders.push_order_error(req_id, 10169,
                format!("Regulatory snapshot for {} is already being fetched in ticker id={}", symbol, other));
            return Ok(());
        }
        let (reply_tx, reply_rx) = crossbeam_channel::bounded(1);
        control_tx.send(ControlCommand::RegisterInstrument {
            con_id, symbol: symbol.to_string(), sec_type: sec_type.to_string(), exchange: exchange.to_string(), reply_tx: None,
        }).map_err(|e| format!("Engine stopped: {}", e))?;
        control_tx.send(ControlCommand::SubscribeSnapshot {
            con_id, symbol: symbol.to_string(), exchange: exchange.to_string(), sec_type: sec_type.to_string(),
            reply_tx: Some(reply_tx),
        }).map_err(|e| format!("Engine stopped: {}", e))?;
        let instrument = Self::recv_registration(reply_rx)?;
        self.reg_snapshots.lock().unwrap().push(crate::control::regsnapshot::Fetch::new(
            req_id, con_id, instrument, symbol.to_string(), std::time::Instant::now(),
        ));
        Ok(())
    }

    /// Stop a regulatory snapshot (cancelMktData): nothing more is sent
    /// for it. False when the request is not one.
    pub fn cancel_regulatory_snapshot(&self, req_id: i64, control_tx: &ControlSender) -> bool {
        let mut fetches = self.reg_snapshots.lock().unwrap();
        let Some(pos) = fetches.iter().position(|f| f.req_id == req_id) else { return false };
        let f = fetches.remove(pos);
        self.reg_snapshot_acks.lock().unwrap().remove(&f.instrument);
        let _ = control_tx.send(ControlCommand::DropSnapshot { instrument: f.instrument });
        true
    }

    /// One look at every regulatory snapshot: the finished ones with their
    /// ticks or their error.
    #[allow(clippy::type_complexity)]
    pub fn poll_regulatory_snapshots(
        &self, shared: &SharedState, control_tx: &ControlSender,
    ) -> Vec<(i64, Result<Vec<crate::control::regsnapshot::SnapshotTick>, (i64, String)>)> {
        use crate::control::regsnapshot::{SnapshotFields, Step};
        {
            let mut acks = self.reg_snapshot_acks.lock().unwrap();
            for a in shared.market.drain_snapshot_acks() {
                acks.insert(a.instrument, (a.snapshot_permissions, a.bbo_exchange));
            }
        }
        let mut fetches = self.reg_snapshots.lock().unwrap();
        if fetches.is_empty() {
            return Vec::new();
        }
        let now = std::time::Instant::now();
        let acks = self.reg_snapshot_acks.lock().unwrap().clone();
        let mut out = Vec::new();
        fetches.retain_mut(|f| {
            let q = shared.market.quote(f.instrument);
            let price = |v: Price| (v != 0).then(|| v as f64 / PRICE_SCALE_F);
            let size = |v: Qty| (v != 0).then(|| v as f64 / QTY_SCALE_F);
            let exch = |m: i64| (m != 0).then(|| render_exchange_mask(m, f.instrument, shared));
            let exchange_map = shared.reference.instrument_exchange_map(f.instrument).is_some();
            let fields = SnapshotFields {
                bid: price(q.bid), ask: price(q.ask), last: price(q.last),
                bid_size: size(q.bid_size), ask_size: size(q.ask_size), last_size: size(q.last_size),
                bid_exchange: exch(q.bid_exch_mask), ask_exchange: exch(q.ask_exch_mask), last_exchange: exch(q.last_exch_mask),
                high: price(q.high), low: price(q.low), close: price(q.close), volume: size(q.volume),
                last_snapshot_time: None,
            };
            let ack = acks.get(&f.instrument).map(|(p, b)| (*p, b.as_str()));
            let result = match f.poll(now, ack, &fields, exchange_map) {
                Step::Wait => return true,
                Step::Fail(code, text) => Err((code, text)),
                Step::Deliver(ticks) => Ok(ticks),
            };
            let _ = control_tx.send(ControlCommand::DropSnapshot { instrument: f.instrument });
            out.push((f.req_id, result));
            false
        });
        let mut acks = self.reg_snapshot_acks.lock().unwrap();
        acks.retain(|i, _| fetches.iter().any(|f| f.instrument == *i));
        out
    }

    /// True when no P&L request runs and no P&L quote is held:
    /// `maintain_pnl_quotes` then has nothing to do.
    pub fn pnl_quotes_idle(&self) -> bool {
        self.pnl_reqs.lock().unwrap().is_empty()
            && self.pnl_single_reqs.lock().unwrap().is_empty()
            && {
                let q = self.pnl_quotes.lock().unwrap();
                q.active.is_empty() && q.pending.is_empty()
            }
    }

    /// Keep a quote for each stock position the running P&L requests need
    /// (held now or at midnight, and each pnl_single contract), subscribed
    /// by ibx itself when the caller has none; cancel them when no P&L
    /// request needs them. Never waits: a registration reply is read on a
    /// later call. Checked when positions or requests change, and every
    /// second.
    pub fn maintain_pnl_quotes(&self, shared: &SharedState, control_tx: &ControlSender) {
        let n_pnl = self.pnl_reqs.lock().unwrap().len();
        let singles: Vec<i64> = self.pnl_single_reqs.lock().unwrap().values().copied().collect();
        let mut q = self.pnl_quotes.lock().unwrap();
        if n_pnl == 0 && singles.is_empty() && q.active.is_empty() && q.pending.is_empty() {
            return;
        }

        let done: Vec<(i64, Result<InstrumentId, String>)> = q.pending.iter()
            .filter_map(|(&con_id, rx)| rx.try_recv().ok().map(|r| (con_id, r)))
            .collect();
        for (con_id, reply) in done {
            q.pending.remove(&con_id);
            match reply {
                Ok(instrument) => {
                    q.active.insert(con_id, instrument);
                    self.con_id_to_instrument.lock().unwrap().insert(con_id, instrument);
                }
                Err(e) => log::warn!("P&L quote for conId {} not subscribed: {}", con_id, e),
            }
        }

        let seeds = shared.portfolio.midnight_seeds();
        let key = (shared.portfolio.position_generation(), n_pnl, singles.len(), seeds.len());
        if key == q.key && q.checked_at.is_some_and(|t| t.elapsed() < PNL_QUOTES_CHECK) {
            return;
        }
        q.key = key;
        q.checked_at = Some(std::time::Instant::now());

        let infos: HashMap<i64, PositionInfo> = shared.portfolio.position_infos()
            .into_iter().map(|p| (p.con_id, p)).collect();
        let is_stock = |con_id: i64| infos.get(&con_id).is_none_or(|p| p.sec_type.is_empty() || p.sec_type == "STK");
        let mut wanted: HashSet<i64> = HashSet::new();
        if n_pnl > 0 {
            wanted.extend(infos.values().filter(|p| p.position_fixed != 0).map(|p| p.con_id));
            wanted.extend(seeds.iter().filter(|s| s.qty_midnight_fixed != 0).map(|s| s.con_id));
        }
        wanted.extend(singles.iter().copied());
        wanted.retain(|&c| c != 0 && is_stock(c));

        let caller_has = |con_id: i64| -> bool {
            let map = self.con_id_to_instrument.lock().unwrap();
            map.get(&con_id).is_some_and(|iid| self.instrument_to_req.lock().unwrap().contains_key(iid))
        };

        for &con_id in &wanted {
            if q.active.contains_key(&con_id) || q.pending.contains_key(&con_id) || caller_has(con_id) {
                continue;
            }
            let symbol = infos.get(&con_id).map(|p| p.symbol.clone()).unwrap_or_default();
            let (reply_tx, reply_rx) = crossbeam_channel::bounded(1);
            let registered = control_tx.send(ControlCommand::RegisterInstrument {
                con_id, symbol: symbol.clone(), sec_type: "STK".into(), exchange: "SMART".into(), reply_tx: None,
            }).and_then(|_| control_tx.send(ControlCommand::Subscribe {
                con_id, symbol, exchange: "SMART".into(), sec_type: "STK".into(),
                last_trade_date: String::new(), strike: 0.0, right: String::new(), multiplier: String::new(),
                mode_9887: 0, snapshot: false, reply_tx: Some(reply_tx),
            }));
            if registered.is_ok() {
                q.pending.insert(con_id, reply_rx);
            }
        }

        let unwanted: Vec<(i64, InstrumentId)> = q.active.iter()
            .filter(|(c, _)| !wanted.contains(c))
            .map(|(&c, &i)| (c, i))
            .collect();
        for (con_id, instrument) in unwanted {
            q.active.remove(&con_id);
            if !caller_has(con_id) {
                let _ = control_tx.send(ControlCommand::Unsubscribe { instrument });
                // The engine may reuse the slot (ibx#233).
                self.forget_instrument(instrument);
            }
        }
    }

    /// Unregister a market data request (ibx#444): None for an unknown
    /// request id. The instrument's subscription ends with its last
    /// request; before that, only the request's share of the news entry
    /// goes.
    pub fn unregister_mkt_data(&self, shared: &SharedState, req_id: i64) -> Option<MdCancel> {
        self.drop_mkt_data(shared, req_id, true)
    }

    /// [`Self::unregister_mkt_data`]; `cancelled`: the client cancelled it,
    /// so the steps queued before still go to it (ibx#446); not for a
    /// request the server or a join refused.
    fn drop_mkt_data(&self, shared: &SharedState, req_id: i64, cancelled: bool) -> Option<MdCancel> {
        {
            let mut waiting = self.md_waiting.lock().unwrap();
            if let Some(i) = waiting.iter().position(|w| w.req_id == req_id) {
                waiting.remove(i);
                return Some(MdCancel::Waiting);
            }
        }
        let instrument = self.req_to_instrument.lock().unwrap().remove(&req_id)?;
        let last = {
            let mut observers = self.instrument_to_req.lock().unwrap();
            // Its steps queued so far are still its own (ibx#446).
            self.end_md_stream(shared, req_id, instrument, cancelled);
            match observers.get_mut(&instrument) {
                Some(reqs) => {
                    reqs.retain(|r| *r != req_id);
                    let last = reqs.is_empty();
                    if last {
                        observers.remove(&instrument);
                    }
                    last
                }
                None => true,
            }
        };
        self.mdt_sent.lock().unwrap().remove(&req_id);
        let delayed = self.delayed_reqs.lock().unwrap().remove(&req_id);
        self.tick_req_params_sent.lock().unwrap().remove(&req_id);
        self.md_joins.lock().unwrap().retain(|(r, _)| *r != req_id);
        let news = self.md_news.lock().unwrap().remove(&req_id);
        let generic = self.md_generic.lock().unwrap().remove(&req_id).map(|g| g.codes).unwrap_or_default();
        self.md_top_off.lock().unwrap().remove(&req_id);
        self.end_snapshot(req_id);
        if !last {
            return Some(MdCancel::Shared { instrument, news, generic });
        }
        // No step of the contract is queued any more (ibx#446).
        shared.market.md_events.listen(instrument, false);
        self.md_frozen.lock().unwrap().retain(|_, frozen| *frozen != instrument);
        self.instrument_params.lock().unwrap().remove(&instrument);
        self.instrument_news.lock().unwrap().remove(&instrument);
        // A delayed record that is let go loses its "delayed available"
        // mark, as the reference's (`jclient.is` cancel of a delayed
        // record, `pa.l(false)`): the next request asks for live data again.
        if delayed {
            let con_ids: Vec<i64> = self.con_id_to_instrument.lock().unwrap().iter()
                .filter(|(_, iid)| **iid == instrument).map(|(c, _)| *c).collect();
            let mut known = self.md_delayed_known.lock().unwrap();
            for c in con_ids { known.remove(&c); }
        }
        // The slot stays while tick-by-tick data uses it.
        if !self.tbt_reqs.lock().unwrap().values().any(|(i, ..)| *i == instrument) {
            self.forget_instrument(instrument);
        }
        Some(MdCancel::Instrument(instrument))
    }

    /// The market data requests of an instrument, in the order they came.
    pub fn md_requests_of(&self, instrument: InstrumentId) -> Vec<i64> {
        self.instrument_to_req.lock().unwrap().get(&instrument).cloned().unwrap_or_default()
    }

    /// What the server said about the subscriptions since the last
    /// dispatch, for each request of their instrument (ibx#444, ibx#447),
    /// with the commands for the engine. First the requests without a
    /// conId whose contract another request had subscribed: they join that
    /// subscription (or get 10168, as any join) and their own slot is
    /// freed. Then the rejects: on delayed data, marketDataType 3 and
    /// 10167; a request with the news tick keeps it, with 2117; the others
    /// end with their error.
    pub fn take_md_rejects(&self, shared: &SharedState) -> (Vec<MdNotice>, Vec<ControlCommand>) {
        let mut notices = Vec::new();
        let mut commands = Vec::new();
        let resolved = shared.market.drain_md_resolved();
        if !resolved.is_empty() {
            self.md_slot_con_ids.lock().unwrap().extend(resolved);
        }
        for (from, into, at) in shared.market.drain_md_merges() {
            self.md_slot_con_ids.lock().unwrap().remove(&from);
            // None left: they were cancelled, which freed the slot.
            let Some(reqs) = self.instrument_to_req.lock().unwrap().remove(&from) else { continue };
            for req_id in reqs {
                // Its generic ticks go to the contract it joins (ibx#450).
                if let Some(MdGeneric { codes, exchange, sec_type }) = self.md_generic.lock().unwrap().get(&req_id).cloned() {
                    commands.push(ControlCommand::SubscribeGeneric { instrument: into, con_id: 0, exchange, sec_type, codes });
                }
                if !self.join_md_observers(shared, req_id, into, false, None, Some(at)) {
                    // Refused (10168): the request is gone.
                    self.req_to_instrument.lock().unwrap().insert(req_id, into);
                    commands.extend(self.drop_mkt_data(shared, req_id, false).map(MdCancel::commands).unwrap_or_default());
                }
            }
            commands.push(ControlCommand::Unsubscribe { instrument: from });
        }
        for reject in shared.market.drain_md_rejects() {
            match reject {
                crate::bridge::MdReject::Frozen { instrument, frozen } => {
                    self.md_freeze(shared, instrument, frozen, &mut notices);
                    continue;
                }
                crate::bridge::MdReject::Live { instrument, live } => {
                    self.md_thaw(shared, instrument, live, &mut notices);
                    commands.push(ControlCommand::Unsubscribe { instrument });
                    continue;
                }
                _ => {}
            }
            let (code, text, gone) = Self::md_reject_error(&reject);
            // The record keeps "not subscribed, delayed available" (ibx#444).
            if let crate::bridge::MdReject::NotSubscribed { instrument, delayed_available, .. } = &reject {
                let con_ids: Vec<i64> = self.con_id_to_instrument.lock().unwrap().iter()
                    .filter(|(_, iid)| **iid == *instrument).map(|(c, _)| *c).collect();
                let mut known = self.md_delayed_known.lock().unwrap();
                for c in con_ids {
                    if *delayed_available { known.insert(c); } else { known.remove(&c); }
                }
            }
            let keeps_news = matches!(reject, crate::bridge::MdReject::NotSubscribed { .. });
            // The parameters the contract's record kept from an earlier
            // subscription go first (ibx#444).
            let kept = match &reject {
                crate::bridge::MdReject::NotSubscribed { kept_params, .. } => kept_params.clone(),
                _ => None,
            };
            for req_id in self.md_requests_of(reject.instrument()) {
                if let Some((min_tick, bbo_exchange, permissions)) = kept.clone()
                    && self.tick_req_params_sent.lock().unwrap().insert(req_id)
                {
                    notices.push(MdNotice::TickReqParams { req_id, min_tick, bbo_exchange, permissions });
                }
                if !gone {
                    self.set_delayed(req_id);
                    notices.push(MdNotice::MarketDataType { req_id, market_data_type: 3 });
                    notices.push(MdNotice::Error { req_id, code, text: text.to_string() });
                } else if keeps_news && self.md_news.lock().unwrap().contains_key(&req_id) {
                    notices.push(MdNotice::Error { req_id, code: MD_TOP_REJECTED.0, text: MD_TOP_REJECTED.1.to_string() });
                } else {
                    notices.push(MdNotice::Error { req_id, code, text: text.to_string() });
                    let streaming = !self.snapshot_reqs.lock().unwrap().contains_key(&req_id);
                    commands.extend(self.drop_mkt_data(shared, req_id, false).map(MdCancel::commands).unwrap_or_default());
                    // A refused news tick: its contract is looked at later (10197).
                    if let crate::bridge::MdReject::NewsRefused { con_id, .. } = &reject
                        && streaming
                    {
                        self.note_news_refused(shared, req_id, *con_id);
                    }
                }
            }
        }
        (notices, commands)
    }

    /// The contract of slot `live` is frozen (ibx#447): its requests go on
    /// with the frozen top of book of slot `frozen`, as the reference's
    /// subscriber reads the record's frozen view once frozen is confirmed
    /// (`jextend.v.a(boolean)@36-140`): marketDataType 2 (after the request
    /// parameters the contract kept, when the request has not had them),
    /// then every value of the frozen view as it comes, sent again from
    /// nothing (captured 07/10/2026).
    fn md_freeze(&self, shared: &SharedState, live: InstrumentId, frozen: InstrumentId, notices: &mut Vec<MdNotice>) {
        let Some(reqs) = self.instrument_to_req.lock().unwrap().remove(&live) else { return };
        shared.market.md_events.listen(live, false);
        let params = self.instrument_params.lock().unwrap().remove(&live);
        if let Some(p) = params.clone() {
            self.instrument_params.lock().unwrap().insert(frozen, p);
        }
        for iid in self.con_id_to_instrument.lock().unwrap().values_mut() {
            if *iid == live { *iid = frozen; }
        }
        self.md_frozen.lock().unwrap().insert(live, frozen);
        for req_id in reqs {
            self.req_to_instrument.lock().unwrap().remove(&req_id);
            self.md_joins.lock().unwrap().retain(|(r, _)| *r != req_id);
            if !self.join_md_observers(shared, req_id, frozen, false, None, None) {
                continue;
            }
            if let Some((min_tick, bbo_exchange, permissions)) = params.clone()
                && self.tick_req_params_sent.lock().unwrap().insert(req_id)
            {
                notices.push(MdNotice::TickReqParams { req_id, min_tick, bbo_exchange, permissions });
            }
            self.mdt_sent.lock().unwrap().insert(req_id);
            notices.push(MdNotice::MarketDataType { req_id, market_data_type: MDT_FROZEN });
        }
    }

    /// The contract of the frozen slot `frozen` has real-time data again
    /// (ibx#447): its requests go back to slot `live`, marketDataType 1,
    /// then every value of the real-time quote (`jextend.v.a(boolean)`
    /// false: REGULAR, then `s.e(false)`).
    fn md_thaw(&self, shared: &SharedState, frozen: InstrumentId, live: InstrumentId, notices: &mut Vec<MdNotice>) {
        self.md_frozen.lock().unwrap().retain(|_, f| *f != frozen);
        let reqs = self.instrument_to_req.lock().unwrap().remove(&frozen).unwrap_or_default();
        shared.market.md_events.listen(frozen, false);
        if let Some(p) = self.instrument_params.lock().unwrap().remove(&frozen) {
            self.instrument_params.lock().unwrap().entry(live).or_insert(p);
        }
        for iid in self.con_id_to_instrument.lock().unwrap().values_mut() {
            if *iid == frozen { *iid = live; }
        }
        for req_id in reqs {
            self.req_to_instrument.lock().unwrap().remove(&req_id);
            self.md_joins.lock().unwrap().retain(|(r, _)| *r != req_id);
            if self.join_md_observers(shared, req_id, live, true, None, None) {
                notices.push(MdNotice::MarketDataType { req_id, market_data_type: MDT_REALTIME });
            }
        }
    }

    /// What the requests that joined a running subscription get at once
    /// (ibx#444, captured 02/10/2026): the market data type (3 on delayed
    /// data), the request parameters the subscription has, then, when
    /// another request of the contract has the same news key, its latest
    /// headlines (at most 5, oldest first). The quote fields follow at the
    /// next poll.
    pub fn take_md_joins(&self) -> Vec<MdNotice> {
        let joins = std::mem::take(&mut *self.md_joins.lock().unwrap());
        let mut notices = Vec::new();
        for (req_id, delayed) in joins {
            let Some(instrument) = self.req_to_instrument.lock().unwrap().get(&req_id).copied() else { continue };
            if delayed {
                notices.push(MdNotice::MarketDataType { req_id, market_data_type: 3 });
            }
            let params = self.instrument_params.lock().unwrap().get(&instrument).cloned();
            if let Some((min_tick, bbo_exchange, permissions)) = params
                && self.tick_req_params_sent.lock().unwrap().insert(req_id)
            {
                if let Some(market_data_type) = self.check_mdt_needed(req_id, true) {
                    notices.push(MdNotice::MarketDataType { req_id, market_data_type });
                }
                notices.push(MdNotice::TickReqParams { req_id, min_tick, bbo_exchange, permissions });
            }
            let news = self.md_news.lock().unwrap();
            let Some(key) = news.get(&req_id) else { continue };
            let shared_key = self.md_requests_of(instrument).iter().any(|r| *r != req_id && news.get(r) == Some(key));
            if !shared_key {
                continue;
            }
            let stored = self.instrument_news.lock().unwrap();
            let mut latest: Vec<&crate::types::TickNews> = stored.get(&instrument).into_iter().flatten()
                .filter(|n| news_key_covers(key, &n.provider_code)).collect();
            // By time, the order they came for the same time.
            latest.sort_by_key(|n| n.timestamp);
            let skip = latest.len().saturating_sub(NEWS_REPLAYED);
            for n in &latest[skip..] {
                notices.push(MdNotice::News { req_id, news: (*n).clone() });
            }
        }
        notices
    }

    /// The requests a headline of an instrument goes to (ibx#444): those
    /// with the news tick whose key has its provider. The headline is kept
    /// for the requests that join later.
    pub fn route_tick_news(&self, news: &crate::types::TickNews) -> Vec<i64> {
        {
            let mut stored = self.instrument_news.lock().unwrap();
            let kept = stored.entry(news.instrument).or_default();
            if kept.len() >= NEWS_KEPT {
                kept.remove(0);
            }
            kept.push(news.clone());
        }
        let keys = self.md_news.lock().unwrap();
        self.md_requests_of(news.instrument).into_iter()
            .filter(|r| keys.get(r).is_some_and(|key| news_key_covers(key, &news.provider_code)))
            .collect()
    }

    /// Drop the client-side conId cache entries for an instrument id. The
    /// engine may reclaim and reuse the slot after an unsubscribe (ibx#233);
    /// a stale cache entry would silently point the old conId at whatever
    /// contract inherits the id. A later request for that conId simply
    /// re-registers.
    pub fn forget_instrument(&self, instrument: InstrumentId) {
        self.md_slot_con_ids.lock().unwrap().remove(&instrument);
        self.instrument_params.lock().unwrap().remove(&instrument);
        self.instrument_news.lock().unwrap().remove(&instrument);
        let mut sent = self.currency_sent.lock().unwrap();
        self.con_id_to_instrument.lock().unwrap().retain(|con_id, iid| {
            let keep = *iid != instrument;
            if !keep {
                // The engine forgets the slot's currency too.
                sent.remove(con_id);
            }
            keep
        });
    }


    // ── Contract cache ──

    /// Cache a contract for later enrichment.
    pub fn cache_contract(&self, con_id: i64, contract: ApiContract) {
        self.contract_cache.lock().unwrap().insert(con_id, contract);
    }

    /// Look up a contract: merge local cache with shared reference for richest data.
    pub fn get_contract(&self, con_id: i64, shared: &SharedState) -> Option<ApiContract> {
        let local = self.contract_cache.lock().unwrap().get(&con_id).cloned();
        let shared_ref = shared.reference.get_contract(con_id);
        match (local, shared_ref) {
            (Some(mut l), Some(s)) => {
                // Enrich local with shared reference fields (secdef has richer data)
                if l.local_symbol.is_empty() { l.local_symbol = s.local_symbol; }
                if l.trading_class.is_empty() { l.trading_class = s.trading_class; }
                if l.primary_exchange.is_empty() { l.primary_exchange = s.primary_exchange; }
                Some(l)
            }
            (Some(l), None) => Some(l),
            (None, Some(s)) => Some(s),
            (None, None) => None,
        }
    }

    /// Contract for a portfolio row: the cached contract when the session
    /// has one, else the symbol, security type and currency the server's
    /// portfolio row carries. The reference fills these in updatePortfolio;
    /// ibx sent only the contract id for a contract it had not looked up.
    pub fn position_contract(&self, con_id: i64, shared: &SharedState) -> ApiContract {
        self.get_contract(con_id, shared).unwrap_or_else(|| {
            let pi = shared.portfolio.position_info(con_id).unwrap_or_default();
            ApiContract {
                con_id, symbol: pi.symbol, sec_type: pi.sec_type, currency: pi.currency,
                multiplier: pi.multiplier, ..Default::default()
            }
        })
    }

    /// The reference's local refusal of a fundamental data request
    /// (#434): only a stock may be asked, else 321.
    pub fn fundamental_refusal(sec_type: &str) -> Option<(i32, String)> {
        (sec_type != "STK").then(|| (321, "Error validating request.-'bL' : cause - Please enter a valid security type".to_string()))
    }

    /// The local checks of a tick-by-tick request, in the reference's order
    /// (ibx#455): 321 for a combo security type or a tick type that is not
    /// exactly one of the four names, 10189 when the logon turns
    /// tick-by-tick data off. The route and the limit (10190) are checked by
    /// the engine, which knows the streams. The type when the request may
    /// go.
    pub fn tbt_refusal(
        &self,
        shared: &SharedState,
        sec_type: &str,
        tick_type: &str,
        local_symbol: &str,
    ) -> Result<TbtType, (i64, String)> {
        let sec_type = sec_type.to_ascii_uppercase();
        if matches!(sec_type.as_str(), "BAG" | "PDC") {
            return Err((321, format!(
                "Error validating request.-'bT' : cause - '{}' security type is not supported in ReqTickByTick(97) request",
                sec_type)));
        }
        let Some(tbt_type) = TbtType::from_api(tick_type) else {
            return Err((321, "Error validating request.-'bT' : cause - Tick-by-tick data type is incorrect/not set.".to_string()));
        };
        let (_, off) = shared.reference.tick_by_tick_limits();
        if off {
            return Err((10189, format!(
                "Failed to request tick-by-tick data.{} tick-by-tick requests are not supported for {}",
                tbt_type.as_str(), local_symbol)));
        }
        Ok(tbt_type)
    }

    /// Register a TBT subscription mapping.
    pub fn register_tbt(
        &self,
        _shared: &SharedState,
        control_tx: &ControlSender,
        req_id: i64,
        con_id: i64,
        symbol: &str,
        exchange: &str,
        sec_type: &str,
        tbt_type: TbtType,
        number_of_ticks: i32,
        ignore_size: bool,
    ) -> Result<InstrumentId, String> {
        let (reply_tx, reply_rx) = crossbeam_channel::bounded(1);
        control_tx.send(ControlCommand::SubscribeTbt {
            req_id,
            con_id,
            symbol: symbol.to_string(),
            exchange: exchange.to_string(),
            sec_type: sec_type.to_string(),
            tbt_type,
            number_of_ticks,
            ignore_size,
            reply_tx: Some(reply_tx),
        }).map_err(|e| format!("Engine stopped: {}", e))?;

        let instrument_id = Self::recv_registration(reply_rx)?;
        self.con_id_to_instrument.lock().unwrap().insert(con_id, instrument_id);
        self.tbt_reqs.lock().unwrap().insert(req_id, (instrument_id, con_id, tbt_type));
        Ok(instrument_id)
    }

    /// A tick-by-tick request for a contract given without a conId: the
    /// engine looks the contract up first, as the reference does (captured
    /// 05/10/2026: a symbol lookup, then the query with the conId found);
    /// the request has no slot until then.
    pub fn register_tbt_by_symbol(
        &self, control_tx: &ControlSender, req_id: i64, contract: &crate::api::types::Contract,
        tbt_type: TbtType, number_of_ticks: i32, ignore_size: bool,
    ) -> Result<(), String> {
        let request = ControlCommand::SubscribeTbt {
            req_id, con_id: 0, symbol: contract.symbol.clone(), exchange: contract.exchange.clone(),
            sec_type: contract.sec_type.clone(), tbt_type, number_of_ticks, ignore_size, reply_tx: None,
        };
        control_tx.send(Self::resolve_first(req_id, contract, request)).map_err(|e| format!("Engine stopped: {}", e))?;
        self.tbt_reqs.lock().unwrap().insert(req_id, (NO_SLOT, 0, tbt_type));
        Ok(())
    }

    /// End a tick-by-tick request: its instrument, None when unknown. The
    /// conId cache keeps the slot while market data uses it.
    pub fn unregister_tbt(&self, req_id: i64) -> Option<InstrumentId> {
        let (instrument, ..) = self.tbt_reqs.lock().unwrap().remove(&req_id)?;
        if instrument == NO_SLOT {
            return Some(instrument);
        }
        let still_used = self.instrument_to_req.lock().unwrap().contains_key(&instrument)
            || self.tbt_reqs.lock().unwrap().values().any(|(i, ..)| *i == instrument);
        if !still_used {
            self.forget_instrument(instrument);
        }
        Some(instrument)
    }

    /// Look up the first req_id of an instrument; -1 for none.
    pub fn req_id_for_instrument(&self, instrument: InstrumentId) -> i64 {
        self.instrument_to_req.lock().unwrap()
            .get(&instrument).and_then(|reqs| reqs.first()).copied().unwrap_or(-1)
    }

    // ── PnL subscription management ──

    pub fn subscribe_pnl(&self, req_id: i64) {
        self.pnl_reqs.lock().unwrap().entry(req_id).or_insert([PNL_NOT_SENT; 3]);
    }

    pub fn unsubscribe_pnl(&self, req_id: i64) -> bool {
        self.pnl_reqs.lock().unwrap().remove(&req_id).is_some()
    }

    pub fn subscribe_pnl_single(&self, req_id: i64, con_id: i64) {
        self.pnl_single_reqs.lock().unwrap().insert(req_id, con_id);
    }

    pub fn unsubscribe_pnl_single(&self, req_id: i64) -> bool {
        self.last_pnl_single.lock().unwrap().remove(&req_id);
        self.pnl_single_reqs.lock().unwrap().remove(&req_id).is_some()
    }

    /// Check and start a req_pnl (ibx#478), as the reference: an empty or
    /// unknown account gives 321, a request id already running gives 102.
    pub fn request_pnl(&self, req_id: i64, account: &str, own_account: &str) -> Result<(), (i64, String)> {
        pnl_account_refusal("cj", account, own_account)?;
        if self.pnl_reqs.lock().unwrap().contains_key(&req_id) {
            return Err((102, "Duplicate ticker id".into()));
        }
        self.subscribe_pnl(req_id);
        Ok(())
    }

    /// Check and start a req_pnl_single (ibx#478); same checks as req_pnl.
    pub fn request_pnl_single(&self, req_id: i64, account: &str, own_account: &str, con_id: i64) -> Result<(), (i64, String)> {
        pnl_account_refusal("ck", account, own_account)?;
        if self.pnl_single_reqs.lock().unwrap().contains_key(&req_id) {
            return Err((102, "Duplicate ticker id".into()));
        }
        self.subscribe_pnl_single(req_id, con_id);
        Ok(())
    }

    /// cancel_pnl: 10185 for a request id not running (ibx#478).
    pub fn cancel_pnl_request(&self, req_id: i64) -> Option<(i64, String)> {
        (!self.unsubscribe_pnl(req_id)).then(|| (10185, "Failed to cancel PNL (not subscribed)".to_string()))
    }

    /// cancel_pnl_single: 10186 for a request id not running (ibx#478).
    pub fn cancel_pnl_single_request(&self, req_id: i64) -> Option<(i64, String)> {
        (!self.unsubscribe_pnl_single(req_id)).then(|| (10186, "Failed to cancel PNL single (not subscribed)".to_string()))
    }

    // ── Account summary subscription management ──

    /// Check and register an account summary request (ibx#479), as the
    /// reference does: empty tags or group, or a group other than `All` /
    /// `AllNonProp` on a single account, give 321; a third request gives 322.
    /// A request id already running replaces that request.
    pub fn subscribe_account_summary(&self, req_id: i64, group: &str, tags: &str) -> Result<AccountSummaryPlan, (i64, String)> {
        if tags.trim().is_empty() {
            return Err(summary_refusal("Tags cannot be null"));
        }
        if group.is_empty() {
            return Err(summary_refusal("Group name cannot be null"));
        }
        if group != "All" && group != "AllNonProp" {
            return Err(summary_refusal("Group name is invalid"));
        }
        let mut ledger: Vec<String> = Vec::new();
        let mut wire: Vec<&str> = Vec::new();
        for item in tags.split(',').map(str::trim).filter(|t| !t.is_empty()) {
            if let Some(rest) = item.strip_prefix("$LEDGER") {
                let ccy = match rest.find(':') {
                    None => "BASE",
                    Some(k) => &rest[k + 1..],
                };
                if !ledger.iter().any(|c| c == ccy) {
                    ledger.push(ccy.to_string());
                }
                if !wire.contains(&"$LEDGER") {
                    wire.push("$LEDGER");
                }
            } else {
                wire.push(item);
            }
        }
        let mut reqs = self.account_summaries.lock().unwrap();
        let cancel_sr_id = reqs.iter().position(|r| r.req_id == req_id).map(|i| reqs.remove(i).sr_id);
        // The reference's 322 has the rule as its cause (`jextend.dL.a(jextend.b2)@38-72`, ibx#485).
        if reqs.len() >= ACCOUNT_SUMMARY_MAX {
            return Err((322, "Error processing request.-'b2' : cause - Maximum number of account summary requests \
                exceeded; desubscribe to previous request first".into()));
        }
        let sr_id = format!("SR.Socket.{}", self.next_account_summary.fetch_add(1, Ordering::Relaxed));
        reqs.push(AccountSummaryRequest { req_id, sr_id: sr_id.clone(), ledger, ledger_rows: Vec::new() });
        Ok(AccountSummaryPlan { cancel_sr_id, sr_id, wire_tags: wire.join(","), group: group.to_string() })
    }

    /// Forget an account summary request; returns its subscription id to
    /// cancel on the server.
    pub fn unsubscribe_account_summary(&self, req_id: i64) -> Option<String> {
        let mut reqs = self.account_summaries.lock().unwrap();
        let i = reqs.iter().position(|r| r.req_id == req_id)?;
        Some(reqs.remove(i).sr_id)
    }

    // ── Positions subscription (ibx#477) ──

    /// Start req_positions: a subscription, as the reference. One per
    /// client; a new request replaces the running one.
    pub fn subscribe_positions(&self) {
        *self.positions_sub.lock().unwrap() = Some(PositionsSubscription::new());
    }

    pub fn unsubscribe_positions(&self) {
        *self.positions_sub.lock().unwrap() = None;
    }

    /// Position rows to send (ibx#477): the snapshot and the end once the
    /// position data is in; then one row each time a position or its
    /// average cost changes. When the data is not in after 30 s: error 2151
    /// and no end, and the request ends.
    pub fn prepare_positions(&self, shared: &SharedState, account: &str) -> Option<PositionsBatch> {
        let mut guard = self.positions_sub.lock().unwrap();
        let sub = guard.as_mut()?;
        let batch = advance_positions(sub, shared, Some(account));
        if batch.as_ref().is_some_and(|b| b.error.is_some()) {
            *guard = None;
        }
        batch
    }

    // ── Multi-account requests (ibx#476) ──

    /// Start req_positions_multi: the same subscription as req_positions,
    /// with its request id and model code. The same id again replaces it.
    pub fn subscribe_positions_multi(&self, req_id: i64, account: &str, model_code: &str) {
        let mut subs = self.positions_multi.lock().unwrap();
        subs.retain(|m| m.req_id != req_id);
        subs.push(PositionsMultiSubscription {
            req_id, account: account.to_string(), model_code: model_code.to_string(),
            sub: PositionsSubscription::new(),
        });
    }

    pub fn unsubscribe_positions_multi(&self, req_id: i64) {
        self.positions_multi.lock().unwrap().retain(|m| m.req_id != req_id);
    }

    /// Rows of each running req_positions_multi: the snapshot and the end,
    /// then one row per change (ibx#476). No position data after 30 s:
    /// error 2151 for that request, and it ends.
    pub fn prepare_positions_multi(&self, shared: &SharedState) -> Vec<(i64, String, String, PositionsBatch)> {
        let mut subs = self.positions_multi.lock().unwrap();
        let mut out = Vec::new();
        subs.retain_mut(|m| match advance_positions(&mut m.sub, shared, None) {
            Some(batch) => {
                let expired = batch.error.is_some();
                out.push((m.req_id, m.account.clone(), m.model_code.clone(), batch));
                !expired
            }
            None => true,
        });
        out
    }

    /// Start req_account_updates_multi (ibx#476). A request id already
    /// running gives 322 with the duplicate id text as its cause, as the
    /// reference (`jextend.bj.o()@13-36`; ibx#485).
    pub fn subscribe_account_multi(&self, req_id: i64, account: &str, model_code: &str, ledger_and_nlv: bool) -> Result<(), (i64, String)> {
        let mut subs = self.account_multi.lock().unwrap();
        if subs.iter().any(|m| m.req_id == req_id) {
            return Err((322, "Error processing request.-'bj' : cause - Duplicate ticker id".into()));
        }
        subs.push(AccountMultiSubscription {
            req_id, account: account.to_string(), model_code: model_code.to_string(),
            ledger_only: ledger_and_nlv, image_sent: false, sent: HashMap::new(), generation: 0,
        });
        Ok(())
    }

    /// Cancel sends nothing, as the reference.
    pub fn unsubscribe_account_multi(&self, req_id: i64) {
        self.account_multi.lock().unwrap().retain(|m| m.req_id != req_id);
    }

    /// Rows of each running req_account_updates_multi (ibx#476): the
    /// account values (unless ledgerAndNLV) and the ledger rows as the server
    /// sent them, then the end, once the account image is complete; then the
    /// rows that change, with no end.
    pub fn prepare_account_multi(&self, shared: &SharedState) -> Vec<AccountMultiBatch> {
        let mut subs = self.account_multi.lock().unwrap();
        if subs.is_empty() {
            return Vec::new();
        }
        let (generation, complete, _) = shared.portfolio.account_rows_generation();
        if !complete {
            return Vec::new();
        }
        let mut store = None;
        let mut out = Vec::new();
        for m in subs.iter_mut() {
            if m.image_sent && m.generation == generation {
                continue;
            }
            let rows_now = store.get_or_insert_with(|| shared.portfolio.account_rows());
            let mut rows = Vec::new();
            for row in rows_now.rows.iter().filter(|r| !m.ledger_only || r.ledger) {
                let id = (row.key.clone(), row.currency.clone());
                if m.sent.get(&id) != Some(&row.value) {
                    m.sent.insert(id, row.value.clone());
                    rows.push(row.clone());
                }
            }
            let end = !m.image_sent;
            m.image_sent = true;
            m.generation = generation;
            if end || !rows.is_empty() {
                out.push(AccountMultiBatch {
                    req_id: m.req_id, account: m.account.clone(), model_code: m.model_code.clone(), rows, end,
                });
            }
        }
        out
    }

    // ── Account updates subscription management ──

    /// Subscribe to or unsubscribe from account updates. As the reference
    /// (ibx#475): a subscribe while already subscribed changes nothing (no
    /// second image, no second end); an unsubscribe of a subscription
    /// answers error 2100, which the caller reports with id -1.
    pub fn subscribe_account_updates(&self, subscribe: bool) -> Option<(i64, String)> {
        let was = self.account_updates_subscribed.swap(subscribe, Ordering::AcqRel);
        if subscribe {
            if !was {
                *self.account_stream.lock().unwrap() = AccountStream { image_pending: true, ..Default::default() };
                *self.last_portfolio.lock().unwrap() = None;
            }
            return None;
        }
        *self.account_stream.lock().unwrap() = AccountStream::default();
        *self.last_portfolio.lock().unwrap() = None;
        was.then(|| (ACCOUNT_UNSUBSCRIBED.0, ACCOUNT_UNSUBSCRIBED.1.to_string()))
    }

    // ── Market data type tracking ──

    /// Store the requested market data type and give it to the engine
    /// (ibx#447), which sets its modes as the reference does
    /// (`MarketDataModes`): with delayed on (3, 4, and 2 after them), a
    /// subscription the server rejects with delayed data available goes on
    /// delayed; with frozen on (2, until 1), the requests made from then on
    /// ask the contract's market data status and get its frozen data while
    /// it is frozen (`md_freeze`). A value outside 1..=4 is refused with
    /// 321 under id -1, as the reference, and changes nothing.
    pub fn set_market_data_type(&self, control_tx: &ControlSender, mdt: i32) -> Option<(i64, String)> {
        if !(1..=4).contains(&mdt) {
            return Some((321, "Error validating request.-'b0' : cause - Invalid market data type".to_string()));
        }
        self.market_data_type.store(mdt, Ordering::Relaxed);
        self.md_modes.lock().unwrap().apply(mdt);
        let _ = control_tx.send(ControlCommand::SetMarketDataType { market_data_type: mdt });
        None
    }

    /// A request switched to delayed data (ibx#447): its market data type
    /// is reported (3), so the first data does not report realtime, and its
    /// ticks use the delayed tick types.
    pub fn set_delayed(&self, req_id: i64) {
        self.mdt_sent.lock().unwrap().insert(req_id);
        self.delayed_reqs.lock().unwrap().insert(req_id);
        if let Some(st) = self.last_quotes.lock().unwrap().get_mut(&req_id) {
            st.delayed = true;
        }
    }

    /// reqSmartComponents (ibx#441), answered from the exchange maps the
    /// market data acknowledgements made known, as the reference: an
    /// unknown BBO exchange is refused with 321; a known one whose map has
    /// not come yet waits for it up to 2 s (None: the answer comes from
    /// `take_smart_components`).
    pub fn req_smart_components(&self, req_id: i64, bbo_exchange: &str, shared: &SharedState) -> Option<SmartComponentsAnswer> {
        use crate::bridge::ExchangeMapState;
        let (code, sec_type_id) = split_bbo_exchange(bbo_exchange);
        match shared.reference.exchange_map(&code, sec_type_id) {
            ExchangeMapState::Unknown => Some(Err((321,
                "Error validating request.-'V' : cause - Invalid BBO exchange/security type code".to_string()))),
            ExchangeMapState::Ready(map) => Some(Ok(map)),
            ExchangeMapState::Waiting => {
                self.smart_components_waiting.lock().unwrap()
                    .push((req_id, code, sec_type_id, std::time::Instant::now() + SMART_COMPONENTS_WAIT));
                None
            }
        }
    }

    /// The waiting reqSmartComponents whose exchange map came, or whose
    /// wait is over (ibx#441).
    pub fn take_smart_components(&self, shared: &SharedState) -> Vec<(i64, SmartComponentsAnswer)> {
        let mut waiting = self.smart_components_waiting.lock().unwrap();
        if waiting.is_empty() {
            return Vec::new();
        }
        let now = std::time::Instant::now();
        let mut out = Vec::new();
        waiting.retain(|(req_id, code, sec_type_id, deadline)| {
            match shared.reference.exchange_map(code, *sec_type_id) {
                crate::bridge::ExchangeMapState::Ready(map) => out.push((*req_id, Ok(map))),
                _ if now >= *deadline => out.push((*req_id, Err(smart_components_timeout(code, *sec_type_id)))),
                _ => return true,
            }
            false
        });
        out
    }

    /// The tickReqParams to report, (reqId, marketDataType, minTick,
    /// bboExchange, snapshotPermissions): once per request id, as the
    /// reference (ibx#449), for each request of the instrument; a later ack
    /// of the same request (a delayed fallback) gives none. The market data
    /// type, when not sent yet, comes before them: the reference sends it
    /// when the farm grants the subscription, before the request
    /// parameters (ibx#446). The parameters are kept for the requests that
    /// join later (ibx#444).
    pub fn take_tick_req_params(&self, shared: &SharedState) -> Vec<(i64, Option<i32>, f64, String, i64)> {
        let params = shared.market.drain_tick_req_params();
        if params.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        for p in params {
            let permissions = p.snapshot_permissions as i64;
            // The requests of a frozen contract are on its frozen slot
            // (ibx#447).
            let instrument = self.md_frozen.lock().unwrap().get(&p.instrument).copied().unwrap_or(p.instrument);
            self.instrument_params.lock().unwrap().insert(instrument, (p.min_tick, p.bbo_exchange.clone(), permissions));
            for req_id in self.md_requests_of(instrument) {
                if self.tick_req_params_sent.lock().unwrap().insert(req_id) {
                    out.push((req_id, self.check_mdt_needed(req_id, true), p.min_tick, p.bbo_exchange.clone(), permissions));
                }
            }
        }
        out
    }

    /// The news tick check of a market data request whose contract is
    /// known (a conId): error 10094 before anything is sent, as the
    /// reference (ibx#458). A contract without a conId is checked by the
    /// engine once its lookup found it.
    pub fn news_tick_refusal(&self, shared: &SharedState, generic_tick_list: &str, con_id: i64, sec_type: &str) -> Option<(i64, String)> {
        if con_id == 0 {
            return None;
        }
        let codes = match news_tick(generic_tick_list) {
            NewsTick::Default => None,
            NewsTick::Codes(codes) => Some(codes),
            NewsTick::None | NewsTick::Invalid => return None,
        };
        news_providers(sec_type, codes.as_deref(), &shared.reference.news_sources()).err().map(|text| (10094, text))
    }

    /// The 321 of an invalid generic tick list of a market data request
    /// that is not a snapshot (ibx#450, `jextend.bQ.n()@220-388`): checked
    /// first, before anything else of the request. Its "Legal ones" are
    /// those of the security type of the first list refused, as the
    /// reference computes them once.
    pub fn generic_tick_list_refusal(&self, generic_tick_list: &str, snapshot: bool, sec_type: &str) -> Option<(i64, String)> {
        use crate::control::generic_tick;
        if snapshot || generic_tick_list.is_empty() || generic_tick::parse(generic_tick_list, sec_type).is_some() {
            return None;
        }
        let legal = self.generic_legal.lock().unwrap().get_or_insert_with(|| generic_tick::legal_ones(sec_type)).clone();
        Some((321, generic_tick::refusal(generic_tick_list, sec_type, &legal)))
    }

    /// A market data request whose id is already live: error 322, as the
    /// reference (the key is the request id only; ibx#444).
    pub fn duplicate_ticker_refusal(&self, req_id: i64) -> Option<(i64, String)> {
        (self.req_to_instrument.lock().unwrap().contains_key(&req_id)
            || self.md_waiting.lock().unwrap().iter().any(|w| w.req_id == req_id)
            || self.tbt_reqs.lock().unwrap().contains_key(&req_id)
            || self.reg_snapshots.lock().unwrap().iter().any(|f| f.req_id == req_id))
            .then(|| (322, "Error processing request.-'bQ' : cause - Duplicate ticker id".to_string()))
    }

    /// The error and text a client reports for a rejected subscription, and
    /// whether the subscription is gone (ibx#444, ibx#447). The texts are the
    /// reference's; for 354 and 10089 on this path any contract suffix the
    /// reference adds is not captured.
    pub fn md_reject_error(reject: &crate::bridge::MdReject) -> (i64, String, bool) {
        use crate::bridge::MdReject;
        match reject {
            MdReject::Delayed { .. } =>
                (10167, "Requested market data is not subscribed. Displaying delayed market data.".into(), false),
            // The farm's refusal of the top of book: the message, the
            // "delayed available" sentence appended after its period
            // without a space (`jextend.ac.a(String,String)`), then the
            // contract and "/TOP/ALL" (the market data type and the quote;
            // captured 05/10/2026: "...availability of delayed data.Delayed
            // market data is available.7203 TSEJ (7203.T) /TOP/ALL").
            MdReject::NotSubscribed { needs_api_subscription, delayed_available, description, .. } => {
                let (code, base) = if *needs_api_subscription {
                    (10089, "Requested market data requires additional subscription for API. See link in 'Market Data Connections' dialog for more details.")
                } else {
                    (354, MD_NOT_SUBSCRIBED)
                };
                let delayed = if *delayed_available { "Delayed market data is available." } else { "" };
                let suffix = if description.is_empty() { String::new() } else { format!("{description}/TOP/ALL") };
                (code, format!("{base}{delayed}{suffix}"), true)
            }
            MdReject::NoSecurityDefinition { .. } =>
                (200, crate::engine::hot_loop::ccp::NO_SECURITY_DEFINITION.into(), true),
            MdReject::NewsRefused { text, .. } => (10094, text.clone(), true),
            // Not errors: `take_md_rejects` moves the requests (ibx#447).
            MdReject::Frozen { .. } | MdReject::Live { .. } => (0, String::new(), false),
        }
    }

    /// Check if the `market_data_type` callback should fire for this req_id.
    /// Returns `Some(type)` on the first call per req_id that has data, `None`
    /// thereafter. Always reports realtime — the DELIVERED type — rather than
    /// echoing a requested type the engine never transmitted; the old echo
    /// confirmed a state that did not exist (ibx#234).
    pub fn check_mdt_needed(&self, req_id: i64, has_data: bool) -> Option<i32> {
        if has_data && self.mdt_sent.lock().unwrap().insert(req_id) {
            // A request on the frozen slot of a frozen contract (ibx#447).
            let frozen = self.md_frozen.lock().unwrap();
            let on_frozen = !frozen.is_empty()
                && self.req_to_instrument.lock().unwrap().get(&req_id).is_some_and(|i| frozen.values().any(|f| f == i));
            Some(if on_frozen { MDT_FROZEN } else { MDT_REALTIME })
        } else {
            None
        }
    }

    // ── Display groups ──

    /// queryDisplayGroups as the reference (ibx#424): the fixed list of
    /// groups, or the text of its refusal.
    pub fn query_display_groups(req_id: i64) -> Result<&'static str, String> {
        if req_id == i32::MAX as i64 {
            return Err(display_group_refusal("bW", format!("Invalid request ID={req_id}")));
        }
        Ok(DISPLAY_GROUP_LIST)
    }

    /// subscribeToGroupEvents as the reference (ibx#424): the contract of
    /// the group, sent at once, or the text of the refusal. Groups are 1
    /// to 7; a request id subscribes once.
    pub fn subscribe_to_group_events(&self, req_id: i64, group_id: i32) -> Result<&'static str, String> {
        if req_id == i32::MAX as i64 {
            return Err(display_group_refusal("bX", format!("Invalid request ID={req_id}")));
        }
        if !(1..=7).contains(&group_id) {
            return Err(display_group_refusal("bX", format!("Invalid window group ID={group_id}")));
        }
        if !self.display_group_subs.lock().unwrap().insert(req_id) {
            return Err(display_group_refusal("bX", format!("Request with ID={req_id} was already subscribed.")));
        }
        Ok(DISPLAY_GROUP_NO_CONTRACT)
    }

    /// unsubscribeFromGroupEvents as the reference (ibx#424): the text of
    /// the refusal when the request id is not subscribed, else no answer.
    pub fn unsubscribe_from_group_events(&self, req_id: i64) -> Option<String> {
        if req_id == i32::MAX as i64 {
            return Some(display_group_refusal("bY", format!("Invalid request ID={req_id}")));
        }
        if self.display_group_subs.lock().unwrap().remove(&req_id) {
            return None;
        }
        Some(display_group_refusal("bY", format!("Subscription for Group Events with request ID={req_id} wasn't found.")))
    }

    /// updateDisplayGroup as the reference (ibx#424). `contract_info` is
    /// `conid@exch|param1=value1|...|action=(action)`; one token is a group
    /// change, which needs the request id of a subscription. A valid
    /// update changes nothing there and gives no event; `known` tells
    /// whether a conId was seen before. The parameters of the other action
    /// are not checked.
    pub fn update_display_group(&self, req_id: i64, contract_info: &str, known: impl Fn(i64) -> bool) -> DisplayGroupUpdate {
        let refused = |cause: String| DisplayGroupUpdate::Refused(display_group_refusal("bZ", cause));
        if req_id == i32::MAX as i64 {
            return refused(format!("Invalid request ID={req_id}"));
        }
        let mut tokens: Vec<&str> = contract_info.split('|').collect();
        while tokens.len() > 1 && tokens.last().is_some_and(|t| t.is_empty()) {
            tokens.pop();
        }
        let mut change_group = true;
        if tokens.len() > 1 {
            let action = tokens.iter().find_map(|t| {
                t.get(..7).filter(|k| k.eq_ignore_ascii_case("action=")).map(|_| t[7..].trim())
            });
            match action {
                None => return refused(
                    "Action is unknown. Please check the pattern: conid@exch|param1=value1|...|action=(action)".to_string()),
                Some(name) if name.eq_ignore_ascii_case("ChangeGroupEc") => {}
                Some(name) if name.eq_ignore_ascii_case("OpenTS") => change_group = false,
                Some(name) => return refused(format!("Action '{name}' is unknown")),
            }
        }
        let token = tokens[0];
        if change_group {
            if !self.display_group_subs.lock().unwrap().contains(&req_id) {
                return refused(format!("Request with ID={req_id} failed since request ID wasn't found."));
            }
            if token.eq_ignore_ascii_case("none") {
                return DisplayGroupUpdate::Nothing;
            }
        }
        let con_id = match token.split('@').next().unwrap_or("").parse::<i32>() {
            Ok(con_id) => con_id,
            Err(_) => return refused(format!(
                "Request with ID={req_id} failed with invalid contract info={token}, expected format 'contractId@exchange'")),
        };
        if con_id == 0 || con_id == i32::MAX {
            return refused(format!(
                "Request with ID={req_id} failed with invalid contract info={token}: conid or excahge are missing, expected format 'contractId@exchange'"));
        }
        if !change_group || known(con_id as i64) {
            return DisplayGroupUpdate::Nothing;
        }
        DisplayGroupUpdate::Lookup(con_id as i64)
    }

    // ── Bulletin subscription management ──

    /// All bulletin types from now on; with `all_msgs`, the stored
    /// bulletins of the day first (ibx#461).
    pub fn subscribe_bulletins(&self, all_msgs: bool) {
        self.bulletin_subscribed.store(true, Ordering::Release);
        if all_msgs {
            self.bulletin_replay.store(true, Ordering::Release);
        }
    }

    /// Back to popup bulletins only (ibx#461).
    pub fn unsubscribe_bulletins(&self) {
        self.bulletin_subscribed.store(false, Ordering::Release);
    }

    pub fn bulletins_subscribed(&self) -> bool {
        self.bulletin_subscribed.load(Ordering::Acquire)
    }

    /// The bulletins to hand to the client now, as the reference delivers
    /// them (ibx#461): every client gets the popup types (5, 6), a
    /// subscribed client every type; a replay hands over the whole store.
    pub fn bulletins_to_deliver(&self, shared: &SharedState) -> Vec<NewsBulletin> {
        let replay = self.bulletin_replay.swap(false, Ordering::AcqRel);
        let mut list = shared.market.take_news_bulletins(replay);
        if !self.bulletins_subscribed() {
            list.retain(|b| b.msg_type == 5 || b.msg_type == 6);
        }
        list
    }

    // ── Execution replay store ──

    /// Store an execution for `req_executions`. Returns the commission
    /// report that came before it, to send after `exec_details`.
    /// Store an execution of another API client's order (kept for
    /// `req_executions`); its commission report is not given to this
    /// client (`jextend.ba.a(dq, aQ)`).
    pub fn push_silent_execution(&self, contract: ApiContract, execution: ApiExecution, time_secs: Option<i64>) {
        let (base, _) = crate::engine::hot_loop::ccp::split_exec_revision(&execution.exec_id);
        if !base.is_empty() {
            self.silent_executions.lock().unwrap().insert(base.to_string());
        }
        let _ = self.push_execution(-1, contract, execution, time_secs);
    }

    pub fn push_execution(&self, req_id: i64, contract: ApiContract, execution: ApiExecution, time_secs: Option<i64>) -> Option<ApiCommissionAndFeesReport> {
        let (base, _) = crate::engine::hot_loop::ccp::split_exec_revision(&execution.exec_id);
        let commission_and_fees = if execution.exec_id.is_empty() {
            None
        } else {
            self.pending_commissions.lock().unwrap().remove(base)
        };
        self.executions.lock().unwrap().push(StoredExecution {
            req_id, contract, execution, time_secs, commission_and_fees: commission_and_fees.clone(),
        });
        commission_and_fees
    }

    /// Set what the fill report says about this execution (ibx#474): the
    /// server's execution id, the time as the reference writes it, tag 100 /
    /// 207 as exchange, the placing client (this client when the report has
    /// none), modelCode, lastLiquidity, and the orderRef of the report or of
    /// the tracked order. Fields the report did not carry keep their value.
    pub fn apply_fill_exec(&self, ex: &mut ApiExecution, fe: &crate::bridge::FillExec, order_id: OrderId) {
        if !fe.exec_id.is_empty() {
            ex.exec_id = fe.exec_id.clone();
        }
        if let Some(secs) = fe.time_secs {
            ex.time = format_exec_time(secs);
        }
        if !fe.exchange.is_empty() {
            ex.exchange = fe.exchange.clone();
        }
        ex.client_id = if fe.client_id != 0 { fe.client_id } else { self.client_id.load(Ordering::Relaxed) };
        if !fe.model_code.is_empty() {
            ex.model_code = fe.model_code.clone();
        }
        // The report's own: a combo leg's report says how the leg filled,
        // not the combo's report (captured 30/09/2026, i105_combo_fill:
        // 851=1 on the combo, 851=2 on each leg, execDetails 2 for the legs).
        if fe.last_liquidity != 0 {
            ex.last_liquidity = fe.last_liquidity;
        }
        ex.order_ref = if !fe.order_ref.is_empty() {
            fe.order_ref.clone()
        } else {
            self.open_orders.lock().unwrap().get(&order_id)
                .map(|t| t.order.order_ref.clone())
                .unwrap_or_default()
        };
    }

    /// Attach a commission report to its stored execution (a higher revision
    /// of the execution id replaces the report). Returns `true` when the
    /// execution is known, and the report is to be sent now. Otherwise the
    /// report waits for its execution, as the reference sends a live report
    /// only for a known execution (ibx#471).
    pub fn apply_commission(&self, report: &ApiCommissionAndFeesReport) -> bool {
        let (base, _) = crate::engine::hot_loop::ccp::split_exec_revision(&report.exec_id);
        // The commission of another client's execution: kept, not reported.
        let silent = self.silent_executions.lock().unwrap().contains(base);
        let mut execs = self.executions.lock().unwrap();
        let stored = execs.iter_mut().rev().find(|se| {
            !se.execution.exec_id.is_empty()
                && crate::engine::hot_loop::ccp::split_exec_revision(&se.execution.exec_id).0 == base
        });
        match stored {
            Some(se) => {
                se.commission_and_fees = Some(report.clone());
                !silent
            }
            None => {
                self.pending_commissions.lock().unwrap().insert(base.to_string(), report.clone());
                false
            }
        }
    }

    /// Copies of the executions matching the given filter, taken under one
    /// short lock: the caller sends its callbacks after the lock is released
    /// (ibx#265), as the reference builds its list before writing.
    ///
    /// As the reference (ibx#474): symbol, secType and exchange match
    /// exactly; the side is read as buy or sell, so `BUY` matches `BOT`;
    /// clientId 0 is every client; the time keeps executions at or after it.
    /// A time that does not parse is logged and not applied.
    pub fn matching_executions(&self, filter: &ExecutionFilter) -> Vec<StoredExecution> {
        let side = if filter.side.is_empty() { None } else { Some(side_is_buy(&filter.side)) };
        let since = if filter.time.trim().is_empty() {
            None
        } else {
            match crate::config::parse_ib_time(&filter.time) {
                Ok(Some(secs)) => Some(secs),
                _ => {
                    log::warn!("req_executions: filter time '{}' is not a valid time; not applied", filter.time);
                    None
                }
            }
        };
        let execs = self.executions.lock().unwrap();
        execs.iter().filter_map(|se| {
            if !filter.symbol.is_empty() && se.contract.symbol != filter.symbol {
                return None;
            }
            if !filter.sec_type.is_empty() && se.contract.sec_type != filter.sec_type {
                return None;
            }
            if !filter.exchange.is_empty() && se.execution.exchange != filter.exchange {
                return None;
            }
            if let Some(buy) = side {
                if buy.is_none() || buy != side_is_buy(&se.execution.side) {
                    return None;
                }
            }
            if let Some(since) = since {
                if !se.time_secs.is_some_and(|t| t >= since) {
                    return None;
                }
            }
            if !filter.acct_code.is_empty() && !se.execution.acct_number.eq_ignore_ascii_case(&filter.acct_code) {
                return None;
            }
            if filter.client_id != 0 && se.execution.client_id != filter.client_id {
                return None;
            }
            Some(se.clone())
        }).collect()
    }

    // ── Open order tracking ──

    /// Check if an order with this ID is currently tracked (for modify detection).
    pub fn is_order_tracked(&self, order_id: OrderId) -> bool {
        self.open_orders.lock().unwrap().contains_key(&order_id)
    }

    /// Track a newly placed order. For a modify of a tracked order, the
    /// status and fill counts stay as the server last reported them: the
    /// engine can still refuse the modify (ibx#463).
    pub fn track_order(&self, order_id: OrderId, contract: ApiContract, mut order: ApiOrder, instrument: InstrumentId) {
        // Kept with the time in force the reference reports for it, which
        // a modify must restate (ibx#467).
        order.tif = Self::held_tif(&order).to_string();
        self.note_order_id(order_id);
        let mut orders = self.open_orders.lock().unwrap();
        if let Some(o) = orders.get_mut(&order_id) {
            o.contract = contract;
            o.order = order;
            o.instrument = instrument;
            self.modified_orders.lock().unwrap().insert(order_id);
            return;
        }
        let remaining = order.total_quantity;
        orders.insert(order_id, TrackedOrder {
            contract, order, status: "PendingSubmit".into(), filled: 0.0, remaining, instrument,
            last_fill_price: 0.0,
        });
    }

    /// The order as `open_order` reports it after a server report, with
    /// `status`: the tracked order as the caller placed it when this client
    /// placed it, else the order the server reports; the order state of the
    /// latest report (ibx#473). `None` for an order known to neither.
    /// The view of a filled order no longer tracked shows the placing
    /// client of the fill's report (6119), as the reference (captured
    /// 30/09/2026, i105_combo_fill: the leg fills after the combo's Filled
    /// report, openOrder and orderStatus with clientId 198).
    pub fn report_client(view: &mut Option<OrderView>, fe: &crate::bridge::FillExec) {
        if let Some(v) = view.as_mut()
            && v.client_id == 0
            && fe.client_id != 0
        {
            v.client_id = fe.client_id;
            v.order.client_id = fe.client_id as i32;
        }
    }

    pub fn order_view(&self, order_id: OrderId, shared: &SharedState, status: &str) -> Option<OrderView> {
        let tracked = self.open_orders.lock().unwrap().get(&order_id).cloned();
        let info = shared.orders.get_order_info(order_id);
        // The conId of the report, for an order placed without one: the
        // reference shows the contract it looked up (ibx#486).
        let reported_con_id = info.as_ref().map_or(0, |i| i.contract.con_id);
        let mut state = info.as_ref().map(|i| i.order_state.clone()).unwrap_or_default();
        state.status = status.into();
        let (contract, order, last_fill_price, client_id) = match (tracked, info) {
            (Some(t), info) => {
                let mut order = t.order;
                order.order_id = order_id;
                if let Some(i) = &info {
                    if order.perm_id == 0 { order.perm_id = i.order.perm_id; }
                    if order.account.is_empty() { order.account = i.order.account.clone(); }
                    reported_trail_limit(&mut order, &i.order);
                }
                reported_price_mgmt(&mut order, info.as_ref().map(|i| &i.order));
                // The order of this client carries its client id, as the
                // reference's openOrder (ibx#486).
                let client_id = self.client_id.load(Ordering::Relaxed);
                order.client_id = client_id as i32;
                // Outside RTH as sent (not when it was dropped with 2109), or
                // as a report gave it.
                // A PEG BENCH order's starting price is its 99 on the wire;
                // the reference's openOrder shows it as auxPrice (ibx#486,
                // rth_order_types of 28/09/2026: 718.26, then 718.36 after
                // the replace).
                if order.order_type.eq_ignore_ascii_case("PEG BENCH")
                    && (order.aux_price == 0.0 || order.aux_price == f64::MAX)
                    && order.starting_price != f64::MAX && order.starting_price != 0.0
                {
                    order.aux_price = order.starting_price;
                }
                let dropped = self.rth_dropped.lock().unwrap().contains(&order_id);
                order.outside_rth = (order.outside_rth && !dropped)
                    || info.as_ref().is_some_and(|i| i.order.outside_rth);
                let modified = self.modified_orders.lock().unwrap().contains(&order_id);
                session_order_fields(&mut order, &shared.reference.user_name(), modified);
                (t.contract, order, t.last_fill_price, client_id)
            }
            // An order the server reported: its own client id, and no
            // submitter (ibx#519).
            (None, Some(i)) => {
                let client_id = i.order.client_id as i64;
                let mut order = i.order;
                // An order this client placed and saw end is still an
                // order of the session (the leg fills after a combo's
                // Filled report).
                order.submitter.clear();
                if self.finished_orders.lock().unwrap().contains(&order_id) {
                    let modified = self.modified_orders.lock().unwrap().contains(&order_id);
                    session_order_fields(&mut order, &shared.reference.user_name(), modified);
                }
                (i.contract, order, 0.0, client_id)
            }
            (None, None) => return None,
        };
        let placed_exchange = contract.exchange.clone();
        let bag = contract.sec_type.eq_ignore_ascii_case("BAG");
        let mut contract = if contract.con_id != 0 && !bag {
            self.get_contract(contract.con_id, shared).unwrap_or(contract)
        } else if reported_con_id != 0 && !bag {
            self.get_contract(reported_con_id, shared).unwrap_or(contract)
        } else {
            contract
        };
        placed_contract(&mut contract, &placed_exchange);
        let mut order = order;
        reported_unset_values(&mut order);
        Self::apply_combo_view(order_id, &mut contract, &mut order, shared);
        // The API order id the reference shows (0 for an order of another
        // session whose report gave none).
        order.order_id = shared.orders.api_order_id(order_id);
        Some(OrderView { contract, order, state, last_fill_price, client_id })
    }

    /// An order error on its way to the caller: a 2109 (outside RTH
    /// ignored) marks the order as sent without it (ibx#486, the STP and
    /// TRAIL orders of premarket_order_types, 28/09/2026: openOrder shows
    /// outsideRth false, also after a replace).
    pub fn note_order_error(&self, order_id: OrderId, code: i64) {
        if code == 2109 {
            self.rth_dropped.lock().unwrap().insert(order_id);
        }
        // The redirect precaution discards the order (ibx#486).
        if matches!(code, 10311 | 10329) {
            self.discarded.lock().unwrap().insert(order_id);
        }
    }

    /// orderStatus whyHeld, as the reference's `jclient.pe.iK()`: only for
    /// a PreSubmitted order (states Acked and Pending), the reasons joined
    /// with commas: "child" for an order whose parent is not done with a
    /// fill (`pe.ie()`: a parent not Filled or Cancelled with a filled
    /// quantity, `pe.i5()`), "trigger" for an order type that waits for
    /// its trigger (`jibtypes.s.q()`: STP, STP LMT, STP PRT, TRAIL, TRAIL
    /// LIMIT). Captured 26/09/2026 (bracket: "child", "child,trigger") and
    /// 28/09/2026 (premarket_order_types: "trigger" for STP and TRAIL).
    /// The third reason, "locate" (a short sale with the server's allowed
    /// quantity 6365), is not given: no recorded report carries 6365.
    pub fn why_held(&self, status: &str, order_type: &str, parent_id: i64) -> String {
        if status != "PreSubmitted" {
            return String::new();
        }
        let mut why: Vec<&str> = Vec::new();
        if parent_id != 0 {
            let parent_done = self.last_reports.lock().unwrap().get(&parent_id)
                .is_some_and(|r| matches!(r.status.as_str(), "Filled" | "Cancelled") && r.filled > 0.0);
            if !parent_done {
                why.push("child");
            }
        }
        if ["STP", "STP LMT", "STP PRT", "TRAIL", "TRAIL LIMIT"].iter().any(|t| order_type.eq_ignore_ascii_case(t)) {
            why.push("trigger");
        }
        why.join(",")
    }

    /// Keep the openOrder and orderStatus just given for an order.
    pub fn remember_report(&self, order_id: OrderId, report: OrderReport) {
        self.last_reports.lock().unwrap().insert(order_id, report);
    }

    /// The order of a commission report that `apply_commission` took, with
    /// the openOrder and orderStatus last given for it: the reference gives
    /// them again before the commission report (ibx#486; every commission
    /// frame of the four-leg recordings of 28/09 and 30/09/2026).
    pub fn report_of_commission(&self, report: &ApiCommissionAndFeesReport) -> Option<(OrderId, OrderReport)> {
        let (base, _) = crate::engine::hot_loop::ccp::split_exec_revision(&report.exec_id);
        let order_id = self.executions.lock().unwrap().iter().rev()
            .find(|se| !se.execution.exec_id.is_empty()
                && crate::engine::hot_loop::ccp::split_exec_revision(&se.execution.exec_id).0 == base)
            .map(|se| se.execution.order_id)?;
        let last = self.last_reports.lock().unwrap().get(&order_id).cloned()?;
        Some((order_id, last))
    }

    /// Keep the price of an order's last print for later order_status
    /// callbacks (ibx#473).
    pub fn record_last_fill_price(&self, order_id: OrderId, price: f64) {
        if let Some(o) = self.open_orders.lock().unwrap().get_mut(&order_id) {
            o.last_fill_price = price;
        }
    }

    /// Update a tracked order after a fill. Removes the order if fully filled.
    pub fn update_order_fill(&self, order_id: OrderId, status: &str, filled: f64, remaining: f64) {
        let mut orders = self.open_orders.lock().unwrap();
        if remaining == 0.0 {
            if orders.remove(&order_id).is_some() {
                self.finished_orders.lock().unwrap().insert(order_id);
            }
        } else if let Some(o) = orders.get_mut(&order_id) {
            o.status = status.into();
            o.filled = filled;
            o.remaining = remaining;
        }
    }

    /// Drop a tracked order with no status: the engine no longer knows it
    /// (filled while the auth link was lost, ibx#251). It is no open order
    /// and no finished one.
    pub fn forget_order(&self, order_id: OrderId) {
        self.open_orders.lock().unwrap().remove(&order_id);
    }

    /// Update a tracked order status from an order update event.
    pub fn update_order_status(&self, order_id: OrderId, status: &str, filled: f64, remaining: f64) {
        let mut orders = self.open_orders.lock().unwrap();
        if let Some(o) = orders.get_mut(&order_id) {
            o.status = status.into();
            o.filled = filled;
            o.remaining = remaining;
            if matches!(status, "Filled" | "Cancelled") {
                self.finished_orders.lock().unwrap().insert(order_id);
            }
        }
        // An order that never left (a global cancel came while it
        // waited), or one the redirect precaution discarded (ibx#486; its
        // id placed again gets 103, captured 28/09/2026): the reference
        // never put it in its book, so its id names no order any more.
        let discarded = status == "Cancelled" && self.discarded.lock().unwrap().remove(&order_id);
        if status == "ApiCancelled" || discarded {
            orders.remove(&order_id);
            self.finished_orders.lock().unwrap().remove(&order_id);
        }
    }

    /// The reference's answers to the order id of `place_order`.
    ///
    /// Its id check (`jextend.bH.W()`) looks the id up in the client's
    /// order book (`jclient.jv.b(int, int)`): an order found there goes on
    /// (a modify, or a what-if preview). Any other id must be above the
    /// highest id the client placed (`jextend.H.b(int)`), else error 103
    /// and nothing sent (`bH.W()@222`).
    ///
    /// The reference looks each order's contract up on the server before
    /// it puts the order in its book, so an order just placed is not there
    /// for about one server round trip: a what-if sent right after a live
    /// order with the same id got 103 (ibx#462, captured 02/10/2026). ibx
    /// counts a tracked order as not in the book until the server answered
    /// it, for a what-if; a modify of such an order goes on as before.
    ///
    /// An order this client tracked that is filled or cancelled: error
    /// 104 and nothing sent (ibx#463); a what-if is not a modify and
    /// previews it. A pending cancel is checked by the engine, which holds
    /// the current status.
    pub fn refusal_for_order_id(&self, order_id: OrderId, order: &ApiOrder, shared: &SharedState) -> Option<(i64, String)> {
        let duplicate = || Some((103, "Duplicate order id".to_string()));
        if self.finished_orders.lock().unwrap().contains(&order_id) {
            if order.what_if {
                return None;
            }
            let (code, message) = MODIFY_OF_FINISHED_ORDER;
            return Some((code, message.into()));
        }
        let pending = self.open_orders.lock().unwrap().get(&order_id).map(|t| t.status == "PendingSubmit");
        if let Some(pending) = pending {
            return if order.what_if && pending { duplicate() } else { None };
        }
        // A new id: above the highest placed one, unless the server knows
        // the order (one of an earlier session).
        if order_id > 0 && order_id <= self.highest_used_order_id(shared)
            && shared.orders.get_order_info(order_id).is_none()
        {
            return duplicate();
        }
        None
    }

    /// Start keeping this client's highest order id on disk, raised first
    /// to the value its earlier sessions saved (ibx#518). Called at the
    /// connect, once the client id is known; a file that cannot be used is
    /// logged and the client goes on without it.
    pub fn keep_order_ids(&self, account: &str) {
        let Some(path) = crate::order_ids::default_path() else { return };
        let client_id = self.client_id.load(Ordering::Relaxed);
        let mut saver = self.order_id_saver.lock().unwrap();
        // The saver of the last connect saves its value before the new one reads.
        *saver = None;
        match crate::order_ids::Saver::start(path, account, client_id, self.highest_order_id.clone()) {
            Ok(s) => *saver = Some(s),
            Err(e) => log::warn!("Order ids are not kept across sessions: {}", e),
        }
    }

    /// Save this client's highest order id and stop keeping it (at the
    /// disconnect).
    pub fn stop_keeping_order_ids(&self) {
        *self.order_id_saver.lock().unwrap() = None;
    }

    /// Note an order id this client placed: the highest one bounds the
    /// ids of new orders (ibx#462).
    pub fn note_order_id(&self, order_id: OrderId) {
        self.highest_order_id.fetch_max(order_id, Ordering::AcqRel);
    }

    /// The highest order id this client used, as the reference keeps it
    /// per client (`jextend.H`): the ids it placed, and the ids the
    /// server's reports gave for its client id (the orders of its earlier
    /// sessions the logon replay shows). 0 for none.
    pub fn highest_used_order_id(&self, shared: &SharedState) -> OrderId {
        let me = self.client_id.load(Ordering::Relaxed);
        self.highest_order_id.load(Ordering::Acquire).max(shared.orders.reported_order_id(me))
    }

    /// The next valid order id, as the reference gives it in nextValidId
    /// at the connect and to reqIds (`jextend.dK.aC()` = `jextend.H.c()`):
    /// the highest order id the client used + 1, 1 when none. reqIds
    /// reserves nothing (ibx#466).
    pub fn next_valid_id(&self, shared: &SharedState) -> OrderId {
        self.highest_used_order_id(shared) + 1
    }

    /// The next order id for a new order (`next_order_id`): the next
    /// valid id, or above the ids handed out before, which it reserves.
    pub fn take_order_id(&self, shared: &SharedState) -> OrderId {
        let floor = self.next_valid_id(shared);
        let mut current = self.reserved_order_id.load(Ordering::Acquire);
        loop {
            let id = current.max(floor);
            match self.reserved_order_id.compare_exchange_weak(current, id + 1, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return id,
                Err(now) => current = now,
            }
        }
    }

    /// The reference sends nextValidId once the orders of the logon are
    /// known (`jextend.dL.bp()@34-48` waits for the order list): wait for
    /// the end of the order replay of the logon, so the ids of the earlier
    /// sessions count, at most `ORDER_REPLAY_WAIT`.
    pub fn wait_order_replay(shared: &SharedState) {
        let deadline = std::time::Instant::now() + ORDER_REPLAY_WAIT;
        while shared.orders.open_orders_held() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// Hold an open-order request from the logon, or from a lost auth link,
    /// until the order replay of that logon has ended, as in the reference
    /// (ibx#251). A second request of the same kind replaces the
    /// first. Returns false when the request is to be answered now.
    pub fn hold_open_orders(&self, request: OpenOrdersRequest, shared: &SharedState) -> bool {
        if !shared.orders.open_orders_held() {
            return false;
        }
        let mut held = self.held_open_orders.lock().unwrap();
        if !held.contains(&request) {
            held.push(request);
        }
        true
    }

    /// The held open-order requests to answer now, in the order they were
    /// made: none while the replay has not ended. Call it before the order
    /// updates are dispatched, so the answer has the replayed statuses
    /// (ibx#251).
    pub fn released_open_orders(&self, shared: &SharedState) -> Vec<OpenOrdersRequest> {
        if shared.orders.open_orders_held() {
            return Vec::new();
        }
        std::mem::take(&mut *self.held_open_orders.lock().unwrap())
    }

    /// Collect open orders: merge local tracking with shared state.
    /// Returns (order_id, contract, order, status, filled, remaining) for non-terminal orders.
    pub fn collect_open_orders(&self, shared: &SharedState) -> Vec<(OrderId, TrackedOrder)> {
        let mut result: Vec<(OrderId, TrackedOrder)> = Vec::new();

        // Drain shared order cache first to enrich local tracking
        let shared_orders = shared.orders.drain_open_orders();
        {
            let mut orders = self.open_orders.lock().unwrap();
            for (oid, info) in &shared_orders {
                if let Some(o) = orders.get_mut(oid) {
                    if o.order.account.is_empty() {
                        o.order.account = info.order.account.clone();
                    }
                    if o.order.perm_id == 0 {
                        o.order.perm_id = info.order.perm_id;
                    }
                }
            }
        }

        // Local tracked orders (non-terminal), enriched from secdef cache
        {
            let orders = self.open_orders.lock().unwrap();
            for (&oid, o) in orders.iter() {
                if is_open_status(&o.status) {
                    let info = shared.orders.get_order_info(oid);
                    // An order placed without a conId shows the contract the
                    // server reported (captured 05/10/2026, reqOpenOrders of
                    // two TRAIL orders on SPY placed by symbol).
                    let con_id = if o.contract.con_id != 0 {
                        o.contract.con_id
                    } else {
                        info.as_ref().map_or(0, |i| i.contract.con_id)
                    };
                    let mut contract = if con_id != 0 {
                        self.get_contract(con_id, shared)
                            .or_else(|| info.as_ref().map(|i| i.contract.clone()).filter(|c| c.con_id == con_id))
                            .unwrap_or_else(|| o.contract.clone())
                    } else {
                        o.contract.clone()
                    };
                    let mut order = o.order.clone();
                    if let Some(info) = &info {
                        reported_trail_limit(&mut order, &info.order);
                    }
                    reported_price_mgmt(&mut order, info.as_ref().map(|i| &i.order));
                    let modified = self.modified_orders.lock().unwrap().contains(&oid);
                    session_order_fields(&mut order, &shared.reference.user_name(), modified);
                    placed_contract(&mut contract, &o.contract.exchange);
                    reported_unset_values(&mut order);
                    Self::apply_combo_view(oid, &mut contract, &mut order, shared);
                    result.push((oid, TrackedOrder {
                        contract,
                        order,
                        status: o.status.clone(),
                        filled: o.filled,
                        remaining: o.remaining,
                        instrument: o.instrument,
                        last_fill_price: o.last_fill_price,
                    }));
                }
            }
        }

        // Add shared-only entries not already present from local
        for (oid, info) in shared_orders {
            if !is_open_status(&info.order_state.status) {
                continue;
            }
            if !result.iter().any(|(id, _)| *id == oid) {
                let contract = if info.contract.con_id != 0 {
                    shared.reference.get_contract(info.contract.con_id).unwrap_or(info.contract)
                } else {
                    info.contract
                };
                let mut order = info.order;
                order.submitter.clear();
                reported_unset_values(&mut order);
                result.push((oid, TrackedOrder {
                    contract,
                    order,
                    status: info.order_state.status.clone(),
                    filled: 0.0,
                    remaining: 0.0,
                    instrument: 0,
                    last_fill_price: 0.0,
                }));
            }
        }

        result
    }

    /// The answer to an open-order request, as the reference lists it
    /// (`jextend.cs.o()`): the orders of the book in its order
    /// (`jclient.jv.w()`, a hash table keyed by permId, walked bucket by
    /// bucket in insertion order; captured 01/10/2026), every client's for
    /// reqAllOpenOrders, this client's for reqOpenOrders
    /// (`jextend.cu.b(List)@149`: `pe.T() == clientId`). Each with the
    /// order id and client id the reference shows.
    pub fn open_orders_listing(&self, shared: &SharedState, request: OpenOrdersRequest) -> Vec<(OrderId, TrackedOrder, i64)> {
        let me = self.client_id.load(Ordering::Relaxed);
        let tracked: HashSet<OrderId> = self.open_orders.lock().unwrap().keys().copied().collect();
        let mut out: Vec<(OrderId, TrackedOrder, i64, (usize, u64))> = Vec::new();
        for (oid, mut t) in self.collect_open_orders(shared) {
            let client_id = if tracked.contains(&oid) { me } else { t.order.client_id as i64 };
            if request == OpenOrdersRequest::Open && client_id != me {
                continue;
            }
            if !tracked.contains(&oid) {
                // An order the server reported: what is left of it.
                t.filled = t.order.filled_quantity.max(0.0);
                t.remaining = (t.order.total_quantity - t.filled).max(0.0);
            }
            let (seq, peak) = shared.orders.book_place(oid);
            let perm = if t.order.perm_id != 0 { t.order.perm_id } else { oid };
            let place = (crate::engine::context::book_bucket(perm, crate::engine::context::book_table_size(peak)),
                seq.unwrap_or(u64::MAX));
            let shown = shared.orders.api_order_id(oid);
            t.order.order_id = shown;
            t.order.client_id = client_id as i32;
            out.push((shown, t, client_id, place));
        }
        out.sort_by_key(|(.., place)| *place);
        out.into_iter().map(|(id, t, c, _)| (id, t, c)).collect()
    }

    // ── Dispatch preparation methods ──

    /// Forget the plain snapshot of the request, if any.
    fn end_snapshot(&self, req_id: i64) {
        if self.snapshot_count.load(Ordering::Acquire) == 0 {
            return;
        }
        let mut snaps = self.snapshot_reqs.lock().unwrap();
        snaps.remove(&req_id);
        self.snapshot_count.store(snaps.len(), Ordering::Release);
    }

    /// The local refusal of a snapshot request (ibx#446), before the
    /// duplicate check, as the reference checks it: generic ticks, then the
    /// per-second limit, which counts the request when it lets it go.
    pub fn snapshot_refusal(&self, shared: &SharedState, generic_tick_list: &str, sec_type: &str) -> Option<(i64, String)> {
        use crate::control::snapshot;
        if snapshot::generic_ticks_refused(generic_tick_list, sec_type) {
            return Some((321, snapshot::GENERIC_TICKS_REFUSED.to_string()));
        }
        let limit = shared.reference.snapshot_rate_limit();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64).unwrap_or(0);
        if !self.snapshot_rate.lock().unwrap().allow(now, limit) {
            return Some((321, snapshot::rate_refusal(limit)));
        }
        None
    }

    /// Snapshot the current instrument→req_id mapping: each request of
    /// each instrument, in the order they came.
    pub fn snapshot_instruments(&self) -> Vec<(InstrumentId, i64)> {
        let map = self.instrument_to_req.lock().unwrap();
        map.iter().flat_map(|(&iid, reqs)| reqs.iter().map(move |&req_id| (iid, req_id))).collect()
    }

    /// Poll PnL and return update if values changed.
    /// Computes daily P&L client-side from midnight seeds + live quotes.
    /// Formula: dailyPnL = Σ(qtyNow × priceNow - qtyMidnight × prevClose - moneyTraded)
    /// For positions opened intraday (no seed), synthesizes
    /// moneyTraded = qtyNow × avgCost so the formula collapses to unrealized P&L.
    pub fn poll_pnl(&self, shared: &SharedState) -> Vec<PnlUpdate> {
        if self.pnl_reqs.lock().unwrap().is_empty() {
            return Vec::new();
        }
        let realized_since = shared.portfolio.realized_since_seed();
        let money_since = shared.portfolio.money_since_seed();

        let seeds: HashMap<i64, MidnightSeed> = shared.portfolio.midnight_seeds()
            .into_iter().map(|s| (s.con_id, s)).collect();
        let positions = shared.portfolio.position_infos();

        let mut con_ids: HashSet<i64> = seeds.keys().copied().collect();
        for pi in &positions {
            con_ids.insert(pi.con_id);
        }
        con_ids.extend(realized_since.keys().copied());
        con_ids.extend(money_since.keys().copied());
        // Sum in a fixed order: a set's order changes between polls, and the
        // float sums with it, so the same P&L passed the change filter again
        // and again (a callback every poll, seen on paper).
        let mut con_ids: Vec<i64> = con_ids.into_iter().collect();
        con_ids.sort_unstable();
        // No early return on an empty set: an account with no position still
        // has account-level P&L (for example realized today), delivered by the
        // fallback below (ibx#239, ibx#301).

        let con_id_map = self.con_id_to_instrument.lock().unwrap();
        let mut total_daily: f64 = 0.0;
        let mut total_unrealized: f64 = 0.0;
        let mut total_realized: f64 = 0.0;
        let mut live_priced = 0usize;
        let mut daily_known = true;
        let mut unrealized_known = true;

        for con_id in con_ids {
            let seed = seeds.get(&con_id);
            let pi = shared.portfolio.position_info(con_id);
            // Positions are fixed-point; the P&L works in shares.
            let qty_now = pi.as_ref().map(|p| p.position_fixed).unwrap_or(0) as f64 / QTY_SCALE_F;
            let avg_cost = pi.as_ref().map(|p| p.avg_cost).unwrap_or(0);
            let qty_midnight = seed.map(|s| s.qty_midnight_fixed).unwrap_or(0) as f64 / QTY_SCALE_F;

            // The seed's realized P&L, plus this session's fills (ibx#478).
            total_realized += seed.map(|s| s.realized_pnl).unwrap_or(0.0)
                + realized_since.get(&con_id).copied().unwrap_or(0.0);

            let money_traded = money_traded_today(seed, money_since.get(&con_id).copied(), qty_now, avg_cost);
            // A position flat at midnight and now: its daily P&L is its cash.
            if qty_now == 0.0 && qty_midnight == 0.0 {
                total_daily += money_traded;
                continue;
            }

            // Price: this client's quote, else the server's mark on the
            // position (ibx#238). The previous close comes with a quote only.
            let quote = con_id_map.get(&con_id).map(|&iid| shared.market.quote(iid));
            let live = quote.map(|q| q.last).filter(|&p| p != 0);
            if live.is_some() {
                live_priced += 1;
            }
            let Some(price_now) = live.or_else(|| pi.as_ref().map(|p| p.market_price).filter(|&p| p != 0)) else {
                daily_known = false;
                unrealized_known = false;
                continue;
            };
            let prev_close = quote.map(|q| q.close).unwrap_or(0);

            // Daily P&L = value change since midnight plus today's net cash.
            // A position held at midnight needs the previous close.
            if qty_midnight != 0.0 && prev_close == 0 {
                daily_known = false;
            } else {
                let mv_now = qty_now * price_now as f64 / PRICE_SCALE_F;
                let mv_midnight = qty_midnight * prev_close as f64 / PRICE_SCALE_F;
                total_daily += mv_now - mv_midnight + money_traded;
            }

            if qty_now != 0.0 {
                if avg_cost != 0 {
                    total_unrealized += qty_now * (price_now - avg_cost) as f64 / PRICE_SCALE_F;
                } else {
                    unrealized_known = false;
                }
            }
        }

        // No live quote at all: the server's own account P&L keys, when it
        // sends them (ibx#239); the paper account's frames carry none.
        if live_priced == 0 {
            let rows = shared.portfolio.account_rows();
            let server = |key: &str| rows.rows.iter()
                .filter(|r| r.key == key)
                .min_by_key(|r| r.currency != "BASE")
                .and_then(|r| r.value.parse::<f64>().ok());
            if let (Some(daily), Some(unrealized), Some(realized)) =
                (server("DailyPnL"), server("UnrealizedPnL"), server("RealizedPnL"))
            {
                total_daily = daily;
                total_unrealized = unrealized;
                total_realized = realized;
                daily_known = true;
                unrealized_known = true;
            }
        }
        // A value that cannot be computed is unset, as the reference (ibx#478).
        if !daily_known {
            total_daily = f64::MAX;
        }
        if !unrealized_known {
            total_unrealized = f64::MAX;
        }

        let scaled = |v: f64| if v == f64::MAX { i64::MAX } else { (v * PRICE_SCALE_F) as i64 };
        let pnl = [scaled(total_daily), scaled(total_unrealized), scaled(total_realized)];
        // Every running request gets the values; each its own change filter.
        let mut reqs = self.pnl_reqs.lock().unwrap();
        let mut out: Vec<PnlUpdate> = reqs.iter_mut().filter_map(|(&req_id, last)| {
            if *last == pnl {
                return None;
            }
            *last = pnl;
            Some(PnlUpdate {
                req_id,
                daily_pnl: total_daily,
                unrealized_pnl: total_unrealized,
                realized_pnl: total_realized,
            })
        }).collect();
        out.sort_by_key(|u| u.req_id);
        out
    }

    /// Poll per-position PnL and return updates whose values changed.
    /// Routes the quote lookup by con_id (not first-non-zero across all subscriptions),
    /// computes daily/realized from the matching midnight seed, and synthesizes
    /// money_traded = qty_now × avg_cost for intraday-opened positions.
    pub fn poll_pnl_single(&self, shared: &SharedState) -> Vec<PnlSingleUpdate> {
        let reqs: Vec<(i64, i64)> = self.pnl_single_reqs.lock().unwrap()
            .iter().map(|(&r, &c)| (r, c)).collect();
        if reqs.is_empty() {
            return Vec::new();
        }

        let seeds: HashMap<i64, MidnightSeed> = shared.portfolio.midnight_seeds()
            .into_iter().map(|s| (s.con_id, s)).collect();
        let realized_since = shared.portfolio.realized_since_seed();
        let money_since = shared.portfolio.money_since_seed();
        let con_id_map = self.con_id_to_instrument.lock().unwrap();
        let mut last_cache = self.last_pnl_single.lock().unwrap();
        let mut results = Vec::new();

        for (req_id, con_id) in reqs {
            let Some(pi) = shared.portfolio.position_info(con_id) else { continue; };
            // Positions are fixed-point; the P&L works in shares.
            let qty_fx = pi.position_fixed;
            let qty_now = qty_fx as f64 / QTY_SCALE_F;
            let avg_cost = pi.avg_cost;

            // Price from this client's market-data subscription when there is
            // one, else the server's own mark on the position (ibx#238). A
            // req_pnl_single-only client has no subscription, so without the
            // mark it never got a callback (same gap as ibx#239 for req_pnl).
            let q = con_id_map.get(&con_id).map(|&iid| shared.market.quote(iid));
            let price_now = match q.map(|q| q.last) {
                Some(last) if last != 0 => last,
                _ => pi.market_price,
            };
            if price_now == 0 {
                continue;
            }

            let seed = seeds.get(&con_id);
            let qty_midnight = seed.map(|s| s.qty_midnight_fixed).unwrap_or(0) as f64 / QTY_SCALE_F;
            let prev_close = q.map(|q| q.close).unwrap_or(0);

            let money_traded = money_traded_today(seed, money_since.get(&con_id).copied(), qty_now, avg_cost);

            let mv_now = qty_now * price_now as f64 / PRICE_SCALE_F;
            let mv_midnight = qty_midnight * prev_close as f64 / PRICE_SCALE_F;
            // A position held at midnight needs the previous close, which
            // comes with a quote only: without one, the daily P&L is unset.
            let daily = if qty_midnight != 0.0 && prev_close == 0 {
                f64::MAX
            } else {
                mv_now - mv_midnight + money_traded
            };
            // A value that cannot be computed is unset (f64::MAX), as the
            // reference (ibx#478): no average cost, or no realized P&L on
            // this position today (captured 25/09/2026).
            let unrealized = if avg_cost != 0 {
                qty_now * (price_now - avg_cost) as f64 / PRICE_SCALE_F
            } else { f64::MAX };
            let realized_fills = realized_since.get(&con_id).copied();
            let realized = match (seed.map(|s| s.realized_pnl), realized_fills) {
                (None, None) => f64::MAX,
                (seed_value, fills) => seed_value.unwrap_or(0.0) + fills.unwrap_or(0.0),
            };
            let value = mv_now;

            let scaled = |v: f64| if v == f64::MAX { i64::MAX } else { (v * PRICE_SCALE_F) as i64 };
            let snapshot: [i64; 5] = [
                qty_fx,
                scaled(daily),
                scaled(unrealized),
                scaled(realized),
                scaled(value),
            ];
            if last_cache.get(&req_id) == Some(&snapshot) {
                continue;
            }
            last_cache.insert(req_id, snapshot);

            results.push(PnlSingleUpdate {
                req_id,
                pos: qty_now,
                daily_pnl: daily,
                unrealized_pnl: unrealized,
                realized_pnl: realized,
                value,
            });
        }
        results
    }

    /// Prepare account update fields (subscription-gated, change-detected).
    /// Account values to send (ibx#475). The first batch after a subscribe
    /// is the full image, once the server's image is complete: every value
    /// the server sent, with its text and currency, then the end. Later
    /// batches carry the values that changed, and no end. `None` while not
    /// subscribed or before the image is complete.
    pub fn prepare_account_updates(&self, shared: &SharedState, account: &str) -> Option<AccountUpdateBatch> {
        if !self.account_updates_subscribed.load(Ordering::Acquire) {
            return None;
        }
        let (generation, complete, time_secs) = shared.portfolio.account_rows_generation();
        let mut stream = self.account_stream.lock().unwrap();
        if stream.image_pending && !complete {
            return None;
        }
        let first = stream.image_pending;
        let mut fields = Vec::new();
        if first || generation != stream.generation {
            let store = shared.portfolio.account_rows();
            for row in &store.rows {
                let id = (row.key.clone(), row.currency.clone());
                if stream.sent.get(&id) != Some(&row.value) {
                    stream.sent.insert(id, row.value.clone());
                    fields.push(AccountFieldUpdate {
                        key: row.key.clone(),
                        // The ledger's account row is the subscription's
                        // account (`jaccount.X`, column 23).
                        value: if row.key == "AccountOrGroup" { account.to_string() } else { row.value.clone() },
                        currency: row.currency.clone(),
                    });
                }
            }
            stream.generation = store.generation;
        }
        // The image is the reference's cached values in the order of their
        // map, keyed by key, "!" and currency (`trader.cm.G`, a TreeMap;
        // `jextend.dK.a(String, bl)@40-88`; captured 07/10/2026).
        if first {
            fields.sort_by_cached_key(|f| format!("{}!{}", f.key, f.currency));
        }
        stream.image_pending = false;
        Some(AccountUpdateBatch { fields, time: format_account_time(time_secs), download_end: first })
    }

    /// Prepare portfolio updates (position entries) for account streaming.
    /// Returns changed/new position infos when account updates are subscribed.
    pub fn prepare_portfolio_updates(&self, shared: &SharedState) -> Vec<PortfolioUpdateEntry> {
        // Called after `prepare_account_updates`, so only once the account
        // image is complete (ibx#475).
        if !self.account_updates_subscribed.load(Ordering::Acquire) {
            return Vec::new();
        }

        // In the order the server's portfolio rows first came, as the
        // reference's image (captured 07/10/2026: the rows of the portfolio
        // frame, AAPL, AXTI, MSFT, SPY); a position with no row yet last.
        let mut current = shared.portfolio.position_infos();
        current.sort_by_key(|p| (p.portfolio_seq == 0, p.portfolio_seq, p.con_id));
        let mut prev_guard = self.last_portfolio.lock().unwrap();
        let is_first = prev_guard.is_none();

        let to_entry = |pi: &PositionInfo| PortfolioUpdateEntry {
            con_id: pi.con_id,
            position: pi.position_fixed as f64 / QTY_SCALE_F,
            avg_cost: pi.avg_cost as f64 / PRICE_SCALE_F,
            market_price: pi.market_price as f64 / PRICE_SCALE_F,
            market_value: pi.market_value as f64 / PRICE_SCALE_F,
            unrealized_pnl: pi.unrealized_pnl as f64 / PRICE_SCALE_F,
            realized_pnl: pi.realized_pnl as f64 / PRICE_SCALE_F,
        };

        let changed = if is_first {
            current.iter().map(&to_entry).collect()
        } else {
            let prev = prev_guard.as_ref().unwrap();
            // Marks are part of the row: a mark move (each account-updates
            // snapshot) is a genuine update, so compare them too (ibx#238).
            current.iter().filter(|pi| {
                !prev.iter().any(|pp| pp.con_id == pi.con_id
                    && pp.position_fixed == pi.position_fixed
                    && pp.avg_cost == pi.avg_cost
                    && pp.market_price == pi.market_price
                    && pp.market_value == pi.market_value
                    && pp.unrealized_pnl == pi.unrealized_pnl
                    && pp.realized_pnl == pi.realized_pnl)
            }).map(&to_entry).collect()
        };

        *prev_guard = Some(current);
        changed
    }

    /// Account summary rows and ends the server sent, by request (ibx#479):
    /// every row of the request's subscription, with its text and currency;
    /// the end at each end marker. Rows of a cancelled subscription are
    /// dropped. As the reference (ibx#486, `jextend.l`, `jextend.ef`):
    /// - a ledger frame gives the rows of each account for the currencies
    ///   asked (not `ALL`), in the order of the reference's currency set;
    /// - with `$LEDGER:ALL`, the end marker gives first one row per
    ///   currency summed over the accounts, for the account `All`;
    /// - ledger values are written as the reference's account values
    ///   (`jaccount.X`: two to seven decimals, no grouping);
    /// - the reference registers the request's listener twice
    ///   (`jextend.b2.o()@62`, `@75`: `trader.cm.j.a(aU)` and `c(aU)`), so
    ///   each frame's rows and end are given twice (account_summary of
    ///   26/09/2026: the four tag rows twice; the ledger sums and the end
    ///   twice).
    pub fn prepare_account_summary(&self, shared: &SharedState) -> Vec<AccountSummaryBatch> {
        let events = shared.portfolio.drain_account_summary_events();
        if events.is_empty() {
            return Vec::new();
        }
        let mut reqs = self.account_summaries.lock().unwrap();
        let mut out = Vec::new();
        for event in events {
            let Some(req) = reqs.iter_mut().find(|r| r.sr_id == event.sr_id) else { continue };
            let mut batches: Vec<AccountSummaryBatch> = Vec::new();
            if event.ledger {
                for row in event.ledgers {
                    match req.ledger_rows.iter_mut().find(|r| r.account == row.account && r.currency == row.currency) {
                        Some(kept) => *kept = row,
                        None => req.ledger_rows.push(row),
                    }
                }
                let currencies = java_hash_order(&req.ledger);
                for account in java_hash_order(&accounts_of(&req.ledger_rows)) {
                    let rows: Vec<crate::bridge::AccountRow> = currencies.iter().filter(|c| *c != LEDGER_ALL)
                        .filter_map(|c| req.ledger_rows.iter().find(|r| r.account == account && &r.currency == c))
                        .flat_map(|r| ledger_summary_rows(r, &account))
                        .collect();
                    if !rows.is_empty() {
                        batches.push(AccountSummaryBatch { req_id: req.req_id, account: Some(account.clone()), rows, end: false });
                    }
                }
            } else if !event.rows.is_empty() {
                batches.push(AccountSummaryBatch { req_id: req.req_id, account: None, rows: event.rows, end: false });
            }
            if event.end {
                if req.ledger.iter().any(|c| c == LEDGER_ALL) {
                    let rows: Vec<crate::bridge::AccountRow> = ledger_sums(&req.ledger_rows).iter()
                        .flat_map(|r| ledger_summary_rows(r, "All")).collect();
                    if !rows.is_empty() {
                        batches.push(AccountSummaryBatch { req_id: req.req_id, account: Some("All".into()), rows, end: false });
                    }
                }
                batches.push(AccountSummaryBatch { req_id: req.req_id, account: None, rows: Vec::new(), end: true });
            }
            out.extend(batches.iter().cloned());
            out.extend(batches);
        }
        out
    }

    // ── Order routing ──

    /// The name ibx uses for an order type the reference also accepts under
    /// another name (ibx#469), or None when the name is not one of these.
    pub fn canonical_order_type(order_type: &str) -> Option<&'static str> {
        ORDER_TYPE_ALIASES.iter()
            .find(|(alias, _)| alias.eq_ignore_ascii_case(order_type))
            .map(|&(_, name)| name)
    }

    /// The order with its type under the name ibx uses (ibx#469). Copied
    /// only when the type is one of the other names.
    pub fn with_canonical_order_type(order: &ApiOrder) -> std::borrow::Cow<'_, ApiOrder> {
        let order_type = Self::canonical_order_type(&order.order_type);
        let tif = Self::canonical_tif(&order.tif);
        if order_type.is_none() && tif.is_none() {
            return std::borrow::Cow::Borrowed(order);
        }
        let mut owned = order.clone();
        if let Some(name) = order_type { owned.order_type = name.to_string(); }
        if let Some(name) = tif { owned.tif = name.to_string(); }
        std::borrow::Cow::Owned(owned)
    }

    /// customerAccount or professionalCustomer=true on an account whose
    /// config has no CUSTACCT: error 145 with the reference's text, nothing
    /// sent (captured 28/09/2026, ibx#425). The customer account is checked
    /// first. An unknown config counts as one without CUSTACCT.
    pub fn account_config_refusal(order: &ApiOrder, features: Option<&[String]>, session_account: &str) -> Option<(i64, String)> {
        if order.customer_account.is_empty() && !order.professional_customer {
            return None;
        }
        if features.is_some_and(|f| f.iter().any(|x| x == "CUSTACCT")) {
            return None;
        }
        let account = if order.account.is_empty() { session_account } else { order.account.as_str() };
        let cause = if !order.customer_account.is_empty() {
            format!("Account config doesn't allow to specify customer account value: {} for account {}", order.customer_account, account)
        } else {
            format!("Account config doesn't allow to assign 'true' value for ProfessionalCustomer for account {}", account)
        };
        Some((145, format!("Error in validating entry fields -{}", cause)))
    }

    /// The time in force the reference sends for an API value it does not
    /// send as such: GTX and NMIN go out as GTC and read back as GTC
    /// (captured 28/09/2026, ibx#307).
    pub fn canonical_tif(tif: &str) -> Option<&'static str> {
        (tif.eq_ignore_ascii_case("GTX") || tif.eq_ignore_ascii_case("NMIN")).then_some("GTC")
    }

    /// Pre-validate order fields that don't depend on instrument ID.
    /// Call this before `find_or_register_instrument` to fail fast.
    pub fn validate_order(order: &ApiOrder) -> Result<(), String> {
        // An action the reference does not know is its 321 refusal
        // (`order_rule_refusal`); one it knows that ibx cannot send
        // (SSHORTX, SELL LONG) fails here.
        if Self::action_known(&order.action) {
            order.side()?;
        }

        // transmit=false cannot be honoured: every order is sent to the
        // broker immediately when place_order is called; there is no
        // staging concept. Accepting it would send a "staged" bracket
        // parent live on its own, so reject loudly at the call instead.
        // See: https://github.com/deepentropy/ibx/issues/226
        // A what-if with transmit off gets the reference's refusal 321
        // instead (ibx#462).
        if !order.transmit && !order.what_if {
            return Err(
                "transmit=false is not supported: orders are transmitted \
                 immediately on place_order; there is no staging concept, so \
                 the order would go live despite transmit=false. Place child \
                 orders with parent_id/oca_group set and keep transmit=true \
                 (the engine links them server-side)."
                    .into(),
            );
        }

        // An unrecognized tif would otherwise be sent as DAY silently.
        match order.tif.as_str() {
            "" | "DAY" | "GTC" | "IOC" | "FOK" | "OPG" | "GTD" | "DTC" | "AUC"
            | TIF_OVERNIGHT | TIF_OVERNIGHT_DAY => {}
            other => {
                return Err(format!(
                    "Unsupported tif '{}': use DAY, GTC, IOC, FOK, OPG, GTD, DTC, AUC, OVERNIGHT or OVERNIGHT + DAY",
                    other
                ));
            }
        }

        let order_type = order.order_type.to_uppercase();

        // An algorithm name is checked against the server's definitions
        // (439, ibx#263); one ibx cannot send fails when the order is built.
        // An algo order is checked as the order type it rides (ibx#263).
        // A what-if is checked as the order it previews (ibx#462).
        match order_type.as_str() {
            "MKT" | "LMT" | "STP" | "STP LMT" | "TRAIL" | "TRAIL LIMIT"
            | "MOC" | "LOC" | "MIT" | "LIT" | "MTL" | "MKT PRT" | "STP PRT"
            | "REL" | "PEG MKT" | "PEG MID" | "PEG MIDPT" | "MIDPX" | "MIDPRICE"
            | "SNAP MKT" | "SNAP MID" | "SNAP MIDPT" | "SNAP PRI" | "SNAP PRIM"
            | "BOX TOP" | "PEG BENCH"
            | "TRAIL MIT" | "TRAIL LIT" | "PEG BEST" | "RPI" | "PASSV REL" => {}
            _ => return Err(format!("Unsupported order type: '{}'", order.order_type)),
        }

        // Reject orders that require aux_price when it is zero — prevents silent no-trigger bugs.
        // See: https://github.com/deepentropy/ibx/issues/115
        match order_type.as_str() {
            "STP" | "STP PRT" | "MIT" if order.aux_price == 0.0 => {
                return Err(format!(
                    "{} order requires aux_price (stop/trigger price) but got 0.0 — \
                     set aux_price to the desired trigger price, not lmt_price",
                    order.order_type
                ));
            }
            "STP LMT" | "LIT" if order.aux_price == 0.0 => {
                return Err(format!(
                    "{} order requires aux_price (stop/trigger price) but got 0.0",
                    order.order_type
                ));
            }
            "TRAIL" if !is_given(order.trailing_percent) && !is_given(order.aux_price) => {
                return Err(
                    "TRAIL order requires either trailing_percent or aux_price (trail amount) \
                     but both are 0.0".into()
                );
            }
            "TRAIL LIMIT" if !is_given(order.aux_price) => {
                return Err(
                    "TRAIL LIMIT order requires aux_price (trail amount) but got 0.0".into()
                );
            }
            "TRAIL MIT" if !is_given(order.trailing_percent) && !is_given(order.aux_price) => {
                return Err(
                    "TRAIL MIT order requires either trailing_percent or aux_price (trail amount) \
                     but both are 0.0".into()
                );
            }
            "TRAIL LIT" if !is_given(order.trailing_percent) && !is_given(order.aux_price) => {
                return Err(
                    "TRAIL LIT order requires either trailing_percent or aux_price (trail amount) \
                     but both are 0.0".into()
                );
            }
            _ => {}
        }

        Ok(())
    }

    /// Check and translate a scanner subscription as the reference reads
    /// it (ibx#456): the client fields that are set become filters in the
    /// reference's code map and order, then the filter options are added
    /// in client order (a code already present is replaced and moves to
    /// the end). Refusals in the reference's order: 320 for a filter
    /// option that is not a key and value pair, 10337 / 10338 for a bad
    /// subscription option, then 321 for any subscription option, since
    /// its only key needs a verified session.
    pub fn scanner_request(
        sub: &crate::api::types::ScannerSubscription,
        options: &[crate::api::types::TagValue],
        filter_options: &[crate::api::types::TagValue],
    ) -> Result<crate::control::scanner::ScannerSubscription, (i64, String)> {
        use crate::control::scanner::java_double_text;
        fn put(filters: &mut Vec<(String, String)>, code: &str, value: String) {
            filters.retain(|(c, _)| c != code);
            if !code.is_empty() && !value.is_empty() {
                filters.push((code.to_string(), value));
            }
        }
        // Unset: the ibapi marker or a negative number.
        let double = |v: f64| (v >= 0.0 && v != f64::MAX).then(|| java_double_text(v)).unwrap_or_default();
        let int = |v: i32| (v >= 0 && v != i32::MAX).then(|| v.to_string()).unwrap_or_default();

        let mut filters: Vec<(String, String)> = Vec::new();
        put(&mut filters, "priceAbove", double(sub.above_price));
        put(&mut filters, "priceBelow", double(sub.below_price));
        put(&mut filters, "volumeAbove", int(sub.above_volume));
        put(&mut filters, "marketCapAbove1e6", double(sub.market_cap_above));
        put(&mut filters, "marketCapBelow1e6", double(sub.market_cap_below));
        put(&mut filters, "moodyRatingAbove", sub.moody_rating_above.clone());
        put(&mut filters, "moodyRatingBelow", sub.moody_rating_below.clone());
        put(&mut filters, "spRatingAbove", sub.sp_rating_above.clone());
        put(&mut filters, "spRatingBelow", sub.sp_rating_below.clone());
        put(&mut filters, "maturityDateAbove", sub.maturity_date_above.clone());
        put(&mut filters, "maturityDateBelow", sub.maturity_date_below.clone());
        put(&mut filters, "couponRateAbove", double(sub.coupon_rate_above));
        put(&mut filters, "couponRateBelow", double(sub.coupon_rate_below));
        if sub.exclude_convertible {
            put(&mut filters, "excludeConvertible", "true".into());
        }
        put(&mut filters, "avgOptVolumeAbove", int(sub.average_option_volume_above));
        let stock_types = match sub.stock_type_filter.trim().to_ascii_uppercase().as_str() {
            "STOCK" => "exc:ETF".to_string(),
            t @ ("ETF" | "CORP" | "ADR" | "REIT" | "CEF") => format!("inc:{}", t),
            _ => String::new(),
        };
        put(&mut filters, "stkTypes", stock_types);

        // The client sends each option list as `tag=value;` items.
        let items = |list: &[crate::api::types::TagValue]| -> Vec<String> {
            let text: String = list.iter().map(|tv| format!("{}={};", tv.tag, tv.value)).collect();
            text.split(';').filter(|i| !i.is_empty()).map(str::to_string).collect()
        };
        for item in items(filter_options) {
            let parts: Vec<&str> = item.split('=').filter(|p| !p.is_empty()).collect();
            if parts.len() < 2 {
                return Err((320, format!("Error reading request:Not a key-value pair in generic options list: {}", item)));
            }
            put(&mut filters, parts[0], parts[1].to_string());
        }

        let options = items(options);
        for item in &options {
            let Some((key, value)) = item.split_once('=') else { continue };
            if key != "manual" {
                return Err((10337, format!(
                    "Misc options key={} is invalid in ReqScannerSubscription(22) request. Valid keys are: manual", key)));
            }
            if value != "0" && value != "1" {
                return Err((10338, format!(
                    "Misc options value={} is invalid for key=manual in ReqScannerSubscription(22) request. Valid values are: 0, 1", value)));
            }
        }
        if !options.is_empty() {
            return Err((321, "Error validating request.-'co' : cause - Historical data: 'manual' requires Verified API.".into()));
        }

        Ok(crate::control::scanner::ScannerSubscription {
            instrument: sub.instrument.clone(),
            location_code: sub.location_code.clone(),
            scan_code: sub.scan_code.clone(),
            number_of_rows: sub.number_of_rows,
            filters,
        })
    }

    /// Most headlines one historical news request returns (ibx#459).
    pub const MAX_NEWS_RESULTS: i64 = 300;

    /// The reference's local checks of a historical news request
    /// (ibx#459), all 321: every requested provider must be a subscribed
    /// source; the total, capped at 300, must be positive.
    pub fn historical_news_refusal(provider_codes: &str, total_results: i64, sources: &[String]) -> Option<(i64, String)> {
        let refuse = |cause: String| Some((321, format!("Error validating request.-'ca' : cause - {}", cause)));
        // The codes as a Java split on `+`: trailing empty codes dropped,
        // an empty text is one empty code.
        let mut codes: Vec<&str> = provider_codes.split('+').collect();
        if !provider_codes.is_empty() {
            while codes.last() == Some(&"") { codes.pop(); }
        }
        if let Some(code) = codes.into_iter().find(|c| !news_source_subscribed(c, sources)) {
            return refuse(format!("Not subscribed for '{}' provider", code));
        }
        if total_results.min(Self::MAX_NEWS_RESULTS) <= 0 {
            return refuse("Total results must be > 0".into());
        }
        None
    }

    /// The reference's local checks of an option calculation (ibx#442):
    /// 321 with its text, before any lookup. `price` is a price calculation
    /// (else an implied volatility one); `features` is the account feature
    /// list.
    pub fn option_calc_refusal(contract: &crate::api::types::Contract, price: bool, features: Option<&[String]>) -> Option<(i64, String)> {
        let class = if price { "bJ" } else { "bI" };
        let refuse = |cause: &str| Some((321, format!("Error validating request.-'{}' : cause - {}", class, cause)));
        if contract.exchange.is_empty() {
            return refuse("Please enter exchange");
        }
        let sec_type = contract.sec_type.as_str();
        let option_type = matches!(sec_type, "OPT" | "FOP" | "IOPT");
        let con_id_set = contract.con_id != 0;
        if !con_id_set && !option_type {
            return refuse(if price {
                "Calculation of Option Price supported for option securities only"
            } else {
                "Calculation of Implied Volatility supported for option securities only"
            });
        }
        if !price {
            return None;
        }
        if !con_id_set || sec_type == "BAG" {
            if contract.symbol.is_empty() {
                return refuse("The symbol or the local-symbol must be entered");
            }
            if contract.symbol.chars().any(|c| c as u32 >= 128) {
                return refuse("Symbol should contain valid non-unicode characters only");
            }
            if matches!(sec_type, "" | "UNK" | "All" | "*") {
                return refuse("Please enter a valid security type");
            }
        }
        let expiry_empty = contract.last_trade_date_or_contract_month.is_empty();
        if sec_type == "OPT" && !con_id_set {
            let zero_strike_ok = features.map(|f| f.iter().any(|x| x == "ZEROSTRKOPT")).unwrap_or(false);
            let right_set = matches!(contract.right.chars().next(), Some('P') | Some('C'));
            let strike_unset = contract.strike == 0.0 || contract.strike == f64::MAX;
            if expiry_empty || (strike_unset && !zero_strike_ok) || !right_set {
                return refuse("When the local symbol field is empty, please fill all option fields (right, strike, expiry)");
            }
        }
        if !con_id_set && matches!(sec_type, "OPT" | "FOP" | "IOPT" | "FUT" | "FWD") && expiry_empty {
            return refuse("Please enter a local symbol or an expiry");
        }
        None
    }

    /// The reference's local checks of a news article request (ibx#459),
    /// both 321.
    pub fn news_article_refusal(provider_code: &str, article_id: &str, sources: &[String]) -> Option<(i64, String)> {
        let refuse = |cause: String| Some((321, format!("Error validating request.-'cg' : cause - {}", cause)));
        if !news_source_subscribed(provider_code, sources) {
            return refuse(format!("Not subscribed for '{}' provider", provider_code));
        }
        if article_id.is_empty() {
            return refuse("Article ID must not be empty".into());
        }
        None
    }

    /// Validate historical-request arguments before anything reaches the
    /// engine (ibx#232): an unrecognized bar_size previously fell back to
    /// 5-minute bars silently (via TWO divergent tables), and an
    /// unrecognized what_to_show fell back to TRADES. The caller gets a
    /// synchronous Err at the call instead of plausible, wrong candles.
    /// A historical-data request for a contract with no conId is looked up
    /// first, as the reference (ibx#427): the request is wrapped so the
    /// engine sends it once the contract is found. Others go as they are.
    pub fn resolve_first(req_id: ReqId, contract: &crate::api::types::Contract, request: ControlCommand) -> ControlCommand {
        if contract.con_id != 0 {
            return request;
        }
        ControlCommand::ResolveContract {
            req_id,
            lookup: crate::types::ContractLookup {
                symbol: contract.symbol.clone(),
                sec_type: contract.sec_type.clone(),
                exchange: contract.exchange.clone(),
                currency: contract.currency.clone(),
                filters: crate::types::SecDefFilters {
                    primary_exchange: contract.primary_exchange.clone(),
                    local_symbol: contract.local_symbol.clone(),
                    last_trade_date_or_contract_month: contract.last_trade_date_or_contract_month.clone(),
                    strike: contract.strike,
                    right: contract.right.clone(),
                    multiplier: contract.multiplier.clone(),
                    trading_class: contract.trading_class.clone(),
                    sec_id: contract.sec_id.clone(),
                    sec_id_type: contract.sec_id_type.clone(),
                    include_expired: contract.include_expired,
                    issuer_id: String::new(),
                },
            },
            request: Box::new(request),
        }
    }

    /// The reference's local refusal of a reqHistoricalData (ibx#430,
    /// ibx#429): (code, text) to report as an error, 321 or 10314, with no
    /// end. Every legal bar size streams with keepUpToDate.
    /// `max_backfill_years` is the logon limit of the session, None when
    /// it is not checked (ibx#421). A contract with no exchange (empty, the
    /// official API's default) is refused first after an end date the
    /// reference cannot read, but for a continuous future
    /// (`jextend.bM.n()@4-63`, HIST-BARS 1.2).
    #[allow(clippy::too_many_arguments)]
    pub fn historical_refusal(
        end_date_time: &str,
        duration: &str,
        bar_size: &str,
        what_to_show: &str,
        format_date: i32,
        keep_up_to_date: bool,
        sec_type: &str,
        exchange: &str,
        max_backfill_years: Option<i32>,
        include_expired: bool,
    ) -> Option<(i32, String)> {
        use crate::control::historical::{bar_request_refusal, is_valid_end_date};
        if is_valid_end_date(end_date_time) && exchange.is_empty() && !sec_type.eq_ignore_ascii_case("CONTFUT") {
            return Some((321, bar_request_refusal("Please enter exchange")));
        }
        crate::control::historical::check_bar_request(
            end_date_time, duration, bar_size, what_to_show, Some(format_date), keep_up_to_date, sec_type,
            max_backfill_years, include_expired,
        ).err()
    }

    /// A reqHeadTimeStamp whose contract has no exchange (empty, the official
    /// API's default): 321, the reference's first check
    /// (`jextend.bN.n()@4-37`, HIST-BARS 4.1). Nothing is sent.
    pub fn head_timestamp_refusal(exchange: &str) -> Option<(i32, String)> {
        exchange.is_empty()
            .then(|| (321, "Error validating request.-'bN' : cause - Please enter exchange".to_string()))
    }

    /// A reqMktData whose contract has no exchange (empty, the official
    /// API's default), but a news contract: 321, the reference's first
    /// check (`jextend.bQ.n()@12-65`, MKTDATA-L1 1.2). Nothing is sent.
    pub fn market_data_exchange_refusal(exchange: &str, sec_type: &str) -> Option<(i64, String)> {
        (exchange.is_empty() && !sec_type.eq_ignore_ascii_case("NEWS"))
            .then(|| (321, "Error validating request.-'bQ' : cause - Please enter exchange".to_string()))
    }

    /// The reference's checks of a reqContractDetails contract
    /// (`jextend.bK.n()` → `jextend.bF.B()`, CONTRACT-DETAILS 1.2, 1.3),
    /// 321: with no conId, no symbol, local symbol nor security id (an
    /// issuer id or a news contract need none); then with no conId and no
    /// security id, a security type that is not one (empty, the official
    /// API's default; `UNK`, `All`, `*`); an issuer id makes the type a
    /// fixed income one. An empty exchange or currency is not refused:
    /// those fields are left out of the lookup.
    pub fn contract_details_refusal(c: &crate::api::types::Contract) -> Option<(i64, String)> {
        let refuse = |cause: &str| Some((321, format!("Error validating request.-'bK' : cause - {}", cause)));
        let sec_type = c.sec_type.trim();
        let news = sec_type.eq_ignore_ascii_case("NEWS");
        let sec_id = !c.sec_id.is_empty();
        let issuer = !c.issuer_id.trim().is_empty();
        if c.con_id <= 0 && c.symbol.is_empty() && c.local_symbol.is_empty() && !sec_id && !news && !issuer {
            return refuse("The symbol or the local-symbol or the security id must be entered");
        }
        let no_type = sec_type.is_empty() || ["UNK", "All", "*"].iter().any(|t| sec_type.eq_ignore_ascii_case(t));
        if c.con_id <= 0 && no_type && !sec_id && !issuer {
            return refuse("Please enter a valid security type");
        }
        None
    }

    /// The reference's local answers to a reqHistoricalTicks before its
    /// query (ibx#432): the warnings (2174, 10299) and the refusal (10314,
    /// 321), in the order the client gets them. True when the request
    /// goes on.
    pub fn historical_ticks_checks(
        start: &str, end: &str, number_of_ticks: i32, what_to_show: &str, ignore_size: bool,
        sec_type: &str, exchange: &str,
    ) -> (Vec<(i32, String)>, bool) {
        let (mut out, checked) = crate::control::historical::check_ticks_request(
            start, end, number_of_ticks, what_to_show, ignore_size, sec_type, exchange,
            &crate::gateway::machine_time_zone(), crate::control::historical::now_secs(),
        );
        let ok = match checked {
            Ok(_) => true,
            Err(e) => {
                out.push(e);
                false
            }
        };
        (out, ok)
    }

    /// Reject orders whose contract is not a common stock.
    ///
    /// The outbound order encoding in `engine::hot_loop::order_builder` only
    /// supports common stock, and the instrument registry drops
    /// `sec_type`/`exchange`. A non-STK contract (OPT/FUT/BAG/…) would
    /// therefore be sent as a stock order on the underlying symbol with no
    /// error surfaced. Until non-STK encoding lands, reject those contracts
    /// up front.
    ///
    /// An empty `sec_type` is treated as STK (the engine default), so existing
    /// stock callers that omit the field are unaffected.
    /// See: https://github.com/deepentropy/ibx/issues/202
    pub fn validate_order_contract(sec_type: &str) -> Result<(), String> {
        // A combo (BAG) goes out with its legs, as the reference builds it
        // (ibx#470).
        if sec_type.is_empty() || sec_type.eq_ignore_ascii_case("STK") || sec_type.eq_ignore_ascii_case("BAG") {
            return Ok(());
        }
        Err(format!(
            "Unsupported contract sec_type '{}': only STK and BAG orders are supported. \
             Other contracts (OPT/FUT/…) are not yet wire-encoded and would \
             otherwise be silently sent as a stock order on the underlying symbol. \
             See https://github.com/deepentropy/ibx/issues/202",
            sec_type
        ))
    }

    /// The combo of a BAG order, read and checked as the reference reads a
    /// placeOrder (ibx#470); `Ok(None)` for any other contract, `Err` with
    /// the code and text of a refusal, sent before anything goes out.
    ///
    /// Reading (`jextend.bH.q(ee)`, error 320): per-leg prices whose count
    /// is not the leg count (10057), a smartComboRoutingParams name the
    /// reference does not know (10027). Checking (`jextend.bH.S()`, error
    /// 321): no leg (314), a smart combo in a currency with no smart combo
    /// conId in the logon (10011), then each leg in order
    /// (`jextend.at.a(OrderCreator)`): a per-leg price on an order type
    /// other than LMT or REL + LMT (10055), a leg without price after a
    /// priced first leg (10056), a conId or ratio not above 0 or an action
    /// that is not BUY, SELL, SSHORT or SSHORTX, an open/close code that
    /// is not 0, 1 or 2 for an institutional order, a short leg or a short
    /// sale slot for one that is not (346); then a limit price with per-leg
    /// prices (10054) and ratios with a common divisor (476). The rule
    /// texts of the leg checks follow the leg's text "The combo details
    /// for leg 'i' are invalid. - ". The institutional short-sale rules of
    /// the legs (494, 348, 347, 353, 352, 495) are not checked.
    pub fn combo_order(
        contract: &ApiContract,
        order: &ApiOrder,
        reference: &crate::bridge::ReferenceState,
        account_id: &str,
    ) -> Result<Option<ComboSpec>, (i64, String)> {
        if !contract.sec_type.eq_ignore_ascii_case("BAG") {
            return Ok(None);
        }
        let read_error = |cause: &str| (320, format!("Error reading request:{}", cause));
        let refuse = |cause: &str| (321, format!("Error validating request.-'bH' : cause - {}", cause));
        let legs = &contract.combo_legs;
        let prices = &order.order_combo_legs;
        if !prices.is_empty() && prices.len() != legs.len() {
            return Err(read_error("Mismatch per-leg price number with combo leg specification."));
        }
        let mut routing_attrs = Vec::with_capacity(order.smart_combo_routing_params.len());
        let mut non_guaranteed = false;
        for tv in &order.smart_combo_routing_params {
            let Some(tag) = combo_routing_tag(&tv.tag) else {
                return Err(read_error(INVALID_COMBO_ROUTING_TAG));
            };
            if tag == 6248 { non_guaranteed = tv.value.trim() == "1"; }
            routing_attrs.push((tag, tv.value.clone()));
        }
        if legs.is_empty() {
            return Err(refuse("Security type 'BAG' requires combo leg details."));
        }
        let smart = matches!(contract.exchange.to_uppercase().as_str(), "" | "SMART");
        let smart_con_id = if smart {
            match reference.smart_combo_con_id(&contract.currency) {
                Some(con_id) => con_id,
                None => return Err(refuse(&format!("Currency {} isn't supported for smart combo.", contract.currency))),
            }
        } else {
            0
        };
        let (super_user, _) = reference.short_sale_flags();
        let institutional = super_user
            || crate::engine::hot_loop::order_builder::clearing_away(&order.clearing_intent, account_id, super_user);
        let order_type = order.order_type.to_uppercase();
        let priced = |i: usize| prices.get(i).is_some_and(|p| *p != f64::MAX);
        for (i, leg) in legs.iter().enumerate() {
            let leg_error = |rule: &str| refuse(&format!("The combo details for leg '{}' are invalid. - {}", i, rule));
            if priced(i) && !matches!(order_type.as_str(), "LMT" | "REL + LMT") {
                return Err(leg_error("Only LMT or REL+LMT order allows using per-leg prices."));
            }
            if i > 0 && priced(0) && !priced(i) {
                return Err(leg_error("All leg prices are needed when specifying per-leg prices."));
            }
            let (action, short) = match leg.action.to_uppercase().as_str() {
                "BUY" => ('1', false),
                "SELL" => ('2', false),
                "SSHORT" | "SSHORTX" => ('2', true),
                _ => ('0', false),
            };
            if leg.con_id <= 0 || leg.ratio <= 0 || action == '0' {
                return Err(leg_error(&format!(" conid, ratio, side: {}, {}, {}", leg.con_id, leg.ratio, action)));
            }
            if institutional && !(0..=2).contains(&leg.open_close) {
                return Err(leg_error(&format!("Open/Close: {}", leg.open_close)));
            }
            if !institutional && (short || leg.short_sale_slot != 0) {
                return Err(leg_error("Not an institutional account, or an away clearing order"));
            }
        }
        let all_priced = (0..legs.len()).all(priced);
        let lmt_set = order.lmt_price != f64::MAX && !(order.lmt_price == 0.0 && all_priced);
        if all_priced && lmt_set {
            return Err(refuse("Can't specify combo price when using per-leg prices."));
        }
        let divisor = legs.iter().fold(0u32, |g, l| gcd(g, l.ratio.unsigned_abs()));
        if divisor != 1 {
            return Err(refuse("Invalid leg ratio."));
        }
        Ok(Some(ComboSpec {
            exchange: contract.exchange.clone(),
            currency: contract.currency.clone(),
            symbol: contract.symbol.clone(),
            smart_con_id,
            legs: legs.iter().map(|l| ComboLegSpec {
                con_id: l.con_id,
                ratio: l.ratio,
                buy: l.action.eq_ignore_ascii_case("BUY"),
                exchange: l.exchange.clone(),
            }).collect(),
            leg_prices: if all_priced {
                prices.iter().map(|p| (p * PRICE_SCALE_F).round() as Price).collect()
            } else {
                Vec::new()
            },
            routing_attrs,
            non_guaranteed,
        }))
    }

    /// A modify of a combo order must name the legs of the working order
    /// (ibx#470): else 10059 and nothing is sent
    /// (`jextend.bH.c(jclient.pe, jsecdef.dl)@965-1070`).
    pub fn combo_modify_refusal(contract: &ApiContract, working: &ApiContract) -> Option<(i64, String)> {
        if !contract.sec_type.eq_ignore_ascii_case("BAG") || contract.combo_legs.is_empty() {
            return None;
        }
        let known = |con_id: i64| working.combo_legs.iter().any(|l| l.con_id == con_id);
        (!contract.combo_legs.iter().all(|l| known(l.con_id)))
            .then(|| (10059, "Mismatch leg contract.".to_string()))
    }

    /// The combo order as the engine builds it (ibx#470): the order as
    /// placed, through the extended encoder, with its combo. A limit price
    /// left unset with per-leg prices goes as 0; the engine writes the
    /// price the leg prices give.
    pub fn build_combo_order_request(
        order: &ApiOrder,
        order_id: OrderId,
        instrument: InstrumentId,
        combo: ComboSpec,
    ) -> Result<ControlCommand, String> {
        let mut order = order.clone();
        if !combo.leg_prices.is_empty() {
            order.lmt_price = 0.0;
        }
        Self::build_order_request_with(&order, order_id, instrument, Some(Box::new(combo)))
    }

    /// The contract and order openOrder shows for a combo order (ibx#470):
    /// the combo contract with its legs, and the per-leg prices the server
    /// reports in leg order (no limit price with per-leg prices).
    pub fn apply_combo_view(order_id: OrderId, contract: &mut ApiContract, order: &mut ApiOrder, shared: &SharedState) {
        let Some(view) = shared.orders.combo_view(order_id) else { return };
        *contract = view.contract;
        if view.leg_prices.iter().any(|p| *p != f64::MAX) {
            order.lmt_price = f64::MAX;
        }
        order.order_combo_legs = view.leg_prices;
    }

    /// The execution a combo report shows (ibx#470): the combo contract on
    /// the combo's report; the leg's contract, side, size, prices and
    /// totals on a leg's report.
    pub fn apply_combo_exec(fe: &crate::bridge::FillExec, contract: &mut ApiContract, exec: &mut ApiExecution) {
        let Some(combo) = &fe.combo else { return };
        *contract = combo.contract.clone();
        if let Some(leg) = &combo.leg {
            exec.side = leg.side.clone();
            exec.shares = leg.shares;
            exec.price = leg.price;
            exec.cum_qty = leg.cum_qty;
            exec.avg_price = leg.avg_price;
        }
    }

    /// The reference refuses a fractional quantity before sending anything,
    /// with error 10243 (ib-agent#192 B3). ibx used to cut it to a whole
    /// number and send it, so 1.5 shares went out as 1 (ibx#313).
    pub fn fractional_quantity_refusal(order: &ApiOrder) -> Option<(i64, String)> {
        if order.total_quantity.fract() != 0.0 {
            Some((FRACTIONAL_VIA_API.0, FRACTIONAL_VIA_API.1.to_string()))
        } else {
            None
        }
    }

    /// Every order the reference refuses before sending anything, with its
    /// error code and text. Nothing is sent for such an order. `exchange`
    /// is the contract's.
    pub fn refusal_before_sending(order: &ApiOrder, exchange: &str) -> Option<(i64, String)> {
        Self::refusal_before_sending_for(order, exchange, false)
    }

    /// [`Self::refusal_before_sending`] for an order whose account's
    /// application is not approved yet when `account_pending` (see
    /// [`Self::order_account_pending`], ibx#421).
    pub fn refusal_before_sending_for(order: &ApiOrder, exchange: &str, account_pending: bool) -> Option<(i64, String)> {
        Self::read_refusal(order)
            .or_else(|| Self::exchange_refusal(exchange))
            .or_else(|| Self::order_rule_refusal(order, account_pending))
            .or_else(|| Self::fractional_quantity_refusal(order))
    }

    /// Whether the account of an order is one whose application is not
    /// approved yet (8092 of the logon, ibx#421): the reference refuses
    /// such an order with 10136 on a session that is not an FA one
    /// (`jextend.bH.S()@5072-5127`, `jextend.bi.g(String)`). The account
    /// is the order's on a session with several accounts, else the logon
    /// account, which the reference puts on the orders of a one-account
    /// session.
    pub fn order_account_pending(order: &ApiOrder, reference: &crate::bridge::ReferenceState, logon_account: &str) -> bool {
        if reference.fa_session() {
            return false;
        }
        reference.account_pending(Self::request_account(&order.account, reference, logon_account))
    }

    /// The account of a request (ibx#421): the one given on a session
    /// with several accounts (the logon account when none is given), else
    /// the logon account.
    fn request_account<'a>(given: &'a str, reference: &crate::bridge::ReferenceState, logon_account: &'a str) -> &'a str {
        if reference.managed_accounts().len() > 1 && !given.is_empty() { given } else { logon_account }
    }

    /// The pending accounts check of a reqPositions (ibx#421,
    /// `jextend.cl.n()@162-251`): the accounts of the logon's list whose
    /// application is not approved. All of them: Err, error 10275 and the
    /// request stops; some: Ok with the warning 10275, the request goes
    /// on; none: Ok(None).
    pub fn positions_pending_check(reference: &crate::bridge::ReferenceState) -> Result<Option<(i64, String)>, (i64, String)> {
        let pending = reference.pending_managed_accounts();
        if pending.is_empty() {
            return Ok(None);
        }
        let names: Vec<&str> = pending.iter().map(String::as_str).collect();
        let error = (10275, crate::control::logon::positions_not_available(&names));
        if reference.managed_accounts().len() <= pending.len() { Err(error) } else { Ok(Some(error)) }
    }

    /// A reqPositionsMulti for an account whose application is not
    /// approved: error 10275 and the request stops (ibx#421,
    /// `jextend.bk.n()@9-39`).
    pub fn positions_multi_pending_refusal(account: &str, reference: &crate::bridge::ReferenceState, logon_account: &str) -> Option<(i64, String)> {
        let account = Self::request_account(account, reference, logon_account);
        reference.account_pending(account)
            .then(|| (10275, crate::control::logon::positions_not_available(&[account])))
    }

    /// A reqAccountUpdates for an account whose application is not
    /// approved: the warning 10275, and the request goes on (ibx#421,
    /// `jextend.bl.n()@127-196`).
    pub fn account_updates_pending_warning(acct_code: &str, reference: &crate::bridge::ReferenceState, logon_account: &str) -> Option<(i64, String)> {
        Self::positions_multi_pending_refusal(acct_code, reference, logon_account)
    }

    /// An order whose contract has no exchange (empty, the official API's
    /// default): error 452's text "Missing order exchange", sent as 321,
    /// nothing sent. The reference checks it before its other order rules
    /// (`jextend.bH.S()@437-455`: `jutils.dO.c(String)`, null or empty, on
    /// the contract's exchange `jclient.ph.G()`); ibx used to send the
    /// order with no exchange.
    fn exchange_refusal(exchange: &str) -> Option<(i64, String)> {
        exchange.is_empty()
            .then(|| (321, "Error validating request.-'bH' : cause - Missing order exchange".to_string()))
    }

    /// The refusals of the reference's reading of the order, in its order
    /// (ibx#263): it keeps reading after a bad field and gives only the
    /// last one. A negative discretionary amount and a trailing percent
    /// given with a trailing amount are answered as 320 with the rule
    /// text; a goodAfterTime that is not a date and time keeps its own
    /// code.
    fn read_refusal(order: &ApiOrder) -> Option<(i64, String)> {
        let read_error = |cause: &str| Some((320, format!("Error reading request:{}", cause)));
        let set = |v: f64| v != 0.0 && v != f64::MAX;
        let mut last = None;
        if order.discretionary_amt < 0.0 {
            last = read_error("Discretionary amount does not conform to the minimum price variation for this contract ");
        }
        if let Some(refusal) = Self::good_after_time_refusal(order) {
            last = Some(refusal);
        }
        if set(order.trailing_percent) && set(order.aux_price) {
            last = read_error("Cannot specify Trailing Amount and Trailing Percent at the same time");
        }
        // The conditions are read last: a time it cannot read is 10314
        // with the label "Time" (ibx#416, captured 02/10/2026).
        let machine = crate::gateway::machine_time_zone();
        if Self::condition_times(order).any(|t| !t.is_empty() && parse_condition_time(t, &machine).is_none()) {
            last = Some((10314, INVALID_DATE_TIME.replace("%s", "Time")));
        }
        last
    }

    /// The times of the order's time conditions, in order.
    fn condition_times(order: &ApiOrder) -> impl Iterator<Item = &str> {
        order.conditions.iter().filter_map(|c| match c {
            OrderCondition::Time { time, .. } => Some(time.as_str()),
            _ => None,
        })
    }

    /// One warning 2174 per time condition whose time names no zone, as
    /// the reference sends them while it reads the order, before any
    /// refusal (ibx#416, captured 02/10/2026: "20991231 23:59:59").
    pub fn implied_zone_warnings(order: &ApiOrder) -> Vec<(i64, String)> {
        let machine = crate::gateway::machine_time_zone();
        Self::condition_times(order)
            .filter(|t| parse_condition_time(t, &machine).is_some_and(|c| c.implied_zone))
            .map(|_| (2174, IMPLIED_TIME_ZONE.to_string()))
            .collect()
    }

    /// The zone rule of the time conditions (ibx#416, `trader.order.ay`,
    /// `feature.date.ad.c`): when one time is in a zone other than UTC and
    /// the machine's, the order waits for the contract's trading hours and
    /// every condition time must then be in exactly the contract's zone
    /// (`contract_zone`), else error 10314 and nothing is sent (captured
    /// 02/10/2026: Asia/Tokyo on SPY). Until the contract's zone is known
    /// any zone that resolves is taken.
    pub fn condition_time_zone_refusal(order: &ApiOrder, contract_zone: Option<&str>) -> Option<(i64, String)> {
        let machine = crate::gateway::machine_time_zone();
        let zones: Vec<String> = Self::condition_times(order)
            .filter_map(|t| parse_condition_time(t, &machine)).map(|c| c.zone).collect();
        if zones.iter().all(|z| z == "UTC" || *z == machine) {
            return None;
        }
        let contract = contract_zone?;
        if zones.iter().all(|z| z == contract) {
            None
        } else {
            Some((10314, INVALID_DATE_TIME.replace("%s", "Time")))
        }
    }

    /// The order with each condition time as the reference sends it: the
    /// UTC form unchanged, any other form as its instant in UTC (ibx#416,
    /// captured 02/10/2026: "20991231 23:59:59 US/Eastern" goes out as
    /// 21000101-04:59:59). Call it after the refusals.
    pub fn with_condition_times(order: &ApiOrder) -> std::borrow::Cow<'_, ApiOrder> {
        if Self::condition_times(order).next().is_none() {
            return std::borrow::Cow::Borrowed(order);
        }
        let machine = crate::gateway::machine_time_zone();
        let mut sent = order.clone();
        for c in &mut sent.conditions {
            if let OrderCondition::Time { time, .. } = c
                && let Some(t) = parse_condition_time(time, &machine)
            {
                *time = t.wire;
            }
        }
        std::borrow::Cow::Owned(sent)
    }

    /// A goodTillDate the reference refuses before sending, with error 343
    /// (ibx#335; ib-agent#192 F6): it is not a date and time, or its zone is
    /// not UTC, the zone of this machine, or exactly the zone of the
    /// contract's trading hours (`contract_zone`). The order is not sent,
    /// rather than sent with no expiry. The contract's zone is known once
    /// its details were received; until then any zone that resolves is
    /// taken.
    pub fn good_till_date_refusal(order: &ApiOrder, contract_zone: Option<&str>) -> Option<(i64, String)> {
        let accepted = match crate::config::parse_ib_expiry_zoned(&order.good_till_date) {
            Err(_) => false,
            Ok(Some((_, Some(zone)))) => zone == "UTC"
                || contract_zone.is_none_or(|c| c == zone)
                || crate::gateway::machine_time_zone() == zone,
            Ok(_) => true,
        };
        if accepted { None } else { Some((343, INVALID_DATE_TIME.replace("%s", "End Time"))) }
    }

    /// A goodAfterTime that is not a date and time: error 337 with the
    /// reference's text, whose label for this field is "Start Time"
    /// (ibx#467). ibx needs the date; the reference also takes a time alone
    /// (today assumed).
    fn good_after_time_refusal(order: &ApiOrder) -> Option<(i64, String)> {
        match crate::config::parse_ib_expiry(&order.good_after_time) {
            Ok(None) | Ok(Some(crate::config::IbExpiry::Instant(_))) => None,
            _ => Some((337, INVALID_DATE_TIME.replace("%s", "Start Time"))),
        }
    }

    /// Whether the reference knows an order action (`jfix.eU.a(String,
    /// boolean)`, case ignored): BUY, SELL, SSHORT, SSHRT, SSHORTX, SSHRTX,
    /// SELL LONG and Exchange_Action. Any other reads as side 0.
    pub fn action_known(action: &str) -> bool {
        ["BUY", "SELL", "SSHORT", "SSHRT", "SSHORTX", "SSHRTX", "SELL LONG", "Exchange_Action"]
            .iter().any(|a| a.eq_ignore_ascii_case(action))
    }

    /// An order id the reference refuses with 10149 when no order has it:
    /// 0 and the two int bounds (`jextend.bH.W()@29-55`,
    /// `twslaunch.jutils.av.c(int)`; ibx#485). ibx used to take the next id
    /// for 0.
    pub fn order_id_refusal(order_id: OrderId) -> Option<(i64, String)> {
        matches!(order_id, 0 | 2147483647 | -2147483648)
            .then(|| (10149, format!("Invalid order id: {}", order_id)))
    }

    /// Order rules the reference checks before sending, answered as error
    /// 321 with the rule text (ibx#468), in the reference's order
    /// (`jextend.bH.S()` stops at the first). The text after "cause - " is
    /// the rule's; for the TRAIL LIMIT rule only its end is known.
    fn order_rule_refusal(order: &ApiOrder, account_pending: bool) -> Option<(i64, String)> {
        let refuse = |cause: &str| Some((321, format!("Error validating request.-'bH' : cause - {}", cause)));
        let order_type = order.order_type.to_uppercase();
        // An action the reference does not know reads as side 0, refused
        // first (`jextend.bH.S()@1311`, ibx#485).
        if !Self::action_known(&order.action) {
            return refuse("Invalid side field was entered");
        }
        // A quantity below 0 or above 999,999,999 (ibx#263).
        if order.total_quantity < 0.0 || order.total_quantity > 999_999_999.0 {
            return refuse("Order size does not conform to market rule.");
        }
        // A stop type without its stop price, the API's unset value
        // (`jextend.bH.S()@1690-1750`, ibx#485): the auxPrice of STP,
        // STP LMT and STP PRT, the trailStopPrice of a TRAIL LIMIT
        // (ib-agent#194, no final period; ibx takes 0 as unset there too)
        // and of a TRAIL LIT (ibx#469, captured 07/10/2026 by amount and
        // by percent; a TRAIL MIT without one is sent).
        let stop_type = matches!(order_type.as_str(), "STP" | "STP LMT" | "STP PRT");
        if (stop_type && order.aux_price == f64::MAX)
            || (matches!(order_type.as_str(), "TRAIL LIMIT" | "TRAIL LIT")
                && (order.trail_stop_price == f64::MAX || order.trail_stop_price == 0.0))
        {
            return refuse("Please enter a stop price");
        }
        // A trigger method that is not one of the reference's (ibx#263,
        // `jextend.bH.S()@2257`, error 146's text): 0 default, 1 double
        // bid/ask, 2 last, 3 double last, 4 bid/ask, 7 last or bid/ask,
        // 8 midpoint.
        if !matches!(order.trigger_method, 0..=4 | 7 | 8) {
            return refuse("Invalid trigger method");
        }
        // A what-if with transmit off (ibx#462, `jextend.bH.S()@4692`;
        // captured 02/10/2026, with this check's own 'v' in the text).
        if order.what_if && !order.transmit {
            return Some((321, "Error validating request.-'v' : cause - What-If order should have transmit flag set to TRUE.".into()));
        }
        // An account whose application is not approved (ibx#421,
        // `jextend.bH.S()@5127`, thrown with its own code 10136).
        if account_pending {
            return Some((10136, crate::control::logon::PENDING_ACCOUNT.to_string()));
        }
        // A trailing percent below 0 or above 100 (ibx#263).
        let pct = order.trailing_percent;
        if matches!(order_type.as_str(), "TRAIL" | "TRAIL LIMIT" | "TRAIL MIT" | "TRAIL LIT") && pct != 0.0 && pct != f64::MAX
            && (pct < 0.0 || pct > 100.0)
        {
            return refuse("Invalid Trailing Percent value. Valid values are greater than 0 and less than 100.");
        }
        // A stop or trigger price that is not a number (`jextend.bH.S()`
        // @6661-6696 for the stop types, error 403's text; @6731-6766 for
        // MIT and LIT, error 361's; the value must be finite and set,
        // `twslaunch.jutils.av.d(double)`; ibx#485).
        let stop = if order_type == "TRAIL LIMIT" { order.trail_stop_price } else { order.aux_price };
        if (stop_type || order_type == "TRAIL LIMIT") && !stop.is_finite() {
            return refuse("Invalid Stop Price");
        }
        if matches!(order_type.as_str(), "MIT" | "LIT") && (!order.aux_price.is_finite() || order.aux_price == f64::MAX) {
            return refuse("Invalid Trigger Price");
        }
        // TRAIL LIMIT: exactly one of lmtPrice and lmtPriceOffset (also on a
        // replace: sending back the computed lmtPrice with the offset was
        // refused, captured 23/09/2026). lmtPrice is unset at 0 or MAX,
        // lmtPriceOffset at MAX.
        if order_type == "TRAIL LIMIT" {
            let price_set = order.lmt_price != 0.0 && order.lmt_price != f64::MAX;
            let offset_set = order.lmt_price_offset != f64::MAX;
            if price_set == offset_set {
                return refuse("You must specify one value: limit price or limit price offset value.");
            }
        }
        // Midprice outside regular hours: refused whatever the time of day
        // (the flag alone, reference refusal 10210, `jextend.bH.S()@7521`).
        if matches!(order_type.as_str(), "MIDPRICE" | "MIDPX") && order.outside_rth {
            return refuse("Midprice orders are not supported outside of regular trading hours.");
        }
        None
    }

    /// Algo parameters the reference refuses before sending, checked
    /// against the algo definitions the server sent (ibx#263): 439 for an
    /// algorithm they do not have, 442 for one not allowed overnight on an
    /// overnight order, 443 for a parameter the algorithm does not have,
    /// 441 for a number it cannot read, 10314 for a time it cannot read,
    /// 145 for a value not in the parameter's legal values, 441 for a
    /// number out of its bounds or a required parameter with no value
    /// (`crate::control::algo::refusal`). Nothing is checked before the
    /// first definitions came.
    /// The warnings 2174 of the time parameters the check reached, given
    /// with no zone, go to `warnings`, in order (ibx#263).
    pub fn algo_definition_refusal(order: &ApiOrder, exchange: &str, reference: &crate::bridge::ReferenceState,
        warnings: &mut Vec<(i64, String)>) -> Option<(i64, String)>
    {
        if order.algo_strategy.is_empty() {
            return None;
        }
        let values: Vec<(&str, &str)> = order.algo_params.iter()
            .map(|tv| (tv.tag.as_str(), tv.value.as_str())).collect();
        // An overnight order: the overnight exchanges, or includeOvernight
        // (`jfix.R.C`, `jattrib.Attributes.bl`).
        let overnight = matches!(exchange, "OVERNIGHT" | "IBEOS") || order.include_overnight;
        reference.algo_refusal(&order.algo_strategy, &values, overnight, warnings)
    }

    /// A limit price that is not a number is off the contract's price
    /// grid: error 110 and nothing sent, as the reference
    /// (`trader.common.b9.a(OrderCreator, o)`; ibx#263, captured
    /// 02/10/2026 on a LMT and an Adaptive LMT). ibx used to send it as
    /// a price of 0.
    pub fn price_refusal(order: &ApiOrder) -> Option<(i64, String)> {
        order.lmt_price.is_nan()
            .then(|| (PRICE_VARIATION.0, PRICE_VARIATION.1.to_string()))
    }

    /// Status to report with a fill that leaves part of the order open.
    /// The reference keeps the order's working status on a fill and has no
    /// partially-filled status: a fill on an order last reported as
    /// PreSubmitted stays PreSubmitted, otherwise Submitted (ib-agent#192 C8).
    pub fn partial_fill_status(&self, order_id: OrderId) -> &'static str {
        match self.open_orders.lock().unwrap().get(&order_id).map(|t| t.status.as_str()) {
            Some("PreSubmitted") => "PreSubmitted",
            _ => "Submitted",
        }
    }

    /// Order type of a tracked order, as the caller placed it.
    pub fn tracked_order_type(&self, order_id: OrderId) -> Option<String> {
        self.open_orders.lock().unwrap().get(&order_id).map(|t| t.order.order_type.clone())
    }

    /// Keep a what-if preview for its answer (ibx#462).
    pub fn track_what_if(&self, order_id: OrderId, contract: ApiContract, order: ApiOrder) {
        // A what-if goes through the same id record as an order
        // (`jextend.bH.Z()`).
        self.note_order_id(order_id);
        self.what_if_orders.lock().unwrap().entry(order_id).or_default().push_back((contract, order));
    }

    /// The contract and order of the oldest what-if preview of `order_id`
    /// in flight, removed (ibx#462).
    pub fn take_what_if(&self, order_id: OrderId) -> Option<(ApiContract, ApiOrder)> {
        let mut previews = self.what_if_orders.lock().unwrap();
        let queue = previews.get_mut(&order_id)?;
        let first = queue.pop_front();
        if queue.is_empty() { previews.remove(&order_id); }
        first
    }

    /// The contract and order of the oldest what-if preview of `order_id`
    /// in flight, kept: the reply was not the last one (ibx#462).
    pub fn peek_what_if(&self, order_id: OrderId) -> Option<(ApiContract, ApiOrder)> {
        self.what_if_orders.lock().unwrap().get(&order_id).and_then(|q| q.front().cloned())
    }

    /// The order state of a what-if reply as the reference reports it
    /// (ibx#462, captured 02/10/2026): each margin value as the double's
    /// shortest text (`954397.0`, `7440.699999999999`, `1093814945.9`),
    /// each change as after minus before in doubles, empty when the server
    /// sent none; a commission it did not send, the minimum and maximum
    /// commissions and the outside-hours values unset (the maximum
    /// double).
    pub fn what_if_order_state(s: &WhatIfState) -> ApiOrderState {
        let text = |v: Option<f64>| v.map(|v| format!("{:?}", v)).unwrap_or_default();
        let change = |before: Option<f64>, after: Option<f64>| match (before, after) {
            (Some(b), Some(a)) => format!("{:?}", a - b),
            _ => String::new(),
        };
        ApiOrderState {
            status: s.status.clone(),
            init_margin_before: text(s.init_margin_before),
            maint_margin_before: text(s.maint_margin_before),
            equity_with_loan_before: text(s.equity_with_loan_before),
            init_margin_change: change(s.init_margin_before, s.init_margin_after),
            maint_margin_change: change(s.maint_margin_before, s.maint_margin_after),
            equity_with_loan_change: change(s.equity_with_loan_before, s.equity_with_loan_after),
            init_margin_after: text(s.init_margin_after),
            maint_margin_after: text(s.maint_margin_after),
            equity_with_loan_after: text(s.equity_with_loan_after),
            commission_and_fees: s.commission.unwrap_or(f64::MAX),
            min_commission_and_fees: f64::MAX,
            max_commission_and_fees: f64::MAX,
            commission_and_fees_currency: s.commission_currency.clone(),
            warning_text: s.warning_text.clone(),
            margin_currency: s.margin_currency.clone(),
            init_margin_before_outside_rth: f64::MAX,
            maint_margin_before_outside_rth: f64::MAX,
            equity_with_loan_before_outside_rth: f64::MAX,
            init_margin_change_outside_rth: f64::MAX,
            maint_margin_change_outside_rth: f64::MAX,
            equity_with_loan_change_outside_rth: f64::MAX,
            init_margin_after_outside_rth: f64::MAX,
            maint_margin_after_outside_rth: f64::MAX,
            equity_with_loan_after_outside_rth: f64::MAX,
            suggested_size: s.suggested_size.clone(),
            ..Default::default()
        }
    }

    /// A tracked order as the caller placed it (the order a modify is
    /// checked against).
    pub fn tracked_order(&self, order_id: OrderId) -> Option<ApiOrder> {
        self.open_orders.lock().unwrap().get(&order_id).map(|t| t.order.clone())
    }

    /// The working order a `placeOrder` of `order_id` modifies: the order
    /// this session placed, or this client's own working order of an
    /// earlier session, which the server reported at the logon (ibx#538:
    /// the reference finds an order by its client id and order id, so that
    /// order is the order, and a placeOrder of its id changes it).
    pub fn working_order(&self, shared: &SharedState, order_id: OrderId) -> Option<ApiOrder> {
        self.tracked_order(order_id).or_else(|| {
            let me = self.client_id.load(Ordering::Relaxed);
            shared.orders.get_order_info(order_id)
                .filter(|info| is_open_status(&info.order_state.status) && i64::from(info.order.client_id) == me)
                .map(|info| info.order)
        })
    }

    /// The contract a tracked order was placed with.
    pub fn tracked_contract(&self, order_id: OrderId) -> Option<ApiContract> {
        self.open_orders.lock().unwrap().get(&order_id).map(|t| t.contract.clone())
    }

    /// The order kind with its prices, as the extended submit path builds it
    /// from the same `Order` fields. Used for a replace, which restates the
    /// order type and its prices (ibx#247).
    pub fn order_kind(order: &ApiOrder) -> Result<OrderKind, String> {
        let scale = price_or_zero;
        if !order.adjusted_order_type.is_empty() {
            let adjusted = match order.adjusted_order_type.to_uppercase().as_str() {
                "STP" => AdjustedOrderType::Stop,
                "STP LMT" => AdjustedOrderType::StopLimit,
                "TRAIL" => AdjustedOrderType::Trail,
                "TRAIL LIMIT" => AdjustedOrderType::TrailLimit,
                other => return Err(format!("unknown adjustedOrderType '{}'", other)),
            };
            let adj_trail = if order.adjusted_trailing_amount == f64::MAX {
                0.0
            } else {
                order.adjusted_trailing_amount
            };
            return Ok(OrderKind::AdjustableStop {
                stop_price: scale(order.aux_price),
                trigger_price: scale(order.trigger_price),
                adjusted_order_type: adjusted,
                adjusted_stop_price: scale(order.adjusted_stop_price),
                adjusted_stop_limit_price: scale(order.adjusted_stop_limit_price),
                adjusted_trailing_amount: scale(adj_trail),
                adjustable_trailing_unit: order.adjustable_trailing_unit,
            });
        }
        let trail_stop = scale(order.trail_stop_price);
        Ok(match order.order_type.to_uppercase().as_str() {
            "MKT" => OrderKind::Market,
            "LMT" => OrderKind::Limit { price: scale(order.lmt_price) },
            "STP" => OrderKind::Stop { stop_price: scale(order.aux_price) },
            "STP LMT" => OrderKind::StopLimit {
                price: scale(order.lmt_price), stop_price: scale(order.aux_price),
            },
            "TRAIL" => {
                if is_given(order.trailing_percent) && order.trailing_percent > 0.0 {
                    OrderKind::TrailPct {
                        trail_percent: crate::api::types::price_from_f64(order.trailing_percent),
                        trail_stop_price: trail_stop,
                    }
                } else {
                    OrderKind::TrailingStop { trail_amt: scale(order.aux_price), trail_stop_price: trail_stop }
                }
            }
            "TRAIL LIMIT" => {
                // Exactly one of the two (refused otherwise, ibx#468): the
                // offset, or the absolute limit price (ib-agent#194).
                let (lmt_offset, lmt_price) = if order.lmt_price_offset != f64::MAX {
                    (scale(order.lmt_price_offset), None)
                } else {
                    (0, Some(scale(order.lmt_price)))
                };
                OrderKind::TrailingStopLimit {
                    lmt_offset,
                    lmt_price,
                    trail_amt: scale(order.aux_price),
                    trail_stop_price: trail_stop,
                }
            }
            "MOC" => OrderKind::Moc,
            "LOC" => OrderKind::Loc { price: scale(order.lmt_price) },
            "MIT" => OrderKind::Mit { stop_price: scale(order.aux_price) },
            "LIT" => OrderKind::Lit { price: scale(order.lmt_price), stop_price: scale(order.aux_price) },
            "MTL" | "BOX TOP" => OrderKind::Mtl,
            "MKT PRT" => OrderKind::MktPrt,
            "STP PRT" => OrderKind::StpPrt { stop_price: scale(order.aux_price) },
            "REL" => OrderKind::Rel { price: scale(aux_or_zero(order.lmt_price)), offset: scale(order.aux_price) },
            "PEG MKT" => OrderKind::PegMkt {
                price: scale(aux_or_zero(order.lmt_price)), offset: scale(aux_or_zero(order.aux_price)),
            },
            "PEG MID" | "PEG MIDPT" => OrderKind::PegMid {
                price: scale(aux_or_zero(order.lmt_price)), offset: scale(aux_or_zero(order.aux_price)),
            },
            "MIDPX" | "MIDPRICE" => OrderKind::MidPrice { price_cap: scale(order.lmt_price) },
            "SNAP MKT" => OrderKind::SnapMkt { offset: scale(aux_or_zero(order.aux_price)) },
            "SNAP MID" | "SNAP MIDPT" => OrderKind::SnapMid { offset: scale(aux_or_zero(order.aux_price)) },
            "SNAP PRI" | "SNAP PRIM" => OrderKind::SnapPri { offset: scale(aux_or_zero(order.aux_price)) },
            "PEG BENCH" => Self::peg_bench_kind(order),
            "TRAIL MIT" | "TRAIL LIT" | "PEG BEST" | "RPI" | "PASSV REL" => Self::new_type_kind(order),
            _ => return Err(format!("Unsupported order type: '{}'", order.order_type)),
        })
    }

    /// A TRAIL MIT, TRAIL LIT, PEG BEST, RPI or PASSV REL order from the
    /// API fields (ibx#469). TRAIL MIT and TRAIL LIT trail by the percent
    /// when one is given, else by the amount (the auxPrice). The RPI and
    /// PASSV REL offset is the auxPrice, 0 when unset.
    fn new_type_kind(order: &ApiOrder) -> OrderKind {
        let scale = price_or_zero;
        let trail_stop_price = scale(order.trail_stop_price);
        let percent = is_given(order.trailing_percent) && order.trailing_percent > 0.0;
        let trail = if percent { crate::api::types::price_from_f64(order.trailing_percent) } else { scale(order.aux_price) };
        match order.order_type.to_uppercase().as_str() {
            "TRAIL MIT" => OrderKind::TrailMit { trail, percent, trail_stop_price },
            "TRAIL LIT" => OrderKind::TrailLit { price: scale(order.lmt_price), trail, percent, trail_stop_price },
            "PEG BEST" => OrderKind::PegBest { price: scale(aux_or_zero(order.lmt_price)) },
            "PASSV REL" => OrderKind::PassvRel {
                price: scale(aux_or_zero(order.lmt_price)), offset: scale(aux_or_zero(order.aux_price)),
            },
            _ => OrderKind::Rpi { price: scale(order.lmt_price), offset: scale(aux_or_zero(order.aux_price)) },
        }
    }

    /// A pegged-to-benchmark order from the API fields (ibx#415): the
    /// starting price, the stock reference price, the reference contract,
    /// the pegged change and its direction, the reference change. An
    /// unset price is 0 (not sent).
    fn peg_bench_kind(order: &ApiOrder) -> OrderKind {
        let scale = |v: f64| crate::api::types::price_from_f64(aux_or_zero(v));
        OrderKind::PegBench {
            starting_price: scale(order.starting_price),
            stock_ref_price: scale(order.stock_ref_price),
            ref_con_id: i64::from(order.reference_contract_id.max(0)),
            is_peg_decrease: order.is_pegged_change_amount_decrease,
            pegged_change_amount: scale(order.pegged_change_amount),
            ref_change_amount: scale(order.reference_change_amount),
        }
    }

    /// The time in force the reference holds and reports for a placed
    /// order (captured 26/09/2026 and 28/09/2026, ibx#467): OVERNIGHT and
    /// OVERNIGHT + DAY go out and are held as DAY; a DAY order with
    /// includeOvernight is held as OVERNIGHT + DAY. An empty one (the
    /// official API's default) goes out and is held as DAY. Any other value
    /// as is.
    pub fn held_tif(order: &ApiOrder) -> &str {
        match order.tif.as_str() {
            "" | "DAY" | TIF_OVERNIGHT_DAY if order.include_overnight => TIF_OVERNIGHT_DAY,
            "" | TIF_OVERNIGHT | TIF_OVERNIGHT_DAY => "DAY",
            other => other,
        }
    }

    /// The reference's refusal of a modify whose time in force is not the
    /// one it holds for the order, where it was seen (captured 28/09/2026,
    /// ibx#467): a modify that restates OVERNIGHT or OVERNIGHT + DAY on an
    /// order held as DAY, and any other time in force on an order held as
    /// OVERNIGHT + DAY. Error 462 with the new time in force; other changes
    /// (DAY to GTC, ib-agent#192 A4a) go out. `held` is the tracked order's.
    pub fn modify_tif_refusal(order: &ApiOrder, held: &str) -> Option<(i64, String)> {
        let new = if order.tif.is_empty() { "DAY" } else { order.tif.as_str() };
        let overnight = |t: &str| t == TIF_OVERNIGHT || t == TIF_OVERNIGHT_DAY;
        (new != held && (overnight(new) || overnight(held))).then(|| {
            (462, format!("Order modify failed. Cannot change to the new Time in Force.{}", new))
        })
    }

    /// The reference's refusals of a modify that changes what an order
    /// cannot change (ibx#463), in the order it checks them: the side
    /// (105), the OCA group when both are set (10326), the OCA type when
    /// both are set (10327). `working` is the order as it was placed.
    pub fn modify_change_refusal(order: &ApiOrder, working: &ApiOrder) -> Option<(i64, String)> {
        if order.side().ok() != working.side().ok() {
            return Some((105, "Order being modified does not match original order".into()));
        }
        if !order.oca_group.is_empty() && !working.oca_group.is_empty() && order.oca_group != working.oca_group {
            return Some((10326, "OCA group revision is not allowed".into()));
        }
        let oca_type = |o: &ApiOrder| (1..=4).contains(&o.oca_type).then_some(o.oca_type);
        if let (Some(new), Some(old)) = (oca_type(order), oca_type(working)) {
            if new != old {
                return Some((10327, "OCA group type revision is not allowed".into()));
            }
        }
        None
    }

    /// Build the replace for an order that is already working, from the full
    /// `Order` the caller passed (ibx#247 ibx#324 ibx#334 ibx#349).
    ///
    /// The reference refuses a change of side or OCA group (ibx#463) and a
    /// change of order type (error 329, ib-agent#192 A4b) before sending
    /// anything; so does this. `working` is the order as it was placed.
    pub fn build_modify_request(
        order: &ApiOrder,
        order_id: OrderId,
        working: &ApiOrder,
    ) -> Result<ModifyPlan, String> {
        if let Some((code, message)) = Self::modify_change_refusal(order, working) {
            return Ok(ModifyPlan::Refused { code, message });
        }
        if !working.order_type.eq_ignore_ascii_case(&order.order_type) {
            return Ok(ModifyPlan::Refused {
                code: 329,
                message: format!(
                    "Order modify failed. Cannot change to the new order type.{}",
                    order.order_type.to_uppercase(),
                ),
            });
        }
        if let Some((code, message)) = Self::modify_tif_refusal(order, &working.tif) {
            return Ok(ModifyPlan::Refused { code, message });
        }
        Ok(ModifyPlan::Send(ControlCommand::Order(OrderRequest::Modify {
            new_order_id: order_id,
            order_id,
            qty: order.total_quantity as u32,
            kind: Self::order_kind(order)?,
            tif: order.tif_byte(),
            // The algo, restated by the reference's replace (ibx#263).
            attrs: crate::types::OrderAttrs { algo: Self::order_algo(order)?, ..order.attrs() },
        })))
    }

    /// The order's algo, None when it has none: the Adaptive priority
    /// (Normal when not given), or the parameters of another algo.
    fn order_algo(order: &ApiOrder) -> Result<Option<crate::types::OrderAlgo>, String> {
        use crate::types::OrderAlgo;
        if order.algo_strategy.is_empty() {
            return Ok(None);
        }
        if order.algo_strategy.eq_ignore_ascii_case("Adaptive") {
            let priority = match order.algo_params.iter()
                .find(|tv| tv.tag == "adaptivePriority")
                .map(|tv| tv.value.as_str())
            {
                Some("Patient") => AdaptivePriority::Patient,
                Some("Urgent") => AdaptivePriority::Urgent,
                _ => AdaptivePriority::Normal,
            };
            return Ok(Some(OrderAlgo::Adaptive(priority)));
        }
        crate::api::client::parse_algo_params(&order.algo_strategy, &order.algo_params)
            .map(|params| Some(OrderAlgo::Params(params)))
    }

    /// Build an `OrderRequest` from an API `Order`, handling all order types.
    /// This is the shared order-type match block used by both Rust and Python.
    pub fn build_order_request(
        order: &ApiOrder,
        order_id: OrderId,
        instrument: InstrumentId,
    ) -> Result<ControlCommand, String> {
        Self::build_order_request_with(order, order_id, instrument, None)
    }

    /// `build_order_request` with the combo of a BAG order (ibx#470), which
    /// takes the extended encoder.
    fn build_order_request_with(
        order: &ApiOrder,
        order_id: OrderId,
        instrument: InstrumentId,
        combo: Option<Box<ComboSpec>>,
    ) -> Result<ControlCommand, String> {
        // A what-if first (ibx#462): the order as it would be placed, of
        // any type or algo, previewed, never placed.
        if order.what_if {
            let real = ApiOrder { what_if: false, ..order.clone() };
            return match Self::build_order_request_with(&real, order_id, instrument, combo)? {
                ControlCommand::Order(request) => Ok(ControlCommand::Order(
                    OrderRequest::SubmitWhatIf { request: Box::new(request) })),
                other => Ok(other),
            };
        }
        let side = order.side()?;
        let qty = order.total_quantity as u32;
        let order_type = order.order_type.to_uppercase();

        // An algo rides the order's own type and price fields, as the
        // reference writes it (ibx#263: an Adaptive STP goes out as a stop
        // with its stop price and the Adaptive block); it takes the
        // extended path, which writes the algo block.
        let algo = Self::order_algo(order)?;

        // Every order type must carry extended attributes and a non-DAY tif
        // when the caller sets them — dropping them silently produced
        // unlinked, immediate-DAY bracket children (ibx#224). An empty tif
        // is treated as DAY, matching the official API default.
        let extended = order.has_extended_attrs()
            || !matches!(order.tif.as_str(), "" | "DAY")
            || algo.is_some()
            || combo.is_some();
        let attrs = || crate::types::OrderAttrs { algo: algo.clone(), combo: combo.clone(), ..order.attrs() };
        let ex = |kind: OrderKind| OrderRequest::SubmitEx {
            order_id, instrument, side, qty,
            kind,
            tif: order.tif_byte(),
            attrs: attrs(),
        };

        // Adjustable stop: a base STP that converts to another order type when
        // its trigger is reached. Signalled by a non-empty adjustedOrderType,
        // which is empty on every ordinary order, so this affects nothing else.
        // A Trail/TrailLimit conversion carries the trailing amount + unit
        // (tags 6260/6269, ib-agent#167). (ibx#225)
        // With extended attributes or a non-DAY tif it takes the extended path,
        // so a bracket child keeps its parent, OCA group and tif (ibx#240).
        if !order.adjusted_order_type.is_empty() {
            let adjusted = match order.adjusted_order_type.to_uppercase().as_str() {
                "STP" => AdjustedOrderType::Stop,
                "STP LMT" => AdjustedOrderType::StopLimit,
                "TRAIL" => AdjustedOrderType::Trail,
                "TRAIL LIMIT" => AdjustedOrderType::TrailLimit,
                other => return Err(format!("unknown adjustedOrderType '{}'", other)),
            };
            let scale = price_or_zero;
            // adjusted_trailing_amount defaults to f64::MAX when unset.
            let adj_trail = if order.adjusted_trailing_amount == f64::MAX {
                0.0
            } else {
                order.adjusted_trailing_amount
            };
            let stop_price = scale(order.aux_price);
            let trigger_price = scale(order.trigger_price);
            let adjusted_stop_price = scale(order.adjusted_stop_price);
            let adjusted_stop_limit_price = scale(order.adjusted_stop_limit_price);
            let adjusted_trailing_amount = scale(adj_trail);
            let adjustable_trailing_unit = order.adjustable_trailing_unit;
            let req = if extended {
                ex(OrderKind::AdjustableStop {
                    stop_price, trigger_price, adjusted_order_type: adjusted,
                    adjusted_stop_price, adjusted_stop_limit_price,
                    adjusted_trailing_amount, adjustable_trailing_unit,
                })
            } else {
                OrderRequest::SubmitAdjustableStop {
                    order_id, instrument, side, qty,
                    stop_price, trigger_price, adjusted_order_type: adjusted,
                    adjusted_stop_price, adjusted_stop_limit_price,
                    adjusted_trailing_amount, adjustable_trailing_unit,
                }
            };
            return Ok(ControlCommand::Order(req));
        }

        let req = match order_type.as_str() {
            "MKT" => {
                if extended { ex(OrderKind::Market) }
                else { OrderRequest::SubmitMarket { order_id, instrument, side, qty } }
            }
            "LMT" => {
                let price = price_or_zero(order.lmt_price);
                if extended {
                    OrderRequest::SubmitLimitEx {
                        order_id, instrument, side, qty, price,
                        tif: order.tif_byte(),
                        attrs: attrs(),
                    }
                } else {
                    OrderRequest::SubmitLimit { order_id, instrument, side, qty, price }
                }
            }
            "STP" => {
                let stop = price_or_zero(order.aux_price);
                if extended { ex(OrderKind::Stop { stop_price: stop }) }
                else { OrderRequest::SubmitStop { order_id, instrument, side, qty, stop_price: stop } }
            }
            "STP LMT" => {
                let price = price_or_zero(order.lmt_price);
                let stop = price_or_zero(order.aux_price);
                if extended { ex(OrderKind::StopLimit { price, stop_price: stop }) }
                else { OrderRequest::SubmitStopLimit { order_id, instrument, side, qty, price, stop_price: stop } }
            }
            "TRAIL" => {
                // Optional initial stop trigger (tag 6117); default f64::MAX = unset.
                let trail_stop = price_or_zero(order.trail_stop_price);
                if is_given(order.trailing_percent) && order.trailing_percent > 0.0 {
                    let pct = price_or_zero(order.trailing_percent);
                    if extended {
                        OrderRequest::SubmitTrailingStopPctEx {
                            order_id, instrument, side, qty, trail_percent: pct,
                            tif: order.tif_byte(),
                            attrs: attrs(),
                            trail_stop_price: trail_stop,
                        }
                    } else {
                        OrderRequest::SubmitTrailingStopPct { order_id, instrument, side, qty, trail_percent: pct, trail_stop_price: trail_stop }
                    }
                } else {
                    let trail = price_or_zero(order.aux_price);
                    if extended { ex(OrderKind::TrailingStop { trail_amt: trail, trail_stop_price: trail_stop }) }
                    else { OrderRequest::SubmitTrailingStop { order_id, instrument, side, qty, trail_amt: trail, trail_stop_price: trail_stop } }
                }
            }
            "TRAIL LIMIT" => {
                // The offset (6370), or the absolute limit price (44, no
                // 6370), as the caller gave it (ib-agent#194): exactly one of
                // them, else refused before this (ibx#468).
                let (lmt_offset, lmt_price) = if order.lmt_price_offset != f64::MAX {
                    (price_or_zero(order.lmt_price_offset), None)
                } else {
                    (0, Some(price_or_zero(order.lmt_price)))
                };
                let trail = price_or_zero(order.aux_price);
                let trail_stop = price_or_zero(order.trail_stop_price);
                if extended { ex(OrderKind::TrailingStopLimit { lmt_offset, lmt_price, trail_amt: trail, trail_stop_price: trail_stop }) }
                else { OrderRequest::SubmitTrailingStopLimit { order_id, instrument, side, qty, lmt_offset, lmt_price, trail_amt: trail, trail_stop_price: trail_stop } }
            }
            "MOC" => {
                if extended { ex(OrderKind::Moc) }
                else { OrderRequest::SubmitMoc { order_id, instrument, side, qty } }
            }
            "LOC" => {
                let price = price_or_zero(order.lmt_price);
                if extended { ex(OrderKind::Loc { price }) }
                else { OrderRequest::SubmitLoc { order_id, instrument, side, qty, price } }
            }
            "MIT" => {
                let stop = price_or_zero(order.aux_price);
                if extended { ex(OrderKind::Mit { stop_price: stop }) }
                else { OrderRequest::SubmitMit { order_id, instrument, side, qty, stop_price: stop } }
            }
            "LIT" => {
                let price = price_or_zero(order.lmt_price);
                let stop = price_or_zero(order.aux_price);
                if extended { ex(OrderKind::Lit { price, stop_price: stop }) }
                else { OrderRequest::SubmitLit { order_id, instrument, side, qty, price, stop_price: stop } }
            }
            "MTL" | "BOX TOP" => {
                if extended { ex(OrderKind::Mtl) }
                else { OrderRequest::SubmitMtl { order_id, instrument, side, qty } }
            }
            "MKT PRT" => {
                if extended { ex(OrderKind::MktPrt) }
                else { OrderRequest::SubmitMktPrt { order_id, instrument, side, qty } }
            }
            "STP PRT" => {
                let stop = price_or_zero(order.aux_price);
                if extended { ex(OrderKind::StpPrt { stop_price: stop }) }
                else { OrderRequest::SubmitStpPrt { order_id, instrument, side, qty, stop_price: stop } }
            }
            "REL" => {
                // The price cap when given, as the reference (ibx#263).
                let offset = price_or_zero(order.aux_price);
                let price = price_or_zero(order.lmt_price);
                if extended || price > 0 { ex(OrderKind::Rel { price, offset }) }
                else { OrderRequest::SubmitRel { order_id, instrument, side, qty, offset } }
            }
            // The limit price when given, and the offset (ibx#414).
            "PEG MKT" => {
                let price = price_or_zero(order.lmt_price);
                let offset = price_or_zero(order.aux_price);
                if extended { ex(OrderKind::PegMkt { price, offset }) }
                else { OrderRequest::SubmitPegMkt { order_id, instrument, side, qty, price, offset } }
            }
            "PEG MID" | "PEG MIDPT" => {
                let price = price_or_zero(order.lmt_price);
                let offset = price_or_zero(order.aux_price);
                if extended { ex(OrderKind::PegMid { price, offset }) }
                else { OrderRequest::SubmitPegMid { order_id, instrument, side, qty, price, offset } }
            }
            "MIDPX" | "MIDPRICE" => {
                let cap = price_or_zero(order.lmt_price);
                if extended { ex(OrderKind::MidPrice { price_cap: cap }) }
                else { OrderRequest::SubmitMidPrice { order_id, instrument, side, qty, price_cap: cap } }
            }
            // One encoder for every pegged-to-benchmark order: its
            // reference exchange rides the attributes (ibx#415).
            "PEG BENCH" => ex(Self::peg_bench_kind(order)),
            // One encoder for each of these types too (ibx#469).
            "TRAIL MIT" | "TRAIL LIT" | "PEG BEST" | "RPI" | "PASSV REL" => ex(Self::new_type_kind(order)),
            // The offset is the API auxPrice, 0.00 when unset (ibx#413).
            "SNAP MKT" => {
                let offset = price_or_zero(order.aux_price);
                if extended { ex(OrderKind::SnapMkt { offset }) }
                else { OrderRequest::SubmitSnapMkt { order_id, instrument, side, qty, offset } }
            }
            "SNAP MID" | "SNAP MIDPT" => {
                let offset = price_or_zero(order.aux_price);
                if extended { ex(OrderKind::SnapMid { offset }) }
                else { OrderRequest::SubmitSnapMid { order_id, instrument, side, qty, offset } }
            }
            "SNAP PRI" | "SNAP PRIM" => {
                let offset = price_or_zero(order.aux_price);
                if extended { ex(OrderKind::SnapPri { offset }) }
                else { OrderRequest::SubmitSnapPri { order_id, instrument, side, qty, offset } }
            }
            _ => return Err(format!("Unsupported order type: '{}'", order.order_type)),
        };

        Ok(ControlCommand::Order(req))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ibx#486: whyHeld as jclient.pe.iK(): only PreSubmitted; "child" while
    // the parent is not done with a fill; "trigger" for the stop types.
    #[test]
    fn why_held_child_and_trigger() {
        let core = ClientCore::new();
        assert_eq!(core.why_held("PreSubmitted", "STP", 0), "trigger");
        assert_eq!(core.why_held("PreSubmitted", "trail limit", 0), "trigger");
        assert_eq!(core.why_held("Submitted", "STP", 0), "");
        assert_eq!(core.why_held("PreSubmitted", "LMT", 0), "");
        assert_eq!(core.why_held("PreSubmitted", "STP", 3), "child,trigger");
        let report = |status: &str, filled: f64| OrderReport {
            view: None, status: status.into(), filled, remaining: 1.0 - filled, avg_fill_price: 0.0, perm_id: 0,
            parent_id: 0, last_fill_price: 0.0, client_id: 0, why_held: String::new(),
        };
        core.remember_report(3, report("Cancelled", 0.0));
        assert_eq!(core.why_held("PreSubmitted", "LMT", 3), "child", "cancelled with nothing filled");
        core.remember_report(3, report("Filled", 1.0));
        assert_eq!(core.why_held("PreSubmitted", "STP", 3), "trigger");
    }

    // ibx#486: the reference's hash map order (BASE after USD, the order of
    // the account_summary capture of 26/09/2026) and its account values.
    #[test]
    fn java_map_order_and_account_values() {
        let keys = |k: &[&str]| k.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(java_hash_order(&keys(&["BASE", "USD"])), ["USD", "BASE"]);
        assert_eq!(java_hash_order(&keys(&["USD", "BASE", "USD"])), ["USD", "BASE"]);
        // Past twelve keys the table doubles; every key stays once.
        let many: Vec<String> = (0..20).map(|k| format!("C{k}")).collect();
        let mut got = java_hash_order(&many);
        got.sort();
        let mut want = many.clone();
        want.sort();
        assert_eq!(got, want);
        assert_eq!(java_account_value(933115.05), "933115.05");
        assert_eq!(java_account_value(953925.6599), "953925.6599");
        assert_eq!(java_account_value(1.0), "1.00");
        assert_eq!(java_account_value(-1128.69), "-1128.69");
        assert_eq!(java_account_value(0.123456789), "0.1234568");
        assert_eq!(java_account_value(9.99999999), "10.00");
    }

    // ibx#486: $LEDGER:ALL gives, at the end marker, one row per currency
    // summed over the accounts, for the account All; nothing at the ledger
    // frame; a value missing in one account counts as missing.
    #[test]
    fn ledger_all_sums_per_currency_at_the_end() {
        let core = ClientCore::new();
        let shared = SharedState::new();
        let plan = core.subscribe_account_summary(7, "All", "$LEDGER:ALL").unwrap();
        assert_eq!(plan.wire_tags, "$LEDGER");
        let row = |account: &str, currency: &str, values: Vec<(u32, f64)>| crate::bridge::LedgerRow {
            account: account.into(), currency: currency.into(), real_currency: currency.into(), values,
        };
        shared.portfolio.push_account_summary_event(crate::bridge::AccountSummaryEvent {
            sr_id: plan.sr_id.clone(), rows: vec![], ledger: true, end: false,
            ledgers: vec![row("DU1", "BASE", vec![(9806, 10.5), (6242, 1.0)]), row("DU1", "USD", vec![(9806, 10.5)]),
                row("DU2", "USD", vec![(9806, 0.25), (6242, 2.0)])],
        });
        assert!(core.prepare_account_summary(&shared).is_empty(), "no rows at the ledger frame");
        shared.portfolio.push_account_summary_event(crate::bridge::AccountSummaryEvent {
            sr_id: plan.sr_id.clone(), rows: vec![], ledger: false, end: true, ledgers: vec![],
        });
        let out = core.prepare_account_summary(&shared);
        let lines: Vec<String> = out.iter().flat_map(|b| {
            let account = b.account.clone().unwrap_or_default();
            let mut l: Vec<String> = b.rows.iter().filter(|r| matches!(r.key.as_str(), "Currency" | "CashBalance" | "AccruedCash"))
                .map(|r| format!("{account}|{}|{}|{}", r.key, r.value, r.currency)).collect();
            if b.end { l.push("end".into()); }
            l
        }).collect();
        let once = ["All|Currency|USD|USD", "All|CashBalance|10.75|USD", "All|AccruedCash|2.00|USD",
            "All|Currency|BASE|BASE", "All|CashBalance|10.50|BASE", "All|AccruedCash|1.00|BASE", "end"];
        assert_eq!(lines, [once, once].concat());
    }

    // ibx#251: a request is held only while the order replay is pending,
    // once per kind, and released in the order it was made.
    #[test]
    fn open_order_requests_are_held_until_the_replay_ends() {
        let core = ClientCore::new();
        let shared = SharedState::new();
        assert!(!core.hold_open_orders(OpenOrdersRequest::Open, &shared), "answered at once");
        shared.orders.set_open_orders_held(true);
        assert!(core.hold_open_orders(OpenOrdersRequest::All, &shared));
        assert!(core.hold_open_orders(OpenOrdersRequest::Open, &shared));
        assert!(core.hold_open_orders(OpenOrdersRequest::All, &shared), "replaces the first");
        assert!(core.released_open_orders(&shared).is_empty(), "still held");
        shared.orders.set_open_orders_held(false);
        assert_eq!(core.released_open_orders(&shared), vec![OpenOrdersRequest::All, OpenOrdersRequest::Open]);
        assert!(core.released_open_orders(&shared).is_empty(), "once");
    }
    use crate::types::SmartComponent;

    /// Shared state where instrument 0 has an exchange map.
    pub(crate) fn shared_with_components(comps: Vec<(i32, &str)>) -> SharedState {
        let s = SharedState::new();
        s.reference.observe_exchange_map(0, "9c", 1);
        s.reference.set_exchange_map("9c", 1,
            comps.into_iter().map(|(bit, letter)| SmartComponent {
                bit_number: bit,
                exchange: format!("EX{bit}"),
                exchange_letter: letter.to_string(),
            }).collect()
        );
        s
    }

    // ibx#441: the BBO exchange text is split as the reference splits it.
    #[test]
    fn bbo_exchange_split_as_the_reference() {
        assert_eq!(split_bbo_exchange("9c0001"), ("9c".to_string(), Some(1)));
        assert_eq!(split_bbo_exchange("a60001"), ("a6".to_string(), Some(1)));
        assert_eq!(split_bbo_exchange("XYZ"), ("XYZ".to_string(), None));
        assert_eq!(split_bbo_exchange(""), (String::new(), None));
        assert_eq!(split_bbo_exchange("abcd"), ("abcd".to_string(), None), "no security type 0xabcd");
        assert_eq!(split_bbo_exchange("5000a"), ("5".to_string(), Some(10)));
        assert_eq!(split_bbo_exchange("123456789"), ("123456789".to_string(), None), "longer than 8");
    }

    // ibx#441, captured 02/10/2026 (b1_441_smart_components): XYZ before
    // and after an L1 subscription gives 321; the AAPL code right after
    // tickReqParams waits for the map, 3 s later it is answered at once.
    #[test]
    fn smart_components_as_the_reference() {
        let core = ClientCore::new();
        let shared = SharedState::new();
        let invalid = Err((321, "Error validating request.-'V' : cause - Invalid BBO exchange/security type code".to_string()));
        assert_eq!(core.req_smart_components(9490, "XYZ", &shared), Some(invalid.clone()));
        shared.reference.observe_exchange_map(1, "9c", 1);
        assert_eq!(core.req_smart_components(9492, "9c0001", &shared), None, "waits for the map");
        assert_eq!(core.req_smart_components(9500, "XYZ", &shared), Some(invalid));
        assert!(core.take_smart_components(&shared).is_empty());
        let map = crate::engine::hot_loop::farm::parse_exchange_map("0/A/AMEX;9/Q/NASDAQ");
        shared.reference.set_exchange_map("9c", 1, map.clone());
        assert_eq!(core.take_smart_components(&shared), vec![(9492, Ok(map.clone()))]);
        assert_eq!(core.req_smart_components(9493, "9c0001", &shared), Some(Ok(map.clone())));
        // The code alone finds the first map of that code.
        assert_eq!(core.req_smart_components(9494, "9c", &shared), Some(Ok(map)));
        // Another security type for the same code is not known.
        assert!(matches!(core.req_smart_components(9495, "9c0006", &shared), Some(Err((321, _)))));
    }

    // ibx#441: no map within 2 s gives the reference's error.
    #[test]
    fn smart_components_wait_ends_with_an_error() {
        let core = ClientCore::new();
        let shared = SharedState::new();
        shared.reference.observe_exchange_map(1, "a6", 1);
        assert_eq!(core.req_smart_components(7, "a60001", &shared), None);
        core.smart_components_waiting.lock().unwrap()[0].3 = std::time::Instant::now();
        assert_eq!(core.take_smart_components(&shared), vec![(7, Err((2147483647,
            "Unable to retrieve smart components for BBO exchange a6 and security type STK".to_string())))]);
        assert!(core.take_smart_components(&shared).is_empty());
    }

    #[test]
    fn render_exchange_mask_zero_is_empty() {
        let s = shared_with_components(vec![(0, "Q"), (1, "N")]);
        assert_eq!(render_exchange_mask(0, 0, &s), "");
    }

    #[test]
    fn render_exchange_mask_single_bit() {
        let s = shared_with_components(vec![(0, "Q"), (1, "N"), (2, "P")]);
        assert_eq!(render_exchange_mask(0b001, 0, &s), "Q");
        assert_eq!(render_exchange_mask(0b100, 0, &s), "P");
    }

    #[test]
    fn render_exchange_mask_multiple_bits() {
        let s = shared_with_components(vec![
            (0, "Q"), (1, "N"), (2, "P"), (3, "Z"),
        ]);
        // bits 0, 2, 3 set → letters in bit-order: Q, P, Z
        assert_eq!(render_exchange_mask(0b1101, 0, &s), "QPZ");
    }

    #[test]
    fn render_exchange_mask_unknown_bit_skipped() {
        let s = shared_with_components(vec![(0, "Q")]);
        // bit 5 set, no component at bit 5 — skipped
        assert_eq!(render_exchange_mask(0b100000, 0, &s), "");
    }

    // ibx#441: no map for the contract (another contract has one, or the
    // map has not come yet): an empty text, as the reference.
    #[test]
    fn render_exchange_mask_without_the_contract_map_is_empty() {
        let s = shared_with_components(vec![(0, "Q")]);
        assert_eq!(render_exchange_mask(1, 1, &s), "");
        s.reference.observe_exchange_map(2, "a6", 1);
        assert_eq!(render_exchange_mask(1, 2, &s), "");
    }

    // ── poll_pnl regression tests (#166) ──

    fn seed_pnl_position(
        core: &ClientCore,
        shared: &SharedState,
        con_id: i64,
        iid: InstrumentId,
        position: i64,
        avg_cost_dollars: f64,
        last_dollars: f64,
        close_dollars: f64,
    ) {
        core.con_id_to_instrument.lock().unwrap().insert(con_id, iid);
        core.instrument_to_req.lock().unwrap().insert(iid, vec![1]);
        shared.portfolio.set_position_info(PositionInfo {
            con_id,
            position_fixed: position as i64 * crate::types::QTY_SCALE,
            avg_cost: (avg_cost_dollars * PRICE_SCALE_F) as i64,
            symbol: format!("SYM{con_id}"),
            sec_type: "STK".into(),
            currency: "USD".into(),
            multiplier: String::new(),
            ..Default::default()
        });
        let mut q = Quote::default();
        q.last = (last_dollars * PRICE_SCALE_F) as i64;
        q.close = (close_dollars * PRICE_SCALE_F) as i64;
        shared.market.push_quote(iid, &q);
    }

    #[test]
    fn poll_pnl_no_subscription_returns_none() {
        let core = ClientCore::new();
        let shared = SharedState::new();
        assert!(core.poll_pnl(&shared).is_empty());
    }

    #[test]
    fn poll_pnl_intraday_opened_position_fires_callback() {
        // #166: flat-at-midnight account opens an intraday position.
        // Before fix: poll_pnl early-returned on empty seeds → no callback.
        // After fix: position iterated, money_traded synthesized, daily P&L = unrealized.
        let core = ClientCore::new();
        let shared = SharedState::new();
        core.subscribe_pnl(42);

        // 1 share bought at $735.00, now $735.07. No midnight seed (flat at midnight).
        seed_pnl_position(&core, &shared, 756733, 0, 1, 735.00, 735.07, 0.0);

        let update = core.poll_pnl(&shared).pop().expect("callback must fire");
        assert_eq!(update.req_id, 42);
        assert!((update.daily_pnl - 0.07).abs() < 1e-6, "daily={}", update.daily_pnl);
        assert!((update.unrealized_pnl - 0.07).abs() < 1e-6);
        assert!((update.realized_pnl - 0.0).abs() < 1e-6);
    }

    #[test]
    fn poll_pnl_overnight_position_with_seed_unchanged() {
        let core = ClientCore::new();
        let shared = SharedState::new();
        core.subscribe_pnl(99);

        // Held 10 SPY through midnight: qty_midnight=10, prev_close=$730, avg_cost=$700.
        // No fills today (money_traded=0). Current price $735.
        seed_pnl_position(&core, &shared, 756733, 0, 10, 700.00, 735.00, 730.00);
        shared.portfolio.set_midnight_seeds(vec![MidnightSeed {
            con_id: 756733,
            qty_midnight_fixed: (10) as i64 * crate::types::QTY_SCALE,
            money_traded: 0.0,
            realized_pnl: 0.0,
        }]);

        let update = core.poll_pnl(&shared).pop().expect("callback must fire");
        // daily = 10×735 - 10×730 - 0 = 50
        assert!((update.daily_pnl - 50.0).abs() < 1e-6, "daily={}", update.daily_pnl);
        // unrealized = 10 × (735 - 700) = 350
        assert!((update.unrealized_pnl - 350.0).abs() < 1e-6);
    }

    #[test]
    fn poll_pnl_seeded_position_traded_intraday_uses_signed_net_cash() {
        // ibx#221 / ib-agent#163: a position held at midnight AND traded intraday
        // carries a non-zero moneyTradedSinceMidnight (6822), signed SELL+/BUY-.
        // The daily formula must ADD it. Sold 3 of 10 at $110 (avg $100): the
        // seed carries +330 net cash (sell proceeds) and +30 realized.
        let core = ClientCore::new();
        let shared = SharedState::new();
        core.subscribe_pnl(31);

        // Now holding 7 (was 10 at midnight), avg $100, last $110, prev close $100.
        seed_pnl_position(&core, &shared, 1, 0, 7, 100.00, 110.00, 100.00);
        shared.portfolio.set_midnight_seeds(vec![MidnightSeed {
            con_id: 1,
            qty_midnight_fixed: (10) as i64 * crate::types::QTY_SCALE,
            money_traded: 330.0,   // +330 = sold 3 @ $110 (wire sign, SELL positive)
            realized_pnl: 30.0,
        }]);

        let update = core.poll_pnl(&shared).pop().expect("callback must fire");
        // daily = 7×110 - 10×100 + 330 = 100 (70 remaining unrealized + 30 realized)
        assert!((update.daily_pnl - 100.0).abs() < 1e-6, "daily={}", update.daily_pnl);
        // unrealized = 7 × (110 - 100) = 70
        assert!((update.unrealized_pnl - 70.0).abs() < 1e-6, "unreal={}", update.unrealized_pnl);
        assert!((update.realized_pnl - 30.0).abs() < 1e-6, "real={}", update.realized_pnl);
    }

    #[test]
    fn poll_pnl_change_detection_suppresses_duplicate() {
        let core = ClientCore::new();
        let shared = SharedState::new();
        core.subscribe_pnl(7);
        seed_pnl_position(&core, &shared, 1, 0, 1, 100.0, 101.0, 0.0);
        assert!(!core.poll_pnl(&shared).is_empty());
        // Same inputs → no callback.
        assert!(core.poll_pnl(&shared).is_empty());
    }

    #[test]
    fn poll_pnl_without_market_data_uses_marks_and_unset() {
        // The paper case (25/09/2026): no quote, no server P&L keys. The
        // unrealized P&L comes from the server's mark; the realized P&L from
        // the seed; the daily P&L of a position held at midnight needs the
        // previous close, so it is unset (it was 0.0 for all three).
        let core = ClientCore::new();
        let shared = SharedState::new();
        core.subscribe_pnl(21);
        shared.portfolio.set_position_info(PositionInfo {
            con_id: 756733, position_fixed: 31 * crate::types::QTY_SCALE,
            avg_cost: (764.59 * PRICE_SCALE_F) as i64, ..Default::default()
        });
        shared.portfolio.set_position_marks(756733, (770.0 * PRICE_SCALE_F) as i64, 0, 0, 0);
        shared.portfolio.set_midnight_seeds(vec![MidnightSeed {
            con_id: 756733, qty_midnight_fixed: 31 * crate::types::QTY_SCALE, money_traded: 0.0, realized_pnl: 37.12,
        }]);
        let u = core.poll_pnl(&shared).pop().unwrap();
        assert_eq!(u.daily_pnl, f64::MAX);
        assert!((u.unrealized_pnl - 31.0 * (770.0 - 764.59)).abs() < 1e-3, "unreal={}", u.unrealized_pnl);
        assert!((u.realized_pnl - 37.12).abs() < 1e-9);
    }

    // The same inputs give the same P&L, whatever the order of the positions
    // in the stores: no repeated callback.
    #[test]
    fn poll_pnl_does_not_repeat_unchanged_values() {
        let core = ClientCore::new();
        let shared = SharedState::new();
        core.subscribe_pnl(21);
        for con_id in 1..40 {
            shared.portfolio.set_position_info(PositionInfo {
                con_id, position_fixed: crate::types::QTY_SCALE / 3,
                avg_cost: (100.1 * PRICE_SCALE_F) as i64, ..Default::default()
            });
            shared.portfolio.set_position_marks(con_id, (100.7 * PRICE_SCALE_F) as i64, 0, 0, 0);
            shared.portfolio.add_realized_since_seed(con_id, 0.1);
        }
        assert_eq!(core.poll_pnl(&shared).len(), 1);
        for _ in 0..50 {
            assert!(core.poll_pnl(&shared).is_empty());
        }
    }

    #[test]
    fn pnl_single_without_market_data_sends_the_position_with_daily_unset() {
        // It used to skip a position held at midnight when there was no
        // quote: no callback at all.
        let core = ClientCore::new();
        let shared = SharedState::new();
        shared.portfolio.set_position_info(PositionInfo {
            con_id: 756733, position_fixed: 31 * crate::types::QTY_SCALE,
            avg_cost: (764.59 * PRICE_SCALE_F) as i64, ..Default::default()
        });
        shared.portfolio.set_position_marks(756733, (770.0 * PRICE_SCALE_F) as i64, 0, 0, 0);
        shared.portfolio.set_midnight_seeds(vec![MidnightSeed {
            con_id: 756733, qty_midnight_fixed: 31 * crate::types::QTY_SCALE, money_traded: 0.0, realized_pnl: 37.12,
        }]);
        core.subscribe_pnl_single(4, 756733);
        let u = core.poll_pnl_single(&shared).pop().expect("a callback");
        assert_eq!(u.daily_pnl, f64::MAX);
        assert!((u.unrealized_pnl - 31.0 * (770.0 - 764.59)).abs() < 1e-3);
        assert!((u.realized_pnl - 37.12).abs() < 1e-9);
        assert!((u.value - 31.0 * 770.0).abs() < 1e-6);
    }

    #[test]
    fn poll_pnl_falls_back_to_account_level_without_market_data() {
        // #239: a req_pnl-only client never subscribes to market data, so no
        // position has a live quote (con_id_to_instrument is empty and every
        // position hits `continue`). poll_pnl must then emit the gateway's
        // account-level P&L instead of returning None forever.
        let core = ClientCore::new();
        let shared = SharedState::new();
        core.subscribe_pnl(21);

        // Open position, but NO instrument mapping and NO quote pushed.
        shared.portfolio.set_position_info(PositionInfo {
            con_id: 756733,
            position_fixed: (10) as i64 * crate::types::QTY_SCALE,
            avg_cost: (700.00 * PRICE_SCALE_F) as i64,
            symbol: "SPY".into(),
            sec_type: "STK".into(),
            currency: "USD".into(),
            multiplier: String::new(),
            ..Default::default()
        });

        // Server-sent account-level P&L keys (the account values as sent,
        // ibx#475); only used when the server sends them.
        shared.portfolio.update_account_rows(|store| {
            store.set("DailyPnL", "BASE", "12.50");
            store.set("UnrealizedPnL", "BASE", "35.00");
            store.set("RealizedPnL", "BASE", "4.00");
        });

        let update = core.poll_pnl(&shared).pop().expect("callback must fire from account-level P&L");
        assert_eq!(update.req_id, 21);
        assert!((update.daily_pnl - 12.50).abs() < 1e-6, "daily={}", update.daily_pnl);
        assert!((update.unrealized_pnl - 35.00).abs() < 1e-6, "unreal={}", update.unrealized_pnl);
        assert!((update.realized_pnl - 4.00).abs() < 1e-6, "real={}", update.realized_pnl);
    }

    #[test]
    fn poll_pnl_prefers_quotes_over_account_level_when_priced() {
        // When market data IS subscribed, the per-position quote synthesis wins;
        // the account-level fallback must not override it.
        let core = ClientCore::new();
        let shared = SharedState::new();
        core.subscribe_pnl(22);

        // Priced position: 1 share, avg 100, last 101 → daily/unrealized = 1.00.
        seed_pnl_position(&core, &shared, 1, 0, 1, 100.0, 101.0, 0.0);

        // Divergent account-level values that must be ignored while priced.
        let mut acct = AccountState::default();
        acct.daily_pnl = (999.0 * PRICE_SCALE_F) as i64;
        acct.unrealized_pnl = (999.0 * PRICE_SCALE_F) as i64;
        shared.portfolio.set_account(&acct);

        let update = core.poll_pnl(&shared).pop().expect("callback must fire");
        assert!((update.daily_pnl - 1.0).abs() < 1e-6, "daily={}", update.daily_pnl);
        assert!((update.unrealized_pnl - 1.0).abs() < 1e-6, "unreal={}", update.unrealized_pnl);
    }

    // ── poll_pnl_single regression tests (#168) ──

    #[test]
    fn poll_pnl_single_routes_quote_by_con_id() {
        // #168 (bug 3): two subscribed instruments, different prices — each req_id
        // must see the price of its own con_id, not the first non-zero quote.
        let core = ClientCore::new();
        let shared = SharedState::new();

        seed_pnl_position(&core, &shared, 111, 0, 1, 100.0, 105.0, 0.0);  // SPY
        seed_pnl_position(&core, &shared, 222, 1, 1, 200.0, 210.0, 0.0);  // QQQ

        core.subscribe_pnl_single(50, 111);
        core.subscribe_pnl_single(51, 222);

        let updates = core.poll_pnl_single(&shared);
        assert_eq!(updates.len(), 2);

        let spy = updates.iter().find(|u| u.req_id == 50).expect("SPY update");
        let qqq = updates.iter().find(|u| u.req_id == 51).expect("QQQ update");
        // Unrealized = qty × (last - avg_cost). SPY: 1×(105-100)=5; QQQ: 1×(210-200)=10.
        assert!((spy.unrealized_pnl - 5.0).abs() < 1e-6);
        assert!((qqq.unrealized_pnl - 10.0).abs() < 1e-6);
        // Value = qty × last. SPY: 105; QQQ: 210.
        assert!((spy.value - 105.0).abs() < 1e-6);
        assert!((qqq.value - 210.0).abs() < 1e-6);
    }

    #[test]
    fn poll_pnl_single_intraday_opened_position() {
        // #168 (bug 1): daily_pnl must be computed, not hardcoded 0.
        // No seed → money_traded synthesized, daily collapses to unrealized.
        let core = ClientCore::new();
        let shared = SharedState::new();
        seed_pnl_position(&core, &shared, 756733, 0, 1, 735.00, 735.07, 0.0);
        core.subscribe_pnl_single(42, 756733);

        let updates = core.poll_pnl_single(&shared);
        assert_eq!(updates.len(), 1);
        let u = &updates[0];
        assert_eq!(u.req_id, 42);
        assert!((u.daily_pnl - 0.07).abs() < 1e-6, "daily={}", u.daily_pnl);
        assert!((u.unrealized_pnl - 0.07).abs() < 1e-6);
        // No realized P&L on this position today: unset, as the reference
        // (captured 25/09/2026: realized 1.7976931348623157e+308; ibx#478).
        assert_eq!(u.realized_pnl, f64::MAX);
    }

    // ibx#478: the realized P&L of this session's fills adds to the seed's;
    // a new seed includes them.
    #[test]
    fn poll_pnl_single_adds_the_realized_pnl_of_fills() {
        let core = ClientCore::new();
        let shared = SharedState::new();
        seed_pnl_position(&core, &shared, 756733, 0, 1, 735.00, 735.07, 0.0);
        core.subscribe_pnl_single(42, 756733);
        shared.portfolio.add_realized_since_seed(756733, 5.645252);
        let u = core.poll_pnl_single(&shared).pop().unwrap();
        assert!((u.realized_pnl - 5.645252).abs() < 1e-9);
        shared.portfolio.set_midnight_seeds(vec![MidnightSeed {
            con_id: 756733, qty_midnight_fixed: 0, money_traded: 0.0, realized_pnl: 5.645252,
        }]);
        let _ = core.poll_pnl_single(&shared);
        assert!(shared.portfolio.realized_since_seed().is_empty(), "the new seed includes the fill");
    }

    // The paper case of 25/09/2026: 31 SPY held overnight, then BUY 1 at the
    // last price. The daily P&L must not move by the trade value (it jumped
    // by about 770 when the fill's cash was left out).
    #[test]
    fn a_fill_does_not_move_the_daily_pnl_by_its_value() {
        let core = ClientCore::new();
        let shared = SharedState::new();
        seed_pnl_position(&core, &shared, 756733, 0, 31, 764.59, 770.00, 760.00);
        shared.portfolio.set_midnight_seeds(vec![MidnightSeed {
            con_id: 756733, qty_midnight_fixed: 31 * crate::types::QTY_SCALE, money_traded: 0.0, realized_pnl: 0.0,
        }]);
        core.subscribe_pnl_single(4, 756733);
        let before = core.poll_pnl_single(&shared).pop().unwrap().daily_pnl;
        assert!((before - 310.0).abs() < 1e-6, "31 × (770 - 760): {before}");

        seed_pnl_position(&core, &shared, 756733, 0, 32, 764.78, 770.00, 760.00);
        shared.portfolio.add_money_since_seed(756733, -770.0);
        let after = core.poll_pnl_single(&shared).pop().unwrap().daily_pnl;
        assert!((after - 310.0).abs() < 1e-6, "bought at the last price: no change, got {after}");
    }

    // No seed row, fills of this session only: the daily P&L is exact from
    // their cash (buy 1 at 100, sell 1 at 103: +3).
    #[test]
    fn daily_pnl_of_a_position_traded_this_session_only() {
        let core = ClientCore::new();
        let shared = SharedState::new();
        seed_pnl_position(&core, &shared, 1, 0, 0, 0.0, 103.0, 0.0);
        shared.portfolio.add_money_since_seed(1, -100.0);
        shared.portfolio.add_money_since_seed(1, 103.0);
        core.subscribe_pnl_single(4, 1);
        let u = core.poll_pnl_single(&shared).pop().unwrap();
        assert!((u.daily_pnl - 3.0).abs() < 1e-6, "daily={}", u.daily_pnl);
    }

    #[test]
    fn poll_pnl_single_unrealized_is_unset_without_avg_cost() {
        let core = ClientCore::new();
        let shared = SharedState::new();
        seed_pnl_position(&core, &shared, 756733, 0, 1, 0.0, 735.07, 0.0);
        core.subscribe_pnl_single(42, 756733);
        let u = core.poll_pnl_single(&shared).pop().unwrap();
        assert_eq!(u.unrealized_pnl, f64::MAX);
    }

    // ibx#478: several req_pnl run together; each gets the values.
    #[test]
    fn several_pnl_requests_each_get_the_values() {
        let core = ClientCore::new();
        let shared = SharedState::new();
        seed_pnl_position(&core, &shared, 1, 0, 1, 100.0, 101.0, 0.0);
        core.request_pnl(1, "DU1", "DU1").unwrap();
        core.request_pnl(2, "DU1", "DU1").unwrap();
        let ids: Vec<i64> = core.poll_pnl(&shared).iter().map(|u| u.req_id).collect();
        assert_eq!(ids, [1, 2]);
        assert!(core.poll_pnl(&shared).is_empty(), "unchanged: nothing");
        core.request_pnl(3, "DU1", "DU1").unwrap();
        let ids: Vec<i64> = core.poll_pnl(&shared).iter().map(|u| u.req_id).collect();
        assert_eq!(ids, [3], "a new request gets the current values");
    }

    #[test]
    fn pnl_request_checks() {
        let core = ClientCore::new();
        assert_eq!(core.request_pnl(1, "", "DU1").unwrap_err(),
            (321, "Error validating request.-'cj' : cause - Account must not be empty".to_string()));
        assert_eq!(core.request_pnl(1, "DU2", "DU1").unwrap_err(),
            (321, "Error validating request.-'cj' : cause - Invalid account code".to_string()));
        core.request_pnl(1, "DU1", "DU1").unwrap();
        assert_eq!(core.request_pnl(1, "DU1", "DU1").unwrap_err().0, 102);
        assert_eq!(core.request_pnl_single(5, "", "DU1", 1).unwrap_err().1,
            "Error validating request.-'ck' : cause - Account must not be empty");
        assert_eq!(core.cancel_pnl_request(99), Some((10185, "Failed to cancel PNL (not subscribed)".to_string())));
        assert_eq!(core.cancel_pnl_request(1), None);
        assert_eq!(core.cancel_pnl_single_request(98), Some((10186, "Failed to cancel PNL single (not subscribed)".to_string())));
    }

    #[test]
    fn poll_pnl_single_overnight_position_with_seed() {
        // #168 (bug 2): realized_pnl must come from the seed, not hardcoded 0.
        let core = ClientCore::new();
        let shared = SharedState::new();
        seed_pnl_position(&core, &shared, 756733, 0, 10, 700.00, 735.00, 730.00);
        shared.portfolio.set_midnight_seeds(vec![MidnightSeed {
            con_id: 756733,
            qty_midnight_fixed: (10) as i64 * crate::types::QTY_SCALE,
            money_traded: 0.0,
            realized_pnl: 12.34,
        }]);
        core.subscribe_pnl_single(99, 756733);

        let updates = core.poll_pnl_single(&shared);
        assert_eq!(updates.len(), 1);
        let u = &updates[0];
        // daily = 10×735 − 10×730 − 0 = 50
        assert!((u.daily_pnl - 50.0).abs() < 1e-6);
        // unrealized = 10 × (735 − 700) = 350
        assert!((u.unrealized_pnl - 350.0).abs() < 1e-6);
        assert!((u.realized_pnl - 12.34).abs() < 1e-6);
    }

    #[test]
    fn poll_pnl_single_change_detection_suppresses_duplicate() {
        let core = ClientCore::new();
        let shared = SharedState::new();
        seed_pnl_position(&core, &shared, 1, 0, 1, 100.0, 101.0, 0.0);
        core.subscribe_pnl_single(7, 1);
        assert_eq!(core.poll_pnl_single(&shared).len(), 1);
        // Same inputs → no emit.
        assert!(core.poll_pnl_single(&shared).is_empty());
    }

    #[test]
    fn poll_pnl_single_unsubscribe_clears_cache() {
        let core = ClientCore::new();
        let shared = SharedState::new();
        seed_pnl_position(&core, &shared, 1, 0, 1, 100.0, 101.0, 0.0);
        core.subscribe_pnl_single(7, 1);
        let _ = core.poll_pnl_single(&shared);
        core.unsubscribe_pnl_single(7);
        // Re-subscribing with same req_id must re-emit (cache cleared on unsubscribe).
        core.subscribe_pnl_single(7, 1);
        assert_eq!(core.poll_pnl_single(&shared).len(), 1);
    }

    // ibx#466: an order with an orderRef goes to the extended encoder,
    // which sends 6010; the currency reaches the engine once per contract.
    #[test]
    fn order_ref_uses_the_extended_encoder_and_currency_is_sent_once() {
        let order = ApiOrder { order_ref: "t1".into(), ..lmt(100.0) };
        assert!(order.has_extended_attrs());
        assert_eq!(order.attrs().order_ref, "t1");

        let core = ClientCore::new();
        let (tx, rx) = crossbeam_channel::unbounded();
        let tx = crate::engine::park::ControlSender::from(tx);
        core.note_currency(&tx, 1, "EUR");
        core.note_currency(&tx, 1, "EUR");
        core.note_currency(&tx, 1, "");
        let sent: Vec<ControlCommand> = rx.try_iter().collect();
        assert_eq!(sent.len(), 1);
        assert!(matches!(&sent[0], ControlCommand::SetInstrumentCurrency { con_id: 1, currency } if currency == "EUR"));
    }

    // ibx#416, captured 02/10/2026 (gateway machine in Europe/Paris): the
    // UTC form goes out unchanged, a zone is converted to UTC, no zone is
    // the machine's zone; text it cannot read is None (10314).
    #[test]
    fn condition_time_forms_as_the_reference_reads_them() {
        let read = |s: &str| parse_condition_time(s, "Europe/Paris");
        assert_eq!(read("20991231-23:59:59"),
            Some(ConditionTime { wire: "20991231-23:59:59".into(), zone: "UTC".into(), implied_zone: false }));
        assert_eq!(read("20991231 23:59:59 US/Eastern"),
            Some(ConditionTime { wire: "21000101-04:59:59".into(), zone: "US/Eastern".into(), implied_zone: false }));
        assert_eq!(read("20991231 23:59:59"),
            Some(ConditionTime { wire: "20991231-22:59:59".into(), zone: "Europe/Paris".into(), implied_zone: true }));
        assert_eq!(read("20991231 23:59:59 Asia/Tokyo").map(|t| t.wire), Some("20991231-14:59:59".into()));
        assert_eq!(read("20261005 09:05:41 UTC").map(|t| (t.wire, t.zone)), Some(("20261005-09:05:41".into(), "UTC".into())));
        // A day past the end of its month runs into the next month.
        assert_eq!(read("20260231 12:00:00 UTC").map(|t| t.wire), Some("20260303-12:00:00".into()));
        // A time alone is today.
        assert!(read("10:00:00").is_some_and(|t| t.implied_zone && t.wire.len() == 17));
        for bad in ["tomorrow", "20991231 23:59:59 Nowhere/Zone", "20991231-24:00:00", "20991231 23:59", "2099-12-31 23:59:59", " 20991231-23:59:59x"] {
            assert_eq!(read(bad), None, "{bad}");
        }
    }

    // ibx#416, captured 02/10/2026: the reference's local answers on a
    // time condition: 2174 for a time with no zone (the order is sent),
    // 10314 for text it cannot read or a zone other than UTC, the
    // machine's and the contract's (nothing is sent).
    #[test]
    fn time_condition_warnings_and_refusals() {
        let with_time = |t: &str| ApiOrder {
            conditions: vec![OrderCondition::Time { time: t.into(), is_more: true }], tif: "GTC".into(), ..lmt(1.0)
        };
        let invalid = INVALID_DATE_TIME.replace("%s", "Time");
        assert!(invalid.starts_with("Time: The date, time, or time-zone entered is invalid.\nThe correct format"));
        assert_eq!(ClientCore::implied_zone_warnings(&with_time("20991231 23:59:59")),
            [(2174, IMPLIED_TIME_ZONE.to_string())]);
        assert!(ClientCore::implied_zone_warnings(&with_time("20991231-23:59:59")).is_empty());
        assert!(ClientCore::implied_zone_warnings(&with_time("20991231 23:59:59 US/Eastern")).is_empty());
        assert!(ClientCore::implied_zone_warnings(&with_time("tomorrow")).is_empty());
        assert_eq!(ClientCore::refusal_before_sending(&with_time("tomorrow"), "SMART"), Some((10314, invalid.clone())));
        assert_eq!(ClientCore::refusal_before_sending(&with_time("20991231-23:59:59"), "SMART"), None);

        let zone = |t: &str, contract: Option<&str>| ClientCore::condition_time_zone_refusal(&with_time(t), contract);
        let machine = crate::gateway::machine_time_zone();
        let other = if machine == "Asia/Tokyo" { "Asia/Seoul" } else { "Asia/Tokyo" };
        assert_eq!(zone(&format!("20991231 23:59:59 {other}"), Some("US/Eastern")), Some((10314, invalid)));
        assert_eq!(zone(&format!("20991231 23:59:59 {other}"), None), None, "the contract's zone is not known");
        assert_eq!(zone("20991231 23:59:59 US/Eastern", Some("US/Eastern")), None);
        assert_eq!(zone("20991231 23:59:59 UTC", Some("US/Eastern")), None);
        assert_eq!(zone(&format!("20991231 23:59:59 {machine}"), Some("US/Eastern")), None);
        assert_eq!(zone("20991231 23:59:59", Some("US/Eastern")), None);
        assert_eq!(zone("20991231-23:59:59", Some("US/Eastern")), None);
    }

    // ibx#416: the condition goes out with the time in UTC; the order is
    // otherwise unchanged.
    #[test]
    fn condition_times_are_sent_in_utc() {
        let order = ApiOrder {
            conditions: vec![
                OrderCondition::Time { time: "20991231 23:59:59 US/Eastern".into(), is_more: true },
                OrderCondition::Time { time: "20991231-23:59:59".into(), is_more: false },
                OrderCondition::Margin { percent: 10, is_more: true },
            ],
            ..lmt(1.0)
        };
        let sent = ClientCore::with_condition_times(&order);
        let times: Vec<(String, bool)> = sent.conditions.iter().filter_map(|c| match c {
            OrderCondition::Time { time, is_more } => Some((time.clone(), *is_more)),
            _ => None,
        }).collect();
        assert_eq!(times, [("21000101-04:59:59".to_string(), true), ("20991231-23:59:59".to_string(), false)]);
        assert!(matches!(sent.conditions[2], OrderCondition::Margin { percent: 10, is_more: true }));
        assert!(matches!(ClientCore::with_condition_times(&lmt(1.0)), std::borrow::Cow::Borrowed(_)));
    }

    // ibx#263, captured 02/10/2026: a NaN limit price is refused with 110.
    #[test]
    fn nan_limit_price_is_110() {
        assert_eq!(ClientCore::price_refusal(&lmt(f64::NAN)),
            Some((110, "The price does not conform to the minimum price variation for this contract.".into())));
        assert_eq!(ClientCore::price_refusal(&lmt(100.0)), None);
    }

    // ibx#462, captured 02/10/2026 (b1_462_whatif): the order state of a
    // what-if, with the reference's number texts and unset values.
    #[test]
    fn what_if_order_state_as_the_reference() {
        let s = WhatIfState {
            status: "PreSubmitted".into(),
            init_margin_before: Some(4943.4), maint_margin_before: Some(4125.35), equity_with_loan_before: Some(954397.0),
            init_margin_after: Some(12855.55), maint_margin_after: Some(11566.05), equity_with_loan_after: Some(954397.0),
            commission: Some(1.0003), commission_currency: "USD".into(), margin_currency: "USD".into(),
            ..Default::default()
        };
        let st = ClientCore::what_if_order_state(&s);
        assert_eq!((st.init_margin_before.as_str(), st.maint_margin_before.as_str(), st.equity_with_loan_before.as_str()),
            ("4943.4", "4125.35", "954397.0"));
        assert_eq!((st.init_margin_change.as_str(), st.maint_margin_change.as_str(), st.equity_with_loan_change.as_str()),
            ("7912.15", "7440.699999999999", "0.0"));
        assert_eq!((st.init_margin_after.as_str(), st.maint_margin_after.as_str(), st.equity_with_loan_after.as_str()),
            ("12855.55", "11566.05", "954397.0"));
        assert_eq!((st.commission_and_fees, st.min_commission_and_fees, st.max_commission_and_fees), (1.0003, f64::MAX, f64::MAX));
        assert_eq!((st.commission_and_fees_currency.as_str(), st.margin_currency.as_str()), ("USD", "USD"));
        assert_eq!(st.init_margin_after_outside_rth, f64::MAX);
        // Nothing came: empty texts, unset commission.
        let none = ClientCore::what_if_order_state(&WhatIfState { status: "PreSubmitted".into(), warning_text: "Warning".into(), ..Default::default() });
        assert_eq!((none.init_margin_change.as_str(), none.commission_and_fees, none.warning_text.as_str()), ("", f64::MAX, "Warning"));
    }

    // ibx#335: the zone of a goodTillDate is the machine's zone, UTC or
    // the contract's zone; before the contract's zone is known, any zone
    // that resolves is taken.
    #[test]
    fn good_till_date_zone_rule() {
        let gtd = |s: &str| ApiOrder { tif: "GTD".into(), good_till_date: s.into(), ..lmt(100.0) };
        let refused = |s: &str, zone: Option<&str>| ClientCore::good_till_date_refusal(&gtd(s), zone).map(|(code, _)| code);
        assert_eq!(refused("", Some("US/Eastern")), None);
        assert_eq!(refused("20260930", Some("US/Eastern")), None, "a date has no zone");
        assert_eq!(refused("20260930 16:00:00 US/Central", Some("US/Central")), None);
        assert_eq!(refused("20260930 16:00:00 America/Chicago", None), None);
        assert_eq!(refused("20260930 16:00:00 Nowhere/Zone", None), Some(343));
        let machine = crate::gateway::machine_time_zone();
        assert_eq!(refused(&format!("20260930 16:00:00 {machine}"), Some("US/Eastern")), None);
    }

    // ibx#271: the pre-checks the Python client makes before it releases
    // the interpreter lock agree with what the calls would do.
    #[test]
    fn currency_noted_and_pnl_quotes_idle() {
        let core = ClientCore::new();
        let (tx, _rx) = crossbeam_channel::unbounded();
        let tx = crate::engine::park::ControlSender::from(tx);
        assert!(core.currency_noted(1, ""));
        assert!(core.currency_noted(0, "EUR"));
        assert!(!core.currency_noted(1, "EUR"));
        core.note_currency(&tx, 1, "EUR");
        assert!(core.currency_noted(1, "EUR"));
        assert!(!core.currency_noted(1, "USD"));

        assert!(core.pnl_quotes_idle());
        core.subscribe_pnl(5);
        assert!(!core.pnl_quotes_idle());
        core.unsubscribe_pnl(5);
        core.subscribe_pnl_single(6, 1);
        assert!(!core.pnl_quotes_idle());
        core.unsubscribe_pnl_single(6);
        assert!(core.pnl_quotes_idle());
    }

    // ibx#263: the reference's number rules, refused before sending: the
    // reading errors as 320 (only the last one), the order rules as 321.
    #[test]
    fn number_rules_of_the_reference_are_refused() {
        let rule = |cause: &str| Some((321, format!("Error validating request.-'bH' : cause - {}", cause)));
        let read = |cause: &str| Some((320, format!("Error reading request:{}", cause)));
        for qty in [-1.0, 1_000_000_000.0, f64::INFINITY] {
            assert_eq!(ClientCore::refusal_before_sending(&ApiOrder { total_quantity: qty, ..lmt(100.0) }, "SMART"),
                rule("Order size does not conform to market rule."), "{qty}");
        }
        assert_eq!(ClientCore::refusal_before_sending(&ApiOrder { total_quantity: 999_999_999.0, ..lmt(100.0) }, "SMART"), None);
        let trail = |pct: f64| ApiOrder { order_type: "TRAIL".into(), trailing_percent: pct, ..lmt(0.0) };
        for pct in [-0.5, 100.5, f64::INFINITY] {
            assert_eq!(ClientCore::refusal_before_sending(&trail(pct), "SMART"),
                rule("Invalid Trailing Percent value. Valid values are greater than 0 and less than 100."), "{pct}");
        }
        assert_eq!(ClientCore::refusal_before_sending(&trail(1.5), "SMART"), None);
        let both = ApiOrder { aux_price: 0.5, ..trail(1.5) };
        assert_eq!(ClientCore::refusal_before_sending(&both, "SMART"),
            read("Cannot specify Trailing Amount and Trailing Percent at the same time"));
        let disc = ApiOrder { discretionary_amt: -0.1, ..lmt(100.0) };
        assert_eq!(ClientCore::refusal_before_sending(&disc, "SMART"),
            read("Discretionary amount does not conform to the minimum price variation for this contract "));
        // Only the last reading error is given.
        let two = ApiOrder { discretionary_amt: -0.1, ..both };
        assert_eq!(ClientCore::refusal_before_sending(&two, "SMART").unwrap().1,
            "Error reading request:Cannot specify Trailing Amount and Trailing Percent at the same time");
    }

    // ibx#485: the stop and trigger price rules of the reference, in its
    // order: an unset stop price, then a stop or trigger price that is not
    // a number; a set price passes.
    #[test]
    fn stop_and_trigger_prices_follow_the_reference() {
        let rule = |cause: &str| Some((321, format!("Error validating request.-'bH' : cause - {}", cause)));
        let typed = |t: &str, aux: f64| ApiOrder { order_type: t.into(), aux_price: aux, ..lmt(100.0) };
        for t in ["STP", "STP LMT", "STP PRT"] {
            assert_eq!(ClientCore::refusal_before_sending(&typed(t, f64::MAX), "SMART"), rule("Please enter a stop price"), "{t}");
            assert_eq!(ClientCore::refusal_before_sending(&typed(t, f64::NAN), "SMART"), rule("Invalid Stop Price"), "{t}");
            assert_eq!(ClientCore::refusal_before_sending(&typed(t, 90.0), "SMART"), None, "{t}");
        }
        for t in ["MIT", "LIT"] {
            assert_eq!(ClientCore::refusal_before_sending(&typed(t, f64::MAX), "SMART"), rule("Invalid Trigger Price"), "{t}");
            assert_eq!(ClientCore::refusal_before_sending(&typed(t, f64::INFINITY), "SMART"), rule("Invalid Trigger Price"), "{t}");
            assert_eq!(ClientCore::refusal_before_sending(&typed(t, 90.0), "SMART"), None, "{t}");
        }
        // The stop price comes before the trigger method.
        let both = ApiOrder { trigger_method: 5, ..typed("STP", f64::MAX) };
        assert_eq!(ClientCore::refusal_before_sending(&both, "SMART"), rule("Please enter a stop price"));
    }

    // An order whose contract has no exchange (empty, the official API's
    // default): 321 "Missing order exchange", before the other order rules
    // (`jextend.bH.S()@437-455`), nothing sent.
    #[test]
    fn an_order_with_no_exchange_is_refused() {
        let text = "Error validating request.-'bH' : cause - Missing order exchange".to_string();
        assert_eq!(ClientCore::refusal_before_sending(&lmt(100.0), ""), Some((321, text.clone())));
        let unknown_side = ApiOrder { action: "X".into(), ..lmt(100.0) };
        assert_eq!(ClientCore::refusal_before_sending(&unknown_side, ""), Some((321, text)));
        assert_eq!(ClientCore::refusal_before_sending(&lmt(100.0), "ISLAND"), None);
    }

    // ibx#468: two local refusals of the reference.
    #[test]
    fn midprice_outside_rth_and_trail_limit_fields_are_refused() {
        let midprice = ApiOrder { order_type: "MIDPRICE".into(), outside_rth: true, ..lmt(100.0) };
        let (code, text) = ClientCore::refusal_before_sending(&midprice, "SMART").expect("refused");
        assert_eq!(code, 321);
        assert!(text.ends_with("Midprice orders are not supported outside of regular trading hours."), "{text}");
        let rth = ApiOrder { order_type: "MIDPRICE".into(), outside_rth: false, ..lmt(100.0) };
        assert!(ClientCore::refusal_before_sending(&rth, "SMART").is_none());

        let trail = |lmt_price: f64, offset: f64| ApiOrder {
            order_type: "TRAIL LIMIT".into(), aux_price: 1.0, trail_stop_price: 150.0,
            lmt_price, lmt_price_offset: offset, ..lmt(0.0)
        };
        let both = ClientCore::refusal_before_sending(&trail(100.0, 0.30), "SMART").expect("both: refused");
        assert_eq!(both.0, 321);
        assert!(both.1.ends_with("You must specify one value: limit price or limit price offset value."));
        assert!(ClientCore::refusal_before_sending(&trail(0.0, f64::MAX), "SMART").is_some(), "neither: refused");
        assert!(ClientCore::refusal_before_sending(&trail(0.0, 0.30), "SMART").is_none(), "offset only");
        assert!(ClientCore::refusal_before_sending(&trail(100.0, f64::MAX), "SMART").is_none(), "limit price only");
    }

    fn lmt(price: f64) -> ApiOrder {
        ApiOrder { action: "BUY".into(), order_type: "LMT".into(), total_quantity: 100.0, lmt_price: price, ..Default::default() }
    }

    // ibx#263: the replace of an algo order keeps its algo, which the
    // reference restates; a REL keeps its price cap.
    #[test]
    fn modify_keeps_the_algo_and_the_rel_price_cap() {
        use crate::types::{AdaptivePriority, OrderAlgo, OrderKind};
        let adaptive = ApiOrder { algo_strategy: "Adaptive".into(),
            algo_params: vec![crate::api::types::TagValue { tag: "adaptivePriority".into(), value: "Urgent".into() }], ..lmt(100.0) };
        let Ok(ModifyPlan::Send(ControlCommand::Order(OrderRequest::Modify { attrs, .. }))) =
            ClientCore::build_modify_request(&adaptive, 5, &adaptive) else { panic!("not a modify") };
        assert!(matches!(attrs.algo, Some(OrderAlgo::Adaptive(AdaptivePriority::Urgent))), "{:?}", attrs.algo);
        let rel = ApiOrder { order_type: "REL".into(), aux_price: 0.01, ..lmt(250.0) };
        let Ok(ModifyPlan::Send(ControlCommand::Order(OrderRequest::Modify { kind, .. }))) =
            ClientCore::build_modify_request(&rel, 6, &rel) else { panic!("not a modify") };
        assert!(matches!(kind, OrderKind::Rel { price, offset } if price == 250 * crate::types::PRICE_SCALE && offset == crate::types::PRICE_SCALE / 100), "{kind:?}");
        let Ok(ControlCommand::Order(OrderRequest::SubmitEx { kind, .. })) = ClientCore::build_order_request(&rel, 7, 0) else {
            panic!("a REL with a price cap takes the extended path");
        };
        assert!(matches!(kind, OrderKind::Rel { price, offset } if price == 250 * crate::types::PRICE_SCALE && offset == crate::types::PRICE_SCALE / 100), "{kind:?}");
    }

    // ibx#463: a filled order was forgotten, so place_order with its id sent
    // a new order. The reference answers 104 and sends nothing (captured
    // 25/09/2026).
    #[test]
    fn place_on_a_filled_order_id_is_refused() {
        let core = ClientCore::new();
        let shared = SharedState::new();
        core.track_order(7, ApiContract::default(), lmt(100.0), 0);
        core.update_order_status(7, "Submitted", 0.0, 100.0);
        assert!(core.refusal_for_order_id(7, &lmt(101.0), &shared).is_none(), "working: a modify");
        core.update_order_fill(7, "Filled", 100.0, 0.0);
        assert_eq!(core.tracked_order_type(7), None);
        assert_eq!(core.refusal_for_order_id(7, &lmt(101.0), &shared),
            Some((104, "Cannot modify a filled order.".to_string())));
    }

    #[test]
    fn place_on_a_cancelled_order_id_is_refused() {
        let core = ClientCore::new();
        core.track_order(8, ApiContract::default(), lmt(100.0), 0);
        core.update_order_status(8, "Cancelled", 0.0, 100.0);
        assert_eq!(core.refusal_for_order_id(8, &lmt(101.0), &SharedState::new()).map(|r| r.0), Some(104));
    }

    #[test]
    fn what_if_on_a_finished_order_id_is_not_refused() {
        let core = ClientCore::new();
        core.track_order(9, ApiContract::default(), lmt(100.0), 0);
        core.update_order_fill(9, "Filled", 100.0, 0.0);
        let what_if = ApiOrder { what_if: true, ..lmt(101.0) };
        assert!(core.refusal_for_order_id(9, &what_if, &SharedState::new()).is_none());
    }

    #[test]
    fn finished_order_ids_survive_a_reset() {
        let core = ClientCore::new();
        core.track_order(10, ApiContract::default(), lmt(100.0), 0);
        core.update_order_fill(10, "Filled", 100.0, 0.0);
        core.reset();
        assert!(core.refusal_for_order_id(10, &lmt(101.0), &SharedState::new()).is_some());
    }

    // ibx#466: the next valid id is the reference's (`jextend.H.c()`): the
    // highest order id the client used + 1, 1 for none; the ids of the
    // server's reports for this client id count (the orders of its earlier
    // sessions), those of other clients do not. reqIds reserves nothing;
    // `take_order_id` reserves the id it gives.
    #[test]
    fn next_valid_id_is_the_highest_used_plus_one() {
        let core = ClientCore::new();
        let shared = SharedState::new();
        core.client_id.store(198, Ordering::Relaxed);
        assert_eq!(core.next_valid_id(&shared), 1, "no order known: the reference's start");
        shared.orders.note_reported_order_id(198, 68);
        shared.orders.note_reported_order_id(7, 500);
        assert_eq!(core.next_valid_id(&shared), 69, "b1_416_time_condition of 02/10/2026: nextValidId 69");
        assert_eq!(core.next_valid_id(&shared), 69, "nothing reserved");
        assert_eq!((core.take_order_id(&shared), core.take_order_id(&shared)), (69, 70));
        assert_eq!(core.next_valid_id(&shared), 69, "the ids handed out are not placed");
        core.track_order(73, ApiContract::default(), lmt(100.0), 0);
        assert_eq!(core.next_valid_id(&shared), 74);
        assert_eq!(core.take_order_id(&shared), 74);
        // An id at or below a reported one is no new id (103), unless the
        // server holds that order.
        let duplicate = Some((103, "Duplicate order id".to_string()));
        let core = ClientCore::new();
        core.client_id.store(198, Ordering::Relaxed);
        assert_eq!(core.refusal_for_order_id(68, &lmt(100.0), &shared), duplicate);
        assert!(core.refusal_for_order_id(69, &lmt(100.0), &shared).is_none());
    }

    // ibx#462, `jextend.bH.W()@222`: a new order id at or below the highest
    // id the client placed is refused with 103, also after a reconnect; a
    // higher one goes on; a what-if counts as placed.
    #[test]
    fn a_new_order_at_or_below_the_highest_id_is_refused() {
        let core = ClientCore::new();
        let shared = SharedState::new();
        let duplicate = Some((103, "Duplicate order id".to_string()));
        assert!(core.refusal_for_order_id(5, &lmt(100.0), &shared).is_none(), "nothing placed yet");
        core.track_order(5, ApiContract::default(), lmt(100.0), 0);
        core.update_order_status(5, "Submitted", 0.0, 100.0);
        assert_eq!(core.refusal_for_order_id(3, &lmt(100.0), &shared), duplicate, "below the highest");
        assert!(core.refusal_for_order_id(6, &lmt(100.0), &shared).is_none(), "above the highest");
        let what_if = ApiOrder { what_if: true, ..lmt(100.0) };
        assert_eq!(core.refusal_for_order_id(4, &what_if, &shared), duplicate, "a what-if too");
        core.track_what_if(9, ApiContract::default(), what_if.clone());
        assert_eq!(core.refusal_for_order_id(8, &lmt(100.0), &shared), duplicate, "a what-if's id counts");
        core.reset();
        assert_eq!(core.refusal_for_order_id(9, &lmt(100.0), &shared), duplicate, "kept after a reconnect");
        assert!(core.refusal_for_order_id(10, &lmt(100.0), &shared).is_none());
    }

    // ibx#462: a what-if with the id of an order the server has not
    // answered yet gets 103 (captured 02/10/2026); once answered, it is a
    // preview. A modify goes on either way. An id below the highest that
    // the server knows (an order of an earlier session) is no new id.
    #[test]
    fn what_if_on_an_order_not_answered_yet_and_known_ids() {
        let core = ClientCore::new();
        let shared = SharedState::new();
        let what_if = ApiOrder { what_if: true, ..lmt(101.0) };
        core.track_order(79, ApiContract::default(), lmt(100.0), 0);
        assert_eq!(core.refusal_for_order_id(79, &what_if, &shared).map(|r| r.0), Some(103));
        assert!(core.refusal_for_order_id(79, &lmt(101.0), &shared).is_none(), "a modify");
        core.update_order_status(79, "Submitted", 0.0, 1.0);
        assert!(core.refusal_for_order_id(79, &what_if, &shared).is_none(), "a preview");
        assert_eq!(core.refusal_for_order_id(60, &lmt(100.0), &shared).map(|r| r.0), Some(103));
        shared.orders.push_order_info(60, crate::bridge::RichOrderInfo {
            contract: ApiContract::default(), order: lmt(100.0),
            order_state: Default::default(), last_exec: Default::default(),
        });
        assert!(core.refusal_for_order_id(60, &lmt(100.0), &shared).is_none());
    }

    // Reports for executions this client never sees are dropped oldest
    // first (ibx#471).
    #[test]
    fn pending_commissions_are_bounded_oldest_first() {
        let mut pending = PendingCommissions::default();
        let report = |id: &str| ApiCommissionAndFeesReport { exec_id: id.into(), ..Default::default() };
        for i in 0..PENDING_COMMISSIONS_MAX + 10 {
            pending.insert(format!("e{i}"), report(&format!("e{i}")));
        }
        assert_eq!(pending.len(), PENDING_COMMISSIONS_MAX);
        assert!(pending.remove("e0").is_none(), "the oldest is dropped");
        assert!(pending.remove(&format!("e{}", PENDING_COMMISSIONS_MAX + 9)).is_some(), "the newest is kept");
    }

    // A key replaced later is not dropped by its first, stale entry.
    #[test]
    fn pending_commission_replaced_keeps_its_new_place() {
        let mut pending = PendingCommissions::default();
        let report = |id: &str| ApiCommissionAndFeesReport { exec_id: id.into(), ..Default::default() };
        pending.insert("a".into(), report("a.01"));
        for i in 0..PENDING_COMMISSIONS_MAX - 1 {
            pending.insert(format!("e{i}"), report("x"));
        }
        pending.insert("a".into(), report("a.02"));
        pending.insert("b".into(), report("b"));
        assert_eq!(pending.remove("a").map(|r| r.exec_id), Some("a.02".to_string()));
    }

    // A modify keeps the status the server last reported: the engine can
    // still refuse the modify (ibx#463).
    #[test]
    fn a_modify_keeps_the_tracked_status() {
        let core = ClientCore::new();
        core.track_order(11, ApiContract::default(), lmt(100.0), 0);
        core.update_order_status(11, "Submitted", 0.0, 100.0);
        core.track_order(11, ApiContract::default(), lmt(101.0), 0);
        let tracked = core.open_orders.lock().unwrap().get(&11).cloned().unwrap();
        assert_eq!(tracked.status, "Submitted");
        assert_eq!(tracked.order.lmt_price, 101.0);
    }

    // ibx#413: a snap order takes its offset from auxPrice, 0 when unset.
    #[test]
    fn snap_orders_take_the_offset_from_aux_price() {
        let snap = |aux_price: f64| ApiOrder { order_type: "SNAP MKT".into(), aux_price, ..lmt(0.0) };
        let offset = |o: &ApiOrder| match ClientCore::build_order_request(o, 1, 0) {
            Ok(ControlCommand::Order(OrderRequest::SubmitSnapMkt { offset, .. })) => offset,
            other => panic!("unexpected {other:?}"),
        };
        assert_eq!(offset(&snap(0.05)), (0.05 * PRICE_SCALE_F) as i64);
        assert_eq!(offset(&snap(0.0)), 0);
        assert_eq!(offset(&snap(f64::MAX)), 0, "unset");
        let prim = ApiOrder { order_type: "SNAP PRIM".into(), aux_price: 0.10, outside_rth: true, ..lmt(0.0) };
        assert!(matches!(ClientCore::order_kind(&prim), Ok(OrderKind::SnapPri { offset }) if offset == (0.10 * PRICE_SCALE_F) as i64));
    }
}

#[cfg(test)]
mod session_tests {
    use super::*;

    // ibx#426: the level the reference answers a current client.
    #[test]
    fn server_version_is_the_reference_level() {
        assert_eq!(SERVER_VERSION, 214);
    }

    // ibx#426: `yyyyMMdd HH:mm:ss {zone}`, local time.
    #[test]
    fn connection_time_in_the_reference_form() {
        let tz = jiff::tz::TimeZone::get("Europe/Paris").unwrap();
        let at = jiff::civil::date(2026, 5, 9).at(21, 3, 19, 0).to_zoned(tz).unwrap();
        assert_eq!(connection_time(&at, "Europe/Paris"), "20260509 21:03:19 Europe/Paris");
        let now = connection_time_now();
        let (date, rest) = now.split_once(' ').unwrap();
        let (time, zone) = rest.split_once(' ').unwrap();
        assert!(date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit()), "{}", now);
        assert!(time.len() == 8 && time.as_bytes()[2] == b':' && time.as_bytes()[5] == b':', "{}", now);
        assert!(!zone.is_empty());
    }
}
