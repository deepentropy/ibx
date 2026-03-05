//! FIXCOMP: zlib-compressed FIX messages used on farm connections.
//!
//! Wire format: `8=FIXCOMP\x01 9=<body_len>\x01 95=<zlib_len>\x01 96=<zlib_data>\x01`
//! No tag 10 checksum (signing adds 8349 separately).

use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use std::io::{Read, Write};

use super::fix::SOH;

/// Wrap a FIX message in FIXCOMP compression.
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

/// Decompress a FIXCOMP message into individual FIX / 8=O messages.
pub fn fixcomp_decompress(data: &[u8]) -> Vec<Vec<u8>> {
    // Find tag 95 (RawDataLength)
    let raw = if let Some(idx95) = find_tag(data, b"95=") {
        let soh = data[idx95..].iter().position(|&b| b == SOH).unwrap() + idx95;
        let raw_len: usize = std::str::from_utf8(&data[idx95 + 3..soh])
            .unwrap()
            .parse()
            .unwrap();
        if let Some(idx96) = find_tag(&data[soh..], b"96=") {
            let start = soh + idx96 + 3;
            &data[start..start + raw_len]
        } else {
            &data[soh + 1..soh + 1 + raw_len]
        }
    } else {
        // Fallback: zlib data starts after second SOH
        let soh1 = data.iter().position(|&b| b == SOH).unwrap();
        let soh2 = data[soh1 + 1..].iter().position(|&b| b == SOH).unwrap() + soh1 + 1;
        &data[soh2 + 1..]
    };

    let mut decoder = ZlibDecoder::new(raw);
    let mut decompressed = Vec::new();
    decoder.read_to_end(&mut decompressed).unwrap();

    split_messages(&decompressed)
}

/// Return total byte length of a FIXCOMP message, or None if incomplete.
pub fn fixcomp_length(data: &[u8]) -> Option<usize> {
    if data.len() < 10 {
        return None;
    }
    let soh1 = data.iter().position(|&b| b == SOH)?;
    let tag9 = find_tag(&data[soh1..], b"9=").map(|p| soh1 + p)?;
    let soh2 = data[tag9..].iter().position(|&b| b == SOH).map(|p| tag9 + p)?;
    let body_len: usize = std::str::from_utf8(&data[tag9 + 2..soh2]).ok()?.parse().ok()?;
    let total = soh2 + 1 + body_len;
    if data.len() < total {
        None
    } else {
        Some(total)
    }
}

fn find_tag(data: &[u8], needle: &[u8]) -> Option<usize> {
    data.windows(needle.len()).position(|w| w == needle)
}

/// Split decompressed FIXCOMP content into individual messages.
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
                    let total = soh9 + 1 + body_len;
                    if total > chunk.len() {
                        break;
                    }
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
                                scan = tag96 + 3 + rdl;
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
        let messages = fixcomp_decompress(&comp);
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
}
