//! `cc-skills-engineering` executable: serve `skills@1` to the frame (category `skills-engineering`)
//! from the folder's `data/skills`.

/// The plugin's id: its category is `skills-<ID>`, as plugin.toml says.
const ID: &str = "engineering";
const NAME: &str = "Engineering";

fn main() {
    std::process::exit(cc_lang_kit::run_skills_plugin(ID, NAME, env!("CARGO_PKG_VERSION"), None));
}
