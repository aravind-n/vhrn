//! Validated numeric public connection setup.

mod registry;

use std::fmt;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::{Result, anyhow};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::client::conn::http1;
use hyper::{Request, Uri};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::task::JoinHandle;
use tokio::time::{Instant, timeout, timeout_at};

use crate::Shutdown;
use crate::connect::origin_body::{BoundedOriginBody, OriginBodyLimits, SharedOriginResponse};
use crate::connect::pool::IdlePool;
use crate::domain::target::{PublicHost, PublicTarget};
use crate::headers::sanitize_hop_by_hop;

use self::registry::{ipv4_is_global, ipv6_is_global};

const CONNECT_DEADLINE: Duration = Duration::from_secs(10);
const MAX_DNS_ANSWERS: usize = 64;
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// A validated, numeric public connection.
#[derive(Debug)]
pub(crate) enum PublicStream {
    Tcp(TcpStream),
    #[cfg(test)]
    Test(tokio::io::DuplexStream),
}

impl AsyncRead for PublicStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.as_mut().get_mut() {
            Self::Tcp(stream) => Pin::new(stream).poll_read(cx, buffer),
            #[cfg(test)]
            Self::Test(stream) => Pin::new(stream).poll_read(cx, buffer),
        }
    }
}

impl AsyncWrite for PublicStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.as_mut().get_mut() {
            Self::Tcp(stream) => Pin::new(stream).poll_write(cx, buffer),
            #[cfg(test)]
            Self::Test(stream) => Pin::new(stream).poll_write(cx, buffer),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.as_mut().get_mut() {
            Self::Tcp(stream) => Pin::new(stream).poll_flush(cx),
            #[cfg(test)]
            Self::Test(stream) => Pin::new(stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.as_mut().get_mut() {
            Self::Tcp(stream) => Pin::new(stream).poll_shutdown(cx),
            #[cfg(test)]
            Self::Test(stream) => Pin::new(stream).poll_shutdown(cx),
        }
    }
}

pub(crate) type ResolveFuture = Pin<Box<dyn Future<Output = Result<Vec<ResolvedAddress>>> + Send>>;
pub(crate) type DialFuture = Pin<Box<dyn Future<Output = Result<PublicStream>> + Send>>;

/// One resolver result with scope metadata retained until validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ResolvedAddress {
    address: IpAddr,
    scope_id: u32,
}

impl ResolvedAddress {
    fn from_socket(address: SocketAddr) -> Self {
        match address {
            SocketAddr::V4(address) => Self {
                address: IpAddr::V4(*address.ip()),
                scope_id: 0,
            },
            SocketAddr::V6(address) => Self {
                address: IpAddr::V6(*address.ip()),
                scope_id: address.scope_id(),
            },
        }
    }

    #[cfg(test)]
    pub(crate) const fn unscoped(address: IpAddr) -> Self {
        Self {
            address,
            scope_id: 0,
        }
    }

    #[cfg(test)]
    const fn scoped(address: IpAddr, scope_id: u32) -> Self {
        Self { address, scope_id }
    }
}

/// Resolves one normalized public name for one connection attempt.
pub(crate) trait Resolver: Send + Sync + 'static {
    fn resolve(&self, host: String, port: u16) -> ResolveFuture;
}

/// Opens a connection to one validated numeric socket address.
pub(crate) trait NumericDialer: Send + Sync + 'static {
    fn dial(&self, address: SocketAddr) -> DialFuture;
}

struct SystemResolver;
impl Resolver for SystemResolver {
    fn resolve(&self, host: String, port: u16) -> ResolveFuture {
        Box::pin(async move {
            let addresses = tokio::net::lookup_host((host.as_str(), port))
                .await
                .map_err(|_| anyhow!("public name resolution failed"))?;
            Ok(addresses
                .take(MAX_DNS_ANSWERS + 1)
                .map(ResolvedAddress::from_socket)
                .collect())
        })
    }
}

struct SystemDialer;
impl NumericDialer for SystemDialer {
    fn dial(&self, address: SocketAddr) -> DialFuture {
        Box::pin(async move {
            let stream = TcpStream::connect(address)
                .await
                .map_err(|_| anyhow!("public connection failed"))?;
            Ok(PublicStream::Tcp(stream))
        })
    }
}

/// A safe, response-classified public connection failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PublicConnectError {
    PolicyDenied,
    Unavailable,
    DeadlineExceeded,
    Cancelled,
    OriginFailure,
}

impl PublicConnectError {
    pub(crate) const fn is_policy_denial(self) -> bool {
        matches!(self, Self::PolicyDenied)
    }
}

impl fmt::Display for PublicConnectError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::PolicyDenied => "public address rejected",
            Self::Unavailable => "public destination unavailable",
            Self::DeadlineExceeded => "public connection deadline exceeded",
            Self::Cancelled => "public connection cancelled",
            Self::OriginFailure => "public origin exchange failed",
        })
    }
}

impl std::error::Error for PublicConnectError {}

/// Stable identity for a public origin connection pool.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct OriginKey {
    host: PublicHost,
    ipv6_scope: Option<String>,
    port: u16,
}

impl OriginKey {
    #[must_use]
    pub fn from_target(target: &PublicTarget) -> Self {
        Self {
            host: target.host().clone(),
            ipv6_scope: target.ipv6_scope().map(str::to_owned),
            port: target.port(),
        }
    }
}

/// An address which is safe for a public origin dial.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GloballyRoutableIp(IpAddr);

impl GloballyRoutableIp {
    fn new(address: IpAddr) -> Option<Self> {
        let eligible = match address {
            IpAddr::V4(address) => ipv4_is_global(address),
            IpAddr::V6(address) => ipv6_is_global(address),
        };
        eligible.then_some(Self(address))
    }

    fn socket_addr(self, port: u16) -> SocketAddr {
        SocketAddr::new(self.0, port)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ValidatedAnswers(Vec<GloballyRoutableIp>);

impl ValidatedAnswers {
    fn from_dns(answers: Vec<ResolvedAddress>) -> std::result::Result<Self, PublicConnectError> {
        if answers.is_empty() {
            return Err(PublicConnectError::Unavailable);
        }
        if answers.len() > MAX_DNS_ANSWERS {
            return Err(PublicConnectError::PolicyDenied);
        }
        answers
            .into_iter()
            .map(|answer| {
                if answer.scope_id != 0 {
                    return Err(PublicConnectError::PolicyDenied);
                }
                GloballyRoutableIp::new(answer.address).ok_or(PublicConnectError::PolicyDenied)
            })
            .collect::<std::result::Result<Vec<_>, _>>()
            .map(Self)
    }

    fn from_literal(address: IpAddr) -> std::result::Result<Self, PublicConnectError> {
        GloballyRoutableIp::new(address)
            .map(|address| Self(vec![address]))
            .ok_or(PublicConnectError::PolicyDenied)
    }
}

/// Public connector that resolves and opens one validated numeric stream.
pub(crate) struct PublicConnector {
    resolver: Arc<dyn Resolver>,
    dialer: Arc<dyn NumericDialer>,
    pool: IdlePool<OriginKey, OriginConnection>,
    response_timeout: Duration,
    response_limit: usize,
    #[cfg(test)]
    public_calls: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    active_drivers: Arc<std::sync::atomic::AtomicUsize>,
}

struct OriginConnection {
    sender: http1::SendRequest<Full<Bytes>>,
    driver: JoinHandle<()>,
}

pub(crate) type PublicResponse = SharedOriginResponse;

#[cfg(test)]
struct DriverGuard(Arc<std::sync::atomic::AtomicUsize>);

#[cfg(test)]
impl Drop for DriverGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Drop for OriginConnection {
    fn drop(&mut self) {
        self.driver.abort();
    }
}

fn origin_connection_reusable(connection: &OriginConnection) -> bool {
    !connection.driver.is_finished() && connection.sender.is_ready()
}

impl PublicConnector {
    pub(crate) fn system() -> Self {
        Self::from_parts(
            Arc::new(SystemResolver),
            Arc::new(SystemDialer),
            IdlePool::new(),
        )
    }

    #[cfg(test)]
    pub(crate) fn new(resolver: Arc<dyn Resolver>, dialer: Arc<dyn NumericDialer>) -> Self {
        Self::from_parts(resolver, dialer, IdlePool::new())
    }

    #[cfg(test)]
    pub(crate) fn test_with_stream(stream: tokio::io::DuplexStream) -> Self {
        struct OneResolver;
        impl Resolver for OneResolver {
            fn resolve(&self, _: String, _: u16) -> ResolveFuture {
                Box::pin(async {
                    Ok(vec![ResolvedAddress::unscoped(
                        "8.8.8.8".parse().expect("test address"),
                    )])
                })
            }
        }
        struct OneDialer(std::sync::Mutex<Option<PublicStream>>);
        impl NumericDialer for OneDialer {
            fn dial(&self, _: SocketAddr) -> DialFuture {
                let stream = self.0.lock().expect("test dialer lock").take();
                Box::pin(async move { stream.ok_or_else(|| anyhow!("missing test stream")) })
            }
        }
        Self::new(
            Arc::new(OneResolver),
            Arc::new(OneDialer(std::sync::Mutex::new(Some(PublicStream::Test(
                stream,
            ))))),
        )
    }

    fn from_parts(
        resolver: Arc<dyn Resolver>,
        dialer: Arc<dyn NumericDialer>,
        pool: IdlePool<OriginKey, OriginConnection>,
    ) -> Self {
        Self {
            resolver,
            dialer,
            pool,
            response_timeout: HTTP_TIMEOUT,
            response_limit: MAX_RESPONSE_BYTES,
            #[cfg(test)]
            public_calls: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            active_drivers: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    #[cfg(test)]
    fn with_pool_limits(
        resolver: Arc<dyn Resolver>,
        dialer: Arc<dyn NumericDialer>,
        capacity: std::num::NonZeroUsize,
        lifetime: crate::connect::pool::NonZeroDuration,
    ) -> Self {
        Self::from_parts(resolver, dialer, IdlePool::with_limits(capacity, lifetime))
    }

    #[cfg(test)]
    fn with_response_limits(
        resolver: Arc<dyn Resolver>,
        dialer: Arc<dyn NumericDialer>,
        response_timeout: Duration,
        response_limit: usize,
    ) -> Self {
        Self {
            resolver,
            dialer,
            pool: IdlePool::new(),
            response_timeout,
            response_limit,
            public_calls: std::sync::atomic::AtomicUsize::new(0),
            active_drivers: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    async fn open_with_budget(
        resolver: Arc<dyn Resolver>,
        dialer: Arc<dyn NumericDialer>,
        target: PublicTarget,
        cancellation: &Shutdown,
        budget: Duration,
    ) -> std::result::Result<PublicStream, PublicConnectError> {
        let deadline = Instant::now() + budget;
        if target.has_ipv6_scope() {
            return Err(PublicConnectError::PolicyDenied);
        }
        let answers = match target.host() {
            PublicHost::Ip(address) => ValidatedAnswers::from_literal(*address)?,
            PublicHost::Dns(_) => {
                let resolution_result = tokio::select! {
                    biased;
                    () = cancellation.cancelled() => return Err(PublicConnectError::Cancelled),
                    result = timeout_at(
                        deadline,
                        resolver.resolve(target.host().to_string(), target.port()),
                    ) => result,
                };
                let answers = resolution_result
                    .map_err(|_| PublicConnectError::DeadlineExceeded)?
                    .map_err(|_| PublicConnectError::Unavailable)?;
                ValidatedAnswers::from_dns(answers)?
            }
        };

        for address in answers.0 {
            let attempt_result = tokio::select! {
                biased;
                () = cancellation.cancelled() => return Err(PublicConnectError::Cancelled),
                result = timeout_at(
                    deadline,
                    dialer.dial(address.socket_addr(target.port())),
                ) => result,
            };
            match attempt_result {
                Err(_) => return Err(PublicConnectError::DeadlineExceeded),
                Ok(Ok(stream)) => return Ok(stream),
                Ok(Err(_)) => {}
            }
        }
        Err(PublicConnectError::Unavailable)
    }

    async fn open_target(
        &self,
        target: PublicTarget,
        cancellation: &Shutdown,
    ) -> std::result::Result<PublicStream, PublicConnectError> {
        Self::open_with_budget(
            self.resolver.clone(),
            self.dialer.clone(),
            target,
            cancellation,
            CONNECT_DEADLINE,
        )
        .await
    }

    pub(crate) async fn connect_target(
        &self,
        target: PublicTarget,
        cancellation: &Shutdown,
    ) -> std::result::Result<PublicStream, PublicConnectError> {
        self.open_target(target, cancellation).await
    }

    async fn open_sender(
        &self,
        target: &PublicTarget,
        cancellation: &Shutdown,
    ) -> std::result::Result<OriginConnection, PublicConnectError> {
        let stream = self.open_target(target.clone(), cancellation).await?;
        let (sender, connection) = timeout(HTTP_TIMEOUT, http1::handshake(TokioIo::new(stream)))
            .await
            .map_err(|_| PublicConnectError::OriginFailure)?
            .map_err(|_| PublicConnectError::OriginFailure)?;
        #[cfg(test)]
        let driver_guard = {
            self.active_drivers
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            DriverGuard(self.active_drivers.clone())
        };
        let driver = tokio::spawn(async move {
            #[cfg(test)]
            let _driver_guard = driver_guard;
            let _ = connection.await;
        });
        Ok(OriginConnection { sender, driver })
    }

    pub(crate) async fn send(
        &self,
        target: PublicTarget,
        request: Request<Full<Bytes>>,
        cancellation: &Shutdown,
    ) -> std::result::Result<PublicResponse, PublicConnectError> {
        #[cfg(test)]
        self.public_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let key = OriginKey::from_target(&target);
        let request = outbound_request(request).map_err(|_| PublicConnectError::OriginFailure)?;
        let connection = self.pool.take_if_reusable(&key, origin_connection_reusable);
        let mut connection = match connection {
            Some(connection) => connection,
            None => self.open_sender(&target, cancellation).await?,
        };
        let response = timeout(
            self.response_timeout,
            connection.sender.send_request(request),
        )
        .await
        .map_err(|_| PublicConnectError::OriginFailure)?
        .map_err(|_| PublicConnectError::OriginFailure)?;
        let (parts, incoming) = response.into_parts();
        Ok(PublicResponse {
            status: parts.status,
            headers: parts.headers,
            body: BoundedOriginBody::new(
                incoming,
                connection,
                key,
                self.pool.clone(),
                origin_connection_reusable,
                OriginBodyLimits {
                    origin: target.authority().to_string(),
                    timeout: self.response_timeout,
                    limit: self.response_limit,
                },
            )
            .boxed_unsync(),
        })
    }

    #[cfg(test)]
    async fn http(
        &self,
        target: PublicTarget,
        request: Request<Full<Bytes>>,
    ) -> std::result::Result<PublicResponse, PublicConnectError> {
        self.send(target, request, &Shutdown::new()).await
    }

    #[cfg(test)]
    async fn open(
        &self,
        target: PublicTarget,
    ) -> std::result::Result<PublicStream, PublicConnectError> {
        self.open_target(target, &Shutdown::new()).await
    }

    #[cfg(test)]
    pub(crate) async fn pool_len(&self) -> usize {
        tokio::task::yield_now().await;
        self.pool.len()
    }

    #[cfg(test)]
    pub(crate) fn public_calls(&self) -> usize {
        self.public_calls.load(std::sync::atomic::Ordering::SeqCst)
    }

    #[cfg(test)]
    pub(crate) fn active_drivers(&self) -> usize {
        self.active_drivers
            .load(std::sync::atomic::Ordering::SeqCst)
    }
}

fn outbound_request(request: Request<Full<Bytes>>) -> Result<Request<Full<Bytes>>> {
    let (mut parts, body) = request.into_parts();
    sanitize_hop_by_hop(&mut parts.headers);
    let path = parts
        .uri
        .path_and_query()
        .map_or("/", |value| value.as_str());
    parts.uri = path
        .parse::<Uri>()
        .map_err(|_| anyhow!("invalid origin path"))?;
    Ok(Request::from_parts(parts, body))
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    };

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::oneshot;

    use super::*;
    use crate::domain::target::{Target, classify};

    struct FakeResolver {
        answers: Mutex<VecDeque<Result<Vec<ResolvedAddress>>>>,
        calls: Mutex<Vec<(String, u16)>>,
    }
    impl FakeResolver {
        fn new(answers: Vec<Result<Vec<ResolvedAddress>>>) -> Self {
            Self {
                answers: Mutex::new(answers.into()),
                calls: Mutex::new(Vec::new()),
            }
        }
    }
    impl Resolver for FakeResolver {
        fn resolve(&self, host: String, port: u16) -> ResolveFuture {
            self.calls.lock().unwrap().push((host, port));
            let answer = self.answers.lock().unwrap().pop_front().unwrap();
            Box::pin(async move { answer })
        }
    }
    struct FakeDialer {
        calls: Mutex<Vec<SocketAddr>>,
        result: Mutex<Option<Result<PublicStream>>>,
    }
    impl FakeDialer {
        fn success() -> Self {
            let (stream, _) = tokio::io::duplex(1);
            Self {
                calls: Mutex::new(Vec::new()),
                result: Mutex::new(Some(Ok(PublicStream::Test(stream)))),
            }
        }
    }
    impl NumericDialer for FakeDialer {
        fn dial(&self, address: SocketAddr) -> DialFuture {
            self.calls.lock().unwrap().push(address);
            let result = self.result.lock().unwrap().take().unwrap();
            Box::pin(async move { result })
        }
    }
    struct PendingResolver;
    impl Resolver for PendingResolver {
        fn resolve(&self, _: String, _: u16) -> ResolveFuture {
            Box::pin(std::future::pending())
        }
    }
    struct PendingDialer;
    impl NumericDialer for PendingDialer {
        fn dial(&self, _: SocketAddr) -> DialFuture {
            Box::pin(std::future::pending())
        }
    }
    struct QueueDialer {
        calls: Mutex<Vec<SocketAddr>>,
        streams: Mutex<VecDeque<PublicStream>>,
    }
    impl QueueDialer {
        fn new(streams: Vec<PublicStream>) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                streams: Mutex::new(streams.into()),
            }
        }
    }
    impl NumericDialer for QueueDialer {
        fn dial(&self, address: SocketAddr) -> DialFuture {
            self.calls.lock().unwrap().push(address);
            let stream = self.streams.lock().unwrap().pop_front();
            Box::pin(async move { stream.ok_or_else(|| anyhow!("missing test stream")) })
        }
    }
    struct ScriptedDialer {
        calls: Mutex<Vec<SocketAddr>>,
        results: Mutex<VecDeque<Result<PublicStream>>>,
    }
    impl ScriptedDialer {
        fn new(results: Vec<Result<PublicStream>>) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                results: Mutex::new(results.into()),
            }
        }
    }
    impl NumericDialer for ScriptedDialer {
        fn dial(&self, address: SocketAddr) -> DialFuture {
            self.calls.lock().unwrap().push(address);
            let result = self.results.lock().unwrap().pop_front().unwrap();
            Box::pin(async move { result })
        }
    }
    struct DropFlag(Arc<AtomicBool>);
    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    struct DroppingResolver(Arc<AtomicBool>);
    impl Resolver for DroppingResolver {
        fn resolve(&self, _: String, _: u16) -> ResolveFuture {
            let dropped = self.0.clone();
            Box::pin(async move {
                let _drop = DropFlag(dropped);
                std::future::pending().await
            })
        }
    }
    struct DroppingDialer(Arc<AtomicBool>);
    impl NumericDialer for DroppingDialer {
        fn dial(&self, _: SocketAddr) -> DialFuture {
            let dropped = self.0.clone();
            Box::pin(async move {
                let _drop = DropFlag(dropped);
                std::future::pending().await
            })
        }
    }
    fn public_target(value: &str) -> PublicTarget {
        let Target::PublicHttp(target) = classify(&hyper::Method::GET, value.as_bytes()) else {
            panic!("expected public target");
        };
        target
    }
    fn parse_addresses(value: &str) -> Vec<ResolvedAddress> {
        value
            .split(',')
            .map(|address| ResolvedAddress::unscoped(address.parse().unwrap()))
            .collect()
    }

    #[tokio::test]
    async fn bounded_idle_pool_retires_public_drivers_and_reuses_live_key() {
        let targets =
            ["one", "two", "three"].map(|host| public_target(&format!("http://{host}.example/")));
        let resolver = Arc::new(FakeResolver::new(
            (0..targets.len())
                .map(|_| Ok(parse_addresses("8.8.8.8")))
                .collect(),
        ));
        let mut clients = Vec::new();
        let mut closed = Vec::new();
        for requests in [1, 2, 1] {
            let (client, mut peer) = tokio::io::duplex(1024);
            clients.push(PublicStream::Test(client));
            let (sender, receiver) = oneshot::channel();
            tokio::spawn(async move {
                for _ in 0..requests {
                    let mut bytes = [0; 1024];
                    assert!(
                        timeout(Duration::from_millis(200), peer.read(&mut bytes))
                            .await
                            .unwrap()
                            .unwrap()
                            > 0
                    );
                    peer.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
                        .await
                        .unwrap();
                }
                let mut byte = [0];
                let _ = sender.send(
                    timeout(Duration::from_millis(500), peer.read(&mut byte))
                        .await
                        .unwrap()
                        .unwrap(),
                );
            });
            closed.push(receiver);
        }
        let dialer = Arc::new(QueueDialer::new(clients));
        let connector = PublicConnector::with_pool_limits(
            resolver,
            dialer.clone(),
            std::num::NonZeroUsize::new(2).unwrap(),
            crate::connect::pool::NonZeroDuration::new(Duration::from_secs(5)).unwrap(),
        );
        for index in 0..3 {
            let response = connector
                .http(
                    targets[index].clone(),
                    Request::builder()
                        .uri(format!(
                            "http://{}.example/",
                            ["one", "two", "three"][index]
                        ))
                        .body(Full::new(Bytes::new()))
                        .unwrap(),
                )
                .await
                .unwrap();
            response.body.collect().await.unwrap();
        }
        assert_eq!(connector.pool_len().await, 2);
        assert_eq!(
            timeout(Duration::from_millis(50), &mut closed[0])
                .await
                .unwrap()
                .unwrap(),
            0
        );
        let response = connector
            .http(
                targets[1].clone(),
                Request::builder()
                    .uri("http://two.example/reused")
                    .body(Full::new(Bytes::new()))
                    .unwrap(),
            )
            .await
            .unwrap();
        response.body.collect().await.unwrap();
        assert_eq!(connector.public_calls(), 4);
        assert_eq!(dialer.calls.lock().unwrap().len(), 3);
        assert_eq!(connector.pool_len().await, 2);
        drop(connector);
        assert_eq!((&mut closed[1]).await.unwrap(), 0);
        assert_eq!((&mut closed[2]).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn public_idle_pool_expires_without_a_later_request() {
        let (client, mut peer) = tokio::io::duplex(1024);
        let resolver = Arc::new(FakeResolver::new(vec![Ok(parse_addresses("8.8.8.8"))]));
        let connector = PublicConnector::with_pool_limits(
            resolver,
            Arc::new(QueueDialer::new(vec![PublicStream::Test(client)])),
            std::num::NonZeroUsize::new(2).unwrap(),
            crate::connect::pool::NonZeroDuration::new(Duration::from_millis(20)).unwrap(),
        );
        let peer_task = tokio::spawn(async move {
            let mut request = [0; 1024];
            assert!(peer.read(&mut request).await.unwrap() > 0);
            peer.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
            let mut byte = [0];
            peer.read(&mut byte).await.unwrap()
        });
        let target = public_target("http://expiry.example/");
        connector
            .http(
                target,
                Request::builder()
                    .uri("http://expiry.example/")
                    .body(Full::new(Bytes::new()))
                    .unwrap(),
            )
            .await
            .unwrap();
        timeout(Duration::from_millis(500), async {
            while connector.active_drivers() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(connector.pool_len().await, 0);
        assert_eq!(
            timeout(Duration::from_millis(500), peer_task)
                .await
                .unwrap()
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn connection_close_response_is_not_reused() {
        let (first, mut first_peer) = tokio::io::duplex(1024);
        let (second, mut second_peer) = tokio::io::duplex(1024);
        let resolver = Arc::new(FakeResolver::new(vec![
            Ok(parse_addresses("8.8.8.8")),
            Ok(parse_addresses("8.8.8.8")),
        ]));
        let dialer = Arc::new(QueueDialer::new(vec![
            PublicStream::Test(first),
            PublicStream::Test(second),
        ]));
        let connector = PublicConnector::new(resolver, dialer.clone());
        let first_server = tokio::spawn(async move {
            let mut request = [0; 1024];
            assert!(
                timeout(Duration::from_millis(200), first_peer.read(&mut request))
                    .await
                    .unwrap()
                    .unwrap()
                    > 0
            );
            first_peer
                .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
        });
        let second_server = tokio::spawn(async move {
            let mut request = [0; 1024];
            assert!(
                timeout(Duration::from_millis(200), second_peer.read(&mut request))
                    .await
                    .unwrap()
                    .unwrap()
                    > 0
            );
            second_peer
                .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
        });
        let target = public_target("http://close.example/");
        for path in ["/first", "/second"] {
            connector
                .http(
                    target.clone(),
                    Request::builder()
                        .uri(format!("http://close.example{path}"))
                        .body(Full::new(Bytes::new()))
                        .unwrap(),
                )
                .await
                .unwrap();
        }
        timeout(Duration::from_millis(200), first_server)
            .await
            .unwrap()
            .unwrap();
        timeout(Duration::from_millis(200), second_server)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(dialer.calls.lock().unwrap().len(), 2);
    }

    #[test]
    fn classifies_the_address_fixture() {
        for row in include_str!("../../testdata/ip-addresses.tsv")
            .lines()
            .filter(|row| !row.is_empty() && !row.starts_with('#'))
        {
            let fields: Vec<_> = row.split('\t').collect();
            let [input, expected, dial] = fields.as_slice() else {
                panic!("bad row: {row}");
            };
            let answers = match *input {
                "empty answer set" => Vec::new(),
                _ => parse_addresses(input),
            };
            let result = ValidatedAnswers::from_dns(answers);
            let outcome = match &result {
                Ok(_) => "allow",
                Err(PublicConnectError::PolicyDenied) => "deny",
                Err(PublicConnectError::Unavailable) => "bad-gateway",
                Err(error) => panic!("unexpected fixture error {error:?}: {input}"),
            };
            assert_eq!(outcome, *expected, "{input}");
            if let Ok(addresses) = result {
                assert_eq!(
                    addresses
                        .0
                        .iter()
                        .map(|address| address.0.to_string())
                        .collect::<Vec<_>>()
                        .join(","),
                    *dial
                );
            }
        }
    }

    #[tokio::test]
    async fn resolves_once_and_retains_the_validated_answer_set() {
        let resolver = Arc::new(FakeResolver::new(vec![Ok(parse_addresses(
            "8.8.8.8,1.1.1.1",
        ))]));
        let (stream, _) = tokio::io::duplex(1);
        let dialer = Arc::new(ScriptedDialer::new(vec![
            Err(anyhow!("first refused")),
            Ok(PublicStream::Test(stream)),
        ]));
        let connector = PublicConnector::new(resolver.clone(), dialer.clone());
        let _stream = connector
            .open(public_target("http://API.Example.COM.:0080/"))
            .await
            .unwrap();
        assert_eq!(
            *resolver.calls.lock().unwrap(),
            vec![("api.example.com".to_owned(), 80)]
        );
        assert_eq!(
            *dialer.calls.lock().unwrap(),
            vec!["8.8.8.8:80".parse().unwrap(), "1.1.1.1:80".parse().unwrap()]
        );
    }

    #[tokio::test]
    async fn connect_uses_only_an_explicit_authority_port() {
        let resolver = Arc::new(FakeResolver::new(vec![Ok(parse_addresses("8.8.8.8"))]));
        let (stream, _) = tokio::io::duplex(1);
        let dialer = Arc::new(QueueDialer::new(vec![PublicStream::Test(stream)]));
        let connector = PublicConnector::new(resolver, dialer.clone());
        let Target::PublicConnect(explicit_target) =
            classify(&hyper::Method::CONNECT, b"allowed.example:8443")
        else {
            panic!("expected public CONNECT target");
        };
        let _ = connector.open(explicit_target).await.unwrap();
        assert_eq!(
            *dialer.calls.lock().unwrap(),
            vec!["8.8.8.8:8443".parse().unwrap()]
        );
    }

    #[tokio::test]
    async fn rejected_answers_never_dial() {
        for (answers, expected) in [
            (Vec::new(), PublicConnectError::Unavailable),
            (
                parse_addresses("8.8.8.8,127.0.0.1"),
                PublicConnectError::PolicyDenied,
            ),
            (
                parse_addresses("8.8.8.8,::ffff:8.8.8.8"),
                PublicConnectError::PolicyDenied,
            ),
            (parse_addresses("::1"), PublicConnectError::PolicyDenied),
        ] {
            let resolver = Arc::new(FakeResolver::new(vec![Ok(answers)]));
            let dialer = Arc::new(FakeDialer::success());
            let connector = PublicConnector::new(resolver.clone(), dialer.clone());
            assert_eq!(
                connector
                    .open(public_target("http://allowed.example/"))
                    .await
                    .unwrap_err(),
                expected
            );
            assert_eq!(resolver.calls.lock().unwrap().len(), 1);
            assert!(dialer.calls.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn literal_skips_resolution_and_uses_the_same_boundary() {
        let resolver = Arc::new(FakeResolver::new(Vec::new()));
        let dialer = Arc::new(FakeDialer::success());
        let connector = PublicConnector::new(resolver.clone(), dialer.clone());
        let _stream = connector
            .open(public_target("http://8.8.8.8/"))
            .await
            .unwrap();
        assert!(resolver.calls.lock().unwrap().is_empty());
        assert_eq!(
            *dialer.calls.lock().unwrap(),
            vec!["8.8.8.8:80".parse().unwrap()]
        );

        let dialer = Arc::new(FakeDialer::success());
        let connector = PublicConnector::new(resolver.clone(), dialer.clone());
        assert_eq!(
            connector
                .open(public_target("http://[2001:db8::1]/"))
                .await
                .unwrap_err(),
            PublicConnectError::PolicyDenied
        );
        assert!(resolver.calls.lock().unwrap().is_empty());
        assert!(dialer.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn scoped_and_mapped_literals_reach_the_boundary_without_network_work() {
        for target in ["http://[fe80::1%25en0]/", "http://[::ffff:8.8.8.8]/"] {
            let resolver = Arc::new(FakeResolver::new(Vec::new()));
            let dialer = Arc::new(FakeDialer::success());
            let connector = PublicConnector::new(resolver.clone(), dialer.clone());
            assert_eq!(
                connector.open(public_target(target)).await.unwrap_err(),
                PublicConnectError::PolicyDenied,
                "{target}"
            );
            assert!(resolver.calls.lock().unwrap().is_empty(), "{target}");
            assert!(dialer.calls.lock().unwrap().is_empty(), "{target}");
        }
    }

    #[test]
    fn dns_answer_count_accepts_one_through_sixty_four_only() {
        for (count, expected) in [
            (0, Err(PublicConnectError::Unavailable)),
            (1, Ok(())),
            (64, Ok(())),
            (65, Err(PublicConnectError::PolicyDenied)),
        ] {
            let result = ValidatedAnswers::from_dns(vec![
                ResolvedAddress::unscoped(
                    "8.8.8.8".parse().unwrap()
                );
                count
            ])
            .map(|_| ());
            assert_eq!(result, expected, "answer count {count}");
        }
    }

    #[test]
    fn scoped_dns_answers_are_denied_without_reinterpretation() {
        assert_eq!(
            ValidatedAnswers::from_dns(vec![ResolvedAddress::scoped(
                "2606:4700:4700::1111".parse().unwrap(),
                4,
            )]),
            Err(PublicConnectError::PolicyDenied)
        );
    }

    #[tokio::test]
    async fn refusal_falls_back_across_address_families() {
        let resolver = Arc::new(FakeResolver::new(vec![Ok(parse_addresses(
            "2606:4700:4700::1111,8.8.8.8",
        ))]));
        let (stream, _) = tokio::io::duplex(1);
        let dialer = Arc::new(ScriptedDialer::new(vec![
            Err(anyhow!("IPv6 refused")),
            Ok(PublicStream::Test(stream)),
        ]));
        let connector = PublicConnector::new(resolver.clone(), dialer.clone());

        connector
            .open(public_target("http://allowed.example:8080/"))
            .await
            .unwrap();

        assert_eq!(resolver.calls.lock().unwrap().len(), 1);
        assert_eq!(
            *dialer.calls.lock().unwrap(),
            vec![
                "[2606:4700:4700::1111]:8080".parse().unwrap(),
                "8.8.8.8:8080".parse().unwrap(),
            ]
        );
    }

    #[tokio::test]
    async fn resolution_failure_and_exhausted_refusals_are_bad_gateway() {
        let resolver = Arc::new(FakeResolver::new(vec![Err(anyhow!("resolver input"))]));
        let dialer = Arc::new(FakeDialer::success());
        let connector = PublicConnector::new(resolver, dialer.clone());
        assert_eq!(
            connector
                .open(public_target("http://allowed.example/"))
                .await
                .unwrap_err(),
            PublicConnectError::Unavailable
        );
        assert!(dialer.calls.lock().unwrap().is_empty());

        let resolver = Arc::new(FakeResolver::new(vec![Ok(parse_addresses(
            "8.8.8.8,1.1.1.1",
        ))]));
        let dialer = Arc::new(ScriptedDialer::new(vec![
            Err(anyhow!("first refused")),
            Err(anyhow!("second refused")),
        ]));
        let connector = PublicConnector::new(resolver, dialer.clone());
        assert_eq!(
            connector
                .open(public_target("http://allowed.example/"))
                .await
                .unwrap_err(),
            PublicConnectError::Unavailable
        );
        assert_eq!(dialer.calls.lock().unwrap().len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn resolution_and_every_dial_share_one_deadline() {
        struct SlowResolver;
        impl Resolver for SlowResolver {
            fn resolve(&self, _: String, _: u16) -> ResolveFuture {
                Box::pin(async {
                    tokio::time::sleep(Duration::from_secs(6)).await;
                    Ok(parse_addresses("8.8.8.8,1.1.1.1"))
                })
            }
        }
        struct SlowDialer {
            calls: Arc<Mutex<Vec<(SocketAddr, Duration)>>>,
            started: Instant,
        }
        impl NumericDialer for SlowDialer {
            fn dial(&self, address: SocketAddr) -> DialFuture {
                self.calls
                    .lock()
                    .unwrap()
                    .push((address, self.started.elapsed()));
                let attempt = self.calls.lock().unwrap().len();
                Box::pin(async move {
                    if attempt == 1 {
                        tokio::time::sleep(Duration::from_secs(3)).await;
                        Err(anyhow!("first refused"))
                    } else {
                        std::future::pending().await
                    }
                })
            }
        }
        let resolver = Arc::new(PendingResolver);
        let dialer = Arc::new(PendingDialer);
        let cancellation = Shutdown::new();
        let started = Instant::now();
        assert_eq!(
            PublicConnector::open_with_budget(
                resolver,
                dialer,
                public_target("http://allowed.example/"),
                &cancellation,
                Duration::from_secs(10),
            )
            .await
            .unwrap_err(),
            PublicConnectError::DeadlineExceeded
        );
        assert_eq!(started.elapsed(), Duration::from_secs(10));

        let calls = Arc::new(Mutex::new(Vec::new()));
        let started = Instant::now();
        let error = PublicConnector::open_with_budget(
            Arc::new(SlowResolver),
            Arc::new(SlowDialer {
                calls: calls.clone(),
                started,
            }),
            public_target("http://allowed.example/"),
            &cancellation,
            Duration::from_secs(10),
        )
        .await
        .unwrap_err();
        assert_eq!(error, PublicConnectError::DeadlineExceeded);
        assert_eq!(started.elapsed(), Duration::from_secs(10));
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                ("8.8.8.8:80".parse().unwrap(), Duration::from_secs(6)),
                ("1.1.1.1:80".parse().unwrap(), Duration::from_secs(9)),
            ]
        );
    }

    #[tokio::test]
    async fn cancellation_drops_resolution_and_current_dial_futures() {
        let resolver_dropped = Arc::new(AtomicBool::new(false));
        let cancellation = Shutdown::new();
        let task_cancellation = cancellation.clone();
        let task_resolver_dropped = resolver_dropped.clone();
        let task = tokio::spawn(async move {
            PublicConnector::open_with_budget(
                Arc::new(DroppingResolver(task_resolver_dropped)),
                Arc::new(FakeDialer::success()),
                public_target("http://allowed.example/"),
                &task_cancellation,
                Duration::from_secs(30),
            )
            .await
        });
        tokio::task::yield_now().await;
        cancellation.request();
        assert_eq!(
            task.await.unwrap().unwrap_err(),
            PublicConnectError::Cancelled
        );
        assert!(resolver_dropped.load(Ordering::SeqCst));

        let dial_dropped = Arc::new(AtomicBool::new(false));
        let cancellation = Shutdown::new();
        let task_cancellation = cancellation.clone();
        let task_dial_dropped = dial_dropped.clone();
        let task = tokio::spawn(async move {
            PublicConnector::open_with_budget(
                Arc::new(FakeResolver::new(vec![Ok(parse_addresses("8.8.8.8"))])),
                Arc::new(DroppingDialer(task_dial_dropped)),
                public_target("http://allowed.example/"),
                &task_cancellation,
                Duration::from_secs(30),
            )
            .await
        });
        tokio::task::yield_now().await;
        cancellation.request();
        assert_eq!(
            task.await.unwrap().unwrap_err(),
            PublicConnectError::Cancelled
        );
        assert!(dial_dropped.load(Ordering::SeqCst));
    }

    #[test]
    fn origin_key_uses_normalized_public_identity() {
        let first = OriginKey::from_target(&public_target("http://API.Example.COM.:0080/"));
        let same = OriginKey::from_target(&public_target("http://api.example.com/"));
        let port = OriginKey::from_target(&public_target("http://api.example.com:81/"));
        let unscoped = OriginKey::from_target(&public_target("http://[2606:4700:4700::1111]/"));
        let scoped =
            OriginKey::from_target(&public_target("http://[2606:4700:4700::1111%25eth0]/"));
        assert_eq!(first, same);
        assert_ne!(first, port);
        assert_ne!(unscoped, scoped);
    }

    #[test]
    fn outbound_request_strips_hop_by_hop_fields() {
        let request = Request::builder()
            .method("POST")
            .uri("http://api.example.com:8080/path?q=one")
            .header("host", "api.example.com:8080")
            .header("connection", "X-Remove")
            .header("x-remove", "removed")
            .header("proxy-connection", "close")
            .header("proxy-authorization", "Basic ignored")
            .body(Full::new(Bytes::from_static(b"body")))
            .unwrap();
        let request = outbound_request(request).unwrap();
        assert_eq!(request.uri(), "/path?q=one");
        assert!(!request.headers().contains_key("connection"));
        assert!(!request.headers().contains_key("x-remove"));
        assert_eq!(request.headers()["host"], "api.example.com:8080");
        assert!(!request.headers().contains_key("proxy-connection"));
        assert!(!request.headers().contains_key("proxy-authorization"));
    }

    #[tokio::test]
    async fn bounded_origin_response_errors_do_not_retain_pool_entries() {
        let (client, mut peer) = tokio::io::duplex(4096);
        let resolver = Arc::new(FakeResolver::new(vec![Ok(parse_addresses("8.8.8.8"))]));
        let dialer = Arc::new(QueueDialer::new(vec![PublicStream::Test(client)]));
        let connector =
            PublicConnector::with_response_limits(resolver, dialer, Duration::from_millis(80), 3);
        let peer_task = tokio::spawn(async move {
            let mut request = [0; 1024];
            let _ = timeout(Duration::from_millis(300), peer.read(&mut request))
                .await
                .unwrap()
                .unwrap();
            peer.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\ntool")
                .await
                .unwrap();
        });
        let request = Request::builder()
            .uri("http://allowed.example/")
            .body(Full::new(Bytes::new()))
            .unwrap();
        let mut response = connector
            .http(public_target("http://allowed.example/"), request)
            .await
            .unwrap();
        assert!(response.body.frame().await.unwrap().is_err());
        timeout(Duration::from_millis(300), peer_task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(connector.pool_len().await, 0);

        let (client, mut peer) = tokio::io::duplex(4096);
        let resolver = Arc::new(FakeResolver::new(vec![Ok(parse_addresses("8.8.8.8"))]));
        let dialer = Arc::new(QueueDialer::new(vec![PublicStream::Test(client)]));
        let connector =
            PublicConnector::with_response_limits(resolver, dialer, Duration::from_millis(50), 16);
        let peer_task = tokio::spawn(async move {
            let mut request = [0; 1024];
            let _ = timeout(Duration::from_millis(300), peer.read(&mut request))
                .await
                .unwrap()
                .unwrap();
            tokio::time::sleep(Duration::from_millis(120)).await;
        });
        let request = Request::builder()
            .uri("http://allowed.example/")
            .body(Full::new(Bytes::new()))
            .unwrap();
        assert!(
            connector
                .http(public_target("http://allowed.example/"), request)
                .await
                .is_err()
        );
        timeout(Duration::from_millis(300), peer_task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(connector.pool_len().await, 0);
    }
}
