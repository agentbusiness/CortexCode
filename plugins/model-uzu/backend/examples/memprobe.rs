//! Development probe: device memory while a conversation state grows and after it is dropped.
//! Usage: memprobe <model_dir> <tokens>
use std::time::{Duration, Instant};
use uzu_engine::backends::{Backend, BackendSelection, select_backend};
use uzu_engine::engine::Engine;
use uzu_engine::engine::language_model::stream::SamplingMethod;

struct Run(std::path::PathBuf, usize);

impl BackendSelection for Run {
    type Output = ();
    type Error = String;
    fn select<B: Backend>(self) -> impl Future<Output = Result<(), String>> + Send {
        async move {
            let engine = Engine::<B>::new().await.map_err(|e| e.to_string())?;
            let model = engine.load_language_model(&self.0).map_err(|e| e.to_string())?;
            let mb = |e: &Engine<B>| e.device_facts().allocated_bytes.unwrap_or(0) as f64 / 1048576.0;
            println!("after load {:.0} MiB", mb(&engine));
            for round in 0..3 {
                let mut state = model.create_empty_state(Some(65536), 1).map_err(|e| e.to_string())?;
                let ids: Vec<u64> = (0..self.1 as u64).map(|i| 1000 + (i * 7919) % 50000).collect();
                for chunk in ids.chunks(1024) {
                    let mut opts = model.default_stream_options();
                    opts.sampling_method = SamplingMethod::Greedy;
                    let s = model.stream(chunk, &mut state, opts).map_err(|e| e.to_string())?;
                    drop(s);
                    state.clear_pending_output();
                }
                println!("round {round}: state with {} tokens: {:.0} MiB", state.tokens().len(), mb(&engine));
                let snapshot = model
                    .snapshot_state(&state, uzu_engine::engine::language_model::state::SnapshotMode::Full)
                    .map_err(|e| e.to_string())?;
                println!("round {round}: with a full snapshot: {:.0} MiB", mb(&engine));
                let mut bytes = Vec::new();
                snapshot.write_to(&mut bytes).map_err(|e| e.to_string())?;
                drop(snapshot);
                println!("round {round}: snapshot written ({} MiB) and dropped: {:.0} MiB", bytes.len() / 1048576, mb(&engine));
                drop(state);
                println!("round {round}: trimmed {} cached buffers", engine.trim_buffer_cache());
                let t = Instant::now();
                while t.elapsed() < Duration::from_secs(3) {
                    std::thread::sleep(Duration::from_millis(500));
                }
                println!("round {round}: 3 s after dropping it: {:.0} MiB", mb(&engine));
            }
            Ok(())
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let r = futures::executor::block_on(select_backend(Run(args[1].clone().into(), args[2].parse().unwrap_or(16384)), "unknown backend".into()));
    println!("{r:?}");
}
