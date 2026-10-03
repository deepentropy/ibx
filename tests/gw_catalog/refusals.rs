//! Local refusals of an order (ibx#485 item 2): one test per gateway rule
//! of `order_local_rules.csv` that ibx implements. The API order goes in
//! through `place_order` (or `cancel_order`), the error out is the rule's
//! code and text, and the server gets no order message for it: a valid
//! order placed after it is the only one the in-memory server reads.
//!
//! The rules ibx does not implement are rows of the coverage matrix marked
//! open with their issue.

use ibx::api::client::{Contract, Order, TagValue};
use ibx::api::types::ComboLeg;
use ibx::types::OrderCondition;

use super::catalog::{local_rule, matches_template};
use super::harness::{aapl, field, lmt, Engine};

/// The order id of the refused order, and of the valid one after it.
const REFUSED: i64 = 7;
const NEXT: i64 = 900;

/// `engine` refused order `id` with `rule`'s code and text (its `%s` filled
/// with any value) and sent nothing for it.
fn assert_refused(engine: &mut Engine, id: i64, rule: &str) {
    let rule = local_rule(rule);
    // The engine's refusals come from its thread.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut errors = Vec::new();
    while !errors.iter().any(|e: &(i64, i64, String)| e.0 == id) && std::time::Instant::now() < deadline {
        errors.extend(engine.errors());
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    let mine: Vec<&(i64, i64, String)> = errors.iter().filter(|e| e.0 == id).collect();
    assert!(
        mine.iter().any(|(_, code, text)| *code == rule.api_code && matches_template(&rule.text, text, false)),
        "rule {} (gateway code {}): want {} {:?}, got {:?}", rule.id, rule.code, rule.api_code, rule.text, errors,
    );
    // Nothing sent for the refused order: the next order is the first the
    // server reads after it.
    let before = engine.orders(0).len();
    engine.client.place_order(NEXT, &aapl(), &lmt("BUY", 1.0, 100.0)).unwrap();
    let frames = engine.orders(before + 1);
    let after = &frames[before..];
    assert!(!after.is_empty(), "the valid order after it is sent");
    let id_text = id.to_string();
    let refused: Vec<String> = frames.iter()
        .filter(|f| field(f, 6121) == Some(id_text.as_str())
            || field(f, 11).is_some_and(|c| c.split('.').next() == Some(id_text.as_str())))
        .map(|f| ibx::test_support::to_pipe(f))
        .collect();
    assert!(refused.is_empty(), "rule {}: frames sent for the refused order: {refused:?}", rule.id);
}

/// Place `order` on `contract` and check `rule`.
fn refused(rule: &str, contract: &Contract, order: &Order) {
    let mut engine = Engine::start();
    engine.client.place_order(REFUSED, contract, order).expect("a refusal is an error callback, not a call error");
    assert_refused(&mut engine, REFUSED, rule);
}

fn order(order_type: &str) -> Order {
    Order { order_type: order_type.into(), ..lmt("BUY", 1.0, 100.0) }
}

// ── The reading of the order: 320 with the rule text, or the date codes ──

#[test]
fn good_till_date_not_a_date() {
    refused("343 bH.q@2674", &aapl(), &Order { tif: "GTD".into(), good_till_date: "next week".into(), ..order("LMT") });
}

#[test]
fn good_after_time_not_a_date() {
    refused("337 bH.q@2497", &aapl(), &Order { good_after_time: "after lunch".into(), ..order("LMT") });
}

#[test]
fn negative_discretionary_amount() {
    refused("168 bH.q@2411", &aapl(), &Order { discretionary_amt: -0.5, ..order("LMT") });
}

#[test]
fn trailing_percent_with_a_trailing_amount() {
    refused("10060 bH.q@5607", &aapl(), &Order {
        action: "SELL".into(), trailing_percent: 1.0, aux_price: 1.0, ..order("TRAIL")
    });
}

#[test]
fn time_condition_not_a_time() {
    refused("10314 am.a@103", &aapl(), &Order {
        conditions: vec![OrderCondition::Time { time: "tomorrow".into(), is_more: true }],
        ..order("LMT")
    });
}

fn bag(legs: Vec<ComboLeg>) -> Contract {
    Contract {
        con_id: 28812380, symbol: "SPY,QQQ".into(), sec_type: "BAG".into(), exchange: "SMART".into(),
        currency: "USD".into(), combo_legs: legs, ..Default::default()
    }
}

fn leg(con_id: i64, action: &str) -> ComboLeg {
    ComboLeg { con_id, ratio: 1, action: action.into(), exchange: "SMART".into(), exempt_code: -1, ..Default::default() }
}

#[test]
fn per_leg_prices_not_one_per_leg() {
    refused("10057 bH.q@2126", &bag(vec![leg(756733, "BUY"), leg(320227571, "SELL")]),
        &Order { order_combo_legs: vec![1.0], ..order("LMT") });
}

#[test]
fn unknown_combo_routing_tag() {
    refused("10027 bH.q@2254", &bag(vec![leg(756733, "BUY"), leg(320227571, "SELL")]), &Order {
        smart_combo_routing_params: vec![TagValue { tag: "Bogus".into(), value: "1".into() }],
        ..order("LMT")
    });
}

// ── The first checks: 321 with the rule text ──

#[test]
fn combo_without_legs() {
    refused("314 bH.S@2319", &bag(Vec::new()), &order("LMT"));
}

#[test]
fn smart_combo_in_a_currency_without_its_conid() {
    // The logon gave no smart combo conId for any currency.
    refused("10011 bH.S@2410", &bag(vec![leg(756733, "BUY"), leg(320227571, "SELL")]), &order("LMT"));
}

#[test]
fn quantity_out_of_range() {
    refused("355 bH.S@1606", &aapl(), &Order { total_quantity: 2e9, ..order("LMT") });
}

#[test]
fn trail_limit_without_a_stop_price() {
    refused("321 bH.S@1750", &aapl(), &Order {
        action: "SELL".into(), aux_price: 1.0, lmt_price_offset: 0.5, lmt_price: f64::MAX, trail_stop_price: f64::MAX,
        ..order("TRAIL LIMIT")
    });
}

#[test]
fn trigger_method_not_the_gateways() {
    refused("146 bH.S@2257", &aapl(), &Order {
        action: "SELL".into(), aux_price: 90.0, trigger_method: 5, ..order("STP")
    });
}

#[test]
fn what_if_without_transmit() {
    refused("413 bH.S@4745", &aapl(), &Order { what_if: true, transmit: false, ..order("LMT") });
}

#[test]
fn trailing_percent_above_100() {
    refused("10061 bH.S@5907", &aapl(), &Order {
        action: "SELL".into(), trailing_percent: 150.0, ..order("TRAIL")
    });
}

#[test]
fn trail_limit_with_both_limit_fields() {
    refused("321 bH.S@7210", &aapl(), &Order {
        action: "SELL".into(), aux_price: 1.0, lmt_price: 99.0, lmt_price_offset: 0.5, trail_stop_price: 99.5,
        ..order("TRAIL LIMIT")
    });
}

#[test]
fn midprice_outside_regular_hours() {
    refused("10210 bH.S@7521", &aapl(), &Order { outside_rth: true, ..order("MIDPRICE") });
}

#[test]
fn customer_account_without_the_account_config() {
    refused("145 bH.S@8849", &aapl(), &Order { customer_account: "CUST1".into(), ..order("LMT") });
}

#[test]
fn professional_customer_without_the_account_config() {
    refused("145 bH.S@8892", &aapl(), &Order { professional_customer: true, ..order("LMT") });
}

#[test]
fn short_side_without_the_logon_flags() {
    refused("321 bH.S@1380", &aapl(), &Order { action: "SSHORT".into(), ..order("LMT") });
}

/// An institutional session (the logon's super-user flag) for the short
/// sale slot rules.
fn short_sale(rule: &str, slot: i32, location: &str, exempt_code: i32) {
    let mut engine = Engine::start();
    engine.shared.reference.set_short_sale_flags(true, false);
    let order = Order {
        action: "SSHORT".into(), short_sale_slot: slot, designated_location: location.into(), exempt_code,
        ..order("LMT")
    };
    engine.client.place_order(REFUSED, &aapl(), &order).unwrap();
    assert_refused(&mut engine, REFUSED, rule);
}

#[test]
fn short_sale_slot_not_1_or_2() {
    short_sale("347 bH.S@3088", 3, "", -1);
}

#[test]
fn short_sale_exempt_code_on_a_short_sale() {
    short_sale("494 bH.S@2921", 1, "", 1);
}

#[test]
fn short_sale_slot_1_with_a_location() {
    short_sale("353 bH.S@3146", 1, "XYZ", -1);
}

#[test]
fn short_sale_slot_2_without_a_location() {
    short_sale("352 bH.S@3204", 2, "", -1);
}

#[test]
fn short_sale_slot_on_a_non_institutional_session() {
    // The omnibus flag alone: a short side is allowed, a slot is not.
    let mut engine = Engine::start();
    engine.shared.reference.set_short_sale_flags(false, true);
    let order = Order { action: "SSHORT".into(), short_sale_slot: 1, ..order("LMT") };
    engine.client.place_order(REFUSED, &aapl(), &order).unwrap();
    assert_refused(&mut engine, REFUSED, "346 bH.S@2702");
}

// ── The API order build ──

#[test]
fn fractional_quantity() {
    refused("10243 dx.a(dy,boolean)@244", &aapl(), &Order { total_quantity: 1.5, ..order("LMT") });
}

// ── The order id flow ──

#[test]
fn order_id_zero() {
    let mut engine = Engine::start();
    engine.client.place_order(0, &aapl(), &order("LMT")).expect("a refusal is an error callback");
    assert_refused(&mut engine, 0, "10149 bH.W@44");
}

#[test]
fn a_new_order_id_below_the_highest_used() {
    let mut engine = Engine::start();
    engine.client.place_order(50, &aapl(), &order("LMT")).unwrap();
    assert_eq!(engine.orders(1).len(), 1, "the first order is sent");
    engine.client.place_order(40, &aapl(), &order("LMT")).unwrap();
    assert_refused(&mut engine, 40, "103 bH.W@222");
}

// ── Cancel ──

#[test]
fn cancel_of_an_unknown_order() {
    let mut engine = Engine::start();
    engine.client.cancel_order(REFUSED, "").unwrap();
    assert_refused(&mut engine, REFUSED, "10147 bw.a(pe)@179");
}

#[test]
fn cancel_of_an_order_with_a_cancel_pending() {
    let mut engine = Engine::start();
    engine.client.place_order(REFUSED, &aapl(), &order("LMT")).unwrap();
    engine.client.cancel_order(REFUSED, "").unwrap();
    let frames = engine.orders(2);
    assert_eq!(frames.iter().filter(|f| field(f, 35) == Some("F")).count(), 1, "the first cancel is sent");
    engine.client.cancel_order(REFUSED, "").unwrap();
    let rule = local_rule("10148 bw.a(pe)@543");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let errors = loop {
        let errors = engine.errors();
        if !errors.is_empty() || std::time::Instant::now() >= deadline {
            break errors;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    };
    assert!(errors.iter().any(|(id, code, text)| *id == REFUSED && *code == rule.api_code
        && matches_template(&rule.text, text, false)), "want {:?}, got {errors:?}", rule.text);
    let frames = engine.orders(3);
    assert_eq!(frames.iter().filter(|f| field(f, 35) == Some("F")).count(), 1, "no second cancel");
}

// ── Modify ──

/// Place `first` on `contract`, then the same order id again as
/// `second` on `second_contract`: refused with `rule`, no replace sent.
fn modify_refused(rule: &str, contract: &Contract, first: &Order, second_contract: &Contract, second: &Order) {
    let mut engine = Engine::start();
    engine.client.place_order(REFUSED, contract, first).unwrap();
    engine.client.place_order(REFUSED, second_contract, second).unwrap();
    let rule = local_rule(rule);
    let errors = engine.errors();
    assert!(errors.iter().any(|(id, code, text)| *id == REFUSED && *code == rule.api_code
        && matches_template(&rule.text, text, false)), "rule {}: want {:?}, got {errors:?}", rule.id, rule.text);
    // A valid order after it: the replace was not sent before it.
    engine.client.place_order(NEXT, &aapl(), &lmt("BUY", 1.0, 100.0)).unwrap();
    let next = NEXT.to_string();
    let frames = engine.orders(2);
    assert!(frames.iter().any(|f| field(f, 6121) == Some(next.as_str())), "the valid order is sent");
    assert!(frames.iter().all(|f| field(f, 35) != Some("G")), "rule {}: no replace sent", rule.id);
}

#[test]
fn modify_to_another_order_type() {
    let stp = Order { aux_price: 90.0, ..order("STP") };
    modify_refused("329 aj.e(pe)@52", &aapl(), &order("LMT"), &aapl(), &stp);
}

#[test]
fn modify_of_the_side() {
    modify_refused("105 bH.d(pe)@165", &aapl(), &order("LMT"), &aapl(), &Order { action: "SELL".into(), ..order("LMT") });
}

#[test]
fn modify_of_the_oca_group() {
    let first = Order { oca_group: "A".into(), ..order("LMT") };
    modify_refused("10326 bH.d(pe)@836", &aapl(), &first, &aapl(), &Order { oca_group: "B".into(), ..first.clone() });
}

#[test]
fn modify_of_the_oca_type() {
    let first = Order { oca_group: "A".into(), oca_type: 3, ..order("LMT") };
    modify_refused("10327 bH.d(pe)@905", &aapl(), &first, &aapl(), &Order { oca_type: 1, ..first.clone() });
}

#[test]
fn combo_modify_with_another_leg() {
    let directed = |legs| Contract { exchange: "ARCA".into(), ..bag(legs) };
    let first = directed(vec![leg(756733, "BUY"), leg(320227571, "SELL")]);
    let second = directed(vec![leg(756733, "BUY"), leg(9999, "SELL")]);
    modify_refused("10059 bH.c(pe,dl)@1067", &first, &order("LMT"), &second, &order("LMT"));
}

#[test]
fn stop_order_without_a_stop_price() {
    // The API's unset value.
    refused("321 bH.S@1750", &aapl(), &Order { action: "SELL".into(), aux_price: f64::MAX, ..order("STP") });
}

#[test]
fn stop_order_with_a_stop_price_not_a_number() {
    refused("403 bH.S@6696", &aapl(), &Order { action: "SELL".into(), aux_price: f64::NAN, ..order("STP") });
}

#[test]
fn touched_order_without_a_trigger_price() {
    refused("361 bH.S@6766", &aapl(), &Order { aux_price: f64::MAX, ..order("MIT") });
}

#[test]
fn unknown_action() {
    refused("321 bH.S@1380", &aapl(), &Order { action: "HOLD".into(), ..order("LMT") });
    refused("321 bH.S@1380", &aapl(), &Order { action: String::new(), ..order("LMT") });
}
