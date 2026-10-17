//! `cc-lang-go` executable: serve `lang@1` for Go to the frame (category `lang-go`).

fn main() {
    let home = cc_lang_kit::install::cortex_home();
    std::process::exit(cc_lang_kit::run_pack(cc_lang_go::GoPack::new(&home)));
}
