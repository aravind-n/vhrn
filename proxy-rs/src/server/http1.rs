//! Bounded HTTP/1 request-head parsing and request-body framing.

use bytes::Bytes;
use hyper::{HeaderMap, Method, Version, header};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub(crate) const REQUEST_LINE_LIMIT: usize = 8 * 1024;
pub(crate) const HEADER_SECTION_LIMIT: usize = 64 * 1024;
pub(crate) const APPLICATION_BUFFER_LIMIT: usize = 64 * 1024;
const CONNECTION_BUFFER_LIMIT: usize = REQUEST_LINE_LIMIT + HEADER_SECTION_LIMIT;
const CHUNK_LINE_LIMIT: usize = REQUEST_LINE_LIMIT;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IngressError {
    BadRequest,
    HeadersTooLarge,
    UnsupportedVersion,
    Incomplete,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BodyFraming {
    None,
    ContentLength(u64),
    Chunked,
}

impl BodyFraming {
    pub(crate) const fn has_body(self) -> bool {
        !matches!(self, Self::None | Self::ContentLength(0))
    }
}

#[derive(Debug)]
pub(crate) struct RequestHead {
    pub(crate) method: Method,
    pub(crate) raw_target: Vec<u8>,
    pub(crate) version: Version,
    pub(crate) headers: HeaderMap,
    pub(crate) framing: BodyFraming,
    pub(crate) close: bool,
    pub(crate) upgrade: bool,
    pub(crate) expect: bool,
}

/// One downstream connection and its sole fixed-capacity read buffer.
pub(crate) struct Http1Connection<S> {
    io: S,
    buffer: Box<[u8]>,
    start: usize,
    end: usize,
    request_is_head: bool,
    prefetched_error: Option<(IngressError, bool)>,
}

impl<S> Http1Connection<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    pub(crate) fn new(io: S) -> Self {
        Self {
            io,
            buffer: vec![0; CONNECTION_BUFFER_LIMIT].into_boxed_slice(),
            start: 0,
            end: 0,
            request_is_head: false,
            prefetched_error: None,
        }
    }

    pub(crate) async fn wait_for_head_start(&mut self) -> Result<bool, IngressError> {
        if self.start != self.end {
            return Ok(true);
        }
        self.compact();
        let count = self
            .io
            .read(&mut self.buffer[..1])
            .await
            .map_err(|_| IngressError::Incomplete)?;
        self.end = count;
        Ok(count != 0)
    }

    pub(crate) async fn read_head(&mut self) -> Result<Option<RequestHead>, IngressError> {
        if let Some((error, request_is_head)) = self.prefetched_error.take() {
            self.request_is_head = request_is_head;
            return Err(error);
        }
        self.compact();
        self.request_is_head = false;
        let request_start = self.start;
        let request_line_end = loop {
            let available = &self.buffer[request_start..self.end];
            let line_end = find_crlf(available);
            let validated_end = line_end.map_or(available.len(), |end| end + 2);
            validate_line_endings(&available[..validated_end])?;
            let prefix_end = line_end.unwrap_or_else(|| {
                available
                    .len()
                    .saturating_sub(usize::from(available.ends_with(b"\r")))
            });
            validate_request_line_prefix(&available[..prefix_end])?;
            if let Some(end) = line_end {
                let end = request_start + end + 2;
                if end - request_start > REQUEST_LINE_LIMIT {
                    return Err(IngressError::BadRequest);
                }
                self.request_is_head = request_line_is_head(&self.buffer[request_start..end]);
                break end;
            }
            if available.len() >= REQUEST_LINE_LIMIT {
                return Err(IngressError::BadRequest);
            }
            let was_empty = available.is_empty();
            if !self
                .read_more_until(request_start + REQUEST_LINE_LIMIT)
                .await?
            {
                return if was_empty {
                    Ok(None)
                } else {
                    Err(IngressError::Incomplete)
                };
            }
        };

        let head_end = loop {
            let available = &self.buffer[request_start..self.end];
            let found_end = find_head_end(available, request_line_end - request_start);
            let validated_end = found_end.unwrap_or(available.len());
            validate_line_endings(&available[..validated_end])?;
            if let Some(end) = found_end {
                let end = request_start + end;
                if end - request_line_end > HEADER_SECTION_LIMIT {
                    return Err(IngressError::HeadersTooLarge);
                }
                break end;
            }
            if self.end - request_line_end >= HEADER_SECTION_LIMIT {
                return Err(IngressError::HeadersTooLarge);
            }
            if !self
                .read_more_until(request_line_end + HEADER_SECTION_LIMIT)
                .await?
            {
                return Err(IngressError::Incomplete);
            }
        };

        let head = parse_head(
            &self.buffer[request_start..request_line_end],
            &self.buffer[request_line_end..head_end],
        )?;
        self.start = head_end;
        Ok(Some(head))
    }

    pub(crate) fn incoming_body(&mut self, framing: BodyFraming) -> IncomingBody<'_, S> {
        IncomingBody {
            connection: self,
            state: match framing {
                BodyFraming::None => BodyState::Complete,
                BodyFraming::ContentLength(remaining) => BodyState::Fixed { remaining },
                BodyFraming::Chunked => BodyState::ChunkSize,
            },
        }
    }

    pub(crate) const fn request_is_head(&self) -> bool {
        self.request_is_head
    }

    pub(crate) async fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.io.write_all(bytes).await
    }

    pub(crate) async fn flush(&mut self) -> std::io::Result<()> {
        self.io.flush().await
    }

    pub(crate) async fn buffer_during_response(&mut self) -> std::io::Result<bool> {
        self.compact();
        if self.prefetched_error.is_some() {
            return std::future::pending().await;
        }
        let Some(limit) = self.prefetch_limit() else {
            return std::future::pending().await;
        };
        let read_end = self
            .end
            .saturating_add(APPLICATION_BUFFER_LIMIT)
            .min(limit)
            .min(self.buffer.len());
        let count = self.io.read(&mut self.buffer[self.end..read_end]).await?;
        self.end += count;
        if count != 0 {
            let _ = self.prefetch_limit();
        }
        Ok(count != 0)
    }

    fn prefetch_limit(&mut self) -> Option<usize> {
        let available = &self.buffer[..self.end];
        if let Some(line) = find_crlf(available) {
            let request_line_end = line + 2;
            if find_head_end(available, request_line_end).is_some() {
                return None;
            }
            let limit = request_line_end + HEADER_SECTION_LIMIT;
            if self.end >= limit {
                self.prefetched_error = Some((
                    IngressError::HeadersTooLarge,
                    request_line_is_head(available),
                ));
                return None;
            }
            Some(limit)
        } else if self.end >= REQUEST_LINE_LIMIT {
            self.prefetched_error =
                Some((IngressError::BadRequest, request_line_is_head(available)));
            None
        } else {
            Some(REQUEST_LINE_LIMIT)
        }
    }

    pub(crate) fn into_buffered_io(self) -> BufferedIo<S> {
        let prefix = Bytes::copy_from_slice(&self.buffer[self.start..self.end]);
        BufferedIo {
            io: self.io,
            prefix,
            offset: 0,
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

    async fn read_more_until(&mut self, limit: usize) -> Result<bool, IngressError> {
        if self.end >= limit {
            return Ok(false);
        }
        let read_end = self
            .end
            .saturating_add(APPLICATION_BUFFER_LIMIT)
            .min(limit)
            .min(self.buffer.len());
        let count = self
            .io
            .read(&mut self.buffer[self.end..read_end])
            .await
            .map_err(|_| IngressError::Incomplete)?;
        self.end += count;
        Ok(count != 0)
    }

    async fn read_at_most(&mut self, limit: usize) -> Result<usize, IngressError> {
        self.compact();
        let read_end = limit.min(self.buffer.len());
        let count = self
            .io
            .read(&mut self.buffer[..read_end])
            .await
            .map_err(|_| IngressError::Incomplete)?;
        self.end = count;
        Ok(count)
    }

    async fn ensure_buffered(&mut self, count: usize) -> Result<(), IngressError> {
        while self.end - self.start < count {
            self.compact();
            let read_end = self
                .end
                .saturating_add(APPLICATION_BUFFER_LIMIT)
                .min(count)
                .min(self.buffer.len());
            let read = self
                .io
                .read(&mut self.buffer[self.end..read_end])
                .await
                .map_err(|_| IngressError::Incomplete)?;
            if read == 0 {
                return Err(IngressError::Incomplete);
            }
            self.end += read;
        }
        Ok(())
    }
}

fn validate_line_endings(bytes: &[u8]) -> Result<(), IngressError> {
    for (index, byte) in bytes.iter().copied().enumerate() {
        if byte == b'\n' && (index == 0 || bytes[index - 1] != b'\r') {
            return Err(IngressError::BadRequest);
        }
        if byte == b'\r' && index + 1 < bytes.len() && bytes[index + 1] != b'\n' {
            return Err(IngressError::BadRequest);
        }
    }
    Ok(())
}

fn validate_request_line_prefix(bytes: &[u8]) -> Result<(), IngressError> {
    if bytes.iter().any(|byte| *byte < b' ' || *byte >= 0x7f) {
        return Err(IngressError::BadRequest);
    }
    Ok(())
}

fn find_crlf(bytes: &[u8]) -> Option<usize> {
    bytes.windows(2).position(|window| window == b"\r\n")
}

fn request_line_is_head(bytes: &[u8]) -> bool {
    bytes.split(|byte| *byte == b' ').next() == Some(b"HEAD".as_slice())
}

fn find_head_end(bytes: &[u8], request_line_len: usize) -> Option<usize> {
    if bytes.get(request_line_len..request_line_len + 2) == Some(b"\r\n") {
        return Some(request_line_len + 2);
    }
    bytes[request_line_len..]
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| request_line_len + index + 4)
}

fn parse_head(request_line: &[u8], header_section: &[u8]) -> Result<RequestHead, IngressError> {
    let line = request_line
        .strip_suffix(b"\r\n")
        .ok_or(IngressError::BadRequest)?;
    let mut fields = line.split(|byte| *byte == b' ');
    let method = fields.next().filter(|value| !value.is_empty());
    let raw_target = fields.next().filter(|value| !value.is_empty());
    let version = fields.next().filter(|value| !value.is_empty());
    if fields.next().is_some() {
        return Err(IngressError::BadRequest);
    }
    let method = Method::from_bytes(method.ok_or(IngressError::BadRequest)?)
        .map_err(|_| IngressError::BadRequest)?;
    let raw_target = raw_target.ok_or(IngressError::BadRequest)?.to_vec();
    if raw_target.iter().any(u8::is_ascii_control) {
        return Err(IngressError::BadRequest);
    }
    let version = match version.ok_or(IngressError::BadRequest)? {
        b"HTTP/1.0" => Version::HTTP_10,
        b"HTTP/1.1" => Version::HTTP_11,
        [b'H', b'T', b'T', b'P', b'/', major, b'.', minor]
            if major.is_ascii_digit() && minor.is_ascii_digit() =>
        {
            return Err(IngressError::UnsupportedVersion);
        }
        _ => return Err(IngressError::BadRequest),
    };

    let headers = parse_fields(header_section)?;
    validate_host(version, &headers)?;
    if version == Version::HTTP_10 && headers.contains_key(header::TRANSFER_ENCODING) {
        return Err(IngressError::BadRequest);
    }
    let framing = parse_framing(&headers)?;
    let connection_tokens = comma_tokens(&headers, header::CONNECTION)?;
    let close = if version == Version::HTTP_10 {
        !connection_tokens
            .iter()
            .any(|token| token.eq_ignore_ascii_case(b"keep-alive"))
    } else {
        connection_tokens
            .iter()
            .any(|token| token.eq_ignore_ascii_case(b"close"))
    };
    let connection_upgrade = connection_tokens
        .iter()
        .any(|token| token.eq_ignore_ascii_case(b"upgrade"));
    let upgrade = headers.contains_key(header::UPGRADE) || connection_upgrade;

    Ok(RequestHead {
        method,
        raw_target,
        version,
        expect: headers.contains_key(header::EXPECT),
        headers,
        framing,
        close,
        upgrade,
    })
}

fn parse_fields(section: &[u8]) -> Result<HeaderMap, IngressError> {
    if !section.ends_with(b"\r\n") {
        return Err(IngressError::BadRequest);
    }
    let mut headers = HeaderMap::new();
    let mut cursor = 0;
    while cursor < section.len() {
        let relative_end = find_crlf(&section[cursor..]).ok_or(IngressError::BadRequest)?;
        let end = cursor + relative_end;
        let line = &section[cursor..end];
        cursor = end + 2;
        if line.is_empty() {
            if cursor != section.len() {
                return Err(IngressError::BadRequest);
            }
            break;
        }
        if line
            .first()
            .is_some_and(|byte| matches!(byte, b' ' | b'\t'))
        {
            return Err(IngressError::BadRequest);
        }
        let colon = line
            .iter()
            .position(|byte| *byte == b':')
            .ok_or(IngressError::BadRequest)?;
        let name = &line[..colon];
        if name.is_empty() || name.last().is_some_and(u8::is_ascii_whitespace) {
            return Err(IngressError::BadRequest);
        }
        let value = trim_ows(&line[colon + 1..]);
        if value
            .iter()
            .any(|byte| (*byte < 0x20 && *byte != b'\t') || *byte == 0x7f)
        {
            return Err(IngressError::BadRequest);
        }
        let name =
            hyper::header::HeaderName::from_bytes(name).map_err(|_| IngressError::BadRequest)?;
        let value =
            hyper::header::HeaderValue::from_bytes(value).map_err(|_| IngressError::BadRequest)?;
        headers.append(name, value);
    }
    Ok(headers)
}

fn trim_ows(mut value: &[u8]) -> &[u8] {
    while value
        .first()
        .is_some_and(|byte| matches!(byte, b' ' | b'\t'))
    {
        value = &value[1..];
    }
    while value
        .last()
        .is_some_and(|byte| matches!(byte, b' ' | b'\t'))
    {
        value = &value[..value.len() - 1];
    }
    value
}

fn validate_host(version: Version, headers: &HeaderMap) -> Result<(), IngressError> {
    let hosts: Vec<_> = headers.get_all(header::HOST).iter().collect();
    if version == Version::HTTP_11 && hosts.len() != 1 {
        return Err(IngressError::BadRequest);
    }
    for host in hosts {
        let value = host.to_str().map_err(|_| IngressError::BadRequest)?;
        if value.is_empty()
            || value.trim() != value
            || value.contains(',')
            || value
                .bytes()
                .any(|byte| byte.is_ascii_whitespace() || matches!(byte, b'@' | b'/' | b'?' | b'#'))
            || value.parse::<hyper::http::uri::Authority>().is_err()
        {
            return Err(IngressError::BadRequest);
        }
    }
    Ok(())
}

fn parse_framing(headers: &HeaderMap) -> Result<BodyFraming, IngressError> {
    let content_length = parse_content_length(headers)?;
    let transfer_encoding = parse_transfer_encoding(headers)?;
    match (content_length, transfer_encoding) {
        (Some(_), true) => Err(IngressError::BadRequest),
        (Some(length), false) => Ok(BodyFraming::ContentLength(length)),
        (None, true) => Ok(BodyFraming::Chunked),
        (None, false) => Ok(BodyFraming::None),
    }
}

fn parse_content_length(headers: &HeaderMap) -> Result<Option<u64>, IngressError> {
    let mut parsed = None;
    for value in headers.get_all(header::CONTENT_LENGTH) {
        let value = value.as_bytes();
        for member in value.split(|byte| *byte == b',') {
            let member = trim_ows(member);
            if member.is_empty() || !member.iter().all(u8::is_ascii_digit) {
                return Err(IngressError::BadRequest);
            }
            let length = member.iter().try_fold(0_u64, |length, digit| {
                length
                    .checked_mul(10)?
                    .checked_add(u64::from(*digit - b'0'))
            });
            let length = length.ok_or(IngressError::BadRequest)?;
            if parsed.is_some_and(|previous| previous != length) {
                return Err(IngressError::BadRequest);
            }
            parsed = Some(length);
        }
    }
    Ok(parsed)
}

fn parse_transfer_encoding(headers: &HeaderMap) -> Result<bool, IngressError> {
    let mut values = Vec::new();
    for value in headers.get_all(header::TRANSFER_ENCODING) {
        values.extend(
            split_quoted(value.as_bytes(), b',')?
                .into_iter()
                .map(trim_ows),
        );
    }
    if values.is_empty() {
        return Ok(false);
    }
    if values.iter().any(|value| !valid_transfer_coding(value))
        || !values
            .last()
            .is_some_and(|value| transfer_coding_name(value).eq_ignore_ascii_case(b"chunked"))
        || values[..values.len() - 1]
            .iter()
            .any(|value| transfer_coding_name(value).eq_ignore_ascii_case(b"chunked"))
        || values.iter().any(|value| {
            transfer_coding_name(value).eq_ignore_ascii_case(b"chunked") && value.contains(&b';')
        })
    {
        return Err(IngressError::BadRequest);
    }
    Ok(true)
}

fn valid_transfer_coding(value: &[u8]) -> bool {
    let Ok(parts) = split_quoted(value, b';') else {
        return false;
    };
    let name = trim_ows(parts.first().copied().unwrap_or_default());
    if !valid_token(name) {
        return false;
    }
    parts[1..].iter().all(|parameter| {
        let parameter = trim_ows(parameter);
        let Ok(fields) = split_quoted(parameter, b'=') else {
            return false;
        };
        fields.len() == 2
            && valid_token(trim_ows(fields[0]))
            && valid_parameter_value(trim_ows(fields[1]))
    })
}

fn transfer_coding_name(value: &[u8]) -> &[u8] {
    trim_ows(value.split(|byte| *byte == b';').next().unwrap_or_default())
}

fn valid_parameter_value(value: &[u8]) -> bool {
    if valid_token(value) {
        return true;
    }
    let Some(inner) = value
        .strip_prefix(b"\"")
        .and_then(|value| value.strip_suffix(b"\""))
    else {
        return false;
    };
    let mut escaped = false;
    for byte in inner {
        if escaped {
            if !quoted_octet(*byte) {
                return false;
            }
            escaped = false;
        } else if *byte == b'\\' {
            escaped = true;
        } else if *byte == b'"' || !quoted_octet(*byte) {
            return false;
        }
    }
    !escaped
}

fn quoted_octet(byte: u8) -> bool {
    byte == b'\t' || byte == b' ' || (0x21..=0x7e).contains(&byte) || byte >= 0x80
}

fn split_quoted(value: &[u8], delimiter: u8) -> Result<Vec<&[u8]>, IngressError> {
    let mut fields = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    let mut escaped = false;
    for (index, byte) in value.iter().copied().enumerate() {
        if escaped {
            escaped = false;
        } else if quoted && byte == b'\\' {
            escaped = true;
        } else if byte == b'"' {
            quoted = !quoted;
        } else if byte == delimiter && !quoted {
            fields.push(&value[start..index]);
            start = index + 1;
        }
    }
    if quoted || escaped {
        return Err(IngressError::BadRequest);
    }
    fields.push(&value[start..]);
    Ok(fields)
}

fn valid_token(value: &[u8]) -> bool {
    !value.is_empty()
        && value.iter().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

fn comma_tokens(
    headers: &HeaderMap,
    name: header::HeaderName,
) -> Result<Vec<Vec<u8>>, IngressError> {
    let mut tokens = Vec::new();
    for value in headers.get_all(name) {
        for token in value.as_bytes().split(|byte| *byte == b',') {
            let token = trim_ows(token);
            if !valid_token(token) {
                return Err(IngressError::BadRequest);
            }
            tokens.push(token.to_vec());
        }
    }
    Ok(tokens)
}

enum BodyState {
    Complete,
    Fixed { remaining: u64 },
    ChunkSize,
    ChunkData { remaining: u64 },
    ChunkDataEnd,
    Trailers,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum BodyFrame {
    Data(Bytes),
    Trailers(HeaderMap),
}

/// A streaming decoder borrowing the only owner of the downstream socket.
pub(crate) struct IncomingBody<'a, S> {
    connection: &'a mut Http1Connection<S>,
    state: BodyState,
}

impl<S> IncomingBody<'_, S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    pub(crate) async fn next_frame(&mut self) -> Result<Option<BodyFrame>, IngressError> {
        loop {
            match self.state {
                BodyState::Complete => return Ok(None),
                BodyState::Fixed { remaining: 0 } => self.state = BodyState::Complete,
                BodyState::Fixed { remaining } => {
                    let maximum = usize::try_from(remaining)
                        .unwrap_or(usize::MAX)
                        .min(APPLICATION_BUFFER_LIMIT);
                    let available = self.connection.end - self.connection.start;
                    if available == 0 && self.connection.read_at_most(maximum).await? == 0 {
                        return Err(IngressError::Incomplete);
                    }
                    let count = maximum.min(self.connection.end - self.connection.start);
                    let end = self.connection.start + count;
                    let data =
                        Bytes::copy_from_slice(&self.connection.buffer[self.connection.start..end]);
                    self.connection.start = end;
                    self.state = BodyState::Fixed {
                        remaining: remaining - count as u64,
                    };
                    return Ok(Some(BodyFrame::Data(data)));
                }
                BodyState::ChunkSize => {
                    let line = self.read_line(CHUNK_LINE_LIMIT).await?;
                    let size = parse_chunk_size(&line)?;
                    self.state = if size == 0 {
                        BodyState::Trailers
                    } else {
                        BodyState::ChunkData { remaining: size }
                    };
                }
                BodyState::ChunkData { remaining } => {
                    let maximum = usize::try_from(remaining)
                        .unwrap_or(usize::MAX)
                        .min(APPLICATION_BUFFER_LIMIT);
                    let available = self.connection.end - self.connection.start;
                    if available == 0 && self.connection.read_at_most(maximum).await? == 0 {
                        return Err(IngressError::Incomplete);
                    }
                    let count = maximum.min(self.connection.end - self.connection.start);
                    let end = self.connection.start + count;
                    let data =
                        Bytes::copy_from_slice(&self.connection.buffer[self.connection.start..end]);
                    self.connection.start = end;
                    let remaining = remaining - count as u64;
                    self.state = if remaining == 0 {
                        BodyState::ChunkDataEnd
                    } else {
                        BodyState::ChunkData { remaining }
                    };
                    return Ok(Some(BodyFrame::Data(data)));
                }
                BodyState::ChunkDataEnd => {
                    self.connection.ensure_buffered(2).await?;
                    if &self.connection.buffer[self.connection.start..self.connection.start + 2]
                        != b"\r\n"
                    {
                        return Err(IngressError::BadRequest);
                    }
                    self.connection.start += 2;
                    self.state = BodyState::ChunkSize;
                }
                BodyState::Trailers => {
                    let raw = self.read_trailers().await?;
                    let trailers = parse_fields(&raw)?;
                    validate_trailers(&trailers)?;
                    self.state = BodyState::Complete;
                    if trailers.is_empty() {
                        return Ok(None);
                    }
                    return Ok(Some(BodyFrame::Trailers(trailers)));
                }
            }
        }
    }

    async fn read_line(&mut self, limit: usize) -> Result<Vec<u8>, IngressError> {
        loop {
            let available = &self.connection.buffer[self.connection.start..self.connection.end];
            let line_end = find_crlf(available);
            let validated_end = line_end.map_or(available.len(), |end| end + 2);
            validate_line_endings(&available[..validated_end])?;
            if let Some(end) = line_end {
                if end + 2 > limit {
                    return Err(IngressError::BadRequest);
                }
                let line = available[..end].to_vec();
                self.connection.start += end + 2;
                return Ok(line);
            }
            if available.len() >= limit {
                return Err(IngressError::BadRequest);
            }
            self.connection.compact();
            if !self.connection.read_more_until(limit).await? {
                return Err(IngressError::Incomplete);
            }
        }
    }

    async fn read_trailers(&mut self) -> Result<Vec<u8>, IngressError> {
        let mut raw = Vec::new();
        loop {
            let remaining = HEADER_SECTION_LIMIT.saturating_sub(raw.len());
            if remaining < 2 {
                return Err(IngressError::HeadersTooLarge);
            }
            let line = self.read_line(remaining).await?;
            if line.len() + 2 > remaining {
                return Err(IngressError::HeadersTooLarge);
            }
            let empty = line.is_empty();
            raw.extend_from_slice(&line);
            raw.extend_from_slice(b"\r\n");
            if empty {
                return Ok(raw);
            }
        }
    }
}

fn parse_chunk_size(line: &[u8]) -> Result<u64, IngressError> {
    let parts = split_quoted(line, b';')?;
    let size = parts.first().copied().unwrap_or_default();
    if size.is_empty() || !size.iter().all(u8::is_ascii_hexdigit) {
        return Err(IngressError::BadRequest);
    }
    if parts[1..].iter().any(|extension| {
        let extension = trim_ows(extension);
        if extension.is_empty() {
            return true;
        }
        match split_quoted(extension, b'=') {
            Ok(fields) if fields.len() == 1 => !valid_token(trim_ows(fields[0])),
            Ok(fields) if fields.len() == 2 => {
                !valid_token(trim_ows(fields[0])) || !valid_parameter_value(trim_ows(fields[1]))
            }
            _ => true,
        }
    }) {
        return Err(IngressError::BadRequest);
    }
    size.iter()
        .try_fold(0_u64, |value, digit| {
            value
                .checked_mul(16)?
                .checked_add(u64::from(hex_value(*digit)?))
        })
        .ok_or(IngressError::BadRequest)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn validate_trailers(trailers: &HeaderMap) -> Result<(), IngressError> {
    const FORBIDDEN: [&str; 8] = [
        "connection",
        "content-length",
        "host",
        "proxy-authorization",
        "proxy-authenticate",
        "te",
        "trailer",
        "transfer-encoding",
    ];
    if trailers
        .keys()
        .any(|name| FORBIDDEN.contains(&name.as_str()))
    {
        return Err(IngressError::BadRequest);
    }
    Ok(())
}

/// An owned stream which replays bytes read beyond a CONNECT head first.
pub(crate) struct BufferedIo<S> {
    io: S,
    prefix: Bytes,
    offset: usize,
}

impl<S: AsyncRead + Unpin> AsyncRead for BufferedIo<S> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if self.offset < self.prefix.len() {
            let count = (self.prefix.len() - self.offset).min(buffer.remaining());
            buffer.put_slice(&self.prefix[self.offset..self.offset + count]);
            self.offset += count;
            return std::task::Poll::Ready(Ok(()));
        }
        std::pin::Pin::new(&mut self.io).poll_read(context, buffer)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for BufferedIo<S> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buffer: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.io).poll_write(context, buffer)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.io).poll_flush(context)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.io).poll_shutdown(context)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt as _;

    async fn test_connection(bytes: &[u8]) -> Http1Connection<tokio::io::DuplexStream> {
        let (mut client, server) = tokio::io::duplex(CONNECTION_BUFFER_LIMIT * 2);
        client.write_all(bytes).await.unwrap();
        client.shutdown().await.unwrap();
        Http1Connection::new(server)
    }

    #[tokio::test]
    async fn http1_request_line_raw_octet_limits_are_exact() {
        for (length, expected) in [
            (REQUEST_LINE_LIMIT - 1, Ok(())),
            (REQUEST_LINE_LIMIT, Ok(())),
            (REQUEST_LINE_LIMIT + 1, Err(IngressError::BadRequest)),
        ] {
            let fixed = b"GET / HTTP/1.1\r\n".len();
            let mut bytes = b"GET /".to_vec();
            bytes.extend(std::iter::repeat_n(b'a', length - fixed));
            bytes.extend_from_slice(b" HTTP/1.1\r\nHost: test\r\n\r\n");
            let mut connection = test_connection(&bytes).await;
            let actual = connection.read_head().await.map(|_| ());
            assert_eq!(actual, expected, "request-line length {length}");
            if length > REQUEST_LINE_LIMIT {
                assert!(connection.end <= REQUEST_LINE_LIMIT);
            }
        }
    }

    #[tokio::test]
    async fn http1_header_raw_octet_limits_are_exact() {
        for (length, expected) in [
            (HEADER_SECTION_LIMIT - 1, Ok(())),
            (HEADER_SECTION_LIMIT, Ok(())),
            (HEADER_SECTION_LIMIT + 1, Err(IngressError::HeadersTooLarge)),
        ] {
            let fixed = b"Host: test\r\nX: \r\n\r\n".len();
            let mut bytes = b"GET / HTTP/1.1\r\nHost: test\r\nX: ".to_vec();
            bytes.extend(std::iter::repeat_n(b'a', length - fixed));
            bytes.extend_from_slice(b"\r\n\r\n");
            let mut connection = test_connection(&bytes).await;
            let actual = connection.read_head().await.map(|_| ());
            assert_eq!(actual, expected, "header-section length {length}");
            assert!(
                connection.end <= b"GET / HTTP/1.1\r\n".len() + HEADER_SECTION_LIMIT,
                "header overflow was detected before reading beyond its cap"
            );
        }
    }

    #[tokio::test]
    async fn http1_byte_at_a_time_fragmentation_preserves_head_and_body() {
        let bytes = b"POST /healthz HTTP/1.1\r\nHost: test\r\nContent-Length: 4\r\n\r\nbody";
        let (mut client, server) = tokio::io::duplex(1);
        let writer = tokio::spawn(async move {
            for byte in bytes {
                client.write_all(&[*byte]).await.unwrap();
            }
        });
        let mut connection = Http1Connection::new(server);
        let head = connection.read_head().await.unwrap().unwrap();
        assert_eq!(head.raw_target, b"/healthz");
        assert_eq!(head.framing, BodyFraming::ContentLength(4));
        let mut body = connection.incoming_body(head.framing);
        let mut received = Vec::new();
        while let Some(frame) = body.next_frame().await.unwrap() {
            if let BodyFrame::Data(data) = frame {
                received.extend_from_slice(&data);
            }
        }
        assert_eq!(received, b"body");
        writer.await.unwrap();
    }

    #[tokio::test]
    async fn http1_rejects_ambiguous_framing_and_invalid_hosts() {
        let cases: &[&[u8]] = &[
            b"GET / HTTP/1.1\r\n\r\n",
            b"GET / HTTP/1.1\r\nHost: one\r\nHost: two\r\n\r\n",
            b"GET / HTTP/1.1\r\nHost : test\r\n\r\n",
            b"GET / HTTP/1.1\r\n Host: test\r\n\r\n",
            b"GET / HTTP/1.1\r\nHost: test\r\nContent-Length: x\r\n\r\n",
            b"GET / HTTP/1.1\r\nHost: test\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n",
            b"GET / HTTP/1.1\r\nHost: test\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n\r\n",
            b"GET / HTTP/1.1\r\nHost: test\r\nTransfer-Encoding: gzip\r\n\r\n",
            b"GET / HTTP/1.1\r\nHost: test\r\nTransfer-Encoding: chunked, gzip\r\n\r\n",
            b"GET / HTTP/1.1\r\nHost: test\r\nTransfer-Encoding: chunked; x=y\r\n\r\n",
            b"GET / HTTP/1.1\r\nHost: test\r\nContent-Length: 18446744073709551616\r\n\r\n",
            b"GET / HTTP/1.1\nHost: test\r\n\r\n",
            b"GET / HTTP/1.1\rX: y\r\n\r\n",
        ];
        for bytes in cases {
            let mut connection = test_connection(bytes).await;
            assert!(
                matches!(connection.read_head().await, Err(IngressError::BadRequest)),
                "{}",
                String::from_utf8_lossy(bytes)
            );
        }
    }

    #[tokio::test]
    async fn http1_many_small_headers_fit_by_raw_octets_not_field_count() {
        let mut bytes = b"GET / HTTP/1.1\r\nHost: test\r\n".to_vec();
        for _ in 0..1_000 {
            bytes.extend_from_slice(b"X-Small: x\r\n");
        }
        bytes.extend_from_slice(b"\r\n");
        let mut connection = test_connection(&bytes).await;
        let head = connection.read_head().await.unwrap().unwrap();
        assert_eq!(head.headers.get_all("x-small").iter().count(), 1_000);
    }

    #[tokio::test]
    async fn http1_rejects_tls_and_unsupported_versions_without_waiting_for_more() {
        let mut tls = test_connection(b"\x16\x03\x01\x00\x10").await;
        assert!(matches!(
            tls.read_head().await,
            Err(IngressError::BadRequest)
        ));

        let mut http2 = test_connection(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n").await;
        assert!(matches!(
            http2.read_head().await,
            Err(IngressError::UnsupportedVersion)
        ));

        for bytes in [
            b"GET / HTTP/\r\nHost: test\r\n\r\n".as_slice(),
            b"GET / HTTP/1\r\nHost: test\r\n\r\n",
            b"GET / HTTP/1.x\r\nHost: test\r\n\r\n",
            b"GET / HTTP/11.1\r\nHost: test\r\n\r\n",
        ] {
            let mut malformed = test_connection(bytes).await;
            assert_eq!(
                malformed.read_head().await.unwrap_err(),
                IngressError::BadRequest,
                "{}",
                String::from_utf8_lossy(bytes)
            );
        }
    }

    #[tokio::test]
    async fn http1_transfer_parameters_observe_quoted_delimiters_and_escapes() {
        let mut connection = test_connection(
            b"POST / HTTP/1.1\r\nHost: test\r\nTransfer-Encoding: gzip; note=\"a,b;c\\\"d\\\\e\", chunked\r\n\r\n0\r\n\r\n",
        )
        .await;
        let head = connection.read_head().await.unwrap().unwrap();
        assert_eq!(head.framing, BodyFraming::Chunked);
        assert_eq!(
            connection
                .incoming_body(head.framing)
                .next_frame()
                .await
                .unwrap(),
            None
        );

        assert_eq!(parse_chunk_size(b"4; note=\"a,b;c\\\"d\\\\e\""), Ok(4));
        for line in [
            b"4; note=\"unterminated".as_slice(),
            b"4; note=\"dangling\\\"",
            b"4; note=\"bad\"quote\"",
            b"4; note=a=b",
            b"4;;note=x",
        ] {
            assert_eq!(
                parse_chunk_size(line),
                Err(IngressError::BadRequest),
                "{}",
                String::from_utf8_lossy(line)
            );
        }

        for value in [
            b"gzip; note=\"unterminated, chunked".as_slice(),
            b"gzip; note=\"dangling\\\", chunked",
            b"gzip; note=\"bad\"quote\", chunked",
            b"gzip; note=a=b, chunked",
            b"gzip;;note=x, chunked",
        ] {
            let mut bytes = b"POST / HTTP/1.1\r\nHost: test\r\nTransfer-Encoding: ".to_vec();
            bytes.extend_from_slice(value);
            bytes.extend_from_slice(b"\r\n\r\n");
            let mut malformed = test_connection(&bytes).await;
            assert_eq!(
                malformed.read_head().await.unwrap_err(),
                IngressError::BadRequest,
                "{}",
                String::from_utf8_lossy(value)
            );
        }
    }

    #[tokio::test]
    async fn response_prefetch_enforces_the_active_next_head_limit() {
        let first = b"GET /healthz HTTP/1.1\r\nHost: test\r\n\r\n";

        let mut bytes = first.to_vec();
        bytes.extend_from_slice(b"GET /");
        bytes.extend(std::iter::repeat_n(b'a', REQUEST_LINE_LIMIT + 32));
        let (mut client, server) = tokio::io::duplex(CONNECTION_BUFFER_LIMIT * 2);
        client.write_all(&bytes).await.unwrap();
        let mut connection = Http1Connection::new(server);
        let head = connection.read_head().await.unwrap().unwrap();
        assert_eq!(head.raw_target, b"/healthz");
        assert!(connection.buffer_during_response().await.unwrap());
        assert_eq!(
            connection.end, REQUEST_LINE_LIMIT,
            "prefetch stopped at the active request-line boundary"
        );
        assert_eq!(
            connection.read_head().await.unwrap_err(),
            IngressError::BadRequest
        );

        let next_line = b"GET / HTTP/1.1\r\n";
        let mut bytes = first.to_vec();
        bytes.extend_from_slice(next_line);
        bytes.extend_from_slice(b"X: ");
        bytes.extend(std::iter::repeat_n(b'a', HEADER_SECTION_LIMIT + 32));

        let (mut client, server) = tokio::io::duplex(CONNECTION_BUFFER_LIMIT * 2);
        client.write_all(&bytes).await.unwrap();
        let mut connection = Http1Connection::new(server);
        let head = connection.read_head().await.unwrap().unwrap();
        assert_eq!(head.raw_target, b"/healthz");
        assert!(connection.buffer_during_response().await.unwrap());
        assert_eq!(
            connection.end,
            next_line.len() + HEADER_SECTION_LIMIT,
            "prefetch stopped at the active header boundary"
        );
        assert_eq!(
            connection.read_head().await.unwrap_err(),
            IngressError::HeadersTooLarge
        );
    }

    #[tokio::test]
    async fn prefetched_errors_use_the_next_requests_head_state() {
        for (previous, next, expected_body) in [
            ("GET", "HEAD", b"".as_slice()),
            (
                "HEAD",
                "GET",
                b"request header fields too large\n".as_slice(),
            ),
        ] {
            let first = format!("{previous} /healthz HTTP/1.1\r\nHost: test\r\n\r\n");
            let next_line = format!("{next} / HTTP/1.1\r\n");
            let mut bytes = first.as_bytes().to_vec();
            bytes.extend_from_slice(next_line.as_bytes());
            bytes.extend_from_slice(b"X: ");
            bytes.extend(std::iter::repeat_n(b'a', HEADER_SECTION_LIMIT + 32));

            let (mut client, server) = tokio::io::duplex(CONNECTION_BUFFER_LIMIT * 2);
            client.write_all(&bytes).await.unwrap();
            let mut connection = Http1Connection::new(server);
            connection.read_head().await.unwrap().unwrap();
            assert!(connection.buffer_during_response().await.unwrap());
            assert_eq!(
                connection.read_head().await.unwrap_err(),
                IngressError::HeadersTooLarge
            );
            let response = crate::server::response::failure(
                crate::server::response::ProxyFailure::HeadersTooLarge,
                connection.request_is_head(),
            );
            let body = response.into_body().collect().await.unwrap().to_bytes();
            assert_eq!(body, expected_body, "{previous} followed by {next}");
        }
    }

    #[tokio::test]
    async fn http1_accepts_http10_without_host_and_identical_content_lengths() {
        let mut connection = test_connection(
            b"POST /healthz HTTP/1.0\r\nContent-Length: 4, 4\r\nContent-Length: 4\r\n\r\nbody",
        )
        .await;
        let head = connection.read_head().await.unwrap().unwrap();
        assert_eq!(head.version, Version::HTTP_10);
        assert_eq!(head.framing, BodyFraming::ContentLength(4));
    }

    #[tokio::test]
    async fn http1_fixed_and_chunked_decoders_stream_and_preserve_trailers() {
        let payload = vec![b'x'; APPLICATION_BUFFER_LIMIT + 1];
        let mut bytes = format!(
            "POST / HTTP/1.1\r\nHost: test\r\nContent-Length: {}\r\n\r\n",
            payload.len()
        )
        .into_bytes();
        bytes.extend_from_slice(&payload);
        let mut connection = test_connection(&bytes).await;
        let head = connection.read_head().await.unwrap().unwrap();
        let mut body = connection.incoming_body(head.framing);
        let first = body.next_frame().await.unwrap().unwrap();
        let second = body.next_frame().await.unwrap().unwrap();
        assert!(
            matches!(first, BodyFrame::Data(ref data) if data.len() <= APPLICATION_BUFFER_LIMIT)
        );
        assert!(
            matches!(second, BodyFrame::Data(ref data) if data.len() <= APPLICATION_BUFFER_LIMIT)
        );
        assert_eq!(body.next_frame().await.unwrap(), None);

        let mut connection = test_connection(
            b"POST / HTTP/1.1\r\nHost: test\r\nTransfer-Encoding: chunked\r\nTrailer: X-End\r\n\r\n4\r\nbody\r\n0\r\nX-End: yes\r\n\r\n",
        )
        .await;
        let head = connection.read_head().await.unwrap().unwrap();
        let mut body = connection.incoming_body(head.framing);
        assert_eq!(
            body.next_frame().await.unwrap(),
            Some(BodyFrame::Data(Bytes::from_static(b"body")))
        );
        let trailers = body.next_frame().await.unwrap().unwrap();
        assert!(matches!(trailers, BodyFrame::Trailers(ref values) if values["x-end"] == "yes"));
        assert_eq!(body.next_frame().await.unwrap(), None);

        let mut connection = test_connection(
            b"POST / HTTP/1.1\r\nHost: test\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
        )
        .await;
        let head = connection.read_head().await.unwrap().unwrap();
        assert_eq!(
            connection
                .incoming_body(head.framing)
                .next_frame()
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn http1_body_decoder_rejects_early_disconnect_and_chunk_overflow() {
        let mut connection =
            test_connection(b"POST / HTTP/1.1\r\nHost: test\r\nContent-Length: 5\r\n\r\nbody")
                .await;
        let head = connection.read_head().await.unwrap().unwrap();
        let mut body = connection.incoming_body(head.framing);
        assert!(body.next_frame().await.is_ok());
        assert_eq!(body.next_frame().await, Err(IngressError::Incomplete));

        let mut connection = test_connection(
            b"POST / HTTP/1.1\r\nHost: test\r\nTransfer-Encoding: chunked\r\n\r\n10000000000000000\r\n",
        )
        .await;
        let head = connection.read_head().await.unwrap().unwrap();
        assert_eq!(
            connection.incoming_body(head.framing).next_frame().await,
            Err(IngressError::BadRequest)
        );
    }

    #[tokio::test]
    async fn http1_connect_prefix_is_replayed_before_socket_bytes() {
        let (mut client, server) = tokio::io::duplex(1024);
        client
            .write_all(b"CONNECT test:443 HTTP/1.1\r\nHost: test:443\r\n\r\nprefix")
            .await
            .unwrap();
        let mut connection = Http1Connection::new(server);
        let head = connection.read_head().await.unwrap().unwrap();
        assert_eq!(head.method, Method::CONNECT);
        let mut buffered = connection.into_buffered_io();
        client.write_all(b"suffix").await.unwrap();
        let mut bytes = [0; 12];
        buffered.read_exact(&mut bytes).await.unwrap();
        assert_eq!(&bytes, b"prefixsuffix");
    }
}
