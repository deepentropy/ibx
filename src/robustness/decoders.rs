//! Property tests of the decoders of network input (ibx#488): any bytes,
//! or a recorded frame of the fixtures mutated, give a value or an error,
//! never a panic, and the decoder holds at most a bounded multiple of the
//! input in memory. A failure prints the input.
//!
//! The decoders are taken in the groups of `test_support::decoders`, which
//! the mutation tests and the fuzz targets use too.

use proptest::prelude::*;

use super::{all_log_levels, alloc_bound, bytes, config, frames_of, mutated, peak_alloc, server_frames, show, Sample, Wire};
use crate::protocol::fix;
use crate::test_support::decoders::*;

/// Run a decoder on `input`, with every log level on: it returns (a panic
/// fails the case) and holds at most `per_byte` bytes per input byte.
pub(super) fn decode<R>(input: &[u8], per_byte: usize, f: impl FnOnce(&[u8]) -> R) -> Result<R, TestCaseError> {
    all_log_levels();
    let (out, peak) = peak_alloc(|| f(input));
    prop_assert!(
        peak <= alloc_bound(input.len(), per_byte),
        "{} bytes held for {} input bytes: {}", peak, input.len(), show(&input[..input.len().min(200)]),
    );
    Ok(out)
}

/// The frames whose samples `keep` selects.
fn samples(keep: impl Fn(&Sample) -> bool) -> Vec<&'static [u8]> {
    let out: Vec<&'static [u8]> = server_frames().iter().filter(|s| keep(s)).map(|s| s.raw.as_slice()).collect();
    assert!(!out.is_empty(), "no recorded frame for this decoder");
    out
}

/// Any bytes, or a recorded frame of `msg_type` mutated.
fn input(msg_type: &str) -> impl Strategy<Value = Wire> {
    let frames = frames_of(msg_type);
    prop_oneof![bytes(1024), mutated(frames)]
}

/// The tag 6118 texts (XML replies) of the recorded messages.
fn xml_samples() -> Vec<&'static [u8]> {
    static XML: std::sync::OnceLock<Vec<Vec<u8>>> = std::sync::OnceLock::new();
    XML.get_or_init(|| {
        server_frames().iter()
            .filter_map(|s| fix::fix_parse(&s.raw).remove(&6118))
            .map(String::into_bytes)
            .collect()
    }).iter().map(Vec::as_slice).collect()
}

/// A message of the auth link with a sub-protocol (6040) of `comms`.
fn ccp_u(comms: &'static [&'static str]) -> impl Strategy<Value = Wire> {
    let frames = samples(|s| s.is("U") && fix::fix_parse(&s.raw).get(&6040).is_some_and(|c| comms.contains(&c.as_str())));
    prop_oneof![bytes(1024), mutated(frames)]
}

// ── Property tests ──

proptest! {
    #![proptest_config(config(256))]

    #[test]
    fn ns_frames(raw in prop_oneof![bytes(256), mutated(frames_of(""))]) {
        decode(&raw, PER_BYTE, ns)?;
    }

    /// The text payload of an NS frame.
    #[test]
    fn ns_payloads(text in "(MISC)?[0-9]{0,3};[0-9]{0,4};[^;]{0,12}(;[^;]{0,12}){0,6};?") {
        decode(text.as_bytes(), PER_BYTE, ns)?;
    }

    #[test]
    fn xyz_messages(raw in bytes(512)) {
        decode(&raw, PER_BYTE, xyz)?;
    }

    #[test]
    fn fix_frames(raw in prop_oneof![input("8"), input("d"), input("1")]) {
        decode(&raw, PER_BYTE, fix)?;
    }

    #[test]
    fn fixcomp_frames(raw in prop_oneof![bytes(512), mutated(samples(|s| s.raw.starts_with(b"8=FIXCOMP")))]) {
        decode(&raw, PER_BYTE_INFLATED, fixcomp)?;
    }

    #[test]
    fn connection_framing(raw in prop_oneof![bytes(1024), mutated(samples(|s| !s.inner))]) {
        decode(&raw, PER_BYTE, connection)?;
    }

    #[test]
    fn tick_frames(raw in input("P")) {
        decode(&raw, PER_BYTE, ticks)?;
    }

    #[test]
    fn depth_frames(raw in input("Y")) {
        decode(&raw, PER_BYTE, depth)?;
    }

    #[test]
    fn generic_frames(raw in input("G")) {
        decode(&raw, PER_BYTE, generic)?;
    }

    /// No tick-by-tick frame is recorded: they are built, then mutated.
    #[test]
    fn tick_by_tick_frames(raw in prop_oneof![bytes(1024), tbt_frame().prop_flat_map(|f| {
        let f: &'static [u8] = Box::leak(f.0.into_boxed_slice());
        mutated(vec![f])
    })]) {
        decode(&raw, PER_BYTE, tick_by_tick)?;
    }

    #[test]
    fn farm_text_frames(raw in prop_oneof![input("Q"), input("L"), input("3"), input("1")]) {
        decode(&raw, PER_BYTE, farm_text)?;
    }

    #[test]
    fn hmds_xml_replies(xml in prop_oneof![bytes(1024), mutated(xml_samples())]) {
        decode(&xml, PER_BYTE, hmds_xml)?;
    }

    #[test]
    fn hmds_binary_payloads(raw in prop_oneof![bytes(1024), mutated(samples(|s| s.is("W") || s.is("U") && s.conn == "ushmds"))]) {
        decode(&raw, PER_BYTE_INFLATED, hmds_binary)?;
    }

    #[test]
    fn contract_replies(raw in prop_oneof![input("d"), ccp_u(&["107"])]) {
        decode(&raw, PER_BYTE, contracts)?;
    }

    #[test]
    fn logon_replies(raw in prop_oneof![input("A"), input("1"), mutated(samples(|s| s.raw.starts_with(b"8=FIXCOMP")))]) {
        decode(&raw, PER_BYTE_INFLATED, logon)?;
    }

    #[test]
    fn auth_replies(raw in prop_oneof![ccp_u(&["5", "139", "20", "60", "36", "7"]), input("8"), matching_symbols()]) {
        decode(&raw, PER_BYTE, auth)?;
    }

    /// The server hello of the key exchange (a missing field or bad base64
    /// is an error, ibx#276) and the encrypted frames after it.
    #[test]
    fn key_exchange_frames(fields in prop::collection::vec("[A-Za-z0-9+/=]{0,64}", 0..4), sealed in bytes(256)) {
        decode(&sealed, PER_BYTE, key_exchange)?;
        let refs: Vec<&str> = fields.iter().map(String::as_str).collect();
        let _ = crate::auth::dh::SecureChannel::zero_keys_for_test().process_server_hello(&refs);
    }

    /// The certificates of the key-exchange reply (ibx#276): a captured
    /// certificate mutated, in place of each certificate of the captured
    /// chain, parses and checks to a result.
    #[test]
    fn key_exchange_certificates(at in 0..3usize, cert in mutated(captured_certificates())) {
        use crate::auth::certs;
        let chain = captured_certificates();
        decode(&cert, PER_BYTE, |b| {
            let mut ders: Vec<&[u8]> = chain.clone();
            ders[at] = b;
            let parsed: Result<Vec<_>, _> = ders.iter().map(|d| certs::parse_certificate(d)).collect();
            parsed.map(|p| certs::check_server_certificates(&[0u8; 256], &p, certs::fixture::NOW_MS, false))
        })?;
    }
}

/// The certificates of the captured key-exchange reply.
fn captured_certificates() -> Vec<&'static [u8]> {
    use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
    static CERTS: std::sync::OnceLock<Vec<Vec<u8>>> = std::sync::OnceLock::new();
    CERTS
        .get_or_init(|| crate::auth::certs::fixture::captured_hello_certificates()[2..].iter().map(|c| B64.decode(c).unwrap()).collect())
        .iter()
        .map(Vec::as_slice)
        .collect()
}

/// A tick-by-tick frame of a few entries: the 2-byte bit count, then per
/// entry the stream id, the time and numbers and texts (high bit last).
fn tbt_frame() -> impl Strategy<Value = Wire> {
    let vlq = |v: u64| {
        let mut groups = vec![(v & 0x7F) as u8 | 0x80];
        let mut v = v >> 7;
        while v > 0 {
            groups.push((v & 0x7F) as u8);
            v >>= 7;
        }
        groups.reverse();
        groups
    };
    prop::collection::vec((0u64..20, 1_700_000_000u64..1_800_000_000, prop::collection::vec(any::<u32>(), 2..8), "[A-Z]{0,4}"), 1..6)
        .prop_map(move |entries| {
            let mut data = Vec::new();
            for (id, time, numbers, text) in entries {
                data.extend(vlq(id));
                data.extend(vlq(time));
                for n in numbers {
                    data.extend(vlq(n as u64));
                }
                let mut t: Vec<u8> = text.into_bytes();
                match t.last_mut() {
                    Some(last) => *last |= 0x80,
                    None => t.push(0x80),
                }
                data.extend(t);
            }
            let mut frame = ((data.len() * 8) as u16).to_be_bytes().to_vec();
            frame.extend(data);
            Wire(frame)
        })
}

/// Matching symbols replies (6040=186): none is recorded, so they are
/// built, then mutated.
fn matching_symbols() -> impl Strategy<Value = Wire> {
    let rows = prop::collection::vec(
        ("[A-Z.]{0,6}", prop_oneof![Just("STK"), Just("CS"), Just(""), Just("BOND")], "[0-9x-]{0,10}", "[A-Z:0-9,]{0,20}"),
        0..5,
    );
    rows.prop_map(|rows| {
        let mut fields: Vec<(u32, String)> = vec![(35, "U".into()), (6040, "186".into()), (320, "7".into()), (146, rows.len().to_string())];
        for (symbol, sec_type, con_id, derivatives) in rows {
            fields.push((55, symbol));
            fields.push((167, sec_type.to_string()));
            fields.push((6008, con_id));
            fields.push((8533, derivatives));
        }
        let refs: Vec<(u32, &str)> = fields.iter().map(|(t, v)| (*t, v.as_str())).collect();
        Wire(fix::fix_build(&refs, 1))
    })
}

// ── What the property tests found (ibx#488) ──

#[test]
fn ns_frame_buffer_grows_as_its_bytes_come() {
    let mut frame = crate::protocol::ns::NS_MAGIC.to_vec();
    frame.extend(0x7FFF_FFF0u32.to_be_bytes());
    frame.extend(b"50;533;");
    let (result, peak) = peak_alloc(|| crate::protocol::ns::ns_recv(&mut std::io::Cursor::new(&frame)));
    assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::UnexpectedEof);
    assert!(peak < 1 << 20, "{peak} bytes held for a 15-byte frame");
}

#[test]
fn a_signature_before_the_body_does_not_check() {
    let msg = b"8=FIX.4.1\x01rm\x01128349=4D14F2ED\x019=00028\x0135=1\x011=fa=0\x01";
    let (_, iv, valid) = crate::protocol::fix::fix_unsign(msg, &[7u8; 20], &[3u8; 16]);
    assert!(!valid);
    assert_eq!(iv, vec![3u8; 16], "the read IV does not move");
}

#[test]
fn a_length_past_the_address_space_never_completes() {
    use crate::protocol::connection::{mem_pair, Connection};
    let huge = "18446744073709551615";
    let (end, _peer) = mem_pair();
    let mut conn = Connection::new_mem(end);
    for head in ["8=O", "8=1", "8=FIX.4.1"] {
        conn.seed_buffer(format!("{head}\x019={huge}\x0135=0\x01").as_bytes());
        assert!(conn.extract_frames().is_empty(), "{head}");
    }
    let comp = format!("8=FIXCOMP\x019={huge}\x0195=1\x0196=x\x01");
    assert_eq!(crate::protocol::fixcomp::fixcomp_length(comp.as_bytes()), None);
    // A raw block inside the compressed messages, and a binary message,
    // whose lengths run past the end.
    let inner = format!("8=FIX.4.1\x019=10\x0195={huge}\x0196=ab\x0110=000\x018=O\x019={huge}\x0135=P\x01");
    let frame = crate::protocol::fixcomp::fixcomp_build(inner.as_bytes());
    assert_eq!(crate::protocol::fixcomp::fixcomp_decompress(&frame).unwrap(), Vec::<Vec<u8>>::new());
    let raw = format!("35=W\x0195={huge}\x0196=abc\x01");
    assert_eq!(crate::engine::hot_loop::extract_raw_tag(raw.as_bytes(), 96).unwrap(), b"abc\x01");
}

#[test]
fn a_bar_low_price_with_its_sign_bit() {
    let bar = crate::control::historical::decode_bar_payload(b"\x00\x00\x00\x10\x00\x08\x00", 1.0).expect("a bar");
    assert!(bar.low < 0.0, "{bar:?}");
}

#[test]
fn a_dividend_date_is_cut_by_characters() {
    let xml = "<div><date>2027\u{fffd}0617x</date><amt>1.5</amt></div>";
    let items = crate::control::optcalc::inputs::parse_reference_xml(xml);
    assert_eq!(items[0].date, "2027\u{fffd}061");
}
