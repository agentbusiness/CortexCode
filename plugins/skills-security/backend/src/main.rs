//! `cc-skills-security` executable: serve `skills@1` to the frame (category `skills-security`)
//! from the folder's `data/skills`. The secaudit tool beside them (`data/scripts`) is run by the
//! model through `run`; this process never touches it.

/// The plugin's id: its category is `skills-<ID>`, as plugin.toml says.
const ID: &str = "security";
const NAME: &str = "Security";

fn main() {
    std::process::exit(cc_lang_kit::run_skills_plugin(ID, NAME, env!("CARGO_PKG_VERSION"), None));
}
