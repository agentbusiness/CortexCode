//! The machine guard: what the model's commands start never takes the machine down, and nothing
//! they leave runs on unseen. Every two seconds it measures each run's processes (a session
//! shell's process group, a background job's group, and every descendant of theirs, also one that
//! left its group) and stops:
//! - a run that holds more memory than the machine has (it can live only in swap: seen, a test
//!   stuck at 99 GB on a 48 GB Mac, using no CPU, the model paged out under it);
//! - a run that holds more than half of the machine's memory while it still grows or while macOS
//!   reports memory pressure (on Linux: the kernel's memory pressure stall information);
//! - a run that keeps growing and holds the most while macOS reports memory pressure;
//! - at critical memory pressure, the run that holds the most;
//! - a background job that printed nothing, used no CPU and served no port for an hour (hung).
//!
//! Work is never stopped for how long it runs: hours-long builds and computations go on. A
//! stopped run's result says why and what to do next, so the model fixes the code instead of
//! running it again.

use crate::jobs::Jobs;
#[cfg(not(target_os = "linux"))]
use libc::{c_int, c_void};
use std::collections::{HashMap, HashSet, VecDeque};
#[cfg(not(target_os = "linux"))]
use std::ffi::CString;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How often the runs are measured.
const TICK: Duration = Duration::from_secs(2);
/// How far back "keeps growing" looks.
const GROWTH_WINDOW: Duration = Duration::from_secs(30);
/// How long a job does nothing at all before it counts as hung.
const IDLE: Duration = Duration::from_secs(60 * 60);
/// After a stop, the pressure rules wait this long (the machine takes a moment to recover).
const SETTLE: Duration = Duration::from_secs(10);
/// Programs that only pass data along; a run is named by what it really runs.
const PLUMBING: &[&str] = &[
    "bash", "sh", "zsh", "dash", "sandbox-exec", "env", "tail", "head", "grep", "tee", "cat", "sed", "awk", "sort", "uniq", "wc", "cut", "tr", "xargs", "sleep",
    "timeout",
];

/// One process of a run.
#[derive(Debug, Clone)]
pub struct Proc {
    pub pid: i32,
    /// Physical footprint: resident plus compressed and swapped memory (what Activity Monitor shows).
    pub footprint: u64,
    /// User and system CPU time, in nanoseconds.
    pub cpu_ns: u64,
    /// Start time, seconds since the epoch.
    pub start_s: u64,
}

#[cfg(not(target_os = "linux"))]
/// A run's processes: the members of process group `pgid` and all their descendants.
pub fn tree(pgid: i32) -> Vec<Proc> {
    if pgid <= 1 {
        return Vec::new();
    }
    // SAFETY: the callee writes at most `size` bytes of pids into the buffer.
    let mut queue = pids(|buffer, size| unsafe { libc::proc_listpgrppids(pgid, buffer, size) });
    // SAFETY: signal 0 only checks whether the process exists.
    if queue.is_empty() && unsafe { libc::kill(pgid, 0) } == 0 {
        queue.push(pgid);
    }
    let mut seen = HashSet::new();
    while let Some(pid) = queue.pop() {
        if seen.insert(pid) {
            // SAFETY: as above.
            queue.extend(pids(|buffer, size| unsafe { libc::proc_listchildpids(pid, buffer, size) }));
        }
    }
    let mut procs: Vec<Proc> = seen.into_iter().filter_map(proc_info).collect();
    procs.sort_by_key(|proc| proc.pid);
    procs
}

#[cfg(not(target_os = "linux"))]
/// The pids a libproc list call writes (it returns a count or a byte size depending on the call;
/// the buffer starts zeroed, so both read the same).
fn pids(list: impl Fn(*mut c_void, c_int) -> c_int) -> Vec<i32> {
    let mut buffer = vec![0i32; 2048];
    let size = (buffer.len() * std::mem::size_of::<i32>()) as c_int;
    let n = list(buffer.as_mut_ptr().cast(), size);
    if n <= 0 {
        return Vec::new();
    }
    buffer.truncate((n as usize).min(2048));
    buffer.retain(|pid| *pid > 0);
    buffer
}

#[cfg(not(target_os = "linux"))]
fn proc_info(pid: i32) -> Option<Proc> {
    // SAFETY: zeroed plain-data structs the kernel fills.
    let mut usage: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
    // SAFETY: the buffer is a rusage_info_v2, the flavor asked for.
    if unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V2, (&mut usage as *mut libc::rusage_info_v2).cast()) } != 0 {
        return None;
    }
    // SAFETY: as above.
    let mut bsd: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as c_int;
    // SAFETY: the buffer is a proc_bsdinfo of `size` bytes.
    let start_s = if unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&mut bsd as *mut libc::proc_bsdinfo).cast(), size) } == size { bsd.pbi_start_tvsec } else { 0 };
    let (numer, denom) = timebase();
    let ticks = usage.ri_user_time.saturating_add(usage.ri_system_time) as u128;
    Some(Proc { pid, footprint: usage.ri_phys_footprint, cpu_ns: (ticks * numer as u128 / denom.max(1) as u128) as u64, start_s })
}

#[cfg(not(target_os = "linux"))]
/// The Mach timebase (CPU times come in its ticks).
#[allow(deprecated)]
fn timebase() -> (u32, u32) {
    static TIMEBASE: std::sync::OnceLock<(u32, u32)> = std::sync::OnceLock::new();
    *TIMEBASE.get_or_init(|| {
        let mut info = libc::mach_timebase_info { numer: 0, denom: 0 };
        // SAFETY: the kernel fills the struct.
        let ok = unsafe { libc::mach_timebase_info(&mut info) } == 0;
        if ok && info.denom != 0 { (info.numer, info.denom) } else { (1, 1) }
    })
}

#[cfg(not(target_os = "linux"))]
fn sysctl_u64(name: &str) -> Option<u64> {
    let name = CString::new(name).ok()?;
    let mut value = [0u8; 8];
    let mut size = value.len();
    // SAFETY: the kernel writes at most `size` bytes into `value`.
    if unsafe { libc::sysctlbyname(name.as_ptr(), value.as_mut_ptr().cast(), &mut size, std::ptr::null_mut(), 0) } != 0 {
        return None;
    }
    Some(match size {
        4 => u32::from_ne_bytes([value[0], value[1], value[2], value[3]]) as u64,
        8 => u64::from_ne_bytes(value),
        _ => return None,
    })
}

#[cfg(not(target_os = "linux"))]
/// macOS memory pressure: 1 normal, 2 warning, 4 critical.
pub fn pressure() -> u64 {
    sysctl_u64("kern.memorystatus_vm_pressure_level").unwrap_or(1)
}

#[cfg(not(target_os = "linux"))]
/// The machine's physical memory in bytes.
pub fn memsize() -> u64 {
    sysctl_u64("hw.memsize").unwrap_or(0)
}

#[cfg(not(target_os = "linux"))]
/// A process's command line (`KERN_PROCARGS2`), or its name.
pub fn command_line(pid: i32) -> String {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut buffer = vec![0u8; 256 * 1024];
    let mut size = buffer.len();
    // SAFETY: the kernel writes at most `size` bytes into the buffer.
    let ok = unsafe { libc::sysctl(mib.as_mut_ptr(), 3, buffer.as_mut_ptr().cast(), &mut size, std::ptr::null_mut(), 0) } == 0;
    if !ok || size < 4 {
        return name(pid);
    }
    let argc = i32::from_ne_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]).max(0) as usize;
    let rest = &buffer[4..size];
    // The executable path, the padding after it, then argc strings.
    let Some(path_end) = rest.iter().position(|byte| *byte == 0) else { return name(pid) };
    let mut at = path_end;
    while at < rest.len() && rest[at] == 0 {
        at += 1;
    }
    let args: Vec<String> = rest[at..].split(|byte| *byte == 0).take(argc).map(|arg| String::from_utf8_lossy(arg).into_owned()).collect();
    if args.is_empty() { name(pid) } else { args.join(" ") }
}

#[cfg(not(target_os = "linux"))]
fn name(pid: i32) -> String {
    let mut buffer = [0u8; 256];
    // SAFETY: the callee writes at most the buffer's size.
    let n = unsafe { libc::proc_name(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
    if n <= 0 { format!("pid {pid}") } else { String::from_utf8_lossy(&buffer[..n as usize]).into_owned() }
}

/// Linux: the pid, parent pid and process group of every process (`/proc/<pid>/stat`).
#[cfg(target_os = "linux")]
fn proc_table() -> Vec<(i32, i32, i32)> {
    let Ok(entries) = std::fs::read_dir("/proc") else { return Vec::new() };
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<i32>().ok())
        .filter_map(|pid| {
            let fields = stat_fields(pid)?;
            Some((pid, fields.get(1)?.parse().ok()?, fields.get(2)?.parse().ok()?))
        })
        .collect()
}

/// The fields of `/proc/<pid>/stat` after the command name (state first: index 0 is field 3).
#[cfg(target_os = "linux")]
fn stat_fields(pid: i32) -> Option<Vec<String>> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The name is in parentheses and may hold spaces and parentheses itself.
    let rest = &text[text.rfind(')')? + 1..];
    Some(rest.split_whitespace().map(str::to_owned).collect())
}

/// A run's processes: the members of process group `pgid` and all their descendants.
#[cfg(target_os = "linux")]
pub fn tree(pgid: i32) -> Vec<Proc> {
    if pgid <= 1 {
        return Vec::new();
    }
    let table = proc_table();
    let mut queue: Vec<i32> = table.iter().filter(|(_, _, group)| *group == pgid).map(|(pid, _, _)| *pid).collect();
    let mut seen = HashSet::new();
    while let Some(pid) = queue.pop() {
        if seen.insert(pid) {
            queue.extend(table.iter().filter(|(_, parent, _)| *parent == pid).map(|(child, _, _)| *child));
        }
    }
    let mut procs: Vec<Proc> = seen.into_iter().filter_map(proc_info).collect();
    procs.sort_by_key(|proc| proc.pid);
    procs
}

/// Linux: resident plus swapped memory (`VmRSS` + `VmSwap`), CPU time and start time of `pid`.
#[cfg(target_os = "linux")]
fn proc_info(pid: i32) -> Option<Proc> {
    let fields = stat_fields(pid)?;
    // SAFETY: sysconf only reads a constant.
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as u64;
    let field = |n: usize| fields.get(n - 3).and_then(|value| value.parse::<u64>().ok()).unwrap_or(0);
    let cpu_ns = (field(14) + field(15)).saturating_mul(1_000_000_000 / ticks);
    let start_s = boot_time() + field(22) / ticks;
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
    let kb = |key: &str| {
        status.lines().find_map(|line| line.strip_prefix(key)).and_then(|rest| rest.split_whitespace().next()?.parse::<u64>().ok()).unwrap_or(0)
    };
    Some(Proc { pid, footprint: (kb("VmRSS:") + kb("VmSwap:")) * 1024, cpu_ns, start_s })
}

/// Seconds since the epoch when the machine booted (`btime` in /proc/stat).
#[cfg(target_os = "linux")]
fn boot_time() -> u64 {
    static BOOT: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *BOOT.get_or_init(|| {
        let text = std::fs::read_to_string("/proc/stat").unwrap_or_default();
        text.lines().find_map(|line| line.strip_prefix("btime ")?.trim().parse().ok()).unwrap_or(0)
    })
}

/// Memory pressure on the macOS scale (1 normal, 2 warning, 4 critical), from the kernel's
/// pressure stall information: some task stalled on memory over the last 10 s (warning), or all
/// of them (critical).
#[cfg(target_os = "linux")]
pub fn pressure() -> u64 {
    let text = std::fs::read_to_string("/proc/pressure/memory").unwrap_or_default();
    let avg10 = |kind: &str| {
        text.lines()
            .find(|line| line.starts_with(kind))
            .and_then(|line| line.split_whitespace().find_map(|word| word.strip_prefix("avg10=")?.parse::<f64>().ok()))
            .unwrap_or(0.0)
    };
    if avg10("full") >= 10.0 {
        4
    } else if avg10("some") >= 10.0 {
        2
    } else {
        1
    }
}

/// The machine's physical memory in bytes.
#[cfg(target_os = "linux")]
pub fn memsize() -> u64 {
    // SAFETY: sysconf only reads constants.
    let (pages, page) = unsafe { (libc::sysconf(libc::_SC_PHYS_PAGES), libc::sysconf(libc::_SC_PAGESIZE)) };
    (pages.max(0) as u64).saturating_mul(page.max(0) as u64)
}

/// A process's command line (`/proc/<pid>/cmdline`), or its name.
#[cfg(target_os = "linux")]
pub fn command_line(pid: i32) -> String {
    let bytes = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    let args: Vec<String> = bytes.split(|byte| *byte == 0).filter(|arg| !arg.is_empty()).map(|arg| String::from_utf8_lossy(arg).into_owned()).collect();
    if !args.is_empty() {
        return args.join(" ");
    }
    std::fs::read_to_string(format!("/proc/{pid}/comm")).map(|name| name.trim().to_owned()).unwrap_or_else(|_| format!("pid {pid}"))
}

/// Bytes in words: `41 GB`, `3.2 GB`, `640 MB`.
pub fn size(bytes: u64) -> String {
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    let gb = bytes as f64 / GB;
    if gb >= 10.0 {
        format!("{gb:.0} GB")
    } else if gb >= 1.0 {
        format!("{gb:.1} GB")
    } else {
        format!("{} MB", bytes / (1024 * 1024))
    }
}

/// How long since `start_s`: `40 s`, `9 min`, `2 h 5 min`.
pub fn age(start_s: u64) -> String {
    let seconds = (crate::util::now_ms() / 1000).saturating_sub(start_s);
    match seconds {
        0..60 => format!("{seconds} s"),
        60..3600 => format!("{} min", seconds / 60),
        _ => format!("{} h {} min", seconds / 3600, (seconds % 3600) / 60),
    }
}

/// The process a run is really running (the largest that is not plumbing such as a shell or a
/// `tail`), in words: "`target/debug/deps/app-1f2e tests::` (9 min, 41 GB)". Paths under `cwd`
/// are shown relative to it.
pub fn describe(procs: &[Proc], cwd: &str) -> Option<String> {
    let mut named: Vec<(&Proc, String)> = procs.iter().map(|proc| (proc, command_line(proc.pid))).collect();
    named.sort_by_key(|(proc, _)| std::cmp::Reverse(proc.footprint));
    let program = |line: &str| line.split_whitespace().next().and_then(|first| first.rsplit('/').next()).unwrap_or("").to_owned();
    // The host's own runner and shell scripts are never what a run runs.
    named.retain(|(_, line)| !line.contains("/.cortexcode/jobs/") && !line.contains("/cc-tools/shells/"));
    let (proc, line) = named.iter().find(|(_, line)| !PLUMBING.contains(&program(line).as_str())).or(named.first())?;
    let prefix = format!("{}/", cwd.trim_end_matches('/'));
    let mut line = if cwd.is_empty() { line.clone() } else { line.replace(&prefix, "") };
    if line.chars().count() > 120 {
        line = format!("{}…", line.chars().take(119).collect::<String>());
    }
    Some(format!("`{line}` ({}, {})", age(proc.start_s), size(proc.footprint)))
}

/// Signal every process of a run (the group and the descendants that left it).
pub fn signal(pgid: i32, procs: &[Proc], signal: libc::c_int) {
    if pgid > 1 {
        // SAFETY: plain syscalls on processes this plugin started.
        unsafe {
            libc::killpg(pgid, signal);
        }
    }
    for proc in procs {
        if proc.pid > 1 {
            // SAFETY: as above.
            unsafe {
                libc::kill(proc.pid, signal);
            }
        }
    }
}

/// Stop a run: SIGTERM to all of it, SIGKILL to what is left after 1.5 s.
pub async fn stop(pgid: i32) {
    let procs = tree(pgid);
    signal(pgid, &procs, libc::SIGTERM);
    let deadline = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < deadline {
        if procs.iter().all(|proc| !alive(proc.pid)) && !alive(pgid) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    signal(pgid, &procs, libc::SIGKILL);
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks whether the process exists.
    pid > 1 && unsafe { libc::kill(pid, 0) } == 0
}

/// Whether any of the processes listens on a TCP port (a server waits for requests: idle, not hung).
fn serves(procs: &[Proc]) -> bool {
    if procs.is_empty() {
        return false;
    }
    let list: Vec<String> = procs.iter().map(|proc| proc.pid.to_string()).collect();
    std::process::Command::new("/usr/sbin/lsof")
        .args(["-nP", "-a", "-iTCP", "-sTCP:LISTEN", "-t", "-p", &list.join(",")])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .is_ok_and(|output| !output.stdout.is_empty())
}

/// The session shells' process groups. The shells sit behind their session's lock while a command
/// runs, so they tell the guard here; a shell the guard stopped finds why here.
#[derive(Default)]
pub struct ShellGroups {
    live: Mutex<HashMap<String, (i32, String)>>,
    stopped: Mutex<HashMap<i32, String>>,
}

impl ShellGroups {
    /// A session's shell started (process group `pgid`, commands run under `workspace`).
    pub fn started(&self, session: &str, pgid: i32, workspace: &str) {
        if let Ok(mut live) = self.live.lock() {
            live.insert(session.to_owned(), (pgid, workspace.to_owned()));
        }
    }

    /// The process group (the shell's pid) of a session's shell, while it runs.
    pub fn pid(&self, session: &str) -> Option<i32> {
        self.live.lock().ok()?.get(session).map(|(pgid, _)| *pgid)
    }

    fn list(&self) -> Vec<(String, i32, String)> {
        self.live.lock().map(|live| live.iter().map(|(session, (pgid, workspace))| (session.clone(), *pgid, workspace.clone())).collect()).unwrap_or_default()
    }

    fn forget(&self, session: &str, pgid: i32) {
        if let Ok(mut live) = self.live.lock()
            && live.get(session).is_some_and(|(live_pgid, _)| *live_pgid == pgid)
        {
            live.remove(session);
        }
    }

    fn mark(&self, pgid: i32, note: String) {
        if let Ok(mut stopped) = self.stopped.lock() {
            stopped.insert(pgid, note);
        }
    }

    /// Why the guard stopped the shell of process group `pgid`, once.
    pub fn take_stopped(&self, pgid: i32) -> Option<String> {
        self.stopped.lock().ok().and_then(|mut stopped| stopped.remove(&pgid))
    }
}

/// What the guard remembers of a run between ticks.
struct Seen {
    footprints: VecDeque<(Instant, u64)>,
    cpu_ns: u64,
    output: u64,
    active: Instant,
}

enum Owner {
    Job { job_id: String, output: u64 },
    Shell { session: String },
}

struct Run {
    owner: Owner,
    pgid: i32,
    cwd: String,
    procs: Vec<Proc>,
    footprint: u64,
    growing: bool,
}

pub struct Guard {
    jobs: Arc<Jobs>,
    shells: Arc<ShellGroups>,
    seen: Mutex<HashMap<i32, Seen>>,
    last_stop: Mutex<Option<Instant>>,
}

impl Guard {
    /// Start the guard (once per plugin instance).
    pub fn start(jobs: Arc<Jobs>, shells: Arc<ShellGroups>) {
        let guard = Arc::new(Self { jobs, shells, seen: Mutex::new(HashMap::new()), last_stop: Mutex::new(None) });
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(TICK).await;
                let this = guard.clone();
                let stop = tokio::task::spawn_blocking(move || this.decide()).await.ok().flatten();
                if let Some((run, note)) = stop {
                    guard.stop(run, note).await;
                }
            }
        });
    }

    /// Measure every run; the one to stop now, with its note.
    fn decide(&self) -> Option<(Run, String)> {
        let now = Instant::now();
        let mut runs = Vec::new();
        for job in self.jobs.running() {
            let output = self.jobs.output_len(&job);
            runs.push((Owner::Job { job_id: job.job_id.clone(), output }, job.pid, job.cwd.clone()));
        }
        for (session, pgid, workspace) in self.shells.list() {
            if !alive(pgid) {
                self.shells.forget(&session, pgid);
                continue;
            }
            runs.push((Owner::Shell { session }, pgid, workspace));
        }
        let mut seen = self.seen.lock().ok()?;
        let mut measured = Vec::new();
        let mut keep = HashSet::new();
        for (owner, pgid, cwd) in runs {
            let procs = tree(pgid);
            let footprint: u64 = procs.iter().map(|proc| proc.footprint).sum();
            let cpu_ns: u64 = procs.iter().map(|proc| proc.cpu_ns).sum();
            let output = match &owner {
                Owner::Job { output, .. } => *output,
                Owner::Shell { .. } => 0,
            };
            keep.insert(pgid);
            let entry = seen.entry(pgid).or_insert_with(|| Seen { footprints: VecDeque::new(), cpu_ns, output, active: now });
            // Doing something: a second of CPU or any output since it was last active.
            if cpu_ns.saturating_sub(entry.cpu_ns) >= 1_000_000_000 || output != entry.output {
                entry.active = now;
                entry.cpu_ns = cpu_ns;
                entry.output = output;
            }
            entry.footprints.push_back((now, footprint));
            while entry.footprints.len() > 2 && entry.footprints.get(1).is_some_and(|(at, _)| now.duration_since(*at) >= GROWTH_WINDOW) {
                entry.footprints.pop_front();
            }
            // Growing: more than a window ago by at least 5% (and at least 256 MB).
            let growing = entry.footprints.front().is_some_and(|(at, before)| {
                now.duration_since(*at) >= GROWTH_WINDOW && footprint > before.saturating_add((before / 20).max(256 * 1024 * 1024))
            });
            let idle_since = entry.active;
            measured.push((Run { owner, pgid, cwd, procs, footprint, growing }, idle_since));
        }
        seen.retain(|pgid, _| keep.contains(pgid));
        drop(seen);

        let memory = memsize();
        let level = pressure();
        let settled = self.last_stop.lock().ok().and_then(|last| *last).is_none_or(|at| now.duration_since(at) >= SETTLE);
        let largest = measured.iter().map(|(run, _)| run.footprint).max().unwrap_or(0);
        let sizeable = |run: &Run| memory > 0 && run.footprint >= memory / 16;
        // Memory first: it takes the machine down within minutes.
        let mut pick: Option<(usize, String)> = None;
        for (index, (run, _)) in measured.iter().enumerate() {
            let over_ram = memory > 0 && run.footprint > memory;
            let over_half = memory > 0 && run.footprint > memory / 2 && (run.growing || level >= 2);
            let short = settled && level >= 2 && run.growing && sizeable(run) && run.footprint == largest;
            let out = settled && level >= 4 && sizeable(run) && run.footprint == largest;
            if !(over_ram || over_half || short || out) {
                continue;
            }
            let culprit = describe(&run.procs, &run.cwd).unwrap_or_default();
            if over_ram || over_half {
                let how = if run.growing { "and kept growing" } else { "while the machine ran short of memory" };
                pick = Some((index, format!(
                    "[stopped by the host: its processes held {} of this machine's {} {how}; {culprit} held the most. The code it ran allocates without end: find the loop or the collection that keeps growing in the code it runs, fix that, then run it again.]",
                    size(run.footprint),
                    size(memory)
                )));
                break;
            }
            pick = Some((index, if short {
                format!(
                    "[stopped by the host: the machine ran short of memory while its processes grew to {}; {culprit} held the most. The code it ran allocates without end: find the loop or the collection that keeps growing in the code it runs, fix that, then run it again.]",
                    size(run.footprint)
                )
            } else {
                format!(
                    "[stopped by the host: the machine ran out of memory and this run held the most ({}; {culprit}). Make it need less memory (a smaller input, fewer tests at once) before running it again.]",
                    size(run.footprint)
                )
            }));
            break;
        }
        // Then a background job that has done nothing at all for an hour and serves no port.
        if pick.is_none() {
            for (index, (run, idle_since)) in measured.iter().enumerate() {
                if !matches!(run.owner, Owner::Job { .. }) || now.duration_since(*idle_since) < IDLE {
                    continue;
                }
                if serves(&run.procs) {
                    // A server waiting for requests: asked again in an hour.
                    if let Ok(mut seen) = self.seen.lock()
                        && let Some(entry) = seen.get_mut(&run.pgid)
                    {
                        entry.active = now;
                    }
                    continue;
                }
                pick = Some((index, format!(
                    "[stopped by the host: it printed nothing and used no CPU for {} min, so it was hung. Find what it waits for (a lock, a read, another process) before running it again.]",
                    IDLE.as_secs() / 60
                )));
                break;
            }
        }
        let (index, note) = pick?;
        let (run, _) = measured.swap_remove(index);
        Some((run, note))
    }

    async fn stop(&self, run: Run, note: String) {
        if let Ok(mut last) = self.last_stop.lock() {
            *last = Some(Instant::now());
        }
        match &run.owner {
            Owner::Job { job_id, .. } => {
                tracing::warn!(job_id, footprint = run.footprint, note, "stopping a background job");
                self.jobs.stop(job_id, &note, crate::jobs::StopMode::Escalate).await;
            }
            Owner::Shell { session } => {
                tracing::warn!(session, footprint = run.footprint, note, "stopping a command");
                self.shells.mark(run.pgid, note);
                stop(run.pgid).await;
            }
        }
    }
}
