use std::time::Instant;

use crate::bridge::{Event, SharedState};
use crate::protocol::datetime::chrono_free_timestamp;
use crate::engine::context::Context;
use crate::protocol::connection::{Connection, Frame};
use crate::protocol::fix;
use crate::protocol::fixcomp;
use crate::protocol::tick_decoder;
use crate::types::{qty_from_counted, InstrumentId};

use super::{HeartbeatState, ReplayPacing, emit, fast_extract_msg_type, find_body_after_tag, EventSink};

/// The venue and security type exactly as a subscription states them on the
/// wire: the caller's blanks filled, and both in the wire's own spelling.
///
/// A withdrawal states an entry the way the subscription stated it, so the
/// one is worked out from the same answer as the other.
fn stated_venue_and_type<'a>(sec_type: &'a str, exchange: &'a str) -> (&'a str, &'a str) {
    let exchange = if exchange.is_empty() { "SMART" } else { exchange };
    (
        crate::control::contracts::exchange_to_fix(exchange),
        crate::control::contracts::sec_type_to_fix(sec_type),
    )
}

/// Build the 35=V subscribe tag list for a contract whose conId is known.
///
/// Kept pure so the wire shape stays unit-testable. SecurityType (167) and
/// Exchange (207) must describe the actual contract: the server routes the
/// subscription by them, and a mismatch is answered with a partial ack rather
/// than an error.
fn build_conid_subscribe_tags(
    realtime: bool,
    regulatory_snapshot: bool,
    bid_ask_id: u32,
    last_id: u32,
    con_id: i64,
    exchange: &str,
    sec_type: &str,
    mode_9887: i32,
    ts: &str,
    extra_series: &[(u32, u32)],
) -> Vec<(u32, String)> {
    let con_id_str = (con_id as u32).to_string();
    // Subscribing by conId alone is a supported shape — `Contract` defaults
    // both fields to empty, and the in-tree benchmark does exactly that. Keep
    // the smart-routed stock those callers expect rather than sending an empty
    // SecurityType and Exchange, which the venue answers with a partial ack.
    // A subscription naming only a contract id is not answered; tags 167 and
    // 207 must both be present. Confirmed against a live session: the same
    // contract is answered with them and silent without them.
    //
    // What the contract IS arrives stated: the engine fills a caller's blanks
    // from the venue's own definition and reports the subscription where
    // neither says, because a security type invented here subscribes to some
    // other kind of instrument under this contract's id.
    //
    // Where it is ROUTED is a different question and has a default. The venue
    // requires tag 207 — a subscription omitting it is answered with nothing,
    // measured — and a caller who states a type and no venue means the smart
    // route, which is what every example written against the reference client
    // states for one.
    debug_assert!(!sec_type.is_empty(), "contract {con_id} reached the wire untyped");
    let (fix_exchange, fix_sec_type) = stated_venue_and_type(sec_type, exchange);

    // The chargeable snapshot is a request type of its own and one entry.
    // Every stream is the same pair, BID_ASK and LAST, whichever feed serves
    // it: the feed is named beside the entries on 9887 and does not change
    // what they ask for. Asked for as one entry instead, a delayed or frozen
    // subscription has no number for what last traded, so the venue's answer
    // to that half arrives under nothing and the caller's last-trade price,
    // size and time never move.
    let mut entries: Vec<(u32, String)> = if regulatory_snapshot {
        vec![(bid_ask_id, REGULATORY_SNAPSHOT_REQUEST_TYPE.to_string())]
    } else {
        vec![(bid_ask_id, "442".to_string()), (last_id, "443".to_string())]
    };
    // Every extra series the caller named, as further entries of this same
    // request rather than requests of their own. The venue is sent one message
    // carrying as many entries as fit, each under its own number, with the
    // count ahead of them — so a series asked for on its own message states a
    // count of one where the venue states the whole list.
    //
    // The chargeable snapshot is a request type of its own and carries none.
    if !regulatory_snapshot {
        entries.extend(extra_series.iter().map(|(req_id, tick)| (*req_id, tick.to_string())));
    }
    // 146 = NoRelatedSym: how many entries follow, counted rather than stated
    // per shape, so a shape added here cannot state the wrong number.
    let mut tags: Vec<(u32, String)> = vec![
        (fix::TAG_MSG_TYPE, fix::MSG_MARKET_DATA_REQ.to_string()),
        (fix::TAG_SENDING_TIME, ts.to_string()),
        (263, if regulatory_snapshot { SNAPSHOT_ACTION } else { SUBSCRIBE_ACTION }.to_string()),
        (146, entries.len().to_string()),
    ];

    for (req_id, depth) in &entries {
        tags.push((262, req_id.to_string()));
        tags.push((6008, con_id_str.clone()));
        tags.push((207, fix_exchange.to_string()));
        tags.push((167, fix_sec_type.to_string()));
        tags.push((264, depth.to_string()));
        tags.push((6088, "Socket".to_string()));
        tags.push((9830, "1".to_string()));
        tags.push((9839, "1".to_string()));
        // Named only where it selects between the feeds the venue serves a
        // stream from. The chargeable snapshot is served from none of them and
        // is asked for without it.
        if !realtime && !regulatory_snapshot {
            tags.push((9887, mode_9887.to_string()));
        }
    }
    tags
}

/// A subscription carrying extra series and nothing else.
///
/// The same rows the first subscription states for a series, under the same
/// action, with the count ahead of them: a caller joining a contract already
/// being watched needs the series it named asked for, and the prices it hears
/// are the ones already arriving.
fn build_series_subscribe_tags(
    con_id: i64,
    exchange: &str,
    sec_type: &str,
    mode_9887: i32,
    ts: &str,
    rows: &[(u32, u32)],
) -> Vec<(u32, String)> {
    let con_id_str = (con_id as u32).to_string();
    let (fix_exchange, fix_sec_type) = stated_venue_and_type(sec_type, exchange);
    let mut tags: Vec<(u32, String)> = vec![
        (fix::TAG_MSG_TYPE, fix::MSG_MARKET_DATA_REQ.to_string()),
        (fix::TAG_SENDING_TIME, ts.to_string()),
        (263, SUBSCRIBE_ACTION.to_string()),
        (146, rows.len().to_string()),
    ];
    for (req_id, tick) in rows {
        // The model at the close is asked for where the standing model is, not
        // where the contract trades. The venue treats the two as one kind:
        // same record, same flags, same reader, the closing one differing only
        // in which moment it was worked out for — and the standing one is
        // served only when it is asked for on that venue.
        //
        // This does not make the closing model arrive. Asked for either way it
        // is acknowledged and then never stated, on both a paper and a live
        // login for this account, so what withholds it is not the venue named
        // here.
        let venue = if *tick == CLOSING_GREEKS_REQUEST_TYPE { GREEKS_VENUE } else { fix_exchange };
        tags.push((262, req_id.to_string()));
        tags.push((6008, con_id_str.clone()));
        tags.push((207, venue.to_string()));
        tags.push((167, fix_sec_type.to_string()));
        tags.push((264, tick.to_string()));
        tags.push((6088, "Socket".to_string()));
        tags.push((9830, "1".to_string()));
        tags.push((9839, "1".to_string()));
        if mode_9887 != 0 {
            tags.push((9887, mode_9887.to_string()));
        }
    }
    tags
}

/// A quantity as the running-volume string states it.
///
/// The venue's own decimal, whose text is that of a fixed-point number held to
/// sixteen places and not stripped of its trailing zeros — so two shares are
/// `2.0000000000000000` and none at all is `0E-16`. A caller splitting the
/// string on its separators parses these as numbers, but they are the venue's
/// spelling and not this client's, so they are written as the venue writes
/// them.
fn rt_volume_quantity(shares: i64) -> String {
    if shares == 0 {
        return "0E-16".to_string();
    }
    format!("{shares}.0000000000000000")
}

/// A price as the running-volume string states it.
///
/// Up to eight places, which is where the venue's own formatter starts, with
/// no grouping. How many places it keeps at the low end is the instrument's
/// own market rule, which this client does not hold in full — a caller reading
/// the field as a number is unaffected, and one comparing its text against the
/// reference client's may see fewer trailing zeros.
fn rt_volume_price(value: f64) -> String {
    if value == 0.0 {
        return "0".to_string();
    }
    let mut text = format!("{value:.8}");
    if text.contains('.') {
        text = text.trim_end_matches('0').trim_end_matches('.').to_string();
    }
    text
}

/// Read a big-endian `f64` off the front of a payload.
fn series_f64(payload: &[u8], at: usize) -> Option<f64> {
    let bytes: [u8; 8] = payload.get(at..at + 8)?.try_into().ok()?;
    Some(f64::from_be_bytes(bytes))
}

/// Read a big-endian `i64` off a payload.
fn series_i64(payload: &[u8], at: usize) -> Option<i64> {
    let bytes: [u8; 8] = payload.get(at..at + 8)?.try_into().ok()?;
    Some(i64::from_be_bytes(bytes))
}

/// Read a big-endian `f32` off a payload.
///
/// The venue states some figures to single precision and others to double, in
/// the same message: the misc-stats series carries its whole numbers and its
/// fractional ones in two tables, and the fractional table is four bytes wide.
fn series_f32(payload: &[u8], at: usize) -> Option<f32> {
    let bytes: [u8; 4] = payload.get(at..at + 4)?.try_into().ok()?;
    Some(f32::from_be_bytes(bytes))
}

/// Read a big-endian `i32` off a payload.
fn series_i32(payload: &[u8], at: usize) -> Option<i32> {
    let bytes: [u8; 4] = payload.get(at..at + 4)?.try_into().ok()?;
    Some(i32::from_be_bytes(bytes))
}

/// Hand one reading of an extra series to whoever asked for it.
///
/// The venue states each series in its own shape and under its own number, and
/// the reference client republishes it to an API caller under a number of the
/// API's own. Both are the venue's to decide, so both are read from it rather
/// than chosen here: the payload layout is what the series' own reader reads,
/// and the number is the one the reference client publishes it under.
///
/// Returns whether the series was one this client reads. A series it does not
/// is stepped over rather than guessed at — the record's length is known
/// whatever the tick, so the ones beside it still arrive.
fn deliver_series(
    tick: u32,
    payload: &[u8],
    instrument: InstrumentId,
    shared: &SharedState,
) -> bool {
    use crate::types::{SeriesTick, SeriesValue};
    let say = |tick_type: i32, value: SeriesValue| {
        // The venue states a figure it does not hold as the largest its field
        // carries, and which field that is depends on the record: the largest
        // double where it holds a double, the largest single where it holds
        // one of those, the largest signed integer where it counts. The
        // reference client sends nothing at all rather than that number, so
        // neither does this — read as a figure, the largest double is two
        // hundred undecillion and the largest integer is two billion shares.
        //
        // Judged here rather than in each record's own arm. Every series this
        // client publishes passes through here, and the arms that remembered
        // to check were the minority: a price, a size and a rate all reached a
        // caller as the sentinel that says the venue had nothing to say.
        let nothing_was_said = match &value {
            SeriesValue::Generic(v) | SeriesValue::Price(v) => {
                !v.is_finite() || *v == f64::MAX || *v == f32::MAX as f64
            }
            SeriesValue::Size(v) => {
                !v.is_finite() || *v == f64::MAX || *v == i32::MAX as f64
            }
            // And on the callback that takes text, a series with nothing to
            // say says nothing: an empty string is not a reading, and the
            // reference client sends none.
            SeriesValue::Text(t) => t.is_empty(),
        };
        if nothing_was_said {
            return;
        }
        shared.market.push_series_tick(SeriesTick { instrument, tick_type, value });
    };
    match tick {
        // Shortability. The count of shares available to borrow follows the
        // flag where the venue states one, and is left unstated — rather than
        // stated as nothing — where it does not, so a caller is not told that
        // nothing can be borrowed when the venue simply did not say.
        236 => {
            let Some(shortable) = series_i32(payload, 0) else { return true };
            say(46, SeriesValue::Generic(shortable as f64));
            if let Some(shares) = series_i32(payload, 4)
                && shares != i32::MAX
            {
                say(89, SeriesValue::Size(shares as f64));
            }
        }
        // Option volume and open interest: the calls first, then the puts.
        100 => {
            let (Some(call), Some(put)) = (series_i32(payload, 0), series_i32(payload, 4))
            else {
                return true;
            };
            say(29, SeriesValue::Size(call as f64));
            say(30, SeriesValue::Size(put as f64));
        }
        101 => {
            let (Some(call), Some(put)) = (series_i32(payload, 0), series_i32(payload, 4))
            else {
                return true;
            };
            say(27, SeriesValue::Size(call as f64));
            say(28, SeriesValue::Size(put as f64));
        }
        // Volatility, historical and implied. The venue states a figure it
        // does not hold as the largest a double carries, which is not a
        // volatility of nearly two hundred undecillion — it is silence.
        //
        // The historical one answers to two numbers, and the one a caller is
        // likelier to have read somewhere is the second of them. The protocol
        // registers this series under both and resolves either to the same
        // subscription, which is why the reading is the same: one double, on
        // the number the series has always been delivered under. Read under
        // one number only, a caller who named the other was acknowledged and
        // then handed nothing for the life of the subscription.
        104 | 512 => {
            let Some(vol) = series_f64(payload, 0) else { return true };
            say(23, SeriesValue::Generic(vol));
        }
        106 => {
            let Some(vol) = series_f64(payload, 0) else { return true };
            if vol > 0.0 && vol.is_finite() && vol != f64::MAX {
                say(24, SeriesValue::Generic(vol));
            }
        }
        // How fast it is trading: the count of trades, the rate of them, and
        // the rate of volume.
        293 => {
            let Some(value) = series_f64(payload, 0) else { return true };
            say(54, SeriesValue::Generic(value));
        }
        294 => {
            let Some(value) = series_f64(payload, 0) else { return true };
            say(55, SeriesValue::Generic(value));
        }
        295 => {
            let Some(value) = series_f64(payload, 0) else { return true };
            say(56, SeriesValue::Generic(value));
        }
        // Volatility the venue recomputes through the session, and the bond
        // factor a redemption changes.
        411 => {
            let Some(value) = series_f64(payload, 0) else { return true };
            say(58, SeriesValue::Generic(value));
        }
        460 => {
            let Some(value) = series_f64(payload, 0) else { return true };
            say(60, SeriesValue::Generic(value));
        }
        // What the index costs above the future written on it. Stated for an
        // index and for nothing else.
        162 => {
            let Some(value) = series_f64(payload, 0) else { return true };
            say(31, SeriesValue::Generic(value));
        }
        // The extremes of the last quarter, half-year and year, and what
        // trades in an ordinary day. The venue states two tables — whole
        // numbers first, then fractional ones — each opening with how many
        // entries it carries, and each entry naming what it is before stating
        // it. The fractional table is four bytes to the figure, not eight.
        // The two series that state a run of paired figures: the volatility the
        // venue's model holds against each point of a curve, and the weight it
        // puts on each price a contract might reach. Both state the pairs the
        // same way; one states a version in front of the count.
        490 | 546 => {
            let pairs = read_paired_figures(payload, tick == 490);
            if !pairs.is_empty() {
                shared.market.note_paired_figures(instrument, tick, pairs);
            }
        }
        165 | 561 | 562 | 757 => {
            let (whole, fractional) = read_numbered_figures(payload);
            // Kept whole, both tables, under the venue's own numbering. Two of
            // these figures have a documented call and the rest have none, and
            // the ones that have none are the venue's to add to: read for the
            // two and dropped otherwise, every figure the venue started
            // stating went unseen until someone here wrote its number down.
            shared.market.note_numbered_figures(instrument, tick, &whole, &fractional);
            if tick != 165 {
                return true;
            }
            for (named, value) in whole {
                // What trades in an ordinary day. A figure the venue does not
                // hold is stated as the largest the type carries, which is not
                // two billion shares.
                if named == AN_ORDINARY_DAY && value != f64::from(i32::MAX) {
                    say(21, SeriesValue::Size(value));
                }
            }
            for (named, value) in fractional {
                // The high above the low in each pair, and the venue numbers
                // the high first.
                //
                // Two more the venue states here have no tick of their own in
                // the documented API — how many shares are on issue, which is
                // the multiplier that turns a price into a market
                // capitalisation, and what the contract opened at a year ago.
                // They are kept against the contract rather than sent as ticks,
                // because inventing a number for them would put a figure of
                // this client's own choosing where a caller reads the venue's.
                //
                // Measured on two live contracts three orders of magnitude
                // apart: the share count is stated in millions, and both
                // matched the company's own.
                if named == A_SHARE_COUNT || named == AN_OPEN_A_YEAR_AGO {
                    if value.is_finite() && value != f64::from(f32::MAX) {
                        shared.market.note_contract_figures(
                            instrument,
                            (named == A_SHARE_COUNT).then_some(value * 1e6),
                            (named == AN_OPEN_A_YEAR_AGO).then_some(value),
                        );
                    }
                    continue;
                }
                let tick = match named {
                    201 => 16,
                    202 => 15,
                    203 => 18,
                    204 => 17,
                    205 => 20,
                    206 => 19,
                    // The rest have no documented call. They are kept above
                    // under the venue's own number rather than sent as a tick
                    // number of this client's own choosing.
                    _ => continue,
                };
                // And a fractional figure it does not hold, the same way.
                if value.is_finite() && value != f64::from(f32::MAX) {
                    say(tick, SeriesValue::Price(value));
                }
            }
        }
        // The mark the venue keeps for a contract, which is not a trade. A
        // word of flags follows the price: the lowest bit says the price
        // stands, and one high bit says it does not whatever the lowest says.
        // The other number the mark is asked for under states it as a record
        // of the venue's own fields instead, and is read where those are.
        232 => {
            let (Some(price), Some(flags)) = (series_f64(payload, 0), series_i32(payload, 8))
            else {
                return true;
            };
            // And minus one is the venue holding none, not a price of minus
            // one: taken as a price it marks the position at a negative. Nor
            // is the largest a double carries, or a number that is no number.
            if flags & 1 == 1
                && flags & 0x0800_0000 == 0
                && price != -1.0
                && price.is_finite()
                && price != f64::MAX
            {
                say(37, SeriesValue::Price(price));
            }
        }
        // What the contract pays out, which the venue states as text: four
        // bytes of its own and then one line of figures with commas between.
        456 => {
            let Some(rest) = payload.get(4..) else { return true };
            let stated = String::from_utf8_lossy(rest);
            let line = stated.split(['\r', '\n']).next().unwrap_or_default();
            if !line.is_empty() {
                say(59, SeriesValue::Text(line.to_string()));
            }
        }
        // What a fund is worth per share: where it last stood, where it stood
        // when it was frozen, and the day's two extremes.
        577 => {
            let Some(value) = series_f64(payload, 0) else { return true };
            if value != f64::MAX {
                say(96, SeriesValue::Price(value));
            }
        }
        623 => {
            let Some(value) = series_f64(payload, 0) else { return true };
            if value != f64::MAX {
                say(97, SeriesValue::Price(value));
            }
        }
        614 => {
            let (Some(high), Some(low)) = (series_f64(payload, 0), series_f64(payload, 8))
            else {
                return true;
            };
            // A high under its own low is the venue stating neither, and
            // handing over both would state a day that ran backwards.
            if high >= low {
                if high != f64::MAX {
                    say(98, SeriesValue::Price(high));
                }
                if low != f64::MAX {
                    say(99, SeriesValue::Price(low));
                }
            }
        }
        // How much has traded over the last few minutes. The venue states how
        // many spans it carries and names each by its length, so the span is
        // read off the message rather than assumed from its place in it.
        595 => {
            let Some(spans) = series_i32(payload, 0) else { return true };
            let mut at = 4;
            for _ in 0..spans.clamp(0, (payload.len() / 8) as i32) {
                let (Some(minutes), Some(volume)) =
                    (series_i32(payload, at), series_i32(payload, at + 4))
                else {
                    return true;
                };
                at += 8;
                let tick = match minutes {
                    3 => 63,
                    5 => 64,
                    10 => 65,
                    _ => continue,
                };
                // A span the venue holds nothing for states the largest the
                // type carries rather than nought.
                if volume != i32::MAX {
                    say(tick, SeriesValue::Size(f64::from(volume)));
                }
            }
        }
        // What it costs to borrow, which the venue states as a price.
        499 => {
            let Some(value) = series_f64(payload, 0) else { return true };
            say(111, SeriesValue::Price(value));
        }
        // Open interest on a future, left unstated rather than stated as
        // nothing where the venue does not hold it.
        588 => {
            let Some(open) = series_i32(payload, 0) else { return true };
            if open != i32::MAX {
                say(86, SeriesValue::Size(open as f64));
            }
        }
        // What last traded in the regular session, which is not the last
        // trade when the session is shut.
        318 => {
            let Some(price) = series_f64(payload, 0) else { return true };
            say(57, SeriesValue::Price(price));
        }
        // The auction: how much is crossing, which way it is unbalanced, and
        // at what price. The regulatory imbalance follows where the venue
        // states one and is absent where it does not.
        225 => {
            let (Some(volume), Some(imbalance)) = (series_i32(payload, 0), series_i32(payload, 4))
            else {
                return true;
            };
            say(34, SeriesValue::Size(volume as f64));
            say(36, SeriesValue::Size(imbalance as f64));
            if let Some(bytes) = payload.get(8..12) {
                let price = f32::from_be_bytes(bytes.try_into().unwrap_or([0; 4]));
                say(35, SeriesValue::Price(price as f64));
            }
            // The imbalance the venue must publish sits further down the same
            // record, past the auction's own type and six figures the venue
            // keeps for itself, each four bytes wide. Nought there is the
            // venue holding none rather than an auction in balance.
            if let Some(regulatory) = series_i32(payload, 44)
                && regulatory != 0
                && regulatory != i32::MAX
            {
                say(61, SeriesValue::Size(f64::from(regulatory)));
            }
        }
        // The average option volume the venue keeps for each side, which it
        // publishes as one figure: the two added. Either side left unstated
        // leaves the whole figure unstated rather than counting it as none.
        105 => {
            let (Some(call), Some(put)) = (series_i32(payload, 0), series_i32(payload, 4))
            else {
                return true;
            };
            if call != i32::MAX && put != i32::MAX {
                say(87, SeriesValue::Size(call as f64 + put as f64));
            }
        }
        // What a share is expected to open at, and what it did open at. The
        // venue states up to three prices — the top, the middle and the bottom
        // of its estimate — and a flag word saying which of them stand.
        586 => {
            let Some(stated) = series_i32(payload, 4) else { return true };
            // Three points, and a record stating more of them is not this
            // record. Read as three of however many it stated, the word
            // saying which prices stand was read out of the middle of the
            // fourth price — and a fourth price whose low bits happen to say
            // so published an estimate the venue never marked as standing.
            if stated > 3 {
                return true;
            }
            // Where that word sits is where the record said it would, behind
            // as many prices as it stated rather than as many as happened to
            // read. A record stating fewer states the rest as the largest a
            // double carries, which is how the venue says it holds none.
            let stated = stated.max(0) as usize;
            let prices: Vec<f64> = (0..stated)
                .filter_map(|i| series_f64(payload, 8 + 8 * i))
                .collect();
            let Some(flags) = series_i32(payload, 8 + 8 * stated) else { return true };
            if flags == -1 {
                return true;
            }
            // The middle of the estimate stands while the estimate does.
            if flags & 0x03 == 0x03
                && let Some(&middle) = prices.get(1)
                && middle != f64::MAX
            {
                say(101, SeriesValue::Generic(middle));
            }
            // And the figure it finally opened at, once it is final.
            if flags & 0x07 == 0x05
                && let Some(&settled) = prices.first()
                && settled != f64::MAX
            {
                say(102, SeriesValue::Generic(settled));
            }
            // Where the estimate stands, the record states three prices and
            // one of them reaches a caller. The documented API has a single
            // number for this and this client will not pick a second, so the
            // two it cannot hand over are written down rather than passing
            // unseen.
            //
            // Only under the flags that make them prices at all. The venue's
            // own reader takes all three when the estimate stands and position
            // nought alone when the figure is final; under anything else it
            // takes none, and a position recorded there is not a price the
            // venue stated. Nor is one that is not a number: the publishing
            // path below refuses those and so does this.
            if flags & 0x03 == 0x03 {
                for at in [0usize, 2] {
                    let stated = prices.get(at).copied().filter(|p| p.is_finite() && *p != f64::MAX);
                    if stated.is_none() {
                        continue;
                    }
                    shared.market.note_unread_wire_under(
                        "farm",
                        format!("an estimated opening price at position {at}"),
                        format!(
                            "an estimated opening price at position {at} on tick 586, \
                             which no documented call names",
                        ),
                    );
                }
            }
        }
        // The company ratios, which the venue sends as compressed text behind
        // a header it does not describe. Held to what one inflated payload may
        // become, as every other inflate here is: what arrives is bounded on
        // the wire and what it becomes is not.
        258 => {
            let Some(compressed) = payload.get(8..) else { return true };
            use std::io::Read as _;
            let mut text = String::new();
            let read = flate2::read::ZlibDecoder::new(compressed)
                .take(crate::protocol::fixcomp::MAX_INFLATED + 1)
                .read_to_string(&mut text);
            match read {
                Ok(_) if text.len() as u64 <= crate::protocol::fixcomp::MAX_INFLATED => {
                    let text = text.trim();
                    if !text.is_empty() {
                        say(47, SeriesValue::Text(text.to_string()));
                    }
                }
                Ok(_) => log::warn!("the company ratios inflated past what one payload may be"),
                Err(why) => log::debug!("the company ratios did not inflate: {why}"),
            }
        }
        _ => return note_stated_figures(tick, payload, instrument, shared),
    }
    true
}

/// The series that state figures the documented API has no call for, and the
/// fields each one states, in the order it states them.
///
/// `d` is eight bytes of double, `f` four of single and `i` four of signed
/// integer — the venue's own widths in the venue's own order. Three of these
/// carry trailing fields only sometimes, and those are read where the bytes for
/// them arrived rather than guessed at.
///
/// | Series | What the venue states on it |
/// | --- | --- |
/// | 125 | Five analytic figures for a bond |
/// | 266 | The two coefficients that fit an index to its future |
/// | 317, 531 | An underlying's mark once dealing has closed, frozen and delayed-frozen |
/// | 388 | Four figures about the issuer behind a contract |
/// | 391 | What a warrant's implied volatility looks like against its competitors |
/// | 393 | The two figures a beta against the market is worked out from |
/// | 398 | Whether the venue is serving market data |
/// | 399 | Whether the contract is exempt from the short-sale rule, and since when |
/// | 402 | The schedule of average daily volume |
/// | 407 | What margin a future takes |
/// | 418 | The interest a bond has accrued |
/// | 459 | The close the venue keeps through the session |
/// | 493, 606 | What an option is worth in and out of the money, and what is time |
/// | 497, 597 | Two of the venue's own volatility figures for an option |
/// | 504 | What one contract delivers |
/// | 509 | The two halves of a sentiment reading |
/// | 527 | Volatility over twenty days |
/// | 540 | The yield the venue works out from the price |
/// | 545 | The volatility the venue's own model settles on |
/// | 584, 585 | The volume a contract usually opens and closes on |
/// | 613 | What the model makes the contract worth |
/// | 645 | The close stated in money |
/// | 647 | How much of the industry's borrowable stock is out on loan |
/// | 649 | One figure and the number the venue files it under |
/// | 657 | Two figures of four weeks' trading volume |
/// | 658 | The volume of an average minute |
/// | 680 | The venue's own weighted volume figure |
/// | 689, 736 | The volatility either side of the close, and the model behind it |
/// | 767 | What a perpetual contract is funding at |
const STATED_FIGURES: &[(u32, &str)] = &[
    (125, "fffff"), (266, "dd"), (317, "dii"), (388, "dddd"), (391, "ddiidd"),
    (393, "dd"), (398, "i"), (399, "iii"), (402, "dddiii"), (407, "dddd"),
    (418, "d"), (459, "fiii"), (493, "did"), (497, "d"), (504, "di"),
    (509, "ii"), (527, "d"), (531, "dii"), (540, "di"), (545, "d"),
    (584, "i"), (585, "i"), (597, "d"), (606, "did"), (613, "iff"),
    (645, "di"), (647, "d"), (649, "fi"), (657, "dif"), (658, "di"),
    (680, "d"), (689, "ddii"), (736, "ddi"), (767, "di"),
];

/// Keep the figures a series states, where the documented API has no call to
/// hand them over on.
///
/// The figures reach a caller as the venue stated them: in its order, at its
/// widths, and with the number it uses to say it holds nothing left standing.
/// None of them is renamed into a tick number of this client's own choosing —
/// a caller reading the venue's numbers would be reading an invention.
///
/// Returns whether the series was one this client reads.
fn note_stated_figures(
    tick: u32, payload: &[u8], instrument: InstrumentId, shared: &SharedState,
) -> bool {
    let Some((_, layout)) = STATED_FIGURES.iter().find(|(series, _)| *series == tick) else {
        return false;
    };
    let mut figures: Vec<f64> = Vec::with_capacity(layout.len());
    let mut at = 0usize;
    for field in layout.bytes() {
        let (width, value) = match field {
            b'd' => (8, series_f64(payload, at)),
            b'f' => (4, series_f32(payload, at).map(f64::from)),
            _ => (4, series_i32(payload, at).map(f64::from)),
        };
        // A record that stopped short stated the fields before it. Three of
        // these series end with fields the venue writes only sometimes, and
        // the rest simply arrive whole.
        let Some(value) = value else { break };
        figures.push(value);
        at += width;
    }
    // A record carrying no field at all is not a statement about the contract.
    if figures.is_empty() {
        return true;
    }
    shared.market.note_stated_figures(instrument, tick, figures);
    true
}

/// Option resub info: (instrument, symbol, exchange, sec_type, last_trade_date, strike,
/// right, multiplier, mode_9887).
type MdResubInfo = (InstrumentId, String, String, String, String, f64, String, String, i32);

/// `MdResubInfo` with the instrument's resolved con_id spliced in behind it.
type MdResubTarget = (InstrumentId, i64, String, String, String, String, f64, String, String, i32);

/// One entry of an L1 subscription as it went out: the number it was asked
/// under, the kind of market data that number carries, and the venue it was
/// asked on.
///
/// Kept so a withdrawal can state each entry the way the subscription stated
/// it. A withdrawal naming a number alone is one the venue leaves being
/// served, and the number then outlives the engine that asked under it: a
/// connection handed on with one still held is answered with silence when
/// the next engine asks under the same number.
pub(crate) struct MdReqEntry {
    pub(crate) req_id: u32,
    pub(crate) request_type: u32,
    pub(crate) venue: String,
}

/// An instrument's L1 subscriptions as they went out, so each can be
/// withdrawn as itself: the contract and venue and type each entry named.
pub(crate) struct MdReqRecord {
    pub(crate) con_id: i64,
    /// The security type in the wire's own spelling, the one the entries
    /// were asked for under.
    pub(crate) sec_type: String,
    /// The feed a delayed or frozen quote was asked for, stated beside the
    /// entry and so beside its withdrawal too. Zero where none was.
    pub(crate) mode_9887: i32,
    pub(crate) entries: Vec<MdReqEntry>,
}

/// A cursor over a book frame's bits, most significant bit first.
struct BookBits<'a> {
    bytes: &'a [u8],
    at: usize,
    end: usize,
}

impl BookBits<'_> {
    fn left(&self) -> usize {
        self.end - self.at
    }

    /// Step over `n` bits, or nothing where fewer remain.
    fn skip(&mut self, n: usize) -> Option<()> {
        if n > self.left() {
            return None;
        }
        self.at += n;
        Some(())
    }

    /// The next `n` bits as a number, or nothing where fewer remain.
    fn take(&mut self, n: usize) -> Option<u64> {
        if n > self.left() || n > 64 {
            return None;
        }
        let mut v = 0u64;
        for _ in 0..n {
            let bit = (self.bytes[self.at >> 3] >> (7 - (self.at & 7))) & 1;
            v = (v << 1) | u64::from(bit);
            self.at += 1;
        }
        Some(v)
    }
}

pub(crate) struct FarmState {
    /// Subscriptions a reconnect has still to put back, and the earliest the
    /// next burst may go out.
    ///
    /// A server that has just come back up is the least able to take a whole
    /// book at once, so the replay is paced. Sleeping for that pace would stop
    /// the one thread driving every transport, the heartbeats, the reconnects
    /// and shutdown, so the queue is drained a burst at a time on the passes
    /// the loop is already making.
    pub(crate) replay_queue: std::collections::VecDeque<MdResubTarget>,
    /// When the next burst may go out. `None` means there is nothing waiting.
    pub(crate) replay_not_before: Option<Instant>,
    /// The next id this client asks the venue under.
    ///
    /// Every subscription is asked for under one of these and mapped back to
    /// the caller who wanted it. A caller's own id is never sent: the venue
    /// echoes an id back, and one taken from the caller is indistinguishable
    /// from one of these — a caller's second book was answered under another
    /// subscription's venue because both were numbered 2.
    pub(crate) next_md_req_id: u32,
    pub(crate) md_req_to_instrument: Vec<(u32, InstrumentId)>,
    pub(crate) instrument_md_reqs: Vec<(InstrumentId, MdReqRecord)>,
    /// Venue numbers whose quotes this session has no contract for, each said
    /// once. A quote naming one is dropped, and dropped silently there is no
    /// way to tell a venue that stopped sending from one still sending under a
    /// number nobody here holds.
    quotes_for_no_one: std::collections::HashSet<u32>,
    /// Active depth subscriptions: (req_id, is_smart_depth).
    pub(crate) depth_subs: Vec<(u32, bool)>,
    /// How deep each caller asked its book to be, by the caller's own id.
    ///
    /// The venue sends the levels it has and this number is not on the wire —
    /// the reference client asks for a depth and then shows that many, so a
    /// caller that asked for five and was handed ten was handed a book it did
    /// not ask for.
    depth_rows: Vec<(u32, i32)>,
    /// The book each stream is holding, by the venue's number for it: the bid
    /// side and the ask side, deepest last, as maker, price and size.
    ///
    /// Kept because a caller asking for five levels is answered with five, and
    /// the venue answers with all of them: when a level inside those five goes
    /// away, the level that moves up into the fifth place arrives from the
    /// venue in sixth place and is dropped as too deep. Without a book of its
    /// own this client had nothing to put in the place that emptied, so the
    /// caller's book lost a row on every withdrawal near the top and never
    /// refilled — which is what the client this replaces keeps a book to
    /// prevent.
    depth_books: Vec<(u32, BookSides)>,
    /// Which exchange each fanned-out depth subscription is for, so a level
    /// says where it stands. Without it every level of a smart book arrived
    /// unattributed, and a caller was handed one book with no way to tell
    /// which venue any of it was on.
    pub(crate) depth_fanout_exchange: Vec<(u32, String)>,
    /// Maps server_tag → (depth_req_id, is_smart_depth, min_tick) for active depth
    /// subscriptions.
    pub(crate) depth_tag_to_req: Vec<(u32, u32, bool, f64, f64, String)>,
    /// Every book: the id this client asked the venue under, against the id
    /// the caller asked for it under.
    ///
    /// Not only the smart-routed fan-out, whatever it was once for — the
    /// subscribe path fills this for every book, and withdrawing them all at a
    /// stop reads the caller's side of it.
    pub(crate) depth_fanout_map: Vec<(u32, u32)>,
    /// Primary depth subscription params for reconnect: (req_id, con_id, exchange,
    /// sec_type, num_rows, is_smart_depth).
    depth_resub_info: Vec<(u32, i64, String, String, String, i32, bool)>,
    md_resub_info: Vec<MdResubInfo>,
    /// The option-model subscriptions and what they were taken out on, so one
    /// can be withdrawn the same way it was asked for.
    greeks_subs: Vec<(u32, i64, String)>,
    /// What generic tick each request asked for, as (req_id, request type).
    /// The venue numbers a generic tick separately from the prices and states
    /// nothing on the frames themselves about which tick they carry, so the
    /// only thing that says what a frame holds is what was asked for under
    /// that number.
    pub(crate) generic_tick_reqs: Vec<(u32, u32)>,
    /// The extra series a caller named for a contract, by the venue's own
    /// number for each. Held per instrument rather than passed along with the
    /// subscription, because the rebuild after a reconnect sends the
    /// subscription again from what is held here and has nowhere else to read
    /// them from.
    pub(crate) asked_generic_ticks: std::collections::HashMap<InstrumentId, Vec<u32>>,
    /// Where the request that opened each slot's subscription falls in the
    /// order of everything the client has asked for.
    ///
    /// Read on a withdrawal. A withdrawal decided before the subscription that
    /// is now live began is not about it: the caller that asked for the same
    /// contract in between had its own subscription taken down by somebody
    /// else's withdrawal, was published as watching the contract, and heard
    /// nothing for the rest of the session.
    subscription_asked_on: std::collections::HashMap<InstrumentId, u64>,
    /// And the number the request that took the slot asked under, which is the
    /// name of this occupancy of it.
    ///
    /// A slot is reusable and a contract is not always identified, so neither
    /// says which occupancy a withdrawal was about. The request that took the
    /// slot wrote this number down as it took it and states it again when it
    /// gives the slot up: the two match for that occupancy and no other.
    subscription_began_under: std::collections::HashMap<InstrumentId, u64>,
    /// And the same for each series on a slot.
    ///
    /// One number for the whole slot answers for the subscription and not for
    /// its series: a caller asking for one series raised it for all of them,
    /// and the series another caller had just given up was kept — served for
    /// the life of the subscription, reaching callers that never named it, and
    /// asked for again by every rebuild after a reconnect.
    series_asked_on: std::collections::HashMap<(InstrumentId, u32), u64>,
    /// Where a slot's callers were sent when the contract turned out to live in
    /// another slot.
    ///
    /// What those callers asked for beyond the quote went with them, so a
    /// withdrawal naming the slot they left has to be read against the slot
    /// they were sent to: applied to the slot they left, it found nothing there
    /// and the series it was giving up went on being served where they had been
    /// moved to.
    moved_to: std::collections::HashMap<InstrumentId, InstrumentId>,
    /// What the running-volume series last stated for a contract: the
    /// cumulative value, share count and trade count, in that order.
    ///
    /// The series states totals and the caller is owed the trade between two
    /// of them, so the previous totals are what make a reading mean anything.
    /// Until one has been seen there is no trade to state, only a baseline.
    ///
    /// Kept per series as well as per contract: the venue runs two of them
    /// side by side — everything that traded, and what traded on a trade
    /// report — and one baseline for both would state each series' trades
    /// against the other's totals.
    rt_volume_totals: std::collections::HashMap<(InstrumentId, i32), (f64, i64, i32)>,
    /// The venue's number for a generic-tick subscription, and what it
    /// carries: (server tag, request type, instrument).
    generic_tick_tags: Vec<(u32, u32, InstrumentId)>,
    /// The headline subscriptions this session holds: the contract, the
    /// request number they were asked under, the providers, the contract's id
    /// and its type. Kept so a rebuilt connection asks for each again, and a
    /// withdrawal states each the way the subscription did.
    pub(crate) news_subscriptions: Vec<(InstrumentId, u32, String, i64, String)>,
    /// Message types the venue has sent on this connection that nothing reads.
    unread_types: std::collections::HashSet<String>,
    pub(crate) disconnected: bool,
    pub(crate) tick_buf: Vec<tick_decoder::RawTick>,
    /// Which instruments a batch of ticks has already published, so one that
    /// several ticks moved is published once. Kept between batches and cleared
    /// bit by bit as each is published, so the cost is the batch rather than
    /// the size of the table.
    notified: Box<[u64]>,
    /// The instruments those bits stand for, in the order they were touched.
    notified_ids: Vec<crate::types::InstrumentId>,
    pub(crate) farm_msg_buf: Vec<Vec<u8>>,
}

/// The venue's option model, subscribed to by naming the model where a price
/// subscription names an exchange, and the model's own tick where a price
/// subscription names a request type. One subscription per option.
fn build_greeks_subscribe_tags(req_id: u32, con_id: i64, sec_type: &str, ts: &str) -> Vec<(u32, String)> {
    let fix_sec_type = crate::control::contracts::sec_type_to_fix(sec_type);
    vec![
        (fix::TAG_MSG_TYPE, fix::MSG_MARKET_DATA_REQ.to_string()),
        (fix::TAG_SENDING_TIME, ts.to_string()),
        (263, "1".to_string()),
        (146, "1".to_string()),
        (262, req_id.to_string()),
        (6008, (con_id as u32).to_string()),
        (207, GREEKS_VENUE.to_string()),
        (167, fix_sec_type.to_string()),
        (264, GREEKS_REQUEST_TYPE.to_string()),
        (6088, "Socket".to_string()),
        (9830, "1".to_string()),
        (9839, "1".to_string()),
    ]
}

/// Whether a venue is trading a contract, subscribed to the same way the option
/// model is — its own tick where a price subscription names a request type.
///
/// Unlike the model, this names the contract's **own** exchange rather than a
/// stand-in: the model and the news feed are the exceptions that go by a name of
/// their own, and everything else is asked for where it trades. It is stated the
/// way the prices beside it state it — the wire's own spelling of the venue, the
/// smart route where the caller named none — because the entry is written down
/// under that name and withdrawn under it, and a withdrawal naming a venue the
/// subscription never named is one the venue leaves being served.
fn build_trading_status_subscribe_tags(
    req_id: u32,
    con_id: i64,
    sec_type: &str,
    exchange: &str,
    ts: &str,
) -> Vec<(u32, String)> {
    let (fix_exchange, fix_sec_type) = stated_venue_and_type(sec_type, exchange);
    vec![
        (fix::TAG_MSG_TYPE, fix::MSG_MARKET_DATA_REQ.to_string()),
        (fix::TAG_SENDING_TIME, ts.to_string()),
        (263, "1".to_string()),
        (146, "1".to_string()),
        (262, req_id.to_string()),
        (6008, (con_id as u32).to_string()),
        (207, fix_exchange.to_string()),
        (167, fix_sec_type.to_string()),
        (264, TRADING_STATUS_REQUEST_TYPE.to_string()),
        (6088, "Socket".to_string()),
        (9830, "1".to_string()),
        (9839, "1".to_string()),
    ]
}

/// The days the venue states its option model's volatility and rate over.
///
/// It counts the life of a contract in days and states both figures over one
/// of them. The reference client states them over a year, which is what a
/// caller reads them as, so this is what carries them across.
const A_YEAR_OF_DAYS: f64 = 365.0;

/// The venue's chargeable one-shot snapshot, on tag 264.
///
/// A request type of its own rather than a mode on an ordinary quote: the
/// venue names it `regsshot` when it refuses one, and an account without the
/// entitlement is refused by name. It is asked for under the snapshot action
/// below and never with a feed named beside it.
const REGULATORY_SNAPSHOT_REQUEST_TYPE: u32 = 624;

/// The entries that are the quote itself.
///
/// Every stream asks for the pair, whichever feed serves it; the chargeable
/// snapshot is its own. Everything else a subscription registers — the
/// trading status, the exchange map, the model — rides beside whichever of
/// these was asked for and says nothing about which it was. Read as though it
/// did, a contract carrying nothing but a snapshot answered to "is there a
/// stream here" because the companions beside the snapshot are not the
/// snapshot's number.
const REALTIME_BID_ASK_REQUEST_TYPE: u32 = 442;
/// The other half of the pair: what last traded.
const REALTIME_LAST_REQUEST_TYPE: u32 = 443;

/// Deliver the request once, on tag 263, in place of subscribing to it.
const SNAPSHOT_ACTION: &str = "3";

/// Open a subscription, on tag 263.
const SUBSCRIBE_ACTION: &str = "1";

/// The trading-status tick's own number, in place of a request type.
const TRADING_STATUS_REQUEST_TYPE: u32 = 437;

/// The tick that states which venue each bit of a quote's exchange mask means.
///
/// The server sends the map itself — every venue's name and the character it
/// is reported under, in the order the bits refer to. A table written into
/// this client's own source would name nothing for any venue absent from it,
/// and could not be checked against what the server assigns.
const BBO_EXCHANGE_MAP_REQUEST_TYPE: u32 = 626;

/// The kind of market data a subscription asks for, on tag 264.
///
/// A book is `Deep`. A quote is asked for under other numbers, and the two are
/// not interchangeable in either direction: a book asked for as a quote is
/// acknowledged and never sent, and the quote frames a venue does answer with
/// are not a book — read as one they become a bid of 143.87 and an ask of a
/// penny on a share trading at 772.
const DEEP_REQUEST: &str = "0";

/// An increment an acknowledgement states, where it states one that can be
/// counted in.
///
/// Every price and every size on the instrument is a count of these. Zero
/// counts nothing, a negative one turns every figure on the contract upside
/// down, and an infinity — which this parser reads from the word — saturates
/// them all. None of the three is a scale, so an acknowledgement stating one
/// states no increment at all.
fn stated_increment(field: &str) -> Option<f64> {
    let stated: f64 = field.trim().parse().ok()?;
    (stated.is_finite() && stated > 0.0).then_some(stated)
}

/// What the venue counts an instrument's sizes in, as its acknowledgement
/// states it: the last field, after the increment prices move in.
///
/// A size on the wire is a count of these. Whole ones for a share, and
/// hundred-millionths for a crypto — so a count taken as whole ones reports
/// one of the two a hundred million times over. An acknowledgement that
/// states none is counted in whole ones, which is what stating none means.
fn trailing_size_increment(parts: &[&str]) -> Option<f64> {
    // Only on an acknowledgement long enough to be one that carries it.
    // Captured off the wire, the two shapes are five fields for a ticker
    // setup — contract, price increment, server tag, an empty field, the size
    // increment — and nine for a subscription, ending the same way. Every
    // neighbouring field parses as a positive number, so a shorter
    // acknowledgement than either would not fail this: it would quietly hand
    // back the field before it, and a server tag read as a size increment
    // multiplies every size on the contract by five figures.
    const SHORTEST_THAT_CARRIES_ONE: usize = 5;
    if parts.len() < SHORTEST_THAT_CARRIES_ONE {
        return None;
    }
    stated_increment(parts.last()?)
}


/// What the option model goes by where an exchange would be named.
const GREEKS_VENUE: &str = "IBVOL";

/// The venue refusing to serve data this account is not subscribed to.
///
/// The number the venue reports that under. It was this client's own
/// number for a malformed request, which said the caller had asked wrongly
/// when the venue had simply declined to serve it — a caller branching on it
/// the way it would against the reference client read a refusal it could fix
/// as one it could not.
///
/// The refusal itself carries no number: the venue names the request and says
/// why in words, and the number is the one a caller is owed for those words.
pub(super) const DEPTH_VENUE_REFUSED: i32 = 354;

/// The option model's own tick, in place of a request type.
const GREEKS_REQUEST_TYPE: u32 = 732;

/// The same model as the contract closed, under its own number.
const CLOSING_GREEKS_REQUEST_TYPE: u32 = 733;


/// The news tick's own number, in place of a request type.
const NEWS_REQUEST_TYPE: u32 = 292;

/// The venue's number for what a contract trades in an ordinary day.
const AN_ORDINARY_DAY: i32 = 768;

/// How many shares a company has on issue, as the venue numbers the figure on
/// the tick that carries a contract's price extremes. Stated in millions.
const A_SHARE_COUNT: i32 = 408;

/// What the contract opened at a year ago, numbered on the same tick.
const AN_OPEN_A_YEAR_AGO: i32 = 210;

/// What a companion request rides beside the quote for, in words a caller can
/// act on.
///
/// "A request beside the quote" names nothing anyone can do anything about;
/// "the venue's option model" says to stop waiting for a computation. The
/// venue states which request it refused, so the answer says which.
fn companion_named(kind: u32) -> &'static str {
    match kind {
        TRADING_STATUS_REQUEST_TYPE => "whether the contract is halted",
        BBO_EXCHANGE_MAP_REQUEST_TYPE => "which venues its best bid and offer are on",
        GREEKS_REQUEST_TYPE => "the venue's option model",
        NEWS_REQUEST_TYPE => "the news on the contract",
        REALTIME_BID_ASK_REQUEST_TYPE => "every quote change",
        REALTIME_LAST_REQUEST_TYPE => "every trade",
        REGULATORY_SNAPSHOT_REQUEST_TYPE => "the chargeable snapshot",
        // The venue refused something this does not name. Its own reason
        // still carries, which is the part a caller reads.
        _ => "a series asked for beside the quote",
    }
}

/// The venue a headline subscription names.
const NEWS_VENUE: &str = "NEWS";

/// How a generic tick states the length of its payload.
///
/// Which of the three a tick uses is not on the wire: it is a property of the
/// tick, held as the protocol states it. Guessing from the frame
/// works until two readings both square with it, and then it misframes every
/// record after the one it got wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PayloadLength {
    /// One byte. What most ticks use, the option model and the trading status
    /// among them.
    OneByte,
    /// Two bytes.
    TwoBytes,
    /// None at all. The venue states no length for these and its own reader
    /// takes what the tick's shape needs, so where the record ends is the
    /// reader's to know and nothing here can work it out — a message may well
    /// carry more after one.
    ///
    /// Where nothing here reads one, the rest of the message goes unread
    /// rather than being handed on as this record's payload. Reading one means
    /// knowing its shape, which for every one of these is the venue's own
    /// field grammar: see `SELF_DESCRIBING_TICKS`.
    ToTheEnd,
}

/// The ticks that state their payload's length in two bytes.
const TWO_BYTE_LENGTH_TICKS: [u32; 28] = [
    247, 256, 257, 258, 292, 385, 386, 434, 454, 481, 490, 491, 496, 546, 593, 594, 628, 631,
    633, 669, 678, 687, 691, 699, 700, 703, 705, 726,
];

/// The ticks that state no length of their own.
const NO_LENGTH_TICKS: [u32; 7] = [221, 320, 376, 530, 532, 619, 787];

/// The ticks that state no length and whose record says where it ends.
///
/// Their payload is the venue's own field grammar — a run of fields, the last
/// of which says no more follows — so the record ends itself and the length
/// the framing does not state is not needed to read one.
///
/// Nor is a record like this the last thing in its message. Measured on a
/// front-month future: the mark arrived as four bytes at one moment and twelve
/// at the next, and behind it in the same message came the venue list and the
/// option parameters. Every field is a whole number of bytes wide, so where
/// the record ends is where the next one starts.
///
/// Every tick that states no length is one of these. Four of them were left
/// out, and a message carrying one of those was abandoned at it — so the
/// quote, the trading status, the venue list and every other series answering
/// into the same message were dropped with it, for as long as the caller kept
/// asking for that one.
const SELF_DESCRIBING_TICKS: [u32; 8] = [220, 221, 320, 376, 530, 532, 619, 787];

impl PayloadLength {
    fn of(tick: u32) -> Self {
        if TWO_BYTE_LENGTH_TICKS.contains(&tick) {
            Self::TwoBytes
        } else if NO_LENGTH_TICKS.contains(&tick) {
            Self::ToTheEnd
        } else {
            Self::OneByte
        }
    }

    /// How many bytes stand between the start of a record and its payload.
    fn header(self) -> usize {
        match self {
            Self::ToTheEnd => 4,
            Self::OneByte => 5,
            Self::TwoBytes => 6,
        }
    }
}

/// The length of a generic tick message, in bytes, from the length it states
/// and how much arrived with it.
///
/// These records stand on byte boundaries, so a count of bits that is not a
/// whole number of bytes describes no message this reads.
fn generic_tick_length(stated_bits: u16, arrived: usize) -> Option<usize> {
    let bits = crate::protocol::tick_decoder::bits_carried(stated_bits, arrived);
    if !bits.is_multiple_of(8) {
        return None;
    }
    Some(bits / 8)
}

/// A stream's two sides, deepest last, as maker, price and size.
type BookSides = [Vec<(String, f64, f64)>; 2];

/// One generic tick record: the venue's number for the subscription, and the
/// payload under it.
///
/// A message carries one or more of these, one after another. Nothing on a
/// record says which tick it carries — that is what the subscription said —
/// so reading one takes knowing, from its number, how the tick that was asked
/// for states its length.
struct GenericTickRecord<'a> {
    server_tag: u32,
    payload: &'a [u8],
}

/// Read the records in a generic tick message, asking `tick_of` what tick each
/// number was asked for under.
///
/// A number nothing asked for stops the reading rather than skipping the
/// record: without knowing the tick, there is no knowing where the record
/// ends, and carrying on would read the next one from the middle of this one.
fn read_generic_ticks<'a>(
    body: &'a [u8],
    mut tick_of: impl FnMut(u32) -> Option<u32>,
    mut each: impl FnMut(u32, GenericTickRecord<'a>),
) {
    let Some(stated_bits) = body.get(..2) else { return };
    let stated_bits = u16::from_be_bytes([stated_bits[0], stated_bits[1]]);
    let Some(length) = generic_tick_length(stated_bits, body.len()) else {
        return;
    };
    let Some(frame) = body.get(2..2 + length) else { return };

    let mut at = 0usize;
    let mut fields = Vec::new();
    while at + 4 <= frame.len() {
        let server_tag =
            u32::from_be_bytes([frame[at], frame[at + 1], frame[at + 2], frame[at + 3]]);
        let Some(tick) = tick_of(server_tag) else {
            log::debug!(
                "A generic tick arrived under server tag {server_tag}, which nothing here asked \
                 for. Without the tick there is no knowing where its record ends, so the rest of \
                 the message goes unread",
            );
            return;
        };
        let form = PayloadLength::of(tick);
        let stated = match form {
            // Nothing here reads one of these, and where it ends is its own
            // reader's to know — so the rest of the message goes unread rather
            // than being handed on as this record's payload and the ones
            // behind it mistaken for part of it.
            PayloadLength::ToTheEnd => {
                if !SELF_DESCRIBING_TICKS.contains(&tick) {
                    log::debug!(
                        "A generic tick {tick} states no length of its own and nothing here \
                         reads it, so where its record ends is not known and the rest of the \
                         message goes unread",
                    );
                    return;
                }
                // The record says where it ends, so reading it is what states
                // its length — and the next record starts where it stops.
                // Handing over the rest of the message instead read the record
                // behind it as part of this one, and stopping at this one threw
                // every record behind it away.
                let from = at + form.header();
                let Some(rest) = frame.get(from..) else { return };
                let mut reader = crate::protocol::tick_decoder::BitReader::new(rest, 0);
                fields.clear();
                let ended =
                    crate::protocol::tick_decoder::decode_record_into(&mut reader, &mut fields);
                // Every field is a whole number of bytes, so a record that
                // does not end on one was not read to its end and where the
                // next one starts is not known. Nor is a record that ran out
                // of bits before a field said it was the last: landing on a
                // byte boundary is not the same as having ended, and the
                // length taken from one that had not is short by whatever the
                // venue did not send.
                let read = reader.bits_read();
                if !ended || read == 0 || !read.is_multiple_of(8) {
                    log::debug!(
                        "A generic tick {tick} under server tag {server_tag} does not say where \
                         its record ends; the rest of the message goes unread",
                    );
                    return;
                }
                let width = read / 8;
                let Some(payload) = rest.get(..width) else { return };
                each(tick, GenericTickRecord { server_tag, payload });
                at = from + width;
                continue;
            }
            PayloadLength::OneByte => match frame.get(at + 4) {
                Some(&n) => n as usize,
                None => return,
            },
            PayloadLength::TwoBytes => match frame.get(at + 4..at + 6) {
                Some(n) => u16::from_be_bytes([n[0], n[1]]) as usize,
                None => return,
            },
        };
        let from = at + form.header();
        let Some(payload) = frame.get(from..from + stated) else {
            log::debug!(
                "A generic tick under server tag {server_tag} states {stated} bytes of payload \
                 and the message does not carry them; the rest goes unread",
            );
            return;
        };
        each(tick, GenericTickRecord { server_tag, payload });
        at = from + stated;
    }
}

/// The text inside a tick payload that states its own length.
///
/// Four bytes of big-endian length, then that many bytes of text, then padding
/// to a four-byte boundary. Read end to end as text, the length bytes join the
/// first name and the padding joins the last.
fn length_prefixed_text(payload: &[u8]) -> Option<std::borrow::Cow<'_, str>> {
    Some(String::from_utf8_lossy(length_prefixed_bytes(payload)?))
}

/// The pairs of figures a series states, and how many of them it carries.
///
/// A count, then that many pairs of two eight-byte figures. One of the two
/// series states a version in front of the count and a figure of its own
/// behind the pairs; the other begins at the count. A count reaching past what
/// arrived is held to what the record could carry, and a record that stops
/// short states the pairs before it.
fn read_paired_figures(payload: &[u8], versioned: bool) -> Vec<(f64, f64)> {
    let mut at = if versioned { 4 } else { 0 };
    let mut out = Vec::new();
    let Some(count) = series_i32(payload, at) else { return out };
    at += 4;
    for _ in 0..count.clamp(0, (payload.len() / 16) as i32) {
        let (Some(first), Some(second)) = (series_f64(payload, at), series_f64(payload, at + 8))
        else {
            return out;
        };
        out.push((first, second));
        at += 16;
    }
    out
}

/// Figures the venue numbers itself: its number for each, and the figure.
pub(crate) type NumberedFigures = Vec<(i32, f64)>;

/// The two tables of numbered figures a series states.
///
/// A count of whole-number figures, that many pairs of a number and its
/// figure, then a count of fractional ones and that many pairs again. The
/// fractional table is four bytes to the figure where the whole one is four to
/// an integer, and the venue numbers the two tables separately — a figure is
/// its number and the table it stood in, not its number alone.
///
/// Four series state their figures this way and this reads all four. A count
/// reaching past what arrived is held to what the record could carry, and a
/// table that stops short states the pairs before it.
fn read_numbered_figures(payload: &[u8]) -> (NumberedFigures, NumberedFigures) {
    let mut at = 0usize;
    let whole = read_numbered_table(payload, &mut at, false);
    let fractional = read_numbered_table(payload, &mut at, true);
    (whole, fractional)
}

/// One of those tables, leaving `at` on the byte after it.
fn read_numbered_table(payload: &[u8], at: &mut usize, fractional: bool) -> NumberedFigures {
    let mut out = Vec::new();
    let Some(count) = series_i32(payload, *at) else { return out };
    *at += 4;
    for _ in 0..count.clamp(0, (payload.len() / 8) as i32) {
        let Some(named) = series_i32(payload, *at) else { return out };
        let stated = if fractional {
            series_f32(payload, *at + 4).map(f64::from)
        } else {
            series_i32(payload, *at + 4).map(f64::from)
        };
        let Some(stated) = stated else { return out };
        out.push((named, stated));
        *at += 8;
    }
    out
}

/// The bytes inside a tick payload that states its own length, as they came.
///
/// A length that is nothing or less states no text: the venue's own reader
/// leaves such a record alone rather than reading what follows it, and a
/// length reaching past what arrived states text this record does not carry.
fn length_prefixed_bytes(payload: &[u8]) -> Option<&[u8]> {
    let stated = i32::from_be_bytes(payload.get(..4)?.try_into().ok()?);
    let stated = usize::try_from(stated).ok().filter(|stated| *stated > 0)?;
    payload.get(4..4 + stated)
}

/// The text a company-data series carries, out from behind what it states in
/// front of it.
///
/// Three shapes, and which one a series uses is the series' own. Four series
/// state four bytes of their own in front of text that runs to the end of the
/// record, and those bytes say nothing this client reads. One states eight of
/// its own and compresses what follows. The rest state the text's length as a
/// count before it, and only that much of what follows is the text: what comes
/// after is padding to a four-byte boundary and is not part of what the venue
/// stated.
///
/// The second analyst rating is one of the counted ones, and was read as
/// though the text began at the first byte. The count's low byte is a letter
/// for any text between sixty-five and ninety characters or between
/// ninety-seven and a hundred and twenty-two, so that byte joined the record's
/// first name: three contracts stated the same field under three different
/// names, each one the length of its own record.
fn company_text(series: u32, payload: &[u8]) -> Option<std::borrow::Cow<'_, [u8]>> {
    match series {
        454 | 505 | 669 | 705 => payload.get(4..).map(std::borrow::Cow::Borrowed),
        // The company's calendar is the one of these the venue compresses. It
        // states eight bytes of its own in front, and what follows becomes the
        // same runs of `KEY=VALUE` the rest state outright. Held to what one
        // payload may become, as every other inflate here is: what arrives is
        // bounded on the wire and what it becomes is not.
        386 => {
            use std::io::Read as _;
            let compressed = payload.get(8..)?;
            let mut text = Vec::new();
            flate2::read::ZlibDecoder::new(compressed)
                .take(crate::protocol::fixcomp::MAX_INFLATED + 1)
                .read_to_end(&mut text)
                .ok()?;
            if text.len() as u64 > crate::protocol::fixcomp::MAX_INFLATED {
                log::warn!("the company calendar inflated past what one payload may be");
                return None;
            }
            Some(std::borrow::Cow::Owned(text))
        }
        _ => length_prefixed_bytes(payload).map(std::borrow::Cow::Borrowed),
    }
}

/// Whether a series states its text in an alphabet of one byte to the
/// character.
///
/// The venue names the alphabet for some of these series and not for others.
/// Where it names one it is that alphabet, or the plain seven-bit one in which
/// the same bytes come out the same way; where it names none, the text is read
/// the way every other text this client is sent is read.
fn company_text_is_one_byte(series: u32) -> bool {
    matches!(series, 669 | 700 | 703 | 705 | 726)
}

/// What the venue sent of its option model.
///
/// A set of flags says which fields the payload carries, then one price that
/// is always there, then the stated fields in a fixed order that is not the
/// order of the flags: the time value is flagged first and read last. A field
/// whose flag is clear occupies no bytes at all, so the payload's length
/// varies with what was stated.
fn decode_greeks(payload: &[u8]) -> Option<crate::types::OptionComputation> {
    const VALID: u32 = 1 << 0;
    const TIME_VALUE: u32 = 1 << 13;
    const DELTA: u32 = 1 << 16;
    const GAMMA: u32 = 1 << 17;
    const VEGA: u32 = 1 << 18;
    const RHO: u32 = 1 << 19;
    const THETA: u32 = 1 << 20;
    const FUGIT: u32 = 1 << 21;
    const BOUNDARY: u32 = 1 << 22;
    const FORWARD_COEFF: u32 = 1 << 23;
    // Which volatility the model was given, not a figure: it states no bytes
    // and the walk below steps over nothing for it.
    const PRICE_BASED_VOL: u32 = 1 << 24;
    const UNDERLYING_PRICE: u32 = 1 << 25;
    const IMPLIED_VOL: u32 = 1 << 26;
    const CALENDAR_DAYS: u32 = 1 << 27;
    const DAILY_RATE: u32 = 1 << 28;
    const MODEL_YIELD: u32 = 1 << 29;
    const BRIDGE_YIELD: u32 = 1 << 30;
    // A value the venue did not state. The reference client's own mark, so a
    // caller can tell an unstated field from a real zero — and zero is a real
    // greek.
    const UNSTATED: f64 = f64::MAX;

    if payload.len() < 12 {
        return None;
    }
    let flags = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
    // The first flag gates the whole payload: without it nothing was computed
    // and nothing after it is a number.
    if flags & VALID == 0 {
        return None;
    }
    let mut pos = 4;
    let mut next = |stated: bool| -> f64 {
        if !stated {
            return UNSTATED;
        }
        let Some(bytes) = payload.get(pos..pos + 8) else { return UNSTATED };
        pos += 8;
        f64::from_be_bytes(bytes.try_into().unwrap_or_default())
    };

    // The model's own price for the option, stated whatever else is.
    let opt_price = next(true);
    let delta = next(flags & DELTA != 0);
    let gamma = next(flags & GAMMA != 0);
    let vega = next(flags & VEGA != 0);
    let rho = next(flags & RHO != 0);
    let theta = next(flags & THETA != 0);
    let fugit = next(flags & FUGIT != 0);
    let exercise_boundary = next(flags & BOUNDARY != 0);
    let forward_coeff = next(flags & FORWARD_COEFF != 0);
    let und_price = next(flags & UNDERLYING_PRICE != 0);
    let implied_vol = next(flags & IMPLIED_VOL != 0);
    let cal_days = next(flags & CALENDAR_DAYS != 0);
    let daily_rate = next(flags & DAILY_RATE != 0);
    let model_yield = next(flags & MODEL_YIELD != 0);
    let bridge_yield = next(flags & BRIDGE_YIELD != 0);
    let time_value = next(flags & TIME_VALUE != 0);

    // Below the walk, so nothing here can be mistaken for a field and step the
    // read position. Both figures are stated over one of the days counted
    // beside them, and both are handed on over a year: that is the scale the
    // reference client reports them on, and the scale a caller reads every
    // other volatility against. A volatility grows with the root of time, so
    // it carries over by the root of the days in a year; a rate accrues with
    // time itself.
    //
    // The sentinel is left alone. It is not a figure, and scaling it would
    // turn the mark for "not stated" into an infinity.
    let over_a_year = |v: f64, by: f64| if v == UNSTATED { UNSTATED } else { v * by };
    let implied_vol = over_a_year(implied_vol, A_YEAR_OF_DAYS.sqrt());
    let rate = over_a_year(daily_rate, A_YEAR_OF_DAYS);

    Some(crate::types::OptionComputation {
        // The venue's, reported under whichever request subscribed it.
        answers: None,
        instrument: 0,
        implied_vol,
        delta,
        opt_price,
        // The venue states no present value of dividends on this tick.
        pv_dividend: UNSTATED,
        gamma,
        vega,
        theta,
        und_price,
        cal_days,
        rate,
        // Stated on the same tick as the greeks beside them, read to step the
        // cursor past and then dropped on the floor. None of them has a field
        // anywhere in the documented API, and the venue sends every one of
        // them to anybody who asks for a model.
        rho,
        fugit,
        exercise_boundary,
        forward_coeff,
        model_yield,
        bridge_yield,
        time_value,
        price_based_vol: flags & PRICE_BASED_VOL != 0,
    })
}

impl FarmState {
    /// Whether this instrument still has market-data state that would be
    /// repointed by a slot reuse: a live subscription, or a record kept for the
    /// next reconnect.
    ///
    /// The resubscribe record is the reason this matters. Before it existed a
    /// reclaimed slot replayed nothing; now it replays the old contract's
    /// descriptor against whatever contract holds the id at reconnect.
    ///
    /// `md_req_to_instrument` is deliberately not consulted. A subscribe fills
    /// it and `instrument_md_reqs` together, so it says nothing the first check
    /// does not already cover — and an entry left there after an unsubscribe is
    /// a defect in its own right, not a reason to pin the slot. Held
    /// on that basis, a subscribe/unsubscribe cycle would consume a slot
    /// permanently and the instrument cap would become cumulative per session,
    /// which is the failure exists to prevent.
    pub(crate) fn holds_market_data(&self, instrument: InstrumentId) -> bool {
        self.instrument_md_reqs.iter().any(|(id, _)| *id == instrument)
            || self.md_resub_info.iter().any(|r| r.0 == instrument)
            // And the queue the reconnect replays from. Between the reconnect
            // and the last burst, `md_resub_info` has been taken and
            // `instrument_md_reqs` is not refilled until each subscription is
            // sent, so an instrument waiting its turn is held here and nowhere
            // else. Missing it, the slot reads as free while a subscription for
            // it is still to go out, and the replay binds this contract's
            // server tag and minimum tick onto whatever contract took the slot.
            || self.replay_queue.iter().any(|r| r.0 == instrument)
    }

    /// Whether this instrument has a subscription a second caller can be given
    /// instead of one of its own.
    ///
    /// The question above is a different one. It asks whether anything would
    /// be repointed by a slot reuse, and the chargeable snapshot answers yes:
    /// the slot must not go back to the table while one is out on it. It
    /// cannot answer yes here. A snapshot is a request of its own, withdrawn
    /// the moment it completes, and it is never recorded for replay — so a
    /// subscribe handed it instead of the wire was never sent, and the
    /// snapshot's own withdrawal then took the record it was pointed at. The
    /// caller held a number that reads as subscribed with nothing arriving on
    /// it for the rest of the session.
    pub(crate) fn holds_a_stream(&self, instrument: InstrumentId) -> bool {
        self.instrument_md_reqs.iter().any(|(id, record)| {
            *id == instrument
                && record.entries.iter().any(|e| {
                    matches!(
                        e.request_type,
                        REALTIME_BID_ASK_REQUEST_TYPE | REALTIME_LAST_REQUEST_TYPE,
                    )
                })
        }) || self.md_resub_info.iter().any(|r| r.0 == instrument)
            || self.replay_queue.iter().any(|r| r.0 == instrument)
    }

    pub(crate) fn new() -> Self {
        Self {
            replay_queue: Default::default(),
            replay_not_before: None,
            next_md_req_id: 1,
            md_req_to_instrument: Vec::new(),
            instrument_md_reqs: Vec::new(),
            quotes_for_no_one: std::collections::HashSet::new(),
            depth_subs: Vec::new(),
            depth_rows: Vec::new(),
            depth_books: Vec::new(),
            depth_tag_to_req: Vec::new(),
            depth_fanout_map: Vec::new(),
            depth_fanout_exchange: Vec::new(),
            depth_resub_info: Vec::new(),
            md_resub_info: Vec::new(),
            greeks_subs: Vec::new(),
            generic_tick_reqs: Vec::new(),
            asked_generic_ticks: std::collections::HashMap::new(),
            subscription_asked_on: std::collections::HashMap::new(),
            subscription_began_under: std::collections::HashMap::new(),
            series_asked_on: std::collections::HashMap::new(),
            moved_to: std::collections::HashMap::new(),
            rt_volume_totals: std::collections::HashMap::new(),
            news_subscriptions: Vec::new(),
            generic_tick_tags: Vec::new(),
            unread_types: std::collections::HashSet::new(),
            disconnected: false,
            tick_buf: Vec::with_capacity(16),
            notified: vec![0u64; crate::types::MAX_INSTRUMENTS / 64].into(),
            notified_ids: Vec::with_capacity(16),
            farm_msg_buf: Vec::with_capacity(32),
        }
    }

    pub(crate) fn poll_market_data(
        &mut self,
        farm_conn: &mut Option<Connection>,
        context: &mut Context,
        shared: &SharedState,
        event_tx: &Option<EventSink>,
        hb: &mut HeartbeatState,
    ) {
        if self.disconnected {
            return;
        }
        self.farm_msg_buf.clear();
        {
            let conn = match farm_conn.as_mut() {
                None => return,
                Some(c) => c,
            };
            match conn.try_recv() {
                // Nothing new on the socket, but a frame may already be whole
                // in the buffer — the other three pollers make that
                // distinction and this one did not.
                Ok(0) if !conn.has_buffered_data() => return,
                Ok(0) => {}
                Err(e) => {
                    log::error!("Farm connection lost: {e}");
                    self.handle_disconnect(farm_conn, context, event_tx, shared);
                    return;
                }
                Ok(n) => {
                    log::trace!("Farm recv: {} bytes, buffered: {}", n, conn.buffered());
                    let now = Instant::now();
                    hb.last_farm_recv = now;
                    context.recv_at = now;
                    hb.pending_farm_test = None;
                }
            }
            let frames = conn.extract_frames();
            log::trace!("Farm frames: {}", frames.len());
            for frame in &frames {
                match frame {
                    Frame::FixComp(raw) => {
                        let Some(unsigned) = conn.unsign(raw) else { continue };
                        match fixcomp::fixcomp_decompress(&unsigned) {
                            Ok(inner) => {
                                if log::log_enabled!(log::Level::Trace) {
                                    for m in &inner {
                                        log::trace!("WIRE< farm/comp {}", fix::fmt_pipe(m));
                                    }
                                }
                                self.farm_msg_buf.extend(inner);
                            }
                            Err(e) => {
                                log::warn!(
                                    "Farm: dropping malformed FIXCOMP frame ({} bytes): {}",
                                    unsigned.len(), e,
                                );
                            }
                        }
                    }
                    Frame::Binary(raw) => {
                        let Some(unsigned) = conn.unsign(raw) else { continue };
                        if log::log_enabled!(log::Level::Trace) {
                            log::trace!("WIRE< farm/bin {}", fix::fmt_pipe(&unsigned));
                        }
                        self.farm_msg_buf.push(unsigned);
                    }
                    Frame::Fix(raw) => {
                        let Some(unsigned) = conn.unsign(raw) else { continue };
                        if log::log_enabled!(log::Level::Trace) {
                            log::trace!("WIRE< farm/fix {}", fix::fmt_pipe(&unsigned));
                        }
                        self.farm_msg_buf.push(unsigned);
                    }
                    Frame::Control(_) => {
                    // 8=1 / 8=X control state — not consumed on the farm path.
                    }
                }
            }
        }

        let mut msgs = std::mem::take(&mut self.farm_msg_buf);
        for msg in &msgs {
            self.process_farm_message(msg, farm_conn, context, shared, event_tx, hb);
        }
        msgs.clear();
        self.farm_msg_buf = msgs;
    }

    pub(crate) fn process_farm_message(
        &mut self,
        msg: &[u8],
        farm_conn: &mut Option<Connection>,
        context: &mut Context,
        shared: &SharedState,
        event_tx: &Option<EventSink>,
        hb: &mut HeartbeatState,
    ) {
        let msg_type = match fast_extract_msg_type(msg) {
            Some(t) => t,
            None => return,
        };
        // Every message this connection carries, kept whole when asked. What a
        // subscription answers with is a question the wire answers; a reading
        // of it is not evidence of it.
        if *crate::engine::hot_loop::CAPTURE_WIRE {
            let hex: String = msg.iter().map(|b| format!("{b:02x}")).collect();
            shared.market.note_unread_wire("farm-msg", hex);
        }
        match msg_type {
            b"P" => self.handle_tick_data(msg, context, shared, event_tx),
            b"Q" => {
                if std::env::var("IBX_TRACE_Q").is_ok() {
                    log::info!("35=Q body: {}", String::from_utf8_lossy(msg).replace('\x01', "|"));
                }
                log::info!("Farm 35=Q subscription ack received");
                self.handle_subscription_ack(msg, context, shared);
            }
            b"0" => {}
            b"1" => {
                let parsed = fix::fix_parse(msg);
                let test_id = parsed.get(&fix::TAG_TEST_REQ_ID).cloned().unwrap_or_default();
                if let Some(conn) = farm_conn.as_mut() {
                    let ts = chrono_free_timestamp();
                    let result = conn.send_fix(&[
                        (fix::TAG_MSG_TYPE, fix::MSG_HEARTBEAT),
                        (fix::TAG_SENDING_TIME, &ts),
                        (fix::TAG_TEST_REQ_ID, &test_id),
                    ]);
                    log::info!("Farm TestReq '{}' -> heartbeat response seq={} result={:?}",
                        test_id, conn.seq, result);
                    hb.last_farm_sent = Instant::now();
                }
            }
            b"L" => self.handle_ticker_setup(msg, context, shared),
            // The venue refusing a subscription it was asked for, naming the
            // request that asked. Dropped, a caller that asked for depth on a
            // venue this account cannot see waits for data that was refused
            // before it started.
            b"3" => self.handle_subscription_reject(msg, context, shared),
            b"Y" => self.handle_depth_35y(msg, shared),
            b"G" => self.handle_generic_tick(msg, context, shared, event_tx),
            // Named once, the first time each arrives, the way the trading
            // connection names what it does not read. Reported where nobody
            // looks, a type the venue sends and this client drops is
            // indistinguishable from one it never sends.
            other => {
                let named = String::from_utf8_lossy(other).to_string();
                if self.unread_types.insert(named.clone()) {
                    shared.market.note_unread_wire("market data", format!("type {named}"));
                    log::info!(
                        "Unread on the market data connection: type {named}, {} bytes. Nothing \
                         here reads it, so whatever it carries is being discarded",
                        msg.len(),
                    );
                }
            }
        }
    }

    fn handle_tick_data(&mut self, msg: &[u8], context: &mut Context, shared: &SharedState, event_tx: &Option<EventSink>) {
        let body = match find_body_after_tag(msg, b"35=P\x01") {
            Some(b) => b,
            None => return,
        };

        let mut ticks = std::mem::take(&mut self.tick_buf);
        tick_decoder::decode_ticks_35p_into(body, &mut ticks);
        // Which instruments this batch touched, and each of them once. The
        // membership set is kept rather than rebuilt so nothing is zeroed per
        // batch, and the list beside it is what the publish loop walks — so
        // the work is the number of instruments in the batch rather than the
        // size of the table.
        let mut notified = std::mem::take(&mut self.notified);
        let mut notified_ids = std::mem::take(&mut self.notified_ids);

        // Every entry as decoded, before the apply loop below drops the types it
        // does not map. A field that arrives but is unmapped otherwise leaves no
        // trace anywhere, which is what let the identities above go wrong; this
        // is the measurement that settles what a given wire number carries.
        if log::log_enabled!(log::Level::Trace) {
            for tick in &ticks {
                log::trace!("35=P raw: server_tag={} type={} magnitude={}",
                    tick.server_tag, tick.tick_type, tick.magnitude);
            }
        }

        // Phase 1: Apply all ticks to internal quotes before publishing.
        for tick in &ticks {
            // An entry stating a decimal shift is not counted in the
            // contract's increments at all: the venue divides by ten to that
            // power instead. Applied as increments it would be wrong by
            // whatever the contract's increment happens to be, so it is left
            // out rather than published as a number nobody sent.
            if tick.decimal_shift != 0 {
                if self.unread_types.insert(format!("35=P shifted tick {}", tick.tick_type)) {
                    log::info!(
                        "a quote entry states its value {} places out, which this client \
                         counts in the contract's own increments and cannot state; dropped",
                        tick.decimal_shift,
                    );
                }
                continue;
            }
            let instrument = match context.market.instrument_by_server_tag(tick.server_tag) {
                Some(id) => id,
                None => {
                    // Said once for the number rather than once for the quote:
                    // a stream sends as fast held or not, and a line each would
                    // be the whole log. The ordinary case is what was in flight
                    // when a withdrawal went out, which arrives for a moment
                    // and stops.
                    if self.quotes_for_no_one.insert(tick.server_tag) {
                        log::warn!(
                            "quotes are arriving under venue number {}, which no contract \
                             in this session holds; dropped",
                            tick.server_tag,
                        );
                    }
                    continue;
                }
            };

            // A yield is stated on the numbers a price is stated on, and the
            // record says which by its layout. It is counted in ten
            // thousandths rather than in the contract's own increments — the
            // path the venue reads it back on does not use them — and it is
            // published beside the quote rather than in it, the way every
            // other series is.
            //
            // As a price, which is the callback these three belong on. The
            // reference client tells a caller what a bond's bid, ask and last
            // imply the same way it tells them the bid, the ask and the last:
            // the yield is a member of that family and travels with it, not
            // with the numbers that have no price behind them. Delivered as
            // one of those instead, a caller reading prices the way the
            // reference client's callers read them never saw the yield at all.
            if let Some(tick_type) = tick.layout.yield_tick(tick.tick_type) {
                let value = tick.magnitude as f64 * tick_decoder::YIELD_SCALE;
                shared.market.push_series_tick(crate::types::SeriesTick {
                    instrument, tick_type, value: crate::types::SeriesValue::Price(value),
                });
                continue;
            }
            // And what this number means under this record's layout. A sidecar
            // states the day's volume, the size of the last trade or the two
            // sides under the numbers an ordinary record states the bid, the
            // ask and the last under; read as ordinary, every one of them was
            // published as a price it is not.
            let Some(tick_type) = tick.layout.as_ordinary(tick.tick_type) else {
                if self.unread_types.insert(format!("35=P {:?} field {}", tick.layout, tick.tick_type)) {
                    log::debug!(
                        "a quote record of the {:?} kind states field {}, which nothing here \
                         reads",
                        tick.layout, tick.tick_type,
                    );
                }
                continue;
            };

            let mts = context.market.min_tick_scaled(instrument);
            // A size is a count of what the venue said sizes move in for this
            // contract, the same way a price is a count of what prices move in.
            let size_tick = context.market.size_tick(instrument);
            let (q, clock) = context.market.quote_and_clock_mut(instrument);

            // The extended 35=P format carries a full 8-bit byte width, so a
            // magnitude can reach i64::MAX and the scaling multiply wrapped to
            // an arbitrary price in release builds. Drop the price
            // rather than pinning it: a saturated value is ~92e9 at
            // PRICE_SCALE, which reads downstream as an ordinary quote, and
            // leaving the previous one standing is the honest failure.
            // And an increment of nothing scales every price to nothing,
            // which reads downstream as a real quote of zero rather than as
            // the absent increment it is. The venue states one per contract
            // and this is what happens when it has not, or when it states one
            // finer than the scale can hold. The tick-by-tick path already
            // refuses on the same test.
            let scaled = |m: i64| if mts > 0 { m.checked_mul(mts) } else { None };
            // Whether this entry changed the quote. An unmapped opcode does
            // not mark the instrument notified; it is visible in the raw trace
            // above instead.
            //
            // A price too large to scale is refused above, leaving the quote
            // unchanged, so it does not mark the instrument notified either.
            let mut applied = true;
            match tick_type {
                // Opcode 18 is deliberately not read as a halt. It was named
                // for one on no evidence, and the venue states a halt
                // elsewhere — as a generic tick carrying a status mask. A
                // wrong halt is worse than none: a caller told a contract is
                // trading when it is not prices against a book that is not
                // there.
                tick_decoder::O_BID_PRICE => {
                    match scaled(tick.magnitude) {
                        Some(v) => q.bid = v,
                        None => {
                            log::warn!("35=P bid price out of range (magnitude={}), tick dropped", tick.magnitude);
                            applied = false;
                        }
                    }
                }
                tick_decoder::O_ASK_PRICE => {
                    match scaled(tick.magnitude) {
                        Some(v) => q.ask = v,
                        None => {
                            log::warn!("35=P ask price out of range (magnitude={}), tick dropped", tick.magnitude);
                            applied = false;
                        }
                    }
                }
                tick_decoder::O_LAST_PRICE => {
                    match scaled(tick.magnitude) {
                        Some(v) => q.last = v,
                        None => {
                            log::warn!("35=P last price out of range (magnitude={}), tick dropped", tick.magnitude);
                            applied = false;
                        }
                    }
                }
                tick_decoder::O_HIGH_PRICE => {
                    match scaled(tick.magnitude) {
                        Some(v) => q.high = v,
                        None => {
                            log::warn!("35=P high price out of range (magnitude={}), tick dropped", tick.magnitude);
                            applied = false;
                        }
                    }
                }
                tick_decoder::O_LOW_PRICE => {
                    match scaled(tick.magnitude) {
                        Some(v) => q.low = v,
                        None => {
                            log::warn!("35=P low price out of range (magnitude={}), tick dropped", tick.magnitude);
                            applied = false;
                        }
                    }
                }
                tick_decoder::O_OPEN_PRICE => {
                    match scaled(tick.magnitude) {
                        Some(v) => q.open = v,
                        None => {
                            log::warn!("35=P open price out of range (magnitude={}), tick dropped", tick.magnitude);
                            applied = false;
                        }
                    }
                }
                tick_decoder::O_CLOSE_PRICE => {
                    match scaled(tick.magnitude) {
                        Some(v) => q.close = v,
                        None => {
                            log::warn!("35=P close price out of range (magnitude={}), tick dropped", tick.magnitude);
                            applied = false;
                        }
                    }
                }
                // Quantities are fixed-point, the same way prices are; every
                // reader divides by `QTY_SCALE` on the way out.
                tick_decoder::O_BID_SIZE => { q.bid_size = qty_from_counted(tick.magnitude, size_tick); }
                tick_decoder::O_ASK_SIZE => { q.ask_size = qty_from_counted(tick.magnitude, size_tick); }
                tick_decoder::O_LAST_SIZE => { q.last_size = qty_from_counted(tick.magnitude, size_tick); }
                tick_decoder::O_VOLUME => { q.volume = qty_from_counted(tick.magnitude, size_tick); }
                // Type 20 carries Unix seconds. Guarded because the same
                // type also carried a `yyyymmdd` value in capture, and a date
                // read as an epoch is worse than no timestamp.
                //
                // Type 21 is the seconds since that base, and the two add.
                // The reference client keeps them as a pair, per stream, and
                // settles it the same way: a base is stored and clears the
                // offset, and an offset only ever moves forward. Left out, a quote carried
                // the time of the last base for as long as one stood — every
                // print in between stamped with the same second.
                // Type 23 was previously folded in here and is now dropped:
                // it did not appear once in 733 captured entries on a future,
                // and it was writing a raw magnitude of unknown unit into a
                // nanosecond field. Left unmapped until a capture identifies
                // it rather than guessed at.
                tick_decoder::O_TS_BASE if tick.magnitude > 1_000_000_000 => {
                    clock.base_secs = tick.magnitude as u64;
                    clock.offset_secs = 0;
                    q.timestamp_ns = clock.base_secs.saturating_mul(1_000_000_000);
                }
                // Only ever forward, as the reference client keeps it: an
                // offset behind the one in hand belongs to a base that has
                // been replaced.
                tick_decoder::O_TS_OFFSET if clock.base_secs > 0 => {
                    let offset = tick.magnitude.max(0) as u64;
                    if offset >= clock.offset_secs {
                        clock.offset_secs = offset;
                        q.timestamp_ns = clock.base_secs
                            .saturating_add(offset)
                            .saturating_mul(1_000_000_000);
                    } else {
                        applied = false;
                    }
                }
                // What the venue says about the two prices rather than what
                // they are. Kept beside the quote rather than in it: a quote
                // is read on the hot path and sized to the cache lines it
                // occupies, and these change seldom. Read per side where a
                // caller is handed each price.
                tick_decoder::O_ELIGIBLE => {
                    let (_, state) = shared.market.quote_attribute_masks(instrument);
                    shared.market.note_quote_attributes(instrument, tick.magnitude, state);
                }
                tick_decoder::O_QUOTE_STATE => {
                    let (eligible, _) = shared.market.quote_attribute_masks(instrument);
                    shared.market.note_quote_attributes(instrument, eligible, tick.magnitude);
                }
                tick_decoder::O_BID_EXCH => { q.bid_exch_mask = tick.magnitude; }
                tick_decoder::O_ASK_EXCH => { q.ask_exch_mask = tick.magnitude; }
                tick_decoder::O_LAST_EXCH => { q.last_exch_mask = tick.magnitude; }
                _ => applied = false,
            }

            if applied {
                let word = &mut notified[(instrument >> 6) as usize];
                let bit = 1u64 << (instrument & 63);
                if *word & bit == 0 {
                    *word |= bit;
                    notified_ids.push(instrument);
                }
            }
        }

        // Phase 2: Publish complete quotes after all ticks in the batch are applied.
        for &instrument in &notified_ids {
            shared.market.push_quote(instrument, context.quote(instrument));
            emit(event_tx, Event::Tick(instrument));
            notified[(instrument >> 6) as usize] &= !(1u64 << (instrument & 63));
        }
        notified_ids.clear();
        self.notified = notified;
        self.notified_ids = notified_ids;
        self.tick_buf = ticks;
    }

    fn handle_subscription_ack(&mut self, msg: &[u8], context: &mut Context, shared: &SharedState) {
        let body = match find_body_after_tag(msg, b"35=Q\x01") {
            Some(b) => b,
            None => return,
        };
        let text = String::from_utf8_lossy(body);
        let text = text.split("\x018349=").next().unwrap_or(&text);
        let parts: Vec<&str> = text.trim().split(',').collect();
        if parts.len() < 3 { return; }
        let server_tag: u32 = match parts[0].parse() { Ok(v) => v, Err(_) => return };
        let req_id: u32 = match parts[1].parse() { Ok(v) => v, Err(_) => return };
        // The venue states the increment; a penny is not stood in where it did
        // not. Every price on this instrument is scaled by it, so a wrong one
        // is wrong prices rather than a wrong field.
        let Some(min_tick) = stated_increment(parts[2]) else {
            let told = format!(
                "this subscription was acknowledged with an increment prices cannot be \
                 counted in ({}), so none of its prices can be worked out",
                parts[2],
            );
            log::warn!("{told}");
            // Told to whoever asked. Left as a log line, the subscription
            // stands with no scale and every price on it is dropped, which
            // reads as a contract nobody is quoting.
            if let Some((_, instrument)) =
                self.md_req_to_instrument.iter().find(|(id, _)| *id == req_id)
            {
                shared.market.push_subscription_failure(*instrument, told);
            } else if let Some((_, asked_for)) =
                self.depth_fanout_map.iter().find(|(sub, _)| *sub == req_id)
            {
                shared.reference.push_historical_error(*asked_for, DEPTH_VENUE_REFUSED, told);
            }
            return;
        };

        // Depth ack: always map the server_tag if this req_id is a depth subscription,
        // even when depth_levels=0 (book empty now but updates may arrive later).
        let depth_levels: i32 = parts.get(4).and_then(|s| s.parse().ok()).unwrap_or(0);
        if let Some((_, is_smart)) = self.depth_subs.iter().find(|(id, _)| *id == req_id) {
            let is_smart = *is_smart;
            // For SmartDepth fan-out, map back to the user's original req_id
            let user_req = self.depth_fanout_map.iter()
                .find(|(sub, _)| *sub == req_id)
                .map(|(_, user)| *user)
                .unwrap_or(req_id);
            // Which venue this subscription's levels stand on. Every
            // subscription is asked for at one named venue, so every level has
            // one to be placed on.
            let venue = self.depth_fanout_exchange.iter()
                .find(|(sub, _)| *sub == req_id)
                .map(|(_, exch)| exch.clone())
                .unwrap_or_default();
            // The venue answers a second subscription on a contract and venue
            // it is already streaming with the tag it is already using, so two
            // acks can name the same one. Two records then match every update
            // and the book applies each level twice. The generic-tick branch
            // below already keeps one record per tag.
            self.depth_tag_to_req.retain(|(tag, id, ..)| {
                !(*tag == server_tag && *id == user_req)
            });
            self.depth_tag_to_req.push((
                server_tag,
                user_req,
                is_smart,
                min_tick,
                trailing_size_increment(&parts).unwrap_or(1.0),
                venue.clone(),
            ));
            log::info!("Depth ack: server_tag {server_tag} -> req_id {user_req} venue={venue:?} (levels={depth_levels}, smart={is_smart}, min_tick={min_tick})");
            return;
        }

        // L1 ack. What says an answer is a late one is the request it names,
        // not the number the venue put on it: the venue hands its numbers out
        // again, so a subscription opened after another was withdrawn is
        // routinely answered under the number the withdrawn one held. Refused
        // on the number, every such subscription was acknowledged, bound to
        // nothing and left silent — with no refusal, on any contract whose
        // number came round again, for the rest of the session.
        //
        // The request is the whole of it. An answer to a request nothing is
        // waiting on falls out below, where the wait is looked up and there is
        // none.
        let instrument = match self.md_req_to_instrument.iter()
            .position(|(id, _)| *id == req_id)
        {
            Some(idx) => {
                let (_, instr) = self.md_req_to_instrument.remove(idx);
                instr
            }
            None => return,
        };

        // A generic tick is numbered apart from the prices, and its frames say
        // nothing about which tick they carry. What was asked for under this
        // request is the only thing that does, so the two are recorded
        // together here and read back when a frame arrives.
        if let Some((_, request_type)) =
            self.generic_tick_reqs.iter().find(|(id, _)| *id == req_id).copied()
        {
            self.generic_tick_tags.retain(|(tag, ..)| *tag != server_tag);
            self.generic_tick_tags.push((server_tag, request_type, instrument));
            log::info!(
                "Generic tick {request_type} on instrument {instrument} -> server_tag {server_tag}"
            );
            return;
        }

        context.market.register_server_tag(server_tag, instrument);
        context.market.set_min_tick(instrument, min_tick);
        shared.market.push_tick_req_params(instrument, min_tick);
        // The venue has taken it, so whatever it said the last time it would
        // not is no longer what a request joining this contract is owed.
        //
        // The chargeable snapshot does not say that. It is a request of its
        // own and nothing joins it — one is never followed — so a snapshot the
        // venue takes on a contract whose stream it refused says only that the
        // snapshot was served. Read as the contract being live, it cleared the
        // refusal a joiner of that dead stream was owed, and the joiner was
        // told nothing was wrong with a subscription nothing was arriving on.
        let is_a_snapshot = self.instrument_md_reqs.iter()
            .find(|(id, _)| *id == instrument)
            .is_some_and(|(_, record)| record.entries.iter().any(|e| {
                e.req_id == req_id && e.request_type == REGULATORY_SNAPSHOT_REQUEST_TYPE
            }));
        if !is_a_snapshot {
            shared.market.note_subscription_accepted(instrument);
        }
        if let Some(size_tick) = trailing_size_increment(&parts) {
            context.market.set_size_tick(instrument, size_tick);
        }
        log::info!("Subscribed instrument {instrument} -> server_tag {server_tag}, minTick {min_tick}");
    }

    /// The venue refusing something this connection asked for.
    ///
    /// It names the request on tag 262 and says why on tag 58, so unlike the
    /// trading connection's own error channel this one can be handed back to
    /// the caller that asked. A refusal read as nothing leaves that caller
    /// waiting on data the venue already said it would not send.
    fn handle_subscription_reject(&mut self, msg: &[u8], context: &Context, shared: &SharedState) {
        let parsed = fix::fix_parse(msg);
        let said = parsed.get(&58).map(String::as_str).unwrap_or("");
        if said.is_empty() {
            return;
        }
        // The venue writes these as "Error&VENUE/TYPE/WHAT". The lead-in says
        // only that it is one, which the caller already knows from being told.
        let reason = said.strip_prefix("Error&").unwrap_or(said);
        let req_id = parsed.get(&262).and_then(|s| s.parse::<u32>().ok());
        let instrument = req_id.and_then(|rid| {
            self.md_req_to_instrument.iter().find(|(id, _)| *id == rid).map(|(_, i)| *i)
        });
        match instrument {
            Some(instrument) => {
                let named = context.market.symbol(instrument);
                // A request riding beside the quote — the trading status, the
                // exchange map, the option model — is refused on its own. The
                // quote it rides beside is not, and its prices go on arriving;
                // told the quote was refused, a caller withdrew it.
                if let Some(rid) = req_id
                    && let Some(&(_, kind)) = self.generic_tick_reqs.iter().find(|(id, _)| *id == rid)
                {
                    // The news beside the quote is refused on its own. Left in
                    // place, the entry the rebuild reads re-sends it on the
                    // next reconnect — the venue refuses it again — and a
                    // headline that never comes is waited on; so it is
                    // released, the request and its filing both. The other
                    // companions — the trading status, the exchange map, the
                    // option model — carry no such standing state to release.
                    if kind == NEWS_REQUEST_TYPE
                        && let Some(pos) = self.news_subscriptions.iter().position(|(_, id, ..)| *id == rid)
                    {
                        let (.., con_id, _) = self.news_subscriptions.remove(pos);
                        self.forget_news(rid, instrument);
                        // Told so the client clears whoever asked, else its
                        // dedup holds a re-ask against a claim the venue
                        // already refused and no fresh subscription is sent.
                        shared.market.push_news_rejection(con_id);
                    }
                    // Told, not only logged. The venue names the request it is
                    // refusing and says why — the one refusal channel on this
                    // wire that does — and a caller that asked for the option
                    // model on a class the venue has no model for watched an
                    // acknowledged subscription that could never produce a
                    // computation, with the reason sitting in a log line.
                    shared.market.push_companion_refusal(
                        instrument,
                        kind,
                        format!("the venue refused {} on {named}: {reason}", companion_named(kind)),
                    );
                    log::warn!("The venue refused a request beside the quote on {named}: {reason}");
                    return;
                }
                // The chargeable snapshot is refused on its own as well, and
                // the acknowledgement path already says why: it is a request
                // of its own, nothing joins it, and what the venue says about
                // it says nothing about the stream on the same contract. The
                // refusal side had no such reading, so a snapshot declined for
                // want of the entitlement — the documented outcome — was
                // recorded against the contract: every caller watching a
                // healthy, ticking stream was told their quote had been
                // refused, and every later joiner was handed that reason
                // beside the quote it contradicts. Nothing cleared it either,
                // because only a fresh acknowledgement does and a subscribe
                // that joins an existing subscription never draws one.
                //
                // Where there is no stream to protect, the refusal is the
                // contract's: the snapshot's caller is the only one watching,
                // and it is who the reason is for.
                let refuses_a_snapshot = req_id.is_some_and(|rid| {
                    self.instrument_md_reqs.iter().any(|(id, record)| {
                        *id == instrument
                            && record.entries.iter().any(|e| {
                                e.req_id == rid
                                    && e.request_type == REGULATORY_SNAPSHOT_REQUEST_TYPE
                            })
                    })
                });
                if refuses_a_snapshot && self.holds_a_stream(instrument) {
                    log::warn!("The venue refused the snapshot on {named}: {reason}");
                    return;
                }
                log::warn!("The venue refused a subscription on {named}: {reason}");
                shared.market.push_subscription_failure(
                    instrument, format!("the venue refused this subscription: {reason}"),
                );
            }
            // A depth subscription asks under an id of its own, and one venue's
            // refusal does not end the caller's request: one book is asked for
            // at several venues and the others may answer. The caller asked
            // once, so they are told once — when the last venue has refused
            // and there is no book coming.
            None => {
                log::warn!("The venue refused a subscription: {reason}");
                let Some(rid) = req_id else { return };
                let fanned_out = self.depth_fanout_map.iter()
                    .find(|(sub, _)| *sub == rid)
                    .map(|(_, user)| *user);
                // Naming nothing this client still asks under, the book was
                // withdrawn or refused before this arrived, and there is no
                // caller to tell. Handed on as it stood, the wire number was
                // published as though it were a caller's request number, and
                // whoever held that number was told a book had been refused.
                let Some(asked_for) = fanned_out else {
                    log::info!("the refusal names {rid}, which no book here asks under any more");
                    return;
                };
                // Every record of the refused book. The two beside the map
                // stayed for the life of the connection, scanned on every
                // acknowledgement and every subscribe, and a later
                // acknowledgement of the wire id would have filed the book
                // under the wire number as though a caller held it.
                self.depth_fanout_map.retain(|(sub, _)| *sub != rid);
                self.depth_subs.retain(|(sub, _)| *sub != rid);
                self.depth_fanout_exchange.retain(|(sub, _)| *sub != rid);
                if self.depth_fanout_map.iter().any(|(_, u)| *u == asked_for) {
                    return;
                }
                // No venue is going to answer, so the book is over — and what
                // the withdrawal drops has to go the same way. The record the
                // reconnect rebuilds from stayed, so every reconnect asked for
                // the refused book again, told the caller its book had been
                // emptied before doing so, and drew the same refusal: two
                // messages a reconnect, for the rest of the session, on a
                // request already answered once. The routing stayed with it,
                // so a refusal arriving after an acknowledgement went on
                // handing levels to a number that had been told there was no
                // book. The headlines beside this release their own replay
                // record for exactly this reason.
                self.depth_resub_info.retain(|(id, ..)| *id != asked_for);
                self.depth_tag_to_req.retain(|(_, rid, ..)| *rid != asked_for);
                self.depth_rows.retain(|(id, _)| *id != asked_for);
                shared.reference.push_historical_error(
                    asked_for,
                    DEPTH_VENUE_REFUSED,
                    format!("the venue refused depth here: {reason}"),
                );
            }
        }
    }

    fn handle_ticker_setup(&mut self, msg: &[u8], context: &mut Context, shared: &SharedState) {
        let body = match find_body_after_tag(msg, b"35=L\x01") {
            Some(b) => b,
            None => return,
        };
        let text = String::from_utf8_lossy(body);
        let text = text.split("\x018349=").next().unwrap_or(&text);
        let parts: Vec<&str> = text.trim().split(',').collect();
        if parts.len() < 3 { return; }
        let con_id: i64 = match parts[0].parse() { Ok(v) => v, Err(_) => return };
        let Some(min_tick) = stated_increment(parts[1]) else {
            let told = format!(
                "a depth subscription was acknowledged with an increment prices cannot \
                 be counted in ({}), so none of its levels can be worked out",
                parts[1],
            );
            log::warn!("{told}");
            if let Some(instrument) = context.market.instrument_by_con_id(con_id) {
                shared.market.push_subscription_failure(instrument, told);
            }
            return;
        };
        let server_tag: u32 = match parts[2].parse() { Ok(v) => v, Err(_) => return };

        if let Some(instrument) = context.market.instrument_by_con_id(con_id) {
            context.market.register_server_tag(server_tag, instrument);
            context.market.set_min_tick(instrument, min_tick);
            if let Some(size_tick) = trailing_size_increment(&parts) {
                context.market.set_size_tick(instrument, size_tick);
            }
            log::info!("Ticker setup: con_id {con_id} -> server_tag {server_tag}, minTick {min_tick}");
            // A news subscription on this contract may be acknowledged here,
            // keyed by the contract, rather than under its own number. Its
            // frames are told apart from the prices only by the tag's type,
            // so the tag is filed as the news tag or every headline is
            // dropped.
            if self.news_subscriptions.iter().any(|(i, ..)| *i == instrument) {
                self.generic_tick_tags.retain(|(tag, ..)| *tag != server_tag);
                self.generic_tick_tags.push((server_tag, NEWS_REQUEST_TYPE, instrument));
            }
        }
    }

    /// Subscribe to the headlines on `instrument`, asked for under `req_id`.
    ///
    /// A market-data request, on the market-data connection: the tick is
    /// 292, the venue is `NEWS`, and the providers ride on 6472. Sent on the
    /// trading connection, the venue refused it as a message that connection
    /// does not carry, and no headline could ever arrive.
    pub(crate) fn send_news_subscribe(
        &mut self,
        con_id: i64,
        instrument: InstrumentId,
        sec_type: &str,
        providers: &str,
        req_id: u32,
        farm_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
    ) {
        self.news_subscriptions.push((instrument, req_id, providers.to_string(), con_id, sec_type.to_string()));
        // Filed before it is sent: the venue numbers a generic tick apart from
        // the prices and says what it carries once, on the acknowledgement,
        // and the reader tells a frame's bytes apart only by what was asked
        // for under the tag.
        self.arm_news_request(req_id, instrument);
        let Some(conn) = farm_conn.as_mut() else { return };
        let req_id_str = req_id.to_string();
        let con_id_str = (con_id as u32).to_string();
        // What the contract is, as the venue states it. Stamped as a US
        // stock, headlines for a future or an index went out describing
        // something else.
        let stated_type = crate::control::contracts::sec_type_to_fix(sec_type).to_string();
        let tick = NEWS_REQUEST_TYPE.to_string();
        let sent = conn.send_fixcomp(&[
            (fix::TAG_MSG_TYPE, fix::MSG_MARKET_DATA_REQ),
            (263, SUBSCRIBE_ACTION),
            (146, "1"),
            (262, &req_id_str),
            (6008, &con_id_str),
            (207, NEWS_VENUE),
            (167, &stated_type),
            (264, &tick),
            (6472, providers),
        ]);
        hb.last_farm_sent = Instant::now();
        match sent {
            Ok(()) => log::info!("Sent news subscribe: con_id={con_id} req_id={req_id} providers={providers}"),
            // The write failing means the socket is going: the write-dead
            // sweep gives the farm up and its 2103 tells the caller the feed
            // is broken, and the reconnect rebuild re-sends this from
            // `news_subscriptions`. So the entry is left in place — not taken
            // out of the one list that re-asks — and not refused under an
            // internal request id the caller never issued.
            Err(e) => {
                log::warn!("news subscribe did not go out: con_id={con_id} req_id={req_id}: {e}");
            }
        }
    }

    /// Withdraw the headlines on `instrument`, stated the way they were asked
    /// for: which tick, on which contract, on which venue, of which type.
    pub(crate) fn send_news_unsubscribe(
        &mut self,
        instrument: InstrumentId,
        farm_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
    ) {
        let Some(pos) = self.news_subscriptions.iter().position(|(id, ..)| *id == instrument) else { return };
        let (_, req_id, _, con_id, sec_type) = self.news_subscriptions.remove(pos);
        self.forget_news(req_id, instrument);
        let Some(conn) = farm_conn.as_mut() else { return };
        let req_id_str = req_id.to_string();
        let con_id_str = (con_id as u32).to_string();
        let stated_type = crate::control::contracts::sec_type_to_fix(&sec_type).to_string();
        let tick = NEWS_REQUEST_TYPE.to_string();
        let sent = conn.send_fixcomp(&[
            (fix::TAG_MSG_TYPE, fix::MSG_MARKET_DATA_REQ),
            (263, "2"),
            (146, "1"),
            (262, &req_id_str),
            (6008, &con_id_str),
            (207, NEWS_VENUE),
            (167, &stated_type),
            (264, &tick),
        ]);
        hb.last_farm_sent = Instant::now();
        match sent {
            Ok(()) => log::info!("Sent news unsubscribe: instrument={instrument} req_id={req_id}"),
            // Reported as what it is. Logged as sent, a withdrawal that never
            // left reads as one the venue took, and headlines keep arriving
            // for a subscription no caller is reading.
            Err(e) => log::warn!(
                "News unsubscribe for instrument={instrument} req_id={req_id} was not sent: {e}; \
                 the venue goes on serving it until the session ends",
            ),
        }
    }

    /// The request and its tick, where an acknowledgement under the request's
    /// number will find them.
    fn arm_news_request(&mut self, req_id: u32, instrument: InstrumentId) {
        self.md_req_to_instrument.retain(|(rid, _)| *rid != req_id);
        self.md_req_to_instrument.push((req_id, instrument));
        self.generic_tick_reqs.retain(|(rid, _)| *rid != req_id);
        self.generic_tick_reqs.push((req_id, NEWS_REQUEST_TYPE));
    }

    /// Forget a news subscription: its request, and the tag it was filed
    /// under. A headline arriving after is a tick nothing asked for.
    fn forget_news(&mut self, req_id: u32, instrument: InstrumentId) {
        self.md_req_to_instrument.retain(|(rid, _)| *rid != req_id);
        self.generic_tick_reqs.retain(|(rid, _)| *rid != req_id);
        self.generic_tick_tags
            .retain(|(_, tick, i)| !(*tick == NEWS_REQUEST_TYPE && *i == instrument));
    }

    pub(crate) fn send_mktdata_subscribe(
        &mut self,
        con_id: i64,
        symbol: &str,
        exchange: &str,
        sec_type: &str,
        last_trade_date: &str,
        strike: f64,
        right: &str,
        multiplier: &str,
        instrument: InstrumentId,
        mode_9887: i32,
        regulatory_snapshot: bool,
        farm_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
    ) {
        // The slot holds a contract of its own again, so the route the last
        // occupancy's callers were sent along is over: a withdrawal naming this
        // slot from here on is about what is being asked for now, and the
        // contract on it says whose.
        self.moved_to.remove(&instrument);
        // A stream is the pair BID_ASK + LAST on every feed; a delayed or
        // frozen one names the feed beside them on 9887 and asks for the same
        // two. The chargeable snapshot is one entry whatever the feed, so it
        // takes one id and has no second leg to route.
        let realtime = mode_9887 == 0 && !regulatory_snapshot;
        let bid_ask_id = self.next_md_req_id;
        let last_id = self.next_md_req_id + 1;
        if regulatory_snapshot {
            self.next_md_req_id += 1;
        } else {
            self.next_md_req_id += 2;
        }


        // An option is worth asking the venue to model. Anything else has no
        // volatility to imply, and the venue answers such a request with
        // nothing at all.
        let models_a_volatility = matches!(
            crate::control::contracts::sec_type_to_fix(sec_type),
            "OPT" | "FOP" | "IOPT" | "WAR",
        );
        let greeks_req_id = if models_a_volatility && con_id > 0 {
            let id = self.next_md_req_id;
            self.next_md_req_id += 1;
            Some(id)
        } else {
            None
        };

        let status_req_id = self.next_md_req_id;
        self.next_md_req_id += 1;

        self.md_req_to_instrument.push((bid_ask_id, instrument));
        self.md_req_to_instrument.push((status_req_id, instrument));
        self.generic_tick_reqs.push((status_req_id, TRADING_STATUS_REQUEST_TYPE));

        let venue_map_req_id = self.next_md_req_id;
        self.next_md_req_id += 1;
        self.md_req_to_instrument.push((venue_map_req_id, instrument));
        self.generic_tick_reqs.push((venue_map_req_id, BBO_EXCHANGE_MAP_REQUEST_TYPE));

        // One request per extra series the caller named, allocated whether or
        // not the farm is up: what was asked for is bookkeeping, and the
        // withdrawal has to find every one of them either way.
        //
        // Except the ones this subscription already asks for on its own. The
        // trading status, the venue map and the option model are series like
        // any other and a caller may name them; named and allocated again,
        // one caller's request took out two subscriptions to the same series
        // on the same contract under different numbers, against the same
        // allowance. And a chargeable snapshot carries no extra series at all
        // — the message it sends leaves them out — so allocating them here
        // recorded rows nothing sent and made the withdrawal state them.
        let already_asked = [
            TRADING_STATUS_REQUEST_TYPE,
            BBO_EXCHANGE_MAP_REQUEST_TYPE,
            GREEKS_REQUEST_TYPE,
        ];
        let asked_ticks: Vec<u32> = if regulatory_snapshot {
            Vec::new()
        } else {
            self.asked_generic_ticks.get(&instrument)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|tick| !already_asked.contains(tick))
                .collect()
        };
        let mut extra_series: Vec<(u32, u32)> = Vec::new();
        for &tick in &asked_ticks {
            let id = self.next_md_req_id;
            self.next_md_req_id += 1;
            self.md_req_to_instrument.push((id, instrument));
            self.generic_tick_reqs.push((id, tick));
            extra_series.push((id, tick));
        }
        if !regulatory_snapshot {
            self.md_req_to_instrument.push((last_id, instrument));
        }
        if let Some(id) = greeks_req_id {
            self.md_req_to_instrument.push((id, instrument));
            self.generic_tick_reqs.push((id, GREEKS_REQUEST_TYPE));
            // Recorded whether or not the farm is up: what was asked for is
            // bookkeeping, and a cancel has to find it either way.
            self.greeks_subs.push((id, con_id, sec_type.to_string()));
        }

        // Each entry as it goes onto the wire: the number it is asked under,
        // the kind of market data that number carries, and the venue it is
        // asked on. Kept so the withdrawal can state each one the way the
        // subscription stated it — named by number alone, the venue leaves it
        // being served.
        let (venue, wire_sec_type) = stated_venue_and_type(sec_type, exchange);
        let venue = venue.to_string();
        let mut entries = if regulatory_snapshot {
            vec![MdReqEntry { req_id: bid_ask_id, request_type: REGULATORY_SNAPSHOT_REQUEST_TYPE, venue: venue.clone() }]
        } else {
            vec![
                MdReqEntry {
                    req_id: bid_ask_id,
                    request_type: REALTIME_BID_ASK_REQUEST_TYPE,
                    venue: venue.clone(),
                },
                MdReqEntry {
                    req_id: last_id,
                    request_type: REALTIME_LAST_REQUEST_TYPE,
                    venue: venue.clone(),
                },
            ]
        };
        entries.push(MdReqEntry { req_id: status_req_id, request_type: TRADING_STATUS_REQUEST_TYPE, venue: venue.clone() });
        entries.push(MdReqEntry { req_id: venue_map_req_id, request_type: BBO_EXCHANGE_MAP_REQUEST_TYPE, venue: venue.clone() });
        // Each series the caller named is an entry of the subscription, as the
        // greeks are: the withdrawal is composed from these, and a number left
        // out of them was never withdrawn — the venue went on serving it
        // against the account's allowance, and its request number outlived the
        // slot it was asked on, so an acknowledgement still in flight bound the
        // old contract's series onto whatever contract the slot went to next.
        for &(id, tick) in &extra_series {
            entries.push(MdReqEntry { req_id: id, request_type: tick, venue: venue.clone() });
        }
        if let Some(id) = greeks_req_id {
            entries.push(MdReqEntry { req_id: id, request_type: GREEKS_REQUEST_TYPE, venue: GREEKS_VENUE.to_string() });
        }
        match self.instrument_md_reqs.iter_mut().find(|(id, _)| *id == instrument) {
            Some((_, record)) => {
                // A snapshot can precede the stream on this contract. The
                // stream's withdrawal carries the selector it was asked with.
                if !regulatory_snapshot {
                    record.mode_9887 = mode_9887;
                }
                record.entries.extend(entries);
            }
            None => {
                self.instrument_md_reqs.push((instrument, MdReqRecord {
                    con_id,
                    sec_type: wire_sec_type.to_string(),
                    mode_9887,
                    entries,
                }));
            }
        }
        // A chargeable snapshot is not recorded for replay: the caller asked
        // for the contract once, and a reconnect that re-sent it would deliver
        // — and bill for — a second burst nobody asked for.
        if !regulatory_snapshot && self.md_resub_info.iter().all(|(id, ..)| *id != instrument) {
            self.md_resub_info.push((instrument, symbol.to_string(), exchange.to_string(), sec_type.to_string(), last_trade_date.to_string(), strike, right.to_string(), multiplier.to_string(), mode_9887));
        }

        if let Some(conn) = farm_conn.as_mut() {
            let bid_ask_str = bid_ask_id.to_string();
            let last_str = last_id.to_string();
            let mode_str = mode_9887.to_string();
            let ts = chrono_free_timestamp();

            // 146 = NoRelatedSym count: 2 entries for the stream's pair, 1 for
            // the chargeable snapshot.
            let no_related_sym = if regulatory_snapshot { "1" } else { "2" };

            let fix_exchange = crate::control::contracts::exchange_to_fix(exchange);
            let fix_sec_type = crate::control::contracts::sec_type_to_fix(sec_type);

            // When con_id is known the server still routes the subscription by
            // SecurityType (167) and Exchange (207), so both must describe the
            // actual contract. When con_id is 0, include the descriptive fields
            // as well so the server can resolve by description.
            if con_id > 0 {
                let tags = build_conid_subscribe_tags(
                    realtime, regulatory_snapshot, bid_ask_id, last_id, con_id, exchange, sec_type,
                    mode_9887, &ts, &extra_series,
                );
                let refs: Vec<(u32, &str)> =
                    tags.iter().map(|(tag, val)| (*tag, val.as_str())).collect();
                let _ = conn.send_fixcomp(&refs);

                // The venue's option model is a subscription of its own, not a
                // field on the price one: it names the model in place of an
                // exchange and its own tick in place of a request type. A
                // subscription that does not ask for it is sent prices alone.
                if let Some(greeks_id) = greeks_req_id {
                    let tags = build_greeks_subscribe_tags(greeks_id, con_id, sec_type, &ts);
                    let refs: Vec<(u32, &str)> =
                        tags.iter().map(|(tag, val)| (*tag, val.as_str())).collect();
                    let _ = conn.send_fixcomp(&refs);
                }

                // Whether the venue is trading the contract at all. A halt
                // changes what every price on this subscription means — the
                // ones standing are from before it stopped — so it is asked for
                // alongside them rather than left to a caller to think of.
                //
                // Asked for under the same request as the prices, so a caller
                // reading a halt reads it against the quote it belongs to.
                let tags = build_trading_status_subscribe_tags(
                    status_req_id, con_id, sec_type, exchange, &ts,
                );
                let refs: Vec<(u32, &str)> =
                    tags.iter().map(|(tag, val)| (*tag, val.as_str())).collect();
                let _ = conn.send_fixcomp(&refs);

                // And which venue each bit of the exchange mask means, which
                // the server states rather than this client assuming.
                let mut venue_map = build_trading_status_subscribe_tags(
                    venue_map_req_id, con_id, sec_type, exchange, &ts,
                );
                for (tag, value) in venue_map.iter_mut() {
                    if *tag == 264 {
                        *value = BBO_EXCHANGE_MAP_REQUEST_TYPE.to_string();
                    }
                }
                let refs: Vec<(u32, &str)> =
                    venue_map.iter().map(|(tag, val)| (*tag, val.as_str())).collect();
                let _ = conn.send_fixcomp(&refs);

            } else {
                // No con_id — send descriptive fields
                let strike_str = if strike > 0.0 { strike.to_string() } else { String::new() };
                let mut tags: Vec<(u32, &str)> = vec![
                    (fix::TAG_MSG_TYPE, fix::MSG_MARKET_DATA_REQ),
                    (fix::TAG_SENDING_TIME, &ts),
                    (263, if regulatory_snapshot { SNAPSHOT_ACTION } else { SUBSCRIBE_ACTION }),
                    (146, no_related_sym),
                ];
                let snapshot_type = REGULATORY_SNAPSHOT_REQUEST_TYPE.to_string();
                let entries: &[(&String, &str)] = if regulatory_snapshot {
                    &[(&bid_ask_str, &snapshot_type)]
                } else {
                    &[(&bid_ask_str, "442"), (&last_str, "443")]
                };
                for (req_str, depth) in entries {
                    tags.push((262, req_str));
                    tags.push((55, symbol));
                    tags.push((207, fix_exchange));
                    tags.push((167, fix_sec_type));
                    if !last_trade_date.is_empty() { tags.push((200, last_trade_date)); }
                    if strike > 0.0 { tags.push((202, &strike_str)); }
                    if !right.is_empty() { tags.push((201, right)); }
                    if !multiplier.is_empty() { tags.push((231, multiplier)); }
                    tags.push((264, depth));
                    tags.push((6088, "Socket"));
                    tags.push((9830, "1"));
                    tags.push((9839, "1"));
                    if !realtime && !regulatory_snapshot { tags.push((9887, &mode_str)); }
                }
                let _ = conn.send_fixcomp(&tags);
            }
            if regulatory_snapshot {
                log::info!("Sent 35=V snapshot: con_id={} sec_type={} id={} seq={}",
                    con_id, sec_type, bid_ask_id, conn.seq);
            } else {
                log::info!("Sent 35=V subscribe (9887={}): con_id={} sec_type={} ids={},{} seq={}",
                    mode_9887, con_id, sec_type, bid_ask_id, last_id, conn.seq);
            }
            hb.last_farm_sent = Instant::now();
        }
    }

    pub(crate) fn send_mktdata_unsubscribe(
        &mut self,
        instrument: InstrumentId,
        con_id: i64,
        took_it: u64,
        series: &[u32],
        issued: u64,
        moving_in: bool,
        farm_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
    ) {
        // Not a subscription this withdrawal is about. A slot is reusable, so
        // its number says nothing about which occupancy ended: the caller names
        // the contract it believed was there and the subscription says which
        // contract it went out under. Where they differ, the slot has been
        // given to another contract since the withdrawal was decided, and
        // sending it would take down a subscription that caller never asked
        // for.
        if self.another_contract_holds_it(instrument, con_id)
            || self.another_occupancy_holds_it(instrument, took_it)
        {
            log::debug!(
                "a withdrawal of slot {instrument} named contract {con_id}, taken under \
                 {took_it}, which is not the subscription now on it; it stands",
            );
            return;
        }
        // A slot whose callers were sent elsewhere holds nothing, and what they
        // asked for beyond the quote went with them: the withdrawal of those
        // series belongs to the slot they were sent to. Applied here, it found
        // nothing and the venue went on serving them there with nobody asking.
        if !self.instrument_md_reqs.iter().any(|(id, _)| *id == instrument)
            && let Some(into) = self.where_its_callers_went(instrument)
        {
            // Under whatever holds the slot they were sent to: the caller
            // that named this withdrawal never took that slot, and the series
            // it gave up are entries of the subscription now on it. Named with
            // nothing, the withdrawal was applied wherever the walk ended
            // whether or not that subscription was the one its caller joined.
            let held_under = self.what_took_it(into);
            self.stop_asking_for_series(
                into, self.what_contract_holds_it(into), held_under, series, issued, farm_conn, hb,
            );
            return;
        }
        // Nor while a caller is on its way onto this slot and has not read it
        // yet. It asked for the contract and is not yet recorded as watching
        // it, so the decision to withdraw could not see it: the subscription
        // went, and that caller arrived on a slot with nothing on the wire.
        if moving_in {
            log::debug!(
                "a withdrawal of slot {instrument} was decided while a caller was on its \
                 way onto it; the subscription stands",
            );
            self.stop_asking_for_series(instrument, con_id, took_it, series, issued, farm_conn, hb);
            return;
        }
        // Not a subscription this withdrawal is about. The client decides to
        // withdraw and says so a moment later, and a caller asking for the
        // same contract in between is answered off the subscription that is
        // up — so the withdrawal that follows would take down a subscription
        // decided against before that caller ever asked for it, leaving it
        // published as watching a contract with nothing on the wire.
        if self.began_after(instrument, issued) {
            log::debug!(
                "a withdrawal of slot {instrument} was decided before the subscription \
                 now on it began; the subscription stands",
            );
            // What that caller asked for beyond the quote is still its own to
            // give up, and each series answers for itself: one another caller
            // has since asked for stays, and one nobody else named goes.
            self.stop_asking_for_series(instrument, con_id, took_it, series, issued, farm_conn, hb);
            return;
        }
        self.subscription_asked_on.remove(&instrument);
        // The occupancy stays until the slot itself goes back: the release that
        // follows names it, and cleared here that release named nothing —
        // which the client reads as "forget whatever is on that slot now",
        // including a subscription that had just been given it.
        self.series_asked_on.retain(|(watched, _), _| *watched != instrument);
        // Drop the resubscribe record first. The lookup below early-returns
        // when the instrument has no active requests, which is always the case
        // while the farm is down — `handle_disconnect` cleared that list — so
        // an unsubscribe issued during an outage would otherwise leave the
        // record standing and the reconnect would re-subscribe an instrument
        // the caller explicitly cancelled.
        self.md_resub_info.retain(|(id, ..)| *id != instrument);
        // And out of the replay queue, for the same reason. A reconnect moves
        // the records here and empties the list above, so between the reconnect
        // and the last burst this is the only place the subscription is
        // written down — left standing, the replay re-sends a subscription the
        // caller has just cancelled.
        self.replay_queue.retain(|r| r.0 != instrument);
        // The extra series asked for on this contract go with it. Left behind,
        // the entry outlives every subscription that used it and the next
        // caller to watch this slot is given a series nobody asked for.
        self.asked_generic_ticks.remove(&instrument);
        // And so do the totals a running series was being read against. Left
        // behind, the first reading after this contract is watched again is
        // measured from a total the venue stated in another subscription — a
        // print for everything that traded in between, or for a negative
        // number of shares where the venue has started its day over.
        self.rt_volume_totals.retain(|(watched, _), _| *watched != instrument);
        let record = match self.instrument_md_reqs.iter()
            .position(|(id, _)| *id == instrument)
        {
            Some(idx) => self.instrument_md_reqs.remove(idx).1,
            None => return,
        };
        let reqs: Vec<u32> = record.entries.iter().map(|e| e.req_id).collect();
        // Forget the pending acks too. A `35=Q` still in flight when the
        // unsubscribe goes out would otherwise resolve its request id after
        // the slot has been reclaimed and reused, binding this subscription's
        // server_tag AND its minTick onto whatever contract now holds the
        // slot — prices for the new contract then scale by the old one's tick
        // size, which reads as plausible rather than broken.
        self.md_req_to_instrument.retain(|(req_id, _)| !reqs.contains(req_id));
        // And what was asked for under those requests, for the same reason: a
        // number the venue hands to the next subscription would otherwise
        // still be read as the tick this one asked for.
        self.generic_tick_reqs.retain(|(req_id, _)| !reqs.contains(req_id));
        // The news tick is asked for under its own request and withdrawn under
        // its own, so the tag it was filed under is not this withdrawal's to
        // take. Dropped here, the subscription stands while its headlines
        // arrive under a tag nothing reads.
        let news_stands = self.news_subscriptions.iter().any(|(i, ..)| *i == instrument);
        self.generic_tick_tags.retain(|(_, tick, held)| {
            *held != instrument || (news_stands && *tick == NEWS_REQUEST_TYPE)
        });
        // The option-model records go with the withdrawal whether or not the
        // farm is up: left behind, they outlive the subscription they
        // describe.
        self.greeks_subs.retain(|(id, ..)| !reqs.contains(id));

        let conn = match farm_conn.as_mut() {
            Some(c) => c,
            None => return,
        };

        let asked = record.entries.len();
        let con_id_str = (record.con_id as u32).to_string();
        for entry in &record.entries {
            // Withdrawn the way it was asked for: the venue is told which
            // tick, on which contract, on which venue, not merely which
            // request. Named by the number alone the venue leaves it being
            // served, and the number stays held against whoever asks under it
            // next on this connection.
            let req_id_str = entry.req_id.to_string();
            let request_type_str = entry.request_type.to_string();
            let mut tags: Vec<(u32, &str)> = vec![
                (fix::TAG_MSG_TYPE, fix::MSG_MARKET_DATA_REQ),
                (263, "2"),
                (146, "1"),
                (262, &req_id_str),
                (6008, &con_id_str),
                (207, entry.venue.as_str()),
                (167, record.sec_type.as_str()),
                (264, &request_type_str),
                // Stated the way the subscription stated them. The venue
                // writes an entry from one encoder whichever action carries
                // it, so a withdrawal short of the fields the subscription
                // carried is not the same entry coming back.
                (6088, "Socket"),
                (9830, "1"),
                (9839, "1"),
            ];
            // On every entry the subscription carried it on, which is every
            // entry of a delayed or frozen stream — the extra series among
            // them. Written only on the two price entries, a series asked for
            // on a delayed stream was withdrawn without the field it was asked
            // with, and a withdrawal short of the subscription's fields is one
            // the venue leaves being served. The chargeable snapshot is served
            // from no feed and is asked for without it.
            let mode_str = record.mode_9887.to_string();
            if record.mode_9887 != 0
                && entry.request_type != REGULATORY_SNAPSHOT_REQUEST_TYPE
            {
                tags.push((9887, &mode_str));
            }
            let _ = conn.send_fixcomp(&tags);
        }
        // Said, because the venue serves a limited number of these at once and
        // a withdrawal that leaves no trace cannot be told apart from one that
        // never went. The subscribe on the way in is logged; this is the other
        // half of the pair, and without it a session that runs out of room
        // looks like a venue that stopped sending for no reason.
        log::info!("Sent 35=V unsubscribe: instrument={instrument} requests={asked}");
        hb.last_farm_sent = Instant::now();
    }

    /// Ask for a book, on the venue the caller named or on none.
    ///
    /// The venue is passed through as the caller stated it: named, and the book
    /// is that venue's; empty or smart-routed, and the venue aggregates one.
    /// Either way it is one subscription, asked for under an id of this
    /// client's own and mapped back to the caller's.
    pub(crate) fn send_depth_subscribe(
        &mut self,
        req_id: u32,
        con_id: i64,
        exchange: &str,
        primary_exchange: &str,
        sec_type: &str,
        num_rows: i32,
        is_smart_depth: bool,
        farm_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
        shared: &SharedState,
    ) {
        // A book the venue does not serve where this is routed.
        //
        // The routing answer names, per market and security type, which kinds
        // of data are served. For the smart destination and a stock it names
        // one — the top of the book — and never a deep one, in any region. So
        // a book asked for there is asked of somewhere that has none, and the
        // answer is nothing at all: no refusal, no book, no way to tell which.
        //
        // Said as a refusal instead. Only where the table was read and names
        // the market: a table this client does not have is not evidence that
        // the venue serves nothing.
        // Under the name the venue routes by, not the one a caller was handed.
        // The smart destination is remapped before sending and this
        // client already does it for quotes and for orders; a book asked for
        // under the caller's spelling was the one request still going out
        // under a name the venue does not route by.
        let destination = match exchange {
            "" | "SMART" | "IBKRATS" | "BEST" => "BEST",
            other => other,
        };

        if let Some(conn) = farm_conn.as_ref()
            && !conn.routing.is_empty()
            && !sec_type.is_empty()
        {
            // Whichever name the venue serves a book under here. Asked only
            // about one of them, two markets that serve a book under another
            // read as serving none, and a request the venue would have
            // answered is refused instead.
            let book = conn.routing.book_endpoint(destination, sec_type);
            // Which server the table says serves it. A book asked for on a
            // connection to another farm is acknowledged and then silent,
            // which is indistinguishable from a market with nothing to send.
            if let Some(endpoint) = book
                && let Some(route) = conn.routing.find(destination, sec_type, endpoint)
            {
                log::info!(
                    "book for {sec_type} {con_id} on {destination} is {endpoint}, \
                     served by {} on {} ({num_rows} rows asked)",
                    route.farm, route.host,
                );
            }
            let serves_a_book = book.is_some();
            let named_at_all = conn.routing.find(destination, sec_type, "Top").is_some();
            if named_at_all && !serves_a_book {
                shared.reference.push_historical_error(
                    req_id,
                    crate::error_codes::Refusal::VALIDATION,
                    format!(
                        "the venue serves no book for a {sec_type} on {destination}, \
                         only the top of one — ask on the exchange the contract \
                         trades on instead",
                    ),
                );
                return;
            }
        }

        let fix_sec_type = match sec_type {
            "STK" => "CS", "FUT" => "FUT", "OPT" => "OPT", "IND" => "IND",
            "CASH" => "CASH", other => other,
        };
        // The caller's request, kept so a reconnect can ask for it again. What
        // is registered as a subscription is the id this client asks under,
        // one per venue, below.
        // One record per request, not one per time it was asked for. A caller
        // that asks twice under the same number without withdrawing gets two
        // otherwise, and a reconnect then asks the venue for the book twice.
        self.depth_resub_info.retain(|(id, ..)| *id != req_id);
        self.depth_resub_info.push((
            req_id, con_id, exchange.to_string(), primary_exchange.to_string(),
            sec_type.to_string(), num_rows, is_smart_depth,
        ));

        // A book on no particular venue is asked for once, on no particular
        // venue. The venue aggregates it.
        //
        // Asking each venue the contract is routed to instead — which is what
        // names the venue on every level — costs one subscription per venue,
        // and all but one of them is refused by name. Four contracts cycled
        // that way put seventy-odd subscribes and as many withdrawals on the
        // connection every minute, and the venue stopped answering it
        // altogether: no error, no loss, quotes and books both silent. One
        // request is what the protocol defines and what the connection can
        // carry.
        // A book on one named venue is a book gathered from one venue. Asking
        // under an id of this client's own either way is what keeps the two
        // apart: the venue echoes the id back, and a caller's id sent as it
        // stands is indistinguishable from one of these. A caller's second
        // book was answered under another subscription's venue because both
        // were numbered 2.
        let venues: Vec<String> = vec![destination.to_string()];
        // What the caller asked for is recorded whether or not the socket is
        // up. Recorded only when it was, a book asked for while the farm was
        // down could not be withdrawn, and the reconnect asked for it again.
        self.depth_rows.retain(|(id, _)| *id != req_id);
        if num_rows > 0 {
            self.depth_rows.push((req_id, num_rows));
        }

        // At most one live wire subscription per caller, whatever put a
        // previous one there.
        //
        // The three records below are what a row is routed by, and nothing
        // deduped them on the caller's number. Two ways in: a caller asking
        // twice without withdrawing, which left two contracts' rows arriving
        // interleaved under one number with nothing to tell them apart and a
        // withdrawal that named only the later contract; and a book asked for
        // while this connection was down, which recorded a wire id nothing
        // ever sent -- so when the reconnect asked properly and the venue
        // refused, the refusal saw the phantom still asking and was swallowed,
        // and the caller waited for ever.
        //
        // Cleared here rather than refused, because the reconnect below asks
        // again under the same number by design and must not be refused for
        // it. A caller asking twice is refused at the surface, before this.
        self.depth_subs.retain(|(under, _)| {
            !self.depth_fanout_map.iter().any(|(u, user)| u == under && *user == req_id)
        });
        self.depth_fanout_exchange.retain(|(under, _)| {
            !self.depth_fanout_map.iter().any(|(u, user)| u == under && *user == req_id)
        });
        self.depth_fanout_map.retain(|(_, user)| *user != req_id);

        let mut asked_under = Vec::with_capacity(venues.len());
        for venue in &venues {
            let under = self.next_md_req_id;
            self.next_md_req_id += 1;
            self.depth_subs.push((under, is_smart_depth));
            self.depth_fanout_map.push((under, req_id));
            self.depth_fanout_exchange.push((under, venue.clone()));
            asked_under.push(under);
        }
        log::info!(
            "Book for req={req_id} con_id={con_id} gathered from {}",
            venues.join(", "),
        );

        if let Some(conn) = farm_conn.as_mut() {
            let con_id_str = (con_id as u32).to_string();
            for (under, venue) in asked_under.iter().zip(&venues) {
                self.send_depth_one(
                    conn, &under.to_string(), &con_id_str, venue, fix_sec_type,
                );
            }
            hb.last_farm_sent = Instant::now();
        }
    }

    pub(crate) fn send_depth_unsubscribe(
        &mut self,
        req_id: u32,
        farm_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
    ) {
        // What this client asked under for the caller's book — one venue or
        // several. The caller's own id never went to the venue, so it is not
        // what is withdrawn.
        let asked_under: Vec<u32> = self.depth_fanout_map.iter()
            .filter(|(_, user)| *user == req_id)
            .map(|(sub, _)| *sub)
            .collect();
        // Each entry as it was asked for, gathered before the records go:
        // withdrawn the way it was subscribed, by contract and venue and type
        // as well as number, or the venue leaves it being served.
        let mut entries: Vec<(u32, i64, String, String)> = Vec::new();
        if let Some((_, con_id, _, _, sec_type, ..)) =
            self.depth_resub_info.iter().find(|(id, ..)| *id == req_id)
        {
            let con_id = *con_id;
            let fix_sec_type = crate::control::contracts::sec_type_to_fix(sec_type).to_string();
            for sub in &asked_under {
                let venue = self.depth_fanout_exchange.iter()
                    .find(|(s, _)| s == sub)
                    .map(|(_, venue)| venue.clone())
                    .unwrap_or_default();
                entries.push((*sub, con_id, venue, fix_sec_type.clone()));
            }
        }
        // Cleared whether or not anything was asked yet: left behind, a book
        // the caller withdrew was asked for again by the next reconnect.
        self.depth_resub_info.retain(|(id, ..)| *id != req_id);
        // And the routing, whether or not anything was asked: a tag record
        // left behind after the venue refused the book mid-stream was
        // inherited by the next contract asked for under this number, which
        // then read the old book as its own.
        self.depth_subs.retain(|(id, _)| !asked_under.contains(id));
        self.depth_fanout_map.retain(|(_, user)| *user != req_id);
        self.depth_fanout_exchange.retain(|(sub, _)| !asked_under.contains(sub));
        // The venue's numbers this caller's book was arriving under, taken
        // before its records go — the record holds the caller's own number,
        // not the numbers this client asked under, so the two are not
        // interchangeable and a book released by comparing them was released
        // for nobody.
        let arriving_under: Vec<u32> = self.depth_tag_to_req.iter()
            .filter(|(_, rid, ..)| *rid == req_id)
            .map(|(tag, ..)| *tag)
            .collect();
        self.depth_tag_to_req.retain(|(_, rid, ..)| *rid != req_id);
        // The book itself goes with the stream that was carrying it, and only
        // where nobody else is still reading that stream: kept, it would
        // answer for the window of whatever stream took the number next.
        self.depth_books.retain(|(tag, _)| {
            !arriving_under.contains(tag)
                || self.depth_tag_to_req.iter().any(|(held, ..)| held == tag)
        });
        self.depth_rows.retain(|(id, _)| *id != req_id);
        if asked_under.is_empty() {
            return;
        }

        if let Some(conn) = farm_conn.as_mut() {
            for (sub_req, con_id, venue, fix_sec_type) in &entries {
                let sub_req_str = sub_req.to_string();
                let con_id_str = (*con_id as u32).to_string();
                let _ = conn.send_fixcomp(&[
                    (fix::TAG_MSG_TYPE, fix::MSG_MARKET_DATA_REQ),
                    (263, "2"),
                    (146, "1"),
                    (262, &sub_req_str),
                    (6008, &con_id_str),
                    (207, venue.as_str()),
                    (167, fix_sec_type.as_str()),
                    (264, DEEP_REQUEST),
                    (6088, "Socket"),
                    (9830, "1"),
                    (9839, "1"),
                ]);
            }
            hb.last_farm_sent = Instant::now();
            log::info!(
                "Sent depth unsubscribe for req_id={req_id}: {} venue(s)",
                entries.len(),
            );
        }
    }

    /// Ask for series on a contract already being watched.
    ///
    /// A joining caller brings its own list, and a series nobody asked for yet
    /// has to be asked for or it never arrives: the first caller's list is
    /// what went to the venue. What is already being served is not asked for
    /// twice — the venue answers a second subscription to the same series
    /// under the same number, and two records for one number read every frame
    /// twice.
    pub(crate) fn also_ask_for_series(
        &mut self,
        instrument: InstrumentId,
        named_contract: i64,
        wanted: &[u32],
        context: &Context,
        farm_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
    ) {
        // The contract the caller joined, and not whatever holds the slot now.
        // A slot is given back and handed on, and this is handled after the
        // caller that asked has been registered — so named by slot alone, one
        // contract's series were asked for on another's.
        let Some(holds_the_slot) = context.market.con_id(instrument) else { return };
        if named_contract != 0 && holds_the_slot != named_contract {
            log::debug!(
                "a series asked for on contract {named_contract} arrived for slot \
                 {instrument}, which now holds {holds_the_slot}; nothing is asked",
            );
            return;
        }
        // The ones this subscription already asks for on its own are not
        // asked for again, as they are not at the start: a caller may name the
        // trading status, the venue map or the option model, and a second
        // subscription to the same series on the same contract is two numbers
        // against one allowance.
        let already_asked = [
            TRADING_STATUS_REQUEST_TYPE,
            BBO_EXCHANGE_MAP_REQUEST_TYPE,
            GREEKS_REQUEST_TYPE,
        ];
        // Nothing is written down before the subscription this would join is
        // found: a list created here for a slot that holds no stream is a list
        // the next contract to take the slot is asked for.
        let already: Vec<u32> =
            self.asked_generic_ticks.get(&instrument).cloned().unwrap_or_default();
        let new_ones: Vec<u32> = wanted.iter()
            .copied()
            .filter(|tick| !already.contains(tick) && !already_asked.contains(tick))
            .collect();
        if new_ones.is_empty() {
            return;
        }
        // What the subscription this joins was asked for, from the record the
        // rebuild reads — or from the queue, where a reconnect that is still
        // pacing its way through has put it. Looked for in the record alone, a
        // caller joining between the reconnect and the burst that carries this
        // contract was answered while its series went nowhere: the queue holds
        // what will be sent, and the list below is what is sent with it.
        let known = self.md_resub_info
            .iter()
            .find(|(id, ..)| *id == instrument)
            .map(|(_, _, exchange, sec_type, .., mode)| (exchange.clone(), sec_type.clone(), *mode))
            .or_else(|| self.replay_queue
                .iter()
                .find(|(id, ..)| *id == instrument)
                .map(|(_, _, _, exchange, sec_type, .., mode)| {
                    (exchange.clone(), sec_type.clone(), *mode)
                }));
        let Some((exchange, sec_type, mode_9887)) = known else {
            // Nothing here says what contract that slot holds, so there is
            // nothing to ask on and nothing for a rebuild to ask on either.
            return;
        };
        // The record the rebuild reads is what the caller asked for, so it is
        // written the moment the subscription this joins is found — before the
        // checks below, which are about what can be sent now. Written after
        // them, a series named while the connection was down was dropped on
        // the floor: the call answered, the rebuild asked for the list as it
        // was, and the caller waited on a stream nobody had asked for.
        self.asked_generic_ticks.entry(instrument).or_default().extend(new_ones.iter().copied());
        let Some(con_id) = context.market.con_id(instrument).filter(|id| *id > 0) else { return };
        // Nothing is allocated where there is no live subscription to add it
        // to. A connection that has gone takes the record with it and leaves
        // the caller's list, which the rebuild reads: numbers allocated here
        // would then belong to no subscription and to no withdrawal, and the
        // rebuild allocates its own.
        if !self.instrument_md_reqs.iter().any(|(id, _)| *id == instrument) {
            return;
        }

        let mut rows: Vec<(u32, u32)> = Vec::new();
        for tick in new_ones {
            let id = self.next_md_req_id;
            self.next_md_req_id += 1;
            self.md_req_to_instrument.push((id, instrument));
            self.generic_tick_reqs.push((id, tick));
            rows.push((id, tick));
        }
        // Each is an entry of the subscription, as the ones asked for at the
        // start are: the withdrawal is composed from those.
        let (venue, _) = stated_venue_and_type(&sec_type, &exchange);
        let venue = venue.to_string();
        if let Some((_, record)) = self.instrument_md_reqs.iter_mut()
            .find(|(id, _)| *id == instrument)
        {
            for (id, tick) in &rows {
                record.entries.push(MdReqEntry {
                    req_id: *id, request_type: *tick, venue: venue.clone(),
                });
            }
        }
        if let Some(conn) = farm_conn.as_mut() {
            let ts = chrono_free_timestamp();
            let tags = build_series_subscribe_tags(
                con_id, &exchange, &sec_type, mode_9887, &ts, &rows,
            );
            let refs: Vec<(u32, &str)> =
                tags.iter().map(|(tag, val)| (*tag, val.as_str())).collect();
            let _ = conn.send_fixcomp(&refs);
            hb.last_farm_sent = Instant::now();
            log::info!(
                "Asked for {} more series on slot {instrument}, for a caller that joined",
                rows.len(),
            );
        }
    }

    /// Record which request the subscription on a slot belongs to, as one
    /// number in the order of everything the client has asked for.
    ///
    /// The highest wins: a caller that joins a stream already up is asking for
    /// that contract too, and a withdrawal decided before it asked is not
    /// about the subscription it is being served off.
    pub(crate) fn note_subscription_asked_on(&mut self, instrument: InstrumentId, issued: u64) {
        let held = self.subscription_asked_on.entry(instrument).or_insert(issued);
        *held = (*held).max(issued);
    }

    /// Forget when a slot's subscription and its series were asked for.
    ///
    /// The slot is going back to the table, so the numbers belong to a contract
    /// that has gone: read against a withdrawal of the contract that takes the
    /// slot next, they leave that subscription standing for ever.
    pub(crate) fn forget_what_was_asked_on(&mut self, instrument: InstrumentId) {
        self.subscription_asked_on.remove(&instrument);
        self.subscription_began_under.remove(&instrument);
        self.series_asked_on.retain(|(watched, _), _| *watched != instrument);
        // The route this slot's callers were sent along is kept: a withdrawal
        // decided against the occupancy that left is still on its way, and the
        // route is the only thing that can carry it to where they went.
        //
        // A route *into* this slot is not dropped either — it is pointed past
        // it, at where the callers went from here. Dropped, a withdrawal
        // decided against the slot at the head of a route lost its way the
        // moment a slot in the middle of it went back to the table.
        match self.where_its_callers_went(instrument) {
            Some(end) if end != instrument => {
                for (_, into) in self.moved_to.iter_mut().filter(|(_, into)| **into == instrument) {
                    *into = end;
                }
            }
            _ => self.moved_to.retain(|_, into| *into != instrument),
        }
    }

    /// Where a slot's callers ended up, following as many moves as they were
    /// sent through.
    ///
    /// A contract can be moved more than once — resolution sends the callers of
    /// one slot to another, and that slot's own contract can turn out to live
    /// somewhere else again. Followed one hop, a withdrawal decided against the
    /// first slot reached the second and did nothing there.
    fn where_its_callers_went(&self, from: InstrumentId) -> Option<InstrumentId> {
        let mut at = *self.moved_to.get(&from)?;
        // Every step is a slot, and each is stepped through once: a slot seen
        // twice means the routes lead in a circle, and there is no end to
        // deliver the withdrawal to.
        let mut walked = std::collections::HashSet::from([from, at]);
        while let Some(&next) = self.moved_to.get(&at) {
            if !walked.insert(next) {
                log::warn!("the moves off slot {from} lead in a circle; nothing is withdrawn");
                return None;
            }
            at = next;
        }
        Some(at)
    }

    /// Say that a slot's callers, and what they asked for, were sent to another
    /// slot.
    pub(crate) fn note_moved(&mut self, from: InstrumentId, into: InstrumentId) {
        self.moved_to.insert(from, into);
    }

    /// Name this occupancy of a slot by the number the request that took it
    /// asked under.
    pub(crate) fn note_subscription_began_under(&mut self, instrument: InstrumentId, took_it: u64) {
        // Only where the slot holds no occupancy yet. A request that joins what
        // is already there — the venue's one-shot beside a stream, a second
        // caller on the same contract — did not begin it, and renaming it left
        // the caller that did unable to take its own subscription down.
        self.subscription_began_under.entry(instrument).or_insert(took_it);
    }

    /// Say that a slot's occupancy has changed hands, whatever it was before.
    ///
    /// The callers of another slot have been moved onto this one and hold a
    /// number of their own now: what could withdraw it before cannot any more,
    /// and what arrived can.
    pub(crate) fn note_it_changed_hands(&mut self, instrument: InstrumentId, took_it: u64) {
        self.subscription_began_under.insert(instrument, took_it);
    }

    /// Whether a slot is held by an occupancy, or a contract, other than the
    /// one a caller named. Asked before anything is recorded against it.
    pub(crate) fn another_occupancy_holds_it_now(
        &self, instrument: InstrumentId, took_it: u64, con_id: i64,
    ) -> bool {
        self.another_occupancy_holds_it(instrument, took_it)
            || self.another_contract_holds_it(instrument, con_id)
    }

    /// Which contract a slot's subscription went out under, as far as this
    /// engine's record of it says. Zero where it holds none.
    fn what_contract_holds_it(&self, instrument: InstrumentId) -> i64 {
        self.instrument_md_reqs
            .iter()
            .find(|(id, _)| *id == instrument)
            .map_or(0, |(_, record)| record.con_id)
    }

    /// Which occupancy of a slot holds it, as the number the request that took
    /// it asked under. Zero where nothing here has one for it.
    pub(crate) fn what_took_it(&self, instrument: InstrumentId) -> u64 {
        self.subscription_began_under.get(&instrument).copied().unwrap_or(0)
    }

    /// Whether a slot's subscription is a different occupancy from the one a
    /// caller took.
    ///
    /// A caller that states nothing — a withdrawal carried through a move, or a
    /// session closing — is not compared, and the contract and the order of the
    /// decisions are what is left.
    fn another_occupancy_holds_it(&self, instrument: InstrumentId, took_it: u64) -> bool {
        if took_it == 0 {
            return false;
        }
        self.subscription_began_under
            .get(&instrument)
            .is_some_and(|began| *began != took_it)
    }

    /// The same for each series a request named on a slot.
    pub(crate) fn note_series_asked_on(
        &mut self, instrument: InstrumentId, ticks: &[u32], issued: u64,
    ) {
        for tick in ticks {
            let held = self.series_asked_on.entry((instrument, *tick)).or_insert(issued);
            *held = (*held).max(issued);
        }
    }

    /// Whether the subscription on a slot went out under a different contract
    /// from the one a caller named.
    ///
    /// The contract is the identity of an occupancy and the slot is not: the
    /// slot goes to the next contract that needs one. A caller that named no
    /// contract — every contract the venue has not identified yet — states
    /// zero, and then there is nothing to compare and the numbers decide.
    fn another_contract_holds_it(&self, instrument: InstrumentId, con_id: i64) -> bool {
        if con_id == 0 {
            return false;
        }
        self.instrument_md_reqs
            .iter()
            .find(|(id, _)| *id == instrument)
            .is_some_and(|(_, record)| record.con_id != con_id)
    }

    /// Whether the subscription on a slot was asked for after a decision was
    /// taken.
    fn began_after(&self, instrument: InstrumentId, issued: u64) -> bool {
        self.subscription_asked_on.get(&instrument).is_some_and(|began| *began > issued)
    }

    /// Stop asking for series on a contract whose subscription stands.
    ///
    /// The other half of `also_ask_for_series`. The caller that brought a
    /// series with it withdraws while the subscription it joined stays up for
    /// whoever opened it, and the series nobody asks for any more goes with
    /// it: each is an entry of the subscription, withdrawn as itself the way
    /// the whole subscription withdraws its entries. Left behind, the venue
    /// served it for as long as the subscription outlived its caller — against
    /// an allowance that is counted — and the rebuild after a reconnect asked
    /// for it again.
    ///
    /// Which series nobody asks for any more is the client's question, not
    /// this one's: only the client knows what each caller named.
    pub(crate) fn stop_asking_for_series(
        &mut self,
        instrument: InstrumentId,
        con_id: i64,
        took_it: u64,
        unwanted: &[u32],
        issued: u64,
        farm_conn: &mut Option<Connection>,
        hb: &mut HeartbeatState,
    ) {
        // Nor are these that subscription's series. See
        // `send_mktdata_unsubscribe`.
        if self.another_contract_holds_it(instrument, con_id)
            || self.another_occupancy_holds_it(instrument, took_it)
        {
            return;
        }
        // The headlines are asked for under a request of their own and
        // withdrawn under it, so they are not this withdrawal's to take. Nor
        // are the entries every subscription opens for itself — the trading
        // status, the venue map and, on a contract that has one, the option
        // model: a caller naming one of those is not asked for a second
        // subscription to it, so it is not that caller's to withdraw either.
        // Withdrawn here, a caller that named the trading status took the
        // trading status off a subscription that goes on running, and nothing
        // asks for it again until a reconnect.
        let unwanted: Vec<u32> = unwanted.iter()
            .copied()
            .filter(|tick| {
                ![
                    NEWS_REQUEST_TYPE,
                    TRADING_STATUS_REQUEST_TYPE,
                    BBO_EXCHANGE_MAP_REQUEST_TYPE,
                    GREEKS_REQUEST_TYPE,
                ]
                .contains(tick)
            })
            // And nothing another caller has asked for since this was decided.
            // Answered for the slot as a whole, a caller asking for one series
            // kept every series on it, and the one this caller had given up
            // went on being served with nobody reading it.
            .filter(|tick| {
                !self.series_asked_on
                    .get(&(instrument, *tick))
                    .is_some_and(|asked| *asked > issued)
            })
            .collect();
        if unwanted.is_empty() {
            return;
        }
        // The caller's list first, because that is what the rebuild after a
        // reconnect asks for. Left standing, a series withdrawn on this
        // connection came back on the next one with nobody asking for it.
        if let Some(asked) = self.asked_generic_ticks.get_mut(&instrument) {
            asked.retain(|tick| !unwanted.contains(tick));
            if asked.is_empty() {
                self.asked_generic_ticks.remove(&instrument);
            }
        }
        // And when they were asked for, here rather than once the wire state
        // below has been found: a withdrawal while the connection is down has
        // no wire state to take and left the number behind, so a contract
        // joined and given up often enough grew the record for ever.
        for tick in &unwanted {
            self.series_asked_on.remove(&(instrument, *tick));
        }
        let Some((_, record)) = self.instrument_md_reqs.iter_mut()
            .find(|(id, _)| *id == instrument)
        else {
            return;
        };
        // Each entry as it went out: the number it was asked under, what it
        // asked for, and the venue it was asked on. A withdrawal short of
        // those is one the venue leaves being served.
        let going: Vec<(u32, u32, String)> = record.entries.iter()
            .filter(|entry| unwanted.contains(&entry.request_type))
            .map(|entry| (entry.req_id, entry.request_type, entry.venue.clone()))
            .collect();
        if going.is_empty() {
            return;
        }
        record.entries.retain(|entry| !unwanted.contains(&entry.request_type));
        let con_id_str = (record.con_id as u32).to_string();
        let sec_type = record.sec_type.clone();
        let mode_9887 = record.mode_9887;
        let reqs: Vec<u32> = going.iter().map(|(req_id, ..)| *req_id).collect();
        // And everything filed under those numbers, the way the withdrawal of
        // a whole subscription forgets them: a number the venue hands to the
        // next subscription would otherwise still be read as the series this
        // one asked for.
        self.md_req_to_instrument.retain(|(req_id, _)| !reqs.contains(req_id));
        self.generic_tick_reqs.retain(|(req_id, _)| !reqs.contains(req_id));
        self.generic_tick_tags
            .retain(|(_, tick, held)| *held != instrument || !unwanted.contains(tick));
        self.greeks_subs.retain(|(id, ..)| !reqs.contains(id));
        // And the totals a running series was being read against, as the
        // withdrawal of a whole subscription drops them. Kept, the first
        // reading after somebody asks for that series again is measured from
        // the total the venue stated before it stopped — a print for
        // everything that traded while nobody was asking.
        for (tick, counted) in [(233u32, 48i32), (375, 77)] {
            if unwanted.contains(&tick) {
                self.rt_volume_totals.remove(&(instrument, counted));
            }
        }
        let Some(conn) = farm_conn.as_mut() else { return };
        let mode_str = mode_9887.to_string();
        for (req_id, request_type, venue) in &going {
            let req_id_str = req_id.to_string();
            let request_type_str = request_type.to_string();
            let mut tags: Vec<(u32, &str)> = vec![
                (fix::TAG_MSG_TYPE, fix::MSG_MARKET_DATA_REQ),
                (263, "2"),
                (146, "1"),
                (262, &req_id_str),
                (6008, &con_id_str),
                (207, venue.as_str()),
                (167, sec_type.as_str()),
                (264, &request_type_str),
                (6088, "Socket"),
                (9830, "1"),
                (9839, "1"),
            ];
            // Stated the way the subscription stated it, as the withdrawal of
            // a whole subscription states it: a withdrawal short of the
            // fields the subscription carried is not the same entry coming
            // back, and the venue leaves it being served.
            if mode_9887 != 0 && *request_type != REGULATORY_SNAPSHOT_REQUEST_TYPE {
                tags.push((9887, &mode_str));
            }
            let _ = conn.send_fixcomp(&tags);
        }
        hb.last_farm_sent = Instant::now();
        log::info!(
            "Stopped asking for {} series on slot {instrument}, which nobody watches for",
            going.len(),
        );
    }

    /// Ask for one contract's book.
    ///
    /// The same request whether the caller named a venue or left it to the
    /// venue to aggregate one. Asked for as a quote
    /// instead, the venue answers with quote frames, which this client read
    /// with its book reader: a bid of 143.87 and an ask of a penny on a share
    /// trading at 772, a book made of misread quotes, which is worse than no
    /// book. Under the request type that means a book the venue answers —
    /// with the book, or by refusing the entitlement, and either reaches the
    /// caller.
    fn send_depth_one(
        &self, conn: &mut Connection, req_id_str: &str, con_id_str: &str,
        exchange: &str, sec_type: &str,
    ) {
        // 9839 rides on a book the same way it rides on a quote: the venue
        // states it on every entry it writes, a book's among them, and this
        // was the one entry this client left it off.
        let _ = conn.send_fixcomp(&[
            (fix::TAG_MSG_TYPE, fix::MSG_MARKET_DATA_REQ),
            (263, "1"), (146, "1"), (262, req_id_str),
            (6008, con_id_str), (207, exchange), (167, sec_type),
            (264, DEEP_REQUEST), (6088, "Socket"), (9830, "1"), (9839, "1"),
        ]);
    }

    /// Keep a depth frame exactly as the venue sent it, when asked to.
    ///
    /// A reading checked only against frames this client made up says nothing
    /// about the ones that arrive.
    fn note_depth_wire(&self, kind: &'static str, body: &[u8], shared: &SharedState) {
        if !*crate::engine::hot_loop::CAPTURE_WIRE {
            return;
        }
        let hex: String = body.iter().map(|b| format!("{b:02x}")).collect();
        shared.market.note_unread_wire(kind, hex);
    }

    /// Whether a level is inside the depth its caller asked for.
    ///
    /// The venue sends what it has. A caller that asked for five levels and is
    /// handed every one the venue sends was handed a different book from the
    /// one it asked for, and the reference client it was written against would
    /// have shown five.
    fn within_asked_depth(&self, req_id: u32, position: i32) -> bool {
        match self.depth_rows.iter().find(|(id, _)| *id == req_id) {
            Some((_, rows)) => position < *rows,
            // Nothing asked for in particular, so nothing is too deep.
            None => true,
        }
    }

    /// How many levels a caller asked for, where it named a number.
    fn asked_depth(&self, req_id: u32) -> Option<i32> {
        self.depth_rows.iter()
            .find(|(id, _)| *id == req_id)
            .map(|(_, rows)| *rows)
            .filter(|rows| *rows > 0)
    }

    /// A book frame's payload, read a bit at a time, most significant first.
    ///
    /// The frame states how many bits follow its two-byte count, and the
    /// layout is not byte-aligned once a name of odd length has been read, so
    /// nothing below indexes a byte directly.
    fn book_bits(body: &[u8]) -> Option<BookBits<'_>> {
        let stated = u16::from_be_bytes([*body.first()?, *body.get(1)?]);
        let bytes = &body[2..];
        // Recovered across the wrap, as every bit-counted section on this wire
        // is: two bytes state the count, so it repeats every sixty-five
        // thousand five hundred and thirty-six, and how much arrived is what
        // says how many times. Read as stated, a book of four thousand
        // withdrawals — a full cycle exactly, which states nought — was
        // dropped whole, and the caller's book kept every level the venue had
        // just taken away.
        let end = crate::protocol::tick_decoder::bits_carried(stated, body.len());
        // A count the bytes do not satisfy is a frame cut short. Read up to
        // the bytes instead, part of a book arrives as the whole of it: the
        // levels the frame did not finish stating read as levels the venue no
        // longer holds.
        if end > bytes.len() * 8 {
            return None;
        }
        Some(BookBits { bytes, at: 0, end })
    }

    /// Parse a 35=Y book frame and deliver its levels.
    ///
    /// The layout, as the wire states it, after the two-byte bit count:
    ///
    /// ```text
    /// section := more:1  tag:31  entry+
    /// entry   := more:1  _:1  op:2  name_len:4  name:8×name_len  position:8  field*
    /// field   := id:5  more:1  len:2  sign:1  magnitude:(8·(len+1)−1)
    /// ```
    ///
    /// `op` is 0 insert, 1 update, 2 delete on the bid, 3 delete on the ask,
    /// and a delete carries no fields. A field's meaning is `id >> 2`: 0 the
    /// bid price, 1 the ask price, 4 the bid size, 5 the ask size; an id of 31
    /// opens a longer field (`id:8`, then `len:8`), and anything else is read
    /// and left. A field list ends when a field says it is the last, an
    /// entry list when an entry says it is, and the frame when a section says
    /// it is or the bits run out.
    ///
    /// Read by byte shape before — a marker byte opening a section, a marker
    /// byte opening an entry, a byte of field tag — which held wherever the
    /// bits happened to line up and failed everywhere else: an update or a
    /// delete, whose entry byte is neither of the two shapes looked for, was
    /// skipped as an unknown byte, so a book here could never shrink; the
    /// flag that says more fields follow was read as a snapshot flag and
    /// turned into the insert/update distinction; and an entry whose position
    /// byte was zero read as a section switch.
    fn handle_depth_35y(&mut self, msg: &[u8], shared: &SharedState) {
        use crate::types::DepthUpdate;
        self.note_depth_wire("depth-35y", msg, shared);
        let Some(body) = find_body_after_tag(msg, b"35=Y\x01") else { return };
        let Some(mut bits) = Self::book_bits(body) else { return };

        loop {
            let (Some(more_sections), Some(tag)) = (bits.take(1), bits.take(31)) else { return };
            let tag = tag as u32;
            // Nothing is delivered from a section naming a stream this session
            // does not hold: the withdrawal of a book is on its way to the
            // venue while the frames it already sent are still arriving.
            let held = self.lookup_depth_stag(tag);
            let subscribers = if held.is_some() { self.depth_subscribers_of(tag) } else { Vec::new() };
            let (min_tick, size_tick) = held.map_or((0.01, 1.0), |(_, _, mt, st, _)| (mt, st));
            let counted_in = if size_tick > 0.0 { size_tick } else { 1.0 };

            loop {
                let Some(more_entries) = bits.take(1) else { return };
                let (Some(_), Some(op), Some(name_len)) = (bits.take(1), bits.take(2), bits.take(4)) else { return };
                let mut name = String::with_capacity(name_len as usize);
                for _ in 0..name_len {
                    let Some(c) = bits.take(8) else { return };
                    name.push(char::from(c as u8));
                }
                let Some(position) = bits.take(8) else { return };
                let position = position as i32;

                let mut price = None;
                let mut size = None;
                let mut side = None;
                if op < 2 {
                    loop {
                        let (Some(id), Some(more_fields), Some(len)) = (bits.take(5), bits.take(1), bits.take(2)) else { return };
                        let (id, len) = if id == 31 {
                            let (Some(id), Some(len)) = (bits.take(8), bits.take(8)) else { return };
                            (id, len as usize)
                        } else {
                            (id, len as usize + 1)
                        };
                        let width = 8 * len;
                        // A field wider than a number this reads, or stating no
                        // width, is stepped over: the frame's other fields
                        // still stand. Ended here, every entry after it in the
                        // frame was lost, silently from the caller's side.
                        //
                        // The sign bit is part of every field and is consumed
                        // whatever the width says — a field stating a width of
                        // nothing still costs it. Skipped by the width alone,
                        // a stated nought left the walk one bit behind the
                        // wire, and every field and entry after it in the
                        // frame was read from shifted bits and handed over as
                        // a priced, sized level.
                        if width == 0 || width > 64 {
                            let Some(_) = bits.take(1) else { return };
                            let Some(()) = bits.skip(width.saturating_sub(1)) else { return };
                            if more_fields == 0 {
                                break;
                            }
                            continue;
                        }
                        let (Some(sign), Some(magnitude)) = (bits.take(1), bits.take(width - 1)) else { return };
                        let value = if sign == 1 { -(magnitude as i64) } else { magnitude as i64 };
                        // The side is the first priced or sized field's, as
                        // the reference client takes it.
                        match id >> 2 {
                            0 => { price = Some(value); side.get_or_insert(1); }
                            1 => { price = Some(value); side.get_or_insert(0); }
                            4 => { size = Some(value); side.get_or_insert(1); }
                            5 => { size = Some(value); side.get_or_insert(0); }
                            _ => {}
                        }
                        if more_fields == 0 { break; }
                    }
                }

                // A delete names its side in the operation and carries no
                // level; an insert or update is a price and a size, and one
                // stating either alone is not a quoted level.
                let level = match op {
                    2 | 3 => Some((2, if op == 2 { 1 } else { 0 }, 0.0, 0.0)),
                    _ => match (price, size, side) {
                        (Some(p), Some(s), Some(side)) => {
                            Some((op as i32, side, p as f64 * min_tick, s as f64 * counted_in))
                        }
                        _ => {
                            if price.is_some() || size.is_some() {
                                log::debug!(
                                    "book entry states {} alone; a level is a price and a size, so it is left out",
                                    if price.is_some() { "a price" } else { "a size" },
                                );
                            }
                            None
                        }
                    },
                };
                if let Some((operation, side, price, size)) = level {
                    // How deep the book is on this side before the entry is
                    // applied, which is what says whether a level was pushed
                    // out of the window by an insert.
                    let held = side.clamp(0, 1) as usize;
                    let was_deep = self.depth_books.iter()
                        .find(|(held_tag, _)| *held_tag == tag)
                        .map_or(0, |(_, sides)| sides[held].len());
                    for (req_id, is_smart, venue) in &subscribers {
                        // A level pushed out of the window by an insert goes
                        // away for the caller, and the venue says nothing
                        // about it: it is still in the book, one place deeper.
                        // Said nothing here either, the caller's book kept a
                        // row the venue has moved below what was asked for.
                        if let Some(rows) = self.asked_depth(*req_id)
                            && operation == 0
                            && position < rows
                            && was_deep >= rows as usize
                        {
                            // Named the way the row it withdraws was named: a
                            // withdrawal carrying no maker is handed to a
                            // caller on the callback that carries none, so the
                            // one row this client states itself arrived on a
                            // different callback from every row beside it.
                            let pushed_out = self.depth_books.iter()
                                .find(|(held_tag, _)| *held_tag == tag)
                                .and_then(|(_, sides)| sides[held].get(rows as usize - 1))
                                .map(|(maker, ..)| maker.clone())
                                .unwrap_or_default();
                            let named = match pushed_out.as_str() {
                                "" if *is_smart => venue.clone(),
                                stated => stated.to_string(),
                            };
                            shared.market.push_depth_update(DepthUpdate {
                                req_id: *req_id, position: rows - 1,
                                market_maker: named,
                                operation: 2, side, price: 0.0, size: 0.0,
                                is_smart_depth: *is_smart,
                            });
                        }
                        if !self.within_asked_depth(*req_id, position) { continue; }
                        // The venue's name for the maker, as it states it.
                        // Where it states none and the book is the aggregated
                        // one, the exchange stands in — which is what the
                        // aggregating reader does there and only there.
                        // Applied to an exchange-level book as well, a caller
                        // keying its book by maker was given a maker the venue
                        // never named, on every level of a book that quotes no
                        // makers at all.
                        let named = match name.trim() {
                            "" if *is_smart => venue.clone(),
                            stated => stated.to_string(),
                        };
                        shared.market.push_depth_update(DepthUpdate {
                            req_id: *req_id, position, market_maker: named,
                            operation, side, price, size, is_smart_depth: *is_smart,
                        });
                    }
                    // The book this client keeps, so it can answer for the
                    // place a withdrawal empties.
                    // Only for a stream this session holds. A frame already on
                    // its way when a book was withdrawn arrives under a number
                    // nothing here reads, and a book kept for it is state no
                    // later withdrawal can reach.
                    if subscribers.is_empty() {
                        if more_entries == 0 { break; }
                        continue;
                    }
                    let book = match self.depth_books.iter_mut()
                        .find(|(held_tag, _)| *held_tag == tag)
                    {
                        Some((_, sides)) => sides,
                        None => {
                            self.depth_books.push((tag, [Vec::new(), Vec::new()]));
                            let (_, sides) = self.depth_books.last_mut().expect("just pushed");
                            sides
                        }
                    };
                    let at = position.max(0) as usize;
                    let rows = &mut book[held];
                    match operation {
                        // A position past the end of the side is not a place in
                        // this book, and the reader this follows ignores such an
                        // insert rather than finding it a place. Put at the end
                        // instead, the row was somewhere the venue never said,
                        // and a later withdrawal inside the window pulled it up
                        // into the caller's book from there.
                        0 if at <= rows.len() => {
                            rows.insert(at, (name.trim().to_string(), price, size));
                            // A position is eight bits on this wire, so a level
                            // past the two hundred and fifty sixth cannot be
                            // named by any later update and is not a place the
                            // venue can speak about. Kept anyway, a side grew
                            // for as long as the stream lasted.
                            rows.truncate(256);
                        }
                        0 => log::debug!(
                            "a book insert names position {at}, past the {} levels this side \
                             holds; the book is left as it was",
                            rows.len(),
                        ),
                        1 => {
                            if let Some(row) = rows.get_mut(at) {
                                *row = (name.trim().to_string(), price, size);
                            }
                        }
                        _ => {
                            if at < rows.len() {
                                rows.remove(at);
                            }
                        }
                    }
                    // And the level that moved up into the place a withdrawal
                    // emptied. It arrives from the venue one place below what
                    // the caller asked for and is dropped as too deep, so
                    // without this the caller's book is a row short from the
                    // first withdrawal near the top and never refills.
                    if operation == 2 || operation == 3 {
                        for (req_id, is_smart, venue) in &subscribers {
                            let Some(rows_asked) = self.asked_depth(*req_id) else { continue };
                            if position >= rows_asked {
                                continue;
                            }
                            let pulled_up = self.depth_books.iter()
                                .find(|(held_tag, _)| *held_tag == tag)
                                .and_then(|(_, sides)| sides[held].get(rows_asked as usize - 1))
                                .cloned();
                            if let Some((maker, price, size)) = pulled_up {
                                // Named as the rows beside it are named, so it
                                // reaches the caller on the same callback.
                                let named = match maker.as_str() {
                                    "" if *is_smart => venue.clone(),
                                    stated => stated.to_string(),
                                };
                                shared.market.push_depth_update(DepthUpdate {
                                    req_id: *req_id, position: rows_asked - 1,
                                    market_maker: named,
                                    operation: 0, side, price, size,
                                    is_smart_depth: *is_smart,
                                });
                            }
                        }
                    }
                }
                if more_entries == 0 { break; }
            }
            if more_sections == 0 || bits.left() < 32 { return; }
        }
    }


    /// Every request this tag's levels belong to, as (req_id, is_smart, venue).
    ///
    /// The venue answers a second subscription on a contract and venue it is
    /// already streaming with the tag it is already using, so a level can
    /// belong to more than one request. Delivering to the first alone left
    /// every later caller with a book that never arrived and no word of why.
    fn depth_subscribers_of(&self, stag: u32) -> Vec<(u32, bool, String)> {
        self.depth_tag_to_req.iter()
            .filter(|(s, ..)| *s == stag)
            .map(|(_, r, sm, _, _, ex)| (*r, *sm, ex.clone()))
            .collect()
    }

    /// Look up a depth server_tag → (req_id, is_smart, min_tick, size_tick, venue).
    fn lookup_depth_stag(&self, stag: u32) -> Option<(u32, bool, f64, f64, String)> {
        self.depth_tag_to_req.iter()
            .find(|(s, ..)| *s == stag)
            .map(|(_, r, sm, mt, st, ex)| (*r, *sm, *mt, *st, ex.clone()))
    }


    /// Give this farm up, and the socket it was carried on with it.
    ///
    /// The connection goes here rather than at the reconnect that replaces it.
    /// A liveness timeout says the venue has stopped answering, not that the
    /// socket is closed, so one kept through the outage is a session this
    /// client still holds while it dials another — and on a hard error it is a
    /// descriptor held for as long as the outage lasts. The historical and
    /// calendar farms clear theirs on the same transition.
    pub(crate) fn handle_disconnect(
        &mut self,
        farm_conn: &mut Option<Connection>,
        context: &mut Context,
        event_tx: &Option<EventSink>,
        shared: &SharedState,
    ) {
        self.disconnected = true;
        *farm_conn = None;
        // Anything the replay had not reached goes back where the next
        // reconnect looks for it. A subscription that was sent is recorded
        // again as it goes out; one still waiting was never sent, so dropping
        // the queue here loses it — and the reconnect that follows rebuilds
        // the queue from the record, which would no longer name it.
        for (instrument, _con_id, symbol, exchange, sec_type, ltd, strike, right, mult, mode)
            in self.replay_queue.drain(..)
        {
            if self.md_resub_info.iter().all(|(id, ..)| *id != instrument) {
                self.md_resub_info.push((
                    instrument, symbol, exchange, sec_type, ltd, strike, right, mult, mode,
                ));
            }
        }
        self.replay_not_before = None;
        self.md_req_to_instrument.clear();
        self.instrument_md_reqs.clear();
        // Clear depth wire-state (server_tags become invalid after disconnect).
        // depth_resub_info is preserved for resubscription on reconnect.
        self.depth_subs.clear();
        self.depth_tag_to_req.clear();
        self.depth_fanout_map.clear();
        // And the books held against those numbers. The next connection
        // states every book again from the top, so a book kept here would be
        // answered for out of levels the venue has stopped standing behind.
        self.depth_books.clear();
        // The venue's numbers do not survive the connection that issued them,
        // and one left behind would read the next subscription's frames as the
        // tick the last one asked for. What the caller asked for is not one of
        // the venue's numbers and does survive: cleared with them, the rebuild
        // read an empty list, the extra series were never asked for again, and
        // the subscription went on looking healthy because the prices came
        // back. It ends where the caller ends it, which is the withdrawal.
        self.generic_tick_reqs.clear();
        self.generic_tick_tags.clear();
        self.rt_volume_totals.clear();
        // Keyed the same way, and left behind they are never reachable again:
        // what removes an entry looks it up by an id the reconnect has already
        // replaced, so nothing afterwards names the old one. Both are scanned
        // in full on a path that runs per acknowledgement and per withdrawal,
        // so a session that reconnects through a night grows its own latency.
        self.depth_fanout_exchange.clear();
        self.greeks_subs.clear();
        // Server tags are the venue's and start again with the connection, so
        // one already warned about would otherwise silence the warning for a
        // different tick that happened to be given the same number.
        self.quotes_for_no_one.clear();
        context.market.clear_server_tags();
        context.market.zero_all_quotes();
        // And the copy the caller reads, which is a different one: zeroing the
        // engine's alone left the pre-drop prices standing where
        // `poll_instrument_ticks` looks, so the notice below was followed by
        // the whole stale quote going out again as fresh ticks.
        shared.market.zero_all_quotes();
        // Not Event::Disconnected — that is the session going, and a rebuild
        // usually takes this back without one. But the venue says when this
        // connection breaks and a caller stands down on being told: the
        // quotes it can read do not go anywhere when the connection carrying
        // them does, so without this it goes on reading the last price before
        // the drop as though it were still a price.
        crate::engine::hot_loop::announce_venue_data(
            shared, event_tx, crate::bridge::VenueDataConnection::MarketData, false,
        );
    }

    /// Test-only: set disconnected without clearing state or emitting events.
    pub fn handle_disconnect_for_test(&mut self) {
        self.disconnected = true;
    }

    /// The L1 subscriptions to re-issue on a new farm connection, drained from
    /// the record that survives a disconnect. `handle_disconnect` clears
    /// `instrument_md_reqs`, so selecting from that list re-subscribes nothing
    /// and the reconnect silently delivers no market data. Skips
    /// instruments whose slot was reclaimed while the farm was down.
    fn take_resub_targets(
        &mut self,
        market: &crate::engine::market_state::MarketState,
    ) -> Vec<MdResubTarget> {
        std::mem::take(&mut self.md_resub_info)
            .into_iter()
            .filter_map(|(id, sym, exch, st, ltd, strike, right, mult, mode)| {
                market.con_id(id)
                    .map(|con_id| (id, con_id, sym, exch, st, ltd, strike, right, mult, mode))
            })
            .collect()
    }

    pub(crate) fn reconnect(
        &mut self,
        conn: Connection,
        farm_conn: &mut Option<Connection>,
        context: &mut Context,
        hb: &mut HeartbeatState,
        replay: ReplayPacing,
        shared: &SharedState,
    ) {
        *farm_conn = Some(conn);
        self.disconnected = false;
        hb.last_farm_sent = Instant::now();
        hb.last_farm_recv = Instant::now();
        hb.pending_farm_test = None;

        // Re-issue the L1 subscriptions from `md_resub_info`, which survives a
        // disconnect for exactly this purpose — the same shape the depth path
        // below uses. Driving this off `instrument_md_reqs` re-subscribed
        // nothing, because `handle_disconnect` clears that list before the
        // reconnect runs.
        let active = self.take_resub_targets(&context.market);
        self.md_req_to_instrument.clear();
        self.instrument_md_reqs.clear();
        // The headline subscriptions belonged to the dead session and are not
        // part of what the venue pushes back. Left alone they went quiet for
        // good, with the connection reporting healthy the whole time.
        for (instrument, req_id, providers, con_id, sec_type) in std::mem::take(&mut self.news_subscriptions) {
            self.send_news_subscribe(con_id, instrument, &sec_type, &providers, req_id, farm_conn, hb);
        }
        // Queued rather than sent here. The first burst goes out on this pass
        // and the rest on the passes that follow, so the pacing costs the
        // venue nothing and the engine nothing.
        self.replay_queue = active.into_iter().collect();
        self.replay_not_before = None;
        self.drive_replay(replay, farm_conn, hb);

        // Re-subscribe depth subscriptions (depth_resub_info survived disconnect)
        let depth_params = std::mem::take(&mut self.depth_resub_info);
        let depth_count = depth_params.len();
        for (req_id, con_id, exchange, listed_on, sec_type, num_rows, is_smart_depth)
            in depth_params
        {
            // A book given up on for running away is refused until it is asked
            // for again, and this is asking for it again: the venue restarts it
            // from the top on the new connection, so what was held against the
            // old one goes with the old one. Without this the reconnect brings
            // the book back on the wire and every update is discarded at the
            // guard, silently and for the life of the session.
            shared.market.purge_depth_updates(req_id);
            // The venue restarts the book from the top on the new connection,
            // and every level of that restart is delivered as a level the
            // caller does not already hold. Told nothing, a caller keyed on
            // position refreshed the levels the new book reaches and kept
            // every level the old one held below them, for the life of the
            // session; a caller that inserts got the new book stacked on top
            // of the old. This says to empty it first, which is the only way a
            // book that shrank can shrink on the caller's side -- nothing on
            // this wire states a level has gone.
            //
            // The queue purge above clears what is queued here, not what the
            // caller holds.
            shared.reference.push_historical_error(
                req_id,
                crate::error_codes::DEPTH_BOOK_RESET,
                "Market depth data has been RESET. Please empty deep book contents \
                 before applying any new entries.".to_string(),
            );
            self.send_depth_subscribe(
                req_id, con_id, &exchange, &listed_on, &sec_type, num_rows, is_smart_depth,
                farm_conn, hb, shared,
            );
        }

        log::info!(
            "Farm reconnected, re-subscribing {} instruments + {} depth",
            self.instrument_md_reqs.len() + self.replay_queue.len(), depth_count,
        );
    }

    /// Put back as much of the reconnect's book as the pacing allows.
    ///
    /// Called on every pass of the loop. Returns immediately when there is
    /// nothing waiting or the pace has not elapsed, so the cost of an idle
    /// engine is one comparison.
    pub(crate) fn drive_replay(
        &mut self,
        replay: crate::engine::hot_loop::ReplayPacing,
        farm_conn: &mut Option<Connection>,
        hb: &mut crate::engine::hot_loop::HeartbeatState,
    ) {
        if self.replay_queue.is_empty() || self.disconnected {
            return;
        }
        let now = Instant::now();
        if let Some(not_before) = self.replay_not_before
            && now < not_before
        {
            return;
        }
        let burst = if replay.burst == 0 { self.replay_queue.len() } else { replay.burst };
        for _ in 0..burst {
            let Some((instrument, con_id, sym, exch, st, ltd, strike, right, mult, mode)) =
                self.replay_queue.pop_front()
            else {
                break;
            };
            self.send_mktdata_subscribe(
                con_id, &sym, &exch, &st, &ltd, strike, &right, &mult, instrument, mode,
                // Nothing replayed here is a snapshot: a one-shot is never
                // written down for replay in the first place.
                false, farm_conn, hb,
            );
        }
        self.replay_not_before =
            (!self.replay_queue.is_empty()).then(|| now + replay.pace);
    }

    fn handle_generic_tick(&mut self, msg: &[u8], context: &mut Context, shared: &SharedState, event_tx: &Option<EventSink>) {
        let body = match find_body_after_tag(msg, b"35=G\x01") {
            Some(b) => b,
            None => return,
        };

        // Which tick a record carries is not on it. The venue numbers a
        // generic tick apart from the prices and says what it carries once,
        // when it takes the subscription on, so what was asked for under that
        // number is the only thing that says what these bytes are — and, since
        // a tick's payload states its length in a way particular to that tick,
        // the only thing that says where the record ends.
        // The contract each record belongs to is resolved here, while the
        // record of what was asked for is still in hand, so that reading a
        // series which keeps state of its own does not borrow it a second time.
        let mut delivered: Vec<(u32, InstrumentId, &[u8])> = Vec::new();
        {
            let asked = &self.generic_tick_tags;
            read_generic_ticks(
                body,
                |server_tag| {
                    asked.iter().find(|(tag, ..)| *tag == server_tag).map(|(_, tick, _)| *tick)
                },
                |tick, record| {
                    if let Some((_, _, instrument)) =
                        asked.iter().find(|(tag, ..)| *tag == record.server_tag)
                    {
                        delivered.push((tick, *instrument, record.payload));
                    }
                },
            );
        }

        for (tick, instrument, payload) in delivered {
            match tick {
                GREEKS_REQUEST_TYPE => {
                    if let Some(mut comp) = decode_greeks(payload) {
                        comp.instrument = instrument;
                        shared.market.push_option_computation(comp);
                        emit(event_tx, Event::OptionComputation(comp));
                    }
                }
                // The same model, worked out as the contract closed rather
                // than as it stands. Stated in the same shape, down to the
                // flags that say which fields it carries, so it is read by the
                // same reader — and kept apart from the standing one, which it
                // would otherwise overwrite with a figure from yesterday.
                CLOSING_GREEKS_REQUEST_TYPE => {
                    if let Some(mut comp) = decode_greeks(payload) {
                        comp.instrument = instrument;
                        shared.market.note_closing_option_model(comp);
                    }
                }
                BBO_EXCHANGE_MAP_REQUEST_TYPE => {
                    // The venues the exchange masks are written over, one per
                    // entry: the bit it answers to, the single letter it is
                    // shown as, and its name, in that order.
                    //
                    // Read as a name and a letter, with the bit taken from
                    // where the entry sat in the list, every letter came out
                    // as the letter and the name run together — a bid on two
                    // venues rendered `J/EDGEAY/BYX` — and a list that numbers
                    // its own bits was renumbered by position.
                    let Some(stated) = length_prefixed_text(payload) else {
                        // An empty payload states no length at all, which is
                        // the venue naming no venues rather than a message
                        // that lost its tail — reported as the same thing, it
                        // sends the next reader looking for a truncation.
                        if payload.is_empty() {
                            log::debug!("the exchange map named no venues");
                        } else {
                            log::warn!(
                                "the exchange map states a length its {} bytes do not carry",
                                payload.len(),
                            );
                        }
                        continue;
                    };
                    let venues: Vec<crate::types::SmartComponent> = stated
                        .split(';')
                        .filter_map(|entry| {
                            // Three parts or it is not an entry. A shorter one
                            // read as a venue puts whatever it does carry in
                            // the place of a letter.
                            let mut parts = entry.trim().split('/');
                            let bit = parts.next()?.trim().parse::<i32>().ok()?;
                            let letter = parts.next()?.trim();
                            let name = parts.next()?.trim();
                            if parts.next().is_some() {
                                return None;
                            }
                            Some(crate::types::SmartComponent {
                                bit_number: bit,
                                exchange: name.to_string(),
                                exchange_letter: letter.to_string(),
                            })
                        })
                        .collect();
                    if !venues.is_empty() {
                        log::info!("the server names {} venues for the exchange mask", venues.len());
                        shared.reference.set_smart_components(venues);
                        shared.reference.note_smart_components_provisional(false);
                    }
                }
                TRADING_STATUS_REQUEST_TYPE => {
                    let Some(status) =
                        crate::protocol::trading_status::parse_trading_status(payload)
                    else {
                        continue;
                    };
                    // Published with the quote it belongs to, so a caller
                    // reads the halt against the prices it applies to rather
                    // than a moment either side of them.
                    //
                    // The reason as well as the fact. Read off the mask, not
                    // off the one status the venue names beside it: the name
                    // can say nothing while the mask says trading has stopped,
                    // and a live listing in the premarket names a regulatory
                    // halt with a mask of nothing. A regulator's halt outranks
                    // a volatility pause where both are set, because it is the
                    // one that does not lift on a clock.
                    let quote = context.quote_mut(instrument);
                    quote.halted = if !status.is_halted() {
                        0
                    } else if status.mask
                        & crate::protocol::trading_status::TradingStatus::RegulatoryHalt.mask()
                        != 0
                    {
                        1
                    } else {
                        2
                    };
                    // Kept beside the quote rather than on it: that record is
                    // two cache lines exactly, on every slot in the table.
                    shared.market.note_short_sale_restriction(
                        instrument, status.short_sales_restricted(),
                    );
                    shared.market.push_quote(instrument, context.quote(instrument));
                    emit(event_tx, Event::Tick(instrument));
                }
                NEWS_REQUEST_TYPE => self.deliver_news(instrument, payload, shared, event_tx),
                // The running volume states totals, and what a caller is owed
                // is the trade between two of them — so it is read here, where
                // the totals this contract last stated are held.
                // Everything that traded is one series and what traded on a
                // trade report is another; the venue states both as totals in
                // the same shape and the reference client publishes each under
                // its own number.
                // The mark the venue keeps for a contract, and the slow one
                // beside it. Both arrive as a record of the venue's own
                // fields rather than as a struct: the price is the field
                // numbered two, and a word of flags numbered thirteen says
                // whether it stands.
                // The odd lot: the two prices nobody has to deal in round
                // lots at, their sizes, and where each is quoted.
                787 => self.deliver_odd_lot(instrument, payload, context, shared),
                221 => self.deliver_mark(instrument, 37, payload, context, shared),
                220 => self.deliver_mark(instrument, 78, payload, context, shared),
                619 => self.deliver_mark(instrument, 79, payload, context, shared),
                233 => self.deliver_running_volume(instrument, 48, payload, shared),
                375 => self.deliver_running_volume(instrument, 77, payload, shared),
                // What the venue holds about the company behind a contract
                // rather than about its quote, and what it holds about the
                // terms of dealing in it. Every one of these is text the venue
                // writes as its own named fields: the two analyst ratings and
                // what institutions and insiders hold beside the shares on
                // issue; the ratios it keeps a history of; how a contract
                // scores against the principles an account can screen on and
                // whether it passes a religious screen; what margin it takes
                // and what a fund will take part in; the technical readings,
                // the fund-family figures and the two news-derived scores it
                // buys in from elsewhere; the lens it publishes over a
                // company's accounts; and the price it holds a contract
                // against for reference.
                386 | 434 | 454 | 505 | 548 | 628 | 631 | 633 | 669 | 678 | 699 | 700 | 703
                | 705 | 726 | 750 | 752 => {
                    self.deliver_company_data(instrument, tick, payload, context, shared)
                }
                other => {
                    if !deliver_series(other, payload, instrument, shared) {
                        log::debug!("Generic tick {other} arrives and nothing here reads it");
                    }
                }
            }
        }
    }

    /// What the venue states about a contract's company and its terms, as its
    /// own text.
    ///
    /// Seventeen series carry it and every one states it the same way:
    /// runs of `KEY=VALUE` separated by semicolons or by line breaks, which
    /// are held under the contract for a caller to read. What differs between
    /// them is only what stands in front of the text, and which alphabet the
    /// bytes behind it are read in.
    ///
    /// Held against the venue's id for the contract rather than against the
    /// slot it is watched in, because it is a fact about the contract and
    /// outlives the subscription that fetched it.
    fn deliver_company_data(
        &mut self,
        instrument: InstrumentId,
        series: u32,
        payload: &[u8],
        context: &Context,
        shared: &SharedState,
    ) {
        let Some(con_id) = context.market.con_id(instrument).filter(|id| *id > 0) else {
            return;
        };
        let Some(text) = company_text(series, payload) else {
            log::debug!(
                "company data on series {series} does not carry the text it \
                 frames; nothing is published",
            );
            return;
        };
        let text: &[u8] = &text;
        let text = if company_text_is_one_byte(series) {
            // An alphabet of one byte to the character: what the venue states
            // for these series, and every byte of it stands for something, so
            // there is nothing here that can fail to be read.
            std::borrow::Cow::Owned(text.iter().map(|b| char::from(*b)).collect::<String>())
        } else {
            // Read as text or not at all. Read with the bytes it could not make
            // sense of replaced, a record carrying one such byte was handed to a
            // caller with a replacement character in it — a character the venue
            // never sent, in a value a caller reads as the venue's own.
            let Ok(text) = std::str::from_utf8(text) else {
                log::debug!("company data on series {series} is not text; nothing is published");
                return;
            };
            std::borrow::Cow::Borrowed(text)
        };
        let pairs = crate::control::fundamental::stated_pairs(&text);
        // A record stating no pair states nothing. Written down anyway, a
        // caller reading the series could not tell a contract the venue holds
        // nothing for from one it has not answered yet.
        if pairs.is_empty() {
            return;
        }
        shared.reference.note_company_data(con_id as u32, series, pairs);
    }

    /// The odd lot, off a record of the venue's own fields.
    ///
    /// The two prices are the fields numbered nought and one, their sizes four
    /// and five, and where each is quoted sixteen and seventeen — the same
    /// bits over the same venue list the ordinary quote's exchanges are read
    /// from.
    ///
    /// A price with no size behind it is not a price the venue is standing
    /// behind: it states one and withdraws it by leaving the size at nothing,
    /// so a side is handed over only where its size says there is something to
    /// deal in.
    fn deliver_odd_lot(
        &mut self,
        instrument: InstrumentId,
        payload: &[u8],
        context: &Context,
        shared: &SharedState,
    ) {
        use crate::protocol::tick_decoder::{BitReader, decode_record};
        let Some(fields) = decode_record(&mut BitReader::new(payload, 0)) else { return };
        let stated = |id: u64| {
            fields.iter().find(|f| f.id == id).filter(|f| f.decimal_shift == 0)
        };
        let increment = context.market.min_tick_scaled(instrument);
        // Counted in whole ones where the venue stated no increment, as
        // every other reader of this figure does. Multiplied in raw, a size
        // the venue stated reached the caller as nought — a reading nobody
        // sent — whenever the increment had not arrived yet or the venue
        // stated none.
        let counted_in = match context.market.size_tick(instrument) {
            stated if stated > 0.0 => stated,
            _ => 1.0,
        };
        let say = |tick_type: i32, value: crate::types::SeriesValue| {
            shared.market.push_series_tick(crate::types::SeriesTick {
                instrument, tick_type, value,
            });
        };
        // (price, size, where) for each side, and the numbers each is read
        // under.
        for (price_id, size_id, venue_id, price_tick, size_tick, venue_tick) in
            [(0u64, 4u64, 16u64, 105, 107, 109), (1, 5, 17, 106, 108, 110)]
        {
            let size = stated(size_id).map(|f| f.magnitude).filter(|n| *n != 0);
            if let Some(size) = size {
                say(size_tick, crate::types::SeriesValue::Size(size as f64 * counted_in));
            }
            if let Some(price) = stated(price_id)
                && size.is_some()
                && increment > 0
                && let Some(scaled) = price.magnitude.checked_mul(increment)
            {
                say(
                    price_tick,
                    crate::types::SeriesValue::Price(
                        scaled as f64 / crate::types::PRICE_SCALE as f64,
                    ),
                );
            }
            // Where it is quoted, as the letters the venue's own list gives
            // the bits. A list it has not sent yet renders none, and a side
            // with nothing to show for its bits states nothing rather than an
            // empty venue.
            if let Some(mask) = stated(venue_id).map(|f| f.magnitude).filter(|m| *m > 0) {
                let letters = crate::client_core::render_exchange_mask(mask, shared);
                if !letters.is_empty() {
                    say(venue_tick, crate::types::SeriesValue::Text(letters));
                }
            }
        }
    }

    /// The mark the venue keeps for a contract, off a record of its own
    /// fields.
    ///
    /// The price is the field numbered two, counted in tenths of the
    /// contract's minimum increment rather than whole ones. Measured against
    /// the mark the venue also states as a plain number, on three contracts
    /// whose increments differ by two orders of magnitude: a share on a
    /// hundredth, gold on a tenth and the index future on a quarter all read
    /// back the same price only on tenths. Taken as whole increments every
    /// mark this client published was ten times the price.
    ///
    /// A word of flags numbered thirteen carries, on the fifth bit up, the
    /// venue saying the price does not stand; absent, nothing is said against
    /// it. A negative mark is the venue's to state and is passed on as stated.
    fn deliver_mark(
        &mut self,
        instrument: InstrumentId,
        tick_type: i32,
        payload: &[u8],
        context: &Context,
        shared: &SharedState,
    ) {
        use crate::protocol::tick_decoder::{BitReader, decode_record};
        const MARK_PRICE: u64 = 2;
        const FLAGS: u64 = 13;
        const DOES_NOT_STAND: i64 = 16;
        /// What the price field counts in, against the contract's increment.
        const TENTHS_OF_AN_INCREMENT: f64 = 10.0;

        let Some(fields) = decode_record(&mut BitReader::new(payload, 0)) else { return };
        // A word of flags that never arrived says nothing against the price.
        // Read as nought it would say the opposite of nothing, and a mark the
        // venue never questioned would still be published — the same either
        // way here, but not once another bit of it is read.
        if let Some(flags) = fields.iter().find(|f| f.id == FLAGS)
            && flags.magnitude & DOES_NOT_STAND != 0
        {
            return;
        }
        let Some(price) = fields.iter().find(|f| f.id == MARK_PRICE) else { return };
        // A field stating how far its decimal point moves is not counted in
        // the contract's own increments, and this path has no other scale to
        // put it on; left out rather than published as a number nobody sent.
        if price.decimal_shift != 0 {
            return;
        }
        let increment = context.market.min_tick_scaled(instrument);
        let Some(scaled) = price.magnitude.checked_mul(increment).filter(|_| increment > 0) else {
            return;
        };
        shared.market.push_series_tick(crate::types::SeriesTick {
            instrument,
            tick_type,
            value: crate::types::SeriesValue::Price(
                scaled as f64 / (crate::types::PRICE_SCALE as f64 * TENTHS_OF_AN_INCREMENT),
            ),
        });
    }

    /// One trade off the running-volume series, as the reference client
    /// states it.
    ///
    /// The venue states running totals — what has traded by value, by shares
    /// and by count — and the reference client publishes the difference
    /// between two of them as one semicolon-separated string. Until a first
    /// pair has been seen there is no difference to state, so the first
    /// reading sets the baseline and says nothing; a caller is not handed a
    /// trade of the whole day's volume for having just subscribed.
    fn deliver_running_volume(
        &mut self,
        instrument: InstrumentId,
        tick_type: i32,
        payload: &[u8],
        shared: &SharedState,
    ) {
        let (Some(value), Some(shares), Some(trades)) = (
            series_f64(payload, 0),
            series_i64(payload, 8),
            series_i32(payload, 16),
        ) else {
            return;
        };
        // The venue says it holds no total by stating the largest the type
        // carries, which is not a share count.
        if shares == i64::MAX {
            return;
        }
        let Some(&(was_value, was_shares, was_trades)) =
            self.rt_volume_totals.get(&(instrument, tick_type))
        else {
            self.rt_volume_totals.insert((instrument, tick_type), (value, shares, trades));
            return;
        };
        self.rt_volume_totals.insert((instrument, tick_type), (value, shares, trades));

        let moved_shares = shares - was_shares;
        let moved_value = value - was_value;
        let moved_trades = trades - was_trades;
        // A trade of no shares has no price: the venue divides by the share
        // difference and states nothing where it is nought, rather than
        // standing in the last price it knew.
        let last_price = if moved_shares == 0 {
            String::new()
        } else {
            rt_volume_price(moved_value / moved_shares as f64)
        };
        // The average is struck over everything traded, not over this trade,
        // so it survives a trade that moved no shares.
        let vwap = if shares == 0 {
            String::new()
        } else {
            rt_volume_price(value / shares as f64)
        };
        let said = format!(
            "{last_price};{};{};{};{vwap};{}",
            rt_volume_quantity(moved_shares),
            shared.market.venue_time_millis(),
            rt_volume_quantity(shares),
            moved_trades < 2,
        );
        shared.market.push_series_tick(crate::types::SeriesTick {
            instrument,
            tick_type,
            value: crate::types::SeriesValue::Text(said),
        });
    }

    /// The articles in one news tick.
    fn deliver_news(
        &self,
        instrument: InstrumentId,
        body: &[u8],
        shared: &SharedState,
        event_tx: &Option<EventSink>,
    ) {
        if body.len() < 4 { return; }
        let batch_count = u32::from_be_bytes([body[0], body[1], body[2], body[3]]) as usize;
        let mut pos = 4;

        for _ in 0..batch_count {
            if pos + 4 > body.len() { break; }
            let prov_len = u32::from_be_bytes([body[pos], body[pos+1], body[pos+2], body[pos+3]]) as usize;
            pos += 4;
            if pos + prov_len > body.len() { break; }
            let provider = String::from_utf8_lossy(&body[pos..pos+prov_len]).to_string();
            pos += prov_len;

            if pos + 4 > body.len() { break; }
            pos += 4;

            if pos + 2 > body.len() { break; }
            let aid_len = u16::from_be_bytes([body[pos], body[pos+1]]) as usize;
            pos += 2;
            if pos + aid_len > body.len() { break; }
            let article_id = String::from_utf8_lossy(&body[pos..pos+aid_len]).to_string();
            pos += aid_len;

            if pos + 8 > body.len() { break; }
            pos += 4;
            // Seconds on the wire, and the reference client hands a caller
            // milliseconds: it builds a date out of this and passes what that
            // date reads as. Handed on as it arrived, every stamp a caller
            // read was a thousandth of the moment it meant.
            let stated_secs =
                u32::from_be_bytes([body[pos], body[pos+1], body[pos+2], body[pos+3]]) as u64;
            let timestamp = stated_secs.saturating_mul(1_000);
            pos += 4;

            if pos + 4 > body.len() { break; }
            let hl_len = u32::from_be_bytes([body[pos], body[pos+1], body[pos+2], body[pos+3]]) as usize;
            pos += 4;
            if pos + hl_len > body.len() { break; }
            let raw_headline = String::from_utf8_lossy(&body[pos..pos+hl_len]).to_string();
            pos += hl_len;

            let headline = if raw_headline.starts_with('{') {
                match raw_headline.find('}') {
                    Some(i) => raw_headline[i+1..].to_string(),
                    None => raw_headline,
                }
            } else {
                raw_headline
            };
            // With the characters the venue escaped put back, the same as on a
            // headline out of the archive: it writes anything outside plain
            // ASCII as `&#xNN;`, and the reference client reads those back
            // here too.
            let headline = crate::control::news::unescape_venue_characters(&headline);

            let news = crate::types::TickNews {
                instrument,
                provider_code: provider,
                article_id,
                headline,
                timestamp,
            };
            shared.market.push_tick_news(news.clone());
            emit(event_tx, Event::News(news));
        }
    }
}

#[cfg(test)]
pub(crate) mod tests;
