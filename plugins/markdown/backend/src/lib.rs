//! The Markdown language pack (`lang-markdown`): a repository's prose (README, `docs/`, ADRs,
//! changelogs) for the code graph. Every heading is a `section` symbol with its own text, nested by
//! level, and every backtick identifier in a section's prose is a `Mention` reference the code graph
//! links to the one symbol of that name. No language server, no verifiers, no skills: documents are
//! indexed so that what a project says about itself is searchable and bindable, beside its code
//! (docs/PROJECT-KNOWLEDGE-PLAN.md P1).

pub mod sections;

use cc_lang_kit::cc_protocol::contracts::lang::{FileFacts, LanguageInfo, SourceFile, Verifier};
use cc_lang_kit::lsp::ServerSpec;
use cc_lang_kit::{ExtractContext, LanguagePack, Sample};

pub struct MarkdownPack;

/// The probe's sample: nested headings, a fenced block, a mention.
const SAMPLE: &str = "# Bowling\n\nA league scoring app.\n\n## Scoring\n\nFrame ten gets bonus balls; see `frame_ten`.\n\n```python\n# not a heading\n```\n\n### Handicap\n\nHandicap is (200 - average) * 0.9.\n";

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

impl LanguagePack for MarkdownPack {
    fn id(&self) -> &str {
        "markdown"
    }

    fn name(&self) -> &str {
        "Markdown"
    }

    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    fn languages(&self) -> Vec<LanguageInfo> {
        vec![LanguageInfo {
            name: "markdown".into(),
            sid_prefix: "md".into(),
            extensions: strings(&["md", "mdx", "markdown"]),
            filenames: strings(&["README", "CHANGELOG", "CONTRIBUTING", "AGENTS", "CLAUDE"]),
            ..LanguageInfo::default()
        }]
    }

    fn extract(&self, _language: &str, file: &SourceFile, _ctx: &ExtractContext, facts: &mut FileFacts) {
        sections::extract(&file.text, facts);
    }

    fn sample(&self) -> Sample {
        Sample { path: "README.md".into(), text: SAMPLE.into(), expect: strings(&["Bowling", "Bowling > Scoring", "Bowling > Scoring > Handicap"]) }
    }

    fn verifiers(&self) -> Vec<Verifier> {
        Vec::new()
    }

    fn server(&self) -> Option<ServerSpec> {
        None
    }

    /// No skills ship with the documents plugin.
    fn skills_dir(&self) -> Option<std::path::PathBuf> {
        None
    }
}
