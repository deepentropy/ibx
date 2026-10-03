//! ibapi-compatible types: Contract, Order, OrderState, Execution, TagValue, BarData,
//! ContractDetails, ContractDescription, and order conditions.
//!
//! These are plain Rust structs (no PyO3) shared by both the Rust EClient and the Python bridge.

use crate::types::*;

pub const PRICE_SCALE_F: f64 = PRICE_SCALE as f64;
/// Fixed-point quantities (fills, positions) to decimal shares.
pub const QTY_SCALE_F: f64 = QTY_SCALE as f64;

// ── ComboLeg ──

/// ibapi-compatible ComboLeg for combination orders.
#[derive(Clone, Debug, PartialEq)]
pub struct ComboLeg {
    pub con_id: i64,
    pub ratio: i32,
    pub action: String,
    pub exchange: String,
    /// 0 same as the order, 1 open, 2 close, 3 unknown.
    pub open_close: i32,
    pub short_sale_slot: i32,
    pub designated_location: String,
    /// -1 when no exempt code, as the official API's default.
    pub exempt_code: i32,
}

impl Default for ComboLeg {
    fn default() -> Self {
        Self {
            con_id: 0, ratio: 0, action: String::new(), exchange: String::new(), open_close: 0,
            short_sale_slot: 0, designated_location: String::new(), exempt_code: -1,
        }
    }
}

// ── DeltaNeutralContract ──

/// ibapi-compatible DeltaNeutralContract for delta-neutral orders.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DeltaNeutralContract {
    pub con_id: i64,
    pub delta: f64,
    pub price: f64,
}

// ── Contract ──

/// ibapi-compatible Contract. Matches C++ `Contract` struct fields.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Contract {
    pub con_id: i64,
    pub symbol: String,
    pub sec_type: String,
    pub exchange: String,
    pub currency: String,
    pub last_trade_date_or_contract_month: String,
    pub strike: f64,
    pub right: String,
    pub multiplier: String,
    pub local_symbol: String,
    pub primary_exchange: String,
    pub trading_class: String,
    pub last_trade_date: String,
    pub include_expired: bool,
    pub sec_id_type: String,
    pub sec_id: String,
    pub description: String,
    pub issuer_id: String,
    pub combo_legs_descrip: String,
    pub combo_legs: Vec<ComboLeg>,
    pub delta_neutral_contract: Option<DeltaNeutralContract>,
}

// ── Order ──

/// ibapi-compatible Order. Matches C++ `Order` struct fields.
#[derive(Clone, Debug)]
pub struct Order {
    pub order_id: i64,
    pub action: String,
    pub total_quantity: f64,
    pub order_type: String,
    pub lmt_price: f64,
    pub aux_price: f64,
    pub tif: String,
    pub outside_rth: bool,
    pub display_size: i32,
    pub min_qty: i32,
    pub hidden: bool,
    pub good_after_time: String,
    pub good_till_date: String,
    pub oca_group: String,
    pub trailing_percent: f64,
    pub algo_strategy: String,
    pub algo_params: Vec<TagValue>,
    pub what_if: bool,
    pub cash_qty: f64,
    pub parent_id: i64,
    pub transmit: bool,
    pub discretionary_amt: f64,
    pub sweep_to_fill: bool,
    pub all_or_none: bool,
    pub trigger_method: i32,
    pub adjusted_order_type: String,
    pub trigger_price: f64,
    pub adjusted_stop_price: f64,
    pub adjusted_stop_limit_price: f64,
    pub conditions: Vec<OrderCondition>,
    pub conditions_ignore_rth: bool,
    pub conditions_cancel_order: bool,
    // ── ibapi-parity fields ──
    pub account: String,
    pub active_start_time: String,
    pub active_stop_time: String,
    pub adjustable_trailing_unit: i32,
    pub adjusted_trailing_amount: f64,
    pub advanced_error_override: String,
    pub algo_id: String,
    pub allow_pre_open: bool,
    pub auction_strategy: i32,
    pub auto_cancel_date: String,
    pub auto_cancel_parent: bool,
    pub basis_points: f64,
    pub basis_points_type: i32,
    pub block_order: bool,
    pub bond_accrued_interest: String,
    pub clearing_account: String,
    pub clearing_intent: String,
    pub client_id: i32,
    pub compete_against_best_offset: f64,
    pub continuous_update: bool,
    pub customer_account: String,
    pub deactivate: bool,
    pub delta: f64,
    pub delta_neutral_aux_price: f64,
    pub delta_neutral_clearing_account: String,
    pub delta_neutral_clearing_intent: String,
    pub delta_neutral_con_id: i32,
    pub delta_neutral_designated_location: String,
    pub delta_neutral_open_close: String,
    pub delta_neutral_order_type: String,
    pub delta_neutral_settling_firm: String,
    pub delta_neutral_short_sale: bool,
    pub delta_neutral_short_sale_slot: i32,
    pub designated_location: String,
    pub discretionary_up_to_limit_price: bool,
    pub dont_use_auto_price_for_hedge: bool,
    pub duration: i32,
    pub exempt_code: i32,
    pub ext_operator: String,
    pub fa_group: String,
    pub fa_method: String,
    pub fa_percentage: String,
    pub filled_quantity: f64,
    pub hedge_param: String,
    pub hedge_type: String,
    pub ignore_open_auction: bool,
    pub imbalance_only: bool,
    pub include_overnight: bool,
    pub is_oms_container: bool,
    pub is_pegged_change_amount_decrease: bool,
    pub lmt_price_offset: f64,
    pub manual_order_indicator: i32,
    pub manual_order_time: String,
    pub mid_offset_at_half: f64,
    pub mid_offset_at_whole: f64,
    pub mifid2_decision_algo: String,
    pub mifid2_decision_maker: String,
    pub mifid2_execution_algo: String,
    pub mifid2_execution_trader: String,
    pub min_compete_size: i32,
    pub min_trade_qty: i32,
    pub model_code: String,
    pub not_held: bool,
    pub oca_type: i32,
    pub open_close: String,
    pub opt_out_smart_routing: bool,
    pub order_combo_legs: Vec<f64>,
    pub order_misc_options: Vec<TagValue>,
    pub order_ref: String,
    pub origin: i32,
    pub override_percentage_constraints: bool,
    pub parent_perm_id: i64,
    pub pegged_change_amount: f64,
    pub percent_offset: f64,
    pub perm_id: i64,
    pub post_only: bool,
    pub post_to_ats: i32,
    pub professional_customer: bool,
    pub pt_order_id: i32,
    pub pt_order_type: String,
    pub randomize_price: bool,
    pub randomize_size: bool,
    pub ref_futures_con_id: i32,
    pub reference_change_amount: f64,
    pub reference_contract_id: i32,
    pub reference_exchange_id: String,
    pub reference_price_type: i32,
    pub route_marketable_to_bbo: bool,
    pub rule80a: String,
    pub scale_auto_reset: bool,
    pub scale_init_fill_qty: i32,
    pub scale_init_level_size: i32,
    pub scale_init_position: i32,
    pub scale_price_adjust_interval: i32,
    pub scale_price_adjust_value: f64,
    pub scale_price_increment: f64,
    pub scale_profit_offset: f64,
    pub scale_random_percent: bool,
    pub scale_subs_level_size: i32,
    pub scale_table: String,
    pub seek_price_improvement: bool,
    pub settling_firm: String,
    pub shareholder: String,
    pub short_sale_slot: i32,
    pub sl_order_id: i32,
    pub sl_order_type: String,
    pub smart_combo_routing_params: Vec<TagValue>,
    pub soft_dollar_tier_name: String,
    pub soft_dollar_tier_val: String,
    pub soft_dollar_tier_display_name: String,
    pub solicited: bool,
    pub starting_price: f64,
    pub stock_range_lower: f64,
    pub stock_range_upper: f64,
    pub stock_ref_price: f64,
    pub submitter: String,
    pub trail_stop_price: f64,
    pub use_price_mgmt_algo: i32,
    pub volatility: f64,
    pub volatility_type: i32,
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
            tif: "DAY".into(),
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
            dont_use_auto_price_for_hedge: true,
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
            route_marketable_to_bbo: false,
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
            seek_price_improvement: false,
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
            // Unset, as the API (ibx#492).
            use_price_mgmt_algo: i32::MAX,
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
    pub fn tif_byte(&self) -> u8 {
        match self.tif.as_str() {
            "GTC" => b'1',
            "IOC" => b'3',
            "FOK" => b'4',
            "OPG" => b'2',
            "GTD" => b'6',
            // DTC keeps its own code; it goes out as GTC plus the DTC flag
            // (ibx#467).
            "DTC" => crate::types::TIF_DTC,
            "AUC" => b'8',
            // The overnight values keep their own codes; both go out as
            // DAY (ibx#467).
            "OVERNIGHT" => b'j',
            "OVERNIGHT + DAY" => b'b',
            _ => b'0', // DAY
        }
    }

    /// Build OrderAttrs from Order fields.
    pub fn attrs(&self) -> OrderAttrs {
        // Parse the good-till expiry string into either a UTC instant or a
        // calendar date. An expiry that does not parse is refused with 343
        // before the order is built (ibx#335), so the drop below is not
        // reached from place_order.
        let (good_till, good_till_date_ymd) =
            match crate::config::parse_ib_expiry(&self.good_till_date) {
                Ok(None) => (0, 0),
                Ok(Some(crate::config::IbExpiry::Instant(secs))) => (secs, 0),
                Ok(Some(crate::config::IbExpiry::DateOnly(ymd))) => (0, ymd),
                Err(e) => {
                    log::warn!("dropping good_till_date: {}", e);
                    (0, 0)
                }
            };
        OrderAttrs {
            order_ref: self.order_ref.clone(),
            display_size: self.display_size.max(0) as u32,
            min_qty: self.min_qty.max(0) as u32,
            hidden: self.hidden,
            outside_rth: self.outside_rth,
            // A goodAfterTime that is not a date and time is refused before
            // sending (ibx#467), so only an instant reaches here.
            good_after: match crate::config::parse_ib_expiry(&self.good_after_time) {
                Ok(Some(crate::config::IbExpiry::Instant(secs))) => secs,
                _ => 0,
            },
            good_till,
            good_till_date_ymd,
            oca_group: self.oca_group.parse().unwrap_or(0),
            oca_group_str: if self.oca_group.parse::<u64>().is_err() && !self.oca_group.is_empty() {
                self.oca_group.clone()
            } else {
                String::new()
            },
            parent_id: self.parent_id.max(0),
            discretionary_amt: (self.discretionary_amt * PRICE_SCALE_F) as Price,
            sweep_to_fill: self.sweep_to_fill,
            all_or_none: self.all_or_none,
            // Valid trigger-method codes only (ibx#223): the raw `as u8`
            // cast wrapped the gateway's -1 (Unknown) to 255, and
            // out-of-range codes went to the wire verbatim. Anything
            // unrecognized coerces to 0 (default = not emitted), matching
            // the gateway's unknown->default handling.
            trigger_method: match self.trigger_method {
                0..=4 | 7 | 8 => self.trigger_method as u8,
                _ => 0,
            },
            cash_qty: (self.cash_qty * PRICE_SCALE_F) as Price,
            conditions: self.conditions.clone(),
            conditions_cancel_order: self.conditions_cancel_order,
            conditions_ignore_rth: self.conditions_ignore_rth,
            // Keep 1..=4; anything else is "unset" and emits the gateway
            // default 3 (ReduceOnFillNonBlock). See ibx#215.
            oca_type: match self.oca_type {
                1..=4 => self.oca_type as u8,
                _ => 0,
            },
            customer_account: self.customer_account.clone(),
            professional_customer: self.professional_customer,
            reference_exchange: self.reference_exchange_id.clone(),
            include_overnight: self.include_overnight,
            clearing_intent: self.clearing_intent.clone(),
            short_sale: ShortSale {
                slot: self.short_sale_slot,
                location: self.designated_location.clone(),
                exempt_code: self.exempt_code,
            },
            use_price_mgmt_algo: self.price_mgmt_algo(),
            // Set where a new order is built (ibx#263).
            algo: None,
            // Set where a combo order is built, from its contract (ibx#470).
            combo: None,
        }
    }

    /// The usePriceMgmtAlgo value: None when unset (`i32::MAX`, the API's
    /// unset value), else whether it is non-zero (ibx#492).
    pub fn price_mgmt_algo(&self) -> Option<bool> {
        (self.use_price_mgmt_algo != i32::MAX).then_some(self.use_price_mgmt_algo != 0)
    }

    /// Check if the order has any extended attributes set.
    pub fn has_extended_attrs(&self) -> bool {
        // An orderRef rides the extended encoders, which send it (ibx#466).
        !self.order_ref.is_empty()
            || self.display_size > 0
            || self.min_qty > 0
            || self.hidden
            || self.outside_rth
            || !self.good_after_time.is_empty()
            || !self.good_till_date.is_empty()
            || !self.customer_account.is_empty()
            || self.professional_customer
            || !self.oca_group.is_empty()
            || self.parent_id > 0
            || self.discretionary_amt > 0.0
            || self.sweep_to_fill
            || self.all_or_none
            || self.trigger_method > 0
            || self.cash_qty > 0.0
            // An order whose only extra is conditions went down a path that
            // sends none, and was routed at once (ibx#325).
            || !self.conditions.is_empty()
            // includeOvernight rides the attributes (ibx#467).
            || self.include_overnight
            // The clearing intent and the short-sale instructions reach the
            // side check and the short-sale fields (ibx#417).
            || !self.clearing_intent.is_empty()
            || self.short_sale_slot != 0
            || !self.designated_location.is_empty()
            || self.exempt_code != -1
            // A usePriceMgmtAlgo the caller set (ibx#492).
            || self.use_price_mgmt_algo != i32::MAX
    }
}

// ── TagValue ──

/// ibapi-compatible TagValue for algo parameters.
#[derive(Clone, Debug, PartialEq)]
pub struct TagValue {
    pub tag: String,
    pub value: String,
}

// ── ScannerSubscription ──

/// ibapi-compatible scanner subscription (ibx#456). Unset values are the
/// ibapi ones: `-1` rows, `f64::MAX` / `i32::MAX` numbers, empty texts.
#[derive(Clone, Debug)]
pub struct ScannerSubscription {
    pub number_of_rows: i32,
    pub instrument: String,
    pub location_code: String,
    pub scan_code: String,
    pub above_price: f64,
    pub below_price: f64,
    pub above_volume: i32,
    pub market_cap_above: f64,
    pub market_cap_below: f64,
    pub moody_rating_above: String,
    pub moody_rating_below: String,
    pub sp_rating_above: String,
    pub sp_rating_below: String,
    pub maturity_date_above: String,
    pub maturity_date_below: String,
    pub coupon_rate_above: f64,
    pub coupon_rate_below: f64,
    pub exclude_convertible: bool,
    pub average_option_volume_above: i32,
    pub scanner_setting_pairs: String,
    pub stock_type_filter: String,
}

impl Default for ScannerSubscription {
    fn default() -> Self {
        Self {
            number_of_rows: -1,
            instrument: String::new(),
            location_code: String::new(),
            scan_code: String::new(),
            above_price: f64::MAX,
            below_price: f64::MAX,
            above_volume: i32::MAX,
            market_cap_above: f64::MAX,
            market_cap_below: f64::MAX,
            moody_rating_above: String::new(),
            moody_rating_below: String::new(),
            sp_rating_above: String::new(),
            sp_rating_below: String::new(),
            maturity_date_above: String::new(),
            maturity_date_below: String::new(),
            coupon_rate_above: f64::MAX,
            coupon_rate_below: f64::MAX,
            exclude_convertible: false,
            average_option_volume_above: i32::MAX,
            scanner_setting_pairs: String::new(),
            stock_type_filter: String::new(),
        }
    }
}

// ── OrderState ──

/// Per-account allocation for grouped/allocation orders (ibapi-compatible).
/// Decimal fields are carried as strings to preserve precision.
#[derive(Clone, Debug, Default)]
pub struct OrderAllocation {
    pub account: String,
    pub position: String,
    pub position_desired: String,
    pub position_after: String,
    pub desired_alloc_qty: String,
    pub allowed_alloc_qty: String,
    pub is_monetary: bool,
}

/// ibapi-compatible OrderState (used in openOrder callback).
#[derive(Clone, Debug, Default)]
pub struct OrderState {
    pub status: String,
    pub init_margin_before: String,
    pub maint_margin_before: String,
    pub equity_with_loan_before: String,
    pub init_margin_change: String,
    pub maint_margin_change: String,
    pub equity_with_loan_change: String,
    pub init_margin_after: String,
    pub maint_margin_after: String,
    pub equity_with_loan_after: String,
    pub commission_and_fees: f64,
    pub min_commission_and_fees: f64,
    pub max_commission_and_fees: f64,
    pub commission_and_fees_currency: String,
    pub warning_text: String,
    pub completed_time: String,
    pub completed_status: String,
    // ── ibapi-iso extension (2026-04-30): RTH-split margin + allocations ──
    pub margin_currency: String,
    pub init_margin_before_outside_rth: f64,
    pub maint_margin_before_outside_rth: f64,
    pub equity_with_loan_before_outside_rth: f64,
    pub init_margin_change_outside_rth: f64,
    pub maint_margin_change_outside_rth: f64,
    pub equity_with_loan_change_outside_rth: f64,
    pub init_margin_after_outside_rth: f64,
    pub maint_margin_after_outside_rth: f64,
    pub equity_with_loan_after_outside_rth: f64,
    pub suggested_size: String,
    pub reject_reason: String,
    pub order_allocations: Vec<OrderAllocation>,
}

// ── Execution ──

/// ibapi-compatible Execution (used in execDetails callback).
#[derive(Clone, Debug, Default)]
pub struct Execution {
    pub exec_id: String,
    pub time: String,
    pub acct_number: String,
    pub exchange: String,
    pub side: String,
    pub shares: f64,
    pub price: f64,
    pub perm_id: i64,
    pub client_id: i64,
    pub order_id: i64,
    pub cum_qty: f64,
    pub avg_price: f64,
    pub last_liquidity: i32,
    pub liquidation: i32,
    pub model_code: String,
    /// The order's orderRef (ibx#474).
    pub order_ref: String,
    pub ev_rule: String,
    pub ev_multiplier: f64,
    pub pending_price_revision: bool,
}

// ── ExecutionFilter ──

/// ibapi-compatible ExecutionFilter (used in reqExecutions).
#[derive(Clone, Debug, Default)]
pub struct ExecutionFilter {
    pub client_id: i64,
    pub acct_code: String,
    pub time: String,
    pub symbol: String,
    pub sec_type: String,
    pub exchange: String,
    pub side: String,
}

// ── CommissionAndFeesReport ──

/// ibapi-compatible CommissionAndFeesReport.
#[derive(Clone, Debug, Default)]
pub struct CommissionAndFeesReport {
    pub exec_id: String,
    pub commission_and_fees: f64,
    pub currency: String,
    pub realized_pnl: f64,
    pub yield_amount: f64,
    pub yield_redemption_date: String,
}

// ── TickAttrib ──

/// ibapi-compatible TickAttrib for tick_price callback.
#[derive(Clone, Debug, Default)]
pub struct TickAttrib {
    pub can_auto_execute: bool,
    pub past_limit: bool,
    pub pre_open: bool,
}

// ── TickAttribLast ──

/// ibapi-compatible TickAttribLast for tick_by_tick_all_last callback.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TickAttribLast {
    pub past_limit: bool,
    pub unreported: bool,
}

// ── TickAttribBidAsk ──

/// ibapi-compatible TickAttribBidAsk for tick_by_tick_bid_ask callback.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TickAttribBidAsk {
    pub bid_past_low: bool,
    pub ask_past_high: bool,
}

// ── BarData ──

/// ibapi-compatible BarData for historical data callbacks.
#[derive(Clone, Debug)]
pub struct BarData {
    pub date: String,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: i64,
    pub wap: f64,
    pub bar_count: i32,
    /// Timezone of `date` as reported by the reply (ibx#234) — previously
    /// parsed and then discarded, leaving the bare timestamp string as the
    /// only (unverifiable) evidence of what the bar times mean. Empty on
    /// streaming updates, which carry no timezone of their own.
    pub timezone: String,
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
        }
    }
}

// ── ContractDetails ──

/// ibapi-compatible IneligibilityReason of a contract (ibx#436).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct IneligibilityReason {
    pub id: String,
    pub description: String,
}

/// ibapi-compatible ContractDetails, with the fields of a contract row and
/// of a bond row (ibx#436). Unset decimals are `f64::MAX`.
///
/// `trading_hours` / `liquid_hours` are in the zone `time_zone_id`, from
/// the current day on (`"YYYYMMDD:HHMM-YYYYMMDD:HHMM;YYYYMMDD:CLOSED;..."`).
#[derive(Clone, Debug)]
pub struct ContractDetails {
    pub contract: Contract,
    pub market_name: String,
    pub min_tick: f64,
    pub order_types: String,
    pub valid_exchanges: String,
    pub price_magnifier: i32,
    pub under_con_id: i32,
    pub long_name: String,
    pub contract_month: String,
    pub industry: String,
    pub category: String,
    pub subcategory: String,
    pub time_zone_id: String,
    pub trading_hours: String,
    pub liquid_hours: String,
    pub ev_rule: String,
    pub ev_multiplier: f64,
    pub agg_group: i32,
    pub under_symbol: String,
    pub under_sec_type: String,
    pub market_rule_ids: String,
    pub sec_id_list: Vec<TagValue>,
    pub real_expiration_date: String,
    pub last_trade_time: String,
    pub stock_type: String,
    pub min_size: f64,
    pub size_increment: f64,
    pub suggested_size_increment: f64,
    pub min_algo_size: f64,
    pub last_price_precision: f64,
    pub last_size_precision: f64,
    // Bond values
    pub cusip: String,
    pub ratings: String,
    pub desc_append: String,
    pub bond_type: String,
    pub coupon_type: String,
    pub callable: bool,
    pub putable: bool,
    pub coupon: f64,
    pub convertible: bool,
    pub maturity: String,
    pub issue_date: String,
    pub next_option_date: String,
    pub next_option_type: String,
    pub next_option_partial: bool,
    pub notes: String,
    // Fund values
    pub fund_name: String,
    pub fund_family: String,
    pub fund_type: String,
    pub fund_front_load: String,
    pub fund_back_load: String,
    pub fund_back_load_time_interval: String,
    pub fund_management_fee: String,
    pub fund_closed: bool,
    pub fund_closed_for_new_investors: bool,
    pub fund_closed_for_new_money: bool,
    pub fund_notify_amount: String,
    pub fund_minimum_initial_purchase: String,
    pub fund_subsequent_minimum_purchase: String,
    pub fund_blue_sky_states: String,
    pub fund_blue_sky_territories: String,
    /// The API value of the policy (`N`, `Y`), empty when none.
    pub fund_distribution_policy_indicator: String,
    /// The API value of the asset type (`000`, `001`, ...), empty when none.
    pub fund_asset_type: String,
    pub ineligibility_reason_list: Vec<IneligibilityReason>,
    pub event_contract1: String,
    pub event_contract_description1: String,
    pub event_contract_description2: String,
}

impl Default for ContractDetails {
    fn default() -> Self {
        Self {
            contract: Contract::default(),
            market_name: String::new(),
            min_tick: 0.0,
            order_types: String::new(),
            valid_exchanges: String::new(),
            price_magnifier: 0,
            under_con_id: 0,
            long_name: String::new(),
            contract_month: String::new(),
            industry: String::new(),
            category: String::new(),
            subcategory: String::new(),
            time_zone_id: String::new(),
            trading_hours: String::new(),
            liquid_hours: String::new(),
            ev_rule: String::new(),
            ev_multiplier: 0.0,
            agg_group: 0,
            under_symbol: String::new(),
            under_sec_type: String::new(),
            market_rule_ids: String::new(),
            sec_id_list: Vec::new(),
            real_expiration_date: String::new(),
            last_trade_time: String::new(),
            stock_type: String::new(),
            min_size: f64::MAX,
            size_increment: f64::MAX,
            suggested_size_increment: f64::MAX,
            min_algo_size: f64::MAX,
            last_price_precision: f64::MAX,
            last_size_precision: f64::MAX,
            cusip: String::new(),
            ratings: String::new(),
            desc_append: String::new(),
            bond_type: String::new(),
            coupon_type: String::new(),
            callable: false,
            putable: false,
            coupon: 0.0,
            convertible: false,
            maturity: String::new(),
            issue_date: String::new(),
            next_option_date: String::new(),
            next_option_type: String::new(),
            next_option_partial: false,
            notes: String::new(),
            fund_name: String::new(),
            fund_family: String::new(),
            fund_type: String::new(),
            fund_front_load: String::new(),
            fund_back_load: String::new(),
            fund_back_load_time_interval: String::new(),
            fund_management_fee: String::new(),
            fund_closed: false,
            fund_closed_for_new_investors: false,
            fund_closed_for_new_money: false,
            fund_notify_amount: String::new(),
            fund_minimum_initial_purchase: String::new(),
            fund_subsequent_minimum_purchase: String::new(),
            fund_blue_sky_states: String::new(),
            fund_blue_sky_territories: String::new(),
            fund_distribution_policy_indicator: String::new(),
            fund_asset_type: String::new(),
            ineligibility_reason_list: Vec::new(),
            event_contract1: String::new(),
            event_contract_description1: String::new(),
            event_contract_description2: String::new(),
        }
    }
}

/// Minimum size, size increment and suggested size increment of a row
/// (`jextend.dL.a/b/c(dy, o)`): from the size set of the record's market
/// rule; for a contract with fractional sizes (a rule increment below 1,
/// or detail 8193) the fractional increment (the rule's when below 1,
/// else detail 8175, else 1/10000) is the minimum and the increment.
fn size_fields(def: &crate::control::contracts::ContractDefinition, fund: bool) -> (f64, f64, f64) {
    let min_of = |f: fn(&crate::control::contracts::PriceIncrement) -> f64| {
        def.size_increments.iter().map(f).fold(None, |m: Option<f64>, v| Some(m.map_or(v, |m| m.min(v))))
    };
    let (Some(min_low), Some(min_inc)) = (min_of(|p| p.low_edge), min_of(|p| p.increment)) else {
        return (f64::MAX, f64::MAX, f64::MAX);
    };
    let fractional_rule = min_inc < 1.0;
    if !fund && (fractional_rule || def.fractional) {
        let size = if fractional_rule { min_inc } else { def.fraction_size.unwrap_or(0.0001) };
        return (size, size, min_inc);
    }
    (min_low, min_inc, min_inc)
}

/// EV rule text of a row (`feature.evrule.h.b(h)`): `name:value`.
fn ev_rule_text(raw: &str) -> String {
    if raw.is_empty() {
        return String::new();
    }
    match raw.split_once(':') {
        Some((name, value)) => format!("{}:{}", name, value.split(':').next().unwrap_or("")),
        None => format!("{}:", raw),
    }
}

impl ContractDetails {
    /// The API row of a definition, as the reference fills it (ibx#436,
    /// `jextend.protobuf.outgoing.a.a(dy, o, lh.D, String, boolean)` and the
    /// client's decoding). A bond row without the BONDAPI feature has most
    /// bond texts empty and its short description as `desc_append`.
    pub fn from_definition(def: &crate::control::contracts::ContractDefinition) -> Self {
        let bond = def.is_bond();
        let sec_type = def.api_sec_type();
        let shown = |s: &str| if !bond || def.bond_api { s.to_string() } else { String::new() };
        // Last trade date and time: from the schedule, else the date alone.
        let (expiry, expiry_time) = def.schedule_expiry.clone()
            .unwrap_or_else(|| (def.last_trade_date.clone(), String::new()));
        let multiplier = if def.multiplier_text.is_empty() {
            String::new()
        } else {
            def.multiplier_text.trim().parse::<f64>().map(|m| format!("{}", m)).unwrap_or_default()
        };
        let c = Contract {
            con_id: def.con_id,
            symbol: shown(&def.symbol),
            sec_type: sec_type.clone(),
            exchange: def.exchange.clone(),
            primary_exchange: def.primary_exchange.clone(),
            currency: shown(&def.currency),
            local_symbol: def.local_symbol.clone(),
            trading_class: def.trading_class.clone(),
            // A bond's date is its maturity (the client's decoding).
            last_trade_date_or_contract_month: if bond { String::new() } else { expiry.clone() },
            last_trade_date: shown(&def.last_trade_date),
            strike: def.strike,
            right: match def.right {
                Some(crate::control::contracts::OptionRight::Call) => "C".to_string(),
                Some(crate::control::contracts::OptionRight::Put) => "P".to_string(),
                None => String::new(),
            },
            multiplier,
            ..Default::default()
        };
        let mut sec_id_list = Vec::new();
        if !def.isin.is_empty() {
            sec_id_list.push(TagValue { tag: "ISIN".into(), value: def.isin.clone() });
        }
        // The CUSIP when it is not an internal id (`jsecdef.cm.ak()`).
        if !def.cusip.is_empty() && !def.cusip.starts_with("IBCID") {
            sec_id_list.push(TagValue { tag: "CUSIP".into(), value: def.cusip.clone() });
        }
        let fund = sec_type == "FUND";
        let (min_size, size_increment, suggested_size_increment) = size_fields(def, fund);
        let mut d = Self {
            contract: c,
            market_name: shown(&def.market_name),
            min_tick: def.min_tick,
            // Each order type without its flags (`jibtypes.i.aK()`).
            order_types: def.order_types.iter()
                .map(|t| t.split('/').next().unwrap_or(""))
                .collect::<Vec<_>>()
                .join(","),
            valid_exchanges: def.valid_exchanges.join(","),
            price_magnifier: def.price_magnifier,
            under_con_id: if def.under_con_id > 0 && def.under_con_id < i32::MAX as i64 { def.under_con_id as i32 } else { 0 },
            long_name: shown(&def.long_name),
            contract_month: def.contract_month.clone(),
            industry: def.industry.clone(),
            category: def.category.clone(),
            subcategory: def.subcategory.clone(),
            time_zone_id: def.time_zone_id.clone().unwrap_or_default(),
            trading_hours: def.trading_hours.clone().unwrap_or_default(),
            liquid_hours: def.liquid_hours.clone().unwrap_or_default(),
            ev_rule: if def.ev_api { ev_rule_text(&def.ev_rule) } else { String::new() },
            agg_group: def.agg_group.filter(|g| *g != -1 && *g != i32::MAX).unwrap_or(0),
            under_symbol: def.under_symbol.clone(),
            under_sec_type: def.under_sec_type.clone(),
            market_rule_ids: def.market_rule_ids.clone(),
            sec_id_list,
            real_expiration_date: def.real_expiration_date.clone(),
            last_trade_time: if bond && !def.bond_api { String::new() } else { expiry_time },
            stock_type: def.stock_type.clone(),
            min_size,
            size_increment,
            suggested_size_increment,
            ineligibility_reason_list: def.ineligibility.iter()
                .map(|(id, description)| IneligibilityReason { id: id.clone(), description: description.clone() })
                .collect(),
            ..Default::default()
        };
        if fund {
            let f = &def.fund;
            d.fund_name = f.name.clone();
            d.fund_family = f.family.clone();
            d.fund_type = f.fund_type.clone();
            d.fund_front_load = f.front_load.clone();
            d.fund_back_load = f.back_load.clone();
            d.fund_back_load_time_interval = f.back_load_time_interval.clone();
            d.fund_management_fee = f.management_fee.clone();
            d.fund_closed = f.closed;
            d.fund_closed_for_new_investors = f.closed_for_new_investors;
            d.fund_closed_for_new_money = f.closed_for_new_money;
            d.fund_notify_amount = f.notify_amount.clone();
            d.fund_minimum_initial_purchase = f.minimum_initial_purchase.clone();
            d.fund_subsequent_minimum_purchase = f.subsequent_minimum_purchase.clone();
            d.fund_blue_sky_states = f.blue_sky_states.clone();
            d.fund_blue_sky_territories = f.blue_sky_territories.clone();
            d.fund_distribution_policy_indicator = f.distribution_policy_indicator.clone();
            d.fund_asset_type = f.asset_type.clone();
        }
        if bond {
            let b = &def.bond;
            let api = def.bond_api;
            // The request symbol when it reads as a CUSIP, else the local
            // symbol (`jextend.dK.a(dy, String)`; the CUSIP feature of the
            // account is not known here).
            d.cusip = if def.lookup_cusip.is_empty() { def.local_symbol.clone() } else { def.lookup_cusip.clone() };
            d.maturity = if api { expiry } else { String::new() };
            d.issue_date = shown(&b.issue_date);
            d.bond_type = shown(&b.bond_type);
            d.coupon = if api { b.coupon.trim().parse().unwrap_or(0.0) } else { 0.0 };
            d.coupon_type = shown(&b.coupon_type);
            d.convertible = api && b.convertible;
            d.callable = api && b.callable;
            d.putable = api && b.putable;
            d.desc_append = if api { b.desc_append.clone() } else { b.short_description.clone() };
            d.next_option_date = shown(&b.next_option_date);
            d.next_option_type = shown(&b.next_option_type);
            d.next_option_partial = api && b.next_option_partial;
            d.notes = shown(&b.notes);
        }
        d
    }
}

// ── ContractDescription ──

/// ibapi-compatible ContractDescription for symbol search results.
#[derive(Clone, Debug, Default)]
pub struct ContractDescription {
    pub con_id: i64,
    pub symbol: String,
    pub sec_type: String,
    pub currency: String,
    pub primary_exchange: String,
    pub derivative_sec_types: Vec<String>,
    pub description: String,
    pub issuer_id: String,
}

// ── PriceIncrement (for market rules) ──

/// ibapi-compatible PriceIncrement for market_rule callback.
#[derive(Clone, Debug)]
pub struct PriceIncrement {
    pub low_edge: f64,
    pub increment: f64,
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
        assert_eq!(o.tif, "DAY");
        assert!(o.transmit);
        assert!(!o.what_if);
        assert!(!o.outside_rth);
    }

    #[test]
    fn order_side_parsing() {
        let mut o = Order::default();
        o.action = "BUY".into();
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

    // ibx#417: the clearing intent and the short-sale instructions reach
    // the engine, through the extended path.
    #[test]
    fn short_sale_instructions_reach_the_attributes() {
        let plain = Order { action: "SSHORT".into(), ..Default::default() };
        assert!(!plain.has_extended_attrs());
        assert_eq!(plain.attrs().short_sale, ShortSale::default());
        let o = Order {
            action: "SSHORT".into(), clearing_intent: "Away".into(), short_sale_slot: 2,
            designated_location: "XYZ".into(), exempt_code: 3, ..Default::default()
        };
        assert!(o.has_extended_attrs());
        let a = o.attrs();
        assert_eq!(a.clearing_intent, "Away");
        assert_eq!(a.short_sale, ShortSale { slot: 2, location: "XYZ".into(), exempt_code: 3 });
    }

    #[test]
    fn order_side_invalid() {
        let mut o = Order::default();
        o.action = "INVALID".into();
        assert!(o.side().is_err());
    }

    #[test]
    fn order_tif_byte_mapping() {
        let mut o = Order::default();
        o.tif = "DAY".into();
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
        o.tif = "DTC".into();
        assert_eq!(o.tif_byte(), crate::types::TIF_DTC);
    }

    // ibx#467: goodAfterTime reaches the attributes as a UTC instant.
    #[test]
    fn good_after_time_is_an_instant() {
        let o = Order { good_after_time: "20261230 09:30:00 US/Eastern".into(), ..Default::default() };
        assert_eq!(o.attrs().good_after, 1_798_641_000); // 20261230 14:30:00 UTC
        let o = Order { good_after_time: "20261230-14:30:00".into(), ..Default::default() };
        assert_eq!(o.attrs().good_after, 1_798_641_000);
    }

    #[test]
    fn order_has_extended_attrs() {
        let o = Order::default();
        assert!(!o.has_extended_attrs());

        let mut o2 = Order::default();
        o2.hidden = true;
        assert!(o2.has_extended_attrs());

        let mut o3 = Order::default();
        o3.display_size = 50;
        assert!(o3.has_extended_attrs());
    }

    // ibx#492: usePriceMgmtAlgo is unset by default, as the API; a value
    // the caller sets rides the attributes.
    #[test]
    fn price_management_value_is_unset_by_default() {
        let o = Order::default();
        assert_eq!(o.use_price_mgmt_algo, i32::MAX);
        assert_eq!(o.attrs().use_price_mgmt_algo, None);
        for (v, want) in [(0, Some(false)), (1, Some(true))] {
            let o = Order { use_price_mgmt_algo: v, ..Default::default() };
            assert!(o.has_extended_attrs());
            assert_eq!(o.attrs().use_price_mgmt_algo, want);
        }
    }

    #[test]
    fn order_attrs_conversion() {
        let mut o = Order::default();
        o.display_size = 50;
        o.hidden = true;
        o.discretionary_amt = 0.05;
        let attrs = o.attrs();
        assert_eq!(attrs.display_size, 50);
        assert!(attrs.hidden);
        assert_eq!(attrs.discretionary_amt, (0.05 * PRICE_SCALE_F) as Price);
    }

    #[test]
    fn order_attrs_conditions_forwarded() {
        let mut o = Order::default();
        o.conditions = vec![
            OrderCondition::Time { time: "20260311-09:30:00".into(), is_more: true },
        ];
        o.conditions_cancel_order = true;
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

    #[test]
    fn order_state_default() {
        let os = OrderState::default();
        assert_eq!(os.status, "");
        assert_eq!(os.commission_and_fees, 0.0);
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

    // ibx#223: the raw cast wrapped -1 to 255 and forwarded out-of-range
    // trigger codes to the wire verbatim.
    #[test]
    fn attrs_trigger_method_coerces_invalid_codes() {
        for (input, expected) in [(-1, 0u8), (5, 0), (6, 0), (9, 0), (255, 0),
                                  (0, 0), (2, 2), (4, 4), (7, 7), (8, 8)] {
            let o = Order { trigger_method: input, ..Default::default() };
            assert_eq!(o.attrs().trigger_method, expected, "input {}", input);
        }
    }

    // ibx#230: the reported Contract must round-trip — sec_type is the
    // official API string, and market_name is no longer thrown away.
    #[test]
    fn contract_details_from_definition_round_trips() {
        let def = crate::control::contracts::ContractDefinition {
            con_id: 265598,
            symbol: "AAPL".into(),
            sec_type: crate::control::contracts::SecurityType::Stock,
            market_name: "NMS".into(),
            ..Default::default()
        };
        let details = ContractDetails::from_definition(&def);
        assert_eq!(details.contract.sec_type, "STK", "not the Debug derive 'Stock'");
        assert_eq!(details.market_name, "NMS");
        // Unclassifiable instruments must not claim to be stocks.
        let def = crate::control::contracts::ContractDefinition {
            sec_type: crate::control::contracts::SecurityType::Other,
            ..Default::default()
        };
        assert_eq!(ContractDetails::from_definition(&def).contract.sec_type, "");
    }

    // ── ibx#436: the API row of a definition ──

    fn bond_definition() -> crate::control::contracts::ContractDefinition {
        use crate::control::contracts::*;
        ContractDefinition {
            con_id: 29105555,
            symbol: "IBM".into(),
            sec_type: SecurityType::Bond,
            wire_sec_type: "BOND".into(),
            exchange: "SMART".into(),
            currency: "USD".into(),
            local_symbol: "IBCID29105555".into(),
            market_name: "IBM".into(),
            long_name: "International Business Machines Corp".into(),
            last_trade_date: "20451030".into(),
            cusip: "459200AN1".into(),
            isin: "US459200AN17".into(),
            bond: BondFields {
                notes: "Notes".into(), desc_append: "Append".into(), short_description: "IBM 7 10/30/45".into(),
                bond_type: "Bonds".into(), coupon_type: "FIXED".into(), callable: true, putable: true, convertible: true,
                next_option_partial: true, next_option_date: "20270801".into(), next_option_type: "P".into(),
                ratings: "A3".into(), coupon: "7".into(), issue_date: "19951030".into(),
            },
            schedule_expiry: Some(("20451030".into(), String::new())),
            ..Default::default()
        }
    }

    // Without BONDAPI (the paper logon): most bond texts empty, the short
    // description as descAppend, the local symbol as CUSIP, ISIN and CUSIP
    // in the id list (captured 02/10/2026).
    #[test]
    fn bond_row_without_the_bond_feature() {
        let d = ContractDetails::from_definition(&bond_definition());
        assert_eq!((d.contract.symbol.as_str(), d.contract.currency.as_str(), d.market_name.as_str(), d.long_name.as_str()), ("", "", "", ""));
        assert_eq!((d.contract.last_trade_date_or_contract_month.as_str(), d.contract.last_trade_date.as_str(), d.maturity.as_str()), ("", "", ""));
        assert_eq!((d.cusip.as_str(), d.desc_append.as_str()), ("IBCID29105555", "IBM 7 10/30/45"));
        assert_eq!((d.bond_type.as_str(), d.coupon_type.as_str(), d.notes.as_str(), d.issue_date.as_str()), ("", "", "", ""));
        assert_eq!((d.callable, d.putable, d.convertible, d.next_option_partial, d.coupon), (false, false, false, false, 0.0));
        assert_eq!(d.ratings, "");
        assert_eq!(d.sec_id_list, vec![
            TagValue { tag: "ISIN".into(), value: "US459200AN17".into() },
            TagValue { tag: "CUSIP".into(), value: "459200AN1".into() },
        ]);
    }

    // With BONDAPI: the bond fields, the maturity, the description append.
    #[test]
    fn bond_row_with_the_bond_feature() {
        let mut def = bond_definition();
        def.bond_api = true;
        def.lookup_cusip = "459200AN1".into();
        let d = ContractDetails::from_definition(&def);
        assert_eq!((d.contract.symbol.as_str(), d.contract.currency.as_str(), d.long_name.as_str()), ("IBM", "USD", "International Business Machines Corp"));
        assert_eq!((d.maturity.as_str(), d.contract.last_trade_date_or_contract_month.as_str(), d.contract.last_trade_date.as_str()), ("20451030", "", "20451030"));
        assert_eq!((d.cusip.as_str(), d.desc_append.as_str()), ("459200AN1", "Append"));
        assert_eq!((d.bond_type.as_str(), d.coupon_type.as_str(), d.notes.as_str(), d.issue_date.as_str()), ("Bonds", "FIXED", "Notes", "19951030"));
        assert_eq!((d.callable, d.putable, d.convertible, d.next_option_partial, d.coupon), (true, true, true, true, 7.0));
        assert_eq!((d.next_option_date.as_str(), d.next_option_type.as_str()), ("20270801", "P"));
        // An internal id is not a CUSIP of the id list.
        def.cusip = "IBCID29105555".into();
        assert_eq!(ContractDetails::from_definition(&def).sec_id_list.len(), 1);
    }

    // Sizes from the record's rule; a fractional contract has the
    // fractional increment as minimum and increment (captured AAPL:
    // 0.0001, 0.0001, 40).
    #[test]
    fn size_fields_of_a_row() {
        use crate::control::contracts::{ContractDefinition, PriceIncrement};
        let set = |v: &[(f64, f64)]| v.iter().map(|&(low_edge, increment)| PriceIncrement { low_edge, increment }).collect::<Vec<_>>();
        let mut def = ContractDefinition { wire_sec_type: "STK".into(), size_increments: set(&[(40.0, 40.0)]), fractional: true, ..Default::default() };
        let d = ContractDetails::from_definition(&def);
        assert_eq!((d.min_size, d.size_increment, d.suggested_size_increment), (0.0001, 0.0001, 40.0));
        def.fraction_size = Some(0.01);
        assert_eq!(ContractDetails::from_definition(&def).min_size, 0.01);
        def.fractional = false;
        let d = ContractDetails::from_definition(&def);
        assert_eq!((d.min_size, d.size_increment, d.suggested_size_increment), (40.0, 40.0, 40.0));
        def.size_increments = set(&[(0.0, 0.5), (10.0, 1.0)]);
        let d = ContractDetails::from_definition(&def);
        assert_eq!((d.min_size, d.size_increment, d.suggested_size_increment), (0.5, 0.5, 0.5));
        def.size_increments.clear();
        assert_eq!(ContractDetails::from_definition(&def).min_size, f64::MAX);
        assert_eq!(ContractDetails::default().min_algo_size, f64::MAX);
    }

    // Order types without their flags, the multiplier as received, the
    // right as C or P, the last trade time of the schedule, the fund
    // fields only for a fund, the EV rule only with EVAPI.
    #[test]
    fn texts_of_a_row() {
        use crate::control::contracts::{ContractDefinition, FundFields, OptionRight};
        let def = ContractDefinition {
            wire_sec_type: "OPT".into(),
            order_types: vec!["ACTIVETIM/1".into(), "AD/5".into(), "LMT/3".into()],
            multiplier_text: "100".into(),
            multiplier: 100.0,
            right: Some(OptionRight::Put),
            last_trade_date: "20261005".into(),
            schedule_expiry: Some(("20261005".into(), "16:00:00".into())),
            ev_rule: "factor:0.5".into(),
            fund: FundFields { name: "Name".into(), ..Default::default() },
            agg_group: Some(2),
            under_con_id: 265598,
            price_magnifier: 1,
            ..Default::default()
        };
        let d = ContractDetails::from_definition(&def);
        assert_eq!(d.order_types, "ACTIVETIM,AD,LMT");
        assert_eq!((d.contract.multiplier.as_str(), d.contract.right.as_str()), ("100", "P"));
        assert_eq!((d.contract.last_trade_date_or_contract_month.as_str(), d.last_trade_time.as_str()), ("20261005", "16:00:00"));
        assert_eq!((d.agg_group, d.under_con_id, d.price_magnifier), (2, 265598, 1));
        assert_eq!((d.fund_name.as_str(), d.ev_rule.as_str()), ("", ""));
        let fund = ContractDefinition { wire_sec_type: "FUND".into(), ev_api: true, ..def.clone() };
        let d = ContractDetails::from_definition(&fund);
        assert_eq!((d.fund_name.as_str(), d.ev_rule.as_str()), ("Name", "factor:0.5"));
        let no_multiplier = ContractDefinition { multiplier_text: String::new(), ..def };
        assert_eq!(ContractDetails::from_definition(&no_multiplier).contract.multiplier, "");
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
}
