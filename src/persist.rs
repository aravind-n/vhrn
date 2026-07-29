//! The container-owned state store and the disposable config sync. `state/<harness>/` is
//! the persistent store mounted as the container's config dir; host credentials seed it
//! bootstrap-only (an in-container login is never clobbered). Everything else in the store
//! belongs to the agent — onboarding and per-project trust included — so a decision made
//! in the container is the one that survives. The sandbox sync + container guide are
//! re-derived each run and layered on top as nested mounts.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;
use tracing::warn;

use crate::harness::Harness;
use crate::run::{look_path, set_mode};

/// The persistent, container-owned store for one harness (`<cache>/state/<harness>`),
/// physically separate from the disposable sandbox so no config sync can reach it.
fn host_state_dir(cache: &Path, harness: &str) -> PathBuf {
    cache.join("state").join(harness)
}

/// Ready the persistent store before launch and return its path: ensure the dir and
/// bootstrap credentials from the host once.
pub(crate) fn prepare_state(home: &Path, cache: &Path, h: &Harness) -> Result<PathBuf> {
    let state = host_state_dir(cache, &h.name);
    std::fs::create_dir_all(&state)?;
    set_mode(&state, 0o700)?;
    bootstrap_credentials(home, &state, h);
    Ok(state)
}

/// Copy each host credentials file into the store, but only when the store's copy is
/// absent. Bootstrap-only: once the container has its own (refreshed) credentials they are
/// authoritative and never clobbered, so an in-container login is never overwritten.
fn bootstrap_credentials(home: &Path, state: &Path, h: &Harness) {
    for rel in &h.credentials {
        let dst = state.join(rel);
        if dst.is_file() {
            continue; // container store already populated
        }
        let src = home.join(&h.host_config).join(rel);
        if !src.is_file() {
            continue; // nothing on the host to inherit; the container will prompt to log in
        }
        if let Err(e) = copy_file(&src, &dst) {
            warn!("could not seed {rel}: {e}");
            continue;
        }
        let _ = set_mode(&dst, 0o600); // credentials stay private
    }
}

/// Copy src to dst, following symlinks in src (like cp -L), creating parents.
fn copy_file(src: &Path, dst: &Path) -> std::io::Result<()> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(src, dst)?;
    Ok(())
}

/// Mirror one host subdir into the sandbox, dereferencing symlinks (rsync -aL
/// --delete, cp -RL fallback). `real` is the host parent — the harness config dir for a
/// synced config dir, the home dir for `.agents`. --delete is confined to the
/// subdir, so top-level sandbox files are never pruned.
pub(crate) fn sync_subdir(real: &Path, sandbox: &Path, name: &str) {
    let src = real.join(name);
    let dst = sandbox.join(name);
    if !src.is_dir() {
        // The source is gone, so the mirror goes too — otherwise config the user deleted
        // keeps being mounted every run.
        let _ = std::fs::remove_dir_all(&dst);
        return;
    }
    if look_path("rsync") {
        let ok = Command::new("rsync")
            .args(["-aL", "--delete"])
            .arg(format!("{}/", src.display()))
            .arg(format!("{}/", dst.display()))
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            warn_skipped(name);
        }
        return;
    }
    let _ = std::fs::remove_dir_all(&dst);
    let ok = Command::new("cp")
        .arg("-RL")
        .arg(&src)
        .arg(&dst)
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        warn_skipped(name);
    }
}

/// Copy a single ~/.claude file into the sandbox (cp -L).
pub(crate) fn copy_file_into(real: &Path, sandbox: &Path, name: &str) {
    let src = real.join(name);
    let dst = sandbox.join(name);
    if !src.is_file() {
        let _ = std::fs::remove_file(&dst); // as in sync_subdir: the mirror follows the source
        return;
    }
    if copy_file(&src, &dst).is_err() {
        warn!("could not copy '{name}'");
    }
}

fn warn_skipped(name: &str) {
    warn!("some '{name}' entries were skipped (broken symlink?)");
}

/// Rebuild the sandbox CLAUDE.md fresh each run: the host global CLAUDE.md (if any)
/// followed by a guard-aware section that tracks the net mode, so it never
/// accumulates across runs.
pub(crate) fn write_container_guide(
    real_claude: &Path,
    sandbox: &Path,
    open_net: bool,
) -> std::io::Result<()> {
    let mut b: Vec<u8> = Vec::new();
    if let Ok(data) = std::fs::read(real_claude.join("CLAUDE.md")) {
        b.extend_from_slice(&data);
    }
    b.extend_from_slice(CONTAINER_GUIDE_HEADER.as_bytes());
    b.extend_from_slice(
        if open_net {
            CONTAINER_GUIDE_OPEN
        } else {
            CONTAINER_GUIDE_GUARD
        }
        .as_bytes(),
    );
    std::fs::write(sandbox.join("CLAUDE.md"), b)
}

const CONTAINER_GUIDE_HEADER: &str = r"
# vhrn environment

You are running inside vhrn: a container jailed to this project with a
network egress guard. Adapt as follows:

- **No sudo, no apt at runtime.** The toolchain is baked at build time. You can still
  `uv tool install <pkg>` for Python CLIs (PyPI is allowlisted). Anything else that is
  missing — a language runtime or a system package — must be added by the user to
  `[tools]` in their vhrn config (an `apt` entry or a `run` install command) and
  reinstalled; you cannot install it from inside the container.
";

const CONTAINER_GUIDE_OPEN: &str =
    "- **Network egress is unrestricted this session** (the guard is off via `--open-net`).\n";

const CONTAINER_GUIDE_GUARD: &str = "- **Network egress is allowlisted (default-deny).** A blocked request fails with\n  an error naming the domain. You cannot change the allowlist from inside the\n  container; tell the user the exact host(s) and ask them to run\n  `vhrn net allow <host>` on the host, then retry — no restart is needed.\n";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::temp_dir;

    fn claude() -> Harness {
        Harness {
            name: "claude".into(),
            host_config: ".claude".into(),
            credentials: vec![".credentials.json".into()],
            ..Default::default()
        }
    }

    #[test]
    fn sync_mirrors_a_live_source() {
        let host = temp_dir();
        let sandbox = temp_dir();
        std::fs::create_dir_all(host.path().join("skills")).unwrap();
        std::fs::write(host.path().join("skills").join("SKILL.md"), "real").unwrap();
        std::fs::write(host.path().join("settings.json"), "{}").unwrap();

        sync_subdir(host.path(), sandbox.path(), "skills");
        copy_file_into(host.path(), sandbox.path(), "settings.json");

        assert_eq!(
            std::fs::read_to_string(sandbox.path().join("skills").join("SKILL.md")).unwrap(),
            "real"
        );
        assert_eq!(
            std::fs::read_to_string(sandbox.path().join("settings.json")).unwrap(),
            "{}"
        );
    }

    #[test]
    fn sync_drops_a_copy_whose_source_is_gone() {
        let host = temp_dir(); // nothing on the host behind either copy
        let sandbox = temp_dir();
        std::fs::create_dir_all(sandbox.path().join("skills")).unwrap();
        std::fs::write(sandbox.path().join("skills").join("SKILL.md"), "stale").unwrap();
        std::fs::write(sandbox.path().join("settings.json"), "stale").unwrap();

        sync_subdir(host.path(), sandbox.path(), "skills");
        copy_file_into(host.path(), sandbox.path(), "settings.json");

        // Config the user deleted must not keep being mounted.
        assert!(
            !sandbox.path().join("skills").exists(),
            "stale dir outlived its source"
        );
        assert!(
            !sandbox.path().join("settings.json").exists(),
            "stale file outlived its source"
        );
    }

    #[test]
    fn bootstrap_credentials_is_seed_only() {
        let home = temp_dir();
        let state = temp_dir();
        let h = claude();

        // No host creds: nothing seeded.
        bootstrap_credentials(home.path(), state.path(), &h);
        assert!(
            !state.path().join(".credentials.json").is_file(),
            "seeded creds without a host source"
        );

        // Host login present + empty store: inherited.
        std::fs::create_dir_all(home.path().join(".claude")).unwrap();
        std::fs::write(
            home.path().join(".claude").join(".credentials.json"),
            "HOST",
        )
        .unwrap();
        bootstrap_credentials(home.path(), state.path(), &h);
        assert_eq!(
            std::fs::read_to_string(state.path().join(".credentials.json")).unwrap(),
            "HOST"
        );

        // Container has since logged in: the host seed must not clobber it.
        std::fs::write(state.path().join(".credentials.json"), "existing").unwrap();
        bootstrap_credentials(home.path(), state.path(), &h);
        assert_eq!(
            std::fs::read_to_string(state.path().join(".credentials.json")).unwrap(),
            "existing"
        );
    }

    #[test]
    fn prepare_state_leaves_the_config_json_alone() {
        let home = temp_dir();
        let cache = temp_dir();
        let h = claude();

        // A container-owned file that has the project explicitly *untrusted*.
        let state = cache.path().join("state").join("claude");
        std::fs::create_dir_all(&state).unwrap();
        let path = state.join(".claude.json");
        let before = r#"{"projects":{"/proj":{"hasTrustDialogAccepted":false}}}"#;
        std::fs::write(&path, before).unwrap();

        prepare_state(home.path(), cache.path(), &h).unwrap();

        // The agent owns this file: an untrust made in the container must survive.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }
}
