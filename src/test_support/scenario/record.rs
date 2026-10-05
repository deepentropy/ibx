//! The recorded scenarios: the four-leg files (tests/fixtures/gw1040/
//! scenarios/, format `four-leg/1`, with their decoded API side
//! `<name>.api.jsonl`) and the codec fixtures (tests/fixtures/gw1040/codec/,
//! format `codec/1`). Both give the same records: the frames as recorded
//! and the API side as the official client library read it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use serde_json::Value;

use crate::test_support::{parse_fields, Fields};

/// One recorded message.
#[derive(Debug, Clone)]
pub struct Rec {
    pub seq: u64,
    /// `api_out`, `api_in`, `fix_out` or `fix_in`.
    pub leg: String,
    /// `api:<port>`, `CCP`, `usfarm`, ...
    pub conn: String,
    /// The frame's message type, or the API message's name.
    pub msg: String,
    pub raw: Vec<u8>,
    /// `api_out`: the request's fields.
    pub request: Value,
    /// `api_in`: the wrapper calls of the client library.
    pub callbacks: Value,
}

impl Rec {
    pub fn fields(&self) -> Fields {
        parse_fields(&self.raw)
    }

    pub fn get(&self, tag: u32) -> Option<String> {
        self.fields().into_iter().find(|(t, _)| *t == tag).map(|(_, v)| v)
    }

    pub fn is(&self, leg: &str, conn: &str, msg: &str) -> bool {
        self.leg == leg && self.conn == conn && self.msg == msg
    }
}

/// A scenario: its header and its records in recorded order.
#[derive(Clone)]
pub struct Scenario {
    pub header: Value,
    pub recs: Vec<Rec>,
}

/// The fixture tree of the reference recordings.
pub fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/gw1040")
}

/// A codec fixture by name (tests/fixtures/gw1040/codec/<name>.jsonl).
pub fn load_codec(name: &str) -> Scenario {
    load_path(&fixtures_dir().join("codec").join(format!("{name}.jsonl")))
}

/// A four-leg scenario by its path under tests/fixtures/gw1040/scenarios/
/// (`20260926/lmt_cancel`), with its decoded API side.
pub fn load_scenario(rel: &str) -> Scenario {
    load_path(&fixtures_dir().join("scenarios").join(format!("{rel}.jsonl")))
}

/// A scenario file of either format. A four-leg file takes its API side
/// from `<name>.api.jsonl` (scripts/codec_fixtures.py --scenarios).
pub fn load_path(path: &Path) -> Scenario {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let mut lines = text.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    let four_leg = match header["format"].as_str() {
        Some("codec/1") => false,
        Some("four-leg/1") => true,
        other => panic!("{}: format {other:?}", path.display()),
    };
    let mut header = header;
    let api: HashMap<u64, Value> = if four_leg {
        let side = path.with_file_name(format!("{}.api.jsonl", path.file_stem().unwrap().to_string_lossy()));
        let text = std::fs::read_to_string(&side).unwrap_or_else(|e| {
            panic!("{}: {e} (made by scripts/codec_fixtures.py --scenarios)", side.display())
        });
        // The sidecar's header gives the recording machine's zone.
        if let Some(side_header) = text.lines().next().and_then(|l| serde_json::from_str::<Value>(l).ok())
            && header["machine_zone"].is_null()
        {
            header["machine_zone"] = side_header["machine_zone"].clone();
        }
        text.lines().skip(1).map(|l| {
            let v: Value = serde_json::from_str(l).unwrap();
            (v["seq"].as_u64().unwrap(), v)
        }).collect()
    } else {
        HashMap::new()
    };
    let recs = lines.map(|l| {
        let v: Value = serde_json::from_str(l).unwrap();
        let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
        let seq = v["seq"].as_u64().unwrap();
        let decoded = api.get(&seq).unwrap_or(&v);
        Rec {
            seq,
            leg: s("leg"),
            conn: s("conn"),
            msg: if v["msg_type"].is_string() { s("msg_type") } else { s("msg_name") },
            raw: v["raw_b64"].as_str().map(|b| base64::engine::general_purpose::STANDARD.decode(b).unwrap()).unwrap_or_default(),
            request: decoded["request"].clone(),
            callbacks: decoded["callbacks"].clone(),
        }
    }).collect();
    Scenario { header, recs }
}

/// A number of the client library as text: `"80"` (a decimal) or `80.0`.
pub fn num(v: &Value) -> f64 {
    match v {
        Value::String(s) if s == "MAX" => f64::MAX,
        Value::String(s) => s.parse().unwrap_or_else(|_| panic!("number {s}")),
        Value::Number(n) => n.as_f64().unwrap(),
        Value::Bool(b) => *b as u8 as f64,
        Value::Null => 0.0,
        other => panic!("number {other}"),
    }
}

/// A number as the callbacks are compared: shortest decimal text, so 80,
/// 80.0 and "80" are the same.
pub fn n(v: f64) -> String {
    if v == f64::MAX { "MAX".into() } else { format!("{v}") }
}

/// The attribute mask of a price tick: 1 can auto execute, 2 past limit,
/// 4 pre-open (as the API message's `attrMask`).
pub fn attr_mask(auto: bool, past: bool, pre: bool) -> u8 {
    auto as u8 | (past as u8) << 1 | (pre as u8) << 2
}

/// The callbacks of a recorded API message, each as one line of text in the
/// form [`super::Recorder`] gives for ibx's callbacks. Callbacks the
/// comparison does not cover give `None`.
pub fn canonical(cb: &Value) -> Option<String> {
    let a = cb.as_array().unwrap();
    let name = a[0].as_str().unwrap();
    let s = |i: usize| a[i].as_str().unwrap_or("").to_string();
    let i = |i: usize| a[i].as_i64().unwrap();
    Some(match name {
        "tickPrice" => {
            let at = &a[4];
            let flag = |k: &str| at[k].as_bool().unwrap_or(false);
            format!("tickPrice|{}|{}|{}|{}", i(1), i(2), n(num(&a[3])), attr_mask(flag("canAutoExecute"), flag("pastLimit"), flag("preOpen")))
        }
        "tickSize" => format!("tickSize|{}|{}|{}", i(1), i(2), n(num(&a[3]))),
        "tickString" => format!("tickString|{}|{}|{}", i(1), i(2), s(3)),
        "tickGeneric" => format!("tickGeneric|{}|{}|{}", i(1), i(2), n(num(&a[3]))),
        "marketDataType" => format!("marketDataType|{}|{}", i(1), i(2)),
        "tickReqParams" => format!("tickReqParams|{}|{}|{}|{}", i(1), n(num(&a[2])), s(3), i(4)),
        "tickSnapshotEnd" => format!("tickSnapshotEnd|{}", i(1)),
        "error" => format!("error|{}|{}|{}", i(1), i(3), s(4)),
        "orderStatus" => format!(
            "orderStatus|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
            i(1), s(2), n(num(&a[3])), n(num(&a[4])), n(num(&a[5])), perm(i(6)), i(7), n(num(&a[8])), i(9), s(10), n(num(&a[11])),
        ),
        "openOrder" => open_order_line(i(1), &a[2], &a[3], &a[4]),
        "openOrderEnd" => "openOrderEnd".into(),
        "execDetails" => {
            let (c, e) = (&a[2], &a[3]);
            let es = |k: &str| e[k].as_str().unwrap_or("").to_string();
            format!(
                "execDetails|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
                i(1), c["conId"].as_i64().unwrap_or(0), c["symbol"].as_str().unwrap_or(""), es("exchange"), es("side"),
                n(num(&e["shares"])), n(num(&e["price"])), n(num(&e["cumQty"])), n(num(&e["avgPrice"])),
                e["orderId"].as_i64().unwrap_or(0), es("orderRef"), e["lastLiquidity"].as_i64().unwrap_or(0),
            )
        }
        "execDetailsEnd" => format!("execDetailsEnd|{}", i(1)),
        "commissionAndFeesReport" => {
            let r = &a[1];
            format!("commissionAndFeesReport|{}|{}", n(num(&r["commissionAndFees"])), r["currency"].as_str().unwrap_or(""))
        }
        "position" => {
            let c = &a[2];
            format!("position|{}|{}|{}|{}|{}", s(1), c["conId"].as_i64().unwrap_or(0), c["symbol"].as_str().unwrap_or(""), n(num(&a[3])), n(num(&a[4])))
        }
        "positionEnd" => "positionEnd".into(),
        "updateAccountValue" => format!("updateAccountValue|{}|{}|{}|{}", s(1), s(2), s(3), s(4)),
        "updatePortfolio" => {
            let c = &a[1];
            format!(
                "updatePortfolio|{}|{}|{}|{}|{}|{}|{}|{}|{}",
                c["conId"].as_i64().unwrap_or(0), c["symbol"].as_str().unwrap_or(""), n(num(&a[2])), n(num(&a[3])),
                n(num(&a[4])), n(num(&a[5])), n(num(&a[6])), n(num(&a[7])), s(8),
            )
        }
        "updateAccountTime" => format!("updateAccountTime|{}", s(1)),
        "accountDownloadEnd" => format!("accountDownloadEnd|{}", s(1)),
        "pnl" => format!("pnl|{}|{}|{}|{}", i(1), n(num(&a[2])), n(num(&a[3])), n(num(&a[4]))),
        "pnlSingle" => format!("pnlSingle|{}|{}|{}|{}|{}|{}", i(1), n(num(&a[2])), n(num(&a[3])), n(num(&a[4])), n(num(&a[5])), n(num(&a[6]))),
        "scannerData" => {
            let c = &a[3]["contract"];
            format!("scannerData|{}|{}|{}|{}", i(1), i(2), c["conId"].as_i64().unwrap_or(0), c["symbol"].as_str().unwrap_or(""))
        }
        "scannerDataEnd" => format!("scannerDataEnd|{}", i(1)),
        "historicalData" | "historicalDataUpdate" => {
            let b = &a[2];
            let d = |k: &str| n(num(&b[k]));
            format!(
                "{name}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
                i(1), b["date"].as_str().unwrap_or(""), d("open"), d("high"), d("low"), d("close"),
                num(&b["volume"]) as i64, d("wap"), b["barCount"].as_i64().unwrap_or(0),
            )
        }
        "historicalDataEnd" => format!("historicalDataEnd|{}|{}|{}", i(1), s(2), s(3)),
        // The attributes as a mask: 2 past limit (bid past low), 4
        // unreported (ask past high).
        "tickByTickAllLast" => {
            let at = &a[6];
            let flag = |k: &str| at[k].as_bool().unwrap_or(false);
            format!(
                "tickByTickAllLast|{}|{}|{}|{}|{}|{}|{}|{}",
                i(1), i(2), i(3), n(num(&a[4])), n(num(&a[5])), attr_mask(false, flag("pastLimit"), flag("unreported")), s(7), s(8),
            )
        }
        "tickByTickBidAsk" => {
            let at = &a[7];
            let flag = |k: &str| at[k].as_bool().unwrap_or(false);
            format!(
                "tickByTickBidAsk|{}|{}|{}|{}|{}|{}|{}",
                i(1), i(2), n(num(&a[3])), n(num(&a[4])), n(num(&a[5])), n(num(&a[6])),
                attr_mask(false, flag("bidPastLow"), flag("askPastHigh")),
            )
        }
        "tickByTickMidPoint" => format!("tickByTickMidPoint|{}|{}|{}", i(1), i(2), n(num(&a[3]))),
        "historicalTicksLast" => {
            let rows: Vec<String> = a[2].as_array().into_iter().flatten().map(|t| {
                let at = &t["tickAttribLast"];
                let flag = |k: &str| at[k].as_bool().unwrap_or(false);
                format!(
                    "{}:{}:{}:{}:{}:{}", t["time"].as_i64().unwrap_or(0), n(num(&t["price"])), n(num(&t["size"])),
                    attr_mask(false, flag("pastLimit"), flag("unreported")), t["exchange"].as_str().unwrap_or(""),
                    t["specialConditions"].as_str().unwrap_or(""),
                )
            }).collect();
            format!("historicalTicksLast|{}|{}|{}", i(1), rows.join(","), a[3].as_bool().unwrap_or(false))
        }
        "realtimeBar" => format!(
            "realtimeBar|{}|{}|{}|{}|{}|{}|{}|{}|{}",
            i(1), i(2), n(num(&a[3])), n(num(&a[4])), n(num(&a[5])), n(num(&a[6])), n(num(&a[7])), n(num(&a[8])), i(9),
        ),
        "headTimestamp" => format!("headTimestamp|{}|{}", i(1), s(2)),
        "accountSummary" => format!("accountSummary|{}|{}|{}|{}|{}", i(1), s(2), s(3), s(4), s(5)),
        "accountSummaryEnd" => format!("accountSummaryEnd|{}", i(1)),
        "smartComponents" => {
            let mut rows: Vec<(i64, String)> = a[2].as_object().unwrap().iter()
                .map(|(bit, v)| (bit.parse().unwrap(), format!("{bit}:{}:{}", v[0].as_str().unwrap(), v[1].as_str().unwrap())))
                .collect();
            rows.sort();
            format!("smartComponents|{}|{}", i(1), rows.into_iter().map(|(_, r)| r).collect::<Vec<_>>().join(","))
        }
        _ => return None,
    })
}

/// A permId as compared: the reference's is the integer part of its
/// ClOrdID, ibx's its own; both are session values. Set or not.
pub fn perm(v: i64) -> &'static str {
    if v == 0 { "0" } else { "{perm}" }
}

/// The fields of an openOrder the comparison covers. trailStopPrice only
/// for a plain TRAIL order, whose stop is the one the server reports
/// (6117, captured 05/10/2026); the reference shows one for other orders
/// too that is not read yet (a LMT at 272.86 shows 273.86, ibx#491): see
/// [`compared_field`].
pub const OPEN_ORDER_FIELDS: &[&str] = &[
    "action", "totalQuantity", "orderType", "lmtPrice", "auxPrice", "tif", "ocaGroup", "orderRef",
    "parentId", "outsideRth", "goodAfterTime", "goodTillDate", "account", "trailingPercent",
    "trailStopPrice", "whatIf", "permId", "clientId",
];

/// Whether a field of [`OPEN_ORDER_FIELDS`] is compared for an order of
/// this type.
pub fn compared_field(field: &str, order_type: &str) -> bool {
    field != "trailStopPrice" || order_type == "TRAIL"
}

/// A price field of an openOrder: unset (the client library's MAX, ibx's
/// 0 for trailingPercent) as one value.
pub fn order_price(v: f64) -> String {
    if v == f64::MAX || v == 0.0 { "-".into() } else { n(v) }
}

/// openOrder as one line: the order id, the contract's conId, symbol and
/// type, the order fields of [`OPEN_ORDER_FIELDS`] and the status. A field
/// the recorded object does not hold has its default.
pub fn open_order_line(id: i64, contract: &Value, order: &Value, state: &Value) -> String {
    let field = |k: &str| -> String {
        let v = &order[k];
        match k {
            "action" | "orderType" | "tif" | "ocaGroup" | "orderRef" | "goodAfterTime" | "goodTillDate" | "account" =>
                v.as_str().unwrap_or("").to_string(),
            "outsideRth" | "whatIf" => v.as_bool().unwrap_or(false).to_string(),
            "lmtPrice" | "auxPrice" | "trailingPercent" | "trailStopPrice" =>
                if v.is_null() { "-".into() } else { order_price(num(v)) },
            "permId" => perm(if v.is_null() { 0 } else { num(v) as i64 }).into(),
            _ => if v.is_null() { "0".into() } else { n(num(v)) },
        }
    };
    let order_type = order["orderType"].as_str().unwrap_or("");
    let fields: Vec<String> = OPEN_ORDER_FIELDS.iter().filter(|k| compared_field(k, order_type))
        .map(|k| format!("{k}={}", field(k))).collect();
    format!(
        "openOrder|{id}|{}|{}|{}|{}|{}",
        contract["conId"].as_i64().unwrap_or(0), contract["symbol"].as_str().unwrap_or(""),
        contract["secType"].as_str().unwrap_or(""), fields.join(","), state["status"].as_str().unwrap_or(""),
    )
}

/// Rebuild a text frame (`8=FIX...`) with changed fields: the body length
/// and the checksum computed again; the other fields kept in their order.
pub fn rebuild_text(fields: &[(u32, String)]) -> Vec<u8> {
    let begin = fields.iter().find(|(t, _)| *t == 8).map_or("FIX.4.1", |(_, v)| v.as_str());
    let mut body = Vec::new();
    for (t, v) in fields.iter().filter(|(t, _)| !matches!(t, 8..=10)) {
        body.extend_from_slice(format!("{t}={v}\x01").as_bytes());
    }
    let mut msg = format!("8={begin}\x019={:04}\x01", body.len()).into_bytes();
    msg.extend_from_slice(&body);
    let sum: u32 = msg.iter().map(|&b| b as u32).sum();
    msg.extend_from_slice(format!("10={:03}\x01", sum % 256).as_bytes());
    msg
}

/// Rebuild a farm frame with a text body (`8=O|9=|35=Q|<body>|8349=..`)
/// with another body; the length counts the signature, as on the wire.
pub fn rebuild_binary(raw: &[u8], new_body: &str) -> Vec<u8> {
    let text = String::from_utf8_lossy(raw);
    let msg_type = text.split("\x0135=").nth(1).unwrap().split('\x01').next().unwrap();
    let sig = text.split("\x018349=").nth(1).map(|s| s.trim_end_matches('\x01')).unwrap_or("00000000");
    let body = format!("35={msg_type}\x01{new_body}\x018349={sig}\x01");
    format!("8=O\x019={:04}\x01{body}", body.len()).into_bytes()
}

/// The text body of a farm frame (the part after `35=X`).
pub fn binary_body(raw: &[u8]) -> String {
    let text = String::from_utf8_lossy(raw);
    let after = text.split_once("\x0135=").unwrap().1;
    let after = after.split_once('\x01').unwrap().1;
    after.split("\x018349=").next().unwrap().trim_end_matches('\x01').to_string()
}
