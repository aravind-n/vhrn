//! Merged vhrn configuration. Precedence is CLI flags over the global `config.toml`
//! (under `~/.config/vhrn`) over built-in defaults (flags applied in the run path).
//! Config is host-owned only — nothing is read from the project directory, so a cloned
//! repo can never configure the jail. Each optional field is an `Option` so an unset
//! key falls through to a lower-precedence layer.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Result, bail};

/// The merged configuration.
#[derive(Debug, Clone, Default, PartialEq)]
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
#[serde(default, deny_unknown_fields)]
pub(crate) struct RunConfig {
    pub blocked_dirs: Option<Vec<String>>,
}

/// Extra tooling baked onto the harness image at build time. `apt` is sugar for a Debian
/// package install; `run` is arbitrary build-time shell (vendor installers, tarballs, a
/// private mirror) — vhrn stays language-agnostic and just bakes what it is told.
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ToolsConfig {
    pub apt: Option<Vec<String>>,
    pub run: Option<Vec<String>>,
}

/// Folds into the egress policy: extra allowlist domains and the guard mode. `mode`
/// stays a raw `Option<String>` — an unknown value is tolerated here and mapped to
/// enforce (with a warning) at run time, so we don't parse it into an enum yet.
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct NetConfig {
    pub allow: Option<Vec<String>>,
    pub mode: Option<String>,
}

/// Optional container resource limits. An unset value leaves the engine's default in
/// effect, except where the run path supplies an engine-specific default.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ResourcesConfig {
    pub memory: Option<String>,
    pub cpus: Option<u32>,
}

/// Resource values as TOML represents them. CPU stays signed and wide here so semantic errors
/// in an unrelated project do not prevent selecting or prewarming another project.
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ParsedResourcesConfig {
    pub(crate) memory: Option<String>,
    pub(crate) cpus: Option<i64>,
}

/// The host-owned TOML shape. This is intentionally separate from `Config`: callers which
/// prepare a run receive only the values selected for that one canonical cwd.
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ConfigFile {
    pub(crate) run: RunConfig,
    pub(crate) tools: ToolsConfig,
    pub(crate) net: NetConfig,
    pub(crate) resources: ParsedResourcesConfig,
    #[serde(rename = "project")]
    pub(crate) projects: BTreeMap<String, ProjectOverrides>,
}

#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ProjectOverrides {
    pub(crate) tools: ProjectToolsOverrides,
    pub(crate) resources: ProjectResourcesOverrides,
}

#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ProjectToolsOverrides {
    pub(crate) apt: Option<Vec<String>>,
    pub(crate) run: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ProjectResourcesOverrides {
    pub(crate) memory: Option<String>,
    pub(crate) cpus: Option<i64>,
}

/// One distinct effective tools profile to pre-build. The global profile is considered
/// first, followed by projects in lexical key order; duplicate normalized profiles retain
/// every source path so an error tells the user which projects it affects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolsProfile {
    pub(crate) tools: crate::image::NormalizedTools,
    pub(crate) global: bool,
    pub(crate) projects: Vec<String>,
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

/// Read the one host-owned config file. Project keys are validated lexically only: the
/// loader never touches any configured project path.
pub(crate) fn load_config_file(config_dir: &Path) -> Result<ConfigFile> {
    let cfg = read_config_file(&config_dir.join("config.toml"))?.unwrap_or_default();
    for key in cfg.projects.keys() {
        validate_project_key(key)?;
    }
    Ok(cfg)
}

/// Resolve an already parsed host config for one canonical cwd. Keeping this pure makes it
/// impossible for project config to cause filesystem reads before the jail is prepared.
pub(crate) fn resolve_config(file: &ConfigFile, project: &str) -> Result<Config> {
    // Keep this invariant at the pure boundary too; tests and future callers may construct a
    // ConfigFile without going through the filesystem loader.
    for key in file.projects.keys() {
        validate_project_key(key)?;
    }
    let global = Config {
        run: file.run.clone(),
        tools: file.tools.clone(),
        net: file.net.clone(),
        resources: ResourcesConfig {
            memory: file.resources.memory.clone(),
            cpus: None,
        },
    };
    let mut cfg = merge_config(default_config(), global);
    let mut cpus = file.resources.cpus;
    if let Some(over) = file.projects.get(project) {
        if over.tools.apt.is_some() {
            cfg.tools.apt.clone_from(&over.tools.apt);
        }
        if over.tools.run.is_some() {
            cfg.tools.run.clone_from(&over.tools.run);
        }
        if over.resources.memory.is_some() {
            cfg.resources.memory.clone_from(&over.resources.memory);
        }
        if over.resources.cpus.is_some() {
            cpus = over.resources.cpus;
        }
    }
    cfg.resources.cpus = normalize_cpus(cpus)?;
    normalize_config(&mut cfg)?;
    Ok(cfg)
}

/// Load the host file and select the exact canonical project key.
pub(crate) fn load_project_config(config_dir: &Path, project: &str) -> Result<Config> {
    resolve_config(&load_config_file(config_dir)?, project)
}

/// Produce the tools profiles which install/update must prewarm without selecting the
/// command's cwd. This deliberately does not resolve resources, whose semantic validation is
/// run-specific and must not make an unrelated profile unbuildable.
pub(crate) fn tools_profiles(file: &ConfigFile) -> Vec<ToolsProfile> {
    use std::collections::BTreeMap;

    let global_apt = file.tools.apt.clone().unwrap_or_default();
    let global_run = file.tools.run.clone().unwrap_or_default();
    let mut profiles: BTreeMap<crate::image::NormalizedTools, ToolsProfile> = BTreeMap::new();
    let global = crate::image::NormalizedTools::new(&global_apt, &global_run);
    profiles.insert(
        global.clone(),
        ToolsProfile {
            tools: global,
            global: true,
            projects: Vec::new(),
        },
    );
    for (path, over) in &file.projects {
        let apt = over.tools.apt.as_ref().unwrap_or(&global_apt);
        let run = over.tools.run.as_ref().unwrap_or(&global_run);
        let tools = crate::image::NormalizedTools::new(apt, run);
        profiles
            .entry(tools.clone())
            .and_modify(|profile| profile.projects.push(path.clone()))
            .or_insert_with(|| ToolsProfile {
                tools,
                global: false,
                projects: vec![path.clone()],
            });
    }
    // BTreeMap above deduplicates identities but sorts by tools. Reconstruct the promised
    // deterministic source order: global first, then first-associated project key.
    let mut profiles: Vec<_> = profiles.into_values().collect();
    profiles.sort_by(|a, b| match (a.global, b.global) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.projects.first().cmp(&b.projects.first()),
    });
    profiles
}

/// Validate global resource settings once, while config is loaded, so the run path can
/// assemble engine arguments from a known-good value.
fn normalize_config(cfg: &mut Config) -> Result<()> {
    if let Some(memory) = &mut cfg.resources.memory {
        *memory = normalize_memory(memory)?;
    }
    Ok(())
}

fn normalize_cpus(cpus: Option<i64>) -> Result<Option<u32>> {
    let Some(cpus) = cpus else {
        return Ok(None);
    };
    if cpus <= 0 {
        bail!("[resources].cpus must be a positive integer");
    }
    u32::try_from(cpus).map(Some).map_err(|_| {
        anyhow::anyhow!(
            "[resources].cpus must be a positive integer no greater than {max}",
            max = u32::MAX
        )
    })
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
fn read_config_file(path: &Path) -> Result<Option<ConfigFile>> {
    let data = match std::fs::read_to_string(path) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let cfg: ConfigFile =
        toml::from_str(&data).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
    Ok(Some(cfg))
}

/// Project keys are deliberately not fed through `Path::components`: on some platforms it
/// normalizes away a dot component before we can reject it. TOML keys use `/` here because
/// vhrn's supported hosts use POSIX paths; repeated separators are inert but not rewritten.
fn validate_project_key(key: &str) -> Result<()> {
    if !key.starts_with('/') {
        bail!("[project.{key:?}] must be an absolute path");
    }
    if key.split('/').any(|part| matches!(part, "." | "..")) {
        bail!("[project.{key:?}] must not contain '.' or '..' path components");
    }
    Ok(())
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
        let cfg = resolve_config(&load_config_file(dir.path()).unwrap(), "").unwrap();
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

        let cfg = resolve_config(&load_config_file(config_dir.path()).unwrap(), "").unwrap();
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
    fn project_table_is_singular_and_strict() {
        for text in [
            "[projects.\"/work/x\"]\n",
            "unknown = true\n",
            "[project.\"/work/x\".run]\nblocked_dirs = []\n",
            "[project.\"/work/x\".net]\nmode = \"open\"\n",
            "[project.\"/work/x\".tools]\nunknown = []\n",
            "[project.\"/work/x\".project.\"/work/y\"]\n",
        ] {
            let error = toml::from_str::<ConfigFile>(text).unwrap_err();
            assert!(!error.to_string().is_empty(), "{text}");
        }
    }

    #[test]
    fn project_keys_are_lexically_validated_without_filesystem_access() {
        for key in ["relative", "~/work/x", "/work/./x", "/work/x/../y"] {
            let config_dir = temp_dir();
            std::fs::write(
                config_dir.path().join("config.toml"),
                format!("[project.\"{key}\".tools]\napt = []\n"),
            )
            .unwrap();
            assert!(load_config_file(config_dir.path()).is_err(), "{key}");
        }
        let config_dir = temp_dir();
        std::fs::write(
            config_dir.path().join("config.toml"),
            "[project.\"/does/not/exist/*.x\".tools]\napt = []\n",
        )
        .unwrap();
        assert!(load_config_file(config_dir.path()).is_ok());
    }

    #[test]
    fn project_resolution_is_exact_and_fields_overlay_independently() {
        let file: ConfigFile = toml::from_str(
            r#"
                [tools]
                apt = ["global"]
                run = ["global-run"]
                [resources]
                memory = "4g"
                cpus = 2
                [project."/work/a".tools]
                apt = []
                [project."/work/a".resources]
                cpus = 8
            "#,
        )
        .unwrap();
        let selected = resolve_config(&file, "/work/a").unwrap();
        assert_eq!(selected.tools.apt, Some(vec![]));
        assert_eq!(selected.tools.run, Some(vec!["global-run".into()]));
        assert_eq!(selected.resources.memory.as_deref(), Some("4g"));
        assert_eq!(selected.resources.cpus, Some(8));

        // Parent entries, symlink spellings, and absent entries do not inherit.
        for project in ["/work/a/sub", "/symlink/a", "/work/other"] {
            let cfg = resolve_config(&file, project).unwrap();
            assert_eq!(cfg.tools.apt, Some(vec!["global".into()]));
            assert_eq!(cfg.resources.cpus, Some(2));
        }
    }

    #[test]
    fn resources_are_validated_only_after_exact_project_resolution() {
        let file: ConfigFile = toml::from_str(
            r#"
                [resources]
                memory = "not-a-limit"
                [project."/work/good".resources]
                memory = "04G"
                [project."/work/bad".resources]
                cpus = 0
            "#,
        )
        .unwrap();
        let good = resolve_config(&file, "/work/good").unwrap();
        assert_eq!(good.resources.memory.as_deref(), Some("04g"));
        assert!(resolve_config(&file, "/work/other").is_err());
        // The bad project's value must not poison an unrelated selected project.
        assert!(resolve_config(&file, "/work/good").is_ok());
        assert!(resolve_config(&file, "/work/bad").is_err());
    }

    #[test]
    fn project_cannot_override_global_blocked_dirs() {
        let file: ConfigFile = toml::from_str(
            "[run]\nblocked_dirs = [\"/work/x\"]\n[project.\"/work/x\".tools]\napt = []\n",
        )
        .unwrap();
        let cfg = resolve_config(&file, "/work/x").unwrap();
        assert_eq!(cfg.run.blocked_dirs, Some(vec!["/work/x".into()]));
    }

    #[test]
    fn tools_profiles_overlay_deduplicate_and_preserve_associations() {
        let file: ConfigFile = toml::from_str(
            r#"
                [tools]
                apt = [" jq ", "ripgrep", "jq"]
                run = [" global "]
                [project."/z".tools]
                apt = ["ripgrep", "jq"]
                [project."/a".tools]
                apt = []
                [project."/b".tools]
                apt = ["jq", "ripgrep"]
                [project."/c".tools]
                run = ["second", "global", "second"]
            "#,
        )
        .unwrap();
        let profiles = tools_profiles(&file);
        assert_eq!(profiles.len(), 3);
        assert!(profiles[0].global);
        assert_eq!(profiles[0].projects, vec!["/b", "/z"]);
        assert_eq!(profiles[0].tools.apt, vec!["jq", "ripgrep"]);
        assert_eq!(profiles[0].tools.run, vec!["global"]);
        assert_eq!(profiles[1].projects, vec!["/a"]);
        assert!(profiles[1].tools.apt.is_empty());
        assert_eq!(profiles[1].tools.run, vec!["global"]);
        assert_eq!(profiles[2].projects, vec!["/c"]);
        assert_eq!(profiles[2].tools.run, vec!["second", "global", "second"]);
    }

    #[test]
    fn tools_profiles_keep_empty_global_and_project_profiles_for_the_builder_to_skip() {
        let file: ConfigFile = toml::from_str(
            "[project.\"/empty\".tools]\napt = []\n[project.\"/run\".tools]\nrun = [\"x\"]\n",
        )
        .unwrap();
        let profiles = tools_profiles(&file);
        assert_eq!(profiles.len(), 2);
        assert!(profiles[0].global);
        assert_eq!(profiles[0].projects, vec!["/empty"]);
        assert!(profiles[0].tools.is_empty());
        assert_eq!(profiles[1].projects, vec!["/run"]);
    }

    #[test]
    fn load_config_malformed_is_error() {
        let config_dir = temp_dir();
        std::fs::write(
            config_dir.path().join("config.toml"),
            "this is = not valid = toml",
        )
        .unwrap();
        assert!(load_config_file(config_dir.path()).is_err());
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
        let error = resolve_config(&load_config_file(config_dir.path()).unwrap(), "").unwrap_err();
        assert!(error.to_string().contains("[resources].cpus"));
    }

    #[test]
    fn resource_cpus_reject_invalid_toml_types_at_parse_time() {
        for cpus in ["1.5", "\"2\"", "true", "9223372036854775808"] {
            let config_dir = temp_dir();
            std::fs::write(
                config_dir.path().join("config.toml"),
                format!("[resources]\ncpus = {cpus}\n"),
            )
            .unwrap();
            let error = load_config_file(config_dir.path()).unwrap_err();
            assert!(!error.to_string().is_empty(), "{cpus:?}: {error}");
        }
    }

    #[test]
    fn resource_cpus_semantic_values_fail_only_when_selected() {
        for cpus in ["-1", "0", "4294967296"] {
            let config_dir = temp_dir();
            std::fs::write(
                config_dir.path().join("config.toml"),
                format!("[resources]\ncpus = {cpus}\n"),
            )
            .unwrap();
            let file = load_config_file(config_dir.path()).unwrap();
            let error = resolve_config(&file, "").unwrap_err();
            assert!(
                error.to_string().contains("[resources].cpus"),
                "{cpus:?}: {error}"
            );
        }
    }

    #[test]
    fn unrelated_project_cpu_errors_do_not_block_selection_or_tools_profiles() {
        for cpus in ["-1", "4294967296"] {
            let config = format!(
                "[resources]\ncpus = 2\n[project.\"/good\".resources]\ncpus = 4\n[project.\"/bad\".resources]\ncpus = {cpus}\n[project.\"/bad\".tools]\nrun = [\"bad-tools\"]\n"
            );
            let config_dir = temp_dir();
            std::fs::write(config_dir.path().join("config.toml"), config).unwrap();
            let file = load_config_file(config_dir.path()).unwrap();
            assert_eq!(
                resolve_config(&file, "/good").unwrap().resources.cpus,
                Some(4)
            );
            assert!(resolve_config(&file, "/bad").is_err());
            assert_eq!(tools_profiles(&file).len(), 2, "{cpus}");
        }
    }

    #[test]
    fn selected_project_can_replace_invalid_global_cpu() {
        let file: ConfigFile =
            toml::from_str("[resources]\ncpus = -1\n[project.\"/good\".resources]\ncpus = 3\n")
                .unwrap();
        assert_eq!(
            resolve_config(&file, "/good").unwrap().resources.cpus,
            Some(3)
        );
        assert!(resolve_config(&file, "/other").is_err());
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
