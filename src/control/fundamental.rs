//! Fundamental data queries via the data farm connection.

use std::io::Read;

// FIX tags for fundamental data
pub const TAG_FUNDAMENTAL_XML: u32 = 6118;
pub const TAG_SUB_PROTOCOL: u32 = 6040;
pub const TAG_RAW_DATA_LENGTH: u32 = 95;
pub const TAG_RAW_DATA: u32 = 96;

/// Report types for fundamental data queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportType {
    Snapshot,
    FinancialSummary,
    FinancialStatements,
}

impl ReportType {
    pub fn provider(&self) -> &'static str {
        match self {
            Self::Snapshot => "Fundamentals",
            Self::FinancialSummary => "Morningstar",
            Self::FinancialStatements => "Morningstar",
        }
    }

    pub fn report_type_str(&self) -> &'static str {
        match self {
            Self::Snapshot => "snapshot",
            Self::FinancialSummary => "finsum",
            Self::FinancialStatements => "finstat",
        }
    }
}

/// Parameters for a fundamental data request.
#[derive(Debug, Clone)]
pub struct FundamentalRequest {
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

/// Build the XML query for a fundamental data request.
pub fn build_fundamental_request_xml(req: &FundamentalRequest) -> String {
    format!(
        "<ListOfQueries>\
         <FundamentalsQuery>\
         <id>COMPANY_FUNDAMENTALS</id>\
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

    #[test]
    fn report_type_mapping() {
        assert_eq!(ReportType::Snapshot.provider(), "Fundamentals");
        assert_eq!(ReportType::Snapshot.report_type_str(), "snapshot");

        assert_eq!(ReportType::FinancialSummary.provider(), "Morningstar");
        assert_eq!(ReportType::FinancialSummary.report_type_str(), "finsum");

        assert_eq!(ReportType::FinancialStatements.provider(), "Morningstar");
        assert_eq!(ReportType::FinancialStatements.report_type_str(), "finstat");
    }

    #[test]
    fn fundamental_request_xml_structure() {
        let req = FundamentalRequest {
            con_id: 265598,
            sec_type: "STK",
            currency: "USD",
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

    #[test]
    fn parse_response_rejects_other() {
        assert!(parse_fundamental_response_id("<ResultSetBar>...</ResultSetBar>").is_none());
        assert!(parse_fundamental_response_id("not xml").is_none());
    }
}
