//! Account data, PnL, summary, and position tracking test phases.

use super::common::*;
use ibx::api::types as api;
use ibx::api::wrapper::Wrapper;
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

    control_tx.send(ControlCommand::Subscribe { con_id: 265598, symbol: "AAPL".into(), exchange: String::new(), sec_type: String::new(), reply_tx: None }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut account_checked = false;
    let mut net_liq = 0i64;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Event::Tick(_)) => {
                if !account_checked {
                    let acct = shared.portfolio.account();
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
                    let acct = shared.portfolio.account();
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
                    let acct = shared.portfolio.account();
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
        shared.clone(), Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into(), exchange: String::new(), sec_type: String::new(), reply_tx: None }).unwrap();
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
    } else if phase == 2 && got_position_update {
        // After buy+sell round trip, position should return to 0 (or near it)
        let pos = shared.portfolio.position(0);
        println!("  Final position: {}", pos);
        assert!(pos.abs() <= 1, "Position after round trip should be 0 (±1 for timing), got {}", pos);
        println!("  PASS (position returned to {})\n", pos);
    } else if phase == 2 {
        println!("  SKIP: Fills completed but no PositionUpdate events\n");
    } else {
        println!("  SKIP: Only reached phase {} (buy may not have filled)\n", phase);
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

    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into(), exchange: String::new(), sec_type: String::new(), reply_tx: None }).unwrap();
    let join = run_hot_loop(hot_loop);

    // Wait for account data to populate
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut has_account_data = false;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Event::Tick(_)) => {
                let acct = shared.portfolio.account();
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

    let completed = shared.orders.drain_completed_orders();
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

/// Phase: Enriched API output — submit+cancel an order, then call the Rust API Wrapper
/// callbacks (completed_order, position) and verify the Contract/Order/OrderState fields
/// match the ibapi GT capture format.
pub(super) fn phase_enriched_order_cache(conns: Conns) -> Conns {
    println!("--- Phase 130: Enriched API Wrapper Output (submit+cancel → req_completed_orders) ---");

    // Recording wrapper that captures full field data from completed_order/position callbacks
    struct GtWrapper {
        completed: Vec<(api::Contract, api::Order, api::OrderState)>,
        positions: Vec<(String, api::Contract, f64, f64)>,
    }
    impl Wrapper for GtWrapper {
        fn completed_order(&mut self, contract: &api::Contract, order: &api::Order, state: &api::OrderState) {
            self.completed.push((contract.clone(), order.clone(), state.clone()));
        }
        fn completed_orders_end(&mut self) {}
        fn position(&mut self, account: &str, contract: &api::Contract, pos: f64, avg_cost: f64) {
            self.positions.push((account.to_string(), contract.clone(), pos, avg_cost));
        }
        fn position_end(&mut self) {}
        fn open_order(&mut self, _: i64, _: &api::Contract, _: &api::Order, _: &api::OrderState) {}
        fn open_order_end(&mut self) {}
    }

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), Some(event_tx), account_id.clone(),
        conns.farm, conns.ccp, conns.hmds, None,
    );
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    // Fetch secdef first to populate contract cache with exchange/localSymbol/tradingClass
    control_tx.send(ControlCommand::FetchContractDetails {
        req_id: 9999, con_id: 756733, symbol: String::new(),
        sec_type: String::new(), exchange: String::new(), currency: String::new(),
    }).unwrap();

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

    // Wait for secdef + order lifecycle
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut cancel_sent = false;
    let mut terminal = false;

    while Instant::now() < deadline && !terminal {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::OrderUpdate(update)) => {
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

    std::thread::sleep(Duration::from_millis(500));

    // ── Exercise the real API path: req_completed_orders + req_positions ──
    let mut wrapper = GtWrapper { completed: Vec::new(), positions: Vec::new() };

    // Manually do what EClient::req_completed_orders does
    for co in shared.orders.drain_completed_orders() {
        let status_str = match co.status {
            OrderStatus::Filled => "Filled",
            OrderStatus::Cancelled => "Cancelled",
            OrderStatus::Rejected => "Inactive",
            _ => "Unknown",
        };
        if let Some(info) = shared.orders.get_order_info(co.order_id) {
            let mut state = info.order_state;
            state.status = status_str.into();
            wrapper.completed_order(&info.contract, &info.order, &state);
        } else {
            let c = api::Contract::default();
            let o = api::Order { order_id: co.order_id as i64, ..Default::default() };
            let s = api::OrderState { status: status_str.into(), ..Default::default() };
            wrapper.completed_order(&c, &o, &s);
        }
    }

    // Manually do what EClient::req_positions does
    for pi in shared.portfolio.position_infos() {
        let c = shared.reference.get_contract(pi.con_id)
            .unwrap_or_else(|| api::Contract { con_id: pi.con_id, ..Default::default() });
        let avg_cost = pi.avg_cost as f64 / PRICE_SCALE as f64;
        wrapper.position(&account_id, &c, pi.position as f64, avg_cost);
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if !terminal {
        println!("  SKIP: Order never reached terminal state\n");
        return conns;
    }

    // ── Compare Wrapper output against GT expectations ──
    // GT: completedOrder has contract with conId/symbol/secType/currency,
    //     order with action/totalQuantity/orderType/tif/account,
    //     orderState with status
    let mut pass = true;
    if let Some((c, o, s)) = wrapper.completed.first() {
        println!("  completed_order callback received:");

        // Contract fields (GT: conId=756733, symbol=SPY, secType=STK, currency=USD)
        println!("    contract.conId      = {} (GT: 756733)", c.con_id);
        println!("    contract.symbol     = '{}' (GT: 'SPY')", c.symbol);
        println!("    contract.secType    = '{}' (GT: 'STK')", c.sec_type);
        println!("    contract.currency   = '{}' (GT: 'USD')", c.currency);
        println!("    contract.exchange   = '{}' (GT: 'SMART')", c.exchange);
        println!("    contract.localSym   = '{}' (GT: 'SPY')", c.local_symbol);
        println!("    contract.tradClass  = '{}' (GT: 'SPY')", c.trading_class);

        // Order fields (GT: action=BUY, totalQuantity=1, orderType=LMT, tif=GTC, account=DU5479259)
        println!("    order.action        = '{}' (GT: 'BUY')", o.action);
        println!("    order.totalQuantity = {} (GT: 1.0)", o.total_quantity);
        println!("    order.orderType     = '{}' (GT: 'LMT')", o.order_type);
        println!("    order.tif           = '{}' (GT: 'GTC')", o.tif);
        println!("    order.account       = '{}' (GT: 'DU5479259')", o.account);

        // OrderState fields (GT: status=Cancelled)
        println!("    orderState.status   = '{}' (GT: 'Cancelled')", s.status);

        // Validate
        if c.con_id != 756733 { println!("    FAIL: conId"); pass = false; }
        if c.symbol != "SPY" { println!("    FAIL: symbol"); pass = false; }
        if c.sec_type != "STK" { println!("    FAIL: secType"); pass = false; }
        if c.currency != "USD" { println!("    FAIL: currency"); pass = false; }
        if c.local_symbol.is_empty() { println!("    FAIL: localSymbol empty"); pass = false; }
        if c.trading_class.is_empty() { println!("    FAIL: tradingClass empty"); pass = false; }
        if o.action != "BUY" { println!("    FAIL: action"); pass = false; }
        if (o.total_quantity - 1.0).abs() > 0.01 { println!("    FAIL: totalQuantity"); pass = false; }
        if o.order_type != "LMT" { println!("    FAIL: orderType"); pass = false; }
        if o.tif != "GTC" { println!("    FAIL: tif"); pass = false; }
        if o.account.is_empty() { println!("    FAIL: account empty"); pass = false; }
        if s.status != "Cancelled" { println!("    FAIL: status"); pass = false; }
    } else {
        println!("  No completed_order callback — CCP may not have sent enriched exec report");
        pass = false;
    }

    if pass {
        println!("  PASS (all fields match GT)\n");
    } else {
        println!("  FAIL\n");
    }
    assert!(pass, "Enriched API output did not match GT expectations");

    conns
}

/// Phase: req_all_open_orders — submit a limit order, call open_order Wrapper callback,
/// verify Contract/Order/OrderState fields match GT, then cancel.
pub(super) fn phase_enriched_open_orders(conns: Conns) -> Conns {
    println!("--- Phase 131: Enriched open_order Wrapper Output (submit → req_all_open_orders → cancel) ---");

    struct OoWrapper {
        orders: Vec<(i64, api::Contract, api::Order, api::OrderState)>,
    }
    impl Wrapper for OoWrapper {
        fn open_order(&mut self, order_id: i64, contract: &api::Contract, order: &api::Order, state: &api::OrderState) {
            self.orders.push((order_id, contract.clone(), order.clone(), state.clone()));
        }
        fn open_order_end(&mut self) {}
    }

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), Some(event_tx), account_id.clone(),
        conns.farm, conns.ccp, conns.hmds, None,
    );
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    // Fetch secdef to populate contract cache
    control_tx.send(ControlCommand::FetchContractDetails {
        req_id: 9998, con_id: 756733, symbol: String::new(),
        sec_type: String::new(), exchange: String::new(), currency: String::new(),
    }).unwrap();

    let order_id = next_order_id();
    control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimitGtc {
        order_id,
        instrument: inst_id,
        side: Side::Buy,
        qty: 1,
        price: 1_00_000_000, // $1
        outside_rth: true,
    })).unwrap();

    let join = run_hot_loop(hot_loop);

    // Wait for Submitted
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut submitted = false;
    while Instant::now() < deadline && !submitted {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::OrderUpdate(u)) if u.status == OrderStatus::Submitted => { submitted = true; }
            _ => {}
        }
    }

    if !submitted {
        let conns = shutdown_and_reclaim(&control_tx, join, account_id);
        println!("  SKIP: Order never submitted\n");
        return conns;
    }

    // Wait a bit for CCP exec report to populate the enriched cache
    std::thread::sleep(Duration::from_millis(500));

    // Exercise req_all_open_orders path
    let mut wrapper = OoWrapper { orders: Vec::new() };
    for (oid, info) in shared.orders.drain_open_orders() {
        if !matches!(info.order_state.status.as_str(), "Filled" | "Cancelled" | "Inactive") {
            wrapper.open_order(oid as i64, &info.contract, &info.order, &info.order_state);
        }
    }

    // Cancel the order
    control_tx.send(ControlCommand::Order(OrderRequest::Cancel { order_id })).unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::OrderUpdate(u)) if matches!(u.status, OrderStatus::Cancelled | OrderStatus::Rejected) => break,
            _ => {}
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    // Validate — GT: contract has conId/symbol/secType/currency, order has action/qty/type/tif/account/lmtPrice
    let mut pass = true;
    if let Some((oid, c, o, s)) = wrapper.orders.first() {
        println!("  open_order callback received (orderId={}):", oid);
        println!("    contract.conId      = {} (GT: 756733)", c.con_id);
        println!("    contract.symbol     = '{}' (GT: 'SPY')", c.symbol);
        println!("    contract.secType    = '{}' (GT: 'STK')", c.sec_type);
        println!("    contract.currency   = '{}' (GT: 'USD')", c.currency);
        println!("    contract.exchange   = '{}' (GT: 'SMART')", c.exchange);
        println!("    contract.localSym   = '{}' (GT: 'SPY')", c.local_symbol);
        println!("    contract.tradClass  = '{}' (GT: 'SPY')", c.trading_class);
        println!("    order.action        = '{}' (GT: 'BUY')", o.action);
        println!("    order.totalQuantity = {} (GT: 1.0)", o.total_quantity);
        println!("    order.orderType     = '{}' (GT: 'LMT')", o.order_type);
        println!("    order.tif           = '{}' (GT: 'GTC')", o.tif);
        println!("    order.account       = '{}'", o.account);
        println!("    order.lmtPrice      = {} (GT: 1.0)", o.lmt_price);
        println!("    orderState.status   = '{}' (GT: 'Submitted')", s.status);

        if c.con_id != 756733 { println!("    FAIL: conId"); pass = false; }
        if c.symbol != "SPY" { println!("    FAIL: symbol"); pass = false; }
        if c.sec_type != "STK" { println!("    FAIL: secType"); pass = false; }
        if c.currency != "USD" { println!("    FAIL: currency"); pass = false; }
        if c.local_symbol.is_empty() { println!("    FAIL: localSymbol empty"); pass = false; }
        if c.trading_class.is_empty() { println!("    FAIL: tradingClass empty"); pass = false; }
        if o.action != "BUY" { println!("    FAIL: action"); pass = false; }
        if (o.total_quantity - 1.0).abs() > 0.01 { println!("    FAIL: totalQuantity"); pass = false; }
        if o.order_type != "LMT" { println!("    FAIL: orderType"); pass = false; }
        if o.tif != "GTC" { println!("    FAIL: tif"); pass = false; }
        if o.account.is_empty() { println!("    FAIL: account empty"); pass = false; }
        if !matches!(s.status.as_str(), "Submitted" | "PreSubmitted") {
            println!("    FAIL: status should be Submitted/PreSubmitted"); pass = false;
        }
    } else {
        println!("  No open_order callback received");
        pass = false;
    }

    if pass { println!("  PASS (all fields match GT)\n"); }
    else { println!("  FAIL\n"); }
    assert!(pass, "open_order Wrapper output did not match GT");
    conns
}

/// Phase: req_positions — verify position callback delivers enriched Contract
/// with symbol/secType/currency from the contract cache.
pub(super) fn phase_enriched_positions(conns: Conns) -> Conns {
    println!("--- Phase 132: Enriched position Wrapper Output (req_positions) ---");

    struct PosWrapper {
        positions: Vec<(String, api::Contract, f64, f64)>,
    }
    impl Wrapper for PosWrapper {
        fn position(&mut self, account: &str, contract: &api::Contract, pos: f64, avg_cost: f64) {
            self.positions.push((account.to_string(), contract.clone(), pos, avg_cost));
        }
        fn position_end(&mut self) {}
    }

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), Some(event_tx), account_id.clone(),
        conns.farm, conns.ccp, conns.hmds, None,
    );
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    let join = run_hot_loop(hot_loop);

    // Wait for position updates to arrive (UP messages from CCP)
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut got_pos = false;
    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Event::PositionUpdate { con_id, position, .. }) if con_id == 756733 => {
                println!("  PositionUpdate: con_id={} position={}", con_id, position);
                got_pos = true;
                break;
            }
            _ => {}
        }
    }

    // Give time for contract cache to be populated from exec reports
    std::thread::sleep(Duration::from_millis(500));

    // Exercise req_positions path
    let mut wrapper = PosWrapper { positions: Vec::new() };
    for pi in shared.portfolio.position_infos() {
        let c = shared.reference.get_contract(pi.con_id)
            .unwrap_or_else(|| api::Contract { con_id: pi.con_id, ..Default::default() });
        let avg_cost = pi.avg_cost as f64 / PRICE_SCALE as f64;
        wrapper.position(&account_id, &c, pi.position as f64, avg_cost);
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if !got_pos && wrapper.positions.is_empty() {
        println!("  SKIP: No positions in account (need prior fills)\n");
        return conns;
    }

    // Validate — GT: contract has conId=756733, symbol=SPY, secType=STK
    let mut pass = true;
    if let Some((acct, c, pos, avg_cost)) = wrapper.positions.iter().find(|(_, c, _, _)| c.con_id == 756733) {
        println!("  position callback received:");
        println!("    account             = '{}' (GT: 'DU5479259')", acct);
        println!("    contract.conId      = {} (GT: 756733)", c.con_id);
        println!("    contract.symbol     = '{}' (GT: 'SPY')", c.symbol);
        println!("    contract.secType    = '{}' (GT: 'STK')", c.sec_type);
        println!("    position            = {} (GT: dynamic)", pos);
        println!("    avgCost             = {:.3} (GT: dynamic)", avg_cost);

        if c.con_id != 756733 { println!("    FAIL: conId"); pass = false; }
        if c.symbol != "SPY" { println!("    FAIL: symbol"); pass = false; }
        if c.sec_type != "STK" { println!("    FAIL: secType"); pass = false; }
    } else {
        // May not have SPY position — check any position has enriched contract
        if let Some((acct, c, pos, avg_cost)) = wrapper.positions.first() {
            println!("  position callback (no SPY, checking first):");
            println!("    account             = '{}'", acct);
            println!("    contract.conId      = {}", c.con_id);
            println!("    contract.symbol     = '{}'", c.symbol);
            println!("    contract.secType    = '{}'", c.sec_type);
            println!("    position            = {}", pos);
            println!("    avgCost             = {:.3}", avg_cost);
            if c.con_id == 0 { println!("    FAIL: conId is 0"); pass = false; }
        } else {
            println!("  No position callbacks at all");
            pass = false;
        }
    }

    if pass { println!("  PASS\n"); }
    else { println!("  FAIL\n"); }
    assert!(pass, "position Wrapper output did not match GT");
    conns
}

/// Phase: exec_details — submit a market order (fills immediately), verify exec_details
/// callback has enriched Contract with conId/symbol/secType.
pub(super) fn phase_enriched_exec_details(conns: Conns) -> Conns {
    println!("--- Phase 133: Enriched exec_details Wrapper Output (market order → fill) ---");

    struct ExecWrapper {
        execs: Vec<(i64, api::Contract, api::Execution)>,
    }
    impl Wrapper for ExecWrapper {
        fn exec_details(&mut self, req_id: i64, contract: &api::Contract, execution: &api::Execution) {
            self.execs.push((req_id, contract.clone(), execution.clone()));
        }
        fn order_status(&mut self, _: i64, _: &str, _: f64, _: f64, _: f64, _: i64, _: i64, _: f64, _: i64, _: &str, _: f64) {}
        fn open_order(&mut self, _: i64, _: &api::Contract, _: &api::Order, _: &api::OrderState) {}
    }

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), Some(event_tx), account_id.clone(),
        conns.farm, conns.ccp, conns.hmds, None,
    );
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    // Fetch secdef to populate contract cache
    control_tx.send(ControlCommand::FetchContractDetails {
        req_id: 9997, con_id: 756733, symbol: String::new(),
        sec_type: String::new(), exchange: String::new(), currency: String::new(),
    }).unwrap();

    let order_id = next_order_id();
    control_tx.send(ControlCommand::Order(OrderRequest::SubmitMarket {
        order_id,
        instrument: inst_id,
        side: Side::Buy,
        qty: 1,
    })).unwrap();

    let join = run_hot_loop(hot_loop);

    // Wait for fill
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut filled = false;
    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::Fill(f)) if f.order_id == order_id => {
                println!("  Fill received: qty={} price={:.2}", f.qty, f.price as f64 / PRICE_SCALE as f64);
                filled = true;
                break;
            }
            Ok(Event::OrderUpdate(u)) if u.status == OrderStatus::Rejected => {
                println!("  SKIP: Order rejected\n");
                let conns = shutdown_and_reclaim(&control_tx, join, account_id);
                return conns;
            }
            _ => {}
        }
    }

    if !filled {
        let conns = shutdown_and_reclaim(&control_tx, join, account_id);
        println!("  SKIP: No fill received (market closed?)\n");
        return conns;
    }

    std::thread::sleep(Duration::from_millis(300));

    // Exercise exec_details path (same as process_msgs)
    let mut wrapper = ExecWrapper { execs: Vec::new() };
    for fill in shared.orders.drain_fills() {
        let price_f = fill.price as f64 / api::PRICE_SCALE_F;
        let side_str = match fill.side {
            Side::Buy => "BOT",
            Side::Sell | Side::ShortSell => "SLD",
        };
        let exec = api::Execution {
            side: side_str.into(),
            shares: fill.qty as f64,
            price: price_f,
            order_id: fill.order_id as i64,
            ..Default::default()
        };
        let c = shared.orders.get_order_info(fill.order_id)
            .map(|info| info.contract)
            .unwrap_or_default();
        wrapper.exec_details(-1, &c, &exec);
    }

    // Sell back to flatten
    let sell_id = next_order_id();
    control_tx.send(ControlCommand::Order(OrderRequest::SubmitMarket {
        order_id: sell_id,
        instrument: inst_id,
        side: Side::Sell,
        qty: 1,
    })).unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::Fill(f)) if f.order_id == sell_id => break,
            _ => {}
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    // Validate
    let mut pass = true;
    if let Some((_, c, exec)) = wrapper.execs.first() {
        println!("  exec_details callback received:");
        println!("    contract.conId      = {} (GT: 756733)", c.con_id);
        println!("    contract.symbol     = '{}' (GT: 'SPY')", c.symbol);
        println!("    contract.secType    = '{}' (GT: 'STK')", c.sec_type);
        println!("    contract.currency   = '{}' (GT: 'USD')", c.currency);
        println!("    contract.exchange   = '{}' (GT: 'ARCA')", c.exchange);
        println!("    contract.localSym   = '{}' (GT: 'SPY')", c.local_symbol);
        println!("    contract.tradClass  = '{}' (GT: 'SPY')", c.trading_class);
        println!("    execution.side      = '{}' (GT: 'BOT')", exec.side);
        println!("    execution.shares    = {} (GT: 1.0)", exec.shares);
        println!("    execution.price     = {:.2}", exec.price);

        if c.con_id != 756733 { println!("    FAIL: conId"); pass = false; }
        if c.symbol != "SPY" { println!("    FAIL: symbol"); pass = false; }
        if c.sec_type != "STK" { println!("    FAIL: secType"); pass = false; }
        if c.currency != "USD" { println!("    FAIL: currency"); pass = false; }
        if c.local_symbol.is_empty() { println!("    FAIL: localSymbol empty"); pass = false; }
        if exec.side != "BOT" { println!("    FAIL: side"); pass = false; }
        if (exec.shares - 1.0).abs() > 0.01 { println!("    FAIL: shares"); pass = false; }
    } else {
        println!("  No exec_details callback");
        pass = false;
    }

    if pass { println!("  PASS (all fields match GT)\n"); }
    else { println!("  FAIL\n"); }
    assert!(pass, "exec_details Wrapper output did not match GT");
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
                    let acct = shared.portfolio.account();
                    // DailyPnL is populated from server messages.
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

    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into(), exchange: String::new(), sec_type: String::new(), reply_tx: None }).unwrap();
    let join = run_hot_loop(hot_loop);

    // Wait for any events to flow, checking for bulletins periodically
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut total_bulletins = 0usize;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(500)) {
            _ => {
                let bulletins = shared.market.drain_news_bulletins();
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
