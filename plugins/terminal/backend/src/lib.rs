//! `cc-terminal`: PTY terminals for the CortexCode `terminal@1` contract.
//!
//! Each terminal is the user's login shell on its own pseudo-terminal. A reader thread feeds every
//! output byte both to a VT emulator (`vt100`, for `terminal.snapshot`) and to a pump task that
//! publishes `terminal.output` events, coalescing bytes that arrive within about 16 ms into one
//! event. A waiter thread reports the shell's exit as `terminal.exit`.
//!
//! Terminals belong to the plugin instance that opened them: closing (or shutting the instance
//! down) hangs up the shell's process group and the foreground job, then kills whatever ignored it.

pub mod probe;

use base64::Engine;
use cc_protocol::contracts::terminal::{
    ExitEvent, ListResult, OpenParams, OutputEvent, ResizeParams, Snapshot, TermInfo, TermParams, WriteParams, topics,
};
use cc_protocol::{RpcError, codes, to_value};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde_json::Value;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::sync::{mpsc, watch};

/// Output arriving within this window after the first unpublished byte goes into one event.
pub const COALESCE: Duration = Duration::from_millis(16);
/// A batch this large is published immediately instead of waiting for the window to end.
const FLUSH_BYTES: usize = 1 << 20;
/// The emulator keeps no scrollback: snapshots are the visible screen (the contract), and every
/// byte of output is delivered in `terminal.output` events, where the UI keeps its own history.
const SCROLLBACK: usize = 0;
/// Largest screen accepted. The emulator allocates every cell up front, so an absurd size from a
/// caller (e.g. 65535x65535, four billion cells) would exhaust memory instead of drawing anything.
const MAX_CELLS: u32 = 4_000_000;
/// How long a hung-up shell gets to exit before it is killed.
const HANGUP_GRACE: Duration = Duration::from_millis(1500);

/// Publishes an event (topic, data). The plugin routes this to the frame.
pub type Emit = Arc<dyn Fn(&str, Value) + Send + Sync>;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// How the shell ended.
#[derive(Debug, Clone, PartialEq)]
pub struct Exit {
    pub code: i32,
    pub signal: Option<String>,
}

struct Term {
    id: String,
    cwd: String,
    shell: String,
    pid: u32,
    size: Mutex<(u16, u16)>,
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    writer: Arc<Mutex<Option<Box<dyn Write + Send>>>>,
    parser: Arc<Mutex<vt100::Parser>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    exit: watch::Receiver<Option<Exit>>,
}

impl Term {
    fn info(&self) -> TermInfo {
        let (cols, rows) = *lock(&self.size);
        let exit = self.exit.borrow().clone();
        TermInfo {
            term_id: self.id.clone(),
            cwd: self.cwd.clone(),
            pid: self.pid,
            cols,
            rows,
            alive: exit.is_none(),
            shell: self.shell.clone(),
            exit_code: exit.map(|e| e.code),
        }
    }

    /// Process groups to signal: the shell's own (it is a session leader, so its pgid is its pid)
    /// and the terminal's current foreground group (a running job such as `vim` or `sleep`).
    fn groups(&self) -> Vec<libc::pid_t> {
        let mut groups = Vec::new();
        if let Ok(pid) = libc::pid_t::try_from(self.pid)
            && pid > 0
        {
            groups.push(pid);
        }
        if let Some(master) = lock(&self.master).as_ref()
            && let Some(foreground) = master.process_group_leader()
            && foreground > 0
            && !groups.contains(&foreground)
        {
            groups.push(foreground);
        }
        groups
    }

    async fn wait_exit(&self, limit: Duration) -> bool {
        let mut exit = self.exit.clone();
        tokio::time::timeout(limit, exit.wait_for(Option::is_some)).await.is_ok()
    }

    fn session(&self) -> libc::pid_t {
        libc::pid_t::try_from(self.pid).unwrap_or(0)
    }

    async fn close(&self) {
        let sid = self.session();
        let shell_alive = self.exit.borrow().is_none();
        if shell_alive || !session_members(sid).is_empty() {
            let groups = self.groups();
            signal(&groups, libc::SIGHUP);
            // The shell forwards the hang-up to its jobs; give everything in the session a moment.
            let deadline = tokio::time::Instant::now() + HANGUP_GRACE;
            while tokio::time::Instant::now() < deadline {
                if self.exit.borrow().is_none() || !session_members(sid).is_empty() {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                } else {
                    break;
                }
            }
            // Whatever ignored the hang-up (still in the terminal's session) is killed.
            let remaining = session_members(sid);
            if self.exit.borrow().is_none() || !remaining.is_empty() {
                signal(&groups, libc::SIGKILL);
                for pid in remaining {
                    // SAFETY: plain syscall on a pid that belongs to this terminal's session.
                    unsafe {
                        libc::kill(pid, libc::SIGKILL);
                    }
                }
                let _ = lock(&self.killer).kill();
                self.wait_exit(Duration::from_secs(5)).await;
            }
        }
        // Closing the master hangs up the line for anything that still holds the slave side.
        lock(&self.writer).take();
        lock(&self.master).take();
    }
}

/// Processes whose session is `sid` (the shell is a session leader, so its pid is the session id;
/// members keep it after the leader exits). Processes that started their own session (daemons,
/// `setsid`) deliberately left the terminal and are not included.
fn session_members(sid: libc::pid_t) -> Vec<libc::pid_t> {
    if sid <= 0 {
        return Vec::new();
    }
    all_pids()
        .into_iter()
        // SAFETY: getsid only reads kernel process state.
        .filter(|pid| *pid > 0 && unsafe { libc::getsid(*pid) } == sid)
        .collect()
}

#[cfg(target_os = "macos")]
fn all_pids() -> Vec<libc::pid_t> {
    // SAFETY: proc_listallpids fills at most `buffersize` bytes and returns the number of pids.
    unsafe {
        let count = libc::proc_listallpids(std::ptr::null_mut(), 0);
        if count <= 0 {
            return Vec::new();
        }
        let mut pids: Vec<libc::pid_t> = vec![0; count as usize + 256];
        let bytes = (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
        let filled = libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes);
        pids.truncate(filled.max(0) as usize);
        pids
    }
}

#[cfg(not(target_os = "macos"))]
fn all_pids() -> Vec<libc::pid_t> {
    std::fs::read_dir("/proc")
        .map(|entries| {
            entries.flatten().filter_map(|entry| entry.file_name().to_str().and_then(|name| name.parse().ok())).collect()
        })
        .unwrap_or_default()
}

fn signal(groups: &[libc::pid_t], signo: libc::c_int) {
    for group in groups {
        // SAFETY: plain syscall; a negative pid addresses the whole process group.
        unsafe {
            libc::kill(-group, signo);
        }
    }
}

/// The user's login shell: `$SHELL` when it is executable, else the passwd entry, else `/bin/sh`.
pub fn login_shell() -> String {
    if let Ok(shell) = std::env::var("SHELL")
        && is_executable(Path::new(&shell))
    {
        return shell;
    }
    // SAFETY: getpwuid returns a pointer into static storage or null; we copy the string at once.
    let from_passwd = unsafe {
        let entry = libc::getpwuid(libc::getuid());
        if entry.is_null() || (*entry).pw_shell.is_null() {
            None
        } else {
            Some(std::ffi::CStr::from_ptr((*entry).pw_shell).to_string_lossy().into_owned())
        }
    };
    match from_passwd {
        Some(shell) if is_executable(Path::new(&shell)) => shell,
        _ => "/bin/sh".into(),
    }
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.is_absolute() && std::fs::metadata(path).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

/// All terminals of one plugin instance.
pub struct Terminals {
    terms: Mutex<HashMap<String, Arc<Term>>>,
    emit: Emit,
}

fn not_found(term_id: &str) -> RpcError {
    RpcError::new(codes::NOT_FOUND, format!("no terminal `{term_id}`"))
}

fn check_size(cols: u16, rows: u16) -> Result<(), RpcError> {
    if cols == 0 || rows == 0 {
        return Err(RpcError::invalid_params("cols and rows must be at least 1"));
    }
    if u32::from(cols) * u32::from(rows) > MAX_CELLS {
        return Err(RpcError::invalid_params(format!("{cols}x{rows} exceeds {MAX_CELLS} cells")));
    }
    Ok(())
}

impl Terminals {
    pub fn new(emit: Emit) -> Self {
        Self { terms: Mutex::new(HashMap::new()), emit }
    }

    fn get(&self, term_id: &str) -> Result<Arc<Term>, RpcError> {
        lock(&self.terms).get(term_id).cloned().ok_or_else(|| not_found(term_id))
    }

    pub fn open(&self, params: OpenParams) -> Result<TermInfo, RpcError> {
        check_size(params.cols, params.rows)?;
        let cwd = PathBuf::from(&params.cwd);
        if !cwd.is_absolute() || !cwd.is_dir() {
            return Err(RpcError::invalid_params(format!("cwd `{}` is not an existing absolute directory", params.cwd)));
        }
        let shell = match params.shell {
            Some(shell) if !shell.is_empty() => shell,
            _ => login_shell(),
        };
        let pair = native_pty_system()
            .openpty(PtySize { rows: params.rows, cols: params.cols, pixel_width: 0, pixel_height: 0 })
            .map_err(|e| RpcError::internal(format!("cannot open a pseudo-terminal: {e}")))?;
        let mut command = CommandBuilder::new(&shell);
        match &params.args {
            Some(args) => command.args(args),
            None => command.arg("-l"),
        }
        command.cwd(&cwd);
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        command.env("TERM_PROGRAM", "CortexCode");
        command.env("PWD", &cwd);
        // Plugin plumbing is not the shell's business (CORTEX_HOME stays, so `cx` finds the frame).
        for var in ["CORTEX_FRAME_SOCKET", "CORTEX_PLUGIN_INSTANCE", "CORTEX_PLUGIN_DATA", "CORTEX_LOG"] {
            command.env_remove(var);
        }
        let mut child = pair
            .slave
            .spawn_command(command)
            .map_err(|e| RpcError::internal(format!("cannot start `{shell}`: {e}")))?;
        // Only the shell holds the slave side now; its last close is what ends our reads.
        drop(pair.slave);
        let pid = child.process_id().unwrap_or(0);
        let killer = child.clone_killer();
        let reader = pair.master.try_clone_reader().map_err(|e| RpcError::internal(format!("pty reader: {e}")))?;
        let writer = pair.master.take_writer().map_err(|e| RpcError::internal(format!("pty writer: {e}")))?;

        let id = format!("term-{}", uuid::Uuid::new_v4().simple());
        let parser = Arc::new(Mutex::new(vt100::Parser::new(params.rows, params.cols, SCROLLBACK)));
        let (exit_tx, exit_rx) = watch::channel(None);
        let (chunk_tx, chunk_rx) = mpsc::unbounded_channel::<Vec<u8>>();

        spawn_reader(&id, reader, parser.clone(), chunk_tx)?;
        let waiter_exit = exit_tx.clone();
        std::thread::Builder::new()
            .name(format!("{id}-wait"))
            .spawn(move || {
                let exit = match child.wait() {
                    Ok(status) => Exit {
                        code: i32::try_from(status.exit_code()).unwrap_or(i32::MAX),
                        signal: status.signal().map(str::to_owned),
                    },
                    Err(error) => Exit { code: -1, signal: Some(format!("wait failed: {error}")) },
                };
                let _ = waiter_exit.send(Some(exit));
            })
            .map_err(|e| RpcError::internal(format!("cannot start the waiter thread: {e}")))?;
        tokio::spawn(pump(id.clone(), chunk_rx, exit_rx.clone(), self.emit.clone()));

        let term = Arc::new(Term {
            id: id.clone(),
            cwd: cwd.to_string_lossy().into_owned(),
            shell,
            pid,
            size: Mutex::new((params.cols, params.rows)),
            master: Mutex::new(Some(pair.master)),
            writer: Arc::new(Mutex::new(Some(writer))),
            parser,
            killer: Mutex::new(killer),
            exit: exit_rx,
        });
        let info = term.info();
        lock(&self.terms).insert(id, term);
        Ok(info)
    }

    pub async fn write(&self, params: WriteParams) -> Result<(), RpcError> {
        let term = self.get(&params.term_id)?;
        if term.exit.borrow().is_some() {
            return Err(RpcError::new(codes::CONFLICT, format!("terminal `{}` has exited", params.term_id)));
        }
        let writer = term.writer.clone();
        let data = params.data.into_bytes();
        // A full PTY input buffer blocks the write until the program reads; keep that off the runtime.
        tokio::task::spawn_blocking(move || -> Result<(), RpcError> {
            let mut guard = lock(&writer);
            let writer = guard.as_mut().ok_or_else(|| RpcError::new(codes::CONFLICT, "terminal is closed"))?;
            writer.write_all(&data).and_then(|()| writer.flush()).map_err(|e| RpcError::new(codes::CONFLICT, format!("write failed: {e}")))
        })
        .await
        .map_err(|e| RpcError::internal(format!("write task failed: {e}")))?
    }

    pub fn resize(&self, params: ResizeParams) -> Result<TermInfo, RpcError> {
        check_size(params.cols, params.rows)?;
        let term = self.get(&params.term_id)?;
        {
            let master = lock(&term.master);
            let master = master.as_ref().ok_or_else(|| RpcError::new(codes::CONFLICT, "terminal is closed"))?;
            // TIOCSWINSZ on the master; the kernel delivers SIGWINCH to the foreground group.
            master
                .resize(PtySize { rows: params.rows, cols: params.cols, pixel_width: 0, pixel_height: 0 })
                .map_err(|e| RpcError::internal(format!("resize failed: {e}")))?;
        }
        lock(&term.parser).screen_mut().set_size(params.rows, params.cols);
        *lock(&term.size) = (params.cols, params.rows);
        Ok(term.info())
    }

    pub async fn close(&self, term_id: &str) -> Result<TermInfo, RpcError> {
        let term = self.get(term_id)?;
        term.close().await;
        lock(&self.terms).remove(term_id);
        Ok(term.info())
    }

    pub fn list(&self) -> ListResult {
        let mut terminals: Vec<TermInfo> = lock(&self.terms).values().map(|t| t.info()).collect();
        terminals.sort_by(|a, b| a.term_id.cmp(&b.term_id));
        ListResult { terminals }
    }

    pub fn snapshot(&self, term_id: &str) -> Result<Snapshot, RpcError> {
        let term = self.get(term_id)?;
        let parser = lock(&term.parser);
        let screen = parser.screen();
        let (cursor_row, cursor_col) = screen.cursor_position();
        let (rows, cols) = screen.size();
        Ok(Snapshot {
            term_id: term_id.to_owned(),
            text: screen.contents(),
            cursor_row,
            cursor_col,
            cols,
            rows,
            alive: term.exit.borrow().is_none(),
            alternate_screen: screen.alternate_screen(),
        })
    }

    /// Close every terminal (plugin shutdown); hang-ups run concurrently.
    pub async fn close_all(&self) {
        let terms: Vec<Arc<Term>> = lock(&self.terms).drain().map(|(_, t)| t).collect();
        let mut tasks = Vec::new();
        for term in terms {
            tasks.push(tokio::spawn(async move { term.close().await }));
        }
        for task in tasks {
            let _ = task.await;
        }
    }

    /// Serve one `terminal.*` method.
    pub async fn dispatch(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        use cc_protocol::contracts::terminal::methods;
        use cc_protocol::parse_params;
        match method {
            methods::OPEN => to_value(&self.open(parse_params(params)?)?),
            methods::WRITE => {
                self.write(parse_params(params)?).await?;
                Ok(Value::Null)
            }
            methods::RESIZE => to_value(&self.resize(parse_params(params)?)?),
            methods::CLOSE => {
                let p: TermParams = parse_params(params)?;
                to_value(&self.close(&p.term_id).await?)
            }
            methods::LIST => to_value(&self.list()),
            methods::SNAPSHOT => {
                let p: TermParams = parse_params(params)?;
                to_value(&self.snapshot(&p.term_id)?)
            }
            other => Err(RpcError::unknown_method(other)),
        }
    }
}

fn spawn_reader(
    id: &str,
    mut reader: Box<dyn Read + Send>,
    parser: Arc<Mutex<vt100::Parser>>,
    chunks: mpsc::UnboundedSender<Vec<u8>>,
) -> Result<(), RpcError> {
    std::thread::Builder::new()
        .name(format!("{id}-read"))
        .spawn(move || {
            let mut buffer = vec![0u8; 64 * 1024];
            loop {
                match reader.read(&mut buffer) {
                    // EOF, or EIO once the last slave descriptor is closed (macOS and Linux).
                    Ok(0) => break,
                    Ok(n) => {
                        let bytes = &buffer[..n];
                        // The emulator sees bytes before they are published, so a snapshot taken
                        // after an output event always reflects that output.
                        lock(&parser).process(bytes);
                        if chunks.send(bytes.to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        })
        .map(|_| ())
        .map_err(|e| RpcError::internal(format!("cannot start the reader thread: {e}")))
}

/// Publish output in ~16 ms batches, then the exit once the shell has ended and its output is out.
async fn pump(id: String, mut chunks: mpsc::UnboundedReceiver<Vec<u8>>, mut exit: watch::Receiver<Option<Exit>>, emit: Emit) {
    let engine = base64::engine::general_purpose::STANDARD;
    let mut seq = 0u64;
    let mut pending: Vec<u8> = Vec::new();
    let mut deadline: Option<tokio::time::Instant> = None;
    let mut reader_done = false;
    let mut exit_seen: Option<Exit> = None;
    let mut exit_published = false;
    let mut flush = |pending: &mut Vec<u8>| {
        if pending.is_empty() {
            return;
        }
        seq += 1;
        let event = OutputEvent { term_id: id.clone(), data: engine.encode(&pending[..]), seq };
        if let Ok(value) = serde_json::to_value(&event) {
            emit(topics::OUTPUT, value);
        }
        pending.clear();
    };
    let publish_exit = |exit: &Exit| {
        let event = ExitEvent { term_id: id.clone(), exit_code: exit.code, signal: exit.signal.clone() };
        if let Ok(value) = serde_json::to_value(&event) {
            emit(topics::EXIT, value);
        }
    };
    loop {
        if reader_done && exit_seen.is_some() {
            break;
        }
        let wait_deadline = async {
            match deadline {
                Some(at) => tokio::time::sleep_until(at).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            chunk = chunks.recv(), if !reader_done => match chunk {
                Some(chunk) => {
                    if pending.is_empty() {
                        deadline = Some(tokio::time::Instant::now() + COALESCE);
                    }
                    pending.extend_from_slice(&chunk);
                    if pending.len() >= FLUSH_BYTES {
                        flush(&mut pending);
                        deadline = None;
                    }
                }
                None => {
                    reader_done = true;
                    flush(&mut pending);
                    deadline = None;
                }
            },
            () = wait_deadline => {
                flush(&mut pending);
                deadline = None;
            }
            value = async { exit.wait_for(Option::is_some).await.ok().and_then(|v| v.clone()) }, if exit_seen.is_none() => {
                exit_seen = Some(value.unwrap_or(Exit { code: -1, signal: None }));
                // Give the reader a moment to drain the shell's last words before announcing the
                // exit; a background job that keeps the PTY open must not delay it indefinitely.
                let grace = tokio::time::sleep(Duration::from_millis(150));
                tokio::pin!(grace);
                loop {
                    tokio::select! {
                        chunk = chunks.recv(), if !reader_done => match chunk {
                            Some(chunk) => pending.extend_from_slice(&chunk),
                            None => { reader_done = true; break; }
                        },
                        () = &mut grace => break,
                    }
                }
                flush(&mut pending);
                deadline = None;
                if let Some(exit) = &exit_seen {
                    publish_exit(exit);
                    exit_published = true;
                }
            }
        }
    }
    flush(&mut pending);
    if !exit_published && let Some(exit) = &exit_seen {
        publish_exit(exit);
    }
}
