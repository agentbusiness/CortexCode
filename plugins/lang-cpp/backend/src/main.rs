//! `cc-lang-cpp` executable: serve `lang@1` for C and C++ to the frame (category `lang-cpp`).

fn main() {
    let home = cc_lang_kit::install::cortex_home();
    std::process::exit(cc_lang_kit::run_pack(cc_lang_cpp::CppPack::new(&home)));
}
