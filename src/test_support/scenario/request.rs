//! The recorded API requests made again on the Rust API client: the
//! request's fields as the official client library read them back
//! (`request` of a record), turned into the client's arguments.

use serde_json::Value;

use crate::api::client::EClient;
use crate::api::types::{ComboLeg, Contract, Order, TagValue};
use crate::types::OrderCondition;

use super::record::{num, Rec};
use super::session::Recorder;

/// A contract of a request: the client library's object or the request's
/// protobuf fields (the names differ for the primary exchange).
pub fn contract_of(v: &Value) -> Contract {
    let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
    Contract {
        con_id: v["conId"].as_i64().unwrap_or(0),
        symbol: s("symbol"),
        sec_type: s("secType"),
        exchange: s("exchange"),
        currency: s("currency"),
        primary_exchange: if v["primaryExchange"].is_string() { s("primaryExchange") } else { s("primaryExch") },
        last_trade_date_or_contract_month: s("lastTradeDateOrContractMonth"),
        strike: v["strike"].as_f64().unwrap_or(0.0),
        right: s("right"),
        multiplier: s("multiplier"),
        local_symbol: s("localSymbol"),
        trading_class: s("tradingClass"),
        ..Default::default()
    }
}

/// The contract of a placeOrder, with its combo legs.
pub fn order_contract(v: &Value) -> Contract {
    let mut c = contract_of(v);
    c.combo_legs = v["comboLegs"].as_array().into_iter().flatten().map(|l| ComboLeg {
        con_id: l["conId"].as_i64().unwrap_or(0),
        ratio: l["ratio"].as_i64().unwrap_or(0) as i32,
        action: l["action"].as_str().unwrap_or("").to_string(),
        exchange: l["exchange"].as_str().unwrap_or("").to_string(),
        ..Default::default()
    }).collect();
    c
}

/// An API order as the client library read it back from the request.
/// A field the recorded object does not hold keeps the client library's
/// default (ibx's defaults are the same). An unknown field fails, so a new
/// fixture never drops a field silently.
pub fn order_of(v: &Value) -> Order {
    let mut o = Order::default();
    let s = |x: &Value| x.as_str().unwrap_or("").to_string();
    let tags = |x: &Value| -> Vec<TagValue> {
        x.as_array().into_iter().flatten().map(|t| TagValue { tag: s(&t["tag"]), value: s(&t["value"]) }).collect()
    };
    for (k, x) in v.as_object().unwrap() {
        match k.as_str() {
            "softDollarTier" | "orderId" => {}
            "action" => o.action = s(x),
            "totalQuantity" => o.total_quantity = num(x),
            "orderType" => o.order_type = s(x),
            "lmtPrice" => o.lmt_price = num(x),
            "auxPrice" => o.aux_price = num(x),
            "tif" => o.tif = s(x),
            "orderRef" => o.order_ref = s(x),
            "outsideRth" => o.outside_rth = x.as_bool().unwrap(),
            "conditions" => o.conditions = x.as_array().unwrap().iter().map(condition_of).collect(),
            "conditionsIgnoreRth" => o.conditions_ignore_rth = x.as_bool().unwrap(),
            "conditionsCancelOrder" => o.conditions_cancel_order = x.as_bool().unwrap(),
            "algoStrategy" => o.algo_strategy = s(x),
            "algoParams" => o.algo_params = tags(x),
            "whatIf" => o.what_if = x.as_bool().unwrap(),
            "trailingPercent" => o.trailing_percent = num(x),
            "trailStopPrice" => o.trail_stop_price = num(x),
            "parentId" => o.parent_id = x.as_i64().unwrap(),
            "transmit" => o.transmit = x.as_bool().unwrap(),
            "includeOvernight" => o.include_overnight = x.as_bool().unwrap(),
            "orderComboLegs" => o.order_combo_legs = x.as_array().unwrap().iter()
                .map(|l| if l["price"].is_null() { f64::MAX } else { num(&l["price"]) }).collect(),
            "smartComboRoutingParams" => o.smart_combo_routing_params = tags(x),
            "ocaGroup" => o.oca_group = s(x),
            "ocaType" => o.oca_type = x.as_i64().unwrap() as i32,
            "startingPrice" => o.starting_price = num(x),
            "stockRefPrice" => o.stock_ref_price = num(x),
            "referenceContractId" => o.reference_contract_id = x.as_i64().unwrap() as i32,
            "peggedChangeAmount" => o.pegged_change_amount = num(x),
            "referenceChangeAmount" => o.reference_change_amount = num(x),
            "customerAccount" => o.customer_account = s(x),
            "professionalCustomer" => o.professional_customer = x.as_bool().unwrap(),
            "goodAfterTime" => o.good_after_time = s(x),
            "goodTillDate" => o.good_till_date = s(x),
            "displaySize" => o.display_size = x.as_i64().unwrap() as i32,
            "hidden" => o.hidden = x.as_bool().unwrap(),
            "account" => o.account = s(x),
            "cashQty" => o.cash_qty = num(x),
            "allOrNone" => o.all_or_none = x.as_bool().unwrap(),
            "triggerMethod" => o.trigger_method = x.as_i64().unwrap() as i32,
            "lmtPriceOffset" => o.lmt_price_offset = num(x),
            other => panic!("order field {other} = {x} is not read by the replay"),
        }
    }
    o
}

fn condition_of(c: &Value) -> OrderCondition {
    let is_more = c["isMore"].as_bool().unwrap_or(false);
    match c["_type"].as_str().unwrap() {
        "TimeCondition" => OrderCondition::Time { time: c["time"].as_str().unwrap_or("").to_string(), is_more },
        other => panic!("condition {other} is not read by the replay"),
    }
}

/// A scanner subscription of a request (protobuf field names).
fn scanner_of(v: &Value) -> crate::api::types::ScannerSubscription {
    let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
    let mut sub = crate::api::types::ScannerSubscription {
        instrument: s("instrument"),
        location_code: s("locationCode"),
        scan_code: s("scanCode"),
        ..Default::default()
    };
    if let Some(rows) = v["numberOfRows"].as_i64() {
        sub.number_of_rows = rows as i32;
    }
    sub
}

/// Make a recorded request on the Rust client. The callbacks a request
/// gives at once (open orders, positions) go to `rec`. False when the
/// request is not one the replay makes.
pub fn make(c: &EClient, r: &Rec, rec: &mut Recorder) -> bool {
    let q = &r.request;
    let id = q["reqId"].as_i64().unwrap_or(0);
    let st = |k: &str| q[k].as_str().unwrap_or("").to_string();
    match r.msg.as_str() {
        "PLACE_ORDER" => {
            let (contract, order) = (order_contract(&q["contract"]), order_of(&q["order"]));
            let _ = c.place_order(q["orderId"].as_i64().unwrap(), &contract, &order);
        }
        "CANCEL_ORDER" => { let _ = c.cancel_order(q["orderId"].as_i64().unwrap(), ""); }
        "REQ_GLOBAL_CANCEL" => { let _ = c.req_global_cancel(); }
        "REQ_OPEN_ORDERS" => c.req_open_orders(rec),
        "REQ_ALL_OPEN_ORDERS" => c.req_all_open_orders(rec),
        "REQ_POSITIONS" => c.req_positions(rec),
        "CANCEL_POSITIONS" => c.cancel_positions(),
        "REQ_MKT_DATA" => {
            let contract = contract_of(&q["contract"]);
            let _ = c.req_mkt_data(id, &contract, &st("genericTickList"), q["snapshot"].as_bool().unwrap_or(false),
                q["regulatorySnapshot"].as_bool().unwrap_or(false));
        }
        "CANCEL_MKT_DATA" => { let _ = c.cancel_mkt_data(id); }
        "REQ_MARKET_DATA_TYPE" => c.req_market_data_type(q["marketDataType"].as_i64().unwrap_or(1) as i32),
        "REQ_SMART_COMPONENTS" => c.req_smart_components(id, &st("bboExchange"), rec),
        "REQ_ACCOUNT_SUMMARY" => c.req_account_summary(id, &st("group"), &st("tags")),
        "CANCEL_ACCOUNT_SUMMARY" => c.cancel_account_summary(id),
        "REQ_ACCT_DATA" => c.req_account_updates(q["subscribe"].as_bool().unwrap_or(false), &st("acctCode")),
        "REQ_PNL" => c.req_pnl(id, &st("account"), &st("modelCode")),
        "CANCEL_PNL" => c.cancel_pnl(id),
        "REQ_PNL_SINGLE" => c.req_pnl_single(id, &st("account"), &st("modelCode"), q["conId"].as_i64().unwrap_or(0)),
        "CANCEL_PNL_SINGLE" => c.cancel_pnl_single(id),
        "REQ_SCANNER_SUBSCRIPTION" => {
            let sub = scanner_of(&q["scannerSubscription"]);
            let _ = c.req_scanner_subscription(id, &sub, &[], &[]);
        }
        "CANCEL_SCANNER_SUBSCRIPTION" => { let _ = c.cancel_scanner_subscription(id); }
        "REQ_HISTORICAL_DATA" => {
            let contract = contract_of(&q["contract"]);
            let _ = c.req_historical_data(id, &contract, &st("endDateTime"), &st("duration"), &st("barSizeSetting"),
                &st("whatToShow"), q["useRTH"].as_bool().unwrap_or(false), q["formatDate"].as_i64().unwrap_or(1) as i32,
                q["keepUpToDate"].as_bool().unwrap_or(false));
        }
        "CANCEL_HISTORICAL_DATA" => { let _ = c.cancel_historical_data(id); }
        "REQ_HEAD_TIMESTAMP" => {
            let contract = contract_of(&q["contract"]);
            let _ = c.req_head_time_stamp(id, &contract, &st("whatToShow"), q["useRTH"].as_bool().unwrap_or(false),
                q["formatDate"].as_i64().unwrap_or(1) as i32);
        }
        "CANCEL_HEAD_TIMESTAMP" => { let _ = c.cancel_head_time_stamp(id); }
        "REQ_HISTORICAL_TICKS" => {
            let contract = contract_of(&q["contract"]);
            let _ = c.req_historical_ticks(id, &contract, &st("startDateTime"), &st("endDateTime"),
                q["numberOfTicks"].as_i64().unwrap_or(0) as i32, &st("whatToShow"), q["useRTH"].as_bool().unwrap_or(false),
                q["ignoreSize"].as_bool().unwrap_or(false), &[]);
        }
        "REQ_MKT_DEPTH" => {
            let contract = contract_of(&q["contract"]);
            let _ = c.req_mkt_depth(id, &contract, q["numRows"].as_i64().unwrap_or(0) as i32, q["isSmartDepth"].as_bool().unwrap_or(false));
        }
        "CANCEL_MKT_DEPTH" => { let _ = c.cancel_mkt_depth(id); }
        "REQ_CONTRACT_DATA" => { let _ = c.req_contract_details(id, &contract_of(&q["contract"])); }
        "REQ_EXECUTIONS" => {
            let f = &q["executionFilter"];
            let fs = |k: &str| f[k].as_str().unwrap_or("").to_string();
            let filter = crate::api::types::ExecutionFilter {
                client_id: f["clientId"].as_i64().unwrap_or(0), acct_code: fs("acctCode"), time: fs("time"),
                symbol: fs("symbol"), sec_type: fs("secType"), exchange: fs("exchange"), side: fs("side"),
            };
            c.req_executions(id, &filter, rec);
        }
        _ => return false,
    }
    true
}
