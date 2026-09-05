//! The run path — container preparation, engine selection, the proxy sidecar, and the
//! small host-side path/exec helpers the run and subcommand handlers share.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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

/// The XDG state root for vhrn. Relative XDG values are deliberately ignored: state
/// must never accidentally become relative to a project being jailed.
#[allow(dead_code)] // Consumed by the scoped egress policy store.
pub(crate) fn vhrn_state_from(home: &Path, xdg_state: Option<&str>) -> PathBuf {
    let base = match xdg_state {
        Some(value) if !value.is_empty() && Path::new(value).is_absolute() => PathBuf::from(value),
        _ => home.join(".local/state"),
    };
    base.join("vhrn")
}

/// The XDG state root for vhrn, reading `XDG_STATE_HOME` at the edge.
#[allow(dead_code)]
pub(crate) fn vhrn_state(home: &Path) -> PathBuf {
    vhrn_state_from(home, std::env::var("XDG_STATE_HOME").ok().as_deref())
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
/// policy/log files and the private state dir and credentials.
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

type CleanupAction = Arc<dyn Fn() + Send + Sync>;

#[derive(Clone)]
struct ProxyCleanup {
    action: CleanupAction,
    done: Arc<AtomicBool>,
}

impl ProxyCleanup {
    fn new(action: CleanupAction) -> Self {
        Self {
            action,
            done: Arc::new(AtomicBool::new(false)),
        }
    }

    fn run(&self) {
        if !self.done.swap(true, Ordering::AcqRel) {
            (self.action)();
        }
    }
}

/// Owns the proxy lifetime. Every clone of its cleanup action shares one stop.
struct ProxyGuard {
    cleanup: ProxyCleanup,
    proxy: Proxy,
}

impl Drop for ProxyGuard {
    fn drop(&mut self) {
        self.cleanup.run();
    }
}

fn proxy_guard(proxy: &Proxy) -> ProxyGuard {
    let cleanup_proxy = proxy.clone();
    let cleanup = ProxyCleanup::new(Arc::new(move || cleanup_proxy.stop()));
    ProxyGuard {
        cleanup,
        proxy: proxy.clone(),
    }
}

/// Shared between the run path and the signal thread. The proxy action is installed as
/// soon as the engine has created it, before any inspection can fail.
struct SignalControl {
    policy: crate::net::PolicyCleanup,
    terminating: AtomicBool,
    teardown: Mutex<()>,
    agent_client: Arc<Mutex<Option<std::process::Child>>>,
    client_cleanup: Mutex<Option<ProxyCleanup>>,
    agent_cleanup_failed: Mutex<Option<Arc<AtomicBool>>>,
    agent: Mutex<Option<ProxyCleanup>>,
    proxy: Mutex<Option<ProxyCleanup>>,
}

impl SignalControl {
    fn install_proxy(&self, cleanup: ProxyCleanup) {
        *lock_cleanup(&self.proxy) = Some(cleanup);
    }

    fn finish_agent(&self) {
        finish_agent_lifecycle(
            &self.teardown,
            &self.client_cleanup,
            &self.agent_client,
            &self.agent,
            &self.agent_cleanup_failed,
        );
    }

    fn terminate(&self) {
        self.terminating.store(true, Ordering::Release);
        if !run_gated_teardown(
            &self.teardown,
            &self.client_cleanup,
            &self.agent,
            &self.proxy,
            || {
                let _ = self.policy.retire();
            },
            || {
                !lock_cleanup_failed(&self.agent_cleanup_failed)
                    .take()
                    .is_some_and(|failed| failed.load(Ordering::Acquire))
            },
        ) {
            eprintln!(
                "vhrn: agent cleanup could not be confirmed; proxy and policy were retired to revoke egress"
            );
        }
    }

    fn create_agent(
        &self,
        engine: &str,
        args: &[String],
        cleanup: ProxyCleanup,
        cleanup_failed: Arc<AtomicBool>,
    ) -> Result<()> {
        create_agent_with(
            &self.teardown,
            &self.terminating,
            &self.agent,
            &self.agent_cleanup_failed,
            cleanup,
            cleanup_failed,
            || {
                let status = Command::new(engine)
                    .args(args)
                    .stdout(Stdio::null())
                    .status()?;
                if status.success() {
                    Ok(())
                } else {
                    bail!("agent container creation failed ({status})")
                }
            },
        )
    }

    fn start_agent(&self, engine: &str, args: &[String]) -> Result<()> {
        begin_agent_attach(&self.teardown, &self.terminating, &self.agent, || {
            let child = Command::new(engine).args(args).spawn().map_err(|error| {
                anyhow::anyhow!("could not start and attach agent container: {error}")
            })?;
            *lock_child(&self.agent_client) = Some(child);
            let client = Arc::clone(&self.agent_client);
            *lock_cleanup(&self.client_cleanup) = Some(ProxyCleanup::new(Arc::new(move || {
                if let Some(mut child) = lock_child(&client).take() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            })));
            Ok(())
        })
    }

    fn wait_agent(&self) -> Result<std::process::ExitStatus> {
        loop {
            let status = {
                let mut child = lock_child(&self.agent_client);
                let Some(child) = child.as_mut() else {
                    bail!("agent engine client was terminated");
                };
                child.try_wait()?
            };
            if let Some(status) = status {
                return Ok(status);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn create_agent_with<T>(
    gate: &Mutex<()>,
    terminating: &AtomicBool,
    agent: &Mutex<Option<ProxyCleanup>>,
    cleanup_failed: &Mutex<Option<Arc<AtomicBool>>>,
    cleanup: ProxyCleanup,
    failed: Arc<AtomicBool>,
    create: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let _teardown = lock_teardown(gate);
    if terminating.load(Ordering::Acquire) {
        bail!("termination requested before agent creation");
    }
    let created = create()?;
    *lock_cleanup(agent) = Some(cleanup);
    *lock_cleanup_failed(cleanup_failed) = Some(failed);
    if terminating.load(Ordering::Acquire) {
        if let Some(agent) = lock_cleanup(agent).take() {
            agent.run();
        }
        bail!("termination requested during agent creation");
    }
    Ok(created)
}

fn begin_agent_attach<T>(
    gate: &Mutex<()>,
    terminating: &AtomicBool,
    agent: &Mutex<Option<ProxyCleanup>>,
    start: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let _teardown = lock_teardown(gate);
    if terminating.load(Ordering::Acquire) {
        if let Some(agent) = lock_cleanup(agent).take() {
            agent.run();
        }
        bail!("termination requested before agent attach");
    }
    match start() {
        Ok(value) => Ok(value),
        Err(error) => {
            if let Some(agent) = lock_cleanup(agent).take() {
                agent.run();
            }
            Err(error)
        }
    }
}

#[cfg(test)]
fn begin_agent_launch<T>(
    gate: &Mutex<()>,
    terminating: &AtomicBool,
    slot: &Mutex<Option<ProxyCleanup>>,
    cleanup: ProxyCleanup,
    spawn: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let _teardown = lock_teardown(gate);
    if terminating.load(Ordering::Acquire) {
        bail!("termination requested before agent launch");
    }
    *lock_cleanup(slot) = Some(cleanup);
    match spawn() {
        Ok(value) => Ok(value),
        Err(error) => {
            disarm_cleanup(slot);
            Err(error)
        }
    }
}

fn lock_teardown(gate: &Mutex<()>) -> std::sync::MutexGuard<'_, ()> {
    gate.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn run_gated_teardown<F, C>(
    gate: &Mutex<()>,
    client: &Mutex<Option<ProxyCleanup>>,
    agent: &Mutex<Option<ProxyCleanup>>,
    proxy: &Mutex<Option<ProxyCleanup>>,
    policy: F,
    confirmed: C,
) -> bool
where
    F: FnOnce(),
    C: FnOnce() -> bool,
{
    let _teardown = lock_teardown(gate);
    if let Some(client) = lock_cleanup(client).take() {
        client.run();
    }
    if let Some(agent) = lock_cleanup(agent).take() {
        agent.run();
    }
    let confirmed = confirmed();
    run_termination(proxy, policy);
    confirmed
}

fn lock_child(
    slot: &Mutex<Option<std::process::Child>>,
) -> std::sync::MutexGuard<'_, Option<std::process::Child>> {
    slot.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn lock_cleanup_failed(
    slot: &Mutex<Option<Arc<AtomicBool>>>,
) -> std::sync::MutexGuard<'_, Option<Arc<AtomicBool>>> {
    slot.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn disarm_cleanup(slot: &Mutex<Option<ProxyCleanup>>) {
    let _ = lock_cleanup(slot).take();
}

/// Finish a normally-returned engine client under the same gate as termination. The child
/// has already been reaped by `wait_agent`; clear both client ownership records before
/// stopping the named container, since a detach can leave that container behind.
fn finish_agent_lifecycle(
    gate: &Mutex<()>,
    client_cleanup: &Mutex<Option<ProxyCleanup>>,
    agent_client: &Mutex<Option<std::process::Child>>,
    agent: &Mutex<Option<ProxyCleanup>>,
    agent_cleanup_failed: &Mutex<Option<Arc<AtomicBool>>>,
) {
    let _teardown = lock_teardown(gate);
    disarm_cleanup(client_cleanup);
    let _ = lock_child(agent_client).take();
    finish_agent_cleanup_locked(agent);
    let _ = lock_cleanup_failed(agent_cleanup_failed).take();
}

fn finish_agent_cleanup_locked(slot: &Mutex<Option<ProxyCleanup>>) {
    if let Some(agent) = lock_cleanup(slot).take() {
        agent.run();
    }
}

fn run_termination<F>(proxy: &Mutex<Option<ProxyCleanup>>, policy: F)
where
    F: FnOnce(),
{
    if let Some(proxy) = lock_cleanup(proxy).take() {
        proxy.run();
    }
    policy();
}

fn lock_cleanup(
    slot: &Mutex<Option<ProxyCleanup>>,
) -> std::sync::MutexGuard<'_, Option<ProxyCleanup>> {
    slot.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn install_signal_control(policy: crate::net::PolicyCleanup) -> Result<Arc<SignalControl>> {
    let mut signals = Signals::new([SIGINT, SIGTERM])?;
    let control = Arc::new(SignalControl {
        policy,
        terminating: AtomicBool::new(false),
        teardown: Mutex::new(()),
        agent_client: Arc::new(Mutex::new(None)),
        client_cleanup: Mutex::new(None),
        agent_cleanup_failed: Mutex::new(None),
        agent: Mutex::new(None),
        proxy: Mutex::new(None),
    });
    let signal_control = Arc::clone(&control);
    std::thread::spawn(move || {
        for sig in signals.forever() {
            if sig == SIGTERM {
                signal_control.terminate();
                std::process::exit(1);
            }
        }
    });
    Ok(control)
}

fn agent_name() -> String {
    format!("vhrn-agent-{}", std::process::id())
}

fn agent_cleanup(engine: &str, name: &str, failed: Arc<AtomicBool>) -> ProxyCleanup {
    let engine = engine.to_string();
    let name = name.to_string();
    ProxyCleanup::new(Arc::new(move || {
        if let Err(error) = stop_and_confirm_agent(&engine, &name) {
            failed.store(true, Ordering::Release);
            eprintln!("vhrn: could not confirm agent container {name:?} stopped: {error}");
        }
    }))
}

#[derive(Debug, PartialEq, Eq)]
enum AgentInspect {
    Present,
    Absent,
    EngineError(String),
}

fn classify_agent_inspect(success: bool, status: Option<i32>, stderr: &[u8]) -> AgentInspect {
    if success {
        return AgentInspect::Present;
    }

    let diagnostic = String::from_utf8_lossy(stderr);
    let folded = diagnostic.to_ascii_lowercase();
    if folded.contains("not found")
        || folded.contains("notfound")
        || folded.contains("no such object")
        || folded.contains("no such container")
    {
        return AgentInspect::Absent;
    }

    let sanitized = diagnostic
        .trim()
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    AgentInspect::EngineError(if sanitized.is_empty() {
        format!(
            "inspect exited unsuccessfully (status {}) without an absence diagnostic",
            status.map_or_else(|| "signal".to_string(), |code| code.to_string())
        )
    } else {
        format!(
            "inspect exited unsuccessfully (status {}): {sanitized}",
            status.map_or_else(|| "signal".to_string(), |code| code.to_string())
        )
    })
}

fn inspect_agent(engine: &str, name: &str) -> Result<AgentInspect> {
    let output = Command::new(engine)
        .args(["inspect", name])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()?;
    Ok(classify_agent_inspect(
        output.status.success(),
        output.status.code(),
        &output.stderr,
    ))
}

fn stop_agent(engine: &str, name: &str, command: &str) -> Result<()> {
    let _ = Command::new(engine)
        .args([command, name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .output()?;
    Ok(())
}

fn agent_force_remove_args(engine: &str, name: &str) -> Vec<String> {
    if engine == "container" {
        vec!["delete".into(), "--force".into(), name.into()]
    } else {
        vec!["rm".into(), "--force".into(), name.into()]
    }
}

fn remove_status_error(status: Option<i32>, stderr: &[u8]) -> Result<()> {
    let state = classify_agent_inspect(false, status, stderr);
    match state {
        AgentInspect::Absent => Ok(()),
        AgentInspect::EngineError(error) => bail!("force-remove failed: {error}"),
        AgentInspect::Present => unreachable!("unsuccessful remove cannot classify as present"),
    }
}

fn force_remove_agent(engine: &str, name: &str) -> Result<()> {
    let output = Command::new(engine)
        .args(agent_force_remove_args(engine, name))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()?;
    if output.status.success() {
        Ok(())
    } else {
        remove_status_error(output.status.code(), &output.stderr)
    }
}

fn stop_and_confirm_agent(engine: &str, name: &str) -> Result<()> {
    stop_agent(engine, name, "stop")?;
    match inspect_agent(engine, name)? {
        AgentInspect::Absent => Ok(()),
        AgentInspect::EngineError(error) => {
            bail!("could not confirm whether container is absent after stop: {error}")
        }
        AgentInspect::Present => {
            stop_agent(engine, name, "kill")?;
            match inspect_agent(engine, name)? {
                AgentInspect::Absent => Ok(()),
                AgentInspect::Present => {
                    force_remove_agent(engine, name)?;
                    match inspect_agent(engine, name)? {
                        AgentInspect::Absent => Ok(()),
                        AgentInspect::Present => bail!(
                            "container remains after stop, kill, and force-remove; remove {name:?} manually"
                        ),
                        AgentInspect::EngineError(error) => {
                            bail!(
                                "could not confirm whether container is absent after force-remove: {error}"
                            )
                        }
                    }
                }
                AgentInspect::EngineError(error) => {
                    bail!("could not confirm whether container is absent after kill: {error}")
                }
            }
        }
    }
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
fn start_proxy(
    engine: &str,
    image: &str,
    policy_dir: &Path,
    run_id: &str,
    project_key: &str,
    port: &str,
    control: &SignalControl,
) -> Result<(ProxyGuard, String)> {
    start_proxy_with(
        engine,
        image,
        policy_dir,
        run_id,
        project_key,
        port,
        |cleanup| control.install_proxy(cleanup),
        Proxy::inspect_ip,
    )
}

fn proxy_args(policy_dir: &Path, run_id: &str, project_key: &str, port: &str) -> Vec<String> {
    vec![
        "--volume".into(),
        format!("{}:/etc/vhrn:ro", policy_dir.display()),
        "--volume".into(),
        format!("{}:/var/log/vhrn", policy_dir.join("log").display()),
        "--env".into(),
        format!(
            "VHRN_ALLOWLISTS=/etc/vhrn/runs/{run_id}/base.allow,/etc/vhrn/runs/{run_id}/harness.allow,/etc/vhrn/allow.local,/etc/vhrn/projects/{project_key}/allow.local,/etc/vhrn/runs/{run_id}/run.allow"
        ),
        "--env".into(),
        format!("VHRN_MODE_FILE=/etc/vhrn/runs/{run_id}/mode"),
        "--env".into(),
        "VHRN_DENY_LOG=/var/log/vhrn/denied.log".into(),
        "--env".into(),
        format!("VHRN_PROXY_LISTEN=:{port}"),
    ]
}

#[allow(clippy::too_many_arguments)] // injected lifecycle seams avoid a real engine in tests
fn start_proxy_with<F, I>(
    engine: &str,
    image: &str,
    policy_dir: &Path,
    run_id: &str,
    project_key: &str,
    port: &str,
    publish: F,
    inspect: I,
) -> Result<(ProxyGuard, String)>
where
    F: FnOnce(ProxyCleanup),
    I: Fn(&Proxy) -> String,
{
    let name = format!("vhrn-proxy-{}", std::process::id());
    let status = Command::new(engine)
        .args(["run", "-d", "--rm", "--name", &name])
        .args(proxy_args(policy_dir, run_id, project_key, port))
        .arg(image)
        .stdout(Stdio::null()) // discard the container id; keep our stdout clean
        .stderr(Stdio::inherit())
        .status();
    if !matches!(status, Ok(s) if s.success()) {
        bail!("proxy failed to start (is the {image:?} image built?)");
    }
    let proxy = Proxy {
        engine: engine.to_string(),
        name: name.clone(),
    };

    let guard = proxy_guard(&proxy);
    finish_started_proxy(guard, publish, inspect, std::thread::sleep)
}

fn finish_started_proxy<F, I, S>(
    guard: ProxyGuard,
    publish: F,
    inspect: I,
    sleep: S,
) -> Result<(ProxyGuard, String)>
where
    F: FnOnce(ProxyCleanup),
    I: Fn(&Proxy) -> String,
    S: Fn(Duration),
{
    publish(guard.cleanup.clone());
    let mut ip = String::new();
    for _ in 0..30 {
        ip = inspect(&guard.proxy);
        if !ip.is_empty() {
            break;
        }
        sleep(Duration::from_millis(300));
    }
    if ip.is_empty() {
        bail!("proxy started but did not receive an IP address");
    }
    Ok((guard, ip))
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
    pub policy_project: Option<crate::net::ProjectIdentity>, // exact canonical bytes
    pub policy_state: PathBuf,
    pub key: String,         // history key: [^A-Za-z0-9] -> '-'
    pub state: String,       // <cache>/state/<harness> -> the container's persistent config dir
    pub sandbox: String,     // <cache>/sandbox/<harness> -> disposable synced config
    pub config_dir: String,  // container config dir, e.g. /home/dev/.claude
    pub host_config: String, // host config dir, e.g. ~/.claude
    pub history: String,     // <host_config>/projects/<key>; empty unless the harness shares it
    pub sessions: String,    // <cache>/state/<harness>-sessions/<key>; empty = not partitioned
    pub config: Config,      // merged defaults + global + project config
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
    let policy_project = crate::net::ProjectIdentity::from_canonical(project.clone())?;
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
        policy_project: Some(policy_project),
        policy_state: vhrn_state(&home),
        key,
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

/// Convert the established run-shaped argv into a synchronous create without changing the
/// resource, mount, image, or agent-argument ordering.
fn agent_create_args(run_args: &[String], name: &str) -> Vec<String> {
    let mut args = run_args.to_vec();
    assert_eq!(args.first().map(String::as_str), Some("run"));
    args[0] = "create".to_string();
    args.splice(1..1, ["--name".to_string(), name.to_string()]);
    args
}

fn agent_start_args(name: &str) -> Vec<String> {
    vec![
        "start".to_string(),
        "--attach".to_string(),
        "--interactive".to_string(),
        name.to_string(),
    ]
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

/// Publish the egress policy, start the proxy sidecar, then run the jailed container with all
/// egress pinned to the proxy. The container run inherits the terminal; its exit status is
/// returned verbatim as the process exit code.
fn start_container(mut cfg: ContainerConfig, f: &RunFlags) -> Result<i32> {
    let port = env_or("VHRN_PROXY_PORT", "8080");
    let mode = if f.open_net {
        Mode::Open
    } else {
        Mode::Enforce
    };
    let project = cfg
        .policy_project
        .take()
        .ok_or_else(|| anyhow::anyhow!("missing canonical project identity"))?;
    let store = crate::net::PolicyStore::new(&cfg.policy_state);
    let policy_run = store.publish_run(
        &cfg.harness.name,
        &project,
        &cfg.harness.allow_domains,
        &f.extra_allow,
        mode,
    )?;
    let signal_control = install_signal_control(policy_run.cleanup_handle())?;
    let policy_dir = store.root().to_path_buf();
    let run_id = policy_run
        .id()
        .and_then(|id| id.to_str())
        .ok_or_else(|| anyhow::anyhow!("invalid policy run id"))?
        .to_string();

    // The guide lands wherever the harness reads it from: the disposable sandbox, or the
    // state dir for an agent that resolves it under its own config dir.
    let guide_dst = if cfg.harness.guide.in_state {
        &cfg.state
    } else {
        &cfg.sandbox
    };
    let project_shell = project.shell_quote();
    if let Err(e) = crate::persist::write_container_guide(
        Path::new(&cfg.host_config),
        Path::new(guide_dst),
        &cfg.harness,
        mode == Mode::Open,
        &project_shell,
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
    let (_proxy, ip) = start_proxy(
        &cfg.engine,
        &proxy_image,
        &policy_dir,
        &run_id,
        project.key(),
        &port,
        &signal_control,
    )?;

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

    let name = agent_name();
    let run_args = container_run_args(&cfg, f, mode, &ip, &port);
    let create_args = agent_create_args(&run_args, &name);
    let attach_args = agent_start_args(&name);
    let agent_cleanup_failed = Arc::new(AtomicBool::new(false));
    let agent = agent_cleanup(&cfg.engine, &name, Arc::clone(&agent_cleanup_failed));
    signal_control.create_agent(&cfg.engine, &create_args, agent, agent_cleanup_failed)?;
    signal_control.start_agent(&cfg.engine, &attach_args)?;
    let status = signal_control.wait_agent();
    signal_control.finish_agent();
    let status = status?;
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
    fn vhrn_state_resolution() {
        let home = Path::new("/home/u");
        assert_eq!(
            vhrn_state_from(home, Some("/x/state")),
            Path::new("/x/state/vhrn")
        );
        assert_eq!(
            vhrn_state_from(home, Some("relative")),
            Path::new("/home/u/.local/state/vhrn")
        );
        assert_eq!(
            vhrn_state_from(home, Some("")),
            Path::new("/home/u/.local/state/vhrn")
        );
        assert_eq!(
            vhrn_state_from(home, None),
            Path::new("/home/u/.local/state/vhrn")
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
    fn proxy_args_mount_the_policy_layers_without_leaking_host_paths() {
        let args = proxy_args(
            Path::new("/state,with-comma/net"),
            "12-34",
            "project-key",
            "8080",
        );
        let want = vec![
                "--volume", "/state,with-comma/net:/etc/vhrn:ro",
                "--volume", "/state,with-comma/net/log:/var/log/vhrn",
                "--env", "VHRN_ALLOWLISTS=/etc/vhrn/runs/12-34/base.allow,/etc/vhrn/runs/12-34/harness.allow,/etc/vhrn/allow.local,/etc/vhrn/projects/project-key/allow.local,/etc/vhrn/runs/12-34/run.allow",
                "--env", "VHRN_MODE_FILE=/etc/vhrn/runs/12-34/mode",
                "--env", "VHRN_DENY_LOG=/var/log/vhrn/denied.log",
                "--env", "VHRN_PROXY_LISTEN=:8080",
            ].into_iter().map(str::to_string).collect::<Vec<_>>();
        assert_eq!(args, want);
        assert!(
            !args
                .iter()
                .any(|arg| arg.starts_with("VHRN_") && arg.contains("/state,with-comma/net")),
            "host policy root leaked into proxy environment: {args:?}"
        );
    }

    #[test]
    fn cleanup_action_is_idempotent() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls_for_action = Arc::clone(&calls);
        let cleanup = ProxyCleanup::new(Arc::new(move || {
            calls_for_action.fetch_add(1, Ordering::Relaxed);
        }));
        let clone = cleanup.clone();
        cleanup.run();
        clone.run();
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn agent_inspect_classification_only_accepts_explicit_absence() {
        assert_eq!(
            classify_agent_inspect(true, Some(0), b""),
            AgentInspect::Present
        );
        for diagnostic in [
            b"not found: container vhrn-agent".as_slice(),
            b"notFound: container vhrn-agent".as_slice(),
            b"Error response from daemon: No such object: vhrn-agent".as_slice(),
            b"Error: No such container: vhrn-agent".as_slice(),
        ] {
            assert_eq!(
                classify_agent_inspect(false, Some(1), diagnostic),
                AgentInspect::Absent,
                "absence diagnostic: {:?}",
                String::from_utf8_lossy(diagnostic)
            );
        }
        assert_eq!(
            classify_agent_inspect(false, Some(1), b""),
            AgentInspect::EngineError(
                "inspect exited unsuccessfully (status 1) without an absence diagnostic".into()
            )
        );
        assert_eq!(
            classify_agent_inspect(
                false,
                Some(125),
                b"permission denied\n\x1b[31mengine\x1b[0m"
            ),
            AgentInspect::EngineError(
                "inspect exited unsuccessfully (status 125): permission denied  [31mengine [0m"
                    .into()
            )
        );
    }

    #[test]
    fn force_remove_argv_and_confirmation_errors_are_explicit() {
        assert_eq!(
            agent_force_remove_args("container", "vhrn-agent-test"),
            ["delete", "--force", "vhrn-agent-test"]
        );
        assert_eq!(
            agent_force_remove_args("docker", "vhrn-agent-test"),
            ["rm", "--force", "vhrn-agent-test"]
        );
        assert!(remove_status_error(Some(1), b"No such container: vhrn-agent-test").is_ok());
        assert!(remove_status_error(Some(125), b"permission denied").is_err());
    }

    #[test]
    fn termination_runs_proxy_before_policy_once() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let proxy_order = Arc::clone(&order);
        let proxy = ProxyCleanup::new(Arc::new(move || proxy_order.lock().unwrap().push("proxy")));
        let policy = Arc::new(AtomicBool::new(false));
        let policy_once = Arc::clone(&policy);
        let policy_order = Arc::clone(&order);
        let slot = Mutex::new(Some(proxy));
        run_termination(&slot, move || {
            if !policy_once.swap(true, Ordering::AcqRel) {
                policy_order.lock().unwrap().push("policy");
            }
        });
        let policy_once = Arc::clone(&policy);
        let policy_order = Arc::clone(&order);
        run_termination(&slot, move || {
            if !policy_once.swap(true, Ordering::AcqRel) {
                policy_order.lock().unwrap().push("policy");
            }
        });
        assert_eq!(*order.lock().unwrap(), ["proxy", "policy"]);
    }

    #[test]
    fn sigterm_stops_agent_before_proxy_and_policy() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let agent_order = Arc::clone(&order);
        let agent = ProxyCleanup::new(Arc::new(move || agent_order.lock().unwrap().push("agent")));
        let proxy_order = Arc::clone(&order);
        let proxy = ProxyCleanup::new(Arc::new(move || proxy_order.lock().unwrap().push("proxy")));
        let policy_order = Arc::clone(&order);
        let gate = Mutex::new(());
        let clients = Mutex::new(None);
        let agents = Mutex::new(Some(agent));
        let proxies = Mutex::new(Some(proxy));
        run_gated_teardown(
            &gate,
            &clients,
            &agents,
            &proxies,
            move || policy_order.lock().unwrap().push("policy"),
            || true,
        );
        run_gated_teardown(&gate, &clients, &agents, &proxies, || {}, || true);
        assert_eq!(*order.lock().unwrap(), ["agent", "proxy", "policy"]);
    }

    #[test]
    fn unconfirmed_agent_cleanup_still_revokes_proxy_and_policy() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let gate = Mutex::new(());
        let clients = Mutex::new(None);
        let agent_order = Arc::clone(&order);
        let agents = Mutex::new(Some(ProxyCleanup::new(Arc::new(move || {
            agent_order.lock().unwrap().push("agent");
        }))));
        let proxy_order = Arc::clone(&order);
        let proxies = Mutex::new(Some(ProxyCleanup::new(Arc::new(move || {
            proxy_order.lock().unwrap().push("proxy");
        }))));
        let policy_order = Arc::clone(&order);
        assert!(!run_gated_teardown(
            &gate,
            &clients,
            &agents,
            &proxies,
            move || policy_order.lock().unwrap().push("policy"),
            || false,
        ));
        assert_eq!(*order.lock().unwrap(), ["agent", "proxy", "policy"]);
        assert!(lock_cleanup(&proxies).is_none());
    }

    #[test]
    fn engine_client_return_clears_client_ownership_then_stops_surviving_agent() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let gate = Mutex::new(());
        let clients = Mutex::new(Some(ProxyCleanup::new(Arc::new({
            let order = Arc::clone(&order);
            move || order.lock().unwrap().push("client")
        }))));
        let child = Mutex::new(None);
        let failed = Mutex::new(Some(Arc::new(AtomicBool::new(false))));
        let agent_order = Arc::clone(&order);
        let agents = Mutex::new(Some(ProxyCleanup::new(Arc::new(move || {
            agent_order.lock().unwrap().push("agent");
        }))));
        // Client return does not prove --rm removed a detached agent.
        finish_agent_lifecycle(&gate, &clients, &child, &agents, &failed);
        finish_agent_lifecycle(&gate, &clients, &child, &agents, &failed);
        assert!(lock_cleanup(&clients).is_none());
        assert!(lock_child(&child).is_none());
        assert!(lock_cleanup_failed(&failed).is_none());
        let proxy_order = Arc::clone(&order);
        let proxies = Mutex::new(Some(ProxyCleanup::new(Arc::new(move || {
            proxy_order.lock().unwrap().push("proxy");
        }))));
        let policy_order = Arc::clone(&order);
        run_gated_teardown(
            &gate,
            &clients,
            &agents,
            &proxies,
            move || policy_order.lock().unwrap().push("policy"),
            || true,
        );
        assert_eq!(*order.lock().unwrap(), ["agent", "proxy", "policy"]);
    }

    #[test]
    fn normal_finish_waits_for_the_entire_sigterm_teardown() {
        use std::sync::mpsc;

        let gate = Arc::new(Mutex::new(()));
        let clients = Arc::new(Mutex::new(None));
        let agent_client = Arc::new(Mutex::new(None));
        let agents = Arc::new(Mutex::new(None));
        let proxies = Arc::new(Mutex::new(None));
        let order = Arc::new(Mutex::new(Vec::new()));
        let (agent_started, agent_started_rx) = mpsc::channel();
        let (release_agent, release_agent_rx) = mpsc::channel();
        let release_agent_rx = Mutex::new(release_agent_rx);
        let agent_order = Arc::clone(&order);
        *lock_cleanup(&agents) = Some(ProxyCleanup::new(Arc::new(move || {
            agent_order.lock().unwrap().push("agent");
            agent_started.send(()).unwrap();
            release_agent_rx.lock().unwrap().recv().unwrap();
        })));
        let proxy_order = Arc::clone(&order);
        *lock_cleanup(&proxies) = Some(ProxyCleanup::new(Arc::new(move || {
            proxy_order.lock().unwrap().push("proxy");
        })));

        let signal_gate = Arc::clone(&gate);
        let signal_clients = Arc::clone(&clients);
        let signal_agents = Arc::clone(&agents);
        let signal_proxies = Arc::clone(&proxies);
        let policy_order = Arc::clone(&order);
        let signal = std::thread::spawn(move || {
            run_gated_teardown(
                &signal_gate,
                &signal_clients,
                &signal_agents,
                &signal_proxies,
                move || {
                    policy_order.lock().unwrap().push("policy");
                },
                || true,
            );
        });
        agent_started_rx.recv().unwrap();

        let (normal_done, normal_done_rx) = mpsc::channel();
        let normal_gate = Arc::clone(&gate);
        let normal_clients = Arc::clone(&clients);
        let normal_agent_client = Arc::clone(&agent_client);
        let normal_agents = Arc::clone(&agents);
        let normal_failed = Arc::new(Mutex::new(None));
        let normal_failed_for_finish = Arc::clone(&normal_failed);
        let normal = std::thread::spawn(move || {
            finish_agent_lifecycle(
                &normal_gate,
                &normal_clients,
                &normal_agent_client,
                &normal_agents,
                &normal_failed_for_finish,
            );
            normal_done.send(()).unwrap();
        });
        assert!(normal_done_rx.try_recv().is_err());
        release_agent.send(()).unwrap();
        signal.join().unwrap();
        normal_done_rx.recv().unwrap();
        normal.join().unwrap();
        assert_eq!(*order.lock().unwrap(), ["agent", "proxy", "policy"]);
    }

    #[test]
    fn termination_before_registration_prevents_spawn() {
        let gate = Mutex::new(());
        let terminating = AtomicBool::new(true);
        let slot = Mutex::new(None);
        let calls = Arc::new(AtomicBool::new(false));
        let calls_for_spawn = Arc::clone(&calls);
        let cleanup = ProxyCleanup::new(Arc::new(|| {}));
        assert!(
            begin_agent_launch(&gate, &terminating, &slot, cleanup, move || {
                calls_for_spawn.store(true, Ordering::Release);
                Ok(())
            })
            .is_err()
        );
        assert!(!calls.load(Ordering::Acquire));
    }

    #[test]
    fn synchronous_create_observes_termination_before_attach() {
        use std::sync::mpsc;

        let gate = Arc::new(Mutex::new(()));
        let terminating = Arc::new(AtomicBool::new(false));
        let agents = Arc::new(Mutex::new(None));
        let failed_slot = Arc::new(Mutex::new(None));
        let clients = Arc::new(Mutex::new(None));
        let proxies = Arc::new(Mutex::new(None));
        let resource = Arc::new(AtomicBool::new(false));
        let order = Arc::new(Mutex::new(Vec::new()));
        let (create_started, create_started_rx) = mpsc::channel();
        let (release_create, release_create_rx) = mpsc::channel();

        let create_gate = Arc::clone(&gate);
        let create_terminating = Arc::clone(&terminating);
        let create_agents = Arc::clone(&agents);
        let create_failed_slot = Arc::clone(&failed_slot);
        let create_resource = Arc::clone(&resource);
        let cleanup_resource = Arc::clone(&resource);
        let cleanup_order = Arc::clone(&order);
        let create = std::thread::spawn(move || {
            create_agent_with(
                &create_gate,
                &create_terminating,
                &create_agents,
                &create_failed_slot,
                ProxyCleanup::new(Arc::new(move || {
                    assert!(cleanup_resource.swap(false, Ordering::AcqRel));
                    cleanup_order.lock().unwrap().push("agent");
                })),
                Arc::new(AtomicBool::new(false)),
                || {
                    create_started.send(()).unwrap();
                    release_create_rx.recv().unwrap();
                    create_resource.store(true, Ordering::Release);
                    Ok(())
                },
            )
        });
        create_started_rx.recv().unwrap();
        let proxy_order = Arc::clone(&order);
        *lock_cleanup(&proxies) = Some(ProxyCleanup::new(Arc::new(move || {
            proxy_order.lock().unwrap().push("proxy");
        })));
        let signal_gate = Arc::clone(&gate);
        let signal_terminating = Arc::clone(&terminating);
        let signal_clients = Arc::clone(&clients);
        let signal_agents = Arc::clone(&agents);
        let signal_proxies = Arc::clone(&proxies);
        let policy_order = Arc::clone(&order);
        let (signal_requested, signal_requested_rx) = mpsc::channel();
        let signal = std::thread::spawn(move || {
            signal_terminating.store(true, Ordering::Release);
            signal_requested.send(()).unwrap();
            run_gated_teardown(
                &signal_gate,
                &signal_clients,
                &signal_agents,
                &signal_proxies,
                move || policy_order.lock().unwrap().push("policy"),
                || true,
            );
        });
        signal_requested_rx.recv().unwrap();
        release_create.send(()).unwrap();
        assert!(create.join().unwrap().is_err());
        signal.join().unwrap();
        assert_eq!(*order.lock().unwrap(), ["agent", "proxy", "policy"]);
        assert!(!resource.load(Ordering::Acquire));
    }

    #[test]
    fn termination_between_create_and_attach_cleans_without_starting_attach() {
        let gate = Mutex::new(());
        let terminating = AtomicBool::new(false);
        let agents = Mutex::new(None);
        let failed_slot = Mutex::new(None);
        let resource = Arc::new(AtomicBool::new(false));
        let cleanup_resource = Arc::clone(&resource);
        create_agent_with(
            &gate,
            &terminating,
            &agents,
            &failed_slot,
            ProxyCleanup::new(Arc::new(move || {
                assert!(cleanup_resource.swap(false, Ordering::AcqRel));
            })),
            Arc::new(AtomicBool::new(false)),
            || {
                resource.store(true, Ordering::Release);
                Ok(())
            },
        )
        .unwrap();
        terminating.store(true, Ordering::Release);
        let attached = AtomicBool::new(false);
        assert!(
            begin_agent_attach(&gate, &terminating, &agents, || {
                attached.store(true, Ordering::Release);
                Ok(())
            })
            .is_err()
        );
        assert!(!attached.load(Ordering::Acquire));
        assert!(!resource.load(Ordering::Acquire));
    }

    #[test]
    fn failed_create_does_not_publish_or_clean_agent_ownership() {
        let gate = Mutex::new(());
        let terminating = AtomicBool::new(false);
        let agents = Mutex::new(None);
        let failed_slot = Mutex::new(None);
        let called = Arc::new(AtomicBool::new(false));
        let cleanup_called = Arc::clone(&called);
        let create = create_agent_with(
            &gate,
            &terminating,
            &agents,
            &failed_slot,
            ProxyCleanup::new(Arc::new(move || {
                cleanup_called.store(true, Ordering::Release);
            })),
            Arc::new(AtomicBool::new(false)),
            || -> Result<()> { bail!("name collision") },
        );
        let attach_started = AtomicBool::new(false);
        if create.is_ok() {
            attach_started.store(true, Ordering::Release);
        }
        assert!(create.is_err());
        assert!(lock_cleanup(&agents).is_none());
        assert!(lock_cleanup_failed(&failed_slot).is_none());
        assert!(!called.load(Ordering::Acquire));
        assert!(!attach_started.load(Ordering::Acquire));
    }

    #[test]
    fn termination_waits_for_registered_spawn_then_cleans_created_agent() {
        use std::sync::mpsc;
        let gate = Arc::new(Mutex::new(()));
        let terminating = Arc::new(AtomicBool::new(false));
        let slot = Arc::new(Mutex::new(None));
        let order = Arc::new(Mutex::new(Vec::new()));
        let (spawn_started, spawn_started_rx) = mpsc::channel();
        let (release_spawn, release_spawn_rx) = mpsc::channel();
        let cleanup_order = Arc::clone(&order);
        let launch_gate = Arc::clone(&gate);
        let launch_term = Arc::clone(&terminating);
        let launch_slot = Arc::clone(&slot);
        let spawn_slot = Arc::clone(&slot);
        let launch = std::thread::spawn(move || {
            begin_agent_launch(
                &launch_gate,
                &launch_term,
                &launch_slot,
                ProxyCleanup::new(Arc::new(move || {
                    cleanup_order.lock().unwrap().push("agent");
                })),
                || {
                    assert!(lock_cleanup(&spawn_slot).is_some());
                    spawn_started.send(()).unwrap();
                    release_spawn_rx.recv().unwrap();
                    Ok(())
                },
            )
        });
        spawn_started_rx.recv().unwrap();
        let signal_gate = Arc::clone(&gate);
        let clients = Arc::new(Mutex::new(None));
        let signal_clients = Arc::clone(&clients);
        let signal_slot = Arc::clone(&slot);
        let signal_term = Arc::clone(&terminating);
        let signal_order = Arc::clone(&order);
        let (termination_requested, termination_requested_rx) = mpsc::channel();
        let signal = std::thread::spawn(move || {
            signal_term.store(true, Ordering::Release);
            termination_requested.send(()).unwrap();
            run_gated_teardown(
                &signal_gate,
                &signal_clients,
                &signal_slot,
                &Mutex::new(None),
                move || {
                    signal_order.lock().unwrap().push("policy");
                },
                || true,
            );
        });
        termination_requested_rx.recv().unwrap();
        assert!(order.lock().unwrap().is_empty());
        release_spawn.send(()).unwrap();
        launch.join().unwrap().unwrap();
        signal.join().unwrap();
        assert_eq!(*order.lock().unwrap(), ["agent", "policy"]);
    }

    #[test]
    fn teardown_reads_confirmation_failure_after_blocked_launch_releases_gate() {
        use std::sync::mpsc;

        let gate = Arc::new(Mutex::new(()));
        let terminating = Arc::new(AtomicBool::new(false));
        let agents = Arc::new(Mutex::new(None));
        let clients = Arc::new(Mutex::new(None));
        let proxies = Arc::new(Mutex::new(None));
        let failure_slot = Arc::new(Mutex::new(None));
        let failed = Arc::new(AtomicBool::new(false));
        let order = Arc::new(Mutex::new(Vec::new()));
        let (launch_holds_gate, launch_holds_gate_rx) = mpsc::channel();
        let (release_launch, release_launch_rx) = mpsc::channel();

        let launch_gate = Arc::clone(&gate);
        let launch_terminating = Arc::clone(&terminating);
        let launch_agents = Arc::clone(&agents);
        let launch_failure_slot = Arc::clone(&failure_slot);
        let launch_failed = Arc::clone(&failed);
        let agent_order = Arc::clone(&order);
        let launch = std::thread::spawn(move || {
            begin_agent_launch(
                &launch_gate,
                &launch_terminating,
                &launch_agents,
                ProxyCleanup::new(Arc::new(move || {
                    launch_failed.store(true, Ordering::Release);
                    agent_order.lock().unwrap().push("agent");
                })),
                || {
                    launch_holds_gate.send(()).unwrap();
                    release_launch_rx.recv().unwrap();
                    *lock_cleanup_failed(&launch_failure_slot) = Some(Arc::clone(&failed));
                    Ok(())
                },
            )
        });
        launch_holds_gate_rx.recv().unwrap();

        let proxy_order = Arc::clone(&order);
        *lock_cleanup(&proxies) = Some(ProxyCleanup::new(Arc::new(move || {
            proxy_order.lock().unwrap().push("proxy");
        })));
        let signal_gate = Arc::clone(&gate);
        let signal_terminating = Arc::clone(&terminating);
        let signal_clients = Arc::clone(&clients);
        let signal_agents = Arc::clone(&agents);
        let signal_proxies = Arc::clone(&proxies);
        let signal_failure_slot = Arc::clone(&failure_slot);
        let policy_order = Arc::clone(&order);
        let (signal_started, signal_started_rx) = mpsc::channel();
        let signal = std::thread::spawn(move || {
            signal_terminating.store(true, Ordering::Release);
            signal_started.send(()).unwrap();
            run_gated_teardown(
                &signal_gate,
                &signal_clients,
                &signal_agents,
                &signal_proxies,
                move || policy_order.lock().unwrap().push("policy"),
                || {
                    !lock_cleanup_failed(&signal_failure_slot)
                        .take()
                        .is_some_and(|failed| failed.load(Ordering::Acquire))
                },
            )
        });
        signal_started_rx.recv().unwrap();
        assert!(order.lock().unwrap().is_empty());
        release_launch.send(()).unwrap();
        launch.join().unwrap().unwrap();
        assert!(!signal.join().unwrap());
        assert_eq!(*order.lock().unwrap(), ["agent", "proxy", "policy"]);
        assert!(lock_cleanup(&proxies).is_none());
    }

    #[test]
    fn starting_teardown_waits_for_daemon_creation_then_confirms_absence_before_policy() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let resource_alive = Arc::new(AtomicBool::new(false));
        let client_order = Arc::clone(&order);
        let client_resource = Arc::clone(&resource_alive);
        let agent_order = Arc::clone(&order);
        let agent_resource = Arc::clone(&resource_alive);
        let proxy_order = Arc::clone(&order);
        let policy_order = Arc::clone(&order);
        let gate = Mutex::new(());
        let clients = Mutex::new(Some(ProxyCleanup::new(Arc::new(move || {
            // The daemon creates the named resource only after the CLI spawn returned.
            client_resource.store(true, Ordering::Release);
            client_order.lock().unwrap().push("client-wait");
        }))));
        let agents = Mutex::new(Some(ProxyCleanup::new(Arc::new(move || {
            assert!(agent_resource.swap(false, Ordering::AcqRel));
            agent_order.lock().unwrap().push("agent-confirmed-absent");
        }))));
        let proxies = Mutex::new(Some(ProxyCleanup::new(Arc::new(move || {
            proxy_order.lock().unwrap().push("proxy");
        }))));
        run_gated_teardown(
            &gate,
            &clients,
            &agents,
            &proxies,
            move || {
                assert!(!resource_alive.load(Ordering::Acquire));
                policy_order.lock().unwrap().push("policy");
            },
            || true,
        );
        assert_eq!(
            *order.lock().unwrap(),
            ["client-wait", "agent-confirmed-absent", "proxy", "policy"]
        );
    }

    fn test_guard(cleanup: ProxyCleanup) -> ProxyGuard {
        ProxyGuard {
            cleanup,
            proxy: Proxy {
                engine: "unused".into(),
                name: "unused".into(),
            },
        }
    }

    #[test]
    fn finish_started_proxy_publishes_before_inspection_and_retires_once_on_error() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let action_calls = Arc::clone(&calls);
        let guard = test_guard(ProxyCleanup::new(Arc::new(move || {
            action_calls.fetch_add(1, Ordering::Relaxed);
        })));
        let slot = Arc::new(Mutex::new(None));
        let publish_slot = Arc::clone(&slot);
        let inspect_slot = Arc::clone(&slot);
        let result = finish_started_proxy(
            guard,
            move |cleanup| *lock_cleanup(&publish_slot) = Some(cleanup),
            move |_| {
                assert!(lock_cleanup(&inspect_slot).is_some());
                String::new()
            },
            |_| {},
        );
        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        lock_cleanup(&slot).as_ref().unwrap().run();
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn finish_started_proxy_allows_termination_during_inspection() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let proxy_order = Arc::clone(&order);
        let guard = test_guard(ProxyCleanup::new(Arc::new(move || {
            proxy_order.lock().unwrap().push("proxy");
        })));
        let slot = Arc::new(Mutex::new(None));
        let publish_slot = Arc::clone(&slot);
        let inspect_slot = Arc::clone(&slot);
        let policy_once = Arc::new(AtomicBool::new(false));
        let policy_for_inspect = Arc::clone(&policy_once);
        let policy_order = Arc::clone(&order);
        let result = finish_started_proxy(
            guard,
            move |cleanup| *lock_cleanup(&publish_slot) = Some(cleanup),
            move |_| {
                run_termination(&inspect_slot, || {
                    if !policy_for_inspect.swap(true, Ordering::AcqRel) {
                        policy_order.lock().unwrap().push("policy");
                    }
                });
                String::new()
            },
            |_| {},
        );
        assert!(result.is_err());
        assert_eq!(*order.lock().unwrap(), ["proxy", "policy"]);
    }

    #[test]
    fn returned_guard_keeps_waiting_cleanup_ordered_and_idempotent() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let proxy_order = Arc::clone(&order);
        let guard = test_guard(ProxyCleanup::new(Arc::new(move || {
            proxy_order.lock().unwrap().push("proxy");
        })));
        let slot = Arc::new(Mutex::new(None));
        let publish_slot = Arc::clone(&slot);
        let (guard, ip) = finish_started_proxy(
            guard,
            move |cleanup| *lock_cleanup(&publish_slot) = Some(cleanup),
            |_| "10.0.0.2".into(),
            |_| {},
        )
        .unwrap();
        assert_eq!(ip, "10.0.0.2");
        let policy_order = Arc::clone(&order);
        run_termination(&slot, move || policy_order.lock().unwrap().push("policy"));
        drop(guard);
        assert_eq!(*order.lock().unwrap(), ["proxy", "policy"]);
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

    #[test]
    fn agent_create_and_start_argv_preserve_run_payload_for_both_engines() {
        let (mut cfg, _dir) = golden_fixture();
        for engine in ["docker", "container"] {
            cfg.engine = engine.into();
            let run = container_run_args(
                &cfg,
                &RunFlags::default(),
                Mode::Enforce,
                "10.0.0.2",
                "8080",
            );
            let create = agent_create_args(&run, "vhrn-agent-test");
            assert_eq!(create[0], "create", "{engine}");
            assert_eq!(&create[1..3], ["--name", "vhrn-agent-test"], "{engine}");
            assert_eq!(&create[3..], &run[1..], "{engine}");
            assert_eq!(
                agent_start_args("vhrn-agent-test"),
                ["start", "--attach", "--interactive", "vhrn-agent-test"],
                "{engine}"
            );
        }
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
            "/etc/vhrn",
            "/var/log/vhrn",
            "VHRN_ALLOWLIST",
            "VHRN_MODE_FILE",
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
        assert!(!args.iter().any(|arg| {
            arg.contains("/etc/vhrn")
                || arg.contains("/var/log/vhrn")
                || arg.starts_with("VHRN_ALLOWLIST")
                || arg.starts_with("VHRN_MODE_FILE")
        }));
    }
}
