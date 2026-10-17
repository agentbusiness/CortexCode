//! `cc-lang-python` executable: serve `lang@1` for Python to the frame (category `lang-python`).

fn main() {
    std::process::exit(cc_lang_kit::run_pack(cc_lang_python::PythonPack));
}
