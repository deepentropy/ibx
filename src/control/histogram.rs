//! Histogram data queries via the data connection.
//!
//! Responses contain XML with Tick entries (price, size pairs).

use crate::protocol::fix;

use super::historical::TAG_HISTORICAL_XML;

/// Parameters for a histogram data request.
#[derive(Debug, Clone)]
pub struct HistogramRequest {
    /// Window id of the query, unique per request (ibx#428): the reply
    /// carries it back.
    pub window_id: String,
    pub con_id: u32,
    /// Security type of the API contract. Empty is a stock.
    pub sec_type: String,
    /// Exchange of the API contract. Empty is `SMART`.
    pub exchange: String,
    pub use_rth: bool,
    /// Time period, e.g. "1 week", "3 days", "1 month".
    pub period: String,
    /// End time for the histogram query (HMDS requires 2 of startTime/endTime/timeLength).
    pub end_time: String,
}

/// A single histogram entry (price level and count at that level).
#[derive(Debug, Clone, PartialEq)]
pub struct HistogramEntry {
    pub price: f64,
    pub count: i64,
}

/// Build the XML query for a histogram data request.
pub fn build_histogram_request_xml(req: &HistogramRequest) -> String {
    let rth = if req.use_rth { "true" } else { "false" };

    // Convert period: "1 week" → "1 w", "3 days" → "3 d"
    // HMDS requires lowercase duration units
    let time_length = convert_period(&req.period).to_lowercase();

    let exchange = super::historical::query_exchange(&req.exchange, &req.sec_type);
    let sec_type = super::historical::query_sec_type(&req.sec_type);
    // As the reference, the id does not carry useRTH.
    let id = format!(
        "{};;{}@{exchange} Histogram;;0;;true;;0;;U",
        req.window_id, req.con_id,
    );

    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <ListOfQueries>\
         <Query>\
         <id>{id}</id>\
         <useRTH>{rth}</useRTH>\
         <contractID>{con_id}</contractID>\
         <exchange>{exchange}</exchange>\
         <secType>{sec_type}</secType>\
         <type>HistogramData</type>\
         <data>Last</data>\
         <endTime>{end_time}</endTime>\
         <timeLength>{time_length}</timeLength>\
         <source>API</source>\
         <needTotalValue>false</needTotalValue>\
         <wholeDays>false</wholeDays>\
         <delay>auto</delay>\
         </Query>\
         </ListOfQueries>",
        con_id = req.con_id,
        end_time = req.end_time,
    )
}

/// Build a histogram query message.
pub fn build_histogram_fix_request(req: &HistogramRequest, seq: u32) -> Vec<u8> {
    let xml = build_histogram_request_xml(req);
    fix::fix_build(
        &[
            (fix::TAG_MSG_TYPE, "W"),
            (TAG_HISTORICAL_XML, &xml),
        ],
        seq,
    )
}

/// Extract a simple XML tag value: `<tag>value</tag>` → `value`.
fn extract_xml_tag<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(&xml[start..end])
}

/// Parse a histogram XML response into entries.
///
/// The response contains `<Tick>` elements with `<price>` and `<size>` children
/// inside an `<Events>` block.
pub fn parse_histogram_response(xml: &str) -> Option<Vec<HistogramEntry>> {
    if !xml.contains("<ResultSetHistogram>") {
        return None;
    }

    let mut entries = Vec::new();
    let mut search_start = 0;

    while let Some(tick_start) = xml[search_start..].find("<Tick>") {
        let abs_start = search_start + tick_start;
        let tick_end = match xml[abs_start..].find("</Tick>") {
            Some(e) => abs_start + e + 7,
            None => break,
        };
        let tick_xml = &xml[abs_start..tick_end];

        let price = extract_xml_tag(tick_xml, "price")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);
        let count = extract_xml_tag(tick_xml, "size")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);

        entries.push(HistogramEntry { price, count });
        search_start = tick_end;
    }

    Some(entries)
}

/// Convert human-readable period to HMDS timeLength format.
/// HMDS accepts: S (seconds), D (days), M (months), Y (years) — NOT W (weeks).
/// Weeks are converted to days: "1 week" → "7 D", "2 weeks" → "14 D".
fn convert_period(period: &str) -> String {
    let parts: Vec<&str> = period.trim().split_whitespace().collect();
    if parts.len() != 2 {
        return period.to_string();
    }
    let num: u32 = match parts[0].parse() {
        Ok(n) => n,
        Err(_) => return period.to_string(),
    };
    match parts[1].to_lowercase().as_str() {
        "second" | "seconds" | "secs" | "sec" | "s" => format!("{} S", num),
        "day" | "days" | "d" => format!("{} D", num),
        "week" | "weeks" | "w" => format!("{} D", num * 7),
        "month" | "months" | "m" => format!("{} M", num),
        "year" | "years" | "y" => format!("{} Y", num),
        _ => period.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convert_period_variants() {
        assert_eq!(convert_period("1 week"), "7 D");
        assert_eq!(convert_period("2 weeks"), "14 D");
        assert_eq!(convert_period("3 days"), "3 D");
        assert_eq!(convert_period("2 months"), "2 M");
        assert_eq!(convert_period("1 year"), "1 Y");
        assert_eq!(convert_period("30 seconds"), "30 S");
        // passthrough for unknown
        assert_eq!(convert_period("foo"), "foo");
    }

    #[test]
    fn build_xml_structure() {
        let req = HistogramRequest {
            window_id: "histogramQuery0".to_string(),
            con_id: 265598,
            sec_type: "STK".to_string(),
            exchange: "SMART".to_string(),
            use_rth: true,
            period: "1 week".to_string(),
            end_time: "20260320-21:00:00".to_string(),
        };
        let xml = build_histogram_request_xml(&req);
        assert!(xml.contains("<type>HistogramData</type>"));
        assert!(xml.contains("<contractID>265598</contractID>"));
        assert!(xml.contains("<useRTH>true</useRTH>"));
        assert!(xml.contains("<timeLength>7 d</timeLength>"));
        assert!(xml.contains("<data>Last</data>"));
        assert!(xml.contains("<exchange>BEST</exchange>"));
        assert!(xml.contains("<secType>STK</secType>"));
        assert!(xml.contains("<endTime>20260320-21:00:00</endTime>"));
        assert!(xml.contains("<id>histogramQuery0;;265598@BEST Histogram;;0;;true;;0;;U</id>"), "{}", xml);
        // No <step> tag
        assert!(!xml.contains("<step>"));
    }

    #[test]
    fn build_xml_rth_false() {
        let req = HistogramRequest {
            window_id: "histogramQuery0".to_string(),
            con_id: 100,
            sec_type: String::new(),
            exchange: String::new(),
            use_rth: false,
            period: "3 days".to_string(),
            end_time: "20260320-21:00:00".to_string(),
        };
        let xml = build_histogram_request_xml(&req);
        assert!(xml.contains("<useRTH>false</useRTH>"));
        assert!(xml.contains("<timeLength>3 d</timeLength>"));
    }

    #[test]
    fn build_fix_request() {
        let req = HistogramRequest {
            window_id: "histogramQuery0".to_string(),
            con_id: 265598,
            sec_type: "STK".to_string(),
            exchange: "SMART".to_string(),
            use_rth: true,
            period: "1 week".to_string(),
            end_time: "20260320-21:00:00".to_string(),
        };
        let msg = build_histogram_fix_request(&req, 1);
        let tags = fix::fix_parse(&msg);
        assert_eq!(tags[&fix::TAG_MSG_TYPE], "W");
        assert!(tags[&TAG_HISTORICAL_XML].contains("<type>HistogramData</type>"));
    }

    #[test]
    fn parse_histogram_basic() {
        let xml = r#"<ResultSetHistogram>
            <id>histogramQuery;;265598@BEST Histogram;;0;;true;;0;;U</id>
            <eoq>true</eoq>
            <Events>
                <Tick><price>270.50</price><size>1500</size></Tick>
                <Tick><price>271.00</price><size>2300</size></Tick>
                <Tick><price>269.75</price><size>800</size></Tick>
            </Events>
        </ResultSetHistogram>"#;

        let entries = parse_histogram_response(xml).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].price, 270.50);
        assert_eq!(entries[0].count, 1500);
        assert_eq!(entries[1].price, 271.00);
        assert_eq!(entries[1].count, 2300);
        assert_eq!(entries[2].price, 269.75);
        assert_eq!(entries[2].count, 800);
    }

    #[test]
    fn parse_histogram_empty() {
        let xml = r#"<ResultSetHistogram>
            <id>test</id>
            <eoq>true</eoq>
            <Events></Events>
        </ResultSetHistogram>"#;
        // Valid histogram response with no data → empty vec
        let entries = parse_histogram_response(xml).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn parse_histogram_rejects_non_histogram() {
        assert!(parse_histogram_response("<ResultSetBar>...</ResultSetBar>").is_none());
        assert!(parse_histogram_response("not xml at all").is_none());
    }
}
