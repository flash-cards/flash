//! Scratch directories for the .apkg codecs: SQLite needs a real file, so
//! the importer extracts into one and the exporter builds in one. Removed
//! on drop.

use std::path::PathBuf;

pub(crate) struct TempDir {
    pub(crate) path: PathBuf,
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Unique per call, not just per process: imports and exports run
/// concurrently (several users, parallel tests) and must never share a
/// path. `prefix` names the family in the temp dir.
pub(crate) fn tempdir(prefix: &str) -> Result<TempDir, String> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("{prefix}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&path).map_err(|e| format!("temp dir: {e}"))?;
    Ok(TempDir { path })
}
