//! OS-level sandbox for child processes spawned by the Bash tool and
//! background jobs (Phase 2 #12).
//!
//! The sandbox is a second layer, orthogonal to the permission layer:
//! permissions decide whether a tool runs at all; the sandbox decides
//! what the child process may do at the kernel level. Enforcement exists
//! only on Linux, via Landlock (filesystem + network) and seccomp
//! (syscall denylist). Hooks run without a sandbox in this round, and
//! in-process file tools (Read, Write, Edit) run inside the agent
//! process, outside the child-process sandbox; see SECURITY_MODEL.md.
//!
//! Settled decisions (Zen, Phase 2 #12):
//! 1. Linux defaults to `workspace-write`.
//! 2. macOS/Windows: warn clearly, then run without a sandbox.
//! 3. Network: outbound allowed by default; TCP bind blocked on
//!    Landlock ABI v4+ only.
//! 4. Hooks run without a sandbox in this round.
//! 5. In-process file tools: accepted gap, the permission layer is the
//!    main control.
//! 6. `bypassPermissions` does NOT skip the sandbox.
//! 7. seccompiler (pure Rust), not libseccomp.
//! 8. Current `landlock` crate; it detects the ABI itself, and the real
//!    ABI is logged.
//!
//! The single enforcement point is `apply_to_command`, used by the Bash
//! tool (`builtin.rs`) and background jobs (`jobs.rs`). The Landlock
//! ruleset and the seccomp BPF program are built in the parent process;
//! the child only applies them in `pre_exec`, in this order:
//! `no_new_privs` -> Landlock `restrict_self` -> seccomp -> `exec`.
//! (`seccompiler::apply_filter` sets `no_new_privs` itself, and the
//! Landlock ruleset is configured to set it before restricting, so the
//! order holds even though both layers set the flag.)

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use zen_core::SandboxMode;

/// Configuration for one sandboxed spawn: the mode plus the directories
/// the child may write to.
#[derive(Debug, Clone)]
pub struct SandboxConfig {
    pub mode: SandboxMode,
    pub cwd: PathBuf,
    pub extra_dirs: Vec<PathBuf>,
    pub scratch: PathBuf,
}

impl SandboxConfig {
    pub fn new(
        mode: SandboxMode,
        cwd: PathBuf,
        extra_dirs: Vec<PathBuf>,
        scratch: PathBuf,
    ) -> Self {
        Self {
            mode,
            cwd,
            extra_dirs,
            scratch,
        }
    }

    /// Build from a tool context: cwd, additional directories and the
    /// session scratch directory.
    pub fn from_tool_context(ctx: &zen_core::ToolContext) -> Self {
        Self::new(
            ctx.sandbox_mode,
            Self::contained_cwd(ctx),
            ctx.extra_dirs.clone(),
            ctx.scratch_dir(),
        )
    }

    /// The cwd the sandbox may grant writes in: the session cwd, clamped
    /// to the workspace root (issue #321). A cwd outside the root can
    /// only come from a writer that bypassed the Bash persistence check,
    /// so the write set falls back to the root instead of following it.
    fn contained_cwd(ctx: &zen_core::ToolContext) -> PathBuf {
        let cwd = ctx.cwd();
        let canon_cwd = crate::canonicalize_lenient(&cwd);
        let canon_root = crate::canonicalize_lenient(&ctx.workspace_root());
        if canon_cwd == canon_root || canon_cwd.starts_with(&canon_root) {
            cwd
        } else {
            ctx.workspace_root()
        }
    }
}

/// Outcome of applying the sandbox to a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxOutcome {
    /// Mode `off`: nothing was applied.
    Disabled,
    /// Enforced on Linux; carries the real Landlock ABI (decision 8).
    Enforced { landlock_abi: i32 },
    /// No enforcement on this platform; the child runs unsandboxed
    /// after a one-time warning (decision 2).
    UnsupportedPlatform,
}

#[cfg(not(target_os = "linux"))]
static WARN_NO_SANDBOX: AtomicBool = AtomicBool::new(false);
/// Linux-only: only the Landlock path prints the bind-block warning
/// and the enforced-mode log line, so these are dead on other
/// platforms (Windows clippy errors on them without the gate).
#[cfg(target_os = "linux")]
static WARN_NO_BIND_BLOCK: AtomicBool = AtomicBool::new(false);
#[cfg(target_os = "linux")]
static LOG_ENFORCED: AtomicBool = AtomicBool::new(false);

/// Print a message at most once per process; returns true when this
/// call printed. The return value exists so tests can observe the
/// once-only contract without capturing stderr.
fn print_once(flag: &AtomicBool, message: &str) -> bool {
    if !flag.swap(true, Ordering::SeqCst) {
        eprintln!("{message}");
        true
    } else {
        false
    }
}

/// Directories the child may read (and execute) from, in every mode.
/// System directories the shell and common tools need: binaries,
/// libraries and configuration.
pub fn read_only_paths() -> Vec<PathBuf> {
    ["/usr", "/lib", "/lib64", "/bin", "/sbin", "/etc"]
        .iter()
        .map(PathBuf::from)
        .collect()
}

/// Character devices every sandboxed child may use. The shell and common
/// tools redirect to /dev/null constantly, and randomness sources must
/// stay available. These are harmless on their own.
pub fn device_paths() -> Vec<PathBuf> {
    [
        "/dev/null",
        "/dev/zero",
        "/dev/full",
        "/dev/random",
        "/dev/urandom",
    ]
    .iter()
    .map(PathBuf::from)
    .collect()
}

/// User git config files the child may read, in every mode. Git refuses
/// to run when its global config is unreadable, so without these the
/// sandboxed shell could not run `git status` at all. This is a narrow
/// exception on top of the design's system read set: only the two config
/// files, never the whole home directory (which holds secrets).
pub fn git_config_paths(home: Option<&Path>) -> Vec<PathBuf> {
    let Some(home) = home else {
        return vec![];
    };
    vec![
        home.join(".gitconfig"),
        home.join(".config").join("git").join("config"),
    ]
}

/// Rust toolchain paths the child may read and execute, in every mode.
/// On this machine `cargo` is a rustup shim, so the shim directory, the
/// toolchain directory and the rustup settings file must all stay
/// readable, or even `cargo --version` fails. Only these paths are
/// opened, not the registry cache or the rest of the home directory, so
/// a real `cargo build` that needs the registry stays out of scope.
pub fn toolchain_paths(
    home: Option<&Path>,
    cargo_home: Option<&Path>,
    rustup_home: Option<&Path>,
) -> Vec<PathBuf> {
    let mut out = vec![];
    let cargo_base = cargo_home
        .map(PathBuf::from)
        .or_else(|| home.map(|h| h.join(".cargo")));
    if let Some(base) = cargo_base {
        out.push(base.join("bin"));
    }
    let rustup_base = rustup_home
        .map(PathBuf::from)
        .or_else(|| home.map(|h| h.join(".rustup")));
    if let Some(base) = rustup_base {
        out.push(base.join("toolchains"));
        out.push(base.join("settings.toml"));
    }
    out
}

/// Whether `path` is a file the shell could execute. On Unix this needs
/// the executable bit, not just existence: a non-executable `cargo` on
/// PATH is not the binary the shell would run.
#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.is_file()
        && path
            .metadata()
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

/// Non-Unix fallback: no executable bit to check.
#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

/// Follow one level of indirection behind the `cargo` file PATH
/// resolves to: a symlink is followed to its target, and a `#!` script
/// is scanned for an `exec <absolute path>` line naming the real
/// binary. Returns None when the file is already the real binary, or
/// when its target cannot be determined (for example a wrapper that
/// builds its path from environment variables).
fn resolve_cargo_target(path: &Path) -> Option<PathBuf> {
    if let Ok(target) = std::fs::read_link(path) {
        if target.is_absolute() {
            return Some(target);
        }
        return path.parent().map(|parent| parent.join(target));
    }
    let text = std::fs::read_to_string(path).ok()?;
    let mut lines = text.lines();
    if !lines.next().map(|l| l.starts_with("#!")).unwrap_or(false) {
        return None;
    }
    for line in lines {
        let Some(rest) = line.trim_start().strip_prefix("exec ") else {
            continue;
        };
        let first = rest.split_whitespace().next()?;
        let first = first.trim_matches(|c| c == '"' || c == '\'');
        if first.starts_with('/') {
            return Some(PathBuf::from(first));
        }
    }
    None
}

/// First executable `cargo` on PATH, if any. Empty entries are
/// skipped, and a non-executable file named `cargo` does not count.
/// `cargo.exe` is tried as a fallback so the lookup also works on
/// Windows, where the binary carries the `.exe` suffix.
fn find_cargo_on_path(path_var: &str) -> Option<PathBuf> {
    for dir in std::env::split_paths(path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join("cargo");
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
        let candidate = dir.join("cargo.exe");
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// Directories that must stay readable for the `cargo` found on PATH
/// to run inside the sandbox (issue #254).
///
/// The file PATH resolves to is often not the real binary: rustup
/// shims are symlinks into a toolchain directory, and some hosts ship
/// `cargo` as a shell wrapper that `exec`s the real binary from
/// elsewhere (for example `/usr/local/bin/cargo` exec'ing
/// `/opt/rust/cargo/bin/cargo`). The env-var paths in
/// `toolchain_paths` never see those layouts, so even `cargo
/// --version` failed inside the sandbox with exit 126. Resolution
/// follows one level of symlink or wrapper script and returns the
/// directory holding the real binary, plus the wrapper's own directory
/// when it differs, so the shell can reach the wrapper at all. The
/// first executable `cargo` on PATH wins; empty when none is found.
pub fn cargo_executable_dirs(path_var: &str) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = vec![];
    let Some(candidate) = find_cargo_on_path(path_var) else {
        return out;
    };
    if let Some(parent) = candidate.parent() {
        out.push(parent.to_path_buf());
    }
    if let Some(parent) =
        resolve_cargo_target(&candidate).and_then(|real| real.parent().map(Path::to_path_buf))
        && !out.contains(&parent)
    {
        out.push(parent);
    }
    out
}

/// Hint to append when a sandboxed shell exits 126 ("command cannot
/// execute"). Inside the sandbox that almost always means Landlock
/// denied the real binary's path, so the hint names where the `cargo`
/// on PATH resolves to when the command mentions cargo, and says the
/// target could not be followed when the wrapper hides it.
pub fn exit_126_hint(command: &str, path_var: &str) -> String {
    let mut hint = String::from(
        "hint: exit 126 means the sandbox refused to execute a program (Landlock denied its path)",
    );
    if command.contains("cargo") {
        match find_cargo_on_path(path_var) {
            Some(candidate) => {
                hint.push_str(&format!("; the cargo on PATH is {}", candidate.display()));
                match resolve_cargo_target(&candidate) {
                    Some(real) => {
                        hint.push_str(&format!(", whose real binary is {}", real.display()))
                    }
                    None => {
                        hint.push_str(", whose real binary could not be followed");
                    }
                }
            }
            None => hint.push_str("; no executable cargo was found on PATH"),
        }
    }
    hint
}

/// Directories the child may write to, per mode. `read-only` allows no
/// writes anywhere; `workspace-write` allows writes in the working
/// directory, additional directories, the session scratch directory and
/// /tmp (the scratch directory must cover /tmp when tools need it).
pub fn write_allowed_paths(
    mode: SandboxMode,
    cwd: &Path,
    extra_dirs: &[PathBuf],
    scratch: &Path,
) -> Vec<PathBuf> {
    match mode {
        SandboxMode::Off | SandboxMode::ReadOnly => vec![],
        SandboxMode::WorkspaceWrite => {
            let mut out = vec![
                cwd.to_path_buf(),
                scratch.to_path_buf(),
                PathBuf::from("/tmp"),
            ];
            out.extend(extra_dirs.iter().cloned());
            out.sort();
            out.dedup();
            out
        }
    }
}

/// Syscalls denied unconditionally (design 2.4). A denied syscall
/// returns EPERM instead of killing the process.
pub fn unconditional_denied_syscalls() -> Vec<&'static str> {
    let mut out = vec![
        "mount",
        "umount2",
        "pivot_root",
        "init_module",
        "finit_module",
        "delete_module",
        "kexec_load",
        "kexec_file_load",
        "ptrace",
        "perf_event_open",
        "bpf",
        "keyctl",
        "add_key",
        "request_key",
        "setns",
        "reboot",
        "swapon",
        "swapoff",
    ];
    // iopl/ioperm only exist on x86_64.
    #[cfg(target_arch = "x86_64")]
    out.extend(["iopl", "ioperm"]);
    out
}

/// Syscalls denied only when they ask for a new user or mount namespace
/// (design 2.4). Plain `clone` is included alongside `unshare`: it is
/// the classic user-namespace creation path, and the design's `clone3`
/// entry cannot be expressed here because seccomp cannot inspect the
/// struct `clone3` takes its flags from, so `clone3` is a known gap
/// (see the report).
pub fn namespace_denied_syscalls() -> Vec<&'static str> {
    vec!["unshare", "clone"]
}

/// Whether TCP bind blocking applies at this Landlock ABI (decision 3):
/// only on ABI v4+, where the kernel learned network rules. Older
/// kernels keep outbound access and get a warning instead.
pub fn bind_blocked(abi: i32) -> bool {
    abi >= 4
}

/// Whether the user must be warned that this kernel cannot block TCP
/// binds: the negation of [`bind_blocked`], kept as its own function
/// so the warning decision is unit-testable at the ABI boundary.
pub fn bind_block_warning_needed(abi: i32) -> bool {
    !bind_blocked(abi)
}

/// Apply the sandbox to a `tokio::process::Command` before it is
/// spawned. This is the single enforcement point: the Bash tool and
/// background jobs both go through here. On error (e.g. Landlock
/// unavailable while a sandbox mode is on) the caller must not spawn
/// the command unsandboxed.
pub fn apply_to_command(
    cmd: &mut tokio::process::Command,
    config: &SandboxConfig,
) -> Result<SandboxOutcome, String> {
    // Mode `off` is decided here, once, for every platform, so the
    // platform modules never see it.
    if config.mode == SandboxMode::Off {
        return Ok(SandboxOutcome::Disabled);
    }
    imp::apply_to_command(cmd, config)
}

#[cfg(target_os = "linux")]
mod imp {
    use super::*;
    use landlock::{
        ABI, Access, AccessFs, AccessNet, BitFlags, PathBeneath, PathFd, Ruleset, RulesetAttr,
        RulesetCreated, RulesetCreatedAttr,
    };
    use seccompiler::{
        BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
        SeccompRule, TargetArch,
    };
    use std::collections::BTreeMap;
    use std::io;
    use std::sync::{Arc, Mutex};

    /// Syscall number of `landlock_create_ruleset` on Linux. It is 444
    /// on every architecture in the generic table (x86_64, aarch64,
    /// riscv64, 32-bit arm); libc does not expose the constant for
    /// x86_64, so it is spelled out here. Public inside the crate so
    /// tests can probe the same syscall independently.
    pub(crate) const SYS_LANDLOCK_CREATE_RULESET: libc::c_long = 444;

    // aarch64: libc does not define SYS_kexec_file_load, but the
    // kernel provides kexec_file_load as __NR_kexec_file_load 294
    // (arch/arm64/include/uapi/asm/unistd.h).
    #[cfg(target_arch = "aarch64")]
    const SYS_KEXEC_FILE_LOAD_AARCH64: libc::c_long = 294;

    /// Flag value of `LANDLOCK_CREATE_RULESET_VERSION`: with this flag
    /// the syscall takes no ruleset and returns the kernel ABI version.
    pub(crate) const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1;

    /// Probe the running kernel's Landlock ABI with a read-only
    /// `landlock_create_ruleset` call. Returns 0 when Landlock is not
    /// implemented or not enabled.
    pub fn detect_landlock_abi() -> i32 {
        let ret = unsafe {
            libc::syscall(
                SYS_LANDLOCK_CREATE_RULESET,
                std::ptr::null::<libc::c_void>(),
                0usize,
                LANDLOCK_CREATE_RULESET_VERSION,
            )
        };
        if ret < 0 { 0 } else { ret as i32 }
    }

    fn syscall_number(name: &str) -> Option<i64> {
        Some(match name {
            "mount" => libc::SYS_mount,
            "umount2" => libc::SYS_umount2,
            "pivot_root" => libc::SYS_pivot_root,
            "init_module" => libc::SYS_init_module,
            "finit_module" => libc::SYS_finit_module,
            "delete_module" => libc::SYS_delete_module,
            "kexec_load" => libc::SYS_kexec_load,
            #[cfg(target_arch = "x86_64")]
            "kexec_file_load" => libc::SYS_kexec_file_load,
            #[cfg(target_arch = "aarch64")]
            "kexec_file_load" => SYS_KEXEC_FILE_LOAD_AARCH64,
            "ptrace" => libc::SYS_ptrace,
            "perf_event_open" => libc::SYS_perf_event_open,
            "bpf" => libc::SYS_bpf,
            "keyctl" => libc::SYS_keyctl,
            "add_key" => libc::SYS_add_key,
            "request_key" => libc::SYS_request_key,
            "setns" => libc::SYS_setns,
            "unshare" => libc::SYS_unshare,
            "clone" => libc::SYS_clone,
            "reboot" => libc::SYS_reboot,
            "swapon" => libc::SYS_swapon,
            "swapoff" => libc::SYS_swapoff,
            #[cfg(target_arch = "x86_64")]
            "iopl" => libc::SYS_iopl,
            #[cfg(target_arch = "x86_64")]
            "ioperm" => libc::SYS_ioperm,
            _ => return None,
        })
    }

    /// Build the seccomp denylist filter. Denied syscalls return EPERM;
    /// everything else is allowed. The filter is compiled in the parent
    /// and installed by the child in `pre_exec`.
    pub(crate) fn build_seccomp_filter() -> Result<BpfProgram, String> {
        let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
        for name in super::unconditional_denied_syscalls() {
            let nr = syscall_number(name)
                .ok_or_else(|| format!("no syscall number for denied syscall {name}"))?;
            // An empty rule vector matches every call of the syscall
            // (SeccompRule::new(vec![]) is rejected with EmptyRule, so
            // this is the way to deny a syscall unconditionally).
            rules.insert(nr, vec![]);
        }
        // Namespace creation: deny only when the flags ask for a new
        // user or mount namespace, so ordinary thread creation (clone
        // without those bits) keeps working. One rule per bit: a syscall
        // matches when any rule in its vector matches.
        for name in super::namespace_denied_syscalls() {
            let nr = syscall_number(name)
                .ok_or_else(|| format!("no syscall number for denied syscall {name}"))?;
            let mut rs = Vec::new();
            for bit in [libc::CLONE_NEWUSER as u64, libc::CLONE_NEWNS as u64] {
                let cond = SeccompCondition::new(
                    0,
                    SeccompCmpArgLen::Qword,
                    SeccompCmpOp::MaskedEq(bit),
                    bit,
                )
                .map_err(|e| format!("seccomp condition for {name} failed: {e}"))?;
                rs.push(
                    SeccompRule::new(vec![cond])
                        .map_err(|e| format!("seccomp rule for {name} failed: {e}"))?,
                );
            }
            rules.insert(nr, rs);
        }
        let arch = TargetArch::try_from(std::env::consts::ARCH).map_err(|_| {
            format!(
                "unsupported architecture for seccomp: {}",
                std::env::consts::ARCH
            )
        })?;
        let filter = SeccompFilter::new(
            rules,
            SeccompAction::Allow,
            SeccompAction::Errno(libc::EPERM as u32),
            arch,
        )
        .map_err(|e| format!("seccomp filter build failed: {e}"))?;
        let bpf: BpfProgram = filter
            .try_into()
            .map_err(|e| format!("seccomp BPF compile failed: {e}"))?;
        Ok(bpf)
    }

    fn add_beneath(
        created: RulesetCreated,
        path: &Path,
        access: BitFlags<AccessFs>,
    ) -> Result<RulesetCreated, String> {
        let fd = PathFd::new(path)
            .map_err(|e| format!("landlock open {} failed: {e}", path.display()))?;
        let rule = PathBeneath::new(fd, access);
        created
            .add_rule(rule)
            .map_err(|e| format!("landlock add_rule {} failed: {e}", path.display()))
    }

    /// Build the Landlock ruleset in the parent process: handled access
    /// is deny-by-default, rules grant access per path. Missing paths
    /// are skipped (a path that does not exist needs no rule).
    fn build_landlock_ruleset(config: &SandboxConfig, abi: i32) -> Result<RulesetCreated, String> {
        let abi_enum = ABI::from(abi);
        let mut ruleset = Ruleset::default();
        ruleset = ruleset
            .handle_access(AccessFs::from_all(abi_enum))
            .map_err(|e| format!("landlock handle_access fs failed: {e}"))?;
        if super::bind_blocked(abi) {
            // Only bind is handled: binds are denied by default, while
            // outbound connects stay unhandled and therefore allowed.
            let bind_only: BitFlags<AccessNet> = AccessNet::BindTcp.into();
            ruleset = ruleset
                .handle_access(bind_only)
                .map_err(|e| format!("landlock handle_access net failed: {e}"))?;
        }
        let mut created = ruleset
            .create()
            .map_err(|e| format!("landlock create ruleset failed: {e}"))?;
        let read = AccessFs::from_read(abi_enum);
        let write = AccessFs::from_read(abi_enum) | AccessFs::from_write(abi_enum);
        for path in super::write_allowed_paths(
            config.mode,
            &config.cwd,
            &config.extra_dirs,
            &config.scratch,
        ) {
            if path.exists() {
                created = add_beneath(created, &path, write)?;
            }
        }
        for path in super::read_only_paths() {
            if path.exists() {
                created = add_beneath(created, &path, read)?;
            }
        }
        // Git needs its global config readable, or it refuses to run.
        for path in super::git_config_paths(dirs::home_dir().as_deref()) {
            if path.exists() {
                created = add_beneath(created, &path, read)?;
            }
        }
        // The Rust toolchain lives under the home directory, and `cargo`
        // is a rustup shim: without these paths even `cargo --version`
        // fails inside the sandbox.
        let home = dirs::home_dir();
        let cargo_home = std::env::var_os("CARGO_HOME").map(PathBuf::from);
        let rustup_home = std::env::var_os("RUSTUP_HOME").map(PathBuf::from);
        for path in super::toolchain_paths(
            home.as_deref(),
            cargo_home.as_deref(),
            rustup_home.as_deref(),
        ) {
            if path.exists() {
                created = add_beneath(created, &path, read)?;
            }
        }
        // `cargo` on PATH can be a wrapper or shim whose real binary
        // lives outside the home directory (issue #254): without the
        // real binary's directory even `cargo --version` fails with
        // exit 126.
        let path_var = std::env::var("PATH").unwrap_or_default();
        for path in super::cargo_executable_dirs(&path_var) {
            if path.exists() {
                created = add_beneath(created, &path, read)?;
            }
        }
        for path in super::device_paths() {
            if path.exists() {
                let device = AccessFs::ReadFile | AccessFs::WriteFile;
                created = add_beneath(created, &path, device)?;
            }
        }
        Ok(created.no_new_privs(true))
    }

    pub fn apply_to_command(
        cmd: &mut tokio::process::Command,
        config: &SandboxConfig,
    ) -> Result<SandboxOutcome, String> {
        // Mode `off` is already handled by the shared wrapper above.
        let abi = detect_landlock_abi();
        if abi <= 0 {
            return Err(format!(
                "Landlock is not available on this kernel, so sandbox mode '{}' cannot be enforced; refusing to run the command unsandboxed",
                config.mode.as_str()
            ));
        }
        if super::bind_block_warning_needed(abi) {
            print_once(
                &WARN_NO_BIND_BLOCK,
                &format!(
                    "warning: Landlock ABI {abi} does not support TCP bind blocking; network binds are not restricted on this kernel"
                ),
            );
        }
        // The scratch directory must exist before the ruleset opens it.
        if let Err(e) = std::fs::create_dir_all(&config.scratch) {
            return Err(format!(
                "cannot create sandbox scratch directory {}: {e}",
                config.scratch.display()
            ));
        }
        let ruleset = build_landlock_ruleset(config, abi)?;
        let bpf = build_seccomp_filter()?;
        let ruleset = Arc::new(Mutex::new(Some(ruleset)));
        let bpf = Arc::new(bpf);
        let ruleset_child = Arc::clone(&ruleset);
        let bpf_child = Arc::clone(&bpf);
        unsafe {
            cmd.pre_exec(move || {
                // Order: no_new_privs (set by the Landlock ruleset) ->
                // Landlock restrict_self -> seccomp -> exec.
                if let Some(rs) = ruleset_child
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take()
                {
                    rs.restrict_self().map_err(|e| {
                        io::Error::other(format!("landlock restrict_self failed: {e}"))
                    })?;
                }
                seccompiler::apply_filter(&bpf_child)
                    .map_err(|e| io::Error::other(format!("seccomp apply_filter failed: {e}")))?;
                Ok(())
            });
        }
        print_once(
            &LOG_ENFORCED,
            &format!(
                "sandbox: Landlock ABI {abi} enforced (mode {})",
                config.mode.as_str()
            ),
        );
        Ok(SandboxOutcome::Enforced { landlock_abi: abi })
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use super::*;

    pub fn apply_to_command(
        cmd: &mut tokio::process::Command,
        config: &SandboxConfig,
    ) -> Result<SandboxOutcome, String> {
        let _ = cmd;
        // Mode `off` is already handled by the shared wrapper above.
        print_once(
            &WARN_NO_SANDBOX,
            &format!(
                "warning: sandbox mode '{}' requested but this platform has no OS-level sandbox enforcement; running without a sandbox",
                config.mode.as_str()
            ),
        );
        Ok(SandboxOutcome::UnsupportedPlatform)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    use std::process::Stdio;

    #[test]
    fn sandbox_mode_parse_round_trip() {
        assert_eq!(SandboxMode::parse("off"), Some(SandboxMode::Off));
        assert_eq!(
            SandboxMode::parse("workspace-write"),
            Some(SandboxMode::WorkspaceWrite)
        );
        assert_eq!(
            SandboxMode::parse("workspace_write"),
            Some(SandboxMode::WorkspaceWrite)
        );
        assert_eq!(SandboxMode::parse("read-only"), Some(SandboxMode::ReadOnly));
        assert_eq!(SandboxMode::parse("read_only"), Some(SandboxMode::ReadOnly));
        assert_eq!(SandboxMode::parse("strict"), None);
        assert_eq!(SandboxMode::parse(""), None);
        for mode in [
            SandboxMode::Off,
            SandboxMode::WorkspaceWrite,
            SandboxMode::ReadOnly,
        ] {
            assert_eq!(SandboxMode::parse(mode.as_str()), Some(mode));
        }
    }

    #[test]
    fn write_paths_workspace_write_covers_cwd_scratch_tmp_and_extra() {
        let cwd = PathBuf::from("/work/repo");
        let scratch = PathBuf::from("/work/repo/.zen/scratch");
        let extra = vec![PathBuf::from("/work/shared")];
        let paths = write_allowed_paths(SandboxMode::WorkspaceWrite, &cwd, &extra, &scratch);
        assert!(paths.contains(&cwd));
        assert!(paths.contains(&scratch));
        assert!(paths.contains(&PathBuf::from("/tmp")));
        assert!(paths.contains(&PathBuf::from("/work/shared")));
        // No duplicates even when inputs overlap.
        let dupes = write_allowed_paths(
            SandboxMode::WorkspaceWrite,
            &cwd,
            std::slice::from_ref(&cwd),
            &cwd,
        );
        assert_eq!(dupes.len(), 2);
        assert!(dupes.contains(&cwd));
        assert!(dupes.contains(&PathBuf::from("/tmp")));
    }

    #[test]
    fn write_paths_read_only_and_off_allow_nothing() {
        let cwd = PathBuf::from("/work/repo");
        let scratch = PathBuf::from("/work/repo/.zen/scratch");
        assert!(write_allowed_paths(SandboxMode::ReadOnly, &cwd, &[], &scratch).is_empty());
        assert!(write_allowed_paths(SandboxMode::Off, &cwd, &[], &scratch).is_empty());
    }

    #[test]
    fn from_tool_context_clamps_cwd_outside_workspace_root() {
        // Issue #321: a session cwd outside the workspace root must not
        // move the sandbox write set; it falls back to the root.
        let root = std::env::temp_dir().join("zen-321-root");
        let _ = std::fs::create_dir_all(&root);
        let ctx = zen_core::ToolContext::new(root.clone(), "s321");
        *ctx.cwd.lock().unwrap() = std::env::temp_dir();
        let cfg = SandboxConfig::from_tool_context(&ctx);
        assert_eq!(cfg.cwd, root);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn read_only_paths_cover_system_dirs() {
        let paths = read_only_paths();
        for p in ["/usr", "/lib", "/bin", "/etc"] {
            assert!(paths.contains(&PathBuf::from(p)), "missing {p}");
        }
    }

    #[test]
    fn device_paths_cover_dev_null_and_random() {
        let paths = device_paths();
        assert!(paths.contains(&PathBuf::from("/dev/null")));
        assert!(paths.contains(&PathBuf::from("/dev/urandom")));
    }

    #[test]
    fn git_config_paths_cover_both_config_files() {
        assert!(git_config_paths(None).is_empty());
        let home = Path::new("/home/someone");
        let paths = git_config_paths(Some(home));
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/home/someone/.gitconfig"),
                PathBuf::from("/home/someone/.config/git/config"),
            ]
        );
    }

    #[test]
    fn toolchain_paths_follow_env_overrides() {
        assert!(toolchain_paths(None, None, None).is_empty());
        let home = Path::new("/home/someone");
        let paths = toolchain_paths(Some(home), None, None);
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/home/someone/.cargo/bin"),
                PathBuf::from("/home/someone/.rustup/toolchains"),
                PathBuf::from("/home/someone/.rustup/settings.toml"),
            ]
        );
        let paths = toolchain_paths(
            Some(home),
            Some(Path::new("/opt/cargo")),
            Some(Path::new("/opt/rustup")),
        );
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/opt/cargo/bin"),
                PathBuf::from("/opt/rustup/toolchains"),
                PathBuf::from("/opt/rustup/settings.toml"),
            ]
        );
    }

    /// Unique scratch dir for one cargo-resolution test case, so
    /// parallel tests never share a layout.
    #[cfg(unix)]
    fn cargo_case_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zen-cargo-dirs-{}-{}-{}",
            std::process::id(),
            tag,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock is after the epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(unix)]
    fn write_executable(path: &Path, contents: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, contents).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// A wrapper script that execs the real binary elsewhere must
    /// resolve to both directories (issue #254): the wrapper's own
    /// directory, so the shell can reach it, and the real binary's
    /// directory, so Landlock lets it run.
    #[cfg(unix)]
    #[test]
    fn cargo_executable_dirs_follows_wrapper_script() {
        let base = cargo_case_dir("wrapper");
        let real_bin = base.join("realbin");
        let wrapper_bin = base.join("wrapperbin");
        std::fs::create_dir_all(&real_bin).unwrap();
        std::fs::create_dir_all(&wrapper_bin).unwrap();
        write_executable(&real_bin.join("cargo"), "#!/bin/bash\necho cargo-fake\n");
        write_executable(
            &wrapper_bin.join("cargo"),
            &format!("#!/bin/bash\nexec {}/cargo \"$@\"\n", real_bin.display()),
        );
        let path_var = format!("{}:/usr/bin", wrapper_bin.display());
        assert_eq!(
            cargo_executable_dirs(&path_var),
            vec![wrapper_bin, real_bin]
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A wrapper that quotes its exec target must resolve the same way:
    /// the quotes are stripped before the absolute-path check, so a
    /// quoted `exec "/path/cargo"` still yields the real binary's
    /// directory.
    #[cfg(unix)]
    #[test]
    fn cargo_executable_dirs_follows_quoted_wrapper_exec() {
        let base = cargo_case_dir("quoted");
        let real_bin = base.join("realbin");
        let wrapper_bin = base.join("wrapperbin");
        std::fs::create_dir_all(&real_bin).unwrap();
        std::fs::create_dir_all(&wrapper_bin).unwrap();
        write_executable(&real_bin.join("cargo"), "#!/bin/bash\necho cargo-fake\n");
        write_executable(
            &wrapper_bin.join("cargo"),
            &format!(
                "#!/bin/bash\nexec \"{}/cargo\" \"$@\"\n",
                real_bin.display()
            ),
        );
        let path_var = format!("{}:/usr/bin", wrapper_bin.display());
        assert_eq!(
            cargo_executable_dirs(&path_var),
            vec![wrapper_bin, real_bin]
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// When the resolved binary lives in the same directory as the
    /// wrapper, the directory must appear exactly once: the dedup guard
    /// is what keeps the allowlist from growing a duplicate entry.
    #[cfg(unix)]
    #[test]
    fn cargo_executable_dirs_dedupes_same_directory() {
        let base = cargo_case_dir("samedir");
        let bin = base.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let real = bin.join("real-cargo");
        write_executable(&real, "#!/bin/bash\necho cargo-fake\n");
        std::os::unix::fs::symlink(&real, bin.join("cargo")).unwrap();
        let path_var = format!("{}:/usr/bin", bin.display());
        assert_eq!(cargo_executable_dirs(&path_var), vec![bin]);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A rustup-style symlink must resolve to the toolchain binary's
    /// directory as well as the shim's own directory.
    #[cfg(unix)]
    #[test]
    fn cargo_executable_dirs_follows_symlink() {
        let base = cargo_case_dir("symlink");
        let shim_bin = base.join("shimbin");
        let toolchain_bin = base.join("toolchain").join("bin");
        std::fs::create_dir_all(&shim_bin).unwrap();
        std::fs::create_dir_all(&toolchain_bin).unwrap();
        let real = toolchain_bin.join("cargo");
        write_executable(&real, "#!/bin/bash\necho cargo-fake\n");
        std::os::unix::fs::symlink(&real, shim_bin.join("cargo")).unwrap();
        let path_var = format!("{}:/usr/bin", shim_bin.display());
        assert_eq!(
            cargo_executable_dirs(&path_var),
            vec![shim_bin, toolchain_bin]
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A plain executable binary needs only its own directory, and a
    /// PATH with no executable cargo resolves to nothing.
    #[cfg(unix)]
    #[test]
    fn cargo_executable_dirs_plain_binary_and_missing() {
        let base = cargo_case_dir("plain");
        let bin = base.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_executable(&bin.join("cargo"), "#!/bin/bash\necho cargo-fake\n");
        let path_var = format!("{}:/usr/bin", bin.display());
        assert_eq!(cargo_executable_dirs(&path_var), vec![bin.clone()]);
        assert!(cargo_executable_dirs("/nonexistent-dir-xyz").is_empty());
        assert!(cargo_executable_dirs("").is_empty());
        // A non-executable file named cargo does not count: the next
        // PATH entry wins.
        let dead = base.join("dead");
        std::fs::create_dir_all(&dead).unwrap();
        std::fs::write(dead.join("cargo"), "not executable\n").unwrap();
        let path_var = format!("{}:{}", dead.display(), bin.display());
        assert_eq!(cargo_executable_dirs(&path_var), vec![bin]);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// On Windows the binary is `cargo.exe`: an executable file by
    /// that name counts, a non-executable one does not.
    #[cfg(unix)]
    #[test]
    fn cargo_executable_dirs_finds_cargo_exe() {
        let base = cargo_case_dir("exe");
        let bin = base.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_executable(&bin.join("cargo.exe"), "#!/bin/bash\necho cargo-fake\n");
        let path_var = format!("{}:/usr/bin", bin.display());
        assert_eq!(cargo_executable_dirs(&path_var), vec![bin.clone()]);
        // A non-executable cargo.exe does not count, and there is no
        // plain `cargo` here to fall back to.
        let dead = base.join("dead");
        std::fs::create_dir_all(&dead).unwrap();
        std::fs::write(dead.join("cargo.exe"), "not executable\n").unwrap();
        let path_var = format!("{}:/usr/bin", dead.display());
        assert!(cargo_executable_dirs(&path_var).is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A wrapper the resolver cannot follow still yields its own
    /// directory (the shell must at least reach the wrapper), and the
    /// 126 hint says so instead of naming a binary it never found.
    #[cfg(unix)]
    #[test]
    fn cargo_executable_dirs_unfollowable_wrapper_and_hint() {
        let base = cargo_case_dir("unfollowable");
        let bin = base.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_executable(
            &bin.join("cargo"),
            "#!/bin/bash\nexec \"$SOMEWHERE/bin/cargo\" \"$@\"\n",
        );
        let path_var = format!("{}:/usr/bin", bin.display());
        assert_eq!(cargo_executable_dirs(&path_var), vec![bin.clone()]);
        let hint = exit_126_hint("cargo --version", &path_var);
        assert!(
            hint.contains(&format!(
                "the cargo on PATH is {}",
                bin.join("cargo").display()
            )),
            "hint: {hint}"
        );
        assert!(hint.contains("could not be followed"), "hint: {hint}");
        // A command that never mentions cargo gets the generic hint
        // only, with no cargo location attached.
        let hint = exit_126_hint("git status", &path_var);
        assert!(hint.contains("exit 126"), "hint: {hint}");
        assert!(!hint.contains("cargo on PATH"), "hint: {hint}");
        // No cargo on PATH at all: the hint says so.
        let hint = exit_126_hint("cargo --version", "/nonexistent-dir-xyz");
        assert!(
            hint.contains("no executable cargo was found on PATH"),
            "hint: {hint}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn denylist_contains_design_syscalls() {
        let deny = unconditional_denied_syscalls();
        for name in [
            "mount",
            "umount2",
            "pivot_root",
            "init_module",
            "finit_module",
            "delete_module",
            "kexec_load",
            "kexec_file_load",
            "ptrace",
            "perf_event_open",
            "bpf",
            "keyctl",
            "add_key",
            "request_key",
            "setns",
            "reboot",
            "swapon",
            "swapoff",
        ] {
            assert!(deny.contains(&name), "denylist is missing {name}");
        }
        #[cfg(target_arch = "x86_64")]
        for name in ["iopl", "ioperm"] {
            assert!(deny.contains(&name), "denylist is missing {name}");
        }
        // No duplicates: every entry must deny exactly once.
        let mut sorted = deny.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), deny.len());
    }

    #[test]
    fn namespace_denylist_covers_unshare_and_clone() {
        let deny = namespace_denied_syscalls();
        assert!(deny.contains(&"unshare"));
        assert!(deny.contains(&"clone"));
    }

    #[test]
    fn print_once_prints_only_the_first_time() {
        let flag = AtomicBool::new(false);
        assert!(print_once(&flag, "first"));
        assert!(!print_once(&flag, "second"));
        assert!(flag.load(Ordering::SeqCst));
    }

    #[test]
    fn bind_block_warning_only_below_abi_4() {
        assert!(bind_block_warning_needed(0));
        assert!(bind_block_warning_needed(3));
        assert!(!bind_block_warning_needed(4));
        assert!(!bind_block_warning_needed(8));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn read_and_write_access_sets_are_disjoint() {
        // The landlock crate documents from_read and from_write as
        // exclusive. Pin it: disjoint sets mean `|` and `^` coincide
        // on them, so swapping the operator cannot change behaviour.
        use landlock::{ABI, AccessFs};
        for abi_int in 0..=9 {
            let abi = ABI::from(abi_int);
            assert!(
                (AccessFs::from_read(abi) & AccessFs::from_write(abi)).is_empty(),
                "read/write sets overlap at ABI {abi_int}"
            );
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn device_access_flags_share_no_bits() {
        // ReadFile and WriteFile are distinct single bits, so `|` and
        // `^` coincide on them.
        use landlock::AccessFs;
        let combined = AccessFs::ReadFile | AccessFs::WriteFile;
        assert_eq!(combined.iter().count(), 2);
    }

    #[test]
    fn bind_blocked_only_from_abi_v4() {
        assert!(!bind_blocked(0));
        assert!(!bind_blocked(1));
        assert!(!bind_blocked(3));
        assert!(bind_blocked(4));
        assert!(bind_blocked(8));
    }

    #[cfg(target_os = "linux")]
    mod linux_tests {
        use super::super::imp::detect_landlock_abi;
        use super::*;

        #[test]
        fn landlock_abi_detected_on_this_kernel() {
            // The seat kernel has Landlock (measured ABI 8); the probe
            // must see a supported ABI, otherwise nothing here enforces.
            let abi = detect_landlock_abi();
            assert!(abi >= 1, "Landlock ABI probe returned {abi}");
        }

        #[test]
        fn detect_landlock_abi_matches_raw_syscall() {
            // Independent probe of the same syscall, so a stubbed
            // detect function cannot pass this test.
            let raw = unsafe {
                libc::syscall(
                    super::super::imp::SYS_LANDLOCK_CREATE_RULESET,
                    std::ptr::null::<libc::c_void>(),
                    0usize,
                    super::super::imp::LANDLOCK_CREATE_RULESET_VERSION,
                )
            };
            let expected = if raw < 0 { 0 } else { raw as i32 };
            assert_eq!(detect_landlock_abi(), expected);
        }

        #[test]
        fn seccomp_filter_builds() {
            let bpf = super::super::imp::build_seccomp_filter().expect("seccomp filter must build");
            assert!(!bpf.is_empty(), "seccomp filter must not be empty");
        }

        /// Run one shell command under the sandbox and return its
        /// combined output and exit code.
        async fn run_sandboxed(config: &SandboxConfig, script: &str) -> (String, Option<i32>) {
            let mut cmd = tokio::process::Command::new("/bin/bash");
            cmd.args(["-c", script])
                .current_dir(&config.cwd)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            let outcome = apply_to_command(&mut cmd, config).expect("sandbox must apply");
            assert_eq!(
                outcome,
                SandboxOutcome::Enforced {
                    landlock_abi: detect_landlock_abi()
                }
            );
            let out = cmd.output().await.expect("spawned command must run");
            let mut combined = String::from_utf8_lossy(&out.stdout).to_string();
            combined.push_str(&String::from_utf8_lossy(&out.stderr));
            (combined, out.status.code())
        }

        fn test_config(mode: SandboxMode, cwd: &Path) -> SandboxConfig {
            let scratch = cwd.join(".zen-scratch-test");
            SandboxConfig::new(mode, cwd.to_path_buf(), vec![], scratch)
        }

        #[tokio::test]
        async fn workspace_write_writes_inside_cwd() {
            let dir =
                std::env::temp_dir().join(format!("zen-sandbox-allowed-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let config = test_config(SandboxMode::WorkspaceWrite, &dir);
            let (out, code) = run_sandboxed(&config, "echo hi > out.txt && cat out.txt").await;
            assert_eq!(code, Some(0), "output: {out}");
            assert!(out.contains("hi"), "output: {out}");
            assert!(dir.join("out.txt").exists());
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[tokio::test]
        async fn workspace_write_denies_write_outside_cwd() {
            let dir =
                std::env::temp_dir().join(format!("zen-sandbox-denied-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            // $HOME is outside every allowed path, so a write there
            // must fail with EACCES under the sandbox.
            let home = dirs::home_dir().expect("home dir must exist");
            let target = home.join(".zen-sandbox-deny-test");
            let _ = std::fs::remove_file(&target);
            let config = test_config(SandboxMode::WorkspaceWrite, &dir);
            let script = format!("echo hi > {}", target.display());
            let (out, code) = run_sandboxed(&config, &script).await;
            assert_ne!(code, Some(0), "write outside cwd must fail: {out}");
            assert!(
                out.contains("Permission denied"),
                "expected EACCES, got: {out}"
            );
            assert!(!target.exists(), "outside file must not have been created");
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[tokio::test]
        async fn workspace_write_reads_system_dirs() {
            let dir = std::env::temp_dir().join(format!("zen-sandbox-read-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let config = test_config(SandboxMode::WorkspaceWrite, &dir);
            let (out, code) = run_sandboxed(&config, "ls /usr/bin | head -1 && echo READ_OK").await;
            assert_eq!(code, Some(0), "output: {out}");
            assert!(out.contains("READ_OK"), "output: {out}");
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[tokio::test]
        async fn workspace_write_dev_null_redirect_works() {
            let dir =
                std::env::temp_dir().join(format!("zen-sandbox-devnull-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let config = test_config(SandboxMode::WorkspaceWrite, &dir);
            let (out, code) =
                run_sandboxed(&config, "echo hi > /dev/null && echo DEVNULL_OK").await;
            assert_eq!(code, Some(0), "output: {out}");
            assert!(out.contains("DEVNULL_OK"), "output: {out}");
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[tokio::test]
        async fn read_only_denies_write_inside_cwd() {
            let dir =
                std::env::temp_dir().join(format!("zen-sandbox-readonly-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let config = test_config(SandboxMode::ReadOnly, &dir);
            let (out, code) = run_sandboxed(&config, "echo hi > out.txt").await;
            assert_ne!(code, Some(0), "write in read-only mode must fail: {out}");
            assert!(
                out.contains("Permission denied"),
                "expected EACCES, got: {out}"
            );
            assert!(!dir.join("out.txt").exists());
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[tokio::test]
        async fn unshare_user_namespace_is_blocked() {
            let dir =
                std::env::temp_dir().join(format!("zen-sandbox-unshare-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let config = test_config(SandboxMode::WorkspaceWrite, &dir);
            // `unshare --user` creates a user namespace; under the
            // seccomp denylist it must fail with EPERM. Without the
            // filter (running as root) it would succeed.
            let (out, code) = run_sandboxed(&config, "unshare --user true; echo rc=$?").await;
            assert_eq!(code, Some(0), "output: {out}");
            assert!(out.contains("rc=1"), "expected unshare to fail, got: {out}");
            assert!(
                out.contains("Operation not permitted"),
                "expected EPERM, got: {out}"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[tokio::test]
        async fn git_status_and_cargo_version_still_run() {
            // Build a throwaway repo instead of relying on the ambient
            // checkout, so the test also runs in copied trees
            // (cargo-mutants) that carry no .git directory.
            let dir = std::env::temp_dir().join(format!("zen-sandbox-git-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let setup = |args: &[&str]| {
                std::process::Command::new("git")
                    .args(args)
                    .current_dir(&dir)
                    .output()
                    .expect("git must run for test setup")
            };
            assert!(setup(&["init", "-q"]).status.success());
            std::fs::write(dir.join("probe.txt"), "probe\n").unwrap();
            assert!(setup(&["add", "probe.txt"]).status.success());
            assert!(
                setup(&[
                    "-c",
                    "user.name=test",
                    "-c",
                    "user.email=test@test",
                    "commit",
                    "-q",
                    "-m",
                    "probe",
                ])
                .status
                .success()
            );
            let scratch = dir.join("scratch");
            let config =
                SandboxConfig::new(SandboxMode::WorkspaceWrite, dir.clone(), vec![], scratch);
            let (out, code) = run_sandboxed(
                &config,
                "git status --porcelain=v1 | head -1; git rev-parse --is-inside-work-tree",
            )
            .await;
            assert_eq!(code, Some(0), "git status output: {out}");
            assert!(out.contains("true"), "output: {out}");
            // `cargo` runs inside the sandbox only when its location can
            // be resolved the same way the product resolves it (issue
            // #254). Without an executable cargo on PATH there is nothing
            // to check; when the wrapper hides its target, the run fails
            // closed with the documented 126 instead of anything else.
            let path_var = std::env::var("PATH").unwrap_or_default();
            if let Some(candidate) = find_cargo_on_path(&path_var) {
                let target = resolve_cargo_target(&candidate);
                let is_script = std::fs::read_to_string(&candidate)
                    .map(|t| t.starts_with("#!"))
                    .unwrap_or(false);
                let (out, code) = run_sandboxed(&config, "cargo --version").await;
                if target.is_some() || !is_script {
                    assert_eq!(code, Some(0), "cargo output: {out}");
                    assert!(out.contains("cargo "), "output: {out}");
                } else {
                    assert!(
                        code == Some(0) || code == Some(126),
                        "unfollowable cargo wrapper must run or fail closed with 126, got: {code:?} / {out}"
                    );
                }
            }
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[tokio::test]
        async fn mode_off_runs_without_sandbox() {
            let dir = std::env::temp_dir().join(format!("zen-sandbox-off-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let config = test_config(SandboxMode::Off, &dir);
            let mut cmd = tokio::process::Command::new("/bin/bash");
            cmd.args(["-c", "echo OFF_OK"])
                .current_dir(&dir)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            let outcome = apply_to_command(&mut cmd, &config).expect("off must apply");
            assert_eq!(outcome, SandboxOutcome::Disabled);
            let out = cmd.output().await.expect("spawned command must run");
            assert!(out.status.success());
            assert!(
                String::from_utf8_lossy(&out.stdout).contains("OFF_OK"),
                "stdout: {}",
                String::from_utf8_lossy(&out.stdout)
            );
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// Probe denied syscalls directly: with zeroed arguments the
        /// call fails with EINVAL without a filter and with EPERM under
        /// it, so the errno tells whether the denylist caught it. No
        /// side effects: the arguments are bogus, the kernel never acts.
        #[cfg(target_arch = "x86_64")]
        #[tokio::test]
        async fn denied_syscalls_return_eperm() {
            let dir =
                std::env::temp_dir().join(format!("zen-sandbox-errno-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let config = test_config(SandboxMode::WorkspaceWrite, &dir);
            // syscall numbers on x86_64: mount 165, ptrace 101,
            // bpf 321, keyctl 250, setns 308.
            let probe = r#"python3 -c "import ctypes; l=ctypes.CDLL('libc.so.6',use_errno=True); print(' '.join(str(ctypes.get_errno() if l.syscall(n,0,0,0,0,0)==-1 else 0) for n in (165,101,321,250,308)))""#;
            let (out, code) = run_sandboxed(&config, probe).await;
            assert_eq!(code, Some(0), "output: {out}");
            // EPERM is errno 1.
            assert_eq!(
                out.trim(),
                "1 1 1 1 1",
                "expected EPERM for all five, got: {out}"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
