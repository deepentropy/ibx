//! News queries via the data connection.
//!
//! Request format: FIX msg_type=U, 6040=10030, 6118=XML
//! Response format: FIX msg_type=U, 6040=10032, 6118=XML (id echo), 96=binary payload
//!
//! The binary payload in tag 96 is: "200\n" + offset table + ZIP archive.
//! The ZIP contains a single ENTRY file in Java Properties format.

use std::io::Read;

// Tags for news data
pub const TAG_NEWS_XML: u32 = 6118;
pub const TAG_SUB_PROTOCOL: u32 = 6040;
pub const TAG_RAW_DATA_LENGTH: u32 = 95;
pub const TAG_RAW_DATA: u32 = 96;

/// Parameters for a historical news request.
#[derive(Debug, Clone)]
pub struct HistoricalNewsRequest {
    /// Query number, part of the query id (ibx#459).
    pub query_id: String,
    pub con_id: u32,
    /// Requested provider codes, `+` separated as the client gives them.
    pub provider_codes: String,
    pub start_time: String,
    pub end_time: String,
    pub max_results: u32,
    /// Subscribed source codes of the session, in logon order (ibx#459).
    pub subscribed: Vec<String>,
    /// Session key of news queries, from `news_url_key` (ibx#459).
    pub url_key: String,
}

/// Parameters for a news article request.
#[derive(Debug, Clone)]
pub struct NewsArticleRequest {
    pub query_id: String,
    pub provider_code: String,
    pub article_id: String,
    /// Session key of news queries, from `news_url_key` (ibx#459).
    pub url_key: String,
}

/// A single news headline parsed from a response.
#[derive(Debug, Clone)]
pub struct NewsHeadline {
    pub time: String,
    pub provider_code: String,
    pub article_id: String,
    pub headline: String,
}

/// Extract a simple XML tag value: `<tag>value</tag>` -> `value`.
fn extract_xml_tag<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(&xml[start..end])
}

/// URL-encode a string for the `<query>` field.
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9'
            | b'-' | b'_' | b'.' | b'*' | b';' | b'\\' | b'=' | b':' | b'@' => {
                out.push(b as char);
            }
            b' ' => out.push('+'),
            b'"' => out.push_str("%22"),
            _ => {
                out.push('%');
                out.push(char::from(b"0123456789ABCDEF"[(b >> 4) as usize]));
                out.push(char::from(b"0123456789ABCDEF"[(b & 0x0F) as usize]));
            }
        }
    }
    out
}

/// The session key of news queries, built as the reference builds it, once
/// per session (ibx#459).
pub fn news_url_key(subscribed: &[String], epoch_secs: u64, random: u32) -> String {
    format!("dummy;{};{};{}", subscribed.join(","), epoch_secs, random)
}

/// The query text of news properties, escaped and encoded as the
/// reference does (ibx#459).
fn news_query(props: &[(&str, &str)]) -> String {
    let mut raw = String::new();
    for (key, value) in props {
        raw.push_str(key);
        raw.push_str("=\"");
        for c in value.chars() {
            if matches!(c, '=' | ';' | '"' | '\\') {
                raw.push('\\');
            }
            raw.push(c);
        }
        raw.push_str("\";");
    }
    url_encode(&raw.replace(',', "*"))
}

/// Build the `<id>` value for a news request.
fn build_news_id(req_num: &str, cmd: &str) -> String {
    format!("{};;NewsQuery;;0;;true;;0;;U", format_args!("{}-{}", req_num, cmd))
}

/// The query id the server echoes for a request (ibx#459).
pub fn news_query_id(req_num: &str, cmd: &str) -> String {
    build_news_id(req_num, cmd)
}

/// The command and the date of a historical news request (ibx#459): the
/// end date when there is no start date, else the start date only.
pub fn historical_news_command(req: &HistoricalNewsRequest) -> (&'static str, &str) {
    if req.start_time.is_empty() {
        ("history", req.end_time.as_str())
    } else {
        ("more", req.start_time.as_str())
    }
}

/// The source list of a historical news request, as the reference writes
/// it (ibx#459): the requested codes sorted; the all-subscribed form when
/// they equal the subscribed list, or the all-subscribed-except form when
/// that is shorter.
pub fn news_sources_text(provider_codes: &str, subscribed: &[String]) -> String {
    let requested: std::collections::BTreeSet<&str> =
        provider_codes.split('+').filter(|c| !c.is_empty()).collect();
    let sources = requested.iter().copied().collect::<Vec<_>>().join(",");
    if subscribed.is_empty() {
        return sources;
    }
    if subscribed.join(",") == sources {
        return "ALL_SUB".to_string();
    }
    let mut sorted: Vec<&str> = subscribed.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    sorted.dedup();
    let mut except = String::from("ALL_SUB");
    for code in sorted {
        if !requested.contains(code) {
            except.push(',');
            except.push_str(code);
        }
    }
    if except.len() < sources.len() { except } else { sources }
}

/// Build the XML query for a historical news request.
pub fn build_historical_news_xml(req: &HistoricalNewsRequest) -> String {
    let (cmd, time) = historical_news_command(req);
    let tags = format!("{}@@{}:{}@", time, req.con_id, news_sources_text(&req.provider_codes, &req.subscribed));
    let count = req.max_results.to_string();
    let mut props: Vec<(&str, &str)> = Vec::new();
    if req.max_results != 0 {
        props.push(("conid_count", &count));
        props.push(("total_count", &count));
    }
    props.extend([("ip", "dummy"), ("fingerprint", "dummy"), ("cmd", cmd), ("tags", tags.as_str()),
                  ("url_key", req.url_key.as_str())]);

    let id = build_news_id(&req.query_id, cmd);
    let query_encoded = news_query(&props);

    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <ListOfQueries>\
         <NewsHMDSQuery>\
         <id>{id}</id>\
         <exchange>NEWS</exchange>\
         <secType>*</secType>\
         <source>API</source>\
         <needTotalValue>false</needTotalValue>\
         <wholeDays>false</wholeDays>\
         <delay>auto</delay>\
         <query>{query}</query>\
         <currency>*</currency>\
         </NewsHMDSQuery>\
         </ListOfQueries>",
        id = id,
        query = query_encoded,
    )
}

/// Build the XML query for a news article request.
pub fn build_article_request_xml(req: &NewsArticleRequest) -> String {
    let e_id = format!("{},{}", req.article_id, req.provider_code);
    let query_encoded = news_query(&[
        ("eId", &e_id), ("ip", "dummy"), ("fingerprint", "dummy"), ("cmd", "article_file"),
        ("url_key", &req.url_key),
    ]);

    let id = build_news_id(&req.query_id, "article_file");

    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <ListOfQueries>\
         <NewsHMDSQuery>\
         <id>{id}</id>\
         <exchange>NEWS</exchange>\
         <secType>*</secType>\
         <source>API</source>\
         <needTotalValue>false</needTotalValue>\
         <wholeDays>false</wholeDays>\
         <delay>auto</delay>\
         <query>{query}</query>\
         <currency>*</currency>\
         </NewsHMDSQuery>\
         </ListOfQueries>",
        id = id,
        query = query_encoded,
    )
}

/// The `<query>` text of a news request: equal texts are the same query
/// (ibx#459).
pub fn news_query_text(xml: &str) -> &str {
    extract_xml_tag(xml, "query").unwrap_or(xml)
}

/// Extract the query ID from a news response XML (tag 6118).
pub fn parse_news_response_id(xml: &str) -> Option<String> {
    extract_xml_tag(xml, "id").map(|s| s.to_string())
}

/// Extract the first file from a ZIP archive embedded in raw bytes.
/// Finds PK\x03\x04 magic and extracts using deflate.
/// Handles both sized entries and streamed entries (data descriptor with csize=0).
fn extract_zip_entry(data: &[u8]) -> Option<Vec<u8>> {
    // Find ZIP local file header magic
    let pk_pos = data.windows(4).position(|w| w == b"PK\x03\x04")?;
    let zip_data = &data[pk_pos..];

    // Parse local file header (30 bytes minimum)
    if zip_data.len() < 30 {
        return None;
    }
    let compression = u16::from_le_bytes([zip_data[8], zip_data[9]]);
    let compressed_size = u32::from_le_bytes([zip_data[18], zip_data[19], zip_data[20], zip_data[21]]) as usize;
    let filename_len = u16::from_le_bytes([zip_data[26], zip_data[27]]) as usize;
    let extra_len = u16::from_le_bytes([zip_data[28], zip_data[29]]) as usize;

    let data_start = 30 + filename_len + extra_len;
    if data_start > zip_data.len() {
        return None;
    }

    let entry_data = if compressed_size > 0 && data_start + compressed_size <= zip_data.len() {
        &zip_data[data_start..data_start + compressed_size]
    } else {
        // Data descriptor: csize=0 in header; feed all remaining data to decoder.
        // The deflate decoder will stop when the stream ends naturally.
        &zip_data[data_start..]
    };

    match compression {
        0 => Some(entry_data.to_vec()), // stored
        8 => {
            // deflate — feed all remaining data; decoder stops at stream end.
            // Use chunked reads to tolerate trailing garbage after the deflate stream.
            let mut decoder = flate2::read::DeflateDecoder::new(entry_data);
            let mut out = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                match decoder.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => out.extend_from_slice(&buf[..n]),
                    Err(_) => break, // trailing data after deflate stream
                }
            }
            if out.is_empty() { None } else { Some(out) }
        }
        _ => None,
    }
}

/// Reason text when a reply holds no data (ibx#459).
pub const NO_DATA: &str = "No data available";
/// Reason text of a reply status the reference does not read (ibx#459).
pub const REQUEST_IGNORED: &str = "Request ignored";

/// Load a Java properties text (ibx#459): comment lines, line
/// continuations, the `=`, `:` or blank key separator and the escapes.
pub fn load_properties(text: &str) -> Vec<(String, String)> {
    fn unescape(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c != '\\' {
                out.push(c);
                continue;
            }
            match chars.next() {
                Some('t') => out.push('\t'),
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('f') => out.push('\u{c}'),
                Some('u') => {
                    let hex: String = chars.by_ref().take(4).collect();
                    if let Some(ch) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                        out.push(ch);
                    }
                }
                Some(other) => out.push(other),
                None => {}
            }
        }
        out
    }
    // Join continued lines: a line ending in an odd number of backslashes
    // goes on with the next line, its leading blanks removed.
    let mut logical: Vec<String> = Vec::new();
    let mut pending: Option<String> = None;
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let line = match pending.take() {
            Some(mut acc) => { acc.push_str(line.trim_start_matches([' ', '\t', '\u{c}'])); acc }
            None => {
                let t = line.trim_start_matches([' ', '\t', '\u{c}']);
                if t.is_empty() || t.starts_with('#') || t.starts_with('!') { continue; }
                t.to_string()
            }
        };
        let trailing = line.chars().rev().take_while(|c| *c == '\\').count();
        if trailing % 2 == 1 {
            pending = Some(line[..line.len() - 1].to_string());
        } else {
            logical.push(line);
        }
    }
    if let Some(acc) = pending { logical.push(acc); }

    logical.into_iter().map(|line| {
        let bytes: Vec<char> = line.chars().collect();
        let mut i = 0;
        let mut escaped = false;
        while i < bytes.len() {
            let c = bytes[i];
            if escaped { escaped = false; } else if c == '\\' { escaped = true; } else if matches!(c, '=' | ':' | ' ' | '\t' | '\u{c}') { break; }
            i += 1;
        }
        let key: String = bytes[..i].iter().collect();
        let mut j = i;
        while j < bytes.len() && matches!(bytes[j], ' ' | '\t' | '\u{c}') { j += 1; }
        if j < bytes.len() && matches!(bytes[j], '=' | ':') { j += 1; }
        while j < bytes.len() && matches!(bytes[j], ' ' | '\t' | '\u{c}') { j += 1; }
        let value: String = bytes[j..].iter().collect();
        (unescape(&key), unescape(&value))
    }).collect()
}

/// Read the payload of a news reply as the reference does (ibx#459): its
/// status decides whether the properties are read, empty, or the request
/// is reported as ignored; a reply with no data gives "No data
/// available".
pub fn news_payload_properties(raw: &[u8]) -> Result<Vec<(String, String)>, String> {
    let (status, payload) = match raw.iter().position(|b| *b == b'\n') {
        Some(i) => (&raw[..i], &raw[i + 1..]),
        None => (raw, &raw[raw.len()..]),
    };
    let status = String::from_utf8_lossy(status).replace('\r', "");
    match status.trim().parse::<i32>().unwrap_or(500) {
        200 | 413 => {}
        403 => return Ok(Vec::new()),
        _ => return Err(REQUEST_IGNORED.to_string()),
    }
    if payload.is_empty() {
        return Err(NO_DATA.to_string());
    }
    let decoded = jc_decode(payload);
    let entry = extract_zip_entry(&decoded).ok_or_else(|| NO_DATA.to_string())?;
    Ok(load_properties(&String::from_utf8_lossy(&entry)))
}

fn property<'a>(props: &'a [(String, String)], key: &str) -> Option<&'a str> {
    props.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

/// A date as the reference reads a headline time: date, time and
/// fraction numbers from the start of the text.
fn parses_as_news_time(s: &str) -> bool {
    let mut rest = s;
    for sep in ['-', '-', ' ', ':', ':', '.'] {
        let n = rest.chars().take_while(|c| c.is_ascii_digit()).count();
        if n == 0 || !rest[n..].starts_with(sep) { return false; }
        rest = &rest[n + 1..];
    }
    rest.starts_with(|c: char| c.is_ascii_digit())
}

/// One headline line as the reference reads it (ibx#459): the time is the
/// first field after the headline, 15 to 25 long, that reads as a date;
/// fields before it are joined into the headline, which is kept as is.
/// The provider is taken only when it is not an integer and another field
/// follows. None when the line does not read.
pub fn parse_headline_line(line: &str) -> Option<NewsHeadline> {
    let parts: Vec<&str> = line.split('|').collect();
    let t = (1..parts.len())
        .find(|&i| (15..=25).contains(&parts[i].len()) && parses_as_news_time(parts[i]))
        .unwrap_or(1);
    let headline = parts[..t.min(parts.len())].concat();
    let field = |i: usize| parts.get(i).copied();
    let time = field(t)?;
    let article_id = field(t + 1)?;
    field(t + 2)?.parse::<i32>().ok()?;
    field(t + 3)?.parse::<i32>().ok()?;
    let mut provider_code = "";
    if parts.len() > t + 5 {
        let next = parts[t + 4];
        if next.parse::<i32>().map(|n| n.to_string() != next).unwrap_or(true) {
            provider_code = next;
        }
    }
    Some(NewsHeadline {
        headline,
        time: time.to_string(),
        article_id: article_id.to_string(),
        provider_code: provider_code.to_string(),
    })
}

/// Parse historical news headlines from the reply payload (ibx#459): the
/// headlines in their numbered order, and hasMore as the reference reads
/// it. A reply with an error text, or a status the reference does not
/// read, gives the reason.
pub fn parse_news_payload(raw: &[u8]) -> Result<(Vec<NewsHeadline>, bool), String> {
    let props = news_payload_properties(raw)?;
    if let Some(err) = property(&props, "error_code") {
        return Err(err.to_string());
    }
    let mut keyed: Vec<(u32, &str)> = props.iter()
        .filter_map(|(k, v)| Some((k.strip_prefix("h:")?.parse::<u32>().ok()?, v.as_str())))
        .collect();
    keyed.sort_by_key(|(n, _)| *n);
    let headlines = keyed.into_iter().filter_map(|(_, line)| {
        let h = parse_headline_line(line);
        if h.is_none() {
            log::warn!("Cannot process headline '{}'", line);
        }
        h
    }).collect();
    let has_more = property(&props, "has_more") == Some("1");
    Ok((headlines, has_more))
}

/// Read an interleaved big-endian int32 from j.c codec format.
/// Each int32 is stored as 8 bytes: `[b3, 0x00, b2, 0x00, b1, 0x00, b0, 0x00]`
fn jc_read_int32(buf: &[u8], offset: usize) -> u32 {
    if offset + 7 >= buf.len() {
        return 0;
    }
    (buf[offset] as u32) << 24
        | (buf[offset + 2] as u32) << 16
        | (buf[offset + 4] as u32) << 8
        | buf[offset + 6] as u32
}

/// Reverse the j.c newline-escape codec.
///
/// The codec replaces `0x0a` bytes in the binary with `0x00` and stores
/// their positions in an interleaved int32 offset table.
///
/// Layout after "200\n" status prefix:
/// - Bytes 0–7: count of offsets (interleaved int32)
/// - Bytes 8–8*(count+1)-1: offset entries (each 8 bytes)
/// - Bytes 8*(count+1)+: modified binary payload (ZIP data)
pub fn jc_decode(buf: &[u8]) -> Vec<u8> {
    if buf.len() < 8 {
        return buf.to_vec();
    }
    let count = jc_read_int32(buf, 0) as usize;
    let header_size = (count + 1) * 8;
    if header_size > buf.len() {
        return buf.to_vec();
    }
    let mut out = buf[header_size..].to_vec();
    for i in 0..count {
        let pos = jc_read_int32(buf, (i + 1) * 8) as usize;
        if pos < out.len() {
            out[pos] = 0x0A;
        }
    }
    out
}

/// Decode a signed-byte-encoded array from an article response.
/// Each char in the string represents a signed byte value.
/// Decode a signed-byte-encoded array from an article response.
/// Format: `{length}#{signed_byte}{signed_byte}...` where each signed byte
/// is a decimal integer prefixed by `+` or `-`, e.g. `1725#+31-117+8+0`.
fn decode_byte_array(s: &str) -> Vec<u8> {
    // Strip length prefix before '#'
    let data = match s.find('#') {
        Some(pos) => &s[pos + 1..],
        None => s,
    };
    let mut result = Vec::new();
    let mut num_start = 0;
    let bytes = data.as_bytes();
    let mut i = 0;
    while i <= bytes.len() {
        let at_delim = i == bytes.len()
            || (i > num_start && (bytes[i] == b'+' || bytes[i] == b'-'));
        if at_delim {
            let token = &data[num_start..i];
            if let Ok(val) = token.parse::<i16>() {
                result.push(val as u8);
            }
            num_start = i;
        }
        i += 1;
    }
    result
}

/// Parse a news article body from the reply payload, as the reference
/// reads it (ibx#459): `(article type, text)`, type 1 with the PDF bytes
/// in Base64 for a PDF answer, type 0 with the text for a text answer;
/// else the reason of the failure.
pub fn parse_article_payload(raw: &[u8]) -> Result<(i32, String), String> {
    use base64::Engine as _;
    let props = news_payload_properties(raw)?;
    if props.is_empty() {
        return Err(NO_DATA.to_string());
    }
    if let Some(err) = property(&props, "error_code") {
        return Err(err.to_string());
    }
    match property(&props, "cmd") {
        None => Err("Invalid response".to_string()),
        Some("pdf") if property(&props, "pdf").is_some() => {
            let bytes = decode_byte_array(property(&props, "pdf").unwrap_or(""));
            Ok((1, base64::engine::general_purpose::STANDARD.encode(bytes)))
        }
        Some("chains" | "details") if property(&props, "h").is_some() => {
            let mut article = String::new();
            if let Some(encoded) = property(&props, "b") {
                let compressed = decode_byte_array(encoded);
                let mut decoder = flate2::read::GzDecoder::new(&compressed[..]);
                let _ = decoder.read_to_string(&mut article);
            }
            Ok((0, article))
        }
        Some(_) => Err("No article was retrieved".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paper_sources() -> Vec<String> {
        ["BRFG", "BRFUPDN", "DJ-N", "DJ-RTA", "DJ-RTE", "DJ-RTG", "DJ-RTPRO", "DJNL"].map(String::from).to_vec()
    }

    fn news_req(con_id: u32, providers: &str, start: &str, end: &str) -> HistoricalNewsRequest {
        HistoricalNewsRequest {
            query_id: "1".to_string(),
            con_id,
            provider_codes: providers.to_string(),
            start_time: start.to_string(),
            end_time: end.to_string(),
            max_results: 10,
            subscribed: paper_sources(),
            url_key: news_url_key(&paper_sources(), 1772548584, 591255284),
        }
    }

    // ibx#459: the captured queries (ib-agent #35, #67, #69): all
    // subscribed providers are written in the all-subscribed form, two
    // providers as the sorted list; the session key as captured.
    #[test]
    fn historical_news_xml_structure() {
        let xml = build_historical_news_xml(&news_req(265598, "DJNL+BRFG+BRFUPDN+DJ-N+DJ-RTA+DJ-RTE+DJ-RTG+DJ-RTPRO", "", ""));
        assert!(xml.contains("<ListOfQueries>"));
        assert!(xml.contains("<NewsHMDSQuery>"));
        assert!(xml.contains("<id>1-history;;NewsQuery;;0;;true;;0;;U</id>"));
        assert!(xml.contains("<exchange>NEWS</exchange>"));
        assert!(xml.contains("cmd=%22history%22;"), "{xml}");
        assert!(xml.contains("tags=%22@@265598:ALL_SUB@%22;"), "{xml}");
        assert!(xml.contains("url_key=%22dummy\\;BRFG*BRFUPDN*DJ-N*DJ-RTA*DJ-RTE*DJ-RTG*DJ-RTPRO*DJNL\\;1772548584\\;591255284%22;"), "{xml}");
        assert!(xml.contains("conid_count=%2210%22;total_count=%2210%22;"), "{xml}");

        let xml = build_historical_news_xml(&news_req(0, "BRFUPDN+BRFG", "", ""));
        assert!(xml.contains("tags=%22@@0:BRFG*BRFUPDN@%22;"), "{xml}");
    }

    // ibx#459: the end date when there is no start date, else the start
    // date and no end date.
    #[test]
    fn historical_news_dates() {
        let xml = build_historical_news_xml(&news_req(265598, "BRFG", "", "2026-09-01 00:00:00.0"));
        assert!(xml.contains("<id>1-history;"));
        assert!(xml.contains("tags=%222026-09-01+00:00:00.0@@265598:BRFG@%22;"), "{xml}");
        let xml = build_historical_news_xml(&news_req(265598, "BRFG", "2026-08-01 00:00:00.0", "2026-09-01 00:00:00.0"));
        assert!(xml.contains("<id>1-more;") && xml.contains("cmd=%22more%22;"), "{xml}");
        assert!(xml.contains("tags=%222026-08-01+00:00:00.0@@265598:BRFG@%22;"), "{xml}");
        assert!(!xml.contains("2026-09-01"), "no end date with a start date: {xml}");
    }

    // ibx#459: the all-subscribed word minus the codes not asked, when
    // shorter than the list.
    #[test]
    fn news_sources_forms() {
        let subs = paper_sources();
        assert_eq!(news_sources_text("BRFG+BRFUPDN+DJ-N+DJ-RTA+DJ-RTE+DJ-RTG+DJ-RTPRO", &subs), "ALL_SUB,DJNL");
        assert_eq!(news_sources_text("DJ-N+BRFG", &subs), "BRFG,DJ-N");
        assert_eq!(news_sources_text("BRFG+BRFG", &subs), "BRFG");
        assert_eq!(news_sources_text("BRFG", &[]), "BRFG");
    }

    #[test]
    fn article_request_xml_structure() {
        let req = NewsArticleRequest {
            query_id: "2".to_string(),
            provider_code: "BRFG".to_string(),
            article_id: "BRFG$12345678".to_string(),
            url_key: news_url_key(&paper_sources(), 1, 2),
        };
        let xml = build_article_request_xml(&req);
        assert!(xml.contains("<ListOfQueries>"));
        assert!(xml.contains("<NewsHMDSQuery>"));
        assert!(xml.contains("<id>2-article_file;;NewsQuery;;0;;true;;0;;U</id>"));
        assert!(xml.contains("eId=%22BRFG%2412345678*BRFG%22;"), "{xml}");
        assert!(xml.contains("url_key=%22dummy\\;BRFG*"), "{xml}");
    }

    #[test]
    fn parse_news_response_id_basic() {
        let xml = r#"<NewsResponse><id>1-history;;NewsQuery;;0;;true;;0;;U</id></NewsResponse>"#;
        assert_eq!(
            parse_news_response_id(xml),
            Some("1-history;;NewsQuery;;0;;true;;0;;U".to_string())
        );
        assert_eq!(parse_news_response_id("<other>no id here</other>"), None);
    }

    #[test]
    fn extract_xml_tag_basic() {
        assert_eq!(extract_xml_tag("<a>hello</a>", "a"), Some("hello"));
        assert_eq!(extract_xml_tag("<x>123</x>", "x"), Some("123"));
        assert_eq!(extract_xml_tag("<x>123</x>", "y"), None);
    }

    fn payload(status: &[u8], props: &[u8]) -> Vec<u8> {
        let mut out = status.to_vec();
        out.extend_from_slice(&build_test_zip(b"ENTRY", props));
        out
    }

    #[test]
    fn parse_news_payload_from_zip() {
        let props = b"h\\:0=Earnings beat|2026-01-15 10:00:00.0|BRFG$100|200|1|BRFG|265598\n\
                       h\\:1=Guidance raised|2026-01-16 11:00:00.0|BRFG$101|200|1|BRFG|265598\n\
                       has_more=0\n";
        let (headlines, has_more) = parse_news_payload(&payload(b"200\n", props)).unwrap();
        assert_eq!(headlines.len(), 2);
        assert_eq!(headlines[0].headline, "Earnings beat");
        assert_eq!(headlines[0].time, "2026-01-15 10:00:00.0");
        assert_eq!(headlines[0].article_id, "BRFG$100");
        assert_eq!(headlines[0].provider_code, "BRFG");
        assert_eq!(headlines[1].headline, "Guidance raised");
        assert!(!has_more);
    }

    // ibx#459 (ibx#147 reopened): the metadata prefix is kept, as the
    // reference delivers it.
    #[test]
    fn parse_news_payload_keeps_metadata_prefix() {
        let props = b"h\\:0={A\\:800015\\:L\\:en}Globalstar Stock Rises|2026-03-03 14:02:00.0|BRFG$100|0|1|BRFG|265598\n\
                       h\\:1=Plain headline|2026-01-15 12:00:00.0|BRFG$102|0|1|BRFG|265598\n";
        let (headlines, _) = parse_news_payload(&payload(b"200\n", props)).unwrap();
        assert_eq!(headlines[0].headline, "{A:800015:L:en}Globalstar Stock Rises");
        assert_eq!(headlines[1].headline, "Plain headline");
    }

    // ibx#459: hasMore as captured (ib-agent #35).
    #[test]
    fn parse_news_payload_has_more() {
        let props = b"h\\:0=Test|2026-01-01 00:00:00.0|ART1|0|1|DJ-N|1234\nhas_more=1\n";
        let (headlines, has_more) = parse_news_payload(&payload(b"200\n", props)).unwrap();
        assert_eq!(headlines.len(), 1);
        assert!(has_more);
        let (_, has_more) = parse_news_payload(&payload(b"200\n", b"has_more=true\n")).unwrap();
        assert!(!has_more);
    }

    // ibx#459: the reply status decides; an error text is a failure.
    #[test]
    fn news_payload_status_and_errors() {
        assert_eq!(parse_news_payload(b"403\n").unwrap().0.len(), 0);
        assert!(!parse_news_payload(b"403\n").unwrap().1);
        for status in [&b"500\n"[..], b"401\n", b"100\n", b"abc\n"] {
            assert_eq!(parse_news_payload(status).unwrap_err(), "Request ignored");
        }
        assert_eq!(parse_news_payload(b"200\n").unwrap_err(), "No data available");
        assert_eq!(parse_news_payload(&payload(b"200\r\n", b"error_code=Not available\n")).unwrap_err(), "Not available");
        assert_eq!(parse_article_payload(b"403\n").unwrap_err(), "No data available");
    }

    // ibx#459: the headline line rules of the reference.
    #[test]
    fn headline_line_rules() {
        let h = parse_headline_line("A|B|2026-03-03 14:02:00.0|ID|0|1|DJ-N|265598").unwrap();
        assert_eq!((h.headline.as_str(), h.time.as_str(), h.article_id.as_str(), h.provider_code.as_str()),
                   ("AB", "2026-03-03 14:02:00.0", "ID", "DJ-N"));
        let h = parse_headline_line("T|2026-03-03 14:02:00.0|ID|0|1|265598").unwrap();
        assert_eq!(h.provider_code, "", "a single field left is a conId");
        let h = parse_headline_line("T|2026-03-03 14:02:00.0|ID|0|1|42|265598").unwrap();
        assert_eq!(h.provider_code, "", "an integer is not a provider");
        assert!(parse_headline_line("T|2026-03-03 14:02:00.0|ID|x|1|BRFG|1").is_none());
        assert!(parse_headline_line("T|2026-03-03 14:02:00.0|ID").is_none());
    }

    #[test]
    fn properties_loader() {
        let p = load_properties("# c\n! c\na\\:b=x\\ty\\u00e9\nk : v\\\n   w\nz\n");
        assert_eq!(p, [("a:b".to_string(), "x\ty\u{e9}".to_string()), ("k".to_string(), "vw".to_string()),
                       ("z".to_string(), String::new())]);
    }

    // ibx#459: a PDF answer is type 1 with Base64 bytes; other commands
    // and a missing command are failures.
    #[test]
    fn article_payload_forms() {
        let (t, text) = parse_article_payload(&payload(b"200\n", b"cmd=pdf\npdf=3#+37+80-1\n")).unwrap();
        assert_eq!((t, text.as_str()), (1, "JVD/"));
        assert_eq!(parse_article_payload(&payload(b"200\n", b"cmd=top\nh=x\n")).unwrap_err(), "No article was retrieved");
        assert_eq!(parse_article_payload(&payload(b"200\n", b"h=x\n")).unwrap_err(), "Invalid response");
        assert_eq!(parse_article_payload(&payload(b"200\n", b"error_code=Request Timed Out\n")).unwrap_err(), "Request Timed Out");
    }

    /// Build a minimal ZIP archive with one stored (uncompressed) file.
    fn build_test_zip(name: &[u8], data: &[u8]) -> Vec<u8> {
        let mut zip = Vec::new();
        let crc = crc32(data);
        // Local file header
        zip.extend_from_slice(b"PK\x03\x04");      // signature
        zip.extend_from_slice(&20u16.to_le_bytes()); // version needed
        zip.extend_from_slice(&0u16.to_le_bytes());  // flags
        zip.extend_from_slice(&0u16.to_le_bytes());  // compression: stored
        zip.extend_from_slice(&0u16.to_le_bytes());  // mod time
        zip.extend_from_slice(&0u16.to_le_bytes());  // mod date
        zip.extend_from_slice(&crc.to_le_bytes());   // crc32
        zip.extend_from_slice(&(data.len() as u32).to_le_bytes()); // compressed
        zip.extend_from_slice(&(data.len() as u32).to_le_bytes()); // uncompressed
        zip.extend_from_slice(&(name.len() as u16).to_le_bytes()); // name len
        zip.extend_from_slice(&0u16.to_le_bytes());  // extra len
        zip.extend_from_slice(name);
        zip.extend_from_slice(data);
        zip
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut crc: u32 = 0xFFFFFFFF;
        for &b in data {
            crc ^= b as u32;
            for _ in 0..8 {
                if crc & 1 != 0 { crc = (crc >> 1) ^ 0xEDB88320; }
                else { crc >>= 1; }
            }
        }
        !crc
    }
}
