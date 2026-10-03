//! Market inputs of the option model: the interest-rate curve and the
//! dividend schedule the server sends as reference-data XML, the model
//! clock, and the time to expiry.

use super::model::{self, TreeDividend};

/// Milliseconds in a day.
pub const DAY_MS: i64 = 86_400_000;
/// The model clock moves once a minute.
pub const CLOCK_TICK_MS: i64 = 60_000;
/// Time zone of the model dates.
pub const MODEL_TZ: &str = "America/New_York";

/// One `div` (or `rate`) element of a reference-data answer.
#[derive(Debug, Clone, PartialEq)]
pub struct XmlItem {
    /// `date`, cut to 8 characters (YYYYMMDD).
    pub date: String,
    pub amount: f64,
    pub currency: String,
    pub special: bool,
}

fn element_text<'a>(block: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = block.find(&open)? + open.len();
    let end = block[start..].find(&close)? + start;
    Some(block[start..end].trim())
}

/// Items of a reference-data answer, in document order.
pub fn parse_reference_xml(xml: &str) -> Vec<XmlItem> {
    let mut items = Vec::new();
    let mut rest = xml;
    loop {
        // Next item start: `<div>`, `<div ...>`, `<rate>` or `<rate ...>`.
        let next = ["<div", "<rate"]
            .iter()
            .filter_map(|tag| {
                let mut from = 0;
                while let Some(p) = rest[from..].find(tag) {
                    let at = from + p;
                    let after = rest[at + tag.len()..].chars().next();
                    if matches!(after, Some('>') | Some(' ') | Some('\t') | Some('\n') | Some('\r') | Some('/')) {
                        return Some((at, *tag));
                    }
                    from = at + tag.len();
                }
                None
            })
            .min_by_key(|(at, _)| *at);
        let Some((at, tag)) = next else { break };
        let name = &tag[1..];
        let head_end = match rest[at..].find('>') {
            Some(e) => at + e,
            None => break,
        };
        let head = &rest[at..head_end];
        let close = format!("</{name}>");
        let body_end = rest[head_end..].find(&close).map(|e| head_end + e).unwrap_or(rest.len());
        let body = &rest[head_end + 1..body_end.max(head_end + 1)];
        let special = head
            .split("special=")
            .nth(1)
            .map(|v| v.trim_start_matches(['"', '\'']).to_ascii_lowercase().starts_with("true"))
            .unwrap_or(false);
        // The first 8 characters: a cut at byte 8 panicked inside a
        // character that is not ASCII (ibx#488).
        let mut date = element_text(body, "date").unwrap_or("").to_string();
        if let Some((cut, _)) = date.char_indices().nth(8) {
            date.truncate(cut);
        }
        let amount = element_text(body, "amt").and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
        let currency = element_text(body, "curr").unwrap_or("").to_string();
        items.push(XmlItem { date, amount, currency, special });
        rest = &rest[body_end.min(rest.len())..];
        if rest.is_empty() {
            break;
        }
    }
    items
}

/// Calendar date of a `YYYYMMDD` text.
pub fn parse_yyyymmdd(text: &str) -> Option<jiff::civil::Date> {
    if text.len() != 8 || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let y: i16 = text[0..4].parse().ok()?;
    let m: i8 = text[4..6].parse().ok()?;
    let d: i8 = text[6..8].parse().ok()?;
    jiff::civil::Date::new(y, m, d).ok()
}

fn tz(name: &str) -> jiff::tz::TimeZone {
    jiff::tz::TimeZone::get(name).unwrap_or(jiff::tz::TimeZone::UTC)
}

/// Midnight of a date in a time zone, in milliseconds.
pub fn midnight_ms(date: jiff::civil::Date, zone: &str) -> i64 {
    date.at(0, 0, 0, 0)
        .to_zoned(tz(zone))
        .map(|z| z.timestamp().as_millisecond())
        .unwrap_or(0)
}

/// A date and time of day in a time zone, in milliseconds.
pub fn local_ms(date: jiff::civil::Date, hour: i8, minute: i8, zone: &str) -> i64 {
    date.at(hour, minute, 0, 0)
        .to_zoned(tz(zone))
        .map(|z| z.timestamp().as_millisecond())
        .unwrap_or(0)
}

/// Date of an instant in a time zone.
pub fn date_of_ms(ms: i64, zone: &str) -> jiff::civil::Date {
    jiff::Timestamp::from_millisecond(ms)
        .map(|t| t.to_zoned(tz(zone)).date())
        .unwrap_or(jiff::civil::date(1970, 1, 1))
}

/// Money-market rate in percent (actual/360) as a continuous rate.
pub fn continuous_rate(percent: f64) -> f64 {
    let d = percent * 365.0 / 36000.0;
    4.0 * (1.0 + d / 4.0).ln()
}

/// Interest-rate curve of a currency: forward rates from each point date.
#[derive(Debug, Clone, PartialEq)]
pub struct RateCurve {
    /// (years from the curve date, continuous rate), in date order.
    pub points: Vec<(f64, f64)>,
}

impl RateCurve {
    /// Curve of the reference-data items, dated at `base_ms` (midnight of
    /// the model date when the curve arrived).
    pub fn from_items(items: &[XmlItem], base_ms: i64) -> RateCurve {
        let mut dated: Vec<(jiff::civil::Date, f64)> = items
            .iter()
            .filter_map(|it| parse_yyyymmdd(&it.date).map(|d| (d, it.amount)))
            .collect();
        dated.sort_by_key(|(d, _)| *d);
        dated.dedup_by_key(|(d, _)| *d);
        let points = dated
            .into_iter()
            .map(|(d, pct)| {
                let years = model::years_between_ms(base_ms, midnight_ms(d, MODEL_TZ));
                (if years < 0.0 { 0.0 } else { years }, continuous_rate(pct))
            })
            .collect();
        RateCurve { points }
    }

    /// Average rate from the curve date to `t` years: each point rate holds
    /// until the next point, the first one also before it, the last one
    /// after it. No point gives `f64::MAX`.
    pub fn average(&self, t: f64) -> f64 {
        if self.points.is_empty() {
            return f64::MAX;
        }
        let t_days = t * 365.0;
        let mut avg = 0.0;
        let mut prev_days = 0.0;
        let mut prev_rate = 0.0;
        let mut seg;
        let mut denom = 0.0;
        let mut done = false;
        for (i, (years, rate)) in self.points.iter().enumerate() {
            let days = (years * 365.0 + 0.5).floor() as i32 as f64;
            if days > t_days {
                seg = t_days - prev_days;
                denom = t_days;
                done = true;
            } else {
                seg = days - prev_days;
                denom = days;
            }
            if i == 0 {
                prev_rate = *rate;
            }
            avg = if denom > 0.0 { (prev_rate * seg + avg * prev_days) / denom } else { *rate };
            if done {
                break;
            }
            prev_rate = *rate;
            prev_days = days;
        }
        if !done && denom != 0.0 {
            let seg = t_days - prev_days;
            denom = t_days;
            avg = (prev_rate * seg + avg * prev_days) / denom;
        }
        avg
    }

    /// Rate of the model for a time to expiry: the average to the whole
    /// days of that time.
    pub fn model_rate(&self, t: f64) -> f64 {
        self.average(model::ONE_DAY_T * ((t * 365.0) as i32) as f64)
    }
}

/// One dividend of the underlying.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Dividend {
    pub ex_date: jiff::civil::Date,
    pub amount: f64,
}

/// Dividends of a reference-data answer: special dividends are left out.
pub fn dividends_from_items(items: &[XmlItem]) -> Vec<Dividend> {
    let mut out: Vec<Dividend> = items
        .iter()
        .filter(|it| !it.special)
        .filter_map(|it| parse_yyyymmdd(&it.date).map(|d| Dividend { ex_date: d, amount: it.amount }))
        .collect();
    out.sort_by_key(|d| d.ex_date);
    out
}

/// Present value of the dividends with an ex-date after `today` and not
/// after the last trading day, discounted at the flat rate `r` over whole
/// days.
pub fn pv_dividends(divs: &[Dividend], today: jiff::civil::Date, last_day: jiff::civil::Date, r: f64) -> f64 {
    let today_ms = midnight_ms(today, MODEL_TZ);
    let mut pv = 0.0;
    for d in divs {
        if d.ex_date > today && d.ex_date <= last_day {
            let years = model::years_integer_days(model::years_between_ms(today_ms, midnight_ms(d.ex_date, MODEL_TZ)));
            pv += d.amount * (-r * years).exp();
        }
    }
    pv
}

/// Dividends as the tree uses them, measured from the model clock. `local`
/// is the time zone whose end of day closes an ex-date.
pub fn tree_dividends(divs: &[Dividend], clock_ms: i64, local: &str) -> Vec<TreeDividend> {
    divs.iter()
        .map(|d| {
            let years = model::years_between_ms(clock_ms, midnight_ms(d.ex_date, MODEL_TZ));
            let next_day = d.ex_date.tomorrow().unwrap_or(d.ex_date);
            let end_of_day = midnight_ms(next_day, local);
            TreeDividend {
                years,
                years_int: model::years_integer_days(years),
                days: (end_of_day - clock_ms) / DAY_MS,
                amount: d.amount,
            }
        })
        .collect()
}

/// System time zone name used for the end of an ex-date.
pub fn local_zone_name() -> String {
    jiff::tz::TimeZone::system().iana_name().unwrap_or("UTC").to_string()
}

/// The model clock: set when the model starts, then moved once a minute.
#[derive(Debug, Clone, Copy)]
pub struct ModelClock {
    pub start_ms: i64,
}

impl ModelClock {
    pub fn at(&self, now_ms: i64) -> i64 {
        if now_ms <= self.start_ms {
            return self.start_ms;
        }
        self.start_ms + (now_ms - self.start_ms) / CLOCK_TICK_MS * CLOCK_TICK_MS
    }
}

/// End of the option life used for the time to expiry.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ExpiryEnd {
    /// Exact time in milliseconds (last trading time known).
    At(i64),
    /// Date only, midnight in milliseconds; the model adds 16 hours.
    DateOnly(i64),
}

/// Time to expiry of the model at the clock time.
pub fn time_to_expiry(clock_ms: i64, end: ExpiryEnd) -> f64 {
    match end {
        ExpiryEnd::At(ms) => model::time_to_expiry_close(model::years_between_ms(clock_ms, ms), false),
        ExpiryEnd::DateOnly(ms) => model::time_to_expiry_close(model::years_between_ms(clock_ms, ms), true),
    }
}

/// End of life of an option: the last trading date with its last trading
/// time (`HHMM`) when known, else the end of the trading session of that
/// date in the schedule, else the date only.
pub fn expiry_end(last_trade_date: &str, last_trade_time: &str, trading_hours: Option<&str>, zone: &str) -> Option<ExpiryEnd> {
    let date = parse_yyyymmdd(last_trade_date.get(0..8)?)?;
    if last_trade_time.len() == 4 && last_trade_time.bytes().all(|b| b.is_ascii_digit()) {
        let h: i8 = last_trade_time[0..2].parse().ok()?;
        let m: i8 = last_trade_time[2..4].parse().ok()?;
        return Some(ExpiryEnd::At(local_ms(date, h, m, zone)));
    }
    if let Some(end) = trading_hours.and_then(|hours| session_end(hours, date, zone)) {
        return Some(ExpiryEnd::At(end));
    }
    Some(ExpiryEnd::DateOnly(midnight_ms(date, zone)))
}

/// End of the first open session that starts on `date` in a trading-hours
/// text (`20261001:0930-20261001:1600;20261002:CLOSED;...`).
fn session_end(hours: &str, date: jiff::civil::Date, zone: &str) -> Option<i64> {
    for part in hours.split(';') {
        let part = part.trim();
        if part.is_empty() || part.ends_with("CLOSED") {
            continue;
        }
        let (start, end) = part.split_once('-')?;
        let (start_day, _) = start.split_once(':')?;
        if parse_yyyymmdd(start_day) != Some(date) {
            continue;
        }
        let (end_day, end_time) = end.split_once(':')?;
        let end_date = parse_yyyymmdd(end_day)?;
        if end_time.len() != 4 {
            return None;
        }
        let h: i8 = end_time[0..2].parse().ok()?;
        let m: i8 = end_time[2..4].parse().ok()?;
        return Some(local_ms(end_date, h, m, zone));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE_XML: &str = "<?xml version=\"1.0\" encoding=\"US-ASCII\"?>\n<financialReferenceData>\n\t<contract id=\"iUSD\">\n\t\t<dividends style=\"forward365\">\n\t\t\t<minDate>20260925</minDate>\n\t\t\t<maxDate>20560929</maxDate>\n\t\t\t<div>\n\t\t\t\t<date>20260925</date>\n\t\t\t\t<exDate>20260925</exDate>\n\t\t\t\t<curr>USD</curr>\n\t\t\t\t<amt>4.3084</amt>\n\t\t\t</div>\n\t\t\t<div>\n\t\t\t\t<date>20261001</date>\n\t\t\t\t<exDate>20261001</exDate>\n\t\t\t\t<curr>USD</curr>\n\t\t\t\t<amt>4.31329</amt>\n\t\t\t</div>\n\t\t\t<div>\n\t\t\t\t<date>20261101</date>\n\t\t\t\t<exDate>20261101</exDate>\n\t\t\t\t<curr>USD</curr>\n\t\t\t\t<amt>4.46283</amt>\n\t\t\t</div>\n\t\t</dividends>\n\t</contract>\n</financialReferenceData>\n";

    #[test]
    fn parses_the_captured_rate_answer() {
        let items = parse_reference_xml(RATE_XML);
        assert_eq!(items.len(), 3);
        assert_eq!(items[0], XmlItem { date: "20260925".into(), amount: 4.3084, currency: "USD".into(), special: false });
        assert_eq!(items[2].amount, 4.46283);
    }

    #[test]
    fn special_dividends_are_left_out() {
        let xml = "<dividends><div special=\"true\"><date>20261218</date><amt>5</amt></div><div><date>20261219</date><amt>1.8311</amt></div></dividends>";
        let divs = dividends_from_items(&parse_reference_xml(xml));
        assert_eq!(divs, vec![Dividend { ex_date: jiff::civil::date(2026, 12, 19), amount: 1.8311 }]);
    }

    #[test]
    fn rate_conversion_is_quarterly_money_market() {
        let r = continuous_rate(4.0);
        assert!((r - 4.0 * (1.0 + 4.0 * 365.0 / 36000.0 / 4.0f64).ln()).abs() < 1e-15);
    }

    #[test]
    fn curve_average_steps_from_each_point() {
        let curve = RateCurve { points: vec![(2.0 / 365.0, 0.04), (10.0 / 365.0, 0.05)] };
        // Before the first point: its rate.
        assert!((curve.average(1.0 / 365.0) - 0.04).abs() < 1e-15);
        // Between: the first rate holds to the second point.
        assert!((curve.average(6.0 / 365.0) - 0.04).abs() < 1e-15);
        // After the last point: the last rate from it.
        let a = curve.average(20.0 / 365.0);
        assert!((a - (0.04 * 10.0 + 0.05 * 10.0) / 20.0).abs() < 1e-15, "{a}");
        assert_eq!(RateCurve { points: vec![] }.average(0.1), f64::MAX);
    }

    #[test]
    fn model_rate_uses_whole_days() {
        let curve = RateCurve { points: vec![(1.0 / 365.0, 0.03), (3.0 / 365.0, 0.06)] };
        // 2.49 days -> 2 days.
        assert_eq!(curve.model_rate(2.49 / 365.0), curve.average(2.0 * model::ONE_DAY_T));
    }

    #[test]
    fn pv_counts_ex_dates_after_today_up_to_the_last_day() {
        let divs = vec![
            Dividend { ex_date: jiff::civil::date(2026, 3, 4), amount: 1.0 },
            Dividend { ex_date: jiff::civil::date(2026, 3, 5), amount: 1.0 },
            Dividend { ex_date: jiff::civil::date(2026, 3, 6), amount: 1.0 },
            Dividend { ex_date: jiff::civil::date(2026, 3, 9), amount: 1.0 },
        ];
        let pv = pv_dividends(&divs, jiff::civil::date(2026, 3, 4), jiff::civil::date(2026, 3, 6), 0.0);
        assert_eq!(pv, 2.0);
    }

    #[test]
    fn clock_moves_once_a_minute() {
        let c = ModelClock { start_ms: 1_000 };
        assert_eq!(c.at(500), 1_000);
        assert_eq!(c.at(60_999), 1_000);
        assert_eq!(c.at(61_000), 61_000);
        assert_eq!(c.at(150_000), 121_000);
    }

    #[test]
    fn expiry_with_last_trading_time() {
        let e = expiry_end("20260306", "1615", None, MODEL_TZ).unwrap();
        assert_eq!(e, ExpiryEnd::At(local_ms(jiff::civil::date(2026, 3, 6), 16, 15, MODEL_TZ)));
        let d = expiry_end("20260306", "", None, MODEL_TZ).unwrap();
        assert_eq!(d, ExpiryEnd::DateOnly(midnight_ms(jiff::civil::date(2026, 3, 6), MODEL_TZ)));
        let s = expiry_end("20260306", "", Some("20260305:0930-20260305:1600;20260306:0930-20260306:1600"), MODEL_TZ).unwrap();
        assert_eq!(s, ExpiryEnd::At(local_ms(jiff::civil::date(2026, 3, 6), 16, 0, MODEL_TZ)));
    }

    #[test]
    fn capture_40_time_to_expiry() {
        // 04/03/2026 10:17:36.422 CET = 09:17:36.422 UTC.
        let clock = jiff::civil::date(2026, 3, 4).at(9, 17, 36, 422_000_000)
            .to_zoned(jiff::tz::TimeZone::UTC).unwrap().timestamp().as_millisecond();
        let t = time_to_expiry(clock, expiry_end("20260306", "", None, MODEL_TZ).unwrap());
        let expected = 0.006814384830035515 + 45.138 / 3.1536e7;
        assert!((t - expected).abs() < 1e-10, "{t}");
    }
}
