//! Startup configuration resolved from a small environment surface.

use std::net::SocketAddr;

use anyhow::{Context, Result, bail};
use vhrn_policy::BrokerToken;

pub const DEFAULT_ALLOWLIST: &str = "/etc/vhrn/allowlist";
pub const DEFAULT_MODE_FILE: &str = "/etc/vhrn/mode";
pub const DEFAULT_LISTEN: &str = ":8080";

/// Values required to make decisions after startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub allowlists: Vec<String>,
    pub mode_file: String,
    pub listen: String,
    pub deny_log: Option<String>,
    pub local: Option<LocalConfig>,
}

/// Inputs used exclusively for local routing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalConfig {
    pub policy_paths: [String; 3],
    pub broker_addr: SocketAddr,
    pub token_file: String,
}

/// Resolves configuration from an injected environment lookup.
///
/// # Errors
///
/// Returns an error when the local-routing variables are incomplete or invalid.
pub fn resolve_config<F>(lookup: F) -> Result<Config>
where
    F: Fn(&str) -> Option<String>,
{
    let plural = lookup("VHRN_ALLOWLISTS");
    let allowlists = match plural.filter(|value| !value.is_empty()) {
        Some(value) => value.split(',').map(str::to_owned).collect(),
        None => vec![lookup("VHRN_ALLOWLIST").unwrap_or_else(|| DEFAULT_ALLOWLIST.to_owned())],
    };
    let local_paths = lookup("VHRN_LOOPBACK_ALLOWLISTS");
    let broker_addr = lookup("VHRN_BROKER_ADDR");
    let token_file = lookup("VHRN_BROKER_TOKEN_FILE");
    let local = match (local_paths, broker_addr, token_file) {
        (None, None, None) => None,
        (Some(paths), Some(addr), Some(token)) => Some(LocalConfig {
            policy_paths: parse_local_paths(&paths)?,
            broker_addr: addr.parse().context("invalid broker address")?,
            token_file: nonempty(token, "broker token file")?,
        }),
        _ => bail!("local routing requires policy paths, broker address, and token file"),
    };
    Ok(Config {
        allowlists,
        mode_file: lookup("VHRN_MODE_FILE").unwrap_or_else(|| DEFAULT_MODE_FILE.to_owned()),
        listen: lookup("VHRN_PROXY_LISTEN").unwrap_or_else(|| DEFAULT_LISTEN.to_owned()),
        deny_log: lookup("VHRN_DENY_LOG").filter(|value| !value.is_empty()),
        local,
    })
}

/// Resolves configuration from the process environment.
///
/// # Errors
///
/// Returns an error when the local-routing variables are incomplete or invalid.
pub fn config_from_env() -> Result<Config> {
    resolve_config(|key| std::env::var(key).ok())
}

/// Loads the local credential only when local routing is configured.
///
/// # Errors
///
/// Returns an error when the credential file cannot be read or is invalid.
pub fn load_broker_token(config: &LocalConfig) -> Result<BrokerToken> {
    let bytes = std::fs::read(&config.token_file).context("read broker token")?;
    let value = String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("invalid broker token"))?;
    BrokerToken::parse(value).map_err(|_| anyhow::anyhow!("invalid broker token"))
}

fn parse_local_paths(value: &str) -> Result<[String; 3]> {
    let paths: Vec<_> = value.split(',').map(str::to_owned).collect();
    let [first, second, third] = paths.as_slice() else {
        bail!("local routing requires exactly three policy paths");
    };
    Ok([
        nonempty(first.clone(), "local policy path")?,
        nonempty(second.clone(), "local policy path")?,
        nonempty(third.clone(), "local policy path")?,
    ])
}
fn nonempty(value: String, name: &str) -> Result<String> {
    if value.is_empty() {
        bail!("{name} must not be empty");
    }
    Ok(value)
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
        assert_eq!(config.allowlists, [DEFAULT_ALLOWLIST]);
        for row in include_str!("../../testdata/proxy-process-cases.tsv")
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
    fn plural_keeps_empty_items() {
        let config = resolve_config(|key| match key {
            "VHRN_ALLOWLISTS" => Some("one,,three".to_owned()),
            _ => None,
        })
        .unwrap();
        assert_eq!(config.allowlists, ["one", "", "three"]);
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
            .allowlists,
            ["one", "two"]
        );
        assert_eq!(
            resolve(BTreeMap::from([
                ("VHRN_ALLOWLISTS", ""),
                ("VHRN_ALLOWLIST", "single")
            ]))
            .unwrap()
            .allowlists,
            ["single"]
        );
        assert_eq!(
            resolve(BTreeMap::from([("VHRN_ALLOWLIST", "")]))
                .unwrap()
                .allowlists,
            [""]
        );
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
    fn token_file_is_literal_and_redacted() {
        let directory = tempdir().unwrap();
        let file = directory.path().join("token");
        let config = LocalConfig {
            policy_paths: ["a".to_owned(), "b".to_owned(), "c".to_owned()],
            broker_addr: "127.0.0.1:1".parse().unwrap(),
            token_file: file.display().to_string(),
        };
        let valid = "a".repeat(64);
        fs::write(&file, &valid).unwrap();
        let token = load_broker_token(&config).unwrap();
        assert!(!format!("{token:?}").contains(&valid));
        for contents in [format!("{valid}\n"), "A".repeat(64)] {
            fs::write(&file, contents).unwrap();
            let error = load_broker_token(&config).unwrap_err();
            assert!(!error.to_string().contains(&valid));
        }
        fs::write(&file, [0xff]).unwrap();
        assert!(load_broker_token(&config).is_err());
        fs::remove_file(&file).unwrap();
        assert!(load_broker_token(&config).is_err());
    }
}
