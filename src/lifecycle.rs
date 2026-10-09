//! Explicit bounded paper-login control. Address resolution and hardware discovery
//! are supplied by the caller, so no uninterruptible resolver/subprocess is hidden.
use std::{
    collections::BTreeMap,
    io,
    net::{IpAddr, Shutdown, SocketAddr, TcpStream},
    sync::{Arc, Condvar, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const CONNECT_SLICE: Duration = Duration::from_millis(100);
/// Controlled blocking I/O wakes at least once per this poll interval.
pub const CONTROLLED_IO_POLL: Duration = Duration::from_millis(100);
/// Maximum retained login-stage buffer and aggregate input per ControlledIo.
/// Raw farm token/SRP helpers separately cap frames and exchange counts.
pub const LOGIN_BYTES: usize = 4 * 1024 * 1024;
/// Maximum individual NS/XYZ payload accepted during controlled login.
pub const LOGIN_FRAME_BYTES: usize = 256 * 1024;

struct State {
    cancelled: bool,
    login_claimed: bool,
    deadline: Option<Instant>,
    sockets: Vec<TcpStream>,
}
struct Signals {
    state: Mutex<State>,
    wake: Condvar,
}
struct Owner {
    signals: Arc<Signals>,
    addresses: BTreeMap<String, Vec<IpAddr>>,
    hw_info: String,
    watcher: Mutex<Option<JoinHandle<()>>>,
    workers: Mutex<Vec<JoinHandle<()>>>,
    failure: Mutex<Option<io::ErrorKind>>,
}
fn stop(signals: &Signals) {
    let mut state = signals.state.lock().unwrap();
    state.cancelled = true;
    signals.wake.notify_all();
}
impl Drop for Owner {
    fn drop(&mut self) {
        stop(&self.signals);
        let watcher = self.watcher.get_mut().unwrap().take();
        let workers = std::mem::take(self.workers.get_mut().unwrap());
        // Drop only signals cleanup. Explicit join_workers is the confirmation
        // path. The reaper owns every handle and never opens a socket or retries.
        if watcher.is_some() || !workers.is_empty() {
            let _ = thread::Builder::new()
                .name("ibx-cleanup-reaper".into())
                .spawn(move || {
                    if let Some(watcher) = watcher {
                        let _ = watcher.join();
                    }
                    for worker in workers {
                        let _ = worker.join();
                    }
                });
        }
    }
}

/// Shared cancellation/deadline and abort ownership for one physical session.
/// Clones do not open connections. Unknown hosts fail before TCP; callers provide
/// trusted pre-resolved addresses for authentication, redirects and both farms.
/// No runtime DNS, hardware probing, process launch or autonomous retry is used.
#[derive(Clone)]
pub struct ConnectionControl {
    owner: Arc<Owner>,
}
impl ConnectionControl {
    /// Create one controlled attempt with an explicit positive login budget.
    /// Hardware information must be prepared outside native login; never log it.
    pub fn new(
        timeout: Duration,
        addresses: BTreeMap<String, Vec<IpAddr>>,
        hw_info: String,
    ) -> io::Result<Self> {
        if timeout.is_zero()
            || addresses.is_empty()
            || addresses.len() > 32
            || hw_info.is_empty()
            || hw_info.len() > 1024
            || addresses.iter().any(|(host, ips)| {
                host.is_empty() || host.len() > 253 || ips.is_empty() || ips.len() > 8
            })
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "positive deadline, bounded explicit addresses and hardware information required",
            ));
        }
        let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "login deadline overflow")
        })?;
        let signals = Arc::new(Signals {
            state: Mutex::new(State {
                cancelled: false,
                login_claimed: false,
                deadline: Some(deadline),
                sockets: Vec::new(),
            }),
            wake: Condvar::new(),
        });
        let watched = signals.clone();
        let watcher = thread::Builder::new()
            .name("ibx-login-deadline".into())
            .spawn(move || {
                let mut state = watched.state.lock().unwrap();
                loop {
                    if state.cancelled {
                        // Abort outside the state mutex on this owned thread. In
                        // Windows a shutdown may wait for a pending recv to poll;
                        // cancellation/Drop and admission checks must not wait too.
                        let sockets = std::mem::take(&mut state.sockets);
                        drop(state);
                        for socket in sockets {
                            let _ = socket.shutdown(Shutdown::Both);
                        }
                        return;
                    }
                    match state.deadline {
                        Some(deadline) if Instant::now() >= deadline => {
                            state.cancelled = true;
                            watched.wake.notify_all();
                            continue;
                        }
                        Some(deadline) => {
                            state = watched
                                .wake
                                .wait_timeout(
                                    state,
                                    deadline.saturating_duration_since(Instant::now()),
                                )
                                .unwrap()
                                .0;
                        }
                        None => {
                            state = watched.wake.wait(state).unwrap();
                        }
                    }
                }
            })?;
        Ok(Self {
            owner: Arc::new(Owner {
                signals,
                addresses,
                hw_info,
                watcher: Mutex::new(Some(watcher)),
                workers: Mutex::new(Vec::new()),
                failure: Mutex::new(None),
            }),
        })
    }
    /// Check cancellation/deadline before entering or publishing native work.
    pub fn check(&self) -> io::Result<()> {
        let state = self.owner.signals.state.lock().unwrap();
        if state.cancelled
            || state
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "controlled connection cancelled or deadline elapsed",
            ));
        }
        Ok(())
    }
    /// Whether cancellation or the login deadline has been observed.
    pub fn is_cancelled(&self) -> bool {
        self.check().is_err()
    }
    /// Signal stop immediately without queue admission or blocking socket work.
    /// The owned watchdog aborts sockets; checked joins confirm disposal.
    pub fn cancel(&self) {
        stop(&self.owner.signals);
    }
    /// Disarm the login deadline only after successful setup; cancellation persists.
    pub fn finish_login(&self) -> io::Result<()> {
        let mut state = self.owner.signals.state.lock().unwrap();
        if state.cancelled
            || state
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "login completed after cancellation or deadline",
            ));
        }
        state.deadline = None;
        self.owner.signals.wake.notify_all();
        Ok(())
    }
    /// Claim this scope for exactly one physical login before TCP effects.
    pub(crate) fn begin_login(&self) -> io::Result<()> {
        let mut state = self.owner.signals.state.lock().unwrap();
        if state.cancelled
            || state.login_claimed
            || state.deadline.is_none()
            || state
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "connection control is expired or already used",
            ));
        }
        state.login_claimed = true;
        Ok(())
    }
    pub(crate) fn record_cleanup_failure(&self, kind: io::ErrorKind) {
        self.owner.failure.lock().unwrap().get_or_insert(kind);
    }
    pub(crate) fn hardware_info(&self) -> &str {
        &self.owner.hw_info
    }
    /// Validate locally against bundled Mozilla roots. No platform verifier,
    /// certificate fetching, native root discovery or global provider mutation.
    pub(crate) fn connect_tls(
        &self,
        host: &str,
        socket: TcpStream,
    ) -> io::Result<Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>> {
        self.check()?;
        let roots =
            rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "controlled TLS versions unavailable",
            )
        })?
        .with_root_certificates(roots)
        .with_no_client_auth();
        self.connect_tls_with_config(host, socket, config)
    }
    fn connect_tls_with_config(
        &self,
        host: &str,
        socket: TcpStream,
        config: rustls::ClientConfig,
    ) -> io::Result<Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>> {
        let name = rustls::pki_types::ServerName::try_from(host.to_owned()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid controlled TLS server name",
            )
        })?;
        let connection = rustls::ClientConnection::new(Arc::new(config), name).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "controlled TLS configuration rejected",
            )
        })?;
        let mut stream = Box::new(rustls::StreamOwned::new(connection, socket));
        while stream.conn.is_handshaking() {
            self.check()?;
            match stream.conn.complete_io(&mut stream.sock) {
                Ok(_) => {}
                Err(error)
                    if error.kind() == io::ErrorKind::TimedOut
                        || error.kind() == io::ErrorKind::WouldBlock =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            }
        }
        self.check()?;
        Ok(stream)
    }
    /// Establish a TCP socket from caller-provided addresses only. Each connect
    /// is capped at 100ms and checked again before the socket enters authentication.
    pub(crate) fn connect_tcp(&self, host: &str, port: u16) -> io::Result<TcpStream> {
        self.check()?;
        let ips = self.owner.addresses.get(host).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "controlled login has no pre-resolved address for routed host",
            )
        })?;
        let mut last = io::Error::new(
            io::ErrorKind::NotConnected,
            "controlled TCP connection failed",
        );
        for ip in ips {
            self.check()?;
            let timeout = {
                let state = self.owner.signals.state.lock().unwrap();
                state
                    .deadline
                    .map(|deadline| {
                        deadline
                            .saturating_duration_since(Instant::now())
                            .min(CONNECT_SLICE)
                    })
                    .unwrap_or(CONNECT_SLICE)
            };
            if timeout.is_zero() {
                self.cancel();
                self.check()?;
            }
            match TcpStream::connect_timeout(&SocketAddr::new(*ip, port), timeout) {
                Ok(socket) => {
                    // Windows shutdown on a clone can wait for an outstanding
                    // synchronous recv. Finite I/O polling is the stop bound.
                    socket.set_read_timeout(Some(CONTROLLED_IO_POLL))?;
                    socket.set_write_timeout(Some(CONTROLLED_IO_POLL))?;
                    let clone = socket.try_clone()?;
                    let mut state = self.owner.signals.state.lock().unwrap();
                    if state.cancelled
                        || state
                            .deadline
                            .is_some_and(|deadline| Instant::now() >= deadline)
                    {
                        let _ = socket.shutdown(Shutdown::Both);
                        return Err(io::Error::new(
                            io::ErrorKind::Interrupted,
                            "TCP completed after cancellation",
                        ));
                    }
                    if state.sockets.len() >= 16 {
                        return Err(io::Error::new(
                            io::ErrorKind::OutOfMemory,
                            "controlled socket bound exceeded",
                        ));
                    }
                    state.sockets.push(clone);
                    return Ok(socket);
                }
                Err(error) => last = error,
            }
        }
        self.check()?;
        Err(last)
    }
    /// Retain an unfinished engine handle after best-effort client drop.
    pub(crate) fn retain_worker(&self, handle: JoinHandle<()>) {
        self.owner.workers.lock().unwrap().push(handle);
    }
    /// Confirm retained workers and the socket watchdog are joined. Timeout keeps
    /// ownership for a later check; an observed panic remains a permanent failure.
    pub fn join_workers(&self, timeout: Duration) -> io::Result<()> {
        let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "cleanup deadline overflow")
        })?;
        self.cancel();
        loop {
            let mut workers = self.owner.workers.lock().unwrap();
            let mut index = 0;
            while index < workers.len() {
                if workers[index].is_finished() {
                    if workers.swap_remove(index).join().is_err() {
                        *self.owner.failure.lock().unwrap() = Some(io::ErrorKind::Other);
                    }
                } else {
                    index += 1;
                }
            }
            let mut watcher = self.owner.watcher.lock().unwrap();
            if watcher.as_ref().is_some_and(|handle| handle.is_finished()) {
                if watcher.take().unwrap().join().is_err() {
                    *self.owner.failure.lock().unwrap() = Some(io::ErrorKind::Other);
                }
            }
            if workers.is_empty() && watcher.is_none() {
                self.owner.signals.state.lock().unwrap().sockets.clear();
                return match *self.owner.failure.lock().unwrap() {
                    Some(kind) => Err(io::Error::new(kind, "controlled cleanup worker panicked")),
                    None => Ok(()),
                };
            }
            drop(watcher);
            drop(workers);
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "controlled cleanup remains unconfirmed",
                ));
            }
            thread::sleep(Duration::from_millis(1));
        }
    }
}

/// Login-only read/write wrapper: checks cancellation and caps aggregate inbound
/// bytes before buffering. Nested frame parsers must also reject announced sizes
/// before allocation; this wrapper alone cannot supply that guarantee.
pub(crate) struct ControlledIo<S> {
    pub(crate) stream: S,
    control: Option<ConnectionControl>,
    received: usize,
}
impl<S> ControlledIo<S> {
    /// std::Read/Write helpers retry Interrupted, so scope cancellation must be
    /// terminal at the I/O boundary. Public control.check retains its stop result.
    fn check_transport_control(&self) -> io::Result<()> {
        if let Some(control) = &self.control {
            control.check().map_err(|error| {
                if error.kind() == io::ErrorKind::Interrupted {
                    io::Error::new(
                        io::ErrorKind::ConnectionAborted,
                        "controlled login transport stopped",
                    )
                } else {
                    error
                }
            })?;
        }
        Ok(())
    }
    pub(crate) fn new(stream: S, control: Option<&ConnectionControl>) -> Self {
        Self {
            stream,
            control: control.cloned(),
            received: 0,
        }
    }
}
impl<S: io::Read> ControlledIo<S> {
    /// One poll for idle-gap drains. A timeout remains visible to the caller;
    /// frame-oriented Read retries below preserve partial read_exact progress.
    pub(crate) fn read_poll(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.check_transport_control()?;
        if buf.is_empty() {
            return Ok(0);
        }
        let allowance = if self.control.is_some() {
            LOGIN_BYTES.saturating_sub(self.received)
        } else {
            buf.len()
        };
        if allowance == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "controlled login response byte bound exceeded",
            ));
        }
        let capacity = buf.len().min(allowance);
        let result = self.stream.read(&mut buf[..capacity]);
        if let Ok(amount) = result {
            self.received += amount;
        }
        self.check_transport_control()?;
        result
    }
}
impl<S: io::Read> io::Read for ControlledIo<S> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.read_poll(buf) {
                Err(error)
                    if self.control.is_some()
                        && (error.kind() == io::ErrorKind::TimedOut
                            || error.kind() == io::ErrorKind::WouldBlock) =>
                {
                    continue;
                }
                result => return result,
            }
        }
    }
}
impl<S: io::Write> io::Write for ControlledIo<S> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.check_transport_control()?;
        let amount = self.stream.write(buf)?;
        self.check_transport_control()?;
        Ok(amount)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.check_transport_control()?;
        let result = self.stream.flush();
        self.check_transport_control()?;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    fn control(timeout: Duration) -> ConnectionControl {
        ConnectionControl::new(
            timeout,
            BTreeMap::from([("localhost".into(), vec![IpAddr::from([127, 0, 0, 1])])]),
            "offline-hardware".into(),
        )
        .unwrap()
    }
    #[test]
    fn cancelled_and_unknown_hosts_never_open_sockets() {
        let control = control(Duration::from_secs(1));
        assert_eq!(
            control.connect_tcp("unknown", 1).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
        control.cancel();
        assert_eq!(
            control.connect_tcp("localhost", 1).unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
        control.join_workers(Duration::from_secs(1)).unwrap();
    }
    #[test]
    fn deadline_closes_stalled_raw_socket_and_prevents_late_success() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let control = control(Duration::from_secs(2));
        let mut socket = control
            .connect_tcp("localhost", listener.local_addr().unwrap().port())
            .unwrap();
        // Arm the short deadline after accept, so Windows cannot remove a
        // cancelled connection from the pending accept queue before the fixture
        // accepts it. Never let a failed watchdog leave this fixture blocked.
        listener.set_nonblocking(true).unwrap();
        let accept_deadline = Instant::now() + Duration::from_secs(1);
        let _peer = loop {
            match listener.accept() {
                Ok((peer, _)) => break peer,
                Err(error)
                    if error.kind() == io::ErrorKind::WouldBlock
                        && Instant::now() < accept_deadline =>
                {
                    thread::sleep(Duration::from_millis(1))
                }
                Err(error) => panic!("offline accept failed: {error}"),
            }
        };
        assert_eq!(socket.read_timeout().unwrap(), Some(CONTROLLED_IO_POLL));
        control.owner.signals.state.lock().unwrap().deadline =
            Some(Instant::now() + Duration::from_millis(40));
        control.owner.signals.wake.notify_all();
        let mut byte = [0];
        let stopped = Instant::now();
        let result = socket.read(&mut byte);
        assert!(result.is_err() || result.unwrap() == 0);
        assert!(control.finish_login().is_err());
        assert!(
            stopped.elapsed() < Duration::from_millis(500),
            "raw cancellation did not wake promptly"
        );
        control.join_workers(Duration::from_secs(1)).unwrap();
    }
    #[test]
    fn successful_disarm_retains_explicit_socket_cancellation() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let control = control(Duration::from_secs(1));
        let mut socket = control
            .connect_tcp("localhost", listener.local_addr().unwrap().port())
            .unwrap();
        let (_peer, _) = listener.accept().unwrap();
        control.finish_login().unwrap();
        control.cancel();
        let mut byte = [0];
        let result = socket.read(&mut byte);
        assert!(result.is_err() || result.unwrap() == 0);
        control.join_workers(Duration::from_secs(1)).unwrap();
    }
    #[test]
    fn cleanup_timeout_retains_handle_and_later_join_is_honest() {
        let control = control(Duration::from_secs(1));
        let (tx, rx) = std::sync::mpsc::channel();
        control.retain_worker(thread::spawn(move || {
            rx.recv().unwrap();
        }));
        assert_eq!(
            control.join_workers(Duration::ZERO).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert_eq!(control.owner.workers.lock().unwrap().len(), 1);
        tx.send(()).unwrap();
        control.join_workers(Duration::from_secs(1)).unwrap();
        control.join_workers(Duration::from_secs(1)).unwrap();
    }
}

#[cfg(test)]
mod controlled_tls_tests {
    use super::*;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    use std::io::{Read, Write};

    const CA: &[u8] = include_bytes!("testdata/controlled-tls/ca.der");
    const CERT: &[u8] = include_bytes!("testdata/controlled-tls/server.der");
    const KEY: &[u8] = include_bytes!("testdata/controlled-tls/server-key.der");

    fn control() -> ConnectionControl {
        ConnectionControl::new(
            Duration::from_secs(3),
            BTreeMap::from([("localhost".into(), vec![IpAddr::from([127, 0, 0, 1])])]),
            "offline-hardware".into(),
        )
        .unwrap()
    }
    fn client_config() -> rustls::ClientConfig {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(CertificateDer::from(CA.to_vec())).unwrap();
        rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth()
    }
    fn accept(listener: &std::net::TcpListener) -> TcpStream {
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match listener.accept() {
                Ok((peer, _)) => {
                    peer.set_nonblocking(false).unwrap();
                    return peer;
                }
                Err(error)
                    if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline =>
                {
                    thread::sleep(Duration::from_millis(1))
                }
                Err(error) => panic!("loopback accept failed: {error}"),
            }
        }
    }
    fn server() -> (u16, JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(CERT.to_vec())],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(KEY.to_vec())),
        )
        .unwrap();
        let handle = thread::spawn(move || {
            let socket = accept(&listener);
            socket
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            socket
                .set_write_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let connection = rustls::ServerConnection::new(Arc::new(config)).unwrap();
            let mut stream = rustls::StreamOwned::new(connection, socket);
            while stream.conn.is_handshaking() {
                if stream.conn.complete_io(&mut stream.sock).is_err() {
                    return;
                }
            }
            let mut request = [0];
            if stream.read_exact(&mut request).is_err() {
                return;
            }
            assert_eq!(request, *b"x");
            stream.write_all(b"ready").unwrap();
            stream.flush().unwrap();
            let mut end = [0];
            assert!(matches!(stream.read(&mut end), Ok(0) | Err(_)));
        });
        (port, handle)
    }
    #[test]
    fn controlled_tls_verifies_local_chain_and_retains_socket_stop() {
        let (port, peer) = server();
        let control = control();
        let socket = control.connect_tcp("localhost", port).unwrap();
        let mut stream = control
            .connect_tls_with_config("localhost", socket, client_config())
            .unwrap();
        control.finish_login().unwrap();
        stream.write_all(b"x").unwrap();
        stream.flush().unwrap();
        let mut ready = [0; 5];
        stream.read_exact(&mut ready).unwrap();
        assert_eq!(&ready, b"ready");
        control.cancel();
        drop(stream);
        peer.join().unwrap();
        control.join_workers(Duration::from_secs(1)).unwrap();
    }
    #[test]
    fn controlled_tls_rejects_untrusted_certificate_and_wrong_hostname() {
        for trusted_wrong_name in [false, true] {
            let (port, peer) = server();
            let control = control();
            let socket = control.connect_tcp("localhost", port).unwrap();
            let result = if trusted_wrong_name {
                control.connect_tls_with_config("wrong.invalid", socket, client_config())
            } else {
                control.connect_tls("localhost", socket)
            };
            assert!(result.is_err());
            control.cancel();
            peer.join().unwrap();
            control.join_workers(Duration::from_secs(1)).unwrap();
        }
    }
    #[test]
    fn controlled_tls_cancel_and_deadline_wake_stalled_handshake_promptly() {
        for expire in [false, true] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let control = control();
            let socket = control
                .connect_tcp("localhost", listener.local_addr().unwrap().port())
                .unwrap();
            let mut peer = accept(&listener);
            peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
            let active = control.clone();
            let (tx, rx) = std::sync::mpsc::channel();
            let worker = thread::spawn(move || {
                tx.send(active.connect_tls("localhost", socket).map(|_| ()))
                    .unwrap();
            });
            let mut hello = [0; 1];
            peer.read_exact(&mut hello).unwrap();
            let start = Instant::now();
            if expire {
                control.owner.signals.state.lock().unwrap().deadline =
                    Some(Instant::now() + Duration::from_millis(40));
                control.owner.signals.wake.notify_all();
            } else {
                control.cancel();
            }
            assert!(rx.recv_timeout(Duration::from_secs(1)).unwrap().is_err());
            worker.join().unwrap();
            assert!(
                start.elapsed() < Duration::from_millis(500),
                "controlled TLS stop did not wake promptly"
            );
            assert!(control.finish_login().is_err());
            control.join_workers(Duration::from_secs(1)).unwrap();
        }
    }
    #[test]
    fn controlled_io_keeps_partial_reads_across_poll_timeouts() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let control = control();
        let socket = control
            .connect_tcp("localhost", listener.local_addr().unwrap().port())
            .unwrap();
        let mut peer = accept(&listener);
        let active = control.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            let mut stream = ControlledIo::new(socket, Some(&active));
            let mut pair = [0; 2];
            let result = stream.read_exact(&mut pair).map(|_| pair);
            tx.send(result).unwrap();
        });
        peer.write_all(b"a").unwrap();
        thread::sleep(Duration::from_millis(150));
        peer.write_all(b"b").unwrap();
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap().unwrap(),
            *b"ab"
        );
        worker.join().unwrap();
        control.cancel();
        control.join_workers(Duration::from_secs(1)).unwrap();
    }
}

#[cfg(test)]
mod controlled_poll_tests {
    use super::*;
    use std::io::{Read, Write};
    fn control() -> ConnectionControl {
        ConnectionControl::new(
            Duration::from_secs(3),
            BTreeMap::from([("localhost".into(), vec![IpAddr::from([127, 0, 0, 1])])]),
            "offline-hardware".into(),
        )
        .unwrap()
    }
    fn pair(control: &ConnectionControl) -> (TcpStream, TcpStream) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let socket = control
            .connect_tcp("localhost", listener.local_addr().unwrap().port())
            .unwrap();
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        let peer = loop {
            match listener.accept() {
                Ok((peer, _)) => break peer,
                Err(error)
                    if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline =>
                {
                    thread::sleep(Duration::from_millis(1))
                }
                Err(error) => panic!("loopback accept failed: {error}"),
            }
        };
        peer.set_nonblocking(false).unwrap();
        (socket, peer)
    }
    #[test]
    fn controlled_poll_preserves_idle_gap_without_cancelling_session() {
        let control = control();
        let (socket, mut peer) = pair(&control);
        let mut stream = ControlledIo::new(socket, Some(&control));
        peer.write_all(b"init").unwrap();
        let mut bytes = [0; 4];
        assert_eq!(stream.read_poll(&mut bytes).unwrap(), 4);
        let start = Instant::now();
        let error = stream.read_poll(&mut bytes).unwrap_err();
        assert!(matches!(
            error.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
        ));
        assert!(start.elapsed() < Duration::from_millis(500));
        assert!(!control.is_cancelled());
        control.cancel();
        drop(stream);
        control.join_workers(Duration::from_secs(1)).unwrap();
    }
    #[tokio::test(flavor = "current_thread")]
    async fn controlled_drop_with_pending_socket_read_does_not_block_executor() {
        let control = control();
        let (mut socket, _peer) = pair(&control);
        control.finish_login().unwrap();
        let active = control.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            ready_tx.send(()).unwrap();
            while !active.is_cancelled() {
                let _ = socket.read(&mut [0; 1]);
            }
        });
        let (tx, _rx) = crossbeam_channel::bounded(1);
        let client = crate::api::EClient::from_parts_controlled(
            Arc::new(crate::bridge::SharedState::new()),
            tx,
            worker,
            "DU-offline".into(),
            control.clone(),
        );
        ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        let start = Instant::now();
        drop(client);
        assert!(
            start.elapsed() < Duration::from_millis(50),
            "controlled Drop waited for blocked native socket I/O"
        );
        tokio::time::timeout(
            Duration::from_millis(50),
            tokio::time::sleep(Duration::from_millis(1)),
        )
        .await
        .unwrap();
        control.join_workers(Duration::from_secs(1)).unwrap();
    }
}

#[cfg(test)]
mod controlled_terminal_io_tests {
    use super::*;
    use std::io::{Read, Write};
    struct NoEffects;
    impl Read for NoEffects {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            panic!("cancelled transport read underlying stream")
        }
    }
    impl Write for NoEffects {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            panic!("cancelled transport wrote underlying stream")
        }
        fn flush(&mut self) -> io::Result<()> {
            panic!("cancelled transport flushed underlying stream")
        }
    }
    #[test]
    fn controlled_cancelled_std_io_helpers_are_terminal_without_retry_or_effects() {
        let control = ConnectionControl::new(
            Duration::from_secs(1),
            BTreeMap::from([("localhost".into(), vec![IpAddr::from([127, 0, 0, 1])])]),
            "offline-hardware".into(),
        )
        .unwrap();
        control.cancel();
        assert_eq!(
            control.check().unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
        let active = control.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            let mut io = ControlledIo::new(NoEffects, Some(&active));
            let errors = [
                io.read_exact(&mut [0]).unwrap_err().kind(),
                io.read_to_end(&mut Vec::new()).unwrap_err().kind(),
                io.write_all(b"never").unwrap_err().kind(),
                io.flush().unwrap_err().kind(),
            ];
            tx.send(errors).unwrap();
        });
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            [io::ErrorKind::ConnectionAborted; 4]
        );
        worker.join().unwrap();
        control.join_workers(Duration::from_secs(1)).unwrap();
    }
}
