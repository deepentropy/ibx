//! The types the engine and the wire share: what a request is, what a
//! quote is, and the fixed-point scales prices and sizes are held in.
//!
//! Prices and sizes are integers here, scaled by [`PRICE_SCALE`] and
//! [`QTY_SCALE`]. They become floating point at the caller's edge and
//! nowhere before it.
//!
//! Not the caller-facing surface. What a program written against this client
//! touches is [`crate::api`], which is documented in full and gated on staying
//! that way. This module is the engine underneath it, exported because the
//! binaries, benchmarks and integration tests in this repository reach it.

/// The objects a caller works in: contracts, orders, executions, the state
/// the venue reports them in. Named apart from the wire scalars above because
/// both surfaces present them and both carry an `Order` — one the caller's,
/// one this engine's.
pub mod model;

/// An order as this client holds it.
pub mod orders;
pub use orders::*;

/// What a surface asks the engine to do.
pub mod commands;
pub use commands::*;

/// How the venue names an order's state.
pub mod order_status;


/// Internal instrument identifier. Mapped from IB's conId at subscription time.
/// Used as an index into pre-allocated arrays, so values are dense and small.
pub type InstrumentId = u32;

/// Engine-assigned order identifier.
pub type OrderId = u64;

/// Fixed-point price: value * 10^8. Avoids floating-point on the hot path.
/// Example: $150.25 = 15_025_000_000
pub type Price = i64;

/// Fixed-point quantity: value * `QTY_SCALE`.
/// Example: 100 shares = 1_000_000
pub type Qty = i64;

/// How a price is held here: a whole number of hundred-millionths.
///
/// This is not how the venue sends one. The venue sends a price as a whole
/// number of the contract's own smallest increment, carried alongside that
/// increment and converted to a decimal only where one is needed. That
/// representation has no floor: a contract
/// quoted in millionths works exactly as well as one quoted in pennies, because
/// the count is relative to the contract rather than to a fixed scale.
///
/// Holding a price against one fixed scale instead buys simple arithmetic
/// between contracts and costs a floor. A contract whose increment is finer
/// than a hundred-millionth cannot be held at all, and its increment scales to
/// nothing — which is caught rather than silently rounded, but caught is not
/// carried. Matching the venue would mean holding the count and the increment
/// together and converting at the edge, which is a change to every price this
/// client touches.
pub const PRICE_SCALE: i64 = 100_000_000; // 10^8
/// Quantities are held to a hundred-millionth, the same as prices.
///
/// A size is a count of what the venue said sizes move in for the contract,
/// and for a crypto that is a hundred-millionth of a coin. Held to a
/// ten-thousandth, every size finer than that rounded to nothing: a quote for
/// a thousandth of a coin came back as no quote at all, which reads as an
/// empty book rather than as a limit of this client's.
///
/// A day's volume in the busiest listing is some thousands of millions of
/// shares, which at this scale is four orders of magnitude inside what the
/// field holds.
pub const QTY_SCALE: i64 = 100_000_000; // 10^8

/// Convert a wire quantity into the `QTY_SCALE` fixed-point form the `Quote`
/// fields hold. Every reader divides by `QTY_SCALE`, so a decode path that
/// stores the magnitude raw delivers quantities 10_000x too small.
/// Saturating: the magnitude is server-supplied, and a clamped quantity is
/// preferable to a wrapped one.
#[inline(always)]
pub fn qty_from_wire(magnitude: i64) -> Qty {
    magnitude.saturating_mul(QTY_SCALE)
}

/// Convert a fixed-point `Qty` into the decimal a caller reads it as.
///
/// The inverse of [`qty_from_wire`], and the one place the division lives: a
/// quantity handed out without it is `QTY_SCALE` times what filled.
#[inline(always)]
pub fn qty_to_f64(qty: Qty) -> f64 {
    qty as f64 / QTY_SCALE as f64
}

/// The largest quantity an order may state.
///
/// The venue's own bound, which it refuses past rather than works: the size it
/// reads an order under is checked against this on the way in and the order is
/// answered as too large. Stated here so a caller is told before the request
/// goes, in the same terms they would have been told after.
///
/// Well inside what the fixed-point form holds — `Qty` reaches some
/// ninety-two thousand million at this scale — so every accepted quantity
/// converts exactly. Bounded instead at where a double stops multiplying
/// exactly, at a fortieth of this, an ordinary cash order stated in currency
/// units was refused here and taken by the venue.
pub const MAX_QTY_SHARES: f64 = 999_999_999.0;

/// Convert a decimal price into the fixed-point form `Price` holds.
///
/// The one place the multiplication lives, and rounded rather than truncated
/// for the same reason [`qty_from_f64`] is. A binary double holding a decimal
/// price sits a hair below it about as often as above: `0.29 * 1e8` is
/// `28999999.999...`, and truncating sends `0.28999999` — a price the caller
/// never stated, off the instrument's tick, which the venue may refuse or may
/// work at a price that is not the one asked for. Better than five in a
/// hundred ordinary two-decimal prices land on that side.
///
/// Reading a price the venue stated is the same conversion and wants the same
/// answer: a fill reported at `0.29` is worth `0.29`, not a hundredth of a
/// cent less, everywhere it is later added up.
#[inline]
pub fn price_from_f64(price: f64) -> Price {
    if !price.is_finite() {
        return 0;
    }
    (price * PRICE_SCALE as f64).round() as Price
}

/// Convert a caller's decimal quantity into the fixed-point form `Qty` holds.
///
/// The inverse of [`qty_to_f64`], and the one place the multiplication lives.
/// Rounded rather than truncated: a caller asking for a fraction of a share
/// stated it as a decimal, and truncation places an order for none of it.
///
/// The whole part is scaled in integer arithmetic and only the fraction goes
/// through the double. Multiplied together instead, the product passes the 53
/// bits a double carries at some ninety million units and the low digits go —
/// so every quantity above that had either to be refused or to go out as a
/// size nobody asked for. A cash order is stated in currency units, where a
/// hundred million is an ordinary size rather than a corner case.
#[inline]
pub fn qty_from_f64(shares: f64) -> Qty {
    if !shares.is_finite() {
        return 0;
    }
    // Always below one, so this product is nowhere near the double's limit.
    let whole = shares.trunc();
    let frac = ((shares - whole) * QTY_SCALE as f64).round();
    // Bounded by what this form holds, not by what an order may state. The two
    // are different rules: the venue refuses an order above its own size bound,
    // and order validation says so before the request goes — but a position, a
    // book size and a cumulative quantity all convert through here as well, and
    // none of them is an order. Held to the order bound, a holding above it
    // came back as the same clamped figure however it moved, so a position
    // that changed by a share reported no change at all.
    const HOLDS_AT_MOST: f64 = (Qty::MAX / QTY_SCALE) as f64;
    if whole.abs() > HOLDS_AT_MOST {
        return if shares < 0.0 { Qty::MIN } else { Qty::MAX };
    }
    (whole as Qty)
        .saturating_mul(QTY_SCALE)
        .saturating_add(frac as Qty)
}

/// Convert a counted size into the `QTY_SCALE` fixed-point form, where the
/// venue stated what it counts this instrument's sizes in.
///
/// A size on the wire is a count of the increment the venue named on the
/// subscription acknowledgement: whole ones for a share, hundred-millionths
/// for a crypto. Counting every one as whole ones reports a crypto's size a
/// hundred million times over.
///
/// The venue names one on every subscription a session has made — a share
/// acknowledged `1`, a crypto `1e-8`, never nothing. Nought here is therefore
/// a shape it has not been seen to send, and the whole ones it falls to are a
/// defence rather than a reading: what stating none would mean is not
/// something this client has been told.
#[inline]
pub fn qty_from_counted(counted: i64, size_tick: f64) -> Qty {
    if size_tick <= 0.0 || size_tick == 1.0 {
        return qty_from_wire(counted);
    }
    // Held at the ceiling if it will not fit, rather than wrapped. The cast
    // from a float saturates in Rust, so a size past what can be held comes
    // back as the largest one and never as a negative — a negative size is a
    // sell where there was a buy.
    (counted as f64 * size_tick * QTY_SCALE as f64).round() as Qty
}

/// How many contracts this client holds a slot for at once.
///
/// This number is this client's own and is not stated anywhere on the wire.
/// The tables are allocated once at this size and never move, so a slot's
/// address is stable while a reader holds it, and the size has to be chosen
/// before any of them is taken.
///
/// What is measured is that it has to be well above the two hundred and
/// fifty-six it used to be: one option chain asked for at once is 282 live
/// subscriptions on a single underlying, and the venue served all of them
/// without refusing one (`src/bin/capture_line_limit.rs`). At the old size
/// this client refused the two hundred and fifty-seventh while the venue was
/// still serving.
///
/// The venue states its subscription allowance on the logon. This size only
/// bounds storage; slots are reused when nothing holds their contract.
pub const MAX_INSTRUMENTS: usize = 4096;

/// How deep a healthy backlog of order requests goes, which is what the
/// buffer is built to hold without asking for more room. Not a limit: it
/// grows past this rather than drop anything.
const MAX_PENDING_ORDERS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Which way a trade goes.
pub enum Side {
    /// Buying.
    Buy,
    /// Selling.
    Sell,
    /// Short sell (FIX tag 54 = "5"). Used for short-selling stocks.
    ShortSell,
}

/// Current quote for an instrument. Cache-line aligned for hot-path access.
#[derive(Clone, Copy)]
#[repr(C, align(64))]
#[derive(Default)]
pub struct Quote {
    /// The best price anyone is offering to buy at, scaled by `PRICE_SCALE`.
    pub bid: Price,
    /// The best price anyone is offering to sell at.
    pub ask: Price,
    /// What it last traded at.
    pub last: Price,
    /// How much is offered at the bid, scaled by `QTY_SCALE`.
    pub bid_size: Qty,
    /// How much at the ask.
    pub ask_size: Qty,
    /// How much last traded.
    pub last_size: Qty,
    /// How much has traded today.
    pub volume: Qty,
    /// What it opened at.
    pub open: Price,
    /// The highest it has traded today.
    pub high: Price,
    /// The lowest.
    pub low: Price,
    /// What it closed at, which is what a quiet market states.
    pub close: Price,
    /// When this quote was read, in nanoseconds since the epoch.
    pub timestamp_ns: u64,
    /// Bid-exchange bitmask. Each set bit indexes into smart_components by bit_number.
    pub bid_exch_mask: i64,
    /// Which venues are at the ask, as a mask over the contract's own
    /// list.
    pub ask_exch_mask: i64,
    /// Which venue the last trade was on.
    pub last_exch_mask: i64,
    /// Whether the venue has halted trading in this contract, and why.
    ///
    /// 0 not halted, 1 stopped by a regulator, 2 stopped because the price
    /// moved too far. The venue states it and it was decoded and dropped, so a
    /// halted contract read as a live market: every surface kept presenting the
    /// last price before the halt as a current one, and a program pricing
    /// against it is pricing against a book that is not there.
    ///
    /// The two reasons are the venue's own, and they call for opposite
    /// handling: a volatility pause lifts on a clock, and a regulator's halt
    /// lifts when the regulator says so. Folded to a yes or no, this field
    /// could only ever be 0 or 1 and the second reason was unreachable.
    /// The restriction the venue states on the same record is not here: this
    /// record is two cache lines exactly and one more field costs a third, on
    /// every slot in the table. It is read by
    /// [`crate::bridge::MarketDataState::short_sale_restricted`] instead.
    pub halted: i64,
}

/// Execution fill report.
#[derive(Debug, Clone, Copy)]
pub struct Fill {
    /// Which contract filled.
    pub instrument: InstrumentId,
    /// The order it filled against.
    pub order_id: OrderId,
    /// Whether it bought or sold.
    pub side: Side,
    /// At what price.
    pub price: Price,
    /// How much filled on this report.
    pub qty: i64,
    /// How much of the order is still working.
    pub remaining: i64,
    /// When it filled.
    pub timestamp_ns: u64,
    /// FIX tag 14 CumQty — filled across the whole order, not this print.
    pub cum_qty: i64,
    /// FIX tag 6 AvgPx — volume-weighted across every print of this order.
    /// `price` is this print alone.
    pub avg_price: Price,
}

/// A holding the venue reports that this broker does not hold itself.
///
/// The venue keeps three sets of holdings: its own, those held away at another
/// broker, and rows it marks as shown but not held. Only the first is what a
/// caller asking for positions means, so the others are kept here rather than
/// added to them.
#[derive(Debug, Clone, PartialEq)]
pub struct PositionElsewhere {
    /// The contract held.
    pub con_id: i64,
    /// Its ticker.
    pub symbol: String,
    /// What kind of contract it is.
    pub sec_type: String,
    /// What it is priced in.
    pub currency: String,
    /// How much is held.
    pub position: f64,
    /// What it cost on average.
    pub avg_cost: Price,
    /// Where the venue says it sits.
    pub held: HeldElsewhere,
}

/// Which of the venue's other sets of holdings a row came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HeldElsewhere {
    /// Held away, at another broker.
    Away,
    /// Shown but not held.
    DisplayOnly,
    /// Reported apart from the account's own holdings, without saying why.
    Aside,
}

/// What the venue makes of an option: its own model price, the greeks and the
/// volatility that price implies.
///
/// A field the venue did not state is `f64::MAX`, the reference client's own
/// mark for a value that was not sent. Zero is a real greek.
#[derive(Default, Debug, Clone, Copy)]
pub struct OptionComputation {
    /// The option this models.
    pub instrument: InstrumentId,
    /// The request this answers, where the computation was made here rather
    /// than by the venue. Venue-sent bulletins arrive against an instrument and are
    /// reported under whichever request subscribed it; a local calculation
    /// answers the call that asked for it and has no subscription behind it,
    /// so it names that call rather than borrowing the instrument field.
    pub answers: Option<i64>,
    /// What volatility the price implies, over a year.
    ///
    /// The venue states this over one of the days it counts beside it, and the
    /// reference client states it over a year. Handed on as the wire carries
    /// it, every volatility read from this client would be short by the root
    /// of a year — under one per cent where the contract carries eighteen.
    pub implied_vol: f64,
    /// How much the option moves with the underlying.
    pub delta: f64,
    /// What the model says the option is worth.
    pub opt_price: f64,
    /// The present value of dividends before expiry.
    pub pv_dividend: f64,
    /// How much the delta moves with it.
    pub gamma: f64,
    /// How much the option moves with volatility.
    pub vega: f64,
    /// How much it loses with time.
    pub theta: f64,
    /// What the model says the underlying is worth.
    pub und_price: f64,
    /// How long the venue says the contract has left, in days, carried to the
    /// fraction — the hours of the last day included.
    ///
    /// `f64::MAX` where the venue stated none. Read rather than counted: a
    /// count of whole days from this machine's clock has no hours in it, and
    /// makes a contract expiring today look expired.
    pub cal_days: f64,
    /// The interest rate the venue discounted this contract at, over a year.
    ///
    /// Stated on the same tick as the model, over one of the days it counts
    /// beside it, and carried across to the year the volatility above is
    /// stated over so the two read together.
    ///
    /// `f64::MAX` where the venue stated none.
    pub rate: f64,
    /// How much the option moves with the interest rate, over a year.
    ///
    /// The one first-order greek the documented callback has no field for, and
    /// the venue states it on the same tick as the four that do. A rates desk,
    /// and any long-dated or bond-like option book, prices against it.
    ///
    /// `f64::MAX` where the venue stated none.
    pub rho: f64,
    /// How long the venue's model expects the option to run before it is
    /// exercised, in the days it counts.
    ///
    /// An American option can be exercised early and a European one cannot,
    /// which is the whole of the difference between their prices; this is what
    /// the venue's model makes of that for this contract. `f64::MAX` where the
    /// venue stated none.
    pub fugit: f64,
    /// The underlying price at which the venue's model says exercising early
    /// becomes worth more than holding.
    ///
    /// `f64::MAX` where the venue stated none.
    pub exercise_boundary: f64,
    /// The coefficient the venue's model carries the underlying forward by.
    ///
    /// `f64::MAX` where the venue stated none.
    pub forward_coeff: f64,
    /// The yield the venue's own model discounted this contract at.
    ///
    /// `f64::MAX` where the venue stated none.
    pub model_yield: f64,
    /// The yield the venue carried across from the contract this one is
    /// bridged to, where it priced one from the other.
    ///
    /// `f64::MAX` where the venue stated none.
    pub bridge_yield: f64,
    /// What the venue's model says the option is worth beyond its intrinsic
    /// value.
    ///
    /// The model's own figure, not the difference a caller works out from the
    /// model price and the strike — those disagree wherever the model prices
    /// early exercise. `f64::MAX` where the venue stated none.
    pub time_value: f64,
    /// Whether the venue priced this contract on a volatility stated in the
    /// contract's own price units rather than as a fraction of the underlying.
    ///
    /// The venue states which of the two it used, per contract, on the same
    /// tick as the model. A price-unit volatility is a standard deviation in
    /// points per root year: read as a fraction it is wrong by roughly the
    /// forward, which prices a far strike at nothing.
    pub price_based_vol: bool,
}

impl OptionComputation {
    /// A computation with only the figures a solve produces, and every figure
    /// it does not marked unstated.
    ///
    /// Solving states a volatility against a price, and no greek. Left at
    /// zero, a greek nobody computed reads as a real one — an option with no
    /// delta — where an unstated figure reads as the nothing it is.
    ///
    /// Every field is named rather than filled in from the default, so a
    /// figure added to this record fails to compile here instead of arriving
    /// from a solve as a real zero. That is the fault this constructor exists
    /// to prevent, and a default would reintroduce it silently.
    pub fn solved(answers: i64) -> Self {
        Self {
            answers: Some(answers),
            instrument: 0,
            implied_vol: 0.0,
            opt_price: 0.0,
            und_price: 0.0,
            delta: f64::MAX,
            gamma: f64::MAX,
            vega: f64::MAX,
            theta: f64::MAX,
            pv_dividend: f64::MAX,
            cal_days: f64::MAX,
            rate: f64::MAX,
            rho: f64::MAX,
            fugit: f64::MAX,
            exercise_boundary: f64::MAX,
            forward_coeff: f64::MAX,
            model_yield: f64::MAX,
            bridge_yield: f64::MAX,
            time_value: f64::MAX,
            price_based_vol: false,
        }
    }
}

/// What the venue states about a contract itself on the tick that carries its
/// price extremes, beyond the extremes.
///
/// The documented API has no tick for either of these: the reference client
/// reaches share count through a separate fundamentals request and never
/// reports a year-ago open at all. The venue states both on the quote feed of
/// every subscription that asks for the extremes, and they were read past.
///
/// `f64::MAX` where the venue stated none.
#[derive(Debug, Clone, Copy)]
pub struct ContractFigures {
    /// How many shares the company has on issue.
    ///
    /// The multiplier that turns a price into a market capitalisation. The
    /// venue states it in millions; it is carried here as a count of shares,
    /// which is the figure it stands for — read against two live contracts
    /// three orders of magnitude apart, and both matched.
    pub shares_outstanding: f64,
    /// What the contract opened at a year ago, which gives a trailing return
    /// without asking for a year of history.
    pub open_a_year_ago: f64,
}

impl Default for ContractFigures {
    fn default() -> Self {
        Self { shares_outstanding: f64::MAX, open_a_year_ago: f64::MAX }
    }
}

/// Tick-by-tick data type for subscription requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TbtType {
    /// Every trade, including those reported away from the exchange.
    AllLast,
    /// Trades on the exchange itself. A different stream to the venue, and
    /// asking for one under the other's name is asking for someone else's
    /// trades.
    Last,
    /// Bid/ask quote ticks (BidAsk).
    BidAsk,
    /// The point between the two, each time it moves.
    MidPoint,
}

impl TbtType {
    /// The stream a caller named, under the reference client's own names.
    ///
    /// `Last` and `AllLast` are two streams, not two names for one: the second
    /// carries trades reported away from the exchange and the first does not.
    /// Both clients read the name here, so neither can drift from the other.
    pub fn named(name: &str) -> Result<Self, String> {
        match name {
            "AllLast" => Ok(Self::AllLast),
            "Last" => Ok(Self::Last),
            "BidAsk" => Ok(Self::BidAsk),
            "MidPoint" => Ok(Self::MidPoint),
            other => Err(format!("no such kind of tick: {other}")),
        }
    }
}

/// One reading of the point between the bid and the ask.
#[derive(Debug, Clone, Copy)]
pub struct TbtMid {
    /// The contract.
    pub instrument: InstrumentId,
    /// The request this arrived under, as the caller numbered it.
    pub req_id: i64,
    /// The point between the two.
    pub price: Price,
    /// When, in seconds since the epoch, as the venue states it.
    pub timestamp: u64,
}

/// A single tick-by-tick trade (AllLast) from 35=E.
#[derive(Debug, Clone)]
pub struct TbtTrade {
    /// The contract that traded.
    pub instrument: InstrumentId,
    /// The request this arrived under, as the caller numbered it.
    ///
    /// Carried on the record rather than looked up from the contract: a
    /// contract can have several tick streams at once — every trade, and
    /// every quote change — and looking up by contract hands both of them
    /// whichever request was made last.
    pub req_id: i64,
    /// At what price.
    pub price: Price,
    /// How much.
    pub size: i64,
    /// When, in seconds since the epoch, as the venue states it — handed on
    /// unscaled, which is what the reference client's tick-by-tick callbacks
    /// carry.
    pub timestamp: u64,
    /// Which venue it printed on.
    pub exchange: String,
    /// What has to be true before it is placed.
    pub conditions: String,
    /// The venue may still revise this print.
    ///
    /// Stated by the venue and decoded off the wire, then thrown away and
    /// reported as false — so a caller was told a print was final when the
    /// venue had said it might not be.
    pub past_limit: bool,
    /// The print did not go to the tape.
    pub unreported: bool,
}

/// A single tick-by-tick bid/ask quote from 35=E.
#[derive(Debug, Clone, Copy)]
pub struct TbtQuote {
    /// The contract quoted.
    pub instrument: InstrumentId,
    /// The request this arrived under, as the caller numbered it.
    ///
    /// Carried on the record rather than looked up from the contract: a
    /// contract can have several tick streams at once — every trade, and
    /// every quote change — and looking up by contract hands both of them
    /// whichever request was made last.
    pub req_id: i64,
    /// The best bid.
    pub bid: Price,
    /// The best ask.
    pub ask: Price,
    /// How much at the bid.
    pub bid_size: i64,
    /// How much at the ask.
    pub ask_size: i64,
    /// When, in seconds since the epoch, as the venue states it — handed on
    /// unscaled, which is what the reference client's tick-by-tick callbacks
    /// carry.
    pub timestamp: u64,
    /// The bid is below the day's low, or the ask above its high — the venue's
    /// own words about whether this quote sits outside the day's range.
    pub bid_past_low: bool,
    /// Whether the ask is above the day's high.
    pub ask_past_high: bool,
}

/// An IB news bulletin from auth server news bulletin message.
#[derive(Debug, Clone)]
pub struct NewsBulletin {
    /// The venue's number for this notice.
    pub msg_id: i32,
    /// 1=Regular, 2=Exchange unavailable, 3=Exchange available.
    pub msg_type: i32,
    /// What it says.
    pub message: String,
    /// Where the request is directed, or the contract is listed.
    pub exchange: String,
}

/// A market depth (L2 order book) update.
#[derive(Debug, Clone)]
pub struct DepthUpdate {
    /// The request these levels answer.
    pub req_id: u32,
    /// Book position (0-based).
    pub position: i32,
    /// Market maker ID (L2 only).
    pub market_maker: String,
    /// 0 = insert, 1 = update, 2 = delete.
    pub operation: i32,
    /// 0 = ask, 1 = bid.
    pub side: i32,
    /// At what price.
    pub price: f64,
    /// How much.
    pub size: f64,
    /// Whether the book was asked for on no particular venue.
    pub is_smart_depth: bool,
}

/// Exchange metadata for market depth availability.
#[derive(Debug, Clone)]
pub struct DepthMktDataDescription {
    /// The exchange, as the venue names it.
    pub exchange: String,
    /// Which kind of contract this row is about.
    pub sec_type: String,
    /// The exchange's own full name.
    pub listing_exch: String,
    /// What kind of data it carries. Not stated by the venue
    /// here, and so not stated.
    pub service_data_type: String,
    /// Which group it aggregates into. Not stated by the venue here.
    pub agg_group: i32,
}
/// The single character the server gives a venue, where it has given one.
///
/// A table written here would name nothing for every venue absent from it —
/// most of the United States, and all of everywhere else — and could not be
/// checked against what the server assigns. A venue is named by the name the
/// server states it under.
pub fn exchange_letter(_exchange: &str) -> &'static str {
    ""
}

#[derive(Debug, Clone)]
/// One venue behind a quote's exchange mask, and the letter it is named by.
pub struct SmartComponent {
    /// Which bit of a quote's exchange mask this venue is.
    pub bit_number: i32,
    /// The venue.
    pub exchange: String,
    /// The letter it is named by.
    pub exchange_letter: String,
}

/// A news data provider.
#[derive(Debug, Clone)]
pub struct NewsProvider {
    /// How the venue names the provider.
    pub code: String,
    /// Its full name.
    pub name: String,
}

/// A soft dollar tier (commission sharing arrangement).
#[derive(Debug, Clone)]
pub struct SoftDollarTier {
    /// The tier's name.
    pub name: String,
    /// What it is worth.
    pub val: String,
    /// How it is shown.
    pub display_name: String,
}

/// A family code linking related accounts.
#[derive(Debug, Clone)]
pub struct FamilyCode {
    /// The account.
    pub account_id: String,
    /// The family, as the venue names it.
    pub family_code_str: String,
}

/// A real-time news headline from 8=O|35=G tick type 0x1E90.
#[derive(Debug, Clone)]
pub struct TickNews {
    /// The contract it is about.
    pub instrument: InstrumentId,
    /// Which provider published it.
    pub provider_code: String,
    /// Its id, for fetching the body.
    pub article_id: String,
    /// The headline itself.
    pub headline: String,
    /// When it was published.
    pub timestamp: u64,
}

/// What the venue says about one side of a quote, from the masks it states
/// both sides in.
///
/// It carries the two sides together — one bit each for whether the bid and
/// the ask may be dealt on without a human, one shared bit for a pre-open
/// indication, and one each for a side that has run past its limit — while a
/// caller is handed an attribute beside each price. Which side to read is the
/// number the price is going out under.
pub fn quote_attributes(
    tick_type: i32,
    eligible_mask: i64,
    quote_state_mask: i64,
) -> crate::types::model::TickAttrib {
    // The delayed numbers are the same two prices, served from another feed.
    let bid = matches!(tick_type, 1 | 66);
    let ask = matches!(tick_type, 2 | 67);
    crate::types::model::TickAttrib {
        can_auto_execute: (bid && eligible_mask & 0x04 != 0)
            || (ask && eligible_mask & 0x08 != 0),
        past_limit: (bid && quote_state_mask & 0x02 != 0)
            || (ask && quote_state_mask & 0x04 != 0),
        pre_open: (bid || ask) && quote_state_mask & 0x01 != 0,
    }
}

/// One reading of an extra series the caller asked for.
///
/// The series ride on the same subscription as the prices and arrive on
/// records of their own, so a reading is queued rather than written into the
/// quote: a quote holds one value per field and these are not fields of a
/// quote. Which of the caller's four tick callbacks carries it is part of what
/// the venue's own record says, so it travels with the value.
#[derive(Debug, Clone)]
pub struct SeriesTick {
    /// The contract it is about.
    pub instrument: InstrumentId,
    /// The number the reference client reports it under.
    pub tick_type: i32,
    /// The reading itself, and the callback that carries it.
    pub value: SeriesValue,
}

/// What an extra series states, and therefore how it reaches a caller.
#[derive(Debug, Clone)]
pub enum SeriesValue {
    /// A price, on `tick_price`.
    Price(f64),
    /// A size, on `tick_size`.
    Size(f64),
    /// A number that is neither, on `tick_generic`.
    Generic(f64),
    /// Text, on `tick_string`.
    Text(String),
}

/// A historical tick (midpoint).
#[derive(Debug, Clone)]
pub struct HistoricalTickMidpoint {
    /// When.
    pub time: String,
    /// The midpoint then.
    pub price: f64,
}

/// A historical tick (last trade).
#[derive(Debug, Clone)]
pub struct HistoricalTickLast {
    /// When.
    pub time: String,
    /// What it traded at.
    pub price: f64,
    /// How much.
    ///
    /// A historical size crosses as text rather than as a number, because a
    /// size can be a fraction of a share. Read as a whole number, `0.5` was
    /// no size at all. Held as a decimal, like the price beside it.
    ///
    /// A historical size crosses as text rather than as a number, because a
    /// size can be a fraction of a share. Read as a whole number, `0.5` was
    /// no size at all.
    pub size: f64,
    /// Which venue.
    pub exchange: String,
    /// What the venue notes about it.
    pub special_conditions: String,
    /// Whether the venue marked the print as past the limit it states for the
    /// contract.
    pub past_limit: bool,
    /// And whether it marked it as one the tape does not carry.
    pub unreported: bool,
}

/// A historical tick (bid/ask).
#[derive(Debug, Clone)]
pub struct HistoricalTickBidAsk {
    /// When.
    pub time: String,
    /// The bid then.
    pub bid_price: f64,
    /// The ask.
    pub ask_price: f64,
    /// How much at the bid.
    pub bid_size: f64,
    /// How much at the ask.
    pub ask_size: f64,
    /// Whether the venue marked the bid as past the low it states for the
    /// contract.
    pub bid_past_low: bool,
    /// And the ask as past the high.
    pub ask_past_high: bool,
}

/// Historical tick data (one of three types based on whatToShow).
#[derive(Debug, Clone)]
pub enum HistoricalTickData {
    /// Midpoints.
    Midpoint(Vec<HistoricalTickMidpoint>),
    /// Trades.
    Last(Vec<HistoricalTickLast>),
    /// Quotes.
    BidAsk(Vec<HistoricalTickBidAsk>),
}

/// A real-time 5-second bar.
#[derive(Debug, Clone, Copy, Default)]
pub struct RealTimeBar {
    /// When the bar opened.
    pub timestamp: u32,
    /// Its first price.
    pub open: f64,
    /// Its highest.
    pub high: f64,
    /// Its lowest.
    pub low: f64,
    /// Its last.
    pub close: f64,
    /// How much traded in it.
    pub volume: f64,
    /// The volume-weighted average price.
    pub wap: f64,
    /// How many trades made it.
    pub count: i32,
}

/// A single trading session from a historical schedule response.
#[derive(Debug, Clone)]
pub struct ScheduleSession {
    /// The day it belongs to.
    pub ref_date: String,
    /// When the session opened.
    pub open_time: String,
    /// When it closed.
    pub close_time: String,
}

/// Parsed historical schedule response from historical data connection.
#[derive(Debug, Clone)]
pub struct HistoricalScheduleResponse {
    /// The name this client gave the query, which the answer echoes.
    pub query_id: String,
    /// The zone the times are stated in.
    pub timezone: String,
    /// The start of the window asked for.
    pub start_date_time: String,
    /// The end of the window asked for. Empty means now.
    pub end_date_time: String,
    /// Each session in the window.
    pub sessions: Vec<ScheduleSession>,
}

impl From<&crate::types::model::Contract> for ContractRef {
    /// Take what identifies the contract, and leave the rest.
    ///
    /// A caller's contract carries more than this — a primary exchange, a
    /// trading class, the fields a lookup filters on. Those travel separately
    /// where they are needed, because a request that filters is a different
    /// thing from a request that names.
    fn from(c: &crate::types::model::Contract) -> Self {
        Self {
            con_id: c.con_id,
            symbol: c.symbol.clone(),
            sec_type: c.sec_type.clone(),
            exchange: c.exchange.clone(),
            currency: c.currency.clone(),
            last_trade_date: c.last_trade_date_or_contract_month.clone(),
            strike: c.strike,
            right: c.right.clone(),
            multiplier: c.multiplier.clone(),
        }
    }
}

/// Account-level state.
#[derive(Debug, Clone, Copy, Default)]
pub struct AccountState {
    /// What the account is worth if everything is closed now.
    pub net_liquidation: Price,
    /// What it can still buy.
    pub buying_power: Price,
    /// What its positions require.
    pub margin_used: Price,
    /// What its positions have made and not realised.
    pub unrealized_pnl: Price,
    /// What it has realised.
    pub realized_pnl: Price,
    /// Cash, before settlement.
    pub total_cash_value: Price,
    /// Cash that has settled.
    pub settled_cash: Price,
    /// Interest and dividends accrued and not paid.
    pub accrued_cash: Price,
    /// What the account is worth counting borrowings.
    pub equity_with_loan: Price,
    /// What its positions are worth, long and short added.
    pub gross_position_value: Price,
    /// What opening its positions required.
    pub init_margin_req: Price,
    /// What holding them requires.
    pub maint_margin_req: Price,
    /// What it may still commit.
    pub available_funds: Price,
    /// What it holds above its maintenance requirement.
    pub excess_liquidity: Price,
    /// How much of that is left, as a fraction scaled by `PRICE_SCALE`.
    pub cushion: Price,        // percentage * PRICE_SCALE (e.g. 0.45 = 45%)
    /// Its special memorandum account.
    pub sma: Price,
    /// How many day trades it may still make. -1 means unlimited.
    pub day_trades_remaining: i64,
    /// How much it is levered, scaled by `PRICE_SCALE`.
    pub leverage: Price,       // ratio * PRICE_SCALE
    /// What it has made today.
    pub daily_pnl: Price,
}

/// Position with average cost, for P&L computation and reqPositions.
#[derive(Debug, Clone, Default)]
pub struct PositionInfo {
    /// The contract held.
    pub con_id: i64,
    /// The holding exactly as the account states it. Fractional: a holding of
    /// half a share is a holding, and rounding it to a whole number reported
    /// it as flat.
    pub position: f64,
    /// What it cost on average.
    pub avg_cost: Price,      // per-share avg cost * PRICE_SCALE
    /// Its ticker.
    pub symbol: String,
    /// What kind of contract it is.
    pub sec_type: String,
    /// What it is priced in.
    pub currency: String,
    /// How many units one contract is worth.
    pub multiplier: String,
    // Per-position marks from the account-updates snapshot.
    // Set only by the portfolio-value message, not the lean position feed.
    /// What it is worth now, each.
    pub market_price: Price,     // per-share mark * PRICE_SCALE
    /// What the holding is worth.
    pub market_value: Price,     // position mark * PRICE_SCALE
    /// What it has made and not realised.
    pub unrealized_pnl: Price,   // * PRICE_SCALE
    /// Whether the venue has stated that figure.
    ///
    /// Zero is one of the figures it states — a position marked at what it
    /// cost has made nothing — so the number alone cannot say whether it was
    /// given. Read as unstated, a stated zero fell through to one worked out
    /// here from the last print, which differs from the venue's by whatever
    /// the mark and the last print differ by.
    pub unrealized_stated: bool,
    /// What has been realised on it.
    pub realized_pnl: Price,     // * PRICE_SCALE
}

/// Per-position midnight seed from 6040=143 P&L subscription.
/// Used for client-side daily P&L computation.
#[derive(Debug, Clone, Copy, Default)]
pub struct MidnightSeed {
    /// The venue's id for the contract.
    pub con_id: i64,
    /// Position held at midnight. `None` when the row arrived without a
    /// parseable quantity: the position exists but its overnight size is
    /// unknown, which is not the same as having opened it today.
    pub qty_midnight: Option<f64>,
    /// What the venue states the position was worth at midnight. `None` where
    /// the row did not state it, which is when the day's change has to be
    /// sized against a previous close the client finds for itself.
    pub cost_midnight: Option<f64>,
    /// Quantity traded since midnight, as the venue states it.
    pub qty_traded: Option<f64>,
    /// Net cash from today's fills, signed.
    pub money_traded: f64,            // net cash from today's fills (signed)
    /// What it has realised.
    pub realized_pnl: f64,           // realized P&L since midnight
}

#[cfg(test)]
mod tests;
