//! Validated numeric public connection setup.

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::client::conn::http1;
use hyper::{Request, Uri};
use hyper_util::rt::TokioIo;
use rustls::pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_rustls::TlsConnector;

use crate::idle_pool::{IDLE_POOL_CAPACITY, IDLE_POOL_LIFETIME, IdlePool, IdleValue};
use crate::service::{
    PublicConnectFuture, PublicConnector, PublicHttpFuture, PublicResponse, sealed,
};
use crate::target::PublicTarget;

const RESOLVE_TIMEOUT: Duration = Duration::from_secs(10);
const DIAL_TIMEOUT: Duration = Duration::from_secs(10);
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

pub(crate) trait PublicStream: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T> PublicStream for T where T: AsyncRead + AsyncWrite + Send + Unpin {}

pub(crate) type BoxStream = Box<dyn PublicStream>;
pub(crate) type ResolveFuture = Pin<Box<dyn Future<Output = Result<Vec<IpAddr>>> + Send>>;
pub(crate) type DialFuture = Pin<Box<dyn Future<Output = Result<BoxStream>> + Send>>;

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
            Ok(addresses.map(|address| address.ip()).collect())
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
            Ok(Box::new(stream) as BoxStream)
        })
    }
}

/// Stable identity for a public origin connection pool.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OriginKey {
    scheme: &'static str,
    host: String,
    port: u16,
}

impl OriginKey {
    #[must_use]
    pub fn from_target(target: &PublicTarget) -> Self {
        Self {
            scheme: if target.secure() { "https" } else { "http" },
            host: target.host().to_owned(),
            port: target.port(),
        }
    }

    #[must_use]
    pub const fn scheme(&self) -> &'static str {
        self.scheme
    }

    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ValidatedAddress(IpAddr);

impl ValidatedAddress {
    fn socket_addr(self, port: u16) -> SocketAddr {
        SocketAddr::new(self.0, port)
    }
}

fn validated_answers(answers: Vec<IpAddr>) -> Result<ValidatedAddress> {
    let mut validated = Vec::with_capacity(answers.len());
    for answer in answers {
        let address = canonical_ip(answer);
        if !is_public(address) {
            return Err(anyhow!("public address rejected"));
        }
        validated.push(ValidatedAddress(address));
    }
    validated
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("public name returned no addresses"))
}

fn canonical_ip(address: IpAddr) -> IpAddr {
    match address {
        IpAddr::V6(address) => address
            .to_ipv4_mapped()
            .map_or(IpAddr::V6(address), IpAddr::V4),
        IpAddr::V4(address) => IpAddr::V4(address),
    }
}

fn is_public(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => {
            !address.is_loopback()
                && !address.is_private()
                && !address.is_unspecified()
                && !address.is_link_local()
                && !address.is_multicast()
                && !is_shared(address)
        }
        IpAddr::V6(address) => {
            !address.is_loopback()
                && !address.is_unspecified()
                && !address.is_unicast_link_local()
                && !address.is_multicast()
                && !address.is_unique_local()
        }
    }
}

fn is_shared(address: Ipv4Addr) -> bool {
    let octets = address.octets();
    octets[0] == 100 && (64..128).contains(&octets[1])
}

/// Public connector that resolves and opens one validated numeric stream.
pub(crate) struct PublicConnectorAdapter {
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

impl IdleValue for OriginConnection {
    fn reusable(&self) -> bool {
        !self.driver.is_finished() && self.sender.is_ready()
    }
}

impl PublicConnectorAdapter {
    pub(crate) fn system() -> Self {
        Self::new(Arc::new(SystemResolver), Arc::new(SystemDialer))
    }

    pub(crate) fn new(resolver: Arc<dyn Resolver>, dialer: Arc<dyn NumericDialer>) -> Self {
        Self::new_with_pool(
            resolver,
            dialer,
            IdlePool::new(IDLE_POOL_CAPACITY, IDLE_POOL_LIFETIME),
        )
    }

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
        capacity: usize,
        lifetime: Duration,
    ) -> Self {
        Self::new_with_pool(resolver, dialer, IdlePool::new(capacity, lifetime))
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
            pool: IdlePool::new(IDLE_POOL_CAPACITY, IDLE_POOL_LIFETIME),
            response_timeout,
            response_limit,
            public_calls: std::sync::atomic::AtomicUsize::new(0),
            active_drivers: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    async fn open_with(
        resolver: Arc<dyn Resolver>,
        dialer: Arc<dyn NumericDialer>,
        target: PublicTarget,
    ) -> Result<BoxStream> {
        Self::open_with_deadlines(resolver, dialer, target, RESOLVE_TIMEOUT, DIAL_TIMEOUT).await
    }

    async fn open_with_deadlines(
        resolver: Arc<dyn Resolver>,
        dialer: Arc<dyn NumericDialer>,
        target: PublicTarget,
        resolve_timeout: Duration,
        dial_timeout: Duration,
    ) -> Result<BoxStream> {
        let answers = timeout(
            resolve_timeout,
            resolver.resolve(target.host().to_owned(), target.port()),
        )
        .await
        .map_err(|_| anyhow!("public name resolution timed out"))??;
        let address = validated_answers(answers)?.socket_addr(target.port());
        timeout(dial_timeout, dialer.dial(address))
            .await
            .map_err(|_| anyhow!("public connection timed out"))?
    }

    fn connect_target(&self, target: PublicTarget) -> PublicConnectFuture<'_> {
        let resolver = self.resolver.clone();
        let dialer = self.dialer.clone();
        Box::pin(async move { Self::open_with(resolver, dialer, target).await })
    }

    async fn open_sender(&self, target: &PublicTarget) -> Result<OriginConnection> {
        let stream =
            Self::open_with(self.resolver.clone(), self.dialer.clone(), target.clone()).await?;
        let io: BoxStream = if target.secure() {
            let _ = rustls::crypto::ring::default_provider().install_default();
            let roots =
                rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let config = rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();
            let server_name = ServerName::try_from(target.host().to_owned())
                .map_err(|_| anyhow!("invalid TLS server name"))?;
            let tls = timeout(
                HTTP_TIMEOUT,
                TlsConnector::from(Arc::new(config)).connect(server_name, stream),
            )
            .await
            .map_err(|_| anyhow!("TLS handshake timed out"))??;
            Box::new(tls)
        } else {
            stream
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

    async fn send(
        &self,
        target: PublicTarget,
        request: Request<Full<Bytes>>,
    ) -> Result<PublicResponse> {
        #[cfg(test)]
        self.public_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let key = OriginKey::from_target(&target);
        let request = outbound_request(request)?;
        let connection = self.pool.take(&key);
        let mut connection = match connection {
            Some(connection) => connection,
            None => self.open_sender(&target).await?,
        };
        let response = timeout(
            self.response_timeout,
            connection.sender.send_request(request),
        )
        .await
        .map_err(|_| anyhow!("origin response timed out"));
        let Ok(response) = response else {
            return Err(response.unwrap_err());
        };
        let response = match response {
            Ok(response) => response,
            Err(error) => return Err(anyhow!(error)),
        };
        let (parts, body) = response.into_parts();
        let body = timeout(
            self.response_timeout,
            collect_limited(body, self.response_limit),
        )
        .await
        .map_err(|_| anyhow!("origin body timed out"))??;
        self.pool.put(key, connection);
        Ok(PublicResponse {
            status: parts.status,
            headers: parts.headers,
            body,
        })
    }

    #[cfg(test)]
    async fn open(&self, target: PublicTarget) -> Result<BoxStream> {
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

async fn collect_limited(mut body: Incoming, limit: usize) -> Result<Bytes> {
    let mut bytes = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|_| anyhow!("origin body failed"))?;
        if let Ok(data) = frame.into_data() {
            let remaining = limit.saturating_sub(bytes.len());
            if data.len() > remaining {
                return Err(anyhow!("origin body too large"));
            }
            bytes.extend_from_slice(&data);
        }
    }
    Ok(Bytes::from(bytes))
}

impl sealed::Public for PublicConnectorAdapter {}
impl PublicConnector for PublicConnectorAdapter {
    fn http(&self, target: PublicTarget, request: Request<Full<Bytes>>) -> PublicHttpFuture<'_> {
        Box::pin(async move { self.send(target, request).await })
    }

    fn connect(&self, target: PublicTarget) -> PublicConnectFuture<'_> {
        self.connect_target(target)
    }
}

fn outbound_request(request: Request<Full<Bytes>>) -> Result<Request<Full<Bytes>>> {
    let (mut parts, body) = request.into_parts();
    parts.headers.remove("proxy-connection");
    parts.headers.remove("proxy-authorization");
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

    use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
    use tokio::sync::oneshot;

    use super::*;
    use crate::target::{Target, classify};

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
        result: Mutex<Option<Result<BoxStream>>>,
    }
    impl FakeDialer {
        fn success() -> Self {
            let (stream, _) = tokio::io::duplex(1);
            Self {
                calls: Mutex::new(Vec::new()),
                result: Mutex::new(Some(Ok(Box::new(stream)))),
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
        streams: Mutex<VecDeque<BoxStream>>,
    }
    impl QueueDialer {
        fn new(streams: Vec<BoxStream>) -> Self {
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
        let Target::PublicHttp(target) = classify("GET", value) else {
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
            clients.push(Box::new(client) as BoxStream);
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
        let connector = PublicConnectorAdapter::with_pool_limits(
            resolver,
            dialer.clone(),
            2,
            Duration::from_secs(5),
        );
        for index in 0..3 {
            connector
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
        }
        assert_eq!(connector.pool_len().await, 2);
        assert_eq!(
            timeout(Duration::from_millis(50), &mut closed[0])
                .await
                .unwrap()
                .unwrap(),
            0
        );
        connector
            .http(
                targets[1].clone(),
                Request::builder()
                    .uri("http://two.example/reused")
                    .body(Full::new(Bytes::new()))
                    .unwrap(),
            )
            .await
            .unwrap();
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
        let connector = PublicConnectorAdapter::with_pool_limits(
            resolver,
            Arc::new(QueueDialer::new(vec![Box::new(client)])),
            2,
            Duration::from_millis(20),
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
        let dialer = Arc::new(QueueDialer::new(vec![Box::new(first), Box::new(second)]));
        let connector = PublicConnectorAdapter::new(resolver, dialer.clone());
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
        for row in include_str!("../testdata/ip-addresses.tsv")
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
        let connector = PublicConnectorAdapter::new(resolver.clone(), dialer.clone());
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
        let dialer = Arc::new(QueueDialer::new(vec![Box::new(first), Box::new(second)]));
        let connector = PublicConnectorAdapter::new(resolver, dialer.clone());
        let Target::PublicConnect(default_target) = classify("CONNECT", "allowed.example") else {
            panic!("expected public CONNECT target");
        };
        let Target::PublicConnect(explicit_target) = classify("CONNECT", "allowed.example:8443")
        else {
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
            parse_addresses("::1"),
        ] {
            let resolver = Arc::new(FakeResolver::new(vec![Ok(answers)]));
            let dialer = Arc::new(FakeDialer::success());
            let connector = PublicConnectorAdapter::new(resolver.clone(), dialer.clone());
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
        let connector = PublicConnectorAdapter::new(resolver, dialer.clone());
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
        let connector = PublicConnectorAdapter::new(resolver, dialer.clone());
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
        let connector = PublicConnectorAdapter::new(resolver, dialer.clone());
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
            PublicConnectorAdapter::open_with_deadlines(
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
            PublicConnectorAdapter::open_with_deadlines(
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
    fn outbound_request_keeps_ordinary_connection_fields() {
        let request = Request::builder()
            .method("POST")
            .uri("http://api.example.com:8080/path?q=one")
            .header("host", "api.example.com:8080")
            .header("connection", "X-Remove")
            .header("x-remove", "kept")
            .header("proxy-connection", "close")
            .header("proxy-authorization", "Basic ignored")
            .body(Full::new(Bytes::from_static(b"body")))
            .unwrap();
        let request = outbound_request(request).unwrap();
        assert_eq!(request.uri(), "/path?q=one");
        assert_eq!(request.headers()["connection"], "X-Remove");
        assert_eq!(request.headers()["x-remove"], "kept");
        assert_eq!(request.headers()["host"], "api.example.com:8080");
        assert!(!request.headers().contains_key("proxy-connection"));
        assert!(!request.headers().contains_key("proxy-authorization"));
    }

    #[tokio::test]
    async fn secure_origin_never_receives_an_http_request_before_tls() {
        struct ProbeDialer(Mutex<Option<BoxStream>>);
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
        let dialer = Arc::new(ProbeDialer(Mutex::new(Some(Box::new(client)))));
        let connector = PublicConnectorAdapter::new(resolver, dialer);
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
        let dialer = Arc::new(QueueDialer::new(vec![Box::new(client)]));
        let connector = PublicConnectorAdapter::with_response_limits(
            resolver,
            dialer,
            Duration::from_millis(80),
            3,
        );
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

        let (client, mut peer) = tokio::io::duplex(4096);
        let resolver = Arc::new(FakeResolver::new(vec![Ok(parse_addresses("8.8.8.8"))]));
        let dialer = Arc::new(QueueDialer::new(vec![Box::new(client)]));
        let connector = PublicConnectorAdapter::with_response_limits(
            resolver,
            dialer,
            Duration::from_millis(50),
            16,
        );
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

    #[allow(dead_code)]
    fn stream_is_owned(_: DuplexStream) {}
}
