//! Development probe: load a Mirai model directory and stream a few tokens (prints timings).
//! Usage: probe <model_dir> <n_tokens>
use std::time::Instant;
use uzu_engine::backends::{Backend, BackendSelection, select_backend};
use uzu_engine::engine::Engine;
use uzu_engine::engine::language_model::stream::SamplingMethod;

struct Run(std::path::PathBuf, usize);

impl BackendSelection for Run {
    type Output = ();
    type Error = String;
    fn select<B: Backend>(self) -> impl Future<Output = Result<(), String>> + Send {
        async move {
            let t = Instant::now();
            let engine = Engine::<B>::new().await.map_err(|e| e.to_string())?;
            let model = engine.load_language_model(&self.0).map_err(|e| e.to_string())?;
            println!("load {:?} max_ctx {:?} rec {:?} spec {}", t.elapsed(), model.max_context_length(), model.recommended_context_length(), model.speculation_supported());
            let tok = model.tokenizer().clone();
            let prompt = "<|im_start|>user\nName the three primary colors of light.<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\n";
            let ids: Vec<u64> = tok.encode(prompt, false).map_err(|e| e.to_string())?.get_ids().iter().map(|&i| u64::from(i)).collect();
            let mut state = model.create_empty_state(Some(4096), 1).map_err(|e| e.to_string())?;
            let mut opts = model.default_stream_options();
            opts.sampling_method = SamplingMethod::Greedy;
            let t = Instant::now();
            let mut out = Vec::new();
            {
                let mut stream = model.stream(&ids, &mut state, opts).map_err(|e| e.to_string())?;
                for _ in 0..self.1 {
                    match stream.generate().await.transpose() { Some(Ok(t)) => out.push(t as u32), Some(Err(e)) => return Err(e.to_string()), None => break }
                    if out.len() == 1 { println!("first token after {:?}", t.elapsed()); }
                }
                println!("metrics {:?}", stream.metrics());
            }
            println!("{} tokens in {:?}: {:?}", out.len(), t.elapsed(), tok.decode(&out, false).unwrap_or_default());
            println!("state tokens {}", state.tokens().len());
            Ok(())
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let r = futures::executor::block_on(select_backend(Run(args[1].clone().into(), args[2].parse().unwrap_or(16)), "unknown backend".into()));
    println!("{r:?}");
}
