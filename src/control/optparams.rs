//! Option chain parameters (ibx#440): the reference's request checks, its
//! derivative queries on the auth connection and the API rows it builds
//! from the chain answers (`jextend.cp`, `feature.derivatives`).

use std::collections::HashMap;

/// Error text of a query the auth connection could not send.
pub const SEND_FAILED: &str = "Sending message failed";
/// Error text when the derivative answer has no usable row for the conId.
pub const NO_DERIVATIVES_FOUND: &str = "No derivatives found";
/// Error text when the option leg ended without data.
pub const NO_DERIVATIVES_RETURNED: &str = "no derivatives returned";

/// The security type the reference reads from an API string
/// (`jfix.eh.a(String, boolean)`): case-insensitive, `CS` is a stock, an
/// unknown name is the empty type.
pub fn sec_type_name(api: &str) -> &'static str {
    const NAMES: [&str; 24] = [
        "STK", "CFD", "OPT", "FOP", "WAR", "IOPT", "FUT", "FWD", "BAG", "CASH", "IND", "BOND",
        "BILL", "FUND", "FIXED", "SLB", "CMDTY", "BSK", "ICU", "ICS", "CRYPTO", "PDC", "EC", "UNK",
    ];
    if api.is_empty() || api.eq_ignore_ascii_case("NONE") {
        return "";
    }
    if api.eq_ignore_ascii_case("CS") {
        return "STK";
    }
    if api == "*" || api.eq_ignore_ascii_case("ANY") {
        return "*";
    }
    let upper = api.to_ascii_uppercase();
    NAMES.iter().find(|n| **n == upper && **n != "UNK").copied().unwrap_or("")
}

/// The local refusal of a request (`jextend.cp.n()`): 321 when the
/// underlying type is not FUT, STK, IND or CASH, when a FUT has no
/// exchange, or when the conId is not a valid id.
pub fn refusal(sec_type: &str, fut_fop_exchange: &str, con_id: i64) -> Option<(i64, String)> {
    let refuse = |cause: String| Some((321, format!("Error validating request.-'cp' : cause - {}", cause)));
    let name = sec_type_name(sec_type);
    if !matches!(name, "FUT" | "STK" | "IND" | "CASH") {
        return refuse(format!("Invalid security type - {}", name));
    }
    if name == "FUT" && fut_fop_exchange.trim().is_empty() {
        return refuse("Missing exchange for security type FUT".into());
    }
    if con_id <= 0 || con_id >= i32::MAX as i64 {
        return refuse("Invalid contract id".into());
    }
    None
}

/// An error after the checks: 322 with the reference's text
/// (`jextend.cp.a(String)`).
pub fn processing_error(text: &str) -> (i64, String) {
    (322, format!("Error processing request: {}", text))
}

/// The derivative types a leg of the reference can be, in the order the
/// legs run (`DerivativeCacheType.COMPLETE`).
pub const LEG_ORDER: [&str; 6] = ["STK", "FUT", "OPT", "FOP", "CASH", "IND"];

fn fields(msg: &[u8]) -> impl Iterator<Item = (u32, &str)> {
    msg.split(|&b| b == crate::protocol::fix::SOH).filter_map(|part| {
        let text = std::str::from_utf8(part).ok()?;
        let (tag, val) = text.split_once('=')?;
        Some((tag.parse().ok()?, val))
    })
}

/// `;` separated values without the empty ones (`jutils.dO.m`).
fn split_list(value: &str) -> impl Iterator<Item = &str> {
    value.split(';').filter(|v| !v.trim().is_empty())
}

// ─── The derivative answer (6040=5) ───

/// One underlying of a derivative answer.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UnderlyingEntry {
    pub con_id: i64,
    /// The derivative types listed (6070).
    pub derivative_types: Vec<String>,
    /// The exchanges of its futures options (6589).
    pub fop_exchanges: String,
}

/// A derivative answer: the symbol asked and its underlyings.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UnderlyingAnswer {
    pub symbol: String,
    pub entries: Vec<UnderlyingEntry>,
}

impl UnderlyingAnswer {
    /// The answer can serve a later request on `con_id` without a new
    /// query: one of its underlyings is that conId and lists the
    /// underlying's own type (`jclient.mw.a(String, int, eh)`).
    pub fn serves(&self, con_id: i64, sec_type: &str) -> bool {
        self.entries.iter().any(|e| e.con_id == con_id && e.derivative_types.iter().any(|t| t == sec_type))
    }

    /// The legs of a request on `con_id`, in their run order, with the
    /// futures option exchanges of the FOP leg; empty when no underlying
    /// of the answer is that conId or none lists a leg type
    /// (`feature.derivatives.A`).
    pub fn legs(&self, con_id: i64) -> Vec<(&'static str, Vec<String>)> {
        let mut legs: Vec<(&'static str, Vec<String>)> = Vec::new();
        for entry in self.entries.iter().filter(|e| e.con_id == con_id) {
            for leg in LEG_ORDER {
                if entry.derivative_types.iter().any(|t| t == leg) && !legs.iter().any(|(l, _)| *l == leg) {
                    let exchanges = if leg == "FOP" { split_list(&entry.fop_exchanges).map(str::to_string).collect() } else { Vec::new() };
                    legs.push((leg, exchanges));
                }
            }
        }
        legs
    }
}

/// The derivative query of an underlying: symbol in upper case, the conId
/// except for CASH (`jfix.cb`).
pub fn underlying_query(symbol: &str, sec_type: &str, con_id: i64) -> Vec<(u32, String)> {
    let mut out = vec![(6040, "5".to_string()), (55, symbol.to_uppercase()), (310, sec_type.to_string())];
    if sec_type != "CASH" {
        out.push((6457, con_id.to_string()));
    }
    out.push((6320, "1".into()));
    out
}

/// Parse a derivative answer (6040=5): each conId (6457) starts an
/// underlying; its derivative types (6070) and futures option exchanges
/// (6589) follow it.
pub fn parse_underlying_answer(msg: &[u8]) -> Option<UnderlyingAnswer> {
    let mut answer = UnderlyingAnswer::default();
    let mut symbol = None;
    for (tag, val) in fields(msg) {
        match tag {
            55 if symbol.is_none() => symbol = Some(val.to_uppercase()),
            6457 => answer.entries.push(UnderlyingEntry { con_id: val.trim().parse().unwrap_or(0), ..Default::default() }),
            6070 => if let Some(e) = answer.entries.last_mut() {
                e.derivative_types = split_list(val).map(str::to_string).collect();
            },
            6589 => if let Some(e) = answer.entries.last_mut() {
                e.fop_exchanges = val.to_string();
            },
            _ => {}
        }
    }
    answer.symbol = symbol?;
    Some(answer)
}

// ─── The chain query (6040=138) and its answer (6040=139) ───

/// The key the reference matches a chain answer to its query with
/// (`feature.derivatives.T.a(...)`, `jfix.az.k()`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ChainKey {
    pub con_id: i64,
    pub symbol: String,
    pub sec_type: String,
    pub exchange: String,
    /// The conId goes on 6457 (futures options of a non-future).
    pub by_6457: bool,
}

/// The chain query of a key (`jfix.cc`): 6995 only for a FUT type.
pub fn chain_query(key: &ChainKey) -> Vec<(u32, String)> {
    let mut out = vec![
        (6040, "138".to_string()),
        (55, key.symbol.to_uppercase()),
        (310, key.sec_type.clone()),
        (if key.by_6457 { 6457 } else { 6346 }, key.con_id.to_string()),
        (6320, "1".into()),
        (6994, "1".into()),
    ];
    if key.sec_type == "FUT" && !key.exchange.is_empty() {
        out.push((6995, key.exchange.clone()));
    }
    out
}

/// The strike scale of a market rule of a chain answer
/// (`jmarketrules.o`): the price magnifier and the decimals of its first
/// display tier.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StrikeScale {
    pub magnifier: i32,
    pub decimals: i32,
}

/// One trading class of a chain answer (`jfix.Z` with its dates and
/// strikes).
#[derive(Debug, Clone, PartialEq)]
pub struct ChainClass {
    pub con_id: i64,
    /// Exchange of its group (100).
    pub exchange: String,
    pub trading_class: Option<String>,
    pub multiplier: Option<String>,
    /// The extra key 6957.
    pub extra: Option<String>,
    /// Expirations of 6775, 6777 and 6971.
    pub expirations: Vec<String>,
    /// Strikes (6997); None when the class has none.
    pub strikes: Option<Vec<f64>>,
    pub rule_id: Option<String>,
}

/// A chain answer.
#[derive(Debug, Clone, PartialEq)]
pub struct ChainAnswer {
    pub key: ChainKey,
    pub classes: Vec<ChainClass>,
    /// The exchanges of 6523, every group's, in order, once.
    pub exchanges: Vec<String>,
    pub rules: HashMap<String, StrikeScale>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ClassKey {
    con_id: i64,
    exchange: String,
    trading_class: Option<String>,
    extra: Option<String>,
    multiplier: Option<String>,
}

/// Parse a chain answer (6040=139) as the reference reads it
/// (`jfix.az`, `jfix.ay`, `jfix.cc`): a group exchange (100) starts a group
/// and clears the trading class and multiplier; the conId of a class is
/// the last 6346; dates (6775, 6777, 6971) and strikes, currency and rule
/// (6997, 15, 6031) are kept per class, the last value of a tag winning;
/// 6523 lists add to one exchange list; the market rule table follows 6019.
pub fn parse_chain_answer(msg: &[u8]) -> Option<ChainAnswer> {
    let mut symbol: Option<String> = None;
    let mut sec_type: Option<String> = None;
    let mut con_id = 0i64;
    let mut fop_con_id: Option<i64> = None;
    let mut exchange_key = String::new();
    let mut group: Option<String> = None;
    // The class fields of the strike entries (cleared by a group) and of
    // the date entries (kept across groups), as the reference keeps both.
    let (mut tc, mut mult, mut extra) = (None::<String>, None::<String>, None::<String>);
    let (mut tc_dates, mut mult_dates, mut extra_dates) = (None::<String>, None::<String>, None::<String>);
    let mut dates: Vec<(ClassKey, HashMap<u32, Option<String>>)> = Vec::new();
    let mut values: Vec<(ClassKey, HashMap<u32, String>)> = Vec::new();
    let mut exchanges: Vec<String> = Vec::new();
    let mut seen_6040 = false;
    for (tag, val) in fields(msg) {
        if tag == 6019 {
            break;
        }
        match tag {
            6040 => seen_6040 = true,
            55 if symbol.is_none() => symbol = Some(val.to_uppercase()),
            310 if sec_type.is_none() => sec_type = Some(sec_type_name(val).to_string()),
            6346 => con_id = val.trim().parse().unwrap_or(0),
            6995 => exchange_key = val.to_string(),
            6457 => fop_con_id = Some(val.trim().parse().unwrap_or(0)),
            100 => {
                group = Some(val.to_string());
                tc = None;
                mult = None;
                extra = None;
            }
            6058 => {
                tc = Some(val.to_string());
                tc_dates = Some(val.to_string());
            }
            6957 => {
                extra = Some(val.to_string());
                extra_dates = Some(val.to_string());
            }
            231 => {
                mult = Some(val.to_string());
                mult_dates = Some(val.to_string());
            }
            6523 => {
                for e in split_list(val) {
                    if !exchanges.iter().any(|x| x == e) {
                        exchanges.push(e.to_string());
                    }
                }
            }
            6775 | 6777 | 6971 => {
                let Some(g) = group.as_ref().filter(|g| !g.is_empty()) else { continue };
                let key = ClassKey { con_id, exchange: g.clone(), trading_class: tc_dates.clone(), extra: extra_dates.clone(), multiplier: mult_dates.clone() };
                let value = (!val.is_empty()).then(|| val.to_string());
                match dates.iter_mut().find(|(k, _)| *k == key) {
                    Some((_, m)) => { m.insert(tag, value); }
                    None => dates.push((key, HashMap::from([(tag, value)]))),
                }
            }
            6997 | 15 | 6031 => {
                let Some(g) = group.as_ref().filter(|g| !g.is_empty()) else { continue };
                let key = ClassKey { con_id, exchange: g.clone(), trading_class: tc.clone(), extra: extra.clone(), multiplier: mult.clone() };
                let pos = match values.iter().position(|(k, _)| *k == key) {
                    Some(p) => p,
                    None => { values.push((key, HashMap::new())); values.len() - 1 }
                };
                values[pos].1.insert(tag, val.to_string());
                if [6997, 15, 6031].iter().all(|t| values[pos].1.contains_key(t)) {
                    extra = None;
                    extra_dates = None;
                }
            }
            _ => {}
        }
    }
    if !seen_6040 {
        return None;
    }
    let by_6457 = fop_con_id.is_some();
    let key = ChainKey {
        con_id: fop_con_id.unwrap_or(con_id),
        symbol: symbol.unwrap_or_default(),
        sec_type: sec_type.unwrap_or_default(),
        exchange: exchange_key,
        by_6457,
    };
    let classes = dates.into_iter().map(|(k, m)| {
        let mut expirations = Vec::new();
        for tag in [6775, 6777, 6971] {
            if let Some(Some(v)) = m.get(&tag) {
                expirations.extend(split_list(v).map(str::to_string));
            }
        }
        let own = values.iter().find(|(vk, _)| *vk == k).map(|(_, v)| v);
        ChainClass {
            con_id: k.con_id,
            exchange: k.exchange,
            trading_class: k.trading_class,
            multiplier: k.multiplier,
            extra: k.extra,
            expirations,
            strikes: own.and_then(|v| v.get(&6997)).map(|s| split_list(s).filter_map(|x| x.trim().parse().ok()).collect()),
            rule_id: own.and_then(|v| v.get(&6031)).cloned(),
        }
    }).collect();
    Some(ChainAnswer { key, classes, exchanges, rules: parse_strike_scales(msg) })
}

/// The strike scales of the market rule table of a chain answer
/// (`jsecdef.n`): each rule id (6031) starts a rule; the first tier set
/// (6022 entries) gives the magnifier, 10 to the power of 6021, and the
/// decimals, the 6025 of its first entry. A rule without a first tier set
/// gives no scale.
pub fn parse_strike_scales(msg: &[u8]) -> HashMap<String, StrikeScale> {
    let mut out = HashMap::new();
    let mut in_table = false;
    let mut id: Option<String> = None;
    let mut power = 0i32;
    let mut first_tier = true;
    let mut tier_left = 0usize;
    let mut tier_first: Option<i32> = None;
    let mut scale: Option<StrikeScale> = None;
    let mut close = |id: &mut Option<String>, scale: &mut Option<StrikeScale>| {
        if let (Some(i), Some(s)) = (id.take(), scale.take()) {
            out.insert(i, s);
        }
    };
    for (tag, val) in fields(msg) {
        if tag == 6019 {
            in_table = true;
            continue;
        }
        if !in_table {
            continue;
        }
        let n = val.trim().parse::<i32>().unwrap_or(0);
        match tag {
            6031 => {
                close(&mut id, &mut scale);
                id = Some(val.trim().to_string());
                tier_left = 0;
            }
            6021 => power = n,
            6022 => { first_tier = true; tier_left = n.max(0) as usize; tier_first = None; }
            6029 => { first_tier = false; tier_left = n.max(0) as usize; tier_first = None; }
            6026 => first_tier = true,
            6030 => first_tier = false,
            6025 if tier_left > 0 => {
                tier_first.get_or_insert(n);
                tier_left -= 1;
                if tier_left == 0 && first_tier {
                    let magnifier = (10f64.powi(power) as f32 + 0.5).floor() as i32;
                    scale = Some(StrikeScale { magnifier, decimals: tier_first.unwrap_or(0) });
                }
            }
            6020 | 6023 | 6024 | 6025 | 6027 | 6028 => {}
            _ => break,
        }
    }
    close(&mut id, &mut scale);
    out
}

/// A strike as the API gets it (`trader.common.b9.a(o, double, StrikeHint)`):
/// unchanged with the logon feature NOMAGNFIX or without a rule; else
/// times the rule's magnifier, rounded to its decimals (2 when not above
/// 0, none for -3).
pub fn api_strike(raw: f64, scale: Option<&StrikeScale>, no_magnifier_fix: bool) -> f64 {
    let Some(s) = scale.filter(|_| !no_magnifier_fix) else { return raw };
    let v = raw * s.magnifier as f64;
    if s.decimals == -3 {
        return v;
    }
    let decimals = if s.decimals > 0 { s.decimals } else { 2 };
    let p = POWERS.get(decimals as usize).copied().unwrap_or_else(|| 10f64.powi(decimals));
    java_round(v * p) / p
}

const POWERS: [f64; 16] = [1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15];

/// `Math.round(double)`: half up.
fn java_round(x: f64) -> f64 {
    let f = x.floor();
    if x - f >= 0.5 { f + 1.0 } else { f }
}

// ─── API rows ───

/// One option chain row: SECURITY_DEFINITION_OPTION_PARAMETER.
#[derive(Debug, Clone, PartialEq)]
pub struct OptionChain {
    pub exchange: String,
    pub underlying_con_id: i64,
    pub trading_class: String,
    pub multiplier: String,
    /// Sorted, once each.
    pub expirations: Vec<String>,
    /// Sorted, once each.
    pub strikes: Vec<f64>,
}

/// `String.hashCode()`.
fn java_hash(s: &str) -> i32 {
    s.encode_utf16().fold(0i32, |h, c| h.wrapping_mul(31).wrapping_add(c as i32))
}

struct Group {
    chain: OptionChain,
    hash: i32,
    /// The comparator fields of its first row: expiry, strike, extra key,
    /// multiplier as a number.
    first: (String, f64, String),
    multiplier_value: f64,
}

/// The order of the reference's rows (`feature.derivatives.I.a(I)`) for the
/// first row of two groups: expiry, strike, exchange, extra key, trading
/// class, then the multiplier difference cut to an int.
fn first_row_order(a: &Group, b: &Group) -> std::cmp::Ordering {
    a.first.0.cmp(&b.first.0)
        .then_with(|| a.first.1.total_cmp(&b.first.1))
        .then_with(|| a.chain.exchange.cmp(&b.chain.exchange))
        .then_with(|| a.first.2.cmp(&b.first.2))
        .then_with(|| a.chain.trading_class.cmp(&b.chain.trading_class))
        .then_with(|| ((a.multiplier_value - b.multiplier_value) as i32).cmp(&0))
}

/// The iteration order of a `java.util.HashMap` filled with keys of these
/// hashes in this order: by bucket of the final table, then insertion
/// order. The table starts at 16, doubles past 3/4 full, and doubles for a
/// bin of 9 keys while under 64 (a bin made a tree at 64 or more keeps its
/// insertion order here).
fn java_hashmap_order(hashes: &[i32]) -> Vec<usize> {
    let spread = |h: i32| (h ^ ((h as u32) >> 16) as i32) as u32;
    let mut cap: u32 = 16;
    let mut bins: HashMap<u32, usize> = HashMap::new();
    let rebin = |cap: u32, n: usize| {
        let mut b: HashMap<u32, usize> = HashMap::new();
        for h in &hashes[..n] {
            *b.entry(spread(*h) & (cap - 1)).or_default() += 1;
        }
        b
    };
    for (i, h) in hashes.iter().enumerate() {
        let len = { let e = bins.entry(spread(*h) & (cap - 1)).or_default(); *e += 1; *e };
        if len >= 9 && cap < 64 {
            cap *= 2;
            bins = rebin(cap, i + 1);
        }
        if (i + 1) as u32 > cap / 4 * 3 {
            cap *= 2;
            bins = rebin(cap, i + 1);
        }
    }
    let mut order: Vec<usize> = (0..hashes.len()).collect();
    order.sort_by_key(|&i| (spread(hashes[i]) & (cap - 1), i));
    order
}

/// The API rows of a request from the chain answers of its leg, as the
/// reference builds them (`feature.derivatives.D`, `jextend.cp.a(s)`):
/// rows for each class of an allowed group exchange, again for every
/// allowed exchange of 6523 when the group is BEST or SMART (reported as
/// SMART); rows grouped by exchange, conId, trading class and multiplier;
/// sorted expirations and strikes; the groups in the order of the
/// reference's hash map.
/// The excluded exchanges are NASDAQ and IBCX, IBCX only with the logon
/// feature ISLAND2NASDAQ.
pub fn chain_rows(answers: &[ChainAnswer], no_magnifier_fix: bool, island_to_nasdaq: bool) -> Vec<OptionChain> {
    let excluded = |e: &str| e == "IBCX" || (!island_to_nasdaq && e == "NASDAQ");
    let smart = |e: &str| e == "BEST" || e == "SMART";
    let mut groups: Vec<Group> = Vec::new();
    for answer in answers {
        for class in &answer.classes {
            let Some(raw) = class.strikes.as_ref() else { continue };
            if excluded(&class.exchange) || class.expirations.is_empty() || raw.is_empty() {
                continue;
            }
            let mut targets = vec![class.exchange.clone()];
            if smart(&class.exchange) {
                targets.extend(answer.exchanges.iter().filter(|e| !excluded(e)).cloned());
            }
            let scale = class.rule_id.as_ref().and_then(|r| answer.rules.get(r));
            let strikes: Vec<f64> = raw.iter().map(|s| api_strike(*s, scale, no_magnifier_fix)).collect();
            let first_expiry = class.expirations.iter().min().cloned().unwrap_or_default();
            let first_strike = raw.iter().copied().fold(f64::INFINITY, f64::min);
            let trading_class = class.trading_class.clone().unwrap_or_default();
            let multiplier = class.multiplier.clone().unwrap_or_default();
            let extra = class.extra.clone().unwrap_or_default();
            for target in targets {
                let exchange = if smart(&target) { "SMART".to_string() } else { target };
                let hash = class.con_id as i32;
                let hash = hash.wrapping_add(java_hash(&trading_class)).wrapping_add(java_hash(&multiplier)).wrapping_add(java_hash(&exchange));
                let same = |g: &&mut Group| g.hash == hash && g.chain.underlying_con_id == class.con_id
                    && g.chain.exchange.eq_ignore_ascii_case(&exchange)
                    && g.chain.trading_class.eq_ignore_ascii_case(&trading_class)
                    && g.chain.multiplier.eq_ignore_ascii_case(&multiplier);
                let first = (first_expiry.clone(), first_strike, extra.clone());
                match groups.iter_mut().find(same) {
                    Some(g) => {
                        g.chain.expirations.extend(class.expirations.iter().cloned());
                        g.chain.strikes.extend(strikes.iter().copied());
                        if (first.0.as_str(), first.1, first.2.as_str()) < (g.first.0.as_str(), g.first.1, g.first.2.as_str()) {
                            g.first = first;
                        }
                    }
                    None => groups.push(Group {
                        chain: OptionChain {
                            exchange,
                            underlying_con_id: class.con_id,
                            trading_class: trading_class.clone(),
                            multiplier: multiplier.clone(),
                            expirations: class.expirations.clone(),
                            strikes: strikes.clone(),
                        },
                        hash,
                        first,
                        multiplier_value: multiplier.trim().parse().unwrap_or(0.0),
                    }),
                }
            }
        }
    }
    // The groups enter the hash map in the order of their first row.
    let mut inserted: Vec<Group> = Vec::with_capacity(groups.len());
    for g in groups {
        let at = inserted.iter().position(|o| first_row_order(&g, o) == std::cmp::Ordering::Less).unwrap_or(inserted.len());
        inserted.insert(at, g);
    }
    let hashes: Vec<i32> = inserted.iter().map(|g| g.hash).collect();
    let order = java_hashmap_order(&hashes);
    let mut slots: Vec<Option<Group>> = inserted.into_iter().map(Some).collect();
    order.into_iter().filter_map(|i| slots[i].take()).map(|g| {
        let mut chain = g.chain;
        chain.expirations.sort();
        chain.expirations.dedup();
        chain.strikes.sort_by(f64::total_cmp);
        chain.strikes.dedup_by(|a, b| a.total_cmp(b).is_eq());
        chain
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(text: &str) -> Vec<u8> {
        text.replace('|', "\x01").into_bytes()
    }

    #[test]
    fn refusals_as_the_reference() {
        let cause = |c: &str| Some((321, format!("Error validating request.-'cp' : cause - {}", c)));
        assert_eq!(refusal("OPT", "", 265598), cause("Invalid security type - OPT"));
        assert_eq!(refusal("xyz", "", 265598), cause("Invalid security type - "));
        assert_eq!(refusal("", "", 265598), cause("Invalid security type - "));
        assert_eq!(refusal("FUT", "", 495512563), cause("Missing exchange for security type FUT"));
        assert_eq!(refusal("STK", "", 0), cause("Invalid contract id"));
        assert_eq!(refusal("STK", "", 2147483647), cause("Invalid contract id"));
        assert_eq!(refusal("stk", "", 265598), None);
        assert_eq!(refusal("CS", "", 265598), None);
        assert_eq!(refusal("FUT", "CME", 495512563), None);
        assert_eq!(refusal("IND", "", 416904), None);
        assert_eq!(refusal("CASH", "", 12087792), None);
        assert_eq!(processing_error(NO_DERIVATIVES_FOUND).1, "Error processing request: No derivatives found");
    }

    #[test]
    fn queries_as_the_reference() {
        let pipe = |f: Vec<(u32, String)>| f.iter().map(|(t, v)| format!("{t}={v}")).collect::<Vec<_>>().join("|");
        assert_eq!(pipe(underlying_query("aapl", "STK", 265598)), "6040=5|55=AAPL|310=STK|6457=265598|6320=1");
        assert_eq!(pipe(underlying_query("EUR", "CASH", 12087792)), "6040=5|55=EUR|310=CASH|6320=1");
        let key = ChainKey { con_id: 265598, symbol: "AAPL".into(), sec_type: "STK".into(), exchange: String::new(), by_6457: false };
        assert_eq!(pipe(chain_query(&key)), "6040=138|55=AAPL|310=STK|6346=265598|6320=1|6994=1");
        let fut = ChainKey { con_id: 495512563, symbol: "ES".into(), sec_type: "FUT".into(), exchange: "CME".into(), by_6457: false };
        assert_eq!(pipe(chain_query(&fut)), "6040=138|55=ES|310=FUT|6346=495512563|6320=1|6994=1|6995=CME");
        let fop = ChainKey { con_id: 416904, symbol: "SPX".into(), sec_type: "FUT".into(), exchange: "CBOE".into(), by_6457: true };
        assert_eq!(pipe(chain_query(&fop)), "6040=138|55=SPX|310=FUT|6457=416904|6320=1|6994=1|6995=CBOE");
    }

    // Captured 28/09/2026 (AAPL): the underlying lists the stock and option
    // legs, in run order.
    #[test]
    fn underlying_answer_legs() {
        let m = msg("35=U|6040=5|55=AAPL|6455=1|55=AAPL|310=STK|6453=NASDAQ|6455=1|55=AAPL|6457=265598|6070=BAG;CFD;IOPT;OPT;STK;WAR;|6588=AMEX;BEST;|15=USD");
        let a = parse_underlying_answer(&m).unwrap();
        assert_eq!(a.symbol, "AAPL");
        assert_eq!(a.legs(265598), [("STK", vec![]), ("OPT", vec![])]);
        assert!(a.legs(1).is_empty());
        assert!(a.serves(265598, "STK") && !a.serves(265598, "IND") && !a.serves(1, "STK"));
        let f = msg("35=U|6040=5|55=SPX|6457=416904|6070=FOP;IND;OPT;|6589=CBOE;FORECASTX;");
        assert_eq!(parse_underlying_answer(&f).unwrap().legs(416904),
            [("OPT", vec![]), ("FOP", vec!["CBOE".to_string(), "FORECASTX".to_string()]), ("IND", vec![])]);
    }

    #[test]
    fn strike_scales_of_the_rule_table() {
        let m = msg("35=U|6040=139|6019=2|6031=32|6020=0|6021=0|6022=1|6023=0|6024=4|6025=2|6026=1|6023=0|6027=0.01|6028=0|6029=1|6023=0|6024=6|6025=0|6030=1|6023=1|6027=1|6031=7|6020=0|6021=2|6022=1|6023=0|6024=4|6025=-3|6026=1|6023=0|6027=1");
        let s = parse_strike_scales(&m);
        assert_eq!(s["32"], StrikeScale { magnifier: 1, decimals: 2 });
        assert_eq!(s["7"], StrikeScale { magnifier: 100, decimals: -3 });
        assert_eq!(api_strike(297.5, Some(&s["32"]), false), 297.5);
        assert_eq!(api_strike(2.975, Some(&s["7"]), false), 297.5);
        assert_eq!(api_strike(2.975, Some(&s["7"]), true), 2.975);
        assert_eq!(api_strike(1.005, Some(&StrikeScale { magnifier: 1, decimals: 0 }), false), 1.0);
        assert_eq!(api_strike(1.5, None, false), 1.5);
    }

    #[test]
    fn java_hashes() {
        assert_eq!(java_hash(""), 0);
        assert_eq!(java_hash("AAPL"), 2001436);
        assert_eq!(java_hash("SMART"), 79011241);
    }

    // A small answer: IBUSOPT and BEST groups, 6523 with an excluded
    // exchange; the rows of BEST come again per exchange as SMART first.
    #[test]
    fn rows_per_group_exchange_and_listed_exchange() {
        let m = msg("35=U|6040=139|55=XYZ|310=STK|6346=11|6994=1|8009=2|100=IBUSOPT|6996=1|6058=XYZ|231=100|6346=11|6775=20261120|6971=20261016|6997=10;5;10|15=USD|6031=32|100=BEST|6996=1|6058=XYZ|231=100|6346=11|6775=20261120;20261016|6971=|6997=5|15=USD|6031=32|6523=AMEX;IBCX;NASDAQ;|6019=1|6031=32|6020=0|6021=0|6022=1|6023=0|6024=4|6025=2|6026=1|6023=0|6027=0.01");
        let a = parse_chain_answer(&m).unwrap();
        assert_eq!(a.key, ChainKey { con_id: 11, symbol: "XYZ".into(), sec_type: "STK".into(), exchange: String::new(), by_6457: false });
        assert_eq!(a.exchanges, ["AMEX", "IBCX", "NASDAQ"]);
        let rows = chain_rows(std::slice::from_ref(&a), false, false);
        let mut names: Vec<&str> = rows.iter().map(|r| r.exchange.as_str()).collect();
        names.sort();
        assert_eq!(names, ["AMEX", "IBUSOPT", "SMART"]);
        let ib = rows.iter().find(|r| r.exchange == "IBUSOPT").unwrap();
        assert_eq!(ib.expirations, ["20261016", "20261120"]);
        assert_eq!(ib.strikes, [5.0, 10.0]);
        assert_eq!(rows.iter().find(|r| r.exchange == "SMART").unwrap().strikes, [5.0]);
        // ISLAND2NASDAQ: only IBCX is excluded.
        assert_eq!(chain_rows(&[a], false, true).len(), 4);
    }
}
