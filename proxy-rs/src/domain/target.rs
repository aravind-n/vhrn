//! Request-target classification with validated destination identities.

use crate::domain::policy::DomainName;
use hyper::{Method, Uri};
use std::{
    fmt,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    str::FromStr,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthorityParseError {
    InvalidHost,
    MissingPort,
    InvalidPort,
    NonLoopback,
}
impl fmt::Display for AuthorityParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidHost => "invalid authority host",
            Self::MissingPort => "authority is missing a port",
            Self::InvalidPort => "authority has an invalid port",
            Self::NonLoopback => "authority is not loopback",
        })
    }
}
impl std::error::Error for AuthorityParseError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LocalTlsIdentity {
    DnsLocalhost,
    Ip(IpAddr),
}
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
    #[must_use]
    pub(crate) fn tls_identity(&self) -> LocalTlsIdentity {
        match self.host {
            LoopbackHost::Localhost => LocalTlsIdentity::DnsLocalhost,
            LoopbackHost::Ipv4(o) => LocalTlsIdentity::Ip(IpAddr::V4(Ipv4Addr::from(o))),
            LoopbackHost::Ipv6Loopback => LocalTlsIdentity::Ip(IpAddr::V6(Ipv6Addr::LOCALHOST)),
        }
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
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.host {
            LoopbackHost::Localhost => write!(f, "localhost:{}", self.port),
            LoopbackHost::Ipv4(o) => write!(f, "{}.{}.{}.{}:{}", o[0], o[1], o[2], o[3], self.port),
            LoopbackHost::Ipv6Loopback => write!(f, "[::1]:{}", self.port),
        }
    }
}
fn parse_ipv4(value: &str) -> Option<[u8; 4]> {
    let mut octets = [0; 4];
    let mut parts = value.split('.');
    for octet in &mut octets {
        let value = parts.next()?;
        if value.is_empty()
            || !value.bytes().all(|b| b.is_ascii_digit())
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
    if !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(AuthorityParseError::InvalidPort);
    }
    value
        .parse::<u16>()
        .ok()
        .filter(|p| *p != 0)
        .ok_or(AuthorityParseError::InvalidPort)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Scheme {
    Http,
    Https,
}
impl Scheme {
    #[must_use]
    pub(crate) const fn secure(self) -> bool {
        matches!(self, Self::Https)
    }
    const fn default_port(self) -> u16 {
        match self {
            Self::Http => 80,
            Self::Https => 443,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum PublicHost {
    Dns(DomainName),
    Ip(IpAddr),
}
impl fmt::Display for PublicHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Dns(x) => x.fmt(f),
            Self::Ip(x) => x.fmt(f),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Target {
    PublicHttp(PublicTarget),
    PublicConnect(PublicTarget),
    LocalHttp(LocalTarget),
    LocalConnect(LocalTarget),
    Direct(String),
    Malformed,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PublicTarget {
    authority: String,
    host: PublicHost,
    port: u16,
    scheme: Scheme,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocalTarget {
    authority: String,
    canonical_authority: LoopbackAuthority,
    scheme: Scheme,
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
    pub(crate) const fn secure(&self) -> bool {
        self.scheme.secure()
    }
}
impl LocalTarget {
    pub(crate) fn canonical_authority(&self) -> &LoopbackAuthority {
        &self.canonical_authority
    }
    pub(crate) const fn secure(&self) -> bool {
        self.scheme.secure()
    }
}
impl fmt::Display for PublicTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.authority)
    }
}
impl fmt::Display for LocalTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.authority)
    }
}

/// Classifies parsed HTTP request parts without stringifying the URI.
#[must_use]
pub(crate) fn classify(method: &Method, uri: &Uri) -> Target {
    if method == Method::CONNECT {
        return uri.authority().map_or(Target::Malformed, |a| {
            classify_authority(a.as_str(), Scheme::Https, true)
        });
    }
    if uri.scheme().is_none() {
        return uri
            .path_and_query()
            .filter(|p| p.as_str().starts_with('/'))
            .map_or(Target::Malformed, |p| Target::Direct(p.as_str().to_owned()));
    }
    let scheme = match uri.scheme_str() {
        Some("http") => Scheme::Http,
        Some("https") => Scheme::Https,
        _ => return Target::Malformed,
    };
    uri.authority().map_or(Target::Malformed, |a| {
        classify_authority(a.as_str(), scheme, false)
    })
}
fn classify_authority(value: &str, scheme: Scheme, connect: bool) -> Target {
    if value.is_empty() || value.trim() != value || value.contains(['@', '?', '#']) {
        return Target::Malformed;
    }
    if let Ok(local) = value.parse::<LoopbackAuthority>() {
        let target = LocalTarget {
            authority: value.to_owned(),
            canonical_authority: local,
            scheme,
        };
        return if connect {
            Target::LocalConnect(target)
        } else {
            Target::LocalHttp(target)
        };
    }
    let Some((host, port)) = parse_public_authority(value, scheme.default_port()) else {
        return Target::Malformed;
    };
    let target = PublicTarget {
        authority: value.to_owned(),
        host,
        port,
        scheme,
    };
    if connect {
        Target::PublicConnect(target)
    } else {
        Target::PublicHttp(target)
    }
}
fn parse_public_authority(value: &str, default_port: u16) -> Option<(PublicHost, u16)> {
    let authority: hyper::http::uri::Authority = value.parse().ok()?;
    if authority.as_str() != value {
        return None;
    }
    let port = authority.port_u16().unwrap_or(default_port);
    if port == 0 {
        return None;
    }
    let host = authority.host();
    let numeric_host = host
        .strip_prefix('[')
        .and_then(|v| v.strip_suffix(']'))
        .unwrap_or(host);
    let host = numeric_host
        .parse::<IpAddr>()
        .map(PublicHost::Ip)
        .or_else(|_| host.parse::<DomainName>().map(PublicHost::Dns))
        .ok()?;
    Some((host, port))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn c(method: &Method, text: &str) -> Target {
        text.parse()
            .map_or(Target::Malformed, |uri| classify(method, &uri))
    }
    #[test]
    fn query_and_ipv6_work() {
        assert!(matches!(
            c(&Method::GET, "http://example.com?x=1"),
            Target::PublicHttp(_)
        ));
        assert!(matches!(
            c(&Method::GET, "http://[2606:4700:4700::1111]/"),
            Target::PublicHttp(_)
        ));
    }
    #[test]
    fn identities_are_exact() {
        assert_eq!(
            "localhost:443"
                .parse::<LoopbackAuthority>()
                .unwrap()
                .tls_identity(),
            LocalTlsIdentity::DnsLocalhost
        );
        assert_eq!(
            "127.0.0.2:443"
                .parse::<LoopbackAuthority>()
                .unwrap()
                .tls_identity(),
            LocalTlsIdentity::Ip("127.0.0.2".parse().unwrap())
        );
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
    fn http_target_fixture_classifies_without_string_slicing() {
        for row in include_str!("../../testdata/proxy-http-cases.tsv")
            .lines()
            .filter(|row| !row.starts_with('#'))
        {
            let fields: Vec<_> = row.split('\t').collect();
            let [class, target, ..] = fields.as_slice() else {
                panic!("bad fixture row: {row}")
            };
            if target.contains("<local-port>")
                || *target == "missing authority"
                || *class == "tunnel-relay"
            {
                continue;
            }
            let method = if class.contains("connect") {
                Method::CONNECT
            } else {
                Method::GET
            };
            let target = c(&method, target);
            if *class == "local-http" {
                assert!(matches!(target, Target::LocalHttp(_)), "{row}");
            } else if *class == "local-connect" {
                assert!(matches!(target, Target::LocalConnect(_)), "{row}");
            } else if class.contains("connect") {
                assert!(matches!(target, Target::PublicConnect(_)), "{row}");
            } else if *class == "public-http" && fields[1].starts_with('/') {
                assert!(matches!(target, Target::Direct(_)), "{row}");
            } else if class.starts_with("public-http") {
                assert!(matches!(target, Target::PublicHttp(_)), "{row}");
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
        assert!(
            format!("{}.example", "a".repeat(64))
                .parse::<DomainName>()
                .is_err()
        );
        let pattern = " *.example. ".parse::<DomainPattern>().unwrap();
        assert!(pattern.matches(&"api.example".parse().unwrap()));
        for value in ["bad\t.example", "bad\r.example", "bad\n.example"] {
            assert!(value.parse::<DomainName>().is_err());
        }
    }
}
