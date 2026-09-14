//! Every trade and every quote change, as the venue packs them.
//!
//! A frame carries records until the bits run out, each headed by the
//! subscription it belongs to and the second it happened, so one frame can hold
//! more than one moment. There is no marker between records and no length in
//! front of one: a record ends when the fields its kind has are read, and each
//! field ends itself — a variable-length integer by its continuation bit, a
//! string by its own length.
//!
//! A price is not sent. What is sent is how far it moved, in whole ticks of the
//! contract's own smallest increment, added to where the price already was.
//! That running price starts at zero when a subscription opens, so the first
//! record's move is the whole price rather than a small step — a decoder that
//! expects a small step rejects the first record of every subscription.

/// Reads the bits of a frame, in the order the venue wrote them.
///
/// Every read is from the same position, so a field read wrongly does not
/// return a wrong value and carry on: it leaves the position in the middle of
/// the next field, and everything after it is nonsense. That is why a kind's
/// field count is followed exactly rather than guessed at.
pub struct Bits<'a> {
    bytes: &'a [u8],
    /// Position in bits, not bytes.
    at: usize,
}

impl<'a> Bits<'a> {
    /// Read from the start of these bytes.
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    /// How many bits are left unread.
    pub fn remaining(&self) -> usize {
        (self.bytes.len() * 8).saturating_sub(self.at)
    }

    fn bit(&mut self) -> Option<u8> {
        let byte = self.bytes.get(self.at / 8)?;
        let shift = 7 - (self.at % 8);
        self.at += 1;
        Some((byte >> shift) & 1)
    }

    /// One octet, which need not be byte-aligned.
    fn octet(&mut self) -> Option<u8> {
        let mut out = 0u8;
        for _ in 0..8 {
            out = (out << 1) | self.bit()?;
        }
        Some(out)
    }

    /// A whole number, seven bits at a time, most significant first.
    ///
    /// The top bit marks the LAST octet, not a further one — the opposite of
    /// the usual convention, and settled against a real frame: read the usual
    /// way the same bytes give no time anywhere and no price that exists, and
    /// read this way they give the second and a quote that was on the screen.
    pub fn unsigned(&mut self) -> Option<u64> {
        let mut acc: u64 = 0;
        loop {
            let octet = self.octet()?;
            acc = (acc << 7) | u64::from(octet & 0x7F);
            if octet & 0x80 != 0 {
                return Some(acc);
            }
        }
    }

    /// A whole number that may be negative.
    ///
    /// The sign is bit six of the leading octet, and it is not removed from the
    /// value: it seeds the accumulator with all ones and then stays put, which
    /// is ordinary two's-complement sign extension seven bits at a time. Masking
    /// it away as though it were a flag makes every negative move wrong.
    ///
    /// The top bit marks the last octet, as it does for a plain whole number.
    pub fn signed(&mut self) -> Option<i64> {
        let first = self.octet()?;
        let mut acc: i64 = if first & 0x40 != 0 { -1 } else { 0 };
        acc = (acc << 7) | i64::from(first & 0x7F);
        if first & 0x80 != 0 {
            return Some(acc);
        }
        loop {
            let octet = self.octet()?;
            acc = (acc << 7) | i64::from(octet & 0x7F);
            if octet & 0x80 != 0 {
                return Some(acc);
            }
        }
    }

    /// A string, one character per octet, ending where it says it ends.
    ///
    /// There is no length in front of it. The last character carries the top
    /// bit, exactly as the last octet of a number does — one convention for
    /// the whole format rather than two.
    ///
    /// Read as though a length came first, the first character of a venue's
    /// name was taken for one: `ARCA` announced sixty-five characters where
    /// four followed, and every field after it was read from the wrong place.
    pub fn text(&mut self) -> Option<String> {
        let mut out = String::new();
        loop {
            let octet = self.octet()?;
            // A string with nothing in it is the end marker alone. Appending
            // its low bits regardless gave callers a name one character long
            // that was not a character.
            if octet == 0x80 {
                return Some(out);
            }
            out.push((octet & 0x7F) as char);
            if octet & 0x80 != 0 {
                return Some(out);
            }
            // A string that never ends is a misread, not a string: nothing in
            // a frame outlives the frame.
            if out.len() * 8 > self.bytes.len() * 8 {
                return None;
            }
        }
    }
}

/// Which kind of record a subscription carries, and how many fields one has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TbtKind {
    /// Trades that print to the tape.
    Last,
    /// Every trade, printed or not.
    AllLast,
    /// Changes to the top of the book.
    BidAsk,
    /// Changes to the midpoint.
    MidPoint,
}

impl TbtKind {
    /// How many fields a record of this kind nominally carries.
    ///
    /// A description of the format, not a bound the reader is held to: a trade
    /// record can restate its size at greater width, so `read_record` reads
    /// what each kind's own fields say rather than counting to this. It is
    /// here because it is the shape the venue writes, and what a reader of the
    /// format wants to know first.
    pub fn fields(self) -> usize {
        match self {
            Self::Last | Self::AllLast | Self::BidAsk => 5,
            Self::MidPoint => 2,
        }
    }
}

/// What the venue said about a trade.
#[derive(Debug, Clone, PartialEq)]
pub struct TbtTradeRecord {
    /// The price, in the units the record carries.
    pub price: f64,
    /// How much.
    pub size: u64,
    /// The venue may still revise this print.
    pub past_limit: bool,
    /// The print did not go to the tape.
    pub unreported: bool,
    /// Which venue.
    pub exchange: String,
    /// What the venue notes about the trade.
    pub conditions: String,
}

/// What the venue said about the top of the book.
#[derive(Debug, Clone, PartialEq)]
pub struct TbtQuoteRecord {
    /// The bid.
    pub bid: f64,
    /// The ask.
    pub ask: f64,
    /// How much at the bid.
    pub bid_size: u64,
    /// How much at the ask.
    pub ask_size: u64,
    /// Whether the bid is below the day's low.
    pub bid_past_low: bool,
    /// Whether the ask is above the day's high.
    pub ask_past_high: bool,
}

/// One record.
#[derive(Debug, Clone, PartialEq)]
pub enum TbtRecord {
    /// A trade.
    Trade(TbtTradeRecord),
    /// A change to the top of the book.
    Quote(TbtQuoteRecord),
    /// Changes to the midpoint.
    MidPoint {
        /// The midpoint now, counted in the contract's own smallest step —
        /// the same form the two shapes beside it state their prices in, so
        /// one scale turns any of them into money.
        ticks: i64,
    },
}

/// The smallest price step this representation can hold.
///
/// A move is stated in whole increments of the contract's own smallest one, and
/// that increment comes from the venue — nothing here assumes a scale for a
/// currency or a kind of contract. What is fixed is how a price is held once
/// worked out, and a contract whose increment is finer than this cannot be
/// held at all. A satoshi sits exactly on it.
pub const SMALLEST_STEP: f64 = 1.0 / 100_000_000.0;

// A satoshi sits exactly on the limit. If the representation is ever coarsened,
// nothing should compile rather than crypto prices quietly rounding to nothing.
const _: () = assert!(
    SMALLEST_STEP <= 1e-8,
    "a price step of a hundred-millionth must remain representable"
);

/// Where a subscription's prices have got to.
///
/// Kept for the life of a subscription and never reset between records: a move
/// is measured from the last price, not from nothing. Held as a whole number of
/// ticks rather than as a price, so a session's worth of additions cannot drift
/// the way repeatedly adding fractions would.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RunningPrice {
    last_ticks: i64,
    bid_ticks: i64,
    ask_ticks: i64,
    mid_ticks: i64,
}

/// One record, and the second the venue stamped that record with.
#[derive(Debug, Clone, PartialEq)]
pub struct TbtStamped {
    /// Seconds since the epoch, from this record's own header.
    pub seconds: u64,
    /// What the record says.
    pub record: TbtRecord,
}

/// A frame: one subscription and the records it carries.
#[derive(Debug, Clone, PartialEq)]
pub struct TbtFrame {
    /// The venue's number for the stream.
    pub ticker_id: u64,
    /// The records in this frame, each with the moment it happened.
    pub records: Vec<TbtStamped>,
}

/// Read every record in a frame.
///
/// A frame opens with two bytes saying how many bits of payload follow, and the
/// payload is what those bits say it is rather than whatever is left in the
/// message — a message carries a field after the payload, and reading to the
/// end of it swallows that field as though it were data.
///
/// Every record then states which subscription it belongs to and when it
/// happened, so a frame can carry records of more than one moment. A reading
/// that takes those once for the whole frame consumes the second record's
/// header as though it were the first record's prices.
///
/// `min_tick` is the contract's own smallest increment, which is what a price
/// move is counted in. `running` carries prices forward and must be the same
/// one across every frame of a subscription.
pub fn decode_frame(
    body: &[u8],
    kind: TbtKind,
    min_tick: f64,
    running: &mut RunningPrice,
) -> Option<TbtFrame> {
    if body.len() < 2 {
        return None;
    }
    // The count is two bytes and wraps at sixty-five thousand bits, which is
    // eight kilobytes of records — a batch that size states nought. Recovered
    // against how much arrived, the way every other bit-counted section of this
    // wire is: read as stated, a frame at the wrap decoded as no records at
    // all, and one past it as its own first eight kilobytes.
    let bits_stated = crate::protocol::tick_decoder::bits_carried(
        u16::from_be_bytes([body[0], body[1]]),
        body.len(),
    );
    let bytes_stated = bits_stated.div_ceil(8);
    // A length the bytes do not satisfy is a frame cut short: refused rather
    // than read as the shorter frame it is not, which would deliver part of
    // a moment's records as the whole of them.
    let payload = body.get(2..2 + bytes_stated)?;

    let mut bits = Bits::new(payload);
    let mut records = Vec::new();
    let mut ticker_id = 0;

    // A record needs at least its own header, so anything shorter is what is
    // left over rather than a record that failed to read.
    while bits.remaining() >= 16 {
        let Some(id) = bits.unsigned() else { break };
        let Some(at) = bits.unsigned() else { break };
        match read_record(&mut bits, kind, min_tick, running) {
            Some(record) => {
                ticker_id = id;
                // Kept with the record it came from. Held once for the frame,
                // the last record's second overwrote every earlier one, and a
                // frame carrying a second's worth of prints stamped them all
                // at the moment the last of them happened.
                records.push(TbtStamped { seconds: at, record });
            }
            None => break,
        }
    }

    Some(TbtFrame { ticker_id, records })
}

/// Which subscription a frame's records belong to, as the frame states it.
///
/// Every record names it, and they agree within a frame. Reading it without
/// decoding the rest lets a caller route the frame before it knows how to read
/// what is in it.
pub fn frame_ticker_id(body: &[u8]) -> Option<u64> {
    if body.len() < 2 {
        return None;
    }
    // Read the way the frame itself reads it, or the two disagree about where
    // one frame's records end.
    let bits_stated = crate::protocol::tick_decoder::bits_carried(
        u16::from_be_bytes([body[0], body[1]]),
        body.len(),
    );
    let payload = body.get(2..2 + bits_stated.div_ceil(8))?;
    Bits::new(payload).unsigned()
}

/// A size, which the venue may state in one number or in two.
///
/// The two-number form sends the low half first. Reading one number where it
/// sent two leaves the second half in the stream and every field after it is
/// read from the wrong place — which is worse than a wrong size, because it is
/// a wrong everything.
fn size(bits: &mut Bits<'_>, extended: bool) -> Option<u64> {
    if extended {
        let low = bits.unsigned()?;
        let high = bits.unsigned()?;
        Some(low | (high << 32))
    } else {
        bits.unsigned()
    }
}

fn read_record(
    bits: &mut Bits<'_>,
    kind: TbtKind,
    min_tick: f64,
    running: &mut RunningPrice,
) -> Option<TbtRecord> {
    // Every kind opens with a price move.
    let first_move = bits.signed()?;

    match kind {
        TbtKind::Last | TbtKind::AllLast => {
            // Where the price would stand. A move is a step from the last one,
            // so it is committed only once the whole record has been read: a
            // record that runs out part way through leaves the baseline where
            // it was, rather than shifting every price after it by a step
            // belonging to a record nobody could read.
            let last_ticks = running.last_ticks.checked_add(first_move)?;
            let price = last_ticks as f64 * min_tick;
            // The flags, then the size — the order the quote beside this one
            // is read in, and the order a frame the venue sent is written in.
            //
            // Read the other way round every trade came back sized nought,
            // with the marks taken off the low bits of the size: a tape where
            // nothing traded and a quarter of the prints were away from the
            // exchange, on a contract whose prints are ones and twos. Six
            // frames the venue sent, decoded both ways: this way the sizes are
            // 2, 1, 1, 2, 1, 4, 1, 1, 1, 6, 1, 1, 1 and no mark is set, which
            // is what that tape was; the other way all thirteen are nought.
            // The prices and the seconds are the same either way — they are
            // read before this — and they were right all along, which is why
            // nothing looked wrong.
            let flags = bits.unsigned()?;
            // Bit five says the size is stated again, at greater width; bit
            // four says that restatement comes in two numbers.
            let size = if flags & (1 << 5) != 0 {
                size(bits, flags & (1 << 4) != 0)?
            } else {
                bits.unsigned()?
            };
            let record = TbtRecord::Trade(TbtTradeRecord {
                price,
                size,
                past_limit: flags & 1 != 0,
                unreported: flags & (1 << 1) != 0,
                exchange: bits.text()?,
                conditions: bits.text()?,
            });
            running.last_ticks = last_ticks;
            Some(record)
        }
        TbtKind::BidAsk => {
            // Both moves, then the flags, then a size a side. Settled against a
            // real frame: read with the sizes before the flags, five records
            // consumed eighty-six bytes and gave prices no market has ever
            // quoted; read this way the same bytes give the quote that was on
            // the screen and consume the payload exactly.
            let ask_move = bits.signed()?;
            // Committed after the record reads, as above.
            let bid_ticks = running.bid_ticks.checked_add(first_move)?;
            let ask_ticks = running.ask_ticks.checked_add(ask_move)?;
            let flags = bits.unsigned()?;
            let bid_size = size(bits, flags & (1 << 2) != 0)?;
            let ask_size = size(bits, flags & (1 << 3) != 0)?;
            let record = TbtRecord::Quote(TbtQuoteRecord {
                bid: bid_ticks as f64 * min_tick,
                ask: ask_ticks as f64 * min_tick,
                bid_size,
                ask_size,
                bid_past_low: flags & 1 != 0,
                ask_past_high: flags & (1 << 1) != 0,
            });
            running.bid_ticks = bid_ticks;
            running.ask_ticks = ask_ticks;
            Some(record)
        }
        TbtKind::MidPoint => {
            let mid_ticks = running.mid_ticks.checked_add(first_move)?;
            // Two fields: the move, and a flag word. No size, no venue, no text.
            let _flags = bits.unsigned()?;
            running.mid_ticks = mid_ticks;
            Some(TbtRecord::MidPoint { ticks: mid_ticks })
        }
    }
}

/// A quote frame the venue sent on the tenth of August 2026, on a currency
/// pair that was 1.15510 bid at 1.15515: five records under one stream number.
#[cfg(test)]
/// A trade frame the venue sent, byte for byte.
///
/// Five prints on the smaller S&P future, taken at 22:03 UTC on the thirteenth
/// of September 2026, when it was trading either side of 7,687. The contract
/// moves in a quarter point and its sizes are counted in whole contracts.
///
/// The quote frame below it is what found the last fault in this module. This
/// is the same kind of evidence for the other kind of record, and it is here
/// because a made-up trade frame cannot tell the size from the marks: both are
/// whole numbers, written the same way, one after the other. Every check on
/// this arm was written against frames this file made up, and they all passed
/// while every size the venue sent came back as nought.
pub(crate) const A_CAPTURED_TRADE_FRAME: &str = "383d4f01393d303037370133353d450101b8\
    8106551c3ba580808180808106551c3ba5ff808280808106551c3ba5ff8081808081\
    06551c3ba582808480808106551c3ba6818081808001383334393d41383537313034\
    3501";

pub(crate) const A_CAPTURED_QUOTE_FRAME: &str = "383d4f01393d303130380133353d450102b08106536549c40134be0134bf80\
    0e1765f03d04c08106536549c48080800f4e73b03d04c08106536549c48080\
    800e1765f03d04c08106536549c48081800e1765f00d5a61b08106536549c4\
    8080800d7923d00d5a61b001383334393d363932384333303801";

#[cfg(test)]
mod tests {

    use super::*;

    /// Write the wire encodings, so a test frame is built the way a
    /// frame is built rather than by hand.
    ///
    /// These agreed with the decoder while both were wrong about which bit ends
    /// a number, and every test built on them passed while nothing the venue
    /// sent could be read. They are kept for the cases a capture does not cover,
    /// and the capture is what settles the encoding.
    #[derive(Default)]
    struct Writer {
        out: Vec<u8>,
    }

    impl Writer {
        fn unsigned(&mut self, mut v: u64) -> &mut Self {
            let mut groups = Vec::new();
            loop {
                groups.push((v & 0x7F) as u8);
                v >>= 7;
                if v == 0 {
                    break;
                }
            }
            groups.reverse();
            let last = groups.len() - 1;
            for (i, g) in groups.iter().enumerate() {
                self.out.push(if i == last { g | 0x80 } else { *g });
            }
            self
        }

        fn signed(&mut self, v: i64) -> &mut Self {
            // Seven bits at a time, most significant first, stopping once what
            // is left is only sign.
            let mut groups = Vec::new();
            // The highest seven-bit group of a 64-bit value.
            let mut shift: i32 = 63;
            let mut started = false;
            while shift >= 0 {
                let g = ((v >> shift) & 0x7F) as u8;
                let sign_only = if v < 0 { g == 0x7F } else { g == 0 };
                if started || !sign_only || shift == 0 {
                    started = true;
                    groups.push(g);
                }
                shift -= 7;
            }
            // The leading group must carry the sign in bit six.
            if let Some(first) = groups.first().copied() {
                let sign_set = first & 0x40 != 0;
                if (v < 0) != sign_set {
                    groups.insert(0, if v < 0 { 0x7F } else { 0x00 });
                }
            }
            let last = groups.len() - 1;
            for (i, g) in groups.iter().enumerate() {
                self.out.push(if i == last { g | 0x80 } else { *g });
            }
            self
        }

    }

    /// The sizes and the marks on a trade frame the venue actually sent.
    ///
    /// Both are whole numbers written the same way, one after the other, so a
    /// frame this file makes up cannot tell which order they come in — it is
    /// written in whatever order the reader expects and read back the same
    /// way. Only a frame the venue sent can say, and read the wrong way round
    /// this one gives five prints of nought size with marks on four of them,
    /// on a contract whose prints are ones and twos and which had no
    /// off-exchange tape at all.
    #[test]
    fn a_trade_frame_the_venue_sent_states_sizes_and_no_marks() {
        let message: String = A_CAPTURED_TRADE_FRAME.split_whitespace().collect();
        let bytes: Vec<u8> = (0..message.len() / 2)
            .map(|i| u8::from_str_radix(&message[i * 2..i * 2 + 2], 16).unwrap())
            .collect();
        let start = bytes.windows(5).position(|w| w == b"35=E\x01").unwrap() + 5;
        let end = bytes.windows(6).position(|w| w == b"\x018349=").unwrap();
        let body = &bytes[start..end];

        let mut running = RunningPrice::default();
        let frame = decode_frame(body, TbtKind::AllLast, 0.25, &mut running)
            .expect("the frame decodes");
        assert_eq!(frame.records.len(), 5, "five prints in the frame");

        let mut sizes = Vec::new();
        for stamped in &frame.records {
            match &stamped.record {
                TbtRecord::Trade(t) => {
                    sizes.push(t.size);
                    assert!(!t.past_limit, "a future has no print away from its exchange");
                    assert!(!t.unreported, "nor one kept off the tape");
                }
                other => panic!("a trade subscription carried {other:?}"),
            }
        }
        assert_eq!(
            sizes, vec![1, 2, 1, 4, 1],
            "the sizes the venue stated, not the low bits of its marks",
        );
    }

    /// A frame captured from the venue, byte for byte.
    ///
    /// This is the whole point of the module and the only test that could have
    /// found what was wrong. Every earlier check was written against frames
    /// this file made up, and they all passed while nothing the venue sent
    /// could be read at all.
    ///
    /// It is a quote subscription on a currency pair, taken at 05:42:28 UTC on
    /// the tenth of August 2026, when the market was 1.15510 bid at 1.15515.
    #[test]
    fn a_frame_the_venue_actually_sent_decodes_to_the_market_that_was_there() {
        let message = A_CAPTURED_QUOTE_FRAME;
        let bytes: Vec<u8> = (0..message.len() / 2)
            .map(|i| u8::from_str_radix(&message[i * 2..i * 2 + 2], 16).unwrap())
            .collect();

        // The payload sits between the type field and the field after it. A
        // message's own length cannot be used to find the end: the payload
        // contains the byte that separates fields.
        let start = bytes.windows(5).position(|w| w == b"35=E\x01").unwrap() + 5;
        let end = bytes.windows(6).position(|w| w == b"\x018349=").unwrap();
        let body = &bytes[start..end];

        // A currency pair moves in half a pip.
        let mut running = RunningPrice::default();
        let frame = decode_frame(body, TbtKind::BidAsk, 0.00005, &mut running)
            .expect("the frame decodes");

        // Seconds, as the wire states them and as the reference client's
        // `tickByTickAllLast` hands them on. Scaling to milliseconds places
        // every print in the year 58553 when read as a moment.
        assert_eq!(frame.records.len(), 5, "five records in the frame");
        // Each record carries its own second rather than the frame's last.
        for (at, stamped) in frame.records.iter().enumerate() {
            assert_eq!(stamped.seconds, 1_786_340_548, "record {at}");
        }

        match &frame.records[0].record {
            TbtRecord::Quote(q) => {
                assert!((q.bid - 1.15510).abs() < 1e-9, "the bid was {}", q.bid);
                assert!((q.ask - 1.15515).abs() < 1e-9, "the ask was {}", q.ask);
                assert_eq!(q.bid_size, 29_750_000);
                assert_eq!(q.ask_size, 1_000_000);
            }
            other => panic!("expected a quote, got {other:?}"),
        }

        // The first record states the whole price as a move from nothing; the
        // rest move from it, and a record that does not move the price is a
        // size change.
        match &frame.records[1].record {
            TbtRecord::Quote(q) => {
                assert!((q.bid - 1.15510).abs() < 1e-9, "unmoved, the bid stands");
                assert_eq!(q.bid_size, 32_750_000, "and the size changed");
            }
            other => panic!("expected a quote, got {other:?}"),
        }

        // The ask moves one increment on the fourth record.
        match &frame.records[3].record {
            TbtRecord::Quote(q) => {
                assert!((q.ask - 1.15520).abs() < 1e-9, "the ask was {}", q.ask);
            }
            other => panic!("expected a quote, got {other:?}"),
        }
    }

    /// A number that may be negative survives being written and read back,
    /// including the boundaries where the sign bit sits.
    #[test]
    fn a_move_that_may_be_negative_reads_back_as_itself() {
        for v in [0i64, 1, -1, 63, -63, 64, -64, 8191, -8191, 1_000_000, -1_000_000] {
            let mut w = Writer::default();
            w.signed(v);
            let mut bits = Bits::new(&w.out);
            assert_eq!(bits.signed(), Some(v), "{v} did not read back");
        }
    }

    /// Build a frame the way the venue builds one: a two-byte bit length, then
    /// records that each state their own subscription and moment.
    fn frame(records: &[Vec<u8>]) -> Vec<u8> {
        let mut payload = Vec::new();
        for r in records {
            payload.extend_from_slice(r);
        }
        let mut out = ((payload.len() * 8) as u16).to_be_bytes().to_vec();
        out.extend_from_slice(&payload);
        out
    }

    /// A batch at the wrap of the bit count is read whole.
    ///
    /// The count is two bytes, so eight kilobytes of records state nought and a
    /// batch past that states only what is left over. Read as stated, such a
    /// batch decoded as no records at all, or as its own first eight
    /// kilobytes, with every record after that gone and nothing saying so.
    /// Every other bit-counted section of this wire recovers it against how
    /// much arrived.
    #[test]
    fn a_batch_at_the_wrap_of_the_bit_count_is_read_whole() {
        let one = quote(23_102, 23_103, 1, 1);
        let mut payload = Vec::new();
        while payload.len() + one.len() <= 8192 {
            payload.extend_from_slice(&one);
        }
        payload.resize(8192, 0);
        // Exactly sixty-five thousand five hundred and thirty-six bits, which
        // the count states as nought.
        let mut body = vec![0x00, 0x00];
        body.extend_from_slice(&payload);
        assert_eq!(&body[..2], &[0x00, 0x00], "the count wrapped");

        let mut running = RunningPrice::default();
        let frame = decode_frame(&body, TbtKind::BidAsk, 0.00005, &mut running)
            .expect("a frame at the wrap is read");
        assert!(
            frame.records.len() > 1,
            "every record in it is read, not nought of them: {}",
            frame.records.len(),
        );
        assert_eq!(
            frame_ticker_id(&body), Some(frame.ticker_id),
            "and the two readers of the count agree where the records end",
        );
    }

    /// One quote record, headed by its subscription and its moment.
    fn quote(bid_move: i64, ask_move: i64, bid_size: u64, ask_size: u64) -> Vec<u8> {
        quote_at(1_786_340_548, bid_move, ask_move, bid_size, ask_size)
    }

    /// The same, at a moment the caller names.
    fn quote_at(at: u64, bid_move: i64, ask_move: i64, bid_size: u64, ask_size: u64) -> Vec<u8> {
        let mut w = Writer::default();
        w.unsigned(1).unsigned(at);
        w.signed(bid_move).signed(ask_move).unsigned(0).unsigned(bid_size).unsigned(ask_size);
        w.out
    }

    /// A frame can carry records from more than one second, and each states
    /// its own. Held once for the frame, the last record's second was put on
    /// every earlier one and the prints before it moved forward in time.
    #[test]
    fn each_record_keeps_the_second_it_states() {
        let body = frame(&[
            quote_at(1_786_340_548, 23_102, 23_103, 1, 1),
            quote_at(1_786_340_549, 1, 1, 1, 1),
            quote_at(1_786_340_551, 1, 1, 1, 1),
        ]);
        let mut running = RunningPrice::default();
        let f = decode_frame(&body, TbtKind::BidAsk, 0.00005, &mut running).expect("a frame");
        let seconds: Vec<u64> = f.records.iter().map(|r| r.seconds).collect();
        assert_eq!(seconds, vec![1_786_340_548, 1_786_340_549, 1_786_340_551]);
    }

    /// The first record states the whole price as a move from nothing, and
    /// later records move from it. A decoder treating a large first move as a
    /// fault rejects the first record of every subscription.
    #[test]
    fn a_move_is_measured_from_where_the_price_already_was() {
        let body = frame(&[quote(23_102, 23_103, 1, 1), quote(0, 1, 1, 1), quote(-2, 0, 1, 1)]);
        let mut running = RunningPrice::default();
        let f = decode_frame(&body, TbtKind::BidAsk, 0.00005, &mut running).expect("a frame");
        assert_eq!(f.records.len(), 3);
        let bids: Vec<f64> = f
            .records
            .iter()
            .map(|r| match &r.record {
                TbtRecord::Quote(q) => q.bid,
                other => panic!("expected a quote, got {other:?}"),
            })
            .collect();
        assert!((bids[0] - 1.15510).abs() < 1e-9, "{bids:?}");
        assert!((bids[1] - 1.15510).abs() < 1e-9, "a record that does not move it leaves it");
        assert!((bids[2] - 1.15500).abs() < 1e-9, "and a move down moves it down");
    }

    /// A record that runs out part way through moves nothing.
    ///
    /// Prices arrive as steps from the last one, so a step taken from a record
    /// that could not be read is not a price that was wrong once — it is an
    /// offset carried into every price on that stream afterwards, including
    /// across frames.
    #[test]
    fn a_record_that_cannot_be_read_leaves_the_price_where_it_was() {
        let mut running = RunningPrice::default();

        // A whole frame first, so there is a baseline to corrupt.
        decode_frame(&frame(&[quote(23_102, 23_103, 1, 1)]), TbtKind::BidAsk, 0.00005, &mut running)
            .expect("a frame");
        let baseline = (running.bid_ticks, running.ask_ticks);

        // A record that states its header and both moves and then stops: the
        // moves read, and nothing after them does.
        let cut = {
            let mut w = Writer::default();
            w.unsigned(1).unsigned(1_786_340_548);
            w.signed(500).signed(500);
            frame(&[w.out])
        };
        let _ = decode_frame(&cut, TbtKind::BidAsk, 0.00005, &mut running);

        assert_eq!(
            (running.bid_ticks, running.ask_ticks), baseline,
            "a record nobody could read does not move the price",
        );

        // And the next good frame is measured from where the price really was.
        let f = decode_frame(&frame(&[quote(1, 1, 1, 1)]), TbtKind::BidAsk, 0.00005, &mut running)
            .expect("a frame");
        match &f.records[0].record {
            TbtRecord::Quote(q) => assert!((q.bid - 1.15515).abs() < 1e-9, "{}", q.bid),
            other => panic!("expected a quote, got {other:?}"),
        }
    }

    /// The running price carries across frames, not only across records.
    #[test]
    fn the_price_carries_from_one_frame_to_the_next() {
        let mut running = RunningPrice::default();
        decode_frame(&frame(&[quote(23_102, 23_103, 1, 1)]), TbtKind::BidAsk, 0.00005, &mut running)
            .expect("a frame");
        let f = decode_frame(&frame(&[quote(1, 1, 1, 1)]), TbtKind::BidAsk, 0.00005, &mut running)
            .expect("a frame");
        match &f.records[0].record {
            TbtRecord::Quote(q) => assert!((q.bid - 1.15515).abs() < 1e-9, "{}", q.bid),
            other => panic!("expected a quote, got {other:?}"),
        }
    }

    /// A frame stating more bits than it holds is a frame cut short, and is
    /// refused rather than read as the shorter frame it is not: read short,
    /// part of a moment's records would arrive as the whole of them.
    #[test]
    fn a_frame_cut_short_by_its_own_length_is_refused() {
        let body = frame(&[quote(23_102, 23_103, 1, 1)]);
        let cut = &body[..body.len() - 3];
        let mut running = RunningPrice::default();
        assert!(
            decode_frame(cut, TbtKind::BidAsk, 0.00005, &mut running).is_none(),
            "a frame short of its stated length is not a frame",
        );
        assert!(frame_ticker_id(cut).is_none(), "and it names no subscription");
        assert_eq!(running, RunningPrice::default(), "nothing it carried was read");
    }

    /// The stated bit length bounds the payload. A message carries a field
    /// after it, and reading to the end of the message swallows that field as
    /// though it were data.
    #[test]
    fn the_stated_length_bounds_the_payload() {
        let mut body = frame(&[quote(23_102, 23_103, 1, 1)]);
        // Whatever follows is not the venue's data.
        body.extend_from_slice(b"8349=DEADBEEF");
        let mut running = RunningPrice::default();
        let f = decode_frame(&body, TbtKind::BidAsk, 0.00005, &mut running).expect("a frame");
        assert_eq!(f.records.len(), 1, "the trailing field was not read as a record");
    }

    /// Every kind states how many fields it has, and a decoder that reads a
    /// different number desynchronises everything after it.
    #[test]
    fn each_kind_states_its_field_count() {
        assert_eq!(TbtKind::Last.fields(), 5);
        assert_eq!(TbtKind::AllLast.fields(), 5);
        assert_eq!(TbtKind::BidAsk.fields(), 5);
        assert_eq!(TbtKind::MidPoint.fields(), 2);
    }
}

#[cfg(test)]
mod scale_tests {
    /// Nothing here assumes a scale for a currency or a kind of contract: the
    /// increment comes from the venue, per contract, and the same decoder reads
    /// a currency pair quoted in half pips and a share quoted in pennies.
    #[test]
    fn the_increment_comes_from_the_contract_not_from_a_table() {
        let moves = 23_102i64;
        // A currency pair moves in half a pip.
        assert!((moves as f64 * 0.00005 - 1.15510).abs() < 1e-9);
        // The same count of moves on a share quoted in pennies is a different
        // price entirely, and neither is assumed.
        assert!((moves as f64 * 0.01 - 231.02).abs() < 1e-9);
    }

}


#[cfg(test)]
mod text_tests {
    use super::Bits;

    /// A venue's name, from a real trade on a real listing: four characters,
    /// the last carrying the top bit. Read as though a length came first, the
    /// `A` was taken for one and sixty-five characters were expected where
    /// four followed.
    #[test]
    fn a_name_ends_where_it_says_it_ends() {
        let wire = [0x41, 0x52, 0x43, 0xc1];
        let mut bits = Bits::new(&wire);
        assert_eq!(bits.text().as_deref(), Some("ARCA"));
        assert_eq!(bits.remaining(), 0, "it read exactly its own length");
    }

    /// The conditions on that same trade, which follow the name.
    #[test]
    fn one_string_follows_another() {
        let wire = [0x41, 0x52, 0x43, 0xc1, 0x20, 0x46, 0x20, 0xc9];
        let mut bits = Bits::new(&wire);
        assert_eq!(bits.text().as_deref(), Some("ARCA"));
        assert_eq!(bits.text().as_deref(), Some(" F I"));
    }

    /// A trade with no conditions on it carries the end marker alone, and
    /// that is a string with nothing in it. Read as a character it gave
    /// callers a name one character long that was not a character.
    #[test]
    fn a_string_with_nothing_in_it_is_empty() {
        let wire = [0x80, 0x41, 0xc2];
        let mut bits = Bits::new(&wire);
        assert_eq!(bits.text().as_deref(), Some(""));
        assert_eq!(bits.text().as_deref(), Some("AB"), "and the next one follows it");
    }

    /// A string that never ends is a misread, not a string.
    #[test]
    fn a_string_that_never_ends_is_refused() {
        let wire = [0x41; 8];
        let mut bits = Bits::new(&wire);
        assert!(bits.text().is_none());
    }
}
