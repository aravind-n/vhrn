//! Live policy snapshots and pure policy evaluation.

use std::collections::{BTreeSet, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result, bail};

use crate::domain::target::{LoopbackAuthority, PublicHost};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Enforce,
    Report,
    Open,
}
impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
impl FromStr for Mode {
    type Err = ();
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "enforce" => Ok(Self::Enforce),
            "report" => Ok(Self::Report),
            "open" => Ok(Self::Open),
            _ => Err(()),
        }
    }
}
impl Mode {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Enforce => "enforce",
            Self::Report => "report",
            Self::Open => "open",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct DomainName(String);
impl fmt::Display for DomainName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl FromStr for DomainName {
    type Err = ();
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.is_empty() || value.trim() != value {
            return Err(());
        }
        let value = value.strip_suffix('.').unwrap_or(value);
        valid_domain(value)
            .then(|| Self(value.to_ascii_lowercase()))
            .ok_or(())
    }
}
/// A persisted policy hostname.  This deliberately follows the host CLI's
/// storage grammar, which is broader than a wire DNS name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct PolicyDomain(String);
impl FromStr for PolicyDomain {
    type Err = ();
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim().trim_matches('.');
        valid_policy_domain(value)
            .then(|| Self(value.to_ascii_lowercase()))
            .ok_or(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct DomainPattern(PolicyDomain);
impl FromStr for DomainPattern {
    type Err = ();
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value
            .trim()
            .strip_prefix("*.")
            .unwrap_or(value.trim())
            .parse()
            .map(Self)
    }
}
impl DomainPattern {
    pub(crate) fn matches(&self, host: &DomainName) -> bool {
        host.0 == self.0.0
            || host
                .0
                .strip_suffix(&self.0.0)
                .is_some_and(|p| p.ends_with('.'))
    }
}
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum PolicyEntry {
    Domain(DomainPattern),
    Ip(std::net::Ipv4Addr),
}
impl FromStr for PolicyEntry {
    type Err = ();
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let normalized = value
            .trim()
            .strip_prefix("*.")
            .unwrap_or(value.trim())
            .trim_matches('.');
        normalized
            .parse()
            .map(Self::Ip)
            .or_else(|_| value.parse::<DomainPattern>().map(Self::Domain))
    }
}
fn valid_domain(value: &str) -> bool {
    value.is_ascii()
        && value.len() <= 253
        && value.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}
fn valid_policy_domain(value: &str) -> bool {
    value.is_ascii()
        && !value.is_empty()
        && value.split('.').all(|label| !label.is_empty())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        && value.bytes().any(|byte| byte.is_ascii_alphanumeric())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PolicyParseError;
impl fmt::Display for PolicyParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid policy entry")
    }
}
impl std::error::Error for PolicyParseError {}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicDecision {
    pub allowed: bool,
    pub record_denial: bool,
    pub mode: Mode,
}
pub struct PolicySnapshot {
    mode: Mode,
    public: BTreeSet<PolicyEntry>,
    local: HashSet<LoopbackAuthority>,
}

impl PolicySnapshot {
    pub async fn public(paths: &[PathBuf], mode_path: &Path) -> Result<(Self, Option<String>)> {
        let mode_contents = tokio::fs::read_to_string(mode_path)
            .await
            .with_context(|| format!("read policy mode {}", mode_path.display()))?;
        let (mode, warning) = parse_mode(&mode_contents);
        let public = read_domain_layers(paths).await?;
        Ok((
            Self {
                mode,
                public,
                local: HashSet::new(),
            },
            warning,
        ))
    }
    pub async fn local(paths: &[PathBuf; 3]) -> Result<Self> {
        Ok(Self {
            mode: Mode::Enforce,
            public: BTreeSet::new(),
            local: read_local_layers(paths).await?,
        })
    }
    pub fn decide_public(&self, host: &PublicHost) -> PublicDecision {
        let matched = match host {
            PublicHost::Dns(host) => self.public.iter().any(
                |entry| matches!(entry, PolicyEntry::Domain(pattern) if pattern.matches(host)),
            ),
            PublicHost::Ip(std::net::IpAddr::V4(host)) => {
                self.public.contains(&PolicyEntry::Ip(*host))
            }
            PublicHost::Ip(std::net::IpAddr::V6(_)) => false,
        };
        match self.mode {
            Mode::Enforce => PublicDecision {
                allowed: matched,
                record_denial: !matched,
                mode: self.mode,
            },
            Mode::Report => PublicDecision {
                allowed: true,
                record_denial: !matched,
                mode: self.mode,
            },
            Mode::Open => PublicDecision {
                allowed: true,
                record_denial: false,
                mode: self.mode,
            },
        }
    }
    pub fn decide_local(&self, authority: &LoopbackAuthority) -> bool {
        self.local.contains(authority)
    }
    pub(crate) const fn mode(&self) -> Mode {
        self.mode
    }
}
/// Resolves the mode for an operational status endpoint without requiring a
/// readable policy layer. Request authorization still uses `public` above.
pub async fn effective_status_mode(paths: &[PathBuf], mode_path: &Path) -> (Mode, Option<String>) {
    match PolicySnapshot::public(paths, mode_path).await {
        Ok((snapshot, warning)) => (snapshot.mode(), warning),
        Err(error) => (Mode::Enforce, Some(error.to_string())),
    }
}
pub async fn decide_public(
    paths: &[PathBuf],
    mode_path: &Path,
    host: &PublicHost,
) -> Result<(PublicDecision, Option<String>)> {
    let (snapshot, warning) = PolicySnapshot::public(paths, mode_path).await?;
    Ok((snapshot.decide_public(host), warning))
}
pub async fn decide_local(paths: &[PathBuf; 3], authority: &LoopbackAuthority) -> Result<bool> {
    Ok(PolicySnapshot::local(paths).await?.decide_local(authority))
}

async fn read_domain_layers(paths: &[PathBuf]) -> Result<BTreeSet<PolicyEntry>> {
    if paths.is_empty() {
        bail!("public policy has no layers");
    }
    let mut all = BTreeSet::new();
    for (index, path) in paths.iter().enumerate() {
        let contents = tokio::fs::read_to_string(path)
            .await
            .with_context(|| format!("read public policy layer {index} at {}", path.display()))?;
        all.extend(parse_domain_layer_at(&contents, path, index)?);
    }
    Ok(all)
}
async fn read_local_layers(paths: &[PathBuf; 3]) -> Result<HashSet<LoopbackAuthority>> {
    let mut all = HashSet::new();
    for (index, path) in paths.iter().enumerate() {
        let contents = tokio::fs::read_to_string(path)
            .await
            .with_context(|| format!("read local policy layer {index} at {}", path.display()))?;
        all.extend(parse_local_layer_at(&contents, path, index)?);
    }
    Ok(all)
}
fn parse_mode(contents: &str) -> (Mode, Option<String>) {
    if contents.lines().count() != 1 {
        return (
            Mode::Enforce,
            Some("mode file is multiline; enforcing".to_owned()),
        );
    }
    let value = contents.trim_end_matches(['\r', '\n']);
    match value.parse() {
        Ok(mode) => (mode, None),
        Err(()) => (
            Mode::Enforce,
            Some("mode file is unknown; enforcing".to_owned()),
        ),
    }
}
fn parse_domain_layer_at(
    contents: &str,
    path: &Path,
    index: usize,
) -> Result<BTreeSet<PolicyEntry>> {
    if let Ok(entries) = parse_domain_layer(contents) {
        Ok(entries)
    } else {
        let line = contents
            .lines()
            .position(|value| value.parse::<PolicyEntry>().is_err())
            .unwrap_or(0);
        Err(anyhow::anyhow!(
            "malformed public policy layer {index} at {} line {}",
            path.display(),
            line + 1
        ))
    }
}
fn parse_local_layer_at(
    contents: &str,
    path: &Path,
    index: usize,
) -> Result<HashSet<LoopbackAuthority>> {
    if let Ok(entries) = parse_local_layer(contents) {
        Ok(entries)
    } else {
        let line = contents
            .lines()
            .position(|value| value.parse::<LoopbackAuthority>().is_err())
            .unwrap_or(0);
        Err(anyhow::anyhow!(
            "malformed local policy layer {index} at {} line {}",
            path.display(),
            line + 1
        ))
    }
}
pub(crate) fn parse_domain_layer(
    contents: &str,
) -> Result<BTreeSet<PolicyEntry>, PolicyParseError> {
    contents
        .lines()
        .map(str::parse)
        .collect::<std::result::Result<_, _>>()
        .map_err(|()| PolicyParseError)
}
pub fn parse_local_layer(contents: &str) -> Result<HashSet<LoopbackAuthority>, PolicyParseError> {
    contents
        .lines()
        .map(LoopbackAuthority::parse)
        .collect::<std::result::Result<_, _>>()
        .map_err(|_| PolicyParseError)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn host(value: &str) -> PublicHost {
        PublicHost::Dns(value.parse().unwrap())
    }
    #[tokio::test]
    async fn mode_corpus_and_live_replacement() {
        let directory = tempdir().unwrap();
        let layer = directory.path().join("layer");
        let mode = directory.path().join("mode");
        tokio::fs::write(&layer, "allowed.example\n").await.unwrap();
        for row in include_str!("../../testdata/proxy-modes.tsv")
            .lines()
            .filter(|row| !row.starts_with('#'))
        {
            let fields: Vec<_> = row.split('\t').collect();
            let [stored, state, yes, allowed, recorded, effective] = fields.as_slice() else {
                panic!("bad fixture {row}")
            };
            match *state {
                "valid" | "replace" => {
                    tokio::fs::write(&mode, format!("{stored}\n"))
                        .await
                        .unwrap();
                }
                "missing-mode" => {
                    let _ = tokio::fs::remove_file(&mode).await;
                }
                "missing-layer" | "read-error" => {
                    let _ = tokio::fs::remove_file(&layer).await;
                    tokio::fs::write(&mode, format!("{stored}\n"))
                        .await
                        .unwrap();
                }
                _ => {
                    tokio::fs::write(&layer, "bad!entry\n").await.unwrap();
                    tokio::fs::write(&mode, format!("{stored}\n"))
                        .await
                        .unwrap();
                }
            }
            let requested = host(if *yes == "yes" {
                "allowed.example"
            } else {
                "blocked.example"
            });
            let result = decide_public(std::slice::from_ref(&layer), &mode, &requested).await;
            if matches!(
                *state,
                "missing-mode" | "missing-layer" | "read-error" | "malformed-layer"
            ) {
                assert!(result.is_err(), "{row}");
            } else {
                let (decision, _) = result.unwrap();
                assert_eq!(decision.allowed, *allowed == "yes", "{row}");
                assert_eq!(decision.record_denial, *recorded == "yes", "{row}");
                assert_eq!(decision.mode.as_str(), *effective, "{row}");
            }
            tokio::fs::write(&layer, "allowed.example\n").await.unwrap();
        }
    }
    #[tokio::test]
    async fn layers_context_and_mode_warnings() {
        let directory = tempdir().unwrap();
        let paths: [PathBuf; 3] = [0, 1, 2].map(|n| directory.path().join(n.to_string()));
        let mode = directory.path().join("mode");
        for path in &paths {
            tokio::fs::write(path, "").await.unwrap();
        }
        tokio::fs::write(&mode, "enforce\n").await.unwrap();
        for index in 0..3 {
            tokio::fs::write(&paths[index], "localhost:80\n")
                .await
                .unwrap();
            assert!(
                decide_local(&paths, &"localhost:80".parse().unwrap())
                    .await
                    .unwrap()
            );
            tokio::fs::write(&paths[index], "").await.unwrap();
        }
        tokio::fs::write(&paths[1], "bad\n").await.unwrap();
        let error = decide_local(&paths, &"localhost:80".parse().unwrap())
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("layer 1")
                && error.contains("line 1")
                && error.contains(&paths[1].display().to_string())
        );
        let public = vec![paths[0].clone()];
        tokio::fs::write(&paths[0], "allowed.example\n")
            .await
            .unwrap();
        for value in ["strange\n", "open\nreport\n"] {
            tokio::fs::write(&mode, value).await.unwrap();
            let (decision, warning) = decide_public(&public, &mode, &host("blocked.example"))
                .await
                .unwrap();
            assert!(!decision.allowed);
            assert!(warning.is_some());
        }
    }
    #[tokio::test]
    async fn public_layers_union_replacement_and_fail_closed_context() {
        let directory = tempdir().unwrap();
        let paths: [PathBuf; 5] =
            ["base", "harness", "global", "project", "run"].map(|name| directory.path().join(name));
        let mode = directory.path().join("mode");
        for path in &paths {
            tokio::fs::write(path, "").await.unwrap();
        }
        tokio::fs::write(&mode, "enforce\n").await.unwrap();
        tokio::fs::write(&paths[0], "one.example\none.example\n")
            .await
            .unwrap();
        tokio::fs::write(&paths[4], "two.example\n").await.unwrap();
        assert!(
            decide_public(&paths, &mode, &host("two.example"))
                .await
                .unwrap()
                .0
                .allowed
        );
        assert!(
            decide_public(&[], &mode, &host("two.example"))
                .await
                .unwrap_err()
                .to_string()
                .contains("no layers")
        );
        tokio::fs::write(&paths[0], "three.example\n")
            .await
            .unwrap();
        assert!(
            !decide_public(&paths, &mode, &host("one.example"))
                .await
                .unwrap()
                .0
                .allowed
        );
        for contents in ["\n", "bad!entry\n"] {
            tokio::fs::write(&paths[1], contents).await.unwrap();
            for mode_value in ["report\n", "open\n"] {
                tokio::fs::write(&mode, mode_value).await.unwrap();
                let error = decide_public(&paths, &mode, &host("one.example"))
                    .await
                    .unwrap_err()
                    .to_string();
                assert!(
                    error.contains("layer 1")
                        && error.contains("line 1")
                        && error.contains(&paths[1].display().to_string())
                );
            }
        }
        tokio::fs::remove_file(&paths[1]).await.unwrap();
        let error = decide_public(&paths, &mode, &host("one.example"))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("layer 1") && error.contains(&paths[1].display().to_string()));
    }
    #[tokio::test]
    async fn local_failures_and_replacement_are_contextual_not_panics() {
        let directory = tempdir().unwrap();
        let paths: [PathBuf; 3] = [0, 1, 2].map(|n| directory.path().join(n.to_string()));
        let authority = "localhost:80".parse().unwrap();
        for path in &paths {
            tokio::fs::write(path, "").await.unwrap();
        }
        assert!(!decide_local(&paths, &authority).await.unwrap());
        tokio::fs::write(&paths[0], "localhost:80\n").await.unwrap();
        assert!(decide_local(&paths, &authority).await.unwrap());
        tokio::fs::write(&paths[0], "127.0.0.1:81\n").await.unwrap();
        assert!(!decide_local(&paths, &authority).await.unwrap());
        for contents in ["\n", "bad\n"] {
            tokio::fs::write(&paths[2], contents).await.unwrap();
            let error = decide_local(&paths, &authority)
                .await
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("layer 2")
                    && error.contains("line 1")
                    && error.contains(&paths[2].display().to_string())
            );
        }
        tokio::fs::remove_file(&paths[2]).await.unwrap();
        let error = decide_local(&paths, &authority)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("layer 2") && error.contains(&paths[2].display().to_string()));
    }
    #[test]
    fn parsers_cover_domain_fixture_and_whitespace() {
        assert!(parse_domain_layer("").unwrap().is_empty());
        assert!(parse_local_layer("").unwrap().is_empty());
        for value in ["\n", "\r\n", " \n"] {
            assert!(parse_domain_layer(value).is_err());
            assert!(parse_local_layer(value).is_err());
        }
        for row in include_str!("../../testdata/domain-policy.tsv")
            .lines()
            .filter(|row| !row.starts_with('#'))
        {
            let fields: Vec<_> = row.split('\t').collect();
            if let ["entry", value, _, _, outcome] = fields.as_slice() {
                assert_eq!(
                    parse_domain_layer(&format!("{value}\n")).is_ok(),
                    *outcome == "accept",
                    "{row}"
                );
            }
        }
    }

    #[test]
    fn persisted_ip_grants_are_exact_and_policy_hostnames_remain_broader_than_wire_names() {
        for value in ["_service.example", "-edge.example", "edge-.example"] {
            assert!(value.parse::<PolicyEntry>().is_ok(), "{value}");
            assert!(value.parse::<DomainName>().is_err(), "{value}");
        }
        assert!("2606:4700:4700::1111".parse::<PolicyEntry>().is_err());
        let policy = parse_domain_layer("8.8.8.8\n").unwrap();
        let snapshot = PolicySnapshot {
            mode: Mode::Enforce,
            public: policy,
            local: HashSet::new(),
        };
        assert!(
            snapshot
                .decide_public(&PublicHost::Ip("8.8.8.8".parse().unwrap()))
                .allowed
        );
        assert!(
            !snapshot
                .decide_public(&PublicHost::Ip("8.8.8.9".parse().unwrap()))
                .allowed
        );
    }
}
