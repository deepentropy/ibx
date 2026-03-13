//! Order submission, modification, cancellation, and fill test phases.

use super::common::*;

// ─── Phase 6: Market order round-trip ───

pub(super) fn phase_market_order(conns: Conns) -> Conns {
    println!("--- Phase 6: Market Order Round-Trip (SPY) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared, Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(60);
    let mut tick_count = 0u32;
    let mut phase = 0u8;
    let mut buy_order_id;
    let mut sell_order_id;
    let mut buy_price = 0i64;
    let mut sell_price = 0i64;
    let mut buy_rtt_us = 0u64;
    let mut sell_rtt_us = 0u64;
    let mut buy_sent_at: Option<Instant> = None;
    let mut sell_sent_at: Option<Instant> = None;
    let mut order_rejected = false;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::Tick(instrument)) => {
                tick_count += 1;
                if phase == 0 && tick_count >= 5 {
                    buy_order_id = next_order_id();
                    control_tx.send(ControlCommand::Order(OrderRequest::SubmitMarket {
                        order_id: buy_order_id,
                        instrument,
                        side: Side::Buy,
                        qty: 1,
                    })).unwrap();
                    buy_sent_at = Some(Instant::now());
                    phase = 1;
                }
            }
            Ok(Event::Fill(fill)) => {
                if phase == 1 && fill.side == Side::Buy {
                    buy_price = fill.price;
                    buy_rtt_us = buy_sent_at.map(|t| t.elapsed().as_micros() as u64).unwrap_or(0);
                    sell_order_id = next_order_id() + 1;
                    control_tx.send(ControlCommand::Order(OrderRequest::SubmitMarket {
                        order_id: sell_order_id,
                        instrument: fill.instrument,
                        side: Side::Sell,
                        qty: 1,
                    })).unwrap();
                    sell_sent_at = Some(Instant::now());
                    phase = 2;
                } else if phase == 2 && fill.side == Side::Sell {
                    sell_price = fill.price;
                    sell_rtt_us = sell_sent_at.map(|t| t.elapsed().as_micros() as u64).unwrap_or(0);
                    let _ = phase;
                    break;
                }
            }
            Ok(Event::OrderUpdate(update)) => {
                if update.status == OrderStatus::Rejected {
                    order_rejected = true;
                    break;
                }
            }
            _ => {}
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if order_rejected {
        println!("  SKIP: Order rejected — market may be closed\n");
        return conns;
    }
    if buy_price == 0 {
        println!("  SKIP: No buy fill — market is closed\n");
        return conns;
    }
    assert!(sell_price > 0, "Buy filled but no sell fill received");

    println!("  Buy: ${:.4} (RTT {:.3}ms)", buy_price as f64 / PRICE_SCALE as f64, buy_rtt_us as f64 / 1000.0);
    println!("  Sell: ${:.4} (RTT {:.3}ms)", sell_price as f64 / PRICE_SCALE as f64, sell_rtt_us as f64 / 1000.0);
    println!("  Mean RTT: {:.3}ms", (buy_rtt_us + sell_rtt_us) as f64 / 2000.0);
    println!("  PASS\n");
    conns
}

// ─── Phase 7: Limit order submit + cancel ───

pub(super) fn phase_limit_order(conns: Conns) -> Conns {
    println!("--- Phase 7: Limit Order Submit + Cancel (SPY) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared, Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    let order_id = next_order_id();
    control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimit {
        order_id,
        instrument: inst_id,
        side: Side::Buy,
        qty: 1,
        price: 1_00_000_000,
    })).unwrap();
    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();

    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(60);
    let submitted = true;
    let mut order_acked = false;
    let mut cancel_sent = false;
    let mut order_cancelled = false;
    let mut order_rejected = false;
    let submit_time = Instant::now();
    let mut cancel_time: Option<Instant> = None;
    let mut submit_ack_us = 0u64;
    let mut cancel_conf_us = 0u64;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::OrderUpdate(update)) => {
                match update.status {
                    OrderStatus::Submitted => {
                        if !order_acked {
                            submit_ack_us = submit_time.elapsed().as_micros() as u64;
                            order_acked = true;
                        }
                        if !cancel_sent {
                            control_tx.send(ControlCommand::Order(OrderRequest::Cancel { order_id })).unwrap();
                            cancel_time = Some(Instant::now());
                            cancel_sent = true;
                        }
                    }
                    OrderStatus::Cancelled => {
                        cancel_conf_us = cancel_time.map(|t| t.elapsed().as_micros() as u64).unwrap_or(0);
                        order_cancelled = true;
                        break;
                    }
                    OrderStatus::Rejected => {
                        order_rejected = true;
                        break;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    let _ = submit_time; // suppress unused warning

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if order_rejected {
        println!("  SKIP: Order rejected — market may be closed\n");
        return conns;
    }

    assert!(submitted, "Order was never submitted");
    assert!(order_acked, "Order was never acknowledged");
    assert!(order_cancelled, "Order was never cancelled");

    println!("  Submit→Ack: {:.3}ms  Cancel→Conf: {:.3}ms", submit_ack_us as f64 / 1000.0, cancel_conf_us as f64 / 1000.0);
    println!("  PASS\n");
    conns
}

// ─── Phase 8: Stop order submit + cancel ───

pub(super) fn phase_stop_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 8: Stop Order Submit + Cancel (SPY)",
        OrderRequest::SubmitStop { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, stop_price: 1_00_000_000 },
        false)
}

// ─── Phase 9: Order modify (35=G) ───

pub(super) fn phase_modify_order(conns: Conns) -> Conns {
    println!("--- Phase 9: Order Modify (35=G) + Cancel (SPY) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared, Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    let order_id = next_order_id();
    control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimit {
        order_id, instrument: inst_id, side: Side::Buy, qty: 1, price: 1_00_000_000,
    })).unwrap();
    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(60);
    let mut order_acked = false;
    let mut modify_sent = false;
    let mut modify_acked = false;
    let mut order_cancelled = false;
    let mut order_rejected = false;
    let new_order_id = order_id + 1;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::OrderUpdate(update)) => {
                match update.status {
                    OrderStatus::Submitted => {
                        if modify_sent && !modify_acked {
                            modify_acked = true;
                            control_tx.send(ControlCommand::Order(OrderRequest::Cancel { order_id: new_order_id })).unwrap();
                        } else if !order_acked {
                            order_acked = true;
                            control_tx.send(ControlCommand::Order(OrderRequest::Modify {
                                order_id, new_order_id, price: 2_00_000_000, qty: 1,
                            })).unwrap();
                            modify_sent = true;
                        }
                    }
                    OrderStatus::Cancelled => { order_cancelled = true; break; }
                    OrderStatus::Rejected => { order_rejected = true; break; }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if order_rejected {
        println!("  SKIP: Modify test rejected\n");
        return conns;
    }
    assert!(order_acked, "Order was never acknowledged");
    assert!(modify_sent, "Modify was never sent");
    assert!(modify_acked, "Modify was never acknowledged");
    assert!(order_cancelled, "Modified order was never cancelled");
    println!("  PASS\n");
    conns
}

// ─── Phase 10: Outside RTH limit order ───

pub(super) fn phase_outside_rth(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 10: Outside RTH Limit Order (GTC+OutsideRTH, SPY)",
        OrderRequest::SubmitLimitGtc { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, price: 1_00_000_000, outside_rth: true },
        false)
}

// ─── Phase 15: Stop limit order submit + cancel ───

pub(super) fn phase_stop_limit_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 15: Stop Limit Order Submit + Cancel (SPY)",
        OrderRequest::SubmitStopLimit { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, price: 998_00_000_000, stop_price: 999_00_000_000 },
        false)
}

// ─── Phase 17: Commission tracking ───

pub(super) fn phase_commission(conns: Conns) -> Conns {
    println!("--- Phase 17: Commission Tracking (GTC+OutsideRTH fill) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared, Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    let buy_id = next_order_id();
    control_tx.send(ControlCommand::Order(OrderRequest::SubmitMarket {
        order_id: buy_id, instrument: inst_id, side: Side::Buy, qty: 1,
    })).unwrap();
    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(60);
    let mut phase = 1u8;
    let mut buy_price = 0i64;
    let mut buy_comm = 0i64;
    let mut sell_price = 0i64;
    let mut sell_comm = 0i64;
    let mut order_rejected = false;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::Fill(fill)) => {
                if phase == 1 && fill.side == Side::Buy {
                    buy_price = fill.price;
                    buy_comm = fill.commission;
                    let sid = next_order_id() + 1;
                    control_tx.send(ControlCommand::Order(OrderRequest::SubmitMarket {
                        order_id: sid, instrument: fill.instrument, side: Side::Sell, qty: 1,
                    })).unwrap();
                    phase = 2;
                } else if phase == 2 && fill.side == Side::Sell {
                    sell_price = fill.price;
                    sell_comm = fill.commission;
                    break;
                }
            }
            Ok(Event::OrderUpdate(update)) => {
                if update.status == OrderStatus::Rejected { order_rejected = true; break; }
            }
            _ => {}
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if order_rejected {
        println!("  SKIP: Order rejected — extended hours may not be active\n");
        return conns;
    }
    if buy_price == 0 {
        println!("  SKIP: No fill — market may not have liquidity\n");
        return conns;
    }
    println!("  Buy: ${:.4} commission=${:.4}", buy_price as f64 / PRICE_SCALE as f64, buy_comm as f64 / PRICE_SCALE as f64);
    println!("  Sell: ${:.4} commission=${:.4}", sell_price as f64 / PRICE_SCALE as f64, sell_comm as f64 / PRICE_SCALE as f64);
    if buy_comm == 0 {
        println!("  PASS (commission=0 — paper account does not report tag 12)\n");
    } else {
        println!("  PASS (commission=${:.4})\n", buy_comm as f64 / PRICE_SCALE as f64);
    }
    conns
}

// ─── Phase 10b: Outside RTH GTC Stop ───

pub(super) fn phase_outside_rth_stop(conns: Conns) -> Conns {
    println!("--- Phase 10b: Outside RTH GTC Stop Order (SPY) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared, Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    let order_id = next_order_id();
    control_tx.send(ControlCommand::Order(OrderRequest::SubmitStopGtc {
        order_id, instrument: inst_id, side: Side::Sell, qty: 1, stop_price: 1_00_000_000, outside_rth: true,
    })).unwrap();
    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut order_acked = false;
    let mut order_cancelled = false;
    let mut order_rejected = false;
    let mut cancel_sent = false;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::OrderUpdate(update)) => {
                match update.status {
                    OrderStatus::Submitted => {
                        order_acked = true;
                        if !cancel_sent {
                            control_tx.send(ControlCommand::Order(OrderRequest::Cancel { order_id })).unwrap();
                            cancel_sent = true;
                        }
                    }
                    OrderStatus::Cancelled => { order_cancelled = true; break; }
                    OrderStatus::Rejected => { order_rejected = true; break; }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if order_rejected {
        println!("  SKIP: GTC stop outside RTH rejected\n");
        return conns;
    }
    assert!(order_acked, "GTC stop outside RTH was never acknowledged");
    assert!(order_cancelled, "GTC stop outside RTH was never cancelled");
    println!("  PASS\n");
    conns
}

// ─── Phase 9b: Modify Order Qty ───

pub(super) fn phase_modify_qty(conns: Conns) -> Conns {
    println!("--- Phase 9b: Order Modify Qty (SPY) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared, Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    let order_id = next_order_id();
    let new_order_id = order_id + 1;
    control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimit {
        order_id, instrument: inst_id, side: Side::Buy, qty: 1, price: 1_00_000_000,
    })).unwrap();
    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(60);
    let mut order_acked = false;
    let mut modify_sent = false;
    let mut modify_acked_local = false;
    let mut order_cancelled = false;
    let mut order_rejected = false;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::OrderUpdate(update)) => {
                match update.status {
                    OrderStatus::Submitted => {
                        if modify_sent && !modify_acked_local {
                            modify_acked_local = true;
                            control_tx.send(ControlCommand::Order(OrderRequest::Cancel { order_id: new_order_id })).unwrap();
                        } else if !order_acked {
                            order_acked = true;
                            control_tx.send(ControlCommand::Order(OrderRequest::Modify {
                                order_id, new_order_id, price: 1_00_000_000, qty: 2,
                            })).unwrap();
                            modify_sent = true;
                        }
                    }
                    OrderStatus::Cancelled => { order_cancelled = true; break; }
                    OrderStatus::Rejected => { order_rejected = true; break; }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if order_rejected {
        println!("  SKIP: Modify qty test rejected\n");
        return conns;
    }
    assert!(order_acked, "Order was never acknowledged");
    assert!(modify_sent, "Modify was never sent");
    assert!(modify_acked_local, "Qty modify was never acknowledged");
    assert!(order_cancelled, "Modified order was never cancelled");
    println!("  PASS\n");
    conns
}

// ─── Phase 19: Trailing Stop ───

pub(super) fn phase_trailing_stop(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 19: Trailing Stop Order (SPY)",
        OrderRequest::SubmitTrailingStop { order_id: oid, instrument: 0, side: Side::Sell, qty: 1, trail_amt: 5_00_000_000 },
        false)
}

// ─── Phase 20: Trailing Stop Limit ───

pub(super) fn phase_trailing_stop_limit(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 20: Trailing Stop Limit Order (SPY)",
        OrderRequest::SubmitTrailingStopLimit { order_id: oid, instrument: 0, side: Side::Sell, qty: 1, price: 1_00_000_000, trail_amt: 5_00_000_000 },
        false)
}

// ─── Phase 21: Limit IOC ───

pub(super) fn phase_limit_ioc(conns: Conns) -> Conns {
    println!("--- Phase 21: Limit IOC Order (SPY) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared, Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    let order_id = next_order_id();
    control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimitIoc {
        order_id, instrument: inst_id, side: Side::Buy, qty: 1, price: 1_00_000_000,
    })).unwrap();
    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut order_cancelled = false;
    let mut order_rejected = false;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::OrderUpdate(update)) => {
                match update.status {
                    OrderStatus::Cancelled => { order_cancelled = true; break; }
                    OrderStatus::Rejected => { order_rejected = true; break; }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if order_rejected {
        println!("  SKIP: IOC order rejected\n");
        return conns;
    }
    assert!(order_cancelled, "IOC order was not cancelled (should expire immediately at $1)");
    println!("  PASS (IOC cancelled as expected — no fill at $1)\n");
    conns
}

// ─── Phase 22: Limit FOK ───

pub(super) fn phase_limit_fok(conns: Conns) -> Conns {
    println!("--- Phase 22: Limit FOK Order (SPY) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared, Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    let order_id = next_order_id();
    control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimitFok {
        order_id, instrument: inst_id, side: Side::Buy, qty: 1, price: 1_00_000_000,
    })).unwrap();
    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut order_cancelled = false;
    let mut order_rejected = false;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::OrderUpdate(update)) => {
                match update.status {
                    OrderStatus::Cancelled => { order_cancelled = true; break; }
                    OrderStatus::Rejected => { order_rejected = true; break; }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if order_rejected {
        println!("  SKIP: FOK order rejected\n");
        return conns;
    }
    assert!(order_cancelled, "FOK order was not cancelled (should expire immediately at $1)");
    println!("  PASS (FOK cancelled as expected — no fill at $1)\n");
    conns
}

// ─── Phase 23: Stop GTC ───

pub(super) fn phase_stop_gtc(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 23: Stop GTC Order (SPY)",
        OrderRequest::SubmitStopGtc { order_id: oid, instrument: 0, side: Side::Sell, qty: 1, stop_price: 1_00_000_000, outside_rth: true },
        false)
}

// ─── Phase 24: Stop Limit GTC ───

pub(super) fn phase_stop_limit_gtc(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 24: Stop Limit GTC Order (SPY)",
        OrderRequest::SubmitStopLimitGtc { order_id: oid, instrument: 0, side: Side::Sell, qty: 1, price: 1_00_000_000, stop_price: 1_00_000_000, outside_rth: true },
        false)
}

// ─── Phase 25: Market if Touched ───

pub(super) fn phase_mit_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 25: Market if Touched Order (SPY)",
        OrderRequest::SubmitMit { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, stop_price: 1_00_000_000 },
        false)
}

// ─── Phase 26: Limit if Touched ───

pub(super) fn phase_lit_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 26: Limit if Touched Order (SPY)",
        OrderRequest::SubmitLit { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, price: 2_00_000_000, stop_price: 1_00_000_000 },
        false)
}

// ─── Phase 27: Market on Close ───

pub(super) fn phase_moc_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 27: MOC Order (SPY)",
        OrderRequest::SubmitMoc { order_id: oid, instrument: 0, side: Side::Buy, qty: 1 },
        false)
}

// ─── Phase 28: Limit on Close ───

pub(super) fn phase_loc_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 28: LOC Order (SPY)",
        OrderRequest::SubmitLoc { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, price: 1_00_000_000 },
        false)
}

// ─── Phase 29: Bracket Order ───

pub(super) fn phase_bracket_order(conns: Conns) -> Conns {
    println!("--- Phase 29: Bracket Order (SPY) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared, Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    let parent_id = next_order_id();
    let tp_id = parent_id + 1;
    let sl_id = parent_id + 2;
    control_tx.send(ControlCommand::Order(OrderRequest::SubmitBracket {
        parent_id, tp_id, sl_id, instrument: inst_id, side: Side::Buy, qty: 1,
        entry_price: 1_00_000_000, take_profit: 2_00_000_000, stop_loss: 50_000_000,
    })).unwrap();
    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(60);
    let mut parent_acked = false;
    let mut any_rejected = false;
    let mut cancelled_count = 0u32;
    let mut cancel_sent = false;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::OrderUpdate(update)) => {
                match update.status {
                    OrderStatus::Submitted => {
                        if update.order_id == parent_id { parent_acked = true; }
                        if parent_acked && !cancel_sent {
                            control_tx.send(ControlCommand::Order(OrderRequest::Cancel { order_id: parent_id })).unwrap();
                            cancel_sent = true;
                        }
                    }
                    OrderStatus::Cancelled => {
                        cancelled_count += 1;
                        if cancelled_count >= 1 { break; }
                    }
                    OrderStatus::Rejected => { any_rejected = true; break; }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if any_rejected {
        println!("  SKIP: Bracket order rejected\n");
        return conns;
    }
    assert!(parent_acked, "Parent order was never acknowledged");
    println!("  Parent acked: {}, Cancelled: {} orders", parent_acked, cancelled_count);
    println!("  PASS\n");
    conns
}

// ─── Phase 30: Adaptive Algo Limit ───

pub(super) fn phase_adaptive_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 30: Adaptive Algo Limit Order (SPY)",
        OrderRequest::SubmitAdaptive { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, price: 1_00_000_000, priority: AdaptivePriority::Normal },
        false)
}

// ─── Phase 31: Relative / Pegged-to-Primary ───

pub(super) fn phase_rel_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 31: Relative Order (SPY)",
        OrderRequest::SubmitRel { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, offset: 1_000_000 },
        false)
}

// ─── Phase 32: Limit OPG ───

pub(super) fn phase_limit_opg(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 32: Limit OPG Order (SPY)",
        OrderRequest::SubmitLimitOpg { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, price: 1_00_000_000 },
        false)
}

// ─── Phase 33: Iceberg ───

pub(super) fn phase_iceberg_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 33: Iceberg Order (SPY)",
        OrderRequest::SubmitLimitEx { order_id: oid, instrument: 0, side: Side::Buy, qty: 10, price: 1_00_000_000, tif: b'1', attrs: OrderAttrs { display_size: 1, outside_rth: true, ..OrderAttrs::default() } },
        false)
}

// ─── Phase 34: Hidden ───

pub(super) fn phase_hidden_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 34: Hidden Order (SPY)",
        OrderRequest::SubmitLimitEx { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, price: 1_00_000_000, tif: b'1', attrs: OrderAttrs { hidden: true, outside_rth: true, ..OrderAttrs::default() } },
        false)
}

// ─── Phase 35: Short Sell ───

pub(super) fn phase_short_sell(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 35: Short Sell Limit Order (SPY)",
        OrderRequest::SubmitLimitEx { order_id: oid, instrument: 0, side: Side::ShortSell, qty: 1, price: 1_00_000_000, tif: b'0', attrs: OrderAttrs::default() },
        false)
}

// ─── Phase 36: Trailing Stop Percent ───

pub(super) fn phase_trailing_stop_pct(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 36: Trailing Stop Percent Order (SPY)",
        OrderRequest::SubmitTrailingStopPct { order_id: oid, instrument: 0, side: Side::Sell, qty: 1, trail_pct: 100 },
        false)
}

// ─── Phase 37: OCA Group ───

pub(super) fn phase_oca_group(conns: Conns) -> Conns {
    println!("--- Phase 37: OCA Group (SPY) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared, Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    let oca = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64;
    let id1 = next_order_id();
    let id2 = id1 + 1;
    control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimitEx {
        order_id: id1, instrument: inst_id, side: Side::Buy, qty: 1, price: 1_00_000_000, tif: b'1',
        attrs: OrderAttrs { oca_group: oca, outside_rth: true, ..OrderAttrs::default() },
    })).unwrap();
    control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimitEx {
        order_id: id2, instrument: inst_id, side: Side::Buy, qty: 1, price: 2_00_000_000, tif: b'1',
        attrs: OrderAttrs { oca_group: oca, outside_rth: true, ..OrderAttrs::default() },
    })).unwrap();
    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(60);
    let mut order1_acked = false;
    let mut order2_acked = false;
    let mut any_rejected = false;
    let mut cancelled_count = 0u32;
    let mut cancel_sent = false;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::OrderUpdate(update)) => {
                match update.status {
                    OrderStatus::Submitted => {
                        if update.order_id == id1 { order1_acked = true; }
                        if update.order_id == id2 { order2_acked = true; }
                        if order1_acked && order2_acked && !cancel_sent {
                            control_tx.send(ControlCommand::Order(OrderRequest::Cancel { order_id: id1 })).unwrap();
                            cancel_sent = true;
                        }
                    }
                    OrderStatus::Cancelled => {
                        cancelled_count += 1;
                        if cancelled_count >= 1 { break; }
                    }
                    OrderStatus::Rejected => { any_rejected = true; break; }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if any_rejected {
        println!("  SKIP: OCA order rejected\n");
        return conns;
    }
    assert!(order1_acked, "Order 1 never acked");
    assert!(order2_acked, "Order 2 never acked");
    println!("  Order1 acked: {}, Order2 acked: {}, Cancelled: {}", order1_acked, order2_acked, cancelled_count);
    println!("  PASS\n");
    conns
}

// ─── Phase 38: Market to Limit ───

pub(super) fn phase_mtl_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 38: Market to Limit Order (SPY)",
        OrderRequest::SubmitMtl { order_id: oid, instrument: 0, side: Side::Buy, qty: 1 },
        true)
}

// ─── Phase 39: Market with Protection ───

pub(super) fn phase_mkt_prt_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 39: Market with Protection Order (SPY)",
        OrderRequest::SubmitMktPrt { order_id: oid, instrument: 0, side: Side::Buy, qty: 1 },
        true)
}

// ─── Phase 40: Stop with Protection ───

pub(super) fn phase_stp_prt_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 40: Stop with Protection Order (SPY)",
        OrderRequest::SubmitStpPrt { order_id: oid, instrument: 0, side: Side::Sell, qty: 1, stop_price: 1_00_000_000 },
        false)
}

// ─── Phase 41: Mid-Price ───

pub(super) fn phase_mid_price_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 41: Mid-Price Order (SPY)",
        OrderRequest::SubmitMidPrice { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, price_cap: 1_00_000_000 },
        false)
}

// ─── Phase 42: Snap to Market ───

pub(super) fn phase_snap_mkt_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 42: Snap to Market Order (SPY)",
        OrderRequest::SubmitSnapMkt { order_id: oid, instrument: 0, side: Side::Buy, qty: 1 },
        true)
}

// ─── Phase 43: Snap to Midpoint ───

pub(super) fn phase_snap_mid_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 43: Snap to Midpoint Order (SPY)",
        OrderRequest::SubmitSnapMid { order_id: oid, instrument: 0, side: Side::Buy, qty: 1 },
        true)
}

// ─── Phase 44: Snap to Primary ───

pub(super) fn phase_snap_pri_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 44: Snap to Primary Order (SPY)",
        OrderRequest::SubmitSnapPri { order_id: oid, instrument: 0, side: Side::Buy, qty: 1 },
        true)
}

// ─── Phase 45: Pegged to Market ───

pub(super) fn phase_peg_mkt_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 45: Pegged to Market Order (SPY)",
        OrderRequest::SubmitPegMkt { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, offset: 0 },
        true)
}

// ─── Phase 46: Pegged to Midpoint ───

pub(super) fn phase_peg_mid_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 46: Pegged to Midpoint Order (SPY)",
        OrderRequest::SubmitPegMid { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, offset: 0 },
        true)
}

// ─── Phase 47: Discretionary Amount ───

pub(super) fn phase_discretionary_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 47: Discretionary Amount Order (SPY)",
        OrderRequest::SubmitLimitEx { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, price: 1_00_000_000, tif: b'1', attrs: OrderAttrs { discretionary_amt: 50_000_000, outside_rth: true, ..OrderAttrs::default() } },
        false)
}

// ─── Phase 48: Sweep to Fill ───

pub(super) fn phase_sweep_to_fill_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 48: Sweep to Fill Order (SPY)",
        OrderRequest::SubmitLimitEx { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, price: 1_00_000_000, tif: b'1', attrs: OrderAttrs { sweep_to_fill: true, outside_rth: true, ..OrderAttrs::default() } },
        false)
}

// ─── Phase 49: All or None ───

pub(super) fn phase_all_or_none_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 49: All or None Order (SPY)",
        OrderRequest::SubmitLimitEx { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, price: 1_00_000_000, tif: b'1', attrs: OrderAttrs { all_or_none: true, outside_rth: true, ..OrderAttrs::default() } },
        false)
}

// ─── Phase 50: Trigger Method ───

pub(super) fn phase_trigger_method_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 50: Trigger Method Order (SPY)",
        OrderRequest::SubmitLimitEx { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, price: 1_00_000_000, tif: b'1', attrs: OrderAttrs { trigger_method: 2, outside_rth: true, ..OrderAttrs::default() } },
        false)
}

// ─── Phase 57: Price Condition Order ───

pub(super) fn phase_price_condition_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 57: Price Condition Order (SPY)",
        OrderRequest::SubmitLimitEx { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, price: 1_00_000_000, tif: b'1',
            attrs: OrderAttrs { outside_rth: true, conditions: vec![OrderCondition::Price { con_id: 756733, exchange: "BEST".into(), price: 1_00_000_000, is_more: false, trigger_method: 0 }], ..OrderAttrs::default() } },
        false)
}

// ─── Phase 58: Time Condition Order ───

pub(super) fn phase_time_condition_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 58: Time Condition Order (SPY)",
        OrderRequest::SubmitLimitEx { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, price: 1_00_000_000, tif: b'1',
            attrs: OrderAttrs { outside_rth: true, conditions: vec![OrderCondition::Time { time: "20991231-23:59:59".into(), is_more: false }], ..OrderAttrs::default() } },
        false)
}

// ─── Phase 59: Volume Condition Order ───

pub(super) fn phase_volume_condition_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 59: Volume Condition Order (SPY)",
        OrderRequest::SubmitLimitEx { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, price: 1_00_000_000, tif: b'1',
            attrs: OrderAttrs { outside_rth: true, conditions: vec![OrderCondition::Volume { con_id: 756733, exchange: "BEST".into(), volume: 999_999_999, is_more: true }], ..OrderAttrs::default() } },
        false)
}

// ─── Phase 60: Multi-Condition Order ───

pub(super) fn phase_multi_condition_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 60: Multi-Condition Order (SPY)",
        OrderRequest::SubmitLimitEx { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, price: 1_00_000_000, tif: b'1',
            attrs: OrderAttrs {
                outside_rth: true,
                conditions: vec![
                    OrderCondition::Price { con_id: 756733, exchange: "BEST".into(), price: 1_00_000_000, is_more: false, trigger_method: 2 },
                    OrderCondition::Volume { con_id: 756733, exchange: "BEST".into(), volume: 999_999_999, is_more: true },
                ],
                conditions_cancel_order: true,
                ..OrderAttrs::default()
            } },
        false)
}

// ─── Phase 62: VWAP Algo ───

pub(super) fn phase_vwap_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 62: VWAP Algo Order (SPY)",
        OrderRequest::SubmitAlgo { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, price: 1_00_000_000,
            algo: AlgoParams::Vwap { max_pct_vol: 0.1, no_take_liq: false, allow_past_end_time: true, start_time: "20260311-13:30:00".into(), end_time: "20260311-20:00:00".into() } },
        false)
}

// ─── Phase 63: TWAP Algo ───

pub(super) fn phase_twap_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 63: TWAP Algo Order (SPY)",
        OrderRequest::SubmitAlgo { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, price: 1_00_000_000,
            algo: AlgoParams::Twap { allow_past_end_time: true, start_time: "20260311-13:30:00".into(), end_time: "20260311-20:00:00".into() } },
        false)
}

// ─── Phase 64: Arrival Price Algo ───

pub(super) fn phase_arrival_px_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 64: Arrival Price Algo Order (SPY)",
        OrderRequest::SubmitAlgo { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, price: 1_00_000_000,
            algo: AlgoParams::ArrivalPx { max_pct_vol: 0.1, risk_aversion: RiskAversion::Neutral, allow_past_end_time: true, force_completion: false, start_time: "20260311-13:30:00".into(), end_time: "20260311-20:00:00".into() } },
        false)
}

// ─── Phase 65: Close Price Algo ───

pub(super) fn phase_close_px_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 65: Close Price Algo Order (SPY)",
        OrderRequest::SubmitAlgo { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, price: 1_00_000_000,
            algo: AlgoParams::ClosePx { max_pct_vol: 0.1, risk_aversion: RiskAversion::Neutral, force_completion: false, start_time: "20260311-13:30:00".into() } },
        false)
}

// ─── Phase 66: Dark Ice Algo ───

pub(super) fn phase_dark_ice_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 66: Dark Ice Algo Order (SPY)",
        OrderRequest::SubmitAlgo { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, price: 1_00_000_000,
            algo: AlgoParams::DarkIce { allow_past_end_time: true, display_size: 1, start_time: "20260311-13:30:00".into(), end_time: "20260311-20:00:00".into() } },
        false)
}

// ─── Phase 67: % of Volume Algo ───

pub(super) fn phase_pct_vol_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 67: % of Volume Algo Order (SPY)",
        OrderRequest::SubmitAlgo { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, price: 1_00_000_000,
            algo: AlgoParams::PctVol { pct_vol: 0.1, no_take_liq: false, start_time: "20260311-13:30:00".into(), end_time: "20260311-20:00:00".into() } },
        false)
}

// ─── Phase 68: Pegged to Benchmark ───

pub(super) fn phase_peg_bench_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 68: Pegged to Benchmark Order (SPY pegged to AAPL)",
        OrderRequest::SubmitPegBench { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, price: 1_00_000_000, ref_con_id: 265598, is_peg_decrease: false, pegged_change_amount: 50_000_000, ref_change_amount: 50_000_000 },
        false)
}

// ─── Phase 69: Limit Auction ───

pub(super) fn phase_limit_auc_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 69: Limit Auction Order (SPY)",
        OrderRequest::SubmitLimitAuc { order_id: oid, instrument: 0, side: Side::Buy, qty: 1, price: 1_00_000_000 },
        false)
}

// ─── Phase 70: MTL Auction ───

pub(super) fn phase_mtl_auc_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 70: Market-to-Limit Auction Order (SPY)",
        OrderRequest::SubmitMtlAuc { order_id: oid, instrument: 0, side: Side::Buy, qty: 1 },
        false)
}

// ─── Phase 71: Box Top (wire-identical to MTL) ───

pub(super) fn phase_box_top_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 71: Box Top Order (SPY)",
        OrderRequest::SubmitMtl { order_id: oid, instrument: 0, side: Side::Buy, qty: 1 },
        true)
}

// ─── Phase 72: What-If Order ───

pub(super) fn phase_what_if_order(conns: Conns) -> Conns {
    println!("--- Phase 72: What-If Order (SPY) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared, Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    let order_id = next_order_id();
    control_tx.send(ControlCommand::Order(OrderRequest::SubmitWhatIf {
        order_id, instrument: inst_id, side: Side::Buy, qty: 100, price: 1_00_000_000,
    })).unwrap();
    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(60);
    let mut what_if_received = false;
    let mut commission = 0i64;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::WhatIf(response)) => {
                commission = response.commission;
                what_if_received = true;
                break;
            }
            _ => {}
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    assert!(what_if_received, "What-if response was never received");
    assert!(commission > 0, "Commission should be > 0, got {}", commission);
    println!("  Commission: ${:.2}", commission as f64 / PRICE_SCALE as f64);
    println!("  PASS\n");
    conns
}

// ─── Phase 73: Cash Quantity Order ───

pub(super) fn phase_cash_qty_order(conns: Conns) -> Conns {
    println!("--- Phase 73: Cash Quantity Order (SPY) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared, Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    let order_id = next_order_id();
    control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimitEx {
        order_id, instrument: inst_id, side: Side::Buy, qty: 100, price: 1_00_000_000, tif: b'0',
        attrs: OrderAttrs { cash_qty: 1000 * PRICE_SCALE, ..OrderAttrs::default() },
    })).unwrap();
    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(60);
    let mut order_acked = false;
    let mut order_cancelled = false;
    let mut order_rejected = false;
    let mut cancel_sent = false;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::OrderUpdate(update)) => {
                match update.status {
                    OrderStatus::Submitted => {
                        order_acked = true;
                        if !cancel_sent {
                            control_tx.send(ControlCommand::Order(OrderRequest::Cancel { order_id })).unwrap();
                            cancel_sent = true;
                        }
                    }
                    OrderStatus::Cancelled => { order_cancelled = true; break; }
                    OrderStatus::Rejected => { order_rejected = true; break; }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if order_rejected {
        println!("  SKIP: Cash qty rejected (expected on paper account)\n");
        return conns;
    }
    assert!(order_acked, "Order was never acknowledged");
    assert!(order_cancelled, "Order was never cancelled");
    println!("  PASS\n");
    conns
}

// ─── Phase 74: Fractional Shares Order ───

pub(super) fn phase_fractional_order(conns: Conns) -> Conns {
    println!("--- Phase 74: Fractional Shares Order (SPY) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared, Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    let order_id = next_order_id();
    control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimitFractional {
        order_id, instrument: inst_id, side: Side::Buy, qty: QTY_SCALE / 2, price: 1_00_000_000,
    })).unwrap();
    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(60);
    let mut order_acked = false;
    let mut order_cancelled = false;
    let mut order_rejected = false;
    let mut cancel_sent = false;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::OrderUpdate(update)) => {
                match update.status {
                    OrderStatus::Submitted => {
                        order_acked = true;
                        if !cancel_sent {
                            control_tx.send(ControlCommand::Order(OrderRequest::Cancel { order_id })).unwrap();
                            cancel_sent = true;
                        }
                    }
                    OrderStatus::Cancelled => { order_cancelled = true; break; }
                    OrderStatus::Rejected => { order_rejected = true; break; }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if order_rejected {
        println!("  SKIP: Fractional rejected (may be blocked by CCP)\n");
        return conns;
    }
    assert!(order_acked, "Order was never acknowledged");
    assert!(order_cancelled, "Order was never cancelled");
    println!("  PASS\n");
    conns
}

// ─── Phase 75: Adjustable Stop ───

pub(super) fn phase_adjustable_stop_order(conns: Conns) -> Conns {
    let oid = next_order_id();
    run_submit_cancel_phase(conns, "Phase 75: Adjustable Stop Order (SPY)",
        OrderRequest::SubmitAdjustableStop { order_id: oid, instrument: 0, side: Side::Sell, qty: 1, stop_price: 1_00_000_000, trigger_price: 500_00_000_000, adjusted_order_type: AdjustedOrderType::StopLimit, adjusted_stop_price: 1_50_000_000, adjusted_stop_limit_price: 1_00_000_000 },
        false)
}

// ─── Phase 51: Bracket Fill Cascade ───

pub(super) fn phase_bracket_fill_cascade(conns: Conns) -> Conns {
    println!("--- Phase 51: Bracket Fill Cascade (SPY) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());
    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(60);
    let mut tick_count = 0u32;
    let mut parent_id: Option<u64> = None;
    let mut tp_id: Option<u64> = None;
    let mut sl_id: Option<u64> = None;
    let mut entry_filled = false;
    let mut tp_active = false;
    let mut sl_active = false;
    let mut cancelled_count = 0u32;
    let mut cancel_sent = false;
    let mut any_rejected = false;
    let mut done = false;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::Tick(_)) => {
                tick_count += 1;
                if tick_count == 5 && parent_id.is_none() {
                    let q = shared.quote(inst_id);
                    if q.ask <= 0 { continue; }
                    let entry = q.ask + 1_00_000_000;
                    let pid = next_order_id();
                    let tid = pid + 1;
                    let sid = pid + 2;
                    control_tx.send(ControlCommand::Order(OrderRequest::SubmitBracket {
                        parent_id: pid, tp_id: tid, sl_id: sid,
                        instrument: inst_id, side: Side::Buy, qty: 1,
                        entry_price: entry,
                        take_profit: entry + 100_00_000_000,
                        stop_loss: 1_000_000,
                    })).unwrap();
                    parent_id = Some(pid);
                    tp_id = Some(tid);
                    sl_id = Some(sid);
                }
            }
            Ok(Event::Fill(fill)) => {
                if Some(fill.order_id) == parent_id { entry_filled = true; }
                if cancel_sent && Some(fill.order_id) != parent_id { done = true; break; }
            }
            Ok(Event::OrderUpdate(update)) => {
                match update.status {
                    OrderStatus::Submitted => {
                        if Some(update.order_id) == tp_id { tp_active = true; }
                        if Some(update.order_id) == sl_id { sl_active = true; }
                        if tp_active && sl_active && !cancel_sent {
                            if let Some(t) = tp_id { control_tx.send(ControlCommand::Order(OrderRequest::Cancel { order_id: t })).unwrap(); }
                            if let Some(s) = sl_id { control_tx.send(ControlCommand::Order(OrderRequest::Cancel { order_id: s })).unwrap(); }
                            cancel_sent = true;
                        }
                    }
                    OrderStatus::Cancelled => {
                        cancelled_count += 1;
                        if cancelled_count >= 2 {
                            let sid = next_order_id() + 10;
                            control_tx.send(ControlCommand::Order(OrderRequest::SubmitMarket {
                                order_id: sid, instrument: inst_id, side: Side::Sell, qty: 1,
                            })).unwrap();
                        }
                    }
                    OrderStatus::Rejected => { any_rejected = true; break; }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    let _ = done;

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if any_rejected {
        println!("  SKIP: Bracket fill cascade rejected\n");
        return conns;
    }
    println!("  Entry filled: {}, TP active: {}, SL active: {}", entry_filled, tp_active, sl_active);
    if !entry_filled {
        println!("  SKIP: Entry did not fill — market may not have liquidity\n");
        return conns;
    }
    assert!(tp_active, "Take-profit child was never activated after entry fill");
    assert!(sl_active, "Stop-loss child was never activated after entry fill");
    println!("  PASS\n");
    conns
}

// ─── Phase 52: PnL After Round Trip ───

pub(super) fn phase_pnl_after_round_trip(conns: Conns) -> Conns {
    println!("--- Phase 52: PnL After Round Trip (SPY) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());
    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    let initial_rpnl = shared.account().realized_pnl;
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut tick_count = 0u32;
    let mut phase = 0u8;
    let mut buy_filled = false;
    let mut sell_filled = false;
    let mut pnl_updated = false;
    let mut order_rejected = false;
    let mut realized_pnl = 0i64;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::Tick(_)) => {
                tick_count += 1;
                if phase == 0 && tick_count >= 5 {
                    let oid = next_order_id();
                    control_tx.send(ControlCommand::Order(OrderRequest::SubmitMarket {
                        order_id: oid, instrument: inst_id, side: Side::Buy, qty: 1,
                    })).unwrap();
                    phase = 1;
                }
                if phase == 3 {
                    let current = shared.account().realized_pnl;
                    if current != initial_rpnl {
                        realized_pnl = current;
                        pnl_updated = true;
                        break;
                    }
                }
            }
            Ok(Event::Fill(fill)) => {
                if phase == 1 && fill.side == Side::Buy {
                    buy_filled = true;
                    let sid = next_order_id() + 1;
                    control_tx.send(ControlCommand::Order(OrderRequest::SubmitMarket {
                        order_id: sid, instrument: fill.instrument, side: Side::Sell, qty: 1,
                    })).unwrap();
                    phase = 2;
                } else if phase == 2 && fill.side == Side::Sell {
                    sell_filled = true;
                    phase = 3;
                }
            }
            Ok(Event::OrderUpdate(update)) => {
                if update.status == OrderStatus::Rejected { order_rejected = true; break; }
            }
            _ => {}
        }
    }

    if sell_filled && !pnl_updated {
        let extra = Instant::now() + Duration::from_secs(5);
        while Instant::now() < extra {
            let current = shared.account().realized_pnl;
            if current != initial_rpnl { realized_pnl = current; pnl_updated = true; break; }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if order_rejected { println!("  SKIP: Order rejected\n"); return conns; }
    if !buy_filled { println!("  SKIP: No fill — market may not have liquidity\n"); return conns; }

    println!("  Buy filled: {}, Sell filled: {}", buy_filled, sell_filled);
    if pnl_updated {
        println!("  RealizedPnL changed: ${:.2}", realized_pnl as f64 / PRICE_SCALE as f64);
        println!("  PASS\n");
    } else {
        println!("  PASS (PnL not yet updated — paper account delay is expected)\n");
    }
    conns
}

// ─── Phase 87: CancelReject Event path (issue #78) ───

pub(super) fn phase_cancel_reject(conns: Conns) -> Conns {
    println!("--- Phase 87: CancelReject Event (bogus order cancel) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared, Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    // Register instrument and submit a real order so there's a known order in context
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    let order_id = next_order_id();
    control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimitGtc {
        order_id, instrument: inst_id, side: Side::Buy, qty: 1,
        price: 1_00_000_000, outside_rth: true,
    })).unwrap();
    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    // Wait for order ack, then cancel it twice — second cancel should produce CancelReject
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut order_acked = false;
    let mut _first_cancel_sent = false;
    let mut first_cancelled = false;
    let mut _second_cancel_sent = false;
    let mut got_reject = false;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::OrderUpdate(update)) => {
                if update.status == OrderStatus::Submitted && !order_acked {
                    order_acked = true;
                    control_tx.send(ControlCommand::Order(OrderRequest::Cancel { order_id })).unwrap();
                    _first_cancel_sent = true;
                }
                if update.status == OrderStatus::Cancelled && !first_cancelled {
                    first_cancelled = true;
                    // Cancel again — order is already dead, should produce reject
                    control_tx.send(ControlCommand::Order(OrderRequest::Cancel { order_id })).unwrap();
                    _second_cancel_sent = true;
                }
            }
            Ok(Event::CancelReject(reject)) => {
                println!("  CancelReject: order_id={} type={} code={}", reject.order_id, reject.reject_type, reject.reason_code);
                got_reject = true;
                break;
            }
            _ => {}
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if !order_acked {
        println!("  SKIP: Order never acknowledged\n");
        return conns;
    }
    if got_reject {
        println!("  PASS\n");
    } else {
        // CancelReject may not be emitted if IB silently ignores the second cancel
        println!("  SKIP: No CancelReject received (IB may silently ignore duplicate cancel)\n");
    }
    conns
}

// ─── Phase 113: Rapid order dedup and interleaving (issue #100) ───

pub(super) fn phase_rapid_order_dedup(conns: Conns) -> Conns {
    println!("--- Phase 113: Rapid Order Submission + Dedup (5 orders, SPY) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared, Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    // Submit 5 limit orders rapidly at different prices
    let base_oid = next_order_id();
    let order_ids: Vec<u64> = (0..5).map(|i| base_oid + i * 1000).collect();
    for (i, &oid) in order_ids.iter().enumerate() {
        let price = (1 + i as i64) * 1_00_000_000; // $1, $2, $3, $4, $5
        control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimit {
            order_id: oid, instrument: inst_id, side: Side::Buy, qty: 1, price,
        })).unwrap();
    }
    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(60);
    let mut acked: std::collections::HashSet<u64> = std::collections::HashSet::new();
    let mut cancelled: std::collections::HashSet<u64> = std::collections::HashSet::new();
    let mut rejected: std::collections::HashSet<u64> = std::collections::HashSet::new();
    let mut cancel_batch_sent = false;
    let mut duplicate_acks = 0u32;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::OrderUpdate(update)) => {
                match update.status {
                    OrderStatus::Submitted => {
                        if acked.contains(&update.order_id) {
                            duplicate_acks += 1;
                        }
                        acked.insert(update.order_id);
                        // Once all 5 are acked, cancel them all
                        if acked.len() == 5 && !cancel_batch_sent {
                            for &oid in &order_ids {
                                control_tx.send(ControlCommand::Order(OrderRequest::Cancel { order_id: oid })).unwrap();
                            }
                            cancel_batch_sent = true;
                        }
                    }
                    OrderStatus::Cancelled => {
                        cancelled.insert(update.order_id);
                        if cancelled.len() + rejected.len() >= order_ids.len() { break; }
                    }
                    OrderStatus::Rejected => {
                        rejected.insert(update.order_id);
                        if cancelled.len() + rejected.len() >= order_ids.len() { break; }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    println!("  Acked: {} Cancelled: {} Rejected: {} Duplicate acks: {}",
        acked.len(), cancelled.len(), rejected.len(), duplicate_acks);

    if rejected.len() == order_ids.len() {
        println!("  SKIP: All orders rejected\n");
        return conns;
    }

    assert_eq!(duplicate_acks, 0, "No duplicate OrderUpdate(Submitted) for same order_id");
    assert!(acked.len() >= 3, "At least 3 of 5 orders should be acknowledged, got {}", acked.len());
    println!("  PASS\n");
    conns
}

// ─── Phase 115: Modify both price and qty simultaneously ───

pub(super) fn phase_modify_price_and_qty(conns: Conns) -> Conns {
    println!("--- Phase 115: Modify Price + Qty Simultaneously (SPY) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared, Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    let order_id = next_order_id();
    let new_order_id = order_id + 1;
    // Submit limit buy at $1, qty=1
    control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimit {
        order_id, instrument: inst_id, side: Side::Buy, qty: 1, price: 1_00_000_000,
    })).unwrap();
    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(60);
    let mut order_acked = false;
    let mut modify_sent = false;
    let mut modify_acked = false;
    let mut order_cancelled = false;
    let mut order_rejected = false;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::OrderUpdate(update)) => {
                match update.status {
                    OrderStatus::Submitted => {
                        if modify_sent && !modify_acked {
                            modify_acked = true;
                            control_tx.send(ControlCommand::Order(OrderRequest::Cancel { order_id: new_order_id })).unwrap();
                        } else if !order_acked {
                            order_acked = true;
                            // Modify BOTH price ($1→$2) and qty (1→3) in a single Modify
                            control_tx.send(ControlCommand::Order(OrderRequest::Modify {
                                order_id, new_order_id, price: 2_00_000_000, qty: 3,
                            })).unwrap();
                            modify_sent = true;
                        }
                    }
                    OrderStatus::Cancelled => { order_cancelled = true; break; }
                    OrderStatus::Rejected => { order_rejected = true; break; }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if order_rejected {
        println!("  SKIP: Order rejected\n");
        return conns;
    }
    assert!(order_acked, "Order was never acknowledged");
    assert!(modify_sent, "Modify was never sent");
    assert!(modify_acked, "Modify (price+qty) was never acknowledged");
    assert!(order_cancelled, "Modified order was never cancelled");
    println!("  PASS\n");
    conns
}

// ─── Phase 116: Double modify chain ───

pub(super) fn phase_double_modify(conns: Conns) -> Conns {
    println!("--- Phase 116: Double Modify Chain (SPY: $1→$2→$3) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared, Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    let order_id = next_order_id();
    let modify_id_1 = order_id + 1;
    let modify_id_2 = order_id + 2;

    // Submit limit buy at $1
    control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimit {
        order_id, instrument: inst_id, side: Side::Buy, qty: 1, price: 1_00_000_000,
    })).unwrap();
    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(90);
    let mut phase = 0u8; // 0=waiting for ack, 1=waiting for modify1 ack, 2=waiting for modify2 ack
    let mut order_cancelled = false;
    let mut order_rejected = false;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::OrderUpdate(update)) => {
                match update.status {
                    OrderStatus::Submitted => {
                        match phase {
                            0 => {
                                // Original order acked → modify to $2
                                control_tx.send(ControlCommand::Order(OrderRequest::Modify {
                                    order_id, new_order_id: modify_id_1, price: 2_00_000_000, qty: 1,
                                })).unwrap();
                                phase = 1;
                            }
                            1 => {
                                // First modify acked → modify again to $3
                                control_tx.send(ControlCommand::Order(OrderRequest::Modify {
                                    order_id: modify_id_1, new_order_id: modify_id_2, price: 3_00_000_000, qty: 1,
                                })).unwrap();
                                phase = 2;
                            }
                            2 => {
                                // Second modify acked → cancel
                                control_tx.send(ControlCommand::Order(OrderRequest::Cancel { order_id: modify_id_2 })).unwrap();
                                phase = 3;
                            }
                            _ => {}
                        }
                    }
                    OrderStatus::Cancelled => { order_cancelled = true; break; }
                    OrderStatus::Rejected => { order_rejected = true; break; }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if order_rejected {
        println!("  SKIP: Order rejected\n");
        return conns;
    }
    assert!(phase >= 3, "Did not complete double modify chain (reached phase {})", phase);
    assert!(order_cancelled, "Final modified order was never cancelled");
    println!("  PASS\n");
    conns
}

// ─── Phase 117: Cancel during modify (race condition) ───

pub(super) fn phase_cancel_during_modify(conns: Conns) -> Conns {
    println!("--- Phase 117: Cancel During Modify (race condition, SPY) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared, Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );
    let inst_id = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(inst_id, "SPY".to_string());

    let order_id = next_order_id();
    let new_order_id = order_id + 1;

    // Submit limit buy at $1
    control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimit {
        order_id, instrument: inst_id, side: Side::Buy, qty: 1, price: 1_00_000_000,
    })).unwrap();
    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(60);
    let mut order_acked = false;
    let mut race_sent = false;
    let mut order_cancelled = false;
    let mut order_rejected = false;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::OrderUpdate(update)) => {
                match update.status {
                    OrderStatus::Submitted => {
                        if !order_acked {
                            order_acked = true;
                            // Send modify AND cancel back-to-back — no waiting
                            control_tx.send(ControlCommand::Order(OrderRequest::Modify {
                                order_id, new_order_id, price: 2_00_000_000, qty: 1,
                            })).unwrap();
                            control_tx.send(ControlCommand::Order(OrderRequest::Cancel { order_id })).unwrap();
                            control_tx.send(ControlCommand::Order(OrderRequest::Cancel { order_id: new_order_id })).unwrap();
                            race_sent = true;
                        }
                    }
                    OrderStatus::Cancelled => { order_cancelled = true; break; }
                    OrderStatus::Rejected => { order_rejected = true; break; }
                    _ => {}
                }
            }
            Ok(Event::CancelReject(_)) => {
                // Expected: one of the cancels may be rejected
            }
            _ => {}
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if order_rejected {
        println!("  SKIP: Order rejected\n");
        return conns;
    }
    assert!(order_acked, "Order was never acknowledged");
    assert!(race_sent, "Race condition commands were never sent");
    assert!(order_cancelled, "Order was never cancelled (neither original nor modified)");
    println!("  PASS\n");
    conns
}
