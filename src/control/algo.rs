//! Algo definitions sent by the server (ibx#263).
//!
//! The reference does not hold the algo parameters itself: it asks the
//! server for the definitions of each algo provider (one answer with the
//! provider's legal value lists, one with its algorithms and their
//! parameters) and checks every algo order against them. The parameter
//! names, their legal values and their bounds can change without a client
//! update.

use std::collections::HashMap;

/// The definition requests of the reference for stock algos: the
/// provider's legal value lists, then its stock algorithms.
pub const DEFINITION_KEYS: [&str; 2] = ["IBALGO-AE", "IBALGO-AL-STK"];

/// One parameter of an algorithm.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AlgoParamDef {
    /// The parameter name the API uses.
    pub name: String,
    /// The label the refusal texts give.
    pub description: String,
    /// The name of its legal value list, when it has one.
    pub legal_strings: Option<String>,
    /// Its bounds, as the definition writes them.
    pub min: Option<String>,
    pub max: Option<String>,
    /// Its value type (`Double`, `Integer`, `Boolean`, `String`, `Time`...).
    pub value_class: String,
    /// It must have a value: given, or its default.
    pub required: bool,
    /// Its default value, when it has one.
    pub default_value: Option<String>,
    /// It is the strategy selector (the algorithm's name), not a parameter
    /// the caller gives.
    pub strategy_selector: bool,
}

/// What the server defined so far.
#[derive(Debug, Clone, Default)]
pub struct AlgoDefinitions {
    /// Legal value lists by name.
    pub legal_strings: HashMap<String, Vec<String>>,
    /// Parameters common to every algorithm of a set, by set name.
    pub common: HashMap<String, Vec<AlgoParamDef>>,
    /// The algorithms by name: their parameters and their common sets.
    pub algorithms: HashMap<String, (Vec<AlgoParamDef>, Vec<String>)>,
    /// The algorithms that may run overnight (`allowOvernight` yes).
    pub overnight: std::collections::HashSet<String>,
}

impl AlgoDefinitions {
    /// Add one definition answer.
    pub fn add(&mut self, xml: &str) {
        for block in blocks(xml, "AlgoLegalStrings") {
            let Some(name) = text(block, "name") else { continue };
            // Each value is written with its scope first ("ALL:Urgent").
            let values = string_values(block).into_iter()
                .map(|v| v.split_once(':').map_or(v, |(_, value)| value).to_string())
                .collect();
            self.legal_strings.insert(name.to_string(), values);
        }
        for block in blocks(xml, "AlgoAttributeContentHolder") {
            let Some(name) = text(block, "name") else { continue };
            self.common.insert(name.to_string(), params(block));
        }
        for block in blocks(xml, "Algorithm") {
            let head = block.split("<Array").next().unwrap_or(block);
            let Some(name) = text(head, "shortName") else { continue };
            let sets = text(head, "commonAttributeSets").unwrap_or("")
                .split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect();
            if text(head, "allowOvernight").is_some_and(|v| v.eq_ignore_ascii_case("yes")) {
                self.overnight.insert(name.to_string());
            }
            self.algorithms.insert(name.to_string(), (params(block), sets));
        }
    }

    /// The parameters an algorithm takes, its common sets included. None
    /// when the algorithm is not defined.
    pub fn parameters(&self, algorithm: &str) -> Option<Vec<&AlgoParamDef>> {
        let (own, sets) = self.algorithms.get(algorithm)?;
        let mut all: Vec<&AlgoParamDef> = own.iter().collect();
        for set in sets {
            if let Some(common) = self.common.get(set) {
                all.extend(common.iter());
            }
        }
        Some(all)
    }
}

/// Every `<tag>...</tag>` block of `xml`, in order.
fn blocks<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find(&open) {
        let body = &rest[start + open.len()..];
        let Some(end) = body.find(&close) else { break };
        out.push(&body[..end]);
        rest = &body[end + close.len()..];
    }
    out
}

/// The text of the first `<tag>` of `xml`.
fn text<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    crate::control::historical::extract_xml_tag(xml, tag).map(str::trim)
}

/// The values of the `<String ...>` elements of a legal value list.
fn string_values(xml: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<String") {
        let after = &rest[start + "<String".len()..];
        let Some(gt) = after.find('>') else { break };
        let body = &after[gt + 1..];
        let Some(end) = body.find("</String>") else { break };
        out.push(body[..end].trim());
        rest = &body[end..];
    }
    out
}

/// The value of `<Object varName="name" ...>value</Object>` in `xml`.
fn object(xml: &str, name: &str) -> Option<String> {
    let marker = format!("varName=\"{}\"", name);
    let at = xml.find(&marker)?;
    let after = &xml[at..];
    let body = &after[after.find('>')? + 1..];
    Some(body[..body.find("</Object>")?].trim().to_string())
}

/// The parameters of a definition block.
fn params(xml: &str) -> Vec<AlgoParamDef> {
    blocks(xml, "AlgoAttributeContent").into_iter().filter_map(|p| {
        Some(AlgoParamDef {
            name: text(p, "shortName")?.to_string(),
            description: text(p, "description").unwrap_or("").to_string(),
            legal_strings: text(p, "legalStringsName").map(String::from),
            min: object(p, "minValue"),
            max: object(p, "maxValue"),
            value_class: text(p, "valueClassName").unwrap_or("").to_string(),
            required: text(p, "required") == Some("true"),
            default_value: object(p, "defaultValue"),
            strategy_selector: text(p, "isStrategySelector") == Some("true"),
        })
    }).collect()
}

/// Text of error 439: an algorithm the definitions do not have.
pub const NO_ALGO_DEFINITION: &str = "Order processing failed. Algorithm definition not found";

/// Text of error 442: an algorithm that may not run overnight on an
/// overnight order.
pub const ALGO_NOT_ALLOWED: &str = "Specified algorithm is not allowed for this order.";

/// The reference's checks of an algo order against the definitions
/// (ibx#263), in its order (`jextend.algo.a`):
/// - the algorithm must be defined and have its strategy selector (439);
/// - an overnight order (`overnight`: the OVERNIGHT or IBEOS exchange, or
///   includeOvernight) needs an algorithm allowed overnight (442);
/// - then each parameter in the caller's order: a name the algorithm does
///   not have (443); a time (value type Time, Date or DateTime) the
///   reference cannot read (10314 with the parameter's name; read as the
///   reference reads dates, a time with no zone gets the warning 2174 on
///   the way, `jextend.algo.a.a(AlgoExchanges, AlgoAttributeMap,
///   OrderCreator, dy)@477-610`); a number the reference cannot read (441
///   `name=value`);
/// - then each value with a legal value list must be in it (145), each
///   number within its bounds (441);
/// - last, each required parameter with no value and no default (441,
///   one line per parameter).
///
/// None when the order passes, or when no definition came yet.
pub fn refusal(definitions: &AlgoDefinitions, algorithm: &str, values: &[(&str, &str)], overnight: bool) -> Option<(i64, String)> {
    check(definitions, algorithm, values, overnight, &mut Vec::new())
}

/// [`refusal`], with the warnings 2174 of the time parameters the check
/// reached, given with no zone, in `warnings`.
pub fn check(definitions: &AlgoDefinitions, algorithm: &str, values: &[(&str, &str)], overnight: bool,
    warnings: &mut Vec<(i64, String)>) -> Option<(i64, String)>
{
    if definitions.algorithms.is_empty() {
        return None;
    }
    let Some(params) = definitions.parameters(algorithm) else {
        return Some((439, NO_ALGO_DEFINITION.to_string()));
    };
    if !params.iter().any(|p| p.strategy_selector) {
        return Some((439, NO_ALGO_DEFINITION.to_string()));
    }
    if overnight && !definitions.overnight.contains(algorithm) {
        return Some((442, ALGO_NOT_ALLOWED.to_string()));
    }
    let find = |name: &str| params.iter().find(|p| p.name == name).copied();
    for (name, value) in values {
        let Some(param) = find(name) else {
            return Some((443, format!("Order processing failed. Unknown algo attribute:{}", name)));
        };
        // A time parameter (`AlgoAttribute.c(Class)`: the value types the
        // reference reads as dates, `jattrib.algo.i`).
        if matches!(param.value_class.as_str(), "Time" | "Date" | "DateTime") && !value.is_empty() {
            match crate::client_core::parse_condition_time(value, &crate::gateway::machine_time_zone()) {
                Some(t) => {
                    if t.implied_zone {
                        warnings.push((2174, crate::client_core::IMPLIED_TIME_ZONE.to_string()));
                    }
                }
                None => return Some((10314, crate::client_core::INVALID_DATE_TIME.replace("%s", name))),
            }
            continue;
        }
        let readable = match param.value_class.as_str() {
            "Double" => java_double(value).is_some(),
            "Integer" => value.trim().parse::<i32>().is_ok(),
            _ => true,
        };
        if !value.trim().is_empty() && !readable {
            return Some((441, format!("Algo attributes validation failed:{}={}", name, value)));
        }
    }
    for (name, value) in values {
        let Some(list) = find(name).and_then(|p| p.legal_strings.as_ref())
            .and_then(|l| definitions.legal_strings.get(l)) else { continue };
        if !value.is_empty() && !list.iter().any(|v| v == value) {
            return Some((145, format!("Error in validating entry fields -{}", value)));
        }
    }
    for (name, value) in values {
        let Some(param) = find(name) else { continue };
        let Some(number) = java_double(value) else { continue };
        let bound = |b: &Option<String>| b.as_ref().and_then(|s| s.parse::<f64>().ok().map(|n| (n, s.clone())));
        let refuse = |what: &str, bound: &str| Some((441, format!(
            "Algo attributes validation failed: '{}' is invalid: Value is {} {}.. ", param.description, what, bound)));
        // A number that is not a number is above every bound, as the
        // reference compares them.
        if let Some((min, text)) = bound(&param.min) {
            if !number.is_nan() && number < min {
                return refuse("less than minimum value", &text);
            }
        }
        if let Some((max, text)) = bound(&param.max) {
            if number.is_nan() || number > max {
                return refuse("greater than maximum value", &text);
            }
        }
    }
    // Captured 02/10/2026 (PctVol with no pctVol): "Algo attributes
    // validation failed:\n'Target Percentage' is invalid: value is
    // required.\n" (`jattrib.algo.p.a(AlgoAttribute, Object,
    // StringBuffer)`, one line per parameter).
    let missing: String = params.iter()
        .filter(|p| p.required && !p.strategy_selector && p.default_value.is_none())
        .filter(|p| !values.iter().any(|(name, value)| *name == p.name && !value.trim().is_empty()))
        .map(|p| format!("'{}' is invalid: value is required.\n", p.description))
        .collect();
    if !missing.is_empty() {
        return Some((441, format!("Algo attributes validation failed:\n{}", missing)));
    }
    None
}

/// A number as the reference reads it (`Double.parseDouble`): spaces
/// around it, an optional `d`/`f` suffix, the infinities and NaN only in
/// their Java spelling. None for a value it cannot read.
fn java_double(value: &str) -> Option<f64> {
    let v = value.trim();
    let word = v.trim_start_matches(['+', '-']);
    if matches!(word, "Infinity" | "NaN") {
        return v.parse::<f64>().ok();
    }
    let body = v.strip_suffix(['d', 'D', 'f', 'F']).unwrap_or(v);
    if body.chars().any(|c| c.is_ascii_alphabetic() && !matches!(c, 'e' | 'E')) {
        return None;
    }
    body.parse::<f64>().ok()
}

/// The definitions of the captured answers (ib-agent capture of
/// 11/03/2026), cut to the algorithms the tests use.
#[cfg(test)]
pub(crate) fn captured_definitions() -> AlgoDefinitions {
    let mut d = AlgoDefinitions::default();
    d.add(CAPTURED_AE);
    d.add(CAPTURED_AL_STK);
    d
}

#[cfg(test)]
pub(crate) const CAPTURED_AE: &str = "<AlgoExchange>
	<name>IBALGO</name>
	<AlgoAttributeContentHolderMap varName=\"commonAlgoAttributeContent\">
		<AlgoAttributeContentHolder>
			<name>IBALGO_COMMON</name>
			<Array varName=\"algoAttribContents\">
				<AlgoAttributeContent>
					<shortName>strategy</shortName>
					<required>true</required>
					<isStrategySelector>true</isStrategySelector>
					<description>Strategy</description>
					<valueClassName>String</valueClassName>
				</AlgoAttributeContent>
			</Array>
		</AlgoAttributeContentHolder>
	</AlgoAttributeContentHolderMap>
	<AlgoLegalStringsMap varName=\"algoLegalStringsMap\">
		<AlgoLegalStrings>
			<name>AdaptivePriority</name>
			<ArString varName=\"arString\">
				<String>ALL:Urgent</String>
				<String>ALL:Normal</String>
				<String>ALL:Patient</String>
			</ArString>
		</AlgoLegalStrings>
		<AlgoLegalStrings>
			<name>RiskAversion</name>
			<ArString varName=\"arString\">
				<String>ALL:GetDone</String>
				<String>ALL:Aggressive</String>
				<String>ALL:Neutral</String>
				<String>ALL:Passive</String>
			</ArString>
		</AlgoLegalStrings>
	</AlgoLegalStringsMap>
</AlgoExchange>";

#[cfg(test)]
pub(crate) const CAPTURED_AL_STK: &str = "<AlgorithmsMap>
	<Algorithm>
		<algoExchange>IBALGO</algoExchange>
		<shortName>ArrivalPx</shortName>
		<description>Arrival Price</description>
		<commonAttributeSets>IBALGO_COMMON</commonAttributeSets>
		<Array varName=\"attribContents\">
			<AlgoAttributeContent>
				<shortName>maxPctVol</shortName>
				<description>Max Percentage</description>
				<valueClassName>Double</valueClassName>
				<Object varName=\"minValue\">0.01</Object>
				<Object varName=\"maxValue\">50.0</Object>
			</AlgoAttributeContent>
			<AlgoAttributeContent>
				<shortName>riskAversion</shortName>
				<description>Urgency/Risk aversion</description>
				<Object varName=\"defaultValue\">Neutral</Object>
				<valueClassName>String</valueClassName>
				<legalStringsName>RiskAversion</legalStringsName>
			</AlgoAttributeContent>
			<AlgoAttributeContent><shortName>startTime</shortName><description>Start Time</description></AlgoAttributeContent>
			<AlgoAttributeContent><shortName>endTime</shortName><description>End Time</description></AlgoAttributeContent>
			<AlgoAttributeContent><shortName>allowPastEndTime</shortName><description>Allow trading past end time</description></AlgoAttributeContent>
			<AlgoAttributeContent><shortName>forceCompletion</shortName><description>Attempt completion by EOD</description></AlgoAttributeContent>
			<AlgoAttributeContent><shortName>monetaryValue</shortName><description>Cash Quantity</description></AlgoAttributeContent>
		</Array>
	</Algorithm>
	<Algorithm>
		<algoExchange>IBALGO</algoExchange>
		<shortName>Adaptive</shortName>
		<commonAttributeSets>IBALGO_COMMON</commonAttributeSets>
		<Array varName=\"attribContents\">
			<AlgoAttributeContent><shortName>monetaryValue</shortName><description>Cash Quantity</description></AlgoAttributeContent>
			<AlgoAttributeContent>
				<shortName>adaptivePriority</shortName>
				<description>Priority</description>
				<legalStringsName>AdaptivePriority</legalStringsName>
			</AlgoAttributeContent>
		</Array>
	</Algorithm>
	<Algorithm>
		<algoExchange>IBALGO</algoExchange>
		<shortName>PctVol</shortName>
		<commonAttributeSets>IBALGO_COMMON</commonAttributeSets>
		<Array varName=\"attribContents\">
			<AlgoAttributeContent>
				<shortName>pctVol</shortName>
				<description>Target Percentage</description>
				<Object varName=\"minValue\">0.01</Object>
				<Object varName=\"maxValue\">50.0</Object>
			</AlgoAttributeContent>
			<AlgoAttributeContent><shortName>startTime</shortName><description>Start Time</description></AlgoAttributeContent>
			<AlgoAttributeContent><shortName>endTime</shortName><description>End Time</description></AlgoAttributeContent>
			<AlgoAttributeContent><shortName>noTakeLiq</shortName><description>Attempt never to take liquidity</description></AlgoAttributeContent>
		</Array>
	</Algorithm>
	<Algorithm>
		<algoExchange>IBALGO</algoExchange>
		<shortName>Vwap</shortName>
		<commonAttributeSets>IBALGO_COMMON</commonAttributeSets>
		<Array varName=\"attribContents\">
			<AlgoAttributeContent>
				<shortName>maxPctVol</shortName>
				<description>Max Percentage</description>
				<Object varName=\"minValue\">0.01</Object>
				<Object varName=\"maxValue\">50.0</Object>
			</AlgoAttributeContent>
			<AlgoAttributeContent><shortName>startTime</shortName><description>Start Time</description></AlgoAttributeContent>
			<AlgoAttributeContent><shortName>endTime</shortName><description>End Time</description></AlgoAttributeContent>
			<AlgoAttributeContent><shortName>allowPastEndTime</shortName><description>Allow trading past end time</description></AlgoAttributeContent>
			<AlgoAttributeContent><shortName>noTakeLiq</shortName><description>Attempt never to take liquidity</description></AlgoAttributeContent>
		</Array>
	</Algorithm>
	<Algorithm>
		<algoExchange>IBALGO</algoExchange>
		<shortName>Twap</shortName>
		<commonAttributeSets>IBALGO_COMMON</commonAttributeSets>
		<Array varName=\"attribContents\">
			<AlgoAttributeContent><shortName>startTime</shortName><description>Start Time</description></AlgoAttributeContent>
			<AlgoAttributeContent><shortName>endTime</shortName><description>End Time</description></AlgoAttributeContent>
			<AlgoAttributeContent><shortName>allowPastEndTime</shortName><description>Allow trading past end time</description></AlgoAttributeContent>
		</Array>
	</Algorithm>
</AlgorithmsMap>";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captured_definitions_are_read() {
        let d = captured_definitions();
        assert_eq!(d.legal_strings["RiskAversion"], ["GetDone", "Aggressive", "Neutral", "Passive"]);
        let vwap = d.parameters("Vwap").unwrap();
        let max = vwap.iter().find(|p| p.name == "maxPctVol").unwrap();
        assert_eq!((max.min.as_deref(), max.max.as_deref(), max.description.as_str()),
            (Some("0.01"), Some("50.0"), "Max Percentage"));
        assert!(vwap.iter().any(|p| p.name == "strategy"), "the common set is included");
        assert!(d.parameters("Nope").is_none());
    }

    // ib-agent#192 B9b and B10: the captured refusals, from the
    // definitions (gateway local rules `145 algo.a.a(pe,OrderCreator)@212`,
    // `443 algo.a.a(AlgoExchanges,...)@376`, `441 algo.a.a(AlgoExchanges,...)@649`,
    // `439 algo.a.a(AlgoExchanges,...)@36`).
    #[test]
    fn refusals_follow_the_definitions() {
        let d = captured_definitions();
        assert_eq!(refusal(&d, "Twap", &[("strategyType", "Marketable"), ("allowPastEndTime", "1")], false),
            Some((443, "Order processing failed. Unknown algo attribute:strategyType".into())));
        assert_eq!(refusal(&d, "Adaptive", &[("adaptivePriority", "Bogus")], false),
            Some((145, "Error in validating entry fields -Bogus".into())));
        assert_eq!(refusal(&d, "ArrivalPx", &[("riskAversion", "neutral")], false),
            Some((145, "Error in validating entry fields -neutral".into())), "the legal values are exact");
        assert_eq!(refusal(&d, "Vwap", &[("maxPctVol", "NaN")], false).unwrap().1,
            "Algo attributes validation failed: 'Max Percentage' is invalid: Value is greater than maximum value 50.0.. ");
        assert_eq!(refusal(&d, "Vwap", &[("maxPctVol", "-0.1")], false).unwrap().1,
            "Algo attributes validation failed: 'Max Percentage' is invalid: Value is less than minimum value 0.01.. ");
        assert_eq!(refusal(&d, "PctVol", &[("pctVol", "-Infinity")], false).unwrap().1,
            "Algo attributes validation failed: 'Target Percentage' is invalid: Value is less than minimum value 0.01.. ");
        assert_eq!(refusal(&d, "PctVol", &[("pctVol", "Infinity")], false).unwrap().0, 441);
        assert_eq!(refusal(&d, "PctVol", &[("pctVol", "inf")], false), None, "not a number the reference reads");
        // The unknown name is reported before a bad value.
        assert_eq!(refusal(&d, "Vwap", &[("maxPctVol", "99"), ("bogus", "1")], false).unwrap().0, 443);
        // Accepted values.
        assert_eq!(refusal(&d, "ArrivalPx", &[("maxPctVol", "0.1"), ("riskAversion", "Neutral"), ("startTime", "")], false), None);
        assert_eq!(refusal(&d, "Vwap", &[("maxPctVol", "50")], false), None);
        // An algorithm the definitions do not have is refused (ibx#263,
        // captured 02/10/2026), but nothing is checked before the first
        // definitions came.
        assert_eq!(refusal(&d, "DarkIce", &[("anything", "1")], false),
            Some((439, "Order processing failed. Algorithm definition not found".into())));
        assert_eq!(refusal(&AlgoDefinitions::default(), "Foo", &[], false), None);
    }

    /// The definitions the server sent on 02/10/2026 (paper, stock
    /// algorithms of the provider IBALGO).
    fn definitions_of_20261002() -> AlgoDefinitions {
        let mut d = AlgoDefinitions::default();
        d.add(include_str!("../../tests/fixtures/algo/IBALGO-AE-20261002.xml"));
        d.add(include_str!("../../tests/fixtures/algo/IBALGO-AL-STK-20261002.xml"));
        d
    }

    // ibx#263, captured 02/10/2026 (b1_263_algo_refusals): the refusals
    // the reference gives before sending, with their texts.
    #[test]
    fn captured_refusals_of_20261002() {
        let d = definitions_of_20261002();
        assert_eq!(refusal(&d, "Foo", &[], false),
            Some((439, "Order processing failed. Algorithm definition not found".into())));
        assert_eq!(refusal(&d, "PctVol", &[], false),
            Some((441, "Algo attributes validation failed:\n'Target Percentage' is invalid: value is required.\n".into())));
        assert_eq!(refusal(&d, "Vwap", &[("maxPctVol", "abc")], false),
            Some((441, "Algo attributes validation failed:maxPctVol=abc".into())));
        assert_eq!(refusal(&d, "Twap", &[("strategyType", "abc")], false),
            Some((443, "Order processing failed. Unknown algo attribute:strategyType".into())));
        // Adaptive on a STP order is accepted; its priority has a default.
        assert_eq!(refusal(&d, "Adaptive", &[("adaptivePriority", "Normal")], false), None);
        assert_eq!(refusal(&d, "Adaptive", &[], false), None, "the required priority has a default");
    }

    // ibx#263: a time parameter (Vwap startTime and endTime are of type
    // Time in the definitions of 02/10/2026) the reference cannot read is
    // refused with 10314 and the parameter's name, in the caller's order
    // of the parameters; one read with no zone gets the warning 2174, as
    // the time conditions (`jextend.algo.a.a(...)@477-610`).
    #[test]
    fn time_parameters_are_read_as_dates() {
        let d = definitions_of_20261002();
        let mut warnings = Vec::new();
        assert_eq!(check(&d, "Vwap", &[("startTime", "09:00:00 US/Eastern"), ("endTime", "20261002-20:00:00")], false, &mut warnings), None);
        assert!(warnings.is_empty());
        assert_eq!(check(&d, "Vwap", &[("startTime", "09:00:00")], false, &mut warnings), None);
        assert_eq!(warnings, [(2174, crate::client_core::IMPLIED_TIME_ZONE.to_string())]);
        let (code, text) = refusal(&d, "Vwap", &[("startTime", "9am"), ("maxPctVol", "abc")], false).unwrap();
        assert_eq!(code, 10314);
        assert!(text.starts_with("startTime: The date, time, or time-zone entered is invalid.\nThe correct format"), "{text}");
        assert_eq!(refusal(&d, "Vwap", &[("maxPctVol", "abc"), ("startTime", "9am")], false).map(|r| r.0), Some(441));
        assert_eq!(refusal(&d, "Vwap", &[("startTime", "")], false), None, "an empty value is not read");
    }

    // ibx#263 (`jextend.algo.a`): an overnight order needs an algorithm
    // allowed overnight (442, gateway local rule
    // `442 algo.a.a(AlgoExchanges,...)@150`); the first parameter in the
    // caller's order that fails gives the refusal; a number the reference
    // cannot read is refused, and an empty value is not read.
    #[test]
    fn overnight_and_number_rules() {
        let d = definitions_of_20261002();
        assert!(d.overnight.contains("Adaptive"));
        assert_eq!(refusal(&d, "Vwap", &[], true),
            Some((442, "Specified algorithm is not allowed for this order.".into())));
        assert_eq!(refusal(&d, "Adaptive", &[("adaptivePriority", "Normal")], true), None);
        assert_eq!(refusal(&d, "Vwap", &[("maxPctVol", "abc"), ("bogus", "1")], false).unwrap().1,
            "Algo attributes validation failed:maxPctVol=abc");
        assert_eq!(refusal(&d, "Vwap", &[("bogus", "1"), ("maxPctVol", "abc")], false).unwrap().0, 443);
        assert_eq!(refusal(&d, "PctVol", &[("pctVol", "inf")], false).unwrap().1,
            "Algo attributes validation failed:pctVol=inf");
        assert_eq!(refusal(&d, "PctVol", &[("pctVol", " 10d ")], false), None, "read as Java reads a double");
        assert_eq!(refusal(&d, "PctVol", &[("pctVol", "1e1")], false), None);
        assert_eq!(refusal(&d, "PctVol", &[("pctVol", "")], false).unwrap().1,
            "Algo attributes validation failed:\n'Target Percentage' is invalid: value is required.\n");
    }
}
