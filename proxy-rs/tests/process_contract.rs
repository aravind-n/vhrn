//! Black-box process checks for the proxy consumer contract.
//!
//! The harness deliberately uses only the executable's environment and TCP
//! interface, so `VHRN_PROXY_TEST_BIN` can point it at another compatible
//! executable.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;
use std::{future::Future, time::Instant};

use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{sleep, timeout};

const DEADLINE: Duration = Duration::from_secs(3);
const SCENARIO_DEADLINE: Duration = Duration::from_secs(20);
const TOKEN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

struct Proxy {
    _temp: TempDir,
    child: Child,
    address: SocketAddr,
    policy: PathBuf,
    mode: PathBuf,
    log: PathBuf,
    local_policies: Option<[PathBuf; 3]>,
    local_grant: Option<usize>,
}

struct ManagedChild(Child);

impl std::ops::Deref for ManagedChild {
    type Target = Child;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for ManagedChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        if self.0.try_wait().expect("check child status").is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
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
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let (local_policies, local_grant) = if let Some((broker, authority, grant)) = local {
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
            command
                .env("VHRN_LOOPBACK_ALLOWLISTS", join_paths(&paths))
                .env("VHRN_BROKER_ADDR", broker)
                .env("VHRN_BROKER_TOKEN_FILE", token);
            (
                Some(paths.try_into().expect("three local policies")),
                Some(grant),
            )
        } else {
            (None, None)
        };
        let child = command.spawn().expect("start proxy executable");
        let proxy = Self {
            _temp: temp,
            child,
            address,
            policy: public_policies[3].clone(),
            mode,
            log,
            local_policies,
            local_grant,
        };
        proxy.ready().await;
        proxy
    }

    async fn ready(&self) {
        for _ in 0..30 {
            if TcpStream::connect(self.address).await.is_ok() {
                return;
            }
            sleep(Duration::from_millis(25)).await;
        }
        panic!("proxy did not accept work before deadline");
    }

    async fn request(&self, bytes: &str) -> String {
        request(self.address, bytes.as_bytes()).await
    }

    async fn start_local(broker: SocketAddr, authority: &str) -> Self {
        Self::start_local_at(broker, authority, 0).await
    }

    async fn start_local_at(broker: SocketAddr, authority: &str, grant: usize) -> Self {
        Self::start(Some((broker.to_string(), authority, grant))).await
    }

    async fn start_local_hostname(broker: SocketAddr, authority: &str) -> Self {
        Self::start(Some((format!("localhost:{}", broker.port()), authority, 0))).await
    }

    fn revoke_local_grant(&self) {
        let paths = self.local_policies.as_ref().expect("local policies");
        std::fs::write(&paths[self.local_grant.expect("local grant")], "")
            .expect("replace local grant");
    }
}

fn proxy_command() -> Command {
    let mut command = Command::new(proxy_bin());
    for name in [
        "VHRN_ALLOWLISTS",
        "VHRN_ALLOWLIST",
        "VHRN_MODE_FILE",
        "VHRN_PROXY_LISTEN",
        "VHRN_DENY_LOG",
        "VHRN_LOOPBACK_ALLOWLISTS",
        "VHRN_BROKER_ADDR",
        "VHRN_BROKER_TOKEN_FILE",
    ] {
        command.env_remove(name);
    }
    command
}

#[test]
fn proxy_command_removes_every_proxy_environment_variable() {
    let command = proxy_command();
    for name in [
        "VHRN_ALLOWLISTS",
        "VHRN_ALLOWLIST",
        "VHRN_MODE_FILE",
        "VHRN_PROXY_LISTEN",
        "VHRN_DENY_LOG",
        "VHRN_LOOPBACK_ALLOWLISTS",
        "VHRN_BROKER_ADDR",
        "VHRN_BROKER_TOKEN_FILE",
    ] {
        assert!(
            command
                .get_envs()
                .any(|(key, value)| key == name && value.is_none()),
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

    async fn ready(&self) {
        let (mut stream, _) = accept(&self.listener).await;
        assert_eq!(
            String::from_utf8(read_line(&mut stream).await).expect("ready text"),
            format!("VHRN-BROKER/1 READY {TOKEN}\n")
        );
        stream.write_all(b"OK\n").await.expect("ready response");
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

impl Drop for Proxy {
    fn drop(&mut self) {
        if self.child.try_wait().expect("check proxy status").is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn proxy_bin() -> PathBuf {
    if let Some(path) = std::env::var_os("VHRN_PROXY_TEST_BIN") {
        return path.into();
    }
    PathBuf::from(env!("CARGO_BIN_EXE_vhrn-proxy"))
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

async fn accept(listener: &TcpListener) -> (TcpStream, SocketAddr) {
    timeout(DEADLINE, listener.accept())
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
    read_line_with_timeout(stream, DEADLINE)
        .await
        .expect("broker frame deadline")
}

async fn read_line_with_timeout(
    stream: &mut TcpStream,
    frame_deadline: Duration,
) -> Result<Vec<u8>, tokio::time::error::Elapsed> {
    timeout(frame_deadline, async {
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
}

#[tokio::test]
async fn partial_broker_frame_obeys_the_injected_whole_frame_deadline() {
    scenario(async {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("broker listener");
        let address = listener.local_addr().expect("broker address");
        let writer = tokio::spawn(async move {
            let mut stream = TcpStream::connect(address).await.expect("broker connect");
            stream.write_all(b"OK").await.expect("partial broker frame");
            sleep(Duration::from_secs(1)).await;
        });
        let (mut peer, _) = accept(&listener).await;
        let frame_deadline = Duration::from_millis(75);
        let started = Instant::now();
        assert!(
            read_line_with_timeout(&mut peer, frame_deadline)
                .await
                .is_err(),
            "a partial broker frame must time out before its peer closes"
        );
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "the frame deadline must bound the complete read"
        );
        writer.await.expect("partial broker peer");
    })
    .await;
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
        let address = broker.address();
        let ready = tokio::spawn(async move { broker.ready().await });
        let proxy = Proxy::start_local(address, "localhost:80").await;
        ready.await.expect("broker ready task");
        assert_eq!(
            status(
                &proxy
                    .request("GET /healthz HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n")
                    .await
            ),
            200
        );

        let directory = tempfile::tempdir().expect("temporary directory");
        let mut child = ManagedChild(
            proxy_command()
                .env("VHRN_PROXY_LISTEN", "127.0.0.1:0")
                .env("VHRN_BROKER_ADDR", "127.0.0.1:1")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("start invalid proxy"),
        );
        let result = timeout(DEADLINE, async {
            loop {
                if let Some(exit) = child.try_wait().expect("status") {
                    break exit;
                }
                sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("invalid startup deadline");
        assert!(
            !result.success(),
            "partial local configuration must fail startup"
        );
        drop(directory);
    })
    .await;
}

#[tokio::test]
async fn each_local_policy_layer_can_independently_grant_through_the_broker() {
    scenario(async {
    let authority = "localhost:8122";
    for grant in 0..3 {
        let broker = Broker::bind().await;
        let broker_address = broker.address();
        let ready_broker = broker.clone();
        let ready = tokio::spawn(async move { ready_broker.ready().await });
        let proxy = Proxy::start_local_at(broker_address, authority, grant).await;
        ready.await.expect("ready task");
        let request_task = tokio::spawn({
            let address = proxy.address;
            async move {
                request(
                    address,
                    b"GET http://localhost:8122/grant HTTP/1.1\r\nHost: localhost:8122\r\nConnection: close\r\n\r\n",
                )
                .await
            }
        });
        let mut origin = broker.connect(authority).await;
        let _ = read_through(&mut origin, b"\r\n\r\n").await;
        origin
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
            .await
            .expect("broker response");
        assert_eq!(status(&request_task.await.expect("request task")), 200);
    }
    })
    .await;
}

#[tokio::test]
async fn local_http_forwards_and_streams_through_authenticated_broker() {
    scenario(async {
    let broker = Broker::bind().await;
    let authority = "localhost:8123";
    let broker_address = broker.address();
    let ready_broker = broker.clone();
    let ready = tokio::spawn(async move { ready_broker.ready().await });
    let proxy = Proxy::start_local_hostname(broker_address, authority).await;
    ready.await.expect("ready task");

    let mut client = TcpStream::connect(proxy.address)
        .await
        .expect("client connect");
    client
        .write_all(b"POST http://localhost:8123/path?q=one HTTP/1.1\r\nHost: localhost:8123\r\nX-Ordinary: retained\r\nProxy-Connection: remove\r\nProxy-Authorization: remove\r\nContent-Length: 4\r\n\r\nbody")
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
    let normalized_headers = headers.to_ascii_lowercase();
    assert!(normalized_headers.contains("x-ordinary: retained\r\n"));
    assert!(
        !normalized_headers.contains("proxy-connection")
            && !normalized_headers.contains("proxy-authorization")
    );
    let mut body = [0_u8; 4];
    origin.read_exact(&mut body).await.expect("origin body");
    assert_eq!(&body, b"body");
    origin
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\none")
        .await
        .expect("first response bytes");
    let response_headers = read_through(&mut client, b"\r\n\r\n").await;
    assert!(
        String::from_utf8(response_headers)
            .expect("response headers")
            .starts_with("HTTP/1.1 200")
    );
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
        let broker_address = broker.address();
        let ready_broker = broker.clone();
        let ready = tokio::spawn(async move { ready_broker.ready().await });
        let proxy = Proxy::start_local(broker_address, authority).await;
        ready.await.expect("ready task");
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
        let broker_address = broker.address();
        let ready_broker = broker.clone();
        let ready = tokio::spawn(async move { ready_broker.ready().await });
        let proxy = Proxy::start_local(broker_address, authority).await;
        ready.await.expect("ready task");
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
        let broker_address = broker.address();
        let ready_broker = broker.clone();
        let ready = tokio::spawn(async move { ready_broker.ready().await });
        let proxy = Proxy::start_local(broker_address, authority).await;
        ready.await.expect("ready task");
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
        let broker_address = broker.address();
        let ready_broker = broker.clone();
        let ready = tokio::spawn(async move { ready_broker.ready().await });
        let proxy = Proxy::start_local(broker_address, authority).await;
        ready.await.expect("ready task");
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

#[tokio::test]
async fn sigterm_closes_an_established_local_tunnel() {
    scenario(async {
        let broker = Broker::bind().await;
        let authority = "localhost:8128";
        let broker_address = broker.address();
        let ready_broker = broker.clone();
        let ready = tokio::spawn(async move { ready_broker.ready().await });
        let mut proxy = Proxy::start_local(broker_address, authority).await;
        ready.await.expect("ready task");
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
        #[cfg(unix)]
        assert!(
            Command::new("kill")
                .arg("-TERM")
                .arg(proxy.child.id().to_string())
                .status()
                .expect("send SIGTERM")
                .success()
        );
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
        let exit = timeout(Duration::from_secs(1), async {
            loop {
                if let Some(exit) = proxy.child.try_wait().expect("proxy status") {
                    break exit;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("SIGTERM exit deadline");
        assert!(!exit.success());
    })
    .await;
}

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
        let mut occupied = ManagedChild(
            proxy_command()
                .env("VHRN_ALLOWLISTS", join_paths(&policies))
                .env("VHRN_MODE_FILE", mode)
                .env("VHRN_PROXY_LISTEN", address.to_string())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("start occupied proxy"),
        );
        let exit = timeout(DEADLINE, async {
            loop {
                if let Some(exit) = occupied.try_wait().expect("status") {
                    break exit;
                }
                sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("occupied startup deadline");
        assert!(!exit.success());
        drop(listener);

        let mut proxy = Proxy::start(None).await;
        #[cfg(unix)]
        {
            let status = Command::new("kill")
                .arg("-TERM")
                .arg(proxy.child.id().to_string())
                .status()
                .expect("send SIGTERM");
            assert!(status.success());
        }
        let exit = timeout(Duration::from_secs(1), async {
            loop {
                if let Some(exit) = proxy.child.try_wait().expect("status") {
                    break exit;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("SIGTERM deadline");
        assert!(!exit.success());
    })
    .await;
}
