//! The decoders of network input in groups, for the robustness tests
//! (ibx#488): the property and mutation tests of `src/robustness/` and the
//! fuzz targets of `fuzz/` give any bytes to each group. A group returns
//! nothing: it passes when no decoder panics.

use crate::protocol::{depth_decoder, fix, fixcomp, ns, tick_decoder, xyz};

/// Bytes a decoder may hold per input byte: the decoded frames hold a few
/// copies and a map entry per field.
pub const PER_BYTE: usize = 256;
/// A compressed payload inflates up to about 1032 times.
pub const PER_BYTE_INFLATED: usize = 2048;

/// A decoder group: name, function, bytes held per input byte.
pub type DecoderGroup = (&'static str, fn(&[u8]), usize);

/// The decoder groups.
pub const DECODERS: &[DecoderGroup] = &[
    ("ns", ns, PER_BYTE),
    ("xyz", xyz, PER_BYTE),
    ("fix", fix, PER_BYTE),
    ("fixcomp", fixcomp, PER_BYTE_INFLATED),
    ("connection", connection, PER_BYTE),
    ("ticks", ticks, PER_BYTE),
    ("depth", depth, PER_BYTE),
    ("generic", generic, PER_BYTE),
    ("tick_by_tick", tick_by_tick, PER_BYTE),
    ("farm_text", farm_text, PER_BYTE),
    ("hmds_xml", hmds_xml, PER_BYTE),
    ("hmds_binary", hmds_binary, PER_BYTE_INFLATED),
    ("contracts", contracts, PER_BYTE),
    ("logon", logon, PER_BYTE_INFLATED),
    ("auth", auth, PER_BYTE),
    ("key_exchange", key_exchange, PER_BYTE),
];

/// NS frames of the login: header, length and payload.
pub fn ns(b: &[u8]) {
    let _ = ns::ns_recv(&mut std::io::Cursor::new(b));
    let _ = crate::auth::session::recv_msg(&mut std::io::Cursor::new(b));
    for payload in [b, b.get(8..).unwrap_or_default()] {
        let _ = ns::ns_parse(payload);
        let _ = ns::parse_test_request_timestamp(payload);
        let _ = ns::is_ns_text(payload);
        let _ = crate::auth::session::AuthStart::parse(payload);
    }
    for f in String::from_utf8_lossy(b).split(';') {
        let _ = crate::auth::session::SecondFactor::parse(f);
    }
}

/// XYZ binary messages of the login.
pub fn xyz(b: &[u8]) {
    if let Some((_, _, _, fields)) = xyz::xyz_parse_response(b) {
        let _ = xyz::parse_swcr_token_challenge(&fields);
    }
}

/// FIX text frames: tag parsing, signature check, checksum framing.
pub fn fix(b: &[u8]) {
    let _ = fix::fix_parse(b);
    let _ = fix::is_signed(b);
    let _ = fix::fix_unsign(b, &[7u8; 20], &[3u8; 16]);
    let _ = fix::fix_read(&mut std::io::Cursor::new(b));
    let _ = fix::fix_read_deadline(&mut std::io::Cursor::new(b), std::time::Instant::now());
    let _ = fix::fmt_pipe(b);
    let _ = crate::engine::hot_loop::fast_extract_msg_type(b);
    let _ = crate::engine::hot_loop::extract_raw_tag(b, 96);
}

/// Compressed frames: length, inflate, split into messages.
pub fn fixcomp(b: &[u8]) {
    let _ = fixcomp::fixcomp_length(b);
    let _ = fixcomp::fixcomp_decompress(b);
}

/// The frame splitter of a signed connection: any bytes in its buffer.
pub fn connection(b: &[u8]) {
    use crate::protocol::connection::{mem_pair, Connection, Frame};
    let (end, _peer) = mem_pair();
    let mut conn = Connection::new_mem(end);
    conn.set_keys(Vec::new(), Vec::new(), vec![7u8; 20], vec![3u8; 16]);
    conn.seed_buffer(b);
    for frame in conn.extract_frames() {
        let (Frame::Fix(raw) | Frame::FixComp(raw) | Frame::Binary(raw) | Frame::Control(raw)) = frame;
        let _ = conn.unsign(&raw);
    }
}

/// 35=P ticks, and their values in the price arithmetic (ibx#272).
pub fn ticks(b: &[u8]) {
    let body = crate::engine::hot_loop::find_body_after_tag(b, b"35=P\x01").unwrap_or(b);
    let mut ticks = Vec::new();
    let _ = tick_decoder::decode_ticks_35p_into(body, &mut ticks);
    let _ = tick_decoder::decode_ticks_35p(b);
    let mut market = crate::engine::market_state::MarketState::new();
    let id = market.try_register(1).unwrap();
    for t in &ticks {
        market.apply_tick_sized(id, 1_000_000, 1_000_000, t.server_tag % 2 == 0, t);
    }
}

/// 35=Y / 35=Z depth.
pub fn depth(b: &[u8]) {
    if let Some((body, extended)) = depth_decoder::depth_body(b) {
        let _ = depth_decoder::decode_depth(body, extended);
    }
    let _ = depth_decoder::decode_depth(b, true);
    let _ = depth_decoder::decode_depth(b, false);
}

/// 35=G: news headlines, exchange maps, real-time bars.
pub fn generic(b: &[u8]) {
    use crate::engine::hot_loop::farm;
    let body = crate::engine::hot_loop::find_body_after_tag(b, b"35=G\x01").unwrap_or(b);
    for payload in [body, b] {
        for item in farm::decode_news(payload) {
            let _ = farm::split_headline(&item.raw_headline);
        }
        let _ = farm::parse_exchange_map(&String::from_utf8_lossy(payload));
        for (_, _, bar) in tick_decoder::rtbar_entries(payload) {
            let _ = tick_decoder::decode_bar_payload(bar, 0.01);
            let _ = crate::control::historical::decode_bar_payload(bar, 0.01);
        }
        let _ = tick_decoder::decode_bar_payload(payload, 0.01);
        let _ = crate::control::historical::decode_bar_payload(payload, 1e-300);
    }
}

/// 35=E tick-by-tick, each entry kind taken from its stream id.
pub fn tick_by_tick(b: &[u8]) {
    use tick_decoder::{TbtEntryKind as K, TbtLayout as L};
    let kind = |id: u64| match id % 9 {
        0 => K::Read(L::Trade { sized: true }),
        1 => K::Read(L::Trade { sized: false }),
        2 => K::Read(L::BidAsk { sized: true }),
        3 => K::Read(L::BidAsk { sized: false }),
        4 => K::Read(L::MidPoint { sized: true }),
        5 => K::Read(L::MidPoint { sized: false }),
        6 => K::Skip((id % 7) as usize),
        _ => K::Guess,
    };
    let body = crate::engine::hot_loop::find_body_after_tag(b, b"35=E\x01").unwrap_or(b);
    let _ = tick_decoder::decode_tbt_frame(body, kind, (1_600_000_000, 1_900_000_000));
    for pos in 0..body.len().min(16) {
        let (v, n) = tick_decoder::read_vlq(body, pos);
        let _ = tick_decoder::vlq_signed(v, n);
        let _ = tick_decoder::read_vlq_bounded(body, pos);
        let _ = tick_decoder::read_hibit_str(body, pos);
    }
}

/// 35=Q acknowledgements, 35=L trade setups, 35=T routing tables, 35=3
/// rejects: comma-separated and tag text.
pub fn farm_text(b: &[u8]) {
    use crate::engine::routing::{table_text, RoutingTable, TableKind};
    if let Some(text) = table_text(b) {
        let _ = RoutingTable::parse(&text, TableKind::MarketData);
    }
    let text = String::from_utf8_lossy(b);
    let _ = RoutingTable::parse(&text, TableKind::Historical);
    let _ = crate::engine::price_mgmt::parse_exclusions(&text);
}

/// XML replies of the historical farm and the auth link: bars, ticks,
/// head timestamps, schedules, histograms, scanner results, news,
/// fundamentals, option model inputs, algo definitions.
fn xml_reply(xml: &str) {
    use crate::control::{algo, fundamental, histogram, historical, news, optcalc, scanner};
    use crate::types::TbtType;
    let _ = historical::parse_bar_response(xml);
    let _ = historical::parse_series(xml);
    let _ = historical::parse_leg_bars(xml);
    let _ = historical::parse_ticker_id(xml);
    let _ = historical::parse_tick_response(xml);
    for t in [TbtType::Last, TbtType::AllLast, TbtType::BidAsk, TbtType::MidPoint] {
        let _ = historical::parse_tick_by_tick_history(xml, t);
    }
    let _ = historical::parse_schedule_response(xml);
    if let Some(r) = historical::parse_head_timestamp_response(xml) {
        let _ = historical::parse_server_time(&r.head_timestamp);
    }
    let _ = historical::extract_xml_tag(xml, "eoq");
    let _ = historical::window_id(xml);
    let _ = histogram::parse_histogram_response(xml);
    let _ = histogram::parse_histogram_frame(xml);
    let _ = scanner::parse_scanner_response(xml);
    let _ = scanner::scan_size_limits(xml);
    let _ = news::parse_news_response_id(xml);
    let _ = news::news_query_text(xml);
    let _ = fundamental::fundamental_error_text(xml);
    let _ = fundamental::parse_fundamental_response_id(xml);
    let items = optcalc::inputs::parse_reference_xml(xml);
    let _ = optcalc::inputs::dividends_from_items(&items);
    let mut algos = algo::AlgoDefinitions::default();
    algos.add(xml);
    let _ = algo::refusal(&algos, "Vwap", &[("maxPctVol", "x")], false);
}

pub fn hmds_xml(b: &[u8]) {
    xml_reply(&String::from_utf8_lossy(b));
    if let Some(xml) = fix::fix_parse(b).get(&6118) {
        xml_reply(xml);
    }
}

/// Binary payloads of the historical farm: news results and articles
/// (properties, compressed), fundamental data (compressed).
pub fn hmds_binary(b: &[u8]) {
    use crate::control::{fundamental, news};
    let raw96 = crate::engine::hot_loop::extract_raw_tag(b, 96);
    for payload in [b, raw96.as_deref().unwrap_or_default()] {
        let _ = news::news_payload_properties(payload);
        let _ = news::parse_news_payload(payload);
        let _ = news::parse_article_payload(payload);
        let _ = news::jc_decode(payload);
        let _ = fundamental::decompress_fundamental_data(payload);
    }
    let text = String::from_utf8_lossy(b);
    let _ = news::load_properties(&text);
    for line in text.lines() {
        let _ = news::parse_headline_line(line);
    }
}

/// 35=d contract replies, 6040=107 schedules and the rows built from them.
pub fn contracts(b: &[u8]) {
    use crate::control::contracts;
    let _ = contracts::parse_secdef_response(b);
    let _ = contracts::secdef_response_req_id(b);
    let _ = contracts::secdef_response_is_last(b);
    let _ = contracts::parse_market_rules(b);
    let _ = contracts::parse_size_increments(b);
    let _ = contracts::round_lot_from_secdef(b);
    let _ = contracts::agg_group_from_secdef(b);
    let _ = contracts::smart_components_from_secdef(b);
    let schedule = contracts::parse_schedule_response(b);
    if let Some(s) = &schedule {
        let _ = contracts::format_sessions_string(&s.trading_hours);
        let _ = contracts::schedule_zone(&s.timezone);
    }
    for mut def in contracts::parse_secdef_records(b).unwrap_or_default() {
        let _ = contracts::market_rule_ids(&def, &Default::default());
        if let Some(s) = &schedule {
            contracts::apply_schedule(&mut def, s, jiff::Timestamp::now());
        }
    }
    let _ = contracts::row_of_reply(b, 265598, "SMART", false, &Default::default(), &[b], Some(b), jiff::Timestamp::now(), true, true);
}

/// The logon reply: its tags, plain or compressed, and the values the
/// session takes from them.
pub fn logon(b: &[u8]) {
    use crate::control::logon;
    let mut tags = fix::fix_parse(b);
    if let Ok(inner) = fixcomp::fixcomp_decompress(b) {
        for m in inner {
            tags.extend(fix::fix_parse(&m));
        }
    }
    let values = crate::gateway::LogonValues::read(&tags, 1_759_000_000_000, 1_759_000_000_100);
    let _ = logon::has_feature(values.features.as_deref().unwrap_or(""), "PRICEMGMT");
    for v in tags.values() {
        let _ = logon::server_time_ms(v);
        let _ = logon::max_backfill_years(Some(v));
        let _ = logon::version_cutoff_warning(Some(v), Some(v));
        let _ = logon::cutoff_applies(&logon::own_version(), v);
        let _ = logon::backfill_years_refusal(v, 3);
        let _ = crate::engine::price_mgmt::parse_exclusions(v);
    }
}

/// Text tags of auth messages: matching symbols (6040=186), option chain
/// answers (6040=5, 6040=139).
pub fn auth(b: &[u8]) {
    use crate::control::{contracts, optparams};
    let _ = contracts::parse_matching_symbols_response(b);
    let _ = optparams::parse_underlying_answer(b);
    let _ = optparams::parse_strike_scales(b);
    if let Some(answer) = optparams::parse_chain_answer(b) {
        let _ = optparams::chain_rows(std::slice::from_ref(&answer), false, true);
        let _ = optparams::chain_rows(&[answer], true, false);
    }
}

/// An encrypted frame of the login, after the key exchange (ibx#276).
pub fn key_exchange(b: &[u8]) {
    let mut channel = crate::auth::dh::SecureChannel::zero_keys_for_test();
    let _ = channel.decrypt(b);
    let mut framed = ns::NS_MAGIC.to_vec();
    framed.extend((b.len() as u32).to_be_bytes());
    framed.extend(b);
    let _ = crate::auth::session::recv_secure(&mut std::io::Cursor::new(&framed), &mut channel);
    let _ = crate::auth::session::recv_auth_start(&mut std::io::Cursor::new(&framed), &mut channel);
}

/// A message given to the three message handlers of a new engine (farm,
/// auth, historical); a compressed frame is opened first.
pub fn engine(b: &[u8]) {
    use std::sync::Arc;
    let shared = Arc::new(crate::bridge::SharedState::new());
    let (farm, _farm) = super::Peer::pair();
    let (ccp, _ccp) = super::Peer::pair();
    let (hmds, _hmds) = super::Peer::pair();
    let (mut engine, _control) = crate::engine::hot_loop::HotLoop::with_connections(
        shared, None, "DUXXXXXXX".into(), farm, ccp, Some(hmds), None);
    let msgs = if b.starts_with(b"8=FIXCOMP") { fixcomp::fixcomp_decompress(b).unwrap_or_default() } else { vec![b.to_vec()] };
    for m in msgs {
        engine.inject_farm_message(&m);
        engine.inject_ccp_message(&m);
        engine.inject_hmds_message(&m);
    }
}
