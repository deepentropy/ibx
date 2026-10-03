//! Values of the auth logon reply that the reference applies to the API
//! (ibx#421): the clock offset of the current time request, the version
//! cutoff warning, the feature list tokens that gate API requests, the
//! historical data years limit and the data permission stamp.

use std::sync::atomic::{AtomicI64, Ordering};

/// Local clock in milliseconds since the epoch.
pub fn local_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

/// A server time of tag 52 (`yyyyMMdd-HH:mm:ss`, UTC) in milliseconds
/// since the epoch.
pub fn server_time_ms(value: &str) -> Option<i64> {
    let dt = jiff::civil::DateTime::strptime("%Y%m%d-%H:%M:%S", value.trim()).ok()?;
    Some(dt.to_zoned(jiff::tz::TimeZone::UTC).ok()?.timestamp().as_millisecond())
}

/// Longest time between the receipt of a message and the use of its
/// server time; above it the reference keeps its offset.
const MAX_HANDLING_DELAY_MS: i64 = 100;

/// Shortest time between two offset updates from messages after the
/// logon.
const MESSAGE_UPDATE_INTERVAL_MS: i64 = 30_000;

/// The offset of the local clock to the server clock, as the reference
/// keeps it: set by the server time of every logon reply, then refreshed
/// at most every 30 s by the server time of a test request or of an
/// eligible `35=U` message (`twslaunch.jutils.aO.a(long, List, boolean)`,
/// `jutils.d1.a(jfix.dk, long, String)`). The current time request
/// answers the local clock plus this offset (`aO.r()`).
#[derive(Debug, Default)]
pub struct ClockOffset {
    offset_ms: AtomicI64,
    last_message_update_ms: AtomicI64,
}

impl ClockOffset {
    /// The current offset in milliseconds (server minus local).
    pub fn offset_ms(&self) -> i64 {
        self.offset_ms.load(Ordering::Relaxed)
    }

    /// Set the offset, as computed by [`logon_offset`].
    pub fn set(&self, offset_ms: i64) {
        self.offset_ms.store(offset_ms, Ordering::Relaxed);
    }

    /// The server clock: the local clock plus the offset.
    pub fn now_ms(&self) -> i64 {
        local_now_ms() + self.offset_ms()
    }

    /// Refresh from the server time `server_ms` of a message handled at
    /// `now_ms`, local time in `zone`: skipped within 30 s of the last
    /// refresh, and when a negative offset would put the server date
    /// before the local date. Returns the new offset when it was set.
    pub fn apply_message(&self, server_ms: i64, now_ms: i64, zone: &jiff::tz::TimeZone) -> Option<i64> {
        if now_ms - self.last_message_update_ms.load(Ordering::Relaxed) <= MESSAGE_UPDATE_INTERVAL_MS {
            return None;
        }
        self.last_message_update_ms.store(now_ms, Ordering::Relaxed);
        let offset = offset(server_ms, now_ms, now_ms, false, zone)?;
        self.set(offset);
        Some(offset)
    }
}

/// The offset of a logon reply with server time `server_ms`, received at
/// `received_ms` and handled at `now_ms` (local times). None when the
/// handling took more than 100 ms, as in the reference.
pub fn logon_offset(server_ms: i64, received_ms: i64, now_ms: i64) -> Option<i64> {
    offset(server_ms, received_ms, now_ms, true, &jiff::tz::TimeZone::UTC)
}

fn offset(server_ms: i64, received_ms: i64, now_ms: i64, logon: bool, zone: &jiff::tz::TimeZone) -> Option<i64> {
    let delay = (now_ms - received_ms).max(0);
    if delay > MAX_HANDLING_DELAY_MS {
        log::info!("Server time not used: handled {} ms after its receipt", delay);
        return None;
    }
    let offset = server_ms + delay - now_ms;
    if !logon && offset < 0 {
        let day = |ms: i64| jiff::Timestamp::from_millisecond(ms).ok().map(|t| t.to_zoned(zone.clone()).date());
        if day(now_ms + offset) < day(now_ms) {
            log::warn!("Setting time offset SKIPPED: suggested offset {} ms", offset);
            return None;
        }
    }
    Some(offset)
}

/// Whether a server message sets the clock offset after the logon: a test
/// request, or a `35=U` message without an account (tag 1) whose
/// message number `comm` (tag 6040) is not 60, 146 or 151
/// (`jconnection.ai.a(jfix.dk, int)@160-177`, `@569-665`). The reference
/// also uses a 6040=110 message in one session state ibx does not track;
/// such a message is not used here.
pub fn message_sets_clock(msg_type: &str, comm: Option<&str>, has_account: bool) -> bool {
    match msg_type {
        "1" => true,
        "U" => !has_account && !matches!(comm.map(str::trim), Some("60" | "146" | "151" | "110")),
        _ => false,
    }
}

/// The version the client logs on with, build then sub-version
/// (`10401c`), as the reference's `JtsVersion.a()`.
pub fn own_version() -> String {
    format!("{}{}", crate::config::IB_BUILD, crate::config::IB_VERSION)
}

/// The version the reference names in its warning: major, a dot, minor
/// (`1040.1`, `JtsVersion.BUILDANDREV`).
fn own_version_label() -> String {
    let build = crate::config::IB_BUILD;
    format!("{}.{}", &build[..4], &build[4..])
}

/// A version of the form `\d{5}[a-z]?` (`JtsVersion.VALID_FULL_VERSION_PATTERN`).
fn is_full_version(v: &str) -> bool {
    let b = v.as_bytes();
    (b.len() == 5 || (b.len() == 6 && b[5].is_ascii_lowercase())) && b[..5].iter().all(u8::is_ascii_digit)
}

/// Whether the cutoff version of the logon (tag 6243) is above `own`, as
/// `jsetting.JtsVersion.b(String)`: the five digits compare as numbers;
/// with the same digits, a version with a letter is above one without, and
/// two letters compare as characters. False when either is not a full
/// version.
pub fn cutoff_applies(own: &str, cutoff: &str) -> bool {
    if !is_full_version(own) || !is_full_version(cutoff) {
        return false;
    }
    let (a, b): (u32, u32) = (own[..5].parse().unwrap_or(0), cutoff[..5].parse().unwrap_or(0));
    if a != b {
        return a < b;
    }
    if own.len() < cutoff.len() {
        return true;
    }
    own.len() == 6 && cutoff.len() == 6 && own.as_bytes()[5] < cutoff.as_bytes()[5]
}

/// API error code of the version cutoff warning.
pub const VERSION_CUTOFF_CODE: i64 = 2172;

/// The warning 2172 the reference sends with id -1 at every API connect
/// when the cutoff version of the logon (tag 6243) is above the client's
/// own, with the cutoff date (tag 6244); None otherwise
/// (`jextend.dL.bq()@1148-1297`). The text is the reference's
/// `Version_Cutoff` text with its markup removed (`<br>` becomes a space).
pub fn version_cutoff_warning(cutoff: Option<&str>, date: Option<&str>) -> Option<String> {
    let cutoff = cutoff?;
    if !cutoff_applies(&own_version(), cutoff) {
        return None;
    }
    Some(format!(
        "The version of the application you are running, {}, needs to be upgraded, as it will be desupported on {}. \
         The minimum supported version at that time will be {}.{}. ",
        own_version_label(), date.unwrap_or(""), &cutoff[..4], &cutoff[4..],
    ))
}

/// One of the comma separated tokens of a feature list is `feature`.
pub fn has_feature(features: &str, feature: &str) -> bool {
    features.split(',').any(|f| f == feature)
}

/// Feature tokens of the logon feature list (tag 6542) that gate API
/// requests (ibx#421).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApiFeatures {
    /// DENYAPI: API connections are refused (`jfix.s.be()`).
    pub deny_api: bool,
    /// SECDEFTA: matching symbols requests are allowed (`jfix.s.d1()`).
    pub matching_symbols: bool,
    /// NIGHTLY: the historical data years limit is not checked
    /// (`jfix.s.x()`).
    pub nightly: bool,
    /// NOMAGNFIX: option chain strikes are not scaled by the price
    /// magnifier (`jfix.s.e3()`, ibx#440).
    pub no_magnifier_fix: bool,
    /// ISLAND2NASDAQ: NASDAQ is not left out of the option chains
    /// (`jfix.s.hy()`, ibx#440).
    pub island_to_nasdaq: bool,
}

impl ApiFeatures {
    pub fn parse(features: &str) -> Self {
        Self {
            deny_api: has_feature(features, "DENYAPI"),
            matching_symbols: has_feature(features, "SECDEFTA"),
            nightly: has_feature(features, "NIGHTLY"),
            no_magnifier_fix: has_feature(features, "NOMAGNFIX"),
            island_to_nasdaq: has_feature(features, "ISLAND2NASDAQ"),
        }
    }
}

/// The reference's text when DENYAPI refuses an API connection
/// (`jextend.ev`, API-SESSION 1.1).
pub const API_NOT_ALLOWED: &str = "Disconnecting API request since regular API is not allowed.";

/// Most years of a historical data request: tag 6774 of the logon when
/// above 0, else 1 (`jclient.gi.a(jfix.dk, jfix.bb, boolean, boolean, boolean)@573-594`).
pub fn max_backfill_years(tag: Option<&str>) -> i32 {
    tag.and_then(|v| v.trim().parse::<i32>().ok()).filter(|n| *n > 0).unwrap_or(1)
}

/// The cause of the refusal of a duration in years above `max_years`
/// (`jextend.bM.n()@925-1046`); `duration` is the normalised duration
/// (`{n} y`).
pub fn backfill_years_refusal(duration: &str, max_years: i32) -> Option<String> {
    let (n, unit) = duration.split_once(' ')?;
    let n: i32 = n.parse().ok()?;
    (unit == "y" && n > max_years).then(|| {
        format!("Historical data request for {} year(s) rejected. Max API Backfill Years={}", n, max_years)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ibx#421: tag 52 of the captured logon reply of 02/10/2026.
    #[test]
    fn server_time_of_tag_52_is_utc() {
        assert_eq!(server_time_ms("20261002-06:15:59"), Some(1_790_921_759_000));
        assert_eq!(server_time_ms("2026-10-02"), None);
    }

    // ibx#421: a logon reply one minute ahead of the local clock gives an
    // offset of one minute; a reply handled more than 100 ms after its
    // receipt is not used.
    #[test]
    fn logon_offset_from_the_server_time() {
        let local = 1_790_921_759_000;
        assert_eq!(logon_offset(local + 60_000, local, local), Some(60_000));
        assert_eq!(logon_offset(local + 60_000, local, local + 50), Some(60_000));
        assert_eq!(logon_offset(local - 2_000, local, local), Some(-2_000));
        assert_eq!(logon_offset(local + 60_000, local, local + 101), None);
    }

    #[test]
    fn clock_answers_local_time_plus_offset() {
        let clock = ClockOffset::default();
        clock.set(60_000);
        let now = clock.now_ms();
        assert!((now - local_now_ms() - 60_000).abs() < 1_000, "{now}");
    }

    // ibx#421: after the logon, messages refresh the offset at most every
    // 30 s; a negative offset that changes the date is skipped.
    #[test]
    fn message_refresh_every_30_s_and_date_rule() {
        let utc = jiff::tz::TimeZone::UTC;
        let clock = ClockOffset::default();
        let t = 1_790_921_759_000; // 02/10/2026 06:15:59 UTC
        assert_eq!(clock.apply_message(t + 1_000, t, &utc), Some(1_000));
        assert_eq!(clock.apply_message(t + 5_000, t + 30_000, &utc), None);
        assert_eq!(clock.offset_ms(), 1_000);
        assert_eq!(clock.apply_message(t + 30_001 - 500, t + 30_001, &utc), Some(-500));
        // 00:00:10 local, server 23:59:50 the day before: skipped.
        let midnight = jiff::civil::date(2026, 10, 3).at(0, 0, 10, 0).to_zoned(utc.clone()).unwrap().timestamp().as_millisecond();
        assert_eq!(clock.apply_message(midnight - 20_000, midnight, &utc), None);
        assert_eq!(clock.offset_ms(), -500);
    }

    #[test]
    fn messages_that_set_the_clock() {
        assert!(message_sets_clock("1", None, false));
        assert!(message_sets_clock("U", Some("75"), false));
        assert!(!message_sets_clock("U", Some("75"), true));
        for comm in ["60", "146", "151", "110"] {
            assert!(!message_sets_clock("U", Some(comm), false), "{comm}");
        }
        assert!(!message_sets_clock("0", None, false));
        assert!(!message_sets_clock("8", None, false));
    }

    // ibx#421: the version compare of the reference.
    #[test]
    fn version_cutoff_compare() {
        assert!(cutoff_applies("10401c", "10411"));
        assert!(cutoff_applies("10401c", "10401d"));
        assert!(cutoff_applies("10401", "10401a"));
        assert!(!cutoff_applies("10401c", "10401c"));
        assert!(!cutoff_applies("10401c", "10401"));
        assert!(!cutoff_applies("10401c", "10391z"));
        assert!(!cutoff_applies("10401c", "1041"));
        assert!(!cutoff_applies("10401c", "10411A"));
    }

    #[test]
    fn version_cutoff_warning_text() {
        assert_eq!(own_version(), "10401c");
        assert_eq!(
            version_cutoff_warning(Some("10411"), Some("20261201")).unwrap(),
            "The version of the application you are running, 1040.1, needs to be upgraded, as it will be \
             desupported on 20261201. The minimum supported version at that time will be 1041.1. "
        );
        assert_eq!(version_cutoff_warning(Some("10401c"), Some("20261201")), None);
        assert_eq!(version_cutoff_warning(None, None), None);
    }

    // ibx#421: the captured paper feature list has SECDEFTA, not DENYAPI
    // or NIGHTLY.
    #[test]
    fn api_features_of_the_feature_list() {
        let f = ApiFeatures::parse("1DAYSORDER,APIELOG,ISLAND2NASDAQ,SECDEFTA,SCALEUSLOT");
        assert_eq!(f, ApiFeatures { deny_api: false, matching_symbols: true, nightly: false,
            no_magnifier_fix: false, island_to_nasdaq: true });
        let f = ApiFeatures::parse("DENYAPI,NIGHTLY,SECDEFTA:x,NOMAGNFIX");
        assert_eq!(f, ApiFeatures { deny_api: true, matching_symbols: false, nightly: true,
            no_magnifier_fix: true, island_to_nasdaq: false });
    }

    #[test]
    fn backfill_years() {
        assert_eq!(max_backfill_years(Some("199")), 199);
        assert_eq!(max_backfill_years(Some("0")), 1);
        assert_eq!(max_backfill_years(None), 1);
        assert_eq!(backfill_years_refusal("2 y", 1).unwrap(),
            "Historical data request for 2 year(s) rejected. Max API Backfill Years=1");
        assert_eq!(backfill_years_refusal("1 y", 1), None);
        assert_eq!(backfill_years_refusal("300 d", 0), None);
    }
}
