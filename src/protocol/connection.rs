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
}

impl Connection {
    /// Create a new connection from an already-established TLS stream.
    /// Sets the underlying TCP stream to non-blocking mode.
    pub fn new(stream: TlsStream<TcpStream>) -> io::Result<Self> {
        stream.get_ref().set_nonblocking(true)?;
        Ok(Self {
            stream: Stream::Tls(stream),
            buf: Vec::with_capacity(RECV_BUF_SIZE),
            seq: 0,
            sign_key: Vec::new(),
            sign_iv: Vec::new(),
            read_key: Vec::new(),
            read_iv: Vec::new(),
        })
    }

    /// Create a new connection from a raw TCP stream (for farm connections).
    /// Sets the stream to non-blocking mode and enables TCP_NODELAY.
    pub fn new_raw(stream: TcpStream) -> io::Result<Self> {
        stream.set_nodelay(true)?;
        stream.set_nonblocking(true)?;
        Ok(Self {
            stream: Stream::Raw(stream),
            buf: Vec::with_capacity(RECV_BUF_SIZE),
            seq: 0,
            sign_key: Vec::new(),
            sign_iv: Vec::new(),
            read_key: Vec::new(),
            read_iv: Vec::new(),
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
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(0),
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

            // Find earliest message start
            let fix_pos = find_subsequence(&self.buf, b"8=FIX.");
            let o_pos = find_subsequence(&self.buf, b"8=O\x01");

            match (fix_pos, o_pos) {
                (None, None) => {
                    self.buf.clear();
                    break;
                }
                _ => {}
            }

            // Skip garbage before earliest message
            let earliest = match (fix_pos, o_pos) {
                (Some(f), Some(o)) => f.min(o),
                (Some(f), None) => f,
                (None, Some(o)) => o,
                (None, None) => unreachable!(),
            };
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

    /// Build a FIX message, sign it, and send it. Increments seq and chains sign IV.
    pub fn send_fix(&mut self, fields: &[(u32, &str)]) -> io::Result<()> {
        self.seq += 1;
        let msg = fix::fix_build(fields, self.seq);
        let to_send = if self.sign_key.is_empty() {
            msg
        } else {
            let (signed, new_iv) = fix::fix_sign(&msg, &self.sign_key, &self.sign_iv);
            self.sign_iv = new_iv;
            signed
        };
        self.stream.write_all(&to_send)?;
        Ok(())
    }

    /// Build a message, compress, sign, and send. For farm non-heartbeat messages.
    pub fn send_fixcomp(&mut self, fields: &[(u32, &str)]) -> io::Result<()> {
        self.seq += 1;
        let msg = fix::fix_build(fields, self.seq);
        let wrapped = fixcomp::fixcomp_build(&msg);
        let to_send = if self.sign_key.is_empty() {
            wrapped
        } else {
            let (signed, new_iv) = fix::fix_sign(&wrapped, &self.sign_key, &self.sign_iv);
            self.sign_iv = new_iv;
            signed
        };
        self.stream.write_all(&to_send)?;
        Ok(())
    }

    /// Send raw bytes (pre-built message).
    pub fn send_raw(&mut self, data: &[u8]) -> io::Result<()> {
        self.stream.write_all(data)?;
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

/// Compute total length of a `8=O\x01 9=<body_len>\x01 ...` message.
fn binary_msg_length(data: &[u8]) -> Option<usize> {
    // 8=O\x01 → 4 bytes, then find 9=
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
}
