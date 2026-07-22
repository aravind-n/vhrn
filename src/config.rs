//! Merged vhrn configuration. Precedence is project `.vhrn.toml` over global
//! `config.toml` over built-in defaults (CLI flags win over all of it, applied in
//! the run path). Each optional field is an `Option` so an unset key falls through
//! to a lower-precedence layer.

use std::path::Path;

use anyhow::{Result, bail};

/// The merged configuration.
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
#[serde(default)]
pub(crate) struct Config {
    pub run: RunConfig,
    pub toolchains: ToolchainsConfig,
    pub net: NetConfig,
}

/// Guards where a box may launch. `blocked_dirs` are refused as an exact resolved
/// cwd (not a subtree), so ordinary projects under $HOME still run while jailing all
/// of $HOME or / is prevented.
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
#[serde(default)]
pub(crate) struct RunConfig {
    pub blocked_dirs: Option<Vec<String>>,
}

/// Tools provisioned into the box beyond the base image, e.g. "go@1.26", "node@22".
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
#[serde(default)]
pub(crate) struct ToolchainsConfig {
    pub tools: Option<Vec<String>>,
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

/// The lowest-precedence layer.
fn default_config() -> Config {
    Config {
        run: RunConfig { blocked_dirs: Some(vec!["~".into(), "/".into()]) },
        toolchains: ToolchainsConfig::default(),
        net: NetConfig { allow: None, mode: Some("enforce".into()) },
    }
}

/// Overlay `over` onto `base`: a field wins only when it is set (`Some`), so an
/// unspecified key falls through to the lower-precedence layer.
fn merge_config(base: Config, over: Config) -> Config {
    let mut out = base;
    if over.run.blocked_dirs.is_some() {
        out.run.blocked_dirs = over.run.blocked_dirs;
    }
    if over.toolchains.tools.is_some() {
        out.toolchains.tools = over.toolchains.tools;
    }
    if over.net.allow.is_some() {
        out.net.allow = over.net.allow;
    }
    if over.net.mode.is_some() {
        out.net.mode = over.net.mode;
    }
    out
}

/// Read and merge the config layers in precedence order — built-in defaults, then
/// the global `config.toml` under `config_dir`, then the project's `.vhrn.toml`.
/// Missing files are not an error; a malformed one is. `config_dir` is injected (the
/// caller resolves it from XDG) so this is testable without touching process env.
pub(crate) fn load_config(config_dir: &Path, project: &Path) -> Result<Config> {
    let mut cfg = default_config();
    for path in [config_dir.join("config.toml"), project.join(".vhrn.toml")] {
        if let Some(c) = read_config_file(&path)? {
            cfg = merge_config(cfg, c);
        }
    }
    Ok(cfg)
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
/// against the physical cwd (which `prepare_box` has already resolved). Falls back to
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
        let cfg = load_config(&temp_dir(), &temp_dir()).unwrap();
        assert_eq!(cfg, default_config());
    }

    #[test]
    fn load_config_precedence() {
        let config_dir = temp_dir();
        std::fs::write(
            config_dir.join("config.toml"),
            "[toolchains]\ntools = [\"go@1.26\"]\n[net]\nmode = \"report\"\nallow = [\"global.example\"]\n",
        )
        .unwrap();
        let project = temp_dir();
        std::fs::write(project.join(".vhrn.toml"), "[net]\nallow = [\"project.example\"]\n").unwrap();

        let cfg = load_config(&config_dir, &project).unwrap();
        assert_eq!(cfg.net.allow, Some(vec!["project.example".to_string()])); // project overrides
        assert_eq!(cfg.net.mode, Some("report".to_string())); // inherited from global
        assert_eq!(cfg.toolchains.tools, Some(vec!["go@1.26".to_string()]));
        assert_eq!(cfg.run.blocked_dirs, Some(vec!["~".to_string(), "/".to_string()])); // default
    }

    #[test]
    fn load_config_malformed_is_error() {
        let project = temp_dir();
        std::fs::write(project.join(".vhrn.toml"), "this is = not valid = toml").unwrap();
        assert!(load_config(&temp_dir(), &project).is_err());
    }

    #[test]
    fn check_blocked_dir_exact_match_only() {
        let home = temp_dir();
        let home = home.to_str().unwrap();
        let blocked = vec!["~".to_string(), "/".to_string()];

        // Exact $HOME and exact / are refused.
        assert!(check_blocked_dir(home, home, &blocked).is_err(), "cwd == $HOME should be blocked");
        assert!(check_blocked_dir("/", home, &["/".to_string()]).is_err(), "cwd == / should be blocked");

        // A subdirectory of home is allowed — exact-match, not subtree.
        let sub = Path::new(home).join("projects").join("x");
        std::fs::create_dir_all(&sub).unwrap();
        let sub = sub.to_str().unwrap();
        assert!(check_blocked_dir(sub, home, &blocked).is_ok(), "a project under $HOME must run");

        // No blocked dirs -> nothing refused.
        assert!(check_blocked_dir(home, home, &[]).is_ok(), "empty blocked list should allow anything");
    }

    #[test]
    fn merge_overlays_only_set_fields() {
        let over = Config {
            net: NetConfig { allow: Some(vec!["x".into()]), mode: None },
            ..Config::default()
        };
        let merged = merge_config(default_config(), over);
        assert_eq!(merged.net.allow, Some(vec!["x".to_string()])); // set in over
        assert_eq!(merged.net.mode.as_deref(), Some("enforce")); // inherited from default
        assert_eq!(merged.run.blocked_dirs, Some(vec!["~".to_string(), "/".to_string()])); // inherited
        assert_eq!(merged.toolchains.tools, None); // set nowhere
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
