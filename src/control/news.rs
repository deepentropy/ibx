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
    pub query_id: String,
    pub con_id: u32,
    pub provider_codes: String,
    pub start_time: String,
    pub end_time: String,
    pub max_results: u32,
}

/// Parameters for a news article request.
#[derive(Debug, Clone)]
pub struct NewsArticleRequest {
    pub query_id: String,
    pub provider_code: String,
    pub article_id: String,
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

/// Build the `<id>` value for a news request.
fn build_news_id(req_num: &str, cmd: &str) -> String {
    format!("{};;NewsQuery;;0;;true;;0;;U", format_args!("{}-{}", req_num, cmd))
}

/// Build the XML query for a historical news request.
pub fn build_historical_news_xml(req: &HistoricalNewsRequest) -> String {
    // Convert "BRFG+BRFUPDN" to "BRFG*BRFUPDN" for url_key
    let providers_star = req.provider_codes.replace('+', "*");

    let tags = if req.con_id > 0 {
        format!("@@{}:ALL_SUB@", req.con_id)
    } else {
        format!("@@0:{}@", providers_star)
    };

    let query_raw = format!(
        "conid_count=\"{count}\";\
         total_count=\"{count}\";\
         ip=\"dummy\";\
         fingerprint=\"dummy\";\
         cmd=\"history\";\
         tags=\"{tags}\";\
         url_key=\"dummy\\;{providers}\";\
         ",
        count = req.max_results,
        tags = tags,
        providers = providers_star,
    );

    let id = build_news_id(&req.query_id, "history");
    let query_encoded = url_encode(&query_raw);

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
    let query_raw = format!(
        "eId=\"{article_id}*{provider}\";\
         ip=\"dummy\";\
         fingerprint=\"dummy\";\
         cmd=\"article_file\";\
         url_key=\"dummy\";\
         ",
        article_id = req.article_id,
        provider = req.provider_code,
    );

    let id = build_news_id(&req.query_id, "article_file");
    let query_encoded = url_encode(&query_raw);

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

/// Parse historical news headlines from the binary payload in tag 96.
///
/// Format: "200\n" + offset_table + ZIP(ENTRY with Java Properties)
/// Properties: h:0..h:N = pipe-delimited headlines, has_more flag.
/// Headline: `{headline}|{time}|{articleId}|{status}|{hasContent}|{providerCode}|{conIds...}`
pub fn parse_news_payload(raw: &[u8]) -> (Vec<NewsHeadline>, bool) {
    let mut headlines = Vec::new();
    let mut has_more = false;

    // Strip "200\n" status prefix, then decode j.c codec
    let after_status = if raw.starts_with(b"200\n") { &raw[4..] } else { raw };
    let decoded = jc_decode(after_status);

    let entry = match extract_zip_entry(&decoded) {
        Some(e) => e,
        None => return (headlines, has_more),
    };

    let text = String::from_utf8_lossy(&entry);

    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue; // Skip Java Properties comments and blank lines
        }
        // Java Properties: unescape `\:` → `:`, `\=` → `=`
        let unescaped = line.replace("\\:", ":").replace("\\=", "=");
        if unescaped.starts_with("has_more=") {
            has_more = unescaped.ends_with("true");
            continue;
        }
        // Match h:N= keys
        if let Some(eq_pos) = unescaped.find('=') {
            let key = &unescaped[..eq_pos];
            if key.starts_with("h:") && key[2..].parse::<u32>().is_ok() {
                let value = &unescaped[eq_pos + 1..];
                // Parse pipe-delimited: headline|time|articleId|status|hasContent|providerCode|conIds...
                let parts: Vec<&str> = value.split('|').collect();
                if parts.len() >= 6 {
                    headlines.push(NewsHeadline {
                        headline: parts[0].to_string(),
                        time: parts[1].to_string(),
                        article_id: parts[2].to_string(),
                        provider_code: parts[5].to_string(),
                    });
                }
            }
        }
    }

    (headlines, has_more)
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

/// Parse a news article body from the binary payload in tag 96.
/// Returns (article_type, article_text).
pub fn parse_article_payload(raw: &[u8]) -> Option<(i32, String)> {
    let after_status = if raw.starts_with(b"200\n") { &raw[4..] } else { raw };
    let decoded = jc_decode(after_status);
    let entry = extract_zip_entry(&decoded)?;
    let text = String::from_utf8_lossy(&entry);

    let mut body_encoded: Option<String> = None;

    for line in text.lines() {
        let line = line.trim();
        // Java Properties: unescape \: and \=
        let unescaped = line.replace("\\:", ":").replace("\\=", "=");
        if let Some(val) = unescaped.strip_prefix("b=") {
            body_encoded = Some(val.to_string());
        }
    }

    if let Some(encoded) = &body_encoded {
        let compressed = decode_byte_array(encoded);
        let mut decoder = flate2::read::GzDecoder::new(&compressed[..]);
        let mut article = String::new();
        if decoder.read_to_string(&mut article).is_ok() {
            return Some((0, article));
        }
    }

    // Fallback: return raw properties text
    Some((1, text.into_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn historical_news_xml_structure() {
        let req = HistoricalNewsRequest {
            query_id: "1".to_string(),
            con_id: 265598,
            provider_codes: "BRFG+BRFUPDN".to_string(),
            start_time: String::new(),
            end_time: String::new(),
            max_results: 10,
        };
        let xml = build_historical_news_xml(&req);
        assert!(xml.contains("<ListOfQueries>"));
        assert!(xml.contains("<NewsHMDSQuery>"));
        assert!(xml.contains("<id>1-history;;NewsQuery;;0;;true;;0;;U</id>"));
        assert!(xml.contains("<exchange>NEWS</exchange>"));
        assert!(xml.contains("<query>"));
        assert!(xml.contains("cmd="));
        assert!(xml.contains("265598"));
        assert!(xml.contains("BRFG*BRFUPDN"));
    }

    #[test]
    fn article_request_xml_structure() {
        let req = NewsArticleRequest {
            query_id: "2".to_string(),
            provider_code: "BRFG".to_string(),
            article_id: "BRFG$12345678".to_string(),
        };
        let xml = build_article_request_xml(&req);
        assert!(xml.contains("<ListOfQueries>"));
        assert!(xml.contains("<NewsHMDSQuery>"));
        assert!(xml.contains("<id>2-article_file;;NewsQuery;;0;;true;;0;;U</id>"));
        assert!(xml.contains("BRFG%2412345678*BRFG"));
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

    #[test]
    fn parse_news_payload_from_zip() {
        // Build a minimal ZIP with a single ENTRY containing news properties
        let props = b"h:0=Earnings beat|2026-01-15 10:00:00|BRFG$100|200|1|BRFG|265598\n\
                       h:1=Guidance raised|2026-01-16 11:00:00|BRFG$101|200|1|BRFG|265598\n\
                       has_more=false\n";
        let zip = build_test_zip(b"ENTRY", props);
        // Prefix with "200\n" status
        let mut payload = b"200\n".to_vec();
        payload.extend_from_slice(&zip);

        let (headlines, has_more) = parse_news_payload(&payload);
        assert_eq!(headlines.len(), 2);
        assert_eq!(headlines[0].headline, "Earnings beat");
        assert_eq!(headlines[0].time, "2026-01-15 10:00:00");
        assert_eq!(headlines[0].article_id, "BRFG$100");
        assert_eq!(headlines[0].provider_code, "BRFG");
        assert_eq!(headlines[1].headline, "Guidance raised");
        assert!(!has_more);
    }

    #[test]
    fn parse_news_payload_has_more() {
        let props = b"h:0=Test|2026-01-01|ART1|200|1|DJ-N|1234\nhas_more=true\n";
        let zip = build_test_zip(b"ENTRY", props);
        let mut payload = b"200\n".to_vec();
        payload.extend_from_slice(&zip);

        let (headlines, has_more) = parse_news_payload(&payload);
        assert_eq!(headlines.len(), 1);
        assert!(has_more);
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
