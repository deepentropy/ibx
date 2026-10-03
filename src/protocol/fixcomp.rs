//! Compressed FIX message framing for market data connections.

use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use std::io::{self, Read, Write};

use super::fix::SOH;

fn parse_err(msg: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

/// Wrap a FIX message in compressed framing.
pub fn fixcomp_build(inner_msg: &[u8]) -> Vec<u8> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(inner_msg).unwrap();
    let compressed = encoder.finish().unwrap();

    // Body: 95=<len>\x01 96=<compressed>\x01
    let mut body = Vec::new();
    body.extend_from_slice(format!("95={}\x01", compressed.len()).as_bytes());
    body.extend_from_slice(b"96=");
    body.extend_from_slice(&compressed);
    body.push(SOH);

    // Header: 8=FIXCOMP\x01 9=<body_len>\x01
    let mut msg = Vec::new();
    msg.extend_from_slice(format!("8=FIXCOMP\x019={}\x01", body.len()).as_bytes());
    msg.extend_from_slice(&body);
    msg
}

/// Decompress a compressed message into individual inner messages.
///
/// Returns `Err` if the frame is malformed (no tag 95, bad raw-data-length, etc.)
/// or if the zlib payload fails to inflate. Hot-loop callers should `log::warn!`
/// and skip the frame rather than propagate.
pub fn fixcomp_decompress(data: &[u8]) -> io::Result<Vec<Vec<u8>>> {
    let raw = if let Some(idx95) = find_tag(data, b"\x0195=").map(|p| p + 1) {
        let soh = data[idx95..]
            .iter()
            .position(|&b| b == SOH)
            .map(|p| idx95 + p)
            .ok_or_else(|| parse_err("fixcomp: tag 95 has no terminating SOH"))?;
        let raw_len: usize = std::str::from_utf8(&data[idx95 + 3..soh])
            .ok()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| parse_err("fixcomp: tag 95 value is not a usize"))?;
        let payload_start = if let Some(idx96) = find_tag(&data[soh..], b"96=") {
            soh + idx96 + 3
        } else {
            soh + 1
        };
        let payload_end = payload_start
            .checked_add(raw_len)
            .ok_or_else(|| parse_err("fixcomp: tag 95 length overflows usize"))?;
        if payload_end > data.len() {
            return Err(parse_err("fixcomp: tag 95 length exceeds frame size"));
        }
        &data[payload_start..payload_end]
    } else {
        // Fallback: zlib data starts after second SOH
        let soh1 = data
            .iter()
            .position(|&b| b == SOH)
            .ok_or_else(|| parse_err("fixcomp: no SOH in frame"))?;
        let soh2 = data[soh1 + 1..]
            .iter()
            .position(|&b| b == SOH)
            .map(|p| p + soh1 + 1)
            .ok_or_else(|| parse_err("fixcomp: no second SOH in frame"))?;
        &data[soh2 + 1..]
    };

    let mut decoder = ZlibDecoder::new(raw);
    let mut decompressed = Vec::new();
    if let Err(e) = decoder.read_to_end(&mut decompressed) {
        // ibx#182 root-cause tee: on inflate failure, dump the raw zlib
        // payload + the full enclosing frame as hex so we can tell
        // whether the slicing is off, the deflate stream is mid-message,
        // or the gateway sent genuinely corrupt bytes. Remove once the
        // upstream cause is identified.
        let raw_hex: String = raw.iter().map(|b| format!("{:02x}", b)).collect();
        let unsigned_hex: String = data.iter().map(|b| format!("{:02x}", b)).collect();
        log::warn!(
            "fixcomp tee: inflate failed ({}); unsigned_len={} raw_payload_len={} raw_hex={} unsigned_hex={}",
            e, data.len(), raw.len(), raw_hex, unsigned_hex,
        );
        return Err(e);
    }

    Ok(split_messages(&decompressed))
}

/// Return total byte length of a compressed message, or None if incomplete.
pub fn fixcomp_length(data: &[u8]) -> Option<usize> {
    if data.len() < 10 {
        return None;
    }
    let soh1 = data.iter().position(|&b| b == SOH)?;
    let tag9 = find_tag(&data[soh1..], b"9=").map(|p| soh1 + p)?;
    let soh2 = data[tag9..].iter().position(|&b| b == SOH).map(|p| tag9 + p)?;
    let body_len: usize = std::str::from_utf8(&data[tag9 + 2..soh2]).ok()?.parse().ok()?;
    // A length past the address space never completes (ibx#488).
    let total = (soh2 + 1).checked_add(body_len)?;
    if data.len() < total {
        None
    } else {
        Some(total)
    }
}

fn find_tag(data: &[u8], needle: &[u8]) -> Option<usize> {
    data.windows(needle.len()).position(|w| w == needle)
}

/// Split decompressed content into individual messages.
fn split_messages(buf: &[u8]) -> Vec<Vec<u8>> {
    let mut messages = Vec::new();
    let mut pos = 0;

    while pos < buf.len() {
        let remaining = &buf[pos..];

        let fix_start = find_tag(remaining, b"8=FIX.");
        let o_start = find_tag(remaining, b"8=O\x01");

        match (fix_start, o_start) {
            (None, None) => break,
            (fix_s, o_s) => {
                // Pick whichever comes first
                let o_first = match (o_s, fix_s) {
                    (Some(_), None) => true,
                    (Some(o), Some(f)) if o < f => true,
                    _ => false,
                };

                if o_first {
                    let o = o_s.unwrap();
                    let chunk = &remaining[o..];
                    // 8=O protocol: length-delimited via tag 9
                    let tag9 = match find_tag(&chunk[4..], b"9=") {
                        Some(p) => 4 + p,
                        None => break,
                    };
                    let soh9 = match chunk[tag9..].iter().position(|&b| b == SOH) {
                        Some(p) => tag9 + p,
                        None => break,
                    };
                    let body_len: usize = match std::str::from_utf8(&chunk[tag9 + 2..soh9]) {
                        Ok(s) => match s.parse() {
                            Ok(n) => n,
                            Err(_) => break,
                        },
                        Err(_) => break,
                    };
                    let total = match (soh9 + 1).checked_add(body_len) {
                        Some(t) if t <= chunk.len() => t,
                        _ => break,
                    };
                    messages.push(chunk[..total].to_vec());
                    pos += o + total;
                } else {
                    let f = fix_start.unwrap();
                    let chunk = &remaining[f..];
                    // Standard FIX: find 10=XXX SOH, skip past raw data blocks
                    let mut scan = 0;
                    let mut cksum = None;
                    loop {
                        let raw_tag = find_tag(&chunk[scan..], b"\x0195=").map(|p| scan + p);
                        let ck = find_tag(&chunk[scan..], b"\x0110=").map(|p| scan + p);

                        if let (Some(rt), _) = (raw_tag, ck) {
                            if ck.is_none() || rt < ck.unwrap() {
                                // Skip past raw data block
                                let after95 = match chunk[rt + 4..]
                                    .iter()
                                    .position(|&b| b == SOH)
                                    .map(|p| rt + 4 + p)
                                {
                                    Some(p) => p,
                                    None => break,
                                };
                                let rdl: usize =
                                    match std::str::from_utf8(&chunk[rt + 4..after95]) {
                                        Ok(s) => match s.parse() {
                                            Ok(n) => n,
                                            Err(_) => break,
                                        },
                                        Err(_) => break,
                                    };
                                let tag96 = match find_tag(&chunk[after95..], b"96=") {
                                    Some(p) => after95 + p,
                                    None => break,
                                };
                                // A block past the end leaves no
                                // message (ibx#488: the scan went past it).
                                scan = match (tag96 + 3).checked_add(rdl) {
                                    Some(s) if s <= chunk.len() => s,
                                    _ => break,
                                };
                                continue;
                            }
                        }
                        cksum = ck;
                        break;
                    }

                    let ck = match cksum {
                        Some(c) => c,
                        None => break,
                    };
                    let end = match chunk[ck + 4..].iter().position(|&b| b == SOH) {
                        Some(p) => ck + 4 + p,
                        None => break,
                    };
                    messages.push(chunk[..end + 1].to_vec());
                    pos += f + end + 1;
                }
            }
        }
    }

    messages
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::fix::{fix_build, fix_parse};

    #[test]
    fn build_structure() {
        let inner = fix_build(&[(35, "0")], 1);
        let comp = fixcomp_build(&inner);
        assert!(comp.starts_with(b"8=FIXCOMP"));
        assert!(comp.windows(3).any(|w| w == b"95="));
        assert!(comp.windows(3).any(|w| w == b"96="));
    }

    #[test]
    fn roundtrip() {
        let inner = fix_build(&[(35, "D"), (55, "MSFT"), (54, "2")], 7);
        let comp = fixcomp_build(&inner);
        let messages = fixcomp_decompress(&comp).unwrap();
        assert_eq!(messages.len(), 1);
        let parsed = fix_parse(&messages[0]);
        assert_eq!(parsed[&35], "D");
        assert_eq!(parsed[&55], "MSFT");
    }

    #[test]
    fn length_complete() {
        let inner = fix_build(&[(35, "0")], 1);
        let comp = fixcomp_build(&inner);
        assert_eq!(fixcomp_length(&comp), Some(comp.len()));
    }

    #[test]
    fn length_incomplete() {
        let inner = fix_build(&[(35, "0")], 1);
        let comp = fixcomp_build(&inner);
        assert_eq!(fixcomp_length(&comp[..10]), None);
    }

    #[test]
    fn roundtrip_large_message() {
        // Build a FIX message with body > 1000 bytes
        let long_value = "X".repeat(1000);
        let inner = fix_build(&[(35, "B"), (58, &long_value)], 1);
        assert!(inner.len() > 1000);

        let comp = fixcomp_build(&inner);
        let messages = fixcomp_decompress(&comp).unwrap();
        assert_eq!(messages.len(), 1);
        let parsed = fix_parse(&messages[0]);
        assert_eq!(parsed[&35], "B");
        assert_eq!(parsed[&58], long_value);
    }

    #[test]
    fn decompress_multiple_inner_fix_messages() {
        // Compress two FIX messages together into one FIXCOMP wrapper
        let msg1 = fix_build(&[(35, "0")], 1);
        let msg2 = fix_build(&[(35, "D"), (55, "GOOG")], 2);
        let mut combined = msg1.clone();
        combined.extend_from_slice(&msg2);

        let comp = fixcomp_build(&combined);
        let messages = fixcomp_decompress(&comp).unwrap();
        assert_eq!(messages.len(), 2, "expected 2 inner messages");

        let parsed1 = fix_parse(&messages[0]);
        assert_eq!(parsed1[&35], "0");

        let parsed2 = fix_parse(&messages[1]);
        assert_eq!(parsed2[&35], "D");
        assert_eq!(parsed2[&55], "GOOG");
    }

    #[test]
    fn fixcomp_length_missing_tag9() {
        // A buffer starting with 8=FIXCOMP but no tag 9 → should return None
        let data = b"8=FIXCOMP\x0195=5\x01";
        assert_eq!(fixcomp_length(data), None);
    }

    #[test]
    fn fixcomp_length_body_shorter_than_declared() {
        // Build a valid FIXCOMP, then check that fixcomp_length returns
        // the expected total even if the actual data is shorter (returns None).
        let inner = fix_build(&[(35, "0")], 1);
        let comp = fixcomp_build(&inner);
        let expected_total = fixcomp_length(&comp).unwrap();

        // Truncate: provide only half the body
        let half = comp.len() / 2;
        assert!(half < expected_total);
        assert_eq!(fixcomp_length(&comp[..half]), None);
    }

    #[test]
    fn decompress_corrupt_deflate_returns_err() {
        // Build a valid FIXCOMP frame, then trash the compressed payload so
        // ZlibDecoder fails. The function must return Err rather than panic
        // (regression for ibx#182).
        let inner = fix_build(&[(35, "0")], 1);
        let mut comp = fixcomp_build(&inner);
        let tag96 = comp.windows(3).position(|w| w == b"96=").unwrap();
        // Corrupt the first compressed byte (zlib CMF)
        comp[tag96 + 3] ^= 0xFF;
        let err = fixcomp_decompress(&comp).unwrap_err();
        assert!(err.to_string().to_lowercase().contains("corrupt")
            || err.to_string().to_lowercase().contains("invalid"));
    }

    #[test]
    fn decompress_truncated_payload_returns_err() {
        // tag 95 declares length N but the frame is shorter — must not panic.
        let inner = fix_build(&[(35, "0")], 1);
        let comp = fixcomp_build(&inner);
        let truncated = &comp[..comp.len() - 5];
        assert!(fixcomp_decompress(truncated).is_err());
    }

    #[test]
    fn fixcomp_build_produces_valid_zlib() {
        use flate2::read::ZlibDecoder;
        use std::io::Read as _;

        let inner = fix_build(&[(35, "A"), (108, "30")], 1);
        let comp = fixcomp_build(&inner);

        // Extract the zlib data from tag 96
        let tag96_pos = comp
            .windows(3)
            .position(|w| w == b"96=")
            .expect("tag 96 not found");
        let zlib_start = tag96_pos + 3;

        // Find tag 95 value for length
        let tag95_pos = comp
            .windows(3)
            .position(|w| w == b"95=")
            .expect("tag 95 not found");
        let soh_after_95 = comp[tag95_pos + 3..]
            .iter()
            .position(|&b| b == SOH)
            .unwrap()
            + tag95_pos
            + 3;
        let zlib_len: usize = std::str::from_utf8(&comp[tag95_pos + 3..soh_after_95])
            .unwrap()
            .parse()
            .unwrap();

        let zlib_data = &comp[zlib_start..zlib_start + zlib_len];

        // Decompress with raw flate2 to verify it's valid zlib
        let mut decoder = ZlibDecoder::new(zlib_data);
        let mut decompressed = Vec::new();
        decoder
            .read_to_end(&mut decompressed)
            .expect("zlib decompression failed");

        // Decompressed data should equal the original inner FIX message
        assert_eq!(decompressed, inner);
    }
}
