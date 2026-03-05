//! Historical data queries via ushmds connection (XML-in-FIX tag 6118).
//!
//! Requests use FIX 35=W with XML query in tag 6118.
//! Responses contain XML ResultSetBar with OHLCV bar data.
//! Cancellations use FIX 35=Z with XML in tag 6118.

use crate::protocol::fix;

// FIX tags for historical data
pub const TAG_HISTORICAL_XML: u32 = 6118;

/// Bar data types for historical queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BarDataType {
    Trades,
    Midpoint,
    Bid,
    Ask,
    BidAsk,
    AdjustedLast,
    HistoricalVolatility,
    ImpliedVolatility,
}

impl BarDataType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Trades => "Last",
            Self::Midpoint => "Midpoint",
            Self::Bid => "Bid",
            Self::Ask => "Ask",
            Self::BidAsk => "BidAsk",
            Self::AdjustedLast => "AdjustedLast",
            Self::HistoricalVolatility => "HV",
            Self::ImpliedVolatility => "IV",
        }
    }
}

/// Bar size / time step for historical queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BarSize {
    Sec1,
    Sec5,
    Sec10,
    Sec15,
    Sec30,
    Min1,
    Min2,
    Min3,
    Min5,
    Min10,
    Min15,
    Min20,
    Min30,
    Hour1,
    Hour2,
    Hour3,
    Hour4,
    Hour8,
    Day1,
    Week1,
    Month1,
}

impl BarSize {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Sec1 => "1 secs",
            Self::Sec5 => "5 secs",
            Self::Sec10 => "10 secs",
            Self::Sec15 => "15 secs",
            Self::Sec30 => "30 secs",
            Self::Min1 => "1 min",
            Self::Min2 => "2 mins",
            Self::Min3 => "3 mins",
            Self::Min5 => "5 mins",
            Self::Min10 => "10 mins",
            Self::Min15 => "15 mins",
            Self::Min20 => "20 mins",
            Self::Min30 => "30 mins",
            Self::Hour1 => "1 hour",
            Self::Hour2 => "2 hours",
            Self::Hour3 => "3 hours",
            Self::Hour4 => "4 hours",
            Self::Hour8 => "8 hours",
            Self::Day1 => "1 day",
            Self::Week1 => "1 week",
            Self::Month1 => "1 month",
        }
    }
}

/// Parameters for a historical data request.
#[derive(Debug, Clone)]
pub struct HistoricalRequest {
    pub query_id: String,
    pub con_id: u32,
    pub symbol: String,
    pub sec_type: &'static str,
    pub exchange: &'static str,
    pub data_type: BarDataType,
    pub end_time: String,
    pub duration: String,
    pub bar_size: BarSize,
    pub use_rth: bool,
}

/// A single historical OHLCV bar parsed from XML.
#[derive(Debug, Clone, PartialEq)]
pub struct HistoricalBar {
    pub time: String,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: i64,
    pub wap: f64,
    pub count: u32,
}

/// Parsed historical data response.
#[derive(Debug, Clone)]
pub struct HistoricalResponse {
    pub query_id: String,
    pub timezone: String,
    pub bars: Vec<HistoricalBar>,
    pub is_complete: bool,
}

/// Build the XML query for a historical bar data request.
pub fn build_query_xml(req: &HistoricalRequest) -> String {
    let exchange = match req.exchange {
        "SMART" => "BEST",
        e => e,
    };
    let rth = if req.use_rth { "true" } else { "false" };

    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <ListOfQueries>\
         <Query>\
         <id>{id}</id>\
         <useRTH>{rth}</useRTH>\
         <contractID>{con_id}</contractID>\
         <exchange>{exchange}</exchange>\
         <secType>{sec_type}</secType>\
         <expired>no</expired>\
         <type>BarData</type>\
         <data>{data}</data>\
         <endTime>{end}</endTime>\
         <timeLength>{dur}</timeLength>\
         <step>{step}</step>\
         <source>API</source>\
         <needTotalValue>false</needTotalValue>\
         <wholeDays>false</wholeDays>\
         <delay>auto</delay>\
         </Query>\
         </ListOfQueries>",
        id = req.query_id,
        con_id = req.con_id,
        sec_type = req.sec_type,
        data = req.data_type.as_str(),
        end = req.end_time,
        dur = req.duration,
        step = req.bar_size.as_str(),
    )
}

/// Build a FIX 35=W message containing the historical query XML.
pub fn build_historical_request(req: &HistoricalRequest, seq: u32) -> Vec<u8> {
    let xml = build_query_xml(req);
    fix::fix_build(
        &[
            (fix::TAG_MSG_TYPE, "W"),
            (TAG_HISTORICAL_XML, &xml),
        ],
        seq,
    )
}

/// Build a FIX 35=Z cancellation for a real-time bar subscription.
pub fn build_cancel_request(ticker_id: &str, seq: u32) -> Vec<u8> {
    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <ListOfCancelQueries>\
         <CancelQuery>\
         <id>ticker:{tid}</id>\
         </CancelQuery>\
         </ListOfCancelQueries>",
        tid = ticker_id,
    );
    fix::fix_build(
        &[
            (fix::TAG_MSG_TYPE, "Z"),
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

/// Parse a ResultSetBar XML response into bars.
pub fn parse_bar_response(xml: &str) -> Option<HistoricalResponse> {
    // Check for ResultSetBar
    if !xml.contains("<ResultSetBar>") {
        return None;
    }

    let query_id = extract_xml_tag(xml, "id").unwrap_or("").to_string();
    let timezone = extract_xml_tag(xml, "tz").unwrap_or("").to_string();
    let is_complete = extract_xml_tag(xml, "eoq").unwrap_or("false") == "true";

    let mut bars = Vec::new();
    let mut search_start = 0;

    while let Some(bar_start) = xml[search_start..].find("<Bar>") {
        let abs_start = search_start + bar_start;
        let bar_end = match xml[abs_start..].find("</Bar>") {
            Some(e) => abs_start + e + 6,
            None => break,
        };
        let bar_xml = &xml[abs_start..bar_end];

        let bar = HistoricalBar {
            time: extract_xml_tag(bar_xml, "time").unwrap_or("").to_string(),
            open: extract_xml_tag(bar_xml, "open")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0.0),
            high: extract_xml_tag(bar_xml, "high")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0.0),
            low: extract_xml_tag(bar_xml, "low")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0.0),
            close: extract_xml_tag(bar_xml, "close")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0.0),
            volume: extract_xml_tag(bar_xml, "volume")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0),
            wap: extract_xml_tag(bar_xml, "weightedAvg")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0.0),
            count: extract_xml_tag(bar_xml, "count")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0),
        };
        bars.push(bar);
        search_start = bar_end;
    }

    Some(HistoricalResponse {
        query_id,
        timezone,
        bars,
        is_complete,
    })
}

/// Extract the ticker ID from a ResultSetTickerId response (for real-time bar subscriptions).
pub fn parse_ticker_id(xml: &str) -> Option<String> {
    if !xml.contains("<ResultSetTickerId>") {
        return None;
    }
    extract_xml_tag(xml, "tickerId").map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bar_data_type_strings() {
        assert_eq!(BarDataType::Trades.as_str(), "Last");
        assert_eq!(BarDataType::Midpoint.as_str(), "Midpoint");
        assert_eq!(BarDataType::BidAsk.as_str(), "BidAsk");
    }

    #[test]
    fn bar_size_strings() {
        assert_eq!(BarSize::Min5.as_str(), "5 mins");
        assert_eq!(BarSize::Hour1.as_str(), "1 hour");
        assert_eq!(BarSize::Day1.as_str(), "1 day");
    }

    #[test]
    fn build_query_xml_structure() {
        let req = HistoricalRequest {
            query_id: "q1".to_string(),
            con_id: 265598,
            symbol: "AAPL".to_string(),
            sec_type: "CS",
            exchange: "SMART",
            data_type: BarDataType::Trades,
            end_time: "20260228-15:00:00".to_string(),
            duration: "1 d".to_string(),
            bar_size: BarSize::Min5,
            use_rth: true,
        };
        let xml = build_query_xml(&req);
        assert!(xml.contains("<id>q1</id>"));
        assert!(xml.contains("<contractID>265598</contractID>"));
        assert!(xml.contains("<exchange>BEST</exchange>")); // SMART→BEST
        assert!(xml.contains("<data>Last</data>"));
        assert!(xml.contains("<step>5 mins</step>"));
        assert!(xml.contains("<useRTH>true</useRTH>"));
        assert!(xml.contains("<timeLength>1 d</timeLength>"));
    }

    #[test]
    fn build_fix_request() {
        let req = HistoricalRequest {
            query_id: "q1".to_string(),
            con_id: 265598,
            symbol: "AAPL".to_string(),
            sec_type: "CS",
            exchange: "SMART",
            data_type: BarDataType::Trades,
            end_time: "20260228-15:00:00".to_string(),
            duration: "1 d".to_string(),
            bar_size: BarSize::Min5,
            use_rth: true,
        };
        let msg = build_historical_request(&req, 1);
        let tags = fix::fix_parse(&msg);
        assert_eq!(tags[&fix::TAG_MSG_TYPE], "W");
        assert!(tags[&TAG_HISTORICAL_XML].contains("<ListOfQueries>"));
    }

    #[test]
    fn cancel_request_structure() {
        let msg = super::build_cancel_request("12345", 1);
        let tags = fix::fix_parse(&msg);
        assert_eq!(tags[&fix::TAG_MSG_TYPE], "Z");
        assert!(tags[&TAG_HISTORICAL_XML].contains("ticker:12345"));
    }

    #[test]
    fn parse_bar_response_basic() {
        let xml = r#"<ResultSetBar>
            <id>q1</id>
            <eoq>true</eoq>
            <tz>US/Eastern</tz>
            <Events>
                <Open><time>20260227-14:30:00</time></Open>
                <Bar>
                    <time>20260227-14:30:00</time>
                    <open>272.77</open>
                    <close>269.47</close>
                    <high>272.81</high>
                    <low>269.2</low>
                    <weightedAvg>270.998</weightedAvg>
                    <volume>1411775</volume>
                    <count>5165</count>
                </Bar>
                <Bar>
                    <time>20260227-14:35:00</time>
                    <open>269.48</open>
                    <close>270.10</close>
                    <high>270.50</high>
                    <low>269.30</low>
                    <weightedAvg>269.90</weightedAvg>
                    <volume>500000</volume>
                    <count>2000</count>
                </Bar>
                <Close><time>20260227-21:00:00</time></Close>
            </Events>
        </ResultSetBar>"#;

        let resp = parse_bar_response(xml).unwrap();
        assert_eq!(resp.query_id, "q1");
        assert_eq!(resp.timezone, "US/Eastern");
        assert!(resp.is_complete);
        assert_eq!(resp.bars.len(), 2);

        let bar = &resp.bars[0];
        assert_eq!(bar.time, "20260227-14:30:00");
        assert_eq!(bar.open, 272.77);
        assert_eq!(bar.high, 272.81);
        assert_eq!(bar.low, 269.2);
        assert_eq!(bar.close, 269.47);
        assert_eq!(bar.volume, 1411775);
        assert_eq!(bar.wap, 270.998);
        assert_eq!(bar.count, 5165);

        let bar2 = &resp.bars[1];
        assert_eq!(bar2.time, "20260227-14:35:00");
        assert_eq!(bar2.close, 270.10);
    }

    #[test]
    fn parse_bar_response_incomplete() {
        let xml = r#"<ResultSetBar>
            <id>q2</id>
            <eoq>false</eoq>
            <tz>US/Eastern</tz>
            <Events>
                <Bar>
                    <time>20260227-14:30:00</time>
                    <open>100.0</open>
                    <close>101.0</close>
                    <high>102.0</high>
                    <low>99.0</low>
                    <volume>1000</volume>
                    <count>10</count>
                </Bar>
            </Events>
        </ResultSetBar>"#;

        let resp = parse_bar_response(xml).unwrap();
        assert!(!resp.is_complete);
        assert_eq!(resp.bars.len(), 1);
    }

    #[test]
    fn parse_bar_response_rejects_non_bar() {
        assert!(parse_bar_response("<ResultSetTickerId>...").is_none());
        assert!(parse_bar_response("not xml at all").is_none());
    }

    #[test]
    fn parse_ticker_id() {
        let xml = r#"<ResultSetTickerId>
            <id>q1</id>
            <tickerId>42</tickerId>
        </ResultSetTickerId>"#;
        assert_eq!(super::parse_ticker_id(xml), Some("42".to_string()));
    }

    #[test]
    fn parse_ticker_id_rejects_other() {
        assert!(super::parse_ticker_id("<ResultSetBar>...</ResultSetBar>").is_none());
    }

    #[test]
    fn extract_xml_tag_basic() {
        assert_eq!(extract_xml_tag("<a>hello</a>", "a"), Some("hello"));
        assert_eq!(extract_xml_tag("<x>123</x>", "x"), Some("123"));
        assert_eq!(extract_xml_tag("<x>123</x>", "y"), None);
    }
}
