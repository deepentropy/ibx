//! Combo (BAG) orders as the reference builds them (ibx#470).
//!
//! The reference does not send a combo order at once: it first builds the
//! combo with set-up requests on the order connection (captured 26/09/2026,
//! 28/09/2026, 30/09/2026 and 02/10/2026, ib-agent#105).
//!
//! - Smart combo: the BAG definition by the currency's smart combo conId
//!   (logon tag 6611), the ICS definition the BAG record names (6346), the
//!   definition of each leg, one after the other, the smart combo
//!   validation of every leg at once, the combo multiplier (35=U 6040=36)
//!   and the leg confirmation (35=U 6040=7).
//! - Directed combo: the BAG by symbol on its exchange, the ICS and leg
//!   definitions on that exchange, the leg confirmation.
//!
//! The BAG definition and the built combo are kept for the session: a
//! second order on the same legs goes out at once, with no request.

use std::collections::HashMap;

use crate::engine::outside_rth::RthTypes;
use crate::types::{ComboSpec, OrderId, Price, PRICE_SCALE};

/// Order fields as (tag, value) pairs.
pub(crate) type Fields = Vec<(u32, String)>;

/// Error 200, the reference's answer when a definition of the combo or of
/// a leg is not found (`jextend.av.a(jsecdef.f, boolean)@24/@132/@395`).
pub(crate) const NO_DEFINITION: (i64, &str) = (200, "No security definition has been found for the request");

/// One leg of a built combo.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Leg {
    pub con_id: i64,
    pub ratio: i32,
    pub buy: bool,
    /// The leg's exchange as the caller gave it.
    pub exchange: String,
    pub symbol: String,
    /// API security type (STK, OPT, FUT, ...).
    pub sec_type: String,
    pub currency: String,
    /// The leg's multiplier, 1 when its definition has none.
    pub multiplier: f64,
}

/// A combo built by the set-up requests.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Combo {
    /// conId of the BAG record (6008 of the order).
    pub bag_con_id: i64,
    /// The BAG exchange as the caller gave it, and as it is routed (BEST
    /// for SMART).
    pub exchange: String,
    pub routed: String,
    pub currency: String,
    /// The BAG record's trading class (COMB).
    pub trading_class: String,
    /// The legs in the contract's order: ascending conId (`jcomb.a9.C()`).
    pub legs: Vec<Leg>,
    /// The combo multiplier the server gave (6040=36), 1 when not asked.
    pub multiplier: f64,
}

impl Combo {
    /// The combo's symbol: the leg symbols, sorted, each once, comma
    /// separated (`jcomb.a9.aI()`, `jclient.dy.R()`).
    pub(crate) fn symbol(&self) -> String {
        let symbols: std::collections::BTreeSet<&str> = self.legs.iter().map(|l| l.symbol.as_str()).collect();
        symbols.into_iter().collect::<Vec<_>>().join(",")
    }

    /// The leg category of the combo (`jcomb.b2.g(List)`), tag 6134.
    pub(crate) fn category(&self) -> i32 {
        category(self.legs.iter().map(|l| l.sec_type.as_str()))
    }

    /// The legs sub-blocks: the leg count, then per leg its conId, ratio
    /// and side (1 buy, 0 sell) (`jcomb.bk.a(StringBuffer, int, boolean)`).
    fn legs_fields(&self) -> Fields {
        let mut f = vec![(6079, self.legs.len().to_string())];
        for leg in &self.legs {
            f.push((6080, leg.con_id.to_string()));
            f.push((6081, leg.ratio.to_string()));
            f.push((6082, if leg.buy { "1" } else { "0" }.to_string()));
        }
        f
    }

    /// The combo block of a new order, after its conId (`pe.c@1767`,
    /// `jcomb.bk.a(StringBuffer, boolean)`): the multiplier for the leg
    /// categories that carry it, the legs, the combo type (0) and the leg
    /// category. Captured 26/09/2026: `6079=2|6080=756733|6081=1|6082=1|
    /// 6080=320227571|6081=1|6082=0|6175=0|6134=9`.
    pub(crate) fn order_block(&self) -> Fields {
        let mut f = Fields::new();
        let category = self.category();
        if matches!(category, 2..=5 | 7 | 8) {
            f.push((231, format_number(self.multiplier)));
        }
        f.extend(self.legs_fields());
        f.push((6175, "0".to_string()));
        f.push((6134, category.to_string()));
        f
    }

    /// The combo key the reference shows as comboLegsDescrip: per leg in
    /// contract order its conId and its ratio, negative for a selling leg,
    /// the ratios divided by their greatest common divisor
    /// (`jextend.protobuf.outgoing.a.b(jclient.pe)`). Captured:
    /// `756733|1,320227571|-1`.
    pub(crate) fn legs_descrip(&self) -> String {
        let divisor = self.legs.iter().fold(0, |g, l| gcd(g, l.ratio.unsigned_abs())).max(1) as i32;
        self.legs.iter()
            .map(|l| format!("{}|{}", l.con_id, if l.buy { l.ratio / divisor } else { -l.ratio / divisor }))
            .collect::<Vec<_>>()
            .join(",")
    }

    /// The combo contract openOrder shows (captured 26/09/2026): the BAG
    /// conId, the leg symbols as symbol and local symbol, the trading
    /// class, the combo key and the legs in contract order.
    pub(crate) fn api_contract(&self, spec: &ComboSpec) -> crate::api::types::Contract {
        let symbol = self.symbol();
        let combo_legs = self.legs.iter().map(|leg| {
            let given = spec.legs.iter().find(|l| l.con_id == leg.con_id);
            crate::api::types::ComboLeg {
                con_id: leg.con_id,
                ratio: leg.ratio,
                action: if leg.buy { "BUY" } else { "SELL" }.to_string(),
                exchange: given.map(|l| l.exchange.clone()).unwrap_or_else(|| leg.exchange.clone()),
                ..Default::default()
            }
        }).collect();
        crate::api::types::Contract {
            con_id: self.bag_con_id,
            symbol: symbol.clone(),
            sec_type: "BAG".to_string(),
            exchange: self.exchange.clone(),
            currency: self.currency.clone(),
            local_symbol: symbol,
            trading_class: self.trading_class.clone(),
            combo_legs_descrip: self.legs_descrip(),
            combo_legs,
            ..Default::default()
        }
    }

    /// The per-leg prices of `spec` in contract leg order; empty when the
    /// order has none.
    pub(crate) fn leg_prices(&self, spec: &ComboSpec) -> Vec<Price> {
        if spec.leg_prices.len() != spec.legs.len() || spec.leg_prices.is_empty() {
            return Vec::new();
        }
        self.legs.iter().filter_map(|leg| {
            spec.legs.iter().position(|l| l.con_id == leg.con_id).map(|i| spec.leg_prices[i])
        }).collect()
    }

    /// The combo price of per-leg prices (`jcomb.a9.a(double, o, double...)`):
    /// each leg price times its ratio and multiplier, added for a buying
    /// leg and taken off for a selling one, over the combo multiplier.
    /// Captured: 721.35 - 794.50 = -73.15.
    pub(crate) fn price_of_legs(&self, prices: &[Price]) -> Option<Price> {
        if prices.len() != self.legs.len() || prices.is_empty() {
            return None;
        }
        let mut sum = 0.0;
        for (leg, price) in self.legs.iter().zip(prices) {
            let sign = if leg.buy { 1.0 } else { -1.0 };
            sum += sign * leg.ratio as f64 * leg.multiplier * (*price as f64 / PRICE_SCALE as f64);
        }
        let multiplier = if self.multiplier > 0.0 { self.multiplier } else { 1.0 };
        Some(((sum / multiplier) * PRICE_SCALE as f64).round() as Price)
    }
}

fn gcd(a: u32, b: u32) -> u32 {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// A number as the reference writes a multiplier: no trailing zeros.
fn format_number(v: f64) -> String {
    let s = format!("{v}");
    s.strip_suffix(".0").map(str::to_string).unwrap_or(s)
}

/// The leg category of a combo by its legs' security types
/// (`jcomb.b2.g(List)`): 0 for options, 9 for stocks, 1 for options and
/// stocks, 2 futures, 3 stocks and futures, 4 options and futures, 5 all
/// three, 7 futures options, 8 futures options and futures, 11 options and
/// cash, 12 with a warrant, 13 with a commodity, 14 with a CFD, -1 else.
pub(crate) fn category<'a>(sec_types: impl Iterator<Item = &'a str>) -> i32 {
    let (mut opt, mut stk, mut fut, mut fop, mut cash, mut cmdty, mut war, mut cfd) =
        (false, false, false, false, false, false, false, false);
    for t in sec_types {
        match t {
            "OPT" => opt = true,
            "STK" => stk = true,
            "FUT" => fut = true,
            "FOP" => fop = true,
            "CASH" => cash = true,
            "CMDTY" => cmdty = true,
            "WAR" => war = true,
            "CFD" => cfd = true,
            _ => {}
        }
    }
    if cfd { return 14; }
    let others = |o: bool, s: bool, f: bool, fo: bool, c: bool| {
        opt == o && stk == s && fut == f && fop == fo && cash == c && !war && !cmdty
    };
    if others(true, false, false, false, false) { return 0; }
    if others(false, false, true, false, false) { return 2; }
    if others(true, true, false, false, false) { return 1; }
    if others(true, false, false, false, true) { return 11; }
    if others(true, false, true, false, false) { return 4; }
    if others(false, true, true, false, false) { return 3; }
    if others(true, true, true, false, false) { return 5; }
    if others(false, false, false, true, false) { return 7; }
    if others(false, false, true, true, false) { return 8; }
    if others(false, true, false, false, false) { return 9; }
    if war { return 12; }
    if cmdty { return 13; }
    -1
}

/// The combo a set-up builds, and the cache key of a built combo: the BAG
/// exchange and currency, and the legs (conId, ratio, side) by conId. The
/// BAG symbol is not part of it: a second order with another symbol on the
/// same legs sent no request (captured 26/09/2026).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ComboKey {
    exchange: String,
    currency: String,
    legs: Vec<(i64, i32, bool)>,
}

impl ComboKey {
    pub(crate) fn of(spec: &ComboSpec) -> Self {
        let mut legs: Vec<(i64, i32, bool)> = spec.legs.iter().map(|l| (l.con_id, l.ratio, l.buy)).collect();
        legs.sort_unstable();
        ComboKey { exchange: spec.exchange.to_uppercase(), currency: spec.currency.clone(), legs }
    }
}

/// The BAG definition of a smart combo currency or of a directed combo.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Bag {
    pub con_id: i64,
    /// conId of the ICS definition the record names (6346), 0 for none.
    pub ics_con_id: i64,
    pub trading_class: String,
    /// What the outside-RTH rule reads from the BAG record (ibx#465).
    pub types: RthTypes,
}

/// The step a set-up waits for.
#[derive(Debug, Clone, PartialEq)]
enum Step {
    Bag,
    Ics,
    /// The definition of the leg at this position (the caller's order).
    Leg(usize),
    /// The smart combo validations still to come.
    Validation(Vec<String>),
    Multiplier,
    Confirm,
}

/// A combo being built.
#[derive(Debug, Clone)]
struct Setup {
    key: ComboKey,
    spec: ComboSpec,
    step: Step,
    /// Request id of the step in flight (not used by `Validation`).
    req_id: String,
    bag: Option<Bag>,
    legs: Vec<Leg>,
    multiplier: f64,
    /// Orders waiting for this combo.
    waiting: Vec<OrderId>,
}

/// What a request the set-up sent and its answer did.
#[derive(Debug, Default)]
pub(crate) struct Progress {
    /// Frames to send now.
    pub send: Vec<Fields>,
    /// The waiting orders can go on: the combo is built, or the set-up
    /// failed and they get its error.
    pub release: bool,
}

/// The combo of an order sent this session, for its replace and its
/// reports.
#[derive(Debug, Clone)]
pub(crate) struct ComboOrder {
    pub combo: Combo,
    pub spec: ComboSpec,
    /// Per-leg prices in contract leg order, as sent; empty when none.
    pub leg_prices: Vec<Price>,
    /// The combo price sent: the order's limit price, or the one the leg
    /// prices give.
    pub price: Option<Price>,
    /// The combo totals of the last report of the combo, for the reports
    /// of its legs: filled so far, quantity left, average price, last
    /// price.
    pub cum_qty: crate::types::Qty,
    pub leaves_qty: crate::types::Qty,
    pub avg_price: Price,
    pub last_price: Price,
}

/// What the encoder of a combo order writes besides the order's own
/// fields (ibx#470): the combo's symbol, its block after the conId, the
/// per-leg prices after the limit price, and the limit price the per-leg
/// prices give.
#[derive(Debug, Clone, Default)]
pub(crate) struct ComboSend {
    pub symbol: String,
    pub block: Fields,
    pub leg_prices: Vec<Price>,
    pub price: Option<Price>,
}

/// Most finished combo orders kept for the reports of their legs.
const FINISHED_COMBO_ORDERS_MAX: usize = 1024;

/// The session's combos (ibx#470).
#[derive(Debug, Default)]
pub(crate) struct ComboBook {
    /// BAG definitions: (exchange, currency, symbol of a directed combo).
    bags: HashMap<(String, String, String), Bag>,
    /// Built combos.
    built: HashMap<ComboKey, Combo>,
    setups: Vec<Setup>,
    /// The error of a failed set-up for the orders that waited on it.
    failed: HashMap<OrderId, (i64, String)>,
    next_id: u32,
    /// Combo orders sent this session.
    pub(crate) orders: HashMap<OrderId, ComboOrder>,
    finished: std::collections::VecDeque<OrderId>,
}

/// Where a combo order stands before it goes out.
#[derive(Debug, PartialEq)]
pub(crate) enum Ready {
    /// Built: the order goes on with it.
    Built(Combo),
    /// It waits for the set-up; these frames go out now.
    Waiting(Vec<Fields>),
    /// The set-up failed: the order gets this error and nothing is sent.
    Refused(i64, String),
}

fn routed(exchange: &str) -> String {
    match exchange.to_uppercase().as_str() {
        "" | "SMART" => "BEST".to_string(),
        other => other.to_string(),
    }
}

fn is_smart(exchange: &str) -> bool {
    matches!(exchange.to_uppercase().as_str(), "" | "SMART")
}

fn first_tag(msg: &[u8], tag: u32) -> Option<&str> {
    let prefix = format!("{tag}=");
    msg.split(|&b| b == crate::protocol::fix::SOH)
        .filter_map(|p| std::str::from_utf8(p).ok())
        .find_map(|p| p.strip_prefix(prefix.as_str()))
}

fn api_sec_type(fix: &str) -> String {
    match fix {
        "CS" | "COMMON" => "STK".to_string(),
        "FOR" => "CASH".to_string(),
        other => other.to_string(),
    }
}

impl ComboBook {
    fn next_req(&mut self, label: &str) -> String {
        self.next_id = self.next_id.wrapping_add(1);
        format!("{}{}", label, self.next_id)
    }

    /// Where the combo of order `order_id` stands: built, waiting (the
    /// set-up starts on the first order), or refused because its set-up
    /// failed.
    pub(crate) fn ready(&mut self, order_id: OrderId, spec: &ComboSpec, ts: &str) -> Ready {
        if let Some((code, message)) = self.failed.remove(&order_id) {
            return Ready::Refused(code, message);
        }
        let key = ComboKey::of(spec);
        if let Some(combo) = self.built.get(&key) {
            return Ready::Built(combo.clone());
        }
        if let Some(setup) = self.setups.iter_mut().find(|s| s.key == key) {
            if !setup.waiting.contains(&order_id) { setup.waiting.push(order_id); }
            return Ready::Waiting(Vec::new());
        }
        let mut setup = Setup {
            key, spec: spec.clone(), step: Step::Bag, req_id: String::new(), bag: None,
            legs: Vec::new(), multiplier: 1.0, waiting: vec![order_id],
        };
        let bag_key = Self::bag_key(spec);
        let send = match self.bags.get(&bag_key).cloned() {
            Some(bag) => {
                setup.bag = Some(bag);
                self.leg_request(&mut setup, 0, ts)
            }
            None => vec![self.bag_request(&mut setup, ts)],
        };
        self.setups.push(setup);
        Ready::Waiting(send)
    }

    fn bag_key(spec: &ComboSpec) -> (String, String, String) {
        let symbol = if is_smart(&spec.exchange) { String::new() } else { spec.symbol.clone() };
        (spec.exchange.to_uppercase(), spec.currency.clone(), symbol)
    }

    /// The BAG definition request: by the smart combo conId on BEST, or
    /// by symbol on a directed exchange (captured 26/09/2026:
    /// `35=c|320=SecDefReqMsgReqByConid81|321=2|146=1|6008=28812380|6004=BEST`,
    /// `35=c|320=FixSecDefReqBySymbol102|321=2|55=QQQ,SPY|167=BAG|100=ARCA`).
    fn bag_request(&mut self, setup: &mut Setup, ts: &str) -> Fields {
        setup.step = Step::Bag;
        if is_smart(&setup.spec.exchange) {
            setup.req_id = self.next_req("SecDefReqMsgReqByConid");
            by_con_id(ts, &setup.req_id, setup.spec.smart_con_id, "BEST")
        } else {
            setup.req_id = self.next_req("FixSecDefReqBySymbol");
            vec![
                (35, "c".to_string()), (52, ts.to_string()), (320, setup.req_id.clone()), (321, "2".to_string()),
                (55, setup.spec.symbol.clone()), (167, "BAG".to_string()), (100, setup.spec.exchange.to_uppercase()),
            ]
        }
    }

    /// The definition of the leg at `index`, or the next step after the
    /// last leg.
    fn leg_request(&mut self, setup: &mut Setup, index: usize, ts: &str) -> Vec<Fields> {
        if index < setup.spec.legs.len() {
            setup.step = Step::Leg(index);
            setup.req_id = self.next_req("EComboReqByConid");
            let leg = &setup.spec.legs[index];
            let exchange = if is_smart(&setup.spec.exchange) { routed(&leg.exchange) } else { routed(&setup.spec.exchange) };
            return vec![by_con_id(ts, &setup.req_id, leg.con_id, &exchange)];
        }
        if is_smart(&setup.spec.exchange) {
            let mut ids = Vec::new();
            let mut send = Vec::new();
            for leg in &setup.spec.legs {
                let id = self.next_req("SmartComboEComboValidationProcessorReqByConid");
                send.push(by_con_id(ts, &id, leg.con_id, "ANYEXCH"));
                ids.push(id);
            }
            setup.step = Step::Validation(ids);
            send
        } else {
            vec![self.confirm_request(setup, ts)]
        }
    }

    /// The combo multiplier request (captured: `35=U|6040=36|
    /// 320=FixIMCMultReq79|207=BEST|6134=9|6079=2|...|6175=0`).
    fn multiplier_request(&mut self, setup: &mut Setup, ts: &str) -> Fields {
        setup.step = Step::Multiplier;
        setup.req_id = self.next_req("FixIMCMultReq");
        let combo = setup_combo(setup);
        let mut f = vec![
            (35, "U".to_string()), (52, ts.to_string()), (6040, "36".to_string()), (320, setup.req_id.clone()),
            (207, combo.routed.clone()), (6134, combo.category().to_string()),
        ];
        f.extend(combo.legs_fields());
        f.push((6175, "0".to_string()));
        f
    }

    /// The leg confirmation (`jcomb.fJ.a(StringBuffer)`; captured:
    /// `35=U|6040=7|320=FixComboLegConfirmRequest80|207=BEST|6134=9|6248=1|
    /// 6079=2|...|6175=0`), with the non-guaranteed flag when the order has
    /// it.
    fn confirm_request(&mut self, setup: &mut Setup, ts: &str) -> Fields {
        setup.step = Step::Confirm;
        setup.req_id = self.next_req("FixComboLegConfirmRequest");
        let combo = setup_combo(setup);
        let mut f = vec![
            (35, "U".to_string()), (52, ts.to_string()), (6040, "7".to_string()), (320, setup.req_id.clone()),
            (207, combo.routed.clone()), (6134, combo.category().to_string()),
        ];
        if setup.spec.non_guaranteed {
            f.push((6248, "1".to_string()));
        }
        f.extend(combo.legs_fields());
        f.push((6175, "0".to_string()));
        f
    }

    /// A definition reply (35=d) for a set-up request. None when the reply
    /// is not for one.
    pub(crate) fn secdef_reply(&mut self, req_id: &str, msg: &[u8], ts: &str) -> Option<Progress> {
        let idx = self.setups.iter().position(|s| match &s.step {
            Step::Validation(ids) => ids.iter().any(|id| id == req_id),
            _ => s.req_id == req_id,
        })?;
        let mut setup = self.setups.swap_remove(idx);
        let records = crate::control::contracts::parse_secdef_records(msg).unwrap_or_default();
        let result = match setup.step.clone() {
            Step::Bag => match records.first() {
                Some(def) => {
                    let tokens: Vec<String> = first_tag(msg, 6431)
                        .map(|v| v.split(',').map(String::from).collect()).unwrap_or_default();
                    let bag = Bag {
                        con_id: def.con_id,
                        ics_con_id: first_tag(msg, 6346).and_then(|v| v.parse().ok()).unwrap_or(0),
                        trading_class: first_tag(msg, 6058).or_else(|| first_tag(msg, 58)).unwrap_or("").to_string(),
                        types: RthTypes::from_definition(&tokens, first_tag(msg, 6523).unwrap_or(""), "BAG", &def.currency),
                    };
                    self.bags.insert(Self::bag_key(&setup.spec), bag.clone());
                    setup.bag = Some(bag.clone());
                    if bag.ics_con_id > 0 {
                        setup.step = Step::Ics;
                        setup.req_id = self.next_req("Query ICS typeReqByConid");
                        let exchange = routed(&setup.spec.exchange);
                        Ok(vec![by_con_id(ts, &setup.req_id, bag.ics_con_id, &exchange)])
                    } else {
                        Ok(self.leg_request(&mut setup, 0, ts))
                    }
                }
                None => Err((NO_DEFINITION.0, NO_DEFINITION.1.to_string())),
            },
            Step::Ics => Ok(self.leg_request(&mut setup, 0, ts)),
            Step::Leg(i) => {
                let spec_leg = setup.spec.legs[i].clone();
                match records.iter().find(|d| d.con_id == spec_leg.con_id).or(records.first()) {
                    Some(def) => {
                        let sec_type = api_sec_type(first_tag(msg, 167).unwrap_or(""));
                        if sec_type == "STK" && !is_smart(&setup.spec.exchange) {
                            // `jextend.av.a(jsecdef.f, boolean)@219-280`.
                            Err((10125, format!(
                                "Combos with STK legs are only supported on SMART. Please change the exchange for your BAG contract from '{}' to 'SMART'.",
                                setup.spec.exchange)))
                        } else {
                            setup.legs.push(Leg {
                                con_id: spec_leg.con_id,
                                ratio: spec_leg.ratio,
                                buy: spec_leg.buy,
                                exchange: spec_leg.exchange.clone(),
                                symbol: def.symbol.clone(),
                                sec_type,
                                currency: def.currency.clone(),
                                multiplier: if def.multiplier > 0.0 { def.multiplier } else { 1.0 },
                            });
                            Ok(self.leg_request(&mut setup, i + 1, ts))
                        }
                    }
                    None => Err((NO_DEFINITION.0, NO_DEFINITION.1.to_string())),
                }
            }
            Step::Validation(mut ids) => {
                ids.retain(|id| id != req_id);
                if ids.is_empty() {
                    Ok(vec![self.multiplier_request(&mut setup, ts)])
                } else {
                    setup.step = Step::Validation(ids);
                    self.setups.push(setup);
                    return Some(Progress::default());
                }
            }
            Step::Multiplier | Step::Confirm => {
                self.setups.push(setup);
                return None;
            }
        };
        Some(self.advance(setup, result))
    }

    /// A 35=U answer (6040=36, 6040=7) for a set-up request. None when the
    /// answer is not for one.
    pub(crate) fn user_reply(&mut self, parsed: &HashMap<u32, String>, ts: &str) -> Option<Progress> {
        let req_id = parsed.get(&320)?;
        let idx = self.setups.iter().position(|s| &s.req_id == req_id
            && matches!(s.step, Step::Multiplier | Step::Confirm))?;
        let mut setup = self.setups.swap_remove(idx);
        let result = match setup.step {
            Step::Multiplier => {
                setup.multiplier = parsed.get(&231).and_then(|v| v.parse::<f64>().ok())
                    .filter(|m| *m > 0.0).unwrap_or(1.0);
                Ok(vec![self.confirm_request(&mut setup, ts)])
            }
            _ => {
                // The combo key (6085) gives the legs; the order keeps
                // them by conId.
                let combo = setup_combo(&setup);
                log::info!("Combo {} built: BAG conId {}, key {:?}", combo.symbol(), combo.bag_con_id, parsed.get(&6085));
                self.built.insert(setup.key.clone(), combo);
                for oid in &setup.waiting {
                    self.failed.remove(oid);
                }
                return Some(Progress { send: Vec::new(), release: true });
            }
        };
        Some(self.advance(setup, result))
    }

    fn advance(&mut self, setup: Setup, result: Result<Vec<Fields>, (i64, String)>) -> Progress {
        match result {
            Ok(send) => {
                self.setups.push(setup);
                Progress { send, release: false }
            }
            Err((code, message)) => {
                log::warn!("Combo set-up failed ({code}): {message}");
                for oid in setup.waiting {
                    self.failed.insert(oid, (code, message.clone()));
                }
                Progress { send: Vec::new(), release: true }
            }
        }
    }

    /// The one built combo of the session (test-only).
    #[cfg(test)]
    pub(crate) fn built_for_test(&self) -> Option<Combo> {
        self.built.values().next().cloned()
    }

    /// The BAG definition of a built combo, for the outside-RTH rule.
    pub(crate) fn bag_types(&self, combo: &Combo) -> Option<&RthTypes> {
        self.bags.values().find(|b| b.con_id == combo.bag_con_id).map(|b| &b.types)
    }

    /// Keep the combo of an order that goes out.
    pub(crate) fn track(&mut self, order_id: OrderId, order: ComboOrder) {
        self.orders.insert(order_id, order);
    }

    /// The order is finished: its combo is kept a while for the reports of
    /// its legs, which come after the combo's last report.
    pub(crate) fn finished(&mut self, order_id: OrderId) {
        if !self.orders.contains_key(&order_id) || self.finished.contains(&order_id) { return; }
        self.finished.push_back(order_id);
        while self.finished.len() > FINISHED_COMBO_ORDERS_MAX {
            if let Some(old) = self.finished.pop_front() {
                self.orders.remove(&old);
            }
        }
    }

    /// Set-ups in flight are lost with the order connection: they start
    /// again for the waiting orders. True when some order waited.
    pub(crate) fn connection_lost(&mut self) -> bool {
        let waited = !self.setups.is_empty();
        self.setups.clear();
        waited
    }
}

/// A definition request by conId (`jfix.bf.a(StringBuffer,
/// ArConidExchangePair, int, int)`): `35=c|320={id}|321=2|146=1|6008=
/// {conId}|6004={exchange}`.
fn by_con_id(ts: &str, req_id: &str, con_id: i64, exchange: &str) -> Fields {
    vec![
        (35, "c".to_string()), (52, ts.to_string()), (320, req_id.to_string()), (321, "2".to_string()),
        (146, "1".to_string()), (6008, con_id.to_string()), (6004, exchange.to_string()),
    ]
}

/// The combo a set-up has built so far, its legs in contract order.
fn setup_combo(setup: &Setup) -> Combo {
    let mut legs = setup.legs.clone();
    legs.sort_by_key(|l| l.con_id);
    let bag = setup.bag.clone();
    Combo {
        bag_con_id: bag.as_ref().map_or(setup.spec.smart_con_id, |b| b.con_id),
        exchange: setup.spec.exchange.clone(),
        routed: routed(&setup.spec.exchange),
        currency: setup.spec.currency.clone(),
        trading_class: bag.map(|b| b.trading_class).unwrap_or_default(),
        legs,
        multiplier: setup.multiplier,
    }
}

/// The reference's checks of a built combo before the order goes out, in
/// its order, with the code itself (`jextend.dC.a(jclient.dy)`,
/// `trader.order.proc.aN`): legs in more than one currency (10000), a BAG
/// symbol that is not among the legs' (478), a non-guaranteed smart combo
/// without exactly two legs (10002).
pub(crate) fn order_refusal(combo: &Combo, spec: &ComboSpec) -> Option<(i64, String)> {
    if combo.legs.windows(2).any(|w| w[0].currency != w[1].currency) {
        return Some((10000, "Cross-currency combo order is not supported.".into()));
    }
    let legs = combo.symbol();
    if !spec.symbol.is_empty() && spec.symbol != "USD" && !legs.contains(spec.symbol.as_str()) {
        return Some((478, format!(
            "Parameters in request conflicts with contract parameters received by contract id:Requested symbol {}, in legs {}",
            spec.symbol, legs)));
    }
    if is_smart(&spec.exchange) && spec.non_guaranteed && combo.legs.len() != 2 {
        return Some((10002, "Non-guaranteed order should only have two legs".into()));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ComboLegSpec;

    fn spy_qqq(symbol: &str) -> ComboSpec {
        ComboSpec {
            exchange: "SMART".into(), currency: "USD".into(), symbol: symbol.into(), smart_con_id: 28812380,
            legs: vec![
                ComboLegSpec { con_id: 756733, ratio: 1, buy: true, exchange: "SMART".into() },
                ComboLegSpec { con_id: 320227571, ratio: 1, buy: false, exchange: "SMART".into() },
            ],
            leg_prices: Vec::new(),
            routing_attrs: vec![(6248, "1".into())],
            non_guaranteed: true,
        }
    }

    fn text(f: &Fields) -> String {
        f.iter().filter(|(t, _)| *t != 52).map(|(t, v)| format!("{t}={v}")).collect::<Vec<_>>().join("|")
    }

    fn frame(s: &str) -> Vec<u8> {
        s.replace('|', "\x01").into_bytes()
    }

    #[test]
    fn categories_of_the_reference() {
        assert_eq!(category(["STK", "STK"].into_iter()), 9);
        assert_eq!(category(["OPT", "OPT"].into_iter()), 0);
        assert_eq!(category(["OPT", "STK"].into_iter()), 1);
        assert_eq!(category(["FUT"].into_iter()), 2);
        assert_eq!(category(["FOP", "FUT"].into_iter()), 8);
        assert_eq!(category(["CFD", "STK"].into_iter()), 14);
        assert_eq!(category(["WAR", "STK"].into_iter()), 12);
        assert_eq!(category(["IND"].into_iter()), -1);
    }

    #[test]
    fn leg_prices_give_the_combo_price() {
        let combo = Combo {
            bag_con_id: 28812380, exchange: "SMART".into(), routed: "BEST".into(), currency: "USD".into(),
            trading_class: "COMB".into(), multiplier: 1.0,
            legs: vec![
                Leg { con_id: 756733, ratio: 1, buy: true, exchange: "SMART".into(), symbol: "SPY".into(),
                    sec_type: "STK".into(), currency: "USD".into(), multiplier: 1.0 },
                Leg { con_id: 320227571, ratio: 1, buy: false, exchange: "SMART".into(), symbol: "QQQ".into(),
                    sec_type: "STK".into(), currency: "USD".into(), multiplier: 1.0 },
            ],
        };
        let px = |v: f64| (v * PRICE_SCALE as f64).round() as Price;
        let mut spec = spy_qqq("QQQ,SPY");
        spec.leg_prices = vec![px(721.35), px(794.50)];
        let prices = combo.leg_prices(&spec);
        assert_eq!(prices, vec![px(721.35), px(794.50)]);
        assert_eq!(combo.price_of_legs(&prices), Some(px(-73.15)));
        assert_eq!(combo.symbol(), "QQQ,SPY");
        assert_eq!(combo.legs_descrip(), "756733|1,320227571|-1");
        assert_eq!(text(&combo.order_block()), "6079=2|6080=756733|6081=1|6082=1|6080=320227571|6081=1|6082=0|6175=0|6134=9");
    }

    // ibx#470, the set-up of the first combo order of a session (captured
    // 26/09/2026, ib-agent captures/105/20260926 i105_combo_stock_smart):
    // the BAG, the ICS, each leg in turn, both validations at once, the
    // multiplier, the leg confirmation; then the combo is kept.
    #[test]
    fn smart_combo_set_up_as_the_reference() {
        let mut book = ComboBook::default();
        let spec = spy_qqq("QQQ,SPY");
        let Ready::Waiting(send) = book.ready(1, &spec, "T") else { panic!() };
        assert_eq!(send.iter().map(text).collect::<Vec<_>>(), ["35=c|320=SecDefReqMsgReqByConid1|321=2|146=1|6008=28812380|6004=BEST"]);
        assert_eq!(book.ready(2, &spec, "T"), Ready::Waiting(Vec::new()), "a second order waits on the same set-up");

        let bag = frame("35=d|320=SecDefReqMsgReqByConid1|322=*|323=4|55=USD|167=BAG|207=BEST|6008=28812380|15=USD|58=COMB|6035=28812380|6058=COMB|146=1|6038=Y|6019=0|6344=1|6008=28812380|6346=32636645|6921=IBCX|6432=1|6430=5/COMB/IBCX#NOAON|6431=LMT/3,MKT/3,RTH/1");
        let p = book.secdef_reply("SecDefReqMsgReqByConid1", &bag, "T").unwrap();
        assert_eq!(p.send.iter().map(text).collect::<Vec<_>>(), ["35=c|320=Query ICS typeReqByConid2|321=2|146=1|6008=32636645|6004=BEST"]);
        let p = book.secdef_reply("Query ICS typeReqByConid2", &frame("35=d|320=Query ICS typeReqByConid2|322=*|323=4|6038=Y|6019=0|6344=0"), "T").unwrap();
        assert_eq!(p.send.iter().map(text).collect::<Vec<_>>(), ["35=c|320=EComboReqByConid3|321=2|146=1|6008=756733|6004=BEST"]);
        let spy = frame("35=d|320=EComboReqByConid3|322=*|323=4|55=SPY|167=STK|207=BEST|6008=756733|15=USD|146=1|6038=Y|6019=0|6344=1|6008=756733");
        let p = book.secdef_reply("EComboReqByConid3", &spy, "T").unwrap();
        assert_eq!(p.send.iter().map(text).collect::<Vec<_>>(), ["35=c|320=EComboReqByConid4|321=2|146=1|6008=320227571|6004=BEST"]);
        let qqq = frame("35=d|320=EComboReqByConid4|322=*|323=4|55=QQQ|167=STK|207=BEST|6008=320227571|15=USD|146=1|6038=Y|6019=0|6344=1|6008=320227571");
        let p = book.secdef_reply("EComboReqByConid4", &qqq, "T").unwrap();
        assert_eq!(p.send.iter().map(text).collect::<Vec<_>>(), [
            "35=c|320=SmartComboEComboValidationProcessorReqByConid5|321=2|146=1|6008=756733|6004=ANYEXCH",
            "35=c|320=SmartComboEComboValidationProcessorReqByConid6|321=2|146=1|6008=320227571|6004=ANYEXCH",
        ]);
        let p = book.secdef_reply("SmartComboEComboValidationProcessorReqByConid5", &spy, "T").unwrap();
        assert!(p.send.is_empty() && !p.release);
        let p = book.secdef_reply("SmartComboEComboValidationProcessorReqByConid6", &qqq, "T").unwrap();
        assert_eq!(p.send.iter().map(text).collect::<Vec<_>>(),
            ["35=U|6040=36|320=FixIMCMultReq7|207=BEST|6134=9|6079=2|6080=756733|6081=1|6082=1|6080=320227571|6081=1|6082=0|6175=0"]);
        let mult: HashMap<u32, String> = [(320, "FixIMCMultReq7".to_string()), (231, "1".to_string())].into();
        let p = book.user_reply(&mult, "T").unwrap();
        assert_eq!(p.send.iter().map(text).collect::<Vec<_>>(),
            ["35=U|6040=7|320=FixComboLegConfirmRequest8|207=BEST|6134=9|6248=1|6079=2|6080=756733|6081=1|6082=1|6080=320227571|6081=1|6082=0|6175=0"]);
        let confirm: HashMap<u32, String> = [(320, "FixComboLegConfirmRequest8".to_string()), (6085, "756733/1,320227571/-1".to_string())].into();
        let p = book.user_reply(&confirm, "T").unwrap();
        assert!(p.release && p.send.is_empty());

        let Ready::Built(combo) = book.ready(1, &spec, "T") else { panic!() };
        assert_eq!((combo.bag_con_id, combo.symbol(), combo.trading_class.as_str()), (28812380, "QQQ,SPY".to_string(), "COMB"));
        // Another symbol on the same legs: no request (captured 26/09/2026b).
        assert!(matches!(book.ready(3, &spy_qqq("SPY,QQQ"), "T"), Ready::Built(_)));
        assert_eq!(order_refusal(&combo, &spy_qqq("SPY,QQQ")), Some((478,
            "Parameters in request conflicts with contract parameters received by contract id:Requested symbol SPY,QQQ, in legs QQQ,SPY".to_string())));
        assert_eq!(order_refusal(&combo, &spy_qqq("QQQ,SPY")), None);
        assert_eq!(order_refusal(&combo, &spy_qqq("USD")), None);
        assert_eq!(order_refusal(&combo, &spy_qqq("")), None);
    }

    // ibx#470 (captured 26/09/2026, ARCA): a directed combo asks the BAG by
    // symbol on its exchange; an empty answer gives error 200 to every
    // order that waited on it.
    #[test]
    fn directed_combo_without_definition_gives_200() {
        let mut book = ComboBook::default();
        let mut spec = spy_qqq("QQQ,SPY");
        spec.exchange = "ARCA".into();
        spec.smart_con_id = 0;
        let Ready::Waiting(send) = book.ready(26, &spec, "T") else { panic!() };
        assert_eq!(send.iter().map(text).collect::<Vec<_>>(), ["35=c|320=FixSecDefReqBySymbol1|321=2|55=QQQ,SPY|167=BAG|100=ARCA"]);
        let p = book.secdef_reply("FixSecDefReqBySymbol1", &frame("35=d|320=FixSecDefReqBySymbol1|322=*|323=4|6038=Y|6019=0|6344=0"), "T").unwrap();
        assert!(p.release);
        assert_eq!(book.ready(26, &spec, "T"), Ready::Refused(200, NO_DEFINITION.1.to_string()));
        // A later order starts again.
        assert!(matches!(book.ready(27, &spec, "T"), Ready::Waiting(s) if s.len() == 1));
    }

    #[test]
    fn stock_legs_of_a_directed_combo_give_10125() {
        let mut book = ComboBook::default();
        let mut spec = spy_qqq("QQQ,SPY");
        spec.exchange = "CBOE".into();
        let _ = book.ready(5, &spec, "T");
        let bag = frame("35=d|320=FixSecDefReqBySymbol1|322=*|323=4|55=SPY|167=BAG|207=CBOE|6008=32880942|15=USD|58=COMB|146=1|6038=Y|6019=0|6344=1|6008=32880942");
        let p = book.secdef_reply("FixSecDefReqBySymbol1", &bag, "T").unwrap();
        assert_eq!(p.send.iter().map(text).collect::<Vec<_>>(), ["35=c|320=EComboReqByConid2|321=2|146=1|6008=756733|6004=CBOE"]);
        let spy = frame("35=d|320=EComboReqByConid2|322=*|323=4|55=SPY|167=STK|207=CBOE|6008=756733|15=USD|146=1|6038=Y|6019=0");
        assert!(book.secdef_reply("EComboReqByConid2", &spy, "T").unwrap().release);
        assert_eq!(book.ready(5, &spec, "T"), Ready::Refused(10125,
            "Combos with STK legs are only supported on SMART. Please change the exchange for your BAG contract from 'CBOE' to 'SMART'.".into()));
    }

    #[test]
    fn non_guaranteed_needs_two_legs_and_one_currency() {
        let leg = |con_id: i64, currency: &str| Leg { con_id, ratio: 1, buy: true, exchange: "SMART".into(),
            symbol: format!("S{con_id}"), sec_type: "STK".into(), currency: currency.into(), multiplier: 1.0 };
        let combo = |legs: Vec<Leg>| Combo { bag_con_id: 1, exchange: "SMART".into(), routed: "BEST".into(),
            currency: "USD".into(), trading_class: "COMB".into(), legs, multiplier: 1.0 };
        let spec = spy_qqq("");
        assert_eq!(order_refusal(&combo(vec![leg(1, "USD"), leg(2, "USD"), leg(3, "USD")]), &spec).map(|r| r.0), Some(10002));
        assert_eq!(order_refusal(&combo(vec![leg(1, "USD"), leg(2, "EUR")]), &spec).map(|r| r.0), Some(10000));
    }
}
