//! Redacted operational diagnostics, denial auditing, and live health probes.

use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

use crate::Shutdown;
use crate::domain::policy::{Mode, PolicyReader};
use crate::domain::target::{LocalTarget, LoopbackAuthority, PublicTarget};

/// Reports a bounded category without exposing raw internal errors.
pub(crate) fn report(_: &dyn std::fmt::Display) {
    eprintln!("vhrn-proxy: operational_failure");
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DenialDestination {
    value: String,
}

impl DenialDestination {
    pub(crate) fn public(target: &PublicTarget) -> Self {
        Self {
            value: target.host().to_string(),
        }
    }

    pub(crate) fn local(target: &LocalTarget) -> Self {
        Self {
            value: target.canonical_authority().to_string(),
        }
    }

    pub(crate) fn authority(authority: &LoopbackAuthority) -> Self {
        Self {
            value: authority.to_string(),
        }
    }
}

impl From<&PublicTarget> for DenialDestination {
    fn from(target: &PublicTarget) -> Self {
        Self::public(target)
    }
}

impl From<&LocalTarget> for DenialDestination {
    fn from(target: &LocalTarget) -> Self {
        Self::local(target)
    }
}

impl From<&LoopbackAuthority> for DenialDestination {
    fn from(authority: &LoopbackAuthority) -> Self {
        Self::authority(authority)
    }
}

#[derive(Clone)]
pub(crate) struct AuditService {
    inner: Arc<AuditInner>,
}

struct AuditInner {
    path: Option<PathBuf>,
    append_lock: Mutex<()>,
    append_failed: AtomicBool,
    #[cfg(test)]
    fixed_timestamp: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AuditResult {
    Recorded,
    Disabled,
    AppendFailed,
}

impl AuditService {
    pub(crate) fn new(path: Option<PathBuf>) -> Self {
        Self {
            inner: Arc::new(AuditInner {
                path,
                append_lock: Mutex::new(()),
                append_failed: AtomicBool::new(false),
                #[cfg(test)]
                fixed_timestamp: None,
            }),
        }
    }

    #[cfg(test)]
    fn fixed(path: Option<PathBuf>, timestamp: &str) -> Self {
        Self {
            inner: Arc::new(AuditInner {
                path,
                append_lock: Mutex::new(()),
                append_failed: AtomicBool::new(false),
                fixed_timestamp: Some(timestamp.to_owned()),
            }),
        }
    }

    pub(crate) async fn verify_open(&self) -> bool {
        match &self.inner.path {
            Some(path) => open_append_file(path).await.is_ok(),
            None => true,
        }
    }

    pub(crate) async fn record(
        &self,
        destination: &DenialDestination,
        effective_mode: Mode,
    ) -> AuditResult {
        eprintln!("{}", render_denial_diagnostic(destination, effective_mode));

        let Some(path) = &self.inner.path else {
            return AuditResult::Disabled;
        };
        let _guard = self.inner.append_lock.lock().await;
        #[cfg(test)]
        let timestamp = self
            .inner
            .fixed_timestamp
            .clone()
            .unwrap_or_else(rfc3339_now);
        #[cfg(not(test))]
        let timestamp = rfc3339_now();
        let record = render_denial(&timestamp, destination);
        let result = async {
            let mut file = open_append_file(path).await?;
            file.write_all(record.as_bytes()).await?;
            file.flush().await
        }
        .await;
        if result.is_ok() {
            self.inner.append_failed.store(false, Ordering::Release);
            AuditResult::Recorded
        } else {
            self.inner.append_failed.store(true, Ordering::Release);
            AuditResult::AppendFailed
        }
    }

    pub(crate) fn append_failed(&self) -> bool {
        self.inner.append_failed.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn has_path(&self) -> bool {
        self.inner.path.is_some()
    }
}

async fn open_append_file(path: &Path) -> std::io::Result<tokio::fs::File> {
    let mut options = tokio::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(any(target_os = "linux", target_os = "android"))]
    options.custom_flags(0x800);
    #[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
    options.custom_flags(0x4);
    let file = options.open(path).await?;
    if !file.metadata().await?.is_file() {
        return Err(std::io::Error::other("denial log is not a regular file"));
    }
    Ok(file)
}

#[derive(Clone)]
pub(crate) struct HealthService {
    public_paths: Vec<PathBuf>,
    mode_path: PathBuf,
    local_paths: Option<[PathBuf; 3]>,
    audit: AuditService,
    shutdown: Shutdown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Health {
    Healthy,
    Unhealthy,
}

impl HealthService {
    pub(crate) fn new(
        public_paths: Vec<PathBuf>,
        mode_path: PathBuf,
        local_paths: Option<[PathBuf; 3]>,
        audit: AuditService,
        shutdown: Shutdown,
    ) -> Self {
        Self {
            public_paths,
            mode_path,
            local_paths,
            audit,
            shutdown,
        }
    }

    pub(crate) async fn read(&self) -> Health {
        if self.shutdown.is_requested()
            || PolicyReader::load_public_strict(&self.public_paths, &self.mode_path)
                .await
                .is_err()
        {
            return Health::Unhealthy;
        }
        if let Some(paths) = &self.local_paths
            && PolicyReader::load_local_strict(paths).await.is_err()
        {
            return Health::Unhealthy;
        }
        if !self.audit.verify_open().await || self.audit.append_failed() {
            return Health::Unhealthy;
        }
        Health::Healthy
    }
}

#[must_use]
fn render_denial(timestamp: &str, destination: &DenialDestination) -> String {
    format!("{timestamp}\t{}\n", destination.value)
}

#[must_use]
fn render_denial_diagnostic(destination: &DenialDestination, mode: Mode) -> String {
    format!(
        "vhrn-proxy: denial target={} mode={mode}",
        destination.value
    )
}

fn rfc3339_now() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |value| value.as_secs());
    let (year, month, day, hour, minute, second) = civil_time(seconds);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn civil_time(seconds: u64) -> (i64, i64, i64, u64, u64, u64) {
    let days = i64::try_from(seconds / 86_400).unwrap_or(i64::MAX);
    let time = seconds % 86_400;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    (
        year + i64::from(month <= 2),
        month,
        day,
        time / 3_600,
        time / 60 % 60,
        time % 60,
    )
}

#[cfg(test)]
mod tests {
    use hyper::{Method, Uri};
    use tempfile::tempdir;

    use super::*;
    use crate::domain::target::{Target, classify};

    fn public_destination() -> DenialDestination {
        let uri: Uri = "http://denied.example/".parse().unwrap();
        let Target::PublicHttp(target) = classify(&Method::GET, &uri) else {
            panic!("public target");
        };
        DenialDestination::public(&target)
    }

    #[tokio::test]
    async fn denial_output_appends_exact_bytes_and_disabled_is_typed() {
        let directory = tempdir().unwrap();
        let file = directory.path().join("deny.log");
        let destination = public_destination();
        tokio::fs::write(&file, b"preexisting\n").await.unwrap();
        assert_eq!(
            AuditService::fixed(Some(file.clone()), "2026-01-02T03:04:05Z")
                .record(&destination, Mode::Enforce)
                .await,
            AuditResult::Recorded
        );
        assert_eq!(
            AuditService::fixed(Some(file.clone()), "2026-01-02T03:04:06Z")
                .record(&destination, Mode::Enforce)
                .await,
            AuditResult::Recorded
        );
        assert_eq!(
            tokio::fs::read(file).await.unwrap(),
            b"preexisting\n2026-01-02T03:04:05Z\tdenied.example\n2026-01-02T03:04:06Z\tdenied.example\n"
        );
        let disabled = AuditService::fixed(None, "time");
        assert_eq!(
            disabled.record(&destination, Mode::Enforce).await,
            AuditResult::Disabled
        );
        assert!(!disabled.has_path());
    }

    #[tokio::test]
    async fn concurrent_appends_are_complete_and_never_truncate() {
        let directory = tempdir().unwrap();
        let file = directory.path().join("deny.log");
        tokio::fs::write(&file, b"preexisting\n").await.unwrap();
        let audit = AuditService::new(Some(file.clone()));
        let destination = public_destination();
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..128 {
            let audit = audit.clone();
            let destination = destination.clone();
            tasks.spawn(async move { audit.record(&destination, Mode::Enforce).await });
        }
        while let Some(result) = tasks.join_next().await {
            assert_eq!(result.unwrap(), AuditResult::Recorded);
        }
        let contents = tokio::fs::read_to_string(file).await.unwrap();
        let lines: Vec<_> = contents.lines().collect();
        assert_eq!(lines[0], "preexisting");
        assert_eq!(lines.len(), 129);
        for line in &lines[1..] {
            let (timestamp, target) = line.split_once('\t').expect("one record delimiter");
            assert_rfc3339_utc(timestamp);
            assert_eq!(target, "denied.example");
            assert!(!target.contains(char::is_whitespace));
        }
        assert!(!contents.contains("secret"));
    }

    fn assert_rfc3339_utc(timestamp: &str) {
        assert_eq!(timestamp.len(), 20);
        for (index, byte) in timestamp.bytes().enumerate() {
            if matches!(index, 4 | 7) {
                assert_eq!(byte, b'-');
            } else if index == 10 {
                assert_eq!(byte, b'T');
            } else if matches!(index, 13 | 16) {
                assert_eq!(byte, b':');
            } else if index == 19 {
                assert_eq!(byte, b'Z');
            } else {
                assert!(byte.is_ascii_digit());
            }
        }
    }

    #[tokio::test]
    async fn append_failure_is_sticky_until_a_required_append_succeeds() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("denials");
        tokio::fs::create_dir(&path).await.unwrap();
        let audit = AuditService::fixed(Some(path.clone()), "2026-01-02T03:04:05Z");
        let destination = public_destination();
        assert_eq!(
            audit.record(&destination, Mode::Enforce).await,
            AuditResult::AppendFailed
        );
        assert!(audit.append_failed());
        assert_eq!(
            audit.record(&destination, Mode::Enforce).await,
            AuditResult::AppendFailed
        );
        assert!(audit.append_failed());
        tokio::fs::remove_dir(&path).await.unwrap();
        tokio::fs::write(&path, b"").await.unwrap();
        assert!(audit.verify_open().await);
        assert!(
            audit.append_failed(),
            "an open probe cannot clear stickiness"
        );
        assert_eq!(
            audit.record(&destination, Mode::Enforce).await,
            AuditResult::Recorded
        );
        assert!(!audit.append_failed());
    }

    #[tokio::test]
    async fn health_probes_live_policy_log_stickiness_and_shutdown() {
        let directory = tempdir().unwrap();
        let public = directory.path().join("public");
        let mode = directory.path().join("mode");
        let log = directory.path().join("denials");
        tokio::fs::write(&public, b"allowed.example\n")
            .await
            .unwrap();
        tokio::fs::write(&mode, b"enforce\n").await.unwrap();
        let audit = AuditService::fixed(Some(log.clone()), "2026-01-02T03:04:05Z");
        let shutdown = Shutdown::new();
        let health = HealthService::new(
            vec![public.clone()],
            mode.clone(),
            None,
            audit.clone(),
            shutdown.clone(),
        );
        assert_eq!(health.read().await, Health::Healthy);
        tokio::fs::write(&mode, b"invalid\n").await.unwrap();
        assert_eq!(health.read().await, Health::Unhealthy);
        tokio::fs::write(&mode, b"enforce\n").await.unwrap();
        assert_eq!(health.read().await, Health::Healthy);
        tokio::fs::remove_file(&log).await.unwrap();
        tokio::fs::create_dir(&log).await.unwrap();
        assert_eq!(
            audit.record(&public_destination(), Mode::Enforce).await,
            AuditResult::AppendFailed
        );
        tokio::fs::remove_dir(&log).await.unwrap();
        tokio::fs::write(&log, b"").await.unwrap();
        assert_eq!(health.read().await, Health::Unhealthy);
        assert_eq!(
            audit.record(&public_destination(), Mode::Enforce).await,
            AuditResult::Recorded
        );
        assert_eq!(health.read().await, Health::Healthy);
        shutdown.request();
        assert_eq!(health.read().await, Health::Unhealthy);
    }

    #[tokio::test]
    async fn local_policy_is_part_of_health() {
        let directory = tempdir().unwrap();
        let public = directory.path().join("public");
        let mode = directory.path().join("mode");
        let local = [
            directory.path().join("local-one"),
            directory.path().join("local-two"),
            directory.path().join("local-three"),
        ];
        tokio::fs::write(&public, b"").await.unwrap();
        tokio::fs::write(&mode, b"enforce\n").await.unwrap();
        for path in &local {
            tokio::fs::write(path, b"").await.unwrap();
        }
        let health = HealthService::new(
            vec![public],
            mode,
            Some(local.clone()),
            AuditService::fixed(None, "time"),
            Shutdown::new(),
        );
        assert_eq!(health.read().await, Health::Healthy);
        tokio::fs::write(&local[1], b"bad authority\n")
            .await
            .unwrap();
        assert_eq!(health.read().await, Health::Unhealthy);
    }

    #[test]
    fn diagnostics_are_bounded_and_redacted() {
        let destination = public_destination();
        assert_eq!(
            render_denial("2026-01-02T03:04:05Z", &destination),
            "2026-01-02T03:04:05Z\tdenied.example\n"
        );
        assert!(!destination.value.contains(char::is_whitespace));
        assert_eq!(
            render_denial_diagnostic(&destination, Mode::Report),
            "vhrn-proxy: denial target=denied.example mode=report"
        );
        assert_eq!(
            render_denial_diagnostic(&destination, Mode::Enforce),
            "vhrn-proxy: denial target=denied.example mode=enforce"
        );
    }

    #[test]
    fn destinations_use_normalized_host_or_canonical_local_authority() {
        let public_uri: Uri = "http://DENIED.EXAMPLE.:8080/".parse().unwrap();
        let Target::PublicHttp(public) = classify(&Method::GET, &public_uri) else {
            panic!("public target");
        };
        assert_eq!(DenialDestination::public(&public).value, "denied.example");

        let local_uri: Uri = "http://LOCALHOST:080/".parse().unwrap();
        let Target::LocalHttp(local) = classify(&Method::GET, &local_uri) else {
            panic!("local target");
        };
        assert_eq!(DenialDestination::local(&local).value, "localhost:80");
    }

    #[test]
    fn civil_time_boundaries_are_stable() {
        assert_eq!(civil_time(0), (1970, 1, 1, 0, 0, 0));
        assert_eq!(civil_time(951_782_400), (2000, 2, 29, 0, 0, 0));
    }
}
