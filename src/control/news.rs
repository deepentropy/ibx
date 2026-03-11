//! News queries via ushmds connection (XML-in-FIX tag 6118).
//!
//! Historical news and article retrieval use FIX 35=U with 6040=10030/10032.
//! News providers are resolved from gateway cache (no FIX message).
//! Real-time tick news arrives via binary 8=O|35=G on usfarm.

// FIX tags for news data
pub const TAG_NEWS_XML: u32 = 6118;
pub const TAG_SUB_PROTOCOL: u32 = 6040;
pub const TAG_RAW_DATA_LENGTH: u32 = 95;
pub const TAG_RAW_DATA: u32 = 96;

/// Parameters for a historical news request.
#[derive(Debug, Clone)]
pub struct HistoricalNewsRequest {
    pub con_id: u32,
    pub provider_codes: String,
    pub start_time: String,
    pub end_time: String,
    pub max_results: u32,
}

/// Parameters for a news article request.
#[derive(Debug, Clone)]
pub struct NewsArticleRequest {
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

/// Build the XML query for a historical news request (6040=10030, cmd="history").
pub fn build_historical_news_xml(req: &HistoricalNewsRequest) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <NewsHMDSQuery>\
         <cmd>history</cmd>\
         <conId>{con_id}</conId>\
         <providerCodes>{providers}</providerCodes>\
         <startTime>{start}</startTime>\
         <endTime>{end}</endTime>\
         <maxResults>{max}</maxResults>\
         </NewsHMDSQuery>",
        con_id = req.con_id,
        providers = req.provider_codes,
        start = req.start_time,
        end = req.end_time,
        max = req.max_results,
    )
}

/// Build the XML query for a news article request (6040=10030, cmd="article_file").
pub fn build_article_request_xml(req: &NewsArticleRequest) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <NewsHMDSQuery>\
         <cmd>article_file</cmd>\
         <eId>{article_id}*{provider_code}</eId>\
         </NewsHMDSQuery>",
        article_id = req.article_id,
        provider_code = req.provider_code,
    )
}

/// Extract the query ID from a news response XML in tag 6118.
pub fn parse_news_response_id(xml: &str) -> Option<String> {
    extract_xml_tag(xml, "id").map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn historical_news_xml_structure() {
        let req = HistoricalNewsRequest {
            con_id: 265598,
            provider_codes: "BRFG+BRFUPDN".to_string(),
            start_time: "2026-03-01T00:00:00".to_string(),
            end_time: "2026-03-10T23:59:59".to_string(),
            max_results: 10,
        };
        let xml = build_historical_news_xml(&req);
        assert!(xml.contains("<NewsHMDSQuery>"));
        assert!(xml.contains("<cmd>history</cmd>"));
        assert!(xml.contains("<conId>265598</conId>"));
        assert!(xml.contains("<providerCodes>BRFG+BRFUPDN</providerCodes>"));
        assert!(xml.contains("<startTime>2026-03-01T00:00:00</startTime>"));
        assert!(xml.contains("<endTime>2026-03-10T23:59:59</endTime>"));
        assert!(xml.contains("<maxResults>10</maxResults>"));
    }

    #[test]
    fn article_request_xml_structure() {
        let req = NewsArticleRequest {
            provider_code: "BRFG".to_string(),
            article_id: "BRFG$12345678".to_string(),
        };
        let xml = build_article_request_xml(&req);
        assert!(xml.contains("<NewsHMDSQuery>"));
        assert!(xml.contains("<cmd>article_file</cmd>"));
        assert!(xml.contains("<eId>BRFG$12345678*BRFG</eId>"));
    }

    #[test]
    fn parse_news_response_id_basic() {
        let xml = r#"<NewsResponse><id>news_q1</id><status>ok</status></NewsResponse>"#;
        assert_eq!(parse_news_response_id(xml), Some("news_q1".to_string()));

        assert_eq!(parse_news_response_id("<other>no id here</other>"), None);
    }

    #[test]
    fn extract_xml_tag_basic() {
        assert_eq!(extract_xml_tag("<a>hello</a>", "a"), Some("hello"));
        assert_eq!(extract_xml_tag("<x>123</x>", "x"), Some("123"));
        assert_eq!(extract_xml_tag("<x>123</x>", "y"), None);
    }
}
