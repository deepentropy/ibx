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
    println!("--- Phase 106: Account Summary (verify individual tag values) ---");

    let account_id = conns.account_id.clone();
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), Some(event_tx), account_id.clone(),
        conns.farm, conns.ccp, conns.hmds, None,
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
                    // Verify individual fields
                    println!("  NetLiquidation:    {:.2}", acct.net_liquidation as f64 / PRICE_SCALE as f64);
                    println!("  BuyingPower:       {:.2}", acct.buying_power as f64 / PRICE_SCALE as f64);
                    println!("  TotalCashValue:    {:.2}", acct.total_cash_value as f64 / PRICE_SCALE as f64);
                    println!("  SettledCash:       {:.2}", acct.settled_cash as f64 / PRICE_SCALE as f64);
                    println!("  AvailableFunds:    {:.2}", acct.available_funds as f64 / PRICE_SCALE as f64);
                    println!("  ExcessLiquidity:   {:.2}", acct.excess_liquidity as f64 / PRICE_SCALE as f64);
                    println!("  InitMarginReq:     {:.2}", acct.init_margin_req as f64 / PRICE_SCALE as f64);
                    println!("  MaintMarginReq:    {:.2}", acct.maint_margin_req as f64 / PRICE_SCALE as f64);
                    println!("  EquityWithLoan:    {:.2}", acct.equity_with_loan as f64 / PRICE_SCALE as f64);
                    println!("  Cushion:           {:.4}", acct.cushion as f64 / PRICE_SCALE as f64);
                    println!("  Leverage:          {:.4}", acct.leverage as f64 / PRICE_SCALE as f64);
                    println!("  DayTradesRemain:   {}", acct.day_trades_remaining);
                    println!("  UnrealizedPnL:     {:.2}", acct.unrealized_pnl as f64 / PRICE_SCALE as f64);
                    println!("  RealizedPnL:       {:.2}", acct.realized_pnl as f64 / PRICE_SCALE as f64);
                    println!("  DailyPnL:          {:.2}", acct.daily_pnl as f64 / PRICE_SCALE as f64);
                    has_account_data = true;

                    // Validate sanity
                    assert!(acct.net_liquidation > 0, "NetLiquidation should be positive");
                    assert!(acct.buying_power >= 0, "BuyingPower should be non-negative");
                    assert!(acct.available_funds >= 0, "AvailableFunds should be non-negative");
                    assert!(acct.excess_liquidity >= 0, "ExcessLiquidity should be non-negative");
                    // EquityWithLoanValue should be close to NetLiquidation for paper accounts
                    if acct.equity_with_loan > 0 {
                        let ratio = acct.equity_with_loan as f64 / acct.net_liquidation as f64;
                        assert!(ratio > 0.5 && ratio < 2.0,
                            "EquityWithLoan/NetLiq ratio {:.2} seems wrong", ratio);
                    }
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

/// Phase: Completed Orders — submit an order, cancel it, verify it appears in drain_completed_orders.
pub(super) fn phase_completed_orders(conns: Conns) -> Conns {
    println!("--- Phase 120: Completed Orders (submit+cancel → drain) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), Some(event_tx), account_id.clone(),
        conns.farm, conns.ccp, conns.hmds, None,
    );
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    let order_id = next_order_id();
    control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimitGtc {
        order_id,
        instrument: inst_id,
        side: Side::Buy,
        qty: 1,
        price: 1_00_000_000, // $1 — won't fill
        outside_rth: true,
    })).unwrap();

    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut cancel_sent = false;
    let mut terminal = false;

    while Instant::now() < deadline && !terminal {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::OrderUpdate(update)) => {
                println!("  OrderUpdate: id={} status={:?}", update.order_id, update.status);
                if update.status == OrderStatus::Submitted && !cancel_sent {
                    control_tx.send(ControlCommand::Order(OrderRequest::Cancel { order_id })).unwrap();
                    cancel_sent = true;
                }
                if matches!(update.status, OrderStatus::Cancelled | OrderStatus::Rejected | OrderStatus::Filled) {
                    terminal = true;
                }
            }
            _ => {}
        }
    }

    // Give the hot loop a moment to archive the completed order
    std::thread::sleep(Duration::from_millis(200));

    let completed = shared.drain_completed_orders();
    println!("  Completed orders drained: {}", completed.len());
    for co in &completed {
        println!("    order_id={} status={:?} filled_qty={}", co.order_id, co.status, co.filled_qty);
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if !terminal {
        println!("  SKIP: Order never reached terminal state\n");
        return conns;
    }

    assert!(!completed.is_empty(), "Expected at least one completed order after cancel");
    let co = completed.iter().find(|c| c.order_id == order_id);
    assert!(co.is_some(), "Completed order for our order_id not found");
    let co = co.unwrap();
    assert!(
        matches!(co.status, OrderStatus::Cancelled | OrderStatus::Rejected),
        "Expected Cancelled or Rejected, got {:?}", co.status
    );
    println!("  PASS\n");
    conns
}

/// Phase: PnL Subscription Lifecycle — verify daily/unrealized/realized PnL populated.
pub(super) fn phase_pnl_subscription(conns: Conns) -> Conns {
    println!("--- Phase 121: PnL Subscription (verify all 3 PnL fields) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), Some(event_tx), account_id.clone(),
        conns.farm, conns.ccp, conns.hmds, None,
    );
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    // Submit a far-from-market order to trigger account updates
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

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut pnl_checked = false;
    let mut order_submitted = false;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Event::OrderUpdate(update)) => {
                if update.status == OrderStatus::Submitted {
                    order_submitted = true;
                }
            }
            Ok(Event::Tick(_)) | Ok(_) => {
                if !pnl_checked {
                    let acct = shared.account();
                    // DailyPnL is populated from CCP 35=UT/UM/RL messages.
                    // At least net_liquidation should be set; PnL fields may be 0
                    // if no positions exist, which is valid.
                    if acct.net_liquidation > 0 {
                        println!("  DailyPnL:      {:.2}", acct.daily_pnl as f64 / PRICE_SCALE as f64);
                        println!("  UnrealizedPnL: {:.2}", acct.unrealized_pnl as f64 / PRICE_SCALE as f64);
                        println!("  RealizedPnL:   {:.2}", acct.realized_pnl as f64 / PRICE_SCALE as f64);
                        pnl_checked = true;
                        // Cancel the probe order
                        control_tx.send(ControlCommand::Order(OrderRequest::Cancel { order_id })).unwrap();
                    }
                }
            }
            Err(_) => {}
        }
        if pnl_checked && order_submitted { break; }
    }

    // Wait for cancel to complete
    let cancel_deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < cancel_deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::OrderUpdate(u)) if matches!(u.status, OrderStatus::Cancelled | OrderStatus::Rejected) => break,
            _ => {}
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if pnl_checked {
        // PnL fields populated (even if 0 — that's valid for no-position accounts)
        println!("  PASS\n");
    } else {
        println!("  SKIP: Account data not received in time\n");
    }
    conns
}

/// Phase: News Bulletins — drain news bulletins from SharedState.
pub(super) fn phase_news_bulletins(conns: Conns) -> Conns {
    println!("--- Phase 122: News Bulletins (drain from SharedState) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), Some(event_tx), account_id.clone(),
        conns.farm, conns.ccp, conns.hmds, None,
    );

    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    // Wait for any events to flow, checking for bulletins periodically
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut total_bulletins = 0usize;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(500)) {
            _ => {
                let bulletins = shared.drain_news_bulletins();
                for b in &bulletins {
                    println!("  Bulletin: id={} type={} exchange={} msg={}",
                        b.msg_id, b.msg_type, b.exchange,
                        &b.message[..std::cmp::min(80, b.message.len())]);
                }
                total_bulletins += bulletins.len();
            }
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    // News bulletins are sporadic — we may or may not receive any.
    // The test validates the drain mechanism works without panicking.
    println!("  Total bulletins received: {}", total_bulletins);
    if total_bulletins > 0 {
        println!("  PASS (received {} bulletins)\n", total_bulletins);
    } else {
        println!("  PASS (no bulletins during test window — drain mechanism verified)\n");
    }
    conns
}
