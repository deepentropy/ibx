//! ibx#597, its closed-market part: the order cases of the #551 case script
//! (`.tmp/551/case.py`) run through the Rust client, one session per case,
//! each case printed in the rows the case script prints, so that
//! `compare_logs.py` compares them with the reference's logs
//! (`.tmp/551/gateway_<CASE>.log`) and ibx's Python logs
//! (`.tmp/551/ibx_<CASE>.log`).
//!
//!     cargo run --release --example ex597_order_cases_closed -- K M P AA Z S Y
//!
//! Options: `--base <id>` the first order id, for one case at a time
//! (e.g. the first id of the reference's log, so that the ids of the rows
//! match; else as the case script: the larger of the next valid id and
//! 9001 + seconds % 50000);
//! `--out <dir>` where `rust_<CASE>.log` is written (default `.tmp/551`);
//! `--pause <s>` between two sessions (default 10).
//!
//! Cases (the letters of the case script): K L M N P Q R S T U V W X Y Z
//! AA AB AC AD AE. Every order is far from the market (BUY at 100 or
//! less, SELL at 400, or refused); what a case leaves working is
//! cancelled once its rows are printed, outside the rows.
//!
//! Paper account only: it stops unless the account id starts with DU.
//! Paper: IB_USERNAME, IB_PASSWORD (IB_HOST optional).
use std::collections::HashMap;
use std::env;
use std::fmt::Write as _;
use std::io::Write as _;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ibx::api::client::{EClient, EClientConfig};
use ibx::api::types::{CommissionAndFeesReport, Contract, Execution, Order, OrderState};
use ibx::api::wrapper::Wrapper;

const CASES: [&str; 20] = [
    "K", "L", "M", "N", "P", "Q", "R", "S", "T", "U", "V", "W", "X", "Y", "Z", "AA", "AB", "AC", "AD", "AE",
];

// ── The rows of the case script: Python's repr of each value ──

/// Python's `repr` of a text.
fn py_repr(s: &str) -> String {
    let q = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(q);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == q => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || (0x7f..=0xa0).contains(&(c as u32)) || c as u32 == 0xad => {
                let _ = write!(out, "\\x{:02x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push(q);
    out
}

/// Python's `repr` of a float.
fn py_float(v: f64) -> String {
    if v.is_nan() {
        return "nan".into();
    }
    if v.is_infinite() {
        return if v > 0.0 { "inf".into() } else { "-inf".into() };
    }
    let a = v.abs();
    if a != 0.0 && !(1e-4..1e16).contains(&a) {
        let s = format!("{v:e}");
        let (m, e) = s.split_once('e').unwrap_or((&s, "0"));
        let (sign, digits) = match e.strip_prefix('-') {
            Some(d) => ('-', d),
            None => ('+', e),
        };
        return format!("{m}e{sign}{digits:0>2}");
    }
    format!("{v:?}")
}

/// `str()` of the API's Decimal as the case script prints a quantity.
fn py_decimal_str(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 { format!("{}", v as i64) } else { format!("{v}") }
}

/// The case script's `show()` of a Decimal: no attribute when zero, else
/// its `real`.
fn show_decimal(v: f64) -> &'static str {
    if v == 0.0 { "Decimal{}" } else { "Decimal{'real': 'Decimal'}" }
}

fn cut(s: String, n: usize) -> String {
    if s.chars().count() <= n { s } else { s.chars().take(n).collect() }
}

fn show_str(s: &str) -> String {
    cut(py_repr(s), 120)
}

/// The case script's `show()` of an object: its attributes that are set,
/// sorted by name, under the official API's names, 600 characters at most.
#[derive(Default)]
struct Dump(Vec<(&'static str, String)>);

impl Dump {
    fn s(&mut self, k: &'static str, v: &str) -> &mut Self {
        if !v.is_empty() { self.0.push((k, py_repr(v))); }
        self
    }
    fn i(&mut self, k: &'static str, v: i64) -> &mut Self {
        if v != 0 { self.0.push((k, v.to_string())); }
        self
    }
    fn f(&mut self, k: &'static str, v: f64) -> &mut Self {
        if v != 0.0 { self.0.push((k, py_float(v))); }
        self
    }
    fn b(&mut self, k: &'static str, v: bool) -> &mut Self {
        if v { self.0.push((k, "True".into())); }
        self
    }
    /// A value that is not a number, a text or a bool: its type's name.
    fn obj(&mut self, k: &'static str, present: bool, type_name: &str) -> &mut Self {
        if present { self.0.push((k, py_repr(type_name))); }
        self
    }
    fn dec(&mut self, k: &'static str, v: f64) -> &mut Self {
        self.obj(k, v != 0.0, "Decimal")
    }
    /// An int the Python object holds as None when unset.
    fn opt_i(&mut self, k: &'static str, v: i32) -> &mut Self {
        if v != i32::MAX { self.i(k, v as i64); }
        self
    }
    fn finish(&mut self, name: &str) -> String {
        self.0.sort_by(|a, b| a.0.cmp(b.0));
        let body: Vec<String> = self.0.iter().map(|(k, v)| format!("'{k}': {v}")).collect();
        cut(format!("{name}{{{}}}", body.join(", ")), 600)
    }
}

fn dump_contract(c: &Contract) -> String {
    Dump::default()
        .obj("comboLegs", !c.combo_legs.is_empty(), "list")
        .s("comboLegsDescrip", &c.combo_legs_descrip)
        .i("conId", c.con_id)
        .s("currency", &c.currency)
        .obj("deltaNeutralContract", c.delta_neutral_contract.is_some(), "DeltaNeutralContract")
        .s("description", &c.description)
        .s("exchange", &c.exchange)
        .b("includeExpired", c.include_expired)
        .s("issuerId", &c.issuer_id)
        .s("lastTradeDate", &c.last_trade_date)
        .s("lastTradeDateOrContractMonth", &c.last_trade_date_or_contract_month)
        .s("localSymbol", &c.local_symbol)
        .s("multiplier", &c.multiplier)
        .s("primaryExchange", &c.primary_exchange)
        .s("right", &c.right)
        .s("secId", &c.sec_id)
        .s("secIdType", &c.sec_id_type)
        .s("secType", &c.sec_type)
        .f("strike", c.strike)
        .s("symbol", &c.symbol)
        .s("tradingClass", &c.trading_class)
        .finish("Contract")
}

fn dump_order(o: &Order) -> String {
    let mut d = Dump::default();
    d.s("account", &o.account).s("action", &o.action).s("activeStartTime", &o.active_start_time)
        .s("activeStopTime", &o.active_stop_time).i("adjustableTrailingUnit", o.adjustable_trailing_unit as i64)
        .s("adjustedOrderType", &o.adjusted_order_type).f("adjustedStopLimitPrice", o.adjusted_stop_limit_price)
        .f("adjustedStopPrice", o.adjusted_stop_price).f("adjustedTrailingAmount", o.adjusted_trailing_amount)
        .s("advancedErrorOverride", &o.advanced_error_override).s("algoId", &o.algo_id)
        .obj("algoParams", !o.algo_params.is_empty(), "list").s("algoStrategy", &o.algo_strategy)
        .b("allOrNone", o.all_or_none).b("allowPreOpen", o.allow_pre_open)
        .i("auctionStrategy", o.auction_strategy as i64).s("autoCancelDate", &o.auto_cancel_date)
        .b("autoCancelParent", o.auto_cancel_parent).f("auxPrice", o.aux_price).f("basisPoints", o.basis_points)
        .i("basisPointsType", o.basis_points_type as i64).b("blockOrder", o.block_order)
        .s("bondAccruedInterest", &o.bond_accrued_interest).f("cashQty", o.cash_qty)
        .s("clearingAccount", &o.clearing_account).s("clearingIntent", &o.clearing_intent)
        .i("clientId", o.client_id as i64).f("competeAgainstBestOffset", o.compete_against_best_offset)
        .obj("conditions", !o.conditions.is_empty(), "list").b("conditionsCancelOrder", o.conditions_cancel_order)
        .b("conditionsIgnoreRth", o.conditions_ignore_rth).b("continuousUpdate", o.continuous_update)
        .s("customerAccount", &o.customer_account).b("deactivate", o.deactivate).f("delta", o.delta)
        .f("deltaNeutralAuxPrice", o.delta_neutral_aux_price)
        .s("deltaNeutralClearingAccount", &o.delta_neutral_clearing_account)
        .s("deltaNeutralClearingIntent", &o.delta_neutral_clearing_intent)
        .i("deltaNeutralConId", o.delta_neutral_con_id as i64)
        .s("deltaNeutralDesignatedLocation", &o.delta_neutral_designated_location)
        .s("deltaNeutralOpenClose", &o.delta_neutral_open_close)
        .s("deltaNeutralOrderType", &o.delta_neutral_order_type)
        .s("deltaNeutralSettlingFirm", &o.delta_neutral_settling_firm)
        .b("deltaNeutralShortSale", o.delta_neutral_short_sale)
        .i("deltaNeutralShortSaleSlot", o.delta_neutral_short_sale_slot as i64)
        .s("designatedLocation", &o.designated_location).f("discretionaryAmt", o.discretionary_amt)
        .b("discretionaryUpToLimitPrice", o.discretionary_up_to_limit_price)
        .i("displaySize", o.display_size as i64).b("dontUseAutoPriceForHedge", o.dont_use_auto_price_for_hedge)
        .i("duration", o.duration as i64).i("exemptCode", o.exempt_code as i64).s("extOperator", &o.ext_operator)
        .s("faGroup", &o.fa_group).s("faMethod", &o.fa_method).s("faPercentage", &o.fa_percentage)
        .dec("filledQuantity", o.filled_quantity).s("goodAfterTime", &o.good_after_time)
        .s("goodTillDate", &o.good_till_date).s("hedgeParam", &o.hedge_param).s("hedgeType", &o.hedge_type)
        .b("hidden", o.hidden).b("ignoreOpenAuction", o.ignore_open_auction).b("imbalanceOnly", o.imbalance_only)
        .b("includeOvernight", o.include_overnight).b("isOmsContainer", o.is_oms_container)
        .b("isPeggedChangeAmountDecrease", o.is_pegged_change_amount_decrease).f("lmtPrice", o.lmt_price)
        .f("lmtPriceOffset", o.lmt_price_offset).i("manualOrderIndicator", o.manual_order_indicator as i64)
        .s("manualOrderTime", &o.manual_order_time).f("midOffsetAtHalf", o.mid_offset_at_half)
        .f("midOffsetAtWhole", o.mid_offset_at_whole).s("mifid2DecisionAlgo", &o.mifid2_decision_algo)
        .s("mifid2DecisionMaker", &o.mifid2_decision_maker).s("mifid2ExecutionAlgo", &o.mifid2_execution_algo)
        .s("mifid2ExecutionTrader", &o.mifid2_execution_trader).i("minCompeteSize", o.min_compete_size as i64)
        .i("minQty", o.min_qty as i64).i("minTradeQty", o.min_trade_qty as i64).s("modelCode", &o.model_code)
        .b("notHeld", o.not_held).s("ocaGroup", &o.oca_group).i("ocaType", o.oca_type as i64)
        .s("openClose", &o.open_close).b("optOutSmartRouting", o.opt_out_smart_routing)
        .obj("orderComboLegs", !o.order_combo_legs.is_empty(), "list").i("orderId", o.order_id)
        .obj("orderMiscOptions", !o.order_misc_options.is_empty(), "list").s("orderRef", &o.order_ref)
        .s("orderType", &o.order_type).i("origin", o.origin as i64).b("outsideRth", o.outside_rth)
        .b("overridePercentageConstraints", o.override_percentage_constraints).i("parentId", o.parent_id)
        .i("parentPermId", o.parent_perm_id).f("peggedChangeAmount", o.pegged_change_amount)
        .f("percentOffset", o.percent_offset).i("permId", o.perm_id).b("postOnly", o.post_only)
        .i("postToAts", o.post_to_ats as i64).b("professionalCustomer", o.professional_customer)
        .i("ptOrderId", o.pt_order_id as i64).s("ptOrderType", &o.pt_order_type)
        .b("randomizePrice", o.randomize_price).b("randomizeSize", o.randomize_size)
        .i("refFuturesConId", o.ref_futures_con_id as i64).f("referenceChangeAmount", o.reference_change_amount)
        .i("referenceContractId", o.reference_contract_id as i64).s("referenceExchangeId", &o.reference_exchange_id)
        .i("referencePriceType", o.reference_price_type as i64).b("routeMarketableToBbo", o.route_marketable_to_bbo)
        .s("rule80A", &o.rule80a).b("scaleAutoReset", o.scale_auto_reset)
        .i("scaleInitFillQty", o.scale_init_fill_qty as i64).i("scaleInitLevelSize", o.scale_init_level_size as i64)
        .i("scaleInitPosition", o.scale_init_position as i64)
        .i("scalePriceAdjustInterval", o.scale_price_adjust_interval as i64)
        .f("scalePriceAdjustValue", o.scale_price_adjust_value).f("scalePriceIncrement", o.scale_price_increment)
        .f("scaleProfitOffset", o.scale_profit_offset).b("scaleRandomPercent", o.scale_random_percent)
        .i("scaleSubsLevelSize", o.scale_subs_level_size as i64).s("scaleTable", &o.scale_table)
        .b("seekPriceImprovement", o.seek_price_improvement).s("settlingFirm", &o.settling_firm)
        .s("shareholder", &o.shareholder).i("shortSaleSlot", o.short_sale_slot as i64)
        .i("slOrderId", o.sl_order_id as i64).s("slOrderType", &o.sl_order_type)
        .obj("smartComboRoutingParams", !o.smart_combo_routing_params.is_empty(), "list")
        // Always an object in the Python API.
        .obj("softDollarTier", true, "SoftDollarTier")
        .b("solicited", o.solicited).f("startingPrice", o.starting_price).f("stockRangeLower", o.stock_range_lower)
        .f("stockRangeUpper", o.stock_range_upper).f("stockRefPrice", o.stock_ref_price)
        .s("submitter", &o.submitter).b("sweepToFill", o.sweep_to_fill).s("tif", &o.tif)
        .dec("totalQuantity", o.total_quantity).f("trailStopPrice", o.trail_stop_price)
        .f("trailingPercent", o.trailing_percent).b("transmit", o.transmit)
        .i("triggerMethod", o.trigger_method as i64).f("triggerPrice", o.trigger_price)
        .opt_i("usePriceMgmtAlgo", o.use_price_mgmt_algo).f("volatility", o.volatility)
        .i("volatilityType", o.volatility_type as i64).b("whatIf", o.what_if).i("whatIfType", o.what_if_type as i64);
    d.finish("Order")
}

fn dump_state(s: &OrderState) -> String {
    // The suggested size is a Decimal in the Python API: unset (not zero)
    // when the text is empty.
    let suggested = s.suggested_size.parse::<f64>().map(|v| v != 0.0).unwrap_or(true);
    Dump::default()
        .f("commissionAndFees", s.commission_and_fees).s("commissionAndFeesCurrency", &s.commission_and_fees_currency)
        .s("completedStatus", &s.completed_status).s("completedTime", &s.completed_time)
        .s("equityWithLoanAfter", &s.equity_with_loan_after)
        .f("equityWithLoanAfterOutsideRTH", s.equity_with_loan_after_outside_rth)
        .s("equityWithLoanBefore", &s.equity_with_loan_before)
        .f("equityWithLoanBeforeOutsideRTH", s.equity_with_loan_before_outside_rth)
        .s("equityWithLoanChange", &s.equity_with_loan_change)
        .f("equityWithLoanChangeOutsideRTH", s.equity_with_loan_change_outside_rth)
        .s("initMarginAfter", &s.init_margin_after).f("initMarginAfterOutsideRTH", s.init_margin_after_outside_rth)
        .s("initMarginBefore", &s.init_margin_before).f("initMarginBeforeOutsideRTH", s.init_margin_before_outside_rth)
        .s("initMarginChange", &s.init_margin_change).f("initMarginChangeOutsideRTH", s.init_margin_change_outside_rth)
        .s("maintMarginAfter", &s.maint_margin_after).f("maintMarginAfterOutsideRTH", s.maint_margin_after_outside_rth)
        .s("maintMarginBefore", &s.maint_margin_before)
        .f("maintMarginBeforeOutsideRTH", s.maint_margin_before_outside_rth)
        .s("maintMarginChange", &s.maint_margin_change)
        .f("maintMarginChangeOutsideRTH", s.maint_margin_change_outside_rth)
        .s("marginCurrency", &s.margin_currency).f("maxCommissionAndFees", s.max_commission_and_fees)
        .f("minCommissionAndFees", s.min_commission_and_fees)
        .obj("orderAllocations", !s.order_allocations.is_empty(), "list").s("rejectReason", &s.reject_reason)
        .s("status", &s.status).obj("suggestedSize", suggested, "Decimal").s("warningText", &s.warning_text)
        .finish("OrderState")
}

fn dump_execution(e: &Execution) -> String {
    Dump::default()
        .s("acctNumber", &e.acct_number).f("avgPrice", e.avg_price).i("clientId", e.client_id)
        .dec("cumQty", e.cum_qty).f("evMultiplier", e.ev_multiplier).s("evRule", &e.ev_rule)
        .s("exchange", &e.exchange).s("execId", &e.exec_id).i("lastLiquidity", e.last_liquidity as i64)
        .i("liquidation", e.liquidation as i64).s("modelCode", &e.model_code).i("orderId", e.order_id)
        .s("orderRef", &e.order_ref).b("pendingPriceRevision", e.pending_price_revision).i("permId", e.perm_id)
        .f("price", e.price).dec("shares", e.shares).s("side", &e.side).s("time", &e.time)
        .finish("Execution")
}

fn epoch_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

// ── The callbacks, logged as the case script logs them ──

struct Rec {
    start: Instant,
    rows: Vec<(i64, String)>,
    /// The last status of each order, for the clean-up at the end.
    status: HashMap<i64, String>,
    /// Case T: the order whose Cancelled status places it again as STP.
    t_watch: Option<(i64, Contract)>,
    /// A placeOrder asked from a callback, made once `process_msgs` returns.
    fire: Option<(i64, Contract, Order)>,
}

impl Rec {
    fn add(&mut self, text: String) {
        let ms = self.start.elapsed().as_millis() as i64;
        self.rows.push((ms, text));
    }
    fn count(&self, word: &str) -> usize {
        self.rows.iter().filter(|(_, l)| l.contains(word)).count()
    }
}

impl Wrapper for Rec {
    fn error(&mut self, req_id: i64, error_code: i64, error_string: &str, advanced: &str) {
        // The Rust callback has no error time: the time it is received.
        self.add(format!("error {req_id} {} {error_code} {} {}", epoch_ms(), show_str(error_string), show_str(advanced)));
    }
    fn order_status(
        &mut self, order_id: i64, status: &str, filled: f64, remaining: f64, avg_fill_price: f64, perm_id: i64,
        parent_id: i64, last_fill_price: f64, client_id: i64, why_held: &str, mkt_cap_price: f64,
    ) {
        self.add(format!(
            "orderStatus {order_id} {} {} {} {} {perm_id} {parent_id} {} {client_id} {} {}",
            show_str(status), show_decimal(filled), show_decimal(remaining), py_float(avg_fill_price),
            py_float(last_fill_price), show_str(why_held), py_float(mkt_cap_price),
        ));
        self.add(format!(
            "orderStatus {order_id} {status} filled={} parent={parent_id} why={}",
            py_decimal_str(filled), py_repr(why_held),
        ));
        self.status.insert(order_id, status.to_string());
        if status == "Cancelled" {
            if let Some((x, contract)) = self.t_watch.take_if(|(x, _)| *x == order_id) {
                self.add("---- placeOrder as STP from the callback".into());
                self.fire = Some((x, contract, order("BUY", "STP", 500.0, 1.0, true, 0, "")));
            }
        }
    }
    fn open_order(&mut self, order_id: i64, contract: &Contract, order: &Order, state: &OrderState) {
        self.add(format!("openOrder {order_id} {} {} {}", dump_contract(contract), dump_order(order), dump_state(state)));
        self.add(format!("openOrder {order_id} {} {} parent={}", order.order_type, state.status, order.parent_id));
        self.status.insert(order_id, state.status.clone());
    }
    fn open_order_end(&mut self) {
        self.add("openOrderEnd ".into());
    }
    fn exec_details(&mut self, req_id: i64, contract: &Contract, execution: &Execution) {
        self.add(format!("execDetails {req_id} {} {}", dump_contract(contract), dump_execution(execution)));
    }
    fn exec_details_end(&mut self, req_id: i64) {
        self.add(format!("execDetailsEnd {req_id}"));
    }
    fn commission_and_fees_report(&mut self, r: &CommissionAndFeesReport) {
        let d = Dump::default()
            .f("commissionAndFees", r.commission_and_fees).s("currency", &r.currency).s("execId", &r.exec_id)
            .f("realizedPNL", r.realized_pnl).f("yield_", r.yield_amount)
            .i("yieldRedemptionDate", r.yield_redemption_date.parse().unwrap_or(0))
            .finish("CommissionAndFeesReport");
        self.add(format!("commissionAndFeesReport {d}"));
    }
    fn completed_order(&mut self, contract: &Contract, order: &Order, state: &OrderState) {
        self.add(format!("completedOrder {} {} {}", dump_contract(contract), dump_order(order), dump_state(state)));
    }
    fn completed_orders_end(&mut self) {
        self.add("completedOrdersEnd ".into());
    }
}

// ── The orders of the case script ──

fn stock(symbol: &str, con_id: i64) -> Contract {
    Contract {
        con_id, symbol: symbol.into(), sec_type: "STK".into(), exchange: "SMART".into(), currency: "USD".into(),
        ..Default::default()
    }
}

fn order(action: &str, typ: &str, price: f64, qty: f64, transmit: bool, parent: i64, oca: &str) -> Order {
    let mut o = Order {
        action: action.into(), total_quantity: qty, order_type: typ.into(), tif: "DAY".into(), outside_rth: true,
        transmit, parent_id: parent, oca_group: oca.into(), ..Default::default()
    };
    if typ == "LMT" { o.lmt_price = price } else { o.aux_price = price }
    o
}

/// The case script's `order()`: BUY 1 LMT 100.
fn plain() -> Order {
    order("BUY", "LMT", 100.0, 1.0, true, 0, "")
}

fn lmt(price: f64) -> Order {
    order("BUY", "LMT", price, 1.0, true, 0, "")
}

fn held(price: f64, oca: &str) -> Order {
    order("BUY", "LMT", price, 1.0, false, 0, oca)
}

fn dated(gtd: &str, gat: &str) -> Order {
    let mut o = plain();
    if !gtd.is_empty() {
        o.tif = "GTD".into();
        o.good_till_date = gtd.into();
    }
    o.good_after_time = gat.into();
    o
}

// ── One session ──

struct Ctx<'a> {
    client: &'a EClient,
    rec: Rec,
    base: i64,
    placed: Vec<i64>,
}

impl Ctx<'_> {
    fn pump_until(&mut self, until: Instant) {
        loop {
            self.client.process_msgs(&mut self.rec);
            if let Some((id, contract, o)) = self.rec.fire.take() {
                self.place(id, &contract, &o);
            }
            if Instant::now() >= until { break; }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    fn sleep(&mut self, secs: f64) {
        self.pump_until(Instant::now() + Duration::from_secs_f64(secs.max(0.0)));
    }
    /// The case script's `at(t0, seconds)`.
    fn at(&mut self, t0: Instant, secs: f64) {
        self.pump_until(t0 + Duration::from_secs_f64(secs));
    }
    fn wait(&mut self, timeout: f64, pred: impl Fn(&Rec) -> bool) -> bool {
        let until = Instant::now() + Duration::from_secs_f64(timeout);
        loop {
            self.client.process_msgs(&mut self.rec);
            if let Some((id, contract, o)) = self.rec.fire.take() {
                self.place(id, &contract, &o);
            }
            if pred(&self.rec) { return true; }
            if Instant::now() >= until { return false; }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    /// The case script's `app.wait(word, n)`: n rows hold the word.
    fn wait_word(&mut self, word: &str, n: usize) -> bool {
        self.wait(30.0, |r| r.count(word) >= n)
    }
    fn ended(&mut self, oid: i64, timeout: f64) -> bool {
        let (a, b) = (format!("orderStatus {oid} Cancelled"), format!("orderStatus {oid} Inactive"));
        self.wait(timeout, move |r| r.rows.iter().any(|(_, l)| l.contains(&a) || l.contains(&b)))
    }
    fn ids(&mut self, n: i64) -> Vec<i64> {
        let first = self.base;
        self.base += n;
        (first..first + n).collect()
    }
    fn mark(&mut self, text: &str) {
        self.rec.add(format!("---- {text}"));
    }
    fn place(&mut self, id: i64, contract: &Contract, o: &Order) {
        self.placed.push(id);
        if let Err(e) = self.client.place_order(id, contract, o) {
            self.rec.add(format!("placeOrderErr {id} {}", show_str(&e)));
        }
    }
    fn cancel(&mut self, id: i64) {
        if let Err(e) = self.client.cancel_order(id, "") {
            self.rec.add(format!("cancelOrderErr {id} {}", show_str(&e)));
        }
    }
    fn global_cancel(&mut self) {
        if let Err(e) = self.client.req_global_cancel() {
            self.rec.add(format!("reqGlobalCancelErr {}", show_str(&e)));
        }
    }
    fn open_orders(&mut self) {
        self.client.req_open_orders(&mut self.rec);
    }
    fn all_open_orders(&mut self) {
        self.client.req_all_open_orders(&mut self.rec);
    }
}

fn run_case(ctx: &mut Ctx, case: &str) {
    let aapl = stock("AAPL", 0);
    match case {
        // A refused order: cancelOrder at 3 s, 12 s and 19 s after the refusal.
        "K" => {
            let x = ctx.ids(1)[0];
            ctx.place(x, &aapl, &order("BUY", "LMT", 100.0, 5_000_000.0, true, 0, ""));
            let word = format!("error {x} ");
            ctx.wait(10.0, |r| r.count(&word) > 0);
            let t = Instant::now();
            for s in [3, 12, 19] {
                ctx.at(t, s as f64);
                ctx.mark(&format!("cancelOrder at +{s} s of the refusal"));
                ctx.cancel(x);
            }
            ctx.sleep(2.0);
        }
        // A resting order cancelled: cancelOrder again at 3 s, 8 s, 12 s and 19 s after the cancel.
        "L" => {
            let x = ctx.ids(1)[0];
            ctx.place(x, &aapl, &plain());
            ctx.wait_word(&format!("orderStatus {x} "), 1);
            ctx.sleep(2.0);
            ctx.cancel(x);
            ctx.ended(x, 15.0);
            let t = Instant::now();
            for s in [3, 8, 12, 19] {
                ctx.at(t, s as f64);
                ctx.mark(&format!("cancelOrder at +{s} s of the cancel"));
                ctx.cancel(x);
            }
            ctx.sleep(2.0);
        }
        // A refused order: global cancels at 3 s and 6 s, then at 12 s.
        "M" => {
            let x = ctx.ids(1)[0];
            ctx.place(x, &aapl, &order("BUY", "LMT", 100.0, 5_000_000.0, true, 0, ""));
            let word = format!("error {x} ");
            ctx.wait(10.0, |r| r.count(&word) > 0);
            let t = Instant::now();
            for s in [3, 6, 12] {
                ctx.at(t, s as f64);
                ctx.mark(&format!("reqGlobalCancel at +{s} s of the refusal"));
                ctx.global_cancel();
            }
            ctx.sleep(2.0);
        }
        // Held orders (transmit off): two of one OCA group and two alone, one global cancel; a second 3 s later.
        "N" => {
            let v = ctx.ids(4);
            let group = format!("c551-{}", v[0]);
            ctx.place(v[0], &aapl, &held(100.0, &group));
            ctx.place(v[1], &aapl, &held(99.0, &group));
            ctx.place(v[2], &aapl, &held(98.0, ""));
            ctx.place(v[3], &aapl, &held(97.0, ""));
            ctx.sleep(4.0);
            ctx.mark("reqOpenOrders");
            ctx.open_orders();
            ctx.sleep(2.0);
            ctx.mark("reqGlobalCancel");
            ctx.global_cancel();
            ctx.sleep(3.0);
            ctx.mark("reqGlobalCancel again");
            ctx.global_cancel();
            ctx.sleep(2.0);
            ctx.mark("reqOpenOrders");
            ctx.open_orders();
            ctx.sleep(2.0);
        }
        // An order refused locally (unknown algorithm), placed again under the same id, then a good one under it.
        "P" => {
            let x = ctx.ids(1)[0];
            let mut o = plain();
            o.algo_strategy = "NoSuchAlgo".into();
            ctx.place(x, &aapl, &o);
            ctx.sleep(2.0);
            ctx.mark("the same again");
            ctx.place(x, &aapl, &o);
            ctx.sleep(2.0);
            ctx.mark("a plain order under the same id");
            ctx.place(x, &aapl, &plain());
            ctx.sleep(3.0);
            ctx.mark("cancelOrder");
            ctx.cancel(x);
            ctx.sleep(3.0);
        }
        // A good-till date that is no date; an order on a contract with no exchange.
        "Q" => {
            let v = ctx.ids(3);
            let mut o = plain();
            o.tif = "GTD".into();
            o.good_till_date = "not a date".into();
            ctx.place(v[0], &aapl, &o);
            ctx.sleep(2.0);
            ctx.mark("no exchange");
            let mut c = stock("AAPL", 0);
            c.exchange = String::new();
            ctx.place(v[1], &c, &plain());
            ctx.sleep(2.0);
            ctx.mark("no exchange, with the contract id");
            let mut c = stock("AAPL", 265598);
            c.exchange = String::new();
            ctx.place(v[2], &c, &plain());
            ctx.sleep(3.0);
            for i in v {
                ctx.cancel(i);
            }
            ctx.sleep(2.0);
        }
        // A parent and its child working, the open-order list twice; then two orders alone.
        "R" => {
            let v = ctx.ids(2);
            let (p, ch) = (v[0], v[1]);
            ctx.place(p, &aapl, &held(100.0, ""));
            ctx.place(ch, &aapl, &order("SELL", "LMT", 400.0, 1.0, true, p, ""));
            ctx.sleep(5.0);
            ctx.mark("reqOpenOrders");
            ctx.open_orders();
            ctx.wait_word("openOrderEnd", 1);
            ctx.sleep(1.0);
            ctx.mark("reqAllOpenOrders");
            ctx.all_open_orders();
            ctx.wait_word("openOrderEnd", 2);
            ctx.sleep(1.0);
            ctx.mark("cancel the parent");
            ctx.cancel(p);
            ctx.sleep(4.0);
            let v = ctx.ids(3);
            ctx.place(v[0], &aapl, &lmt(100.0));
            ctx.place(v[1], &aapl, &lmt(99.0));
            ctx.place(v[2], &aapl, &lmt(98.0));
            ctx.sleep(5.0);
            ctx.mark("reqOpenOrders, three orders alone");
            ctx.open_orders();
            ctx.wait_word("openOrderEnd", 3);
            ctx.sleep(1.0);
            ctx.mark("reqGlobalCancel");
            ctx.global_cancel();
            ctx.sleep(4.0);
        }
        // A bracket placed at once: the order of its callbacks.
        "S" => {
            let v = ctx.ids(3);
            let (p, tp, sl) = (v[0], v[1], v[2]);
            ctx.place(p, &aapl, &held(100.0, ""));
            ctx.place(tp, &aapl, &order("SELL", "LMT", 400.0, 1.0, false, p, ""));
            ctx.place(sl, &aapl, &order("SELL", "STP", 50.0, 1.0, true, p, ""));
            ctx.sleep(8.0);
            ctx.mark("cancel the parent");
            ctx.cancel(p);
            ctx.sleep(5.0);
        }
        // ibx#544: a modify made from the callback of the Cancelled status, to another order type.
        "T" => {
            let x = ctx.ids(1)[0];
            ctx.rec.t_watch = Some((x, aapl.clone()));
            ctx.place(x, &aapl, &plain());
            ctx.wait_word(&format!("orderStatus {x} "), 1);
            ctx.sleep(2.0);
            ctx.cancel(x);
            ctx.sleep(5.0);
            ctx.mark("cancelOrder at the end, in case an order was left");
            ctx.cancel(x);
            ctx.sleep(3.0);
        }
        // ibx#506: an order with a soft dollar tier the account does not have.
        "U" => {
            let x = ctx.ids(1)[0];
            let mut o = plain();
            o.soft_dollar_tier_name = "Tier1".into();
            o.soft_dollar_tier_val = "Value1".into();
            ctx.place(x, &aapl, &o);
            ctx.sleep(4.0);
            ctx.mark("cancelOrder");
            ctx.cancel(x);
            ctx.sleep(3.0);
        }
        // ibx#500: an order with the four MiFID fields set, on a session without the feature.
        "V" => {
            let v = ctx.ids(2);
            let mut o = plain();
            o.mifid2_decision_maker = "1234".into();
            o.mifid2_execution_trader = "5678".into();
            ctx.place(v[0], &aapl, &o);
            ctx.sleep(4.0);
            ctx.mark("cancelOrder");
            ctx.cancel(v[0]);
            ctx.sleep(3.0);
            let mut o = plain();
            o.mifid2_decision_algo = "ALGO1".into();
            o.mifid2_execution_algo = "ALGO2".into();
            ctx.mark("the two algo fields");
            ctx.place(v[1], &aapl, &o);
            ctx.sleep(4.0);
            ctx.mark("cancelOrder");
            ctx.cancel(v[1]);
            ctx.sleep(3.0);
        }
        // ibx#507: an order with an allocation on a session that is not FA.
        "W" => {
            let v = ctx.ids(2);
            let mut o = plain();
            o.fa_group = "Group1".into();
            o.fa_method = "NetLiq".into();
            ctx.place(v[0], &aapl, &o);
            ctx.sleep(4.0);
            ctx.mark("cancelOrder");
            ctx.cancel(v[0]);
            ctx.sleep(3.0);
            let mut o = plain();
            o.model_code = "Model1".into();
            ctx.mark("a model code");
            ctx.place(v[1], &aapl, &o);
            ctx.sleep(4.0);
            ctx.mark("cancelOrder");
            ctx.cancel(v[1]);
            ctx.sleep(3.0);
        }
        // ibx#548: eight working orders with ids apart, the open-order list twice, then a global cancel.
        "X" => {
            let first = ctx.ids(50)[0];
            for (k, off) in [0, 3, 7, 8, 16, 17, 30, 45].into_iter().enumerate() {
                ctx.place(first + off, &aapl, &lmt(100.0 - k as f64));
                ctx.sleep(0.5);
            }
            ctx.sleep(4.0);
            ctx.mark("reqOpenOrders");
            ctx.open_orders();
            ctx.wait_word("openOrderEnd", 1);
            ctx.sleep(1.0);
            ctx.mark("reqAllOpenOrders");
            ctx.all_open_orders();
            ctx.wait_word("openOrderEnd", 2);
            ctx.sleep(1.0);
            ctx.mark("reqGlobalCancel");
            ctx.global_cancel();
            ctx.sleep(5.0);
        }
        // The quantity in the text of the order message: 1000 and 12345 shares.
        "Y" => {
            let v = ctx.ids(2);
            ctx.place(v[0], &aapl, &order("BUY", "LMT", 1.0, 1000.0, true, 0, ""));
            ctx.sleep(1.0);
            ctx.place(v[1], &aapl, &order("BUY", "LMT", 1.0, 12345.0, true, 0, ""));
            ctx.sleep(4.0);
            ctx.mark("reqGlobalCancel");
            ctx.global_cancel();
            ctx.sleep(4.0);
        }
        // Dates that are refused, and the same id placed again after a refusal.
        "Z" => {
            let v = ctx.ids(5);
            ctx.place(v[0], &aapl, &dated("not a date", ""));
            ctx.sleep(2.0);
            ctx.mark("the same id again, a plain order");
            ctx.place(v[0], &aapl, &plain());
            ctx.sleep(3.0);
            ctx.mark("a zone that does not exist");
            ctx.place(v[1], &aapl, &dated("20261230 16:00:00 Nowhere/Zone", ""));
            ctx.sleep(2.0);
            ctx.mark("a zone that is not the contract's");
            ctx.place(v[2], &aapl, &dated("20261230 16:00:00 Asia/Tokyo", ""));
            ctx.sleep(3.0);
            ctx.mark("goodAfterTime that is no date");
            ctx.place(v[3], &aapl, &dated("", "not a date"));
            ctx.sleep(2.0);
            ctx.mark("an unknown action, then the same id again as a plain order");
            let mut o = plain();
            o.action = "FOO".into();
            ctx.place(v[4], &aapl, &o);
            ctx.sleep(2.0);
            ctx.place(v[4], &aapl, &plain());
            ctx.sleep(3.0);
            ctx.mark("reqGlobalCancel");
            ctx.global_cancel();
            ctx.sleep(4.0);
        }
        // The order rules refused before anything is sent: code and text of each.
        "AA" => {
            let bag = Contract {
                symbol: "SPY".into(), sec_type: "BAG".into(), exchange: "SMART".into(), currency: "USD".into(),
                ..Default::default()
            };
            let mut noex = stock("AAPL", 0);
            noex.exchange = String::new();
            type Change = fn(&mut Order);
            let rules: [(&str, Change, Option<&Contract>); 16] = [
                ("action FOO", |o| o.action = "FOO".into(), None),
                ("quantity -5", |o| o.total_quantity = -5.0, None),
                ("quantity 2,000,000,000", |o| o.total_quantity = 2_000_000_000.0, None),
                ("STP without a stop price", |o| o.order_type = "STP".into(), None),
                ("STP LMT without a stop price", |o| {
                    o.order_type = "STP LMT".into();
                    o.lmt_price = 100.0;
                }, None),
                ("TRAIL LIMIT without a stop price", |o| {
                    o.order_type = "TRAIL LIMIT".into();
                    o.lmt_price_offset = 1.0;
                    o.aux_price = 1.0;
                }, None),
                ("trigger method 5", |o| o.trigger_method = 5, None),
                ("what-if with transmit off", |o| {
                    o.what_if = true;
                    o.transmit = false;
                }, None),
                ("TRAIL percent 150", |o| {
                    o.order_type = "TRAIL".into();
                    o.trailing_percent = 150.0;
                }, None),
                ("STP stop price infinite", |o| {
                    o.order_type = "STP".into();
                    o.aux_price = f64::INFINITY;
                }, None),
                ("MIT without a trigger price", |o| o.order_type = "MIT".into(), None),
                ("TRAIL LIMIT with a price and an offset", |o| {
                    o.order_type = "TRAIL LIMIT".into();
                    o.lmt_price = 100.0;
                    o.lmt_price_offset = 1.0;
                    o.aux_price = 1.0;
                    o.trail_stop_price = 90.0;
                }, None),
                ("TRAIL LIMIT with neither", |o| {
                    o.order_type = "TRAIL LIMIT".into();
                    o.lmt_price = 0.0;
                    o.aux_price = 1.0;
                    o.trail_stop_price = 90.0;
                }, None),
                ("MIDPRICE outside regular hours", |o| {
                    o.order_type = "MIDPRICE".into();
                    o.lmt_price = 100.0;
                    o.outside_rth = true;
                }, None),
                ("BAG without legs", |_| {}, Some(&bag)),
                ("no exchange", |_| {}, Some(&noex)),
            ];
            for (label, change, contract) in rules {
                let i = ctx.ids(1)[0];
                let mut o = plain();
                change(&mut o);
                ctx.mark(label);
                ctx.place(i, contract.unwrap_or(&aapl), &o);
                ctx.sleep(0.8);
            }
            ctx.sleep(2.0);
            ctx.mark("reqGlobalCancel");
            ctx.global_cancel();
            ctx.sleep(4.0);
        }
        // Dates the reference takes or refuses: goodAfterTime and goodTillDate forms.
        "AB" => {
            let forms: [(&str, &str, &str); 10] = [
                ("goodAfterTime 20261230 99:99:99", "", "20261230 99:99:99"),
                ("goodAfterTime 20261230", "", "20261230"),
                ("goodAfterTime 2026-12-30 10:00:00", "", "2026-12-30 10:00:00"),
                ("goodAfterTime 20261230 10:00:00 Nowhere/Zone", "", "20261230 10:00:00 Nowhere/Zone"),
                ("goodAfterTime 20261230 10:00:00 Asia/Tokyo", "", "20261230 10:00:00 Asia/Tokyo"),
                ("goodAfterTime 20261230 10:00:00 US/Eastern", "", "20261230 10:00:00 US/Eastern"),
                ("goodTillDate 20261230 99:99:99", "20261230 99:99:99", ""),
                ("goodTillDate 20261230", "20261230", ""),
                ("goodTillDate 2026-12-30 10:00:00", "2026-12-30 10:00:00", ""),
                ("goodTillDate 20261230 10:00:00 US/Eastern", "20261230 10:00:00 US/Eastern", ""),
            ];
            for (label, gtd, gat) in forms {
                let i = ctx.ids(1)[0];
                ctx.mark(label);
                ctx.place(i, &aapl, &dated(gtd, gat));
                ctx.sleep(2.5);
            }
            ctx.mark("reqGlobalCancel");
            ctx.global_cancel();
            ctx.sleep(5.0);
        }
        // The same id placed again after each kind of refusal; trigger price rules.
        "AC" => {
            type Change = fn(&mut Order);
            let refusals: [(&str, Change); 6] = [
                ("goodTillDate in a zone that is not the contract's", |o| {
                    o.tif = "GTD".into();
                    o.good_till_date = "20261230 16:00:00 Asia/Tokyo".into();
                }),
                ("a limit price that is not a number", |o| o.lmt_price = f64::NAN),
                ("a customer account the account cannot use", |o| o.customer_account = "X123".into()),
                ("LIT without a trigger price", |o| o.order_type = "LIT".into()),
                ("MIT with an infinite trigger price", |o| {
                    o.order_type = "MIT".into();
                    o.aux_price = f64::INFINITY;
                }),
                ("LIT with an infinite trigger price", |o| {
                    o.order_type = "LIT".into();
                    o.aux_price = f64::INFINITY;
                }),
            ];
            for (label, change) in refusals {
                let i = ctx.ids(1)[0];
                let mut o = plain();
                change(&mut o);
                ctx.mark(label);
                ctx.place(i, &aapl, &o);
                ctx.sleep(2.0);
                ctx.mark("the same id again, a plain order");
                ctx.place(i, &aapl, &plain());
                ctx.sleep(3.0);
            }
            ctx.mark("reqGlobalCancel");
            ctx.global_cancel();
            ctx.sleep(5.0);
        }
        // Six orders held with transmit off, alone: global cancels 4 s apart, then the cancel of each.
        "AD" => {
            let v = ctx.ids(6);
            for (k, &i) in v.iter().enumerate() {
                ctx.place(i, &aapl, &held(100.0 - k as f64, ""));
            }
            ctx.sleep(4.0);
            for n in 1..=4 {
                ctx.mark(&format!("reqGlobalCancel {n}"));
                ctx.global_cancel();
                ctx.sleep(4.0);
            }
            ctx.mark("cancelOrder of each");
            for &i in &v {
                ctx.cancel(i);
                ctx.sleep(0.5);
            }
            ctx.sleep(3.0);
        }
        // Five held orders, the second and the fourth in one OCA group: global cancels.
        "AE" => {
            let v = ctx.ids(5);
            let group = format!("c551-{}", v[0]);
            for (k, &i) in v.iter().enumerate() {
                let oca = if k == 1 || k == 3 { group.as_str() } else { "" };
                ctx.place(i, &aapl, &held(100.0 - k as f64, oca));
            }
            ctx.sleep(4.0);
            for n in 1..=3 {
                ctx.mark(&format!("reqGlobalCancel {n}"));
                ctx.global_cancel();
                ctx.sleep(4.0);
            }
            ctx.mark("cancelOrder of each");
            for &i in &v {
                ctx.cancel(i);
                ctx.sleep(0.5);
            }
            ctx.sleep(3.0);
        }
        _ => unreachable!("case letters are checked before any login"),
    }
}

fn session(case: &str, base: Option<i64>, out_dir: &str) -> Result<(), Box<dyn std::error::Error>> {
    let config = EClientConfig {
        username: env::var("IB_USERNAME")?, password: env::var("IB_PASSWORD")?,
        host: env::var("IB_HOST").ok().filter(|h| !h.is_empty()).unwrap_or_else(|| "cdc1.ibllc.com".into()),
        paper: true, core_id: None,
        // The case scripts connect as client 198 (ibx#704).
        client_id: 198,
        copied_waits: false,
        code_provider: None,
        restart_token_file: None,
    };
    let client = match EClient::connect(&config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("case {case}: login failed ({e}), one more try in 15 s");
            std::thread::sleep(Duration::from_secs(15));
            EClient::connect(&config)?
        }
    };
    // Orders and tests only ever on the paper account.
    if !client.account_id.starts_with("DU") {
        client.disconnect();
        return Err("NOT A PAPER ACCOUNT: stopped".into());
    }
    let rec = Rec { start: Instant::now(), rows: Vec::new(), status: HashMap::new(), t_watch: None, fire: None };
    let mut ctx = Ctx { client: &client, rec, base: 0, placed: Vec::new() };
    // The connect notices settle, then are left out, as the case script.
    ctx.sleep(3.0);
    ctx.rec.rows.clear();
    let next_id = client.next_order_id();
    let header = format!(
        "== case {case} on rust, client 0, {}",
        jiff::Zoned::now().strftime("%d/%m/%Y %H:%M:%S"),
    );
    println!("{header}");
    ctx.rec.start = Instant::now();
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    ctx.base = base.unwrap_or_else(|| next_id.max(9001 + secs % 50000));

    run_case(&mut ctx, case);

    // An answer that follows the last request is lost without this wait.
    ctx.sleep(2.0);
    let mut text = format!("{header}\n");
    for (t, line) in &ctx.rec.rows {
        let _ = writeln!(text, "   +{t:6} ms  {line}");
    }
    print!("{}", &text[header.len() + 1..]);
    let _ = std::io::stdout().flush();
    std::fs::create_dir_all(out_dir)?;
    let path = format!("{out_dir}/rust_{case}.log");
    std::fs::write(&path, &text)?;
    println!("-- written {path}");

    // Outside the rows: nothing of this case is left working.
    let mut placed = std::mem::take(&mut ctx.placed);
    placed.sort_unstable();
    placed.dedup();
    let mut cleaned = 0;
    for id in placed {
        let done = ctx.rec.status.get(&id)
            .is_some_and(|s| matches!(s.as_str(), "Cancelled" | "ApiCancelled" | "Inactive" | "Filled"));
        if !done {
            let _ = client.cancel_order(id, "");
            cleaned += 1;
        }
    }
    if cleaned > 0 {
        let until = Instant::now() + Duration::from_secs(3);
        let mut quiet = Rec { start: Instant::now(), rows: Vec::new(), status: HashMap::new(), t_watch: None, fire: None };
        while Instant::now() < until {
            client.process_msgs(&mut quiet);
            std::thread::sleep(Duration::from_millis(5));
        }
        println!("-- clean-up: cancel sent for {cleaned} order(s) not seen ended");
    }
    client.disconnect();
    std::thread::sleep(Duration::from_secs(1));
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().skip(1).collect();
    let mut cases: Vec<String> = Vec::new();
    let (mut base, mut out_dir, mut pause) = (None, ".tmp/551".to_string(), 10.0_f64);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--base" => base = Some(it.next().ok_or("--base needs an id")?.parse::<i64>()?),
            "--out" => out_dir = it.next().ok_or("--out needs a directory")?.clone(),
            "--pause" => pause = it.next().ok_or("--pause needs seconds")?.parse::<f64>()?,
            "--case" => cases.push(it.next().ok_or("--case needs a letter")?.to_uppercase()),
            other => cases.push(other.to_uppercase()),
        }
    }
    if cases.is_empty() {
        return Err(format!("give case letters, among {}", CASES.join(" ")).into());
    }
    if let Some(bad) = cases.iter().find(|c| !CASES.contains(&c.as_str())) {
        return Err(format!("unknown case {bad}: the cases are {}", CASES.join(" ")).into());
    }
    if base.is_some() && cases.len() > 1 {
        return Err("--base is for one case at a time (the first id of that case's reference log)".into());
    }
    let _log = ibx::logging::init(&ibx::logging::LogConfig::from_env());
    for (n, case) in cases.iter().enumerate() {
        if n > 0 {
            std::thread::sleep(Duration::from_secs_f64(pause));
        }
        session(case, base, &out_dir)?;
    }
    Ok(())
}
