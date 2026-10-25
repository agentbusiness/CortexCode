//! A skills plugin: one body of know-how for the model (`skills-<id>`, contract `skills@1`), served
//! over CPP/1 from a directory of skills in [`crate::skills`]' format.
//!
//! The plugin knows nothing beyond its skills: `describe` lists them (name and one-line summary),
//! `skill` returns one skill's whole text, both read from the folder in use at the time (the live
//! one when it exists, [`skills::current_dir`]). The probe runs both through the real methods: it
//! requires at least one skill, reads every one back and checks that its first line is its summary
//! and that a body follows.
//!
//! The category is `skills-` + the id: the frame starts the process for the category its
//! plugin.toml names and rejects a hello that says another one, so the two must agree.

use crate::skills;
use cc_protocol::contracts::contract_of;
use cc_protocol::contracts::lifecycle::{Hello, ProbeCheck, ProbeReport};
use cc_protocol::contracts::skills::{self as contract, Description, SkillInfo, SkillParams, SkillResult, methods};
use cc_protocol::{BoxFuture, Call, Ctx, Plugin, PluginEnv, RpcError, codes, parse_params, run_plugin, to_value};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

/// The `skills@1` plugin of one skills directory.
#[derive(Debug, Clone)]
pub struct SkillsPlugin {
    id: String,
    name: String,
    category: String,
    env: PluginEnv,
    packaged: Option<PathBuf>,
}

impl SkillsPlugin {
    /// The plugin `skills-<id>` named `name`, serving the live skills of its folder in the code
    /// folder or else the skills in `packaged` (None: the directory was not found; every method
    /// then says so).
    pub fn new(id: &str, name: &str, env: PluginEnv, packaged: Option<PathBuf>) -> Self {
        Self { id: id.to_owned(), name: name.to_owned(), category: format!("skills-{id}"), env, packaged }
    }

    pub fn category(&self) -> &str {
        &self.category
    }

    /// Methods served, as `<category>.<method>`.
    pub fn provides(&self) -> Vec<String> {
        methods::ALL.iter().map(|method| format!("{}.{method}", self.category)).collect()
    }

    fn require_dir(&self) -> Result<PathBuf, RpcError> {
        skills::current_dir(&self.env, self.packaged.as_deref()).ok_or_else(|| RpcError::new(codes::UNAVAILABLE, format!("{}: the data/skills directory is missing", self.category)))
    }

    fn describe(&self) -> Result<Description, RpcError> {
        let dir = self.require_dir()?;
        // An invalid codes.toml or applies.toml leaves the skills served without codes and
        // conditions (the probe says why).
        let listed = skills::load(&dir).or_else(|_| skills::list(&dir)).map_err(|e| RpcError::new(codes::UNAVAILABLE, e))?;
        Ok(Description {
            id: self.id.clone(),
            name: self.name.clone(),
            skills: listed.into_iter().map(|skill| SkillInfo { name: skill.name, summary: skill.summary, codes: skill.codes, applies: skill.applies, done: skill.done }).collect(),
        })
    }

    fn skill(&self, params: SkillParams) -> Result<SkillResult, RpcError> {
        let text = skills::read(&self.require_dir()?, &params.name)?;
        Ok(SkillResult { name: params.name, text })
    }

    /// Serve one `skills@1` method (`<category>.<method>`).
    pub fn dispatch(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let verb = method.strip_prefix(self.category.as_str()).and_then(|rest| rest.strip_prefix('.')).ok_or_else(|| RpcError::unknown_method(method))?;
        match verb {
            methods::DESCRIBE => to_value(&self.describe()?),
            methods::SKILL => to_value(&self.skill(parse_params(params)?)?),
            _ => Err(RpcError::unknown_method(method)),
        }
    }

    /// The conformance self-test: `describe` lists at least one skill, and every skill read through
    /// `skill` starts with its summary line and has a body after it.
    pub fn self_test(&self) -> ProbeReport {
        let category = &self.category;
        let mut checks = Vec::new();

        let dir = skills::current_dir(&self.env, self.packaged.as_deref());
        let started = Instant::now();
        let described = self.dispatch(&format!("{category}.{}", methods::DESCRIBE), Value::Null).and_then(parse_params::<Description>);
        let listed = described.as_ref().map(|d| d.skills.clone()).unwrap_or_default();
        let outcome = match described {
            Err(error) => Err(error.to_string()),
            Ok(d) if d.skills.is_empty() => Err(format!("no skills in {}", dir.as_ref().map(|p| p.display().to_string()).unwrap_or_default())),
            Ok(d) => Ok(format!("{}: {} skills", d.name, d.skills.len())),
        };
        checks.push(check("describe", started, outcome));

        let started = Instant::now();
        let mut outcome = Ok(crate::pack::skills_detail(&listed));
        if listed.is_empty() {
            outcome = Err("no skill to read".to_owned());
        } else if let Some(Err(error)) = dir.as_deref().map(skills::load) {
            outcome = Err(error);
        }
        for info in &listed {
            let read = self.dispatch(&format!("{category}.{}", methods::SKILL), json!({"name": info.name})).and_then(parse_params::<SkillResult>);
            let problem = match read {
                Err(error) => Some(error.to_string()),
                Ok(result) => well_formed(info, &result).err(),
            };
            if let Some(problem) = problem {
                outcome = Err(format!("skill {}: {problem}", info.name));
                break;
            }
        }
        checks.push(check("skills", started, outcome));

        let started = Instant::now();
        let outcome = match self.dispatch(&format!("{category}.{}", methods::SKILL), json!({"name": "no-such-skill-exists"})) {
            Err(error) if error.code == codes::NOT_FOUND => Ok("an unknown skill is not found".to_owned()),
            Err(error) => Err(format!("an unknown skill gave {error}, not not_found")),
            Ok(_) => Err("an unknown skill was served".to_owned()),
        };
        checks.push(check("unknown skill", started, outcome));
        ProbeReport::from_checks(checks)
    }
}

/// A skill read back: its name, its first line equal to the summary `describe` gave, a body after it.
fn well_formed(info: &SkillInfo, result: &SkillResult) -> Result<(), String> {
    if result.name != info.name {
        return Err(format!("read back as `{}`", result.name));
    }
    if info.summary.is_empty() {
        return Err("no summary on line 1".to_owned());
    }
    if skills::summary_of(&result.text) != info.summary {
        return Err("line 1 is not the summary describe gave".to_owned());
    }
    if result.text.lines().skip(1).all(|line| line.trim().is_empty()) {
        return Err("no body after the summary".to_owned());
    }
    Ok(())
}

fn check(name: &str, started: Instant, outcome: Result<String, String>) -> ProbeCheck {
    let ms = started.elapsed().as_millis() as u64;
    match outcome {
        Ok(detail) => ProbeCheck { name: name.into(), ok: true, detail, ms },
        Err(detail) => ProbeCheck { name: name.into(), ok: false, detail, ms },
    }
}

impl Plugin for SkillsPlugin {
    fn handle(&self, _ctx: Ctx, call: Call) -> BoxFuture<'static, Result<Value, RpcError>> {
        let outcome = self.dispatch(&call.method, call.params);
        Box::pin(async move { outcome })
    }

    fn probe(&self, _ctx: Ctx) -> BoxFuture<'static, Result<ProbeReport, RpcError>> {
        let report = self.self_test();
        Box::pin(async move { Ok(report) })
    }

    fn activate(&self, ctx: Ctx) -> BoxFuture<'static, Result<(), RpcError>> {
        skills::watch(ctx, self.category.clone(), self.packaged.clone());
        Box::pin(async { Ok(()) })
    }
}

/// Serve the skills of `skills_dir` (None: `data/skills` beside the executable, see
/// [`skills::locate_dir`]), or the live ones that replace them ([`skills::current_dir`]), as
/// `skills-<id>` until the frame shuts the plugin down; returns the
/// process exit code. `version` is the plugin build's (its crate's `CARGO_PKG_VERSION`).
pub fn run_skills_plugin(id: &str, name: &str, version: &str, skills_dir: Option<PathBuf>) -> i32 {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_env("CORTEX_LOG").unwrap_or_else(|_| "warn".into()))
        .with_writer(std::io::stderr)
        .init();
    let category = format!("skills-{id}");
    if contract_of(&category) != Some(contract::CONTRACT) {
        eprintln!("cc-{category}: `{id}` is not a skills id (lowercase letters, digits and -)");
        return 2;
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("cc-{category}: cannot start the async runtime: {error}");
            return 1;
        }
    };
    let env = match PluginEnv::from_env() {
        Ok(env) => env,
        Err(error) => {
            eprintln!("cc-{category}: {error}");
            return 2;
        }
    };
    let plugin = Arc::new(SkillsPlugin::new(id, name, env, skills_dir.or_else(skills::locate_dir)));
    runtime.block_on(async move {
        let mut hello = Hello::new(&format!("cc-{category}"), version, &category, contract::CONTRACT);
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
