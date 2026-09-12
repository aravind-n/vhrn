//! Request-target classification with no connector capability.

use std::fmt;
use std::net::Ipv6Addr;
use std::str::FromStr;

use crate::policy::normalize_domain_host;

/// Proxy-owned canonical local target identity.
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
impl LoopbackAuthority {
    pub(crate) fn parse(input: &str) -> Result<Self, ()> {
        if input.is_empty() || input.trim() != input || !input.is_ascii() {
            return Err(());
        }
        let (host, port) = input.rsplit_once(':').ok_or(())?;
        let port = parse_port(port).ok_or(())?;
        let host = if host.eq_ignore_ascii_case("localhost") {
            LoopbackHost::Localhost
        } else if let Some(value) = host
            .strip_prefix('[')
            .and_then(|value| value.strip_suffix(']'))
        {
            let value = Ipv6Addr::from_str(value).map_err(|_| ())?;
            if !value.is_loopback() {
                return Err(());
            }
            LoopbackHost::Ipv6Loopback
        } else {
            LoopbackHost::Ipv4(parse_loopback_ipv4(host).ok_or(())?)
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
fn parse_loopback_ipv4(value: &str) -> Option<[u8; 4]> {
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
    (parts.next().is_none() && octets[0] == 127).then_some(octets)
}

/// A fully classified request destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    PublicHttp(PublicTarget),
    PublicConnect(PublicTarget),
    LocalHttp(LocalTarget),
    LocalConnect(LocalTarget),
    Direct(String),
    Malformed,
}
/// A normalized public name and port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicTarget {
    authority: String,
    host: String,
    port: u16,
    secure: bool,
}
/// A canonical local authority and request scheme.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalTarget {
    authority: String,
    canonical_authority: LoopbackAuthority,
    secure: bool,
}

impl PublicTarget {
    #[must_use]
    pub fn authority(&self) -> &str {
        &self.authority
    }
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }
    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }
    #[must_use]
    pub const fn secure(&self) -> bool {
        self.secure
    }
}
impl LocalTarget {
    #[must_use]
    pub fn authority(&self) -> &str {
        &self.authority
    }
    #[must_use]
    pub fn canonical_authority(&self) -> &LoopbackAuthority {
        &self.canonical_authority
    }
    #[must_use]
    pub const fn secure(&self) -> bool {
        self.secure
    }
}

/// Classifies an HTTP request target and method.
#[must_use]
pub fn classify(method: &str, request_target: &str) -> Target {
    if method.eq_ignore_ascii_case("CONNECT") {
        return classify_connect(request_target);
    }
    if request_target.starts_with('/') {
        return Target::Direct(request_target.to_owned());
    }
    let Some((secure, authority)) = request_target
        .strip_prefix("http://")
        .map(|v| (false, v))
        .or_else(|| request_target.strip_prefix("https://").map(|v| (true, v)))
    else {
        return Target::Malformed;
    };
    let authority = authority
        .split_once('/')
        .map_or(authority, |(value, _)| value);
    classify_authority(authority, if secure { 443 } else { 80 }, secure, false)
}
fn classify_connect(authority: &str) -> Target {
    classify_authority(authority, 443, false, true)
}
fn classify_authority(authority: &str, default_port: u16, secure: bool, connect: bool) -> Target {
    if let Ok(local) = LoopbackAuthority::parse(authority) {
        let target = LocalTarget {
            authority: authority.to_owned(),
            canonical_authority: local,
            secure,
        };
        return if connect {
            Target::LocalConnect(target)
        } else {
            Target::LocalHttp(target)
        };
    }
    let Some((host, port)) = parse_public_authority(authority, default_port) else {
        return Target::Malformed;
    };
    let target = PublicTarget {
        authority: authority.to_owned(),
        host,
        port,
        secure,
    };
    if connect {
        Target::PublicConnect(target)
    } else {
        Target::PublicHttp(target)
    }
}
fn parse_public_authority(value: &str, default_port: u16) -> Option<(String, u16)> {
    if value.is_empty() || value.contains(['@', '?', '#']) {
        return None;
    }
    let (host, port) = match value.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => (host, parse_port(port)?),
        Some(_) => return None,
        None => (value, default_port),
    };
    let host = normalize_domain_host(host).ok()?;
    Some((host, port))
}
fn parse_port(value: &str) -> Option<u16> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let port = value.parse().ok()?;
    (port != 0).then_some(port)
}
impl fmt::Display for PublicTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.authority)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn classifies_http_corpus() {
        for row in include_str!("../testdata/proxy-http-cases.tsv")
            .lines()
            .filter(|r| !r.starts_with('#'))
        {
            let fields: Vec<_> = row.split('\t').collect();
            let [class, input, ..] = fields.as_slice() else {
                panic!("bad row: {row}")
            };
            let result = match *class {
                "public-http" | "public-http-stream" | "public-http-cancel"
                | "public-http-pool" | "local-http" => classify("GET", input),
                "public-connect" | "local-connect" => classify("CONNECT", input),
                _ => continue,
            };
            if input.contains("missing authority") {
                assert_eq!(result, Target::Malformed);
            } else if *input == "/origin-form" {
                assert!(matches!(result, Target::Direct(_)));
            } else if !input.contains("<local-port>")
                && (*class == "local-http" || *class == "local-connect")
            {
                assert!(matches!(
                    result,
                    Target::LocalHttp(_) | Target::LocalConnect(_)
                ));
            } else if !input.contains("<local-port>") {
                assert!(
                    matches!(result, Target::PublicHttp(_) | Target::PublicConnect(_)),
                    "{input}"
                );
            }
        }
    }
    #[test]
    fn direct_paths_do_not_become_origins() {
        assert_eq!(
            classify("GET", "/healthz"),
            Target::Direct("/healthz".to_owned())
        );
    }
    #[test]
    fn preserves_authority_while_normalizing_public_identity() {
        let Target::PublicHttp(target) = classify("GET", "http://API.Example.COM.:0080/path")
        else {
            panic!()
        };
        assert_eq!(target.authority(), "API.Example.COM.:0080");
        assert_eq!(target.host(), "api.example.com");
        assert_eq!(target.port(), 80);
        assert!(!target.secure());
        let Target::PublicConnect(target) = classify("CONNECT", "api.example.com") else {
            panic!()
        };
        assert_eq!(target.authority(), "api.example.com");
        assert_eq!(target.port(), 443);
    }
    #[test]
    fn preserves_authority_while_canonicalizing_local_identity() {
        let Target::LocalHttp(target) = classify("GET", "http://LOCALHOST:00080/path") else {
            panic!()
        };
        assert_eq!(target.authority(), "LOCALHOST:00080");
        assert_eq!(target.canonical_authority().to_string(), "localhost:80");
        let Target::LocalConnect(target) = classify("CONNECT", "[0:0:0:0:0:0:0:1]:00443") else {
            panic!()
        };
        assert_eq!(target.authority(), "[0:0:0:0:0:0:0:1]:00443");
        assert_eq!(target.canonical_authority().to_string(), "[::1]:443");
    }
}
