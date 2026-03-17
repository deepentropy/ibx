//! Scanner subscriptions via the data connection.

use crate::protocol::fix;

// Tags for scanner messages
pub const TAG_SCANNER_XML: u32 = 6118;
pub const TAG_SUB_PROTOCOL: u32 = 6040;

/// Parameters for a scanner subscription request.
#[derive(Debug, Clone)]
pub struct ScannerSubscription {
    pub instrument: String,
    pub location_code: String,
    pub scan_code: String,
    pub max_items: u32,
}

/// Parsed scanner subscription response.
#[derive(Debug, Clone)]
pub struct ScannerResult {
    pub con_ids: Vec<u32>,
    pub scan_time: String,
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

/// Build the XML payload for a scanner subscription request.
pub fn build_scanner_subscribe_xml(sub: &ScannerSubscription) -> String {
    format!(
        "<ScanSubscription>\
         <instrument>{instrument}</instrument>\
         <locations>{locations}</locations>\
         <scanCode>{scan_code}</scanCode>\
         <source>API</source>\
         <maxItems>{max_items}</maxItems>\
         <suspend>no</suspend>\
         <inclRestrictedLocations>yes</inclRestrictedLocations>\
         <apiManual>no</apiManual>\
         <aggGroup>-1</aggGroup>\
         </ScanSubscription>",
        instrument = sub.instrument,
        locations = sub.location_code,
        scan_code = sub.scan_code,
        max_items = sub.max_items,
    )
}

/// Build the XML payload for cancelling a scanner subscription.
pub fn build_scanner_cancel_xml(scan_id: &str) -> String {
    format!(
        "<ScanDesubscription>\
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

/// Parse a ScanResponse XML into contract IDs and scan time.
pub fn parse_scanner_response(xml: &str) -> Option<ScannerResult> {
    if !xml.contains("<ScanResponse>") {
        return None;
    }

    let scan_time = extract_xml_tag(xml, "scanTime").unwrap_or("").to_string();

    let mut con_ids = Vec::new();
    let mut search_start = 0;

    while let Some(c_start) = xml[search_start..].find("<Contract>") {
        let abs_start = search_start + c_start;
        let c_end = match xml[abs_start..].find("</Contract>") {
            Some(e) => abs_start + e + 11,
            None => break,
        };
        let contract_xml = &xml[abs_start..c_end];

        if let Some(id) = extract_xml_tag(contract_xml, "contractID") {
            if let Ok(n) = id.parse::<u32>() {
                con_ids.push(n);
            }
        }
        search_start = c_end;
    }

    Some(ScannerResult { con_ids, scan_time })
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
            max_items: 50,
        };
        let xml = build_scanner_subscribe_xml(&sub);
        assert!(xml.contains("<instrument>STK</instrument>"));
        assert!(xml.contains("<locations>STK.US.MAJOR</locations>"));
        assert!(xml.contains("<scanCode>TOP_PERC_GAIN</scanCode>"));
        assert!(xml.contains("<maxItems>50</maxItems>"));
        assert!(xml.contains("<source>API</source>"));
        assert!(xml.contains("<aggGroup>-1</aggGroup>"));
    }

    #[test]
    fn scanner_cancel_xml_structure() {
        let xml = build_scanner_cancel_xml("APISCAN31:3");
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

    #[test]
    fn parse_scanner_response_rejects_other() {
        assert!(parse_scanner_response("<ResultSetBar>...</ResultSetBar>").is_none());
        assert!(parse_scanner_response("not xml at all").is_none());
    }
}
