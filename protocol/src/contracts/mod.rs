//! Versioned category contracts (CPP/1).
//!
//! A contract is `<category>@<major>`. A plugin declares exactly one category and contract in its
//! hello; the frame routes every method named `<category>.<verb>` to the active plugin of that
//! category. Contracts evolve additively within a major version: new optional fields (with
//! `#[serde(default)]`) and new methods only. Renaming or removing anything requires a new major.

pub mod agents;
pub mod browser;
pub mod chatflow;
pub mod container;
pub mod frame;
pub mod lang;
pub mod skills;
pub mod lifecycle;
pub mod memory;
pub mod model;
pub mod notify;
pub mod research;
pub mod source;
pub mod terminal;
pub mod tools;
pub mod vcs;

/// Every category this frame knows, with the contract major it speaks.
pub const CATEGORIES: &[(&str, &str)] = &[
    ("model", "model@1"),
    ("memory", "memory@1"),
    ("tools", "tools@1"),
    ("vcs", "vcs@1"),
    ("terminal", "terminal@1"),
    ("chatflow", "chatflow@1"),
    ("agents", "agents@1"),
    ("browser", "browser@1"),
    ("source", "source@1"),
    ("notify", "notify@1"),
    ("container", "container@1"),
    ("research", "research@1"),
];

/// Families: any number of plugins, each in its own category `<family>-<name>` (lowercase letters,
/// digits and `-`), all speaking the family's contract. One language plugin per language:
/// `lang-rust`, `lang-go`, ... all serve `lang@1`; one skills plugin per body of know-how:
/// `skills-engineering`, ... all serve `skills@1`.
pub const FAMILIES: &[(&str, &str)] = &[("lang", "lang@1"), ("skills", "skills@1")];

/// The contract a category speaks (`vcs` -> `vcs@1`, `lang-go` -> `lang@1`), None when unknown.
pub fn contract_of(category: &str) -> Option<&'static str> {
    if let Some((_, contract)) = CATEGORIES.iter().find(|(known, _)| *known == category) {
        return Some(contract);
    }
    FAMILIES.iter().find_map(|(family, contract)| {
        let name = category.strip_prefix(family)?.strip_prefix('-')?;
        let valid = !name.is_empty() && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        valid.then_some(*contract)
    })
}

/// The family of a category in one (`lang-go` -> `lang`), None for a single category.
pub fn family_of(category: &str) -> Option<&'static str> {
    FAMILIES.iter().map(|(family, _)| *family).find(|family| category.strip_prefix(family).is_some_and(|rest| rest.starts_with('-')))
}

/// Category of a method name (`model.chat` -> `model`).
pub fn category_of(method: &str) -> &str {
    method.split('.').next().unwrap_or(method)
}

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}
