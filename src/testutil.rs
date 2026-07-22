//! Shared test helpers, compiled only under `cfg(test)`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

/// A fresh, canonicalized temp dir (the tempfile crate isn't in the budget).
/// Canonicalized so comparisons mirror the run path's physical cwd — on macOS the
/// system temp dir lives under the /var -> /private/var symlink.
pub(crate) fn temp_dir() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("vhrn-test-{}-{n}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::canonicalize(&dir).unwrap()
}
