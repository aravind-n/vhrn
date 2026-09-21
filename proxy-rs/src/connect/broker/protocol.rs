//! Authenticated host-broker capability protocol.

use std::fmt;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::{Buf, Bytes};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpStream, lookup_host};
use tokio::time::timeout;

use crate::Shutdown;
use crate::config::BrokerEndpoint;
use crate::domain::target::LoopbackAuthority;
use crate::shutdown::{ManagedIo, ProcessResources};

const READY_TIMEOUT: Duration = Duration::from_secs(3);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(13);
const MAX_FRAME_BYTES: usize = 256;
const MAX_RESPONSE_BYTES: usize = 4;
const REDACTED_TOKEN: &str = "BrokerToken([REDACTED])";

/// Capability token carried only in the authenticated broker frame.
pub(crate) struct BrokerToken(Box<str>);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BrokerTokenError;

impl fmt::Display for BrokerTokenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid broker token format")
    }
}

impl std::error::Error for BrokerTokenError {}

impl fmt::Debug for BrokerToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(REDACTED_TOKEN)
    }
}

impl fmt::Display for BrokerToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(REDACTED_TOKEN)
    }
}

impl std::str::FromStr for BrokerToken {
    type Err = BrokerTokenError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        (value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')))
        .then(|| Self(value.into()))
        .ok_or(BrokerTokenError)
    }
}

/// Safe response classification for every broker transport failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BrokerError {
    Rejected,
    Unavailable,
    DeadlineExceeded,
    Cancelled,
    OriginFailure,
    Exhausted,
}

impl fmt::Display for BrokerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Rejected => "broker rejected exchange",
            Self::Unavailable => "broker transport unavailable",
            Self::DeadlineExceeded => "broker exchange deadline exceeded",
            Self::Cancelled => "broker exchange cancelled",
            Self::OriginFailure => "local origin exchange failed",
            Self::Exhausted => "broker connection capacity exhausted",
        })
    }
}

impl std::error::Error for BrokerError {}

type BrokerDialFuture = Pin<Box<dyn Future<Output = io::Result<BrokerIo>> + Send>>;

trait BrokerDialer: Send + Sync + 'static {
    fn dial(&self, endpoint: BrokerEndpoint) -> BrokerDialFuture;
}

struct SystemBrokerDialer;

impl BrokerDialer for SystemBrokerDialer {
    fn dial(&self, endpoint: BrokerEndpoint) -> BrokerDialFuture {
        Box::pin(async move {
            match endpoint {
                BrokerEndpoint::Socket(address) => {
                    TcpStream::connect(address).await.map(BrokerIo::Tcp)
                }
                BrokerEndpoint::Host { host, port } => {
                    let mut last_error = None;
                    for address in lookup_host((host.as_str(), port)).await? {
                        match TcpStream::connect(address).await {
                            Ok(stream) => return Ok(BrokerIo::Tcp(stream)),
                            Err(error) => last_error = Some(error),
                        }
                    }
                    Err(last_error.unwrap_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::AddrNotAvailable,
                            "broker hostname resolved empty",
                        )
                    }))
                }
            }
        })
    }
}

struct BrokerProtocolInner {
    endpoint: BrokerEndpoint,
    token: BrokerToken,
    dialer: Arc<dyn BrokerDialer>,
    resources: ProcessResources,
}

/// Authenticated broker protocol exchange state.
#[derive(Clone)]
pub(crate) struct BrokerProtocol {
    inner: Arc<BrokerProtocolInner>,
    deadlines: Deadlines,
}

#[derive(Clone, Copy)]
pub(crate) struct Deadlines {
    pub(crate) ready: Duration,
    pub(crate) connect: Duration,
}

impl Default for Deadlines {
    fn default() -> Self {
        Self {
            ready: READY_TIMEOUT,
            connect: CONNECT_TIMEOUT,
        }
    }
}

impl BrokerProtocol {
    pub(crate) fn new(
        endpoint: impl Into<BrokerEndpoint>,
        token: BrokerToken,
        resources: ProcessResources,
    ) -> Self {
        Self::from_parts(
            endpoint.into(),
            token,
            Arc::new(SystemBrokerDialer),
            Deadlines::default(),
            resources,
        )
    }

    /// Completes the startup exchange before the proxy can accept traffic.
    pub(crate) async fn ready(&self, cancellation: &Shutdown) -> Result<(), BrokerError> {
        let (_, prefix) = self
            .exchange(self.ready_frame(), self.deadlines.ready, cancellation)
            .await?;
        if prefix.is_empty() {
            Ok(())
        } else {
            Err(BrokerError::Rejected)
        }
    }

    /// Opens an authenticated broker stream for one already-validated authority.
    pub(crate) async fn connect(
        &self,
        authority: &LoopbackAuthority,
        cancellation: &Shutdown,
    ) -> Result<BrokerStream, BrokerError> {
        let (stream, prefix) = self
            .exchange(
                self.connect_frame(authority),
                self.deadlines.connect,
                cancellation,
            )
            .await?;
        Ok(BrokerStream { stream, prefix })
    }

    async fn exchange(
        &self,
        frame: Vec<u8>,
        budget: Duration,
        cancellation: &Shutdown,
    ) -> Result<(ManagedIo<BrokerIo>, Bytes), BrokerError> {
        let permit = self
            .inner
            .resources
            .try_upstream()
            .ok_or(BrokerError::Exhausted)?;
        let exchange = async {
            let stream = self
                .inner
                .dialer
                .dial(self.inner.endpoint.clone())
                .await
                .map_err(|_| BrokerError::Unavailable)?;
            let mut stream = self.inner.resources.manage_upstream(stream, permit);
            stream
                .write_all(&frame)
                .await
                .map_err(|_| BrokerError::Unavailable)?;
            let prefix = read_response(&mut stream).await?;
            Ok((stream, prefix))
        };
        tokio::select! {
            biased;
            () = cancellation.cancelled() => Err(BrokerError::Cancelled),
            result = timeout(budget, exchange) => {
                result.map_err(|_| BrokerError::DeadlineExceeded)?
            },
        }
    }

    fn ready_frame(&self) -> Vec<u8> {
        self.frame(b"VHRN-BROKER/1 READY ", None)
    }

    fn connect_frame(&self, authority: &LoopbackAuthority) -> Vec<u8> {
        self.frame(
            b"VHRN-BROKER/1 CONNECT ",
            Some(authority.to_string().as_bytes()),
        )
    }

    fn frame(&self, prefix: &[u8], authority: Option<&[u8]>) -> Vec<u8> {
        let authority_len = authority.map_or(0, |value| value.len() + 1);
        let mut frame =
            Vec::with_capacity(prefix.len() + self.inner.token.0.len() + authority_len + 1);
        frame.extend_from_slice(prefix);
        frame.extend_from_slice(self.inner.token.0.as_bytes());
        if let Some(authority) = authority {
            frame.push(b' ');
            frame.extend_from_slice(authority);
        }
        frame.push(b'\n');
        assert!(
            frame.len() <= MAX_FRAME_BYTES,
            "broker frame exceeds protocol bound"
        );
        frame
    }

    fn from_parts(
        endpoint: BrokerEndpoint,
        token: BrokerToken,
        dialer: Arc<dyn BrokerDialer>,
        deadlines: Deadlines,
        resources: ProcessResources,
    ) -> Self {
        Self {
            inner: Arc::new(BrokerProtocolInner {
                endpoint,
                token,
                dialer,
                resources,
            }),
            deadlines,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_deadlines(
        endpoint: impl Into<BrokerEndpoint>,
        token: BrokerToken,
        deadlines: Deadlines,
    ) -> Self {
        Self::from_parts(
            endpoint.into(),
            token,
            Arc::new(SystemBrokerDialer),
            deadlines,
            ProcessResources::testing(256, 256),
        )
    }

    #[cfg(test)]
    fn with_dialer(
        token: BrokerToken,
        dialer: Arc<dyn BrokerDialer>,
        deadlines: Deadlines,
    ) -> Self {
        Self::with_dialer_and_resources(
            token,
            dialer,
            deadlines,
            ProcessResources::testing(256, 256),
        )
    }

    #[cfg(test)]
    fn with_dialer_and_resources(
        token: BrokerToken,
        dialer: Arc<dyn BrokerDialer>,
        deadlines: Deadlines,
        resources: ProcessResources,
    ) -> Self {
        Self::from_parts(
            "127.0.0.1:1"
                .parse::<std::net::SocketAddr>()
                .unwrap()
                .into(),
            token,
            dialer,
            deadlines,
            resources,
        )
    }
}

async fn read_response<S>(stream: &mut S) -> Result<Bytes, BrokerError>
where
    S: AsyncRead + Unpin,
{
    let mut bytes = [0; MAX_RESPONSE_BYTES];
    let mut len = 0;
    loop {
        let read = stream
            .read(&mut bytes[len..])
            .await
            .map_err(|_| BrokerError::Unavailable)?;
        if read == 0 {
            return Err(BrokerError::Rejected);
        }
        len += read;
        if len >= 3 && bytes[..3] == *b"OK\n" {
            return Ok(Bytes::copy_from_slice(&bytes[3..len]));
        }
        if !b"OK\n".starts_with(&bytes[..len]) || len == bytes.len() {
            return Err(BrokerError::Rejected);
        }
    }
}

#[cfg(test)]
pub(crate) fn short_test_deadlines() -> Deadlines {
    Deadlines {
        ready: Duration::from_millis(100),
        connect: Duration::from_millis(100),
    }
}

#[cfg(test)]
pub(crate) fn test_connect_authority(frame: &str) -> &str {
    frame
        .strip_prefix("VHRN-BROKER/1 CONNECT ")
        .expect("broker CONNECT frame")
        .split_once(' ')
        .expect("broker CONNECT token and authority")
        .1
        .trim_end()
}

#[cfg(test)]
pub(crate) fn test_connect_frame(authority: &str) -> String {
    format!("VHRN-BROKER/1 CONNECT {} {authority}\n", "a".repeat(64))
}

/// A broker-owned stream with bytes co-read with the success response.
pub(crate) struct BrokerStream {
    stream: ManagedIo<BrokerIo>,
    prefix: Bytes,
}

impl fmt::Debug for BrokerStream {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("BrokerStream([REDACTED])")
    }
}

impl BrokerStream {
    /// Removes bytes co-read with `OK\n` for the CONNECT tunnel handoff.
    pub(crate) fn take_prefix(&mut self) -> Bytes {
        std::mem::take(&mut self.prefix)
    }
}

enum BrokerIo {
    Tcp(TcpStream),
    #[cfg(test)]
    Duplex(tokio::io::DuplexStream),
}

#[cfg(test)]
impl BrokerStream {
    pub(crate) fn test_with_stream(stream: tokio::io::DuplexStream) -> Self {
        let resources = ProcessResources::testing(256, 256);
        let permit = resources.try_upstream().expect("test upstream permit");
        Self {
            stream: resources.manage_upstream(BrokerIo::Duplex(stream), permit),
            prefix: Bytes::new(),
        }
    }
}

impl AsyncRead for BrokerIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match &mut *self {
            Self::Tcp(stream) => Pin::new(stream).poll_read(cx, buffer),
            #[cfg(test)]
            Self::Duplex(stream) => Pin::new(stream).poll_read(cx, buffer),
        }
    }
}

impl AsyncWrite for BrokerIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        match &mut *self {
            Self::Tcp(stream) => Pin::new(stream).poll_write(cx, bytes),
            #[cfg(test)]
            Self::Duplex(stream) => Pin::new(stream).poll_write(cx, bytes),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut *self {
            Self::Tcp(stream) => Pin::new(stream).poll_flush(cx),
            #[cfg(test)]
            Self::Duplex(stream) => Pin::new(stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut *self {
            Self::Tcp(stream) => Pin::new(stream).poll_shutdown(cx),
            #[cfg(test)]
            Self::Duplex(stream) => Pin::new(stream).poll_shutdown(cx),
        }
    }
}

impl AsyncRead for BrokerStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if buffer.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        if !self.prefix.is_empty() {
            let count = buffer.remaining().min(self.prefix.len());
            buffer.put_slice(&self.prefix[..count]);
            self.prefix.advance(count);
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.stream).poll_read(cx, buffer)
    }
}

impl AsyncWrite for BrokerStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, bytes)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use std::future::pending;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::Notify;

    use super::*;

    fn token() -> BrokerToken {
        "a".repeat(64).parse().unwrap()
    }

    fn cancellation() -> Shutdown {
        Shutdown::new()
    }

    async fn listener() -> TcpListener {
        TcpListener::bind("127.0.0.1:0").await.unwrap()
    }

    #[test]
    fn token_format_is_exact_and_all_formatting_is_redacted() {
        for (name, value, valid) in [
            ("lower hex", "0123456789abcdef".repeat(4), true),
            ("empty", String::new(), false),
            ("short", "a".repeat(63), false),
            ("long", "a".repeat(65), false),
            ("uppercase", "A".repeat(64), false),
            ("non-hex", format!("{}g", "a".repeat(63)), false),
            ("newline", format!("{}\n", "a".repeat(64)), false),
            ("carriage return", format!("{}\r", "a".repeat(64)), false),
            ("non-ASCII", format!("{}é", "a".repeat(62)), false),
        ] {
            assert_eq!(value.parse::<BrokerToken>().is_ok(), valid, "{name}");
        }

        let secret = "a".repeat(64);
        let token = secret.parse::<BrokerToken>().unwrap();
        for rendered in [format!("{token}"), format!("{token:?}")] {
            assert_eq!(rendered, REDACTED_TOKEN);
            assert!(!rendered.contains(&secret));
        }
        let panic =
            std::panic::catch_unwind(|| panic!("{token:?}")).expect_err("redacted panic payload");
        let payload = panic
            .downcast_ref::<String>()
            .expect("string panic payload");
        assert!(!payload.contains(&secret));
    }

    #[tokio::test]
    async fn readiness_and_every_connect_use_fresh_exact_frames() {
        let listener = listener().await;
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut frames = Vec::new();
            for length in [85, 100, 106, 96] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut frame = vec![0; length];
                stream.read_exact(&mut frame).await.unwrap();
                stream.write_all(b"OK\n").await.unwrap();
                frames.push(frame);
            }
            frames
        });
        let protocol = BrokerProtocol::new(address, token(), ProcessResources::testing(256, 256));
        let cancellation = cancellation();
        protocol.ready(&cancellation).await.unwrap();
        for input in [
            "LOCALHOST:00080",
            "127.255.255.255:00081",
            "[0:0:0:0:0:0:0:1]:00082",
        ] {
            let authority = LoopbackAuthority::parse(input).unwrap();
            drop(protocol.connect(&authority, &cancellation).await.unwrap());
        }
        assert_eq!(
            server.await.unwrap(),
            [
                format!("VHRN-BROKER/1 READY {}\n", "a".repeat(64)).into_bytes(),
                format!("VHRN-BROKER/1 CONNECT {} localhost:80\n", "a".repeat(64)).into_bytes(),
                format!(
                    "VHRN-BROKER/1 CONNECT {} 127.255.255.255:81\n",
                    "a".repeat(64)
                )
                .into_bytes(),
                format!("VHRN-BROKER/1 CONNECT {} [::1]:82\n", "a".repeat(64)).into_bytes(),
            ]
        );
    }

    #[tokio::test]
    async fn hostname_endpoint_is_used_only_as_the_configured_broker_route() {
        let listener = listener().await;
        let endpoint = BrokerEndpoint::parse(&format!(
            "localhost:{}",
            listener.local_addr().unwrap().port()
        ))
        .unwrap();
        let server = tokio::spawn(async move {
            for length in [85, 100] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut frame = vec![0; length];
                stream.read_exact(&mut frame).await.unwrap();
                stream.write_all(b"OK\n").await.unwrap();
            }
        });
        let protocol = BrokerProtocol::with_deadlines(endpoint, token(), short_test_deadlines());
        let cancellation = cancellation();
        protocol.ready(&cancellation).await.unwrap();
        drop(
            protocol
                .connect(
                    &LoopbackAuthority::parse("localhost:80").unwrap(),
                    &cancellation,
                )
                .await
                .unwrap(),
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn fragmented_success_and_all_coalesced_payload_bytes_are_preserved() {
        let listener = listener().await;
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut frame = vec![0; 100];
            stream.read_exact(&mut frame).await.unwrap();
            stream.write_all(b"O").await.unwrap();
            tokio::task::yield_now().await;
            stream.write_all(b"K").await.unwrap();
            tokio::task::yield_now().await;
            stream.write_all(b"\npayload").await.unwrap();
        });
        let mut stream = BrokerProtocol::new(address, token(), ProcessResources::testing(256, 256))
            .connect(
                &LoopbackAuthority::parse("localhost:80").unwrap(),
                &cancellation(),
            )
            .await
            .unwrap();
        let mut payload = [0; 7];
        stream.read_exact(&mut payload).await.unwrap();
        assert_eq!(&payload, b"payload");
        assert_eq!(format!("{stream:?}"), "BrokerStream([REDACTED])");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn fixture_covers_bounded_response_outcomes() {
        for row in include_str!("../../../testdata/broker-frames.tsv")
            .lines()
            .filter(|row| !row.starts_with('#'))
        {
            let fields: Vec<_> = row.split('\t').collect();
            let [kind, wire, response, payload, outcome] = fields.as_slice() else {
                panic!("invalid broker fixture row")
            };
            let listener = listener().await;
            let address = listener.local_addr().unwrap();
            let expected = wire
                .replace("<token>", &"a".repeat(64))
                .replace("\\r", "\r")
                .replace("\\n", "\n");
            let response = response.replace("\\r", "\r").replace("\\n", "\n");
            let payload = payload.replace("\\r", "\r").replace("\\n", "\n");
            let server_payload = payload.clone();
            let stalled = response == "timeout";
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut received = vec![0; expected.len()];
                stream.read_exact(&mut received).await.unwrap();
                assert_eq!(received, expected.as_bytes());
                match response.as_str() {
                    "timeout" => std::future::pending::<()>().await,
                    "eof" => {}
                    _ => {
                        let mut bytes = response.into_bytes();
                        bytes.extend_from_slice(server_payload.as_bytes());
                        let _ = stream.write_all(&bytes).await;
                    }
                }
            });
            let protocol = BrokerProtocol::with_deadlines(address, token(), short_test_deadlines());
            let cancellation = cancellation();
            let result = match *kind {
                "ready" => protocol.ready(&cancellation).await.map(|()| Vec::new()),
                "connect" => {
                    match protocol
                        .connect(
                            &LoopbackAuthority::parse("localhost:80").unwrap(),
                            &cancellation,
                        )
                        .await
                    {
                        Ok(mut stream) => {
                            let mut received = vec![0; payload.len()];
                            stream
                                .read_exact(&mut received)
                                .await
                                .map_err(|_| BrokerError::Unavailable)
                                .map(|_| received)
                        }
                        Err(error) => Err(error),
                    }
                }
                _ => panic!("unknown broker fixture exchange"),
            };
            match *outcome {
                "ready" => assert_eq!(result.unwrap(), Vec::<u8>::new()),
                "connected" => assert_eq!(result.unwrap(), payload.as_bytes()),
                "rejected" => assert_eq!(result.unwrap_err(), BrokerError::Rejected),
                "deadline" => assert_eq!(result.unwrap_err(), BrokerError::DeadlineExceeded),
                _ => panic!("unknown broker fixture outcome"),
            }
            if stalled {
                server.abort();
                assert!(server.await.unwrap_err().is_cancelled());
            } else {
                server.await.unwrap();
            }
        }
    }

    struct OneStreamDialer(Mutex<Option<tokio::io::DuplexStream>>);

    impl BrokerDialer for OneStreamDialer {
        fn dial(&self, _: BrokerEndpoint) -> BrokerDialFuture {
            let stream = self.0.lock().unwrap().take();
            Box::pin(async move {
                stream
                    .map(BrokerIo::Duplex)
                    .ok_or_else(|| io::Error::other("test stream already taken"))
            })
        }
    }

    struct DropFlag(Arc<AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    struct PendingDialer {
        entered: Arc<Notify>,
        dropped: Arc<AtomicBool>,
    }

    #[tokio::test]
    async fn rejected_handshake_releases_the_shared_upstream_permit() {
        let resources = ProcessResources::testing(8, 1);
        let (stream, mut peer) = tokio::io::duplex(256);
        peer.write_all(b"ERR\n").await.unwrap();
        let protocol = BrokerProtocol::with_dialer_and_resources(
            token(),
            Arc::new(OneStreamDialer(Mutex::new(Some(stream)))),
            short_test_deadlines(),
            resources.clone(),
        );

        assert_eq!(
            protocol.ready(&cancellation()).await.unwrap_err(),
            BrokerError::Rejected
        );
        assert_eq!(resources.upstream_counts(), (0, 1));
        assert!(resources.try_upstream().is_some());
    }

    impl BrokerDialer for PendingDialer {
        fn dial(&self, _: BrokerEndpoint) -> BrokerDialFuture {
            let entered = self.entered.clone();
            let dropped = self.dropped.clone();
            Box::pin(async move {
                let _guard = DropFlag(dropped);
                entered.notify_one();
                pending().await
            })
        }
    }

    #[tokio::test]
    async fn cancellation_interrupts_dial_and_drops_pending_work() {
        let entered = Arc::new(Notify::new());
        let dropped = Arc::new(AtomicBool::new(false));
        let protocol = BrokerProtocol::with_dialer(
            token(),
            Arc::new(PendingDialer {
                entered: entered.clone(),
                dropped: dropped.clone(),
            }),
            short_test_deadlines(),
        );
        let cancellation = cancellation();
        let task_cancellation = cancellation.clone();
        let task = tokio::spawn(async move { protocol.ready(&task_cancellation).await });
        entered.notified().await;
        cancellation.request();
        assert_eq!(task.await.unwrap().unwrap_err(), BrokerError::Cancelled);
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn cancellation_interrupts_write_and_closes_the_socket() {
        let (stream, mut peer) = tokio::io::duplex(1);
        let protocol = BrokerProtocol::with_dialer(
            token(),
            Arc::new(OneStreamDialer(Mutex::new(Some(stream)))),
            short_test_deadlines(),
        );
        let cancellation = cancellation();
        let task_cancellation = cancellation.clone();
        let task = tokio::spawn(async move { protocol.ready(&task_cancellation).await });
        let mut first = [0];
        peer.read_exact(&mut first).await.unwrap();
        cancellation.request();
        assert_eq!(task.await.unwrap().unwrap_err(), BrokerError::Cancelled);
        let mut remainder = Vec::new();
        peer.read_to_end(&mut remainder).await.unwrap();
        assert!(remainder.len() < 84, "cancelled write sent a full frame");
    }

    #[tokio::test]
    async fn cancellation_interrupts_read_and_closes_the_socket() {
        let (stream, mut peer) = tokio::io::duplex(256);
        let protocol = BrokerProtocol::with_dialer(
            token(),
            Arc::new(OneStreamDialer(Mutex::new(Some(stream)))),
            short_test_deadlines(),
        );
        let cancellation = cancellation();
        let task_cancellation = cancellation.clone();
        let task = tokio::spawn(async move {
            protocol
                .connect(
                    &LoopbackAuthority::parse("localhost:80").unwrap(),
                    &task_cancellation,
                )
                .await
        });
        let mut frame = [0; 100];
        peer.read_exact(&mut frame).await.unwrap();
        cancellation.request();
        assert_eq!(task.await.unwrap().unwrap_err(), BrokerError::Cancelled);
        let mut byte = [0];
        assert_eq!(peer.read(&mut byte).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn one_cumulative_deadline_closes_a_stalled_exchange() {
        let (stream, mut peer) = tokio::io::duplex(256);
        let protocol = BrokerProtocol::with_dialer(
            token(),
            Arc::new(OneStreamDialer(Mutex::new(Some(stream)))),
            short_test_deadlines(),
        );
        let task = tokio::spawn(async move { protocol.ready(&cancellation()).await });
        let mut frame = [0; 85];
        peer.read_exact(&mut frame).await.unwrap();
        assert_eq!(
            task.await.unwrap().unwrap_err(),
            BrokerError::DeadlineExceeded
        );
        let mut byte = [0];
        assert_eq!(peer.read(&mut byte).await.unwrap(), 0);
    }

    #[test]
    fn typed_errors_never_render_secret_or_endpoint() {
        let secret = "a".repeat(64);
        let endpoint = "192.168.64.1:54321";
        for error in [
            BrokerError::Rejected,
            BrokerError::Unavailable,
            BrokerError::DeadlineExceeded,
            BrokerError::Cancelled,
            BrokerError::OriginFailure,
            BrokerError::Exhausted,
        ] {
            for rendered in [format!("{error}"), format!("{error:?}")] {
                assert!(!rendered.contains(&secret));
                assert!(!rendered.contains(endpoint));
            }
        }
    }
}
