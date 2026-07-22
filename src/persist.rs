//! The box-owned state store and the disposable config sync. `state/<harness>/` is
//! the persistent store mounted as the box's config dir; host credentials seed it
//! bootstrap-only (an in-box login is never clobbered), and `.claude.json` is merged
//! in place to complete onboarding + trust this project without touching
//! `oauthAccount`/other projects. The sandbox sync + box guide are re-derived each
//! run and layered on top as nested mounts. Ports state.go + sandbox.go + guide.go.

use crate::harness::Harness;
use crate::run::{look_path, set_mode};
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The persistent, box-owned store for one harness (`<cache>/state/<harness>`),
/// physically separate from the disposable sandbox so no config sync can reach it.
fn host_state_dir(cache: &Path, harness: &str) -> PathBuf {
    cache.join("state").join(harness)
}

/// Ready the persistent store before launch and return its path: ensure the dir,
/// bootstrap credentials from the host once, and seed onboarding + this project's
/// trust into the config JSON.
pub(crate) fn prepare_state(home: &Path, cache: &Path, h: &Harness, project: &str) -> Result<PathBuf> {
    let state = host_state_dir(cache, &h.name);
    std::fs::create_dir_all(&state)?;
    set_mode(&state, 0o700)?;
    bootstrap_credentials(home, &state, h);
    if h.seed_trust
        && !h.config_json.is_empty()
        && let Err(e) = seed_claude_config_json(&state.join(&h.config_json), project)
    {
        eprintln!("vhrn: warning: could not seed {}: {e}", h.config_json);
    }
    Ok(state)
}

/// Copy each host credentials file into the store, but only when the store's copy is
/// absent. Bootstrap-only: once the box has its own (refreshed) credentials they are
/// authoritative and never clobbered, so an in-box login is never overwritten.
fn bootstrap_credentials(home: &Path, state: &Path, h: &Harness) {
    for rel in &h.credentials {
        let dst = state.join(rel);
        if dst.is_file() {
            continue; // box store already populated
        }
        let src = home.join(&h.host_config).join(rel);
        if !src.is_file() {
            continue; // nothing on the host to inherit; the box will prompt to log in
        }
        if let Err(e) = copy_file(&src, &dst) {
            eprintln!("vhrn: warning: could not seed {rel}: {e}");
            continue;
        }
        let _ = set_mode(&dst, 0o600); // credentials stay private
    }
}

/// Ensure the box-owned config JSON has onboarding completed and this project
/// pre-trusted, without disturbing anything the box wrote (login/oauthAccount, other
/// projects). Numbers are preserved exactly (arbitrary_precision), and an unparseable
/// box-owned file is left untouched rather than clobbered.
fn seed_claude_config_json(path: &Path, project: &str) -> Result<()> {
    use serde_json::{Map, Value};

    let mut m: Map<String, Value> = match std::fs::read(path) {
        Ok(data) if !data.is_empty() => match serde_json::from_slice::<Value>(&data) {
            Ok(Value::Object(map)) => map,
            _ => return Ok(()), // unparseable / not an object: leave the box's file untouched
        },
        _ => Map::new(), // absent or empty: fresh
    };

    m.entry("hasCompletedOnboarding").or_insert(Value::Bool(true));

    let projects = m.entry("projects").or_insert_with(|| Value::Object(Map::new()));
    if !projects.is_object() {
        *projects = Value::Object(Map::new());
    }
    let projects = projects.as_object_mut().unwrap();

    let proj = projects.entry(project).or_insert_with(|| Value::Object(Map::new()));
    if !proj.is_object() {
        *proj = Value::Object(Map::new());
    }
    let proj = proj.as_object_mut().unwrap();
    proj.insert("hasTrustDialogAccepted".to_string(), Value::Bool(true));
    proj.insert("hasCompletedProjectOnboarding".to_string(), Value::Bool(true));

    let mut out = serde_json::to_string_pretty(&Value::Object(m))?;
    out.push('\n');
    std::fs::write(path, out)?;
    set_mode(path, 0o600)?;
    Ok(())
}

/// Copy src to dst, following symlinks in src (like cp -L), creating parents.
fn copy_file(src: &Path, dst: &Path) -> std::io::Result<()> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(src, dst)?;
    Ok(())
}

/// Mirror one ~/.claude subdir into the sandbox, dereferencing symlinks (rsync -aL
/// --delete, cp -RL fallback). --delete is confined to the subdir, so top-level
/// sandbox files are never pruned.
pub(crate) fn sync_claude_subdir(real: &Path, sandbox: &Path, name: &str) {
    let src = real.join(name);
    if !src.is_dir() {
        return;
    }
    let dst = sandbox.join(name);
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
    if !src.is_file() {
        return;
    }
    if copy_file(&src, &sandbox.join(name)).is_err() {
        eprintln!("vhrn: warning: could not copy '{name}'");
    }
}

fn warn_skipped(name: &str) {
    eprintln!("vhrn: warning: some '{name}' entries were skipped (broken symlink?)");
}

/// Rebuild the sandbox CLAUDE.md fresh each run: the host global CLAUDE.md (if any)
/// followed by a guard-aware section that tracks the net mode, so it never
/// accumulates across runs.
pub(crate) fn write_box_guide(real_claude: &Path, sandbox: &Path, open_net: bool) -> std::io::Result<()> {
    let mut b: Vec<u8> = Vec::new();
    if let Ok(data) = std::fs::read(real_claude.join("CLAUDE.md")) {
        b.extend_from_slice(&data);
    }
    b.extend_from_slice(BOX_GUIDE_HEADER.as_bytes());
    b.extend_from_slice(if open_net { BOX_GUIDE_OPEN } else { BOX_GUIDE_GUARD }.as_bytes());
    std::fs::write(sandbox.join("CLAUDE.md"), b)
}

const BOX_GUIDE_HEADER: &str = r#"
# vhrn environment

You are running inside vhrn: a container jailed to this project with a
network egress guard. Adapt as follows:

- **No sudo, no apt.** Install tools in user space: `mise use -g <tool>` for
  runtimes (node, go, python, ...), `uv tool install <pkg>` for Python CLIs, and
  `npm i -g <pkg>` after `mise use -g node` for npm CLIs.
"#;

const BOX_GUIDE_OPEN: &str =
    "- **Network egress is unrestricted this session** (the guard is off via `--open-net`).\n";

const BOX_GUIDE_GUARD: &str = "- **Network egress is allowlisted (default-deny).** A blocked request fails with\n  an error naming the domain. You cannot change the allowlist from inside the\n  box; tell the user the exact host(s) and ask them to run\n  `vhrn net allow <host>` on the host, then retry — no restart is needed.\n";

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
    fn bootstrap_credentials_is_seed_only() {
        let home = temp_dir();
        let state = temp_dir();
        let h = claude();

        // No host creds: nothing seeded.
        bootstrap_credentials(&home, &state, &h);
        assert!(!state.join(".credentials.json").is_file(), "seeded creds without a host source");

        // Host login present + empty store: inherited.
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::write(home.join(".claude").join(".credentials.json"), "HOST").unwrap();
        bootstrap_credentials(&home, &state, &h);
        assert_eq!(std::fs::read_to_string(state.join(".credentials.json")).unwrap(), "HOST");

        // Box has since logged in: the host seed must not clobber it.
        std::fs::write(state.join(".credentials.json"), "BOX").unwrap();
        bootstrap_credentials(&home, &state, &h);
        assert_eq!(std::fs::read_to_string(state.join(".credentials.json")).unwrap(), "BOX");
    }

    #[test]
    fn seed_claude_config_json_preserves_login() {
        let path = temp_dir().join(".claude.json");
        std::fs::write(
            &path,
            r#"{"hasCompletedOnboarding":false,"oauthAccount":{"emailAddress":"a@b.c"},"numberOfStartups":1784592922215,"projects":{"/other":{"hasTrustDialogAccepted":true}}}"#,
        )
        .unwrap();

        seed_claude_config_json(&path, "/proj").unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        // Big integers survive without float mangling.
        assert!(raw.contains("1784592922215"), "large number not preserved:\n{raw}");

        let m: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(m.get("oauthAccount").is_some(), "oauthAccount (login) dropped");
        assert_eq!(m["hasCompletedOnboarding"], false, "existing onboarding overwritten");
        assert!(m["projects"].get("/other").is_some(), "existing project trust dropped");
        assert_eq!(m["projects"]["/proj"]["hasTrustDialogAccepted"], true);
        assert_eq!(m["projects"]["/proj"]["hasCompletedProjectOnboarding"], true);
    }

    #[test]
    fn seed_claude_config_json_fresh() {
        let path = temp_dir().join(".claude.json");
        seed_claude_config_json(&path, "/proj").unwrap();
        let m: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(m["hasCompletedOnboarding"], true);
        assert_eq!(m["projects"]["/proj"]["hasTrustDialogAccepted"], true);
    }
}
