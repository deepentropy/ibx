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

    // The period in the reference form (ibx#433). The engine refuses an
    // unreadable period before building the query.
    let time_length = match parse_period(&req.period) {
        Some((n, unit)) => format!("{} {}", n, unit),
        None => req.period.clone(),
    };

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

/// Time units of a histogram period, in the reference order: name and the
/// unit text of the query.
const PERIOD_UNITS: [(&str, &str); 8] = [
    ("seconds", "S"),
    ("minutes", "min"),
    ("hours", "h"),
    ("days", "d"),
    ("weeks", "W"),
    ("months", "m"),
    ("quarters", "q"),
    ("years", "y"),
];

/// Read a histogram period as the reference does (ibx#433): spaces
/// removed, a leading number above 0, then a unit named by a prefix of
/// exactly one unit name (`1 week`, `3 days`, `2 mo`; `2 wk` and `1 m`
/// are refused). Seconds and minutes that make whole hours or minutes
/// are given in the larger unit. Gives the number and the unit text of
/// the query.
pub fn parse_period(period: &str) -> Option<(u64, &'static str)> {
    let s: String = period.chars().filter(|c| !c.is_whitespace()).collect();
    let digits = s.chars().take_while(|c| c.is_ascii_digit()).count();
    let n: u64 = s[..digits].parse().ok()?;
    if n == 0 {
        return None;
    }
    let rest = s[digits..].to_ascii_lowercase();
    if rest.is_empty() {
        return None;
    }
    let mut found = None;
    for (i, (name, _)) in PERIOD_UNITS.iter().enumerate() {
        if name.starts_with(rest.as_str()) {
            if found.is_some() {
                return None;
            }
            found = Some(i);
        }
    }
    let mut unit = found?;
    let mut n = n;
    if unit == 0 && n % 3600 == 0 {
        n /= 3600;
        unit = 2;
    } else if unit == 0 && n % 60 == 0 {
        n /= 60;
        unit = 1;
    }
    if unit == 1 && n % 60 == 0 {
        n /= 60;
        unit = 2;
    }
    Some((n, PERIOD_UNITS[unit].1))
}

/// One histogram reply frame: the server sends one per trading day
/// (ibx#433).
#[derive(Debug, Clone, PartialEq)]
pub struct HistogramFrame {
    /// Price increment of the reply; 0 when absent.
    pub min_tick: f64,
    /// Size unit of the reply; 1 when absent.
    pub size_min_tick: f64,
    /// Last frame of the reply.
    pub is_complete: bool,
    /// (price, size) of every row, sizes as sent.
    pub ticks: Vec<(f64, f64)>,
}

/// Parse one histogram reply frame.
pub fn parse_histogram_frame(xml: &str) -> Option<HistogramFrame> {
    if !xml.contains("<ResultSetHistogram>") {
        return None;
    }
    let num = |x: &str, tag: &str| extract_xml_tag(x, tag).and_then(|s| s.trim().parse::<f64>().ok());
    let mut ticks = Vec::new();
    let mut search_start = 0;
    while let Some(tick_start) = xml[search_start..].find("<Tick>") {
        let abs_start = search_start + tick_start;
        let tick_end = match xml[abs_start..].find("</Tick>") {
            Some(e) => abs_start + e + 7,
            None => break,
        };
        let tick_xml = &xml[abs_start..tick_end];
        ticks.push((num(tick_xml, "price").unwrap_or(0.0), num(tick_xml, "size").unwrap_or(0.0)));
        search_start = tick_end;
    }
    Some(HistogramFrame {
        min_tick: num(xml, "minTick").filter(|t| *t > 0.0).unwrap_or(0.0),
        size_min_tick: num(xml, "sizeMinTick").filter(|t| *t > 0.0).unwrap_or(1.0),
        is_complete: extract_xml_tag(xml, "eoq").map(|s| s.trim() == "true").unwrap_or(false),
        ticks,
    })
}

/// The histogram of one request, summed over every frame of its reply
/// (ibx#433): each price is put in the bucket of the price increment of
/// the reply, and the bucket adds size times the size unit. The reference
/// buckets by the contract's market rule; the increment of the reply is
/// used here.
#[derive(Debug, Clone, Default)]
pub struct HistogramSum {
    buckets: std::collections::BTreeMap<i64, f64>,
}

impl HistogramSum {
    /// Price grid of the bucket keys (1e-8).
    const KEY_SCALE: f64 = 1e8;

    pub fn add(&mut self, frame: &HistogramFrame) {
        for &(price, size) in &frame.ticks {
            let bucket = if frame.min_tick > 0.0 {
                (price / frame.min_tick).round() * frame.min_tick
            } else {
                price
            };
            let key = (bucket * Self::KEY_SCALE).round() as i64;
            *self.buckets.entry(key).or_insert(0.0) += size * frame.size_min_tick;
        }
    }

    /// One entry per bucket, by price.
    pub fn entries(&self) -> Vec<HistogramEntry> {
        self.buckets.iter()
            .map(|(&key, &size)| HistogramEntry {
                price: key as f64 / Self::KEY_SCALE,
                count: size.round() as i64,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ibx#433: the period as the reference reads and writes it.
    #[test]
    fn parse_period_reference_rules() {
        assert_eq!(parse_period("1 week"), Some((1, "W")));
        assert_eq!(parse_period("2 weeks"), Some((2, "W")));
        assert_eq!(parse_period("3 days"), Some((3, "d")));
        assert_eq!(parse_period("2 months"), Some((2, "m")));
        assert_eq!(parse_period("2 mo"), Some((2, "m")));
        assert_eq!(parse_period("1 year"), Some((1, "y")));
        assert_eq!(parse_period("1 quarter"), Some((1, "q")));
        assert_eq!(parse_period("30 seconds"), Some((30, "S")));
        assert_eq!(parse_period("120 seconds"), Some((2, "min")));
        assert_eq!(parse_period("7200 s"), Some((2, "h")));
        assert_eq!(parse_period("120 min"), Some((2, "h")));
        assert_eq!(parse_period("1weeks"), Some((1, "W")));
        assert_eq!(parse_period("1 Week"), Some((1, "W")));
        for bad in ["abc", "", "1", "week", "0 days", "2 wk", "1 m", "1 weekss"] {
            assert_eq!(parse_period(bad), None, "{:?}", bad);
        }
    }

    #[test]
    fn histogram_sum_adds_every_frame_into_buckets() {
        let day1 = r#"<ResultSetHistogram><id>histogramQuery0;;265598@BEST Histogram;;0;;true;;0;;U</id>
            <eoq>false</eoq><minTick>0.01</minTick><sizeMinTick>1</sizeMinTick><Events>
            <Tick><time>20260227-14:30:00</time><price>270.50</price><size>1500</size></Tick>
            <Tick><time>20260227-14:30:00</time><price>271.00</price><size>2300</size></Tick>
            </Events></ResultSetHistogram>"#;
        let day2 = r#"<ResultSetHistogram><id>histogramQuery0;;265598@BEST Histogram;;0;;true;;0;;U</id>
            <eoq>true</eoq><minTick>0.01</minTick><sizeMinTick>100</sizeMinTick><Events>
            <Tick><time>20260226-14:30:00</time><price>270.5000001</price><size>2</size></Tick>
            <Tick><time>20260226-14:30:00</time><price>269.75</price><size>8</size></Tick>
            </Events></ResultSetHistogram>"#;
        let f1 = parse_histogram_frame(day1).unwrap();
        let f2 = parse_histogram_frame(day2).unwrap();
        assert!(!f1.is_complete && f2.is_complete);
        assert_eq!(f1.ticks.len(), 2);
        let mut sum = HistogramSum::default();
        sum.add(&f1);
        sum.add(&f2);
        assert_eq!(sum.entries(), vec![
            HistogramEntry { price: 269.75, count: 800 },
            HistogramEntry { price: 270.5, count: 1700 },
            HistogramEntry { price: 271.0, count: 2300 },
        ]);
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
        assert!(xml.contains("<timeLength>1 W</timeLength>"), "{}", xml);
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
        assert!(xml.contains("<timeLength>3 d</timeLength>"), "{}", xml);
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
