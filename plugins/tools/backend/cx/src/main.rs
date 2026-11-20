//! `cx` entry point.

use std::io::Write;

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("cx: cannot start: {error}");
            std::process::exit(1);
        }
    };
    // `cx mcp`: a long-running MCP server on stdio (a frontier worker's CLI starts it).
    if argv.first().map(String::as_str) == Some("mcp") {
        let code = runtime.block_on(cx::mcp::serve());
        std::process::exit(code);
    }
    let mut inputs = cx::args::Inputs::default();
    let outcome = runtime.block_on(cx::run(argv, &mut inputs));
    let _ = std::io::stdout().write_all(outcome.stdout.as_bytes());
    let _ = std::io::stderr().write_all(outcome.stderr.as_bytes());
    let _ = std::io::stdout().flush();
    std::process::exit(outcome.code);
}
