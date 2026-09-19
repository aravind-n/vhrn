//! Startup configuration resolved from a small environment surface.

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

pub const DEFAULT_ALLOWLIST: &str = "/etc/vhrn/allowlist";
pub const DEFAULT_MODE_FILE: &str = "/etc/vhrn/mode";
pub const DEFAULT_LISTEN: &str = ":8080";

/// Values required to make decisions after startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub(crate) allowlists: PolicyPaths,
    pub(crate) mode_file: PathBuf,
    pub(crate) listen: SocketAddr,
    pub(crate) deny_log: Option<PathBuf>,
    pub(crate) local: Option<LocalConfig>,
}
impl Config {
    /// Resolves an injected environment lookup once during bootstrap.
    ///
    /// # Errors
    ///
    /// Returns an error when an environment value is invalid or incomplete.
    pub fn resolve<F>(lookup: F) -> Result<Self>
    where
        F: Fn(&str) -> Option<String>,
    {
        resolve_config(lookup)
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PolicyPaths(Vec<PathBuf>);
impl PolicyPaths {
    fn new(paths: Vec<PathBuf>) -> Result<Self> {
        if paths.is_empty() || paths.iter().any(|path| path.as_os_str().is_empty()) {
            bail!("VHRN_ALLOWLISTS must contain only nonempty paths");
        }
        Ok(Self(paths))
    }
    pub(crate) fn as_slice(&self) -> &[PathBuf] {
        &self.0
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocalPolicyPaths([PathBuf; 3]);
impl LocalPolicyPaths {
    #[allow(dead_code)] // Broker routing consumes local layers.
    pub(crate) fn as_array(&self) -> &[PathBuf; 3] {
        &self.0
    }
}
impl std::ops::Index<usize> for LocalPolicyPaths {
    type Output = PathBuf;
    fn index(&self, index: usize) -> &Self::Output {
        &self.0[index]
    }
}
/// Inputs used exclusively for local routing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalConfig {
    pub(crate) policy_paths: LocalPolicyPaths,
    pub(crate) broker_addr: BrokerEndpoint,
    pub(crate) token_file: PathBuf,
}

/// A host-owned broker address, limited to an unambiguous socket or DNS name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BrokerEndpoint {
    Socket(SocketAddr),
    Host { host: String, port: u16 },
}

impl BrokerEndpoint {
    /// Parses a numeric socket or hostname-and-port broker endpoint.
    pub(crate) fn parse(value: &str) -> Result<Self> {
        if let Ok(address) = value.parse::<SocketAddr>() {
            if address.port() != 0 {
                return Ok(Self::Socket(address));
            }
            bail!("invalid broker address");
        }
        if value.is_empty()
            || value.contains(['/', '?', '#', '@', '[', ']'])
            || value.bytes().any(|byte| byte.is_ascii_whitespace())
        {
            bail!("invalid broker address");
        }
        let Some((host, port)) = value.rsplit_once(':') else {
            bail!("invalid broker address");
        };
        if host.is_empty()
            || host.contains(':')
            || !valid_hostname(host)
            || port.is_empty()
            || !port.bytes().all(|byte| byte.is_ascii_digit())
        {
            bail!("invalid broker address");
        }
        let port = port
            .parse::<u16>()
            .ok()
            .filter(|port| *port != 0)
            .ok_or_else(|| anyhow::anyhow!("invalid broker address"))?;
        Ok(Self::Host {
            host: host.to_ascii_lowercase(),
            port,
        })
    }
}

impl From<SocketAddr> for BrokerEndpoint {
    fn from(address: SocketAddr) -> Self {
        Self::Socket(address)
    }
}

fn valid_hostname(host: &str) -> bool {
    host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

/// Resolves configuration from an injected environment lookup.
///
/// # Errors
///
/// Returns an error when the local-routing variables are incomplete or invalid.
pub(crate) fn resolve_config<F>(lookup: F) -> Result<Config>
where
    F: Fn(&str) -> Option<String>,
{
    let plural = lookup("VHRN_ALLOWLISTS");
    let allowlists = match plural {
        Some(value) => PolicyPaths::new(value.split(',').map(PathBuf::from).collect())
            .context("parse VHRN_ALLOWLISTS")?,
        None => PolicyPaths::new(vec![PathBuf::from(
            lookup("VHRN_ALLOWLIST").unwrap_or_else(|| DEFAULT_ALLOWLIST.to_owned()),
        )])
        .context("parse VHRN_ALLOWLIST")?,
    };
    let local_paths = lookup("VHRN_LOOPBACK_ALLOWLISTS");
    let broker_addr = lookup("VHRN_BROKER_ADDR");
    let token_file = lookup("VHRN_BROKER_TOKEN_FILE");
    let local = match (local_paths, broker_addr, token_file) {
        (None, None, None) => None,
        (Some(paths), Some(addr), Some(token)) => Some(LocalConfig {
            policy_paths: parse_local_paths(&paths).context("parse VHRN_LOOPBACK_ALLOWLISTS")?,
            broker_addr: BrokerEndpoint::parse(&addr).context("parse VHRN_BROKER_ADDR")?,
            token_file: nonempty_path(token, "VHRN_BROKER_TOKEN_FILE")?,
        }),
        _ => bail!(
            "VHRN_LOOPBACK_ALLOWLISTS, VHRN_BROKER_ADDR, and VHRN_BROKER_TOKEN_FILE must be set together"
        ),
    };
    Ok(Config {
        allowlists,
        mode_file: nonempty_path(
            lookup("VHRN_MODE_FILE").unwrap_or_else(|| DEFAULT_MODE_FILE.to_owned()),
            "VHRN_MODE_FILE",
        )?,
        listen: parse_listener(
            &lookup("VHRN_PROXY_LISTEN").unwrap_or_else(|| DEFAULT_LISTEN.to_owned()),
        )
        .context("parse VHRN_PROXY_LISTEN")?,
        deny_log: lookup("VHRN_DENY_LOG")
            .filter(|value| !value.is_empty())
            .map(|value| nonempty_path(value, "VHRN_DENY_LOG"))
            .transpose()?,
        local,
    })
}

fn parse_local_paths(value: &str) -> Result<LocalPolicyPaths> {
    let paths: [PathBuf; 3] = value
        .split(',')
        .map(PathBuf::from)
        .collect::<Vec<_>>()
        .try_into()
        .map_err(|_| anyhow::anyhow!("requires exactly three policy paths"))?;
    if paths.iter().any(|path| path.as_os_str().is_empty()) {
        bail!("contains an empty policy path");
    }
    Ok(LocalPolicyPaths(paths))
}
fn nonempty_path(value: String, name: &str) -> Result<PathBuf> {
    if value.is_empty() {
        bail!("{name} must not be empty");
    }
    Ok(PathBuf::from(value))
}
fn parse_listener(value: &str) -> Result<SocketAddr> {
    let address: SocketAddr = value
        .strip_prefix(':')
        .map_or_else(|| value.parse(), |port| format!("0.0.0.0:{port}").parse())?;
    if address.port() == 0 {
        bail!("listener port must be nonzero");
    }
    Ok(address)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn resolves_startup_corpus() {
        let config = resolve_config(|_| None).unwrap();
        assert_eq!(
            config.allowlists.as_slice(),
            [PathBuf::from(DEFAULT_ALLOWLIST)]
        );
        for row in include_str!("../testdata/proxy-process-cases.tsv")
            .lines()
            .filter(|r| !r.starts_with('#'))
        {
            let fields: Vec<_> = row.split('\t').collect();
            if let ["startup", input, expected, _] = fields.as_slice() {
                if *input == "listener address already bound"
                    || *input == "broker readiness refused"
                {
                    continue;
                }
                let mut values = BTreeMap::new();
                if *input == "partial local variables" {
                    values.insert("VHRN_BROKER_ADDR", "127.0.0.1:1");
                }
                if *input == "invalid local token" {
                    values.insert("VHRN_LOOPBACK_ALLOWLISTS", "a,b,c");
                    values.insert("VHRN_BROKER_ADDR", "127.0.0.1:1");
                    values.insert("VHRN_BROKER_TOKEN_FILE", "token");
                }
                let result = resolve_config(|key| values.get(key).map(ToString::to_string));
                assert_eq!(
                    result.is_ok(),
                    *expected == "start" || *input == "invalid local token",
                    "{input}"
                );
            }
        }
    }
    #[test]
    fn plural_rejects_empty_items() {
        assert!(
            resolve_config(|key| match key {
                "VHRN_ALLOWLISTS" => Some("one,,three".to_owned()),
                _ => None,
            })
            .is_err()
        );
    }
    #[test]
    fn resolves_allowlist_and_local_edges() {
        let resolve = |values: BTreeMap<&str, &str>| {
            resolve_config(|key| values.get(key).map(ToString::to_string))
        };
        assert_eq!(
            resolve(BTreeMap::from([
                ("VHRN_ALLOWLISTS", "one,two"),
                ("VHRN_ALLOWLIST", "ignored")
            ]))
            .unwrap()
            .allowlists
            .as_slice(),
            [PathBuf::from("one"), PathBuf::from("two")]
        );
        assert!(
            resolve(BTreeMap::from([
                ("VHRN_ALLOWLISTS", ""),
                ("VHRN_ALLOWLIST", "single")
            ]))
            .is_err()
        );
        assert!(resolve(BTreeMap::from([("VHRN_ALLOWLIST", "")])).is_err());
        let config = resolve(BTreeMap::from([("VHRN_DENY_LOG", "")])).unwrap();
        assert_eq!(config.deny_log, None);
        assert!(
            resolve(BTreeMap::from([
                ("VHRN_LOOPBACK_ALLOWLISTS", "a,b,c"),
                ("VHRN_BROKER_ADDR", "not-an-address"),
                ("VHRN_BROKER_TOKEN_FILE", "token")
            ]))
            .is_err()
        );
        for paths in ["a,b", "a,b,c,d", "a,,c"] {
            assert!(
                resolve(BTreeMap::from([
                    ("VHRN_LOOPBACK_ALLOWLISTS", paths),
                    ("VHRN_BROKER_ADDR", "127.0.0.1:1"),
                    ("VHRN_BROKER_TOKEN_FILE", "token")
                ]))
                .is_err()
            );
        }
    }
    #[test]
    fn broker_endpoint_accepts_only_socket_or_hostname_with_port() {
        let config = resolve_config(|key| match key {
            "VHRN_LOOPBACK_ALLOWLISTS" => Some("one,two,three".to_owned()),
            "VHRN_BROKER_ADDR" => Some("host.docker.internal:12345".to_owned()),
            "VHRN_BROKER_TOKEN_FILE" => Some("token".to_owned()),
            _ => None,
        })
        .unwrap();
        assert_eq!(
            config.local.unwrap().broker_addr,
            BrokerEndpoint::Host {
                host: "host.docker.internal".to_owned(),
                port: 12345,
            }
        );
        assert_eq!(
            BrokerEndpoint::parse("127.0.0.1:12345").unwrap(),
            BrokerEndpoint::Socket("127.0.0.1:12345".parse().unwrap())
        );
        assert_eq!(
            BrokerEndpoint::parse("[::1]:12345").unwrap(),
            BrokerEndpoint::Socket("[::1]:12345".parse().unwrap())
        );
        for value in [
            "",
            "host.docker.internal",
            "host.docker.internal:0",
            "127.0.0.1:0",
            "[::1]:0",
            "host.docker.internal:not-a-port",
            "http://host.docker.internal:12345",
            "user@host.docker.internal:12345",
            "host.docker.internal:12345/path",
            "[host.docker.internal]:12345",
            "[::1:12345",
            "::1:12345",
            ":12345",
            "host..internal:12345",
        ] {
            assert!(BrokerEndpoint::parse(value).is_err(), "{value}");
        }
    }
}
