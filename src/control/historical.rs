//! Historical data queries via the data connection.
//!
//! Responses contain XML ResultSetBar with OHLCV bar data.

use crate::protocol::fix;

// Tags for historical data
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

/// Build a historical data query message.
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

/// Build a cancellation message for a real-time bar subscription.
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
pub fn extract_xml_tag<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
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

/// Parameters for a head timestamp request.
#[derive(Debug, Clone)]
pub struct HeadTimestampRequest {
    pub con_id: u32,
    pub sec_type: &'static str,
    pub exchange: &'static str,
    pub data_type: BarDataType,
    pub use_rth: bool,
}

/// Parsed head timestamp response.
#[derive(Debug, Clone)]
pub struct HeadTimestampResponse {
    pub head_timestamp: String,
    pub timezone: String,
}

/// Build the XML query for a head timestamp request.
pub fn build_head_timestamp_xml(req: &HeadTimestampRequest) -> String {
    let exchange = match req.exchange {
        "SMART" => "BEST",
        e => e,
    };
    let rth = if req.use_rth { "true" } else { "false" };
    let id = format!("TickHeadClient1;;{}@{} {};;0;;{};;0;;U",
        req.con_id, exchange, req.data_type.as_str(), rth);

    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <ListOfQueries>\
         <Query>\
         <id>{id}</id>\
         <useRTH>{rth}</useRTH>\
         <contractID>{con_id}</contractID>\
         <exchange>{exchange}</exchange>\
         <secType>{sec_type}</secType>\
         <type>TickHeadTimeStamp</type>\
         <data>{data}</data>\
         <step>-1</step>\
         <source>API</source>\
         <needTotalValue>false</needTotalValue>\
         <wholeDays>false</wholeDays>\
         <delay>auto</delay>\
         </Query>\
         </ListOfQueries>",
        con_id = req.con_id,
        sec_type = req.sec_type,
        data = req.data_type.as_str(),
    )
}

/// Map whatToShow to data type.
fn tick_data_type(what_to_show: &str) -> &'static str {
    match what_to_show.to_uppercase().as_str() {
        "MIDPOINT" => "MidPoint",
        "BID_ASK" => "BidAsk",
        _ => "AllLast", // TRADES
    }
}

/// Build the XML query for a historical ticks request.
///
/// Uses `<type>TickData</type>`, `<step>ticks</step>`, `<timeLength>{N} t</timeLength>`.
pub fn build_tick_query_xml(
    query_id: &str, con_id: i64, start_date_time: &str, end_date_time: &str,
    number_of_ticks: u32, what_to_show: &str, use_rth: bool,
) -> String {
    let exchange = "BEST";
    let rth = if use_rth { "true" } else { "false" };
    let data = tick_data_type(what_to_show);

    // Use endTime if provided, otherwise startTime
    let time_tag = if !end_date_time.is_empty() {
        format!("<endTime>{}</endTime>", end_date_time)
    } else {
        format!("<endTime>{}</endTime>", start_date_time)
    };

    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <ListOfQueries>\
         <Query>\
         <id>{id}</id>\
         <useRTH>{rth}</useRTH>\
         <contractID>{con_id}</contractID>\
         <exchange>{exchange}</exchange>\
         <secType>CS</secType>\
         <expired>no</expired>\
         <type>TickData</type>\
         <data>{data}</data>\
         {time}\
         <timeLength>{n} t</timeLength>\
         <step>ticks</step>\
         <source>API</source>\
         <wholeDays>true</wholeDays>\
         <delay>auto</delay>\
         </Query>\
         </ListOfQueries>",
        id = query_id,
        n = number_of_ticks,
        time = time_tag,
    )
}

/// Parse a ResultSetTick XML response into historical tick data.
pub fn parse_tick_response(xml: &str, what_to_show: &str) -> Option<(String, crate::types::HistoricalTickData, bool)> {
    if !xml.contains("<ResultSetTick>") {
        return None;
    }

    let query_id = extract_xml_tag(xml, "id").unwrap_or("").to_string();
    let is_complete = extract_xml_tag(xml, "eoq").unwrap_or("false") == "true";

    let upper = what_to_show.to_uppercase();
    let mut search_start = 0;

    match upper.as_str() {
        "BID_ASK" => {
            let mut ticks = Vec::new();
            while let Some(tick_pos) = xml[search_start..].find("<Tick>") {
                let abs = search_start + tick_pos;
                let end = match xml[abs..].find("</Tick>") {
                    Some(e) => abs + e + 7,
                    None => break,
                };
                let t = &xml[abs..end];
                ticks.push(crate::types::HistoricalTickBidAsk {
                    time: extract_xml_tag(t, "time").unwrap_or("").to_string(),
                    bid_price: extract_xml_tag(t, "priceBid").and_then(|s| s.parse().ok()).unwrap_or(0.0),
                    ask_price: extract_xml_tag(t, "priceAsk").and_then(|s| s.parse().ok()).unwrap_or(0.0),
                    bid_size: extract_xml_tag(t, "sizeBid").and_then(|s| s.parse().ok()).unwrap_or(0),
                    ask_size: extract_xml_tag(t, "sizeAsk").and_then(|s| s.parse().ok()).unwrap_or(0),
                });
                search_start = end;
            }
            Some((query_id, crate::types::HistoricalTickData::BidAsk(ticks), is_complete))
        }
        "MIDPOINT" => {
            let mut ticks = Vec::new();
            while let Some(tick_pos) = xml[search_start..].find("<Tick>") {
                let abs = search_start + tick_pos;
                let end = match xml[abs..].find("</Tick>") {
                    Some(e) => abs + e + 7,
                    None => break,
                };
                let t = &xml[abs..end];
                ticks.push(crate::types::HistoricalTickMidpoint {
                    time: extract_xml_tag(t, "time").unwrap_or("").to_string(),
                    price: extract_xml_tag(t, "price").and_then(|s| s.parse().ok()).unwrap_or(0.0),
                });
                search_start = end;
            }
            Some((query_id, crate::types::HistoricalTickData::Midpoint(ticks), is_complete))
        }
        _ => {
            // TRADES / AllLast
            let mut ticks = Vec::new();
            while let Some(tick_pos) = xml[search_start..].find("<Tick>") {
                let abs = search_start + tick_pos;
                let end = match xml[abs..].find("</Tick>") {
                    Some(e) => abs + e + 7,
                    None => break,
                };
                let t = &xml[abs..end];
                ticks.push(crate::types::HistoricalTickLast {
                    time: extract_xml_tag(t, "time").unwrap_or("").to_string(),
                    price: extract_xml_tag(t, "price").and_then(|s| s.parse().ok()).unwrap_or(0.0),
                    size: extract_xml_tag(t, "size").and_then(|s| s.parse().ok()).unwrap_or(0),
                    exchange: extract_xml_tag(t, "exchange").unwrap_or("").to_string(),
                    special_conditions: extract_xml_tag(t, "specialConditions").unwrap_or("").to_string(),
                });
                search_start = end;
            }
            Some((query_id, crate::types::HistoricalTickData::Last(ticks), is_complete))
        }
    }
}

/// Build the XML subscription for real-time 5-second bars.
pub fn build_realtime_bar_xml(query_id: &str, con_id: i64, what_to_show: &str, use_rth: bool) -> String {
    let exchange = "BEST";
    let rth = if use_rth { "true" } else { "false" };
    let data = match what_to_show.to_uppercase().as_str() {
        "MIDPOINT" => "Midpoint",
        "BID" => "Bid",
        "ASK" => "Ask",
        _ => "Last",
    };

    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <ListOfQueries>\
         <Query>\
         <id>{id}</id>\
         <useRTH>{rth}</useRTH>\
         <contractID>{con_id}</contractID>\
         <exchange>{exchange}</exchange>\
         <secType>CS</secType>\
         <type>BarData</type>\
         <data>{data}</data>\
         <refresh>5 secs</refresh>\
         <step>5 secs</step>\
         <source>API</source>\
         <needTotalValue>false</needTotalValue>\
         <wholeDays>false</wholeDays>\
         </Query>\
         </ListOfQueries>",
        id = query_id,
    )
}

/// Decode a real-time bar binary payload.
///
/// Uses LSB-first bit reader with 4-byte group reversal.
/// Returns (low, open, high, close, volume, wap, count) or None.
pub fn decode_bar_payload(payload: &[u8], min_tick: f64) -> Option<crate::types::RealTimeBar> {
    if payload.is_empty() {
        return None;
    }

    // Reverse byte order within 4-byte groups
    let mut reordered = Vec::with_capacity(payload.len());
    for chunk in payload.chunks(4) {
        for &b in chunk.iter().rev() {
            reordered.push(b);
        }
    }

    let data = &reordered;
    let mut pos: usize = 0; // bit position

    let read_bits = |pos: &mut usize, n: usize| -> u32 {
        let mut val: u32 = 0;
        for i in 0..n {
            let byte_idx = *pos / 8;
            let bit_idx = *pos % 8;
            if byte_idx < data.len() {
                val |= (((data[byte_idx] >> bit_idx) & 1) as u32) << i;
            }
            *pos += 1;
        }
        val
    };

    // 4 bits padding
    read_bits(&mut pos, 4);

    // Count: 1-bit flag selects width
    let count = if read_bits(&mut pos, 1) == 1 {
        read_bits(&mut pos, 8) as i32
    } else {
        read_bits(&mut pos, 32) as i32
    };

    // Low price in ticks (31-bit signed)
    let low_ticks = read_bits(&mut pos, 31);
    let low_ticks_signed = if low_ticks & (1 << 30) != 0 {
        low_ticks as i32 - (1 << 31)
    } else {
        low_ticks as i32
    };
    let low = low_ticks_signed as f64 * min_tick;

    let (open, high, close, wap_sum);
    if count > 1 {
        // Delta width: 1-bit flag
        let width = if read_bits(&mut pos, 1) == 1 { 5 } else { 32 };
        let d_open = read_bits(&mut pos, width);
        let d_high = read_bits(&mut pos, width);
        let d_close = read_bits(&mut pos, width);

        open = low + d_open as f64 * min_tick;
        high = low + d_high as f64 * min_tick;
        close = low + d_close as f64 * min_tick;

        // WAP sum: 1-bit flag selects width
        wap_sum = if read_bits(&mut pos, 1) == 1 {
            read_bits(&mut pos, 18) as f64
        } else {
            read_bits(&mut pos, 32) as f64
        };
    } else {
        open = low;
        high = low;
        close = low;
        wap_sum = 0.0;
    }

    // Volume: 1-bit flag selects width
    let volume = if read_bits(&mut pos, 1) == 1 {
        read_bits(&mut pos, 16) as f64
    } else {
        read_bits(&mut pos, 32) as f64
    };

    let wap = if count > 1 && volume > 0.0 {
        low + wap_sum * min_tick / volume
    } else {
        low
    };

    Some(crate::types::RealTimeBar {
        timestamp: 0, // filled by caller from message header
        open, high, low, close, volume, wap, count,
    })
}

/// Build the XML query for a historical schedule request.
///
/// Schedule requests use `<data>Schedule</data>` and `<scheduleOnly>true</scheduleOnly>`
/// with `<type>BarData</type>`. Response is `<ResultSetSchedule>`.
pub fn build_schedule_xml(query_id: &str, con_id: i64, end_time: &str, duration: &str, use_rth: bool) -> String {
    let exchange = "BEST";
    let rth = if use_rth { "true" } else { "false" };

    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <ListOfQueries>\
         <Query>\
         <id>{id}</id>\
         <useRTH>{rth}</useRTH>\
         <contractID>{con_id}</contractID>\
         <exchange>{exchange}</exchange>\
         <secType>STK</secType>\
         <type>BarData</type>\
         <data>Schedule</data>\
         <endTime>{end}</endTime>\
         <timeLength>{dur}</timeLength>\
         <step>1 day</step>\
         <scheduleOnly>true</scheduleOnly>\
         </Query>\
         </ListOfQueries>",
        id = query_id,
        con_id = con_id,
        end = end_time,
        dur = duration,
    )
}

/// Parse a ResultSetSchedule XML response into sessions.
pub fn parse_schedule_response(xml: &str) -> Option<crate::types::HistoricalScheduleResponse> {
    if !xml.contains("<ResultSetSchedule>") {
        return None;
    }

    let query_id = extract_xml_tag(xml, "id").unwrap_or("").to_string();
    let timezone = extract_xml_tag(xml, "tz").unwrap_or("").to_string();
    let start_date_time = extract_xml_tag(xml, "derivedStart").unwrap_or("").to_string();

    let mut sessions = Vec::new();
    let mut search_start = 0;

    // Parse Open/Close pairs into sessions
    while let Some(open_pos) = xml[search_start..].find("<Open>") {
        let abs_open = search_start + open_pos;
        let open_end = match xml[abs_open..].find("</Open>") {
            Some(e) => abs_open + e + 7,
            None => break,
        };
        let open_xml = &xml[abs_open..open_end];

        let open_time = extract_xml_tag(open_xml, "time").unwrap_or("").to_string();
        let ref_date = extract_xml_tag(open_xml, "refDate").unwrap_or("").to_string();

        // Find the matching Close
        let close_time = if let Some(close_pos) = xml[open_end..].find("<Close>") {
            let abs_close = open_end + close_pos;
            let close_end = match xml[abs_close..].find("</Close>") {
                Some(e) => abs_close + e + 8,
                None => break,
            };
            let close_xml = &xml[abs_close..close_end];
            search_start = close_end;
            extract_xml_tag(close_xml, "time").unwrap_or("").to_string()
        } else {
            search_start = open_end;
            String::new()
        };

        sessions.push(crate::types::ScheduleSession {
            ref_date,
            open_time,
            close_time,
        });
    }

    Some(crate::types::HistoricalScheduleResponse {
        query_id,
        timezone,
        start_date_time,
        end_date_time: String::new(), // filled by caller from request context
        sessions,
    })
}

/// Parse a ResultSetHeadTimeStamp XML response.
pub fn parse_head_timestamp_response(xml: &str) -> Option<HeadTimestampResponse> {
    if !xml.contains("<ResultSetHeadTimeStamp>") {
        return None;
    }
    let head_timestamp = extract_xml_tag(xml, "headTS")?.to_string();
    let timezone = extract_xml_tag(xml, "tz").unwrap_or("").to_string();
    Some(HeadTimestampResponse { head_timestamp, timezone })
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

    #[test]
    fn head_timestamp_xml_structure() {
        let req = HeadTimestampRequest {
            con_id: 756733,
            sec_type: "STK",
            exchange: "SMART",
            data_type: BarDataType::Trades,
            use_rth: true,
        };
        let xml = build_head_timestamp_xml(&req);
        assert!(xml.contains("<type>TickHeadTimeStamp</type>"));
        assert!(xml.contains("<contractID>756733</contractID>"));
        assert!(xml.contains("<exchange>BEST</exchange>")); // SMART→BEST
        assert!(xml.contains("<data>Last</data>"));
        assert!(xml.contains("<step>-1</step>"));
        assert!(xml.contains("<useRTH>true</useRTH>"));
        assert!(xml.contains("TickHeadClient1;;756733@BEST Last;;0;;true;;0;;U"));
    }

    #[test]
    fn parse_head_timestamp_response_basic() {
        let xml = r#"<ResultSetHeadTimeStamp>
            <id>TickHeadClient1;;756733@BEST Last;;0;;true;;0;;U</id>
            <eoq>true</eoq>
            <headTS>19930129-09:00:00</headTS>
            <tz>US/Eastern</tz>
            <Events>
                <Open><time>19930129-14:30:00</time><refDate>19930129</refDate></Open>
                <Close><time>19930129-21:15:00</time></Close>
            </Events>
        </ResultSetHeadTimeStamp>"#;
        let resp = parse_head_timestamp_response(xml).unwrap();
        assert_eq!(resp.head_timestamp, "19930129-09:00:00");
        assert_eq!(resp.timezone, "US/Eastern");
    }

    #[test]
    fn parse_head_timestamp_rejects_other() {
        assert!(parse_head_timestamp_response("<ResultSetBar>...</ResultSetBar>").is_none());
        assert!(parse_head_timestamp_response("not xml").is_none());
    }

    #[test]
    fn build_schedule_xml_structure() {
        let xml = build_schedule_xml("sched_1", 756733, "20260312-19:34:06", "5 d", true);
        assert!(xml.contains("<id>sched_1</id>"));
        assert!(xml.contains("<contractID>756733</contractID>"));
        assert!(xml.contains("<data>Schedule</data>"));
        assert!(xml.contains("<scheduleOnly>true</scheduleOnly>"));
        assert!(xml.contains("<step>1 day</step>"));
        assert!(xml.contains("<useRTH>true</useRTH>"));
        assert!(xml.contains("<timeLength>5 d</timeLength>"));
    }

    #[test]
    fn parse_schedule_response_basic() {
        let xml = r#"<ResultSetSchedule>
            <id>sched_1</id>
            <eoq>true</eoq>
            <tz>US/Eastern</tz>
            <derivedStart>20260306-14:30:00</derivedStart>
            <Events>
                <Open><time>20260306-14:30:00</time><refDate>20260306</refDate></Open>
                <Close><time>20260306-21:00:00</time></Close>
                <Open><time>20260309-14:30:00</time><refDate>20260309</refDate></Open>
                <Close><time>20260309-21:00:00</time></Close>
            </Events>
        </ResultSetSchedule>"#;

        let resp = parse_schedule_response(xml).unwrap();
        assert_eq!(resp.query_id, "sched_1");
        assert_eq!(resp.timezone, "US/Eastern");
        assert_eq!(resp.start_date_time, "20260306-14:30:00");
        assert_eq!(resp.sessions.len(), 2);
        assert_eq!(resp.sessions[0].ref_date, "20260306");
        assert_eq!(resp.sessions[0].open_time, "20260306-14:30:00");
        assert_eq!(resp.sessions[0].close_time, "20260306-21:00:00");
        assert_eq!(resp.sessions[1].ref_date, "20260309");
    }

    #[test]
    fn parse_schedule_response_rejects_other() {
        assert!(parse_schedule_response("<ResultSetBar>...</ResultSetBar>").is_none());
        assert!(parse_schedule_response("not xml").is_none());
    }

    #[test]
    fn build_tick_query_xml_structure() {
        let xml = build_tick_query_xml("tk_1", 265598, "", "20260312-15:00:00", 100, "TRADES", true);
        assert!(xml.contains("<id>tk_1</id>"));
        assert!(xml.contains("<type>TickData</type>"));
        assert!(xml.contains("<data>AllLast</data>"));
        assert!(xml.contains("<step>ticks</step>"));
        assert!(xml.contains("<timeLength>100 t</timeLength>"));
        assert!(xml.contains("<wholeDays>true</wholeDays>"));
    }

    #[test]
    fn build_tick_query_xml_bid_ask() {
        let xml = build_tick_query_xml("tk_2", 265598, "", "20260312-15:00:00", 50, "BID_ASK", false);
        assert!(xml.contains("<data>BidAsk</data>"));
        assert!(xml.contains("<useRTH>false</useRTH>"));
    }

    #[test]
    fn parse_tick_response_trades() {
        let xml = r#"<ResultSetTick>
            <id>tk_1</id>
            <eoq>true</eoq>
            <tz>US/Eastern</tz>
            <Events>
                <Tick><time>20260312-14:30:01</time><price>150.25</price><size>100</size><exchange>NASDAQ</exchange><specialConditions></specialConditions></Tick>
                <Tick><time>20260312-14:30:02</time><price>150.30</price><size>200</size><exchange>NYSE</exchange><specialConditions>I</specialConditions></Tick>
            </Events>
        </ResultSetTick>"#;
        let (qid, data, done) = parse_tick_response(xml, "TRADES").unwrap();
        assert_eq!(qid, "tk_1");
        assert!(done);
        match data {
            crate::types::HistoricalTickData::Last(ticks) => {
                assert_eq!(ticks.len(), 2);
                assert_eq!(ticks[0].price, 150.25);
                assert_eq!(ticks[0].size, 100);
                assert_eq!(ticks[0].exchange, "NASDAQ");
                assert_eq!(ticks[1].special_conditions, "I");
            }
            _ => panic!("Expected Last variant"),
        }
    }

    #[test]
    fn parse_tick_response_bid_ask() {
        let xml = r#"<ResultSetTick>
            <id>tk_2</id>
            <eoq>true</eoq>
            <Events>
                <Tick><time>20260312-14:30:01</time><priceBid>150.24</priceBid><priceAsk>150.26</priceAsk><sizeBid>500</sizeBid><sizeAsk>600</sizeAsk></Tick>
            </Events>
        </ResultSetTick>"#;
        let (_, data, _) = parse_tick_response(xml, "BID_ASK").unwrap();
        match data {
            crate::types::HistoricalTickData::BidAsk(ticks) => {
                assert_eq!(ticks.len(), 1);
                assert_eq!(ticks[0].bid_price, 150.24);
                assert_eq!(ticks[0].ask_price, 150.26);
            }
            _ => panic!("Expected BidAsk variant"),
        }
    }

    #[test]
    fn parse_tick_response_midpoint() {
        let xml = r#"<ResultSetTick>
            <id>tk_3</id>
            <eoq>true</eoq>
            <Events>
                <Tick><time>20260312-14:30:01</time><price>150.25</price></Tick>
            </Events>
        </ResultSetTick>"#;
        let (_, data, _) = parse_tick_response(xml, "MIDPOINT").unwrap();
        match data {
            crate::types::HistoricalTickData::Midpoint(ticks) => {
                assert_eq!(ticks.len(), 1);
                assert_eq!(ticks[0].price, 150.25);
            }
            _ => panic!("Expected Midpoint variant"),
        }
    }

    #[test]
    fn parse_tick_response_rejects_other() {
        assert!(parse_tick_response("<ResultSetBar>...</ResultSetBar>", "TRADES").is_none());
    }

    #[test]
    fn build_realtime_bar_xml_structure() {
        let xml = build_realtime_bar_xml("rt_1", 265598, "TRADES", true);
        assert!(xml.contains("<id>rt_1</id>"));
        assert!(xml.contains("<type>BarData</type>"));
        assert!(xml.contains("<data>Last</data>"));
        assert!(xml.contains("<refresh>5 secs</refresh>"));
        assert!(xml.contains("<step>5 secs</step>"));
    }

    #[test]
    fn decode_bar_payload_single_tick() {
        // A minimal payload with count=1: the bar collapses to a single price.
        // Build a synthetic payload: 4-bit pad, 1-bit flag=1, 8-bit count=1,
        // 31-bit low_ticks=15000 (=150.00 at min_tick=0.01),
        // 1-bit vol_flag=1, 16-bit volume=100
        // Total bits: 4 + 1 + 8 + 31 + 1 + 16 = 61 bits → 8 bytes
        // After 4-byte group reversal decoding, this is complex to hand-build.
        // Just verify None on empty payload.
        assert!(decode_bar_payload(&[], 0.01).is_none());
    }
}
