/// Internal instrument identifier. Mapped from IB's conId at subscription time.
/// Used as an index into pre-allocated arrays, so values are dense and small.
pub type InstrumentId = u32;

/// Engine-assigned order identifier.
pub type OrderId = u64;

/// Fixed-point price: value * 10^8. Avoids floating-point on the hot path.
/// Example: $150.25 = 15_025_000_000
pub type Price = i64;

/// Fixed-point quantity: value * 10^4. Matches IB's 0.0001 minimum increment.
/// Example: 100 shares = 1_000_000
pub type Qty = i64;

pub const PRICE_SCALE: i64 = 100_000_000; // 10^8
pub const QTY_SCALE: i64 = 10_000; // 10^4

/// Maximum number of concurrently tracked instruments.
pub const MAX_INSTRUMENTS: usize = 256;

/// Maximum pending order requests per tick cycle.
const MAX_PENDING_ORDERS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderStatus {
    /// Locally queued, not yet acknowledged by server.
    PendingSubmit,
    /// Acknowledged by server (FIX 39=0 or 39=A).
    Submitted,
    Filled,
    PartiallyFilled,
    Cancelled,
    Rejected,
}

/// Current quote for an instrument. Cache-line aligned for hot-path access.
/// 88 bytes of data, padded to 128 bytes (2 cache lines).
#[derive(Clone, Copy)]
#[repr(C, align(64))]
pub struct Quote {
    pub bid: Price,
    pub ask: Price,
    pub last: Price,
    pub bid_size: Qty,
    pub ask_size: Qty,
    pub last_size: Qty,
    pub volume: Qty,
    pub open: Price,
    pub high: Price,
    pub low: Price,
    pub close: Price,
    pub timestamp_ns: u64,
}

impl Default for Quote {
    fn default() -> Self {
        Self {
            bid: 0,
            ask: 0,
            last: 0,
            bid_size: 0,
            ask_size: 0,
            last_size: 0,
            volume: 0,
            open: 0,
            high: 0,
            low: 0,
            close: 0,
            timestamp_ns: 0,
        }
    }
}

/// Execution fill report.
#[derive(Debug, Clone, Copy)]
pub struct Fill {
    pub instrument: InstrumentId,
    pub order_id: OrderId,
    pub side: Side,
    pub price: Price,
    pub qty: i64,
    pub remaining: i64,
    pub commission: Price,
    pub timestamp_ns: u64,
}

/// Order status change notification.
#[derive(Debug, Clone, Copy)]
pub struct OrderUpdate {
    pub order_id: OrderId,
    pub instrument: InstrumentId,
    pub status: OrderStatus,
    pub filled_qty: i64,
    pub remaining_qty: i64,
    pub timestamp_ns: u64,
}

/// A tracked open order.
#[derive(Debug, Clone, Copy)]
pub struct Order {
    pub order_id: OrderId,
    pub instrument: InstrumentId,
    pub side: Side,
    pub price: Price,
    pub qty: u32,
    pub filled: u32,
    pub status: OrderStatus,
}

/// Order request written by strategy, drained by engine after on_tick.
#[derive(Debug, Clone, Copy)]
pub enum OrderRequest {
    SubmitLimit {
        order_id: OrderId,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price: Price,
    },
    SubmitMarket {
        order_id: OrderId,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
    },
    SubmitStop {
        order_id: OrderId,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        stop_price: Price,
    },
    SubmitStopLimit {
        order_id: OrderId,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price: Price,
        stop_price: Price,
    },
    SubmitLimitGtc {
        order_id: OrderId,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price: Price,
        outside_rth: bool,
    },
    Cancel {
        order_id: OrderId,
    },
    CancelAll {
        instrument: InstrumentId,
    },
    Modify {
        new_order_id: OrderId,
        order_id: OrderId,
        price: Price,
        qty: u32,
    },
}

/// Pre-allocated buffer for pending order requests. Never allocates on the hot path.
/// Created once with capacity, then push/clear cycle each tick.
pub struct OrderBuffer {
    buf: Vec<OrderRequest>,
}

impl OrderBuffer {
    pub fn new() -> Self {
        Self {
            buf: Vec::with_capacity(MAX_PENDING_ORDERS),
        }
    }

    pub fn push(&mut self, req: OrderRequest) {
        debug_assert!(self.buf.len() < MAX_PENDING_ORDERS, "order buffer overflow");
        self.buf.push(req);
    }

    pub fn drain(&mut self) -> std::vec::Drain<'_, OrderRequest> {
        self.buf.drain(..)
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

/// Commands sent from the control plane to the hot loop via SPSC channel.
#[derive(Debug, Clone)]
pub enum ControlCommand {
    /// Subscribe to market data for a contract.
    Subscribe { con_id: i64, symbol: String },
    /// Unsubscribe from market data for an instrument.
    Unsubscribe { instrument: InstrumentId },
    /// Update a strategy parameter.
    UpdateParam { key: String, value: String },
    /// Submit an order from external caller (bridge mode).
    Order(OrderRequest),
    /// Register an instrument from external caller (bridge mode).
    RegisterInstrument { con_id: i64 },
    /// Graceful shutdown.
    Shutdown,
}

/// Account-level state.
#[derive(Debug, Clone, Copy, Default)]
pub struct AccountState {
    pub net_liquidation: Price,
    pub buying_power: Price,
    pub margin_used: Price,
    pub unrealized_pnl: Price,
    pub realized_pnl: Price,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem;

    // --- Quote layout ---

    #[test]
    fn quote_alignment_is_64() {
        assert_eq!(mem::align_of::<Quote>(), 64);
    }

    #[test]
    fn quote_size_is_128() {
        // 11 × i64 (88) + 1 × u64 (8) = 96 bytes data, padded to 128 (2 cache lines)
        assert_eq!(mem::size_of::<Quote>(), 128);
    }

    #[test]
    fn quote_is_copy() {
        let q = Quote::default();
        let q2 = q; // Copy
        assert_eq!(q.bid, q2.bid);
    }

    // --- Price fixed-point ---

    #[test]
    fn price_150_25() {
        let p: Price = 150_25 * (PRICE_SCALE / 100);
        assert_eq!(p, 15_025_000_000);
    }

    #[test]
    fn price_to_float() {
        let p: Price = 15_025_000_000;
        let f = p as f64 / PRICE_SCALE as f64;
        assert!((f - 150.25).abs() < 1e-10);
    }

    #[test]
    fn price_negative() {
        let p: Price = -500 * PRICE_SCALE;
        assert_eq!(p, -50_000_000_000);
    }

    // --- Qty fixed-point ---

    #[test]
    fn qty_100_shares() {
        let q: Qty = 100 * QTY_SCALE;
        assert_eq!(q, 1_000_000);
    }

    #[test]
    fn qty_fractional() {
        // 0.5 shares (fractional shares)
        let q: Qty = QTY_SCALE / 2;
        assert_eq!(q, 5_000);
    }

    // --- OrderBuffer ---

    #[test]
    fn order_buffer_starts_empty() {
        let buf = OrderBuffer::new();
        assert!(buf.is_empty());
    }

    #[test]
    fn order_buffer_push_and_drain() {
        let mut buf = OrderBuffer::new();
        buf.push(OrderRequest::SubmitLimit {
            order_id: 1,
            instrument: 0,
            side: Side::Buy,
            qty: 100,
            price: 150 * PRICE_SCALE,
        });
        buf.push(OrderRequest::Cancel { order_id: 42 });
        assert!(!buf.is_empty());

        let drained: Vec<_> = buf.drain().collect();
        assert_eq!(drained.len(), 2);
        assert!(buf.is_empty());
    }

    #[test]
    fn order_buffer_no_realloc() {
        let mut buf = OrderBuffer::new();
        let cap_before = buf.buf.capacity();
        for i in 0..MAX_PENDING_ORDERS {
            buf.push(OrderRequest::Cancel { order_id: i as u64 });
        }
        // Capacity should not have grown (pre-allocated)
        assert_eq!(buf.buf.capacity(), cap_before);
    }

    #[test]
    fn order_buffer_drain_reusable() {
        let mut buf = OrderBuffer::new();
        buf.push(OrderRequest::SubmitMarket {
            order_id: 1,
            instrument: 0,
            side: Side::Sell,
            qty: 50,
        });
        let _: Vec<_> = buf.drain().collect();
        assert!(buf.is_empty());

        // Can push again after drain
        buf.push(OrderRequest::CancelAll { instrument: 1 });
        assert!(!buf.is_empty());
    }

    // --- OrderRequest variants ---

    #[test]
    fn order_request_is_copy() {
        let req = OrderRequest::Modify {
            new_order_id: 2,
            order_id: 1,
            price: 100 * PRICE_SCALE,
            qty: 200,
        };
        let req2 = req; // Copy
        match (req, req2) {
            (
                OrderRequest::Modify { order_id: a, .. },
                OrderRequest::Modify { order_id: b, .. },
            ) => assert_eq!(a, b),
            _ => panic!("should both be Modify"),
        }
    }

    // --- Quote field independence ---

    #[test]
    fn quote_default_all_zeros() {
        let q = Quote::default();
        assert_eq!(q.bid, 0);
        assert_eq!(q.ask, 0);
        assert_eq!(q.last, 0);
        assert_eq!(q.bid_size, 0);
        assert_eq!(q.ask_size, 0);
        assert_eq!(q.last_size, 0);
        assert_eq!(q.volume, 0);
        assert_eq!(q.open, 0);
        assert_eq!(q.high, 0);
        assert_eq!(q.low, 0);
        assert_eq!(q.close, 0);
        assert_eq!(q.timestamp_ns, 0);
    }

    #[test]
    fn quote_field_independence() {
        let mut q = Quote::default();
        q.bid = 100 * PRICE_SCALE;
        assert_eq!(q.ask, 0); // other fields untouched
        assert_eq!(q.last, 0);
        q.ask = 101 * PRICE_SCALE;
        assert_eq!(q.bid, 100 * PRICE_SCALE); // bid unchanged
    }

    #[test]
    fn quote_in_array_no_false_sharing() {
        // Two adjacent quotes should be on different cache lines
        let quotes = [Quote::default(); 4];
        let ptr0 = &quotes[0] as *const Quote as usize;
        let ptr1 = &quotes[1] as *const Quote as usize;
        // Each quote is 128 bytes (2 cache lines), so stride should be 128
        assert_eq!(ptr1 - ptr0, 128);
    }

    // --- Price edge cases ---

    #[test]
    fn price_zero() {
        let p: Price = 0;
        assert_eq!(p as f64 / PRICE_SCALE as f64, 0.0);
    }

    #[test]
    fn price_one_cent() {
        let p: Price = PRICE_SCALE / 100; // $0.01
        let f = p as f64 / PRICE_SCALE as f64;
        assert!((f - 0.01).abs() < 1e-10);
    }

    #[test]
    fn price_sub_penny() {
        // $0.0001 (minimum tick for some instruments)
        let p: Price = PRICE_SCALE / 10_000;
        assert_eq!(p, 10_000); // 10^4
        let f = p as f64 / PRICE_SCALE as f64;
        assert!((f - 0.0001).abs() < 1e-12);
    }

    #[test]
    fn price_large_value() {
        // $100,000.00 (like BRK.A)
        let p: Price = 100_000 * PRICE_SCALE;
        assert_eq!(p, 10_000_000_000_000);
        // Should be well within i64 range (max ~9.2 * 10^18)
        assert!(p < i64::MAX);
    }

    #[test]
    fn price_max_representable() {
        // Maximum price: i64::MAX / PRICE_SCALE = ~92,233,720,368
        let max_price = i64::MAX / PRICE_SCALE;
        let p: Price = max_price * PRICE_SCALE;
        // Should not overflow
        assert!(p > 0);
    }

    // --- Qty edge cases ---

    #[test]
    fn qty_zero() {
        let q: Qty = 0;
        assert_eq!(q, 0);
    }

    #[test]
    fn qty_negative() {
        let q: Qty = -100 * QTY_SCALE;
        assert_eq!(q, -1_000_000);
    }

    #[test]
    fn qty_one_ten_thousandth() {
        let q: Qty = 1; // smallest representable: 0.0001 shares
        let f = q as f64 / QTY_SCALE as f64;
        assert!((f - 0.0001).abs() < 1e-10);
    }

    // --- OrderBuffer edge cases ---

    #[test]
    fn order_buffer_multiple_drain_cycles() {
        let mut buf = OrderBuffer::new();
        for cycle in 0..10 {
            for i in 0..5 {
                buf.push(OrderRequest::Cancel { order_id: (cycle * 5 + i) as u64 });
            }
            let drained: Vec<_> = buf.drain().collect();
            assert_eq!(drained.len(), 5);
            assert!(buf.is_empty());
        }
    }

    #[test]
    fn order_buffer_drain_empty() {
        let mut buf = OrderBuffer::new();
        let drained: Vec<_> = buf.drain().collect();
        assert!(drained.is_empty());
    }

    // --- All OrderRequest variants ---

    #[test]
    fn order_request_submit_limit_fields() {
        let req = OrderRequest::SubmitLimit {
            order_id: 1,
            instrument: 42,
            side: Side::Buy,
            qty: 100,
            price: 150 * PRICE_SCALE,
        };
        match req {
            OrderRequest::SubmitLimit { instrument, side, qty, price, .. } => {
                assert_eq!(instrument, 42);
                assert_eq!(side, Side::Buy);
                assert_eq!(qty, 100);
                assert_eq!(price, 150 * PRICE_SCALE);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn order_request_submit_market_fields() {
        let req = OrderRequest::SubmitMarket {
            order_id: 1,
            instrument: 0,
            side: Side::Sell,
            qty: 50,
        };
        match req {
            OrderRequest::SubmitMarket { instrument, side, qty, .. } => {
                assert_eq!(instrument, 0);
                assert_eq!(side, Side::Sell);
                assert_eq!(qty, 50);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn order_request_modify_fields() {
        let req = OrderRequest::Modify { new_order_id: 100, order_id: 99, price: 200 * PRICE_SCALE, qty: 10 };
        match req {
            OrderRequest::Modify { order_id, price, qty, .. } => {
                assert_eq!(order_id, 99);
                assert_eq!(price, 200 * PRICE_SCALE);
                assert_eq!(qty, 10);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn order_request_cancel_all_fields() {
        let req = OrderRequest::CancelAll { instrument: 7 };
        match req {
            OrderRequest::CancelAll { instrument } => assert_eq!(instrument, 7),
            _ => panic!("wrong variant"),
        }
    }

    // --- AccountState ---

    #[test]
    fn account_state_default() {
        let a = AccountState::default();
        assert_eq!(a.net_liquidation, 0);
        assert_eq!(a.buying_power, 0);
        assert_eq!(a.margin_used, 0);
        assert_eq!(a.unrealized_pnl, 0);
        assert_eq!(a.realized_pnl, 0);
    }

    #[test]
    fn account_state_copy() {
        let mut a = AccountState::default();
        a.net_liquidation = 100_000 * PRICE_SCALE;
        let b = a; // Copy
        assert_eq!(b.net_liquidation, 100_000 * PRICE_SCALE);
    }

    // --- ControlCommand ---

    #[test]
    fn control_command_subscribe() {
        let cmd = ControlCommand::Subscribe { con_id: 265598, symbol: "AAPL".into() };
        match cmd {
            ControlCommand::Subscribe { con_id, .. } => assert_eq!(con_id, 265598),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn control_command_unsubscribe() {
        let cmd = ControlCommand::Unsubscribe { instrument: 3 };
        match cmd {
            ControlCommand::Unsubscribe { instrument } => assert_eq!(instrument, 3),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn control_command_update_param() {
        let cmd = ControlCommand::UpdateParam { key: "k".into(), value: "v".into() };
        match cmd {
            ControlCommand::UpdateParam { key, value } => {
                assert_eq!(key, "k");
                assert_eq!(value, "v");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn control_command_clone() {
        let cmd = ControlCommand::Subscribe { con_id: 42, symbol: "TEST".into() };
        let cmd2 = cmd.clone();
        match cmd2 {
            ControlCommand::Subscribe { con_id, .. } => assert_eq!(con_id, 42),
            _ => panic!("wrong variant"),
        }
    }

    // --- Fill ---

    #[test]
    fn fill_is_copy() {
        let f = Fill {
            instrument: 0,
            order_id: 1,
            side: Side::Buy,
            price: 150 * PRICE_SCALE,
            qty: 100,
            remaining: 0,
            commission: 0,
            timestamp_ns: 123456789,
        };
        let f2 = f; // Copy
        assert_eq!(f.order_id, f2.order_id);
        assert_eq!(f.timestamp_ns, f2.timestamp_ns);
    }

    // --- Order ---

    #[test]
    fn order_is_copy() {
        let o = Order {
            order_id: 42,
            instrument: 0,
            side: Side::Sell,
            price: 200 * PRICE_SCALE,
            qty: 50,
            filled: 10,
            status: OrderStatus::PartiallyFilled,
        };
        let o2 = o; // Copy
        assert_eq!(o.order_id, o2.order_id);
        assert_eq!(o.filled, o2.filled);
    }

    // --- Side ---

    #[test]
    fn side_equality() {
        assert_eq!(Side::Buy, Side::Buy);
        assert_eq!(Side::Sell, Side::Sell);
        assert_ne!(Side::Buy, Side::Sell);
    }

    // --- OrderStatus ---

    #[test]
    fn order_status_equality() {
        assert_eq!(OrderStatus::Submitted, OrderStatus::Submitted);
        assert_ne!(OrderStatus::Filled, OrderStatus::Cancelled);
        assert_ne!(OrderStatus::PartiallyFilled, OrderStatus::Filled);
    }
}
