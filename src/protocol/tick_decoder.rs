//! Binary bit-packed tick decoder for 35=P messages from usfarm.
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
        let total_bits = if total_bits > 0 {
            total_bits
        } else {
            data.len() * 8
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
}
