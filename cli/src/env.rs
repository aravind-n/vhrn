//! Terminal, GitHub-token, and gitconfig env for the container run. Terminal identity
//! crosses verbatim (Claude branches rendering on it, so it is never forced); the gh
//! token is env-injected, never file-mounted; ~/.gitconfig is a disposable copy
//! bind-mounted in.

use std::path::Path;
use std::process::Command;

use tracing::warn;

use crate::run::look_path;

/// Build the terminal `--env` args: TERM falls back to xterm-256color; the rest cross
/// only when set (an empty value counts as unset). Split
/// from the env read so it is testable without touching process env.
fn terminal_env_from(
    term: Option<&str>,
    colorterm: Option<&str>,
    term_program: Option<&str>,
    term_program_version: Option<&str>,
) -> Vec<String> {
    let term = term.filter(|s| !s.is_empty()).unwrap_or("xterm-256color");
    let mut env = vec!["--env".to_string(), format!("TERM={term}")];
    for (k, v) in [
        ("COLORTERM", colorterm),
        ("TERM_PROGRAM", term_program),
        ("TERM_PROGRAM_VERSION", term_program_version),
    ] {
        if let Some(val) = v.filter(|s| !s.is_empty()) {
            env.push("--env".to_string());
            env.push(format!("{k}={val}"));
        }
    }
    env
}

/// Forward the terminal identity verbatim — Claude branches per-terminal rendering on
/// these, so they are never forced or invented.
pub(crate) fn terminal_env() -> Vec<String> {
    let get = |k: &str| std::env::var(k).ok();
    terminal_env_from(
        get("TERM").as_deref(),
        get("COLORTERM").as_deref(),
        get("TERM_PROGRAM").as_deref(),
        get("TERM_PROGRAM_VERSION").as_deref(),
    )
}

/// Resolve a GitHub token — explicit env wins, else `gh auth token` (the only route
/// that works with macOS Keychain storage) — and pass it as `GH_TOKEN`. Empty when the
/// host has no gh login.
pub(crate) fn gh_token_env() -> Vec<String> {
    let mut tok = std::env::var("GH_TOKEN").unwrap_or_default();
    if tok.is_empty() {
        tok = std::env::var("GITHUB_TOKEN").unwrap_or_default();
    }
    if tok.is_empty()
        && look_path("gh")
        && let Ok(out) = Command::new("gh").args(["auth", "token"]).output()
        && out.status.success()
    {
        tok = String::from_utf8_lossy(&out.stdout).trim().to_string();
    }
    if tok.is_empty() {
        return Vec::new();
    }
    vec!["--env".to_string(), format!("GH_TOKEN={tok}")]
}

/// Forward the harness's credential variables, each only when the host actually has it
/// set. Mirrors `gh_token_env`: an unset (or empty) variable produces no `--env` arg at
/// all, so nothing arbitrary from the host environment crosses.
pub(crate) fn credential_env(names: &[String]) -> Vec<String> {
    credential_env_from(names, |k| std::env::var(k).ok())
}

/// Pure resolver behind `credential_env`, so the selection is testable without process env.
fn credential_env_from(names: &[String], get: impl Fn(&str) -> Option<String>) -> Vec<String> {
    let mut env = Vec::new();
    for name in names {
        if let Some(v) = get(name).filter(|v| !v.is_empty()) {
            env.push("--env".to_string());
            env.push(format!("{name}={v}"));
        }
    }
    env
}

/// The variable names in an assembled `--env` list. For user-facing messages that must
/// name what crossed without ever printing a value — the `--env` literals carry no `=`
/// and drop out.
pub(crate) fn env_arg_names(args: &[String]) -> Vec<String> {
    args.iter()
        .filter_map(|a| a.split_once('='))
        .map(|(k, _)| k.to_string())
        .collect()
}

/// Copy the host ~/.gitconfig into the cache (dereferencing symlinks) and mount it at
/// /home/dev/.gitconfig so in-container commits use the user's identity. A disposable copy,
/// re-synced each run. Empty when absent.
pub(crate) fn git_config_mount(home: &Path, cache: &Path) -> Vec<String> {
    let src = home.join(".gitconfig");
    let dst = cache.join("gitconfig");
    if !src.is_file() {
        let _ = std::fs::remove_file(&dst);
        return Vec::new();
    }
    if std::fs::create_dir_all(cache)
        .and_then(|()| std::fs::copy(&src, &dst).map(|_| ()))
        .is_err()
    {
        warn!("could not copy .gitconfig");
        return Vec::new();
    }
    vec![
        "--volume".to_string(),
        format!("{}:/home/dev/.gitconfig", dst.display()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_env_defaults_to_xterm() {
        assert_eq!(
            terminal_env_from(None, None, None, None),
            ["--env", "TERM=xterm-256color"]
        );
        // Empty strings are treated as unset.
        assert_eq!(
            terminal_env_from(Some(""), Some(""), Some(""), Some("")),
            ["--env", "TERM=xterm-256color"]
        );
    }

    #[test]
    fn terminal_env_forwards_set_vars() {
        let got = terminal_env_from(
            Some("screen-256color"),
            Some("truecolor"),
            Some("Apple_Terminal"),
            None,
        );
        let want = [
            "--env",
            "TERM=screen-256color",
            "--env",
            "COLORTERM=truecolor",
            "--env",
            "TERM_PROGRAM=Apple_Terminal",
        ];
        assert_eq!(got, want);
    }

    #[test]
    fn credential_env_forwards_only_what_is_set() {
        let names: Vec<String> = ["CODEX_API_KEY", "CODEX_ACCESS_TOKEN", "OPENAI_API_KEY"]
            .iter()
            .map(std::string::ToString::to_string)
            .collect();

        // Nothing set: no --env args at all, not empty assignments.
        assert!(credential_env_from(&names, |_| None).is_empty());
        // An empty value counts as unset, as it does for the terminal and gh vars.
        assert!(credential_env_from(&names, |_| Some(String::new())).is_empty());

        // Only the set ones cross, in spec order.
        let got = credential_env_from(&names, |k| match k {
            "OPENAI_API_KEY" => Some("sk-live".into()),
            "CODEX_API_KEY" => Some("ck".into()),
            _ => None,
        });
        assert_eq!(
            got,
            [
                "--env",
                "CODEX_API_KEY=ck",
                "--env",
                "OPENAI_API_KEY=sk-live"
            ]
        );
    }

    #[test]
    fn env_arg_names_never_expose_values() {
        let args = credential_env_from(&["OPENAI_API_KEY".to_string()], |_| {
            Some("sk-secret".into())
        });
        let names = env_arg_names(&args);
        assert_eq!(names, ["OPENAI_API_KEY"]);
        assert!(
            !names.join(" ").contains("sk-secret"),
            "a credential value reached a user-facing name list"
        );
        assert!(env_arg_names(&[]).is_empty());
    }

    #[test]
    fn git_config_mount_present_and_absent() {
        let home = crate::testutil::temp_dir();
        let cache = crate::testutil::temp_dir();

        // Absent: no mount, and any stale copy is removed.
        let dst = cache.path().join("gitconfig");
        std::fs::write(&dst, "old").unwrap();
        assert!(git_config_mount(home.path(), cache.path()).is_empty());
        assert!(!dst.exists(), "stale gitconfig copy should be removed");

        // Present: copied and mounted.
        std::fs::write(home.path().join(".gitconfig"), "[user]\n\tname = X\n").unwrap();
        let m = git_config_mount(home.path(), cache.path());
        assert_eq!(
            m,
            vec![
                "--volume".to_string(),
                format!("{}:/home/dev/.gitconfig", dst.display())
            ]
        );
        assert_eq!(
            std::fs::read_to_string(&dst).unwrap(),
            "[user]\n\tname = X\n"
        );
    }
}
