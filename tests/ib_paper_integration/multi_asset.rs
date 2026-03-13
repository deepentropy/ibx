//! Multi-asset-class order test phases (forex, futures, options).

use super::common::*;
use ibx::control::contracts;
use ibx::protocol::fix;
use ibx::protocol::fixcomp;
use ibx::protocol::connection::Frame;

pub(super) fn phase_forex_order(conns: Conns) -> Conns {
    println!("--- Phase 98: Forex Order Lifecycle (EUR.USD) ---");

    // First, look up EUR.USD contract via CCP
    let now = ibx::gateway::chrono_free_timestamp();
    let mut ccp = conns.ccp;
    ccp.send_fix(&[
        (fix::TAG_MSG_TYPE, "c"),
        (fix::TAG_SENDING_TIME, &now),
        (contracts::TAG_SECURITY_REQ_ID, "RFXEUR"),
        (contracts::TAG_SECURITY_REQ_TYPE, "2"),
        (contracts::TAG_SYMBOL, "EUR"),
        (contracts::TAG_SECURITY_TYPE, "CASH"),
        (contracts::TAG_EXCHANGE, "IDEALPRO"),
        (contracts::TAG_CURRENCY, "USD"),
        (contracts::TAG_IB_SOURCE, "Socket"),
    ]).expect("Failed to send forex secdef request");

    let mut forex_con_id: Option<u32> = None;
    let deadline = Instant::now() + Duration::from_secs(10);

    while Instant::now() < deadline && forex_con_id.is_none() {
        match ccp.try_recv() {
            Ok(0) => { std::thread::sleep(Duration::from_millis(50)); continue; }
            Err(e) => { println!("  CCP recv error: {}", e); break; }
            Ok(_) => {}
        }
        for frame in ccp.extract_frames() {
            let messages = match frame {
                Frame::FixComp(raw) => {
                    let (unsigned, _) = ccp.unsign(&raw);
                    fixcomp::fixcomp_decompress(&unsigned)
                }
                Frame::Fix(raw) => vec![raw],
                _ => continue,
            };
            for msg in messages {
                let tags = fix::fix_parse(&msg);
                if tags.get(&fix::TAG_MSG_TYPE).map(|s| s.as_str()) == Some("d") {
                    if let Some(def) = contracts::parse_secdef_response(&msg) {
                        if def.sec_type == contracts::SecurityType::Forex {
                            println!("  Contract: {} conId={} secType={:?} exchange={}",
                                def.symbol, def.con_id, def.sec_type, def.exchange);
                            forex_con_id = Some(def.con_id);
                        }
                    }
                }
            }
        }
    }

    let fx_con_id = match forex_con_id {
        Some(id) => id,
        None => {
            println!("  SKIP: No forex contract found for EUR.USD\n");
            return Conns { farm: conns.farm, ccp, hmds: conns.hmds, account_id: conns.account_id };
        }
    };

    // Submit a forex limit order using the actual forex con_id
    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared, Some(event_tx), account_id.clone(), conns.farm, ccp, conns.hmds, None,
    );
    let inst = hot_loop.context_mut().register_instrument(fx_con_id as i64);
    hot_loop.context_mut().set_symbol(inst, "EUR".to_string());

    let oid = next_order_id();
    control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimitGtc {
        order_id: oid, instrument: inst, side: Side::Buy, qty: 20000, price: 50_000_000, outside_rth: true,
    })).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut order_acked = false;
    let mut cancel_sent = false;
    let mut order_cancelled = false;
    let mut order_rejected = false;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::OrderUpdate(update)) => {
                if update.order_id == oid {
                    match update.status {
                        OrderStatus::Submitted => {
                            order_acked = true;
                            if !cancel_sent {
                                control_tx.send(ControlCommand::Order(OrderRequest::Cancel { order_id: oid })).unwrap();
                                cancel_sent = true;
                            }
                        }
                        OrderStatus::Cancelled => { order_cancelled = true; break; }
                        OrderStatus::Rejected => { order_rejected = true; break; }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if order_rejected {
        println!("  SKIP: Forex order rejected (may need trading permissions)\n");
    } else {
        assert!(order_acked, "Forex order should be acknowledged");
        assert!(order_cancelled, "Forex order should be cancelled");
        println!("  PASS\n");
    }
    conns
}

pub(super) fn phase_futures_order(conns: Conns) -> Conns {
    println!("--- Phase 99: Futures Contract Details (MES) ---");

    // Look up MES (Micro E-mini S&P 500) via CCP
    let now = ibx::gateway::chrono_free_timestamp();
    let mut ccp = conns.ccp;
    ccp.send_fix(&[
        (fix::TAG_MSG_TYPE, "c"),
        (fix::TAG_SENDING_TIME, &now),
        (contracts::TAG_SECURITY_REQ_ID, "RFUT"),
        (contracts::TAG_SECURITY_REQ_TYPE, "2"),
        (contracts::TAG_SYMBOL, "MES"),
        (contracts::TAG_SECURITY_TYPE, "FUT"),
        (contracts::TAG_EXCHANGE, "CME"),
        (contracts::TAG_CURRENCY, "USD"),
        (contracts::TAG_IB_SOURCE, "Socket"),
    ]).expect("Failed to send futures secdef request");

    let mut fut_contract: Option<contracts::ContractDefinition> = None;
    let deadline = Instant::now() + Duration::from_secs(10);

    while Instant::now() < deadline && fut_contract.is_none() {
        match ccp.try_recv() {
            Ok(0) => { std::thread::sleep(Duration::from_millis(50)); continue; }
            Err(e) => { println!("  CCP recv error: {}", e); break; }
            Ok(_) => {}
        }
        for frame in ccp.extract_frames() {
            let messages = match frame {
                Frame::FixComp(raw) => {
                    let (unsigned, _) = ccp.unsign(&raw);
                    fixcomp::fixcomp_decompress(&unsigned)
                }
                Frame::Fix(raw) => vec![raw],
                _ => continue,
            };
            for msg in messages {
                let tags = fix::fix_parse(&msg);
                if tags.get(&fix::TAG_MSG_TYPE).map(|s| s.as_str()) == Some("d") {
                    if let Some(def) = contracts::parse_secdef_response(&msg) {
                        if def.sec_type == contracts::SecurityType::Future {
                            println!("  Contract: {} conId={} secType={:?} exchange={} expiry={} multiplier={}",
                                def.symbol, def.con_id, def.sec_type, def.exchange,
                                def.last_trade_date, def.multiplier);
                            assert!(def.multiplier > 0.0, "Futures multiplier should be positive");
                            assert!(!def.last_trade_date.is_empty(), "Futures should have expiry date");
                            // Take the first (front-month) contract
                            if fut_contract.is_none() {
                                fut_contract = Some(def);
                            }
                        }
                    }
                }
            }
        }
    }

    let fut_def = match fut_contract {
        Some(def) => def,
        None => {
            println!("  SKIP: No MES futures contract found\n");
            return Conns { farm: conns.farm, ccp, hmds: conns.hmds, account_id: conns.account_id };
        }
    };

    // Submit a futures limit order using the actual futures con_id
    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared, Some(event_tx), account_id.clone(), conns.farm, ccp, conns.hmds, None,
    );
    let inst = hot_loop.context_mut().register_instrument(fut_def.con_id as i64);
    hot_loop.context_mut().set_symbol(inst, "MES".to_string());

    let oid = next_order_id();
    control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimitGtc {
        order_id: oid, instrument: inst, side: Side::Buy, qty: 1, price: 100_00_000_000, outside_rth: true,
    })).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut order_acked = false;
    let mut cancel_sent = false;
    let mut order_cancelled = false;
    let mut order_rejected = false;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::OrderUpdate(update)) => {
                if update.order_id == oid {
                    match update.status {
                        OrderStatus::Submitted => {
                            order_acked = true;
                            if !cancel_sent {
                                control_tx.send(ControlCommand::Order(OrderRequest::Cancel { order_id: oid })).unwrap();
                                cancel_sent = true;
                            }
                        }
                        OrderStatus::Cancelled => { order_cancelled = true; break; }
                        OrderStatus::Rejected => { order_rejected = true; break; }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if order_rejected {
        println!("  SKIP: Futures order rejected (may need trading permissions)\n");
    } else {
        assert!(order_acked, "Futures order should be acknowledged");
        assert!(order_cancelled, "Futures order should be cancelled");
        println!("  PASS\n");
    }
    conns
}

pub(super) fn phase_options_order(conns: Conns) -> Conns {
    println!("--- Phase 100: Options Contract Details + Order (SPY options) ---");

    // Look up SPY options via CCP
    let now = ibx::gateway::chrono_free_timestamp();
    let mut ccp = conns.ccp;
    ccp.send_fix(&[
        (fix::TAG_MSG_TYPE, "c"),
        (fix::TAG_SENDING_TIME, &now),
        (contracts::TAG_SECURITY_REQ_ID, "ROPT"),
        (contracts::TAG_SECURITY_REQ_TYPE, "2"),
        (contracts::TAG_SYMBOL, "SPY"),
        (contracts::TAG_SECURITY_TYPE, "OPT"),
        (contracts::TAG_EXCHANGE, "BEST"),
        (contracts::TAG_CURRENCY, "USD"),
        (contracts::TAG_IB_SOURCE, "Socket"),
    ]).expect("Failed to send options secdef request");

    let mut option_contracts: Vec<contracts::ContractDefinition> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(15);

    while Instant::now() < deadline {
        match ccp.try_recv() {
            Ok(0) => { std::thread::sleep(Duration::from_millis(50)); continue; }
            Err(e) => { println!("  CCP recv error: {}", e); break; }
            Ok(_) => {}
        }
        let mut got_end = false;
        for frame in ccp.extract_frames() {
            let messages = match frame {
                Frame::FixComp(raw) => {
                    let (unsigned, _) = ccp.unsign(&raw);
                    fixcomp::fixcomp_decompress(&unsigned)
                }
                Frame::Fix(raw) => vec![raw],
                _ => continue,
            };
            for msg in messages {
                let tags = fix::fix_parse(&msg);
                let msg_type = tags.get(&fix::TAG_MSG_TYPE).map(|s| s.as_str()).unwrap_or("?");
                if msg_type == "d" {
                    if let Some(resp_type) = tags.get(&contracts::TAG_SECURITY_RESPONSE_TYPE) {
                        if resp_type == "6" || resp_type == "5" {
                            got_end = true;
                            continue;
                        }
                    }
                    if let Some(def) = contracts::parse_secdef_response(&msg) {
                        if def.sec_type == contracts::SecurityType::Option && def.right.is_some() {
                            option_contracts.push(def);
                        }
                    }
                }
            }
        }
        if got_end && !option_contracts.is_empty() { break; }
    }

    if option_contracts.is_empty() {
        println!("  SKIP: No SPY option contracts found\n");
        return Conns { farm: conns.farm, ccp, hmds: conns.hmds, account_id: conns.account_id };
    }

    // Pick the first call option found
    let opt = option_contracts.iter()
        .find(|d| d.right == Some(contracts::OptionRight::Call))
        .unwrap_or(&option_contracts[0]);
    println!("  Found {} option contracts, using: {} conId={} strike={} right={:?} expiry={}",
        option_contracts.len(), opt.symbol, opt.con_id, opt.strike,
        opt.right, opt.last_trade_date);
    assert!(opt.strike > 0.0, "Option strike should be positive");
    assert!(opt.multiplier > 0.0, "Option multiplier should be positive (typically 100)");

    // Submit an option limit order using the actual option con_id
    let opt_con_id = opt.con_id;
    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared, Some(event_tx), account_id.clone(), conns.farm, ccp, conns.hmds, None,
    );
    let inst = hot_loop.context_mut().register_instrument(opt_con_id as i64);
    hot_loop.context_mut().set_symbol(inst, "SPY".to_string());

    let oid = next_order_id();
    control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimitGtc {
        order_id: oid, instrument: inst, side: Side::Buy, qty: 1, price: 1_000_000, outside_rth: true,
    })).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut order_acked = false;
    let mut cancel_sent = false;
    let mut order_cancelled = false;
    let mut order_rejected = false;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::OrderUpdate(update)) => {
                if update.order_id == oid {
                    match update.status {
                        OrderStatus::Submitted => {
                            order_acked = true;
                            if !cancel_sent {
                                control_tx.send(ControlCommand::Order(OrderRequest::Cancel { order_id: oid })).unwrap();
                                cancel_sent = true;
                            }
                        }
                        OrderStatus::Cancelled => { order_cancelled = true; break; }
                        OrderStatus::Rejected => { order_rejected = true; break; }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if order_rejected {
        println!("  SKIP: Option order rejected (may need trading permissions)\n");
    } else {
        assert!(order_acked, "Option order should be acknowledged");
        assert!(order_cancelled, "Option order should be cancelled");
        println!("  PASS\n");
    }
    conns
}

pub(super) fn phase_concurrent_orders(conns: Conns) -> Conns {
    println!("--- Phase 101: Concurrent Orders in Flight (3 simultaneous limit orders) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared, Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    // Register SPY
    let spy_inst = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(spy_inst, "SPY".to_string());

    // Submit 3 limit orders simultaneously at $1.00 (far below market)
    let oid1 = next_order_id();
    let oid2 = oid1 + 1;
    let oid3 = oid1 + 2;

    control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimitGtc {
        order_id: oid1, instrument: 0, side: Side::Buy, qty: 1, price: 1_00_000_000, outside_rth: true,
    })).unwrap();
    control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimitGtc {
        order_id: oid2, instrument: 0, side: Side::Buy, qty: 1, price: 1_00_000_000, outside_rth: true,
    })).unwrap();
    control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimitGtc {
        order_id: oid3, instrument: 0, side: Side::Buy, qty: 1, price: 1_00_000_000, outside_rth: true,
    })).unwrap();

    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut acked = [false; 3];
    let mut cancelled = [false; 3];
    let mut cancel_sent = false;
    let mut rejected = false;
    let oids = [oid1, oid2, oid3];

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::OrderUpdate(update)) => {
                let idx = oids.iter().position(|&id| id == update.order_id);
                if let Some(i) = idx {
                    match update.status {
                        OrderStatus::Submitted => {
                            acked[i] = true;
                            // Once all 3 are acked, cancel them all
                            if acked.iter().all(|&a| a) && !cancel_sent {
                                for &oid in &oids {
                                    control_tx.send(ControlCommand::Order(OrderRequest::Cancel { order_id: oid })).unwrap();
                                }
                                cancel_sent = true;
                            }
                        }
                        OrderStatus::Cancelled => { cancelled[i] = true; }
                        OrderStatus::Rejected => { rejected = true; break; }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        if cancelled.iter().all(|&c| c) { break; }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if rejected {
        println!("  SKIP: One or more orders rejected\n");
        return conns;
    }

    let acked_count = acked.iter().filter(|&&a| a).count();
    let cancelled_count = cancelled.iter().filter(|&&c| c).count();
    println!("  Acked: {}/3  Cancelled: {}/3", acked_count, cancelled_count);

    assert_eq!(acked_count, 3, "All 3 orders should be acknowledged");
    assert_eq!(cancelled_count, 3, "All 3 orders should be cancelled");
    println!("  PASS\n");
    conns
}
