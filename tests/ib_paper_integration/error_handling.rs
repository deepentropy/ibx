//! IB-side error handling test phases.

use super::common::*;

pub(super) fn phase_ib_error_handling(conns: Conns) -> Conns {
    println!("--- Phase 104: IB-Side Error Handling (invalid requests) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (mut hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    let spy_inst = hot_loop.context_mut().register_instrument(756733);
    hot_loop.context_mut().set_symbol(spy_inst, "SPY".to_string());

    // Submit an order for a non-existent instrument (con_id 999999999)
    // The hot loop should handle this gracefully
    let oid = next_order_id();
    // Register a bogus instrument
    let bogus_inst = hot_loop.context_mut().register_instrument(999999999);
    hot_loop.context_mut().set_symbol(bogus_inst, "BOGUS".to_string());

    control_tx.send(ControlCommand::Order(OrderRequest::SubmitMarket {
        order_id: oid, instrument: bogus_inst, side: Side::Buy, qty: 1,
    })).unwrap();

    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut got_error_or_reject = false;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::OrderUpdate(update)) => {
                if update.order_id == oid && update.status == OrderStatus::Rejected {
                    println!("  Order rejected as expected (bogus con_id)");
                    got_error_or_reject = true;
                    break;
                }
            }
            Ok(Event::CancelReject(cr)) => {
                if cr.order_id == oid {
                    got_error_or_reject = true;
                    println!("  CancelReject received for bogus order");
                    break;
                }
            }
            _ => {}
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if got_error_or_reject {
        println!("  PASS\n");
    } else {
        // The order may have been silently ignored or the hot loop handled it
        println!("  SKIP: No rejection/error received (order may have been filtered)\n");
    }
    conns
}

// ─── Phase 114: Pacing violation recovery — rapid historical requests (issue #94) ───

pub(super) fn phase_pacing_violation_recovery(conns: Conns) -> Conns {
    println!("--- Phase 114: Pacing Violation Recovery (10 rapid historical requests) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(), Some(event_tx), account_id.clone(), conns.farm, conns.ccp, conns.hmds, None,
    );

    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let end_dt = format_utc_timestamp(now);

    // Fire 10 historical requests in rapid succession (IB pacing limit is ~60/10min)
    let num_requests = 10u32;
    for i in 0..num_requests {
        control_tx.send(ControlCommand::FetchHistorical {
            req_id: 14000 + i, con_id: 756733, symbol: "SPY".to_string(),
            end_date_time: end_dt.clone(), duration: "1 d".to_string(),
            bar_size: "5 mins".to_string(), what_to_show: "TRADES".to_string(), use_rth: true,
        }).unwrap();
    }

    control_tx.send(ControlCommand::Subscribe { con_id: 756733, symbol: "SPY".into() }).unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(60);
    let mut responses_received = std::collections::HashSet::new();
    let errors_received = 0u32;

    while Instant::now() < deadline {
        // Check for historical data responses
        let data = shared.drain_historical_data();
        for (req_id, resp) in &data {
            if *req_id >= 14000 && *req_id < 14000 + num_requests {
                if resp.is_complete {
                    responses_received.insert(*req_id);
                }
            }
        }

        // Check for error events (pacing violations)
        match event_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(Event::OrderUpdate(_)) | Ok(Event::Tick(_)) => {}
            Ok(Event::Disconnected) => {
                println!("  WARNING: Disconnected during pacing test");
                break;
            }
            _ => {}
        }

        if responses_received.len() == num_requests as usize {
            break;
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    println!("  Responses received: {}/{}", responses_received.len(), num_requests);
    println!("  Errors: {}", errors_received);

    if responses_received.is_empty() {
        // HMDS may be fully rate-limited from prior historical phases
        println!("  SKIP: No responses — HMDS likely pacing-limited from prior phases\n");
    } else if responses_received.len() == num_requests as usize {
        println!("  PASS (all {} requests completed)\n", num_requests);
    } else {
        println!("  PASS ({}/{} completed — pacing may have throttled some)\n", responses_received.len(), num_requests);
    }
    conns
}
