//! Binary tick decoder for market data messages.
//!
//! Decodes bid/ask/last/size/volume fields from IB's proprietary binary format.
//! Also includes VLQ and hi-bit string decoders for 35=E tick-by-tick data,
//! and the RTBAR decoder for 35=G real-time bar data.

/// MSB-first bit-level reader for 8=O 35=P binary tick data.
pub struct BitReader<'a> {
    data: &'a [u8],
    bit_pos: usize,
    total_bits: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8], total_bits: usize) -> Self {
        let max_bits = data.len() * 8;
        let total_bits = if total_bits > 0 {
            total_bits.min(max_bits)
        } else {
            max_bits
        };
        Self {
            data,
            bit_pos: 0,
            total_bits,
        }
    }

    pub fn remaining(&self) -> usize {
        self.total_bits.saturating_sub(self.bit_pos)
    }

    /// Read n bits as unsigned integer (MSB first).
    /// Uses word-aligned reads for performance (1-3 ops instead of n iterations).
    #[inline]
    pub fn read_unsigned(&mut self, n: usize) -> Option<u64> {
        if n == 0 {
            return Some(0);
        }
        if n > 64 || self.bit_pos + n > self.total_bits {
            return None;
        }
        let byte_idx = self.bit_pos >> 3;
        let bit_offset = self.bit_pos & 7;
        self.bit_pos += n;

        // Total bits we need from the byte stream: bit_offset + n.
        // If <= 64, one u64 load suffices. If > 64 (cross-word), load two words.
        let needed = bit_offset + n;

        let remaining_bytes = self.data.len() - byte_idx;

        if needed <= 64 {
            // Fast path: single word load
            let word = if remaining_bytes >= 8 {
                u64::from_be_bytes(self.data[byte_idx..byte_idx + 8].try_into().unwrap())
            } else {
                let mut buf = [0u8; 8];
                buf[..remaining_bytes].copy_from_slice(&self.data[byte_idx..]);
                u64::from_be_bytes(buf)
            };
            let result = (word << bit_offset) >> (64 - n);
            Some(result)
        } else {
            // Cross-word: need bits from two consecutive u64s
            let load_be = |off: usize| -> u64 {
                let rem = self.data.len().saturating_sub(off);
                if rem >= 8 {
                    u64::from_be_bytes(self.data[off..off + 8].try_into().unwrap())
                } else if rem > 0 {
                    let mut buf = [0u8; 8];
                    buf[..rem].copy_from_slice(&self.data[off..off + rem]);
                    u64::from_be_bytes(buf)
                } else {
                    0
                }
            };
            let hi = load_be(byte_idx);
            let lo = load_be(byte_idx + 8);
            // Combine: take (64 - bit_offset) bits from hi, then (n - (64 - bit_offset)) from lo
            let hi_bits = 64 - bit_offset; // bits available in hi after discarding offset
            let lo_bits = n - hi_bits;
            let result = ((hi << bit_offset) >> (64 - n)) | (lo >> (64 - lo_bits));
            Some(result)
        }
    }
}

/// LSB-first bit-level reader for 35=G real-time bar data.
/// Uses word-aligned u64 loads (1-2 ops per field instead of n iterations per bit).
struct LsbBitReader<'a> {
    data: &'a [u8],
    bit_pos: usize,
    total_bits: usize,
}

impl<'a> LsbBitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            bit_pos: 0,
            total_bits: data.len() * 8,
        }
    }

    /// Read n bits as unsigned integer (LSB first).
    /// Uses word-aligned reads: 1-2 u64 loads instead of n bit iterations.
    #[inline]
    fn read(&mut self, n: usize) -> u64 {
        if n == 0 || self.bit_pos + n > self.total_bits {
            return 0;
        }
        let byte_idx = self.bit_pos >> 3;
        let bit_offset = self.bit_pos & 7;
        self.bit_pos += n;

        let remaining_bytes = self.data.len() - byte_idx;

        // Load up to 8 bytes as a little-endian u64
        let word = if remaining_bytes >= 8 {
            u64::from_le_bytes(self.data[byte_idx..byte_idx + 8].try_into().unwrap())
        } else {
            let mut buf = [0u8; 8];
            buf[..remaining_bytes].copy_from_slice(&self.data[byte_idx..]);
            u64::from_le_bytes(buf)
        };

        let needed = bit_offset + n;
        if needed <= 64 {
            // Fast path: single word
            (word >> bit_offset) & ((1u64 << n) - 1)
        } else {
            // Cross-word boundary
            let lo_bits = 64 - bit_offset;
            let hi_bits = n - lo_bits;
            let lo = word >> bit_offset;
            let hi_word = if byte_idx + 8 < self.data.len() {
                let rem = self.data.len() - (byte_idx + 8);
                if rem >= 8 {
                    u64::from_le_bytes(self.data[byte_idx + 8..byte_idx + 16].try_into().unwrap())
                } else {
                    let mut buf = [0u8; 8];
                    buf[..rem].copy_from_slice(&self.data[byte_idx + 8..]);
                    u64::from_le_bytes(buf)
                }
            } else {
                0
            };
            lo | ((hi_word & ((1u64 << hi_bits) - 1)) << lo_bits)
        }
    }
}

// Binary tick type IDs, as they come off the wire.
pub const O_BID_PRICE: u64 = 0;
pub const O_ASK_PRICE: u64 = 1;
pub const O_LAST_PRICE: u64 = 2;
pub const O_CLOSE_PRICE: u64 = 3;
pub const O_BID_SIZE: u64 = 4;
pub const O_ASK_SIZE: u64 = 5;
pub const O_LAST_SIZE: u64 = 6;
/// The bid and ask auto-execution bits of a quote.
pub const O_AUTO_EXEC: u64 = 7;
pub const O_HIGH_PRICE: u64 = 8;
pub const O_LOW_PRICE: u64 = 9;
pub const O_VOLUME: u64 = 10;
/// On a quote, attribute bits with the auto-execution bits; on a trade,
/// its trading status.
pub const O_ATTRIBUTES: u64 = 13;
pub const O_BID_EXCH: u64 = 16;
pub const O_ASK_EXCH: u64 = 17;
pub const O_HALTED: u64 = 18;
/// Last trade time base; the close date on a daily-stats block.
pub const O_TIMESTAMP_BASE: u64 = 20;
/// Added to the base for the last trade time.
pub const O_TIMESTAMP_DELTA: u64 = 21;
pub const O_OPEN_PRICE: u64 = 22;
pub const O_LAST_EXCH: u64 = 27;

/// Volume multiplier: IB encodes volume * 10000.
pub const VOLUME_MULT: f64 = 0.0001;

/// A single decoded tick from a 35=P message.
#[derive(Debug, Clone, Copy)]
pub struct RawTick {
    pub server_tag: u32,
    pub tick_type: u64,
    pub magnitude: i64,
    /// The tick comes from a daily-stats block.
    pub stats_block: bool,
    /// The tick is the first of its block.
    pub first: bool,
}

/// Decode all ticks from a 35=P binary payload.
///
/// `body` is the raw message body after stripping FIX framing and HMAC signature.
/// Returns a list of raw ticks with server_tag, tick_type, and signed magnitude.
pub fn decode_ticks_35p(body: &[u8]) -> Vec<RawTick> {
    let mut ticks = Vec::with_capacity(8);
    decode_ticks_35p_into(body, &mut ticks);
    ticks
}

/// Widest tick value ibx accepts: the widest that still fits an `i64`;
/// a wider one is an oversized value (ibx#272).
pub const MAX_VALUE_BYTES: u64 = 8;

/// Decode ticks into a caller-supplied buffer (avoids heap allocation on hot path).
///
/// A malformed block (cut short, or with an oversized value) is dropped
/// with all its ticks and decoding stops,
/// as the reference ends its loop on such a block; the ticks of the blocks
/// before it are kept. Returns true when a block was dropped (ibx#272).
pub fn decode_ticks_35p_into(body: &[u8], ticks: &mut Vec<RawTick>) -> bool {
    ticks.clear();
    if body.len() < 4 {
        return false;
    }

    let bit_count = ((body[0] as usize) << 8) | (body[1] as usize);
    let payload = &body[2..];
    let mut reader = BitReader::new(payload, bit_count);

    while reader.remaining() > 32 {
        let block_start = ticks.len();
        let stats_block = match reader.read_unsigned(1) {
            Some(v) => v == 1,
            None => break,
        };
        let server_tag = match reader.read_unsigned(31) {
            Some(v) => v as u32,
            None => break,
        };

        let mut has_more = 1u64;
        // The type of the block's last entry: on a quote tag, a time base
        // after a close is the close date, not a time (`jmdclient.bl.a(...)`
        // types 20/21 of a quote block, `@2611-2900`; captured 05/10/2026,
        // 7203 on delayed data).
        let mut prev_type: Option<u64> = None;
        while has_more == 1 {
            if reader.remaining() < 8 {
                // The block announced one more entry that is not there.
                ticks.truncate(block_start);
                return true;
            }
            let tick_type;
            let byte_width;

            let raw_tick_type = match reader.read_unsigned(5) {
                Some(v) => v,
                None => break,
            };
            has_more = match reader.read_unsigned(1) {
                Some(v) => v,
                None => break,
            };
            let raw_width = match reader.read_unsigned(2) {
                Some(v) => v + 1,
                None => break,
            };

            if raw_tick_type == 31 {
                // Extended format
                if reader.remaining() < 16 {
                    ticks.truncate(block_start);
                    return true;
                }
                tick_type = match reader.read_unsigned(8) {
                    Some(v) => v,
                    None => { ticks.truncate(block_start); return true; }
                };
                byte_width = match reader.read_unsigned(8) {
                    Some(v) => v,
                    None => { ticks.truncate(block_start); return true; }
                };
            } else {
                tick_type = raw_tick_type;
                byte_width = raw_width;
            }

            let total_value_bits = (8 * byte_width) as usize;
            if byte_width == 0 || byte_width > MAX_VALUE_BYTES || reader.remaining() < total_value_bits {
                ticks.truncate(block_start);
                return true;
            }

            let sign = match reader.read_unsigned(1) {
                Some(v) => v,
                None => { ticks.truncate(block_start); return true; }
            };
            let magnitude_unsigned = match reader.read_unsigned(total_value_bits - 1) {
                Some(v) => v as i64,
                None => { ticks.truncate(block_start); return true; }
            };

            let magnitude = if sign == 1 {
                -magnitude_unsigned
            } else {
                magnitude_unsigned
            };

            ticks.push(RawTick {
                server_tag,
                tick_type,
                magnitude,
                stats_block: stats_block || (tick_type == O_TIMESTAMP_BASE && prev_type == Some(O_CLOSE_PRICE)),
                first: ticks.len() == block_start,
            });
            prev_type = Some(tick_type);
        }
    }
    false
}

/// Longest VLQ read: the longest whose value still fits an `i64`; a
/// longer or unterminated run is malformed (ibx#272).
pub const MAX_VLQ_BYTES: usize = 9;

/// A VLQ at `pos` that is terminated within [`MAX_VLQ_BYTES`]:
/// (value, num_bytes). None for an oversized or unterminated run (ibx#272).
#[inline]
pub fn read_vlq_bounded(data: &[u8], pos: usize) -> Option<(u64, usize)> {
    let end = data.len().min(pos.saturating_add(MAX_VLQ_BYTES));
    let mut val: u64 = 0;
    for (i, &b) in data.get(pos..end)?.iter().enumerate() {
        val = (val << 7) | (b as u64 & 0x7F);
        if b & 0x80 != 0 {
            return Some((val, i + 1));
        }
    }
    None
}

/// Read a VLQ-encoded unsigned integer (hi-bit terminated).
///
/// Bit7=1 means last byte. 7 data bits per byte, MSB first.
/// Returns (value, num_bytes).
pub fn read_vlq(data: &[u8], pos: usize) -> (u64, usize) {
    let mut val: u64 = 0;
    let mut n = 0usize;
    let mut p = pos;
    while p < data.len() {
        let b = data[p];
        val = (val << 7) | (b as u64 & 0x7F);
        n += 1;
        p += 1;
        if b & 0x80 != 0 {
            return (val, n);
        }
    }
    (val, n)
}

/// Convert VLQ value to signed (upper half of range = negative).
/// An empty or oversized `num_bytes` has no signed meaning and gives 0
/// (ibx#272).
pub fn vlq_signed(val: u64, num_bytes: usize) -> i64 {
    if num_bytes == 0 || num_bytes > MAX_VLQ_BYTES {
        return 0;
    }
    let bits = 7 * num_bytes;
    let half: u64 = 1 << (bits - 1);
    if val >= half {
        // Exact for the longest run too, whose range is half of an i64.
        (val as i64).wrapping_sub(1i64.wrapping_shl(bits as u32))
    } else {
        val as i64
    }
}

/// Read a high-bit terminated ASCII string.
///
/// Last character has bit7 set. Single 0x80 byte = empty string.
/// Returns (string, bytes_consumed).
pub fn read_hibit_str(data: &[u8], pos: usize) -> (String, usize) {
    let mut chars = Vec::new();
    let mut p = pos;
    while p < data.len() {
        let b = data[p];
        p += 1;
        if b & 0x80 != 0 {
            let ch = b & 0x7F;
            if ch != 0 {
                chars.push(ch as char);
            }
            return (chars.into_iter().collect(), p - pos);
        }
        chars.push(b as char);
    }
    (chars.into_iter().collect(), p - pos)
}

/// Decoded real-time bar from 35=G.
#[derive(Debug, Clone, Copy)]
pub struct RtBar {
    pub low: f64,
    pub open: f64,
    pub high: f64,
    pub close: f64,
    pub volume: i64,
    pub wap: f64,
    pub count: u32,
}

/// Decode a 35=G real-time bar payload.
///
/// Uses LSB-first bit reader with 4-byte group byte reversal.
pub fn decode_bar_payload(payload: &[u8], min_tick: f64) -> Option<RtBar> {
    // Reverse byte order within 4-byte groups
    let mut reordered = Vec::with_capacity(payload.len());
    for chunk in payload.chunks(4) {
        for &b in chunk.iter().rev() {
            reordered.push(b);
        }
    }

    let mut reader = LsbBitReader::new(&reordered);

    // 4 bits padding
    reader.read(4);

    // Count: 1-bit flag selects width
    let count = if reader.read(1) == 1 {
        reader.read(8) as u32
    } else {
        reader.read(32) as u32
    };

    // Low price in ticks (31-bit signed)
    let low_ticks = reader.read(31) as i64;
    let low_ticks = if low_ticks & (1 << 30) != 0 {
        low_ticks - (1 << 31)
    } else {
        low_ticks
    };
    let low = low_ticks as f64 * min_tick;

    let (open, high, close, wap_sum);
    if count > 1 {
        let width = if reader.read(1) == 1 { 5 } else { 32 };
        let delta_open = reader.read(width) as f64;
        let delta_high = reader.read(width) as f64;
        let delta_close = reader.read(width) as f64;

        open = low + delta_open * min_tick;
        high = low + delta_high * min_tick;
        close = low + delta_close * min_tick;

        wap_sum = if reader.read(1) == 1 {
            reader.read(18) as f64
        } else {
            reader.read(32) as f64
        };
    } else {
        open = low;
        high = low;
        close = low;
        wap_sum = 0.0;
    }

    // Volume: 1-bit flag selects width
    let volume = if reader.read(1) == 1 {
        reader.read(16) as i64
    } else {
        reader.read(32) as i64
    };

    let wap = if count > 1 && volume > 0 {
        low + wap_sum * min_tick / volume as f64
    } else {
        low
    };

    Some(RtBar {
        low,
        open,
        high,
        close,
        volume,
        wap,
        count,
    })
}

/// The bars of a 5-second bar frame body, read as the reference reads
/// them (ibx#454): ticker id, bar time and payload of each. The body is
/// what follows `35=G` (bit length, then the entries).
pub fn rtbar_entries(body: &[u8]) -> Vec<(u32, u32, &[u8])> {
    let mut entries = Vec::new();
    if body.len() < 2 {
        return entries;
    }
    let mut bits = u16::from_be_bytes([body[0], body[1]]) as usize;
    let available = (body.len() - 2) * 8;
    while bits + 65536 <= available {
        bits += 65536;
    }
    let end = (2 + bits.div_ceil(8)).min(body.len());
    let mut pos = 2;
    while pos + 9 <= end {
        let ticker_id = u32::from_be_bytes([body[pos], body[pos + 1], body[pos + 2], body[pos + 3]]);
        let time = u32::from_be_bytes([body[pos + 4], body[pos + 5], body[pos + 6], body[pos + 7]]);
        let len = body[pos + 8] as usize;
        let start = pos + 9;
        if start + len > end {
            break;
        }
        entries.push((ticker_id, time, &body[start..start + len]));
        pos = start + len;
    }
    entries
}

/// How the entries of one tick-by-tick stream are laid out (ibx#404): the
/// stream's type, which is not on the wire, and whether its acknowledgement
/// gave a size increment (the field order differs without one).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TbtLayout {
    Trade { sized: bool },
    BidAsk { sized: bool },
    MidPoint { sized: bool },
}

/// The fields of one tick-by-tick entry, raw: price deltas in ticks, sizes
/// in size increments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TbtFields {
    Trade { price_delta: i64, attribs: u64, size: u64, exchange: String, conditions: String },
    BidAsk { bid_delta: i64, ask_delta: i64, attribs: u64, bid_size: u64, ask_size: u64 },
    MidPoint { delta: i64 },
}

/// One tick-by-tick entry: the stream id the server gave in its
/// acknowledgement, the time (Unix seconds) and the fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TbtRawEntry {
    pub rt_ticker_id: u64,
    pub time: u64,
    pub fields: TbtFields,
}

/// Why the decode of a frame stopped before its end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TbtStop {
    /// Every entry was read.
    Done,
    /// A number or text ran past the data (ibx#272).
    Malformed,
}

/// How the entries of a stream id are read (ibx#404).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TbtEntryKind {
    /// A live stream: its fields by its layout.
    Read(TbtLayout),
    /// A stream that is gone: this many fields are skipped (its type's
    /// field count, 0 when not known), as the reference does.
    Skip(usize),
    /// A stream id with no stream yet: the field count is guessed, as the
    /// reference does, and the entry skipped.
    Guess,
}

/// A decoded tick-by-tick frame: the entries of live streams, the ids of
/// the entries skipped, and why the read ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TbtFrame {
    pub entries: Vec<TbtRawEntry>,
    pub skipped: Vec<u64>,
    pub stop: TbtStop,
}

/// The reference's field count of a stream type, used to skip the entry
/// of a stream that is gone: 5 for trades and bid/ask, 2 for midpoints.
pub fn tbt_field_count(layout: TbtLayout) -> usize {
    match layout {
        TbtLayout::Trade { .. } | TbtLayout::BidAsk { .. } => 5,
        TbtLayout::MidPoint { .. } => 2,
    }
}

/// A signed number and its width, or None past the data.
fn read_svlq(data: &[u8], pos: &mut usize) -> Option<i64> {
    let (v, n) = read_vlq_bounded(data, *pos)?;
    *pos += n;
    Some(vlq_signed(v, n))
}

fn read_uvlq(data: &[u8], pos: &mut usize) -> Option<u64> {
    let (v, n) = read_vlq_bounded(data, *pos)?;
    *pos += n;
    Some(v)
}

/// A size of one number, or of two (low part, then high part) when the
/// attribute bit says so.
fn read_size(data: &[u8], pos: &mut usize, wide: bool) -> Option<u64> {
    let low = read_uvlq(data, pos)?;
    if !wide {
        return Some(low);
    }
    let high = read_uvlq(data, pos)?;
    Some((low & 0xFFFF_FFFF) | (high << 32))
}

fn read_text(data: &[u8], pos: &mut usize, end: usize) -> Option<String> {
    if *pos >= end {
        return None;
    }
    let (s, n) = read_hibit_str(&data[..end], *pos);
    *pos += n;
    Some(s.trim().to_string())
}

fn read_fields(data: &[u8], pos: &mut usize, end: usize, layout: TbtLayout) -> Option<TbtFields> {
    let d = &data[..end];
    Some(match layout {
        TbtLayout::Trade { sized: true } => {
            let price_delta = read_svlq(d, pos)?;
            let attribs = read_uvlq(d, pos)?;
            let size = read_size(d, pos, attribs & 0x10 != 0)?;
            let exchange = read_text(data, pos, end)?;
            let conditions = read_text(data, pos, end)?;
            TbtFields::Trade { price_delta, attribs, size, exchange, conditions }
        }
        TbtLayout::Trade { sized: false } => {
            let price_delta = read_svlq(d, pos)?;
            let size = read_uvlq(d, pos)?;
            let attribs = read_uvlq(d, pos)?;
            let exchange = read_text(data, pos, end)?;
            let conditions = read_text(data, pos, end)?;
            TbtFields::Trade { price_delta, attribs, size, exchange, conditions }
        }
        TbtLayout::BidAsk { sized: true } => {
            let bid_delta = read_svlq(d, pos)?;
            let ask_delta = read_svlq(d, pos)?;
            let attribs = read_uvlq(d, pos)?;
            let bid_size = read_size(d, pos, attribs & 0x04 != 0)?;
            let ask_size = read_size(d, pos, attribs & 0x08 != 0)?;
            TbtFields::BidAsk { bid_delta, ask_delta, attribs, bid_size, ask_size }
        }
        TbtLayout::BidAsk { sized: false } => {
            let bid_delta = read_svlq(d, pos)?;
            let ask_delta = read_svlq(d, pos)?;
            let bid_size = read_uvlq(d, pos)?;
            let ask_size = read_uvlq(d, pos)?;
            let attribs = read_uvlq(d, pos)?;
            TbtFields::BidAsk { bid_delta, ask_delta, attribs, bid_size, ask_size }
        }
        TbtLayout::MidPoint { sized: true } => {
            let delta = read_svlq(d, pos)?;
            let attribs = read_uvlq(d, pos)?;
            read_size(d, pos, attribs & 0x02 != 0)?;
            TbtFields::MidPoint { delta }
        }
        TbtLayout::MidPoint { sized: false } => {
            let delta = read_svlq(d, pos)?;
            read_uvlq(d, pos)?;
            TbtFields::MidPoint { delta }
        }
    })
}

/// Skip `n` fields: each runs to its byte with bit 7 set; the data may
/// end first.
fn skip_fields(data: &[u8], pos: &mut usize, n: usize) {
    for _ in 0..n {
        while *pos < data.len() {
            let b = data[*pos];
            *pos += 1;
            if b & 0x80 != 0 {
                break;
            }
        }
    }
}

/// A number as the reference's guess reads it: a 32-bit value that wraps,
/// None past the data.
fn guess_number(data: &[u8], pos: &mut usize) -> Option<i32> {
    let mut v: i32 = 0;
    loop {
        let b = *data.get(*pos)?;
        *pos += 1;
        v = v.wrapping_shl(7).wrapping_add((b & 0x7F) as i32);
        if b & 0x80 != 0 {
            return Some(v);
        }
    }
}

/// The reference's field count guess for an entry of a stream id with no
/// stream (`GuessRawTickSize`): 2 when the 4th number after the time looks
/// like a time (the next entry's, after a midpoint), else 5 when the 7th
/// does (after a trade), else 0. A time looks right in `window` (Unix
/// seconds, start included).
fn guess_field_count(data: &[u8], pos: usize, window: (i64, i64)) -> usize {
    let looks_like_time = |v: i32| v != 0 && v != i32::MAX && v != i32::MIN
        && (v as i64) >= window.0 && (v as i64) < window.1;
    let mut p = pos;
    let mut nth = |n: usize| -> Option<i32> {
        let mut v = 0;
        for _ in 0..n {
            v = guess_number(data, &mut p)?;
        }
        Some(v)
    };
    let Some(fourth) = nth(4) else { return 0 };
    if looks_like_time(fourth) {
        return 2;
    }
    match nth(3) {
        Some(seventh) if looks_like_time(seventh) => 5,
        _ => 0,
    }
}

/// Decode a tick-by-tick frame (ibx#404), as the reference reads it: each
/// entry names its stream, and its fields are laid out by the stream's
/// type, which `kind_of` gives for a stream id. The entry of a stream that
/// is gone or not there yet is skipped by a field count and the read goes
/// on, as the reference does; `window` is the time range of its guess.
/// Reading stops at the declared size or at a number past the data.
pub fn decode_tbt_frame(body: &[u8], mut kind_of: impl FnMut(u64) -> TbtEntryKind, window: (i64, i64)) -> TbtFrame {
    let mut out = TbtFrame { entries: Vec::new(), skipped: Vec::new(), stop: TbtStop::Done };
    if body.len() < 2 {
        out.stop = TbtStop::Malformed;
        return out;
    }
    let data = &body[2..];
    // The declared size wraps on a long frame.
    let mut bits = u16::from_be_bytes([body[0], body[1]]) as usize;
    while bits + 65_536 <= data.len() * 8 {
        bits += 65_536;
    }
    let end = bits.div_ceil(8).min(data.len());
    let mut pos = 0;
    while pos < end {
        let (Some(rt_ticker_id), Some(time)) = (read_uvlq(&data[..end], &mut pos), read_uvlq(&data[..end], &mut pos)) else {
            out.stop = TbtStop::Malformed;
            return out;
        };
        let skip = match kind_of(rt_ticker_id) {
            TbtEntryKind::Read(layout) => {
                let Some(fields) = read_fields(data, &mut pos, end, layout) else {
                    out.stop = TbtStop::Malformed;
                    return out;
                };
                out.entries.push(TbtRawEntry { rt_ticker_id, time, fields });
                continue;
            }
            TbtEntryKind::Skip(n) => n,
            TbtEntryKind::Guess => guess_field_count(&data[..end], pos, window),
        };
        skip_fields(&data[..end], &mut pos, skip);
        out.skipped.push(rt_ticker_id);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bit_reader_basic() {
        let data = [0b1010_0011, 0b1100_0000];
        let mut r = BitReader::new(&data, 16);
        assert_eq!(r.read_unsigned(4), Some(0b1010)); // 10
        assert_eq!(r.read_unsigned(4), Some(0b0011)); // 3
        assert_eq!(r.read_unsigned(2), Some(0b11));   // 3
        assert_eq!(r.remaining(), 6);
    }

    #[test]
    fn bit_reader_single_bits() {
        let data = [0b10110000];
        let mut r = BitReader::new(&data, 5);
        assert_eq!(r.read_unsigned(1), Some(1));
        assert_eq!(r.read_unsigned(1), Some(0));
        assert_eq!(r.read_unsigned(1), Some(1));
        assert_eq!(r.read_unsigned(1), Some(1));
        assert_eq!(r.read_unsigned(1), Some(0));
        assert_eq!(r.read_unsigned(1), None); // exhausted
    }

    // ibx#446, captured 28/09/2026 (fix-agent-gw.20260928-164130.jsonl,
    // 16:07:16.345): a side with no quote comes as -100 at tick 0.01, a
    // price of -1, with size 0. The reference keeps it as it is
    // (`jccp.f.a(jutils.a)` = raw x tick, `jclient.record.ck.a(jccp.h, ar,
    // MarketDataType)@282-357`) and sends tickPrice -1 with size 0.
    #[test]
    fn empty_quote_side_is_minus_one_tick_count() {
        let hex = "01480000055004e424000ce42c005800000005501600851634016c00a76aba1e3dac00d80000000550b000";
        let body: Vec<u8> = (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect();
        let ticks = decode_ticks_35p(&body);
        let quote: Vec<(u64, i64)> = ticks.iter().take(5).map(|t| (t.tick_type, t.magnitude)).collect();
        assert!(ticks[..5].iter().all(|t| t.server_tag == 1360 && !t.stats_block));
        assert_eq!(quote, [(O_BID_PRICE, -100), (O_BID_SIZE, 0), (O_ASK_PRICE, -100), (O_ASK_SIZE, 0), (11, 0)]);
    }

    #[test]
    fn bit_reader_overflow() {
        let data = [0xFF];
        let mut r = BitReader::new(&data, 8);
        assert_eq!(r.read_unsigned(9), None); // not enough bits
    }

    #[test]
    fn bit_reader_total_bits_capped_to_data_length() {
        // total_bits exceeds data.len() * 8 — must be capped, not panic
        let data = [0xFF, 0xAA]; // 16 bits of data
        let mut r = BitReader::new(&data, 1000); // claim 1000 bits
        assert_eq!(r.remaining(), 16); // capped to 16
        assert_eq!(r.read_unsigned(8), Some(0xFF));
        assert_eq!(r.read_unsigned(8), Some(0xAA));
        assert_eq!(r.read_unsigned(1), None); // no more data
    }

    #[test]
    fn bit_reader_empty_data_with_nonzero_bits() {
        let data: [u8; 0] = [];
        let r = BitReader::new(&data, 100);
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn vlq_single_byte() {
        // 0x85 = 1_0000101 → hi-bit set (last byte), value = 5
        let (val, n) = read_vlq(&[0x85], 0);
        assert_eq!(val, 5);
        assert_eq!(n, 1);
    }

    #[test]
    fn vlq_two_bytes() {
        // 0x01, 0x80 → more(0x01), last(0x80)
        // val = (1 << 7) | 0 = 128
        let (val, n) = read_vlq(&[0x01, 0x80], 0);
        assert_eq!(val, 128);
        assert_eq!(n, 2);
    }

    #[test]
    fn vlq_signed_positive() {
        // 1 byte: range 0..63 is positive, 64..127 is negative
        assert_eq!(vlq_signed(5, 1), 5);
        assert_eq!(vlq_signed(63, 1), 63);
    }

    #[test]
    fn vlq_signed_negative() {
        // 1 byte: 64 → 64 - 128 = -64
        assert_eq!(vlq_signed(64, 1), -64);
        // 1 byte: 127 → 127 - 128 = -1
        assert_eq!(vlq_signed(127, 1), -1);
    }

    #[test]
    fn hibit_str_simple() {
        // "AB" + terminator: 0x41, 0x42|0x80 = 0x41, 0xC2
        let (s, n) = read_hibit_str(&[0x41, 0xC2], 0);
        assert_eq!(s, "AB");
        assert_eq!(n, 2);
    }

    #[test]
    fn hibit_str_empty() {
        // Single 0x80 = empty string
        let (s, n) = read_hibit_str(&[0x80], 0);
        assert_eq!(s, "");
        assert_eq!(n, 1);
    }

    #[test]
    fn hibit_str_single_char() {
        // "X" terminated: 0x58 | 0x80 = 0xD8
        let (s, n) = read_hibit_str(&[0xD8], 0);
        assert_eq!(s, "X");
        assert_eq!(n, 1);
    }

    #[test]
    fn decode_ticks_empty() {
        assert!(decode_ticks_35p(&[]).is_empty());
        assert!(decode_ticks_35p(&[0, 0]).is_empty());
    }

    // ── Helper: build bit-packed payloads for decode_ticks_35p ──────────

    /// Accumulates individual bits (MSB-first order) and produces the
    /// complete 35=P body: 2-byte big-endian bit_count + payload bytes.
    struct PayloadBuilder {
        bits: Vec<u8>, // each element is 0 or 1
    }

    impl PayloadBuilder {
        fn new() -> Self {
            Self { bits: Vec::new() }
        }

        /// Push `n` bits from the MSB side of `val`.
        fn push(&mut self, val: u64, n: usize) {
            for i in (0..n).rev() {
                self.bits.push(((val >> i) & 1) as u8);
            }
        }

        /// Emit a server-tag header: 1-bit continuation + 31-bit tag.
        fn server_tag(&mut self, cont: u64, tag: u32) {
            self.push(cont, 1);
            self.push(tag as u64, 31);
        }

        /// Emit a normal tick entry.
        /// `has_more`: 0 or 1.
        /// `width_bytes`: 1..=4 (maps to raw_width 0..=3).
        /// `value`: absolute value written into `width_bytes * 8 - 1` bits.
        /// `negative`: if true the sign bit is 1.
        fn tick(&mut self, tick_type: u64, has_more: u64, width_bytes: u64, value: u64, negative: bool) {
            assert!(tick_type < 31);
            assert!((1..=4).contains(&width_bytes));
            self.push(tick_type, 5);
            self.push(has_more, 1);
            self.push(width_bytes - 1, 2); // raw_width
            // sign bit + magnitude
            let total_value_bits = (width_bytes * 8) as usize;
            self.push(if negative { 1 } else { 0 }, 1);
            self.push(value, total_value_bits - 1);
        }

        /// Emit an extended tick entry (raw_tick_type == 31).
        fn tick_extended(
            &mut self,
            has_more: u64,
            ext_tick_type: u64,
            ext_byte_width: u64,
            value: u64,
            negative: bool,
        ) {
            self.push(31, 5); // sentinel
            self.push(has_more, 1);
            self.push(0, 2); // raw_width (ignored for extended)
            self.push(ext_tick_type, 8);
            self.push(ext_byte_width, 8);
            let total_value_bits = (ext_byte_width * 8) as usize;
            self.push(if negative { 1 } else { 0 }, 1);
            self.push(value, total_value_bits - 1);
        }

        /// Finalize into the full body: [bit_count_hi, bit_count_lo, payload…]
        fn build(&self) -> Vec<u8> {
            let bit_count = self.bits.len();
            let byte_count = (bit_count + 7) / 8;
            let mut payload = vec![0u8; byte_count];
            for (i, &b) in self.bits.iter().enumerate() {
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
    }

    // ── decode_ticks_35p tests ──────────────────────────────────────────

    #[test]
    fn decode_single_tag_single_bid_size_tick() {
        // O_BID_SIZE = 4, width 1 byte, unsigned value 42, positive
        let mut b = PayloadBuilder::new();
        b.server_tag(0, 7); // cont=0, tag=7
        b.tick(O_BID_SIZE, 0, 1, 42, false); // has_more=0
        let ticks = decode_ticks_35p(&b.build());
        assert_eq!(ticks.len(), 1);
        assert_eq!(ticks[0].server_tag, 7);
        assert_eq!(ticks[0].tick_type, O_BID_SIZE);
        assert_eq!(ticks[0].magnitude, 42);
    }

    #[test]
    fn decode_single_tag_single_bid_price_signed() {
        // O_BID_PRICE = 0, width 2 bytes, value 500, negative (signed delta)
        let mut b = PayloadBuilder::new();
        b.server_tag(0, 100);
        b.tick(O_BID_PRICE, 0, 2, 500, true);
        let ticks = decode_ticks_35p(&b.build());
        assert_eq!(ticks.len(), 1);
        assert_eq!(ticks[0].server_tag, 100);
        assert_eq!(ticks[0].tick_type, O_BID_PRICE);
        assert_eq!(ticks[0].magnitude, -500);
    }

    #[test]
    fn decode_multiple_ticks_for_one_server_tag() {
        // Two ticks under the same server_tag via has_more=1 on first tick
        let mut b = PayloadBuilder::new();
        b.server_tag(0, 55);
        b.tick(O_BID_PRICE, 1, 1, 10, false); // has_more=1
        b.tick(O_ASK_PRICE, 0, 1, 20, false); // has_more=0
        let ticks = decode_ticks_35p(&b.build());
        assert_eq!(ticks.len(), 2);
        assert_eq!(ticks[0].server_tag, 55);
        assert_eq!(ticks[0].tick_type, O_BID_PRICE);
        assert_eq!(ticks[0].magnitude, 10);
        assert_eq!(ticks[1].server_tag, 55);
        assert_eq!(ticks[1].tick_type, O_ASK_PRICE);
        assert_eq!(ticks[1].magnitude, 20);
    }

    #[test]
    fn decode_multiple_server_tags() {
        // Two server tags, one tick each
        let mut b = PayloadBuilder::new();
        b.server_tag(1, 10); // cont=1 (continuation)
        b.tick(O_VOLUME, 0, 2, 9999, false);
        b.server_tag(0, 20); // cont=0
        b.tick(O_LAST_PRICE, 0, 1, 3, true);
        let ticks = decode_ticks_35p(&b.build());
        assert_eq!(ticks.len(), 2);
        assert_eq!(ticks[0].server_tag, 10);
        assert_eq!(ticks[0].tick_type, O_VOLUME);
        assert_eq!(ticks[0].magnitude, 9999);
        assert_eq!(ticks[1].server_tag, 20);
        assert_eq!(ticks[1].tick_type, O_LAST_PRICE);
        assert_eq!(ticks[1].magnitude, -3);
    }

    #[test]
    fn decode_width_1_byte() {
        let mut b = PayloadBuilder::new();
        b.server_tag(0, 1);
        b.tick(O_BID_SIZE, 0, 1, 127, false); // max 7-bit unsigned = 127
        let ticks = decode_ticks_35p(&b.build());
        assert_eq!(ticks.len(), 1);
        assert_eq!(ticks[0].magnitude, 127);
    }

    #[test]
    fn decode_width_2_bytes() {
        let mut b = PayloadBuilder::new();
        b.server_tag(0, 2);
        // 2-byte value: 15 bits of magnitude, max 32767
        b.tick(O_ASK_SIZE, 0, 2, 32767, false);
        let ticks = decode_ticks_35p(&b.build());
        assert_eq!(ticks.len(), 1);
        assert_eq!(ticks[0].magnitude, 32767);
    }

    #[test]
    fn decode_width_3_bytes() {
        let mut b = PayloadBuilder::new();
        b.server_tag(0, 3);
        // 3-byte value: 23 bits of magnitude
        b.tick(O_LAST_SIZE, 0, 3, 1_000_000, false);
        let ticks = decode_ticks_35p(&b.build());
        assert_eq!(ticks.len(), 1);
        assert_eq!(ticks[0].magnitude, 1_000_000);
    }

    #[test]
    fn decode_width_4_bytes() {
        let mut b = PayloadBuilder::new();
        b.server_tag(0, 4);
        // 4-byte value: 31 bits of magnitude
        let big_val = 2_000_000_000u64;
        b.tick(O_VOLUME, 0, 4, big_val, false);
        let ticks = decode_ticks_35p(&b.build());
        assert_eq!(ticks.len(), 1);
        assert_eq!(ticks[0].magnitude, big_val as i64);
    }

    #[test]
    fn decode_negative_magnitude() {
        let mut b = PayloadBuilder::new();
        b.server_tag(0, 9);
        b.tick(O_HIGH_PRICE, 0, 2, 1234, true); // negative
        let ticks = decode_ticks_35p(&b.build());
        assert_eq!(ticks.len(), 1);
        assert_eq!(ticks[0].magnitude, -1234);
    }

    #[test]
    fn decode_extended_tick_type() {
        // raw_tick_type == 31 triggers extended: 8-bit tick_type + 8-bit byte_width
        let mut b = PayloadBuilder::new();
        b.server_tag(0, 42);
        // Extended tick with tick_type=O_OPEN_PRICE(22), byte_width=2, value=777, positive
        b.tick_extended(0, O_OPEN_PRICE, 2, 777, false);
        let ticks = decode_ticks_35p(&b.build());
        assert_eq!(ticks.len(), 1);
        assert_eq!(ticks[0].server_tag, 42);
        assert_eq!(ticks[0].tick_type, O_OPEN_PRICE);
        assert_eq!(ticks[0].magnitude, 777);
    }

    #[test]
    fn decode_keeps_the_block_flag() {
        // ibx#448: each tick keeps the stats flag of its block.
        let mut b = PayloadBuilder::new();
        b.server_tag(1, 1098);
        b.tick(O_TIMESTAMP_BASE, 1, 4, 20_260_922, false);
        b.tick(O_CLOSE_PRICE, 0, 3, 25_512, false);
        b.server_tag(0, 1098);
        b.tick(O_TIMESTAMP_BASE, 0, 4, 1_790_159_184, false);
        let ticks = decode_ticks_35p(&b.build());
        assert_eq!(ticks.len(), 3);
        assert!(ticks[0].stats_block && ticks[1].stats_block);
        assert!(!ticks[2].stats_block);
        assert_eq!(ticks[2].magnitude, 1_790_159_184);
    }

    #[test]
    fn decode_extended_tick_type_negative() {
        let mut b = PayloadBuilder::new();
        b.server_tag(0, 50);
        b.tick_extended(0, O_TIMESTAMP_DELTA, 3, 12345, true);
        let ticks = decode_ticks_35p(&b.build());
        assert_eq!(ticks.len(), 1);
        assert_eq!(ticks[0].tick_type, O_TIMESTAMP_DELTA);
        assert_eq!(ticks[0].magnitude, -12345);
    }

    #[test]
    fn decode_zero_bit_count() {
        // bit_count = 0 means no bits to read → no ticks
        let body = [0u8, 0, 0xFF, 0xFF]; // bit_count=0, garbage payload
        let ticks = decode_ticks_35p(&body);
        assert!(ticks.is_empty());
    }

    #[test]
    fn decode_insufficient_bits_for_server_tag() {
        // bit_count = 16 (only 16 bits), not enough for 32-bit server_tag header
        let body = [0u8, 16, 0xFF, 0xFF];
        let ticks = decode_ticks_35p(&body);
        assert!(ticks.is_empty());
    }

    #[test]
    fn decode_insufficient_bits_for_tick_value() {
        // Server tag fits but tick value is truncated
        let mut b = PayloadBuilder::new();
        b.server_tag(0, 1);
        // Start writing tick header but only 5+1+2 = 8 bits of header,
        // then width=4 means 32 bits needed for value, which won't exist.
        b.push(O_BID_PRICE, 5);
        b.push(0, 1); // has_more=0
        b.push(3, 2); // raw_width=3 → byte_width=4 → needs 32 value bits
        // Only provide 8 bits of value instead of 32
        b.push(0xFF, 8);
        let ticks = decode_ticks_35p(&b.build());
        // Should return empty: server_tag read ok, but tick value bits insufficient
        assert!(ticks.is_empty());
    }

    #[test]
    fn decode_extended_insufficient_bits() {
        // Extended tick where the 8+8 extension bits are not fully available
        let mut b = PayloadBuilder::new();
        b.server_tag(0, 1);
        b.push(31, 5); // raw_tick_type = 31 (extended)
        b.push(0, 1);  // has_more
        b.push(0, 2);  // raw_width (ignored)
        // Need 16 more bits for ext_tick_type + ext_byte_width, only provide 4
        b.push(0, 4);
        let ticks = decode_ticks_35p(&b.build());
        assert!(ticks.is_empty());
    }

    #[test]
    fn decode_magnitude_zero() {
        // Value 0 with sign=0 → magnitude 0
        let mut b = PayloadBuilder::new();
        b.server_tag(0, 1);
        b.tick(O_BID_PRICE, 0, 1, 0, false);
        let ticks = decode_ticks_35p(&b.build());
        assert_eq!(ticks.len(), 1);
        assert_eq!(ticks[0].magnitude, 0);
    }

    #[test]
    fn decode_negative_zero() {
        // Value 0 with sign=1 → magnitude -0 = 0
        let mut b = PayloadBuilder::new();
        b.server_tag(0, 1);
        b.tick(O_BID_PRICE, 0, 1, 0, true);
        let ticks = decode_ticks_35p(&b.build());
        assert_eq!(ticks.len(), 1);
        assert_eq!(ticks[0].magnitude, 0); // -0 == 0 in i64
    }

    #[test]
    fn decode_three_ticks_chained() {
        // Three ticks under one server_tag: has_more=1, has_more=1, has_more=0
        let mut b = PayloadBuilder::new();
        b.server_tag(0, 999);
        b.tick(O_BID_PRICE, 1, 1, 5, false);
        b.tick(O_ASK_PRICE, 1, 1, 10, true);
        b.tick(O_LAST_PRICE, 0, 2, 300, false);
        let ticks = decode_ticks_35p(&b.build());
        assert_eq!(ticks.len(), 3);
        assert_eq!(ticks[0].tick_type, O_BID_PRICE);
        assert_eq!(ticks[0].magnitude, 5);
        assert_eq!(ticks[1].tick_type, O_ASK_PRICE);
        assert_eq!(ticks[1].magnitude, -10);
        assert_eq!(ticks[2].tick_type, O_LAST_PRICE);
        assert_eq!(ticks[2].magnitude, 300);
        for t in &ticks {
            assert_eq!(t.server_tag, 999);
        }
    }

    #[test]
    fn decode_body_too_short() {
        // body < 4 bytes triggers early return
        assert!(decode_ticks_35p(&[0]).is_empty());
        assert!(decode_ticks_35p(&[0, 10]).is_empty());
        assert!(decode_ticks_35p(&[0, 10, 0xFF]).is_empty());
    }

    #[test]
    fn decode_mixed_server_tags_and_ticks() {
        // Tag1 with 2 ticks, then Tag2 with 1 tick
        let mut b = PayloadBuilder::new();
        b.server_tag(1, 111);
        b.tick(O_BID_SIZE, 1, 1, 50, false);
        b.tick(O_ASK_SIZE, 0, 1, 60, false);
        b.server_tag(0, 222);
        b.tick(O_LAST_SIZE, 0, 2, 1000, true);
        let ticks = decode_ticks_35p(&b.build());
        assert_eq!(ticks.len(), 3);
        assert_eq!(ticks[0].server_tag, 111);
        assert_eq!(ticks[0].tick_type, O_BID_SIZE);
        assert_eq!(ticks[0].magnitude, 50);
        assert_eq!(ticks[1].server_tag, 111);
        assert_eq!(ticks[1].tick_type, O_ASK_SIZE);
        assert_eq!(ticks[1].magnitude, 60);
        assert_eq!(ticks[2].server_tag, 222);
        assert_eq!(ticks[2].tick_type, O_LAST_SIZE);
        assert_eq!(ticks[2].magnitude, -1000);
    }

    #[test]
    fn decode_extended_with_has_more() {
        // Extended tick with has_more=1, followed by a normal tick
        let mut b = PayloadBuilder::new();
        b.server_tag(0, 77);
        b.tick_extended(1, O_HALTED, 1, 1, false); // has_more=1
        b.tick(O_BID_PRICE, 0, 1, 99, false);      // has_more=0
        let ticks = decode_ticks_35p(&b.build());
        assert_eq!(ticks.len(), 2);
        assert_eq!(ticks[0].tick_type, O_HALTED);
        assert_eq!(ticks[0].magnitude, 1);
        assert_eq!(ticks[1].tick_type, O_BID_PRICE);
        assert_eq!(ticks[1].magnitude, 99);
    }

    // ibx#272: an oversized value, or a block cut short after
    // some of its ticks, drops the whole block and ends the decode; the
    // blocks before it are kept.
    #[test]
    fn malformed_block_is_dropped_with_its_ticks() {
        // An oversized value.
        let mut b = PayloadBuilder::new();
        b.server_tag(0, 5);
        b.tick(O_BID_SIZE, 0, 1, 42, false);
        b.server_tag(0, 6);
        b.tick(O_ASK_SIZE, 1, 1, 7, false);
        b.push(31, 5);
        b.push(0, 1);
        b.push(0, 2);
        b.push(O_BID_PRICE, 8);
        b.push(9, 8);
        b.push(0, 36);
        b.push(0, 36);
        let mut ticks = Vec::new();
        assert!(decode_ticks_35p_into(&b.build(), &mut ticks));
        assert_eq!(ticks.len(), 1);
        assert_eq!((ticks[0].server_tag, ticks[0].magnitude), (5, 42));

        // A block that announces one more tick than it holds.
        let mut b = PayloadBuilder::new();
        b.server_tag(0, 5);
        b.tick(O_BID_SIZE, 0, 1, 42, false);
        b.server_tag(0, 6);
        b.tick(O_ASK_SIZE, 1, 1, 7, false);
        b.push(O_BID_PRICE, 5);
        b.push(0, 1);
        b.push(3, 2);
        b.push(0, 8);
        assert!(decode_ticks_35p_into(&b.build(), &mut ticks));
        assert_eq!(ticks.len(), 1);
        assert_eq!(ticks[0].server_tag, 5);

        // The widest value accepted decodes.
        let mut b = PayloadBuilder::new();
        b.server_tag(0, 5);
        b.tick_extended(0, O_VOLUME, 8, i64::MAX as u64, false);
        assert!(!decode_ticks_35p_into(&b.build(), &mut ticks));
        assert_eq!(ticks[0].magnitude, i64::MAX);
    }

    // ibx#272: an oversized or unterminated length is refused; the
    // signed reading never shifts out of range.
    #[test]
    fn vlq_widths_are_bounded() {
        assert_eq!(read_vlq_bounded(&[0x85], 0), Some((5, 1)));
        assert_eq!(read_vlq_bounded(&[0x01, 0x80], 0), Some((128, 2)));
        assert_eq!(read_vlq_bounded(&[0x01; 11], 0), None, "unterminated");
        let mut ten = [0x01u8; 10];
        ten[9] = 0x81;
        assert_eq!(read_vlq_bounded(&ten, 0), None, "too long");
        let mut nine = [0x7Fu8; 9];
        nine[8] = 0xFF;
        let (v, n) = read_vlq_bounded(&nine, 0).unwrap();
        assert_eq!((v, n), ((1u64 << 63) - 1, 9));
        assert_eq!(vlq_signed(v, n), -1);
        assert_eq!(vlq_signed(1 << 62, 9), -(1 << 62));
        assert_eq!(vlq_signed(5, 0), 0);
        assert_eq!(vlq_signed(5, 11), 0);
        assert_eq!(read_vlq_bounded(&[0x85], 1), None);
        assert_eq!(read_vlq_bounded(&[0x85], 5), None);

    }

    #[test]
    fn decode_max_server_tag() {
        // Maximum 31-bit server_tag value
        let max_tag = (1u32 << 31) - 1;
        let mut b = PayloadBuilder::new();
        b.server_tag(0, max_tag);
        b.tick(O_BID_SIZE, 0, 1, 1, false);
        let ticks = decode_ticks_35p(&b.build());
        assert_eq!(ticks.len(), 1);
        assert_eq!(ticks[0].server_tag, max_tag);
    }

    // ── decode_bar_payload tests ────────────────────────────────────────

    #[test]
    fn decode_bar_single_trade() {
        // count=1: only low is meaningful; open=high=close=low, volume encoded
        // Build LSB-first bit stream, then reverse within 4-byte groups.
        //
        // Layout (LSB first within the reordered buffer):
        //   4 bits padding (0)
        //   1 bit count_flag = 1 (short count)
        //   8 bits count = 1
        //   31 bits low_ticks = 1000 (positive)
        //   (no delta fields when count==1)
        //   1 bit vol_flag = 1 (short volume)
        //   16 bits volume = 500
        //
        // Total: 4+1+8+31+1+16 = 61 bits, 8 bytes
        let min_tick = 0.01;
        let mut bits_lsb: Vec<u8> = Vec::new();

        // helper: push n bits from val LSB-first
        let push_lsb = |bits: &mut Vec<u8>, val: u64, n: usize| {
            for i in 0..n {
                bits.push(((val >> i) & 1) as u8);
            }
        };

        push_lsb(&mut bits_lsb, 0, 4);     // padding
        push_lsb(&mut bits_lsb, 1, 1);      // count_flag=1 (8-bit)
        push_lsb(&mut bits_lsb, 1, 8);      // count=1
        push_lsb(&mut bits_lsb, 1000, 31);  // low_ticks=1000
        // count==1: no delta/wap fields
        push_lsb(&mut bits_lsb, 1, 1);      // vol_flag=1 (16-bit)
        push_lsb(&mut bits_lsb, 500, 16);   // volume=500

        // Convert bit stream to bytes (LSB first)
        let byte_count = (bits_lsb.len() + 7) / 8;
        let mut reordered = vec![0u8; byte_count];
        for (i, &b) in bits_lsb.iter().enumerate() {
            if b == 1 {
                reordered[i / 8] |= 1 << (i % 8);
            }
        }

        // Reverse within 4-byte groups to produce the wire payload
        let mut payload = Vec::new();
        for chunk in reordered.chunks(4) {
            let mut c = chunk.to_vec();
            c.reverse();
            payload.extend_from_slice(&c);
        }

        let bar = decode_bar_payload(&payload, min_tick).unwrap();
        assert_eq!(bar.count, 1);
        assert!((bar.low - 10.0).abs() < 1e-9);    // 1000 * 0.01
        assert!((bar.open - bar.low).abs() < 1e-9);
        assert!((bar.high - bar.low).abs() < 1e-9);
        assert!((bar.close - bar.low).abs() < 1e-9);
        assert_eq!(bar.volume, 500);
    }

    #[test]
    fn decode_bar_multi_trade_short_deltas() {
        // count > 1 with narrow (5-bit) deltas
        let min_tick = 0.01;
        let mut bits_lsb: Vec<u8> = Vec::new();

        let push_lsb = |bits: &mut Vec<u8>, val: u64, n: usize| {
            for i in 0..n {
                bits.push(((val >> i) & 1) as u8);
            }
        };

        push_lsb(&mut bits_lsb, 0, 4);     // padding
        push_lsb(&mut bits_lsb, 1, 1);      // count_flag=1 (8-bit)
        push_lsb(&mut bits_lsb, 5, 8);      // count=5
        push_lsb(&mut bits_lsb, 2000, 31);  // low_ticks=2000

        // count > 1: delta fields
        push_lsb(&mut bits_lsb, 1, 1);      // width_flag=1 → 5-bit deltas
        push_lsb(&mut bits_lsb, 3, 5);      // delta_open=3
        push_lsb(&mut bits_lsb, 7, 5);      // delta_high=7
        push_lsb(&mut bits_lsb, 2, 5);      // delta_close=2

        // wap
        push_lsb(&mut bits_lsb, 1, 1);      // wap_flag=1 → 18-bit
        push_lsb(&mut bits_lsb, 100, 18);   // wap_sum=100

        // volume
        push_lsb(&mut bits_lsb, 1, 1);      // vol_flag=1 → 16-bit
        push_lsb(&mut bits_lsb, 1000, 16);  // volume=1000

        let byte_count = (bits_lsb.len() + 7) / 8;
        let mut reordered = vec![0u8; byte_count];
        for (i, &b) in bits_lsb.iter().enumerate() {
            if b == 1 {
                reordered[i / 8] |= 1 << (i % 8);
            }
        }

        let mut payload = Vec::new();
        for chunk in reordered.chunks(4) {
            let mut c = chunk.to_vec();
            c.reverse();
            payload.extend_from_slice(&c);
        }

        let bar = decode_bar_payload(&payload, min_tick).unwrap();
        assert_eq!(bar.count, 5);
        let low = 2000.0 * min_tick; // 20.00
        assert!((bar.low - low).abs() < 1e-9);
        assert!((bar.open - (low + 3.0 * min_tick)).abs() < 1e-9);
        assert!((bar.high - (low + 7.0 * min_tick)).abs() < 1e-9);
        assert!((bar.close - (low + 2.0 * min_tick)).abs() < 1e-9);
        assert_eq!(bar.volume, 1000);
        // wap = low + wap_sum * min_tick / volume = 20.0 + 100*0.01/1000
        let expected_wap = low + 100.0 * min_tick / 1000.0;
        assert!((bar.wap - expected_wap).abs() < 1e-9);
    }

    // ── decode_tbt_frame tests (ibx#404) ──────────────────────────────

    /// Helper: encode a VLQ value into bytes (hi-bit terminated).
    fn encode_vlq(val: u64) -> Vec<u8> {
        if val == 0 {
            return vec![0x80];
        }
        let mut v = val;
        let mut groups = Vec::new();
        while v > 0 {
            groups.push((v & 0x7F) as u8);
            v >>= 7;
        }
        groups.reverse();
        let last = groups.len() - 1;
        groups[last] |= 0x80;
        groups
    }

    /// Helper: encode a hi-bit terminated string.
    fn encode_hibit_str(s: &str) -> Vec<u8> {
        if s.is_empty() {
            return vec![0x80];
        }
        let mut out = s.as_bytes().to_vec();
        let last = out.len() - 1;
        out[last] |= 0x80;
        out
    }

    fn frame(entries: &[u8]) -> Vec<u8> {
        let bits = (entries.len() * 8) as u16;
        let mut f = bits.to_be_bytes().to_vec();
        f.extend_from_slice(entries);
        f
    }

    /// The time window of the guess in these tests.
    const WINDOW: (i64, i64) = (1_781_000_000, 1_781_000_000 + 259_200);

    /// Decode with a layout per stream id (None: no stream yet).
    fn decode(body: &[u8], layout_of: impl Fn(u64) -> Option<TbtLayout>) -> (Vec<TbtRawEntry>, TbtStop) {
        let f = decode_tbt_frame(body, |id| layout_of(id).map_or(TbtEntryKind::Guess, TbtEntryKind::Read), WINDOW);
        (f.entries, f.stop)
    }

    // The captured entry (18/06/2026, seq 11117): stream 5, time
    // 1781772222, price 61900 ticks, attributes 12, size 100, ARCA, T; then
    // the next entry of stream 1.
    #[test]
    fn decode_captured_trade_entry() {
        let mut e = vec![0x85, 0x06, 0x51, 0x4e, 0x5f, 0xbe, 0x03, 0x63, 0xcc, 0x8c, 0xe4, 0x41, 0x52, 0x43, 0xc1, 0x20, 0x20, 0x54, 0xa0];
        e.extend(encode_vlq(1));
        e.extend(encode_vlq(1_781_772_223));
        e.extend(encode_vlq(127)); // -1 tick
        e.extend(encode_vlq(12));
        e.extend(encode_vlq(5));
        e.extend(encode_hibit_str("NYSE"));
        e.extend(encode_hibit_str(""));
        let (got, stop) = decode(&frame(&e), |_| Some(TbtLayout::Trade { sized: true }));
        assert_eq!(stop, TbtStop::Done);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0], TbtRawEntry {
            rt_ticker_id: 5, time: 1_781_772_222,
            fields: TbtFields::Trade { price_delta: 61900, attribs: 12, size: 100, exchange: "ARCA".into(), conditions: "T".into() },
        });
        assert_eq!(got[1].rt_ticker_id, 1);
        assert!(matches!(got[1].fields, TbtFields::Trade { price_delta: -1, size: 5, .. }));
    }

    // The layout comes from the stream, not from the first byte: stream 1
    // as bid/ask, stream 2 as trades; wide sizes take two numbers.
    #[test]
    fn decode_by_the_layout_of_each_stream() {
        let mut e = Vec::new();
        e.extend(encode_vlq(1));
        e.extend(encode_vlq(1000));
        e.extend(encode_vlq(10));   // bid delta
        e.extend(encode_vlq(12));   // ask delta
        e.extend(encode_vlq(0x04)); // wide bid size
        e.extend(encode_vlq(7));
        e.extend(encode_vlq(1));    // high part
        e.extend(encode_vlq(3));
        e.extend(encode_vlq(2));
        e.extend(encode_vlq(1001));
        e.extend(encode_vlq(5));
        e.extend(encode_vlq(0));
        e.extend(encode_vlq(100));
        e.extend(encode_hibit_str("ISLAND"));
        e.extend(encode_hibit_str(""));
        let layout = |id| match id {
            1 => Some(TbtLayout::BidAsk { sized: true }),
            2 => Some(TbtLayout::Trade { sized: true }),
            _ => None,
        };
        let (got, stop) = decode(&frame(&e), layout);
        assert_eq!(stop, TbtStop::Done);
        assert_eq!(got[0].fields, TbtFields::BidAsk { bid_delta: 10, ask_delta: 12, attribs: 4, bid_size: 7 | (1 << 32), ask_size: 3 });
        assert!(matches!(&got[1].fields, TbtFields::Trade { exchange, .. } if exchange == "ISLAND"));

        // A stream that is gone is skipped by its type's field count (5
        // for bid/ask: the wide bid size makes it one short) and the read
        // goes on, as the reference does.
        let f = decode_tbt_frame(&frame(&e), |id| match id {
            1 => TbtEntryKind::Skip(tbt_field_count(TbtLayout::BidAsk { sized: true })),
            _ => TbtEntryKind::Read(TbtLayout::Trade { sized: true }),
        }, WINDOW);
        assert_eq!(f.skipped, [1]);
        assert_eq!(f.entries.len(), 1);
        assert_eq!(f.entries[0].rt_ticker_id, 3, "the 6th field of the skipped entry is read as a stream id");
    }

    // ibx#404: an entry of a stream id with no stream yet is skipped by the
    // reference's guess: 2 fields when the 4th number after the time looks
    // like a time (a midpoint before the next entry), 5 when the 7th does
    // (a trade or a bid/ask), else none; the frame goes on after it.
    #[test]
    fn unknown_stream_entries_are_skipped_by_the_guess() {
        let t = WINDOW.0 as u64 + 10;
        let trade = |id: u64| {
            let mut e = Vec::new();
            e.extend(encode_vlq(id)); e.extend(encode_vlq(t)); e.extend(encode_vlq(61900)); e.extend(encode_vlq(12));
            e.extend(encode_vlq(100)); e.extend(encode_hibit_str("ARCA")); e.extend(encode_hibit_str("T"));
            e
        };
        let mid = |id: u64| [encode_vlq(id), encode_vlq(t), encode_vlq(5), encode_vlq(0)].concat();
        let live = |id| (id == 1).then_some(TbtLayout::Trade { sized: true });

        // Unknown trade entry (stream 9), then a live one.
        let e = [trade(9), trade(1)].concat();
        let f = decode_tbt_frame(&frame(&e), |id| live(id).map_or(TbtEntryKind::Guess, TbtEntryKind::Read), WINDOW);
        assert_eq!((f.skipped.as_slice(), f.entries.len(), f.stop), (&[9][..], 1, TbtStop::Done));
        assert_eq!(f.entries[0].rt_ticker_id, 1);

        // Unknown unsized midpoint (2 fields), then a live trade.
        let e = [mid(9), trade(1)].concat();
        let f = decode_tbt_frame(&frame(&e), |id| live(id).map_or(TbtEntryKind::Guess, TbtEntryKind::Read), WINDOW);
        assert_eq!((f.skipped.as_slice(), f.entries.len()), (&[9][..], 1));

        // A time out of the window: nothing skipped, the next numbers are
        // read as an entry.
        let mut e = trade(9);
        e.extend(trade(1));
        let f = decode_tbt_frame(&frame(&e), |id| live(id).map_or(TbtEntryKind::Guess, TbtEntryKind::Read), (0, 1));
        assert_eq!(f.skipped.first(), Some(&9));
        assert!(f.skipped.len() > 1, "the fields are read as more entries: {f:?}");
    }

    // Without a size increment the fields come in another order; the
    // midpoint layouts read their size and drop it.
    #[test]
    fn decode_unsized_and_midpoint_layouts() {
        let mut e = Vec::new();
        e.extend(encode_vlq(3));
        e.extend(encode_vlq(50));
        e.extend(encode_vlq(9));   // bid
        e.extend(encode_vlq(11));  // ask
        e.extend(encode_vlq(200)); // bid size
        e.extend(encode_vlq(300)); // ask size
        e.extend(encode_vlq(1));   // attribs
        e.extend(encode_vlq(4));
        e.extend(encode_vlq(51));
        e.extend(encode_vlq(20));  // mid delta
        e.extend(encode_vlq(2));   // wide size
        e.extend(encode_vlq(1));
        e.extend(encode_vlq(0));
        let layout = |id| match id {
            3 => Some(TbtLayout::BidAsk { sized: false }),
            4 => Some(TbtLayout::MidPoint { sized: true }),
            _ => None,
        };
        let (got, stop) = decode(&frame(&e), layout);
        assert_eq!(stop, TbtStop::Done);
        assert_eq!(got[0].fields, TbtFields::BidAsk { bid_delta: 9, ask_delta: 11, attribs: 1, bid_size: 200, ask_size: 300 });
        assert_eq!(got[1].fields, TbtFields::MidPoint { delta: 20 });
    }

    // A truncated number ends the decode without the entry (ibx#272); the
    // bit count bounds the read.
    #[test]
    fn decode_stops_on_truncated_data() {
        let mut e = Vec::new();
        e.extend(encode_vlq(1));
        e.extend(encode_vlq(1000));
        e.push(0x05); // unterminated price
        let (got, stop) = decode(&frame(&e), |_| Some(TbtLayout::Trade { sized: true }));
        assert!(got.is_empty());
        assert_eq!(stop, TbtStop::Malformed);
        assert_eq!(decode(&[], |_| None).1, TbtStop::Malformed);
        let (got, stop) = decode(&[0, 0, 0x81, 0x82], |_| None);
        assert!(got.is_empty() && stop == TbtStop::Done, "a zero bit count holds no entry");
    }
}
