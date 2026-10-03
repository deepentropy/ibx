//! Robustness tests on network input and callbacks (ibx#488, tests layer
//! D). No gateway reference is needed: no input from the network may
//! panic, block the hot loop or corrupt the state. Bad input gives an error
//! value and the session goes on, as the gateway drops a bad block and
//! continues (`jmdclient.bl.a(...)@5249-5332`, log "Exception top3").
//!
//! - `decoders`: property tests, any bytes into each decoder: no panic,
//!   allocation bounded by the input.
//! - `mutation`: the recorded server frames of the fixtures flipped, cut,
//!   extended and repeated, into the decoders and into the engine.
//!
//! The case counts are small so the normal test run stays fast; set
//! `PROPTEST_CASES` (and `IBX_MUTATIONS` for the mutation tests) to run
//! longer by hand. The fuzz targets of `fuzz/` take the same decoders.

mod decoders;
mod mutation;

use std::cell::Cell;
use std::sync::OnceLock;

use base64::Engine as _;
use rand::{Rng, RngCore};

/// Cases of a property test: `PROPTEST_CASES` when set, else `default`.
pub(crate) fn cases(default: u32) -> u32 {
    std::env::var("PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// The proptest settings of these tests: `cases` cases, no file of
/// failures (a failure prints its input). The seed is fixed so that a run
/// in CI gives the same inputs each time; `PROPTEST_RNG_SEED` sets another
/// one for a longer run by hand.
pub(crate) fn config(default_cases: u32) -> proptest::test_runner::Config {
    let mut config = proptest::test_runner::Config {
        cases: cases(default_cases),
        failure_persistence: None,
        ..Default::default()
    };
    if std::env::var_os("PROPTEST_RNG_SEED").is_none() {
        config.rng_seed = proptest::test_runner::RngSeed::Fixed(488);
    }
    config
}

// ── Logging ──
//
// The decoders log what they drop, some at debug and trace level with
// parts of the input in the message. Those arguments run only when the
// level is on, so the tests turn every level on: an argument that panics
// on bad input is found too.

thread_local! {
    /// The records of this thread are formatted (robustness tests only).
    static FORMAT_LOGS: Cell<bool> = const { Cell::new(false) };
}

struct SinkLogger;

impl log::Log for SinkLogger {
    fn enabled(&self, _: &log::Metadata) -> bool {
        FORMAT_LOGS.with(Cell::get)
    }

    fn log(&self, record: &log::Record) {
        if FORMAT_LOGS.with(Cell::get) {
            use std::fmt::Write as _;
            let mut sink = String::new();
            let _ = write!(sink, "{}", record.args());
        }
    }

    fn flush(&self) {}
}

/// Every log level on, the records of this thread formatted and dropped.
pub(crate) fn all_log_levels() {
    static LOGGER: SinkLogger = SinkLogger;
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        if log::set_logger(&LOGGER).is_ok() {
            log::set_max_level(log::LevelFilter::Trace);
        }
    });
    FORMAT_LOGS.with(|f| f.set(true));
}

// ── Allocation ──
//
// The test binary counts the bytes each thread holds, so a test can see
// the most a call held at once: a decoder that allocates by a length read
// from the input, not by the input it has, is found.

thread_local! {
    static LIVE: Cell<isize> = const { Cell::new(0) };
    static PEAK: Cell<isize> = const { Cell::new(0) };
}

struct CountingAlloc;

#[inline]
fn note(delta: isize) {
    let _ = LIVE.try_with(|live| {
        let now = live.get() + delta;
        live.set(now);
        let _ = PEAK.try_with(|peak| if now > peak.get() { peak.set(now) });
    });
}

unsafe impl std::alloc::GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        let p = unsafe { std::alloc::System.alloc(layout) };
        if !p.is_null() {
            note(layout.size() as isize);
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: std::alloc::Layout) -> *mut u8 {
        let p = unsafe { std::alloc::System.alloc_zeroed(layout) };
        if !p.is_null() {
            note(layout.size() as isize);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        unsafe { std::alloc::System.dealloc(ptr, layout) };
        note(-(layout.size() as isize));
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: std::alloc::Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { std::alloc::System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            note(new_size as isize - layout.size() as isize);
        }
        p
    }
}

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

/// Run `f` and give the most bytes this thread held at once above what it
/// held before.
pub(crate) fn peak_alloc<R>(f: impl FnOnce() -> R) -> (R, usize) {
    let start = LIVE.with(Cell::get);
    PEAK.with(|p| p.set(start));
    let out = f();
    let peak = PEAK.with(Cell::get);
    (out, (peak - start).max(0) as usize)
}

/// The allocation a decoder may make for an input of `len` bytes: `per_byte`
/// bytes per input byte, plus 1 MiB.
pub(crate) fn alloc_bound(len: usize, per_byte: usize) -> usize {
    len * per_byte + (1 << 20)
}

// ── Recorded server frames ──

/// A frame the gateway received from a server, as recorded, or one message
/// of a recorded compressed frame (`inner`).
#[derive(Debug, Clone)]
pub(crate) struct Sample {
    /// The fixture file (relative to tests/fixtures/gw1040/).
    pub file: String,
    /// The link: `CCP`, `usfarm`, `ushmds`, ...
    pub conn: String,
    /// The message type (tag 35), empty for an NS or XYZ frame.
    pub msg_type: String,
    pub raw: Vec<u8>,
    pub inner: bool,
}

impl Sample {
    pub fn is(&self, msg_type: &str) -> bool {
        self.msg_type == msg_type
    }
}

/// The server frames of every codec and scenario fixture
/// (tests/fixtures/gw1040/), and the messages of their compressed frames.
pub(crate) fn server_frames() -> &'static [Sample] {
    static FRAMES: OnceLock<Vec<Sample>> = OnceLock::new();
    FRAMES.get_or_init(|| {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/gw1040");
        let mut files = Vec::new();
        let mut dirs = vec![root.join("codec"), root.join("scenarios")];
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(&dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    dirs.push(path);
                } else if path.extension().is_some_and(|e| e == "jsonl") {
                    files.push(path);
                }
            }
        }
        files.sort();
        let mut out = Vec::new();
        for path in files {
            let file = path.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/");
            let text = std::fs::read_to_string(&path).unwrap();
            for line in text.lines().skip(1) {
                let v: serde_json::Value = serde_json::from_str(line).unwrap();
                if v["leg"] != "fix_in" {
                    continue;
                }
                let Some(b64) = v["raw_b64"].as_str() else { continue };
                let raw = base64::engine::general_purpose::STANDARD.decode(b64).unwrap();
                let conn = v["conn"].as_str().unwrap_or("").to_string();
                if raw.starts_with(b"8=FIXCOMP\x01") {
                    for m in crate::protocol::fixcomp::fixcomp_decompress(&raw).unwrap_or_default() {
                        out.push(Sample { file: file.clone(), conn: conn.clone(), msg_type: msg_type(&m), raw: m, inner: true });
                    }
                }
                out.push(Sample { file: file.clone(), conn, msg_type: msg_type(&raw), raw, inner: false });
            }
        }
        assert!(out.len() > 2000, "server frames: {}", out.len());
        out
    })
}

/// The value of tag 35 of a frame.
pub(crate) fn msg_type(raw: &[u8]) -> String {
    crate::engine::hot_loop::fast_extract_msg_type(raw)
        .map(|t| String::from_utf8_lossy(t).into_owned())
        .unwrap_or_default()
}

/// The recorded frames of a message type (`""` for the NS and XYZ frames
/// and the compressed frames).
pub(crate) fn frames_of(msg_type: &str) -> Vec<&'static [u8]> {
    server_frames().iter().filter(|s| s.is(msg_type)).map(|s| s.raw.as_slice()).collect()
}

// ── Mutations ──

/// Numbers put in place of a run of digits: the edges of the integer
/// types and texts that are no number.
const NUMBERS: &[&str] = &[
    "0", "-1", "-0", "", "2147483647", "2147483648", "-2147483649", "4294967295", "4294967296",
    "9223372036854775807", "9223372036854775808", "18446744073709551615", "18446744073709551616",
    "99999999999999999999999999", "1e309", "NaN", "0x10", "+5", "1.5", " 7",
];

/// Bytes the frames give a meaning to.
const MARKS: &[u8] = b"\x00\x01\x7f\x80\xff=;|,.<>/&\"'{}#%8 \r\n";

/// A copy of `raw` with one to four changes: a byte flipped or set to a
/// mark, a cut, a range removed, repeated or moved, bytes inserted or
/// appended, a number replaced by an edge value, the frame doubled.
pub(crate) fn mutate(rng: &mut impl RngCore, raw: &[u8]) -> Vec<u8> {
    let mut out = raw.to_vec();
    let changes = rng.random_range(1..=4);
    for _ in 0..changes {
        let len = out.len();
        let at = |rng: &mut dyn RngCore, n: usize| if n == 0 { 0 } else { rng.random_range(0..n) };
        match rng.random_range(0..10) {
            0 if len > 0 => {
                let i = at(rng, len);
                out[i] ^= 1 << rng.random_range(0..8);
            }
            1 if len > 0 => {
                let i = at(rng, len);
                out[i] = MARKS[at(rng, MARKS.len())];
            }
            2 => out.truncate(at(rng, len + 1)),
            3 if len > 0 => {
                let a = at(rng, len);
                let b = (a + 1 + at(rng, 16)).min(len);
                out.drain(a..b);
            }
            4 if len > 0 => {
                let a = at(rng, len);
                let b = (a + 1 + at(rng, 64)).min(len);
                let copy = out[a..b].to_vec();
                let times = rng.random_range(1..=8);
                for _ in 0..times {
                    out.splice(b..b, copy.iter().copied());
                }
            }
            5 => {
                let i = at(rng, len + 1);
                let n = rng.random_range(1..=16);
                let bytes: Vec<u8> = (0..n).map(|_| rng.random()).collect();
                out.splice(i..i, bytes);
            }
            6 => {
                let n = rng.random_range(1..=64);
                out.extend((0..n).map(|_| rng.random::<u8>()));
            }
            7 => {
                // A run of digits (a length, a count, a price) replaced.
                let starts: Vec<usize> = (0..len)
                    .filter(|&i| out[i].is_ascii_digit() && (i == 0 || !out[i - 1].is_ascii_digit()))
                    .collect();
                if !starts.is_empty() {
                    let a = starts[at(rng, starts.len())];
                    let b = a + out[a..].iter().take_while(|c| c.is_ascii_digit()).count();
                    let number = NUMBERS[at(rng, NUMBERS.len())];
                    out.splice(a..b, number.bytes());
                }
            }
            8 if len > 0 => {
                // A range moved elsewhere.
                let a = at(rng, len);
                let b = (a + 1 + at(rng, 32)).min(len);
                let moved: Vec<u8> = out.drain(a..b).collect();
                let i = at(rng, out.len() + 1);
                out.splice(i..i, moved);
            }
            _ => out.extend_from_slice(raw),
        }
    }
    out
}

/// Input bytes of a property test; a failure shows them as frame text.
#[derive(Clone)]
pub(crate) struct Wire(pub Vec<u8>);

impl std::fmt::Debug for Wire {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} bytes: \"{}\"", self.0.len(), show(&self.0))
    }
}

impl std::ops::Deref for Wire {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.0
    }
}

/// A proptest strategy: one of `samples`, mutated by a seeded generator.
pub(crate) fn mutated(samples: Vec<&'static [u8]>) -> impl proptest::strategy::Strategy<Value = Wire> {
    use proptest::prelude::*;
    assert!(!samples.is_empty());
    (0..samples.len(), any::<u64>(), 0..4u8).prop_map(move |(i, seed, rounds)| {
        let mut rng = <rand::rngs::StdRng as rand::SeedableRng>::seed_from_u64(seed);
        let mut out = samples[i].to_vec();
        for _ in 0..=rounds {
            out = mutate(&mut rng, &out);
        }
        Wire(out)
    })
}

/// Any bytes, up to `max` long.
pub(crate) fn bytes(max: usize) -> impl proptest::strategy::Strategy<Value = Wire> {
    use proptest::strategy::Strategy;
    proptest::collection::vec(proptest::prelude::any::<u8>(), 0..max).prop_map(Wire)
}

/// A frame as text for a failure message: `|` for the separator, other
/// unprintable bytes escaped.
pub(crate) fn show(raw: &[u8]) -> String {
    crate::protocol::fix::fmt_pipe(raw)
}
