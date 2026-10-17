//! `cc-lang-java` executable: serve `lang@1` for Java to the frame (category `lang-java`).

fn main() {
    std::process::exit(cc_lang_kit::run_pack(cc_lang_java::JavaPack));
}
