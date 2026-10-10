//! Scanner subscriptions via the data connection.

use crate::protocol::fix;

// Tags for scanner messages
pub const TAG_SCANNER_XML: u32 = 6118;
pub const TAG_SUB_PROTOCOL: u32 = 6040;

/// Parameters for a scanner subscription request.
#[derive(Debug, Clone, Default)]
pub struct ScannerSubscription {
    pub instrument: String,
    pub location_code: String,
    pub scan_code: String,
    /// Rows asked by the client; negative when not set (ibx#456).
    pub number_of_rows: i32,
    /// Filter codes and values, in the order they are written (ibx#456).
    pub filters: Vec<(String, String)>,
}

/// Row limit of a scan type when the scanner parameters give none, and
/// the cap of a request above the limit (ibx#456).
const DEFAULT_SCAN_SIZE_LIMIT: i32 = 50;

/// The rows written in a subscription, as the reference decides them
/// (ibx#456): the asked rows when they fit the scan type's limit (50 when
/// the parameters give none); 256 is kept; otherwise, and when not set,
/// the limit capped at 50.
pub fn scanner_max_items(number_of_rows: i32, size_limit: Option<u32>) -> i32 {
    let limit = size_limit.map_or(DEFAULT_SCAN_SIZE_LIMIT, |l| l.min(i32::MAX as u32) as i32);
    let capped = limit.min(DEFAULT_SCAN_SIZE_LIMIT);
    if number_of_rows < 0 {
        capped
    } else if number_of_rows <= limit {
        number_of_rows
    } else if number_of_rows == 256 {
        256
    } else {
        capped
    }
}

/// Row limit of each scan type in the scanner parameters (ibx#456).
pub fn scan_size_limits(params_xml: &str) -> std::collections::HashMap<String, u32> {
    let mut out = std::collections::HashMap::new();
    let mut rest = params_xml;
    while let Some(start) = rest.find("<ScanType>") {
        let block_end = rest[start..].find("</ScanType>").map_or(rest.len(), |e| start + e);
        let block = &rest[start..block_end];
        if let (Some(code), Some(limit)) = (extract_xml_tag(block, "scanCode"), extract_xml_tag(block, "respSizeLimit")) {
            if let Ok(limit) = limit.trim().parse::<u32>() {
                out.insert(code.trim().to_string(), limit);
            }
        }
        rest = &rest[block_end..];
    }
    out
}

/// A number as the reference's runtime writes a double: at least one
/// digit after the point, the exponent form below 0.001 and from 10^7
/// (`10.0`, `1.0E7`) (ibx#456).
pub fn java_double_text(d: f64) -> String {
    if d.is_nan() {
        return "NaN".into();
    }
    if d.is_infinite() {
        return if d > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    if d == 0.0 {
        return if d.is_sign_negative() { "-0.0".into() } else { "0.0".into() };
    }
    let a = d.abs();
    if (1e-3..1e7).contains(&a) {
        let s = format!("{}", d);
        if s.contains('.') { s } else { s + ".0" }
    } else {
        let s = format!("{:e}", d);
        let (mantissa, exp) = s.split_once('e').unwrap_or((&s, "0"));
        if mantissa.contains('.') {
            format!("{}E{}", mantissa, exp)
        } else {
            format!("{}.0E{}", mantissa, exp)
        }
    }
}

/// Escape a filter value for an XML element.
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// One entry from a scanner result.
#[derive(Debug, Clone, Default)]
pub struct ScannerEntry {
    pub con_id: i64,
    pub symbol: String,
    pub sec_type: String,
    pub exchange: String,
    pub currency: String,
    /// Row values passed to the client as they come (ibx#457).
    pub distance: String,
    pub benchmark: String,
    pub projection: String,
    /// Combo legs of the row, in the client form (`|` between the parts
    /// of a leg) (ibx#457).
    pub legs: String,
}

/// Parsed scanner subscription response.
#[derive(Debug, Clone, Default)]
pub struct ScannerResult {
    /// Subscription id echoed by the server (ibx#457).
    pub id: String,
    pub con_ids: Vec<i64>,
    pub entries: Vec<ScannerEntry>,
    pub scan_time: String,
    /// Server refusal text: the subscription ends (ibx#457).
    pub error_text: String,
    /// Server warning text: the rows are still used (ibx#457).
    pub warning_text: String,
}

/// The subscription id of a client scanner: client id, then request id
/// (ibx#457).
pub fn scanner_subscription_id(client_id: i64, req_id: crate::types::ReqId) -> String {
    format!("APISCAN{}:{}", client_id, req_id)
}

/// The live scanner subscriptions in the order the reference walks them
/// when the scanner parameters arrive (ibx#513): a hash table keyed by the
/// subscription id, 11 slots to start, grown to twice plus one when three
/// quarters full, walked from the last slot to the first, the newest id of
/// a slot first. Six requests 9001 to 9006 of client 198 went out as 9002,
/// 9001, 9006, 9005, 9004, 9003 (reference run of 10/10/2026).
///
/// A client's parameters request that waits for its answer has a place in
/// the table too (`PARAMS_REQUEST`, ibx#552): it counts for the growth, so
/// with it eight subscriptions already go out in the order of a grown
/// table (9008 down to 9001, reference run of 10/10/2026).
#[derive(Debug, Clone)]
pub struct ScannerTable {
    /// The ids of each slot, newest first.
    slots: Vec<Vec<String>>,
    count: usize,
}

impl Default for ScannerTable {
    fn default() -> Self {
        Self { slots: vec![Vec::new(); 11], count: 0 }
    }
}

impl ScannerTable {
    /// The place of a waiting parameters request. Its key only has to
    /// differ from every subscription id: the order of the others does
    /// not depend on the slot it falls in.
    pub const PARAMS_REQUEST: &'static str = "APISCANPARAMS";

    fn slot(id: &str, slots: usize) -> usize {
        let h = id.encode_utf16().fold(0u32, |h, c| h.wrapping_mul(31).wrapping_add(c as u32));
        (h & 0x7FFF_FFFF) as usize % slots
    }

    pub fn insert(&mut self, id: &str) {
        if self.slots[Self::slot(id, self.slots.len())].iter().any(|e| e == id) {
            return;
        }
        if self.count >= self.slots.len() * 3 / 4 {
            let size = self.slots.len() * 2 + 1;
            let mut grown: Vec<Vec<String>> = vec![Vec::new(); size];
            for chain in std::mem::take(&mut self.slots).into_iter().rev() {
                for e in chain {
                    grown[Self::slot(&e, size)].insert(0, e);
                }
            }
            self.slots = grown;
        }
        let at = Self::slot(id, self.slots.len());
        self.slots[at].insert(0, id.to_string());
        self.count += 1;
    }

    pub fn remove(&mut self, id: &str) {
        let at = Self::slot(id, self.slots.len());
        if let Some(pos) = self.slots[at].iter().position(|e| e == id) {
            self.slots[at].remove(pos);
            self.count -= 1;
        }
    }

    /// The ids in the reference's order.
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.slots.iter().rev().flatten().map(String::as_str)
    }
}

/// Build a scanner parameters request (no XML payload).
pub fn build_scanner_params_request(seq: u32) -> Vec<u8> {
    fix::fix_build(
        &[
            (fix::TAG_MSG_TYPE, "U"),
            (TAG_SUB_PROTOCOL, "10001"),
        ],
        seq,
    )
}

/// Build the XML payload for a scanner subscription request; `max_items`
/// comes from `scanner_max_items`.
pub fn build_scanner_subscribe_xml(sub: &ScannerSubscription, scan_id: &str, max_items: i32) -> String {
    // The filters, only when there is one (ibx#456).
    let mut filter = String::new();
    if !sub.filters.is_empty() {
        filter.push_str("<Filter varName=\"filter\">");
        for (code, value) in &sub.filters {
            filter.push_str(&format!("<{code}>{}</{code}>", xml_escape(value)));
        }
        filter.push_str("</Filter>");
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <ScanSubscription>\
         <id>{id}</id>\
         <instrument>{instrument}</instrument>\
         <locations>{locations}</locations>\
         <scanCode>{scan_code}</scanCode>\
         <source>API</source>\
         <maxItems>{max_items}</maxItems>\
         {filter}\
         <suspend>no</suspend>\
         <inclRestrictedLocations>yes</inclRestrictedLocations>\
         <apiManual>no</apiManual>\
         <aggGroup>-1</aggGroup>\
         </ScanSubscription>",
        id = scan_id,
        instrument = sub.instrument,
        locations = sub.location_code,
        scan_code = sub.scan_code,
        max_items = max_items,
        filter = filter,
    )
}

/// Build the XML payload for cancelling a scanner subscription.
pub fn build_scanner_cancel_xml(scan_id: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <ScanDesubscription>\
         <id>{id}</id>\
         </ScanDesubscription>",
        id = scan_id,
    )
}

/// Extract a simple XML tag value: `<tag>value</tag>` -> `value`.
fn extract_xml_tag<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(&xml[start..end])
}

/// Replace the five predefined XML entities.
fn xml_unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"")
        .replace("&apos;", "'").replace("&amp;", "&")
}

/// Text of an element, unescaped; empty when absent.
fn xml_text(xml: &str, tag: &str) -> String {
    extract_xml_tag(xml, tag).map(|v| xml_unescape(v.trim())).unwrap_or_default()
}

/// Parse a ScanResponse XML into its id, texts and rows.
pub fn parse_scanner_response(xml: &str) -> Option<ScannerResult> {
    if !xml.contains("<ScanResponse>") {
        return None;
    }

    // Response-level elements come before the row list.
    let head = &xml[..xml.find("<Contracts>").unwrap_or(xml.len())];
    let id = xml_text(head, "id");
    let error_text = xml_text(head, "errorText");
    let warning_text = xml_text(head, "warningText");
    let scan_time = extract_xml_tag(head, "scanTime").unwrap_or("").to_string();

    let mut con_ids = Vec::new();
    let mut entries = Vec::new();
    let mut search_start = 0;

    while let Some(c_start) = xml[search_start..].find("<Contract>") {
        let abs_start = search_start + c_start;
        let c_end = match xml[abs_start..].find("</Contract>") {
            Some(e) => abs_start + e + 11,
            None => break,
        };
        let contract_xml = &xml[abs_start..c_end];

        let con_id = extract_xml_tag(contract_xml, "contractID")
            .and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
        if con_id != 0 {
            con_ids.push(con_id);
        }
        entries.push(ScannerEntry {
            con_id,
            distance: xml_text(contract_xml, "distance"),
            benchmark: xml_text(contract_xml, "benchmark"),
            projection: xml_text(contract_xml, "projection"),
            legs: xml_text(contract_xml, "legs").replace('/', "|"),
            ..Default::default()
        });
        search_start = c_end;
    }

    Some(ScannerResult { id, con_ids, entries, scan_time, error_text, warning_text })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scanner_params_request_structure() {
        let msg = build_scanner_params_request(1);
        let tags = fix::fix_parse(&msg);
        assert_eq!(tags[&fix::TAG_MSG_TYPE], "U");
        assert_eq!(tags[&TAG_SUB_PROTOCOL], "10001");
    }

    #[test]
    fn scanner_subscribe_xml_structure() {
        let sub = ScannerSubscription {
            instrument: "STK".to_string(),
            location_code: "STK.US.MAJOR".to_string(),
            scan_code: "TOP_PERC_GAIN".to_string(),
            number_of_rows: -1,
            filters: Vec::new(),
        };
        let xml = build_scanner_subscribe_xml(&sub, "APISCAN1:1", 50);
        assert!(xml.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?><ScanSubscription>"), "{xml}");
        assert!(!xml.contains("<Filter"), "no filter block without a filter");
        assert!(xml.contains("<id>APISCAN1:1</id>"));
        assert!(xml.contains("<instrument>STK</instrument>"));
        assert!(xml.contains("<locations>STK.US.MAJOR</locations>"));
        assert!(xml.contains("<scanCode>TOP_PERC_GAIN</scanCode>"));
        assert!(xml.contains("<maxItems>50</maxItems>"));
        assert!(xml.contains("<source>API</source>"));
        assert!(xml.contains("<aggGroup>-1</aggGroup>"));
    }

    // ibx#456: one filter block between the row count and the suspend flag.
    #[test]
    fn scanner_subscribe_xml_filter_block() {
        let sub = ScannerSubscription {
            instrument: "STK".into(),
            location_code: "STK.US.MAJOR".into(),
            scan_code: "TOP_PERC_GAIN".into(),
            number_of_rows: 10,
            filters: vec![("priceAbove".into(), "10.0".into()), ("stkTypes".into(), "inc:ETF".into()),
                          ("x".into(), "a<b".into())],
        };
        let xml = build_scanner_subscribe_xml(&sub, "APISCAN0:1", 10);
        assert!(xml.contains("<maxItems>10</maxItems><Filter varName=\"filter\"><priceAbove>10.0</priceAbove>\
                              <stkTypes>inc:ETF</stkTypes><x>a&lt;b</x></Filter><suspend>no</suspend>"), "{xml}");
    }

    // ibx#456: the reference's row rule.
    #[test]
    fn scanner_max_items_rule() {
        assert_eq!(scanner_max_items(-1, None), 50);
        assert_eq!(scanner_max_items(25, None), 25);
        assert_eq!(scanner_max_items(100, None), 50);
        assert_eq!(scanner_max_items(256, None), 256);
        assert_eq!(scanner_max_items(100, Some(750)), 100);
        assert_eq!(scanner_max_items(-1, Some(750)), 50);
        assert_eq!(scanner_max_items(800, Some(750)), 50);
        assert_eq!(scanner_max_items(30, Some(20)), 20);
        assert_eq!(scanner_max_items(0, None), 0);
    }

    #[test]
    fn scan_size_limits_from_parameters() {
        let xml = "<ScanTypeList><ScanType><scanCode>A</scanCode><snapshotSizeLimit>500</snapshotSizeLimit>\
                   </ScanType><ScanType><scanCode>B</scanCode><respSizeLimit>750</respSizeLimit></ScanType>\
                   </ScanTypeList>";
        let limits = scan_size_limits(xml);
        assert_eq!(limits.get("B"), Some(&750));
        assert_eq!(limits.get("A"), None);
    }

    #[test]
    fn java_double_text_forms() {
        assert_eq!(java_double_text(10.0), "10.0");
        assert_eq!(java_double_text(1e7), "1.0E7");
        assert_eq!(java_double_text(2.5), "2.5");
        assert_eq!(java_double_text(1234567.5), "1234567.5");
        assert_eq!(java_double_text(12345678.0), "1.2345678E7");
        assert_eq!(java_double_text(0.0005), "5.0E-4");
        assert_eq!(java_double_text(0.001), "0.001");
        assert_eq!(java_double_text(0.0), "0.0");
    }

    // ibx#513: the order of the reference run of 10/10/2026 (six requests
    // before the scanner parameters) and of the recording of 26/09/2026.
    #[test]
    fn scanner_table_order_as_the_reference() {
        let order = |client: i64, reqs: &[i64]| {
            let mut t = ScannerTable::default();
            for r in reqs {
                t.insert(&scanner_subscription_id(client, *r));
            }
            t.ids().map(|id| id.rsplit(':').next().unwrap().parse::<i64>().unwrap()).collect::<Vec<_>>()
        };
        assert_eq!(order(198, &[9001, 9002, 9003, 9004, 9005, 9006]), [9002, 9001, 9006, 9005, 9004, 9003]);
        assert_eq!(order(198, &[9005, 9006]), [9006, 9005]);
    }

    // ibx#552: a parameters request made first has its place in the table.
    // The orders of the reference runs of 10/10/2026 with seven, eight and
    // nine subscriptions behind it: the table grows one subscription sooner.
    #[test]
    fn scanner_table_counts_a_waiting_parameters_request() {
        let order = |subs: i64, with_request: bool| {
            let mut t = ScannerTable::default();
            if with_request {
                t.insert(ScannerTable::PARAMS_REQUEST);
            }
            for r in 9001..9001 + subs {
                t.insert(&scanner_subscription_id(198, r));
            }
            t.ids().filter(|id| *id != ScannerTable::PARAMS_REQUEST)
                .map(|id| id.rsplit(':').next().unwrap().parse::<i64>().unwrap()).collect::<Vec<_>>()
        };
        assert_eq!(order(7, true), [9002, 9001, 9007, 9006, 9005, 9004, 9003]);
        assert_eq!(order(8, true), [9008, 9007, 9006, 9005, 9004, 9003, 9002, 9001]);
        assert_eq!(order(9, true), [9009, 9008, 9007, 9006, 9005, 9004, 9003, 9002, 9001]);
        // Without it, eight subscriptions are not yet in a grown table.
        assert_eq!(order(8, false), [9002, 9001, 9008, 9007, 9006, 9005, 9004, 9003]);
    }

    // ibx#513: ids of one slot come newest first; a removed id leaves; the
    // table grows at the ninth id and keeps every id once.
    #[test]
    fn scanner_table_slots_removal_and_growth() {
        let mut t = ScannerTable::default();
        // "a" and "l" are 11 apart: the same slot of 11.
        for id in ["a", "l", "b"] {
            t.insert(id);
        }
        t.insert("a");
        assert_eq!(t.ids().collect::<Vec<_>>(), ["b", "l", "a"]);
        t.remove("l");
        t.remove("zz");
        assert_eq!(t.ids().collect::<Vec<_>>(), ["b", "a"]);

        let mut t = ScannerTable::default();
        let ids: Vec<String> = (1..=10).map(|r| scanner_subscription_id(7, r)).collect();
        for id in &ids {
            t.insert(id);
        }
        assert_eq!(t.slots.len(), 23);
        let mut listed: Vec<&str> = t.ids().collect();
        assert_eq!(listed.len(), 10);
        listed.sort_unstable();
        let mut want: Vec<&str> = ids.iter().map(String::as_str).collect();
        want.sort_unstable();
        assert_eq!(listed, want);
    }

    #[test]
    fn scanner_cancel_xml_structure() {
        let xml = build_scanner_cancel_xml("APISCAN31:3");
        assert!(xml.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?><ScanDesubscription>"), "{xml}");
        assert!(xml.contains("<ScanDesubscription>"));
        assert!(xml.contains("<id>APISCAN31:3</id>"));
    }

    #[test]
    fn parse_scanner_response_basic() {
        let xml = r#"<ScanResponse>
            <id>APISCAN31:3</id>
            <scanTime>20260311-11:08:43</scanTime>
            <Contracts>
                <Contract>
                    <contractID>592977497</contractID>
                    <inScanTime>20260311-11:08:43</inScanTime>
                </Contract>
                <Contract>
                    <contractID>265598</contractID>
                    <inScanTime>20260311-11:08:43</inScanTime>
                </Contract>
            </Contracts>
        </ScanResponse>"#;

        let result = parse_scanner_response(xml).unwrap();
        assert_eq!(result.scan_time, "20260311-11:08:43");
        assert_eq!(result.con_ids.len(), 2);
        assert_eq!(result.con_ids[0], 592977497);
        assert_eq!(result.con_ids[1], 265598);
    }

    // ibx#457: the id, the server texts and the row values are read.
    #[test]
    fn parse_scanner_response_texts_and_row_values() {
        let xml = "<ScanResponse><id>APISCAN198:9005</id><errorText>bad &amp; worse</errorText>\
                   <warningText>late</warningText><scanTime>20260926-20:16:34</scanTime><Contracts>\
                   <Contract><contractID>4815747</contractID><distance>1.5</distance>\
                   <benchmark>SPX</benchmark><projection>2</projection>\
                   <legs>4815747/1/BUY/SMART,270639/1/SELL/SMART</legs></Contract>\
                   <Contract><contractID>270639</contractID><frozen>yes</frozen></Contract>\
                   </Contracts></ScanResponse>";
        let r = parse_scanner_response(xml).unwrap();
        assert_eq!(r.id, "APISCAN198:9005");
        assert_eq!(r.error_text, "bad & worse");
        assert_eq!(r.warning_text, "late");
        assert_eq!(r.entries.len(), 2);
        let e = &r.entries[0];
        assert_eq!((e.distance.as_str(), e.benchmark.as_str(), e.projection.as_str()), ("1.5", "SPX", "2"));
        assert_eq!(e.legs, "4815747|1|BUY|SMART,270639|1|SELL|SMART");
        assert_eq!(r.entries[1].distance, "");
        assert_eq!(scanner_subscription_id(198, 9005), "APISCAN198:9005");
    }

    #[test]
    fn parse_scanner_response_rejects_other() {
        assert!(parse_scanner_response("<ResultSetBar>...</ResultSetBar>").is_none());
        assert!(parse_scanner_response("not xml at all").is_none());
    }
}
