//! When outside regular trading hours is kept on an order (ibx#465).
//!
//! The reference drops the outside-RTH flag of an order it does not apply
//! to: the new order gets warning 2109 and no outside-RTH tag, and a replace
//! has no outside-RTH tag either. Whether it applies depends on the order
//! type, the time in force, the exchange, and two values the server gives
//! with the contract definition for the order's exchange: the market type
//! (6523) and the order-type token list (6431). Rule read from the
//! reference in ib-agent#199, checked there on 10 captured orders.

use crate::types::{OrderKind, OrderRequest};

/// Text of warning 2109 when outside-RTH is dropped from a new order.
pub(crate) const OUTSIDE_RTH_IGNORED: &str = "Order Event Warning:Attribute 'Outside Regular Trading Hours' \
    is ignored based on the order type and destination. PlaceOrder is now being processed.";

/// What the rule needs from the contract definition of one exchange. The
/// empty value (no definition found) keeps outside-RTH on no order, as the
/// reference with an empty list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RthTypes {
    /// Market type, tag 6523 (for example USSTK).
    pub market_type: String,
    /// Security type of the contract (STK, OPT, ...).
    pub sec_type: String,
    pub currency: String,
    /// Order-type tokens, from tag 6431 `TOKEN/N` (N = 4 counts as absent).
    pub rth: bool,
    pub lth: bool,
    pub rth_only: bool,
    pub lth_only: bool,
    pub elh_only: bool,
    pub erh_only: bool,
    pub rth4mkt: bool,
}

impl RthTypes {
    /// From the definition's token list (tag 6431, `TOKEN/N` entries) and
    /// market type (6523).
    pub(crate) fn from_definition(tokens: &[String], market_type: &str, sec_type: &str, currency: &str) -> Self {
        let mut t = RthTypes {
            market_type: market_type.to_string(),
            sec_type: sec_type.to_string(),
            currency: currency.to_string(),
            ..Default::default()
        };
        for token in tokens {
            let (name, n) = token.split_once('/').unwrap_or((token.as_str(), ""));
            if n == "4" { continue; }
            match name {
                "RTH" => t.rth = true,
                "LTH" => t.lth = true,
                "RTHONLY" => t.rth_only = true,
                "LTHONLY" => t.lth_only = true,
                "ELHONLY" => t.elh_only = true,
                "ERHONLY" => t.erh_only = true,
                "RTH4MKT" => t.rth4mkt = true,
                _ => {}
            }
        }
        t
    }
}

/// The order-type classes the rule tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct RthKind {
    /// Market-like: MKT, MTL, STP, MIT, TRAIL, STP PRT, MIDPRICE, PEG MKT,
    /// PEG MID (TRAIL MIT and FUNARI are not order types of ibx).
    pub market_like: bool,
    /// Stop or touched: STP, STP LMT, STP PRT, TRAIL, TRAIL LIMIT, LIT, MIT.
    pub stop_or_touched: bool,
    /// MOC or LOC.
    pub moc_loc: bool,
}

impl RthKind {
    pub(crate) fn of(kind: &OrderKind) -> Self {
        let (market_like, stop_or_touched, moc_loc) = match kind {
            OrderKind::Market | OrderKind::Mtl | OrderKind::MidPrice { .. }
            | OrderKind::PegMkt { .. } | OrderKind::PegMid { .. } => (true, false, false),
            OrderKind::Stop { .. } | OrderKind::Mit { .. } | OrderKind::StpPrt { .. }
            | OrderKind::TrailingStop { .. } | OrderKind::TrailPct { .. }
            | OrderKind::AdjustableStop { .. } => (true, true, false),
            OrderKind::StopLimit { .. } | OrderKind::TrailingStopLimit { .. }
            | OrderKind::Lit { .. } => (false, true, false),
            OrderKind::Moc | OrderKind::Loc { .. } => (false, false, true),
            OrderKind::Limit { .. } | OrderKind::MktPrt | OrderKind::SnapMkt { .. } | OrderKind::SnapMid { .. }
            | OrderKind::SnapPri { .. } | OrderKind::Rel { .. } | OrderKind::PegBench { .. } => (false, false, false),
        };
        RthKind { market_like, stop_or_touched, moc_loc }
    }
}

/// True when outside-RTH stays on the order (ib-agent#199, conditions A and
/// B). `exchange` is the order's exchange as sent (BEST for SMART); `tif` the
/// time-in-force byte. ibx sends no combo, volatility, algo AccuDistr,
/// relative-discretionary or OMS container order, so those conditions pass.
pub(crate) fn outside_rth_applies(kind: RthKind, tif: u8, exchange: &str, types: &RthTypes) -> bool {
    // A4: a market-like type is regular-hours only on a US stock or
    // warrant, a USD option, or a contract with the RTH4MKT token.
    let forced_rth = kind.market_like
        && ((types.sec_type == "STK" && types.market_type == "USSTK")
            || (types.sec_type == "WAR" && types.market_type == "USWAR")
            || (matches!(types.sec_type.as_str(), "OPT" | "FOP" | "IOPT") && types.currency == "USD")
            || types.rth4mkt);
    if forced_rth { return false; }
    // A6: the exchange list allows outside RTH (RTH token and a time in
    // force it applies to, or LTH token and a stop or touched type).
    if exchange == "IBKRATS" { return false; }
    let tif_ok = !kind.moc_loc
        && !matches!(tif, b'?' | b'4' | b'3')
        && (tif != b'2' || exchange == "ARCA");
    if !((types.rth && tif_ok) || (types.lth && kind.stop_or_touched)) { return false; }
    // A9: not the overnight venues.
    if matches!(exchange, "OVERNIGHT" | "IBEOS") { return false; }
    // B: session-only lists.
    !(types.lth_only && kind.market_like)
        && !types.rth_only
        && !(types.elh_only && kind.market_like)
        && !types.erh_only
}

/// The parts of a request the rule reads: the instrument (None for a
/// replace, whose instrument is the order's), the type classes, the time in
/// force, and the outside-RTH flag. None for a request that never carries
/// outside-RTH.
pub(crate) fn rth_parts(req: &mut OrderRequest) -> Option<(Option<u32>, RthKind, u8, Option<OrderKind>, &mut bool)> {
    use OrderRequest as R;
    let limit = RthKind::of(&OrderKind::Limit { price: 0 });
    match req {
        R::SubmitLimitGtc { instrument, outside_rth, .. } => Some((Some(*instrument), limit, b'1', None, outside_rth)),
        R::SubmitStopGtc { instrument, outside_rth, .. } =>
            Some((Some(*instrument), RthKind::of(&OrderKind::Stop { stop_price: 0 }), b'1', None, outside_rth)),
        R::SubmitStopLimitGtc { instrument, outside_rth, .. } =>
            Some((Some(*instrument), RthKind::of(&OrderKind::StopLimit { price: 0, stop_price: 0 }), b'1', None, outside_rth)),
        R::SubmitTrailingStopPctEx { instrument, tif, attrs, .. } =>
            Some((Some(*instrument), RthKind::of(&OrderKind::TrailPct { trail_pct: 0, trail_stop_price: 0 }), *tif, None, &mut attrs.outside_rth)),
        R::SubmitLimitEx { instrument, tif, attrs, .. }
        | R::SubmitAdaptive { instrument, tif, attrs, .. }
        | R::SubmitAlgo { instrument, tif, attrs, .. } => Some((Some(*instrument), limit, *tif, None, &mut attrs.outside_rth)),
        R::SubmitEx { instrument, kind, tif, attrs, .. } =>
            Some((Some(*instrument), RthKind::of(kind), *tif, Some(*kind), &mut attrs.outside_rth)),
        R::Modify { kind, tif, attrs, .. } => Some((None, RthKind::of(kind), *tif, Some(*kind), &mut attrs.outside_rth)),
        _ => None,
    }
}

/// The exchange an order goes to: the instrument's routing, except the
/// types that need a directed exchange (ibx#217). The snap types keep the
/// instrument's routing (ibx#413).
pub(crate) fn order_destination(kind: Option<&OrderKind>, routed: String) -> String {
    match kind {
        Some(OrderKind::MidPrice { .. } | OrderKind::PegMkt { .. } | OrderKind::PegMid { .. }) => "ISLAND".to_string(),
        _ => routed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aapl_best() -> RthTypes {
        // AAPL on BEST, paper 28/09/2026: RTH/1 only, 6523=USSTK.
        let tokens: Vec<String> = "ACTIVETIM/1,AD/5,ADJUST/1,RTH/1,AON/1".split(',').map(String::from).collect();
        RthTypes::from_definition(&tokens, "USSTK", "STK", "USD")
    }

    // ib-agent#199 section 7: the 10 captured orders (AAPL, outsideRth=true).
    #[test]
    fn captured_orders_on_a_us_stock() {
        let t = aapl_best();
        let k = |kind: OrderKind| RthKind::of(&kind);
        let cases = [
            ("STP DAY", k(OrderKind::Stop { stop_price: 1 }), b'0', "BEST", false),
            ("TRAIL amount DAY", k(OrderKind::TrailingStop { trail_amt: 1, trail_stop_price: 0 }), b'0', "BEST", false),
            ("TRAIL percent GTC", k(OrderKind::TrailPct { trail_pct: 1, trail_stop_price: 0 }), b'1', "BEST", false),
            ("STP LMT GTC", k(OrderKind::StopLimit { price: 1, stop_price: 1 }), b'1', "BEST", true),
            ("TRAIL LIMIT GTC", k(OrderKind::TrailingStopLimit { lmt_offset: 1, lmt_price: None, trail_amt: 1, trail_stop_price: 1 }), b'1', "BEST", true),
            ("MIT GTC", k(OrderKind::Mit { stop_price: 1 }), b'1', "BEST", false),
            ("LIT GTC", k(OrderKind::Lit { price: 1, stop_price: 1 }), b'1', "BEST", true),
            ("MKT DAY", k(OrderKind::Market), b'0', "BEST", false),
            ("LMT FOK", k(OrderKind::Limit { price: 1 }), b'4', "BEST", false),
            ("LMT IOC", k(OrderKind::Limit { price: 1 }), b'3', "BEST", false),
            ("LMT OPG", k(OrderKind::Limit { price: 1 }), b'2', "BEST", false),
            ("REL DAY", k(OrderKind::Rel { offset: 1 }), b'0', "BEST", true),
            ("LMT DAY", k(OrderKind::Limit { price: 1 }), b'0', "BEST", true),
            ("LMT GTC", k(OrderKind::Limit { price: 1 }), b'1', "BEST", true),
        ];
        for (label, kind, tif, exch, keep) in cases {
            assert_eq!(outside_rth_applies(kind, tif, exch, &t), keep, "{label}");
        }
    }

    // Parts of the rule not captured on paper, from the code read.
    #[test]
    fn rule_from_the_code_read() {
        let t = aapl_best();
        let lmt = RthKind::of(&OrderKind::Limit { price: 1 });
        // OPG is kept only on ARCA.
        assert!(outside_rth_applies(lmt, b'2', "ARCA", &t));
        // No list found: nothing keeps outside-RTH.
        assert!(!outside_rth_applies(lmt, b'0', "BEST", &RthTypes::default()));
        // Overnight venues and IBKRATS drop it.
        assert!(!outside_rth_applies(lmt, b'0', "OVERNIGHT", &t));
        assert!(!outside_rth_applies(lmt, b'0', "IBKRATS", &t));
        // MOC / LOC.
        assert!(!outside_rth_applies(RthKind::of(&OrderKind::Loc { price: 1 }), b'0', "BEST", &t));
        // A market-like type on a stock that is not a US stock keeps it
        // unless the list has RTH4MKT.
        let de = RthTypes::from_definition(&["RTH/1".to_string()], "DESTK", "STK", "EUR");
        let stp = RthKind::of(&OrderKind::Stop { stop_price: 1 });
        assert!(outside_rth_applies(stp, b'0', "BEST", &de));
        let de4 = RthTypes::from_definition(&["RTH/1".to_string(), "RTH4MKT/1".to_string()], "DESTK", "STK", "EUR");
        assert!(!outside_rth_applies(stp, b'0', "BEST", &de4));
        // LTH lets a stop or touched type through without RTH.
        let lth = RthTypes::from_definition(&["LTH/1".to_string()], "DESTK", "STK", "EUR");
        assert!(outside_rth_applies(stp, b'0', "BEST", &lth));
        assert!(!outside_rth_applies(lmt, b'0', "BEST", &lth));
        // A token with number 4 counts as absent; RTHONLY drops it.
        assert!(!outside_rth_applies(lmt, b'0', "BEST", &RthTypes::from_definition(&["RTH/4".to_string()], "USSTK", "STK", "USD")));
        assert!(!outside_rth_applies(lmt, b'0', "BEST", &RthTypes::from_definition(&["RTH/1".to_string(), "RTHONLY/1".to_string()], "USSTK", "STK", "USD")));
    }
}
