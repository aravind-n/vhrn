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
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::time::{Instant, timeout_at};

use crate::Shutdown;
use crate::connect::forward::{ForwardError, ForwardErrorKind, Forwarded, exchange, forward_error};
use crate::connect::origin_body::{HttpOrigin, OriginLease, take_origin};
use crate::connect::pool::IdlePool;
use crate::domain::target::{PublicHost, PublicTarget};
use crate::server::http1::{Http1Connection, RequestHead};
use crate::shutdown::{ManagedIo, ProcessResources};

use self::registry::{ipv4_is_global, ipv6_is_global};

const CONNECT_DEADLINE: Duration = Duration::from_secs(10);
const MAX_DNS_ANSWERS: usize = 64;

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
    Exhausted,
}

#[derive(Debug)]
pub(crate) enum PublicForwardError {
    Connect(PublicConnectError),
    Exchange(ForwardError),
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
            Self::Exhausted => "public connection capacity exhausted",
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
    pool: IdlePool<OriginKey, HttpOrigin<ManagedIo<PublicStream>>>,
    resources: ProcessResources,
    #[cfg(test)]
    public_calls: std::sync::atomic::AtomicUsize,
}

impl PublicConnector {
    pub(crate) fn system(resources: ProcessResources) -> Self {
        Self::from_parts(
            Arc::new(SystemResolver),
            Arc::new(SystemDialer),
            IdlePool::new(),
            resources,
        )
    }

    #[cfg(test)]
    pub(crate) fn new(resolver: Arc<dyn Resolver>, dialer: Arc<dyn NumericDialer>) -> Self {
        Self::from_parts(
            resolver,
            dialer,
            IdlePool::new(),
            ProcessResources::testing(256, 256),
        )
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
        pool: IdlePool<OriginKey, HttpOrigin<ManagedIo<PublicStream>>>,
        resources: ProcessResources,
    ) -> Self {
        Self {
            resolver,
            dialer,
            pool,
            resources,
            #[cfg(test)]
            public_calls: std::sync::atomic::AtomicUsize::new(0),
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
    ) -> std::result::Result<ManagedIo<PublicStream>, PublicConnectError> {
        let permit = self
            .resources
            .try_upstream()
            .ok_or(PublicConnectError::Exhausted)?;
        let stream = Self::open_with_budget(
            self.resolver.clone(),
            self.dialer.clone(),
            target,
            cancellation,
            CONNECT_DEADLINE,
        )
        .await?;
        Ok(self.resources.manage_upstream(stream, permit))
    }

    pub(crate) async fn connect_target(
        &self,
        target: PublicTarget,
        cancellation: &Shutdown,
    ) -> std::result::Result<ManagedIo<PublicStream>, PublicConnectError> {
        self.open_target(target, cancellation).await
    }

    async fn checkout(
        &self,
        target: &PublicTarget,
        cancellation: &Shutdown,
    ) -> std::result::Result<OriginLease<OriginKey, ManagedIo<PublicStream>>, PublicConnectError>
    {
        let key = OriginKey::from_target(target);
        let origin = match take_origin(&self.pool, &key) {
            Some(origin) => origin,
            None => HttpOrigin::new(self.open_target(target.clone(), cancellation).await?),
        };
        Ok(OriginLease::new(origin, key, self.pool.clone()))
    }

    pub(crate) async fn forward<D>(
        &self,
        target: PublicTarget,
        head: &RequestHead,
        downstream: &mut Http1Connection<D>,
        cancellation: &Shutdown,
    ) -> std::result::Result<Forwarded, PublicForwardError>
    where
        D: AsyncRead + AsyncWrite + Unpin,
    {
        #[cfg(test)]
        self.public_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let lease = tokio::select! {
            biased;
            _ = downstream.wait_for_peer_close() => {
                return Err(PublicForwardError::Exchange(forward_error(
                    ForwardErrorKind::ClientDisconnected,
                    false,
                )));
            }
            result = self.checkout(&target, cancellation) => {
                result.map_err(PublicForwardError::Connect)?
            }
        };
        exchange(
            downstream,
            head,
            target.path_and_query(),
            target.authority(),
            lease,
            cancellation,
        )
        .await
        .map_err(PublicForwardError::Exchange)
    }

    #[cfg(test)]
    async fn open(
        &self,
        target: PublicTarget,
    ) -> std::result::Result<ManagedIo<PublicStream>, PublicConnectError> {
        self.open_target(target, &Shutdown::new()).await
    }

    pub(crate) fn prune_pool(&self) {
        self.pool.prune();
    }

    pub(crate) fn close_pool(&self) {
        self.pool.close();
    }

    #[cfg(test)]
    pub(crate) fn public_calls(&self) -> usize {
        self.public_calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    };

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

    #[tokio::test]
    async fn upstream_capacity_rejects_before_dns_then_releases_for_reacquisition() {
        let resources = ProcessResources::testing(8, 1);
        let held = resources.manage_upstream((), resources.try_upstream().unwrap());
        let resolver = Arc::new(FakeResolver::new(vec![Ok(parse_addresses("8.8.8.8"))]));
        let connector = PublicConnector::from_parts(
            resolver.clone(),
            Arc::new(FakeDialer::success()),
            IdlePool::new(),
            resources.clone(),
        );

        assert_eq!(
            connector
                .open(public_target("http://allowed.example/"))
                .await
                .unwrap_err(),
            PublicConnectError::Exhausted
        );
        assert!(resolver.calls.lock().unwrap().is_empty());
        drop(held);

        let stream = connector
            .open(public_target("http://allowed.example/"))
            .await
            .unwrap();
        assert_eq!(resources.upstream_counts(), (1, 1));
        drop(stream);
        assert_eq!(resources.upstream_counts(), (0, 1));
    }

    #[tokio::test]
    async fn cancelled_resolution_releases_its_upstream_permit() {
        let resources = ProcessResources::testing(8, 1);
        let connector = Arc::new(PublicConnector::from_parts(
            Arc::new(PendingResolver),
            Arc::new(PendingDialer),
            IdlePool::new(),
            resources.clone(),
        ));
        let shutdown = Shutdown::new();
        let task_connector = connector.clone();
        let task_shutdown = shutdown.clone();
        let task = tokio::spawn(async move {
            task_connector
                .open_target(public_target("http://allowed.example/"), &task_shutdown)
                .await
        });
        tokio::task::yield_now().await;
        assert_eq!(resources.upstream_counts(), (1, 1));
        shutdown.request();
        assert_eq!(
            task.await.unwrap().unwrap_err(),
            PublicConnectError::Cancelled
        );
        assert_eq!(resources.upstream_counts(), (0, 1));
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
}
