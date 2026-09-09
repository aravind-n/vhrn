//! Authenticated connector for the host-side loopback broker.

use std::collections::VecDeque;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::{Result, bail};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;
use tokio::time::timeout;
use vhrn_policy::{BrokerToken, LoopbackAuthority};

const READY_TIMEOUT: Duration = Duration::from_secs(3);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(13);
const MAX_RESPONSE_BYTES: usize = 4;

/// Private connector for the broker capability.
#[derive(Clone)]
pub(crate) struct BrokerConnector {
    address: SocketAddr,
    token: BrokerToken,
    deadlines: Deadlines,
}

#[derive(Clone, Copy)]
struct Deadlines {
    ready: Duration,
    connect: Duration,
    handshake: Duration,
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

impl BrokerConnector {
    pub(crate) fn new(address: SocketAddr, token: BrokerToken) -> Self {
        Self {
            address,
            token,
            deadlines: Deadlines::default(),
        }
    }

    /// Completes the startup exchange before the proxy can accept traffic.
    pub(crate) async fn ready(&self) -> Result<()> {
        let result = timeout(self.deadlines.ready, async {
            let mut stream = TcpStream::connect(self.address).await.map_err(|_| ())?;
            let frame = format!("VHRN-BROKER/1 READY {}\n", token_text(&self.token));
            stream.write_all(frame.as_bytes()).await.map_err(|_| ())?;
            read_response(&mut stream)
                .await
                .and_then(|prefix| prefix.is_empty().then_some(()).ok_or(()))
        })
        .await;
        match result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(())) | Err(_) => bail!("broker readiness failed"),
        }
    }

    /// Opens an authenticated broker stream for one already-validated authority.
    pub(crate) async fn connect(&self, authority: &LoopbackAuthority) -> Result<BrokerStream> {
        let Ok(Ok(stream)) =
            timeout(self.deadlines.connect, TcpStream::connect(self.address)).await
        else {
            bail!("broker connection failed");
        };
        let frame = format!(
            "VHRN-BROKER/1 CONNECT {} {}\n",
            token_text(&self.token),
            authority
        );
        let result = timeout(self.deadlines.handshake, async move {
            let mut stream = stream;
            stream.write_all(frame.as_bytes()).await.map_err(|_| ())?;
            let prefix = read_response(&mut stream).await?;
            Ok(BrokerStream { stream, prefix })
        })
        .await;
        match result {
            Ok(Ok(stream)) => Ok(stream),
            Ok(Err(())) | Err(_) => bail!("broker connection failed"),
        }
    }

    #[cfg(test)]
    fn with_deadlines(address: SocketAddr, token: BrokerToken, deadlines: Deadlines) -> Self {
        Self {
            address,
            token,
            deadlines,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_ready_timeout(
        address: SocketAddr,
        token: BrokerToken,
        ready: Duration,
    ) -> Self {
        Self::with_deadlines(
            address,
            token,
            Deadlines {
                ready,
                ..Deadlines::default()
            },
        )
    }
}

fn token_text(token: &BrokerToken) -> &str {
    // BrokerToken validation guarantees its bytes are ASCII.
    std::str::from_utf8(token.as_bytes()).expect("validated broker token is ASCII")
}

async fn read_response(stream: &mut TcpStream) -> std::result::Result<VecDeque<u8>, ()> {
    let mut bytes = [0; MAX_RESPONSE_BYTES];
    let mut len = 0;
    while len < bytes.len() {
        let read = stream.read(&mut bytes[len..]).await.map_err(|_| ())?;
        if read == 0 {
            return Err(());
        }
        len += read;
        if bytes[..len].starts_with(b"OK\n") {
            return Ok(bytes[3..len].iter().copied().collect());
        }
        if bytes[..len] == *b"ERR\n" || bytes[..len] == *b"NO\n" {
            return Err(());
        }
    }
    Err(())
}

/// A broker-owned stream with bytes co-read with the success response.
#[derive(Debug)]
pub(crate) struct BrokerStream {
    stream: TcpStream,
    prefix: VecDeque<u8>,
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
            while buffer.remaining() != 0 {
                let Some(byte) = self.prefix.pop_front() else {
                    break;
                };
                buffer.put_slice(&[byte]);
            }
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
    use std::sync::Arc;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::Notify;
    use tokio::time::{Duration, timeout};

    use super::*;

    fn token() -> BrokerToken {
        BrokerToken::parse("a".repeat(64)).unwrap()
    }
    fn short_deadlines() -> Deadlines {
        Deadlines {
            ready: Duration::from_millis(100),
            connect: Duration::from_millis(100),
            handshake: Duration::from_millis(100),
        }
    }
    async fn listener() -> TcpListener {
        TcpListener::bind("127.0.0.1:0").await.unwrap()
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
        BrokerConnector::new(address, token())
            .ready()
            .await
            .unwrap();
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
        let mut stream = BrokerConnector::new(address, token())
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
            stream,
            prefix: VecDeque::from(*b"p"),
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
                BrokerConnector::new(address, token())
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
        for row in include_str!("../../testdata/broker-frames.tsv")
            .lines()
            .filter(|row| !row.starts_with('#'))
        {
            let fields: Vec<_> = row.split('\t').collect();
            let [kind, wire, response, payload, outcome] = fields.as_slice() else {
                panic!("invalid broker fixture row");
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
            let connector = BrokerConnector::with_deadlines(address, token(), short_deadlines());
            let result = match *kind {
                "ready" => connector.ready().await,
                "connect" => match connector
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
            let error = BrokerConnector::with_deadlines(address, token(), short_deadlines())
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
        let result = BrokerConnector::with_deadlines(address, token(), short_deadlines())
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
        let connector = BrokerConnector::with_deadlines(address, token(), short_deadlines());
        let task = tokio::spawn(async move {
            let _ = connector.ready().await;
        });
        timeout(Duration::from_millis(500), entered.notified())
            .await
            .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(server.await.unwrap(), 0);
    }
}
