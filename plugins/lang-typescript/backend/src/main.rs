//! `cc-lang-typescript` executable: serve `lang@1` for TypeScript and JavaScript to the frame
//! (category `lang-typescript`).

fn main() {
    // The frame sets CORTEX_HOME for every plugin; the server's bundled TypeScript lives under it.
    let home = cc_lang_kit::install::cortex_home();
    std::process::exit(cc_lang_kit::run_pack(cc_lang_typescript::TypeScriptPack::new(home)));
}
