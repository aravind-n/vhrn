//! Shared test helpers, compiled only under `cfg(test)`.

use tempfile::TempDir;

/// A fresh temp dir, removed when the returned guard drops. Rooted in the canonicalized
/// system temp dir so `path()` is physical — on macOS the temp dir lives under the
/// /var -> /private/var symlink, and `check_blocked_dir` compares against a physical cwd.
pub(crate) fn temp_dir() -> TempDir {
    let root = std::fs::canonicalize(std::env::temp_dir()).unwrap();
    tempfile::Builder::new()
        .prefix("vhrn-test-")
        .tempdir_in(root)
        .unwrap()
}
