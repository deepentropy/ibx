//! Connection wrapping a TLS or raw TCP stream with read/write buffers.
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
    Rustls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
    Raw(TcpStream),
    #[cfg(any(test, feature = "test-support"))]
    Mem(MemTransport),
}

impl Stream {
    /// A socket of the system, not an in-memory pipe.
    fn is_socket(&self) -> bool {
        self.raw_handle().is_some()
    }

    #[cfg(unix)]
    fn raw_handle(&self) -> Option<crate::engine::park::RawHandle> {
        use std::os::fd::AsRawFd;
        match self {
            Self::Tls(s) => Some(s.get_ref().as_raw_fd()),
            Self::Rustls(s) => Some(s.sock.as_raw_fd()),
            Self::Raw(s) => Some(s.as_raw_fd()),
            #[cfg(any(test, feature = "test-support"))]
            Self::Mem(_) => None,
        }
    }

    #[cfg(windows)]
    fn raw_handle(&self) -> Option<crate::engine::park::RawHandle> {
        use std::os::windows::io::AsRawSocket;
        match self {
            Self::Tls(s) => Some(s.get_ref().as_raw_socket()),
            Self::Rustls(s) => Some(s.sock.as_raw_socket()),
            Self::Raw(s) => Some(s.as_raw_socket()),
            #[cfg(any(test, feature = "test-support"))]
            Self::Mem(_) => None,
        }
    }
}

impl Read for Stream {
    #[inline]
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Tls(s) => s.read(buf),
            Self::Rustls(s) => s.read(buf),
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
            Self::Rustls(s) => write_controlled_tls(s, buf),
            Self::Raw(s) => s.write(buf),
            #[cfg(any(test, feature = "test-support"))]
            Self::Mem(s) => s.write(buf),
        }
    }

    #[inline]
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Tls(s) => s.flush(),
            Self::Rustls(s) => s.flush(),
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
            Self::Rustls(s) => { let _ = s.sock.shutdown(std::net::Shutdown::Both); },
            Self::Raw(s) => Transport::shutdown(s),
            #[cfg(any(test, feature = "test-support"))]
            Self::Mem(s) => Transport::shutdown(s),
        }
    }

    fn set_nonblocking(&mut self, on: bool) -> io::Result<()> {
        match self {
            Self::Tls(s) => Transport::set_nonblocking(s, on),
            Self::Rustls(s) => s.sock.set_nonblocking(on),
            Self::Raw(s) => Transport::set_nonblocking(s, on),
            #[cfg(any(test, feature = "test-support"))]
            Self::Mem(s) => Transport::set_nonblocking(s, on),
        }
    }
}

impl Stream {
    fn pending_tls_output(&self) -> bool {
        matches!(self, Self::Rustls(stream) if stream.conn.wants_write())
    }

    fn flush_queued_transport(&mut self) -> io::Result<()> {
        match self {
            Self::Rustls(stream) => flush_rustls_output(&mut stream.conn, &mut stream.sock),
            _ => Ok(()),
        }
    }

    fn write_queued(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self {
            Self::Rustls(stream) => write_rustls_queued(&mut stream.conn, &mut stream.sock, bytes),
            _ => self.write(bytes),
        }
    }
}

fn flush_rustls_output<W: Write>(connection: &mut rustls::ClientConnection, writer: &mut W) -> io::Result<()> {
    while connection.wants_write() {
        match connection.write_tls(writer) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "TLS transport accepted no bytes")),
            Ok(_) => {},
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {},
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn write_rustls_queued<W: Write>(connection: &mut rustls::ClientConnection, writer: &mut W, bytes: &[u8]) -> io::Result<usize> {
    // Flush earlier ciphertext first. WouldBlock here consumes no new plaintext,
    // so the outer queue can retain its exact offset.
    flush_rustls_output(connection, writer)?;
    // A post-acceptance flush error must never be reported as no acceptance:
    // retrying those same plaintext bytes would duplicate an encrypted frame.
    connection.writer().write(bytes)
}

// rustls StreamOwned::write accepts plaintext then suppresses a complete_io
// error. Flush immediately so the caller sees transport errors in this attempt.
// An error can still follow partial effects: the caller must retire the socket,
// never retry/replay this write on the same or a replacement controlled session.
pub(crate) fn write_controlled_tls(
    stream: &mut rustls::StreamOwned<rustls::ClientConnection, TcpStream>,
    buf: &[u8],
) -> io::Result<usize> {
    let written = stream.write(buf)?;
    stream.flush()?;
    Ok(written)
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
    /// The first bounded receive failure. Retire the transport after it.
    limited_read_error: Option<(io::ErrorKind, String)>,
    /// The socket is in non-blocking mode for good (ibx#530): a read with
    /// nothing to read returns at once. Set with the queued writes, on a
    /// socket only.
    nonblocking: bool,
    /// Bytes read so far.
    bytes_in: u64,
}

impl Connection {
    /// Create a new connection from an already-established TLS stream.
    ///
    /// A blocking socket with a 1ms read timeout until the engine takes it
    /// (`set_queued_writes`), as `new_raw`: before that a write either
    /// commits fully or fails, so the sequence and signature chains never
    /// advance for a message that is not on the wire.
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
            limited_read_error: None,
            nonblocking: false,
            bytes_in: 0,
        }
    }

    /// Carry an already-established controlled TLS stream into the engine. The
    /// cancellation scope continues to own an abort clone of its raw socket.
    /// Trust configuration and TLS handshake completion belong to the caller.
    pub(crate) fn new_controlled_tls(
        stream: Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>,
    ) -> io::Result<Self> {
        stream.sock.set_read_timeout(Some(std::time::Duration::from_millis(1)))?;
        Ok(Self::on(Stream::Rustls(stream)))
    }

    /// Create a new connection from a raw TCP stream (for farm connections).
    /// Enables TCP_NODELAY.
    pub fn new_raw(stream: TcpStream) -> io::Result<Self> {
        stream.set_nodelay(true)?;
        // A blocking socket with a 1ms read timeout until the engine takes
        // it (`set_queued_writes`): a non-blocking write_all can fail part
        // way (WouldBlock) with the signature chain already advanced.
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

    /// Read what the socket has into the internal buffer. Returns the number
    /// of bytes read, or 0 when there is none: at once on a connection the
    /// engine took (`set_queued_writes`), after the 1ms read timeout before
    /// that and on an in-memory connection.
    pub fn try_recv(&mut self) -> io::Result<usize> {
        let mut tmp = [0u8; RECV_BUF_SIZE];
        match self.stream.read(&mut tmp) {
            Ok(0) => Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "connection closed",
            )),
            Ok(n) => {
                self.buf.extend_from_slice(&tmp[..n]);
                self.bytes_in += n as u64;
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
                // A read can end inside a frame header ("8=FIXC" seen live,
                // ibx#436 paper run of 04/10/2026): those bytes start the
                // next frame and are kept; dropping them lost a compressed
                // frame and the reply it held.
                None if partial_header_len(&self.buf) > 0 => {
                    let keep = partial_header_len(&self.buf);
                    let drop = self.buf.len() - keep;
                    if drop > 0 {
                        log::warn!("extract_frames: dropping {}B (no header) before a partial header", drop);
                        self.buf.drain(..drop);
                    }
                    break;
                }
                None => {
                    // Unknown input can include login tokens. Record only length;
                    // never format raw payload bytes into a diagnostic log.
                    log::warn!("extract_frames: dropping {}B (no header)", self.buf.len());
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

    /// Read at most one complete frame under explicit retained/frame byte limits.
    ///
    /// Returns no frames for a partial frame or an idle transport. EOF, malformed
    /// headers/checksums, exceeded limits and hard transport errors are sticky:
    /// retire this connection instead of retrying it. Unknown bytes are rejected,
    /// never skipped. Only FIX.4.1, FIXCOMP and 8=O/1/X headers are admitted.
    /// Header bytes are inspected before body growth; the declared total must
    /// fit both limits. This does not inflate, unsign or interpret a frame.
    ///
    /// One frame per call bounds owned result count/bytes even under a flood.
    /// Existing login seed buffers are checked before new allocation. The legacy
    /// initial 32KiB capacity is unchanged; these limits bound retained lengths.
    pub fn poll_limited(&mut self, max_buffer_bytes: usize, max_frame_bytes: usize) -> io::Result<Vec<Frame>> {
        if let Some((kind, text)) = &self.limited_read_error {
            return Err(io::Error::new(*kind, text.clone()));
        }
        if max_frame_bytes == 0 || max_frame_bytes > max_buffer_bytes {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid receive limits"));
        }
        let result = self.poll_limited_inner(max_buffer_bytes, max_frame_bytes);
        if let Err(error) = &result {
            self.limited_read_error = Some((error.kind(), error.to_string()));
        }
        result
    }

    fn poll_limited_inner(&mut self, max_buffer_bytes: usize, max_frame_bytes: usize) -> io::Result<Vec<Frame>> {
        if self.buf.len() > max_buffer_bytes {
            return Err(limited_frame_error("retained receive byte limit exceeded"));
        }
        loop {
            let announced = limited_frame_length(&self.buf, max_frame_bytes)?;
            if let Some((total, kind)) = announced {
                if total > max_buffer_bytes {
                    return Err(limited_frame_error("announced receive byte limit exceeded"));
                }
                if self.buf.len() >= total {
                    // Signed FIX computes checksum before XOR distortion. The
                    // caller must verify HMAC, then checksum on undistorted bytes.
                    if matches!(kind, LimitedFrameKind::Fix) && !fix::is_signed(&self.buf[..total]) {
                        validate_limited_fix_checksum(&self.buf[..total])?;
                    }
                    let bytes: Vec<u8> = self.buf.drain(..total).collect();
                    let frame = match kind {
                        LimitedFrameKind::Fix => Frame::Fix(bytes),
                        LimitedFrameKind::FixComp => Frame::FixComp(bytes),
                        LimitedFrameKind::Binary => Frame::Binary(bytes),
                        LimitedFrameKind::Control => Frame::Control(bytes),
                    };
                    return Ok(vec![frame]);
                }
            }
            // Until a header announces a valid total, read just one stack byte.
            // Afterwards read only this frame's admitted remainder. No coalesced
            // suffix can exceed the budget while the caller processes this frame.
            let wanted = announced.map_or(1, |(total, _)| total - self.buf.len())
                .min(RECV_BUF_SIZE).min(max_buffer_bytes.saturating_sub(self.buf.len()));
            if wanted == 0 {
                return Err(limited_frame_error("incomplete frame exhausted receive budget"));
            }
            let mut chunk = [0u8; RECV_BUF_SIZE];
            match self.stream.read(&mut chunk[..wanted]) {
                Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "bounded transport closed")),
                Ok(count) => {
                    // Validate the next header byte in fixed stack storage before
                    // extending the owned buffer. Header length is independently
                    // limited, including unterminated decimal length fields.
                    if announced.is_none() {
                        let mut header = [0u8; LIMITED_HEADER_BYTES];
                        let used = self.buf.len();
                        if used >= header.len() {
                            return Err(limited_frame_error("frame header byte limit exceeded"));
                        }
                        header[..used].copy_from_slice(&self.buf);
                        header[used] = chunk[0];
                        limited_frame_length(&header[..used + 1], max_frame_bytes)?;
                    }
                    self.buf.try_reserve_exact(count).map_err(|_| limited_frame_error("receive allocation failed"))?;
                    self.buf.extend_from_slice(&chunk[..count]);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock || error.kind() == io::ErrorKind::TimedOut => return Ok(Vec::new()),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => return Ok(Vec::new()),
                Err(error) => return Err(error),
            }
        }
    }

    /// Send a bounded signed FIX control frame with at most one in-flight write.
    ///
    /// Requires queued-write mode. Any pending plaintext or Rustls ciphertext
    /// rejects admission with WouldBlock before allocation, signing or sequence
    /// changes. The caller should retire a session on an excessive TestRequest
    /// flood instead of creating an unbounded retry queue. Hard write failures
    /// remain sticky through the existing write_error/flush_queued contract.
    /// Fields are restricted to 1..=32, bounded in total, without embedded SOH or
    /// generated framing tags; sequence exhaustion fails before writing.
    pub fn send_fix_limited(&mut self, fields: &[(u32, &str)], max_frame_bytes: usize) -> io::Result<()> {
        if self.write_error.is_some() {
            return Err(self.failed());
        }
        if !self.queued_writes {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "bounded sends require queued-write mode"));
        }
        if self.has_queued_output() {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "bounded output is still pending"));
        }
        if fields.is_empty() || fields.len() > 32 || max_frame_bytes == 0 || self.seq >= 999_999 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid bounded FIX send"));
        }
        // Conservatively includes headers, checksum, sequence and HMAC trailer.
        // Admission precedes the native builder's owned temporary buffers.
        let mut admitted = 64usize;
        for &(tag, value) in fields {
            if matches!(tag, 8 | 9 | 10 | 34 | 8349) || value.as_bytes().contains(&SOH) {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid bounded FIX field"));
            }
            admitted = admitted.checked_add(12).and_then(|n| n.checked_add(value.len()))
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "bounded FIX size overflow"))?;
            if admitted > max_frame_bytes {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "bounded FIX frame byte limit exceeded"));
            }
        }
        self.send_fix(fields)
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
    ///
    /// With it a socket goes to non-blocking mode for good (ibx#530): a read
    /// returns at once when there is nothing, and the engine waits on all
    /// its sockets together instead of on each in turn.
    pub fn set_queued_writes(&mut self, on: bool) {
        self.queued_writes = on;
        if on && !self.nonblocking && self.stream.is_socket() {
            self.nonblocking = self.stream.set_nonblocking(true).is_ok();
        }
    }

    /// The socket to wait on for data, for a connection whose reads return
    /// at once. `None` for one that still waits in its own read.
    pub(crate) fn wait_handle(&self) -> Option<crate::engine::park::RawHandle> {
        if self.nonblocking { self.stream.raw_handle() } else { None }
    }

    /// Bytes read so far: a change tells a pass that read something.
    #[inline]
    pub(crate) fn bytes_in(&self) -> u64 {
        self.bytes_in
    }

    /// Whether accepted frames are still waiting to be written.
    #[inline]
    pub fn has_queued_output(&self) -> bool {
        !self.out.is_empty() || self.stream.pending_tls_output()
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
        if !self.has_queued_output() {
            return Ok(());
        }
        if !self.nonblocking {
            if let Err(e) = self.stream.set_nonblocking(true) {
                return Err(self.record_write_error(e));
            }
        }
        // The frames that wait leave in one write, as the reference writes
        // the messages it sends together (ibx#547: the orders of a bracket
        // in one write, 26/09 and 09/10/2026), and with one system call.
        // Not while a frame is partly written: that write is completed
        // with the same bytes first.
        if self.out.len() > 1 && self.out_pos == 0 {
            let mut all = Vec::with_capacity(self.out.iter().map(Vec::len).sum());
            for frame in self.out.drain(..) {
                all.extend_from_slice(&frame);
            }
            self.out.push_back(all);
        }
        let result = loop {
            let Some(front) = self.out.front() else {
                // Rustls can retain ciphertext after accepting the last queued
                // plaintext. Keep it visible and poll it without replaying bytes.
                break match self.stream.flush_queued_transport() {
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(()),
                    result => result,
                };
            };
            // A partial TLS record is completed by calling again with the
            // same bytes, which this does.
            match self.stream.write_queued(&front[self.out_pos..]) {
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
        let restored = if self.nonblocking { Ok(()) } else { self.stream.set_nonblocking(false) };
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

// A valid complete header needs at most 10 + 2 + 20 + 1 bytes on 64-bit.
// A fixed ceiling also prevents endless unterminated versions/length fields.
const LIMITED_HEADER_BYTES: usize = 64;

#[derive(Clone, Copy)]
enum LimitedFrameKind { Fix, FixComp, Binary, Control }

fn limited_frame_error(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// Inspect a borrowed prefix without allocating or permissive resynchronization.
fn limited_frame_length(bytes: &[u8], max_frame_bytes: usize) -> io::Result<Option<(usize, LimitedFrameKind)>> {
    if bytes.is_empty() { return Ok(None); }
    let headers: [(&[u8], LimitedFrameKind); 5] = [
        (b"8=FIX.4.1\x01", LimitedFrameKind::Fix),
        (b"8=FIXCOMP\x01", LimitedFrameKind::FixComp),
        (b"8=O\x01", LimitedFrameKind::Binary),
        (b"8=1\x01", LimitedFrameKind::Control),
        (b"8=X\x01", LimitedFrameKind::Control),
    ];
    let Some((header, kind)) = headers.into_iter().find(|(header, _)| bytes.starts_with(header)) else {
        if bytes.len() < LIMITED_HEADER_BYTES && headers.iter().any(|(header, _)| header.starts_with(bytes)) {
            return Ok(None);
        }
        return Err(limited_frame_error("unsupported frame header"));
    };
    let rest = &bytes[header.len()..];
    if rest.len() < 2 {
        return if b"9=".starts_with(rest) { Ok(None) } else { Err(limited_frame_error("frame body length tag missing")) };
    }
    if !rest.starts_with(b"9=") { return Err(limited_frame_error("frame body length tag missing")); }
    let mut length = 0usize;
    for (index, &byte) in rest[2..].iter().enumerate() {
        let header_end = header.len() + 2 + index + 1;
        if header_end > LIMITED_HEADER_BYTES { return Err(limited_frame_error("frame header byte limit exceeded")); }
        if byte == SOH {
            if index == 0 { return Err(limited_frame_error("empty frame body length")); }
            let trailer = if matches!(kind, LimitedFrameKind::Fix) { 7 } else { 0 };
            let total = header_end.checked_add(length).and_then(|n| n.checked_add(trailer))
                .ok_or_else(|| limited_frame_error("frame total length overflow"))?;
            if total > max_frame_bytes { return Err(limited_frame_error("announced frame byte limit exceeded")); }
            return Ok(Some((total, kind)));
        }
        if !byte.is_ascii_digit() { return Err(limited_frame_error("invalid frame body length")); }
        length = length.checked_mul(10).and_then(|n| n.checked_add((byte - b'0') as usize))
            .ok_or_else(|| limited_frame_error("frame body length overflow"))?;
        // This check rejects an already oversized decimal prefix, before its
        // terminator (or any payload) is buffered.
        if length > max_frame_bytes { return Err(limited_frame_error("announced frame byte limit exceeded")); }
    }
    if bytes.len() >= LIMITED_HEADER_BYTES { return Err(limited_frame_error("frame header byte limit exceeded")); }
    Ok(None)
}

fn validate_limited_fix_checksum(bytes: &[u8]) -> io::Result<()> {
    if bytes.len() < 7 { return Err(limited_frame_error("missing FIX checksum")); }
    let trailer = &bytes[bytes.len() - 7..];
    if !trailer.starts_with(b"10=") || trailer[6] != SOH || !trailer[3..6].iter().all(u8::is_ascii_digit) {
        return Err(limited_frame_error("invalid FIX checksum trailer"));
    }
    let expected = bytes[..bytes.len() - 7].iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte));
    let supplied = (trailer[3] - b'0') as u16 * 100 + (trailer[4] - b'0') as u16 * 10 + (trailer[5] - b'0') as u16;
    if supplied != expected as u16 { return Err(limited_frame_error("FIX checksum mismatch")); }
    Ok(())
}

/// Frame headers the reader recognizes.
const FRAME_HEADERS: [&[u8]; 5] = [b"8=FIX.", b"8=FIXCOMP\x01", b"8=O\x01", b"8=1\x01", b"8=X\x01"];

/// Length of the longest end of `buf` that is the start of a frame header
/// (a header cut by the end of a read), 0 when none.
fn partial_header_len(buf: &[u8]) -> usize {
    let longest = FRAME_HEADERS.iter().map(|h| h.len()).max().unwrap_or(0);
    (1..longest.min(buf.len() + 1))
        .rev()
        .find(|&n| FRAME_HEADERS.iter().any(|h| n < h.len() && buf[buf.len() - n..] == h[..n]))
        .unwrap_or(0)
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

    // ibx#436: a read that ends inside the header of a compressed frame
    // ("8=FIXC", seen live) keeps those bytes: the frame is read whole with
    // the next bytes, for every cut of its header.
    #[test]
    fn frame_extraction_header_cut_by_the_read_end() {
        let first = fixcomp_build(&fix_build(&[(35, "Q")], 1));
        let second = fixcomp_build(&fix_build(&[(35, "P")], 2));
        for cut in 1..="8=FIXCOMP\x01".len() {
            let mut conn = test_connection_with_buf(first.clone());
            conn.inject_buf(&second[..cut]);
            let frames = conn.extract_frames();
            assert_eq!(frames.len(), 1, "cut {cut}");
            assert_eq!(conn.buffered(), cut, "cut {cut}: the header start is kept");
            conn.inject_buf(&second[cut..]);
            let frames = conn.extract_frames();
            match frames.as_slice() {
                [Frame::FixComp(data)] => assert_eq!(data, &second, "cut {cut}"),
                other => panic!("cut {cut}: {:?}", other),
            }
        }
        // Bytes that cannot start a header are still dropped.
        let mut conn = test_connection_with_buf(b"zz8=FIXC".to_vec());
        assert!(conn.extract_frames().is_empty());
        assert_eq!(conn.buffered(), 6);
        let mut conn = test_connection_with_buf(b"zzzz".to_vec());
        assert!(conn.extract_frames().is_empty());
        assert_eq!(conn.buffered(), 0);
        assert_eq!(partial_header_len(b"..8=FIX"), 5);
        assert_eq!(partial_header_len(b"..8=O"), 3);
        assert_eq!(partial_header_len(b"8=FIXCOMP"), 9);
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

    #[test]
    fn controlled_tls_connection_preserves_framing_and_closes_raw_socket() {
        // This fixture tests transport ownership/conversion, not a TLS login.
        // Actual verified handshake/cancellation fixtures belong to lifecycle.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let socket = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut peer, _) = listener.accept().unwrap();
        let config = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions().unwrap()
        .with_root_certificates(rustls::RootCertStore::empty())
        .with_no_client_auth();
        let tls = rustls::ClientConnection::new(std::sync::Arc::new(config),
            rustls::pki_types::ServerName::try_from("localhost").unwrap().to_owned()).unwrap();
        let login = crate::gateway::LinkStream::ControlledTls(Box::new(rustls::StreamOwned::new(tls, socket)));
        let local_address = login.tcp().local_addr().unwrap();
        let mut connection = login.into_connection().unwrap();
        match &connection.stream {
            Stream::Rustls(stream) => {
                assert_eq!(stream.sock.local_addr().unwrap(), local_address);
                assert_eq!(stream.sock.read_timeout().unwrap(),
                    Some(std::time::Duration::from_millis(1)));
            }
            _ => panic!("controlled TLS must not switch to legacy native-tls"),
        }
        // The engine must poll controlled TLS through the same raw socket.
        assert!(connection.wait_handle().is_none());
        connection.set_queued_writes(true);
        assert!(connection.wait_handle().is_some());
        if let Stream::Rustls(stream) = &mut connection.stream {
            assert_eq!(stream.sock.read(&mut [0]).unwrap_err().kind(), io::ErrorKind::WouldBlock);
        }
        let frame = fix_build(&[(35, "0")], 1);
        connection.seed_buffer(&frame);
        assert!(matches!(connection.extract_frames().as_slice(),
            [Frame::Fix(bytes)] if bytes == &frame));
        connection.shutdown();
        peer.set_read_timeout(Some(std::time::Duration::from_secs(1))).unwrap();
        let mut byte = [0];
        assert_eq!(peer.read(&mut byte).unwrap(), 0);
    }
}

#[cfg(test)]
mod controlled_queued_tls_tests {
    use super::*;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
    use std::sync::Arc;

    fn established_pair() -> (rustls::ClientConnection, rustls::ServerConnection) {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(CertificateDer::from(include_bytes!("../testdata/controlled-tls/ca.der").to_vec())).unwrap();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let client_config = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions().unwrap()
            .with_root_certificates(roots).with_no_client_auth();
        let server_config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions().unwrap().with_no_client_auth()
            .with_single_cert(vec![CertificateDer::from(include_bytes!("../testdata/controlled-tls/server.der").to_vec())],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(include_bytes!("../testdata/controlled-tls/server-key.der").to_vec()))).unwrap();
        let mut client = rustls::ClientConnection::new(Arc::new(client_config), ServerName::try_from("localhost").unwrap().to_owned()).unwrap();
        let mut server = rustls::ServerConnection::new(Arc::new(server_config)).unwrap();
        for _ in 0..16 {
            let mut bytes = Vec::new();
            client.write_tls(&mut bytes).unwrap();
            if !bytes.is_empty() {
                server.read_tls(&mut io::Cursor::new(bytes)).unwrap();
                server.process_new_packets().unwrap();
            }
            let mut bytes = Vec::new();
            server.write_tls(&mut bytes).unwrap();
            if !bytes.is_empty() {
                client.read_tls(&mut io::Cursor::new(bytes)).unwrap();
                client.process_new_packets().unwrap();
            }
            if !client.is_handshaking() && !server.is_handshaking() { return (client, server); }
        }
        panic!("offline TLS handshake did not complete");
    }

    struct Backpressure { remaining: usize, bytes: Vec<u8> }
    impl Write for Backpressure {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.remaining == 0 { return Err(io::ErrorKind::WouldBlock.into()); }
            let count = bytes.len().min(self.remaining);
            self.bytes.extend_from_slice(&bytes[..count]);
            self.remaining -= count;
            Ok(count)
        }
        fn flush(&mut self) -> io::Result<()> { Ok(()) }
    }

    #[test]
    fn controlled_tls_queue_retains_ciphertext_without_replaying_plaintext() {
        let (mut client, mut server) = established_pair();
        let mut output = Backpressure { remaining: 7, bytes: Vec::new() };
        assert_eq!(write_rustls_queued(&mut client, &mut output, b"first-frame").unwrap(), 11);
        assert_eq!(flush_rustls_output(&mut client, &mut output).unwrap_err().kind(), io::ErrorKind::WouldBlock);
        assert!(client.wants_write());
        assert_eq!(write_rustls_queued(&mut client, &mut output, b"second-frame").unwrap_err().kind(), io::ErrorKind::WouldBlock);
        output.remaining = usize::MAX;
        assert_eq!(write_rustls_queued(&mut client, &mut output, b"second-frame").unwrap(), 12);
        flush_rustls_output(&mut client, &mut output).unwrap();
        assert!(!client.wants_write());
        server.read_tls(&mut io::Cursor::new(output.bytes)).unwrap();
        server.process_new_packets().unwrap();
        let mut plaintext = [0u8; 23];
        server.reader().read_exact(&mut plaintext).unwrap();
        assert_eq!(&plaintext, b"first-framesecond-frame");
        assert_eq!(server.reader().read(&mut [0u8; 1]).unwrap_err().kind(), io::ErrorKind::WouldBlock);
    }
}

#[cfg(test)]
mod bounded_transport_tests {
    use super::*;
    use crate::protocol::fix::fix_build;

    fn pair() -> (Connection, MemTransport) {
        let (stream, peer) = mem_pair();
        let connection = Connection::new_mem(stream);
        connection.set_mem_read_timeout(std::time::Duration::ZERO);
        (connection, peer)
    }

    #[test]
    fn bounded_announced_lengths_fail_before_body_allocation() {
        for header in ["8=FIX.4.1\x01", "8=FIXCOMP\x01", "8=O\x01", "8=1\x01", "8=X\x01"] {
            let (mut connection, mut peer) = pair();
            let capacity = connection.buf.capacity();
            peer.write_all(format!("{header}9=999999999999999999999999\x01").as_bytes()).unwrap();
            let error = connection.poll_limited(128, 128).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert!(connection.buffered() < 32, "only admitted header prefix retained");
            assert_eq!(connection.buf.capacity(), capacity, "no owned body allocation");
            // A later valid frame cannot hide the prior framing failure.
            peer.write_all(&fix_build(&[(35, "0")], 1)).unwrap();
            assert_eq!(connection.poll_limited(128, 128).unwrap_err().to_string(), error.to_string());
        }
    }

    #[test]
    fn bounded_signed_fix_is_admitted_and_verified_before_checksum_validation() {
        let key = vec![7; 20];
        let iv: Vec<u8> = (0..16).collect();
        let plain = fix_build(&[(35, "0"), (112, "signed-test")], 1);
        let (signed, _) = fix::fix_sign(&plain, &key, &iv);
        let (mut connection, mut peer) = pair();
        connection.set_keys(Vec::new(), Vec::new(), key.clone(), iv.clone());
        peer.write_all(&signed).unwrap();
        let frames = connection.poll_limited(512, 512).unwrap();
        let [Frame::Fix(bytes)] = frames.as_slice() else { panic!("signed frame expected") };
        let (undistorted, valid) = connection.unsign(bytes);
        assert!(valid);
        validate_limited_fix_checksum(&undistorted).unwrap();
        let (mut connection, mut peer) = pair();
        connection.set_keys(Vec::new(), Vec::new(), key, iv);
        let mut bad = signed.clone();
        let position = find_subsequence(&bad, b"8349=").unwrap() + 5;
        bad[position] = if bad[position] == b'0' { b'1' } else { b'0' };
        peer.write_all(&bad).unwrap();
        let frames = connection.poll_limited(512, 512).unwrap();
        let [Frame::Fix(bytes)] = frames.as_slice() else { panic!("signed frame expected") };
        assert!(!connection.unsign(bytes).1, "corrupt HMAC must not be accepted");
    }
    #[test]
    fn bounded_decimal_overflow_and_malformed_headers_are_rejected() {
        let overflow = format!("8=O\x019={}0\x01", usize::MAX);
        for data in [overflow.as_bytes(), b"8=O\x019=-1\x01", b"8=X\x019=\x01", b"8=FIX.4.2\x019=1\x01", b"junk8=O\x019=1\x01", b"8=O\x0135=0\x019=1\x01"] {
            let (mut connection, mut peer) = pair();
            peer.write_all(data).unwrap();
            assert_eq!(connection.poll_limited(usize::MAX, usize::MAX).unwrap_err().kind(), io::ErrorKind::InvalidData);
        }
    }

    #[test]
    fn bounded_incremental_reads_preserve_and_cap_valid_frames() {
        let frame = fix_build(&[(35, "0"), (112, "test")], 1);
        let (mut connection, mut peer) = pair();
        for (index, byte) in frame.iter().enumerate() {
            peer.write_all(&[*byte]).unwrap();
            let frames = connection.poll_limited(frame.len(), frame.len()).unwrap();
            assert!(connection.buffered() <= frame.len());
            if index + 1 == frame.len() {
                assert!(matches!(frames.as_slice(), [Frame::Fix(bytes)] if bytes == &frame));
                assert_eq!(connection.buffered(), 0);
            } else {
                assert!(frames.is_empty());
                assert_eq!(connection.buffered(), index + 1);
            }
        }
        let mut corrupted = frame.clone();
        let checksum_digit = corrupted.len() - 2;
        corrupted[checksum_digit] = if corrupted[checksum_digit] == b'9' { b'8' } else { b'9' };
        peer.write_all(&corrupted).unwrap();
        assert_eq!(connection.poll_limited(frame.len(), frame.len()).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn bounded_coalesced_seed_and_wire_return_one_frame_per_poll() {
        let first = fix_build(&[(35, "0")], 1);
        let compressed = fixcomp::fixcomp_build(&first);
        let binary = b"8=O\x019=1\x01x".to_vec();
        let control = b"8=1\x019=1\x01y".to_vec();
        let frames = [&first, &compressed, &binary, &control];
        for seeded in [false, true] {
            let (mut connection, mut peer) = pair();
            let bytes: Vec<u8> = frames.iter().flat_map(|frame| frame.iter().copied()).collect();
            if seeded { connection.seed_buffer(&bytes); } else { peer.write_all(&bytes).unwrap(); }
            for expected in frames {
                let returned = connection.poll_limited(bytes.len(), bytes.len()).unwrap();
                assert_eq!(returned.len(), 1);
                let actual = match &returned[0] {
                    Frame::Fix(bytes) | Frame::FixComp(bytes) | Frame::Binary(bytes) | Frame::Control(bytes) => bytes,
                };
                assert_eq!(actual, expected);
            }
            assert_eq!(connection.buffered(), 0);
        }
    }

    #[test]
    fn bounded_seed_limit_and_eof_are_explicit_and_sticky() {
        let (mut connection, peer) = pair();
        let frame = fix_build(&[(35, "0")], 1);
        connection.seed_buffer(&frame);
        let capacity = connection.buf.capacity();
        assert_eq!(connection.poll_limited(8, 8).unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert_eq!(connection.buf.capacity(), capacity);
        drop(peer);
        assert_eq!(connection.poll_limited(128, 128).unwrap_err().kind(), io::ErrorKind::InvalidData);
        let (mut connection, mut peer) = pair();
        peer.write_all(b"8=O\x019=4\x01a").unwrap();
        drop(peer);
        assert_eq!(connection.poll_limited(128, 128).unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(connection.poll_limited(128, 128).unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn bounded_outgoing_admission_preserves_sequence_and_signature_under_backpressure() {
        let (mut connection, mut peer) = pair();
        if let Stream::Mem(stream) = &connection.stream { stream.set_write_capacity(Some(3)); }
        connection.set_queued_writes(true);
        connection.set_keys(vec![1; 20], vec![2; 16], Vec::new(), Vec::new());
        connection.send_fix_limited(&[(35, "0"), (112, "first")], 256).unwrap();
        assert!(connection.has_queued_output());
        let iv = connection.sign_iv.clone();
        let queued: Vec<Vec<u8>> = connection.out.iter().cloned().collect();
        for _ in 0..100 {
            assert_eq!(connection.send_fix_limited(&[(35, "0"), (112, "second")], 256).unwrap_err().kind(), io::ErrorKind::WouldBlock);
        }
        assert_eq!(connection.seq, 1);
        assert_eq!(connection.sign_iv, iv);
        assert_eq!(connection.out.iter().cloned().collect::<Vec<_>>(), queued);
        assert_eq!(connection.out.len(), 1);
        let mut received = Vec::new();
        while connection.has_queued_output() {
            let mut chunk = [0u8; 3];
            let count = peer.read(&mut chunk).unwrap();
            received.extend_from_slice(&chunk[..count]);
            connection.flush_queued().unwrap();
        }
        let mut chunk = [0u8; 3];
        peer.set_read_timeout(Some(std::time::Duration::ZERO)).unwrap();
        if let Ok(count) = peer.read(&mut chunk) { received.extend_from_slice(&chunk[..count]); }
        assert_eq!(received, queued[0]);
    }

    #[test]
    fn bounded_send_rejects_oversize_before_state_changes_and_hard_failures_remain_sticky() {
        let (mut connection, peer) = pair();
        connection.set_queued_writes(true);
        assert!(connection.send_fix_limited(&[(35, "0"), (112, &"x".repeat(256))], 128).is_err());
        assert!(connection.send_fix_limited(&[(35, "0\x01bad")], 128).is_err());
        assert_eq!(connection.seq, 0);
        assert!(!connection.has_queued_output());
        assert!(connection.write_error().is_none());
        drop(peer);
        assert_eq!(connection.send_fix_limited(&[(35, "0")], 128).unwrap_err().kind(), io::ErrorKind::BrokenPipe);
        assert!(connection.write_error().is_some());
        assert_eq!(connection.seq, 0);
        assert_eq!(connection.send_fix_limited(&[(35, "0")], 128).unwrap_err().kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(connection.flush_queued().unwrap_err().kind(), io::ErrorKind::BrokenPipe);
    }
}