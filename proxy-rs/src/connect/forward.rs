//! One streaming HTTP/1 exchange over an already-checked origin stream.

use std::collections::VecDeque;
use std::hash::Hash;

use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::header::{CONTENT_LENGTH, EXPECT, HOST, HeaderValue, TRANSFER_ENCODING};
use hyper::{HeaderMap, Method, StatusCode};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, WriteHalf};

use crate::Shutdown;
use crate::connect::origin_body::{
    OriginBodyConfig, OriginLease, OriginReader, OriginResponse, SharedOriginResponse,
    StreamingOriginBody,
};
use crate::headers::{append_via, connection_nominations, sanitize_hop_by_hop};
use crate::server::http1::{
    BodyDecoder, BodyFrame, BodyFraming, Http1Connection, OriginBodyFraming, OriginError,
    OriginResponseHead, OriginTransferCoding, RequestHead,
};
use crate::server::response::write_informational;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ForwardErrorKind {
    BadRequest,
    BadGateway,
    Cancelled,
    ClientDisconnected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ForwardError {
    pub(crate) kind: ForwardErrorKind,
    pub(crate) request_complete: bool,
}

pub(crate) struct Forwarded {
    pub(crate) response: SharedOriginResponse,
    pub(crate) request_complete: bool,
}

pub(crate) const fn forward_error(kind: ForwardErrorKind, request_complete: bool) -> ForwardError {
    ForwardError {
        kind,
        request_complete,
    }
}

pub(crate) async fn exchange<D, K, S>(
    downstream: &mut Http1Connection<D>,
    head: &RequestHead,
    path_and_query: &[u8],
    host: &str,
    lease: OriginLease<K, S>,
    cancellation: &Shutdown,
) -> Result<Forwarded, ForwardError>
where
    D: AsyncRead + AsyncWrite + Unpin,
    K: Clone + Eq + Hash + Send + Unpin + 'static,
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut reader, mut writer, key, pool) = lease.into_parts();
    let request_nominations = connection_nominations(&head.headers)
        .map_err(|_| forward_error(ForwardErrorKind::BadRequest, false))?;
    let request_head = serialize_request_head(head, path_and_query, host)
        .map_err(|()| forward_error(ForwardErrorKind::BadRequest, false))?;
    write_request_head(downstream, &mut writer, &request_head, cancellation).await?;

    let mut request = RequestWriter::new(head.framing, request_nominations);
    loop {
        let response = advance_exchange(
            downstream,
            &head.method,
            &mut reader,
            &mut writer,
            &mut request,
            cancellation,
        )
        .await
        .map_err(|kind| forward_error(kind, request.complete))?;
        let Some(response) = response else {
            continue;
        };
        let mut response =
            response.map_err(|_| forward_error(ForwardErrorKind::BadGateway, request.complete))?;
        if response.status == StatusCode::SWITCHING_PROTOCOLS {
            return Err(forward_error(
                ForwardErrorKind::BadGateway,
                request.complete,
            ));
        }
        let transfer_codings = response
            .headers
            .get_all(TRANSFER_ENCODING)
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        let response_nominations = connection_nominations(&response.headers)
            .map_err(|_| forward_error(ForwardErrorKind::BadGateway, request.complete))?;
        sanitize_hop_by_hop(&mut response.headers)
            .map_err(|_| forward_error(ForwardErrorKind::BadGateway, request.complete))?;
        if head.version == hyper::Version::HTTP_11 {
            for value in transfer_codings {
                response.headers.append(TRANSFER_ENCODING, value);
            }
            if response.transfer_coding == OriginTransferCoding::FinalNonChunked
                && response.framing != OriginBodyFraming::None
            {
                response
                    .headers
                    .append(TRANSFER_ENCODING, HeaderValue::from_static("chunked"));
            }
        } else if response.framing != OriginBodyFraming::None
            && matches!(
                response.transfer_coding,
                OriginTransferCoding::FinalNonChunked
                    | OriginTransferCoding::FinalChunked { has_prior: true }
            )
        {
            return Err(forward_error(
                ForwardErrorKind::BadGateway,
                request.complete,
            ));
        }
        append_via(&mut response.headers, response.version);
        if response.status.is_informational() {
            forward_informational(downstream, &response, head.version)
                .await
                .map_err(|_| {
                    forward_error(ForwardErrorKind::ClientDisconnected, request.complete)
                })?;
            continue;
        }

        let request_complete = request.complete;
        let poolable = request_complete
            && !response.close
            && response.framing != OriginBodyFraming::CloseDelimited;
        let body = StreamingOriginBody::new(
            reader,
            writer,
            key,
            pool,
            OriginBodyConfig {
                framing: response.framing,
                trailer_nominations: response_nominations,
                poolable,
                shutdown: cancellation.clone(),
            },
        )
        .boxed_unsync();
        return Ok(Forwarded {
            response: OriginResponse {
                status: response.status,
                headers: response.headers,
                body,
            },
            request_complete,
        });
    }
}

async fn forward_informational<D>(
    downstream: &mut Http1Connection<D>,
    response: &OriginResponseHead,
    downstream_version: hyper::Version,
) -> std::io::Result<()>
where
    D: AsyncRead + AsyncWrite + Unpin,
{
    if downstream_version == hyper::Version::HTTP_10 {
        return Ok(());
    }
    write_informational(
        downstream,
        response.status,
        &response.headers,
        downstream_version,
    )
    .await
}

async fn advance_exchange<D, S>(
    downstream: &mut Http1Connection<D>,
    method: &Method,
    reader: &mut OriginReader<tokio::io::ReadHalf<S>>,
    writer: &mut WriteHalf<S>,
    request: &mut RequestWriter,
    cancellation: &Shutdown,
) -> Result<Option<Result<OriginResponseHead, OriginError>>, ForwardErrorKind>
where
    D: AsyncRead + AsyncWrite + Unpin,
    S: AsyncRead + AsyncWrite + Unpin,
{
    if request.complete {
        return tokio::select! {
            biased;
            () = cancellation.cancelled() => Err(ForwardErrorKind::Cancelled),
            response = reader.read_response_head(method) => Ok(Some(response)),
            _ = downstream.wait_for_peer_close() => Err(ForwardErrorKind::ClientDisconnected),
        };
    }
    if request.waiting_on_upstream() {
        return tokio::select! {
            biased;
            () = cancellation.cancelled() => Err(ForwardErrorKind::Cancelled),
            response = reader.read_response_head(method) => Ok(Some(response)),
            sent = request.step_upstream(writer) => sent.map(|()| None),
            _ = downstream.wait_for_peer_close() => Err(ForwardErrorKind::ClientDisconnected),
        };
    }
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(ForwardErrorKind::Cancelled),
        response = reader.read_response_head(method) => Ok(Some(response)),
        staged = request.step_downstream(downstream) => staged.map(|()| None),
    }
}

async fn write_request_head<D, S>(
    downstream: &mut Http1Connection<D>,
    writer: &mut WriteHalf<S>,
    request_head: &[u8],
    cancellation: &Shutdown,
) -> Result<(), ForwardError>
where
    D: AsyncRead + AsyncWrite + Unpin,
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut offset = 0;
    while offset != request_head.len() {
        let written = tokio::select! {
            biased;
            () = cancellation.cancelled() => {
                return Err(forward_error(ForwardErrorKind::Cancelled, false));
            },
            result = writer.write(&request_head[offset..]) => result
                .map_err(|_| forward_error(ForwardErrorKind::BadGateway, false))?,
            _ = downstream.wait_for_peer_close() => {
                return Err(forward_error(ForwardErrorKind::ClientDisconnected, false));
            },
        };
        if written == 0 {
            return Err(forward_error(ForwardErrorKind::BadGateway, false));
        }
        offset += written;
    }
    tokio::select! {
        biased;
        () = cancellation.cancelled() => {
            Err(forward_error(ForwardErrorKind::Cancelled, false))
        },
        result = writer.flush() => result
            .map_err(|_| forward_error(ForwardErrorKind::BadGateway, false)),
        _ = downstream.wait_for_peer_close() => {
            Err(forward_error(ForwardErrorKind::ClientDisconnected, false))
        },
    }
}

fn serialize_request_head(
    head: &RequestHead,
    path_and_query: &[u8],
    host: &str,
) -> Result<Vec<u8>, ()> {
    let path = if head.method == Method::OPTIONS && path_and_query.is_empty() {
        b"*".as_slice()
    } else if path_and_query.is_empty() {
        b"/".as_slice()
    } else if path_and_query[0] == b'?' {
        // Origin-form always begins with an absolute path.
        // This one-byte prefix is emitted with the request line below.
        b"".as_slice()
    } else {
        path_and_query
    };
    let mut headers = head.headers.clone();
    let transfer_codings = headers
        .get_all(TRANSFER_ENCODING)
        .iter()
        .cloned()
        .collect::<Vec<_>>();
    sanitize_hop_by_hop(&mut headers).map_err(|_| ())?;
    if head.version == hyper::Version::HTTP_10 {
        headers.remove(EXPECT);
    }
    headers.remove(HOST);
    headers.remove(CONTENT_LENGTH);
    headers.remove(TRANSFER_ENCODING);
    headers.insert(HOST, HeaderValue::from_str(host).map_err(|_| ())?);
    append_via(&mut headers, head.version);
    match head.framing {
        BodyFraming::None => {}
        BodyFraming::ContentLength(length) => {
            headers.insert(
                CONTENT_LENGTH,
                HeaderValue::from_str(&length.to_string()).map_err(|_| ())?,
            );
        }
        BodyFraming::Chunked => {
            for value in transfer_codings {
                headers.append(TRANSFER_ENCODING, value);
            }
        }
    }

    let mut bytes = Vec::new();
    bytes.extend_from_slice(head.method.as_str().as_bytes());
    bytes.push(b' ');
    if path.is_empty() {
        bytes.extend_from_slice(b"/");
    }
    bytes.extend_from_slice(path_and_query_if_needed(path, path_and_query));
    bytes.extend_from_slice(b" HTTP/1.1\r\n");
    serialize_fields(&mut bytes, &headers);
    bytes.extend_from_slice(b"\r\n");
    Ok(bytes)
}

fn path_and_query_if_needed<'a>(path: &'a [u8], original: &'a [u8]) -> &'a [u8] {
    if path.is_empty() { original } else { path }
}

fn serialize_fields(bytes: &mut Vec<u8>, headers: &HeaderMap) {
    for (name, value) in headers {
        bytes.extend_from_slice(name.as_str().as_bytes());
        bytes.extend_from_slice(b": ");
        bytes.extend_from_slice(value.as_bytes());
        bytes.extend_from_slice(b"\r\n");
    }
}

enum RequestState {
    NeedFrame,
    ChunkData(Bytes),
    ChunkEnd,
    TrailerFields(VecDeque<(hyper::header::HeaderName, HeaderValue)>),
    TrailerValuePrefix(
        HeaderValue,
        VecDeque<(hyper::header::HeaderName, HeaderValue)>,
    ),
    TrailerValue(
        HeaderValue,
        VecDeque<(hyper::header::HeaderName, HeaderValue)>,
    ),
    TrailerLineEnd(VecDeque<(hyper::header::HeaderName, HeaderValue)>),
    Flush,
}

impl RequestState {
    fn buffered_bytes(&self) -> usize {
        match self {
            Self::ChunkData(data) => data.len(),
            Self::TrailerFields(fields) => fields
                .iter()
                .map(|(name, value)| name.as_str().len() + value.as_bytes().len())
                .sum(),
            Self::TrailerValuePrefix(value, fields) | Self::TrailerValue(value, fields) => {
                value.as_bytes().len()
                    + fields
                        .iter()
                        .map(|(name, value)| name.as_str().len() + value.as_bytes().len())
                        .sum::<usize>()
            }
            Self::TrailerLineEnd(fields) => fields
                .iter()
                .map(|(name, value)| name.as_str().len() + value.as_bytes().len())
                .sum(),
            Self::NeedFrame | Self::ChunkEnd | Self::Flush => 0,
        }
    }
}

enum PendingBytes {
    Bytes(Bytes),
    HeaderName(hyper::header::HeaderName),
    HeaderValue(HeaderValue),
}

impl PendingBytes {
    fn as_bytes(&self) -> &[u8] {
        match self {
            Self::Bytes(bytes) => bytes,
            Self::HeaderName(name) => name.as_str().as_bytes(),
            Self::HeaderValue(value) => value.as_bytes(),
        }
    }
}

struct PendingWrite {
    bytes: PendingBytes,
    offset: usize,
    next: RequestState,
}

impl PendingWrite {
    fn buffered_bytes(&self) -> usize {
        self.bytes.as_bytes().len() - self.offset + self.next.buffered_bytes()
    }
}

struct RequestWriter {
    decoder: BodyDecoder,
    chunked: bool,
    state: RequestState,
    pending: Option<PendingWrite>,
    trailer_nominations: Vec<hyper::header::HeaderName>,
    complete: bool,
}

impl RequestWriter {
    fn new(framing: BodyFraming, trailer_nominations: Vec<hyper::header::HeaderName>) -> Self {
        Self {
            decoder: BodyDecoder::new(framing),
            chunked: matches!(framing, BodyFraming::Chunked),
            state: RequestState::NeedFrame,
            pending: None,
            trailer_nominations,
            complete: false,
        }
    }

    fn waiting_on_upstream(&self) -> bool {
        self.pending.is_some() || matches!(self.state, RequestState::Flush)
    }

    fn buffered_bytes(&self) -> usize {
        self.decoder.buffered_bytes()
            + self.state.buffered_bytes()
            + self
                .pending
                .as_ref()
                .map_or(0, PendingWrite::buffered_bytes)
    }

    async fn step_upstream<S>(&mut self, writer: &mut WriteHalf<S>) -> Result<(), ForwardErrorKind>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        if let Some(pending) = &mut self.pending {
            let bytes = pending.bytes.as_bytes();
            let written = writer
                .write(&bytes[pending.offset..])
                .await
                .map_err(|_| ForwardErrorKind::BadGateway)?;
            if written == 0 {
                return Err(ForwardErrorKind::BadGateway);
            }
            pending.offset += written;
            if pending.offset == pending.bytes.as_bytes().len() {
                let pending = self.pending.take().expect("pending write is present");
                self.state = pending.next;
            }
            return Ok(());
        }

        debug_assert!(matches!(self.state, RequestState::Flush));
        writer
            .flush()
            .await
            .map_err(|_| ForwardErrorKind::BadGateway)?;
        self.complete = true;
        Ok(())
    }

    async fn step_downstream<D>(
        &mut self,
        downstream: &mut Http1Connection<D>,
    ) -> Result<(), ForwardErrorKind>
    where
        D: AsyncRead + AsyncWrite + Unpin,
    {
        debug_assert!(!self.waiting_on_upstream());
        match std::mem::replace(&mut self.state, RequestState::NeedFrame) {
            RequestState::NeedFrame => {
                match downstream
                    .read_body_frame(&mut self.decoder)
                    .await
                    .map_err(|_| ForwardErrorKind::BadRequest)?
                {
                    Some(BodyFrame::Data(data)) if self.chunked => {
                        self.queue(
                            Bytes::from(format!("{:x}\r\n", data.len())),
                            RequestState::ChunkData(data),
                        );
                    }
                    Some(BodyFrame::Data(data)) => {
                        self.queue(data, RequestState::NeedFrame);
                    }
                    Some(BodyFrame::Trailers(mut trailers)) => {
                        sanitize_hop_by_hop(&mut trailers)
                            .map_err(|_| ForwardErrorKind::BadRequest)?;
                        for name in &self.trailer_nominations {
                            trailers.remove(name);
                        }
                        let fields = owned_fields(trailers);
                        self.queue(
                            Bytes::from_static(b"0\r\n"),
                            RequestState::TrailerFields(fields),
                        );
                    }
                    None if self.chunked => {
                        self.queue(Bytes::from_static(b"0\r\n\r\n"), RequestState::Flush);
                    }
                    None => self.state = RequestState::Flush,
                }
            }
            RequestState::ChunkData(data) => self.queue(data, RequestState::ChunkEnd),
            RequestState::ChunkEnd => {
                self.queue(Bytes::from_static(b"\r\n"), RequestState::NeedFrame);
            }
            RequestState::TrailerFields(mut fields) => {
                if let Some((name, value)) = fields.pop_front() {
                    self.queue(
                        PendingBytes::HeaderName(name),
                        RequestState::TrailerValuePrefix(value, fields),
                    );
                } else {
                    self.queue(Bytes::from_static(b"\r\n"), RequestState::Flush);
                }
            }
            RequestState::TrailerValuePrefix(value, fields) => {
                self.queue(
                    Bytes::from_static(b": "),
                    RequestState::TrailerValue(value, fields),
                );
            }
            RequestState::TrailerValue(value, fields) => {
                self.queue(
                    PendingBytes::HeaderValue(value),
                    RequestState::TrailerLineEnd(fields),
                );
            }
            RequestState::TrailerLineEnd(fields) => {
                self.queue(
                    Bytes::from_static(b"\r\n"),
                    RequestState::TrailerFields(fields),
                );
            }
            RequestState::Flush => unreachable!("upstream state is handled separately"),
        }
        #[cfg(test)]
        downstream.observe_application_buffer(self.buffered_bytes());
        Ok(())
    }

    fn queue(&mut self, bytes: impl Into<PendingBytes>, next: RequestState) {
        self.pending = Some(PendingWrite {
            bytes: bytes.into(),
            offset: 0,
            next,
        });
        debug_assert!(self.buffered_bytes() <= crate::server::http1::APPLICATION_BUFFER_LIMIT);
    }
}

impl From<Bytes> for PendingBytes {
    fn from(bytes: Bytes) -> Self {
        Self::Bytes(bytes)
    }
}

fn owned_fields(headers: HeaderMap) -> VecDeque<(hyper::header::HeaderName, HeaderValue)> {
    let mut fields = VecDeque::new();
    let mut current_name = None;
    let mut entries = headers.into_iter().peekable();
    while let Some((name, value)) = entries.next() {
        let repeated_next = entries.peek().is_some_and(|(name, _)| name.is_none());
        let name = if let Some(name) = name {
            if repeated_next {
                current_name = Some(name.clone());
            }
            name
        } else if repeated_next {
            current_name
                .as_ref()
                .expect("repeated header value has a field name")
                .clone()
        } else {
            current_name
                .take()
                .expect("repeated header value has a field name")
        };
        fields.push_back((name, value));
    }
    fields
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::connect::broker::BrokerConnector;
    use crate::connect::public::{
        DialFuture, NumericDialer, PublicConnector, PublicStream, ResolveFuture, ResolvedAddress,
        Resolver,
    };
    use crate::server::http1::{
        APPLICATION_BUFFER_LIMIT, HEADER_SECTION_LIMIT, REQUEST_BODY_FRAME_LIMIT,
        REQUEST_LINE_LIMIT,
    };
    use crate::server::listener::serve_test_connection;
    use crate::server::router::RequestContext;
    use hyper::{Version, header};
    use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::oneshot;
    use tokio::time::{Duration, timeout};

    struct PendingDrop(Arc<AtomicBool>);

    impl Drop for PendingDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    struct PendingForwardResolver {
        started: Arc<AtomicBool>,
        dropped: Arc<AtomicBool>,
    }

    impl Resolver for PendingForwardResolver {
        fn resolve(&self, _: String, _: u16) -> ResolveFuture {
            let started = self.started.clone();
            let dropped = self.dropped.clone();
            Box::pin(async move {
                let _drop = PendingDrop(dropped);
                started.store(true, Ordering::SeqCst);
                std::future::pending().await
            })
        }
    }

    struct PendingForwardDialer {
        started: Arc<AtomicBool>,
        dropped: Arc<AtomicBool>,
    }

    impl NumericDialer for PendingForwardDialer {
        fn dial(&self, _: SocketAddr) -> DialFuture {
            let started = self.started.clone();
            let dropped = self.dropped.clone();
            Box::pin(async move {
                let _drop = PendingDrop(dropped);
                started.store(true, Ordering::SeqCst);
                std::future::pending().await
            })
        }
    }

    struct NeverDialer;

    impl NumericDialer for NeverDialer {
        fn dial(&self, _: SocketAddr) -> DialFuture {
            panic!("resolver never completed")
        }
    }

    fn head(method: Method, target: &[u8], framing: BodyFraming) -> RequestHead {
        let mut headers = HeaderMap::new();
        headers.insert(HOST, HeaderValue::from_static("conflict.invalid"));
        RequestHead {
            method,
            raw_target: target.to_vec(),
            version: Version::HTTP_11,
            headers,
            framing,
            close: false,
            upgrade: false,
            expect: false,
        }
    }

    #[test]
    fn request_line_uses_origin_form_options_star_and_canonical_host() {
        let request = head(Method::GET, b"http://example.com?q", BodyFraming::None);
        let wire = serialize_request_head(&request, b"?q", "example.com").unwrap();
        assert!(wire.starts_with(b"GET /?q HTTP/1.1\r\n"));
        assert!(
            wire.windows(19)
                .any(|value| value == b"host: example.com\r\n")
        );
        assert!(!wire.windows(16).any(|value| value == b"conflict.invalid"));

        let request = head(Method::OPTIONS, b"http://example.com", BodyFraming::None);
        let wire = serialize_request_head(&request, b"", "example.com").unwrap();
        assert!(wire.starts_with(b"OPTIONS * HTTP/1.1\r\n"));
        assert!(wire.windows(15).any(|value| value == b"via: 1.1 vhrn\r\n"));
        assert!(!wire.windows(12).any(|value| value == b"forwarded:"));
        assert!(!wire.windows(16).any(|value| value == b"x-forwarded-for:"));
        assert!(
            !wire
                .windows(21)
                .any(|value| value == b"proxy-authorization:")
        );
        assert!(
            !wire
                .windows(20)
                .any(|value| value == b"proxy-authenticate:")
        );
        assert!(!wire.windows(17).any(|value| value == b"proxy-connection:"));
        assert!(!wire.windows(9).any(|value| value == b"upgrade:"));
        assert!(!wire.windows(11).any(|value| value == b"keep-alive:"));
        assert!(!wire.windows(4).any(|value| value == b"te:"));
        assert!(!wire.windows(9).any(|value| value == b"trailer:"));
        assert!(!wire.windows(13).any(|value| value == b"connection:"));
        assert!(!wire.windows(18).any(|value| value == b"internal-address"));
        assert!(!wire.windows(13).any(|value| value == b"x-real-ip:"));
        assert!(!wire.windows(15).any(|value| value == b"proxy-via-host"));
        assert!(wire.ends_with(b"\r\n\r\n"));
        let _ = header::VIA;

        let mut request = head(
            Method::POST,
            b"http://example.com",
            BodyFraming::ContentLength(1),
        );
        request.version = Version::HTTP_10;
        request
            .headers
            .insert(EXPECT, HeaderValue::from_static("100-continue"));
        let wire = serialize_request_head(&request, b"", "example.com").unwrap();
        assert!(!wire.windows(7).any(|value| value == b"expect:"));
    }

    #[tokio::test]
    async fn stalled_chunk_prefix_accounts_for_its_full_retained_data_frame() {
        let mut request = RequestWriter::new(BodyFraming::Chunked, Vec::new());
        let data = Bytes::from(vec![b'x'; REQUEST_BODY_FRAME_LIMIT]);
        let prefix = Bytes::from(format!("{:x}\r\n", data.len()));
        let expected = prefix.len() + data.len();
        request.queue(prefix, RequestState::ChunkData(data));
        assert_eq!(request.buffered_bytes(), expected);
        assert!(request.buffered_bytes() <= APPLICATION_BUFFER_LIMIT);

        let (stream, _stalled) = tokio::io::duplex(1);
        let (_reader, mut writer) = tokio::io::split(stream);
        request.step_upstream(&mut writer).await.unwrap();
        assert_eq!(request.buffered_bytes(), expected - 1);
        assert!(request.buffered_bytes() <= APPLICATION_BUFFER_LIMIT);
    }

    fn public_context(
        directory: &std::path::Path,
        public: PublicConnector,
        shutdown: &Shutdown,
    ) -> Arc<RequestContext> {
        let allowlist = directory.join("allowlist");
        let mode = directory.join("mode");
        std::fs::write(&allowlist, "allowed.example\n").unwrap();
        std::fs::write(&mode, "enforce\n").unwrap();
        let config = crate::Config::resolve(|name| match name {
            "VHRN_ALLOWLIST" => Some(allowlist.display().to_string()),
            "VHRN_MODE_FILE" => Some(mode.display().to_string()),
            "VHRN_PROXY_LISTEN" => Some("127.0.0.1:8080".to_owned()),
            _ => None,
        })
        .unwrap();
        Arc::new(RequestContext::new(config, public, None, shutdown))
    }

    fn local_context(
        directory: &std::path::Path,
        local: BrokerConnector,
        shutdown: &Shutdown,
    ) -> (Arc<RequestContext>, [std::path::PathBuf; 3]) {
        let allowlist = directory.join("allowlist");
        let mode = directory.join("mode");
        let token = directory.join("token");
        let local_paths = [
            directory.join("local-global"),
            directory.join("local-project"),
            directory.join("local-run"),
        ];
        std::fs::write(&allowlist, "").unwrap();
        std::fs::write(&mode, "enforce\n").unwrap();
        std::fs::write(&token, "a".repeat(64)).unwrap();
        std::fs::write(&local_paths[0], "localhost:80\n").unwrap();
        std::fs::write(&local_paths[1], "").unwrap();
        std::fs::write(&local_paths[2], "").unwrap();
        let joined = local_paths
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(",");
        let config = crate::Config::resolve(|name| match name {
            "VHRN_ALLOWLIST" => Some(allowlist.display().to_string()),
            "VHRN_MODE_FILE" => Some(mode.display().to_string()),
            "VHRN_PROXY_LISTEN" => Some("127.0.0.1:8080".to_owned()),
            "VHRN_LOOPBACK_ALLOWLISTS" => Some(joined.clone()),
            "VHRN_BROKER_ADDR" => Some("127.0.0.1:1".to_owned()),
            "VHRN_BROKER_TOKEN_FILE" => Some(token.display().to_string()),
            _ => None,
        })
        .unwrap();
        (
            Arc::new(RequestContext::new(
                config,
                PublicConnector::system(crate::shutdown::ProcessResources::testing(256, 256)),
                Some(local),
                shutdown,
            )),
            local_paths,
        )
    }

    async fn read_head(stream: &mut DuplexStream) -> Vec<u8> {
        let mut bytes = Vec::new();
        while !bytes.ends_with(b"\r\n\r\n") {
            let mut byte = [0_u8; 1];
            let read = stream.read(&mut byte).await.unwrap();
            assert_ne!(read, 0, "stream closed before HTTP head completed");
            bytes.push(byte[0]);
            assert!(bytes.len() <= REQUEST_LINE_LIMIT + HEADER_SECTION_LIMIT);
        }
        bytes
    }

    fn start_public(
        context: Arc<RequestContext>,
        shutdown: Shutdown,
    ) -> (DuplexStream, tokio::task::JoinHandle<anyhow::Result<()>>) {
        let (client, server) = tokio::io::duplex(APPLICATION_BUFFER_LIMIT);
        let task = tokio::spawn(serve_test_connection(server, context, shutdown));
        (client, task)
    }

    async fn start_public_tcp(
        context: Arc<RequestContext>,
        shutdown: Shutdown,
    ) -> (TcpStream, tokio::task::JoinHandle<anyhow::Result<()>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let client = TcpStream::connect(address).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();
        let task = tokio::spawn(serve_test_connection(server, context, shutdown));
        (client, task)
    }

    async fn wait_until_set(flag: &AtomicBool, message: &str) {
        timeout(Duration::from_secs(1), async {
            while !flag.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{message}"));
    }

    async fn disconnect_pending_public_checkout(
        connector: PublicConnector,
        started: Arc<AtomicBool>,
        dropped: Arc<AtomicBool>,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let shutdown = Shutdown::new();
        let context = public_context(directory.path(), connector, &shutdown);
        let (mut client, server_task) = start_public_tcp(context, shutdown).await;
        client
            .write_all(b"GET http://allowed.example/pending HTTP/1.1\r\nHost: ignored\r\n\r\n")
            .await
            .unwrap();
        wait_until_set(&started, "public checkout never started").await;
        drop(client);
        let _ = timeout(Duration::from_secs(1), server_task)
            .await
            .expect("public checkout survived the client disconnect")
            .unwrap();
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn client_disconnect_cancels_public_resolution_and_dial() {
        let resolution_started = Arc::new(AtomicBool::new(false));
        let resolution_dropped = Arc::new(AtomicBool::new(false));
        let connector = PublicConnector::new(
            Arc::new(PendingForwardResolver {
                started: resolution_started.clone(),
                dropped: resolution_dropped.clone(),
            }),
            Arc::new(NeverDialer),
        );
        disconnect_pending_public_checkout(connector, resolution_started, resolution_dropped).await;

        let dial_started = Arc::new(AtomicBool::new(false));
        let dial_dropped = Arc::new(AtomicBool::new(false));
        let connector = PublicConnector::new(
            Arc::new(OneAnswer),
            Arc::new(PendingForwardDialer {
                started: dial_started.clone(),
                dropped: dial_dropped.clone(),
            }),
        );
        disconnect_pending_public_checkout(connector, dial_started, dial_dropped).await;
    }

    #[tokio::test]
    async fn client_disconnect_cancels_pending_broker_authentication() {
        let broker_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let connector = BrokerConnector::new(
            broker_listener.local_addr().unwrap(),
            "a".repeat(64).parse().unwrap(),
            crate::shutdown::ProcessResources::testing(256, 256),
        );
        let directory = tempfile::tempdir().unwrap();
        let shutdown = Shutdown::new();
        let (context, _) = local_context(directory.path(), connector, &shutdown);
        let (frame_seen_send, frame_seen) = oneshot::channel();
        let broker_task = tokio::spawn(async move {
            let (mut stream, _) = broker_listener.accept().await.unwrap();
            let mut frame = Vec::new();
            while !frame.ends_with(b"\n") {
                let mut byte = [0_u8; 1];
                assert_eq!(stream.read(&mut byte).await.unwrap(), 1);
                frame.push(byte[0]);
            }
            assert!(frame.starts_with(b"VHRN-BROKER/1 CONNECT "));
            frame_seen_send.send(()).unwrap();
            let mut rest = Vec::new();
            timeout(Duration::from_secs(1), stream.read_to_end(&mut rest))
                .await
                .expect("broker authentication survived the client disconnect")
                .unwrap();
            assert!(rest.is_empty());
        });
        let (mut client, server_task) = start_public_tcp(context, shutdown).await;
        client
            .write_all(b"GET http://localhost/pending HTTP/1.1\r\nHost: ignored\r\n\r\n")
            .await
            .unwrap();
        frame_seen.await.unwrap();
        drop(client);
        let _ = timeout(Duration::from_secs(1), server_task)
            .await
            .expect("broker checkout survived the client disconnect")
            .unwrap();
        broker_task.await.unwrap();
    }

    #[tokio::test]
    async fn streams_request_larger_than_former_cap_with_aggregate_buffer_bound() {
        let directory = tempfile::tempdir().unwrap();
        let shutdown = Shutdown::new();
        let (origin_proxy, mut origin) = tokio::io::duplex(APPLICATION_BUFFER_LIMIT);
        let context = public_context(
            directory.path(),
            PublicConnector::test_with_stream(origin_proxy),
            &shutdown,
        );
        let body_bytes = 8 * 1024 * 1024 + 17;
        let origin_task = tokio::spawn(async move {
            let head = read_head(&mut origin).await;
            let text = String::from_utf8(head).unwrap();
            assert!(text.starts_with("POST /upload HTTP/1.1\r\n"));
            assert!(text.contains("host: allowed.example:80\r\n"));
            assert!(text.contains(&format!("content-length: {body_bytes}\r\n")));
            assert!(text.contains("via: 1.1 vhrn\r\n"));
            let mut remaining = body_bytes;
            let mut peak = 0;
            let mut buffer = vec![0_u8; APPLICATION_BUFFER_LIMIT];
            while remaining != 0 {
                let read = origin
                    .read(&mut buffer[..remaining.min(APPLICATION_BUFFER_LIMIT)])
                    .await
                    .unwrap();
                assert_ne!(read, 0);
                peak = peak.max(read);
                remaining -= read;
            }
            assert!(peak <= APPLICATION_BUFFER_LIMIT);
            origin
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .await
                .unwrap();
        });
        let (mut client, server_task) = start_public_tcp(context, shutdown).await;
        client
            .write_all(
                format!(
                    "POST http://allowed.example:80/upload HTTP/1.1\r\nHost: conflict.invalid\r\nContent-Length: {body_bytes}\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let chunk = vec![b'x'; 32 * 1024];
        let mut remaining = body_bytes;
        while remaining != 0 {
            let count = remaining.min(chunk.len());
            client.write_all(&chunk[..count]).await.unwrap();
            remaining -= count;
        }
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        assert!(response.ends_with(b"\r\n\r\nok"));
        origin_task.await.unwrap();
        server_task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn streams_response_larger_than_former_cap_before_origin_finishes() {
        let directory = tempfile::tempdir().unwrap();
        let shutdown = Shutdown::new();
        let (origin_proxy, mut origin) = tokio::io::duplex(APPLICATION_BUFFER_LIMIT);
        let context = public_context(
            directory.path(),
            PublicConnector::test_with_stream(origin_proxy),
            &shutdown,
        );
        let body_bytes = 8 * 1024 * 1024 + 19;
        let (first_sent, first_seen) = oneshot::channel();
        let (continue_send, continue_receive) = oneshot::channel();
        let origin_task = tokio::spawn(async move {
            let _ = read_head(&mut origin).await;
            origin
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {body_bytes}\r\nConnection: close\r\n\r\na"
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            origin.flush().await.unwrap();
            first_sent.send(()).unwrap();
            continue_receive.await.unwrap();
            let chunk = vec![b'b'; 32 * 1024];
            let mut remaining = body_bytes - 1;
            while remaining != 0 {
                let count = remaining.min(chunk.len());
                origin.write_all(&chunk[..count]).await.unwrap();
                remaining -= count;
            }
        });
        let (mut client, server_task) = start_public(context, shutdown);
        client
            .write_all(b"GET http://allowed.example/large HTTP/1.1\r\nHost: ignored.invalid\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        first_seen.await.unwrap();
        let mut response = Vec::new();
        let body_start = loop {
            let mut buffer = [0_u8; 4096];
            let read = timeout(Duration::from_secs(1), client.read(&mut buffer))
                .await
                .unwrap()
                .unwrap();
            assert_ne!(read, 0);
            response.extend_from_slice(&buffer[..read]);
            if let Some(index) = response.windows(4).position(|value| value == b"\r\n\r\n")
                && response.len() > index + 4
            {
                break index + 4;
            }
        };
        assert_eq!(response[body_start], b'a');
        continue_send.send(()).unwrap();
        let mut buffer = vec![0_u8; APPLICATION_BUFFER_LIMIT];
        let mut peak = 0;
        loop {
            let read = client.read(&mut buffer).await.unwrap();
            if read == 0 {
                break;
            }
            peak = peak.max(read);
            response.extend_from_slice(&buffer[..read]);
        }
        assert!(peak <= APPLICATION_BUFFER_LIMIT);
        assert_eq!(response.len() - body_start, body_bytes);
        origin_task.await.unwrap();
        server_task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn forwards_expect_informationals_hop_filtering_via_and_trailers() {
        let directory = tempfile::tempdir().unwrap();
        let shutdown = Shutdown::new();
        let (origin_proxy, mut origin) = tokio::io::duplex(APPLICATION_BUFFER_LIMIT);
        let context = public_context(
            directory.path(),
            PublicConnector::test_with_stream(origin_proxy),
            &shutdown,
        );
        let origin_task = tokio::spawn(async move {
            let head = String::from_utf8(read_head(&mut origin).await).unwrap();
            assert!(head.starts_with("POST /expect HTTP/1.1\r\n"));
            assert!(head.contains("host: allowed.example\r\n"));
            assert!(head.contains("expect: 100-continue\r\n"));
            assert!(head.contains("via: 1.0 prior\r\n"));
            assert!(head.contains("via: 1.1 vhrn\r\n"));
            assert!(!head.contains("conflict.invalid"));
            assert!(!head.contains("x-remove"));
            assert!(!head.contains("proxy-authorization"));
            origin
                .write_all(
                    b"HTTP/1.1 103 Early Hints\r\nLink: </a>\r\n\r\nHTTP/1.1 100 Continue\r\nX-Origin-Continue: yes\r\n\r\n",
                )
                .await
                .unwrap();
            origin.flush().await.unwrap();
            let mut body = [0_u8; 4];
            origin.read_exact(&mut body).await.unwrap();
            assert_eq!(&body, b"data");
            origin
                .write_all(
                    b"HTTP/1.1 200 OK\r\nConnection: x-remove, close\r\nX-Remove: no\r\nProxy-Authenticate: Basic secret\r\nKeep-Alive: timeout=5\r\nVia: 1.0 prior\r\nTrailer: X-Checksum, X-Trace\r\nTransfer-Encoding: chunked\r\n\r\n1\r\na\r\n1\r\nb\r\n0\r\nX-Checksum: yes\r\nX-Trace: one\r\nX-Trace: two\r\n\r\n",
                )
                .await
                .unwrap();
        });
        let (mut client, server_task) = start_public(context, shutdown);
        client
            .write_all(
                b"POST http://allowed.example/expect HTTP/1.1\r\nHost: conflict.invalid\r\nConnection: X-Remove, close\r\nX-Remove: no\r\nProxy-Authorization: Basic secret\r\nVia: 1.0 prior\r\nExpect: 100-continue\r\nContent-Length: 4\r\n\r\n",
            )
            .await
            .unwrap();
        let mut informational = Vec::new();
        loop {
            let mut buffer = [0_u8; 256];
            let read = client.read(&mut buffer).await.unwrap();
            assert_ne!(read, 0);
            informational.extend_from_slice(&buffer[..read]);
            if informational
                .windows(b"x-origin-continue: yes\r\n".len())
                .any(|value| value == b"x-origin-continue: yes\r\n")
                && informational.ends_with(b"\r\n\r\n")
            {
                break;
            }
        }
        let informational = String::from_utf8(informational).unwrap();
        assert!(informational.contains("HTTP/1.1 103 Early Hints\r\n"));
        assert!(informational.contains("HTTP/1.1 100 Continue\r\n"));
        assert_eq!(informational.matches("via: 1.1 vhrn\r\n").count(), 2);
        client.write_all(b"data").await.unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8(response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(response.contains("via: 1.0 prior\r\nvia: 1.1 vhrn\r\n"));
        assert!(response.contains("transfer-encoding: chunked\r\n"));
        assert!(!response.contains("x-remove:"));
        assert!(!response.contains("proxy-authenticate:"));
        assert!(!response.contains("keep-alive:"));
        assert!(!response.contains("trailer:"));
        assert!(response.contains("1\r\na\r\n1\r\nb\r\n0\r\nx-checksum: yes\r\n"));
        assert!(response.contains("x-trace: one\r\nx-trace: two\r\n"));
        assert!(response.ends_with("\r\n\r\n"));
        origin_task.await.unwrap();
        server_task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn suppresses_origin_informationals_for_http10_clients() {
        let directory = tempfile::tempdir().unwrap();
        let shutdown = Shutdown::new();
        let (origin_proxy, mut origin) = tokio::io::duplex(APPLICATION_BUFFER_LIMIT);
        let context = public_context(
            directory.path(),
            PublicConnector::test_with_stream(origin_proxy),
            &shutdown,
        );
        let origin_task = tokio::spawn(async move {
            let _ = read_head(&mut origin).await;
            origin
                .write_all(
                    b"HTTP/1.1 103 Early Hints\r\nLink: </a>\r\n\r\nHTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                )
                .await
                .unwrap();
        });
        let (mut client, server_task) = start_public(context, shutdown);
        client
            .write_all(b"GET http://allowed.example/http10 HTTP/1.0\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8(response).unwrap();
        assert!(response.starts_with("HTTP/1.0 200 OK\r\n"));
        assert!(!response.contains("100 Continue"));
        assert!(!response.contains("103 Early Hints"));
        assert!(response.ends_with("\r\n\r\nok"));
        origin_task.await.unwrap();
        server_task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn forwards_request_trailers_and_removes_connection_nominations() {
        let directory = tempfile::tempdir().unwrap();
        let shutdown = Shutdown::new();
        let (origin_proxy, mut origin) = tokio::io::duplex(APPLICATION_BUFFER_LIMIT);
        let context = public_context(
            directory.path(),
            PublicConnector::test_with_stream(origin_proxy),
            &shutdown,
        );
        let origin_task = tokio::spawn(async move {
            let head = String::from_utf8(read_head(&mut origin).await).unwrap();
            assert!(head.starts_with("POST /trailers HTTP/1.1\r\n"));
            assert!(head.contains("transfer-encoding: chunked\r\n"));
            assert!(!head.contains("trailer:"));
            assert!(!head.contains("x-drop:"));

            let mut body = Vec::new();
            while !body.ends_with(b"0\r\nx-end: yes\r\n\r\n") {
                let mut buffer = [0_u8; 64];
                let count = origin.read(&mut buffer).await.unwrap();
                assert_ne!(count, 0);
                body.extend_from_slice(&buffer[..count]);
            }
            assert_eq!(body, b"4\r\ndata\r\n0\r\nx-end: yes\r\n\r\n");
            origin
                .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
        });
        let (mut client, server_task) = start_public(context, shutdown);
        client
            .write_all(
                b"POST http://allowed.example/trailers HTTP/1.1\r\nHost: ignored\r\nConnection: X-Drop, close\r\nX-Drop: no\r\nTransfer-Encoding: chunked\r\nTrailer: X-End, X-Drop\r\n\r\n4\r\ndata\r\n0\r\nX-End: yes\r\nX-Drop: no\r\n\r\n",
            )
            .await
            .unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        assert!(response.starts_with(b"HTTP/1.1 204 No Content\r\n"));
        assert!(response.ends_with(b"\r\n\r\n"));
        origin_task.await.unwrap();
        server_task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn forwards_a_limit_sized_request_trailer_with_bounded_state() {
        let directory = tempfile::tempdir().unwrap();
        let shutdown = Shutdown::new();
        let (origin_proxy, mut origin) = tokio::io::duplex(APPLICATION_BUFFER_LIMIT);
        let context = public_context(
            directory.path(),
            PublicConnector::test_with_stream(origin_proxy),
            &shutdown,
        );
        let prefix = b"X-Large: ";
        let value_length = HEADER_SECTION_LIMIT - prefix.len() - 4;
        let origin_task = tokio::spawn(async move {
            let _ = read_head(&mut origin).await;
            let expected_length = 3 + HEADER_SECTION_LIMIT;
            let mut body = Vec::with_capacity(expected_length);
            while body.len() != expected_length {
                let mut buffer = [0_u8; 4096];
                let count = origin.read(&mut buffer).await.unwrap();
                assert_ne!(count, 0);
                body.extend_from_slice(&buffer[..count]);
            }
            let prefix = b"0\r\nx-large: ";
            assert!(body.starts_with(prefix));
            assert!(body.ends_with(b"\r\n\r\n"));
            assert_eq!(body.len() - prefix.len() - 4, value_length);
            assert!(
                body[prefix.len()..body.len() - 4]
                    .iter()
                    .all(|byte| *byte == b'a')
            );
            origin
                .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
        });
        let (mut client, server_task) = start_public(context, shutdown);
        let mut request = b"POST http://allowed.example/trailer-limit HTTP/1.1\r\nHost: ignored\r\nConnection: close\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n".to_vec();
        request.extend_from_slice(prefix);
        request.extend(std::iter::repeat_n(b'a', value_length));
        request.extend_from_slice(b"\r\n\r\n");
        client.write_all(&request).await.unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        assert!(response.starts_with(b"HTTP/1.1 204 No Content\r\n"));
        origin_task.await.unwrap();
        server_task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn reframes_origin_transfer_codings_for_http11_and_http10_clients() {
        let directory = tempfile::tempdir().unwrap();
        let shutdown = Shutdown::new();
        let (origin_proxy, mut origin) = tokio::io::duplex(APPLICATION_BUFFER_LIMIT);
        let context = public_context(
            directory.path(),
            PublicConnector::test_with_stream(origin_proxy),
            &shutdown,
        );
        let origin_task = tokio::spawn(async move {
            let _ = read_head(&mut origin).await;
            origin
                .write_all(
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip\r\nConnection: close\r\n\r\ncoded",
                )
                .await
                .unwrap();
            origin.shutdown().await.unwrap();
        });
        let (mut client, server_task) = start_public(context, shutdown);
        client
            .write_all(b"GET http://allowed.example/gzip HTTP/1.1\r\nHost: ignored\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        let text = String::from_utf8(response).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("transfer-encoding: gzip\r\n"));
        assert!(text.contains("transfer-encoding: chunked\r\n"));
        assert!(text.contains("connection: close\r\n"));
        assert!(text.ends_with("5\r\ncoded\r\n0\r\n\r\n"));
        origin_task.await.unwrap();
        server_task.await.unwrap().unwrap();

        let directory = tempfile::tempdir().unwrap();
        let shutdown = Shutdown::new();
        let (origin_proxy, mut origin) = tokio::io::duplex(APPLICATION_BUFFER_LIMIT);
        let context = public_context(
            directory.path(),
            PublicConnector::test_with_stream(origin_proxy),
            &shutdown,
        );
        let origin_task = tokio::spawn(async move {
            let _ = read_head(&mut origin).await;
            origin
                .write_all(
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\n",
                )
                .await
                .unwrap();
            let mut byte = [0_u8; 1];
            assert_eq!(origin.read(&mut byte).await.unwrap(), 0);
        });
        let (mut client, server_task) = start_public(context, shutdown);
        client
            .write_all(b"GET http://allowed.example/chunked HTTP/1.0\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        let text = String::from_utf8(response).unwrap();
        assert!(text.starts_with("HTTP/1.0 200 OK\r\n"));
        assert!(!text.contains("transfer-encoding:"));
        assert!(text.contains("connection: close\r\n"));
        assert!(text.ends_with("\r\n\r\nabc"));
        server_task.await.unwrap().unwrap();
        origin_task.await.unwrap();
    }

    #[tokio::test]
    async fn head_204_and_304_preserve_describing_headers_without_body_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let shutdown = Shutdown::new();
        let (origin_proxy, mut origin) = tokio::io::duplex(APPLICATION_BUFFER_LIMIT);
        let context = public_context(
            directory.path(),
            PublicConnector::test_with_stream(origin_proxy),
            &shutdown,
        );
        let origin_task = tokio::spawn(async move {
            let head = String::from_utf8(read_head(&mut origin).await).unwrap();
            assert!(head.starts_with("HEAD /head HTTP/1.1\r\n"));
            origin
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nX-Case: head\r\n\r\n")
                .await
                .unwrap();

            let head = String::from_utf8(read_head(&mut origin).await).unwrap();
            assert!(head.starts_with("GET /no-content HTTP/1.1\r\n"));
            origin
                .write_all(b"HTTP/1.1 204 No Content\r\nETag: one\r\n\r\n")
                .await
                .unwrap();

            let head = String::from_utf8(read_head(&mut origin).await).unwrap();
            assert!(head.starts_with("GET /not-modified HTTP/1.1\r\n"));
            origin
                .write_all(
                    b"HTTP/1.1 304 Not Modified\r\nContent-Length: 123\r\nETag: two\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
        });
        let (mut client, server_task) = start_public(context, shutdown);

        client
            .write_all(b"HEAD http://allowed.example/head HTTP/1.1\r\nHost: ignored\r\n\r\n")
            .await
            .unwrap();
        let response = String::from_utf8(read_head(&mut client).await).unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(response.contains("content-length: 5\r\n"));
        assert!(response.contains("x-case: head\r\n"));

        client
            .write_all(b"GET http://allowed.example/no-content HTTP/1.1\r\nHost: ignored\r\n\r\n")
            .await
            .unwrap();
        let response = String::from_utf8(read_head(&mut client).await).unwrap();
        assert!(response.starts_with("HTTP/1.1 204 No Content\r\n"));
        assert!(response.contains("etag: one\r\n"));

        client
            .write_all(b"GET http://allowed.example/not-modified HTTP/1.1\r\nHost: ignored\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8(response).unwrap();
        assert!(response.starts_with("HTTP/1.1 304 Not Modified\r\n"));
        assert!(response.contains("content-length: 123\r\n"));
        assert!(response.contains("etag: two\r\n"));
        assert!(response.ends_with("\r\n\r\n"));
        origin_task.await.unwrap();
        server_task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn public_and_local_pool_reuse_remain_policy_fresh() {
        let directory = tempfile::tempdir().unwrap();
        let shutdown = Shutdown::new();
        let (origin_proxy, mut origin) = tokio::io::duplex(APPLICATION_BUFFER_LIMIT);
        let context = public_context(
            directory.path(),
            PublicConnector::test_with_stream(origin_proxy),
            &shutdown,
        );
        let public_origin = tokio::spawn(async move {
            for expected in ["/one", "/reuse"] {
                let request = String::from_utf8(read_head(&mut origin).await).unwrap();
                assert!(request.starts_with(&format!("GET {expected} HTTP/1.1\r\n")));
                origin
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                    .await
                    .unwrap();
            }
            let mut byte = [0_u8; 1];
            let read = timeout(Duration::from_millis(150), origin.read(&mut byte)).await;
            assert!(!matches!(read, Ok(Ok(count)) if count > 0));
        });
        let (mut client, server_task) = start_public(context, shutdown);
        client
            .write_all(b"GET http://allowed.example/one HTTP/1.1\r\nHost: ignored\r\n\r\n")
            .await
            .unwrap();
        let first = read_head(&mut client).await;
        assert!(first.starts_with(b"HTTP/1.1 200 OK\r\n"));
        client
            .write_all(b"GET http://allowed.example/reuse HTTP/1.1\r\nHost: ignored\r\n\r\n")
            .await
            .unwrap();
        let second = read_head(&mut client).await;
        assert!(second.starts_with(b"HTTP/1.1 200 OK\r\n"));
        std::fs::write(directory.path().join("allowlist"), "").unwrap();
        client
            .write_all(b"GET http://allowed.example/two HTTP/1.1\r\nHost: ignored\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut denied = Vec::new();
        client.read_to_end(&mut denied).await.unwrap();
        assert!(denied.starts_with(b"HTTP/1.1 403 Forbidden\r\n"));
        public_origin.await.unwrap();
        server_task.await.unwrap().unwrap();

        let local_directory = tempfile::tempdir().unwrap();
        let local_shutdown = Shutdown::new();
        let (local_proxy, mut local_origin) = tokio::io::duplex(APPLICATION_BUFFER_LIMIT);
        let connector = BrokerConnector::test_with_connect_stream(local_proxy);
        let (local_context, local_paths) =
            local_context(local_directory.path(), connector, &local_shutdown);
        let local_origin_task = tokio::spawn(async move {
            for expected in ["/one", "/reuse"] {
                let request = String::from_utf8(read_head(&mut local_origin).await).unwrap();
                assert!(request.starts_with(&format!("GET {expected} HTTP/1.1\r\n")));
                assert!(request.contains("host: localhost\r\n"));
                local_origin
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                    .await
                    .unwrap();
            }
            let mut byte = [0_u8; 1];
            let read = timeout(Duration::from_millis(150), local_origin.read(&mut byte)).await;
            assert!(!matches!(read, Ok(Ok(count)) if count > 0));
        });
        let (mut local_client, local_server_task) = start_public(local_context, local_shutdown);
        local_client
            .write_all(b"GET http://localhost/one HTTP/1.1\r\nHost: ignored\r\n\r\n")
            .await
            .unwrap();
        let first = read_head(&mut local_client).await;
        assert!(first.starts_with(b"HTTP/1.1 200 OK\r\n"));
        local_client
            .write_all(b"GET http://localhost/reuse HTTP/1.1\r\nHost: ignored\r\n\r\n")
            .await
            .unwrap();
        let second = read_head(&mut local_client).await;
        assert!(second.starts_with(b"HTTP/1.1 200 OK\r\n"));
        for path in local_paths {
            std::fs::write(path, "").unwrap();
        }
        local_client
            .write_all(
                b"GET http://localhost/two HTTP/1.1\r\nHost: ignored\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        let mut denied = Vec::new();
        local_client.read_to_end(&mut denied).await.unwrap();
        assert!(denied.starts_with(b"HTTP/1.1 403 Forbidden\r\n"));
        local_origin_task.await.unwrap();
        local_server_task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn precommit_origin_error_is_502_but_postcommit_error_only_closes() {
        let directory = tempfile::tempdir().unwrap();
        let shutdown = Shutdown::new();
        let (origin_proxy, mut origin) = tokio::io::duplex(APPLICATION_BUFFER_LIMIT);
        let context = public_context(
            directory.path(),
            PublicConnector::test_with_stream(origin_proxy),
            &shutdown,
        );
        let origin_task = tokio::spawn(async move {
            let _ = read_head(&mut origin).await;
            origin
                .write_all(b"HTTP/1.1 200 OK\r\nBad Header: value\r\n\r\n")
                .await
                .unwrap();
        });
        let (mut client, server_task) = start_public(context, shutdown);
        client
            .write_all(b"GET http://allowed.example/pre HTTP/1.1\r\nHost: ignored\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        assert!(response.starts_with(b"HTTP/1.1 502 Bad Gateway\r\n"));
        assert!(response.ends_with(b"bad gateway\n"));
        origin_task.await.unwrap();
        server_task.await.unwrap().unwrap();

        let directory = tempfile::tempdir().unwrap();
        let shutdown = Shutdown::new();
        let (origin_proxy, mut origin) = tokio::io::duplex(APPLICATION_BUFFER_LIMIT);
        let context = public_context(
            directory.path(),
            PublicConnector::test_with_stream(origin_proxy),
            &shutdown,
        );
        let origin_task = tokio::spawn(async move {
            let _ = read_head(&mut origin).await;
            origin
                .write_all(
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n10000000000000000\r\n",
                )
                .await
                .unwrap();
        });
        let (mut client, server_task) = start_public(context, shutdown);
        client
            .write_all(b"GET http://allowed.example/post HTTP/1.1\r\nHost: ignored\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
        assert!(
            !response
                .windows(b"bad gateway\n".len())
                .any(|value| value == b"bad gateway\n")
        );
        origin_task.await.unwrap();
        assert!(server_task.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn client_disconnect_cancels_pending_origin_io_without_detached_work() {
        let directory = tempfile::tempdir().unwrap();
        let shutdown = Shutdown::new();
        let (origin_proxy, mut origin) = tokio::io::duplex(APPLICATION_BUFFER_LIMIT);
        let context = public_context(
            directory.path(),
            PublicConnector::test_with_stream(origin_proxy),
            &shutdown,
        );
        let (head_seen_send, head_seen) = oneshot::channel();
        let origin_task = tokio::spawn(async move {
            let _ = read_head(&mut origin).await;
            head_seen_send.send(()).unwrap();
            let mut byte = [0_u8; 1];
            let read = timeout(Duration::from_secs(1), origin.read(&mut byte))
                .await
                .expect("origin stream was cancelled promptly")
                .unwrap();
            assert_eq!(read, 0);
        });
        let (mut client, server_task) = start_public_tcp(context, shutdown).await;
        client
            .write_all(b"GET http://allowed.example/wait HTTP/1.1\r\nHost: ignored\r\n\r\n")
            .await
            .unwrap();
        head_seen.await.unwrap();
        drop(client);
        origin_task.await.unwrap();
        let _ = timeout(Duration::from_secs(1), server_task)
            .await
            .expect("exchange task survived the client disconnect")
            .unwrap();
    }

    #[tokio::test]
    async fn client_disconnect_cancels_a_backpressured_origin_upload() {
        let directory = tempfile::tempdir().unwrap();
        let shutdown = Shutdown::new();
        let (origin_proxy, mut origin) = tokio::io::duplex(64);
        let context = public_context(
            directory.path(),
            PublicConnector::test_with_stream(origin_proxy),
            &shutdown,
        );
        let (head_seen_send, head_seen) = oneshot::channel();
        let (client_dropped_send, client_dropped) = oneshot::channel();
        let (server_done_send, server_done) = oneshot::channel();
        let body_bytes = REQUEST_BODY_FRAME_LIMIT * 2;
        let origin_task = tokio::spawn(async move {
            let _ = read_head(&mut origin).await;
            head_seen_send.send(()).unwrap();
            client_dropped.await.unwrap();
            let mut received = Vec::new();
            timeout(Duration::from_secs(1), origin.read_to_end(&mut received))
                .await
                .expect("backpressured origin stream was closed promptly")
                .unwrap();
            assert!(received.len() < body_bytes);
            server_done.await.unwrap();
            timeout(Duration::from_secs(1), origin.write_all(b"closed"))
                .await
                .expect("origin write half survived the client disconnect")
                .expect_err("origin write half remained open");
        });
        let (mut client, server_task) = start_public_tcp(context, shutdown).await;
        client
            .write_all(
                format!(
                    "POST http://allowed.example/stall HTTP/1.1\r\nHost: ignored\r\nContent-Length: {body_bytes}\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        head_seen.await.unwrap();
        client.write_all(&vec![b'x'; body_bytes]).await.unwrap();
        drop(client);
        client_dropped_send.send(()).unwrap();
        let _ = timeout(Duration::from_secs(1), server_task)
            .await
            .expect("exchange task survived the client disconnect")
            .unwrap();
        server_done_send.send(()).unwrap();
        origin_task.await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_after_response_commit_cancels_pending_origin_body_io() {
        let directory = tempfile::tempdir().unwrap();
        let shutdown = Shutdown::new();
        let (origin_proxy, mut origin) = tokio::io::duplex(APPLICATION_BUFFER_LIMIT);
        let context = public_context(
            directory.path(),
            PublicConnector::test_with_stream(origin_proxy),
            &shutdown,
        );
        let origin_task = tokio::spawn(async move {
            let _ = read_head(&mut origin).await;
            origin
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1024\r\n\r\na")
                .await
                .unwrap();
            origin.flush().await.unwrap();
            let mut byte = [0_u8; 1];
            assert_eq!(
                timeout(Duration::from_secs(1), origin.read(&mut byte))
                    .await
                    .expect("origin response halves survived shutdown")
                    .unwrap(),
                0
            );
        });
        let (mut client, server_task) = start_public(context, shutdown.clone());
        client
            .write_all(b"GET http://allowed.example/shutdown HTTP/1.1\r\nHost: ignored\r\n\r\n")
            .await
            .unwrap();
        let mut committed = Vec::new();
        loop {
            let mut buffer = [0_u8; 256];
            let count = timeout(Duration::from_secs(1), client.read(&mut buffer))
                .await
                .unwrap()
                .unwrap();
            assert_ne!(count, 0);
            committed.extend_from_slice(&buffer[..count]);
            if let Some(head_end) = committed.windows(4).position(|value| value == b"\r\n\r\n")
                && committed.len() > head_end + 4
            {
                assert_eq!(committed[head_end + 4], b'a');
                break;
            }
        }
        assert!(committed.starts_with(b"HTTP/1.1 200 OK\r\n"));
        shutdown.request();
        tokio::task::yield_now().await;
        assert!(!server_task.is_finished());
        shutdown.force();
        let mut rest = Vec::new();
        timeout(Duration::from_secs(1), client.read_to_end(&mut rest))
            .await
            .expect("downstream response survived shutdown")
            .unwrap();
        assert!(
            !rest
                .windows(b"bad gateway\n".len())
                .any(|window| window == b"bad gateway\n")
        );
        timeout(Duration::from_secs(1), server_task)
            .await
            .expect("forwarding task survived shutdown")
            .unwrap()
            .unwrap_err();
        origin_task.await.unwrap();
    }

    struct OneAnswer;
    impl Resolver for OneAnswer {
        fn resolve(&self, _: String, _: u16) -> ResolveFuture {
            Box::pin(async { Ok(vec![ResolvedAddress::unscoped("8.8.8.8".parse().unwrap())]) })
        }
    }

    struct QueueDialer {
        calls: Arc<AtomicUsize>,
        streams: Mutex<VecDeque<PublicStream>>,
    }
    impl NumericDialer for QueueDialer {
        fn dial(&self, _: SocketAddr) -> DialFuture {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let stream = self.streams.lock().unwrap().pop_front();
            Box::pin(async move { stream.ok_or_else(|| anyhow::anyhow!("no test stream")) })
        }
    }

    fn origin_response_with_header_section(length: usize) -> Vec<u8> {
        let status = b"HTTP/1.1 200 OK\r\n";
        let fixed = b"Connection: close\r\nX: ";
        let suffix = b"\r\n\r\n";
        let value_length = length - fixed.len() - suffix.len();
        let mut response = status.to_vec();
        response.extend_from_slice(fixed);
        response.extend(std::iter::repeat_n(b'a', value_length));
        response.extend_from_slice(suffix);
        assert_eq!(response.len() - status.len(), length);
        response
    }

    #[tokio::test]
    async fn origin_header_limit_is_enforced_before_downstream_commit() {
        let directory = tempfile::tempdir().unwrap();
        let shutdown = Shutdown::new();
        let (first_proxy, mut first_origin) = tokio::io::duplex(HEADER_SECTION_LIMIT * 2);
        let (second_proxy, mut second_origin) = tokio::io::duplex(HEADER_SECTION_LIMIT * 2);
        let calls = Arc::new(AtomicUsize::new(0));
        let public = PublicConnector::new(
            Arc::new(OneAnswer),
            Arc::new(QueueDialer {
                calls: calls.clone(),
                streams: Mutex::new(
                    [
                        PublicStream::Test(first_proxy),
                        PublicStream::Test(second_proxy),
                    ]
                    .into(),
                ),
            }),
        );
        let context = public_context(directory.path(), public, &shutdown);
        let first_task = tokio::spawn(async move {
            let _ = read_head(&mut first_origin).await;
            first_origin
                .write_all(&origin_response_with_header_section(HEADER_SECTION_LIMIT))
                .await
                .unwrap();
        });
        let (mut client, server_task) = start_public(context.clone(), shutdown.clone());
        client
            .write_all(b"GET http://allowed.example/exact HTTP/1.1\r\nHost: ignored\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
        first_task.await.unwrap();
        server_task.await.unwrap().unwrap();

        let second_task = tokio::spawn(async move {
            let _ = read_head(&mut second_origin).await;
            second_origin
                .write_all(&origin_response_with_header_section(
                    HEADER_SECTION_LIMIT + 1,
                ))
                .await
                .unwrap();
        });
        let (mut client, server_task) = start_public(context, shutdown);
        client
            .write_all(b"GET http://allowed.example/too-large HTTP/1.1\r\nHost: ignored\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        assert!(response.starts_with(b"HTTP/1.1 502 Bad Gateway\r\n"));
        assert!(
            !response
                .windows(32)
                .any(|window| window.iter().all(|byte| *byte == b'a'))
        );
        second_task.await.unwrap();
        server_task.await.unwrap().unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn origin_head_errors_and_connection_close_never_reuse_streams() {
        let directory = tempfile::tempdir().unwrap();
        let shutdown = Shutdown::new();
        let (first_proxy, mut first_origin) = tokio::io::duplex(APPLICATION_BUFFER_LIMIT);
        let (second_proxy, mut second_origin) = tokio::io::duplex(APPLICATION_BUFFER_LIMIT);
        let (third_proxy, mut third_origin) = tokio::io::duplex(APPLICATION_BUFFER_LIMIT);
        let calls = Arc::new(AtomicUsize::new(0));
        let public = PublicConnector::new(
            Arc::new(OneAnswer),
            Arc::new(QueueDialer {
                calls: calls.clone(),
                streams: Mutex::new(
                    [
                        PublicStream::Test(first_proxy),
                        PublicStream::Test(second_proxy),
                        PublicStream::Test(third_proxy),
                    ]
                    .into(),
                ),
            }),
        );
        let context = public_context(directory.path(), public, &shutdown);
        let first_task = tokio::spawn(async move {
            let _ = read_head(&mut first_origin).await;
            first_origin
                .write_all(b"HTTP/1.1 200 OK\r\nBad Header: no\r\n\r\n")
                .await
                .unwrap();
        });
        let second_task = tokio::spawn(async move {
            let request = String::from_utf8(read_head(&mut second_origin).await).unwrap();
            assert!(request.starts_with("GET /close HTTP/1.1\r\n"));
            second_origin
                .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
        });
        let (mut client, server_task) = start_public(context, shutdown);
        client
            .write_all(b"GET http://allowed.example/invalid HTTP/1.1\r\nHost: ignored\r\n\r\n")
            .await
            .unwrap();
        let first_response = read_head(&mut client).await;
        assert!(first_response.starts_with(b"HTTP/1.1 502 Bad Gateway\r\n"));
        let mut error_body = [0_u8; 12];
        client.read_exact(&mut error_body).await.unwrap();
        assert_eq!(&error_body, b"bad gateway\n");
        client
            .write_all(b"GET http://allowed.example/close HTTP/1.1\r\nHost: ignored\r\n\r\n")
            .await
            .unwrap();
        let response = read_head(&mut client).await;
        assert!(response.starts_with(b"HTTP/1.1 204 No Content\r\n"));
        let third_task = tokio::spawn(async move {
            let request = String::from_utf8(read_head(&mut third_origin).await).unwrap();
            assert!(request.starts_with("GET /new HTTP/1.1\r\n"));
            third_origin
                .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
        });
        client
            .write_all(b"GET http://allowed.example/new HTTP/1.1\r\nHost: ignored\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        assert!(response.starts_with(b"HTTP/1.1 204 No Content\r\n"));
        first_task.await.unwrap();
        second_task.await.unwrap();
        third_task.await.unwrap();
        server_task.await.unwrap().unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn post_origin_failure_is_not_automatically_retried() {
        let directory = tempfile::tempdir().unwrap();
        let shutdown = Shutdown::new();
        let (first_proxy, mut first_origin) = tokio::io::duplex(APPLICATION_BUFFER_LIMIT);
        let (second_proxy, _second_origin) = tokio::io::duplex(APPLICATION_BUFFER_LIMIT);
        let calls = Arc::new(AtomicUsize::new(0));
        let public = PublicConnector::new(
            Arc::new(OneAnswer),
            Arc::new(QueueDialer {
                calls: calls.clone(),
                streams: Mutex::new(
                    [
                        PublicStream::Test(first_proxy),
                        PublicStream::Test(second_proxy),
                    ]
                    .into(),
                ),
            }),
        );
        let context = public_context(directory.path(), public, &shutdown);
        let origin_task = tokio::spawn(async move {
            let _ = read_head(&mut first_origin).await;
            let mut body = [0_u8; 4];
            first_origin.read_exact(&mut body).await.unwrap();
            assert_eq!(&body, b"data");
        });
        let (mut client, server_task) = start_public(context, shutdown);
        client
            .write_all(b"POST http://allowed.example/fail HTTP/1.1\r\nHost: ignored\r\nContent-Length: 4\r\nConnection: close\r\n\r\ndata")
            .await
            .unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        assert!(response.starts_with(b"HTTP/1.1 502 Bad Gateway\r\n"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        origin_task.await.unwrap();
        server_task.await.unwrap().unwrap();
    }
}
