//! Protocol test vectors from ibgw-headless.
//! These tests validate the binary protocol decoders against known-good captured data.
//!
//! Sources:
//! - ibgw-headless/tests/test_fix.py (FIX checksum, XOR fold)
//! - ibgw-headless/tests/test_historical.py (VLQ, hibit strings, tick decoding, RTBAR captures)

// ============================================================
// VLQ (Variable-Length Quantity) decoder
// ============================================================
// IB uses VLQ encoding for tick-by-tick data. Bit 7 (0x80) marks the last byte.
// Multi-byte values: bits [6:0] concatenated, MSB first.

/// Decode a VLQ value from a byte slice starting at `pos`.
/// Returns (value, byte_count, new_position).
fn read_vlq(data: &[u8], pos: usize) -> (u64, usize, usize) {
    let mut val: u64 = 0;
    let mut i = pos;
    loop {
        let b = data[i] as u64;
        val = (val << 7) | (b & 0x7F);
        i += 1;
        if b & 0x80 != 0 {
            break;
        }
    }
    (val, i - pos, i)
}

/// Convert VLQ unsigned value to signed using n-byte range.
/// Upper half of the range maps to negative values.
fn vlq_signed(val: i64, n: usize) -> i64 {
    let half = 1i64 << (7 * n - 1);
    if val >= half {
        val - (half << 1)
    } else {
        val
    }
}

/// Decode a hi-bit terminated string. Each byte is a 7-bit ASCII char;
/// the last byte has bit 7 set. 0x80 alone = empty string.
fn read_hibit_str(data: &[u8], pos: usize) -> (String, usize) {
    let mut s = String::new();
    let mut i = pos;
    loop {
        let b = data[i];
        let ch = (b & 0x7F) as char;
        i += 1;
        if b & 0x80 != 0 {
            if ch != '\0' {
                s.push(ch);
            }
            break;
        }
        s.push(ch);
    }
    (s, i)
}

// --- VLQ tests (from ibgw-headless TestVLQ) ---

#[test]
fn vlq_single_byte() {
    // 0x88 → bit7=1 (last), value = 8
    let (val, n, _) = read_vlq(&[0x88], 0);
    assert_eq!(val, 8);
    assert_eq!(n, 1);
}

#[test]
fn vlq_multi_byte() {
    // 0x01 0x4d 0xe7 → 26343 (263.43 cents)
    let (val, n, _) = read_vlq(&[0x01, 0x4d, 0xe7], 0);
    assert_eq!(val, 26343);
    assert_eq!(n, 3);
}

#[test]
fn vlq_signed_positive() {
    assert_eq!(vlq_signed(8, 1), 8);
}

#[test]
fn vlq_signed_negative() {
    // 122 with 1 byte: 122 >= 64 → 122 - 128 = -6
    assert_eq!(vlq_signed(122, 1), -6);
}

#[test]
fn vlq_signed_zero() {
    assert_eq!(vlq_signed(0, 1), 0);
}

// --- Hi-bit string tests ---

#[test]
fn hibit_str_finra() {
    // FINRA = 46 49 4e 52 c1 (last char A=0x41|0x80)
    let (s, pos) = read_hibit_str(&[0x46, 0x49, 0x4e, 0x52, 0xc1], 0);
    assert_eq!(s, "FINRA");
    assert_eq!(pos, 5);
}

#[test]
fn hibit_str_empty() {
    // 0x80 = empty string (null with hi-bit)
    let (s, pos) = read_hibit_str(&[0x80], 0);
    assert_eq!(s, "");
    assert_eq!(pos, 1);
}

#[test]
fn hibit_str_single_char() {
    // 0xC9 = 'I' | 0x80
    let (s, _) = read_hibit_str(&[0xc9], 0);
    assert_eq!(s, "I");
}

#[test]
fn hibit_str_arca() {
    // "ARCA" = 0x41 0x52 0x43 0xc1
    let (s, pos) = read_hibit_str(&[0x41, 0x52, 0x43, 0xc1], 0);
    assert_eq!(s, "ARCA");
    assert_eq!(pos, 4);
}

// ============================================================
// Tick-by-tick binary decoding
// ============================================================
// AllLast marker = 0x81, BidAsk marker = 0x82
// Format: [2-byte header] [marker] [ts_vlq] [fields...]

/// Parse an AllLast tick entry from raw bytes.
/// Returns (timestamp, price_cents, attribs_mask, size, exchange, conditions).
fn decode_alllast_tick(
    data: &[u8],
    pos: usize,
    price_state: &mut i64,
) -> (u64, i64, u8, u64, String, String) {
    let mut p = pos;

    let (ts, _, new_p) = read_vlq(data, p);
    p = new_p;

    let (price_raw, n, new_p) = read_vlq(data, p);
    p = new_p;
    let delta = vlq_signed(price_raw as i64, n);
    *price_state += delta;

    let (attribs_raw, _, new_p) = read_vlq(data, p);
    p = new_p;
    let attribs_mask = (attribs_raw & 3) as u8;

    let (size, _, new_p) = read_vlq(data, p);
    p = new_p;

    let (exchange, new_p) = read_hibit_str(data, p);
    p = new_p;

    let (conditions, _) = read_hibit_str(data, p);

    (ts, *price_state, attribs_mask, size, exchange, conditions)
}

/// Parse a BidAsk tick entry from raw bytes.
/// Returns (timestamp, bid_cents, ask_cents, attribs, bid_size, ask_size).
fn decode_bidask_tick(
    data: &[u8],
    pos: usize,
    bid_state: &mut i64,
    ask_state: &mut i64,
) -> (u64, i64, i64, u8, u64, u64) {
    let mut p = pos;

    let (ts, _, new_p) = read_vlq(data, p);
    p = new_p;

    let (bid_raw, n, new_p) = read_vlq(data, p);
    p = new_p;
    *bid_state += vlq_signed(bid_raw as i64, n);

    let (ask_raw, n, new_p) = read_vlq(data, p);
    p = new_p;
    *ask_state += vlq_signed(ask_raw as i64, n);

    let (attribs_raw, _, new_p) = read_vlq(data, p);
    p = new_p;
    let attribs = (attribs_raw & 3) as u8;

    let (bid_size, _, new_p) = read_vlq(data, p);
    p = new_p;

    let (ask_size, _, _) = read_vlq(data, p);

    (ts, *bid_state, *ask_state, attribs, bid_size, ask_size)
}

// --- AllLast tick vectors (from ibgw-headless TestTbtDecode) ---

#[test]
fn alllast_first_tick() {
    // First tick: delta from 0 → absolute price = 26343 ($263.43)
    let body: &[u8] = &[
        0x00, 0x00, // header
        0x81, // AllLast marker
        0xe4, // ts=100
        0x01, 0x4d, 0xe7, // price=26343
        0x84, // attribs=4
        0xe4, // size=100
        0x41, 0x52, 0x43, 0xc1, // "ARCA"
        0x80, // empty conditions
    ];

    let mut price_state: i64 = 0;
    let (ts, price, attribs, size, exchange, conditions) =
        decode_alllast_tick(body, 3, &mut price_state); // skip header + marker

    assert_eq!(ts, 100);
    assert_eq!(price, 26343);
    assert_eq!(attribs, 0); // 4 & 3 = 0
    assert_eq!(size, 100);
    assert_eq!(exchange, "ARCA");
    assert_eq!(conditions, "");
    assert_eq!(price_state, 26343);
}

#[test]
fn alllast_delta_positive() {
    // Second tick: delta = +8 cents → 26343 + 8 = 26351 ($263.51)
    let body: &[u8] = &[
        0x00, 0x00, // header
        0x81, // AllLast marker
        0xe4, // ts=100
        0x88, // price delta=+8
        0x84, // attribs=4
        0xc8, // size=72
        0x80, // empty exchange
        0x80, // empty conditions
    ];

    let mut price_state: i64 = 26343;
    let (_, price, _, size, exchange, _) = decode_alllast_tick(body, 3, &mut price_state);

    assert_eq!(price, 26351);
    assert_eq!(size, 72);
    assert_eq!(exchange, "");
    assert_eq!(price_state, 26351);
}

#[test]
fn alllast_delta_negative() {
    // delta = -6: VLQ byte 0xfa → value=122, signed(122,1) = 122-128 = -6
    let body: &[u8] = &[
        0x00, 0x00, //
        0x81, //
        0xe4, // ts
        0xfa, // VLQ value=122 → signed=-6
        0x84, // attribs
        0x81, // size=1
        0x80, // exchange
        0x80, // conditions
    ];

    let mut price_state: i64 = 26351;
    let (_, price, _, _, _, _) = decode_alllast_tick(body, 3, &mut price_state);

    assert_eq!(price, 26345); // 26351 - 6
}

#[test]
fn alllast_delta_state_persists() {
    // First message: price = 26343
    let body1: &[u8] = &[
        0x00, 0x00, 0x81, 0xe4, 0x01, 0x4d, 0xe7, 0x84, 0xe4, 0x80, 0x80,
    ];
    let mut price_state: i64 = 0;
    decode_alllast_tick(body1, 3, &mut price_state);
    assert_eq!(price_state, 26343);

    // Second message: delta = +8
    let body2: &[u8] = &[0x00, 0x00, 0x81, 0xe4, 0x88, 0x84, 0xe4, 0x80, 0x80];
    decode_alllast_tick(body2, 3, &mut price_state);
    assert_eq!(price_state, 26351);
}

#[test]
fn attribs_unreported() {
    // raw=14 → mask = 14 & 3 = 2 (unreported bit)
    let body: &[u8] = &[
        0x00, 0x00, 0x81, 0xe4, 0x01, 0x4d, 0xe7, 0x8e, // attribs=14
        0xe4, 0x80, 0x80,
    ];
    let mut ps: i64 = 0;
    let (_, _, attribs, _, _, _) = decode_alllast_tick(body, 3, &mut ps);
    assert_eq!(attribs, 2); // 14 & 3 = 2
}

// --- BidAsk tick vectors ---

#[test]
fn bidask_first_tick() {
    // bid=26340 ($263.40), ask=26345 ($263.45), bidSize=200, askSize=300
    let body: &[u8] = &[
        0x00, 0x00, //
        0x82, // BidAsk marker
        0xe4, // ts=100
        0x01, 0x4d, 0xe4, // bid=26340
        0x01, 0x4d, 0xe9, // ask=26345
        0x80, // attribs=0
        0x01, 0xc8, // bidSize=200
        0x02, 0xac, // askSize=300
    ];

    let mut bid_state: i64 = 0;
    let mut ask_state: i64 = 0;
    let (ts, bid, ask, attribs, bid_size, ask_size) =
        decode_bidask_tick(body, 3, &mut bid_state, &mut ask_state);

    assert_eq!(ts, 100);
    assert_eq!(bid, 26340);
    assert_eq!(ask, 26345);
    assert_eq!(attribs, 0);
    assert_eq!(bid_size, 200);
    assert_eq!(ask_size, 300);
}

// ============================================================
// FIX protocol
// ============================================================

/// Compute FIX checksum: sum of all bytes mod 256, zero-padded to 3 digits.
fn fix_checksum(data: &[u8]) -> String {
    let sum: u32 = data.iter().map(|&b| b as u32).sum();
    format!("{:03}", sum % 256)
}

/// XOR-fold a 20-byte HMAC digest to 4 bytes → 8-char uppercase hex.
fn xor_fold(data: &[u8]) -> String {
    assert_eq!(data.len(), 20);
    let mut result = [0u8; 4];
    for (i, &b) in data.iter().enumerate() {
        result[i % 4] ^= b;
    }
    result
        .iter()
        .map(|b| format!("{:02X}", b))
        .collect::<String>()
}

#[test]
fn fix_checksum_abc() {
    assert_eq!(fix_checksum(b"abc"), format!("{:03}", (97 + 98 + 99) % 256));
}

#[test]
fn fix_checksum_zero_padded() {
    let result = fix_checksum(&[0x01]);
    assert_eq!(result.len(), 3);
    assert_eq!(result, "001");
}

#[test]
fn xor_fold_sequential() {
    // 20-byte input: bytes 0..20
    let data: Vec<u8> = (0u8..20).collect();
    let result = xor_fold(&data);
    assert_eq!(result.len(), 8);
    assert_eq!(result, result.to_uppercase());

    // Verify manually: fold 5 groups of 4
    let mut r = [0u8; 4];
    for off in (0..20).step_by(4) {
        for i in 0..4 {
            r[i] ^= data[off + i];
        }
    }
    let expected: String = r.iter().map(|b| format!("{:02X}", b)).collect();
    assert_eq!(result, expected);
}

// ============================================================
// Real-time bar captures (RTBAR)
// ============================================================
// Binary payloads captured from ibgw-headless issue #37.
// Format: 8=O\x019=0043\x0135=G\x01 [binary OHLCV data]

/// Known RTBAR captures: (raw_bytes, (time, open, high, low, close, volume, count))
const RTBAR_CAPTURES: &[(&[u8], (u32, f64, f64, f64, f64, u32, u32))] = &[
    (
        b"8=O\x019=0043\x0135=G\x01\
          \x00\xa8\x00\x00\x00\x01\x69\xa7\x01\xf2\
          \x0c\x0c\xd6\x20\xda\xd3\x18\x30\x00\x02\x5b\x81\x3a",
        (1772552690, 262.90, 262.95, 262.89, 262.95, 603, 6),
    ),
    (
        b"8=O\x019=0043\x0135=G\x01\
          \x00\xa8\x00\x00\x00\x01\x69\xa7\x01\xf7\
          \x0c\x0c\xd6\x03\x1b\xd1\x98\xb0\x00\x0f\x14\x86\x61",
        (1772552695, 262.93, 262.94, 262.88, 262.91, 3860, 24),
    ),
    (
        b"8=O\x019=0043\x0135=G\x01\
          \x00\xa8\x00\x00\x00\x01\x69\xa7\x01\xfc\
          \x0c\x0c\xd6\xa5\x3c\xfe\x70\x30\x00\x14\x51\xa4\x9f",
        (1772552700, 262.94, 263.21, 262.93, 263.21, 5201, 41),
    ),
    (
        b"8=O\x019=0043\x0135=G\x01\
          \x00\xa8\x00\x00\x00\x01\x69\xa7\x02\x01\
          \x0c\x0c\xd8\x06\xdd\x50\x66\x50\x00\x23\x2d\xb6\xf7",
        (1772552705, 263.22, 263.29, 263.04, 263.04, 9005, 54),
    ),
    (
        b"8=O\x019=0043\x0135=G\x01\
          \x00\xa8\x00\x00\x00\x01\x69\xa7\x02\x06\
          \x0c\x0c\xd6\xc3\x5e\x90\x31\x30\x00\x10\x64\x8c\x2b",
        (1772552710, 263.03, 263.06, 262.94, 262.94, 4196, 26),
    ),
    (
        b"8=O\x019=0043\x0135=G\x01\
          \x00\xa8\x00\x00\x00\x01\x69\xa7\x02\x0b\
          \x0c\x0c\xd5\x83\x7f\x53\xa5\x30\x00\x15\x39\x8c\xa6",
        (1772552715, 262.93, 262.93, 262.84, 262.91, 5433, 27),
    ),
];

#[test]
fn rtbar_captures_have_fix_prefix() {
    // Verify all captures start with the expected FIX header
    for (raw, _) in RTBAR_CAPTURES {
        assert!(raw.starts_with(b"8=O\x01"));
        let text = String::from_utf8_lossy(&raw[..20]);
        assert!(text.contains("35=G"));
    }
}

#[test]
fn rtbar_captures_consistent_length() {
    // All captures should have the same body length (tag 9=0043)
    for (raw, _) in RTBAR_CAPTURES {
        let text = String::from_utf8_lossy(raw);
        assert!(text.contains("9=0043"));
    }
}

#[test]
fn rtbar_timestamps_are_sequential() {
    let times: Vec<u32> = RTBAR_CAPTURES.iter().map(|(_, e)| e.0).collect();
    for w in times.windows(2) {
        assert_eq!(w[1] - w[0], 5, "RTBAR timestamps should be 5s apart");
    }
}
