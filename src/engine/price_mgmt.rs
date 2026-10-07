//! When an order carries the price management flag (ibx#492).
//!
//! The reference writes the flag, on a new order and on a replace, only
//! when all of these hold: the order's value is on (the client's value,
//! or for an unset value the order preset of the security type); the
//! order type is not market, stop, market-if-touched or trailing stop;
//! the contract's order-type list on the order's exchange has the price
//! check key; the session allows price management; the session's
//! exclusion list does not exclude the security type, for every exchange
//! or for the order's exchange. It never writes the flag off. A combo
//! order carries no flag (seen with the value on and unset).

use std::collections::HashMap;

use crate::types::{InstrumentId, OrderKind, OrderRequest};

/// Order types the flag never goes with: market, stop, market-if-touched,
/// trailing stop (amount or percent) and trailing market-if-touched. An
/// adjustable stop is a stop until it converts.
pub(crate) fn excluded_kind(kind: &OrderKind) -> bool {
    matches!(kind,
        OrderKind::Market | OrderKind::Stop { .. } | OrderKind::Mit { .. }
        | OrderKind::TrailingStop { .. } | OrderKind::TrailPct { .. }
        | OrderKind::TrailMit { .. } | OrderKind::AdjustableStop { .. })
}

/// The same rule read on a new order as written: its order type and
/// instruction, whose first letter is the trailing one (ibx#263).
pub(crate) fn excluded_frame(fields: &[(u32, &str)]) -> bool {
    let get = |tag: u32| fields.iter().find(|&&(t, _)| t == tag).map(|&(_, v)| v);
    match get(40) {
        Some("1" | "3" | "J" | "TMIT") => true,
        Some("P") => get(18).is_some_and(|v| v.split(' ').next() == Some("a")),
        _ => false,
    }
}

/// The value for a security type when the client leaves it unset: the
/// order preset of that type. The preset is a stored user setting with no
/// default in the reference's code; it was seen on for stocks on the test
/// paper account. Other types are not known and stay off.
pub(crate) fn preset(sec_type: &str) -> bool {
    sec_type == "STK"
}

/// The parts of a request the rule reads: its instrument (None for a
/// replace, whose instrument is the order's) and the client's value
/// (None when unset). None for a request no order of which can carry the
/// flag.
pub(crate) fn parts(req: &OrderRequest) -> Option<(Option<InstrumentId>, Option<bool>)> {
    use OrderRequest as R;
    match req {
        R::SubmitLimit { instrument, .. }
        | R::SubmitStopLimit { instrument, .. }
        | R::SubmitLimitGtc { instrument, .. }
        | R::SubmitStopLimitGtc { instrument, .. }
        | R::SubmitLimitIoc { instrument, .. }
        | R::SubmitLimitFok { instrument, .. }
        | R::SubmitTrailingStopLimit { instrument, .. }
        | R::SubmitMoc { instrument, .. }
        | R::SubmitLoc { instrument, .. }
        | R::SubmitLit { instrument, .. }
        | R::SubmitBracket { instrument, .. }
        | R::SubmitRel { instrument, .. }
        | R::SubmitLimitOpg { instrument, .. }
        | R::SubmitMtl { instrument, .. }
        | R::SubmitMktPrt { instrument, .. }
        | R::SubmitStpPrt { instrument, .. }
        | R::SubmitMidPrice { instrument, .. }
        | R::SubmitSnapMkt { instrument, .. }
        | R::SubmitSnapMid { instrument, .. }
        | R::SubmitSnapPri { instrument, .. }
        | R::SubmitPegMkt { instrument, .. }
        | R::SubmitPegMid { instrument, .. }
        | R::SubmitPegBench { instrument, .. }
        | R::SubmitLimitAuc { instrument, .. }
        | R::SubmitMtlAuc { instrument, .. }
        | R::SubmitLimitFractional { instrument, .. } => Some((Some(*instrument), None)),
        R::SubmitLimitEx { instrument, attrs, .. }
        | R::SubmitAdaptive { instrument, attrs, .. }
        | R::SubmitAlgo { instrument, attrs, .. } => Some((Some(*instrument), attrs.use_price_mgmt_algo)),
        R::SubmitEx { instrument, kind, attrs, .. } if !excluded_kind(kind) =>
            Some((Some(*instrument), attrs.use_price_mgmt_algo)),
        R::Modify { kind, attrs, .. } if !excluded_kind(kind) => Some((None, attrs.use_price_mgmt_algo)),
        _ => None,
    }
}

/// The exclusion list of the logon, `exchange/secType` items separated by
/// `;`; an item of another form is left out, as the reference.
pub(crate) fn parse_exclusions(text: &str) -> HashMap<String, Vec<String>> {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    for item in text.split(';').filter(|i| !i.is_empty()) {
        let parts: Vec<&str> = item.split('/').collect();
        if let [exchange, sec_type] = parts.as_slice() {
            map.entry(exchange.to_string()).or_default().push(sec_type.to_string());
        } else {
            log::warn!("Price management exclusion item not in exchange/secType form: {}", item);
        }
    }
    map
}

/// The exclusion list allows the security type on the order's exchange:
/// no list allows all, an empty list none; a type listed for every
/// exchange (`*`) is excluded; on SMART at least one of the contract's
/// exchanges must not list it (none known allows it); on another exchange
/// that exchange must not list it.
pub(crate) fn allowed(
    exclusions: Option<&HashMap<String, Vec<String>>>,
    exchange: &str,
    sec_type: &str,
    smart_exchanges: Option<&[String]>,
) -> bool {
    let Some(map) = exclusions else { return true };
    if map.is_empty() { return false; }
    let lists = |ex: &str| map.get(ex).is_some_and(|types| types.iter().any(|t| t == sec_type));
    if lists("*") { return false; }
    if matches!(exchange, "SMART" | "BEST") {
        return match smart_exchanges {
            None | Some([]) => true,
            Some(exchanges) => exchanges.iter().any(|e| !lists(e)),
        };
    }
    !lists(exchange)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The paper logon of 28/09/2026 (ib-agent captures/0928).
    const PAPER: &str = "*/CMDTY;*/CRYPTO;*/FUND;*/IOPT;*/SLB";

    #[test]
    fn the_paper_list_excludes_its_types_everywhere() {
        let map = parse_exclusions(PAPER);
        assert!(allowed(Some(&map), "BEST", "STK", None));
        assert!(allowed(Some(&map), "NYSE", "OPT", None));
        for t in ["CMDTY", "CRYPTO", "FUND", "IOPT", "SLB"] {
            assert!(!allowed(Some(&map), "BEST", t, None), "{t}");
            assert!(!allowed(Some(&map), "PAXOS", t, None), "{t}");
        }
    }

    #[test]
    fn no_list_allows_and_an_empty_list_blocks() {
        assert!(allowed(None, "BEST", "STK", None));
        let empty = parse_exclusions("");
        assert!(!allowed(Some(&empty), "BEST", "STK", None));
    }

    #[test]
    fn exchange_items_and_smart() {
        let map = parse_exclusions("ARCA/STK;bad;NYSE/STK");
        assert!(!allowed(Some(&map), "ARCA", "STK", None));
        assert!(allowed(Some(&map), "ISLAND", "STK", None));
        let all: Vec<String> = ["ARCA", "NYSE"].iter().map(|s| s.to_string()).collect();
        assert!(!allowed(Some(&map), "BEST", "STK", Some(&all)), "every exchange lists it");
        let some: Vec<String> = ["ARCA", "ISLAND"].iter().map(|s| s.to_string()).collect();
        assert!(allowed(Some(&map), "SMART", "STK", Some(&some)));
        assert!(allowed(Some(&map), "BEST", "STK", None));
    }

    // ib-agent ORDER-PRICEMGMT.md section 4.2 and the captures of
    // 28/09/2026 and 01/10/2026: MKT, STP, TRAIL MIT have no flag; LMT,
    // STP LMT, MOC, LOC, PEG BENCH, TRAIL LIT have it.
    #[test]
    fn excluded_order_types() {
        let frame = |t: &'static str, i: Option<&'static str>| {
            let mut f = vec![(40u32, t)];
            if let Some(i) = i { f.push((18, i)); }
            f
        };
        for (t, i) in [("1", None), ("3", None), ("J", None), ("P", Some("a")), ("TMIT", None)] {
            assert!(excluded_frame(&frame(t, i)), "{t}");
        }
        for (t, i) in [("2", None), ("4", None), ("5", None), ("B", None), ("PB", Some("R")), ("P", Some("R")), ("TSL", None), ("LT", None),
            ("TLIT", None), ("E2M", None), ("RPI", None), ("PSVR", None)] {
            assert!(!excluded_frame(&frame(t, i)), "{t}");
        }
        assert!(excluded_kind(&OrderKind::Market));
        assert!(excluded_kind(&OrderKind::Stop { stop_price: 1 }));
        assert!(!excluded_kind(&OrderKind::Limit { price: 1 }));
        assert!(!excluded_kind(&OrderKind::StopLimit { price: 1, stop_price: 1 }));
    }

    #[test]
    fn only_stocks_have_a_known_preset() {
        assert!(preset("STK"));
        for t in ["OPT", "FUT", "CASH", "BAG", "WAR"] {
            assert!(!preset(t), "{t}");
        }
    }
}
