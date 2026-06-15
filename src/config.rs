/// Client version identifiers.
pub const IB_BUILD: &str = "10401";
pub const IB_VERSION: &str = "c";
pub const IB_ENCODED: &str = "17.0.10.0.101/W/en_US/G";

/// Auth server endpoints.
pub const CCP_HOSTS: &[&str] = &[
    "cdc1.ibllc.com",
    "ndc1.ibllc.com",
];

/// Network ports.
pub fn misc_port() -> u16 {
    std::env::var("IBX_MISC_PORT").ok().and_then(|s| s.parse().ok()).unwrap_or(4000)
}
pub fn farm_host_override() -> Option<String> {
    std::env::var("IBX_FARM_HOST").ok()
}
pub const AUTH_PORT: u16 = 4001;

/// Heartbeat intervals (seconds).
pub const CCP_HEARTBEAT: u64 = 10;
pub const FARM_HEARTBEAT: u64 = 30;

/// Recv buffer sizes (bytes).
pub const CCP_RECV_BUF: usize = 8192;
pub const FARM_RECV_BUF: usize = 32768;
pub const FIX_RECV_BUF: usize = 4096;

/// Timeouts (seconds).
pub const TIMEOUT_FIX_LOGON: f64 = 10.0;
pub const TIMEOUT_FIX_READ: f64 = 30.0;
pub const TIMEOUT_FARM_LOGON: f64 = 5.0;
pub const TIMEOUT_SSL_AUTH: u64 = 20;
pub const TIMEOUT_FARM_CONNECT: u64 = 8;

/// Protocol version.
pub const NS_VERSION: u32 = 50;
pub const NS_VERSION_MIN: u32 = 38;

/// Stack-allocated FIX timestamp ("YYYYMMDD-HH:MM:SS"). Zero heap allocation.
pub struct TimestampBuf {
    buf: [u8; 17],
}

impl std::ops::Deref for TimestampBuf {
    type Target = str;
    #[inline]
    fn deref(&self) -> &str {
        // SAFETY: buf is all ASCII digits, '-', and ':'
        unsafe { std::str::from_utf8_unchecked(&self.buf) }
    }
}

impl std::fmt::Display for TimestampBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self)
    }
}

/// FIX-compliant UTC timestamp without chrono dependency. Zero heap allocation.
pub fn chrono_free_timestamp() -> TimestampBuf {
    use std::time::SystemTime;
    let dur = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap();
    let secs = dur.as_secs();
    let days = secs / 86400;
    let time_secs = secs % 86400;
    let hours = time_secs / 3600;
    let minutes = (time_secs % 3600) / 60;
    let seconds = time_secs % 60;
    let (year, month, day) = days_to_ymd(days);
    // Write directly into a fixed buffer: "YYYYMMDD-HH:MM:SS"
    let mut buf = [b'0'; 17];
    write_u2(&mut buf[0..], (year / 100) as u8);
    write_u2(&mut buf[2..], (year % 100) as u8);
    write_u2(&mut buf[4..], month as u8);
    write_u2(&mut buf[6..], day as u8);
    buf[8] = b'-';
    write_u2(&mut buf[9..], hours as u8);
    buf[11] = b':';
    write_u2(&mut buf[12..], minutes as u8);
    buf[14] = b':';
    write_u2(&mut buf[15..], seconds as u8);
    TimestampBuf { buf }
}

/// Write a u8 as 2 zero-padded decimal digits into a byte slice.
#[inline]
fn write_u2(buf: &mut [u8], val: u8) {
    buf[0] = b'0' + val / 10;
    buf[1] = b'0' + val % 10;
}

/// Format unix timestamp (seconds) to IB's "YYYYMMDD HH:MM:SS" format (UTC).
pub fn unix_to_ib_datetime(secs: i64) -> String {
    let secs = secs as u64;
    let days = secs / 86400;
    let time_secs = secs % 86400;
    let hours = time_secs / 3600;
    let minutes = (time_secs % 3600) / 60;
    let seconds = time_secs % 60;
    let (year, month, day) = days_to_ymd(days);
    format!(
        "{:04}{:02}{:02} {:02}:{:02}:{:02}",
        year, month, day, hours, minutes, seconds
    )
}

/// Format unix timestamp (UTC seconds) to "YYYYMMDD-HH:MM:SS" — the dash-joined
/// form the gateway requires for a time-precise good-till expiry (tag 126).
/// Distinct from `unix_to_ib_datetime` (space-joined) which other callers use.
pub fn unix_to_ib_utc_dash(secs: i64) -> String {
    let secs = secs.max(0) as u64;
    let days = secs / 86400;
    let t = secs % 86400;
    let (year, month, day) = days_to_ymd(days);
    format!(
        "{:04}{:02}{:02}-{:02}:{:02}:{:02}",
        year, month, day, t / 3600, (t % 3600) / 60, t % 60
    )
}

/// A parsed good-till expiry: either a calendar date (no time) or a precise
/// instant in UTC. The two are mutually exclusive on the wire — date-only is
/// emitted as tag 432, time-precise as tag 126 (UTC).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IbExpiry {
    /// Date with no time component. Packed `YYYYMMDD` (e.g. 20260620).
    DateOnly(u32),
    /// Precise instant, unix seconds (UTC).
    Instant(i64),
}

/// Parse a user-supplied `good_till_date` / `good_after_time` string into an
/// `IbExpiry`. Returns `Ok(None)` for an empty string.
///
/// Accepted input forms (the same the official API accepts):
///   - `YYYYMMDD`                          → date-only
///   - `YYYYMMDD HH:MM:SS`                 → time, no timezone
///   - `YYYYMMDD-HH:MM:SS`                 → time, no timezone (dash separator)
///   - `YYYYMMDD HH:MM:SS <IANA zone>`     → time in a named zone (e.g. `US/Eastern`)
///
/// A named timezone is converted to UTC with DST applied (matching what the
/// gateway does). A time with no timezone is interpreted as UTC and logged —
/// the gateway's implied-timezone behavior is deprecated, so callers should
/// pass an explicit zone or UTC.
pub fn parse_ib_expiry(input: &str) -> Result<Option<IbExpiry>, String> {
    let s = input.trim();
    if s.is_empty() {
        return Ok(None);
    }
    if s.len() < 8 || !s.as_bytes()[..8].iter().all(|b| b.is_ascii_digit()) {
        return Err(format!("expiry '{}': must start with YYYYMMDD", input));
    }
    let ymd: u32 = s[..8].parse().unwrap(); // 8 ascii digits — infallible
    let year: i16 = s[0..4].parse().unwrap();
    let month: i8 = s[4..6].parse().unwrap();
    let day: i8 = s[6..8].parse().unwrap();

    // Validate the date regardless of whether a time follows.
    let date = jiff::civil::Date::new(year, month, day)
        .map_err(|e| format!("expiry '{}': {}", input, e))?;

    // Strip the date, then an optional `-` or whitespace separator before the time.
    let rest = s[8..].strip_prefix('-').unwrap_or(&s[8..]).trim();
    if rest.is_empty() {
        return Ok(Some(IbExpiry::DateOnly(ymd)));
    }

    // Split the time token from an optional trailing timezone token.
    let mut it = rest.splitn(2, char::is_whitespace);
    let time_str = it.next().unwrap();
    let tz = it.next().map(str::trim).filter(|t| !t.is_empty());

    let tp: Vec<&str> = time_str.split(':').collect();
    if tp.len() != 3 {
        return Err(format!("expiry '{}': time must be HH:MM:SS", input));
    }
    let parse_u = |p: &str, what: &str| -> Result<i8, String> {
        p.parse::<i8>()
            .map_err(|_| format!("expiry '{}': invalid {}", input, what))
    };
    let (h, mi, sec) = (
        parse_u(tp[0], "hour")?,
        parse_u(tp[1], "minute")?,
        parse_u(tp[2], "second")?,
    );
    let time =
        jiff::civil::Time::new(h, mi, sec, 0).map_err(|e| format!("expiry '{}': {}", input, e))?;
    let dt = date.to_datetime(time);

    let zone = match tz {
        Some(z) => z,
        None => {
            log::warn!(
                "good-till expiry '{}' has a time but no timezone; interpreting as UTC. \
                 Pass an explicit zone (e.g. 'US/Eastern') or UTC.",
                input
            );
            "UTC"
        }
    };
    let zoned = dt
        .in_tz(zone)
        .map_err(|e| format!("expiry '{}': unknown timezone '{}': {}", input, zone, e))?;
    Ok(Some(IbExpiry::Instant(zoned.timestamp().as_second())))
}

/// Convert days since Unix epoch to (year, month, day).
pub fn days_to_ymd(days: u64) -> (u64, u64, u64) {
    // Algorithm from Howard Hinnant
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod expiry_tests {
    use super::*;

    fn instant(s: &str) -> i64 {
        match parse_ib_expiry(s).unwrap().unwrap() {
            IbExpiry::Instant(secs) => secs,
            other => panic!("expected Instant, got {:?}", other),
        }
    }

    #[test]
    fn empty_is_none() {
        assert_eq!(parse_ib_expiry("").unwrap(), None);
        assert_eq!(parse_ib_expiry("   ").unwrap(), None);
    }

    #[test]
    fn date_only() {
        assert_eq!(
            parse_ib_expiry("20260620").unwrap(),
            Some(IbExpiry::DateOnly(20260620))
        );
    }

    #[test]
    fn named_zone_converts_with_dst() {
        // June -> US/Eastern is EDT (UTC-4): 18:00 local == 22:00 UTC.
        // Matches the gateway capture (ib-agent#158).
        let eastern = instant("20260620 18:00:00 US/Eastern");
        let utc = instant("20260620 22:00:00 UTC");
        assert_eq!(eastern, utc, "EDT 18:00 must equal 22:00 UTC");
    }

    #[test]
    fn no_timezone_is_utc() {
        // Both separators accepted; absent zone treated as UTC.
        let dash = instant("20260620-18:00:00");
        let space = instant("20260620 18:00:00");
        let utc = instant("20260620 18:00:00 UTC");
        assert_eq!(dash, space);
        assert_eq!(dash, utc);
    }

    #[test]
    fn instant_round_trips_to_wire() {
        // parse -> seconds -> tag 126 wire string must be the dash UTC form.
        let secs = instant("20260620 18:00:00 US/Eastern");
        assert_eq!(unix_to_ib_utc_dash(secs), "20260620-22:00:00");
    }

    #[test]
    fn rejects_bad_input() {
        assert!(parse_ib_expiry("2026").is_err());
        assert!(parse_ib_expiry("20260620 18:00").is_err()); // needs seconds
        assert!(parse_ib_expiry("20261320").is_err()); // month 13
        assert!(parse_ib_expiry("20260620 18:00:00 Mars/Olympus").is_err());
    }
}
