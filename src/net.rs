//! Egress guard mode and the five-layer host policy under XDG state. The proxy alone mounts
//! policy snapshots plus live global/project files; the agent never sees them, so `vhrn net`
//! remains the only widening path. Modes are per-published-run files.

use std::collections::HashSet;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use crate::run::set_mode;

/// The egress guard mode for a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Enforce,
    Report,
    Open,
}

impl Mode {
    /// The wire string written to the mode file and `VHRN_NET`.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Mode::Enforce => "enforce",
            Mode::Report => "report",
            Mode::Open => "open",
        }
    }

    /// Parse a mode string; unknown values yield None (callers fall back to enforce).
    fn from_str(s: &str) -> Option<Mode> {
        match s {
            "enforce" => Some(Mode::Enforce),
            "report" => Some(Mode::Report),
            "open" => Some(Mode::Open),
            _ => None,
        }
    }
}

// Per-process unique suffix for atomic temp files (os.CreateTemp's role).
fn next_tmp_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CTR: AtomicU64 = AtomicU64::new(0);
    CTR.fetch_add(1, Ordering::Relaxed)
}

#[allow(dead_code)] // Wired into the run path in the following ship step.
/// The six domains required by the wrapper itself.  These are launch snapshots,
/// rather than user-editable state.
pub(crate) const BASE_ALLOWLIST: [&str; 6] = [
    "github.com",
    "githubusercontent.com",
    "registry.npmjs.org",
    "pypi.org",
    "files.pythonhosted.org",
    "astral.sh",
];

/// Where a resolved domain came from.  Order is intentionally meaningful: it is the
/// order in which the additive layers were inspected.
#[allow(dead_code)] // Kept crate-visible for the forthcoming status command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LayerSource {
    Base,
    Harness(String),
    Global,
    Project(PathBuf),
    Run(String),
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedDomain {
    pub(crate) domain: String,
    pub(crate) sources: Vec<LayerSource>,
}

/// Normalize a user supplied hostname. Stored policy is deliberately ASCII only.
#[allow(dead_code)]
pub(crate) fn normalize_domain(input: &str) -> std::result::Result<String, String> {
    let value = input
        .trim()
        .strip_prefix("*.")
        .unwrap_or(input.trim())
        .trim_matches('.');
    if !value.is_ascii() {
        let suggestion = idna::domain_to_ascii(value)
            .ok()
            .filter(|value| valid_domain(value));
        return Err(match suggestion {
            Some(value) => format!("domain must be ASCII; use {value:?}"),
            None => "domain must be a valid ASCII IDNA hostname".to_string(),
        });
    }
    let value = value.to_ascii_lowercase();
    if valid_domain(&value) {
        Ok(value)
    } else {
        Err(format!("invalid domain {input:?}"))
    }
}

fn valid_domain(value: &str) -> bool {
    !value.is_empty()
        && !value.split('.').any(str::is_empty)
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        && value.bytes().any(|b| b.is_ascii_alphanumeric())
}

/// A byte-stable project identity. Unix paths need not be UTF-8, so neither the key
/// nor collision comparison ever uses lossy text.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectIdentity {
    pub(crate) key: String,
    pub(crate) path: PathBuf,
    bytes: Vec<u8>,
}

#[allow(dead_code)]
impl ProjectIdentity {
    pub(crate) fn from_canonical(path: PathBuf) -> std::io::Result<Self> {
        use sha2::{Digest, Sha256};
        use std::os::unix::ffi::OsStrExt;
        if !path.is_absolute() || !path.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "project must be an absolute directory",
            ));
        }
        let bytes = path.as_os_str().as_bytes().to_vec();
        let key = hex::encode(Sha256::digest(&bytes));
        Ok(Self { key, path, bytes })
    }
    pub(crate) fn from_path(path: &Path) -> std::io::Result<Self> {
        let path = std::fs::canonicalize(path)?;
        Self::from_canonical(path)
    }

    pub(crate) fn display(&self) -> String {
        quote_path_bytes(&self.bytes)
    }
    pub(crate) fn shell_quote(&self) -> Vec<u8> {
        shell_quote_bytes(&self.bytes)
    }
    pub(crate) fn key(&self) -> &str {
        &self.key
    }
}

#[allow(dead_code)]
pub(crate) fn quote_path_bytes(bytes: &[u8]) -> String {
    let mut out = String::from("\"");
    for &b in bytes {
        match b {
            b'"' => out.push_str("\\\""),
            b'\\' => {
                out.push('\\');
                out.push('\\');
            }
            0x20..=0x7e => out.push(b as char),
            _ => {
                use std::fmt::Write;
                let _ = write!(out, "\\x{b:02X}");
            }
        }
    }
    out.push('"');
    out
}

/// POSIX shell representation of arbitrary non-NUL Unix path bytes.
#[allow(dead_code)]
pub(crate) fn shell_quote_bytes(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 2);
    out.push(b'\'');
    for &b in bytes {
        if b == b'\'' {
            out.extend_from_slice(b"'\\''");
        } else {
            out.push(b);
        }
    }
    out.push(b'\'');
    out
}

/// Host-owned policy state. Public operations lock `policy.lock`; helpers ending in
/// `_locked` deliberately do not, avoiding accidental reentrant locking.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub(crate) struct PolicyStore {
    root: PathBuf,
}

#[allow(dead_code)]
impl PolicyStore {
    pub(crate) fn new(state: &Path) -> Self {
        Self {
            root: state.join("net"),
        }
    }
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }
    fn lock_path(&self) -> PathBuf {
        self.root.join("policy.lock")
    }
    fn global(&self) -> PathBuf {
        self.root.join("allow.local")
    }
    fn projects(&self) -> PathBuf {
        self.root.join("projects")
    }
    fn runs(&self) -> PathBuf {
        self.root.join("runs")
    }
    fn log(&self) -> PathBuf {
        self.root.join("log").join("denied.log")
    }

    pub(crate) fn ensure(&self) -> std::io::Result<()> {
        Self::mkdir(&self.root)?;
        Self::mkdir(&self.projects())?;
        Self::mkdir(&self.root.join("log"))?;
        Self::mkdir(&self.runs())?;
        Self::ensure_lock(&self.lock_path())?;
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(self.lock_path())?;
        lock.lock()?;
        let result = self.ensure_files();
        lock.unlock()?;
        result
    }
    fn ensure_files(&self) -> std::io::Result<()> {
        Self::file_if_absent(&self.global(), b"", 0o644)?;
        Self::ensure_log(&self.log())
    }
    fn mkdir(path: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(path)?;
        set_mode(path, 0o755)
    }
    fn file_if_absent(path: &Path, contents: &[u8], mode: u32) -> std::io::Result<()> {
        if path.exists() {
            set_mode(path, mode)?;
        } else {
            write_atomic(path, contents, mode)?;
        }
        Ok(())
    }
    fn ensure_lock(path: &Path) -> std::io::Result<()> {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(file) => {
                file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let metadata = std::fs::symlink_metadata(path)?;
                if !metadata.file_type().is_file() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "policy lock must be a regular file",
                    ));
                }
                set_mode(path, 0o600)
            }
            Err(error) => Err(error),
        }
    }
    fn ensure_log(path: &Path) -> std::io::Result<()> {
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() => Ok(()),
            Ok(metadata) if metadata.file_type().is_file() => set_mode(path, 0o622),
            Ok(_) => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "denied log must be a regular file",
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                write_atomic(path, b"", 0o622)
            }
            Err(error) => Err(error),
        }
    }
    fn locked<T>(&self, f: impl FnOnce() -> std::io::Result<T>) -> std::io::Result<T> {
        use std::fs::OpenOptions;
        self.ensure()?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(self.lock_path())?;
        file.lock()?;
        let result = f();
        file.unlock()?;
        result
    }
    fn project_file_locked(&self, project: &ProjectIdentity) -> std::io::Result<PathBuf> {
        let dir = self.projects().join(&project.key);
        Self::mkdir(&dir)?;
        let metadata = dir.join("path");
        if metadata.exists() {
            if std::fs::read(&metadata)? != project.bytes {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "project key collision",
                ));
            }
        } else {
            write_atomic(&metadata, &project.bytes, 0o600)?;
        }
        set_mode(&metadata, 0o600)?;
        let allow = dir.join("allow.local");
        Self::file_if_absent(&allow, b"", 0o644)?;
        Ok(allow)
    }
    pub(crate) fn mutate_global(&self, add: &[String], remove: &[String]) -> std::io::Result<()> {
        let (add, remove) = normalize_batch(add, remove)?;
        self.locked(|| Self::mutate_file_locked(&self.global(), &add, &remove))
    }
    pub(crate) fn mutate_project(
        &self,
        project: &ProjectIdentity,
        add: &[String],
        remove: &[String],
    ) -> std::io::Result<()> {
        let (add, remove) = normalize_batch(add, remove)?;
        self.locked(|| {
            let file = self.project_file_locked(project)?;
            Self::mutate_file_locked(&file, &add, &remove)
        })
    }

    /// Remove a whole batch only when every requested domain is present.  Provenance
    /// is computed from the same locked state as the replacement, so a report cannot
    /// describe a policy other than the one we actually changed.
    pub(crate) fn deny(
        &self,
        project: Option<&ProjectIdentity>,
        domains: &[String],
    ) -> std::io::Result<DenyReport> {
        let domains = normalize_domains(domains)?;
        self.locked(|| {
            let snapshot = self.snapshot_locked()?;
            let (selected, selected_domains, selected_label) = match project {
                None => {
                    let selected = self.global();
                    let domains = read_domains(&selected)?;
                    (Some(selected), domains, "global".to_owned())
                }
                Some(project) => {
                    let stored = snapshot
                        .projects
                        .iter()
                        .find(|entry| entry.key == project.key && entry.path == project.bytes);
                    let selected =
                        stored.map(|entry| self.projects().join(&entry.key).join("allow.local"));
                    let domains = match (&selected, stored) {
                        (Some(file), Some(_)) => read_domains(file)?,
                        (None, None) => Vec::new(),
                        _ => unreachable!(),
                    };
                    (
                        selected,
                        domains,
                        format!("project:{}", quote_path_bytes(&project.bytes)),
                    )
                }
            };
            let missing: Vec<_> = domains
                .iter()
                .filter(|d| !selected_domains.contains(*d))
                .cloned()
                .collect();
            if !missing.is_empty() {
                return Ok(DenyReport {
                    selected: selected_label.clone(),
                    missing: missing
                        .into_iter()
                        .map(|domain| {
                            let sources =
                                other_sources(&snapshot, project, &domain, &selected_label);
                            DenyDomainReport { domain, sources }
                        })
                        .collect(),
                    remaining: Vec::new(),
                });
            }
            let selected = selected.expect("a nonempty selected policy must have a file");
            let remove: HashSet<_> = domains.iter().cloned().collect();
            let mut retained = selected_domains;
            retained.retain(|domain| !remove.contains(domain));
            write_atomic(&selected, domains_text(&retained).as_bytes(), 0o644)?;
            let mut after = snapshot;
            match project {
                None => after.global = retained,
                Some(project) => {
                    after
                        .projects
                        .iter_mut()
                        .find(|p| p.key == project.key)
                        .expect("selected project is in snapshot")
                        .domains = retained;
                }
            }
            Ok(DenyReport {
                selected: selected_label,
                missing: Vec::new(),
                remaining: domains
                    .into_iter()
                    .map(|domain| {
                        let sources = other_sources(&after, project, &domain, "");
                        DenyDomainReport { domain, sources }
                    })
                    .collect(),
            })
        })
    }
    fn mutate_file_locked(
        file: &Path,
        add: &[String],
        remove: &HashSet<String>,
    ) -> std::io::Result<()> {
        let mut domains = read_domains(file)?;
        for value in add {
            let value = value.clone();
            if !domains.contains(&value) {
                domains.push(value);
            }
        }
        let missing: Vec<_> = remove
            .iter()
            .filter(|d| !domains.contains(*d))
            .cloned()
            .collect();
        if !missing.is_empty() {
            return Err(invalid_input(format!(
                "domains not allowed: {}",
                missing.join(", ")
            )));
        }
        domains.retain(|d| !remove.contains(d));
        write_atomic(file, domains_text(&domains).as_bytes(), 0o644)
    }
    pub(crate) fn resolved(
        &self,
        harness: &str,
        harness_domains: &[String],
        project: Option<&ProjectIdentity>,
        run_id: &str,
        run: &[String],
    ) -> std::io::Result<Vec<ResolvedDomain>> {
        self.ensure()?;
        let mut output: Vec<ResolvedDomain> = Vec::new();
        let mut index = std::collections::HashMap::<String, usize>::new();
        let mut layer = |domains: Vec<String>, source: LayerSource| -> std::io::Result<()> {
            for d in domains {
                let d = normalize_domain(&d).map_err(invalid_input)?;
                if let Some(i) = index.get(&d) {
                    output[*i].sources.push(source.clone());
                } else {
                    index.insert(d.clone(), output.len());
                    output.push(ResolvedDomain {
                        domain: d,
                        sources: vec![source.clone()],
                    });
                }
            }
            Ok(())
        };
        layer(
            BASE_ALLOWLIST.iter().map(|s| (*s).to_string()).collect(),
            LayerSource::Base,
        )?;
        layer(
            harness_domains.to_vec(),
            LayerSource::Harness(harness.to_string()),
        )?;
        layer(read_domains(&self.global())?, LayerSource::Global)?;
        if let Some(project) = project {
            let file = self.projects().join(&project.key).join("allow.local");
            if file.exists() {
                layer(
                    read_domains(&file)?,
                    LayerSource::Project(project.path.clone()),
                )?;
            }
        }
        layer(run.to_vec(), LayerSource::Run(run_id.to_string()))?;
        Ok(output)
    }
    pub(crate) fn denied_domains(&self) -> std::io::Result<Vec<String>> {
        self.ensure()?;
        if !std::fs::symlink_metadata(self.log())?.file_type().is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "denied log must be a regular file",
            ));
        }
        let mut v: Vec<_> = read_denied(&self.log())?.into_iter().collect();
        v.sort();
        Ok(v)
    }

    /// Read a coherent, validated view for status and provenance reporting.  In
    /// particular, do not expose a run list which is then resolved after releasing
    /// the lock: a concurrently retired run would make status internally inconsistent.
    pub(crate) fn snapshot(&self) -> std::io::Result<PolicySnapshot> {
        self.locked(|| self.snapshot_locked())
    }

    fn snapshot_locked(&self) -> std::io::Result<PolicySnapshot> {
        self.reap_runs_locked()?;
        let global = read_domains(&self.global())?;
        let projects = self.projects_locked()?;
        let mut runs = Vec::new();
        for entry in std::fs::read_dir(self.runs())? {
            let entry = entry?;
            let name = entry.file_name();
            if name.to_string_lossy().starts_with('.') {
                continue;
            }
            if !entry.file_type()?.is_dir() {
                return Err(invalid_data("published run state is not a directory"));
            }
            runs.push(Self::active_run_locked(
                &name.to_string_lossy(),
                &entry.path(),
                &global,
                &projects,
            )?);
        }
        runs.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(PolicySnapshot {
            global,
            projects,
            runs,
        })
    }

    fn projects_locked(&self) -> std::io::Result<Vec<PolicyProject>> {
        let mut projects = Vec::new();
        for entry in std::fs::read_dir(self.projects())? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                return Err(invalid_data("project state is not a directory"));
            }
            let key = entry.file_name().to_string_lossy().into_owned();
            let path = std::fs::read(entry.path().join("path"))?;
            if key_for_bytes(&path) != key {
                return Err(invalid_data("project key does not match project path"));
            }
            projects.push(PolicyProject {
                key,
                path,
                domains: read_domains(&entry.path().join("allow.local"))?,
            });
        }
        projects.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(projects)
    }

    fn active_run_locked(
        id: &str,
        path: &Path,
        global: &[String],
        projects: &[PolicyProject],
    ) -> std::io::Result<ActiveRun> {
        let harness = read_trimmed(&path.join("harness"), "run harness")?;
        let mode = read_trimmed(&path.join("mode"), "run mode")?;
        if Mode::from_str(&mode).is_none() {
            return Err(invalid_data("invalid run mode"));
        }
        let project_key = read_trimmed(&path.join("project-key"), "run project key")?;
        let project_path = std::fs::read(path.join("project-path"))?;
        if key_for_bytes(&project_path) != project_key {
            return Err(invalid_data("run project key does not match project path"));
        }
        let project = projects
            .iter()
            .find(|p| p.key == project_key)
            .ok_or_else(|| invalid_data("run project policy is missing"))?;
        if project.path != project_path {
            return Err(invalid_data(
                "run project path does not match project policy",
            ));
        }
        let base = read_domains(&path.join("base.allow"))?;
        let harness_domains = read_domains(&path.join("harness.allow"))?;
        let run = read_domains(&path.join("run.allow"))?;
        let effective = resolve_layers([
            (base, LayerSource::Base),
            (harness_domains, LayerSource::Harness(harness.clone())),
            (global.to_vec(), LayerSource::Global),
            (
                project.domains.clone(),
                LayerSource::Project(path_from_bytes(&project_path)),
            ),
            (run, LayerSource::Run(id.to_owned())),
        ])?;
        Ok(ActiveRun {
            id: id.to_owned(),
            harness,
            project_key,
            project_path,
            mode,
            effective,
        })
    }

    /// Active runs sorted by their opaque, lexical id. Project bytes are retained for
    /// callers that need deterministic byte ordering rather than lossy path text.
    pub(crate) fn active_runs(&self) -> std::io::Result<Vec<ActiveRun>> {
        Ok(self.snapshot()?.runs)
    }

    /// Change every currently published run under the policy lock. A run disappearing
    /// after its mode file is discovered is a normal lifecycle race.
    pub(crate) fn set_active_mode(&self, mode: Mode) -> std::io::Result<usize> {
        self.locked(|| {
            self.reap_runs_locked()?;
            let mut paths = Vec::new();
            for entry in std::fs::read_dir(self.runs())? {
                let entry = entry?;
                if !entry.file_name().to_string_lossy().starts_with('.') {
                    paths.push(entry.path().join("mode"));
                }
            }
            paths.sort();
            let mut valid = Vec::new();
            for path in paths {
                let metadata = match std::fs::symlink_metadata(&path) {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error),
                };
                if !metadata.file_type().is_file() || metadata.permissions().mode() & 0o777 != 0o644
                {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "run mode must be a regular 0644 file",
                    ));
                }
                valid.push(path);
            }
            let mut changed = 0;
            for path in valid {
                match write_atomic(&path, format!("{}\n", mode.as_str()).as_bytes(), 0o644) {
                    Ok(()) => changed += 1,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            }
            Ok(changed)
        })
    }

    fn reset_log_if_idle_locked(&self) -> std::io::Result<()> {
        let active = std::fs::read_dir(self.runs())?.try_fold(false, |active, entry| {
            let entry = entry?;
            Ok::<_, std::io::Error>(active || !entry.file_name().to_string_lossy().starts_with('.'))
        })?;
        if !active {
            write_atomic(&self.log(), b"", 0o622)?;
        }
        Ok(())
    }

    /// Publish immutable run snapshots before any later fallible launch operation.
    pub(crate) fn publish_run(
        &self,
        harness: &str,
        project: &ProjectIdentity,
        harness_domains: &[String],
        run_domains: &[String],
        mode: Mode,
    ) -> std::io::Result<PolicyRun> {
        let harness_domains = normalize_domains(harness_domains)?;
        let run_domains = normalize_domains(run_domains)?;
        self.locked(|| {
            self.reap_runs_locked()?;
            self.project_file_locked(project)?;
            self.reset_log_if_idle_locked()?;
            let id = format!(
                "{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(std::io::Error::other)?
                    .as_nanos()
            );
            let temporary = self.runs().join(format!(".{id}.tmp"));
            std::fs::create_dir(&temporary)?;
            set_mode(&temporary, 0o755)?;
            let build = || -> std::io::Result<PolicyRun> {
                let lease = temporary.join("lease");
                write_atomic(&lease, b"", 0o600)?;
                let lease_file = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&lease)?;
                lease_file.lock()?;
                write_atomic(&temporary.join("harness"), harness.as_bytes(), 0o600)?;
                write_atomic(
                    &temporary.join("project-key"),
                    project.key.as_bytes(),
                    0o600,
                )?;
                write_atomic(&temporary.join("project-path"), &project.bytes, 0o600)?;
                write_domains(
                    &temporary.join("base.allow"),
                    &BASE_ALLOWLIST
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>(),
                )?;
                write_domains(&temporary.join("harness.allow"), &harness_domains)?;
                write_domains(&temporary.join("run.allow"), &run_domains)?;
                write_atomic(
                    &temporary.join("mode"),
                    format!("{}\n", mode.as_str()).as_bytes(),
                    0o644,
                )?;
                let published = self.runs().join(&id);
                std::fs::rename(&temporary, &published)?;
                Ok(PolicyRun {
                    inner: Arc::new(PolicyRunInner {
                        published,
                        dead: self.runs().join(format!(".{id}.dead")),
                        retired: AtomicBool::new(false),
                        _lease: lease_file,
                    }),
                })
            };
            match build() {
                Ok(run) => Ok(run),
                Err(error) => {
                    let _ = std::fs::remove_dir_all(&temporary);
                    Err(error)
                }
            }
        })
    }

    fn reap_runs_locked(&self) -> std::io::Result<()> {
        for entry in std::fs::read_dir(self.runs())? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let path = entry.path();
            if name.starts_with('.') {
                if !entry.file_type()?.is_dir() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "temporary run state is not a directory",
                    ));
                }
                std::fs::remove_dir_all(path)?;
                continue;
            }
            if !entry.file_type()?.is_dir() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "published run state is not a directory",
                ));
            }
            let lease = path.join("lease");
            let meta = std::fs::symlink_metadata(&lease)?;
            if !meta.file_type().is_file() || meta.permissions().mode() & 0o777 != 0o600 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "published run lease must be a regular 0600 file",
                ));
            }
            let Ok(file) = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&lease)
            else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "could not open published run lease",
                ));
            };
            match file.try_lock() {
                Ok(()) => {
                    file.unlock()?;
                    std::fs::remove_dir_all(path)?;
                }
                Err(std::fs::TryLockError::WouldBlock) => {}
                Err(std::fs::TryLockError::Error(error)) => return Err(error),
            }
        }
        Ok(())
    }
}

struct PolicyRunInner {
    published: PathBuf,
    dead: PathBuf,
    retired: AtomicBool,
    // Keeping the descriptor locked is the liveness lease used by reaping.
    _lease: std::fs::File,
}

/// Idempotent, lock-free run retirement. It is safe from Drop and signal cleanup.
pub(crate) struct PolicyRun {
    inner: Arc<PolicyRunInner>,
}
impl PolicyRun {
    #[allow(dead_code)] // Consumed by run wiring in the next ship step.
    pub(crate) fn id(&self) -> Option<&std::ffi::OsStr> {
        self.inner.published.file_name()
    }
    pub(crate) fn retire(&self) -> std::io::Result<()> {
        retire_inner(&self.inner)
    }
    #[allow(dead_code)] // Exposed to signal cleanup before the run path owns it.
    pub(crate) fn cleanup_handle(&self) -> PolicyCleanup {
        PolicyCleanup {
            inner: Arc::clone(&self.inner),
        }
    }
}
impl Drop for PolicyRun {
    fn drop(&mut self) {
        let _ = self.retire();
    }
}

/// Cloneable signal-path cleanup action. Unlike `PolicyRun`, it has no Drop.
#[allow(dead_code)] // The signal thread is added by run-path integration.
#[derive(Clone)]
pub(crate) struct PolicyCleanup {
    inner: Arc<PolicyRunInner>,
}
impl PolicyCleanup {
    #[allow(dead_code)]
    pub(crate) fn retire(&self) -> std::io::Result<()> {
        retire_inner(&self.inner)
    }
}

fn retire_inner(inner: &PolicyRunInner) -> std::io::Result<()> {
    if inner.retired.load(Ordering::Acquire) {
        return Ok(());
    }
    match std::fs::rename(&inner.published, &inner.dead) {
        Ok(()) => {
            inner.retired.store(true, Ordering::Release);
            std::fs::remove_dir_all(&inner.dead)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            inner.retired.store(true, Ordering::Release);
            Ok(())
        }
        Err(error) => Err(error),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ActiveRun {
    pub(crate) id: String,
    pub(crate) harness: String,
    pub(crate) project_key: String,
    pub(crate) project_path: Vec<u8>,
    pub(crate) mode: String,
    pub(crate) effective: Vec<ResolvedDomain>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PolicyProject {
    pub(crate) key: String,
    pub(crate) path: Vec<u8>,
    pub(crate) domains: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PolicySnapshot {
    pub(crate) global: Vec<String>,
    pub(crate) projects: Vec<PolicyProject>,
    pub(crate) runs: Vec<ActiveRun>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DenyDomainReport {
    pub(crate) domain: String,
    pub(crate) sources: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DenyReport {
    pub(crate) selected: String,
    pub(crate) missing: Vec<DenyDomainReport>,
    pub(crate) remaining: Vec<DenyDomainReport>,
}

fn other_sources(
    snapshot: &PolicySnapshot,
    selected_project: Option<&ProjectIdentity>,
    domain: &str,
    selected_label: &str,
) -> Vec<String> {
    let mut sources = Vec::new();
    let mut add = |source: String| {
        if source != selected_label && !sources.contains(&source) {
            sources.push(source);
        }
    };
    if BASE_ALLOWLIST.contains(&domain) {
        add("base".into());
    }
    for name in crate::harness::harness_names() {
        if crate::harness::lookup_harness(&name)
            .is_some_and(|harness| harness.allow_domains.iter().any(|value| value == domain))
        {
            add(format!("harness:{name}"));
        }
    }
    if snapshot.global.iter().any(|value| value == domain) {
        add("global".into());
    }
    if selected_project.is_none() {
        for project in &snapshot.projects {
            if project.domains.iter().any(|value| value == domain) {
                add(format!("project:{}", quote_path_bytes(&project.path)));
            }
        }
    }
    for run in &snapshot.runs {
        if selected_project.is_some_and(|selected| selected.key != run.project_key) {
            continue;
        }
        if let Some(resolved) = run.effective.iter().find(|value| value.domain == domain) {
            for source in &resolved.sources {
                if let LayerSource::Run(id) = source {
                    add(format!("run:{id}"));
                }
            }
        }
    }
    sources
}

fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    PathBuf::from(std::ffi::OsString::from_vec(bytes.to_vec()))
}

fn key_for_bytes(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

fn invalid_data(message: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message)
}

fn resolve_layers<const N: usize>(
    layers: [(Vec<String>, LayerSource); N],
) -> std::io::Result<Vec<ResolvedDomain>> {
    let mut output: Vec<ResolvedDomain> = Vec::new();
    let mut index = std::collections::HashMap::<String, usize>::new();
    for (domains, source) in layers {
        for domain in domains {
            let domain = normalize_domain(&domain).map_err(invalid_input)?;
            if let Some(index) = index.get(&domain) {
                output[*index].sources.push(source.clone());
            } else {
                index.insert(domain.clone(), output.len());
                output.push(ResolvedDomain {
                    domain,
                    sources: vec![source.clone()],
                });
            }
        }
    }
    Ok(output)
}

fn invalid_input(error: String) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, error)
}
fn normalize_batch(
    add: &[String],
    remove: &[String],
) -> std::io::Result<(Vec<String>, HashSet<String>)> {
    let add = add
        .iter()
        .map(|value| normalize_domain(value).map_err(invalid_input))
        .collect::<std::io::Result<Vec<_>>>()?;
    let remove = remove
        .iter()
        .map(|value| normalize_domain(value).map_err(invalid_input))
        .collect::<std::io::Result<HashSet<_>>>()?;
    Ok((add, remove))
}
fn read_domains(path: &Path) -> std::io::Result<Vec<String>> {
    normalize_domains(
        &std::fs::read_to_string(path)?
            .lines()
            .map(str::to_owned)
            .collect::<Vec<_>>(),
    )
}
fn normalize_domains(domains: &[String]) -> std::io::Result<Vec<String>> {
    let mut result = Vec::new();
    for domain in domains {
        let domain = normalize_domain(domain).map_err(invalid_input)?;
        if !result.contains(&domain) {
            result.push(domain);
        }
    }
    Ok(result)
}
fn read_denied(path: &Path) -> std::io::Result<std::collections::HashSet<String>> {
    Ok(std::fs::read_to_string(path)?
        .lines()
        .filter_map(|l| l.split_whitespace().nth(1).map(str::to_owned))
        .collect())
}
fn read_trimmed(path: &Path, label: &str) -> std::io::Result<String> {
    let value = std::fs::read_to_string(path)?;
    let value = value.trim();
    if value.is_empty() || value.contains(['\n', '\r']) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid {label}"),
        ));
    }
    Ok(value.to_owned())
}
fn domains_text(domains: &[String]) -> String {
    let mut text = String::new();
    for domain in domains {
        text.push_str(domain);
        text.push('\n');
    }
    text
}
fn write_domains(path: &Path, domains: &[String]) -> std::io::Result<()> {
    let normalized = normalize_domains(domains)?;
    write_atomic(path, domains_text(&normalized).as_bytes(), 0o644)
}

/// Atomically replace a state file without ever following a pre-existing temporary.
#[allow(dead_code)]
pub(crate) fn write_atomic(path: &Path, contents: &[u8], mode: u32) -> std::io::Result<()> {
    use std::io::Write;
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "state file has no parent")
    })?;
    for attempt in 0..100 {
        let temp = parent.join(format!(
            ".{}.{}.{}",
            path.file_name().unwrap_or_default().to_string_lossy(),
            std::process::id(),
            next_tmp_id() + attempt
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
        {
            Ok(mut file) => {
                let result = (|| {
                    file.write_all(contents)?;
                    file.sync_all()?;
                    set_mode(&temp, mode)?;
                    std::fs::rename(&temp, path)
                })();
                if result.is_err() {
                    let _ = std::fs::remove_file(&temp);
                }
                return result;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not allocate atomic temporary",
    ))
}

/// Handle `vhrn net <subcommand>`: mutate the host-side egress policy the running container
/// reads. This is the only path to that policy — the container has none.
pub(crate) fn run_net(args: &[String]) -> i32 {
    let home = match crate::run::home_dir() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("vhrn: {e}");
            return 1;
        }
    };
    let result = run_net_with_store(&PolicyStore::new(&crate::run::vhrn_state(&home)), args);
    print!("{}", result.stdout);
    eprint!("{}", result.stderr);
    result.code
}

#[derive(Debug, PartialEq, Eq)]
struct CommandResult {
    code: i32,
    stdout: String,
    stderr: String,
}

fn ok(stdout: String) -> CommandResult {
    CommandResult {
        code: 0,
        stdout,
        stderr: String::new(),
    }
}
fn usage(message: &str) -> CommandResult {
    CommandResult {
        code: 2,
        stdout: String::new(),
        stderr: format!("{message}\n"),
    }
}
fn failure(error: impl std::fmt::Display) -> CommandResult {
    CommandResult {
        code: 1,
        stdout: String::new(),
        stderr: format!("{error}\n"),
    }
}

#[allow(
    clippy::too_many_lines,
    clippy::format_push_string,
    clippy::format_collect
)]
fn run_net_with_store(store: &PolicyStore, args: &[String]) -> CommandResult {
    let (cmd, rest): (&str, &[String]) = match args.split_first() {
        Some((c, r)) => (c, r),
        None => ("status", &[]),
    };

    match cmd {
        "status" if !rest.is_empty() && rest != ["--domains"] => {
            usage("usage: vhrn net status [--domains]")
        }
        "status" => match store.snapshot() {
            Ok(snapshot) => {
                let mut out = format!(
                    "global: {} domain(s)\nprojects: {} project(s), {} domain(s)\n",
                    snapshot.global.len(),
                    snapshot.projects.len(),
                    snapshot
                        .projects
                        .iter()
                        .map(|p| p.domains.len())
                        .sum::<usize>(),
                );
                for run in &snapshot.runs {
                    out.push_str(&format!(
                        "run {}: harness={} project={} mode={} effective={}\n",
                        run.id,
                        run.harness,
                        quote_path_bytes(&run.project_path),
                        run.mode,
                        run.effective.len(),
                    ));
                }
                if snapshot.runs.is_empty() {
                    out.push_str("no active runs; future runs default to enforce\n");
                }
                if rest == ["--domains"] {
                    out.push_str("global domains:\n");
                    if snapshot.global.is_empty() {
                        out.push_str("  (none)\n");
                    } else {
                        for domain in &snapshot.global {
                            out.push_str(&format!("  {domain}\n"));
                        }
                    }
                    for project in &snapshot.projects {
                        out.push_str(&format!("project {}:\n", quote_path_bytes(&project.path)));
                        if project.domains.is_empty() {
                            out.push_str("  (none)\n");
                        } else {
                            for domain in &project.domains {
                                out.push_str(&format!("  {domain}\n"));
                            }
                        }
                    }
                    for run in &snapshot.runs {
                        out.push_str(&format!("run {} domains:\n", run.id));
                        if run.effective.is_empty() {
                            out.push_str("  (none)\n");
                        } else {
                            for domain in &run.effective {
                                let sources = domain
                                    .sources
                                    .iter()
                                    .map(source_label)
                                    .collect::<Vec<_>>()
                                    .join(", ");
                                out.push_str(&format!("  {} [{}]\n", domain.domain, sources));
                            }
                        }
                    }
                }
                ok(out)
            }
            Err(e) => failure(e),
        },
        "denied" if !rest.is_empty() => usage("usage: vhrn net denied"),
        "denied" => {
            let domains = match store.denied_domains() {
                Ok(v) => v,
                Err(e) => {
                    return failure(e);
                }
            };
            if domains.is_empty() {
                return ok("no denials recorded this session\n".into());
            }
            ok(domains.into_iter().map(|d| format!("{d}\n")).collect())
        }
        "allow" | "deny" => {
            let (project_path, domains) = match parse_scope(rest) {
                Ok(v) => v,
                Err(e) => {
                    return usage(&e);
                }
            };
            let domains = match normalize_domains(&domains) {
                Ok(v) => v,
                Err(e) => {
                    return failure(e);
                }
            };
            let project = match project_path {
                Some(path) => match ProjectIdentity::from_path(Path::new(&path)) {
                    Ok(project) => Some(project),
                    Err(error) => return failure(error),
                },
                None => None,
            };
            if cmd == "deny" {
                return match store.deny(project.as_ref(), &domains) {
                    Ok(report) if !report.missing.is_empty() => {
                        failure(format_deny_missing(&report))
                    }
                    Ok(report) => ok(format_deny_success(&report)),
                    Err(error) => failure(error),
                };
            }
            let result = match (cmd, project.as_ref()) {
                ("allow", None) => store.mutate_global(&domains, &[]),
                ("allow", Some(p)) => store.mutate_project(p, &domains, &[]),
                _ => unreachable!(),
            };
            if let Err(e) = result {
                return failure(e);
            }
            ok(format!("{}: {}\n", cmd, domains.join(" ")))
        }
        "open" => {
            if !rest.is_empty() {
                return usage("usage: vhrn net open");
            }
            mode_result(store, Mode::Open)
        }
        "guard" => {
            if !rest.is_empty() {
                return usage("usage: vhrn net guard");
            }
            mode_result(store, Mode::Enforce)
        }
        "report" => {
            if !rest.is_empty() {
                return usage("usage: vhrn net report");
            }
            mode_result(store, Mode::Report)
        }
        _ => usage("usage: vhrn net {status|denied|allow|deny|open|guard|report}"),
    }
}

fn format_deny_missing(report: &DenyReport) -> String {
    report
        .missing
        .iter()
        .map(|entry| {
            if entry.sources.is_empty() {
                format!("{} is not allowed by {}", entry.domain, report.selected)
            } else {
                format!(
                    "{} is not allowed by {}; allowed by {}",
                    entry.domain,
                    report.selected,
                    entry.sources.join(", ")
                )
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_deny_success(report: &DenyReport) -> String {
    use std::fmt::Write;
    let mut output = format!(
        "deny: {}\n",
        report
            .remaining
            .iter()
            .map(|entry| entry.domain.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    );
    for entry in &report.remaining {
        if !entry.sources.is_empty() {
            let _ = writeln!(
                output,
                "{} remains allowed by {}",
                entry.domain,
                entry.sources.join(", ")
            );
        }
    }
    output
}

fn parse_scope(args: &[String]) -> std::result::Result<(Option<String>, Vec<String>), String> {
    let (project, domains) = if args.first().is_some_and(|a| a == "--project") {
        let Some(path) = args.get(1) else {
            return Err("usage: vhrn net allow|deny [--project <path>] <domain>...".into());
        };
        (Some(path.clone()), args[2..].to_vec())
    } else {
        (None, args.to_vec())
    };
    if domains.is_empty() || domains.iter().any(|domain| domain.starts_with('-')) {
        return Err("usage: vhrn net allow|deny [--project <path>] <domain>...".into());
    }
    Ok((project, domains))
}

fn mode_result(store: &PolicyStore, mode: Mode) -> CommandResult {
    match store.set_active_mode(mode) {
        Ok(0) => ok("no active runs; future runs default to enforce\n".into()),
        Ok(count) => ok(format!(
            "updated {count} active run(s) to {}\n",
            mode.as_str()
        )),
        Err(e) => failure(e),
    }
}

fn source_label(source: &LayerSource) -> String {
    match source {
        LayerSource::Base => "base".into(),
        LayerSource::Harness(name) => format!("harness:{name}"),
        LayerSource::Global => "global".into(),
        LayerSource::Project(path) => {
            use std::os::unix::ffi::OsStrExt;
            format!("project:{}", quote_path_bytes(path.as_os_str().as_bytes()))
        }
        LayerSource::Run(id) => format!("run:{id}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_roundtrips() {
        for m in [Mode::Enforce, Mode::Report, Mode::Open] {
            assert_eq!(Mode::from_str(m.as_str()), Some(m));
        }
        assert_eq!(Mode::from_str("nope"), None);
    }

    #[test]
    fn domain_normalization_and_idna_guidance() {
        assert_eq!(normalize_domain(" *.Example.COM. ").unwrap(), "example.com");
        assert!(normalize_domain("https://example.com").is_err());
        assert!(normalize_domain("example..com").is_err());
        assert!(normalize_domain("foo*bar.com").is_err());
        assert!(
            normalize_domain("bücher.example")
                .unwrap_err()
                .contains("xn--bcher-kva.example")
        );
    }

    #[test]
    fn injected_net_edge_validates_before_creating_state_and_formats_idle() {
        let state = crate::testutil::temp_dir();
        let store = PolicyStore::new(state.path());
        let bad = run_net_with_store(&store, &["status".into(), "nope".into()]);
        assert_eq!(bad.code, 2);
        assert!(!store.root().exists());
        let invalid = run_net_with_store(&store, &["allow".into(), "https://bad".into()]);
        assert_eq!(invalid.code, 1);
        assert!(!store.root().exists());
        let mode = run_net_with_store(&store, &["open".into()]);
        assert_eq!(
            mode.stdout,
            "no active runs; future runs default to enforce\n"
        );
        let absent = state.path().join("does-not-exist").display().to_string();
        let missing = run_net_with_store(
            &store,
            &[
                "allow".into(),
                "--project".into(),
                absent,
                "ok.example".into(),
            ],
        );
        assert_eq!(missing.code, 1);
    }

    #[test]
    fn status_idle_output_is_exact() {
        let state = crate::testutil::temp_dir();
        let store = PolicyStore::new(state.path());

        let status = run_net_with_store(&store, &["status".into()]);

        assert_eq!(status.code, 0);
        assert_eq!(
            status.stdout,
            "global: 0 domain(s)\nprojects: 0 project(s), 0 domain(s)\n".to_owned()
                + "no active runs; future runs default to enforce\n"
        );
        assert!(status.stderr.is_empty());
    }

    #[test]
    fn denied_initializes_empty_log_sorts_entries_and_rejects_extras_without_state() {
        let fresh_state = crate::testutil::temp_dir();
        let fresh = PolicyStore::new(fresh_state.path());
        let extra = run_net_with_store(&fresh, &["denied".into(), "extra".into()]);
        assert_eq!(extra.code, 2);
        assert!(extra.stdout.is_empty());
        assert!(!fresh.root().exists());

        let state = crate::testutil::temp_dir();
        let store = PolicyStore::new(state.path());
        let empty = run_net_with_store(&store, &["denied".into()]);
        assert_eq!(empty.code, 0);
        assert_eq!(empty.stdout, "no denials recorded this session\n");
        assert!(empty.stderr.is_empty());

        std::fs::write(
            store.log(),
            "one tracker.io GET\ntwo evil.example GET\nthree tracker.io POST\n",
        )
        .unwrap();
        let listed = run_net_with_store(&store, &["denied".into()]);
        assert_eq!(listed.code, 0);
        assert_eq!(listed.stdout, "evil.example\ntracker.io\n");
        assert!(listed.stderr.is_empty());
    }

    #[test]
    fn denied_rejects_malformed_or_symlink_log() {
        let state = crate::testutil::temp_dir();
        let store = PolicyStore::new(state.path());
        store.ensure().unwrap();
        std::fs::remove_file(store.log()).unwrap();
        std::fs::create_dir(store.log()).unwrap();
        let malformed = run_net_with_store(&store, &["denied".into()]);
        assert_eq!(malformed.code, 1);
        assert!(malformed.stdout.is_empty());

        std::fs::remove_dir(store.log()).unwrap();
        std::os::unix::fs::symlink("elsewhere", store.log()).unwrap();
        let symlink = run_net_with_store(&store, &["denied".into()]);
        assert_eq!(symlink.code, 1);
        assert!(symlink.stdout.is_empty());
    }

    #[test]
    fn scoped_store_isolated_and_preserves_provenance() {
        let state = crate::testutil::temp_dir();
        let project_dir = crate::testutil::temp_dir();
        let project = ProjectIdentity::from_path(project_dir.path()).unwrap();
        let store = PolicyStore::new(state.path());
        store
            .mutate_global(&["GLOBAL.example".into()], &[])
            .unwrap();
        store
            .mutate_project(
                &project,
                &["project.example".into(), "github.com".into()],
                &[],
            )
            .unwrap();
        let resolved = store
            .resolved(
                "codex",
                &["github.com".into(), "vendor.example".into()],
                Some(&project),
                "test-run",
                &["run.example".into()],
            )
            .unwrap();
        assert_eq!(resolved.len(), 10);
        assert_eq!(resolved[0].domain, "github.com");
        assert_eq!(
            resolved[0].sources,
            vec![
                LayerSource::Base,
                LayerSource::Harness("codex".into()),
                LayerSource::Project(project.path.clone())
            ]
        );
        assert!(
            resolved
                .iter()
                .any(|domain| domain.domain == "global.example")
        );
        store
            .mutate_project(&project, &[], &["project.example".into()])
            .unwrap();
        assert!(
            !store
                .resolved("codex", &[], Some(&project), "test-run", &[])
                .unwrap()
                .iter()
                .any(|domain| domain.domain == "project.example")
        );
    }

    #[test]
    fn deny_is_atomic_and_reports_other_layers() {
        let state = crate::testutil::temp_dir();
        let project_dir = crate::testutil::temp_dir();
        let project = ProjectIdentity::from_path(project_dir.path()).unwrap();
        let store = PolicyStore::new(state.path());
        store
            .mutate_global(&["github.com".into(), "global.example".into()], &[])
            .unwrap();
        store
            .mutate_project(
                &project,
                &["github.com".into(), "local.example".into()],
                &[],
            )
            .unwrap();
        let file = store.projects().join(&project.key).join("allow.local");
        let original = std::fs::read(&file).unwrap();
        let missing = store
            .deny(
                Some(&project),
                &["github.com".into(), "missing.example".into()],
            )
            .unwrap();
        assert_eq!(missing.missing.len(), 1);
        assert!(missing.missing[0].sources.is_empty());
        assert_eq!(std::fs::read(&file).unwrap(), original);
        let removed = store.deny(Some(&project), &["github.com".into()]).unwrap();
        assert!(removed.missing.is_empty());
        assert!(
            removed.remaining[0]
                .sources
                .iter()
                .any(|source| source == "global")
        );
        assert!(
            !read_domains(&file)
                .unwrap()
                .contains(&"github.com".to_owned())
        );
    }

    #[test]
    fn project_deny_ignores_unrelated_project_and_run_provenance() {
        let state = crate::testutil::temp_dir();
        let projects = crate::testutil::temp_dir();
        let first_path = projects.path().join("a");
        let second_path = projects.path().join("b");
        std::fs::create_dir(&first_path).unwrap();
        std::fs::create_dir(&second_path).unwrap();
        let first = ProjectIdentity::from_path(&first_path).unwrap();
        let second = ProjectIdentity::from_path(&second_path).unwrap();
        let store = PolicyStore::new(state.path());
        store.mutate_global(&["github.com".into()], &[]).unwrap();
        store
            .mutate_project(&first, &["github.com".into()], &[])
            .unwrap();
        store
            .mutate_project(&second, &["github.com".into()], &[])
            .unwrap();
        let first_run = store
            .publish_run("codex", &first, &[], &["github.com".into()], Mode::Enforce)
            .unwrap();
        let second_run = store
            .publish_run("codex", &second, &[], &["github.com".into()], Mode::Enforce)
            .unwrap();

        let report = store.deny(Some(&first), &["github.com".into()]).unwrap();
        let expected_harnesses = crate::harness::harness_names()
            .into_iter()
            .filter(|name| {
                crate::harness::lookup_harness(name)
                    .is_some_and(|harness| harness.allow_domains.iter().any(|d| d == "github.com"))
            })
            .map(|name| format!("harness:{name}"));
        let expected = std::iter::once("base".to_owned())
            .chain(expected_harnesses)
            .chain(std::iter::once("global".to_owned()))
            .chain(std::iter::once(format!(
                "run:{}",
                first_run.id().unwrap().to_string_lossy()
            )))
            .collect::<Vec<_>>();
        assert!(report.missing.is_empty());
        assert_eq!(report.remaining.len(), 1);
        assert_eq!(report.remaining[0].domain, "github.com");
        assert_eq!(report.remaining[0].sources, expected);
        assert!(
            !report.remaining[0]
                .sources
                .contains(&format!("project:{}", second.display()))
        );
        assert!(!report.remaining[0].sources.contains(&format!(
            "run:{}",
            second_run.id().unwrap().to_string_lossy()
        )));
    }

    #[test]
    fn absent_project_deny_reports_other_sources_without_creating_project_state() {
        let state = crate::testutil::temp_dir();
        let projects = crate::testutil::temp_dir();
        let project_path = projects.path().join("absent-policy");
        std::fs::create_dir(&project_path).unwrap();
        let project = ProjectIdentity::from_path(&project_path).unwrap();
        let store = PolicyStore::new(state.path());

        let result = run_net_with_store(
            &store,
            &[
                "deny".into(),
                "--project".into(),
                project_path.display().to_string(),
                "github.com".into(),
            ],
        );

        assert_eq!(result.code, 1);
        let expected_sources = std::iter::once("base".to_owned())
            .chain(
                crate::harness::harness_names()
                    .into_iter()
                    .filter(|name| {
                        crate::harness::lookup_harness(name).is_some_and(|harness| {
                            harness.allow_domains.iter().any(|d| d == "github.com")
                        })
                    })
                    .map(|name| format!("harness:{name}")),
            )
            .collect::<Vec<_>>()
            .join(", ");
        assert_eq!(
            result.stderr,
            format!(
                "github.com is not allowed by project:{}; allowed by {expected_sources}\n",
                project.display()
            )
        );
        let directory = store.projects().join(project.key());
        assert!(!directory.join("path").exists());
        assert!(!directory.join("allow.local").exists());
    }

    #[test]
    #[allow(clippy::format_push_string)]
    fn status_domains_output_is_exact_and_grouped_after_summary() {
        let state = crate::testutil::temp_dir();
        let projects = crate::testutil::temp_dir();
        let first_path = projects.path().join("a-first");
        let second_path = projects.path().join("z-second");
        std::fs::create_dir(&first_path).unwrap();
        std::fs::create_dir(&second_path).unwrap();
        let first = ProjectIdentity::from_path(&first_path).unwrap();
        let second = ProjectIdentity::from_path(&second_path).unwrap();
        let store = PolicyStore::new(state.path());
        store
            .mutate_global(&["global.example".into()], &[])
            .unwrap();
        store
            .mutate_project(&second, &["second.example".into()], &[])
            .unwrap();
        let one = store
            .publish_run(
                "a",
                &first,
                &["harness.example".into()],
                &["run.example".into()],
                Mode::Report,
            )
            .unwrap();
        let two = store
            .publish_run("b", &second, &[], &[], Mode::Enforce)
            .unwrap();
        let snapshot = store.snapshot().unwrap();
        let output = run_net_with_store(&store, &["status".into(), "--domains".into()]);
        assert_eq!(output.code, 0);
        let mut expected = "global: 1 domain(s)\nprojects: 2 project(s), 1 domain(s)\n".to_owned();
        for run in &snapshot.runs {
            expected.push_str(&format!(
                "run {}: harness={} project={} mode={} effective={}\n",
                run.id,
                run.harness,
                quote_path_bytes(&run.project_path),
                run.mode,
                run.effective.len(),
            ));
        }
        expected.push_str("global domains:\n  global.example\n");
        for project in &snapshot.projects {
            expected.push_str(&format!("project {}:\n", quote_path_bytes(&project.path)));
            if project.domains.is_empty() {
                expected.push_str("  (none)\n");
            } else {
                for domain in &project.domains {
                    expected.push_str(&format!("  {domain}\n"));
                }
            }
        }
        for run in &snapshot.runs {
            expected.push_str(&format!("run {} domains:\n", run.id));
            for domain in &run.effective {
                let sources = domain
                    .sources
                    .iter()
                    .map(source_label)
                    .collect::<Vec<_>>()
                    .join(", ");
                expected.push_str(&format!("  {} [{}]\n", domain.domain, sources));
            }
        }
        assert_eq!(output.stdout, expected);
        drop((one, two));
    }

    #[test]
    fn status_domains_marks_empty_live_run_group() {
        let state = crate::testutil::temp_dir();
        let project_dir = crate::testutil::temp_dir();
        let project = ProjectIdentity::from_path(project_dir.path()).unwrap();
        let store = PolicyStore::new(state.path());
        let run = store
            .publish_run("empty", &project, &[], &[], Mode::Enforce)
            .unwrap();
        let id = run.id().unwrap().to_string_lossy().into_owned();
        write_domains(&store.runs().join(&id).join("base.allow"), &[]).unwrap();

        let result = run_net_with_store(&store, &["status".into(), "--domains".into()]);

        assert_eq!(result.code, 0);
        assert_eq!(
            result.stdout,
            format!(
                "global: 0 domain(s)\nprojects: 1 project(s), 0 domain(s)\n\
                 run {id}: harness=empty project={} mode=enforce effective=0\n\
                 global domains:\n  (none)\n\
                 project {}:\n  (none)\n\
                 run {id} domains:\n  (none)\n",
                project.display(),
                project.display(),
            )
        );
        assert!(result.stderr.is_empty());
        drop(run);
    }

    #[test]
    fn store_permissions_and_run_retirement() {
        use std::os::unix::fs::PermissionsExt;
        let state = crate::testutil::temp_dir();
        let project_dir = crate::testutil::temp_dir();
        let project = ProjectIdentity::from_path(project_dir.path()).unwrap();
        let store = PolicyStore::new(state.path());
        store.ensure().unwrap();
        assert_eq!(
            std::fs::metadata(store.root())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        assert_eq!(
            std::fs::metadata(store.root().join("allow.local"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o644
        );
        assert_eq!(
            std::fs::metadata(store.root().join("log/denied.log"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o622
        );
        let run = store
            .publish_run("codex", &project, &[], &[], Mode::Enforce)
            .unwrap();
        let published = store.root().join("runs").join(run.id().unwrap());
        assert!(published.is_dir());
        assert_eq!(
            std::fs::metadata(published.join("lease"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(published.join("mode"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o644
        );
        run.retire().unwrap();
        run.retire().unwrap();
        assert!(!published.exists());
    }

    #[test]
    fn byte_path_rendering_is_unambiguous() {
        let mut expected = String::from("\"a\\x0A");
        expected.push('\\');
        expected.push('\\');
        expected.push('\\');
        expected.push('"');
        expected.push_str(" b\"");
        assert_eq!(quote_path_bytes(b"a\n\\\" b"), expected);
        assert_eq!(shell_quote_bytes(b"a'b"), b"'a'\\''b'");
        assert_eq!(shell_quote_bytes(b"a b\n\x1b\xff"), b"'a b\n\x1b\xff'");
    }

    #[test]
    fn batch_validation_is_atomic_and_concurrent_updates_do_not_lose_entries() {
        let state = crate::testutil::temp_dir();
        let store = PolicyStore::new(state.path());
        let missing = store
            .mutate_global(&[], &["missing.example".into()])
            .unwrap_err();
        assert_eq!(missing.kind(), std::io::ErrorKind::InvalidInput);
        let mut threads = Vec::new();
        for number in 0..8 {
            let store = store.clone();
            threads.push(std::thread::spawn(move || {
                store.mutate_global(&[format!("{number}.example")], &[])
            }));
        }
        for thread in threads {
            thread.join().unwrap().unwrap();
        }
        assert_eq!(read_domains(&store.global()).unwrap().len(), 8);
    }

    #[test]
    fn concurrent_first_use_keeps_one_regular_lock_inode() {
        use std::sync::{Arc, Barrier};
        let state = crate::testutil::temp_dir();
        let barrier = Arc::new(Barrier::new(8));
        let mut threads = Vec::new();
        for number in 0..8 {
            let barrier = Arc::clone(&barrier);
            let root = state.path().to_path_buf();
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                PolicyStore::new(&root).mutate_global(&[format!("{number}.first")], &[])
            }));
        }
        for thread in threads {
            thread.join().unwrap().unwrap();
        }
        let store = PolicyStore::new(state.path());
        assert_eq!(read_domains(&store.global()).unwrap().len(), 8);
        let lock = std::fs::symlink_metadata(store.lock_path()).unwrap();
        assert!(lock.file_type().is_file());
        assert_eq!(lock.permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn project_collision_invalid_input_and_idle_log_replacement() {
        use std::os::unix::fs::symlink;
        let state = crate::testutil::temp_dir();
        let store = PolicyStore::new(state.path());
        let not_dir = state.path().join("file");
        std::fs::write(&not_dir, b"").unwrap();
        assert!(ProjectIdentity::from_path(&not_dir).is_err());
        let project = crate::testutil::temp_dir();
        let id = ProjectIdentity::from_path(project.path()).unwrap();
        let bad = store
            .mutate_project(&id, &["bad/domain".into()], &[])
            .unwrap_err();
        assert_eq!(bad.kind(), std::io::ErrorKind::InvalidInput);
        assert!(!store.projects().join(&id.key).exists());
        store
            .mutate_project(&id, &["ok.example".into()], &[])
            .unwrap();
        std::fs::write(store.log(), b"denial stays\n").unwrap();
        std::fs::write(store.projects().join(&id.key).join("path"), b"other").unwrap();
        assert!(
            store
                .mutate_project(&id, &["again.example".into()], &[])
                .is_err()
        );
        assert!(
            store
                .publish_run("codex", &id, &[], &[], Mode::Enforce)
                .is_err()
        );
        assert_eq!(std::fs::read(store.log()).unwrap(), b"denial stays\n");
        std::fs::write(store.projects().join(&id.key).join("path"), &id.bytes).unwrap();
        let target = state.path().join("target");
        std::fs::write(&target, b"keep").unwrap();
        std::fs::remove_file(store.log()).unwrap();
        symlink(&target, store.log()).unwrap();
        let run = store
            .publish_run("codex", &id, &[], &[], Mode::Enforce)
            .unwrap();
        assert!(
            !std::fs::symlink_metadata(store.log())
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"keep");
        let cleanup = run.cleanup_handle();
        cleanup.retire().unwrap();
    }

    #[test]
    fn canonical_project_identity_keeps_the_supplied_absolute_path() {
        let project = crate::testutil::temp_dir();
        let supplied = project.path().to_path_buf();
        let identity = ProjectIdentity::from_canonical(supplied.clone()).unwrap();
        assert_eq!(identity.path, supplied);
        assert!(ProjectIdentity::from_canonical(PathBuf::from("relative")).is_err());
        let file = project.path().join("file");
        std::fs::write(&file, b"").unwrap();
        assert!(ProjectIdentity::from_canonical(file).is_err());
    }

    #[test]
    fn active_modes_and_reaping_contract() {
        let state = crate::testutil::temp_dir();
        let project_dir = crate::testutil::temp_dir();
        let store = PolicyStore::new(state.path());
        let project = ProjectIdentity::from_path(project_dir.path()).unwrap();
        let run = store
            .publish_run("codex", &project, &[], &[], Mode::Enforce)
            .unwrap();
        assert_eq!(store.set_active_mode(Mode::Report).unwrap(), 1);
        let active = store.active_runs().unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].mode, "report");
        let temp = store.runs().join(".stale.tmp");
        std::fs::create_dir(&temp).unwrap();
        run.retire().unwrap();
        store.locked(|| store.reap_runs_locked()).unwrap();
        assert!(!temp.exists());
        let malformed = store.runs().join("malformed");
        std::fs::create_dir(&malformed).unwrap();
        assert!(store.locked(|| store.reap_runs_locked()).is_err());
        assert!(malformed.exists());
    }

    #[test]
    fn malformed_mode_prevents_any_live_mode_rewrite() {
        use std::os::unix::fs::PermissionsExt;
        let state = crate::testutil::temp_dir();
        let one = crate::testutil::temp_dir();
        let two = crate::testutil::temp_dir();
        let store = PolicyStore::new(state.path());
        let first = store
            .publish_run(
                "a",
                &ProjectIdentity::from_path(one.path()).unwrap(),
                &[],
                &[],
                Mode::Enforce,
            )
            .unwrap();
        let second = store
            .publish_run(
                "b",
                &ProjectIdentity::from_path(two.path()).unwrap(),
                &[],
                &[],
                Mode::Enforce,
            )
            .unwrap();
        let bad = store
            .root()
            .join("runs")
            .join(second.id().unwrap())
            .join("mode");
        std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(store.set_active_mode(Mode::Open).is_err());
        let good = store
            .root()
            .join("runs")
            .join(first.id().unwrap())
            .join("mode");
        assert_eq!(std::fs::read_to_string(good).unwrap(), "enforce\n");
    }
}
