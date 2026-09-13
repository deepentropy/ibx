//! FIX message framing.
//!
//! IB's FIX format, as observed on the wire:
//! - Body length: 4-digit zero-padded (e.g. "0199")
//! - MsgSeqNum: 6-digit zero-padded (e.g. "000001")
//! - No SenderCompID (49) or TargetCompID (56) — set at session level
//! - SOH (0x01) delimiter

use std::collections::HashMap;
use std::io::{self, Read};

use hmac::{Hmac, Mac};
use sha1::Sha1;

/// What separates one field from the next.
pub const SOH: u8 = 0x01;

// Standard FIX tags
/// FIX tag 8: the begin string.
pub const TAG_BEGIN_STRING: u32 = 8;
/// FIX tag 9: the body length.
pub const TAG_BODY_LENGTH: u32 = 9;
/// FIX tag 10: the checksum.
pub const TAG_CHECKSUM: u32 = 10;
/// FIX tag 34: the msg seq num.
pub const TAG_MSG_SEQ_NUM: u32 = 34;
/// FIX tag 35: the msg type.
pub const TAG_MSG_TYPE: u32 = 35;
/// FIX tag 49: the sender comp id.
pub const TAG_SENDER_COMP_ID: u32 = 49;
/// FIX tag 52: the sending time.
pub const TAG_SENDING_TIME: u32 = 52;

/// This file's own text, so a handler can resolve a tag it refers to by name.
pub const SOURCE: &str = include_str!("fix.rs");
/// FIX tag 56: the target comp id.
pub const TAG_TARGET_COMP_ID: u32 = 56;
/// FIX tag 58: the text.
pub const TAG_TEXT: u32 = 58;
/// FIX tag 61: the urgency.
pub const TAG_URGENCY: u32 = 61;
/// FIX tag 98: the encrypt method.
pub const TAG_ENCRYPT_METHOD: u32 = 98;
/// FIX tag 108: the heartbeat int.
pub const TAG_HEARTBEAT_INT: u32 = 108;
/// FIX tag 112: the test req id.
pub const TAG_TEST_REQ_ID: u32 = 112;
/// FIX tag 141: the reset seq num.
pub const TAG_RESET_SEQ_NUM: u32 = 141;
/// FIX tag 148: the headline.
pub const TAG_HEADLINE: u32 = 148;
/// FIX tag 207: the security exchange.
pub const TAG_SECURITY_EXCHANGE: u32 = 207;

// IB custom tags
/// FIX tag 6034: the build.
pub const TAG_IB_BUILD: u32 = 6034;
/// FIX tag 6040: the comm type.
pub const TAG_IB_COMM_TYPE: u32 = 6040;
/// FIX tag 6143: the comp version.
pub const TAG_IB_COMP_VERSION: u32 = 6143;

/// FIX tag 6143 again, on a news bulletin: the number the venue gives it.
///
/// The same tag carries the comp version on a logon. Named twice because a
/// bulletin's id has nothing to do with a version, and reading one under the
/// other's name is how it went unread.
pub const TAG_BULLETIN_ID: u32 = 6143;
/// FIX tag 17: the venue's own name for an execution.
pub const TAG_EXEC_ID: u32 = 17;

/// FIX tag 6378, on a trade record: what the fill cost.
///
/// The execution report states no commission of any kind — this is where the
/// charge is. The same tag carries the estimated commission on a preview,
/// which is the same figure asked about before the event rather than after.
pub const TAG_TRADE_CHARGE: u32 = 6378;

/// FIX tag 6381: the currency the charge is in, which need not be the
/// contract's.
pub const TAG_TRADE_CHARGE_CURRENCY: u32 = 6381;

/// FIX tag 6968: the version.
pub const TAG_IB_VERSION: u32 = 6968;
/// FIX tag 8349: the hmac signature.
pub const TAG_HMAC_SIGNATURE: u32 = 8349;
/// What the signature field occupies: `8349=`, eight hex characters, and the
/// separator. Written by [`fix_sign`] and read back at that width by
/// [`carries_signature`], so the two cannot state different geometry.
const SIG_FIELD_LEN: usize = 5 + 8 + 1;
/// What a FIX.4.1 frame carries after its body: `10=XXX` and the separator.
pub(crate) const FIX41_TRAILER_LEN: usize = 7;

// Message types
/// Message type `0`: a heartbeat.
pub const MSG_HEARTBEAT: &str = "0";
/// Message type `1`: a test request.
pub const MSG_TEST_REQUEST: &str = "1";
/// Message type `3`: a reject.
pub const MSG_REJECT: &str = "3";
/// Message type `5`: a logout.
pub const MSG_LOGOUT: &str = "5";
/// Message type `A`: a logon.
pub const MSG_LOGON: &str = "A";
/// Message type `B`: a news item.
pub const MSG_NEWS: &str = "B";
/// Message type `U`: a venue-defined message.
pub const MSG_IB_CUSTOM: &str = "U";
/// Message type `8`: an execution report.
pub const MSG_EXEC_REPORT: &str = "8";
/// Message type `9`: a cancel reject.
pub const MSG_CANCEL_REJECT: &str = "9";
/// Message type `D`: a new order.
pub const MSG_NEW_ORDER: &str = "D";
/// Message type `F`: an order cancel.
pub const MSG_ORDER_CANCEL: &str = "F";
/// Message type `G`: an order replace.
pub const MSG_ORDER_REPLACE: &str = "G";
/// Message type `V`: a market-data request.
pub const MSG_MARKET_DATA_REQ: &str = "V";

/// Render a FIX byte buffer with SOH (0x01) shown as `|` and any other
/// non-printable bytes escaped as `\xNN`. For trace-level wire diagnostics
/// For trace-level wire diagnostics; not used on hot paths.
pub fn fmt_pipe(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() + 8);
    for &b in bytes {
        match b {
            SOH => s.push('|'),
            0x20..=0x7e => s.push(b as char),
            _ => s.push_str(&format!("\\x{b:02x}")),
        }
    }
    s
}

/// Sum of all bytes mod 256, zero-padded to 3 digits.
pub fn fix_checksum(data: &[u8]) -> String {
    let sum: u32 = data.iter().map(|&b| b as u32).sum();
    format!("{:03}", sum % 256)
}

/// Write one field's value, in the characters this wire carries.
///
/// A value is written character by character up to the first one outside
/// ASCII, and every character from there on that is outside it is written as a
/// single star. That is what the venue's own frames carry: no field value on
/// this wire holds a byte of 0x80 or above, and a name a caller gave in its own
/// script — an order reference, a group name, a model — went out as two or
/// three bytes where the venue reads one character, so the field the venue read
/// was not the field the caller stated and the length in front of it counted
/// bytes the venue does not count.
fn push_value(body: &mut Vec<u8>, val: &str) {
    if val.is_ascii() {
        body.extend_from_slice(val.as_bytes());
        return;
    }
    for c in val.chars() {
        if c.is_ascii() {
            let mut one = [0u8; 1];
            body.extend_from_slice(c.encode_utf8(&mut one).as_bytes());
        } else {
            body.push(b'*');
        }
    }
}

/// Build a complete FIX message matching IB Gateway format.
///
/// `fields` should NOT include tags 8, 9, 34, or 10 (auto-generated).
/// Body length is 4-digit zero-padded, MsgSeqNum is 6-digit zero-padded.
pub fn fix_build(fields: &[(u32, &str)], seq: u32) -> Vec<u8> {
    // Build body directly into bytes (no intermediate String, no format! per field)
    let mut body = Vec::with_capacity(fields.len() * 20 + 20);
    for (i, &(tag, val)) in fields.iter().enumerate() {
        if i == 1 {
            body.extend_from_slice(b"34=");
            push_u32_padded::<6>(&mut body, seq);
            body.push(SOH);
        }
        push_u32(&mut body, tag);
        body.push(b'=');
        push_value(&mut body, val);
        body.push(SOH);
    }
    if fields.len() == 1 {
        body.extend_from_slice(b"34=");
        push_u32_padded::<6>(&mut body, seq);
        body.push(SOH);
    }

    // Header: 8=FIX.4.1 SOH 9=NNNN SOH
    let mut msg = Vec::with_capacity(body.len() + 30);
    msg.extend_from_slice(b"8=FIX.4.1\x019=");
    push_u32_padded::<4>(&mut msg, body.len() as u32);
    msg.push(SOH);

    // Checksum covers header + body
    let cksum: u32 = msg.iter().chain(body.iter()).map(|&b| b as u32).sum();
    msg.extend_from_slice(&body);
    msg.extend_from_slice(b"10=");
    push_u32_padded::<3>(&mut msg, cksum % 256);
    msg.push(SOH);
    msg
}

/// Write a u32 as decimal digits (no zero padding, no alloc).
#[inline]
fn push_u32(buf: &mut Vec<u8>, mut val: u32) {
    if val == 0 {
        buf.push(b'0');
        return;
    }
    let start = buf.len();
    while val > 0 {
        buf.push(b'0' + (val % 10) as u8);
        val /= 10;
    }
    buf[start..].reverse();
}

/// Write a u32 as at least N zero-padded decimal digits (no alloc).
///
/// A value too large for N digits keeps every digit rather than losing the
/// high ones. Truncation states a number that is not the value: a body of
/// 10500 bytes announced as 0500 is framed ten thousand bytes short, and a
/// sequence number past 999999 restarts the session's count.
#[inline]
fn push_u32_padded<const N: usize>(buf: &mut Vec<u8>, val: u32) {
    let mut digits = [b'0'; N];
    let mut v = val;
    for d in digits.iter_mut().rev() {
        *d = b'0' + (v % 10) as u8;
        v /= 10;
    }
    if v > 0 {
        push_u32(buf, val);
        return;
    }
    buf.extend_from_slice(&digits);
}

/// The fields of a message, in wire order.
///
/// A field a length prefixes is not walked like the rest: tag 95 states the
/// length and tag 96 carries it, and the value is the bytes that length
/// states, delimiter or not. Splitting one on the delimiter reads whatever it
/// carries as fields of its own — a corporate action or a calendar entry
/// arriving under some other tag's number, and the field it came in cut short
/// at its first delimiter.
struct Fields<'a> {
    data: &'a [u8],
    at: usize,
    /// A raw field read ahead of the walk, so the length beside it stays the
    /// walk's own.
    queued: Option<(u32, usize, usize, usize)>,
}

impl<'a> Fields<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, at: 0, queued: None }
    }
}

/// One field: its tag, the offset it starts at, and the span of its value.
impl Iterator for Fields<'_> {
    type Item = (u32, usize, usize, usize);

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(item) = self.queued.take() {
            return Some(item);
        }
        let data = self.data;
        while self.at < data.len() {
            let seg_end = data[self.at..]
                .iter()
                .position(|&b| b == SOH)
                .map(|p| self.at + p)
                .unwrap_or(data.len());
            let eq = data[self.at..seg_end].iter().position(|&b| b == b'=');
            let start = self.at;
            self.at = seg_end + 1;
            let Some(eq) = eq else { continue };
            let Some(tag) = std::str::from_utf8(&data[start..start + eq])
                .ok()
                .and_then(|s| s.parse::<u32>().ok())
            else {
                continue;
            };
            let val = (start + eq + 1, seg_end);
            if tag == TAG_RAW_DATA_LENGTH && self.at < data.len() {
                // The length states how much follows under the next tag. Read
                // that span whole, then walk on behind it. A length the bytes
                // do not satisfy is a message cut inside the block: what is
                // here is read, and nothing past the end is.
                if let Some(n) = std::str::from_utf8(&data[val.0..val.1])
                    .ok()
                    .and_then(|s| s.parse::<usize>().ok())
                    && data[self.at..].starts_with(b"96=")
                {
                    let v0 = self.at + 3;
                    let v1 = v0 + n.min(data.len().saturating_sub(v0));
                    self.queued = Some((TAG_RAW_DATA, self.at, v0, v1));
                    self.at = v1;
                }
            }
            return Some((tag, start, val.0, val.1));
        }
        None
    }
}

/// FIX tag 95: the length of the raw data field that follows it.
pub const TAG_RAW_DATA_LENGTH: u32 = 95;
/// FIX tag 96: raw data, carried whole under the length tag 95 states.
pub const TAG_RAW_DATA: u32 = 96;

/// Parse a SOH-delimited FIX message into {tag: value}.
pub fn fix_parse(data: &[u8]) -> HashMap<u32, String> {
    let mut result = HashMap::new();
    for (tag, _, v0, v1) in Fields::new(data) {
        result.insert(tag, String::from_utf8_lossy(&data[v0..v1]).into_owned());
    }
    result
}

/// Parse a message whose fields repeat, one map per repetition.
///
/// [`fix_parse`] keeps the last value of each tag, which is right for a message
/// that states each field once and wrong for one that states a group of them
/// per item. A holding list and an order's conditions are both groups: read
/// flat, only the last holding survives and the conditions are not read at
/// all.
///
/// A repetition begins at `start_tag`. What appears before the first one
/// describes the message rather than any repetition, so it is carried onto
/// each; what follows the last belongs to it, because nothing on the wire marks
/// where a group ends. A message with no `start_tag` yields nothing, which is
/// what a message carrying no such group means.
pub fn fix_parse_repeating(data: &[u8], start_tag: u32) -> Vec<HashMap<u32, String>> {
    let mut header: Vec<(u32, String)> = Vec::new();
    let mut groups: Vec<Vec<(u32, String)>> = Vec::new();

    for part in data.split(|&b| b == SOH) {
        if part.is_empty() {
            continue;
        }
        let text = String::from_utf8_lossy(part);
        let Some((tag_str, value)) = text.split_once('=') else { continue };
        let Ok(tag) = tag_str.parse::<u32>() else { continue };

        if tag == start_tag {
            groups.push(vec![(tag, value.to_string())]);
        } else if let Some(current) = groups.last_mut() {
            current.push((tag, value.to_string()));
        } else {
            header.push((tag, value.to_string()));
        }
    }

    groups
        .into_iter()
        .map(|group| {
            let mut map: HashMap<u32, String> = header.iter().cloned().collect();
            map.extend(group);
            map
        })
        .collect()
}

/// Fold 20-byte HMAC digest to 4 bytes → 8-char uppercase hex.
pub fn xor_fold(h: &[u8]) -> String {
    let r = xor_fold_bytes(h);
    r.iter().map(|b| format!("{b:02X}")).collect()
}

/// Fold 20-byte HMAC digest to 4 bytes (no heap allocation).
#[inline]
fn xor_fold_bytes(h: &[u8]) -> [u8; 4] {
    debug_assert!(h.len() >= 20);
    let mut r = [0u8; 4];
    for (i, &b) in h[..20].iter().enumerate() {
        r[i % 4] ^= b;
    }
    r
}

/// Compare 4 XOR-folded bytes against an 8-char hex string (case-insensitive, no
/// alloc).
#[inline]
fn sig_hex_eq(expected: &[u8; 4], hex_ascii: &[u8]) -> bool {
    if hex_ascii.len() != 8 {
        return false;
    }
    for i in 0..4 {
        let hi = hex_nibble(hex_ascii[i * 2]);
        let lo = hex_nibble(hex_ascii[i * 2 + 1]);
        // A character that is not hexadecimal is not a signature. Left to
        // arithmetic alone, its `0xFF` lost its top four bits to the shift and
        // read as an `F`, so any expected byte from `F0` to `FF` was matched by
        // text that is not hexadecimal at all — a signature check a malformed
        // frame could pass.
        if hi > 0xF || lo > 0xF || (hi << 4 | lo) != expected[i] {
            return false;
        }
    }
    true
}

/// Write 4 bytes as 8-char uppercase hex (no alloc).
#[inline]
fn push_hex_upper(buf: &mut Vec<u8>, bytes: &[u8; 4]) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for &b in bytes {
        buf.push(HEX[(b >> 4) as usize]);
        buf.push(HEX[(b & 0xF) as usize]);
    }
}

#[inline]
fn hex_nibble(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'A'..=b'F' => b - b'A' + 10,
        b'a'..=b'f' => b - b'a' + 10,
        _ => 0xFF,
    }
}

/// Find the byte position right after `tag=value\x01` in the message.
fn find_after_tag(data: &[u8], tag_num: u32) -> Option<usize> {
    // Fast path for common tags (avoids format! heap allocation)
    match tag_num {
        9 => find_after_tag_bytes(data, b"9="),
        35 => find_after_tag_bytes(data, b"35="),
        _ => {
            let needle = format!("{tag_num}=");
            find_after_tag_bytes(data, needle.as_bytes())
        }
    }
}

/// Find byte position right after `needle value \x01`.
#[inline]
fn find_after_tag_bytes(data: &[u8], needle: &[u8]) -> Option<usize> {
    let idx = data.windows(needle.len()).position(|w| w == needle)?;
    let after = idx + needle.len();
    let soh = data[after..].iter().position(|&b| b == SOH)?;
    Some(after + soh + 1)
}

/// Sign a message with HMAC.
///
/// Returns (signed_msg, new_iv). The IV chains across messages.
pub fn fix_sign(msg: &[u8], mac_key: &[u8], iv: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let is_fixcomp = msg.starts_with(b"8=FIXCOMP");
    let is_fix41 = msg.starts_with(b"8=FIX.4.1");

    // 1. Extract body (after tag 9 SOH)
    let after9 = find_after_tag(msg, 9).unwrap();
    let body_end = if is_fix41 {
        // FIX.4.1: body ends before tag 10 (include the SOH before "10=")
        msg.windows(4)
            .rposition(|w| w == b"\x0110=")
            .map(|p| p + 1)
            .unwrap()
    } else {
        msg.len()
    };
    let body = &msg[after9..body_end];

    // 2. HMAC-SHA1(iv + body)
    let mut mac = Hmac::<Sha1>::new_from_slice(mac_key).unwrap();
    mac.update(iv);
    mac.update(body);
    let hmac_res = mac.finalize().into_bytes();

    // 3. XOR-fold → 4 bytes, write as hex directly (no String alloc)
    let folded = xor_fold_bytes(&hmac_res);


    // 4. Rebuild message with signature (pre-allocate)
    let signed_body_len = body.len() + SIG_FIELD_LEN;
    let hdr_end = msg.iter().position(|&b| b == SOH).unwrap() + 1;
    let header = &msg[..hdr_end];

    // Estimate: header + "9=NNNN\x01" + signed_body + "10=NNN\x01"
    let mut new_msg = Vec::with_capacity(hdr_end + 8 + signed_body_len + 8);
    new_msg.extend_from_slice(header);
    new_msg.extend_from_slice(b"9=");
    // Written plain, whichever header this is. An unsigned frame pads its
    // FIX.4.1 length to four digits, because it reserves those four bytes
    // before it knows the length and back-fills them; a signed one is built
    // again from the length it now knows, and the signer this follows writes
    // that plain and pads nothing. Padded here for consistency with the
    // unsigned build, every signed frame went out two bytes longer than the
    // venue is sent by the client this replaces, with a different checksum
    // behind it.
    push_u32(&mut new_msg, signed_body_len as u32);
    new_msg.push(SOH);
    new_msg.extend_from_slice(body);
    new_msg.extend_from_slice(b"8349=");
    push_hex_upper(&mut new_msg, &folded);
    new_msg.push(SOH);

    if is_fix41 {
        // Add tag 10 checksum
        let cksum: u32 = new_msg.iter().map(|&b| b as u32).sum();
        new_msg.extend_from_slice(b"10=");
        push_u32_padded::<3>(&mut new_msg, cksum % 256);
        new_msg.push(SOH);
    }

    // 5. XOR distortion (8 positions using IV pairs)
    let start = if is_fixcomp {
        find_after_tag(&new_msg, 9).unwrap()
    } else {
        find_after_tag(&new_msg, 35).unwrap()
    };
    // End offset: skip trailing 8349=XXXXXXXX SOH (14 bytes) + SOH (1 byte)
    let mut end = new_msg.len() as isize - 14 - 2;
    if is_fix41 {
        end -= 7; // also skip 10=XXX SOH
    }
    if end >= start as isize {
        let rng = end as usize - start + 1;
        for i in 0..8 {
            let pos = (iv[i * 2] as usize) % rng;
            new_msg[start + pos] ^= iv[i * 2 + 1];
        }
    }

    // 6. IV chain
    let mut new_iv = [0u8; 16];
    for i in 0..16 {
        new_iv[i] = iv[i] ^ hmac_res[i];
    }

    (new_msg, new_iv.to_vec())
}

/// Whether a frame carries a signature field.
///
/// Two readings, because neither answers alone and a frame either can call
/// signed is one to verify rather than hand on.
///
/// The position is where a signature this wire produces always is: last, at a
/// fixed width, ahead of the checksum where there is one — [`fix_sign`] puts
/// it there and nothing follows it. It is also the one reading the distortion
/// cannot spoil, because the reversal in [`fix_unsign`] stops short of the
/// trailer, so those bytes arrive as they were written whatever the eight XORs
/// did to the body.
///
/// The walk is what the position cannot do: a signature of some other width is
/// not at that offset, and handing one on unverified would be trusting a
/// field this client is able to check. It is the reading that fails on a
/// distorted body — eight bytes are flipped somewhere in it and one landing on
/// a separator merges two fields, so the walk runs past an intact signature
/// without seeing it. On its own that called a signed frame unsigned and
/// handed it on still distorted with the chain not advanced, and every signed
/// frame after it verified against the wrong state.
///
/// Neither reading matches the tag's characters inside a field value, which is
/// the third thing a text search would do and the reason this is not one.
pub fn carries_signature(msg: &[u8]) -> bool {
    let trailer = if msg.starts_with(b"8=FIX.4.1") { FIX41_TRAILER_LEN } else { 0 };
    // The whole shape, and a field of its own. A reject reason quoting the tag
    // can sit at exactly this offset, carry eight characters that read as hex,
    // and end where the separator is expected — `58=rejected: 8349=DEADBEEF`
    // is all three. What it cannot do is start a field: the byte in front of a
    // real one ends the field before it.
    let at_the_trailer = msg
        .len()
        .checked_sub(trailer + SIG_FIELD_LEN)
        .is_some_and(|at| {
            let field = &msg[at..at + SIG_FIELD_LEN];
            at > 0
                && msg[at - 1] == SOH
                && field.starts_with(b"8349=")
                && field[SIG_FIELD_LEN - 1] == SOH
                && field[5..SIG_FIELD_LEN - 1].iter().all(|b| b.is_ascii_hexdigit())
        });
    at_the_trailer || Fields::new(msg).any(|(tag, ..)| tag == TAG_HMAC_SIGNATURE)
}

/// Un-distort and verify a signed FIX message.
///
/// Returns (undistorted_msg, new_iv, signature_valid).
pub fn fix_unsign(msg: &[u8], mac_key: &[u8], iv: &[u8]) -> (Vec<u8>, Vec<u8>, bool) {
    let mut msg_bytes = msg.to_vec();

    let is_fix41 = msg.starts_with(b"8=FIX.4.1");
    let is_fixcomp = msg.starts_with(b"8=FIXCOMP");

    // 1. Un-distort (reverse XOR, loop backward)
    let start = if is_fixcomp {
        find_after_tag(&msg_bytes, 9).unwrap_or(0)
    } else {
        find_after_tag(&msg_bytes, 35).unwrap_or(0)
    };
    let mut end = msg_bytes.len() as isize - 14 - 2;
    if is_fix41 {
        end -= 7;
    }
    if end >= start as isize {
        let rng = end as usize - start + 1;
        for i in (0..8).rev() {
            let pos = (iv[i * 2] as usize) % rng;
            msg_bytes[start + pos] ^= iv[i * 2 + 1];
        }
    }

    // 2. Extract body + verify HMAC
    let after9 = match find_after_tag(&msg_bytes, 9) {
        Some(p) => p,
        None => return (msg_bytes, iv.to_vec(), false),
    };

    // Find the 8349 signature field. Walked as a field rather than matched
    // as a substring: the same text can sit inside a field *value* — a reject
    // reason quoting it, say — or inside a length-prefixed raw block the
    // message carries, and matched there the body boundary lands in the middle
    // of the message and a legitimate frame reports invalid.
    let (t8349, sig) = match Fields::new(&msg_bytes)
        .find(|&(tag, _, _, _)| tag == TAG_HMAC_SIGNATURE)
    {
        Some((_, start, v0, v1)) => (start, &msg_bytes[v0..v1]),
        None => return (msg_bytes, iv.to_vec(), false),
    };

    // The two offsets are found by separate searches, and a frame can put them
    // in either order: the bare `9=` needle matches inside `8349=`, so a frame
    // whose only `9=` lies within the signature field ends the body before it
    // begins. Sliced that way round it panics, and a panic here is the reader
    // thread and every subscription on it.
    let Some(body) = msg_bytes.get(after9..t8349) else {
        return (msg_bytes, iv.to_vec(), false);
    };

    let mut mac = Hmac::<Sha1>::new_from_slice(mac_key).unwrap();
    mac.update(iv);
    mac.update(body);
    let hmac_res = mac.finalize().into_bytes();
    let expected = xor_fold_bytes(&hmac_res);

    // The field's value, as the walk read it.
    let sig_valid = sig_hex_eq(&expected, sig);

    // 3. IV chain
    let mut new_iv = [0u8; 16];
    for i in 0..16 {
        new_iv[i] = iv[i] ^ hmac_res[i];
    }

    (msg_bytes, new_iv.to_vec(), sig_valid)
}

/// The end of the first complete FIX message in a buffer, if there is one.
///
/// A FIX.4.1 message ends at its checksum field, which is the last field it
/// carries. A compressed one carries no checksum at all: it ends where its body
/// length says it does, and the bytes before that are deflate output that can
/// hold anything, including the `10=` a checksum scan is looking for. Framed by
/// that scan, a compressed frame either never completed — the auth ACK arrives
/// as one, so the logon read timed out on it — or ended early inside its own
/// payload and took the message behind it with it.
fn frame_end(buf: &[u8]) -> Option<usize> {
    if buf.starts_with(b"8=FIXCOMP\x01") {
        return super::fixcomp::fixcomp_length(buf);
    }
    // Where the length says, and only where the trailer is there to confirm
    // it. Scanned for instead, the first `<SOH>10=` in the buffer ended the
    // frame — and a signature, a reject reason or any length-prefixed block
    // may carry those four bytes inside its value, so a whole frame was cut
    // at the first one and its tail was read as the head of the next.
    let total = super::connection::fix_msg_length(buf)?;
    if buf.len() < total || !super::connection::trailer_is_where_the_length_says(buf, total) {
        return None;
    }
    Some(total)
}

/// Read one complete FIX message, tolerating transient read timeouts.
///
/// The caller holds the buffer, and holds it across every call it makes on one
/// socket. A read takes whatever the peer has already sent, and this peer
/// sends messages back to back: the bytes past the end of one message are the
/// start of the next, so a reader that returned the first message and dropped
/// the rest would take them off the socket and lose them. What that costs is
/// not abstract — the routing tags, the account, and the venue's stamp on the
/// logon all arrive in the messages that follow the first one.
///
/// A `WouldBlock`/`TimedOut` read (a poll-timeout expiry on a slow,
/// high-latency venue) is retried until `deadline` rather than being treated
/// as fatal. The reader's socket should carry a short read timeout so the poll
/// stays responsive.
pub fn fix_read_deadline<R: Read>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    deadline: std::time::Instant,
) -> io::Result<Vec<u8>> {
    let mut tmp = [0u8; 4096];
    loop {
        if let Some(total) = frame_end(buf) {
            return Ok(buf.drain(..total).collect());
        }
        // Before the read, not only where a read came back empty. Tested only
        // there, a peer that keeps sending bytes which never finish a frame is
        // never once measured against the deadline: every read succeeds, the
        // arm that checks is never taken, and this waits for as long as the
        // bytes keep coming while what has arrived grows without bound. It runs
        // on every logon and on every reconnect.
        // And the same for what has arrived. The deadline bounds how long this
        // waits and not how much it holds, so a peer sending at line rate for
        // the length of a logon is bounded in seconds and unbounded in bytes.
        // The socket reader on the other side of this module refuses past the
        // same figure, for the same reason.
        if buf.len() > super::connection::MAX_BUFFERED {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "a logon reply grew past {} bytes without finishing a message",
                    super::connection::MAX_BUFFERED,
                ),
            ));
        }
        if std::time::Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "timed out reading FIX message",
            ));
        }
        match reader.read(&mut tmp) {
            Ok(0) => return Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "socket closed while reading FIX message",
            )),
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(e) if super::connection::read_found_nothing(&e) => {
                // Nothing came. The deadline is measured at the top of the
                // loop, so this waits for the next poll.
            }
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    /// A field value carries the characters this wire carries.
    ///
    /// A name a caller gave in its own script went out as the two or three
    /// bytes its script needs, where the venue reads one character and no field
    /// value on this wire holds a byte above ASCII at all — so the field the
    /// venue read was not the field the caller stated, and the length written
    /// in front of it counted bytes the venue does not count.
    #[test]
    fn a_field_value_carries_the_characters_this_wire_carries() {
        let msg = super::fix_build(&[(super::TAG_MSG_TYPE, "D"), (6010, "Caf\u{e9} x")], 1);
        let text = String::from_utf8_lossy(&msg);
        assert!(text.contains("6010=Caf* x\u{1}"), "one star for the one character: {text}");
        assert!(
            msg.iter().all(|b| *b < 0x80),
            "and nothing above ASCII reaches the wire",
        );
    }

    use super::*;

    // A reader that fails `blocks_left` times with `kind` before yielding its
    // bytes — models a slow socket with a short read timeout.
    struct SlowReader { blocks_left: u32, kind: io::ErrorKind, data: std::io::Cursor<Vec<u8>> }
    impl Read for SlowReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.blocks_left > 0 {
                self.blocks_left -= 1;
                return Err(io::Error::new(self.kind, "nothing yet"));
            }
            self.data.read(buf)
        }
    }

    #[test]
    fn fix_read_deadline_retries_wouldblock_then_reads() {
        let msg = fix_build(&[(35, "A"), (108, "30")], 1);
        let mut reader = SlowReader {
            blocks_left: 3,
            kind: io::ErrorKind::WouldBlock,
            data: std::io::Cursor::new(msg.clone()),
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let got = fix_read_deadline(&mut reader, &mut Vec::new(), deadline).unwrap();
        assert_eq!(got, msg);
    }

    // The logon socket carries a read timeout, so a signal landing in the read
    // comes back interrupted instead of being restarted. Read as a failure it
    // fails the logon over something the peer never did.
    #[test]
    fn fix_read_deadline_retries_a_signal_then_reads() {
        let msg = fix_build(&[(35, "A"), (108, "30")], 1);
        let mut reader = SlowReader {
            blocks_left: 3,
            kind: io::ErrorKind::Interrupted,
            data: std::io::Cursor::new(msg.clone()),
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let got = fix_read_deadline(&mut reader, &mut Vec::new(), deadline).unwrap();
        assert_eq!(got, msg);
    }

    #[test]
    fn fix_read_deadline_times_out_past_deadline() {
        let mut reader = SlowReader {
            blocks_left: u32::MAX,
            kind: io::ErrorKind::WouldBlock,
            data: std::io::Cursor::new(Vec::new()),
        };
        // Deadline already passed: the first WouldBlock must surface as TimedOut.
        let deadline = std::time::Instant::now() - std::time::Duration::from_secs(1);
        let err = fix_read_deadline(&mut reader, &mut Vec::new(), deadline).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    }

    /// The auth ACK arrives compressed and carries no checksum, so the reader
    /// has to take its length from tag 9. Framed by a checksum scan it never
    /// completes at all, and the logon times out waiting for a message that had
    /// already arrived in full.
    #[test]
    fn a_compressed_frame_is_read_whole_and_alone() {
        let inner = fix_build(&[(35, "A"), (108, "30")], 1);
        let comp = crate::protocol::fixcomp::fixcomp_build(&inner);
        let mut reader = std::io::Cursor::new(comp.clone());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let got = fix_read_deadline(&mut reader, &mut Vec::new(), deadline).unwrap();
        assert_eq!(got, comp);
    }

    /// And the message behind it is still there to be read, rather than having
    /// been swallowed as part of the compressed one.
    #[test]
    fn a_message_behind_a_compressed_frame_survives_it() {
        let comp = crate::protocol::fixcomp::fixcomp_build(&fix_build(&[(35, "A")], 1));
        let plain = fix_build(&[(35, "0")], 2);
        let mut wire = comp.clone();
        wire.extend_from_slice(&plain);
        let mut reader = std::io::Cursor::new(wire);
        let mut carry = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        assert_eq!(fix_read_deadline(&mut reader, &mut carry, deadline).unwrap(), comp);
        assert_eq!(fix_read_deadline(&mut reader, &mut carry, deadline).unwrap(), plain);
    }

    /// Signing rebuilds the header, and writes the length it now knows plain
    /// — whichever header it is.
    ///
    /// Only the unsigned build pads: it reserves four bytes before it knows
    /// the length and back-fills them. A signed frame is composed again from
    /// the length in hand, and the signer this follows pads nothing. Padded
    /// here for consistency with the unsigned build, every signed frame left
    /// two bytes longer than the venue is sent elsewhere, with a different
    /// checksum behind it.
    #[test]
    fn signing_writes_the_body_length_plain() {
        let iv = [7u8; 16];
        for frame in [
            fix_build(&[(35, "0")], 1),
            crate::protocol::fixcomp::fixcomp_build(&fix_build(&[(35, "0")], 1)),
        ] {
            let (signed, _) = fix_sign(&frame, b"key0000000000000", &iv);
            let at = signed.windows(2).position(|w| w == b"9=").expect("a tag 9") + 2;
            let end = at + signed[at..].iter().position(|&b| b == SOH).expect("its delimiter");
            assert_ne!(signed[at], b'0', "a length written plain has no leading nought");
            assert!(end > at);
            // And the length is the body that follows it.
            let stated: usize = std::str::from_utf8(&signed[at..end]).unwrap().parse().unwrap();
            let body = &signed[end + 1..];
            let body_end = match signed.starts_with(b"8=FIX.4.1") {
                // The checksum field is not part of the body the length names.
                true => body.len() - 7,
                false => body.len(),
            };
            assert_eq!(stated, body_end, "the length names the body behind it");
        }
    }

    /// A body longer than the padding keeps its high digits. Dropped, the field
    /// names a shorter body than the one that follows it.
    #[test]
    fn a_length_too_long_to_pad_keeps_every_digit() {
        let mut out = Vec::new();
        push_u32_padded::<4>(&mut out, 10_500);
        assert_eq!(out, b"10500");
        let mut out = Vec::new();
        push_u32_padded::<4>(&mut out, 52);
        assert_eq!(out, b"0052");
    }

    #[test]
    fn checksum_abc() {
        assert_eq!(fix_checksum(b"abc"), format!("{:03}", (97 + 98 + 99) % 256));
    }

    #[test]
    fn checksum_zero_padded() {
        assert_eq!(fix_checksum(&[0x01]), "001");
    }

    #[test]
    fn build_structure() {
        let msg = fix_build(&[(35, "A"), (108, "10")], 1);
        let parsed = fix_parse(&msg);
        assert_eq!(parsed[&8], "FIX.4.1");
        assert!(parsed.contains_key(&9));
        assert_eq!(parsed[&34], "000001");
        assert!(parsed.contains_key(&10));
    }

    #[test]
    fn build_body_length() {
        let msg = fix_build(&[(35, "0")], 5);
        let text = String::from_utf8_lossy(&msg);
        let idx9 = text.find("9=").unwrap() + 2;
        let soh9 = text[idx9..].find('\x01').unwrap() + idx9;
        let body_len: usize = text[idx9..soh9].parse().unwrap();
        let body_start = soh9 + 1;
        let idx10 = text.rfind("\x0110=").unwrap();
        let actual_body = &msg[body_start..idx10 + 1];
        assert_eq!(body_len, actual_body.len());
    }

    #[test]
    fn build_parse_roundtrip() {
        let msg = fix_build(
            &[(35, "D"), (11, "12345"), (55, "AAPL"), (54, "1"), (38, "100")],
            42,
        );
        let parsed = fix_parse(&msg);
        assert_eq!(parsed[&35], "D");
        assert_eq!(parsed[&11], "12345");
        assert_eq!(parsed[&55], "AAPL");
        assert_eq!(parsed[&54], "1");
        assert_eq!(parsed[&38], "100");
        assert_eq!(parsed[&34], "000042");
    }

    #[test]
    fn parse_empty_value() {
        let msg = fix_build(&[(35, "A"), (58, "")], 1);
        let parsed = fix_parse(&msg);
        assert_eq!(parsed[&58], "");
    }

    #[test]
    fn xor_fold_sequential() {
        let data: Vec<u8> = (0u8..20).collect();
        let result = xor_fold(&data);
        assert_eq!(result.len(), 8);
        assert_eq!(result, result.to_uppercase());
        let mut r = [0u8; 4];
        for off in (0..20).step_by(4) {
            for i in 0..4 {
                r[i] ^= data[off + i];
            }
        }
        let expected: String = r.iter().map(|b| format!("{b:02X}")).collect();
        assert_eq!(result, expected);
    }

    #[test]
    fn sign_unsign_roundtrip() {
        let mac_key: Vec<u8> = (0..20).collect();
        let iv: Vec<u8> = (0..16).collect();
        let msg = fix_build(
            &[(35, "D"), (55, "AAPL"), (54, "1"), (38, "100")],
            1,
        );
        let (signed, new_iv) = fix_sign(&msg, &mac_key, &iv);
        let (_, new_iv2, valid) = fix_unsign(&signed, &mac_key, &iv);
        assert!(valid);
        assert_eq!(new_iv, new_iv2);
    }

    #[test]
    fn sign_iv_chains() {
        let mac_key: Vec<u8> = (0..20).collect();
        let iv: Vec<u8> = (0..16).collect();
        let msg1 = fix_build(&[(35, "0")], 1);
        let (_, iv1) = fix_sign(&msg1, &mac_key, &iv);
        let msg2 = fix_build(&[(35, "0")], 2);
        let (_, iv2) = fix_sign(&msg2, &mac_key, &iv1);
        assert_ne!(iv, iv1);
        assert_ne!(iv1, iv2);
    }

    #[test]
    fn sign_wrong_key_fails() {
        let mac_key: Vec<u8> = (0..20).collect();
        let wrong_key: Vec<u8> = (20..40).collect();
        let iv: Vec<u8> = (0..16).collect();
        let msg = fix_build(&[(35, "0")], 1);
        let (signed, _) = fix_sign(&msg, &mac_key, &iv);
        let (_, _, valid) = fix_unsign(&signed, &wrong_key, &iv);
        assert!(!valid);
    }

    #[test]
    fn fix_parse_empty_value_raw() {
        // "35=\x01" should parse tag 35 with empty string
        let data = b"35=\x01";
        let parsed = fix_parse(data);
        assert_eq!(parsed.get(&35), Some(&String::new()));
    }

    #[test]
    fn fix_parse_non_fix_prefix() {
        // Random bytes before valid tag=value pairs should still parse
        let data = b"\xDE\xAD\xBE\xEF\x0155=AAPL\x0154=1\x01";
        let parsed = fix_parse(data);
        assert_eq!(parsed.get(&55), Some(&"AAPL".to_string()));
        assert_eq!(parsed.get(&54), Some(&"1".to_string()));
    }

    #[test]
    fn fix_build_many_tags_all_present() {
        let fields: Vec<(u32, &str)> = vec![
            (35, "D"),
            (11, "CLO001"),
            (55, "MSFT"),
            (54, "2"),
            (38, "500"),
            (40, "2"),
            (44, "350.50"),
            (59, "0"),
        ];
        let msg = fix_build(&fields, 10);
        let parsed = fix_parse(&msg);
        for (tag, val) in &fields {
            assert_eq!(
                parsed.get(tag),
                Some(&val.to_string()),
                "tag {tag} missing or wrong"
            );
        }
        // Auto-generated tags must also be present
        assert!(parsed.contains_key(&8));
        assert!(parsed.contains_key(&9));
        assert!(parsed.contains_key(&10));
        assert!(parsed.contains_key(&34));
    }

    #[test]
    fn fix_build_seq_zero_pads_to_6_digits() {
        let msg1 = fix_build(&[(35, "0")], 1);
        let parsed1 = fix_parse(&msg1);
        assert_eq!(parsed1[&34], "000001");

        let msg2 = fix_build(&[(35, "0")], 999999);
        let parsed2 = fix_parse(&msg2);
        assert_eq!(parsed2[&34], "999999");
    }

    #[test]
    fn fix_checksum_wraps_at_256() {
        // 256 bytes of value 1 → sum = 256, mod 256 = 0
        let data = vec![1u8; 256];
        assert_eq!(fix_checksum(&data), "000");

        // 255 bytes of value 1 → sum = 255, mod 256 = 255
        let data2 = vec![1u8; 255];
        assert_eq!(fix_checksum(&data2), "255");

        // 257 bytes of value 1 → sum = 257, mod 256 = 1
        let data3 = vec![1u8; 257];
        assert_eq!(fix_checksum(&data3), "001");
    }

    #[test]
    fn sign_unsign_iv_chaining_three_messages() {
        let mac_key: Vec<u8> = (0..20).collect();
        let iv: Vec<u8> = (0..16).collect();

        let msg1 = fix_build(&[(35, "D"), (55, "AAPL")], 1);
        let msg2 = fix_build(&[(35, "0")], 2);
        let msg3 = fix_build(&[(35, "A"), (108, "10")], 3);

        // Sign sequentially
        let (signed1, iv_after1) = fix_sign(&msg1, &mac_key, &iv);
        let (signed2, iv_after2) = fix_sign(&msg2, &mac_key, &iv_after1);
        let (signed3, iv_after3) = fix_sign(&msg3, &mac_key, &iv_after2);

        // Unsign in same order with same initial IV
        let (_, unsign_iv1, valid1) = fix_unsign(&signed1, &mac_key, &iv);
        assert!(valid1, "message 1 signature invalid");
        let (_, unsign_iv2, valid2) = fix_unsign(&signed2, &mac_key, &unsign_iv1);
        assert!(valid2, "message 2 signature invalid");
        let (_, unsign_iv3, valid3) = fix_unsign(&signed3, &mac_key, &unsign_iv2);
        assert!(valid3, "message 3 signature invalid");

        // IVs must match between sign and unsign chains
        assert_eq!(iv_after1, unsign_iv1);
        assert_eq!(iv_after2, unsign_iv2);
        assert_eq!(iv_after3, unsign_iv3);
    }

    #[test]
    fn unsign_wrong_iv_returns_invalid() {
        let mac_key: Vec<u8> = (0..20).collect();
        let iv: Vec<u8> = (0..16).collect();
        let wrong_iv: Vec<u8> = (16..32).collect();

        let msg = fix_build(&[(35, "D"), (55, "AAPL")], 1);
        let (signed, _) = fix_sign(&msg, &mac_key, &iv);

        let (_, _, valid) = fix_unsign(&signed, &mac_key, &wrong_iv);
        assert!(!valid);
    }

    #[test]
    fn unsign_empty_key_returns_original() {
        // When mac_key is empty, HMAC-SHA1 still works (empty key is valid).
        // The test verifies that sign → unsign with the same empty key is valid.
        let mac_key: Vec<u8> = vec![];
        let iv: Vec<u8> = (0..16).collect();

        let msg = fix_build(&[(35, "0")], 1);
        let (signed, _) = fix_sign(&msg, &mac_key, &iv);
        let (_, _, valid) = fix_unsign(&signed, &mac_key, &iv);
        assert!(valid);
    }

    // ── reading ───────────────────────────────────────────────────────

    fn read_one(reader: &mut impl Read, buf: &mut Vec<u8>) -> io::Result<Vec<u8>> {
        fix_read_deadline(reader, buf, std::time::Instant::now()
            + std::time::Duration::from_secs(5))
    }

    #[test]
    fn one_message_is_read_whole() {
        let msg = fix_build(&[(35, "A"), (108, "30"), (98, "0")], 1);
        let mut cursor = std::io::Cursor::new(msg.clone());
        let mut buf = Vec::new();
        let got = read_one(&mut cursor, &mut buf).unwrap();
        assert_eq!(got, msg);
        assert!(buf.is_empty(), "nothing was left over");
        assert_eq!(fix_parse(&got)[&108], "30");
    }

    /// A peer that sends two messages back to back puts both in one read, and
    /// the second one is not the reader's to drop.
    #[test]
    fn a_second_message_in_the_same_read_is_kept() {
        let msg1 = fix_build(&[(35, "0")], 1);
        let msg2 = fix_build(&[(35, "D"), (55, "AAPL")], 2);
        let mut wire = msg1.clone();
        wire.extend_from_slice(&msg2);
        let mut cursor = std::io::Cursor::new(wire);
        let mut buf = Vec::new();

        assert_eq!(read_one(&mut cursor, &mut buf).unwrap(), msg1);
        assert_eq!(read_one(&mut cursor, &mut buf).unwrap(), msg2);
        assert!(buf.is_empty());
    }

    /// And the bytes of a message split across two reads are one message.
    #[test]
    fn a_message_split_across_reads_is_read_whole() {
        let msg = fix_build(&[(35, "A"), (108, "30")], 1);
        let (head, tail) = msg.split_at(msg.len() / 2);
        let mut reader = std::io::Cursor::new(head.to_vec())
            .chain(std::io::Cursor::new(tail.to_vec()));
        let mut buf = Vec::new();
        assert_eq!(read_one(&mut reader, &mut buf).unwrap(), msg);
    }

    #[test]
    fn a_closed_socket_is_an_error() {
        let mut cursor = std::io::Cursor::new(Vec::<u8>::new());
        let mut buf = Vec::new();
        let err = read_one(&mut cursor, &mut buf).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::ConnectionReset);
    }

    #[test]
    fn a_message_cut_short_by_a_close_is_an_error() {
        let partial = b"8=FIX.4.1\x019=0010\x0135=0\x01".to_vec();
        let mut cursor = std::io::Cursor::new(partial);
        let mut buf = Vec::new();
        let err = read_one(&mut cursor, &mut buf).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::ConnectionReset);
    }

    // ── fix_unsign edge case tests ────────────────────────────────────

    #[test]
    fn unsign_empty_data_no_panic() {
        let mac_key: Vec<u8> = (0..20).collect();
        let iv: Vec<u8> = (0..16).collect();
        let (_, _, valid) = fix_unsign(&[], &mac_key, &iv);
        assert!(!valid);
    }

    #[test]
    fn unsign_short_data_no_panic() {
        let mac_key: Vec<u8> = (0..20).collect();
        let iv: Vec<u8> = (0..16).collect();
        let (_, _, valid) = fix_unsign(b"8=FIX.4.1\x01", &mac_key, &iv);
        assert!(!valid);
    }

    #[test]
    fn unsign_tiny_fixcomp_no_panic() {
        let mac_key: Vec<u8> = (0..20).collect();
        let iv: Vec<u8> = (0..16).collect();
        let (_, _, valid) = fix_unsign(b"8=FIXCOMP\x01", &mac_key, &iv);
        assert!(!valid);
    }

    /// A signature that is not hexadecimal is not a signature.
    ///
    /// Every expected byte whose top half is `F` was matched by text carrying
    /// any character at all in that position, because the invalid nibble's
    /// `0xFF` was truncated to `F` by the shift that placed it.
    #[test]
    fn a_signature_that_is_not_hexadecimal_does_not_match() {
        let expected = [0xF5u8, 0xFA, 0xFB, 0xFC];
        assert!(sig_hex_eq(&expected, b"F5FAFBFC"), "the signature itself matches");
        assert!(sig_hex_eq(&expected, b"f5fafbfc"), "and matches in either case");
        for bad in [
            &b"G5FAFBFC"[..], &b"F5ZAFBFC"[..], &b"F5FA.BFC"[..], &b"F5FAFB C"[..],
        ] {
            assert!(
                !sig_hex_eq(&expected, bad),
                "{} is not hexadecimal and must not match",
                String::from_utf8_lossy(bad),
            );
        }
    }

    #[test]
    fn xor_fold_all_zero_digest() {
        let digest = [0u8; 20];
        let result = xor_fold(&digest);
        assert_eq!(result, "00000000");
    }

    #[test]
    fn xor_fold_known_values() {
        // Manually compute: 5 groups of 4 bytes each, XOR-folded
        // Group 0: [0x01, 0x02, 0x03, 0x04]
        // Group 1: [0x10, 0x20, 0x30, 0x40]
        // Group 2: [0x05, 0x06, 0x07, 0x08]
        // Group 3: [0xFF, 0x00, 0xFF, 0x00]
        // Group 4: [0xAA, 0xBB, 0xCC, 0xDD]
        let digest: [u8; 20] = [
            0x01, 0x02, 0x03, 0x04, // group 0
            0x10, 0x20, 0x30, 0x40, // group 1
            0x05, 0x06, 0x07, 0x08, // group 2
            0xFF, 0x00, 0xFF, 0x00, // group 3
            0xAA, 0xBB, 0xCC, 0xDD, // group 4
        ];
        let r0 = 0x01 ^ 0x10 ^ 0x05 ^ 0xFF ^ 0xAA;
        let r1 = (0x02 ^ 0x20 ^ 0x06) ^ 0xBB;
        let r2 = 0x03 ^ 0x30 ^ 0x07 ^ 0xFF ^ 0xCC;
        let r3 = (0x04 ^ 0x40 ^ 0x08) ^ 0xDD;
        let expected = format!("{r0:02X}{r1:02X}{r2:02X}{r3:02X}");
        assert_eq!(xor_fold(&digest), expected);
    }
}

#[cfg(test)]
mod hostile_frame_tests {
    use super::*;

    /// A peer that keeps sending, and never finishes a frame, is given up on.
    ///
    /// The deadline was measured only where a read came back empty, so bytes
    /// that never complete a frame were never measured against it at all: every
    /// read succeeded, the arm that checked was never taken, and this waited for
    /// as long as the bytes kept coming while what had arrived grew without
    /// bound. It runs on every logon and on every reconnect.
    #[test]
    fn a_peer_that_never_finishes_a_frame_is_given_up_on() {
        /// Always has one more byte, and never a whole frame.
        struct Dribbles;
        impl std::io::Read for Dribbles {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                buf[0] = b'8';
                Ok(1)
            }
        }

        let mut buf = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(50);
        let out = fix_read_deadline(&mut Dribbles, &mut buf, deadline);

        let err = out.expect_err("a frame that never ends is not a frame");
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
    }

    /// A frame whose only `9=` lies inside the signature field is refused
    /// rather than panicking.
    ///
    /// The bare needle matches at offset 3 of `8349=`, so the body would be
    /// sliced from after that to before it. A panic on the reader thread ends
    /// the session and every subscription on it, which is what this protocol
    /// module exists to prevent.
    #[test]
    fn a_frame_whose_length_field_sits_inside_the_signature_is_refused() {
        let hostile = b"8=FIX.4.1\x018349=00000000\x01";
        let (_, _, valid) = fix_unsign(hostile, &[0u8; 20], &[0u8; 16]);
        assert!(!valid, "a frame this shape must be refused, not accepted");
    }

    /// A raw block is one field's value, not a run of fields: what it carries
    /// is not walked. A block quoting the signature's own text does not stand
    /// in for the signature, and the field it came in arrives whole rather
    /// than cut at its first delimiter with the rest of it injected as tags.
    #[test]
    fn a_raw_block_is_read_whole_and_not_walked_as_fields() {
        let payload = b"row1\x0158=injected\x018349=DEADBEEF\x01more";
        let mut body = Vec::new();
        body.extend_from_slice(b"35=U\x016118=<R/>\x01");
        body.extend_from_slice(format!("95={}\x01", payload.len()).as_bytes());
        body.extend_from_slice(b"96=");
        body.extend_from_slice(payload);
        body.push(SOH);

        let mut msg = Vec::new();
        msg.extend_from_slice(b"8=FIX.4.1\x019=0000\x01");
        msg.extend_from_slice(&body);
        msg.extend_from_slice(b"10=000\x01");

        let iv = [7u8; 16];
        let (signed, _) = fix_sign(&msg, b"key0000000000000", &iv);
        let (unsigned, _, valid) = fix_unsign(&signed, b"key0000000000000", &iv);
        assert!(valid, "a signature quoted inside a raw block is not one");

        let parsed = fix_parse(&unsigned);
        assert_eq!(
            parsed.get(&96).map(|s| s.as_bytes()),
            Some(&payload[..]),
            "the block arrives whole",
        );
        assert!(!parsed.contains_key(&58), "what a block carries is not fields");
    }

    /// A signature the walk cannot reach is still a signature.
    ///
    /// The distortion covers the body and stops short of the trailer, so a
    /// signature arrives as it was written. What does not survive is the walk
    /// to it: a length-prefixed block states how far to step, that length sits
    /// in the distorted span, and one flipped digit steps the walk past the
    /// trailer and off the end. Classified only that way the frame was called
    /// unsigned and handed on still distorted with the chain not advanced, and
    /// every signed frame after it verified against the wrong state.
    #[test]
    fn a_signature_the_walk_steps_over_is_still_found() {
        let mut frame = Vec::new();
        frame.extend_from_slice(b"8=FIX.4.1\x019=0042\x0135=8\x0195=94\x0196=abcd\x01");
        frame.extend_from_slice(b"8349=DEADBEEF\x01");
        frame.extend_from_slice(b"10=000\x01");

        assert!(
            !Fields::new(&frame).any(|(tag, ..)| tag == TAG_HMAC_SIGNATURE),
            "the walk is meant to step over the trailer here; without that this \
             test proves nothing",
        );
        assert!(
            carries_signature(&frame),
            "a frame carrying a signature was handed on unverified",
        );
    }

    /// A checksum inside a length-prefixed block does not end the frame.
    ///
    /// Tag 95 states how much follows under tag 96 and the block is bytes,
    /// separators and all. Framed by a scan for the first `<SOH>10=`, a frame
    /// carrying one was cut at whatever its payload happened to contain and
    /// its tail was read as the head of the next.
    #[test]
    fn a_block_that_carries_a_checksum_does_not_end_the_frame() {
        let frame = fix_build(&[(35, "8"), (95, "10"), (96, "A\x0110=000\x01B")], 1);
        assert_eq!(
            frame_end(&frame),
            Some(frame.len()),
            "the frame was cut at a checksum inside the block it carries",
        );
    }

    /// A quoted signature at the trailer's own offset is not a signature.
    ///
    /// A reject reason can carry the tag, eight characters that read as hex,
    /// and end where the separator is expected — every part of the shape, at
    /// exactly the offset a real one occupies. What it cannot do is begin a
    /// field. Taken for one on a signed connection, the frame went to a verify
    /// that found no signature field, and a failed verify gives the transport
    /// up: the frame is dropped and the session rebuilt, on a message that was
    /// never signed.
    #[test]
    fn a_signature_quoted_inside_a_value_is_not_one() {
        let frame = fix_build(&[(35, "3"), (58, "rejected: 8349=DEADBEEF")], 1);
        assert!(
            !carries_signature(&frame),
            "an unsigned frame was sent to a verify that can only fail it",
        );
    }
}
