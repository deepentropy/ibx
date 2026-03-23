//! Replay benchmark: measures IBX processing pipeline without network I/O.
//!
//! Benchmarks the full message processing path using synthetic (or captured) data:
//!   1. HMAC unsign (verify + un-distort)
//!   2. FIXCOMP decompression
//!   3. Tick decode (35=P binary payload)
//!   4. Market state update (quote mutation)
//!   5. Full pipeline: sign → unsign → decode → state update
//!
//! When a JSONL fixture file is provided (from ib-agent captures), it loads
//! real 35=P messages captured from the wire. Otherwise uses synthetic payloads.
//!
//! Usage:
//!   cargo run --release --bin bench_replay
//!   cargo run --release --bin bench_replay -- --fixture path/to/fix-agent-gw.jsonl

use std::sync::Arc;
use std::time::Instant;

use crossbeam_channel::bounded;

use ibx::bridge::{Event, SharedState};
use ibx::engine::market_state::MarketState;
use ibx::protocol::fix::{fix_build, fix_sign, fix_unsign};
use ibx::protocol::fixcomp::{fixcomp_build, fixcomp_decompress};
use ibx::protocol::tick_decoder::{self, RawTick};
use ibx::types::Quote;

const ITERATIONS: u64 = 1_000_000;
const WARMUP: u64 = 100_000;

// Simulated session keys (realistic size, arbitrary values)
const MAC_KEY: [u8; 20] = [
    0x4a, 0x1b, 0x7c, 0x3d, 0x5e, 0x8f, 0x2a, 0x6b, 0x9c, 0x0d,
    0x1e, 0x4f, 0x7a, 0x3b, 0x5c, 0x8d, 0x2e, 0x6f, 0x9a, 0x0b,
];
const INIT_IV: [u8; 16] = [
    0xa1, 0x52, 0xb3, 0x64, 0xc5, 0x76, 0xd7, 0x88,
    0xe9, 0x1a, 0xf2, 0x3b, 0x04, 0x5c, 0x16, 0x7d,
];

fn main() {
    let fixture_path = std::env::args().nth(1).filter(|a| a != "--fixture")
        .or_else(|| std::env::args().nth(2));

    // ── Build test payloads ──
    let tick_payload = build_35p_payload(1, &[
        (0, 2, 15025, false),  // bid_price
        (1, 2, 15030, false),  // ask_price
        (2, 2, 15028, false),  // last_price
        (4, 2, 500, false),    // bid_size
        (5, 2, 300, false),    // ask_size
    ]);

    let tick_payload_heavy = build_35p_payload_multi(&[
        (1, &[
            (0, 2, 15025, false), (1, 2, 15030, false),
            (2, 2, 15028, false), (4, 2, 500, false), (5, 2, 300, false),
        ]),
        (2, &[
            (0, 2, 20050, false), (1, 2, 20055, false),
            (2, 2, 20052, false), (4, 2, 200, false), (6, 4, 1_000_000, false),
        ]),
    ]);

    // Build FIX-framed 35=P messages (unsigned, for sign→unsign roundtrip)
    let fix_tick_msg = build_fix_tick_message(&tick_payload);
    let fix_tick_heavy = build_fix_tick_message(&tick_payload_heavy);

    // Sign them (so we can benchmark unsign)
    let (signed_msg, _iv_after) = fix_sign(&fix_tick_msg, &MAC_KEY, &INIT_IV);
    let (signed_heavy, _) = fix_sign(&fix_tick_heavy, &MAC_KEY, &INIT_IV);

    // Build FIXCOMP-wrapped message (compressed tick data)
    let fixcomp_msg = fixcomp_build(&fix_tick_msg);
    let (signed_fixcomp, _) = fix_sign(&fixcomp_msg, &MAC_KEY, &INIT_IV);

    // Load fixture if provided
    let fixture_msgs = fixture_path.as_ref().and_then(|p| load_fixture(p));

    println!("========================================");
    println!("  Bench: Replay Pipeline (no network)");
    println!("========================================");
    println!();
    println!("  Iterations:  {}", ITERATIONS);
    println!("  Warmup:      {}", WARMUP);
    if let Some(ref msgs) = fixture_msgs {
        println!("  Fixture:     {} messages loaded", msgs.len());
    } else {
        println!("  Fixture:     synthetic payloads");
    }
    println!();

    // ── Stage 0: Isolated HMAC-SHA1 compute ──
    println!("── Stage 0: Isolated Components ──");
    {
        use hmac::{Hmac, Mac};
        use sha1::Sha1;
        // Pure HMAC-SHA1: key init + update + finalize (no XOR, no sig compare, no alloc)
        let body = &signed_msg[..]; // use full msg as body stand-in
        bench("HMAC-SHA1 compute only (80B body)", ITERATIONS, || {
            let mut mac = Hmac::<Sha1>::new_from_slice(&MAC_KEY).unwrap();
            mac.update(&INIT_IV);
            mac.update(body);
            let _ = mac.finalize();
        });
        // msg.to_vec() — the unavoidable copy
        bench("msg.to_vec() copy (80B)", ITERATIONS, || {
            let _ = signed_msg.to_vec();
        });
        // XOR un-distortion (8 iterations)
        let mut buf = signed_msg.to_vec();
        bench("XOR un-distort (8 positions)", ITERATIONS, || {
            for i in (0..8).rev() {
                let pos = (INIT_IV[i * 2] as usize) % buf.len();
                buf[pos] ^= INIT_IV[i * 2 + 1];
            }
        });
        // find_after_tag_bytes for tag 9= and 35=
        bench("find_after_tag (9= + 35=)", ITERATIONS, || {
            let _ = signed_msg.windows(2).position(|w| w == b"9=");
            let _ = signed_msg.windows(3).position(|w| w == b"35=");
        });
    }
    println!();

    // ── Stage 1: HMAC unsign ──
    println!("── Stage 1: HMAC Unsign ──");
    bench("unsign (5-tick msg, 80B)", ITERATIONS, || {
        let _ = fix_unsign(&signed_msg, &MAC_KEY, &INIT_IV);
    });
    bench("unsign (10-tick msg, 2 tags)", ITERATIONS, || {
        let _ = fix_unsign(&signed_heavy, &MAC_KEY, &INIT_IV);
    });
    bench("unsign FIXCOMP wrapper", ITERATIONS, || {
        let _ = fix_unsign(&signed_fixcomp, &MAC_KEY, &INIT_IV);
    });
    println!();

    // ── Stage 2: FIXCOMP decompress ──
    println!("── Stage 2: FIXCOMP Decompress ──");
    bench("decompress (1 inner msg)", ITERATIONS, || {
        let _ = fixcomp_decompress(&fixcomp_msg);
    });
    {
        // Multi-message FIXCOMP (3 inner messages)
        let inner2 = fix_build(&[(35, "0")], 2);
        let inner3 = fix_build(&[(35, "1"), (112, "farm")], 3);
        let mut combined = fix_tick_msg.clone();
        combined.extend_from_slice(&inner2);
        combined.extend_from_slice(&inner3);
        let multi_comp = fixcomp_build(&combined);
        bench("decompress (3 inner msgs)", ITERATIONS, || {
            let _ = fixcomp_decompress(&multi_comp);
        });
    }
    println!();

    // ── Stage 3: Tick decode (already in bench_decode, included for comparison) ──
    println!("── Stage 3: Tick Decode ──");
    bench("decode_ticks_35p (5 ticks)", ITERATIONS, || {
        let _ = tick_decoder::decode_ticks_35p(&tick_payload);
    });
    bench("decode_ticks_35p (10 ticks, 2 tags)", ITERATIONS, || {
        let _ = tick_decoder::decode_ticks_35p(&tick_payload_heavy);
    });
    println!();

    // ── Stage 4: Decode + state update ──
    println!("── Stage 4: Decode + State Update ──");
    {
        let mut market = MarketState::new();
        let id = market.register(756733);
        market.register_server_tag(1, id);
        market.set_min_tick(id, 0.01);

        bench("decode + state update (5 ticks)", ITERATIONS, || {
            let ticks = tick_decoder::decode_ticks_35p(&tick_payload);
            for tick in &ticks {
                if let Some(inst) = market.instrument_by_server_tag(tick.server_tag) {
                    let mts = market.min_tick_scaled(inst);
                    apply_tick(market.quote_mut(inst), tick, mts);
                }
            }
        });
    }
    {
        let mut market = MarketState::new();
        let id1 = market.register(756733);
        let id2 = market.register(265598);
        market.register_server_tag(1, id1);
        market.register_server_tag(2, id2);
        market.set_min_tick(id1, 0.01);
        market.set_min_tick(id2, 0.01);

        bench("decode + state update (10 ticks, 2 instruments)", ITERATIONS, || {
            let ticks = tick_decoder::decode_ticks_35p(&tick_payload_heavy);
            for tick in &ticks {
                if let Some(inst) = market.instrument_by_server_tag(tick.server_tag) {
                    let mts = market.min_tick_scaled(inst);
                    apply_tick(market.quote_mut(inst), tick, mts);
                }
            }
        });
    }
    println!();

    // ── Stage 5: Full pipeline (sign → unsign → extract → decode → state → notify) ──
    println!("── Stage 5: Full Pipeline ──");
    {
        let mut market = MarketState::new();
        let id = market.register(756733);
        market.register_server_tag(1, id);
        market.set_min_tick(id, 0.01);
        let shared = Arc::new(SharedState::new());
        let (tx, rx) = bounded::<Event>(65536);
        let mut sent = 0u64;

        bench("full: unsign → decode → state → seqlock → channel", ITERATIONS, || {
            // 1. HMAC unsign
            let (unsigned, _new_iv, _valid) = fix_unsign(&signed_msg, &MAC_KEY, &INIT_IV);
            // 2. Extract body after 35=P marker
            if let Some(body) = find_body_after_tag(&unsigned, b"35=P\x01") {
                // 3. Decode ticks
                let ticks = tick_decoder::decode_ticks_35p(body);
                // 4. State update
                for tick in &ticks {
                    if let Some(inst) = market.instrument_by_server_tag(tick.server_tag) {
                        let mts = market.min_tick_scaled(inst);
                        apply_tick(market.quote_mut(inst), tick, mts);
                    }
                }
                // 5. SeqLock + channel notify
                shared.market.push_quote(id, market.quote(id));
                let _ = tx.try_send(Event::Tick(id));
                sent += 1;
                if sent % 60000 == 0 {
                    while rx.try_recv().is_ok() {}
                }
            }
        });
        while rx.try_recv().is_ok() {}
    }
    {
        // Full pipeline with FIXCOMP decompression
        let mut market = MarketState::new();
        let id = market.register(756733);
        market.register_server_tag(1, id);
        market.set_min_tick(id, 0.01);

        bench("full with FIXCOMP: unsign → decompress → decode → state", ITERATIONS / 10, || {
            // 1. HMAC unsign the FIXCOMP wrapper
            let (unsigned, _new_iv, _valid) = fix_unsign(&signed_fixcomp, &MAC_KEY, &INIT_IV);
            // 2. Decompress
            let inner_msgs = fixcomp_decompress(&unsigned);
            // 3. Process each inner message
            for inner in &inner_msgs {
                if let Some(body) = find_body_after_tag(inner, b"35=P\x01") {
                    let ticks = tick_decoder::decode_ticks_35p(body);
                    for tick in &ticks {
                        if let Some(inst) = market.instrument_by_server_tag(tick.server_tag) {
                            let mts = market.min_tick_scaled(inst);
                            apply_tick(market.quote_mut(inst), tick, mts);
                        }
                    }
                }
            }
        });
    }
    println!();

    // ── Stage 6: Order Path (strategy → wire) ──
    println!("── Stage 6: Order Path (strategy → wire) ──");
    {
        // Isolated: format_price + timestamp + field strings
        let price: i64 = 150_25000000; // $150.25 in fixed-point
        bench("format_price", ITERATIONS, || {
            let whole = price / 100_000_000;
            let frac = (price % 100_000_000).unsigned_abs();
            if frac == 0 {
                let _ = whole.to_string();
            } else {
                let frac_str = format!("{:08}", frac);
                let trimmed = frac_str.trim_end_matches('0');
                let _ = format!("{}.{}", whole, trimmed);
            }
        });

        bench("chrono_free_timestamp", ITERATIONS, || {
            let _ = ibx::config::chrono_free_timestamp();
        });

        // Isolated: fix_build (FIX message construction)
        let clord = "12345";
        let account = "DU5479259";
        let now = ibx::config::chrono_free_timestamp();
        let price_str = "150.25";
        let fields: Vec<(u32, &str)> = vec![
            (35, "D"), (52, &now), (11, clord), (1, account),
            (21, "2"), (55, "SPY"), (54, "1"), (38, "100"),
            (40, "2"), (44, price_str), (59, "0"), (60, &now),
            (167, "STK"), (100, "SMART"), (15, "USD"), (204, "0"),
        ];
        bench("fix_build (16-field limit order)", ITERATIONS, || {
            let _ = fix_build(&fields, 42);
        });

        // Isolated: fix_sign
        let order_msg = fix_build(&fields, 42);
        bench("fix_sign (HMAC sign)", ITERATIONS, || {
            let _ = fix_sign(&order_msg, &MAC_KEY, &INIT_IV);
        });

        // Full limit order: timestamp + build + sign
        bench("full: limit order (16 fields) build+sign", ITERATIONS, || {
            let now = ibx::config::chrono_free_timestamp();
            let fields: Vec<(u32, &str)> = vec![
                (35, "D"), (52, &now), (11, "12345"), (1, "DU5479259"),
                (21, "2"), (55, "SPY"), (54, "1"), (38, "100"),
                (40, "2"), (44, "150.25"), (59, "0"), (60, &now),
                (167, "STK"), (100, "SMART"), (15, "USD"), (204, "0"),
            ];
            let msg = fix_build(&fields, 42);
            let _ = fix_sign(&msg, &MAC_KEY, &INIT_IV);
        });

        // Full market order (fewer fields, no price)
        bench("full: market order (14 fields) build+sign", ITERATIONS, || {
            let now = ibx::config::chrono_free_timestamp();
            let fields: Vec<(u32, &str)> = vec![
                (35, "D"), (52, &now), (11, "12345"), (1, "DU5479259"),
                (21, "2"), (55, "SPY"), (54, "1"), (38, "1"),
                (40, "1"), (59, "0"), (60, &now),
                (167, "STK"), (100, "SMART"), (15, "USD"),
            ];
            let msg = fix_build(&fields, 42);
            let _ = fix_sign(&msg, &MAC_KEY, &INIT_IV);
        });

        // Cancel order (5 fields, lightweight)
        bench("full: cancel order (5 fields) build+sign", ITERATIONS, || {
            let now = ibx::config::chrono_free_timestamp();
            let fields: Vec<(u32, &str)> = vec![
                (35, "F"), (52, &now), (11, "C12345"), (41, "12345"), (60, &now),
            ];
            let msg = fix_build(&fields, 42);
            let _ = fix_sign(&msg, &MAC_KEY, &INIT_IV);
        });

        // Modify order (16+ fields, same as limit but replace msg type)
        bench("full: modify order (16 fields) build+sign", ITERATIONS, || {
            let now = ibx::config::chrono_free_timestamp();
            let fields: Vec<(u32, &str)> = vec![
                (35, "G"), (52, &now), (11, "12346"), (41, "12345"),
                (1, "DU5479259"), (21, "2"), (55, "SPY"), (54, "1"),
                (38, "100"), (40, "2"), (44, "151.00"), (59, "0"), (60, &now),
                (167, "STK"), (100, "SMART"), (15, "USD"), (204, "0"),
            ];
            let msg = fix_build(&fields, 42);
            let _ = fix_sign(&msg, &MAC_KEY, &INIT_IV);
        });

        // Channel enqueue (SPSC crossbeam)
        let (order_tx, order_rx) = bounded::<u64>(65536);
        let mut enqueued = 0u64;
        bench("channel enqueue (crossbeam bounded)", ITERATIONS, || {
            let _ = order_tx.try_send(enqueued);
            enqueued += 1;
            if enqueued % 60000 == 0 {
                while order_rx.try_recv().is_ok() {}
            }
        });
        while order_rx.try_recv().is_ok() {}
    }
    println!();

    // ── Stage 7: Fixture replay (if available) ──
    if let Some(ref msgs) = fixture_msgs {
        println!("── Stage 6: Fixture Replay ──");
        let n = msgs.len();
        let iters = ITERATIONS.min(n as u64 * 1000); // scale to fixture size

        let mut market = MarketState::new();
        let id = market.register(756733);
        market.register_server_tag(1, id);
        market.set_min_tick(id, 0.01);

        let mut idx = 0usize;
        bench(&format!("fixture replay ({} msgs, cycled)", n), iters, || {
            let msg = &msgs[idx % n];
            // Decode body directly (fixture messages are already unsigned)
            if let Some(body) = find_body_after_tag(msg, b"35=P\x01") {
                let ticks = tick_decoder::decode_ticks_35p(body);
                for tick in &ticks {
                    if let Some(inst) = market.instrument_by_server_tag(tick.server_tag) {
                        let mts = market.min_tick_scaled(inst);
                        apply_tick(market.quote_mut(inst), tick, mts);
                    }
                }
            }
            idx += 1;
        });
        println!();
    }

    // ── Summary comparison table ──
    println!("── Summary ──");
    println!("  Pipeline stage breakdown (5-tick typical message):");
    println!();
    let stages = [
        ("HMAC unsign", &signed_msg as &[u8], Stage::Unsign),
        ("Body extract + decode", &tick_payload, Stage::Decode),
        ("State update (5 fields)", &tick_payload, Stage::StateUpdate),
    ];

    let mut market = MarketState::new();
    let id = market.register(756733);
    market.register_server_tag(1, id);
    market.set_min_tick(id, 0.01);

    for (label, data, stage) in &stages {
        let ns = measure_stage(*stage, data, &mut market, id);
        println!("    {:<45} {:>6} ns", label, ns);
    }

    // Measure full pipeline
    let full_ns = measure_full_pipeline(&signed_msg, &mut market, id);
    println!("    {:<45} {:>6} ns", "TOTAL (full pipeline)", full_ns);
    println!();
    println!("  Compare these numbers with the Java gateway benchmark");
    println!("  (ib-agent issue: gateway-side replay benchmark).");
}

// ── Stage measurement ──

#[derive(Copy, Clone)]
enum Stage { Unsign, Decode, StateUpdate }

fn measure_stage(stage: Stage, data: &[u8], market: &mut MarketState, _id: u32) -> u64 {
    // Warmup
    for _ in 0..WARMUP {
        match stage {
            Stage::Unsign => { let _ = fix_unsign(data, &MAC_KEY, &INIT_IV); }
            Stage::Decode => { let _ = tick_decoder::decode_ticks_35p(data); }
            Stage::StateUpdate => {
                let ticks = tick_decoder::decode_ticks_35p(data);
                for tick in &ticks {
                    if let Some(inst) = market.instrument_by_server_tag(tick.server_tag) {
                        let mts = market.min_tick_scaled(inst);
                        apply_tick(market.quote_mut(inst), tick, mts);
                    }
                }
            }
        }
    }
    let start = Instant::now();
    for _ in 0..ITERATIONS {
        match stage {
            Stage::Unsign => { let _ = fix_unsign(data, &MAC_KEY, &INIT_IV); }
            Stage::Decode => { let _ = tick_decoder::decode_ticks_35p(data); }
            Stage::StateUpdate => {
                let ticks = tick_decoder::decode_ticks_35p(data);
                for tick in &ticks {
                    if let Some(inst) = market.instrument_by_server_tag(tick.server_tag) {
                        let mts = market.min_tick_scaled(inst);
                        apply_tick(market.quote_mut(inst), tick, mts);
                    }
                }
            }
        }
    }
    start.elapsed().as_nanos() as u64 / ITERATIONS
}

fn measure_full_pipeline(signed: &[u8], market: &mut MarketState, id: u32) -> u64 {
    for _ in 0..WARMUP {
        run_full_pipeline(signed, market, id);
    }
    let start = Instant::now();
    for _ in 0..ITERATIONS {
        run_full_pipeline(signed, market, id);
    }
    start.elapsed().as_nanos() as u64 / ITERATIONS
}

#[inline]
fn run_full_pipeline(signed: &[u8], market: &mut MarketState, _id: u32) {
    let (unsigned, _iv, _valid) = fix_unsign(signed, &MAC_KEY, &INIT_IV);
    if let Some(body) = find_body_after_tag(&unsigned, b"35=P\x01") {
        let ticks = tick_decoder::decode_ticks_35p(body);
        for tick in &ticks {
            if let Some(inst) = market.instrument_by_server_tag(tick.server_tag) {
                let mts = market.min_tick_scaled(inst);
                apply_tick(market.quote_mut(inst), tick, mts);
            }
        }
    }
}

// ── Fixture loader ──

fn load_fixture(path: &str) -> Option<Vec<Vec<u8>>> {
    use std::io::BufRead;

    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    let mut messages = Vec::new();

    for line in reader.lines() {
        let line = line.ok()?;
        if line.is_empty() { continue; }

        // Parse JSONL: look for raw_b64 field and hook=SIGN_IN6
        // Simple extraction without a JSON dependency
        if !line.contains("\"SIGN_IN6\"") { continue; }
        if !line.contains("35=P") && !line.contains("35\\u003dP") { continue; }

        if let Some(b64) = extract_json_string(&line, "raw_b64") {
            use base64::Engine;
            if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(&b64) {
                messages.push(bytes);
            }
        }
    }

    if messages.is_empty() { None } else { Some(messages) }
}

fn extract_json_string<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("\"{}\":\"", key);
    let start = json.find(&needle)? + needle.len();
    let end = json[start..].find('"')? + start;
    Some(&json[start..end])
}

// ── Payload builders (shared with bench_decode) ──

fn build_35p_payload(server_tag: u32, ticks: &[(u64, u64, u64, bool)]) -> Vec<u8> {
    let mut bits: Vec<u8> = Vec::new();
    push_bits(&mut bits, 0, 1);
    push_bits(&mut bits, server_tag as u64, 31);
    for (i, &(tick_type, width, value, negative)) in ticks.iter().enumerate() {
        let has_more = if i < ticks.len() - 1 { 1 } else { 0 };
        push_bits(&mut bits, tick_type, 5);
        push_bits(&mut bits, has_more, 1);
        push_bits(&mut bits, width - 1, 2);
        push_bits(&mut bits, if negative { 1 } else { 0 }, 1);
        push_bits(&mut bits, value, (width * 8 - 1) as usize);
    }
    finalize_payload(&bits)
}

fn build_35p_payload_multi(tags: &[(u32, &[(u64, u64, u64, bool)])]) -> Vec<u8> {
    let mut bits: Vec<u8> = Vec::new();
    for (tag_idx, &(server_tag, ticks)) in tags.iter().enumerate() {
        let cont = if tag_idx > 0 { 1 } else { 0 };
        push_bits(&mut bits, cont, 1);
        push_bits(&mut bits, server_tag as u64, 31);
        for (i, &(tick_type, width, value, negative)) in ticks.iter().enumerate() {
            let has_more = if i < ticks.len() - 1 { 1 } else { 0 };
            push_bits(&mut bits, tick_type, 5);
            push_bits(&mut bits, has_more, 1);
            push_bits(&mut bits, width - 1, 2);
            push_bits(&mut bits, if negative { 1 } else { 0 }, 1);
            push_bits(&mut bits, value, (width * 8 - 1) as usize);
        }
    }
    finalize_payload(&bits)
}

fn push_bits(bits: &mut Vec<u8>, val: u64, n: usize) {
    for i in (0..n).rev() {
        bits.push(((val >> i) & 1) as u8);
    }
}

fn finalize_payload(bits: &[u8]) -> Vec<u8> {
    let bit_count = bits.len();
    let byte_count = (bit_count + 7) / 8;
    let mut payload = vec![0u8; byte_count];
    for (i, &b) in bits.iter().enumerate() {
        if b == 1 {
            payload[i >> 3] |= 1 << (7 - (i & 7));
        }
    }
    let mut body = Vec::with_capacity(2 + byte_count);
    body.push((bit_count >> 8) as u8);
    body.push((bit_count & 0xFF) as u8);
    body.extend_from_slice(&payload);
    body
}

fn build_fix_tick_message(tick_payload: &[u8]) -> Vec<u8> {
    // Build: 8=O\x01 9=<len>\x01 35=P\x01 <tick_payload>
    let body = format!("35=P\x01");
    let body_bytes = body.as_bytes();
    let body_len = body_bytes.len() + tick_payload.len();
    let header = format!("8=O\x019={:04}\x01", body_len);
    let mut msg = header.into_bytes();
    msg.extend_from_slice(body_bytes);
    msg.extend_from_slice(tick_payload);
    msg
}

fn find_body_after_tag<'a>(msg: &'a [u8], tag_marker: &[u8]) -> Option<&'a [u8]> {
    msg.windows(tag_marker.len())
        .position(|w| w == tag_marker)
        .map(|pos| &msg[pos + tag_marker.len()..])
}

#[inline]
fn apply_tick(q: &mut Quote, tick: &RawTick, min_tick_scaled: i64) {
    match tick.tick_type {
        tick_decoder::O_BID_PRICE => q.bid = tick.magnitude * min_tick_scaled,
        tick_decoder::O_ASK_PRICE => q.ask = tick.magnitude * min_tick_scaled,
        tick_decoder::O_LAST_PRICE => q.last = tick.magnitude * min_tick_scaled,
        tick_decoder::O_HIGH_PRICE => q.high = tick.magnitude * min_tick_scaled,
        tick_decoder::O_LOW_PRICE => q.low = tick.magnitude * min_tick_scaled,
        tick_decoder::O_OPEN_PRICE => q.open = tick.magnitude * min_tick_scaled,
        tick_decoder::O_CLOSE_PRICE => q.close = tick.magnitude * min_tick_scaled,
        tick_decoder::O_BID_SIZE => q.bid_size = tick.magnitude,
        tick_decoder::O_ASK_SIZE => q.ask_size = tick.magnitude,
        tick_decoder::O_LAST_SIZE => q.last_size = tick.magnitude,
        tick_decoder::O_VOLUME => q.volume = tick.magnitude,
        tick_decoder::O_TIMESTAMP | tick_decoder::O_LAST_TS => {
            q.timestamp_ns = tick.magnitude as u64;
        }
        _ => {}
    }
}

fn bench(label: &str, iterations: u64, mut f: impl FnMut()) {
    for _ in 0..WARMUP { f(); }
    let start = Instant::now();
    for _ in 0..iterations { f(); }
    let elapsed = start.elapsed();
    let per_iter_ns = elapsed.as_nanos() as u64 / iterations;
    println!(
        "  {:<55} {:>6} ns/iter  ({:.2}s total)",
        label, per_iter_ns, elapsed.as_secs_f64(),
    );
}
