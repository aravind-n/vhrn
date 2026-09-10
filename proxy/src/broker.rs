//! Authenticated connector for the host-side loopback broker.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::{Result, bail};
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::{Body, Frame, Incoming};
use hyper::client::conn::http1;
use hyper::{HeaderMap, Request, StatusCode, Uri};
use hyper_util::rt::TokioIo;
use rustls::pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;
use tokio::task::JoinHandle;
use tokio::time::{Sleep, timeout};
use tokio_rustls::TlsConnector;
use vhrn_policy::{BrokerToken, LoopbackAuthority};

const READY_TIMEOUT: Duration = Duration::from_secs(3);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(13);
const MAX_RESPONSE_BYTES: usize = 4;
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_HTTP_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// Private connector for the broker capability.
#[derive(Clone)]
pub(crate) struct BrokerConnector {
    address: SocketAddr,
    token: BrokerToken,
    deadlines: Deadlines,
    pool: Arc<std::sync::Mutex<HashMap<BrokerKey, BrokerConnection>>>,
    http_timeout: Duration,
    response_limit: usize,
    #[cfg(test)]
    active_drivers: Arc<std::sync::atomic::AtomicUsize>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct BrokerKey {
    secure: bool,
    authority: LoopbackAuthority,
}

struct BrokerConnection {
    sender: http1::SendRequest<Full<Bytes>>,
    driver: JoinHandle<()>,
}

impl BrokerConnection {
    fn reusable(&self) -> bool {
        !self.driver.is_finished() && self.sender.is_ready()
    }
}
#[cfg(test)]
struct DriverGuard(Arc<std::sync::atomic::AtomicUsize>);
#[cfg(test)]
impl Drop for DriverGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Drop for BrokerConnection {
    fn drop(&mut self) {
        self.driver.abort();
    }
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
            pool: Arc::new(std::sync::Mutex::new(HashMap::new())),
            http_timeout: HTTP_TIMEOUT,
            response_limit: MAX_HTTP_RESPONSE_BYTES,
            #[cfg(test)]
            active_drivers: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
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
            pool: Arc::new(std::sync::Mutex::new(HashMap::new())),
            http_timeout: HTTP_TIMEOUT,
            response_limit: MAX_HTTP_RESPONSE_BYTES,
            active_drivers: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
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
    #[cfg(test)]
    fn with_http_limits(
        address: SocketAddr,
        token: BrokerToken,
        http_timeout: Duration,
        response_limit: usize,
    ) -> Self {
        let mut connector = Self::with_deadlines(address, token, short_test_deadlines());
        connector.http_timeout = http_timeout;
        connector.response_limit = response_limit;
        connector
    }
    #[cfg(test)]
    pub(crate) fn pool_len(&self) -> usize {
        self.pool.lock().map_or(0, |pool| pool.len())
    }
    #[cfg(test)]
    pub(crate) fn active_drivers(&self) -> usize {
        self.active_drivers
            .load(std::sync::atomic::Ordering::SeqCst)
    }
    #[cfg(test)]
    fn clear_pool(&self) {
        if let Ok(mut pool) = self.pool.lock() {
            pool.clear();
        }
    }
}

#[cfg(test)]
fn short_test_deadlines() -> Deadlines {
    Deadlines {
        ready: Duration::from_millis(100),
        connect: Duration::from_millis(100),
        handshake: Duration::from_millis(100),
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

pub(crate) struct BrokerResponse {
    pub(crate) status: StatusCode,
    pub(crate) headers: HeaderMap,
    pub(crate) body: BrokerBody,
}

pub(crate) struct BrokerBody {
    incoming: Incoming,
    connection: Option<BrokerConnection>,
    key: BrokerKey,
    pool: Arc<std::sync::Mutex<HashMap<BrokerKey, BrokerConnection>>>,
    bytes: usize,
    limit: usize,
    timeout: Duration,
    timer: Pin<Box<Sleep>>,
    ended: bool,
}

impl BrokerConnector {
    pub(crate) async fn http(
        &self,
        authority: &LoopbackAuthority,
        secure: bool,
        request: Request<Full<Bytes>>,
    ) -> Result<BrokerResponse> {
        let key = BrokerKey {
            secure,
            authority: authority.clone(),
        };
        let connection = self
            .pool
            .lock()
            .ok()
            .and_then(|mut pool| pool.remove(&key))
            .filter(BrokerConnection::reusable);
        let mut connection = match connection {
            Some(connection) => connection,
            None => self.open_http(authority, secure).await?,
        };
        let (mut parts, body) = request.into_parts();
        parts.headers.remove("proxy-connection");
        parts.headers.remove("proxy-authorization");
        let path = parts.uri.path_and_query().map_or("/", |path| path.as_str());
        parts.uri = path.parse::<Uri>()?;
        let response = timeout(
            self.http_timeout,
            connection
                .sender
                .send_request(Request::from_parts(parts, body)),
        )
        .await
        .map_err(|_| anyhow::anyhow!("local origin response timed out"))??;
        let (parts, incoming) = response.into_parts();
        Ok(BrokerResponse {
            status: parts.status,
            headers: parts.headers,
            body: BrokerBody {
                incoming,
                connection: Some(connection),
                key,
                pool: self.pool.clone(),
                bytes: 0,
                limit: self.response_limit,
                timeout: self.http_timeout,
                timer: Box::pin(tokio::time::sleep(self.http_timeout)),
                ended: false,
            },
        })
    }

    async fn open_http(
        &self,
        authority: &LoopbackAuthority,
        secure: bool,
    ) -> Result<BrokerConnection> {
        let stream = self.connect(authority).await?;
        let stream: Box<dyn AsyncReadWrite> = if secure {
            let _ = rustls::crypto::ring::default_provider().install_default();
            let roots =
                rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let config = rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();
            let name = ServerName::try_from("localhost")?;
            Box::new(
                timeout(
                    HTTP_TIMEOUT,
                    TlsConnector::from(Arc::new(config)).connect(name, stream),
                )
                .await
                .map_err(|_| anyhow::anyhow!("local TLS handshake timed out"))??,
            )
        } else {
            Box::new(stream)
        };
        let (sender, connection) =
            timeout(self.http_timeout, http1::handshake(TokioIo::new(stream)))
                .await
                .map_err(|_| anyhow::anyhow!("local origin handshake timed out"))??;
        #[cfg(test)]
        let guard = {
            self.active_drivers
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            DriverGuard(self.active_drivers.clone())
        };
        let driver = tokio::spawn(async move {
            #[cfg(test)]
            let _guard = guard;
            let _ = connection.await;
        });
        Ok(BrokerConnection { sender, driver })
    }
}

trait AsyncReadWrite: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T> AsyncReadWrite for T where T: AsyncRead + AsyncWrite + Send + Unpin {}

impl BrokerBody {
    fn return_connection(&mut self) {
        let Some(connection) = self.connection.take() else {
            return;
        };
        if connection.reusable()
            && let Ok(mut pool) = self.pool.lock()
        {
            pool.insert(self.key.clone(), connection);
        }
    }
}

impl Body for BrokerBody {
    type Data = Bytes;
    type Error = anyhow::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        if self.ended {
            return Poll::Ready(None);
        }
        match Pin::new(&mut self.incoming).poll_frame(cx) {
            Poll::Pending => {
                if self.timer.as_mut().poll(cx).is_ready() {
                    self.connection.take();
                    self.ended = true;
                    Poll::Ready(Some(Err(anyhow::anyhow!("local origin body timed out"))))
                } else {
                    Poll::Pending
                }
            }
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    let remaining = self.limit.saturating_sub(self.bytes);
                    if data.len() > remaining {
                        self.connection.take();
                        self.ended = true;
                        return Poll::Ready(Some(Err(anyhow::anyhow!(
                            "local origin body too large"
                        ))));
                    }
                    self.bytes += data.len();
                }
                let deadline = tokio::time::Instant::now() + self.timeout;
                self.timer.as_mut().reset(deadline);
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(error))) => {
                self.connection.take();
                self.ended = true;
                Poll::Ready(Some(Err(anyhow::Error::new(error))))
            }
            Poll::Ready(None) => {
                self.return_connection();
                self.ended = true;
                Poll::Ready(None)
            }
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

    use http_body_util::BodyExt;
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

    async fn read_http_request(stream: &mut TcpStream) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut chunk = [0; 256];
        loop {
            let read = timeout(Duration::from_millis(500), stream.read(&mut chunk))
                .await
                .unwrap()
                .unwrap();
            assert_ne!(read, 0, "origin closed before completing request");
            bytes.extend_from_slice(&chunk[..read]);
            assert!(bytes.len() <= 16 * 1024, "request exceeds test bound");
            let Some(headers_end) = bytes.windows(4).position(|value| value == b"\r\n\r\n") else {
                continue;
            };
            let headers = std::str::from_utf8(&bytes[..headers_end]).unwrap();
            let length = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length: "))
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(0);
            if bytes.len() >= headers_end + 4 + length {
                return bytes;
            }
        }
    }

    async fn wait_for_no_drivers(connector: &BrokerConnector) {
        timeout(Duration::from_millis(500), async {
            while connector.active_drivers() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    fn local_request() -> Request<Full<Bytes>> {
        Request::builder()
            .uri("http://localhost:80/")
            .body(Full::new(Bytes::new()))
            .unwrap()
    }

    async fn accept_broker(listener: &TcpListener) -> TcpStream {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut frame = vec![0; 100];
        stream.read_exact(&mut frame).await.unwrap();
        stream.write_all(b"OK\n").await.unwrap();
        let _ = read_http_request(&mut stream).await;
        stream
    }

    #[tokio::test]
    async fn local_http_streams_reuses_and_forwards_exactly() {
        let listener = Arc::new(listener().await);
        let address = listener.local_addr().unwrap();
        let (release, released) = tokio::sync::oneshot::channel();
        let server_listener = listener.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = server_listener.accept().await.unwrap();
            let mut frame = vec![0; 100];
            stream.read_exact(&mut frame).await.unwrap();
            assert_eq!(
                String::from_utf8(frame).unwrap(),
                format!("VHRN-BROKER/1 CONNECT {} localhost:80\n", "a".repeat(64))
            );
            stream.write_all(b"OK\n").await.unwrap();
            let request = read_http_request(&mut stream).await;
            let request = String::from_utf8_lossy(&request);
            assert!(request.starts_with("POST /path?q=one HTTP/1.1\r\n"));
            assert!(request.contains("host: localhost:80"));
            assert!(request.contains("connection: X-Ordinary"));
            assert!(request.contains("x-ordinary: kept"));
            assert!(!request.contains("proxy-connection"));
            assert!(!request.contains("proxy-authorization"));
            assert!(request.ends_with("exact body"));
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nfirst\r\n")
                .await
                .unwrap();
            released.await.unwrap();
            stream.write_all(b"6\r\nsecond\r\n0\r\n\r\n").await.unwrap();
            let request = read_http_request(&mut stream).await;
            assert!(request.starts_with(b"GET /again HTTP/1.1\r\n"));
            stream
                .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
            let mut byte = [0];
            assert_eq!(
                timeout(Duration::from_millis(500), stream.read(&mut byte))
                    .await
                    .unwrap()
                    .unwrap(),
                0
            );
        });
        let connector =
            BrokerConnector::with_http_limits(address, token(), Duration::from_millis(200), 1024);
        let authority = LoopbackAuthority::parse("localhost:80").unwrap();
        let request = Request::builder()
            .method("POST")
            .uri("http://localhost:80/path?q=one")
            .header("host", "localhost:80")
            .header("connection", "X-Ordinary")
            .header("x-ordinary", "kept")
            .header("proxy-connection", "close")
            .header("proxy-authorization", "ignored")
            .body(Full::new(Bytes::from_static(b"exact body")))
            .unwrap();
        let mut response = connector.http(&authority, false, request).await.unwrap();
        let first = timeout(Duration::from_millis(100), response.body.frame())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .into_data()
            .unwrap();
        assert_eq!(first, "first");
        release.send(()).unwrap();
        assert_eq!(response.body.collect().await.unwrap().to_bytes(), "second");
        assert_eq!(connector.pool_len(), 1);
        assert_eq!(connector.active_drivers(), 1);
        let second = Request::builder()
            .uri("http://localhost:80/again")
            .body(Full::new(Bytes::new()))
            .unwrap();
        let second = connector.http(&authority, false, second).await.unwrap();
        assert_eq!(second.status, StatusCode::NO_CONTENT);
        assert!(second.body.collect().await.unwrap().to_bytes().is_empty());
        assert_eq!(connector.pool_len(), 1);
        assert_eq!(connector.active_drivers(), 1);
        assert!(
            timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        );
        connector.clear_pool();
        server.await.unwrap();
        for _ in 0..8 {
            if connector.active_drivers() == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(connector.active_drivers(), 0);
    }

    #[tokio::test]
    async fn local_body_drop_discards_connection() {
        let listener = listener().await;
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut stream = accept_broker(&listener).await;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n")
                .await
                .unwrap();
            let mut byte = [0];
            assert_eq!(
                timeout(Duration::from_millis(500), stream.read(&mut byte))
                    .await
                    .unwrap()
                    .unwrap(),
                0
            );
        });
        let connector =
            BrokerConnector::with_http_limits(address, token(), Duration::from_millis(100), 16);
        let authority = LoopbackAuthority::parse("localhost:80").unwrap();
        let response = connector
            .http(&authority, false, local_request())
            .await
            .unwrap();
        drop(response);
        assert_eq!(connector.pool_len(), 0);
        server.await.unwrap();
        wait_for_no_drivers(&connector).await;
    }

    #[tokio::test]
    async fn local_body_truncation_is_terminal_and_discards_connection() {
        let listener = listener().await;
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut stream = accept_broker(&listener).await;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nab")
                .await
                .unwrap();
            stream.shutdown().await.unwrap();
            let mut byte = [0];
            assert_eq!(
                timeout(Duration::from_millis(500), stream.read(&mut byte))
                    .await
                    .unwrap()
                    .unwrap(),
                0
            );
        });
        let connector =
            BrokerConnector::with_http_limits(address, token(), Duration::from_millis(100), 16);
        let authority = LoopbackAuthority::parse("localhost:80").unwrap();
        let mut response = connector
            .http(&authority, false, local_request())
            .await
            .unwrap();
        assert!(
            response
                .body
                .frame()
                .await
                .unwrap()
                .unwrap()
                .into_data()
                .is_ok()
        );
        assert!(response.body.frame().await.unwrap().is_err());
        assert!(response.body.frame().await.is_none());
        assert_eq!(connector.pool_len(), 0);
        server.await.unwrap();
        wait_for_no_drivers(&connector).await;
    }

    #[tokio::test]
    async fn local_body_timeout_is_terminal_and_discards_connection() {
        let listener = listener().await;
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut stream = accept_broker(&listener).await;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\n")
                .await
                .unwrap();
            let mut byte = [0];
            assert_eq!(
                timeout(Duration::from_millis(500), stream.read(&mut byte))
                    .await
                    .unwrap()
                    .unwrap(),
                0
            );
        });
        let connector =
            BrokerConnector::with_http_limits(address, token(), Duration::from_millis(20), 16);
        let authority = LoopbackAuthority::parse("localhost:80").unwrap();
        let mut response = connector
            .http(&authority, false, local_request())
            .await
            .unwrap();
        assert!(response.body.frame().await.unwrap().is_err());
        assert!(response.body.frame().await.is_none());
        assert_eq!(connector.pool_len(), 0);
        server.await.unwrap();
        wait_for_no_drivers(&connector).await;
    }

    #[tokio::test]
    async fn local_body_limit_is_terminal_and_discards_connection() {
        let listener = listener().await;
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut stream = accept_broker(&listener).await;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nabc")
                .await
                .unwrap();
            let mut byte = [0];
            assert_eq!(
                timeout(Duration::from_millis(500), stream.read(&mut byte))
                    .await
                    .unwrap()
                    .unwrap(),
                0
            );
        });
        let connector =
            BrokerConnector::with_http_limits(address, token(), Duration::from_millis(100), 2);
        let authority = LoopbackAuthority::parse("localhost:80").unwrap();
        let mut response = connector
            .http(&authority, false, local_request())
            .await
            .unwrap();
        assert!(response.body.frame().await.unwrap().is_err());
        assert!(response.body.frame().await.is_none());
        assert_eq!(connector.pool_len(), 0);
        server.await.unwrap();
        wait_for_no_drivers(&connector).await;
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
