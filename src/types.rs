/// Internal instrument identifier. Mapped from IB's conId at subscription time.
/// Used as an index into pre-allocated arrays, so values are dense and small.
pub type InstrumentId = u32;

/// Order identifier: the API order id, or the server's for an order of
/// an earlier session. Signed, as the API's ids; 64 bits, as the server's
/// ids can be wider than the API's 32-bit ints.
pub type OrderId = i64;

/// Request and ticker id, as the API gives it. Signed like the
/// reference's, whose ids are 32-bit ints; -1 names no request.
pub type ReqId = i64;

/// Fixed-point price: value * 10^8. Avoids floating-point on the hot path.
/// Example: $150.25 = 15_025_000_000
pub type Price = i64;

/// Fixed-point quantity: value * 10^4. Matches IB's 0.0001 minimum increment.
/// Example: 100 shares = 1_000_000
pub type Qty = i64;

pub const PRICE_SCALE: i64 = 100_000_000; // 10^8
pub const QTY_SCALE: i64 = 10_000; // 10^4

/// Whether a price the reference checks is off the contract's price grid
/// (ibx#263, reversing the snapping of ibx#216): negative where the rule
/// does not allow it (ib-agent#192 B5, a REL offset of -0.50 refused with
/// 110), or not a multiple of `tick` when the tick is known (a
/// non-positive tick: unknown, not checked). The rule allows a price at or
/// below 0 only when it says so (`jmarketrules.o.o(double)`: above 0, or
/// its flag `ak`): a combo's rule does (`jclient.dy.cP()`, captured BAG
/// limit prices of -73.15 and -50.10, ibx#470), a stock's does not.
/// The reference refuses such an order with 110 and sends nothing
/// (`jextend.dx.a(dy, boolean)@1886-1961`, `trader.common.b9.a(OrderCreator,
/// o)`); it does not round it.
pub fn off_grid(price: Price, tick: i64, signed: bool) -> bool {
    (price < 0 && !signed) || (tick > 0 && price % tick != 0)
}

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
    /// Received by server, not yet accepted by exchange (FIX 39=A).
    PreSubmitted,
    /// Accepted and working on exchange (FIX 39=0 or 39=5).
    Submitted,
    /// Cancel request sent, awaiting confirmation (FIX 39=6).
    PendingCancel,
    /// Modify request sent, awaiting confirmation (FIX 39=E).
    PendingReplace,
    Filled,
    PartiallyFilled,
    Cancelled,
    Rejected,
    /// Server reports order inactive (FIX 39=I).
    Inactive,
    /// An order the client placed that never left: a global cancel came
    /// while it waited (the reference's `ApiCancelled`).
    ApiCancelled,
}

impl OrderStatus {
    /// Lifecycle progress rank for the monotonic status guard (ibx#212).
    /// A stale or reordered frame must not move an order's reported status
    /// backwards (e.g. a late PreSubmitted after Submitted, or a mass-status
    /// snapshot after a fill). Same-rank transitions are free — the tiers
    /// group states that legitimately alternate. Deliberate regressions
    /// (cancel-reject restore, disconnect reconciliation) bypass the guard
    /// via `Context::set_order_status_forced`.
    pub fn rank(self) -> u8 {
        match self {
            Self::PendingSubmit => 1,
            Self::PreSubmitted => 2,
            // Working tier: a modify ack returns PendingReplace to
            // Submitted, and Inactive orders can reactivate.
            Self::Submitted | Self::PendingReplace | Self::Inactive => 3,
            // A partially filled order can still be cancelled, and a fill
            // can land while a cancel is pending.
            Self::PendingCancel | Self::PartiallyFilled => 4,
            Self::Filled | Self::Cancelled | Self::Rejected | Self::ApiCancelled => 5,
        }
    }

    /// Terminal states are absorbing: no ordinary frame may leave them.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Filled | Self::Cancelled | Self::Rejected | Self::ApiCancelled)
    }
}

/// Current quote for an instrument. Cache-line aligned for hot-path access.
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
    /// Last trade time, ns since the epoch (the server gives whole seconds).
    pub timestamp_ns: u64,
    /// Bid-exchange bitmask. Each set bit indexes into smart_components by bit_number.
    /// Hypothesis pending wire-format confirmation; see deepentropy/ib-agent#120.
    pub bid_exch_mask: i64,
    pub ask_exch_mask: i64,
    pub last_exch_mask: i64,
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
            bid_exch_mask: 0,
            ask_exch_mask: 0,
            last_exch_mask: 0,
        }
    }
}

/// What the farm told about a quote beside its prices and sizes (ibx#446),
/// packed in one word: whether the bid and the ask can execute
/// automatically, the trading status the last trade came with, and which
/// sizes came at least once. The engine keeps it per instrument for the
/// catch-up steps of the market data queue (`md_events`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QuoteMarks(pub u64);

impl QuoteMarks {
    const BID_AUTO: u32 = 0;
    const ASK_AUTO: u32 = 2;
    const HALTED_KNOWN: u64 = 1 << 4;
    const HALTED_SHIFT: u32 = 5;
    const SEEN_SHIFT: u32 = 8;

    fn flag(self, shift: u32) -> Option<bool> {
        match (self.0 >> shift) & 3 {
            1 => Some(false),
            2 => Some(true),
            _ => None,
        }
    }

    fn set_flag(&mut self, shift: u32, on: bool) {
        self.0 = (self.0 & !(3 << shift)) | ((if on { 2 } else { 1 }) << shift);
    }

    /// Whether the bid can execute automatically; `None` before the farm said.
    pub fn bid_auto(self) -> Option<bool> { self.flag(Self::BID_AUTO) }
    /// Whether the ask can execute automatically; `None` before the farm said.
    pub fn ask_auto(self) -> Option<bool> { self.flag(Self::ASK_AUTO) }
    pub fn set_bid_auto(&mut self, on: bool) { self.set_flag(Self::BID_AUTO, on) }
    pub fn set_ask_auto(&mut self, on: bool) { self.set_flag(Self::ASK_AUTO, on) }

    /// Both flags from the farm's bits: 4 the bid, 8 the ask.
    pub fn set_auto_bits(&mut self, bits: i64) {
        self.set_bid_auto(bits & 4 != 0);
        self.set_ask_auto(bits & 8 != 0);
    }

    /// Both auto-execution flags as one byte (0: the farm said nothing).
    pub fn auto_word(self) -> u8 { (self.0 & 0xF) as u8 }
    /// The flags of an `auto_word`.
    pub fn from_auto_word(word: u8) -> Self { Self(word as u64 & 0xF) }

    /// The trading status of the last trade, as its two low bits (1 halted,
    /// 2 volatility halted); `None` before a trade gave one.
    pub fn halted(self) -> Option<i64> {
        (self.0 & Self::HALTED_KNOWN != 0).then_some(((self.0 >> Self::HALTED_SHIFT) & 3) as i64)
    }

    /// A trade's status; -1 is no status.
    pub fn set_halted(&mut self, status: i64) {
        if status == -1 {
            return;
        }
        self.0 = (self.0 & !(3 << Self::HALTED_SHIFT)) | Self::HALTED_KNOWN | (((status & 3) as u64) << Self::HALTED_SHIFT);
    }

    /// The API value of the halted tick for a status: 1 halted, else 2
    /// volatility halted, else 0.
    pub fn halted_tick_value(status: i64) -> f64 {
        if status & 1 != 0 { 1.0 } else if status & 2 != 0 { 2.0 } else { 0.0 }
    }

    /// Whether a size or the volume came from the farm at least once: the
    /// reference sends a first size even when it is 0.
    pub fn seen(self, size: SizeKind) -> bool { self.0 & (1 << (Self::SEEN_SHIFT + size as u32)) != 0 }
    pub fn set_seen(&mut self, size: SizeKind) { self.0 |= 1 << (Self::SEEN_SHIFT + size as u32) }
    /// The sizes seen as one byte (`SizeKind` bits).
    pub fn seen_word(self) -> u8 { ((self.0 >> Self::SEEN_SHIFT) & 0xF) as u8 }
}

/// A size kind for `QuoteMarks::seen`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeKind { Bid = 0, Ask = 1, Last = 2, Volume = 3 }

/// Execution fill report.
#[derive(Debug, Clone, Copy)]
pub struct Fill {
    pub instrument: InstrumentId,
    pub order_id: OrderId,
    pub side: Side,
    /// Price of this print.
    pub price: Price,
    /// Size of this print, fixed-point (QTY_SCALE).
    pub qty_fixed: Qty,
    /// Quantity still working, fixed-point (QTY_SCALE).
    pub remaining_fixed: Qty,
    /// Quantity filled on the order so far, this print included,
    /// fixed-point (QTY_SCALE). 0 when the report did not carry it.
    pub cum_qty_fixed: Qty,
    /// Average price over every print of the order so far. 0 when the
    /// report did not carry it.
    pub avg_price: Price,
    pub commission: Price,
    pub timestamp_ns: u64,
}

impl Fill {
    /// Quantity filled on the order so far; the print when the report did
    /// not carry the total.
    pub fn filled_so_far_fixed(&self) -> Qty {
        if self.cum_qty_fixed > 0 { self.cum_qty_fixed } else { self.qty_fixed }
    }

    /// Average price over the order's prints so far; the print price when
    /// the report did not carry it.
    pub fn average_price(&self) -> Price {
        if self.avg_price > 0 { self.avg_price } else { self.price }
    }
}

/// Order status change notification.
#[derive(Debug, Clone, Copy)]
pub struct OrderUpdate {
    pub order_id: OrderId,
    pub instrument: InstrumentId,
    pub status: OrderStatus,
    /// Fixed-point (QTY_SCALE).
    pub filled_qty_fixed: Qty,
    /// Fixed-point (QTY_SCALE).
    pub remaining_qty_fixed: Qty,
    /// Average price over the order's prints so far; 0 before any fill or
    /// when the report did not carry it.
    pub avg_fill_price: Price,
    pub perm_id: i64,
    pub parent_id: i64,
    pub timestamp_ns: u64,
}

/// Cancel/modify reject notification (reject message).
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
pub const ORD_PEG_MKT: u8 = 6;   // FIX "P" + ExecInst "P" — Pegged to Market
pub const ORD_PEG_MID: u8 = 7;   // FIX "P" + ExecInst "M" — Pegged to Midpoint
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
        ORD_PEG_MKT | ORD_PEG_MID => "P",
        ORD_PEG_BENCH => "PB",
        b'1' => "1", b'2' => "2", b'3' => "3", b'4' => "4", b'5' => "5",
        b'B' => "B", b'E' => "E", b'J' => "J", b'K' => "K",
        b'P' => "P", b'R' => "R", b'U' => "U",
        _ => "2",
    }
}

/// What-If margin/commission preview response (execution report with tag 6091=1).
/// Returned when a what-if order is submitted — the order is NOT placed.
#[derive(Debug, Clone, Default)]
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
    /// The reply as the reference reports it to the API (ibx#462).
    pub state: WhatIfState,
    /// The reply that ends the preview: false for the frame that carries
    /// only an order message, after which the preview still waits.
    pub final_reply: bool,
}

/// One what-if reply as the server sent it (ibx#462): the values that
/// came, unset when absent or not a number.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WhatIfState {
    /// The API status of the reply's order status (39=A is PreSubmitted).
    pub status: String,
    pub init_margin_before: Option<f64>,
    pub maint_margin_before: Option<f64>,
    pub equity_with_loan_before: Option<f64>,
    pub init_margin_after: Option<f64>,
    pub maint_margin_after: Option<f64>,
    pub equity_with_loan_after: Option<f64>,
    pub commission: Option<f64>,
    pub commission_currency: String,
    pub margin_currency: String,
    /// The suggested size, empty when the server sent none.
    pub suggested_size: String,
    /// The order message of the reply (warning text).
    pub warning_text: String,
    /// The server's reason when it refuses the order (error 201 after the
    /// open order).
    pub reject_reason: String,
    /// The permId of the preview's order (37 of the reply); 0 without one.
    pub perm_id: i64,
    /// The conId of the reply (6008), for a preview placed without one: the
    /// reference shows the contract it looked up (ibx#486).
    pub con_id: i64,
}

/// Adjusted order type for adjustable stops (FIX tag 6261).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdjustedOrderType {
    Stop,       // 3
    StopLimit,  // 4
    Trail,      // T
    TrailLimit, // TSL
}

impl AdjustedOrderType {
    /// The order-type code, the same one the base order type rides on.
    /// Stop and Trail captured (ib-agent#167, ib-agent#192); StopLimit and
    /// TrailLimit from the reference order-type table. The earlier 7/8 were
    /// not codes the reference ever sends (ibx#240).
    pub fn fix_code(&self) -> &'static str {
        match self {
            Self::Stop => "3",
            Self::StopLimit => "4",
            Self::Trail => "T",
            Self::TrailLimit => "TSL",
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
    /// Order quantity, fixed-point (QTY_SCALE).
    pub qty_fixed: Qty,
    /// Filled so far, fixed-point (QTY_SCALE).
    pub filled_fixed: Qty,
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
    /// Create a new tracked order with its order-type metadata. `qty` is in
    /// whole shares; it is stored fixed-point.
    pub fn new(order_id: OrderId, instrument: InstrumentId, side: Side, qty: u32, price: Price, ord_type: u8, tif: u8, stop_price: Price) -> Self {
        Self { order_id, instrument, side, price, qty_fixed: qty as Qty * QTY_SCALE, filled_fixed: 0, status: OrderStatus::PendingSubmit, ord_type, tif, stop_price }
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

/// Time-in-force code of a DTC order. It is sent as GTC with the DTC flag
/// (ibx#467).
pub const TIF_DTC: u8 = b'r';

/// Optional attributes for extended order submissions.
/// All fields default to "not set" (0/false).
#[derive(Debug, Clone, Default)]
pub struct OrderAttrs {
    /// The caller's orderRef, sent in tag 6010 on the order and every
    /// replace (ibx#466).
    pub order_ref: String,
    /// Show on book as this many shares (tag 111). 0 = not set (show full qty).
    pub display_size: u32,
    /// Minimum fill quantity (FIX tag 110). 0 = not set.
    pub min_qty: u32,
    /// Hidden order — not displayed on book (IB tag 6135).
    pub hidden: bool,
    /// Customer account (tag 6207) and professional customer (tag 6636),
    /// sent only on an account whose config allows them (ibx#425).
    pub customer_account: String,
    pub professional_customer: bool,
    /// Allow trading outside regular hours (IB tag 6433).
    pub outside_rth: bool,
    /// Delay order activation until this time (FIX tag 168). 0 = not set. Unix seconds.
    pub good_after: i64,
    /// Auto-expire order at this instant (FIX tag 126, time-precise GTD).
    /// 0 = not set. Unix seconds in UTC. Mutually exclusive with `good_till_date_ymd`.
    /// When set, TIF should be GTD (but IB infers it from the tag).
    pub good_till: i64,
    /// Auto-expire order on this calendar date (FIX tag 432, date-only GTD).
    /// 0 = not set. Packed `YYYYMMDD`. Mutually exclusive with `good_till`.
    pub good_till_date_ymd: u32,
    /// OCA group ID (FIX tag 583). 0 = not set. Links orders so one cancels others.
    /// Orders sharing the same non-zero oca_group are in the same OCA group.
    pub oca_group: u64,
    /// OCA group as a string (FIX tag 583). Used by Python compat for user-specified OCA names.
    /// When non-empty, takes precedence over numeric `oca_group`.
    pub oca_group_str: String,
    /// Parent order ID (IB tag 6107). 0 = no parent. Links child orders to parent in brackets.
    pub parent_id: OrderId,
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
    /// Cash quantity — order by dollar amount instead of shares (tag 152,
    /// ibx#263). 0 = not set.
    /// Fixed-point Price value (e.g., $1000 = 1000 * PRICE_SCALE).
    pub cash_qty: Price,
    /// Conditions that must be met before the order activates (IB tag 6136+).
    pub conditions: Vec<OrderCondition>,
    /// Cancel order if conditions are no longer met (IB tag 6128). Default false.
    pub conditions_cancel_order: bool,
    /// Evaluate conditions outside regular trading hours (IB tag 6151). Default false.
    pub conditions_ignore_rth: bool,
    /// OCA cancellation semantics (IB tag 6209), 1..=4. 0 = not set, which
    /// emits the gateway default 3 (ReduceOnFillNonBlock). Only emitted when
    /// an OCA group is present. See ibx#215.
    pub oca_type: u8,
    /// Reference exchange of a pegged-to-benchmark order; empty = not set
    /// (ibx#415). Not sent for other order types.
    pub reference_exchange: String,
    /// Work the order in the overnight session too (API includeOvernight),
    /// sent as an order attribute (ibx#467).
    pub include_overnight: bool,
    /// The API clearingIntent: empty, IB, Away or PTA (ibx#417). A
    /// clearing away from the broker lets a short-side order pass the
    /// reference's side check.
    pub clearing_intent: String,
    /// The short-sale instructions of a short-side order (ibx#417).
    pub short_sale: ShortSale,
    /// The API usePriceMgmtAlgo: None when unset (ibx#492).
    pub use_price_mgmt_algo: Option<bool>,
    /// The order's algo, None for none. The reference writes it on top of
    /// the order's own type and price fields, whatever the type (ibx#263).
    pub algo: Option<OrderAlgo>,
    /// The combo of a BAG order, None for any other order (ibx#470).
    pub combo: Option<Box<ComboSpec>>,
}

/// A combo (BAG) order as the caller gave it (ibx#470): the engine builds
/// the combo from it with the reference's set-up requests before the
/// order goes out.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ComboSpec {
    /// The BAG contract's exchange as given: SMART for a smart combo.
    pub exchange: String,
    pub currency: String,
    /// The BAG symbol as given, checked against the legs (478).
    pub symbol: String,
    /// The conId of the smart combo of the currency (logon tag 6611), 0
    /// for a directed combo.
    pub smart_con_id: i64,
    /// The legs in the caller's order.
    pub legs: Vec<ComboLegSpec>,
    /// The per-leg prices (orderComboLegs) in the caller's leg order;
    /// empty when the order has none.
    pub leg_prices: Vec<Price>,
    /// The smartComboRoutingParams as order attributes, (tag, value) in
    /// the caller's order.
    pub routing_attrs: Vec<(u32, String)>,
    /// NonGuaranteed=1 among the routing parameters.
    pub non_guaranteed: bool,
}

/// One leg of a combo as the caller gave it (ibx#470).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ComboLegSpec {
    pub con_id: i64,
    pub ratio: i32,
    /// The leg buys (BUY); a SELL, SSHORT or SSHORTX leg sells.
    pub buy: bool,
    /// The leg's exchange as given.
    pub exchange: String,
}

/// An algo on an order (ibx#263): the Adaptive priority, or the
/// parameters of another algo.
#[derive(Debug, Clone)]
pub enum OrderAlgo {
    Adaptive(AdaptivePriority),
    Params(AlgoParams),
}

/// The short-sale instructions of an order (ibx#417): the API
/// shortSaleSlot, designatedLocation and exemptCode. The default is the
/// API's: no slot, no location, no exempt code (-1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShortSale {
    /// 1 = the broker holds the shares, 2 = delivered from elsewhere,
    /// 0 = not set.
    pub slot: i32,
    /// Where the shares are held, needed with slot 2.
    pub location: String,
    /// Exempt reason code, -1 = none.
    pub exempt_code: i32,
}

impl Default for ShortSale {
    fn default() -> Self {
        Self { slot: 0, location: String::new(), exempt_code: -1 }
    }
}

impl ShortSale {
    /// The exempt code names a reason of the reference's reason table:
    /// -2 and 0 to 9. -1 and any other code are no reason.
    pub fn exempt_reason_given(&self) -> bool {
        matches!(self.exempt_code, -2 | 0..=9)
    }
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

/// The order-type-specific part of an extended order submission: which
/// order type and its price parameters. Used by `OrderRequest::SubmitEx`,
/// which pairs any of these with a TIF and an `OrderAttrs` block, so every
/// order type can carry extended attributes without a per-type `*Ex`
/// variant (ibx#224).
#[derive(Debug, Clone, Copy)]
pub enum OrderKind {
    Market,
    Limit { price: Price },
    Stop { stop_price: Price },
    StopLimit { price: Price, stop_price: Price },
    /// Trailing stop by absolute amount. `trail_stop_price` is the optional
    /// initial stop trigger (tag 6117); 0 = not set.
    TrailingStop { trail_amt: Price, trail_stop_price: Price },
    /// Trailing stop limit; `lmt_offset` is the limit-vs-trail offset (tag 6370).
    /// `trail_stop_price` is the optional initial stop trigger (tag 6117); 0 = not set.
    /// `lmt_price`: the absolute limit price, sent in 44 with no 6370;
    /// else `lmt_offset` in 6370 (ib-agent#194).
    TrailingStopLimit { lmt_offset: Price, lmt_price: Option<Price>, trail_amt: Price, trail_stop_price: Price },
    /// Trailing stop by percentage. Basis points: 100 = 1%.
    /// `trail_stop_price` is the optional initial stop trigger (tag 6117); 0 = not set.
    /// The percent as the API gives it, in the price fixed point (1% =
    /// PRICE_SCALE): the reference writes it with its price formatter, so
    /// 1.239 goes out as 1.239 (ibx#263; basis points kept two decimals).
    TrailPct { trail_percent: Price, trail_stop_price: Price },
    Moc,
    Loc { price: Price },
    Mit { stop_price: Price },
    Lit { price: Price, stop_price: Price },
    Mtl,
    MktPrt,
    StpPrt { stop_price: Price },
    MidPrice { price_cap: Price },
    /// The snap types carry their offset (the API auxPrice) in both price
    /// fields, 0 when unset (ibx#413).
    SnapMkt { offset: Price },
    SnapMid { offset: Price },
    SnapPri { offset: Price },
    /// Pegged to market / to midpoint (ibx#414): `price` is the limit
    /// price, 0 = unset; `offset` the API auxPrice (pegged to midpoint
    /// always sends a zero offset, as the reference).
    PegMkt { price: Price, offset: Price },
    PegMid { price: Price, offset: Price },
    /// Relative: `price` is the price cap (the API lmtPrice), 0 = unset;
    /// `offset` the API auxPrice (ibx#263).
    Rel { price: Price, offset: Price },
    /// Pegged to benchmark (ibx#415): the starting price (0 = unset), the
    /// stock reference price (0 = unset), the reference contract, the
    /// pegged change (sent negative for a decrease) and the reference
    /// change. The reference exchange rides `OrderAttrs::reference_exchange`.
    PegBench {
        starting_price: Price,
        stock_ref_price: Price,
        ref_con_id: i64,
        is_peg_decrease: bool,
        pegged_change_amount: Price,
        ref_change_amount: Price,
    },
    /// Adjustable stop, same fields as `OrderRequest::SubmitAdjustableStop`.
    /// On this path it also carries parent, OCA and tif, so it can be a
    /// bracket child (ibx#240).
    AdjustableStop {
        stop_price: Price,
        trigger_price: Price,
        adjusted_order_type: AdjustedOrderType,
        adjusted_stop_price: Price,
        adjusted_stop_limit_price: Price,
        adjusted_trailing_amount: Price,
        adjustable_trailing_unit: i32,
    },
}

impl OrderKind {
    /// The prices of this kind the reference checks against the price grid
    /// (`trader.common.b9.a(OrderCreator, o)`, ibx#263): the limit price of
    /// every type, and the stop or offset price (the API auxPrice) of the
    /// types that have one (`jibtypes.s.s()`), for a trailing type its
    /// amount, never a percent. Not checked: the trailing stop price, the
    /// TRAIL LIMIT offset, the adjusted prices, the benchmark changes.
    /// Unset prices are 0 and pass.
    pub fn grid_prices(&self) -> [Price; 2] {
        match *self {
            OrderKind::Market | OrderKind::Moc | OrderKind::Mtl | OrderKind::MktPrt
            | OrderKind::TrailPct { .. } => [0, 0],
            OrderKind::Limit { price } | OrderKind::Loc { price } => [price, 0],
            OrderKind::Stop { stop_price }
            | OrderKind::Mit { stop_price }
            | OrderKind::StpPrt { stop_price }
            | OrderKind::AdjustableStop { stop_price, .. } => [0, stop_price],
            OrderKind::StopLimit { price, stop_price }
            | OrderKind::Lit { price, stop_price } => [price, stop_price],
            OrderKind::TrailingStop { trail_amt, .. } => [0, trail_amt],
            OrderKind::TrailingStopLimit { lmt_price, trail_amt, .. } => [lmt_price.unwrap_or(0), trail_amt],
            OrderKind::MidPrice { price_cap } => [price_cap, 0],
            OrderKind::PegMkt { price, offset } | OrderKind::PegMid { price, offset }
            | OrderKind::Rel { price, offset } => [price, offset],
            OrderKind::SnapMkt { offset }
            | OrderKind::SnapMid { offset } | OrderKind::SnapPri { offset } => [0, offset],
            OrderKind::PegBench { starting_price, .. } => [0, starting_price],
        }
    }
}

/// Order request sent via control channel, processed by engine.
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
        /// Optional initial stop trigger (tag 6117); 0 = not set.
        trail_stop_price: Price,
    },
    SubmitTrailingStopLimit {
        order_id: OrderId,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        /// Limit offset from the trail-stop price (wire tag 6370 LimitPriceOffset).
        /// The gateway derives the absolute limit price; do not pass an absolute price here.
        lmt_offset: Price,
        /// Absolute limit price (44, no 6370) instead of the offset (ib-agent#194).
        lmt_price: Option<Price>,
        trail_amt: Price,
        /// Optional initial stop trigger (tag 6117); 0 = not set.
        trail_stop_price: Price,
    },
    /// Trailing stop by percentage. `trail_percent` is the percent in the
    /// price fixed point (1% = PRICE_SCALE, ibx#263);
    /// on the wire the percent rides as a decimal with the unit flag set to
    /// percent (ibx#339).
    SubmitTrailingStopPct {
        order_id: OrderId,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        trail_percent: Price, // 1% = PRICE_SCALE, 2.5% = 2.5 * PRICE_SCALE
        /// Optional initial stop trigger (tag 6117); 0 = not set.
        trail_stop_price: Price,
    },
    SubmitTrailingStopPctEx {
        order_id: OrderId,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        trail_percent: Price,
        tif: u8,
        attrs: OrderAttrs,
        /// Optional initial stop trigger (tag 6117); 0 = not set.
        trail_stop_price: Price,
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
    /// Extended submission for any order type: `kind` selects the order type
    /// and its prices, paired with a TIF and the full `OrderAttrs` block.
    /// This is how non-LMT types carry parent_id/oca_group/outside_rth/tif
    /// (ibx#224).
    SubmitEx {
        order_id: OrderId,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        kind: OrderKind,
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
        /// Time-in-force byte and extended attributes, like every other
        /// order type: a parented or GTC algo order kept neither (ibx#318).
        tif: u8,
        attrs: OrderAttrs,
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
        offset: Price, // the API auxPrice, 0 = unset
    },
    /// Snap to Midpoint: snaps to midpoint. OrdType SMID.
    SubmitSnapMid {
        order_id: OrderId,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        offset: Price, // the API auxPrice, 0 = unset
    },
    /// Snap to Primary: snaps to primary (NBBO). OrdType SREL.
    SubmitSnapPri {
        order_id: OrderId,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        offset: Price, // the API auxPrice, 0 = unset
    },
    /// Pegged to Market: pegs to market with optional offset and limit price.
    SubmitPegMkt {
        order_id: OrderId,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price: Price,  // limit price, 0 = unset
        offset: Price, // peg offset, 0 = no offset
    },
    /// Pegged to Midpoint: pegs to midpoint with optional limit price. The
    /// offset is kept but goes out as zero, as the reference (ibx#414).
    SubmitPegMid {
        order_id: OrderId,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price: Price,  // limit price, 0 = unset
        offset: Price,
    },
    /// Algorithmic order: limit order with IB algo strategy overlay (VWAP, TWAP, etc.).
    SubmitAlgo {
        order_id: OrderId,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        price: Price,
        algo: AlgoParams,
        /// Time-in-force byte and extended attributes, like every other
        /// order type: a parented or GTC algo order kept neither (ibx#318).
        tif: u8,
        attrs: OrderAttrs,
    },
    /// Pegged to Benchmark: pegs to a benchmark instrument's price, written
    /// as the reference writes it (ibx#415).
    SubmitPegBench {
        order_id: OrderId,
        instrument: InstrumentId,
        side: Side,
        qty: u32,
        /// The starting price, 0 = unset. There is no limit price.
        price: Price,
        ref_con_id: i64,
        is_peg_decrease: bool,
        pegged_change_amount: Price,
        ref_change_amount: Price,
        /// The stock reference price, 0 = unset.
        stock_ref_price: Price,
        /// The reference contract's exchange, empty = not sent.
        ref_exchange: String,
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
    /// What-If preview of a new order (ibx#462): `request` is the order as
    /// it would be placed, written by its own encoder with the preview flag,
    /// under a ClOrdID of its own. It is NOT placed and never modifies a
    /// working order; the answer is an `Event::WhatIf`.
    SubmitWhatIf {
        request: Box<OrderRequest>,
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
        /// Trailing amount for a Trail/TrailLimit conversion (tag 6260). When the
        /// unit is amount it is a price offset (scaled); when percent it is the
        /// percent value scaled (1.00% = PRICE_SCALE). 0 = not set.
        adjusted_trailing_amount: Price,
        /// Unit of `adjusted_trailing_amount` on the wire (tag 6269): 0 = amount,
        /// 100 = percent. Other values are rejected by the gateway.
        adjustable_trailing_unit: i32,
    },
    Cancel {
        order_id: OrderId,
    },
    CancelAll {
        instrument: InstrumentId,
    },
    /// The API global cancel: every order of the book, whatever its
    /// client or session, as the reference cancels them.
    GlobalCancel,
    /// Replace a working order. Carries the full wanted state, like a new
    /// order does: the replace restates the order type, prices, time-in-force
    /// and the attributes the reference restates (ibx#247 ibx#324 ibx#334
    /// ibx#349, reference capture ib-agent#192 group A).
    Modify {
        new_order_id: OrderId,
        order_id: OrderId,
        qty: u32,
        kind: OrderKind,
        tif: u8,
        attrs: OrderAttrs,
    },
}

impl OrderRequest {
    /// Extract the order_id from any variant. Returns 0 for CancelAll and
    /// GlobalCancel (no order_id).
    pub fn order_id(&self) -> OrderId {
        match self {
            Self::Cancel { order_id } => *order_id,
            Self::CancelAll { .. } | Self::GlobalCancel => 0,
            Self::Modify { order_id, .. } => *order_id,
            Self::SubmitLimit { order_id, .. }
            | Self::SubmitMarket { order_id, .. }
            | Self::SubmitStop { order_id, .. }
            | Self::SubmitStopLimit { order_id, .. }
            | Self::SubmitLimitGtc { order_id, .. }
            | Self::SubmitStopGtc { order_id, .. }
            | Self::SubmitStopLimitGtc { order_id, .. }
            | Self::SubmitLimitIoc { order_id, .. }
            | Self::SubmitLimitFok { order_id, .. }
            | Self::SubmitTrailingStop { order_id, .. }
            | Self::SubmitTrailingStopLimit { order_id, .. }
            | Self::SubmitTrailingStopPct { order_id, .. }
            | Self::SubmitTrailingStopPctEx { order_id, .. }
            | Self::SubmitMoc { order_id, .. }
            | Self::SubmitLoc { order_id, .. }
            | Self::SubmitMit { order_id, .. }
            | Self::SubmitLit { order_id, .. }
            | Self::SubmitLimitEx { order_id, .. }
            | Self::SubmitRel { order_id, .. }
            | Self::SubmitLimitOpg { order_id, .. }
            | Self::SubmitAdaptive { order_id, .. }
            | Self::SubmitMtl { order_id, .. }
            | Self::SubmitMktPrt { order_id, .. }
            | Self::SubmitStpPrt { order_id, .. }
            | Self::SubmitMidPrice { order_id, .. }
            | Self::SubmitSnapMkt { order_id, .. }
            | Self::SubmitSnapMid { order_id, .. }
            | Self::SubmitSnapPri { order_id, .. }
            | Self::SubmitPegMkt { order_id, .. }
            | Self::SubmitPegMid { order_id, .. }
            | Self::SubmitAlgo { order_id, .. }
            | Self::SubmitPegBench { order_id, .. }
            | Self::SubmitLimitAuc { order_id, .. }
            | Self::SubmitMtlAuc { order_id, .. }
            | Self::SubmitLimitFractional { order_id, .. }
            | Self::SubmitAdjustableStop { order_id, .. }
            | Self::SubmitEx { order_id, .. } => *order_id,
            Self::SubmitBracket { parent_id, .. } => *parent_id,
            Self::SubmitWhatIf { request } => request.order_id(),
        }
    }

    /// The order ids of the new orders the request makes: none for a
    /// cancel or a replace, the three orders of a bracket.
    pub fn new_order_ids(&self) -> Vec<OrderId> {
        match self {
            Self::SubmitBracket { parent_id, tp_id, sl_id, .. } => vec![*parent_id, *tp_id, *sl_id],
            Self::SubmitWhatIf { request } => request.new_order_ids(),
            _ if self.new_order_qty().is_some() => vec![self.order_id()],
            _ => Vec::new(),
        }
    }

    /// The quantity of a new order, fixed-point (QTY_SCALE); None for a
    /// cancel or a replace. A bracket gives its legs' quantity.
    pub fn new_order_qty(&self) -> Option<Qty> {
        match self {
            Self::Cancel { .. } | Self::CancelAll { .. } | Self::GlobalCancel | Self::Modify { .. } => None,
            Self::SubmitWhatIf { request } => request.new_order_qty(),
            Self::SubmitLimitFractional { qty, .. } => Some(*qty),
            Self::SubmitLimit { qty, .. }
            | Self::SubmitMarket { qty, .. }
            | Self::SubmitStop { qty, .. }
            | Self::SubmitStopLimit { qty, .. }
            | Self::SubmitLimitGtc { qty, .. }
            | Self::SubmitStopGtc { qty, .. }
            | Self::SubmitStopLimitGtc { qty, .. }
            | Self::SubmitLimitIoc { qty, .. }
            | Self::SubmitLimitFok { qty, .. }
            | Self::SubmitTrailingStop { qty, .. }
            | Self::SubmitTrailingStopLimit { qty, .. }
            | Self::SubmitTrailingStopPct { qty, .. }
            | Self::SubmitTrailingStopPctEx { qty, .. }
            | Self::SubmitMoc { qty, .. }
            | Self::SubmitLoc { qty, .. }
            | Self::SubmitMit { qty, .. }
            | Self::SubmitLit { qty, .. }
            | Self::SubmitLimitEx { qty, .. }
            | Self::SubmitRel { qty, .. }
            | Self::SubmitLimitOpg { qty, .. }
            | Self::SubmitAdaptive { qty, .. }
            | Self::SubmitMtl { qty, .. }
            | Self::SubmitMktPrt { qty, .. }
            | Self::SubmitStpPrt { qty, .. }
            | Self::SubmitMidPrice { qty, .. }
            | Self::SubmitSnapMkt { qty, .. }
            | Self::SubmitSnapMid { qty, .. }
            | Self::SubmitSnapPri { qty, .. }
            | Self::SubmitPegMkt { qty, .. }
            | Self::SubmitPegMid { qty, .. }
            | Self::SubmitAlgo { qty, .. }
            | Self::SubmitPegBench { qty, .. }
            | Self::SubmitLimitAuc { qty, .. }
            | Self::SubmitMtlAuc { qty, .. }
            | Self::SubmitAdjustableStop { qty, .. }
            | Self::SubmitEx { qty, .. }
            | Self::SubmitBracket { qty, .. } => Some(*qty as Qty * QTY_SCALE),
        }
    }

    /// Extract the instrument from any submit variant. None for
    /// Cancel/Modify, which carry no instrument (the engine resolves it from
    /// the tracked order).
    pub fn instrument(&self) -> Option<InstrumentId> {
        match self {
            Self::Cancel { .. } | Self::Modify { .. } | Self::GlobalCancel => None,
            Self::CancelAll { instrument }
            | Self::SubmitLimit { instrument, .. }
            | Self::SubmitMarket { instrument, .. }
            | Self::SubmitStop { instrument, .. }
            | Self::SubmitStopLimit { instrument, .. }
            | Self::SubmitLimitGtc { instrument, .. }
            | Self::SubmitStopGtc { instrument, .. }
            | Self::SubmitStopLimitGtc { instrument, .. }
            | Self::SubmitLimitIoc { instrument, .. }
            | Self::SubmitLimitFok { instrument, .. }
            | Self::SubmitTrailingStop { instrument, .. }
            | Self::SubmitTrailingStopLimit { instrument, .. }
            | Self::SubmitTrailingStopPct { instrument, .. }
            | Self::SubmitTrailingStopPctEx { instrument, .. }
            | Self::SubmitMoc { instrument, .. }
            | Self::SubmitLoc { instrument, .. }
            | Self::SubmitMit { instrument, .. }
            | Self::SubmitLit { instrument, .. }
            | Self::SubmitLimitEx { instrument, .. }
            | Self::SubmitRel { instrument, .. }
            | Self::SubmitLimitOpg { instrument, .. }
            | Self::SubmitAdaptive { instrument, .. }
            | Self::SubmitMtl { instrument, .. }
            | Self::SubmitMktPrt { instrument, .. }
            | Self::SubmitStpPrt { instrument, .. }
            | Self::SubmitMidPrice { instrument, .. }
            | Self::SubmitSnapMkt { instrument, .. }
            | Self::SubmitSnapMid { instrument, .. }
            | Self::SubmitSnapPri { instrument, .. }
            | Self::SubmitPegMkt { instrument, .. }
            | Self::SubmitPegMid { instrument, .. }
            | Self::SubmitAlgo { instrument, .. }
            | Self::SubmitPegBench { instrument, .. }
            | Self::SubmitLimitAuc { instrument, .. }
            | Self::SubmitMtlAuc { instrument, .. }
            | Self::SubmitLimitFractional { instrument, .. }
            | Self::SubmitAdjustableStop { instrument, .. }
            | Self::SubmitEx { instrument, .. }
            | Self::SubmitBracket { instrument, .. } => Some(*instrument),
            Self::SubmitWhatIf { request } => request.instrument(),
        }
    }

    /// The instrument of a new order, to change: the slot of its contract
    /// once the contract's lookup found it (ibx#486). None for a cancel, a
    /// modify, a cancel-all and a global cancel.
    pub fn new_order_instrument_mut(&mut self) -> Option<&mut InstrumentId> {
        match self {
            Self::Cancel { .. } | Self::Modify { .. } | Self::CancelAll { .. } | Self::GlobalCancel => None,
            Self::SubmitLimit { instrument, .. }
            | Self::SubmitMarket { instrument, .. }
            | Self::SubmitStop { instrument, .. }
            | Self::SubmitStopLimit { instrument, .. }
            | Self::SubmitLimitGtc { instrument, .. }
            | Self::SubmitStopGtc { instrument, .. }
            | Self::SubmitStopLimitGtc { instrument, .. }
            | Self::SubmitLimitIoc { instrument, .. }
            | Self::SubmitLimitFok { instrument, .. }
            | Self::SubmitTrailingStop { instrument, .. }
            | Self::SubmitTrailingStopLimit { instrument, .. }
            | Self::SubmitTrailingStopPct { instrument, .. }
            | Self::SubmitTrailingStopPctEx { instrument, .. }
            | Self::SubmitMoc { instrument, .. }
            | Self::SubmitLoc { instrument, .. }
            | Self::SubmitMit { instrument, .. }
            | Self::SubmitLit { instrument, .. }
            | Self::SubmitLimitEx { instrument, .. }
            | Self::SubmitRel { instrument, .. }
            | Self::SubmitLimitOpg { instrument, .. }
            | Self::SubmitAdaptive { instrument, .. }
            | Self::SubmitMtl { instrument, .. }
            | Self::SubmitMktPrt { instrument, .. }
            | Self::SubmitStpPrt { instrument, .. }
            | Self::SubmitMidPrice { instrument, .. }
            | Self::SubmitSnapMkt { instrument, .. }
            | Self::SubmitSnapMid { instrument, .. }
            | Self::SubmitSnapPri { instrument, .. }
            | Self::SubmitPegMkt { instrument, .. }
            | Self::SubmitPegMid { instrument, .. }
            | Self::SubmitAlgo { instrument, .. }
            | Self::SubmitPegBench { instrument, .. }
            | Self::SubmitLimitAuc { instrument, .. }
            | Self::SubmitMtlAuc { instrument, .. }
            | Self::SubmitLimitFractional { instrument, .. }
            | Self::SubmitAdjustableStop { instrument, .. }
            | Self::SubmitEx { instrument, .. }
            | Self::SubmitBracket { instrument, .. } => Some(instrument),
            Self::SubmitWhatIf { request } => request.new_order_instrument_mut(),
        }
    }

    /// The combo of a new combo (BAG) order (ibx#470).
    pub fn combo(&self) -> Option<&ComboSpec> {
        self.new_order_side()?.1?.combo.as_deref()
    }

    /// The instrument of a new order sent through the extended encoder,
    /// the one every combo order takes (ibx#470).
    pub fn ex_instrument_mut(&mut self) -> Option<&mut InstrumentId> {
        match self {
            Self::SubmitWhatIf { request } => request.ex_instrument_mut(),
            Self::SubmitTrailingStopPctEx { instrument, .. }
            | Self::SubmitLimitEx { instrument, .. }
            | Self::SubmitEx { instrument, .. }
            | Self::SubmitAdaptive { instrument, .. }
            | Self::SubmitAlgo { instrument, .. } => Some(instrument),
            _ => None,
        }
    }

    /// The side of a new order and its attributes when it carries them.
    /// None for a cancel or a replace. A bracket gives its parent's side.
    pub fn new_order_side(&self) -> Option<(Side, Option<&OrderAttrs>)> {
        match self {
            Self::Cancel { .. } | Self::CancelAll { .. } | Self::GlobalCancel | Self::Modify { .. } => None,
            Self::SubmitWhatIf { request } => request.new_order_side(),
            Self::SubmitTrailingStopPctEx { side, attrs, .. }
            | Self::SubmitLimitEx { side, attrs, .. }
            | Self::SubmitEx { side, attrs, .. }
            | Self::SubmitAdaptive { side, attrs, .. }
            | Self::SubmitAlgo { side, attrs, .. } => Some((*side, Some(attrs))),
            Self::SubmitLimit { side, .. }
            | Self::SubmitMarket { side, .. }
            | Self::SubmitStop { side, .. }
            | Self::SubmitStopLimit { side, .. }
            | Self::SubmitLimitGtc { side, .. }
            | Self::SubmitStopGtc { side, .. }
            | Self::SubmitStopLimitGtc { side, .. }
            | Self::SubmitLimitIoc { side, .. }
            | Self::SubmitLimitFok { side, .. }
            | Self::SubmitTrailingStop { side, .. }
            | Self::SubmitTrailingStopLimit { side, .. }
            | Self::SubmitTrailingStopPct { side, .. }
            | Self::SubmitMoc { side, .. }
            | Self::SubmitLoc { side, .. }
            | Self::SubmitMit { side, .. }
            | Self::SubmitLit { side, .. }
            | Self::SubmitBracket { side, .. }
            | Self::SubmitRel { side, .. }
            | Self::SubmitLimitOpg { side, .. }
            | Self::SubmitMtl { side, .. }
            | Self::SubmitMktPrt { side, .. }
            | Self::SubmitStpPrt { side, .. }
            | Self::SubmitMidPrice { side, .. }
            | Self::SubmitSnapMkt { side, .. }
            | Self::SubmitSnapMid { side, .. }
            | Self::SubmitSnapPri { side, .. }
            | Self::SubmitPegMkt { side, .. }
            | Self::SubmitPegMid { side, .. }
            | Self::SubmitPegBench { side, .. }
            | Self::SubmitLimitAuc { side, .. }
            | Self::SubmitMtlAuc { side, .. }
            | Self::SubmitLimitFractional { side, .. }
            | Self::SubmitAdjustableStop { side, .. } => Some((*side, None)),
        }
    }

    /// The order of this request with a price off the contract's price
    /// grid (`off_grid`, `signed` for a rule that allows prices at or
    /// below 0), as the reference checks it (ibx#263): None when
    /// every checked price is on the grid. A bracket answers for the first
    /// leg found.
    pub fn off_grid_order(&self, tick: i64, signed: bool) -> Option<OrderId> {
        let off = |prices: &[Price]| prices.iter().any(|&p| off_grid(p, tick, signed));
        let checked: (OrderId, [Price; 2]) = match self {
            Self::Cancel { .. } | Self::CancelAll { .. } | Self::GlobalCancel
            | Self::SubmitMarket { .. } | Self::SubmitMoc { .. }
            | Self::SubmitMtl { .. } | Self::SubmitMktPrt { .. }
            | Self::SubmitMtlAuc { .. }
            | Self::SubmitTrailingStopPct { .. } | Self::SubmitTrailingStopPctEx { .. } => return None,
            Self::SubmitWhatIf { request } => return request.off_grid_order(tick, signed),
            Self::Modify { order_id, kind, .. } | Self::SubmitEx { order_id, kind, .. } => (*order_id, kind.grid_prices()),
            Self::SubmitLimit { order_id, price, .. }
            | Self::SubmitLimitGtc { order_id, price, .. }
            | Self::SubmitLimitIoc { order_id, price, .. }
            | Self::SubmitLimitFok { order_id, price, .. }
            | Self::SubmitLimitEx { order_id, price, .. }
            | Self::SubmitLimitOpg { order_id, price, .. }
            | Self::SubmitLimitAuc { order_id, price, .. }
            | Self::SubmitLimitFractional { order_id, price, .. }
            | Self::SubmitAdaptive { order_id, price, .. }
            | Self::SubmitAlgo { order_id, price, .. }
            | Self::SubmitLoc { order_id, price, .. }
            | Self::SubmitMidPrice { order_id, price_cap: price, .. } => (*order_id, [*price, 0]),
            Self::SubmitStop { order_id, stop_price, .. }
            | Self::SubmitStopGtc { order_id, stop_price, .. }
            | Self::SubmitMit { order_id, stop_price, .. }
            | Self::SubmitStpPrt { order_id, stop_price, .. }
            | Self::SubmitAdjustableStop { order_id, stop_price, .. } => (*order_id, [0, *stop_price]),
            Self::SubmitStopLimit { order_id, price, stop_price, .. }
            | Self::SubmitStopLimitGtc { order_id, price, stop_price, .. }
            | Self::SubmitLit { order_id, price, stop_price, .. } => (*order_id, [*price, *stop_price]),
            Self::SubmitTrailingStop { order_id, trail_amt, .. } => (*order_id, [0, *trail_amt]),
            Self::SubmitTrailingStopLimit { order_id, lmt_price, trail_amt, .. } => (*order_id, [lmt_price.unwrap_or(0), *trail_amt]),
            Self::SubmitPegMkt { order_id, price, offset, .. }
            | Self::SubmitPegMid { order_id, price, offset, .. } => (*order_id, [*price, *offset]),
            Self::SubmitRel { order_id, offset, .. }
            | Self::SubmitSnapMkt { order_id, offset, .. }
            | Self::SubmitSnapMid { order_id, offset, .. }
            | Self::SubmitSnapPri { order_id, offset, .. } => (*order_id, [0, *offset]),
            Self::SubmitPegBench { order_id, price, .. } => (*order_id, [0, *price]),
            Self::SubmitBracket { parent_id, tp_id, sl_id, entry_price, take_profit, stop_loss, .. } => {
                return [(*parent_id, [*entry_price, 0]), (*tp_id, [*take_profit, 0]), (*sl_id, [0, *stop_loss])]
                    .into_iter().find(|(_, prices)| off(prices)).map(|(id, _)| id);
            }
        };
        off(&checked.1).then_some(checked.0)
    }
}

/// Pre-allocated buffer for pending order requests. Allocates on the hot
/// path only for a burst past its capacity (more than 64 orders in one
/// pass, or held while the auth link is down): the reference has no limit.
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
        self.buf.push(req);
    }

    pub fn drain(&mut self) -> std::vec::Drain<'_, OrderRequest> {
        self.buf.drain(..)
    }

    /// Put requests back ahead of the queued ones, in their order.
    pub fn prepend(&mut self, reqs: Vec<OrderRequest>) {
        self.buf.splice(0..0, reqs);
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

/// The client's market data modes, as the reference sets them from
/// reqMarketDataType (ibx#447): 1 turns all off; 2 turns frozen on and
/// leaves the delayed modes; 3 turns delayed on and delayed-frozen off,
/// leaving frozen; 4 turns delayed and delayed-frozen on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MarketDataModes {
    pub frozen: bool,
    pub delayed: bool,
    pub delayed_frozen: bool,
}

impl MarketDataModes {
    /// Apply a reqMarketDataType value; false (nothing changed) outside
    /// 1..=4, which the reference refuses.
    pub fn apply(&mut self, market_data_type: i32) -> bool {
        match market_data_type {
            1 => *self = Self::default(),
            2 => self.frozen = true,
            3 => { self.delayed = true; self.delayed_frozen = false; }
            4 => { self.delayed = true; self.delayed_frozen = true; }
            _ => return false,
        }
        true
    }

    /// The per-entry mode code of a subscription, from the reference's
    /// table: 0 real time, 1 delayed, 2 frozen, 3 delayed frozen.
    pub fn entry_mode(frozen: bool, delayed: bool) -> i32 {
        match (frozen, delayed) {
            (false, false) => 0,
            (false, true) => 1,
            (true, false) => 2,
            (true, true) => 3,
        }
    }
}

/// Tick-by-tick data type for subscription requests: the four types of
/// the API, each asked under its own name (ibx#455).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TbtType {
    /// Last trade ticks ("Last").
    Last,
    /// All trade ticks ("AllLast").
    AllLast,
    /// Bid/ask quote ticks ("BidAsk").
    BidAsk,
    /// Midpoint ticks ("MidPoint").
    MidPoint,
}

impl TbtType {
    /// The type of an API tick type string: exactly one of the four names,
    /// case sensitive, as the reference checks it; None otherwise.
    pub fn from_api(tick_type: &str) -> Option<Self> {
        match tick_type {
            "Last" => Some(TbtType::Last),
            "AllLast" => Some(TbtType::AllLast),
            "BidAsk" => Some(TbtType::BidAsk),
            "MidPoint" => Some(TbtType::MidPoint),
            _ => None,
        }
    }

    /// The API name, also the name the request is sent with.
    pub fn as_str(self) -> &'static str {
        match self {
            TbtType::Last => "Last",
            TbtType::AllLast => "AllLast",
            TbtType::BidAsk => "BidAsk",
            TbtType::MidPoint => "MidPoint",
        }
    }

    /// The tickType a client reports: 1 Last, 2 AllLast, 3 BidAsk, 4
    /// MidPoint.
    pub fn api_tick_type(self) -> i32 {
        match self {
            TbtType::Last => 1,
            TbtType::AllLast => 2,
            TbtType::BidAsk => 3,
            TbtType::MidPoint => 4,
        }
    }
}

/// A single tick-by-tick trade (Last or AllLast) from 35=E, for one
/// request of the stream (ibx#455).
#[derive(Debug, Clone)]
pub struct TbtTrade {
    pub instrument: InstrumentId,
    pub req_id: ReqId,
    /// Last or AllLast: the tickType the request reports.
    pub tbt_type: TbtType,
    pub price: Price,
    pub size: i64,
    pub timestamp: u64,
    pub exchange: String,
    pub conditions: String,
    /// Attribute bits 0 and 1 of the entry (ibx#404).
    pub past_limit: bool,
    pub unreported: bool,
}

/// A single tick-by-tick bid/ask quote from 35=E, for one request of the
/// stream (ibx#455).
#[derive(Debug, Clone, Copy)]
pub struct TbtQuote {
    pub instrument: InstrumentId,
    pub req_id: ReqId,
    pub bid: Price,
    pub ask: Price,
    pub bid_size: i64,
    pub ask_size: i64,
    pub timestamp: u64,
    /// Attribute bits 0 and 1 of the entry (ibx#404).
    pub bid_past_low: bool,
    pub ask_past_high: bool,
}

/// A single tick-by-tick midpoint from 35=E, for one request of the
/// stream (ibx#404).
#[derive(Debug, Clone, Copy)]
pub struct TbtMidPoint {
    pub instrument: InstrumentId,
    pub req_id: ReqId,
    pub mid_point: Price,
    pub timestamp: u64,
}

/// An IB news bulletin from auth server news bulletin message.
#[derive(Debug, Clone)]
pub struct NewsBulletin {
    /// Message id given by the server (ibx#461).
    pub msg_id: i32,
    /// 1=Regular, 2=Exchange available, 3=Exchange unavailable, 4=HTML,
    /// 5=Popup text, 6=Popup HTML (ibx#461).
    pub msg_type: i32,
    pub message: String,
    pub exchange: String,
}

/// A market depth (L2 order book) update.
#[derive(Debug, Clone)]
pub struct DepthUpdate {
    pub req_id: ReqId,
    /// Book position (0-based).
    pub position: i32,
    /// Market maker ID (L2 only).
    pub market_maker: String,
    /// 0 = insert, 1 = update, 2 = delete.
    pub operation: i32,
    /// 0 = ask, 1 = bid.
    pub side: i32,
    pub price: f64,
    pub size: f64,
    pub is_smart_depth: bool,
    /// Sent as updateMktDepthL2 (SmartDepth, or a book with market
    /// makers), else as updateMktDepth (#451).
    pub l2: bool,
}

/// Exchange metadata for market depth availability.
#[derive(Debug, Clone)]
pub struct DepthMktDataDescription {
    pub exchange: String,
    pub sec_type: String,
    pub listing_exch: String,
    pub service_data_type: String,
    pub agg_group: i32,
}

/// The reference's security type ids (`SecType` values), by API
/// security type (ibx#449, ibx#441).
const SEC_TYPE_IDS: [(&str, u8); 23] = [
    ("STK", 1), ("CFD", 2), ("OPT", 3), ("FOP", 4), ("WAR", 5), ("FUT", 6), ("FWD", 7),
    ("BAG", 8), ("CASH", 10), ("IND", 11), ("BOND", 12), ("BILL", 13), ("FIXED", 14),
    ("FUND", 15), ("SLB", 16), ("NEWS", 17), ("CMDTY", 18), ("BSK", 19), ("IOPT", 20),
    ("ICU", 21), ("ICS", 22), ("PHYSS", 23), ("CRYPTO", 24),
];

/// The reference's security type id of an API security type.
pub fn sec_type_id(sec_type: &str) -> Option<u8> {
    SEC_TYPE_IDS.iter().find(|(name, _)| *name == sec_type).map(|(_, id)| *id)
}

/// The API security type of a security type id.
pub fn sec_type_by_id(id: u8) -> Option<&'static str> {
    SEC_TYPE_IDS.iter().find(|(_, i)| *i == id).map(|(name, _)| *name)
}

/// A component exchange in a SMART routing map.
#[derive(Debug, Clone, PartialEq)]
pub struct SmartComponent {
    pub bit_number: i32,
    pub exchange: String,
    pub exchange_letter: String,
}

/// A news data provider.
#[derive(Debug, Clone)]
pub struct NewsProvider {
    pub code: String,
    pub name: String,
}

/// A soft dollar tier (commission sharing arrangement).
#[derive(Debug, Clone)]
pub struct SoftDollarTier {
    pub name: String,
    pub val: String,
    pub display_name: String,
}

/// A family code linking related accounts.
#[derive(Debug, Clone)]
pub struct FamilyCode {
    pub account_id: String,
    pub family_code_str: String,
}

/// A news headline of a contract's news tick (ibx#458), as the reference
/// gives it to tickNews.
#[derive(Debug, Clone, PartialEq)]
pub struct TickNews {
    pub instrument: InstrumentId,
    pub provider_code: String,
    pub article_id: String,
    /// The headline without its leading `{...}` part.
    pub headline: String,
    /// Time of the headline, epoch milliseconds.
    pub timestamp: i64,
    /// The text between the first `{` and the next `}` of the raw headline.
    pub extra_data: String,
}

/// A historical tick (midpoint), as the official `HistoricalTick` (ibx#432):
/// time in Unix seconds, and a size (0 for a midpoint).
#[derive(Debug, Clone, PartialEq)]
pub struct HistoricalTickMidpoint {
    pub time: i64,
    pub price: f64,
    pub size: f64,
}

/// A historical tick (last trade), as the official `HistoricalTickLast`
/// (ibx#432): time in Unix seconds, the past limit and unreported flags.
#[derive(Debug, Clone, PartialEq)]
pub struct HistoricalTickLast {
    pub time: i64,
    pub tick_attrib_last: crate::api::types::TickAttribLast,
    pub price: f64,
    pub size: f64,
    pub exchange: String,
    pub special_conditions: String,
}

/// A historical tick (bid/ask), as the official `HistoricalTickBidAsk`
/// (ibx#432): time in Unix seconds, the bid past low and ask past high
/// flags.
#[derive(Debug, Clone, PartialEq)]
pub struct HistoricalTickBidAsk {
    pub time: i64,
    pub tick_attrib_bid_ask: crate::api::types::TickAttribBidAsk,
    pub price_bid: f64,
    pub price_ask: f64,
    pub size_bid: f64,
    pub size_ask: f64,
}

/// Historical tick data (one of three types based on whatToShow).
#[derive(Debug, Clone, PartialEq)]
pub enum HistoricalTickData {
    Midpoint(Vec<HistoricalTickMidpoint>),
    Last(Vec<HistoricalTickLast>),
    BidAsk(Vec<HistoricalTickBidAsk>),
}

/// A real-time 5-second bar.
#[derive(Debug, Clone, Copy)]
pub struct RealTimeBar {
    pub timestamp: u32,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
    pub wap: f64,
    pub count: i32,
}

/// A single trading session from a historical schedule response.
#[derive(Debug, Clone)]
pub struct ScheduleSession {
    pub ref_date: String,
    pub open_time: String,
    pub close_time: String,
}

/// Parsed historical schedule response from historical data connection.
#[derive(Debug, Clone)]
pub struct HistoricalScheduleResponse {
    pub query_id: String,
    pub timezone: String,
    pub start_date_time: String,
    pub end_date_time: String,
    pub sessions: Vec<ScheduleSession>,
}

/// A completed order record for req_completed_orders.
#[derive(Debug, Clone)]
pub struct CompletedOrder {
    pub order_id: OrderId,
    pub instrument: InstrumentId,
    pub status: OrderStatus,
    /// Fixed-point (QTY_SCALE).
    pub filled_qty_fixed: Qty,
    pub timestamp_ns: u64,
}

/// Optional request-side filters for a by-symbol contract-details lookup.
/// Empty/zero fields are omitted from the request (ib-agent#171, ibx#229).
#[derive(Debug, Clone, Default)]
pub struct SecDefFilters {
    pub primary_exchange: String,
    pub local_symbol: String,
    pub last_trade_date_or_contract_month: String,
    pub strike: f64,
    pub right: String,
    pub multiplier: String,
    pub trading_class: String,
    /// Identifier lookup (e.g. ISIN): raw identifier and its type. When set, the
    /// lookup rides the identifier instead of the symbol (ib-agent#174).
    pub sec_id: String,
    pub sec_id_type: String,
    /// Expired contracts are included (ibx#229).
    pub include_expired: bool,
    /// Bond issuer id (ibx#438): when set, the lookup is for the issuer's
    /// bonds.
    pub issuer_id: String,
}

impl SecDefFilters {
    /// The strike of a lookup, empty when unset: 0 (the Rust API's unset
    /// strike) or the maximum double (the official API's).
    pub fn strike_text(&self) -> String {
        if self.strike > 0.0 && self.strike != f64::MAX { format!("{}", self.strike) } else { String::new() }
    }
}

/// A contract as the API gave it, for a lookup by symbol (ibx#427).
#[derive(Debug, Clone, Default)]
pub struct ContractLookup {
    pub symbol: String,
    pub sec_type: String,
    pub exchange: String,
    pub currency: String,
    pub filters: SecDefFilters,
}

/// Commands sent from the control plane to the hot loop via SPSC channel.
#[derive(Debug, Clone)]
pub enum ControlCommand {
    /// A historical-data request for a contract with no conId (ibx#427):
    /// the contract is looked up first. With exactly one contract found,
    /// `request` is sent with its conId; otherwise the request gets error
    /// 200 and no query is sent.
    ResolveContract {
        req_id: ReqId,
        lookup: ContractLookup,
        request: Box<ControlCommand>,
    },
    /// Subscribe to market data for a contract.
    /// `exchange` and `sec_type` determine farm routing (empty = UsFarm default).
    /// `mode_9887` is the per-request market-data mode sent on each entry
    /// of the bid/ask and last pair: 0 = REALTIME (none sent), 1 = DELAYED,
    /// 2 = FROZEN, 3 = DELAYED_FROZEN (`MarketDataModes::entry_mode`).
    /// `snapshot` asks the pair once, as a snapshot, instead of a stream
    /// (ibx#446).
    Subscribe {
        con_id: i64, symbol: String, exchange: String, sec_type: String,
        last_trade_date: String, strike: f64, right: String, multiplier: String,
        mode_9887: i32, snapshot: bool,
        reply_tx: Option<crossbeam_channel::Sender<Result<InstrumentId, String>>>,
    },
    /// Subscribe to market data for a contract given without a conId
    /// (ibx#278): the engine looks the contract up first, as the
    /// reference does, then subscribes with the conId found. The reply
    /// gives the instrument at once; a lookup that finds no single contract
    /// ends the subscription with error 200.
    SubscribeBySymbol {
        symbol: String, sec_type: String, exchange: String, currency: String,
        filters: SecDefFilters,
        mode_9887: i32, snapshot: bool,
        reply_tx: Option<crossbeam_channel::Sender<Result<InstrumentId, String>>>,
    },
    /// Unsubscribe from market data for an instrument.
    Unsubscribe { instrument: InstrumentId },
    /// The client's reqMarketDataType (1..4), applied to the engine's
    /// `MarketDataModes`: with delayed on, a subscription the server
    /// rejects switches to delayed data (ibx#447).
    SetMarketDataType { market_data_type: i32 },
    /// Subscribe to tick-by-tick data via historical data connection.
    /// `number_of_ticks` above 0 asks for that many past ticks first,
    /// given to `req_id` as historical ticks; `ignore_size` sends the size
    /// filter. A request for a stream that exists (same contract, type and
    /// size filter) joins it, with no new query (ibx#455).
    SubscribeTbt {
        req_id: ReqId,
        con_id: i64, symbol: String, exchange: String, sec_type: String,
        tbt_type: TbtType, number_of_ticks: i32, ignore_size: bool,
        reply_tx: Option<crossbeam_channel::Sender<Result<InstrumentId, String>>>,
    },
    /// End the tick-by-tick request `req_id`: its stream is cancelled once
    /// no request is left on it.
    UnsubscribeTbt { req_id: ReqId },
    /// The news tick of a market data request (generic tick 292, ibx#458),
    /// given after its `Subscribe` / `SubscribeBySymbol`: the news entry
    /// goes to the farm of the contract's route with the request's top of
    /// book. `providers` is the provider key (codes sorted, comma
    /// separated); a `refusal` (the text of error 10094) ends the request
    /// once its contract is known, before anything is sent. The news entry
    /// is cancelled with the request (`Unsubscribe`).
    SubscribeNews {
        instrument: InstrumentId, con_id: i64, exchange: String, sec_type: String,
        providers: String, refusal: Option<String>,
    },
    /// A request with the news tick left an instrument other requests
    /// still use (ibx#444): its share of the news entry with this provider
    /// key goes; the entry is cancelled when no request uses it.
    UnsubscribeNews { instrument: InstrumentId, providers: String },
    /// The generic ticks of a market data request (ibx#450), request codes,
    /// given after its `Subscribe` / `SubscribeBySymbol`: each one the
    /// contract does not have yet is an entry on the farm of the contract's
    /// route, at once or once its top of book is acknowledged
    /// (`control::generic_values`). The entries are cancelled with the
    /// request (`Unsubscribe`).
    SubscribeGeneric { instrument: InstrumentId, con_id: i64, exchange: String, sec_type: String, codes: Vec<i32> },
    /// A request with generic ticks left an instrument other requests still
    /// use (ibx#450): the entries no request needs any more are cancelled.
    UnsubscribeGeneric { instrument: InstrumentId, codes: Vec<i32> },
    /// Subscribe to whole-account P&L via CCP (6040=142).
    SubscribePnl { req_id: i64, account: String },
    /// Cancel P&L subscription.
    CancelPnl { req_id: i64 },
    /// The currency of a contract, for the orders on its instrument
    /// (tag 15, ibx#466). Sent before an order when the engine does not have
    /// it yet.
    SetInstrumentCurrency { con_id: i64, currency: String },
    /// Subscribe to an account summary (6040=55, ibx#479): `sr_id` is the
    /// subscription id the server echoes on the rows (`SR.Socket.{n}`).
    SubscribeAccountSummary { sr_id: String, tags: String, group: String },
    /// Cancel an account summary subscription.
    CancelAccountSummary { sr_id: String },
    /// Update a strategy parameter.
    UpdateParam { key: String, value: String },
    /// Submit an order from external caller (bridge mode).
    Order(OrderRequest),
    /// Register an instrument from external caller (bridge mode).
    RegisterInstrument { con_id: i64, symbol: String, sec_type: String, exchange: String, reply_tx: Option<crossbeam_channel::Sender<Result<InstrumentId, String>>> },
    /// A slot of its own for the contract of an order given without a
    /// conId: the engine looks the contract up before the order goes out,
    /// as the reference does for each API order (ibx#486).
    RegisterOrderContract { symbol: String, sec_type: String, exchange: String, currency: String, reply_tx: Option<crossbeam_channel::Sender<Result<InstrumentId, String>>> },
    /// Request historical bar data via historical data connection.
    FetchHistorical {
        req_id: ReqId,
        con_id: i64,
        symbol: String,
        /// Security type of the API contract (ibx#305). Empty is a stock.
        sec_type: String,
        /// Exchange of the API contract (ibx#305). Empty is `SMART`.
        exchange: String,
        end_date_time: String,
        duration: String,
        bar_size: String,
        what_to_show: String,
        use_rth: bool,
        keep_up_to_date: bool,
        /// The contract includes expired contracts (ibx#427).
        include_expired: bool,
        /// How bar times are written: 1, 2 or 3 (ibx#431).
        format_date: i32,
    },
    /// Measure auth-connection round-trip time (ibx#158): sends a
    /// test request immediately; the sample lands in
    /// `SharedState::last_ccp_rtt` when the reply arrives.
    Ping,
    /// Cancel a historical data request.
    CancelHistorical { req_id: ReqId },
    /// Request head timestamp via historical data connection.
    FetchHeadTimestamp {
        req_id: ReqId,
        con_id: i64,
        /// Security type of the API contract (ibx#305). Empty is a stock.
        sec_type: String,
        /// Exchange of the API contract (ibx#305). Empty is `SMART`.
        exchange: String,
        what_to_show: String,
        use_rth: bool,
        /// How the time is written: 1, 2 or 3 (ibx#431).
        format_date: i32,
    },
    /// Request contract details via auth connection.
    FetchContractDetails {
        req_id: ReqId,
        con_id: i64,
        symbol: String,
        sec_type: String,
        exchange: String,
        currency: String,
        filters: SecDefFilters,
    },
    /// Cancel a head timestamp request.
    CancelHeadTimestamp { req_id: ReqId },
    /// Search for matching symbols via auth connection.
    FetchMatchingSymbols { req_id: ReqId, pattern: String },
    /// Option chain parameters of an underlying via auth connection
    /// (ibx#440), after the local checks.
    FetchSecDefOptParams {
        req_id: ReqId,
        underlying_symbol: String,
        fut_fop_exchange: String,
        /// The type as the reference reads it: FUT, STK, IND or CASH.
        underlying_sec_type: String,
        underlying_con_id: i64,
    },
    /// Request available exchanges for market depth.
    FetchMktDepthExchanges,
    /// Request scanner parameter XML via historical data connection.
    FetchScannerParams,
    /// Subscribe to a scanner scan via historical data connection.
    SubscribeScanner {
        req_id: ReqId,
        /// Client id of the session, part of the subscription id (ibx#457).
        client_id: i64,
        /// The checked request, with its filters (ibx#456).
        subscription: crate::control::scanner::ScannerSubscription,
    },
    /// Cancel a scanner subscription.
    CancelScanner { req_id: ReqId },
    /// Request historical news via historical data connection.
    FetchHistoricalNews {
        req_id: ReqId,
        con_id: i64,
        provider_codes: String,
        start_time: String,
        end_time: String,
        max_results: u32,
    },
    /// Request a news article via historical data connection.
    FetchNewsArticle {
        req_id: ReqId,
        provider_code: String,
        article_id: String,
    },
    /// Request fundamental data via historical data connection.
    FetchFundamentalData {
        req_id: ReqId,
        con_id: i64,
        report_type: String,
    },
    /// Cancel fundamental data request.
    CancelFundamentalData { req_id: ReqId },
    /// Regulatory snapshot of a contract (ibx#446): one snapshot request to
    /// the farm, answered into the instrument's record.
    SubscribeSnapshot {
        con_id: i64, symbol: String, exchange: String, sec_type: String,
        reply_tx: Option<crossbeam_channel::Sender<Result<InstrumentId, String>>>,
    },
    /// End of a regulatory snapshot: its request is forgotten, nothing is
    /// sent to the farm.
    DropSnapshot { instrument: InstrumentId },
    /// Option calculation (implied volatility or price) of an option by
    /// conId, answered by the local option model.
    CalcOption {
        req_id: ReqId,
        con_id: i64,
        kind: crate::control::optcalc::CalcKind,
        under_price: f64,
    },
    /// Request histogram data via historical data connection.
    FetchHistogramData {
        req_id: ReqId,
        con_id: i64,
        /// Security type of the API contract (ibx#305). Empty is a stock.
        sec_type: String,
        /// Exchange of the API contract (ibx#305). Empty is `SMART`.
        exchange: String,
        use_rth: bool,
        period: String,
    },
    /// Cancel histogram data request.
    CancelHistogramData { req_id: ReqId },
    /// Request historical ticks via historical data connection.
    FetchHistoricalTicks {
        req_id: ReqId,
        con_id: i64,
        /// Symbol of the chart name of the query: the local symbol when
        /// given, else the symbol (ibx#432).
        symbol: String,
        /// Security type of the API contract (ibx#305). Empty is a stock.
        sec_type: String,
        /// Exchange of the API contract (ibx#305). Empty is `SMART`.
        exchange: String,
        start_date_time: String,
        end_date_time: String,
        number_of_ticks: i32,
        what_to_show: String,
        use_rth: bool,
        /// BID_ASK without sizes (ibx#432).
        ignore_size: bool,
    },
    /// Subscribe to real-time 5-second bars via historical data connection.
    SubscribeRealTimeBar {
        req_id: ReqId,
        con_id: i64,
        symbol: String,
        /// Security type of the API contract (ibx#305). Empty is a stock.
        sec_type: String,
        /// Exchange of the API contract (ibx#305). Empty is `SMART`.
        exchange: String,
        what_to_show: String,
        use_rth: bool,
    },
    /// Cancel real-time bar subscription.
    CancelRealTimeBar { req_id: ReqId },
    /// Request historical schedule via historical data connection.
    FetchHistoricalSchedule {
        req_id: ReqId,
        con_id: i64,
        /// Security type of the API contract (ibx#305). Empty is a stock.
        sec_type: String,
        /// Exchange of the API contract (ibx#305). Empty is `SMART`.
        exchange: String,
        end_date_time: String,
        duration: String,
        use_rth: bool,
    },
    /// Subscribe to market depth (L2) for a contract.
    SubscribeDepth {
        req_id: ReqId,
        con_id: i64,
        exchange: String,
        sec_type: String,
        num_rows: i32,
        is_smart_depth: bool,
    },
    /// Unsubscribe from market depth.
    UnsubscribeDepth { req_id: ReqId },
    /// Request news providers list (gateway-local).
    FetchNewsProviders { req_id: ReqId },
    /// Request SMART routing components.
    FetchSmartComponents { req_id: ReqId, bbo_exchange: String },
    /// Request soft dollar tiers.
    FetchSoftDollarTiers { req_id: ReqId },
    /// Request user info.
    FetchUserInfo { req_id: ReqId },
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
    pub total_cash_value: Price,
    pub settled_cash: Price,
    pub accrued_cash: Price,
    pub equity_with_loan: Price,
    pub gross_position_value: Price,
    pub init_margin_req: Price,
    pub maint_margin_req: Price,
    pub available_funds: Price,
    pub excess_liquidity: Price,
    pub cushion: Price,        // percentage * PRICE_SCALE (e.g. 0.45 = 45%)
    pub sma: Price,
    pub day_trades_remaining: i64,
    pub leverage: Price,       // ratio * PRICE_SCALE
    pub daily_pnl: Price,
}

/// Position with average cost, for P&L computation and reqPositions.
#[derive(Debug, Clone, Default)]
pub struct PositionInfo {
    pub con_id: i64,
    /// Fixed-point (QTY_SCALE).
    pub position_fixed: Qty,
    pub avg_cost: Price,      // per-share avg cost * PRICE_SCALE
    pub symbol: String,
    pub sec_type: String,
    pub currency: String,
    pub multiplier: String,
    // Per-position marks from the account-updates snapshot (ib-agent#172).
    // Set only by the portfolio-value message, not the lean position feed.
    pub market_price: Price,     // per-share mark * PRICE_SCALE
    pub market_value: Price,     // position mark * PRICE_SCALE
    pub unrealized_pnl: Price,   // * PRICE_SCALE
    pub realized_pnl: Price,     // * PRICE_SCALE
}

/// Per-position midnight seed from 6040=143 P&L subscription.
/// Used for client-side daily P&L computation.
#[derive(Debug, Clone, Copy, Default)]
pub struct MidnightSeed {
    pub con_id: i64,
    pub qty_midnight_fixed: Qty,  // position held at midnight, fixed-point (QTY_SCALE)
    pub money_traded: f64,            // net cash from today's fills (signed)
    pub realized_pnl: f64,           // realized P&L since midnight
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem;

    // ibx#447: the reference's reqMarketDataType table, and the entry mode
    // of each frozen / delayed pair.
    #[test]
    fn market_data_modes_follow_the_reference_table() {
        let mut m = MarketDataModes::default();
        assert!(m.apply(3));
        assert_eq!(m, MarketDataModes { frozen: false, delayed: true, delayed_frozen: false });
        assert!(m.apply(2));
        assert_eq!(m, MarketDataModes { frozen: true, delayed: true, delayed_frozen: false }, "2 keeps delayed");
        assert!(m.apply(4));
        assert_eq!(m, MarketDataModes { frozen: true, delayed: true, delayed_frozen: true });
        assert!(m.apply(3));
        assert_eq!(m, MarketDataModes { frozen: true, delayed: true, delayed_frozen: false }, "3 keeps frozen");
        assert!(!m.apply(0) && !m.apply(5));
        assert_eq!(m, MarketDataModes { frozen: true, delayed: true, delayed_frozen: false });
        assert!(m.apply(1));
        assert_eq!(m, MarketDataModes::default());
        assert_eq!([(false, false), (false, true), (true, false), (true, true)]
            .map(|(f, d)| MarketDataModes::entry_mode(f, d)), [0, 1, 2, 3]);
    }

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
            buf.push(OrderRequest::Cancel { order_id: i as OrderId });
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
            qty: 200,
            kind: OrderKind::Limit { price: 100 * PRICE_SCALE },
            tif: b'0',
            attrs: OrderAttrs::default(),
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
                buf.push(OrderRequest::Cancel { order_id: (cycle * 5 + i) as OrderId });
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

    // ── ibx#263: a price off the grid is refused, not snapped (ibx#216) ──

    const TICK_CENT: i64 = PRICE_SCALE / 100; // 0.01

    #[test]
    fn off_grid_is_a_negative_price_or_one_off_the_tick() {
        assert!(!off_grid(15_012_000_000, TICK_CENT, false));
        assert!(off_grid(15_012_300_000, TICK_CENT, false));
        assert!(!off_grid(0, TICK_CENT, false));
        let nickel = 5 * TICK_CENT;
        assert!(off_grid(10_02_000_000, nickel, false));
        assert!(!off_grid(10_05_000_000, nickel, false));
        // Unknown tick: only a negative price is refused (ib-agent#192 B5:
        // a REL offset of -0.50).
        assert!(!off_grid(15_012_345_678, 0, false));
        assert!(off_grid(-50_000_000, 0, false));
        assert!(off_grid(-50_000_000, TICK_CENT, false));
        // A combo's rule allows a price at or below 0 (`jclient.dy.cP()`;
        // captured BAG limits -73.15 and -50.10, ibx#470), on its tick.
        assert!(!off_grid(-7_315_000_000, 0, true));
        assert!(!off_grid(-5_010_000_000, TICK_CENT, true));
        assert!(off_grid(-5_010_500_000, TICK_CENT, true));
    }

    #[test]
    fn off_grid_checks_the_limit_and_the_stop_or_offset_prices() {
        let stop_limit = |price, stop_price| OrderRequest::SubmitStopLimit {
            order_id: 3, instrument: 0, side: Side::Buy, qty: 1, price, stop_price,
        };
        assert_eq!(stop_limit(15_012_000_000, 15_100_000_000).off_grid_order(TICK_CENT, false), None);
        assert_eq!(stop_limit(15_012_345_678, 15_100_000_000).off_grid_order(TICK_CENT, false), Some(3));
        assert_eq!(stop_limit(15_012_000_000, 15_099_999_999).off_grid_order(TICK_CENT, false), Some(3));
        let ex = |kind| OrderRequest::SubmitEx {
            order_id: 4, instrument: 0, side: Side::Sell, qty: 1, kind, tif: b'1', attrs: OrderAttrs::default(),
        };
        assert_eq!(ex(OrderKind::Stop { stop_price: 24_000_123_456 }).off_grid_order(TICK_CENT, false), Some(4));
        assert_eq!(ex(OrderKind::Rel { price: 0, offset: -50_000_000 }).off_grid_order(TICK_CENT, false), Some(4));
        // Not checked: a percent, a trailing stop price, a TRAIL LIMIT offset.
        assert_eq!(ex(OrderKind::TrailPct { trail_percent: 123_900_000, trail_stop_price: 1 }).off_grid_order(TICK_CENT, false), None);
        assert_eq!(ex(OrderKind::TrailingStop { trail_amt: TICK_CENT, trail_stop_price: 1 }).off_grid_order(TICK_CENT, false), None);
        assert_eq!(ex(OrderKind::TrailingStopLimit { lmt_offset: 1, lmt_price: None, trail_amt: TICK_CENT, trail_stop_price: 0 })
            .off_grid_order(TICK_CENT, false), None);
        assert_eq!(ex(OrderKind::TrailingStop { trail_amt: 1, trail_stop_price: 0 }).off_grid_order(TICK_CENT, false), Some(4));
        // A what-if is checked as its order; a bracket answers for its leg.
        let what_if = OrderRequest::SubmitWhatIf { request: Box::new(stop_limit(15_012_345_678, 0)) };
        assert_eq!(what_if.off_grid_order(TICK_CENT, false), Some(3));
        let bracket = OrderRequest::SubmitBracket {
            parent_id: 10, tp_id: 11, sl_id: 12, instrument: 0, side: Side::Buy, qty: 1,
            entry_price: 100 * PRICE_SCALE, take_profit: 110 * PRICE_SCALE, stop_loss: 90 * PRICE_SCALE + 1,
        };
        assert_eq!(bracket.off_grid_order(TICK_CENT, false), Some(12));
        // Unknown tick: nothing off the grid but a negative price.
        assert_eq!(stop_limit(15_012_345_678, 0).off_grid_order(0, false), None);
    }

    #[test]
    fn instrument_accessor_covers_submits() {
        let req = OrderRequest::SubmitMarket { order_id: 1, instrument: 7, side: Side::Buy, qty: 1 };
        assert_eq!(req.instrument(), Some(7));
        assert_eq!(OrderRequest::Cancel { order_id: 1 }.instrument(), None);
        assert_eq!(
            OrderRequest::Modify {
                new_order_id: 2, order_id: 1, qty: 1,
                kind: OrderKind::Market, tif: b'0', attrs: OrderAttrs::default(),
            }.instrument(),
            None
        );
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
        let req = OrderRequest::Modify {
            new_order_id: 100, order_id: 99, qty: 10,
            kind: OrderKind::Stop { stop_price: 200 * PRICE_SCALE },
            tif: b'1', attrs: OrderAttrs::default(),
        };
        match req {
            OrderRequest::Modify { order_id, qty, kind, tif, .. } => {
                assert_eq!(order_id, 99);
                assert!(matches!(kind, OrderKind::Stop { stop_price } if stop_price == 200 * PRICE_SCALE));
                assert_eq!(qty, 10);
                assert_eq!(tif, b'1');
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
        let cmd = ControlCommand::Subscribe { con_id: 265598, symbol: "AAPL".into(), exchange: String::new(), sec_type: String::new(), last_trade_date: String::new(), strike: 0.0, right: String::new(), multiplier: String::new(), mode_9887: 0, snapshot: false, reply_tx: None };
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
        let cmd = ControlCommand::Subscribe { con_id: 42, symbol: "TEST".into(), exchange: String::new(), sec_type: String::new(), last_trade_date: String::new(), strike: 0.0, right: String::new(), multiplier: String::new(), mode_9887: 0, snapshot: false, reply_tx: None };
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
            cum_qty_fixed: (0) as i64 * crate::types::QTY_SCALE, avg_price: 0,
            instrument: 0,
            order_id: 1,
            side: Side::Buy,
            price: 150 * PRICE_SCALE,
            qty_fixed: (100) as i64 * crate::types::QTY_SCALE,
            remaining_fixed: (0) as i64 * crate::types::QTY_SCALE,
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
            qty_fixed: (50) as i64 * crate::types::QTY_SCALE,
            filled_fixed: (10) as i64 * crate::types::QTY_SCALE,
            status: OrderStatus::PartiallyFilled,
            ord_type: b'2',
            tif: b'0',
            stop_price: 0,
        };
        let o2 = o; // Copy
        assert_eq!(o.order_id, o2.order_id);
        assert_eq!(o.filled_fixed / crate::types::QTY_SCALE, o2.filled_fixed / crate::types::QTY_SCALE);
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
    fn what_if_response_is_clone() {
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
            ..Default::default()
        };
        let r2 = r.clone();
        assert_eq!(r.init_margin_after, r2.init_margin_after);
        assert_eq!(r.commission, r2.commission);
    }

    // --- AdjustedOrderType ---

    #[test]
    fn adjusted_order_type_fix_codes() {
        assert_eq!(AdjustedOrderType::Stop.fix_code(), "3");
        assert_eq!(AdjustedOrderType::StopLimit.fix_code(), "4");
        assert_eq!(AdjustedOrderType::Trail.fix_code(), "T");
        assert_eq!(AdjustedOrderType::TrailLimit.fix_code(), "TSL");
    }

    // --- OrderAttrs cash_qty ---

    #[test]
    fn order_attrs_cash_qty_default_zero() {
        let attrs = OrderAttrs::default();
        assert_eq!(attrs.cash_qty, 0);
    }
}
