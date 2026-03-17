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
    pub fn read_unsigned(&mut self, n: usize) -> Option<u64> {
        if n == 0 {
            return Some(0);
        }
        if self.bit_pos + n > self.total_bits {
            return None;
        }
        let mut result: u64 = 0;
        for _ in 0..n {
            let byte_idx = self.bit_pos >> 3;
            let bit_idx = 7 - (self.bit_pos & 7); // MSB first
            result = (result << 1) | ((self.data[byte_idx] >> bit_idx) as u64 & 1);
            self.bit_pos += 1;
        }
        Some(result)
    }
}

// 8=O binary tick type IDs (what comes off the wire in 35=P)
pub const O_BID_PRICE: u64 = 0;
pub const O_ASK_PRICE: u64 = 1;
pub const O_LAST_PRICE: u64 = 2;
pub const O_HIGH_PRICE: u64 = 3;
pub const O_BID_SIZE: u64 = 4;
pub const O_ASK_SIZE: u64 = 5;
pub const O_VOLUME: u64 = 6;
pub const O_OPEN_PRICE: u64 = 8;
pub const O_LOW_PRICE: u64 = 9;
pub const O_TIMESTAMP: u64 = 10;
pub const O_LAST_SIZE: u64 = 12;
pub const O_LAST_EXCH: u64 = 13;
pub const O_BID_EXCH: u64 = 16;
pub const O_ASK_EXCH: u64 = 17;
pub const O_HALTED: u64 = 18;
pub const O_CLOSE_PRICE: u64 = 22;
pub const O_LAST_TS: u64 = 23;

/// Volume multiplier: IB encodes volume * 10000.
pub const VOLUME_MULT: f64 = 0.0001;

/// A single decoded tick from a 35=P message.
#[derive(Debug, Clone, Copy)]
pub struct RawTick {
    pub server_tag: u32,
    pub tick_type: u64,
    pub magnitude: i64,
}

/// Decode all ticks from a 35=P binary payload.
///
/// `body` is the raw message body after stripping FIX framing and HMAC signature.
/// Returns a list of raw ticks with server_tag, tick_type, and signed magnitude.
pub fn decode_ticks_35p(body: &[u8]) -> Vec<RawTick> {
    if body.len() < 4 {
        return Vec::new();
    }

    let bit_count = ((body[0] as usize) << 8) | (body[1] as usize);
    let payload = &body[2..];
    let mut reader = BitReader::new(payload, bit_count);
    let mut ticks = Vec::new();

    while reader.remaining() > 32 {
        let cont = match reader.read_unsigned(1) {
            Some(v) => v,
            None => break,
        };
        let _ = cont; // continuation flag, not used in decoding
        let server_tag = match reader.read_unsigned(31) {
            Some(v) => v as u32,
            None => break,
        };

        let mut has_more = 1u64;
        while has_more == 1 && reader.remaining() >= 8 {
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
                    return ticks;
                }
                tick_type = match reader.read_unsigned(8) {
                    Some(v) => v,
                    None => return ticks,
                };
                byte_width = match reader.read_unsigned(8) {
                    Some(v) => v,
                    None => return ticks,
                };
            } else {
                tick_type = raw_tick_type;
                byte_width = raw_width;
            }

            let total_value_bits = (8 * byte_width) as usize;
            if reader.remaining() < total_value_bits {
                return ticks;
            }

            let sign = match reader.read_unsigned(1) {
                Some(v) => v,
                None => return ticks,
            };
            let magnitude_unsigned = if total_value_bits > 1 {
                match reader.read_unsigned(total_value_bits - 1) {
                    Some(v) => v as i64,
                    None => return ticks,
                }
            } else {
                0i64
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
            });
        }
    }

    ticks
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
pub fn vlq_signed(val: u64, num_bytes: usize) -> i64 {
    let bits = 7 * num_bytes;
    let half: u64 = 1 << (bits - 1);
    if val >= half {
        val as i64 - (1i64 << bits)
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

    let mut pos = 0usize;
    let total_bits = reordered.len() * 8;

    let read_bits = |pos: &mut usize, n: usize| -> u64 {
        let mut val: u64 = 0;
        for i in 0..n {
            if *pos < total_bits {
                let byte_idx = *pos / 8;
                let bit_idx = *pos % 8;
                if byte_idx < reordered.len() {
                    val |= ((reordered[byte_idx] >> bit_idx) as u64 & 1) << i;
                }
                *pos += 1;
            }
        }
        val
    };

    // 4 bits padding
    read_bits(&mut pos, 4);

    // Count: 1-bit flag selects width
    let count = if read_bits(&mut pos, 1) == 1 {
        read_bits(&mut pos, 8) as u32
    } else {
        read_bits(&mut pos, 32) as u32
    };

    // Low price in ticks (31-bit signed)
    let low_ticks = read_bits(&mut pos, 31) as i64;
    let low_ticks = if low_ticks & (1 << 30) != 0 {
        low_ticks - (1 << 31)
    } else {
        low_ticks
    };
    let low = low_ticks as f64 * min_tick;

    let (open, high, close, wap_sum);
    if count > 1 {
        let width = if read_bits(&mut pos, 1) == 1 { 5 } else { 32 };
        let delta_open = read_bits(&mut pos, width) as f64;
        let delta_high = read_bits(&mut pos, width) as f64;
        let delta_close = read_bits(&mut pos, width) as f64;

        open = low + delta_open * min_tick;
        high = low + delta_high * min_tick;
        close = low + delta_close * min_tick;

        wap_sum = if read_bits(&mut pos, 1) == 1 {
            read_bits(&mut pos, 18) as f64
        } else {
            read_bits(&mut pos, 32) as f64
        };
    } else {
        open = low;
        high = low;
        close = low;
        wap_sum = 0.0;
    }

    // Volume: 1-bit flag selects width
    let volume = if read_bits(&mut pos, 1) == 1 {
        read_bits(&mut pos, 16) as i64
    } else {
        read_bits(&mut pos, 32) as i64
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

/// Marker bytes for tick-by-tick 35=E binary entries.
pub const TBT_MARKER_ALL_LAST: u8 = 0x81;
pub const TBT_MARKER_BID_ASK: u8 = 0x82;

/// A decoded tick-by-tick entry from 35=E.
#[derive(Debug, Clone)]
pub enum TbtEntry {
    /// AllLast trade tick.
    Trade {
        timestamp: u64,
        price_cents_delta: i64,
        size: u64,
        exchange: String,
        conditions: String,
    },
    /// BidAsk quote tick.
    Quote {
        timestamp: u64,
        bid_cents_delta: i64,
        ask_cents_delta: i64,
        bid_size: u64,
        ask_size: u64,
    },
}

/// Decode tick-by-tick entries from a 35=E binary payload.
///
/// `body` is the raw message body after stripping FIX framing and HMAC.
/// Prices are signed VLQ deltas in cents from a running state (caller tracks).
pub fn decode_ticks_35e(body: &[u8]) -> Vec<TbtEntry> {
    let mut entries = Vec::new();
    let mut pos = 0;

    while pos < body.len() {
        let marker = body[pos];
        pos += 1;

        match marker {
            TBT_MARKER_ALL_LAST => {
                if pos >= body.len() { break; }
                let (ts, n) = read_vlq(body, pos);
                pos += n;
                if pos >= body.len() { break; }
                let (price_raw, n) = read_vlq(body, pos);
                let price_delta = vlq_signed(price_raw, n);
                pos += n;
                if pos >= body.len() { break; }
                let (attribs, n) = read_vlq(body, pos);
                let _ = attribs; // reserved
                pos += n;
                if pos >= body.len() { break; }
                let (size, n) = read_vlq(body, pos);
                pos += n;
                if pos >= body.len() { break; }
                let (exchange, n) = read_hibit_str(body, pos);
                pos += n;
                if pos >= body.len() { break; }
                let (conditions, n) = read_hibit_str(body, pos);
                pos += n;

                entries.push(TbtEntry::Trade {
                    timestamp: ts,
                    price_cents_delta: price_delta,
                    size,
                    exchange,
                    conditions,
                });
            }
            TBT_MARKER_BID_ASK => {
                if pos >= body.len() { break; }
                let (ts, n) = read_vlq(body, pos);
                pos += n;
                if pos >= body.len() { break; }
                let (bid_raw, n) = read_vlq(body, pos);
                let bid_delta = vlq_signed(bid_raw, n);
                pos += n;
                if pos >= body.len() { break; }
                let (ask_raw, n) = read_vlq(body, pos);
                let ask_delta = vlq_signed(ask_raw, n);
                pos += n;
                if pos >= body.len() { break; }
                let (attribs, n) = read_vlq(body, pos);
                let _ = attribs;
                pos += n;
                if pos >= body.len() { break; }
                let (bid_size, n) = read_vlq(body, pos);
                pos += n;
                if pos >= body.len() { break; }
                let (ask_size, n) = read_vlq(body, pos);
                pos += n;

                entries.push(TbtEntry::Quote {
                    timestamp: ts,
                    bid_cents_delta: bid_delta,
                    ask_cents_delta: ask_delta,
                    bid_size,
                    ask_size,
                });
            }
            _ => break, // unknown marker, stop parsing
        }
    }

    entries
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
        // Extended tick with tick_type=O_CLOSE_PRICE(22), byte_width=2, value=777, positive
        b.tick_extended(0, O_CLOSE_PRICE, 2, 777, false);
        let ticks = decode_ticks_35p(&b.build());
        assert_eq!(ticks.len(), 1);
        assert_eq!(ticks[0].server_tag, 42);
        assert_eq!(ticks[0].tick_type, O_CLOSE_PRICE);
        assert_eq!(ticks[0].magnitude, 777);
    }

    #[test]
    fn decode_extended_tick_type_negative() {
        let mut b = PayloadBuilder::new();
        b.server_tag(0, 50);
        b.tick_extended(0, O_LAST_TS, 3, 12345, true);
        let ticks = decode_ticks_35p(&b.build());
        assert_eq!(ticks.len(), 1);
        assert_eq!(ticks[0].tick_type, O_LAST_TS);
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

    // ── decode_ticks_35e tests ────────────────────────────────────────

    /// Helper: encode a VLQ value into bytes (hi-bit terminated).
    fn encode_vlq(val: u64) -> Vec<u8> {
        if val == 0 {
            return vec![0x80];
        }
        // Find how many 7-bit groups we need
        let mut v = val;
        let mut groups = Vec::new();
        while v > 0 {
            groups.push((v & 0x7F) as u8);
            v >>= 7;
        }
        groups.reverse();
        let last = groups.len() - 1;
        groups[last] |= 0x80; // hi-bit on last byte
        groups
    }

    /// Helper: encode a hi-bit terminated string.
    fn encode_hibit_str(s: &str) -> Vec<u8> {
        if s.is_empty() {
            return vec![0x80];
        }
        let bytes = s.as_bytes();
        let mut out = bytes.to_vec();
        let last = out.len() - 1;
        out[last] |= 0x80;
        out
    }

    #[test]
    fn decode_35e_single_trade() {
        let mut payload = Vec::new();
        payload.push(TBT_MARKER_ALL_LAST);
        payload.extend(encode_vlq(1000));    // timestamp
        payload.extend(encode_vlq(5));       // price delta = +5 (1 byte VLQ, val < 64 = positive)
        payload.extend(encode_vlq(0));       // attribs
        payload.extend(encode_vlq(100));     // size
        payload.extend(encode_hibit_str("ARCA"));   // exchange
        payload.extend(encode_hibit_str(""));       // conditions

        let entries = decode_ticks_35e(&payload);
        assert_eq!(entries.len(), 1);
        match &entries[0] {
            TbtEntry::Trade { timestamp, price_cents_delta, size, exchange, conditions } => {
                assert_eq!(*timestamp, 1000);
                assert_eq!(*price_cents_delta, 5);
                assert_eq!(*size, 100);
                assert_eq!(exchange, "ARCA");
                assert_eq!(conditions, "");
            }
            _ => panic!("expected Trade"),
        }
    }

    #[test]
    fn decode_35e_single_quote() {
        let mut payload = Vec::new();
        payload.push(TBT_MARKER_BID_ASK);
        payload.extend(encode_vlq(2000));    // timestamp
        payload.extend(encode_vlq(3));       // bid delta = +3
        payload.extend(encode_vlq(7));       // ask delta = +7
        payload.extend(encode_vlq(0));       // attribs
        payload.extend(encode_vlq(500));     // bid_size
        payload.extend(encode_vlq(300));     // ask_size

        let entries = decode_ticks_35e(&payload);
        assert_eq!(entries.len(), 1);
        match &entries[0] {
            TbtEntry::Quote { timestamp, bid_cents_delta, ask_cents_delta, bid_size, ask_size } => {
                assert_eq!(*timestamp, 2000);
                assert_eq!(*bid_cents_delta, 3);
                assert_eq!(*ask_cents_delta, 7);
                assert_eq!(*bid_size, 500);
                assert_eq!(*ask_size, 300);
            }
            _ => panic!("expected Quote"),
        }
    }

    #[test]
    fn decode_35e_mixed_trade_and_quote() {
        let mut payload = Vec::new();
        // Trade
        payload.push(TBT_MARKER_ALL_LAST);
        payload.extend(encode_vlq(100));
        payload.extend(encode_vlq(10));
        payload.extend(encode_vlq(0));
        payload.extend(encode_vlq(50));
        payload.extend(encode_hibit_str("NYSE"));
        payload.extend(encode_hibit_str("@"));
        // Quote
        payload.push(TBT_MARKER_BID_ASK);
        payload.extend(encode_vlq(200));
        payload.extend(encode_vlq(1));
        payload.extend(encode_vlq(2));
        payload.extend(encode_vlq(0));
        payload.extend(encode_vlq(1000));
        payload.extend(encode_vlq(800));

        let entries = decode_ticks_35e(&payload);
        assert_eq!(entries.len(), 2);
        assert!(matches!(&entries[0], TbtEntry::Trade { .. }));
        assert!(matches!(&entries[1], TbtEntry::Quote { .. }));
    }

    #[test]
    fn decode_35e_negative_price_delta() {
        // VLQ 1 byte: value 64 = -64 (upper half)
        let mut payload = Vec::new();
        payload.push(TBT_MARKER_ALL_LAST);
        payload.extend(encode_vlq(500));
        // Encode -1: 1-byte VLQ val=127 → vlq_signed(127,1) = -1
        payload.push(0xFF); // 0x7F | 0x80 = 0xFF → val=127, n=1
        payload.extend(encode_vlq(0));
        payload.extend(encode_vlq(25));
        payload.extend(encode_hibit_str(""));
        payload.extend(encode_hibit_str(""));

        let entries = decode_ticks_35e(&payload);
        assert_eq!(entries.len(), 1);
        match &entries[0] {
            TbtEntry::Trade { price_cents_delta, .. } => {
                assert_eq!(*price_cents_delta, -1);
            }
            _ => panic!("expected Trade"),
        }
    }

    #[test]
    fn decode_35e_empty() {
        assert!(decode_ticks_35e(&[]).is_empty());
    }

    #[test]
    fn decode_35e_unknown_marker_stops() {
        let mut payload = Vec::new();
        payload.push(0x99); // unknown
        payload.extend(encode_vlq(100));
        assert!(decode_ticks_35e(&payload).is_empty());
    }
}
