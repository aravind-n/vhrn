//! Bounded, handle-based reads of host-owned policy files.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tokio::io::AsyncReadExt;

use super::{
    LocalDecision, LocalPolicySnapshot, PublicDecision, PublicPolicySnapshot,
    parse_domain_layer_at, parse_local_layer_at, parse_mode,
};
use crate::domain::target::{LoopbackAuthority, PublicHost};

const MAX_POLICY_BYTES: usize = 1024 * 1024;
const MAX_POLICY_BYTES_U64: u64 = 1024 * 1024;
// O_NONBLOCK is ignored for regular files and prevents a FIFO open from waiting for a writer.
#[cfg(any(target_os = "linux", target_os = "android"))]
const O_NONBLOCK: i32 = 0x800;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
const O_NONBLOCK: i32 = 0x4;

/// Opens policy paths afresh for strict loads and fail-closed live decisions.
pub(crate) struct PolicyReader;

impl PolicyReader {
    /// Loads every public layer and the mode, returning any validation error.
    pub(crate) async fn load_public_strict(
        paths: &[PathBuf],
        mode_path: &Path,
    ) -> Result<PublicPolicySnapshot> {
        if paths.is_empty() {
            bail!("public policy has no layers");
        }

        let mut entries = std::collections::BTreeSet::new();
        for (index, path) in paths.iter().enumerate() {
            let contents = read_bounded_utf8(path, "public policy layer").await?;
            entries.extend(parse_domain_layer_at(&contents, path, index)?);
        }

        let mode_contents = read_bounded_utf8(mode_path, "policy mode").await?;
        let mode = parse_mode(&mode_contents)
            .with_context(|| format!("validate policy mode at {}", mode_path.display()))?;
        Ok(PublicPolicySnapshot { mode, entries })
    }

    /// Loads all three local layers, returning any validation error.
    pub(crate) async fn load_local_strict(paths: &[PathBuf; 3]) -> Result<LocalPolicySnapshot> {
        let mut authorities = std::collections::HashSet::new();
        for (index, path) in paths.iter().enumerate() {
            let contents = read_bounded_utf8(path, "local policy layer").await?;
            authorities.extend(parse_local_layer_at(&contents, path, index)?);
        }
        Ok(LocalPolicySnapshot { authorities })
    }

    /// Reopens public policy and maps any bad input to empty enforce policy.
    pub(crate) async fn decide_public_live(
        paths: &[PathBuf],
        mode_path: &Path,
        host: &PublicHost,
    ) -> PublicDecision {
        match Self::load_public_strict(paths, mode_path).await {
            Ok(snapshot) => snapshot.decide(host, false),
            Err(_) => PublicPolicySnapshot::empty().decide(host, true),
        }
    }

    /// Reopens local policy and maps any bad input to an empty grant set.
    pub(crate) async fn decide_local_live(
        paths: &[PathBuf; 3],
        authority: &LoopbackAuthority,
    ) -> LocalDecision {
        match Self::load_local_strict(paths).await {
            Ok(snapshot) => snapshot.decide(authority, false),
            Err(_) => LocalPolicySnapshot::empty().decide(authority, true),
        }
    }
}

async fn read_bounded_utf8(path: &Path, kind: &str) -> Result<String> {
    let file = tokio::fs::OpenOptions::new()
        .read(true)
        .custom_flags(O_NONBLOCK)
        .open(path)
        .await
        .with_context(|| format!("open {kind} at {}", path.display()))?;
    let metadata = file
        .metadata()
        .await
        .with_context(|| format!("inspect {kind} at {}", path.display()))?;
    if !metadata.is_file() {
        bail!("{kind} at {} is not a regular file", path.display());
    }
    if metadata.len() > MAX_POLICY_BYTES_U64 {
        bail!("{kind} at {} exceeds 1 MiB", path.display());
    }

    let capacity = usize::try_from(metadata.len()).unwrap_or(MAX_POLICY_BYTES);
    let mut bytes = Vec::with_capacity(capacity);
    file.take(MAX_POLICY_BYTES_U64 + 1)
        .read_to_end(&mut bytes)
        .await
        .with_context(|| format!("read {kind} at {}", path.display()))?;
    if bytes.len() > MAX_POLICY_BYTES {
        bail!("{kind} at {} exceeds 1 MiB", path.display());
    }
    String::from_utf8(bytes)
        .with_context(|| format!("decode {kind} at {} as UTF-8", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use tokio::time::{Duration, timeout};

    #[tokio::test]
    async fn bounded_open_accepts_one_mib_and_rejects_one_byte_more() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("policy");
        tokio::fs::write(&path, vec![b'a'; MAX_POLICY_BYTES])
            .await
            .unwrap();
        assert_eq!(
            read_bounded_utf8(&path, "test policy").await.unwrap().len(),
            MAX_POLICY_BYTES
        );

        tokio::fs::write(&path, vec![b'a'; MAX_POLICY_BYTES + 1])
            .await
            .unwrap();
        assert!(read_bounded_utf8(&path, "test policy").await.is_err());
    }

    #[tokio::test]
    async fn bounded_open_rejects_directory_missing_and_invalid_utf8() {
        let directory = tempdir().unwrap();
        assert!(
            read_bounded_utf8(directory.path(), "test policy")
                .await
                .is_err()
        );

        let missing = directory.path().join("missing");
        assert!(read_bounded_utf8(&missing, "test policy").await.is_err());

        let invalid = directory.path().join("invalid");
        tokio::fs::write(&invalid, [0xff]).await.unwrap();
        assert!(read_bounded_utf8(&invalid, "test policy").await.is_err());
    }

    #[tokio::test]
    async fn bounded_open_rejects_fifo_without_waiting_for_a_writer() {
        let directory = tempdir().unwrap();
        let fifo = directory.path().join("fifo");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(status.success());

        let result = timeout(
            Duration::from_secs(1),
            read_bounded_utf8(&fifo, "test policy"),
        )
        .await
        .expect("opening a FIFO must not wait for a writer");
        assert!(result.is_err());
    }
}
