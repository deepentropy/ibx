//! Non-blocking connection wrapping a TLS or raw TCP stream with read/write buffers.
//!
//! Maintains per-connection state: buffer, seq counter,
//! HMAC sign/read IVs (chained per message).

use std::io::{self, Read, Write};
use std::net::TcpStream;

use native_tls::TlsStream;

use super::fix::{self, SOH};
use super::fixcomp;

/// Recv buffer size.
const RECV_BUF_SIZE: usize = 32768;

/// The most an unfinished frame may hold in the buffer.
///
/// A frame states its length and then never completes: the peer trickles,
/// stops, or stated a length it never meant to fill. Without a bound,
/// waiting for the rest grew the buffer until the process died. Passing the
/// bound gives up the read, and the reconnect path takes the connection
/// over, as it does for a frame that failed to verify.
///
/// Set above anything legitimate: the largest frame this protocol carries
/// is a compressed one, and nothing is used once it inflates past
/// `fixcomp::MAX_INFLATED` — its wire form cannot exceed its inflated one by
/// more than the compression's own overhead. The margin beside it covers
/// that overhead and whatever else one read held when the bound was passed.
pub(crate) const MAX_BUFFERED: usize = fixcomp::MAX_INFLATED as usize + 1024 * 1024;

/// Hold what a read took, or give the connection up rather than hold it.
///
/// The same bound [`Connection::read`] keeps, for the loops that assemble a
/// message by hand instead of framing one. Those run while the session is
/// being established, which is before the peer has proved anything, and a
/// length it chose is not a length this side has to find room for: past the
/// bound what is arriving is not a message on its way but a demand for
/// memory, and a deadline does not answer it — the bytes arrive as fast as
/// the sender can write them and the wait ends long after the memory is gone.
pub(crate) fn hold_what_was_read(buf: &mut Vec<u8>, read: &[u8]) -> io::Result<()> {
    if buf.len() + read.len() > MAX_BUFFERED {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "a peer sent more than {MAX_BUFFERED} bytes without completing a \
                 message; refusing to buffer it",
            ),
        ));
    }
    buf.extend_from_slice(read);
    Ok(())
}

/// What a compressed frame starts with, and the longest header this framing
/// recognises. A read that stops inside it leaves fewer bytes than the marker,
/// so the buffer holds them until the rest arrives.
const FIXCOMP_MARKER: &[u8] = b"8=FIXCOMP\x01";

/// A framed message extracted from the connection buffer.
#[derive(Debug)]
pub enum Frame {
    /// Standard FIX 4.1 message (checksum-terminated).
    Fix(Vec<u8>),
    /// Compressed message (may contain multiple inner messages).
    FixComp(Vec<u8>),
    /// 8=O binary protocol message (length-delimited).
    Binary(Vec<u8>),
    /// 8=1 / 8=X control message (token-auth / encrypted control state).
    /// Same length-prefixed framing as 8=O; not consumed downstream, but
    /// extracted explicitly so it cannot clobber FIXCOMP frames queued behind
    /// it in the same recv slice.
    Control(Vec<u8>),
}

/// Stream wrapper supporting both TLS and raw TCP.
enum Stream {
    Tls(TlsStream<TcpStream>),
    Raw(TcpStream),
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Tls(s) => s.read(buf),
            Self::Raw(s) => s.read(buf),
        }
    }
}

impl Stream {
    /// The underlying socket, whichever transport this is.
    fn socket(&self) -> &TcpStream {
        match self {
            Self::Tls(s) => s.get_ref(),
            Self::Raw(s) => s,
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Tls(s) => s.write(buf),
            Self::Raw(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Tls(s) => s.flush(),
            Self::Raw(s) => s.flush(),
        }
    }
}

/// How long a write may make no progress at all before it gives up. Every send
/// goes out on the one hot-loop thread, so an unbounded write stops that thread
/// evaluating liveness, servicing shutdown, or polling a reconnect — and the
/// heartbeat that exists to detect a wedged peer is itself a write, so the
/// detector cannot run in exactly the case it was built for.
///
/// This bounds a single write syscall, not a whole frame: a peer draining a
/// trickle keeps resetting it and a large frame can take proportionally longer.
/// The case this exists for is the peer that makes no progress whatsoever,
/// which trips the first timeout. Kept under the liveness probe interval so a
/// stalled send cannot hold the loop past its own cadence.
const WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// How long a whole frame may take to reach the socket.
///
/// The bound above is per syscall and a partial write restarts it, so on its
/// own it bounds a peer taking nothing and not a peer taking a little. This is
/// the bound on the frame.
///
/// Set to the interval the loop probes liveness on, which is the bound the
/// per-write timeout was already reasoned against — "kept under the liveness
/// probe interval so a stalled send cannot hold the loop past its own
/// cadence". A frame that has not gone out within one probe interval has
/// already held the thread that would have done the probing for a whole one.
///
/// Stated here rather than taken from the loop, because this layer is below
/// it. The loop asserts the two agree, so they cannot drift apart in silence.
pub const WHOLE_FRAME_TIMEOUT_SECS: u64 = 15;
const WHOLE_FRAME_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(WHOLE_FRAME_TIMEOUT_SECS);

/// The timeouts every connection runs with, in one place so the two
/// constructors cannot drift apart — a write bound present on one transport and
/// missing on another is invisible to a test that probes either one.
fn configure_socket(stream: &TcpStream) -> io::Result<()> {
    stream.set_read_timeout(Some(std::time::Duration::from_millis(1)))?;
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
    Ok(())
}

/// Whether a write that stopped with `err` after `written` bytes leaves the
/// transport usable. `tls` is whether the bytes went through a TLS layer,
/// which cannot report how much of the frame actually reached the socket.
///
/// Outbound frames are HMAC-chained, so a frame that went out in part is
/// unrecoverable: the peer has a prefix it cannot verify and every later frame
/// is signed from state it does not share. A write that made no progress at all
/// put nothing on the wire — the chain is intact, so the frame is offered again
/// where `write_frame` reads this, and a peer that is merely slow does not cost
/// the transport. One that takes nothing for the whole frame's budget does.
fn write_is_recoverable(err: &io::Error, written: usize, tls: bool) -> bool {
    // Never on TLS. The count here is plaintext accepted by the TLS layer, not
    // bytes on the socket: a stalled write can report none accepted while a
    // partial record has already gone to the peer. Retrying then puts a second
    // frame behind half of the first, and the sequence and signature state
    // would be committed for a frame the peer never receives whole — an order
    // reported as failed here could still arrive at the venue.
    !tls
        && written == 0
        && matches!(err.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)
}

/// Whether a read that stopped with `err` means nothing arrived, rather than a
/// transport that is gone.
///
/// Every socket here carries a short read timeout, so an idle peer reports one
/// on almost every poll. A signal delivered to the reading thread reports
/// `Interrupted` from the same place, and Linux does not restart a socket read
/// that carries a timeout — so a signal that has nothing to do with the peer
/// arrives looking exactly like a failure. Read as one it costs the whole
/// connection: the hot loop's pollers reconnect and replay on any error at
/// all, and a logon fails outright.
pub(crate) fn read_found_nothing(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted,
    )
}

/// Per-connection state for an auth or data socket.
pub struct Connection {
    stream: Stream,
    buf: Vec<u8>,
    /// FIX message sequence number (6-digit zero-padded).
    pub seq: u32,
    /// HMAC key for signing outbound messages.
    pub sign_key: Vec<u8>,
    /// IV for signing outbound messages (chains across messages).
    pub sign_iv: Vec<u8>,
    /// HMAC key for verifying inbound messages.
    pub read_key: Vec<u8>,
    /// IV for verifying inbound messages (chains across messages).
    pub read_iv: Vec<u8>,
    /// The server this connection was finally made on.
    ///
    /// A logon can be redirected, and the address dialled is then not the
    /// address answering. A session that does not remember where it ended up
    /// starts every later attempt at a door that only redirects, and never
    /// learns the one host it is certain answers for this account.
    pub connected_host: Option<String>,
    /// When the logon that built this connection was answered, in the spelling
    /// the venue uses when it names somebody else's session.
    pub logged_in_at: Option<String>,
    /// Another session that already held this account when this connection was
    /// made, as the venue named it: address, login time, and whether this
    /// session is held to reading only.
    ///
    /// A reconnect that finds one has taken the account back from whoever had
    /// it. That is worth telling a caller — silently, it reads to them as the
    /// other program's data stopping for no reason.
    pub competing: Option<(String, String, bool)>,
    /// How often this connection's far side expects to hear from the session,
    /// where it said so in its answer to the logon.
    ///
    /// `None` where it named none, which leaves the interval this client
    /// proposed. The number it answers with is the one the session is held to,
    /// and the one it proposed is not.
    pub heartbeat_secs: Option<u64>,
    /// How many quote subscriptions this session may hold; the venue states it on the logon.
    pub market_data_allowance: usize,
    /// What the venue answered this connection's routing request with.
    ///
    /// Sent once, right after logon, on the connection that asked — so it
    /// arrives here and nowhere else. Empty on a connection that did not ask.
    pub routing: crate::protocol::routing::RoutingTable,
    /// Set once a write has failed. A timed-out or errored `write_all` may
    /// have put part of a frame on the wire, and outbound frames are
    /// HMAC-chained, so anything sent afterwards would be signed from state
    /// the peer does not share. There is no resuming mid-frame: the transport
    /// is finished and the reconnect path takes it from here.
    write_failed: bool,
    /// Set when a frame failed to verify, or when what arrived stopped being
    /// readable at all. The chain cannot advance past either, so nothing
    /// further on this socket can be read.
    read_failed: bool,
    /// Bytes discarded as unreadable since a frame was last extracted.
    ///
    /// A peer that sends traffic this client cannot read still counts as
    /// arriving while the bytes keep coming, so the liveness deadlines never
    /// fire and it holds the connection open for as long as it sends. Once
    /// more has arrived unreadable than one frame can be, what is arriving is
    /// not a frame this lost the start of but traffic that is not readable,
    /// and the connection is given up the way one is for a frame that failed
    /// to verify.
    unreadable: u64,
}

impl Connection {
    /// Create a new connection from an already-established TLS stream.
    ///
    /// Uses a blocking socket with a 1ms read timeout and a bounded write —
    /// mirrors `new_raw`, through the same `configure_socket`. Non-blocking
    /// writes can return `WouldBlock` after a partial send, which poisons the
    /// seq/sign_iv chain that already advanced for the not-yet-on-the-wire
    /// message. A bounded blocking write either commits the frame in full or
    /// reports how far it got, which `write_frame` acts on.
    pub fn new(stream: TlsStream<TcpStream>) -> io::Result<Self> {
        Self::from_stream(Stream::Tls(stream))
    }

    /// Create a new connection from a raw TCP stream (for farm connections).
    /// Enables TCP_NODELAY.
    pub fn new_raw(stream: TcpStream) -> io::Result<Self> {
        stream.set_nodelay(true)?;
        Self::from_stream(Stream::Raw(stream))
    }

    /// The one place a connection is built, so a transport cannot be given a
    /// different set of socket timeouts from the others by being constructed
    /// down a different path.
    fn from_stream(stream: Stream) -> io::Result<Self> {
        configure_socket(stream.socket())?;
        Ok(Self {
            stream,
            buf: Vec::with_capacity(RECV_BUF_SIZE),
            seq: 0,
            sign_key: Vec::new(),
            sign_iv: Vec::new(),
            read_key: Vec::new(),
            read_iv: Vec::new(),
            connected_host: None,
            logged_in_at: None,
            competing: None,
            heartbeat_secs: None,
            market_data_allowance: 40,
            routing: Default::default(),
            write_failed: false,
            read_failed: false,
            unreadable: 0,
        })
    }

    /// Set HMAC keys and IVs after authentication.
    pub fn set_keys(
        &mut self,
        sign_key: Vec<u8>,
        sign_iv: Vec<u8>,
        read_key: Vec<u8>,
        read_iv: Vec<u8>,
    ) {
        self.sign_key = sign_key;
        self.sign_iv = sign_iv;
        self.read_key = read_key;
        self.read_iv = read_iv;
    }

    /// Pre-load data into the read buffer (e.g. init burst bytes read before Connection
    /// was created).
    pub fn seed_buffer(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// Whether the internal buffer contains unprocessed data.
    pub fn has_buffered_data(&self) -> bool {
        !self.buf.is_empty()
    }

    /// Whether a write left the stream mid-frame and the transport was given
    /// up. Nothing more can go out on it, so the socket is dead in the only
    /// direction that matters even where the peer is still sending.
    pub fn write_failed(&self) -> bool {
        self.write_failed
    }

    /// Mark the write side failed, for tests that exercise a refused write.
    #[cfg(test)]
    pub(crate) fn fail_writes(&mut self) {
        self.write_failed = true;
    }

    /// Whether a frame failed to verify, which finishes the connection for
    /// reading.
    ///
    /// The chain only advances on a frame that verified, so one that did not
    /// leaves the receiver a step behind a sender that moved on: every frame
    /// after it fails the same way. There is no recovering the step, and
    /// nothing arrives again on this socket.
    pub fn read_failed(&self) -> bool {
        self.read_failed
    }

    /// Non-blocking read from the socket into the internal buffer.
    /// Returns the number of bytes read, or 0 if no data available (WouldBlock).
    pub fn try_recv(&mut self) -> io::Result<usize> {
        let mut tmp = [0u8; RECV_BUF_SIZE];
        match self.stream.read(&mut tmp) {
            Ok(0) => Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "connection closed",
            )),
            Ok(n) => {
                // Past the bound this is not a frame arriving but a demand
                // for memory: nothing this large completes into a frame this
                // protocol carries, so the read is given up rather than
                // buffered, and the reconnect path takes the connection.
                if self.buf.len() + n > MAX_BUFFERED {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "a frame states a length that is not completing; \
                             refusing to buffer beyond {MAX_BUFFERED} bytes",
                        ),
                    ));
                }
                self.buf.extend_from_slice(&tmp[..n]);
                Ok(n)
            }
            Err(e) if read_found_nothing(&e) => Ok(0),
            Err(e) => Err(e),
        }
    }

    /// Extract all complete frames from the internal buffer.
    /// Handles compressed, standard, and binary protocols.
    pub fn extract_frames(&mut self) -> Vec<Frame> {
        let mut frames = Vec::new();
        // What this call threw away without reading. Kept beside the walk
        // because a stream of it is traffic this client cannot read, which is
        // accounted for at the end of the walk rather than as it happens.
        let mut discarded = 0usize;
        loop {
            if self.buf.is_empty() {
                break;
            }
            // Compressed protocol
            if self.buf.starts_with(FIXCOMP_MARKER) {
                match fixcomp::fixcomp_frame_length(&self.buf) {
                    fixcomp::FrameLength::Complete(total) if self.buf.len() >= total => {
                        let msg: Vec<u8> = self.buf.drain(..total).collect();
                        frames.push(Frame::FixComp(msg));
                        continue;
                    }
                    // Still arriving. Worth waiting for.
                    fixcomp::FrameLength::Incomplete | fixcomp::FrameLength::Complete(_) => break,
                    // Its own header does not read, so no number of further
                    // bytes completes it. Waited on, it sat at the head of the
                    // buffer and every acknowledgement and fill behind it was
                    // never extracted, on a connection that went on reading as
                    // alive. Stepped over instead, onto the next header.
                    fixcomp::FrameLength::Unreadable => {
                        let head_n = self.buf.len().min(64);
                        let head_ascii: String = self.buf[..head_n]
                            .iter()
                            .map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '.' })
                            .collect();
                        // Onto the next header of any kind this reads, not
                        // only the next compressed or FIX one: a binary or
                        // control frame delivered in the same slice sits at a
                        // lower offset than either, and searching past it
                        // dropped a quote or a token frame that had arrived
                        // intact.
                        let next = next_header(&self.buf[1..]).map(|p| p + 1);
                        // Where no whole header is here, the tail may still be
                        // the front of one: a read ends where it ends, and it
                        // can end in the middle of `8=FIX`. Dropped to the end
                        // of the buffer, those bytes went with it and the
                        // frame they began could never be recognised when the
                        // rest of it arrived. What is kept back is a marker's
                        // length short of one, which is the most that can be a
                        // prefix without being a match.
                        let keep = FIXCOMP_MARKER.len() - 1;
                        let dropped = next
                            .unwrap_or_else(|| self.buf.len().saturating_sub(keep))
                            .max(1);
                        log::warn!(
                            "extract_frames: a compressed frame states a length that does not \
                             read; dropping {dropped}B to the next header. \
                             first {head_n}B ascii={head_ascii:?}",
                        );
                        self.buf.drain(..dropped);
                        discarded += dropped;
                        continue;
                    }
                }
            }

            // Find earliest message start among all recognized headers.
            // 8=1 (token-auth state) and 8=X (encrypted control) share the
            // length-prefixed, trailer-free framing of 8=O. Recognizing them
            // here keeps them out of the buf.clear() arm below, which would
            // otherwise wipe any FIXCOMP frame queued behind them in the same
            // recv slice.
            let fix_pos = find_subsequence(&self.buf, b"8=FIX.");
            let o_pos = find_subsequence(&self.buf, b"8=O\x01");
            let one_pos = find_subsequence(&self.buf, b"8=1\x01");
            let x_pos = find_subsequence(&self.buf, b"8=X\x01");
            // The compressed header belongs in this scan as much as the rest.
            // Left out, a stream of nothing but compressed frames has no
            // header to resynchronise onto: one cut header drops the `8=` that
            // starts it, and every frame behind it is then dumped as garbage
            // while the socket keeps delivering bytes and the connection keeps
            // reading as alive.
            let comp_pos = find_subsequence(&self.buf, FIXCOMP_MARKER);

            let earliest = [fix_pos, o_pos, one_pos, x_pos, comp_pos]
                .into_iter()
                .flatten()
                .min();
            let earliest = match earliest {
                Some(e) => e,
                None => {
                    // Enough of what is discarded to tell a framing error from
                    // a genuinely malformed frame, which a 64-byte prefix is
                    // not — and no more than that.
                    //
                    // This buffer holds whatever the socket has delivered and
                    // not yet been read out of, which runs to megabytes. Every
                    // byte of it written out as hex is twice that again, built
                    // as one string and written as one line: a single discard
                    // of eight megabytes was measured at sixteen million
                    // characters on one line. That is an allocation and a line
                    // nobody reads, on the path that is already dealing with
                    // something unexpected — and enough of them to stall a
                    // reader that handles output a line at a time.
                    const MOST_HEX_BYTES: usize = 512;
                    let shown = self.buf.len().min(MOST_HEX_BYTES);
                    let full_hex: String = self.buf[..shown]
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect();
                    let head_n = self.buf.len().min(64);
                    let head_ascii: String = self.buf[..head_n]
                        .iter()
                        .map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '.' })
                        .collect();
                    // The tail could be a header the socket has not finished
                    // delivering. Anything shorter than the longest marker is
                    // kept: clearing it discards a genuine frame — an ack, a
                    // fill — that had only been split across two reads, and
                    // leaves the read after it starting mid-header.
                    const LONGEST_MARKER: usize = FIXCOMP_MARKER.len();
                    let keep = self.buf.len().min(LONGEST_MARKER - 1);
                    let dropped = self.buf.len() - keep;
                    if dropped > 0 {
                        log::warn!(
                            "extract_frames: dropping {dropped}B (no header). \
                             first {head_n}B ascii={head_ascii:?} \
                             hex of the first {shown}B={full_hex}",
                        );
                    }
                    self.buf.drain(..dropped);
                    discarded += dropped;
                    break;
                }
            };

            // Skip garbage before earliest message
            if earliest > 0 {
                self.buf.drain(..earliest);
                discarded += earliest;
                continue;
            }

            // 8=O binary and 8=1 / 8=X control: one framing, length-delimited
            // via tag 9 with no checksum trailer. They differ only in what the
            // frame is called downstream, where control frames are ignored.
            if self.buf.starts_with(b"8=O\x01")
                || self.buf.starts_with(b"8=1\x01")
                || self.buf.starts_with(b"8=X\x01")
            {
                let binary = self.buf.starts_with(b"8=O\x01");
                if let Some(total) = binary_msg_length(&self.buf)
                    && self.buf.len() >= total {
                        let msg: Vec<u8> = self.buf.drain(..total).collect();
                        frames.push(if binary { Frame::Binary(msg) } else { Frame::Control(msg) });
                        continue;
                    }
                // As on the FIX branch below: a length that is there and
                // unreadable is not a length still arriving. These three
                // headers are resynchronisation targets, so four such bytes
                // inside a payload put one at the front of the buffer with
                // something other than a length behind it. Waited on, it stays
                // there and every acknowledgement and fill behind it is never
                // extracted, on a connection that goes on reading as alive.
                if tag9_is_unreadable(&self.buf, binary_msg_length) {
                    log::warn!(
                        "extract_frames: a header states an unreadable body \
                         length; resynchronising past it",
                    );
                    self.buf.drain(..1);
                    discarded += 1;
                    continue;
                }
                break; // incomplete
            }

            // FIX.4.1: length-delimited via tag 9, +7 for checksum "10=XXX\x01"
            if self.buf.starts_with(b"8=FIX.") {
                if let Some(total) = fix_msg_length(&self.buf)
                    && self.buf.len() >= total {
                        // Only where the trailer is where the length said it
                        // would be. A length that names some other position is
                        // resynchronised past, as an unreadable one is: it is
                        // the same failure reaching the reader through the
                        // same field, and waiting it out holds a header at the
                        // front of the buffer that no further byte can
                        // complete.
                        if !trailer_is_where_the_length_says(&self.buf, total) {
                            log::warn!(
                                "extract_frames: a FIX header states a body length whose \
                                 trailer is not where it says; resynchronising past it",
                            );
                            self.buf.drain(..1);
                            discarded += 1;
                            continue;
                        }
                        let msg: Vec<u8> = self.buf.drain(..total).collect();
                        frames.push(Frame::Fix(msg));
                        continue;
                    }
                // A length that is there and unreadable is not a length
                // still arriving. Waiting for it holds the header at the front
                // of the buffer indefinitely, and every frame behind it queues
                // unread while the socket keeps delivering and the
                // connection kept reading as alive.
                if tag9_is_unreadable(&self.buf, fix_msg_length) {
                    log::warn!(
                        "extract_frames: a FIX header states an unreadable body \
                         length; resynchronising past it",
                    );
                    self.buf.drain(..1);
                    discarded += 1;
                    continue;
                }
                break; // incomplete
            }

            // Unknown prefix — skip one byte and retry
            self.buf.drain(..1);
            discarded += 1;
        }
        // Unreadable traffic still counts as traffic arriving while the bytes
        // keep coming, so the liveness deadlines never fire on a peer sending
        // nothing this can read. Past what one frame can be, what is arriving
        // is not a frame this lost the start of: the connection is given up
        // the way it is for a frame that failed to verify, and the reconnect
        // path takes it over.
        if frames.is_empty() {
            self.unreadable = self.unreadable.saturating_add(discarded as u64);
            if self.unreadable > MAX_BUFFERED as u64 && !self.read_failed {
                self.read_failed = true;
                log::warn!(
                    "extract_frames: {}B of traffic arrived that this client could not \
                     read; giving up the transport",
                    self.unreadable,
                );
            }
        } else {
            self.unreadable = 0;
        }
        frames
    }

    /// Unsign a received frame using the read IV, chaining the IV.
    ///
    /// `None` means the frame did not verify and must not be parsed. Returning
    /// no message states that in a form a caller cannot discard, which a
    /// validity flag beside the bytes would be: a tampered order ack, fill or
    /// account push would otherwise be applied like an authentic one.
    ///
    /// A failed frame also leaves the IV alone, because the IV it would advance
    /// to is derived from the body the signature just failed to vouch for.
    /// Advancing would let one injected frame steer the receiver's chain and
    /// drop every genuine frame after it.
    ///
    /// This costs the case where a genuine frame is damaged in exactly its
    /// signature: its body is intact, so the derived IV would have been the
    /// sender's true next one, and the connection cannot verify what follows
    /// until it reconnects. That case is indistinguishable from an injection
    /// here, and the reasoning for preferring this side is set out below.
    pub fn unsign(&mut self, msg: &[u8]) -> Option<Vec<u8>> {
        if self.read_key.is_empty() {
            return Some(msg.to_vec()); // no signing configured
        }
        // A frame carrying no signature is accepted unchanged, which is what
        // the client this one replaces does: it classifies a frame as signed
        // by looking for the trailer at the position the length implies, and
        // where that is absent it says so and hands the frame on — no verify
        // and no refusal. Refusing here instead would drop traffic that client
        // delivers. The warning makes the case visible in logs.
        if !fix::carries_signature(msg) {
            log::warn!("inbound frame carries no 8349 signature on a signed connection");
            return Some(msg.to_vec());
        }
        let (undistorted, new_iv, valid) = fix::fix_unsign(msg, &self.read_key, &self.read_iv);
        if !valid {
            // The chain is not advanced. `new_iv` is derived from the received
            // body, and a failed MAC is exactly the statement that this body
            // cannot be vouched for — so advancing would let one injected frame
            // steer the receiver's chain state and drop every genuine frame
            // after it.
            //
            // The cost is a genuine frame whose signature alone was damaged in
            // transit: its body is intact, so `new_iv` would have been the
            // sender's true next one, and holding it back leaves the connection
            // stuck. That case is indistinguishable from an injection at this
            // point, and a channel where a MAC failure has occurred is not one
            // whose state can be inferred either way. Failing closed on
            // unauthenticated input is the side to err on; the connection
            // wanting a teardown rather than a guess is the real answer, and is
            // a larger change than this.
            // And the connection is finished, not just this frame. The chain
            // does not advance past a frame that did not verify, so the sender
            // is now a step ahead for good and every frame after this one
            // fails the same test. Left standing, the socket keeps delivering
            // bytes — which is what the liveness deadlines watch — so it reads
            // as healthy while nothing on it is ever read again. Given up
            // instead, so it is rebuilt.
            self.read_failed = true;
            log::warn!(
                "inbound frame failed signature verification — the chain cannot advance \
                 past it, so this transport is given up to be rebuilt",
            );
            return None;
        }
        self.read_iv = new_iv;
        Some(undistorted)
    }

    /// Test-only: a `Connection` whose writes land on the returned peer socket,
    /// so a test can assert on the bytes an encoder actually puts on the wire.
    #[cfg(test)]
    pub(crate) fn for_test() -> (Connection, std::net::TcpStream) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = std::net::TcpStream::connect(addr).unwrap();
        let (peer, _) = listener.accept().unwrap();
        peer.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        let conn = Connection {
            stream: Stream::Raw(client),
            buf: Vec::new(),
            seq: 0,
            sign_key: Vec::new(),
            sign_iv: Vec::new(),
            read_key: Vec::new(),
            read_iv: Vec::new(),
            connected_host: None,
            logged_in_at: None,
            competing: None,
            heartbeat_secs: None,
            market_data_allowance: 40,
            routing: Default::default(),
            write_failed: false,
            read_failed: false,
            unreadable: 0,
        };
        (conn, peer)
    }

    /// Write a whole frame, or give up on the transport.
    ///
    /// Tracks how much of the frame reached the socket, because that is what
    /// separates a transport that can carry on from one that cannot: a frame
    /// sent in part leaves the signature chain desynchronised and the first
    /// such failure is final, while a write that moved nothing is offered again
    /// here until the frame's budget is spent. Once
    /// final, every later send fails fast rather than putting
    /// more frames on a wire the peer can no longer verify.
    ///
    /// So a failure from here always means the transport has been given up,
    /// and the loop's own sweep of unwritable transports reconnects and replays
    /// what was on them. That is what a caller which does not read the result
    /// of a send is relying on: without it, a frame could be lost while the
    /// transport stayed installed, and nothing would ever put it back.
    fn write_frame(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.write_failed {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "connection abandoned after an incomplete write",
            ));
        }
        // The per-syscall timeout bounds a peer making no progress at all. It
        // does not bound one making a little: every partial write restarts it,
        // so a peer taking a byte just under the timeout holds this thread for
        // as long as it cares to — and this thread is the one that polls the
        // sockets, checks the deadlines and reads the stop flag, so a wedged
        // peer keeps the whole loop while the detector built for it cannot
        // run. The whole frame gets one budget.
        let give_up_at = std::time::Instant::now() + WHOLE_FRAME_TIMEOUT;
        let mut written = 0;
        while written < bytes.len() {
            if std::time::Instant::now() >= give_up_at {
                self.write_failed = true;
                log::warn!(
                    "a frame took longer than {}s to go out and was abandoned part-written",
                    WHOLE_FRAME_TIMEOUT.as_secs(),
                );
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "the peer did not take a whole frame in time",
                ));
            }
            match self.stream.write(&bytes[written..]) {
                Ok(0) => {
                    self.write_failed = true;
                    log::warn!("write returned no progress — abandoning the transport");
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "peer accepted no more of the frame",
                    ));
                }
                Ok(n) => written += n,
                // A signal is not the peer's doing and costs nothing to retry.
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    if write_is_recoverable(&e, written, matches!(self.stream, Stream::Tls(_))) {
                        // Offered again inside the budget the whole frame has,
                        // because nothing above this layer offers it again.
                        // Handed back instead, the frame was simply gone: the
                        // transport stayed installed, so no reconnect and no
                        // replay put it back, and every caller of a send whose
                        // result is not read had been told its request was on
                        // the wire. A peer that takes nothing for the whole
                        // budget finishes the transport at the top of the loop.
                        log::warn!("write made no progress ({e}) — offering the frame again");
                        continue;
                    }
                    self.write_failed = true;
                    log::warn!("write failed after {written} of {} bytes ({e}) \
                        — abandoning the transport", bytes.len());
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    /// Build a FIX message, sign it, and send it. Increments seq and chains sign IV.
    ///
    /// State (seq, sign_iv) is committed only after the frame is fully written.
    /// A write that moved nothing leaves the connection usable and the frame
    /// unsent; one that moved part of a frame finishes the transport, because
    /// the chain the peer verifies against has moved and cannot be rejoined.
    pub fn send_fix(&mut self, fields: &[(u32, &str)]) -> io::Result<()> {
        let next_seq = self.seq + 1;
        let msg = fix::fix_build(fields, next_seq);
        if log::log_enabled!(log::Level::Trace) {
            log::trace!("WIRE> seq={} {}", next_seq, fix::fmt_pipe(&msg));
        }
        let (to_send, next_iv) = if self.sign_key.is_empty() {
            (msg, None)
        } else {
            let (signed, iv) = fix::fix_sign(&msg, &self.sign_key, &self.sign_iv);
            (signed, Some(iv))
        };
        self.write_frame(&to_send)?;
        self.seq = next_seq;
        if let Some(iv) = next_iv {
            self.sign_iv = iv;
        }
        Ok(())
    }

    /// Build a message, compress, sign, and send. For farm subscribe/data messages.
    /// Uses seq=0 (separate seq space from heartbeats).
    ///
    /// State (sign_iv) is committed only after `write_all` returns Ok.
    pub fn send_fixcomp(&mut self, fields: &[(u32, &str)]) -> io::Result<()> {
        let msg = fix::fix_build(fields, 0);
        if log::log_enabled!(log::Level::Trace) {
            log::trace!("WIRE> comp {}", fix::fmt_pipe(&msg));
        }
        let wrapped = fixcomp::fixcomp_build(&msg);
        let (to_send, next_iv) = if self.sign_key.is_empty() {
            (wrapped, None)
        } else {
            let (signed, iv) = fix::fix_sign(&wrapped, &self.sign_key, &self.sign_iv);
            (signed, Some(iv))
        };
        self.write_frame(&to_send)?;
        if let Some(iv) = next_iv {
            self.sign_iv = iv;
        }
        Ok(())
    }

    /// Send raw bytes (pre-built message).
    pub fn send_raw(&mut self, data: &[u8]) -> io::Result<()> {
        self.write_frame(data)?;
        Ok(())
    }

    /// Number of buffered bytes not yet extracted as frames.
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    /// Inject pre-read bytes into the buffer (e.g., leftover from routing response).
    pub fn inject_buf(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }
}

/// Compute total length of a length-prefixed, trailer-free message whose
/// tag-8 header is 4 bytes: `8=O\x01`, `8=1\x01`, or `8=X\x01`, each followed
/// by `9=<body_len>\x01 ...`.
pub(crate) fn binary_msg_length(data: &[u8]) -> Option<usize> {
    let (soh_pos, body_len) = stated_body_length(data)?;
    // The length is whatever the peer wrote, so a total that does not fit is
    // a length no frame can have rather than something to add anyway: added
    // unchecked it aborted the process where overflow is checked and framed
    // from a wrapped offset where it is not.
    soh_pos.checked_add(1)?.checked_add(body_len)
}

/// Compute total length of a `8=FIX.4.1\x01 9=<body_len>\x01 ...` message.
/// Includes the 7-byte checksum trailer `10=XXX\x01`.
pub(crate) fn fix_msg_length(data: &[u8]) -> Option<usize> {
    let (soh_pos, body_len) = stated_body_length(data)?;
    // header up to and including SOH after tag 9, + body + "10=XXX\x01" (7 bytes)
    soh_pos.checked_add(8)?.checked_add(body_len)
}

/// Where tag 9 must begin: straight after the tag-8 field, which is the first
/// field of every frame on this wire.
///
/// `None` while the tag-8 field is still arriving.
fn where_tag_9_begins(data: &[u8]) -> Option<usize> {
    let rest = data.strip_prefix(b"8=")?;
    rest.iter().position(|&b| b == SOH).map(|p| p + 3)
}

/// The body length a header states, with the offset of the separator that ends
/// the field.
///
/// Read at the one position the length can be. Tag 9 follows tag 8 and nothing
/// sits between them, so searching for the first `9=` in the buffer finds the
/// length only by luck: it matches inside `49=` as readily, and it matches a
/// length the peer wrote into a payload. A total taken from either reaches
/// over the frames queued behind this one, and an acknowledgement and a fill
/// were drained as part of it and never seen.
///
/// `None` where tag 9 is not where the header puts it, has not arrived whole,
/// or states something that is not a length.
fn stated_body_length(data: &[u8]) -> Option<(usize, usize)> {
    let at = where_tag_9_begins(data)?;
    if !data.get(at..)?.starts_with(b"9=") {
        return None;
    }
    let soh_pos = data[at..].iter().position(|&b| b == SOH).map(|p| at + p)?;
    let body_len: usize = std::str::from_utf8(&data[at + 2..soh_pos]).ok()?.parse().ok()?;
    Some((soh_pos, body_len))
}

/// Whether the trailer sits where the stated body length says it does.
///
/// The client this one replaces requires exactly that before it will call a
/// buffer a frame: `<SOH>10=` at the position the length implies, and a final
/// `<SOH>`. A buffer whose trailer is elsewhere is not a frame to it, whatever
/// tag 9 claims.
///
/// Trusted without the check, a stated length longer than the message swallows
/// the frames behind it, and one shorter cuts the message and leaves its tail
/// to be read as the start of the next — and neither is distinguishable from a
/// frame still arriving, so both wait on bytes that were already read.
///
/// The three digits are not recomputed. That client does not check them
/// either: it generates a checksum outbound and verifies none inbound, and
/// refusing a frame it accepts is not the same client.
///
/// Answers only where the whole of the stated length is held; before that
/// there is nothing at the position to look at.
pub(super) fn trailer_is_where_the_length_says(data: &[u8], total: usize) -> bool {
    if data.len() < total || total < 8 {
        return false;
    }
    let at = total - 8;
    data[at] == SOH && &data[at + 1..at + 4] == b"10=" && data[total - 1] == SOH
}

/// The offset of the earliest header this reader recognises, if there is one.
///
/// The same set the frame scan uses, so resynchronising after an unreadable
/// frame lands on whatever comes next rather than only on the two kinds a
/// particular caller was looking for.
fn next_header(data: &[u8]) -> Option<usize> {
    [
        find_subsequence(data, FIXCOMP_MARKER),
        find_subsequence(data, b"8=FIX."),
        find_subsequence(data, b"8=O\x01"),
        find_subsequence(data, b"8=1\x01"),
        find_subsequence(data, b"8=X\x01"),
    ]
    .into_iter()
    .flatten()
    .min()
}

/// Whether the tag 9 at the front of this buffer has arrived in full and no
/// frame length can be got from it.
///
/// That is the case a length-framed reader cannot wait out: the field is
/// complete, so no further byte will make it read.
///
/// `total` is the same function the extractor frames this header with, and it
/// is asked rather than second-guessed. Reimplemented here, the two answered
/// differently on a length that parses and then overflows the total: the
/// extractor refused to frame it and this said it was readable, so the header
/// stayed at the front of the buffer and every frame behind it queued unread
/// on a connection that went on reading as alive.
fn tag9_is_unreadable(data: &[u8], total: fn(&[u8]) -> Option<usize>) -> bool {
    let Some(at) = where_tag_9_begins(data) else {
        return false; // the tag-8 field is still arriving
    };
    let Some(head) = data.get(at..) else { return false };
    if head.len() < 2 {
        return false; // still arriving
    }
    if !head.starts_with(b"9=") {
        // The header puts the length here and this is not one. No later byte
        // moves it, so this is not a frame however long it is waited on.
        return true;
    }
    if !head.contains(&SOH) {
        return false; // the field itself is still arriving
    }
    total(data).is_none()
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::fix::fix_build;
    use crate::protocol::fixcomp::fixcomp_build;

    /// A Connection-like buffer for frame-extraction tests. A `TlsStream` is
    /// not constructible here, so the framing functions are exercised directly.

    #[test]
    fn fix_msg_length_basic() {
        let msg = fix_build(&[(35, "0")], 1);
        let len = fix_msg_length(&msg);
        assert_eq!(len, Some(msg.len()));
    }

    #[test]
    fn fix_msg_length_incomplete() {
        let msg = fix_build(&[(35, "0")], 1);
        assert_eq!(fix_msg_length(&msg[..10]), None);
    }

    /// A stated length whose trailer is somewhere else is not a length.
    ///
    /// The client this one replaces asks for the trailer at the position the
    /// length implies before it will call a buffer a frame. Trusted without
    /// that, a length longer than the message swallows the frames behind it,
    /// and a length shorter cuts the message and leaves its tail to be read as
    /// the start of the next.
    #[test]
    fn a_length_whose_trailer_is_elsewhere_is_not_a_frame() {
        let good = fix_build(&[(35, "0")], 1);
        assert!(
            trailer_is_where_the_length_says(&good, good.len()),
            "a message the venue built states its own length",
        );
        // A length two bytes short: the trailer is no longer at the position
        // it names, and the two bytes it leaves behind start no frame.
        assert!(!trailer_is_where_the_length_says(&good, good.len() - 2));
        // And one longer than what is held answers nothing yet, because there
        // is nothing at that position to look at.
        assert!(!trailer_is_where_the_length_says(&good, good.len() + 2));
    }

    /// And the reader resynchronises past it rather than waiting.
    ///
    /// The length is readable, so no further byte can make it parse and no
    /// further byte can put the trailer where it claims — the header would sit
    /// at the front of the buffer for the life of the connection, with every
    /// frame behind it unread while the socket kept delivering.
    #[test]
    fn a_frame_whose_stated_length_lies_does_not_hold_the_ones_behind_it() {
        let honest = fix_build(&[(35, "0")], 1);
        // A header stating a body two bytes longer than it carries, with a
        // whole frame behind it.
        let mut lying = honest.clone();
        let at9 = find_subsequence(&lying, b"9=").expect("tag 9");
        let soh = lying[at9..].iter().position(|&b| b == SOH).expect("its value ends") + at9;
        let stated: usize = std::str::from_utf8(&lying[at9 + 2..soh]).unwrap().parse().unwrap();
        let wider = format!("{}", stated + 2);
        lying.splice(at9 + 2..soh, wider.bytes());

        let mut buf = lying;
        buf.extend_from_slice(&honest);
        let mut conn = test_connection_with_buf(buf);
        let frames = conn.extract_frames();
        assert!(
            frames.iter().any(|f| matches!(f, Frame::Fix(m) if *m == honest)),
            "the frame behind the lying header is read: {} frame(s)", frames.len(),
        );
    }

    /// A length that is there and unreadable is not a length still arriving.
    /// Waited out as though it were, the header sat at the front of the buffer
    /// for good and every frame behind it queued unread while the socket kept
    /// delivering and the connection kept reading as alive.
    #[test]
    fn a_body_length_that_cannot_be_read_is_not_one_still_arriving() {
        let whole = fix_build(&[(35, "0")], 1);
        assert!(!tag9_is_unreadable(&whole, fix_msg_length), "a good header is readable");
        // A header cut inside its own length field: more bytes would finish it.
        assert!(!tag9_is_unreadable(&whole[..12], fix_msg_length), "still arriving");

        // The field is complete and states something that is not a number.
        assert!(tag9_is_unreadable(b"8=FIX.4.1\x019=00X4\x0135=0\x01", fix_msg_length));
        assert!(tag9_is_unreadable(b"8=FIX.4.1\x019=\x0135=0\x01", fix_msg_length));

        // A header long past where its length would sit, carrying none.
        assert!(tag9_is_unreadable(b"8=FIX.4.1\x0135=0\x0134=000001\x01", fix_msg_length));
    }

    #[test]
    fn binary_msg_length_basic() {
        // Build a minimal 8=O message
        let body = b"35=P\x01data";
        let msg = format!("8=O\x019={}\x01", body.len());
        let mut full = msg.into_bytes();
        full.extend_from_slice(body);
        assert_eq!(binary_msg_length(&full), Some(full.len()));
    }

    /// A length read from a `9=` the peer wrote into a payload is not this
    /// frame's length.
    ///
    /// The header is a fixed width, so tag 9 has one position. Searched for
    /// instead, the first `9=` anywhere in the buffer answered — and a total
    /// taken from it reached over whatever was queued behind this frame, so an
    /// acknowledgement or a fill sitting there was drained as part of this one
    /// and never seen. The FIX reader beside this one bounds its own search;
    /// this one did not.
    #[test]
    fn a_binary_header_reads_its_length_where_the_length_is() {
        // A well-formed frame still reads.
        let body = b"35=P\x01data";
        let mut good = format!("8=O\x019={}\x01", body.len()).into_bytes();
        good.extend_from_slice(body);
        assert_eq!(binary_msg_length(&good), Some(good.len()));

        // The same header with no length where one belongs, and a `9=` further
        // along naming a total that covers the frame behind it.
        let mut lying = b"8=O\x01zz\x019=999\x01".to_vec();
        lying.extend_from_slice(&good);
        assert_eq!(
            binary_msg_length(&lying), None,
            "a length found past its position is not this frame's length",
        );
    }

    #[test]
    fn binary_msg_length_incomplete() {
        // binary_msg_length returns the expected total, caller checks buf.len() >=
        // total
        let msg = b"8=O\x019=50\x01short";
        let expected_total = binary_msg_length(msg).unwrap();
        assert!(msg.len() < expected_total); // data too short → incomplete
    }

    #[test]
    fn fixcomp_length_basic() {
        let inner = fix_build(&[(35, "0")], 1);
        let comp = fixcomp_build(&inner);
        // fixcomp_length is from fixcomp module, already tested there
        assert_eq!(fixcomp::fixcomp_length(&comp), Some(comp.len()));
    }

    #[test]
    fn frame_extraction_fix() {
        let msg1 = fix_build(&[(35, "0")], 1);
        let msg2 = fix_build(&[(35, "A"), (108, "10")], 2);
        let mut buf = msg1.clone();
        buf.extend_from_slice(&msg2);

        // Simulate extraction by testing the length functions
        let len1 = fix_msg_length(&buf).unwrap();
        assert_eq!(len1, msg1.len());
        let remaining = &buf[len1..];
        let len2 = fix_msg_length(remaining).unwrap();
        assert_eq!(len2, msg2.len());
    }

    #[test]
    fn frame_extraction_mixed_binary_and_fix() {
        let body = b"35=P\x01tickdata";
        let o_msg = format!("8=O\x019={}\x01", body.len());
        let mut o_full = o_msg.into_bytes();
        o_full.extend_from_slice(body);

        let fix_msg = fix_build(&[(35, "8"), (11, "1001")], 3);

        let mut buf = o_full.clone();
        buf.extend_from_slice(&fix_msg);

        // First message is 8=O
        assert!(buf.starts_with(b"8=O\x01"));
        let len1 = binary_msg_length(&buf).unwrap();
        assert_eq!(len1, o_full.len());

        let remaining = &buf[len1..];
        assert!(remaining.starts_with(b"8=FIX."));
        let len2 = fix_msg_length(remaining).unwrap();
        assert_eq!(len2, fix_msg.len());
    }

    #[test]
    fn find_subsequence_basic() {
        assert_eq!(find_subsequence(b"hello world", b"world"), Some(6));
        assert_eq!(find_subsequence(b"hello world", b"xyz"), None);
        assert_eq!(find_subsequence(b"8=FIX.4.1\x01", b"8=FIX."), Some(0));
    }

    /// Helper: create a Connection with a dummy TCP stream for buffer tests.
    /// Connects to a local listener, which yields a valid `TcpStream`.
    fn test_connection_with_buf(buf: Vec<u8>) -> Connection {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stream = std::net::TcpStream::connect(addr).unwrap();
        stream.set_nonblocking(true).unwrap();
        Connection {
            stream: Stream::Raw(stream),
            buf,
            seq: 0,
            sign_key: Vec::new(),
            sign_iv: Vec::new(),
            read_key: Vec::new(),
            read_iv: Vec::new(),
            connected_host: None,
            logged_in_at: None,
            competing: None,
            heartbeat_secs: None,
            market_data_allowance: 40,
            routing: Default::default(),
            write_failed: false,
            read_failed: false,
            unreadable: 0,
        }
    }

    /// A header split across two reads is a frame, not garbage.
    ///
    /// TCP delivers bytes, not messages. A read that ends part way through
    /// `8=FIX.` leaves a buffer with no header in it, and clearing that buffer
    /// throws away the frame — an ack, a fill — that the next read completes.
    #[test]
    fn a_header_split_across_two_reads_still_arrives() {
        let inner = fix_build(&[(35, "0")], 1);

        // The socket hands over the first three bytes of the header.
        let mut conn = test_connection_with_buf(inner[..3].to_vec());
        assert!(conn.extract_frames().is_empty(), "nothing whole has arrived yet");

        // The rest follows, and the frame reads as one.
        conn.buf.extend_from_slice(&inner[3..]);
        let frames = conn.extract_frames();
        assert_eq!(frames.len(), 1, "the frame survived being split");
        match &frames[0] {
            Frame::Fix(data) => assert_eq!(data, &inner),
            other => panic!("expected Frame::Fix, got {other:?}"),
        }
    }

    #[test]
    fn frame_extraction_fixcomp() {
        let inner = fix_build(&[(35, "0")], 1);
        let comp = fixcomp_build(&inner);
        let mut conn = test_connection_with_buf(comp.clone());
        let frames = conn.extract_frames();
        assert_eq!(frames.len(), 1);
        match &frames[0] {
            Frame::FixComp(data) => assert_eq!(data, &comp),
            other => panic!("expected Frame::FixComp, got {other:?}"),
        }
    }

    /// A read boundary can land anywhere, including inside a header. The
    /// bytes it stops on are the start of a frame, not garbage — and on a
    /// connection carrying nothing but compressed frames, dropping them takes
    /// every frame behind them with it: there is no other header left to
    /// resynchronise on, and the socket goes on delivering bytes that never
    /// reach the caller while the connection still reads as alive.
    #[test]
    fn a_compressed_header_split_by_a_read_boundary_still_arrives() {
        let comp = fixcomp_build(&fix_build(&[(35, "0")], 1));
        // Every place a read can stop inside the ten-byte header.
        for split in 1..FIXCOMP_MARKER.len() {
            let mut conn = test_connection_with_buf(comp[..split].to_vec());
            assert!(conn.extract_frames().is_empty(), "split {split}");
            conn.buf.extend_from_slice(&comp[split..]);
            let frames = conn.extract_frames();
            assert_eq!(frames.len(), 1, "split {split} lost the frame");
            match &frames[0] {
                Frame::FixComp(data) => assert_eq!(data, &comp, "split {split}"),
                other => panic!("split {split}: expected FixComp, got {other:?}"),
            }
        }
    }

    /// And a header the buffer has already lost the start of does not take the
    /// whole frames queued behind it.
    #[test]
    fn a_cut_compressed_header_does_not_eat_the_frames_behind_it() {
        let comp = fixcomp_build(&fix_build(&[(35, "0")], 1));
        let mut buf = comp[7..].to_vec();   // a header already missing its start
        buf.extend_from_slice(&comp);
        buf.extend_from_slice(&comp);
        let mut conn = test_connection_with_buf(buf);
        let frames = conn.extract_frames();
        assert_eq!(frames.len(), 2, "the two whole frames were dropped with the cut one");
    }

    #[test]
    fn frame_extraction_garbage_before_fix() {
        let msg = fix_build(&[(35, "A"), (108, "10")], 1);
        let mut buf = vec![0xDE, 0xAD, 0xBE, 0xEF, 0xFF];
        buf.extend_from_slice(&msg);
        let mut conn = test_connection_with_buf(buf);
        let frames = conn.extract_frames();
        assert_eq!(frames.len(), 1);
        match &frames[0] {
            Frame::Fix(data) => assert_eq!(data, &msg),
            other => panic!("expected Frame::Fix, got {other:?}"),
        }
    }

    #[test]
    fn frame_extraction_incomplete_fix() {
        let msg = fix_build(&[(35, "D"), (55, "AAPL")], 1);
        // Take only first half of the message
        let half = msg.len() / 2;
        let buf = msg[..half].to_vec();
        let mut conn = test_connection_with_buf(buf);
        let frames = conn.extract_frames();
        assert!(frames.is_empty(), "incomplete message should not produce a frame");
    }

    /// A frame states a length and then never completes: the bytes behind the
    /// statement are a demand for memory, not a frame arriving, because no
    /// frame this protocol carries is that large. Without a bound the wait
    /// grew the buffer until the process died. The read is given up once the
    /// bound is passed, and the caller treats that as a lost connection, the
    /// same path a dead socket takes.
    #[test]
    fn a_stated_length_that_never_completes_is_given_up_not_buffered_forever() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stream = std::net::TcpStream::connect(addr).unwrap();
        stream.set_nonblocking(true).unwrap();
        let (mut peer, _) = listener.accept().unwrap();
        // Both ends non-blocking: a blocking write would wait on the kernel
        // buffer this side fills faster than the bound allows, and the test
        // would hang on its own deadlock instead of reaching the bound.
        peer.set_nonblocking(true).unwrap();
        let mut conn = Connection {
            stream: Stream::Raw(stream),
            buf: Vec::new(),
            seq: 0,
            sign_key: Vec::new(),
            sign_iv: Vec::new(),
            read_key: Vec::new(),
            read_iv: Vec::new(),
            connected_host: None,
            logged_in_at: None,
            competing: None,
            heartbeat_secs: None,
            market_data_allowance: 40,
            routing: Default::default(),
            write_failed: false,
            read_failed: false,
            unreadable: 0,
        };
        // States near a gigabyte; the largest frame this protocol carries is
        // a compressed one, and nothing is used that inflates past a small
        // fraction of this.
        conn.buf.extend_from_slice(b"8=FIX.4.1\x019=999999999\x01");

        let chunk = vec![0xABu8; RECV_BUF_SIZE];
        let mut refused = false;
        let mut sent = 0usize;
        let mut spins = 0usize;
        while sent <= MAX_BUFFERED + 8 * 1024 * 1024 && spins < 1_000_000 {
            spins += 1;
            if let Ok(n) = peer.write(&chunk) {
                sent += n;
            }
            match conn.try_recv() {
                Err(_) => { refused = true; break; }
                Ok(0) => std::thread::sleep(std::time::Duration::from_millis(1)),
                Ok(_) => {
                    assert!(
                        conn.buffered() <= MAX_BUFFERED,
                        "the buffer grew past the bound: {}B",
                        conn.buffered(),
                    );
                }
            }
        }
        assert!(refused, "the read was never given up on the uncompleting frame");
    }

    #[test]
    fn frame_extraction_two_fix_back_to_back() {
        let msg1 = fix_build(&[(35, "0")], 1);
        let msg2 = fix_build(&[(35, "D"), (55, "MSFT"), (54, "1")], 2);
        let mut buf = msg1.clone();
        buf.extend_from_slice(&msg2);
        let mut conn = test_connection_with_buf(buf);
        let frames = conn.extract_frames();
        assert_eq!(frames.len(), 2);
        match &frames[0] {
            Frame::Fix(data) => assert_eq!(data, &msg1),
            other => panic!("expected Frame::Fix for msg1, got {other:?}"),
        }
        match &frames[1] {
            Frame::Fix(data) => assert_eq!(data, &msg2),
            other => panic!("expected Frame::Fix for msg2, got {other:?}"),
        }
    }

    #[test]
    fn frame_extraction_binary_8o() {
        let body = b"35=P\x01somedata";
        let header = format!("8=O\x019={}\x01", body.len());
        let mut msg = header.into_bytes();
        msg.extend_from_slice(body);
        let mut conn = test_connection_with_buf(msg.clone());
        let frames = conn.extract_frames();
        assert_eq!(frames.len(), 1);
        match &frames[0] {
            Frame::Binary(data) => assert_eq!(data, &msg),
            other => panic!("expected Frame::Binary, got {other:?}"),
        }
    }

    /// Build a length-prefixed, trailer-free control frame (`8=1` / `8=X`).
    fn build_control_frame(tag8: &str, body: &[u8]) -> Vec<u8> {
        let header = format!("8={}\x019={}\x01", tag8, body.len());
        let mut msg = header.into_bytes();
        msg.extend_from_slice(body);
        msg
    }

    #[test]
    fn frame_extraction_control_8_1() {
    // 8=1 token-auth state message (35=X family). Mirrors slice 2.
        let msg = build_control_frame("1", b"35=X\x011137=ABCDEF\x01");
        let mut conn = test_connection_with_buf(msg.clone());
        let frames = conn.extract_frames();
        assert_eq!(frames.len(), 1);
        match &frames[0] {
            Frame::Control(data) => assert_eq!(data, &msg),
            other => panic!("expected Frame::Control, got {other:?}"),
        }
        assert_eq!(conn.buffered(), 0, "no bytes should be left buffered");
    }

    #[test]
    fn frame_extraction_control_8_x() {
        // 8=X encrypted control / auth state-machine message.
        let msg = build_control_frame("X", b"35=X\x01encctl\x01");
        let mut conn = test_connection_with_buf(msg.clone());
        let frames = conn.extract_frames();
        assert_eq!(frames.len(), 1);
        match &frames[0] {
            Frame::Control(data) => assert_eq!(data, &msg),
            other => panic!("expected Frame::Control, got {other:?}"),
        }
        assert_eq!(conn.buffered(), 0);
    }

    #[test]
    fn frame_extraction_control_then_fixcomp_zero_loss() {
        // acceptance: an 8=1 control frame ahead of a FIXCOMP frame in
        // the same buffer must NOT trigger buf.clear() — the FIXCOMP queued
        // behind it has to survive byte-for-byte.
        let control = build_control_frame("1", b"35=X\x019=0045\x01PASSED\x01");
        let inner = fix_build(&[(35, "0")], 1);
        let comp = fixcomp_build(&inner);

        let mut buf = control.clone();
        buf.extend_from_slice(&comp);
        let mut conn = test_connection_with_buf(buf);

        let frames = conn.extract_frames();
        assert_eq!(frames.len(), 2, "control + fixcomp should both extract");
        match &frames[0] {
            Frame::Control(data) => assert_eq!(data, &control),
            other => panic!("expected Frame::Control first, got {other:?}"),
        }
        match &frames[1] {
            Frame::FixComp(data) => assert_eq!(data, &comp),
            other => panic!("expected Frame::FixComp second, got {other:?}"),
        }
        assert_eq!(conn.buffered(), 0);
    }

    #[test]
    fn frame_extraction_incomplete_control() {
        // A partial 8=1 frame must wait for more bytes, not drop the buffer.
        let msg = build_control_frame("1", b"35=X\x01partialbodythatislong\x01");
        let half = msg.len() / 2;
        let buf = msg[..half].to_vec();
        let mut conn = test_connection_with_buf(buf);
        let frames = conn.extract_frames();
        assert!(frames.is_empty(), "incomplete control frame should not produce a frame");
        assert!(conn.buffered() > 0, "partial frame must stay buffered, not be cleared");
    }

    #[test]
    fn find_subsequence_needle_at_start() {
        assert_eq!(find_subsequence(b"hello world", b"hello"), Some(0));
    }

    #[test]
    fn find_subsequence_needle_at_end() {
        assert_eq!(find_subsequence(b"hello world", b"world"), Some(6));
    }

    #[test]
    fn find_subsequence_overlapping() {
        // "aaa" in "aaaa" — should find at position 0 (first match)
        assert_eq!(find_subsequence(b"aaaa", b"aaa"), Some(0));
    }

    #[test]
    #[should_panic(expected = "window size must be non-zero")]
    fn find_subsequence_empty_needle() {
        // windows(0) panics, so empty needle panics
        find_subsequence(b"hello", b"");
    }

    /// Build a connection with signing configured, over a loopback pair.
    fn signed_conn(key: &[u8], iv: &[u8]) -> Connection {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (_peer, _) = listener.accept().unwrap();
        let mut conn = Connection::new_raw(stream).unwrap();
        conn.read_key = key.to_vec();
        conn.read_iv = iv.to_vec();
        conn
    }

    /// A frame that does not verify must not reach a parser. A validity flag
    /// beside the bytes can be discarded by a caller; no message cannot.
    #[test]
    fn a_frame_that_fails_verification_is_not_returned() {
        let key = b"0123456789abcdef";
        let iv = vec![0u8; 16];
        let (frame, _) = crate::protocol::fix::fix_sign(&fix_build(&[(35, "8")], 1), key, &iv);

        let mut conn = signed_conn(key, &iv);
        assert!(conn.unsign(&frame).is_some(), "the genuine frame verifies");

        // Flip a byte in the body, leaving the signature tag in place.
        let mut tampered = frame.clone();
        let at = tampered.len() / 2;
        tampered[at] ^= 0x01;
        let mut conn = signed_conn(key, &iv);
        assert!(conn.unsign(&tampered).is_none(), "a tampered frame is dropped");
    }

    /// A frame that fails is dropped without moving the chain, so a genuine
    /// frame after it still verifies. The batch is the case that matters —
    /// good, bad, good — because an injected frame between two authentic ones
    /// is what advancing on unauthenticated input would let poison the rest.
    #[test]
    fn an_injected_frame_does_not_poison_the_authentic_ones_around_it() {
        let key = b"0123456789abcdef";
        let iv0 = vec![0u8; 16];

        // Two frames as a sender produces them: the second signed from the IV
        // the first chained to.
        let (first, iv1) =
            crate::protocol::fix::fix_sign(&fix_build(&[(35, "8")], 1), key, &iv0);
        let (second, _) =
            crate::protocol::fix::fix_sign(&fix_build(&[(35, "8")], 2), key, &iv1);

        // Something else entirely, arriving between them.
        let (mut injected, _) =
            crate::protocol::fix::fix_sign(&fix_build(&[(35, "8"), (58, "x")], 9), b"wrongkey00000000", &iv1);
        let at = find_subsequence(&injected, b"\x018349=").expect("signed") + 6;
        injected[at] = if injected[at] == b'0' { b'1' } else { b'0' };

        let mut conn = signed_conn(key, &iv0);
        assert!(conn.unsign(&first).is_some(), "the first authentic frame");
        assert_eq!(conn.read_iv, iv1, "and it advanced the chain");

        assert!(conn.unsign(&injected).is_none(), "the injected frame is dropped");
        assert_eq!(conn.read_iv, iv1, "without moving the chain");

        assert!(
            conn.unsign(&second).is_some(),
            "so the authentic frame after it still verifies",
        );
    }

    /// Two shapes that must keep working, because refusing either would drop
    /// real traffic rather than protect anything: an unsigned connection, and a
    /// frame carrying no signature tag on a signed one.
    #[test]
    fn unsigned_connections_and_untagged_frames_still_pass() {
        let plain = fix_build(&[(35, "0")], 1);

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (_peer, _) = listener.accept().unwrap();
        let mut conn = Connection::new_raw(stream).unwrap();
        assert_eq!(conn.unsign(&plain), Some(plain.clone()), "no key configured");

        let mut conn = signed_conn(b"0123456789abcdef", &[0u8; 16]);
        assert_eq!(conn.unsign(&plain), Some(plain), "no 8349 tag on the frame");
    }

    /// A frame that fails to verify finishes the connection for reading.
    ///
    /// The chain advances only on a frame that verified, so one that did not
    /// leaves the receiver a step behind a sender that carried on — every
    /// frame after it fails the same way. Dropped one at a time the socket
    /// keeps delivering bytes, which is what the liveness deadlines watch, so
    /// it reads as healthy while nothing on it is read again. It has to be
    /// given up instead.
    #[test]
    fn a_frame_that_fails_to_verify_finishes_the_connection() {
        let mut conn = signed_conn(b"0123456789abcdef", &[0u8; 16]);
        assert!(!conn.read_failed(), "nothing has failed yet");

        // Carries the signature tag, so it is verified — and does not verify.
        let forged = fix_build(&[(35, "0"), (8349, "not-a-real-signature")], 1);
        assert_eq!(conn.unsign(&forged), None, "an unverifiable frame is not passed on");
        assert!(conn.read_failed(), "and the transport is finished for reading");
    }

    /// And the shape a value-quoting test cannot reach: the six bytes
    /// separator-`8349=` sitting inside a length-prefixed raw block.
    ///
    /// The test below quotes the tag in an ordinary value, where no separator
    /// precedes it — so a needle looking for one passed that case without ever
    /// being right. A raw block carries whatever bytes the venue put in it,
    /// separators among them. Matched as text the frame went to a verify whose
    /// own field walk found no signature field, which is a failed verify, and a
    /// failed verify gives the transport up for good — then the reconnect
    /// replays the same content and it happens again.
    #[test]
    fn a_raw_block_carrying_the_signature_bytes_is_not_a_signature() {
        // 95=<len> 96=<len bytes>, with the six bytes inside the block.
        let raw = b"\x018349=DEADBEEF";
        let mut frame = Vec::new();
        frame.extend_from_slice(b"8=FIX.4.1\x019=");
        let body_len = 5 + format!("{}", raw.len()).len() + 1 + 3 + raw.len() + 1;
        frame.extend_from_slice(format!("{body_len:04}\x01").as_bytes());
        frame.extend_from_slice(format!("95={}\x0196=", raw.len()).as_bytes());
        frame.extend_from_slice(raw);
        frame.push(SOH);

        let mut conn = signed_conn(b"0123456789abcdef", &[0u8; 16]);
        assert_eq!(
            conn.unsign(&frame),
            Some(frame.clone()),
            "a raw block's contents are not a signature field",
        );
        assert!(
            !conn.read_failed(),
            "so the transport is not given up over a frame the venue sent",
        );
    }

    /// The same rule at the pre-check. An *unsigned* frame quoting the tag in a
    /// value must not be routed into verification at all: it carries no
    /// signature field, so it would be judged invalid and dropped. The signed
    /// case below cannot catch this — its pre-check passes under either needle.
    #[test]
    fn an_unsigned_frame_quoting_the_signature_tag_is_not_verified() {
        let quoting = fix_build(&[(35, "8"), (58, "rejected: 8349= missing")], 1);

        let mut conn = signed_conn(b"0123456789abcdef", &[0u8; 16]);
        assert_eq!(
            conn.unsign(&quoting),
            Some(quoting.clone()),
            "the tag text in a value is not a signature field",
        );
    }

    /// A signature is a field, not a substring. A legitimate signed frame whose
    /// *value* happens to contain the same text — a reject reason quoting it,
    /// say — was reported invalid, and enforcing that verdict would drop it.
    #[test]
    fn a_signed_frame_quoting_the_signature_tag_in_a_value_still_verifies() {
        let key = b"0123456789abcdef";
        let iv = vec![0u8; 16];
        let quoting = fix_build(&[(35, "8"), (58, "rejected: 8349= missing")], 1);
        let (frame, _) = crate::protocol::fix::fix_sign(&quoting, key, &iv);

        let mut conn = signed_conn(key, &iv);
        assert!(
            conn.unsign(&frame).is_some(),
            "the tag text inside a value is not the signature field",
        );
    }
    /// The hot loop is one thread driving three transports, so no send may
    /// block it without bound. A peer that stops draining without closing
    /// blocks an unbounded `write_all`, and with the thread blocked liveness
    /// cannot be evaluated, shutdown cannot be serviced and the reconnect
    /// cannot be polled. The heartbeat that detects a wedged peer is itself a
    /// write, so the detector would not run in the case it exists for.
    #[test]
    fn a_write_timeout_is_configured_on_both_constructors() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (_peer, _) = listener.accept().unwrap();

        let probe = stream.try_clone().unwrap();
        let mut conn = Connection::new_raw(stream).unwrap();
        assert_eq!(
            probe.write_timeout().unwrap(), Some(WRITE_TIMEOUT),
            "a send must be bounded, or one stalled peer stops the whole loop",
        );
        assert_eq!(
            probe.read_timeout().unwrap(), Some(std::time::Duration::from_millis(1)),
            "and the read cadence is unchanged",
        );
        assert!(conn.send_raw(b"x").is_ok(), "and a fresh connection writes");

        // The TLS constructor cannot be built without a real handshake, so it
        // is held to the same rule by construction: both constructors reach the
        // socket through `from_stream`, so there is one place the bound can be
        // dropped from and this probe covers it.
    }

    /// The first write failure is final. A bounded write can time out after
    /// part of a frame is already on the wire, and outbound frames are
    /// HMAC-chained — so anything sent afterwards is signed from state the peer
    /// does not share. There is no resuming mid-frame.
    #[test]
    fn a_failed_write_finishes_the_transport() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (peer, _) = listener.accept().unwrap();
        let mut conn = Connection::new_raw(stream).unwrap();

        // Peer gone: the write fails rather than blocking.
        drop(peer);
        let big = vec![0u8; 1 << 20];
        let first = conn.send_raw(&big);
        if first.is_ok() {
            // The kernel buffered it; a second one past the buffer will not be.
            let _ = conn.send_raw(&big);
            let _ = conn.send_raw(&big);
        }


        // And every later send is refused by this connection rather than
        // attempted on the socket. Asserted on the message, not the kind: a
        // dead peer produces a broken pipe of its own, so only this client's
        // wording separates "refused without trying" from "tried and failed".
        for label in ["send_raw", "send_fix", "send_fixcomp"] {
            let err = match label {
                "send_raw" => conn.send_raw(b"x").unwrap_err(),
                "send_fix" => conn.send_fix(&[(35, "0")]).unwrap_err(),
                _ => conn.send_fixcomp(&[(35, "0")]).unwrap_err(),
            };
            assert!(
                err.to_string().contains("abandoned after an incomplete write"),
                "{label} must be refused by the connection, not attempted: {err}",
            );
        }

        // The sequence number did not advance for a frame that never went out.
        assert_eq!(conn.seq, 0, "a refused send does not consume a sequence number");
    }

    /// What separates a transport that can carry on from one that cannot. A
    /// frame that went out in part left the peer a prefix it cannot verify and
    /// the chain cannot be rejoined; a write that moved nothing put nothing on
    /// the wire, so a merely slow peer does not cost the transport — that frame
    /// is offered again, and the peer that never takes it runs out the frame's
    /// budget instead.
    #[test]
    fn only_a_frame_that_partly_went_out_finishes_the_transport() {
        for kind in [io::ErrorKind::WouldBlock, io::ErrorKind::TimedOut] {
            let e = io::Error::new(kind, "stalled");
            assert!(write_is_recoverable(&e, 0, false), "{kind:?} with nothing sent is retryable");
            assert!(!write_is_recoverable(&e, 1, false), "{kind:?} mid-frame is not");
            // The plaintext count is not a byte count on TLS: none accepted can
            // still mean a partial record reached the peer, so there is no
            // stall it is safe to retry.
            assert!(!write_is_recoverable(&e, 0, true), "{kind:?} on TLS is never retryable");
        }
        for kind in [io::ErrorKind::BrokenPipe, io::ErrorKind::ConnectionReset] {
            let e = io::Error::new(kind, "gone");
            assert!(!write_is_recoverable(&e, 0, false), "{kind:?} is not a stall");
        }
    }

    /// A signal is not the peer hanging up. Every socket here carries a read
    /// timeout, which stops Linux restarting the read a signal landed in, so
    /// the poll comes back interrupted with the connection perfectly alive —
    /// and the pollers above treat any error as a connection to tear down and
    /// replay.
    #[test]
    fn a_signal_is_nothing_arriving_not_a_lost_connection() {
        for kind in [
            io::ErrorKind::WouldBlock,
            io::ErrorKind::TimedOut,
            io::ErrorKind::Interrupted,
        ] {
            let e = io::Error::new(kind, "nothing yet");
            assert!(read_found_nothing(&e), "{kind:?} means nothing arrived, not a dead socket");
        }
        for kind in [io::ErrorKind::ConnectionReset, io::ErrorKind::BrokenPipe] {
            let e = io::Error::new(kind, "gone");
            assert!(!read_found_nothing(&e), "{kind:?} is the transport ending");
        }
    }

    /// A peer that will take nothing right now is offered the frame again,
    /// rather than the frame being handed back as gone.
    ///
    /// Nothing above this layer sends a frame a second time, so a stall
    /// reported here was a request that simply vanished — and because the
    /// transport was left installed, no reconnect and no replay put it back.
    /// A subscribe or a withdrawal whose result nobody reads had already been
    /// written into this session's own record as sent.
    #[test]
    fn a_peer_that_takes_nothing_now_is_offered_the_frame_again() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = std::net::TcpStream::connect(addr).unwrap();
        let (mut peer, _) = listener.accept().unwrap();
        client.set_nonblocking(true).unwrap();

        // Filled through a second handle on the same socket, so the filling is
        // not itself a part-written frame on the connection under test. The
        // kernel reports a stall only when it can place no byte at all, so
        // there is no room left when this stops.
        let mut filler = client.try_clone().unwrap();
        let block = vec![0u8; 64 * 1024];
        loop {
            match filler.write(&block) {
                Ok(_) => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("the transport would not fill: {e}"),
            }
        }

        // The peer starts reading a moment later, so the frame below is
        // offered into a transport that is still full when it is first tried.
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(250));
            let mut sink = vec![0u8; 64 * 1024];
            while peer.read(&mut sink).is_ok_and(|n| n > 0) {}
        });

        let mut conn = Connection {
            stream: Stream::Raw(client),
            buf: Vec::new(),
            seq: 0,
            sign_key: Vec::new(),
            sign_iv: Vec::new(),
            read_key: Vec::new(),
            read_iv: Vec::new(),
            connected_host: None,
            logged_in_at: None,
            competing: None,
            heartbeat_secs: None,
            market_data_allowance: 40,
            routing: Default::default(),
            write_failed: false,
            read_failed: false,
            unreadable: 0,
        };
        let sent = conn.send_raw(b"8=FIX.4.1\x0135=V\x0110=000\x01");
        assert!(sent.is_ok(), "the frame goes out once the peer reads again: {sent:?}");
        assert!(!conn.write_failed(), "and a peer that took it did not cost the transport");
    }

    /// The bound has to stay under the interval at which the loop probes a
    /// silent peer, or a stalled send holds the thread past the point the
    /// liveness machinery was supposed to act.
    #[test]
    fn the_write_bound_stays_within_the_liveness_cadence() {
        assert!(
            WRITE_TIMEOUT < std::time::Duration::from_secs(
                crate::engine::hot_loop::LIVENESS_TEST_SECS),
            "a write may not outlast the liveness probe interval",
        );
        assert!(WRITE_TIMEOUT > std::time::Duration::ZERO);
    }
}

#[cfg(test)]
mod wedge_tests {
    use super::*;

    /// A compressed frame whose own length does not read is stepped over.
    ///
    /// Waited on, it sat at the head of the buffer and no frame behind it was
    /// ever extracted: every acknowledgement and fill queued up unread while
    /// the socket went on delivering and the connection went on reading as
    /// alive.
    #[test]
    fn a_frame_whose_length_does_not_read_does_not_wedge_the_rest() {
        let (mut conn, _peer) = Connection::for_test();
        // A compressed header whose tag 9 is not a number, then a good frame
        // behind it.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"8=FIXCOMP9=notanumber35=A");
        let good = b"8=FIX.4.29=535=A";
        bytes.extend_from_slice(good);
        conn.inject_buf(&bytes);

        let held = conn.buffered();
        let _ = conn.extract_frames();
        assert!(
            conn.buffered() < held,
            "the unreadable frame is stepped over rather than waited on forever",
        );
        // Again, to show it does not stall one call later.
        let after = conn.buffered();
        let _ = conn.extract_frames();
        assert!(after == 0 || conn.buffered() <= after);
    }

    /// A frame that is merely still arriving is waited for, not discarded.
    #[test]
    fn a_frame_still_arriving_is_waited_for() {
        let (mut conn, _peer) = Connection::for_test();
        conn.inject_buf(b"8=FIXCOMP9=50035=A");
        assert!(conn.extract_frames().is_empty(), "nothing complete yet");
        assert!(conn.buffered() > 0, "and the partial frame is kept");
    }

    /// After a compressed frame whose length does not read, the next intact
    /// frame is extracted whatever kind it is.
    ///
    /// Resynchronising to a compressed or FIX header alone searched straight
    /// past a binary quote frame that had arrived whole in the same slice, and
    /// dropped it with the corrupt bytes.
    #[test]
    fn an_unreadable_compressed_frame_resynchronises_onto_any_header() {
        let mut conn = Connection::for_test().0;
        let body = b"35=P\x01data";
        let mut good = format!("8=O\x019={}\x01", body.len()).into_bytes();
        good.extend_from_slice(body);

        let mut buf = b"8=FIXCOMP9=not-a-number\x01".to_vec();
        buf.extend_from_slice(&good);
        conn.inject_buf(&buf);

        let frames = conn.extract_frames();
        assert!(
            frames.iter().any(|f| matches!(f, Frame::Binary(_))),
            "the intact binary frame behind the corrupt one survives: {frames:?}",
        );
    }

    /// A binary or control header whose length does not read is stepped past,
    /// not waited on.
    ///
    /// All three of these headers are resynchronisation targets, so four such
    /// bytes inside a payload leave one at the front of the buffer with
    /// something other than a length behind it. Waited on, it stays there and
    /// every acknowledgement and fill behind it goes unextracted, on a
    /// connection that reads as alive because bytes keep arriving.
    #[test]
    fn a_binary_header_with_an_unreadable_length_does_not_hold_the_stream() {
        for header in [&b"8=O\x01"[..], &b"8=1\x01"[..], &b"8=X\x01"[..]] {
            let mut conn = Connection::for_test().0;
            let body = b"35=P\x01data";
            let mut good = format!("8=O\x019={}\x01", body.len()).into_bytes();
            good.extend_from_slice(body);

            let mut buf = header.to_vec();
            buf.extend_from_slice(b"9=not-a-number\x01");
            buf.extend_from_slice(&good);
            conn.inject_buf(&buf);

            let frames = conn.extract_frames();
            assert!(
                frames.iter().any(|f| matches!(f, Frame::Binary(_))),
                "{:?}: the intact frame behind the unreadable header survives, got {frames:?}",
                std::str::from_utf8(header),
            );
        }
    }

    /// A stated length no frame can have is not a length to add.
    ///
    /// Added unchecked it aborted where overflow is checked and framed from a
    /// wrapped offset where it is not, on a number the peer chooses.
    #[test]
    fn a_stated_length_that_cannot_fit_is_not_a_frame() {
        let huge = format!("8=O\x019={}\x01body", usize::MAX);
        assert_eq!(binary_msg_length(huge.as_bytes()), None);
        let huge_fix = format!("8=FIX.4.1\x019={}\x01body", usize::MAX);
        assert_eq!(fix_msg_length(huge_fix.as_bytes()), None);
    }

    /// A peer sending nothing this client can read still keeps bytes
    /// arriving, and bytes arriving is what the liveness deadlines measure —
    /// so without a bound of its own it held the connection open for as long
    /// as it sent. Once more has arrived unreadable than one frame can be,
    /// the transport is given up the way one is for a frame that failed to
    /// verify, and the reconnect path takes it over.
    #[test]
    fn traffic_that_cannot_be_read_does_not_hold_the_connection_open_for_ever() {
        let (mut conn, _peer) = Connection::for_test();
        assert!(!conn.read_failed(), "nothing has failed yet");

        let chunk = vec![0xABu8; 8 * 1024 * 1024];
        let mut fed = 0usize;
        while fed <= MAX_BUFFERED && !conn.read_failed() {
            conn.inject_buf(&chunk);
            assert!(conn.extract_frames().is_empty(), "garbage frames nothing");
            fed += chunk.len();
        }
        assert!(
            conn.read_failed(),
            "the transport is given up once what it cannot read passes what one frame can be",
        );

        // And a stream that does deliver frames is not measured against the
        // bound at all: what could not be read before the frame is let go
        // with it.
        let (mut conn, _peer) = Connection::for_test();
        conn.inject_buf(&vec![0xABu8; 4096]);
        let msg = crate::protocol::fix::fix_build(&[(35, "0")], 1);
        conn.inject_buf(&msg);
        let frames = conn.extract_frames();
        assert_eq!(frames.len(), 1, "the frame behind the garbage survives it");
        conn.inject_buf(&vec![0xABu8; 4096]);
        conn.inject_buf(&msg);
        assert_eq!(conn.extract_frames().len(), 1);
        assert!(!conn.read_failed(), "a delivering stream is not finished");
    }

    /// Tag 9 is where the header puts it, and the tail of `49=` is not it.
    ///
    /// Searched for, the first `9=` in the buffer matched inside another tag
    /// and a frame with no body length at all was framed from whatever
    /// followed — reaching over the frames queued behind it, so an
    /// acknowledgement and a fill were drained as part of it and never seen.
    #[test]
    fn a_length_read_out_of_another_tag_is_not_a_length() {
        assert_eq!(fix_msg_length(b"8=FIX.4.1\x0149=5\x0135=0\x0110=000\x01"), None);
    }

    /// What frames a header and what says a header cannot be framed have to
    /// agree.
    ///
    /// A length that reads as a number and then makes no total is refused by
    /// the one and called readable by the other, so the extractor would not
    /// frame it and nothing resynchronised past it: the header stayed at the
    /// front of the buffer and every frame behind it queued unread, on a
    /// connection that went on reading as alive.
    #[test]
    fn a_length_that_can_make_no_total_reads_as_unreadable() {
        let over = format!("8=FIX.4.1\x019={}\x0135=0\x01", usize::MAX);
        assert_eq!(fix_msg_length(over.as_bytes()), None, "no total fits");
        assert!(
            tag9_is_unreadable(over.as_bytes(), fix_msg_length),
            "waiting on a header no byte can complete holds the buffer for ever",
        );
    }

    /// A read that ends inside the next header does not take it away.
    ///
    /// Stepping past a compressed frame whose length does not read goes to the
    /// next header, and where none is here yet the rest was dropped as well.
    /// A read ends where it ends, so the tail is as likely to be the front of
    /// a header as anything: dropped with the rest, the frame it began could
    /// never be recognised once the remainder arrived, and it was lost whole.
    #[test]
    fn a_header_arriving_in_two_reads_survives_the_step_past_a_bad_frame() {
        let (mut conn, _peer) = Connection::for_test();
        // A compressed header stating a length that does not read, and behind
        // it the first bytes of a real one.
        conn.inject_buf(b"8=FIXCOMP\x019=00X\x01padpadpad8=FIX");
        let _ = conn.extract_frames();

        conn.inject_buf(b".4.1\x019=0005\x0135=0\x0110=000\x01");
        let frames = conn.extract_frames();
        assert!(
            frames.iter().any(|f| matches!(f, Frame::Fix(_))),
            "the header that arrived across two reads was dropped with the \
             frame in front of it: {frames:?}",
        );
    }

    /// A peer that never completes a message is given up on at the bound.
    ///
    /// The establishment loops assemble a message by hand rather than framing
    /// one, and they run before the peer has proved anything. A deadline does
    /// not answer a sender writing as fast as the socket takes it: the bytes
    /// arrive at the speed of the link and the memory is gone long before the
    /// clock is. Held to the same bound the framed reads keep, what is
    /// arriving past it is refused rather than stored.
    #[test]
    fn a_read_that_would_pass_the_bound_is_refused_rather_than_held() {
        // Room for exactly one more byte, and that byte is taken.
        let mut buf = vec![0u8; MAX_BUFFERED - 1];
        hold_what_was_read(&mut buf, &[1u8]).expect("what fits is held");
        assert_eq!(buf.len(), MAX_BUFFERED);

        // The next one does not fit, and is not held.
        let err = hold_what_was_read(&mut buf, &[1u8])
            .expect_err("a peer past the bound is given up on");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{err}");
        assert_eq!(buf.len(), MAX_BUFFERED, "and nothing of it was kept");
    }
}
