//! File-descriptor-bound reads inside a project directory (issue #444).
//!
//! A path check followed by a separate open is a TOCTOU gap: a file that
//! is a regular file inside the project at check time can be replaced by
//! a symlink pointing outside before the open, and the open then reads
//! the outside file. The fix is to open first and resolve the path of
//! the file descriptor that was actually opened, and only then decide
//! whether the opened file is inside the project, is a regular file, and
//! is not ignored by the project's gitignore rules. A swap after the
//! open cannot affect the already-open descriptor, and a swap before the
//! open is caught because the descriptor resolves to the swapped target.
//!
//! The open itself must never block: opening a FIFO blocks until a
//! writer appears, so a file swapped for a FIFO in the check-then-open
//! window would hang the reader. On Unix the open uses O_NONBLOCK and
//! the descriptor is re-checked after the open; on Windows there is no
//! open flag that both avoids the block and keeps synchronous reads
//! working, so non-regular files are rejected before the open instead
//! (a swap to a pipe in that window could still block there).

use std::fs::File;
use std::path::{Path, PathBuf};

/// Open `path` and return the file only when the file descriptor that was
/// actually opened is a regular file inside `root` and is not ignored by
/// the project's gitignore rules.
///
/// The descriptor's own path is resolved (`/dev/fd` on Unix,
/// `GetFinalPathNameByHandle` on Windows) and compared against the
/// canonical `root`; the path spelling used for the open is never
/// trusted. Containment, regular-file type and gitignore are all checked
/// on the resolved descriptor path, after the open. Any failure to open,
/// resolve, or verify fails closed.
///
/// Callers must not check containment, file type or gitignore on the
/// path before calling this function: such a pre-open check is exactly
/// the TOCTOU window this helper closes, and a redundant early check
/// would only re-introduce it.
pub fn open_file_inside_project(path: &Path, root: &Path) -> Option<File> {
    // FIFO guard (issue #444): on Windows the open below can block on a
    // named pipe, so non-regular files are rejected before the open.
    // On Unix this check is compiled out: the non-blocking open plus the
    // post-open fstat below already cover a FIFO swapped in at any point,
    // and a redundant layer here could not be killed by a mutant test.
    #[cfg(not(unix))]
    if !std::fs::metadata(path).is_ok_and(|m| m.is_file()) {
        return None;
    }
    let file = open_nonblocking(path).ok()?;
    if !file.metadata().is_ok_and(|m| m.is_file()) {
        return None;
    }
    let resolved = resolved_fd_path(&file)?;
    let root = std::fs::canonicalize(root).ok()?;
    let root = normalize_verbatim_prefix(root);
    if !resolved.starts_with(&root) {
        return None;
    }
    let rules = crate::gitignore::load_ignore_rules(&root);
    if crate::gitignore::is_ignored(&rules, &resolved, false) {
        return None;
    }
    Some(file)
}

/// Open `path` for reading without ever blocking on a FIFO.
///
/// On Unix the open uses O_NONBLOCK: a FIFO swapped in after any
/// earlier type check still opens at once, and the post-open fstat in
/// `open_file_inside_project` rejects it. O_NONBLOCK is ignored for
/// regular files, so reads behave normally. On Windows there is no
/// equivalent flag that keeps synchronous reads working, so the open is
/// plain and the pre-open type check in `open_file_inside_project` is
/// the only guard against a named pipe.
#[cfg(unix)]
fn open_nonblocking(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
}

#[cfg(not(unix))]
fn open_nonblocking(path: &Path) -> std::io::Result<File> {
    File::open(path)
}

/// Resolve the path of the file behind an open descriptor.
///
/// On Unix `/dev/fd/<fd>` is a symlink to the opened file; the kernel
/// resolves it to the real path, so a symlink swapped in before the open
/// shows up as the outside target. On Windows the same information comes
/// from `GetFinalPathNameByHandle`, which returns the final path with
/// symlinks resolved.
#[cfg(unix)]
fn resolved_fd_path(file: &File) -> Option<PathBuf> {
    use std::os::unix::io::AsRawFd;
    std::fs::read_link(format!("/dev/fd/{}", file.as_raw_fd())).ok()
}

#[cfg(windows)]
fn resolved_fd_path(file: &File) -> Option<PathBuf> {
    use std::os::windows::io::AsRawHandle;
    unsafe extern "system" {
        fn GetFinalPathNameByHandleW(
            handle: *mut std::ffi::c_void,
            path: *mut u16,
            len: u32,
            flags: u32,
        ) -> u32;
    }
    let mut buf = vec![0u16; 32768];
    let n = unsafe {
        GetFinalPathNameByHandleW(file.as_raw_handle(), buf.as_mut_ptr(), buf.len() as u32, 0)
    };
    if n == 0 || n as usize >= buf.len() {
        return None;
    }
    Some(normalize_verbatim_prefix(PathBuf::from(
        String::from_utf16_lossy(&buf[..n as usize]),
    )))
}

/// Strip the `\\?\` verbatim prefix Windows APIs spell paths with, so a
/// resolved path compares with a canonicalized root the same way.
/// `\\?\UNC\server\share\...` becomes `\\server\share\...`.
/// On Unix this is the identity function.
#[cfg(windows)]
fn normalize_verbatim_prefix(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    let text = text.strip_prefix(r"\\?\").unwrap_or(&text);
    if let Some(rest) = text.strip_prefix(r"UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    PathBuf::from(text)
}

#[cfg(not(windows))]
fn normalize_verbatim_prefix(path: PathBuf) -> PathBuf {
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        // Unique per test run without a uuid dependency: pid + nanos.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "zen_core_fs_{}_{}_{}",
            tag,
            std::process::id(),
            nanos
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn opens_regular_file_inside_project() {
        let dir = tmp("inside");
        let p = dir.join("data.txt");
        std::fs::write(&p, "inside").unwrap();
        assert!(open_file_inside_project(&p, &dir).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_absolute_path_outside_project() {
        let dir = tmp("outside");
        let proj = dir.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let secret = dir.join("secret.txt");
        std::fs::write(&secret, "top-secret").unwrap();
        assert!(open_file_inside_project(&secret, &proj).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_missing_file() {
        let dir = tmp("missing");
        assert!(open_file_inside_project(&dir.join("nope.txt"), &dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_to_outside_file() {
        let dir = tmp("symlink");
        let proj = dir.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let secret = dir.join("secret.txt");
        std::fs::write(&secret, "top-secret").unwrap();
        std::os::unix::fs::symlink(&secret, proj.join("link.txt")).unwrap();
        assert!(open_file_inside_project(&proj.join("link.txt"), &proj).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_fifo_without_blocking() {
        // Issue #444 (Z Tester BLOCKED #554): opening a FIFO blocks until
        // a writer appears, so the old plain File::open hung here. The
        // open must reject a FIFO at once. Hangs (fails by timeout) on
        // the pre-fix code, returns None promptly with the fix.
        let dir = tmp("fifo");
        let fifo = dir.join("pipe");
        let cpath = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(cpath.as_ptr(), 0o600) }, 0);
        let start = std::time::Instant::now();
        assert!(open_file_inside_project(&fifo, &dir).is_none());
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "open blocked on a FIFO"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn swapped_fifo_mid_open_never_blocks() {
        // Issue #444 (Z Tester BLOCKED #554): a file that passes any
        // pre-open type check can be swapped for a FIFO before the open,
        // so the open itself must not block. A swapper thread flips the
        // victim between a regular file and a FIFO while the reader
        // loops; the loop must finish promptly. Hangs (fails by timeout)
        // on the pre-fix code, finishes with the non-blocking open.
        let dir = tmp("fiforace");
        let victim = dir.join("victim.txt");
        std::fs::write(&victim, "inside").unwrap();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let swapper = {
            let stop = std::sync::Arc::clone(&stop);
            let victim = victim.clone();
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let _ = std::fs::remove_file(&victim);
                    let cpath = std::ffi::CString::new(victim.to_str().unwrap()).unwrap();
                    unsafe {
                        libc::mkfifo(cpath.as_ptr(), 0o600);
                    }
                    let _ = std::fs::remove_file(&victim);
                    let _ = std::fs::write(&victim, "inside");
                }
            })
        };
        let start = std::time::Instant::now();
        for _ in 0..100 {
            let _ = open_file_inside_project(&victim, &dir);
        }
        assert!(
            start.elapsed() < std::time::Duration::from_secs(10),
            "open blocked on a swapped-in FIFO"
        );
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = swapper.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn swapped_ignored_symlink_never_leaks() {
        // Issue #444 (Z Tester BLOCKED #554): the gitignore check used to
        // run on the path before the open, so a swap to a symlink
        // pointing at an ignored file in between leaked the ignored
        // content. A swapper thread flips the victim between a regular
        // file and a symlink to the ignored secret while the reader
        // loops; the secret must never come back. Fails on the pre-fix
        // code (the swap lands in the check-then-open window) and passes
        // with the descriptor-bound ignore check.
        use std::io::Read;
        let dir = tmp("ignorerace");
        std::fs::write(dir.join(".gitignore"), "secret.txt\n").unwrap();
        let secret = dir.join("secret.txt");
        std::fs::write(&secret, "IGNORED-SECRET-444").unwrap();
        let victim = dir.join("victim.txt");
        std::fs::write(&victim, "inside").unwrap();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let swapper = {
            let stop = std::sync::Arc::clone(&stop);
            let victim = victim.clone();
            let secret = secret.clone();
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let _ = std::fs::remove_file(&victim);
                    let _ = std::os::unix::fs::symlink(&secret, &victim);
                    let _ = std::fs::remove_file(&victim);
                    let _ = std::fs::write(&victim, "inside");
                }
            })
        };
        for _ in 0..300 {
            if let Some(mut file) = open_file_inside_project(&victim, &dir) {
                let mut content = String::new();
                file.read_to_string(&mut content).unwrap();
                assert!(
                    !content.contains("IGNORED-SECRET-444"),
                    "gitignore race leak: swapped symlink read the ignored file"
                );
            }
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = swapper.join();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
