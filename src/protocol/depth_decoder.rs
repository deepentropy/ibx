//! Decoder of the farm's depth messages (35=Y, 35=Z), bit for bit as the
//! reference reads them (#451).
//!
//! The body starts with a 2-byte bit count, then a bit stream read MSB
//! first: groups of `[more groups:1][server tag:31]`, each with entries
//! `[more entries:1][unused:1][operation:2][market maker length:4]
//! [market maker:8 x len][position:8]`, an insert or update entry then
//! carrying fields `[type:5][more:1][width-1:2]` (type 31: an 8-bit type and
//! an 8-bit width follow), `[sign:1][value:8 x width - 1]`. The field id is
//! the type divided by 4.

use crate::protocol::tick_decoder::BitReader;

/// Entry operations, as on the wire.
pub const OP_INSERT: u8 = 0;
pub const OP_UPDATE: u8 = 1;
pub const OP_DELETE_BID: u8 = 2;
pub const OP_DELETE_ASK: u8 = 3;

/// Field ids (the wire type divided by 4).
const F_BID_PRICE: u64 = 0;
const F_ASK_PRICE: u64 = 1;
const F_BID_SIZE: u64 = 4;
const F_ASK_SIZE: u64 = 5;
/// Extra bid / ask value of an entry, not a price or a size.
const F_BID_EXTRA: u64 = 25;
const F_ASK_EXTRA: u64 = 26;

/// The side of a book row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Bid,
    Ask,
}

impl Side {
    /// The side number of the API: 1 bid, 0 ask.
    #[inline]
    pub fn api(self) -> i32 {
        match self { Side::Bid => 1, Side::Ask => 0 }
    }
}

/// One entry of a depth group.
#[derive(Debug, Clone, PartialEq)]
pub struct DepthEntry {
    /// 0 insert, 1 update, 2 delete bid, 3 delete ask.
    pub op: u8,
    /// Row index set by the server.
    pub position: i32,
    /// Market maker, trimmed; empty when the entry has none.
    pub market_maker: String,
    /// From the operation for a delete, else from the first field; None
    /// for an entry without fields (taken as the bid side by the book).
    pub side: Option<Side>,
    /// Price in ticks.
    pub price: Option<i64>,
    /// Size in wire units.
    pub size: Option<i64>,
}

impl DepthEntry {
    /// The side list the entry applies to: the delete's side, else the
    /// side of its first field, else the bid side, as the reference.
    #[inline]
    pub fn book_side(&self) -> Side {
        self.side.unwrap_or(Side::Bid)
    }
}

/// The entries of one server tag, applied to its book as one change.
#[derive(Debug, Clone, PartialEq)]
pub struct DepthGroup {
    pub server_tag: u32,
    pub entries: Vec<DepthEntry>,
}

/// Bits the body holds: its 2-byte count, raised by 65,536 while that
/// still fits the body, as the reference corrects a count that wrapped.
#[inline]
fn bit_count(body: &[u8]) -> usize {
    let mut n = ((body[0] as usize) << 8) | body[1] as usize;
    while n + 65_536 < body.len() * 8 {
        n += 65_536;
    }
    n
}

/// Decode a depth body (the bytes after `35=Y` / `35=Z`, up to the
/// signature). `extended` allows extended fields (35=Z); in 35=Y an
/// extended field drops the whole message, as the reference does. Groups
/// read before the bits run out are kept; the group cut short is dropped.
pub fn decode_depth(body: &[u8], extended: bool) -> Vec<DepthGroup> {
    let mut groups = Vec::new();
    if body.len() < 2 {
        return groups;
    }
    let bits = bit_count(body);
    let mut r = BitReader::new(&body[2..], bits);
    while r.remaining() > 0 {
        match read_groups(&mut r, extended, &mut groups) {
            Ok(()) => {}
            Err(Stop::Short) => break,
            Err(Stop::Extended) => {
                log::warn!("Depth message with an extended field: dropped");
                return Vec::new();
            }
        }
    }
    groups
}

enum Stop {
    /// The bits ran out in the middle of a group.
    Short,
    /// An extended field where none is allowed.
    Extended,
}

#[inline(always)]
fn read(r: &mut BitReader, n: usize) -> Result<u64, Stop> {
    r.read_unsigned(n).ok_or(Stop::Short)
}

/// Read groups until one says no other follows.
fn read_groups(r: &mut BitReader, extended: bool, groups: &mut Vec<DepthGroup>) -> Result<(), Stop> {
    loop {
        let more_groups = read(r, 1)?;
        let server_tag = read(r, 31)? as u32;
        let mut entries = Vec::new();
        loop {
            let more_entries = read(r, 1)?;
            read(r, 1)?;
            let op = read(r, 2)? as u8;
            let mm_len = read(r, 4)? as usize;
            let mut market_maker = String::new();
            if mm_len > 0 {
                let mut mm = [0u8; 15];
                for b in mm.iter_mut().take(mm_len) {
                    *b = read(r, 8)? as u8;
                }
                market_maker = mm[..mm_len].iter().map(|&b| b as char).collect::<String>().trim().to_string();
            }
            let position = read(r, 8)? as i32;
            let mut entry = DepthEntry {
                op, position, market_maker,
                side: match op {
                    OP_DELETE_BID => Some(Side::Bid),
                    OP_DELETE_ASK => Some(Side::Ask),
                    _ => None,
                },
                price: None, size: None,
            };
            if op == OP_INSERT || op == OP_UPDATE {
                let mut more = 1;
                while more == 1 {
                    let mut kind = read(r, 5)?;
                    more = read(r, 1)?;
                    let mut width = read(r, 2)? + 1;
                    let wide = kind == 31;
                    if wide {
                        kind = read(r, 8)?;
                        width = read(r, 8)?;
                    }
                    let field = kind / 4;
                    let negative = read(r, 1)? == 1;
                    let value_bits = (8 * width as usize).saturating_sub(1);
                    if value_bits > 63 {
                        return Err(Stop::Short);
                    }
                    let magnitude = read(r, value_bits)? as i64;
                    let value = if negative { -magnitude } else { magnitude };
                    if wide && !extended {
                        return Err(Stop::Extended);
                    }
                    let side = |s: Side, e: &mut DepthEntry| { e.side.get_or_insert(s); };
                    match field {
                        F_BID_PRICE => { entry.price = Some(value); side(Side::Bid, &mut entry) }
                        F_ASK_PRICE => { entry.price = Some(value); side(Side::Ask, &mut entry) }
                        F_BID_SIZE => { entry.size = Some(value); side(Side::Bid, &mut entry) }
                        F_ASK_SIZE => { entry.size = Some(value); side(Side::Ask, &mut entry) }
                        F_BID_EXTRA => side(Side::Bid, &mut entry),
                        F_ASK_EXTRA => side(Side::Ask, &mut entry),
                        other => log::debug!("Depth entry: invalid field type {}", other),
                    }
                }
            }
            entries.push(entry);
            if more_entries == 0 {
                break;
            }
        }
        groups.push(DepthGroup { server_tag, entries });
        if more_groups == 0 {
            return Ok(());
        }
    }
}

/// The depth body of a farm message: after `35=Y` / `35=Z`, up to the
/// signature trailer or the message's last separator.
pub fn depth_body(msg: &[u8]) -> Option<(&[u8], bool)> {
    let start = msg.windows(5).position(|w| w == b"35=Y\x01" || w == b"35=Z\x01")?;
    let extended = msg[start + 3] == b'Z';
    let rest = &msg[start + 5..];
    let end = rest.windows(6).rposition(|w| w == b"\x018349=")
        .unwrap_or_else(|| rest.len() - usize::from(rest.last() == Some(&0x01)));
    Some((&rest[..end], extended))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bits written MSB first.
    struct W { bytes: Vec<u8>, bits: usize }
    impl W {
        fn new() -> Self { Self { bytes: Vec::new(), bits: 0 } }
        fn put(&mut self, n: usize, v: u64) {
            for i in (0..n).rev() {
                if self.bits.is_multiple_of(8) { self.bytes.push(0); }
                let bit = ((v >> i) & 1) as u8;
                let last = self.bytes.len() - 1;
                self.bytes[last] |= bit << (7 - self.bits % 8);
                self.bits += 1;
            }
        }
        fn body(&self) -> Vec<u8> {
            let mut b = vec![(self.bits >> 8) as u8, self.bits as u8];
            b.extend_from_slice(&self.bytes);
            b
        }
        fn field(&mut self, field: u64, more: bool, width: u64, value: i64) {
            self.put(5, field * 4);
            self.put(1, more as u64);
            self.put(2, width - 1);
            self.put(1, (value < 0) as u64);
            self.put((8 * width - 1) as usize, value.unsigned_abs());
        }
        fn entry(&mut self, more: bool, op: u64, mm: &str, pos: u64) {
            self.put(1, more as u64);
            self.put(1, 0);
            self.put(2, op);
            self.put(4, mm.len() as u64);
            for b in mm.bytes() { self.put(8, b as u64); }
            self.put(8, pos);
        }
    }

    // #451: the first entry of a captured frame (AAPL on IEX, 28/09/2026),
    // with the "more" bits of its group and entry cleared: `00 00 7d 98` =
    // server tag 32,152; `00 00` = insert at position 0 without market
    // maker; `06 00 84 c5` = bid price, 3 bytes, 33,989; `80 01` = bid
    // size 1, the last field.
    #[test]
    fn captured_insert_entry() {
        let body = [0x00, 0x60, 0x00, 0x00, 0x7d, 0x98, 0x00, 0x00, 0x06, 0x00, 0x84, 0xc5, 0x80, 0x01];
        let groups = decode_depth(&body, false);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].server_tag, 32_152);
        assert_eq!(groups[0].entries, [DepthEntry {
            op: OP_INSERT, position: 0, market_maker: String::new(), side: Some(Side::Bid),
            price: Some(33_989), size: Some(1),
        }]);
    }

    // #451: operations, market maker, sides from the first field or the
    // delete, negative values, and a second group in the same message.
    #[test]
    fn operations_market_maker_and_groups() {
        let mut w = W::new();
        w.put(1, 1); w.put(31, 0x7fff_fffe);
        w.entry(true, 0, "NSDQ", 3); w.field(1, true, 2, 1234); w.field(5, false, 1, 7);
        w.entry(true, 1, "", 2); w.field(4, false, 1, 5);
        w.entry(true, 2, "", 9);
        w.entry(false, 3, "", 0);
        w.put(1, 0); w.put(31, 44_593);
        w.entry(false, 1, "", 1); w.field(0, false, 2, -1);
        let groups = decode_depth(&w.body(), false);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].server_tag, 0x7fff_fffe);
        let e = &groups[0].entries;
        assert_eq!((e[0].op, e[0].position, e[0].market_maker.as_str(), e[0].side, e[0].price, e[0].size),
            (0, 3, "NSDQ", Some(Side::Ask), Some(1234), Some(7)));
        assert_eq!((e[1].op, e[1].side, e[1].price, e[1].size), (1, Some(Side::Bid), None, Some(5)));
        assert_eq!((e[2].op, e[2].position, e[2].side), (2, 9, Some(Side::Bid)));
        assert_eq!((e[3].op, e[3].side), (3, Some(Side::Ask)));
        assert_eq!(groups[1].server_tag, 44_593);
        assert_eq!(groups[1].entries[0].price, Some(-1));
    }

    // #451: an extended field drops a 35=Y message whole, 35=Z reads it.
    #[test]
    fn extended_field_only_in_35z() {
        let mut w = W::new();
        w.put(1, 0); w.put(31, 9);
        w.entry(false, 0, "", 0);
        w.put(5, 31); w.put(1, 0); w.put(2, 0); w.put(8, 0); w.put(8, 2); w.put(1, 0); w.put(15, 300);
        assert!(decode_depth(&w.body(), false).is_empty());
        let groups = decode_depth(&w.body(), true);
        assert_eq!(groups[0].entries[0].price, Some(300));
    }

    // #451: a group cut short is dropped, the groups before it are kept.
    #[test]
    fn short_group_dropped() {
        let mut w = W::new();
        w.put(1, 1); w.put(31, 5);
        w.entry(false, 2, "", 1);
        w.put(1, 0); w.put(31, 6);
        w.put(1, 1);
        let groups = decode_depth(&w.body(), false);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].server_tag, 5);
    }

    #[test]
    fn body_stops_at_the_signature() {
        let msg = b"8=O\x019=0006\x0135=Y\x01\x00\x08\xAA\x018349=12AB\x01";
        assert_eq!(depth_body(msg), Some((&b"\x00\x08\xAA"[..], false)));
        let msg = b"8=O\x0135=Z\x01\x00\x08\xAA\x01";
        assert_eq!(depth_body(msg), Some((&b"\x00\x08\xAA"[..], true)));
    }
}
