//! Gateway: authenticates a login and opens the connections a session runs on.

mod logon;
pub use logon::*;
mod second_factor;
use second_factor::*;

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use native_tls::TlsConnector;
use num_bigint::BigUint;
use zeroize::Zeroizing;

use std::net::ToSocketAddrs;

use crate::auth::crypto::strip_leading_zeros;
use crate::auth::dh::SecureChannel;
use crate::auth::session::{self, do_srp, do_soft_token};
use crate::config::*;
use crate::bridge::SharedState;
use crate::protocol::connection::Connection;
use crate::protocol::fix::{self, fix_build, fix_parse};
use crate::protocol::fixcomp;
use crate::protocol::ns;


/// The trading route a reconnect should use.
///
/// The auth server states one per account, and the reconnect uses it rather
/// than announcing a literal farm and connecting to the host the caller
/// configured, which would put a regional account on a farm it is not on. Empty means
/// no route was parsed, which is the case the literals
/// were there for; the initial connect falls back the same way.
pub(crate) fn reconnect_trading_route(auth: &ReconnectAuth) -> (String, String) {
    let host = if auth.trading_host.is_empty() {
        auth.host.clone()
    } else {
        auth.trading_host.clone()
    };
    // The farm the venue named for this account, and no literal behind it: a
    // session exists only where the venue named one, and the name a guess would
    // reach serves other accounts than this. Empty here would be this client
    // having lost what it was told, which a reconnect to somewhere else does
    // not repair.
    (host, auth.trading_farm.clone())
}

/// The connect-time credentials a reconnect needs, supplied by whichever
/// binding built the session.
///
/// A constructor parameter rather than a setter called afterwards: leaving
/// these blank for the caller to fill meant one binding filled them and the
/// other did not, and every reconnect scheduler silently refused to run on the
/// empty host. The compiler asks for them now.
pub struct CallerAuth {
    /// What this session runs under.
    pub settings: std::sync::Arc<crate::settings::SessionSettings>,
    /// Which host to open the session against.
    pub host: String,
    /// The login.
    pub username: String,
    /// Its password, zeroed when dropped.
    pub password: Zeroizing<String>,
    /// Whether this is a paper session. It decides one step of the logon and
    /// nothing after it.
    pub paper: bool,
    /// What the second-factor gate needs if the server bumps a reconnect back
    /// to SRP. Without these an unattended client cannot finish that handshake.
    pub code_provider: Option<session::CodeProvider>,
    /// How long to wait for the second factor before giving up.
    pub ib_key_timeout_secs: u64,
    /// Which kind of second factor to ask for.
    pub ib_key_token_sub_type: String,
}

/// Credentials cached for auto-reconnect (no SRP needed).
#[derive(Clone)]
pub struct ReconnectAuth {
    /// What this session runs under. Carried so a reconnect states what the
    /// session stated when it opened, rather than whatever the process holds
    /// by the time it drops.
    pub settings: std::sync::Arc<crate::settings::SessionSettings>,
    /// Which host to open the session against.
    pub host: String,
    /// When the session being rebuilt first logged in, in the venue's spelling.
    ///
    /// A reconnect that finds somebody on the account compares the two: later
    /// than this is another client, which took the account fairly and should
    /// keep it. Earlier is this session's own previous logon, not yet reaped,
    /// and taking that back is just finishing the reconnect.
    ///
    /// Empty means every session the venue names counts as another client,
    /// which is the careful reading: the reconnect gives the account up rather
    /// than taking it from somebody who may hold it fairly.
    pub logged_in_at: String,
    /// The other auth servers this session reached the venue through.
    ///
    /// Tried in order when the first cannot be reached. Every one was named by
    /// the venue on the way in, so none of them is a guess — a client that
    /// invented an address to fail over to would be reaching for a server
    /// nobody said was there.
    pub alternate_hosts: Vec<String>,
    /// The login.
    pub username: String,
    /// Wrapped in `Zeroizing` so the plaintext is wiped from memory on drop.
    pub password: Zeroizing<String>,
    /// Whether this is a paper session. It decides one step of the logon and
    /// nothing after it.
    pub paper: bool,
    /// What the second-factor gate needs if the server bumps a reconnect back
    /// to SRP. Without these an unattended client cannot finish that handshake.
    pub code_provider: Option<session::CodeProvider>,
    /// How long to wait for the second factor before giving up.
    pub ib_key_timeout_secs: u64,
    /// Which kind of second factor to ask for.
    pub ib_key_token_sub_type: String,
    /// The key the session was established with.
    pub session_key: BigUint,
    /// The token it resumes with.
    pub session_token: BigUint,
    /// Session id assigned by the venue.
    pub server_session_id: String,
    /// Which account this session opened under.
    ///
    /// A reconnect states the same opening sequence a first logon does, and
    /// the messages in it that name an account need one to name. A first logon
    /// reads it off the wire and falls back to the login name where the venue
    /// states none; a reconnect carries whichever of the two that left it
    /// with, so the rebuilt sequence states the same account the first one
    /// did rather than a different one — or none.
    pub account_id: String,
    /// What this machine identified itself as.
    pub hw_info: String,
    /// What the client string encoded to.
    pub encoded: String,
    /// Historical-data farm routing parsed from the auth-server response.
    /// Used by HMDS reconnect — empty when no HMDS route was parsed.
    pub hmds_host: String,
    /// Which historical farm the venue routed this session to.
    pub hmds_farm: String,
    /// Trading farm routing exactly as parsed from the same response — empty
    /// when the auth server stated none, rather than carrying the fallback the
    /// initial connect materializes. The distinction matters: an empty host
    /// lets the reconnect use whatever host the session has now, which a
    /// redirect may have changed since.
    pub trading_host: String,
    /// Which trading farm.
    pub trading_farm: String,
    /// Security-definition farm routing from the same response, empty when the
    /// venue stated none. The calendar rides this one, and without the route a
    /// connection that goes cannot be rebuilt.
    pub secdef_host: String,
    /// Which security-definition farm.
    pub secdef_farm: String,
    /// The port the venue named for each farm, where it named one. A reconnect
    /// that fell back to the configured port would dial somewhere the venue
    /// never routed this session to.
    pub trading_port: Option<u16>,
    /// The port stated for the historical farm.
    pub hmds_port: Option<u16>,
    /// The port stated for the security-definition farm.
    pub secdef_port: Option<u16>,
}

/// The suffix the venue adds when the session being told is the one held to
/// reading only.
const COMPETING_READ_ONLY: &str = "(RO)";

/// Read on until the venue states the key exchange, and answer with its fields.
///
/// A message it states of its own accord while this is awaited is read past —
/// it names a backup host without being asked, and ending the connection over
/// one gives up on a venue that has not refused anything. What is not read
/// past is what means something: an error is surfaced with its words, and a
/// retarget ends the attempt naming where it was sent instead.
///
/// Naming it is as far as this goes. Following one is done where the venue
/// issues them, which is after the connect request has said which account is
/// asking — by then it knows enough to send you elsewhere. One arriving before
/// that is reported and the attempt ends, rather than being read past and
/// waited out against a host that has already answered.
///
/// Bounded by the clock the auth socket is already given. The socket's own
/// timeout ends a venue that goes quiet; this ends one that keeps talking
/// without ever stating the exchange.
fn read_server_hello(tls: &mut impl std::io::Read, what: &str) -> io::Result<Vec<String>> {
    let deadline = std::time::Instant::now() + Duration::from_secs(TIMEOUT_SSL_AUTH);
    loop {
        if std::time::Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "{what}: no key exchange before the auth timeout, with the \
                     venue still speaking"
                ),
            ));
        }
        let (payload, _) = ns::ns_recv(tls, deadline)?;
        let text = String::from_utf8_lossy(&payload);
        let parts: Vec<&str> = text.split(';').collect();
        let msg_type: u32 = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);

        if msg_type == ns::NS_SECURE_ERROR || msg_type == ns::NS_ERROR_RESPONSE {
            return Err(session::error_the_venue_stated(
                &format!("{what} DH error"),
                msg_type,
                &parts[2..],
            ));
        }
        if msg_type == ns::NS_REDIRECT {
            let target = parts.get(2).unwrap_or(&"");
            return Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                format!("REDIRECT:{target}"),
            ));
        }
        if msg_type == ns::NS_SECURE_CONNECTION_START {
            return Ok(parts.get(2..).unwrap_or(&[]).iter().map(|s| s.to_string()).collect());
        }
        log::info!(
            "{what}: received msg id {msg_type} while awaiting the key exchange; read past"
        );
    }
}

/// The error a reconnect ends with when the client took it back — a stop or
/// a spent recovery budget. Stated as the client's own doing: read as an
/// answer from the venue, the retry ladder would try it again.
fn cancelled_by_the_client(what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::Interrupted,
        format!("{what} cancelled by the client"),
    )
}

/// Whether a failed connect means nothing was reached, rather than something
/// answering and saying no.
///
/// The difference decides whether another door is worth knocking on: a door
/// that cannot be reached says nothing about the account, and one that refuses
/// says everything, at every door.
fn nobody_answered(e: &io::Error) -> bool {
    use crate::reliability::retry::DisconnectReason;
    matches!(
        DisconnectReason::from_error(e),
        DisconnectReason::Transport | DisconnectReason::NoResponse,
    )
}

/// How a reconnect says it found somebody else on the account.
///
/// Read back by the failover, which is why it is one spelling rather than two,
/// and classified as a takeover by the retry ladder, which is why it says
/// "competing".
const TOOK_THE_ACCOUNT: &str = "competing logon:";

/// Full gateway connection.
pub struct Gateway {
    /// The account this session acts for.
    pub account_id: String,
    /// Every account this login holds, the first being [`Gateway::account_id`].
    /// A login with one account holds one entry.
    pub accounts: Vec<String>,
    /// The token it resumes with.
    pub session_token: BigUint,
    /// Session ID surfaced to webapp REST clients as `x-ccp-session-id`.
    /// Sourced from the post-auth FIX logon ACK, falling back to the locally generated
    /// session ID when the server does not echo one back.
    pub server_session_id: String,
    /// What the trading connection authenticates with.
    pub ccp_token: String,
    /// How often the venue expects to hear from this client.
    pub heartbeat_interval: u64,
    /// Stored for farm reconnection.
    pub hw_info: String,
    /// What the client string encoded to.
    pub encoded: String,
    /// Raw soft dollar tier data from CCP logon tag 6560.
    pub raw_soft_dollar_tiers: String,
    /// Raw family code data from CCP logon tag 6823.
    pub raw_family_codes: String,
    /// Raw news provider data from CCP logon tag 6830.
    pub raw_news_providers: String,
    /// Raw per-security-type order permissions from CCP logon tag 6652.
    pub raw_order_permissions: String,
    /// What the venue says it has turned on for this session, as it states
    /// them at logon (tag 6542).
    ///
    /// Server behaviour is conditioned on these rather than on any client
    /// setting; the spelling a Nasdaq listing comes back under is one such
    /// case. Kept so that what this client does can be told apart from what
    /// the account was permitted to do.
    pub enabled_features: String,
    /// Raw enabled-feature token list from CCP logon tag 6542.
    pub raw_enabled_features: String,
    /// White branding ID from CCP logon (empty for standard accounts).
    pub white_branding_id: String,
    /// Logical-name → host URL map pushed by the server during logon. Empty when no
    /// URL set was pushed (callers should then fall back to a documented literal,
    /// e.g. `api.ibkr.com` for `region_dam`).
    pub misc_urls: std::collections::HashMap<String, String>,
    /// Historical-data farm routing parsed from the auth-server response,
    /// retained for HMDS reconnect.
    pub hmds_host: String,
    /// Which historical farm the venue routed this session to.
    pub hmds_farm: String,
    /// Another session already logged in on this account when this one
    /// connected, as the venue stated it. `None` when this session is alone.
    pub competing: Option<CompetingSession>,
    /// When this session logged in, in the spelling the venue uses for the
    /// same fact about somebody else.
    ///
    /// The two are comparable, which is what makes a takeover tellable from a
    /// ghost: a session that logged in after this one is another client, and a
    /// session that logged in before it is this one's own, still being reaped.
    pub logged_in_at: String,
    /// The auth servers this session's logon ran through, in the order it
    /// reached them: the one it knocked on, then each one it was sent to.
    ///
    /// Every entry is a host the venue named, so a reconnect that cannot reach
    /// the one it was last on has somewhere else to try that was stated rather
    /// than guessed. A session that was never redirected has one.
    pub auth_hosts: Vec<String>,
    /// Trading farm routing from the same response, retained so the trading
    /// reconnect uses the route the auth server gave rather than a literal.
    pub trading_host: String,
    /// Which trading farm.
    pub trading_farm: String,
    /// Security-definition farm routing from the same response, retained so a
    /// connection that goes can be rebuilt. The calendar rides this one.
    pub secdef_host: String,
    /// Which security-definition farm.
    pub secdef_farm: String,
    /// The port the venue named for the trading farm, where it named one.
    pub trading_port: Option<u16>,
    /// The port stated for the historical farm.
    pub hmds_port: Option<u16>,
    /// The port stated for the security-definition farm.
    pub secdef_port: Option<u16>,
}

/// Which farm a connection is to, which decides two separate numbers the
/// logon carries and which were held as one for as long as there were only
/// two farms.
///
/// The logon states a version, and the routing request that follows states
/// which service is being asked for. They are not the same number: every
/// service logs on at seventeen except market data, which logs on at
/// eighteen, while the service number counts from zero across all of them.
/// Held as one value, a third farm cannot be stated at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Farm {
    /// Quotes, depth and the ticks that follow them.
    MarketData,
    /// Bars, historical ticks and schedules.
    Historical,
    /// Contract definitions, and the calendar that rides with them.
    SecurityDefinition,
}

impl Farm {
    /// The version this farm's logon states.
    pub fn login_version(self) -> u32 {
        match self {
            Self::MarketData => 18,
            Self::Historical | Self::SecurityDefinition => 17,
        }
    }

    /// The service this farm is, as the routing request states it.
    pub fn channel_id(self) -> &'static str {
        match self {
            Self::MarketData => "1",
            Self::Historical => "2",
            Self::SecurityDefinition => "4",
        }
    }
}

/// A session: the [`Gateway`], and the connections it opened.
///
/// Named fields rather than a five-element tuple: two are optional and none
/// would state which farm it belongs to.
pub struct Session {
    /// The session itself.
    pub gateway: Gateway,
    /// Quotes, depth and the ticks that follow them.
    pub market_data: Connection,
    /// Orders, positions, account figures and contract lookups.
    pub trading: Connection,
    /// Bars, historical ticks and tick-by-tick streams.
    pub historical: Option<Connection>,
    /// Contract definitions and the calendar that rides with them.
    pub security_definition: Option<Connection>,
}

/// Where a farm connection goes: the host the venue named unless the session
/// states one of its own, and the port the venue named unless it named none.
///
/// Every farm this opens, not the market-data one alone. `Farm` does name the
/// connection — `MarketData` is the quote connection, and the trading
/// connection is the CCP one, which never comes through here — so scoping this
/// is a one-line change and deliberately not made: the setting is an escape
/// hatch for pointing a session somewhere else, and moving one of three
/// connections is not what anyone reaches for it to do. Documented as moving
/// them all, which is what it does.
///
/// Named on its own because it is the whole of what `market_data_host` and
/// `port` do, and a destination that is only computed on the way into a socket
/// cannot be read back without opening one.
pub(crate) fn farm_destination(
    settings: &crate::settings::SessionSettings,
    host: &str,
    stated_port: Option<u16>,
) -> (String, u16) {
    (
        settings.market_data_host.clone().unwrap_or_else(|| host.to_string()),
        stated_port.unwrap_or(settings.port),
    )
}

/// Connect to a data farm: key exchange → encrypted logon → token auth → routing →
/// Connection.
pub fn connect_farm(
    settings: &crate::settings::SessionSettings,
    host: &str,
    farm_id: &str,
    username: &str,
    password: &str,
    paper: bool,
    server_session_id: &str,
    session_key: &BigUint,
    hw_info: &str,
    encoded: &str,
    farm: Farm,
    stated_port: Option<u16>,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> io::Result<Connection> {
    let cancelled = || {
        cancel.is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed))
    };
    if cancelled() {
        return Err(cancelled_by_the_client(&format!("{farm_id} reconnect")));
    }
    let (farm_host, port) = farm_destination(settings, host, stated_port);
    log::info!("Connecting to {farm_id} {farm_host}:{port}");
    let addr = format!("{farm_host}:{port}")
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "DNS resolution failed"))?;
    let farm_tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(TIMEOUT_FARM_CONNECT))
        .map_err(|e| io::Error::new(e.kind(), format!("{farm_id} TCP connect: {e}")))?;
    farm_tcp.set_nodelay(true)?;
    farm_tcp.set_read_timeout(Some(Duration::from_secs(TIMEOUT_FARM_CONNECT)))?;
    // Bounded both ways, as the auth dials are: a write that never drains
    // would otherwise hold the handshake open for as long as the peer likes.
    farm_tcp.set_write_timeout(Some(Duration::from_secs(TIMEOUT_FARM_CONNECT)))?;

    // Key exchange (raw TCP)
    let mut channel = SecureChannel::new();
    let dh_msg = channel.build_secure_connect(NS_VERSION, NS_VERSION);
    let mut stream = farm_tcp;
    stream.write_all(&dh_msg)?;

    let (payload, _) = ns::ns_recv(
        &mut stream,
        std::time::Instant::now() + Duration::from_secs(TIMEOUT_FARM_CONNECT),
    )?;
    let text = String::from_utf8_lossy(&payload);
    let parts: Vec<&str> = text.split(';').collect();
    let msg_type: u32 = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    if msg_type != ns::NS_SECURE_CONNECTION_START {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{farm_id} DH: expected 533, got {msg_type}"),
        ));
    }
    channel.process_server_hello(parts.get(2..).unwrap_or(&[]))?;
    log::info!("{farm_id} key exchange complete");

    // Checked again before the logon goes out: past this point the venue
    // holds an authenticated connection, and one the client has stopped
    // asking for must not be opened.
    if cancelled() {
        return Err(cancelled_by_the_client(&format!("{farm_id} reconnect")));
    }

    // Encrypted logon
    let farm_session_id = if server_session_id.is_empty() {
        session::get_session_id()
    } else {
        server_session_id.to_string()
    };
    let logon_bytes = build_farm_encrypted_logon(
        settings,
        &mut channel, username, paper, farm_id,
        &farm_session_id, session_key, hw_info, encoded, farm.login_version(),
    );
    stream.write_all(&logon_bytes)?;
    log::info!("{farm_id} encrypted logon sent");

    // Logon exchange: challenge → token auth → logon ACK
    let read_mac_key = channel.key_block().map(|kb| kb[84..104].to_vec()).unwrap_or_default();
    let initial_read_iv = channel.key_block().map(|kb| kb[48..64].to_vec()).unwrap_or_default();
    let FarmLogon { read_iv, sign_iv, remaining: logon_remaining, heartbeat_secs: stated_heartbeat } =
        farm_logon_exchange(
        &mut stream, &mut channel, session_key, username, password,
        &read_mac_key, &initial_read_iv,
    )?;
    log::info!("{} logon exchange complete, {} bytes remaining", farm_id, logon_remaining.len());

    let sign_mac_key = channel.key_block().map(|kb| kb[64..84].to_vec()).unwrap_or_default();

    // Send routing table request after logon.
    let channel_id = farm.channel_id();
    let now = chrono_free_timestamp();
    let routing_msg = fix_build(&[
        (fix::TAG_MSG_TYPE, "U"),
        (fix::TAG_SENDING_TIME, &now),
        (6040, "112"),
        (6556, channel_id),
    ], 1);
    let wrapped = fixcomp::fixcomp_build(&routing_msg);

    let (signed, new_sign_iv) = fix::fix_sign(&wrapped, &sign_mac_key, &sign_iv);
    stream.write_all(&signed)?;
    let final_sign_iv = new_sign_iv;
    log::info!("{farm_id} sent routing request (6556={channel_id})");

    // Read routing response. Poll with a short timeout and return as soon as
    // the complete routing reply is buffered. The
    // deadline remains as the worst-case fallback for an answer that is not
    // coming.
    stream.set_read_timeout(Some(Duration::from_millis(100)))?;
    let resp_buf = read_routing_response(
        &mut stream,
        logon_remaining,
        std::time::Instant::now() + Duration::from_secs(5),
    )?;
    log::info!("{} routing response: {} bytes", farm_id, resp_buf.len());

    // Create Connection (switches to non-blocking), inject routing bytes
    let mut conn = Connection::new_raw(stream)?;
    conn.set_keys(sign_mac_key, final_sign_iv, read_mac_key, read_iv);
    conn.heartbeat_secs = stated_heartbeat;
    conn.seq = 1; // routing request was seq=1; next send_fix will be seq=2

    // The logon remainder and routing response stay in wire order for IV chaining.
    if !resp_buf.is_empty() {
        conn.inject_buf(&resp_buf);
    }
    // Extract and process all frames (unsign + respond to TestRequests, like Python).
    let frames = conn.extract_frames();
    for frame in &frames {
        match frame {
            crate::protocol::connection::Frame::FixComp(raw) => {
                let Some(unsigned) = conn.unsign(raw) else { continue };
                let inner = fixcomp::fixcomp_decompress(&unsigned).unwrap_or_else(|e| {
                    log::warn!("{farm_id}: dropping malformed FIXCOMP frame: {e}");
                    Vec::new()
                });
                for m in &inner {
                    let parsed = fix_parse(m);
                    let mt = parsed.get(&35).map(|s| s.as_str()).unwrap_or("");
                    log::debug!("{farm_id} routing compressed inner 35={mt}");
                    if mt == "1" {
                        let test_id = parsed.get(&112).cloned().unwrap_or_default();
                        let ts = chrono_free_timestamp();
                        let _ = conn.send_fix(&[
                            (fix::TAG_MSG_TYPE, "0"),
                            (fix::TAG_SENDING_TIME, &ts),
                            (112, &test_id),
                        ]);
                    }
                }
            }
            crate::protocol::connection::Frame::Fix(raw) => {
                let Some(unsigned) = conn.unsign(raw) else { continue };
                let parsed = fix_parse(&unsigned);
                let mt = parsed.get(&35).map(|s| s.as_str()).unwrap_or("");
                log::debug!("{farm_id} routing FIX 35={mt}");
                if mt == "1" {
                    let test_id = parsed.get(&112).cloned().unwrap_or_default();
                    let ts = chrono_free_timestamp();
                    let _ = conn.send_fix(&[
                        (fix::TAG_MSG_TYPE, "0"),
                        (fix::TAG_SENDING_TIME, &ts),
                        (112, &test_id),
                    ]);
                }
            }
            crate::protocol::connection::Frame::Binary(raw) => {
                let Some(unsigned) = conn.unsign(raw) else { continue };
                log::info!("{} routing 8=O: {} bytes", farm_id, raw.len());
                // The table this farm answered with. Read here because this
                // is the only place it is sent: once, on the connection that
                // asked for it.
                let text = String::from_utf8_lossy(&unsigned);
                let body = text.split_once("6556=").map_or("", |(_, rest)| {
                    rest.split_once('\x01').map_or("", |(_, table)| table)
                });
                let table = crate::protocol::routing::RoutingTable::parse(body);
                if !table.is_empty() {
                    // Which farms serve a book, next to the farm this session
                    // is on: a book served from another farm is asked for on a
                    // connection that does not serve it.
                    let book_farms = table.book_farms();
                    log::info!(
                        "{farm_id} routing table: {} markets across {} farms; \
                         books served from {:?}",
                        table.rows().len(), table.farms().len(), book_farms,
                    );
                    conn.routing = table;
                }
                if let Ok(dir) = std::env::var("IBX_CAPTURE_WIRE")
                    && dir != "1"
                {
                    let path = format!("{dir}/routing-{farm_id}.bin");
                    let _ = std::fs::write(&path, &unsigned);
                    log::info!("{farm_id} routing table written to {path}");
                }
            }
            crate::protocol::connection::Frame::Control(raw) => {
            // 8=1 / 8=X control state — extracted, not routed.
                log::debug!("{} ignoring control frame: {} bytes", farm_id, raw.len());
            }
        }
    }
    if !frames.is_empty() {
        log::info!("{} post-logon frames: {} frames, seq now {}", farm_id, frames.len(), conn.seq);
    }
    Ok(conn)
}

/// The hosts left to try, given every host this session reached the venue
/// through and the one being reconnected to now.
///
/// Order is the order they were reached in. The host being tried is not among
/// them: retrying the one that just failed is not a second attempt.
fn alternates_to(seen: &[String], current: &str) -> Vec<String> {
    seen.iter().filter(|host| *host != current).cloned().collect()
}

/// How many backup peers the venue keeps behind a host. Two, and not a number
/// anything a caller sets moves.
const HOT_BACKUP_PEERS: u32 = 2;

/// The venue's own backup peers for a host, which it names by rewriting the
/// host rather than by announcing them anywhere.
///
/// The first label of `x.example.com` becomes `x-hb1` and `x-hb2`, on the same
/// port. A host already carrying one is taken back to its primary first, so a
/// walk that stepped onto a backup does not go looking for backups of a
/// backup. A literal address has no label to rewrite and is left alone.
///
/// These come first when the host stops answering: same region, same account,
/// and nothing has to redirect the session to reach them. One that does not
/// exist does not resolve, and a host that cannot be reached is exactly what
/// the walk already steps past — which is how the venue tolerates their
/// absence too.
fn hot_backup_peers(host: &str) -> Vec<String> {
    if host.parse::<std::net::IpAddr>().is_ok() {
        return Vec::new();
    }
    let (label, domain) = match host.split_once('.') {
        Some((label, rest)) => (label, format!(".{rest}")),
        None => (host, String::new()),
    };
    let primary = match label.to_ascii_lowercase().find("-hb") {
        Some(at) => &label[..at],
        None => label,
    };
    if primary.is_empty() {
        return Vec::new();
    }
    (1..=HOT_BACKUP_PEERS).map(|n| format!("{primary}-hb{n}{domain}")).collect()
}

/// Where a first connect knocks next, once the host the caller named has not
/// answered.
///
/// The host's own backups first. They stand for the host that was asked for,
/// so they are tried whatever host that is, and reaching one needs no
/// redirect.
///
/// Then the doors this client ships. The venue runs one per region and any of
/// them will route a session to where its account lives, so a session whose
/// default door is unreachable knocks on the next rather than giving up on a
/// venue that is up. Only when the caller is standing at one of them: a caller
/// that named its own host meant that host, and being sent to another region
/// is not failover, it is the session going somewhere nobody asked for.
fn doors_after(host: &str) -> Vec<String> {
    let mut doors = hot_backup_peers(host);
    if crate::config::CCP_HOSTS.contains(&host) {
        doors.extend(alternates_to(
            &crate::config::CCP_HOSTS.iter().map(|h| h.to_string()).collect::<Vec<_>>(),
            host,
        ));
    }
    doors
}

/// Reconnect to the CCP (order/auth) server using cached session credentials.
/// Performs TLS + DH + CONNECT_REQUEST, then attempts SOFT_TOKEN auth with cached K.
/// If the server signals at AUTH_START that it requires full SRP, transparently
/// falls back to a fresh SRP handshake using the cached `username`/`password`
/// in `ReconnectAuth` (the same path used by `Gateway::connect`).
///
/// `cancel` is the engine taking the attempt back — a stop, or a recovery
/// budget that is spent. Checked between the phases of the handshake: a
/// reconnect is what the engine schedules, and one it has stopped scheduling
/// must not go on opening a session at the venue.
pub fn reconnect_ccp(
    auth: &ReconnectAuth,
    cancel: &std::sync::atomic::AtomicBool,
) -> io::Result<Connection> {
    let token_hash = token_short_hash(&auth.session_token);
    let first = reconnect_ccp_attempt(auth, &token_hash, &auth.host, 0, cancel);
    let Err(why) = first else { return first };
    // The host's own backups before the hosts this session was sent to: they
    // stand in for the one that stopped answering without the session having
    // to be routed anywhere else.
    let mut hosts = hot_backup_peers(&auth.host);
    hosts.extend(auth.alternate_hosts.iter().cloned());
    failover(&auth.host, why, &hosts, |host| {
        reconnect_ccp_attempt(auth, &token_hash, host, 0, cancel)
    })
}

/// The hosts a session reached the venue through, tried in that order when
/// the one it was on stopped answering, until one of them answers.
///
/// The walk is only for a door that did not open. Every host answers for the
/// same account, so an answer says the same thing at all of them: walking on
/// after a refusal re-asks the same question with the same credentials, which
/// is how an account gets locked rather than connected — and if the answer is
/// that another client holds the account, walking on takes it back from a
/// client that holds it fairly, the fight that answer exists to end.
fn failover(
    first_host: &str,
    first: io::Error,
    hosts: &[String],
    mut attempt: impl FnMut(&str) -> io::Result<Connection>,
) -> io::Result<Connection> {
    if !nobody_answered(&first) {
        return Err(first);
    }

    let mut last = first;
    for host in hosts {
        log::warn!("{first_host} did not answer ({last}); trying {host}");
        match attempt(host) {
            Ok(conn) => {
                log::info!("reconnected on {host}");
                return Ok(conn);
            }
            // A door that answered and refused has answered for the account,
            // not for itself, and the reason it stated is the one the caller
            // hears. Only a door that did not open sends the walk on.
            Err(e) if !nobody_answered(&e) => return Err(e),
            Err(e) => {
                log::warn!("{host} did not answer either ({e})");
                last = e;
            }
        }
    }
    Err(last)
}

/// Drain the init burst the venue answers a logon with.
///
/// The burst has no terminator the client can wait for — the venue sends it
/// back to back and then goes quiet — so silence is the end, and the socket
/// carries a read timeout of one `poll`, making each quiet read stand for one
/// `poll` of nothing. Ending the drain on the first quiet read meant a single
/// gap inside a burst arriving over a slow path read as the end of it: the
/// routing tags sat in the bytes never drained, and the venue was blamed for
/// naming no route it had named. The drain ends once `gap` worth of quiet
/// reads arrive back to back; anything that arrives resets the count.
///
/// A socket that closes while the burst is still arriving is stated as one:
/// read as a quiet end, the close comes back later as a venue that named no
/// route.
///
/// Silence ends the drain, and so does the deadline: a peer that keeps
/// stating something, one byte at a time, never lets the quiet stretch begin,
/// and would otherwise hold the connect open for as long as it kept talking.
fn drain_init_burst<R: Read>(
    reader: &mut R,
    mut data: Vec<u8>,
    poll: Duration,
    gap: Duration,
    deadline: std::time::Instant,
) -> io::Result<Vec<u8>> {
    let quiet_reads_at_the_end = gap.as_millis().div_ceil(poll.as_millis());
    let mut quiet = 0u128;
    let mut tmp = [0u8; 65536];
    loop {
        if std::time::Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the init burst was still arriving when the deadline passed",
            ));
        }
        match reader.read(&mut tmp) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionReset,
                    "the socket closed while the init burst was still arriving",
                ));
            }
            Ok(n) => {
                quiet = 0;
                crate::protocol::connection::hold_what_was_read(&mut data, &tmp[..n])?;
            }
            // A signal cut the read short. No time passed and the peer said
            // nothing either way, so counting it as a poll's worth of silence
            // ends the drain early — four of them inside one second read as
            // the whole gap, and routing bytes still in flight are never read.
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) if crate::protocol::connection::read_found_nothing(&e) => {
                quiet += 1;
                if quiet >= quiet_reads_at_the_end {
                    break;
                }
            }
            Err(e) => return Err(e),
        }
    }
    Ok(data)
}

/// What the venue says between authentication and the data start, on a
/// reconnect. The same two shapes the first connect's wait answers: the
/// venue can send the session somewhere else after authenticating it, or
/// say no.
enum ReconnectPostAuth {
    /// The data start arrived, and with it whoever the venue said was on
    /// the account when this reconnect got here — where it said so.
    Ready(Option<CompetingSession>),
    /// The venue sent this session somewhere else before it got that far.
    Redirect(String),
}

/// Wait for the data start after a reconnect's authentication, answering
/// what the venue asks for on the way.
///
/// Acts on what the first connect's wait acts on, for the same reasons: a
/// secure error is a refusal and ends the attempt as one, and a redirect is
/// followed. Read past, either is waited out against a host that has
/// already answered, and the attempt repeats exactly as it was — one that
/// was sent elsewhere knocks again at the door that sent it, and one that
/// was refused asks the same question again with the same credentials.
fn wait_for_fix_start<S: Read + Write>(
    stream: &mut S,
    channel: &mut SecureChannel,
    our_logged_in_at: &str,
    deadline: std::time::Instant,
    mut unread: Option<Vec<u8>>,
) -> io::Result<ReconnectPostAuth> {
    let mut competing: Option<CompetingSession> = None;
    while std::time::Instant::now() < deadline {
        // As on the first login: what the gate read past its own answer is the
        // first message here, not something to wait for again.
        let payload = if let Some(carried) = unread.take() {
            carried
        } else {
            match ns::ns_recv(stream, deadline) {
                Ok((payload, _)) => payload,
                Err(e)
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::TimedOut
                        || e.kind() == io::ErrorKind::Interrupted =>
                {
                    log::warn!("CCP reconnect post-auth recv timeout, retrying until deadline: {e}");
                    continue;
                }
                Err(e) => {
                    log::warn!("CCP reconnect post-auth recv: {e}");
                    break;
                }
            }
        };
        let text = String::from_utf8_lossy(&payload);
        let parts: Vec<&str> = text.split(';').collect();
        let raw_type: u32 = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);

        let inner = if raw_type == ns::NS_SECURE_MESSAGE {
            // A secure message states three fields and the frame header
            // establishes none of them. Read rather than indexed: a short
            // frame is a thing to report, where indexing it takes the thread
            // down — and on the reconnect path that is the thread every
            // recovery depends on.
            let body = parts.get(2).copied().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "secure message carries no body")
            })?;
            let ct = B64.decode(body)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
            channel.decrypt(&ct)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?
        } else if raw_type == ns::NS_SECURE_ERROR {
            return Err(ns::refused_by_the_venue(
                "CCP reconnect post-auth secure error",
                parts[2..].join(";"),
            ));
        } else if raw_type == ns::NS_REDIRECT {
            // The host alone: a reconnect dials the auth port whatever port
            // the redirect names, as the one before authentication does.
            let target = parts.get(2).unwrap_or(&"");
            let redirect_host = target.split(':').next().unwrap_or(target).to_string();
            log::info!("CCP reconnect redirected to {redirect_host} after auth");
            return Ok(ReconnectPostAuth::Redirect(redirect_host));
        } else {
            payload
        };

        let inner_text = String::from_utf8_lossy(&inner);
        let inner_parts: Vec<&str> = inner_text.split(';').collect();
        let msg_type: u32 = inner_parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);

        if msg_type == ns::NS_CONNECT_RESPONSE {
            if let Some(other) = parse_competing_session(&inner_text) {
                // Whose session it is, decided by the one fact that separates
                // them: when it logged in. A session older than this one's own
                // last logon is this one's own, still being reaped by the
                // venue, and reconnecting over it finishes the reconnect. A
                // session younger than that logged in while this one was away —
                // it is another client, it took the account fairly, and taking
                // it back starts a fight that was watched happen: both sides
                // reconnecting onto each other every twenty-odd seconds, each
                // seeing only its own data stop.
                if is_another_client(&other.since, our_logged_in_at) {
                    return Err(io::Error::other(format!(
                        "{TOOK_THE_ACCOUNT} another client holds this account, \
                         from {} since {}, after this session logged in at {}",
                        other.ip, other.since, our_logged_in_at,
                    )));
                }
                log::warn!(
                    "reconnected over this session's own earlier logon, from {} at {}{}",
                    other.ip, other.since,
                    if other.read_only { ", and this one may not trade" } else { "" },
                );
                // Carried out with the connection so the engine can say so.
                competing = Some(other);
            }
            let newcomm = format!("{};{};0;;2;0;", NS_VERSION_MIN, ns::NS_NEWCOMMPORTTYPE);
            session::send_secure(stream, channel, newcomm.as_bytes())?;
        } else if msg_type == ns::NS_FIX_START {
            return Ok(ReconnectPostAuth::Ready(competing));
        } else if msg_type == ns::NS_ERROR_RESPONSE {
            return Err(session::error_the_venue_stated(
                "CCP reconnect post-auth error",
                msg_type,
                &inner_parts[2..],
            ));
        }
        // Ignore 530 keepalives and other types
    }
    Err(io::Error::other("CCP reconnect: no FIX_START after auth"))
}

fn reconnect_ccp_attempt(
    auth: &ReconnectAuth,
    token_hash: &str,
    host: &str,
    depth: u32,
    cancel: &std::sync::atomic::AtomicBool,
) -> io::Result<Connection> {
    if depth > 5 {
        return Err(io::Error::other("CCP reconnect: too many redirects"));
    }
    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
        return Err(cancelled_by_the_client("CCP reconnect"));
    }
    log::info!("CCP reconnect to {}:{} (attempt {})", host, AUTH_PORT, depth + 1);
    // The connection carries the venue's own stamp, absent if the
    // acknowledgement states none.
    let mut venue_stamp: Option<String> = None;

    // TLS + DH key exchange
    let addr = format!("{host}:{AUTH_PORT}")
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "DNS resolution failed"))?;
    let tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(TIMEOUT_SSL_AUTH))?;
    // Where this machine is, as far as the venue is concerned: the address of
    // the socket that reached it. The identity announced at logon is built
    // from this in the client this replaces, rather than from a route probed
    // separately, and it is settled once — the first socket's address is the
    // one the venue already has.
    if let Ok(local) = tcp.local_addr() {
        session::note_the_socket_we_opened(local.ip());
    }
    // Bounded from the moment it is open, as the farm connection is. A peer
    // that accepts the socket and then says nothing would otherwise hold the
    // key exchange below for as long as it stays open — and on the reconnect
    // path that worker is what the scheduler waits on, so one silent endpoint
    // stops every later attempt for the life of the process.
    tcp.set_read_timeout(Some(Duration::from_secs(TIMEOUT_SSL_AUTH)))?;
    tcp.set_write_timeout(Some(Duration::from_secs(TIMEOUT_SSL_AUTH)))?;
    let connector = TlsConnector::builder()
        .build()
        .map_err(|e| io::Error::other(e.to_string()))?;
    let mut tls = shake_hands_by(
        &connector, host, tcp, std::time::Instant::now() + Duration::from_secs(TIMEOUT_SSL_AUTH),
    )?;

    let mut channel = SecureChannel::new();
    let dh_msg = channel.build_secure_connect(NS_VERSION, NS_VERSION);
    tls.write_all(&dh_msg)?;

    let hello = read_server_hello(&mut tls, "CCP reconnect")?;
    let hello: Vec<&str> = hello.iter().map(String::as_str).collect();
    channel.process_server_hello(&hello)?;

    // CONNECT_REQUEST with SOFT_TOKEN flag + token hash (field 9)
    let flags = session::FLAG_OK_TO_REDIRECT
        | session::FLAG_VERSION
        | session::FLAG_VERSION_PRESENT
        | session::FLAG_DEVICE_INFO
        | session::FLAG_SOFT_TOKEN
        | session::FLAG_UNKNOWN_U
        | session::FLAG_UNKNOWN_19
        | session::FLAG_UNKNOWN_20
        | if auth.paper { session::FLAG_PAPER_CONNECT } else { 0 };
    let display_name = if auth.paper {
        format!("S{}", auth.username)
    } else {
        auth.username.clone()
    };
    let connect_req = format!(
        "{};{};{};{};{};27;{};{};{};{};",
        NS_VERSION_MIN,
        ns::NS_CONNECT_REQUEST,
        display_name,
        flags,
        NS_VERSION,
        auth.hw_info,
        auth.server_session_id,
        auth.encoded,
        token_hash,
    );
    session::send_secure(&mut tls, &mut channel, connect_req.as_bytes())?;
    log::info!("CCP reconnect CONNECT_REQUEST sent (session={}, hash={})", auth.server_session_id, token_hash);

    // Receive AUTH_START — may get NS_REDIRECT instead
    let auth_start = match session::recv_secure(&mut tls, &mut channel) {
        Ok(data) => data,
        Err(e) if e.to_string().starts_with("REDIRECT:") => {
            let target = e.to_string().replace("REDIRECT:", "");
            let redirect_host = target.split(':').next().unwrap_or(&target).to_string();
            log::info!("CCP reconnect redirected to {redirect_host}");
            drop(tls);
            // Floor before following: this runs on the background
            // reconnect thread, and an instant re-dial chain risks the same
            // rate limiting the backoff ladder exists for.
            std::thread::sleep(Duration::from_secs(2));
            return reconnect_ccp_attempt(auth, token_hash, &redirect_host, depth + 1, cancel);
        }
        Err(e) => return Err(e),
    };

    // Parse AUTH_START field[5] for auth mode: 2=SOFT_TOKEN, 0=SRP required
    let auth_text = auth_start_text(&auth_start)?;
    let auth_fields: Vec<&str> = auth_text.split(';').collect();
    let auth_mode: u32 = auth_fields.get(5).and_then(|s| s.parse().ok()).unwrap_or(0);

    // A message the gate reads that belongs to the loop below, which cannot
    // ask the socket for it a second time.
    let mut post_auth_unread: Option<Vec<u8>>;
    if auth_mode == 2 {
        // SOFT_TOKEN challenge-response (4 states)
        do_ccp_soft_token(&mut tls, &auth.session_key)?;

        // AUTH_FINISH, which has to say the authentication passed rather than
        // merely arrive. If the venue moved on without it, the frame read past
        // it is the post-auth loop's, not this gate's to keep.
        post_auth_unread = expect_auth_finish(&mut tls, "CCP reconnect")?;
    } else {
        // Server requires full SRP (auth_mode != 2). Re-run the SRP handshake
        // with the credentials cached on ReconnectAuth — the same path
        // Gateway::connect uses on first login.
        log::info!("CCP reconnect: server requires SRP, running handshake with cached credentials");
        do_srp(&mut tls, &auth.username, &auth.password)?;
        // The same gate the first login runs. A session dropped across a
        // maintenance window comes back asking for the second factor again,
        // and a reconnect that skipped it retried a handshake the server was
        // never going to finish.
        let (token_type, token_sub_type) = parse_auth_start_token(&auth_text);
        // ponytail: a SOFT token issued here is not written back to `auth`,
        // which the reconnect thread only holds by reference — the next
        // reconnect offers the token it already had.
        //
        // What that costs depends on whether the venue keeps the old token
        // valid once it has issued another, which is its answer to give and
        // has not been captured. If it does, this is the cheaper path lost and
        // nothing more. If it does not, the next reconnect is refused at the
        // challenge and the walk over the other hosts is refused with it —
        // they answer for the same account, and the mode is the venue's
        // choice, so every one of them asks for the same stale token. Writing
        // it back needs the key to reach the caller, which means carrying it
        // out with the connection rather than holding `auth` by reference.
        post_auth_unread = run_second_factor(&mut tls, SecondFactor {
            paper: auth.paper,
            username: &auth.username,
            token_type,
            token_sub_type,
            code_provider: auth.code_provider.as_ref(),
            timeout_secs: auth.ib_key_timeout_secs,
            default_sub_type: &auth.ib_key_token_sub_type,
            cancel: Some(cancel),
        })?.unread;
    }

    // Post-auth: wait for NS_CONNECT_RESPONSE → NEWCOMMPORTTYPE → NS_FIX_START.
    // Per-iteration read timeout aligned with the initial-connect path.
    tls.get_ref().set_read_timeout(Some(Duration::from_secs_f64(TIMEOUT_FIX_LOGON)))?;
    // A read that times out here is the data start still being on its way, not
    // its absence. Giving up on the first one failed the reconnect outright and
    // sent the whole thing round the backoff ladder again — the initial connect
    // already retries to an overall deadline and describes itself as
    // mirroring this path, which never did it.
    let fix_deadline = std::time::Instant::now()
        + std::time::Duration::from_secs_f64(TIMEOUT_FIX_LOGON * 2.0);
    let mut stated_heartbeat: Option<u64> = None;
    let mut market_data_allowance = 40;
    // Whoever held the account when this reconnect arrived, if the venue said,
    // and the interval it holds this connection to.
    let took_from = match wait_for_fix_start(
        &mut tls, &mut channel, &auth.logged_in_at, fix_deadline, post_auth_unread.take(),
    )? {
        ReconnectPostAuth::Ready(other) => other.map(|o| (o.ip, o.since, o.read_only)),
        ReconnectPostAuth::Redirect(redirect_host) => {
            drop(tls);
            // The same floor the redirect before authentication has: this
            // runs on the background reconnect thread, and an instant
            // re-dial chain risks the rate limiting the backoff ladder
            // exists for.
            std::thread::sleep(Duration::from_secs(2));
            return reconnect_ccp_attempt(auth, token_hash, &redirect_host, depth + 1, cancel);
        }
    };
    tls.get_ref().set_read_timeout(None)?;
    // Checked again at the threshold: past it, a session is open at the
    // venue, and one the client has stopped asking for must not be opened.
    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
        return Err(cancelled_by_the_client("CCP reconnect"));
    }

    // FIX Logon
    let logon_msg = build_ccp_logon(&auth.settings, &auth.hw_info, &auth.encoded, CCP_HEARTBEAT, 1);
    tls.write_all(&logon_msg)?;
    tls.flush()?;

    // Short poll timeout + overall deadline so a slow response segment from a
    // high-latency gateway is retried, not treated as a fatal logon failure
    // (, same tolerance as the farm-logon path).
    tls.get_ref().set_read_timeout(Some(Duration::from_millis(FARM_LOGON_POLL_MS)))?;
    let fix_deadline = std::time::Instant::now() + Duration::from_secs_f64(TIMEOUT_FARM_LOGON);
    // What a read took past the end of the message it returned. On this path
    // the venue pushes what it holds as soon as the logon is answered, so
    // those bytes are the session's own state and are handed to the connection
    // below rather than dropped here.
    let mut carry: Vec<u8> = Vec::new();
    let mut acked = false;
    for _ in 0..5 {
        // Read the same way the first logon reads it. This loop parsed the
        // frame as it arrived, so a compressed answer was read as FIX, found
        // nothing in it, and the reconnect failed on an answer it was holding.
        let messages = logon::read_fix_messages(&mut tls, &mut carry, fix_deadline)?;
        let response = logon::joined_body(&messages);
        let fields = fix_parse(&response);
        let msg_type = fields.get(&35).map(|s| s.as_str()).unwrap_or("");
        // The ACK is looked for among the messages rather than taken from the
        // parse. An answer arriving as one envelope holds several messages and
        // the parse keeps the last value for tag 35, so an ACK followed by the
        // session's own init data reads as init data — and this loop acts on
        // nothing but the ACK, so it would read a full answer as no answer.
        //
        // Asked of each message, because the type that answers a logon is the
        // type the account's own traffic arrives under. Asked of the joined
        // body instead, a first burst of nothing but holdings named that type
        // and this loop opened the session on it: the opening requests went
        // out and the connection was handed back as established, on a logon
        // the venue had not answered.
        //
        // The refusal below is deliberately not tested the same way, and the
        // two are not worth making symmetric. A refusal is acted on when the
        // answer ended on one, because a reject riding along in an envelope
        // beside good init data belongs to some earlier message and is not
        // this logon being turned down — scanning the body for it would abort
        // healthy reconnects.
        let names_ack = logon::the_ack_is_among(&messages);
        match msg_type {
            "3" | "5" => {
                let reason = fields.get(&58).map(|s| s.as_str()).unwrap_or("unknown");
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("CCP reconnect logon rejected: {reason}"),
                ));
            }
            _ if names_ack => {
                market_data_allowance = logon::market_data_allowance(&fields);
                // The interval the venue holds this connection to. Read from
                // the answer for the same reason the first logon reads it: the
                // number this client proposes is not what it is held to, and a
                // reconnect that kept the old connection's number would answer
                // a new agreement at the old rate.
                if let Some(stated) = fields.get(&108).and_then(|v| v.parse::<u64>().ok())
                    && stated > 0
                {
                    log::info!("the venue holds this reconnect to a {stated}s heartbeat");
                    stated_heartbeat = Some(stated);
                }
                // When the venue says this logon happened, by its own clock.
                // The competing session it names is stamped by that clock too,
                // and one clock's readings are comparable where two machines'
                // are not.
                // Tag 52 is on every message, so over an envelope this is the
                // last one's sending time rather than the ACK's. They are the
                // same clock and this is compared at second granularity, so
                // the difference does not reach anything — noted because it is
                // the same last-wins hazard the read above works around, and
                // the next tag read here may not be as forgiving.
                venue_stamp = fields.get(&52).cloned();
                // And what rode in with it. The venue pushes what it holds
                // the moment the logon is answered, so the envelope carrying
                // the acknowledgement carries the session's own traffic beside
                // it — an execution report among it. Read for the
                // acknowledgement and then dropped with the envelope, that
                // report reached nothing: a fill nobody was ever told about.
                // Ahead of what the read took past the frame, because that is
                // the order they arrived in.
                let mut beside = logon::what_rode_in_beside_the_ack(&messages);
                if !beside.is_empty() {
                    log::info!("{} bytes rode in with the reconnect ACK", beside.len());
                    beside.extend_from_slice(&carry);
                    carry = beside;
                }
                acked = true;
                break;
            }
            _ => {}
        }
    }
    // Five is a budget against a preamble the venue sets the length of, not a
    // statement that the logon was answered. Falling out of it with nothing
    // read leaves the session with no stamp while reporting success.
    if !acked {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "CCP logon read past five messages without an ACK",
        ));
    }
    tls.get_ref().set_read_timeout(None)?;

    let now = chrono_free_timestamp();
    let account = if auth.account_id.is_empty() { &auth.username } else { &auth.account_id };
    let ccp_seq = logon::send_reconnect_opening(
        &mut tls, auth.settings.execution_reports, account, &now,
    )?;

    let mut conn = Connection::new(tls)?;
    if !carry.is_empty() {
        log::info!("{} bytes arrived with the reconnect ACK", carry.len());
        conn.seed_buffer(&carry);
    }
    conn.seq = ccp_seq;
    log::info!("CCP reconnect complete (seq={})", conn.seq);
    conn.competing = took_from;
    conn.heartbeat_secs = stated_heartbeat;
    conn.market_data_allowance = market_data_allowance;
    // Where this attempt actually landed. A redirect followed here is followed
    // again on every later attempt unless the session remembers it.
    conn.connected_host = Some(host.to_string());
    conn.logged_in_at = venue_stamp;
    Ok(conn)
}

/// Configuration for connecting to IB.
pub struct GatewayConfig {
    /// What this session runs under, settled before it opened.
    ///
    /// Held per session rather than read from the process as it goes, so a
    /// second session in one process cannot write its own settings where the
    /// first session's reconnects would find them.
    pub settings: std::sync::Arc<crate::settings::SessionSettings>,
    /// The login.
    pub username: String,
    /// Wrapped in `Zeroizing` so the plaintext is wiped from memory on drop.
    pub password: Zeroizing<String>,
    /// Which host to open the session against.
    pub host: String,
    /// Whether this is a paper session. It decides one step of the logon and
    /// nothing after it.
    pub paper: bool,
    /// Accept invalid TLS certificates during auth. Default: `false` (secure).
    /// Only set to `true` for local testing against self-signed gateways.
    pub accept_invalid_certs: bool,
    /// Per-session second-factor approval timeout. Defaults to
    /// [`session::IB_KEY_DEFAULT_TIMEOUT_SECS`] (~18 min, matching the
    /// server-side deadline). Set lower to fail fast for unattended logins.
    /// Only consulted on non-paper logins; paper logins skip the gate entirely.
    pub ib_key_timeout_secs: u64,
    /// Fallback second-factor token sub-type for the SWCR_TOKEN state=1 init
    /// body (its fourth field).
    ///
    /// `AUTH_START` states the sub-type for the session, and that value wins:
    /// it is account- and session-specific, so a fixed setting can only ever
    /// match the profile it came from. This is what gets used when the server
    /// states none. Default `"2a"` matches the captured reference profile.
    pub ib_key_token_sub_type: String,
    /// A session from an earlier connect, to log on with instead of the
    /// password.
    ///
    /// The server keeps a session alive past the process that made it, and will
    /// answer a request that names one with a challenge rather than a full
    /// handshake — which is how a client comes back after a restart without a
    /// person to approve it. Ignored unless it names this same account and the
    /// same kind of session, and falls back to the password whenever the server
    /// declines it. Obtained from
    /// [`EClient::session()`](crate::api::client::EClient::session).
    pub resume: Option<crate::auth::resume::ResumableSession>,

    /// Supplies the second factor's code, for whichever exchange `AUTH_START`
    /// selects.
    ///
    /// On the IBKey path it takes the **Challenge/Response** route instead of
    /// waiting for a mobile push: after the server delivers state=2 the
    /// callback is invoked once with the challenge details and the returned
    /// code is submitted as state=3. `None` there leaves behaviour unchanged,
    /// and the login completes by push approval.
    ///
    /// On an authenticator-code account it is the only way to log in — there is
    /// no push to approve — so `None` fails the connect rather than falling
    /// back. See [`session::CodeProvider`] for the contract.
    pub code_provider: Option<session::CodeProvider>,
}

/// What the second-factor gate needs, whether this is the first login or a
/// reconnect that the server bumped back to SRP.
pub(crate) struct SecondFactor<'a> {
    pub paper: bool,
    pub username: &'a str,
    /// Token type and per-session subtype as AUTH_START stated them.
    pub token_type: String,
    pub token_sub_type: Option<String>,
    pub code_provider: Option<&'a session::CodeProvider>,
    pub timeout_secs: u64,
    pub default_sub_type: &'a str,
    /// Set when the client can take the wait back — a reconnect that was
    /// stopped or whose recovery budget is spent. The gate checks it between
    /// polls; the first login has nobody to cancel it and states none.
    pub cancel: Option<&'a std::sync::atomic::AtomicBool>,
}

/// What the venue says between authentication and the data start.
enum PostAuth {
    /// The data start arrived, naming whoever else was already logged in when
    /// this session got here — where the venue named one.
    Ready(Option<CompetingSession>),
    /// The venue sent this session somewhere else before it got that far.
    Redirect(String, u16),
}

/// Wait for the data-farm start, answering what the venue asks for on the way.
///
/// A transient stall here must not be fatal: a single read timeout does not
/// end the wait while the data start is still pending, and keepalive chatter
/// does not exhaust a fixed iteration budget before it arrives. So this
/// retries within an overall deadline and ignores intervening messages,
/// mirroring the CCP-reconnect path.
fn wait_for_data_start(
    tls: &mut native_tls::TlsStream<TcpStream>,
    channel: &mut SecureChannel,
    port: u16,
    mut unread: Option<Vec<u8>>,
) -> io::Result<PostAuth> {
    // Receive post-auth messages (encrypted via 534) and wait for the
    // data-farm start (NS_FIX_START). A transient stall here must not be
    // fatal. A single read timeout does not end the wait while the data
    // start is still pending, and keepalive chatter does not exhaust a
    // fixed iteration budget before it arrives.
    // Retry within an overall deadline and ignore intervening messages,
    // mirroring the CCP-reconnect path.
    tls.get_ref().set_read_timeout(Some(Duration::from_secs_f64(TIMEOUT_FIX_LOGON)))?;
    let fix_deadline = std::time::Instant::now()
        + std::time::Duration::from_secs_f64(TIMEOUT_FIX_LOGON * 2.0);
    let mut fix_ready = false;
    // Whoever was already here when this session arrived, as the venue
    // said so in its answer to the connect.
    let mut competing: Option<CompetingSession> = None;
    while std::time::Instant::now() < fix_deadline {
        // Whatever the second-factor gate read past its own answer is the
        // first message here. Dropped instead, a connect response leaves this
        // loop waiting out its whole deadline for a message already
        // delivered.
        let payload = if let Some(carried) = unread.take() {
            carried
        } else {
            match ns::ns_recv(&mut *tls, fix_deadline) {
                Ok((payload, _)) => payload,
                Err(e) if crate::protocol::connection::read_found_nothing(&e) => {
                    log::warn!("Post-auth recv timeout, retrying until deadline: {e}");
                    continue;
                }
                Err(e) => {
                    log::warn!("Post-auth recv error: {e}");
                    break;
                }
            }
        };
        let text = String::from_utf8_lossy(&payload);
        let parts: Vec<&str> = text.split(';').collect();
        let raw_type: u32 = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);

        // Decrypt if encrypted, otherwise use raw
        let inner = if raw_type == ns::NS_SECURE_MESSAGE {
            // A secure message states three fields and the frame header
            // establishes none of them. Read rather than indexed: a short
            // frame is a thing to report, where indexing it takes the thread
            // down — and on the reconnect path that is the thread every
            // recovery depends on.
            let body = parts.get(2).copied().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "secure message carries no body")
            })?;
            let ct = B64.decode(body)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
            channel.decrypt(&ct)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?
        } else if raw_type == ns::NS_SECURE_ERROR {
            return Err(ns::refused_by_the_venue(
                "Post-auth secure error",
                parts[2..].join(";"),
            ));
        } else if raw_type == ns::NS_REDIRECT {
            let target = parts.get(2).unwrap_or(&"");
            let (redirect_host, redirect_port) = host_and_port(target, port);
            log::info!("Post-auth redirect to {redirect_host}:{redirect_port}");
            return Ok(PostAuth::Redirect(redirect_host.to_string(), redirect_port));
        } else {
            payload
        };

        let inner_text = String::from_utf8_lossy(&inner);
        let inner_parts: Vec<&str> = inner_text.split(';').collect();
        let msg_type: u32 = inner_parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);

        if msg_type == ns::NS_CONNECT_RESPONSE {
            log::info!("NS_CONNECT_RESPONSE: {inner_text}");
            competing = parse_competing_session(&inner_text);
            if let Some(other) = &competing {
                log::warn!(
                    "another session was already logged in from {} at {}{}",
                    other.ip, other.since,
                    if other.read_only { ", and this one may not trade" } else { "" },
                );
            }
            // Send port type change (required before data start)
            let newcomm = format!("{};{};0;;2;0;", NS_VERSION_MIN, ns::NS_NEWCOMMPORTTYPE);
            session::send_secure(tls, channel, newcomm.as_bytes())?;
            log::info!("Port type change sent");
        } else if msg_type == ns::NS_FIX_START {
            log::info!("Data start: {inner_text}");
            fix_ready = true;
            break;
        } else if msg_type == ns::NS_ERROR_RESPONSE {
            return Err(session::error_the_venue_stated(
                "Post-auth error",
                msg_type,
                &inner_parts[2..],
            ));
        } else {
            log::info!("Post-auth msg type={msg_type}: {inner_text}");
        }
    }
    if !fix_ready {
        // TimedOut (not Other) so callers can distinguish a transient
        // post-auth handshake miss — which is retryable — from a genuine
        // auth failure.
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "Never received data start after auth",
        ));
    }
    Ok(PostAuth::Ready(competing))
}

/// Read the routing table's answer, which is optional.
///
/// The answer ends the read the moment its complete frame is held, and its
/// absence ends it at the deadline: the venue owes no routing response, and
/// an idle timeout on a still-open socket is the only thing that may stand
/// for one that is not coming. A socket that closes while the answer is still
/// awaited is stated as one — read as an absent answer, the close comes back
/// later as a connection that was never open.
fn read_routing_response<R: Read>(
    reader: &mut R,
    mut resp_buf: Vec<u8>,
    deadline: std::time::Instant,
) -> io::Result<Vec<u8>> {
    // The logon can leave the head of the next frame. Its continuation alone
    // has no header, so the walk needs the bytes already read as well.
    if holds_a_routing_reply(&resp_buf) {
        return Ok(resp_buf);
    }
    let mut tmp = [0u8; 8192];
    loop {
        // The deadline stands for an answer that is not coming, and a peer
        // sending bytes that never complete a frame keeps every read
        // successful — so read only on the quiet socket below, it never came
        // due at all.
        if std::time::Instant::now() >= deadline {
            // Whatever is held, as the quiet-socket arm below returns it:
            // the venue owes no routing response, so a deadline reached with
            // nothing is an absent answer and not a failure.
            return Ok(resp_buf);
        }
        match reader.read(&mut tmp) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionReset,
                    "the socket closed while the routing response was still arriving",
                ));
            }
            Ok(n) => {
                crate::protocol::connection::hold_what_was_read(&mut resp_buf, &tmp[..n])?;
                // The reply itself, not merely something complete. The farm
                // sends its own traffic while this is waited for, and a
                // compressed heartbeat arriving first ended the wait on a frame
                // that is not the answer — the reply then arrived on the
                // connection's own reads, where nothing takes a routing table
                // out of it. A peer that has gone quiet is still answered by
                // the arm below, so a farm that sends no routing reply at all
                // waits no longer than it did.
                if holds_a_routing_reply(&resp_buf) {
                    return Ok(resp_buf);
                }
            }
            Err(e) if crate::protocol::connection::read_found_nothing(&e) => {
                // The reply itself here too, on the same rule as the read
                // above. A quiet socket is not an answer: the farm's own
                // traffic arriving and the socket then pausing for one poll
                // ended the wait on a frame that is not the reply, and the
                // reply arrived afterwards where nothing takes a routing table
                // out of it. A farm that sends no reply at all waits for the
                // deadline as it already did — silence has always cost that.
                if holds_a_routing_reply(&resp_buf) || std::time::Instant::now() >= deadline {
                    return Ok(resp_buf);
                }
            }
            Err(e) => return Err(e),
        }
    }
}

/// Shake hands under one deadline for the whole of it.
///
/// The socket's own timeouts bound a read, not a handshake, and the handshake
/// is many reads. A peer that answers each one just inside its timeout makes
/// progress by every measure the socket can take and still never finishes: the
/// blocking handshake stays inside the library, no timeout it was given ever
/// fires, and the wait runs to whatever the peer chooses. On the reconnect path
/// this worker is what the scheduler waits on and what a shutdown joins, so one
/// such peer holds both for as long as it likes.
///
/// Driven here instead, the handshake is resumed until it completes or until
/// the deadline the caller set for the whole attempt, whichever comes first.
/// The socket goes back to blocking before it is handed on, so everything after
/// this reads the way the rest of the session expects.
fn shake_hands_by(
    connector: &TlsConnector,
    host: &str,
    tcp: TcpStream,
    deadline: std::time::Instant,
) -> io::Result<native_tls::TlsStream<TcpStream>> {
    fn settle(
        tls: native_tls::TlsStream<TcpStream>,
    ) -> io::Result<native_tls::TlsStream<TcpStream>> {
        tls.get_ref().set_nonblocking(false)?;
        Ok(tls)
    }

    tcp.set_nonblocking(true)?;
    let mut waiting = match connector.connect(host, tcp) {
        Ok(tls) => return settle(tls),
        Err(native_tls::HandshakeError::WouldBlock(mid)) => mid,
        Err(e) => return Err(io::Error::other(e.to_string())),
    };
    loop {
        if std::time::Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the TLS handshake did not complete within the time this attempt has",
            ));
        }
        // The socket answers WouldBlock the moment it has nothing, so this
        // would spin on the processor otherwise. Short enough that it costs a
        // handshake nothing anybody would measure.
        std::thread::sleep(Duration::from_millis(20));
        waiting = match waiting.handshake() {
            Ok(tls) => return settle(tls),
            Err(native_tls::HandshakeError::WouldBlock(mid)) => mid,
            Err(e) => return Err(io::Error::other(e.to_string())),
        };
    }
}

/// Dial the auth server and agree a key with it.
///
/// Returns the stream and the channel every message after this is encrypted
/// with. The port a redirect named travels with the redirect for the record;
/// what is dialled is the auth port, for the reason stated below.
fn dial_auth_server(
    config: &GatewayConfig,
    host: &str,
    port: u16,
) -> io::Result<(native_tls::TlsStream<TcpStream>, SecureChannel)> {
    // --- Phase 1: TLS + auth ---
    //
    // On the auth port, not the port the redirect named. The venue does
    // state one — a redirect to this host carries 4000 — and it is not
    // where the logon is answered: connecting there is accepted at the
    // socket and then reset, and the session only completes on 4001.
    // Measured on a live account, both ports, every redirect in the chain.
    // So the port travels with the redirect for the record, and the auth
    // port is what is dialled.
    if port != AUTH_PORT {
        log::debug!("redirect named port {port}; auth is answered on {AUTH_PORT}");
    }
    log::info!("Connecting to auth server {host}:{AUTH_PORT}");
    let addr = format!("{host}:{AUTH_PORT}")
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "DNS resolution failed"))?;
    let tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(TIMEOUT_SSL_AUTH))?;
    // Where this machine is, as far as the venue is concerned: the address of
    // the socket that reached it. The identity announced at logon is built
    // from this in the client this replaces, rather than from a route probed
    // separately, and it is settled once — the first socket's address is the
    // one the venue already has.
    if let Ok(local) = tcp.local_addr() {
        session::note_the_socket_we_opened(local.ip());
    }
    // Bounded from the moment it is open, as the farm connection is. A
    // peer that accepts the socket and then says nothing would otherwise
    // hold the key exchange below for as long as it stays open, and the
    // caller's connect with it.
    tcp.set_read_timeout(Some(Duration::from_secs(TIMEOUT_SSL_AUTH)))?;
    tcp.set_write_timeout(Some(Duration::from_secs(TIMEOUT_SSL_AUTH)))?;

    let connector = TlsConnector::builder()
        .danger_accept_invalid_certs(config.accept_invalid_certs)
        .build()
        .map_err(|e| io::Error::other(e.to_string()))?;
    let mut tls = shake_hands_by(
        &connector, host, tcp, std::time::Instant::now() + Duration::from_secs(TIMEOUT_SSL_AUTH),
    )?;

    // Key exchange
    let mut channel = SecureChannel::new();
    let dh_msg = channel.build_secure_connect(NS_VERSION, NS_VERSION);
    tls.write_all(&dh_msg)?;

    // A message the venue states while the hello is awaited is read past, not
    // taken for a failure. It states a backup host of its own accord, and
    // ending the connection over one would give up on a venue that had not
    // refused anything.
    let hello = read_server_hello(&mut tls, "connect")?;
    let hello: Vec<&str> = hello.iter().map(String::as_str).collect();
    channel.process_server_hello(&hello)?;
    log::info!("Auth key exchange complete");
    Ok((tls, channel))
}

/// Open the farm connections this login is routed to, all at once.
///
/// Each logon is about six seconds sequentially, and both servers accept
/// concurrent logons with the same credentials, so running them together
/// halves the phase — see the parallel farm logon example. Validated
/// against paper and live.
///
/// Only the trading farm is required. Everything else works without the other
/// two, and a session that refused to open because one farm was down would be
/// worse than one that says which farm it has.
fn connect_farms(
    config: &GatewayConfig,
    session_id: &str,
    token: &BigUint,
    hw_info: &str,
    encoded: &str,
    trading: (&str, &str, Option<u16>),
    mktdata: (&str, &str, Option<u16>),
    secdef: Option<(&str, &str, Option<u16>)>,
) -> io::Result<(Connection, Option<Connection>, Option<Connection>)> {
    let (farm_conn, hmds_conn, secdef_conn) = std::thread::scope(|scope| {
        let username = &config.username;
        let password = &*config.password;
        let paper = config.paper;
        let settings = config.settings.as_ref();
        let trading_handle = scope.spawn(move || {
            connect_farm(settings, trading.0, trading.1, username, password,
                paper, session_id, token, hw_info, encoded, Farm::MarketData, trading.2, None)
        });
        let mktdata_handle = scope.spawn(move || {
            connect_farm(settings, mktdata.0, mktdata.1, username, password,
                paper, session_id, token, hw_info, encoded, Farm::Historical, mktdata.2, None)
        });
        let secdef_handle = secdef.map(|(host, farm, port)| {
            scope.spawn(move || {
                connect_farm(settings, host, farm, username, password,
                    paper, session_id, token, hw_info, encoded, Farm::SecurityDefinition, port, None)
            })
        });
        let trading = trading_handle.join().expect("trading farm thread panicked");
        let mktdata = mktdata_handle.join().expect("mktdata farm thread panicked");
        let secdef = secdef_handle
            .map(|h| h.join().expect("secdef farm thread panicked"));
        (trading, mktdata, secdef)
    });
    let farm_conn = farm_conn?;
    let hmds_conn = match hmds_conn {
        Ok(c) => { log::info!("Historical data farm connected"); Some(c) }
        Err(e) => { log::warn!("Historical data farm connection failed (non-fatal): {e}"); None }
    };
    let secdef_conn = match secdef_conn {
        Some(Ok(c)) => { log::info!("Security definition farm connected"); Some(c) }
        Some(Err(e)) => {
            log::warn!("Security definition farm connection failed (non-fatal): {e}");
            None
        }
        None => {
            log::info!("No security definition farm route was stated at logon");
            None
        }
    };
    Ok((farm_conn, hmds_conn, secdef_conn))
}

/// The key this session signs with, and the token it hands to the farms.
///
/// Two ways in, and the venue has already said which one it will take: it
/// answers a request that named a session it still holds with a challenge over
/// that session, and every other request with a handshake. So this reads the
/// answer rather than deciding, and a resume that is stale, or for another
/// account, or simply older than the venue keeps, logs on with the password
/// instead of failing with an error the caller would have to know how to
/// retry.
fn authenticate(
    tls: &mut native_tls::TlsStream<TcpStream>,
    config: &GatewayConfig,
    auth_start: &[u8],
    resume_key: Option<BigUint>,
) -> io::Result<(BigUint, Option<BigUint>, Option<Vec<u8>>)> {
    // AUTH_START field 4 names the second-factor token type and its
    // per-session subtype, e.g. "5.2i" = tokenType 5 (IBKey), subtype "2i".
    // The subtype is account- and session-specific, so the compiled-in
    // default only ever matches the profile it was captured from; every
    // other account has its SWCR_TOKEN rejected and the socket closed
    // before a challenge is issued.
    let auth_text = auth_start_text(auth_start)?;
    let (server_token_type, server_token_sub_type) = parse_auth_start_token(&auth_text);

    // Field 5 of AUTH_START says which of the two the server will accept.
    // It answers 2 only when the request named a session it still holds, so
    // a resume that is stale, or for another account, or simply older than
    // the server keeps, comes back asking for the handshake — and gets it,
    // rather than an error the caller has to know how to retry.
    let auth_mode: u32 = auth_text
        .split(';')
        .nth(5)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let key = match (resume_key, auth_mode) {
        (Some(key), 2) => {
            log::info!("Resuming the session for {} — no handshake", crate::logging::redacted(&config.username));
            do_ccp_soft_token(tls, &key)?;
            // AUTH_FINISH follows the challenge exactly as it follows a
            // handshake, and it says whether the session exists. What it reads
            // past its own answer belongs to the post-auth exchange, and is
            // handed on rather than dropped.
            let unread = expect_auth_finish(tls, "Resume")?;
            // The stored token is the session key, so the farm logons that
            // follow have what they need without a second factor: the
            // approval that made this session is the one being resumed.
            (key, None, unread)
        }
        (resume_key, _) => {
            if resume_key.is_some() {
                log::info!(
                    "The session offered was not accepted (mode {auth_mode}) — logging on with the password",
                );
            }
            log::info!("Starting auth for {}", crate::logging::redacted(&config.username));
            let session_key = do_srp(tls, &config.username, &config.password)?;
            log::info!("Auth complete");

            // Per-session second-factor approval gate (IBKey / seamless push).
            // Skipped on paper logins; live logins enter a wait state if the
            // account has a second factor configured server-side.
            // Would capture a SOFT session token from AUTH_FINISH PASSED, but per
            // that body carries none, so this stays `None` on both
            // live paths and the farm logon falls back to the SRP session key.
            let gate = run_second_factor(tls, SecondFactor {
                paper: config.paper,
                username: &config.username,
                token_type: server_token_type,
                token_sub_type: server_token_sub_type,
                code_provider: config.code_provider.as_ref(),
                timeout_secs: config.ib_key_timeout_secs,
                default_sub_type: &config.ib_key_token_sub_type,
                cancel: None,
            })?;
            (session_key, gate.soft_token, gate.unread)
        }
    };
    Ok(key)
}

impl Gateway {
    /// Connect to IB: auth + logon + data farm connections.
    /// Returns Gateway + farm Connection + auth Connection + optional historical data
    /// Connection.
    pub fn connect(config: &GatewayConfig) -> io::Result<Session> {
        let first = Self::connect_to_host(config, &config.host, AUTH_PORT, 0);
        let Err(why) = first else { return first };

        // A door that does not open is not an answer. Only when nothing
        // answered, though: a refusal is the same refusal at every door, and
        // asking again with the same credentials is how an account gets locked
        // rather than connected.
        if !nobody_answered(&why) {
            return Err(why);
        }

        let mut last = why;
        for host in doors_after(&config.host) {
            log::warn!("{} did not answer ({last}); trying {host}", config.host);
            match Self::connect_to_host(config, &host, AUTH_PORT, 0) {
                Ok(session) => {
                    log::info!("connected through {host}");
                    return Ok(session);
                }
                // A door that answered and refused has answered for the
                // account, not for itself. Knocking on the rest asks the same
                // question with the same credentials, which is how an account
                // gets locked rather than connected.
                Err(e) if !nobody_answered(&e) => return Err(e),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    /// Internal: connect to a specific host, with redirect depth tracking.
    fn connect_to_host(
        config: &GatewayConfig,
        host: &str,
        port: u16,
        redirect_depth: u32,
    ) -> io::Result<Session> {
        if redirect_depth > 3 {
            return Err(io::Error::other(
                "Too many redirects during auth",
            ));
        }

        // A session belongs to the account and the kind of session that made
        // it. One from another login describes a session this connect has no
        // claim on, and offering it asks the server a question about somebody
        // else's.
        let resume = config.resume.as_ref().filter(|r| {
            r.username == config.username && r.paper == config.paper
        });

        // Tag 6266 carries `{jdkVer}/{platform}/{locale}/{dist}`. The locale
        // segment must be a canonical Java `Locale.toString()` value (e.g.
        // `en_US`, `fr`, `ja_JP`); bare `en` is rejected as `invalid twsInfo`.
        // `IBX_LOCALE` overrides just the locale; `IBX_ENCODED` overrides
        // the whole string for full control.
        let encoded = match resume {
            Some(r) => r.encoded.clone(),
            None => config.settings.encoded.clone(),
        };

        let (mut tls, mut channel) = dial_auth_server(config, host, port)?;

        // Composed behind the dial, not in front of it: the card this machine
        // names is the card of the interface the session opened on, which is
        // not known until it has. Read in front of the dial, the identity was
        // built from whichever card the machine answers with first — a virtual
        // one on some systems.
        let hw_info = match resume {
            Some(r) => r.hw_info.clone(),
            None => session::get_hw_info(
                config.settings.hardware_id.as_deref(),
                config.settings.mac_address.as_deref(),
            )
            .ok_or_else(|| {
                // A host with no hardware address of its own cannot name
                // itself, and the farm behind the logon refuses an identity
                // without one in silence. Said here instead, with the way out.
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "this machine answers with no hardware address, which the data farms \
                     refuse without saying so: state one for it (IBX_MAC) and connect again",
                )
            })?,
        };

        // Send CONNECT_REQUEST (encrypted)
        let flags = session::FLAG_OK_TO_REDIRECT
            | session::FLAG_VERSION
            | session::FLAG_VERSION_PRESENT
            | session::FLAG_DEVICE_INFO
            | session::FLAG_UNKNOWN_U
            | session::FLAG_UNKNOWN_19
            | session::FLAG_UNKNOWN_20
            | if resume.is_some() { session::FLAG_SOFT_TOKEN } else { 0 }
            | if config.paper { session::FLAG_PAPER_CONNECT } else { 0 };
        let display_name = if config.paper {
            format!("S{}", config.username)
        } else {
            config.username.clone()
        };
        // Naming the session that already exists is what lets the server
        // answer with a challenge instead of a handshake; a fresh id has no
        // session behind it and gets the handshake.
        let resume_key = resume.map(|r| BigUint::from_bytes_be(&r.token));
        let session_id = resume
            .map_or_else(session::get_session_id, |r| r.server_session_id.clone());
        let connect_req = match resume_key.as_ref() {
            Some(key) => format!(
                "{};{};{};{};{};27;{};{};{};{};",
                NS_VERSION_MIN, ns::NS_CONNECT_REQUEST, display_name, flags, NS_VERSION,
                hw_info, session_id, encoded, token_short_hash(key),
            ),
            None => format!(
                "{};{};{};{};{};27;{};{};{};",
                NS_VERSION_MIN, ns::NS_CONNECT_REQUEST, display_name, flags, NS_VERSION,
                hw_info, session_id, encoded,
            ),
        };
        session::send_secure(&mut tls, &mut channel, connect_req.as_bytes())?;

        // Receive AUTH_START (may get a redirect instead for paper accounts)
        let auth_start = match session::recv_secure(&mut tls, &mut channel) {
            Ok(data) => data,
            Err(e) if e.to_string().starts_with("REDIRECT:") => {
                let target = e.to_string().strip_prefix("REDIRECT:").unwrap().to_string();
                // Where the venue sent this session, carrying the port it
                // named alongside the host.
                let (redirect_host, redirect_port) = host_and_port(&target, port);
                log::info!("Redirected to {redirect_host}:{redirect_port}, reconnecting...");
                drop(tls);
                // The host that sent us on still answers for this account, so
                // it is worth trying if the one it named stops.
                let knocked = host.to_string();
                let mut session = Self::connect_to_host(
                    config, redirect_host, redirect_port, redirect_depth + 1,
                )?;
                if !session.gateway.auth_hosts.contains(&knocked) {
                    session.gateway.auth_hosts.push(knocked);
                }
                return Ok(session);
            }
            Err(e) => return Err(e),
        };

        let (session_key, soft_token, mut post_auth_unread) =
            authenticate(&mut tls, config, &auth_start, resume_key)?;

        let competing = match wait_for_data_start(
            &mut tls, &mut channel, port, post_auth_unread.take(),
        )? {
            PostAuth::Ready(competing) => competing,
            PostAuth::Redirect(redirect_host, redirect_port) => {
                log::info!("Reconnecting to {redirect_host}:{redirect_port}");
                drop(tls);
                // The host that answered is one this account can reach, and a
                // redirect here dropped it — a session redirected at data-start
                // ran the rest of its life one failover host short. Kept the
                // same way the redirect above keeps it.
                let knocked = host.to_string();
                let mut session = Self::connect_to_host(
                    config, &redirect_host, redirect_port, redirect_depth + 1,
                )?;
                if !session.gateway.auth_hosts.contains(&knocked) {
                    session.gateway.auth_hosts.push(knocked);
                }
                return Ok(session);
            }
        };

        // --- Phase 2: Auth server logon (over TLS) ---
        let logon_msg = build_ccp_logon(&config.settings, &hw_info, &encoded, CCP_HEARTBEAT, 1);
        log::info!("Sending auth logon ({} bytes)", logon_msg.len());
        tls.write_all(&logon_msg)?;
        tls.flush()?;

        // Read FIX messages until the logon ACK (35=A) carrying session info.
        // Short poll timeout + overall deadline so a slow ACK segment from a
        // high-latency gateway is retried, not fatal.
        tls.get_ref().set_read_timeout(Some(Duration::from_millis(FARM_LOGON_POLL_MS)))?;
        let ack_deadline = std::time::Instant::now() + Duration::from_secs_f64(TIMEOUT_FARM_LOGON);
        // What the venue sent past the end of the message that ended the
        // logon. It arrived before the burst below was asked for, so it leads
        // what that burst is answered with.
        let mut carry: Vec<u8> = Vec::with_capacity(65536);
        let mut ack = logon::LogonAck::read(&mut tls, &mut carry, ack_deadline)?;
        tls.get_ref().set_read_timeout(None)?;

        // Fall back to the auth session_id where the server states none
        if ack.server_session_id.is_empty() {
            ack.server_session_id = session_id.clone();
        }

        log::info!(
            "Auth logon: account={} session_id={} hb={}s",
            ack.account_id, ack.server_session_id, ack.heartbeat_interval,
        );

        // --- Post-logon init sequence ---
        let account = if ack.account_id.is_empty() { config.username.clone() } else { ack.account_id.clone() };
        let now = chrono_free_timestamp();
        let mut ccp_seq =
            logon::send_init_sequence(&mut tls, config.settings.execution_reports, &account, &now, 1)?;
        // Counted, not stated: the burst above has been edited before, and a
        // number typed beside it does not follow.
        log::info!("Init sequence sent ({} messages, seq now {})", ccp_seq - 1, ccp_seq);

        // Drain init responses — extract account ID + farm routing tags.
        // read-throughput investigation (2026-05-05):
        // the burst's bulk (~28 kB compressed) arrives in ~300 ms continuous,
        // after which the server emits 67-byte keep-alive trickles every ~10 s
        // until it FINs the socket at ~140 s. On a slow path the same burst
        // arrives with gaps in it, so the drain ends on a quiet stretch
        // rather than on the first gap: one second is past any intra-burst
        // jitter and well short of the 10 s keep-alive trickle interval,
        // which is left undrained — draining it pushes the grace-window
        // messages past the server-side deadline.
        tls.get_ref().set_read_timeout(Some(Duration::from_millis(FARM_LOGON_POLL_MS)))?;
        let read_start = std::time::Instant::now();
        let init_data = drain_init_burst(
            &mut tls, carry,
            Duration::from_millis(FARM_LOGON_POLL_MS), Duration::from_secs(1),
            read_start + Duration::from_secs_f64(TIMEOUT_FARM_LOGON),
        )?;
        log::info!(
            "Init response: {} bytes in {:?}",
            init_data.len(), read_start.elapsed(),
        );

        ack.scan_init(&init_data, &config.username);
        let logon::LogonAck {
            account_id,
            mut accounts,
            mut logged_in_at,
            heartbeat_interval,
            server_session_id,
            ccp_token,
            raw_soft_dollar_tiers,
            raw_family_codes,
            raw_news_providers,
            raw_order_permissions,
            enabled_features,
            raw_enabled_features,
            white_branding_id,
            raw_misc_urls,
            trading_route,
            mktdata_route,
            secdef_route,
            trading_port,
            market_data_allowance,
        } = ack;

        // Sent in plain FIX over TLS: the CCP socket has no AES/HMAC envelope
        // at this stage, as encryption is set up only after `Connection::new`.
        let post_burst_account = if account_id.is_empty() {
            config.username.clone()
        } else {
            account_id.clone()
        };
        ccp_seq = logon::send_post_burst_grace(
            &mut tls, &post_burst_account, &chrono_free_timestamp(), ccp_seq,
        )?;
        log::info!(
            "CCP post-burst grace messages sent (AR+H+PLR+DR+74), seq now {ccp_seq}"
        );

        tls.get_ref().set_read_timeout(None)?;

        // Auth connection (non-blocking TLS for hot loop)
        let mut ccp_conn = Connection::new(tls)?;
        ccp_conn.seq = ccp_seq;
        ccp_conn.market_data_allowance = market_data_allowance;
        // The venue's stamp on this logon, where it gave one. The clock a
        // caller asks for reads from it; left None where the venue stamped
        // nothing, because the local fallback the session keeps below is not
        // the venue's time.
        if !logged_in_at.is_empty() {
            ccp_conn.logged_in_at = Some(logged_in_at.clone());
        }
        // The init burst, seeded into the connection's buffer so the hot loop
        // reads the account data it carries.
        ccp_conn.seed_buffer(&init_data);

        // --- Phase 3: Data farm connections ---
        // A login opens exactly three authed sessions: market data (tag 6145),
        // historical data (tag 6171), and security definitions (tag 8008).
        //
        // The farm token is `SHA1(strip(S))` where S is the SRP shared secret,
        // which is what `do_srp` returns through `srp_compute_k` — so the
        // session key IS the token and is not hashed again here. The
        // per-channel SHA1 on tag 8483 is added where the logon is built. Tag
        // 6386 is an object key, not a token source.
        let farm_token: BigUint = soft_token.clone().unwrap_or_else(|| session_key.clone());
        // read the farm names from the auth-server's
        // routing tags rather than hardcoding `usfarm`/`ushmds`. EU accounts
        // are routed to `eufarm`/`euhmds`/`secdefeu`, US to `usfarm`/`ushmds`,
        // etc. Format of the route strings:
        //   trading (6145):  "<host>/<farm>"            (port from tag 6146, default
        // 4000)
        //   mktdata (6171):  "<host>/<farm>/<port>"
        //   secdef  (8008):  "<host>/<farm>/<port>"
        // Kept as parsed, before the fallback below materializes one. Storing
        // the materialized pair would make "no route was given" indistinguishable
        // from "the route happens to match the connect host", and the reconnect
        // would then prefer a stale host over one the caller has since updated
        // through a redirect.
        //
        // Where the venue names none, a name is invented — and an invented one
        // is a US farm on whichever host this session happened to knock at.
        // That is a guess in both halves, and a farm asked for on a host it is
        // not on answers with ten seconds of silence and a close, so it fails
        // as missing data rather than as a wrong name. Said out loud for that
        // reason: every session in this codebase's logs is named both, so if
        // this is ever read in one, it is the interesting part of it.
        // The venue names where this account is served, and the name is not
        // this client's to supply: it serves accounts from farms of its own
        // naming, and the one a guess would reach is wrong for every account it
        // does not serve from there. Named or there is no connection, which is
        // how the definitions route below is already handled.
        let named = |what: &str, route: &str| {
            parse_farm_route(route).ok_or_else(|| {
                io::Error::other(format!(
                    "the venue named no {what} route for this account, so there is \
                     nowhere to open it: it states one at logon and this session \
                     was given none",
                ))
            })
        };
        let (trading_host, trading_farm, trading_route_port) = named("trading", &trading_route)?;
        // Tag first, then the route, then the configured port: the order the
        // protocol resolves them in.
        let trading_port = trading_port.or(trading_route_port);
        let (mktdata_host, mktdata_farm, mktdata_port) = named("market data", &mktdata_route)?;
        // Contract definitions and the calendar that rides with them. Stated
        // by the venue at logon like the other two; where it states none, this
        // client simply has no such connection rather than guessing a name.
        let secdef = parse_farm_route(&secdef_route);
        log::info!("Farm routing: trading={trading_host}/{trading_farm}, mktdata={mktdata_host}/{mktdata_farm}");

        // Retain HMDS routing for the reconnect loop — the values
        // below are moved into the thread::scope closures.
        let hmds_host_for_gw = mktdata_host.clone();
        let hmds_farm_for_gw = mktdata_farm.clone();
        let trading_host_for_gw = trading_host.clone();
        let trading_farm_for_gw = trading_farm.clone();

        let (farm_conn, hmds_conn, secdef_conn) = connect_farms(
            config,
            &server_session_id,
            &farm_token,
            &hw_info,
            &encoded,
            (&trading_host, &trading_farm, trading_port),
            (&mktdata_host, &mktdata_farm, mktdata_port),
            secdef.as_ref().map(|(h, f, p)| (h.as_str(), f.as_str(), *p)),
        )?;

        // A family names the accounts linked to this login, which are accounts
        // this login may trade, so they belong in the list a caller asks for.
        for entry in raw_family_codes.split(';') {
            if let Some((account, _)) = entry.split_once('|') { note_account(&mut accounts, account); }
        }
        let account_id = if account_id.is_empty() { config.username.clone() } else { account_id };
        note_account(&mut accounts, &account_id);
        // The acknowledgement names the username on the tag an account arrives
        // on, and the reader that takes it there has no username to tell it
        // apart from one — the burst scanner does, and guards it; this is the
        // other place that knows. Left in, the list a caller reads as the
        // accounts this login holds carried a name the venue refuses every
        // request made under it.
        //
        // Unless it is the account: a login the venue named no account for
        // stands on the username, which the fallback above just used.
        if account_id != config.username {
            accounts.retain(|a| a != &config.username);
        }
        // The account a caller gets by default leads the list.
        if accounts.first().map(String::as_str) != Some(account_id.as_str()) {
            accounts.retain(|a| a != &account_id);
            accounts.insert(0, account_id.clone());
        }
        log::info!("Login holds {} account(s)", accounts.len());

        // This clock only where the venue stated none, for the reason the
        // reconnect states: no stamp at all reads every session the venue
        // names as another client.
        if logged_in_at.is_empty() {
            logged_in_at = chrono_free_timestamp().to_string();
            log::info!("the venue stamped no time on this logon; using this clock");
        }

        let gw = Gateway {
            competing,
            logged_in_at,
            auth_hosts: vec![host.to_string()],
            account_id,
            accounts,
            session_token: session_key,
            server_session_id,
            ccp_token,
            heartbeat_interval,
            hw_info,
            encoded,
            raw_soft_dollar_tiers,
            raw_family_codes,
            raw_news_providers,
            raw_order_permissions,
            enabled_features,
            raw_enabled_features,
            white_branding_id,
            misc_urls: parse_misc_urls(&raw_misc_urls),
            hmds_host: hmds_host_for_gw,
            hmds_farm: hmds_farm_for_gw,
            trading_host: trading_host_for_gw,
            trading_farm: trading_farm_for_gw,
            secdef_host: secdef.as_ref().map(|(h, ..)| h.clone()).unwrap_or_default(),
            secdef_farm: secdef.as_ref().map(|(_, f, _)| f.clone()).unwrap_or_default(),
            trading_port,
            hmds_port: mktdata_port,
            secdef_port: secdef.as_ref().and_then(|&(_, _, p)| p),
        };
        Ok(Session {
            gateway: gw,
            market_data: farm_conn,
            trading: ccp_conn,
            historical: hmds_conn,
            security_definition: secdef_conn,
        })
    }

    /// Populate shared state with gateway-local init data parsed from CCP logon.
    pub fn populate_init_data(&self, shared: &SharedState) {
        use crate::types::{NewsProvider, SoftDollarTier, FamilyCode};

// Which venues SMART routes to, and in what order a quote's exchange
        // mask refers to them, is stated by the server on a contract's own
        // definition. Nothing is assumed before it says so: a list written
        // here carried this client's own order, and the order is the whole of
        // what the mask means, so every quote's bid, ask and last named the
        // wrong venue until a definition arrived to correct it.

        // News providers, as the logon stated them and only as it stated them.
        //
        // There is no request for this list. It arrives on the logon and
        // nowhere else, and an empty tag means the account is entitled to no
        // providers. A fallback list would state entitlements the account does
        // not hold, and an article request against one of them is refused with
        // no explanation the caller can act on.
        //
        // Wire format: "code1/name1,code2/name2,…". Read as though the pairs
        // were separated by semicolons and the halves by commas, one provider
        // came back holding the whole list: its code was the first code and
        // name, and its name was every other provider. A caller then asked for
        // headlines under a code no venue knows and was answered with nothing.
        let news_providers: Vec<NewsProvider> = self
            .raw_news_providers
            .split(',')
            .filter_map(|entry| {
                let entry = entry.trim();
                if entry.is_empty() {
                    return None;
                }
                let (code, name) = entry.split_once('/')?;
                Some(NewsProvider {
                    code: code.trim().to_string(),
                    name: name.trim().to_string(),
                })
            })
            .collect();
        if news_providers.is_empty() {
            log::info!(
                "the logon stated no news providers, so this account is entitled to none"
            );
        }
        shared.reference.set_news_providers(news_providers);

        // Soft dollar tiers, as the logon states them and only as it states
        // them.
        //
        // The venue sends them comma separated, each one its value, the name it
        // is shown under, and the name it is asked for by, separated by
        // slashes: "1/Maximize Rebate/MaxRebate,9/Prefer Rebate/PreferRebate".
        //
        // This looked for a different shape entirely — semicolons between the
        // entries and bars inside them — so every entry failed to parse and a
        // list written here stood in. That list was a transcription of what the
        // venue actually sends, which is why it looked right: the data had been
        // copied out of a reply instead of read from one, and the day the venue
        // changed a tier nobody would have known.
        let tiers: Vec<SoftDollarTier> = self
            .raw_soft_dollar_tiers
            .split(',')
            .filter_map(|entry| {
                let entry = entry.trim();
                if entry.is_empty() {
                    return None;
                }
                let mut parts = entry.split('/');
                let val = parts.next()?.trim();
                let display_name = parts.next()?.trim();
                let name = parts.next()?.trim();
                if val.is_empty() || name.is_empty() {
                    return None;
                }
                Some(SoftDollarTier {
                    name: name.to_string(),
                    val: val.to_string(),
                    display_name: display_name.to_string(),
                })
            })
            .collect();
        if tiers.is_empty() && !self.raw_soft_dollar_tiers.is_empty() {
            log::warn!(
                "the logon stated soft dollar tiers in a shape this does not read: {}",
                self.raw_soft_dollar_tiers,
            );
        }
        shared.reference.set_soft_dollar_tiers(tiers);

        // Family codes: parse from CCP logon tag 6823.
        // Empty for paper/single accounts.
        let codes = if self.raw_family_codes.is_empty() {
            Vec::new()
        } else {
            self.raw_family_codes.split(';').filter_map(|entry| {
                let parts: Vec<&str> = entry.split('|').collect();
                if parts.len() >= 2 {
                    Some(FamilyCode {
                        account_id: parts[0].to_string(),
                        family_code_str: parts[1].to_string(),
                    })
                } else {
                    log::warn!("Unexpected family code format: {entry}");
                    None
                }
            }).collect()
        };
        shared.reference.set_family_codes(codes);

        // Order permissions: "SECTYPE:ORDTYPE,ORDTYPE;SECTYPE:..." from logon tag
        // 6652. A security type named with no order types after it is still
        // permitted, so an absent list is an empty one, never a missing key.
        let perms = parse_order_permissions(&self.raw_order_permissions);
        if !perms.is_empty() {
            let mut named: Vec<&str> = perms.keys().map(String::as_str).collect();
            named.sort_unstable();
            log::info!("Order permissions: {}", named.join(" "));
        }
        shared.reference.set_order_permissions(perms);

        // Enabled features: a plain comma-separated token list, logon tag 6542.
        shared.reference.set_enabled_features(
            self.raw_enabled_features.split(',').filter(|t| !t.is_empty()).map(str::to_string).collect(),
        );

        // White branding ID (empty for standard accounts).
        shared.reference.set_white_branding_id(self.white_branding_id.clone());

        // Webapp-REST-facing fields from the FIX logon roundtrip.
        shared.reference.set_ccp_session_id(self.server_session_id.clone());
        shared.reference.set_misc_urls(self.misc_urls.clone());
        shared.reference.set_competing_session(self.competing.as_ref().map(|other| {
            (other.ip.clone(), other.since.clone(), other.read_only)
        }));
    }

    /// The credentials and session a reconnect needs, from this gateway plus
    /// what the caller supplied.
    ///
    /// Shared with the test harness so a compatibility run recovers a dropped
    /// transport the same way a real client does, instead of failing the phase
    /// that happened to be running.
    pub fn reconnect_auth(&self, caller: CallerAuth) -> ReconnectAuth {
        // Where this session actually ended up, not where it first knocked.
        // The venue names the server an account belongs on and the session
        // follows it, so reconnecting to the address the caller configured
        // starts again at a door that only redirects — and on a session that
        // was redirected, that is every reconnect.
        let on = self.auth_hosts.first().cloned().unwrap_or_else(|| caller.host.clone());
        ReconnectAuth {
            logged_in_at: self.logged_in_at.clone(),
            settings: caller.settings,
            alternate_hosts: alternates_to(&self.auth_hosts, &on),
            host: on,
            username: caller.username,
            password: caller.password,
            paper: caller.paper,
            code_provider: caller.code_provider,
            ib_key_timeout_secs: caller.ib_key_timeout_secs,
            ib_key_token_sub_type: caller.ib_key_token_sub_type,
            session_key: self.session_token.clone(),
            session_token: self.session_token.clone(),
            server_session_id: self.server_session_id.clone(),
            account_id: self.account_id.clone(),
            hw_info: self.hw_info.clone(),
            encoded: self.encoded.clone(),
            secdef_host: self.secdef_host.clone(),
            secdef_farm: self.secdef_farm.clone(),
            hmds_host: self.hmds_host.clone(),
            hmds_farm: self.hmds_farm.clone(),
            trading_host: self.trading_host.clone(),
            trading_farm: self.trading_farm.clone(),
            trading_port: self.trading_port,
            hmds_port: self.hmds_port,
            secdef_port: self.secdef_port,
        }
    }

}

// Where a market-data subscription is composed, and the clock a request is
// stamped with. Both moved to the wire module; reachable here because that is
// the path a program written against this client already names.
pub use crate::protocol::datetime::{chrono_free_timestamp, days_to_ymd};
pub use crate::protocol::market_data::{build_mktdata_subscribe, build_mktdata_unsubscribe};

#[cfg(test)]
mod tests;
