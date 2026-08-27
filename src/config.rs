//! Merged vhrn configuration. Precedence is CLI flags over the global `config.toml`
//! (under `~/.config/vhrn`) over built-in defaults (flags applied in the run path).
//! Config is host-owned only — nothing is read from the project directory, so a cloned
//! repo can never configure the jail. Each optional field is an `Option` so an unset
//! key falls through to a lower-precedence layer.

use std::path::Path;

use anyhow::{Result, bail};

/// The merged configuration.
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
#[serde(default)]
pub(crate) struct Config {
    pub run: RunConfig,
    pub tools: ToolsConfig,
    pub net: NetConfig,
    pub resources: ResourcesConfig,
}

/// Guards where a container may launch. `blocked_dirs` are refused as an exact resolved
/// cwd (not a subtree), so ordinary projects under $HOME still run while jailing all
/// of $HOME or / is prevented.
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
#[serde(default)]
pub(crate) struct RunConfig {
    pub blocked_dirs: Option<Vec<String>>,
}

/// Extra tooling baked onto the harness image at build time. `apt` is sugar for a Debian
/// package install; `run` is arbitrary build-time shell (vendor installers, tarballs, a
/// private mirror) — vhrn stays language-agnostic and just bakes what it is told.
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
#[serde(default)]
pub(crate) struct ToolsConfig {
    pub apt: Option<Vec<String>>,
    pub run: Option<Vec<String>>,
}

/// Folds into the egress policy: extra allowlist domains and the guard mode. `mode`
/// stays a raw `Option<String>` — an unknown value is tolerated here and mapped to
/// enforce (with a warning) at run time, so we don't parse it into an enum yet.
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
#[serde(default)]
pub(crate) struct NetConfig {
    pub allow: Option<Vec<String>>,
    pub mode: Option<String>,
}

/// Optional container resource limits. An unset value leaves the engine's default in
/// effect, except where the run path supplies an engine-specific default.
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
#[serde(default)]
pub(crate) struct ResourcesConfig {
    pub memory: Option<String>,
    #[serde(deserialize_with = "deserialize_optional_cpus")]
    pub cpus: Option<u32>,
}

/// Keep TOML's strict unsigned-integer parsing while giving malformed values an error
/// that names the user-facing setting.
fn deserialize_optional_cpus<'de, D>(deserializer: D) -> std::result::Result<Option<u32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    <Option<u32> as serde::Deserialize>::deserialize(deserializer)
        .map_err(|_| serde::de::Error::custom("[resources].cpus must be a positive integer"))
}

/// The lowest-precedence layer.
fn default_config() -> Config {
    Config {
        run: RunConfig {
            blocked_dirs: Some(vec!["~".into(), "/".into()]),
        },
        tools: ToolsConfig::default(),
        net: NetConfig {
            allow: None,
            mode: Some("enforce".into()),
        },
        resources: ResourcesConfig::default(),
    }
}

/// Overlay `over` onto `base`: a field wins only when it is set (`Some`), so an
/// unspecified key falls through to the lower-precedence layer.
fn merge_config(base: Config, over: Config) -> Config {
    let mut out = base;
    if over.run.blocked_dirs.is_some() {
        out.run.blocked_dirs = over.run.blocked_dirs;
    }
    if over.tools.apt.is_some() {
        out.tools.apt = over.tools.apt;
    }
    if over.tools.run.is_some() {
        out.tools.run = over.tools.run;
    }
    if over.net.allow.is_some() {
        out.net.allow = over.net.allow;
    }
    if over.net.mode.is_some() {
        out.net.mode = over.net.mode;
    }
    if over.resources.memory.is_some() {
        out.resources.memory = over.resources.memory;
    }
    if over.resources.cpus.is_some() {
        out.resources.cpus = over.resources.cpus;
    }
    out
}

/// Read the global config: built-in defaults overlaid with `config.toml` under
/// `config_dir`. A missing file is not an error; a malformed one is. `config_dir` is
/// injected (the caller resolves it from XDG) so this is testable without touching process
/// env. Nothing is read from the project directory — config is host-owned, so repo content
/// cannot configure the jail.
pub(crate) fn load_config(config_dir: &Path) -> Result<Config> {
    let mut cfg = default_config();
    if let Some(c) = read_config_file(&config_dir.join("config.toml"))? {
        cfg = merge_config(cfg, c);
    }
    normalize_config(&mut cfg)?;
    Ok(cfg)
}

/// Validate global resource settings once, while config is loaded, so the run path can
/// assemble engine arguments from a known-good value.
fn normalize_config(cfg: &mut Config) -> Result<()> {
    if let Some(memory) = &mut cfg.resources.memory {
        *memory = normalize_memory(memory)?;
    }
    if cfg.resources.cpus == Some(0) {
        bail!("[resources].cpus must be greater than zero");
    }
    Ok(())
}

/// Normalize a portable memory value while preserving its numeric spelling. Engines
/// accept only a unit-bearing amount here; `engine` explicitly requests their default.
fn normalize_memory(memory: &str) -> Result<String> {
    if memory.eq_ignore_ascii_case("engine") {
        return Ok("engine".to_string());
    }

    if !memory.is_ascii() || memory.len() < 2 {
        bail!("[resources].memory must be 'engine' or a nonzero integer ending in m or g");
    }
    let (amount, unit) = memory.split_at(memory.len() - 1);
    if amount.is_empty()
        || !amount.bytes().all(|byte| byte.is_ascii_digit())
        || !amount.bytes().any(|byte| byte != b'0')
    {
        bail!("[resources].memory must be 'engine' or a nonzero integer ending in m or g");
    }
    let Some(unit) = unit.as_bytes().first().copied() else {
        bail!("[resources].memory must be 'engine' or a nonzero integer ending in m or g");
    };
    let unit = match unit {
        b'm' | b'M' => 'm',
        b'g' | b'G' => 'g',
        _ => bail!("[resources].memory must be 'engine' or a nonzero integer ending in m or g"),
    };
    Ok(format!("{amount}{unit}"))
}

/// Parse one TOML config file; a missing file yields `None`.
fn read_config_file(path: &Path) -> Result<Option<Config>> {
    let data = match std::fs::read_to_string(path) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let cfg: Config =
        toml::from_str(&data).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
    Ok(Some(cfg))
}

/// Refuse to launch when the resolved cwd exactly matches a blocked dir. The match
/// is exact, not subtree: subtree-blocking ~ would refuse every project under $HOME,
/// so exact-match is what prevents jailing all of $HOME or / while leaving ordinary
/// projects runnable.
pub(crate) fn check_blocked_dir(project: &str, home: &str, blocked: &[String]) -> Result<()> {
    for b in blocked {
        if resolve_dir(b, home) == project {
            bail!("refusing to run in {project} (blocked_dirs); cd into a project subdirectory");
        }
    }
    Ok(())
}

/// Expand a leading `~` then resolve symlinks so a blocked entry can be compared
/// against the physical cwd (which `prepare_container` has already resolved). Falls back to
/// a lexical clean when the path does not exist.
fn resolve_dir(p: &str, home: &str) -> String {
    let expanded = if p == "~" {
        home.to_string()
    } else if let Some(rest) = p.strip_prefix("~/") {
        Path::new(home).join(rest).to_string_lossy().into_owned()
    } else {
        p.to_string()
    };
    match std::fs::canonicalize(&expanded) {
        Ok(r) => r.to_string_lossy().into_owned(),
        Err(_) => clean_path(&expanded),
    }
}

/// Lexically clean a path: collapse redundant
/// separators, drop `.`, resolve `..` against the preceding element, and never let
/// `..` climb above a rooted path. Only the fallback for a non-existent path.
fn clean_path(p: &str) -> String {
    if p.is_empty() {
        return ".".to_string();
    }
    let rooted = p.starts_with('/');
    let mut out: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if out.last().is_some_and(|s| *s != "..") {
                    out.pop();
                } else if !rooted {
                    out.push("..");
                }
            }
            s => out.push(s),
        }
    }
    let joined = out.join("/");
    if rooted {
        format!("/{joined}")
    } else if joined.is_empty() {
        ".".to_string()
    } else {
        joined
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::temp_dir;

    #[test]
    fn load_config_no_files_yields_defaults() {
        let dir = temp_dir();
        let cfg = load_config(dir.path()).unwrap();
        assert_eq!(cfg, default_config());
    }

    #[test]
    fn load_config_global_over_defaults() {
        let config_dir = temp_dir();
        std::fs::write(
            config_dir.path().join("config.toml"),
            "[tools]\napt = [\"ripgrep\"]\nrun = [\"curl https://example.test | sh\"]\n[net]\nmode = \"report\"\nallow = [\"global.example\"]\n[resources]\nmemory = \"04G\"\ncpus = 2\n",
        )
        .unwrap();

        let cfg = load_config(config_dir.path()).unwrap();
        assert_eq!(cfg.net.allow, Some(vec!["global.example".to_string()])); // from global config
        assert_eq!(cfg.net.mode, Some("report".to_string()));
        assert_eq!(cfg.tools.apt, Some(vec!["ripgrep".to_string()]));
        assert_eq!(
            cfg.tools.run,
            Some(vec!["curl https://example.test | sh".to_string()])
        );
        assert_eq!(
            cfg.run.blocked_dirs,
            Some(vec!["~".to_string(), "/".to_string()])
        ); // unset key falls through to the default
        assert_eq!(cfg.resources.memory.as_deref(), Some("04g"));
        assert_eq!(cfg.resources.cpus, Some(2));
    }

    #[test]
    fn load_config_malformed_is_error() {
        let config_dir = temp_dir();
        std::fs::write(
            config_dir.path().join("config.toml"),
            "this is = not valid = toml",
        )
        .unwrap();
        assert!(load_config(config_dir.path()).is_err());
    }

    #[test]
    fn check_blocked_dir_exact_match_only() {
        let home_dir = temp_dir();
        let home = home_dir.path().to_str().unwrap();
        let blocked = vec!["~".to_string(), "/".to_string()];

        // Exact $HOME and exact / are refused.
        assert!(
            check_blocked_dir(home, home, &blocked).is_err(),
            "cwd == $HOME should be blocked"
        );
        assert!(
            check_blocked_dir("/", home, &["/".to_string()]).is_err(),
            "cwd == / should be blocked"
        );

        // A subdirectory of home is allowed — exact-match, not subtree.
        let sub = Path::new(home).join("projects").join("x");
        std::fs::create_dir_all(&sub).unwrap();
        let sub = sub.to_str().unwrap();
        assert!(
            check_blocked_dir(sub, home, &blocked).is_ok(),
            "a project under $HOME must run"
        );

        // No blocked dirs -> nothing refused.
        assert!(
            check_blocked_dir(home, home, &[]).is_ok(),
            "empty blocked list should allow anything"
        );
    }

    #[test]
    fn merge_overlays_only_set_fields() {
        let over = Config {
            net: NetConfig {
                allow: Some(vec!["x".into()]),
                mode: None,
            },
            ..Config::default()
        };
        let merged = merge_config(default_config(), over);
        assert_eq!(merged.net.allow, Some(vec!["x".to_string()])); // set in over
        assert_eq!(merged.net.mode.as_deref(), Some("enforce")); // inherited from default
        assert_eq!(
            merged.run.blocked_dirs,
            Some(vec!["~".to_string(), "/".to_string()])
        ); // inherited
        assert_eq!(merged.tools.apt, None); // set nowhere
        assert_eq!(merged.tools.run, None);
    }

    #[test]
    fn resource_memory_normalizes_keyword_and_units() {
        assert_eq!(normalize_memory("ENGINE").unwrap(), "engine");
        assert_eq!(normalize_memory("4M").unwrap(), "4m");
        assert_eq!(normalize_memory("004G").unwrap(), "004g");
    }

    #[test]
    fn resource_memory_rejects_malformed_and_zero_values() {
        for memory in [
            "", "engine ", " 4g", "4g ", "+4g", "-4g", "4.0g", "4", "4k", "g", "0m", "00G",
            "fourg", "4㎇",
        ] {
            let error = normalize_memory(memory).unwrap_err();
            assert!(
                error.to_string().contains("[resources].memory"),
                "{memory:?}: {error}"
            );
        }
    }

    #[test]
    fn resource_cpus_must_be_positive() {
        let config_dir = temp_dir();
        std::fs::write(
            config_dir.path().join("config.toml"),
            "[resources]\ncpus = 0\n",
        )
        .unwrap();
        let error = load_config(config_dir.path()).unwrap_err();
        assert!(error.to_string().contains("[resources].cpus"));
    }

    #[test]
    fn resource_cpus_reject_invalid_toml_values() {
        for cpus in ["-1", "1.5", "4294967296", "\"2\""] {
            let config_dir = temp_dir();
            std::fs::write(
                config_dir.path().join("config.toml"),
                format!("[resources]\ncpus = {cpus}\n"),
            )
            .unwrap();
            let error = load_config(config_dir.path()).unwrap_err();
            assert!(
                error.to_string().contains("[resources].cpus"),
                "{cpus:?}: {error}"
            );
        }
    }

    #[test]
    fn merge_resources_fall_through_independently() {
        let base = Config {
            resources: ResourcesConfig {
                memory: Some("4g".into()),
                cpus: Some(4),
            },
            ..Config::default()
        };
        let over = Config {
            resources: ResourcesConfig {
                memory: None,
                cpus: Some(2),
            },
            ..Config::default()
        };
        let merged = merge_config(base, over);
        assert_eq!(merged.resources.memory.as_deref(), Some("4g"));
        assert_eq!(merged.resources.cpus, Some(2));
    }

    #[test]
    fn clean_path_normalizes() {
        assert_eq!(clean_path("/a/../b"), "/b");
        assert_eq!(clean_path("/.."), "/");
        assert_eq!(clean_path("/"), "/");
        assert_eq!(clean_path("a/b/"), "a/b");
        assert_eq!(clean_path("a/../.."), "..");
        assert_eq!(clean_path(""), ".");
        assert_eq!(clean_path("/a//b/./c"), "/a/b/c");
    }
}
