//! Fundamental data queries via the data farm connection.

use std::io::Read;

/// FIX tag 6040: the sub protocol.
pub const TAG_SUB_PROTOCOL: u32 = 6040;
/// FIX tag 95: the raw data length.
pub const TAG_RAW_DATA_LENGTH: u32 = 95;
/// FIX tag 96: the raw data.
pub const TAG_RAW_DATA: u32 = 96;

/// The pairs a company-data series states, as the venue writes them.
///
/// Sixteen series carry what the venue holds about the company behind a
/// contract, or about the terms of dealing in it, rather than about its quote
/// — among them the two analyst ratings and the insider and institutional
/// interest, which states how much of the company institutions and insiders
/// hold and how many shares are on issue: a float is worked out from those and
/// is not itself stated. Every one of them carries text: one line or several,
/// each a run of `KEY=VALUE` joined by semicolons.
///
/// The keys are the venue's own and are handed on unchanged. This reads the
/// shape and not the vocabulary, so a key the venue adds arrives with the rest
/// instead of being dropped for not being recognised here, and none of them is
/// renamed into something this client made up.
///
/// A pair with no key, or a key with nothing behind it, states nothing and is
/// left out. Where a key is stated twice the last statement of it stands.
pub fn stated_pairs(text: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    // A key is a run of word characters and a value is everything up to the
    // next separator, which for a value is an equals sign as much as a
    // semicolon: `A=B=C` states `A` as `B` and then a key of nothing, which
    // states nothing. Read to the end of the run instead, the value would
    // carry a second pair's worth of text.
    let is_key = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut rest = text;
    while let Some(at) = rest.find('=') {
        let (before, after) = rest.split_at(at);
        // The key is the word that ends at the separator, whatever came
        // before it. Anything not a word character between two pairs is not
        // part of either.
        let key: String = before
            .chars()
            .rev()
            .take_while(|c| is_key(*c))
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let value: &str = &after[1..];
        let end = value.find([';', '=', '\n', '\r']).unwrap_or(value.len());
        let (value, after) = value.split_at(end);
        rest = after;
        if key.is_empty() || value.is_empty() {
            continue;
        }
        match out.iter_mut().find(|(held, _)| *held == key) {
            Some(held) => held.1 = value.to_string(),
            None => out.push((key, value.to_string())),
        }
    }
    out
}

/// Report types for fundamental data queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportType {
    /// A summary of the issuer as it stands.
    Snapshot,
    /// What analysts expect of it.
    Estimates,
    /// What it has coming: earnings dates and the like.
    Calendar,
}

impl ReportType {
    /// Which provider supplies this report.
    pub fn provider(&self) -> &'static str {
        "Fundamentals"
    }

    /// The name the venue knows the report by.
    ///
    /// Three, and these three: the reference client names exactly these and
    /// asks for them under exactly these words. Two others used to be offered
    /// here — a summary and the full statements — under names that appear
    /// nowhere in the vendor build at all, so what went out for them was a
    /// word the venue has never been asked for. The two the reference client
    /// does offer were refused instead.
    pub fn report_type_str(&self) -> &'static str {
        match self {
            Self::Snapshot => "snapshot",
            Self::Estimates => "estimates",
            Self::Calendar => "calendar",
        }
    }

    /// The name a caller asks for it by.
    pub fn api_name(&self) -> &'static str {
        match self {
            Self::Snapshot => "ReportSnapshot",
            Self::Estimates => "RESC",
            Self::Calendar => "CalendarReport",
        }
    }
}

/// Parameters for a fundamental data request.
#[derive(Debug, Clone)]
pub struct FundamentalRequest {
    /// The venue's id for the contract.
    pub con_id: u32,
    /// What kind of contract it is, as the venue names it.
    pub sec_type: String,
    /// What currency that is in.
    pub currency: String,
    /// Which report is wanted.
    pub report_type: ReportType,
    /// What this request calls itself, which the venue echoes on the answer.
    pub query_id: String,
}

/// What a fundamentals request calls itself.
///
/// The reference client counts these, and puts the count in the name, so the
/// venue echoes a different one on every answer. One name for all of them left
/// two in flight indistinguishable, and the reply to either was handed to
/// whichever had waited longest.
pub fn fundamentals_query_id(counter: u32) -> String {
    format!("COMPANY_FUNDAMENTALS{counter}")
}

/// The name the venue echoes back on an answer, so it can be matched to the
/// request that asked.
pub fn echoed_query_id(xml: &str) -> String {
    let Some(at) = xml.find("<id>") else { return String::new() };
    let rest = &xml[at + 4..];
    match rest.find("</id>") {
        Some(end) => rest[..end].to_string(),
        None => String::new(),
    }
}

/// Build the XML query for a fundamental data request.
pub fn build_fundamental_request_xml(req: &FundamentalRequest) -> String {
    let query_id = &req.query_id;
    format!(
        "<ListOfQueries>\
         <FundamentalsQuery>\
         <id>{query_id}</id>\
         <contractID>{con_id}</contractID>\
         <exchange>RTRSFND</exchange>\
         <secType>{sec_type}</secType>\
         <source>API</source>\
         <needTotalValue>false</needTotalValue>\
         <wholeDays>false</wholeDays>\
         <delay>auto</delay>\
         <reportType>{report_type}</reportType>\
         <currency>{currency}</currency>\
         </FundamentalsQuery>\
         </ListOfQueries>",
        con_id = req.con_id,
        sec_type = req.sec_type,
        report_type = req.report_type.report_type_str(),
        currency = req.currency,
    )
}

/// Extract the query ID from a `<FundResponse>` XML correlation tag.
pub fn parse_fundamental_response_id(xml: &str) -> Option<String> {
    if !xml.contains("<FundResponse>") {
        return None;
    }
    crate::control::xml::tag(xml, "id").map(|s| s.to_string())
}

/// Decompress gzip-compressed fundamental data (FIX tag 96).
///
/// Held to what one inflated frame may become: the frame this came in on is
/// bounded on the wire, and a payload inside it is not, so without a ceiling
/// a small frame could ask this process for every byte it has.
pub fn decompress_fundamental_data(compressed: &[u8]) -> Option<String> {
    let mut decoder = flate2::read::GzDecoder::new(compressed)
        .take(crate::protocol::fixcomp::MAX_INFLATED + 1);
    let mut result = String::new();
    decoder.read_to_string(&mut result).ok()?;
    if result.len() as u64 > crate::protocol::fixcomp::MAX_INFLATED {
        return None;
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;

    /// The pairs come back as the venue wrote them, and what is not a pair
    /// does not.
    ///
    /// The keys belong to the venue: it adds to them and renames them, so a
    /// reader that kept a list of the ones it knew would drop whatever was
    /// added next. What is refused is only what states nothing — a key with
    /// nothing behind it, a value with no key, and a run of bytes that
    /// happens to carry an equals sign.
    #[test]
    fn the_pairs_a_series_states_are_read_as_the_venue_wrote_them() {
        assert_eq!(
            stated_pairs("RATING=2;ANALYSTS=17"),
            [("RATING".to_string(), "2".to_string()),
             ("ANALYSTS".to_string(), "17".to_string())],
        );
        // Several lines, and a decimal in a value.
        assert_eq!(
            stated_pairs("FLOAT=123456789\nPCTHELD=61.2\r\n"),
            [("FLOAT".to_string(), "123456789".to_string()),
             ("PCTHELD".to_string(), "61.2".to_string())],
        );
        // Nothing stated: no key, no value, no pair at all.
        assert!(stated_pairs("=5;KEY=;;plain text;").is_empty());
        // A value ends at the next separator, and an equals sign is one: read
        // to the end of the run, the value would carry a second pair's worth
        // of text.
        assert_eq!(
            stated_pairs("RATING=BUY=SELL"),
            [("RATING".to_string(), "BUY".to_string())],
        );
        // The key is the word that ends at the separator, and a word is
        // written in the characters the venue writes one in: what is in front
        // of it belongs to neither pair, and a key outside those characters is
        // no key at all.
        assert_eq!(
            stated_pairs("not a key=5"),
            [("key".to_string(), "5".to_string())],
        );
        assert!(stated_pairs("\u{c9}=1").is_empty());
        // Stated twice, the last statement stands, as it does where this
        // arrives.
        assert_eq!(
            stated_pairs("RATING=2;RATING=3"),
            [("RATING".to_string(), "3".to_string())],
        );
    }

    /// The three the venue states, under the words it states them by. Two
    /// others used to be offered under words that appear nowhere in the
    /// vendor build, so a caller asking for either sent the venue something
    /// it has never been asked for.
    #[test]
    fn report_type_mapping() {
        for (kind, asked_by, on_the_wire) in [
            (ReportType::Snapshot, "ReportSnapshot", "snapshot"),
            (ReportType::Estimates, "RESC", "estimates"),
            (ReportType::Calendar, "CalendarReport", "calendar"),
        ] {
            assert_eq!(kind.api_name(), asked_by);
            assert_eq!(kind.report_type_str(), on_the_wire);
            assert_eq!(kind.provider(), "Fundamentals");
        }
    }

    #[test]
    fn fundamental_request_xml_structure() {
        let req = FundamentalRequest {
            query_id: fundamentals_query_id(1),
            con_id: 265598,
            sec_type: "STK".to_string(),
            currency: "USD".to_string(),
            report_type: ReportType::Snapshot,
        };
        let xml = build_fundamental_request_xml(&req);
        assert!(xml.contains("<ListOfQueries>"));
        assert!(xml.contains("<FundamentalsQuery>"));
        assert!(xml.contains("<contractID>265598</contractID>"));
        assert!(xml.contains("<exchange>RTRSFND</exchange>"));
        assert!(xml.contains("<secType>STK</secType>"));
        assert!(xml.contains("<reportType>snapshot</reportType>"));
        assert!(xml.contains("<currency>USD</currency>"));
        assert!(xml.contains("<source>API</source>"));
    }

    #[test]
    fn parse_response_id_basic() {
        let xml = "<FundResponse><id>q42</id></FundResponse>";
        assert_eq!(parse_fundamental_response_id(xml), Some("q42".to_string()));
    }

    #[test]
    fn decompress_gzip_data() {
        let original = "<FundamentalData><Revenue>1000000</Revenue></FundamentalData>";
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(original.as_bytes()).unwrap();
        let compressed = encoder.finish().unwrap();

        let decompressed = decompress_fundamental_data(&compressed).unwrap();
        assert_eq!(decompressed, original);
    }

    /// The frame this arrives on is bounded on the wire, and what it inflates
    /// to is not: a payload past the ceiling is refused rather than read.
    #[test]
    fn a_payload_inflating_past_the_ceiling_is_refused() {
        let huge = vec![0u8; (crate::protocol::fixcomp::MAX_INFLATED + 4096) as usize];
        let mut encoder = GzEncoder::new(Vec::new(), Compression::best());
        encoder.write_all(&huge).unwrap();
        let bomb = encoder.finish().unwrap();
        assert!(bomb.len() < 1 << 20, "the frame itself is small: {}", bomb.len());

        assert!(
            decompress_fundamental_data(&bomb).is_none(),
            "what inflates past the ceiling is not read",
        );
    }

    #[test]
    fn parse_response_rejects_other() {
        assert!(parse_fundamental_response_id("<ResultSetBar>...</ResultSetBar>").is_none());
        assert!(parse_fundamental_response_id("not xml").is_none());
    }

    /// The shape the venue expects, which is a list even when it withdraws one
    /// thing, and names the request by the id the request gave itself.
    #[test]
    fn a_withdrawal_names_the_request_it_withdraws() {
        // Each request names itself, as the reference client does — it counts
        // them and puts the count in the name — so the venue echoes a
        // different one on each answer and two in flight are told apart.
        let first = fundamentals_query_id(1);
        let second = fundamentals_query_id(2);
        assert_ne!(first, second, "two requests do not share a name");

        let asked = build_fundamental_request_xml(&FundamentalRequest {
            con_id: 265598, sec_type: "STK".to_string(), currency: "USD".to_string(),
            report_type: ReportType::Snapshot, query_id: first.clone(),
        });
        assert!(asked.contains(&format!("<id>{first}</id>")));
        assert_eq!(echoed_query_id(&asked), first, "and the answer is matched by it");

        assert_eq!(
            crate::control::xml::cancel_query(&first),
            format!("<ListOfCancelQueries><CancelQuery><id>{first}</id>\
                     </CancelQuery></ListOfCancelQueries>").replace(' ', ""),
            "the request and the withdrawal name the same thing",
        );
    }
}
