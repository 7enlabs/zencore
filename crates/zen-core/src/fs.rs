//! File-descriptor-bound reads inside a project directory (issue #444).
//!
//! A path check followed by a separate open is a TOCTOU gap: a file that
//! is a regular file inside the project at check time can be replaced by
//! a symlink pointing outside before the open, and the open then reads
//! the outside file. The fix is to open first and resolve the path of
//! the file descriptor that was actually opened, and only then decide
//! whether the opened file is inside the project. A swap after the open
//! cannot affect the already-open descriptor, and a swap before the open
//! is caught because the descriptor resolves to the outside file.

use std::fs::File;
use std::path::{Path, PathBuf};

/// Open `path` and return the file only when the file descriptor that was
/// actually opened is a regular file inside `root`.
///
/// The descriptor's own path is resolved (`/dev/fd` on Unix,
/// `GetFinalPathNameByHandle` on Windows) and compared against the
/// canonical `root`; the path spelling used for the open is never
/// trusted. Any failure to open, resolve, or verify fails closed.
pub fn open_file_inside_project(path: &Path, root: &Path) -> Option<File> {
    let file = File::open(path).ok()?;
    if !file.metadata().is_ok_and(|m| m.is_file()) {
        return None;
    }
    let resolved = resolved_fd_path(&file)?;
    let root = std::fs::canonicalize(root).ok()?;
    let root = normalize_verbatim_prefix(root);
    if !resolved.starts_with(&root) {
        return None;
    }
    Some(file)
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
        GetFinalPathNameByHandleW(
            file.as_raw_handle() as *mut std::ffi::c_void,
            buf.as_mut_ptr(),
            buf.len() as u32,
            0,
        )
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
}
