//! ibapi-compatible types: Contract, Order, OrderState, Execution, TagValue, BarData,
//! ContractDetails, ContractDescription, and order conditions.
//!
//! These are plain Rust structs (no PyO3) shared by both the Rust EClient and the Python bridge.

use crate::types::*;

/// What a fixed-point price is divided by to reach money.
pub const PRICE_SCALE_F: f64 = PRICE_SCALE as f64;

// ── ComboLeg ──

/// ibapi-compatible ComboLeg for combination orders.
#[derive(Clone, Debug, Default)]
pub struct ComboLeg {
    /// The venue's id for this leg's contract.
    pub con_id: i64,
    /// How many of this leg go with one of the combination.
    pub ratio: i32,
    /// `BUY` or `SELL`, for this leg rather than the combination.
    pub action: String,
    /// Where this leg is to be filled.
    pub exchange: String,
    /// Whether this leg opens a position or closes one: 0 same as the
    /// parent, 1 open, 2 close, 3 either.
    pub open_close: i32,
    /// For a short leg, who is lending: 0 unspecified, 1 the account
    /// itself, 2 the venue.
    pub shorting_policy: i32,
    /// Where a short leg's borrow is located. Required by some
    /// venues for a short sale and empty otherwise.
    pub designated_location: String,
    /// -1 unless this leg is exempt from the short-sale price test.
    pub exempt_code: i32,
}

// ── DeltaNeutralContract ──

/// ibapi-compatible DeltaNeutralContract for delta-neutral orders.
#[derive(Clone, Debug, Default)]
pub struct DeltaNeutralContract {
    /// The contract the hedge is placed in.
    pub con_id: i64,
    /// How much of the hedge goes with one of the order.
    pub delta: f64,
    /// The price the hedge is struck at.
    pub price: f64,
}

// ── Contract ──

/// ibapi-compatible Contract. Matches C++ `Contract` struct fields.
#[derive(Clone, Debug, Default)]
pub struct Contract {
    /// Contract id assigned by the venue, and the only field that
    /// names one on its own. Every request that carries a contract is answered
    /// by id; a contract described by symbol has its id looked up first.
    pub con_id: i64,
    /// The ticker, as the venue lists it.
    pub symbol: String,
    /// What kind of contract: `STK`, `OPT`, `FUT`, `CASH`, `IND`,
    /// `CRYPTO`, `BAG` for a combination, and the rest the venue names.
    pub sec_type: String,
    /// Where an order on it is to be filled, or where a subscription is
    /// to be taken from. `SMART` lets the venue route.
    pub exchange: String,
    /// What it is priced in. The same symbol on the same venue exists in
    /// more than one currency, so this is part of what names the contract.
    pub currency: String,
    /// An option's expiry or a future's month,
    /// `YYYYMMDD` or `YYYYMM`. Empty for anything a symbol names completely.
    pub last_trade_date_or_contract_month: String,
    /// An option's strike. Zero for anything that has none.
    pub strike: f64,
    /// `C` or `P` for an option, empty otherwise.
    pub right: String,
    /// How many units one contract is worth — 100 for most US options.
    pub multiplier: String,
    /// The venue's name for this contract, which is not the
    /// symbol for anything but a share.
    pub local_symbol: String,
    /// Where the contract is listed, as opposed to where an
    /// order on it may be routed. What decides which venues its book is
    /// gathered from.
    pub primary_exchange: String,
    /// Which class within a symbol's chain this contract belongs to.
    pub trading_class: String,
    /// The last day this contract trades, where the venue states one
    /// separately from the expiry.
    pub last_trade_date: String,
    /// Whether a lookup may return a contract that has expired.
    /// Asking for one without this is answered with nothing.
    pub include_expired: bool,
    /// Which identifier `sec_id` is: `ISIN`, `CUSIP`.
    pub sec_id_type: String,
    /// The identifier itself. A lookup carrying one rides the
    /// identifier and ignores the symbol.
    pub sec_id: String,
    /// The venue's description of the contract.
    pub description: String,
    /// Who issued it, where the venue names an issuer.
    pub issuer_id: String,
    /// The venue's description of the legs, as it states
    /// them back.
    pub combo_legs_descrip: String,
    /// The legs of a combination. Empty for anything else.
    pub combo_legs: Vec<ComboLeg>,
    /// The hedge that goes with a delta-neutral order.
    pub delta_neutral_contract: Option<DeltaNeutralContract>,
}

// ── Order ──

/// ibapi-compatible Order. Matches C++ `Order` struct fields.
#[derive(Clone, Debug)]
pub struct Order {
    /// The caller's own number for this order. Every later message about
    /// it names this, and a modification is the same number placed again.
    ///
    /// **Reported by the venue, not sent.** It arrives on the echo of
    /// an order and on the reports that follow it, and an order going
    /// out carries nothing here — placing states the number in the
    /// call. Held so an order read back from the venue and placed
    /// again is the same value it was, which is why it is taken rather
    /// than refused.
    pub order_id: i64,
    /// `BUY` or `SELL`. `SSHORT` where the venue takes one.
    pub action: String,
    /// How much to trade, in the units the venue counts that contract
    /// in.
    pub total_quantity: f64,
    /// What kind of order: `MKT`, `LMT`, `STP`, `STP LMT`, `TRAIL`,
    /// `MIT`, `LIT`, `REL`, `VWAP` and the rest the venue takes.
    pub order_type: String,
    /// The limit. What a limit order will not pay above, or accept below.
    pub lmt_price: f64,
    /// The order's other price, which differs by kind: a stop's trigger,
    /// a trailing order's amount, a relative order's offset. A stop read from
    /// `lmt_price` becomes a limit at zero.
    pub aux_price: f64,
    /// How long it lives: `DAY`, `GTC`, `IOC`, `FOK`, `GTD`, `OPG`, `AUC`.
    pub tif: String,
    /// Whether it may fill outside regular trading hours.
    pub outside_rth: bool,
    /// How much of an iceberg is shown at once.
    pub display_size: i32,
    /// The smallest fill that will be accepted.
    pub min_qty: i32,
    /// Whether the order is kept off the book entirely.
    pub hidden: bool,
    /// When the order should become active: `YYYYMMDD HH:MM:SS`, with an
    /// optional zone, or `YYYYMMDD` for the start of that day.
    ///
    /// Sent on tag 168, in UTC and joined by a dash — the form this tag
    /// writes it in, which is not the space-joined form the rest of this
    /// client's timestamps use. Written the other way the venue reads a
    /// different moment, and the order goes live at a time nobody chose.
    pub good_after_time: String,
    /// When a `GTD` order expires, in the same form.
    pub good_till_date: String,
    /// Which one-cancels-all group this belongs to. A fill on one
    /// withdraws the rest.
    pub oca_group: String,
    /// How far a trailing stop follows, as a percentage rather than
    /// an amount.
    pub trailing_percent: f64,
    /// Which algorithm runs the order, as the venue names it.
    pub algo_strategy: String,
    /// What that algorithm was given.
    pub algo_params: Vec<TagValue>,
    /// Ask what the order would cost without placing it. Answered on
    /// `open_order` with the margin and commission it would take, and no status,
    /// because a preview is not an order.
    pub what_if: bool,
    /// Size the order by money rather than by quantity.
    pub cash_qty: f64,
    /// The order this one is a child of. A bracket's children name their
    /// parent, and the venue holds them until it fills.
    pub parent_id: i64,
    /// Whether the order goes to the venue now.
    ///
    /// **Held here.** No tag carries it: the reference client writes it into
    /// its own message, between the order reference and the parent id, and it
    /// does not reach the venue. Its own words for a false value are that the
    /// order is created and not transmitted.
    ///
    /// So it is held here instead. Nothing is sent until an order that
    /// transmits releases it — this one placed again, or another in the same
    /// family. The reference client's own bracket sample places a parent and a
    /// take-profit held back and lets the stop-loss send all three.
    ///
    /// A held order never reached the venue, so it goes when the session
    /// does.
    pub transmit: bool,
    /// How far past the limit the order may reach, unshown.
    pub discretionary_amt: f64,
    /// Take liquidity from several venues at once rather than
    /// resting.
    pub sweep_to_fill: bool,
    /// Fill in full or not at all.
    pub all_or_none: bool,
    /// What price trips a stop: 0 the venue's default, 1 double bid or
    /// ask, 2 last, 3 double last, 4 bid or ask, 7 last or bid or ask, 8
    /// midpoint.
    pub trigger_method: i32,
    /// What this order becomes once its trigger is reached.
    pub adjusted_order_type: String,
    /// The price at which that change happens.
    pub trigger_price: f64,
    /// The stop the order takes on when it changes.
    pub adjusted_stop_price: f64,
    /// The limit it takes on with it.
    pub adjusted_stop_limit_price: f64,
    /// What has to be true before the order is placed — a price, a volume,
    /// a percentage change, a margin level, another execution, a time. All six
    /// kinds are held by the venue.
    pub conditions: Vec<OrderCondition>,
    /// Whether those conditions are measured outside regular
    /// hours too.
    pub conditions_ignore_rth: bool,
    /// Whether meeting them withdraws the order instead of
    /// placing it.
    pub conditions_cancel_order: bool,
    // ── ibapi-parity fields ──
    /// Which account to trade for. Every message states the session's own
    /// account, so an order naming another is refused rather than filled
    /// somewhere unintended.
    pub account: String,
    /// When a conditional order starts being watched.
    pub active_start_time: String,
    /// When it stops.
    pub active_stop_time: String,
    /// Whether that amount is a price or a percentage.
    pub adjustable_trailing_unit: i32,
    /// How far the adjusted order trails.
    pub adjusted_trailing_amount: f64,
    /// A venue precaution the caller has chosen to accept.
    pub advanced_error_override: String,
    /// A caller's own name for the algo running this order. **Not carried by
    /// this protocol.** The venue refuses tag 8016: previewed with
    /// one, both on an algo and without, it answers `Invalid value in field #
    /// 8016`. Accepted and retained, so an order built against another client
    /// reads back what it set.
    pub algo_id: String,
    /// Whether the order may be worked before the venue opens.
    pub allow_pre_open: bool,
    /// Which auction an order competes in.
    ///
    /// **Not carried by this protocol.** No tag in the protocol carries it,
    /// though its siblings each have one. Accepted and retained, so an order
    /// built against another client reads back what it set.
    pub auction_strategy: i32,
    /// A date on which the venue withdraws the order itself.
    pub auto_cancel_date: String,
    /// Whether cancelling this child cancels its parent too.
    pub auto_cancel_parent: bool,
    /// An offset stated in basis points, and what it is measured against.
    ///
    /// **Not carried by this protocol.** No tag in the protocol carries it,
    /// though its siblings each have one. Accepted and retained, so an order
    /// built against another client reads back what it set.
    pub basis_points: f64,
    /// See `basis_points`.
    ///
    /// **Not carried by this protocol.** No tag in the protocol carries it,
    /// though its siblings each have one. Accepted and retained, so an order
    /// built against another client reads back what it set.
    pub basis_points_type: i32,
    /// A large order worked as a block.
    pub block_order: bool,
    /// Interest accrued on a bond since its last coupon.
    ///
    /// **Not carried by this protocol.** No tag in the protocol carries it,
    /// though its siblings each have one. Accepted and retained, so an order
    /// built against another client reads back what it set.
    pub bond_accrued_interest: String,
    /// Where the trade clears.
    pub clearing_account: String,
    /// How it clears: `IB`, `Away`, `PTA`.
    pub clearing_intent: String,
    /// Which client placed it.
    ///
    /// **Reported by the venue, not sent.** It arrives on the echo of
    /// an order and on the reports that follow it, and an order going
    /// out carries nothing here — placing states the number in the
    /// call. Held so an order read back from the venue and placed
    /// again is the same value it was, which is why it is taken rather
    /// than refused.
    pub client_id: i32,
    /// How far a pegged-to-best order may improve on the
    /// best price.
    pub compete_against_best_offset: f64,
    /// Whether a volatility order keeps repricing as the
    /// underlying moves.
    pub continuous_update: bool,
    /// The end customer an order is placed for.
    pub customer_account: String,
    /// Whether the order is held inactive.
    pub deactivate: bool,
    /// Stand the order down if the connection goes (tag 6661).
    pub deactivate_on_disconnect: bool,
    /// The hedge ratio a delta-neutral order is worked at.
    ///
    /// **Not carried by this protocol.** No tag carries it; the delta that
    /// travels is the hedging contract's, stated on `delta_neutral_contract`.
    /// Accepted and retained, so an order built against another client reads
    /// back what it set.
    pub delta: f64,
    /// The hedging leg's own trigger price.
    pub delta_neutral_aux_price: f64,
    /// Where the hedging leg clears.
    ///
    /// **Not carried by this protocol.** No tag in the protocol carries it,
    /// though its siblings each have one. Accepted and retained, so an order
    /// built against another client reads back what it set.
    pub delta_neutral_clearing_account: String,
    /// How the hedging leg clears.
    ///
    /// **Not carried by this protocol.** No tag in the protocol carries it,
    /// though its siblings each have one. Accepted and retained, so an order
    /// built against another client reads back what it set.
    pub delta_neutral_clearing_intent: String,
    /// The contract the hedge is placed in.
    pub delta_neutral_con_id: i32,
    /// Where the hedging leg's shares are held.
    ///
    /// **Not carried by this protocol.** No tag in the protocol carries it,
    /// though its siblings each have one. Accepted and retained, so an order
    /// built against another client reads back what it set.
    pub delta_neutral_designated_location: String,
    /// Whether the hedging leg opens or closes a position.
    ///
    /// **Not carried by this protocol.** No tag in the protocol carries it, on any
    /// path. Accepted and retained, so an order built against another client
    /// still reads back what it set.
    pub delta_neutral_open_close: String,
    /// What kind of order the hedge is.
    pub delta_neutral_order_type: String,
    /// Who settles the hedging leg.
    ///
    /// **Not carried by this protocol.** No tag in the protocol carries it,
    /// though its siblings each have one. Accepted and retained, so an order
    /// built against another client reads back what it set.
    pub delta_neutral_settling_firm: String,
    /// Whether the hedging leg is a short sale.
    ///
    /// **Not carried by this protocol.** No tag in the protocol carries it,
    /// though its siblings each have one. Accepted and retained, so an order
    /// built against another client reads back what it set.
    pub delta_neutral_short_sale: bool,
    /// Which short-sale slot the hedging leg uses.
    ///
    /// **Not carried by this protocol.** No tag in the protocol carries it,
    /// though its siblings each have one. Accepted and retained, so an order
    /// built against another client reads back what it set.
    pub delta_neutral_short_sale_slot: i32,
    /// Where a short sale's borrow is located.
    pub designated_location: String,
    /// Whether that discretion is measured to the limit
    /// rather than from it.
    pub discretionary_up_to_limit_price: bool,
    /// Whether the hedge is priced automatically.
    ///
    /// **Not carried by this protocol.** No tag in the protocol carries it,
    /// though its siblings each have one. Accepted and retained, so an order
    /// built against another client reads back what it set.
    ///
    /// False when nothing states it, as the reference client leaves it. True
    /// stood here, and the refusal that guards an uncarried field reads a value
    /// away from the default as one the caller asked for: an order that spelled
    /// out the reference client's own default was refused for it, and one that
    /// actually asked for the hedge not to be priced automatically was accepted
    /// and had that instruction dropped. The polarity was the whole of the
    /// fault.
    pub dont_use_auto_price_for_hedge: bool,
    /// How long a duration-limited order lives, in seconds.
    pub duration: i32,
    /// -1 unless the order is exempt from the short-sale price test.
    pub exempt_code: i32,
    /// Who at the firm is operating the order.
    pub ext_operator: String,
    /// Which advisor group the order is allocated across, on tag 6160.
    ///
    /// It rides the order itself, in the section a replace restates, so the
    /// allocation follows an order that is moved. The reports coming back name
    /// the same three tags, and this client read them there while sending none
    /// of them: an order that asked to be spread across a group filled whole on
    /// the connected account.
    pub fa_group: String,
    /// How it is divided among them, on tag 6159.
    pub fa_method: String,
    /// What share each takes, where the method is a percentage, on tag 6164.
    pub fa_percentage: String,
    /// How much has filled, as the venue states it back.
    ///
    /// **Reported by the venue, not sent.** It arrives on the echo of
    /// an order and on the reports that follow it, and an order going
    /// out carries nothing here — placing states the number in the
    /// call. Held so an order read back from the venue and placed
    /// again is the same value it was, which is why it is taken rather
    /// than refused.
    pub filled_quantity: f64,
    /// What the hedge is measured by.
    pub hedge_param: String,
    /// What kind of hedge goes with the order: delta, beta, FX, pair.
    pub hedge_type: String,
    /// Whether the order stays out of the opening auction.
    pub ignore_open_auction: bool,
    /// Only fill against an auction imbalance.
    pub imbalance_only: bool,
    /// Whether the order is worked in the overnight session.
    pub include_overnight: bool,
    /// Whether the order is a container held by an order management
    /// system.
    pub is_oms_container: bool,
    /// Whether a pegged order's reference moving down
    /// moves it down too.
    pub is_pegged_change_amount_decrease: bool,
    /// How far a pegged order's limit sits from its reference.
    pub lmt_price_offset: f64,
    /// Whether a person entered the order rather than a
    /// program.
    pub manual_order_indicator: i32,
    /// When they did.
    pub manual_order_time: String,
    /// How far a midpoint order sits from the midpoint at half
    /// the spread.
    pub mid_offset_at_half: f64,
    /// And at the whole spread.
    pub mid_offset_at_whole: f64,
    /// Under MiFID II, which algorithm decided to trade.
    pub mifid2_decision_algo: String,
    /// Which person did.
    pub mifid2_decision_maker: String,
    /// Which algorithm executed it.
    pub mifid2_execution_algo: String,
    /// Which person did.
    pub mifid2_execution_trader: String,
    /// The smallest size it will compete against.
    pub min_compete_size: i32,
    /// The smallest quantity the order will trade in.
    pub min_trade_qty: i32,
    /// Which model the order belongs to.
    ///
    /// Stated on tag 6700, beside the account: the account names where the
    /// order goes and this names which sleeve of it. Left empty the order is
    /// placed against the account at large, which for a model portfolio is a
    /// different position from the one asked for.
    pub model_code: String,
    /// Whether the venue may use its discretion over the order.
    pub not_held: bool,
    /// What happens to the rest when one fills: 1 cancel and reduce
    /// nothing, 2 reduce with block, 3 reduce without.
    pub oca_type: i32,
    /// Whether the order opens a position or closes one.
    pub open_close: String,
    /// Whether smart routing is declined.
    ///
    /// **Not carried by this protocol.** No tag in the protocol carries it,
    /// though its siblings each have one. Accepted and retained, so an order
    /// built against another client reads back what it set.
    pub opt_out_smart_routing: bool,
    /// A price for each leg of a combination, in the order the legs
    /// are given. The venue validates the leg order and refuses a spread it
    /// reads as inverted.
    pub order_combo_legs: Vec<f64>,
    /// Free-form options carried alongside an order.
    ///
    /// **Not carried by this protocol.** No tag in the protocol carries it,
    /// though its siblings each have one. Accepted and retained, so an order
    /// built against another client reads back what it set.
    pub order_misc_options: Vec<TagValue>,
    /// The caller's own label, carried back on every message about the
    /// order.
    pub order_ref: String,
    /// Who originated the order.
    ///
    /// **Not carried by this protocol.** No tag in the protocol carries it,
    /// though its siblings each have one. Accepted and retained, so an order
    /// built against another client reads back what it set.
    pub origin: i32,
    /// Whether percentage limits are set aside.
    ///
    /// **Not carried by this protocol.** No tag in the protocol carries it,
    /// though its siblings each have one. Accepted and retained, so an order
    /// built against another client reads back what it set.
    pub override_percentage_constraints: bool,
    /// Parent order id, as the venue assigns it.
    ///
    /// **Not carried by this protocol.** No tag in the protocol carries it,
    /// though its siblings each have one. Accepted and retained, so an order
    /// built against another client reads back what it set.
    pub parent_perm_id: i64,
    /// How far it moves when the reference does.
    pub pegged_change_amount: f64,
    /// How far a relative order sits from its reference, as a
    /// percentage.
    pub percent_offset: f64,
    /// Order id assigned by the venue, stable across sessions where the
    /// caller's number is not.
    ///
    /// **Reported by the venue, not sent.** It arrives on the echo of
    /// an order and on the reports that follow it, and an order going
    /// out carries nothing here — placing states the number in the
    /// call. Held so an order read back from the venue and placed
    /// again is the same value it was, which is why it is taken rather
    /// than refused.
    pub perm_id: i64,
    /// Add liquidity or do not fill at all.
    pub post_only: bool,
    /// How long to rest on an alternative trading system before
    /// routing.
    pub post_to_ats: i32,
    /// Whether that customer is a professional, which the
    /// venture prices differently.
    pub professional_customer: bool,
    /// The profit-taking leg's id.
    ///
    /// **Not carried by this protocol.** No tag in the protocol carries it,
    /// though its siblings each have one. Accepted and retained, so an order
    /// built against another client reads back what it set.
    pub pt_order_id: i32,
    /// The profit-taking leg's type.
    ///
    /// **Not carried by this protocol.** No tag in the protocol carries it,
    /// though its siblings each have one. Accepted and retained, so an order
    /// built against another client reads back what it set.
    pub pt_order_type: String,
    /// Whether a ladder's prices are varied.
    ///
    /// **Not carried by this protocol.** No tag in the protocol carries it,
    /// though its siblings each have one. Accepted and retained, so an order
    /// built against another client reads back what it set.
    pub randomize_price: bool,
    /// Vary the displayed size so the order is harder to read.
    pub randomize_size: bool,
    /// Which future a volatility order prices against.
    pub ref_futures_con_id: i32,
    /// How far the reference has to move first.
    pub reference_change_amount: f64,
    /// Which contract it is pegged to.
    pub reference_contract_id: i32,
    /// Which venue's price of it to use.
    pub reference_exchange_id: String,
    /// Which of that venue's prices: 1 the midpoint, 2 the bid
    /// or ask.
    pub reference_price_type: i32,
    /// Send a marketable order to the best bid or offer
    /// rather than working it. `None` until stated; written
    /// only when true, as the wire carries it.
    pub route_marketable_to_bbo: Option<bool>,
    /// What kind of trader this is, under Rule 80A.
    pub rule80a: String,
    /// Whether the ladder starts again once it is worked through.
    pub scale_auto_reset: bool,
    /// How much of a ladder's first component is already filled. **Not carried
    /// by this protocol.** The venue answers `Can not contain field # 6486` —
    /// not a bad value but a field that does not belong on an order. The
    /// position a ladder starts against, beside it, is taken.
    pub scale_init_fill_qty: i32,
    /// How much the first level of a scale order trades.
    pub scale_init_level_size: i32,
    /// The position the ladder starts from.
    pub scale_init_position: i32,
    /// How often they adjust, in seconds.
    pub scale_price_adjust_interval: i32,
    /// How far the levels move when they adjust.
    pub scale_price_adjust_value: f64,
    /// How far apart the levels are priced.
    pub scale_price_increment: f64,
    /// How far past each level the profit-taking order sits.
    pub scale_profit_offset: f64,
    /// Whether the level sizes are varied to be harder to read.
    pub scale_random_percent: bool,
    /// How much each level after it trades.
    pub scale_subs_level_size: i32,
    /// The name of a scale table held by the venue.
    ///
    /// **Not carried by this protocol.** A named table is resolved into the ladder
    /// it stands for and the levels are sent, so the name never reaches the
    /// venue. Setting the ladder's own fields has the same effect.
    pub scale_table: String,
    /// Whether the venue may seek a better price than the
    /// limit. `None` until stated; written only when true, as
    /// the wire carries it.
    pub seek_price_improvement: Option<bool>,
    /// Which firm settles the trade.
    pub settling_firm: String,
    /// The shareholder an order is placed for.
    ///
    /// **Not carried by this protocol.** No tag in the protocol carries it,
    /// though its siblings each have one. Accepted and retained, so an order
    /// built against another client reads back what it set.
    pub shareholder: String,
    /// Who is lending for a short sale: 1 the account, 2 elsewhere,
    /// which is what `designated_location` then names.
    pub short_sale_slot: i32,
    /// The stop-loss leg's id.
    ///
    /// **Not carried by this protocol.** No tag in the protocol carries it,
    /// though its siblings each have one. Accepted and retained, so an order
    /// built against another client reads back what it set.
    pub sl_order_id: i32,
    /// The stop-loss leg's type.
    ///
    /// **Not carried by this protocol.** No tag in the protocol carries it,
    /// though its siblings each have one. Accepted and retained, so an order
    /// built against another client reads back what it set.
    pub sl_order_type: String,
    /// Routing parameters for a smart-routed combination.
    ///
    /// **Not carried by this protocol.** No tag in the protocol carries it,
    /// though its siblings each have one. Accepted and retained, so an order
    /// built against another client reads back what it set.
    pub smart_combo_routing_params: Vec<TagValue>,
    /// Which soft dollar tier the commission is directed to.
    pub soft_dollar_tier_name: String,
    /// What that tier is worth.
    pub soft_dollar_tier_val: String,
    /// What the soft-dollar tier is called on a screen.
    ///
    /// Reported by the venue and not sent: it states all three when it lists
    /// the tiers an account has, and the tier is named to the wire by the
    /// other two. The reference client sends those two and no more either.
    ///
    /// So it is a label rather than an instruction, and a caller who lists the
    /// tiers, picks one and hands the whole thing to an order has asked for
    /// nothing by carrying it. Refused for being stated, as a field nothing
    /// carries would be, that ordinary sequence could not be written without
    /// first blanking something the venue itself filled in.
    pub soft_dollar_tier_display_name: String,
    /// Whether the order was solicited from the customer.
    pub solicited: bool,
    /// Where a scale order starts.
    pub starting_price: f64,
    /// The lowest underlying price a volatility order stays active
    /// through.
    pub stock_range_lower: f64,
    /// And the highest.
    pub stock_range_upper: f64,
    /// The underlying price a volatility order is priced against.
    pub stock_ref_price: f64,
    /// Who submitted it.
    ///
    /// **Reported by the venue, not sent.** It arrives on the echo of an order
    /// and on the reports that follow it, and an order going out carries
    /// nothing here. Held so an order read back from the venue and placed
    /// again is the same value it was, which is why it is taken rather than
    /// refused — every report supplies it, so refusing it would refuse every
    /// order anyone read back.
    pub submitter: String,
    /// Where a trailing stop starts, before it has followed
    /// anything.
    pub trail_stop_price: f64,
    /// Whether the venue's price management algorithm works
    /// the order. `None` until stated; written only when above
    /// nought, as the wire carries it.
    pub use_price_mgmt_algo: Option<i32>,
    /// The volatility a volatility order is worked at, as the number of
    /// percent: 25 is a quarter. Carried to the wire as it stands.
    pub volatility: f64,
    /// Whether that volatility is daily or annual: 1 daily, 2
    /// annual.
    pub volatility_type: i32,
    /// Which kind of preview is being asked for.
    ///
    /// **Not carried by this protocol.** No tag in the protocol carries it,
    /// though its siblings each have one. Accepted and retained, so an order
    /// built against another client reads back what it set.
    pub what_if_type: i32,
}

impl Default for Order {
    fn default() -> Self {
        Self {
            order_id: 0,
            action: String::new(),
            total_quantity: 0.0,
            order_type: String::new(),
            lmt_price: 0.0,
            aux_price: 0.0,
            tif: String::new(),
            outside_rth: false,
            display_size: 0,
            min_qty: 0,
            hidden: false,
            good_after_time: String::new(),
            good_till_date: String::new(),
            oca_group: String::new(),
            trailing_percent: 0.0,
            algo_strategy: String::new(),
            algo_params: Vec::new(),
            what_if: false,
            cash_qty: 0.0,
            parent_id: 0,
            transmit: true,
            discretionary_amt: 0.0,
            sweep_to_fill: false,
            all_or_none: false,
            trigger_method: 0,
            adjusted_order_type: String::new(),
            trigger_price: 0.0,
            adjusted_stop_price: 0.0,
            adjusted_stop_limit_price: 0.0,
            conditions: Vec::new(),
            conditions_ignore_rth: false,
            conditions_cancel_order: false,
            // ibapi-parity defaults
            account: String::new(),
            active_start_time: String::new(),
            active_stop_time: String::new(),
            adjustable_trailing_unit: 0,
            adjusted_trailing_amount: f64::MAX,
            advanced_error_override: String::new(),
            algo_id: String::new(),
            allow_pre_open: false,
            auction_strategy: 0,
            auto_cancel_date: String::new(),
            auto_cancel_parent: false,
            basis_points: f64::MAX,
            basis_points_type: i32::MAX,
            block_order: false,
            bond_accrued_interest: String::new(),
            clearing_account: String::new(),
            clearing_intent: String::new(),
            client_id: 0,
            compete_against_best_offset: f64::MAX,
            continuous_update: false,
            customer_account: String::new(),
            deactivate: false,
            deactivate_on_disconnect: false,
            delta: f64::MAX,
            delta_neutral_aux_price: f64::MAX,
            delta_neutral_clearing_account: String::new(),
            delta_neutral_clearing_intent: String::new(),
            delta_neutral_con_id: 0,
            delta_neutral_designated_location: String::new(),
            delta_neutral_open_close: String::new(),
            delta_neutral_order_type: String::new(),
            delta_neutral_settling_firm: String::new(),
            delta_neutral_short_sale: false,
            delta_neutral_short_sale_slot: 0,
            designated_location: String::new(),
            discretionary_up_to_limit_price: false,
            dont_use_auto_price_for_hedge: false,
            duration: i32::MAX,
            exempt_code: -1,
            ext_operator: String::new(),
            fa_group: String::new(),
            fa_method: String::new(),
            fa_percentage: String::new(),
            filled_quantity: 0.0,
            hedge_param: String::new(),
            hedge_type: String::new(),
            ignore_open_auction: false,
            imbalance_only: false,
            include_overnight: false,
            is_oms_container: false,
            is_pegged_change_amount_decrease: false,
            lmt_price_offset: f64::MAX,
            manual_order_indicator: i32::MAX,
            manual_order_time: String::new(),
            mid_offset_at_half: f64::MAX,
            mid_offset_at_whole: f64::MAX,
            mifid2_decision_algo: String::new(),
            mifid2_decision_maker: String::new(),
            mifid2_execution_algo: String::new(),
            mifid2_execution_trader: String::new(),
            min_compete_size: i32::MAX,
            min_trade_qty: i32::MAX,
            model_code: String::new(),
            not_held: false,
            oca_type: 0,
            open_close: String::new(),
            opt_out_smart_routing: false,
            order_combo_legs: Vec::new(),
            order_misc_options: Vec::new(),
            order_ref: String::new(),
            origin: 0,
            override_percentage_constraints: false,
            parent_perm_id: 0,
            pegged_change_amount: 0.0,
            percent_offset: f64::MAX,
            perm_id: 0,
            post_only: false,
            post_to_ats: i32::MAX,
            professional_customer: false,
            pt_order_id: i32::MAX,
            pt_order_type: String::new(),
            randomize_price: false,
            randomize_size: false,
            ref_futures_con_id: 0,
            reference_change_amount: 0.0,
            reference_contract_id: 0,
            reference_exchange_id: String::new(),
            reference_price_type: 0,
            route_marketable_to_bbo: None,
            rule80a: String::new(),
            scale_auto_reset: false,
            scale_init_fill_qty: i32::MAX,
            scale_init_level_size: i32::MAX,
            scale_init_position: i32::MAX,
            scale_price_adjust_interval: i32::MAX,
            scale_price_adjust_value: f64::MAX,
            scale_price_increment: f64::MAX,
            scale_profit_offset: f64::MAX,
            scale_random_percent: false,
            scale_subs_level_size: i32::MAX,
            scale_table: String::new(),
            seek_price_improvement: None,
            settling_firm: String::new(),
            shareholder: String::new(),
            short_sale_slot: 0,
            sl_order_id: i32::MAX,
            sl_order_type: String::new(),
            smart_combo_routing_params: Vec::new(),
            soft_dollar_tier_name: String::new(),
            soft_dollar_tier_val: String::new(),
            soft_dollar_tier_display_name: String::new(),
            solicited: false,
            starting_price: f64::MAX,
            stock_range_lower: f64::MAX,
            stock_range_upper: f64::MAX,
            stock_ref_price: f64::MAX,
            submitter: String::new(),
            trail_stop_price: f64::MAX,
            use_price_mgmt_algo: None,
            volatility: f64::MAX,
            volatility_type: 0,
            what_if_type: i32::MAX,
        }
    }
}

impl Order {
    /// Parse the action string to Side.
    pub fn side(&self) -> Result<Side, String> {
        match self.action.to_uppercase().as_str() {
            "BUY" | "B" => Ok(Side::Buy),
            "SELL" | "S" => Ok(Side::Sell),
            "SSHORT" | "SS" => Ok(Side::ShortSell),
            _ => Err(format!("Invalid action '{}': use BUY or SELL", self.action)),
        }
    }

    /// Parse the TIF string to FIX byte.
    /// The order-type byte this order tracks under, or 0 when the type is one
    /// a replace cannot state.
    ///
    /// Only the types a modify is accepted for are mapped; everything else is
    /// refused before it reaches a replace, and 0 tells the encoder to keep
    /// whatever the resting order holds.
    pub fn ord_type_byte(&self) -> u8 {
        match self.order_type.to_uppercase().as_str() {
            "MKT" => b'1',
            "LMT" => b'2',
            "STP" => b'3',
            "STP LMT" => b'4',
            "MOC" => b'5',
            "LOC" => b'B',
            "MIT" => b'J',
            "MTL" | "BOX TOP" => b'K',
            "MKT PRT" => b'U',
            "STP PRT" => crate::types::ORD_STP_PRT,
            _ => 0,
        }
    }

    /// How long the order lives, as the single byte the wire carries it in.
    pub fn tif_byte(&self) -> u8 {
        match self.tif.as_str() {
            "GTC" => b'1',
            "IOC" => b'3',
            "FOK" => b'4',
            "OPG" => b'2',
            "GTD" => b'6',
            "GTX" => b'5',
            // Day-til-cancelled is the good-til-cancelled byte with the
            // stand-down flag on tag 6436 beside it, and the flag is the whole
            // of the difference between the two lives. Its own name on tag 59
            // is answered `Invalid value in field # 59`, and it shared the
            // good-til-date byte here instead — so the order carried an
            // expiry the caller never named, or, where no date was given, none
            // at all, and it did not stand down at the close either way.
            "DTC" => b'1',
            "AUC" => b'8',
            // A peg that lives by the minute. Refused here before, so a caller
            // could not ask for it at all; the venue answers it by name and
            // refuses only the pairing, which is a thing a caller can change.
            //
            // The two overnight lives are deliberately absent. They are named
            // in the same family, and tag 59 answers both with
            // `Invalid value in field # 59` — on the regular route and on the
            // overnight one — exactly as it answers day-til-cancelled's own
            // byte. Day-til-cancelled has a flag that states the rest of what
            // it means; nothing here states an overnight life beside a DAY
            // byte, and a name taken without one would be a plain DAY order
            // under an overnight name. A name for a life is not proof the
            // field carries it.
            "NMIN" => b'p',
            _ => b'0', // DAY
        }
    }

    /// Build OrderAttrs from Order fields.
    pub fn attrs(&self) -> OrderAttrs {
        // Parse the good-till expiry string into either a UTC instant (tag 126)
        // or a calendar date (tag 432). On a parse error, log and drop the
        // expiry — the order then surfaces a visible gateway rejection rather
        // than silently carrying a wrong expiry.
        // Not active until this moment. Tag 168 carries a timestamp, so a
        // date with no time is the start of that day.
        let good_after = match crate::protocol::datetime::parse_ib_expiry(&self.good_after_time) {
            Ok(None) | Err(_) => 0,
            Ok(Some(crate::protocol::datetime::IbExpiry::Instant(secs))) => secs,
            Ok(Some(crate::protocol::datetime::IbExpiry::DateOnly(ymd))) => {
                crate::protocol::datetime::ib_datetime_to_unix(&format!("{ymd:08} 00:00:00"))
                    .unwrap_or(0)
            }
        };
        let (good_till, good_till_date_ymd) =
            match crate::protocol::datetime::parse_ib_expiry(&self.good_till_date) {
                Ok(None) => (0, 0),
                Ok(Some(crate::protocol::datetime::IbExpiry::Instant(secs))) => (secs, 0),
                Ok(Some(crate::protocol::datetime::IbExpiry::DateOnly(ymd))) => (0, ymd),
                Err(e) => {
                    log::warn!("dropping good_till_date: {e}");
                    (0, 0)
                }
            };
        OrderAttrs {
            soft_dollar_tier_name: self.soft_dollar_tier_name.clone(),
            soft_dollar_tier_val: self.soft_dollar_tier_val.clone(),
            algo_id: self.algo_id.clone(),
            settling_firm: self.settling_firm.clone(),
            discretionary_up_to_limit: self.discretionary_up_to_limit_price,
            display_size: self.display_size.max(0) as u32,
            min_qty: self.min_qty.max(0) as u32,
            hidden: self.hidden,
            outside_rth: self.outside_rth,
            good_after,
            good_till,
            good_till_date_ymd,
            // The group as the caller names it, whatever it reads as. A name
            // that reads as a number was rewritten to the engine's own form
            // and read back under it; the venue holds such a name as named,
            // measured with two orders under "1234" on the paper account, and
            // the reference client sends it as named.
            oca_group: 0,
            oca_group_str: self.oca_group.clone(),
            parent_id: self.parent_id.max(0) as u64,
            discretionary_amt: crate::types::price_from_f64(self.discretionary_amt),
            sweep_to_fill: self.sweep_to_fill,
            all_or_none: self.all_or_none,
            // `f64::MAX` is this API's "not set" for a price-like field, and
            // it is not a volatility or an offset.
            volatility: if self.volatility == f64::MAX { 0.0 } else { self.volatility },
            volatility_type: self.volatility_type.clamp(0, 255) as u8,
            // Stated by a caller and carried nowhere until now: an order that
            // asked to be re-priced as the underlying moved, or to stay inside
            // a band of underlying prices, was accepted and sent without either.
            seek_price_improvement: self.seek_price_improvement.unwrap_or(false),
            manual_order_time: self.manual_order_time.clone(),
            advanced_error_override: self.advanced_error_override.clone(),
            active_start_time: self.active_start_time.clone(),
            active_stop_time: self.active_stop_time.clone(),
            post_only: self.post_only,
            solicited: self.solicited,
            manual_order_indicator: self.manual_order_indicator,
            route_marketable_to_bbo: self.route_marketable_to_bbo.unwrap_or(false),
            imbalance_only: self.imbalance_only,
            allow_pre_open: self.allow_pre_open,
            ignore_open_auction: self.ignore_open_auction,
            is_oms_container: self.is_oms_container,
            ext_operator: self.ext_operator.clone(),
            customer_account: self.customer_account.clone(),
            professional_customer: self.professional_customer,
            model_code: self.model_code.clone(),
            // Named as a life on the caller's order and carried as a flag
            // here, because that is how the wire states it.
            deactivate_at_close: self.tif == "DTC",
            fa_group: self.fa_group.clone(),
            fa_method: self.fa_method.clone(),
            fa_percentage: self.fa_percentage.clone(),
            ref_futures_con_id: self.ref_futures_con_id,
            mifid2_decision_maker: self.mifid2_decision_maker.clone(),
            mifid2_decision_algo: self.mifid2_decision_algo.clone(),
            mifid2_execution_trader: self.mifid2_execution_trader.clone(),
            mifid2_execution_algo: self.mifid2_execution_algo.clone(),
            mid_offset_at_whole: self.mid_offset_at_whole,
            mid_offset_at_half: self.mid_offset_at_half,
            use_price_mgmt_algo: self.use_price_mgmt_algo.unwrap_or(0),
            duration: self.duration,
            min_compete_size: if self.min_compete_size == i32::MAX { 0 } else { self.min_compete_size },
            compete_against_best_offset: self.compete_against_best_offset,
            continuous_update: self.continuous_update,
            reference_price_type: self.reference_price_type,
            stock_range_lower: self.stock_range_lower,
            stock_range_upper: self.stock_range_upper,
            percent_offset: self.percent_offset,
            not_held: self.not_held,
            order_ref: self.order_ref.clone(),
            open_close: self.open_close.clone(),
            scale: self.scale_attrs(),
            delta_neutral: self.delta_neutral_attrs(),
            short_sale_slot: self.short_sale_slot.clamp(0, 255) as u8,
            designated_location: self.designated_location.clone(),
            exempt_code: self.exempt_code,
            // The wire takes a number, not the API's letter.
            hedge_type: match self.hedge_type.to_ascii_uppercase().as_str() {
                "F" => 1, "D" => 2, "P" => 3, "B" => 4, "S" => 5, _ => 0,
            },
            hedge_beta: if self.hedge_type.eq_ignore_ascii_case("B") {
                self.hedge_param.parse().unwrap_or(0.0)
            } else { 0.0 },
            hedge_ratio: if self.hedge_type.eq_ignore_ascii_case("P") {
                self.hedge_param.parse().unwrap_or(0.0)
            } else { 0.0 },
            deactivate: self.deactivate,
            deactivate_on_disconnect: self.deactivate_on_disconnect,
            include_overnight: self.include_overnight,
            auto_cancel_parent: self.auto_cancel_parent,
            min_trade_qty: if self.min_trade_qty == i32::MAX { 0 } else { self.min_trade_qty.max(0) as u32 },
            block_order: self.block_order,
            auto_cancel_date: self.auto_cancel_date.clone(),
            clearing_account: self.clearing_account.clone(),
            clearing_intent: self.clearing_intent.clone(),
            rule80a: self.rule80a.clone(),
            post_to_ats: if self.post_to_ats == i32::MAX { 0 } else { self.post_to_ats.max(0) as u32 },
            combo_legs: Vec::new(),
            primary_exchange: String::new(),
            delta_neutral_contract: None,
            // Valid trigger-method codes only: the raw `as u8`
            // cast wrapped -1 (Unknown) to 255, and
            // out-of-range codes went to the wire verbatim. Anything
            // unrecognized coerces to 0 (default = not emitted), matching
            // unknown->default handling.
            trigger_method: match self.trigger_method {
                0..=4 | 7 | 8 => self.trigger_method as u8,
                _ => 0,
            },
            cash_qty: crate::types::price_from_f64(self.cash_qty),
            conditions: self.conditions.clone(),
            conditions_cancel_order: self.conditions_cancel_order,
            conditions_ignore_rth: self.conditions_ignore_rth,
            // Keep 1..=4; anything else is "unset" and emits the protocol
            // default 3 (ReduceOnFillNonBlock).
            oca_type: match self.oca_type {
                1..=4 => self.oca_type as u8,
                _ => 0,
            },
            // No field on an order sets this. An exercise names an action and a
            // number of contracts and nothing else an order carries, so it has
            // a call of its own that builds the request directly.
            exercise_action: 0,
            what_if: self.what_if,
        }
    }

    /// Check if the order has any extended attributes set.
    /// The ladder this order describes, if it describes one.
    ///
    /// `i32::MAX` and `f64::MAX` are this API's "not set", so a field left
    /// alone contributes nothing and an order that sets none has no ladder.
    fn scale_attrs(&self) -> Option<Box<crate::types::ScaleAttrs>> {
        // Any one of them means a ladder was asked for. Keying only off the
        // first size and the step let the rest be set on their own and dropped.
        let asked = self.scale_init_level_size != i32::MAX
            || self.scale_subs_level_size != i32::MAX
            || self.scale_price_increment != f64::MAX
            || self.scale_profit_offset != f64::MAX
            || self.scale_price_adjust_value != f64::MAX
            || self.scale_price_adjust_interval != i32::MAX
            || self.scale_auto_reset
            || self.scale_random_percent
            || self.scale_init_position != i32::MAX
            || self.scale_init_fill_qty != i32::MAX
            // The public name for varying a ladder's component sizes. One
            // tag carries it.
            || self.randomize_size;
        if !asked {
            return None;
        }
        let px = |v: f64| if v == f64::MAX { 0 } else { crate::types::price_from_f64(v) };
        let n = |v: i32| if v == i32::MAX { 0 } else { v.max(0) as u32 };
        Some(Box::new(crate::types::ScaleAttrs {
            init_level_size: n(self.scale_init_level_size),
            subs_level_size: n(self.scale_subs_level_size),
            price_increment: px(self.scale_price_increment),
            profit_offset: px(self.scale_profit_offset),
            price_adjust_value: px(self.scale_price_adjust_value),
            price_adjust_interval: n(self.scale_price_adjust_interval),
            auto_reset: self.scale_auto_reset,
            random_percent: self.scale_random_percent || self.randomize_size,
            init_position: if self.scale_init_position == i32::MAX { 0 } else { self.scale_init_position },
            init_fill_qty: if self.scale_init_fill_qty == i32::MAX { 0 } else { self.scale_init_fill_qty },
        }))
    }

    /// The hedging leg this order asks for, if it asks for one.
    fn delta_neutral_attrs(&self) -> Option<Box<crate::types::DeltaNeutralAttrs>> {
        if self.delta_neutral_order_type.is_empty() {
            return None;
        }
        Some(Box::new(crate::types::DeltaNeutralAttrs {
            order_type: self.delta_neutral_order_type.clone(),
            aux_price: if self.delta_neutral_aux_price == f64::MAX { 0 }
                       else { crate::types::price_from_f64(self.delta_neutral_aux_price) },
            con_id: self.delta_neutral_con_id as i64,
        }))
    }
    /// Whether this order states anything beyond a plain one.
    ///
    /// Every order routes through the encoder now, so nothing branches on
    /// this: it is kept because it answers a question worth asking of an
    /// order, and its own tests are what check the attribute block stays
    /// complete as fields are added.
    pub fn has_extended_attrs(&self) -> bool {
        !self.settling_firm.is_empty()
            || self.discretionary_up_to_limit_price
            || self.randomize_size
            || !self.soft_dollar_tier_name.is_empty()
            || !self.soft_dollar_tier_val.is_empty()
            || !self.algo_id.is_empty()
            || self.display_size > 0
            || self.min_qty > 0
            || self.hidden
            || self.outside_rth
            || !self.good_after_time.is_empty()
            || !self.good_till_date.is_empty()
            || !self.oca_group.is_empty()
            || self.parent_id > 0
            || self.discretionary_amt > 0.0
            || self.sweep_to_fill
            || self.all_or_none
            || self.trigger_method > 0
            || self.cash_qty > 0.0
            // Everything `attrs()` carries has to be named here, or the order
            // takes the plain encoder and the attribute is dropped without a
            // word. Conditions are the costly one: the order goes out
            // unconditional and routes immediately.
            || !self.conditions.is_empty()
            || self.conditions_cancel_order
            || self.conditions_ignore_rth
            || self.oca_type > 0
            || (self.volatility != f64::MAX && self.volatility > 0.0)
            || self.volatility_type > 0
            || self.seek_price_improvement == Some(true)
            || !self.manual_order_time.is_empty()
            || !self.advanced_error_override.is_empty()
            || !self.active_start_time.is_empty()
            || !self.active_stop_time.is_empty()
            || self.post_only
            || self.solicited
            || (self.manual_order_indicator != i32::MAX && self.manual_order_indicator > 0)
            || self.route_marketable_to_bbo == Some(true)
            || self.imbalance_only
            || self.allow_pre_open
            || self.ignore_open_auction
            || self.is_oms_container
            || !self.ext_operator.is_empty()
            || !self.customer_account.is_empty()
            || !self.model_code.is_empty()
            || self.professional_customer
            || !self.fa_group.is_empty()
            || !self.fa_method.is_empty()
            || !self.fa_percentage.is_empty()
            || self.ref_futures_con_id > 0
            || !self.mifid2_decision_maker.is_empty()
            || !self.mifid2_decision_algo.is_empty()
            || !self.mifid2_execution_trader.is_empty()
            || !self.mifid2_execution_algo.is_empty()
            || self.mid_offset_at_whole != f64::MAX
            || self.mid_offset_at_half != f64::MAX
            || self.use_price_mgmt_algo.unwrap_or(0) > 0
            || self.duration != i32::MAX
            || (self.min_compete_size != i32::MAX && self.min_compete_size > 0)
            || self.compete_against_best_offset != f64::MAX
            || self.continuous_update
            || self.reference_price_type > 0
            || self.stock_range_lower != f64::MAX
            || self.stock_range_upper != f64::MAX
            || self.percent_offset != f64::MAX
            || self.not_held
            || !self.order_ref.is_empty()
            || !self.open_close.is_empty()
            || self.scale_attrs().is_some()
            || !self.delta_neutral_order_type.is_empty()
            || self.short_sale_slot != 0
            || !self.designated_location.is_empty()
            || self.exempt_code != -1
            || !self.hedge_type.is_empty()
            || !self.rule80a.is_empty()
            || self.post_to_ats != i32::MAX
            || self.deactivate
            || self.tif == "DTC"
            || self.deactivate_on_disconnect
            || self.include_overnight
            || self.auto_cancel_parent
            || self.min_trade_qty != i32::MAX
            || self.block_order
            || !self.auto_cancel_date.is_empty()
            || !self.clearing_account.is_empty()
            || !self.clearing_intent.is_empty()
    }
}

// ── TagValue ──

/// ibapi-compatible TagValue for algo and scanner filter parameters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TagValue {
    /// The name of one option a request carries.
    pub tag: String,
    /// What it is set to.
    pub value: String,
}

// ── OrderState ──

/// Per-account allocation for grouped/allocation orders (ibapi-compatible).
/// Decimal fields are carried as strings to preserve precision.
#[derive(Clone, Debug, Default)]
pub struct OrderAllocation {
    /// Which account this share of an advisor's order is for.
    pub account: String,
    /// What it holds now.
    pub position: String,
    /// What it should hold.
    pub position_desired: String,
    /// What it would hold once this order fills.
    pub position_after: String,
    /// How much of the order it was meant to take.
    pub desired_alloc_qty: String,
    /// How much it may take.
    pub allowed_alloc_qty: String,
    /// Whether those figures are money rather than shares.
    pub is_monetary: bool,
}

/// ibapi-compatible OrderState (used in openOrder callback).
#[derive(Clone, Debug)]
pub struct OrderState {
    /// Where the order stands, as the venue names it.
    pub status: String,
    /// What initial margin the account carried before this order.
    pub init_margin_before: String,
    /// What maintenance margin it carried.
    pub maint_margin_before: String,
    /// What equity with loan value it carried.
    pub equity_with_loan_before: String,
    /// What this order would change initial margin by.
    pub init_margin_change: String,
    /// And maintenance margin.
    pub maint_margin_change: String,
    /// And equity with loan value.
    pub equity_with_loan_change: String,
    /// What initial margin the account would carry with this order on.
    pub init_margin_after: String,
    /// And maintenance margin.
    pub maint_margin_after: String,
    /// And equity with loan value.
    pub equity_with_loan_after: String,
    /// What the order would cost, or did.
    pub commission_and_fees: f64,
    /// The least it could cost, where the venue states a range.
    pub min_commission_and_fees: f64,
    /// The most.
    pub max_commission_and_fees: f64,
    /// What those figures are in.
    pub commission_and_fees_currency: String,
    /// What the venue warns about the order, which rides its own field
    /// and not the order's text.
    pub warning_text: String,
    /// When the order finished.
    pub completed_time: String,
    /// What it finished as.
    pub completed_status: String,
    // ── ibapi-iso extension (2026-04-30): RTH-split margin + allocations ──
    /// What the margin figures are in.
    pub margin_currency: String,
    /// The same figure, measured outside regular hours,
    /// where the venue margins the two differently.
    pub init_margin_before_outside_rth: f64,
    /// As above.
    pub maint_margin_before_outside_rth: f64,
    /// As above.
    pub equity_with_loan_before_outside_rth: f64,
    /// As above.
    pub init_margin_change_outside_rth: f64,
    /// As above.
    pub maint_margin_change_outside_rth: f64,
    /// As above.
    pub equity_with_loan_change_outside_rth: f64,
    /// As above.
    pub init_margin_after_outside_rth: f64,
    /// As above.
    pub maint_margin_after_outside_rth: f64,
    /// As above.
    pub equity_with_loan_after_outside_rth: f64,
    /// A size the venue suggests instead of the one asked for.
    pub suggested_size: String,
    /// Why the venue refused it, in its own words.
    pub reject_reason: String,
    /// How an advisor's order divides across accounts.
    pub order_allocations: Vec<OrderAllocation>,
}

impl Default for OrderState {
    /// Every figure unstated. The three charges take the reference's own
    /// unset value rather than zero: an order the venue has not priced is
    /// not an order that costs nothing, and a program written against the
    /// reference tests for the sentinel before recording a cost.
    fn default() -> Self {
        Self {
            status: Default::default(),
            init_margin_before: Default::default(),
            maint_margin_before: Default::default(),
            equity_with_loan_before: Default::default(),
            init_margin_change: Default::default(),
            maint_margin_change: Default::default(),
            equity_with_loan_change: Default::default(),
            init_margin_after: Default::default(),
            maint_margin_after: Default::default(),
            equity_with_loan_after: Default::default(),
            commission_and_fees: f64::MAX,
            min_commission_and_fees: f64::MAX,
            max_commission_and_fees: f64::MAX,
            commission_and_fees_currency: Default::default(),
            warning_text: Default::default(),
            completed_time: Default::default(),
            completed_status: Default::default(),
            margin_currency: Default::default(),
            init_margin_before_outside_rth: f64::MAX,
            maint_margin_before_outside_rth: f64::MAX,
            equity_with_loan_before_outside_rth: f64::MAX,
            init_margin_change_outside_rth: f64::MAX,
            maint_margin_change_outside_rth: f64::MAX,
            equity_with_loan_change_outside_rth: f64::MAX,
            init_margin_after_outside_rth: f64::MAX,
            maint_margin_after_outside_rth: f64::MAX,
            equity_with_loan_after_outside_rth: f64::MAX,
            suggested_size: Default::default(),
            reject_reason: Default::default(),
            order_allocations: Default::default(),
        }
    }
}

impl From<&WhatIfResponse> for OrderState {
    /// What a preview says about the account, as margin figures rather than
    /// scaled integers.
    ///
    /// The venue answers a preview with the account's margin before and after
    /// the order; the change between them is the arithmetic, and the status a
    /// preview carries is what the order would be if it were sent.
    fn from(wi: &WhatIfResponse) -> Self {
        let fmt = |p: Price| format!("{:.2}", p as f64 / PRICE_SCALE_F);
        Self {
            status: "PreSubmitted".into(),
            init_margin_before: fmt(wi.init_margin_before),
            maint_margin_before: fmt(wi.maint_margin_before),
            equity_with_loan_before: fmt(wi.equity_with_loan_before),
            init_margin_change: fmt(wi.init_margin_change()),
            maint_margin_change: fmt(wi.maint_margin_change()),
            equity_with_loan_change: fmt(wi.equity_with_loan_change()),
            init_margin_after: fmt(wi.init_margin_after),
            maint_margin_after: fmt(wi.maint_margin_after),
            equity_with_loan_after: fmt(wi.equity_with_loan_after),
            // The sentinel this record already uses for an unstated figure,
            // which is what the two bounds beside it are given.
            commission_and_fees: wi.commission
                .map_or(f64::MAX, |c| c as f64 / PRICE_SCALE_F),
            // A commission the venue quotes as a range, and what it quotes it
            // in. Left off, a preview reported a cost of zero for every order
            // whose commission the venue could only bound, and a warning it
            // had attached went nowhere.
            min_commission_and_fees: wi.min_commission.map_or(f64::MAX, |p| p as f64 / PRICE_SCALE_F),
            max_commission_and_fees: wi.max_commission.map_or(f64::MAX, |p| p as f64 / PRICE_SCALE_F),
            commission_and_fees_currency: wi.commission_currency.clone(),
            warning_text: wi.warning_text.clone(),
            ..Default::default()
        }
    }
}

// ── Execution ──

/// ibapi-compatible Execution (used in execDetails callback).
#[derive(Clone, Debug, Default)]
pub struct Execution {
    /// Every field the report stated that this client does not name, as
    /// (tag, value), in the order the venue stated them.
    ///
    /// A report carries far more than any one client reads. What is not named
    /// is kept rather than dropped, so a fact the venue stated about a fill can
    /// be reached under its number instead of waiting to be named.
    pub unnamed_fields: Vec<(u32, String)>,
    /// The venue's id for this fill. What a commission report is matched
    /// to it by.
    pub exec_id: String,
    /// When it filled, as the venue states it.
    pub time: String,
    /// Which account it filled for.
    pub acct_number: String,
    /// Where it filled, which is not necessarily where the order was
    /// sent.
    pub exchange: String,
    /// `BOT` or `SLD`, as the venue names it.
    pub side: String,
    /// How much filled.
    pub shares: f64,
    /// At what price.
    pub price: f64,
    /// Order id assigned by the venue, stable across sessions.
    pub perm_id: i64,
    /// Which client placed the order.
    pub client_id: i64,
    /// The id the client placed it under.
    pub order_id: i64,
    /// The caller's own label for the order, as the report restates it.
    ///
    /// The venue states it on every report, so it is read there rather than
    /// looked up against the order this client remembers: a fill on an order
    /// placed in another session is still labelled, and a program matching its
    /// fills by label matched none.
    pub order_ref: String,
    /// How much of the order has filled in total.
    pub cum_qty: f64,
    /// The average price of everything filled so far.
    pub avg_price: f64,
    /// Whether this fill added liquidity or took it: 1 added, 2
    /// took, 3 both.
    pub last_liquidity: i32,
    /// Whether the venue closed the position rather than the caller.
    pub liquidation: i32,
    /// Which model the order belongs to, for an advisor.
    pub model_code: String,
    /// The rule the venue prices an economic-value contract by.
    pub ev_rule: String,
    /// What that rule multiplies by.
    pub ev_multiplier: f64,
    /// Whether the venue is still revising this order's price.
    pub pending_price_revision: bool,
    /// Who entered the order this fill belongs to, as the report names them.
    /// Empty where the report names nobody, which is what a session placing
    /// its own orders sees.
    pub submitter: String,
}

// ── ExecutionFilter ──

/// ibapi-compatible ExecutionFilter (used in reqExecutions).
#[derive(Clone, Debug, Default)]
pub struct ExecutionFilter {
    /// Only fills placed by this client. Zero for all.
    pub client_id: i64,
    /// Only fills on this account.
    pub acct_code: String,
    /// Only fills after this moment, applied to what this session has seen:
    /// the answer comes from the session's own record of its fills, not from
    /// the venue, so a caller wanting older fills reads them from a
    /// statement instead.
    pub time: String,
    /// Only fills on this symbol.
    pub symbol: String,
    /// Only fills on this kind of contract.
    pub sec_type: String,
    /// Only fills at this venue.
    pub exchange: String,
    /// Only buys, or only sells.
    pub side: String,
}

// ── CommissionAndFeesReport ──

/// ibapi-compatible CommissionAndFeesReport.
#[derive(Clone, Debug, Default)]
pub struct CommissionAndFeesReport {
    /// The fill this is the cost of.
    pub exec_id: String,
    /// What it cost.
    pub commission_and_fees: f64,
    /// In what currency.
    pub currency: String,
    /// What closing the position realised, where this fill
    /// closed one.
    pub realized_pnl: f64,
    /// A bond's yield at this price.
    pub yield_amount: f64,
    /// Which redemption that yield is measured to.
    pub yield_redemption_date: i64,
}

impl CommissionAndFeesReport {
    /// What a fill cost, against the execution it was charged on.
    ///
    /// The currency is the one the venue stated on that execution. Empty where
    /// it stated none: a currency nobody stated is not the dollar by default,
    /// and a fill on a contract denominated in anything else would otherwise
    /// report a cost in a currency it was not charged in.
    ///
    /// A fill states no realised P&L and no yield, and `f64::MAX` is how a
    /// field carrying no value is written on this surface.
    pub fn charged(exec_id: &str, commission_and_fees: f64, currency: &str) -> Self {
        Self {
            exec_id: exec_id.to_string(),
            commission_and_fees,
            currency: currency.to_string(),
            realized_pnl: f64::MAX,
            yield_amount: f64::MAX,
            yield_redemption_date: 0,
        }
    }
}

// ── TickAttrib ──

/// ibapi-compatible TickAttrib for tick_price callback.
#[derive(Clone, Debug, Default)]
pub struct TickAttrib {
    /// Whether this price can be traded against now.
    pub can_auto_execute: bool,
    /// Whether the price is outside the venue's limits.
    pub past_limit: bool,
    /// Whether it was stated before the venue opened.
    pub pre_open: bool,
}

// ── TickAttribLast ──

/// ibapi-compatible TickAttribLast for tick_by_tick_all_last callback.
#[derive(Clone, Debug, Default)]
pub struct TickAttribLast {
    /// Whether the trade was outside the venue's limits.
    pub past_limit: bool,
    /// Whether the trade goes unreported to the tape, which is
    /// what separates every trade from the ones that print.
    pub unreported: bool,
}

// ── TickAttribBidAsk ──

/// ibapi-compatible TickAttribBidAsk for tick_by_tick_bid_ask callback.
#[derive(Clone, Debug, Default)]
pub struct TickAttribBidAsk {
    /// Whether the bid is below the day's low.
    pub bid_past_low: bool,
    /// Whether the ask is above the day's high.
    pub ask_past_high: bool,
}

// ── BarData ──

/// ibapi-compatible BarData for historical data callbacks.
#[derive(Clone, Debug)]
pub struct BarData {
    /// When the bar opened. A day for a daily bar, a moment for anything
    /// shorter, in the zone the bar carries.
    pub date: String,
    /// The first price in the bar.
    pub open: f64,
    /// The highest.
    pub high: f64,
    /// The lowest.
    pub low: f64,
    /// The last.
    pub close: f64,
    /// How much traded, in the units the venue counts that contract in.
    pub volume: i64,
    /// The volume-weighted average price over the bar.
    pub wap: f64,
    /// How many trades made it.
    pub bar_count: i32,
    /// Which timezone `date` is stated in, as the reply states it. Without
    /// it the timestamp says nothing about what the bar times mean. Empty on
    /// streaming updates, which carry no timezone of their own.
    pub timezone: String,
    /// When the bar closed, as the venue states it.
    ///
    /// A bar the venue aggregated states its own bounds — a week runs Monday to
    /// Friday, a month the first to the last — and those are not derivable from
    /// the start. The last bar of any series is normally partial, so this is
    /// what tells a finished week from a running one. Empty where the venue
    /// stated none; it states one under `endTime` on the sizes it times and
    /// under `endDate` on the two it dates, and both are read.
    pub end: String,
}

impl Default for BarData {
    fn default() -> Self {
        Self {
            date: String::new(),
            open: 0.0,
            high: 0.0,
            low: 0.0,
            close: 0.0,
            volume: 0,
            wap: 0.0,
            bar_count: 0,
            timezone: String::new(),
            end: String::new(),
        }
    }
}

// ── ContractDetails ──

/// ibapi-compatible ContractDetails.
///
/// `trading_hours` / `liquid_hours` carry semicolon-delimited UTC session strings
/// (`"YYYYMMDD:HHMM-YYYYMMDD:HHMM;..."`) when populated. Consumers should convert
/// to local time using `time_zone_id` for display.
#[derive(Clone, Debug, Default)]
pub struct ContractDetails {
    /// The contract these details are about.
    pub contract: Contract,
    /// The venue's name for the market it trades on.
    pub market_name: String,
    /// The smallest amount its price can move. What every price on it is
    /// rounded to.
    pub min_tick: f64,
    /// Which order types this venue takes for it.
    pub order_types: String,
    /// Every venue it can be routed to.
    pub valid_exchanges: String,
    /// The issuer's full name.
    pub long_name: String,
    /// The last day it trades.
    pub last_trade_date: String,
    /// How many units one contract is worth.
    pub multiplier: String,
    /// When the venue is open for it, session by session, in the zone
    /// below.
    pub trading_hours: Option<String>,
    /// When it is liquid, which is narrower than when it is open.
    pub liquid_hours: Option<String>,
    /// The zone both of those are stated in.
    pub time_zone_id: Option<String>,
    /// The price-increment rules this contract trades under, as the venue
    /// names them.
    ///
    /// Usually empty here, and that is the venue's doing rather than a gap:
    /// it sends the increments themselves with the definition — the price
    /// steps and the size steps, whole tables of them — instead of naming a
    /// rule to go and fetch. `req_market_rule` answers from those, and says
    /// so when asked for one no contract on this session has used.
    pub market_rule_ids: String,
    /// What kind of stock it is, what it does, and where it is domiciled —
    /// parsed off the definition all along and handed to nobody.
    pub stock_type: String,
    /// The rule the venue evaluates this contract's economic value under, and
    /// what that evaluation is multiplied by. Both stated on the definition; a
    /// contract whose value follows something other than its own price is
    /// valued wrongly without them.
    pub ev_rule: String,
    /// What an economic-value contract's rule multiplies by.
    pub ev_multiplier: f64,
    /// What a bond is and what a fund is — terms, ratings, charges and where
    /// it may be sold. A caller asking about either received a symbol.
    pub coupon: f64,
    /// A future's delivery month.
    pub contract_month: String,
    /// What kind of contract the underlying is.
    pub under_sec_type: String,
    /// The venue's id for the underlying.
    pub under_con_id: u32,
    /// Its ticker.
    pub under_symbol: String,
    /// And the time of day it stops.
    pub last_trade_time: String,
    /// When the instrument was issued.
    pub issue_date: String,
    /// The smallest amount of it that can be traded.
    pub size_increment: f64,
    /// What the venue suggests trading in.
    ///
    /// The larger of two things the venue states: the size the contract's own
    /// rule deals in, and a figure it states separately for the contract. It
    /// is not the finest size the contract can be dealt in — for one dealt in
    /// fractions those differ by four decimal places, and reporting the finest
    /// as the suggestion suggests a ten-thousandth of a share where the venue
    /// suggests one.
    pub suggested_size_increment: f64,
    /// How many decimal places its prices carry.
    pub last_price_precision: f64,
    /// How many its sizes carry.
    pub last_size_precision: f64,
    /// How it settles: physically, or in cash.
    pub settlement_method: String,
    /// Every field the venue stated about this contract that this client does
    /// not yet name, as (tag, value). Kept rather than dropped: what is not
    /// named is still a fact the venue stated.
    pub unnamed_fields: Vec<(u32, String)>,
    /// What the venue notes about a bond.
    pub bond_notes: String,
    /// What it appends to the description.
    pub desc_append: String,
    /// What kind of bond it is.
    pub bond_type: String,
    /// How its coupon is set.
    pub coupon_type: String,
    /// When the next call or put may be exercised.
    pub next_option_date: String,
    /// Which of the two it is.
    pub next_option_type: String,
    /// What the agencies rate it.
    pub ratings: String,
    /// A fund's name.
    pub fund_name: String,
    /// The family it belongs to.
    pub fund_family: String,
    /// What kind of fund it is.
    pub fund_type: String,
    /// What it charges on the way in.
    pub fund_front_load: String,
    /// What it charges on the way out.
    pub fund_back_load: String,
    /// Over what period that exit charge falls away.
    pub fund_back_load_time_interval: String,
    /// What it charges to run.
    pub fund_management_fee: String,
    /// The amount above which the fund asks to be told in advance.
    pub fund_notify_amount: String,
    /// The least that may be bought to open.
    pub fund_minimum_initial_purchase: String,
    /// The least that may be added.
    pub fund_minimum_subsequent_purchase: String,
    /// Which US states it may be sold in.
    pub fund_blue_sky_states: String,
    /// And which territories.
    pub fund_blue_sky_territories: String,
    /// Whether it distributes income or accumulates
    /// it.
    pub fund_distribution_policy_indicator: String,
    /// What it holds.
    pub fund_asset_type: String,
    /// When it actually expires, where that differs from the last
    /// day it trades.
    pub real_expiration_date: String,
    /// Whether the issuer may redeem it early.
    pub callable: bool,
    /// Whether the holder may demand redemption.
    pub puttable: bool,
    /// Whether it converts to equity.
    pub convertible: bool,
    /// Whether that call or put redeems part of the principal rather than
    /// all of it.
    pub next_option_partial: bool,
    /// Whether it is closed.
    pub fund_closed: bool,
    /// Whether it is closed to new investors.
    pub fund_closed_for_new_investors: bool,
    /// Whether it is closed to new money from existing ones.
    pub fund_closed_for_new_money: bool,
    /// Which group of venues its book aggregates into.
    pub agg_group: i32,
    /// What a quoted price is multiplied by to reach money. Not one
    /// for every contract, and a price read without it is wrong by that factor.
    pub price_magnifier: i32,
    /// What the issuer does, broadest first. The venue states all three in one
    /// field; kept whole, a caller asking for the category was handed all of
    /// them with bars between.
    pub industry: String,
    /// What sector the issuer is in.
    pub category: String,
    /// More narrowly.
    pub subcategory: String,
    /// Where the issuer is.
    pub country: String,
    /// The identifier the contract is known by outside this venue.
    pub isin: String,
    /// The identifier a contract is known by in the American market, taken from
    /// the identifiers below by its kind — it has no field of its own.
    pub cusip: String,
    /// Every identifier the contract is known by, as the kind and the value.
    pub sec_id_list: Vec<(String, String)>,
    /// Why the contract may not be dealt in, each as the number the venue
    /// files the reason under and the words it states for that number. Empty
    /// where the venue states none, which is the usual answer.
    pub ineligibility_reason_list: Vec<(String, String)>,
    /// The smallest quantity the contract trades in, which is not always one.
    pub min_size: f64,
    /// The smallest quantity an algorithmic order trades in. `f64::MAX`:
    /// this client reads none off a definition.
    pub min_algo_size: f64,
    /// A bond's date, which sits here and not on the contract's expiry,
    /// as the reference client files it.
    pub maturity: String,
    /// An event contract's underlying. Empty: this client reads none off
    /// a definition.
    pub event_contract1: String,
    /// The first line of an event contract's terms. Empty, as above.
    pub event_contract_description1: String,
    /// The second line of an event contract's terms. Empty, as above.
    pub event_contract_description2: String,
}

impl Contract {
    /// What tells two contracts on one underlying apart, for a lookup that
    /// names this one.
    ///
    /// Every request that carries a contract rather than an id needs these:
    /// a lookup for an option by symbol alone answers whichever the venue
    /// lists first, which is a different contract from the one asked about.
    pub(crate) fn lookup_filters(&self) -> crate::types::SecDefFilters {
        crate::types::SecDefFilters {
            primary_exchange: self.primary_exchange.clone(),
            local_symbol: self.local_symbol.clone(),
            last_trade_date_or_contract_month: self.last_trade_date_or_contract_month.clone(),
            strike: self.strike,
            right: self.right.clone(),
            multiplier: self.multiplier.clone(),
            trading_class: self.trading_class.clone(),
            sec_id: self.sec_id.clone(),
            sec_id_type: self.sec_id_type.clone(),
            issuer_id: self.issuer_id.clone(),
        }
    }
}


// ── ContractDescription ──

/// ibapi-compatible ContractDescription for symbol search results.
#[derive(Clone, Debug, Default)]
pub struct ContractDescription {
    /// The venue's id for the contract found.
    pub con_id: i64,
    /// Its ticker.
    pub symbol: String,
    /// What kind of contract it is.
    pub sec_type: String,
    /// What it is priced in.
    pub currency: String,
    /// Where it is listed.
    pub primary_exchange: String,
    /// Which kinds of derivative the venue lists on it.
    pub derivative_sec_types: Vec<String>,
    /// The venue's own words for it.
    pub description: String,
    /// The venue's id for an issuer, where the match names one rather than a
    /// contract. Empty on a contract; on an issuer it is the only thing that
    /// identifies it, and it is what a lookup for that issuer's fixed income
    /// is made under.
    pub issuer_id: String,
}


// ── PriceIncrement (for market rules) ──

/// ibapi-compatible PriceIncrement for market_rule callback.
#[derive(Clone, Debug)]
pub struct PriceIncrement {
    /// Where this step of the ladder starts.
    pub low_edge: f64,
    /// What the price moves in above it.
    pub increment: f64,
}

/// What separates two contracts that share a symbol: expiry, strike, right
/// and multiplier. Empty for anything those do not distinguish, which is
/// every stock and every currency pair.
pub fn contract_identity(
    last_trade_date: &str, strike: f64, right: &str, multiplier: &str, currency: &str,
) -> String {
    let named_by_symbol = last_trade_date.is_empty() && strike <= 0.0 && right.is_empty();
    // A holding priced in the account's own currency is named completely by
    // its symbol. One priced in another is not: an order that says nothing
    // about the currency is taken as an order in the default one, which is a
    // different contract, and the venue answers it with nothing at all.
    let stated_currency = !currency.is_empty() && !currency.eq_ignore_ascii_case("USD");
    if named_by_symbol && !stated_currency {
        return String::new();
    }
    format!("{last_trade_date}|{strike}|{right}|{multiplier}|||{currency}")
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Contract ──

    #[test]
    fn contract_default_values() {
        let c = Contract::default();
        assert_eq!(c.con_id, 0);
        assert_eq!(c.symbol, "");
        assert_eq!(c.sec_type, "");
        assert_eq!(c.exchange, "");
        assert_eq!(c.currency, "");
        assert_eq!(c.strike, 0.0);
    }

    #[test]
    fn contract_clone() {
        let c = Contract { con_id: 265598, symbol: "AAPL".into(), ..Default::default() };
        let c2 = c.clone();
        assert_eq!(c2.con_id, 265598);
        assert_eq!(c2.symbol, "AAPL");
    }

    // ── Order ──

    #[test]
    fn order_default_values() {
        let o = Order::default();
        assert_eq!(o.order_id, 0);
        assert_eq!(o.action, "");
        assert_eq!(o.total_quantity, 0.0);
        assert_eq!(o.order_type, "");
        assert_eq!(o.tif, "");
        assert!(o.transmit);
        assert!(!o.what_if);
        assert!(!o.outside_rth);
    }

    #[test]
    fn order_side_parsing() {
        let mut o = Order { action: "BUY".into(), ..Default::default() };
        assert_eq!(o.side().unwrap(), Side::Buy);
        o.action = "SELL".into();
        assert_eq!(o.side().unwrap(), Side::Sell);
        o.action = "SSHORT".into();
        assert_eq!(o.side().unwrap(), Side::ShortSell);
        o.action = "B".into();
        assert_eq!(o.side().unwrap(), Side::Buy);
        o.action = "S".into();
        assert_eq!(o.side().unwrap(), Side::Sell);
    }

    #[test]
    fn order_side_invalid() {
        let o = Order { action: "INVALID".into(), ..Default::default() };
        assert!(o.side().is_err());
    }

    #[test]
    fn order_tif_byte_mapping() {
        let mut o = Order { tif: "DAY".into(), ..Default::default() };
        assert_eq!(o.tif_byte(), b'0');
        o.tif = "GTC".into();
        assert_eq!(o.tif_byte(), b'1');
        o.tif = "IOC".into();
        assert_eq!(o.tif_byte(), b'3');
        o.tif = "FOK".into();
        assert_eq!(o.tif_byte(), b'4');
        o.tif = "OPG".into();
        assert_eq!(o.tif_byte(), b'2');
        o.tif = "GTD".into();
        assert_eq!(o.tif_byte(), b'6');
        o.tif = "AUC".into();
        assert_eq!(o.tif_byte(), b'8');
    }

    #[test]
    fn order_has_extended_attrs() {
        let o = Order::default();
        assert!(!o.has_extended_attrs());

        let o2 = Order { hidden: true, ..Default::default() };
        assert!(o2.has_extended_attrs());

        let o3 = Order { display_size: 50, ..Default::default() };
        assert!(o3.has_extended_attrs());
    }

    #[test]
    fn order_attrs_conversion() {
        let o = Order {
            display_size: 50,
            hidden: true,
            discretionary_amt: 0.05,
            ..Default::default()
        };
        let attrs = o.attrs();
        assert_eq!(attrs.display_size, 50);
        assert!(attrs.hidden);
        assert_eq!(attrs.discretionary_amt, (0.05 * PRICE_SCALE_F) as Price);
    }

    #[test]
    fn order_attrs_conditions_forwarded() {
        let o = Order {
            conditions: vec![
                OrderCondition::Time { time: "20260311-09:30:00".into(), is_more: true, is_conjunction_connection: true },
            ],
            conditions_cancel_order: true,
            ..Default::default()
        };
        let attrs = o.attrs();
        assert_eq!(attrs.conditions.len(), 1);
        assert!(attrs.conditions_cancel_order);
    }

    // ── TagValue ──

    #[test]
    fn tag_value_fields() {
        let tv = TagValue { tag: "maxPctVol".into(), value: "0.1".into() };
        assert_eq!(tv.tag, "maxPctVol");
        assert_eq!(tv.value, "0.1");
    }

    // ── OrderState ──

    /// A default state has priced nothing: its charges are the reference's
    /// unset value, not zero, which a program reads as a cost.
    #[test]
    fn order_state_default() {
        let os = OrderState::default();
        assert_eq!(os.status, "");
        assert_eq!(os.commission_and_fees, f64::MAX);
        assert_eq!(os.min_commission_and_fees, f64::MAX);
        assert_eq!(os.max_commission_and_fees, f64::MAX);
    }

    // ── Execution ──

    #[test]
    fn execution_default() {
        let e = Execution::default();
        assert_eq!(e.exec_id, "");
        assert_eq!(e.shares, 0.0);
        assert_eq!(e.price, 0.0);
    }

    // ── TickAttrib ──

    #[test]
    fn tick_attrib_default() {
        let ta = TickAttrib::default();
        assert!(!ta.can_auto_execute);
        assert!(!ta.past_limit);
        assert!(!ta.pre_open);
    }

    // ── BarData ──

    #[test]
    fn bar_data_default() {
        let b = BarData::default();
        assert_eq!(b.date, "");
        assert_eq!(b.open, 0.0);
        assert_eq!(b.volume, 0);
    }

    // ── ContractDetails ──

    #[test]
    fn contract_details_default() {
        let cd = ContractDetails::default();
        assert_eq!(cd.contract.con_id, 0);
        assert_eq!(cd.min_tick, 0.0);
    }

    // The raw cast wrapped -1 to 255 and forwarded out-of-range
    // trigger codes to the wire verbatim.
    #[test]
    fn attrs_trigger_method_coerces_invalid_codes() {
        for (input, expected) in [(-1, 0u8), (5, 0), (6, 0), (9, 0), (255, 0),
                                  (0, 0), (2, 2), (4, 4), (7, 7), (8, 8)] {
            let o = Order { trigger_method: input, ..Default::default() };
            assert_eq!(o.attrs().trigger_method, expected, "input {input}");
        }
    }

    // ── ContractDescription ──

    #[test]
    fn contract_description_default() {
        let cd = ContractDescription::default();
        assert_eq!(cd.con_id, 0);
        assert_eq!(cd.symbol, "");
    }

    // ── CommissionAndFeesReport ──

    #[test]
    fn commission_and_fees_report_default() {
        let cr = CommissionAndFeesReport::default();
        assert_eq!(cr.exec_id, "");
        assert_eq!(cr.commission_and_fees, 0.0);
    }

    // ── PriceIncrement ──

    #[test]
    fn price_increment_fields() {
        let pi = PriceIncrement { low_edge: 0.0, increment: 0.01 };
        assert_eq!(pi.low_edge, 0.0);
        assert_eq!(pi.increment, 0.01);
    }

    /// `has_extended_attrs` decides whether an order routes through the encoder
    /// that emits the attribute block. Anything `attrs()` carries but this does
    /// not name is copied into `OrderAttrs` and then thrown away, with no error
    /// and nothing on the wire — so the two have to agree field for field.
    ///
    /// One entry per attribute `attrs()` carries. Adding a field there without
    /// adding it here is the bug this guards.
    #[test]
    fn every_carried_attribute_routes_through_the_extended_encoder() {
        /// Attribute name paired with the setter that turns it on.
        type Case = (&'static str, fn(&mut Order));

        let cases: Vec<Case> = vec![
            ("display_size", |o| o.display_size = 100),
            ("min_qty", |o| o.min_qty = 50),
            ("hidden", |o| o.hidden = true),
            ("outside_rth", |o| o.outside_rth = true),
            ("good_after_time", |o| o.good_after_time = "20260311 09:30:00".into()),
            ("good_till_date", |o| o.good_till_date = "20260311 16:00:00".into()),
            ("oca_group", |o| o.oca_group = "G1".into()),
            ("oca_type", |o| o.oca_type = 2),
            ("parent_id", |o| o.parent_id = 7),
            ("discretionary_amt", |o| o.discretionary_amt = 0.05),
            ("sweep_to_fill", |o| o.sweep_to_fill = true),
            ("all_or_none", |o| o.all_or_none = true),
            ("trigger_method", |o| o.trigger_method = 2),
            ("cash_qty", |o| o.cash_qty = 1000.0),
            ("conditions", |o| o.conditions.push(
                OrderCondition::Time { time: "20260311-09:30:00".into(), is_more: true, is_conjunction_connection: true },
            )),
            ("conditions_cancel_order", |o| o.conditions_cancel_order = true),
            ("conditions_ignore_rth", |o| o.conditions_ignore_rth = true),
            ("volatility", |o| o.volatility = 0.25),
            ("volatility_type", |o| o.volatility_type = 2),
            ("percent_offset", |o| o.percent_offset = 0.5),
            ("not_held", |o| o.not_held = true),
            ("order_ref", |o| o.order_ref = "ref-1".into()),
            ("open_close", |o| o.open_close = "O".into()),
            ("scale", |o| o.scale_init_level_size = 100),
            ("delta_neutral", |o| o.delta_neutral_order_type = "MKT".into()),
            ("short_sale_slot", |o| o.short_sale_slot = 2),
            ("designated_location", |o| o.designated_location = "IBKR".into()),
            ("exempt_code", |o| o.exempt_code = 3),
            ("hedge_type", |o| o.hedge_type = "B".into()),
            ("rule80a", |o| o.rule80a = "I".into()),
            ("post_to_ats", |o| o.post_to_ats = 30),
            ("deactivate", |o| o.deactivate = true),
            ("deactivate_at_close", |o| o.tif = "DTC".into()),
            ("deactivate_on_disconnect", |o| o.deactivate_on_disconnect = true),
            ("include_overnight", |o| o.include_overnight = true),
            ("auto_cancel_parent", |o| o.auto_cancel_parent = true),
            ("min_trade_qty", |o| o.min_trade_qty = 50),
            ("block_order", |o| o.block_order = true),
            ("auto_cancel_date", |o| o.auto_cancel_date = "20261231".into()),
            ("clearing_account", |o| o.clearing_account = "U123".into()),
            ("clearing_intent", |o| o.clearing_intent = "IB".into()),
            ("seek_price_improvement", |o| o.seek_price_improvement = Some(true)),
            ("manual_order_time", |o| o.manual_order_time = "20260101-09:30:00".into()),
            ("advanced_error_override", |o| o.advanced_error_override = "1".into()),
            ("active_start_time", |o| o.active_start_time = "20260101-09:30:00".into()),
            ("active_stop_time", |o| o.active_stop_time = "20260101-16:00:00".into()),
            ("post_only", |o| o.post_only = true),
            ("solicited", |o| o.solicited = true),
            ("manual_order_indicator", |o| o.manual_order_indicator = 1),
            ("route_marketable_to_bbo", |o| o.route_marketable_to_bbo = Some(true)),
            ("imbalance_only", |o| o.imbalance_only = true),
            ("allow_pre_open", |o| o.allow_pre_open = true),
            ("ignore_open_auction", |o| o.ignore_open_auction = true),
            ("is_oms_container", |o| o.is_oms_container = true),
            ("ext_operator", |o| o.ext_operator = "OP1".into()),
            ("customer_account", |o| o.customer_account = "CUST".into()),
            ("model_code", |o| o.model_code = "MODEL-1".into()),
            ("professional_customer", |o| o.professional_customer = true),
            ("fa_group", |o| o.fa_group = "AllAccounts".into()),
            ("fa_method", |o| o.fa_method = "EqualQuantity".into()),
            ("fa_percentage", |o| o.fa_percentage = "50".into()),
            ("ref_futures_con_id", |o| o.ref_futures_con_id = 12345),
            ("mifid2_decision_maker", |o| o.mifid2_decision_maker = "DM".into()),
            ("mifid2_decision_algo", |o| o.mifid2_decision_algo = "DA".into()),
            ("mifid2_execution_trader", |o| o.mifid2_execution_trader = "ET".into()),
            ("mifid2_execution_algo", |o| o.mifid2_execution_algo = "EA".into()),
            ("mid_offset_at_whole", |o| o.mid_offset_at_whole = 0.01),
            ("mid_offset_at_half", |o| o.mid_offset_at_half = 0.005),
            ("use_price_mgmt_algo", |o| o.use_price_mgmt_algo = Some(1)),
            ("duration", |o| o.duration = 60),
            ("min_compete_size", |o| o.min_compete_size = 100),
            ("compete_against_best_offset", |o| o.compete_against_best_offset = 0.02),
            ("continuous_update", |o| o.continuous_update = true),
            ("reference_price_type", |o| o.reference_price_type = 2),
            ("stock_range_lower", |o| o.stock_range_lower = 100.0),
            ("stock_range_upper", |o| o.stock_range_upper = 200.0),
            ("soft_dollar_tier_name", |o| o.soft_dollar_tier_name = "Tier A".into()),
            ("soft_dollar_tier_val", |o| o.soft_dollar_tier_val = "45.5".into()),
            ("settling_firm", |o| o.settling_firm = "FIRM".into()),
            ("discretionary_up_to_limit_price", |o| o.discretionary_up_to_limit_price = true),
            ("randomize_size", |o| o.randomize_size = true),
        ];

        // Structural link to `attrs()`: destructured without `..`, so adding a
        // field to `OrderAttrs` stops compiling here until it is accounted for
        // both in the predicate and in the list above.
        let crate::types::OrderAttrs {
            display_size: _, min_qty: _, hidden: _, outside_rth: _,
            good_after: _, good_till: _, good_till_date_ymd: _, oca_group: _, oca_group_str: _,
            oca_type: _, parent_id: _, discretionary_amt: _, sweep_to_fill: _,
            all_or_none: _, trigger_method: _, cash_qty: _, conditions: _,
            conditions_cancel_order: _, conditions_ignore_rth: _,
            volatility: _, volatility_type: _, use_price_mgmt_algo: _, duration: _,
            seek_price_improvement: _, manual_order_time: _,
            advanced_error_override: _,
            active_start_time: _, active_stop_time: _, post_only: _, solicited: _,
            manual_order_indicator: _, route_marketable_to_bbo: _, imbalance_only: _,
            allow_pre_open: _, ignore_open_auction: _, is_oms_container: _,
            ext_operator: _, customer_account: _, professional_customer: _,
            model_code: _,
            fa_group: _, fa_method: _, fa_percentage: _,
            ref_futures_con_id: _, mifid2_decision_maker: _, mifid2_decision_algo: _,
            mifid2_execution_trader: _, mifid2_execution_algo: _,
            mid_offset_at_whole: _, mid_offset_at_half: _,
            min_compete_size: _, compete_against_best_offset: _,
            continuous_update: _, reference_price_type: _,
            stock_range_lower: _, stock_range_upper: _,
            percent_offset: _, not_held: _, order_ref: _, open_close: _,
            scale: _, delta_neutral: _, short_sale_slot: _, designated_location: _,
            exempt_code: _, hedge_type: _, hedge_beta: _, hedge_ratio: _,
            combo_legs: _, rule80a: _, post_to_ats: _, deactivate: _,
            deactivate_at_close: _,
            deactivate_on_disconnect: _,
            include_overnight: _, auto_cancel_parent: _, min_trade_qty: _,
            block_order: _, auto_cancel_date: _, clearing_account: _, clearing_intent: _,
            primary_exchange: _, delta_neutral_contract: _,
            soft_dollar_tier_name: _, soft_dollar_tier_val: _, algo_id: _,
            settling_firm: _, discretionary_up_to_limit: _,
            // Reached by `exercise_options` rather than by an order, so there is
            // no setter to list above and nothing for the predicate to name.
            exercise_action: _,
            // Asks the venue what the order would cost instead of placing it.
            // It changes the question the message asks rather than what the
            // order carries, so it is not one of the attributes above.
            what_if: _,
        } = Order::default().attrs();

        assert!(
            !Order::default().has_extended_attrs(),
            "a default order carries nothing extended",
        );
        for (name, set) in cases {
            let mut order = Order::default();
            set(&mut order);
            assert!(
                order.has_extended_attrs(),
                "{name} is carried by attrs() but does not route through the extended encoder",
            );
        }
    }
}

#[cfg(test)]
mod varying_a_ladder_tests {
    use super::Order;

    /// Varying a ladder's component sizes is one thing on the wire, and this
    /// API has two names for it. Either one asks for it.
    #[test]
    fn either_name_varies_the_ladder() {
        for set in [
            (|o: &mut Order| o.randomize_size = true) as fn(&mut Order),
            |o: &mut Order| o.scale_random_percent = true,
        ] {
            let mut order = Order { scale_init_level_size: 100, ..Default::default() };
            set(&mut order);
            let scale = order.attrs().scale.expect("a ladder was asked for");
            assert!(scale.random_percent, "the ladder's sizes are not varied");
        }
    }

    /// And an order that asks for neither leaves them alone.
    #[test]
    fn neither_name_leaves_the_ladder_even() {
        let order = Order { scale_init_level_size: 100, ..Default::default() };
        assert!(!order.attrs().scale.expect("a ladder").random_percent);
    }
}

/// Naming a contract without filling a struct.
///
/// The venue identifies an instrument by a handful of fields whose defaults are
/// the same on nearly every request: a US stock routed to SMART in dollars, an
/// option on the same terms with a strike and an expiry. Written out in full
/// each time, the fields that matter to a caller are the two that differ from
/// the ones that do not.
///
/// Each of these returns a contract that a request will take as it stands.
/// They state what the kind of instrument requires and what does not vary; a
/// field that identifies one listing from another — a currency on a future, a
/// multiplier on an option — is left for the venue to answer with, because a
/// value assumed there asks about a contract the caller did not name. Where a
/// venue, a currency or a class is not the usual one, say so:
///
/// ```
/// # use ibx::types::model::Contract;
/// let spy = Contract::stock("SPY");
/// let toyota = Contract::stock("7203").on_exchange("TSEJ").in_currency("JPY");
/// let call = Contract::call("AAPL", 150.0, "20261218");
/// let eurusd = Contract::forex("EUR", "USD");
/// ```
impl Contract {
    /// Whether the venue quotes this instrument rather than printing trades on
    /// it.
    ///
    /// Asked rather than assumed. On 2026-08-27 the venue refused TRADES on
    /// `EUR/CASH@IDEALPRO` and `XAUUSD/CMDTY@SMART` with 162, no historical
    /// market data, and answered MIDPOINT on both. So those two have no trades
    /// to report and the price a caller means is the midpoint of the quote.
    ///
    /// `CFD` is deliberately absent, and it was in this list before anyone
    /// asked. The venue refused TRADES on `IBUS30/CFD` and answered 29 hourly
    /// bars of it on `AAPL/CFD` over the same window: a contract for difference
    /// on a share has the share's trades, one on an index has none. Nothing in
    /// `sec_type` separates them, so this cannot answer for CFDs and does not
    /// pretend to. A CFD is asked for what it says it wants; an index one is
    /// refused by name, which a caller can act on, rather than being handed
    /// midpoints it did not ask for.
    pub fn is_quoted_not_traded(&self) -> bool {
        ["CASH", "CMDTY"]
            .iter()
            .any(|kind| self.sec_type.eq_ignore_ascii_case(kind))
    }

    /// A share, routed to SMART and priced in dollars unless told otherwise.
    pub fn stock(symbol: &str) -> Self {
        Self {
            symbol: symbol.into(), sec_type: "STK".into(),
            exchange: "SMART".into(), currency: "USD".into(),
            ..Default::default()
        }
    }

    /// An option on a stock, by strike and expiry. `expiry` is `YYYYMMDD`, or
    /// `YYYYMM` for the month's own contract.
    ///
    /// No multiplier is stated. It is a hundred shares on nearly every listed
    /// option and something else on one adjusted by a split or a special
    /// dividend, and it is sent as part of what identifies the contract — so
    /// stating the usual one would ask about a contract that is not the one a
    /// caller with an adjusted option means. Left out, the venue answers with
    /// what it lists, and a description matching more than one is refused.
    fn option(symbol: &str, strike: f64, expiry: &str, right: &str) -> Self {
        Self {
            symbol: symbol.into(), sec_type: "OPT".into(),
            exchange: "SMART".into(), currency: "USD".into(),
            strike, right: right.into(),
            last_trade_date_or_contract_month: expiry.into(),
            ..Default::default()
        }
    }

    /// The right to buy, at `strike`, until `expiry` (`YYYYMMDD`).
    pub fn call(symbol: &str, strike: f64, expiry: &str) -> Self {
        Self::option(symbol, strike, expiry, "C")
    }

    /// The right to sell, at `strike`, until `expiry` (`YYYYMMDD`).
    pub fn put(symbol: &str, strike: f64, expiry: &str) -> Self {
        Self::option(symbol, strike, expiry, "P")
    }

    /// A future, by contract month (`YYYYMM`) or expiry (`YYYYMMDD`). The
    /// venue is named because futures do not route to SMART.
    ///
    /// No currency is stated: a future is quoted in whatever its venue quotes
    /// in, and currency identifies the contract. Assuming dollars would ask
    /// about a contract that does not exist on Eurex. State one with
    /// [`in_currency`](Contract::in_currency) where the venue lists the same
    /// symbol in more than one.
    pub fn future(symbol: &str, expiry: &str, exchange: &str) -> Self {
        Self {
            symbol: symbol.into(), sec_type: "FUT".into(),
            exchange: exchange.into(),
            last_trade_date_or_contract_month: expiry.into(),
            ..Default::default()
        }
    }

    /// A currency pair, quoted base against quote, on IDEALPRO.
    pub fn forex(base: &str, quote: &str) -> Self {
        Self {
            symbol: base.into(), sec_type: "CASH".into(),
            exchange: "IDEALPRO".into(), currency: quote.into(),
            ..Default::default()
        }
    }

    /// An index, which is quoted and never traded.
    ///
    /// No currency is stated, for the reason [`future`](Contract::future) does
    /// not state one.
    pub fn index(symbol: &str, exchange: &str) -> Self {
        Self {
            symbol: symbol.into(), sec_type: "IND".into(),
            exchange: exchange.into(),
            ..Default::default()
        }
    }

    /// A contract the venue has already named, by its own id. Every other
    /// field is left empty: an id identifies the contract on its own.
    pub fn by_id(con_id: i64) -> Self {
        Self { con_id, ..Default::default() }
    }

    /// Route somewhere other than the default.
    #[must_use]
    pub fn on_exchange(mut self, exchange: &str) -> Self {
        self.exchange = exchange.into();
        self
    }

    /// Price in a currency other than dollars.
    #[must_use]
    pub fn in_currency(mut self, currency: &str) -> Self {
        self.currency = currency.into();
        self
    }

    /// State which listing, where a symbol is carried on more than one and the
    /// venue would otherwise answer with whichever it lists first.
    #[must_use]
    pub fn listed_on(mut self, primary_exchange: &str) -> Self {
        self.primary_exchange = primary_exchange.into();
        self
    }

    /// State the venue's name for this contract, where the symbol is
    /// ambiguous without it.
    #[must_use]
    pub fn named(mut self, local_symbol: &str) -> Self {
        self.local_symbol = local_symbol.into();
        self
    }
}

/// Stating an order without filling a struct.
///
/// An order has a hundred and fifty-four fields and a caller states four of
/// them: which way, how much, what kind, and at what price. The rest carry the
/// defaults the venue assumes. Each of these fills those four and leaves the
/// rest alone, so what a reader sees is the order and not the form it was
/// written on.
///
/// `side` is `"BUY"` or `"SELL"`, as the venue spells them. Every one of these is
/// a plain [`Order`], so a field this shorthand does not reach is set on the
/// value it returns.
///
/// ```
/// # use ibx::types::model::Order;
/// let buy = Order::market("BUY", 100.0);
/// let bid = Order::limit("BUY", 100.0, 42.50);
/// let out = Order::stop("SELL", 100.0, 41.00);
/// let good_till_cancelled = Order { tif: "GTC".into(), ..Order::limit("BUY", 1.0, 10.0) };
/// ```
impl Order {
    /// Filled at whatever the market is, immediately.
    pub fn market(side: &str, quantity: f64) -> Self {
        Self {
            action: side.into(), total_quantity: quantity,
            order_type: "MKT".into(), tif: "DAY".into(),
            ..Default::default()
        }
    }

    /// Filled at `price` or better, or not at all.
    pub fn limit(side: &str, quantity: f64, price: f64) -> Self {
        Self {
            action: side.into(), total_quantity: quantity,
            order_type: "LMT".into(), lmt_price: price, tif: "DAY".into(),
            ..Default::default()
        }
    }

    /// Becomes a market order once the market reaches `trigger`.
    pub fn stop(side: &str, quantity: f64, trigger: f64) -> Self {
        Self {
            action: side.into(), total_quantity: quantity,
            order_type: "STP".into(), aux_price: trigger, tif: "DAY".into(),
            ..Default::default()
        }
    }

    /// Becomes a limit order at `limit` once the market reaches `trigger`.
    ///
    /// The limit is what stops a stop from filling at any price at all in a
    /// market that has gapped past the trigger.
    pub fn stop_limit(side: &str, quantity: f64, trigger: f64, limit: f64) -> Self {
        Self {
            action: side.into(), total_quantity: quantity,
            order_type: "STP LMT".into(), aux_price: trigger, lmt_price: limit,
            tif: "DAY".into(),
            ..Default::default()
        }
    }

    /// A stop that follows the market by `percent`, and does not follow it back.
    pub fn trailing_stop(side: &str, quantity: f64, percent: f64) -> Self {
        Self {
            action: side.into(), total_quantity: quantity,
            order_type: "TRAIL".into(), trailing_percent: percent, tif: "DAY".into(),
            ..Default::default()
        }
    }

    /// Stand until cancelled rather than expiring at the close.
    #[must_use]
    pub fn good_till_cancelled(mut self) -> Self {
        self.tif = "GTC".into();
        self
    }

    /// Fill in the auction and the session, not only the session.
    #[must_use]
    pub fn outside_regular_hours(mut self) -> Self {
        self.outside_rth = true;
        self
    }
}

#[cfg(test)]
mod quoted_not_traded_tests {
    use super::Contract;

    /// The rule was written twice and the second spelling had dropped a class,
    /// so a commodity was asked for trades that do not exist and the stream
    /// stayed empty. One predicate, and this says which classes are in it.
    #[test]
    fn only_what_the_venue_refused_trades_on_is_quoted() {
        // Measured 2026-08-27: TRADES refused with 162 on both, MIDPOINT answered.
        for kind in ["CASH", "CMDTY", "cmdty"] {
            let mut c = Contract::stock("X");
            c.sec_type = kind.to_string();
            assert!(c.is_quoted_not_traded(), "{kind} was refused TRADES by the venue");
        }
        // A share CFD answered 29 TRADES bars where an index CFD refused, and
        // the security type does not say which one is in hand. Guessing here
        // hands a caller midpoints where trades exist.
        for kind in ["STK", "FUT", "OPT", "CFD"] {
            let mut c = Contract::stock("X");
            c.sec_type = kind.to_string();
            assert!(!c.is_quoted_not_traded(), "{kind} is not answered for here");
        }
    }
}
