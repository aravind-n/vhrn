//! The run path — container preparation, engine selection, the proxy sidecar, and the
//! small host-side path/exec helpers the run and subcommand handlers share.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Result, bail};
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;
use tracing::warn;

use crate::cli::RunFlags;
use crate::config::Config;
use crate::harness::Harness;
use crate::net::Mode;

/// Reproduce Claude's `projects/<key>` encoding so in-container history unifies with
/// native history: every character outside `[A-Za-z0-9]` becomes `-`
/// (sed 's/[^A-Za-z0-9]/-/g').
fn history_key(project: &str) -> String {
    project
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// The user's home directory from `$HOME`. Errors when
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

/// Whether `name` is an executable on `$PATH`: a file with any execute bit set in some
/// PATH directory.
pub(crate) fn look_path(name: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| {
        std::fs::metadata(dir.join(name))
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
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

/// A running egress-proxy sidecar. The container's firewall pins all egress to it;
/// policy files live host-side and are mounted only into this sidecar.
#[derive(Clone)]
pub(crate) struct Proxy {
    engine: String,
    name: String,
}

impl Proxy {
    fn stop(&self) {
        let _ = Command::new(&self.engine)
            .args(["stop", &self.name])
            .status();
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
                Ok(o) if o.status.success() => {
                    String::from_utf8_lossy(&o.stdout).trim().to_string()
                }
                _ => String::new(),
            };
        }
        // Apple `container inspect` prints JSON; scan it for the first dotted quad.
        match Command::new("container")
            .args(["inspect", &self.name])
            .output()
        {
            Ok(o) if o.status.success() => first_ipv4(&String::from_utf8_lossy(&o.stdout)),
            _ => String::new(),
        }
    }
}

/// Launch the detached proxy sidecar and resolve its IP (engines differ; retry until
/// it has one). `policy_dir` is the host-side net policy dir, mounted into the proxy
/// only — never the container.
pub(crate) fn start_proxy(
    engine: &str,
    image: &str,
    policy_dir: &Path,
    port: &str,
) -> Result<(Proxy, String)> {
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
    let proxy = Proxy {
        engine: engine.to_string(),
        name,
    };

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

/// The first dotted quad on the first line mentioning `ipv4Address` in the engine's
/// inspect output. Apple's inspect JSON escapes the CIDR slash (192.168.64.73\/24),
/// so we match only the quad. No regex crate.
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
        return; // best-effort
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

/// The unprivileged container user's home; all container-side paths hang off it.
const CONTAINER_HOME: &str = "/home/dev";

/// The resolved host-side state for one run: paths, engine/image, and the extra
/// --volume/--env args assembled during preparation.
#[derive(Default)]
pub(crate) struct ContainerConfig {
    pub engine: String,
    pub harness: Harness,
    pub image: String, // resolved container image ref (registry ref, or bare local name)
    pub version: String, // installed image version (a tag, or "local")
    pub project: String, // physical cwd (pwd -P)
    pub key: String,   // history key: [^A-Za-z0-9] -> '-'
    pub cache: String, // ~/.cache/vhrn
    pub state: String, // <cache>/state/<harness> -> the container's persistent config dir
    pub sandbox: String, // <cache>/sandbox -> disposable synced config
    pub config_dir: String, // container config dir, e.g. /home/dev/.claude
    pub host_config: String, // host config dir, e.g. ~/.claude
    pub history: String, // <host_config>/projects/<key>
    pub config: Config, // merged defaults + global + project config
    pub git_mount: Vec<String>,
    pub gh_env: Vec<String>,
    pub term_env: Vec<String>,
}

impl ContainerConfig {
    /// Layer the disposable synced config, the container guide, and the shared history dir
    /// on top of the persistent state mount as nested bind mounts. Each is guarded on
    /// source existence so we never bind a missing path or turn a file mount into a
    /// stray directory.
    fn nested_mounts(&self) -> Vec<String> {
        let mut m = Vec::new();
        for d in &self.harness.sync_dirs {
            let src = Path::new(&self.sandbox).join(d);
            if src.is_dir() {
                m.push("--volume".to_string());
                m.push(format!("{}:{}/{}", src.display(), self.config_dir, d));
            }
        }
        for f in &self.harness.sync_files {
            let src = Path::new(&self.sandbox).join(f);
            if src.is_file() {
                m.push("--volume".to_string());
                m.push(format!("{}:{}/{}", src.display(), self.config_dir, f));
            }
        }
        let guide = Path::new(&self.sandbox).join("CLAUDE.md");
        if guide.is_file() {
            m.push("--volume".to_string());
            m.push(format!("{}:{}/CLAUDE.md", guide.display(), self.config_dir));
        }
        m.push("--volume".to_string());
        m.push(format!(
            "{}:{}/projects/{}",
            self.history, self.config_dir, self.key
        ));
        m
    }
}

/// Perform all host-side preparation: resolve paths and engine, ready the persistent
/// state store, sync the disposable config, and assemble the git/gh/terminal args.
fn prepare_container(h: &Harness) -> Result<ContainerConfig> {
    let home = home_dir()?;
    let project = std::fs::canonicalize(std::env::current_dir()?)?; // pwd -P
    let project_s = project.to_string_lossy().into_owned();
    let engine = detect_engine()?;

    // Config first: a blocked cwd must abort before any host-side work.
    let config_dir_host = crate::shell::vhrn_config_dir(&home);
    let conf = crate::config::load_config(&config_dir_host)?;
    crate::config::check_blocked_dir(
        &project_s,
        &home.to_string_lossy(),
        conf.run.blocked_dirs.as_deref().unwrap_or(&[]),
    )?;

    // Resolve the container image from the installed registry; VHRN_IMAGE overrides it.
    let installed = crate::shell::installed_version(&config_dir_host, &h.name);
    let img_override = std::env::var("VHRN_IMAGE").unwrap_or_default();
    if installed.is_none() && img_override.is_empty() {
        bail!(
            "{} is not installed — run `vhrn install {}`",
            h.name,
            h.name
        );
    }
    let version = installed.unwrap_or_else(|| crate::image::LOCAL_VERSION.to_string());
    let image = if img_override.is_empty() {
        crate::image::harness_image_ref(&crate::image::registry_base(), h, &version)
    } else {
        img_override
    };

    let cache = vhrn_cache(&home);
    let key = history_key(&project_s);
    let host_config = home.join(&h.host_config);
    let sandbox = cache.join("sandbox");
    let history = host_config.join("projects").join(&key);

    // The persistent, container-owned store — login/credentials/onboarding live here.
    let state = crate::persist::prepare_state(&home, &cache, h, &project_s)?;

    std::fs::create_dir_all(&sandbox)?;
    std::fs::create_dir_all(&history)?;

    // Disposable config synced from the host, layered on top of the state mount.
    for d in &h.sync_dirs {
        crate::persist::sync_claude_subdir(&host_config, &sandbox, d);
    }
    for f in &h.sync_files {
        crate::persist::copy_file_into(&host_config, &sandbox, f);
    }

    Ok(ContainerConfig {
        engine,
        harness: h.clone(),
        image,
        version,
        project: project_s,
        key,
        cache: cache.to_string_lossy().into_owned(),
        state: state.to_string_lossy().into_owned(),
        sandbox: sandbox.to_string_lossy().into_owned(),
        config_dir: format!("{CONTAINER_HOME}/{}", h.state_dir),
        host_config: host_config.to_string_lossy().into_owned(),
        history: history.to_string_lossy().into_owned(),
        config: conf,
        git_mount: crate::env::git_config_mount(&home, &cache),
        gh_env: crate::env::gh_token_env(),
        term_env: crate::env::terminal_env(),
    })
}

/// Assemble the full engine run argv (pure; the golden test snapshots it). Point the
/// agent at its config dir, mount the persistent state there, then layer the
/// disposable synced config + history on top as nested mounts.
fn container_run_args(
    cfg: &ContainerConfig,
    f: &RunFlags,
    mode: Mode,
    ip: &str,
    port: &str,
) -> Vec<String> {
    let proxy_url = format!("http://{ip}:{port}");
    let mut args = vec![
        "run".to_string(),
        "-it".into(),
        "--rm".into(),
        "--cap-add".into(),
        "CAP_NET_ADMIN".into(),
        "--env".into(),
        "VHRN_SANDBOX=1".into(),
        "--env".into(),
        format!("VHRN_NET={}", mode.as_str()),
        "--env".into(),
        format!("VHRN_PROXY_IP={ip}"),
        "--env".into(),
        format!("VHRN_PROXY_PORT={port}"),
        "--env".into(),
        format!("HTTP_PROXY={proxy_url}"),
        "--env".into(),
        format!("HTTPS_PROXY={proxy_url}"),
        "--env".into(),
        format!("http_proxy={proxy_url}"),
        "--env".into(),
        format!("https_proxy={proxy_url}"),
        "--volume".into(),
        format!("{p}:{p}", p = cfg.project),
        "--workdir".into(),
        cfg.project.clone(),
    ];
    if !cfg.harness.config_dir_env.is_empty() {
        args.push("--env".into());
        args.push(format!("{}={}", cfg.harness.config_dir_env, cfg.config_dir));
    }
    args.push("--volume".into());
    args.push(format!("{}:{}", cfg.state, cfg.config_dir));
    args.extend(cfg.nested_mounts());
    args.extend(cfg.git_mount.iter().cloned());
    args.extend(cfg.term_env.iter().cloned());
    args.extend(cfg.gh_env.iter().cloned());
    args.push(cfg.image.clone());
    args.push(cfg.harness.command.clone());
    args.extend(f.rest.iter().cloned());
    args
}

/// Stop the sidecar on any normal/error return.
struct ProxyGuard(Proxy);
impl Drop for ProxyGuard {
    fn drop(&mut self) {
        self.0.stop();
    }
}

/// Seed the egress policy, start the proxy sidecar, then run the jailed container with all
/// egress pinned to the proxy. The container run inherits the terminal; its exit status is
/// returned verbatim as the process exit code.
fn start_container(mut cfg: ContainerConfig, f: &RunFlags) -> Result<i32> {
    let port = env_or("VHRN_PROXY_PORT", "8080");
    let cfg_mode = cfg.config.net.mode.clone().unwrap_or_default();
    let mode = crate::net::resolve_mode(&cfg_mode, f.open_net);
    if !f.open_net && !cfg_mode.is_empty() && cfg_mode != mode.as_str() {
        warn!("invalid net mode {cfg_mode:?}; using {}", mode.as_str());
    }

    let config_allow = cfg.config.net.allow.clone().unwrap_or_default();
    let policy_dir =
        crate::net::prepare_policy(Path::new(&cfg.cache), mode, &config_allow, &f.extra_allow)?;

    if let Err(e) = crate::persist::write_container_guide(
        Path::new(&cfg.host_config),
        Path::new(&cfg.sandbox),
        mode == Mode::Open,
    ) {
        warn!("could not write container CLAUDE.md: {e}");
    }

    // Apple container needs its system service up; Docker manages its own daemon.
    if cfg.engine == "container" {
        let _ = Command::new("container").args(["system", "start"]).status();
    }

    // A declared toolchain resolves to a derived, content-addressed image.
    let tools = cfg.config.toolchains.tools.clone().unwrap_or_default();
    if !tools.is_empty() {
        cfg.image = crate::image::ensure_toolchain_image(
            &cfg.engine,
            &cfg.image,
            &cfg.harness.image,
            &tools,
        )?;
    }

    let proxy_image = env_or(
        "VHRN_PROXY_IMAGE",
        &crate::image::proxy_image_ref(
            &crate::image::registry_base(),
            &crate::image::proxy_tag(crate::cli::version(), &cfg.version),
        ),
    );
    let (proxy, ip) = start_proxy(&cfg.engine, &proxy_image, &policy_dir, &port)?;
    let _guard = ProxyGuard(proxy.clone());
    stop_on_signal(proxy);

    // Security banner for --open-net: a direct stderr write, not a tracing event, so
    // no RUST_LOG level can silence the token-exposure caution.
    if mode == Mode::Open {
        eprintln!("vhrn: network guard OFF (open) — all public egress allowed this session.");
        if !cfg.gh_env.is_empty() {
            eprintln!("vhrn: a GitHub token is present in the container with the guard off.");
        }
    }

    let args = container_run_args(&cfg, f, mode, &ip, &port);
    let status = Command::new(&cfg.engine).args(&args).status()?;
    Ok(status.code().unwrap_or(1))
}

/// Run a harness in the container: prepare host-side state, then launch. Returns the agent's
/// exit code (a non-zero agent is not a wrapper error).
pub(crate) fn run_harness(h: &Harness, f: &RunFlags) -> Result<i32> {
    let cfg = prepare_container(h)?;
    start_container(cfg, f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_key_encoding() {
        #[rustfmt::skip]
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
        assert_eq!(
            vhrn_cache_from(home, Some("/x/cache")),
            Path::new("/x/cache/vhrn")
        );
        // Empty or unset falls back to ~/.cache.
        assert_eq!(
            vhrn_cache_from(home, Some("")),
            Path::new("/home/u/.cache/vhrn")
        );
        assert_eq!(
            vhrn_cache_from(home, None),
            Path::new("/home/u/.cache/vhrn")
        );
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

    // A ContainerConfig fixture whose sandbox has skills/ + settings.json + CLAUDE.md, but
    // no commands/agents dirs or statusline.sh.
    fn fixture_with_sandbox() -> (ContainerConfig, std::path::PathBuf) {
        let sandbox = crate::testutil::temp_dir();
        std::fs::create_dir_all(sandbox.join("skills")).unwrap();
        std::fs::write(sandbox.join("settings.json"), "{}").unwrap();
        std::fs::write(sandbox.join("CLAUDE.md"), "guide").unwrap();
        let cfg = ContainerConfig {
            harness: Harness {
                sync_dirs: vec!["skills".into(), "commands".into(), "agents".into()],
                sync_files: vec!["settings.json".into(), "statusline.sh".into()],
                ..Default::default()
            },
            sandbox: sandbox.to_string_lossy().into_owned(),
            config_dir: "/home/dev/.claude".into(),
            history: "/host/history".into(),
            key: "-proj".into(),
            ..Default::default()
        };
        (cfg, sandbox)
    }

    #[test]
    fn nested_mounts_guard_on_existence() {
        let (cfg, sandbox) = fixture_with_sandbox();
        let got = cfg.nested_mounts();
        assert_eq!(
            got.len() % 2,
            0,
            "mount args must pair --volume with a value: {got:?}"
        );
        let joined = got.join(" ");
        for want in [
            format!(
                "{}:/home/dev/.claude/skills",
                sandbox.join("skills").display()
            ),
            format!(
                "{}:/home/dev/.claude/settings.json",
                sandbox.join("settings.json").display()
            ),
            format!(
                "{}:/home/dev/.claude/CLAUDE.md",
                sandbox.join("CLAUDE.md").display()
            ),
            "/host/history:/home/dev/.claude/projects/-proj".to_string(),
        ] {
            assert!(joined.contains(&want), "missing mount {want:?} in {got:?}");
        }
        for absent in ["commands", "agents", "statusline.sh"] {
            assert!(
                !joined.contains(&format!("/home/dev/.claude/{absent}")),
                "mounted absent source {absent:?}: {got:?}"
            );
        }
    }

    #[test]
    fn container_run_args_golden() {
        let sandbox = crate::testutil::temp_dir();
        std::fs::create_dir_all(sandbox.join("skills")).unwrap();
        std::fs::write(sandbox.join("settings.json"), "{}").unwrap();
        std::fs::write(sandbox.join("CLAUDE.md"), "guide").unwrap();

        let cfg = ContainerConfig {
            engine: "container".into(),
            harness: Harness {
                command: "claude".into(),
                config_dir_env: "CLAUDE_CONFIG_DIR".into(),
                sync_dirs: vec!["skills".into(), "commands".into()], // commands absent
                sync_files: vec!["settings.json".into()],
                ..Default::default()
            },
            image: "vhrn-claude:latest".into(),
            project: "/proj".into(),
            key: "-proj".into(),
            state: "/state".into(),
            sandbox: sandbox.to_string_lossy().into_owned(),
            config_dir: "/home/dev/.claude".into(),
            history: "/hist".into(),
            git_mount: vec![
                "--volume".into(),
                "/c/gitconfig:/home/dev/.gitconfig".into(),
            ],
            term_env: vec!["--env".into(), "TERM=xterm-256color".into()],
            gh_env: vec!["--env".into(), "GH_TOKEN=tok".into()],
            ..Default::default()
        };
        let f = RunFlags {
            open_net: false,
            extra_allow: vec![],
            rest: vec!["--model".into(), "opus".into()],
        };

        let args = container_run_args(&cfg, &f, Mode::Enforce, "10.0.0.2", "8080");

        let skills = format!(
            "{}:/home/dev/.claude/skills",
            sandbox.join("skills").display()
        );
        let settings = format!(
            "{}:/home/dev/.claude/settings.json",
            sandbox.join("settings.json").display()
        );
        let guide = format!(
            "{}:/home/dev/.claude/CLAUDE.md",
            sandbox.join("CLAUDE.md").display()
        );
        let expected: Vec<String> = [
            "run",
            "-it",
            "--rm",
            "--cap-add",
            "CAP_NET_ADMIN",
            "--env",
            "VHRN_SANDBOX=1",
            "--env",
            "VHRN_NET=enforce",
            "--env",
            "VHRN_PROXY_IP=10.0.0.2",
            "--env",
            "VHRN_PROXY_PORT=8080",
            "--env",
            "HTTP_PROXY=http://10.0.0.2:8080",
            "--env",
            "HTTPS_PROXY=http://10.0.0.2:8080",
            "--env",
            "http_proxy=http://10.0.0.2:8080",
            "--env",
            "https_proxy=http://10.0.0.2:8080",
            "--volume",
            "/proj:/proj",
            "--workdir",
            "/proj",
            "--env",
            "CLAUDE_CONFIG_DIR=/home/dev/.claude",
            "--volume",
            "/state:/home/dev/.claude",
            "--volume",
            skills.as_str(),
            "--volume",
            settings.as_str(),
            "--volume",
            guide.as_str(),
            "--volume",
            "/hist:/home/dev/.claude/projects/-proj",
            "--volume",
            "/c/gitconfig:/home/dev/.gitconfig",
            "--env",
            "TERM=xterm-256color",
            "--env",
            "GH_TOKEN=tok",
            "vhrn-claude:latest",
            "claude",
            "--model",
            "opus",
        ]
        .iter()
        .map(std::string::ToString::to_string)
        .collect();

        assert_eq!(args, expected);
    }
}
