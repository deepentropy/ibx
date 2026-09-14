//! Corporate actions on a contract — the splits and dividends that make a
//! historical price series adjusted rather than raw.
//!
//! A caller asks once for a date range and the venue answers with every action
//! in it; the subscription is withdrawn by the id it was asked under. Without
//! this a price series crosses a split unadjusted, which is not a missing
//! feature but a wrong number, and nothing in the series says so.

/// What a corporate action did to the contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdjustmentKind {
    /// A dividend paid in cash.
    CashDividend,
    /// A dividend paid in shares.
    StockDividend,
    /// A share split.
    Split,
    /// A business spun out as its own listing.
    SpinOff,
    /// A right to buy new shares.
    RightsOffer,
    /// A future rolling into the next month.
    FutureRollover,
}

impl AdjustmentKind {
    /// The venue's two-letter name for the action, which is the element it
    /// arrives under.
    pub fn code(self) -> &'static str {
        match self {
            Self::CashDividend => "CD",
            Self::StockDividend => "SD",
            Self::Split => "SS",
            Self::SpinOff => "SO",
            Self::RightsOffer => "RO",
            Self::FutureRollover => "FR",
        }
    }

    /// Read the kind from the code the venue states it as.
    pub fn from_code(code: &str) -> Option<Self> {
        Some(match code {
            "CD" => Self::CashDividend,
            "SD" => Self::StockDividend,
            "SS" => Self::Split,
            "SO" => Self::SpinOff,
            "RO" => Self::RightsOffer,
            "FR" => Self::FutureRollover,
            _ => return None,
        })
    }

    /// Whether an action of this kind moves the scale a bar is priced on.
    ///
    /// Three of the six do. A cash dividend is a payment out of the price
    /// rather than a restatement of it, a rights offer moves what a holder
    /// paid rather than what the share is quoted at, and a rollover carries no
    /// value to move anything by.
    pub fn moves_the_scale(self) -> bool {
        matches!(self, Self::StockDividend | Self::Split | Self::SpinOff)
    }

    /// The factor an action of this kind states, from the value it carries.
    ///
    /// A split states its ratio: ten for one states ten. A spin-off states the
    /// reciprocal of one, so it is inverted here and every other kind is taken
    /// as it stands.
    pub fn factor(self, value: f64) -> Option<f64> {
        // Finite first, because every comparison with a NaN is false: tested
        // only for being positive, a NaN passes as a factor and every price it
        // touches becomes one, which is a series of holes handed back as an
        // adjusted one.
        if !value.is_finite() || value <= 0.0 {
            return None;
        }
        let factor = match self {
            Self::SpinOff => 1.0 / value,
            _ => value,
        };
        factor.is_finite().then_some(factor).filter(|f| *f > 0.0)
    }
}

/// One corporate action.
///
/// The fields an action does not state are left empty rather than invented: a
/// split states no record date, and saying it did would be a date nobody set.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Adjustment {
    /// What kind of action it is.
    pub kind: Option<AdjustmentKind>,
    /// When it takes effect.
    pub date: String,
    /// What the venue states for it.
    pub value: String,
    /// What currency that is in.
    pub currency: String,
    /// When it was announced.
    pub announce_date: String,
    /// The day the holder of record is fixed.
    pub record_date: String,
    /// The day it is paid.
    pub pay_date: String,
    /// Whether a dividend was the regular one or a special.
    pub payment_type: String,
    /// What a dividend was paid out of — income, capital gain, or unstated.
    pub distribution_type: String,
}

/// What a caller asks for.
#[derive(Debug, Clone)]
pub struct AdjustmentRequest {
    /// The name this client gave the query, which the answer echoes.
    pub query_id: String,
    /// The venue's id for the contract.
    pub con_id: u32,
    /// What kind of contract it is.
    pub sec_type: String,
    /// Which venue.
    pub exchange: String,
    /// Both dates as the venue states them back, so a caller can hand back what
    /// it was given.
    pub start_date: String,
    /// Its end.
    pub end_date: String,
}

/// Ask for every corporate action on a contract in a date range.
///
/// The dividend request type is stated as `T`, which is the whole of them. The
/// venue names two narrower kinds beside it; nothing here asks for a subset, so
/// nothing here sends one.
pub fn build_adjustments_request_xml(req: &AdjustmentRequest) -> String {
    format!(
        "<ListOfQueries>\
         <ConAdjQuery>\
         <id>{id}</id>\
         <contractID>{con_id}</contractID>\
         <exchange>{exchange}</exchange>\
         <secType>{sec_type}</secType>\
         <startDate>{start}</startDate>\
         <endDate>{end}</endDate>\
         <divRequestType>T</divRequestType>\
         </ConAdjQuery>\
         </ListOfQueries>",
        id = req.query_id,
        con_id = req.con_id,
        exchange = req.exchange,
        sec_type = req.sec_type,
        start = req.start_date,
        end = req.end_date,
    )
}

/// The contract the actions belong to, echoed back beside them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AdjustedContract {
    /// The venue's id for the contract.
    pub con_id: String,
    /// Its ticker.
    pub symbol: String,
    /// Which venue.
    pub exchange: String,
}

/// The query id the venue echoes back on a reply.
///
/// A reply names the contract it is about, which is enough to file it and not
/// enough to know whose question it answers. Two questions about one contract
/// over different ranges are answered by two replies that name the same
/// contract, so the id the venue echoes is what tells them apart. Without it a
/// late answer to the first satisfies the second, and a series comes back
/// adjusted by the actions of a range nobody asked for.
pub fn parse_response_query_id(xml: &str) -> Option<String> {
    crate::control::xml::tag(xml, "id").map(|s| s.to_string())
}

/// Every corporate action in a reply, and the contract they belong to.
///
/// The venue answers a name on its own line and the rows under it, until the
/// next name: the query comes back echoed as XML and the actions arrive beside
/// it as text. An action stating fewer values than its kind names is kept with
/// the rest empty rather than dropped, because a partial answer is still the
/// venue's answer, and discarding it would leave a series looking unadjusted
/// for a reason nobody could see.
pub fn parse_adjustments(body: &str) -> (AdjustedContract, Vec<Adjustment>) {
    let mut contract = AdjustedContract::default();
    let mut out = Vec::new();
    // A name on its own line, then the records under it until the next name.
    // The reply carries no markup around them: the query is echoed as XML and
    // the actions arrive beside it as text, which is what a live session was
    // answered with.
    let mut under: Option<&str> = None;
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if !line.contains(',') {
            under = Some(line);
            continue;
        }
        let values: Vec<&str> = line.split(',').map(str::trim).collect();
        match under {
            // The contract states its own id first, then what the row is for.
            Some("conc") => contract.con_id = values[0].to_string(),
            Some("consym") => contract.symbol = values.last().unwrap_or(&"").to_string(),
            // Its id, the venue it is listed on, and the date that listing began.
            Some("conexch") => {
                if let Some(exchange) = values.get(1) {
                    contract.exchange = (*exchange).to_string();
                }
            }
            Some(code) => {
                // A name this client does not know is kept with no kind rather
                // than dropped. Dropped, it is indistinguishable from a contract
                // that had no such action — and if the venue ever names a kind
                // that moves the scale, a series missing it comes back looking
                // adjusted. Kept, whoever scales can refuse to guess.
                let kind = AdjustmentKind::from_code(code);
                let mut a = Adjustment { kind, ..Default::default() };
                // Stated in one fixed order, so a value is read by where it sits.
                for (i, v) in values.iter().enumerate() {
                    let v = (*v).to_string();
                    match i {
                        0 => a.date = v,
                        1 => a.value = v,
                        2 => a.currency = v,
                        3 => a.announce_date = v,
                        4 => a.record_date = v,
                        5 => a.pay_date = v,
                        6 => a.payment_type = v,
                        7 => a.distribution_type = v,
                        _ => {}
                    }
                }
                out.push(a);
            }
            None => {}
        }
    }
    out.sort_by(|a, b| a.date.cmp(&b.date));
    // Every row the venue stated is kept. Two that look alike are not collapsed:
    // deciding that two actions are one needs an identity the venue does not
    // state, and the fields that would tell a regular dividend from a special
    // one on the same day for the same amount are exactly the fields such a
    // reading throws away. What the venue said twice, it said twice.
    (contract, out)
}

/// What a price before `date` must be multiplied by to sit on the same scale
/// as prices after every action in `actions`, or what stopped it stating one.
///
/// An action moves the scale for a price dated before it, and leaves a price
/// dated on or after it alone. Three kinds move it: a split, a stock dividend
/// and a spin-off. Each states a factor, and a price from before it is divided
/// by that factor, which is what this returns the reciprocal product of.
///
/// A split ten for one states ten, and a price from before that day is a tenth
/// of what it reads once the split has happened. Established against a contract
/// that split ten for one on a stated day, where the close before it was
/// 1208.88 and the close after was 121.79: dividing the first by the stated ten
/// gives 120.89, which is the same scale as the second.
///
/// The three kinds that do not move it are as deliberate as the three that do.
/// A cash dividend is a payment out of the price rather than a restatement of
/// it. A rights offer moves what a holder paid, not what the share is quoted
/// at. A rollover states no value at all.
///
/// An action that moves the scale and does not say by how much stops this. It
/// could be skipped, and skipping it leaves the price it should have moved
/// exactly as the venue served it — a raw price handed back under the name of
/// an adjusted one, which is the wrong number this module exists to remove
/// arriving by its own back door.
///
/// Volume runs the other way: the shares that traded before a ten-for-one split
/// count for ten times as many after it, so a caller scaling volume multiplies
/// by the reciprocal of this. [`scale_volume_before`] states that, so neither
/// caller has to remember which way round it goes.
pub fn scale_before(date: &str, actions: &[Adjustment]) -> Result<f64, String> {
    // The day given is compared against the days the actions state, so it has
    // to be one. A stamp in seconds opens with eight digits and compares below
    // every date there is, so a price from after a split would be scaled by it
    // — and the caller reaching this helper directly, rather than through
    // `scale_bars`, had nothing checking that for them.
    let Some(date) = day_of(date, "") else {
        return Err(format!(
            "{date:?} is not a day a price can be placed before or after, so no scale \
             can be stated for it",
        ));
    };
    let date = date.as_str();
    let mut factor: f64 = 1.0;
    for a in actions {
        let Some(kind) = a.kind else {
            // An action the venue named and this client cannot classify. It
            // may be one that moves the scale, and a series scaled without it
            // is a raw price wearing an adjusted one's name.
            return Err(format!(
                "this contract states an action dated {} that this client cannot \
                 classify, and an action it cannot name is one it cannot say moves \
                 nothing", a.date,
            ));
        };
        if !kind.moves_the_scale() {
            continue;
        }
        // A day this cannot read is the same failure as a factor it cannot
        // read, arriving through the other field. An empty one compares before
        // every date there is, so the action reads as already past and is
        // skipped — leaving the price it should have moved exactly as the venue
        // served it, under the name of an adjusted one.
        let Some(acted) = day_of(&a.date, "") else {
            return Err(format!(
                "the {} in this contract's actions is dated {:?}, which is not a day a \
                 price can be placed before or after. Adjusting around it would hand \
                 back the price the venue served under the name of an adjusted one",
                kind.code(), a.date,
            ));
        };
        // Compared as the days they are, not as the strings they arrived in. A
        // date carrying anything after its eight digits sorts by that tail, and
        // an action would move the bars either side of the wrong day.
        if acted.as_str() <= date {
            continue;
        }
        let stated = a.value.parse::<f64>().ok().and_then(|v| kind.factor(v));
        let Some(f) = stated else {
            return Err(format!(
                "the {} of {} states {:?}, which is not a factor a price can be put on \
                 the scale of. Adjusting around it would hand back the price the venue \
                 served under the name of an adjusted one",
                kind.code(), a.date, a.value,
            ));
        };
        factor /= f;
        if !factor.is_finite() || factor <= 0.0 {
            return Err(format!(
                "the actions up to {} multiply out to a scale no price survives, which \
                 is not a series anyone can be handed", a.date,
            ));
        }
    }
    Ok(factor)
}

/// What a volume before `date` must be multiplied by to count on the same scale
/// as volumes after every action in `actions`.
///
/// The reciprocal of [`scale_before`]: the same shares are the same shares, so
/// what a split divides out of the price it multiplies into the count.
pub fn scale_volume_before(date: &str, actions: &[Adjustment]) -> Result<f64, String> {
    scale_before(date, actions).map(|f| 1.0 / f)
}

/// The largest count that survives a trip through a float unchanged.
///
/// Two to the fifty-third. Past it the conversion starts rounding, and a volume
/// that comes back one short of what the venue said is a wrong number wearing
/// the shape of a right one.
const EXACT_IN_A_FLOAT: u64 = 1 << 53;

/// The day a bar is dated, as an action states its own.
///
/// A bar states a day and sometimes a time after it; an action states only a
/// day. Comparing the two as text works on that prefix and nowhere else, so a
/// bar that does not state one is `None` rather than compared as it stands.
///
/// Eight digits alone is not enough to say it does. A bar stamped in seconds
/// since the epoch opens with eight digits too, and taking those as a day
/// gives a date in the seventeenth millennium that sorts before every action
/// there is — so the whole series would be scaled by all of them. What tells
/// the two apart is what follows: a day is followed by a time or by nothing,
/// never by a ninth digit.
fn day_of(bar_date: &str, zone: &str) -> Option<String> {
    let day: String = bar_date.chars().take(8).collect();
    if day.len() != 8 || !day.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let carries_a_time = match bar_date.as_bytes().get(8) {
        None => false,
        Some(b' ' | b'-' | b'T') => true,
        Some(c) if !c.is_ascii_digit() => true,
        Some(_) => return None,
    };
    // Eight digits are a shape, not a date. The thirteenth month and the
    // thirty-second of a month both pass the shape and name no day, and read
    // as text they sort among the days that do — so an action stated on one
    // would move the prices either side of a day that never happened.
    crate::protocol::datetime::ib_datetime_to_unix(&format!("{day}-00:00:00"))?;
    // An action is dated on the day the exchange had, and a stamp below a day
    // arrives stated in UTC — so the two are only the same day for a session
    // that does not cross midnight there. A morning in Sydney is the evening
    // before in UTC, and an American evening session is the next day: taken
    // from the wire's own eight digits, the first hour of an ex-date was
    // divided by the ratio a second time and the last hour before one was left
    // on the old scale, which is the wrong number under the adjusted name that
    // this fold exists to prevent.
    //
    // Only for a stamp that has not already been put on the contract's clock:
    // one that has says so by naming the zone, and converting it again would
    // move it twice.
    if carries_a_time && !zone.is_empty() && !already_on_a_named_clock(bar_date) {
        let local = crate::protocol::datetime::bar_date_as_asked(bar_date, 1, zone);
        let there: String = local.chars().take(8).collect();
        if there.len() == 8 && there.bytes().all(|c| c.is_ascii_digit()) {
            return Some(there);
        }
    }
    Some(day)
}

/// Whether a stamp already states the clock it is written on.
///
/// A stamp handed to a caller carries the zone's name after the time; one read
/// off the wire does not.
fn already_on_a_named_clock(bar_date: &str) -> bool {
    bar_date
        .rsplit(|c: char| c.is_ascii_whitespace())
        .next()
        .is_some_and(|last| {
            !last.is_empty()
                && !last.bytes().all(|c| c.is_ascii_digit() || c == b':')
                && crate::protocol::datetime::clock_named(last).is_some()
        })
}

pub fn scale_bars(
    bars: Vec<crate::types::model::BarData>, actions: &[Adjustment],
) -> Result<Vec<crate::types::model::BarData>, String> {
    bars.into_iter()
        .map(|mut b| {
            let Some(day) = day_of(&b.date, &b.timezone) else {
                return Err(format!(
                    "a bar dated {:?} states no day to compare an action against, so \
                     putting it on one scale would be guesswork", b.date,
                ));
            };
            let price = scale_before(&day, actions)?;
            b.open *= price;
            b.high *= price;
            b.low *= price;
            b.close *= price;
            b.wap *= price;
            // Volume is a count, and a count that does not survive the trip
            // through a float is not reported as though it had. Left as it was
            // where nothing moved it, so an unadjusted series is bit for bit
            // what the venue served.
            //
            // It moves for every kind that moves the price, spin-offs included.
            // That reads wrong at first — a spin-off restates what a share is
            // worth without changing how many were traded — and it is what the
            // protocol does: one routine applies a factor to an event, it
            // divides the prices and multiplies the count, and every scale
            // moving kind goes through it. A count scaled differently here
            // would be this client's own convention rather than the one the
            // series is stated in.
            //
            // A factor that is not a whole number leaves a count that is not
            // one either, and an integer cannot hold it: a three-for-two on a
            // hundred and one shares is a hundred and fifty one and a half.
            // Rounded, because the field is a count and the alternative is
            // refusing a series over half a share.
            let volume = scale_volume_before(&day, actions)?;
            if volume != 1.0 {
                // Checked before the conversion as well as after. A count past
                // what a float holds exactly loses bits on the way in, and a
                // factor that then brings it back under the limit hides that:
                // the answer passes the test on the way out having already been
                // rounded on the way in.
                if b.volume.unsigned_abs() > EXACT_IN_A_FLOAT {
                    return Err(format!(
                        "the bar dated {} states a volume of {}, which is more than a \
                         scale can be applied to without changing it", b.date, b.volume,
                    ));
                }
                let scaled = b.volume as f64 * volume;
                if !scaled.is_finite() || scaled.abs() > EXACT_IN_A_FLOAT as f64 {
                    return Err(format!(
                        "the volume of the bar dated {} does not survive being put on one \
                         scale, and a count nobody can state is not one to hand back", b.date,
                    ));
                }
                b.volume = scaled.round() as i64;
            }
            Ok(b)
        })
        .collect()
}

/// Put the bars the historical connection carries on one scale, as
/// [`scale_bars`] does for the bars the callbacks carry.
///
/// The wire bar and the callback bar hold the same numbers under other names,
/// so the wire bar is lent to `scale_bars` and taken back rather than folded a
/// second way: an adjusted series answered on a callback and one answered by a
/// waiting call are then the same series, because one routine states the fold.
/// `scale_bars` moves the prices and the volume and leaves the day it is dated
/// and the count that made it alone, so the round trip carries those across
/// untouched.
pub fn scale_historical_bars(
    bars: Vec<crate::control::historical::HistoricalBar>, actions: &[Adjustment], zone: &str,
) -> Result<Vec<crate::control::historical::HistoricalBar>, String> {
    let model = bars
        .into_iter()
        .map(|b| crate::types::model::BarData {
            date: b.time, open: b.open, high: b.high, low: b.low, close: b.close,
            volume: b.volume, wap: b.wap, bar_count: b.count,
            timezone: zone.to_string(), end: b.end,
        })
        .collect();
    Ok(scale_bars(model, actions)?
        .into_iter()
        .map(|b| crate::control::historical::HistoricalBar {
            time: b.date, open: b.open, high: b.high, low: b.low, close: b.close,
            volume: b.volume, wap: b.wap, count: b.bar_count, end: b.end,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bar below a day is folded on the exchange's day, not on UTC's.
    ///
    /// An action is dated on the day the exchange had. A stamp below a day
    /// arrives stated in UTC, so for any session that crosses midnight there
    /// the two are different days: a morning in Sydney is the evening before
    /// in UTC, and an American evening session is already the next day. Taken
    /// from the wire's own eight digits, the first hour of an ex-date was
    /// divided by the ratio a second time and the last hour before one was
    /// left on the old scale — the wrong number under the adjusted name that
    /// this fold exists to prevent.
    #[test]
    fn a_timed_bar_is_folded_on_the_day_the_exchange_was_having() {
        // Ten for one on the tenth, so a price before it is divided by ten.
        let split = vec![Adjustment {
            kind: Some(AdjustmentKind::Split),
            date: "20240610".to_string(),
            value: "10".into(),
            ..Default::default()
        }];

        // Sydney, 09:00 on the tenth — the ex-date, so this price is already
        // restated and nothing moves it. UTC was still on the ninth.
        let sydney_morning = crate::types::model::BarData {
            date: "20240609-23:00:00".to_string(), open: 100.0, high: 100.0,
            low: 100.0, close: 100.0, volume: 10, wap: 100.0, bar_count: 1,
            timezone: "Australia/Sydney".to_string(), end: String::new(),
        };
        // New York, 20:30 on the ninth — the evening before, so this price is
        // the old one and is divided by the ratio. UTC had turned the tenth.
        let new_york_evening = crate::types::model::BarData {
            date: "20240610-00:30:00".to_string(), open: 1000.0, high: 1000.0,
            low: 1000.0, close: 1000.0, volume: 10, wap: 1000.0, bar_count: 1,
            timezone: "US/Eastern".to_string(), end: String::new(),
        };

        let out = scale_bars(vec![sydney_morning, new_york_evening], &split)
            .expect("both bars state a day");
        assert!((out[0].close - 100.0).abs() < 1e-9,
            "already past the action where it traded, so it is left alone: {}", out[0].close);
        assert!((out[1].close - 100.0).abs() < 1e-9,
            "still before it where it traded, so it is divided by the ratio: {}", out[1].close);
    }

    /// A stamp already put on the contract's clock is not moved again.
    ///
    /// One handed to a caller says which clock it is on. Converted a second
    /// time it would shift by the offset twice, which is the same defect in
    /// the other direction.
    #[test]
    fn a_stamp_that_names_its_clock_is_taken_as_it_stands() {
        assert_eq!(day_of("20240610 09:30:00 US/Eastern", "US/Eastern").as_deref(), Some("20240610"));
        assert_eq!(day_of("20240610 00:30:00 Australia/Sydney", "Australia/Sydney").as_deref(),
            Some("20240610"));
        // And one off the wire is put on it.
        assert_eq!(day_of("20240611-00:30:00", "US/Eastern").as_deref(), Some("20240610"));
        assert_eq!(day_of("20240609-23:30:00", "Australia/Sydney").as_deref(), Some("20240610"));
        // A day on its own states no time to convert, whatever clock is named.
        assert_eq!(day_of("20240610", "Australia/Sydney").as_deref(), Some("20240610"));
        // And with no clock named, the stamp is all there is.
        assert_eq!(day_of("20240611-00:30:00", "").as_deref(), Some("20240611"));
    }

    /// A future rolling into its next month is read, and moves no scale.
    ///
    /// No contract this session can reach carries one, so the row is written
    /// here as the venue writes it. What the code does with it is this
    /// client's own and is checkable without one: the kind is named rather
    /// than left unknown, and a price either side of the day it states is
    /// unmoved, because a rollover is a contract ending rather than a price
    /// being restated.
    #[test]
    fn a_future_rollover_is_named_and_moves_no_scale() {
        let answered = "conc\n4815747,-1,-1\n\
                        FR\n20240315,0,,20240315\n";
        let (_, actions) = parse_adjustments(answered);
        assert_eq!(actions.len(), 1, "the row was dropped rather than read");
        assert_eq!(
            actions[0].kind, Some(AdjustmentKind::FutureRollover),
            "an FR row was left with no kind, so whoever scales cannot tell it \
             from one this client has never heard of",
        );
        assert_eq!(
            actions[0].kind.map(AdjustmentKind::code), Some("FR"),
            "the kind does not write itself back",
        );

        for day in ["20240314", "20240316"] {
            let scale = scale_before(day, &actions).expect("a rollover is not a refusal");
            assert!(
                (scale - 1.0).abs() < f64::EPSILON,
                "a rollover moved the scale to {scale} on {day}; it restates no price",
            );
        }
    }

    /// A price from before a split reads on the scale after it.
    ///
    /// The venue stated a ten-for-one split on a day, and a bar series across
    /// that day closed at 1208.88 the session before and 121.79 the session
    /// after: a tenfold step with nothing in the series saying so. Scaled by
    /// what the split states, the earlier close reads 120.89, which is the
    /// later one's scale. That is the whole of what an adjusted series is, and
    /// both numbers are ones a session was answered with.
    #[test]
    fn a_price_from_before_a_split_reads_on_the_scale_after_it() {
        let answered = "conc\n4815747,-1,-1\n\
                        CD\n20240305,0.04,USD,20240221,20240306,20240327,R,NA\n\
                        SS\n20240610,10,,20240522\n";
        let (_, actions) = parse_adjustments(answered);
        let before = 1208.88 * scale_before("20240607", &actions).expect("a stated split");
        assert!((before - 120.888).abs() < 0.001, "scaled to {before}, not 120.888");
        let after = 121.79 * scale_before("20240611", &actions).expect("a stated split");
        assert!((after - 121.79).abs() < 0.001, "a price after the split is unmoved");
        assert!(
            (before - 121.79).abs() / 121.79 < 0.01,
            "and the sessions either side sit within a day's move of each other",
        );
    }

    #[test]
    fn a_request_names_the_contract_and_the_range() {
        let xml = build_adjustments_request_xml(&AdjustmentRequest {
            query_id: "ADJ.1".into(),
            con_id: 756733,
            sec_type: "STK".into(),
            exchange: "SMART".into(),
            start_date: "20240101".into(),
            end_date: "20241231".into(),
        });
        assert!(xml.contains("<id>ADJ.1</id>"));
        assert!(xml.contains("<contractID>756733</contractID>"));
        assert!(xml.contains("<secType>STK</secType>"));
        assert!(xml.contains("<startDate>20240101</startDate>"));
        assert!(xml.contains("<endDate>20241231</endDate>"));
        assert!(xml.contains("<divRequestType>T</divRequestType>"));
    }

    /// The reply carries its actions in the raw field, and parsing keeps them.
    ///
    /// The query comes back echoed as XML on one tag and the actions arrive on
    /// another, under the length that precedes it. Nothing here would work if
    /// the length-prefixed field did not survive being parsed, so that is
    /// asserted rather than assumed: this is the message a live session was
    /// answered with, put back together and read the way the engine reads one.
    #[test]
    fn the_actions_survive_being_parsed_out_of_the_reply() {
        let payload = "conc\n756733,-1,-1\nconexch\n756733,AMEX,20090223\nCD\n\
20240315,1.594937,USD,20240314,20240318,20240430,R,NA\n";
        let msg = format!(
            "8=FIX.4.1\x019=000355\x0135=U\x016040=10022\x016118=<ConAdjResponse>\
             <id>adj_1</id></ConAdjResponse>\x0195={}\x0196={}\x0110=200\x01",
            payload.len(), payload,
        );
        let parsed = crate::protocol::fix::fix_parse(msg.as_bytes());
        let carried = parsed.get(&96).expect("the actions are on the raw field");
        assert_eq!(carried.len(), payload.len(), "and the whole of it is kept");
        let (contract, adj) = parse_adjustments(carried);
        assert_eq!(contract.con_id, "756733");
        assert_eq!(adj.len(), 1, "the action in it is read");
    }

    /// What a live session was answered with, kept as it arrived.
    ///
    /// Asked for one contract's actions over 2024, the venue answered a name on
    /// its own line and the rows under it. The contract states its id and the
    /// venue it is listed on; each dividend states its date, what it paid, in
    /// what currency, and the three dates around it.
    #[test]
    fn what_the_venue_answered_is_read() {
        let answered = "conc\n756733,-1,-1\n\
                        conexch\n756733,AMEX,20090223\n\
                        CD\n\
                        20240315,1.594937,USD,20240314,20240318,20240430,R,NA\n\
                        20240621,1.759024,USD,20240620,20240621,20240731,R,NA\n\
                        20240920,1.745531,USD,20240919,20240920,20241031,R,NA\n\
                        20241220,1.965548,USD,20241219,20241220,20250131,R,NA\n";
        let (contract, adj) = parse_adjustments(answered);
        assert_eq!(contract.con_id, "756733", "the contract names itself first");
        assert_eq!(contract.exchange, "AMEX", "and the venue it is listed on");
        assert_eq!(adj.len(), 4, "four dividends were paid over the year asked for");
        assert_eq!(adj[0].kind, Some(AdjustmentKind::CashDividend));
        assert_eq!(adj[0].date, "20240315");
        assert_eq!(adj[0].value, "1.594937");
        assert_eq!(adj[0].currency, "USD");
        assert_eq!(adj[0].pay_date, "20240430");
        assert_eq!(adj[3].date, "20241220", "and they come back in date order");
    }


    /// A stock dividend and a spin-off move the scale, and each its own way.
    ///
    /// Both restate the share rather than pay out of it, so both move a price
    /// dated before them. They differ in what their value means: a stock
    /// dividend states the factor, and a spin-off states its reciprocal. Read
    /// the same way round they would move a price in opposite directions, which
    /// is why the kind decides and not the number.
    #[test]
    fn the_kind_decides_which_way_the_value_reads() {
        let at = |kind, value: &str| {
            vec![Adjustment {
                kind: Some(kind),
                date: "20240610".into(),
                value: value.into(),
                ..Default::default()
            }]
        };
        // Two for one: a price before it reads half.
        assert!((scale_before("20240607", &at(AdjustmentKind::StockDividend, "2")).unwrap() - 0.5).abs() < 1e-9);
        // The same number on a spin-off is the reciprocal, so the price doubles.
        assert!((scale_before("20240607", &at(AdjustmentKind::SpinOff, "2")).unwrap() - 2.0).abs() < 1e-9);
    }

    /// A cash dividend and a rights offer leave the scale where it is.
    ///
    /// Both are stated for the same contract and both carry a value, so a
    /// reading that went by the number rather than the kind would move the
    /// price by them. Neither should.
    #[test]
    fn a_payment_out_of_the_price_does_not_restate_it() {
        let actions: Vec<Adjustment> = [AdjustmentKind::CashDividend, AdjustmentKind::RightsOffer]
            .into_iter()
            .map(|kind| Adjustment {
                kind: Some(kind),
                date: "20240610".into(),
                value: "2".into(),
                ..Default::default()
            })
            .collect();
        assert_eq!(scale_before("20240607", &actions), Ok(1.0));
    }

    /// Volume goes the other way from price.
    ///
    /// The same shares traded either side of a split, so ten times the count at
    /// a tenth of the price is the same trade. A caller that scaled volume the
    /// way it scales price would report a tenth of what changed hands.
    #[test]
    fn the_same_shares_count_the_same_across_a_split() {
        let split = vec![Adjustment {
            kind: Some(AdjustmentKind::Split),
            date: "20240610".into(),
            value: "10".into(),
            ..Default::default()
        }];
        assert!((scale_before("20240607", &split).unwrap() - 0.1).abs() < 1e-9);
        assert!((scale_volume_before("20240607", &split).unwrap() - 10.0).abs() < 1e-9);
    }


    /// A series across a split comes back on one scale, volume included.
    ///
    /// The two closes are ones a session was answered with: 1208.88 the day
    /// before a ten-for-one split and 121.79 the day after, a tenfold step in
    /// the raw series. Scaled, the earlier bar reads 120.888 and the later one
    /// is untouched, and the shares that traded before the split count for ten
    /// times as many after it. The volumes here are round numbers so the
    /// direction is unmistakable: a reading that scaled volume the way it
    /// scales price would report a tenth.
    #[test]
    fn a_series_across_a_split_comes_back_on_one_scale() {
        use crate::types::model::BarData;
        let bar = |date: &str, close: f64, volume: i64| BarData {
            date: date.into(),
            open: close, high: close, low: close, close, wap: close,
            volume,
            ..Default::default()
        };
        let split = vec![Adjustment {
            kind: Some(AdjustmentKind::Split),
            date: "20240610".into(),
            value: "10".into(),
            ..Default::default()
        }];
        let out = scale_bars(vec![bar("20240607", 1208.88, 100), bar("20240610", 121.79, 100)], &split)
            .expect("both bars state a day");
        assert!((out[0].close - 120.888).abs() < 1e-9, "before: {}", out[0].close);
        assert_eq!(out[0].volume, 1000);
        // The day of the split is already on the new scale and stays as it is.
        assert!((out[1].close - 121.79).abs() < 1e-9, "after: {}", out[1].close);
        assert_eq!(out[1].volume, 100);
    }

    /// The bars the historical connection carries fold the same way the ones
    /// the callbacks carry do, because one routine states the fold. The
    /// callback path scales these before it hands a bar over, so a split it
    /// crosses steps its prices and its volume and leaves the day and the
    /// count that made it alone.
    #[test]
    fn the_wire_bars_fold_the_same_way_the_callback_bars_do() {
        use crate::control::historical::HistoricalBar;
        let split = vec![Adjustment {
            kind: Some(AdjustmentKind::Split),
            date: "20240610".into(),
            value: "10".into(),
            ..Default::default()
        }];
        let bar = |time: &str, close: f64, volume: i64| HistoricalBar {
            time: time.into(),
            open: close, high: close, low: close, close, wap: close,
            volume, count: 7, end: String::new(),
        };
        let out = scale_historical_bars(
            vec![bar("20240607", 1208.88, 100), bar("20240610", 121.79, 100)], &split, "",
        )
        .expect("both bars state a day");
        assert!((out[0].close - 120.888).abs() < 1e-9, "before: {}", out[0].close);
        assert_eq!(out[0].volume, 1000, "the shares before the split count for ten times as many");
        assert_eq!(out[0].time, "20240607", "the day it is dated is untouched");
        assert_eq!(out[0].count, 7, "and the count that made it");
        assert!((out[1].close - 121.79).abs() < 1e-9, "after: {}", out[1].close);
        assert_eq!(out[1].volume, 100);
    }

    /// A bar with no day in it is refused rather than scaled by a guess.
    ///
    /// Scaling compares the bar's day against the action's as text. A bar
    /// stamped in seconds compares smaller than any day, so every action would
    /// read as later than it and the whole series would be scaled by all of
    /// them. That is a wrong number, and a wrong number is worse than an error.
    #[test]
    fn a_bar_that_states_no_day_is_refused() {
        use crate::types::model::BarData;
        let split = vec![Adjustment {
            kind: Some(AdjustmentKind::Split),
            date: "20240610".into(),
            value: "10".into(),
            ..Default::default()
        }];
        let epoch = BarData { date: "1717718400".into(), close: 1208.88, ..Default::default() };
        assert!(scale_bars(vec![epoch], &split).is_err());
    }


    /// A moving action nobody can read a factor from stops the series.
    ///
    /// Skipping it is the failure this module exists to prevent, arriving by
    /// the other door: the price it should have moved is handed back exactly as
    /// the venue served it, under the name of an adjusted one. A split with no
    /// value, or a value that is not a number, or one that multiplies out to
    /// nothing, is said out loud instead.
    #[test]
    fn a_factor_nobody_can_read_is_stated_rather_than_skipped() {
        use crate::types::model::BarData;
        let bar = BarData { date: "20240607".into(), close: 1208.88, volume: 100, ..Default::default() };
        for unreadable in ["", "nan", "0", "-10", "inf", "banana"] {
            let split = vec![Adjustment {
                kind: Some(AdjustmentKind::Split),
                date: "20240610".into(),
                value: unreadable.into(),
                ..Default::default()
            }];
            assert!(
                scale_bars(vec![bar.clone()], &split).is_err(),
                "a split stating {unreadable:?} must not pass as an adjusted series",
            );
        }
        // A NaN is the one that gets through a `value <= 0.0` test, because
        // every comparison with it is false.
        assert_eq!(AdjustmentKind::Split.factor(f64::NAN), None);
        assert_eq!(AdjustmentKind::Split.factor(f64::INFINITY), None);
        assert_eq!(AdjustmentKind::SpinOff.factor(f64::NAN), None);
    }

    /// An action that does not move the scale is not read for a factor at all.
    ///
    /// A cash dividend states an amount of money, which is not a factor and
    /// never was. Reading one would refuse the series over a number that was
    /// never going to be used.
    #[test]
    fn a_dividend_that_states_no_factor_is_not_an_error() {
        use crate::types::model::BarData;
        let bar = BarData { date: "20240607".into(), close: 100.0, volume: 5, ..Default::default() };
        let dividend = vec![Adjustment {
            kind: Some(AdjustmentKind::CashDividend),
            date: "20240610".into(),
            value: "0.04".into(),
            currency: "USD".into(),
            ..Default::default()
        }];
        let out = scale_bars(vec![bar], &dividend).expect("a dividend moves nothing");
        assert_eq!(out[0].close, 100.0);
        assert_eq!(out[0].volume, 5, "and the count is what the venue served, exactly");
    }


    /// A moving action with no usable day stops the series too.
    ///
    /// The same failure as a factor nobody can read, arriving through the other
    /// field. An empty day compares before every date there is, so the action
    /// reads as already past and is skipped — and the price it should have
    /// moved comes back exactly as the venue served it, under the name of an
    /// adjusted one.
    #[test]
    fn a_day_nobody_can_read_stops_the_series_as_a_factor_does() {
        use crate::types::model::BarData;
        let bar = BarData { date: "20240607".into(), close: 1208.88, volume: 100, ..Default::default() };
        // The last two are shapes without days in them: a thirteenth month and
        // a thirty-first of February pass eight digits and name nothing.
        for undated in ["", "2024-06-10", "1717718400", "june", "20241340", "20240231"] {
            let split = vec![Adjustment {
                kind: Some(AdjustmentKind::Split),
                date: undated.into(),
                value: "10".into(),
                ..Default::default()
            }];
            assert!(
                scale_bars(vec![bar.clone()], &split).is_err(),
                "a split dated {undated:?} must not pass as an adjusted series",
            );
        }
        // A kind that moves nothing is not read for a day it never uses.
        let dividend = vec![Adjustment {
            kind: Some(AdjustmentKind::CashDividend),
            date: String::new(),
            value: "0.04".into(),
            ..Default::default()
        }];
        assert!(scale_bars(vec![bar], &dividend).is_ok(), "a dividend moves nothing, dated or not");
    }


    /// A hostile or sloppy answer cannot come back as a plausible wrong number.
    ///
    /// Three shapes, each of which used to scale a series and say nothing: the
    /// same action stated twice, an action named by a kind this client does not
    /// know, and a date carrying something after its eight digits. The first
    /// moved the scale twice, the second was dropped so the series came back
    /// missing an action that moved it, and the third sorted by its tail and
    /// moved the bars either side of the wrong day.
    #[test]
    fn a_sloppy_answer_is_refused_rather_than_scaled() {
        use crate::types::model::BarData;
        let bars = || vec![BarData {
            date: "20240607".into(), close: 1208.88, volume: 100, ..Default::default()
        }];

        // What the venue states twice is kept twice. Collapsing them needs an
        // identity the venue does not state, and the fields that would tell two
        // same-day actions apart are the ones such a reading discards.
        let (_, twice) = parse_adjustments("conc\n4815747,\nSS\n20240610,10,,20240522\nSS\n20240610,10,,20240522\n");
        assert_eq!(twice.len(), 2, "both rows are kept, because both were stated");

        // A kind this client cannot name stops the series.
        let (_, unknown) = parse_adjustments("conc\n4815747,\nZZ\n20240610,10,,20240522\n");
        assert_eq!(unknown.len(), 1, "the row is kept, not dropped");
        assert!(unknown[0].kind.is_none());
        assert!(
            scale_bars(bars(), &unknown).is_err(),
            "an action nobody can classify is not one that can be said to move nothing",
        );

        // A date is compared as the day it is, not the string it arrived in.
        let tailed = vec![Adjustment {
            kind: Some(AdjustmentKind::Split),
            date: "20240610 00:00:00".into(),
            value: "10".into(),
            ..Default::default()
        }];
        let out = scale_bars(bars(), &tailed).expect("a day with a time after it");
        assert!((out[0].close - 120.888).abs() < 1e-9, "close {}", out[0].close);
    }
}
