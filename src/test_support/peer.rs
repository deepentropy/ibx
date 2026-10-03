//! A scripted peer (server, data farm or historical farm) on the other end
//! of an in-memory connection. It reads with the same framing, signature
//! check and decompression as the engine, and answers with the same
//! builders, so a test sees the exact bytes the engine wrote.

use std::time::{Duration, Instant};

use crate::protocol::connection::{mem_pair, Connection, Frame, MemTransport};
use crate::protocol::fixcomp;

/// The peer end of an in-memory connection.
pub struct Peer {
    conn: Connection,
}

impl Peer {
    /// A connection for the engine and the peer on its other end.
    pub fn pair() -> (Connection, Peer) {
        let (engine, peer) = mem_pair();
        (Connection::new_mem(engine), Peer::new(peer))
    }

    /// A peer on one end of a pipe.
    pub fn new(end: MemTransport) -> Self {
        Self { conn: Connection::new_mem(end) }
    }

    /// Take the keys of the engine's connection, mirrored: the peer checks
    /// what the engine signs and signs what the engine checks. Call it after
    /// the engine side got its keys and before the first signed frame.
    pub fn sign_like(&mut self, engine: &Connection) {
        self.conn.set_keys(
            engine.read_key.clone(),
            engine.read_iv.clone(),
            engine.sign_key.clone(),
            engine.sign_iv.clone(),
        );
    }

    /// The peer's own connection (sequence number, keys).
    pub fn conn(&mut self) -> &mut Connection {
        &mut self.conn
    }

    /// Every complete frame received so far, as written on the wire
    /// (signature included), in order.
    pub fn frames(&mut self) -> Vec<Frame> {
        loop {
            match self.conn.try_recv() {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
        self.conn.extract_frames()
    }

    /// Every message received so far, in order: the signature checked and
    /// removed (a bad one fails the test), each compressed frame opened
    /// into its messages.
    pub fn messages(&mut self) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for frame in self.frames() {
            match frame {
                Frame::Fix(raw) | Frame::Binary(raw) | Frame::Control(raw) => out.push(self.unsign(&raw)),
                Frame::FixComp(raw) => {
                    let plain = self.unsign(&raw);
                    out.extend(fixcomp::fixcomp_decompress(&plain).expect("a valid compressed frame"));
                }
            }
        }
        out
    }

    /// Messages until `done` holds for those received so far, or until
    /// `timeout` (the engine runs on another thread).
    pub fn messages_until(&mut self, timeout: Duration, done: impl Fn(&[Vec<u8>]) -> bool) -> Vec<Vec<u8>> {
        let deadline = Instant::now() + timeout;
        let mut out = Vec::new();
        loop {
            out.extend(self.messages());
            if done(&out) || Instant::now() >= deadline {
                return out;
            }
        }
    }

    fn unsign(&mut self, raw: &[u8]) -> Vec<u8> {
        let (plain, valid) = self.conn.unsign(raw);
        assert!(valid, "bad signature on {}", crate::protocol::fix::fmt_pipe(raw));
        plain
    }

    /// Send a sequenced message (signed when the peer has keys).
    pub fn send_fix(&mut self, fields: &[(u32, &str)]) {
        self.conn.send_fix(fields).expect("peer send");
    }

    /// Send a compressed message (signed when the peer has keys).
    pub fn send_fixcomp(&mut self, fields: &[(u32, &str)]) {
        self.conn.send_fixcomp(fields).expect("peer send");
    }

    /// Send bytes as they are (a recorded frame).
    pub fn send_raw(&mut self, bytes: &[u8]) {
        self.conn.send_raw(bytes).expect("peer send");
    }

    /// Close the peer end: the engine reads the end of the stream.
    pub fn close(&mut self) {
        self.conn.shutdown();
    }
}
