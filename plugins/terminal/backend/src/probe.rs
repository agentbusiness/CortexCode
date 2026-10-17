//! `plugin.probe`: run a real shell on a real PTY and check that it echoes, resizes and closes.
//! The probe uses its own terminal set whose events stay local (never published to the frame).

use crate::{Terminals, login_shell};
use base64::Engine;
use cc_protocol::contracts::lifecycle::{ProbeCheck, ProbeReport};
use cc_protocol::contracts::terminal::{OpenParams, ResizeParams, WriteParams, topics};
use serde_json::Value;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(10);

async fn wait_screen(terms: &Terminals, id: &str, needle: &str) -> Result<String, String> {
    let deadline = Instant::now() + WAIT;
    loop {
        let snapshot = terms.snapshot(id).map_err(|e| e.to_string())?;
        if snapshot.text.contains(needle) {
            return Ok(snapshot.text);
        }
        if Instant::now() > deadline {
            return Err(format!("`{needle}` did not appear; screen: {:?}", snapshot.text));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn check(name: &str, started: Instant, outcome: Result<String, String>) -> ProbeCheck {
    let ms = started.elapsed().as_millis() as u64;
    match outcome {
        Ok(detail) => ProbeCheck { name: name.into(), ok: true, detail, ms },
        Err(detail) => ProbeCheck { name: name.into(), ok: false, detail, ms },
    }
}

pub async fn run() -> ProbeReport {
    let output_bytes = Arc::new(Mutex::new(Vec::<u8>::new()));
    let exits = Arc::new(AtomicUsize::new(0));
    let (sink, exit_count) = (output_bytes.clone(), exits.clone());
    let emit = Arc::new(move |topic: &str, data: Value| {
        if topic == topics::OUTPUT {
            let decoded = data
                .get("data")
                .and_then(Value::as_str)
                .and_then(|d| base64::engine::general_purpose::STANDARD.decode(d).ok())
                .unwrap_or_default();
            if let Ok(mut all) = sink.lock() {
                all.extend_from_slice(&decoded);
            }
        } else if topic == topics::EXIT {
            exit_count.fetch_add(1, Ordering::SeqCst);
        }
    });
    let terms = Terminals::new(emit);
    let mut checks = Vec::new();

    let started = Instant::now();
    let shell = login_shell();
    checks.push(check("login_shell", started, Ok(shell)));

    let started = Instant::now();
    let cwd = std::env::temp_dir().to_string_lossy().into_owned();
    let opened = terms.open(OpenParams { cwd, cols: 80, rows: 24, shell: Some("/bin/sh".into()), args: None });
    let info = match opened {
        Ok(info) => info,
        Err(error) => {
            checks.push(check("open", started, Err(error.to_string())));
            return ProbeReport::from_checks(checks);
        }
    };
    checks.push(check("open", started, Ok(format!("pid {}", info.pid))));
    let id = info.term_id.clone();

    // The shell computes 6*7; the typed input only contains the expression, so "probe-42" on the
    // screen proves a program ran on the PTY and its output went through the emulator.
    let started = Instant::now();
    let echoed = async {
        terms
            .write(WriteParams { term_id: id.clone(), data: "echo cc-probe-$((6*7))\n".into() })
            .await
            .map_err(|e| e.to_string())?;
        wait_screen(&terms, &id, "cc-probe-42").await?;
        let deadline = Instant::now() + WAIT;
        loop {
            let seen = output_bytes.lock().map(|b| String::from_utf8_lossy(&b).contains("cc-probe-42")).unwrap_or(false);
            if seen {
                return Ok("output event and screen agree".to_owned());
            }
            if Instant::now() > deadline {
                return Err("no terminal.output event carried the output".to_owned());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    .await;
    checks.push(check("echo", started, echoed));

    let started = Instant::now();
    let resized = async {
        terms.resize(ResizeParams { term_id: id.clone(), cols: 101, rows: 33 }).map_err(|e| e.to_string())?;
        terms
            .write(WriteParams { term_id: id.clone(), data: "stty size\n".into() })
            .await
            .map_err(|e| e.to_string())?;
        wait_screen(&terms, &id, "33 101").await.map(|_| "stty size: 33 101".to_owned())
    }
    .await;
    checks.push(check("resize", started, resized));

    let started = Instant::now();
    let closed = async {
        let info = terms.close(&id).await.map_err(|e| e.to_string())?;
        // SAFETY: signal 0 only checks whether the process exists.
        let alive = unsafe { libc::kill(info.pid as libc::pid_t, 0) } == 0;
        let deadline = Instant::now() + WAIT;
        while exits.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        if info.alive || alive {
            Err(format!("shell {} still running after close", info.pid))
        } else if exits.load(Ordering::SeqCst) != 1 {
            Err("terminal.exit was not published".to_owned())
        } else {
            Ok(format!("exit code {:?}", info.exit_code))
        }
    }
    .await;
    checks.push(check("close", started, closed));
    terms.close_all().await;
    ProbeReport::from_checks(checks)
}
