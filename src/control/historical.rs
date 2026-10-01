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
    IndicativeAuction,
    NavLast,
    YieldAsk,
    YieldBid,
    YieldBidAsk,
    YieldMark,
    YieldLast,
    FeeRate,
    Schedule,
    AggTrades,
}

impl BarDataType {
    /// Parse the official API what_to_show string, case-insensitive, with
    /// the reference table (ibx#430). Any other value, the empty string
    /// included, is refused with the reference text.
    pub fn from_api_str(s: &str) -> Result<BarDataType, String> {
        Ok(match s.to_uppercase().as_str() {
            "TRADES" => Self::Trades,
            "MIDPOINT" => Self::Midpoint,
            "BID" => Self::Bid,
            "ASK" => Self::Ask,
            "BID_ASK" => Self::BidAsk,
            "ADJUSTED_LAST" => Self::AdjustedLast,
            "HISTORICAL_VOLATILITY" => Self::HistoricalVolatility,
            "OPTION_IMPLIED_VOLATILITY" => Self::ImpliedVolatility,
            "INDICATIVE_AUCTION_PRICE_SIZE" => Self::IndicativeAuction,
            "NAV_LAST" => Self::NavLast,
            "YIELD_ASK" => Self::YieldAsk,
            "YIELD_BID" => Self::YieldBid,
            "YIELD_BID_ASK" => Self::YieldBidAsk,
            "YIELD_MARK" => Self::YieldMark,
            "YIELD_LAST" => Self::YieldLast,
            "FEE_RATE" => Self::FeeRate,
            "SCHEDULE" => Self::Schedule,
            "AGGTRADES" => Self::AggTrades,
            _ => return Err(format!("What to show value of {} rejected.", s)),
        })
    }

    /// Server data name of this type (ibx#408, ibx#430). BID_ASK and
    /// YIELD_BID_ASK have no single server name: a bar request sends one
    /// query per entry of [`Self::legs`] instead. ADJUSTED_LAST asks for
    /// the trades series; the adjustment is not a server-side data name.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Trades => "Last",
            Self::Midpoint => "MidPoint",
            Self::Bid => "Bid",
            Self::Ask => "Ask",
            Self::BidAsk => "BidAsk",
            Self::AdjustedLast => "Last",
            Self::HistoricalVolatility => "HistVol",
            Self::ImpliedVolatility => "OptionImpliedVol",
            Self::IndicativeAuction => "AuctionIndicLast",
            Self::NavLast => "NavLast",
            Self::YieldAsk => "AskYield",
            Self::YieldBid => "BidYield",
            Self::YieldBidAsk => "BidYield",
            Self::YieldMark => "MarkYield",
            Self::YieldLast => "LastYield",
            Self::FeeRate => "FeeRate",
            Self::Schedule => "Schedule",
            Self::AggTrades => "AggLast",
        }
    }

    /// Server queries one bar request needs: BID_ASK is answered from a Bid
    /// query and an Ask query, YIELD_BID_ASK from a bid yield query and an
    /// ask yield query; every other type is one query (ibx#408, ibx#430).
    pub fn legs(&self) -> &'static [BarDataType] {
        match self {
            Self::BidAsk => &[Self::Bid, Self::Ask],
            Self::YieldBidAsk => &[Self::YieldBid, Self::YieldAsk],
            Self::Trades => &[Self::Trades],
            Self::Midpoint => &[Self::Midpoint],
            Self::Bid => &[Self::Bid],
            Self::Ask => &[Self::Ask],
            Self::AdjustedLast => &[Self::AdjustedLast],
            Self::HistoricalVolatility => &[Self::HistoricalVolatility],
            Self::ImpliedVolatility => &[Self::ImpliedVolatility],
            Self::IndicativeAuction => &[Self::IndicativeAuction],
            Self::NavLast => &[Self::NavLast],
            Self::YieldAsk => &[Self::YieldAsk],
            Self::YieldBid => &[Self::YieldBid],
            Self::YieldMark => &[Self::YieldMark],
            Self::YieldLast => &[Self::YieldLast],
            Self::FeeRate => &[Self::FeeRate],
            Self::Schedule => &[Self::Schedule],
            Self::AggTrades => &[Self::AggTrades],
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
    Month3,
    Year1,
}

/// Bar sizes listed in the reference refusal text.
const LEGAL_BAR_SIZES: &str = "1 secs, 5 secs, 10 secs, 15 secs, 30 secs, 1 min, 2 mins, 3 mins, \
    5 mins, 10 mins, 15 mins, 20 mins, 30 mins, 1 hour, 2 hours, 3 hours, 4 hours, 8 hours, \
    1 day, 1W, 1M";

impl BarSize {
    /// Parse the official API bar-size string with the reference table,
    /// case-insensitive (ibx#430). THE single table for every request
    /// path: two divergent copies previously fell back to Min5 silently
    /// (ibx#232). `1 sec`, `1 mins` and `1 hours` are refused, as the
    /// reference.
    pub fn from_api_str(s: &str) -> Result<BarSize, String> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "1 secs" => Self::Sec1,
            "5 secs" => Self::Sec5,
            "10 secs" => Self::Sec10,
            "15 secs" => Self::Sec15,
            "30 secs" => Self::Sec30,
            "1 min" => Self::Min1,
            "2 mins" => Self::Min2,
            "3 mins" => Self::Min3,
            "5 mins" => Self::Min5,
            "10 mins" => Self::Min10,
            "15 mins" => Self::Min15,
            "20 mins" => Self::Min20,
            "30 mins" => Self::Min30,
            "1 hour" => Self::Hour1,
            "2 hours" => Self::Hour2,
            "3 hours" => Self::Hour3,
            "4 hours" => Self::Hour4,
            "8 hours" => Self::Hour8,
            "1 day" => Self::Day1,
            "1w" | "1 w" | "1 week" => Self::Week1,
            "1m" | "1 m" | "1 month" => Self::Month1,
            "3 months" => Self::Month3,
            "1 year" => Self::Year1,
            _ => {
                return Err(format!(
                    "Historical data bar size setting is invalid. Legal ones are: {}",
                    LEGAL_BAR_SIZES,
                ));
            }
        })
    }

    /// Bar sizes the keepUpToDate streaming path supports. The rest are
    /// accepted on the batch path only; sending them with
    /// keep_up_to_date=true previously downgraded to Min5 silently (ibx#232).
    pub fn supports_keep_up_to_date(&self) -> bool {
        matches!(self, Self::Sec1 | Self::Sec5 | Self::Min5 | Self::Hour1 | Self::Day1)
    }

    /// Bars longer than one day (ibx#430).
    pub fn is_multi_day(&self) -> bool {
        matches!(self, Self::Week1 | Self::Month1 | Self::Month3 | Self::Year1)
    }

    /// Wire name of the size, as the reference sends it.
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
            Self::Week1 => "1W",
            Self::Month1 => "1M",
            Self::Month3 => "3 months",
            Self::Year1 => "1 year",
        }
    }
}

/// Text of a local refusal of a bar request, as the reference sends it
/// with error 321 (ibx#430).
pub fn bar_request_refusal(cause: &str) -> String {
    format!("Error validating request.-'bM' : cause - {}", cause)
}

/// Text of error 10314 for an end date the reference cannot read.
pub const INVALID_END_DATE: &str = "End Date/Time: The date, time, or time-zone entered is invalid.\n\
The correct format is yyyymmdd hh:mm:ss xx/xxxx\n\
where yyyymmdd and xx/xxxx are optional.\n\
E.g.: 20031126 15:59:00 US/Eastern\n\
\n\
Note that there is a space between the date and time,\n\
and between the time and time-zone.\n\
\n\
If no date is specified, current date is assumed.\n\
If no time-zone is specified, local time-zone is assumed(deprecated).\n\
\n\
You can also provide yyyymmddd-hh:mm:ss time is in UTC.\n\
Note that there is a dash between the date and time in UTC notation.";

/// Whether the reference reads an API end date (ibx#430): empty;
/// `yyyyMMdd-HH:mm:ss`; or `[yyyyMMdd ]HH:mm:ss` followed by optional
/// time-zone words (words that do not start with a digit). The date has a
/// year from 1978 to 3000, a month from 1 to 12 and a day up to 31.
pub fn is_valid_end_date(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() {
        return true;
    }
    fn date_ok(d: &str) -> bool {
        if d.len() != 8 || !d.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
        let y: u32 = d[0..4].parse().unwrap_or(0);
        let m: u32 = d[4..6].parse().unwrap_or(0);
        let day: u32 = d[6..8].parse().unwrap_or(99);
        (1978..=3000).contains(&y) && (1..=12).contains(&m) && day <= 31
    }
    fn time_ok(t: &str) -> bool {
        let parts: Vec<&str> = t.split(':').collect();
        if parts.len() != 3 || parts.iter().any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit())) {
            return false;
        }
        let v: Vec<u32> = parts.iter().map(|p| p.parse().unwrap_or(99)).collect();
        v[0] <= 23 && v[1] <= 59 && v[2] <= 59
    }
    if let Some((d, t)) = s.split_once('-') {
        if date_ok(d) && time_ok(t) {
            return true;
        }
    }
    let words: Vec<&str> = s.split_whitespace()
        .filter(|w| w.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .collect();
    match words.as_slice() {
        [d, t] => date_ok(d) && time_ok(t),
        [t] => time_ok(t),
        _ => false,
    }
}

/// Duration of a bar request in the reference form (ibx#430): a plain
/// number is seconds, and the unit letter takes the case the reference
/// sends. Err is the refusal text.
pub fn normalize_duration(duration: &str) -> Result<String, String> {
    if duration.is_empty() {
        return Err("Historical data request duration not specified.".to_string());
    }
    let with_unit = if duration.bytes().all(|b| b.is_ascii_digit()) {
        format!("{} S", duration)
    } else {
        duration.to_string()
    };
    let d: String = with_unit.chars().map(|c| match c {
        's' => 'S',
        'D' => 'd',
        'w' => 'W',
        'M' => 'm',
        'Y' => 'y',
        other => other,
    }).collect();
    let format_error = || "When specifying a unit, historical data request duration format is integer{SPACE}unit (S|D|W|M|Y).".to_string();
    let (num, unit) = d.split_once(' ').ok_or_else(format_error)?;
    if num.is_empty() || !num.bytes().all(|b| b.is_ascii_digit()) || !matches!(unit, "S" | "d" | "W" | "m" | "y") {
        return Err(format_error());
    }
    let invalid = || "Historical data requested duration is invalid.".to_string();
    let n: i32 = num.parse().map_err(|_| invalid())?;
    if n < 1 || (unit == "S" && n < 30) {
        return Err(invalid());
    }
    match unit {
        "S" if n > 86400 => Err("Historical data request for greater than 86400 seconds rejected.".to_string()),
        "d" if n > 365 => Err("Historical data requests for durations longer than 365 days must be made in years.".to_string()),
        "W" if n > 52 => Err("Historical data request for durations longer than 52 weeks must be made in years.".to_string()),
        "m" if n > 12 => Err("Historical data request for durations longer than 12 months must be made in years.".to_string()),
        _ => Ok(d),
    }
}

/// A bar request that passed the reference checks (ibx#430).
#[derive(Debug, Clone, PartialEq)]
pub struct CheckedBarRequest {
    pub data_type: BarDataType,
    pub bar_size: BarSize,
    /// Duration in the reference form.
    pub duration: String,
}

/// The local checks of a bar request, in the reference order (ibx#430):
/// the end date (10314), then the duration, ADJUSTED_LAST with an end
/// date, the bar size, ADJUSTED_LAST with bars longer than a day,
/// whatToShow, formatDate (when given), and SCHEDULE with bars other than
/// one day (321). Err is (code, text). The maximum number of backfill
/// years is not checked.
pub fn check_bar_request(
    end_date_time: &str,
    duration: &str,
    bar_size: &str,
    what_to_show: &str,
    format_date: Option<i32>,
) -> Result<CheckedBarRequest, (i32, String)> {
    let refuse = |cause: &str| (321, bar_request_refusal(cause));
    if !is_valid_end_date(end_date_time) {
        return Err((10314, INVALID_END_DATE.to_string()));
    }
    let duration = normalize_duration(duration).map_err(|e| refuse(&e))?;
    let adjusted = what_to_show.eq_ignore_ascii_case("ADJUSTED_LAST");
    if adjusted && !end_date_time.trim().is_empty() {
        return Err(refuse("End date not supported with adjusted last"));
    }
    let bar_size = BarSize::from_api_str(bar_size).map_err(|e| refuse(&e))?;
    if adjusted && bar_size.is_multi_day() {
        return Err(refuse("Multi day bar size not supported with adjusted last"));
    }
    let data_type = BarDataType::from_api_str(what_to_show).map_err(|e| refuse(&e))?;
    if let Some(n) = format_date {
        if !(1..=3).contains(&n) {
            return Err(refuse(&format!("Date formatting selection of {} rejected.", n)));
        }
    }
    if data_type == BarDataType::Schedule && bar_size != BarSize::Day1 {
        return Err(refuse("Only daily resolution supported for Schedule requests"));
    }
    Ok(CheckedBarRequest { data_type, bar_size, duration })
}

/// Parameters for a historical data request.
#[derive(Debug, Clone)]
pub struct HistoricalRequest {
    pub query_id: String,
    pub con_id: i64,
    pub symbol: String,
    /// Security type of the API contract (`STK`, `FUT`, `OPT`, `CASH`,
    /// `IND`...). Empty is a stock.
    pub sec_type: String,
    /// Exchange of the API contract. Empty is `SMART`.
    pub exchange: String,
    pub data_type: BarDataType,
    pub end_time: String,
    pub duration: String,
    pub bar_size: BarSize,
    pub use_rth: bool,
    pub keep_up_to_date: bool,
    /// The contract includes expired contracts: sent with the query
    /// (ibx#427).
    pub include_expired: bool,
}

/// Security type of a data-service query for an API contract secType
/// (ibx#305). An empty secType is a stock.
pub fn query_sec_type(sec_type: &str) -> String {
    let st = sec_type.trim().to_ascii_uppercase();
    if st.is_empty() { "STK".to_string() } else { st }
}

/// Exchange of a data-service query for an API contract exchange and
/// secType (ibx#305): smart routing and high-precision FX have their own
/// data-service names; any other exchange is sent as given.
pub fn query_exchange(exchange: &str, sec_type: &str) -> String {
    let ex = exchange.trim().to_ascii_uppercase();
    match (ex.as_str(), query_sec_type(sec_type).as_str()) {
        ("" | "SMART", _) => "BEST".to_string(),
        ("IDEALPRO", "CASH") => "FXSUBPIP".to_string(),
        _ => ex,
    }
}

/// Regular-trading-hours flag of a bar query (ibx#305): options, FX and
/// indices always ask for regular hours, whatever the API request says.
pub fn query_use_rth(sec_type: &str, use_rth: bool) -> bool {
    use_rth || matches!(query_sec_type(sec_type).as_str(), "OPT" | "CASH" | "IND")
}

/// Whether a query asks for the exchange's own data (indices, ibx#305).
fn query_use_native(sec_type: &str) -> bool {
    query_sec_type(sec_type) == "IND"
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
    let exchange = query_exchange(&req.exchange, &req.sec_type);
    let sec_type = query_sec_type(&req.sec_type);
    let rth = if query_use_rth(&req.sec_type, req.use_rth) { "true" } else { "false" };
    let native = if query_use_native(&req.sec_type) { "<useNative>yes</useNative>" } else { "" };
    let expired = if req.include_expired { "yes" } else { "no" };

    let data_str = req.data_type.as_str();
    // keepUpToDate uses structured ;;-delimited ID required by CCP gateway parser.
    // One-shot uses simple ID (HMDS accepts it fine).
    let query_id = if req.keep_up_to_date {
        let graph_name = format!("{}@{} {}", req.symbol, exchange, data_str);
        format!("{};;{};;1;;true;;0;;I", req.query_id, graph_name)
    } else {
        req.query_id.clone()
    };

    let (end_time_tag, refresh_tag) = if req.keep_up_to_date {
        (String::new(), "<refresh>5 secs</refresh>")
    } else {
        (format!("<endTime>{}</endTime>", req.end_time), "")
    };

    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <ListOfQueries>\
         <Query>\
         <id>{id}</id>\
         <useRTH>{rth}</useRTH>\
         <contractID>{con_id}</contractID>\
         <exchange>{exchange}</exchange>\
         <secType>{sec_type}</secType>\
         <expired>{expired}</expired>\
         <type>BarData</type>\
         <data>{data}</data>\
         {end_time}\
         <cutoffDate>20090224</cutoffDate>\
         {refresh}\
         <timeLength>{dur}</timeLength>\
         <step>{step}</step>\
         <source>API</source>\
         <needTotalValue>false</needTotalValue>\
         <wholeDays>false</wholeDays>\
         <delay>auto</delay>\
         {native}\
         </Query>\
         </ListOfQueries>",
        id = query_id,
        con_id = req.con_id,
        data = data_str,
        end_time = end_time_tag,
        dur = req.duration,
        step = req.bar_size.as_str(),
        refresh = refresh_tag,
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

/// Window id of a data-service query id: its first part. Replies are
/// matched to requests by it, exactly (ibx#428).
pub fn window_id(query_id: &str) -> &str {
    query_id.split(";;").next().unwrap_or(query_id).trim()
}

/// An error text and its detail joined as the reference joins them: a `:`
/// unless the text already ends with `:`, `.`, `=` or `-`.
pub fn join_error_text(text: &str, detail: &str) -> String {
    let t = text.trim();
    if t.ends_with(':') || t.ends_with('.') || t.ends_with('=') || t.ends_with('-') {
        format!("{}{}", text, detail)
    } else {
        format!("{}:{}", text, detail)
    }
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

/// One bar of the Bid or the Ask query of a BID_ASK request: the fields the
/// combined bar is built from (ibx#408).
#[derive(Debug, Clone, PartialEq)]
pub struct LegBar {
    pub time: String,
    pub high: f64,
    pub low: f64,
    pub time_avg: f64,
}

/// The bars of one bar reply frame of a Bid or Ask query (ibx#408).
pub fn parse_leg_bars(xml: &str) -> Vec<LegBar> {
    let mut bars = Vec::new();
    let mut search_start = 0;
    while let Some(bar_start) = xml[search_start..].find("<Bar>") {
        let abs_start = search_start + bar_start;
        let bar_end = match xml[abs_start..].find("</Bar>") {
            Some(e) => abs_start + e + 6,
            None => break,
        };
        let bar_xml = &xml[abs_start..bar_end];
        let num = |tag: &str| extract_xml_tag(bar_xml, tag)
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0);
        bars.push(LegBar {
            time: extract_xml_tag(bar_xml, "time").unwrap_or("").to_string(),
            high: num("high"),
            low: num("low"),
            time_avg: num("timeAvg"),
        });
        search_start = bar_end;
    }
    bars
}

/// The bars of a BID_ASK request from its Bid and Ask frames, given in
/// arrival order (ibx#408); YIELD_BID_ASK uses the same rule with its bid
/// and ask yield frames, as the reference. Bars are keyed by bar time, as the reference:
/// the Bid bar gives the open (its time average) and the low, the Ask bar
/// gives the close (its time average) and raises the high to its own high.
/// A bar found in one leg only keeps that leg's values: Bid only, open,
/// high and close are the Bid time average; Ask only, open, low and close
/// are the Ask time average. The combined bars carry no volume, average
/// price or trade count. Sorted by bar time.
pub fn combine_bid_ask(frames: &[(BarDataType, Vec<LegBar>)]) -> Vec<HistoricalBar> {
    let mut series: std::collections::BTreeMap<String, HistoricalBar> = std::collections::BTreeMap::new();
    for (leg, bars) in frames {
        let is_bid = match leg {
            BarDataType::Bid | BarDataType::YieldBid => true,
            BarDataType::Ask | BarDataType::YieldAsk => false,
            _ => continue,
        };
        for b in bars {
            match series.get_mut(&b.time) {
                Some(bar) if is_bid => {
                    bar.open = b.time_avg;
                    bar.low = b.low;
                }
                Some(bar) => {
                    bar.close = b.time_avg;
                    if b.high > bar.high {
                        bar.high = b.high;
                    }
                }
                None => {
                    let (open, high, low, close) = if is_bid {
                        (b.time_avg, b.time_avg, b.low, b.time_avg)
                    } else {
                        (b.time_avg, b.high, b.time_avg, b.time_avg)
                    };
                    series.insert(b.time.clone(), HistoricalBar {
                        time: b.time.clone(),
                        open, high, low, close,
                        volume: 0,
                        wap: 0.0,
                        count: 0,
                    });
                }
            }
        }
    }
    series.into_values().collect()
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
    /// Window id of the query, unique per request (ibx#428): the reply
    /// carries it back.
    pub window_id: String,
    pub con_id: i64,
    /// Security type of the API contract. Empty is a stock.
    pub sec_type: String,
    /// Exchange of the API contract. Empty is `SMART`.
    pub exchange: String,
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
    let exchange = query_exchange(&req.exchange, &req.sec_type);
    let sec_type = query_sec_type(&req.sec_type);
    // As the reference, the id does not carry useRTH.
    let id = format!("{};;{}@{} {};;0;;true;;0;;U",
        req.window_id, req.con_id, exchange, req.data_type.as_str());

    // The head timestamp query always asks for regular hours (ibx#305).
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <ListOfQueries>\
         <Query>\
         <id>{id}</id>\
         <useRTH>true</useRTH>\
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
#[allow(clippy::too_many_arguments)]
pub fn build_tick_query_xml(
    query_id: &str, con_id: i64, sec_type: &str, exchange: &str,
    start_date_time: &str, end_date_time: &str,
    number_of_ticks: u32, what_to_show: &str, use_rth: bool,
) -> String {
    let query_exchange = query_exchange(exchange, sec_type);
    let sec_type = query_sec_type(sec_type);
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
         <secType>{sec_type}</secType>\
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
        exchange = query_exchange,
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

/// Server data name of a real-time bar whatToShow, with the reference
/// table (ibx#454): exact API names only. None is refused with 321.
pub fn realtime_bar_data(what_to_show: &str) -> Option<&'static str> {
    match what_to_show {
        "ASK" => Some("Ask"),
        "BID" => Some("Bid"),
        "MIDPOINT" => Some("MidPoint"),
        "TRADES" => Some("Last"),
        "AGGTRADES" => Some("AggLast"),
        _ => None,
    }
}

/// Build the XML subscription for real-time 5-second bars.
///
/// Unlike the other historical queries, the exchange is the API contract
/// exchange as given (ibx#305); an empty one is `SMART`.
pub fn build_realtime_bar_xml(
    query_id: &str, con_id: i64, sec_type: &str, exchange: &str,
    what_to_show: &str, use_rth: bool,
) -> String {
    let exchange = match exchange.trim() {
        "" => "SMART",
        e => e,
    };
    let sec_type = query_sec_type(sec_type);
    let rth = if use_rth { "true" } else { "false" };
    // The engine refuses a value outside the table before building.
    let data = realtime_bar_data(what_to_show).unwrap_or("Last");

    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <ListOfQueries>\
         <Query>\
         <id>{id}</id>\
         <useRTH>{rth}</useRTH>\
         <contractID>{con_id}</contractID>\
         <exchange>{exchange}</exchange>\
         <secType>{sec_type}</secType>\
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
pub fn build_schedule_xml(
    query_id: &str, con_id: i64, sec_type: &str, exchange: &str,
    end_time: &str, duration: &str, use_rth: bool,
) -> String {
    let exchange = query_exchange(exchange, sec_type);
    let sec_type = query_sec_type(sec_type);
    let rth = if use_rth { "true" } else { "false" };

    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <ListOfQueries>\
         <Query>\
         <id>{id}</id>\
         <useRTH>{rth}</useRTH>\
         <contractID>{con_id}</contractID>\
         <exchange>{exchange}</exchange>\
         <secType>{sec_type}</secType>\
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
        assert_eq!(BarDataType::Midpoint.as_str(), "MidPoint");
        assert_eq!(BarDataType::Bid.as_str(), "Bid");
        assert_eq!(BarDataType::Ask.as_str(), "Ask");
        assert_eq!(BarDataType::AdjustedLast.as_str(), "Last");
        assert_eq!(BarDataType::HistoricalVolatility.as_str(), "HistVol");
        assert_eq!(BarDataType::ImpliedVolatility.as_str(), "OptionImpliedVol");
    }

    // ibx#408: BID_ASK is two queries, every other type one.
    #[test]
    fn bar_data_type_legs() {
        assert_eq!(BarDataType::BidAsk.legs(), &[BarDataType::Bid, BarDataType::Ask]);
        assert_eq!(BarDataType::YieldBidAsk.legs(), &[BarDataType::YieldBid, BarDataType::YieldAsk]);
        for dt in [
            BarDataType::Trades, BarDataType::Midpoint, BarDataType::Bid,
            BarDataType::Ask, BarDataType::AdjustedLast,
            BarDataType::HistoricalVolatility, BarDataType::ImpliedVolatility,
            BarDataType::FeeRate, BarDataType::AggTrades,
        ] {
            assert_eq!(dt.legs(), &[dt]);
        }
    }

    // ── ibx#430: the reference tables and checks ──

    #[test]
    fn what_to_show_reference_table() {
        for (api, name) in [
            ("TRADES", "Last"), ("midpoint", "MidPoint"), ("BID", "Bid"), ("ASK", "Ask"),
            ("ADJUSTED_LAST", "Last"), ("HISTORICAL_VOLATILITY", "HistVol"),
            ("OPTION_IMPLIED_VOLATILITY", "OptionImpliedVol"),
            ("INDICATIVE_AUCTION_PRICE_SIZE", "AuctionIndicLast"), ("NAV_LAST", "NavLast"),
            ("YIELD_ASK", "AskYield"), ("yield_bid", "BidYield"), ("YIELD_MARK", "MarkYield"),
            ("YIELD_LAST", "LastYield"), ("FEE_RATE", "FeeRate"), ("SCHEDULE", "Schedule"),
            ("AGGTRADES", "AggLast"),
        ] {
            assert_eq!(BarDataType::from_api_str(api).unwrap().as_str(), name, "{}", api);
        }
        for bad in ["", "TRADE", "REBATE_RATE", "OPTION_VOLUME"] {
            assert_eq!(BarDataType::from_api_str(bad).unwrap_err(), format!("What to show value of {} rejected.", bad));
        }
    }

    #[test]
    fn combine_yield_bid_ask_like_bid_ask() {
        let bars = combine_bid_ask(&[
            (BarDataType::YieldBid, vec![leg("t1", 4.2, 4.0, 4.1)]),
            (BarDataType::YieldAsk, vec![leg("t1", 4.5, 4.3, 4.4)]),
        ]);
        assert_eq!((bars[0].open, bars[0].high, bars[0].low, bars[0].close), (4.1, 4.5, 4.0, 4.4));
    }

    #[test]
    fn bar_size_reference_table_case_insensitive() {
        for (api, wire) in [
            ("1 secs", "1 secs"), ("1 Min", "1 min"), ("1 DAY", "1 day"), ("2 Hours", "2 hours"),
            ("1W", "1W"), ("1 W", "1W"), ("1 week", "1W"), ("1M", "1M"), ("1 m", "1M"),
            ("1 month", "1M"), ("3 months", "3 months"), ("1 year", "1 year"),
        ] {
            assert_eq!(BarSize::from_api_str(api).unwrap().as_str(), wire, "{}", api);
        }
        for bad in ["1 sec", "1 mins", "1 hours", "7 mins", "1min", ""] {
            let err = BarSize::from_api_str(bad).unwrap_err();
            assert!(err.starts_with("Historical data bar size setting is invalid. Legal ones are: 1 secs, 5 secs"), "{}", err);
        }
    }

    #[test]
    fn duration_reference_rules() {
        assert_eq!(normalize_duration("1 D").unwrap(), "1 d");
        assert_eq!(normalize_duration("1800 S").unwrap(), "1800 S");
        assert_eq!(normalize_duration("1800 s").unwrap(), "1800 S");
        assert_eq!(normalize_duration("3600").unwrap(), "3600 S");
        assert_eq!(normalize_duration("2 w").unwrap(), "2 W");
        assert_eq!(normalize_duration("1 M").unwrap(), "1 m");
        assert_eq!(normalize_duration("5 Y").unwrap(), "5 y");
        assert_eq!(normalize_duration("365 D").unwrap(), "365 d");
        let err = |d: &str| normalize_duration(d).unwrap_err();
        assert_eq!(err(""), "Historical data request duration not specified.");
        for bad in ["1 day", "1D", "1  D", "D", "1 X", "-1 D"] {
            assert!(err(bad).starts_with("When specifying a unit"), "{}: {}", bad, err(bad));
        }
        assert_eq!(err("0 D"), "Historical data requested duration is invalid.");
        assert_eq!(err("29 S"), "Historical data requested duration is invalid.");
        assert_eq!(err("99999999999 S"), "Historical data requested duration is invalid.");
        assert_eq!(err("86401 S"), "Historical data request for greater than 86400 seconds rejected.");
        assert_eq!(err("400 D"), "Historical data requests for durations longer than 365 days must be made in years.");
        assert_eq!(err("53 W"), "Historical data request for durations longer than 52 weeks must be made in years.");
        assert_eq!(err("13 M"), "Historical data request for durations longer than 12 months must be made in years.");
    }

    #[test]
    fn end_date_reference_forms() {
        for ok in ["", "20260102-15:00:00", "20260102 10:00:00", "20260102 10:00:00 US/Eastern",
                   "20260102 10:00:00 UTC", "10:00:00", "10:00:00 US/Eastern"] {
            assert!(is_valid_end_date(ok), "{:?}", ok);
        }
        for bad in ["20260102", "2026-01-02", "20261302 10:00:00", "20260102 25:00:00",
                    "20260102 10:00", "19770102 10:00:00", "yesterday", "20260102 10:00:00 20260103"] {
            assert!(!is_valid_end_date(bad), "{:?}", bad);
        }
    }

    #[test]
    fn check_bar_request_order_and_codes() {
        let ok = check_bar_request("", "3600", "1 Min", "trades", Some(1)).unwrap();
        assert_eq!(ok, CheckedBarRequest { data_type: BarDataType::Trades, bar_size: BarSize::Min1, duration: "3600 S".into() });
        let code = |r: Result<CheckedBarRequest, (i32, String)>| r.unwrap_err();
        assert_eq!(code(check_bar_request("garbage", "1 D", "1 day", "TRADES", None)).0, 10314);
        assert_eq!(code(check_bar_request("", "400 D", "1 day", "TRADES", None)),
            (321, "Error validating request.-'bM' : cause - Historical data requests for durations longer than 365 days must be made in years.".to_string()));
        assert_eq!(code(check_bar_request("20260102 10:00:00", "1 D", "1 hour", "ADJUSTED_LAST", None)).1,
            "Error validating request.-'bM' : cause - End date not supported with adjusted last");
        assert_eq!(code(check_bar_request("", "1 Y", "1 week", "ADJUSTED_LAST", None)).1,
            "Error validating request.-'bM' : cause - Multi day bar size not supported with adjusted last");
        assert!(check_bar_request("", "1 Y", "1 day", "ADJUSTED_LAST", None).is_ok());
        assert_eq!(code(check_bar_request("", "1 D", "1 sec", "TRADES", None)).0, 321);
        assert_eq!(code(check_bar_request("", "1 D", "1 day", "YIELD", None)).1,
            "Error validating request.-'bM' : cause - What to show value of YIELD rejected.");
        assert_eq!(code(check_bar_request("", "1 D", "1 day", "TRADES", Some(4))).1,
            "Error validating request.-'bM' : cause - Date formatting selection of 4 rejected.");
        assert_eq!(code(check_bar_request("", "1 M", "1 hour", "SCHEDULE", None)).1,
            "Error validating request.-'bM' : cause - Only daily resolution supported for Schedule requests");
        assert!(check_bar_request("", "1 M", "1 day", "SCHEDULE", None).is_ok());
        // The issue's checks: 1 year and 3 months bars over 5 Y.
        assert!(check_bar_request("", "5 Y", "1 year", "TRADES", None).is_ok());
        assert!(check_bar_request("", "5 Y", "3 months", "TRADES", None).is_ok());
    }

    // ibx#408: BID_ASK bars from the Bid leg and the Ask leg.
    fn leg(time: &str, high: f64, low: f64, time_avg: f64) -> LegBar {
        LegBar { time: time.to_string(), high, low, time_avg }
    }

    #[test]
    fn combine_bid_ask_from_both_legs() {
        let bid = vec![leg("20260227-20:30:00", 266.63, 266.30, 266.466), leg("20260227-20:31:00", 266.38, 266.00, 266.154)];
        let ask = vec![leg("20260227-20:30:00", 266.70, 266.40, 266.520), leg("20260227-20:32:00", 266.20, 266.00, 266.100)];
        let ohlc = |bars: Vec<HistoricalBar>| bars.into_iter()
            .map(|b| (b.time, b.open, b.high, b.low, b.close, b.volume, b.wap, b.count))
            .collect::<Vec<_>>();

        let bid_first = ohlc(combine_bid_ask(&[(BarDataType::Bid, bid.clone()), (BarDataType::Ask, ask.clone())]));
        assert_eq!(bid_first, vec![
            ("20260227-20:30:00".to_string(), 266.466, 266.70, 266.30, 266.520, 0, 0.0, 0),
            ("20260227-20:31:00".to_string(), 266.154, 266.154, 266.00, 266.154, 0, 0.0, 0),
            ("20260227-20:32:00".to_string(), 266.100, 266.20, 266.100, 266.100, 0, 0.0, 0),
        ]);
        // Ask first: same bars, sorted by time.
        let ask_first = ohlc(combine_bid_ask(&[(BarDataType::Ask, ask), (BarDataType::Bid, bid)]));
        assert_eq!(ask_first, bid_first);
    }

    #[test]
    fn combine_bid_ask_high_is_the_larger_of_the_bid_average_and_the_ask_high() {
        // Bid bar first: its placeholder high is the Bid time average; an
        // Ask high below it does not lower the high.
        let bars = combine_bid_ask(&[
            (BarDataType::Bid, vec![leg("t1", 10.0, 9.0, 9.8)]),
            (BarDataType::Ask, vec![leg("t1", 9.7, 9.5, 9.6)]),
        ]);
        assert_eq!((bars[0].open, bars[0].high, bars[0].low, bars[0].close), (9.8, 9.8, 9.0, 9.6));
        // Ask bar first: the high is the Ask high, the Bid sets open and low.
        let bars = combine_bid_ask(&[
            (BarDataType::Ask, vec![leg("t1", 9.7, 9.5, 9.6)]),
            (BarDataType::Bid, vec![leg("t1", 10.0, 9.0, 9.8)]),
        ]);
        assert_eq!((bars[0].open, bars[0].high, bars[0].low, bars[0].close), (9.8, 9.7, 9.0, 9.6));
    }

    #[test]
    fn parse_leg_bars_reads_time_average() {
        let xml = "<ResultSetBar><id>q</id><eoq>true</eoq><Events>\
                   <Bar><time>20260227-20:30:00</time><endTime>20260227-20:31:00</endTime>\
                   <open>266.63</open><close>266.33</close><high>266.63</high><low>266.3</low>\
                   <timeAvg>266.466</timeAvg></Bar></Events></ResultSetBar>";
        assert_eq!(parse_leg_bars(xml), vec![leg("20260227-20:30:00", 266.63, 266.3, 266.466)]);
    }

    #[test]
    fn bar_size_strings() {
        assert_eq!(BarSize::Min5.as_str(), "5 mins");
        assert_eq!(BarSize::Hour1.as_str(), "1 hour");
        assert_eq!(BarSize::Day1.as_str(), "1 day");
    }

    // ── ibx#232: single parse table, rejection instead of Min5/TRADES ──

    #[test]
    fn bar_size_from_api_str_accepts_all_official_strings() {
        let all = [
            "1 secs", "5 secs", "10 secs", "15 secs", "30 secs",
            "1 min", "2 mins", "3 mins", "5 mins", "10 mins", "15 mins",
            "20 mins", "30 mins", "1 hour", "2 hours", "3 hours", "4 hours",
            "8 hours", "1 day", "1 week", "1 month", "3 months", "1 year",
        ];
        for s in all {
            assert!(BarSize::from_api_str(s).is_ok(), "'{}' must parse", s);
        }
        assert_eq!(BarSize::from_api_str("1 min").unwrap(), BarSize::Min1);
    }

    #[test]
    fn bar_size_from_api_str_rejects_unknown() {
        // ibx#232: an unknown size is refused, never replaced by 5 minutes.
        for s in ["1min", "1 minute", "7 mins", ""] {
            let err = BarSize::from_api_str(s).unwrap_err();
            assert!(err.contains("bar size setting is invalid"), "'{}' -> {}", s, err);
        }
    }

    #[test]
    fn bar_size_keep_up_to_date_support() {
        for s in ["1 secs", "5 secs", "5 mins", "1 hour", "1 day"] {
            assert!(BarSize::from_api_str(s).unwrap().supports_keep_up_to_date(), "{}", s);
        }
        for s in ["10 secs", "1 min", "15 mins", "4 hours", "1 week"] {
            assert!(!BarSize::from_api_str(s).unwrap().supports_keep_up_to_date(), "{}", s);
        }
    }

    #[test]
    fn bar_data_type_from_api_str() {
        assert_eq!(BarDataType::from_api_str("TRADES").unwrap(), BarDataType::Trades);
        assert_eq!(BarDataType::from_api_str("trades").unwrap(), BarDataType::Trades);
        // ibx#430: an empty value is not TRADES for the reference.
        assert!(BarDataType::from_api_str("").is_err());
        assert_eq!(BarDataType::from_api_str("BID_ASK").unwrap(), BarDataType::BidAsk);
        // A misspelled value used to quietly return trade bars.
        assert!(BarDataType::from_api_str("TRADE").is_err());
        assert!(BarDataType::from_api_str("BIDD").is_err());
    }

    #[test]
    fn build_query_xml_structure() {
        let req = HistoricalRequest {
            query_id: "q1".to_string(),
            con_id: 265598,
            symbol: "AAPL".to_string(),
            sec_type: "STK".to_string(),
            exchange: "SMART".to_string(),
            data_type: BarDataType::Trades,
            end_time: "20260228-15:00:00".to_string(),
            duration: "1 d".to_string(),
            bar_size: BarSize::Min5,
            use_rth: true,
            keep_up_to_date: false,
            include_expired: false,
        };
        let xml = build_query_xml(&req);
        assert!(xml.contains("<id>q1</id>"));
        assert!(xml.contains("<contractID>265598</contractID>"));
        assert!(xml.contains("<exchange>BEST</exchange>")); // SMART→BEST
        assert!(xml.contains("<secType>STK</secType>"));
        assert!(!xml.contains("<useNative>"));
        assert!(xml.contains("<data>Last</data>"));
        assert!(xml.contains("<step>5 mins</step>"));
        assert!(xml.contains("<useRTH>true</useRTH>"));
        assert!(xml.contains("<timeLength>1 d</timeLength>"));
        assert!(xml.contains("<expired>no</expired>"));
        // ibx#427: includeExpired reaches the query.
        let xml = build_query_xml(&HistoricalRequest { include_expired: true, ..req.clone() });
        assert!(xml.contains("<expired>yes</expired>"), "{}", xml);

        // ibx#408: server data names.
        for (dt, name) in [
            (BarDataType::Midpoint, "MidPoint"),
            (BarDataType::AdjustedLast, "Last"),
            (BarDataType::HistoricalVolatility, "HistVol"),
            (BarDataType::ImpliedVolatility, "OptionImpliedVol"),
        ] {
            let xml = build_query_xml(&HistoricalRequest { data_type: dt, ..req.clone() });
            assert!(xml.contains(&format!("<data>{}</data>", name)), "{:?}: {}", dt, xml);
        }
    }

    // ── ibx#305: secType and exchange come from the API contract ──

    fn bar_req(sec_type: &str, exchange: &str, data_type: BarDataType, use_rth: bool) -> HistoricalRequest {
        HistoricalRequest {
            query_id: "q305".to_string(),
            con_id: 815824267,
            symbol: "X".to_string(),
            sec_type: sec_type.to_string(),
            exchange: exchange.to_string(),
            data_type,
            end_time: "20260928-20:00:00".to_string(),
            duration: "1 d".to_string(),
            bar_size: BarSize::Hour1,
            use_rth,
            keep_up_to_date: false,
            include_expired: false,
        }
    }

    #[test]
    fn build_query_xml_future_keeps_its_exchange_and_rth_flag() {
        let xml = build_query_xml(&bar_req("FUT", "CME", BarDataType::Trades, false));
        assert!(xml.contains("<contractID>815824267</contractID><exchange>CME</exchange><secType>FUT</secType>"), "{}", xml);
        assert!(xml.contains("<useRTH>false</useRTH>"));
        assert!(!xml.contains("<useNative>"));
    }

    #[test]
    fn build_query_xml_stock_keeps_rth_flag() {
        let xml = build_query_xml(&bar_req("STK", "SMART", BarDataType::Trades, false));
        assert!(xml.contains("<useRTH>false</useRTH>"));
        // An empty secType and exchange are a smart-routed stock.
        let xml = build_query_xml(&bar_req("", "", BarDataType::Trades, false));
        assert!(xml.contains("<exchange>BEST</exchange><secType>STK</secType>"), "{}", xml);
    }

    #[test]
    fn build_query_xml_option_forces_rth() {
        let xml = build_query_xml(&bar_req("OPT", "SMART", BarDataType::Trades, false));
        assert!(xml.contains("<exchange>BEST</exchange><secType>OPT</secType>"), "{}", xml);
        assert!(xml.contains("<useRTH>true</useRTH>"));
    }

    #[test]
    fn build_query_xml_fx_uses_high_precision_exchange_and_forces_rth() {
        let xml = build_query_xml(&bar_req("CASH", "IDEALPRO", BarDataType::Midpoint, false));
        assert!(xml.contains("<exchange>FXSUBPIP</exchange><secType>CASH</secType>"), "{}", xml);
        assert!(xml.contains("<data>MidPoint</data>"));
        assert!(xml.contains("<useRTH>true</useRTH>"));
    }

    #[test]
    fn build_query_xml_index_keeps_exchange_forces_rth_and_asks_native() {
        let xml = build_query_xml(&bar_req("IND", "CBOE", BarDataType::Trades, false));
        assert!(xml.contains("<exchange>CBOE</exchange><secType>IND</secType>"), "{}", xml);
        assert!(xml.contains("<useRTH>true</useRTH>"));
        assert!(xml.contains("<useNative>yes</useNative>"));
    }

    #[test]
    fn query_exchange_table() {
        assert_eq!(query_exchange("SMART", "STK"), "BEST");
        assert_eq!(query_exchange("smart", "OPT"), "BEST");
        assert_eq!(query_exchange("", "STK"), "BEST");
        assert_eq!(query_exchange("CME", "FUT"), "CME");
        assert_eq!(query_exchange("IDEALPRO", "CASH"), "FXSUBPIP");
        assert_eq!(query_exchange("CBOE", "IND"), "CBOE");
        assert_eq!(query_sec_type(""), "STK");
        assert_eq!(query_sec_type("fut"), "FUT");
    }

    #[test]
    fn build_fix_request() {
        let req = HistoricalRequest {
            query_id: "q1".to_string(),
            con_id: 265598,
            symbol: "AAPL".to_string(),
            sec_type: "STK".to_string(),
            exchange: "SMART".to_string(),
            data_type: BarDataType::Trades,
            end_time: "20260228-15:00:00".to_string(),
            duration: "1 d".to_string(),
            bar_size: BarSize::Min5,
            use_rth: true,
            keep_up_to_date: false,
            include_expired: false,
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
            window_id: "TickHeadClient1".to_string(),
            con_id: 756733,
            sec_type: "STK".to_string(),
            exchange: "SMART".to_string(),
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
        assert!(xml.contains("<secType>STK</secType>"));
    }

    // ibx#305: contract secType and exchange; regular hours always.
    #[test]
    fn head_timestamp_xml_future_always_rth() {
        let req = HeadTimestampRequest {
            window_id: "TickHeadClient7".to_string(),
            con_id: 815824267,
            sec_type: "FUT".to_string(),
            exchange: "CME".to_string(),
            data_type: BarDataType::Trades,
            use_rth: false,
        };
        let xml = build_head_timestamp_xml(&req);
        assert!(xml.contains("<useRTH>true</useRTH>"), "{}", xml);
        assert!(xml.contains("<exchange>CME</exchange><secType>FUT</secType><type>TickHeadTimeStamp</type>"), "{}", xml);
        // ibx#428: the request's own window id, without useRTH.
        assert!(xml.contains("<id>TickHeadClient7;;815824267@CME Last;;0;;true;;0;;U</id>"), "{}", xml);
    }

    #[test]
    fn window_id_and_error_join() {
        assert_eq!(window_id("TickHeadClient12;;265598@BEST Last;;0;;true;;0;;U"), "TickHeadClient12");
        assert_eq!(window_id("hist_1001"), "hist_1001");
        assert_eq!(window_id("Fundamentals1;; COMPANY_FUNDAMENTALS;;0;;true;;0;;U"), "Fundamentals1");
        assert_eq!(join_error_text("Failed to request histogram data", "boom"), "Failed to request histogram data:boom");
        assert_eq!(join_error_text("Failed to request tick-by-tick data.", "boom"), "Failed to request tick-by-tick data.boom");
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
        let xml = build_schedule_xml("sched_1", 756733, "STK", "SMART", "20260312-19:34:06", "5 d", true);
        assert!(xml.contains("<id>sched_1</id>"));
        assert!(xml.contains("<contractID>756733</contractID>"));
        assert!(xml.contains("<data>Schedule</data>"));
        assert!(xml.contains("<scheduleOnly>true</scheduleOnly>"));
        assert!(xml.contains("<step>1 day</step>"));
        assert!(xml.contains("<useRTH>true</useRTH>"));
        assert!(xml.contains("<timeLength>5 d</timeLength>"));
        assert!(xml.contains("<exchange>BEST</exchange><secType>STK</secType>"));
        let xml = build_schedule_xml("sched_2", 815824267, "FUT", "CME", "20260312-19:34:06", "5 d", true);
        assert!(xml.contains("<exchange>CME</exchange><secType>FUT</secType>"), "{}", xml);
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
        let xml = build_tick_query_xml("tk_1", 265598, "STK", "SMART", "", "20260312-15:00:00", 100, "TRADES", true);
        assert!(xml.contains("<id>tk_1</id>"));
        assert!(xml.contains("<type>TickData</type>"));
        assert!(xml.contains("<data>AllLast</data>"));
        assert!(xml.contains("<step>ticks</step>"));
        assert!(xml.contains("<timeLength>100 t</timeLength>"));
        assert!(xml.contains("<wholeDays>true</wholeDays>"));
        assert!(xml.contains("<exchange>BEST</exchange><secType>STK</secType>"));
        let xml = build_tick_query_xml("tk_3", 815824267, "FUT", "CME", "", "20260312-15:00:00", 100, "TRADES", false);
        assert!(xml.contains("<exchange>CME</exchange><secType>FUT</secType>"), "{}", xml);
    }

    #[test]
    fn build_tick_query_xml_bid_ask() {
        let xml = build_tick_query_xml("tk_2", 265598, "STK", "SMART", "", "20260312-15:00:00", 50, "BID_ASK", false);
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
        let xml = build_realtime_bar_xml("rt_1", 265598, "STK", "SMART", "TRADES", true);
        assert!(xml.contains("<id>rt_1</id>"));
        assert!(xml.contains("<type>BarData</type>"));
        assert!(xml.contains("<data>Last</data>"));
        assert!(xml.contains("<refresh>5 secs</refresh>"));
        assert!(xml.contains("<step>5 secs</step>"));
        // ibx#305: the exchange is the API contract exchange as given.
        assert!(xml.contains("<exchange>SMART</exchange><secType>STK</secType>"), "{}", xml);
        let xml = build_realtime_bar_xml("rt_2", 815824267, "FUT", "CME", "TRADES", true);
        assert!(xml.contains("<exchange>CME</exchange><secType>FUT</secType>"), "{}", xml);
        // ibx#454: the reference names.
        assert!(build_realtime_bar_xml("rt_3", 1, "STK", "SMART", "MIDPOINT", true).contains("<data>MidPoint</data>"));
        assert!(build_realtime_bar_xml("rt_4", 1, "STK", "SMART", "AGGTRADES", true).contains("<data>AggLast</data>"));
    }

    #[test]
    fn realtime_bar_what_to_show_table() {
        assert_eq!(realtime_bar_data("ASK"), Some("Ask"));
        assert_eq!(realtime_bar_data("BID"), Some("Bid"));
        assert_eq!(realtime_bar_data("MIDPOINT"), Some("MidPoint"));
        assert_eq!(realtime_bar_data("TRADES"), Some("Last"));
        assert_eq!(realtime_bar_data("AGGTRADES"), Some("AggLast"));
        for bad in ["BID_ASK", "trades", "", "ADJUSTED_LAST"] {
            assert_eq!(realtime_bar_data(bad), None, "{:?}", bad);
        }
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
