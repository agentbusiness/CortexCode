//! `cc-lang-web` executable: serve `lang@1` for HTML and CSS to the frame (category `lang-web`), or,
//! as `cc-lang-web lsp-router`, the language-server router the plugin starts per workspace.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("lsp-router") {
        std::process::exit(cc_lang_web::router::main(&args[1..]));
    }
    std::process::exit(cc_lang_kit::run_pack(cc_lang_web::WebPack));
}
