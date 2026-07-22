//! The run path — box preparation, engine selection, the proxy sidecar, and the
//! small host-side path/exec helpers the run and subcommand handlers share. Ports
//! history_key + these helpers now; the engine and box launch arrive in a later phase.

use anyhow::{Result, bail};
use std::path::{Path, PathBuf};

/// Reproduce Claude's `projects/<key>` encoding so in-box history unifies with
/// native history: every character outside `[A-Za-z0-9]` becomes `-`
/// (sed 's/[^A-Za-z0-9]/-/g').
fn history_key(project: &str) -> String {
    project
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// The user's home directory from `$HOME` (Go's os.UserHomeDir on unix). Errors when
/// unset rather than guessing.
pub(crate) fn home_dir() -> Result<PathBuf> {
    match std::env::var_os("HOME") {
        Some(h) if !h.is_empty() => Ok(PathBuf::from(h)),
        _ => bail!("could not determine home directory ($HOME is unset)"),
    }
}

/// The XDG cache root for vhrn (`${XDG_CACHE_HOME:-~/.cache}/vhrn`). Split from the
/// env read so the resolution is unit-testable without touching process env.
fn vhrn_cache_from(home: &Path, xdg_cache: Option<&str>) -> PathBuf {
    let base = match xdg_cache {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => home.join(".cache"),
    };
    base.join("vhrn")
}

/// The XDG cache root for vhrn, reading `XDG_CACHE_HOME` at the edge.
pub(crate) fn vhrn_cache(home: &Path) -> PathBuf {
    vhrn_cache_from(home, std::env::var("XDG_CACHE_HOME").ok().as_deref())
}

/// Whether `name` is an executable on `$PATH` (approximates exec.LookPath): a file
/// with any execute bit set in some PATH directory.
pub(crate) fn look_path(name: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| {
        std::fs::metadata(dir.join(name))
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_key_encoding() {
        let cases = [
            ("/Users/aravind/projects/vhrn", "-Users-aravind-projects-vhrn"),
            ("/a/b_c.d", "-a-b-c-d"),
            ("/x/y-z", "-x-y-z"),
        ];
        for (input, want) in cases {
            assert_eq!(history_key(input), want, "history_key({input:?})");
        }
    }

    #[test]
    fn vhrn_cache_resolution() {
        let home = Path::new("/home/u");
        assert_eq!(vhrn_cache_from(home, Some("/x/cache")), Path::new("/x/cache/vhrn"));
        // Empty or unset falls back to ~/.cache, like Go's getenv == "".
        assert_eq!(vhrn_cache_from(home, Some("")), Path::new("/home/u/.cache/vhrn"));
        assert_eq!(vhrn_cache_from(home, None), Path::new("/home/u/.cache/vhrn"));
    }
}
