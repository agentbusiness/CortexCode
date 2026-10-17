//! `cc-lang-shell` executable: serve `lang@1` for shell scripts to the frame (category `lang-shell`).

fn main() {
    let pack = cc_lang_shell::ShellPack::new(cc_lang_kit::install::cortex_home());
    std::process::exit(cc_lang_kit::run_pack(pack));
}
