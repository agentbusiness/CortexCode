//! Pack scaffolding: a language plugin is a [`LanguagePack`]; [`run_pack`] serves it as `lang@1`
//! on the category `lang-<id>` over CPP/1.
//!
//! The runtime owns everything that is the same for every language: method routing, extraction
//! batches spread over the cores, skills from `data/skills` (the live folder when it exists, see
//! [`skills::current_dir`], watched with [`skills::watch`]), the language server's lifecycle and
//! queries ([`crate::lsp::Servers`]), installation reporting, the `<category>.server` events and the
//! probe the frame runs before routing traffic (it extracts the pack's sample through the real
//! method, reads every skill and reports whether the server is available).
//!
//! The category is the pack's own (`lang-` + [`LanguagePack::id`]): the frame starts the process
//! for the category its plugin.toml names and rejects a hello that says another one, and CPP/1 does
//! not hand the category to the process, so the pack's id and the plugin.toml's `category` must
//! agree.
//!
//! Plugin configuration (`[plugin.config]`): `server_path`, an executable to use as the language
//! server instead of the one the pack finds (checked with the pack's [`LanguagePack::probe_server`]).
//!
//! Per workspace: the server of each root starts with what [`LanguagePack::server_launch`] says for
//! that root (program, arguments, environment, `workspace/configuration` answers,
//! `initializationOptions`), and every `diagnostics` answer passes through
//! [`LanguagePack::annotate_diagnostics`].

use crate::install::{FoundServer, Toolchains, probe_version};
use crate::lsp::{Launch, Launched, Launcher, ServerEvent, ServerSpec, Servers, spawn_process};
use crate::skills;
use crate::syntax::dedupe_qnames;
use cc_protocol::contracts::contract_of;
use cc_protocol::contracts::lang::{
    self, Diagnostic, Description, DiagnosticsParams, ExtractParams, ExtractResult, FileFacts, InstallResult, LanguageInfo, PositionParams, ServerInfo, SkillInfo, SkillParams,
    SkillResult, SourceFile, Verifier, methods, topics,
};
use cc_protocol::contracts::lifecycle::{Hello, ProbeCheck, ProbeReport};
use cc_protocol::{BoxFuture, Call, Ctx, Plugin, PluginEnv, RpcError, codes, parse_params, run_plugin, to_value};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Where a pack works.
#[derive(Debug, Clone)]
pub struct PackEnv {
    /// CortexCode's home (`CORTEX_HOME`).
    pub home: PathBuf,
    /// `$CORTEX_HOME/toolchains`.
    pub toolchains: Toolchains,
    /// The plugin's `[plugin.config]` (null when absent or before the frame answered hello).
    pub config: Value,
}

impl PackEnv {
    /// `[plugin.config] server_path`: the executable configured as the server, when one is.
    pub fn server_path(&self) -> Option<PathBuf> {
        self.config.get("server_path").and_then(Value::as_str).filter(|p| !p.is_empty()).map(PathBuf::from)
    }
}

/// What an extractor knows beyond a file's path and text.
#[derive(Debug, Clone, Default)]
pub struct ExtractContext {
    /// The repository root, when known. Manifest extractors resolve what lives on disk against it
    /// (lock files, installed dependency sources, the files a command runs); without it they report
    /// only what the file itself says. From `lang@1`'s `extract` `root`.
    pub root: Option<PathBuf>,
}

/// What commands need from a language's installed toolchain.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Toolchain {
    pub path: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// A file the probe extracts through the real `extract` method, with qualified names it must yield.
#[derive(Debug, Clone)]
pub struct Sample {
    pub path: String,
    pub text: String,
    pub expect: Vec<String>,
}

/// Everything CortexCode knows about one language family.
pub trait LanguagePack: Send + Sync + 'static {
    /// The family name (`rust`, `python`, ...): the plugin's category is `lang-<id>`.
    fn id(&self) -> &str;
    /// Display name (`Rust`).
    fn name(&self) -> &str;
    /// The plugin build's version (its crate's `CARGO_PKG_VERSION`).
    fn version(&self) -> &str;
    /// The languages it parses and the files that are theirs.
    fn languages(&self) -> Vec<LanguageInfo>;
    /// Fill `facts` (path and language already set) from one file. `language` is the
    /// [`LanguageInfo::name`] the file matched. Content hash, size, line count and excerpt are the
    /// caller's.
    fn extract(&self, language: &str, file: &SourceFile, ctx: &ExtractContext, facts: &mut FileFacts);
    /// The probe's sample.
    fn sample(&self) -> Sample;
    /// Commands that check work in this language.
    fn verifiers(&self) -> Vec<Verifier>;
    /// The language server, None when the language has none.
    fn server(&self) -> Option<ServerSpec>;
    /// The installed toolchain commands need (blocking): directories for PATH and environment
    /// (`JAVA_HOME`, `GOROOT`). Empty when the system's own tools serve or nothing is installed.
    fn toolchain(&self, _env: &PackEnv) -> Toolchain {
        Toolchain::default()
    }
    /// Find a runnable server (blocking; runs processes). `root`: the workspace it will serve, when
    /// known (toolchain managers pick the version per directory).
    fn locate_server(&self, _env: &PackEnv, _root: Option<&Path>) -> Result<FoundServer, String> {
        Err("this language has no language server".into())
    }
    /// Install the server (blocking), appending what was done to `log`. The runtime locates the
    /// server again afterwards to report the result.
    fn install_server(&self, _env: &PackEnv, _log: &mut Vec<String>) -> Result<(), String> {
        Err("this language has no language server to install".into())
    }
    /// Whether the configured `server_path` runs, and its version (blocking). Default: it answers
    /// the spec's `version_args` (with the spec's environment, in `root`). A server without a
    /// version flag (basedpyright-langserver) says how else to tell.
    fn probe_server(&self, _env: &PackEnv, path: &Path, root: Option<&Path>) -> Result<String, String> {
        let spec = self.server();
        let version_args = spec.as_ref().map(|s| s.version_args.clone()).unwrap_or_default();
        let env: Vec<(&str, &str)> = spec.iter().flat_map(|s| s.env.iter().map(|(k, v)| (k.as_str(), v.as_str()))).collect();
        probe_version(path, &version_args, root, &env)
    }
    /// How the server of workspace `root` starts (blocking; runs when that root's server starts,
    /// after `server` was located — the configured `server_path` when [`PackEnv::server_path`] is
    /// set). Default: as the spec says.
    fn server_launch(&self, _env: &PackEnv, _root: &Path, _server: &FoundServer) -> Result<Launch, String> {
        Ok(Launch::default())
    }
    /// Add to or change the diagnostics of a `diagnostics` answer for `root` (runs on the async
    /// runtime: cheap file checks only). `files`: the files the answer covers, relative to `root`
    /// (the requested files the server took, and every file with diagnostics).
    fn annotate_diagnostics(&self, _env: &PackEnv, _root: &Path, _files: &[String], _diagnostics: &mut Vec<Diagnostic>) {}
    /// Where the packaged skills are (default: `data/skills` beside the executable, see
    /// [`skills`]); None: the plugin ships no skills, live or packaged.
    fn skills_dir(&self) -> Option<PathBuf> {
        skills::locate_dir()
    }
}

/// The interpreter a `#!` line names (`#!/usr/bin/env -S python3 -u` -> `python3`).
pub fn shebang_interpreter(text: &str) -> Option<&str> {
    let line = text.lines().next()?.strip_prefix("#!")?;
    let mut words = line.split_whitespace();
    let program = words.next()?;
    let name = program.rsplit('/').next()?;
    if name == "env" {
        words.find(|w| !w.starts_with('-') && !w.contains('=')).map(|w| w.rsplit('/').next().unwrap_or(w))
    } else {
        Some(name)
    }
}

/// The language of a file: by exact file name, then file-name pattern, then extension, then `#!`
/// interpreter.
pub fn language_of<'a>(languages: &'a [LanguageInfo], path: &str, text: &str) -> Option<&'a LanguageInfo> {
    let name = path.rsplit('/').next().unwrap_or(path);
    if let Some(found) = languages.iter().find(|l| l.filenames.iter().any(|f| f == name)) {
        return Some(found);
    }
    if let Some(found) = languages.iter().find(|l| l.patterns.iter().any(|p| lang::name_matches(p, name))) {
        return Some(found);
    }
    if let Some((_, extension)) = name.rsplit_once('.')
        && let Some(found) = languages.iter().find(|l| l.extensions.iter().any(|e| e == extension))
    {
        return Some(found);
    }
    let interpreter = shebang_interpreter(text)?;
    languages.iter().find(|l| l.shebangs.iter().any(|s| s == interpreter))
}

fn extract_one<P: LanguagePack>(pack: &P, language: &str, file: &SourceFile, ctx: &ExtractContext) -> Result<FileFacts, String> {
    let mut facts = FileFacts { path: file.path.clone(), language: language.to_owned(), ..FileFacts::default() };
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| pack.extract(language, file, ctx, &mut facts)))
        .map_err(|_| format!("the {language} extractor failed on {}", file.path))?;
    dedupe_qnames(&mut facts.symbols);
    Ok(facts)
}

/// Extract a batch, spread over the available cores, results in input order.
pub fn extract_batch<P: LanguagePack>(pack: &P, jobs: &[(String, SourceFile)], ctx: &ExtractContext) -> Result<Vec<FileFacts>, String> {
    if jobs.is_empty() {
        return Ok(Vec::new());
    }
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).min(jobs.len());
    let chunk = jobs.len().div_ceil(threads);
    std::thread::scope(|scope| {
        let handles: Vec<_> = jobs
            .chunks(chunk)
            .map(|part| scope.spawn(move || part.iter().map(|(language, file)| extract_one(pack, language, file, ctx)).collect::<Result<Vec<_>, _>>()))
            .collect();
        let mut out = Vec::with_capacity(jobs.len());
        for handle in handles {
            out.extend(handle.join().map_err(|_| "an extraction thread failed".to_owned())??);
        }
        Ok(out)
    })
}

fn server_info(name: &str, located: Result<FoundServer, String>) -> ServerInfo {
    match located {
        Ok(found) => ServerInfo {
            name: name.to_owned(),
            path: Some(found.path.to_string_lossy().into_owned()),
            installed: true,
            version: Some(found.version),
            problem: None,
        },
        Err(problem) => ServerInfo { name: name.to_owned(), path: None, installed: false, version: None, problem: Some(problem) },
    }
}

/// State shared by the plugin, its server launcher and its event sink.
struct Shared<P: LanguagePack> {
    pack: Arc<P>,
    category: String,
    plugin_env: PluginEnv,
    spec: Option<ServerSpec>,
    ctx: OnceLock<Ctx>,
    config: Mutex<Value>,
    /// The packaged skills folder ([`LanguagePack::skills_dir`]).
    skills: Option<PathBuf>,
}

impl<P: LanguagePack> Shared<P> {
    fn env(&self) -> PackEnv {
        let config = self.config.lock().map(|c| c.clone()).unwrap_or(Value::Null);
        let home = &self.plugin_env.home;
        PackEnv { home: home.clone(), toolchains: Toolchains::new(home), config }
    }

    /// The skills folder to read now ([`skills::current_dir`]).
    fn skills_dir(&self) -> Option<PathBuf> {
        skills::current_dir(&self.plugin_env, self.skills.as_deref())
    }

    /// The configured `server_path` (when the pack's probe accepts it), else what the pack finds
    /// (blocking).
    fn locate(&self, root: Option<&Path>) -> Result<FoundServer, String> {
        let env = self.env();
        if let Some(path) = env.server_path() {
            let version = self.pack.probe_server(&env, &path, root).map_err(|e| format!("server_path {}: {e}", path.display()))?;
            return Ok(FoundServer { path, version });
        }
        self.pack.locate_server(&env, root)
    }

    fn publish(&self, event: &ServerEvent) {
        if let (Some(ctx), Ok(data)) = (self.ctx.get(), serde_json::to_value(event)) {
            ctx.publish(&format!("{}.{}", self.category, topics::SERVER), data);
        }
    }
}

struct PackLauncher<P: LanguagePack>(Arc<Shared<P>>);

impl<P: LanguagePack> Launcher for PackLauncher<P> {
    fn launch(&self, root: &Path) -> BoxFuture<'static, Result<Launched, String>> {
        let shared = self.0.clone();
        let root = root.to_path_buf();
        Box::pin(async move {
            let spec = shared.spec.clone().ok_or("this language has no language server")?;
            let locate_shared = shared.clone();
            let locate_root = root.clone();
            let (found, launch) = tokio::task::spawn_blocking(move || {
                let found = locate_shared.locate(Some(&locate_root))?;
                let launch = locate_shared.pack.server_launch(&locate_shared.env(), &locate_root, &found)?;
                Ok::<_, String>((found, launch))
            })
            .await
            .map_err(|e| e.to_string())??;
            let description = format!("{} ({}) at {}", spec.name, found.version, launch.program.as_deref().unwrap_or(&found.path).display());
            spawn_process(&found.path, &spec, &launch, &root, description)
        })
    }
}

/// The `lang@1` plugin of one pack.
pub struct LangPlugin<P: LanguagePack> {
    shared: Arc<Shared<P>>,
    servers: Option<Arc<Servers>>,
}

fn check(name: &str, started: Instant, outcome: Result<String, String>) -> ProbeCheck {
    let ms = started.elapsed().as_millis() as u64;
    match outcome {
        Ok(detail) => ProbeCheck { name: name.into(), ok: true, detail, ms },
        Err(detail) => ProbeCheck { name: name.into(), ok: false, detail, ms },
    }
}

impl<P: LanguagePack> LangPlugin<P> {
    /// The plugin for `pack`, in the environment the frame started it with.
    pub fn new(pack: P, plugin_env: PluginEnv) -> Self {
        let category = format!("lang-{}", pack.id());
        let spec = pack.server();
        let skills = pack.skills_dir();
        let shared = Arc::new(Shared { pack: Arc::new(pack), category, plugin_env, spec: spec.clone(), ctx: OnceLock::new(), config: Mutex::new(Value::Null), skills });
        let servers = spec.map(|spec| {
            let events = shared.clone();
            let annotate = shared.clone();
            let servers = Servers::new(spec, Arc::new(PackLauncher(shared.clone())), Arc::new(move |event: ServerEvent| events.publish(&event)));
            Arc::new(servers.with_diagnostics_hook(Arc::new(move |root: &Path, files: &[String], diagnostics: &mut Vec<Diagnostic>| {
                annotate.pack.annotate_diagnostics(&annotate.env(), root, files, diagnostics)
            })))
        });
        Self { shared, servers }
    }

    pub fn category(&self) -> &str {
        &self.shared.category
    }

    /// Methods served, as `<category>.<method>`.
    pub fn provides(&self) -> Vec<String> {
        methods::ALL.iter().map(|method| format!("{}.{method}", self.shared.category)).collect()
    }

    pub fn servers(&self) -> Option<Arc<Servers>> {
        self.servers.clone()
    }

    fn remember(&self, ctx: &Ctx) {
        if self.shared.ctx.set(ctx.clone()).is_ok()
            && let Ok(mut config) = self.shared.config.lock()
        {
            *config = ctx.config();
        }
    }

    fn require_servers(&self) -> Result<Arc<Servers>, RpcError> {
        self.servers.clone().ok_or_else(|| RpcError::new(codes::UNAVAILABLE, format!("{} has no language server", self.shared.pack.name())))
    }

    async fn locate(&self, root: Option<PathBuf>) -> Result<FoundServer, String> {
        let shared = self.shared.clone();
        tokio::task::spawn_blocking(move || shared.locate(root.as_deref())).await.map_err(|e| e.to_string())?
    }

    async fn describe(&self) -> Description {
        let pack = &self.shared.pack;
        let server = match &self.shared.spec {
            Some(spec) => Some(server_info(&spec.name, self.locate(None).await)),
            None => None,
        };
        let skills = self
            .shared
            .skills_dir()
            .as_deref()
            // An invalid codes.toml or applies.toml leaves the skills served without codes and
            // conditions (the probe says why).
            .and_then(|dir| skills::load(dir).or_else(|_| skills::list(dir)).ok())
            .unwrap_or_default()
            .into_iter()
            .map(|skill| SkillInfo { name: skill.name, summary: skill.summary, codes: skill.codes, applies: skill.applies })
            .collect();
        let toolchain = {
            let shared = self.shared.clone();
            tokio::task::spawn_blocking(move || shared.pack.toolchain(&shared.env())).await.unwrap_or_default()
        };
        Description {
            id: pack.id().to_owned(),
            name: pack.name().to_owned(),
            languages: pack.languages(),
            server,
            verifiers: pack.verifiers(),
            skills,
            path: toolchain.path,
            env: toolchain.env,
        }
    }

    async fn extract(&self, params: ExtractParams) -> Result<ExtractResult, RpcError> {
        let languages = self.shared.pack.languages();
        let mut jobs = Vec::with_capacity(params.files.len());
        let mut unmatched = Vec::new();
        for file in params.files {
            match language_of(&languages, &file.path, &file.text) {
                Some(language) => jobs.push((language.name.clone(), file)),
                None => unmatched.push(file.path),
            }
        }
        if !unmatched.is_empty() {
            return Err(RpcError::invalid_params(format!("not {} files: {}", self.shared.pack.name(), unmatched.join(", "))));
        }
        let pack = self.shared.pack.clone();
        let ctx = ExtractContext { root: params.root.map(PathBuf::from) };
        let files = tokio::task::spawn_blocking(move || extract_batch(&*pack, &jobs, &ctx))
            .await
            .map_err(|e| RpcError::internal(e.to_string()))?
            .map_err(RpcError::internal)?;
        Ok(ExtractResult { files })
    }

    async fn install(&self) -> Result<InstallResult, RpcError> {
        let spec = self.shared.spec.clone().ok_or_else(|| RpcError::new(codes::UNAVAILABLE, format!("{} has no language server", self.shared.pack.name())))?;
        let shared = self.shared.clone();
        let (outcome, mut log) = tokio::task::spawn_blocking(move || {
            let mut log = Vec::new();
            let outcome = shared.pack.install_server(&shared.env(), &mut log);
            (outcome, log)
        })
        .await
        .map_err(|e| RpcError::internal(e.to_string()))?;
        if let Err(problem) = outcome {
            log.push(format!("failed: {problem}"));
            return Err(RpcError::new(codes::UNAVAILABLE, format!("installing {} failed: {problem}", spec.name)).with_data(json!({"log": log})));
        }
        // Servers started before the install run the old executable (or failed for want of one).
        if let Some(servers) = &self.servers {
            servers.stop_all("reinstalled").await;
        }
        let server = server_info(&spec.name, self.locate(None).await);
        if let Some(problem) = &server.problem {
            log.push(format!("still not available: {problem}"));
            return Err(RpcError::new(codes::UNAVAILABLE, format!("{} is still not available after the install: {problem}", spec.name)).with_data(json!({"log": log})));
        }
        log.push(format!("{} {} at {}", spec.name, server.version.as_deref().unwrap_or(""), server.path.as_deref().unwrap_or("")));
        self.shared.publish(&ServerEvent { state: "installed".into(), server: spec.name.clone(), root: String::new(), detail: log.last().cloned().unwrap_or_default() });
        Ok(InstallResult { server, log })
    }

    fn skill(&self, params: SkillParams) -> Result<SkillResult, RpcError> {
        let dir = self.shared.skills_dir().ok_or_else(|| RpcError::new(codes::UNAVAILABLE, "this plugin's data/skills directory is missing"))?;
        let text = skills::read(&dir, &params.name)?;
        Ok(SkillResult { name: params.name, text })
    }

    /// Serve one `lang@1` method (`<category>.<method>`).
    pub async fn dispatch(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let verb = method
            .strip_prefix(self.shared.category.as_str())
            .and_then(|rest| rest.strip_prefix('.'))
            .ok_or_else(|| RpcError::unknown_method(method))?;
        match verb {
            methods::DESCRIBE => to_value(&self.describe().await),
            methods::EXTRACT => to_value(&self.extract(parse_params(params)?).await?),
            methods::DIAGNOSTICS => {
                let p: DiagnosticsParams = parse_params(params)?;
                let servers = self.require_servers()?;
                // Without a wait, a cold server of named files starts in the background (for later
                // requests); this one answers at once.
                if p.wait_ms == Some(0) && !p.paths.is_empty() && !servers.is_running(&p.root) {
                    let (servers, root, paths) = (servers.clone(), p.root.clone(), p.paths.clone());
                    tokio::spawn(async move {
                        if let Err(error) = servers.start_for(&root, &paths).await {
                            tracing::debug!(%error, "background server start failed");
                        }
                    });
                }
                to_value(&servers.diagnostics(&p.root, &p.paths, p.wait_ms.map(Duration::from_millis)).await?)
            }
            methods::DEFINITION => to_value(&self.require_servers()?.definition(&parse_params::<PositionParams>(params)?).await?),
            methods::REFERENCES => to_value(&self.require_servers()?.references(&parse_params::<PositionParams>(params)?).await?),
            methods::HOVER => to_value(&self.require_servers()?.hover(&parse_params::<PositionParams>(params)?).await?),
            methods::SKILL => to_value(&self.skill(parse_params(params)?)?),
            methods::INSTALL => to_value(&self.install().await?),
            _ => Err(RpcError::unknown_method(method)),
        }
    }

    /// The conformance self-test: the real methods on the pack's sample and skills, and the
    /// server's availability.
    pub async fn self_test(&self) -> ProbeReport {
        let category = &self.shared.category;
        let mut checks = Vec::new();

        let started = Instant::now();
        let description = self.dispatch(&format!("{category}.{}", methods::DESCRIBE), Value::Null).await.and_then(parse_params::<Description>);
        let skills = description.as_ref().map(|d| d.skills.clone()).unwrap_or_default();
        let server = description.as_ref().ok().and_then(|d| d.server.clone());
        checks.push(check(
            "describe",
            started,
            description.map_err(|e| e.to_string()).and_then(|d| {
                if d.languages.is_empty() {
                    return Err("no languages".into());
                }
                let languages: Vec<String> = d.languages.iter().map(|l| format!("{} ({})", l.name, l.sid_prefix)).collect();
                Ok(format!("{}; {} verifiers", languages.join(", "), d.verifiers.len()))
            }),
        ));

        let started = Instant::now();
        let sample = self.shared.pack.sample();
        let request = json!({"files": [{"path": sample.path, "text": sample.text}]});
        let extracted = self.dispatch(&format!("{category}.{}", methods::EXTRACT), request).await.and_then(parse_params::<ExtractResult>);
        checks.push(check(
            "extract",
            started,
            extracted.map_err(|e| e.to_string()).and_then(|result| {
                let facts = result.files.first().ok_or("no facts returned")?;
                let missing: Vec<&String> = sample.expect.iter().filter(|q| !facts.symbols.iter().any(|s| &s.qname == *q)).collect();
                if facts.parse_errors > 0 {
                    return Err(format!("{} parse errors in the sample", facts.parse_errors));
                }
                if !missing.is_empty() {
                    return Err(format!("missing symbols {missing:?}"));
                }
                Ok(format!("{}: {} symbols, {} references", facts.language, facts.symbols.len(), facts.refs.len()))
            }),
        ));

        let started = Instant::now();
        let mut skill_outcome = match &self.shared.skills_dir() {
            // The plugin ships no skills (its `skills_dir` says so): nothing to check.
            None => Ok("no skills".to_owned()),
            Some(dir) if skills.is_empty() => Ok(format!("no skills in {}", dir.display())),
            Some(dir) => match skills::load(dir) {
                Err(error) => Err(error),
                Ok(_) => Ok(skills_detail(&skills)),
            },
        };
        for skill in &skills {
            let read = self.dispatch(&format!("{category}.{}", methods::SKILL), json!({"name": skill.name})).await.and_then(parse_params::<SkillResult>);
            match read {
                Ok(result) if !result.text.trim().is_empty() && !skill.summary.is_empty() => {}
                Ok(_) => skill_outcome = Err(format!("skill {} has no summary or text", skill.name)),
                Err(error) => skill_outcome = Err(format!("skill {}: {error}", skill.name)),
            }
        }
        checks.push(check("skills", started, skill_outcome));

        // The server is optional for the plugin to serve (extract and skills work without it): the
        // check passes and says plainly whether the server can run.
        if let Some(server) = server {
            let detail = if server.installed {
                format!("{} {} at {}", server.name, server.version.unwrap_or_default(), server.path.unwrap_or_default())
            } else {
                format!("{} is not available ({}); `{category}.{}` installs it", server.name, server.problem.unwrap_or_default(), methods::INSTALL)
            };
            checks.push(ProbeCheck { name: "server".into(), ok: true, detail, ms: 0 });
        }
        ProbeReport::from_checks(checks)
    }
}

/// The probe's line for the skills: their names, how many declare diagnostic codes and how many
/// declare the projects they apply to.
pub(crate) fn skills_detail(skills: &[SkillInfo]) -> String {
    let mut detail = skills.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(", ");
    match skills.iter().filter(|s| !s.codes.is_empty()).count() {
        0 => {}
        coded => detail.push_str(&format!("; codes for {coded} skill(s)")),
    }
    match skills.iter().filter(|s| !s.applies.is_empty()).count() {
        0 => {}
        scoped => detail.push_str(&format!("; conditions for {scoped} skill(s)")),
    }
    detail
}

impl<P: LanguagePack> Plugin for LangPlugin<P> {
    fn handle(&self, ctx: Ctx, call: Call) -> BoxFuture<'static, Result<Value, RpcError>> {
        self.remember(&ctx);
        let this = self.clone_handle();
        Box::pin(async move { this.dispatch(&call.method, call.params).await })
    }

    fn probe(&self, ctx: Ctx) -> BoxFuture<'static, Result<ProbeReport, RpcError>> {
        self.remember(&ctx);
        let this = self.clone_handle();
        Box::pin(async move { Ok(this.self_test().await) })
    }

    fn activate(&self, ctx: Ctx) -> BoxFuture<'static, Result<(), RpcError>> {
        self.remember(&ctx);
        skills::watch(ctx, self.shared.category.clone(), self.shared.skills.clone());
        Box::pin(async { Ok(()) })
    }

    fn shutdown(&self, _ctx: Ctx) -> BoxFuture<'static, ()> {
        let servers = self.servers.clone();
        Box::pin(async move {
            if let Some(servers) = servers {
                servers.stop_all("plugin shutdown").await;
            }
        })
    }
}

impl<P: LanguagePack> LangPlugin<P> {
    fn clone_handle(&self) -> Self {
        Self { shared: self.shared.clone(), servers: self.servers.clone() }
    }
}

/// How often idle servers are looked for.
const REAP_EVERY: Duration = Duration::from_secs(60);

/// Serve `pack` to the frame until it shuts the plugin down; returns the process exit code.
pub fn run_pack<P: LanguagePack>(pack: P) -> i32 {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_env("CORTEX_LOG").unwrap_or_else(|_| "warn".into()))
        .with_writer(std::io::stderr)
        .init();
    let category = format!("lang-{}", pack.id());
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("cc-{category}: cannot start the async runtime: {error}");
            return 1;
        }
    };
    runtime.block_on(async move {
        if contract_of(&category) != Some(lang::CONTRACT) {
            eprintln!("cc-{category}: `{}` is not a language id (lowercase letters, digits and -)", pack.id());
            return 2;
        }
        let env = match PluginEnv::from_env() {
            Ok(env) => env,
            Err(error) => {
                eprintln!("cc-{category}: {error}");
                return 2;
            }
        };
        let version = pack.version().to_owned();
        let plugin = Arc::new(LangPlugin::new(pack, env));
        if let Some(servers) = plugin.servers() {
            tokio::spawn(async move {
                let mut every = tokio::time::interval(REAP_EVERY);
                loop {
                    every.tick().await;
                    servers.reap_idle(tokio::time::Instant::now()).await;
                }
            });
        }
        let mut hello = Hello::new(&format!("cc-{category}"), &version, &category, lang::CONTRACT);
        hello.provides = plugin.provides();
        match run_plugin(hello, plugin).await {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("cc-{category}: {error}");
                1
            }
        }
    })
}
