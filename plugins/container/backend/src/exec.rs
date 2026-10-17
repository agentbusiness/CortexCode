//! Running one command process (a `docker exec`) with streamed output, a timeout and cancellation.
//!
//! stdout and stderr are read concurrently; every chunk becomes an [`ExecItem`] and is appended to
//! the interleaved output in the order it was read. A chunk boundary never splits a UTF-8 sequence
//! (incomplete sequences wait for the next chunk; invalid bytes become U+FFFD).
//!
//! On timeout or cancellation the caller's `stop` runs (for docker: kill the command's processes
//! inside the container, which also ends the `docker exec` client), then the engine waits for the
//! process to exit, repeating `stop` up to `stop_attempts` times, and finally kills the client
//! process itself. Killing only the client would leave the command running in the container.

use cc_protocol::BoxFuture;
use cc_protocol::contracts::container::{ExecItem, ExecResult};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub const STDOUT: &str = "stdout";
pub const STDERR: &str = "stderr";

pub struct Run {
    /// Program and arguments; the engine sets stdin/stdout/stderr.
    pub command: tokio::process::Command,
    pub stdin: Option<String>,
    pub timeout: Option<Duration>,
    /// Bytes of output kept in the result (the most recent ones); items carry everything.
    pub max_output: usize,
    /// How long to wait for the process to exit after each `stop`.
    pub stop_wait: Duration,
    pub stop_attempts: u32,
}

/// Called to stop the command on timeout or cancellation; gets the client process id.
pub type Stop = Box<dyn FnMut(Option<u32>) -> BoxFuture<'static, ()> + Send>;

/// Incremental UTF-8 decoding across chunk boundaries.
#[derive(Default)]
pub struct Utf8Carry(Vec<u8>);

impl Utf8Carry {
    pub fn push(&mut self, bytes: &[u8]) -> String {
        self.0.extend_from_slice(bytes);
        let mut out = String::new();
        loop {
            match std::str::from_utf8(&self.0) {
                Ok(text) => {
                    out.push_str(text);
                    self.0.clear();
                    return out;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    out.push_str(std::str::from_utf8(&self.0[..valid]).unwrap_or_default());
                    match error.error_len() {
                        // An incomplete sequence at the end: keep it for the next chunk.
                        None => {
                            self.0.drain(..valid);
                            return out;
                        }
                        Some(len) => {
                            out.push('\u{FFFD}');
                            self.0.drain(..valid + len);
                        }
                    }
                }
            }
        }
    }

    /// Whatever is left at end of stream (an incomplete sequence becomes U+FFFD).
    pub fn finish(&mut self) -> String {
        let out = String::from_utf8_lossy(&self.0).into_owned();
        self.0.clear();
        out
    }
}

/// The interleaved output, keeping at most `cap` bytes (the latest).
pub struct Collected {
    text: String,
    dropped: u64,
    cap: usize,
}

impl Collected {
    pub fn new(cap: usize) -> Self {
        Self {
            text: String::new(),
            dropped: 0,
            cap: cap.max(1),
        }
    }

    pub fn push(&mut self, chunk: &str) {
        self.text.push_str(chunk);
        // Trim in batches (at half a cap over), not on every chunk.
        if self.text.len() > self.cap + self.cap / 2 {
            let mut cut = self.text.len() - self.cap;
            while !self.text.is_char_boundary(cut) {
                cut += 1;
            }
            self.dropped += cut as u64;
            self.text.drain(..cut);
        }
    }

    pub fn finish(mut self) -> String {
        if self.text.len() > self.cap {
            let mut cut = self.text.len() - self.cap;
            while !self.text.is_char_boundary(cut) {
                cut += 1;
            }
            self.dropped += cut as u64;
            self.text.drain(..cut);
        }
        if self.dropped > 0 {
            format!(
                "[{} bytes of earlier output omitted]\n{}",
                self.dropped, self.text
            )
        } else {
            self.text
        }
    }
}

fn reader<R: AsyncRead + Unpin + Send + 'static>(
    mut pipe: R,
    stream: &'static str,
    tx: mpsc::UnboundedSender<(&'static str, Vec<u8>)>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut buffer = vec![0_u8; 16 * 1024];
        loop {
            match pipe.read(&mut buffer).await {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    if tx.send((stream, buffer[..n].to_vec())).is_err() {
                        return;
                    }
                }
            }
        }
    })
}

#[derive(PartialEq)]
enum Ending {
    Done,
    TimedOut,
    Cancelled,
}

struct Sink<'a> {
    emit: &'a (dyn Fn(ExecItem) + Send + Sync),
    out: Utf8Carry,
    err: Utf8Carry,
    collected: Collected,
}

impl Sink<'_> {
    fn chunk(&mut self, stream: &'static str, bytes: &[u8]) {
        let text = if stream == STDOUT {
            self.out.push(bytes)
        } else {
            self.err.push(bytes)
        };
        self.text(stream, text);
    }

    fn text(&mut self, stream: &'static str, text: String) {
        if !text.is_empty() {
            self.collected.push(&text);
            (self.emit)(ExecItem {
                stream: stream.into(),
                text,
            });
        }
    }

    fn finish(mut self) -> String {
        let out = self.out.finish();
        self.text(STDOUT, out);
        let err = self.err.finish();
        self.text(STDERR, err);
        self.collected.finish()
    }
}

/// Run to completion, timeout or cancellation. `Err` only when the process cannot be started.
pub async fn run(
    spec: Run,
    emit: &(dyn Fn(ExecItem) + Send + Sync),
    cancel: CancellationToken,
    mut stop: Stop,
) -> Result<ExecResult, String> {
    let Run {
        mut command,
        stdin,
        timeout,
        max_output,
        stop_wait,
        stop_attempts,
    } = spec;
    command
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let started = Instant::now();
    let mut child = command.spawn().map_err(|e| e.to_string())?;
    let pid = child.id();
    if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
        tokio::spawn(async move {
            // A command that does not read its input closes the pipe; that is not an error.
            let _ = pipe.write_all(text.as_bytes()).await;
            let _ = pipe.shutdown().await;
        });
    }
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut readers = Vec::new();
    if let Some(out) = child.stdout.take() {
        readers.push(reader(out, STDOUT, tx.clone()));
    }
    if let Some(err) = child.stderr.take() {
        readers.push(reader(err, STDERR, tx.clone()));
    }
    drop(tx);
    let mut sink = Sink {
        emit,
        out: Utf8Carry::default(),
        err: Utf8Carry::default(),
        collected: Collected::new(max_output),
    };

    let deadline = timeout.map(|t| tokio::time::Instant::now() + t);
    let mut streams_open = true;
    let mut status: Option<std::process::ExitStatus> = None;
    let ending = loop {
        if !streams_open && status.is_some() {
            break Ending::Done;
        }
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break Ending::Cancelled,
            _ = async { tokio::time::sleep_until(deadline.unwrap_or_else(tokio::time::Instant::now)).await }, if deadline.is_some() => break Ending::TimedOut,
            chunk = rx.recv(), if streams_open => match chunk {
                Some((stream, bytes)) => sink.chunk(stream, &bytes),
                None => streams_open = false,
            },
            exited = child.wait(), if status.is_none() => match exited {
                Ok(exited) => status = Some(exited),
                Err(error) => return Err(format!("waiting for the command: {error}")),
            },
        }
    };

    if ending != Ending::Done {
        let mut exited = false;
        for _ in 0..stop_attempts.max(1) {
            stop(pid).await;
            if status.is_some() {
                exited = true;
                break;
            }
            if let Ok(Ok(_)) = tokio::time::timeout(stop_wait, child.wait()).await {
                exited = true;
                break;
            }
        }
        if !exited {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        // Output written before the kill is still in the pipes: read until they close (the client
        // is gone), bounded in case another process keeps a pipe open.
        let drain_until = tokio::time::Instant::now() + stop_wait;
        while streams_open {
            match tokio::time::timeout_at(drain_until, rx.recv()).await {
                Ok(Some((stream, bytes))) => sink.chunk(stream, &bytes),
                Ok(None) | Err(_) => streams_open = false,
            }
        }
    }
    for reader in readers {
        reader.abort();
    }
    let output = sink.finish();
    Ok(ExecResult {
        exit_code: if ending == Ending::Done {
            status.and_then(|s| s.code())
        } else {
            None
        },
        output,
        duration_ms: started.elapsed().as_millis() as u64,
        timed_out: ending == Ending::TimedOut,
        cancelled: ending == Ending::Cancelled,
    })
}
