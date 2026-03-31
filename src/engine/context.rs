use crate::engine::market_state::MarketState;
use crate::types::*;
use std::collections::HashMap;
use std::time::Instant;

/// TSC-calibrated clock for hot-path timestamps.
pub struct Clock {
    start: std::time::Instant,
}

impl Clock {
    pub fn new() -> Self {
        Self {
            start: std::time::Instant::now(),
        }
    }

    /// Monotonic nanoseconds since engine start. Fast, no syscall.
    #[inline(always)]
    pub fn now_ns(&self) -> u64 {
        self.start.elapsed().as_nanos() as u64
    }

    /// Wall-clock Unix timestamp in seconds.
    pub fn now_utc(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }
}

/// The context passed to strategy callbacks. Provides market data access and
/// order management. All hot-path data is pre-allocated.
pub struct Context {
    pub(crate) market: MarketState,
    positions: [i64; MAX_INSTRUMENTS],
    open_orders: HashMap<OrderId, Order>,
    pub(crate) pending_orders: OrderBuffer,
    pub(crate) account: AccountState,
    clock: Clock,
    next_order_id: OrderId,
    /// Timestamp when the last farm socket recv returned data (for decode latency measurement).
    pub(crate) recv_at: Instant,
    /// Total hot loop iterations since start.
    pub(crate) loop_iterations: u64,
}

impl Context {
    pub fn new() -> Self {
        Self {
            market: MarketState::new(),
            positions: [0i64; MAX_INSTRUMENTS],
            open_orders: HashMap::with_capacity(128),
            pending_orders: OrderBuffer::new(),
            account: AccountState::default(),
            clock: Clock::new(),
            next_order_id: {
                // Epoch-based to avoid "Duplicate ID" across IB sessions
                let secs = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                secs * 1000
            },
            recv_at: Instant::now(),
            loop_iterations: 0,
        }
    }

    // ── Market data (read, zero-copy) ──

    #[inline(always)]
    pub fn bid(&self, id: InstrumentId) -> Price {
        self.market.bid(id)
    }

    #[inline(always)]
    pub fn ask(&self, id: InstrumentId) -> Price {
        self.market.ask(id)
    }

    #[inline(always)]
    pub fn last(&self, id: InstrumentId) -> Price {
        self.market.last(id)
    }

    #[inline(always)]
    pub fn bid_size(&self, id: InstrumentId) -> Qty {
        self.market.bid_size(id)
    }

    #[inline(always)]
    pub fn ask_size(&self, id: InstrumentId) -> Qty {
        self.market.ask_size(id)
    }

    #[inline(always)]
    pub fn mid(&self, id: InstrumentId) -> Price {
        self.market.mid(id)
    }

    #[inline(always)]
    pub fn spread(&self, id: InstrumentId) -> Price {
        self.market.spread(id)
    }

    #[inline(always)]
    pub fn quote(&self, id: InstrumentId) -> &Quote {
        self.market.quote(id)
    }

    // ── Positions & orders (read) ──

    #[inline(always)]
    pub fn position(&self, id: InstrumentId) -> i64 {
        self.positions[id as usize]
    }

    pub fn open_orders_for(&self, id: InstrumentId) -> Vec<&Order> {
        self.open_orders
            .values()
            .filter(|o| {
                o.instrument == id
                    && matches!(
                        o.status,
                        OrderStatus::PendingSubmit
                            | OrderStatus::Submitted
                            | OrderStatus::PartiallyFilled
                            | OrderStatus::Uncertain
                    )
            })
            .collect()
    }

    pub fn order(&self, order_id: OrderId) -> Option<&Order> {
        self.open_orders.get(&order_id)
    }

    pub fn account(&self) -> &AccountState {
        &self.account
    }

    // ── Order management (write to pre-allocated buffer) ──

    pub fn submit_limit(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price: Price,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitLimit {
            order_id: id,
            instrument,
            side,
            qty,
            price,
        });
        id
    }

    pub fn submit_market(&mut self, instrument: InstrumentId, side: Side, qty: u32) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitMarket {
            order_id: id,
            instrument,
            side,
            qty,
        });
        id
    }

    pub fn submit_stop(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        stop_price: Price,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitStop {
            order_id: id,
            instrument,
            side,
            qty,
            stop_price,
        });
        id
    }

    pub fn submit_stop_limit(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price: Price,
        stop_price: Price,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitStopLimit {
            order_id: id,
            instrument,
            side,
            qty,
            price,
            stop_price,
        });
        id
    }

    pub fn submit_limit_gtc(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price: Price,
        outside_rth: bool,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitLimitGtc {
            order_id: id,
            instrument,
            side,
            qty,
            price,
            outside_rth,
        });
        id
    }

    pub fn submit_stop_gtc(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        stop_price: Price,
        outside_rth: bool,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitStopGtc {
            order_id: id,
            instrument,
            side,
            qty,
            stop_price,
            outside_rth,
        });
        id
    }

    pub fn submit_stop_limit_gtc(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price: Price,
        stop_price: Price,
        outside_rth: bool,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitStopLimitGtc {
            order_id: id,
            instrument,
            side,
            qty,
            price,
            stop_price,
            outside_rth,
        });
        id
    }

    pub fn submit_limit_ioc(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price: Price,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitLimitIoc {
            order_id: id,
            instrument,
            side,
            qty,
            price,
        });
        id
    }

    pub fn submit_limit_fok(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price: Price,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitLimitFok {
            order_id: id,
            instrument,
            side,
            qty,
            price,
        });
        id
    }

    pub fn submit_trailing_stop(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        trail_amt: Price,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitTrailingStop {
            order_id: id,
            instrument,
            side,
            qty,
            trail_amt,
        });
        id
    }

    pub fn submit_trailing_stop_limit(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price: Price,
        trail_amt: Price,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders
            .push(OrderRequest::SubmitTrailingStopLimit {
                order_id: id,
                instrument,
                side,
                qty,
                price,
                trail_amt,
            });
        id
    }

    pub fn submit_trailing_stop_pct(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        trail_pct: u32,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders
            .push(OrderRequest::SubmitTrailingStopPct {
                order_id: id,
                instrument,
                side,
                qty,
                trail_pct,
            });
        id
    }

    pub fn submit_moc(&mut self, instrument: InstrumentId, side: Side, qty: u32) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitMoc {
            order_id: id,
            instrument,
            side,
            qty,
        });
        id
    }

    pub fn submit_loc(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price: Price,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitLoc {
            order_id: id,
            instrument,
            side,
            qty,
            price,
        });
        id
    }

    pub fn submit_mit(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        stop_price: Price,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitMit {
            order_id: id,
            instrument,
            side,
            qty,
            stop_price,
        });
        id
    }

    pub fn submit_lit(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price: Price,
        stop_price: Price,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitLit {
            order_id: id,
            instrument,
            side,
            qty,
            price,
            stop_price,
        });
        id
    }

    /// Submit a bracket order: limit entry + take-profit limit + stop-loss stop.
    /// Returns (parent_id, take_profit_id, stop_loss_id).
    pub fn submit_bracket(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        entry_price: Price,
        take_profit: Price,
        stop_loss: Price,
    ) -> (OrderId, OrderId, OrderId) {
        let parent_id = self.next_order_id;
        let tp_id = self.next_order_id + 1;
        let sl_id = self.next_order_id + 2;
        self.next_order_id += 3;
        self.pending_orders.push(OrderRequest::SubmitBracket {
            parent_id,
            tp_id,
            sl_id,
            instrument,
            side,
            qty,
            entry_price,
            take_profit,
            stop_loss,
        });
        (parent_id, tp_id, sl_id)
    }

    /// Submit a limit order with extended attributes (display size, hidden, GAT, GTD, outside RTH).
    /// Use `tif`: b'0' = DAY, b'1' = GTC, b'6' = GTD (auto-set if good_till > 0).
    pub fn submit_limit_ex(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price: Price,
        tif: u8,
        attrs: OrderAttrs,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitLimitEx {
            order_id: id,
            instrument,
            side,
            qty,
            price,
            tif,
            attrs,
        });
        id
    }

    pub fn submit_rel(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        offset: Price,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitRel {
            order_id: id,
            instrument,
            side,
            qty,
            offset,
        });
        id
    }

    pub fn submit_limit_opg(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price: Price,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitLimitOpg {
            order_id: id,
            instrument,
            side,
            qty,
            price,
        });
        id
    }

    pub fn submit_adaptive(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price: Price,
        priority: AdaptivePriority,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitAdaptive {
            order_id: id,
            instrument,
            side,
            qty,
            price,
            priority,
        });
        id
    }

    pub fn submit_mtl(&mut self, instrument: InstrumentId, side: Side, qty: u32) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitMtl {
            order_id: id,
            instrument,
            side,
            qty,
        });
        id
    }

    pub fn submit_mkt_prt(&mut self, instrument: InstrumentId, side: Side, qty: u32) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitMktPrt {
            order_id: id,
            instrument,
            side,
            qty,
        });
        id
    }

    pub fn submit_stp_prt(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        stop_price: Price,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitStpPrt {
            order_id: id,
            instrument,
            side,
            qty,
            stop_price,
        });
        id
    }

    pub fn submit_mid_price(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price_cap: Price,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitMidPrice {
            order_id: id,
            instrument,
            side,
            qty,
            price_cap,
        });
        id
    }

    pub fn submit_snap_mkt(&mut self, instrument: InstrumentId, side: Side, qty: u32) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitSnapMkt {
            order_id: id,
            instrument,
            side,
            qty,
        });
        id
    }

    pub fn submit_snap_mid(&mut self, instrument: InstrumentId, side: Side, qty: u32) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitSnapMid {
            order_id: id,
            instrument,
            side,
            qty,
        });
        id
    }

    pub fn submit_snap_pri(&mut self, instrument: InstrumentId, side: Side, qty: u32) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitSnapPri {
            order_id: id,
            instrument,
            side,
            qty,
        });
        id
    }

    pub fn submit_peg_mkt(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        offset: Price,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitPegMkt {
            order_id: id,
            instrument,
            side,
            qty,
            offset,
        });
        id
    }

    pub fn submit_peg_mid(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        offset: Price,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitPegMid {
            order_id: id,
            instrument,
            side,
            qty,
            offset,
        });
        id
    }

    /// Submit an algorithmic order (VWAP, TWAP, Arrival Price, etc.).
    pub fn submit_algo(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price: Price,
        algo: AlgoParams,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitAlgo {
            order_id: id,
            instrument,
            side,
            qty,
            price,
            algo,
        });
        id
    }

    /// Submit a Pegged to Benchmark order (OrdType PB).
    /// Pegs to a benchmark instrument's price with change amounts.
    pub fn submit_peg_bench(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price: Price,
        ref_con_id: u32,
        is_peg_decrease: bool,
        pegged_change_amount: Price,
        ref_change_amount: Price,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitPegBench {
            order_id: id,
            instrument,
            side,
            qty,
            price,
            ref_con_id,
            is_peg_decrease,
            pegged_change_amount,
            ref_change_amount,
        });
        id
    }

    /// Submit a limit order for exchange auction (TIF=AUC).
    pub fn submit_limit_auc(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price: Price,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitLimitAuc {
            order_id: id,
            instrument,
            side,
            qty,
            price,
        });
        id
    }

    /// Submit a Market-to-Limit order for exchange auction (TIF=AUC).
    pub fn submit_mtl_auc(&mut self, instrument: InstrumentId, side: Side, qty: u32) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitMtlAuc {
            order_id: id,
            instrument,
            side,
            qty,
        });
        id
    }

    /// Submit a Box Top order (wire-identical to MTL, OrdType K). BOX exchange only.
    pub fn submit_box_top(&mut self, instrument: InstrumentId, side: Side, qty: u32) -> OrderId {
        self.submit_mtl(instrument, side, qty)
    }

    /// Submit a what-if order for margin/commission preview. The order is NOT placed.
    /// Response delivered via `Event::WhatIf`.
    pub fn submit_what_if(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price: Price,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::SubmitWhatIf {
            order_id: id,
            instrument,
            side,
            qty,
            price,
        });
        id
    }

    /// Submit a fractional shares limit order. Qty is fixed-point (QTY_SCALE = 10^4).
    /// E.g., 0.5 shares = 5000, 1.25 shares = 12500.
    pub fn submit_limit_fractional(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: Qty,
        price: Price,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders
            .push(OrderRequest::SubmitLimitFractional {
                order_id: id,
                instrument,
                side,
                qty,
                price,
            });
        id
    }

    /// Submit an adjustable stop order. Adjusts to a different order type when trigger is hit.
    pub fn submit_adjustable_stop(
        &mut self,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        stop_price: Price,
        trigger_price: Price,
        adjusted_order_type: AdjustedOrderType,
        adjusted_stop_price: Price,
        adjusted_stop_limit_price: Price,
    ) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders
            .push(OrderRequest::SubmitAdjustableStop {
                order_id: id,
                instrument,
                side,
                qty,
                stop_price,
                trigger_price,
                adjusted_order_type,
                adjusted_stop_price,
                adjusted_stop_limit_price,
            });
        id
    }

    pub fn cancel(&mut self, order_id: OrderId) {
        self.pending_orders.push(OrderRequest::Cancel { order_id });
    }

    pub fn cancel_all(&mut self, instrument: InstrumentId) {
        self.pending_orders
            .push(OrderRequest::CancelAll { instrument });
    }

    pub fn modify(&mut self, order_id: OrderId, price: Price, qty: u32) -> OrderId {
        let new_id = self.next_order_id;
        self.next_order_id += 1;
        self.pending_orders.push(OrderRequest::Modify {
            new_order_id: new_id,
            order_id,
            price,
            qty,
        });
        new_id
    }

    // ── Timing ──

    #[inline(always)]
    pub fn now_ns(&self) -> u64 {
        self.clock.now_ns()
    }

    pub fn now_utc(&self) -> i64 {
        self.clock.now_utc()
    }

    /// Timestamp when the last farm socket recv returned data.
    #[inline(always)]
    pub fn recv_timestamp(&self) -> Instant {
        self.recv_at
    }

    /// Total hot loop iterations since start.
    #[inline(always)]
    pub fn loop_iterations(&self) -> u64 {
        self.loop_iterations
    }

    // ── Instrument management ──

    pub fn register_instrument(&mut self, con_id: i64) -> InstrumentId {
        self.market.register(con_id)
    }

    pub fn set_symbol(&mut self, id: InstrumentId, symbol: String) {
        self.market.set_symbol(id, symbol);
    }

    pub fn set_quote(&mut self, id: InstrumentId, quote: Quote) {
        *self.market.quote_mut(id) = quote;
    }

    pub fn quote_mut(&mut self, id: InstrumentId) -> &mut Quote {
        self.market.quote_mut(id)
    }

    // ── Engine-internal methods ──

    pub fn drain_pending_orders(&mut self) -> std::vec::Drain<'_, OrderRequest> {
        self.pending_orders.drain()
    }

    pub fn update_position(&mut self, instrument: InstrumentId, delta: i64) {
        self.positions[instrument as usize] += delta;
    }

    pub fn insert_order(&mut self, order: Order) {
        self.open_orders.insert(order.order_id, order);
    }

    pub fn update_order_status(&mut self, order_id: OrderId, status: OrderStatus) {
        if let Some(order) = self.open_orders.get_mut(&order_id) {
            order.status = status;
        }
    }

    pub fn update_order_filled(&mut self, order_id: OrderId, last_shares: u32) {
        if let Some(order) = self.open_orders.get_mut(&order_id) {
            order.filled += last_shares;
        }
    }

    pub fn remove_order(&mut self, order_id: OrderId) {
        self.open_orders.remove(&order_id);
    }

    /// Re-queue an order request for retry (e.g., after send failure).
    pub fn requeue_order(&mut self, req: OrderRequest) {
        self.pending_orders.push(req);
    }

    /// Check if there are pending orders waiting to be sent.
    pub fn has_pending_orders(&self) -> bool {
        !self.pending_orders.is_empty()
    }

    /// Number of pending orders waiting to be sent.
    pub fn pending_order_count(&self) -> usize {
        self.pending_orders.len()
    }

    /// Mark all live open orders as Uncertain (auth disconnect — status may have changed).
    pub fn mark_orders_uncertain(&mut self) {
        for order in self.open_orders.values_mut() {
            match order.status {
                OrderStatus::PendingSubmit
                | OrderStatus::Submitted
                | OrderStatus::PartiallyFilled => {
                    order.status = OrderStatus::Uncertain;
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Order submission & drain ---

    #[test]
    fn submit_limit_returns_incrementing_ids() {
        let mut ctx = Context::new();
        let id1 = ctx.submit_limit(0, Side::Buy, 100, 150 * PRICE_SCALE);
        let id2 = ctx.submit_limit(0, Side::Sell, 50, 151 * PRICE_SCALE);
        assert_eq!(id2, id1 + 1, "IDs should be sequential");
    }

    #[test]
    fn submit_limit_drains_correctly() {
        let mut ctx = Context::new();
        ctx.submit_limit(0, Side::Buy, 100, 150 * PRICE_SCALE);

        let orders: Vec<_> = ctx.drain_pending_orders().collect();
        assert_eq!(orders.len(), 1);
        match orders[0] {
            OrderRequest::SubmitLimit {
                instrument,
                side,
                qty,
                price,
                ..
            } => {
                assert_eq!(instrument, 0);
                assert_eq!(side, Side::Buy);
                assert_eq!(qty, 100);
                assert_eq!(price, 150 * PRICE_SCALE);
            }
            _ => panic!("expected SubmitLimit"),
        }
    }

    #[test]
    fn submit_market_drains_correctly() {
        let mut ctx = Context::new();
        ctx.submit_market(1, Side::Sell, 200);

        let orders: Vec<_> = ctx.drain_pending_orders().collect();
        assert_eq!(orders.len(), 1);
        match orders[0] {
            OrderRequest::SubmitMarket {
                instrument,
                side,
                qty,
                ..
            } => {
                assert_eq!(instrument, 1);
                assert_eq!(side, Side::Sell);
                assert_eq!(qty, 200);
            }
            _ => panic!("expected SubmitMarket"),
        }
    }

    #[test]
    fn cancel_drains_correctly() {
        let mut ctx = Context::new();
        ctx.cancel(42);

        let orders: Vec<_> = ctx.drain_pending_orders().collect();
        match orders[0] {
            OrderRequest::Cancel { order_id } => assert_eq!(order_id, 42),
            _ => panic!("expected Cancel"),
        }
    }

    #[test]
    fn cancel_all_drains_correctly() {
        let mut ctx = Context::new();
        ctx.cancel_all(5);

        let orders: Vec<_> = ctx.drain_pending_orders().collect();
        match orders[0] {
            OrderRequest::CancelAll { instrument } => assert_eq!(instrument, 5),
            _ => panic!("expected CancelAll"),
        }
    }

    #[test]
    fn modify_drains_correctly() {
        let mut ctx = Context::new();
        ctx.modify(7, 200 * PRICE_SCALE, 50);

        let orders: Vec<_> = ctx.drain_pending_orders().collect();
        match orders[0] {
            OrderRequest::Modify {
                order_id,
                price,
                qty,
                ..
            } => {
                assert_eq!(order_id, 7);
                assert_eq!(price, 200 * PRICE_SCALE);
                assert_eq!(qty, 50);
            }
            _ => panic!("expected Modify"),
        }
    }

    #[test]
    fn drain_clears_buffer() {
        let mut ctx = Context::new();
        ctx.submit_limit(0, Side::Buy, 100, 150 * PRICE_SCALE);
        let _: Vec<_> = ctx.drain_pending_orders().collect();
        // Second drain should be empty
        let orders: Vec<_> = ctx.drain_pending_orders().collect();
        assert!(orders.is_empty());
    }

    #[test]
    fn multiple_orders_per_tick() {
        let mut ctx = Context::new();
        ctx.submit_limit(0, Side::Buy, 100, 150 * PRICE_SCALE);
        ctx.submit_limit(0, Side::Sell, 50, 152 * PRICE_SCALE);
        ctx.cancel(99);

        let orders: Vec<_> = ctx.drain_pending_orders().collect();
        assert_eq!(orders.len(), 3);
    }

    // --- Position tracking ---

    #[test]
    fn position_starts_at_zero() {
        let ctx = Context::new();
        assert_eq!(ctx.position(0), 0);
        assert_eq!(ctx.position(255), 0);
    }

    #[test]
    fn update_position_accumulates() {
        let mut ctx = Context::new();
        ctx.update_position(0, 100);
        assert_eq!(ctx.position(0), 100);
        ctx.update_position(0, -30);
        assert_eq!(ctx.position(0), 70);
        ctx.update_position(0, -70);
        assert_eq!(ctx.position(0), 0);
    }

    #[test]
    fn positions_per_instrument() {
        let mut ctx = Context::new();
        ctx.update_position(0, 100);
        ctx.update_position(1, -50);
        assert_eq!(ctx.position(0), 100);
        assert_eq!(ctx.position(1), -50);
    }

    // --- Open orders ---

    #[test]
    fn insert_and_query_order() {
        let mut ctx = Context::new();
        let order = Order {
            order_id: 1,
            instrument: 0,
            side: Side::Buy,
            price: 150 * PRICE_SCALE,
            qty: 100,
            filled: 0,
            status: OrderStatus::Submitted,
            ord_type: b'2',
            tif: b'0',
            stop_price: 0,
        };
        ctx.insert_order(order);
        assert!(ctx.order(1).is_some());
        assert_eq!(ctx.order(1).unwrap().qty, 100);
    }

    #[test]
    fn open_orders_for_instrument() {
        let mut ctx = Context::new();
        ctx.insert_order(Order {
            order_id: 1,
            instrument: 0,
            side: Side::Buy,
            price: 150 * PRICE_SCALE,
            qty: 100,
            filled: 0,
            status: OrderStatus::Submitted,
            ord_type: b'2',
            tif: b'0',
            stop_price: 0,
        });
        ctx.insert_order(Order {
            order_id: 2,
            instrument: 1,
            side: Side::Sell,
            price: 400 * PRICE_SCALE,
            qty: 50,
            filled: 0,
            status: OrderStatus::Submitted,
            ord_type: b'2',
            tif: b'0',
            stop_price: 0,
        });

        let inst0_orders = ctx.open_orders_for(0);
        assert_eq!(inst0_orders.len(), 1);
        assert_eq!(inst0_orders[0].order_id, 1);
    }

    #[test]
    fn update_order_status() {
        let mut ctx = Context::new();
        ctx.insert_order(Order {
            order_id: 1,
            instrument: 0,
            side: Side::Buy,
            price: 150 * PRICE_SCALE,
            qty: 100,
            filled: 0,
            status: OrderStatus::Submitted,
            ord_type: b'2',
            tif: b'0',
            stop_price: 0,
        });
        ctx.update_order_status(1, OrderStatus::Cancelled);
        assert_eq!(ctx.order(1).unwrap().status, OrderStatus::Cancelled);

        // Cancelled orders not in open_orders_for (filters by Submitted)
        assert!(ctx.open_orders_for(0).is_empty());
    }

    #[test]
    fn remove_order() {
        let mut ctx = Context::new();
        ctx.insert_order(Order {
            order_id: 1,
            instrument: 0,
            side: Side::Buy,
            price: 150 * PRICE_SCALE,
            qty: 100,
            filled: 0,
            status: OrderStatus::Submitted,
            ord_type: b'2',
            tif: b'0',
            stop_price: 0,
        });
        ctx.remove_order(1);
        assert!(ctx.order(1).is_none());
    }

    // --- Market data through context ---

    #[test]
    fn context_market_data_accessors() {
        let mut ctx = Context::new();
        let id = ctx.market.register(265598);
        let q = ctx.market.quote_mut(id);
        q.bid = 15000 * (PRICE_SCALE / 100);
        q.ask = 15010 * (PRICE_SCALE / 100);

        assert_eq!(ctx.bid(id), 15000 * (PRICE_SCALE / 100));
        assert_eq!(ctx.ask(id), 15010 * (PRICE_SCALE / 100));
        assert_eq!(ctx.spread(id), 10 * (PRICE_SCALE / 100));
        assert_eq!(ctx.mid(id), 15005 * (PRICE_SCALE / 100));
    }

    // --- Clock ---

    #[test]
    fn clock_monotonic() {
        let ctx = Context::new();
        let t1 = ctx.now_ns();
        let t2 = ctx.now_ns();
        assert!(t2 >= t1);
    }

    #[test]
    fn clock_utc_reasonable() {
        let ctx = Context::new();
        let ts = ctx.now_utc();
        // Should be after 2025-01-01 (1735689600)
        assert!(ts > 1_735_689_600);
    }

    // --- submit_limit uses current bid ---

    #[test]
    fn submit_limit_uses_current_bid() {
        let mut ctx = Context::new();
        ctx.market.register(265598);
        ctx.market.quote_mut(0).bid = 150 * PRICE_SCALE;

        ctx.submit_limit(0, Side::Buy, 100, ctx.bid(0));

        let orders: Vec<_> = ctx.drain_pending_orders().collect();
        assert_eq!(orders.len(), 1);
        match orders[0] {
            OrderRequest::SubmitLimit { price, .. } => {
                assert_eq!(price, 150 * PRICE_SCALE);
            }
            _ => panic!("expected SubmitLimit"),
        }
    }

    // --- register_instrument ---

    #[test]
    fn register_instrument_returns_id() {
        let mut ctx = Context::new();
        let id = ctx.register_instrument(265598);
        assert_eq!(id, 0);
        let id2 = ctx.register_instrument(272093);
        assert_eq!(id2, 1);
    }

    #[test]
    fn register_instrument_idempotent() {
        let mut ctx = Context::new();
        let id1 = ctx.register_instrument(265598);
        let id2 = ctx.register_instrument(265598);
        assert_eq!(id1, id2);
    }

    // --- set_quote ---

    #[test]
    fn set_quote_replaces_entire_quote() {
        let mut ctx = Context::new();
        let id = ctx.register_instrument(265598);
        let q = Quote {
            bid: 150 * PRICE_SCALE,
            ask: 151 * PRICE_SCALE,
            last: 150_50 * (PRICE_SCALE / 100),
            bid_size: 500,
            ask_size: 300,
            ..Quote::default()
        };
        ctx.set_quote(id, q);
        assert_eq!(ctx.bid(id), 150 * PRICE_SCALE);
        assert_eq!(ctx.ask(id), 151 * PRICE_SCALE);
        assert_eq!(ctx.bid_size(id), 500);
        assert_eq!(ctx.ask_size(id), 300);
    }

    // --- quote_mut ---

    #[test]
    fn quote_mut_modifies_in_place() {
        let mut ctx = Context::new();
        let id = ctx.register_instrument(265598);
        ctx.quote_mut(id).bid = 42 * PRICE_SCALE;
        assert_eq!(ctx.bid(id), 42 * PRICE_SCALE);
    }

    // --- bid_size, ask_size ---

    #[test]
    fn bid_size_ask_size_delegates() {
        let mut ctx = Context::new();
        let id = ctx.register_instrument(265598);
        ctx.quote_mut(id).bid_size = 123;
        ctx.quote_mut(id).ask_size = 456;
        assert_eq!(ctx.bid_size(id), 123);
        assert_eq!(ctx.ask_size(id), 456);
    }

    // --- account ---

    #[test]
    fn account_default_zeros() {
        let ctx = Context::new();
        let a = ctx.account();
        assert_eq!(a.net_liquidation, 0);
        assert_eq!(a.buying_power, 0);
    }

    #[test]
    fn account_writable() {
        let mut ctx = Context::new();
        ctx.account.net_liquidation = 100_000 * PRICE_SCALE;
        assert_eq!(ctx.account().net_liquidation, 100_000 * PRICE_SCALE);
    }

    // --- Timing ---

    #[test]
    fn now_ns_monotonic() {
        let ctx = Context::new();
        let t1 = ctx.now_ns();
        let t2 = ctx.now_ns();
        assert!(t2 >= t1);
    }

    #[test]
    fn now_utc_positive() {
        let ctx = Context::new();
        let ts = ctx.now_utc();
        // Should be after 2024-01-01 in seconds since epoch
        assert!(ts > 1704067200);
    }

    // --- Multiple orders per instrument ---

    #[test]
    fn multiple_orders_same_instrument() {
        let mut ctx = Context::new();
        ctx.register_instrument(265598);

        ctx.insert_order(Order {
            order_id: 1,
            instrument: 0,
            side: Side::Buy,
            price: 150 * PRICE_SCALE,
            qty: 100,
            filled: 0,
            status: OrderStatus::Submitted,
            ord_type: b'2',
            tif: b'0',
            stop_price: 0,
        });
        ctx.insert_order(Order {
            order_id: 2,
            instrument: 0,
            side: Side::Sell,
            price: 155 * PRICE_SCALE,
            qty: 50,
            filled: 0,
            status: OrderStatus::Submitted,
            ord_type: b'2',
            tif: b'0',
            stop_price: 0,
        });
        ctx.insert_order(Order {
            order_id: 3,
            instrument: 0,
            side: Side::Buy,
            price: 149 * PRICE_SCALE,
            qty: 200,
            filled: 0,
            status: OrderStatus::Filled,
            ord_type: b'2',
            tif: b'0',
            stop_price: 0,
        });

        // open_orders_for only returns Submitted
        let open = ctx.open_orders_for(0);
        assert_eq!(open.len(), 2);
    }

    // --- Update order status edge case ---

    #[test]
    fn update_order_status_nonexistent_no_panic() {
        let mut ctx = Context::new();
        // Should not panic when order doesn't exist
        ctx.update_order_status(999, OrderStatus::Cancelled);
    }

    #[test]
    fn remove_order_nonexistent_no_panic() {
        let mut ctx = Context::new();
        ctx.remove_order(999); // should not panic
    }

    #[test]
    fn submit_stop_returns_id_and_drains() {
        let mut ctx = Context::new();
        let id = ctx.submit_stop(0, Side::Sell, 50, 140 * PRICE_SCALE);

        let orders: Vec<_> = ctx.drain_pending_orders().collect();
        assert_eq!(orders.len(), 1);
        match orders[0] {
            OrderRequest::SubmitStop {
                order_id,
                instrument,
                side,
                qty,
                stop_price,
            } => {
                assert_eq!(order_id, id);
                assert_eq!(instrument, 0);
                assert_eq!(side, Side::Sell);
                assert_eq!(qty, 50);
                assert_eq!(stop_price, 140 * PRICE_SCALE);
            }
            _ => panic!("Expected SubmitStop"),
        }
    }

    #[test]
    fn update_order_filled_accumulates() {
        let mut ctx = Context::new();
        ctx.insert_order(Order {
            order_id: 1,
            instrument: 0,
            side: Side::Buy,
            price: PRICE_SCALE,
            qty: 100,
            filled: 0,
            status: OrderStatus::PendingSubmit,
            ord_type: b'2',
            tif: b'0',
            stop_price: 0,
        });
        ctx.update_order_filled(1, 30);
        assert_eq!(ctx.order(1).unwrap().filled, 30);
        ctx.update_order_filled(1, 50);
        assert_eq!(ctx.order(1).unwrap().filled, 80);
    }

    #[test]
    fn open_orders_for_includes_pending_and_partial() {
        let mut ctx = Context::new();
        ctx.insert_order(Order {
            order_id: 1,
            instrument: 0,
            side: Side::Buy,
            price: PRICE_SCALE,
            qty: 100,
            filled: 0,
            status: OrderStatus::PendingSubmit,
            ord_type: b'2',
            tif: b'0',
            stop_price: 0,
        });
        ctx.insert_order(Order {
            order_id: 2,
            instrument: 0,
            side: Side::Buy,
            price: PRICE_SCALE,
            qty: 100,
            filled: 50,
            status: OrderStatus::PartiallyFilled,
            ord_type: b'2',
            tif: b'0',
            stop_price: 0,
        });
        ctx.insert_order(Order {
            order_id: 3,
            instrument: 0,
            side: Side::Buy,
            price: PRICE_SCALE,
            qty: 100,
            filled: 100,
            status: OrderStatus::Filled,
            ord_type: b'2',
            tif: b'0',
            stop_price: 0,
        });
        let open = ctx.open_orders_for(0);
        // PendingSubmit and PartiallyFilled count as open; Filled does not
        assert_eq!(open.len(), 2);
    }

    #[test]
    fn submit_peg_bench_drains_correctly() {
        let mut ctx = Context::new();
        let id = ctx.submit_peg_bench(
            0,
            Side::Buy,
            100,
            150 * PRICE_SCALE,
            12345,
            false,
            50_000_000,
            50_000_000,
        );
        let orders: Vec<_> = ctx.drain_pending_orders().collect();
        assert_eq!(orders.len(), 1);
        match &orders[0] {
            OrderRequest::SubmitPegBench {
                order_id,
                instrument,
                side,
                qty,
                price,
                ref_con_id,
                is_peg_decrease,
                pegged_change_amount,
                ref_change_amount,
            } => {
                assert_eq!(*order_id, id);
                assert_eq!(*instrument, 0);
                assert_eq!(*side, Side::Buy);
                assert_eq!(*qty, 100);
                assert_eq!(*price, 150 * PRICE_SCALE);
                assert_eq!(*ref_con_id, 12345);
                assert!(!*is_peg_decrease);
                assert_eq!(*pegged_change_amount, 50_000_000);
                assert_eq!(*ref_change_amount, 50_000_000);
            }
            _ => panic!("expected SubmitPegBench"),
        }
    }

    #[test]
    fn submit_limit_auc_drains_correctly() {
        let mut ctx = Context::new();
        let id = ctx.submit_limit_auc(0, Side::Buy, 100, 150 * PRICE_SCALE);
        let orders: Vec<_> = ctx.drain_pending_orders().collect();
        assert_eq!(orders.len(), 1);
        match &orders[0] {
            OrderRequest::SubmitLimitAuc {
                order_id,
                instrument,
                side,
                qty,
                price,
            } => {
                assert_eq!(*order_id, id);
                assert_eq!(*instrument, 0);
                assert_eq!(*side, Side::Buy);
                assert_eq!(*qty, 100);
                assert_eq!(*price, 150 * PRICE_SCALE);
            }
            _ => panic!("expected SubmitLimitAuc"),
        }
    }

    #[test]
    fn submit_mtl_auc_drains_correctly() {
        let mut ctx = Context::new();
        let id = ctx.submit_mtl_auc(0, Side::Buy, 100);
        let orders: Vec<_> = ctx.drain_pending_orders().collect();
        assert_eq!(orders.len(), 1);
        match &orders[0] {
            OrderRequest::SubmitMtlAuc {
                order_id,
                instrument,
                side,
                qty,
            } => {
                assert_eq!(*order_id, id);
                assert_eq!(*instrument, 0);
                assert_eq!(*side, Side::Buy);
                assert_eq!(*qty, 100);
            }
            _ => panic!("expected SubmitMtlAuc"),
        }
    }

    #[test]
    fn submit_box_top_reuses_mtl() {
        let mut ctx = Context::new();
        let id = ctx.submit_box_top(0, Side::Buy, 100);
        let orders: Vec<_> = ctx.drain_pending_orders().collect();
        assert_eq!(orders.len(), 1);
        match &orders[0] {
            OrderRequest::SubmitMtl {
                order_id,
                instrument,
                side,
                qty,
            } => {
                assert_eq!(*order_id, id);
                assert_eq!(*instrument, 0);
                assert_eq!(*side, Side::Buy);
                assert_eq!(*qty, 100);
            }
            _ => panic!("expected SubmitMtl from box_top"),
        }
    }

    #[test]
    fn submit_what_if_drains_correctly() {
        let mut ctx = Context::new();
        let id = ctx.submit_what_if(0, Side::Buy, 100, 256_20 * (PRICE_SCALE / 100));
        let orders: Vec<_> = ctx.drain_pending_orders().collect();
        assert_eq!(orders.len(), 1);
        match &orders[0] {
            OrderRequest::SubmitWhatIf {
                order_id,
                instrument,
                side,
                qty,
                price,
            } => {
                assert_eq!(*order_id, id);
                assert_eq!(*instrument, 0);
                assert_eq!(*side, Side::Buy);
                assert_eq!(*qty, 100);
                assert_eq!(*price, 256_20 * (PRICE_SCALE / 100));
            }
            _ => panic!("expected SubmitWhatIf"),
        }
    }

    #[test]
    fn submit_limit_fractional_drains_correctly() {
        let mut ctx = Context::new();
        // 0.5 shares = 5000 in QTY_SCALE
        let id = ctx.submit_limit_fractional(0, Side::Buy, QTY_SCALE / 2, 150 * PRICE_SCALE);
        let orders: Vec<_> = ctx.drain_pending_orders().collect();
        assert_eq!(orders.len(), 1);
        match &orders[0] {
            OrderRequest::SubmitLimitFractional {
                order_id,
                instrument,
                side,
                qty,
                price,
            } => {
                assert_eq!(*order_id, id);
                assert_eq!(*instrument, 0);
                assert_eq!(*side, Side::Buy);
                assert_eq!(*qty, 5000);
                assert_eq!(*price, 150 * PRICE_SCALE);
            }
            _ => panic!("expected SubmitLimitFractional"),
        }
    }

    #[test]
    fn submit_adjustable_stop_drains_correctly() {
        let mut ctx = Context::new();
        let id = ctx.submit_adjustable_stop(
            0,
            Side::Sell,
            1,
            251_20 * (PRICE_SCALE / 100), // stop_price
            256_20 * (PRICE_SCALE / 100), // trigger_price
            AdjustedOrderType::StopLimit,
            253_20 * (PRICE_SCALE / 100), // adjusted_stop
            252_20 * (PRICE_SCALE / 100), // adjusted_limit
        );
        let orders: Vec<_> = ctx.drain_pending_orders().collect();
        assert_eq!(orders.len(), 1);
        match &orders[0] {
            OrderRequest::SubmitAdjustableStop {
                order_id,
                side,
                qty,
                stop_price,
                trigger_price,
                adjusted_order_type,
                adjusted_stop_price,
                adjusted_stop_limit_price,
                ..
            } => {
                assert_eq!(*order_id, id);
                assert_eq!(*side, Side::Sell);
                assert_eq!(*qty, 1);
                assert_eq!(*stop_price, 251_20 * (PRICE_SCALE / 100));
                assert_eq!(*trigger_price, 256_20 * (PRICE_SCALE / 100));
                assert_eq!(*adjusted_order_type, AdjustedOrderType::StopLimit);
                assert_eq!(*adjusted_stop_price, 253_20 * (PRICE_SCALE / 100));
                assert_eq!(*adjusted_stop_limit_price, 252_20 * (PRICE_SCALE / 100));
            }
            _ => panic!("expected SubmitAdjustableStop"),
        }
    }
}
