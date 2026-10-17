//! Development driver: run memory@1 methods in-process (no frame). Usage:
//! devrun <home> <root> [method params-json]...
use cc_memory::service::Memory;
use std::path::PathBuf;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).with_writer(std::io::stderr).init();
    let mut args = std::env::args().skip(1);
    let home = PathBuf::from(args.next().expect("home"));
    let root = args.next().expect("root");
    let memory = Memory::new(home.clone(), home.join("plugins-data/cc-memory"));
    let t = std::time::Instant::now();
    let status = memory.clone().dispatch("memory.project.open", serde_json::json!({"root": root, "project_id": "dev"})).await;
    println!("open {:?} -> {}", t.elapsed(), serde_json::to_string(&status.map_err(|e| e.to_string())).unwrap());
    let rest: Vec<String> = args.collect();
    for pair in rest.chunks(2) {
        let method = &pair[0];
        let mut params: serde_json::Value = serde_json::from_str(pair.get(1).map(String::as_str).unwrap_or("{}")).expect("json");
        params["project_id"] = serde_json::json!("dev");
        let t = std::time::Instant::now();
        let out = memory.clone().dispatch(method, params).await;
        match out {
            Ok(v) => println!("{method} {:?}\n{}", t.elapsed(), serde_json::to_string_pretty(&v).unwrap()),
            Err(e) => println!("{method} ERROR {e}"),
        }
    }
    memory.close().await;
}
