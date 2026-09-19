//! Authenticated host-broker capability protocol.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpStream, lookup_host};
use tokio::time::timeout;

use crate::config::BrokerEndpoint;
use crate::domain::target::LoopbackAuthority;

const READY_TIMEOUT: Duration = Duration::from_secs(3);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(13);
const MAX_RESPONSE_BYTES: usize = 4;

/// Capability token carried only in the authenticated broker frame.
#[derive(Clone)]
pub(crate) struct BrokerToken(pub(super) String);

#[derive(Debug, Clone, Copy)]
pub(crate) struct BrokerTokenError;

impl std::fmt::Display for BrokerTokenError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("invalid broker token format")
    }
}

impl std::error::Error for BrokerTokenError {}

impl std::fmt::Debug for BrokerToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("BrokerToken([REDACTED])")
    }
}

impl BrokerToken {
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl std::str::FromStr for BrokerToken {
    type Err = BrokerTokenError;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        (value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')))
        .then_some(Self(value.to_owned()))
        .ok_or(BrokerTokenError)
    }
}

/// Authenticated broker protocol exchange state.
#[derive(Clone)]
pub(crate) struct BrokerProtocol {
    endpoint: BrokerEndpoint,
    token: BrokerToken,
    deadlines: Deadlines,
}

#[derive(Clone, Copy)]
pub(crate) struct Deadlines {
    pub(crate) ready: Duration,
    pub(crate) connect: Duration,
    pub(crate) handshake: Duration,
}

impl Default for Deadlines {
    fn default() -> Self {
        Self {
            ready: READY_TIMEOUT,
            connect: CONNECT_TIMEOUT,
            handshake: CONNECT_HANDSHAKE_TIMEOUT,
        }
    }
}

impl BrokerProtocol {
    pub(crate) fn new(endpoint: impl Into<BrokerEndpoint>, token: BrokerToken) -> Self {
        Self {
            endpoint: endpoint.into(),
            token,
            deadlines: Deadlines::default(),
        }
    }

    /// Completes the startup exchange before the proxy can accept traffic.
    pub(crate) async fn ready(&self) -> Result<()> {
        let result = timeout(self.deadlines.ready, async {
            let mut stream = self.dial().await.context("dial broker endpoint")?;
            let frame = format!("VHRN-BROKER/1 READY {}\n", self.token.expose());
            stream
                .write_all(frame.as_bytes())
                .await
                .context("write READY frame")?;
            read_response(&mut stream)
                .await
                .context("read broker response")
                .and_then(|prefix| {
                    prefix
                        .is_none()
                        .then_some(())
                        .ok_or_else(|| anyhow::anyhow!("broker rejected exchange"))
                })
        })
        .await;
        match result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(error.context("broker readiness failed")),
            Err(_) => bail!("broker readiness timeout"),
        }
    }

    /// Opens an authenticated broker stream for one already-validated authority.
    pub(crate) async fn connect(&self, authority: &LoopbackAuthority) -> Result<BrokerStream> {
        let stream = timeout(self.deadlines.connect, self.dial())
            .await
            .map_err(|_| anyhow::anyhow!("broker connection timeout"))?
            .context("dial broker endpoint")?;
        let frame = format!(
            "VHRN-BROKER/1 CONNECT {} {}\n",
            self.token.expose(),
            authority
        );
        let result = timeout(self.deadlines.handshake, async move {
            let mut stream = stream;
            stream
                .write_all(frame.as_bytes())
                .await
                .context("write CONNECT frame")?;
            let prefix = read_response(&mut stream)
                .await
                .context("read broker response")?;
            Ok::<_, anyhow::Error>(BrokerStream {
                stream: BrokerIo::Tcp(stream),
                prefix,
            })
        })
        .await;
        match result {
            Ok(Ok(stream)) => Ok(stream),
            Ok(Err(error)) => Err(error.context("broker rejected exchange")),
            Err(_) => bail!("broker connection timeout"),
        }
    }

    async fn dial(&self) -> io::Result<TcpStream> {
        match &self.endpoint {
            BrokerEndpoint::Socket(address) => TcpStream::connect(address).await,
            BrokerEndpoint::Host { host, port } => {
                let mut last_error = None;
                for address in lookup_host((host.as_str(), *port)).await? {
                    match TcpStream::connect(address).await {
                        Ok(stream) => return Ok(stream),
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
    }

    #[cfg(test)]
    pub(crate) fn with_deadlines(
        endpoint: impl Into<BrokerEndpoint>,
        token: BrokerToken,
        deadlines: Deadlines,
    ) -> Self {
        Self {
            endpoint: endpoint.into(),
            token,
            deadlines,
        }
    }
}

async fn read_response(stream: &mut TcpStream) -> Result<Option<u8>> {
    let mut bytes = [0; MAX_RESPONSE_BYTES];
    let mut len = 0;
    while len < bytes.len() {
        let read = stream
            .read(&mut bytes[len..])
            .await
            .context("read broker response")?;
        if read == 0 {
            bail!("broker rejected exchange");
        }
        len += read;
        if bytes[..len].starts_with(b"OK\n") {
            return Ok(success_prefix(&bytes[..len]));
        }
        if bytes[..len] == *b"ERR\n" || bytes[..len] == *b"NO\n" {
            bail!("broker rejected exchange");
        }
    }
    bail!("broker rejected exchange")
}

fn success_prefix(response: &[u8]) -> Option<u8> {
    response.get(3).copied()
}

#[cfg(test)]
pub(crate) fn short_test_deadlines() -> Deadlines {
    Deadlines {
        ready: Duration::from_millis(100),
        connect: Duration::from_millis(100),
        handshake: Duration::from_millis(100),
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
#[derive(Debug)]
pub(crate) struct BrokerStream {
    stream: BrokerIo,
    pub(super) prefix: Option<u8>,
}

#[derive(Debug)]
enum BrokerIo {
    Tcp(TcpStream),
    #[cfg(test)]
    Duplex(tokio::io::DuplexStream),
}

#[cfg(test)]
impl BrokerStream {
    pub(crate) fn test_with_stream(stream: tokio::io::DuplexStream) -> Self {
        Self {
            stream: BrokerIo::Duplex(stream),
            prefix: None,
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
        if let Some(byte) = self.prefix.take() {
            buffer.put_slice(&[byte]);
            return Poll::Ready(Ok(()));
        }
        match &mut self.stream {
            BrokerIo::Tcp(stream) => Pin::new(stream).poll_read(cx, buffer),
            #[cfg(test)]
            BrokerIo::Duplex(stream) => Pin::new(stream).poll_read(cx, buffer),
        }
    }
}

impl AsyncWrite for BrokerStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        match &mut self.stream {
            BrokerIo::Tcp(stream) => Pin::new(stream).poll_write(cx, bytes),
            #[cfg(test)]
            BrokerIo::Duplex(stream) => Pin::new(stream).poll_write(cx, bytes),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut self.stream {
            BrokerIo::Tcp(stream) => Pin::new(stream).poll_flush(cx),
            #[cfg(test)]
            BrokerIo::Duplex(stream) => Pin::new(stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut self.stream {
            BrokerIo::Tcp(stream) => Pin::new(stream).poll_shutdown(cx),
            #[cfg(test)]
            BrokerIo::Duplex(stream) => Pin::new(stream).poll_shutdown(cx),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::Notify;
    use tokio::time::{Duration, timeout};

    use super::*;

    fn token() -> BrokerToken {
        "a".repeat(64).parse().unwrap()
    }

    #[test]
    fn success_prefix_requires_a_co_read_byte() {
        assert_eq!(success_prefix(b"OK\n"), None);
        assert_eq!(success_prefix(b"OK\np"), Some(b'p'));
    }

    async fn listener() -> TcpListener {
        TcpListener::bind("127.0.0.1:0").await.unwrap()
    }

    #[tokio::test]
    async fn hostname_endpoint_resolves_for_ready_and_connect() {
        let listener = listener().await;
        let endpoint = BrokerEndpoint::parse(&format!(
            "localhost:{}",
            listener.local_addr().unwrap().port()
        ))
        .unwrap();
        let ready_server = tokio::spawn({
            let listener = listener;
            async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut frame = [0; 85];
                stream.read_exact(&mut frame).await.unwrap();
                stream.write_all(b"OK\n").await.unwrap();
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut frame = [0; 128];
                let count = stream.read(&mut frame).await.unwrap();
                assert_eq!(
                    &frame[..count],
                    format!("VHRN-BROKER/1 CONNECT {} localhost:80\n", "a".repeat(64)).as_bytes()
                );
                stream.write_all(b"OK\n").await.unwrap();
            }
        });
        let protocol = BrokerProtocol::with_deadlines(endpoint, token(), short_test_deadlines());
        protocol.ready().await.unwrap();
        drop(
            protocol
                .connect(&LoopbackAuthority::parse("localhost:80").unwrap())
                .await
                .unwrap(),
        );
        ready_server.await.unwrap();
    }

    #[tokio::test]
    async fn ready_writes_exact_frame() {
        let listener = listener().await;
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut frame = vec![0; 85];
            stream.read_exact(&mut frame).await.unwrap();
            stream.write_all(b"OK\n").await.unwrap();
            frame
        });
        BrokerProtocol::new(address, token()).ready().await.unwrap();
        assert_eq!(
            server.await.unwrap(),
            format!("VHRN-BROKER/1 READY {}\n", "a".repeat(64)).into_bytes()
        );
    }

    #[tokio::test]
    async fn connect_uses_canonical_authority_and_keeps_prefix() {
        let listener = listener().await;
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut frame = vec![0; 100];
            let length = stream.read(&mut frame).await.unwrap();
            stream.write_all(b"OK\np").await.unwrap();
            frame[..length].to_vec()
        });
        let authority = LoopbackAuthority::parse("LOCALHOST:00080").unwrap();
        let mut stream = BrokerProtocol::new(address, token())
            .connect(&authority)
            .await
            .unwrap();
        let mut first = [0];
        stream.read_exact(&mut first).await.unwrap();
        assert_eq!(first, *b"p");
        assert_eq!(
            String::from_utf8(server.await.unwrap()).unwrap(),
            format!("VHRN-BROKER/1 CONNECT {} localhost:80\n", "a".repeat(64))
        );
    }

    #[tokio::test]
    async fn saved_response_bytes_complete_read_without_waiting_for_socket_data() {
        let listener = listener().await;
        let address = listener.local_addr().unwrap();
        let (connected, accepted) = tokio::join!(TcpStream::connect(address), listener.accept());
        let stream = connected.unwrap();
        let (peer, _) = accepted.unwrap();
        let (release, released) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let _peer = peer;
            released.await.unwrap();
        });
        let mut stream = BrokerStream {
            stream: BrokerIo::Tcp(stream),
            prefix: Some(b'p'),
        };
        let mut bytes = [0; 8];
        let count = timeout(Duration::from_millis(100), stream.read(&mut bytes))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(&bytes[..count], b"p");
        release.send(()).unwrap();
        drop(stream);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn connect_uses_each_canonical_loopback_spelling() {
        for (input, canonical) in [
            ("LOCALHOST:00080", "localhost:80"),
            ("127.0.0.1:00081", "127.0.0.1:81"),
            ("[0:0:0:0:0:0:0:1]:00082", "[::1]:82"),
        ] {
            let listener = listener().await;
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut frame = [0; 256];
                let count = stream.read(&mut frame).await.unwrap();
                stream.write_all(b"OK\n").await.unwrap();
                String::from_utf8(frame[..count].to_vec()).unwrap()
            });
            let authority = LoopbackAuthority::parse(input).unwrap();
            drop(
                BrokerProtocol::new(address, token())
                    .connect(&authority)
                    .await
                    .unwrap(),
            );
            assert_eq!(
                server.await.unwrap(),
                format!("VHRN-BROKER/1 CONNECT {} {canonical}\n", "a".repeat(64))
            );
        }
    }

    #[tokio::test]
    async fn broker_frame_fixture_drives_client_exchanges() {
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
                .replace("\\n", "\n");
            let response = response.replace("\\n", "\n");
            let payload = payload.replace("\\n", "\n");
            let server_payload = payload.clone();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut received = vec![0; expected.len()];
                stream.read_exact(&mut received).await.unwrap();
                assert_eq!(received, expected.as_bytes());
                if response != "timeout" {
                    stream.write_all(response.as_bytes()).await.unwrap();
                    stream.write_all(server_payload.as_bytes()).await.unwrap();
                }
            });
            let protocol = BrokerProtocol::with_deadlines(address, token(), short_test_deadlines());
            let result = match *kind {
                "ready" => protocol.ready().await,
                "connect" => match protocol
                    .connect(&LoopbackAuthority::parse("localhost:80").unwrap())
                    .await
                {
                    Ok(mut stream) => {
                        let mut received = vec![0; payload.len()];
                        stream.read_exact(&mut received).await.unwrap();
                        assert_eq!(received, payload.as_bytes());
                        Ok(())
                    }
                    Err(error) => Err(error),
                },
                _ => panic!("unknown broker fixture exchange"),
            };
            assert_eq!(result.is_ok(), matches!(*outcome, "ready" | "connected"));
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn failures_and_timeouts_are_redacted() {
        for response in [b"ERR\n".as_slice(), b"NO\n", b"O", b"TOOLONG"] {
            let listener = listener().await;
            let address = listener.local_addr().unwrap();
            let response = response.to_vec();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut frame = [0; 256];
                let _ = stream.read(&mut frame).await.unwrap();
                stream.write_all(&response).await.unwrap();
            });
            let error = BrokerProtocol::with_deadlines(address, token(), short_test_deadlines())
                .connect(&LoopbackAuthority::parse("localhost:80").unwrap())
                .await
                .unwrap_err();
            assert!(!error.to_string().contains(&"a".repeat(64)));
            server.await.unwrap();
        }
        let listener = listener().await;
        let address = listener.local_addr().unwrap();
        let (release, released) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut frame = [0; 256];
            let _ = stream.read(&mut frame).await.unwrap();
            released.await.unwrap();
        });
        let result = BrokerProtocol::with_deadlines(address, token(), short_test_deadlines())
            .ready()
            .await;
        assert!(result.is_err());
        release.send(()).unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn dropping_pending_exchange_closes_socket() {
        let listener = listener().await;
        let address = listener.local_addr().unwrap();
        let entered = Arc::new(Notify::new());
        let server_entered = entered.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut frame = [0; 256];
            let _ = stream.read(&mut frame).await.unwrap();
            server_entered.notify_one();
            let mut byte = [0];
            timeout(Duration::from_millis(500), stream.read(&mut byte))
                .await
                .unwrap()
                .unwrap()
        });
        let protocol = BrokerProtocol::with_deadlines(address, token(), short_test_deadlines());
        let task = tokio::spawn(async move {
            let _ = protocol.ready().await;
        });
        timeout(Duration::from_millis(500), entered.notified())
            .await
            .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(server.await.unwrap(), 0);
    }
}
