//! HTTP/1 listener supervision with inert typed connector seams.

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::{Body, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{HeaderMap, Request, Response, StatusCode};
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
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
const BODY_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) type BoxFuture = Pin<Box<dyn Future<Output = ()> + Send>>;
pub(crate) type PublicHttpFuture<'a> =
    Pin<Box<dyn Future<Output = anyhow::Result<PublicResponse>> + Send + 'a>>;
pub(crate) type PublicConnectFuture<'a> =
    Pin<Box<dyn Future<Output = anyhow::Result<crate::public::BoxStream>> + Send + 'a>>;

pub struct PublicResponse {
    pub(crate) status: StatusCode,
    pub(crate) headers: HeaderMap,
    pub(crate) body: Bytes,
}

pub(crate) mod sealed {
    pub trait Public {}
    pub trait Local {}
}

/// Typed inert seam for a public destination.
pub(crate) trait PublicConnector: sealed::Public + Send + Sync + 'static {
    fn http(&self, target: PublicTarget, request: Request<Full<Bytes>>) -> PublicHttpFuture<'_>;
    fn connect(&self, target: PublicTarget) -> PublicConnectFuture<'_>;
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
    fn http(&self, _: PublicTarget, _: Request<Full<Bytes>>) -> PublicHttpFuture<'_> {
        Box::pin(async { anyhow::bail!("public connector unavailable") })
    }
    fn connect(&self, _: PublicTarget) -> PublicConnectFuture<'_> {
        Box::pin(async { anyhow::bail!("public connector unavailable") })
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
    let tunnels = Arc::new(tokio::sync::Mutex::new(JoinSet::new()));
    let service_tunnels = tunnels.clone();
    let service_shutdown = shutdown.clone();
    let service = service_fn(move |request| {
        handle(
            request,
            config.clone(),
            connectors.clone(),
            service_tunnels.clone(),
            service_shutdown.clone(),
        )
    });
    let mut connection = Box::pin(
        http1::Builder::new()
            .max_headers(MAX_HEADERS)
            .max_buf_size(MAX_HEADER_BYTES)
            .serve_connection(TokioIo::new(stream), service)
            .with_upgrades(),
    );
    let shutting_down = tokio::select! {
        _ = &mut connection => false,
        () = cancelled(&mut shutdown) => {
            connection.as_mut().graceful_shutdown();
            let _ = timeout(CONNECTION_DRAIN_TIMEOUT, &mut connection).await;
            true
        }
    };
    wait_for_tunnels(&tunnels, shutdown, shutting_down).await;
}

async fn wait_for_tunnels(
    tunnels: &tokio::sync::Mutex<JoinSet<()>>,
    mut shutdown: watch::Receiver<bool>,
    shutting_down: bool,
) {
    let mut tunnels = tunnels.lock().await;
    if !shutting_down && !*shutdown.borrow() {
        while !tunnels.is_empty() {
            tokio::select! {
                _ = tunnels.join_next() => {}
                () = cancelled(&mut shutdown) => break,
            }
        }
    }
    if tunnels.is_empty() {
        return;
    }
    let deadline = Instant::now() + CONNECTION_DRAIN_TIMEOUT;
    while !tunnels.is_empty() {
        match timeout(
            deadline.saturating_duration_since(Instant::now()),
            tunnels.join_next(),
        )
        .await
        {
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }
    tunnels.abort_all();
    while tunnels.join_next().await.is_some() {}
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
    tunnels: Arc<tokio::sync::Mutex<JoinSet<()>>>,
    shutdown: watch::Receiver<bool>,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    if request.method() == hyper::Method::CONNECT {
        return Ok(handle_connect(request, config, connectors, tunnels, shutdown).await);
    }
    Ok(handle_with_body_limits(request, config, connectors, BODY_TIMEOUT, MAX_BODY_BYTES).await)
}

async fn handle_connect(
    mut request: Request<Incoming>,
    config: Config,
    connectors: Connectors,
    tunnels: Arc<tokio::sync::Mutex<JoinSet<()>>>,
    shutdown: watch::Receiver<bool>,
) -> Response<Full<Bytes>> {
    let uri = request.uri().to_string();
    let Target::PublicConnect(target) = classify("CONNECT", &uri) else {
        return route_connect_fallback(&config, &connectors, &uri).await;
    };
    let decision = decide_public(&config.allowlists, &config.mode_file, target.host());
    if !decision.allowed {
        record(&config, target.authority());
        return response(StatusCode::FORBIDDEN, None, "forbidden\n");
    }
    if decision.record_denial {
        record(&config, target.authority());
    }
    let Ok(upstream) = connectors.public.connect(target).await else {
        return response(StatusCode::BAD_GATEWAY, None, "bad gateway\n");
    };
    let upgrade = hyper::upgrade::on(&mut request);
    tunnels.lock().await.spawn(async move {
        let Ok(upgraded) = upgrade.await else {
            return;
        };
        let Ok(parts) = upgraded.downcast::<TokioIo<TcpStream>>() else {
            return;
        };
        let _ = crate::relay::relay(TokioIo::new(parts.io), upstream, shutdown).await;
    });
    response(StatusCode::OK, None, "")
}

async fn route_connect_fallback(
    config: &Config,
    connectors: &Connectors,
    uri: &str,
) -> Response<Full<Bytes>> {
    let target = classify("CONNECT", uri);
    match target {
        Target::LocalConnect(target) => {
            if config.local.as_ref().is_some_and(|local| {
                decide_local(&local.policy_paths, target.canonical_authority())
            }) {
                connectors.local.connect(target).await;
                response(StatusCode::BAD_GATEWAY, None, "bad gateway\n")
            } else {
                record(config, target.authority());
                response(StatusCode::FORBIDDEN, None, "forbidden\n")
            }
        }
        _ => response(StatusCode::BAD_REQUEST, None, "bad request\n"),
    }
}

async fn handle_with_body_limits<B>(
    request: Request<B>,
    config: Config,
    connectors: Connectors,
    body_timeout: Duration,
    body_limit: usize,
) -> Response<Full<Bytes>>
where
    B: Body<Data = Bytes> + Unpin,
{
    let (parts, body) = request.into_parts();
    let Ok(collected) = timeout(body_timeout, collect_request_body(body, body_limit)).await else {
        return response(StatusCode::BAD_REQUEST, None, "bad request\n");
    };
    let Ok(body) = collected else {
        return response(StatusCode::BAD_REQUEST, None, "bad request\n");
    };
    let request = Request::from_parts(parts, Full::new(body));
    route_request(request, config, connectors).await
}

async fn collect_request_body<B>(mut body: B, limit: usize) -> std::result::Result<Bytes, ()>
where
    B: Body<Data = Bytes> + Unpin,
{
    let mut bytes = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|_| ())?;
        if let Ok(data) = frame.into_data() {
            let remaining = limit.saturating_sub(bytes.len());
            if data.len() > remaining {
                return Err(());
            }
            bytes.extend_from_slice(&data);
        }
    }
    Ok(Bytes::from(bytes))
}

#[cfg(test)]
async fn route(
    method: &hyper::Method,
    uri: &str,
    config: Config,
    connectors: Connectors,
) -> Response<Full<Bytes>> {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .body(Full::new(Bytes::new()))
        .expect("test request is valid");
    route_request(request, config, connectors).await
}

async fn route_request(
    request: Request<Full<Bytes>>,
    config: Config,
    connectors: Connectors,
) -> Response<Full<Bytes>> {
    let method = request.method().clone();
    let uri = request.uri().to_string();
    let target = classify(method.as_str(), &uri);
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
                match connectors.public.http(target, request).await {
                    Ok(origin) => origin_response(origin),
                    Err(_) => response(StatusCode::BAD_GATEWAY, None, "bad gateway\n"),
                }
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
                let _ = connectors.public.connect(target).await;
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

fn origin_response(origin: PublicResponse) -> Response<Full<Bytes>> {
    let mut builder = Response::builder().status(origin.status);
    for (name, value) in &origin.headers {
        builder = builder.header(name, value);
    }
    builder
        .body(Full::new(origin.body))
        .expect("origin response headers are valid")
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
    use std::collections::VecDeque;
    use std::convert::Infallible;
    use std::pin::Pin;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context as TaskContext, Poll};

    use http_body_util::BodyExt;
    use hyper::body::Frame;
    use tempfile::tempdir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    use tokio::sync::{Notify, oneshot, watch};
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
        fn http(&self, _: PublicTarget, _: Request<Full<Bytes>>) -> PublicHttpFuture<'_> {
            self.public_http.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { anyhow::bail!("test connector") })
        }
        fn connect(&self, _: PublicTarget) -> PublicConnectFuture<'_> {
            self.public_connect.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { anyhow::bail!("test connector") })
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
    struct ControlledPublic {
        entered: Notify,
        calls: AtomicUsize,
        result: Mutex<Option<oneshot::Receiver<anyhow::Result<BoxStream>>>>,
    }
    impl ControlledPublic {
        fn new(result: oneshot::Receiver<anyhow::Result<BoxStream>>) -> Self {
            Self {
                entered: Notify::new(),
                calls: AtomicUsize::new(0),
                result: Mutex::new(Some(result)),
            }
        }
    }
    struct TestBody {
        frames: VecDeque<Bytes>,
        pending: bool,
    }
    impl TestBody {
        fn frames(frames: &[&[u8]]) -> Self {
            Self {
                frames: frames
                    .iter()
                    .map(|frame| Bytes::copy_from_slice(frame))
                    .collect(),
                pending: false,
            }
        }
        fn pending() -> Self {
            Self {
                frames: VecDeque::new(),
                pending: true,
            }
        }
    }
    impl Body for TestBody {
        type Data = Bytes;
        type Error = Infallible;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            _: &mut TaskContext<'_>,
        ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
            if let Some(frame) = self.frames.pop_front() {
                return Poll::Ready(Some(Ok(Frame::data(frame))));
            }
            if self.pending {
                Poll::Pending
            } else {
                Poll::Ready(None)
            }
        }
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
    struct ScenarioResolver {
        calls: AtomicUsize,
    }
    impl Resolver for ScenarioResolver {
        fn resolve(&self, _: String, _: u16) -> ResolveFuture {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(vec!["8.8.8.8".parse().unwrap()]) })
        }
    }
    struct ScenarioDialer {
        calls: AtomicUsize,
        addresses: Mutex<Vec<SocketAddr>>,
        streams: Mutex<VecDeque<BoxStream>>,
    }
    impl ScenarioDialer {
        fn new(streams: Vec<BoxStream>) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                addresses: Mutex::new(Vec::new()),
                streams: Mutex::new(streams.into()),
            }
        }
    }
    impl NumericDialer for ScenarioDialer {
        fn dial(&self, address: SocketAddr) -> DialFuture {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.addresses.lock().unwrap().push(address);
            let stream = self.streams.lock().unwrap().pop_front();
            Box::pin(async move { stream.ok_or_else(|| anyhow::anyhow!("missing test stream")) })
        }
    }
    impl sealed::Public for PendingPublic {}
    impl PublicConnector for PendingPublic {
        fn http(&self, _: PublicTarget, _: Request<Full<Bytes>>) -> PublicHttpFuture<'_> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.entered.notify_one();
            Box::pin(std::future::pending())
        }
        fn connect(&self, _: PublicTarget) -> PublicConnectFuture<'_> {
            Box::pin(async { anyhow::bail!("test connector") })
        }
    }
    impl sealed::Public for ControlledPublic {}
    impl PublicConnector for ControlledPublic {
        fn http(&self, _: PublicTarget, _: Request<Full<Bytes>>) -> PublicHttpFuture<'_> {
            Box::pin(async { anyhow::bail!("test connector") })
        }

        fn connect(&self, _: PublicTarget) -> PublicConnectFuture<'_> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.entered.notify_one();
            let result = self.result.lock().unwrap().take().unwrap();
            Box::pin(async move {
                result
                    .await
                    .map_err(|_| anyhow::anyhow!("test connector released without result"))?
            })
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
    async fn response_head(stream: &mut TcpStream) -> Vec<u8> {
        let mut head = Vec::new();
        loop {
            let mut byte = [0];
            timeout(Duration::from_millis(500), stream.read_exact(&mut byte))
                .await
                .unwrap()
                .unwrap();
            head.push(byte[0]);
            if head.ends_with(b"\r\n\r\n") {
                return head;
            }
        }
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

    fn proxy_request(method: &str, uri: &str, body: &[u8]) -> Request<Full<Bytes>> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header("host", "allowed.example")
            .body(Full::new(Bytes::copy_from_slice(body)))
            .unwrap()
    }
    #[tokio::test]
    async fn inbound_body_limits_reject_before_routing_and_timeout_boundedly() {
        assert!(
            collect_request_body(TestBody::frames(&[b"abc", b"de"]), 4)
                .await
                .is_err()
        );
        assert!(
            timeout(
                Duration::from_millis(40),
                collect_request_body(TestBody::pending(), 4),
            )
            .await
            .is_err()
        );

        let directory = tempdir().unwrap();
        let counts = Arc::new(Counts {
            public_http: AtomicUsize::new(0),
            public_connect: AtomicUsize::new(0),
            local_http: AtomicUsize::new(0),
            local_connect: AtomicUsize::new(0),
        });
        let connectors = Connectors::new(counts.clone(), counts.clone());
        for body in [TestBody::frames(&[b"abc", b"de"]), TestBody::pending()] {
            let request = Request::builder()
                .method("POST")
                .uri("http://allowed.example/upload")
                .body(body)
                .unwrap();
            let response = timeout(
                Duration::from_millis(100),
                handle_with_body_limits(
                    request,
                    config(directory.path(), "enforce"),
                    connectors.clone(),
                    Duration::from_millis(20),
                    4,
                ),
            )
            .await
            .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            assert!(response.headers().get("content-type").is_none());
            assert_eq!(
                response.into_body().collect().await.unwrap().to_bytes(),
                "bad request\n"
            );
        }
        assert_eq!(counts.values(), (0, 0, 0, 0));
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
    async fn denied_public_connect_never_resolves_or_dials() {
        let directory = tempdir().unwrap();
        let listener = bind_listener("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let resolver = Arc::new(AdapterResolver(Mutex::new(Vec::new())));
        let dialer = Arc::new(AdapterDialer(Mutex::new(Vec::new())));
        let (shutdown, receiver) = watch::channel(false);
        let task = tokio::spawn(serve(
            listener,
            config(directory.path(), "enforce"),
            Connectors::new(
                Arc::new(PublicConnectorAdapter::new(
                    resolver.clone(),
                    dialer.clone(),
                )),
                Arc::new(InertConnector),
            ),
            receiver,
        ));
        let response = raw_request(
            address,
            b"CONNECT denied.example:443 HTTP/1.1\r\nHost: denied.example:443\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(response.starts_with(b"HTTP/1.1 403 Forbidden\r\n"));
        assert!(response.ends_with(b"\r\n\r\nforbidden\n"));
        assert!(
            std::fs::read_to_string(directory.path().join("deny"))
                .unwrap()
                .ends_with("\tdenied.example:443\n")
        );
        assert!(resolver.0.lock().unwrap().is_empty());
        assert!(dialer.0.lock().unwrap().is_empty());
        shutdown.send(true).unwrap();
        timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn public_connect_discards_header_buffer_and_relays_after_success() {
        let directory = tempdir().unwrap();
        let listener = bind_listener("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (upstream, mut peer) = tokio::io::duplex(1024);
        let resolver = Arc::new(ScenarioResolver {
            calls: AtomicUsize::new(0),
        });
        let dialer = Arc::new(ScenarioDialer::new(vec![Box::new(upstream)]));
        let adapter = Arc::new(PublicConnectorAdapter::new(resolver, dialer.clone()));
        let (shutdown, receiver) = watch::channel(false);
        let task = tokio::spawn(serve(
            listener,
            config(directory.path(), "enforce"),
            Connectors::new(adapter, Arc::new(InertConnector)),
            receiver,
        ));
        let mut client = TcpStream::connect(address).await.unwrap();
        client
            .write_all(b"CONNECT allowed.example HTTP/1.1\r\nHost: allowed.example\r\n\r\nsentinel")
            .await
            .unwrap();
        let head = response_head(&mut client).await;
        assert!(head.starts_with(b"HTTP/1.1 200 OK\r\n"));
        assert!(head.ends_with(b"\r\n\r\n"));
        assert_eq!(
            dialer.addresses.lock().unwrap().as_slice(),
            &["8.8.8.8:443".parse().unwrap()]
        );
        let mut unexpected = [0; 8];
        assert!(
            timeout(Duration::from_millis(80), peer.read(&mut unexpected))
                .await
                .is_err()
        );
        client.write_all(b"later bytes").await.unwrap();
        client.shutdown().await.unwrap();
        let mut forwarded = Vec::new();
        peer.read_to_end(&mut forwarded).await.unwrap();
        assert_eq!(forwarded, b"later bytes");
        peer.write_all(b"upstream bytes").await.unwrap();
        peer.shutdown().await.unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        assert_eq!(response, b"upstream bytes");
        shutdown.send(true).unwrap();
        timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn public_connect_upstream_eof_forwards_bytes_then_closes_both_sides() {
        for _ in 0..5 {
            let directory = tempdir().unwrap();
            let listener = bind_listener("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let (upstream, mut peer) = tokio::io::duplex(1024);
            let dialer = Arc::new(ScenarioDialer::new(vec![Box::new(upstream)]));
            let adapter = Arc::new(PublicConnectorAdapter::new(
                Arc::new(ScenarioResolver {
                    calls: AtomicUsize::new(0),
                }),
                dialer,
            ));
            let (shutdown, receiver) = watch::channel(false);
            let service = tokio::spawn(serve(
                listener,
                config(directory.path(), "enforce"),
                Connectors::new(adapter, Arc::new(InertConnector)),
                receiver,
            ));
            let mut client = TcpStream::connect(address).await.unwrap();
            client
                .write_all(
                    b"CONNECT allowed.example HTTP/1.1\r\nHost: allowed.example\r\n\r\nignored",
                )
                .await
                .unwrap();
            let head = response_head(&mut client).await;
            assert!(head.starts_with(b"HTTP/1.1 200 OK\r\n"));
            let mut discarded = [0; 1];
            assert!(
                timeout(Duration::from_millis(80), peer.read(&mut discarded))
                    .await
                    .is_err()
            );
            peer.write_all(b"sentinel").await.unwrap();
            peer.shutdown().await.unwrap();
            let mut forwarded = Vec::new();
            timeout(Duration::from_secs(1), client.read_to_end(&mut forwarded))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(forwarded, b"sentinel");
            let mut byte = [0];
            assert_eq!(
                timeout(Duration::from_secs(1), peer.read(&mut byte))
                    .await
                    .unwrap()
                    .unwrap(),
                0
            );
            shutdown.send(true).unwrap();
            let report = timeout(Duration::from_secs(1), service)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(report.aborted_tasks, 0);
        }
    }

    #[tokio::test]
    async fn healthy_public_tunnel_outlives_connection_drain_timeout() {
        for _ in 0..5 {
            let directory = tempdir().unwrap();
            let listener = bind_listener("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let (upstream, mut peer) = tokio::io::duplex(1024);
            let dialer = Arc::new(ScenarioDialer::new(vec![Box::new(upstream)]));
            let adapter = Arc::new(PublicConnectorAdapter::new(
                Arc::new(ScenarioResolver {
                    calls: AtomicUsize::new(0),
                }),
                dialer,
            ));
            let (shutdown, receiver) = watch::channel(false);
            let service = tokio::spawn(serve(
                listener,
                config(directory.path(), "enforce"),
                Connectors::new(adapter, Arc::new(InertConnector)),
                receiver,
            ));
            let mut client = TcpStream::connect(address).await.unwrap();
            client
                .write_all(b"CONNECT allowed.example HTTP/1.1\r\nHost: allowed.example\r\n\r\n")
                .await
                .unwrap();
            let head = response_head(&mut client).await;
            assert!(head.starts_with(b"HTTP/1.1 200 OK\r\n"));
            tokio::time::sleep(Duration::from_millis(1_050)).await;
            client.write_all(b"still open").await.unwrap();
            let mut forwarded = [0; 10];
            timeout(Duration::from_secs(1), peer.read_exact(&mut forwarded))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(&forwarded, b"still open");
            peer.write_all(b"reply").await.unwrap();
            peer.shutdown().await.unwrap();
            let mut reply = Vec::new();
            timeout(Duration::from_secs(1), client.read_to_end(&mut reply))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(reply, b"reply");
            shutdown.send(true).unwrap();
            let report = timeout(Duration::from_secs(1), service)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(report.aborted_tasks, 0);
        }
    }

    #[tokio::test]
    async fn public_connect_waits_for_upstream_before_writing_success() {
        let directory = tempdir().unwrap();
        let listener = bind_listener("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (released, result) = oneshot::channel();
        let connector = Arc::new(ControlledPublic::new(result));
        let (shutdown, receiver) = watch::channel(false);
        let task = tokio::spawn(serve(
            listener,
            config(directory.path(), "enforce"),
            Connectors::new(connector.clone(), Arc::new(InertConnector)),
            receiver,
        ));
        let mut client = TcpStream::connect(address).await.unwrap();
        let entered = connector.entered.notified();
        client
            .write_all(b"CONNECT allowed.example:443 HTTP/1.1\r\nHost: allowed.example:443\r\n\r\n")
            .await
            .unwrap();
        timeout(Duration::from_millis(500), entered).await.unwrap();
        let mut byte = [0];
        assert!(
            timeout(Duration::from_millis(80), client.read(&mut byte))
                .await
                .is_err()
        );
        let (upstream, mut peer) = tokio::io::duplex(1024);
        assert!(released.send(Ok(Box::new(upstream))).is_ok());
        let head = response_head(&mut client).await;
        assert!(head.starts_with(b"HTTP/1.1 200 OK\r\n"));
        assert!(head.ends_with(b"\r\n\r\n"));
        client.write_all(b"later bytes").await.unwrap();
        let mut forwarded = [0; 11];
        timeout(Duration::from_millis(500), peer.read_exact(&mut forwarded))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&forwarded, b"later bytes");
        client.shutdown().await.unwrap();
        peer.shutdown().await.unwrap();
        let mut rest = Vec::new();
        timeout(Duration::from_millis(500), client.read_to_end(&mut rest))
            .await
            .unwrap()
            .unwrap();
        shutdown.send(true).unwrap();
        let report = timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(report.aborted_tasks, 0);
        assert_eq!(connector.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn failed_public_connect_never_upgrades_or_retains_a_tunnel() {
        let directory = tempdir().unwrap();
        let listener = bind_listener("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (released, result) = oneshot::channel();
        let connector = Arc::new(ControlledPublic::new(result));
        let (shutdown, receiver) = watch::channel(false);
        let task = tokio::spawn(serve(
            listener,
            config(directory.path(), "enforce"),
            Connectors::new(connector.clone(), Arc::new(InertConnector)),
            receiver,
        ));
        let mut client = TcpStream::connect(address).await.unwrap();
        let entered = connector.entered.notified();
        client
            .write_all(b"CONNECT allowed.example:443 HTTP/1.1\r\nHost: allowed.example:443\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        timeout(Duration::from_millis(500), entered).await.unwrap();
        assert!(released.send(Err(anyhow::anyhow!("test failure"))).is_ok());
        let mut response = Vec::new();
        timeout(
            Duration::from_millis(500),
            client.read_to_end(&mut response),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(response.starts_with(b"HTTP/1.1 502 Bad Gateway\r\n"));
        assert!(response.ends_with(b"\r\n\r\nbad gateway\n"));
        assert!(!response.windows(3).any(|window| window == b"200"));
        shutdown.send(true).unwrap();
        let report = timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(report.aborted_tasks, 0);
        assert_eq!(report.drained_tasks, 0);
        assert_eq!(report.reaped_tasks, 1);
        assert_eq!(connector.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn active_public_tunnels_close_and_reap_on_disconnect_or_shutdown() {
        for close_client in [true, false].into_iter().cycle().take(10) {
            let directory = tempdir().unwrap();
            let listener = bind_listener("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let (released, result) = oneshot::channel();
            let connector = Arc::new(ControlledPublic::new(result));
            let (shutdown, receiver) = watch::channel(false);
            let task = tokio::spawn(serve(
                listener,
                config(directory.path(), "enforce"),
                Connectors::new(connector.clone(), Arc::new(InertConnector)),
                receiver,
            ));
            let (upstream, mut peer) = tokio::io::duplex(1024);
            assert!(released.send(Ok(Box::new(upstream))).is_ok());
            let mut client = Some(TcpStream::connect(address).await.unwrap());
            let entered = connector.entered.notified();
            client
                .as_mut()
                .unwrap()
                .write_all(
                    b"CONNECT allowed.example:443 HTTP/1.1\r\nHost: allowed.example:443\r\n\r\n",
                )
                .await
                .unwrap();
            timeout(Duration::from_millis(500), entered).await.unwrap();
            let head = response_head(client.as_mut().unwrap()).await;
            assert!(head.starts_with(b"HTTP/1.1 200 OK\r\n"));
            assert!(head.ends_with(b"\r\n\r\n"));
            if close_client {
                drop(client.take());
                let mut byte = [0];
                assert_eq!(
                    timeout(Duration::from_millis(500), peer.read(&mut byte))
                        .await
                        .unwrap()
                        .unwrap(),
                    0
                );
                peer.shutdown().await.unwrap();
                let health = raw_request(
                    address,
                    b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
                )
                .await;
                assert!(health.starts_with(b"HTTP/1.1 200 OK\r\n"));
            }
            shutdown.send(true).unwrap();
            let mut byte = [0];
            if let Some(client) = client.as_mut() {
                assert_eq!(
                    timeout(Duration::from_millis(900), client.read(&mut byte))
                        .await
                        .unwrap()
                        .unwrap(),
                    0
                );
            }
            assert_eq!(
                timeout(Duration::from_millis(900), peer.read(&mut byte))
                    .await
                    .unwrap()
                    .unwrap(),
                0
            );
            let report = timeout(Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(report.aborted_tasks, 0);
            assert!(report.drained_tasks + report.reaped_tasks <= 2);
            if close_client {
                assert!(report.reaped_tasks >= 1);
            }
            assert_eq!(connector.calls.load(Ordering::SeqCst), 1);
        }
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

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn public_service_forwards_exact_request_reuses_pool_then_observes_revocation() {
        let directory = tempdir().unwrap();
        let (client, mut origin) = tokio::io::duplex(8192);
        let resolver = Arc::new(ScenarioResolver {
            calls: AtomicUsize::new(0),
        });
        let dialer = Arc::new(ScenarioDialer::new(vec![Box::new(client)]));
        let adapter = Arc::new(PublicConnectorAdapter::new(
            resolver.clone(),
            dialer.clone(),
        ));
        let connectors = Connectors::new(adapter.clone(), Arc::new(InertConnector));
        let policy_config = config(directory.path(), "enforce");
        let (seen, received) = oneshot::channel();
        let origin_task = tokio::spawn(async move {
            let mut first = [0; 2048];
            let length = timeout(Duration::from_millis(500), origin.read(&mut first))
                .await
                .unwrap()
                .unwrap();
            let first = String::from_utf8_lossy(&first[..length]);
            assert!(first.starts_with("POST /path?q=one HTTP/1.1\r\n"));
            assert!(first.contains("host: allowed.example"));
            assert!(first.contains("connection: X-Remove"));
            assert!(first.contains("x-remove: kept"));
            assert!(first.contains("\r\n\r\nexact body"));
            assert!(!first.contains("proxy-connection"));
            assert!(!first.contains("proxy-authorization"));
            origin
                .write_all(
                    b"HTTP/1.1 201 Created\r\nX-Origin: one\r\nContent-Length: 5\r\n\r\nfirst",
                )
                .await
                .unwrap();
            let mut second = [0; 2048];
            let length = timeout(Duration::from_millis(500), origin.read(&mut second))
                .await
                .unwrap()
                .unwrap();
            let second = String::from_utf8_lossy(&second[..length]);
            assert!(second.starts_with("GET /again HTTP/1.1\r\n"));
            origin
                .write_all(
                    b"HTTP/1.1 202 Accepted\r\nX-Origin: two\r\nContent-Length: 6\r\n\r\nsecond",
                )
                .await
                .unwrap();
            let _ = seen.send(());
        });
        let mut first = proxy_request("POST", "http://allowed.example/path?q=one", b"exact body");
        first
            .headers_mut()
            .insert("connection", "X-Remove".parse().unwrap());
        first
            .headers_mut()
            .insert("x-remove", "kept".parse().unwrap());
        first
            .headers_mut()
            .insert("proxy-connection", "close".parse().unwrap());
        first
            .headers_mut()
            .insert("proxy-authorization", "Basic ignored".parse().unwrap());
        let response = route_request(first, policy_config.clone(), connectors.clone()).await;
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(response.headers()["x-origin"], "one");
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            "first"
        );
        let response = route_request(
            proxy_request("GET", "http://ALLOWED.example./again", b""),
            policy_config.clone(),
            connectors.clone(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            "second"
        );
        timeout(Duration::from_millis(500), received)
            .await
            .unwrap()
            .unwrap();
        timeout(Duration::from_millis(500), origin_task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
        assert_eq!(dialer.calls.load(Ordering::SeqCst), 1);
        assert_eq!(adapter.public_calls(), 2);
        std::fs::write(directory.path().join("public"), "").unwrap();
        let response = route_request(
            proxy_request("GET", "http://allowed.example/blocked", b""),
            policy_config,
            connectors,
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            "forbidden\n"
        );
        assert_eq!(adapter.public_calls(), 2);
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
        assert_eq!(dialer.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn public_service_buffers_origin_response_until_completion() {
        let directory = tempdir().unwrap();
        let (client, mut origin) = tokio::io::duplex(4096);
        let resolver = Arc::new(ScenarioResolver {
            calls: AtomicUsize::new(0),
        });
        let dialer = Arc::new(ScenarioDialer::new(vec![Box::new(client)]));
        let adapter = Arc::new(PublicConnectorAdapter::new(resolver, dialer));
        let connectors = Connectors::new(adapter.clone(), Arc::new(InertConnector));
        let (release, wait) = oneshot::channel();
        let origin_task = tokio::spawn(async move {
            let mut request = [0; 1024];
            timeout(Duration::from_millis(500), origin.read(&mut request))
                .await
                .unwrap()
                .unwrap();
            origin
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 12\r\n\r\nfirst")
                .await
                .unwrap();
            wait.await.unwrap();
            origin.write_all(b" second").await.unwrap();
        });
        let mut response = Box::pin(route_request(
            proxy_request("GET", "http://allowed.example/stream", b""),
            config(directory.path(), "enforce"),
            connectors,
        ));
        assert!(
            timeout(Duration::from_millis(100), &mut response)
                .await
                .is_err()
        );
        let _ = release.send(());
        let response = timeout(Duration::from_millis(500), response).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            "first second"
        );
        timeout(Duration::from_millis(500), origin_task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(adapter.pool_len().await, 1);
    }

    #[tokio::test]
    async fn public_service_disconnect_cancels_origin_and_drops_pool_entry() {
        let directory = tempdir().unwrap();
        let (client, mut origin) = tokio::io::duplex(4096);
        let resolver = Arc::new(ScenarioResolver {
            calls: AtomicUsize::new(0),
        });
        let dialer = Arc::new(ScenarioDialer::new(vec![Box::new(client)]));
        let adapter = Arc::new(PublicConnectorAdapter::new(resolver, dialer));
        let connectors = Connectors::new(adapter.clone(), Arc::new(InertConnector));
        let (started, ready) = oneshot::channel();
        let origin_task = tokio::spawn(async move {
            let mut request = [0; 1024];
            timeout(Duration::from_millis(500), origin.read(&mut request))
                .await
                .unwrap()
                .unwrap();
            let _ = started.send(());
            let mut byte = [0; 1];
            assert_eq!(
                timeout(Duration::from_millis(500), origin.read(&mut byte))
                    .await
                    .unwrap()
                    .unwrap(),
                0
            );
        });
        let request = proxy_request("GET", "http://allowed.example/pending", b"");
        let task = tokio::spawn(route_request(
            request,
            config(directory.path(), "enforce"),
            connectors,
        ));
        timeout(Duration::from_millis(500), ready)
            .await
            .unwrap()
            .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        timeout(Duration::from_millis(500), origin_task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(adapter.pool_len().await, 0);
        timeout(Duration::from_millis(500), async {
            while adapter.active_drivers() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(adapter.active_drivers(), 0);
    }

    #[tokio::test]
    async fn public_service_keys_scheme_normalized_host_and_port_and_verifies_tls() {
        let directory = tempdir().unwrap();
        let policy_config = config(directory.path(), "enforce");
        std::fs::write(
            directory.path().join("public"),
            "allowed.example\nother.example\n",
        )
        .unwrap();
        let (first_client, mut first_origin) = tokio::io::duplex(4096);
        let (second_client, mut second_origin) = tokio::io::duplex(4096);
        let (tls_client, mut tls_peer) = tokio::io::duplex(4096);
        let resolver = Arc::new(ScenarioResolver {
            calls: AtomicUsize::new(0),
        });
        let dialer = Arc::new(ScenarioDialer::new(vec![
            Box::new(first_client),
            Box::new(second_client),
            Box::new(tls_client),
        ]));
        let adapter = Arc::new(PublicConnectorAdapter::new(
            resolver.clone(),
            dialer.clone(),
        ));
        let connectors = Connectors::new(adapter, Arc::new(InertConnector));
        let first = tokio::spawn(async move {
            let mut request = [0; 1024];
            timeout(Duration::from_millis(500), first_origin.read(&mut request))
                .await
                .unwrap()
                .unwrap();
            first_origin
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\na")
                .await
                .unwrap();
        });
        let second = tokio::spawn(async move {
            let mut request = [0; 1024];
            timeout(Duration::from_millis(500), second_origin.read(&mut request))
                .await
                .unwrap()
                .unwrap();
            second_origin
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\nb")
                .await
                .unwrap();
        });
        let tls = tokio::spawn(async move {
            let mut bytes = [0; 1024];
            let size = timeout(Duration::from_millis(500), tls_peer.read(&mut bytes))
                .await
                .unwrap()
                .unwrap();
            assert!(size >= 3);
            assert_eq!(&bytes[..3], &[22, 3, 1]);
            assert!(!bytes[..size].starts_with(b"GET "));
        });
        for uri in [
            "http://ALLOWED.example.:80/one",
            "http://other.example:81/two",
            "https://allowed.example/three",
        ] {
            let response = route_request(
                proxy_request("GET", uri, b""),
                policy_config.clone(),
                connectors.clone(),
            )
            .await;
            if uri.starts_with("https") {
                assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
            } else {
                assert_eq!(response.status(), StatusCode::OK);
            }
        }
        timeout(Duration::from_millis(500), first)
            .await
            .unwrap()
            .unwrap();
        timeout(Duration::from_millis(500), second)
            .await
            .unwrap()
            .unwrap();
        timeout(Duration::from_millis(500), tls)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 3);
        assert_eq!(dialer.calls.load(Ordering::SeqCst), 3);
    }
}
