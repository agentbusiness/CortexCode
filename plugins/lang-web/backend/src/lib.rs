//! The web language pack (`lang-web`): HTML pages and CSS / SCSS / Less stylesheets for the code
//! graph, vscode-html-language-server and vscode-css-language-server (behind this executable's LSP
//! router) for diagnostics and navigation, html-validate and stylelint (the project's own) as
//! verifiers, and the web skills in `data/skills`.

pub mod css;
pub mod html;
pub mod paths;
pub mod router;
pub mod server;

use cc_lang_kit::cc_protocol::contracts::lang::{FileFacts, LanguageInfo, SourceFile, Verifier};
use cc_lang_kit::install::FoundServer;
use cc_lang_kit::lsp::ServerSpec;
use cc_lang_kit::{ExtractContext, LanguagePack, PackEnv, Sample};
use std::path::Path;

pub struct WebPack;

/// The probe's sample: ids, a form and its fields, the outline, an inline style.
const SAMPLE: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <title>Sample</title>
  <link rel="stylesheet" href="css/site.css">
  <style>
    :root { --brand: #0b5; }
    .hero { color: var(--brand); }
    @media (min-width: 40rem) { .hero { padding: 2rem; } }
  </style>
</head>
<body>
  <main id="main" class="page">
    <h1>Welcome</h1>
    <section class="hero">
      <h2>Sign up</h2>
      <form id="signup" action="/api/signup" method="post">
        <label for="email">Email</label>
        <input id="email" name="email" type="email" required>
        <button type="submit">Join</button>
      </form>
    </section>
  </main>
  <script type="module" src="js/app.js"></script>
</body>
</html>
"##;

/// stylelint's configuration files (a `stylelint` key in package.json is not a marker file).
pub const STYLELINT_CONFIGS: &[&str] = &[
    ".stylelintrc",
    ".stylelintrc.json",
    ".stylelintrc.yaml",
    ".stylelintrc.yml",
    ".stylelintrc.js",
    ".stylelintrc.cjs",
    ".stylelintrc.mjs",
    "stylelint.config.js",
    "stylelint.config.cjs",
    "stylelint.config.mjs",
];

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

impl LanguagePack for WebPack {
    fn id(&self) -> &str {
        "web"
    }

    fn name(&self) -> &str {
        "Web (HTML, CSS)"
    }

    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    fn languages(&self) -> Vec<LanguageInfo> {
        let language = |name: &str, prefix: &str, extensions: &[&str]| LanguageInfo {
            name: name.into(),
            sid_prefix: prefix.into(),
            extensions: strings(extensions),
            filenames: Vec::new(),
            shebangs: Vec::new(),
            patterns: Vec::new(),
        };
        vec![language("html", "html", &["html", "htm"]), language("css", "css", &["css"]), language("scss", "scss", &["scss"]), language("less", "less", &["less"])]
    }

    fn extract(&self, language: &str, file: &SourceFile, ctx: &ExtractContext, facts: &mut FileFacts) {
        match (language, css::Dialect::of(language)) {
            ("html", _) => html::extract(&file.path, &file.text, ctx.root.as_deref(), facts),
            (_, Some(dialect)) => css::extract(dialect, &file.path, &file.text, ctx.root.as_deref(), facts),
            _ => {}
        }
    }

    fn sample(&self) -> Sample {
        Sample {
            path: "index.html".into(),
            text: SAMPLE.into(),
            expect: strings(&["#main", "Welcome", "Sign up", "form#signup", "form#signup.email", "#email", "<style>", "<style>/.hero", "<style>/--brand", "<style>/@media (min-width: 40rem)/.hero"]),
        }
    }

    /// The project's own html-validate and stylelint, never downloaded (`npx --no --offline`):
    /// html-validate when it is installed in the project, stylelint when the project configures it.
    fn verifiers(&self) -> Vec<Verifier> {
        vec![
            Verifier {
            files: Vec::new(),                name: "html-validate".into(),
                kind: "lint".into(),
                command: strings(&["npx", "--no", "--offline", "--", "html-validate", "{files}"]),
                when: strings(&["node_modules/.bin/html-validate"]),
                requires: strings(&["npx"]),
            },
            Verifier {
            files: Vec::new(),                name: "stylelint".into(),
                kind: "lint".into(),
                command: strings(&["npx", "--no", "--offline", "--", "stylelint", "\"**/*.css\""]),
                when: strings(STYLELINT_CONFIGS),
                requires: strings(&["npx"]),
            },
        ]
    }

    fn server(&self) -> Option<ServerSpec> {
        Some(server::spec())
    }

    fn locate_server(&self, env: &PackEnv, _root: Option<&Path>) -> Result<FoundServer, String> {
        server::locate(&env.toolchains)
    }

    fn install_server(&self, env: &PackEnv, log: &mut Vec<String>) -> Result<(), String> {
        server::install(&env.toolchains, log)
    }
}
