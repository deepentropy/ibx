//! The C entry layer of the client (ibx#235): a session handle, the
//! requests and a table of callbacks, for a program in C or C++. The C++
//! library of `cpp/` sits on it and gives the classes of the official C++
//! client.
//!
//! Built with the `capi` feature. The header is `cpp/include/ibx_capi.h`;
//! it and `generated.rs` are written by `cpp/gen/generate.py`.
//!
//! A text crosses as a pointer and a length, a structure as its fields
//! encoded one after the other (see [`wire`]). The callbacks of market
//! data take numbers only: nothing is encoded or copied on that path.

#![allow(clippy::missing_safety_doc)]

mod generated;
#[cfg(feature = "test-support")]
pub(crate) mod testing;
pub(crate) mod wire;

use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Mutex;
use std::time::Duration;

use crate::api::types::{ContractDescription, TagValue, TickAttrib, TickAttribBidAsk, TickAttribLast};
use crate::api::wrapper::Wrapper;
use crate::api::{EClient, EClientConfig};
use crate::types::{HistoricalTickData, OrderCondition, SmartComponent, SoftDollarTier};

pub use generated::IbxCallbacks;
use generated::CWrapper;
use wire::{R, W};

/// A text or an encoded structure: a pointer and a length. A text is
/// UTF-8 and has no end marker.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct IbxStr {
    pub p: *const u8,
    pub n: usize,
}

impl IbxStr {
    #[inline]
    pub(crate) fn of(s: &str) -> Self {
        Self { p: s.as_ptr(), n: s.len() }
    }

    #[inline]
    pub(crate) fn of_bytes(b: &[u8]) -> Self {
        Self { p: b.as_ptr(), n: b.len() }
    }

    /// The bytes the caller points to; valid for the call.
    #[inline]
    pub(crate) unsafe fn bytes<'a>(self) -> &'a [u8] {
        if self.p.is_null() || self.n == 0 {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(self.p, self.n) }
        }
    }

    /// The text the caller points to; bytes that are not UTF-8 are replaced.
    #[inline]
    pub(crate) unsafe fn text<'a>(self) -> std::borrow::Cow<'a, str> {
        String::from_utf8_lossy(unsafe { self.bytes() })
    }
}

/// The challenge of a typed second factor, as the code provider gets it.
#[repr(C)]
pub struct IbxChallenge {
    pub display_id: IbxStr,
    pub url: IbxStr,
    pub kind: IbxStr,
    pub message: IbxStr,
    pub image: IbxStr,
    pub delivery: IbxStr,
}

/// Writes the code and returns its length, or a negative number to end the login.
pub type IbxCodeProvider = unsafe extern "C" fn(user: *mut c_void, challenge: *const IbxChallenge, code: *mut u8, capacity: usize) -> i32;

#[repr(C)]
pub struct IbxConnectConfig {
    pub username: IbxStr,
    pub password: IbxStr,
    pub host: IbxStr,
    pub paper: bool,
    pub copied_waits: bool,
    pub client_id: i32,
    /// The core the engine thread is pinned to; negative: not pinned.
    pub core_id: i32,
    pub code_provider: Option<IbxCodeProvider>,
    pub code_user: *mut c_void,
}

/// The buffers the encoded parameters of one callback are written to,
/// kept between callbacks.
#[derive(Default)]
pub(crate) struct Scratch {
    pub b0: Vec<u8>,
    pub b1: Vec<u8>,
    pub b2: Vec<u8>,
}

/// The server a login goes to when the caller names none.
const DEFAULT_HOST: &str = "cdc1.ibllc.com";

/// A session, as the caller holds it.
pub struct IbxClient {
    pub(crate) client: EClient,
    /// The buffers of the last pass, taken by the next one.
    scratch: Mutex<Option<Scratch>>,
    /// What a session starts with, given by the first pass before anything
    /// else: the managed accounts and the next valid order id.
    start: Mutex<Option<(String, i64)>>,
}

impl IbxClient {
    pub(crate) fn new(client: EClient, start: Option<(String, i64)>) -> Self {
        Self { client, scratch: Mutex::new(None), start: Mutex::new(start) }
    }
}

/// A wrapper that takes nothing, for the requests answered by `ibx_process`.
pub(crate) struct Nothing;
impl Wrapper for Nothing {}

/// What a request returns to the caller.
pub(crate) trait Status {
    fn status(self) -> i32;
}

impl Status for () {
    fn status(self) -> i32 {
        0
    }
}

impl Status for Result<(), String> {
    /// The requests fail only when the session has ended; every other
    /// refusal is an error callback.
    fn status(self) -> i32 {
        match self {
            Ok(()) => 0,
            Err(why) => {
                log::debug!("request not sent: {why}");
                1
            }
        }
    }
}

/// Run a request of the client. Its wrapper is never called: the answers
/// come from `ibx_process`. A panic stops at the boundary.
pub(crate) fn request(h: *mut IbxClient, call: impl FnOnce(&EClient, &mut Nothing) -> i32) -> i32 {
    let Some(h) = (unsafe { h.as_ref() }) else { return 1 };
    catch_unwind(AssertUnwindSafe(|| call(&h.client, &mut Nothing))).unwrap_or(2)
}

/// An error the layer answers itself: given by `ibx_process` at its place
/// among the answers, as an error of the request or order `id`.
pub(crate) fn refuse(c: &EClient, id: i64, code: i64, text: &str) -> i32 {
    c.shared.orders.push_order_error(id, code, text.to_string());
    c.shared.notify();
    0
}

// ── Encoders written by hand ──

#[inline]
pub(crate) fn mask_tick_attrib(a: &TickAttrib) -> u32 {
    a.can_auto_execute as u32 | (a.past_limit as u32) << 1 | (a.pre_open as u32) << 2
}

#[inline]
pub(crate) fn mask_tick_attrib_last(a: &TickAttribLast) -> u32 {
    a.past_limit as u32 | (a.unreported as u32) << 1
}

#[inline]
pub(crate) fn mask_tick_attrib_bid_ask(a: &TickAttribBidAsk) -> u32 {
    a.bid_past_low as u32 | (a.ask_past_high as u32) << 1
}

pub(crate) fn read_tags(bytes: &[u8]) -> Vec<TagValue> {
    let mut r = R::new(bytes);
    let n = r.n();
    (0..n).map(|_| TagValue { tag: r.s(), value: r.s() }).collect()
}

pub(crate) fn enc_texts(w: &mut W, items: &[String]) {
    w.n(items.len());
    for s in items {
        w.s(s);
    }
}

pub(crate) fn enc_doubles(w: &mut W, items: &[f64]) {
    w.n(items.len());
    for v in items {
        w.f(*v);
    }
}

pub(crate) fn enc_soft_dollar_tiers(w: &mut W, tiers: &[SoftDollarTier]) {
    w.n(tiers.len());
    for t in tiers {
        w.s(&t.name);
        w.s(&t.val);
        w.s(&t.display_name);
    }
}

pub(crate) fn enc_contract_descriptions(w: &mut W, items: &[ContractDescription]) {
    w.n(items.len());
    for d in items {
        w.i(d.con_id);
        w.s(&d.symbol);
        w.s(&d.sec_type);
        w.s(&d.primary_exchange);
        w.s(&d.currency);
        w.s(&d.description);
        w.s(&d.issuer_id);
        enc_texts(w, &d.derivative_sec_types);
    }
}

pub(crate) fn enc_smart_components(w: &mut W, items: &[SmartComponent]) {
    w.n(items.len());
    for c in items {
        w.i(i64::from(c.bit_number));
        w.s(&c.exchange);
        w.s(&c.exchange_letter);
    }
}

pub(crate) fn enc_histogram(w: &mut W, items: &[(f64, i64)]) {
    w.n(items.len());
    for (price, size) in items {
        w.f(*price);
        w.f(*size as f64);
    }
}

/// The sessions of a schedule; the wrapper gives each as its day, its
/// start and its end.
pub(crate) fn enc_sessions(w: &mut W, sessions: &[(String, String, String)]) {
    w.n(sessions.len());
    for (date, start, end) in sessions {
        w.s(start);
        w.s(end);
        w.s(date);
    }
}

/// The historical ticks of one kind; each callback gets its own kind.
pub(crate) fn enc_ticks(w: &mut W, ticks: &HistoricalTickData) {
    match ticks {
        HistoricalTickData::Midpoint(rows) => {
            w.n(rows.len());
            for t in rows {
                w.i(t.time);
                w.f(t.price);
                w.f(t.size);
            }
        }
        HistoricalTickData::BidAsk(rows) => {
            w.n(rows.len());
            for t in rows {
                w.i(t.time);
                w.i(i64::from(mask_tick_attrib_bid_ask(&t.tick_attrib_bid_ask)));
                w.f(t.price_bid);
                w.f(t.price_ask);
                w.f(t.size_bid);
                w.f(t.size_ask);
            }
        }
        HistoricalTickData::Last(rows) => {
            w.n(rows.len());
            for t in rows {
                w.i(t.time);
                w.i(i64::from(mask_tick_attrib_last(&t.tick_attrib_last)));
                w.f(t.price);
                w.f(t.size);
                w.s(&t.exchange);
                w.s(&t.special_conditions);
            }
        }
    }
}

// The kinds of an order condition, as the official client numbers them.
const COND_PRICE: i64 = 1;
const COND_TIME: i64 = 3;
const COND_MARGIN: i64 = 4;
const COND_EXECUTION: i64 = 5;
const COND_VOLUME: i64 = 6;
const COND_PERCENT_CHANGE: i64 = 7;

/// A condition crosses as its kind, whether it is joined to the next one
/// by "and", then the fields every kind may have.
pub(crate) fn write_conditions(w: &mut W, conditions: &[OrderCondition]) {
    w.n(conditions.len());
    for c in conditions {
        let (kind, is_more, con_id, exchange, number, text, symbol, sec_type, method) = match c {
            OrderCondition::Price { con_id, exchange, price, is_more, trigger_method } => (
                COND_PRICE, *is_more, *con_id, exchange.as_str(), *price as f64 / crate::api::types::PRICE_SCALE_F, "", "", "", i64::from(*trigger_method),
            ),
            OrderCondition::Time { time, is_more } => (COND_TIME, *is_more, 0, "", 0.0, time.as_str(), "", "", 0),
            OrderCondition::Margin { percent, is_more } => (COND_MARGIN, *is_more, 0, "", f64::from(*percent), "", "", "", 0),
            OrderCondition::Execution { symbol, exchange, sec_type } => (COND_EXECUTION, false, 0, exchange.as_str(), 0.0, "", symbol.as_str(), sec_type.as_str(), 0),
            OrderCondition::Volume { con_id, exchange, volume, is_more } => (COND_VOLUME, *is_more, *con_id, exchange.as_str(), *volume as f64, "", "", "", 0),
            OrderCondition::PercentChange { con_id, exchange, percent, is_more } => (COND_PERCENT_CHANGE, *is_more, *con_id, exchange.as_str(), *percent, "", "", "", 0),
        };
        w.i(kind);
        // The conditions of an order are joined by "and".
        w.b(true);
        w.b(is_more);
        w.i(con_id);
        w.s(exchange);
        w.f(number);
        w.s(text);
        w.s(symbol);
        w.s(sec_type);
        w.i(method);
    }
}

/// The conditions of an order to send. One the client cannot send as
/// given refuses the order: it must not go out without it.
pub(crate) fn read_conditions(r: &mut R) -> Vec<OrderCondition> {
    let n = r.n();
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let kind = r.i();
        let and = r.b();
        let is_more = r.b();
        let con_id = r.i();
        let exchange = r.s();
        let number = r.f();
        let text = r.s();
        let symbol = r.s();
        let sec_type = r.s();
        let method = r.i();
        // The conditions of an order are joined by "and" here.
        if i + 1 < n && !and {
            r.refuse(321, "Error validating request.-'PlaceOrder(3)' : cause - Conditions joined by \"or\" are not supported.");
        }
        out.push(match kind {
            COND_PRICE => OrderCondition::Price {
                con_id, exchange, price: crate::api::types::price_from_f64(number), is_more, trigger_method: method as u8,
            },
            COND_TIME => OrderCondition::Time { time: text, is_more },
            COND_MARGIN => OrderCondition::Margin { percent: number.max(0.0) as u32, is_more },
            COND_EXECUTION => OrderCondition::Execution { symbol, exchange, sec_type },
            COND_VOLUME => OrderCondition::Volume { con_id, exchange, volume: number as i64, is_more },
            COND_PERCENT_CHANGE => OrderCondition::PercentChange {
                con_id, exchange, percent: if number == f64::MAX { 0.0 } else { number }, is_more,
            },
            _ => {
                r.refuse(321, "Error validating request.-'PlaceOrder(3)' : cause - Unknown order condition type.");
                continue;
            }
        });
    }
    out
}

// ── Session ──

/// Copy a text into the caller's buffer, as much as fits, and return its
/// full length.
unsafe fn copy_out(text: &str, out: *mut u8, capacity: usize) -> usize {
    if !out.is_null() && capacity > 0 {
        let n = text.len().min(capacity);
        unsafe { std::ptr::copy_nonoverlapping(text.as_ptr(), out, n) };
    }
    text.len()
}

/// The caller's code provider, called on the login's own thread.
struct CodeSource {
    call: IbxCodeProvider,
    user: *mut c_void,
}

// The caller keeps `user` valid and usable from another thread for the
// time of the login: it is the contract of the code provider.
unsafe impl Send for CodeSource {}
unsafe impl Sync for CodeSource {}

fn code_provider(source: CodeSource) -> crate::auth::session::CodeProvider {
    std::sync::Arc::new(move |challenge| {
        let source = &source;
        let c = IbxChallenge {
            display_id: IbxStr::of(&challenge.display_id),
            url: IbxStr::of(&challenge.avth_url),
            kind: IbxStr::of(&challenge.kind),
            message: IbxStr::of(&challenge.message),
            image: IbxStr::of_bytes(&challenge.image),
            delivery: IbxStr::of(&challenge.delivery),
        };
        let mut code = [0u8; 128];
        let n = unsafe { (source.call)(source.user, &c, code.as_mut_ptr(), code.len()) };
        if n < 0 {
            return Err(std::io::Error::other("the code provider ended the login"));
        }
        Ok(String::from_utf8_lossy(&code[..(n as usize).min(code.len())]).into_owned())
    })
}

/// Log in and start the engine. Returns null on failure, with the reason
/// in `error` (cut to its capacity, not ended by a zero).
///
/// The first pass of `ibx_process` gives the managed accounts and the
/// next valid order id before anything else, as a session of the
/// official client starts.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ibx_connect(config: *const IbxConnectConfig, error: *mut u8, error_capacity: usize) -> *mut IbxClient {
    let fail = |why: String| -> *mut IbxClient {
        let n = unsafe { copy_out(&why, error, error_capacity) };
        if !error.is_null() && n < error_capacity {
            unsafe { *error.add(n) = 0 };
        }
        std::ptr::null_mut()
    };
    if config.is_null() {
        return fail("no configuration".into());
    }
    let config = unsafe { &*config };
    let host = unsafe { config.host.text() }.into_owned();
    let settings = EClientConfig {
        username: unsafe { config.username.text() }.into_owned(),
        password: unsafe { config.password.text() }.into_owned(),
        host: if host.is_empty() { DEFAULT_HOST.into() } else { host },
        paper: config.paper,
        core_id: usize::try_from(config.core_id).ok(),
        client_id: config.client_id,
        copied_waits: config.copied_waits,
        code_provider: config.code_provider.map(|call| code_provider(CodeSource { call, user: config.code_user })),
    };
    // On a thread of its own, with the engine's stack: the caller's thread
    // may have a small one, and the login builds the engine on its stack.
    let login = std::thread::Builder::new()
        .name("ibx-login".into())
        .stack_size(crate::engine::hot_loop::ENGINE_STACK)
        .spawn(move || -> Result<IbxClient, String> {
            let client = EClient::connect(&settings).map_err(|e| e.to_string())?;
            // The managed accounts, then the next valid id once the orders
            // of the logon are known, as the reference sends them and as
            // the Python client gives them at its connect (ibx#466).
            let accounts = client.shared.reference.managed_accounts_text(&client.account_id);
            crate::client_core::ClientCore::wait_order_replay(&client.shared);
            let next_id = client.core.next_valid_id(&client.shared);
            Ok(IbxClient::new(client, Some((accounts, next_id))))
        });
    let session = match login.map(|thread| thread.join()) {
        Ok(Ok(Ok(session))) => session,
        Ok(Ok(Err(why))) => return fail(why),
        Ok(Err(_)) => return fail("the login stopped unexpectedly".into()),
        Err(why) => return fail(format!("no thread for the login: {why}")),
    };
    Box::into_raw(Box::new(session))
}

/// End the session: the engine stops and its thread is joined. The handle
/// stays valid until `ibx_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ibx_disconnect(h: *mut IbxClient) {
    if let Some(h) = unsafe { h.as_ref() } {
        // A session the caller ends gives no closing callback, as the
        // official client: that one is for a session the servers end.
        h.client.close_notified.store(true, std::sync::atomic::Ordering::Release);
        let _ = catch_unwind(AssertUnwindSafe(|| h.client.disconnect()));
    }
}

/// Release the handle; the session ends if it has not.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ibx_free(h: *mut IbxClient) {
    if !h.is_null() {
        let _ = catch_unwind(AssertUnwindSafe(|| drop(unsafe { Box::from_raw(h) })));
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn ibx_is_connected(h: *const IbxClient) -> bool {
    unsafe { h.as_ref() }.is_some_and(|h| h.client.is_connected())
}

/// The API level of the session.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ibx_server_version(h: *const IbxClient) -> i32 {
    unsafe { h.as_ref() }.map_or(0, |h| h.client.server_version())
}

/// The time the session started, as the official client gives it.
/// Returns the full length of the text.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ibx_connection_time(h: *const IbxClient, out: *mut u8, capacity: usize) -> usize {
    match unsafe { h.as_ref() } {
        Some(h) => unsafe { copy_out(&h.client.tws_connection_time(), out, capacity) },
        None => 0,
    }
}

/// The account of the login. Returns the full length of the text.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ibx_account_id(h: *const IbxClient, out: *mut u8, capacity: usize) -> usize {
    match unsafe { h.as_ref() } {
        Some(h) => unsafe { copy_out(&h.client.account_id, out, capacity) },
        None => 0,
    }
}

/// Wait until an answer is ready or the time has passed. True when one is.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ibx_wait(h: *const IbxClient, timeout_ms: u32) -> bool {
    unsafe { h.as_ref() }.is_some_and(|h| h.client.shared.wait_for_data(Duration::from_millis(u64::from(timeout_ms))))
}

/// End a wait from another thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ibx_notify(h: *const IbxClient) {
    if let Some(h) = unsafe { h.as_ref() } {
        h.client.shared.notify();
    }
}

/// One pass: what the session starts with, then every ready answer.
fn pass(h: &IbxClient, wrapper: &mut impl Wrapper) {
    let start = h.start.lock().ok().and_then(|mut start| start.take());
    if let Some((accounts, next_id)) = start {
        wrapper.managed_accounts(&accounts);
        wrapper.next_valid_id(next_id);
    }
    h.client.process_msgs(wrapper);
}

/// Give every ready answer to the caller's table, on the calling thread.
/// A callback may make requests. Returns 0, or 2 when the pass stopped
/// unexpectedly.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ibx_process(h: *mut IbxClient, callbacks: *const IbxCallbacks, user: *mut c_void) -> i32 {
    let (Some(h), Some(cb)) = (unsafe { h.as_ref() }, unsafe { callbacks.as_ref() }) else { return 2 };
    let s = h.scratch.lock().map(|mut kept| kept.take()).unwrap_or_default().unwrap_or_default();
    let mut wrapper = CWrapper { cb, user, s };
    let passed = catch_unwind(AssertUnwindSafe(|| pass(h, &mut wrapper)));
    if let Ok(mut kept) = h.scratch.lock() {
        *kept = Some(wrapper.s);
    }
    if passed.is_ok() { 0 } else { 2 }
}

#[cfg(test)]
mod tests;
