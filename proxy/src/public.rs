//! Validated numeric public connection setup.

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::service::{BoxFuture, PublicConnector, sealed};
use crate::target::PublicTarget;

const RESOLVE_TIMEOUT: Duration = Duration::from_secs(10);
const DIAL_TIMEOUT: Duration = Duration::from_secs(10);

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
}

impl PublicConnectorAdapter {
    pub(crate) fn system() -> Self {
        Self::new(Arc::new(SystemResolver), Arc::new(SystemDialer))
    }

    pub(crate) fn new(resolver: Arc<dyn Resolver>, dialer: Arc<dyn NumericDialer>) -> Self {
        Self { resolver, dialer }
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

    fn connect_target(&self, target: PublicTarget) -> BoxFuture {
        let resolver = self.resolver.clone();
        let dialer = self.dialer.clone();
        Box::pin(async move {
            let _ = Self::open_with(resolver, dialer, target).await;
        })
    }

    #[cfg(test)]
    async fn open(&self, target: PublicTarget) -> Result<BoxStream> {
        Self::open_with(self.resolver.clone(), self.dialer.clone(), target).await
    }
}

impl sealed::Public for PublicConnectorAdapter {}
impl PublicConnector for PublicConnectorAdapter {
    fn http(&self, target: PublicTarget) -> BoxFuture {
        self.connect_target(target)
    }

    fn connect(&self, target: PublicTarget) -> BoxFuture {
        self.connect_target(target)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use tokio::io::DuplexStream;

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

    #[allow(dead_code)]
    fn stream_is_owned(_: DuplexStream) {}
}
