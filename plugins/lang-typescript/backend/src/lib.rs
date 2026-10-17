//! The TypeScript language pack (`lang-typescript`): `.ts` / `.tsx` / `.js` files and
//! `package.json` manifests for the code graph, typescript-language-server for diagnostics and
//! navigation, tsc / npm test / eslint as verifiers, and the TypeScript skills in `data/skills`.

pub mod js;
pub mod npm;
pub mod server;

use cc_lang_kit::cc_protocol::contracts::lang::{FileFacts, LanguageInfo, SourceFile, Verifier};
use cc_lang_kit::lsp::ServerSpec;
use cc_lang_kit::{ExtractContext, FoundServer, LanguagePack, PackEnv, Sample};
use js::Dialect;
use std::path::{Path, PathBuf};

/// The pack. `home` is CortexCode's home: the server's fallback TypeScript lives in its toolchains.
pub struct TypeScriptPack {
    home: PathBuf,
}

impl TypeScriptPack {
    pub fn new(home: PathBuf) -> Self {
        Self { home }
    }
}

/// The probe's sample: definitions of every kind the extractor names.
const SAMPLE: &str = r#"import { readFile } from "node:fs/promises";

/** A key-value store. */
export interface Get {
  get(key: string): number | undefined;
}

export type Entry = { key: string; value: number };

export enum Kind {
  A,
  B = 2,
}

export class Store implements Get {
  private items = new Map<string, number>();

  get(key: string): number | undefined {
    return this.items.get(key);
  }
}

export namespace Config {
  export const path = "config.json";
}

export async function load(file: string): Promise<Store> {
  const store = new Store();
  await readFile(file, "utf8");
  return store;
}

describe("store", () => {
  it("is empty", () => {
    expect(new Store().get("x")).toBeUndefined();
  });
});
"#;

/// ESLint configuration files (flat config, then the legacy `.eslintrc` forms).
const ESLINT_CONFIGS: &[&str] = &[
    "eslint.config.js",
    "eslint.config.mjs",
    "eslint.config.cjs",
    "eslint.config.ts",
    "eslint.config.mts",
    "eslint.config.cts",
    ".eslintrc.js",
    ".eslintrc.cjs",
    ".eslintrc.yaml",
    ".eslintrc.yml",
    ".eslintrc.json",
    ".eslintrc",
];

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

impl LanguagePack for TypeScriptPack {
    fn id(&self) -> &str {
        "typescript"
    }

    fn name(&self) -> &str {
        "TypeScript"
    }

    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    /// Names and symbol-id prefixes are the code graph's (the memory plugin's `Lang::name` and
    /// `Lang::sid_prefix`), so ids stay the same whichever extractor produced the facts.
    fn languages(&self) -> Vec<LanguageInfo> {
        let language = |name: &str, prefix: &str, extensions: &[&str], filenames: &[&str], shebangs: &[&str]| LanguageInfo {
            name: name.into(),
            sid_prefix: prefix.into(),
            extensions: strings(extensions),
            filenames: strings(filenames),
            shebangs: strings(shebangs),
            ..LanguageInfo::default()
        };
        vec![
            language("typescript", "js", &["ts", "mts", "cts"], &[], &[]),
            language("tsx", "js", &["tsx"], &[], &[]),
            language("javascript", "js", &["js", "jsx", "mjs", "cjs"], &[], &["node"]),
            language("package.json", "npm", &[], &["package.json"], &[]),
        ]
    }

    fn extract(&self, language: &str, file: &SourceFile, ctx: &ExtractContext, facts: &mut FileFacts) {
        match language {
            "typescript" => js::extract(Dialect::TypeScript, &file.text, facts),
            "tsx" => js::extract(Dialect::Tsx, &file.text, facts),
            "javascript" => js::extract(Dialect::JavaScript, &file.text, facts),
            "package.json" => npm::extract(&file.path, &file.text, ctx, facts),
            _ => {}
        }
    }

    fn sample(&self) -> Sample {
        Sample {
            path: "src/store.ts".into(),
            text: SAMPLE.into(),
            expect: strings(&["Get", "Get.get", "Entry", "Kind", "Kind.B", "Store", "Store.items", "Store.get", "Config", "Config.path", "load", "test:store > is empty"]),
        }
    }

    /// Offline: tools run only from the project's own `node_modules/.bin` (`npx --no --offline`
    /// never installs, and never asks the registry); `npm test` runs the package's test script.
    fn verifiers(&self) -> Vec<Verifier> {
        let verifier = |name: &str, kind: &str, args: &[&str], when: &[&str]| Verifier {
            files: Vec::new(),            name: name.into(),
            kind: kind.into(),
            command: strings(args),
            when: strings(when),
            requires: strings(&[args[0]]),
        };
        vec![
            verifier("tsc --noEmit", "build", &["npx", "--no", "--offline", "--", "tsc", "--noEmit", "-p", "."], &["tsconfig.json"]),
            // The test script's runner (jest, vitest) fails when it finds no tests.
            Verifier { files: strings(&["**/*.test.*", "**/*.spec.*", "**/__tests__/**"]), ..verifier("npm test", "test", &["npm", "test"], &["package.json"]) },
            verifier("eslint", "lint", &["npx", "--no", "--offline", "--", "eslint", "."], ESLINT_CONFIGS),
        ]
    }

    fn server(&self) -> Option<ServerSpec> {
        Some(server::spec(&self.home))
    }

    fn locate_server(&self, env: &PackEnv, root: Option<&Path>) -> Result<FoundServer, String> {
        server::locate(&env.toolchains, root)
    }

    fn install_server(&self, env: &PackEnv, log: &mut Vec<String>) -> Result<(), String> {
        server::install(&env.toolchains, log)
    }
}
