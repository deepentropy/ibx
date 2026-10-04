//! Microbenchmark: farm tick message to API callback (ibx#446).
//!
//! The engine on in-memory links (no network) with the Rust API client on
//! top, one SPY stream acked as the farm acks it. Each iteration hands one
//! 35=P message to the engine's farm handler (decode, quote update,
//! publish) and runs `process_msgs` (dispatch to the wrapper); the time of
//! both is measured, and of each part alone. A second run hands 10
//! messages before one `process_msgs`, the client reading slower than the
//! messages come.
//!
//! Usage:
//!   cargo run --release --features test-support --bin bench_md_dispatch

use std::hint::black_box;

use ibx::api::types::{Contract, TickAttrib};
use ibx::api::wrapper::Wrapper;
use ibx::test_support::scenario::session::Session;

const WARMUP: usize = 20_000;
const ITERATIONS: usize = 200_000;
/// Message variants cycled through, so that values change.
const VARIANTS: usize = 64;

/// Counts the market data callbacks.
#[derive(Default)]
struct Counter {
    ticks: u64,
}

impl Wrapper for Counter {
    fn tick_price(&mut self, _: i64, tick_type: i32, price: f64, _: &TickAttrib) {
        black_box((tick_type, price));
        self.ticks += 1;
    }
    fn tick_size(&mut self, _: i64, tick_type: i32, size: f64) {
        black_box((tick_type, size));
        self.ticks += 1;
    }
    fn tick_string(&mut self, _: i64, tick_type: i32, value: &str) {
        black_box((tick_type, value.len()));
        self.ticks += 1;
    }
    fn tick_generic(&mut self, _: i64, tick_type: i32, value: f64) {
        black_box((tick_type, value));
        self.ticks += 1;
    }
}

/// A block of a 35=P message: (daily stats, server tag, [(wire type, value)]).
type Block<'a> = (bool, u32, &'a [(u64, i64)]);

/// A 35=P message as the farm sends it (4-byte values).
fn tick_message(blocks: &[Block]) -> Vec<u8> {
    let mut bits: Vec<u8> = Vec::new();
    let mut push = |v: u64, n: usize| for i in (0..n).rev() { bits.push(((v >> i) & 1) as u8) };
    for (stats, tag, entries) in blocks {
        push(*stats as u64, 1);
        push(*tag as u64, 31);
        for (k, (tick_type, value)) in entries.iter().enumerate() {
            push(*tick_type, 5);
            push((k + 1 < entries.len()) as u64, 1);
            push(3, 2);
            push((*value < 0) as u64, 1);
            push(value.unsigned_abs(), 31);
        }
    }
    let mut body = vec![(bits.len() >> 8) as u8, bits.len() as u8];
    body.resize(2 + bits.len().div_ceil(8), 0);
    for (i, b) in bits.iter().enumerate() {
        body[2 + i / 8] |= b << (7 - i % 8);
    }
    let mut msg = b"8=O\x019=0\x0135=P\x01".to_vec();
    msg.extend_from_slice(&body);
    msg
}

const QUOTE_TAG: u32 = 41;
const TRADE_TAG: u32 = 43;

/// A SPY stream: requested, acked (bid/ask and last entries) and set up
/// (trade tag), its first callbacks taken.
fn spy_session() -> Session {
    let mut s = Session::new();
    let spy = Contract {
        con_id: 756733, symbol: "SPY".into(), sec_type: "STK".into(), exchange: "SMART".into(),
        currency: "USD".into(), ..Default::default()
    };
    s.call(|c| c.req_mkt_data(1, &spy, "", false, false).unwrap());
    // A stock waits for its round lot: the definition lookup's answer.
    let lookup = s.ccp_out.iter().find(|f| f.iter().any(|(t, v)| *t == 35 && v == "c"))
        .and_then(|f| f.iter().find(|(t, _)| *t == 320).map(|(_, v)| v.clone()))
        .expect("no definition lookup");
    let reply = ibx::protocol::fix::fix_build(&[(35, "d"), (320, &lookup), (6008, "756733"), (167, "CS"),
        (6523, "USSTK"), (6030, "1"), (6023, "100"), (6027, "100")], 1);
    s.send_ccp(&reply);
    let ids: Vec<String> = s.farm_out.iter()
        .filter(|f| f.iter().any(|(t, v)| *t == 35 && v == "V"))
        .flat_map(|f| f.iter().filter(|(t, _)| *t == 262).map(|(_, v)| v.clone()).collect::<Vec<_>>())
        .collect();
    assert!(ids.len() >= 2, "no market data request: {:?}", s.farm_out);
    for (tag, id) in [(QUOTE_TAG, &ids[0]), (QUOTE_TAG, &ids[1])] {
        let ack = format!("8=O\x0135=Q\x01{tag},{id},0.01,0,3,a6,,1,1");
        s.engine.inject_farm_message(ack.as_bytes());
    }
    let setup = format!("8=O\x0135=L\x01756733,0.01,{TRADE_TAG},,1");
    s.engine.inject_farm_message(setup.as_bytes());
    s.settle();
    s
}

/// The quote-only messages: the bid, ask and their sizes move.
fn quote_messages() -> Vec<Vec<u8>> {
    (0..VARIANTS as i64).map(|i| {
        tick_message(&[(false, QUOTE_TAG, &[(0, 66_000 + i % 7), (4, 10 + i % 5), (1, 66_010 + i % 3), (5, 20 + i % 4)])])
    }).collect()
}

/// Messages with a quote, a trade (time, price, size) and daily figures.
fn full_messages() -> Vec<Vec<u8>> {
    (0..VARIANTS as i64).map(|i| {
        tick_message(&[
            (false, QUOTE_TAG, &[(0, 66_000 + i % 7), (4, 10 + i % 5), (1, 66_010 + i % 3), (5, 20 + i % 4)]),
            (false, TRADE_TAG, &[(2, 66_005 + i % 6), (6, 1 + i % 3), (20, 1_790_000_000 + i)]),
            (true, TRADE_TAG, &[(10, 1_000 + i), (8, 66_100 + i % 2)]),
        ])
    }).collect()
}

struct Stats {
    name: String,
    ns: Vec<f64>,
    ticks_per_message: f64,
}

impl Stats {
    fn print(&mut self) {
        self.ns.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let n = self.ns.len();
        let at = |p: f64| self.ns[((n as f64 * p) as usize).min(n - 1)];
        let mean = self.ns.iter().sum::<f64>() / n as f64;
        println!(
            "  {:<44} mean {:>8.0} ns  p50 {:>8.0}  p99 {:>8.0}  p99.9 {:>8.0}  callbacks/msg {:>5.1}",
            self.name, mean, at(0.5), at(0.99), at(0.999), self.ticks_per_message,
        );
    }
}

/// Each message alone: decode, then dispatch; both, and each part.
fn per_message(name: &str, messages: &[Vec<u8>]) -> [Stats; 3] {
    let mut s = spy_session();
    let clock = quanta::Clock::new();
    let mut w = Counter::default();
    let mut both = Vec::with_capacity(ITERATIONS);
    let mut decode = Vec::with_capacity(ITERATIONS);
    let mut dispatch = Vec::with_capacity(ITERATIONS);
    for i in 0..WARMUP + ITERATIONS {
        let msg = &messages[i % messages.len()];
        let t0 = clock.raw();
        s.engine.inject_farm_message(msg);
        let t1 = clock.raw();
        s.client.process_msgs(&mut w);
        let t2 = clock.raw();
        if i == WARMUP {
            w.ticks = 0;
        }
        if i >= WARMUP {
            decode.push(clock.delta(t0, t1).as_nanos() as f64);
            dispatch.push(clock.delta(t1, t2).as_nanos() as f64);
            both.push(clock.delta(t0, t2).as_nanos() as f64);
        }
    }
    let per = w.ticks as f64 / ITERATIONS as f64;
    [
        Stats { name: format!("{name}: decode + dispatch"), ns: both, ticks_per_message: per },
        Stats { name: format!("{name}: decode (engine)"), ns: decode, ticks_per_message: per },
        Stats { name: format!("{name}: dispatch (client)"), ns: dispatch, ticks_per_message: per },
    ]
}

/// `batch` messages, then one dispatch: the time per message.
fn batched(name: &str, messages: &[Vec<u8>], batch: usize) -> Stats {
    let mut s = spy_session();
    let clock = quanta::Clock::new();
    let mut w = Counter::default();
    let rounds = ITERATIONS / batch;
    let mut ns = Vec::with_capacity(rounds);
    let mut k = 0;
    for r in 0..(WARMUP / batch) + rounds {
        let t0 = clock.raw();
        for _ in 0..batch {
            s.engine.inject_farm_message(&messages[k % messages.len()]);
            k += 1;
        }
        s.client.process_msgs(&mut w);
        let t1 = clock.raw();
        if r == WARMUP / batch {
            w.ticks = 0;
        }
        if r >= WARMUP / batch {
            ns.push(clock.delta(t0, t1).as_nanos() as f64 / batch as f64);
        }
    }
    Stats {
        name: format!("{name}: {batch} messages per dispatch, per msg"),
        ns,
        ticks_per_message: w.ticks as f64 / (rounds * batch) as f64,
    }
}

/// The engine alone: each message decoded and applied, no dispatch; with
/// `listened` false no API request takes the instrument's steps (the
/// engine API's own use).
fn engine_only(name: &str, messages: &[Vec<u8>], listened: bool) -> Stats {
    let mut s = spy_session();
    if !listened {
        // The first slot of the session.
        assert!(s.shared.market.md_events.listened(0));
        s.shared.market.md_events.listen(0, false);
    }
    let clock = quanta::Clock::new();
    let mut ns = Vec::with_capacity(ITERATIONS);
    let mut w = Counter::default();
    for i in 0..WARMUP + ITERATIONS {
        let t0 = clock.raw();
        s.engine.inject_farm_message(&messages[i % messages.len()]);
        let t1 = clock.raw();
        if i >= WARMUP {
            ns.push(clock.delta(t0, t1).as_nanos() as f64);
        }
        // The client reads now and then: the queue never fills.
        if i % 1000 == 999 {
            s.client.process_msgs(&mut w);
        }
    }
    let what = if listened { "a request listens" } else { "no request listens" };
    Stats { name: format!("{name}: decode only, {what}"), ns, ticks_per_message: 0.0 }
}

fn main() {
    println!("========================================");
    println!("  Bench: farm tick message to API callback");
    println!("========================================");
    println!("  Iterations: {ITERATIONS}  Warmup: {WARMUP}");
    println!();
    let quotes = quote_messages();
    let full = full_messages();
    for mut st in per_message("quote msg", &quotes) {
        st.print();
    }
    for mut st in per_message("quote+trade+daily msg", &full) {
        st.print();
    }
    for listened in [true, false] {
        engine_only("quote msg", &quotes, listened).print();
        engine_only("quote+trade+daily msg", &full, listened).print();
    }
    batched("quote msg", &quotes, 10).print();
    batched("quote+trade+daily msg", &full, 10).print();
}
