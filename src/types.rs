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
    /// Short sell (FIX tag 54 = "5"). Used for short-selling stocks.
    ShortSell,
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
    /// Order state is unknown due to a CCP disconnect.
    /// Will be reconciled when reconnection completes (35=H mass status request).
    Uncertain,
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

/// Cancel/modify reject notification (FIX 35=9).
#[derive(Debug, Clone, Copy)]
pub struct CancelReject {
    pub order_id: OrderId,
    pub instrument: InstrumentId,
    /// 1 = cancel rejected, 2 = modify rejected (FIX tag 434 CxlRejResponseTo).
    pub reject_type: u8,
    /// Numeric reason code (FIX tag 102 CxlRejReason). 0=TooLate, 1=UnknownOrder, etc.
    pub reason_code: i32,
    pub timestamp_ns: u64,
}

/// Multi-char OrdType discriminants (values < 32 to avoid collision with ASCII single-char types).
/// Used in `Order.ord_type` for order types whose FIX tag 40 value is more than one character.
pub const ORD_STP_PRT: u8 = 1;   // FIX "SP"  — Stop with Protection
pub const ORD_MIDPX: u8 = 2;     // FIX "MIDPX" — Mid-Price
pub const ORD_SNAP_MKT: u8 = 3;  // FIX "SMKT" — Snap to Market
pub const ORD_SNAP_MID: u8 = 4;  // FIX "SMID" — Snap to Midpoint
pub const ORD_SNAP_PRI: u8 = 5;  // FIX "SREL" — Snap to Primary
pub const ORD_PEG_MKT: u8 = 6;   // FIX "E" + ExecInst "P" — Pegged to Market
pub const ORD_PEG_MID: u8 = 7;   // FIX "E" + ExecInst "M" — Pegged to Midpoint
pub const ORD_PEG_BENCH: u8 = 8; // FIX "PB" — Pegged to Benchmark
pub const ORD_WHAT_IF: u8 = 9;   // Not a real OrdType — marker for what-if orders

/// Convert an `ord_type` discriminant to the FIX tag 40 string.
/// Single-char types (ASCII >= 32) are stored as-is; multi-char types use constants above.
pub fn ord_type_fix_str(t: u8) -> &'static str {
    match t {
        ORD_STP_PRT => "SP",
        ORD_MIDPX => "MIDPX",
        ORD_SNAP_MKT => "SMKT",
        ORD_SNAP_MID => "SMID",
        ORD_SNAP_PRI => "SREL",
        ORD_PEG_MKT | ORD_PEG_MID => "E",
        ORD_PEG_BENCH => "PB",
        b'1' => "1", b'2' => "2", b'3' => "3", b'4' => "4", b'5' => "5",
        b'B' => "B", b'E' => "E", b'J' => "J", b'K' => "K",
        b'P' => "P", b'R' => "R", b'U' => "U",
        _ => "2",
    }
}

/// What-If margin/commission preview response (FIX 35=8 with tag 6091=1).
/// Returned when a what-if order is submitted — the order is NOT placed.
#[derive(Debug, Clone, Copy)]
pub struct WhatIfResponse {
    pub order_id: OrderId,
    pub instrument: InstrumentId,
    pub init_margin_before: Price,
    pub maint_margin_before: Price,
    pub equity_with_loan_before: Price,
    pub init_margin_after: Price,
    pub maint_margin_after: Price,
    pub equity_with_loan_after: Price,
    pub commission: Price,
}

/// Adjusted order type for adjustable stops (FIX tag 6261).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdjustedOrderType {
    Stop,       // 3
    StopLimit,  // 4
    Trail,      // 7
    TrailLimit, // 8
}

impl AdjustedOrderType {
    pub fn fix_code(&self) -> &'static str {
        match self {
            Self::Stop => "3",
            Self::StopLimit => "4",
            Self::Trail => "7",
            Self::TrailLimit => "8",
        }
    }
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
    /// FIX tag 40 OrdType: b'1'=MKT, b'2'=LMT, b'3'=STP, b'4'=STPLMT, b'P'=TRAIL, etc.
    /// For multi-char OrdTypes (MIDPX, SP, SMKT, etc.), uses ORD_* constants (values < 32).
    pub ord_type: u8,
    /// FIX tag 59 TimeInForce: b'0'=DAY, b'1'=GTC, b'3'=IOC, b'4'=FOK
    pub tif: u8,
    /// FIX tag 99 stop price (for Stop/StopLimit/MIT/LIT orders)
    pub stop_price: Price,
}

impl Order {
    /// Create a new tracked order with FIX type metadata.
    pub fn new(order_id: OrderId, instrument: InstrumentId, side: Side, qty: u32, price: Price, ord_type: u8, tif: u8, stop_price: Price) -> Self {
        Self { order_id, instrument, side, price, qty, filled: 0, status: OrderStatus::PendingSubmit, ord_type, tif, stop_price }
    }
}

/// Adaptive algo priority level (IB's "adaptivePriority" parameter).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdaptivePriority {
    Patient,
    Normal,
    Urgent,
}

impl AdaptivePriority {
    pub fn as_str(&self) -> &'static str {
        match self {
            AdaptivePriority::Patient => "Patient",
            AdaptivePriority::Normal => "Normal",
            AdaptivePriority::Urgent => "Urgent",
        }
    }
}

/// Optional attributes for extended order submissions.
/// All fields default to "not set" (0/false).
#[derive(Debug, Clone, Default)]
pub struct OrderAttrs {
    /// Show on book as this many shares (tag 111). 0 = not set (show full qty).
    pub display_size: u32,
    /// Minimum fill quantity (FIX tag 110). 0 = not set.
    pub min_qty: u32,
    /// Hidden order — not displayed on book (IB tag 6135).
    pub hidden: bool,
    /// Allow trading outside regular hours (IB tag 6433).
    pub outside_rth: bool,
    /// Delay order activation until this time (FIX tag 168). 0 = not set. Unix seconds.
    pub good_after: i64,
    /// Auto-expire order at this time (FIX tag 126). 0 = not set. Unix seconds.
    /// When set, TIF should be GTD (but IB infers it from the tag).
    pub good_till: i64,
    /// OCA group ID (FIX tag 583). 0 = not set. Links orders so one cancels others.
    /// Orders sharing the same non-zero oca_group are in the same OCA group.
    pub oca_group: u64,
    /// Discretionary amount (IB tag 9813). 0 = not set. Fixed-point Price value.
    /// The amount above the limit price that the order may trade at.
    pub discretionary_amt: Price,
    /// Sweep to fill (IB tag 6102). Routes aggressively across exchanges.
    pub sweep_to_fill: bool,
    /// All or none (FIX tag 18=G ExecInst). Fill entire qty or nothing.
    pub all_or_none: bool,
    /// Trigger method for stop/MIT/LIT orders (IB tag 6115).
    /// 0=default, 1=double-bid-ask, 2=last, 3=double-last, 4=bid-ask,
    /// 7=last-or-bid-ask, 8=mid-point.
    pub trigger_method: u8,
    /// Cash quantity — order by dollar amount instead of shares (IB tag 5920). 0 = not set.
    /// Fixed-point Price value (e.g., $1000 = 1000 * PRICE_SCALE).
    pub cash_qty: Price,
    /// Conditions that must be met before the order activates (IB tag 6136+).
    pub conditions: Vec<OrderCondition>,
    /// Cancel order if conditions are no longer met (IB tag 6128). Default false.
    pub conditions_cancel_order: bool,
    /// Evaluate conditions outside regular trading hours (IB tag 6151). Default false.
    pub conditions_ignore_rth: bool,
}

/// A condition that must be met before an order activates.
/// IB evaluates conditions server-side; order stays PreSubmitted until triggered.
#[derive(Debug, Clone)]
pub enum OrderCondition {
    /// Trigger when an instrument's price crosses a threshold.
    Price {
        con_id: i64,
        exchange: String,
        price: Price,
        is_more: bool,
        /// 0=default, 1=last, 2=bid/ask, 3=bid, 4=ask
        trigger_method: u8,
    },
    /// Trigger at a specific time.
    Time {
        /// Format: YYYYMMDD-HH:MM:SS
        time: String,
        is_more: bool,
    },
    /// Trigger based on margin cushion percentage.
    Margin {
        /// Percentage (e.g., 10 = 10%).
        percent: u32,
        is_more: bool,
    },
    /// Trigger when a trade executes on a specific instrument.
    Execution {
        symbol: String,
        exchange: String,
        sec_type: String,
    },
    /// Trigger when volume exceeds a threshold.
    Volume {
        con_id: i64,
        exchange: String,
        volume: i64,
        is_more: bool,
    },
    /// Trigger on percentage price change.
    PercentChange {
        con_id: i64,
        exchange: String,
        percent: f64,
        is_more: bool,
    },
}

/// Risk aversion level for Arrival Price and Close Price algos.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RiskAversion {
    GetDone,
    Aggressive,
    Neutral,
    Passive,
}

impl RiskAversion {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::GetDone => "Get_Done",
            Self::Aggressive => "Aggressive",
            Self::Neutral => "Neutral",
            Self::Passive => "Passive",
        }
    }
}

/// Parameters for IB algorithmic order strategies.
/// Each variant maps to a specific tag 847 algoStrategy with its required params.
#[derive(Debug, Clone)]
pub enum AlgoParams {
    /// VWAP: Volume-weighted average price.
    /// Tag 847=Vwap, 849=max_pct_vol.
    Vwap {
        /// Maximum participation rate (0.0-1.0). Sent as tag 849.
        max_pct_vol: f64,
        /// Don't take liquidity (0 or 1).
        no_take_liq: bool,
        /// Allow algo to continue past end time.
        allow_past_end_time: bool,
        /// Start time in UTC: "YYYYMMDD-HH:MM:SS".
        start_time: String,
        /// End time in UTC: "YYYYMMDD-HH:MM:SS".
        end_time: String,
    },
    /// TWAP: Time-weighted average price.
    /// Tag 847=Twap.
    Twap {
        allow_past_end_time: bool,
        start_time: String,
        end_time: String,
    },
    /// Arrival Price: Minimize arrival price impact.
    /// Tag 847=ArrivalPx, 849=max_pct_vol.
    ArrivalPx {
        max_pct_vol: f64,
        risk_aversion: RiskAversion,
        allow_past_end_time: bool,
        force_completion: bool,
        start_time: String,
        end_time: String,
    },
    /// Close Price: Target closing price.
    /// Tag 847=ClosePx, 849=max_pct_vol.
    ClosePx {
        max_pct_vol: f64,
        risk_aversion: RiskAversion,
        force_completion: bool,
        start_time: String,
    },
    /// Dark Ice: Hidden iceberg algo.
    /// Tag 847=DarkIce.
    DarkIce {
        allow_past_end_time: bool,
        display_size: u32,
        start_time: String,
        end_time: String,
    },
    /// Percentage of Volume: Participate at % of volume.
    /// Tag 847=PctVol.
    PctVol {
        /// Target participation rate (0.0-1.0). Sent as param pctVol.
        pct_vol: f64,
        no_take_liq: bool,
        start_time: String,
        end_time: String,
    },
}

/// Order request written by strategy, drained by engine after on_tick.
#[derive(Debug, Clone)]
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
    SubmitStopGtc {
        order_id: OrderId,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        stop_price: Price,
        outside_rth: bool,
    },
    SubmitStopLimitGtc {
        order_id: OrderId,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price: Price,
        stop_price: Price,
        outside_rth: bool,
    },
    SubmitLimitIoc {
        order_id: OrderId,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price: Price,
    },
    SubmitLimitFok {
        order_id: OrderId,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price: Price,
    },
    SubmitTrailingStop {
        order_id: OrderId,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        trail_amt: Price,
    },
    SubmitTrailingStopLimit {
        order_id: OrderId,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price: Price,
        trail_amt: Price,
    },
    /// Trailing stop by percentage (tag 6268). Trail percent is in basis points (1% = 100).
    SubmitTrailingStopPct {
        order_id: OrderId,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        trail_pct: u32, // basis points: 100 = 1%, 250 = 2.5%
    },
    SubmitMoc {
        order_id: OrderId,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
    },
    SubmitLoc {
        order_id: OrderId,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price: Price,
    },
    SubmitMit {
        order_id: OrderId,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        stop_price: Price,
    },
    SubmitLit {
        order_id: OrderId,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price: Price,
        stop_price: Price,
    },
    /// Bracket order: parent entry + take-profit + stop-loss, linked via OCA.
    /// Generates 3 FIX messages: parent (35=D), TP child (35=D with 6107+583), SL child (35=D with 6107+583).
    SubmitBracket {
        parent_id: OrderId,
        tp_id: OrderId,
        sl_id: OrderId,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        entry_price: Price,
        take_profit: Price,
        stop_loss: Price,
    },
    /// Extended limit order with optional attributes (display size, hidden, GAT, GTD).
    SubmitLimitEx {
        order_id: OrderId,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price: Price,
        tif: u8,
        attrs: OrderAttrs,
    },
    /// Relative / Pegged-to-Primary order: pegs to NBBO with optional offset.
    SubmitRel {
        order_id: OrderId,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        offset: Price, // peg offset in tag 99
    },
    /// Limit order for opening auction (TIF=OPG).
    SubmitLimitOpg {
        order_id: OrderId,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price: Price,
    },
    /// Adaptive algo limit order: LMT with IB Adaptive algorithm overlay.
    SubmitAdaptive {
        order_id: OrderId,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price: Price,
        priority: AdaptivePriority,
    },
    /// Market to Limit: fills at market, remainder converts to limit at fill price. OrdType K.
    SubmitMtl {
        order_id: OrderId,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
    },
    /// Market with Protection: market order with price protection for futures. OrdType U.
    SubmitMktPrt {
        order_id: OrderId,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
    },
    /// Stop with Protection: stop order with price protection. OrdType SP.
    SubmitStpPrt {
        order_id: OrderId,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        stop_price: Price,
    },
    /// Mid-Price: pegs to midpoint with optional price cap. OrdType MIDPX.
    SubmitMidPrice {
        order_id: OrderId,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price_cap: Price, // 0 = no cap
    },
    /// Snap to Market: snaps to market price. OrdType SMKT.
    SubmitSnapMkt {
        order_id: OrderId,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
    },
    /// Snap to Midpoint: snaps to midpoint. OrdType SMID.
    SubmitSnapMid {
        order_id: OrderId,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
    },
    /// Snap to Primary: snaps to primary (NBBO). OrdType SREL.
    SubmitSnapPri {
        order_id: OrderId,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
    },
    /// Pegged to Market: pegs to market with optional offset. OrdType E + ExecInst P.
    SubmitPegMkt {
        order_id: OrderId,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        offset: Price, // peg offset, 0 = no offset
    },
    /// Pegged to Midpoint: pegs to midpoint with optional offset. OrdType E + ExecInst M.
    SubmitPegMid {
        order_id: OrderId,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        offset: Price, // peg offset, 0 = no offset
    },
    /// Algorithmic order: limit order with IB algo strategy overlay (VWAP, TWAP, etc.).
    SubmitAlgo {
        order_id: OrderId,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price: Price,
        algo: AlgoParams,
    },
    /// Pegged to Benchmark: pegs to a benchmark instrument's price. OrdType PB.
    /// Companion tags: 6941=refConId, 6938=isPegDecrease, 6939=pegChangeAmt, 6942=refChangeAmt.
    SubmitPegBench {
        order_id: OrderId,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price: Price,
        ref_con_id: u32,
        is_peg_decrease: bool,
        pegged_change_amount: Price,
        ref_change_amount: Price,
    },
    /// Limit order for auction (TIF=AUC, tag 59=8). Participates in exchange opening/closing auction.
    SubmitLimitAuc {
        order_id: OrderId,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price: Price,
    },
    /// Market-to-Limit for auction (TIF=AUC, tag 59=8). MTL + auction participation.
    SubmitMtlAuc {
        order_id: OrderId,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
    },
    /// What-If order: sends a limit order with tag 6091=1 for margin/commission preview.
    /// The order is NOT placed — response comes back as 35=8 with margin fields.
    SubmitWhatIf {
        order_id: OrderId,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price: Price,
    },
    /// Fractional shares limit order. Qty is fixed-point (QTY_SCALE = 10^4).
    /// E.g., 0.5 shares = 5000. Tag 38 sent as decimal string.
    SubmitLimitFractional {
        order_id: OrderId,
        instrument: InstrumentId,
        side: Side,
        qty: Qty, // QTY_SCALE fixed-point
        price: Price,
    },
    /// Adjustable stop: a stop order that adjusts to a different order type when trigger_price is hit.
    /// Tags: 6257=1, 6261=adjusted type, 6258=trigger, 6259=adjusted stop, 6262=adjusted limit.
    SubmitAdjustableStop {
        order_id: OrderId,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        stop_price: Price,
        trigger_price: Price,
        adjusted_order_type: AdjustedOrderType,
        adjusted_stop_price: Price,
        /// Only used when adjusted_order_type is StopLimit or TrailLimit. 0 = not set.
        adjusted_stop_limit_price: Price,
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

/// Tick-by-tick data type for subscription requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TbtType {
    /// Last trade ticks (AllLast).
    Last,
    /// Bid/ask quote ticks (BidAsk).
    BidAsk,
}

/// A single tick-by-tick trade (AllLast) from 35=E.
#[derive(Debug, Clone)]
pub struct TbtTrade {
    pub instrument: InstrumentId,
    pub price: Price,
    pub size: i64,
    pub timestamp: u64,
    pub exchange: String,
    pub conditions: String,
}

/// A single tick-by-tick bid/ask quote from 35=E.
#[derive(Debug, Clone, Copy)]
pub struct TbtQuote {
    pub instrument: InstrumentId,
    pub bid: Price,
    pub ask: Price,
    pub bid_size: i64,
    pub ask_size: i64,
    pub timestamp: u64,
}

/// Commands sent from the control plane to the hot loop via SPSC channel.
#[derive(Debug, Clone)]
pub enum ControlCommand {
    /// Subscribe to market data for a contract.
    Subscribe { con_id: i64, symbol: String },
    /// Unsubscribe from market data for an instrument.
    Unsubscribe { instrument: InstrumentId },
    /// Subscribe to tick-by-tick data via HMDS.
    SubscribeTbt { con_id: i64, symbol: String, tbt_type: TbtType },
    /// Unsubscribe from tick-by-tick data.
    UnsubscribeTbt { instrument: InstrumentId },
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
        let req2 = req.clone();
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
            ord_type: b'2',
            tif: b'0',
            stop_price: 0,
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

    // --- WhatIfResponse ---

    #[test]
    fn what_if_response_is_copy() {
        let r = WhatIfResponse {
            order_id: 1,
            instrument: 0,
            init_margin_before: 1364_01 * (PRICE_SCALE / 100),
            maint_margin_before: 1131_67 * (PRICE_SCALE / 100),
            equity_with_loan_before: 754_255_14 * (PRICE_SCALE / 100),
            init_margin_after: 8957_86 * (PRICE_SCALE / 100),
            maint_margin_after: 8143_51 * (PRICE_SCALE / 100),
            equity_with_loan_after: 754_255_14 * (PRICE_SCALE / 100),
            commission: 1 * PRICE_SCALE,
        };
        let r2 = r; // Copy
        assert_eq!(r.init_margin_after, r2.init_margin_after);
        assert_eq!(r.commission, r2.commission);
    }

    // --- AdjustedOrderType ---

    #[test]
    fn adjusted_order_type_fix_codes() {
        assert_eq!(AdjustedOrderType::Stop.fix_code(), "3");
        assert_eq!(AdjustedOrderType::StopLimit.fix_code(), "4");
        assert_eq!(AdjustedOrderType::Trail.fix_code(), "7");
        assert_eq!(AdjustedOrderType::TrailLimit.fix_code(), "8");
    }

    // --- OrderAttrs cash_qty ---

    #[test]
    fn order_attrs_cash_qty_default_zero() {
        let attrs = OrderAttrs::default();
        assert_eq!(attrs.cash_qty, 0);
    }
}
