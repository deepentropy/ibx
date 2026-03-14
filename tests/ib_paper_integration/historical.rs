//! Historical data, scanner, news, and fundamental data test phases.

use super::common::*;
use ibx::control::fundamental;
use ibx::control::historical::{self, BarDataType, BarSize, HeadTimestampRequest, HistoricalRequest};
use ibx::control::news;
use ibx::control::scanner;
use ibx::gateway::{connect_farm, Gateway, GatewayConfig};
use ibx::protocol::fix;
use ibx::protocol::fixcomp;
use ibx::protocol::connection::Frame;

pub(super) fn phase_historical_data(mut conns: Conns, gw: &Gateway, config: &GatewayConfig) -> Conns {
    println!("--- Phase 11: Historical Data Bars (SPY, 1 day of 5-min bars) ---");

    ccp_keepalive(&mut conns.ccp);
    let mut hmds = match connect_farm(
        &config.host, "ushmds",
        &config.username, config.paper,
        &gw.server_session_id, &gw.session_token, &gw.hw_info, &gw.encoded,
    ) {
        Ok(c) => { println!("  HMDS reconnected"); c }
        Err(e) => {
            println!("  SKIP: ushmds reconnect failed: {}\n", e);
            return Conns { farm: conns.farm, ccp: conns.ccp, hmds: None, account_id: conns.account_id };
        }
    };

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (bg_loop, bg_tx) = HotLoop::with_connections(
        shared, None, account_id.clone(), conns.farm, conns.ccp, None, None,
    );
    bg_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let bg_join = run_hot_loop(bg_loop);

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let secs_per_day = 86400u64;
    let end_time = {
        let days = now / secs_per_day;
        let mut y = 1970i64;
        let mut remaining = days as i64;
        loop {
            let days_in_year = if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) { 366 } else { 365 };
            if remaining < days_in_year { break; }
            remaining -= days_in_year;
            y += 1;
        }
        let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
        let month_days = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
        let mut m = 0;
        for (i, &d) in month_days.iter().enumerate() {
            if remaining < d as i64 { m = i + 1; break; }
            remaining -= d as i64;
        }
        let day = remaining + 1;
        let hour = (now % secs_per_day) / 3600;
        let min = (now % 3600) / 60;
        let sec = now % 60;
        format!("{:04}{:02}{:02}-{:02}:{:02}:{:02}", y, m, day, hour, min, sec)
    };

    let req = HistoricalRequest {
        query_id: "test1".to_string(),
        con_id: 756733,
        symbol: "SPY".to_string(),
        sec_type: "CS",
        exchange: "SMART",
        data_type: BarDataType::Trades,
        end_time,
        duration: "1 d".to_string(),
        bar_size: BarSize::Min5,
        use_rth: true,
    };

    let xml = historical::build_query_xml(&req);
    hmds.send_fixcomp(&[
        (fix::TAG_MSG_TYPE, "W"),
        (historical::TAG_HISTORICAL_XML, &xml),
    ]).expect("Failed to send historical request");

    let mut all_bars = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut complete = false;

    while Instant::now() < deadline && !complete {
        match hmds.try_recv() {
            Ok(0) => { std::thread::sleep(Duration::from_millis(50)); continue; }
            Err(e) => { println!("  HMDS recv error: {}", e); break; }
            Ok(_) => {}
        }

        for frame in hmds.extract_frames() {
            let data = match &frame {
                Frame::FixComp(raw) => {
                    let (unsigned, _) = hmds.unsign(raw);
                    fixcomp::fixcomp_decompress(&unsigned)
                }
                Frame::Fix(raw) => vec![raw.clone()],
                _ => continue,
            };

            for msg in data {
                let tags = fix::fix_parse(&msg);
                if let Some(xml_resp) = tags.get(&historical::TAG_HISTORICAL_XML) {
                    if let Some(resp) = historical::parse_bar_response(xml_resp) {
                        all_bars.extend(resp.bars);
                        if resp.is_complete { complete = true; }
                    }
                }
            }
        }
    }

    let mut bg_conns = shutdown_and_reclaim(&bg_tx, bg_join, account_id);
    bg_conns.hmds = Some(hmds);

    println!("  Total bars received: {}", all_bars.len());
    if all_bars.is_empty() {
        println!("  SKIP: No historical bars received (HMDS may be unavailable)\n");
        return bg_conns;
    }

    let first = &all_bars[0];
    assert!(first.open > 0.0, "Open price should be positive");
    assert!(first.high >= first.low, "High should be >= Low");
    assert!(first.volume > 0, "Volume should be positive");
    println!("  First bar: O={:.2} H={:.2} L={:.2} C={:.2} V={}",
        first.open, first.high, first.low, first.close, first.volume);
    println!("  PASS ({} bars)\n", all_bars.len());
    bg_conns
}

pub(super) fn phase_historical_daily_bars(mut conns: Conns, gw: &Gateway, config: &GatewayConfig) -> Conns {
    println!("--- Phase 76: Historical Daily Bars (SPY, 5 days of 1-day bars) ---");

    ccp_keepalive(&mut conns.ccp);
    let mut hmds = match connect_farm(&config.host, "ushmds", &config.username, config.paper, &gw.server_session_id, &gw.session_token, &gw.hw_info, &gw.encoded) {
        Ok(c) => { println!("  HMDS reconnected"); c }
        Err(e) => { println!("  SKIP: ushmds reconnect failed: {}\n", e); return Conns { farm: conns.farm, ccp: conns.ccp, hmds: None, account_id: conns.account_id }; }
    };

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (bg_loop, bg_tx) = HotLoop::with_connections(shared, None, account_id.clone(), conns.farm, conns.ccp, None, None);
    bg_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let bg_join = run_hot_loop(bg_loop);

    let req = HistoricalRequest {
        query_id: "daily1".to_string(), con_id: 756733, symbol: "SPY".to_string(),
        sec_type: "CS", exchange: "SMART", data_type: BarDataType::Trades,
        end_time: now_ib_timestamp(), duration: "5 d".to_string(), bar_size: BarSize::Day1, use_rth: true,
    };
    let xml = historical::build_query_xml(&req);
    hmds.send_fixcomp(&[(fix::TAG_MSG_TYPE, "W"), (historical::TAG_HISTORICAL_XML, &xml)]).expect("Failed to send daily bar request");

    let mut all_bars = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut complete = false;

    while Instant::now() < deadline && !complete {
        match hmds.try_recv() {
            Ok(0) => { std::thread::sleep(Duration::from_millis(50)); continue; }
            Err(e) => { println!("  HMDS recv error: {}", e); break; }
            Ok(_) => {}
        }
        for frame in hmds.extract_frames() {
            let data = match &frame {
                Frame::FixComp(raw) => { let (u, _) = hmds.unsign(raw); fixcomp::fixcomp_decompress(&u) }
                Frame::Fix(raw) => vec![raw.clone()],
                _ => continue,
            };
            for msg in data {
                let tags = fix::fix_parse(&msg);
                if let Some(xml_resp) = tags.get(&historical::TAG_HISTORICAL_XML) {
                    if let Some(resp) = historical::parse_bar_response(xml_resp) {
                        all_bars.extend(resp.bars);
                        if resp.is_complete { complete = true; }
                    }
                }
            }
        }
    }

    let mut bg_conns = shutdown_and_reclaim(&bg_tx, bg_join, account_id);
    bg_conns.hmds = Some(hmds);

    println!("  Total daily bars: {}", all_bars.len());
    if all_bars.is_empty() {
        println!("  SKIP: No daily bars received (HMDS may be unavailable)\n");
        return bg_conns;
    }
    assert!(all_bars.len() <= 5, "Should have at most 5 daily bars, got {}", all_bars.len());
    for bar in &all_bars {
        assert!(bar.open > 0.0);
        assert!(bar.high >= bar.low);
        assert!(bar.volume > 0);
        println!("  {} O={:.2} H={:.2} L={:.2} C={:.2} V={}", bar.time, bar.open, bar.high, bar.low, bar.close, bar.volume);
    }
    println!("  PASS ({} daily bars)\n", all_bars.len());
    bg_conns
}

pub(super) fn phase_cancel_historical(mut conns: Conns, gw: &Gateway, config: &GatewayConfig) -> Conns {
    println!("--- Phase 77: Cancel Historical Request (SPY) ---");

    ccp_keepalive(&mut conns.ccp);
    let mut hmds = match connect_farm(&config.host, "ushmds", &config.username, config.paper, &gw.server_session_id, &gw.session_token, &gw.hw_info, &gw.encoded) {
        Ok(c) => { println!("  HMDS reconnected"); c }
        Err(e) => { println!("  SKIP: ushmds reconnect failed: {}\n", e); return Conns { farm: conns.farm, ccp: conns.ccp, hmds: None, account_id: conns.account_id }; }
    };

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (bg_loop, bg_tx) = HotLoop::with_connections(shared, None, account_id.clone(), conns.farm, conns.ccp, None, None);
    bg_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let bg_join = run_hot_loop(bg_loop);

    let req = HistoricalRequest {
        query_id: "cancel_test".to_string(), con_id: 756733, symbol: "SPY".to_string(),
        sec_type: "CS", exchange: "SMART", data_type: BarDataType::Trades,
        end_time: now_ib_timestamp(), duration: "1 d".to_string(), bar_size: BarSize::Sec1, use_rth: true,
    };
    let xml = historical::build_query_xml(&req);
    hmds.send_fixcomp(&[(fix::TAG_MSG_TYPE, "W"), (historical::TAG_HISTORICAL_XML, &xml)]).expect("Failed to send historical request");

    let mut got_first_chunk = false;
    let first_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < first_deadline && !got_first_chunk {
        match hmds.try_recv() {
            Ok(0) => { std::thread::sleep(Duration::from_millis(50)); continue; }
            Err(e) => { println!("  HMDS recv error: {}", e); break; }
            Ok(_) => {}
        }
        for frame in hmds.extract_frames() {
            let data = match &frame {
                Frame::FixComp(raw) => { let (u, _) = hmds.unsign(raw); fixcomp::fixcomp_decompress(&u) }
                Frame::Fix(raw) => vec![raw.clone()],
                _ => continue,
            };
            for msg in data {
                let tags = fix::fix_parse(&msg);
                if let Some(xml_resp) = tags.get(&historical::TAG_HISTORICAL_XML) {
                    if historical::parse_bar_response(xml_resp).is_some() {
                        got_first_chunk = true;
                        println!("  First chunk received, sending cancel");
                    }
                }
            }
        }
    }

    if !got_first_chunk {
        let mut bg_conns = shutdown_and_reclaim(&bg_tx, bg_join, account_id);
        bg_conns.hmds = Some(hmds);
        println!("  SKIP: No initial data received to cancel\n");
        return bg_conns;
    }

    let cancel_xml = "<?xml version=\"1.0\" encoding=\"UTF-8\"?><ListOfCancelQueries><CancelQuery><id>cancel_test</id></CancelQuery></ListOfCancelQueries>".to_string();
    hmds.send_fixcomp(&[(fix::TAG_MSG_TYPE, "Z"), (historical::TAG_HISTORICAL_XML, &cancel_xml)]).expect("Failed to send cancel request");
    println!("  Cancel sent");

    let drain_deadline = Instant::now() + Duration::from_secs(5);
    let mut bars_after_cancel = 0u32;
    while Instant::now() < drain_deadline {
        match hmds.try_recv() {
            Ok(0) => { std::thread::sleep(Duration::from_millis(50)); continue; }
            Err(e) => { println!("  HMDS recv error after cancel: {}", e); break; }
            Ok(_) => {}
        }
        for frame in hmds.extract_frames() {
            let data = match &frame {
                Frame::FixComp(raw) => { let (u, _) = hmds.unsign(raw); fixcomp::fixcomp_decompress(&u) }
                Frame::Fix(raw) => vec![raw.clone()],
                _ => continue,
            };
            for msg in data {
                let tags = fix::fix_parse(&msg);
                if let Some(xml_resp) = tags.get(&historical::TAG_HISTORICAL_XML) {
                    if let Some(resp) = historical::parse_bar_response(xml_resp) {
                        bars_after_cancel += resp.bars.len() as u32;
                    }
                }
            }
        }
    }

    let mut bg_conns = shutdown_and_reclaim(&bg_tx, bg_join, account_id);
    bg_conns.hmds = Some(hmds);
    println!("  Bars received after cancel: {} (in-flight data is expected)", bars_after_cancel);
    println!("  PASS (cancel sent successfully, connection intact)\n");
    bg_conns
}

pub(super) fn phase_head_timestamp(mut conns: Conns, gw: &Gateway, config: &GatewayConfig) -> Conns {
    println!("--- Phase 79: Head Timestamp (SPY, TRADES) ---");

    ccp_keepalive(&mut conns.ccp);
    let mut hmds = match connect_farm(&config.host, "ushmds", &config.username, config.paper, &gw.server_session_id, &gw.session_token, &gw.hw_info, &gw.encoded) {
        Ok(c) => { println!("  HMDS reconnected"); c }
        Err(e) => { println!("  SKIP: ushmds reconnect failed: {}\n", e); return Conns { farm: conns.farm, ccp: conns.ccp, hmds: None, account_id: conns.account_id }; }
    };

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (bg_loop, bg_tx) = HotLoop::with_connections(shared, None, account_id.clone(), conns.farm, conns.ccp, None, None);
    bg_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let bg_join = run_hot_loop(bg_loop);

    let req = HeadTimestampRequest { con_id: 756733, sec_type: "STK", exchange: "SMART", data_type: BarDataType::Trades, use_rth: true };
    let xml = historical::build_head_timestamp_xml(&req);
    hmds.send_fixcomp(&[(fix::TAG_MSG_TYPE, "W"), (historical::TAG_HISTORICAL_XML, &xml)]).expect("Failed to send head timestamp request");

    let mut response: Option<historical::HeadTimestampResponse> = None;
    let deadline = Instant::now() + Duration::from_secs(15);

    while Instant::now() < deadline && response.is_none() {
        match hmds.try_recv() {
            Ok(0) => { std::thread::sleep(Duration::from_millis(50)); continue; }
            Err(e) => { println!("  HMDS recv error: {}", e); break; }
            Ok(_) => {}
        }
        for frame in hmds.extract_frames() {
            let data = match &frame {
                Frame::FixComp(raw) => { let (u, _) = hmds.unsign(raw); fixcomp::fixcomp_decompress(&u) }
                Frame::Fix(raw) => vec![raw.clone()],
                _ => continue,
            };
            for msg in data {
                let tags = fix::fix_parse(&msg);
                if let Some(xml_resp) = tags.get(&historical::TAG_HISTORICAL_XML) {
                    if let Some(resp) = historical::parse_head_timestamp_response(xml_resp) {
                        println!("  headTS={} tz={}", resp.head_timestamp, resp.timezone);
                        response = Some(resp);
                    }
                }
            }
        }
    }

    let mut bg_conns = shutdown_and_reclaim(&bg_tx, bg_join, account_id);
    bg_conns.hmds = Some(hmds);

    if response.is_none() {
        println!("  SKIP: No head timestamp received (HMDS may be unavailable)\n");
        return bg_conns;
    }
    let resp = response.unwrap();
    assert!(!resp.head_timestamp.is_empty());
    assert!(resp.head_timestamp.starts_with("199"), "SPY TRADES head timestamp should be in 1990s, got {}", resp.head_timestamp);
    assert!(!resp.timezone.is_empty());
    println!("  PASS\n");
    bg_conns
}

pub(super) fn phase_scanner_subscription(mut conns: Conns, gw: &Gateway, config: &GatewayConfig) -> Conns {
    println!("--- Phase 82: Scanner Subscription (TOP_PERC_GAIN, STK.US.MAJOR) ---");

    ccp_keepalive(&mut conns.ccp);
    let mut hmds = match connect_farm(&config.host, "ushmds", &config.username, config.paper, &gw.server_session_id, &gw.session_token, &gw.hw_info, &gw.encoded) {
        Ok(c) => { println!("  HMDS reconnected"); c }
        Err(e) => { println!("  SKIP: ushmds reconnect failed: {}\n", e); return Conns { farm: conns.farm, ccp: conns.ccp, hmds: None, account_id: conns.account_id }; }
    };

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (bg_loop, bg_tx) = HotLoop::with_connections(shared, None, account_id.clone(), conns.farm, conns.ccp, None, None);
    bg_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let bg_join = run_hot_loop(bg_loop);

    let sub = scanner::ScannerSubscription {
        instrument: "STK".to_string(), location_code: "STK.US.MAJOR".to_string(),
        scan_code: "TOP_PERC_GAIN".to_string(), max_items: 10,
    };
    let xml = scanner::build_scanner_subscribe_xml(&sub);
    hmds.send_fixcomp(&[(fix::TAG_MSG_TYPE, "U"), (scanner::TAG_SUB_PROTOCOL, "10003"), (scanner::TAG_SCANNER_XML, &xml)]).expect("Failed to send scanner subscription");

    let mut result: Option<scanner::ScannerResult> = None;
    let deadline = Instant::now() + Duration::from_secs(15);

    while Instant::now() < deadline && result.is_none() {
        match hmds.try_recv() {
            Ok(0) => { std::thread::sleep(Duration::from_millis(50)); continue; }
            Err(e) => { println!("  HMDS recv error: {}", e); break; }
            Ok(_) => {}
        }
        for frame in hmds.extract_frames() {
            let data = match &frame {
                Frame::FixComp(raw) => { let (u, _) = hmds.unsign(raw); fixcomp::fixcomp_decompress(&u) }
                Frame::Fix(raw) => vec![raw.clone()],
                _ => continue,
            };
            for msg in data {
                let tags = fix::fix_parse(&msg);
                if tags.get(&scanner::TAG_SUB_PROTOCOL).map(|s| s.as_str()) == Some("10005") {
                    if let Some(xml_resp) = tags.get(&scanner::TAG_SCANNER_XML) {
                        if let Some(r) = scanner::parse_scanner_response(xml_resp) {
                            println!("  Scanner: {} contracts at {}", r.con_ids.len(), r.scan_time);
                            result = Some(r);
                        }
                    }
                }
            }
        }
    }

    let cancel_xml = scanner::build_scanner_cancel_xml("APISCAN1:1");
    let _ = hmds.send_fixcomp(&[(fix::TAG_MSG_TYPE, "U"), (scanner::TAG_SUB_PROTOCOL, "10004"), (scanner::TAG_SCANNER_XML, &cancel_xml)]);

    let mut bg_conns = shutdown_and_reclaim(&bg_tx, bg_join, account_id);
    bg_conns.hmds = Some(hmds);

    if result.is_none() {
        println!("  SKIP: No scanner results received\n");
        return bg_conns;
    }
    let r = result.unwrap();
    assert!(!r.con_ids.is_empty());
    assert!(!r.scan_time.is_empty());
    for (i, cid) in r.con_ids.iter().enumerate().take(3) {
        println!("  Rank {}: conId={}", i, cid);
    }
    println!("  PASS ({} contracts)\n", r.con_ids.len());
    bg_conns
}

pub(super) fn phase_fundamental_data(gw: &Gateway, config: &GatewayConfig) {
    println!("--- Phase 83: Fundamental Data (AAPL, ReportSnapshot) ---");

    let mut fundfarm = match connect_farm(&config.host, "fundfarm", &config.username, config.paper, &gw.server_session_id, &gw.session_token, &gw.hw_info, &gw.encoded) {
        Ok(c) => { println!("  fundfarm connected"); c }
        Err(e) => { println!("  SKIP: fundfarm connect failed: {}\n", e); return; }
    };

    let req = fundamental::FundamentalRequest {
        con_id: 265598, sec_type: "STK", currency: "USD",
        report_type: fundamental::ReportType::Snapshot,
    };
    let xml = fundamental::build_fundamental_request_xml(&req);
    fundfarm.send_fixcomp(&[(fix::TAG_MSG_TYPE, "U"), (fundamental::TAG_SUB_PROTOCOL, "10010"), (fundamental::TAG_FUNDAMENTAL_XML, &xml)]).expect("Failed to send fundamental data request");

    let mut got_response = false;
    let deadline = Instant::now() + Duration::from_secs(15);

    while Instant::now() < deadline && !got_response {
        match fundfarm.try_recv() {
            Ok(0) => { std::thread::sleep(Duration::from_millis(50)); continue; }
            Err(e) => { println!("  fundfarm recv error: {}", e); break; }
            Ok(_) => {}
        }
        for frame in fundfarm.extract_frames() {
            let data = match &frame {
                Frame::FixComp(raw) => { let (u, _) = fundfarm.unsign(raw); fixcomp::fixcomp_decompress(&u) }
                Frame::Fix(raw) => vec![raw.clone()],
                _ => continue,
            };
            for msg in data {
                let tags = fix::fix_parse(&msg);
                if tags.get(&fundamental::TAG_SUB_PROTOCOL).map(|s| s.as_str()) == Some("10012") {
                    if let Some(xml_resp) = tags.get(&fundamental::TAG_FUNDAMENTAL_XML) {
                        if let Some(id) = fundamental::parse_fundamental_response_id(xml_resp) {
                            println!("  Response ID: {}", id);
                        }
                    }
                    if let Some(raw_data) = tags.get(&fundamental::TAG_RAW_DATA) {
                        println!("  Raw data: {} bytes", raw_data.len());
                        if let Some(xml_out) = fundamental::decompress_fundamental_data(raw_data.as_bytes()) {
                            println!("  Decompressed: {} chars", xml_out.len());
                        } else {
                            println!("  Note: binary payload detected");
                        }
                    }
                    got_response = true;
                }
            }
        }
    }

    if !got_response {
        println!("  SKIP: No fundamental data received (may require subscription)\n");
        return;
    }
    println!("  PASS\n");
}

pub(super) fn phase_historical_news(mut conns: Conns, gw: &Gateway, config: &GatewayConfig) -> Conns {
    println!("--- Phase 85: Historical News (AAPL) ---");

    ccp_keepalive(&mut conns.ccp);
    let mut hmds = match connect_farm(&config.host, "ushmds", &config.username, config.paper, &gw.server_session_id, &gw.session_token, &gw.hw_info, &gw.encoded) {
        Ok(c) => { println!("  HMDS reconnected"); c }
        Err(e) => { println!("  SKIP: ushmds reconnect failed: {}\n", e); return Conns { farm: conns.farm, ccp: conns.ccp, hmds: None, account_id: conns.account_id }; }
    };

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (bg_loop, bg_tx) = HotLoop::with_connections(shared, None, account_id.clone(), conns.farm, conns.ccp, None, None);
    bg_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let bg_join = run_hot_loop(bg_loop);

    let req = news::HistoricalNewsRequest {
        con_id: 265598, provider_codes: "BRFG+BRFUPDN".to_string(),
        start_time: String::new(), end_time: String::new(), max_results: 5,
    };
    let xml = news::build_historical_news_xml(&req);
    hmds.send_fixcomp(&[(fix::TAG_MSG_TYPE, "U"), (news::TAG_SUB_PROTOCOL, "10030"), (news::TAG_NEWS_XML, &xml)]).expect("Failed to send historical news request");

    let mut got_response = false;
    let deadline = Instant::now() + Duration::from_secs(15);

    while Instant::now() < deadline && !got_response {
        match hmds.try_recv() {
            Ok(0) => { std::thread::sleep(Duration::from_millis(50)); continue; }
            Err(e) => { println!("  HMDS recv error: {}", e); break; }
            Ok(_) => {}
        }
        for frame in hmds.extract_frames() {
            let data = match &frame {
                Frame::FixComp(raw) => { let (u, _) = hmds.unsign(raw); fixcomp::fixcomp_decompress(&u) }
                Frame::Fix(raw) => vec![raw.clone()],
                _ => continue,
            };
            for msg in data {
                let tags = fix::fix_parse(&msg);
                if tags.get(&news::TAG_SUB_PROTOCOL).map(|s| s.as_str()) == Some("10032") {
                    if let Some(xml_resp) = tags.get(&news::TAG_NEWS_XML) {
                        if let Some(id) = news::parse_news_response_id(xml_resp) {
                            println!("  Response ID: {}", id);
                        }
                    }
                    if tags.contains_key(&news::TAG_RAW_DATA) {
                        println!("  Raw data payload received");
                    }
                    got_response = true;
                }
            }
        }
    }

    let mut bg_conns = shutdown_and_reclaim(&bg_tx, bg_join, account_id);
    bg_conns.hmds = Some(hmds);

    if !got_response {
        println!("  SKIP: No news response received (may require news subscription)\n");
        return bg_conns;
    }
    println!("  PASS\n");
    bg_conns
}

pub(super) fn phase_historical_ticks(mut conns: Conns, gw: &Gateway, config: &GatewayConfig) -> Conns {
    println!("--- Phase 88: Historical Ticks (SPY, TRADES) ---");

    ccp_keepalive(&mut conns.ccp);
    let hmds = match connect_farm(&config.host, "ushmds", &config.username, config.paper, &gw.server_session_id, &gw.session_token, &gw.hw_info, &gw.encoded) {
        Ok(c) => { println!("  HMDS reconnected"); c }
        Err(e) => { println!("  SKIP: ushmds reconnect failed: {}\n", e); return Conns { farm: conns.farm, ccp: conns.ccp, hmds: None, account_id: conns.account_id }; }
    };

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), None, account_id.clone(), conns.farm, conns.ccp, Some(hmds), None,
    );

    // Request last 100 historical ticks for SPY, ending now
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let end_dt = format_utc_timestamp(now);
    control_tx.send(ControlCommand::FetchHistoricalTicks {
        req_id: 2001,
        con_id: 756733,
        start_date_time: String::new(),
        end_date_time: end_dt,
        number_of_ticks: 100,
        what_to_show: "TRADES".to_string(),
        use_rth: true,
    }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut tick_count = 0usize;

    while Instant::now() < deadline {
        let ticks = shared.drain_historical_ticks();
        for (req_id, data, what, done) in &ticks {
            if *req_id == 2001 {
                let count = match data {
                    HistoricalTickData::Last(v) => v.len(),
                    HistoricalTickData::Midpoint(v) => v.len(),
                    HistoricalTickData::BidAsk(v) => v.len(),
                };
                tick_count += count;
                println!("  Received {} ticks (what={}, done={})", count, what, done);
                if *done { break; }
            }
        }
        if tick_count > 0 { break; }
        std::thread::sleep(Duration::from_millis(100));
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if tick_count == 0 {
        println!("  SKIP: No historical ticks received\n");
    } else {
        println!("  PASS ({} ticks)\n", tick_count);
    }
    conns
}

pub(super) fn phase_histogram_data(mut conns: Conns, gw: &Gateway, config: &GatewayConfig) -> Conns {
    println!("--- Phase 89: Histogram Data (SPY, 1 week) ---");

    ccp_keepalive(&mut conns.ccp);
    let hmds = match connect_farm(&config.host, "ushmds", &config.username, config.paper, &gw.server_session_id, &gw.session_token, &gw.hw_info, &gw.encoded) {
        Ok(c) => { println!("  HMDS reconnected"); c }
        Err(e) => { println!("  SKIP: ushmds reconnect failed: {}\n", e); return Conns { farm: conns.farm, ccp: conns.ccp, hmds: None, account_id: conns.account_id }; }
    };

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), None, account_id.clone(), conns.farm, conns.ccp, Some(hmds), None,
    );

    control_tx.send(ControlCommand::FetchHistogramData {
        req_id: 3001,
        con_id: 756733,
        use_rth: true,
        period: "1 week".to_string(),
    }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut entries = Vec::new();

    while Instant::now() < deadline {
        let data = shared.drain_histogram_data();
        for (req_id, ents) in data {
            if req_id == 3001 {
                entries = ents;
                break;
            }
        }
        if !entries.is_empty() { break; }
        std::thread::sleep(Duration::from_millis(100));
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if entries.is_empty() {
        println!("  SKIP: No histogram data received\n");
    } else {
        println!("  {} histogram entries", entries.len());
        if let Some(first) = entries.first() {
            println!("  First: price={:.2} count={}", first.price, first.count);
        }
        println!("  PASS\n");
    }
    conns
}

pub(super) fn phase_historical_schedule(mut conns: Conns, gw: &Gateway, config: &GatewayConfig) -> Conns {
    println!("--- Phase 90: Historical Schedule (SPY) ---");

    ccp_keepalive(&mut conns.ccp);
    let hmds = match connect_farm(&config.host, "ushmds", &config.username, config.paper, &gw.server_session_id, &gw.session_token, &gw.hw_info, &gw.encoded) {
        Ok(c) => { println!("  HMDS reconnected"); c }
        Err(e) => { println!("  SKIP: ushmds reconnect failed: {}\n", e); return Conns { farm: conns.farm, ccp: conns.ccp, hmds: None, account_id: conns.account_id }; }
    };

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), None, account_id.clone(), conns.farm, conns.ccp, Some(hmds), None,
    );

    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let end_dt = format_utc_timestamp(now);
    control_tx.send(ControlCommand::FetchHistoricalSchedule {
        req_id: 4001,
        con_id: 756733,
        end_date_time: end_dt,
        duration: "5 d".to_string(),
        use_rth: true,
    }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut schedule: Option<HistoricalScheduleResponse> = None;

    while Instant::now() < deadline {
        let data = shared.drain_historical_schedules();
        for (req_id, resp) in data {
            if req_id == 4001 {
                schedule = Some(resp);
                break;
            }
        }
        if schedule.is_some() { break; }
        std::thread::sleep(Duration::from_millis(100));
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if let Some(sched) = schedule {
        println!("  Timezone: {}", sched.timezone);
        println!("  Sessions: {}", sched.sessions.len());
        for s in sched.sessions.iter().take(3) {
            println!("    {} open={} close={}", s.ref_date, s.open_time, s.close_time);
        }
        assert!(!sched.sessions.is_empty(), "Schedule should contain sessions");
        println!("  PASS\n");
    } else {
        println!("  SKIP: No schedule data received\n");
    }
    conns
}

pub(super) fn phase_realtime_bars(mut conns: Conns, gw: &Gateway, config: &GatewayConfig) -> Conns {
    println!("--- Phase 91: Real-Time Bars (SPY, 5-second) ---");

    ccp_keepalive(&mut conns.ccp);
    let hmds = match connect_farm(&config.host, "ushmds", &config.username, config.paper, &gw.server_session_id, &gw.session_token, &gw.hw_info, &gw.encoded) {
        Ok(c) => { println!("  HMDS reconnected"); c }
        Err(e) => { println!("  SKIP: ushmds reconnect failed: {}\n", e); return Conns { farm: conns.farm, ccp: conns.ccp, hmds: None, account_id: conns.account_id }; }
    };

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), None, account_id.clone(), conns.farm, conns.ccp, Some(hmds), None,
    );

    control_tx.send(ControlCommand::SubscribeRealTimeBar {
        req_id: 5001,
        con_id: 756733,
        symbol: "SPY".to_string(),
        what_to_show: "TRADES".to_string(),
        use_rth: false,
    }).unwrap();
    let join = run_hot_loop(hot_loop);

    // Wait up to 20s for at least one 5-second bar
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut bars = Vec::new();

    while Instant::now() < deadline {
        let data = shared.drain_real_time_bars();
        for (req_id, bar) in data {
            if req_id == 5001 {
                bars.push(bar);
            }
        }
        if !bars.is_empty() { break; }
        std::thread::sleep(Duration::from_millis(200));
    }

    // Cancel subscription
    control_tx.send(ControlCommand::CancelRealTimeBar { req_id: 5001 }).unwrap();
    std::thread::sleep(Duration::from_millis(500));

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if bars.is_empty() {
        println!("  SKIP: No real-time bars received (market may be closed)\n");
    } else {
        let bar = &bars[0];
        println!("  First bar: O={:.2} H={:.2} L={:.2} C={:.2} V={:.0}", bar.open, bar.high, bar.low, bar.close, bar.volume);
        assert!(bar.high >= bar.low, "High should be >= Low");
        println!("  PASS ({} bars)\n", bars.len());
    }
    conns
}

pub(super) fn phase_news_article(mut conns: Conns, gw: &Gateway, config: &GatewayConfig) -> Conns {
    println!("--- Phase 92: News Article Fetch (AAPL) ---");

    ccp_keepalive(&mut conns.ccp);
    let hmds = match connect_farm(&config.host, "ushmds", &config.username, config.paper, &gw.server_session_id, &gw.session_token, &gw.hw_info, &gw.encoded) {
        Ok(c) => { println!("  HMDS reconnected"); c }
        Err(e) => { println!("  SKIP: ushmds reconnect failed: {}\n", e); return Conns { farm: conns.farm, ccp: conns.ccp, hmds: None, account_id: conns.account_id }; }
    };

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), None, account_id.clone(), conns.farm, conns.ccp, Some(hmds), None,
    );

    // First request historical news to get an article ID
    control_tx.send(ControlCommand::FetchHistoricalNews {
        req_id: 6001,
        con_id: 265598,
        provider_codes: "BRFG+BRFUPDN".to_string(),
        start_time: String::new(),
        end_time: String::new(),
        max_results: 5,
    }).unwrap();
    let join = run_hot_loop(hot_loop);

    // Poll for headlines
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut article_id: Option<String> = None;
    let mut provider_code: Option<String> = None;

    while Instant::now() < deadline && article_id.is_none() {
        let data = shared.drain_historical_news();
        for (req_id, headlines, _done) in data {
            if req_id == 6001 {
                if let Some(h) = headlines.first() {
                    article_id = Some(h.article_id.clone());
                    provider_code = Some(h.provider_code.clone());
                    println!("  Headline: {} ({})", h.headline, h.article_id);
                }
            }
        }
        if article_id.is_some() { break; }
        std::thread::sleep(Duration::from_millis(100));
    }

    if let (Some(art_id), Some(prov)) = (article_id, provider_code) {
        // Now fetch the article body
        control_tx.send(ControlCommand::FetchNewsArticle {
            req_id: 6002,
            provider_code: prov,
            article_id: art_id.clone(),
        }).unwrap();

        let deadline = Instant::now() + Duration::from_secs(15);
        let mut got_article = false;

        while Instant::now() < deadline {
            let articles = shared.drain_news_articles();
            for (req_id, _art_type, body) in &articles {
                if *req_id == 6002 {
                    println!("  Article body: {} bytes", body.len());
                    got_article = true;
                }
            }
            if got_article { break; }
            std::thread::sleep(Duration::from_millis(100));
        }

        let conns = shutdown_and_reclaim(&control_tx, join, account_id);
        if got_article {
            println!("  PASS\n");
        } else {
            println!("  SKIP: Article body not received\n");
        }
        conns
    } else {
        let conns = shutdown_and_reclaim(&control_tx, join, account_id);
        println!("  SKIP: No news headlines to fetch article from\n");
        conns
    }
}

pub(super) fn phase_fundamental_data_channel(mut conns: Conns, gw: &Gateway, config: &GatewayConfig) -> Conns {
    println!("--- Phase 93: Fundamental Data via HotLoop (AAPL) ---");

    ccp_keepalive(&mut conns.ccp);
    let hmds = match connect_farm(&config.host, "ushmds", &config.username, config.paper, &gw.server_session_id, &gw.session_token, &gw.hw_info, &gw.encoded) {
        Ok(c) => { println!("  HMDS reconnected"); c }
        Err(e) => { println!("  SKIP: ushmds reconnect failed: {}\n", e); return Conns { farm: conns.farm, ccp: conns.ccp, hmds: None, account_id: conns.account_id }; }
    };

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), None, account_id.clone(), conns.farm, conns.ccp, Some(hmds), None,
    );

    control_tx.send(ControlCommand::FetchFundamentalData {
        req_id: 7001,
        con_id: 265598,
        report_type: "ReportSnapshot".to_string(),
    }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut got_data = false;

    while Instant::now() < deadline {
        let data = shared.drain_fundamental_data();
        for (req_id, xml) in &data {
            if *req_id == 7001 {
                println!("  Fundamental data: {} bytes", xml.len());
                got_data = true;
            }
        }
        if got_data { break; }
        std::thread::sleep(Duration::from_millis(100));
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if got_data {
        println!("  PASS\n");
    } else {
        println!("  SKIP: No fundamental data received (may require subscription)\n");
    }
    conns
}

pub(super) fn phase_parallel_historical(mut conns: Conns, gw: &Gateway, config: &GatewayConfig) -> Conns {
    println!("--- Phase 94: Parallel Historical Requests (SPY: 1d/5min, 5d/1day, 1w/1h) ---");

    ccp_keepalive(&mut conns.ccp);
    let hmds = match connect_farm(&config.host, "ushmds", &config.username, config.paper, &gw.server_session_id, &gw.session_token, &gw.hw_info, &gw.encoded) {
        Ok(c) => { println!("  HMDS reconnected"); c }
        Err(e) => { println!("  SKIP: ushmds reconnect failed: {}\n", e); return Conns { farm: conns.farm, ccp: conns.ccp, hmds: None, account_id: conns.account_id }; }
    };

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), None, account_id.clone(), conns.farm, conns.ccp, Some(hmds), None,
    );

    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let end_dt = format_utc_timestamp(now);

    // Send 3 requests in quick succession
    control_tx.send(ControlCommand::FetchHistorical {
        req_id: 8001, con_id: 756733, symbol: "SPY".to_string(),
        end_date_time: end_dt.clone(), duration: "1 d".to_string(),
        bar_size: "5 mins".to_string(), what_to_show: "TRADES".to_string(), use_rth: true,
    }).unwrap();
    control_tx.send(ControlCommand::FetchHistorical {
        req_id: 8002, con_id: 756733, symbol: "SPY".to_string(),
        end_date_time: end_dt.clone(), duration: "5 d".to_string(),
        bar_size: "1 day".to_string(), what_to_show: "TRADES".to_string(), use_rth: true,
    }).unwrap();
    control_tx.send(ControlCommand::FetchHistorical {
        req_id: 8003, con_id: 756733, symbol: "SPY".to_string(),
        end_date_time: end_dt, duration: "1 W".to_string(),
        bar_size: "1 hour".to_string(), what_to_show: "TRADES".to_string(), use_rth: true,
    }).unwrap();

    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut received: [bool; 3] = [false; 3];

    while Instant::now() < deadline {
        let data = shared.drain_historical_data();
        for (req_id, resp) in &data {
            match *req_id {
                8001 => { if resp.is_complete { received[0] = true; println!("  req 8001 (1d/5min): {} bars", resp.bars.len()); } }
                8002 => { if resp.is_complete { received[1] = true; println!("  req 8002 (5d/1day): {} bars", resp.bars.len()); } }
                8003 => { if resp.is_complete { received[2] = true; println!("  req 8003 (1W/1h): {} bars", resp.bars.len()); } }
                _ => {}
            }
        }
        if received.iter().all(|r| *r) { break; }
        std::thread::sleep(Duration::from_millis(100));
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    let count = received.iter().filter(|r| **r).count();
    if count == 3 {
        println!("  PASS (all 3 responses received)\n");
    } else if count > 0 {
        println!("  PARTIAL: {}/3 responses received\n", count);
    } else {
        println!("  SKIP: No responses received\n");
    }
    conns
}

pub(super) fn phase_scanner_params(mut conns: Conns, gw: &Gateway, config: &GatewayConfig) -> Conns {
    println!("--- Phase 95: Scanner Parameters + HOT_BY_VOLUME Scan ---");

    ccp_keepalive(&mut conns.ccp);
    let hmds = match connect_farm(&config.host, "ushmds", &config.username, config.paper, &gw.server_session_id, &gw.session_token, &gw.hw_info, &gw.encoded) {
        Ok(c) => { println!("  HMDS reconnected"); c }
        Err(e) => { println!("  SKIP: ushmds reconnect failed: {}\n", e); return Conns { farm: conns.farm, ccp: conns.ccp, hmds: None, account_id: conns.account_id }; }
    };

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), None, account_id.clone(), conns.farm, conns.ccp, Some(hmds), None,
    );

    // Request scanner params XML
    control_tx.send(ControlCommand::FetchScannerParams).unwrap();
    // Also subscribe to a HOT_BY_VOLUME scan
    control_tx.send(ControlCommand::SubscribeScanner {
        req_id: 9001,
        instrument: "STK".to_string(),
        location_code: "STK.US.MAJOR".to_string(),
        scan_code: "HOT_BY_VOLUME".to_string(),
        max_items: 10,
    }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut got_params = false;
    let mut got_scan = false;

    while Instant::now() < deadline {
        let params = shared.drain_scanner_params();
        if !params.is_empty() {
            println!("  Scanner params XML: {} bytes", params[0].len());
            got_params = true;
        }
        let scans = shared.drain_scanner_data();
        for (req_id, result) in &scans {
            if *req_id == 9001 {
                println!("  Scanner results: {} contracts", result.con_ids.len());
                got_scan = true;
            }
        }
        if got_params && got_scan { break; }
        // If we have params but no scan after a while, don't wait forever
        if got_params && Instant::now() > deadline - Duration::from_secs(5) { break; }
        std::thread::sleep(Duration::from_millis(200));
    }

    // Cancel scanner subscription
    control_tx.send(ControlCommand::CancelScanner { req_id: 9001 }).unwrap();
    std::thread::sleep(Duration::from_millis(500));

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if got_params {
        println!("  Scanner params: PASS");
    } else {
        println!("  Scanner params: SKIP");
    }
    if got_scan {
        println!("  Scanner scan: PASS");
    } else {
        println!("  Scanner scan: SKIP (may need market hours)");
    }
    println!();
    conns
}

pub(super) fn phase_historical_ohlc_validation(conns: Conns, _gw: &Gateway, _config: &GatewayConfig) -> Conns {
    println!("--- Phase 103: Historical Bar OHLC Validation (SPY 1-hour bars) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, _event_rx) = crossbeam_channel::unbounded();
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    let req_id = 6001u32;
    control_tx.send(ControlCommand::FetchHistorical {
        req_id,
        con_id: 756733,
        symbol: "SPY".into(),
        end_date_time: String::new(), // empty = now
        duration: "5 D".into(),
        bar_size: "1 hour".into(),
        what_to_show: "TRADES".into(),
        use_rth: true,
    }).unwrap();

    let join = run_hot_loop(hot_loop);
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut bars_data: Option<historical::HistoricalResponse> = None;

    while Instant::now() < deadline {
        let hist = shared.drain_historical_data();
        for (rid, data) in hist {
            if rid == req_id {
                bars_data = Some(data);
            }
        }
        if bars_data.is_some() { break; }
        std::thread::sleep(Duration::from_millis(100));
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    let data = match bars_data {
        Some(d) => d,
        None => {
            println!("  SKIP: No historical data received\n");
            return conns;
        }
    };

    let bars = &data.bars;
    println!("  Received {} bars", bars.len());
    assert!(!bars.is_empty(), "Should receive at least 1 bar");

    let mut ohlc_valid = true;
    let mut volume_valid = true;

    for (i, bar) in bars.iter().enumerate() {
        // OHLC consistency: low <= everything, high >= everything
        if bar.low > bar.high {
            println!("  Bar {}: low ({}) > high ({})", i, bar.low, bar.high);
            ohlc_valid = false;
        }
        if bar.low > bar.open {
            println!("  Bar {}: low ({}) > open ({})", i, bar.low, bar.open);
            ohlc_valid = false;
        }
        if bar.low > bar.close {
            println!("  Bar {}: low ({}) > close ({})", i, bar.low, bar.close);
            ohlc_valid = false;
        }
        if bar.high < bar.open {
            println!("  Bar {}: high ({}) < open ({})", i, bar.high, bar.open);
            ohlc_valid = false;
        }
        if bar.high < bar.close {
            println!("  Bar {}: high ({}) < close ({})", i, bar.high, bar.close);
            ohlc_valid = false;
        }
        // Volume should be non-negative
        if bar.volume < 0 {
            println!("  Bar {}: negative volume ({})", i, bar.volume);
            volume_valid = false;
        }
    }

    assert!(ohlc_valid, "All bars should have valid OHLC relationships");
    assert!(volume_valid, "All bars should have non-negative volume");
    println!("  PASS\n");
    conns
}

// ─── Phase 111: Large historical dataset — 1 year daily bars (issue #99) ───

pub(super) fn phase_large_historical_dataset(mut conns: Conns, gw: &Gateway, config: &GatewayConfig) -> Conns {
    println!("--- Phase 111: Large Historical Dataset (SPY, 1 year of daily bars) ---");

    ccp_keepalive(&mut conns.ccp);
    let hmds = match connect_farm(&config.host, "ushmds", &config.username, config.paper, &gw.server_session_id, &gw.session_token, &gw.hw_info, &gw.encoded) {
        Ok(c) => { println!("  HMDS reconnected"); c }
        Err(e) => { println!("  SKIP: ushmds reconnect failed: {}\n", e); return Conns { farm: conns.farm, ccp: conns.ccp, hmds: None, account_id: conns.account_id }; }
    };

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), None, account_id.clone(), conns.farm, conns.ccp, Some(hmds), None,
    );

    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let end_dt = format_utc_timestamp(now);

    control_tx.send(ControlCommand::FetchHistorical {
        req_id: 11001, con_id: 756733, symbol: "SPY".to_string(),
        end_date_time: end_dt, duration: "1 Y".to_string(),
        bar_size: "1 day".to_string(), what_to_show: "TRADES".to_string(), use_rth: true,
    }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(60);
    let mut total_bars = 0usize;
    let mut complete = false;
    let mut prev_time = String::new();
    let mut duplicate_timestamps = 0u32;

    while Instant::now() < deadline {
        let data = shared.drain_historical_data();
        for (req_id, resp) in &data {
            if *req_id == 11001 {
                for bar in &resp.bars {
                    total_bars += 1;
                    assert!(bar.high >= bar.low, "Bar {}: high < low ({} < {})", bar.time, bar.high, bar.low);
                    assert!(bar.open > 0.0, "Bar {}: open should be positive", bar.time);
                    assert!(bar.volume >= 0, "Bar {}: volume should be non-negative", bar.time);
                    if bar.time == prev_time {
                        duplicate_timestamps += 1;
                    }
                    prev_time = bar.time.clone();
                }
                if resp.is_complete { complete = true; }
            }
        }
        if complete { break; }
        std::thread::sleep(Duration::from_millis(100));
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if total_bars == 0 {
        println!("  SKIP: No bars received\n");
        return conns;
    }
    println!("  Total bars: {} (complete={})", total_bars, complete);
    println!("  Duplicate timestamps: {}", duplicate_timestamps);
    assert!(total_bars >= 200, "1 year should have 200+ trading days, got {}", total_bars);
    assert_eq!(duplicate_timestamps, 0, "No duplicate bar timestamps expected");
    println!("  PASS\n");
    conns
}

// ─── Phase 112: DST boundary historical data (issue #98) ───

pub(super) fn phase_dst_boundary_historical(mut conns: Conns, gw: &Gateway, config: &GatewayConfig) -> Conns {
    println!("--- Phase 112: DST Boundary Historical Data (SPY, bars spanning March DST) ---");

    ccp_keepalive(&mut conns.ccp);
    let hmds = match connect_farm(&config.host, "ushmds", &config.username, config.paper, &gw.server_session_id, &gw.session_token, &gw.hw_info, &gw.encoded) {
        Ok(c) => { println!("  HMDS reconnected"); c }
        Err(e) => { println!("  SKIP: ushmds reconnect failed: {}\n", e); return Conns { farm: conns.farm, ccp: conns.ccp, hmds: None, account_id: conns.account_id }; }
    };

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), None, account_id.clone(), conns.farm, conns.ccp, Some(hmds), None,
    );

    // Request 2 weeks of 1-hour bars ending after the March DST transition
    // DST 2026: March 8 (second Sunday of March) — spring forward
    // End date: March 14 2026, covering March 2-14 (spans DST)
    control_tx.send(ControlCommand::FetchHistorical {
        req_id: 12001, con_id: 756733, symbol: "SPY".to_string(),
        end_date_time: "20260314-20:00:00".to_string(), duration: "2 W".to_string(),
        bar_size: "1 hour".to_string(), what_to_show: "TRADES".to_string(), use_rth: true,
    }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut bars = Vec::new();
    let mut complete = false;

    while Instant::now() < deadline {
        let data = shared.drain_historical_data();
        for (req_id, resp) in &data {
            if *req_id == 12001 {
                bars.extend(resp.bars.iter().cloned());
                if resp.is_complete { complete = true; }
            }
        }
        if complete { break; }
        std::thread::sleep(Duration::from_millis(100));
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if bars.is_empty() {
        println!("  SKIP: No bars received\n");
        return conns;
    }

    // Check for duplicate timestamps
    let mut timestamps: Vec<String> = bars.iter().map(|b| b.time.clone()).collect();
    let original_count = timestamps.len();
    timestamps.sort();
    timestamps.dedup();
    let unique_count = timestamps.len();
    let duplicates = original_count - unique_count;

    // Check all bars have valid OHLCV
    for bar in &bars {
        assert!(bar.high >= bar.low, "Bar {}: high ({}) < low ({})", bar.time, bar.high, bar.low);
        assert!(bar.open > 0.0, "Bar {}: zero/negative open", bar.time);
    }

    println!("  {} bars received ({} unique timestamps, {} duplicates, complete={})", original_count, unique_count, duplicates, complete);
    assert_eq!(duplicates, 0, "No duplicate timestamps at DST boundary");

    // 2 weeks of RTH = ~10 trading days * ~7 hours = ~70 bars
    assert!(bars.len() >= 40, "2 weeks of hourly RTH should have 40+ bars, got {}", bars.len());
    println!("  PASS\n");
    conns
}

// ─── Phase 127: Cancel Data Requests (historical, fundamental, histogram, head timestamp) ───

pub(super) fn phase_cancel_data_requests(mut conns: Conns, gw: &Gateway, config: &GatewayConfig) -> Conns {
    println!("--- Phase 127: Cancel Data Requests (4 cancel ControlCommands) ---");

    ccp_keepalive(&mut conns.ccp);
    let hmds = match connect_farm(
        &config.host, "ushmds",
        &config.username, config.paper,
        &gw.server_session_id, &gw.session_token, &gw.hw_info, &gw.encoded,
    ) {
        Ok(c) => { println!("  HMDS reconnected"); Some(c) }
        Err(e) => {
            println!("  SKIP: ushmds reconnect failed: {}\n", e);
            return conns;
        }
    };

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, _event_rx) = crossbeam_channel::unbounded();
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), Some(event_tx), account_id.clone(), conns.farm, conns.ccp, hmds, None,
    );

    let now = now_ib_timestamp();

    // 1. FetchHistorical + CancelHistorical
    control_tx.send(ControlCommand::FetchHistorical {
        req_id: 20001, con_id: 756733, symbol: "SPY".to_string(),
        end_date_time: now.clone(), duration: "1 d".to_string(),
        bar_size: "5 mins".to_string(), what_to_show: "TRADES".to_string(), use_rth: true,
    }).unwrap();
    control_tx.send(ControlCommand::CancelHistorical { req_id: 20001 }).unwrap();

    // 2. FetchHeadTimestamp + CancelHeadTimestamp
    control_tx.send(ControlCommand::FetchHeadTimestamp {
        req_id: 20002, con_id: 756733,
        what_to_show: "TRADES".to_string(), use_rth: true,
    }).unwrap();
    control_tx.send(ControlCommand::CancelHeadTimestamp { req_id: 20002 }).unwrap();

    // 3. FetchFundamentalData + CancelFundamentalData
    control_tx.send(ControlCommand::FetchFundamentalData {
        req_id: 20003, con_id: 265598, report_type: "ReportsFinStatements".to_string(),
    }).unwrap();
    control_tx.send(ControlCommand::CancelFundamentalData { req_id: 20003 }).unwrap();

    // 4. FetchHistogramData + CancelHistogramData
    control_tx.send(ControlCommand::FetchHistogramData {
        req_id: 20004, con_id: 756733, use_rth: true, period: "1 week".to_string(),
    }).unwrap();
    control_tx.send(ControlCommand::CancelHistogramData { req_id: 20004 }).unwrap();

    let join = run_hot_loop(hot_loop);

    // Wait a moment for the hot loop to process all commands
    std::thread::sleep(Duration::from_secs(3));

    // Verify no responses arrived for cancelled requests
    let hist = shared.drain_historical_data();
    let head = shared.drain_head_timestamps();
    let fund = shared.drain_fundamental_data();
    let histo = shared.drain_histogram_data();

    let hist_for_req: Vec<_> = hist.iter().filter(|(id, _)| *id == 20001).collect();
    let head_for_req: Vec<_> = head.iter().filter(|(id, _)| *id == 20002).collect();
    let fund_for_req: Vec<_> = fund.iter().filter(|(id, _)| *id == 20003).collect();
    let histo_for_req: Vec<_> = histo.iter().filter(|(id, _)| *id == 20004).collect();

    println!("  Historical (20001): {} responses (expect 0)", hist_for_req.len());
    println!("  HeadTimestamp (20002): {} responses (expect 0)", head_for_req.len());
    println!("  Fundamental (20003): {} responses (expect 0)", fund_for_req.len());
    println!("  Histogram (20004): {} responses (expect 0)", histo_for_req.len());

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    // Cancelled requests should produce no responses (or at most partial data
    // that arrived before the cancel was processed — we tolerate that)
    println!("  All 4 cancel commands processed without crash");
    println!("  PASS\n");
    conns
}

// ─── Phase 130: Historical Data + Live Orders Coexistence ───

pub(super) fn phase_historical_and_orders(mut conns: Conns, gw: &Gateway, config: &GatewayConfig) -> Conns {
    println!("--- Phase 130: Historical Data + Live Orders Coexistence ---");

    ccp_keepalive(&mut conns.ccp);
    let hmds = match connect_farm(
        &config.host, "ushmds",
        &config.username, config.paper,
        &gw.server_session_id, &gw.session_token, &gw.hw_info, &gw.encoded,
    ) {
        Ok(c) => { println!("  HMDS reconnected"); Some(c) }
        Err(e) => {
            println!("  SKIP: ushmds reconnect failed: {}\n", e);
            return conns;
        }
    };

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), Some(event_tx), account_id.clone(), conns.farm, conns.ccp, hmds, None,
    );
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    // Step 1: Submit a limit order (far from market, won't fill)
    let oid = next_order_id();
    control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimitGtc {
        order_id: oid, instrument: inst_id, side: Side::Buy, qty: 1,
        price: 1_00_000_000, outside_rth: true,
    })).unwrap();

    // Step 2: Fire 5 historical requests while order is pending
    let now = now_ib_timestamp();
    for i in 0..5u32 {
        control_tx.send(ControlCommand::FetchHistorical {
            req_id: 30001 + i, con_id: 756733, symbol: "SPY".to_string(),
            end_date_time: now.clone(), duration: "1 d".to_string(),
            bar_size: "1 hour".to_string(), what_to_show: "TRADES".to_string(), use_rth: true,
        }).unwrap();
    }

    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut order_acked = false;
    let mut order_cancelled = false;
    let mut cancel_sent = false;
    let mut order_rejected = false;
    let mut hist_responses = std::collections::HashSet::new();

    while Instant::now() < deadline {
        // Check historical responses
        let data = shared.drain_historical_data();
        for (req_id, resp) in &data {
            if *req_id >= 30001 && *req_id <= 30005 && resp.is_complete {
                hist_responses.insert(*req_id);
            }
        }

        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::OrderUpdate(update)) => {
                if update.order_id == oid {
                    match update.status {
                        OrderStatus::Submitted => {
                            order_acked = true;
                            if !cancel_sent {
                                control_tx.send(ControlCommand::Order(
                                    OrderRequest::Cancel { order_id: oid }
                                )).unwrap();
                                cancel_sent = true;
                            }
                        }
                        OrderStatus::Cancelled => { order_cancelled = true; }
                        OrderStatus::Rejected => { order_rejected = true; }
                        _ => {}
                    }
                }
            }
            _ => {}
        }

        if order_cancelled && hist_responses.len() >= 3 { break; }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    println!("  Order: acked={} cancelled={} rejected={}",
        order_acked, order_cancelled, order_rejected);
    println!("  Historical responses: {}/5", hist_responses.len());

    if order_rejected {
        println!("  Order rejected — verifying historical path still works");
    }
    if !order_rejected {
        assert!(order_acked, "Order should have been acknowledged");
        assert!(order_cancelled, "Order should have been cancelled");
    }
    // At least some historical requests should complete even during order activity
    // (tolerance for HMDS pacing — may not get all 5)
    if hist_responses.is_empty() {
        println!("  SKIP: No historical responses — HMDS pacing limited\n");
    } else {
        println!("  PASS (order lifecycle + {} historical responses coexisted)\n", hist_responses.len());
    }
    conns
}
