//! Option calculations (calculateImpliedVolatility / calculateOptionPrice).
//!
//! The reference computes both answers locally with its own option model
//! and sends one custom option computation tick (type 53). The model inputs
//! are the option terms from its definition, the interest-rate curve of the
//! option currency and the dividends of the underlying, both asked from the
//! server as reference data. See `model` for the formulas and `inputs` for
//! the market inputs.

pub mod inputs;
pub mod model;

pub use model::{Right, Style};

/// Tick type of the answer: custom option computation.
pub const TICK_CUST_OPTION_COMPUTATION: i32 = 53;
/// An implied volatility not found within this time gets no answer.
pub const IV_WINDOW_MS: u64 = 5_000;
/// A price not computed within this time is answered without a price.
pub const PRICE_TIMEOUT_MS: u64 = 30_000;
/// Value of a field that is not computed (the clients read it as unset).
pub const NOT_COMPUTED: f64 = f64::MAX;

/// What a calculation asks for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CalcKind {
    /// Implied volatility of an option price.
    ImpliedVol { option_price: f64 },
    /// Price and greeks at a volatility.
    Price { volatility: f64 },
}

/// The terms of an option the model needs, from its definition.
#[derive(Debug, Clone, PartialEq)]
pub struct OptionTerms {
    pub con_id: i64,
    /// API security type (`OPT`, `FOP`, `IOPT`, ...).
    pub sec_type: String,
    pub currency: String,
    pub right: Option<Right>,
    pub style: Style,
    pub strike: f64,
    /// Last trading date, `YYYYMMDD`.
    pub last_trade_date: String,
    /// Last trading time `HHMM`, empty when unknown.
    pub last_trade_time: String,
    /// Time zone of the contract dates.
    pub time_zone: String,
    pub under_con_id: i64,
    /// API security type of the underlying, empty when unknown.
    pub under_sec_type: String,
}

impl OptionTerms {
    /// Option types the model prices (OPT, FOP, IOPT).
    pub fn is_option(&self) -> bool {
        matches!(self.sec_type.as_str(), "OPT" | "FOP" | "IOPT")
    }

    /// Reference-data query of the dividends of the underlying: special
    /// dividends are asked too, except for a currency pair.
    pub fn dividend_query(&self) -> String {
        if self.under_sec_type == "CASH" {
            format!("div {}", self.under_con_id)
        } else {
            format!("div incSpecial {}", self.under_con_id)
        }
    }

    /// Reference-data query of the interest-rate curve of the currency.
    pub fn rate_query(&self) -> String {
        format!("div {}", self.currency)
    }
}

/// One answer: the fields of the option computation tick. `NOT_COMPUTED`
/// marks a field without a value.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OptionComputation {
    pub req_id: i64,
    pub tick_type: i32,
    pub tick_attrib: i32,
    pub implied_vol: f64,
    pub delta: f64,
    pub opt_price: f64,
    pub pv_dividend: f64,
    pub gamma: f64,
    pub vega: f64,
    pub theta: f64,
    pub und_price: f64,
}

fn value_or_unset(v: f64) -> f64 {
    if v.is_nan() || v == f64::MAX { NOT_COMPUTED } else { v }
}

/// A volatility the model accepts: finite and not the unset value.
pub fn volatility_is_valid(v: f64) -> bool {
    v.is_finite() && v != f64::MAX
}

/// A price the model reports: finite and not the unset value.
pub fn price_is_valid(v: f64) -> bool {
    v.is_finite() && v != f64::MAX
}

/// Market inputs ready for one calculation.
pub struct ModelInputs<'a> {
    pub curve: &'a inputs::RateCurve,
    pub dividends: &'a [inputs::Dividend],
    /// Model clock in milliseconds.
    pub clock_ms: i64,
    /// Time zone whose end of day closes an ex-date.
    pub local_zone: &'a str,
}

/// The model of an option at the model clock, or `None` when the terms do
/// not give one (no right or no expiry).
pub fn build_model(terms: &OptionTerms, under_price: f64, inp: &ModelInputs) -> Option<model::OptionModel> {
    let right = terms.right?;
    let zone = if terms.time_zone.is_empty() { inputs::MODEL_TZ } else { terms.time_zone.as_str() };
    let end = inputs::expiry_end(&terms.last_trade_date, &terms.last_trade_time, None, zone)?;
    let t = inputs::time_to_expiry(inp.clock_ms, end);
    let r = inp.curve.model_rate(t);
    let today = inputs::date_of_ms(inp.clock_ms, inputs::MODEL_TZ);
    let last_day = inputs::parse_yyyymmdd(terms.last_trade_date.get(0..8)?)?;
    let pv = inputs::pv_dividends(inp.dividends, today, last_day, r);
    Some(model::OptionModel {
        right,
        style: terms.style,
        s: under_price,
        strike: terms.strike,
        t,
        pv_dividend: pv,
        q: 0.0,
        r,
        dividends: inputs::tree_dividends(inp.dividends, inp.clock_ms, inp.local_zone),
        index: terms.under_sec_type == "IND",
    })
}

/// Answer of a price calculation once the inputs are ready; `None` while
/// the model cannot price (volatility not valid or no underlying price).
pub fn price_answer(
    req_id: i64, terms: &OptionTerms, volatility: f64, under_price: f64, inp: &ModelInputs,
) -> Option<OptionComputation> {
    if !volatility_is_valid(volatility) || under_price.is_nan() {
        return None;
    }
    let m = build_model(terms, under_price, inp)?;
    let (price, g) = m.price_with_greeks(volatility);
    if !price_is_valid(price) {
        return None;
    }
    Some(OptionComputation {
        req_id,
        tick_type: TICK_CUST_OPTION_COMPUTATION,
        tick_attrib: 0,
        implied_vol: volatility,
        delta: value_or_unset(g.delta),
        opt_price: price,
        pv_dividend: NOT_COMPUTED,
        gamma: value_or_unset(g.gamma),
        vega: value_or_unset(g.vega),
        theta: value_or_unset(g.theta),
        und_price: value_or_unset(under_price),
    })
}

/// Answer of a price calculation that ran out of time: no price, no greeks.
pub fn price_timeout_answer(req_id: i64, volatility: f64, under_price: f64) -> OptionComputation {
    OptionComputation {
        req_id,
        tick_type: TICK_CUST_OPTION_COMPUTATION,
        tick_attrib: 0,
        implied_vol: if volatility_is_valid(volatility) { volatility } else { NOT_COMPUTED },
        delta: NOT_COMPUTED,
        opt_price: NOT_COMPUTED,
        pv_dividend: NOT_COMPUTED,
        gamma: NOT_COMPUTED,
        vega: NOT_COMPUTED,
        theta: NOT_COMPUTED,
        und_price: value_or_unset(under_price),
    }
}

/// Answer of an implied volatility calculation, `None` when the solver
/// finds no volatility.
pub fn implied_vol_answer(
    req_id: i64, terms: &OptionTerms, option_price: f64, under_price: f64, inp: &ModelInputs,
) -> Option<OptionComputation> {
    let m = build_model(terms, under_price, inp)?;
    let iv = m.implied_vol(option_price, model::IV_GUESS)?;
    if !iv.is_finite() {
        return None;
    }
    Some(OptionComputation {
        req_id,
        tick_type: TICK_CUST_OPTION_COMPUTATION,
        tick_attrib: 0,
        implied_vol: iv,
        delta: NOT_COMPUTED,
        opt_price: value_or_unset(option_price),
        pv_dividend: NOT_COMPUTED,
        gamma: NOT_COMPUTED,
        vega: NOT_COMPUTED,
        theta: NOT_COMPUTED,
        und_price: value_or_unset(under_price),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aapl_call() -> OptionTerms {
        OptionTerms {
            con_id: 848313396, sec_type: "OPT".into(), currency: "USD".into(), right: Some(Right::Call),
            style: Style::American, strike: 255.0, last_trade_date: "20260306".into(),
            last_trade_time: String::new(), time_zone: String::new(), under_con_id: 265598,
            under_sec_type: "STK".into(),
        }
    }

    fn capture_clock() -> i64 {
        jiff::civil::date(2026, 3, 4).at(9, 17, 36, 422_000_000)
            .to_zoned(jiff::tz::TimeZone::UTC).unwrap().timestamp().as_millisecond()
    }

    /// A flat curve at the rate that reproduces capture #40.
    fn capture_curve() -> inputs::RateCurve {
        inputs::RateCurve { points: vec![(0.0, 0.03949488153593471)] }
    }

    #[test]
    fn queries_follow_the_reference_forms() {
        let t = aapl_call();
        assert_eq!(t.rate_query(), "div USD");
        assert_eq!(t.dividend_query(), "div incSpecial 265598");
        let fx = OptionTerms { under_sec_type: "CASH".into(), under_con_id: 12087792, ..aapl_call() };
        assert_eq!(fx.dividend_query(), "div 12087792");
    }

    #[test]
    fn capture_40_answers() {
        let curve = capture_curve();
        let inp = ModelInputs { curve: &curve, dividends: &[], clock_ms: capture_clock(), local_zone: "Europe/Paris" };
        let p = price_answer(9002, &aapl_call(), 0.3, 250.0, &inp).unwrap();
        assert_eq!((p.tick_type, p.tick_attrib, p.implied_vol, p.pv_dividend, p.und_price), (53, 0, 0.3, NOT_COMPUTED, 250.0));
        assert!((p.opt_price - 0.7612065964530287).abs() < 1e-8, "{}", p.opt_price);
        assert!((p.delta - 0.21766418914368796).abs() < 1e-8, "{}", p.delta);
        assert!((p.gamma - 0.04785750216838694).abs() < 1e-8);
        assert!((p.vega - 0.06577240653056571).abs() < 1e-8);
        assert!((p.theta - -0.3745716294945733).abs() < 1e-7);
        let iv = implied_vol_answer(9001, &aapl_call(), 5.0, 250.0, &inp).unwrap();
        assert!((iv.implied_vol - 0.8640485765653304).abs() < 1e-8, "{}", iv.implied_vol);
        assert_eq!((iv.delta, iv.opt_price, iv.pv_dividend, iv.gamma, iv.vega, iv.theta, iv.und_price),
            (NOT_COMPUTED, 5.0, NOT_COMPUTED, NOT_COMPUTED, NOT_COMPUTED, NOT_COMPUTED, 250.0));
    }

    #[test]
    fn invalid_volatility_waits_then_times_out_without_price() {
        let curve = capture_curve();
        let inp = ModelInputs { curve: &curve, dividends: &[], clock_ms: capture_clock(), local_zone: "UTC" };
        assert!(price_answer(1, &aapl_call(), f64::NAN, 250.0, &inp).is_none());
        assert!(price_answer(1, &aapl_call(), f64::MAX, 250.0, &inp).is_none());
        assert!(price_answer(1, &aapl_call(), 0.3, f64::NAN, &inp).is_none());
        let t = price_timeout_answer(1, f64::NAN, 250.0);
        assert_eq!((t.implied_vol, t.delta, t.opt_price, t.und_price), (NOT_COMPUTED, NOT_COMPUTED, NOT_COMPUTED, 250.0));
        let t = price_timeout_answer(1, 0.3, 250.0);
        assert_eq!(t.implied_vol, 0.3);
    }

    #[test]
    fn no_implied_volatility_below_intrinsic() {
        let curve = capture_curve();
        let inp = ModelInputs { curve: &curve, dividends: &[], clock_ms: capture_clock(), local_zone: "UTC" };
        let deep = OptionTerms { strike: 200.0, ..aapl_call() };
        assert!(implied_vol_answer(1, &deep, 1.0, 250.0, &inp).is_none());
    }
}
