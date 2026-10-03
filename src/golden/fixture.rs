//! The codec fixtures (tests/fixtures/gw1040/codec/, format `codec/1`): a
//! slice of a recorded reference scenario, the frames as recorded and the
//! API side as the official client library read it.

use base64::Engine as _;
use serde_json::Value;

use crate::test_support::{parse_fields, Fields};

/// One recorded message.
#[derive(Debug, Clone)]
pub(crate) struct Rec {
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

/// A fixture: its header and its records in recorded order.
pub(crate) struct Fixture {
    pub header: Value,
    pub recs: Vec<Rec>,
}

pub(crate) fn load(name: &str) -> Fixture {
    let path = format!("{}/tests/fixtures/gw1040/codec/{name}.jsonl", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let mut lines = text.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["format"], "codec/1", "{path}");
    let recs = lines.map(|l| {
        let v: Value = serde_json::from_str(l).unwrap();
        let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
        Rec {
            seq: v["seq"].as_u64().unwrap(),
            leg: s("leg"),
            conn: s("conn"),
            msg: if v["msg_type"].is_string() { s("msg_type") } else { s("msg_name") },
            raw: v["raw_b64"].as_str().map(|b| base64::engine::general_purpose::STANDARD.decode(b).unwrap()).unwrap_or_default(),
            request: v["request"].clone(),
            callbacks: v["callbacks"].clone(),
        }
    }).collect();
    Fixture { header, recs }
}

/// A number of the client library as text: `"80"` (a decimal) or `80.0`.
pub(crate) fn num(v: &Value) -> f64 {
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
pub(crate) fn n(v: f64) -> String {
    if v == f64::MAX { "MAX".into() } else { format!("{v}") }
}

/// The attribute mask of a price tick: 1 can auto execute, 2 past limit,
/// 4 pre-open (as the API message's `attrMask`).
pub(crate) fn attr_mask(auto: bool, past: bool, pre: bool) -> u8 {
    auto as u8 | (past as u8) << 1 | (pre as u8) << 2
}

/// The callbacks of a recorded API message, each as one line of text in the
/// form [`super::session::Recorder`] gives for ibx's callbacks. Callbacks
/// the comparison does not cover give `None`.
pub(crate) fn canonical(cb: &Value) -> Option<String> {
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
            i(1), s(2), n(num(&a[3])), n(num(&a[4])), n(num(&a[5])), i(6), i(7), n(num(&a[8])), i(9), s(10), n(num(&a[11])),
        ),
        "openOrder" => open_order_line(i(1), &a[2], &a[3], &a[4]),
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

/// The fields of an openOrder the comparison covers.
pub(crate) const OPEN_ORDER_FIELDS: &[&str] = &[
    "action", "totalQuantity", "orderType", "lmtPrice", "auxPrice", "tif", "ocaGroup", "orderRef",
    "parentId", "outsideRth", "goodAfterTime", "goodTillDate", "account", "trailingPercent", "trailStopPrice",
    "whatIf", "permId", "clientId",
];

/// openOrder as one line: the order id, the contract's conId, symbol and
/// type, the order fields of [`OPEN_ORDER_FIELDS`] and the status. A field
/// the recorded object does not hold has its default.
pub(crate) fn open_order_line(id: i64, contract: &Value, order: &Value, state: &Value) -> String {
    let field = |k: &str| -> String {
        let v = &order[k];
        match k {
            "action" | "orderType" | "tif" | "ocaGroup" | "orderRef" | "goodAfterTime" | "goodTillDate" | "account" =>
                v.as_str().unwrap_or("").to_string(),
            "outsideRth" | "whatIf" => v.as_bool().unwrap_or(false).to_string(),
            "lmtPrice" | "auxPrice" | "trailingPercent" | "trailStopPrice" =>
                if v.is_null() { "MAX".into() } else { n(num(v)) },
            _ => if v.is_null() { "0".into() } else { n(num(v)) },
        }
    };
    let fields: Vec<String> = OPEN_ORDER_FIELDS.iter().map(|k| format!("{k}={}", field(k))).collect();
    format!(
        "openOrder|{id}|{}|{}|{}|{}|{}",
        contract["conId"].as_i64().unwrap_or(0), contract["symbol"].as_str().unwrap_or(""),
        contract["secType"].as_str().unwrap_or(""), fields.join(","), state["status"].as_str().unwrap_or(""),
    )
}

/// Rebuild a text frame (`8=FIX...`) with changed fields: the body length
/// and the checksum computed again; the other fields kept in their order.
pub(crate) fn rebuild_text(fields: &[(u32, String)]) -> Vec<u8> {
    let begin = fields.iter().find(|(t, _)| *t == 8).map_or("FIX.4.1", |(_, v)| v.as_str());
    let mut body = Vec::new();
    for (t, v) in fields.iter().filter(|(t, _)| !matches!(t, 8 | 9 | 10)) {
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
pub(crate) fn rebuild_binary(raw: &[u8], new_body: &str) -> Vec<u8> {
    let text = String::from_utf8_lossy(raw);
    let msg_type = text.split("\x0135=").nth(1).unwrap().split('\x01').next().unwrap();
    let sig = text.split("\x018349=").nth(1).map(|s| s.trim_end_matches('\x01')).unwrap_or("00000000");
    let body = format!("35={msg_type}\x01{new_body}\x018349={sig}\x01");
    format!("8=O\x019={:04}\x01{body}", body.len()).into_bytes()
}

/// The text body of a farm frame (the part after `35=X`).
pub(crate) fn binary_body(raw: &[u8]) -> String {
    let text = String::from_utf8_lossy(raw);
    let after = text.split_once("\x0135=").unwrap().1;
    let after = after.split_once('\x01').unwrap().1;
    after.split("\x018349=").next().unwrap().trim_end_matches('\x01').to_string()
}
