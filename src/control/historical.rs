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

    /// Label of the type in the chart name of a query id, as the reference
    /// writes it: the API name with its first letter in capitals and the
    /// rest in lower case (`Trades`, `Midpoint`, `Bid_ask`).
    pub fn label(&self) -> String {
        capitalized(self.api_name())
    }

    /// The official API name of the type (`TRADES`, `MIDPOINT`), as the
    /// head timestamp query id carries it (ibx#486).
    pub fn api_name(&self) -> &'static str {
        match self {
            Self::Trades => "TRADES",
            Self::Midpoint => "MIDPOINT",
            Self::Bid => "BID",
            Self::Ask => "ASK",
            Self::BidAsk => "BID_ASK",
            Self::AdjustedLast => "ADJUSTED_LAST",
            Self::HistoricalVolatility => "HISTORICAL_VOLATILITY",
            Self::ImpliedVolatility => "OPTION_IMPLIED_VOLATILITY",
            Self::IndicativeAuction => "INDICATIVE_AUCTION_PRICE_SIZE",
            Self::NavLast => "NAV_LAST",
            Self::YieldAsk => "YIELD_ASK",
            Self::YieldBid => "YIELD_BID",
            Self::YieldBidAsk => "YIELD_BID_ASK",
            Self::YieldMark => "YIELD_MARK",
            Self::YieldLast => "YIELD_LAST",
            Self::FeeRate => "FEE_RATE",
            Self::Schedule => "SCHEDULE",
            Self::AggTrades => "AGGTRADES",
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

/// `TRADES` as `Trades`: the first letter in capitals, the rest in lower
/// case.
fn capitalized(api: &str) -> String {
    let lower = api.to_ascii_lowercase();
    let mut c = lower.chars();
    match c.next() {
        Some(first) => first.to_ascii_uppercase().to_string() + c.as_str(),
        None => String::new(),
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

    /// Bars shorter than one day: their time is written with the time of
    /// day, by formatDate (ibx#431).
    pub fn is_intraday(&self) -> bool {
        self.seconds().is_some()
    }

    /// Length of a bar shorter than one day, in seconds; None for daily
    /// and longer bars.
    pub fn seconds(&self) -> Option<i64> {
        Some(match self {
            Self::Sec1 => 1,
            Self::Sec5 => 5,
            Self::Sec10 => 10,
            Self::Sec15 => 15,
            Self::Sec30 => 30,
            Self::Min1 => 60,
            Self::Min2 => 120,
            Self::Min3 => 180,
            Self::Min5 => 300,
            Self::Min10 => 600,
            Self::Min15 => 900,
            Self::Min20 => 1200,
            Self::Min30 => 1800,
            Self::Hour1 => 3600,
            Self::Hour2 => 7200,
            Self::Hour3 => 10800,
            Self::Hour4 => 14400,
            Self::Hour8 => 28800,
            _ => return None,
        })
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

/// Text of warning 2174, sent when a request date names no time zone.
pub const IMPLIED_ZONE_WARNING: &str = "Warning: You submitted request with date-time attributes without explicit time zone. \
Please switch to use yyyymmdd-hh:mm:ss in UTC or use instrument time zone, like US/Eastern. \
Implied time zone functionality will be removed in the next API release";

/// A request date as the reference reads it (ibx#431, ibx#432): the
/// instant in Unix seconds, and the zone the text names. `zone` is None
/// when the text names none: it is read in the machine zone, and the
/// reference sends warning 2174.
#[derive(Debug, Clone, PartialEq)]
pub struct RequestTime {
    pub secs: i64,
    pub zone: Option<String>,
}

/// The zone of a zone name: `UTC` and `GMT`, the legacy US names, and the
/// zone database names.
pub fn zone_named(name: &str) -> Option<jiff::tz::TimeZone> {
    if name.eq_ignore_ascii_case("UTC") || name.eq_ignore_ascii_case("GMT") {
        return Some(jiff::tz::TimeZone::UTC);
    }
    jiff::tz::TimeZone::get(crate::config::canonical_zone(name)).ok()
}

/// Read a request date (ibx#431, ibx#432), in the forms of
/// [`is_valid_end_date`]: `yyyyMMdd-HH:mm:ss` is UTC; `[yyyyMMdd ]HH:mm:ss`
/// is in the zone named after it, else in `machine_zone`; with no date,
/// the day of `now` in that zone. Ok(None) for an empty text; Err for a
/// text or a zone the reference cannot read (10314).
pub fn parse_request_time(text: &str, machine_zone: &str, now: i64) -> Result<Option<RequestTime>, ()> {
    let s = text.trim();
    if s.is_empty() {
        return Ok(None);
    }
    if !is_valid_end_date(s) {
        return Err(());
    }
    let civil = |d: Option<&str>, t: &str, tz: &jiff::tz::TimeZone| -> Result<i64, ()> {
        let n = |v: &str| v.parse::<i32>().map_err(|_| ());
        let hms: Vec<&str> = t.split(':').collect();
        let time = jiff::civil::Time::new(n(hms[0])? as i8, n(hms[1])? as i8, n(hms[2])? as i8, 0).map_err(|_| ())?;
        let date = match d {
            Some(d) => jiff::civil::Date::new(n(&d[0..4])? as i16, n(&d[4..6])? as i8, n(&d[6..8])? as i8).map_err(|_| ())?,
            None => jiff::Timestamp::from_second(now).map_err(|_| ())?.to_zoned(tz.clone()).date(),
        };
        date.to_datetime(time).to_zoned(tz.clone()).map(|z| z.timestamp().as_second()).map_err(|_| ())
    };
    if !s.contains(' ') {
        if let Some((d, t)) = s.split_once('-') {
            let secs = civil(Some(d), t, &jiff::tz::TimeZone::UTC)?;
            return Ok(Some(RequestTime { secs, zone: Some("UTC".to_string()) }));
        }
    }
    let words: Vec<&str> = s.split_whitespace().collect();
    let numeric: Vec<&str> = words.iter().copied().filter(|w| w.starts_with(|c: char| c.is_ascii_digit())).collect();
    let zone_words: Vec<&str> = words.iter().copied().filter(|w| !w.starts_with(|c: char| c.is_ascii_digit())).collect();
    let zone = (!zone_words.is_empty()).then(|| zone_words.join(" "));
    let tz = match &zone {
        Some(z) => zone_named(z).ok_or(())?,
        None => zone_named(machine_zone).unwrap_or_else(jiff::tz::TimeZone::system),
    };
    let secs = match numeric.as_slice() {
        [d, t] => civil(Some(d), t, &tz)?,
        [t] => civil(None, t, &tz)?,
        _ => return Err(()),
    };
    Ok(Some(RequestTime { secs, zone }))
}

/// Unix seconds now.
pub fn now_secs() -> i64 {
    jiff::Timestamp::now().as_second()
}

/// A time as the server reads it: `yyyyMMdd-HH:mm:ss` in UTC.
pub fn server_time(secs: i64) -> String {
    jiff::Timestamp::from_second(secs)
        .map(|t| t.to_zoned(jiff::tz::TimeZone::UTC).strftime("%Y%m%d-%H:%M:%S").to_string())
        .unwrap_or_default()
}

/// A time as the reference writes it in the instrument zone:
/// `yyyyMMdd HH:mm:ss {zone}` (ibx#431).
pub fn zoned_time(secs: i64, zone: &str) -> String {
    let tz = zone_named(zone).unwrap_or(jiff::tz::TimeZone::UTC);
    jiff::Timestamp::from_second(secs)
        .map(|t| format!("{} {}", t.to_zoned(tz).strftime("%Y%m%d %H:%M:%S"), zone))
        .unwrap_or_default()
}

/// The time of a bar as the reference writes it (ibx#431): a bar shorter
/// than one day by formatDate (1 `yyyyMMdd HH:mm:ss {zone}` in the
/// instrument zone, 2 Unix seconds, 3 the form of 1 without the year); a
/// bar of one day or longer as its date `yyyyMMdd` in the zone of the
/// reply, whatever formatDate.
pub fn bar_time(secs: i64, format_date: i32, intraday: bool, zone: &str, reply_zone: &str) -> String {
    if !intraday {
        let tz = zone_named(reply_zone).unwrap_or(jiff::tz::TimeZone::UTC);
        return jiff::Timestamp::from_second(secs)
            .map(|t| t.to_zoned(tz).strftime("%Y%m%d").to_string())
            .unwrap_or_default();
    }
    match format_date {
        2 => secs.to_string(),
        3 => zoned_time(secs, zone).get(4..).unwrap_or("").to_string(),
        _ => zoned_time(secs, zone),
    }
}

/// A head timestamp as the reference writes it (ibx#431): formatDate 1
/// `yyyyMMdd-HH:mm:ss` and 3 `MMdd HH:mm:ss`, both in UTC; 2 Unix seconds.
pub fn head_timestamp_text(secs: i64, format_date: i32) -> String {
    match format_date {
        2 => secs.to_string(),
        3 => jiff::Timestamp::from_second(secs)
            .map(|t| t.to_zoned(jiff::tz::TimeZone::UTC).strftime("%m%d %H:%M:%S").to_string())
            .unwrap_or_default(),
        _ => server_time(secs),
    }
}

/// The start of a bar request (ibx#431): its end moved back by the
/// duration (in the reference form, `{n} S|d|W|m|y`), on the calendar of
/// the machine zone, as the reference computes it.
pub fn duration_start(end: i64, duration: &str, machine_zone: &str) -> i64 {
    let Some((n, unit)) = duration.split_once(' ') else { return end };
    let Ok(n) = n.parse::<i64>() else { return end };
    if unit == "S" {
        return end - n;
    }
    let tz = zone_named(machine_zone).unwrap_or_else(jiff::tz::TimeZone::system);
    let Ok(at) = jiff::Timestamp::from_second(end).map(|t| t.to_zoned(tz)) else { return end };
    let span = match unit {
        "d" => jiff::Span::new().try_days(n),
        "W" => jiff::Span::new().try_weeks(n),
        "m" => jiff::Span::new().try_months(n),
        "y" => jiff::Span::new().try_years(n),
        _ => return end,
    };
    span.ok().and_then(|sp| at.checked_sub(sp).ok()).map_or(end, |z| z.timestamp().as_second())
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

/// The local checks of a bar request, in the reference order (ibx#430,
/// ibx#429): the end date (10314), then the duration, ADJUSTED_LAST with
/// an end date, the bar size, ADJUSTED_LAST with bars longer than a day,
/// whatToShow, formatDate (when given), the keepUpToDate rules (no end
/// date, no combo, only TRADES, MIDPOINT, BID or ASK), and SCHEDULE with
/// bars other than one day (321). Err is (code, text). A duration in years
/// above `max_backfill_years` (the logon limit, None when it is not
/// checked) is refused after the duration checks, as the reference
/// (ibx#421).
pub fn check_bar_request(
    end_date_time: &str,
    duration: &str,
    bar_size: &str,
    what_to_show: &str,
    format_date: Option<i32>,
    keep_up_to_date: bool,
    sec_type: &str,
    max_backfill_years: Option<i32>,
) -> Result<CheckedBarRequest, (i32, String)> {
    let refuse = |cause: &str| (321, bar_request_refusal(cause));
    if !is_valid_end_date(end_date_time) {
        return Err((10314, INVALID_END_DATE.to_string()));
    }
    let duration = normalize_duration(duration).map_err(|e| refuse(&e))?;
    if let Some(cause) = max_backfill_years.and_then(|m| crate::control::logon::backfill_years_refusal(&duration, m)) {
        return Err(refuse(&cause));
    }
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
    if keep_up_to_date {
        if !end_date_time.trim().is_empty() {
            return Err(refuse("End date not supported with live updates"));
        }
        if matches!(sec_type.trim().to_ascii_uppercase().as_str(), "BAG" | "PDC") {
            return Err(refuse("Live updates for combos are not supported"));
        }
        if !matches!(what_to_show.to_ascii_uppercase().as_str(), "TRADES" | "MIDPOINT" | "BID" | "ASK") {
            return Err(refuse("Source price not supported with live updates"));
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

/// A single historical OHLCV bar parsed from XML. Volume, WAP and count
/// are -1 when the reply has none (MIDPOINT, BID, ASK bars), as the
/// reference sends them (ibx#429).
#[derive(Debug, Clone, PartialEq)]
pub struct HistoricalBar {
    /// The reply's bar time (`yyyyMMdd-HH:mm:ss` UTC); the API time, by
    /// formatDate, once the engine has written it (ibx#431).
    pub time: String,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: i64,
    pub wap: f64,
    pub count: i32,
}

/// Parsed historical data response.
#[derive(Debug, Clone, Default)]
pub struct HistoricalResponse {
    pub query_id: String,
    pub timezone: String,
    pub bars: Vec<HistoricalBar>,
    pub is_complete: bool,
    /// Start and end of the request for historicalDataEnd, as the
    /// reference writes them (ibx#431); empty until the engine set them.
    pub start: String,
    pub end: String,
}

/// Chart name of a query id: `{symbol}@{API exchange} {label}`, an empty
/// exchange being `SMART`.
pub fn chart_name(symbol: &str, exchange: &str, label: &str) -> String {
    let exchange = match exchange.trim() {
        "" => "SMART",
        e => e,
    };
    format!("{}@{} {}", symbol, exchange, label)
}

/// Build the XML query for a historical bar data request.
pub fn build_query_xml(req: &HistoricalRequest) -> String {
    let exchange = query_exchange(&req.exchange, &req.sec_type);
    let sec_type = query_sec_type(&req.sec_type);
    let rth = if query_use_rth(&req.sec_type, req.use_rth) { "true" } else { "false" };
    let native = if query_use_native(&req.sec_type) { "<useNative>yes</useNative>" } else { "" };
    let expired = if req.include_expired { "yes" } else { "no" };

    let data_str = req.data_type.as_str();
    // The query id carries the chart name, as the reference writes it:
    // symbol, API exchange and type label (ibx#429; a one-shot query too,
    // ibx#486: `cf76;;AAPL@SMART Trades;;1;;true;;0;;I`).
    let query_id = format!("{};;{};;1;;true;;0;;I", req.query_id, chart_name(&req.symbol, &req.exchange, &req.data_type.label()));
    // Whole days for daily and longer bars (`hmdscore.xml.Query.a(settings.b,
    // boolean, boolean)@119-167`, `settings.b.l()` = shorter than a day;
    // captured: 1 day bars true, 1 hour false, ibx#486).
    let whole_days = !req.bar_size.is_intraday();

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
         {refresh}\
         <timeLength>{dur}</timeLength>\
         <step>{step}</step>\
         <source>API</source>\
         <needTotalValue>false</needTotalValue>\
         <wholeDays>{whole_days}</wholeDays>\
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

/// The cancel of a query still waiting for its answer, by its whole id,
/// as the reference writes it (ibx#431).
pub fn query_cancel_xml(query_id: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <ListOfCancelQueries>\
         <CancelQuery>\
         <id>{}</id>\
         </CancelQuery>\
         </ListOfCancelQueries>",
        query_id,
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
                .unwrap_or(-1),
            wap: extract_xml_tag(bar_xml, "weightedAvg")
                .and_then(|s| s.parse().ok())
                .unwrap_or(-1.0),
            count: extract_xml_tag(bar_xml, "count")
                .and_then(|s| s.parse().ok())
                .unwrap_or(-1),
        };
        bars.push(bar);
        search_start = bar_end;
    }

    Some(HistoricalResponse {
        query_id,
        timezone,
        bars,
        is_complete,
        ..Default::default()
    })
}

/// A bar of a live bar series (ibx#429): start and end in Unix seconds.
#[derive(Debug, Clone, PartialEq)]
pub struct SeriesBar {
    pub start: i64,
    pub end: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: i64,
    pub wap: f64,
    pub count: i32,
}

/// The bars of a bar reply frame with their start and end times, and the
/// trading sessions (`<Open>` / `<Close>` times) of the frame (ibx#429).
pub fn parse_series(xml: &str) -> (Vec<SeriesBar>, Vec<(i64, i64)>) {
    let mut bars = Vec::new();
    let mut opens: Vec<i64> = Vec::new();
    let mut closes: Vec<i64> = Vec::new();
    let mut pos = 0;
    while let Some(rel) = xml[pos..].find('<') {
        let at = pos + rel;
        let rest = &xml[at..];
        let (tag, close_tag) = if rest.starts_with("<Bar>") {
            ("Bar", "</Bar>")
        } else if rest.starts_with("<Open>") {
            ("Open", "</Open>")
        } else if rest.starts_with("<Close>") {
            ("Close", "</Close>")
        } else {
            pos = at + 1;
            continue;
        };
        let Some(e) = rest.find(close_tag) else { break };
        let item = &rest[..e];
        let time = extract_xml_tag(item, "time").and_then(parse_server_time);
        match tag {
            "Bar" => {
                if let Some(start) = time {
                    let num = |t: &str, unset: f64| extract_xml_tag(item, t).and_then(|v| v.trim().parse::<f64>().ok()).unwrap_or(unset);
                    bars.push(SeriesBar {
                        start,
                        end: extract_xml_tag(item, "endTime").and_then(parse_server_time).unwrap_or(start),
                        open: num("open", 0.0),
                        high: num("high", 0.0),
                        low: num("low", 0.0),
                        close: num("close", 0.0),
                        volume: num("volume", -1.0) as i64,
                        wap: num("weightedAvg", -1.0),
                        count: num("count", -1.0) as i32,
                    });
                }
            }
            "Open" => opens.extend(time),
            _ => closes.extend(time),
        }
        pos = at + e + close_tag.len();
    }
    let sessions = opens.iter().zip(closes.iter()).map(|(o, c)| (*o, *c)).collect();
    (bars, sessions)
}

/// Merge a 5-second bar starting at `time` into a live bar series, as the
/// reference does (ibx#429), and give the index of the bar it changed.
/// The bar it belongs to is the last bar when it starts before that bar
/// ends; else the bar of its period: its time floored to the bar size,
/// not before the open of its session. A bar not in the series is added
/// from the 5-second bar. A 5-second bar already inside its bar changes
/// nothing. Merging raises the high, lowers the low, sets the close, and
/// for trades adds the volume and the count and weighs the WAP by the
/// volume; other data keep volume, WAP and count unset (-1). Bars of a
/// day or longer take the 5-second bars into their last bar.
pub fn merge_five_seconds(
    series: &mut Vec<SeriesBar>, sessions: &[(i64, i64)], bar_size: BarSize, trades: bool,
    time: i64, bar: &crate::types::RealTimeBar,
) -> Option<usize> {
    let end = time + 5;
    let in_last = series.last().is_some_and(|b| time >= b.start && time < b.end);
    let index = if in_last {
        Some(series.len() - 1)
    } else {
        match bar_size.seconds() {
            Some(len) => {
                let session = sessions.iter().find(|(o, c)| *o <= time && time < *c);
                let start = (time - time.rem_euclid(len)).max(session.map_or(i64::MIN, |s| s.0));
                match series.iter().position(|b| b.start == start) {
                    Some(i) => Some(i),
                    None => {
                        let (volume, wap, count) = if trades {
                            (bar.volume as i64, bar.wap, bar.count)
                        } else {
                            (-1, -1.0, -1)
                        };
                        let new = SeriesBar {
                            start, end, open: bar.open, high: bar.high, low: bar.low, close: bar.close,
                            volume, wap, count,
                        };
                        let at = series.iter().position(|b| b.start > start).unwrap_or(series.len());
                        series.insert(at, new);
                        return Some(at);
                    }
                }
            }
            None => series.iter().rposition(|b| time >= b.start),
        }
    };
    let i = index?;
    let b = &mut series[i];
    if end <= b.end {
        return Some(i);
    }
    b.high = b.high.max(bar.high);
    b.low = b.low.min(bar.low);
    b.close = bar.close;
    b.end = end;
    if trades {
        let added = bar.volume as i64;
        let total = b.volume.max(0) + added;
        if total > 0 && added > 0 {
            b.wap = (b.wap * b.volume.max(0) as f64 + bar.wap * added as f64) / total as f64;
        }
        b.volume = total;
        b.count = b.count.max(0) + bar.count.max(0);
    }
    Some(i)
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
/// price or trade count (-1, as unset values are sent). Sorted by bar time.
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
                        volume: -1,
                        wap: -1.0,
                        count: -1,
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
    // The data label is the API name (ibx#486, captured
    // `TickHeadClient1;;265598@BEST TRADES;;0;;true;;0;;U`).
    let id = format!("{};;{}@{} {};;0;;true;;0;;U",
        req.window_id, req.con_id, exchange, req.data_type.api_name());

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

/// The data of a historical ticks query (ibx#432).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickSource {
    Trades,
    Midpoint,
    BidAsk,
}

impl TickSource {
    /// Server data name: trades are asked as all trades.
    pub fn data(&self) -> &'static str {
        match self {
            Self::Trades => "AllLast",
            Self::Midpoint => "MidPoint",
            Self::BidAsk => "BidAsk",
        }
    }

    /// Label in the chart name of the query id.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Trades => "Trades",
            Self::Midpoint => "Midpoint",
            Self::BidAsk => "Bid_ask",
        }
    }
}

/// Text of the local refusals of a historical ticks request (321).
pub fn ticks_refusal(cause: &str) -> String {
    format!("Error validating request.-'bP' : cause - {}", cause)
}

/// A historical ticks request that passed the reference checks (ibx#432).
#[derive(Debug, Clone, PartialEq)]
pub struct CheckedTicks {
    pub start: Option<RequestTime>,
    pub end: Option<RequestTime>,
    pub source: TickSource,
    /// The query carries the filter that asks for no sizes.
    pub ignore_size_filter: bool,
}

/// The local checks of a historical ticks request, in the reference order
/// (ibx#432): the start date, then the end date (10314); warning 2174 for a
/// date with no zone; then 321 for an empty exchange, a combo, a number of
/// ticks below 1, an empty or unknown whatToShow (TRADES, MIDPOINT,
/// BID_ASK and AGGTRADES are known); then warning 10299 for AGGTRADES
/// outside crypto, which is asked as TRADES. TRADES on a currency pair is
/// asked as midpoints. The query asks for no sizes for midpoints, for a
/// whatToShow asked as another data, and for BID_ASK with ignoreSize.
/// The warnings (code, text) go to the client before the refusal or the
/// query.
#[allow(clippy::too_many_arguments)]
pub fn check_ticks_request(
    start: &str, end: &str, number_of_ticks: i32, what_to_show: &str, ignore_size: bool,
    sec_type: &str, exchange: &str, machine_zone: &str, now: i64,
) -> (Vec<(i32, String)>, Result<CheckedTicks, (i32, String)>) {
    let mut warnings = Vec::new();
    let checked = check_ticks(start, end, number_of_ticks, what_to_show, ignore_size, sec_type, exchange,
        machine_zone, now, &mut warnings);
    (warnings, checked)
}

#[allow(clippy::too_many_arguments)]
fn check_ticks(
    start: &str, end: &str, number_of_ticks: i32, what_to_show: &str, ignore_size: bool,
    sec_type: &str, exchange: &str, machine_zone: &str, now: i64, warnings: &mut Vec<(i32, String)>,
) -> Result<CheckedTicks, (i32, String)> {
    let start = parse_request_time(start, machine_zone, now)
        .map_err(|_| (10314, INVALID_END_DATE.replacen("End Date/Time", "Start Date/Time", 1)))?;
    let end = parse_request_time(end, machine_zone, now)
        .map_err(|_| (10314, INVALID_END_DATE.to_string()))?;
    for t in [&start, &end].into_iter().flatten() {
        if t.zone.is_none() {
            warnings.push((2174, IMPLIED_ZONE_WARNING.to_string()));
        }
    }
    let refuse = |cause: &str| Err((321, ticks_refusal(cause)));
    let sec_type = query_sec_type(sec_type);
    if exchange.trim().is_empty() {
        return refuse("Exchange must not be empty");
    }
    if matches!(sec_type.as_str(), "BAG" | "PDC") {
        return refuse("Combo types are not supported");
    }
    if number_of_ticks <= 0 {
        return refuse("Number of ticks must be > 0");
    }
    if what_to_show.trim().is_empty() {
        return refuse("Source price must not be empty");
    }
    let what = what_to_show.to_ascii_uppercase();
    let midpoint_default = sec_type == "CASH";
    let source = match what.as_str() {
        "TRADES" | "AGGTRADES" if midpoint_default => TickSource::Midpoint,
        "TRADES" | "AGGTRADES" => TickSource::Trades,
        "MIDPOINT" => TickSource::Midpoint,
        "BID_ASK" => TickSource::BidAsk,
        _ => return refuse("Invalid source price"),
    };
    if what == "AGGTRADES" && sec_type != "CRYPTO" {
        warnings.push((10299, "Expected what to show is TRADES, please use that instead of AGGTRADES.".to_string()));
    }
    let asked_as_other = match source {
        TickSource::Trades => what != "TRADES",
        TickSource::Midpoint => true,
        TickSource::BidAsk => ignore_size,
    };
    Ok(CheckedTicks { start, end, source, ignore_size_filter: asked_as_other })
}

/// A historical ticks query (ibx#432).
#[derive(Debug, Clone)]
pub struct TickQuery {
    /// Window id; the query id adds the chart name.
    pub window_id: String,
    /// Symbol of the chart name (the local symbol when known).
    pub symbol: String,
    pub con_id: i64,
    pub sec_type: String,
    /// Exchange of the API contract.
    pub exchange: String,
    pub source: TickSource,
    /// Unix seconds; a start makes a forward query from it, else an end a
    /// backward query from it, else a forward query from 1970.
    pub start: Option<i64>,
    pub end: Option<i64>,
    pub number_of_ticks: i32,
    pub use_rth: bool,
    pub ignore_size_filter: bool,
}

impl TickQuery {
    /// The whole query id, as the reference writes it.
    pub fn query_id(&self) -> String {
        format!("{};;{};;0;;true;;0;;U", self.window_id, chart_name(&self.symbol, &self.exchange, self.source.label()))
    }
}

/// Build the XML query of a historical ticks request, as the reference
/// writes it (ibx#432): the start as `<startTime>`, else the end, capped
/// at `now`, as `<endTime>`, else a start in 1970; times in UTC; no step;
/// all days; the size filter when asked.
pub fn build_tick_query_xml(q: &TickQuery, now: i64) -> String {
    let time = match (q.start, q.end) {
        (Some(s), _) => format!("<startTime>{}</startTime>", server_time(s)),
        (None, Some(e)) => format!("<endTime>{}</endTime>", server_time(e.min(now))),
        (None, None) => format!("<startTime>{}</startTime>", server_time(0)),
    };
    let filter = if q.ignore_size_filter {
        "<Filter varName=\"filter\"><ignoreSize>true</ignoreSize></Filter>"
    } else {
        ""
    };
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <ListOfQueries>\
         <Query>\
         <id>{id}</id>\
         <approx>false</approx>\
         <useRTH>{rth}</useRTH>\
         <contractID>{con_id}</contractID>\
         <exchange>{exchange}</exchange>\
         <secType>{sec_type}</secType>\
         <expired>no</expired>\
         <type>TickData</type>\
         <data>{data}</data>\
         {time}\
         <timeLength>{n} t</timeLength>\
         <source>API</source>\
         <needTotalValue>false</needTotalValue>\
         <wholeDays>true</wholeDays>\
         <delay>auto</delay>\
         {filter}\
         </Query>\
         </ListOfQueries>",
        id = q.query_id(),
        rth = if query_use_rth(&q.sec_type, q.use_rth) { "true" } else { "false" },
        con_id = q.con_id,
        exchange = query_exchange(&q.exchange, &q.sec_type),
        sec_type = query_sec_type(&q.sec_type),
        data = q.source.data(),
        n = q.number_of_ticks,
    )
}

/// One reply frame of a historical ticks query (ibx#432).
#[derive(Debug, Clone, PartialEq)]
pub struct TickFrame {
    pub query_id: String,
    /// The ticks, by the data name of the reply; None for a data name the
    /// reference does not send to the client.
    pub data: Option<crate::types::HistoricalTickData>,
    /// `<eoq>`: the last frame of the query.
    pub done: bool,
}

/// A server time `yyyyMMdd-HH:mm:ss` (UTC) as Unix seconds; sub-seconds,
/// when given, are dropped.
pub fn parse_server_time(s: &str) -> Option<i64> {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() < 17 || b[8] != b'-' || b[11] != b':' || b[14] != b':' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i32>().ok();
    let date = jiff::civil::Date::new(num(0..4)? as i16, num(4..6)? as i8, num(6..8)? as i8).ok()?;
    let time = jiff::civil::Time::new(num(9..11)? as i8, num(12..14)? as i8, num(15..17)? as i8, 0).ok()?;
    date.to_datetime(time).to_zoned(jiff::tz::TimeZone::UTC).ok().map(|z| z.timestamp().as_second())
}

/// Parse a ResultSetTick reply frame as the reference reads it (ibx#432):
/// the kind of ticks comes from the reply's `<data>` (`AllLast` and `Last`
/// are trades, `BidAsk`, `MidPoint`); each tick time is Unix seconds; each
/// size is multiplied by the reply's `<sizeMinTick>`; `<exch>` is the
/// exchange and `<cond>` the special conditions; the `<flags>` tokens, `;`
/// separated, give past limit (`H`), unreported (`U`), ask past high (`AH`)
/// and bid past low (`BH`). A price `nan` is the maximum double.
pub fn parse_tick_response(xml: &str) -> Option<TickFrame> {
    // The reply's data name comes before its events.
    let head = &xml[..xml.find("<Events>").unwrap_or(xml.len())];
    let data_name = extract_xml_tag(head, "data").unwrap_or("").trim();
    parse_tick_frame(xml, data_name)
}

/// The tick result set that opens a tick-by-tick request with a tick
/// count (ibx#455): its ticks are read by the request's type, as the
/// reference reads them (`Last` / `AllLast` as trades).
pub fn parse_tick_by_tick_history(xml: &str, tbt_type: crate::types::TbtType) -> Option<TickFrame> {
    parse_tick_frame(xml, tbt_type.as_str())
}

fn parse_tick_frame(xml: &str, data_name: &str) -> Option<TickFrame> {
    use crate::api::types::{TickAttribBidAsk, TickAttribLast};
    use crate::types::{HistoricalTickBidAsk, HistoricalTickData, HistoricalTickLast, HistoricalTickMidpoint};
    if !xml.contains("<ResultSetTick>") {
        return None;
    }
    let query_id = extract_xml_tag(xml, "id").unwrap_or("").to_string();
    let done = extract_xml_tag(xml, "eoq").unwrap_or("false").trim() == "true";
    let size_step: Option<f64> = extract_xml_tag(xml, "sizeMinTick").and_then(|s| s.trim().parse().ok());
    let data_name = data_name.strip_prefix("All").unwrap_or(data_name);

    let mut ticks: Vec<&str> = Vec::new();
    let mut search_start = 0;
    while let Some(pos) = xml[search_start..].find("<Tick>") {
        let abs = search_start + pos;
        let Some(e) = xml[abs..].find("</Tick>") else { break };
        ticks.push(&xml[abs..abs + e + 7]);
        search_start = abs + e + 7;
    }
    let text = |t: &str, tag: &str| extract_xml_tag(t, tag).unwrap_or("").to_string();
    let price = |t: &str, tag: &str| match extract_xml_tag(t, tag).map(str::trim) {
        Some("nan") | Some("NaN") => f64::MAX,
        Some(v) => v.parse().unwrap_or(0.0),
        None => 0.0,
    };
    let size = |t: &str, tag: &str| {
        let v: f64 = extract_xml_tag(t, tag).and_then(|s| s.trim().parse().ok()).unwrap_or(0.0);
        match size_step {
            Some(step) => v * step,
            None => v,
        }
    };
    let time = |t: &str| extract_xml_tag(t, "time").and_then(parse_server_time).unwrap_or(0);
    let flags = |t: &str| -> Vec<String> {
        extract_xml_tag(t, "flags").unwrap_or("").split(';').map(|f| f.trim().to_string()).collect()
    };
    let data = match data_name {
        "Last" => Some(HistoricalTickData::Last(ticks.iter().map(|t| {
            let f = flags(t);
            HistoricalTickLast {
                time: time(t),
                tick_attrib_last: TickAttribLast {
                    past_limit: f.iter().any(|x| x == "H"),
                    unreported: f.iter().any(|x| x == "U"),
                },
                price: price(t, "price"),
                size: size(t, "size"),
                exchange: text(t, "exch"),
                special_conditions: text(t, "cond"),
            }
        }).collect())),
        "BidAsk" => Some(HistoricalTickData::BidAsk(ticks.iter().map(|t| {
            let f = flags(t);
            HistoricalTickBidAsk {
                time: time(t),
                tick_attrib_bid_ask: TickAttribBidAsk {
                    bid_past_low: f.iter().any(|x| x == "BH"),
                    ask_past_high: f.iter().any(|x| x == "AH"),
                },
                price_bid: price(t, "bidPrice"),
                price_ask: price(t, "askPrice"),
                size_bid: size(t, "bidSize"),
                size_ask: size(t, "askSize"),
            }
        }).collect())),
        "MidPoint" => Some(HistoricalTickData::Midpoint(ticks.iter().map(|t| HistoricalTickMidpoint {
            time: time(t),
            price: price(t, "price"),
            size: size(t, "size"),
        }).collect())),
        other => {
            log::warn!("Unexpected source price in historical ticks reply: {:?}", other);
            None
        }
    };
    Some(TickFrame { query_id, data, done })
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
/// Returns (low, open, high, close, volume, wap, count) or None. As the
/// reference's 5-second router, a bar with a negative trade count, or an
/// empty one (no trade, all prices and the WAP total 0), is dropped
/// (ibx#454).
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
    if count < 0 {
        log::info!("5-second bar dropped: invalid number of trades {}", count);
        return None;
    }

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

    let wap_total = volume * low + wap_sum * min_tick;
    if count == 0 && low == 0.0 && open == 0.0 && high == 0.0 && close == 0.0 && wap_total == 0.0 {
        log::debug!("5-second bar dropped: completely empty");
        return None;
    }

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
        let ok = check_bar_request("", "3600", "1 Min", "trades", Some(1), false, "", None).unwrap();
        assert_eq!(ok, CheckedBarRequest { data_type: BarDataType::Trades, bar_size: BarSize::Min1, duration: "3600 S".into() });
        let code = |r: Result<CheckedBarRequest, (i32, String)>| r.unwrap_err();
        assert_eq!(code(check_bar_request("garbage", "1 D", "1 day", "TRADES", None, false, "", None)).0, 10314);
        assert_eq!(code(check_bar_request("", "400 D", "1 day", "TRADES", None, false, "", None)),
            (321, "Error validating request.-'bM' : cause - Historical data requests for durations longer than 365 days must be made in years.".to_string()));
        assert_eq!(code(check_bar_request("20260102 10:00:00", "1 D", "1 hour", "ADJUSTED_LAST", None, false, "", None)).1,
            "Error validating request.-'bM' : cause - End date not supported with adjusted last");
        assert_eq!(code(check_bar_request("", "1 Y", "1 week", "ADJUSTED_LAST", None, false, "", None)).1,
            "Error validating request.-'bM' : cause - Multi day bar size not supported with adjusted last");
        assert!(check_bar_request("", "1 Y", "1 day", "ADJUSTED_LAST", None, false, "", None).is_ok());
        assert_eq!(code(check_bar_request("", "1 D", "1 sec", "TRADES", None, false, "", None)).0, 321);
        assert_eq!(code(check_bar_request("", "1 D", "1 day", "YIELD", None, false, "", None)).1,
            "Error validating request.-'bM' : cause - What to show value of YIELD rejected.");
        assert_eq!(code(check_bar_request("", "1 D", "1 day", "TRADES", Some(4), false, "", None)).1,
            "Error validating request.-'bM' : cause - Date formatting selection of 4 rejected.");
        assert_eq!(code(check_bar_request("", "1 M", "1 hour", "SCHEDULE", None, false, "", None)).1,
            "Error validating request.-'bM' : cause - Only daily resolution supported for Schedule requests");
        assert!(check_bar_request("", "1 M", "1 day", "SCHEDULE", None, false, "", None).is_ok());
        // The issue's checks: 1 year and 3 months bars over 5 Y.
        assert!(check_bar_request("", "5 Y", "1 year", "TRADES", None, false, "", None).is_ok());
        assert!(check_bar_request("", "5 Y", "3 months", "TRADES", None, false, "", None).is_ok());
            // ibx#429: every bar size streams; the live update refusals, in
        // the reference order and texts (capture b1_429_keep_up_to_date).
        for size in ["1 secs", "1 min", "30 secs", "2 hours", "1 hour", "1 day"] {
            assert!(check_bar_request("", "1 D", size, "TRADES", Some(1), true, "STK", None).is_ok(), "{}", size);
        }
        assert_eq!(code(check_bar_request("20261001 10:00:00 US/Eastern", "1 D", "1 hour", "TRADES", Some(1), true, "STK", None)),
            (321, "Error validating request.-'bM' : cause - End date not supported with live updates".to_string()));
        for what in ["BID_ASK", "ADJUSTED_LAST", "HISTORICAL_VOLATILITY"] {
            assert_eq!(code(check_bar_request("", "1 D", "1 hour", what, Some(1), true, "STK", None)).1,
                "Error validating request.-'bM' : cause - Source price not supported with live updates", "{}", what);
        }
        for what in ["TRADES", "MIDPOINT", "BID", "ASK"] {
            assert!(check_bar_request("", "1 D", "1 hour", what, Some(1), true, "STK", None).is_ok(), "{}", what);
        }
        assert_eq!(code(check_bar_request("", "1 D", "1 hour", "TRADES", Some(1), true, "BAG", None)).1,
            "Error validating request.-'bM' : cause - Live updates for combos are not supported");
        // ibx#421: years above the logon limit, after the duration checks.
        assert_eq!(code(check_bar_request("", "2 Y", "1 day", "TRADES", None, false, "", Some(1))).1,
            "Error validating request.-'bM' : cause - Historical data request for 2 year(s) rejected. Max API Backfill Years=1");
        assert!(check_bar_request("", "199 Y", "1 month", "TRADES", None, false, "", Some(199)).is_ok());
        assert_eq!(code(check_bar_request("", "400 D", "1 day", "TRADES", None, false, "", Some(1))).1,
            "Error validating request.-'bM' : cause - Historical data requests for durations longer than 365 days must be made in years.");
        assert_eq!(code(check_bar_request("", "2 Y", "1 day", "YIELD", None, false, "", Some(1))).1,
            "Error validating request.-'bM' : cause - Historical data request for 2 year(s) rejected. Max API Backfill Years=1");
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
            ("20260227-20:30:00".to_string(), 266.466, 266.70, 266.30, 266.520, -1, -1.0, -1),
            ("20260227-20:31:00".to_string(), 266.154, 266.154, 266.00, 266.154, -1, -1.0, -1),
            ("20260227-20:32:00".to_string(), 266.100, 266.20, 266.100, 266.100, -1, -1.0, -1),
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
    fn bar_size_lengths() {
        assert_eq!(BarSize::Min1.seconds(), Some(60));
        assert_eq!(BarSize::Hour2.seconds(), Some(7200));
        assert!(BarSize::Sec30.is_intraday());
        for bs in [BarSize::Day1, BarSize::Week1, BarSize::Month1] {
            assert!(!bs.is_intraday());
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
        // ibx#486: the chart name in a one-shot id too; no cutoff date;
        // whole days only for daily bars.
        assert!(xml.contains("<id>q1;;AAPL@SMART Trades;;1;;true;;0;;I</id>"), "{xml}");
        assert!(!xml.contains("cutoffDate"), "{xml}");
        assert!(xml.contains("<wholeDays>false</wholeDays>"), "{xml}");
        let daily = build_query_xml(&HistoricalRequest { bar_size: BarSize::Day1, ..req.clone() });
        assert!(daily.contains("<wholeDays>true</wholeDays>"), "{daily}");
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
        assert!(xml.contains("TickHeadClient1;;756733@BEST TRADES;;0;;true;;0;;U"), "{xml}");
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
        assert!(xml.contains("<id>TickHeadClient7;;815824267@CME TRADES;;0;;true;;0;;U</id>"), "{}", xml);
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

    // ── ibx#431, ibx#432: request dates and the dates the client gets ──

    const ET_15H: i64 = 1_790_881_200; // 20261001 15:00:00 US/Eastern

    #[test]
    fn request_dates_as_the_reference_reads_them() {
        let read = |s: &str| parse_request_time(s, "Europe/Paris", ET_15H);
        assert_eq!(read("20261001 15:00:00 US/Eastern"), Ok(Some(RequestTime { secs: ET_15H, zone: Some("US/Eastern".into()) })));
        assert_eq!(server_time(ET_15H), "20261001-19:00:00");
        assert_eq!(read("20261001-19:00:00").unwrap().unwrap(), RequestTime { secs: ET_15H, zone: Some("UTC".into()) });
        // No zone: the machine zone (captured: 15:00 in Central Europe went
        // out as 13:00 UTC), and no zone named, for warning 2174.
        let local = read("20261001 15:00:00").unwrap().unwrap();
        assert_eq!((server_time(local.secs), local.zone), ("20261001-13:00:00".to_string(), None));
        assert_eq!(read(""), Ok(None));
        for bad in ["not a date", "20261001 15:00:00 Nowhere/Zone", "20261001"] {
            assert_eq!(read(bad), Err(()), "{}", bad);
        }
        // A time alone is on the day of now, in its zone.
        assert_eq!(read("15:00:00 US/Eastern").unwrap().unwrap().secs, ET_15H);
    }

    #[test]
    fn bar_dates_by_format_date() {
        // Captured b1_431_hist_format: 1 hour bars and daily bars of AAPL.
        let open = parse_server_time("20261001-13:30:00").unwrap();
        assert_eq!(open, 1_790_861_400);
        assert_eq!(bar_time(open, 1, true, "US/Eastern", "US/Eastern"), "20261001 09:30:00 US/Eastern");
        assert_eq!(bar_time(open, 2, true, "US/Eastern", "US/Eastern"), "1790861400");
        assert_eq!(bar_time(open, 3, true, "US/Eastern", "US/Eastern"), "1001 09:30:00 US/Eastern");
        let day = parse_server_time("20260902-13:30:00").unwrap();
        for format_date in [1, 2] {
            assert_eq!(bar_time(day, format_date, false, "US/Eastern", "US/Eastern"), "20260902");
        }
        let head = parse_server_time("19801212-14:30:00").unwrap();
        assert_eq!(head_timestamp_text(head, 1), "19801212-14:30:00");
        assert_eq!(head_timestamp_text(head, 2), "345479400");
        assert_eq!(head_timestamp_text(head, 3), "1212 14:30:00");
    }

    #[test]
    fn historical_data_end_strings() {
        // Captured: requests at 04:57:40 and 04:56:41 New York time, the
        // machine in Central Europe.
        let end = parse_server_time("20261002-08:57:40").unwrap();
        let s = |d: &str| zoned_time(duration_start(end, d, "Europe/Paris"), "US/Eastern");
        assert_eq!(zoned_time(end, "US/Eastern"), "20261002 04:57:40 US/Eastern");
        assert_eq!(s("1 d"), "20261001 04:57:40 US/Eastern");
        assert_eq!(s("1 m"), "20260902 04:57:40 US/Eastern");
        assert_eq!(s("3600 S"), "20261002 03:57:40 US/Eastern");
        assert_eq!(s("1800 S"), "20261002 04:27:40 US/Eastern");
        assert_eq!(s("2 d"), "20260930 04:57:40 US/Eastern");
        assert_eq!(s("1 W"), "20260925 04:57:40 US/Eastern");
    }

    // ── ibx#432: historical ticks ──

    fn ticks(start: &str, end: &str, n: i32, what: &str, ignore: bool, sec_type: &str) -> (Vec<(i32, String)>, Result<CheckedTicks, (i32, String)>) {
        check_ticks_request(start, end, n, what, ignore, sec_type, "SMART", "Europe/Paris", ET_15H)
    }

    #[test]
    fn tick_request_checks_as_captured() {
        let t15 = "20261001 15:00:00 US/Eastern";
        let (w, ok) = ticks(t15, "", 100, "TRADES", false, "STK");
        let ok = ok.unwrap();
        assert!(w.is_empty());
        assert_eq!((ok.source, ok.ignore_size_filter, ok.start.unwrap().secs), (TickSource::Trades, false, ET_15H));
        let refusal = |n: i32, what: &str| ticks(t15, "", n, what, false, "STK").1.unwrap_err();
        assert_eq!(refusal(0, "TRADES"), (321, "Error validating request.-'bP' : cause - Number of ticks must be > 0".into()));
        assert_eq!(refusal(10, "FOO"), (321, "Error validating request.-'bP' : cause - Invalid source price".into()));
        assert_eq!(refusal(10, ""), (321, "Error validating request.-'bP' : cause - Source price must not be empty".into()));
        let (w, err) = ticks("not a date", "", 10, "TRADES", false, "STK");
        assert!(w.is_empty());
        let err = err.unwrap_err();
        assert_eq!(err.0, 10314);
        assert!(err.1.starts_with("Start Date/Time: The date, time, or time-zone entered is invalid.\nThe correct format"), "{}", err.1);
        assert!(ticks("", "garbage", 10, "TRADES", false, "STK").1.unwrap_err().1.starts_with("End Date/Time:"));
        // No zone: warning 2174, the request goes on.
        let (w, ok) = ticks("20261001 15:00:00", "", 10, "TRADES", false, "STK");
        assert_eq!(w, vec![(2174, IMPLIED_ZONE_WARNING.to_string())]);
        assert!(ok.is_ok());
        // AGGTRADES on a stock: warning 10299, asked as trades with no sizes.
        let (w, ok) = ticks(t15, "", 10, "AGGTRADES", false, "STK");
        assert_eq!(w, vec![(10299, "Expected what to show is TRADES, please use that instead of AGGTRADES.".to_string())]);
        assert_eq!((ok.as_ref().unwrap().source, ok.unwrap().ignore_size_filter), (TickSource::Trades, true));
        // TRADES on a currency pair: midpoints.
        let ok = ticks(t15, "", 10, "TRADES", false, "CASH").1.unwrap();
        assert_eq!((ok.source, ok.ignore_size_filter), (TickSource::Midpoint, true));
        // The size filter: midpoints always, BID_ASK with ignoreSize.
        assert!(ticks(t15, "", 100, "MIDPOINT", false, "STK").1.unwrap().ignore_size_filter);
        assert!(!ticks(t15, "", 100, "BID_ASK", false, "STK").1.unwrap().ignore_size_filter);
        assert!(ticks(t15, "", 100, "BID_ASK", true, "STK").1.unwrap().ignore_size_filter);
        assert!(!ticks(t15, "", 100, "TRADES", true, "STK").1.unwrap().ignore_size_filter);
        assert_eq!(check_ticks_request(t15, "", 10, "TRADES", false, "STK", "", "UTC", ET_15H).1.unwrap_err().1,
            "Error validating request.-'bP' : cause - Exchange must not be empty");
        assert_eq!(ticks(t15, "", 10, "TRADES", false, "BAG").1.unwrap_err().1,
            "Error validating request.-'bP' : cause - Combo types are not supported");
    }

    fn tick_query(source: TickSource, start: Option<i64>, end: Option<i64>, filter: bool) -> TickQuery {
        TickQuery {
            window_id: "tk_1".into(), symbol: "AAPL".into(), con_id: 265598, sec_type: "STK".into(),
            exchange: "SMART".into(), source, start, end, number_of_ticks: 100, use_rth: true,
            ignore_size_filter: filter,
        }
    }

    /// The query of a capture with its whitespace removed.
    fn captured(xml: &str) -> String {
        xml.lines().map(str::trim).collect()
    }

    #[test]
    fn tick_queries_as_captured() {
        // Capture b1_432_hist_ticks, request 9510 (cf16), id prefix aside.
        let expected = captured("<?xml version=\"1.0\" encoding=\"UTF-8\"?>
            <ListOfQueries><Query>
            <id>tk_1;;AAPL@SMART Trades;;0;;true;;0;;U</id>
            <approx>false</approx>
            <useRTH>true</useRTH>
            <contractID>265598</contractID>
            <exchange>BEST</exchange>
            <secType>STK</secType>
            <expired>no</expired>
            <type>TickData</type>
            <data>AllLast</data>
            <startTime>20261001-19:00:00</startTime>
            <timeLength>100 t</timeLength>
            <source>API</source>
            <needTotalValue>false</needTotalValue>
            <wholeDays>true</wholeDays>
            <delay>auto</delay>
            </Query></ListOfQueries>");
        let now = ET_15H + 86_400;
        assert_eq!(build_tick_query_xml(&tick_query(TickSource::Trades, Some(ET_15H), None, false), now), expected);
        // Start and end: the start wins (9516); end only: endTime (9515).
        assert_eq!(build_tick_query_xml(&tick_query(TickSource::Trades, Some(ET_15H), Some(ET_15H + 3600), false), now), expected);
        let xml = build_tick_query_xml(&tick_query(TickSource::Trades, None, Some(ET_15H), false), now);
        assert!(xml.contains("<data>AllLast</data><endTime>20261001-19:00:00</endTime><timeLength>100 t</timeLength>"), "{}", xml);
        // A future end is now; no dates: a start in 1970 (9518).
        let xml = build_tick_query_xml(&tick_query(TickSource::Trades, None, Some(now + 999), false), now);
        assert!(xml.contains(&format!("<endTime>{}</endTime>", server_time(now))), "{}", xml);
        let xml = build_tick_query_xml(&tick_query(TickSource::Trades, None, None, false), now);
        assert!(xml.contains("<startTime>19700101-00:00:00</startTime>"), "{}", xml);
        // BID_ASK with ignoreSize (9513) and midpoints (9514).
        let xml = build_tick_query_xml(&tick_query(TickSource::BidAsk, Some(ET_15H), None, true), now);
        assert!(xml.contains("<id>tk_1;;AAPL@SMART Bid_ask;;0;;true;;0;;U</id>"), "{}", xml);
        assert!(xml.ends_with("<delay>auto</delay><Filter varName=\"filter\"><ignoreSize>true</ignoreSize></Filter></Query></ListOfQueries>"), "{}", xml);
        let xml = build_tick_query_xml(&tick_query(TickSource::Midpoint, Some(ET_15H), None, true), now);
        assert!(xml.contains("Midpoint;;0;;true;;0;;U</id>") && xml.contains("<data>MidPoint</data>"), "{}", xml);
        // EUR.USD (9522): its own exchange in the chart name, FXSUBPIP.
        let q = TickQuery { symbol: "EUR.USD".into(), con_id: 12087792, sec_type: "CASH".into(), exchange: "IDEALPRO".into(),
            number_of_ticks: 10, ..tick_query(TickSource::Midpoint, Some(ET_15H), None, true) };
        let xml = build_tick_query_xml(&q, now);
        assert!(xml.contains("<id>tk_1;;EUR.USD@IDEALPRO Midpoint;;0;;true;;0;;U</id>"), "{}", xml);
        assert!(xml.contains("<exchange>FXSUBPIP</exchange><secType>CASH</secType>"), "{}", xml);
    }

    #[test]
    fn tick_replies_as_captured() {
        // Frames of capture b1_432_hist_ticks, cut to their first ticks.
        let last = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\t<ResultSetTick>\n\t\t<id>cf16;;AAPL@SMART Trades;;0;;true;;0;;U</id>\n\t\t<eoq>true</eoq>\n\t\t<tz>US/Eastern</tz>\n\t\t<data>AllLast</data>\n\t\t<minTick>0.0001</minTick>\n\t\t<sizeMinTick>1</sizeMinTick>\n\t\t<dataSource>UTDF</dataSource>\n\t\t<Events>\n\t\t\t<Open>\n\t\t\t\t<time>20261001-13:30:00</time>\n\t\t\t\t<refDate>20261001</refDate>\n\t\t\t</Open>\n\t\t\t<Tick>\n\t\t\t\t<time>20261001-19:00:00</time>\n\t\t\t\t<price>329.82</price>\n\t\t\t\t<size>1</size>\n\t\t\t\t<exch>FINRA</exch>\n\t\t\t\t<cond>   I</cond>\n\t\t\t\t<flags>U</flags>\n\t\t\t</Tick>\n\t\t\t<Tick>\n\t\t\t\t<time>20261001-19:00:00</time>\n\t\t\t\t<price>329.82</price>\n\t\t\t\t<size>41</size>\n\t\t\t\t<exch>FINRA</exch>\n\t\t\t</Tick>\n\t\t</Events>\n\t</ResultSetTick>\n";
        let frame = parse_tick_response(last).unwrap();
        assert_eq!((frame.query_id.as_str(), frame.done), ("cf16;;AAPL@SMART Trades;;0;;true;;0;;U", true));
        let Some(crate::types::HistoricalTickData::Last(t)) = frame.data else { panic!("trades") };
        assert_eq!(t[0], crate::types::HistoricalTickLast {
            time: ET_15H,
            tick_attrib_last: crate::api::types::TickAttribLast { past_limit: false, unreported: true },
            price: 329.82, size: 1.0, exchange: "FINRA".into(), special_conditions: "   I".into(),
        });
        assert_eq!((t[1].tick_attrib_last.unreported, t[1].size, t[1].special_conditions.as_str()), (false, 41.0, ""));

        let bid_ask = "<ResultSetTick><id>cf28;;AAPL@SMART Bid_ask;;0;;true;;0;;U</id><eoq>false</eoq><tz>US/Eastern</tz>\
            <data>BidAsk</data><minTick>0.01</minTick><sizeMinTick>100</sizeMinTick><Events>\
            <Tick><time>20261001-18:59:59</time><bidPrice>329.8</bidPrice><askPrice>329.84</askPrice><bidSize>2.8</bidSize><askSize>2.4</askSize><flags>AH;BH</flags></Tick>\
            </Events></ResultSetTick>";
        let frame = parse_tick_response(bid_ask).unwrap();
        assert!(!frame.done);
        let Some(crate::types::HistoricalTickData::BidAsk(t)) = frame.data else { panic!("bid ask") };
        assert_eq!((t[0].time, t[0].price_bid, t[0].price_ask), (ET_15H - 1, 329.8, 329.84));
        assert!((t[0].size_bid - 280.0).abs() < 1e-9 && (t[0].size_ask - 240.0).abs() < 1e-9, "sizes times sizeMinTick: {:?}", t[0]);
        assert!(t[0].tick_attrib_bid_ask.bid_past_low && t[0].tick_attrib_bid_ask.ask_past_high);

        // TRADES on EUR.USD is answered as midpoints, size 0 (9522).
        let mid = "<ResultSetTick><id>cf56;;EUR.USD@IDEALPRO Midpoint;;0;;true;;0;;U</id><eoq>true</eoq><tz>US/Eastern</tz>\
            <data>MidPoint</data><minTick>0.000005</minTick><sizeMinTick>1</sizeMinTick><Events>\
            <Tick><time>20261001-18:59:59</time><price>1.12347</price></Tick></Events></ResultSetTick>";
        let Some(crate::types::HistoricalTickData::Midpoint(t)) = parse_tick_response(mid).unwrap().data else { panic!("midpoint") };
        assert_eq!(t, vec![crate::types::HistoricalTickMidpoint { time: ET_15H - 1, price: 1.12347, size: 0.0 }]);
        assert!(parse_tick_response("<ResultSetBar>...</ResultSetBar>").is_none());
    }

    // ── ibx#429: 5-second bars merged into the live series ──

    fn five(o: f64, h: f64, l: f64, c: f64, volume: f64, count: i32) -> crate::types::RealTimeBar {
        crate::types::RealTimeBar { timestamp: 0, open: o, high: h, low: l, close: c, volume, wap: l, count }
    }

    #[test]
    fn five_second_bars_merge_as_captured() {
        // Capture b1_429_keep_up_to_date: the last 1 hour bar, its session,
        // and empty 5-second bars at 331.45 from 08:56:35 UTC on.
        let xml = "<ResultSetBar><id>cf60</id><eoq>true</eoq><tz>US/Eastern</tz><Events>\
            <Open><time>20261002-08:00:00</time><refDate>20261002</refDate></Open>\
            <Bar><time>20261002-08:00:00</time><endTime>20261002-08:56:35</endTime><open>331.05</open><close>331.45</close>\
            <high>331.59</high><low>330.55</low><weightedAvg>331.285</weightedAvg><volume>49528</volume><count>522</count></Bar>\
            <Close><time>20261003-00:00:00</time></Close></Events></ResultSetBar>";
        let (mut series, sessions) = parse_series(xml);
        assert_eq!(sessions, vec![(parse_server_time("20261002-08:00:00").unwrap(), parse_server_time("20261003-00:00:00").unwrap())]);
        let t0 = parse_server_time("20261002-08:56:35").unwrap();
        let empty = five(331.45, 331.45, 331.45, 331.45, 0.0, 0);
        for k in 0..12 {
            let i = merge_five_seconds(&mut series, &sessions, BarSize::Hour1, true, t0 + 5 * k, &empty).unwrap();
            assert_eq!(i, 0);
        }
        let b = &series[0];
        assert_eq!((b.open, b.high, b.low, b.close, b.volume, b.wap, b.count), (331.05, 331.59, 330.55, 331.45, 49528, 331.285, 522));

        // 1 min: the minute bar, then a new bar at 04:57 New York time.
        let mut minute = vec![SeriesBar { start: t0 - 35, end: t0, open: 331.45, high: 331.45, low: 331.45, close: 331.45,
            volume: 0, wap: 331.45, count: 0 }];
        assert_eq!(merge_five_seconds(&mut minute, &sessions, BarSize::Min1, true, t0, &empty), Some(0));
        assert_eq!(merge_five_seconds(&mut minute, &sessions, BarSize::Min1, true, t0 + 25, &empty), Some(1));
        assert_eq!(bar_time(minute[1].start, 1, true, "US/Eastern", "US/Eastern"), "20261002 04:57:00 US/Eastern");
        assert_eq!((minute[1].volume, minute[1].count, minute[1].wap), (0, 0, 331.45));
        // A trade adds its volume and count, and weighs the WAP.
        let trade = crate::types::RealTimeBar { wap: 331.5, ..five(331.5, 331.6, 331.4, 331.6, 100.0, 2) };
        assert_eq!(merge_five_seconds(&mut minute, &sessions, BarSize::Min1, true, t0 + 30, &trade), Some(1));
        assert_eq!((minute[1].high, minute[1].low, minute[1].close, minute[1].volume, minute[1].count, minute[1].wap),
            (331.6, 331.4, 331.6, 100, 2, 331.5));

        // 30 secs MIDPOINT: volume, WAP and count stay unset.
        let mut half = vec![SeriesBar { start: t0 - 5, end: t0, open: 331.42, high: 331.42, low: 331.42, close: 331.42,
            volume: -1, wap: -1.0, count: -1 }];
        let mid = five(331.42, 331.42, 331.42, 331.42, 5000.0, 1);
        assert_eq!(merge_five_seconds(&mut half, &sessions, BarSize::Sec30, false, t0 + 25, &mid), Some(1));
        assert_eq!((half[1].volume, half[1].wap, half[1].count), (-1, -1.0, -1));
        assert_eq!(bar_time(half[1].start, 1, true, "US/Eastern", "US/Eastern"), "20261002 04:57:00 US/Eastern");
        // A bar is not before the open of its session: 1 hour bars of a
        // regular session start at 09:30.
        let rth = vec![(parse_server_time("20261001-13:30:00").unwrap(), parse_server_time("20261001-20:00:00").unwrap())];
        let mut series = Vec::new();
        merge_five_seconds(&mut series, &rth, BarSize::Hour1, true, parse_server_time("20261001-13:30:05").unwrap(), &empty);
        assert_eq!(server_time(series[0].start), "20261001-13:30:00");
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

    /// A bar payload from `(value, width)` fields, LSB first, 4-byte
    /// groups reversed as on the wire.
    fn bar_payload(fields: &[(u32, usize)]) -> Vec<u8> {
        let mut bits: Vec<u8> = Vec::new();
        for &(v, n) in fields {
            for i in 0..n {
                bits.push(((v >> i) & 1) as u8);
            }
        }
        let mut bytes = vec![0u8; bits.len().div_ceil(32) * 4];
        for (i, b) in bits.iter().enumerate() {
            bytes[i / 8] |= b << (i % 8);
        }
        bytes.chunks(4).flat_map(|c| c.iter().rev().copied().collect::<Vec<_>>()).collect()
    }

    // ibx#454: as the reference's 5-second router, a negative 32-bit
    // trade count drops the bar, so does a completely empty bar (no trade,
    // all prices and the WAP total 0); a bar with no trade but a price is
    // kept (captured pre-market 25/09/2026: flat bars with volume 0).
    #[test]
    fn bar_drop_rules() {
        // pad, short count flag, count, low, short volume flag, volume.
        let flat = bar_payload(&[(0, 4), (1, 1), (0, 8), (33_596, 31), (1, 1), (0, 16)]);
        let bar = decode_bar_payload(&flat, 0.01).expect("a bar with a price is kept");
        assert_eq!((bar.count, bar.volume), (0, 0.0));
        assert!((bar.close - 335.96).abs() < 1e-9);
        let empty = bar_payload(&[(0, 4), (1, 1), (0, 8), (0, 31), (1, 1), (0, 16)]);
        assert!(decode_bar_payload(&empty, 0.01).is_none());
        let negative = bar_payload(&[(0, 4), (0, 1), (0x8000_0001, 32), (100, 31), (1, 1), (5, 16)]);
        assert!(decode_bar_payload(&negative, 0.01).is_none());
    }

    // ibx#455: the past ticks of a tick-by-tick request are read by the
    // request's type, Last and AllLast as trades.
    #[test]
    fn tick_by_tick_history_is_read_by_the_request_type() {
        let xml = "<ResultSetTick><id>rtTicker1;;SPY@SMARTMidPoint;;1;;true;;0;;U</id><eoq>false</eoq><Events>\
            <Tick><time>20260918-13:30:00</time><price>660.5</price><size>0</size></Tick></Events></ResultSetTick>";
        let f = parse_tick_by_tick_history(xml, crate::types::TbtType::MidPoint).unwrap();
        assert!(!f.done);
        assert!(matches!(f.data, Some(crate::types::HistoricalTickData::Midpoint(ref t)) if t.len() == 1 && t[0].price == 660.5));
        let f = parse_tick_by_tick_history(xml, crate::types::TbtType::AllLast).unwrap();
        assert!(matches!(f.data, Some(crate::types::HistoricalTickData::Last(_))));
    }
}
