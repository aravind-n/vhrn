//! Installed harness state (`name version` per line), used to resolve the run-path
//! image reference. The registry lives under vhrn's XDG config directory.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A registry entry: a harness name and the image version it was installed at
/// (a tag like "v0.2.0" or "latest", or "local" for a make-built image).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct InstalledHarness {
    pub name: String,
    pub version: String,
}

/// The XDG config root (`${XDG_CONFIG_HOME:-~/.config}`). Split from the env read so the
/// resolution is unit-testable without touching process env.
fn xdg_config_root(home: &Path, xdg_config: Option<&str>) -> PathBuf {
    match xdg_config {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => home.join(".config"),
    }
}

/// `$XDG_CONFIG_HOME` when set and non-empty — the edge read behind `xdg_config_root`.
fn xdg_config_home() -> Option<String> {
    std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|v| !v.is_empty())
}

/// vhrn's own config dir (`<xdg>/vhrn`), reading `XDG_CONFIG_HOME` at the edge.
pub(crate) fn vhrn_config_dir(home: &Path) -> PathBuf {
    xdg_config_root(home, xdg_config_home().as_deref()).join("vhrn")
}

fn installed_registry_path(config_dir: &Path) -> PathBuf {
    config_dir.join("installed")
}

/// Installed harnesses sorted by name, de-duplicated by name. Lines are "name
/// version"; a bare "name" defaults to version "latest". `config_dir` is injected.
pub(crate) fn read_installed(config_dir: &Path) -> Vec<InstalledHarness> {
    let Ok(content) = std::fs::read_to_string(installed_registry_path(config_dir)) else {
        return Vec::new();
    };
    let mut by_name: BTreeMap<String, String> = BTreeMap::new();
    for line in content.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let mut fields = t.split_whitespace();
        let Some(name) = fields.next() else { continue };
        let version = fields.next().unwrap_or("latest");
        by_name.insert(name.to_string(), version.to_string());
    }
    by_name
        .into_iter()
        .map(|(name, version)| InstalledHarness { name, version })
        .collect()
}

/// The version a harness is installed at, or `None` if it is not installed.
pub(crate) fn installed_version(config_dir: &Path, name: &str) -> Option<String> {
    read_installed(config_dir)
        .into_iter()
        .find(|h| h.name == name)
        .map(|h| h.version)
}

/// Write the registry atomically (same-dir temp + rename), sorted and de-duplicated
/// by name, one "name version" per line.
pub(crate) fn write_installed(config_dir: &Path, hs: &[InstalledHarness]) -> std::io::Result<()> {
    use std::fmt::Write as _;
    std::fs::create_dir_all(config_dir)?;
    let mut sorted: Vec<&InstalledHarness> = hs.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    let mut buf =
        String::from("# vhrn installed harnesses — managed by `vhrn install`/`uninstall`.\n");
    let mut seen = std::collections::HashSet::new();
    for h in sorted {
        if h.name.is_empty() || !seen.insert(h.name.clone()) {
            continue;
        }
        let version = if h.version.is_empty() {
            "latest"
        } else {
            h.version.as_str()
        };
        let _ = writeln!(buf, "{} {version}", h.name);
    }
    let tmp = config_dir.join(format!(
        "installed.{}.{}",
        std::process::id(),
        next_tmp_id()
    ));
    std::fs::write(&tmp, &buf)?;
    std::fs::rename(&tmp, installed_registry_path(config_dir))
}

/// Record a harness at a version, updating the version if already present.
pub(crate) fn add_installed(config_dir: &Path, name: &str, version: &str) -> std::io::Result<()> {
    let mut hs = read_installed(config_dir);
    if let Some(h) = hs.iter_mut().find(|h| h.name == name) {
        h.version = version.to_string();
    } else {
        hs.push(InstalledHarness {
            name: name.to_string(),
            version: version.to_string(),
        });
    }
    write_installed(config_dir, &hs)
}

pub(crate) fn remove_installed(config_dir: &Path, name: &str) -> std::io::Result<()> {
    let hs: Vec<InstalledHarness> = read_installed(config_dir)
        .into_iter()
        .filter(|h| h.name != name)
        .collect();
    write_installed(config_dir, &hs)
}

// Per-process unique suffix for atomic temp files (os.CreateTemp's role).
fn next_tmp_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CTR: AtomicU64 = AtomicU64::new(0);
    CTR.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::temp_dir;

    #[test]
    fn xdg_config_root_defaults_and_honors_override() {
        let home = Path::new("/home/dev");
        assert_eq!(xdg_config_root(home, None), Path::new("/home/dev/.config"));
        assert_eq!(xdg_config_root(home, Some("/xdg")), Path::new("/xdg"));
        assert_eq!(
            xdg_config_root(home, Some("")),
            Path::new("/home/dev/.config")
        );
    }

    #[test]
    fn installed_registry_add_update_remove() {
        let dir = temp_dir();
        assert!(read_installed(dir.path()).is_empty());
        assert!(installed_version(dir.path(), "claude").is_none());

        add_installed(dir.path(), "claude", "v0.2.0").unwrap();
        add_installed(dir.path(), "codex", "latest").unwrap();
        add_installed(dir.path(), "claude", "v0.3.0").unwrap();

        assert_eq!(
            read_installed(dir.path()),
            vec![
                InstalledHarness {
                    name: "claude".into(),
                    version: "v0.3.0".into()
                },
                InstalledHarness {
                    name: "codex".into(),
                    version: "latest".into()
                },
            ]
        );
        assert_eq!(
            installed_version(dir.path(), "claude").as_deref(),
            Some("v0.3.0")
        );

        remove_installed(dir.path(), "claude").unwrap();
        assert_eq!(
            read_installed(dir.path()),
            vec![InstalledHarness {
                name: "codex".into(),
                version: "latest".into()
            }]
        );
    }

    #[test]
    fn read_installed_bare_name_defaults_latest() {
        let dir = temp_dir();
        std::fs::write(installed_registry_path(dir.path()), "claude\n").unwrap();
        assert_eq!(
            installed_version(dir.path(), "claude").as_deref(),
            Some("latest")
        );
    }
}
