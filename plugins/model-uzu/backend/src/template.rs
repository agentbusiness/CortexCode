//! Shared engine-neutral template utilities.
pub use cc_model_kit::template::*;

/// Templates shipped with the plugin (see `templates/README.md`).
pub fn template_source(name: &str) -> Option<&'static str> {
    match name {
        "qwen3.8.jinja" => Some(include_str!("../templates/qwen3.8.jinja")),
        _ => None,
    }
}
