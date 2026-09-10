//! Live policy parsing and evaluation.

use std::collections::{BTreeSet, HashSet};

use vhrn_policy::{
    LoopbackAuthority, Mode, domain_entry_matches, normalize_domain_entry, normalize_domain_host,
};

/// A policy layer contains an invalid record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PolicyParseError;

impl std::fmt::Display for PolicyParseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("invalid policy layer")
    }
}
impl std::error::Error for PolicyParseError {}

/// The result of a public-policy decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicDecision {
    pub allowed: bool,
    pub record_denial: bool,
    pub mode: Mode,
}

/// Reads public policy files for one decision and applies their current contents.
#[must_use]
pub fn decide_public(paths: &[String], mode_path: &str, host: &str) -> PublicDecision {
    let Ok(mode) = read_mode(mode_path) else {
        return PublicDecision {
            allowed: false,
            record_denial: true,
            mode: Mode::Enforce,
        };
    };
    let entries = read_domain_layers(paths);
    let matched = entries.as_ref().is_ok_and(|entries| {
        normalize_domain_host(host).is_ok_and(|host| {
            entries
                .iter()
                .any(|entry| domain_entry_matches(entry, &host))
        })
    });
    if entries.is_err() {
        return PublicDecision {
            allowed: false,
            record_denial: true,
            mode: Mode::Enforce,
        };
    }
    match mode {
        Mode::Enforce => PublicDecision {
            allowed: matched,
            record_denial: !matched,
            mode,
        },
        Mode::Report => PublicDecision {
            allowed: true,
            record_denial: !matched,
            mode,
        },
        Mode::Open => PublicDecision {
            allowed: true,
            record_denial: false,
            mode,
        },
    }
}

/// Reads the three local policy layers and grants if any layer contains the authority.
#[must_use]
pub fn decide_local(paths: &[String; 3], authority: &LoopbackAuthority) -> bool {
    read_local_layers(paths).is_ok_and(|entries| entries.contains(authority))
}

fn read_mode(path: &str) -> Result<Mode, std::io::Error> {
    let contents = std::fs::read_to_string(path)?;
    if contents.lines().count() != 1 {
        return Ok(Mode::Enforce);
    }
    Ok(Mode::readable_or_enforce(
        contents.trim_end_matches(['\r', '\n']),
    ))
}
fn read_domain_layers(paths: &[String]) -> Result<BTreeSet<String>, PolicyParseError> {
    if paths.is_empty() {
        return Err(PolicyParseError);
    }
    let mut all = BTreeSet::new();
    for path in paths {
        if path.is_empty() {
            return Err(PolicyParseError);
        }
        let contents = std::fs::read_to_string(path).map_err(|_| PolicyParseError)?;
        let parsed = parse_domain_layer(&contents)?;
        all.extend(parsed);
    }
    Ok(all)
}
fn read_local_layers(paths: &[String; 3]) -> Result<HashSet<LoopbackAuthority>, PolicyParseError> {
    let mut all = HashSet::new();
    for path in paths {
        let contents = std::fs::read_to_string(path).map_err(|_| PolicyParseError)?;
        let parsed = parse_local_layer(&contents)?;
        all.extend(parsed);
    }
    Ok(all)
}
/// Parses one public layer without filesystem access.
///
/// # Errors
///
/// Returns an error when any record is invalid. A zero-byte layer is a valid
/// empty set; a blank record remains invalid.
pub fn parse_domain_layer(contents: &str) -> Result<BTreeSet<String>, PolicyParseError> {
    contents
        .lines()
        .map(normalize_domain_entry)
        .collect::<Result<_, _>>()
        .map_err(|_| PolicyParseError)
}
/// Parses one local layer without filesystem access.
///
/// # Errors
///
/// Returns an error when any record is invalid. A zero-byte layer is a valid
/// empty set; a blank record remains invalid.
pub fn parse_local_layer(contents: &str) -> Result<HashSet<LoopbackAuthority>, PolicyParseError> {
    contents
        .lines()
        .map(LoopbackAuthority::parse)
        .collect::<Result<_, _>>()
        .map_err(|_| PolicyParseError)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn applies_mode_corpus() {
        let directory = tempdir().unwrap();
        let layer = directory.path().join("layer");
        let mode = directory.path().join("mode");
        fs::write(&layer, "allowed.example\n").unwrap();
        for row in include_str!("../../testdata/proxy-modes.tsv")
            .lines()
            .filter(|r| !r.starts_with('#'))
        {
            let fields: Vec<_> = row.split('\t').collect();
            let [stored, state, host, allowed, recorded, effective] = fields.as_slice() else {
                panic!("bad row: {row}")
            };
            if *state == "valid" {
                fs::write(&mode, format!("{stored}\n")).unwrap();
            } else if *state == "replace" {
                fs::write(&layer, "allowed.example\n").unwrap();
                fs::write(&mode, format!("{stored}\n")).unwrap();
            } else if *state == "missing-mode" {
                let _ = fs::remove_file(&mode);
            } else if *state == "missing-layer" || *state == "read-error" {
                let _ = fs::remove_file(&layer);
                fs::write(&mode, format!("{stored}\n")).unwrap();
            } else {
                fs::write(&layer, "bad!entry\n").unwrap();
                fs::write(&mode, format!("{stored}\n")).unwrap();
            }
            let decision = decide_public(
                &[layer.display().to_string()],
                &mode.display().to_string(),
                if *host == "yes" {
                    "allowed.example"
                } else {
                    "blocked.example"
                },
            );
            assert_eq!(decision.allowed, *allowed == "yes", "{row}");
            assert_eq!(decision.record_denial, *recorded == "yes", "{row}");
            assert_eq!(decision.mode.as_str(), *effective, "{row}");
            fs::write(&layer, "allowed.example\n").unwrap();
        }
    }
    #[test]
    fn reopening_observes_current_contents() {
        let directory = tempdir().unwrap();
        let layer = directory.path().join("layer");
        let mode = directory.path().join("mode");
        fs::write(&layer, "one.example\n").unwrap();
        fs::write(&mode, "enforce\n").unwrap();
        let paths = [layer.display().to_string()];
        assert!(decide_public(&paths, &mode.display().to_string(), "one.example").allowed);
        fs::write(&layer, "two.example\n").unwrap();
        assert!(!decide_public(&paths, &mode.display().to_string(), "one.example").allowed);
    }
    #[test]
    fn zero_byte_layers_are_valid_but_blank_records_are_not() {
        assert!(parse_domain_layer("").unwrap().is_empty());
        assert!(parse_local_layer("").unwrap().is_empty());
        for contents in ["\n", "\r\n"] {
            assert!(parse_domain_layer(contents).is_err(), "{contents:?}");
            assert!(parse_local_layer(contents).is_err(), "{contents:?}");
        }
    }
    #[test]
    fn local_requires_readable_well_formed_layers() {
        let directory = tempdir().unwrap();
        let paths: [String; 3] =
            [0, 1, 2].map(|n| directory.path().join(n.to_string()).display().to_string());
        let target = LoopbackAuthority::parse("localhost:80").unwrap();
        for grant in 0..3 {
            for (index, path) in paths.iter().enumerate() {
                fs::write(path, if index == grant { "localhost:80\n" } else { "" }).unwrap();
            }
            assert!(decide_local(&paths, &target));
        }
        for path in &paths {
            fs::write(path, "").unwrap();
        }
        assert!(!decide_local(&paths, &target));
        fs::write(&paths[0], "localhost:80\n").unwrap();
        fs::write(&paths[1], "\n").unwrap();
        assert!(!decide_local(&paths, &target));
        fs::write(&paths[1], "not an authority\n").unwrap();
        assert!(!decide_local(&paths, &target));
        fs::remove_file(&paths[1]).unwrap();
        assert!(!decide_local(&paths, &target));
    }
    #[test]
    fn domain_corpus_is_accepted_by_layer_parser() {
        for row in include_str!("../../testdata/domain-policy.tsv")
            .lines()
            .filter(|r| !r.starts_with('#'))
        {
            let fields: Vec<_> = row.split('\t').collect();
            if let ["entry", input, _, _, outcome] = fields.as_slice() {
                assert_eq!(
                    parse_domain_layer(&format!("{input}\n")).is_ok(),
                    *outcome == "accept"
                );
            }
        }
    }
    #[test]
    fn public_layers_union_and_fail_closed() {
        let directory = tempdir().unwrap();
        let paths: [String; 5] = ["base", "harness", "global", "project", "run"]
            .map(|name| directory.path().join(name).display().to_string());
        let mode = directory.path().join("mode");
        for path in &paths {
            fs::write(path, "").unwrap();
        }
        fs::write(&paths[2], "one.example\none.example\n").unwrap();
        fs::write(&paths[4], "two.example\n").unwrap();
        fs::write(&mode, "enforce\n").unwrap();
        assert!(decide_public(&paths, &mode.display().to_string(), "two.example").allowed);
        assert!(!decide_public(&[], &mode.display().to_string(), "two.example").allowed);
        for contents in ["\n", "bad!entry\n"] {
            fs::write(&paths[1], contents).unwrap();
            for stored_mode in ["report\n", "open\n"] {
                fs::write(&mode, stored_mode).unwrap();
                assert!(!decide_public(&paths, &mode.display().to_string(), "one.example").allowed);
            }
        }
        fs::remove_file(&paths[1]).unwrap();
        assert!(!decide_public(&paths, &mode.display().to_string(), "one.example").allowed);
    }
    #[test]
    fn readable_unknown_and_multiline_modes_enforce() {
        let directory = tempdir().unwrap();
        let layer = directory.path().join("layer");
        let mode = directory.path().join("mode");
        fs::write(&layer, "allowed.example\n").unwrap();
        for contents in ["strange\n", "open\nreport\n"] {
            fs::write(&mode, contents).unwrap();
            assert!(
                decide_public(
                    &[layer.display().to_string()],
                    &mode.display().to_string(),
                    "allowed.example"
                )
                .allowed
            );
            assert!(
                !decide_public(
                    &[layer.display().to_string()],
                    &mode.display().to_string(),
                    "blocked.example"
                )
                .allowed
            );
        }
    }
    #[test]
    fn each_local_layer_can_grant_and_reopens() {
        let directory = tempdir().unwrap();
        let paths: [String; 3] =
            [0, 1, 2].map(|n| directory.path().join(n.to_string()).display().to_string());
        let authority = LoopbackAuthority::parse("localhost:80").unwrap();
        for grant in 0..3 {
            for (index, path) in paths.iter().enumerate() {
                fs::write(path, if index == grant { "localhost:80\n" } else { "" }).unwrap();
            }
            assert!(decide_local(&paths, &authority));
        }
        for path in &paths {
            fs::write(path, "127.0.0.1:81\n").unwrap();
        }
        assert!(!decide_local(&paths, &authority));
        fs::write(&paths[0], "localhost:80\n").unwrap();
        assert!(decide_local(&paths, &authority));
        fs::remove_file(&paths[2]).unwrap();
        assert!(!decide_local(&paths, &authority));
    }
}
