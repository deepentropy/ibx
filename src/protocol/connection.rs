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
/// detector cannot run in exactly the case it was built for (ibx#254).
///
/// This bounds a single write syscall, not a whole frame: a peer draining a
/// trickle keeps resetting it and a large frame can take proportionally longer.
/// The case this exists for is the peer that makes no progress whatsoever,
/// which trips the first timeout. Kept under the liveness probe interval so a
/// stalled send cannot hold the loop past its own cadence.
const WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

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
/// put nothing on the wire — the chain is intact, the frame can be sent again,
/// and a peer that is merely slow does not cost the transport. Only the
/// stalled-forever case reaches the liveness deadlines, which is where it
/// belongs.
fn write_is_recoverable(err: &io::Error, written: usize, tls: bool) -> bool {
    // Never on TLS. The count here is plaintext accepted by the TLS layer, not
    // bytes on the socket: a stalled write can report none accepted while a
    // partial record has already gone to the peer. Retrying then puts a second
    // frame behind half of the first, and the sequence and signature state
    // would be committed for a frame the peer never receives whole — an order
    // reported as failed here could still arrive at the gateway.
    !tls
        && written == 0
        && matches!(err.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)
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
    /// Set once a write has failed. A timed-out or errored `write_all` may
    /// have put part of a frame on the wire, and outbound frames are
    /// HMAC-chained, so anything sent afterwards would be signed from state
    /// the peer does not share. There is no resuming mid-frame: the transport
    /// is finished and the reconnect path takes it from here.
    write_failed: bool,
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
            write_failed: false,
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
            let fix_pos = find_subsequence(&self.buf, b"8=FIX.");
            let o_pos = find_subsequence(&self.buf, b"8=O\x01");
            let one_pos = find_subsequence(&self.buf, b"8=1\x01");
            let x_pos = find_subsequence(&self.buf, b"8=X\x01");

            let earliest = [fix_pos, o_pos, one_pos, x_pos]
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

    /// Unsign a received frame using the read IV. Chains the IV.
    /// Returns the undistorted message bytes and whether the signature was valid.
    pub fn unsign(&mut self, msg: &[u8]) -> (Vec<u8>, bool) {
        if self.read_key.is_empty() {
            return (msg.to_vec(), true); // no signing configured
        }
        // Only unsign if 8349= HMAC tag is present (matching Python _unsign_conn)
        if !msg.windows(5).any(|w| w == b"8349=") {
            return (msg.to_vec(), true);
        }
        let (undistorted, new_iv, valid) = fix::fix_unsign(msg, &self.read_key, &self.read_iv);
        self.read_iv = new_iv;
        (undistorted, valid)
    }

    /// Write a whole frame, or give up on the transport.
    ///
    /// Tracks how much of the frame reached the socket, because that is what
    /// separates a transport that can carry on from one that cannot: a frame
    /// sent in part leaves the signature chain desynchronised and the first
    /// such failure is final, while a write that moved nothing can be retried
    /// (ibx#254). Once final, every later send fails fast rather than putting
    /// more frames on a wire the peer can no longer verify.
    fn write_frame(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.write_failed {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "connection abandoned after an incomplete write",
            ));
        }
        let mut written = 0;
        while written < bytes.len() {
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
                        log::warn!("write made no progress ({e}) — frame not sent");
                        return Err(e);
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
fn binary_msg_length(data: &[u8]) -> Option<usize> {
    // 4-byte tag-8 header ("8=O\x01" / "8=1\x01" / "8=X\x01"), then find 9=
    let after_8 = 4; // "8=O\x01"
    let tag9_pos = find_subsequence(&data[after_8..], b"9=").map(|p| after_8 + p)?;
    let soh_pos = data[tag9_pos..].iter().position(|&b| b == SOH).map(|p| tag9_pos + p)?;
    let body_len: usize = std::str::from_utf8(&data[tag9_pos + 2..soh_pos])
        .ok()?
        .parse()
        .ok()?;
    Some(soh_pos + 1 + body_len)
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
    Some(soh_pos + 1 + body_len + 7)
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

    /// Helper: create a Connection with a dummy TCP stream for buffer tests.
    /// We connect to a local listener so we get a valid TcpStream.
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
            write_failed: false,
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
            other => panic!("expected Frame::FixComp, got {:?}", other),
        }
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

    /// ibx#254: the hot loop is one thread driving three transports, and every
    /// send went out through an unbounded `write_all`. A peer that stops
    /// draining without closing blocks that thread — so liveness cannot be
    /// evaluated, shutdown cannot be serviced, and the reconnect cannot be
    /// polled. The heartbeat that exists to detect a wedged peer is itself a
    /// write, so the detector cannot run in exactly the case it was built for.
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
        // dead peer produces a broken pipe of its own, so only our own wording
        // distinguishes "refused without trying" from "tried and failed".
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
    /// the wire, so a merely slow peer does not cost the transport — that case
    /// belongs to the liveness deadlines, which is where the stalled-forever
    /// peer is caught.
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
