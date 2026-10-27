//! The Linux write sandbox: a Landlock ruleset built by the plugin before each spawn and applied in
//! the child (`pre_exec`), plus, for an offline workspace, a seccomp filter on `socket(2)`.
//!
//! Landlock grants rights per directory hierarchy and has no deny rules, so a protected path inside
//! a granted root is kept out by granting that root child by child instead (see [`Rules::command`]).
//! The rules are a snapshot taken when the shell or job starts.

use crate::sandbox::Policy;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;

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

/// The job files a command must not write (`<CORTEX_HOME>/jobs/<id>/<name>`): the plugin signals
/// the process group named in `meta.json` and runs the scripts.
const JOB_FILES: &[&str] = &["meta.json", "cmd.sh", "runner.sh"];

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
pub fn abi() -> Result<i32, String> {
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
}

/// One session's confinement: what the shell or job may write and what it may not read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rules {
    abi: i32,
    /// Writable roots (real paths, outermost only).
    roots: Vec<PathBuf>,
    /// Single writable files.
    files: Vec<PathBuf>,
    /// Trees nobody writes (whatever root holds them).
    protected_trees: Vec<PathBuf>,
    /// Paths whose own entry nobody replaces or removes: a file here is not written; a directory's
    /// contents may be (its entry is kept by not granting its parent).
    protected_nodes: Vec<PathBuf>,
    /// `<CORTEX_HOME>/jobs`, whose jobs' [`JOB_FILES`] are protected.
    jobs: PathBuf,
    /// Trees commands neither read nor write (the frame credential).
    hidden: Vec<PathBuf>,
    /// No IP network (an offline workspace).
    offline: bool,
}

impl Rules {
    pub fn new(policy: &Policy, abi: i32, roots: Vec<PathBuf>, jobs: PathBuf, offline: bool) -> Rules {
        let mut roots = roots;
        roots.push(PathBuf::from("/dev"));
        Rules {
            abi,
            roots,
            files: policy.files.clone(),
            protected_trees: policy.protected_subpaths.clone(),
            protected_nodes: policy.protected_literals.clone(),
            jobs,
            hidden: policy.hidden_paths().to_vec(),
            offline,
        }
    }

    /// Identity of the confinement (a shell started under another one must be restarted).
    pub fn key(&self) -> String {
        crate::util::content_hash(format!("{self:?}").as_bytes())
    }

    fn handled(&self) -> u64 {
        let mut rights = WRITE_FILE | READ_FILE | REMOVE_DIR | REMOVE_FILE | MAKE_CHAR | MAKE_DIR | MAKE_REG | MAKE_SOCK | MAKE_FIFO | MAKE_BLOCK | MAKE_SYM;
        if self.abi >= 2 {
            rights |= REFER;
        }
        if self.abi >= 3 {
            rights |= TRUNCATE;
        }
        rights
    }

    fn hidden(&self, path: &Path) -> bool {
        self.hidden.iter().any(|root| path.starts_with(root))
    }

    /// A file the rules keep from being written (its directory is never granted whole).
    fn protected_file(&self, path: &Path) -> bool {
        self.protected_nodes.iter().any(|node| node == path)
            || (path.parent().and_then(Path::parent) == Some(self.jobs.as_path())
                && path.file_name().is_some_and(|name| JOB_FILES.iter().any(|file| name == *file)))
    }

    /// Whether something below `dir` must stay unwritable, so `dir` is granted child by child.
    fn holds_protected(&self, dir: &Path) -> bool {
        let below = |path: &PathBuf| path != dir && path.starts_with(dir);
        self.hidden.iter().any(below)
            || self.protected_trees.iter().any(below)
            || self.protected_nodes.iter().any(below)
            || self.jobs.starts_with(dir)
            || dir.parent() == Some(self.jobs.as_path())
    }

    /// Whether these rules let a command write `path` (not protected, under a root): a folder
    /// granted child by child takes no new entries, so a path that does not exist yet is writable
    /// only below a folder granted whole.
    pub fn covers(&self, path: &Path) -> bool {
        let Some(root) = self.roots.iter().filter(|root| path.starts_with(root)).max_by_key(|root| root.as_os_str().len()) else {
            return false;
        };
        let mut dir = root.clone();
        let parts: Vec<_> = path.strip_prefix(root).map(|rest| rest.components().collect()).unwrap_or_default();
        for part in parts.iter().take(parts.len().saturating_sub(1)) {
            if !self.holds_protected(&dir) {
                return true;
            }
            dir.push(part);
        }
        !self.holds_protected(&dir) || path.symlink_metadata().is_ok()
    }

    /// Grant `rights` on `path` and below, except what `skip` keeps out and what `split` says
    /// holds something kept out (granted child by child; the directory itself gets nothing).
    /// Symbolic links are not followed: what they point to is granted, or not, by its own path.
    fn grant(&self, ruleset: &OwnedFd, path: &Path, rights: u64, skip: &dyn Fn(&Path, bool) -> bool, split: &dyn Fn(&Path) -> bool) {
        let Ok(meta) = std::fs::symlink_metadata(path) else { return };
        if meta.file_type().is_symlink() || skip(path, meta.is_dir()) {
            return;
        }
        if meta.is_dir() && split(path) {
            let Ok(entries) = std::fs::read_dir(path) else { return };
            for entry in entries.flatten() {
                self.grant(ruleset, &entry.path(), rights, skip, split);
            }
            return;
        }
        let rights = if meta.is_dir() { rights } else { rights & FILE_RIGHTS };
        add_rule(ruleset, path, rights);
    }

    /// The ruleset for one spawn: reads everywhere but the hidden trees; writes in the roots and
    /// files but the protected paths.
    fn ruleset(&self) -> std::io::Result<OwnedFd> {
        let attr = RulesetAttr { handled_access_fs: self.handled() };
        // SAFETY: `attr` is a valid ruleset attribute of the size passed.
        let fd = unsafe { libc::syscall(libc::SYS_landlock_create_ruleset, &attr as *const RulesetAttr, std::mem::size_of::<RulesetAttr>(), 0u32) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: the kernel returned a new descriptor (close-on-exec) that nothing else owns.
        let ruleset = unsafe { OwnedFd::from_raw_fd(fd as i32) };
        let hidden = |path: &Path, _dir: bool| self.hidden(path);
        let holds_hidden = |dir: &Path| self.hidden.iter().any(|root| root != dir && root.starts_with(dir));
        self.grant(&ruleset, Path::new("/"), READ_FILE, &hidden, &holds_hidden);
        let unwritable = |path: &Path, dir: bool| {
            self.hidden(path) || self.protected_trees.iter().any(|tree| path.starts_with(tree)) || (!dir && self.protected_file(path))
        };
        let holds = |dir: &Path| self.holds_protected(dir);
        for root in &self.roots {
            self.grant(&ruleset, root, self.handled(), &unwritable, &holds);
        }
        for file in &self.files {
            if std::fs::symlink_metadata(file).is_ok_and(|meta| meta.is_file()) && !unwritable(file, false) {
                add_rule(&ruleset, file, (WRITE_FILE | READ_FILE | TRUNCATE) & self.handled());
            }
        }
        Ok(ruleset)
    }

    /// `program` confined by these rules: the ruleset is built here, before the fork, and the child
    /// only applies it (system calls that are safe between fork and exec).
    pub fn command(&self, program: &Path) -> std::io::Result<std::process::Command> {
        use std::os::unix::process::CommandExt;
        let ruleset = Arc::new(self.ruleset()?);
        let filter = Arc::new(if self.offline { offline_filter() } else { Vec::new() });
        let mut command = std::process::Command::new(program);
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
                if !filter.is_empty() {
                    let program = libc::sock_fprog { len: filter.len() as u16, filter: filter.as_ptr() as *mut libc::sock_filter };
                    if libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &program as *const libc::sock_fprog) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        Ok(command)
    }
}

/// Add one path-beneath rule; a path that cannot be opened is left out (nothing granted).
fn add_rule(ruleset: &OwnedFd, path: &Path, rights: u64) {
    use std::os::unix::fs::OpenOptionsExt;
    // `O_PATH`: the descriptor only names the path; nothing is read.
    let Ok(file) = std::fs::File::options().read(true).custom_flags(libc::O_PATH | libc::O_CLOEXEC).open(path) else { return };
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

/// The seccomp filter of an offline workspace: `socket(2)` only for unix and netlink sockets, no
/// io_uring (it opens sockets past this filter), and no system calls of another architecture
/// (32-bit `socketcall` cannot be inspected). Refusals are EACCES ("Permission denied").
fn offline_filter() -> Vec<libc::sock_filter> {
    const ALLOW: u32 = 0x7fff_0000;
    const DENY: u32 = 0x0005_0000 | libc::EACCES as u32;
    let load = |offset: u32| libc::sock_filter { code: (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16, jt: 0, jf: 0, k: offset };
    let jump = |op: u32, value: u32, jt: u8, jf: u8| libc::sock_filter { code: (libc::BPF_JMP | op | libc::BPF_K) as u16, jt, jf, k: value };
    let ret = |value: u32| libc::sock_filter { code: (libc::BPF_RET | libc::BPF_K) as u16, jt: 0, jf: 0, k: value };
    // seccomp_data: nr at 0, arch at 4, args[0] at 16 (its low half on these little-endian
    // targets). Jump offsets count from the next instruction: DENY is at 12, ALLOW at 13.
    vec![
        /* 0 */ load(4),
        /* 1 */ jump(libc::BPF_JEQ, AUDIT_ARCH, 1, 0),
        /* 2 */ ret(DENY),
        /* 3 */ load(0),
        // x32 system calls (bit 30) on x86_64.
        /* 4 */ jump(libc::BPF_JGE, 0x4000_0000, 7, 0),
        /* 5 */ jump(libc::BPF_JEQ, libc::SYS_io_uring_setup as u32, 6, 0),
        /* 6 */ jump(libc::BPF_JEQ, libc::SYS_io_uring_enter as u32, 5, 0),
        /* 7 */ jump(libc::BPF_JEQ, libc::SYS_io_uring_register as u32, 4, 0),
        /* 8 */ jump(libc::BPF_JEQ, libc::SYS_socket as u32, 0, 4),
        /* 9 */ load(16),
        /* 10 */ jump(libc::BPF_JEQ, libc::AF_UNIX as u32, 2, 0),
        /* 11 */ jump(libc::BPF_JEQ, libc::AF_NETLINK as u32, 1, 0),
        /* 12 */ ret(DENY),
        /* 13 */ ret(ALLOW),
    ]
}
