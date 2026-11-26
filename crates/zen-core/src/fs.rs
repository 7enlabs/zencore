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
//! the descriptor is re-checked after the open. On Windows the open
//! uses CreateFileW with FILE_FLAG_OVERLAPPED, which returns at once
//! even for a named pipe; the handle's type is then checked, and a
//! regular file is reopened without the flag through ReOpenFile, which
//! returns a handle to the same file object without going through the
//! path again, so synchronous reads keep working and the swap window
//! has no consequence.

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
/// regular files, so reads behave normally. On Windows the open uses
/// CreateFileW with FILE_FLAG_OVERLAPPED, which returns at once even
/// for a named pipe; the handle's type is checked, and a regular file
/// is reopened without the flag through ReOpenFile, so the returned
/// handle supports the synchronous reads `std::fs::File` performs.
/// A non-regular file is rejected here (the caller maps the error to
/// None), so the open never blocks on a pipe.
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
    use std::os::windows::io::AsRawHandle;
    // Probe open with FILE_FLAG_OVERLAPPED: CreateFileW returns at
    // once even when the path is a named pipe, so a swap to a pipe in
    // the check-then-open window cannot hang the reader. The probe
    // handle cannot be read synchronously (std reads use a NULL
    // OVERLAPPED), so a regular file is reopened without the flag
    // through ReOpenFile, which returns a handle to the same file
    // object without going through the path again. The probe handle
    // is closed when it drops at the end of this function.
    let probe = open_overlapped(path)?;
    if get_file_type(probe.as_raw_handle()) != FILE_TYPE_DISK {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    let sync = reopen_without_overlapped(probe.as_raw_handle())?;
    Ok(sync)
}

/// Open `path` with FILE_FLAG_OVERLAPPED so the open returns at once
/// even when the path is a named pipe. The returned handle is a probe:
/// it is never read from, only type-checked and reopened.
#[cfg(windows)]
fn open_overlapped(path: &Path) -> std::io::Result<File> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED | FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_handle(handle) })
}

/// Return the Win32 file type of `handle`: FILE_TYPE_DISK for a
/// regular file, FILE_TYPE_PIPE for a named pipe, and so on.
#[cfg(windows)]
fn get_file_type(handle: *mut std::ffi::c_void) -> u32 {
    unsafe { GetFileType(handle) }
}

/// Reopen the file behind `handle` without FILE_FLAG_OVERLAPPED, so
/// the returned handle supports synchronous reads. ReOpenFile returns
/// a handle to the same file object, so no path is consulted and the
/// swap window cannot intervene.
///
/// The flags argument must carry no FILE_ATTRIBUTE_* bits: MSDN states
/// that ReOpenFile's dwFlags "cannot contain any of the file attribute
/// flags (FILE_ATTRIBUTE_*)", so passing FILE_ATTRIBUTE_NORMAL here
/// made every reopen fail and regular files unreadable on Windows.
/// Pass 0 for a plain synchronous handle.
#[cfg(windows)]
fn reopen_without_overlapped(handle: *mut std::ffi::c_void) -> std::io::Result<File> {
    use std::os::windows::io::FromRawHandle;
    let new = unsafe {
        ReOpenFile(
            handle,
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            0,
        )
    };
    if new.is_null() || new == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_handle(new) })
}

#[cfg(windows)]
unsafe extern "system" {
    fn CreateFileW(
        file_name: *const u16,
        desired_access: u32,
        share_mode: u32,
        security_attributes: *mut std::ffi::c_void,
        creation_disposition: u32,
        flags_and_attributes: u32,
        template_file: *mut std::ffi::c_void,
    ) -> *mut std::ffi::c_void;
    fn GetFileType(handle: *mut std::ffi::c_void) -> u32;
    fn ReOpenFile(
        original_file: *mut std::ffi::c_void,
        desired_access: u32,
        share_mode: u32,
        flags: u32,
    ) -> *mut std::ffi::c_void;
}

#[cfg(windows)]
const GENERIC_READ: u32 = 0x8000_0000;
#[cfg(windows)]
const FILE_SHARE_READ: u32 = 0x0000_0001;
#[cfg(windows)]
const FILE_SHARE_WRITE: u32 = 0x0000_0002;
#[cfg(windows)]
const FILE_SHARE_DELETE: u32 = 0x0000_0004;
#[cfg(windows)]
const OPEN_EXISTING: u32 = 3;
#[cfg(windows)]
const FILE_ATTRIBUTE_NORMAL: u32 = 0x0000_0080;
#[cfg(windows)]
const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;
#[cfg(windows)]
const FILE_TYPE_DISK: u32 = 1;
#[cfg(windows)]
const INVALID_HANDLE_VALUE: *mut std::ffi::c_void = -1isize as *mut std::ffi::c_void;

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

    // -- Windows named-pipe race tests (issue #444, Z Tester BLOCKED #554)
    //
    // On Windows the pre-open metadata check in open_file_inside_project
    // is the only guard against a named pipe: there is no open flag that
    // both avoids the block and keeps synchronous reads working, so a
    // file swapped for a pipe in the check-then-open window can still
    // block the plain File::open. These tests exercise that window with
    // a symlink to a never-connected named pipe, the Windows analogue
    // of the Unix FIFO.

    #[cfg(windows)]
    fn wide_null(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    #[cfg(windows)]
    unsafe extern "system" {
        fn CreateNamedPipeW(
            name: *const u16,
            open_mode: u32,
            pipe_mode: u32,
            max_instances: u32,
            out_buffer_size: u32,
            in_buffer_size: u32,
            default_timeout: u32,
            security_attributes: *mut std::ffi::c_void,
        ) -> *mut std::ffi::c_void;
        fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
    }

    #[cfg(windows)]
    const PIPE_ACCESS_DUPLEX: u32 = 0x0000_0003;
    #[cfg(windows)]
    const PIPE_TYPE_BYTE: u32 = 0x0000_0000;
    #[cfg(windows)]
    const PIPE_READMODE_BYTE: u32 = 0x0000_0000;
    #[cfg(windows)]
    const PIPE_WAIT: u32 = 0x0000_0000;
    // INVALID_HANDLE_VALUE is defined at the parent module level and
    // reached here through `use super::*`.

    /// Create a named-pipe server that never accepts a client: no
    /// ConnectNamedPipe call is ever made, so a client open blocks
    /// until the server handle is closed. Returns the server handle
    /// (kept alive by the caller) and the pipe path.
    #[cfg(windows)]
    fn create_listening_pipe(tag: &str) -> (*mut std::ffi::c_void, String) {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let name = format!(r"\\.\pipe\zen_fs_{}_{}_{}", tag, std::process::id(), nanos);
        let wname = wide_null(&name);
        let handle = unsafe {
            CreateNamedPipeW(
                wname.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                1,
                4096,
                4096,
                0,
                std::ptr::null_mut(),
            )
        };
        assert!(
            !handle.is_null() && handle != INVALID_HANDLE_VALUE,
            "CreateNamedPipeW failed for {name}"
        );
        (handle, name)
    }

    #[cfg(windows)]
    fn close_pipe(handle: *mut std::ffi::c_void) {
        if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
            unsafe {
                CloseHandle(handle);
            }
        }
    }

    /// Run open_file_inside_project on a worker thread and wait at most
    /// `timeout` for it. Returns Some(result) when the open returned in
    /// time, None when it blocked past the timeout (the worker thread is
    /// left blocked; it exits with the test process).
    #[cfg(windows)]
    fn open_with_timeout(
        path: &Path,
        root: &Path,
        timeout: std::time::Duration,
    ) -> Option<Option<std::fs::File>> {
        let (tx, rx) = std::sync::mpsc::channel();
        let path = path.to_path_buf();
        let root = root.to_path_buf();
        std::thread::spawn(move || {
            let _ = tx.send(open_file_inside_project(&path, &root));
        });
        rx.recv_timeout(timeout).ok()
    }

    #[cfg(windows)]
    #[test]
    fn rejects_pipe_symlink_without_blocking() {
        // Issue #444 (Z Tester BLOCKED #554): the victim is a symlink
        // to a never-connected named pipe at check time. The overlapped
        // probe open returns at once and the type check rejects the
        // pipe, so this returns None promptly. Run through the
        // timeout wrapper so a broken probe (an open that blocks on a
        // pipe) fails the test instead of hanging it.
        let dir = tmp("winpipe");
        let (pipe, pipe_name) = create_listening_pipe("static");
        let victim = dir.join("victim.txt");
        std::os::windows::fs::symlink_file(&pipe_name, &victim).unwrap();
        let result = open_with_timeout(&victim, &dir, std::time::Duration::from_secs(5));
        close_pipe(pipe);
        let _ = std::fs::remove_dir_all(&dir);
        match result {
            Some(None) => {}
            Some(Some(_)) => panic!("pipe symlink was opened as a regular file"),
            None => panic!("open blocked on a pipe symlink (pre-open check did not reject it)"),
        }
    }

    #[cfg(windows)]
    #[test]
    fn swapped_pipe_mid_open_never_blocks() {
        // Issue #444 (Z Tester BLOCKED #554, head 6085da34): the
        // Windows open must never block on a named pipe. A swapper
        // thread flips the victim between a regular file and a symlink
        // to a never-connected named pipe on a fixed alternating
        // schedule while the reader loops; every open runs on a worker
        // thread with a 5s watchdog, so a hang fails the test at the
        // iteration that hit the window instead of hanging the suite.
        // The premise (a symlink to a never-connected pipe blocks a
        // plain open) is checked first: if it returns promptly, the
        // race test could pass vacuously, so the test fails loudly
        // instead. Fails on the pre-fix code (plain File::open blocks
        // on the swapped-in pipe), passes with the overlapped probe.
        let dir = tmp("winpiperace");
        let (race_pipe, race_name) = create_listening_pipe("race");
        let victim = dir.join("victim.txt");
        std::fs::write(&victim, "inside").unwrap();

        // Premise check on a separate pipe, so the blocked premise
        // thread never occupies the race pipe's single instance.
        let (premise_pipe, premise_name) = create_listening_pipe("premise");
        let premise_link = dir.join("premise.txt");
        std::os::windows::fs::symlink_file(&premise_name, &premise_link).unwrap();
        {
            let (tx, rx) = std::sync::mpsc::channel();
            let premise_link = premise_link.clone();
            std::thread::spawn(move || {
                let _ = tx.send(std::fs::File::open(&premise_link));
            });
            match rx.recv_timeout(std::time::Duration::from_secs(2)) {
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                _ => {
                    close_pipe(premise_pipe);
                    close_pipe(race_pipe);
                    let _ = std::fs::remove_dir_all(&dir);
                    panic!(
                        "premise broken: opening a symlink to a never-connected named pipe did not block"
                    );
                }
            }
        }
        // The premise thread stays blocked on the premise pipe for the
        // rest of the test; closing the pipe handle below unblocks it.

        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let swapper = {
            let stop = std::sync::Arc::clone(&stop);
            let victim = victim.clone();
            let race_name = race_name.clone();
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let _ = std::fs::remove_file(&victim);
                    let _ = std::os::windows::fs::symlink_file(&race_name, &victim);
                    let _ = std::fs::remove_file(&victim);
                    let _ = std::fs::write(&victim, "inside");
                }
            })
        };
        let mut blocked_at = None;
        for i in 0..2000 {
            if open_with_timeout(&victim, &dir, std::time::Duration::from_secs(5)).is_none() {
                blocked_at = Some(i);
                break;
            }
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = swapper.join();
        close_pipe(premise_pipe);
        close_pipe(race_pipe);
        let _ = std::fs::remove_dir_all(&dir);
        if let Some(i) = blocked_at {
            panic!(
                "open blocked on a swapped-in pipe at iteration {i} (Windows hang gap, issue #444)"
            );
        }
    }
}
