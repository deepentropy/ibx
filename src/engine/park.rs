//! The engine thread at rest (ibx#530).
//!
//! A pass of the hot loop reads every connection without waiting. When some
//! passes in a row found nothing to do, the thread waits here until a
//! connection has data or a command is sent, whichever comes first, so an
//! order or a request is never held back by a timed read of another
//! connection. A short timeout keeps the timers of the loop running.

use std::io;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crossbeam_channel::{SendError, Sender, TrySendError};

use crate::types::ControlCommand;

/// The socket of a connection, as the system's wait call takes it.
#[cfg(unix)]
pub(crate) type RawHandle = std::os::fd::RawFd;
#[cfg(windows)]
pub(crate) type RawHandle = std::os::windows::io::RawSocket;

#[cfg(unix)]
mod sys {
    pub(super) type PollFd = libc::pollfd;
    pub(super) const READABLE: i16 = libc::POLLIN;

    pub(super) fn entry(handle: super::RawHandle) -> PollFd {
        PollFd { fd: handle, events: READABLE, revents: 0 }
    }

    pub(super) fn wait(fds: &mut [PollFd], timeout_ms: i32) -> i32 {
        // SAFETY: `fds` is a valid, exclusively borrowed slice for the call.
        unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout_ms) }
    }
}

#[cfg(windows)]
mod sys {
    use windows_sys::Win32::Networking::WinSock::{WSAPoll, POLLRDNORM, WSAPOLLFD};

    pub(super) type PollFd = WSAPOLLFD;
    pub(super) const READABLE: i16 = POLLRDNORM;

    pub(super) fn entry(handle: super::RawHandle) -> PollFd {
        PollFd { fd: handle as usize, events: READABLE, revents: 0 }
    }

    pub(super) fn wait(fds: &mut [PollFd], timeout_ms: i32) -> i32 {
        // SAFETY: `fds` is a valid, exclusively borrowed slice for the call.
        unsafe { WSAPoll(fds.as_mut_ptr(), fds.len() as u32, timeout_ms) }
    }
}

/// Wakes the engine thread out of its wait. Shared by the engine and every
/// sender of commands.
pub struct Waker {
    /// The engine is waiting, or about to. Cleared by the first waker, so
    /// the others send nothing.
    parked: AtomicBool,
    /// A datagram socket connected to itself: a byte sent on it makes it
    /// readable, which ends the wait.
    socket: UdpSocket,
}

impl Waker {
    pub(crate) fn new() -> io::Result<Self> {
        let socket = UdpSocket::bind(("127.0.0.1", 0))?;
        socket.connect(socket.local_addr()?)?;
        socket.set_nonblocking(true)?;
        Ok(Self { parked: AtomicBool::new(false), socket })
    }

    /// End the engine's wait, if it waits. One load when it does not.
    #[inline]
    pub fn wake(&self) {
        if self.parked.load(Ordering::SeqCst) && self.parked.swap(false, Ordering::SeqCst) {
            let _ = self.socket.send(&[0]);
        }
    }

    #[cfg(unix)]
    fn handle(&self) -> RawHandle {
        std::os::fd::AsRawFd::as_raw_fd(&self.socket)
    }

    #[cfg(windows)]
    fn handle(&self) -> RawHandle {
        std::os::windows::io::AsRawSocket::as_raw_socket(&self.socket)
    }

    fn drain(&self) {
        let mut byte = [0u8; 8];
        while self.socket.recv(&mut byte).is_ok() {}
    }
}

/// The sender of the engine's command channel. A command sent while the
/// engine waits wakes it (ibx#530).
#[derive(Clone)]
pub struct ControlSender {
    tx: Sender<ControlCommand>,
    waker: Option<Arc<Waker>>,
}

impl ControlSender {
    pub(crate) fn new(tx: Sender<ControlCommand>, waker: Option<Arc<Waker>>) -> Self {
        Self { tx, waker }
    }

    /// As `Sender::send`.
    #[inline]
    pub fn send(&self, cmd: ControlCommand) -> Result<(), SendError<ControlCommand>> {
        let sent = self.tx.send(cmd);
        self.wake();
        sent
    }

    /// As `Sender::try_send`.
    #[inline]
    pub fn try_send(&self, cmd: ControlCommand) -> Result<(), TrySendError<ControlCommand>> {
        let sent = self.tx.try_send(cmd);
        self.wake();
        sent
    }

    /// As `Sender::send_timeout`.
    pub fn send_timeout(
        &self, cmd: ControlCommand, timeout: std::time::Duration,
    ) -> Result<(), crossbeam_channel::SendTimeoutError<ControlCommand>> {
        let sent = self.tx.send_timeout(cmd, timeout);
        self.wake();
        sent
    }

    #[inline]
    fn wake(&self) {
        if let Some(waker) = &self.waker {
            waker.wake();
        }
    }
}

/// A sender with nothing to wake: for an engine that is not running its
/// loop on a thread (tests), or a channel read by something else.
impl From<Sender<ControlCommand>> for ControlSender {
    fn from(tx: Sender<ControlCommand>) -> Self {
        Self { tx, waker: None }
    }
}

/// The engine's side of the wait.
pub(crate) struct Parker {
    waker: Option<Arc<Waker>>,
    fds: Vec<sys::PollFd>,
}

impl Parker {
    pub(crate) fn new(waker: Option<Arc<Waker>>) -> Self {
        Self { waker, fds: Vec::with_capacity(8) }
    }

    /// Wait until one of `handles` is readable, a command is sent, or
    /// `timeout_ms` passed. `pending` is asked once the senders can see the
    /// engine as waiting: a command sent before that is seen by it, one sent
    /// after it wakes the engine, so none is slept on.
    pub(crate) fn park(&mut self, handles: &[RawHandle], timeout_ms: i32, pending: impl FnOnce() -> bool) {
        self.fds.clear();
        self.fds.extend(handles.iter().map(|&h| sys::entry(h)));
        let Some(waker) = &self.waker else {
            // No waker: a command waits for the timeout at most.
            if !self.fds.is_empty() && !pending() {
                sys::wait(&mut self.fds, timeout_ms);
            }
            return;
        };
        self.fds.push(sys::entry(waker.handle()));
        waker.parked.store(true, Ordering::SeqCst);
        if !pending() {
            sys::wait(&mut self.fds, timeout_ms);
        }
        waker.parked.store(false, Ordering::SeqCst);
        if self.fds.last().is_some_and(|fd| fd.revents & sys::READABLE != 0) {
            waker.drain();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn parker() -> (Parker, ControlSender, crossbeam_channel::Receiver<ControlCommand>) {
        let waker = Arc::new(Waker::new().unwrap());
        let (tx, rx) = crossbeam_channel::bounded(8);
        (Parker::new(Some(waker.clone())), ControlSender::new(tx, Some(waker)), rx)
    }

    #[test]
    fn a_command_ends_the_wait_at_once() {
        let (mut parker, tx, rx) = parker();
        let sender = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            tx.send(ControlCommand::Shutdown).unwrap();
        });
        let start = Instant::now();
        parker.park(&[], 5_000, || !rx.is_empty());
        assert!(start.elapsed() < Duration::from_secs(2), "woken by the command, not by the timeout");
        assert!(matches!(rx.try_recv(), Ok(ControlCommand::Shutdown)));
        sender.join().unwrap();
    }

    #[test]
    fn a_command_sent_before_the_wait_is_not_slept_on() {
        let (mut parker, tx, rx) = parker();
        tx.send(ControlCommand::Shutdown).unwrap();
        let start = Instant::now();
        parker.park(&[], 5_000, || !rx.is_empty());
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn data_on_a_socket_ends_the_wait() {
        let (mut parker, _tx, rx) = parker();
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut peer, _) = listener.accept().unwrap();
        #[cfg(unix)]
        let handle = std::os::fd::AsRawFd::as_raw_fd(&client);
        #[cfg(windows)]
        let handle = std::os::windows::io::AsRawSocket::as_raw_socket(&client);
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            std::io::Write::write_all(&mut peer, b"x").unwrap();
            peer
        });
        let start = Instant::now();
        parker.park(&[handle], 5_000, || !rx.is_empty());
        assert!(start.elapsed() < Duration::from_secs(2), "woken by the data, not by the timeout");
        writer.join().unwrap();
    }

    #[test]
    fn the_timeout_ends_a_wait_with_nothing_to_do() {
        let (mut parker, _tx, rx) = parker();
        let start = Instant::now();
        parker.park(&[], 20, || !rx.is_empty());
        assert!(start.elapsed() >= Duration::from_millis(10));
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn a_wake_with_no_wait_sends_nothing() {
        let (mut parker, tx, rx) = parker();
        // Not waiting: the command is queued and no byte is left behind to
        // cut the next wait short.
        tx.send(ControlCommand::Shutdown).unwrap();
        let _ = rx.try_recv();
        let start = Instant::now();
        parker.park(&[], 30, || !rx.is_empty());
        assert!(start.elapsed() >= Duration::from_millis(15));
    }
}
