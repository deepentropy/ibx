//! Non-blocking connection wrapping a TLS or raw TCP stream with read/write buffers.
//!
//! Maintains per-connection state: buffer, seq counter,
//! HMAC sign/read IVs (chained per message).

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::TcpStream;

use native_tls::TlsStream;

use super::fix::{self, SOH};
use super::fixcomp;

/// Recv buffer size.
const RECV_BUF_SIZE: usize = 32768;

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
    /// it in the same recv slice (ibx#185).
    Control(Vec<u8>),
}

/// The byte stream under a [`Connection`]. A connection holds one through
/// [`Stream`], a closed enum: the send and receive path is a static match,
/// with no boxing and no virtual call.
pub trait Transport: Read + Write {
    /// Close both directions. Later reads see the end of the stream and
    /// later writes fail. Errors are ignored: it may be closed already.
    fn shutdown(&mut self);
    /// Writes that return at once with what the stream takes now (`true`),
    /// or the default mode (`false`).
    fn set_nonblocking(&mut self, on: bool) -> io::Result<()>;
}

impl Transport for TcpStream {
    fn shutdown(&mut self) {
        let _ = TcpStream::shutdown(self, std::net::Shutdown::Both);
    }

    fn set_nonblocking(&mut self, on: bool) -> io::Result<()> {
        TcpStream::set_nonblocking(self, on)
    }
}

impl Transport for TlsStream<TcpStream> {
    fn shutdown(&mut self) {
        let _ = self.get_ref().shutdown(std::net::Shutdown::Both);
    }

    fn set_nonblocking(&mut self, on: bool) -> io::Result<()> {
        self.get_ref().set_nonblocking(on)
    }
}

/// The transports a connection runs on: TLS, raw TCP, and for the tests an
/// in-memory pipe ([`MemTransport`], `test-support` feature only, so a
/// release build has the two socket arms alone).
enum Stream {
    Tls(TlsStream<TcpStream>),
    Raw(TcpStream),
    #[cfg(any(test, feature = "test-support"))]
    Mem(MemTransport),
}

impl Read for Stream {
    #[inline]
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Tls(s) => s.read(buf),
            Self::Raw(s) => s.read(buf),
            #[cfg(any(test, feature = "test-support"))]
            Self::Mem(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    #[inline]
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Tls(s) => s.write(buf),
            Self::Raw(s) => s.write(buf),
            #[cfg(any(test, feature = "test-support"))]
            Self::Mem(s) => s.write(buf),
        }
    }

    #[inline]
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Tls(s) => s.flush(),
            Self::Raw(s) => s.flush(),
            #[cfg(any(test, feature = "test-support"))]
            Self::Mem(s) => s.flush(),
        }
    }
}

impl Transport for Stream {
    fn shutdown(&mut self) {
        match self {
            Self::Tls(s) => Transport::shutdown(s),
            Self::Raw(s) => Transport::shutdown(s),
            #[cfg(any(test, feature = "test-support"))]
            Self::Mem(s) => Transport::shutdown(s),
        }
    }

    fn set_nonblocking(&mut self, on: bool) -> io::Result<()> {
        match self {
            Self::Tls(s) => Transport::set_nonblocking(s, on),
            Self::Raw(s) => Transport::set_nonblocking(s, on),
            #[cfg(any(test, feature = "test-support"))]
            Self::Mem(s) => Transport::set_nonblocking(s, on),
        }
    }
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
    /// Frames accepted for sending but not yet written, oldest first
    /// (queued writes only). `out_pos` bytes of the first are written.
    out: VecDeque<Vec<u8>>,
    out_pos: usize,
    /// When set, a send never blocks the caller: what the socket does not
    /// take at once waits in `out` and goes out with `flush_queued`.
    queued_writes: bool,
    /// The first write error. The connection is unusable from then on: the
    /// owner drops it and reconnects; no frame is written again.
    write_error: Option<(io::ErrorKind, String)>,
}

impl Connection {
    /// Create a new connection from an already-established TLS stream.
    ///
    /// Uses a blocking socket with a 1ms read timeout — mirrors `new_raw`.
    /// Non-blocking writes can return `WouldBlock` after a partial send,
    /// which poisons the seq/sign_iv chain that already advanced for the
    /// not-yet-on-the-wire message. Blocking writes either commit fully
    /// or surface a hard error, which the hot-loop reconnect path handles.
    pub fn new(stream: TlsStream<TcpStream>) -> io::Result<Self> {
        stream.get_ref().set_read_timeout(Some(std::time::Duration::from_millis(1)))?;
        Ok(Self::on(Stream::Tls(stream)))
    }

    fn on(stream: Stream) -> Self {
        Self {
            stream,
            buf: Vec::with_capacity(RECV_BUF_SIZE),
            seq: 0,
            sign_key: Vec::new(),
            sign_iv: Vec::new(),
            read_key: Vec::new(),
            read_iv: Vec::new(),
            out: VecDeque::new(),
            out_pos: 0,
            queued_writes: false,
            write_error: None,
        }
    }

    /// Create a new connection from a raw TCP stream (for farm connections).
    /// Sets the stream to non-blocking mode and enables TCP_NODELAY.
    pub fn new_raw(stream: TcpStream) -> io::Result<Self> {
        stream.set_nodelay(true)?;
        // Use blocking socket with 1ms read timeout instead of non-blocking.
        // Non-blocking write_all can silently fail (WouldBlock), causing HMAC-signed
        // messages to never reach the farm — the sign_iv still advances, permanently
        // breaking the signing chain.
        stream.set_read_timeout(Some(std::time::Duration::from_millis(1)))?;
        Ok(Self::on(Stream::Raw(stream)))
    }

    /// A connection on one end of an in-memory pipe (tests): the same
    /// framing, compression, signing and queued-write path as a socket.
    /// A read waits 1 ms for data, as the sockets do.
    #[cfg(any(test, feature = "test-support"))]
    pub fn new_mem(stream: MemTransport) -> Self {
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(1)));
        Self::on(Stream::Mem(stream))
    }

    /// The read wait of an in-memory connection (tests); zero makes a read
    /// return at once. No effect on a socket.
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_mem_read_timeout(&self, timeout: std::time::Duration) {
        if let Stream::Mem(s) = &self.stream {
            let _ = s.set_read_timeout(Some(timeout));
        }
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

    /// Pre-load data into the read buffer (e.g. init burst bytes read before Connection was created).
    pub fn seed_buffer(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// Whether the internal buffer contains unprocessed data.
    pub fn has_buffered_data(&self) -> bool {
        !self.buf.is_empty()
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
                self.buf.extend_from_slice(&tmp[..n]);
                Ok(n)
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock
                || e.kind() == io::ErrorKind::TimedOut => Ok(0),
            Err(e) => Err(e),
        }
    }

    /// Extract all complete frames from the internal buffer.
    /// Handles compressed, standard, and binary protocols.
    pub fn extract_frames(&mut self) -> Vec<Frame> {
        let mut frames = Vec::new();
        loop {
            if self.buf.is_empty() {
                break;
            }
            // Compressed protocol
            if self.buf.starts_with(b"8=FIXCOMP\x01") {
                match fixcomp::fixcomp_length(&self.buf) {
                    Some(total) if self.buf.len() >= total => {
                        let msg: Vec<u8> = self.buf.drain(..total).collect();
                        frames.push(Frame::FixComp(msg));
                        continue;
                    }
                    _ => break, // incomplete
                }
            }

            // Find earliest message start among all recognized headers.
            // 8=1 (token-auth state) and 8=X (encrypted control) share the
            // length-prefixed, trailer-free framing of 8=O. Recognizing them
            // here keeps them out of the buf.clear() arm below, which would
            // otherwise wipe any FIXCOMP frame queued behind them in the same
            // recv slice (ibx#185).
            // A compressed frame is recognized above only at offset 0. Search
            // for it here too: with a stray byte in front ("\x01" seen live),
            // the "8=FIX." search does not match "8=FIXCOMP", the buffer was
            // cleared, and the lost signed frame put the read IV chain out of
            // step, garbling every later frame on the connection.
            let fix_pos = find_subsequence(&self.buf, b"8=FIX.");
            let fixcomp_pos = find_subsequence(&self.buf, b"8=FIXCOMP\x01");
            let o_pos = find_subsequence(&self.buf, b"8=O\x01");
            let one_pos = find_subsequence(&self.buf, b"8=1\x01");
            let x_pos = find_subsequence(&self.buf, b"8=X\x01");

            let earliest = [fix_pos, fixcomp_pos, o_pos, one_pos, x_pos]
                .into_iter()
                .flatten()
                .min();
            let earliest = match earliest {
                Some(e) => e,
                None => {
                    // ibx#183 follow-up: dump the FULL payload (hex + ascii) of
                    // anything we're about to discard. We need the whole frame
                    // for upstream analysis (ib-agent#152 sister fixture), not
                    // just a 64-byte prefix.
                    let full_hex: String = self.buf
                        .iter()
                        .map(|b| format!("{:02x}", b))
                        .collect();
                    let head_n = self.buf.len().min(64);
                    let head_ascii: String = self.buf[..head_n]
                        .iter()
                        .map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '.' })
                        .collect();
                    log::warn!(
                        "extract_frames: dropping {}B (no header). first {}B ascii={:?} full_hex={}",
                        self.buf.len(), head_n, head_ascii, full_hex,
                    );
                    self.buf.clear();
                    break;
                }
            };

            // Skip garbage before earliest message
            if earliest > 0 {
                self.buf.drain(..earliest);
                continue;
            }

            // 8=O binary protocol: length-delimited via tag 9
            if self.buf.starts_with(b"8=O\x01") {
                if let Some(total) = binary_msg_length(&self.buf) {
                    if self.buf.len() >= total {
                        let msg: Vec<u8> = self.buf.drain(..total).collect();
                        frames.push(Frame::Binary(msg));
                        continue;
                    }
                }
                break; // incomplete
            }

            // 8=1 / 8=X control protocol: same length-delimited framing as 8=O
            // (body length in tag 9, no checksum trailer). Extracted as Control
            // frames and ignored downstream (ibx#185).
            if self.buf.starts_with(b"8=1\x01") || self.buf.starts_with(b"8=X\x01") {
                if let Some(total) = binary_msg_length(&self.buf) {
                    if self.buf.len() >= total {
                        let msg: Vec<u8> = self.buf.drain(..total).collect();
                        frames.push(Frame::Control(msg));
                        continue;
                    }
                }
                break; // incomplete
            }

            // FIX.4.1: length-delimited via tag 9, +7 for checksum "10=XXX\x01"
            if self.buf.starts_with(b"8=FIX.") {
                if let Some(total) = fix_msg_length(&self.buf) {
                    if self.buf.len() >= total {
                        let msg: Vec<u8> = self.buf.drain(..total).collect();
                        frames.push(Frame::Fix(msg));
                        continue;
                    }
                }
                break; // incomplete
            }

            // Unknown prefix — skip one byte and retry
            self.buf.drain(..1);
        }
        frames
    }

    /// Unsign a received frame using the read IV.
    /// Returns the undistorted message bytes and whether the signature was valid.
    ///
    /// As in the reference (ibx#275): a frame without the signature trailer
    /// is unsigned and accepted as it is; the read IV advances only after a
    /// signature match. On a mismatch the caller must drop the connection
    /// and reconnect; the frame must not be used.
    pub fn unsign(&mut self, msg: &[u8]) -> (Vec<u8>, bool) {
        if self.read_key.is_empty() {
            return (msg.to_vec(), true); // no signing configured
        }
        if !fix::is_signed(msg) {
            return (msg.to_vec(), true);
        }
        let (undistorted, new_iv, valid) = fix::fix_unsign(msg, &self.read_key, &self.read_iv);
        if valid {
            self.read_iv = new_iv;
        }
        (undistorted, valid)
    }

    /// Close the socket in both directions, for a connection that must not
    /// be read any more (signature mismatch, ibx#275). Errors are ignored:
    /// the socket may be closed already.
    pub fn shutdown(&mut self) {
        self.stream.shutdown();
    }

    /// Writes of this connection stop blocking the caller (ibx#254): each
    /// connection keeps its own output, so a peer that stops reading
    /// stalls only its own link, as in the reference. There is no write
    /// timeout, as in the reference: the output waits until the peer reads
    /// or the system fails the connection.
    pub fn set_queued_writes(&mut self, on: bool) {
        self.queued_writes = on;
    }

    /// Whether accepted frames are still waiting to be written.
    #[inline]
    pub fn has_queued_output(&self) -> bool {
        !self.out.is_empty()
    }

    /// The write error that made this connection unusable, if any.
    #[inline]
    pub fn write_error(&self) -> Option<&str> {
        self.write_error.as_ref().map(|(_, text)| text.as_str())
    }

    fn failed(&self) -> io::Error {
        let (kind, text) = self.write_error.as_ref().expect("write error recorded");
        io::Error::new(*kind, text.clone())
    }

    fn record_write_error(&mut self, e: io::Error) -> io::Error {
        if self.write_error.is_none() {
            self.write_error = Some((e.kind(), e.to_string()));
        }
        e
    }

    /// Hand one complete frame to the socket. Blocking mode: written at
    /// once. Queued mode: written as far as the socket takes it without
    /// waiting, the rest kept in order behind the frames already waiting.
    /// An error marks the connection failed; the frame is never retried.
    fn write_frame(&mut self, frame: Vec<u8>) -> io::Result<()> {
        if self.write_error.is_some() {
            return Err(self.failed());
        }
        if !self.queued_writes {
            return self.stream.write_all(&frame).map_err(|e| self.record_write_error(e));
        }
        self.out.push_back(frame);
        if self.out.len() == 1 {
            self.flush_queued()
        } else {
            Ok(())
        }
    }

    /// Write what the socket takes now of the waiting frames, in order,
    /// without blocking. An error marks the connection failed.
    pub fn flush_queued(&mut self) -> io::Result<()> {
        if self.write_error.is_some() {
            return Err(self.failed());
        }
        if self.out.is_empty() {
            return Ok(());
        }
        if let Err(e) = self.stream.set_nonblocking(true) {
            return Err(self.record_write_error(e));
        }
        let result = loop {
            let Some(front) = self.out.front() else { break Ok(()) };
            // A partial TLS record is completed by calling again with the
            // same bytes, which this does.
            match self.stream.write(&front[self.out_pos..]) {
                Ok(0) => break Err(io::Error::new(io::ErrorKind::WriteZero, "socket accepted no bytes")),
                Ok(n) => {
                    self.out_pos += n;
                    if self.out_pos >= front.len() {
                        self.out.pop_front();
                        self.out_pos = 0;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break Ok(()),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => break Err(e),
            }
        };
        let restored = self.stream.set_nonblocking(false);
        match result.and(restored) {
            Ok(()) => Ok(()),
            Err(e) => Err(self.record_write_error(e)),
        }
    }

    /// Build a FIX message, sign it, and send it. Increments seq and chains sign IV.
    ///
    /// State (seq, sign_iv) is committed once the frame is accepted. A write
    /// error makes the connection unusable: it is dropped and reconnected,
    /// the frame is never retried, as in the reference (ibx#254).
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
        self.write_frame(to_send)?;
        self.seq = next_seq;
        if let Some(iv) = next_iv {
            self.sign_iv = iv;
        }
        Ok(())
    }

    /// Build a FIX message outside the sequence count, sign it, and send it.
    /// The sequence counter does not move.
    pub fn send_fix_unsequenced(&mut self, fields: &[(u32, &str)]) -> io::Result<()> {
        let msg = fix::fix_build(fields, 0);
        if log::log_enabled!(log::Level::Trace) {
            log::trace!("WIRE> seq=0 {}", fix::fmt_pipe(&msg));
        }
        let (to_send, next_iv) = if self.sign_key.is_empty() {
            (msg, None)
        } else {
            let (signed, iv) = fix::fix_sign(&msg, &self.sign_key, &self.sign_iv);
            (signed, Some(iv))
        };
        self.write_frame(to_send)?;
        if let Some(iv) = next_iv {
            self.sign_iv = iv;
        }
        Ok(())
    }

    /// Build a message, compress, sign, and send. For farm subscribe/data messages.
    /// Uses seq=0 (separate seq space from heartbeats).
    ///
    /// State (sign_iv) is committed once the frame is accepted.
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
        self.write_frame(to_send)?;
        if let Some(iv) = next_iv {
            self.sign_iv = iv;
        }
        Ok(())
    }

    /// Send raw bytes (pre-built message).
    pub fn send_raw(&mut self, data: &[u8]) -> io::Result<()> {
        self.write_frame(data.to_vec())
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
fn binary_msg_length(data: &[u8]) -> Option<usize> {
    // 4-byte tag-8 header ("8=O\x01" / "8=1\x01" / "8=X\x01"), then find 9=
    let after_8 = 4; // "8=O\x01"
    let tag9_pos = find_subsequence(&data[after_8..], b"9=").map(|p| after_8 + p)?;
    let soh_pos = data[tag9_pos..].iter().position(|&b| b == SOH).map(|p| tag9_pos + p)?;
    let body_len: usize = std::str::from_utf8(&data[tag9_pos + 2..soh_pos])
        .ok()?
        .parse()
        .ok()?;
    // A length past the address space is no length: the frame never
    // completes, as one whose length is not a number (ibx#488: the sum
    // overflowed).
    (soh_pos + 1).checked_add(body_len)
}

/// Compute total length of a `8=FIX.4.1\x01 9=<body_len>\x01 ...` message.
/// Includes the 7-byte checksum trailer `10=XXX\x01`.
fn fix_msg_length(data: &[u8]) -> Option<usize> {
    let tag9_pos = find_subsequence(data, b"9=").filter(|&p| p < 20)?;
    let soh_pos = data[tag9_pos..].iter().position(|&b| b == SOH).map(|p| tag9_pos + p)?;
    let body_len: usize = std::str::from_utf8(&data[tag9_pos + 2..soh_pos])
        .ok()?
        .parse()
        .ok()?;
    // header up to and including SOH after tag 9, + body + "10=XXX\x01" (7 bytes)
    (soh_pos + 1 + 7).checked_add(body_len)
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
}

#[cfg(any(test, feature = "test-support"))]
pub use mem::{mem_pair, MemTransport};

/// An in-memory byte pipe standing in for a socket in the tests, so the
/// engine runs against a scripted peer with no network. Built only for the
/// tests (`test-support` feature).
#[cfg(any(test, feature = "test-support"))]
mod mem {
    use std::collections::VecDeque;
    use std::io::{self, Read, Write};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::{Duration, Instant};

    /// One direction of a pipe.
    #[derive(Default)]
    struct Pipe {
        data: VecDeque<u8>,
        /// The writing end closed: the reader gets the end of the stream
        /// once the data is read.
        write_closed: bool,
        /// The reading end closed: a write fails, as on a reset socket.
        read_closed: bool,
        /// Most bytes waiting at once; None for no limit.
        capacity: Option<usize>,
    }

    #[derive(Default)]
    struct Shared {
        pipe: Mutex<Pipe>,
        changed: Condvar,
    }

    /// No read timeout: a read waits until data or the end of the stream.
    const WAIT_FOREVER: u64 = u64::MAX;

    /// One end of an in-memory pipe ([`mem_pair`]). It reads and writes as a
    /// TCP stream does: a read waits for data up to the read timeout, then
    /// fails with `WouldBlock`; a closed peer gives the end of the stream to
    /// a read and `BrokenPipe` to a write. Dropping an end closes it.
    pub struct MemTransport {
        rx: Arc<Shared>,
        tx: Arc<Shared>,
        /// Read timeout in nanoseconds; 0 for a read that returns at once,
        /// [`WAIT_FOREVER`] for none.
        read_timeout: AtomicU64,
        /// Reads and writes return at once (`WouldBlock`) when they cannot
        /// proceed.
        nonblocking: bool,
    }

    /// Two connected ends: what one writes, the other reads.
    pub fn mem_pair() -> (MemTransport, MemTransport) {
        let a = Arc::new(Shared::default());
        let b = Arc::new(Shared::default());
        let end = |rx: &Arc<Shared>, tx: &Arc<Shared>| MemTransport {
            rx: rx.clone(),
            tx: tx.clone(),
            read_timeout: AtomicU64::new(WAIT_FOREVER),
            nonblocking: false,
        };
        (end(&a, &b), end(&b, &a))
    }

    impl MemTransport {
        /// As `TcpStream::set_read_timeout`; `Some(Duration::ZERO)` makes a
        /// read return at once.
        pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
            let nanos = timeout.map_or(WAIT_FOREVER, |d| d.as_nanos().min(WAIT_FOREVER as u128 - 1) as u64);
            self.read_timeout.store(nanos, Ordering::Relaxed);
            Ok(())
        }

        /// As `TcpStream::set_nonblocking`: a read or write that cannot
        /// proceed at once fails with `WouldBlock`.
        pub fn set_nonblocking(&mut self, on: bool) -> io::Result<()> {
            self.nonblocking = on;
            Ok(())
        }

        /// Most bytes this end's output holds before a write waits (or, in
        /// non-blocking mode, takes only what fits): a peer that does not
        /// read fills it, as a socket buffer.
        pub fn set_write_capacity(&self, capacity: Option<usize>) {
            self.tx.pipe.lock().unwrap().capacity = capacity;
            self.tx.changed.notify_all();
        }

        /// Bytes written by this end that the peer has not read yet.
        pub fn unread_output(&self) -> usize {
            self.tx.pipe.lock().unwrap().data.len()
        }

        fn close(&self) {
            self.tx.pipe.lock().unwrap().write_closed = true;
            self.tx.changed.notify_all();
            self.rx.pipe.lock().unwrap().read_closed = true;
            self.rx.changed.notify_all();
        }
    }

    impl Drop for MemTransport {
        fn drop(&mut self) {
            self.close();
        }
    }

    impl Read for MemTransport {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if buf.is_empty() {
                return Ok(0);
            }
            let timeout = if self.nonblocking { 0 } else { self.read_timeout.load(Ordering::Relaxed) };
            let deadline = (timeout != WAIT_FOREVER).then(|| Instant::now() + Duration::from_nanos(timeout));
            let mut pipe = self.rx.pipe.lock().unwrap();
            loop {
                if pipe.read_closed {
                    return Ok(0);
                }
                if !pipe.data.is_empty() {
                    let n = buf.len().min(pipe.data.len());
                    for (dst, src) in buf.iter_mut().zip(pipe.data.drain(..n)) {
                        *dst = src;
                    }
                    self.rx.changed.notify_all();
                    return Ok(n);
                }
                if pipe.write_closed {
                    return Ok(0);
                }
                match deadline {
                    None => pipe = self.rx.changed.wait(pipe).unwrap(),
                    Some(at) => {
                        let now = Instant::now();
                        if now >= at {
                            return Err(io::Error::new(io::ErrorKind::WouldBlock, "no data"));
                        }
                        pipe = self.rx.changed.wait_timeout(pipe, at - now).unwrap().0;
                    }
                }
            }
        }
    }

    impl Write for MemTransport {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if buf.is_empty() {
                return Ok(0);
            }
            let mut pipe = self.tx.pipe.lock().unwrap();
            loop {
                if pipe.write_closed || pipe.read_closed {
                    return Err(io::Error::new(io::ErrorKind::BrokenPipe, "pipe closed"));
                }
                let room = pipe.capacity.map_or(buf.len(), |c| c.saturating_sub(pipe.data.len()));
                if room > 0 {
                    let n = room.min(buf.len());
                    pipe.data.extend(&buf[..n]);
                    self.tx.changed.notify_all();
                    return Ok(n);
                }
                if self.nonblocking {
                    return Err(io::Error::new(io::ErrorKind::WouldBlock, "pipe full"));
                }
                pipe = self.tx.changed.wait(pipe).unwrap();
            }
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl super::Transport for MemTransport {
        fn shutdown(&mut self) {
            self.close();
        }

        fn set_nonblocking(&mut self, on: bool) -> io::Result<()> {
            MemTransport::set_nonblocking(self, on)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::fix::fix_build;
    use crate::protocol::fixcomp::fixcomp_build;

    /// Helper: create a Connection-like buffer and test frame extraction.
    /// We can't easily create a TlsStream in tests, so we test the framing
    /// functions directly.

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

    #[test]
    fn binary_msg_length_basic() {
        // Build a minimal 8=O message
        let body = b"35=P\x01data";
        let msg = format!("8=O\x019={}\x01", body.len());
        let mut full = msg.into_bytes();
        full.extend_from_slice(body);
        assert_eq!(binary_msg_length(&full), Some(full.len()));
    }

    #[test]
    fn binary_msg_length_incomplete() {
        // binary_msg_length returns the expected total, caller checks buf.len() >= total
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

    /// A connection on an in-memory pipe with `buf` already received.
    fn test_connection_with_buf(buf: Vec<u8>) -> Connection {
        let (end, _peer) = mem_pair();
        let mut conn = Connection::new_mem(end);
        conn.seed_buffer(&buf);
        conn
    }

    /// A connection and the peer end of its pipe.
    fn loopback() -> (Connection, MemTransport) {
        let (end, peer) = mem_pair();
        (Connection::new_mem(end), peer)
    }

    fn read_available(server: &mut MemTransport, want: usize) -> Vec<u8> {
        server.set_read_timeout(Some(std::time::Duration::from_millis(200))).unwrap();
        let mut out = Vec::new();
        let mut buf = vec![0u8; 1 << 16];
        while out.len() < want {
            match server.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(_) => break,
            }
        }
        out
    }

    /// The in-memory transport under a connection made by `loopback`.
    fn conn_output(conn: &mut Connection) -> &mut MemTransport {
        match &mut conn.stream {
            Stream::Mem(t) => t,
            _ => unreachable!("an in-memory connection"),
        }
    }

    // ibx#254: with a peer that does not read, sends return at once and the
    // output waits on the connection, in order; it goes out when the peer
    // reads again. No timeout ends the wait.
    #[test]
    fn queued_writes_never_block_on_a_peer_that_does_not_read() {
        let (mut conn, mut server) = loopback();
        // The connection's output holds 256 KB, as a socket buffer.
        conn_output(&mut conn).set_write_capacity(Some(256 * 1024));
        conn.set_queued_writes(true);
        let frame = vec![b'x'; 64 * 1024];
        let mut sent = 0usize;
        let started = std::time::Instant::now();
        while !conn.has_queued_output() {
            conn.send_raw(&frame).unwrap();
            sent += 1;
            assert!(sent < 10_000, "the socket buffers never filled");
        }
        // More frames while the peer is stalled: accepted, not written.
        for i in 0..20u8 {
            conn.send_raw(&[b'#', i]).unwrap();
        }
        assert!(started.elapsed() < std::time::Duration::from_secs(10), "sends did not block");
        assert!(conn.write_error().is_none());

        let total = sent * frame.len() + 20 * 2;
        let mut got = Vec::new();
        while got.len() < total {
            got.extend(read_available(&mut server, total - got.len()));
            conn.flush_queued().unwrap();
        }
        assert!(!conn.has_queued_output());
        assert_eq!(got.len(), total);
        assert!(got[..sent * frame.len()].iter().all(|&b| b == b'x'));
        let tail: Vec<u8> = (0..20u8).flat_map(|i| [b'#', i]).collect();
        assert_eq!(&got[sent * frame.len()..], &tail[..], "frames in the order they were sent");
    }

    // ibx#254: a write error makes the connection unusable; the frame is
    // not sent again and the sequence does not move.
    #[test]
    fn a_write_error_fails_the_connection_without_retry() {
        let (mut conn, _server) = loopback();
        conn.set_queued_writes(true);
        conn.send_fix(&[(35, "0")]).unwrap();
        assert_eq!(conn.seq, 1);
        conn.shutdown();
        assert!(conn.send_fix(&[(35, "0")]).is_err());
        assert!(conn.write_error().is_some());
        assert_eq!(conn.seq, 1, "a frame that failed takes no sequence number");
        assert!(conn.send_raw(b"later").is_err(), "nothing is written after a failure");
        assert!(conn.flush_queued().is_err());
    }

    // Before the engine takes a connection over, a send is written at once.
    #[test]
    fn blocking_writes_by_default() {
        let (mut conn, mut server) = loopback();
        conn.send_raw(b"hello").unwrap();
        assert!(!conn.has_queued_output());
        assert_eq!(read_available(&mut server, 5), b"hello");
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
            other => panic!("expected Frame::FixComp, got {:?}", other),
        }
    }

    // A stray byte in front of a compressed frame used to clear the whole
    // buffer, losing the frame (seen live as "dropping 391B (no header)",
    // first byte 0x01 then "8=FIXCOMP").
    #[test]
    fn frame_extraction_stray_byte_before_fixcomp() {
        let inner = fix_build(&[(35, "Q")], 1);
        let comp = fixcomp_build(&inner);
        let mut buf = vec![0x01];
        buf.extend_from_slice(&comp);
        let mut conn = test_connection_with_buf(buf);
        let frames = conn.extract_frames();
        assert_eq!(frames.len(), 1);
        match &frames[0] {
            Frame::FixComp(data) => assert_eq!(data, &comp),
            other => panic!("expected Frame::FixComp, got {:?}", other),
        }
    }

    #[test]
    fn frame_extraction_fixcomp_behind_garbage_keeps_both_frames() {
        let first = fixcomp_build(&fix_build(&[(35, "Q")], 1));
        let second = fixcomp_build(&fix_build(&[(35, "P")], 2));
        let mut buf = vec![0xDE, 0xAD];
        buf.extend_from_slice(&first);
        buf.extend_from_slice(&second);
        let mut conn = test_connection_with_buf(buf);
        let frames = conn.extract_frames();
        assert_eq!(frames.len(), 2);
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
            other => panic!("expected Frame::Fix, got {:?}", other),
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
            other => panic!("expected Frame::Fix for msg1, got {:?}", other),
        }
        match &frames[1] {
            Frame::Fix(data) => assert_eq!(data, &msg2),
            other => panic!("expected Frame::Fix for msg2, got {:?}", other),
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
            other => panic!("expected Frame::Binary, got {:?}", other),
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
        // 8=1 token-auth state message (35=X family). Mirrors ib-agent#152 slice 2.
        let msg = build_control_frame("1", b"35=X\x011137=ABCDEF\x01");
        let mut conn = test_connection_with_buf(msg.clone());
        let frames = conn.extract_frames();
        assert_eq!(frames.len(), 1);
        match &frames[0] {
            Frame::Control(data) => assert_eq!(data, &msg),
            other => panic!("expected Frame::Control, got {:?}", other),
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
            other => panic!("expected Frame::Control, got {:?}", other),
        }
        assert_eq!(conn.buffered(), 0);
    }

    #[test]
    fn frame_extraction_control_then_fixcomp_zero_loss() {
        // ibx#185 acceptance: an 8=1 control frame ahead of a FIXCOMP frame in
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
            other => panic!("expected Frame::Control first, got {:?}", other),
        }
        match &frames[1] {
            Frame::FixComp(data) => assert_eq!(data, &comp),
            other => panic!("expected Frame::FixComp second, got {:?}", other),
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

    fn keyed_conn(mac_key: &[u8], iv: &[u8]) -> (Connection, MemTransport) {
        let (mut conn, peer) = loopback();
        conn.set_keys(Vec::new(), Vec::new(), mac_key.to_vec(), iv.to_vec());
        (conn, peer)
    }

    /// `msg` signed with its signature value changed (body intact).
    fn bad_signature(signed: &[u8]) -> Vec<u8> {
        let mut bad = signed.to_vec();
        let pos = find_subsequence(&bad, b"8349=").unwrap() + 5;
        bad[pos] = if bad[pos] == b'0' { b'1' } else { b'0' };
        bad
    }

    // ibx#275: the read IV advances only after a match; an unsigned frame
    // is accepted and leaves the IV as it is.
    #[test]
    fn unsign_advances_the_iv_only_after_a_match() {
        let mac_key: Vec<u8> = (0..20).collect();
        let iv: Vec<u8> = (0..16).collect();
        let (mut conn, _server) = keyed_conn(&mac_key, &iv);
        let (signed, next_iv) = fix::fix_sign(&fix_build(&[(35, "0")], 1), &mac_key, &iv);

        let (_, valid) = conn.unsign(&bad_signature(&signed));
        assert!(!valid, "tampered signature detected");
        assert_eq!(conn.read_iv, iv, "IV kept after a mismatch");

        let unsigned = fix_build(&[(35, "0")], 2);
        let (out, valid) = conn.unsign(&unsigned);
        assert!(valid);
        assert_eq!(out, unsigned);
        assert_eq!(conn.read_iv, iv, "IV kept for an unsigned frame");

        let (_, valid) = conn.unsign(&signed);
        assert!(valid);
        assert_eq!(conn.read_iv, next_iv, "IV advanced after a match");
    }
}
