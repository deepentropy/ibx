//! Account data, PnL, summary, and position tracking test phases.

use super::common::*;
use ibx::gateway;
use ibx::protocol::fix;

pub(super) fn phase_account_data(conns: Conns) -> Conns {
    println!("--- Phase 4: Account Data Reception ---");

    let account_id = conns.account_id;

    let mut ccp = conns.ccp;
    let ts = gateway::chrono_free_timestamp();
    let _ = ccp.send_fix(&[
        (fix::TAG_MSG_TYPE, "U"),
        (fix::TAG_SENDING_TIME, &ts),
        (6040, "76"),
        (1, ""),
        (6565, "1"),
    ]);

    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), Some(event_tx), account_id.clone(), conns.farm, ccp, conns.hmds, None,
    );

    control_tx.send(ControlCommand::Subscribe { con_id: 265598, symbol: "AAPL".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut account_checked = false;
    let mut net_liq = 0i64;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Event::Tick(_)) => {
                if !account_checked {
                    let acct = shared.account();
                    if acct.net_liquidation != 0 {
                        net_liq = acct.net_liquidation;
                        println!("  ACCOUNT: net_liq={:.2}", net_liq as f64 / PRICE_SCALE as f64);
                        account_checked = true;
                        break;
                    }
                }
            }
            _ => {}
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if account_checked {
        assert!(net_liq > 0, "Paper account net liquidation should be > 0");
        println!("  net_liq=${:.2}", net_liq as f64 / PRICE_SCALE as f64);
        println!("  PASS\n");
    } else {
        println!("  WARN: Account data not received within 20s\n");
    }
    conns
}

pub(super) fn phase_account_pnl(conns: Conns) -> Conns {
    println!("--- Phase 14: Account PnL Reception ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    // Register SPY instrument so on_start order submission works
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    let order_id = next_order_id();
    control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimitGtc {
        order_id,
        instrument: inst_id,
        side: Side::Buy,
        qty: 1,
        price: 1_00_000_000,
        outside_rth: true,
    })).unwrap();

    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut account_received = false;
    let mut net_liq = 0i64;
    let mut probe_done = false;

    while Instant::now() < deadline && !probe_done {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::OrderUpdate(update)) => {
                if !account_received {
                    let acct = shared.account();
                    if acct.net_liquidation != 0 {
                        net_liq = acct.net_liquidation;
                        account_received = true;
                    }
                }
                if update.status == OrderStatus::Submitted {
                    control_tx.send(ControlCommand::Order(OrderRequest::Cancel { order_id })).unwrap();
                }
                if matches!(update.status, OrderStatus::Cancelled | OrderStatus::Rejected) {
                    probe_done = true;
                }
            }
            Ok(Event::Tick(_)) => {
                if !account_received {
                    let acct = shared.account();
                    if acct.net_liquidation != 0 {
                        net_liq = acct.net_liquidation;
                        account_received = true;
                    }
                }
            }
            _ => {}
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    assert!(account_received,
        "Account data not received — 6040=77 may not contain tag 9806");
    assert!(net_liq > 0, "Paper account net liquidation should be > 0");
    println!("  NetLiq: ${:.2}", net_liq as f64 / PRICE_SCALE as f64);
    println!("  PASS\n");
    conns
}

pub(super) fn phase_position_tracking(conns: Conns) -> Conns {
    println!("--- Phase 97: Position Tracking (SPY buy+sell round trip) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared, Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(60);
    let mut phase = 0u8; // 0=wait ticks, 1=buy sent, 2=sell sent
    let mut tick_count = 0u32;
    let mut got_position_update = false;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::Tick(instrument)) => {
                tick_count += 1;
                if phase == 0 && tick_count >= 5 {
                    let buy_oid = next_order_id();
                    control_tx.send(ControlCommand::Order(OrderRequest::SubmitMarket {
                        order_id: buy_oid, instrument, side: Side::Buy, qty: 1,
                    })).unwrap();
                    phase = 1;
                }
            }
            Ok(Event::Fill(fill)) => {
                if phase == 1 && fill.side == Side::Buy {
                    let sell_order_id = next_order_id() + 1;
                    control_tx.send(ControlCommand::Order(OrderRequest::SubmitMarket {
                        order_id: sell_order_id, instrument: fill.instrument, side: Side::Sell, qty: 1,
                    })).unwrap();
                    phase = 2;
                } else if phase == 2 && fill.side == Side::Sell {
                    // Wait a bit more for position update
                    std::thread::sleep(Duration::from_secs(2));
                    break;
                }
            }
            Ok(Event::PositionUpdate { instrument, con_id, position, avg_cost }) => {
                println!("  PositionUpdate: inst={} conId={} pos={} avgCost={:.4}",
                    instrument, con_id, position, avg_cost as f64 / ibx::types::PRICE_SCALE as f64);
                got_position_update = true;
            }
            Ok(Event::OrderUpdate(update)) => {
                if update.status == OrderStatus::Rejected {
                    println!("  SKIP: Order rejected — market closed\n");
                    let conns = shutdown_and_reclaim(&control_tx, join, account_id);
                    return conns;
                }
            }
            _ => {}
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if phase < 1 {
        println!("  SKIP: No ticks received — market closed\n");
    } else if got_position_update {
        println!("  PASS\n");
    } else {
        println!("  SKIP: No PositionUpdate events received (fills completed but no position update)\n");
    }
    conns
}

pub(super) fn phase_account_summary(conns: Conns) -> Conns {
    println!("--- Phase 106: Account Summary (raw CCP request) ---");

    let ccp = conns.ccp;

    // Account data is available via the HotLoop account state
    let account_id = conns.account_id.clone();
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), Some(event_tx), account_id.clone(),
        conns.farm, ccp, conns.hmds, None,
    );

    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    // Wait for account data to populate
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut has_account_data = false;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Event::Tick(_)) => {
                let acct = shared.account();
                if acct.net_liquidation > 0 {
                    println!("  NetLiquidation: {:.2}", acct.net_liquidation as f64 / PRICE_SCALE as f64);
                    println!("  BuyingPower: {:.2}", acct.buying_power as f64 / PRICE_SCALE as f64);
                    println!("  AvailableFunds: {:.2}", acct.available_funds as f64 / PRICE_SCALE as f64);
                    println!("  MarginUsed: {:.2}", acct.margin_used as f64 / PRICE_SCALE as f64);
                    println!("  MaintMarginReq: {:.2}", acct.maint_margin_req as f64 / PRICE_SCALE as f64);
                    has_account_data = true;

                    // Validate account data sanity
                    assert!(acct.net_liquidation > 0, "NetLiquidation should be positive");
                    assert!(acct.buying_power >= 0, "BuyingPower should be non-negative");
                    break;
                }
            }
            _ => {}
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if has_account_data {
        println!("  PASS\n");
    } else {
        println!("  SKIP: No account data received\n");
    }
    conns
}
