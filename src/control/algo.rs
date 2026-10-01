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
        })
    }).collect()
}

/// The reference's checks of an algo order against the definitions
/// (ibx#263), in its order: every parameter name must be one of the
/// algorithm's (443), then each value with a legal value list must be in
/// it (145), then each number must be within the bounds (441). None when
/// the order passes, or when the algorithm is not defined.
pub fn refusal(definitions: &AlgoDefinitions, algorithm: &str, values: &[(&str, &str)]) -> Option<(i64, String)> {
    let params = definitions.parameters(algorithm)?;
    let find = |name: &str| params.iter().find(|p| p.name == name).copied();
    for (name, _) in values {
        if find(name).is_none() {
            return Some((443, format!("Order processing failed. Unknown algo attribute:{}", name)));
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
    None
}

/// A number as the reference reads it: the infinities and NaN only in
/// their Java spelling. A value it cannot read is not checked here.
fn java_double(value: &str) -> Option<f64> {
    let v = value.trim();
    let word = v.trim_start_matches(['+', '-']);
    if word.chars().any(|c| c.is_ascii_alphabetic()) && !matches!(word, "Infinity" | "NaN") {
        return None;
    }
    v.parse::<f64>().ok()
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
    // definitions.
    #[test]
    fn refusals_follow_the_definitions() {
        let d = captured_definitions();
        assert_eq!(refusal(&d, "Twap", &[("strategyType", "Marketable"), ("allowPastEndTime", "1")]),
            Some((443, "Order processing failed. Unknown algo attribute:strategyType".into())));
        assert_eq!(refusal(&d, "Adaptive", &[("adaptivePriority", "Bogus")]),
            Some((145, "Error in validating entry fields -Bogus".into())));
        assert_eq!(refusal(&d, "ArrivalPx", &[("riskAversion", "neutral")]),
            Some((145, "Error in validating entry fields -neutral".into())), "the legal values are exact");
        assert_eq!(refusal(&d, "Vwap", &[("maxPctVol", "NaN")]).unwrap().1,
            "Algo attributes validation failed: 'Max Percentage' is invalid: Value is greater than maximum value 50.0.. ");
        assert_eq!(refusal(&d, "Vwap", &[("maxPctVol", "-0.1")]).unwrap().1,
            "Algo attributes validation failed: 'Max Percentage' is invalid: Value is less than minimum value 0.01.. ");
        assert_eq!(refusal(&d, "PctVol", &[("pctVol", "-Infinity")]).unwrap().1,
            "Algo attributes validation failed: 'Target Percentage' is invalid: Value is less than minimum value 0.01.. ");
        assert_eq!(refusal(&d, "PctVol", &[("pctVol", "Infinity")]).unwrap().0, 441);
        assert_eq!(refusal(&d, "PctVol", &[("pctVol", "inf")]), None, "not a number the reference reads");
        // The unknown name is reported before a bad value.
        assert_eq!(refusal(&d, "Vwap", &[("maxPctVol", "99"), ("bogus", "1")]).unwrap().0, 443);
        // Accepted values.
        assert_eq!(refusal(&d, "ArrivalPx", &[("maxPctVol", "0.1"), ("riskAversion", "Neutral"), ("startTime", "")]), None);
        assert_eq!(refusal(&d, "Vwap", &[("maxPctVol", "50")]), None);
        // An algorithm the definitions do not have is not checked.
        assert_eq!(refusal(&d, "DarkIce", &[("anything", "1")]), None);
    }
}
