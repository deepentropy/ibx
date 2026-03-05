/// IB Gateway version identifiers.
pub const IB_BUILD: &str = "10401";
pub const IB_VERSION: &str = "c";
pub const IB_ENCODED: &str = "17.0.10.0.101/W/fr/G";

/// CCP server endpoints.
pub const CCP_HOSTS: &[&str] = &[
    "cdc1.ibllc.com", // US East
    "ndc1.ibllc.com", // US secondary
];

/// Network ports.
pub const MISC_PORT: u16 = 4000;
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

/// NS protocol version.
pub const NS_VERSION: u32 = 50;
pub const NS_VERSION_MIN: u32 = 38;
