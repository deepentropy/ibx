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
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price: Price,
    },
    SubmitMarket {
        instrument: InstrumentId,
        side: Side,
        qty: u32,
    },
    Cancel {
        order_id: OrderId,
    },
    CancelAll {
        instrument: InstrumentId,
    },
    Modify {
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
    Subscribe { con_id: i64 },
    /// Unsubscribe from market data for an instrument.
    Unsubscribe { instrument: InstrumentId },
    /// Update a strategy parameter.
    UpdateParam { key: String, value: String },
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
}
