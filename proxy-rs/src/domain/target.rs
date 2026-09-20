//! Pure request-target parsing and canonical destination identities.

use std::{
    fmt,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    str::FromStr,
};

use hyper::{Method, Uri};

use crate::domain::policy::DomainName;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthorityParseError {
    InvalidHost,
    MissingPort,
    InvalidPort,
    NonLoopback,
}

impl fmt::Display for AuthorityParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidHost => "invalid authority host",
            Self::MissingPort => "authority is missing a port",
            Self::InvalidPort => "authority has an invalid port",
            Self::NonLoopback => "authority is not loopback",
        })
    }
}

impl std::error::Error for AuthorityParseError {}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct LoopbackAuthority {
    host: LoopbackHost,
    port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum LoopbackHost {
    Localhost,
    Ipv4([u8; 4]),
    Ipv6Loopback,
}

impl LoopbackAuthority {
    #[allow(dead_code)]
    pub(crate) fn parse(input: &str) -> Result<Self, AuthorityParseError> {
        input.parse()
    }
}

impl FromStr for LoopbackAuthority {
    type Err = AuthorityParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        if input.is_empty() || input.trim() != input || !input.is_ascii() {
            return Err(AuthorityParseError::InvalidHost);
        }
        let (host, port) = input
            .rsplit_once(':')
            .ok_or(AuthorityParseError::MissingPort)?;
        let port = parse_port(port)?;
        let host = if host.eq_ignore_ascii_case("localhost") {
            LoopbackHost::Localhost
        } else if let Some(value) = host.strip_prefix('[').and_then(|v| v.strip_suffix(']')) {
            let value = Ipv6Addr::from_str(value).map_err(|_| AuthorityParseError::InvalidHost)?;
            if !value.is_loopback() {
                return Err(AuthorityParseError::NonLoopback);
            }
            LoopbackHost::Ipv6Loopback
        } else if let Some(value) = parse_ipv4(host) {
            if value[0] != 127 {
                return Err(AuthorityParseError::NonLoopback);
            }
            LoopbackHost::Ipv4(value)
        } else {
            return Err(AuthorityParseError::InvalidHost);
        };
        Ok(Self { host, port })
    }
}

impl fmt::Display for LoopbackAuthority {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.host {
            LoopbackHost::Localhost => write!(formatter, "localhost:{}", self.port),
            LoopbackHost::Ipv4(octets) => write!(
                formatter,
                "{}.{}.{}.{}:{}",
                octets[0], octets[1], octets[2], octets[3], self.port
            ),
            LoopbackHost::Ipv6Loopback => write!(formatter, "[::1]:{}", self.port),
        }
    }
}

fn parse_ipv4(value: &str) -> Option<[u8; 4]> {
    let mut octets = [0; 4];
    let mut parts = value.split('.');
    for octet in &mut octets {
        let value = parts.next()?;
        if value.is_empty()
            || !value.bytes().all(|byte| byte.is_ascii_digit())
            || (value.len() > 1 && value.starts_with('0'))
        {
            return None;
        }
        *octet = value.parse().ok()?;
    }
    (parts.next().is_none()).then_some(octets)
}

fn parse_port(value: &str) -> Result<u16, AuthorityParseError> {
    if value.is_empty() {
        return Err(AuthorityParseError::MissingPort);
    }
    if !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(AuthorityParseError::InvalidPort);
    }
    value
        .parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
        .ok_or(AuthorityParseError::InvalidPort)
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum PublicHost {
    Dns(DomainName),
    Ip(IpAddr),
}

impl fmt::Display for PublicHost {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Dns(name) => name.fmt(formatter),
            Self::Ip(address) => address.fmt(formatter),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Target {
    Direct(DirectTarget),
    Asterisk,
    PublicHttp(PublicTarget),
    LocalHttp(LocalTarget),
    PublicConnect(PublicTarget),
    LocalConnect(LocalTarget),
    HttpsAbsoluteRejected,
    Malformed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DirectTarget {
    path_and_query: Vec<u8>,
}

impl DirectTarget {
    pub(crate) fn path_and_query(&self) -> &[u8] {
        &self.path_and_query
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PublicTarget {
    authority: String,
    host: PublicHost,
    port: u16,
    explicit_port: bool,
    path_and_query: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocalTarget {
    canonical_authority: LoopbackAuthority,
    explicit_port: bool,
    path_and_query: Vec<u8>,
}

impl PublicTarget {
    pub(crate) fn authority(&self) -> &str {
        &self.authority
    }

    pub(crate) fn host(&self) -> &PublicHost {
        &self.host
    }

    pub(crate) const fn port(&self) -> u16 {
        self.port
    }

    #[allow(dead_code)]
    pub(crate) const fn explicit_port(&self) -> bool {
        self.explicit_port
    }

    #[allow(dead_code)]
    pub(crate) fn path_and_query(&self) -> &[u8] {
        &self.path_and_query
    }
}

impl LocalTarget {
    pub(crate) fn canonical_authority(&self) -> &LoopbackAuthority {
        &self.canonical_authority
    }

    #[allow(dead_code)]
    pub(crate) const fn explicit_port(&self) -> bool {
        self.explicit_port
    }

    #[allow(dead_code)]
    pub(crate) fn path_and_query(&self) -> &[u8] {
        &self.path_and_query
    }
}

impl fmt::Display for PublicTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.authority)
    }
}

impl fmt::Display for LocalTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.canonical_authority.fmt(formatter)
    }
}

#[derive(Clone, Copy)]
enum RequestKind {
    Http,
    Connect,
}

#[derive(Clone, Copy)]
enum AbsoluteScheme {
    Http,
    Https,
}

/// Parses one request target from the bytes present on the request line.
#[must_use]
pub(crate) fn classify(method: &Method, raw_target: &[u8]) -> Target {
    if method == Method::CONNECT {
        return classify_connect(raw_target);
    }
    if raw_target == b"*" {
        return Target::Asterisk;
    }
    if raw_target.starts_with(b"/") {
        return if valid_path_and_query(raw_target) {
            Target::Direct(DirectTarget {
                path_and_query: raw_target.to_vec(),
            })
        } else {
            Target::Malformed
        };
    }
    classify_absolute(raw_target)
}

/// Bridges the pre-Phase-6 Hyper listener without formatting its parsed URI.
///
/// Hyper retains the authority and path/query octets independently. Phase 6 replaces this
/// component bridge when the raw request-line codec becomes responsible for ingress.
#[must_use]
pub(crate) fn classify_parsed(method: &Method, uri: &Uri) -> Target {
    if method == Method::CONNECT {
        return match (uri.scheme(), uri.authority(), uri.path_and_query()) {
            (None, Some(authority), None) => classify(method, authority.as_str().as_bytes()),
            _ => Target::Malformed,
        };
    }

    if uri.scheme().is_none() && uri.authority().is_none() {
        return match uri.path_and_query() {
            Some(value) => classify(method, value.as_str().as_bytes()),
            _ => Target::Malformed,
        };
    }

    let scheme = match uri.scheme_str() {
        Some(value) if value.eq_ignore_ascii_case("http") => AbsoluteScheme::Http,
        Some(value) if value.eq_ignore_ascii_case("https") => AbsoluteScheme::Https,
        _ => return Target::Malformed,
    };
    let Some(authority) = uri.authority() else {
        return Target::Malformed;
    };
    let path_and_query = uri
        .path_and_query()
        .map_or_else(Vec::new, |value| value.as_str().as_bytes().to_vec());
    if !path_and_query.is_empty() && !valid_absolute_path_and_query(&path_and_query) {
        return Target::Malformed;
    }
    classify_authority(
        authority.as_str(),
        Some(scheme),
        RequestKind::Http,
        path_and_query,
    )
}

fn classify_connect(raw_target: &[u8]) -> Target {
    let Ok(authority) = std::str::from_utf8(raw_target) else {
        return Target::Malformed;
    };
    if authority.is_empty()
        || authority.bytes().any(|byte| {
            byte.is_ascii_whitespace()
                || byte.is_ascii_control()
                || matches!(byte, b'/' | b'?' | b'#' | b'@')
        })
    {
        return Target::Malformed;
    }
    classify_authority(authority, None, RequestKind::Connect, Vec::new())
}

fn classify_absolute(raw_target: &[u8]) -> Target {
    if !valid_uri_octets(raw_target) || raw_target.contains(&b'#') {
        return Target::Malformed;
    }
    let Some(colon) = raw_target.iter().position(|byte| *byte == b':') else {
        return Target::Malformed;
    };
    let scheme = &raw_target[..colon];
    if scheme.is_empty()
        || !scheme[0].is_ascii_alphabetic()
        || !scheme
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'))
    {
        return Target::Malformed;
    }
    let scheme = if scheme.eq_ignore_ascii_case(b"http") {
        AbsoluteScheme::Http
    } else if scheme.eq_ignore_ascii_case(b"https") {
        AbsoluteScheme::Https
    } else {
        return Target::Malformed;
    };
    let remainder = &raw_target[colon + 1..];
    let Some(remainder) = remainder.strip_prefix(b"//") else {
        return Target::Malformed;
    };
    let authority_end = remainder
        .iter()
        .position(|byte| matches!(byte, b'/' | b'?'))
        .unwrap_or(remainder.len());
    let Ok(authority) = std::str::from_utf8(&remainder[..authority_end]) else {
        return Target::Malformed;
    };
    let path_and_query = &remainder[authority_end..];
    if authority.is_empty()
        || (!path_and_query.is_empty() && !valid_absolute_path_and_query(path_and_query))
    {
        return Target::Malformed;
    }
    classify_authority(
        authority,
        Some(scheme),
        RequestKind::Http,
        path_and_query.to_vec(),
    )
}

fn classify_authority(
    value: &str,
    scheme: Option<AbsoluteScheme>,
    kind: RequestKind,
    path_and_query: Vec<u8>,
) -> Target {
    let default_port = scheme.map(|value| match value {
        AbsoluteScheme::Http => 80,
        AbsoluteScheme::Https => 443,
    });
    let Some(parsed) = parse_authority(value, default_port, matches!(kind, RequestKind::Connect))
    else {
        return Target::Malformed;
    };

    if matches!(scheme, Some(AbsoluteScheme::Https)) {
        return Target::HttpsAbsoluteRejected;
    }

    match parsed.host {
        ParsedHost::Local(host) => {
            let target = LocalTarget {
                canonical_authority: LoopbackAuthority {
                    host,
                    port: parsed.port,
                },
                explicit_port: parsed.explicit_port,
                path_and_query,
            };
            match kind {
                RequestKind::Http => Target::LocalHttp(target),
                RequestKind::Connect => Target::LocalConnect(target),
            }
        }
        ParsedHost::Public(host) => {
            let authority = canonical_public_authority(&host, parsed.port, parsed.explicit_port);
            let target = PublicTarget {
                authority,
                host,
                port: parsed.port,
                explicit_port: parsed.explicit_port,
                path_and_query,
            };
            match kind {
                RequestKind::Http => Target::PublicHttp(target),
                RequestKind::Connect => Target::PublicConnect(target),
            }
        }
    }
}

struct ParsedAuthority {
    host: ParsedHost,
    port: u16,
    explicit_port: bool,
}

enum ParsedHost {
    Local(LoopbackHost),
    Public(PublicHost),
}

fn parse_authority(
    value: &str,
    default_port: Option<u16>,
    port_required: bool,
) -> Option<ParsedAuthority> {
    if value.is_empty()
        || !value.is_ascii()
        || value.bytes().any(|byte| {
            byte.is_ascii_whitespace()
                || byte.is_ascii_control()
                || matches!(byte, b'@' | b'/' | b'?' | b'#')
        })
    {
        return None;
    }

    if let Some(value) = value.strip_prefix('[') {
        let close = value.find(']')?;
        let host = &value[..close];
        let suffix = &value[close + 1..];
        let (port, explicit_port) = parse_authority_port(suffix, default_port, port_required)?;
        if host.contains('%') {
            return None;
        }
        let address = host.parse::<Ipv6Addr>().ok()?;
        if address.to_ipv4_mapped().is_some() {
            return None;
        }
        let host = if address.is_loopback() {
            ParsedHost::Local(LoopbackHost::Ipv6Loopback)
        } else {
            ParsedHost::Public(PublicHost::Ip(IpAddr::V6(address)))
        };
        return Some(ParsedAuthority {
            host,
            port,
            explicit_port,
        });
    }

    if value.contains(['[', ']']) || value.matches(':').count() > 1 {
        return None;
    }
    let (host, suffix) = match value.rsplit_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (value, None),
    };
    let (port, explicit_port) = match suffix {
        Some(port) => (parse_port(port).ok()?, true),
        None if port_required => return None,
        None => (default_port?, false),
    };
    if host.is_empty() {
        return None;
    }

    if let Some(octets) = parse_ipv4(host) {
        let host = if octets[0] == 127 {
            ParsedHost::Local(LoopbackHost::Ipv4(octets))
        } else {
            ParsedHost::Public(PublicHost::Ip(IpAddr::V4(Ipv4Addr::from(octets))))
        };
        return Some(ParsedAuthority {
            host,
            port,
            explicit_port,
        });
    }
    if host
        .bytes()
        .all(|byte| byte.is_ascii_digit() || byte == b'.')
    {
        return None;
    }
    let domain = host.parse::<DomainName>().ok()?;
    let host = if domain.to_string() == "localhost" {
        ParsedHost::Local(LoopbackHost::Localhost)
    } else {
        ParsedHost::Public(PublicHost::Dns(domain))
    };
    Some(ParsedAuthority {
        host,
        port,
        explicit_port,
    })
}

fn parse_authority_port(
    suffix: &str,
    default_port: Option<u16>,
    port_required: bool,
) -> Option<(u16, bool)> {
    if let Some(port) = suffix.strip_prefix(':') {
        Some((parse_port(port).ok()?, true))
    } else if suffix.is_empty() && !port_required {
        Some((default_port?, false))
    } else {
        None
    }
}

fn canonical_public_authority(host: &PublicHost, port: u16, explicit_port: bool) -> String {
    let host = match host {
        PublicHost::Dns(name) => name.to_string(),
        PublicHost::Ip(IpAddr::V4(address)) => address.to_string(),
        PublicHost::Ip(IpAddr::V6(address)) => format!("[{address}]"),
    };
    if explicit_port {
        format!("{host}:{port}")
    } else {
        host
    }
}

fn valid_absolute_path_and_query(value: &[u8]) -> bool {
    matches!(value.first(), Some(b'/' | b'?')) && valid_path_and_query(value)
}

fn valid_path_and_query(value: &[u8]) -> bool {
    !value.contains(&b'#')
        && !value.contains(&b'[')
        && !value.contains(&b']')
        && valid_uri_octets(value)
}

fn valid_uri_octets(value: &[u8]) -> bool {
    let mut index = 0;
    while index < value.len() {
        let byte = value[index];
        if byte == b'%' {
            if index + 2 >= value.len()
                || !value[index + 1].is_ascii_hexdigit()
                || !value[index + 2].is_ascii_hexdigit()
            {
                return false;
            }
            index += 3;
            continue;
        }
        if !(byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'!' | b'$'
                    | b'&'
                    | b'\''
                    | b'('
                    | b')'
                    | b'*'
                    | b'+'
                    | b','
                    | b'-'
                    | b'.'
                    | b'/'
                    | b':'
                    | b';'
                    | b'='
                    | b'?'
                    | b'@'
                    | b'_'
                    | b'~'
                    | b'['
                    | b']'
            ))
        {
            return false;
        }
        index += 1;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify_text(method: &Method, target: &str) -> Target {
        classify(method, target.as_bytes())
    }

    #[test]
    fn accepted_target_truth_table_is_canonical_and_disjoint() {
        let cases = [
            ("GET", "/healthz?probe=1", "direct"),
            ("OPTIONS", "*", "asterisk"),
            ("GET", "http://Example.COM./path?q=1", "public-http"),
            ("GET", "http://example.com:0080?x=1", "public-http"),
            ("GET", "http://LOCALHOST/path", "local-http"),
            ("GET", "http://127.0.0.2:8080/", "local-http"),
            ("GET", "http://[0:0:0:0:0:0:0:1]/", "local-http"),
            ("CONNECT", "example.com:443", "public-connect"),
            ("CONNECT", "[2606:4700:4700::1111]:443", "public-connect"),
            ("CONNECT", "LOCALHOST:0443", "local-connect"),
            ("CONNECT", "127.1.2.3:443", "local-connect"),
            ("CONNECT", "[::1]:443", "local-connect"),
            ("GET", "https://example.com/path", "https-rejected"),
        ];
        for (method, raw, expected) in cases {
            let method = Method::from_bytes(method.as_bytes()).unwrap();
            let actual = match classify_text(&method, raw) {
                Target::Direct(_) => "direct",
                Target::Asterisk => "asterisk",
                Target::PublicHttp(_) => "public-http",
                Target::LocalHttp(_) => "local-http",
                Target::PublicConnect(_) => "public-connect",
                Target::LocalConnect(_) => "local-connect",
                Target::HttpsAbsoluteRejected => "https-rejected",
                Target::Malformed => "malformed",
            };
            assert_eq!(actual, expected, "{method} {raw}");
        }
    }

    #[test]
    fn shared_http_outcome_fixture_uses_the_raw_target_classifier() {
        for row in include_str!("../../testdata/proxy-http-cases.tsv")
            .lines()
            .filter(|row| !row.starts_with('#'))
        {
            let fields: Vec<_> = row.split('\t').collect();
            let [class, raw, ..] = fields.as_slice() else {
                panic!("bad fixture row: {row}");
            };
            if raw.contains("<local-port>")
                || *raw == "missing authority"
                || *class == "tunnel-relay"
            {
                continue;
            }
            let method = if class.contains("connect") {
                Method::CONNECT
            } else {
                Method::GET
            };
            let target = classify(&method, raw.as_bytes());
            let expected = match *class {
                _ if raw.starts_with('/') => matches!(target, Target::Direct(_)),
                "public-http" | "public-http-stream" | "public-http-cancel"
                | "public-http-pool" => matches!(target, Target::PublicHttp(_)),
                "public-connect" => matches!(target, Target::PublicConnect(_)),
                "malformed-connect" => target == Target::Malformed,
                "https-rejected" => target == Target::HttpsAbsoluteRejected,
                _ => true,
            };
            assert!(expected, "{row}: {target:?}");
        }
    }

    #[test]
    fn http_targets_retain_path_port_and_canonical_authority() {
        let Target::PublicHttp(implicit) =
            classify_text(&Method::GET, "http://Example.COM./path?q=one")
        else {
            panic!("public HTTP target");
        };
        assert_eq!(implicit.host().to_string(), "example.com");
        assert_eq!(implicit.authority(), "example.com");
        assert_eq!(implicit.port(), 80);
        assert!(!implicit.explicit_port());
        assert_eq!(implicit.path_and_query(), b"/path?q=one");

        let Target::PublicHttp(explicit) =
            classify_text(&Method::GET, "http://example.com:00080?query")
        else {
            panic!("public HTTP target");
        };
        assert_eq!(explicit.authority(), "example.com:80");
        assert_eq!(explicit.port(), 80);
        assert!(explicit.explicit_port());
        assert_eq!(explicit.path_and_query(), b"?query");
    }

    #[test]
    fn connect_requires_one_explicit_canonicalizable_port() {
        for raw in [
            "example.com",
            "example.com:",
            "example.com:0",
            "example.com:65536",
            "example.com:not-a-port",
            "http://example.com:443",
            "user@example.com:443",
            "example.com:443/path",
            "example.com:443?query",
            "example.com:443#fragment",
            "2001:db8::1:443",
            "[2001:db8::1]",
            "[2001:db8::1]:443:444",
        ] {
            assert_eq!(
                classify_text(&Method::CONNECT, raw),
                Target::Malformed,
                "{raw}"
            );
        }
        let Target::PublicConnect(target) = classify_text(&Method::CONNECT, "example.com:000443")
        else {
            panic!("canonicalizable port");
        };
        assert_eq!(target.port(), 443);
        assert_eq!(target.authority(), "example.com:443");
        assert!(target.explicit_port());
    }

    #[test]
    fn dns_lengths_root_dot_and_ascii_alabels_are_enforced() {
        let label63 = "a".repeat(63);
        let maximum = format!("{label63}.{label63}.{label63}.{}", "a".repeat(61));
        assert_eq!(maximum.len(), 253);
        assert!(matches!(
            classify_text(&Method::GET, &format!("http://{maximum}./")),
            Target::PublicHttp(_)
        ));
        for host in [
            format!("{}.example", "a".repeat(64)),
            format!("{maximum}a"),
            "example..com".to_owned(),
            "example.com..".to_owned(),
            "-bad.example".to_owned(),
            "bad-.example".to_owned(),
            "bad_name.example".to_owned(),
            "café.example".to_owned(),
            "bad\texample".to_owned(),
        ] {
            assert_eq!(
                classify_text(&Method::GET, &format!("http://{host}/")),
                Target::Malformed,
                "{host}"
            );
        }
        let Target::PublicHttp(target) =
            classify_text(&Method::GET, "http://XN--BCHER-KVA.Example./")
        else {
            panic!("ASCII A-label");
        };
        assert_eq!(target.host().to_string(), "xn--bcher-kva.example");
    }

    #[test]
    fn numeric_hosts_reject_ambiguous_ipv4_mapped_and_scoped_forms() {
        for raw in [
            "http://127.00.0.1/",
            "http://127.0.0.1./",
            "http://001.2.3.4/",
            "http://192.0.2.1./",
            "http://256.1.1.1/",
            "http://1.2.3/",
            "http://[::ffff:127.0.0.1]/",
            "http://[fe80::1%25eth0]/",
            "http://2001:db8::1/",
        ] {
            assert_eq!(classify_text(&Method::GET, raw), Target::Malformed, "{raw}");
        }
        assert!(matches!(
            classify_text(&Method::GET, "http://192.0.2.1/"),
            Target::PublicHttp(_)
        ));
        assert!(matches!(
            classify_text(&Method::GET, "http://[2606:4700:4700::1111]/"),
            Target::PublicHttp(_)
        ));
        for raw in ["127.0.0.1.:443", "192.0.2.1.:443"] {
            assert_eq!(
                classify_text(&Method::CONNECT, raw),
                Target::Malformed,
                "{raw}"
            );
        }
    }

    #[test]
    fn local_spellings_normalize_before_public_classification() {
        let cases = [
            ("http://LOCALHOST./", "localhost:80"),
            ("http://127.0.0.255:0080/", "127.0.0.255:80"),
            ("http://[0:0:0:0:0:0:0:1]:80/", "[::1]:80"),
        ];
        for (raw, expected) in cases {
            let Target::LocalHttp(target) = classify_text(&Method::GET, raw) else {
                panic!("local target: {raw}");
            };
            assert_eq!(target.canonical_authority().to_string(), expected);
        }
    }

    #[test]
    fn malformed_absolute_forms_do_not_fall_back_to_direct_or_connect() {
        for raw in [
            "example.com/path",
            "http:/example.com/",
            "http:///path",
            "http://user@example.com/",
            "http://example.com/#fragment",
            "http://example.com/%zz",
            "ftp://example.com/",
            "https://example.com:0/",
            "http://example.com/[segment]",
            "http://example.com/path?[query]",
            "/path/[segment]",
            "/path?[query]",
        ] {
            assert_eq!(classify_text(&Method::GET, raw), Target::Malformed, "{raw}");
        }
    }

    #[test]
    fn loopback_authority_fixture_is_preserved() {
        for row in include_str!("../../../shared/testdata/loopback-authorities.tsv")
            .lines()
            .filter(|row| !row.starts_with('#'))
        {
            let fields: Vec<_> = row.split('\t').collect();
            match fields.as_slice() {
                ["valid", input, canonical] => assert_eq!(
                    input.parse::<LoopbackAuthority>().unwrap().to_string(),
                    *canonical,
                    "{input}"
                ),
                ["invalid", input] => {
                    assert!(input.parse::<LoopbackAuthority>().is_err(), "{input}");
                }
                _ => panic!("bad fixture row: {row}"),
            }
        }
    }

    #[test]
    fn domain_grammar_separates_wire_and_policy_records() {
        use crate::domain::policy::DomainPattern;

        assert!(" api.example".parse::<DomainName>().is_err());
        assert!("a_b.example".parse::<DomainName>().is_err());
        assert!("-a.example".parse::<DomainName>().is_err());
        assert!("a-.example".parse::<DomainName>().is_err());
        let pattern = " *.example. ".parse::<DomainPattern>().unwrap();
        assert!(pattern.matches(&"api.example".parse().unwrap()));
    }
}
