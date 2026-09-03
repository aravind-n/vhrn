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
use crate::config::{Config, ResourcesConfig};
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

/// The disposable config copy for one harness (`<cache>/sandbox/<harness>`). Per-harness so
/// one harness's `rsync --delete` never runs on a directory another's live container has
/// mounted — the same split `state/<harness>` already has.
fn sandbox_dir(cache: &Path, harness: &str) -> PathBuf {
    cache.join("sandbox").join(harness)
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
/// world-writable policy dir/log and the private state dir and credentials.
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

/// `.agents`, the vendor-neutral config dir. Host-owned, disposable, mounted for every
/// harness — agents resolve it from $HOME, not from their own config dir, so it is a
/// constant here rather than a per-harness spec field. Whether an agent reads it is the
/// agent's business.
const AGENTS_DIR: &str = ".agents";

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
    pub sandbox: String, // <cache>/sandbox/<harness> -> disposable synced config
    pub config_dir: String, // container config dir, e.g. /home/dev/.claude
    pub host_config: String, // host config dir, e.g. ~/.claude
    pub history: String, // <host_config>/projects/<key>; empty unless the harness shares it
    pub sessions: String, // <cache>/state/<harness>-sessions/<key>; empty = not partitioned
    pub config: Config, // merged defaults + global + project config
    pub git_mount: Vec<String>,
    pub gh_env: Vec<String>,
    pub term_env: Vec<String>,
    pub cred_env: Vec<String>, // the harness's credential vars, when the host has them
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
        // A guide written straight into the state dir is already inside the state mount and
        // needs no mount of its own.
        let h = &self.harness;
        if !h.guide.file.is_empty() && !h.guide.in_state {
            let guide = Path::new(&self.sandbox).join(&h.guide.file);
            if guide.is_file() {
                m.push("--volume".to_string());
                m.push(format!(
                    "{}:{}/{}",
                    guide.display(),
                    self.config_dir,
                    h.guide.file
                ));
            }
        }
        // The transcript dir sits under the config dir but belongs to the session store, so
        // the index and the files it names can never land in different partitions.
        if !self.sessions.is_empty() && !h.sessions_dir.is_empty() {
            let src = Path::new(&self.sessions).join(&h.sessions_dir);
            if src.is_dir() {
                m.push("--volume".to_string());
                m.push(format!(
                    "{}:{}/{}",
                    src.display(),
                    self.config_dir,
                    h.sessions_dir
                ));
            }
        }
        if h.share_history {
            m.push("--volume".to_string());
            m.push(format!(
                "{}:{}/projects/{}",
                self.history, self.config_dir, self.key
            ));
        }
        m
    }

    /// The per-project session store: a top-level mount plus the env var pointing the
    /// agent's session index at it. Login and config stay shared in the state mount; only
    /// what a session produces is partitioned.
    fn sessions_mount(&self) -> Vec<String> {
        if self.sessions.is_empty() || self.harness.sessions_env.is_empty() {
            return Vec::new();
        }
        let dst = format!("{CONTAINER_HOME}/{}-sessions", self.harness.state_dir);
        vec![
            "--volume".to_string(),
            format!("{}:{dst}", self.sessions),
            "--env".to_string(),
            format!("{}={dst}", self.harness.sessions_env),
        ]
    }

    /// The system-config layer, bound read-only at `/etc/<harness>`. A directory mount
    /// rather than two file mounts: it sidesteps the rewrite problem a single-file mount
    /// has, and leaves room for the agent's other admin-scope paths. Deliberately not part
    /// of `nested_mounts` — nothing about it layers onto the state dir. Read-only is what
    /// puts vhrn's constraints out of reach of anything running in the container.
    fn system_config_mount(&self) -> Vec<String> {
        let src = Path::new(&self.sandbox).join(crate::persist::SYSTEM_CONFIG_DIR);
        if !self.harness.system_config || !src.is_dir() {
            return Vec::new();
        }
        vec![
            "--volume".to_string(),
            format!("{}:/etc/{}:ro", src.display(), self.harness.name),
        ]
    }

    /// `.agents`, bound at the container home rather than under the config dir.
    /// Deliberately not part of `nested_mounts` — nothing about it is layered on the state
    /// mount. Guarded on the sandbox copy, so no `~/.agents` on the host means no mount.
    fn agents_mount(&self) -> Vec<String> {
        let src = Path::new(&self.sandbox).join(AGENTS_DIR);
        if !src.is_dir() {
            return Vec::new();
        }
        vec![
            "--volume".to_string(),
            format!("{}:{CONTAINER_HOME}/{AGENTS_DIR}", src.display()),
        ]
    }
}

/// Which host directory each disposable sync reads from: the harness config dir for the
/// harness's own subdirs, the home dir for `.agents`. Directories only — synced *files*
/// are always harness config. Pure so the source of each sync is pinned by a test.
fn dir_sync_plan<'a>(
    home: &'a Path,
    host_config: &'a Path,
    h: &'a Harness,
) -> Vec<(&'a Path, &'a str)> {
    let mut plan: Vec<(&Path, &str)> = h
        .sync_dirs
        .iter()
        .map(|d| (host_config, d.as_str()))
        .collect();
    plan.push((home, AGENTS_DIR));
    plan
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
    let conf = crate::config::load_project_config(&config_dir_host, &project_s)?;
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
    let sandbox = sandbox_dir(&cache, &h.name);

    // The persistent, container-owned store — login/credentials/onboarding live here.
    let state = crate::persist::prepare_state(&home, &cache, h)?;
    let sessions = crate::persist::prepare_sessions(&cache, h, &key)?;

    // Only a harness that shares native history has one, so vhrn never creates a projects/
    // layout under a config dir whose agent does not use it.
    let history = if h.share_history {
        let p = host_config.join("projects").join(&key);
        std::fs::create_dir_all(&p)?;
        p.to_string_lossy().into_owned()
    } else {
        String::new()
    };

    std::fs::create_dir_all(&sandbox)?;

    // Disposable config synced from the host, layered on top of the state mount.
    for (parent, name) in dir_sync_plan(&home, &host_config, h) {
        crate::persist::sync_subdir(parent, &sandbox, name);
    }
    for f in &h.sync_files {
        crate::persist::copy_file_into(&host_config, &sandbox, f);
    }

    // Fatal, unlike the guide: this layer carries vhrn's constraints, and a session that
    // silently ran without them would not be the session the user asked for.
    crate::persist::write_system_config(&host_config, &sandbox, h)?;

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
        history,
        sessions: sessions
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
        config: conf,
        git_mount: crate::env::git_config_mount(&home, &cache),
        gh_env: crate::env::gh_token_env(),
        term_env: crate::env::terminal_env(),
        cred_env: crate::env::credential_env(&h.credential_env),
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
    let mut args = vec!["run".to_string(), "-it".into(), "--rm".into()];
    args.extend(resource_args(&cfg.engine, &cfg.config.resources));
    args.extend([
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
    ]);
    if !cfg.harness.config_dir_env.is_empty() {
        args.push("--env".into());
        args.push(format!("{}={}", cfg.harness.config_dir_env, cfg.config_dir));
    }
    args.push("--volume".into());
    args.push(format!("{}:{}", cfg.state, cfg.config_dir));
    args.extend(cfg.nested_mounts());
    args.extend(cfg.sessions_mount());
    args.extend(cfg.agents_mount());
    args.extend(cfg.system_config_mount());
    args.extend(cfg.git_mount.iter().cloned());
    args.extend(cfg.term_env.iter().cloned());
    args.extend(cfg.gh_env.iter().cloned());
    args.extend(cfg.cred_env.iter().cloned());
    args.push(cfg.image.clone());
    args.push(cfg.harness.command.clone());
    args.extend(f.rest.iter().cloned());
    args
}

/// Translate normalized resource config into portable engine flags. Apple container has a
/// small default memory limit, so vhrn raises only that engine's implicit limit; Docker
/// retains its own default unless the user configured a value.
fn resource_args(engine: &str, resources: &ResourcesConfig) -> Vec<String> {
    let memory = match resources.memory.as_deref() {
        Some("engine") => None,
        Some(memory) => Some(memory),
        None if engine == "container" => Some("4g"),
        None => None,
    };

    let mut args = Vec::new();
    if let Some(memory) = memory {
        args.extend(["--memory".to_string(), memory.to_string()]);
    }
    if let Some(cpus) = resources.cpus {
        args.extend(["--cpus".to_string(), cpus.to_string()]);
    }
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

    // The guide lands wherever the harness reads it from: the disposable sandbox, or the
    // state dir for an agent that resolves it under its own config dir.
    let guide_dst = if cfg.harness.guide.in_state {
        &cfg.state
    } else {
        &cfg.sandbox
    };
    if let Err(e) = crate::persist::write_container_guide(
        Path::new(&cfg.host_config),
        Path::new(guide_dst),
        &cfg.harness,
        mode == Mode::Open,
    ) {
        warn!("could not write container {}: {e}", cfg.harness.guide.file);
    }

    // Apple container needs its system service up; Docker manages its own daemon.
    if cfg.engine == "container" {
        let _ = Command::new("container").args(["system", "start"]).status();
    }

    // Declared tools resolve to a derived, content-addressed image built FROM the harness.
    let apt = cfg.config.tools.apt.clone().unwrap_or_default();
    let run = cfg.config.tools.run.clone().unwrap_or_default();
    if !apt.is_empty() || !run.is_empty() {
        cfg.image = crate::image::ensure_tools_image(
            &cfg.engine,
            &cfg.image,
            &cfg.harness.image,
            &apt,
            &run,
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
        // Name the variables, never their values — this goes to a terminal the user may share.
        let creds = crate::env::env_arg_names(&cfg.cred_env);
        if !creds.is_empty() {
            eprintln!(
                "vhrn: agent credentials are present in the container with the guard off ({}).",
                creds.join(", ")
            );
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
    use crate::harness::Guide;

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
    fn sandbox_dir_is_per_harness() {
        let cache = Path::new("/c/vhrn");
        assert_eq!(
            sandbox_dir(cache, "claude"),
            Path::new("/c/vhrn/sandbox/claude")
        );
        assert_ne!(sandbox_dir(cache, "claude"), sandbox_dir(cache, "codex"));
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

    #[test]
    fn resource_args_resolve_memory_and_cpus() {
        let cases = [
            (
                "container",
                ResourcesConfig::default(),
                vec!["--memory", "4g"],
            ),
            ("docker", ResourcesConfig::default(), vec![]),
            ("other", ResourcesConfig::default(), vec![]),
            (
                "container",
                ResourcesConfig {
                    memory: Some("engine".into()),
                    cpus: None,
                },
                vec![],
            ),
            (
                "docker",
                ResourcesConfig {
                    memory: Some("engine".into()),
                    cpus: None,
                },
                vec![],
            ),
            (
                "docker",
                ResourcesConfig {
                    memory: Some("2g".into()),
                    cpus: None,
                },
                vec!["--memory", "2g"],
            ),
            (
                "container",
                ResourcesConfig {
                    memory: Some("2g".into()),
                    cpus: None,
                },
                vec!["--memory", "2g"],
            ),
            (
                "container",
                ResourcesConfig {
                    memory: None,
                    cpus: Some(2),
                },
                vec!["--memory", "4g", "--cpus", "2"],
            ),
            (
                "docker",
                ResourcesConfig {
                    memory: Some("512m".into()),
                    cpus: Some(4),
                },
                vec!["--memory", "512m", "--cpus", "4"],
            ),
        ];

        for (engine, resources, want) in cases {
            let got = resource_args(engine, &resources);
            assert_eq!(got, want, "{engine}: {resources:?}");
            assert!(
                !got.iter().any(|arg| arg == "-m" || arg == "-c"),
                "resource flags must use long forms: {got:?}"
            );
        }
    }

    #[test]
    fn exact_project_configs_select_independent_resources_and_tools() {
        let file: crate::config::ConfigFile = toml::from_str(
            r#"
                [run]
                blocked_dirs = ["/", "/blocked"]
                [tools]
                apt = ["jq"]
                [resources]
                memory = "4g"
                cpus = 2
                [project."/work/a".tools]
                apt = ["ripgrep"]
                [project."/work/a".resources]
                memory = "8g"
                [project."/work/b".tools]
                run = ["install-b"]
                [project."/work/b".resources]
                cpus = 6
            "#,
        )
        .unwrap();
        let a = crate::config::resolve_config(&file, "/work/a").unwrap();
        let b = crate::config::resolve_config(&file, "/work/b").unwrap();
        assert_eq!(a.run.blocked_dirs, b.run.blocked_dirs);
        assert_ne!(
            resource_args("docker", &a.resources),
            resource_args("docker", &b.resources)
        );
        let a_tools = crate::image::NormalizedTools::new(
            &a.tools.apt.unwrap_or_default(),
            &a.tools.run.unwrap_or_default(),
        );
        let b_tools = crate::image::NormalizedTools::new(
            &b.tools.apt.unwrap_or_default(),
            &b.tools.run.unwrap_or_default(),
        );
        assert_ne!(a_tools, b_tools);
    }

    // A ContainerConfig fixture whose sandbox has skills/ + settings.json + CLAUDE.md, but
    // no commands/agents dirs or statusline.sh.
    fn fixture_with_sandbox() -> (ContainerConfig, tempfile::TempDir) {
        let dir = crate::testutil::temp_dir();
        let sandbox = dir.path();
        std::fs::create_dir_all(sandbox.join("skills")).unwrap();
        std::fs::write(sandbox.join("settings.json"), "{}").unwrap();
        std::fs::write(sandbox.join("CLAUDE.md"), "guide").unwrap();
        let cfg = ContainerConfig {
            harness: Harness {
                sync_dirs: vec!["skills".into(), "commands".into(), "agents".into()],
                sync_files: vec!["settings.json".into(), "statusline.sh".into()],
                guide: Guide {
                    file: "CLAUDE.md".into(),
                    ..Default::default()
                },
                share_history: true,
                ..Default::default()
            },
            sandbox: sandbox.to_string_lossy().into_owned(),
            config_dir: "/home/dev/.claude".into(),
            history: "/host/history".into(),
            key: "-proj".into(),
            ..Default::default()
        };
        (cfg, dir)
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
                sandbox.path().join("skills").display()
            ),
            format!(
                "{}:/home/dev/.claude/settings.json",
                sandbox.path().join("settings.json").display()
            ),
            format!(
                "{}:/home/dev/.claude/CLAUDE.md",
                sandbox.path().join("CLAUDE.md").display()
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
    fn agents_mount_is_per_harness_and_top_level() {
        let cache = crate::testutil::temp_dir();
        // An empty config_dir_env stands in for a harness that names no config dir env at
        // all: the `.agents` mount hangs off the container home, so it must not care.
        for (harness, config_dir, config_dir_env) in [
            ("claude", "/home/dev/.claude", "CLAUDE_CONFIG_DIR"),
            ("codex", "/home/dev/.codex", ""),
        ] {
            let sandbox = sandbox_dir(cache.path(), harness);
            std::fs::create_dir_all(sandbox.join(AGENTS_DIR)).unwrap();
            let cfg = ContainerConfig {
                harness: Harness {
                    config_dir_env: config_dir_env.into(),
                    ..Default::default()
                },
                sandbox: sandbox.to_string_lossy().into_owned(),
                config_dir: config_dir.into(),
                ..Default::default()
            };
            assert_eq!(
                cfg.agents_mount(),
                vec![
                    "--volume".to_string(),
                    format!("{}:/home/dev/.agents", sandbox.join(AGENTS_DIR).display()),
                ],
                "{harness}: .agents mount"
            );
        }
    }

    #[test]
    fn dir_sync_plan_sources_agents_from_home() {
        let home = Path::new("/home/u");
        let host_config = Path::new("/home/u/.claude");
        let h = Harness {
            sync_dirs: vec!["skills".into(), "agents".into()],
            ..Default::default()
        };
        // The whole point: `.agents` reads from the home dir while the harness's own
        // subdirs — including its similarly-named `agents` — read from its config dir.
        assert_eq!(
            dir_sync_plan(home, host_config, &h),
            vec![
                (host_config, "skills"),
                (host_config, "agents"),
                (home, ".agents"),
            ]
        );
    }

    #[test]
    fn dir_sync_plan_covers_every_harness() {
        // A harness that syncs no config dirs of its own still gets `.agents`.
        assert_eq!(
            dir_sync_plan(Path::new("/h"), Path::new("/h/.codex"), &Harness::default()),
            vec![(Path::new("/h"), ".agents")]
        );
    }

    #[test]
    fn agents_mount_absent_without_host_dir() {
        let (cfg, _sandbox) = fixture_with_sandbox();
        assert!(
            cfg.agents_mount().is_empty(),
            "mounted .agents with no sandbox copy"
        );
    }

    #[test]
    fn agents_mount_does_not_collide_with_claude_agents_dir() {
        let (cfg, dir) = fixture_with_sandbox();
        let sandbox = dir.path();
        std::fs::create_dir_all(sandbox.join("agents")).unwrap();
        std::fs::create_dir_all(sandbox.join(AGENTS_DIR)).unwrap();

        let mut got = cfg.nested_mounts();
        got.extend(cfg.agents_mount());
        let joined = got.join(" ");

        // claude's ~/.claude/agents and `.agents` differ by a dot on both sides.
        for want in [
            format!(
                "{}:/home/dev/.claude/agents",
                sandbox.join("agents").display()
            ),
            format!("{}:/home/dev/.agents", sandbox.join(AGENTS_DIR).display()),
        ] {
            assert!(joined.contains(&want), "missing mount {want:?} in {got:?}");
        }
        assert!(
            !joined.contains("/home/dev/.claude/.agents"),
            ".agents nested under the config dir: {got:?}"
        );
    }

    // The fully-populated claude config the golden test snapshots: a sandbox with skills/ +
    // .agents/ + settings.json + CLAUDE.md, and commands/ deliberately absent.
    fn golden_fixture() -> (ContainerConfig, tempfile::TempDir) {
        let dir = crate::testutil::temp_dir();
        let sandbox = dir.path();
        std::fs::create_dir_all(sandbox.join("skills")).unwrap();
        std::fs::create_dir_all(sandbox.join(AGENTS_DIR)).unwrap();
        std::fs::write(sandbox.join("settings.json"), "{}").unwrap();
        std::fs::write(sandbox.join("CLAUDE.md"), "guide").unwrap();
        let cfg = ContainerConfig {
            engine: "container".into(),
            harness: Harness {
                command: "claude".into(),
                config_dir_env: "CLAUDE_CONFIG_DIR".into(),
                sync_dirs: vec!["skills".into(), "commands".into()], // commands absent
                sync_files: vec!["settings.json".into()],
                guide: Guide {
                    file: "CLAUDE.md".into(),
                    ..Default::default()
                },
                share_history: true,
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
        (cfg, dir)
    }

    #[test]
    fn history_mount_is_opt_in() {
        let (mut cfg, _dir) = fixture_with_sandbox();
        assert!(
            cfg.nested_mounts()
                .join(" ")
                .contains("/home/dev/.claude/projects/-proj")
        );

        // A harness that does not share native history gets no projects/ mount at all —
        // otherwise vhrn litters a layout the agent never reads.
        cfg.harness.share_history = false;
        assert!(!cfg.nested_mounts().join(" ").contains("projects"));
    }

    #[test]
    fn sessions_partition_per_project() {
        let store = crate::testutil::temp_dir();
        let (mut cfg, _dir) = fixture_with_sandbox();
        std::fs::create_dir_all(store.path().join("sessions")).unwrap();
        cfg.harness.share_history = false;
        cfg.harness.state_dir = ".codex".into();
        cfg.harness.sessions_env = "CODEX_SQLITE_HOME".into();
        cfg.harness.sessions_dir = "sessions".into();
        cfg.config_dir = "/home/dev/.codex".into();
        cfg.sessions = store.path().to_string_lossy().into_owned();

        // The store is a sibling of the config dir, pointed at by the agent's own env var.
        assert_eq!(
            cfg.sessions_mount(),
            vec![
                "--volume".to_string(),
                format!("{}:/home/dev/.codex-sessions", store.path().display()),
                "--env".to_string(),
                "CODEX_SQLITE_HOME=/home/dev/.codex-sessions".to_string(),
            ]
        );
        // ...and the transcripts it indexes are layered back under the config dir from the
        // same host tree, so index and files can never fall into different partitions.
        assert!(cfg.nested_mounts().join(" ").contains(&format!(
            "{}:/home/dev/.codex/sessions",
            store.path().join("sessions").display()
        )));

        // A harness that declares no session env keeps sessions in the shared state store.
        cfg.harness.sessions_env = String::new();
        assert!(cfg.sessions_mount().is_empty());
    }

    #[test]
    fn prepare_sessions_is_keyed_and_opt_in() {
        let cache = crate::testutil::temp_dir();
        let h = Harness {
            name: "codex".into(),
            sessions_env: "CODEX_SQLITE_HOME".into(),
            sessions_dir: "sessions".into(),
            ..Default::default()
        };
        let a = crate::persist::prepare_sessions(cache.path(), &h, "-a")
            .unwrap()
            .unwrap();
        let b = crate::persist::prepare_sessions(cache.path(), &h, "-b")
            .unwrap()
            .unwrap();
        assert_ne!(a, b, "two projects must not share a session store");
        assert!(a.join("sessions").is_dir());
        // A sibling of the shared state dir, never inside it — the shared store holds the
        // login every project uses.
        assert!(!a.starts_with(cache.path().join("state").join("codex")));

        assert!(
            crate::persist::prepare_sessions(cache.path(), &Harness::default(), "-a")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn system_config_mount_is_read_only_and_opt_in() {
        let (mut cfg, dir) = golden_fixture();
        // claude declares no system config, so it emits no /etc mount even if the dir exists.
        std::fs::create_dir_all(dir.path().join(crate::persist::SYSTEM_CONFIG_DIR)).unwrap();
        assert!(cfg.system_config_mount().is_empty());

        cfg.harness.system_config = true;
        cfg.harness.name = "codex".into();
        assert_eq!(
            cfg.system_config_mount(),
            vec![
                "--volume".to_string(),
                format!(
                    "{}:/etc/codex:ro",
                    dir.path().join(crate::persist::SYSTEM_CONFIG_DIR).display()
                ),
            ]
        );
        // It hangs off /etc, so it must never appear among the state-dir layers.
        assert!(!cfg.nested_mounts().join(" ").contains("/etc/codex"));
    }

    // Credential vars ride alongside the other env passthrough, after the gh token and
    // before the image — the agent's own args must still come last and untouched.
    #[test]
    fn credential_env_args_precede_the_image() {
        let (mut cfg, _dir) = golden_fixture();
        cfg.cred_env = vec!["--env".into(), "OPENAI_API_KEY=sk-x".into()];
        let f = RunFlags {
            open_net: false,
            extra_allow: vec![],
            rest: vec!["--model".into(), "gpt-5".into()],
        };

        let args = container_run_args(&cfg, &f, Mode::Enforce, "10.0.0.2", "8080");
        let pos = |needle: &str| args.iter().position(|a| a == needle).expect(needle);
        assert!(pos("GH_TOKEN=tok") < pos("OPENAI_API_KEY=sk-x"));
        assert!(pos("OPENAI_API_KEY=sk-x") < pos("vhrn-claude:latest"));
        assert_eq!(&args[args.len() - 2..], ["--model", "gpt-5"]);
    }

    #[test]
    fn agent_resource_named_args_remain_at_the_tail() {
        let (cfg, _dir) = golden_fixture();
        let f = RunFlags {
            open_net: false,
            extra_allow: vec![],
            rest: vec![
                "--memory".into(),
                "agent-memory".into(),
                "--cpus".into(),
                "7".into(),
            ],
        };

        let args = container_run_args(&cfg, &f, Mode::Enforce, "10.0.0.2", "8080");
        let image = args
            .iter()
            .position(|arg| arg == "vhrn-claude:latest")
            .unwrap();
        assert_eq!(
            &args[image..],
            [
                "vhrn-claude:latest",
                "claude",
                "--memory",
                "agent-memory",
                "--cpus",
                "7"
            ]
        );
    }

    // The codex mount topology end to end, against the real spec. Each piece has its own
    // test above; what this pins is the combination, which is what actually regresses.
    #[test]
    fn container_run_args_codex_golden() {
        let dir = crate::testutil::temp_dir();
        let sandbox = dir.path().join("sandbox");
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(sandbox.join("prompts")).unwrap();
        std::fs::create_dir_all(sandbox.join(AGENTS_DIR)).unwrap();
        std::fs::create_dir_all(sandbox.join(crate::persist::SYSTEM_CONFIG_DIR)).unwrap();
        std::fs::create_dir_all(sessions.join("sessions")).unwrap();
        // The derived guide is written into the state dir, so it is already inside the
        // state mount. A stray sandbox copy must not become a mount of its own.
        std::fs::write(sandbox.join("AGENTS.override.md"), "stray").unwrap();

        let h = crate::harness::lookup_harness("codex").unwrap();
        let cfg = ContainerConfig {
            engine: "container".into(),
            image: "vhrn-codex:latest".into(),
            project: "/proj".into(),
            key: "-proj".into(),
            state: "/state".into(),
            sandbox: sandbox.to_string_lossy().into_owned(),
            sessions: sessions.to_string_lossy().into_owned(),
            config_dir: format!("{CONTAINER_HOME}/{}", h.state_dir),
            harness: h,
            ..Default::default()
        };

        let args = container_run_args(
            &cfg,
            &RunFlags::default(),
            Mode::Enforce,
            "10.0.0.2",
            "8080",
        );
        let joined = args.join(" ");

        assert_eq!(&args[..5], ["run", "-it", "--rm", "--memory", "4g"]);

        for want in [
            "CODEX_HOME=/home/dev/.codex".to_string(),
            "/state:/home/dev/.codex".to_string(),
            format!(
                "{}:/home/dev/.codex/prompts",
                sandbox.join("prompts").display()
            ),
            format!(
                "{}:/home/dev/.codex/sessions",
                sessions.join("sessions").display()
            ),
            format!("{}:/home/dev/.codex-sessions", sessions.display()),
            "CODEX_SQLITE_HOME=/home/dev/.codex-sessions".to_string(),
            format!(
                "{}:/etc/codex:ro",
                sandbox.join(crate::persist::SYSTEM_CONFIG_DIR).display()
            ),
            format!("{}:/home/dev/.agents", sandbox.join(AGENTS_DIR).display()),
        ] {
            assert!(joined.contains(&want), "missing {want:?} in {args:?}");
        }
        for absent in [
            "CLAUDE_CONFIG_DIR",
            "AGENTS.override.md:", // the guide rides inside the state mount
            "/home/dev/.codex/projects", // no native history layout to share
            "/home/dev/.codex/skills", // container state: the agent installs its own
        ] {
            assert!(
                !joined.contains(absent),
                "unexpected {absent:?} in {args:?}"
            );
        }
        assert_eq!(args.last().unwrap(), "codex");
    }

    #[test]
    fn container_run_args_golden() {
        let (cfg, sandbox) = golden_fixture();
        let f = RunFlags {
            open_net: false,
            extra_allow: vec![],
            rest: vec!["--model".into(), "opus".into()],
        };

        let args = container_run_args(&cfg, &f, Mode::Enforce, "10.0.0.2", "8080");

        let skills = format!(
            "{}:/home/dev/.claude/skills",
            sandbox.path().join("skills").display()
        );
        let settings = format!(
            "{}:/home/dev/.claude/settings.json",
            sandbox.path().join("settings.json").display()
        );
        let guide = format!(
            "{}:/home/dev/.claude/CLAUDE.md",
            sandbox.path().join("CLAUDE.md").display()
        );
        let agents = format!(
            "{}:/home/dev/.agents",
            sandbox.path().join(AGENTS_DIR).display()
        );
        let expected: Vec<String> = [
            "run",
            "-it",
            "--rm",
            "--memory",
            "4g",
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
            agents.as_str(),
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
