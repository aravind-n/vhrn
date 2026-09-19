//! Validated numeric public connection setup.

#[cfg(test)]
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::client::conn::http1;
use hyper::{Request, Uri};
use hyper_util::rt::TokioIo;
use rustls::pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_rustls::TlsConnector;

use crate::connect::origin_body::{BoundedOriginBody, OriginBodyLimits, SharedOriginResponse};
use crate::connect::pool::IdlePool;
use crate::domain::target::{PublicHost, PublicTarget, Scheme};
use crate::headers::sanitize_hop_by_hop;

const RESOLVE_TIMEOUT: Duration = Duration::from_secs(10);
const DIAL_TIMEOUT: Duration = Duration::from_secs(10);
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// A validated, numeric public connection.
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

enum OriginIo {
    Plain(PublicStream),
    Tls(Box<tokio_rustls::client::TlsStream<PublicStream>>),
}

impl AsyncRead for OriginIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.as_mut().get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_read(cx, buffer),
            Self::Tls(stream) => Pin::new(stream).poll_read(cx, buffer),
        }
    }
}

impl AsyncWrite for OriginIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.as_mut().get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_write(cx, buffer),
            Self::Tls(stream) => Pin::new(stream).poll_write(cx, buffer),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.as_mut().get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_flush(cx),
            Self::Tls(stream) => Pin::new(stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.as_mut().get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_shutdown(cx),
            Self::Tls(stream) => Pin::new(stream).poll_shutdown(cx),
        }
    }
}

#[cfg(test)]
pub(crate) type ResolveFuture = Pin<Box<dyn Future<Output = Result<Vec<IpAddr>>> + Send>>;
#[cfg(test)]
pub(crate) type DialFuture = Pin<Box<dyn Future<Output = Result<PublicStream>> + Send>>;

/// Resolves one normalized public name for one connection attempt.
#[cfg(test)]
pub(crate) trait Resolver: Send + Sync + 'static {
    fn resolve(&self, host: String, port: u16) -> ResolveFuture;
}

/// Opens a connection to one validated numeric socket address.
#[cfg(test)]
pub(crate) trait NumericDialer: Send + Sync + 'static {
    fn dial(&self, address: SocketAddr) -> DialFuture;
}

#[cfg(test)]
struct SystemResolver;
#[cfg(test)]
impl Resolver for SystemResolver {
    fn resolve(&self, host: String, port: u16) -> ResolveFuture {
        Box::pin(async move {
            let addresses = tokio::net::lookup_host((host.as_str(), port))
                .await
                .map_err(|_| anyhow!("public name resolution failed"))?;
            Ok(addresses.map(|address| address.ip()).collect())
        })
    }
}

#[cfg(test)]
struct SystemDialer;
#[cfg(test)]
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

/// Stable identity for a public origin connection pool.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct OriginKey {
    scheme: Scheme,
    host: PublicHost,
    port: u16,
}

impl OriginKey {
    #[must_use]
    pub fn from_target(target: &PublicTarget) -> Self {
        Self {
            scheme: if target.secure() {
                Scheme::Https
            } else {
                Scheme::Http
            },
            host: target.host().clone(),
            port: target.port(),
        }
    }
}

/// An address which is safe for a public origin dial.
///
/// The exclusions below mirror IANA's IPv4 and IPv6 Special-Purpose Address
/// Registries.  Addresses absent from those registries are globally routable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GloballyRoutableIp(IpAddr);

impl GloballyRoutableIp {
    fn new(address: IpAddr) -> Option<Self> {
        let address = canonical_ip(address);
        (!is_special_purpose(address)).then_some(Self(address))
    }

    fn socket_addr(self, port: u16) -> SocketAddr {
        SocketAddr::new(self.0, port)
    }
}

fn validated_answers(answers: Vec<IpAddr>) -> Result<GloballyRoutableIp> {
    let mut first = None;
    for answer in answers {
        let Some(address) = GloballyRoutableIp::new(answer) else {
            return Err(anyhow!("public address rejected"));
        };
        first.get_or_insert(address);
    }
    first.ok_or_else(|| anyhow!("public name returned no addresses"))
}

fn canonical_ip(address: IpAddr) -> IpAddr {
    match address {
        IpAddr::V6(address) => address
            .to_ipv4_mapped()
            .map_or(IpAddr::V6(address), IpAddr::V4),
        IpAddr::V4(address) => IpAddr::V4(address),
    }
}

fn is_special_purpose(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => ipv4_special(address),
        IpAddr::V6(address) => ipv6_special(address),
    }
}

fn ipv4_special(address: Ipv4Addr) -> bool {
    // IANA IPv4 Special-Purpose Address Registry, 2026-09-18.
    let blocked = [
        (Ipv4Addr::UNSPECIFIED, 8),
        (Ipv4Addr::new(10, 0, 0, 0), 8),
        (Ipv4Addr::new(100, 64, 0, 0), 10),
        (Ipv4Addr::new(127, 0, 0, 0), 8),
        (Ipv4Addr::new(169, 254, 0, 0), 16),
        (Ipv4Addr::new(172, 16, 0, 0), 12),
        (Ipv4Addr::new(192, 0, 0, 0), 24),
        (Ipv4Addr::new(192, 0, 2, 0), 24),
        (Ipv4Addr::new(192, 88, 99, 0), 24),
        (Ipv4Addr::new(192, 168, 0, 0), 16),
        (Ipv4Addr::new(198, 18, 0, 0), 15),
        (Ipv4Addr::new(198, 51, 100, 0), 24),
        (Ipv4Addr::new(203, 0, 113, 0), 24),
        (Ipv4Addr::new(224, 0, 0, 0), 4),
        (Ipv4Addr::new(240, 0, 0, 0), 4),
    ];
    // PCP and TURN anycast are the two globally reachable exceptions in 192.0.0/24.
    address != Ipv4Addr::new(192, 0, 0, 9)
        && address != Ipv4Addr::new(192, 0, 0, 10)
        && blocked
            .into_iter()
            .any(|(network, prefix)| ipv4_in_prefix(address, network, prefix))
}

fn ipv6_special(address: std::net::Ipv6Addr) -> bool {
    let in_prefix = |network, prefix| ipv6_in_prefix(address, network, prefix);
    if in_prefix([0, 0, 0, 0, 0, 0, 0, 0], 96)
        || in_prefix([0, 0, 0, 0, 0, 0, 0, 1], 128)
        // IANA's IPv4-translated prefix is distinct from IPv4-mapped
        // addresses, which `canonical_ip` deliberately converts to IPv4.
        || in_prefix([0, 0, 0, 0, 0xffff, 0, 0, 0], 96)
        || in_prefix([0x64, 0xff9b, 1, 0, 0, 0, 0, 0], 48)
        || in_prefix([0x100, 0, 0, 0, 0, 0, 0, 0], 64)
        || in_prefix([0x100, 0, 0, 1, 0, 0, 0, 0], 64)
        || in_prefix([0x5f00, 0, 0, 0, 0, 0, 0, 0], 16)
        || in_prefix([0x2002, 0, 0, 0, 0, 0, 0, 0], 16)
        || in_prefix([0x2001, 0xdb8, 0, 0, 0, 0, 0, 0], 32)
        || in_prefix([0x3fff, 0, 0, 0, 0, 0, 0, 0], 20)
        || in_prefix([0xfc00, 0, 0, 0, 0, 0, 0, 0], 7)
        || in_prefix([0xfe80, 0, 0, 0, 0, 0, 0, 0], 10)
        || in_prefix([0xff00, 0, 0, 0, 0, 0, 0, 0], 8)
    {
        return true;
    }
    if in_prefix([0x64, 0xff9b, 0, 0, 0, 0, 0, 0], 96) {
        return false;
    }
    if !in_prefix([0x2001, 0, 0, 0, 0, 0, 0, 0], 23) {
        return false;
    }
    !in_prefix([0x2001, 1, 0, 0, 0, 0, 0, 1], 128)
        && !in_prefix([0x2001, 1, 0, 0, 0, 0, 0, 2], 128)
        && !in_prefix([0x2001, 1, 0, 0, 0, 0, 0, 3], 128)
        && !in_prefix([0x2001, 0, 3, 0, 0, 0, 0, 0], 32)
        && !in_prefix([0x2001, 4, 0x112, 0, 0, 0, 0, 0], 48)
        && !in_prefix([0x2001, 0x20, 0, 0, 0, 0, 0, 0], 28)
        && !in_prefix([0x2001, 0x30, 0, 0, 0, 0, 0, 0], 28)
}

fn ipv4_in_prefix(address: Ipv4Addr, network: Ipv4Addr, prefix: u8) -> bool {
    let mask = u32::MAX << (32 - u32::from(prefix));
    u32::from(address) & mask == u32::from(network) & mask
}
fn ipv6_in_prefix(address: std::net::Ipv6Addr, network: [u16; 8], prefix: u8) -> bool {
    let bits = u128::from_be_bytes(address.octets());
    let base = u128::from_be_bytes(std::net::Ipv6Addr::from(network).octets());
    let mask = u128::MAX << (128 - u32::from(prefix));
    bits & mask == base & mask
}

/// Public connector that resolves and opens one validated numeric stream.
pub(crate) struct PublicConnector {
    #[cfg(test)]
    resolver: Arc<dyn Resolver>,
    #[cfg(test)]
    dialer: Arc<dyn NumericDialer>,
    pool: IdlePool<OriginKey, OriginConnection>,
    response_timeout: Duration,
    response_limit: usize,
    tls_config: Arc<rustls::ClientConfig>,
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
    #[cfg(not(test))]
    pub(crate) fn system_with_tls(tls_config: Arc<rustls::ClientConfig>) -> Self {
        Self {
            pool: IdlePool::new(),
            response_timeout: HTTP_TIMEOUT,
            response_limit: MAX_RESPONSE_BYTES,
            tls_config,
        }
    }

    #[cfg(test)]
    pub(crate) fn system() -> Self {
        Self::new(Arc::new(SystemResolver), Arc::new(SystemDialer))
    }

    #[cfg(test)]
    pub(crate) fn system_with_tls(_: Arc<rustls::ClientConfig>) -> Self {
        Self::system()
    }

    #[cfg(test)]
    pub(crate) fn new(resolver: Arc<dyn Resolver>, dialer: Arc<dyn NumericDialer>) -> Self {
        Self::new_with_pool(resolver, dialer, IdlePool::new())
    }

    #[cfg(test)]
    pub(crate) fn test_with_stream(stream: tokio::io::DuplexStream) -> Self {
        struct OneResolver;
        impl Resolver for OneResolver {
            fn resolve(&self, _: String, _: u16) -> ResolveFuture {
                Box::pin(async { Ok(vec!["8.8.8.8".parse().expect("test address")]) })
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

    #[cfg(test)]
    fn new_with_pool(
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
            tls_config: crate::connect::tls::production_client_config()
                .expect("test TLS configuration"),
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
        Self::new_with_pool(resolver, dialer, IdlePool::with_limits(capacity, lifetime))
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
            tls_config: crate::connect::tls::production_client_config()
                .expect("test TLS configuration"),
            public_calls: std::sync::atomic::AtomicUsize::new(0),
            active_drivers: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    #[cfg(test)]
    async fn open_with(
        resolver: Arc<dyn Resolver>,
        dialer: Arc<dyn NumericDialer>,
        target: PublicTarget,
    ) -> Result<PublicStream> {
        Self::open_with_deadlines(resolver, dialer, target, RESOLVE_TIMEOUT, DIAL_TIMEOUT).await
    }

    #[cfg(test)]
    async fn open_with_deadlines(
        resolver: Arc<dyn Resolver>,
        dialer: Arc<dyn NumericDialer>,
        target: PublicTarget,
        resolve_timeout: Duration,
        dial_timeout: Duration,
    ) -> Result<PublicStream> {
        let answers = timeout(
            resolve_timeout,
            resolver.resolve(target.host().to_string(), target.port()),
        )
        .await
        .map_err(|_| anyhow!("public name resolution timed out"))??;
        let address = validated_answers(answers)?.socket_addr(target.port());
        timeout(dial_timeout, dialer.dial(address))
            .await
            .map_err(|_| anyhow!("public connection timed out"))?
    }

    #[cfg(not(test))]
    async fn open_target(&self, target: PublicTarget) -> Result<PublicStream> {
        let answers = timeout(
            RESOLVE_TIMEOUT,
            tokio::net::lookup_host((target.host().to_string(), target.port())),
        )
        .await
        .map_err(|_| anyhow!("public name resolution timed out"))?
        .map_err(|_| anyhow!("public name resolution failed"))?
        .map(|address| address.ip())
        .collect();
        let address = validated_answers(answers)?.socket_addr(target.port());
        let stream = timeout(DIAL_TIMEOUT, TcpStream::connect(address))
            .await
            .map_err(|_| anyhow!("public connection timed out"))?
            .map_err(|_| anyhow!("public connection failed"))?;
        Ok(PublicStream::Tcp(stream))
    }

    #[cfg(test)]
    async fn open_target(&self, target: PublicTarget) -> Result<PublicStream> {
        Self::open_with(self.resolver.clone(), self.dialer.clone(), target).await
    }

    pub(crate) async fn connect_target(&self, target: PublicTarget) -> Result<PublicStream> {
        self.open_target(target).await
    }

    async fn open_sender(&self, target: &PublicTarget) -> Result<OriginConnection> {
        let stream = self.open_target(target.clone()).await?;
        let io = if target.secure() {
            let server_name = match target.host() {
                crate::domain::target::PublicHost::Dns(name) => {
                    ServerName::try_from(name.to_string()).map_err(|_| anyhow!("TLS identity"))?
                }
                crate::domain::target::PublicHost::Ip(address) => ServerName::from(*address),
            };
            let tls = timeout(
                HTTP_TIMEOUT,
                TlsConnector::from(self.tls_config.clone()).connect(server_name, stream),
            )
            .await
            .map_err(|_| anyhow!("TLS handshake timeout"))
            .context("TLS handshake")??;
            OriginIo::Tls(Box::new(tls))
        } else {
            OriginIo::Plain(stream)
        };
        let (sender, connection) = timeout(HTTP_TIMEOUT, http1::handshake(TokioIo::new(io)))
            .await
            .map_err(|_| anyhow!("origin handshake timed out"))??;
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
    ) -> Result<PublicResponse> {
        #[cfg(test)]
        self.public_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let key = OriginKey::from_target(&target);
        let request = outbound_request(request)?;
        let connection = self.pool.take_if_reusable(&key, origin_connection_reusable);
        let mut connection = match connection {
            Some(connection) => connection,
            None => self.open_sender(&target).await?,
        };
        let response = timeout(
            self.response_timeout,
            connection.sender.send_request(request),
        )
        .await
        .with_context(|| format!("origin response timed out for {}", target.authority()))?
        .with_context(|| format!("receive origin response from {}", target.authority()))?;
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
    ) -> Result<PublicResponse> {
        self.send(target, request).await
    }

    #[cfg(test)]
    async fn open(&self, target: PublicTarget) -> Result<PublicStream> {
        Self::open_with(self.resolver.clone(), self.dialer.clone(), target).await
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
    use std::sync::Mutex;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::oneshot;

    use super::*;
    use crate::domain::target::{Target, classify};

    struct FakeResolver {
        answers: Mutex<VecDeque<Result<Vec<IpAddr>>>>,
        calls: Mutex<Vec<(String, u16)>>,
    }
    impl FakeResolver {
        fn new(answers: Vec<Result<Vec<IpAddr>>>) -> Self {
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
    fn public_target(value: &str) -> PublicTarget {
        let Target::PublicHttp(target) = classify(&hyper::Method::GET, &value.parse().unwrap())
        else {
            panic!("expected public target");
        };
        target
    }
    fn parse_addresses(value: &str) -> Vec<IpAddr> {
        value
            .split(',')
            .map(|address| address.parse().unwrap())
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
            .filter(|row| !row.starts_with('#'))
        {
            let fields: Vec<_> = row.split('\t').collect();
            let [input, expected, dial] = fields.as_slice() else {
                panic!("bad row: {row}");
            };
            let answers = match *input {
                "empty answer set" => Vec::new(),
                _ => parse_addresses(input),
            };
            let result = validated_answers(answers);
            assert_eq!(result.is_ok(), *expected == "allow", "{input}");
            if let Ok(address) = result {
                assert_eq!(address.0.to_string(), *dial);
            }
        }
    }

    #[tokio::test]
    async fn resolves_once_validates_all_answers_then_dials_first() {
        let resolver = Arc::new(FakeResolver::new(vec![Ok(parse_addresses(
            "8.8.8.8,1.1.1.1",
        ))]));
        let dialer = Arc::new(FakeDialer::success());
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
            vec!["8.8.8.8:80".parse().unwrap()]
        );
    }

    #[tokio::test]
    async fn connect_uses_default_or_explicit_authority_number() {
        let resolver = Arc::new(FakeResolver::new(vec![
            Ok(parse_addresses("8.8.8.8")),
            Ok(parse_addresses("8.8.8.8")),
        ]));
        let (first, _) = tokio::io::duplex(1);
        let (second, _) = tokio::io::duplex(1);
        let dialer = Arc::new(QueueDialer::new(vec![
            PublicStream::Test(first),
            PublicStream::Test(second),
        ]));
        let connector = PublicConnector::new(resolver, dialer.clone());
        let Target::PublicConnect(default_target) =
            classify(&hyper::Method::CONNECT, &"allowed.example".parse().unwrap())
        else {
            panic!("expected public CONNECT target");
        };
        let Target::PublicConnect(explicit_target) = classify(
            &hyper::Method::CONNECT,
            &"allowed.example:8443".parse().unwrap(),
        ) else {
            panic!("expected public CONNECT target");
        };
        let _ = connector.open(default_target).await.unwrap();
        let _ = connector.open(explicit_target).await.unwrap();
        assert_eq!(
            *dialer.calls.lock().unwrap(),
            vec![
                "8.8.8.8:443".parse().unwrap(),
                "8.8.8.8:8443".parse().unwrap(),
            ]
        );
    }

    #[tokio::test]
    async fn rejected_answers_never_dial() {
        for answers in [
            Vec::new(),
            parse_addresses("8.8.8.8,127.0.0.1"),
            parse_addresses("8.8.8.8,::ffff:0:127.0.0.1"),
            parse_addresses("::1"),
        ] {
            let resolver = Arc::new(FakeResolver::new(vec![Ok(answers)]));
            let dialer = Arc::new(FakeDialer::success());
            let connector = PublicConnector::new(resolver.clone(), dialer.clone());
            assert!(
                connector
                    .open(public_target("http://allowed.example/"))
                    .await
                    .is_err()
            );
            assert_eq!(resolver.calls.lock().unwrap().len(), 1);
            assert!(dialer.calls.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn mapped_public_address_dials_ipv4() {
        let resolver = Arc::new(FakeResolver::new(vec![Ok(parse_addresses(
            "::ffff:8.8.8.8",
        ))]));
        let dialer = Arc::new(FakeDialer::success());
        let connector = PublicConnector::new(resolver, dialer.clone());
        let _stream = connector
            .open(public_target("http://allowed.example/"))
            .await
            .unwrap();
        assert_eq!(
            *dialer.calls.lock().unwrap(),
            vec!["8.8.8.8:80".parse().unwrap()]
        );
    }

    #[tokio::test]
    async fn resolver_and_dial_failures_are_bounded() {
        let resolver = Arc::new(FakeResolver::new(vec![Err(anyhow!("resolver input"))]));
        let dialer = Arc::new(FakeDialer::success());
        let connector = PublicConnector::new(resolver, dialer.clone());
        assert!(
            connector
                .open(public_target("http://allowed.example/"))
                .await
                .is_err()
        );
        assert!(dialer.calls.lock().unwrap().is_empty());

        let resolver = Arc::new(FakeResolver::new(vec![Ok(parse_addresses("8.8.8.8"))]));
        let dialer = Arc::new(FakeDialer {
            calls: Mutex::new(Vec::new()),
            result: Mutex::new(Some(Err(anyhow!("dial input")))),
        });
        let connector = PublicConnector::new(resolver, dialer.clone());
        assert!(
            connector
                .open(public_target("http://allowed.example/"))
                .await
                .is_err()
        );
        assert_eq!(dialer.calls.lock().unwrap().len(), 1);

        let resolver = Arc::new(PendingResolver);
        let dialer = Arc::new(FakeDialer::success());
        assert!(
            PublicConnector::open_with_deadlines(
                resolver,
                dialer.clone(),
                public_target("http://allowed.example/"),
                Duration::from_millis(1),
                Duration::from_millis(1),
            )
            .await
            .is_err()
        );
        assert!(dialer.calls.lock().unwrap().is_empty());

        let resolver = Arc::new(FakeResolver::new(vec![Ok(parse_addresses("8.8.8.8"))]));
        let dialer = Arc::new(PendingDialer);
        assert!(
            PublicConnector::open_with_deadlines(
                resolver,
                dialer,
                public_target("http://allowed.example/"),
                Duration::from_millis(1),
                Duration::from_millis(1),
            )
            .await
            .is_err()
        );
    }

    #[test]
    fn origin_key_uses_normalized_public_identity() {
        let first = OriginKey::from_target(&public_target("http://API.Example.COM.:0080/"));
        let same = OriginKey::from_target(&public_target("http://api.example.com/"));
        let secure = OriginKey::from_target(&public_target("https://api.example.com/"));
        let port = OriginKey::from_target(&public_target("http://api.example.com:81/"));
        assert_eq!(first, same);
        assert_ne!(first, secure);
        assert_ne!(first, port);
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
    async fn secure_origin_never_receives_an_http_request_before_tls() {
        struct ProbeDialer(Mutex<Option<PublicStream>>);
        impl NumericDialer for ProbeDialer {
            fn dial(&self, _: SocketAddr) -> DialFuture {
                let stream = self.0.lock().unwrap().take().unwrap();
                Box::pin(async move { Ok(stream) })
            }
        }

        let (client, mut peer) = tokio::io::duplex(1024);
        let (observed, receiver) = oneshot::channel();
        tokio::spawn(async move {
            let mut bytes = vec![0; 1024];
            let read = timeout(Duration::from_millis(500), peer.read(&mut bytes))
                .await
                .unwrap()
                .unwrap();
            bytes.truncate(read);
            let _ = observed.send(bytes);
        });
        let resolver = Arc::new(FakeResolver::new(vec![Ok(parse_addresses("8.8.8.8"))]));
        let dialer = Arc::new(ProbeDialer(Mutex::new(Some(PublicStream::Test(client)))));
        let connector = PublicConnector::new(resolver, dialer);
        let target = public_target("https://allowed.example/");
        let request = Request::builder()
            .uri("https://allowed.example/")
            .body(Full::new(Bytes::new()))
            .unwrap();
        assert!(connector.http(target, request).await.is_err());
        let bytes = timeout(Duration::from_millis(500), receiver)
            .await
            .unwrap()
            .unwrap();
        assert!(!bytes.starts_with(b"GET "));
        assert_eq!(&bytes[..3], &[22, 3, 1]);
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
