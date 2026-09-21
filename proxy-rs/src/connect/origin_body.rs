//! Streaming origin HTTP/1 parsing and checked-out connection ownership.

use std::future::Future;
use std::hash::Hash;
use std::pin::Pin;
use std::task::{Context, Poll};

use anyhow::Result;
use bytes::Bytes;
use http_body_util::combinators::UnsyncBoxBody;
use hyper::body::{Body, Frame};
use hyper::{HeaderMap, Method};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadHalf, WriteHalf};

use crate::Shutdown;
use crate::connect::pool::IdlePool;
use crate::headers::sanitize_hop_by_hop;
use crate::server::http1::{
    APPLICATION_BUFFER_LIMIT, HEADER_SECTION_LIMIT, OriginBodyFraming, OriginError,
    OriginResponseHead, REQUEST_LINE_LIMIT, RESPONSE_BODY_FRAME_LIMIT, append_field_line,
    find_crlf, find_head_end, parse_chunk_size, parse_origin_response_head, validate_line_endings,
};

#[cfg(test)]
use crate::server::http1::header_field_bytes;

const ORIGIN_BUFFER_LIMIT: usize = REQUEST_LINE_LIMIT + HEADER_SECTION_LIMIT;

/// An origin response whose body retains the checked-out connection.
pub(crate) struct OriginResponse<B> {
    pub(crate) status: hyper::StatusCode,
    pub(crate) headers: HeaderMap,
    pub(crate) body: B,
}

pub(crate) type OriginBody = UnsyncBoxBody<Bytes, anyhow::Error>;
pub(crate) type SharedOriginResponse = OriginResponse<OriginBody>;

/// A raw checked origin stream plus bytes already read from it.
pub(crate) struct HttpOrigin<S> {
    io: S,
    buffer: Box<[u8]>,
    start: usize,
    end: usize,
    #[cfg(test)]
    peak_body_buffered: usize,
}

impl<S> HttpOrigin<S> {
    pub(crate) fn new(io: S) -> Self {
        Self {
            io,
            buffer: vec![0; ORIGIN_BUFFER_LIMIT].into_boxed_slice(),
            start: 0,
            end: 0,
            #[cfg(test)]
            peak_body_buffered: 0,
        }
    }

    pub(crate) fn split(self) -> (OriginReader<ReadHalf<S>>, WriteHalf<S>)
    where
        S: AsyncRead + AsyncWrite,
    {
        let (read, write) = tokio::io::split(self.io);
        (
            OriginReader {
                io: read,
                buffer: self.buffer,
                start: self.start,
                end: self.end,
                #[cfg(test)]
                peak_body_buffered: self.peak_body_buffered,
            },
            write,
        )
    }
}

pub(crate) struct OriginReader<R> {
    io: R,
    buffer: Box<[u8]>,
    start: usize,
    end: usize,
    #[cfg(test)]
    peak_body_buffered: usize,
}

impl<S: Unpin> OriginReader<ReadHalf<S>> {
    fn reunite(self, write: WriteHalf<S>) -> HttpOrigin<S> {
        HttpOrigin {
            io: self.io.unsplit(write),
            buffer: self.buffer,
            start: self.start,
            end: self.end,
            #[cfg(test)]
            peak_body_buffered: self.peak_body_buffered,
        }
    }
}

impl<R> OriginReader<R>
where
    R: AsyncRead + Unpin,
{
    pub(crate) async fn read_response_head(
        &mut self,
        method: &Method,
    ) -> Result<OriginResponseHead, OriginError> {
        self.compact();
        let message_start = self.start;
        let status_end = loop {
            let available = &self.buffer[message_start..self.end];
            let line_end = find_crlf(available);
            let validated_end = line_end.map_or(available.len(), |end| end + 2);
            validate_line_endings(&available[..validated_end]).map_err(|_| OriginError::Invalid)?;
            if let Some(end) = line_end {
                let end = message_start + end + 2;
                if end - message_start > REQUEST_LINE_LIMIT {
                    return Err(OriginError::Invalid);
                }
                break end;
            }
            if available.len() >= REQUEST_LINE_LIMIT {
                return Err(OriginError::Invalid);
            }
            if !self
                .read_more_until(message_start + REQUEST_LINE_LIMIT)
                .await?
            {
                return Err(OriginError::Incomplete);
            }
        };

        let head_end = loop {
            let available = &self.buffer[message_start..self.end];
            let found = find_head_end(available, status_end - message_start);
            let validated_end = found.unwrap_or(available.len());
            validate_line_endings(&available[..validated_end]).map_err(|_| OriginError::Invalid)?;
            if let Some(end) = found {
                let end = message_start + end;
                if end - status_end > HEADER_SECTION_LIMIT {
                    return Err(OriginError::HeadersTooLarge);
                }
                break end;
            }
            if self.end - status_end >= HEADER_SECTION_LIMIT {
                return Err(OriginError::HeadersTooLarge);
            }
            if !self
                .read_more_until(status_end + HEADER_SECTION_LIMIT)
                .await?
            {
                return Err(OriginError::Incomplete);
            }
        };

        let head = parse_origin_response_head(
            &self.buffer[message_start..status_end],
            &self.buffer[status_end..head_end],
            method,
        )?;
        self.start = head_end;
        Ok(head)
    }

    async fn read_data(&mut self, maximum: usize) -> Result<Option<Bytes>, OriginError> {
        let maximum = maximum.min(RESPONSE_BODY_FRAME_LIMIT);
        if self.start != self.end {
            let count = maximum
                .min(APPLICATION_BUFFER_LIMIT)
                .min(self.end - self.start);
            let end = self.start + count;
            let data = Bytes::copy_from_slice(&self.buffer[self.start..end]);
            self.start = end;
            #[cfg(test)]
            self.observe_application_buffer(data.len());
            return Ok(Some(data));
        }
        self.compact();
        let mut data = vec![0; maximum.min(APPLICATION_BUFFER_LIMIT)];
        let read = self
            .io
            .read(&mut data)
            .await
            .map_err(|_| OriginError::Incomplete)?;
        if read == 0 {
            return Ok(None);
        }
        data.truncate(read);
        #[cfg(test)]
        self.observe_application_buffer(read);
        Ok(Some(Bytes::from(data)))
    }

    async fn read_exact_delimiter(&mut self) -> Result<(), OriginError> {
        while self.end - self.start < 2 {
            self.compact();
            let read = self
                .io
                .read(&mut self.buffer[self.end..2])
                .await
                .map_err(|_| OriginError::Incomplete)?;
            if read == 0 {
                return Err(OriginError::Incomplete);
            }
            self.end += read;
        }
        if &self.buffer[self.start..self.start + 2] != b"\r\n" {
            return Err(OriginError::Invalid);
        }
        self.start += 2;
        Ok(())
    }

    async fn read_line(&mut self, limit: usize) -> Result<Vec<u8>, OriginError> {
        loop {
            let available = &self.buffer[self.start..self.end];
            let line_end = find_crlf(available);
            let validated_end = line_end.map_or(available.len(), |end| end + 2);
            validate_line_endings(&available[..validated_end]).map_err(|_| OriginError::Invalid)?;
            if let Some(end) = line_end {
                if end + 2 > limit {
                    return Err(OriginError::Invalid);
                }
                let line = available[..end].to_vec();
                self.start += end + 2;
                #[cfg(test)]
                self.observe_application_buffer(line.len());
                return Ok(line);
            }
            if available.len() >= limit {
                return Err(OriginError::Invalid);
            }
            self.compact();
            if !self.read_more_until(limit).await? {
                return Err(OriginError::Incomplete);
            }
        }
    }

    async fn read_trailers(&mut self) -> Result<HeaderMap, OriginError> {
        let mut headers = HeaderMap::new();
        let mut raw_octets = 0_usize;
        loop {
            let available = &self.buffer[self.start..self.end];
            let line_end = find_crlf(available);
            let validated_end = line_end.map_or(available.len(), |end| end + 2);
            validate_line_endings(&available[..validated_end]).map_err(|_| OriginError::Invalid)?;
            if let Some(relative_end) = line_end {
                raw_octets = raw_octets
                    .checked_add(relative_end + 2)
                    .filter(|total| *total <= HEADER_SECTION_LIMIT)
                    .ok_or(OriginError::HeadersTooLarge)?;
                let line_start = self.start;
                let line_end = line_start + relative_end;
                self.start = line_end + 2;
                if relative_end == 0 {
                    validate_origin_trailers(&headers)?;
                    sanitize_hop_by_hop(&mut headers).map_err(|_| OriginError::Invalid)?;
                    #[cfg(test)]
                    self.observe_application_buffer(header_field_bytes(&headers));
                    return Ok(headers);
                }
                append_field_line(&mut headers, &self.buffer[line_start..line_end])
                    .map_err(|_| OriginError::Invalid)?;
                #[cfg(test)]
                self.observe_application_buffer(header_field_bytes(&headers));
                continue;
            }
            if raw_octets.saturating_add(available.len()) >= HEADER_SECTION_LIMIT {
                return Err(OriginError::HeadersTooLarge);
            }
            let remaining = HEADER_SECTION_LIMIT - raw_octets;
            self.compact();
            if !self.read_more_until(remaining).await? {
                return Err(OriginError::Incomplete);
            }
        }
    }

    fn compact(&mut self) {
        if self.start == self.end {
            self.start = 0;
            self.end = 0;
        } else if self.start != 0 {
            self.buffer.copy_within(self.start..self.end, 0);
            self.end -= self.start;
            self.start = 0;
        }
    }

    async fn read_more_until(&mut self, limit: usize) -> Result<bool, OriginError> {
        if self.end >= limit {
            return Ok(false);
        }
        // Limit syntax read-ahead; any retained body prefix is consumed before
        // the next zero-copy 64 KiB body read.
        let read_end = self
            .end
            .saturating_add(REQUEST_LINE_LIMIT)
            .min(limit)
            .min(self.buffer.len());
        let read = self
            .io
            .read(&mut self.buffer[self.end..read_end])
            .await
            .map_err(|_| OriginError::Incomplete)?;
        self.end += read;
        Ok(read != 0)
    }

    #[cfg(test)]
    const fn peak_body_buffered(&self) -> usize {
        self.peak_body_buffered
    }

    #[cfg(test)]
    fn observe_application_buffer(&mut self, owned: usize) {
        let aggregate = owned + (self.end - self.start);
        assert!(aggregate <= APPLICATION_BUFFER_LIMIT);
        self.peak_body_buffered = self.peak_body_buffered.max(aggregate);
    }
}

fn validate_origin_trailers(trailers: &HeaderMap) -> Result<(), OriginError> {
    const FORBIDDEN: [&str; 12] = [
        "connection",
        "content-length",
        "host",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "proxy-connection",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
        "via",
    ];
    if trailers
        .keys()
        .any(|name| FORBIDDEN.contains(&name.as_str()))
    {
        return Err(OriginError::Invalid);
    }
    Ok(())
}

enum DecoderState {
    Complete,
    Fixed { remaining: u64 },
    ChunkSize,
    ChunkData { remaining: u64 },
    ChunkDataEnd,
    Trailers,
    CloseDelimited,
}

struct OriginDecoder {
    state: DecoderState,
    trailer_nominations: Vec<hyper::header::HeaderName>,
}

impl OriginDecoder {
    fn new(
        framing: OriginBodyFraming,
        trailer_nominations: Vec<hyper::header::HeaderName>,
    ) -> Self {
        let state = match framing {
            OriginBodyFraming::None => DecoderState::Complete,
            OriginBodyFraming::ContentLength(remaining) => DecoderState::Fixed { remaining },
            OriginBodyFraming::Chunked => DecoderState::ChunkSize,
            OriginBodyFraming::CloseDelimited => DecoderState::CloseDelimited,
        };
        Self {
            state,
            trailer_nominations,
        }
    }

    async fn next_frame<R>(
        &mut self,
        reader: &mut OriginReader<R>,
    ) -> Result<Option<Frame<Bytes>>, OriginError>
    where
        R: AsyncRead + Unpin,
    {
        loop {
            match self.state {
                DecoderState::Complete => return Ok(None),
                DecoderState::Fixed { remaining: 0 } => self.state = DecoderState::Complete,
                DecoderState::Fixed { remaining } => {
                    let maximum = usize::try_from(remaining)
                        .unwrap_or(usize::MAX)
                        .min(APPLICATION_BUFFER_LIMIT);
                    let Some(data) = reader.read_data(maximum).await? else {
                        return Err(OriginError::Incomplete);
                    };
                    self.state = DecoderState::Fixed {
                        remaining: remaining - data.len() as u64,
                    };
                    return Ok(Some(Frame::data(data)));
                }
                DecoderState::ChunkSize => {
                    let line = reader.read_line(REQUEST_LINE_LIMIT).await?;
                    let size = parse_chunk_size(&line).map_err(|_| OriginError::Invalid)?;
                    self.state = if size == 0 {
                        DecoderState::Trailers
                    } else {
                        DecoderState::ChunkData { remaining: size }
                    };
                }
                DecoderState::ChunkData { remaining } => {
                    let maximum = usize::try_from(remaining)
                        .unwrap_or(usize::MAX)
                        .min(APPLICATION_BUFFER_LIMIT);
                    let Some(data) = reader.read_data(maximum).await? else {
                        return Err(OriginError::Incomplete);
                    };
                    let remaining = remaining - data.len() as u64;
                    self.state = if remaining == 0 {
                        DecoderState::ChunkDataEnd
                    } else {
                        DecoderState::ChunkData { remaining }
                    };
                    return Ok(Some(Frame::data(data)));
                }
                DecoderState::ChunkDataEnd => {
                    reader.read_exact_delimiter().await?;
                    self.state = DecoderState::ChunkSize;
                }
                DecoderState::Trailers => {
                    let mut trailers = reader.read_trailers().await?;
                    for name in &self.trailer_nominations {
                        trailers.remove(name);
                    }
                    self.state = DecoderState::Complete;
                    if trailers.is_empty() {
                        return Ok(None);
                    }
                    #[cfg(test)]
                    reader.observe_application_buffer(header_field_bytes(&trailers));
                    return Ok(Some(Frame::trailers(trailers)));
                }
                DecoderState::CloseDelimited => {
                    let Some(data) = reader.read_data(APPLICATION_BUFFER_LIMIT).await? else {
                        self.state = DecoderState::Complete;
                        return Ok(None);
                    };
                    return Ok(Some(Frame::data(data)));
                }
            }
        }
    }
}

fn origin_reusable<S>(origin: &HttpOrigin<S>) -> bool {
    origin.start == origin.end
}

pub(crate) struct OriginLease<K, S> {
    origin: HttpOrigin<S>,
    key: K,
    pool: IdlePool<K, HttpOrigin<S>>,
}

type LeaseParts<K, S> = (
    OriginReader<ReadHalf<S>>,
    WriteHalf<S>,
    K,
    IdlePool<K, HttpOrigin<S>>,
);

impl<K, S> OriginLease<K, S> {
    pub(crate) fn new(origin: HttpOrigin<S>, key: K, pool: IdlePool<K, HttpOrigin<S>>) -> Self {
        Self { origin, key, pool }
    }

    pub(crate) fn into_parts(self) -> LeaseParts<K, S>
    where
        S: AsyncRead + AsyncWrite,
    {
        let (reader, writer) = self.origin.split();
        (reader, writer, self.key, self.pool)
    }
}

pub(crate) fn take_origin<K, S>(pool: &IdlePool<K, HttpOrigin<S>>, key: &K) -> Option<HttpOrigin<S>>
where
    K: Clone + Eq + Hash + Send + 'static,
    S: Send + 'static,
{
    pool.take_if_reusable(key, origin_reusable::<S>)
}

struct OriginBodyState<K, S> {
    reader: Option<OriginReader<ReadHalf<S>>>,
    writer: Option<WriteHalf<S>>,
    decoder: OriginDecoder,
    key: K,
    pool: IdlePool<K, HttpOrigin<S>>,
    poolable: bool,
    shutdown: Shutdown,
}

impl<K, S> OriginBodyState<K, S>
where
    K: Clone + Eq + Hash + Send + 'static,
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    async fn advance(mut self) -> (Option<Self>, Result<Option<Frame<Bytes>>>) {
        let result = tokio::select! {
            biased;
            () = self.shutdown.cancelled() => {
                return (None, Err(anyhow::anyhow!("origin response cancelled")));
            }
            result = self.decoder.next_frame(
                self.reader.as_mut().expect("origin reader is present")
            ) => result,
        };
        match result {
            Ok(Some(frame)) => (Some(self), Ok(Some(frame))),
            Ok(None) => {
                self.finish();
                (None, Ok(None))
            }
            Err(error) => (None, Err(anyhow::Error::new(error))),
        }
    }

    fn finish(&mut self) {
        let Some(reader) = self.reader.take() else {
            return;
        };
        let writer = self.writer.take().expect("origin writer is present");
        let origin = reader.reunite(writer);
        if self.poolable {
            self.pool
                .put_if_reusable(self.key.clone(), origin, origin_reusable::<S>);
        }
    }
}

type BodyFuture<K, S> = Pin<
    Box<dyn Future<Output = (Option<OriginBodyState<K, S>>, Result<Option<Frame<Bytes>>>)> + Send>,
>;

enum StreamingState<K, S> {
    Ready(OriginBodyState<K, S>),
    Reading(BodyFuture<K, S>),
    Done,
}

pub(crate) struct StreamingOriginBody<K, S> {
    state: StreamingState<K, S>,
}

pub(crate) struct OriginBodyConfig {
    pub(crate) framing: OriginBodyFraming,
    pub(crate) trailer_nominations: Vec<hyper::header::HeaderName>,
    pub(crate) poolable: bool,
    pub(crate) shutdown: Shutdown,
}

impl<K, S> StreamingOriginBody<K, S> {
    pub(crate) fn new(
        reader: OriginReader<ReadHalf<S>>,
        writer: WriteHalf<S>,
        key: K,
        pool: IdlePool<K, HttpOrigin<S>>,
        config: OriginBodyConfig,
    ) -> Self
    where
        K: Clone + Eq + Hash + Send + 'static,
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        if config.framing == OriginBodyFraming::None {
            let origin = reader.reunite(writer);
            if config.poolable {
                pool.put_if_reusable(key, origin, origin_reusable::<S>);
            }
            return Self {
                state: StreamingState::Done,
            };
        }
        Self {
            state: StreamingState::Ready(OriginBodyState {
                reader: Some(reader),
                writer: Some(writer),
                decoder: OriginDecoder::new(config.framing, config.trailer_nominations),
                key,
                pool,
                poolable: config.poolable,
                shutdown: config.shutdown,
            }),
        }
    }
}

impl<K, S> Body for StreamingOriginBody<K, S>
where
    K: Clone + Eq + Hash + Send + Unpin + 'static,
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Data = Bytes;
    type Error = anyhow::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        loop {
            match std::mem::replace(&mut this.state, StreamingState::Done) {
                StreamingState::Ready(state) => {
                    this.state = StreamingState::Reading(Box::pin(state.advance()));
                }
                StreamingState::Reading(mut future) => match future.as_mut().poll(context) {
                    Poll::Pending => {
                        this.state = StreamingState::Reading(future);
                        return Poll::Pending;
                    }
                    Poll::Ready((state, Ok(Some(frame)))) => {
                        this.state = StreamingState::Ready(state.expect("frame retains state"));
                        return Poll::Ready(Some(Ok(frame)));
                    }
                    Poll::Ready((_, Ok(None))) => return Poll::Ready(None),
                    Poll::Ready((_, Err(error))) => return Poll::Ready(Some(Err(error))),
                },
                StreamingState::Done => return Poll::Ready(None),
            }
        }
    }
}

impl std::fmt::Display for OriginError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Invalid => "invalid origin response",
            Self::HeadersTooLarge => "origin response headers too large",
            Self::Incomplete => "incomplete origin response",
        })
    }
}

impl std::error::Error for OriginError {}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt as _;
    use hyper::StatusCode;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::time::{Duration, timeout};

    fn response_reader(
        bytes: Vec<u8>,
    ) -> (
        OriginReader<ReadHalf<tokio::io::DuplexStream>>,
        WriteHalf<tokio::io::DuplexStream>,
    ) {
        let (proxy, mut origin) = tokio::io::duplex(ORIGIN_BUFFER_LIMIT * 2);
        tokio::spawn(async move {
            origin.write_all(&bytes).await.unwrap();
        });
        HttpOrigin::new(proxy).split()
    }

    #[tokio::test]
    async fn origin_header_section_raw_limit_is_exact_without_overallocation() {
        for (length, expected) in [
            (HEADER_SECTION_LIMIT, Ok(StatusCode::OK)),
            (HEADER_SECTION_LIMIT + 1, Err(OriginError::HeadersTooLarge)),
        ] {
            let value_length = length - 7;
            let mut response = b"HTTP/1.1 200 OK\r\nX: ".to_vec();
            response.extend(std::iter::repeat_n(b'a', value_length));
            response.extend_from_slice(b"\r\n\r\n");
            let (mut reader, _writer) = response_reader(response);
            let result = reader
                .read_response_head(&Method::GET)
                .await
                .map(|head| head.status);
            assert_eq!(result, expected, "header section length {length}");
        }
    }

    #[tokio::test]
    async fn origin_chunk_overflow_and_truncation_are_terminal_framing_errors() {
        let response =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n10000000000000000\r\n".to_vec();
        let (mut reader, _writer) = response_reader(response);
        let head = reader.read_response_head(&Method::GET).await.unwrap();
        let mut decoder = OriginDecoder::new(head.framing, Vec::new());
        assert_eq!(
            decoder.next_frame(&mut reader).await.unwrap_err(),
            OriginError::Invalid
        );

        let response = b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nabc".to_vec();
        let (mut reader, _writer) = response_reader(response);
        let head = reader.read_response_head(&Method::GET).await.unwrap();
        let mut decoder = OriginDecoder::new(head.framing, Vec::new());
        assert_eq!(
            decoder
                .next_frame(&mut reader)
                .await
                .unwrap()
                .unwrap()
                .into_data()
                .unwrap(),
            "abc"
        );
        assert_eq!(
            decoder.next_frame(&mut reader).await.unwrap_err(),
            OriginError::Incomplete
        );
    }

    #[tokio::test]
    async fn origin_many_tiny_chunks_preserve_trailers_and_bound_each_data_frame() {
        let count = 10_000;
        let mut response = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
        for _ in 0..count {
            response.extend_from_slice(b"1\r\nx\r\n");
        }
        response.extend_from_slice(b"0\r\nX-Checksum: yes\r\n\r\n");
        let (mut reader, _writer) = response_reader(response);
        let head = reader.read_response_head(&Method::GET).await.unwrap();
        let mut decoder = OriginDecoder::new(head.framing, Vec::new());
        let mut received = 0;
        let mut trailers = None;
        while let Some(frame) = decoder.next_frame(&mut reader).await.unwrap() {
            match frame.into_data() {
                Ok(data) => {
                    assert!(data.len() <= RESPONSE_BODY_FRAME_LIMIT);
                    received += data.len();
                }
                Err(frame) => trailers = frame.into_trailers().ok(),
            }
        }
        assert_eq!(received, count);
        assert_eq!(trailers.unwrap()["x-checksum"], "yes");
        assert!(reader.peak_body_buffered() <= APPLICATION_BUFFER_LIMIT);
    }

    #[tokio::test]
    async fn response_frames_reserve_space_for_downstream_chunk_framing() {
        let length = RESPONSE_BODY_FRAME_LIMIT * 2;
        let mut response =
            format!("HTTP/1.1 200 OK\r\nContent-Length: {length}\r\n\r\n").into_bytes();
        response.extend(std::iter::repeat_n(b'x', length));
        let (mut reader, _writer) = response_reader(response);
        let head = reader.read_response_head(&Method::GET).await.unwrap();
        let mut decoder = OriginDecoder::new(head.framing, Vec::new());
        let mut maximum = 0;
        let mut received = 0;
        while let Some(frame) = decoder.next_frame(&mut reader).await.unwrap() {
            let data = frame.into_data().unwrap();
            maximum = maximum.max(data.len());
            received += data.len();
        }
        assert_eq!(received, length);
        assert_eq!(maximum, RESPONSE_BODY_FRAME_LIMIT);
        assert!(reader.peak_body_buffered() <= APPLICATION_BUFFER_LIMIT);
    }

    #[tokio::test]
    async fn origin_trailer_limit_stays_within_the_aggregate_buffer_budget() {
        let prefix = b"X-Large: ";
        let value_length = HEADER_SECTION_LIMIT - prefix.len() - 4;
        let mut response = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n".to_vec();
        response.extend_from_slice(prefix);
        response.extend(std::iter::repeat_n(b'a', value_length));
        response.extend_from_slice(b"\r\n\r\n");
        let (mut reader, _writer) = response_reader(response);
        let head = reader.read_response_head(&Method::GET).await.unwrap();
        let mut decoder = OriginDecoder::new(head.framing, Vec::new());
        let trailers = decoder
            .next_frame(&mut reader)
            .await
            .unwrap()
            .unwrap()
            .into_trailers()
            .unwrap();
        assert_eq!(trailers["x-large"].as_bytes().len(), value_length);
        assert!(reader.peak_body_buffered() <= APPLICATION_BUFFER_LIMIT);
    }

    async fn streaming_body(
        framing: OriginBodyFraming,
        bytes: &[u8],
        poolable: bool,
    ) -> (
        StreamingOriginBody<&'static str, tokio::io::DuplexStream>,
        IdlePool<&'static str, HttpOrigin<tokio::io::DuplexStream>>,
        tokio::io::DuplexStream,
    ) {
        let (proxy, mut origin) = tokio::io::duplex(APPLICATION_BUFFER_LIMIT);
        origin.write_all(bytes).await.unwrap();
        let pool = IdlePool::new();
        let (reader, writer) = HttpOrigin::new(proxy).split();
        let body = StreamingOriginBody::new(
            reader,
            writer,
            "origin",
            pool.clone(),
            OriginBodyConfig {
                framing,
                trailer_nominations: Vec::new(),
                poolable,
                shutdown: Shutdown::new(),
            },
        );
        (body, pool, origin)
    }

    #[tokio::test]
    async fn pool_returns_only_clean_complete_reusable_responses() {
        let (body, pool, _origin) =
            streaming_body(OriginBodyFraming::ContentLength(3), b"abc", true).await;
        assert_eq!(body.collect().await.unwrap().to_bytes(), "abc");
        assert_eq!(pool.len(), 1);
        drop(take_origin(&pool, &"origin"));

        let response = b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nabcHTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n".to_vec();
        let (mut reader, writer) = response_reader(response);
        let head = reader.read_response_head(&Method::GET).await.unwrap();
        let pool = IdlePool::new();
        let body = StreamingOriginBody::new(
            reader,
            writer,
            "origin",
            pool.clone(),
            OriginBodyConfig {
                framing: head.framing,
                trailer_nominations: Vec::new(),
                poolable: true,
                shutdown: Shutdown::new(),
            },
        );
        assert_eq!(body.collect().await.unwrap().to_bytes(), "abc");
        assert_eq!(pool.len(), 0, "buffered surplus response was pooled");

        let response =
            b"HTTP/1.1 204 No Content\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n".to_vec();
        let (mut reader, writer) = response_reader(response);
        let head = reader.read_response_head(&Method::GET).await.unwrap();
        let pool = IdlePool::new();
        let _body = StreamingOriginBody::new(
            reader,
            writer,
            "origin",
            pool.clone(),
            OriginBodyConfig {
                framing: head.framing,
                trailer_nominations: Vec::new(),
                poolable: true,
                shutdown: Shutdown::new(),
            },
        );
        assert_eq!(pool.len(), 0, "bodyless surplus response was pooled");

        let (body, pool, mut origin) =
            streaming_body(OriginBodyFraming::ContentLength(4), b"abc", true).await;
        origin.shutdown().await.unwrap();
        assert!(body.collect().await.is_err());
        assert_eq!(pool.len(), 0, "incomplete fixed body was discarded");

        let (body, pool, mut origin) =
            streaming_body(OriginBodyFraming::Chunked, b"1\r\naX", true).await;
        origin.shutdown().await.unwrap();
        assert!(body.collect().await.is_err());
        assert_eq!(pool.len(), 0, "chunk framing error was discarded");

        let (body, pool, mut origin) =
            streaming_body(OriginBodyFraming::Chunked, b"2\r\na", true).await;
        origin.shutdown().await.unwrap();
        assert!(body.collect().await.is_err());
        assert_eq!(pool.len(), 0, "incomplete chunked body was discarded");

        let (body, pool, _origin) =
            streaming_body(OriginBodyFraming::ContentLength(3), b"abc", false).await;
        assert_eq!(body.collect().await.unwrap().to_bytes(), "abc");
        assert_eq!(pool.len(), 0, "Connection: close stream was discarded");

        let (body, pool, mut origin) =
            streaming_body(OriginBodyFraming::ContentLength(3), b"", true).await;
        drop(body);
        assert_eq!(pool.len(), 0, "dropped response body was discarded");
        let mut byte = [0_u8; 1];
        assert_eq!(
            timeout(Duration::from_secs(1), origin.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );

        let (mut body, pool, mut origin) =
            streaming_body(OriginBodyFraming::ContentLength(3), b"", true).await;
        let task = tokio::spawn(async move { body.frame().await });
        tokio::task::yield_now().await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(pool.len(), 0, "cancelled response body was discarded");
        assert_eq!(
            timeout(Duration::from_secs(1), origin.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
    }
}
