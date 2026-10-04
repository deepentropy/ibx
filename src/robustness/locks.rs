//! Lock tests (ibx#488; ibx#265, ibx#268, ibx#271): a wrapper that calls
//! back into the client from each callback (reqExecutions, open orders,
//! positions, market data, placeOrder, cancel, process_msgs, and at last
//! disconnect), with a real engine thread on in-memory links. No callback
//! may find a lock held by its caller: the whole session runs on a worker
//! thread, and a test fails when it does not end within its time, instead
//! of hanging.

use std::sync::Arc;
use std::time::Duration;

use crate::api::client::{Contract, EClient, Order};
use crate::api::types::{CommissionAndFeesReport, ExecutionFilter, Execution, OrderState, TickAttrib};
use crate::api::wrapper::Wrapper;
use crate::bridge::{FillExec, SharedState};
use crate::engine::hot_loop::HotLoop;
use crate::test_support::Peer;
use crate::types::{Fill, OrderStatus, OrderUpdate, Quote, Side, PRICE_SCALE, QTY_SCALE};

/// Longest a session may take; past it the client is taken as locked.
const LIMIT: Duration = Duration::from_secs(20);

fn stock() -> Contract {
    Contract { con_id: 265598, symbol: "AAPL".into(), sec_type: "STK".into(), exchange: "SMART".into(), currency: "USD".into(), ..Default::default() }
}

fn limit_order() -> Order {
    Order { action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(), lmt_price: 100.0, tif: "DAY".into(), ..Default::default() }
}

/// Calls back into the client from the first call of each callback (and
/// from the callbacks that gives, one level down), and disconnects from
/// the callback `disconnect_on`.
struct Reentrant {
    client: Arc<EClient>,
    depth: u32,
    next_id: i64,
    disconnect_on: &'static str,
    seen: Vec<&'static str>,
}

impl Reentrant {
    fn back_in(&mut self, from: &'static str) {
        let first = !self.seen.contains(&from);
        if first {
            self.seen.push(from);
        }
        // Once per callback, one level down from there too.
        if first && self.depth < 2 {
            self.depth += 1;
            let client = self.client.clone();
            self.next_id += 1;
            let id = self.next_id;
            client.req_executions(id, &ExecutionFilter::default(), self);
            client.req_open_orders(self);
            client.req_positions(self);
            let _ = client.req_mkt_data(id, &stock(), "", false, false);
            let _ = client.place_order(id, &stock(), &limit_order());
            let _ = client.cancel_order(id, "");
            let _ = client.cancel_mkt_data(id);
            client.process_msgs(self);
            self.depth -= 1;
        }
        if from == self.disconnect_on {
            self.client.disconnect();
        }
    }
}

impl Wrapper for Reentrant {
    fn error(&mut self, _: i64, _: i64, _: &str, _: &str) { self.back_in("error") }
    fn connection_closed(&mut self) { self.back_in("connection_closed") }
    fn tick_price(&mut self, _: i64, _: i32, _: f64, _: &TickAttrib) { self.back_in("tick_price") }
    fn tick_size(&mut self, _: i64, _: i32, _: f64) { self.back_in("tick_size") }
    fn order_status(&mut self, _: i64, _: &str, _: f64, _: f64, _: f64, _: i64, _: i64, _: f64, _: i64, _: &str, _: f64) {
        self.back_in("order_status")
    }
    fn open_order(&mut self, _: i64, _: &Contract, _: &Order, _: &OrderState) { self.back_in("open_order") }
    fn open_order_end(&mut self) { self.back_in("open_order_end") }
    fn exec_details(&mut self, _: i64, _: &Contract, _: &Execution) { self.back_in("exec_details") }
    fn exec_details_end(&mut self, _: i64) { self.back_in("exec_details_end") }
    fn commission_and_fees_report(&mut self, _: &CommissionAndFeesReport) { self.back_in("commission_and_fees_report") }
    fn position(&mut self, _: &str, _: &Contract, _: f64, _: f64) { self.back_in("position") }
    fn position_end(&mut self) { self.back_in("position_end") }
}

/// One session: an engine thread, a client on it, a quote, an order with
/// its status and a fill, a lost link notice, each dispatched to a
/// re-entrant wrapper that disconnects from `disconnect_on`. The
/// callbacks seen.
fn session(disconnect_on: &'static str) -> Vec<&'static str> {
    let shared = Arc::new(SharedState::new());
    let (farm, _farm) = Peer::pair();
    let (ccp, _ccp) = Peer::pair();
    let (mut engine, control) = HotLoop::with_connections(
        shared.clone(), None, "DUXXXXXXX".into(), farm, ccp, None, None);
    let handle = std::thread::spawn(move || engine.run());
    let client = Arc::new(EClient::from_parts(shared.clone(), control, handle, "DUXXXXXXX".into()));
    let mut w = Reentrant { client: client.clone(), depth: 0, next_id: 1000, disconnect_on, seen: Vec::new() };

    client.req_mkt_data(1, &stock(), "", false, false).expect("subscribed");
    let instrument = *client.core.req_to_instrument.lock().unwrap().get(&1).expect("an instrument");
    client.place_order(100, &stock(), &limit_order()).expect("placed");
    shared.market.push_test_message(instrument, &Quote { bid: 99 * PRICE_SCALE, ask: 101 * PRICE_SCALE, bid_size: 5 * QTY_SCALE, ..Default::default() }, &Default::default());
    shared.orders.push_order_update(OrderUpdate {
        order_id: 100, instrument, status: OrderStatus::Submitted, filled_qty_fixed: 0, remaining_qty_fixed: QTY_SCALE,
        avg_fill_price: 0, perm_id: 7, parent_id: 0, timestamp_ns: 1,
    });
    let fill = Fill {
        instrument, order_id: 100, side: Side::Buy, price: 100 * PRICE_SCALE, qty_fixed: QTY_SCALE, remaining_fixed: 0,
        cum_qty_fixed: QTY_SCALE, avg_price: 100 * PRICE_SCALE, commission: PRICE_SCALE / 2, timestamp_ns: 2,
    };
    shared.orders.push_fill_with_exec(fill, FillExec { exec_id: "0001.01".into(), ..Default::default() });
    shared.orders.push_commission_report(CommissionAndFeesReport {
        exec_id: "0001.01".into(), commission_and_fees: 1.0, currency: "USD".into(), ..Default::default()
    });
    client.process_msgs(&mut w);
    client.req_executions(2, &ExecutionFilter::default(), &mut w);
    shared.push_connection_notice(1100, "Connectivity between client and server has been lost.".into());
    client.process_msgs(&mut w);
    // The engine stopped by the disconnect: the session ends.
    client.disconnect();
    client.process_msgs(&mut w);
    w.seen
}

/// `session` on a worker thread; fails when it does not end within
/// [`LIMIT`] (a lock held across a callback) or panics.
fn within_limit(disconnect_on: &'static str) -> Vec<&'static str> {
    let (tx, rx) = crossbeam_channel::bounded(1);
    std::thread::Builder::new()
        .name(format!("reentrant-{disconnect_on}"))
        .spawn(move || {
            let seen = std::panic::catch_unwind(|| session(disconnect_on));
            let _ = tx.send(seen.map_err(|e| e.downcast_ref::<String>().cloned()
                .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default()));
        })
        .unwrap();
    match rx.recv_timeout(LIMIT) {
        Ok(Ok(seen)) => seen,
        Ok(Err(text)) => panic!("disconnect from {disconnect_on}: the session panicked: {text}"),
        Err(_) => panic!("disconnect from {disconnect_on}: no end within {LIMIT:?}, a lock is held across a callback"),
    }
}

#[test]
fn callbacks_call_back_into_the_client() {
    let seen = within_limit("none");
    for callback in ["tick_price", "order_status", "open_order", "exec_details", "commission_and_fees_report",
        "exec_details_end", "open_order_end", "error", "connection_closed"]
    {
        assert!(seen.contains(&callback), "{callback} was not called: {seen:?}");
    }
}

#[test]
fn disconnect_from_each_callback() {
    for callback in ["tick_price", "order_status", "open_order", "exec_details", "commission_and_fees_report",
        "exec_details_end", "open_order_end", "position", "position_end", "error", "connection_closed"]
    {
        within_limit(callback);
    }
}
