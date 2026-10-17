//! `cc-skills-data` executable: serve `skills@1` to the frame (category `skills-data`)
//! from the folder's `data/skills`.

/// The plugin's id: its category is `skills-<ID>`, as plugin.toml says.
const ID: &str = "data";
const NAME: &str = "Data";

fn main() {
    std::process::exit(cc_lang_kit::run_skills_plugin(ID, NAME, env!("CARGO_PKG_VERSION"), None));
}
