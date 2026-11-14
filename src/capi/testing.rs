//! What the tests of the C++ library call (`test-support` only): values
//! made from the names of the fields, structures filled or checked on this
//! side, every callback called once, and the replay of a recorded scenario
//! with the C++ client as the API client (ibx#235).

use std::ffi::c_void;

use crate::api::EClient;
use crate::test_support::scenario::runner::{run, Driver, Options};
use crate::test_support::scenario::session::ACCOUNT;
use crate::test_support::scenario::{canonical, load_codec, load_scenario, Links, Rec};

use super::generated::{checks, CWrapper};
use super::wire::R;
use super::{IbxCallbacks, IbxClient, IbxStr, Scratch};

// ── Values made from a name: the same rule as the C++ tests ──

fn hash(name: &str) -> u32 {
    name.bytes().fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(u32::from(b)))
}

pub(crate) fn tv_i(name: &str, seed: i64) -> i64 {
    i64::from(hash(name) % 997) + seed
}

pub(crate) fn tv_f(name: &str, seed: i64) -> f64 {
    tv_i(name, seed) as f64 + 0.25
}

pub(crate) fn tv_b(name: &str, seed: i64) -> bool {
    (i64::from(hash(name)) + seed) % 2 == 1
}

pub(crate) fn tv_s(name: &str, seed: i64) -> String {
    format!("{name}{seed}")
}

/// Note the place of a value that is not as expected.
pub(crate) fn expect(out: &mut Vec<String>, at: String, ok: bool) {
    if !ok {
        out.push(at);
    }
}

/// Takes a text or an encoded structure from this side.
pub type IbxTestSink = unsafe extern "C" fn(user: *mut c_void, data: IbxStr);

/// A structure of the client, filled from `seed` and encoded. False when
/// the kind is unknown.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ibx_test_make(kind: IbxStr, seed: i64, sink: IbxTestSink, user: *mut c_void) -> bool {
    match checks::make(&unsafe { kind.text() }, seed) {
        Some(buf) => {
            unsafe { sink(user, IbxStr::of_bytes(&buf)) };
            true
        }
        None => false,
    }
}

/// Check a structure the caller filled from `seed`: the fields that are
/// not as expected, one per line. False when the kind is unknown.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ibx_test_check(kind: IbxStr, data: IbxStr, seed: i64, sink: IbxTestSink, user: *mut c_void) -> bool {
    match checks::check(&unsafe { kind.text() }, unsafe { data.bytes() }, seed) {
        Some(failures) => {
            unsafe { sink(user, IbxStr::of(&failures.join("\n"))) };
            true
        }
        None => false,
    }
}

/// The structure a request would get, as its debug text, with the reason
/// it would be refused for when there is one.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ibx_test_describe(kind: IbxStr, data: IbxStr, sink: IbxTestSink, user: *mut c_void) -> bool {
    use super::generated::*;
    let r = &mut R::new(unsafe { data.bytes() });
    let text = match &*unsafe { kind.text() } {
        "Contract" => format!("{:?}", r_contract(r)),
        "Order" => format!("{:?}", r_order(r)),
        "ScannerSubscription" => format!("{:?}", r_scanner_subscription(r)),
        "ExecutionFilter" => format!("{:?}", r_execution_filter(r)),
        "WshEventData" => format!("{:?}", r_wsh_event_data(r)),
        "Tags" => format!("{:?}", super::read_tags(unsafe { data.bytes() })),
        // The configuration requests cross as records.
        "ConfigRequest" => format!("{:?}", super::config::from_records::<crate::api::config::ConfigRequest>(&unsafe { data.text() })),
        "UpdateConfigRequest" => format!("{:?}", super::config::from_records::<crate::api::config::UpdateConfigRequest>(&unsafe { data.text() })),
        _ => return false,
    };
    let text = match r.refused {
        Some((code, why)) => format!("{text}\nrefused {code} {why}"),
        None => text,
    };
    unsafe { sink(user, IbxStr::of(&text)) };
    true
}

/// A configuration request with every field set to the value made from its
/// name and `seed`, as its debug text: what `ibx_test_describe` gives for
/// the records of the same request made on the other side.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ibx_test_config(kind: IbxStr, seed: i64, sink: IbxTestSink, user: *mut c_void) -> bool {
    use super::config::Records;
    let text = match &*unsafe { kind.text() } {
        "ConfigRequest" => format!("{:?}", crate::api::config::ConfigRequest::filled(seed)),
        "UpdateConfigRequest" => format!("{:?}", crate::api::config::UpdateConfigRequest::filled(seed)),
        _ => return false,
    };
    unsafe { sink(user, IbxStr::of(&text)) };
    true
}

/// A session with no server behind it, for the requests the client
/// answers itself. The caller releases it with `ibx_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ibx_test_session() -> *mut IbxClient {
    let (tx, rx) = crossbeam_channel::unbounded();
    // Nothing reads the commands: the channel stays open all the same.
    std::mem::forget(rx);
    let shared = std::sync::Arc::new(crate::bridge::SharedState::new());
    let client = EClient::from_parts(shared, tx, std::thread::spawn(|| {}), ACCOUNT.into());
    Box::into_raw(Box::new(IbxClient::new(client, None)))
}

/// A structure with the fields that have a rule of their own set to known
/// values, encoded: the C++ tests check them one by one.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ibx_test_sample(kind: IbxStr, sink: IbxTestSink, user: *mut c_void) -> bool {
    use super::generated::*;
    use super::wire::W;
    use crate::api::types::*;
    use crate::types::OrderCondition;
    let mut buf = Vec::new();
    let w = &mut W(&mut buf);
    match &*unsafe { kind.text() } {
        "Order" => w_order(w, &Order {
            total_quantity: 12.5,
            origin: 1,
            use_price_mgmt_algo: 1,
            route_marketable_to_bbo: true,
            seek_price_improvement: false,
            soft_dollar_tier_name: "tier".into(),
            soft_dollar_tier_val: "val".into(),
            soft_dollar_tier_display_name: "shown".into(),
            order_combo_legs: vec![1.5, f64::MAX],
            adjustable_trailing_unit: i32::MAX,
            reference_contract_id: 7,
            parent_perm_id: 99,
            conditions: vec![
                OrderCondition::Price { con_id: 11, exchange: "EX1".into(), price: price_from_f64(101.5), is_more: true, trigger_method: 2 },
                OrderCondition::Time { time: "20260311-09:30:00".into(), is_more: false },
                OrderCondition::Margin { percent: 30, is_more: true },
                OrderCondition::Execution { symbol: "SYM".into(), exchange: "EX2".into(), sec_type: "STK".into() },
                OrderCondition::Volume { con_id: 12, exchange: "EX3".into(), volume: 5000, is_more: true },
                OrderCondition::PercentChange { con_id: 13, exchange: "EX4".into(), percent: 2.5, is_more: false },
            ],
            ..Default::default()
        }),
        "OrderExact" => w_order(w, &Order { total_quantity: 0.123456789012, total_quantity_text: "0.123456789012".into(), ..Default::default() }),
        "OrderUnset" => w_order(w, &Order::default()),
        "Contract" => w_contract(w, &Contract { strike: 0.0, ..Default::default() }),
        "ContractDetails" => w_contract_details(w, &ContractDetails {
            fund_distribution_policy_indicator: "Y".into(),
            fund_asset_type: "004".into(),
            ..Default::default()
        }),
        "CommissionAndFeesReport" => w_commission_and_fees_report(w, &CommissionAndFeesReport { yield_redemption_date: "20270115".into(), ..Default::default() }),
        "Bar" => w_bar(w, &BarData { volume: 1234, ..Default::default() }),
        _ => return false,
    }
    unsafe { sink(user, IbxStr::of_bytes(&buf)) };
    true
}

/// Call every callback of the table once, with values made from the names
/// of the Rust parameters.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ibx_test_fire(callbacks: *const IbxCallbacks, user: *mut c_void) {
    let Some(cb) = (unsafe { callbacks.as_ref() }) else { return };
    checks::fire(&mut CWrapper { cb, user, s: Scratch::default() });
}

/// Give `count` price ticks and as many size ticks to the table, as the
/// client gives them from market data: what a tick costs through the
/// layer is measured on the other side.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ibx_test_ticks(callbacks: *const IbxCallbacks, user: *mut c_void, count: u64) {
    use crate::api::wrapper::Wrapper;
    let Some(cb) = (unsafe { callbacks.as_ref() }) else { return };
    let mut w = CWrapper { cb, user, s: Scratch::default() };
    let attrib = crate::api::types::TickAttrib { can_auto_execute: true, ..Default::default() };
    for k in 0..count {
        w.tick_price(1, 1, 100.0 + (k & 7) as f64 * 0.25, &attrib);
        w.tick_size(1, 0, 200.0 + (k & 3) as f64);
    }
}

// ── Replay of a recorded scenario ──

/// The C++ side of a replay.
#[repr(C)]
pub struct IbxTestDriver {
    pub user: *mut c_void,
    /// The session of the replay: the C++ client takes it, and releases it
    /// after the replay has returned.
    pub adopt: unsafe extern "C" fn(user: *mut c_void, session: *mut IbxClient),
    /// Make a recorded request: its name, and its fields as records
    /// `path \x1f value \x1e`. True when the driver makes it.
    pub request: unsafe extern "C" fn(user: *mut c_void, name: IbxStr, fields: IbxStr) -> bool,
    /// Give the callbacks that wait, then hand them over as a JSON list of
    /// `[name, argument, ...]` in the form of the official client.
    pub dispatch: unsafe extern "C" fn(user: *mut c_void, sink: IbxTestSink, sink_user: *mut c_void),
}

struct CDriver<'a> {
    session: *mut IbxClient,
    driver: &'a IbxTestDriver,
}

impl CDriver<'_> {
    fn client(&self) -> &EClient {
        unsafe { &(*self.session).client }
    }
}

unsafe extern "C" fn take_text(user: *mut c_void, data: IbxStr) {
    let out = unsafe { &mut *(user as *mut String) };
    out.push_str(&unsafe { data.text() });
}

/// The fields of a recorded request, flat: `contract.symbol`, `order.algoParams.0.tag`;
/// a list also gives its length as `path.#`.
fn flatten(path: &str, v: &serde_json::Value, out: &mut String) {
    use serde_json::Value;
    let key = |k: &str| if path.is_empty() { k.to_string() } else { format!("{path}.{k}") };
    match v {
        Value::Null => {}
        Value::Object(map) => map.iter().for_each(|(k, v)| flatten(&key(k), v, out)),
        Value::Array(items) => {
            out.push_str(&format!("{}\x1f{}\x1e", key("#"), items.len()));
            items.iter().enumerate().for_each(|(k, v)| flatten(&key(&k.to_string()), v, out));
        }
        Value::String(s) => out.push_str(&format!("{path}\x1f{s}\x1e")),
        other => out.push_str(&format!("{path}\x1f{other}\x1e")),
    }
}

impl Driver for CDriver<'_> {
    fn start(&mut self, client_id: i64) {
        self.client().core.client_id.store(client_id, std::sync::atomic::Ordering::Relaxed);
    }

    fn request(&mut self, links: &mut Links, r: &Rec) -> bool {
        let mut fields = String::new();
        flatten("", &r.request, &mut fields);
        let (driver, name) = (self.driver, r.msg.as_str());
        links.during(|| unsafe { (driver.request)(driver.user, IbxStr::of(name), IbxStr::of(&fields)) })
    }

    fn next_due(&self) -> Option<std::time::Instant> {
        self.client().core.next_pnl_due()
    }

    fn dispatch(&mut self) -> Vec<String> {
        let mut calls = String::new();
        unsafe { (self.driver.dispatch)(self.driver.user, take_text, &mut calls as *mut String as *mut c_void) };
        let calls: serde_json::Value = serde_json::from_str(&calls).unwrap_or_else(|e| panic!("callbacks of the C++ driver: {e}\n{calls}"));
        calls.as_array().into_iter().flatten().filter_map(canonical).collect()
    }
}

/// Replay a recorded scenario with the C++ client as the API client and
/// compare its callbacks with the reference's. `options`: one `key=value`
/// per line, the options of the Python replay (`until`, `skip_seqs`,
/// `skip_orders`, `compare`, `farms`, `hmds_farms`, `replies_from`,
/// `skip_frame`, `frame_mask`, and the flags `codec`,
/// `positions_of_the_recording`, `after_account_updates`,
/// `lookups_at_their_point`, `paced`), and for the comparison `drop` (the
/// lines that start with one of these texts, `;` between them), `drop_id`
/// (the lines of these request ids) and `frames` (the least number of
/// frames compared). The sink gets what differs, empty when nothing does.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ibx_test_replay(name: IbxStr, options: IbxStr, driver: *const IbxTestDriver, sink: IbxTestSink, user: *mut c_void) -> bool {
    let Some(driver) = (unsafe { driver.as_ref() }) else { return false };
    let name = unsafe { name.text() }.into_owned();
    let options = unsafe { options.text() }.into_owned();
    let get = |key: &str| options.lines().find_map(|l| l.split_once('=').filter(|(k, _)| *k == key).map(|(_, v)| v.to_string()));
    let flag = |key: &str| options.lines().any(|l| l == key || l == format!("{key}=1"));
    let list = |key: &str| -> Vec<String> { get(key).map(|v| v.split(',').filter(|s| !s.is_empty()).map(str::to_string).collect()).unwrap_or_default() };
    let numbers = |key: &str| -> Vec<i64> { list(key).iter().filter_map(|s| s.parse().ok()).collect() };
    // Test-only: the names live as long as the process.
    let leak = |items: Vec<String>| -> Vec<&'static str> { items.into_iter().map(|s| &*Box::leak(s.into_boxed_str())).collect() };

    let scenario = if flag("codec") { load_codec(&name) } else { load_scenario(&name) };
    let skip_seqs: Vec<u64> = numbers("skip_seqs").into_iter().map(|n| n as u64).collect();
    let mut opts = Options::default().skip_seqs(&skip_seqs).skip_orders(&numbers("skip_orders"));
    if get("farms").is_some() {
        opts = opts.farms(&leak(list("farms")));
    }
    if get("hmds_farms").is_some() {
        opts = opts.hmds_farms(&leak(list("hmds_farms")));
    }
    if let Some(until) = get("until").and_then(|v| v.parse().ok()) {
        opts = opts.until(until);
    }
    if flag("positions_of_the_recording") {
        opts = opts.positions_of_the_recording();
    }
    if let Some(first) = get("replies_from") {
        opts = opts.replies_from(&load_scenario(&first));
    }
    use crate::test_support::scenario::filters;
    if let Some(f) = get("skip_frame") {
        opts = opts.skip_frame(filters::skip_frame_named(&f).unwrap_or_else(|| panic!("no frame filter {f}")));
    }
    if let Some(m) = get("frame_mask") {
        opts = opts.frame_mask(filters::frame_mask_named(&m).unwrap_or_else(|| panic!("no frame mask {m}")));
    }
    if flag("lookups_at_their_point") {
        opts = opts.lookups_at_their_point();
    }
    if flag("paced") {
        opts = opts.paced();
    }
    if let Some(kinds) = get("compare") {
        use crate::test_support::scenario::runner::{HISTORICAL, LOOKUP, MARKET_DATA, ORDER, SCANNER, SUBSCRIPTION};
        let known = [ORDER, LOOKUP, MARKET_DATA, SUBSCRIPTION, HISTORICAL, SCANNER];
        let kinds: Vec<&'static str> = known.iter().copied().filter(|k| kinds.split(',').any(|w| w == *k)).collect();
        opts = opts.compare(&kinds);
    }

    // The replay runs in the recording's machine zone and level; this
    // thread gets its own back after it.
    let zone_before = crate::gateway::machine_zone_for_test();
    let level_before = crate::client_core::session_level_for_test();
    let mut links = Links::new();
    let client = EClient::from_parts(links.shared.clone(), links.control_tx.clone(), std::thread::spawn(|| {}), ACCOUNT.into());
    let session = Box::into_raw(Box::new(IbxClient::new(client, None)));
    unsafe { (driver.adopt)(driver.user, session) };
    if flag("after_account_updates") {
        let _ = unsafe { &(*session).client }.core.subscribe_account_updates(true);
    }
    let mut d = CDriver { session, driver };
    let outcome = run(&scenario, &opts, &mut links, &mut d);
    crate::gateway::set_machine_zone_for_test(zone_before.as_deref());
    crate::client_core::set_session_level_for_test(level_before);

    // What differs.
    let drop: Vec<String> = get("drop").map(|v| v.split(';').map(str::to_string).collect()).unwrap_or_default();
    let drop_id = list("drop_id");
    let keep = |l: &String| !drop.iter().any(|p| l.starts_with(p.as_str())) && !l.split('|').nth(1).is_some_and(|id| drop_id.iter().any(|d| d == id));
    let ours: Vec<&String> = outcome.ours.iter().filter(|l| keep(l)).collect();
    let theirs: Vec<&String> = outcome.theirs.iter().map(|(_, l)| l).filter(|l| keep(l)).collect();
    let mut report = String::new();
    if let Some(e) = &outcome.frame_error {
        report.push_str(&format!("frame: {e}\n"));
    }
    if !outcome.not_made.is_empty() {
        report.push_str(&format!("requests not made: {:?}\n", outcome.not_made));
    }
    if let Some(least) = get("frames").and_then(|v| v.parse::<usize>().ok()) {
        if outcome.frames_compared < least {
            report.push_str(&format!("frames compared: {} (expected at least {least})\n", outcome.frames_compared));
        }
    }
    for (k, (a, b)) in ours.iter().zip(theirs.iter()).enumerate() {
        if a != b {
            report.push_str(&format!("callback {k}:\n  ours      {a}\n  reference {b}\n"));
            break;
        }
    }
    if ours.len() != theirs.len() && report.is_empty() {
        report.push_str(&format!("callbacks: {} of ours, {} of the reference\n  ours left      {:?}\n  reference left {:?}\n",
            ours.len(), theirs.len(), &ours[theirs.len().min(ours.len())..], &theirs[ours.len().min(theirs.len())..]));
    }
    if theirs.is_empty() {
        report.push_str("no callback of the reference was compared\n");
    }
    unsafe { sink(user, IbxStr::of(&report)) };
    true
}
