//! Polling watchers that drive the `ConfigChange` and `FileChanged`
//! hook events in the REPL.
//!
//! ZenCode has no filesystem-watcher dependency, so the REPL polls file
//! mtimes once per prompt iteration. A change is reported once and the
//! snapshot refreshes, so a hook that rewrites the file it watches does
//! not loop forever (the same guard the reference documents for
//! `FileChanged`).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Paths the CLI itself just wrote via `save_local_settings`. The
/// reference does not run `ConfigChange` hooks for a change saved by the
/// session itself (`/model`, `/config`); the watcher skips those paths
/// once instead of firing.
static SELF_WRITTEN: std::sync::LazyLock<Mutex<HashSet<PathBuf>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashSet::new()));

pub fn note_self_write(path: &Path) {
    if let Ok(mut set) = SELF_WRITTEN.lock() {
        set.insert(path.to_path_buf());
    }
}

fn take_self_written(path: &Path) -> bool {
    SELF_WRITTEN
        .lock()
        .map(|mut set| set.remove(path))
        .unwrap_or(false)
}

/// Mtime snapshot for a set of paths. Each change is reported once.
pub struct MtimeWatcher {
    files: HashMap<PathBuf, Option<SystemTime>>,
}

impl MtimeWatcher {
    pub fn new(paths: Vec<PathBuf>) -> Self {
        let mut files = HashMap::new();
        for p in paths {
            files.insert(p.clone(), mtime(&p));
        }
        Self { files }
    }

    /// Watch an additional path, snapshotting its current mtime so the
    /// next poll only reports later changes.
    pub fn watch(&mut self, path: PathBuf) {
        self.files
            .entry(path.clone())
            .or_insert_with(|| mtime(&path));
    }

    /// Drop every watched path not in `keep`.
    pub fn retain(&mut self, keep: &HashSet<PathBuf>) {
        self.files.retain(|p, _| keep.contains(p));
    }

    /// `(path, event)` for paths whose mtime changed since the last poll:
    /// `"change"` for a modified file, `"add"` for a created file,
    /// `"unlink"` for a deleted file. Snapshots refresh, so each change
    /// is reported once.
    pub fn poll(&mut self) -> Vec<(PathBuf, &'static str)> {
        let mut out = Vec::new();
        for (path, seen) in self.files.iter_mut() {
            let now = mtime(path);
            if now != *seen {
                let ev = match (*seen, now) {
                    (None, Some(_)) => "add",
                    (Some(_), None) => "unlink",
                    _ => "change",
                };
                *seen = now;
                out.push((path.clone(), ev));
            }
        }
        out.sort();
        out
    }
}

/// Watches the settings files (and skill files) whose change fires
/// `ConfigChange`. Matcher target is the configuration source.
pub struct ConfigWatcher {
    watcher: MtimeWatcher,
    sources: HashMap<PathBuf, &'static str>,
}

impl ConfigWatcher {
    pub fn new(cwd: &Path, home: Option<&Path>) -> Self {
        let mut sources: HashMap<PathBuf, &'static str> = HashMap::new();
        let mut add = |path: PathBuf, source: &'static str| {
            sources.insert(path, source);
        };
        if let Some(h) = home.map(Path::to_path_buf).or_else(dirs::home_dir) {
            add(h.join(".zen").join("settings.json"), "user_settings");
            add(h.join(".zen").join("hooks.json"), "user_settings");
        }
        add(cwd.join(".zen").join("settings.json"), "project_settings");
        add(cwd.join(".zen").join("hooks.json"), "project_settings");
        add(
            cwd.join(".zen").join("settings.local.json"),
            "local_settings",
        );
        add(cwd.join(".zen").join("hooks.local.json"), "local_settings");
        for skill in zen_ext::discover_skills(cwd, home) {
            add(skill.path, "skills");
        }
        let paths: Vec<PathBuf> = sources.keys().cloned().collect();
        Self {
            watcher: MtimeWatcher::new(paths),
            sources,
        }
    }

    /// `(path, source)` for settings/skill files that changed, skipping
    /// files the CLI itself just wrote.
    pub fn poll(&mut self) -> Vec<(PathBuf, &'static str)> {
        self.watcher
            .poll()
            .into_iter()
            .filter(|(p, _)| !take_self_written(p))
            .map(|(p, _)| {
                let source = self.sources.get(&p).copied().unwrap_or("project_settings");
                (p, source)
            })
            .collect()
    }
}

/// Watches the files named by `FileChanged` matchers (literal filenames,
/// split on `|`), plus the dynamic `watchPaths` the agent collects from
/// `CwdChanged` / `FileChanged` hook output.
pub struct FileWatcher {
    watcher: MtimeWatcher,
    static_paths: HashSet<PathBuf>,
}

impl FileWatcher {
    pub fn new(cwd: &Path, hooks: &serde_json::Value) -> Self {
        let mut static_paths = HashSet::new();
        if let Some(entries) = hooks.get("FileChanged").and_then(|v| v.as_array()) {
            for entry in entries {
                if let Some(matcher) = entry.get("matcher").and_then(|v| v.as_str()) {
                    for seg in matcher.split('|') {
                        let seg = seg.trim();
                        // An omitted matcher matches every watched file
                        // and adds nothing to the watch list; `*` is
                        // treated the same way rather than as a literal
                        // file named `*`.
                        if seg.is_empty() || seg == "*" {
                            continue;
                        }
                        static_paths.insert(cwd.join(seg));
                    }
                }
            }
        }
        let paths: Vec<PathBuf> = static_paths.iter().cloned().collect();
        Self {
            watcher: MtimeWatcher::new(paths),
            static_paths,
        }
    }

    /// Merge the agent's dynamic `watchPaths` into the watch list.
    pub fn set_dynamic_paths(&mut self, dynamic: &[PathBuf]) {
        let mut keep: HashSet<PathBuf> = self.static_paths.clone();
        for p in dynamic {
            keep.insert(p.clone());
        }
        for p in &keep {
            self.watcher.watch(p.clone());
        }
        self.watcher.retain(&keep);
    }

    pub fn poll(&mut self) -> Vec<(PathBuf, &'static str)> {
        self.watcher.poll()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mtime_watcher_reports_change_add_unlink_once() {
        let dir = std::env::temp_dir().join(format!("zen_hook_watch_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.env");
        std::fs::write(&file, "one").unwrap();
        let mut w = MtimeWatcher::new(vec![file.clone()]);
        assert!(w.poll().is_empty(), "snapshot is clean at start");

        std::fs::write(&file, "two").unwrap();
        let changed = w.poll();
        assert_eq!(changed, vec![(file.clone(), "change")]);
        assert!(w.poll().is_empty(), "each change is reported once");

        std::fs::remove_file(&file).unwrap();
        assert_eq!(w.poll(), vec![(file.clone(), "unlink")]);

        std::fs::write(&file, "three").unwrap();
        assert_eq!(w.poll(), vec![(file.clone(), "add")]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn file_watcher_matcher_segments_are_literal_paths() {
        let dir = std::env::temp_dir().join(format!("zen_hook_watch_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let hooks = serde_json::json!({
            "FileChanged": [
                { "matcher": ".envrc|.env", "hooks": [] },
                { "matcher": "*", "hooks": [] },
                { "hooks": [] }
            ]
        });
        let w = FileWatcher::new(&dir, &hooks);
        assert!(w.static_paths.contains(&dir.join(".envrc")));
        assert!(w.static_paths.contains(&dir.join(".env")));
        assert_eq!(w.static_paths.len(), 2, "empty and * add nothing");
        std::fs::remove_dir_all(&dir).ok();
    }
}
