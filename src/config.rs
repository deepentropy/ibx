/// Client version identifiers.
pub const IB_BUILD: &str = "10401";
pub const IB_VERSION: &str = "c";
pub const IB_ENCODED: &str = "17.0.10.0.101/W/fr/G";

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
pub const TIMEOUT_FARM_CONNECT: u64 = 30;

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
