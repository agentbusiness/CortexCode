//! `cc-lang-rust` executable: serve `lang@1` for Rust to the frame (category `lang-rust`).

fn main() {
    std::process::exit(cc_lang_kit::run_pack(cc_lang_rust::RustPack));
}
