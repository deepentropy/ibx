//! ibapi-compatible types: Contract, Order, OrderState, Execution, TagValue, BarData,
//! ContractDetails, ContractDescription, and order conditions.
//!
//! These are plain Rust structs (no PyO3) shared by both the Rust EClient and the Python bridge.

use crate::types::*;

pub const PRICE_SCALE_F: f64 = PRICE_SCALE as f64;

// ── ComboLeg ──

/// ibapi-compatible ComboLeg for combination orders.
#[derive(Clone, Debug, Default)]
pub struct ComboLeg {
    pub con_id: i64,
    pub ratio: i32,
    pub action: String,
    pub exchange: String,
    pub open_close: i32,
    pub shorting_policy: i32,
    pub designated_location: String,
    pub exempt_code: i32,
}

// ── DeltaNeutralContract ──

/// ibapi-compatible DeltaNeutralContract for delta-neutral orders.
#[derive(Clone, Debug, Default)]
pub struct DeltaNeutralContract {
    pub con_id: i64,
    pub delta: f64,
    pub price: f64,
}

// ── Contract ──

/// ibapi-compatible Contract. Matches C++ `Contract` struct fields.
#[derive(Clone, Debug, Default)]
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
            use_price_mgmt_algo: 0,
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
            "GTD" | "DTC" => b'6',
            "AUC" => b'8',
            _ => b'0', // DAY
        }
    }

    /// Build OrderAttrs from Order fields.
    pub fn attrs(&self) -> OrderAttrs {
        OrderAttrs {
            display_size: self.display_size.max(0) as u32,
            min_qty: self.min_qty.max(0) as u32,
            hidden: self.hidden,
            outside_rth: self.outside_rth,
            good_after: 0,
            good_till: 0,
            oca_group: self.oca_group.parse().unwrap_or(0),
            discretionary_amt: (self.discretionary_amt * PRICE_SCALE_F) as Price,
            sweep_to_fill: self.sweep_to_fill,
            all_or_none: self.all_or_none,
            trigger_method: self.trigger_method as u8,
            cash_qty: (self.cash_qty * PRICE_SCALE_F) as Price,
            conditions: self.conditions.clone(),
            conditions_cancel_order: self.conditions_cancel_order,
            conditions_ignore_rth: self.conditions_ignore_rth,
        }
    }

    /// Check if the order has any extended attributes set.
    pub fn has_extended_attrs(&self) -> bool {
        self.display_size > 0
            || self.min_qty > 0
            || self.hidden
            || self.outside_rth
            || !self.good_after_time.is_empty()
            || !self.good_till_date.is_empty()
            || !self.oca_group.is_empty()
            || self.discretionary_amt > 0.0
            || self.sweep_to_fill
            || self.all_or_none
            || self.trigger_method > 0
            || self.cash_qty > 0.0
    }
}

// ── TagValue ──

/// ibapi-compatible TagValue for algo parameters.
#[derive(Clone, Debug)]
pub struct TagValue {
    pub tag: String,
    pub value: String,
}

// ── OrderState ──

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
    pub commission: f64,
    pub min_commission: f64,
    pub max_commission: f64,
    pub commission_currency: String,
    pub warning_text: String,
    pub completed_time: String,
    pub completed_status: String,
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

// ── CommissionReport ──

/// ibapi-compatible CommissionReport.
#[derive(Clone, Debug, Default)]
pub struct CommissionReport {
    pub exec_id: String,
    pub commission: f64,
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
#[derive(Clone, Debug, Default)]
pub struct TickAttribLast {
    pub past_limit: bool,
    pub unreported: bool,
}

// ── TickAttribBidAsk ──

/// ibapi-compatible TickAttribBidAsk for tick_by_tick_bid_ask callback.
#[derive(Clone, Debug, Default)]
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
        }
    }
}

// ── ContractDetails ──

/// ibapi-compatible ContractDetails.
#[derive(Clone, Debug, Default)]
pub struct ContractDetails {
    pub contract: Contract,
    pub market_name: String,
    pub min_tick: f64,
    pub order_types: String,
    pub valid_exchanges: String,
    pub long_name: String,
    pub last_trade_date: String,
    pub multiplier: String,
}

impl ContractDetails {
    pub fn from_definition(def: &crate::control::contracts::ContractDefinition) -> Self {
        let c = Contract {
            con_id: def.con_id as i64,
            symbol: def.symbol.clone(),
            sec_type: match def.sec_type {
                crate::control::contracts::SecurityType::Stock => "STK",
                crate::control::contracts::SecurityType::Option => "OPT",
                crate::control::contracts::SecurityType::Future => "FUT",
                crate::control::contracts::SecurityType::Forex => "CASH",
                crate::control::contracts::SecurityType::Index => "IND",
                crate::control::contracts::SecurityType::Bond => "BOND",
                crate::control::contracts::SecurityType::Warrant => "WAR",
                _ => "STK",
            }.to_string(),
            exchange: def.exchange.clone(),
            primary_exchange: def.primary_exchange.clone(),
            currency: def.currency.clone(),
            local_symbol: def.local_symbol.clone(),
            trading_class: def.trading_class.clone(),
            last_trade_date_or_contract_month: def.last_trade_date.clone(),
            strike: def.strike,
            multiplier: if def.multiplier != 1.0 { format!("{}", def.multiplier) } else { String::new() },
            ..Default::default()
        };
        Self {
            contract: c,
            market_name: String::new(),
            min_tick: def.min_tick,
            order_types: def.order_types.join(","),
            valid_exchanges: def.valid_exchanges.join(","),
            long_name: def.long_name.clone(),
            last_trade_date: def.last_trade_date.clone(),
            multiplier: if def.multiplier != 1.0 { format!("{}", def.multiplier) } else { String::new() },
        }
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
        assert_eq!(os.commission, 0.0);
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

    // ── ContractDescription ──

    #[test]
    fn contract_description_default() {
        let cd = ContractDescription::default();
        assert_eq!(cd.con_id, 0);
        assert_eq!(cd.symbol, "");
    }

    // ── CommissionReport ──

    #[test]
    fn commission_report_default() {
        let cr = CommissionReport::default();
        assert_eq!(cr.exec_id, "");
        assert_eq!(cr.commission, 0.0);
    }

    // ── PriceIncrement ──

    #[test]
    fn price_increment_fields() {
        let pi = PriceIncrement { low_edge: 0.0, increment: 0.01 };
        assert_eq!(pi.low_edge, 0.0);
        assert_eq!(pi.increment, 0.01);
    }
}
