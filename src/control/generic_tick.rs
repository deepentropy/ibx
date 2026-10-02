//! The generic tick list of reqMktData, read as the reference reads it
//! (ibx#450): the parse, the ticks legal for each security type, and the
//! text of the 321 that refuses a list.
//!
//! Only the request side: which generic ticks go to the farm and how their
//! values come back is not done here (the news tick, 292, has its own path).

/// The security types a generic tick is legal for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Legal {
    All,
    /// IBDividends (456, alias 59).
    Dividends,
    /// Index Future Premium (162).
    Index,
    /// Short-Term Volume X Mins (595).
    ShortTermVolume,
}

impl Legal {
    fn allows(self, sec_type: &str) -> bool {
        let set: &[&str] = match self {
            Legal::All => return true,
            Legal::Dividends => &["STK", "CFD", "OPT", "FOP", "FUT", "IND", "SLB"],
            Legal::Index => &["IND"],
            Legal::ShortTermVolume => &["STK", "CFD", "OPT", "FOP", "WAR", "FUT", "FWD", "BOND", "BILL", "SLB", "IOPT", "CRYPTO"],
        };
        set.iter().any(|s| s.eq_ignore_ascii_case(sec_type))
    }
}

/// The reference's generic ticks that are legal for some security type,
/// in its table order (request code, then alias), with their names: the
/// "Legal ones" of the 321 lists them. Its other ticks are legal for none.
const LISTED: [(i32, Option<i32>, &str, Legal); 29] = [
    (100, None, "Option Volume", Legal::All),
    (101, None, "Option Open Interest", Legal::All),
    (105, None, "Average Opt Volume", Legal::All),
    (106, None, "impvolat", Legal::All),
    (162, None, "Index Future Premium", Legal::Index),
    (165, None, "Misc. Stats", Legal::All),
    (221, Some(220), "Creditman Mark Price", Legal::All),
    (225, None, "Auction", Legal::All),
    (232, Some(221), "Pl Price", Legal::All),
    (233, None, "RTVolume", Legal::All),
    (236, None, "inventory", Legal::All),
    (258, Some(47), "Fundamentals", Legal::All),
    (292, None, "Wide_news", Legal::All),
    (293, None, "TradeCount", Legal::All),
    (294, None, "TradeRate", Legal::All),
    (295, None, "VolumeRate", Legal::All),
    (318, None, "LastRTHTrade", Legal::All),
    (375, None, "RTTrdVolume", Legal::All),
    (411, None, "rthistvol", Legal::All),
    (456, Some(59), "IBDividends", Legal::Dividends),
    (460, None, "Bond Factor Multiplier", Legal::All),
    (577, None, "EtfNavLast(navlast)", Legal::All),
    (586, None, "IPOHLMPRC", Legal::All),
    (587, None, "Pl Price Delayed", Legal::All),
    (588, None, "Futures Open Interest", Legal::All),
    (595, None, "Short-Term Volume X Mins", Legal::ShortTermVolume),
    (614, None, "EtfNavMisc(high/low)", Legal::All),
    (619, None, "Creditman Slow Mark Price", Legal::All),
    (623, None, "EtfFrozenNavLast(fznavlast)", Legal::All),
];

/// The tick an API id names, as the reference looks it up: by request code
/// first, then by alias; 221 is the alias of Pl Price (232), its own code
/// giving way to it, and 104 the alias of a tick (request code 512) that
/// the "Legal ones" list does not show. (request code, legality); None
/// for an id the reference does not know or that is legal for no type.
fn lookup(id: i32) -> Option<(i32, Legal)> {
    match id {
        221 => Some((232, Legal::All)),
        220 => Some((221, Legal::All)),
        47 => Some((258, Legal::All)),
        59 => Some((456, Legal::Dividends)),
        104 => Some((512, Legal::All)),
        _ => LISTED.iter().find(|(code, ..)| *code == id).map(|(code, _, _, legal)| (*code, *legal)),
    }
}

/// One tick of a valid list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenericTick {
    /// The id as the list gives it.
    pub id: i32,
    /// The reference's request code for it.
    pub code: i32,
    /// What follows `id:`, if any.
    pub param: Option<String>,
}

/// Java's `String.trim`: every char up to the space, at both ends.
fn java_trim(s: &str) -> &str {
    s.trim_matches(|c: char| c <= ' ')
}

/// Parse a generic tick list for a security type, as the reference does:
/// tokens between commas (empty ones skipped), trimmed; `mdoff` (any case)
/// is no tick; `N:param` keeps `param` (`N:` with nothing after the colon
/// makes the list invalid); `N` is an integer naming a tick legal for the
/// security type. One bad token makes the whole list invalid, and so does a
/// list without any tick ("" or "mdoff" alone). None when invalid. A tick
/// named twice keeps its last parameter.
pub fn parse(list: &str, sec_type: &str) -> Option<Vec<GenericTick>> {
    let mut ticks: Vec<GenericTick> = Vec::new();
    for token in list.split(',').filter(|t| !t.is_empty()) {
        let token = java_trim(token);
        if token.eq_ignore_ascii_case("mdoff") {
            continue;
        }
        let (id, param) = if token.contains(':') {
            // Java's split drops trailing empty parts: "292:" has no second.
            let mut parts: Vec<&str> = token.split(':').collect();
            while parts.len() > 1 && parts.last() == Some(&"") {
                parts.pop();
            }
            (parts[0], Some(parts.get(1)?.to_string()))
        } else {
            (token, None)
        };
        let id: i32 = id.parse().ok()?;
        let (code, legal) = lookup(id)?;
        if !legal.allows(sec_type) {
            return None;
        }
        match ticks.iter_mut().find(|t| t.code == code) {
            Some(t) => t.param = param,
            None => ticks.push(GenericTick { id, code, param }),
        }
    }
    (!ticks.is_empty()).then_some(ticks)
}

/// The security type as the reference prints it.
fn sec_type_name(sec_type: &str) -> String {
    if sec_type.eq_ignore_ascii_case("NEWS") { "News".to_string() } else { sec_type.to_ascii_uppercase() }
}

/// The "Legal ones" of a security type: each legal tick as
/// `code[/alias](name)`, comma separated, in the reference's order.
pub fn legal_ones(sec_type: &str) -> String {
    let mut out = String::new();
    for (code, alias, name, legal) in LISTED {
        if !legal.allows(sec_type) {
            continue;
        }
        if !out.is_empty() {
            out.push(',');
        }
        out.push_str(&code.to_string());
        if let Some(alias) = alias {
            out.push('/');
            out.push_str(&alias.to_string());
        }
        out.push('(');
        out.push_str(name);
        out.push(')');
    }
    out
}

/// The 321 of an invalid list. `legal` is the "Legal ones" text: the
/// reference computes it once, for the security type of the first list it
/// refuses, and gives that text to every later refusal whatever their type.
pub fn refusal(list: &str, sec_type: &str, legal: &str) -> String {
    format!(
        "Error validating request.-'bQ' : cause - Incorrect generic tick list of {}.  Legal ones for ({}) are: {}",
        list, sec_type_name(sec_type), legal,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // The 321 the gateway gave for "mdoff,292:" on GOOGL (STK), 02/10/2026
    // (ibx#450, conn b1_458_news, req 9570); its protobuf client got the
    // letter of its own request class, 'E'.
    const CAPTURED_STK: &str = "Error validating request.-'bQ' : cause - Incorrect generic tick list of mdoff,292:.  \
        Legal ones for (STK) are: 100(Option Volume),101(Option Open Interest),105(Average Opt Volume),106(impvolat),\
        165(Misc. Stats),221/220(Creditman Mark Price),225(Auction),232/221(Pl Price),233(RTVolume),236(inventory),\
        258/47(Fundamentals),292(Wide_news),293(TradeCount),294(TradeRate),295(VolumeRate),318(LastRTHTrade),\
        375(RTTrdVolume),411(rthistvol),456/59(IBDividends),460(Bond Factor Multiplier),577(EtfNavLast(navlast)),\
        586(IPOHLMPRC),587(Pl Price Delayed),588(Futures Open Interest),595(Short-Term Volume X Mins),\
        614(EtfNavMisc(high/low)),619(Creditman Slow Mark Price),623(EtfFrozenNavLast(fznavlast))";

    #[test]
    fn captured_refusal_text() {
        assert!(parse("mdoff,292:", "STK").is_none());
        assert_eq!(refusal("mdoff,292:", "STK", &legal_ones("STK")), CAPTURED_STK);
    }

    #[test]
    fn legal_ones_per_security_type() {
        let cash = legal_ones("CASH");
        assert!(!cash.contains("456/59") && !cash.contains("595(") && !cash.contains("162("), "{cash}");
        let ind = legal_ones("IND");
        assert!(ind.contains(",162(Index Future Premium),165") && ind.contains("456/59") && !ind.contains("595("), "{ind}");
        assert!(legal_ones("BOND").contains("595(") && !legal_ones("BOND").contains("456/59"));
        assert_eq!(legal_ones("stk"), legal_ones("STK"));
    }

    #[test]
    fn parse_rules() {
        let ids = |list: &str, st: &str| parse(list, st).map(|v| v.iter().map(|t| (t.id, t.code, t.param.clone())).collect::<Vec<_>>());
        assert_eq!(ids("233, 236 ,,mdoff", "STK"), Some(vec![(233, 233, None), (236, 236, None)]));
        assert_eq!(ids("MDOFF,292:BRFG+DJNL", "STK"), Some(vec![(292, 292, Some("BRFG+DJNL".into()))]));
        // The last parameter of a tick named twice.
        assert_eq!(ids("292:A,292:B", "STK"), Some(vec![(292, 292, Some("B".into()))]));
        assert_eq!(ids("292:A:B", "STK"), Some(vec![(292, 292, Some("A".into()))]));
        // Aliases: 221 is Pl Price (232), 220 the mark price (221), 104 a
        // tick the legal list does not show.
        assert_eq!(ids("221,220,47,104", "STK"),
            Some(vec![(221, 232, None), (220, 221, None), (47, 258, None), (104, 512, None)]));
        assert_eq!(ids("+233", "STK"), Some(vec![(233, 233, None)]));
        // Invalid: unknown, illegal for the type, not a number, an empty
        // parameter, no tick at all.
        for list in ["999", "13", "512", "233,13", "abc", "292:", ":5", "292 :x", "", "mdoff", ",", " "] {
            assert!(parse(list, "STK").is_none(), "{list:?}");
        }
        assert!(parse("456", "CASH").is_none());
        assert!(parse("456", "FUT").is_some());
        assert!(parse("162", "STK").is_none() && parse("162", "IND").is_some());
        assert!(parse("595", "IND").is_none() && parse("595", "CRYPTO").is_some());
    }
}
