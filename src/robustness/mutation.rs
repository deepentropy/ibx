//! Mutations of the recorded server frames (ibx#488): each frame of the
//! codec and scenario fixtures is flipped, cut, extended, repeated, its
//! numbers set to edge values, then given to the decoders and to the
//! engine. Nothing may panic, and the session goes on: the links still
//! answer a test request after the bad input, and the replays of the
//! golden tests run to their end with bad frames in between.
//!
//! `IBX_MUTATIONS` sets the mutations per frame and `IBX_MUTATION_SEED`
//! the seed, for a longer run by hand.

use std::panic::{catch_unwind, AssertUnwindSafe};

use rand::rngs::StdRng;
use rand::SeedableRng;

use crate::test_support::decoders::{DecoderGroup, DECODERS};
use super::{all_log_levels, alloc_bound, mutate, peak_alloc, server_frames, show, Sample};
use crate::test_support::scenario::runner::{Link, RustDriver};
use crate::test_support::scenario::session::{Links, Session, BAD_COPIES};
use crate::test_support::parse_fields;

/// Mutations of each frame: `IBX_MUTATIONS` when set, else `default`.
fn mutations(default: usize) -> usize {
    std::env::var("IBX_MUTATIONS").ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn rng() -> StdRng {
    let seed = std::env::var("IBX_MUTATION_SEED").ok().and_then(|v| v.parse().ok()).unwrap_or(488);
    StdRng::seed_from_u64(seed)
}

fn panic_text(e: Box<dyn std::any::Any + Send>) -> String {
    e.downcast_ref::<String>().cloned().or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default()
}

/// The decoder groups a frame of this type reaches; every group for one
/// mutation in eight.
fn groups_for(s: &Sample, all: bool) -> Vec<&'static DecoderGroup> {
    let mut names = vec!["fix", "connection"];
    let comp = s.raw.starts_with(b"8=FIXCOMP");
    names.extend(match s.msg_type.as_str() {
        _ if comp => &["fixcomp", "logon", "hmds_binary"][..],
        "" => &["ns", "xyz", "key_exchange"][..],
        "P" => &["ticks"][..],
        "Y" | "Z" => &["depth"][..],
        "G" => &["generic"][..],
        "E" => &["tick_by_tick"][..],
        "W" => &["hmds_xml", "hmds_binary"][..],
        "d" => &["contracts"][..],
        "U" if s.conn == "CCP" => &["auth", "contracts", "hmds_xml"][..],
        "U" => &["hmds_xml", "hmds_binary"][..],
        "8" => &["auth"][..],
        _ => &["farm_text", "logon"][..],
    });
    DECODERS.iter().filter(|(name, ..)| all || names.contains(name)).collect()
}

#[test]
fn recorded_frames_mutated_through_the_decoders() {
    all_log_levels();
    let mut rng = rng();
    let mut runs = 0usize;
    for (k, s) in server_frames().iter().enumerate() {
        for i in 0..mutations(2) {
            let m = mutate(&mut rng, &s.raw);
            for (name, decode, per_byte) in groups_for(s, (k + i) % 8 == 0) {
                let (result, peak) = peak_alloc(|| catch_unwind(AssertUnwindSafe(|| decode(&m))));
                if let Err(e) = result {
                    panic!("decoder {name} panicked ({}) on a mutation of a {} frame of {}: {}",
                        panic_text(e), s.msg_type, s.file, show(&m));
                }
                assert!(peak <= alloc_bound(m.len(), *per_byte),
                    "decoder {name} held {peak} bytes for {} input bytes: {}", m.len(), show(&m));
                runs += 1;
            }
        }
    }
    assert!(runs > 10_000, "decoder runs: {runs}");
}

/// The link of the engine that reads a frame of `conn`.
fn link_of(s: &Sample) -> Link {
    match s.conn.as_str() {
        "CCP" => Link::Ccp,
        "ushmds" | "cashfa" => Link::Hmds,
        _ => Link::Farm,
    }
}

/// A test request on each link: the engine answers each with its
/// heartbeat, so the session went on.
fn assert_links_answer(s: &mut Links, probe: &str, context: &str) {
    s.farm.send_fix(&[(35, "1"), (112, probe)]);
    s.ccp.send_fix(&[(35, "1"), (112, probe)]);
    s.hmds.send_fix(&[(35, "1"), (112, probe)]);
    for _ in 0..4 {
        s.engine.step_for_test();
    }
    let answered = |msgs: Vec<Vec<u8>>| msgs.iter().any(|m| {
        let f = parse_fields(m);
        f.contains(&(35, "0".to_string())) && f.contains(&(112, probe.to_string()))
    });
    assert!(answered(s.farm.messages()), "the farm link gives no heartbeat after {context}");
    assert!(answered(s.ccp.messages()), "the auth link gives no heartbeat after {context}");
    assert!(answered(s.hmds.messages()), "the historical link gives no heartbeat after {context}");
}

/// Each fixture in its order, through the engine: mutated copies of every
/// frame to the three message handlers, the frame itself on its link, the
/// API callbacks taken now and then. The links answer a test request at
/// the end of each fixture.
#[test]
fn recorded_frames_mutated_through_the_engine() {
    all_log_levels();
    let mut rng = rng();
    let frames = server_frames();
    let mut files: Vec<&str> = frames.iter().map(|s| s.file.as_str()).collect();
    files.dedup();
    let mut given = 0usize;
    for (n, file) in files.iter().enumerate() {
        let mut s = Session::new();
        for (k, sample) in frames.iter().filter(|f| f.file == *file).enumerate() {
            for _ in 0..mutations(2) {
                let m = mutate(&mut rng, &sample.raw);
                let msgs = if m.starts_with(b"8=FIXCOMP\x01") {
                    crate::protocol::fixcomp::fixcomp_decompress(&m).unwrap_or_default()
                } else {
                    vec![m]
                };
                for m in msgs {
                    for link in [Link::Farm, Link::Ccp, Link::Hmds] {
                        let engine = &mut s.engine;
                        let run = catch_unwind(AssertUnwindSafe(|| match link {
                            Link::Farm => engine.inject_farm_message(&m),
                            Link::Ccp => engine.inject_ccp_message(&m),
                            Link::Hmds => engine.inject_hmds_message(&m),
                        }));
                        if let Err(e) = run {
                            panic!("{link:?} handler panicked ({}) on a mutation of a {} frame of {file}: {}",
                                panic_text(e), sample.msg_type, show(&m));
                        }
                        given += 1;
                    }
                }
            }
            if !sample.inner {
                // The recorded text frames of the farms have no checksum
                // trailer: it is put back for the wire.
                let raw = if sample.raw.starts_with(b"8=FIX.") && !sample.raw.windows(4).any(|w| w == b"\x0110=") {
                    crate::test_support::scenario::record::rebuild_text(&parse_fields(&sample.raw))
                } else {
                    sample.raw.clone()
                };
                match link_of(sample) {
                    Link::Farm => s.farm.send_raw(&raw),
                    Link::Ccp => s.ccp.send_raw(&raw),
                    Link::Hmds => s.hmds.send_raw(&raw),
                }
                s.engine.step_for_test();
            }
            if k % 16 == 0 {
                s.settle();
            }
        }
        s.settle();
        assert_links_answer(&mut s.links, &format!("probe{n}"), file);
    }
    assert!(given > 10_000, "messages given: {given}");
}

/// The scenario replays of ibx#486 and ibx#487 (every codec fixture and
/// every recorded scenario) with bad copies of each server frame given to
/// the engine right before it: they run to their end, and the links answer
/// a test request.
#[test]
fn replays_run_on_with_bad_frames_in_between() {
    use crate::test_support::scenario::{load_path, run, Options};

    /// Clears the hook, also when a replay panics.
    struct Hook;
    impl Drop for Hook {
        fn drop(&mut self) {
            BAD_COPIES.with(|h| *h.borrow_mut() = None);
        }
    }

    all_log_levels();
    let mut rng = rng();
    let per_frame = mutations(2);
    BAD_COPIES.with(|h| *h.borrow_mut() = Some(Box::new(move |raw: &[u8]| {
        (0..per_frame).map(|_| mutate(&mut rng, raw)).collect()
    })));
    let _hook = Hook;

    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/gw1040");
    let mut files = Vec::new();
    let mut dirs = vec![root.join("codec"), root.join("scenarios")];
    while let Some(dir) = dirs.pop() {
        for path in std::fs::read_dir(dir).unwrap().flatten().map(|e| e.path()) {
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().is_some_and(|e| e == "jsonl") && !path.to_string_lossy().ends_with(".api.jsonl") {
                files.push(path);
            }
        }
    }
    files.sort();
    assert!(files.len() > 40, "fixtures: {}", files.len());
    for path in files {
        let name = path.strip_prefix(&root).unwrap().display().to_string();
        let mut links = Links::new();
        let mut driver = RustDriver::new(&links);
        let _ = run(&load_path(&path), &Options::default(), &mut links, &mut driver);
        assert_links_answer(&mut links, "probe", &name);
    }
}
