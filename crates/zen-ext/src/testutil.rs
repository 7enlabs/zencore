use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new() -> Self {
        // Use a dedicated scratch base for test temp dirs. The project tests
        // create fixture trees under this base and then walk *up* the
        // directory tree, collecting AGENTS.md from every ancestor. If the
        // base sits under the sandbox home, the loader picks up the
        // sandbox's own AGENTS.md (and cross-test state) and the fixtures no
        // longer match their expectations. A /tmp base is not reachable from
        // the sandbox home, so fixtures stay isolated.
        let pid = std::process::id();
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let base = if cfg!(unix) {
            PathBuf::from("/tmp").join(format!(
                "zen-ext-test-{}-{}-{}",
                pid, n, nanos
            ))
        } else {
            std::env::temp_dir().join(format!(
                "zen-ext-test-{}-{}-{}",
                pid, n, nanos
            ))
        };
        std::fs::create_dir_all(&base).expect("create temp dir");
        Self { path: base }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[allow(clippy::derivable_impls)]
impl Default for TempDir {
    fn default() -> Self {
        Self::new()
    }
}
