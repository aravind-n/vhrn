//! Black-box process checks for the proxy consumer contract.
//!
//! The harness deliberately uses only the executable's environment and TCP
//! interface, so `VHRN_PROXY_TEST_BIN` can point it at another compatible
//! executable.

use std::future::Future;
use std::io::Read;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{MissedTickBehavior, timeout};

const DEADLINE: Duration = Duration::from_secs(3);
const ACCEPT_DEADLINE: Duration = Duration::from_secs(10);
const SCENARIO_DEADLINE: Duration = Duration::from_secs(20);
const TOKEN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const START_ATTEMPTS: usize = 5;
const PROXY_ENV_VARS: &[&str] = &[
    "VHRN_ALLOWLISTS",
    "VHRN_ALLOWLIST",
    "VHRN_MODE_FILE",
    "VHRN_PROXY_LISTEN",
    "VHRN_DENY_LOG",
    "VHRN_LOOPBACK_ALLOWLISTS",
    "VHRN_BROKER_ADDR",
    "VHRN_BROKER_TOKEN_FILE",
];

struct Proxy {
    _temp: TempDir,
    child: ManagedChild,
    address: SocketAddr,
    policy: PathBuf,
    mode: PathBuf,
    log: PathBuf,
    local: Option<LocalFixture>,
}

#[derive(Clone)]
struct LocalFixture {
    policies: [PathBuf; 3],
    granted_layer: usize,
}

impl LocalFixture {
    fn new(policies: [PathBuf; 3], granted_layer: usize) -> Self {
        assert!(
            granted_layer < policies.len(),
            "local grant layer must exist"
        );
        Self {
            policies,
            granted_layer,
        }
    }
}

struct ManagedChild {
    child: Child,
    stderr: Arc<Mutex<String>>,
    reader: Option<thread::JoinHandle<()>>,
}

impl std::ops::Deref for ManagedChild {
    type Target = Child;

    fn deref(&self) -> &Self::Target {
        &self.child
    }
}

impl std::ops::DerefMut for ManagedChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.child
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        self.finish_stderr();
    }
}

impl ManagedChild {
    fn spawn(command: &mut Command) -> Self {
        command.stderr(Stdio::piped());
        let mut child = command.spawn().expect("start proxy executable");
        let stderr = Arc::new(Mutex::new(String::new()));
        let captured = stderr.clone();
        let mut output = child.stderr.take().expect("capture proxy stderr");
        let reader = thread::spawn(move || {
            let mut text = String::new();
            let _ = output.read_to_string(&mut text);
            *captured
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = text;
        });
        Self {
            child,
            stderr,
            reader: Some(reader),
        }
    }

    fn finish_stderr(&mut self) {
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }

    fn diagnostics(&self) -> String {
        self.stderr
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .replace(TOKEN, "[REDACTED]")
    }

    async fn wait_for_exit(&mut self, deadline: Duration, operation: &str) -> ExitStatus {
        let mut interval = tokio::time::interval(Duration::from_millis(20));
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        timeout(deadline, async {
            loop {
                interval.tick().await;
                if let Some(status) = self.child.try_wait().expect("query proxy child status") {
                    self.finish_stderr();
                    return status;
                }
            }
        })
        .await
        .unwrap_or_else(|_| {
            let status = self.force_terminate_and_wait();
            panic!(
                "{operation} did not exit within {deadline:?}; terminated with {status}; stderr: {}",
                self.diagnostics(),
            )
        })
    }

    async fn terminate_and_wait(&mut self, operation: &str) -> ExitStatus {
        if self
            .child
            .try_wait()
            .expect("query proxy child status")
            .is_none()
        {
            self.child.kill().expect("terminate proxy child");
        }
        self.wait_for_exit(DEADLINE, operation).await
    }

    fn force_terminate_and_wait(&mut self) -> ExitStatus {
        if self
            .child
            .try_wait()
            .expect("query proxy child status")
            .is_none()
        {
            self.child.kill().expect("terminate proxy child");
        }
        let status = self.child.wait().expect("wait for proxy child");
        self.finish_stderr();
        status
    }
}

impl Proxy {
    async fn start(local: Option<(String, &str, usize)>) -> Self {
        let temp = tempfile::tempdir().expect("temporary test directory");
        let public_policies =
            ["base", "harness", "global", "project", "run"].map(|name| temp.path().join(name));
        let mode = temp.path().join("mode");
        let log = temp.path().join("denials.log");
        for policy in &public_policies {
            std::fs::write(policy, "").expect("policy");
        }
        std::fs::write(&public_policies[3], "allowed.example\n").expect("policy");
        std::fs::write(&mode, "enforce\n").expect("mode");
        let local = if let Some((broker, authority, grant)) = local {
            let policies = ["local-one", "local-two", "local-three"];
            let mut paths = Vec::new();
            for (index, name) in policies.into_iter().enumerate() {
                let path = temp.path().join(name);
                let contents = if index == grant { authority } else { "" };
                std::fs::write(&path, contents).expect("local policy");
                paths.push(path);
            }
            let token = temp.path().join("token");
            std::fs::write(&token, TOKEN).expect("token");
            Some((
                LocalFixture::new(paths.try_into().expect("three local policies"), grant),
                broker,
                token,
            ))
        } else {
            None
        };

        for attempt in 1..=START_ATTEMPTS {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
                .await
                .expect("reserve listener address");
            let address = listener.local_addr().expect("listener address");
            drop(listener);
            let mut command = proxy_command();
            command
                .env("VHRN_ALLOWLISTS", join_paths(&public_policies))
                .env("VHRN_MODE_FILE", &mode)
                .env("VHRN_PROXY_LISTEN", address.to_string())
                .env("VHRN_DENY_LOG", &log)
                .stdout(Stdio::null());
            if let Some((fixture, broker, token)) = &local {
                command
                    .env("VHRN_LOOPBACK_ALLOWLISTS", join_paths(&fixture.policies))
                    .env("VHRN_BROKER_ADDR", broker)
                    .env("VHRN_BROKER_TOKEN_FILE", token);
            }
            let mut child = ManagedChild::spawn(&mut command);
            match ready(address, &mut child).await {
                Ok(()) => {
                    return Self {
                        _temp: temp,
                        child,
                        address,
                        policy: public_policies[3].clone(),
                        mode,
                        log,
                        local: local.as_ref().map(|(fixture, _, _)| fixture.clone()),
                    };
                }
                Err(diagnostic) if address_in_use(&diagnostic) && attempt < START_ATTEMPTS => {
                    drop(child);
                }
                Err(diagnostic) => {
                    panic!("proxy startup failed on attempt {attempt}: {diagnostic}")
                }
            }
        }
        unreachable!("startup attempts are bounded")
    }

    async fn request(&self, bytes: &str) -> String {
        request(self.address, bytes.as_bytes()).await
    }

    async fn start_local(broker: &Broker, authority: &str) -> Self {
        Self::start_with_broker(broker, broker.address().to_string(), authority).await
    }

    async fn start_local_hostname(broker: &Broker, authority: &str) -> Self {
        Self::start_with_broker(
            broker,
            format!("localhost:{}", broker.address().port()),
            authority,
        )
        .await
    }

    async fn start_with_broker(broker: &Broker, endpoint: String, authority: &str) -> Self {
        let readiness = broker.start_readiness_responder();
        let proxy = Self::start(Some((endpoint, authority, 0))).await;
        readiness.stop().await;
        proxy
    }

    fn revoke_local_grant(&self) {
        let fixture = self.local.as_ref().expect("local fixture");
        std::fs::write(&fixture.policies[fixture.granted_layer], "").expect("replace local grant");
    }
}

fn proxy_command() -> Command {
    let mut command = Command::new(proxy_bin());
    for name in PROXY_ENV_VARS {
        command.env_remove(name);
    }
    command
}

#[test]
fn proxy_command_removes_every_proxy_environment_variable() {
    let command = proxy_command();
    for name in PROXY_ENV_VARS {
        assert!(
            command
                .get_envs()
                .any(|(key, value)| key == *name && value.is_none()),
            "{name} must not leak from the test process"
        );
    }
}

#[derive(Clone)]
struct Broker {
    listener: Arc<TcpListener>,
}

impl Broker {
    async fn bind() -> Self {
        Self {
            listener: Arc::new(
                TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
                    .await
                    .expect("broker listener"),
            ),
        }
    }

    fn address(&self) -> SocketAddr {
        self.listener.local_addr().expect("broker address")
    }

    fn start_readiness_responder(&self) -> ReadinessResponder {
        let listener = self.listener.clone();
        let (cancel, mut cancelled) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut cancelled => return,
                    accepted = listener.accept() => {
                        let (mut stream, _) = accepted.expect("broker readiness accept");
                        assert_eq!(
                            String::from_utf8(read_line(&mut stream).await).expect("ready text"),
                            format!("VHRN-BROKER/1 READY {TOKEN}\n")
                        );
                        stream.write_all(b"OK\n").await.expect("ready response");
                    }
                }
            }
        });
        ReadinessResponder {
            cancel: Some(cancel),
            task: Some(task),
        }
    }

    async fn connect(&self, authority: &str) -> TcpStream {
        let (mut stream, _) = accept(&self.listener).await;
        assert_eq!(
            String::from_utf8(read_line(&mut stream).await).expect("connect text"),
            format!("VHRN-BROKER/1 CONNECT {TOKEN} {authority}\n")
        );
        stream.write_all(b"OK\n").await.expect("connect response");
        stream
    }
}

struct ReadinessResponder {
    cancel: Option<tokio::sync::oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl ReadinessResponder {
    async fn stop(mut self) {
        let _ = self.cancel.take().expect("readiness cancellation").send(());
        self.task
            .take()
            .expect("readiness task")
            .await
            .expect("broker readiness task");
    }
}

impl Drop for ReadinessResponder {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

fn proxy_bin() -> PathBuf {
    if let Some(path) = std::env::var_os("VHRN_PROXY_TEST_BIN") {
        return path.into();
    }
    PathBuf::from(env!("CARGO_BIN_EXE_vhrn-proxy"))
}

async fn ready(address: SocketAddr, child: &mut ManagedChild) -> Result<(), String> {
    let mut interval = tokio::time::interval(Duration::from_millis(25));
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let result = timeout(DEADLINE, async {
        loop {
            if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
                child.finish_stderr();
                return Err(format!(
                    "proxy exited during readiness with {status}; stderr: {}",
                    child.diagnostics()
                ));
            }
            if health_probe(address).await.is_ok() {
                if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
                    child.finish_stderr();
                    return Err(format!(
                        "proxy exited after readiness health check with {status}; stderr: {}",
                        child.diagnostics()
                    ));
                }
                return Ok(());
            }
            interval.tick().await;
        }
    })
    .await;
    if let Ok(value) = result {
        value
    } else {
        let status = child.terminate_and_wait("proxy readiness timeout").await;
        Err(format!(
            "proxy readiness timed out; terminated with {status}; stderr: {}",
            child.diagnostics()
        ))
    }
}

async fn health_probe(address: SocketAddr) -> Result<(), ()> {
    timeout(Duration::from_millis(100), async {
        let mut stream = TcpStream::connect(address).await.map_err(|_| ())?;
        stream
            .write_all(b"GET /healthz HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n")
            .await
            .map_err(|_| ())?;
        let mut response = Vec::new();
        let mut buffer = [0_u8; 1024];
        loop {
            let read = stream.read(&mut buffer).await.map_err(|_| ())?;
            if read == 0 {
                break;
            }
            if response.len() + read > 8 * 1024 {
                return Err(());
            }
            response.extend_from_slice(&buffer[..read]);
        }
        (response.starts_with(b"HTTP/1.1 200") && response.ends_with(b"ok\n"))
            .then_some(())
            .ok_or(())
    })
    .await
    .map_err(|_| ())?
}

fn address_in_use(diagnostic: &str) -> bool {
    diagnostic.contains("AddrInUse") || diagnostic.contains("Address already in use")
}

#[test]
fn address_in_use_recognizes_rust_and_os_diagnostics() {
    assert!(address_in_use("bind failed: AddrInUse"));
    assert!(address_in_use("bind failed: Address already in use"));
    assert!(!address_in_use("bind failed: Permission denied"));
}

fn join_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(",")
}

async fn request(address: SocketAddr, bytes: &[u8]) -> String {
    timeout(DEADLINE, async {
        let mut stream = TcpStream::connect(address).await.expect("connect proxy");
        stream.write_all(bytes).await.expect("write request");
        let mut received = Vec::new();
        stream
            .read_to_end(&mut received)
            .await
            .expect("read response");
        String::from_utf8(received).expect("HTTP response is text")
    })
    .await
    .expect("request deadline")
}

async fn scenario(future: impl Future<Output = ()>) {
    timeout(SCENARIO_DEADLINE, future)
        .await
        .expect("scenario deadline");
}

fn status(response: &str) -> u16 {
    response
        .split_whitespace()
        .nth(1)
        .expect("HTTP status")
        .parse()
        .expect("numeric status")
}

fn header_fields(response: &str) -> Vec<(String, String)> {
    response
        .split("\r\n")
        .skip(1)
        .take_while(|line| !line.is_empty())
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
        .collect()
}

fn has_header(headers: &[(String, String)], name: &str) -> bool {
    headers.iter().any(|(actual, _)| actual == name)
}

fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find_map(|(actual, value)| (actual == name).then_some(value.as_str()))
}

fn assert_absent_headers(headers: &[(String, String)], names: &[&str], recipient: &str) {
    for name in names {
        assert!(!has_header(headers, name), "{recipient} received {name}");
    }
}

fn assert_response_headers_are_sanitized(headers: &[(String, String)]) {
    assert_eq!(
        header_value(headers, "x-ordinary-response"),
        Some("retained")
    );
    assert_absent_headers(
        headers,
        &[
            "x-response-nominated",
            "proxy-authenticate",
            "keep-alive",
            "te",
            "trailer",
            "upgrade",
        ],
        "client",
    );
    assert!(
        headers
            .iter()
            .filter(|(name, _)| name == "connection")
            .all(|(_, value)| {
                !value.eq_ignore_ascii_case("X-Response-Nominated, keep-alive")
                    && !value.to_ascii_lowercase().contains("x-response-nominated")
            }),
        "client received origin-controlled Connection value"
    );
}

async fn accept(listener: &TcpListener) -> (TcpStream, SocketAddr) {
    timeout(ACCEPT_DEADLINE, listener.accept())
        .await
        .expect("broker accept")
        .expect("broker connection")
}

async fn read_through(stream: &mut TcpStream, marker: &[u8]) -> Vec<u8> {
    timeout(DEADLINE, async {
        let mut bytes = Vec::new();
        while !bytes.ends_with(marker) {
            let mut byte = [0_u8; 1];
            stream.read_exact(&mut byte).await.expect("read peer bytes");
            bytes.push(byte[0]);
            assert!(bytes.len() <= 32 * 1024, "peer headers are bounded");
        }
        bytes
    })
    .await
    .expect("peer read deadline")
}

async fn read_line(stream: &mut TcpStream) -> Vec<u8> {
    timeout(DEADLINE, async {
        let mut frame = Vec::new();
        loop {
            let mut byte = [0_u8; 1];
            stream
                .read_exact(&mut byte)
                .await
                .expect("broker ready frame");
            frame.push(byte[0]);
            assert!(frame.len() <= 256, "broker frame is bounded");
            if byte[0] == b'\n' {
                return frame;
            }
        }
    })
    .await
    .expect("broker frame deadline")
}

#[tokio::test]
async fn direct_endpoints_and_denial_log_follow_corpus() {
    scenario(async {
    let proxy = Proxy::start(None).await;
    for (path, expected) in [("/healthz", 200), ("/__status", 200), ("/not-found", 404)] {
        let response = proxy
            .request(&format!(
                "GET {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n"
            ))
            .await;
        assert_eq!(status(&response), expected, "{path}");
    }
    let health = proxy
        .request("GET /healthz HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n")
        .await;
    assert!(health.ends_with("ok\n"));
    let status_response = proxy
        .request("GET /__status HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n")
        .await;
    assert!(status_response.contains("application/json") && status_response.ends_with('\n'));
    let denied = proxy.request("GET http://blocked.example/path HTTP/1.1\r\nHost: blocked.example\r\nConnection: close\r\n\r\n").await;
    assert_eq!(status(&denied), 403);
    let log = std::fs::read_to_string(&proxy.log).expect("denial log");
    assert_eq!(log.lines().count(), 1);
    assert!(log.ends_with("\tblocked.example\n"));
    assert!(!log.contains(TOKEN));
    })
    .await;
}

#[tokio::test]
async fn policy_modes_and_live_replacement_are_observed_per_request() {
    scenario(async {
    let proxy = Proxy::start(None).await;
    let allowed = proxy.request("GET http://allowed.example/ HTTP/1.1\r\nHost: allowed.example\r\nConnection: close\r\n\r\n").await;
    assert_eq!(
        status(&allowed),
        502,
        "enforce permits the grant from the populated public layer"
    );
    std::fs::write(&proxy.mode, "report\n").expect("report mode");
    let report = proxy.request("GET http://blocked.example/ HTTP/1.1\r\nHost: blocked.example\r\nConnection: close\r\n\r\n").await;
    assert_eq!(
        status(&report),
        502,
        "report permits the request before connector failure"
    );
    assert_eq!(
        std::fs::read_to_string(&proxy.log)
            .expect("log")
            .lines()
            .count(),
        1
    );
    std::fs::write(&proxy.mode, "open\n").expect("open mode");
    let open = proxy.request("GET http://blocked.example/ HTTP/1.1\r\nHost: blocked.example\r\nConnection: close\r\n\r\n").await;
    assert_eq!(status(&open), 502);
    assert_eq!(
        std::fs::read_to_string(&proxy.log)
            .expect("log")
            .lines()
            .count(),
        1
    );
    std::fs::write(&proxy.mode, "enforce\n").expect("enforce mode");
    std::fs::write(&proxy.policy, "\n").expect("replace policy");
    let denied = proxy.request("GET http://allowed.example/ HTTP/1.1\r\nHost: allowed.example\r\nConnection: close\r\n\r\n").await;
    assert_eq!(status(&denied), 403);
    })
    .await;
}

#[tokio::test]
async fn local_startup_exchange_and_partial_configuration_are_process_checked() {
    scenario(async {
        let broker = Broker::bind().await;
        let proxy = Proxy::start_local(&broker, "localhost:80").await;
        assert_eq!(
            status(
                &proxy
                    .request("GET /healthz HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n")
                    .await
            ),
            200
        );

        let mut command = proxy_command();
        command
            .env("VHRN_PROXY_LISTEN", "127.0.0.1:0")
            .env("VHRN_BROKER_ADDR", "127.0.0.1:1")
            .stdout(Stdio::null());
        let mut child = ManagedChild::spawn(&mut command);
        let result = child
            .wait_for_exit(DEADLINE, "partial local configuration")
            .await;
        assert!(
            !result.success(),
            "partial local configuration must fail startup; stderr: {}",
            child.diagnostics()
        );
        assert!(
            child.diagnostics().contains("VHRN_BROKER"),
            "partial configuration stderr must name the invalid configuration: {}",
            child.diagnostics()
        );
        assert!(!child.diagnostics().contains(TOKEN));
    })
    .await;
}

#[tokio::test]
async fn local_http_forwards_and_streams_through_authenticated_broker() {
    scenario(async {
    let broker = Broker::bind().await;
    let authority = "localhost:8123";
    let proxy = Proxy::start_local_hostname(&broker, authority).await;

    let mut client = TcpStream::connect(proxy.address)
        .await
        .expect("client connect");
    client
        .write_all(b"POST http://localhost:8123/path?q=one HTTP/1.1\r\nHost: localhost:8123\r\nX-Ordinary: retained\r\nConnection: X-Nominated, keep-alive\r\nX-Nominated: remove\r\nProxy-Connection: remove\r\nProxy-Authorization: remove\r\nKeep-Alive: timeout=5\r\nTE: trailers\r\nTrailer: X-Trailer\r\nUpgrade: websocket\r\nContent-Length: 4\r\n\r\nbody")
        .await
        .expect("local HTTP request");
    let mut origin = broker.connect(authority).await;
    let headers = String::from_utf8(read_through(&mut origin, b"\r\n\r\n").await)
        .expect("origin request headers");
    assert!(headers.starts_with("POST /path?q=one HTTP/1.1\r\n"));
    assert!(
        headers.contains("host: localhost:8123\r\n")
            || headers.contains("Host: localhost:8123\r\n")
    );
    let headers = header_fields(&headers);
    assert_eq!(header_value(&headers, "x-ordinary"), Some("retained"));
    assert_absent_headers(
        &headers,
        &[
            "connection",
            "x-nominated",
            "proxy-connection",
            "proxy-authorization",
            "keep-alive",
            "te",
            "trailer",
            "upgrade",
        ],
        "origin",
    );
    let mut body = [0_u8; 4];
    origin.read_exact(&mut body).await.expect("origin body");
    assert_eq!(&body, b"body");
    origin
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nX-Ordinary-Response: retained\r\nConnection: X-Response-Nominated, keep-alive\r\nX-Response-Nominated: remove\r\nProxy-Authenticate: remove\r\nKeep-Alive: timeout=5\r\nTE: trailers\r\nTrailer: X-Trailer\r\nUpgrade: websocket\r\n\r\none")
        .await
        .expect("first response bytes");
    let response_headers = read_through(&mut client, b"\r\n\r\n").await;
    let response_headers = String::from_utf8(response_headers).expect("response headers");
    assert!(response_headers.starts_with("HTTP/1.1 200"));
    let response_headers = header_fields(&response_headers);
    assert_response_headers_are_sanitized(&response_headers);
    let mut first = [0_u8; 3];
    timeout(Duration::from_millis(500), client.read_exact(&mut first))
        .await
        .expect("first local response chunk deadline")
        .expect("first local response chunk");
    assert_eq!(&first, b"one");
    origin
        .write_all(b"two")
        .await
        .expect("second response bytes");
    let mut second = [0_u8; 3];
    timeout(DEADLINE, client.read_exact(&mut second))
        .await
        .expect("second local response chunk deadline")
        .expect("second local response chunk");
    assert_eq!(&second, b"two");
    proxy.revoke_local_grant();
    client
        .write_all(b"GET http://localhost:8123/revoked HTTP/1.1\r\nHost: localhost:8123\r\n\r\n")
        .await
        .expect("revoked local HTTP request");
    let denied = read_through(&mut client, b"\r\n\r\n").await;
    assert!(
        String::from_utf8(denied)
            .expect("revoked response")
            .starts_with("HTTP/1.1 403")
    );
    assert!(
        timeout(Duration::from_millis(150), broker.listener.accept())
            .await
            .is_err(),
        "revoked local request must not reach broker"
    );
    })
    .await;
}

#[tokio::test]
async fn local_http_client_disconnect_closes_broker_origin_work() {
    scenario(async {
        let broker = Broker::bind().await;
        let authority = "localhost:8125";
        let proxy = Proxy::start_local(&broker, authority).await;
        let mut client = TcpStream::connect(proxy.address)
            .await
            .expect("client connect");
        client
            .write_all(b"GET http://localhost:8125/cancel HTTP/1.1\r\nHost: localhost:8125\r\n\r\n")
            .await
            .expect("local request");
        let mut origin = broker.connect(authority).await;
        let _ = read_through(&mut origin, b"\r\n\r\n").await;
        origin
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nearly")
            .await
            .expect("early response");
        let _ = read_through(&mut client, b"\r\n\r\n").await;
        let mut early = [0_u8; 5];
        client
            .read_exact(&mut early)
            .await
            .expect("early client bytes");
        assert_eq!(&early, b"early");
        drop(client);
        let mut end = [0_u8; 1];
        let count = timeout(DEADLINE, origin.read(&mut end))
            .await
            .expect("origin cancellation deadline")
            .expect("origin cancellation read");
        assert_eq!(count, 0, "client disconnect closes origin work");
        assert!(
            timeout(Duration::from_millis(150), broker.listener.accept())
                .await
                .is_err(),
            "cancellation must not create another broker connection"
        );
        let log = std::fs::read_to_string(&proxy.log).unwrap_or_default();
        assert!(!log.contains(TOKEN));
    })
    .await;
}

#[tokio::test]
async fn local_broker_short_failures_are_redacted_and_do_not_upgrade() {
    scenario(async {
    for response in [b"ERR\n".as_slice(), b"O".as_slice(), b"TOOLONG".as_slice()] {
        let broker = Broker::bind().await;
        let authority = "localhost:8126";
        let proxy = Proxy::start_local(&broker, authority).await;
        let request_task = tokio::spawn({
            let address = proxy.address;
            async move {
                request(
                    address,
                    b"CONNECT localhost:8126 HTTP/1.1\r\nHost: localhost:8126\r\nConnection: close\r\n\r\n",
                )
                .await
            }
        });
        let (mut peer, _) = accept(&broker.listener).await;
        assert_eq!(
            String::from_utf8(read_line(&mut peer).await).expect("CONNECT frame"),
            format!("VHRN-BROKER/1 CONNECT {TOKEN} {authority}\n")
        );
        peer.write_all(response).await.expect("failure response");
        peer.shutdown().await.expect("failure close");
        let observed = request_task.await.expect("request task");
        assert_eq!(status(&observed), 502);
        assert!(!observed.contains(TOKEN));
        assert!(!observed.contains("ERR"));
        assert!(!observed.contains("TOOLONG"));
        assert!(
            !std::fs::read_to_string(&proxy.log)
                .unwrap_or_default()
                .contains(TOKEN)
        );
    }
    })
    .await;
}

#[tokio::test]
async fn local_broker_timeout_is_bounded_and_redacted() {
    scenario(async {
        let broker = Broker::bind().await;
        let authority = "localhost:8127";
        let proxy = Proxy::start_local(&broker, authority).await;
        let mut client = TcpStream::connect(proxy.address)
            .await
            .expect("client connect");
        client
        .write_all(
            b"CONNECT localhost:8127 HTTP/1.1\r\nHost: localhost:8127\r\nConnection: close\r\n\r\n",
        )
        .await
        .expect("timeout request");
        let (mut peer, _) = accept(&broker.listener).await;
        assert_eq!(
            String::from_utf8(read_line(&mut peer).await).expect("CONNECT frame"),
            format!("VHRN-BROKER/1 CONNECT {TOKEN} {authority}\n")
        );
        let mut response = Vec::new();
        timeout(Duration::from_secs(15), client.read_to_end(&mut response))
            .await
            .expect("broker timeout deadline")
            .expect("timeout response");
        let response = String::from_utf8(response).expect("timeout text");
        assert_eq!(status(&response), 502);
        assert!(!response.contains(TOKEN));
        let mut end = [0_u8; 1];
        assert_eq!(
            timeout(DEADLINE, peer.read(&mut end))
                .await
                .expect("peer closure deadline")
                .expect("peer closure"),
            0
        );
    })
    .await;
}

#[tokio::test]
async fn local_connect_preserves_buffered_bytes_and_survives_revocation() {
    scenario(async {
        let broker = Broker::bind().await;
        let authority = "localhost:8124";
        let proxy = Proxy::start_local(&broker, authority).await;
        let mut client = TcpStream::connect(proxy.address)
            .await
            .expect("client connect");
        client
            .write_all(
                b"CONNECT localhost:8124 HTTP/1.1\r\nHost: localhost:8124\r\n\r\nclient-prefix",
            )
            .await
            .expect("CONNECT request");
        let (mut origin, _) = accept(&broker.listener).await;
        assert_eq!(
            String::from_utf8(read_line(&mut origin).await).expect("CONNECT frame"),
            format!("VHRN-BROKER/1 CONNECT {TOKEN} {authority}\n")
        );
        assert!(
            timeout(Duration::from_millis(150), client.read_u8())
                .await
                .is_err(),
            "no upgrade before broker approval"
        );
        origin.write_all(b"OK\n").await.expect("broker approval");
        let response = read_through(&mut client, b"\r\n\r\n").await;
        assert!(
            String::from_utf8(response)
                .expect("CONNECT response")
                .starts_with("HTTP/1.1 200")
        );
        let mut buffered = [0_u8; 13];
        timeout(DEADLINE, origin.read_exact(&mut buffered))
            .await
            .expect("buffered relay deadline")
            .expect("buffered relay");
        assert_eq!(&buffered, b"client-prefix");
        proxy.revoke_local_grant();
        origin
            .write_all(b"peer-data")
            .await
            .expect("tunnel peer data");
        let mut peer_data = [0_u8; 9];
        client
            .read_exact(&mut peer_data)
            .await
            .expect("tunnel client data");
        assert_eq!(&peer_data, b"peer-data");
        let denied = proxy
        .request(
            "CONNECT localhost:8124 HTTP/1.1\r\nHost: localhost:8124\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert_eq!(status(&denied), 403);
        assert!(
            timeout(Duration::from_millis(150), broker.listener.accept())
                .await
                .is_err(),
            "denied tunnel must not reach broker"
        );
        client.shutdown().await.expect("client close write");
        let mut end = [0_u8; 1];
        let origin_count = timeout(DEADLINE, origin.read(&mut end))
            .await
            .expect("origin closure deadline")
            .expect("origin closure read");
        assert_eq!(origin_count, 0);
        origin.shutdown().await.expect("origin close write");
        let client_count = timeout(DEADLINE, client.read(&mut end))
            .await
            .expect("client closure deadline")
            .expect("client closure read");
        assert_eq!(client_count, 0);
    })
    .await;
}

#[cfg(unix)]
#[tokio::test]
async fn sigterm_closes_an_established_local_tunnel() {
    scenario(async {
        let broker = Broker::bind().await;
        let authority = "localhost:8128";
        let mut proxy = Proxy::start_local(&broker, authority).await;
        let mut client = TcpStream::connect(proxy.address)
            .await
            .expect("client connect");
        client
            .write_all(b"CONNECT localhost:8128 HTTP/1.1\r\nHost: localhost:8128\r\n\r\n")
            .await
            .expect("CONNECT request");
        let mut peer = broker.connect(authority).await;
        let response = read_through(&mut client, b"\r\n\r\n").await;
        assert!(
            String::from_utf8(response)
                .expect("CONNECT response")
                .starts_with("HTTP/1.1 200")
        );
        send_sigterm(&proxy.child);
        let mut end = [0_u8; 1];
        let client_count = timeout(Duration::from_secs(1), client.read(&mut end))
            .await
            .expect("client SIGTERM deadline")
            .expect("client SIGTERM read");
        let peer_count = timeout(Duration::from_secs(1), peer.read(&mut end))
            .await
            .expect("peer SIGTERM deadline")
            .expect("peer SIGTERM read");
        assert_eq!(client_count, 0);
        assert_eq!(peer_count, 0);
        let exit = proxy.child.wait_for_exit(DEADLINE, "SIGTERM").await;
        assert!(
            exit.success(),
            "SIGTERM should be graceful; stderr: {}",
            proxy.child.diagnostics()
        );
    })
    .await;
}

#[cfg(unix)]
#[tokio::test]
async fn listener_occupation_and_sigterm_have_bounded_lifecycle() {
    scenario(async {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("occupied listener");
        let address = listener.local_addr().expect("occupied address");
        let directory = tempfile::tempdir().expect("temporary directory");
        let policies =
            ["base", "harness", "global", "project", "run"].map(|name| directory.path().join(name));
        let mode = directory.path().join("mode");
        for policy in &policies {
            std::fs::write(policy, "").expect("policy");
        }
        std::fs::write(&policies[0], "allowed.example\n").expect("policy");
        std::fs::write(&mode, "enforce\n").expect("mode");
        let mut command = proxy_command();
        command
            .env("VHRN_ALLOWLISTS", join_paths(&policies))
            .env("VHRN_MODE_FILE", mode)
            .env("VHRN_PROXY_LISTEN", address.to_string())
            .stdout(Stdio::null());
        let mut occupied = ManagedChild::spawn(&mut command);
        let exit = occupied
            .wait_for_exit(DEADLINE, "occupied listener startup")
            .await;
        assert!(
            !exit.success(),
            "occupied listener must fail startup; stderr: {}",
            occupied.diagnostics()
        );
        assert!(
            occupied.diagnostics().contains("bind proxy listener"),
            "occupied listener stderr must include bind context: {}",
            occupied.diagnostics()
        );
        assert!(!occupied.diagnostics().contains(TOKEN));
        drop(listener);

        let mut proxy = Proxy::start(None).await;
        send_sigterm(&proxy.child);
        let exit = proxy.child.wait_for_exit(DEADLINE, "SIGTERM").await;
        assert!(
            exit.success(),
            "SIGTERM should be graceful; stderr: {}",
            proxy.child.diagnostics()
        );
    })
    .await;
}

#[cfg(unix)]
fn send_sigterm(child: &ManagedChild) {
    assert!(
        Command::new("kill")
            .arg("-TERM")
            .arg(child.id().to_string())
            .status()
            .expect("send SIGTERM")
            .success()
    );
}
