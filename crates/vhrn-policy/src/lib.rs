//! Pure values used at the policy boundary.
#![forbid(unsafe_code)]

use std::fmt;
use std::net::Ipv6Addr;
use std::str::FromStr;

/// Maximum number of bytes accepted for a broker request, including its line feed.
pub const MAX_BROKER_FRAME_SIZE: usize = 256;

/// A public-policy operating mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Enforce,
    Report,
    Open,
}

impl Mode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Enforce => "enforce",
            Self::Report => "report",
            Self::Open => "open",
        }
    }
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "enforce" => Some(Self::Enforce),
            "report" => Some(Self::Report),
            "open" => Some(Self::Open),
            _ => None,
        }
    }
    #[must_use]
    pub fn readable_or_enforce(value: &str) -> Self {
        match Self::parse(value) {
            Some(mode) => mode,
            None => Self::Enforce,
        }
    }
}
impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a domain entry or host cannot be normalized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DomainError {
    NonAscii,
    Invalid,
}
impl fmt::Display for DomainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NonAscii => "domain must be ASCII",
            Self::Invalid => "invalid domain",
        })
    }
}
impl std::error::Error for DomainError {}

/// Normalizes a domain entry for policy storage.
///
/// # Errors
///
/// Returns an error when the entry is not a valid ASCII policy name.
pub fn normalize_domain_entry(input: &str) -> Result<String, DomainError> {
    let value = input.trim();
    normalize_domain(value.strip_prefix("*.").unwrap_or(value).trim_matches('.'))
}
/// Normalizes a host for policy comparison.
///
/// # Errors
///
/// Returns an error when the host is not a valid ASCII policy name.
pub fn normalize_domain_host(input: &str) -> Result<String, DomainError> {
    let value = input.trim();
    normalize_domain(value.strip_suffix('.').unwrap_or(value))
}
/// Returns whether an entry permits exactly this host or one of its subdomains.
#[must_use]
pub fn domain_entry_matches(entry: &str, host: &str) -> bool {
    host == entry
        || host
            .strip_suffix(entry)
            .is_some_and(|prefix| prefix.ends_with('.'))
}
fn normalize_domain(value: &str) -> Result<String, DomainError> {
    if !value.is_ascii() {
        return Err(DomainError::NonAscii);
    }
    let value = value.to_ascii_lowercase();
    if value.is_empty()
        || value.split('.').any(str::is_empty)
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        || !value.bytes().any(|byte| byte.is_ascii_alphanumeric())
    {
        return Err(DomainError::Invalid);
    }
    Ok(value)
}

/// An explicitly permitted host-loopback endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LoopbackAuthority {
    host: LoopbackHost,
    port: u16,
}
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum LoopbackHost {
    Localhost,
    Ipv4([u8; 4]),
    Ipv6Loopback,
}
/// Why an authority is not an explicit loopback endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoopbackAuthorityError;
impl fmt::Display for LoopbackAuthorityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid loopback authority")
    }
}
impl std::error::Error for LoopbackAuthorityError {}
impl LoopbackAuthority {
    /// Parses and canonicalizes an allowed loopback authority.
    ///
    /// # Errors
    ///
    /// Returns an error when the input is not an allowed loopback authority.
    pub fn parse(input: &str) -> Result<Self, LoopbackAuthorityError> {
        if input.is_empty() || input.trim() != input || !input.is_ascii() {
            return Err(LoopbackAuthorityError);
        }
        let (host, port) = input.rsplit_once(':').ok_or(LoopbackAuthorityError)?;
        let port = parse_port(port)?;
        let host = if host.eq_ignore_ascii_case("localhost") {
            LoopbackHost::Localhost
        } else if let Some(ipv6) = host
            .strip_prefix('[')
            .and_then(|host| host.strip_suffix(']'))
        {
            let ipv6 = Ipv6Addr::from_str(ipv6).map_err(|_| LoopbackAuthorityError)?;
            if !ipv6.is_loopback() {
                return Err(LoopbackAuthorityError);
            }
            LoopbackHost::Ipv6Loopback
        } else {
            LoopbackHost::Ipv4(parse_ipv4(host)?)
        };
        Ok(Self { host, port })
    }
}
impl fmt::Display for LoopbackAuthority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.host {
            LoopbackHost::Localhost => write!(f, "localhost:{}", self.port),
            LoopbackHost::Ipv4(o) => write!(f, "{}.{}.{}.{}:{}", o[0], o[1], o[2], o[3], self.port),
            LoopbackHost::Ipv6Loopback => write!(f, "[::1]:{}", self.port),
        }
    }
}
fn parse_port(value: &str) -> Result<u16, LoopbackAuthorityError> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(LoopbackAuthorityError);
    }
    let port = value.parse().map_err(|_| LoopbackAuthorityError)?;
    if port == 0 {
        return Err(LoopbackAuthorityError);
    }
    Ok(port)
}
fn parse_ipv4(value: &str) -> Result<[u8; 4], LoopbackAuthorityError> {
    let mut octets = [0; 4];
    let mut parts = value.split('.');
    for octet in &mut octets {
        let value = parts.next().ok_or(LoopbackAuthorityError)?;
        if value.is_empty()
            || !value.bytes().all(|byte| byte.is_ascii_digit())
            || (value.len() > 1 && value.starts_with('0'))
        {
            return Err(LoopbackAuthorityError);
        }
        *octet = value.parse().map_err(|_| LoopbackAuthorityError)?;
    }
    if parts.next().is_some() || octets[0] != 127 {
        return Err(LoopbackAuthorityError);
    }
    Ok(octets)
}

/// A 64-byte lowercase hexadecimal broker credential.
#[derive(Clone, PartialEq, Eq)]
pub struct BrokerToken(String);
impl fmt::Debug for BrokerToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("BrokerToken([REDACTED])")
    }
}
/// Why a broker credential is malformed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrokerTokenError;
impl fmt::Display for BrokerTokenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid broker token")
    }
}
impl std::error::Error for BrokerTokenError {}
impl BrokerToken {
    /// Validates a credential received or generated at the host boundary.
    ///
    /// # Errors
    ///
    /// Returns an error when the credential is not 64 lowercase hexadecimal bytes.
    pub fn parse(value: impl Into<String>) -> Result<Self, BrokerTokenError> {
        let value = value.into();
        if value.len() != 64
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            return Err(BrokerTokenError);
        }
        Ok(Self(value))
    }
    /// Returns the credential bytes for constant-time comparison at the host boundary.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

/// A parsed broker request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrokerRequest {
    Ready(BrokerToken),
    Connect {
        token: BrokerToken,
        authority: LoopbackAuthority,
    },
}
/// Why a broker request frame is invalid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrokerFrameError {
    Oversized,
    Malformed,
}
impl fmt::Display for BrokerFrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid broker frame")
    }
}
impl std::error::Error for BrokerFrameError {}
/// Parses one complete newline-terminated broker request frame.
///
/// # Errors
///
/// Returns an error when the frame is oversized or does not have the required shape.
pub fn parse_broker_request(frame: &[u8]) -> Result<BrokerRequest, BrokerFrameError> {
    if frame.len() > MAX_BROKER_FRAME_SIZE {
        return Err(BrokerFrameError::Oversized);
    }
    let frame = frame
        .strip_suffix(b"\n")
        .ok_or(BrokerFrameError::Malformed)?;
    if frame.contains(&b'\n') {
        return Err(BrokerFrameError::Malformed);
    }
    let frame = std::str::from_utf8(frame).map_err(|_| BrokerFrameError::Malformed)?;
    let fields: Vec<_> = frame.split(' ').collect();
    match fields.as_slice() {
        ["VHRN-BROKER/1", "READY", token] => BrokerToken::parse(*token)
            .map(BrokerRequest::Ready)
            .map_err(|_| BrokerFrameError::Malformed),
        ["VHRN-BROKER/1", "CONNECT", token, raw_authority] => {
            let authority =
                LoopbackAuthority::parse(raw_authority).map_err(|_| BrokerFrameError::Malformed)?;
            if authority.to_string() != *raw_authority {
                return Err(BrokerFrameError::Malformed);
            }
            Ok(BrokerRequest::Connect {
                token: BrokerToken::parse(*token).map_err(|_| BrokerFrameError::Malformed)?,
                authority,
            })
        }
        _ => Err(BrokerFrameError::Malformed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn domain_corpus() {
        for row in include_str!("../../../testdata/domain-policy.tsv")
            .lines()
            .filter(|row| !row.starts_with('#'))
        {
            let fields: Vec<_> = row.split('\t').collect();
            match fields.as_slice() {
                ["entry", input, normalized, _, outcome] => assert_eq!(
                    normalize_domain_entry(input).ok().as_deref(),
                    (*outcome == "accept").then_some(*normalized)
                ),
                ["host", input, normalized, entry, outcome] => {
                    let host = normalize_domain_host(input).ok();
                    assert_eq!(
                        host.as_deref(),
                        (!normalized.is_empty()).then_some(*normalized)
                    );
                    assert_eq!(
                        host.is_some_and(|host| domain_entry_matches(entry, &host)),
                        *outcome == "allow"
                    );
                }
                _ => panic!("invalid corpus row: {row}"),
            }
        }
    }
    #[test]
    fn loopback_authority_corpus() {
        for row in include_str!("../../../testdata/loopback-authorities.tsv")
            .lines()
            .filter(|row| !row.starts_with('#'))
        {
            let fields: Vec<_> = row.split('\t').collect();
            match fields.as_slice() {
                ["valid", input, canonical] => assert_eq!(
                    LoopbackAuthority::parse(input).unwrap().to_string(),
                    *canonical
                ),
                ["invalid", input] => assert!(LoopbackAuthority::parse(input).is_err()),
                _ => panic!("invalid corpus row: {row}"),
            }
        }
    }
    #[test]
    fn mode_corpus() {
        for row in include_str!("../../../testdata/proxy-modes.tsv")
            .lines()
            .filter(|row| !row.starts_with('#'))
        {
            let fields: Vec<_> = row.split('\t').collect();
            let [stored, _, _, _, _, effective] = fields.as_slice() else {
                panic!("invalid corpus row: {row}")
            };
            if *stored == "unknown" || fields[1] == "valid" {
                assert_eq!(Mode::readable_or_enforce(stored).as_str(), *effective);
            }
        }
    }
    #[test]
    fn broker_frame_corpus() {
        let token = "a".repeat(64);
        for row in include_str!("../../../testdata/broker-frames.tsv")
            .lines()
            .filter(|row| !row.starts_with('#'))
        {
            let fields: Vec<_> = row.split('\t').collect();
            let [_kind, wire, _, _, _outcome] = fields.as_slice() else {
                panic!("invalid corpus row: {row}")
            };
            let frame = wire.replace("<token>", &token).replace("\\n", "\n");
            assert!(parse_broker_request(frame.as_bytes()).is_ok());
        }
    }

    #[test]
    fn broker_frames_reject_invalid_shapes() {
        let token = "a".repeat(64);
        let valid = format!("VHRN-BROKER/1 READY {token}\n");
        let invalid = [
            "VHRN-BROKER/1 READY a\n".to_string(),
            format!("VHRN-BROKER/1 READY {}\n", "A".repeat(64)),
            format!("VHRN-BROKER/1 READY {}\n", "g".repeat(64)),
            format!("VHRN-BROKER/1 READY {}\n", "a".repeat(63)),
            format!("VHRN-BROKER/1 READY {}\n", "a".repeat(65)),
            valid.trim_end().to_string(),
            format!("VHRN-BROKER/1 READY {token} extra\n"),
            format!("VHRN-BROKER/1 CONNECT {token} LOCALHOST:80\n"),
            format!("VHRN-BROKER/1 CONNECT {token} 127.0.0.1:080\n"),
            "x".repeat(MAX_BROKER_FRAME_SIZE + 1),
        ];
        for frame in invalid {
            assert!(parse_broker_request(frame.as_bytes()).is_err(), "{frame:?}");
        }
    }

    #[test]
    fn broker_debug_omits_credentials() {
        let credential = "b".repeat(64);
        let token = BrokerToken::parse(credential.clone()).unwrap();
        let request =
            parse_broker_request(format!("VHRN-BROKER/1 READY {credential}\n").as_bytes()).unwrap();
        assert!(!format!("{token:?}").contains(&credential));
        assert!(!format!("{request:?}").contains(&credential));
    }
}
