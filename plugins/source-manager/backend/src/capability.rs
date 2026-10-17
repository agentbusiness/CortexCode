//! What this Mac can build: the tools a plugin's `[build] needs` names, found or not, and from that
//! which plugins' code can be edited here. Data is always editable (it needs no tools).

use crate::proc;
use cc_protocol::contracts::frame::package::PluginToml;
use cc_protocol::contracts::source::{PluginEditing, Toolchain};
use std::path::Path;

/// Plain names for the user.
fn label(need: &str) -> &'static str {
    match need {
        "rust" => "Rust",
        "apple-clt" => "Apple's command line tools",
        "metal" => "Apple's Metal compiler (Xcode)",
        "node" => "Node.js",
        _ => "an unknown tool",
    }
}

async fn run(program: &str, args: &[&str]) -> Option<String> {
    let exe = if program.contains('/') { Some(program.into()) } else { proc::which(program) }?;
    let mut cmd = proc::command(&exe, Path::new("/"));
    cmd.args(args);
    let captured = proc::capture(cmd).await.ok()?;
    if captured.ok() { Some(captured.first_line().unwrap_or_default()) } else { None }
}

/// Check every tool a build can need.
pub async fn detect() -> Vec<Toolchain> {
    let rust = run("cargo", &["--version"]).await;
    let clt = run("/usr/bin/xcode-select", &["-p"]).await;
    let metal = run("/usr/bin/xcrun", &["-sdk", "macosx", "--find", "metal"]).await;
    let node = match (run("node", &["--version"]).await, proc::which("npm")) {
        (Some(version), Some(_)) => Some(version),
        _ => None,
    };
    [("rust", rust), ("apple-clt", clt), ("metal", metal), ("node", node)]
        .into_iter()
        .map(|(name, found)| Toolchain {
            name: name.into(),
            label: label(name).into(),
            found: found.is_some(),
            detail: match (&found, name) {
                (None, "metal") => "install Xcode from the App Store".into(),
                (None, "apple-clt") => "run `xcode-select --install`".into(),
                (None, "rust") => "install Rust from rustup.rs".into(),
                (None, _) => "install Node.js".into(),
                (Some(_), _) => String::new(),
            },
            version: found.filter(|line| !line.is_empty()),
        })
        .collect()
}

/// What a plugin's build needs: its `[build] needs`, plus Node.js when it has UI packages to build.
pub fn needs(manifest: &PluginToml) -> Vec<String> {
    let mut needs: Vec<String> = manifest.build.as_ref().map(|b| b.needs.clone()).unwrap_or_default();
    if manifest.ui.as_ref().is_some_and(|ui| !ui.package.is_empty()) && !needs.iter().any(|n| n == "node") {
        needs.push("node".into());
    }
    needs
}

/// What is missing on this Mac to build a plugin, as plain names.
pub fn missing(manifest: &PluginToml, tools: &[Toolchain]) -> Vec<String> {
    needs(manifest)
        .iter()
        .filter(|need| !tools.iter().any(|tool| &tool.name == *need && tool.found))
        .map(|need| label(need).to_owned())
        .collect()
}

pub fn editing(id: &str, manifest: &PluginToml, tools: &[Toolchain]) -> PluginEditing {
    let missing = missing(manifest, tools);
    PluginEditing {
        id: id.to_owned(),
        name: if manifest.plugin.name.is_empty() { id.to_owned() } else { manifest.plugin.name.clone() },
        data_editable: true,
        code_editable: missing.is_empty(),
        needs: needs(manifest),
        missing,
    }
}
