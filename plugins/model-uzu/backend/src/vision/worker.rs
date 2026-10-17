//! The vision encoder's own thread. It holds its own uzu engine context (so its own Metal command
//! queue: an encode runs on the GPU concurrently with the language model's forward passes) and the
//! loaded tower; jobs (read the file, check its digest, preprocess, encode) arrive over a channel
//! and results are delivered to the engine worker, which keeps scheduling the other conversations
//! meanwhile. The thread exists exactly while the tower is loaded: stopping it drops the tower and
//! releases its context's buffers.

use crate::engine::Failure;
use crate::vision::VisionConfig;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Instant;
use uzu_engine::backends::Backend;
use uzu_engine::engine::Engine;

/// One image to encode.
#[derive(Debug, Clone)]
pub struct Job {
    /// `sha256:rowsxcolumns`, the embedding cache key.
    pub key: String,
    pub path: PathBuf,
    pub sha256: String,
    /// The merged token grid the prompt planned.
    pub grid: (u32, u32),
    pub max_pixels: u64,
}

/// What the thread reports.
#[derive(Debug, Clone)]
pub enum Message {
    Loaded { load_ms: f64 },
    LoadFailed(String),
    Encoded { key: String, result: Result<Arc<[u16]>, Failure>, preprocess_ms: f64, encode_ms: f64 },
}

pub type Deliver = Arc<dyn Fn(Message) + Send + Sync>;

pub struct VisionWorker {
    jobs: Option<mpsc::Sender<Job>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl VisionWorker {
    /// Start the thread and load the tower from `weights` (reported by `Loaded` or `LoadFailed`).
    pub fn spawn<B: Backend>(weights: PathBuf, config: Arc<VisionConfig>, deliver: Deliver) -> Result<Self, String> {
        let (jobs, receiver) = mpsc::channel::<Job>();
        let thread = std::thread::Builder::new()
            .name("uzu-vision".into())
            .spawn(move || run::<B>(weights, config, receiver, deliver))
            .map_err(|error| format!("cannot start the vision thread: {error}"))?;
        Ok(Self { jobs: Some(jobs), thread: Some(thread) })
    }

    pub fn submit(&self, job: Job) -> Result<(), String> {
        self.jobs.as_ref().ok_or("the vision thread is stopping")?.send(job).map_err(|_| "the vision thread has stopped".to_owned())
    }

    /// Stop the thread (after the job it is running) and wait for it: the tower is released.
    pub fn stop(mut self) {
        self.jobs.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for VisionWorker {
    fn drop(&mut self) {
        self.jobs.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run<B: Backend>(weights: PathBuf, config: Arc<VisionConfig>, jobs: mpsc::Receiver<Job>, deliver: Deliver) {
    let started = Instant::now();
    let loaded = Engine::<B>::new().map_err(|error| format!("cannot start an engine for the vision tower: {error}")).and_then(|engine| {
        engine
            .load_vision_model(&weights, config.engine())
            .map(|model| (engine, model))
            .map_err(|error| format!("cannot load the vision tower: {error}"))
    });
    let (engine, model) = match loaded {
        Ok(pair) => pair,
        Err(error) => {
            deliver(Message::LoadFailed(error));
            return;
        }
    };
    deliver(Message::Loaded { load_ms: started.elapsed().as_secs_f64() * 1000.0 });
    while let Ok(job) = jobs.recv() {
        let started = Instant::now();
        let prepared = (|| -> Result<crate::vision::preprocess::Patches, Failure> {
            let bytes = std::fs::read(&job.path)
                .map_err(|error| Failure::Invalid(format!("cannot read image {}: {error}", job.path.display())))?;
            let digest: String = Sha256::digest(&bytes).iter().map(|byte| format!("{byte:02x}")).collect();
            if !job.sha256.is_empty() && !digest.eq_ignore_ascii_case(&job.sha256) {
                return Err(Failure::Invalid(format!(
                    "image {} has SHA-256 {digest}, the request says {}",
                    job.path.display(),
                    job.sha256
                )));
            }
            let patches = crate::vision::preprocess::preprocess(&bytes, &config, job.max_pixels).map_err(Failure::Invalid)?;
            if patches.merged != job.grid {
                return Err(Failure::Invalid(format!(
                    "image {} gives a {}x{} token grid; the prompt planned {}x{} (are its width and height right?)",
                    job.path.display(),
                    patches.merged.0,
                    patches.merged.1,
                    job.grid.0,
                    job.grid.1
                )));
            }
            Ok(patches)
        })();
        let preprocess_ms = started.elapsed().as_secs_f64() * 1000.0;
        let started = Instant::now();
        let result = prepared.and_then(|patches| {
            model
                .encode(&patches.values, patches.grid)
                .map(|rows| rows.iter().map(|value| value.to_bits()).collect::<Arc<[u16]>>())
                .map_err(|error| Failure::Engine(format!("image encoding failed: {error}")))
        });
        let encode_ms = started.elapsed().as_secs_f64() * 1000.0;
        deliver(Message::Encoded { key: job.key, result, preprocess_ms, encode_ms });
    }
    drop(model);
    engine.trim_buffer_cache();
}
