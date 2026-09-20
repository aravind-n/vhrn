//! Strict live policy snapshots and pure policy evaluation.

mod reader;

use std::collections::{BTreeSet, HashSet};
use std::fmt;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::Result;

pub(crate) use self::reader::PolicyReader;
use crate::domain::target::{LoopbackAuthority, PublicHost};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Enforce,
    Report,
    Open,
}

impl fmt::Display for Mode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
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
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
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

/// A persisted policy hostname, kept separate from request-host parsing.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct PolicyDomain(String);

impl FromStr for PolicyDomain {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        valid_policy_domain(value)
            .then(|| Self(value.to_owned()))
            .ok_or(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct DomainPattern(PolicyDomain);

impl FromStr for DomainPattern {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim();
        let value = value.strip_prefix("*.").unwrap_or(value).trim_matches('.');
        value.to_ascii_lowercase().parse().map(Self)
    }
}

impl DomainPattern {
    pub(crate) fn matches(&self, host: &DomainName) -> bool {
        host.0 == self.0.0
            || host
                .0
                .strip_suffix(&self.0.0)
                .is_some_and(|prefix| prefix.ends_with('.'))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum PolicyEntry {
    Domain(DomainPattern),
    Ip(Ipv4Addr),
}

impl FromStr for PolicyEntry {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if !valid_policy_domain(value) {
            return Err(());
        }
        if let Ok(address) = value.parse::<Ipv4Addr>()
            && address.to_string() == value
        {
            return Ok(Self::Ip(address));
        }
        value.parse::<DomainPattern>().map(Self::Domain)
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
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

fn valid_policy_domain(value: &str) -> bool {
    value.is_ascii()
        && !value.is_empty()
        && value.split('.').all(|label| !label.is_empty())
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        })
        && value.bytes().any(|byte| byte.is_ascii_alphanumeric())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PolicyParseError;

impl fmt::Display for PolicyParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid policy entry")
    }
}

impl std::error::Error for PolicyParseError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicDecision {
    pub allowed: bool,
    pub record_denial: bool,
    pub effective_mode: Mode,
    pub invalid_input: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalDecision {
    pub allowed: bool,
    pub invalid_input: bool,
}

#[derive(Debug)]
pub(crate) struct PublicPolicySnapshot {
    mode: Mode,
    entries: BTreeSet<PolicyEntry>,
}

impl PublicPolicySnapshot {
    fn empty() -> Self {
        Self {
            mode: Mode::Enforce,
            entries: BTreeSet::new(),
        }
    }

    fn decide(&self, host: &PublicHost, invalid_input: bool) -> PublicDecision {
        let matched = match host {
            PublicHost::Dns(host) => self.entries.iter().any(
                |entry| matches!(entry, PolicyEntry::Domain(pattern) if pattern.matches(host)),
            ),
            PublicHost::Ip(std::net::IpAddr::V4(host)) => {
                self.entries.contains(&PolicyEntry::Ip(*host))
            }
            PublicHost::Ip(std::net::IpAddr::V6(_)) => false,
        };
        match self.mode {
            Mode::Enforce => PublicDecision {
                allowed: matched,
                record_denial: !matched,
                effective_mode: self.mode,
                invalid_input,
            },
            Mode::Report => PublicDecision {
                allowed: true,
                record_denial: !matched,
                effective_mode: self.mode,
                invalid_input,
            },
            Mode::Open => PublicDecision {
                allowed: true,
                record_denial: false,
                effective_mode: self.mode,
                invalid_input,
            },
        }
    }
}

#[derive(Debug)]
pub(crate) struct LocalPolicySnapshot {
    authorities: HashSet<LoopbackAuthority>,
}

impl LocalPolicySnapshot {
    fn empty() -> Self {
        Self {
            authorities: HashSet::new(),
        }
    }

    fn decide(&self, authority: &LoopbackAuthority, invalid_input: bool) -> LocalDecision {
        LocalDecision {
            allowed: self.authorities.contains(authority),
            invalid_input,
        }
    }
}

/// Resolves status without exposing strict-loader diagnostics to the response layer.
pub async fn effective_status_mode(paths: &[PathBuf], mode_path: &Path) -> (Mode, Option<String>) {
    match PolicyReader::load_public_strict(paths, mode_path).await {
        Ok(snapshot) => (snapshot.mode, None),
        Err(_) => (
            Mode::Enforce,
            Some("invalid public policy; enforcing".to_owned()),
        ),
    }
}

/// Compatibility adapter for the current router; live failures are returned as decisions.
pub async fn decide_public(
    paths: &[PathBuf],
    mode_path: &Path,
    host: &PublicHost,
) -> Result<(PublicDecision, Option<String>)> {
    let decision = PolicyReader::decide_public_live(paths, mode_path, host).await;
    let warning = decision
        .invalid_input
        .then(|| "invalid public policy; enforcing".to_owned());
    Ok((decision, warning))
}

/// Compatibility adapter for the current router; local failures deny the decision.
pub async fn decide_local(paths: &[PathBuf; 3], authority: &LoopbackAuthority) -> Result<bool> {
    Ok(PolicyReader::decide_local_live(paths, authority)
        .await
        .allowed)
}

fn parse_mode(contents: &str) -> Result<Mode, PolicyParseError> {
    match contents {
        "enforce" | "enforce\n" => Ok(Mode::Enforce),
        "report" | "report\n" => Ok(Mode::Report),
        "open" | "open\n" => Ok(Mode::Open),
        _ => Err(PolicyParseError),
    }
}

fn parse_domain_layer_at(
    contents: &str,
    path: &Path,
    index: usize,
) -> Result<BTreeSet<PolicyEntry>> {
    parse_domain_layer(contents).map_err(|_| {
        anyhow::anyhow!(
            "malformed public policy layer {index} at {} line {}",
            path.display(),
            first_invalid_line(contents, |line| line.parse::<PolicyEntry>().is_ok())
        )
    })
}

fn parse_local_layer_at(
    contents: &str,
    path: &Path,
    index: usize,
) -> Result<HashSet<LoopbackAuthority>> {
    parse_local_layer(contents).map_err(|_| {
        anyhow::anyhow!(
            "malformed local policy layer {index} at {} line {}",
            path.display(),
            first_invalid_line(contents, is_canonical_local_authority)
        )
    })
}

fn first_invalid_line(contents: &str, valid: impl Fn(&str) -> bool) -> usize {
    strict_lines(contents)
        .ok()
        .and_then(|mut lines| lines.position(|line| !valid(line)))
        .map_or(1, |index| index + 1)
}

pub(crate) fn parse_domain_layer(
    contents: &str,
) -> Result<BTreeSet<PolicyEntry>, PolicyParseError> {
    strict_lines(contents)?
        .map(str::parse)
        .collect::<std::result::Result<_, _>>()
        .map_err(|()| PolicyParseError)
}

pub fn parse_local_layer(contents: &str) -> Result<HashSet<LoopbackAuthority>, PolicyParseError> {
    strict_lines(contents)?
        .map(|line| {
            let authority = line
                .parse::<LoopbackAuthority>()
                .map_err(|_| PolicyParseError)?;
            (authority.to_string() == line)
                .then_some(authority)
                .ok_or(PolicyParseError)
        })
        .collect()
}

fn strict_lines(contents: &str) -> Result<std::vec::IntoIter<&str>, PolicyParseError> {
    if contents.is_empty() {
        return Ok(Vec::new().into_iter());
    }
    let body = contents.strip_suffix('\n').unwrap_or(contents);
    if body.is_empty() {
        return Err(PolicyParseError);
    }
    Ok(body.split('\n').collect::<Vec<_>>().into_iter())
}

fn is_canonical_local_authority(value: &str) -> bool {
    value
        .parse::<LoopbackAuthority>()
        .is_ok_and(|authority| authority.to_string() == value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn host(value: &str) -> PublicHost {
        PublicHost::Dns(value.parse().unwrap())
    }

    fn ipv4(value: &str) -> PublicHost {
        PublicHost::Ip(value.parse().unwrap())
    }

    fn ipv6(value: &str) -> PublicHost {
        PublicHost::Ip(value.parse().unwrap())
    }

    fn atomic_replace(path: &Path, contents: &[u8]) {
        let replacement = path.with_extension("replacement");
        std::fs::write(&replacement, contents).unwrap();
        std::fs::rename(replacement, path).unwrap();
    }

    #[test]
    fn strict_public_storage_corpus_and_line_framing() {
        assert!(parse_domain_layer("").unwrap().is_empty());
        assert!(parse_domain_layer("example.com").is_ok());
        assert!(parse_domain_layer("example.com\n").is_ok());
        for contents in [
            "\n",
            "\r\n",
            "example.com\r\n",
            "\nexample.com",
            "example.com\n\n",
        ] {
            assert!(parse_domain_layer(contents).is_err(), "{contents:?}");
        }

        for row in include_str!("../../testdata/public-policy-storage.tsv")
            .lines()
            .filter(|row| !row.starts_with('#'))
        {
            let fields: Vec<_> = row.split('\t').collect();
            let [value, expected] = fields.as_slice() else {
                panic!("bad fixture row: {row}");
            };
            assert_eq!(
                parse_domain_layer(&format!("{value}\n")).is_ok(),
                *expected == "accept",
                "{row}"
            );
        }
    }

    #[test]
    fn strict_local_storage_is_distinct_from_user_input_normalization() {
        assert!(parse_local_layer("").unwrap().is_empty());
        assert!(parse_local_layer("localhost:80").is_ok());
        assert!(parse_local_layer("localhost:80\n").is_ok());
        assert!(parse_local_layer("localhost:80\nlocalhost:80\n").is_ok());
        for contents in [
            "\n",
            "\r\n",
            "localhost:80\r\n",
            "\nlocalhost:80",
            "localhost:80\n\n",
        ] {
            assert!(parse_local_layer(contents).is_err(), "{contents:?}");
        }

        for row in include_str!("../../../shared/testdata/loopback-authorities.tsv")
            .lines()
            .filter(|row| !row.starts_with('#'))
        {
            let fields: Vec<_> = row.split('\t').collect();
            match fields.as_slice() {
                ["valid", input, canonical] => {
                    assert!(
                        parse_local_layer(&format!("{canonical}\n")).is_ok(),
                        "{row}"
                    );
                    assert_eq!(
                        parse_local_layer(&format!("{input}\n")).is_ok(),
                        input == canonical,
                        "{row}"
                    );
                }
                ["invalid", input] => {
                    assert!(parse_local_layer(&format!("{input}\n")).is_err(), "{row}");
                }
                _ => panic!("bad fixture row: {row}"),
            }
        }

        for row in include_str!("../../testdata/local-policy-storage.tsv")
            .lines()
            .filter(|row| !row.starts_with('#'))
        {
            let fields: Vec<_> = row.split('\t').collect();
            let [value, expected] = fields.as_slice() else {
                panic!("bad fixture row: {row}");
            };
            assert_eq!(
                parse_local_layer(&format!("{value}\n")).is_ok(),
                *expected == "accept",
                "{row}"
            );
        }
    }

    #[test]
    fn mode_accepts_only_exact_storage() {
        for (contents, expected) in [
            ("enforce", Some(Mode::Enforce)),
            ("enforce\n", Some(Mode::Enforce)),
            ("report", Some(Mode::Report)),
            ("report\n", Some(Mode::Report)),
            ("open", Some(Mode::Open)),
            ("open\n", Some(Mode::Open)),
            ("", None),
            ("open\r\n", None),
            (" open\n", None),
            ("open \n", None),
            ("open\nreport\n", None),
            ("unknown\n", None),
        ] {
            assert_eq!(parse_mode(contents).ok(), expected, "{contents:?}");
        }
    }

    #[tokio::test]
    async fn mode_corpus_maps_invalid_input_to_empty_enforce() {
        let directory = tempdir().unwrap();
        let layer = directory.path().join("layer");
        let mode = directory.path().join("mode");
        tokio::fs::write(&layer, "allowed.example\n").await.unwrap();

        for row in include_str!("../../testdata/proxy-modes.tsv")
            .lines()
            .filter(|row| !row.starts_with('#'))
        {
            let fields: Vec<_> = row.split('\t').collect();
            let [stored, state, yes, allowed, recorded, effective, invalid] = fields.as_slice()
            else {
                panic!("bad fixture {row}");
            };
            tokio::fs::write(&layer, "allowed.example\n").await.unwrap();
            tokio::fs::write(&mode, format!("{stored}\n"))
                .await
                .unwrap();
            match *state {
                "missing-mode" => tokio::fs::remove_file(&mode).await.unwrap(),
                "missing-layer" => tokio::fs::remove_file(&layer).await.unwrap(),
                "malformed-layer" => {
                    tokio::fs::write(&layer, "bad!entry\n").await.unwrap();
                }
                "valid" | "invalid-mode" | "replace" => {}
                _ => panic!("bad state in fixture: {row}"),
            }
            let requested = host(if *yes == "yes" {
                "allowed.example"
            } else {
                "blocked.example"
            });
            let decision =
                PolicyReader::decide_public_live(std::slice::from_ref(&layer), &mode, &requested)
                    .await;
            assert_eq!(decision.allowed, *allowed == "yes", "{row}");
            assert_eq!(decision.record_denial, *recorded == "yes", "{row}");
            assert_eq!(decision.effective_mode.as_str(), *effective, "{row}");
            assert_eq!(decision.invalid_input, *invalid == "yes", "{row}");
        }
    }

    #[tokio::test]
    async fn public_layers_are_additive_and_matching_is_exact() {
        let directory = tempdir().unwrap();
        let paths: [PathBuf; 2] = ["one", "two"].map(|name| directory.path().join(name));
        let mode = directory.path().join("mode");
        tokio::fs::write(&paths[0], "example.com\n8.8.8.8\nexample.com\n")
            .await
            .unwrap();
        tokio::fs::write(&paths[1], "other.example\n")
            .await
            .unwrap();
        tokio::fs::write(&mode, "enforce\n").await.unwrap();

        for value in ["example.com", "sub.example.com", "other.example"] {
            assert!(
                PolicyReader::decide_public_live(&paths, &mode, &host(value))
                    .await
                    .allowed,
                "{value}"
            );
        }
        for value in ["evilexample.com", "example.com.attacker.invalid"] {
            assert!(
                !PolicyReader::decide_public_live(&paths, &mode, &host(value))
                    .await
                    .allowed,
                "{value}"
            );
        }
        assert!(
            PolicyReader::decide_public_live(&paths, &mode, &ipv4("8.8.8.8"))
                .await
                .allowed
        );
        assert!(
            !PolicyReader::decide_public_live(&paths, &mode, &ipv4("8.8.8.9"))
                .await
                .allowed
        );
    }

    #[tokio::test]
    async fn ipv6_is_unmatched_in_enforce_and_mode_controls_only_public_policy() {
        let directory = tempdir().unwrap();
        let layer = directory.path().join("layer");
        let mode = directory.path().join("mode");
        tokio::fs::write(&layer, "example.com\n").await.unwrap();
        let address = ipv6("2606:4700:4700::1111");

        for (stored, allowed, record_denial) in [
            ("enforce\n", false, true),
            ("report\n", true, true),
            ("open\n", true, false),
        ] {
            tokio::fs::write(&mode, stored).await.unwrap();
            let decision =
                PolicyReader::decide_public_live(std::slice::from_ref(&layer), &mode, &address)
                    .await;
            assert_eq!(decision.allowed, allowed, "{stored:?}");
            assert_eq!(decision.record_denial, record_denial, "{stored:?}");
            assert!(!decision.invalid_input, "{stored:?}");
        }
    }

    #[tokio::test]
    async fn invalid_public_layer_or_mode_denies_a_match_in_report_and_open() {
        let directory = tempdir().unwrap();
        let layer = directory.path().join("layer");
        let mode = directory.path().join("mode");
        let requested = host("allowed.example");

        for mode_contents in ["report\n", "open\n"] {
            tokio::fs::write(&layer, "bad!entry\n").await.unwrap();
            tokio::fs::write(&mode, mode_contents).await.unwrap();
            let decision =
                PolicyReader::decide_public_live(std::slice::from_ref(&layer), &mode, &requested)
                    .await;
            assert_eq!(
                decision,
                PublicDecision {
                    allowed: false,
                    record_denial: true,
                    effective_mode: Mode::Enforce,
                    invalid_input: true,
                }
            );

            tokio::fs::write(&layer, "allowed.example\n").await.unwrap();
            tokio::fs::write(&mode, format!(" {mode_contents}"))
                .await
                .unwrap();
            let decision =
                PolicyReader::decide_public_live(std::slice::from_ref(&layer), &mode, &requested)
                    .await;
            assert!(!decision.allowed);
            assert_eq!(decision.effective_mode, Mode::Enforce);
            assert!(decision.invalid_input);
        }
    }

    #[tokio::test]
    async fn bad_local_layer_empties_the_union_and_repair_is_live() {
        let directory = tempdir().unwrap();
        let paths: [PathBuf; 3] = [0, 1, 2].map(|index| directory.path().join(index.to_string()));
        for path in &paths {
            tokio::fs::write(path, "").await.unwrap();
        }
        tokio::fs::write(&paths[0], "localhost:80\n").await.unwrap();
        tokio::fs::write(&paths[2], "LOCALHOST:81\n").await.unwrap();
        let authority = "localhost:80".parse().unwrap();
        let failed = PolicyReader::decide_local_live(&paths, &authority).await;
        assert_eq!(
            failed,
            LocalDecision {
                allowed: false,
                invalid_input: true,
            }
        );

        atomic_replace(&paths[2], b"");
        let repaired = PolicyReader::decide_local_live(&paths, &authority).await;
        assert_eq!(
            repaired,
            LocalDecision {
                allowed: true,
                invalid_input: false,
            }
        );
    }

    #[tokio::test]
    async fn same_reader_observes_atomic_repair_between_persistent_requests() {
        let directory = tempdir().unwrap();
        let layer = directory.path().join("layer");
        let mode = directory.path().join("mode");
        tokio::fs::write(&layer, "bad!entry\n").await.unwrap();
        tokio::fs::write(&mode, "enforce\n").await.unwrap();
        let requested = host("allowed.example");

        let failed =
            PolicyReader::decide_public_live(std::slice::from_ref(&layer), &mode, &requested).await;
        assert!(!failed.allowed && failed.invalid_input);

        atomic_replace(&layer, b"allowed.example\n");
        let repaired =
            PolicyReader::decide_public_live(std::slice::from_ref(&layer), &mode, &requested).await;
        assert!(repaired.allowed && !repaired.invalid_input);

        atomic_replace(&layer, b"other.example\n");
        let revoked =
            PolicyReader::decide_public_live(std::slice::from_ref(&layer), &mode, &requested).await;
        assert!(
            !revoked.allowed,
            "the later replacement must revoke the grant"
        );
        assert!(!revoked.invalid_input);
    }

    #[tokio::test]
    async fn strict_loaders_return_contextual_errors() {
        let directory = tempdir().unwrap();
        let public = directory.path().join("public");
        let mode = directory.path().join("mode");
        tokio::fs::write(&public, "bad!entry\n").await.unwrap();
        tokio::fs::write(&mode, "enforce\n").await.unwrap();
        let error = PolicyReader::load_public_strict(std::slice::from_ref(&public), &mode)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("layer 0") && error.contains("line 1"));

        let local: [PathBuf; 3] = [0, 1, 2].map(|index| directory.path().join(index.to_string()));
        for path in &local {
            tokio::fs::write(path, "").await.unwrap();
        }
        tokio::fs::write(&local[1], "localhost:080\n")
            .await
            .unwrap();
        let error = PolicyReader::load_local_strict(&local)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("layer 1") && error.contains("line 1"));
    }

    #[test]
    fn ipv4_leading_zeroes_remain_policy_text_but_never_grant_an_ip() {
        let entries = parse_domain_layer("01.2.3.4\n").unwrap();
        let snapshot = PublicPolicySnapshot {
            mode: Mode::Enforce,
            entries,
        };
        assert!(!snapshot.decide(&ipv4("1.2.3.4"), false).allowed);
    }
}
