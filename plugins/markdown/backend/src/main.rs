//! `cc-lang-markdown` executable: serve `lang@1` for Markdown documents to the frame (category
//! `lang-markdown`).

fn main() {
    std::process::exit(cc_lang_kit::run_pack(cc_lang_markdown::MarkdownPack));
}
