//! The generic ticks on the wire (ibx#450): which ticks of a valid list go
//! to the farm and when, the entry each one is, and how the values of their
//! generic tick blocks become API ticks, as the reference does it.
//!
//! - Each generic tick is its own entry of the market data request, with
//!   its request code.
//!   A tick the reference does not hold back (101, 106, 233, 375, 456,
//!   588) goes with the request, in a message of its own after the top of
//!   book; the others wait for the top of book's acknowledgement
//!   and go in one message with the exchange map entry, if any, last. The
//!   ticks of a message are in request code order (the reference sorts them).
//! - A tick that is not valid for the contract (the reference's validity
//!   check) is never sent (411 on a stock, 233 on a
//!   currency pair).
//! - A block of the market data update is a 32-bit server code, an 8-bit
//!   length (16-bit for the ticks of `LONG_LENGTH`) and the payload.
//!   Each tick's decoder sets fields of the contract's record; a field
//!   sends its API tick when its value changed (the record's change set),
//!   in the order the reference sends them, block after block.

use std::collections::HashMap;

/// An API tick of a generic tick block.
#[derive(Debug, Clone, PartialEq)]
pub enum GenTick {
    Price(i32, f64),
    Size(i32, f64),
    Generic(i32, f64),
    Text(i32, String),
    /// An option computation of an option's model (ibx#502).
    Option(crate::control::optcalc::OptionComputation),
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
/// Historical volatility, asked as 104 (request code 512, alias 104).
pub const HISTORICAL_VOLATILITY: i32 = 512;
pub const FUTURES_OPEN_INTEREST: i32 = 588;
/// Pl Price (alias 221): the mark price, tick 37.
pub const PL_PRICE: i32 = 232;
/// Fundamentals (alias 47): the ratios, tick 47.
pub const FUNDAMENTALS: i32 = 258;
/// IPOHLMPRC: asked, its ticks (101, 102) not read yet.
pub const IPO_PRICES: i32 = 586;
/// Short-Term Volume X Mins: ticks 63, 64, 65.
pub const SHORT_TERM_VOLUME: i32 = 595;
/// Creditman Slow Mark Price: tick 79.
pub const SLOW_MARK_PRICE: i32 = 619;
/// An ETF's net asset value, last and frozen last (ibx#641): one double
/// each, ticks 96 and 97, asked for a stock of type ETF or ETMF once its
/// top of book is acknowledged; -1 is no value (the reference 10.51, paper,
/// 13/10/2026 regular hours, SPY: 577 answered 777.86932373, given as tick
/// 96; 623 answered -1, nothing given), and so is 0 (SPY before the open,
/// 19/10/2026: 577 answered 0, nothing given, ibx#604).
pub const ETF_NAV_LAST: i32 = 577;
pub const ETF_FROZEN_NAV_LAST: i32 = 623;
/// Index Future Premium (ibx#495): one double, tick 31,
/// for an index only (the reference 10.51 on SPX, paper 12/10/2026).
pub const INDEX_FUTURE_PREMIUM: i32 = 162;

/// The ticks ibx subscribes: the others of a valid list are accepted and
/// not sent (their decoders are not read yet).
const SENT: [i32; 27] = [
    OPTION_VOLUME, OPTION_OPEN_INTEREST, AVERAGE_OPTION_VOLUME, IMPLIED_VOLATILITY, MISC_STATS, AUCTION,
    RT_VOLUME, SHORTABLE, TRADE_COUNT, TRADE_RATE, VOLUME_RATE, LAST_RTH_TRADE, RT_TRADE_VOLUME,
    RT_HISTORICAL_VOLATILITY, DIVIDENDS, HISTORICAL_VOLATILITY, FUTURES_OPEN_INTEREST, 104,
    PL_PRICE, FUNDAMENTALS, IPO_PRICES, SHORT_TERM_VOLUME, SLOW_MARK_PRICE, INDEX_FUTURE_PREMIUM,
    ETF_NAV_LAST, ETF_FROZEN_NAV_LAST, ODD_LOT,
];

/// The odd-lot bid and ask quotes (ibx#616), from session level 225.
pub const ODD_LOT: i32 = 787;

/// The ticks whose block has a 16-bit length.
const LONG_LENGTH: [i32; 29] = [
    257, 258, 256, 292, 247, 385, 386, 434, 433, 454, 481, 490, 491, 496, 546, 593, 594, 631, 628, 633, 705,
    669, 687, 691, 678, 699, 700, 703, 726,
];

/// The ticks whose block has no length.
const NO_LENGTH: [i32; 7] = [376, 320, 530, 532, 221, 619, ODD_LOT];

/// How the length of a block of this tick is written: 1 or 2 bytes, 0
/// when it has none (its payload is a list of quote entries, which ends
/// itself: `entry_list`).
pub fn length_width(code: i32) -> usize {
    if LONG_LENGTH.contains(&code) {
        2
    } else if NO_LENGTH.contains(&code) {
        0
    } else {
        1
    }
}

/// Whether the tick is asked only for a stock of type ETF or ETMF
/// (ibx#641).
pub fn etf_only(code: i32) -> bool {
    matches!(code, ETF_NAV_LAST | ETF_FROZEN_NAV_LAST)
}

/// Whether a stock type is one the ETF ticks are asked for (ibx#641).
pub fn is_etf_type(stock_type: &str) -> bool {
    stock_type.eq_ignore_ascii_case("ETF") || stock_type.eq_ignore_ascii_case("ETMF")
}

/// Whether ibx sends this tick.
pub fn sent(code: i32) -> bool {
    SENT.contains(&code)
}

/// Whether the tick goes with the request (the reference does not hold it):
/// the others wait for the top of book's acknowledgement.
pub fn at_once(code: i32) -> bool {
    matches!(code, OPTION_OPEN_INTEREST | IMPLIED_VOLATILITY | RT_VOLUME | RT_TRADE_VOLUME | DIVIDENDS | FUTURES_OPEN_INTEREST
        | PL_PRICE | FUNDAMENTALS | SLOW_MARK_PRICE | INDEX_FUTURE_PREMIUM | ODD_LOT)
}

fn one_of(sec_type: &str, set: &[&str]) -> bool {
    let st = if sec_type.is_empty() { "STK" } else { sec_type };
    set.iter().any(|s| s.eq_ignore_ascii_case(st))
}

/// Whether the tick is valid for a contract of this security type
/// (the reference's validity check of each tick, with its security type
/// sets); a combo is valid for none. Per tick:
/// 100, 105, 106, 104/512: the option underlyings (STK, IND,
/// FUT); 101: those and the options (OPT, FOP, IOPT); 165, 293 to
/// 295, 318: any contract; 225: STK, FUT, OPT; 233, 375: a contract with a
/// volume (not CASH, not CRYPTO); 236: STK, BOND, BILL, FIXED, IOPT; 456:
/// STK, FUT, OPT, IND, FOP, CFD, SLB; 411, 588: FUT. 232 and
/// 619: any contract (the reference sends them when the top of book is
/// not restricted; captured 07/10/2026 on
/// a stock, an option and a future); 258: not CFD, FOP, FUT, IOPT, OPT,
/// SLB, WAR (the logon's fundamentals feature is taken as
/// on); 586: a stock; 595: any contract its list allows.
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
        PL_PRICE | SLOW_MARK_PRICE | SHORT_TERM_VOLUME | ODD_LOT => true,
        FUNDAMENTALS => !one_of(sec_type, &["CFD", "FOP", "FUT", "IOPT", "OPT", "SLB", "WAR"]),
        IPO_PRICES => one_of(sec_type, &["STK"]),
        INDEX_FUTURE_PREMIUM => one_of(sec_type, &["IND"]),
        // A stock; its type (ETF, ETMF) is checked when the top of book
        // is acknowledged (`etf_only`).
        ETF_NAV_LAST | ETF_FROZEN_NAV_LAST => one_of(sec_type, &["STK"]),
        _ => false,
    }
}

/// The exchange of a tick's entry: the contract's (in the reference, the
/// routing exchange of its top of book), except the auction of a stock
/// asked on SMART, which goes to its primary exchange when known
/// (captured 05/10/2026: AAPL 225 on NASDAQ), and
/// the fundamentals, always asked on `RTRSFND`.
pub fn entry_exchange<'a>(code: i32, sec_type: &str, exchange: &'a str, primary: &'a str) -> &'a str {
    if code == FUNDAMENTALS {
        return "RTRSFND";
    }
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

/// The record fields the generic ticks set (the fields of the reference's
/// contract record), each with its change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Field {
    CallVolume, PutVolume, CallOpenInterest, PutOpenInterest, AvgCallVolume, AvgPutVolume,
    ImpliedVol, HistVol, AvgVolume, Week13Hi, Week13Lo, Week26Hi, Week26Lo, Week52Hi, Week52Lo,
    AuctionVolume, AuctionImbalance, AuctionPrice, RegulatoryImbalance, Shortable, ShortableShares,
    TradeCount, TradeRate, VolumeRate, LastRthTrade, FuturesOpenInterest,
    PlPrice, SlowMarkPrice, Volume3Min, Volume5Min, Volume10Min, IndexPremium,
    EtfNavLast, EtfFrozenNavLast,
    OddBidPrice, OddAskPrice, OddBidSize, OddAskSize, OddBidExchange, OddAskExchange,
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
    /// The fundamental ratios as tick 47 gives them.
    ratios: Option<String>,
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
    /// The contract's size multiplier (its round lot, as the reference has it):
    /// the short-term volumes are counts of it.
    pub round_lot: i64,
    /// The price tick of the tick's own entry (the third field of its
    /// acknowledgement): the slow mark price is scaled by it.
    pub entry_min_tick: f64,
    /// The real-time volume comes in shares and is given in round lots
    /// (the 10.51 form asked as 765, ibx#642); false in the 10.40 lot form.
    pub rt_in_shares: bool,
}

/// sqrt(252): a daily volatility to an annual one, as the reference.
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
/// minute.
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
        // Call and put volume.
        OPTION_VOLUME => {
            let (Some(call), Some(put)) = (r.int(), r.int()) else { return out };
            let (c, p) = (rec.set_int(Field::CallVolume, call as i64), rec.set_int(Field::PutVolume, put as i64));
            if c { size(&mut out, 29, call as i64); }
            if p { size(&mut out, 30, put as i64); }
        }
        // Call and put open interest.
        OPTION_OPEN_INTEREST => {
            let (Some(call), Some(put)) = (r.int(), r.int()) else { return out };
            let (c, p) = (rec.set_int(Field::CallOpenInterest, call as i64), rec.set_int(Field::PutOpenInterest, put as i64));
            if c { size(&mut out, 27, call as i64); }
            if p { size(&mut out, 28, put as i64); }
        }
        // Average call and put volume; tick 87 is their
        // sum (as the reference adds them), or the one known.
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
        // The daily implied volatility; tick 24 is it a
        // year.
        IMPLIED_VOLATILITY => {
            let Some(d) = r.double() else { return out };
            let valid = d.is_finite() && d != f64::MAX;
            if rec.set_double(Field::ImpliedVol, if valid { d } else { f64::NAN }) && valid {
                out.push(GenTick::Generic(24, annual(d)));
            }
        }
        // Historical volatility (512): the daily one; tick 23.
        // The index future premium, tick 31, as it changes.
        // The odd-lot quotes (ibx#616; the reference 10.51, paper,
        // 12/10/2026, MSFT in regular hours, at level 225): a list of quote
        // entries, the bid and ask prices (0, 1; in price ticks of the
        // entry's acknowledgement), sizes (4, 5) and exchanges (16, 17; a
        // set of the contract's exchanges). For the bid then the ask, the
        // price (105, 106), size (107, 108) and exchanges (109, 110), each
        // when it changed; an unset price (0 or below) or exchange (0)
        // gives none (regular hours 12/11/2026: a side with no odd-lot
        // quote came as price 0, size 0 and no exchange, and the reference
        // gave nothing for it; ibx gave a price of 0.0). The exchanges go
        // as the set's number, written as letters by the farm.
        ODD_LOT => {
            let Some((entries, _)) = entry_list(payload) else { return out };
            let get = |t: u64| entries.iter().find(|(e, _)| *e == t).map(|(_, v)| *v);
            let sides = [
                (0, 4, 16, Field::OddBidPrice, Field::OddBidSize, Field::OddBidExchange, (105, 107, 109)),
                (1, 5, 17, Field::OddAskPrice, Field::OddAskSize, Field::OddAskExchange, (106, 108, 110)),
            ];
            for (p, s, x, pf, sf, xf, (pt, st, xt)) in sides {
                if let Some(raw) = get(p) {
                    let px = raw as f64 * ctx.entry_min_tick;
                    if rec.set_double(pf, px) && raw > 0 {
                        price(&mut out, pt, px);
                    }
                }
                if let Some(raw) = get(s)
                    && rec.set_int(sf, raw) && raw > 0
                {
                    size(&mut out, st, raw);
                }
                if let Some(mask) = get(x)
                    && rec.set_int(xf, mask) && mask != 0
                {
                    out.push(GenTick::Text(xt, mask.to_string()));
                }
            }
        }
        ETF_NAV_LAST | ETF_FROZEN_NAV_LAST => {
            let Some(d) = r.double() else { return out };
            let (field, tick) = if code == ETF_NAV_LAST { (Field::EtfNavLast, 96) } else { (Field::EtfFrozenNavLast, 97) };
            if rec.set_double(field, d) && d != -1.0 && d != 0.0 {
                price(&mut out, tick, d);
            }
        }
        INDEX_FUTURE_PREMIUM => {
            let Some(d) = r.double() else { return out };
            if rec.set_double(Field::IndexPremium, d) && d.is_finite() && d != f64::MAX {
                out.push(GenTick::Generic(31, d));
            }
        }
        HISTORICAL_VOLATILITY => {
            let Some(d) = r.double() else { return out };
            if rec.set_double(Field::HistVol, d) && d != f64::MAX {
                out.push(GenTick::Generic(23, annual(d)));
            }
        }
        // Int pairs, then float pairs; 207 is the
        // average volume, 201 to 206 the 13, 26 and 52 week highs and lows.
        // The 10.51 form gives the average volume in shares as 768, given in
        // round lots, rounded down (the reference 10.51 at level 225, AAPL,
        // 19/10/2026 regular hours: 44451884 shares, a lot of 40, gave
        // 1111297; ibx#604). The 10.40 form gives none from it (05/10/2026).
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
            let average = ints.get(&207).map(|&v| v as i64)
                .or_else(|| ints.get(&768).filter(|_| ctx.rt_in_shares).map(|&v| v as i64 / ctx.round_lot.max(1)));
            let avg = average.is_some_and(|v| rec.set_int(Field::AvgVolume, v));
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
        // Volume, imbalance, price, side, one int; then
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
        // The shortable value, then the shares when
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
        // The price, then two ints.
        LAST_RTH_TRADE => {
            let (Some(px), Some(_), Some(_)) = (r.double(), r.int(), r.int()) else { return out };
            if rec.set_double(Field::LastRthTrade, px) {
                price(&mut out, 57, px);
            }
        }
        // The open interest.
        FUTURES_OPEN_INTEREST => {
            let Some(v) = r.int() else { return out };
            if rec.set_int(Field::FuturesOpenInterest, v as i64) {
                size(&mut out, 86, v as i64);
            }
        }
        // The price, then flags; valid with bit 0 and
        // without bit 27, else unset. Tick 37
        // (captured 07/10/2026: 334.9805908203125 gives 334.98059082).
        PL_PRICE => {
            let (Some(px), Some(flags)) = (r.double(), r.int()) else { return out };
            let valid = flags & 1 != 0 && flags & 0x0800_0000 == 0 && px.is_finite();
            if rec.set_double(Field::PlPrice, if valid { px } else { f64::MAX }) && valid {
                price(&mut out, 37, px);
            }
        }
        // A list of quote entries (no length); the price
        // is the entry of type 2 times the price tick of this tick's own
        // acknowledgement. Tick 79 (captured 07/10/2026: 334890 with tick
        // 0.001 gives 334.89). A negative value is unset.
        SLOW_MARK_PRICE => {
            let Some((entries, _)) = entry_list(payload) else { return out };
            let Some(&(_, raw)) = entries.iter().find(|(t, _)| *t == 2) else { return out };
            let px = raw as f64 * ctx.entry_min_tick;
            let valid = raw >= 0 && px.is_finite();
            if rec.set_double(Field::SlowMarkPrice, if valid { px } else { f64::MAX }) && valid {
                price(&mut out, 79, px);
            }
        }
        // A count, then (minutes, volume) pairs; 3, 5
        // and 10 minutes are ticks 63, 64, 65, each the volume times the
        // contract's size multiplier (captured 07/10/2026: 35, 76, 193 on
        // AAPL, round lot 40, give 1400, 3040, 7720).
        SHORT_TERM_VOLUME => {
            let Some(n) = r.int() else { return out };
            let mut seen: [Option<i32>; 3] = [None; 3];
            for _ in 0..n.max(0) {
                let (Some(k), Some(v)) = (r.int(), r.int()) else { break };
                match k {
                    3 => seen[0] = Some(v),
                    5 => seen[1] = Some(v),
                    10 => seen[2] = Some(v),
                    _ => {}
                }
            }
            for (v, field, tick) in [
                (seen[0], Field::Volume3Min, 63), (seen[1], Field::Volume5Min, 64), (seen[2], Field::Volume10Min, 65),
            ] {
                // In shares when asked as 773 (ibx#641; observed 14/10/2026 on
                // AAPL: 352560 shares given as 352560), in round lots in the
                // 10.40 form.
                let unit = if ctx.rt_in_shares { 1 } else { ctx.round_lot.max(1) };
                if let Some(v) = v && rec.set_int(field, v as i64) {
                    size(&mut out, tick, v as i64 * unit);
                }
            }
        }
        // A length, the text's size, then the zlib of
        // `KEY=VALUE;...`. Tick 47 is the pairs in the order of the
        // reference's map (`ratios_text`); `NOTAVAIL=1` gives nothing.
        FUNDAMENTALS => {
            let (Some(n), Some(_size)) = (r.int(), r.int()) else { return out };
            let Some(packed) = payload.get(8..(4 + n.max(4) as usize).min(payload.len())) else { return out };
            let mut text = String::new();
            use std::io::Read;
            if flate2::read::ZlibDecoder::new(packed).read_to_string(&mut text).is_err() {
                return out;
            }
            let text = ratios_text(&text);
            if rec.ratios.as_deref() != Some(text.as_str()) {
                rec.ratios = Some(text.clone());
                if !text.is_empty() {
                    out.push(GenTick::Text(47, text));
                }
            }
        }
        // Asked as the reference asks it; its ticks (101,
        // 102) are not read yet (no block with a value captured).
        IPO_PRICES => {}
        // A 4-byte length, then one line of text.
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

/// One RTVolume block: the total value, volume
/// and trade count of the day. The first one only sets the record; a later
/// one that changes anything gives "price;size;time;volume;vwap;single"
/// (as the reference writes it): the price of what traded since (the
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
    // Wrapping, as the count: a bad frame must not stop the farm handler.
    let d_vol = volume.wrapping_sub(pvol);
    let d_value = value - pv;
    // The real-time volume is asked in its share form (ibx#642): the
    // price comes from the shares traded, an odd lot included, and the
    // size and volume are given in round lots, truncated; with a lot
    // above 1 every block gives a string (observed 13/10/2026, MSFT,
    // round lot 40, on the reference 10.51).
    // So is the real-time trade volume, asked as 766 (ibx#641; observed
    // 14/10/2026 on MSFT: 2451712 shares given as 61292).
    let lot = if !ctx.rt_in_shares { 1 } else { ctx.round_lot.max(1) };
    if lot == 1 && d_count == 0 && d_vol == 0 && d_value == 0.0 {
        return None;
    }
    let px = if d_vol == 0 { None } else { Some(round_to_tick(d_value / d_vol as f64, ctx.min_tick)) };
    let vwap = if volume == 0 { None } else { Some(value / volume as f64) };
    let decimals = tick_decimals(ctx.min_tick);
    let text = |v: f64| rule_price_text(v, decimals);
    Some(format!(
        "{};{};{};{};{};{}",
        px.map(text).unwrap_or_default(), decimal16(d_vol / lot), ctx.now_ms, decimal16(volume / lot),
        vwap.map(text).unwrap_or_default(), d_count < 2,
    ))
}

/// The code a generic tick is asked under on the wire: the reference 10.51
/// asks the real-time volumes in their share forms, 233 as 765 and 375 as
/// 766, and the short-term volumes (595) as 773 (ibx#641, observed on
/// 13/10/2026 and 14/10/2026 on MSFT and AAPL, regular hours).
pub fn wire_code(code: i32) -> i32 {
    match code {
        RT_VOLUME => 765,
        RT_TRADE_VOLUME => 766,
        SHORT_TERM_VOLUME => 773,
        other => other,
    }
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
///; 3 when the tick is not known.
fn tick_decimals(tick: f64) -> usize {
    if !(tick > 0.0 && tick.is_finite()) {
        return 3;
    }
    format!("{tick}").split_once('.').map_or(0, |(_, f)| f.len())
}

/// A price as the market rule writes it (`min_decimals` to eight
/// decimals, half even; 0 is "0"; captured 05/10/2026: AAPL "334.50").
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

/// The dividends of a 456 block, as the reference writes them:
/// "past 12 months,next 12 months,next date,next amount", each amount
/// with two to six decimals (`dividend_amount`),
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

/// A dividend amount as the reference writes it: the value to ten
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

/// The quote entries of a block without a length, as the top-of-book
/// messages write them: per entry one
/// byte (type in 5 bits, "one more follows", width - 1 in 2 bits; type 31:
/// the type and the width in the next two bytes), then the value, its top
/// bit the sign. (type, value) of each, and the bytes read; None when cut
/// short.
pub fn entry_list(data: &[u8]) -> Option<(Vec<(u64, i64)>, usize)> {
    let mut out = Vec::new();
    let mut at = 0;
    loop {
        let head = *data.get(at)?;
        at += 1;
        let (mut tick_type, more, mut width) = ((head >> 3) as u64, head & 0b100 != 0, (head & 0b11) as usize + 1);
        if tick_type == 31 {
            tick_type = *data.get(at)? as u64;
            width = *data.get(at + 1)? as usize;
            at += 2;
        }
        if width == 0 || width > 8 {
            return None;
        }
        let bytes = data.get(at..at + width)?;
        at += width;
        let negative = bytes[0] & 0x80 != 0;
        let magnitude = bytes.iter().enumerate()
            .fold(0i64, |v, (i, &b)| (v << 8) | (if i == 0 { b & 0x7f } else { b }) as i64);
        out.push((tick_type, if negative { -magnitude } else { magnitude }));
        if !more {
            return Some((out, at));
        }
    }
}

/// Java's `String.hashCode`.
fn java_hash(s: &str) -> u32 {
    s.encode_utf16().fold(0u32, |h, c| h.wrapping_mul(31).wrapping_add(c as u32))
}

/// The fundamental ratios as tick 47 gives them: the `KEY=VALUE` pairs
/// of the text joined with `;` in the order of a hash map of their keys (bucket
/// of the key's hash in a table that starts at 16 and doubles while more
/// than three quarters full; keys of one bucket in the order they came),
/// without the contract id (`IBCONID`); the dividend per share
/// (`TTMDIVSHR`) keeps its place with nothing written, so two `;` follow
/// each other there. `NOTAVAIL=1`, or no pair, gives the empty text.
/// Checked on the block and the callback captured on 07/10/2026 (AAPL: 66
/// pairs in, 64 out and the empty place, the same text). The reference keeps the keys it knows
///: every other key is taken as known.
pub fn ratios_text(text: &str) -> String {
    let mut pairs: Vec<(&str, &str)> = Vec::new();
    for part in text.split(';') {
        let Some((k, v)) = part.split_once('=') else { continue };
        if k == "NOTAVAIL" {
            return String::new();
        }
        if k == "IBCONID" {
            continue;
        }
        match pairs.iter_mut().find(|(key, _)| *key == k) {
            Some(p) => p.1 = v,
            None => pairs.push((k, v)),
        }
    }
    let mut capacity = 16usize;
    while pairs.len() * 4 > capacity * 3 {
        capacity *= 2;
    }
    let bucket = |k: &str| {
        let h = java_hash(k);
        ((h ^ (h >> 16)) as usize) & (capacity - 1)
    };
    pairs.sort_by_key(|(k, _)| bucket(k));
    pairs.iter().map(|(k, v)| if *k == "TTMDIVSHR" { String::new() } else { format!("{k}={v}") })
        .collect::<Vec<_>>().join(";")
}

/// The blocks of a generic tick message body (after its 2-byte bit count): (server tag,
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
            // No length: a list of quote entries, as long as it reads.
            0 => match entry_list(&rest[4..]) {
                Some((_, read)) => (read, 4),
                None => break,
            },
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

    const CTX: DecodeCtx = DecodeCtx { min_tick: 0.01, now_ms: 1791219886917, round_lot: 40, entry_min_tick: 0.001, rt_in_shares: false };

    // AAPL, 07/10/2026 (b4_generic_rest, frames 37, 333 and 258): the mark
    // price, the slow mark price (a block without a length, price tick
    // 0.001 from its own acknowledgement) and the short-term volumes, as
    // the reference gave them (ticks 37, 79, 63 to 65).
    #[test]
    fn mark_prices_and_short_term_volume() {
        let mut rec = GenericRecord::default();
        assert_eq!(decode(PL_PRICE, &hex("4074efb08000000000000001"), &mut rec, CTX), vec![GenTick::Price(37, 334.98059082)]);
        assert!(decode(PL_PRICE, &hex("4074efb08000000000000001"), &mut rec, CTX).is_empty(), "no change");
        assert!(decode(PL_PRICE, &hex("4074efb08000000000000000"), &mut GenericRecord::default(), CTX).is_empty(), "not valid");

        let block = hex("0000685616051c2a34006c00a400a800");
        assert_eq!(entry_list(&block[4..]), Some((vec![(2, 334890), (6, 0), (13, 0), (20, 0), (21, 0)], 12)));
        let body = [&[0x00, 0x80][..], &block[..]].concat();
        let blocks = blocks(&body, |tag| (tag == 0x6856).then_some(SLOW_MARK_PRICE));
        assert_eq!(blocks.len(), 1);
        assert_eq!(decode(SLOW_MARK_PRICE, blocks[0].2, &mut rec, CTX), vec![GenTick::Price(79, 334.89)]);

        let volumes = hex("00000004000000006ac6133400000003000000230000000500000 04c0000000a000000c100000000".replace(' ', "").as_str());
        assert_eq!(decode(SHORT_TERM_VOLUME, &volumes, &mut rec, CTX),
            vec![GenTick::Size(63, 1400.0), GenTick::Size(64, 3040.0), GenTick::Size(65, 7720.0)]);
        assert!(decode(SHORT_TERM_VOLUME, &volumes, &mut rec, CTX).is_empty());
    }

    // ibx#495: the index future premium is one double, tick 31 as it
    // changes, for an index only (the reference 10.51 on SPX, 12/10/2026:
    // tickGeneric 31 0.0004214518350973517).
    // ibx#604: the average volume of the 10.51 form, in shares under 768,
    // given in round lots rounded down (AAPL, a lot of 40, 19/10/2026:
    // 44451884 gave 1111297); the 10.40 form gives none from it; 207 is
    // given as it comes.
    #[test]
    fn average_volume_of_the_statistics_block() {
        let ctx = |shares| DecodeCtx { min_tick: 0.01, now_ms: 0, round_lot: 40, entry_min_tick: 0.01, rt_in_shares: shares };
        let shares = hex("000000010000030002a6482c00000000");
        assert_eq!(decode(MISC_STATS, &shares, &mut GenericRecord::default(), ctx(true)), [GenTick::Size(21, 1111297.0)]);
        assert!(decode(MISC_STATS, &shares, &mut GenericRecord::default(), ctx(false)).is_empty());
        let lots = hex("00000001000000cf00118d2a00000000");
        assert_eq!(decode(MISC_STATS, &lots, &mut GenericRecord::default(), ctx(true)), [GenTick::Size(21, 1150250.0)]);
    }

    #[test]
    fn etf_net_asset_values() {
        // ibx#641: the blocks of 13/10/2026 (SPY): 577 then 623.
        let mut rec = GenericRecord::default();
        let ctx = DecodeCtx { min_tick: 0.01, now_ms: 0, round_lot: 1, entry_min_tick: 0.01, rt_in_shares: true };
        assert_eq!(decode(ETF_NAV_LAST, &hex("40884ef460000000"), &mut rec, ctx), [GenTick::Price(96, 777.86932373)]);
        assert!(decode(ETF_FROZEN_NAV_LAST, &hex("bff0000000000000"), &mut rec, ctx).is_empty(), "-1 is no value");
        assert!(decode(ETF_NAV_LAST, &hex("40884ef460000000"), &mut rec, ctx).is_empty(), "unchanged");
        assert!(decode(ETF_NAV_LAST, &hex("0000000000000000"), &mut rec, ctx).is_empty(), "0 is no value (ibx#604)");
        assert!(valid_for(ETF_NAV_LAST, "STK") && !valid_for(ETF_NAV_LAST, "OPT") && etf_only(ETF_FROZEN_NAV_LAST));
        assert!(is_etf_type("ETF") && is_etf_type("ETMF") && !is_etf_type("COMMON"));
    }

    #[test]
    fn index_future_premium() {
        let mut rec = GenericRecord::default();
        let d = 0.0004214518350973517_f64;
        assert_eq!(decode(INDEX_FUTURE_PREMIUM, &d.to_be_bytes(), &mut rec, CTX), vec![GenTick::Generic(31, d)]);
        assert!(decode(INDEX_FUTURE_PREMIUM, &d.to_be_bytes(), &mut rec, CTX).is_empty(), "no change");
        assert!(sent(INDEX_FUTURE_PREMIUM) && at_once(INDEX_FUTURE_PREMIUM));
        assert!(valid_for(INDEX_FUTURE_PREMIUM, "IND") && !valid_for(INDEX_FUTURE_PREMIUM, "STK"));
    }

    // The ratios in the order of the reference's map; the contract id and
    // the dividend per share are left out; "not available" gives nothing
    // (SPY, 07/10/2026, b4_generic_rest2 frame 167).
    #[test]
    fn fundamental_ratios_text() {
        assert_eq!(ratios_text("IBCONID=265598;LATESTADATE=2026-06-27;CURRENCY=USD;MKTCAP=4869057;TTMDIVSHR=1.05;NLOW=243.42"),
            "CURRENCY=USD;MKTCAP=4869057;NLOW=243.42;;LATESTADATE=2026-06-27");
        assert_eq!(ratios_text("NOTAVAIL=1"), "");
        let mut rec = GenericRecord::default();
        let spy = hex("000000160000000a789cf3f30f710c73f4f4b1350400104002cd0000");
        assert!(decode(FUNDAMENTALS, &spy, &mut rec, CTX).is_empty());
    }

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
        // The real-time volume of this 10.40 recording came in its lot form
        // (the share form is #642's test).
        let lots = |code, h: &str, rec: &mut GenericRecord| decode(code, &hex(h), rec, CTX);
        assert!(lots(233, "4196af335ee3acb3000000000004589c000672c400000000", &mut rec).is_empty());
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
        assert_eq!(lots(233, "4196af870119def300000000000458ac000672ce00000000", &mut rec),
            vec![GenTick::Text(48, "334.53;16.0000000000000000;1791219886917;284844.0000000000000000;334.04434805;false".into())]);
        assert_eq!(rule_price_text(334.5, tick_decimals(0.01)), "334.50");
        assert_eq!(d(375, "41910c599b4f1068000000000003442a0001649200000000", &mut rec),
            vec![GenTick::Text(77, "334.54;13.0000000000000000;1791219886917;214058.0000000000000000;334.04736486;false".into())]);
        assert_eq!(d(236, "000000030b566f1e", &mut rec), vec![GenTick::Size(89, 190213918.0)]);
        // Frames 9160 and 9213: one trade; a count change with no volume.
        assert_eq!(lots(233, "4196af8c3b48ec3a00000000000458ad000672cf00000000", &mut rec),
            vec![GenTick::Text(48, "334.55;1.0000000000000000;1791219886917;284845.0000000000000000;334.04434981;true".into())]);
        assert!(lots(233, "4196af9175813e2500000000000458ae000672d200000000", &mut rec).len() == 1);
        assert!(lots(233, "4196af96afb29db400000000000458af000672d500000000", &mut rec).len() == 1);
        assert_eq!(lots(233, "4196af96afb29db400000000000458af000672d700000000", &mut rec),
            vec![GenTick::Text(48, ";0E-16;1791219886917;284847.0000000000000000;334.04435337;false".into())]);
    }

    // ibx#642: the real-time volume in its share form (asked as 765), two
    // blocks of MSFT (round lot 40) of the reference 10.51's run of
    // 13/10/2026, regular hours: one share traded, so the size is 0 lots
    // and the price is that share's, as the reference gave it
    // ('549.66;0E-16;...;151145...;548.54166701;false').
    #[test]
    fn share_forms_of_14_october() {
        // ibx#641, the reference 10.51 on 14/10/2026: MSFT 766 blocks (round
        // lot 40) and AAPL's 773 block, as the reference gave them.
        let ctx = DecodeCtx { min_tick: 0.01, now_ms: 1791987034821, round_lot: 40, entry_min_tick: 0.01, rt_in_shares: true };
        let mut rec = GenericRecord::default();
        assert!(decode(RT_TRADE_VOLUME, &hex("41d3d6f13077d3ed00000000002565d200005d5100000000"), &mut rec, ctx).is_empty());
        assert_eq!(decode(RT_TRADE_VOLUME, &hex("41d3d89e9508c9b0000000000025690000005d5f00000000"), &mut rec, ctx),
            [GenTick::Text(77, "540.17;20.0000000000000000;1791987034821;61292.0000000000000000;543.23424127;false".into())]);
        let ctx = DecodeCtx { round_lot: 100, ..ctx };
        assert_eq!(decode(SHORT_TERM_VOLUME, &hex("00000004000000006acf8d380000000300056130000000050008a2000000000a000f2dc800000000"), &mut GenericRecord::default(), ctx),
            [GenTick::Size(63, 352560.0), GenTick::Size(64, 565760.0), GenTick::Size(65, 994760.0)]);
    }

    #[test]
    fn rt_volume_in_shares() {
        let mut rec = GenericRecord::default();
        let ctx = DecodeCtx { min_tick: 0.01, now_ms: 1791900393444, round_lot: 40, entry_min_tick: 0.01, rt_in_shares: true };
        assert!(decode(RT_VOLUME, &hex("41e8b57e9b1c8ac300000000005c407600024a1600000000"), &mut rec, ctx).is_empty());
        assert_eq!(decode(RT_VOLUME, &hex("41e8b57edfd1930500000000005c407700024a1a00000000"), &mut rec, ctx),
            vec![GenTick::Text(48, "549.66;0E-16;1791900393444;151145.0000000000000000;548.54166701;false".into())]);
        // With a lot above 1, a block with no change gives a string too.
        assert_eq!(decode(RT_VOLUME, &hex("41e8b57edfd1930500000000005c407700024a1a00000000"), &mut rec, ctx).len(), 1);
        assert_eq!((wire_code(RT_VOLUME), wire_code(RT_TRADE_VOLUME), wire_code(SHORT_TERM_VOLUME)), (765, 766, 773));
    }

    #[test]
    fn futures_open_interest() {
        let mut rec = GenericRecord::default();
        assert_eq!(decode(588, &hex("00020134"), &mut rec, CTX), vec![GenTick::Size(86, 131380.0)]);
    }

    // ibx#616, the blocks of the reference 10.51's odd-lot entry (paper,
    // 12/10/2026, MSFT in regular hours, run r1 of `.tmp/632`, price tick
    // 0.01): the first block unset gives nothing; then the ask 538.86 x 23
    // on the exchange set 512; then 538.93 x 3 on 64; then an unset ask,
    // nothing. The client got 106, 108, 110 twice.
    #[test]
    fn odd_lot_quotes_of_the_recording() {
        let ctx = DecodeCtx { entry_min_tick: 0.01, ..CTX };
        let mut rec = GenericRecord::default();
        let mut ticks = |h: &str| decode(ODD_LOT, &hex(h), &mut rec, ctx);
        assert!(ticks("04e424000ce42c003c0c84008c005800").is_empty());
        assert_eq!(ticks("0e00d27e2c17890200"),
            [GenTick::Price(106, 538.86), GenTick::Size(108, 23.0), GenTick::Text(110, "512".into())]);
        assert_eq!(ticks("0e00d2852c038840"),
            [GenTick::Price(106, 538.93), GenTick::Size(108, 3.0), GenTick::Text(110, "64".into())]);
        assert!(ticks("0ce42c008800").is_empty());
        // 12/11/2026, MSFT in regular hours: a bid of 37 and an ask side
        // with no quote (price 0, size 0, no exchange): nothing for the ask.
        let bid_only = ticks("0600cbbf24250c002c008507008800");
        assert!(matches!(&bid_only[..], [GenTick::Price(105, p), GenTick::Size(107, s), GenTick::Text(109, _)]
            if (*p - 521.59).abs() < 1e-9 && *s == 37.0), "{bid_only:?}");
        assert_eq!(length_width(ODD_LOT), 0, "a list of quote entries");
        assert!(at_once(ODD_LOT) && sent(ODD_LOT) && valid_for(ODD_LOT, "STK"));
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
