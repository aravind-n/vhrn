//! Host state: the installed registry (`name version` per line, the source of truth
//! for the run-path image ref and the aliases) and the reversible, marker-delimited
//! shell-alias blocks regenerated from it. `command <name>` / `\<name>` still reach
//! the real binary.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::harness::{Harness, lookup_harness};

// ---- installed registry ---------------------------------------------------------

/// A registry entry: a harness name and the image version it was installed at
/// (a tag like "v0.2.0" or "latest", or "local" for a make-built image).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct InstalledHarness {
    pub name: String,
    pub version: String,
}

/// The XDG config root for vhrn (`${XDG_CONFIG_HOME:-~/.config}/vhrn`). Split from the
/// env read so the resolution is unit-testable without touching process env.
fn vhrn_config_dir_from(home: &Path, xdg_config: Option<&str>) -> PathBuf {
    let base = match xdg_config {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => home.join(".config"),
    };
    base.join("vhrn")
}

/// The XDG config root for vhrn, reading `XDG_CONFIG_HOME` at the edge.
pub(crate) fn vhrn_config_dir(home: &Path) -> PathBuf {
    vhrn_config_dir_from(home, std::env::var("XDG_CONFIG_HOME").ok().as_deref())
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
    by_name.into_iter().map(|(name, version)| InstalledHarness { name, version }).collect()
}

/// The version a harness is installed at, or `None` if it is not installed.
pub(crate) fn installed_version(config_dir: &Path, name: &str) -> Option<String> {
    read_installed(config_dir).into_iter().find(|h| h.name == name).map(|h| h.version)
}

/// Write the registry atomically (same-dir temp + rename), sorted and de-duplicated
/// by name, one "name version" per line.
pub(crate) fn write_installed(config_dir: &Path, hs: &[InstalledHarness]) -> std::io::Result<()> {
    use std::fmt::Write as _;
    std::fs::create_dir_all(config_dir)?;
    let mut sorted: Vec<&InstalledHarness> = hs.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    let mut buf = String::from("# vhrn installed harnesses — managed by `vhrn install`/`uninstall`.\n");
    let mut seen = std::collections::HashSet::new();
    for h in sorted {
        if h.name.is_empty() || !seen.insert(h.name.clone()) {
            continue;
        }
        let version = if h.version.is_empty() { "latest" } else { h.version.as_str() };
        let _ = writeln!(buf, "{} {version}", h.name);
    }
    let tmp = config_dir.join(format!("installed.{}.{}", std::process::id(), next_tmp_id()));
    std::fs::write(&tmp, &buf)?;
    std::fs::rename(&tmp, installed_registry_path(config_dir))
}

/// Record a harness at a version, updating the version if already present.
pub(crate) fn add_installed(config_dir: &Path, name: &str, version: &str) -> std::io::Result<()> {
    let mut hs = read_installed(config_dir);
    if let Some(h) = hs.iter_mut().find(|h| h.name == name) {
        h.version = version.to_string();
    } else {
        hs.push(InstalledHarness { name: name.to_string(), version: version.to_string() });
    }
    write_installed(config_dir, &hs)
}

pub(crate) fn remove_installed(config_dir: &Path, name: &str) -> std::io::Result<()> {
    let hs: Vec<InstalledHarness> =
        read_installed(config_dir).into_iter().filter(|h| h.name != name).collect();
    write_installed(config_dir, &hs)
}

// Per-process unique suffix for atomic temp files (os.CreateTemp's role).
fn next_tmp_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CTR: AtomicU64 = AtomicU64::new(0);
    CTR.fetch_add(1, Ordering::Relaxed)
}

// ---- shell aliases --------------------------------------------------------------

const ALIAS_START: &str = "# >>> vhrn managed aliases >>>";
const ALIAS_END: &str = "# <<< vhrn managed aliases <<<";

/// Render one alias in a shell's syntax. fish's alias takes a space-separated
/// definition and appends $argv itself; bash/zsh use name=value.
fn alias_line(shell: &str, name: &str, target: &str) -> String {
    if shell == "fish" {
        format!("alias {name} '{target}'")
    } else {
        format!("alias {name}='{target}'")
    }
}

/// Build the managed block (markers + one alias per installed harness) for a shell,
/// or "" when nothing is installed so the block is removed entirely.
fn alias_block(hs: &[Harness], shell: &str) -> String {
    if hs.is_empty() {
        return String::new();
    }
    let mut b = String::new();
    b.push_str(ALIAS_START);
    b.push('\n');
    b.push_str("# Regenerated by vhrn install/uninstall; edits here are overwritten.\n");
    for h in hs {
        b.push_str(&alias_line(shell, &h.alias, &format!("vhrn {}", h.name)));
        b.push('\n');
    }
    b.push_str(ALIAS_END);
    b.push('\n');
    b
}

/// Remove an existing managed block (start..end inclusive) plus one trailing blank
/// line, leaving other content untouched. Unchanged if absent.
fn strip_block(content: &str) -> String {
    let lines: Vec<&str> = content.split('\n').collect();
    let mut start = None;
    let mut end = None;
    for (i, l) in lines.iter().enumerate() {
        let t = l.trim();
        if t == ALIAS_START && start.is_none() {
            start = Some(i);
        } else if t == ALIAS_END && start.is_some() && end.is_none() {
            end = Some(i);
        }
    }
    let (Some(s), Some(e)) = (start, end) else {
        return content.to_string();
    };
    let mut rest: Vec<&str> = lines[..s].to_vec();
    let mut tail = &lines[e + 1..];
    if tail.first().is_some_and(|l| l.trim().is_empty()) {
        tail = &tail[1..]; // collapse the blank line the block left behind
    }
    rest.extend_from_slice(tail);
    rest.join("\n")
}

/// Rewrite `path`'s managed block: strip any existing block, then append the fresh
/// one (if non-empty), creating the file/dir when needed. A no-op when nothing
/// changes, and it won't create an empty rc file just to remove an absent block.
fn write_alias_block(path: &Path, block: &str) -> std::io::Result<()> {
    let (orig, existed) = match std::fs::read_to_string(path) {
        Ok(s) => (s, true),
        Err(_) => (String::new(), false),
    };
    let mut content = strip_block(&orig);
    if !block.is_empty() {
        content = content.trim_end_matches('\n').to_string();
        if !content.is_empty() {
            content.push_str("\n\n");
        }
        content.push_str(block);
    }
    if content == orig || (!existed && content.is_empty()) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content)
}

/// The basename of `$SHELL` (bash/zsh/fish), or `None` when unset.
pub(crate) fn current_shell() -> Option<String> {
    let sh = std::env::var("SHELL").ok()?;
    if sh.is_empty() {
        return None;
    }
    Path::new(&sh).file_name().map(|n| n.to_string_lossy().into_owned())
}

/// The rc files to manage: the known-shell rc files that already exist, plus the
/// current shell's (created if missing). `shell` is injected (the caller reads
/// `$SHELL` via `current_shell`).
fn rc_targets(home: &Path, shell: Option<&str>) -> BTreeMap<String, PathBuf> {
    let all = [
        ("bash", home.join(".bashrc")),
        ("zsh", home.join(".zshrc")),
        ("fish", home.join(".config/fish/config.fish")),
    ];
    let mut targets = BTreeMap::new();
    for (name, path) in &all {
        if path.is_file() {
            targets.insert((*name).to_string(), path.clone());
        }
    }
    if let Some(cur) = shell {
        for (name, path) in &all {
            if *name == cur {
                targets.insert(cur.to_string(), path.clone());
            }
        }
    }
    targets
}

/// Regenerate every managed alias block from the installed registry — the single call
/// install and uninstall both make so the aliases always match the installed set.
pub(crate) fn sync_aliases(config_dir: &Path, home: &Path, shell: Option<&str>) -> std::io::Result<()> {
    let hs: Vec<Harness> = read_installed(config_dir)
        .into_iter()
        .filter_map(|ih| lookup_harness(&ih.name))
        .collect();
    for (sh, path) in rc_targets(home, shell) {
        write_alias_block(&path, &alias_block(&hs, &sh))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::temp_dir;

    fn claude() -> Harness {
        Harness { name: "claude".into(), alias: "claude".into(), ..Default::default() }
    }

    #[test]
    fn alias_line_syntax() {
        assert_eq!(alias_line("zsh", "claude", "vhrn claude"), "alias claude='vhrn claude'");
        assert_eq!(alias_line("fish", "claude", "vhrn claude"), "alias claude 'vhrn claude'");
    }

    #[test]
    fn alias_block_markers_and_lines() {
        assert_eq!(alias_block(&[], "zsh"), "", "empty harness set yields no block");
        let b = alias_block(std::slice::from_ref(&claude()), "bash");
        assert!(b.contains(ALIAS_START) && b.contains(ALIAS_END), "block missing markers:\n{b}");
        assert!(b.contains("alias claude='vhrn claude'"), "block missing alias line:\n{b}");
    }

    #[test]
    fn installed_registry_add_update_remove() {
        let dir = temp_dir();
        assert!(read_installed(&dir).is_empty(), "fresh registry should be empty");
        assert!(installed_version(&dir, "claude").is_none());

        add_installed(&dir, "claude", "v0.2.0").unwrap();
        add_installed(&dir, "codex", "latest").unwrap();
        add_installed(&dir, "claude", "v0.3.0").unwrap(); // update in place

        assert_eq!(
            read_installed(&dir),
            vec![
                InstalledHarness { name: "claude".into(), version: "v0.3.0".into() },
                InstalledHarness { name: "codex".into(), version: "latest".into() },
            ]
        );
        assert_eq!(installed_version(&dir, "claude").as_deref(), Some("v0.3.0"));

        remove_installed(&dir, "claude").unwrap();
        assert_eq!(
            read_installed(&dir),
            vec![InstalledHarness { name: "codex".into(), version: "latest".into() }]
        );
    }

    #[test]
    fn read_installed_bare_name_defaults_latest() {
        let dir = temp_dir();
        std::fs::write(installed_registry_path(&dir), "claude\n").unwrap();
        assert_eq!(installed_version(&dir, "claude").as_deref(), Some("latest"));
    }

    #[test]
    fn write_alias_block_round_trip() {
        let path = temp_dir().join(".zshrc");
        std::fs::write(&path, "export FOO=1\n").unwrap();

        let block = alias_block(std::slice::from_ref(&claude()), "zsh");
        write_alias_block(&path, &block).unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.starts_with("export FOO=1\n"), "surrounding content not preserved:\n{after}");
        assert!(after.contains("alias claude='vhrn claude'"));

        // Regenerating with the same block must not duplicate it.
        write_alias_block(&path, &block).unwrap();
        let regen = std::fs::read_to_string(&path).unwrap();
        assert_eq!(regen.matches(ALIAS_START).count(), 1, "block duplicated:\n{regen}");

        // Empty block removes it and restores the original exactly.
        write_alias_block(&path, "").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "export FOO=1\n");
    }

    #[test]
    fn write_alias_block_no_spurious_file() {
        let path = temp_dir().join(".bashrc"); // temp_dir exists; this file does not
        write_alias_block(&path, "").unwrap();
        assert!(!path.exists(), "removing a block from an absent file should not create it");
    }

    #[test]
    fn sync_aliases_manages_existing_and_current_shell() {
        let config_dir = temp_dir();
        let home = temp_dir();
        let bashrc = home.join(".bashrc");
        std::fs::write(&bashrc, "# bash\n").unwrap(); // exists -> managed
        let zshrc = home.join(".zshrc"); // current shell -> created
        let fishrc = home.join(".config/fish/config.fish");

        add_installed(&config_dir, "claude", "latest").unwrap();
        sync_aliases(&config_dir, &home, Some("zsh")).unwrap();

        for p in [&bashrc, &zshrc] {
            let data = std::fs::read_to_string(p).unwrap_or_default();
            assert!(data.contains("alias claude="), "{p:?} should carry the alias");
        }
        assert!(!fishrc.exists(), "fish rc neither existing nor current shell; leave alone");

        // Uninstalling clears the blocks.
        remove_installed(&config_dir, "claude").unwrap();
        sync_aliases(&config_dir, &home, Some("zsh")).unwrap();
        assert!(
            !std::fs::read_to_string(&zshrc).unwrap().contains(ALIAS_START),
            "alias block should be gone after uninstall"
        );
    }
}
