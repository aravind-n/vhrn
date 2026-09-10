//! Request-target classification with no connector capability.

use std::fmt;

use vhrn_policy::{LoopbackAuthority, normalize_domain_host};

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
        for row in include_str!("../../testdata/proxy-http-cases.tsv")
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
