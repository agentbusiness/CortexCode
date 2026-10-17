//! The Java language pack (`lang-java`): `.java` files, `pom.xml` and `build.gradle(.kts)` for the
//! code graph, Eclipse JDT LS for diagnostics and navigation (with a JDK 21 installed into
//! CortexCode's toolchains directory when the system has none), Gradle / Maven verifiers, and the
//! Java skills in `data/skills`.

pub mod gradle;
pub mod java;
pub mod maven;
pub mod server;

use cc_lang_kit::cc_protocol::contracts::lang::{FileFacts, LanguageInfo, SourceFile, Verifier};
use cc_lang_kit::lsp::{Launch, ServerSpec};
use cc_lang_kit::pack::Toolchain;
use cc_lang_kit::{ExtractContext, FoundServer, LanguagePack, PackEnv, Sample};
use std::path::Path;

/// The Java pack (its toolchains are those of the `PackEnv` the runtime hands it).
pub struct JavaPack;

/// The probe's sample: items of every kind the extractor names.
pub(crate) const SAMPLE: &str = r#"package store;

import java.util.HashMap;
import java.util.Map;

/** Keeps items. */
public class Store implements Getter {
    /** The most items kept. */
    public static final int LIMIT = 100;
    private final Map<String, Integer> items = new HashMap<>();

    @Override
    public Integer get(String key) {
        return items.get(key);
    }

    public enum Kind { SMALL, LARGE }

    public record Entry(String key, int value) {}
}

interface Getter {
    Integer get(String key);
}
"#;

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

impl LanguagePack for JavaPack {
    fn id(&self) -> &str {
        "java"
    }

    fn name(&self) -> &str {
        "Java"
    }

    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    /// `java` as the memory plugin's `Lang::Java` (code-graph ids stay the same), and the build files.
    fn languages(&self) -> Vec<LanguageInfo> {
        vec![
            LanguageInfo { name: "java".into(), sid_prefix: "java".into(), extensions: strings(&["java"]), ..LanguageInfo::default() },
            LanguageInfo { name: "pom.xml".into(), sid_prefix: "maven".into(), filenames: strings(&["pom.xml"]), ..LanguageInfo::default() },
            LanguageInfo { name: "build.gradle".into(), sid_prefix: "gradle".into(), filenames: strings(&["build.gradle", "build.gradle.kts"]), ..LanguageInfo::default() },
        ]
    }

    fn extract(&self, language: &str, file: &SourceFile, _ctx: &ExtractContext, facts: &mut FileFacts) {
        match language {
            "java" => java::extract(&file.path, &file.text, facts),
            "pom.xml" => maven::extract(&file.text, facts),
            "build.gradle" => gradle::extract(&file.text, facts),
            _ => {}
        }
    }

    fn sample(&self) -> Sample {
        Sample {
            path: "src/main/java/store/Store.java".into(),
            text: SAMPLE.into(),
            expect: strings(&["Store", "Store.LIMIT", "Store.items", "Store.get", "Store.Kind", "Store.Kind.LARGE", "Store.Entry", "Getter", "Getter.get"]),
        }
    }

    /// The project's own wrappers (`gradlew`, `mvnw`: they need only a JDK), else Maven on PATH.
    fn verifiers(&self) -> Vec<Verifier> {
        let verifier = |name: &str, kind: &str, args: &[&str], when: &str, requires: &[&str]| Verifier {
            files: Vec::new(),            name: name.into(),
            kind: kind.into(),
            command: strings(args),
            when: strings(&[when]),
            requires: strings(requires),
        };
        vec![
            verifier("gradle build", "build", &["./gradlew", "classes", "testClasses", "--offline", "-q"], "gradlew", &["java"]),
            verifier("gradle test", "test", &["./gradlew", "test", "--offline"], "gradlew", &["java"]),
            verifier("maven wrapper build", "build", &["./mvnw", "-q", "-o", "test-compile"], "mvnw", &["java"]),
            verifier("maven wrapper test", "test", &["./mvnw", "-q", "-o", "test"], "mvnw", &["java"]),
            verifier("maven build", "build", &["mvn", "-q", "-o", "test-compile"], "pom.xml", &["mvn", "java"]),
            verifier("maven test", "test", &["mvn", "-q", "-o", "test"], "pom.xml", &["mvn", "java"]),
        ]
    }

    fn server(&self) -> Option<ServerSpec> {
        Some(server::spec())
    }

    fn toolchain(&self, env: &PackEnv) -> Toolchain {
        server::toolchain(&env.toolchains, &server::Host::current())
    }

    fn locate_server(&self, env: &PackEnv, _root: Option<&Path>) -> Result<FoundServer, String> {
        server::locate(&env.toolchains, &server::Host::current())
    }

    /// jdtls with the root's own `-data` directory.
    fn server_launch(&self, env: &PackEnv, root: &Path, _server: &FoundServer) -> Result<Launch, String> {
        server::launch(&env.home, &env.toolchains, root, env.server_path().is_some())
    }

    fn install_server(&self, env: &PackEnv, log: &mut Vec<String>) -> Result<(), String> {
        server::install(&env.toolchains, &server::Host::current(), log)
    }
}
