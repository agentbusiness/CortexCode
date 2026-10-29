//! Images: the variant's vision projector (PrismML's mmproj through llama.cpp's `mtmd`) on its own
//! thread, so an encode never holds up the engine's other conversations.
//!
//! - **On demand.** The projector loads when an image is first measured or encoded, and unloads
//!   after [`IDLE_UNLOAD`] without one. It runs on the GPU when the device has room for it beside
//!   the engine (the engine's pool is wired at load), else on the CPU.
//! - **Measure.** An image's row count (what the prompt reserves for it) comes from the projector's
//!   own preprocessing, so the prompt and the encoder always agree. Kept per image digest.
//! - **Encode.** The image's rows and their positions, handed back through a callback (the engine's
//!   command channel). The last few encodings are kept per digest.

use crate::llama::{self, Model, Vision};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

/// Default pixel budget per image (about 2,340 rows of 32×32 px); larger images are downscaled.
pub const DEFAULT_MAX_IMAGE_PIXELS: u64 = 2_400_000;
/// Pixels one image row covers (patch 16 × spatial merge 2, squared).
pub const PIXELS_PER_ROW: u64 = 1024;
/// The projector is unloaded after this long without an image.
pub const IDLE_UNLOAD: Duration = Duration::from_secs(300);
/// Encodings kept for reuse (a conversation that lost its cache reads its images again).
const KEPT_ENCODINGS: usize = 4;
/// What the encoder needs on the GPU beside the projector's weights (compute buffers for an image at
/// the pixel budget).
const ENCODER_RESERVE: u64 = 1 << 30;

/// What an image takes in the prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shape {
    pub rows: usize,
    /// Positions the image advances the sequence by (fewer than its rows with M-RoPE).
    pub n_pos: u32,
}

/// An encoded image: its rows and their positions relative to its first.
pub struct Encoded {
    /// `rows × n_embd` floats.
    pub embd: Vec<f32>,
    /// (t, x, y, z) per row.
    pub positions: Vec<i32>,
    pub rows: usize,
    pub n_pos: u32,
    pub n_embd: usize,
    pub mrope: bool,
}

pub type EncodeDone = Box<dyn FnOnce(Result<Arc<Encoded>, String>) + Send>;

enum Job {
    Measure { path: PathBuf, sha256: String, reply: tokio::sync::oneshot::Sender<Result<Shape, String>> },
    Encode { path: PathBuf, sha256: String, done: EncodeDone },
}

pub struct VisionHandle {
    tx: Option<mpsc::Sender<Job>>,
    loaded: Arc<AtomicBool>,
    pub max_image_pixels: u64,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl VisionHandle {
    /// Start the worker for the projector at `mmproj` (loaded on first use).
    pub fn start(mmproj: PathBuf, model: Arc<Model>, max_image_pixels: u64) -> Result<VisionHandle, String> {
        let (tx, rx) = mpsc::channel::<Job>();
        let loaded = Arc::new(AtomicBool::new(false));
        let flag = loaded.clone();
        let max_rows = u32::try_from(max_image_pixels / PIXELS_PER_ROW).unwrap_or(u32::MAX).max(8);
        let thread = std::thread::Builder::new()
            .name("bonsai-vision".into())
            .spawn(move || Worker { mmproj, model, max_rows, vision: None, loaded: flag, shapes: HashMap::new(), kept: VecDeque::new() }.run(rx))
            .map_err(|error| format!("cannot start the vision thread: {error}"))?;
        Ok(VisionHandle { tx: Some(tx), loaded, max_image_pixels, thread: Some(thread) })
    }

    /// The projector is in memory now.
    pub fn loaded(&self) -> bool {
        self.loaded.load(Ordering::SeqCst)
    }

    /// The rows `path` takes in a prompt.
    pub async fn measure(&self, path: PathBuf, sha256: String) -> Result<Shape, String> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.send(Job::Measure { path, sha256, reply })?;
        rx.await.map_err(|_| "the vision thread stopped".to_owned())?
    }

    /// Encode `path`; `done` gets the rows (on the vision thread).
    pub fn encode(&self, path: PathBuf, sha256: String, done: EncodeDone) -> Result<(), String> {
        self.send(Job::Encode { path, sha256, done })
    }

    fn send(&self, job: Job) -> Result<(), String> {
        self.tx.as_ref().ok_or("the vision thread stopped")?.send(job).map_err(|_| "the vision thread stopped".to_owned())
    }
}

impl Drop for VisionHandle {
    fn drop(&mut self) {
        // Closing the channel ends the worker, which frees the projector.
        self.tx.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Worker {
    mmproj: PathBuf,
    model: Arc<Model>,
    max_rows: u32,
    vision: Option<Vision>,
    loaded: Arc<AtomicBool>,
    shapes: HashMap<String, Shape>,
    kept: VecDeque<(String, Arc<Encoded>)>,
}

impl Worker {
    fn run(mut self, rx: mpsc::Receiver<Job>) {
        loop {
            let job = if self.vision.is_some() {
                match rx.recv_timeout(IDLE_UNLOAD) {
                    Ok(job) => job,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        tracing::info!("no image for a while; unloading the vision projector");
                        self.vision = None;
                        self.loaded.store(false, Ordering::SeqCst);
                        continue;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            } else {
                match rx.recv() {
                    Ok(job) => job,
                    Err(_) => break,
                }
            };
            match job {
                Job::Measure { path, sha256, reply } => {
                    let _ = reply.send(self.measure(&path, &sha256));
                }
                Job::Encode { path, sha256, done } => done(self.encode(&path, &sha256)),
            }
        }
        self.vision = None;
        self.loaded.store(false, Ordering::SeqCst);
    }

    fn vision(&mut self) -> Result<&mut Vision, String> {
        if self.vision.is_none() {
            let bytes = std::fs::metadata(&self.mmproj).map(|meta| meta.len()).unwrap_or(0);
            // The GPU when it has room beside the engine, else the CPU (slower, never over budget).
            let gpu = llama::gpu().is_some_and(|gpu| gpu.working_set_bytes.saturating_sub(gpu.allocated_bytes) >= bytes + ENCODER_RESERVE);
            let vision = Vision::load(&self.mmproj, self.model.clone(), gpu, self.max_rows)?;
            tracing::info!(gpu, path = %self.mmproj.display(), "vision projector loaded");
            self.loaded.store(true, Ordering::SeqCst);
            self.vision = Some(vision);
        }
        self.vision.as_mut().ok_or_else(|| "the vision projector is not loaded".to_owned())
    }

    fn prepare(&mut self, path: &PathBuf) -> Result<llama::ImageChunk, String> {
        let bytes = std::fs::read(path).map_err(|error| format!("cannot read image {}: {error}", path.display()))?;
        let image = image::ImageReader::new(std::io::Cursor::new(&bytes))
            .with_guessed_format()
            .map_err(|error| format!("cannot read image {}: {error}", path.display()))?
            .decode()
            .map_err(|error| format!("cannot decode image {} (PNG, JPEG, WebP, GIF or BMP expected): {error}", path.display()))?
            .to_rgb8();
        let (width, height) = image.dimensions();
        self.vision()?.prepare(image.as_raw(), width, height)
    }

    fn measure(&mut self, path: &PathBuf, sha256: &str) -> Result<Shape, String> {
        if let Some(shape) = self.shapes.get(sha256) {
            return Ok(*shape);
        }
        let chunk = self.prepare(path)?;
        let shape = Shape { rows: chunk.n_tokens, n_pos: chunk.n_pos };
        self.shapes.insert(sha256.to_owned(), shape);
        Ok(shape)
    }

    fn encode(&mut self, path: &PathBuf, sha256: &str) -> Result<Arc<Encoded>, String> {
        if let Some((_, encoded)) = self.kept.iter().find(|(key, _)| key == sha256) {
            return Ok(encoded.clone());
        }
        let chunk = self.prepare(path)?;
        let started = std::time::Instant::now();
        let vision = self.vision()?;
        let embd = vision.encode(&chunk)?;
        let encoded = Arc::new(Encoded {
            embd,
            positions: chunk.positions.clone(),
            rows: chunk.n_tokens,
            n_pos: chunk.n_pos,
            n_embd: vision.n_embd(),
            mrope: vision.mrope(),
        });
        tracing::info!(rows = encoded.rows, encode_ms = started.elapsed().as_millis() as u64, "image encoded");
        self.shapes.insert(sha256.to_owned(), Shape { rows: encoded.rows, n_pos: encoded.n_pos });
        self.kept.push_back((sha256.to_owned(), encoded.clone()));
        while self.kept.len() > KEPT_ENCODINGS {
            self.kept.pop_front();
        }
        Ok(encoded)
    }
}
