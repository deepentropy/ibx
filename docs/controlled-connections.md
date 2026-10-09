# Caller-controlled connections and strict historical parsing

The existing `Gateway::connect` and `EClient::connect` entry points retain their
legacy behavior. These additions provide an opt-in path for applications that
own a bounded login worker, require cancellation/confirmed teardown before a new
session, or need finite historical data to fail explicitly on malformed input.

## Controlled login and shutdown

`Gateway::connect_once(config, control)` and
`EClient::connect_once(config, control)` make one controlled authentication
attempt. Prepare the address map and hardware information before invoking them
on an owned blocking worker. `ConnectionControl::new(timeout, addresses, hw_info)`
requires a positive timeout; up to 32 hostnames, up to eight IP addresses each;
and hardware information of 1..=1024 bytes. Hostnames are limited to 253 bytes,
usernames to 256 bytes and passwords to 4096 bytes. Redirect/farm hosts must
already be in the map. The controlled path performs no DNS, hardware subprocess,
detached discovery request or autonomous reconnect.

The control is single-use for physical login. Its owned watchdog registers up to
16 sockets; TCP connects and blocking I/O check cancellation on 100ms polls.
`cancel()` signals stop independently of a full command queue. Pending standard
I/O helpers receive a terminal transport error rather than an Interrupted error
they might retry indefinitely. Successful EClient initialization disarms the
login deadline while retaining explicit cancellation.

Controlled authentication uses verified rustls TLS with an explicit Ring
provider and bundled webpki roots; machine-installed trust roots do not affect
it. It permits broker-selected mobile push or no second factor; unsupported
challenge callbacks, disabled certificate checking, malformed input and encryption
downgrades fail. Farm transport selection follows upstream's SSL farm list.
Upstream's key-exchange certificate checks remain in effect on encrypted plain
farm connections. Mobile-push framing preserves partial input, heartbeat handling,
approval and post-authentication carry under byte/frame/count/deadline limits.
NS/XYZ login payloads are limited to 256KiB and retained/aggregate stream input to
4MiB. SRP groups are limited to 8192 bits before big-integer construction.

Controlled engines reject pre-existing/on-demand farm pools and unowned reconnect
workers. Cancellation and Shutdown stop subsequent commands in the same batch.
The new upstream nonblocking engine and wakeable control sender remain intact;
controlled TLS exposes its owned raw socket for polling. Queued writes preserve
accepted plaintext offsets and pending ciphertext under backpressure without
replay. A panic is retained for checked cleanup rather than silently discarded.

`try_send_control` only confirms nonblocking queue admission, not broker effects.
`disconnect_checked(timeout)` checks joining the actual engine and retained
workers/watchdog. A timeout retains handles for a later check; a panic stays a
cleanup failure. `disconnect()` and Drop signal stop and are best effort, not
proof of completed cleanup. The application must also join its own login worker
before replacing a session, including a failed attempt.
`ConnectionControl::join_workers` cannot certify a separately owned worker.

These are cooperative scheduling bounds, not hard real-time guarantees.
Successful native initialization does not validate an application's intended
account, establish trading readiness or bound every post-login cache/queue.

## Bounded transport helpers

`protocol::ns::ns_recv_limited` checks declared payload size before allocation.
`protocol::fixcomp::fixcomp_decompress_limited` caps inflated output and rejects
lost prefixes/tails. Bounded authentication helpers take explicit limits;
they need a controlled transport for cancellation.

`Connection::poll_limited(buffer_limit, frame_limit)` checks framing sizes before
body allocation, preserves partial input, returns at most one frame per poll and
keeps receive/EOF errors terminal. Unsigned FIX checksums are verified immediately;
signed FIX checksums are verified after valid unsigning.
`send_fix_limited` admits one bounded pending frame, including pending rustls
ciphertext, without advancing sequence/signature state for a rejected write.
Legacy transport helpers remain available.

## Strict finite historical parsing

`control::historical::parse_bar_response_strict(xml, max_rows, max_bytes)` is
opt-in and does not rewire the legacy historical dispatcher. It checks bytes/rows
before allocating owned output and uses a bounded-depth borrowed XML stack.
Malformed structure, duplicate/unknown fields, missing/nonfinite OHLC and invalid
completion fail as typed `StrictHistoryError` variants.

`StrictHistoricalResponse` preserves the query ID, timezone, bars and explicit
completion. Only `eoq=true` proves a final frame. Bar fields retain original time,
optional raw `endTime`, optional `timeAvg` distinct from WAP, finite f64 OHLC,
optional i64 volume, optional f64 WAP and optional u32 count. Missing/empty
statistics and volume/count -1 are absent; zero remains present. WAP -1 remains
raw because interpretation depends on the price family. No coverage range or
timestamp conversion is invented.

Callers must bound multipart totals and separately delivered notices, reject
failures before reporting success and preserve connection-generation ownership.
Upstream's legacy request-derived start/end strings are not broker-reported
coverage; the strict parser does not manufacture them.

## Offline validation and limits

Tests use socket-free fixtures or loopback peers with fixture credentials.
They cover deadline/cancellation during login stages, TLS chain/hostname
verification, stalled TLS handshakes, scoped parallel farm failure, checked worker
cleanup, full queues, retained socket owners, partial reads, queued ciphertext,
malformed/oversized history and explicit completion.

The Windows MSVC validation includes controlled/bounded/lifecycle/history,
protocol, gateway, EClient, engine, certificate/DH and order-ID regressions;
inspected scripted-peer, replay, lifecycle, control-plane, concurrency, protocol
vector and catalog integrations; Python-feature compilation and rustdoc.
Some groups overlap. Existing ignored replays remain ignored and existing
upstream warnings remain visible. Full successful controlled broker login,
paper/live order acceptance, comprehensive post-login bounds and loss-aware
request completion have not been established by these offline tests.