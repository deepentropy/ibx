//! Bridge module: connects the Rust HotLoop to external callers (Python via PyO3).
//!
//! Architecture:
//! - `SharedState` holds SeqLock-protected quotes, concurrent event queues, and an order channel.
//! - `BridgeStrategy` implements `Strategy` — copies quotes to shared state, pushes fills/updates.
//! - External callers read snapshots and poll events without blocking the hot loop.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::cell::UnsafeCell;

use crate::engine::context::{Context, Strategy};
use crate::types::*;

/// SeqLock-protected quote slot. Writer (hot loop) never blocks.
/// Reader retries if it catches a write in progress.
#[repr(C)]
pub struct SeqQuote {
    version: AtomicU64,
    data: UnsafeCell<Quote>,
}

// SAFETY: SeqQuote is designed for single-writer (hot loop) + multiple-reader (Python).
// The version counter ensures readers see consistent data.
unsafe impl Sync for SeqQuote {}
unsafe impl Send for SeqQuote {}

impl SeqQuote {
    pub fn new() -> Self {
        Self {
            version: AtomicU64::new(0),
            data: UnsafeCell::new(Quote::default()),
        }
    }

    /// Write a quote (hot loop side). Never blocks.
    #[inline]
    pub fn write(&self, quote: &Quote) {
        let v = self.version.load(Ordering::Relaxed);
        self.version.store(v + 1, Ordering::Release); // odd = writing
        unsafe { *self.data.get() = *quote; }
        self.version.store(v + 2, Ordering::Release); // even = stable
    }

    /// Read a consistent quote snapshot (reader side). Spins on conflict.
    #[inline]
    pub fn read(&self) -> Quote {
        loop {
            let v1 = self.version.load(Ordering::Acquire);
            if v1 & 1 != 0 { continue; } // writer active
            let q = unsafe { *self.data.get() };
            let v2 = self.version.load(Ordering::Acquire);
            if v1 == v2 { return q; }
        }
    }
}

/// Shared state between hot loop and external caller.
pub struct SharedState {
    quotes: Box<[SeqQuote; MAX_INSTRUMENTS]>,
    fills: Mutex<Vec<Fill>>,
    order_updates: Mutex<Vec<OrderUpdate>>,
    positions: [AtomicU64; MAX_INSTRUMENTS],
    account: Mutex<AccountState>,
    /// InstrumentId counter — set by hot loop on RegisterInstrument.
    instrument_count: AtomicU64,
}

impl SharedState {
    pub fn new() -> Self {
        Self {
            quotes: Box::new(std::array::from_fn(|_| SeqQuote::new())),
            fills: Mutex::new(Vec::with_capacity(64)),
            order_updates: Mutex::new(Vec::with_capacity(64)),
            positions: std::array::from_fn(|_| AtomicU64::new(0)),
            account: Mutex::new(AccountState::default()),
            instrument_count: AtomicU64::new(0),
        }
    }

    /// Read a quote snapshot (lock-free via SeqLock).
    #[inline]
    pub fn quote(&self, id: InstrumentId) -> Quote {
        self.quotes[id as usize].read()
    }

    /// Drain all pending fills.
    pub fn drain_fills(&self) -> Vec<Fill> {
        let mut lock = self.fills.lock().unwrap();
        std::mem::take(&mut *lock)
    }

    /// Drain all pending order updates.
    pub fn drain_order_updates(&self) -> Vec<OrderUpdate> {
        let mut lock = self.order_updates.lock().unwrap();
        std::mem::take(&mut *lock)
    }

    /// Read current position for an instrument.
    pub fn position(&self, id: InstrumentId) -> i64 {
        self.positions[id as usize].load(Ordering::Relaxed) as i64
    }

    /// Read account state snapshot.
    pub fn account(&self) -> AccountState {
        *self.account.lock().unwrap()
    }

    /// Number of registered instruments.
    pub fn instrument_count(&self) -> u32 {
        self.instrument_count.load(Ordering::Relaxed) as u32
    }

    // ── Hot-loop-side writers ──

    fn push_quote(&self, id: InstrumentId, quote: &Quote) {
        self.quotes[id as usize].write(quote);
    }

    fn push_fill(&self, fill: Fill) {
        self.fills.lock().unwrap().push(fill);
    }

    fn push_order_update(&self, update: OrderUpdate) {
        self.order_updates.lock().unwrap().push(update);
    }

    fn set_position(&self, id: InstrumentId, pos: i64) {
        self.positions[id as usize].store(pos as u64, Ordering::Relaxed);
    }

    fn set_account(&self, account: &AccountState) {
        *self.account.lock().unwrap() = *account;
    }

    fn set_instrument_count(&self, count: u32) {
        self.instrument_count.store(count as u64, Ordering::Relaxed);
    }
}

/// Strategy implementation that bridges the hot loop to SharedState.
/// No-op on_tick — just syncs quotes/positions/account to shared state.
pub struct BridgeStrategy {
    shared: std::sync::Arc<SharedState>,
}

impl BridgeStrategy {
    pub fn new(shared: std::sync::Arc<SharedState>) -> Self {
        Self { shared }
    }
}

impl Strategy for BridgeStrategy {
    fn on_tick(&mut self, instrument: InstrumentId, ctx: &mut Context) {
        // Sync quote to shared state (SeqLock write, never blocks)
        self.shared.push_quote(instrument, ctx.quote(instrument));
        // Sync position
        self.shared.set_position(instrument, ctx.position(instrument));
        // Sync account
        self.shared.set_account(ctx.account());
        // Sync instrument count
        self.shared.set_instrument_count(ctx.market.count());
    }

    fn on_fill(&mut self, fill: &Fill, ctx: &mut Context) {
        self.shared.push_fill(*fill);
        self.shared.set_position(fill.instrument, ctx.position(fill.instrument));
    }

    fn on_order_update(&mut self, update: &OrderUpdate, ctx: &mut Context) {
        self.shared.push_order_update(*update);
        let _ = ctx;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seqquote_write_read_roundtrip() {
        let sq = SeqQuote::new();
        let mut q = Quote::default();
        q.bid = 150 * PRICE_SCALE;
        q.ask = 151 * PRICE_SCALE;
        sq.write(&q);
        let read = sq.read();
        assert_eq!(read.bid, 150 * PRICE_SCALE);
        assert_eq!(read.ask, 151 * PRICE_SCALE);
    }

    #[test]
    fn seqquote_default_is_zero() {
        let sq = SeqQuote::new();
        let q = sq.read();
        assert_eq!(q.bid, 0);
        assert_eq!(q.ask, 0);
    }

    #[test]
    fn shared_state_fills_drain() {
        let ss = SharedState::new();
        ss.push_fill(Fill {
            instrument: 0, order_id: 1, side: Side::Buy,
            price: 100 * PRICE_SCALE, qty: 10, remaining: 0,
            commission: 0, timestamp_ns: 0,
        });
        ss.push_fill(Fill {
            instrument: 0, order_id: 2, side: Side::Sell,
            price: 101 * PRICE_SCALE, qty: 5, remaining: 0,
            commission: 0, timestamp_ns: 0,
        });
        let fills = ss.drain_fills();
        assert_eq!(fills.len(), 2);
        // Second drain should be empty
        assert!(ss.drain_fills().is_empty());
    }

    #[test]
    fn shared_state_order_updates_drain() {
        let ss = SharedState::new();
        ss.push_order_update(OrderUpdate {
            order_id: 1, instrument: 0, status: OrderStatus::Submitted,
            filled_qty: 0, remaining_qty: 100, timestamp_ns: 0,
        });
        let updates = ss.drain_order_updates();
        assert_eq!(updates.len(), 1);
        assert!(ss.drain_order_updates().is_empty());
    }

    #[test]
    fn shared_state_position_roundtrip() {
        let ss = SharedState::new();
        assert_eq!(ss.position(0), 0);
        ss.set_position(0, 42);
        assert_eq!(ss.position(0), 42);
        ss.set_position(0, -10);
        assert_eq!(ss.position(0), -10);
    }

    #[test]
    fn shared_state_account_roundtrip() {
        let ss = SharedState::new();
        let mut a = AccountState::default();
        a.net_liquidation = 100_000 * PRICE_SCALE;
        ss.set_account(&a);
        let read = ss.account();
        assert_eq!(read.net_liquidation, 100_000 * PRICE_SCALE);
    }

    #[test]
    fn seqquote_concurrent_read_write() {
        use std::sync::Arc;
        use std::thread;

        let sq = Arc::new(SeqQuote::new());
        let sq_writer = sq.clone();
        let sq_reader = sq.clone();

        let writer = thread::spawn(move || {
            for i in 0..1000 {
                let mut q = Quote::default();
                q.bid = i * PRICE_SCALE;
                q.ask = (i + 1) * PRICE_SCALE;
                sq_writer.write(&q);
            }
        });

        let reader = thread::spawn(move || {
            for _ in 0..1000 {
                let q = sq_reader.read();
                // bid and ask should be consistent (ask = bid + PRICE_SCALE)
                if q.bid != 0 {
                    assert_eq!(q.ask, q.bid + PRICE_SCALE);
                }
            }
        });

        writer.join().unwrap();
        reader.join().unwrap();
    }
}
