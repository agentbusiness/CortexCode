//! `cc-skills-security` executable: the security plugin (category `skills-security`). It serves
//! `skills@1` (the audit, review, triage and fix skills from the folder's `data/skills`) and, on
//! the same category, the security sentinel (`cc_protocol::contracts::security`): sweeps of every
//! project, the local model's filter, investigations, mute rules and the notifications the filter
//! allows. The secaudit tool (`data/scripts`) does the reading of trees and live sources; the
//! decision cards the filter reads are `data/triage`.

mod sentinel;
mod store;
mod triage;

use cc_lang_kit::SkillsPlugin;
use cc_protocol::contracts::lifecycle::{Hello, ProbeCheck, ProbeReport};
use cc_protocol::contracts::memory::topics as memory_topics;
use cc_protocol::contracts::security::methods;
use cc_protocol::{BoxFuture, Call, Ctx, Plugin, PluginEnv, RpcError, run_plugin};
use sentinel::Sentinel;
use serde_json::Value;
use std::sync::Arc;

/// The plugin's id: its category is `skills-<ID>`, as plugin.toml says.
const ID: &str = "security";
const NAME: &str = "Security";

struct SecurityPlugin {
    skills: SkillsPlugin,
    sentinel: Arc<Sentinel>,
}

impl Plugin for SecurityPlugin {
    fn handle(&self, ctx: Ctx, call: Call) -> BoxFuture<'static, Result<Value, RpcError>> {
        if methods::ALL.contains(&call.method.as_str()) {
            let sentinel = self.sentinel.clone();
            sentinel.attach(&ctx);
            return Box::pin(async move { sentinel.dispatch(&call.method, call.params).await });
        }
        let outcome = self.skills.dispatch(&call.method, call.params);
        Box::pin(async move { outcome })
    }

    fn probe(&self, ctx: Ctx) -> BoxFuture<'static, Result<ProbeReport, RpcError>> {
        let mut report = self.skills.self_test();
        let sentinel = self.sentinel.clone();
        sentinel.attach(&ctx);
        Box::pin(async move {
            let started = std::time::Instant::now();
            let check = match sentinel.tool_files() {
                Ok(detail) => ProbeCheck { name: "sentinel".into(), ok: true, detail, ms: started.elapsed().as_millis() as u64 },
                Err(detail) => ProbeCheck { name: "sentinel".into(), ok: false, detail, ms: started.elapsed().as_millis() as u64 },
            };
            report.ok = report.ok && check.ok;
            report.checks.push(check);
            Ok(report)
        })
    }

    fn activate(&self, ctx: Ctx) -> BoxFuture<'static, Result<(), RpcError>> {
        let skills_activate = self.skills.activate(ctx.clone());
        let sentinel = self.sentinel.clone();
        Box::pin(async move {
            skills_activate.await?;
            sentinel.start(ctx);
            Ok(())
        })
    }

    fn event(&self, ctx: Ctx, topic: String, data: Value) -> BoxFuture<'static, ()> {
        let sentinel = self.sentinel.clone();
        sentinel.attach(&ctx);
        Box::pin(async move { sentinel.event(&topic, data).await })
    }

    fn shutdown(&self, _ctx: Ctx) -> BoxFuture<'static, ()> {
        self.sentinel.stop();
        Box::pin(async {})
    }
}

fn main() {
    cc_stats_kit::start("CORTEX_LOG", "warn", "cc-skills-security", env!("CARGO_PKG_VERSION"));
    let runtime = match tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("cc-skills-security: cannot start the async runtime: {error}");
            std::process::exit(1);
        }
    };
    let code = runtime.block_on(async {
        let env = match PluginEnv::from_env() {
            Ok(env) => env,
            Err(error) => {
                eprintln!("cc-skills-security: {error}");
                return 2;
            }
        };
        let skills = SkillsPlugin::new(ID, NAME, env.clone(), cc_lang_kit::skills::locate_dir());
        let category = skills.category().to_owned();
        let mut hello = Hello::new(&format!("cc-{category}"), env!("CARGO_PKG_VERSION"), &category, cc_protocol::contracts::skills::CONTRACT);
        hello.provides = skills.provides();
        hello.provides.extend(methods::ALL.iter().map(|m| (*m).to_owned()));
        // A model's shell may ask which projects a package or advisory touches.
        hello.commands = vec![methods::AFFECTED.to_owned()];
        hello.subscribes = vec![memory_topics::SECURITY_CHANGED.to_owned(), memory_topics::SETTINGS_CHANGED.to_owned()];
        let plugin = Arc::new(SecurityPlugin { skills, sentinel: Sentinel::new(env) });
        match run_plugin(hello, plugin).await {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("cc-{category}: {error}");
                1
            }
        }
    });
    std::process::exit(code);
}
