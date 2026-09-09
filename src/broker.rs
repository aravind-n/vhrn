//! Host-only relay for explicitly granted loopback services.

#![allow(dead_code)] // Run lifecycle wiring follows in the next ship step.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use subtle::ConstantTimeEq;
use vhrn_policy::{BrokerRequest, BrokerToken, MAX_BROKER_FRAME_SIZE, parse_broker_request};

use crate::net::{LoopbackAuthority, PolicyStore, ProjectIdentity};

const HANDSHAKE_DEADLINE: Duration = Duration::from_secs(3);
const CONNECT_DEADLINE: Duration = Duration::from_secs(10);
// The broker runs on the host, outside the container resource limits. Bound each run's
// connections so an untrusted container cannot turn incomplete handshakes into host threads.
const MAX_CONNECTIONS: usize = 128;

#[derive(Clone)]
pub(crate) struct BrokerCleanup(Arc<BrokerState>);
impl BrokerCleanup {
    pub(crate) fn run(&self) {
        self.0.stop();
    }
}

pub(crate) struct Broker {
    state: Arc<BrokerState>,
    address: SocketAddr,
    secret_dir: PathBuf,
    args: Mutex<Option<AcceptArgs>>,
}

struct AcceptArgs {
    root: PathBuf,
    project: ProjectIdentity,
    run_id: String,
    token: BrokerToken,
}
struct Lifecycle {
    listener: Option<TcpListener>,
    accept: Option<JoinHandle<()>>,
}
struct Ready {
    ready: bool,
    stopped: bool,
}
struct BrokerState {
    stopped: AtomicBool,
    lifecycle: Mutex<Lifecycle>,
    workers: Mutex<Vec<JoinHandle<()>>>,
    sockets: Mutex<HashMap<u64, TcpStream>>,
    next_socket: AtomicU64,
    secret_dir: PathBuf,
    ready: (Mutex<Ready>, Condvar),
    permits: (Mutex<usize>, Condvar),
}

struct ConnectionPermit {
    state: Arc<BrokerState>,
}
impl Drop for ConnectionPermit {
    fn drop(&mut self) {
        let mut available = lock(&self.state.permits.0);
        *available += 1;
        self.state.permits.1.notify_one();
    }
}

struct SocketRegistration {
    state: Arc<BrokerState>,
    id: u64,
}
impl Drop for SocketRegistration {
    fn drop(&mut self) {
        lock(&self.state.sockets).remove(&self.id);
    }
}

struct ClientConnection {
    stream: TcpStream,
    _permit: ConnectionPermit,
    _registration: SocketRegistration,
}

impl BrokerState {
    fn try_acquire(self: &Arc<Self>) -> Option<ConnectionPermit> {
        let mut available = lock(&self.permits.0);
        if *available == 0 || self.stopped.load(Ordering::Acquire) {
            return None;
        }
        *available -= 1;
        Some(ConnectionPermit {
            state: Arc::clone(self),
        })
    }

    fn register(self: &Arc<Self>, stream: &TcpStream) -> io::Result<SocketRegistration> {
        let copy = stream.try_clone()?;
        let mut sockets = lock(&self.sockets);
        if self.stopped.load(Ordering::Acquire) {
            drop(sockets);
            let _ = copy.shutdown(Shutdown::Both);
            return Err(io::Error::new(io::ErrorKind::Interrupted, "broker stopped"));
        }
        let id = self.next_socket.fetch_add(1, Ordering::Relaxed);
        sockets.insert(id, copy);
        Ok(SocketRegistration {
            state: Arc::clone(self),
            id,
        })
    }

    fn reap_workers(&self) {
        let done = {
            let mut workers = lock(&self.workers);
            let all = std::mem::take(&mut *workers);
            let (done, live): (Vec<_>, Vec<_>) = all.into_iter().partition(JoinHandle::is_finished);
            *workers = live;
            done
        };
        for worker in done {
            let _ = worker.join();
        }
    }

    fn stop(&self) {
        let accept = {
            let mut lifecycle = lock(&self.lifecycle);
            if self.stopped.swap(true, Ordering::AcqRel) {
                return;
            }
            let _ = lifecycle.listener.take();
            lifecycle.accept.take()
        };
        {
            let mut ready = lock(&self.ready.0);
            ready.stopped = true;
            self.ready.1.notify_all();
        }
        let sockets = std::mem::take(&mut *lock(&self.sockets));
        for socket in sockets.into_values() {
            let _ = socket.shutdown(Shutdown::Both);
        }
        if let Some(thread) = accept {
            let _ = thread.join();
        }
        let workers = std::mem::take(&mut *lock(&self.workers));
        for worker in workers {
            let _ = worker.join();
        }
        let _ = fs::remove_dir_all(&self.secret_dir);
    }
}

impl Broker {
    pub(crate) fn bind(
        bind: SocketAddr,
        policy_root: &Path,
        project: ProjectIdentity,
        run_id: String,
        cache: &Path,
    ) -> Result<Self> {
        Self::bind_with_limit(bind, policy_root, project, run_id, cache, MAX_CONNECTIONS)
    }

    fn bind_with_limit(
        bind: SocketAddr,
        policy_root: &Path,
        project: ProjectIdentity,
        run_id: String,
        cache: &Path,
        connection_limit: usize,
    ) -> Result<Self> {
        assert!(
            connection_limit > 0,
            "broker connection limit must be nonzero"
        );
        let listener = TcpListener::bind(bind).context("binding loopback broker")?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let secret_dir = cache.join("broker").join(&run_id);
        fs::create_dir_all(secret_dir.parent().expect("broker secret has parent"))?;
        let token = create_secret(&secret_dir).map_err(anyhow::Error::from)?;
        let state = Arc::new(BrokerState {
            stopped: AtomicBool::new(false),
            lifecycle: Mutex::new(Lifecycle {
                listener: Some(listener),
                accept: None,
            }),
            workers: Mutex::new(Vec::new()),
            sockets: Mutex::new(HashMap::new()),
            next_socket: AtomicU64::new(0),
            secret_dir: secret_dir.clone(),
            ready: (
                Mutex::new(Ready {
                    ready: false,
                    stopped: false,
                }),
                Condvar::new(),
            ),
            permits: (Mutex::new(connection_limit), Condvar::new()),
        });
        Ok(Self {
            state,
            address,
            secret_dir,
            args: Mutex::new(Some(AcceptArgs {
                root: policy_root.to_path_buf(),
                project,
                run_id,
                token,
            })),
        })
    }
    pub(crate) fn cleanup_handle(&self) -> BrokerCleanup {
        BrokerCleanup(Arc::clone(&self.state))
    }
    pub(crate) fn address(&self) -> SocketAddr {
        self.address
    }
    pub(crate) fn token_file(&self) -> PathBuf {
        self.secret_dir.join("token")
    }
    pub(crate) fn start_accepting(&self) -> Result<()> {
        let args = lock(&self.args)
            .take()
            .ok_or_else(|| anyhow::anyhow!("broker already started"))?;
        let mut lifecycle = lock(&self.state.lifecycle);
        if self.state.stopped.load(Ordering::Acquire) {
            bail!("broker stopped");
        }
        let listener = lifecycle
            .listener
            .take()
            .ok_or_else(|| anyhow::anyhow!("broker listener unavailable"))?;
        let state = Arc::clone(&self.state);
        let thread = match thread::Builder::new()
            .name("vhrn-broker".into())
            .spawn(move || accept_loop(listener, state, args))
        {
            Ok(thread) => thread,
            Err(error) => {
                drop(lifecycle);
                self.state.stop();
                return Err(error.into());
            }
        };
        lifecycle.accept = Some(thread);
        Ok(())
    }
    pub(crate) fn wait_ready(&self, deadline: Instant) -> Result<()> {
        let mut ready = lock(&self.state.ready.0);
        while !ready.ready && !ready.stopped {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!("broker readiness timed out");
            }
            let (next, timeout) = self.state.ready.1.wait_timeout(ready, remaining).unwrap();
            ready = next;
            if timeout.timed_out() && !ready.ready {
                bail!("broker readiness timed out");
            }
        }
        if ready.stopped {
            bail!("broker stopped");
        }
        Ok(())
    }
}
impl Drop for Broker {
    fn drop(&mut self) {
        self.state.stop();
    }
}

fn create_secret(dir: &Path) -> io::Result<BrokerToken> {
    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700);
    builder.create(dir)?; // create_new semantics: never remove a competing run's directory.
    match random_token().and_then(|token| {
        crate::net::write_atomic(&dir.join("token"), token.as_bytes(), 0o444)?;
        Ok(token)
    }) {
        Ok(token) => Ok(token),
        Err(error) => {
            let _ = fs::remove_dir_all(dir);
            Err(error)
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
fn accept_loop(listener: TcpListener, state: Arc<BrokerState>, args: AcceptArgs) {
    while !state.stopped.load(Ordering::Acquire) {
        state.reap_workers();
        match listener.accept() {
            Ok((stream, _)) => {
                // Darwin inherits O_NONBLOCK from the listener. Handshake deadlines use socket
                // timeouts; relay copies must remain blocking after those timeouts are cleared.
                if stream.set_nonblocking(false).is_err() {
                    let _ = stream.shutdown(Shutdown::Both);
                    continue;
                }
                // Acquire before registration, cloning, or spawning so rejected clients cannot
                // consume host file descriptors or threads beyond the fixed per-run budget.
                let Some(permit) = state.try_acquire() else {
                    let _ = stream.shutdown(Shutdown::Both);
                    continue;
                };
                let Ok(registration) = state.register(&stream) else {
                    let _ = stream.shutdown(Shutdown::Both);
                    continue;
                };
                let worker_state = Arc::clone(&state);
                let store = PolicyStore::from_root(args.root.clone());
                let project = args.project.clone();
                let run_id = args.run_id.clone();
                let token = args.token.clone();
                let connection = ClientConnection {
                    stream,
                    _permit: permit,
                    _registration: registration,
                };
                let worker = thread::Builder::new()
                    .name("vhrn-broker-client".into())
                    .spawn(move || {
                        serve(connection, worker_state, store, project, run_id, token);
                    });
                if let Ok(worker) = worker {
                    lock(&state.workers).push(worker);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(_) => break,
        }
    }
    state.reap_workers();
}

#[allow(clippy::needless_pass_by_value)]
fn serve(
    connection: ClientConnection,
    state: Arc<BrokerState>,
    store: PolicyStore,
    project: ProjectIdentity,
    run_id: String,
    token: BrokerToken,
) {
    let mut client = connection.stream;
    let result = handshake(&mut client, &token).and_then(|request| match request {
        Request::Ready => {
            let mut ready = lock(&state.ready.0);
            ready.ready = true;
            state.ready.1.notify_all();
            Ok(None)
        }
        Request::Connect(authority) => {
            if !store.loopback_allows_live(&project, &run_id, &authority)? {
                bail!("denied");
            }
            let upstream = connect(&authority, &state)?;
            let registration = state.register(&upstream)?;
            if state.stopped.load(Ordering::Acquire) {
                let _ = upstream.shutdown(Shutdown::Both);
                bail!("broker stopped");
            }
            Ok(Some((upstream, registration)))
        }
    });
    match result {
        Ok(None) => {
            let _ = write_handshake(&mut client, b"OK\n");
        }
        Ok(Some((upstream, _upstream_registration))) => {
            if write_handshake(&mut client, b"OK\n").is_ok() {
                let _ = client.set_read_timeout(None);
                let _ = client.set_write_timeout(None);
                let _ = upstream.set_read_timeout(None);
                let _ = upstream.set_write_timeout(None);
                relay(client, upstream);
            }
        }
        Err(_) => {
            let _ = write_handshake(&mut client, b"ERR\n");
        }
    }
}
enum Request {
    Ready,
    Connect(LoopbackAuthority),
}
fn handshake(stream: &mut TcpStream, token: &BrokerToken) -> Result<Request> {
    let line = read_line(stream, Instant::now() + HANDSHAKE_DEADLINE)?;
    let request = parse_broker_request(&line).map_err(anyhow::Error::from)?;
    let request_token = match &request {
        BrokerRequest::Ready(token) | BrokerRequest::Connect { token, .. } => token,
    };
    if token.as_bytes().ct_eq(request_token.as_bytes()).unwrap_u8() != 1 {
        bail!("denied");
    }
    match request {
        BrokerRequest::Ready(_) => Ok(Request::Ready),
        BrokerRequest::Connect { authority, .. } => Ok(Request::Connect(authority)),
    }
}
fn read_line(stream: &mut TcpStream, deadline: Instant) -> Result<Vec<u8>> {
    let mut line = Vec::new();
    loop {
        let remain = deadline.saturating_duration_since(Instant::now());
        if remain.is_zero() || line.len() == MAX_BROKER_FRAME_SIZE {
            bail!("invalid handshake");
        }
        stream.set_read_timeout(Some(remain))?;
        let mut byte = [0];
        stream.read_exact(&mut byte)?;
        line.push(byte[0]);
        if byte[0] == b'\n' {
            return Ok(line);
        }
    }
}
fn write_handshake(stream: &mut TcpStream, response: &[u8]) -> io::Result<()> {
    stream.set_write_timeout(Some(HANDSHAKE_DEADLINE))?;
    stream.write_all(response)
}
fn connect(authority: &LoopbackAuthority, state: &Arc<BrokerState>) -> Result<TcpStream> {
    let text = authority.to_string();
    let (host, port) = text
        .rsplit_once(':')
        .ok_or_else(|| anyhow::anyhow!("bad authority"))?;
    let port: u16 = port.parse()?;
    let host = host.trim_matches(['[', ']']);
    let addresses = if host == "localhost" {
        vec![
            SocketAddr::from(([127, 0, 0, 1], port)),
            SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], port)),
        ]
    } else {
        vec![SocketAddr::new(host.parse::<IpAddr>()?, port)]
    };
    let deadline = Instant::now() + CONNECT_DEADLINE;
    for address in addresses {
        if state.stopped.load(Ordering::Acquire) {
            bail!("broker stopped");
        }
        let remain = deadline.saturating_duration_since(Instant::now());
        if remain.is_zero() {
            break;
        }
        if let Ok(stream) = TcpStream::connect_timeout(&address, remain) {
            return Ok(stream);
        }
    }
    bail!("loopback connection failed")
}
fn relay(mut client: TcpStream, mut upstream: TcpStream) {
    let Ok(mut client_copy) = client.try_clone() else {
        return;
    };
    let Ok(mut upstream_copy) = upstream.try_clone() else {
        return;
    };
    let Ok(forward) = thread::Builder::new()
        .name("vhrn-broker-forward".into())
        .spawn(move || {
            let _ = io::copy(&mut client_copy, &mut upstream_copy);
            let _ = upstream_copy.shutdown(Shutdown::Both);
            let _ = client_copy.shutdown(Shutdown::Both);
        })
    else {
        let _ = upstream.shutdown(Shutdown::Both);
        let _ = client.shutdown(Shutdown::Both);
        return;
    };
    let _ = io::copy(&mut upstream, &mut client);
    let _ = upstream.shutdown(Shutdown::Both);
    let _ = client.shutdown(Shutdown::Both);
    let _ = forward.join();
}
fn random_token() -> io::Result<BrokerToken> {
    let mut bytes = [0; 32];
    fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    BrokerToken::parse(hex::encode(bytes)).map_err(io::Error::other)
}
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use tempfile::TempDir;

    use super::*;

    fn broker() -> (TempDir, Broker, ProjectIdentity) {
        broker_with_limit(MAX_CONNECTIONS)
    }

    fn broker_with_limit(connection_limit: usize) -> (TempDir, Broker, ProjectIdentity) {
        let temp = TempDir::new().unwrap();
        let project = temp.path().join("project");
        fs::create_dir(&project).unwrap();
        let policy = temp.path().join("net");
        fs::create_dir(&policy).unwrap();
        let identity = ProjectIdentity::from_path(&project).unwrap();
        fs::write(policy.join("loopback.allow"), b"").unwrap();
        fs::create_dir_all(policy.join("projects").join(identity.key())).unwrap();
        fs::write(
            policy
                .join("projects")
                .join(identity.key())
                .join("loopback.allow"),
            b"",
        )
        .unwrap();
        fs::create_dir_all(policy.join("runs/run")).unwrap();
        fs::write(policy.join("runs/run/loopback.allow"), b"").unwrap();
        let broker = Broker::bind_with_limit(
            SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            &policy,
            identity.clone(),
            "run".into(),
            temp.path(),
            connection_limit,
        )
        .unwrap();
        broker.start_accepting().unwrap();
        (temp, broker, identity)
    }

    fn exchange(broker: &Broker, request: &str) -> String {
        let mut stream = TcpStream::connect(broker.address()).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(4)))
            .unwrap();
        let mut response = [0; 4];
        let count = stream.read(&mut response).unwrap();
        String::from_utf8(response[..count].to_vec()).unwrap()
    }

    #[test]
    fn ready_and_token_checks_need_no_policy_or_origin() {
        let (_temp, broker, _) = broker();
        let token = fs::read_to_string(broker.token_file()).unwrap();
        assert_eq!(
            exchange(&broker, &format!("VHRN-BROKER/1 READY {token}\n")),
            "OK\n"
        );
        assert_eq!(exchange(&broker, "VHRN-BROKER/1 READY other\n"), "ERR\n");
        broker
            .wait_ready(Instant::now() + Duration::from_secs(1))
            .unwrap();
    }

    #[test]
    fn connection_limit_rejects_excess_clients_and_recovers_after_disconnect() {
        let (_temp, broker, _) = broker_with_limit(1);
        let held = TcpStream::connect(broker.address()).unwrap();
        held.set_read_timeout(Some(Duration::from_secs(1))).unwrap();

        // The first incomplete handshake owns the only permit. The next accepted socket is
        // closed in the accept loop, without allocating another broker worker.
        let mut excess = TcpStream::connect(broker.address()).unwrap();
        excess
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut byte = [0];
        assert_eq!(excess.read(&mut byte).unwrap(), 0);
        assert_eq!(lock(&broker.state.workers).len(), 1);

        let _ = held.shutdown(Shutdown::Both);
        drop(held);
        let token = fs::read_to_string(broker.token_file()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if exchange(&broker, &format!("VHRN-BROKER/1 READY {token}\n")) == "OK\n" {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "connection permit did not recover"
            );
            thread::sleep(Duration::from_millis(10));
        }

        let started = Instant::now();
        broker.cleanup_handle().run();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(*lock(&broker.state.permits.0), 1);
    }

    #[test]
    fn cleanup_wakes_ready_waiter_and_removes_staging() {
        let (_temp, broker, _) = broker();
        let cleanup = broker.cleanup_handle();
        let state = Arc::clone(&broker.state);
        let waiter = thread::spawn(move || {
            let broker = Broker {
                state,
                address: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
                secret_dir: PathBuf::new(),
                args: Mutex::new(None),
            };
            broker
                .wait_ready(Instant::now() + Duration::from_secs(10))
                .is_err()
        });
        thread::sleep(Duration::from_millis(20));
        cleanup.run();
        assert!(waiter.join().unwrap());
        assert!(!broker.token_file().exists());
    }

    fn connect_line(broker: &Broker, authority: &str) -> String {
        format!(
            "VHRN-BROKER/1 CONNECT {} {authority}\n",
            fs::read_to_string(broker.token_file()).unwrap()
        )
    }

    #[test]
    fn relay_preserves_coalesced_data_and_survives_handshake_timeout() {
        let (temp, broker, _) = broker();
        let origin = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let authority = format!("127.0.0.1:{}", origin.local_addr().unwrap().port());
        fs::write(
            temp.path().join("net/loopback.allow"),
            format!("{authority}\n"),
        )
        .unwrap();
        let server = thread::spawn(move || {
            let (mut peer, _) = origin.accept().unwrap();
            let mut first = [0; 4];
            peer.read_exact(&mut first).unwrap();
            assert_eq!(&first, b"ping");
            peer.write_all(b"pong").unwrap();
            thread::sleep(HANDSHAKE_DEADLINE + Duration::from_millis(100));
            let mut second = [0; 4];
            peer.read_exact(&mut second).unwrap();
            assert_eq!(&second, b"next");
            peer.write_all(b"done").unwrap();
        });
        let mut client = TcpStream::connect(broker.address()).unwrap();
        client
            .write_all(format!("{}ping", connect_line(&broker, &authority)).as_bytes())
            .unwrap();
        thread::sleep(Duration::from_millis(20));
        let mut response = [0; 3];
        client.read_exact(&mut response).unwrap();
        assert_eq!(&response, b"OK\n");
        let mut body = [0; 4];
        client.read_exact(&mut body).unwrap();
        assert_eq!(&body, b"pong");
        thread::sleep(HANDSHAKE_DEADLINE + Duration::from_millis(100));
        client.write_all(b"next").unwrap();
        client.read_exact(&mut body).unwrap();
        assert_eq!(&body, b"done");
        server.join().unwrap();
        drop(client);
        thread::sleep(Duration::from_millis(20));
        broker.state.reap_workers();
        assert!(lock(&broker.state.sockets).is_empty());
        assert!(lock(&broker.state.workers).is_empty());
    }

    #[test]
    fn policy_files_are_live_and_fail_closed() {
        let (temp, broker, identity) = broker();
        let origin = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let authority = format!("127.0.0.1:{}", origin.local_addr().unwrap().port());
        let request = connect_line(&broker, &authority);
        let net = temp.path().join("net");
        fs::write(net.join("loopback.allow"), format!("{authority}\n")).unwrap();
        let project = net
            .join("projects")
            .join(identity.key())
            .join("loopback.allow");
        let run = net.join("runs/run/loopback.allow");
        fs::remove_file(&project).unwrap();
        assert_eq!(exchange(&broker, &request), "ERR\n");
        fs::write(&project, b"").unwrap();
        fs::write(&run, b"bad\n").unwrap();
        assert_eq!(exchange(&broker, &request), "ERR\n");
        fs::write(&run, b"").unwrap();
        let accepted = thread::spawn(move || origin.accept().unwrap().0);
        let mut client = TcpStream::connect(broker.address()).unwrap();
        client.write_all(request.as_bytes()).unwrap();
        let mut ok = [0; 3];
        client.read_exact(&mut ok).unwrap();
        assert_eq!(&ok, b"OK\n");
        drop(client);
        drop(accepted.join().unwrap());
    }

    #[test]
    fn cleanup_interrupts_active_relay_and_registrations_reap() {
        let (temp, broker, _) = broker();
        let origin = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let authority = format!("127.0.0.1:{}", origin.local_addr().unwrap().port());
        fs::write(
            temp.path().join("net/loopback.allow"),
            format!("{authority}\n"),
        )
        .unwrap();
        let server = thread::spawn(move || {
            let (mut peer, _) = origin.accept().unwrap();
            let mut b = [0];
            peer.read(&mut b).unwrap_or(0)
        });
        let mut client = TcpStream::connect(broker.address()).unwrap();
        client
            .write_all(connect_line(&broker, &authority).as_bytes())
            .unwrap();
        let mut ok = [0; 3];
        client.read_exact(&mut ok).unwrap();
        assert_eq!(&ok, b"OK\n");
        broker.cleanup_handle().run();
        assert_eq!(server.join().unwrap(), 0);
        let mut byte = [0];
        assert!(matches!(client.read(&mut byte), Ok(0) | Err(_)));
        assert!(lock(&broker.state.sockets).is_empty());
    }

    #[test]
    fn malformed_oversized_and_cross_run_tokens_are_denied() {
        let (temp, broker, identity) = broker();
        assert_eq!(exchange(&broker, "bad\n"), "ERR\n");
        assert_eq!(
            exchange(&broker, &"x".repeat(MAX_BROKER_FRAME_SIZE)),
            "ERR\n"
        );
        let second = Broker::bind(
            SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            &temp.path().join("net"),
            identity,
            "other".into(),
            temp.path(),
        )
        .unwrap();
        second.start_accepting().unwrap();
        assert_eq!(
            exchange(
                &second,
                &format!(
                    "VHRN-BROKER/1 READY {}\n",
                    fs::read_to_string(broker.token_file()).unwrap()
                )
            ),
            "ERR\n"
        );
        let mut slow = TcpStream::connect(broker.address()).unwrap();
        slow.write_all(b"VHRN-BROKER/1 READY ").unwrap();
        slow.set_read_timeout(Some(HANDSHAKE_DEADLINE + Duration::from_secs(1)))
            .unwrap();
        assert_eq!(exchange_read(&mut slow), "ERR\n");
    }

    #[test]
    fn numeric_and_localhost_grants_stay_distinct_with_ipv6_fallback() {
        let (temp, broker, _) = broker();
        let Ok(origin) = TcpListener::bind((std::net::Ipv6Addr::LOCALHOST, 0)) else {
            return;
        };
        let port = origin.local_addr().unwrap().port();
        let net = temp.path().join("net/loopback.allow");
        fs::write(&net, format!("127.0.0.1:{port}\n")).unwrap();
        assert_eq!(
            exchange(
                &broker,
                &connect_line(&broker, &format!("localhost:{port}"))
            ),
            "ERR\n"
        );
        fs::write(&net, format!("localhost:{port}\n")).unwrap();
        let accepted = thread::spawn(move || origin.accept().unwrap().0);
        let mut client = TcpStream::connect(broker.address()).unwrap();
        client
            .write_all(connect_line(&broker, &format!("localhost:{port}")).as_bytes())
            .unwrap();
        let mut ok = [0; 3];
        client.read_exact(&mut ok).unwrap();
        assert_eq!(&ok, b"OK\n");
        drop(client);
        drop(accepted.join().unwrap());
    }

    #[test]
    fn staging_collision_preserves_existing_directory_and_modes() {
        let (temp, broker, identity) = broker();
        let collision = temp.path().join("broker/collision");
        fs::create_dir_all(&collision).unwrap();
        fs::write(collision.join("token"), b"winner").unwrap();
        assert!(
            Broker::bind(
                SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
                &temp.path().join("net"),
                identity,
                "collision".into(),
                temp.path()
            )
            .is_err()
        );
        assert_eq!(fs::read(collision.join("token")).unwrap(), b"winner");
        let mode = std::os::unix::fs::PermissionsExt::mode(
            &fs::metadata(broker.token_file()).unwrap().permissions(),
        ) & 0o777;
        assert_eq!(mode, 0o444);
    }

    fn exchange_read(stream: &mut TcpStream) -> String {
        let mut response = [0; 4];
        let count = stream.read(&mut response).unwrap();
        String::from_utf8(response[..count].to_vec()).unwrap()
    }
}
