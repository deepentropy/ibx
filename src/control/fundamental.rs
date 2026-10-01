//! Fundamental data queries via the data farm connection.

use std::io::Read;

// FIX tags for fundamental data
pub const TAG_FUNDAMENTAL_XML: u32 = 6118;
pub const TAG_SUB_PROTOCOL: u32 = 6040;
pub const TAG_RAW_DATA_LENGTH: u32 = 95;
pub const TAG_RAW_DATA: u32 = 96;

/// A report type of the reference (#434): the API name, the name the
/// query asks for, the provider (the query id prefix) and the storage
/// directory of the provider's reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReportType {
    pub api_name: &'static str,
    pub wire_name: &'static str,
    pub provider: &'static str,
    pub storage_dir: Option<&'static str>,
}

/// The reference's report types, in its order.
pub const REPORT_TYPES: [ReportType; 5] = [
    ReportType { api_name: "ReportSnapshot", wire_name: "snapshot", provider: "Fundamentals", storage_dir: None },
    ReportType { api_name: "RESC", wire_name: "estimates", provider: "Fundamentals", storage_dir: None },
    ReportType { api_name: "CalendarReport", wire_name: "calendar", provider: "Fundamentals", storage_dir: None },
    ReportType { api_name: "ReportsFinSummary", wire_name: "finsum", provider: "Morningstar", storage_dir: Some("morningstar") },
    ReportType { api_name: "ReportsOwnership", wire_name: "ownstat", provider: "Morningstar", storage_dir: Some("morningstar") },
];

/// The type the reference gives a name it does not know: no name, asked
/// as it is (no local refusal).
pub const UNKNOWN_REPORT_TYPE: ReportType =
    ReportType { api_name: "", wire_name: "", provider: "Fundamentals", storage_dir: None };

impl ReportType {
    /// The type of an API report type text: its API name or its query
    /// name, case-sensitive, as the reference matches it.
    pub fn from_api(text: &str) -> ReportType {
        REPORT_TYPES.into_iter()
            .find(|t| t.api_name == text || t.wire_name == text)
            .unwrap_or(UNKNOWN_REPORT_TYPE)
    }

    pub fn provider(&self) -> &'static str {
        self.provider
    }

    pub fn report_type_str(&self) -> &'static str {
        self.wire_name
    }
}

/// Parameters for a fundamental data request.
#[derive(Debug, Clone)]
pub struct FundamentalRequest {
    /// Window id of the query, unique per request (ibx#428): the reply
    /// carries it back.
    pub window_id: String,
    pub con_id: u32,
    pub sec_type: &'static str,
    pub currency: &'static str,
    pub report_type: ReportType,
}

/// Parsed fundamental data response.
#[derive(Debug, Clone)]
pub struct FundamentalResponse {
    pub query_id: String,
    pub data: Vec<u8>,
}

/// Extract a simple XML tag value: `<tag>value</tag>` -> `value`.
fn extract_xml_tag<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(&xml[start..end])
}

/// Build the XML query for a fundamental data request, as the reference
/// writes it (#434).
pub fn build_fundamental_request_xml(req: &FundamentalRequest) -> String {
    let storage_dir = req.report_type.storage_dir
        .map(|d| format!("<storageDir>{}</storageDir>", d))
        .unwrap_or_default();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <ListOfQueries>\
         <FundamentalsQuery>\
         <id>{window_id}</id>\
         <contractID>{con_id}</contractID>\
         <exchange>RTRSFND</exchange>\
         <secType>{sec_type}</secType>\
         <source>API</source>\
         <needTotalValue>false</needTotalValue>\
         <wholeDays>false</wholeDays>\
         <delay>auto</delay>\
         <reportType>{report_type}</reportType>\
         {storage_dir}\
         <currency>{currency}</currency>\
         </FundamentalsQuery>\
         </ListOfQueries>",
        window_id = fundamental_query_id(&req.window_id),
        con_id = req.con_id,
        sec_type = req.sec_type,
        report_type = req.report_type.report_type_str(),
        currency = req.currency,
    )
}

/// The whole query id of a window id, which the reply and the cancel
/// repeat.
pub fn fundamental_query_id(window_id: &str) -> String {
    format!("{window_id};; COMPANY_FUNDAMENTALS;;0;;true;;0;;U")
}

/// The cancel of a fundamental data query still waiting for its reply.
pub fn build_fundamental_cancel_xml(window_id: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <ListOfCancelQueries><CancelQuery><id>{}</id></CancelQuery></ListOfCancelQueries>",
        fundamental_query_id(window_id),
    )
}

/// The server's error text of a fundamentals reply, when it has one.
pub fn fundamental_error_text(xml: &str) -> Option<String> {
    extract_xml_tag(xml, "errorText").map(|s| s.to_string()).filter(|s| !s.is_empty())
}

/// Text of every fundamentals failure, as the reference sends it with
/// code 430; the cause follows the sentence.
pub const FUNDAMENTALS_NOT_AVAILABLE: &str = "We are sorry, but fundamentals data for the security specified is not available.";

/// Extract the query ID from a `<FundResponse>` XML correlation tag.
pub fn parse_fundamental_response_id(xml: &str) -> Option<String> {
    if !xml.contains("<FundResponse>") {
        return None;
    }
    extract_xml_tag(xml, "id").map(|s| s.to_string())
}

/// Decompress gzip-compressed fundamental data (FIX tag 96).
pub fn decompress_fundamental_data(compressed: &[u8]) -> Option<String> {
    let mut decoder = flate2::read::GzDecoder::new(compressed);
    let mut result = String::new();
    decoder.read_to_string(&mut result).ok()?;
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;

    // #434: the reference's table, by API name or query name,
    // case-sensitive; an unknown name is asked with no type.
    #[test]
    fn report_type_mapping() {
        let t = ReportType::from_api("ReportSnapshot");
        assert_eq!((t.provider(), t.report_type_str(), t.storage_dir), ("Fundamentals", "snapshot", None));
        let t = ReportType::from_api("ReportsFinSummary");
        assert_eq!((t.provider(), t.report_type_str(), t.storage_dir), ("Morningstar", "finsum", Some("morningstar")));
        let t = ReportType::from_api("ReportsOwnership");
        assert_eq!((t.provider(), t.report_type_str(), t.storage_dir), ("Morningstar", "ownstat", Some("morningstar")));
        assert_eq!(ReportType::from_api("CalendarReport").report_type_str(), "calendar");
        assert_eq!(ReportType::from_api("RESC").report_type_str(), "estimates");
        assert_eq!(ReportType::from_api("finsum").api_name, "ReportsFinSummary");
        for unknown in ["ReportFinSummary", "ReportsFinStatements", "ReportRatios", "reportsnapshot", "BadName"] {
            assert_eq!(ReportType::from_api(unknown), UNKNOWN_REPORT_TYPE, "{unknown}");
        }

    }

    #[test]
    fn fundamental_request_xml_structure() {
        let req = FundamentalRequest {
            window_id: "Fundamentals1".to_string(),
            con_id: 265598,
            sec_type: "STK",
            currency: "USD",
            report_type: ReportType::from_api("ReportsFinSummary"),
        };
        let xml = build_fundamental_request_xml(&req);
        assert!(xml.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n"), "{}", xml);
        assert!(xml.contains("<reportType>finsum</reportType><storageDir>morningstar</storageDir><currency>USD</currency>"), "{}", xml);
        assert!(xml.contains("<ListOfQueries>"));
        assert!(xml.contains("<FundamentalsQuery>"));
        assert!(xml.contains("<contractID>265598</contractID>"));
        assert!(xml.contains("<exchange>RTRSFND</exchange>"));
        assert!(xml.contains("<secType>STK</secType>"));
        assert!(xml.contains("<currency>USD</currency>"));
        assert!(xml.contains("<source>API</source>"));
        // ibx#428: the request's own window id, in the reference form.
        assert!(xml.contains("<id>Fundamentals1;; COMPANY_FUNDAMENTALS;;0;;true;;0;;U</id>"), "{}", xml);
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

    #[test]
    fn parse_response_rejects_other() {
        assert!(parse_fundamental_response_id("<ResultSetBar>...</ResultSetBar>").is_none());
        assert!(parse_fundamental_response_id("not xml").is_none());
    }
}
