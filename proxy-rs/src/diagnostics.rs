//! Safe operational diagnostics and asynchronous denial auditing.
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use tokio::io::AsyncWriteExt;

use crate::domain::target::{LocalTarget, LoopbackAuthority, PublicTarget};

/// Reports operational failures without exposing them to proxy clients.
pub(crate) fn report(error: &dyn std::fmt::Display) {
    eprintln!("vhrn-proxy: {error}");
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DenialDestination(String);
impl DenialDestination {
    pub(crate) fn public(target: &PublicTarget) -> Self {
        Self(target.to_string())
    }
    pub(crate) fn local(target: &LocalTarget) -> Self {
        Self(target.to_string())
    }
    pub(crate) fn authority(authority: &LoopbackAuthority) -> Self {
        Self(authority.to_string())
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

pub(crate) struct DenialRecorder {
    path: Option<PathBuf>,
    timestamp: String,
}
impl DenialRecorder {
    pub(crate) fn new(path: Option<PathBuf>) -> Self {
        Self {
            path,
            timestamp: rfc3339_now(),
        }
    }
    #[cfg(test)]
    fn fixed(path: Option<PathBuf>, timestamp: &str) -> Self {
        Self {
            path,
            timestamp: timestamp.to_owned(),
        }
    }
    pub(crate) async fn record(&self, destination: &DenialDestination) -> Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let record = render_denial(&self.timestamp, destination);
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await
            .with_context(|| format!("open denial log {}", path.display()))?;
        file.write_all(record.as_bytes())
            .await
            .with_context(|| format!("write denial log {}", path.display()))?;
        file.flush()
            .await
            .with_context(|| format!("write denial log {}", path.display()))?;
        Ok(())
    }
}
#[must_use]
fn render_denial(timestamp: &str, destination: &DenialDestination) -> String {
    format!("{timestamp}\t{}\n", destination.0)
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
    async fn denial_output_appends_exact_bytes_and_disabled_is_noop() {
        let directory = tempdir().unwrap();
        let file = directory.path().join("deny.log");
        let destination = public_destination();
        DenialRecorder::fixed(Some(file.clone()), "2026-01-02T03:04:05Z")
            .record(&destination)
            .await
            .unwrap();
        DenialRecorder::fixed(Some(file.clone()), "2026-01-02T03:04:06Z")
            .record(&destination)
            .await
            .unwrap();
        assert_eq!(
            tokio::fs::read(file).await.unwrap(),
            b"2026-01-02T03:04:05Z\tdenied.example\n2026-01-02T03:04:06Z\tdenied.example\n"
        );
        DenialRecorder::fixed(None, "time")
            .record(&destination)
            .await
            .unwrap();
    }
    #[tokio::test]
    async fn denial_open_failure_has_path_context() {
        let directory = tempdir().unwrap();
        let error = DenialRecorder::fixed(Some(directory.path().to_owned()), "time")
            .record(&public_destination())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("open denial log"));
    }
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn denial_write_failure_has_path_context() {
        let error = DenialRecorder::fixed(Some("/dev/full".into()), "time")
            .record(&public_destination())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("write denial log"));
    }
    #[test]
    fn civil_time_boundaries_are_stable() {
        assert_eq!(civil_time(0), (1970, 1, 1, 0, 0, 0));
        assert_eq!(civil_time(951_782_400), (2000, 2, 29, 0, 0, 0));
    }
}
