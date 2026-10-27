//! Git inspection on Linux: the macOS sandbox profile of [`crate::git`] as a Landlock ruleset and a
//! seccomp filter, built by the plugin before each spawn and applied in the child (`pre_exec`).
//! Adapted from the tools plugin's write sandbox (plugins share no code).
//!
//! What each macOS rule becomes:
//! - writes only under the scratch and `/dev`: Landlock write rights on those two trees only;
//! - no reads of `<CORTEX_HOME>/run` and `<CORTEX_HOME>/plugins-data/ui/electron`: Landlock has no
//!   deny rules, so `/` is granted for reading child by child along the way to those two trees,
//!   and everything beside them is granted whole (names in their parent folders stay listable,
//!   their contents do not);
//! - no network: seccomp refuses `socket(2)` of every family (unix sockets too: the frame listens
//!   on one) and io_uring (it opens sockets past seccomp), on every kernel; Landlock's own network
//!   rights (ABI 4) only cover TCP bind and connect, so they are not used;
//! - no process information: Landlock keeps a confined process from what needs ptrace access to
//!   a process outside its domain (`/proc/<pid>/environ`, `mem`, `fd`, `maps`), so the frame's and
//!   the UI's environments stay out of reach. Not covered: the world-readable `/proc/<pid>/cmdline`,
//!   `stat` and `status` of other processes (the `kern.procargs2` rule has no Landlock equivalent).
//!
//! Without Landlock (before Linux 5.13, built without it, or blocked) Git inspection is refused.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use tokio::process::Command;

const CREATE_RULESET_VERSION: u32 = 1;
const RULE_PATH_BENEATH: libc::c_int = 1;

const EXECUTE: u64 = 1 << 0;
const WRITE_FILE: u64 = 1 << 1;
const READ_FILE: u64 = 1 << 2;
const REMOVE_DIR: u64 = 1 << 4;
const REMOVE_FILE: u64 = 1 << 5;
const MAKE_CHAR: u64 = 1 << 6;
const MAKE_DIR: u64 = 1 << 7;
const MAKE_REG: u64 = 1 << 8;
const MAKE_SOCK: u64 = 1 << 9;
const MAKE_FIFO: u64 = 1 << 10;
const MAKE_BLOCK: u64 = 1 << 11;
const MAKE_SYM: u64 = 1 << 12;
/// ABI 2 (Linux 5.19): moving and linking across directories.
const REFER: u64 = 1 << 13;
/// ABI 3 (Linux 6.2): truncating a file.
const TRUNCATE: u64 = 1 << 14;
/// Rights that apply to a file itself (the rest name what may happen inside a directory).
const FILE_RIGHTS: u64 = EXECUTE | WRITE_FILE | READ_FILE | TRUNCATE;

#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
}

#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

/// The Landlock ABI version of the running kernel, or what is missing (named for the user).
fn abi() -> Result<i32, String> {
    static ABI: OnceLock<Result<i32, String>> = OnceLock::new();
    ABI.get_or_init(|| {
        // SAFETY: the version query takes no attribute and creates no file descriptor.
        let version = unsafe { libc::syscall(libc::SYS_landlock_create_ruleset, std::ptr::null::<RulesetAttr>(), 0usize, CREATE_RULESET_VERSION) };
        if version >= 1 {
            return Ok(version as i32);
        }
        Err(match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::ENOSYS) => "Landlock (Linux 5.13 or later built with CONFIG_SECURITY_LANDLOCK), which this kernel does not have".to_owned(),
            Some(libc::EOPNOTSUPP) => "Landlock, which is turned off on this system (add `landlock` to the kernel's `lsm=` boot parameter)".to_owned(),
            Some(libc::EPERM) => "Landlock, which this system blocks (a container's seccomp profile refuses its system calls)".to_owned(),
            _ => format!("Landlock, which this system refuses ({})", std::io::Error::last_os_error()),
        })
    })
    .clone()
}

/// Confine `command` (run as a Git inspection): writes only under `scratch` and `/dev`, no reads
/// below `hidden`, no sockets. `Err` names what this system lacks.
pub fn confine(command: &mut Command, scratch: &Path, hidden: &[PathBuf]) -> Result<(), String> {
    let abi = abi()?;
    let ruleset = Arc::new(ruleset(abi, scratch, hidden).map_err(|error| format!("a Landlock ruleset, which this system refuses ({error})"))?);
    let filter = Arc::new(no_sockets_filter());
    // SAFETY: only prctl and the Landlock system call run in the child, both async-signal-safe;
    // the descriptor and the filter were made before the fork and are only read here.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::syscall(libc::SYS_landlock_restrict_self, ruleset.as_raw_fd(), 0u32) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            let program = libc::sock_fprog { len: filter.len() as u16, filter: filter.as_ptr() as *mut libc::sock_filter };
            if libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &program as *const libc::sock_fprog) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(())
}

/// Reads everywhere but `hidden`; every handled right under `scratch` and `/dev`.
fn ruleset(abi: i32, scratch: &Path, hidden: &[PathBuf]) -> std::io::Result<OwnedFd> {
    let mut handled = WRITE_FILE | READ_FILE | REMOVE_DIR | REMOVE_FILE | MAKE_CHAR | MAKE_DIR | MAKE_REG | MAKE_SOCK | MAKE_FIFO | MAKE_BLOCK | MAKE_SYM;
    if abi >= 2 {
        handled |= REFER;
    }
    if abi >= 3 {
        handled |= TRUNCATE;
    }
    let attr = RulesetAttr { handled_access_fs: handled };
    // SAFETY: `attr` is a valid ruleset attribute of the size passed.
    let fd = unsafe { libc::syscall(libc::SYS_landlock_create_ruleset, &attr as *const RulesetAttr, std::mem::size_of::<RulesetAttr>(), 0u32) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the kernel returned a new descriptor (close-on-exec) that nothing else owns.
    let ruleset = unsafe { OwnedFd::from_raw_fd(fd as i32) };
    grant_read(&ruleset, Path::new("/"), hidden);
    for root in [scratch, Path::new("/dev")] {
        add_rule(&ruleset, root, handled);
    }
    Ok(ruleset)
}

/// Grant reading `path` and below, except the `hidden` trees: a directory holding one is granted
/// child by child (the directory itself gets nothing). Symbolic links are not followed: what they
/// point to is granted, or not, by its own path.
fn grant_read(ruleset: &OwnedFd, path: &Path, hidden: &[PathBuf]) {
    let Ok(meta) = std::fs::symlink_metadata(path) else { return };
    if meta.file_type().is_symlink() || hidden.iter().any(|tree| path.starts_with(tree)) {
        return;
    }
    if meta.is_dir() && hidden.iter().any(|tree| tree.starts_with(path)) {
        let Ok(entries) = std::fs::read_dir(path) else { return };
        for entry in entries.flatten() {
            grant_read(ruleset, &entry.path(), hidden);
        }
        return;
    }
    add_rule(ruleset, path, READ_FILE);
}

/// Add one path-beneath rule; a path that cannot be opened is left out (nothing granted).
fn add_rule(ruleset: &OwnedFd, path: &Path, rights: u64) {
    use std::os::unix::fs::OpenOptionsExt;
    // `O_PATH`: the descriptor only names the path; nothing is read.
    let Ok(file) = std::fs::File::options().read(true).custom_flags(libc::O_PATH | libc::O_CLOEXEC).open(path) else { return };
    let rights = if file.metadata().is_ok_and(|meta| meta.is_dir()) { rights } else { rights & FILE_RIGHTS };
    let attr = PathBeneathAttr { allowed_access: rights, parent_fd: file.as_raw_fd() };
    // SAFETY: `attr` is a valid path-beneath attribute; both descriptors are open.
    unsafe {
        libc::syscall(libc::SYS_landlock_add_rule, ruleset.as_raw_fd(), RULE_PATH_BENEATH, &attr as *const PathBeneathAttr, 0u32);
    }
}

#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: u32 = 0xC000_003E;
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: u32 = 0xC000_00B7;

/// No `socket(2)` of any family, no io_uring (it opens sockets past this filter), and no system
/// calls of another architecture (32-bit `socketcall` cannot be inspected). Refusals are EACCES
/// ("Permission denied"). `socketpair(2)` stays: it reaches nothing outside the process tree.
fn no_sockets_filter() -> Vec<libc::sock_filter> {
    const ALLOW: u32 = 0x7fff_0000;
    const DENY: u32 = 0x0005_0000 | libc::EACCES as u32;
    let load = |offset: u32| libc::sock_filter { code: (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16, jt: 0, jf: 0, k: offset };
    let jump = |op: u32, value: u32, jt: u8, jf: u8| libc::sock_filter { code: (libc::BPF_JMP | op | libc::BPF_K) as u16, jt, jf, k: value };
    let ret = |value: u32| libc::sock_filter { code: (libc::BPF_RET | libc::BPF_K) as u16, jt: 0, jf: 0, k: value };
    // seccomp_data: nr at 0, arch at 4. Jump offsets count from the next instruction: ALLOW is at
    // 9, DENY at 10.
    vec![
        /* 0 */ load(4),
        /* 1 */ jump(libc::BPF_JEQ, AUDIT_ARCH, 1, 0),
        /* 2 */ ret(DENY),
        /* 3 */ load(0),
        // x32 system calls (bit 30) on x86_64.
        /* 4 */ jump(libc::BPF_JGE, 0x4000_0000, 5, 0),
        /* 5 */ jump(libc::BPF_JEQ, libc::SYS_io_uring_setup as u32, 4, 0),
        /* 6 */ jump(libc::BPF_JEQ, libc::SYS_io_uring_enter as u32, 3, 0),
        /* 7 */ jump(libc::BPF_JEQ, libc::SYS_io_uring_register as u32, 2, 0),
        /* 8 */ jump(libc::BPF_JEQ, libc::SYS_socket as u32, 1, 0),
        /* 9 */ ret(ALLOW),
        /* 10 */ ret(DENY),
    ]
}
