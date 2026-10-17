//! The Docker language pack (`lang-docker`): Dockerfiles / Containerfiles and Compose files for the
//! code graph, dockerfile-language-server and compose-language-service (behind this executable's
//! LSP router) for diagnostics and navigation, `docker compose config`, `docker build --check` and
//! hadolint as verifiers, the docker CLI and hadolint on commands' PATH, and the Docker skills in
//! `data/skills`.

pub mod compose;
pub mod dockerfile;
pub mod image;
pub mod router;
pub mod server;
pub mod yaml;

use cc_lang_kit::cc_protocol::contracts::lang::{FileFacts, LanguageInfo, SourceFile, Verifier};
use cc_lang_kit::install::FoundServer;
use cc_lang_kit::lsp::ServerSpec;
use cc_lang_kit::pack::Toolchain;
use cc_lang_kit::{ExtractContext, LanguagePack, PackEnv, Sample};
use std::path::Path;

pub struct DockerPack;

/// The probe's sample: stages, ARG / ENV, ports, commands.
const SAMPLE: &str = r#"# syntax=docker/dockerfile:1
ARG NODE_VERSION=20
FROM node:${NODE_VERSION}-alpine AS build
WORKDIR /app
COPY package.json package-lock.json ./
RUN npm ci
COPY . .
RUN npm run build

FROM node:${NODE_VERSION}-alpine AS runtime
ENV NODE_ENV=production PORT=8080
WORKDIR /app
COPY --from=build /app/dist ./dist
USER node
EXPOSE 8080
HEALTHCHECK CMD ["wget", "-qO-", "http://localhost:8080/health"]
CMD ["node", "dist/server.js"]
"#;

/// Dockerfile names: `Dockerfile`, `Containerfile` (and their lower-case spellings).
pub const DOCKERFILE_NAMES: &[&str] = &["Dockerfile", "Containerfile", "dockerfile", "containerfile"];

/// Dockerfile name patterns: `Dockerfile.dev`, `api.Dockerfile`, `Containerfile.ci`, ...
pub const DOCKERFILE_PATTERNS: &[&str] =
    &["Dockerfile.*", "Containerfile.*", "dockerfile.*", "containerfile.*", "*.Dockerfile", "*.Containerfile", "*.dockerfile", "*.containerfile"];

/// Compose name patterns: `compose.override.yaml`, `docker-compose.prod.yml`, ...
pub const COMPOSE_PATTERNS: &[&str] = &["compose.*.yaml", "compose.*.yml", "docker-compose.*.yaml", "docker-compose.*.yml"];

/// The Compose file names the CLI looks for.
pub const STANDARD_COMPOSE: &[&str] = &["compose.yaml", "compose.yml", "docker-compose.yaml", "docker-compose.yml"];

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

impl LanguagePack for DockerPack {
    fn id(&self) -> &str {
        "docker"
    }

    fn name(&self) -> &str {
        "Docker"
    }

    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    fn languages(&self) -> Vec<LanguageInfo> {
        vec![
            LanguageInfo {
                name: "dockerfile".into(),
                sid_prefix: "docker".into(),
                // `api.Dockerfile`, `web.containerfile` (also as patterns, for consumers that read
                // extensions only).
                extensions: strings(&["Dockerfile", "dockerfile", "Containerfile", "containerfile"]),
                filenames: strings(DOCKERFILE_NAMES),
                patterns: strings(DOCKERFILE_PATTERNS),
                ..LanguageInfo::default()
            },
            LanguageInfo {
                name: "compose".into(),
                sid_prefix: "compose".into(),
                filenames: strings(STANDARD_COMPOSE),
                patterns: strings(COMPOSE_PATTERNS),
                ..LanguageInfo::default()
            },
        ]
    }

    fn extract(&self, language: &str, file: &SourceFile, ctx: &ExtractContext, facts: &mut FileFacts) {
        match language {
            "dockerfile" => dockerfile::extract(&file.path, &file.text, ctx.root.as_deref(), facts),
            "compose" => compose::extract(&file.path, &file.text, ctx.root.as_deref(), facts),
            _ => {}
        }
    }

    fn sample(&self) -> Sample {
        Sample {
            path: "Dockerfile".into(),
            text: SAMPLE.into(),
            expect: strings(&["NODE_VERSION", "build", "runtime", "runtime.NODE_ENV", "runtime.PORT", "runtime.8080"]),
        }
    }

    /// `docker compose config` and `docker build --check` (they need the docker CLI), hadolint
    /// (installed into the toolchains directory, which the toolchain puts on commands' PATH, or the
    /// user's own). The consumer leaves out those whose `requires` is not on PATH.
    fn verifiers(&self) -> Vec<Verifier> {
        let lint = |name: &str, command: &[&str], when: &[&str], requires: &str| Verifier {
            files: Vec::new(),            name: name.into(),
            kind: "lint".into(),
            command: strings(command),
            when: strings(when),
            requires: strings(&[requires]),
        };
        vec![
            lint("docker compose config", &["docker", "compose", "config", "--quiet"], STANDARD_COMPOSE, "docker"),
            lint("docker build --check", &["docker", "build", "--check", "."], &["Dockerfile"], "docker"),
            lint("hadolint", &["hadolint", "Dockerfile"], &["Dockerfile"], "hadolint"),
        ]
    }

    /// hadolint's directory when installed; the docker CLI's directory when it is not on PATH;
    /// plain progress output and no hints from docker, buildx and compose (output an agent reads).
    fn toolchain(&self, env: &PackEnv) -> Toolchain {
        let mut path = Vec::new();
        if let Some(hadolint) = server::hadolint(&env.toolchains)
            && let Some(dir) = hadolint.parent()
        {
            path.push(dir.to_string_lossy().into_owned());
        }
        if let Some(dir) = server::docker_dir() {
            path.push(dir.to_string_lossy().into_owned());
        }
        let env = [("BUILDKIT_PROGRESS", "plain"), ("COMPOSE_PROGRESS", "plain"), ("COMPOSE_ANSI", "never"), ("COMPOSE_MENU", "false"), ("DOCKER_CLI_HINTS", "false")];
        Toolchain { path, env: env.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect() }
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
