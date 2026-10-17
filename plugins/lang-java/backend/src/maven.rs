//! pom.xml: the project's coordinates (`groupId:artifactId`, the group inherited from `<parent>`
//! when absent), its dependencies by scope, managed dependencies and build plugins, and their
//! versions in the local Maven repository.
//!
//! Structural: the POM's XML elements (POM 4.0.0), `${property}` references resolved from the
//! POM's own `<properties>` and `project.version` / `project.groupId`. A version is reported as
//! installed only when `~/.m2/repository/<group path>/<artifact>/<version>` is a directory (that
//! directory holds jars, not sources, so no source directory is claimed).

use cc_lang_kit::cc_protocol::contracts::lang::{DependencyFact, FileFacts};
use roxmltree::{Document, Node};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The local repository (`~/.m2/repository`), read once.
fn local_repository() -> Option<&'static Path> {
    static REPO: OnceLock<Option<PathBuf>> = OnceLock::new();
    REPO.get_or_init(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".m2").join("repository")).filter(|p| p.is_dir())).as_deref()
}

pub fn extract(text: &str, facts: &mut FileFacts) {
    extract_with(text, local_repository(), facts);
}

fn child<'a, 'i>(node: Node<'a, 'i>, name: &str) -> Option<Node<'a, 'i>> {
    node.children().find(|c| c.is_element() && c.tag_name().name() == name)
}

fn child_text<'a>(node: Node<'a, '_>, name: &str) -> Option<&'a str> {
    child(node, name).and_then(|c| c.text()).map(str::trim).filter(|t| !t.is_empty())
}

/// `${name}` references replaced from `properties` (unknown ones stay as written).
fn resolve(value: &str, properties: &HashMap<String, String>) -> String {
    let mut out = String::new();
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find('}') {
            Some(end) => {
                let name = &after[..end];
                match properties.get(name) {
                    Some(v) => out.push_str(v),
                    None => out.push_str(&rest[start..start + 2 + end + 1]),
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// Maven scopes as `DependencyFact` sections.
fn section(scope: Option<&str>, optional: bool) -> &'static str {
    if optional {
        return "optional";
    }
    match scope {
        Some("test") => "dev",
        Some("provided") => "provided",
        Some("runtime") => "runtime",
        Some("system") => "system",
        Some("import") => "import",
        _ => "dependencies",
    }
}

/// [`extract`] with an explicit local repository.
pub fn extract_with(text: &str, repository: Option<&Path>, facts: &mut FileFacts) {
    let Ok(doc) = Document::parse(text) else {
        facts.parse_errors = 1;
        return;
    };
    let project = doc.root_element();
    if project.tag_name().name() != "project" {
        facts.parse_errors = 1;
        return;
    }
    let parent = child(project, "parent");
    let group = child_text(project, "groupId").or_else(|| parent.and_then(|p| child_text(p, "groupId")));
    let artifact = child_text(project, "artifactId");
    let version = child_text(project, "version").or_else(|| parent.and_then(|p| child_text(p, "version")));
    let mut properties: HashMap<String, String> = HashMap::new();
    if let Some(props) = child(project, "properties") {
        for prop in props.children().filter(|c| c.is_element()) {
            properties.insert(prop.tag_name().name().to_owned(), prop.text().unwrap_or("").trim().to_owned());
        }
    }
    for (key, value) in [("project.groupId", group), ("project.artifactId", artifact), ("project.version", version)] {
        if let Some(value) = value {
            properties.insert(key.to_owned(), value.to_owned());
        }
    }
    facts.package = match (group, artifact) {
        (Some(g), Some(a)) => Some(format!("{g}:{a}")),
        (None, Some(a)) => Some(a.to_owned()),
        _ => None,
    };
    let make = |dep: Node, section: &str| -> Option<DependencyFact> {
        let (Some(g), Some(a)) = (child_text(dep, "groupId"), child_text(dep, "artifactId")) else { return None };
        let (g, a) = (resolve(g, &properties), resolve(a, &properties));
        let spec = child_text(dep, "version").map(|v| resolve(v, &properties)).unwrap_or_else(|| "managed".into());
        let installed = repository
            .filter(|_| !spec.contains("${") && !spec.starts_with(['[', '(']) && spec != "managed")
            .map(|repo| repo.join(g.replace('.', "/")).join(&a).join(&spec))
            .filter(|dir| dir.is_dir())
            .map(|_| spec.clone());
        Some(DependencyFact { name: format!("{g}:{a}"), ecosystem: "maven".into(), spec, section: section.to_owned(), installed_version: installed, source_dir: None })
    };
    if let Some(deps) = child(project, "dependencies") {
        for dep in deps.children().filter(|c| c.has_tag_name("dependency")) {
            let optional = child_text(dep, "optional") == Some("true");
            facts.dependencies.extend(make(dep, section(child_text(dep, "scope"), optional)));
        }
    }
    if let Some(deps) = child(project, "dependencyManagement").and_then(|m| child(m, "dependencies")) {
        for dep in deps.children().filter(|c| c.has_tag_name("dependency")) {
            facts.dependencies.extend(make(dep, "managed"));
        }
    }
    if let Some(plugins) = child(project, "build").and_then(|b| child(b, "plugins")) {
        for plugin in plugins.children().filter(|c| c.has_tag_name("plugin")) {
            // A plugin's groupId defaults to org.apache.maven.plugins (POM reference).
            if child_text(plugin, "groupId").is_none() {
                let a = child_text(plugin, "artifactId").map(str::to_owned);
                let spec = child_text(plugin, "version").map(|v| resolve(v, &properties)).unwrap_or_else(|| "managed".into());
                if let Some(a) = a {
                    facts.dependencies.push(DependencyFact { name: format!("org.apache.maven.plugins:{a}"), ecosystem: "maven".into(), spec, section: "build".into(), installed_version: None, source_dir: None });
                }
                continue;
            }
            facts.dependencies.extend(make(plugin, "build"));
        }
    }
}
