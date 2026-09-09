//! HTTP/1 listener supervision with inert typed connector seams.

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinSet;
use tokio::time::{Instant, timeout};
use vhrn_policy::Mode;

use crate::config::{Config, config_from_env, load_broker_token};
use crate::diagnostics::{ResponseDescriptor, direct_response, write_denial};
use crate::policy::{decide_local, decide_public};
use crate::public::PublicConnectorAdapter;
use crate::target::{LocalTarget, PublicTarget, Target, classify};

const MAX_CONNECTIONS: usize = 64;
const MAX_HEADERS: usize = 64;
const MAX_HEADER_BYTES: usize = 16 * 1024;
const DRAIN_TIMEOUT: Duration = Duration::from_millis(700);
const CONNECTION_DRAIN_TIMEOUT: Duration = Duration::from_millis(800);

pub(crate) type BoxFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

pub(crate) mod sealed {
    pub trait Public {}
    pub trait Local {}
}

/// Typed inert seam for a public destination.
pub trait PublicConnector: sealed::Public + Send + Sync + 'static {
    fn http(&self, target: PublicTarget) -> BoxFuture;
    fn connect(&self, target: PublicTarget) -> BoxFuture;
}
/// Typed inert seam for a local destination.
pub trait LocalConnector: sealed::Local + Send + Sync + 'static {
    fn http(&self, target: LocalTarget) -> BoxFuture;
    fn connect(&self, target: LocalTarget) -> BoxFuture;
}

#[derive(Default)]
struct InertConnector;
impl sealed::Public for InertConnector {}
impl sealed::Local for InertConnector {}
impl PublicConnector for InertConnector {
    fn http(&self, _: PublicTarget) -> BoxFuture {
        Box::pin(async {})
    }
    fn connect(&self, _: PublicTarget) -> BoxFuture {
        Box::pin(async {})
    }
}
impl LocalConnector for InertConnector {
    fn http(&self, _: LocalTarget) -> BoxFuture {
        Box::pin(async {})
    }
    fn connect(&self, _: LocalTarget) -> BoxFuture {
        Box::pin(async {})
    }
}

/// Connector pair used by the HTTP shell.
#[derive(Clone)]
pub struct Connectors {
    public: Arc<dyn PublicConnector>,
    local: Arc<dyn LocalConnector>,
}
impl Default for Connectors {
    fn default() -> Self {
        Self::new(Arc::new(InertConnector), Arc::new(InertConnector))
    }
}
impl Connectors {
    pub(crate) fn new(public: Arc<dyn PublicConnector>, local: Arc<dyn LocalConnector>) -> Self {
        Self { public, local }
    }

    fn production() -> Self {
        Self::new(
            Arc::new(PublicConnectorAdapter::system()),
            Arc::new(InertConnector),
        )
    }
}

/// Outcome of a completed listener lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServiceReport {
    pub drained_tasks: usize,
    pub aborted_tasks: usize,
    pub reaped_tasks: usize,
}

/// Starts from the process environment after validating local credentials.
///
/// # Errors
///
/// Returns an error before accepting work for invalid configuration, credentials, or binding.
pub async fn run_from_env(shutdown: watch::Receiver<bool>) -> Result<ServiceReport> {
    let config = config_from_env()?;
    run_with_config(config, shutdown).await
}

async fn run_with_config(config: Config, shutdown: watch::Receiver<bool>) -> Result<ServiceReport> {
    if let Some(local) = &config.local {
        let _ = load_broker_token(local)?; /* Sidecar readiness is inserted here before binding. */
    }
    let listener = bind_listener(&config.listen).await?;
    serve(listener, config, Connectors::production(), shutdown).await
}

/// Binds a configured numeric listener address without name resolution.
///
/// # Errors
///
/// Returns an error for unsupported syntax or listener binding failure.
pub async fn bind_listener(value: &str) -> Result<TcpListener> {
    TcpListener::bind(normalize_listen(value)?)
        .await
        .context("bind proxy listener")
}

/// Serves a prebound listener until shutdown and joins every accepted task.
///
/// # Errors
///
/// Returns an error when accepting from the supplied listener fails.
pub async fn serve(
    listener: TcpListener,
    config: Config,
    connectors: Connectors,
    shutdown: watch::Receiver<bool>,
) -> Result<ServiceReport> {
    serve_with_limit(listener, config, connectors, shutdown, MAX_CONNECTIONS).await
}

async fn serve_with_limit(
    listener: TcpListener,
    config: Config,
    connectors: Connectors,
    mut shutdown: watch::Receiver<bool>,
    limit: usize,
) -> Result<ServiceReport> {
    let permits = Arc::new(Semaphore::new(limit));
    let mut tasks = JoinSet::new();
    let mut reaped = 0;
    loop {
        while tasks.try_join_next().is_some() {
            reaped += 1;
        }
        if *shutdown.borrow() {
            break;
        }
        tokio::select! {
            joined = tasks.join_next(), if !tasks.is_empty() => { if joined.is_some() { reaped += 1; } }
            () = cancelled(&mut shutdown) => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let Ok(permit) = permits.clone().try_acquire_owned() else { drop(stream); continue; };
                    let task_config = config.clone(); let task_connectors = connectors.clone(); let task_shutdown = shutdown.clone();
                    tasks.spawn(async move { let _permit = permit; serve_connection(stream, task_config, task_connectors, task_shutdown).await; });
                }
                Err(error) => { let _ = drain(&mut tasks, reaped).await; return Err(anyhow::Error::new(error).context("accept proxy connection")); },
            }
        }
    }
    Ok(drain(&mut tasks, reaped).await)
}

async fn drain(tasks: &mut JoinSet<()>, reaped_tasks: usize) -> ServiceReport {
    let deadline = Instant::now() + DRAIN_TIMEOUT;
    let mut drained = 0;
    while !tasks.is_empty() {
        match timeout(
            deadline.saturating_duration_since(Instant::now()),
            tasks.join_next(),
        )
        .await
        {
            Ok(Some(_)) => drained += 1,
            Ok(None) | Err(_) => break,
        }
    }
    let aborted = tasks.len();
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    ServiceReport {
        drained_tasks: drained,
        aborted_tasks: aborted,
        reaped_tasks,
    }
}

async fn serve_connection(
    stream: TcpStream,
    config: Config,
    connectors: Connectors,
    mut shutdown: watch::Receiver<bool>,
) {
    if *shutdown.borrow() {
        return;
    }
    let service = service_fn(move |request| handle(request, config.clone(), connectors.clone()));
    let mut connection = Box::pin(
        http1::Builder::new()
            .max_headers(MAX_HEADERS)
            .max_buf_size(MAX_HEADER_BYTES)
            .serve_connection(TokioIo::new(stream), service),
    );
    tokio::select! {
        _ = &mut connection => {}
        () = cancelled(&mut shutdown) => { connection.as_mut().graceful_shutdown(); let _ = timeout(CONNECTION_DRAIN_TIMEOUT, &mut connection).await; }
    }
}

async fn cancelled(shutdown: &mut watch::Receiver<bool>) {
    while !*shutdown.borrow() {
        if shutdown.changed().await.is_err() {
            return;
        }
    }
}

async fn handle(
    request: Request<Incoming>,
    config: Config,
    connectors: Connectors,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    Ok(route(
        request.method(),
        &request.uri().to_string(),
        config,
        connectors,
    )
    .await)
}

async fn route(
    method: &hyper::Method,
    uri: &str,
    config: Config,
    connectors: Connectors,
) -> Response<Full<Bytes>> {
    let target = classify(method.as_str(), uri);
    match target {
        Target::Direct(path) if method == hyper::Method::GET => {
            descriptor(&direct_response(&path, effective_mode(&config)))
        }
        Target::Direct(_) => response(StatusCode::NOT_FOUND, None, "404 page not found\n"),
        Target::Malformed => response(StatusCode::BAD_REQUEST, None, "bad request\n"),
        Target::PublicHttp(target) => {
            let decision = decide_public(&config.allowlists, &config.mode_file, target.host());
            if decision.allowed {
                if decision.record_denial {
                    record(&config, target.authority());
                }
                connectors.public.http(target).await;
                response(StatusCode::BAD_GATEWAY, None, "bad gateway\n")
            } else {
                record(&config, target.authority());
                response(StatusCode::FORBIDDEN, None, "forbidden\n")
            }
        }
        Target::PublicConnect(target) => {
            let decision = decide_public(&config.allowlists, &config.mode_file, target.host());
            if decision.allowed {
                if decision.record_denial {
                    record(&config, target.authority());
                }
                connectors.public.connect(target).await;
                response(StatusCode::BAD_GATEWAY, None, "bad gateway\n")
            } else {
                record(&config, target.authority());
                response(StatusCode::FORBIDDEN, None, "forbidden\n")
            }
        }
        Target::LocalHttp(target) => {
            if config.local.as_ref().is_some_and(|local| {
                decide_local(&local.policy_paths, target.canonical_authority())
            }) {
                connectors.local.http(target).await;
                response(StatusCode::BAD_GATEWAY, None, "bad gateway\n")
            } else {
                record(&config, target.authority());
                response(StatusCode::FORBIDDEN, None, "forbidden\n")
            }
        }
        Target::LocalConnect(target) => {
            if config.local.as_ref().is_some_and(|local| {
                decide_local(&local.policy_paths, target.canonical_authority())
            }) {
                connectors.local.connect(target).await;
                response(StatusCode::BAD_GATEWAY, None, "bad gateway\n")
            } else {
                record(&config, target.authority());
                response(StatusCode::FORBIDDEN, None, "forbidden\n")
            }
        }
    }
}

fn effective_mode(config: &Config) -> Mode {
    decide_public(&config.allowlists, &config.mode_file, "status.invalid").mode
}
fn descriptor(value: &ResponseDescriptor) -> Response<Full<Bytes>> {
    response(
        StatusCode::from_u16(value.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        value.content_type,
        &value.body,
    )
}
fn response(status: StatusCode, content_type: Option<&str>, body: &str) -> Response<Full<Bytes>> {
    let mut builder = Response::builder().status(status);
    if let Some(content_type) = content_type {
        builder = builder.header("content-type", content_type);
    }
    builder
        .body(Full::new(Bytes::copy_from_slice(body.as_bytes())))
        .expect("fixed response is valid")
}
fn record(config: &Config, destination: &str) {
    let _ = write_denial(config.deny_log.as_deref(), &rfc3339_now(), destination);
}
fn rfc3339_now() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |value| value.as_secs());
    let (year, month, day, hour, minute, second) = civil_time(seconds);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}
fn civil_time(seconds: u64) -> (i64, i64, i64, u64, u64, u64) {
    let days = i64::try_from(seconds / 86_400).unwrap_or(i64::MAX);
    let time = seconds % 86_400;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    (
        year + i64::from(month <= 2),
        month,
        day,
        time / 3_600,
        time / 60 % 60,
        time % 60,
    )
}
fn normalize_listen(value: &str) -> Result<SocketAddr> {
    if let Some(port) = value.strip_prefix(':') {
        let port = port.parse().context("invalid listen port")?;
        return Ok(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port));
    }
    value
        .parse()
        .map_err(|_| anyhow::anyhow!("listen address must be numeric"))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use http_body_util::BodyExt;
    use tempfile::tempdir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    use tokio::sync::{Notify, watch};
    use tokio::time::{Duration, timeout};

    use super::*;
    use crate::public::{
        BoxStream, DialFuture, NumericDialer, PublicConnectorAdapter, ResolveFuture, Resolver,
    };

    struct Counts {
        public_http: AtomicUsize,
        public_connect: AtomicUsize,
        local_http: AtomicUsize,
        local_connect: AtomicUsize,
    }
    impl Counts {
        fn value(counter: &AtomicUsize) -> usize {
            counter.load(Ordering::SeqCst)
        }
        fn values(&self) -> (usize, usize, usize, usize) {
            (
                Self::value(&self.public_http),
                Self::value(&self.public_connect),
                Self::value(&self.local_http),
                Self::value(&self.local_connect),
            )
        }
    }
    impl sealed::Public for Counts {}
    impl sealed::Local for Counts {}
    impl PublicConnector for Counts {
        fn http(&self, _: PublicTarget) -> BoxFuture {
            self.public_http.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {})
        }
        fn connect(&self, _: PublicTarget) -> BoxFuture {
            self.public_connect.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {})
        }
    }
    impl LocalConnector for Counts {
        fn http(&self, _: LocalTarget) -> BoxFuture {
            self.local_http.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {})
        }
        fn connect(&self, _: LocalTarget) -> BoxFuture {
            self.local_connect.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {})
        }
    }
    struct PendingPublic {
        entered: Notify,
        calls: AtomicUsize,
    }
    struct AdapterResolver(Mutex<Vec<(String, u16)>>);
    impl Resolver for AdapterResolver {
        fn resolve(&self, host: String, port: u16) -> ResolveFuture {
            self.0.lock().unwrap().push((host, port));
            Box::pin(async { Ok(vec!["8.8.8.8".parse().unwrap()]) })
        }
    }
    struct AdapterDialer(Mutex<Vec<SocketAddr>>);
    impl NumericDialer for AdapterDialer {
        fn dial(&self, address: SocketAddr) -> DialFuture {
            self.0.lock().unwrap().push(address);
            let (stream, _) = tokio::io::duplex(1);
            Box::pin(async move { Ok(Box::new(stream) as BoxStream) })
        }
    }
    impl sealed::Public for PendingPublic {}
    impl PublicConnector for PendingPublic {
        fn http(&self, _: PublicTarget) -> BoxFuture {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.entered.notify_one();
            Box::pin(std::future::pending())
        }
        fn connect(&self, _: PublicTarget) -> BoxFuture {
            Box::pin(async {})
        }
    }
    fn config(directory: &std::path::Path, mode: &str) -> Config {
        let layer = directory.join("public");
        let mode_file = directory.join("mode");
        std::fs::write(&layer, "allowed.example\n").unwrap();
        std::fs::write(&mode_file, format!("{mode}\n")).unwrap();
        Config {
            allowlists: vec![layer.display().to_string()],
            mode_file: mode_file.display().to_string(),
            listen: "127.0.0.1:0".to_owned(),
            deny_log: Some(directory.join("deny").display().to_string()),
            local: None,
        }
    }
    fn local_config(mut value: Config, directory: &std::path::Path) -> Config {
        let paths = ["local-one", "local-two", "local-three"].map(|name| {
            let path = directory.join(name);
            std::fs::write(&path, "localhost:8000\n").unwrap();
            path.display().to_string()
        });
        value.local = Some(crate::config::LocalConfig {
            policy_paths: paths,
            broker_addr: "127.0.0.1:1".parse().unwrap(),
            token_file: directory.join("token").display().to_string(),
        });
        value
    }
    fn assert_denial(record: &str, authority: &str) {
        let (timestamp, found_authority) = record.trim_end_matches('\n').split_once('\t').unwrap();
        assert_eq!(found_authority, authority);
        assert_eq!(timestamp.len(), 20);
        assert!(
            timestamp
                .bytes()
                .enumerate()
                .all(|(index, byte)| match index {
                    4 | 7 => byte == b'-',
                    10 => byte == b'T',
                    13 | 16 => byte == b':',
                    19 => byte == b'Z',
                    _ => byte.is_ascii_digit(),
                })
        );
    }
    async fn raw_request(address: SocketAddr, request: &[u8]) -> Vec<u8> {
        let mut stream = timeout(Duration::from_millis(500), TcpStream::connect(address))
            .await
            .unwrap()
            .unwrap();
        timeout(Duration::from_millis(500), stream.write_all(request))
            .await
            .unwrap()
            .unwrap();
        let mut response = Vec::new();
        timeout(
            Duration::from_millis(500),
            stream.read_to_end(&mut response),
        )
        .await
        .unwrap()
        .unwrap();
        response
    }
    async fn route_status(
        method: &hyper::Method,
        uri: &str,
        config: Config,
        connectors: Connectors,
    ) -> StatusCode {
        route(method, uri, config, connectors).await.status()
    }
    fn assert_miss_records(directory: &std::path::Path) {
        let records = std::fs::read_to_string(directory.join("deny")).unwrap();
        let records: Vec<_> = records.lines().collect();
        assert_eq!(records.len(), 2);
        assert_denial(&format!("{}\n", records[0]), "miss.example");
        assert_denial(&format!("{}\n", records[1]), "miss.example");
        assert!(!records.iter().any(|record| record.contains("token")));
    }
    #[tokio::test]
    async fn listen_and_startup_validation() {
        assert_eq!(normalize_listen(":8080").unwrap().port(), 8080);
        assert!(normalize_listen("127.0.0.1:0").is_ok());
        assert!(normalize_listen("[::1]:0").is_ok());
        assert!(normalize_listen("localhost:80").is_err());
        assert!(normalize_listen("bad").is_err());
        let listener = bind_listener("127.0.0.1:0").await.unwrap();
        assert!(
            bind_listener(&listener.local_addr().unwrap().to_string())
                .await
                .is_err()
        );
        let directory = tempdir().unwrap();
        let mut value = config(directory.path(), "enforce");
        value.listen = listener.local_addr().unwrap().to_string();
        value.local = Some(crate::config::LocalConfig {
            policy_paths: ["a".to_owned(), "b".to_owned(), "c".to_owned()],
            broker_addr: "127.0.0.1:1".parse().unwrap(),
            token_file: directory.path().join("missing").display().to_string(),
        });
        let (_, receiver) = watch::channel(false);
        assert!(
            run_with_config(value, receiver)
                .await
                .unwrap_err()
                .to_string()
                .contains("broker token")
        );
    }
    #[tokio::test]
    async fn direct_http_contract_over_listener() {
        let directory = tempdir().unwrap();
        let listener = bind_listener("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown, receiver) = watch::channel(false);
        let task = tokio::spawn(serve(
            listener,
            config(directory.path(), "report"),
            Connectors::default(),
            receiver,
        ));
        let mut stream = TcpStream::connect(address).await.unwrap();
        stream
            .write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut data = Vec::new();
        timeout(Duration::from_secs(1), stream.read_to_end(&mut data))
            .await
            .unwrap()
            .unwrap();
        assert!(data.starts_with(b"HTTP/1.1 200 OK\r\n"));
        assert!(data.ends_with(b"\r\nok\n"));
        shutdown.send(true).unwrap();
        let report = timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(report.aborted_tasks, 0);
        let status = route(
            &hyper::Method::GET,
            "/__status",
            config(directory.path(), "report"),
            Connectors::default(),
        )
        .await;
        assert_eq!(status.headers()["content-type"], "application/json");
        assert_eq!(
            status.into_body().collect().await.unwrap().to_bytes(),
            &b"{\"mode\":\"report\"}\n"[..]
        );
        assert_eq!(
            route(
                &hyper::Method::POST,
                "/healthz",
                config(directory.path(), "report"),
                Connectors::default()
            )
            .await
            .status(),
            StatusCode::NOT_FOUND
        );
    }
    #[tokio::test]
    async fn typed_routes_and_policy_diagnostics() {
        let directory = tempdir().unwrap();
        let counts = Arc::new(Counts {
            public_http: AtomicUsize::new(0),
            public_connect: AtomicUsize::new(0),
            local_http: AtomicUsize::new(0),
            local_connect: AtomicUsize::new(0),
        });
        let connectors = Connectors::new(counts.clone(), counts.clone());
        let enforce = config(directory.path(), "enforce");
        assert_eq!(
            route_status(
                &hyper::Method::GET,
                "http://miss.example/",
                enforce.clone(),
                connectors.clone()
            )
            .await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(counts.values(), (0, 0, 0, 0));
        let report = config(directory.path(), "report");
        assert_eq!(
            route_status(
                &hyper::Method::GET,
                "http://miss.example/",
                report.clone(),
                connectors.clone()
            )
            .await,
            StatusCode::BAD_GATEWAY
        );
        assert_eq!(Counts::value(&counts.public_http), 1);
        assert_eq!(Counts::value(&counts.public_connect), 0);
        let open = config(directory.path(), "open");
        assert_eq!(
            route_status(
                &hyper::Method::GET,
                "http://miss.example/",
                open,
                connectors.clone()
            )
            .await,
            StatusCode::BAD_GATEWAY
        );
        assert_eq!(Counts::value(&counts.public_http), 2);
        assert_eq!(Counts::value(&counts.public_connect), 0);
        assert_eq!(
            route_status(
                &hyper::Method::CONNECT,
                "allowed.example:443",
                config(directory.path(), "enforce"),
                connectors.clone()
            )
            .await,
            StatusCode::BAD_GATEWAY
        );
        assert_eq!(Counts::value(&counts.public_connect), 1);
        assert_eq!(Counts::value(&counts.public_http), 2);
        let local = local_config(config(directory.path(), "enforce"), directory.path());
        assert_eq!(
            route_status(
                &hyper::Method::GET,
                "http://LOCALHOST:08000/path",
                local.clone(),
                connectors.clone()
            )
            .await,
            StatusCode::BAD_GATEWAY
        );
        assert_eq!(Counts::value(&counts.local_http), 1);
        assert_eq!(Counts::value(&counts.local_connect), 0);
        assert_eq!(
            route_status(
                &hyper::Method::CONNECT,
                "LOCALHOST:08000",
                local.clone(),
                connectors.clone()
            )
            .await,
            StatusCode::BAD_GATEWAY
        );
        assert_eq!(Counts::value(&counts.local_http), 1);
        assert_eq!(Counts::value(&counts.local_connect), 1);
        assert_eq!(
            route_status(
                &hyper::Method::GET,
                "not-a-target",
                local,
                connectors.clone()
            )
            .await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(counts.values(), (2, 1, 1, 1));
        assert_miss_records(directory.path());
    }

    #[tokio::test]
    async fn public_adapter_runs_only_after_public_policy_and_never_for_local_targets() {
        let directory = tempdir().unwrap();
        let resolver = Arc::new(AdapterResolver(Mutex::new(Vec::new())));
        let dialer = Arc::new(AdapterDialer(Mutex::new(Vec::new())));
        let connectors = Connectors::new(
            Arc::new(PublicConnectorAdapter::new(
                resolver.clone(),
                dialer.clone(),
            )),
            Arc::new(InertConnector),
        );
        let enforce = config(directory.path(), "enforce");
        assert_eq!(
            route_status(
                &hyper::Method::GET,
                "http://miss.example/",
                enforce.clone(),
                connectors.clone(),
            )
            .await,
            StatusCode::FORBIDDEN
        );
        assert!(resolver.0.lock().unwrap().is_empty());
        assert!(dialer.0.lock().unwrap().is_empty());
        for (method, uri) in [
            (&hyper::Method::GET, "http://allowed.example/"),
            (&hyper::Method::CONNECT, "allowed.example:443"),
        ] {
            assert_eq!(
                route_status(method, uri, enforce.clone(), connectors.clone()).await,
                StatusCode::BAD_GATEWAY
            );
        }
        let local = local_config(enforce, directory.path());
        assert_eq!(
            route_status(
                &hyper::Method::GET,
                "http://localhost:8000/",
                local,
                connectors,
            )
            .await,
            StatusCode::BAD_GATEWAY
        );
        assert_eq!(resolver.0.lock().unwrap().len(), 2);
        assert_eq!(dialer.0.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn completed_connections_are_reaped_before_shutdown() {
        let directory = tempdir().unwrap();
        let listener = bind_listener("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown, receiver) = watch::channel(false);
        let task = tokio::spawn(serve_with_limit(
            listener,
            config(directory.path(), "enforce"),
            Connectors::default(),
            receiver,
            2,
        ));
        for _ in 0..3 {
            let response = raw_request(
                address,
                b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
            )
            .await;
            assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
        }
        timeout(Duration::from_millis(500), tokio::task::yield_now())
            .await
            .unwrap();
        shutdown.send(true).unwrap();
        let report = timeout(Duration::from_millis(900), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(report.aborted_tasks, 0);
        assert_eq!(report.drained_tasks, 0);
        assert_eq!(report.reaped_tasks, 3);
    }

    #[tokio::test]
    async fn initially_cancelled_service_has_no_tasks() {
        let directory = tempdir().unwrap();
        let listener = bind_listener("127.0.0.1:0").await.unwrap();
        let (_, shutdown) = watch::channel(true);
        let report = timeout(
            Duration::from_millis(500),
            serve_with_limit(
                listener,
                config(directory.path(), "enforce"),
                Connectors::default(),
                shutdown,
                1,
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(report.drained_tasks, 0);
        assert_eq!(report.aborted_tasks, 0);
        assert_eq!(report.reaped_tasks, 0);
    }

    #[tokio::test]
    async fn shutdown_closes_idle_connection_within_drain_deadline() {
        let directory = tempdir().unwrap();
        let listener = bind_listener("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown, receiver) = watch::channel(false);
        let task = tokio::spawn(serve_with_limit(
            listener,
            config(directory.path(), "enforce"),
            Connectors::default(),
            receiver,
            1,
        ));
        let mut stream = timeout(Duration::from_millis(500), TcpStream::connect(address))
            .await
            .unwrap()
            .unwrap();
        timeout(
            Duration::from_millis(100),
            tokio::time::sleep(Duration::from_millis(20)),
        )
        .await
        .unwrap();
        shutdown.send(true).unwrap();
        let mut byte = [0];
        assert!(matches!(
            timeout(Duration::from_millis(900), stream.read(&mut byte))
                .await
                .unwrap(),
            Ok(0) | Err(_)
        ));
        let report = timeout(Duration::from_millis(900), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(
            report.drained_tasks + report.reaped_tasks <= 1,
            "shutdown may race accept; at most one task can be drained or reaped"
        );
        assert_eq!(report.aborted_tasks, 0);
    }

    #[tokio::test]
    async fn overload_drops_excess_and_aborts_pending_handler() {
        let directory = tempdir().unwrap();
        let pending = Arc::new(PendingPublic {
            entered: Notify::new(),
            calls: AtomicUsize::new(0),
        });
        let local = Arc::new(Counts {
            public_http: AtomicUsize::new(0),
            public_connect: AtomicUsize::new(0),
            local_http: AtomicUsize::new(0),
            local_connect: AtomicUsize::new(0),
        });
        let listener = bind_listener("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown, receiver) = watch::channel(false);
        let task = tokio::spawn(serve_with_limit(
            listener,
            config(directory.path(), "enforce"),
            Connectors::new(pending.clone(), local),
            receiver,
            1,
        ));
        let mut first = timeout(Duration::from_millis(500), TcpStream::connect(address))
            .await
            .unwrap()
            .unwrap();
        timeout(
            Duration::from_millis(500),
            first.write_all(
                b"GET http://allowed.example/ HTTP/1.1\r\nHost: allowed.example\r\n\r\n",
            ),
        )
        .await
        .unwrap()
        .unwrap();
        timeout(Duration::from_millis(500), pending.entered.notified())
            .await
            .unwrap();
        let mut excess = timeout(Duration::from_millis(500), TcpStream::connect(address))
            .await
            .unwrap()
            .unwrap();
        let mut byte = [0];
        let result = timeout(Duration::from_millis(500), excess.read(&mut byte))
            .await
            .unwrap();
        assert!(matches!(result, Ok(0) | Err(_)));
        assert_eq!(pending.calls.load(Ordering::SeqCst), 1);
        shutdown.send(true).unwrap();
        let report = timeout(Duration::from_millis(950), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(report.aborted_tasks, 1);
        drop(first);
    }
}
