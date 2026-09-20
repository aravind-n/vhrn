//! Startup configuration resolved from a small environment surface.

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use tokio::io::AsyncReadExt;

use crate::connect::broker::BrokerToken;
use crate::diagnostics::AuditService;

pub const DEFAULT_ALLOWLIST: &str = "/etc/vhrn/allowlist";
pub const DEFAULT_MODE_FILE: &str = "/etc/vhrn/mode";
pub const DEFAULT_LISTEN: &str = ":8080";

/// Values required to make decisions after startup.
#[derive(Clone)]
pub struct Config {
    pub(crate) allowlists: PolicyPaths,
    pub(crate) mode_file: PathBuf,
    pub(crate) listen: SocketAddr,
    pub(crate) deny_log: AuditService,
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
#[cfg(test)]
impl LocalPolicyPaths {
    pub(crate) fn test(paths: [PathBuf; 3]) -> Self {
        Self(paths)
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
    let plural = value(&lookup, "VHRN_ALLOWLISTS");
    let allowlists = match plural {
        Some(value) => PolicyPaths::new(value.split(',').map(PathBuf::from).collect())
            .context("parse VHRN_ALLOWLISTS")?,
        None => PolicyPaths::new(vec![PathBuf::from(
            value(&lookup, "VHRN_ALLOWLIST").unwrap_or_else(|| DEFAULT_ALLOWLIST.to_owned()),
        )])
        .context("parse VHRN_ALLOWLIST")?,
    };
    let local_paths = value(&lookup, "VHRN_LOOPBACK_ALLOWLISTS");
    let broker_addr = value(&lookup, "VHRN_BROKER_ADDR");
    let token_file = value(&lookup, "VHRN_BROKER_TOKEN_FILE");
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
    let mode_file = PathBuf::from(
        value(&lookup, "VHRN_MODE_FILE").unwrap_or_else(|| DEFAULT_MODE_FILE.to_owned()),
    );
    let deny_log = value(&lookup, "VHRN_DENY_LOG").map(PathBuf::from);
    let audit = AuditService::new(deny_log);
    Ok(Config {
        allowlists,
        mode_file,
        listen: parse_listener(
            &value(&lookup, "VHRN_PROXY_LISTEN").unwrap_or_else(|| DEFAULT_LISTEN.to_owned()),
        )
        .context("parse VHRN_PROXY_LISTEN")?,
        deny_log: audit,
        local,
    })
}

fn value<F>(lookup: &F, name: &str) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    lookup(name).filter(|value| !value.is_empty())
}

/// Loads the local credential only when local routing is configured.
///
/// # Errors
///
/// Returns an error when the credential file cannot be read or is invalid.
pub(crate) async fn load_broker_token(config: &LocalConfig) -> Result<BrokerToken> {
    let mut options = tokio::fs::OpenOptions::new();
    options.read(true);
    #[cfg(any(target_os = "linux", target_os = "android"))]
    options.custom_flags(0x800);
    #[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
    options.custom_flags(0x4);
    let file = options
        .open(&config.token_file)
        .await
        .map_err(|_| anyhow::anyhow!("VHRN_BROKER_TOKEN_FILE is unreadable"))?;
    let metadata = file
        .metadata()
        .await
        .map_err(|_| anyhow::anyhow!("VHRN_BROKER_TOKEN_FILE is unreadable"))?;
    if !metadata.is_file() || metadata.len() != 64 {
        bail!("VHRN_BROKER_TOKEN_FILE contains an invalid token");
    }
    let mut bytes = Vec::with_capacity(64);
    file.take(65)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| anyhow::anyhow!("VHRN_BROKER_TOKEN_FILE is unreadable"))?;
    let value = String::from_utf8(bytes)
        .map_err(|_| anyhow::anyhow!("VHRN_BROKER_TOKEN_FILE contains an invalid token"))?;
    value
        .parse::<BrokerToken>()
        .map_err(|_| anyhow::anyhow!("VHRN_BROKER_TOKEN_FILE contains an invalid token"))
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
    use std::fs;
    use tempfile::tempdir;

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
        assert_eq!(
            resolve(BTreeMap::from([
                ("VHRN_ALLOWLISTS", ""),
                ("VHRN_ALLOWLIST", "single")
            ]))
            .unwrap()
            .allowlists
            .as_slice(),
            [PathBuf::from("single")]
        );
        assert_eq!(
            resolve(BTreeMap::from([("VHRN_ALLOWLIST", "")]))
                .unwrap()
                .allowlists
                .as_slice(),
            [PathBuf::from(DEFAULT_ALLOWLIST)]
        );
        let config = resolve(BTreeMap::from([("VHRN_DENY_LOG", "")])).unwrap();
        assert!(!config.deny_log.has_path());
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
    fn empty_values_are_unset_for_every_default_and_optional_variable() {
        let values = BTreeMap::from([
            ("VHRN_ALLOWLISTS", ""),
            ("VHRN_ALLOWLIST", ""),
            ("VHRN_MODE_FILE", ""),
            ("VHRN_PROXY_LISTEN", ""),
            ("VHRN_DENY_LOG", ""),
            ("VHRN_LOOPBACK_ALLOWLISTS", ""),
            ("VHRN_BROKER_ADDR", ""),
            ("VHRN_BROKER_TOKEN_FILE", ""),
        ]);
        let config = resolve_config(|key| values.get(key).map(ToString::to_string)).unwrap();
        assert_eq!(
            config.allowlists.as_slice(),
            [PathBuf::from(DEFAULT_ALLOWLIST)]
        );
        assert_eq!(config.mode_file, PathBuf::from(DEFAULT_MODE_FILE));
        assert_eq!(config.listen, "0.0.0.0:8080".parse().unwrap());
        assert!(!config.deny_log.has_path());
        assert!(config.local.is_none());
    }

    #[test]
    fn every_partial_nonempty_local_group_is_rejected() {
        const NAMES: [&str; 3] = [
            "VHRN_LOOPBACK_ALLOWLISTS",
            "VHRN_BROKER_ADDR",
            "VHRN_BROKER_TOKEN_FILE",
        ];
        const VALUES: [&str; 3] = ["one,two,three", "127.0.0.1:1234", "token"];
        for mask in 0_u8..8 {
            let result = resolve_config(|name| {
                NAMES
                    .iter()
                    .position(|candidate| *candidate == name)
                    .map(|index| {
                        if mask & (1 << index) == 0 {
                            String::new()
                        } else {
                            VALUES[index].to_owned()
                        }
                    })
            });
            assert_eq!(result.is_ok(), mask == 0 || mask == 7, "mask {mask:03b}");
            if mask == 0 {
                assert!(result.unwrap().local.is_none());
            }
        }
    }

    #[test]
    fn invalid_listener_values_are_rejected_without_echoing_values() {
        for listener in ["host.invalid:8080", "127.0.0.1:0", ":0", "not a listener"] {
            let error =
                resolve_config(|name| (name == "VHRN_PROXY_LISTEN").then(|| listener.to_owned()))
                    .err()
                    .unwrap();
            assert!(error.to_string().contains("VHRN_PROXY_LISTEN"));
            assert!(!error.to_string().contains(listener));
        }
    }
    #[tokio::test]
    async fn token_file_is_literal_and_redacted() {
        let directory = tempdir().unwrap();
        let file = directory.path().join("token");
        let config = LocalConfig {
            policy_paths: LocalPolicyPaths::test([
                PathBuf::from("a"),
                PathBuf::from("b"),
                PathBuf::from("c"),
            ]),
            broker_addr: BrokerEndpoint::parse("127.0.0.1:1").unwrap(),
            token_file: file.clone(),
        };
        let valid = "a".repeat(64);
        fs::write(&file, &valid).unwrap();
        let token = load_broker_token(&config).await.unwrap();
        assert!(!format!("{token:?}").contains(&valid));
        for contents in [format!("{valid}\n"), "A".repeat(64)] {
            fs::write(&file, contents).unwrap();
            let error = load_broker_token(&config).await.unwrap_err();
            assert!(!error.to_string().contains(&valid));
        }
        fs::write(&file, [0xff]).unwrap();
        assert!(load_broker_token(&config).await.is_err());
        fs::remove_file(&file).unwrap();
        assert!(load_broker_token(&config).await.is_err());
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
