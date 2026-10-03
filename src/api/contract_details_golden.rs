//! Golden test of contract details rows (ibx#436): ibx's rows of the
//! captured definition replies, field by field against the rows the
//! official client received from the gateway (paper, 28/09/2026 and
//! 02/10/2026; fixture made from the four-leg captures).

use std::collections::HashMap;

use serde_json::{json, Value};

use super::types::ContractDetails;
use crate::control::contracts::row_of_reply;

/// The fixture: frames by id, and one entry per captured API row.
pub(crate) fn fixture() -> Value {
    let path = format!("{}/tests/fixtures/contract_details/golden_rows.json", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("fixture json")
}

/// The ibx row of a fixture entry.
pub(crate) fn row_of(fixture: &Value, row: &Value) -> crate::control::contracts::ContractDefinition {
    let frame = |id: &Value| id.as_str().map(|id| fixture["frames"][id].as_str().expect("frame").as_bytes().to_vec());
    let secdef = frame(&row["secdef"]).expect("secdef");
    let schedule = frame(&row["schedule"]);
    let expected = &row["expected"]["contract"];
    let rules: HashMap<(i64, String), u32> = row["rules"].as_array().unwrap().iter()
        .map(|r| ((expected["conId"].as_i64().unwrap(), r[0].as_str().unwrap().to_string()), r[1].as_u64().unwrap() as u32))
        .collect();
    let now = capture_time(row["ts"].as_str().unwrap());
    // The company lookups the gateway had made before the reply.
    let companies: Vec<Vec<u8>> = row["company"].as_array().unwrap().iter().map(|id| frame(id).unwrap()).collect();
    let companies: Vec<&[u8]> = companies.iter().map(|c| c.as_slice()).collect();
    row_of_reply(
        &secdef,
        expected["conId"].as_i64().unwrap(),
        expected["exchange"].as_str().unwrap_or(""),
        row["continuous"].as_bool().unwrap(),
        &rules,
        &companies,
        schedule.as_deref(),
        now,
        false,
        false,
    ).unwrap_or_else(|| panic!("no record for {}", row["req_id"]))
}

/// The instant of a row: its time in the capture log (Paris time).
pub(crate) fn capture_time(ts: &str) -> jiff::Timestamp {
    jiff::civil::DateTime::strptime("%d/%m/%Y %H:%M:%S%.f", ts).unwrap()
        .to_zoned(jiff::tz::TimeZone::get("Europe/Paris").unwrap()).unwrap()
        .timestamp()
}

/// The value of an API field as the client decodes it: the field of the
/// row, or its default when the row does not carry it.
fn expected_value(row: &Value, path: &[&str], default: Value) -> Value {
    let mut v = row;
    for p in path {
        v = &v[*p];
    }
    if v.is_null() { default } else { v.clone() }
}

fn num(v: f64) -> Value {
    json!(v)
}

/// The decimal fields the client receives as text.
fn decimal(v: f64) -> Value {
    if v == f64::MAX { json!("unset") } else { json!(v) }
}

fn expected_decimal(row: &Value, name: &str) -> Value {
    match row[name].as_str() {
        None => json!("unset"),
        Some(s) => s.parse::<f64>().map(|v| json!(v)).unwrap_or(json!("unset")),
    }
}

/// Every API field of a row: (name, expected, ibx).
pub(crate) fn compare(expected: &Value, d: &ContractDetails) -> Vec<(String, Value, Value)> {
    let c = &d.contract;
    let s = |v: &str| json!(v);
    let e = |name: &str| expected_value(expected, &[name], json!(""));
    let ec = |name: &str| expected_value(expected, &["contract", name], json!(""));
    let ef = |name: &str| expected_value(expected, &[name], json!(false));
    let mut out: Vec<(String, Value, Value)> = vec![
        ("contract.conId".into(), ec("conId"), json!(c.con_id)),
        ("contract.symbol".into(), ec("symbol"), s(&c.symbol)),
        ("contract.secType".into(), ec("secType"), s(&c.sec_type)),
        ("contract.lastTradeDateOrContractMonth".into(), ec("lastTradeDateOrContractMonth"), s(&c.last_trade_date_or_contract_month)),
        ("contract.lastTradeDate".into(), ec("lastTradeDate"), s(&c.last_trade_date)),
        ("contract.strike".into(), expected_value(expected, &["contract", "strike"], json!(0.0)), num(c.strike)),
        ("contract.right".into(), ec("right"), s(&c.right)),
        ("contract.multiplier".into(), ec("multiplier"), s(&c.multiplier)),
        ("contract.exchange".into(), ec("exchange"), s(&c.exchange)),
        ("contract.primaryExchange".into(), ec("primaryExchange"), s(&c.primary_exchange)),
        ("contract.currency".into(), ec("currency"), s(&c.currency)),
        ("contract.localSymbol".into(), ec("localSymbol"), s(&c.local_symbol)),
        ("contract.tradingClass".into(), ec("tradingClass"), s(&c.trading_class)),
        ("marketName".into(), e("marketName"), s(&d.market_name)),
        ("minTick".into(), expected_value(expected, &["minTick"], json!(0.0)), num(d.min_tick)),
        ("orderTypes".into(), e("orderTypes"), s(&d.order_types)),
        ("validExchanges".into(), e("validExchanges"), s(&d.valid_exchanges)),
        ("priceMagnifier".into(), expected_value(expected, &["priceMagnifier"], json!(0)), json!(d.price_magnifier)),
        ("underConId".into(), expected_value(expected, &["underConId"], json!(0)), json!(d.under_con_id)),
        ("longName".into(), e("longName"), s(&d.long_name)),
        ("contractMonth".into(), e("contractMonth"), s(&d.contract_month)),
        ("industry".into(), e("industry"), s(&d.industry)),
        ("category".into(), e("category"), s(&d.category)),
        ("subcategory".into(), e("subcategory"), s(&d.subcategory)),
        ("timeZoneId".into(), e("timeZoneId"), s(&d.time_zone_id)),
        ("tradingHours".into(), e("tradingHours"), s(&d.trading_hours)),
        ("liquidHours".into(), e("liquidHours"), s(&d.liquid_hours)),
        ("evRule".into(), e("evRule"), s(&d.ev_rule)),
        ("evMultiplier".into(), expected_value(expected, &["evMultiplier"], json!(0)), json!(d.ev_multiplier as i64)),
        ("aggGroup".into(), expected_value(expected, &["aggGroup"], json!(0)), json!(d.agg_group)),
        ("underSymbol".into(), e("underSymbol"), s(&d.under_symbol)),
        ("underSecType".into(), e("underSecType"), s(&d.under_sec_type)),
        ("marketRuleIds".into(), e("marketRuleIds"), s(&d.market_rule_ids)),
        ("secIdList".into(), expected_value(expected, &["secIdList"], json!([])),
            Value::Array(d.sec_id_list.iter().map(|t| json!({"tag": t.tag, "value": t.value})).collect())),
        ("realExpirationDate".into(), e("realExpirationDate"), s(&d.real_expiration_date)),
        ("lastTradeTime".into(), e("lastTradeTime"), s(&d.last_trade_time)),
        ("stockType".into(), e("stockType"), s(&d.stock_type)),
        ("minSize".into(), expected_decimal(expected, "minSize"), decimal(d.min_size)),
        ("sizeIncrement".into(), expected_decimal(expected, "sizeIncrement"), decimal(d.size_increment)),
        ("suggestedSizeIncrement".into(), expected_decimal(expected, "suggestedSizeIncrement"), decimal(d.suggested_size_increment)),
        ("cusip".into(), e("cusip"), s(&d.cusip)),
        ("ratings".into(), e("ratings"), s(&d.ratings)),
        ("descAppend".into(), e("descAppend"), s(&d.desc_append)),
        ("bondType".into(), e("bondType"), s(&d.bond_type)),
        ("couponType".into(), e("couponType"), s(&d.coupon_type)),
        ("callable".into(), ef("callable"), json!(d.callable)),
        ("putable".into(), ef("putable"), json!(d.putable)),
        ("coupon".into(), expected_value(expected, &["coupon"], json!(0)), num(d.coupon)),
        ("convertible".into(), ef("convertible"), json!(d.convertible)),
        ("maturity".into(), e("maturity"), s(&d.maturity)),
        ("issueDate".into(), e("issueDate"), s(&d.issue_date)),
        ("nextOptionDate".into(), e("nextOptionDate"), s(&d.next_option_date)),
        ("nextOptionType".into(), e("nextOptionType"), s(&d.next_option_type)),
        ("nextOptionPartial".into(), ef("nextOptionPartial"), json!(d.next_option_partial)),
        ("notes".into(), e("notes"), s(&d.notes)),
        ("fundName".into(), e("fundName"), s(&d.fund_name)),
        ("fundFamily".into(), e("fundFamily"), s(&d.fund_family)),
        ("fundType".into(), e("fundType"), s(&d.fund_type)),
        ("fundClosed".into(), ef("fundClosed"), json!(d.fund_closed)),
        ("fundClosedForNewInvestors".into(), ef("fundClosedForNewInvestors"), json!(d.fund_closed_for_new_investors)),
        ("fundClosedForNewMoney".into(), ef("fundClosedForNewMoney"), json!(d.fund_closed_for_new_money)),
        ("ineligibilityReasonList".into(), expected_value(expected, &["ineligibilityReasonList"], json!([])),
            Value::Array(d.ineligibility_reason_list.iter().map(|r| json!({"id_": r.id, "description": r.description})).collect())),
    ];
    // The client's enum of no policy and no asset type is `None`.
    let none = |v: &str| if v.is_empty() { json!("None") } else { json!(v) };
    out.push(("fundDistributionPolicyIndicator".into(), expected_value(expected, &["fundDistributionPolicyIndicator"], json!("None")), none(&d.fund_distribution_policy_indicator)));
    out.push(("fundAssetType".into(), expected_value(expected, &["fundAssetType"], json!("None")), none(&d.fund_asset_type)));
    out
}

/// Numbers compare as numbers (the client prints 0 and 0.0 alike).
fn same(a: &Value, b: &Value) -> bool {
    match (a.as_f64(), b.as_f64()) {
        (Some(x), Some(y)) => (x - y).abs() < 1e-12,
        _ => a == b,
    }
}

/// Fields the gateway filled from a state the captures do not hold, per
/// row: the market rules of exchanges answered before the capture window,
/// a schedule it had cached before the window.
fn known_gaps(row: &Value, field: &str) -> bool {
    let flag = |name: &str| row[name].as_bool().unwrap_or(false);
    (field == "marketRuleIds" && flag("rules_incomplete"))
        || (flag("schedule_cached")
            && matches!(field, "timeZoneId" | "tradingHours" | "liquidHours" | "lastTradeTime"))
}

#[test]
fn rows_match_the_captured_gateway_rows_field_by_field() {
    let fx = fixture();
    let mut mismatches = Vec::new();
    let rows = fx["rows"].as_array().unwrap();
    for row in rows {
        let def = row_of(&fx, row);
        let details = ContractDetails::from_definition(&def);
        assert_eq!(def.is_bond(), row["callback"] == "bondContractDetails", "callback of {}", row["req_id"]);
        for (name, want, got) in compare(&row["expected"], &details) {
            if !same(&want, &got) && !known_gaps(row, &name) {
                mismatches.push(format!("{} req {} conId {}: {}: gateway {} ibx {}",
                    row["scenario"], row["req_id"], row["expected"]["contract"]["conId"], name, want, got));
            }
        }
    }
    assert!(rows.len() >= 140, "{} rows", rows.len());
    assert!(mismatches.is_empty(), "{} mismatches:\n{}", mismatches.len(), mismatches.join("\n"));
}
