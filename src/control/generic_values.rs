//! The generic ticks on the wire (ibx#450): which ticks of a valid list go
//! to the farm and when, the entry each one is, and how the values of their
//! `35=G` blocks become API ticks, as the reference does it.
//!
//! - Each generic tick is its own `35=V` entry, `264` = its request code
//!   (`generictick.b7.a(dy,MarketDataType,MarketDataRequestAction,List)@86`).
//!   A tick whose `generictick.b.f()` is false (101, 106, 233, 375, 456,
//!   588) goes with the request, in a message of its own after the top of
//!   book; the others wait for the top of book's acknowledgement
//!   (`generictick.bp.onTopSubscriptionConfirmed()`, `bp.b(b7)`, `bp.e(b7)`)
//!   and go in one message with the exchange map entry, if any, last. The
//!   ticks of a message are in request code order (`bp.e` sorts its list).
//! - A tick that is not valid for the contract (`generictick.b.a(dy,
//!   OverrideValidityCheck)`) is never sent (411 on a stock, 233 on a
//!   currency pair).
//! - A `35=G` block is a 32-bit server tag, an 8-bit length (16-bit for the
//!   ticks of `jmdclient.br.ax`) and the payload (`jmdclient.br.a(byte[])`).
//!   Each tick's decoder sets fields of the contract's record; a field
//!   sends its API tick when its value changed (the record's change set),
//!   in the order of the reference's sender (`jextend.dK.a(List,s,int,pa,
//!   Map,o,h,Set)`), block after block.

use std::collections::HashMap;

/// An API tick of a generic tick block.
#[derive(Debug, Clone, PartialEq)]
pub enum GenTick {
    Price(i32, f64),
    Size(i32, f64),
    Generic(i32, f64),
    Text(i32, String),
}

/// Request codes of the ticks ibx sends and decodes.
pub const OPTION_VOLUME: i32 = 100;
pub const OPTION_OPEN_INTEREST: i32 = 101;
pub const AVERAGE_OPTION_VOLUME: i32 = 105;
pub const IMPLIED_VOLATILITY: i32 = 106;
pub const MISC_STATS: i32 = 165;
pub const AUCTION: i32 = 225;
pub const RT_VOLUME: i32 = 233;
pub const SHORTABLE: i32 = 236;
pub const TRADE_COUNT: i32 = 293;
pub const TRADE_RATE: i32 = 294;
pub const VOLUME_RATE: i32 = 295;
pub const LAST_RTH_TRADE: i32 = 318;
pub const RT_TRADE_VOLUME: i32 = 375;
pub const RT_HISTORICAL_VOLATILITY: i32 = 411;
pub const DIVIDENDS: i32 = 456;
/// Historical volatility, asked as 104 (`generictick.a7`, request code 512,
/// alias 104).
pub const HISTORICAL_VOLATILITY: i32 = 512;
pub const FUTURES_OPEN_INTEREST: i32 = 588;

/// The ticks ibx subscribes: the others of a valid list are accepted and
/// not sent (their decoders are not read yet).
const SENT: [i32; 18] = [
    OPTION_VOLUME, OPTION_OPEN_INTEREST, AVERAGE_OPTION_VOLUME, IMPLIED_VOLATILITY, MISC_STATS, AUCTION,
    RT_VOLUME, SHORTABLE, TRADE_COUNT, TRADE_RATE, VOLUME_RATE, LAST_RTH_TRADE, RT_TRADE_VOLUME,
    RT_HISTORICAL_VOLATILITY, DIVIDENDS, HISTORICAL_VOLATILITY, FUTURES_OPEN_INTEREST, 104,
];

/// The ticks whose block has a 16-bit length (`jmdclient.br.ax`).
const LONG_LENGTH: [i32; 29] = [
    257, 258, 256, 292, 247, 385, 386, 434, 433, 454, 481, 490, 491, 496, 546, 593, 594, 631, 628, 633, 705,
    669, 687, 691, 678, 699, 700, 703, 726,
];

/// The ticks whose block has no length (`jmdclient.br.ay`).
const NO_LENGTH: [i32; 6] = [376, 320, 530, 532, 221, 619];

/// How the length of a block of this tick is written: 1 or 2 bytes, 0
/// when it has none (ibx does not read those).
pub fn length_width(code: i32) -> usize {
    if LONG_LENGTH.contains(&code) {
        2
    } else if NO_LENGTH.contains(&code) {
        0
    } else {
        1
    }
}

/// Whether ibx sends this tick.
pub fn sent(code: i32) -> bool {
    SENT.contains(&code)
}

/// Whether the tick goes with the request (`generictick.b.f()` false):
/// the others wait for the top of book's acknowledgement.
pub fn at_once(code: i32) -> bool {
    matches!(code, OPTION_OPEN_INTEREST | IMPLIED_VOLATILITY | RT_VOLUME | RT_TRADE_VOLUME | DIVIDENDS | FUTURES_OPEN_INTEREST)
}

fn one_of(sec_type: &str, set: &[&str]) -> bool {
    let st = if sec_type.is_empty() { "STK" } else { sec_type };
    set.iter().any(|s| s.eq_ignore_ascii_case(st))
}

/// Whether the tick is valid for a contract of this security type
/// (`generictick.b.a(dy, OverrideValidityCheck)` of each tick, with the
/// security type sets of `jfix.eh`); a combo is valid for none. Per tick:
/// 100, 105, 106, 104/512: the option underlyings (`eh.ak()`: STK, IND,
/// FUT); 101: those and the options (`eh.d()`: OPT, FOP, IOPT); 165, 293 to
/// 295, 318: any contract; 225: STK, FUT, OPT; 233, 375: a contract with a
/// volume (not CASH, not CRYPTO); 236: STK, BOND, BILL, FIXED, IOPT; 456:
/// `eh.K()` (STK, FUT, OPT, IND, FOP, CFD, SLB); 411, 588: FUT.
pub fn valid_for(code: i32, sec_type: &str) -> bool {
    if one_of(sec_type, &["BAG"]) {
        return false;
    }
    match code {
        OPTION_VOLUME | AVERAGE_OPTION_VOLUME | IMPLIED_VOLATILITY | HISTORICAL_VOLATILITY | 104 =>
            one_of(sec_type, &["STK", "IND", "FUT"]),
        OPTION_OPEN_INTEREST => one_of(sec_type, &["STK", "IND", "FUT", "OPT", "FOP", "IOPT"]),
        MISC_STATS | TRADE_COUNT | TRADE_RATE | VOLUME_RATE | LAST_RTH_TRADE => true,
        AUCTION => one_of(sec_type, &["STK", "FUT", "OPT"]),
        RT_VOLUME | RT_TRADE_VOLUME => !one_of(sec_type, &["CASH", "CRYPTO"]),
        SHORTABLE => one_of(sec_type, &["STK", "BOND", "BILL", "FIXED", "IOPT"]),
        DIVIDENDS => one_of(sec_type, &["STK", "FUT", "OPT", "IND", "FOP", "CFD", "SLB"]),
        RT_HISTORICAL_VOLATILITY | FUTURES_OPEN_INTEREST => one_of(sec_type, &["FUT"]),
        _ => false,
    }
}

/// The exchange of a tick's entry: the contract's (`jclient.dy.d()`, the
/// routing exchange of its top of book), except the auction of a stock
/// asked on SMART, which goes to its primary exchange when known
/// (`generictick.h.b(dy)`; captured 05/10/2026: AAPL 225 on NASDAQ).
pub fn entry_exchange<'a>(code: i32, sec_type: &str, exchange: &'a str, primary: &'a str) -> &'a str {
    if code == AUCTION && one_of(sec_type, &["STK"]) && matches!(exchange, "" | "SMART" | "BEST") && !primary.is_empty() {
        primary
    } else {
        exchange
    }
}

// ── Reading a payload ──

struct Reader<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, at: 0 }
    }

    fn bytes<const N: usize>(&mut self) -> Option<[u8; N]> {
        let b = self.data.get(self.at..self.at + N)?;
        self.at += N;
        b.try_into().ok()
    }

    fn int(&mut self) -> Option<i32> {
        self.bytes::<4>().map(i32::from_be_bytes)
    }

    fn long(&mut self) -> Option<i64> {
        self.bytes::<8>().map(i64::from_be_bytes)
    }

    fn float(&mut self) -> Option<f64> {
        self.bytes::<4>().map(|b| f32::from_be_bytes(b) as f64)
    }

    fn double(&mut self) -> Option<f64> {
        self.bytes::<8>().map(f64::from_be_bytes)
    }

    fn available(&self) -> usize {
        self.data.len().saturating_sub(self.at)
    }
}

// ── The record ──

/// The record fields the generic ticks set (the reference's
/// `jclient.pa` / `jclient.record.b` fields), each with its change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Field {
    CallVolume, PutVolume, CallOpenInterest, PutOpenInterest, AvgCallVolume, AvgPutVolume,
    ImpliedVol, HistVol, AvgVolume, Week13Hi, Week13Lo, Week26Hi, Week26Lo, Week52Hi, Week52Lo,
    AuctionVolume, AuctionImbalance, AuctionPrice, RegulatoryImbalance, Shortable, ShortableShares,
    TradeCount, TradeRate, VolumeRate, LastRthTrade, FuturesOpenInterest,
}

/// The unset value of an int field (the reference's `Integer.MAX_VALUE`).
const UNSET_INT: i64 = i32::MAX as i64;

/// The generic tick fields of one contract, as the reference's record
/// keeps them: an API tick goes out when its field changes.
#[derive(Debug, Default, Clone)]
pub struct GenericRecord {
    ints: HashMap<Field, i64>,
    doubles: HashMap<Field, u64>,
    dividends: Option<String>,
    /// RTVolume and RT trade volume: the last value, volume and count.
    rt: [Option<(f64, i64, i32)>; 2],
}

impl GenericRecord {
    /// Set an int field; true when it changed.
    fn set_int(&mut self, f: Field, v: i64) -> bool {
        let old = self.ints.insert(f, v).unwrap_or(UNSET_INT);
        old != v
    }

    fn int(&self, f: Field) -> i64 {
        self.ints.get(&f).copied().unwrap_or(UNSET_INT)
    }

    /// Set a double field; true when it changed.
    fn set_double(&mut self, f: Field, v: f64) -> bool {
        let old = self.doubles.insert(f, v.to_bits()).map_or(f64::MAX, f64::from_bits);
        old != v
    }
}

/// What a block's decoding needs from the contract: its price tick (the
/// RTVolume price is rounded to it) and the clock (the RTVolume time is
/// the time the block was read, in ms).
#[derive(Debug, Clone, Copy)]
pub struct DecodeCtx {
    pub min_tick: f64,
    pub now_ms: i64,
}

/// sqrt(252): a daily volatility to an annual one (`optionmodel.O.b`).
fn annual(daily: f64) -> f64 {
    252f64.sqrt() * daily
}

fn size(out: &mut Vec<GenTick>, tick: i32, v: i64) {
    if v != UNSET_INT {
        out.push(GenTick::Size(tick, v as f64));
    }
}

fn price(out: &mut Vec<GenTick>, tick: i32, v: f64) {
    if v.is_finite() && v != f64::MAX {
        out.push(GenTick::Price(tick, java_price(v)));
    }
}

/// `(double)(int)(v * 60)`, Java's d2i: a rate per second as a rate per
/// minute (`jextend.dK.a(...)@2089-2094`).
fn per_minute(v: f64) -> f64 {
    let m = v * 60.0;
    if m.is_nan() { 0.0 } else { m.clamp(i32::MIN as f64, i32::MAX as f64).trunc() }
}

/// Decode the payload of one block of tick `code` into the record; the
/// API ticks of the fields that changed, in the reference's order. A
/// payload too short for its tick sets nothing, as the reference's parse
/// error.
pub fn decode(code: i32, payload: &[u8], rec: &mut GenericRecord, ctx: DecodeCtx) -> Vec<GenTick> {
    let mut out = Vec::new();
    let mut r = Reader::new(payload);
    match code {
        // `generictick.a6`: call and put volume (`pa.o(int)`, `pa.p(int)`).
        OPTION_VOLUME => {
            let (Some(call), Some(put)) = (r.int(), r.int()) else { return out };
            let (c, p) = (rec.set_int(Field::CallVolume, call as i64), rec.set_int(Field::PutVolume, put as i64));
            if c { size(&mut out, 29, call as i64); }
            if p { size(&mut out, 30, put as i64); }
        }
        // `generictick.as`: call and put open interest.
        OPTION_OPEN_INTEREST => {
            let (Some(call), Some(put)) = (r.int(), r.int()) else { return out };
            let (c, p) = (rec.set_int(Field::CallOpenInterest, call as i64), rec.set_int(Field::PutOpenInterest, put as i64));
            if c { size(&mut out, 27, call as i64); }
            if p { size(&mut out, 28, put as i64); }
        }
        // `generictick.a3`: average call and put volume; tick 87 is their
        // sum (`jclient.pa.eZ()`), or the one known.
        AVERAGE_OPTION_VOLUME => {
            let (Some(call), Some(put)) = (r.int(), r.int()) else { return out };
            let c = rec.set_int(Field::AvgCallVolume, call as i64);
            let p = rec.set_int(Field::AvgPutVolume, put as i64);
            if c || p {
                let (a, b) = (rec.int(Field::AvgCallVolume), rec.int(Field::AvgPutVolume));
                let sum = match (a == UNSET_INT, b == UNSET_INT) {
                    (false, false) => (a as i32).wrapping_add(b as i32) as i64,
                    (false, true) => a,
                    (true, _) => b,
                };
                size(&mut out, 87, sum);
            }
        }
        // `generictick.a5`: the daily implied volatility; tick 24 is it a
        // year.
        IMPLIED_VOLATILITY => {
            let Some(d) = r.double() else { return out };
            let valid = d.is_finite() && d != f64::MAX;
            if rec.set_double(Field::ImpliedVol, if valid { d } else { f64::NAN }) && valid {
                out.push(GenTick::Generic(24, annual(d)));
            }
        }
        // `generictick.a7`: the daily historical volatility; tick 23.
        HISTORICAL_VOLATILITY => {
            let Some(d) = r.double() else { return out };
            if rec.set_double(Field::HistVol, d) && d != f64::MAX {
                out.push(GenTick::Generic(23, annual(d)));
            }
        }
        // `generictick.ah`: int pairs, then float pairs; 207 is the
        // average volume, 201 to 206 the 13, 26 and 52 week highs and lows.
        MISC_STATS => {
            let mut ints: HashMap<i32, i32> = HashMap::new();
            let mut floats: HashMap<i32, f64> = HashMap::new();
            let Some(n) = r.int() else { return out };
            for _ in 0..n.max(0) {
                let (Some(k), Some(v)) = (r.int(), r.int()) else { return out };
                ints.insert(k, v);
            }
            let Some(m) = r.int() else { return out };
            for _ in 0..m.max(0) {
                let (Some(k), Some(v)) = (r.int(), r.float()) else { return out };
                floats.insert(k, v);
            }
            let avg = ints.get(&207).is_some_and(|&v| rec.set_int(Field::AvgVolume, v as i64));
            if avg { size(&mut out, 21, rec.int(Field::AvgVolume)); }
            for (key, field, tick) in [
                (201, Field::Week13Hi, 16), (202, Field::Week13Lo, 15), (203, Field::Week26Hi, 18),
                (204, Field::Week26Lo, 17), (205, Field::Week52Hi, 20), (206, Field::Week52Lo, 19),
            ] {
                if let Some(&v) = floats.get(&key) && rec.set_double(field, v) {
                    price(&mut out, tick, v);
                }
            }
        }
        // `generictick.h`: volume, imbalance, price, side, one int; then
        // the regulatory imbalance seventh of the optional values.
        AUCTION => {
            let (Some(volume), Some(imbalance), Some(px), Some(_side), Some(_)) = (r.int(), r.int(), r.float(), r.int(), r.int()) else {
                return out;
            };
            let mut regulatory = UNSET_INT;
            for k in 0..7 {
                if r.available() == 0 {
                    break;
                }
                // Doubles are floats on the wire; the ints are 2, 3 and 6.
                let v = r.int().map_or(UNSET_INT, |v| v as i64);
                if k == 6 && v != i32::MIN as i64 {
                    regulatory = v;
                }
            }
            let v = rec.set_int(Field::AuctionVolume, volume as i64);
            let i = rec.set_int(Field::AuctionImbalance, imbalance as i64);
            let p = rec.set_double(Field::AuctionPrice, px);
            let g = rec.set_int(Field::RegulatoryImbalance, regulatory);
            if v { size(&mut out, 34, volume as i64); }
            if i { size(&mut out, 36, imbalance as i64); }
            if p { price(&mut out, 35, px); }
            if g { size(&mut out, 61, regulatory); }
        }
        // `generictick.aL`: the shortable value, then the shares when
        // present.
        SHORTABLE => {
            let Some(value) = r.int() else { return out };
            let shares = if r.available() >= 4 { r.int().unwrap_or(i32::MAX) } else { i32::MAX };
            let v = rec.set_int(Field::Shortable, value as i64);
            let s = rec.set_int(Field::ShortableShares, shares as i64);
            if v { out.push(GenTick::Generic(46, value as f64)); }
            if s { size(&mut out, 89, shares as i64); }
        }
        TRADE_COUNT => {
            let Some(v) = r.int() else { return out };
            if rec.set_int(Field::TradeCount, v as i64) {
                out.push(GenTick::Generic(54, v as f64));
            }
        }
        TRADE_RATE | VOLUME_RATE => {
            let Some(v) = r.double() else { return out };
            let (field, tick) = if code == TRADE_RATE { (Field::TradeRate, 55) } else { (Field::VolumeRate, 56) };
            if rec.set_double(field, v) && v != f64::MAX {
                out.push(GenTick::Generic(tick, per_minute(v)));
            }
        }
        // `generictick.aa`: the price, then two ints.
        LAST_RTH_TRADE => {
            let (Some(px), Some(_), Some(_)) = (r.double(), r.int(), r.int()) else { return out };
            if rec.set_double(Field::LastRthTrade, px) {
                price(&mut out, 57, px);
            }
        }
        // `generictick.K`: the open interest.
        FUTURES_OPEN_INTEREST => {
            let Some(v) = r.int() else { return out };
            if rec.set_int(Field::FuturesOpenInterest, v as i64) {
                size(&mut out, 86, v as i64);
            }
        }
        // `generictick.C`: a 4-byte length, then one line of text.
        DIVIDENDS => {
            let Some(rest) = payload.get(4..) else { return out };
            let line = rest.split(|&b| b == b'\n' || b == b'\r').next().unwrap_or(&[]);
            let text = dividends_text(&String::from_utf8_lossy(line));
            if rec.dividends.as_deref() != Some(text.as_str()) {
                rec.dividends = Some(text.clone());
                if !text.is_empty() {
                    out.push(GenTick::Text(59, text));
                }
            }
        }
        RT_VOLUME | RT_TRADE_VOLUME => {
            let (Some(value), Some(volume), Some(count)) = (r.double(), r.long(), r.int()) else { return out };
            if let Some(text) = rt_volume(rec, code == RT_TRADE_VOLUME, value, volume, count, ctx) {
                out.push(GenTick::Text(if code == RT_VOLUME { 48 } else { 77 }, text));
            }
        }
        _ => {}
    }
    out
}

/// One RTVolume block (`generictick.aB.a(...)`): the total value, volume
/// and trade count of the day. The first one only sets the record; a later
/// one that changes anything gives "price;size;time;volume;vwap;single"
/// (`jclient.nw.a(jmarketrules.o)`): the price of what traded since (the
/// value traded over the volume, on the contract's tick), its size, the
/// time it was read, the day's volume, its average price, and whether one
/// trade at most made it.
fn rt_volume(rec: &mut GenericRecord, trade: bool, value: f64, volume: i64, count: i32, ctx: DecodeCtx) -> Option<String> {
    if volume == i64::MAX {
        return None;
    }
    let slot = &mut rec.rt[trade as usize];
    let prev = slot.replace((value, volume, count));
    let (pv, pvol, pcount) = prev?;
    let d_count = count.wrapping_sub(pcount);
    let d_vol = volume - pvol;
    let d_value = value - pv;
    if d_count == 0 && d_vol == 0 && d_value == 0.0 {
        return None;
    }
    let px = if d_vol == 0 { None } else { Some(round_to_tick(d_value / d_vol as f64, ctx.min_tick)) };
    let vwap = if volume == 0 { None } else { Some(value / volume as f64) };
    let decimals = tick_decimals(ctx.min_tick);
    let text = |v: f64| rule_price_text(v, decimals);
    Some(format!(
        "{};{};{};{};{};{}",
        px.map(text).unwrap_or_default(), decimal16(d_vol), ctx.now_ms, decimal16(volume),
        vwap.map(text).unwrap_or_default(), d_count < 2,
    ))
}

fn round_to_tick(v: f64, tick: f64) -> f64 {
    if tick > 0.0 && tick.is_finite() { (v / tick).round() * tick } else { v }
}

/// A size of the reference's decimal type as text: scale 16 (`0E-16` for
/// zero, as `BigDecimal.toString`).
fn decimal16(v: i64) -> String {
    if v == 0 { "0E-16".into() } else { format!("{v}.0000000000000000") }
}

/// The decimals of a price tick (0.01: 2), the fewest decimals the
/// market rule writes for a rule of that one increment
/// (`jmarketrules.o.s()`, `o.b(int)`); 3 when the tick is not known.
fn tick_decimals(tick: f64) -> usize {
    if !(tick > 0.0 && tick.is_finite()) {
        return 3;
    }
    format!("{tick}").split_once('.').map_or(0, |(_, f)| f.len())
}

/// A price as the market rule writes it (`jmarketrules.o.e(double)`:
/// `min_decimals` to eight decimals, half even; 0 is "0"; captured
/// 05/10/2026: AAPL "334.50").
pub fn rule_price_text(v: f64, min_decimals: usize) -> String {
    if v == 0.0 {
        return "0".into();
    }
    let mut s = format!("{v:.8}");
    let keep = min_decimals.clamp(1, 8);
    while s.ends_with('0') && s.split_once('.').is_some_and(|(_, f)| f.len() > keep) {
        s.pop();
    }
    s
}

/// A price as the API gets it: the market rule's text read back.
pub fn java_price(v: f64) -> f64 {
    rule_price_text(v, 1).parse().unwrap_or(v)
}

/// The dividends of a 456 block (`generictick.bh.a(int,String)`, `bh.o()`):
/// "past 12 months,next 12 months,next date,next amount", each amount
/// with two to six decimals (`feature.company.fundamentals.ui.h.a(double)`),
/// the date as yyyyMMdd; a token that is no number or no date is empty.
pub fn dividends_text(line: &str) -> String {
    let mut tokens = line.split(',').filter(|t| !t.is_empty());
    let amount = |t: Option<&str>| t.and_then(|t| t.trim().parse::<f64>().ok()).map(dividend_amount).unwrap_or_default();
    let ttm = amount(tokens.next());
    let next12 = amount(tokens.next());
    let date = tokens.next().filter(|t| t.len() == 8 && t.bytes().all(|b| b.is_ascii_digit())).unwrap_or("").to_string();
    let next = amount(tokens.next());
    format!("{ttm},{next12},{date},{next}")
}

/// `feature.company.fundamentals.ui.h.a(double)`: the value to ten
/// significant digits, half even, trailing zeros dropped, gives its
/// decimals, kept between two and six; grouped thousands.
fn dividend_amount(v: f64) -> String {
    if !v.is_finite() || v == f64::MAX {
        return String::new();
    }
    let sig = format!("{v:.9e}");
    let (mantissa, exp) = sig.split_once('e').unwrap_or((&sig, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let frac_digits = mantissa.split_once('.').map_or(0, |(_, f)| f.trim_end_matches('0').len() as i32);
    let scale = (frac_digits - exp).clamp(2, 6) as usize;
    let text = format!("{v:.scale$}");
    let (int, frac) = text.split_once('.').unwrap_or((&text, ""));
    let (sign, digits) = int.strip_prefix('-').map_or(("", int), |d| ("-", d));
    let mut grouped = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(c);
    }
    format!("{sign}{grouped}.{frac}")
}

/// The blocks of a `35=G` body (after its 2-byte bit count): (server tag,
/// payload). `code_of` names the tick of a server tag (None: unknown, the
/// reading stops, as the reference cannot know the block's length).
pub fn blocks(body: &[u8], code_of: impl Fn(u32) -> Option<i32>) -> Vec<(u32, Option<i32>, &[u8])> {
    let mut out = Vec::new();
    let Some(bits) = body.get(..2).map(|b| u16::from_be_bytes([b[0], b[1]]) as usize) else { return out };
    let mut rest = &body[2..(2 + bits.div_ceil(8)).min(body.len())];
    while rest.len() >= 4 {
        let tag = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]);
        let code = code_of(tag);
        let width = code.map_or(1, length_width);
        let (len, head) = match width {
            2 if rest.len() >= 6 => (u16::from_be_bytes([rest[4], rest[5]]) as usize, 6),
            1 if rest.len() >= 5 => (rest[4] as usize, 5),
            _ => break,
        };
        let end = (head + len).min(rest.len());
        out.push((tag, code, &rest[head..end]));
        rest = &rest[end..];
        if code.is_none() {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    const CTX: DecodeCtx = DecodeCtx { min_tick: 0.01, now_ms: 1791219886917 };

    // AAPL, 05/10/2026 (b2_generic, frame 9004): open interest and implied
    // volatility, as the reference gave them (ticks 27, 28, 24).
    #[test]
    fn open_interest_and_implied_volatility() {
        let mut rec = GenericRecord::default();
        assert_eq!(decode(101, &hex("002987a0001f18946ac37c2c"), &mut rec, CTX),
            vec![GenTick::Size(27, 2721696.0), GenTick::Size(28, 2037908.0)]);
        assert_eq!(decode(106, &hex("3f907529f97d21aa"), &mut rec, CTX), vec![GenTick::Generic(24, 0.25513421812193265)]);
        // The same values again: nothing changed.
        assert!(decode(101, &hex("002987a0001f18946ac37c2c"), &mut rec, CTX).is_empty());
    }

    // Frame 9080: one block of each tick of the second message.
    #[test]
    fn blocks_of_the_first_frame() {
        let mut rec = GenericRecord::default();
        let d = |code, h: &str, rec: &mut GenericRecord| decode(code, &hex(h), rec, CTX);
        assert!(d(233, "4196af335ee3acb3000000000004589c000672c400000000", &mut rec).is_empty());
        assert!(d(375, "41910c15a777b785000000000003441d0001648d00000000", &mut rec).is_empty());
        assert_eq!(d(456, "00000018312e30362c312e312c32303236313130392c302e32372c34", &mut rec),
            vec![GenTick::Text(59, "1.06,1.10,20261109,0.27".into())]);
        assert_eq!(d(100, "0006979e0002dbdd", &mut rec), vec![GenTick::Size(29, 432030.0), GenTick::Size(30, 187357.0)]);
        assert_eq!(d(105, "000d13c300078c2e", &mut rec), vec![GenTick::Size(87, 1351665.0)]);
        assert_eq!(d(165, "000000010000030002be0e950000000a000000c943acab85000000ca43960000000000cb43acab85000000cc437578d5\
            000000cd43acab85000000ce4372f604000000d043acab85000000d1439af333000000d2437df22d00000198466408cd", &mut rec),
            vec![GenTick::Price(16, 345.33999634), GenTick::Price(15, 300.0), GenTick::Price(18, 345.33999634),
                 GenTick::Price(17, 245.47200012), GenTick::Price(20, 345.33999634), GenTick::Price(19, 242.96099854)]);
        assert_eq!(d(225, "0000000000000000000000000000005f00000000bf800000bf8000000000000000000000000000000000000000000000", &mut rec),
            vec![GenTick::Size(34, 0.0), GenTick::Size(36, 0.0), GenTick::Price(35, 0.0), GenTick::Size(61, 0.0)]);
        assert_eq!(d(236, "000000030b566f50", &mut rec), vec![GenTick::Generic(46, 3.0), GenTick::Size(89, 190213968.0)]);
        assert_eq!(d(293, "0001648d", &mut rec), vec![GenTick::Generic(54, 91277.0)]);
        assert_eq!(d(294, "400d8469ee58469f", &mut rec), vec![GenTick::Generic(55, 221.0)]);
        assert_eq!(d(295, "4076485f2fef7eca", &mut rec), vec![GenTick::Generic(56, 21391.0)]);
        assert_eq!(d(318, "00000000000000000800000100000000", &mut rec), vec![GenTick::Price(57, 0.0)]);
        assert_eq!(d(512, "3f8dc2be5611848d000000016ac32ef8", &mut rec), vec![GenTick::Generic(23, 0.23068199851119725)]);
        // Frame 9129: what traded since, then the shortable shares alone.
        assert_eq!(d(233, "4196af870119def300000000000458ac000672ce00000000", &mut rec),
            vec![GenTick::Text(48, "334.53;16.0000000000000000;1791219886917;284844.0000000000000000;334.04434805;false".into())]);
        assert_eq!(rule_price_text(334.5, tick_decimals(0.01)), "334.50");
        assert_eq!(d(375, "41910c599b4f1068000000000003442a0001649200000000", &mut rec),
            vec![GenTick::Text(77, "334.54;13.0000000000000000;1791219886917;214058.0000000000000000;334.04736486;false".into())]);
        assert_eq!(d(236, "000000030b566f1e", &mut rec), vec![GenTick::Size(89, 190213918.0)]);
        // Frames 9160 and 9213: one trade; a count change with no volume.
        assert_eq!(d(233, "4196af8c3b48ec3a00000000000458ad000672cf00000000", &mut rec),
            vec![GenTick::Text(48, "334.55;1.0000000000000000;1791219886917;284845.0000000000000000;334.04434981;true".into())]);
        assert!(d(233, "4196af9175813e2500000000000458ae000672d200000000", &mut rec).len() == 1);
        assert!(d(233, "4196af96afb29db400000000000458af000672d500000000", &mut rec).len() == 1);
        assert_eq!(d(233, "4196af96afb29db400000000000458af000672d700000000", &mut rec),
            vec![GenTick::Text(48, ";0E-16;1791219886917;284847.0000000000000000;334.04435337;false".into())]);
    }

    #[test]
    fn futures_open_interest() {
        let mut rec = GenericRecord::default();
        assert_eq!(decode(588, &hex("00020134"), &mut rec, CTX), vec![GenTick::Size(86, 131380.0)]);
    }

    #[test]
    fn dividend_amounts() {
        assert_eq!(dividends_text("1.06,1.1,20261109,0.27,4"), "1.06,1.10,20261109,0.27");
        assert_eq!(dividend_amount(2.0), "2.00");
        assert_eq!(dividend_amount(0.1234567), "0.123457");
        assert_eq!(dividend_amount(1234.5), "1,234.50");
        assert_eq!(dividends_text(""), ",,,");
    }

    #[test]
    fn rule_price_texts() {
        assert_eq!(rule_price_text(300.0, 1), "300.0");
        assert_eq!(rule_price_text(f32::from_bits(0x43acab85) as f64, 2), "345.33999634");
        assert_eq!(rule_price_text(0.0, 2), "0");
        assert_eq!(rule_price_text(1.5, tick_decimals(0.00001)), "1.50000");
        assert_eq!(tick_decimals(0.25), 2);
    }

    #[test]
    fn block_lengths() {
        // A 236 block then a 626 block (frame 10537): 8-bit lengths.
        let body = hex("00a00000071f0800000003005f6e7f0000140d0400000000");
        let b = blocks(&body, |t| match t { 0x71f => Some(236), 0x140d => Some(626), _ => None });
        assert_eq!(b.len(), 2);
        assert_eq!(b[0].2, &hex("00000003005f6e7f")[..]);
        // An unknown tag stops the reading.
        assert_eq!(blocks(&body, |_| None).len(), 1);
    }
}
