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

/// Convert (year, month, day) to days since Unix epoch. Inverse of `days_to_ymd`.
pub fn ymd_to_days(y: u64, m: u64, d: u64) -> u64 {
    // Algorithm from Howard Hinnant (days_from_civil)
    let y = if m <= 2 { y - 1 } else { y };
    let era = y / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Parse an ibapi-style datetime string to unix seconds, treated as UTC.
/// Accepts "YYYYMMDD HH:MM:SS", "YYYYMMDD-HH:MM:SS", or bare "YYYYMMDD".
/// Anything after the seconds field (e.g. a timezone suffix) is ignored.
/// Returns 0 for empty or unparseable input.
pub fn ib_datetime_to_unix(s: &str) -> i64 {
    let s = s.trim();
    if s.len() < 8 || !s.is_char_boundary(8) {
        return 0;
    }
    let (date, rest) = s.split_at(8);
    let y = match date[0..4].parse::<u64>() { Ok(v) => v, Err(_) => return 0 };
    let m = match date[4..6].parse::<u64>() { Ok(v) => v, Err(_) => return 0 };
    let d = match date[6..8].parse::<u64>() { Ok(v) => v, Err(_) => return 0 };
    if y < 1970 || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return 0;
    }
    let mut secs = (ymd_to_days(y, m, d) * 86400) as i64;
    let time = rest.trim_start_matches(|c| c == ' ' || c == '-');
    if time.len() >= 8 && time.is_char_boundary(8) {
        let hh = time[0..2].parse::<i64>().unwrap_or(0);
        let mm = time[3..5].parse::<i64>().unwrap_or(0);
        let ss = time[6..8].parse::<i64>().unwrap_or(0);
        if (0..24).contains(&hh) && (0..60).contains(&mm) && (0..60).contains(&ss) {
            secs += hh * 3600 + mm * 60 + ss;
        }
    }
    secs
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
mod gtd_datetime_tests {
    use super::*;

    #[test]
    fn round_trips_with_unix_to_ib_datetime() {
        for secs in [0i64, 86399, 1781199865, 4102444800] {
            let s = unix_to_ib_datetime(secs);
            assert_eq!(ib_datetime_to_unix(&s), secs, "round-trip failed for {s}");
        }
    }

    #[test]
    fn accepts_dash_and_space_separators() {
        assert_eq!(
            ib_datetime_to_unix("20260611-17:44:25"),
            ib_datetime_to_unix("20260611 17:44:25"),
        );
        assert!(ib_datetime_to_unix("20260611-17:44:25") > 0);
    }

    #[test]
    fn bare_date_is_midnight_utc() {
        let secs = ib_datetime_to_unix("20260611");
        assert_eq!(secs % 86400, 0);
        assert_eq!(unix_to_ib_datetime(secs), "20260611 00:00:00");
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(ib_datetime_to_unix(""), 0);
        assert_eq!(ib_datetime_to_unix("tomorrow"), 0);
        assert_eq!(ib_datetime_to_unix("2026061"), 0);
        assert_eq!(ib_datetime_to_unix("20261311"), 0);
    }

    #[test]
    fn ignores_timezone_suffix() {
        assert_eq!(
            ib_datetime_to_unix("20260611 17:44:25 UTC"),
            ib_datetime_to_unix("20260611 17:44:25"),
        );
    }
}
