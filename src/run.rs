//! The run path — box preparation, engine selection, the proxy sidecar, and the
//! small host-side path/exec helpers the run and subcommand handlers share. Ports
//! history_key + these helpers now; the engine and box launch arrive in a later phase.

use anyhow::{Result, bail};
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

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

/// Set a path's unix permission bits (safe — the crate forbids unsafe). Used for the
/// world-writable policy dir/log and the private creds/.claude.json.
pub(crate) fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

/// The container engine to use: an explicit `VHRN_ENGINE` (then `ENGINE`) wins, else
/// auto-detect `container` first, then `docker` — matching the Makefile so build and
/// run agree. Split from the env read so it is testable without touching env.
fn detect_engine_from(vhrn_engine: Option<&str>, engine: Option<&str>) -> Result<String> {
    let explicit = vhrn_engine
        .filter(|s| !s.is_empty())
        .or_else(|| engine.filter(|s| !s.is_empty()));
    let chosen = match explicit {
        Some(e) => e.to_string(),
        None if look_path("container") => "container".to_string(),
        None if look_path("docker") => "docker".to_string(),
        None => bail!("no container engine found; install Apple container or Docker"),
    };
    if !look_path(&chosen) {
        bail!("engine {chosen:?} not found");
    }
    Ok(chosen)
}

/// The container engine, reading `VHRN_ENGINE`/`ENGINE` at the edge.
pub(crate) fn detect_engine() -> Result<String> {
    detect_engine_from(
        std::env::var("VHRN_ENGINE").ok().as_deref(),
        std::env::var("ENGINE").ok().as_deref(),
    )
}

/// The value of env var `key`, or `def` when unset or empty.
pub(crate) fn env_or(key: &str, def: &str) -> String {
    match std::env::var(key) {
        Ok(v) if !v.is_empty() => v,
        _ => def.to_string(),
    }
}

/// A running egress-proxy sidecar. The box's in-container firewall pins all egress to
/// it; policy files live host-side and are mounted only into this sidecar.
#[derive(Clone)]
pub(crate) struct Proxy {
    engine: String,
    name: String,
}

impl Proxy {
    fn stop(&self) {
        let _ = Command::new(&self.engine).args(["stop", &self.name]).status();
    }

    fn inspect_ip(&self) -> String {
        if self.engine == "docker" {
            let out = Command::new("docker")
                .args([
                    "inspect",
                    "-f",
                    "{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}",
                    &self.name,
                ])
                .output();
            return match out {
                Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
                _ => String::new(),
            };
        }
        // Apple `container inspect` prints JSON; scan it for the first dotted quad.
        match Command::new("container").args(["inspect", &self.name]).output() {
            Ok(o) if o.status.success() => first_ipv4(&String::from_utf8_lossy(&o.stdout)),
            _ => String::new(),
        }
    }
}

/// Launch the detached proxy sidecar and resolve its IP (engines differ; retry until
/// it has one). `policy_dir` is the host-side net policy dir, mounted into the proxy
/// only — never the box.
pub(crate) fn start_proxy(engine: &str, image: &str, policy_dir: &Path, port: &str) -> Result<(Proxy, String)> {
    let name = format!("vhrn-proxy-{}", std::process::id());
    let status = Command::new(engine)
        .args(["run", "-d", "--rm", "--name", &name])
        .arg("--volume")
        .arg(format!("{}:/etc/vhrn", policy_dir.display()))
        .args([
            "--env",
            "VHRN_ALLOWLIST=/etc/vhrn/allowlist",
            "--env",
            "VHRN_MODE_FILE=/etc/vhrn/mode",
            "--env",
            "VHRN_DENY_LOG=/etc/vhrn/denied.log",
        ])
        .arg("--env")
        .arg(format!("VHRN_PROXY_LISTEN=:{port}"))
        .arg(image)
        .stdout(Stdio::null()) // discard the container id; keep our stdout clean
        .stderr(Stdio::inherit())
        .status();
    if !matches!(status, Ok(s) if s.success()) {
        bail!("proxy failed to start (is the {image:?} image built?)");
    }
    let proxy = Proxy { engine: engine.to_string(), name };

    let mut ip = String::new();
    for _ in 0..30 {
        ip = proxy.inspect_ip();
        if !ip.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    if ip.is_empty() {
        proxy.stop();
        bail!("proxy failed to start (is the {image:?} image built?)");
    }
    Ok((proxy, ip))
}

/// The first dotted quad on the first line mentioning `ipv4Address`, matching Go's
/// `grep -m1 ipv4Address | grep -oE <quad>`. Apple's inspect JSON escapes the CIDR
/// slash (192.168.64.73\/24), so we match only the quad. No regex crate.
fn first_ipv4(inspect_output: &str) -> String {
    for line in inspect_output.split('\n') {
        if line.contains("ipv4Address") {
            return find_dotted_quad(line).unwrap_or_default();
        }
    }
    String::new()
}

/// Find the leftmost `([0-9]{1,3}\.){3}[0-9]{1,3}` in `s`.
fn find_dotted_quad(s: &str) -> Option<String> {
    let b = s.as_bytes();
    (0..b.len()).find_map(|start| match_quad(b, start).map(|end| s[start..end].to_string()))
}

// Match ([0-9]{1,3}\.){3}[0-9]{1,3} at `start`; return the end index on success.
fn match_quad(b: &[u8], start: usize) -> Option<usize> {
    let mut i = start;
    for group in 0..4 {
        let digits_start = i;
        while i < b.len() && b[i].is_ascii_digit() && i - digits_start < 3 {
            i += 1;
        }
        if i == digits_start {
            return None; // needs at least one digit
        }
        if group < 3 {
            if i < b.len() && b[i] == b'.' {
                i += 1;
            } else {
                return None; // groups 0..2 must be followed by a dot
            }
        }
    }
    Some(i)
}

/// Keep the sidecar from leaking if vhrn is signaled. SIGTERM tears down the sidecar
/// and exits; SIGINT is left to the interactive child (the agent) — the parent stays
/// alive to wait and clean up on exit.
pub(crate) fn stop_on_signal(proxy: Proxy) {
    let Ok(mut signals) = Signals::new([SIGINT, SIGTERM]) else {
        return; // best-effort, like Go
    };
    std::thread::spawn(move || {
        for sig in signals.forever() {
            if sig == SIGTERM {
                proxy.stop();
                std::process::exit(1);
            }
            // SIGINT: do nothing; the engine's -it forwards it to the agent.
        }
    });
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

    #[test]
    fn detect_engine_explicit_override() {
        // `ls` stands in for a real engine binary so the test is deterministic.
        assert_eq!(detect_engine_from(Some("ls"), None).unwrap(), "ls");
    }

    #[test]
    fn detect_engine_explicit_missing() {
        assert!(detect_engine_from(Some("vhrn-no-such-engine-xyz"), None).is_err());
    }

    #[test]
    fn first_ipv4_apple_and_none() {
        // Apple container inspect escapes the CIDR slash; only the dotted quad matters.
        let apple = r#"{
  "networks": [
    { "ipv4Address": "192.168.64.73\/24", "gateway": "192.168.64.1" }
  ]
}"#;
        assert_eq!(first_ipv4(apple), "192.168.64.73");
        assert_eq!(first_ipv4("no address here\nsecond line"), "");
    }
}
