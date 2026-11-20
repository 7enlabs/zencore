//! Polling watchers that drive the `ConfigChange` and `FileChanged`
//! hook events in the REPL.
//!
//! Zen has no filesystem-watcher dependency, so the REPL polls file
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

/// Rewrite `path` with `contents`, ensuring the new mtime is strictly
/// later than the mtime the watcher snapshotted: NTFS (Windows) only
/// ticks the file clock about every 15 ms, so two writes inside one
/// tick leave the mtime unchanged and the watcher legitimately sees
/// no change. Rewrite (sleeping briefly between attempts) until the
/// mtime actually advances, instead of assuming the clock has ticked.
/// Test-only; shared by the watcher tests here and in `repl.rs`.
#[cfg(test)]
pub(crate) fn rewrite_with_newer_mtime(path: &Path, contents: &str) {
    let before = mtime(path);
    for _ in 0..100 {
        std::fs::write(path, contents).unwrap();
        if mtime(path).is_some_and(|t| Some(t) > before) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    panic!(
        "file mtime did not advance after rewrite: {}",
        path.display()
    );
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

        // The rewrite must advance the mtime (see
        // rewrite_with_newer_mtime): on Windows two writes inside one
        // file-clock tick leave the mtime unchanged.
        rewrite_with_newer_mtime(&file, "two");
        let changed = w.poll();
        assert_eq!(changed, vec![(file.clone(), "change")]);
        assert!(w.poll().is_empty(), "each change is reported once");

        std::fs::remove_file(&file).unwrap();
        assert_eq!(w.poll(), vec![(file.clone(), "unlink")]);

        // Plain write is enough here: the snapshot is None after the
        // unlink, so any re-created file reads as "add" regardless of
        // the file-clock tick.
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

    fn hook_watch_tmp(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("zen_hook_watch_{tag}_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn config_watcher_poll_reports_change_with_source() {
        // Kills the whole-body replacements of ConfigWatcher::poll
        // (empty vec, dummy path with "" or "xyzzy" source) and the
        // inverted self-write filter (a plain change must be reported).
        // Each changed file must come back with its real path and its
        // real configuration source, not a placeholder.
        let cwd = hook_watch_tmp("cfg_src");
        let home = hook_watch_tmp("cfg_home");
        let project = cwd.join(".zen").join("settings.json");
        let local = cwd.join(".zen").join("settings.local.json");
        let user = home.join(".zen").join("settings.json");
        for (path, contents) in [
            (&project, "one"),
            (&local, "local-one"),
            (&user, "home-one"),
        ] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
        let mut w = ConfigWatcher::new(&cwd, Some(&home));
        assert!(w.poll().is_empty(), "snapshot is clean at start");

        // Each rewrite must advance the mtime (see
        // rewrite_with_newer_mtime): on Windows two writes inside one
        // file-clock tick leave the mtime unchanged and the watcher
        // would report nothing.
        for path in [&project, &local, &user] {
            rewrite_with_newer_mtime(path, "two");
        }
        let mut got = w.poll();
        got.sort();
        let mut want = vec![
            (local.clone(), "local_settings"),
            (project.clone(), "project_settings"),
            (user.clone(), "user_settings"),
        ];
        want.sort();
        assert_eq!(got, want);
        assert!(w.poll().is_empty(), "each change is reported once");
        std::fs::remove_dir_all(&cwd).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn config_watcher_poll_skips_self_written_change_once() {
        // The CLI's own save must be skipped exactly once: the next
        // poll after note_self_write reports nothing, the change after
        // that is reported again. Kills the no-op note_self_write, the
        // always-false / always-true take_self_written, and the
        // inverted filter in ConfigWatcher::poll.
        let cwd = hook_watch_tmp("cfg_self");
        let home = hook_watch_tmp("cfg_self_home");
        let path = cwd.join(".zen").join("settings.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "one").unwrap();
        let mut w = ConfigWatcher::new(&cwd, Some(&home));
        assert!(w.poll().is_empty(), "snapshot is clean at start");

        note_self_write(&path);
        // Both rewrites must advance the mtime (see
        // rewrite_with_newer_mtime): on Windows two writes inside one
        // file-clock tick leave the mtime unchanged, which would make
        // the skip assertion vacuous and hide the second change.
        rewrite_with_newer_mtime(&path, "two");
        assert!(
            w.poll().is_empty(),
            "a self-written change must be skipped once"
        );

        rewrite_with_newer_mtime(&path, "three");
        assert_eq!(
            w.poll(),
            vec![(path.clone(), "project_settings")],
            "the next change after the skip must be reported"
        );
        std::fs::remove_dir_all(&cwd).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn file_watcher_poll_reports_static_change() {
        // Kills the whole-body replacements of FileWatcher::poll: the
        // poll must return the real changed path with the real event,
        // not an empty vec or a dummy path with "" / "xyzzy". The
        // rewrite must advance the mtime (see
        // rewrite_with_newer_mtime): on Windows two writes inside one
        // file-clock tick leave the mtime unchanged.
        let dir = hook_watch_tmp("file_static");
        let file = dir.join(".envrc");
        std::fs::write(&file, "one").unwrap();
        let hooks = serde_json::json!({
            "FileChanged": [{ "matcher": ".envrc", "hooks": [] }]
        });
        let mut w = FileWatcher::new(&dir, &hooks);
        assert!(w.poll().is_empty(), "snapshot is clean at start");

        rewrite_with_newer_mtime(&file, "two");
        assert_eq!(w.poll(), vec![(file.clone(), "change")]);
        assert!(w.poll().is_empty(), "each change is reported once");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn file_watcher_dynamic_path_reports_change() {
        // Kills the no-op set_dynamic_paths and the no-op
        // MtimeWatcher::watch: adding a dynamic path must watch it,
        // snapshotting its current mtime so the first poll is quiet
        // and a later change is reported.
        let dir = hook_watch_tmp("file_dyn");
        let dynamic = dir.join("dynamic.txt");
        std::fs::write(&dynamic, "one").unwrap();
        let mut w = FileWatcher::new(&dir, &serde_json::json!({"FileChanged": []}));
        w.set_dynamic_paths(std::slice::from_ref(&dynamic));
        assert!(
            w.poll().is_empty(),
            "watching a path must snapshot it, not report it"
        );

        // The rewrite must advance the mtime (see
        // rewrite_with_newer_mtime): on Windows two writes inside one
        // file-clock tick leave the mtime unchanged.
        rewrite_with_newer_mtime(&dynamic, "two");
        assert_eq!(w.poll(), vec![(dynamic.clone(), "change")]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn file_watcher_dynamic_path_removed_stops_reporting() {
        // Kills the no-op MtimeWatcher::retain: dropping a dynamic
        // path must stop reporting it, while a static path keeps
        // reporting.
        let dir = hook_watch_tmp("file_dyn_rm");
        let dynamic = dir.join("dynamic.txt");
        let static_file = dir.join("static.txt");
        std::fs::write(&dynamic, "one").unwrap();
        std::fs::write(&static_file, "one").unwrap();
        let hooks = serde_json::json!({
            "FileChanged": [{ "matcher": "static.txt", "hooks": [] }]
        });
        let mut w = FileWatcher::new(&dir, &hooks);
        w.set_dynamic_paths(std::slice::from_ref(&dynamic));
        assert!(w.poll().is_empty(), "snapshot is clean at start");

        w.set_dynamic_paths(&[]);
        // Plain write is enough here: a removed path is not reported
        // whatever its mtime does.
        std::fs::write(&dynamic, "two").unwrap();
        assert!(
            w.poll().is_empty(),
            "a removed dynamic path must not be reported"
        );

        // The rewrite must advance the mtime (see
        // rewrite_with_newer_mtime): on Windows two writes inside one
        // file-clock tick leave the mtime unchanged.
        rewrite_with_newer_mtime(&static_file, "two");
        assert_eq!(w.poll(), vec![(static_file.clone(), "change")]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mtime_watcher_watch_snapshots_current_mtime() {
        // Kills the no-op MtimeWatcher::watch: a watched path must be
        // snapshotted (quiet until it changes) and then reported.
        let dir = hook_watch_tmp("mtime_watch");
        let file = dir.join("a.txt");
        std::fs::write(&file, "one").unwrap();
        let mut w = MtimeWatcher::new(vec![]);
        w.watch(file.clone());
        assert!(w.poll().is_empty(), "watch must snapshot, not report");

        // The rewrite must advance the mtime (see
        // rewrite_with_newer_mtime): on Windows two writes inside one
        // file-clock tick leave the mtime unchanged.
        rewrite_with_newer_mtime(&file, "two");
        assert_eq!(w.poll(), vec![(file.clone(), "change")]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mtime_watcher_retain_drops_removed_paths() {
        // Kills the no-op MtimeWatcher::retain: a dropped path must
        // stop reporting while a kept path keeps reporting.
        let dir = hook_watch_tmp("mtime_retain");
        let kept = dir.join("kept.txt");
        let dropped = dir.join("dropped.txt");
        std::fs::write(&kept, "one").unwrap();
        std::fs::write(&dropped, "one").unwrap();
        let mut w = MtimeWatcher::new(vec![kept.clone(), dropped.clone()]);
        let keep: HashSet<PathBuf> = [kept.clone()].into_iter().collect();
        w.retain(&keep);

        std::fs::write(&dropped, "two").unwrap();
        assert!(
            w.poll().is_empty(),
            "a retained-out path must not be reported"
        );

        // The rewrite must advance the mtime (see
        // rewrite_with_newer_mtime): on Windows two writes inside one
        // file-clock tick leave the mtime unchanged.
        rewrite_with_newer_mtime(&kept, "two");
        assert_eq!(w.poll(), vec![(kept.clone(), "change")]);
        std::fs::remove_dir_all(&dir).ok();
    }
}
