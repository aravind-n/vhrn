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
use std::time::{Duration, Instant};

use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{MissedTickBehavior, timeout};

const DEADLINE: Duration = Duration::from_secs(3);
const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(8);
const ACCEPT_DEADLINE: Duration = Duration::from_secs(10);
const SCENARIO_DEADLINE: Duration = Duration::from_secs(20);
const TOKEN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const START_ATTEMPTS: usize = 5;
const POLICY_LIMIT: usize = 1024 * 1024;
static RELEASED_PORT_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
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
    token: PathBuf,
}

struct StartupFixture {
    _temp: TempDir,
    public: [PathBuf; 5],
    mode: PathBuf,
    log: PathBuf,
    local: [PathBuf; 3],
    token: PathBuf,
}

impl StartupFixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("startup fixture directory");
        let public =
            ["base", "harness", "global", "project", "run"].map(|name| temp.path().join(name));
        for path in &public {
            std::fs::write(path, "").expect("public policy");
        }
        let mode = temp.path().join("mode");
        std::fs::write(&mode, "enforce\n").expect("mode");
        let log = temp.path().join("denials.log");
        let local =
            ["local-global", "local-project", "local-run"].map(|name| temp.path().join(name));
        for path in &local {
            std::fs::write(path, "").expect("local policy");
        }
        let token = temp.path().join("token");
        std::fs::write(&token, TOKEN).expect("broker token");
        Self {
            _temp: temp,
            public,
            mode,
            log,
            local,
            token,
        }
    }

    fn command(&self, address: SocketAddr) -> Command {
        let mut command = proxy_command();
        command
            .env("VHRN_ALLOWLISTS", join_paths(&self.public))
            .env("VHRN_MODE_FILE", &self.mode)
            .env("VHRN_PROXY_LISTEN", address.to_string())
            .env("VHRN_DENY_LOG", &self.log)
            .stdout(Stdio::null());
        command
    }

    fn configure_local(&self, command: &mut Command, broker: SocketAddr) {
        command
            .env("VHRN_LOOPBACK_ALLOWLISTS", join_paths(&self.local))
            .env("VHRN_BROKER_ADDR", broker.to_string())
            .env("VHRN_BROKER_TOKEN_FILE", &self.token);
    }
}

impl LocalFixture {
    fn new(policies: [PathBuf; 3], granted_layer: usize, token: PathBuf) -> Self {
        assert!(
            granted_layer < policies.len(),
            "local grant layer must exist"
        );
        Self {
            policies,
            granted_layer,
            token,
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
        self.raw_diagnostics().replace(TOKEN, "[REDACTED]")
    }

    fn raw_diagnostics(&self) -> String {
        self.stderr
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
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
        // Serialize the test-only release-and-rebind handoff within this process.
        let _released_port = RELEASED_PORT_LOCK.lock().await;
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
                LocalFixture::new(
                    paths.try_into().expect("three local policies"),
                    grant,
                    token.clone(),
                ),
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
    diagnostic.contains("AddrInUse")
        || diagnostic.contains("Address already in use")
        || diagnostic.contains("startup_listener_bind_failed:address_in_use")
}

#[test]
fn address_in_use_recognizes_rust_and_os_diagnostics() {
    assert!(address_in_use("bind failed: AddrInUse"));
    assert!(address_in_use("bind failed: Address already in use"));
    assert!(address_in_use(
        "Error: startup_listener_bind_failed:address_in_use"
    ));
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

async fn request_with_write_shutdown(address: SocketAddr, bytes: &[u8]) -> String {
    timeout(DEADLINE, async {
        let mut stream = TcpStream::connect(address).await.expect("connect proxy");
        stream.write_all(bytes).await.expect("write request");
        stream
            .shutdown()
            .await
            .expect("shutdown request write half");
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

async fn read_http_response(stream: &mut TcpStream) -> String {
    let mut response = read_through(stream, b"\r\n\r\n").await;
    let headers = String::from_utf8(response.clone()).expect("HTTP response headers are text");
    let fields = header_fields(&headers);
    let length = header_value(&fields, "content-length")
        .expect("test response has Content-Length")
        .parse::<usize>()
        .expect("test response Content-Length is numeric");
    let mut body = vec![0_u8; length];
    timeout(DEADLINE, stream.read_exact(&mut body))
        .await
        .expect("response body deadline")
        .expect("response body");
    response.extend_from_slice(&body);
    String::from_utf8(response).expect("HTTP response is text")
}

fn atomic_replace(path: &std::path::Path, contents: &[u8]) {
    let replacement = path.with_extension("process-replacement");
    std::fs::write(&replacement, contents).expect("write replacement");
    std::fs::rename(replacement, path).expect("publish replacement");
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

async fn accept_for_child(
    listener: &TcpListener,
    child: &mut ManagedChild,
    operation: &str,
) -> (TcpStream, SocketAddr) {
    match timeout(ACCEPT_DEADLINE, listener.accept()).await {
        Ok(Ok(accepted)) => accepted,
        Ok(Err(error)) => panic!("{operation} broker accept failed: {error}"),
        Err(_) => {
            let status = child.try_wait().expect("query proxy child status");
            if status.is_some() {
                child.finish_stderr();
            }
            panic!(
                "{operation} broker accept timed out; child status: {status:?}; stderr: {}",
                child.diagnostics()
            );
        }
    }
}

async fn unused_address() -> SocketAddr {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("reserve listener address");
    let address = listener.local_addr().expect("reserved listener address");
    drop(listener);
    address
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
    let mut proxy = Proxy::start(None).await;
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
    std::fs::write(&proxy.log, "preexisting\n").expect("preexisting denial log");
    let request_secret = "request-secret-must-not-escape";
    let denied = proxy.request(&format!("GET http://blocked.example/path HTTP/1.1\r\nHost: blocked.example\r\nAuthorization: {request_secret}\r\nConnection: close\r\n\r\n")).await;
    assert_eq!(status(&denied), 403);
    let log = std::fs::read_to_string(&proxy.log).expect("denial log");
    assert_eq!(log.lines().count(), 2);
    assert!(log.starts_with("preexisting\n"));
    assert!(log.ends_with("\tblocked.example\n"));
    assert!(!log.contains(TOKEN));
    assert!(!log.contains(request_secret));
    let _ = proxy
        .child
        .terminate_and_wait("denial diagnostic capture")
        .await;
    let diagnostics = proxy.child.raw_diagnostics();
    assert!(diagnostics.contains("vhrn-proxy: denial target=blocked.example mode=enforce"));
    assert!(!diagnostics.contains(TOKEN));
    assert!(!diagnostics.contains(request_secret));
    assert!(!diagnostics.contains(&proxy.policy.display().to_string()));
    })
    .await;
}

#[tokio::test]
async fn http1_ingress_and_direct_endpoint_contract_is_exact() {
    scenario(async {
        let proxy = Proxy::start(None).await;

        let health = proxy
            .request("GET /healthz HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n")
            .await;
        assert_eq!(status(&health), 200);
        assert!(health.contains("content-type: text/plain; charset=utf-8\r\n"));
        assert!(health.ends_with("\r\n\r\nok\n"));

        let health_head = proxy
            .request("HEAD /healthz HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n")
            .await;
        assert_eq!(status(&health_head), 200);
        assert!(health_head.contains("content-length: 3\r\n"));
        assert!(health_head.ends_with("\r\n\r\n"));

        for path in ["/healthz", "/__status"] {
            let response = proxy
                .request(&format!(
                    "POST {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n"
                ))
                .await;
            assert_eq!(status(&response), 405, "{path}");
            assert!(response.contains("allow: GET, HEAD\r\n"), "{path}");
        }
        assert_eq!(
            status(
                &proxy
                    .request("POST /missing HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n")
                    .await
            ),
            404
        );

        let options = proxy
            .request("OPTIONS * HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n")
            .await;
        assert_eq!(status(&options), 204);
        assert!(options.contains("allow: GET, HEAD, OPTIONS, CONNECT\r\n"));
        assert!(options.ends_with("\r\n\r\n"));
        assert_eq!(
            status(
                &proxy
                    .request("GET * HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n")
                    .await
            ),
            400
        );

        std::fs::write(&proxy.policy, "bad!policy\n").expect("invalid allowlist");
        std::fs::write(&proxy.mode, "open\n").expect("valid mode");
        let status_only_mode = proxy
            .request("GET /__status HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n")
            .await;
        assert_eq!(status(&status_only_mode), 200);
        assert!(status_only_mode.ends_with("{\"mode\":\"open\"}\n"));
        std::fs::write(&proxy.mode, "invalid\n").expect("invalid mode");
        let fail_closed = proxy
            .request("HEAD /__status HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n")
            .await;
        assert_eq!(status(&fail_closed), 503);
        assert!(fail_closed.contains("content-type: application/json\r\n"));
        assert!(fail_closed.contains("content-length: 19\r\n"));
        assert!(fail_closed.ends_with("\r\n\r\n"));
    })
    .await;
}

#[tokio::test]
async fn http1_malformed_hosts_versions_and_prefaces_are_rejected() {
    scenario(async {
        let proxy = Proxy::start(None).await;
        assert_eq!(
            status(
                &proxy
                    .request("GET /healthz HTTP/1.1\r\nConnection: close\r\n\r\n")
                    .await
            ),
            400
        );
        let head_error = proxy
            .request("HEAD /healthz HTTP/1.1\r\nConnection: close\r\n\r\n")
            .await;
        assert_eq!(status(&head_error), 400);
        assert!(head_error.ends_with("\r\n\r\n"));
        assert_eq!(
            status(
                &proxy
                    .request(
                        "GET /healthz HTTP/1.1\r\nHost: one\r\nHost: two\r\nConnection: close\r\n\r\n"
                    )
                    .await
            ),
            400
        );
        assert_eq!(
            status(
                &proxy
                    .request("GET /healthz HTTP/2.0\r\nHost: test\r\n\r\n")
                    .await
            ),
            505
        );
        for malformed in [
            "GET /healthz HTTP/\r\nHost: test\r\n\r\n",
            "GET /healthz HTTP/1\r\nHost: test\r\n\r\n",
            "GET /healthz HTTP/1.x\r\nHost: test\r\n\r\n",
            "GET /healthz HTTP/11.1\r\nHost: test\r\n\r\n",
        ] {
            assert_eq!(status(&proxy.request(malformed).await), 400, "{malformed:?}");
        }
        assert_eq!(
            status(
                &proxy
                    .request("GET /healthz HTTP/1.1\rX: bad\r\n\r\n")
                    .await
            ),
            400
        );
        let prior_knowledge = request_with_write_shutdown(
            proxy.address,
            b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n",
        )
        .await;
        assert_eq!(status(&prior_knowledge), 505);
        let tls = request_with_write_shutdown(proxy.address, b"\x16").await;
        assert_eq!(status(&tls), 400);
    })
    .await;
}

#[tokio::test]
async fn http1_persistence_connect_framing_and_upgrade_are_enforced_before_broker_work() {
    scenario(async {
        let broker = Broker::bind().await;
        let authority = "localhost:8130";
        let proxy = Proxy::start_local(&broker, authority).await;

        let mut client = TcpStream::connect(proxy.address)
            .await
            .expect("persistent client");
        client
            .write_all(b"GET /healthz HTTP/1.0\r\nConnection: keep-alive\r\n\r\n")
            .await
            .expect("HTTP/1.0 request");
        let first = read_http_response(&mut client).await;
        assert!(first.starts_with("HTTP/1.0 200"));
        client
            .write_all(b"GET /healthz HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n")
            .await
            .expect("HTTP/1.1 request");
        let mut second = Vec::new();
        client.read_to_end(&mut second).await.expect("closing response");
        assert!(second.starts_with(b"HTTP/1.1 200"));

        for request_head in [
            format!(
                "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\nContent-Length: 0\r\n\r\n"
            ),
            format!(
                "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\nTransfer-Encoding: chunked\r\n\r\n"
            ),
            format!(
                "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\nExpect: 100-continue\r\n\r\n"
            ),
            format!(
                "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\nHost: duplicate:8130\r\n\r\n"
            ),
        ] {
            assert_eq!(status(&proxy.request(&request_head).await), 400);
        }

        let upgrade = proxy
            .request(&format!(
                "GET http://{authority}/ HTTP/1.1\r\nHost: {authority}\r\nConnection: upgrade\r\nUpgrade: h2c\r\n\r\n"
            ))
            .await;
        assert_eq!(status(&upgrade), 501);
        assert!(
            timeout(Duration::from_millis(150), broker.listener.accept())
                .await
                .is_err(),
            "rejected CONNECT framing and upgrades cannot contact the broker"
        );

        let mut expect_client = TcpStream::connect(proxy.address)
            .await
            .expect("Expect client");
        expect_client
            .write_all(
                format!(
                    "POST http://{authority}/expect HTTP/1.1\r\nHost: {authority}\r\nContent-Length: 4\r\nExpect: 100-continue, 100-continue\r\nExpect: 100-continue\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .expect("Expect request head");
        let mut origin = broker.connect(authority).await;
        let origin_head = String::from_utf8(read_through(&mut origin, b"\r\n\r\n").await)
            .expect("origin request head");
        assert!(
            origin_head
                .to_ascii_lowercase()
                .contains("expect: 100-continue")
        );
        origin
            .write_all(b"HTTP/1.1 100 Continue\r\n\r\n")
            .await
            .expect("origin interim response");
        let interim = read_through(&mut expect_client, b"\r\n\r\n").await;
        assert!(interim.starts_with(b"HTTP/1.1 100 Continue\r\n"));
        expect_client.write_all(b"body").await.expect("Expect body");
        let mut origin_body = [0; 4];
        origin
            .read_exact(&mut origin_body)
            .await
            .expect("origin request body");
        assert_eq!(&origin_body, b"body");
        origin
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
            .await
            .expect("origin response");
        let mut final_response = Vec::new();
        expect_client
            .read_to_end(&mut final_response)
            .await
            .expect("Expect final response");
        assert!(final_response.starts_with(b"HTTP/1.1 200"));
    })
    .await;
}

#[tokio::test]
async fn health_tracks_live_policy_and_sticky_audit_failure() {
    scenario(async {
        let mut proxy = Proxy::start(None).await;
        std::fs::write(&proxy.policy, "bad!policy\n").expect("invalidate public policy");
        let unhealthy = proxy
            .request("GET /healthz HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n")
            .await;
        assert_eq!(status(&unhealthy), 503);
        assert!(unhealthy.ends_with("unhealthy\n"));

        std::fs::write(&proxy.policy, "allowed.example\n").expect("repair public policy");
        assert_eq!(
            status(
                &proxy
                    .request(
                        "GET /healthz HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n"
                    )
                    .await
            ),
            200
        );

        std::fs::remove_file(&proxy.log).expect("remove denial log");
        std::fs::create_dir(&proxy.log).expect("replace denial log with directory");
        let enforced = proxy
            .request("GET http://blocked.example/ HTTP/1.1\r\nHost: blocked.example\r\nConnection: close\r\n\r\n")
            .await;
        assert_eq!(status(&enforced), 403, "audit failure cannot alter denial");
        assert_eq!(
            status(
                &proxy
                    .request(
                        "GET /healthz HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n"
                    )
                    .await
            ),
            503
        );

        std::fs::write(&proxy.mode, "report\n").expect("report mode");
        for _ in 0..2 {
            let unavailable = proxy
                .request("GET http://blocked.example/ HTTP/1.1\r\nHost: blocked.example\r\nConnection: close\r\n\r\n")
                .await;
            assert_eq!(status(&unavailable), 503);
            assert!(unavailable.ends_with("proxy temporarily unavailable\n"));
        }

        std::fs::remove_dir(&proxy.log).expect("remove broken denial log");
        std::fs::write(&proxy.log, "repaired\n").expect("repair denial log");
        assert_eq!(
            status(
                &proxy
                    .request(
                        "GET /healthz HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n"
                    )
                    .await
            ),
            503,
            "an open probe cannot clear sticky append failure"
        );
        let recovered = proxy
            .request("GET http://blocked.example/ HTTP/1.1\r\nHost: blocked.example\r\nConnection: close\r\n\r\n")
            .await;
        assert_eq!(status(&recovered), 502, "successful record permits report mode");
        assert_eq!(
            status(
                &proxy
                    .request(
                        "GET /healthz HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n"
                    )
                    .await
            ),
            200
        );
        let log = std::fs::read_to_string(&proxy.log).expect("repaired log");
        assert!(log.starts_with("repaired\n"));
        assert!(log.ends_with("\tblocked.example\n"));
        let _ = proxy
            .child
            .terminate_and_wait("report diagnostic capture")
            .await;
        let diagnostics = proxy.child.raw_diagnostics();
        assert!(diagnostics.contains("denial target=blocked.example mode=enforce"));
        assert!(diagnostics.contains("denial target=blocked.example mode=report"));
    })
    .await;
}

#[tokio::test]
async fn invalid_startup_policy_log_and_token_exit_redacted() {
    scenario(async {
        let _released_port = RELEASED_PORT_LOCK.lock().await;
        let fixture = StartupFixture::new();

        std::fs::write(&fixture.public[0], "bad!policy\n").expect("invalid policy");
        let address = unused_address().await;
        let mut child = ManagedChild::spawn(&mut fixture.command(address));
        let exit = child
            .wait_for_exit(DEADLINE, "invalid startup policy")
            .await;
        assert!(!exit.success());
        assert!(
            child
                .diagnostics()
                .contains("startup_public_policy_invalid")
        );
        assert!(
            !child
                .raw_diagnostics()
                .contains(&fixture.public[0].display().to_string())
        );
        assert!(
            !fixture.log.exists(),
            "policy validation precedes denial-log open"
        );

        std::fs::write(&fixture.public[0], "").expect("repair policy");
        std::fs::create_dir(&fixture.log).expect("invalid log destination");
        let address = unused_address().await;
        let mut child = ManagedChild::spawn(&mut fixture.command(address));
        let exit = child.wait_for_exit(DEADLINE, "invalid startup log").await;
        assert!(!exit.success());
        assert!(
            child
                .diagnostics()
                .contains("startup_denial_log_unavailable")
        );
        assert!(
            !child
                .raw_diagnostics()
                .contains(&fixture.log.display().to_string())
        );
        assert!(TcpStream::connect(address).await.is_err());

        std::fs::remove_dir(&fixture.log).expect("remove invalid log destination");
        std::fs::write(&fixture.local[1], "bad authority\n").expect("invalid local policy");
        let address = unused_address().await;
        let broker = Broker::bind().await;
        let mut command = fixture.command(address);
        fixture.configure_local(&mut command, broker.address());
        let mut child = ManagedChild::spawn(&mut command);
        let exit = child
            .wait_for_exit(DEADLINE, "invalid startup local policy")
            .await;
        assert!(!exit.success());
        assert!(child.diagnostics().contains("startup_local_policy_invalid"));
        assert!(
            !child
                .raw_diagnostics()
                .contains(&fixture.local[1].display().to_string())
        );
        assert!(
            !fixture.log.exists(),
            "local validation precedes denial-log open"
        );

        std::fs::write(&fixture.local[1], "").expect("repair local policy");
        let invalid_token = "invalid-token-secret";
        std::fs::write(&fixture.token, invalid_token).expect("invalid token");
        let address = unused_address().await;
        let mut command = fixture.command(address);
        fixture.configure_local(&mut command, broker.address());
        let mut child = ManagedChild::spawn(&mut command);
        let exit = child.wait_for_exit(DEADLINE, "invalid startup token").await;
        assert!(!exit.success());
        let diagnostics = child.raw_diagnostics();
        assert!(diagnostics.contains("startup_broker_token_invalid"));
        assert!(!diagnostics.contains(invalid_token));
        assert!(!diagnostics.contains(&fixture.token.display().to_string()));
        assert!(!diagnostics.contains(&broker.address().to_string()));
        assert!(TcpStream::connect(address).await.is_err());
    })
    .await;
}

#[cfg(unix)]
#[tokio::test]
async fn nonregular_token_fails_promptly_without_serving() {
    scenario(async {
        let _released_port = RELEASED_PORT_LOCK.lock().await;
        let broker = Broker::bind().await;
        let fixture = StartupFixture::new();
        std::fs::remove_file(&fixture.token).expect("remove regular token");
        assert!(
            Command::new("mkfifo")
                .arg(&fixture.token)
                .status()
                .expect("create token FIFO")
                .success()
        );
        let address = unused_address().await;
        let mut command = fixture.command(address);
        fixture.configure_local(&mut command, broker.address());
        let mut child = ManagedChild::spawn(&mut command);
        let exit = child
            .wait_for_exit(DEADLINE, "nonregular startup token")
            .await;
        assert!(!exit.success());
        let diagnostics = child.raw_diagnostics();
        assert!(diagnostics.contains("startup_broker_token_invalid"));
        assert!(!diagnostics.contains(TOKEN));
        assert!(!diagnostics.contains(&fixture.token.display().to_string()));
        assert!(TcpStream::connect(address).await.is_err());
    })
    .await;
}

#[tokio::test]
async fn listener_does_not_serve_until_broker_readiness_and_refusal_is_fatal() {
    scenario(async {
        let _released_port = RELEASED_PORT_LOCK.lock().await;
        let broker = Broker::bind().await;
        let fixture = StartupFixture::new();
        let address = unused_address().await;
        let mut command = fixture.command(address);
        fixture.configure_local(&mut command, broker.address());
        let mut child = ManagedChild::spawn(&mut command);
        let (mut readiness, _) =
            accept_for_child(&broker.listener, &mut child, "broker readiness approval").await;
        assert_eq!(
            String::from_utf8(read_line(&mut readiness).await).expect("READY frame"),
            format!("VHRN-BROKER/1 READY {TOKEN}\n")
        );
        let mut client = TcpStream::connect(address)
            .await
            .expect("connect bound pre-ready listener");
        client
            .write_all(b"GET /healthz HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n")
            .await
            .expect("queue pre-ready request");
        let mut byte = [0_u8; 1];
        assert!(
            timeout(Duration::from_millis(150), client.read(&mut byte))
                .await
                .is_err(),
            "bound listener must not serve before READY"
        );
        readiness
            .write_all(b"OK\n")
            .await
            .expect("approve readiness");
        let mut response = Vec::new();
        client
            .read_to_end(&mut response)
            .await
            .expect("health response");
        assert_eq!(
            status(&String::from_utf8(response).expect("response text")),
            200
        );
        let _ = child.terminate_and_wait("approved readiness cleanup").await;

        let fixture = StartupFixture::new();
        let address = unused_address().await;
        let mut command = fixture.command(address);
        fixture.configure_local(&mut command, broker.address());
        let mut child = ManagedChild::spawn(&mut command);
        let (mut readiness, _) =
            accept_for_child(&broker.listener, &mut child, "broker readiness refusal").await;
        let _ = read_line(&mut readiness).await;
        let mut client = TcpStream::connect(address)
            .await
            .expect("connect listener before refusal");
        client
            .write_all(b"GET /healthz HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n")
            .await
            .expect("queue request before refusal");
        readiness
            .write_all(b"ERR\n")
            .await
            .expect("refuse readiness");
        readiness
            .shutdown()
            .await
            .expect("close readiness response");
        let exit = child
            .wait_for_exit(DEADLINE, "broker readiness refusal")
            .await;
        assert!(!exit.success());
        assert!(
            child
                .diagnostics()
                .contains("startup_broker_readiness_failed")
        );
        let mut received = Vec::new();
        let _ = client.read_to_end(&mut received).await;
        assert!(
            received.is_empty(),
            "startup failure cannot serve queued HTTP"
        );
    })
    .await;
}

#[tokio::test]
async fn broker_readiness_timeout_is_fatal_and_redacted() {
    scenario(async {
        let _released_port = RELEASED_PORT_LOCK.lock().await;
        let broker = Broker::bind().await;
        let fixture = StartupFixture::new();
        let address = unused_address().await;
        let mut command = fixture.command(address);
        fixture.configure_local(&mut command, broker.address());
        let mut child = ManagedChild::spawn(&mut command);
        let (mut readiness, _) =
            accept_for_child(&broker.listener, &mut child, "broker readiness timeout").await;
        let _ = read_line(&mut readiness).await;
        let exit = child
            .wait_for_exit(Duration::from_secs(5), "broker readiness timeout")
            .await;
        assert!(!exit.success());
        let diagnostics = child.raw_diagnostics();
        assert!(diagnostics.contains("startup_broker_readiness_failed"));
        assert!(!diagnostics.contains(TOKEN));
        assert!(!diagnostics.contains(&broker.address().to_string()));
        let mut byte = [0_u8; 1];
        assert_eq!(readiness.read(&mut byte).await.expect("readiness close"), 0);
    })
    .await;
}

#[cfg(unix)]
#[tokio::test]
async fn cancellation_after_bind_closes_listener_and_pending_broker_exchange() {
    scenario(async {
        let _released_port = RELEASED_PORT_LOCK.lock().await;
        let broker = Broker::bind().await;
        let fixture = StartupFixture::new();
        let address = unused_address().await;
        let mut command = fixture.command(address);
        fixture.configure_local(&mut command, broker.address());
        let mut child = ManagedChild::spawn(&mut command);
        let (mut readiness, _) = accept(&broker.listener).await;
        let _ = read_line(&mut readiness).await;
        let mut client = TcpStream::connect(address)
            .await
            .expect("connect bound startup listener");
        client
            .write_all(b"GET /healthz HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n")
            .await
            .expect("queue startup request");
        send_sigterm(&child);
        let exit = child.wait_for_exit(DEADLINE, "startup cancellation").await;
        assert!(
            exit.success(),
            "startup cancellation should be graceful: {}",
            child.diagnostics()
        );
        let mut byte = [0_u8; 1];
        assert_eq!(readiness.read(&mut byte).await.expect("broker close"), 0);
        let mut received = Vec::new();
        let _ = client.read_to_end(&mut received).await;
        assert!(received.is_empty(), "cancelled startup cannot serve HTTP");
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
async fn persistent_client_reopens_bounded_policy_and_observes_atomic_repair() {
    scenario(async {
        let proxy = Proxy::start(None).await;
        let mut client = TcpStream::connect(proxy.address)
            .await
            .expect("persistent client connect");
        let request = b"GET http://allowed.example/ HTTP/1.1\r\nHost: allowed.example\r\n\r\n";

        let mut exact = b"allowed.example\n".to_vec();
        while exact.len() < POLICY_LIMIT {
            exact.extend_from_slice(b"a\n");
        }
        assert_eq!(exact.len(), POLICY_LIMIT);
        atomic_replace(&proxy.policy, &exact);
        client
            .write_all(request)
            .await
            .expect("exact-limit request");
        assert_eq!(
            status(&read_http_response(&mut client).await),
            502,
            "an exact-limit policy retains its matching grant"
        );

        let mut oversized = exact;
        oversized.push(b'a');
        assert_eq!(oversized.len(), POLICY_LIMIT + 1);
        atomic_replace(&proxy.policy, &oversized);
        client.write_all(request).await.expect("oversized request");
        assert_eq!(
            status(&read_http_response(&mut client).await),
            403,
            "an oversized policy fails the decision closed"
        );

        atomic_replace(&proxy.policy, b"allowed.example\n");
        client.write_all(request).await.expect("repaired request");
        assert_eq!(
            status(&read_http_response(&mut client).await),
            502,
            "atomic repair is live without restarting the process or client"
        );

        atomic_replace(&proxy.policy, b"other.example\n");
        client.write_all(request).await.expect("revoked request");
        assert_eq!(
            status(&read_http_response(&mut client).await),
            403,
            "a later atomic replacement revokes the grant on the same client"
        );
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
        assert!(!child.raw_diagnostics().contains(TOKEN));
    })
    .await;
}

#[tokio::test]
async fn local_policy_is_rechecked_before_every_broker_connection() {
    scenario(async {
        let broker = Broker::bind().await;
        let authority = "localhost:8131";
        let mut proxy = Proxy::start_local(&broker, authority).await;
        let fixture = proxy.local.as_ref().expect("local fixture").clone();
        let granted = &fixture.policies[fixture.granted_layer];
        std::fs::write(&proxy.policy, "localhost\n").expect("matching public policy");

        for (name, mode, local_contents) in [
            ("revoked", "enforce\n", Some("")),
            ("report", "report\n", Some("")),
            ("open", "open\n", Some("")),
            ("invalid", "enforce\n", Some("bad authority\n")),
            ("missing", "enforce\n", None),
        ] {
            std::fs::write(&proxy.mode, mode).expect("replace mode");
            match local_contents {
                Some(contents) => atomic_replace(granted, contents.as_bytes()),
                None => std::fs::remove_file(granted).expect("remove local policy"),
            }
            let response = proxy
                .request(&format!(
                    "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n"
                ))
                .await;
            assert_eq!(status(&response), 403, "{name}");
            assert!(
                timeout(Duration::from_millis(150), broker.listener.accept())
                    .await
                    .is_err(),
                "{name} local decision must not reach the broker"
            );
        }

        atomic_replace(granted, format!("{authority}\n").as_bytes());
        std::fs::write(&fixture.token, "b".repeat(64)).expect("replace mounted token");
        let request = tokio::spawn({
            let address = proxy.address;
            async move {
                request(
                    address,
                    format!(
                        "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await
            }
        });
        let (mut peer, _) = accept(&broker.listener).await;
        assert_eq!(
            String::from_utf8(read_line(&mut peer).await).expect("CONNECT frame"),
            format!("VHRN-BROKER/1 CONNECT {TOKEN} {authority}\n")
        );
        peer.write_all(b"ERR\n").await.expect("reject repaired grant");
        let response = request.await.expect("request task");
        assert_eq!(status(&response), 502);
        assert!(!response.contains(TOKEN));
        assert!(!response.contains(&broker.address().to_string()));
        assert!(
            !std::fs::read_to_string(&proxy.log)
                .unwrap_or_default()
                .contains(TOKEN)
        );
        let _ = proxy
            .child
            .terminate_and_wait("local policy recheck cleanup")
            .await;
        assert!(!proxy.child.raw_diagnostics().contains(TOKEN));
        assert!(
            !proxy
                .child
                .raw_diagnostics()
                .contains(&broker.address().to_string())
        );
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
        .write_all(b"POST http://localhost:8123/path?q=one HTTP/1.1\r\nHost: localhost:8123\r\nX-Ordinary: retained\r\nConnection: X-Nominated, keep-alive\r\nX-Nominated: remove\r\nProxy-Connection: remove\r\nProxy-Authorization: remove\r\nKeep-Alive: timeout=5\r\nTE: trailers\r\nTrailer: X-Trailer\r\nContent-Length: 4\r\n\r\nbody")
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
    for response in [
        b"".as_slice(),
        b"ERR\n".as_slice(),
        b"O".as_slice(),
        b"TOOLONG".as_slice(),
    ] {
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
        assert_eq!(status(&response), 504);
        assert!(!response.contains(TOKEN));
        assert!(!response.contains(&broker.address().to_string()));
        let log = std::fs::read_to_string(&proxy.log).unwrap_or_default();
        assert!(!log.contains(TOKEN));
        assert!(!log.contains(&broker.address().to_string()));
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
async fn local_connect_client_disconnect_before_approval_cancels_broker_work() {
    scenario(async {
        let broker = Broker::bind().await;
        let authority = "localhost:8125";
        let proxy = Proxy::start_local(&broker, authority).await;
        let mut client = TcpStream::connect(proxy.address)
            .await
            .expect("client connect");
        client
            .write_all(b"CONNECT localhost:8125 HTTP/1.1\r\nHost: localhost:8125\r\n\r\n")
            .await
            .expect("CONNECT request");
        let (mut origin, _) = accept(&broker.listener).await;
        assert_eq!(
            String::from_utf8(read_line(&mut origin).await).expect("CONNECT frame"),
            format!("VHRN-BROKER/1 CONNECT {TOKEN} {authority}\n")
        );

        drop(client);

        let mut end = [0_u8; 1];
        assert_eq!(
            timeout(DEADLINE, origin.read(&mut end))
                .await
                .expect("broker cancellation deadline")
                .expect("broker cancellation read"),
            0
        );
        assert!(
            timeout(Duration::from_millis(150), broker.listener.accept())
                .await
                .is_err(),
            "client cancellation must not create another broker connection"
        );
        assert!(
            !std::fs::read_to_string(&proxy.log)
                .unwrap_or_default()
                .contains(TOKEN)
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
        let client_prefix = (0..16 * 1024)
            .map(|index| u8::try_from(index % 251).expect("prefix byte"))
            .collect::<Vec<_>>();
        let broker_prefix = (0..16 * 1024)
            .map(|index| u8::try_from(250 - index % 251).expect("prefix byte"))
            .collect::<Vec<_>>();
        let mut request =
            b"CONNECT localhost:8124 HTTP/1.1\r\nHost: localhost:8124\r\n\r\n".to_vec();
        request.extend_from_slice(&client_prefix);
        client.write_all(&request).await.expect("CONNECT request");
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
        origin.write_all(b"O").await.expect("fragmented approval");
        tokio::task::yield_now().await;
        let mut approval = b"K\n".to_vec();
        approval.extend_from_slice(&broker_prefix);
        origin.write_all(&approval).await.expect("broker approval");
        let response = read_through(&mut client, b"\r\n\r\n").await;
        assert_eq!(response, b"HTTP/1.1 200 Connection Established\r\n\r\n");
        let mut buffered = vec![0_u8; client_prefix.len()];
        timeout(DEADLINE, origin.read_exact(&mut buffered))
            .await
            .expect("buffered relay deadline")
            .expect("buffered relay");
        assert_eq!(buffered, client_prefix);
        let mut broker_buffered = vec![0_u8; broker_prefix.len()];
        timeout(DEADLINE, client.read_exact(&mut broker_buffered))
            .await
            .expect("broker prefix relay deadline")
            .expect("broker prefix relay");
        assert_eq!(broker_buffered, broker_prefix);
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
async fn sigterm_drains_then_forces_an_established_tunnel_without_policy_mounts() {
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
        let started = Instant::now();
        send_sigterm(&proxy.child);
        for path in &proxy.local.as_ref().expect("local fixture").policies {
            std::fs::remove_file(path).expect("remove local policy during drain");
        }
        std::fs::remove_file(&proxy.policy).expect("remove public policy during drain");
        std::fs::remove_file(&proxy.mode).expect("remove mode during drain");
        std::fs::remove_file(&proxy.local.as_ref().expect("local fixture").token)
            .expect("remove token during drain");

        client
            .write_all(b"during-drain")
            .await
            .expect("client remains active during drain");
        let mut drained = [0_u8; 12];
        peer.read_exact(&mut drained)
            .await
            .expect("origin receives active tunnel bytes");
        assert_eq!(&drained, b"during-drain");
        let mut end = [0_u8; 1];
        assert!(
            timeout(Duration::from_secs(1), client.read(&mut end))
                .await
                .is_err(),
            "first signal must not cancel an established tunnel"
        );
        let client_count = timeout(SHUTDOWN_DEADLINE, client.read(&mut end))
            .await
            .expect("client SIGTERM deadline")
            .expect("client SIGTERM read");
        let peer_count = timeout(DEADLINE, peer.read(&mut end))
            .await
            .expect("peer SIGTERM deadline")
            .expect("peer SIGTERM read");
        assert_eq!(client_count, 0);
        assert_eq!(peer_count, 0);
        let elapsed = started.elapsed();
        assert!(elapsed >= Duration::from_secs(5));
        assert!(
            elapsed < Duration::from_secs(6),
            "forced closure must remain anchored to the first signal: {elapsed:?}"
        );
        let exit = proxy
            .child
            .wait_for_exit(SHUTDOWN_DEADLINE, "SIGTERM drain")
            .await;
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
async fn second_sigterm_forces_idempotent_shutdown_and_exits_zero() {
    scenario(async {
        let broker = Broker::bind().await;
        let authority = "localhost:8129";
        let mut proxy = Proxy::start_local(&broker, authority).await;
        let mut client = TcpStream::connect(proxy.address)
            .await
            .expect("client connect");
        client
            .write_all(b"CONNECT localhost:8129 HTTP/1.1\r\nHost: localhost:8129\r\n\r\n")
            .await
            .expect("CONNECT request");
        let mut peer = broker.connect(authority).await;
        let response = read_through(&mut client, b"\r\n\r\n").await;
        assert!(response.starts_with(b"HTTP/1.1 200"));

        let started = Instant::now();
        send_sigterm(&proxy.child);
        tokio::time::sleep(Duration::from_millis(100)).await;
        send_sigterm(&proxy.child);
        let mut end = [0_u8; 1];
        assert_eq!(
            timeout(DEADLINE, client.read(&mut end))
                .await
                .expect("forced client closure")
                .expect("forced client read"),
            0
        );
        assert_eq!(
            timeout(DEADLINE, peer.read(&mut end))
                .await
                .expect("forced peer closure")
                .expect("forced peer read"),
            0
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        let exit = proxy.child.wait_for_exit(DEADLINE, "second SIGTERM").await;
        assert!(
            exit.success(),
            "second SIGTERM should remain graceful; stderr: {}",
            proxy.child.diagnostics()
        );
    })
    .await;
}

#[cfg(unix)]
#[tokio::test]
async fn process_supports_one_hundred_twenty_eight_established_clients() {
    scenario(async {
        let mut proxy = Proxy::start(None).await;
        let mut clients = Vec::with_capacity(128);
        for _ in 0..128 {
            let mut client = TcpStream::connect(proxy.address)
                .await
                .expect("establish client");
            client.write_all(b"G").await.expect("start request head");
            clients.push(client);
        }

        let health = proxy
            .request("GET /healthz HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n")
            .await;
        assert_eq!(status(&health), 200);
        assert_eq!(clients.len(), 128);
        drop(clients);

        send_sigterm(&proxy.child);
        let exit = proxy
            .child
            .wait_for_exit(SHUTDOWN_DEADLINE, "128-client shutdown")
            .await;
        assert!(exit.success());
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
            occupied
                .diagnostics()
                .contains("startup_listener_bind_failed:address_in_use"),
            "occupied listener stderr must include a stable category: {}",
            occupied.diagnostics()
        );
        assert!(!occupied.raw_diagnostics().contains(TOKEN));
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
