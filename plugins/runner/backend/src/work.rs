//! What a job needs besides its run (docs/RUNNER.md): its diff, a build into the proving home, a
//! walk's command, the landing on the main branch, a release, the reads of the mirror's main
//! branch, and the plugin's own probe.

use crate::process::{self, Ran};
use crate::service::{Runner, lock, name_ok};
use cc_protocol::contracts::lifecycle::{ProbeCheck, ProbeReport};
use cc_protocol::contracts::runner::{BuildAnswer, BuildParams, CommandAnswer, CommandParams, DiffAnswer, LandParams, Landed, ReadParams, ReleaseAnswer, ReleaseParams, ReleasedPart, SearchParams};
use cc_protocol::contracts::vcs::{self, methods as vcs_methods};
use cc_protocol::host::Host;
use cc_protocol::{Ctx, RpcError, codes, parse_params};
use serde_json::{Value, json};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Where a diff is cut (at a file boundary) when no limit is given.
const DIFF_LIMIT: usize = 60_000;
/// Lines of a build's or a release's output answered.
const TAIL_LINES: usize = 40;
/// A command's output answered at most (its end).
const COMMAND_OUTPUT: usize = 60_000;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(120);
const COMMAND_TIMEOUT_MAX: Duration = Duration::from_secs(3600);
/// One step of a build (the packer, one part).
const BUILD_TIMEOUT: Duration = Duration::from_secs(3600);
const RELEASE_PLAN_TIMEOUT: Duration = Duration::from_secs(900);
const RELEASE_TIMEOUT: Duration = Duration::from_secs(4 * 3600);
/// A release's own command is not ended by the clock: a week stands for no limit.
const RELEASE_STEP_TIMEOUT: Duration = Duration::from_secs(7 * 24 * 3600);
/// Lines `runner.read` answers when none are asked for, and at most.
const READ_LINES: u32 = 400;
const READ_LINES_MAX: u32 = 2000;
/// The branch of the clean checkout releases run in.
const RELEASE_BRANCH: &str = "runner/release";
/// The trailer a task's commit carries, by which a landing groups the commits.
const REQUEST: &str = "Request";

/// One file of a diff as unified text.
fn unified(file: &vcs::FileDiff) -> String {
    let old = file.old_path.as_deref().unwrap_or(&file.path);
    let added = matches!(file.status.as_str(), "added" | "untracked");
    let deleted = file.status == "deleted";
    let mut text = format!("diff --git a/{old} b/{}\n", file.path);
    if file.binary {
        text.push_str("Binary files differ\n");
        return text;
    }
    text.push_str(&if added { "--- /dev/null\n".to_owned() } else { format!("--- a/{old}\n") });
    text.push_str(&if deleted { "+++ /dev/null\n".to_owned() } else { format!("+++ b/{}\n", file.path) });
    if file.truncated {
        text.push_str(&format!("(too large to show: {} lines added, {} removed)\n", file.added, file.removed));
        return text;
    }
    for hunk in &file.hunks {
        text.push_str(hunk.header.trim_end_matches('\n'));
        text.push('\n');
        for line in &hunk.lines {
            text.push_str(&line.kind);
            text.push_str(&line.text);
            text.push('\n');
            if line.no_eol {
                text.push_str("\\ No newline at end of file\n");
            }
        }
    }
    text
}

/// `owner/name` of a GitHub address (https or ssh); None for any other source.
fn github_repo(url: &str) -> Option<String> {
    let url = url.trim().trim_end_matches('/');
    let rest = ["https://github.com/", "git@github.com:", "ssh://git@github.com/"].iter().find_map(|prefix| url.strip_prefix(prefix))?;
    let rest = rest.trim_end_matches(".git");
    let (owner, name) = rest.split_once('/')?;
    (!owner.is_empty() && !name.is_empty() && !name.contains('/')).then(|| format!("{owner}/{name}"))
}

/// The parts a release moves, from what `scripts/release.sh` prints under "what changed since the
/// last release": `frame  yes (now 0.1.17)`, `plugins  tools vcs-git`, `app  yes (...)`,
/// `npx  yes`. The script says the version a part has now only for the frame, and no new versions.
fn released_parts(lines: &[String]) -> Vec<ReleasedPart> {
    let mut parts: Vec<ReleasedPart> = Vec::new();
    let mut add = |part: &str, from: &str| {
        if !parts.iter().any(|known| known.part == part) {
            parts.push(ReleasedPart { part: part.to_owned(), from: from.to_owned(), to: String::new() });
        }
    };
    for line in lines {
        let mut words = line.split_whitespace();
        let (Some(name), rest) = (words.next(), words.collect::<Vec<_>>()) else { continue };
        if !line.starts_with("  ") || rest.is_empty() {
            continue;
        }
        match name {
            "frame" | "app" | "npx" if rest[0] == "yes" => {
                let now = line.split_once("(now ").and_then(|(_, rest)| rest.split_once(')')).map(|(version, _)| version.trim()).filter(|version| *version != "none").unwrap_or("");
                add(name, now);
            }
            "plugins" if rest[0] != "none" => {
                for plugin in rest.iter().filter(|plugin| name_ok(plugin)) {
                    add(plugin, "");
                }
            }
            _ => {}
        }
    }
    parts
}

impl Runner {
    // ---- diff ------------------------------------------------------------------------------------

    /// `{job_id, whole?, limit?}`: the worktree against the last checkpoint, or against the base.
    pub(crate) async fn diff(&self, ctx: &Ctx, params: &Value) -> Result<DiffAnswer, RpcError> {
        self.ensure_loaded(ctx).await?;
        let job = self.job(ctx, params.get("job_id").and_then(Value::as_str).unwrap_or(""))?;
        let whole = params.get("whole").and_then(Value::as_bool).unwrap_or(false);
        let limit = params.get("limit").and_then(Value::as_u64).map(|limit| limit as usize).filter(|limit| *limit > 0).unwrap_or(DIFF_LIMIT);
        let scope = if whole && !job.base.is_empty() { format!("since:{}", job.base) } else { "uncommitted".to_owned() };
        let diff: vcs::Diff = Host::new(ctx.clone()).get(vcs_methods::DIFF, json!({"root": job.root, "scope": scope})).await?;
        let mut answer = DiffAnswer { files: diff.files.iter().map(|file| file.path.clone()).collect(), ..DiffAnswer::default() };
        for file in &diff.files {
            let text = unified(file);
            if answer.diff.len() + text.len() > limit {
                answer.truncated = true;
                break;
            }
            answer.diff.push_str(&text);
        }
        Ok(answer)
    }

    // ---- build and command -----------------------------------------------------------------------

    /// The home a build installs into and a walk's command runs against: the proving home of the
    /// app, or the app's own home for the soak.
    fn app_home(app: &str, soak: bool) -> PathBuf {
        if soak { process::home_dir().join(format!(".{app}")) } else { process::home_dir().join(".cortex-proving").join(app) }
    }

    async fn tool(name: &str) -> Result<PathBuf, RpcError> {
        process::find_on(&process::login_path().await, name).ok_or_else(|| RpcError::new(codes::UNAVAILABLE, format!("`{name}` is not installed on this runner")))
    }

    pub(crate) async fn build(&self, ctx: &Ctx, params: BuildParams, cancel: &CancellationToken) -> Result<BuildAnswer, RpcError> {
        self.ensure_loaded(ctx).await?;
        let job = self.current(ctx, &params.job_id, params.attempt)?;
        if !name_ok(&params.app) {
            return Err(RpcError::invalid_params(format!("`{}` is not an app's id", params.app)));
        }
        let _busy = self.busy(ctx, &params.job_id, "building")?;
        let root = PathBuf::from(&job.root);
        let host = Host::new(ctx.clone());
        let mut parts = params.parts.clone();
        if parts.is_empty() {
            // Every part whose files changed since the base.
            self.facts(ctx, &params.job_id).await?;
            for path in self.changed(&params.job_id) {
                let part = match path.split('/').collect::<Vec<_>>().as_slice() {
                    ["plugins", id, _, ..] => (*id).to_owned(),
                    ["frame", _, ..] => "frame".to_owned(),
                    _ => continue,
                };
                if !parts.contains(&part) {
                    parts.push(part);
                }
            }
        }
        if let Some(bad) = parts.iter().find(|part| !name_ok(part)) {
            return Err(RpcError::invalid_params(format!("`{bad}` is not a part's name")));
        }
        let commit = host.get::<vcs::RevResult>(vcs_methods::REV, json!({"root": job.root, "rev": "HEAD"})).await?.sha.unwrap_or_default();
        let home = Self::app_home(&params.app, params.soak);
        std::fs::create_dir_all(&home).map_err(|error| RpcError::internal(format!("cannot create {}: {error}", home.display())))?;
        let envs = vec![("CORTEX_HOME".to_owned(), home.to_string_lossy().into_owned())];
        let mut lines: Vec<String> = Vec::new();
        let mut ok = true;
        let step = async |program: &Path, args: &[&str], dir: &Path, lines: &mut Vec<String>| -> Result<bool, RpcError> {
            let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
            lines.push(format!("$ {} {}", program.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default(), args.join(" ")));
            let ran = process::run(program, &args, dir, &envs, BUILD_TIMEOUT, cancel).await?;
            lines.extend(ran.lines.iter().cloned());
            Ok(ran.ok())
        };
        let packer = root.join("packer").join("target").join("release").join("cortex-pack");
        if parts.iter().any(|part| part != "frame") && !packer.is_file() {
            ok = step(&Self::tool("cargo").await?, &["build", "--release"], &root.join("packer"), &mut lines).await?;
        }
        let mut built = Vec::new();
        for part in &parts {
            if !ok {
                break;
            }
            ok = if part == "frame" {
                step(&Self::tool("cargo").await?, &["build", "--release"], &root.join("frame"), &mut lines).await?
            } else if !root.join("plugins").join(part).join("plugin.toml").is_file() {
                lines.push(format!("no plugin `{part}` in this repository (plugins/{part}/plugin.toml)"));
                false
            } else {
                let folder = format!("plugins/{part}");
                step(&packer, &["build", folder.as_str(), "--install"], &root, &mut lines).await?
            };
            if ok {
                built.push(part.clone());
            }
        }
        Ok(BuildAnswer { ok: ok && built.len() == parts.len(), parts: built, tail: lines[lines.len().saturating_sub(TAIL_LINES)..].to_vec(), build: commit })
    }

    /// A walk's command (the manager's stored text, one argument of `sh -lc`), in the job's
    /// worktree, against the app's proving home.
    pub(crate) async fn command(&self, ctx: &Ctx, params: CommandParams, cancel: &CancellationToken) -> Result<CommandAnswer, RpcError> {
        self.ensure_loaded(ctx).await?;
        if !name_ok(&params.app) {
            return Err(RpcError::invalid_params(format!("`{}` is not an app's id", params.app)));
        }
        if params.command.trim().is_empty() {
            return Err(RpcError::invalid_params("no command"));
        }
        let home = Self::app_home(&params.app, false);
        std::fs::create_dir_all(&home).map_err(|error| RpcError::internal(format!("cannot create {}: {error}", home.display())))?;
        let root = lock(&self.inner.jobs).get(&params.job_id).map(|live| PathBuf::from(&live.job.root)).filter(|root| root.is_dir());
        let dir = root.unwrap_or_else(|| home.clone());
        let limit = params.timeout_ms.map(Duration::from_millis).unwrap_or(COMMAND_TIMEOUT).min(COMMAND_TIMEOUT_MAX);
        let envs = vec![("CORTEX_HOME".to_owned(), home.to_string_lossy().into_owned())];
        let ran = process::run(Path::new("/bin/sh"), &["-lc".to_owned(), params.command.clone()], &dir, &envs, limit, cancel).await?;
        // 124: the time limit, as `timeout` reports it; -1: ended by a signal.
        let exit_code = if ran.timed_out { 124 } else { ran.code.unwrap_or(-1) };
        Ok(CommandAnswer { exit_code, output: ran.text(COMMAND_OUTPUT) })
    }

    // ---- landing ---------------------------------------------------------------------------------

    /// The job branch's commits after `base`, oldest first (at most 500).
    async fn commits_since(host: &Host, root: &str, base: &str) -> Vec<String> {
        let Ok(log) = host.get::<vcs::LogResult>(vcs_methods::LOG, json!({"root": root, "limit": 500})).await else { return Vec::new() };
        let mut commits: Vec<String> = log.commits.into_iter().map(|commit| commit.sha).take_while(|sha| sha != base).collect();
        commits.reverse();
        commits
    }

    pub(crate) async fn land(&self, ctx: &Ctx, params: LandParams) -> Result<Landed, RpcError> {
        self.ensure_loaded(ctx).await?;
        let job = self.current(ctx, &params.job_id, params.attempt)?;
        let _busy = self.busy(ctx, &params.job_id, "landing")?;
        if self.running(&params.job_id) {
            return Err(RpcError::new(codes::CONFLICT, format!("job {} is still running; it lands when its run has ended", params.job_id)));
        }
        if params.requests.is_empty() || params.requests.iter().any(|(id, title)| id.trim().is_empty() || title.trim().is_empty()) {
            return Err(RpcError::invalid_params("a landing names its requests: each an id and a title"));
        }
        let refused = |text: String| Landed { ok: false, text, ..Landed::default() };
        let host = Host::new(ctx.clone());
        let status: vcs::Status = host.get(vcs_methods::STATUS, json!({"root": job.root})).await?;
        if !status.files.is_empty() {
            return Ok(refused(self.text(ctx, "dirty", &[("files", &status.files.len().to_string())])));
        }
        let token = (!params.token.trim().is_empty()).then(|| params.token.trim().to_owned());
        let fetched: vcs::FetchResult = host.effect(vcs_methods::FETCH, json!({"root": job.mirror, "remote": "origin", "prune": true, "token": token})).await.and_then(parse_params)?;
        if !fetched.ok {
            return Ok(refused(format!("the source could not be fetched: {}", fetched.detail)));
        }
        let main_ref = format!("refs/remotes/origin/{}", job.main);
        let Some(main_head) = host.get::<vcs::RevResult>(vcs_methods::REV, json!({"root": job.mirror, "rev": main_ref})).await?.sha else {
            return Ok(refused(format!("the source has no branch `{}`", job.main)));
        };
        if !job.landed.is_empty() && job.landed == main_head {
            return Ok(Landed { ok: true, commit: main_head, text: "landed before".into(), ..Landed::default() });
        }
        if main_head != job.base {
            // The main branch moved: the branch goes onto it, and the manager builds and proves
            // again before it asks to land a second time.
            let before = Self::commits_since(&host, &job.root, &job.base).await;
            let rebased: vcs::RebaseResult = host.effect(vcs_methods::REBASE, json!({"root": job.root, "onto": main_head})).await.and_then(parse_params)?;
            if !rebased.ok {
                return Ok(Landed { ok: false, conflicts: rebased.conflicts, text: format!("`{}` moved and the job's branch does not rebase onto it", job.main), ..Landed::default() });
            }
            let after = Self::commits_since(&host, &job.root, &main_head).await;
            self.update(&params.job_id, |live| {
                live.job.base = main_head.clone();
                live.job.squashed.clear();
                // Each checkpoint names its commit after the rebase (the same place in the
                // branch); one that has no such commit names none.
                for checkpoint in &mut live.job.checkpoints {
                    let place = before.iter().position(|sha| *sha == checkpoint.commit).filter(|_| before.len() == after.len());
                    checkpoint.commit = place.and_then(|place| after.get(place).cloned()).unwrap_or_default();
                }
            });
            self.persist(ctx, &params.job_id, None).await;
            return Ok(Landed { ok: false, commit: rebased.head, rebased: true, text: format!("`{}` moved: the branch was rebased onto it and nothing landed; build and prove again, then land", job.main), ..Landed::default() });
        }
        // One commit per request, in the order their commits come.
        let head = if !job.squashed.is_empty() && status.head.as_deref() == Some(job.squashed.as_str()) {
            job.squashed.clone()
        } else {
            let groups: Vec<Value> = params
                .requests
                .iter()
                .map(|(id, title)| {
                    let mut message = format!("{}\n\n{REQUEST}: {}\n", title.trim(), id.trim());
                    for line in job.trailers.iter().map(|line| line.trim()).filter(|line| !line.is_empty()) {
                        message.push_str(&format!("{line}\n"));
                    }
                    json!({"value": id.trim(), "message": message})
                })
                .collect();
            let squashed: vcs::SquashResult = match host.effect(vcs_methods::SQUASH, json!({"root": job.root, "base": job.base, "trailer": REQUEST, "groups": groups})).await.and_then(parse_params) {
                Ok(squashed) => squashed,
                Err(error) if error.code == codes::CONFLICT => return Ok(refused(error.message)),
                Err(error) => return Err(error),
            };
            self.update(&params.job_id, |live| live.job.squashed = squashed.head.clone());
            self.persist(ctx, &params.job_id, None).await;
            squashed.head
        };
        if head == job.base {
            return Ok(refused("the job's branch has no commit to land".into()));
        }
        let mut notes: Vec<String> = Vec::new();
        // The pull request is a record: opened on the pushed branch before the main branch moves,
        // so the push of the same commits closes it as merged.
        if params.changes == "github-pr" {
            match github_repo(&job.url) {
                None => notes.push("no pull request: the source is not on GitHub".into()),
                Some(repo) => {
                    let pushed: Result<vcs::PushResult, RpcError> = host.effect(vcs_methods::PUSH, json!({"root": job.root, "remote": "origin", "branch": job.branch, "token": token})).await.and_then(parse_params);
                    match pushed {
                        Ok(pushed) if pushed.ok => {
                            let owner = repo.split('/').next().unwrap_or("");
                            // A request's title may be a whole commit message: its first line names it.
                            let first = |title: &str| title.trim().lines().next().unwrap_or_default().to_owned();
                            let title = params.requests.iter().map(|(_, title)| first(title)).collect::<Vec<_>>().join("; ");
                            let body = params.requests.iter().map(|(id, title)| format!("- {}: {}", id.trim(), first(title))).collect::<Vec<_>>().join("\n");
                            let made: Result<vcs::GithubPr, RpcError> =
                                host.effect(vcs_methods::GITHUB_PR_CREATE, json!({"repo": repo, "head": format!("{owner}:{}", job.branch), "base": job.main, "title": title, "body": body})).await.and_then(parse_params);
                            match made {
                                Ok(pr) => notes.push(format!("pull request {}", pr.url)),
                                Err(error) => notes.push(format!("no pull request: {}", error.message)),
                            }
                        }
                        Ok(pushed) => notes.push(format!("no pull request: the branch was not pushed ({})", pushed.detail)),
                        Err(error) => notes.push(format!("no pull request: the branch was not pushed ({})", error.message)),
                    }
                }
            }
        }
        // The main branch in the mirror moves forward to the job's commits, then goes to the source.
        match host.effect(vcs_methods::BRANCH_ADVANCE, json!({"root": job.mirror, "branch": job.main, "to": head})).await {
            Ok(_) => {}
            Err(error) if error.code == codes::CONFLICT => return Ok(refused(error.message)),
            Err(error) => return Err(error),
        }
        let pushed: vcs::PushResult = host.effect(vcs_methods::PUSH, json!({"root": job.root, "remote": "origin", "branch": job.main, "token": token})).await.and_then(parse_params)?;
        if !pushed.ok {
            notes.insert(0, format!("`{}` was not pushed: {}", job.main, pushed.detail));
            return Ok(refused(notes.join("; ")));
        }
        self.update(&params.job_id, |live| live.job.landed = head.clone());
        self.persist(ctx, &params.job_id, None).await;
        Ok(Landed { ok: true, commit: head, text: notes.join("; "), ..Landed::default() })
    }

    // ---- release ---------------------------------------------------------------------------------

    /// The repository's own release script in a clean checkout of the main branch.
    pub(crate) async fn release(&self, ctx: &Ctx, params: ReleaseParams, cancel: &CancellationToken) -> Result<ReleaseAnswer, RpcError> {
        self.ensure_loaded(ctx).await?;
        if !name_ok(&params.repo) || params.url.trim().is_empty() || params.main.trim().is_empty() {
            return Err(RpcError::invalid_params("a release names `repo` (a folder name), `url` and `main`"));
        }
        if !lock(&self.inner.releasing).insert(params.repo.clone()) {
            return Err(RpcError::new(codes::BUSY, format!("a release of {} is running", params.repo)).retryable());
        }
        let outcome = self.release_run(ctx, &params, cancel).await;
        lock(&self.inner.releasing).remove(&params.repo);
        outcome
    }

    async fn release_run(&self, ctx: &Ctx, params: &ReleaseParams, cancel: &CancellationToken) -> Result<ReleaseAnswer, RpcError> {
        let host = Host::new(ctx.clone());
        let mirror = self.mirror(ctx, &params.repo, &params.url, &params.main, &params.token).await?;
        if params.fetch_only {
            return Ok(ReleaseAnswer { ok: true, stepped: true, ..ReleaseAnswer::default() });
        }
        let main_ref = format!("refs/remotes/origin/{}", params.main.trim());
        let head = host
            .get::<vcs::RevResult>(vcs_methods::REV, json!({"root": mirror, "rev": main_ref}))
            .await?
            .sha
            .ok_or_else(|| RpcError::not_found(format!("the source has no branch `{}`", params.main)))?;
        let path = Self::work_dir(ctx).join(&params.repo).join("release");
        let tree: vcs::WorktreeResult = host.effect(vcs_methods::WORKTREE_ADD, json!({"repo": mirror, "path": path.to_string_lossy(), "branch": RELEASE_BRANCH, "from": head})).await.and_then(parse_params)?;
        // Clean, at the main branch's head: what an earlier release left (version lines, build
        // output that is not ignored) goes.
        host.effect(vcs_methods::RESET, json!({"root": tree.path, "to": head, "clean": true})).await?;
        let root = PathBuf::from(&tree.path);
        if !params.steps.is_empty() {
            return self.release_steps(params, &root, &head, cancel).await;
        }
        let script = root.join("scripts").join("release.sh");
        if !script.is_file() {
            return Err(RpcError::not_found("this repository has no scripts/release.sh"));
        }
        let bash = process::find_on(&process::login_path().await, "bash").unwrap_or_else(|| PathBuf::from("/bin/bash"));
        let mut args = vec!["scripts/release.sh".to_owned()];
        if params.plan_only {
            args.push("--plan".to_owned());
        }
        let limit = if params.plan_only { RELEASE_PLAN_TIMEOUT } else { RELEASE_TIMEOUT };
        let ran: Ran = process::run(&bash, &args, &root, &[], limit, cancel).await?;
        Ok(ReleaseAnswer { ok: ran.ok(), parts: released_parts(&ran.lines), tail: ran.tail(TAIL_LINES), head, stepped: false })
    }

    /// The repository's own release commands, in order, in the clean checkout: with the files a
    /// release reads that git does not hold written from `secret_texts` or linked in from
    /// `secrets_from`, for the release only. A command that fails ends the release. Releases take
    /// as long as they take (an app's notarization waits on Apple), so no time limit ends them.
    async fn release_steps(&self, params: &ReleaseParams, root: &Path, head: &str, cancel: &CancellationToken) -> Result<ReleaseAnswer, RpcError> {
        let mut linked: Vec<PathBuf> = Vec::new();
        let outcome = async {
            let inside = |file: &str| -> Result<String, RpcError> {
                let name = file.trim().trim_start_matches("./");
                if name.is_empty() || name.split('/').any(|part| part == ".." || part.is_empty()) {
                    return Err(RpcError::invalid_params(format!("`{file}` is not a file of the repository")));
                }
                Ok(name.to_owned())
            };
            let mut written: Vec<String> = Vec::new();
            for (file, text) in &params.secret_texts {
                let name = inside(file)?;
                let target = root.join(&name);
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent).map_err(|error| RpcError::internal(format!("cannot create {}: {error}", parent.display())))?;
                }
                // Readable by its owner only, and gone after the release.
                let mut out = std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .mode(0o600)
                    .open(&target)
                    .map_err(|error| RpcError::internal(format!("cannot write {}: {error}", target.display())))?;
                linked.push(target.clone());
                out.write_all(text.as_bytes()).map_err(|error| RpcError::internal(format!("cannot write {}: {error}", target.display())))?;
                written.push(name);
            }
            for file in &params.secret_files {
                let name = inside(file)?;
                if written.contains(&name) {
                    continue;
                }
                let from = params.secrets_from.trim();
                if from.is_empty() {
                    return Err(RpcError::invalid_params(format!("the release reads {name}, which git does not hold: give its text in the repository's settings")));
                }
                let from = if let Some(rest) = from.strip_prefix("~/") { std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default().join(rest) } else { PathBuf::from(from) };
                let source = from.join(&name);
                if !source.is_file() {
                    return Err(RpcError::not_found(format!("{} is not on this computer: put it there, or set where it is in the repository's settings", source.display())));
                }
                let target = root.join(&name);
                if target.exists() {
                    continue;
                }
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent).map_err(|error| RpcError::internal(format!("cannot create {}: {error}", parent.display())))?;
                }
                std::os::unix::fs::symlink(&source, &target).map_err(|error| RpcError::internal(format!("cannot link {}: {error}", target.display())))?;
                linked.push(target);
            }
            let envs: Vec<(String, String)> = params.env.iter().filter(|(name, _)| !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')).cloned().collect();
            let commands: Vec<String> = if params.plan_only { vec![params.plan_step.clone()] } else { params.steps.clone() };
            let mut lines: Vec<String> = Vec::new();
            for command in commands.iter().map(|command| command.trim()).filter(|command| !command.is_empty()) {
                lines.push(format!("$ {command}"));
                let ran: Ran = process::run(Path::new("/bin/sh"), &["-lc".to_owned(), command.to_owned()], root, &envs, RELEASE_STEP_TIMEOUT, cancel).await?;
                lines.extend(ran.lines.iter().cloned());
                if !ran.ok() {
                    let tail = lines[lines.len().saturating_sub(TAIL_LINES)..].to_vec();
                    return Ok(ReleaseAnswer { ok: false, parts: released_parts(&lines), tail, head: head.to_owned(), stepped: true });
                }
            }
            let tail = lines[lines.len().saturating_sub(TAIL_LINES)..].to_vec();
            Ok(ReleaseAnswer { ok: true, parts: released_parts(&lines), tail, head: head.to_owned(), stepped: true })
        }
        .await;
        // The secret files leave the checkout whatever happened.
        for target in linked {
            let _ = std::fs::remove_file(target);
        }
        outcome
    }

    // ---- the mirror's main branch, without a checkout -------------------------------------------

    /// The repository's mirror and the revision its main branch is at.
    async fn main_of(&self, ctx: &Ctx, repo: &str) -> Result<(String, String), RpcError> {
        self.ensure_loaded(ctx).await?;
        if !name_ok(repo) {
            return Err(RpcError::invalid_params(format!("`{repo}` is not a repository's id")));
        }
        let known = lock(&self.inner.repos).get(repo).cloned();
        let mirror = Self::mirror_path(ctx, repo);
        if !mirror.join("HEAD").is_file() {
            return Err(RpcError::not_found(self.text(ctx, "no_mirror", &[("repo", repo)])));
        }
        let mirror = mirror.to_string_lossy().into_owned();
        let host = Host::new(ctx.clone());
        let names: Vec<String> = match known {
            Some(known) if !known.main.is_empty() => vec![known.main],
            _ => vec!["main".into(), "master".into()],
        };
        for name in names {
            let rev = format!("refs/remotes/origin/{name}");
            if let Some(sha) = host.get::<vcs::RevResult>(vcs_methods::REV, json!({"root": mirror, "rev": rev})).await?.sha {
                return Ok((mirror, sha));
            }
        }
        Err(RpcError::not_found(self.text(ctx, "no_mirror", &[("repo", repo)])))
    }

    pub(crate) async fn read(&self, ctx: &Ctx, params: ReadParams) -> Result<Value, RpcError> {
        let (mirror, rev) = self.main_of(ctx, &params.repo).await?;
        let shown = Host::new(ctx.clone()).call(vcs_methods::SHOW, json!({"root": mirror, "rev": rev, "path": params.path})).await?;
        let text = shown.get("text").and_then(Value::as_str).unwrap_or("");
        let from = params.from.unwrap_or(1).max(1) as usize;
        let count = params.lines.unwrap_or(READ_LINES).clamp(1, READ_LINES_MAX) as usize;
        let lines: Vec<&str> = text.lines().skip(from - 1).take(count).collect();
        Ok(json!({"text": lines.join("\n")}))
    }

    pub(crate) async fn search(&self, ctx: &Ctx, params: SearchParams) -> Result<Value, RpcError> {
        let (mirror, rev) = self.main_of(ctx, &params.repo).await?;
        let found: vcs::GrepResult = Host::new(ctx.clone()).get(vcs_methods::GREP, json!({"root": mirror, "rev": rev, "text": params.text, "under": params.under, "limit": params.limit})).await?;
        Ok(json!({"hits": found.hits}))
    }

    /// `{repo, path?}`.
    pub(crate) async fn tree(&self, ctx: &Ctx, params: &Value) -> Result<Value, RpcError> {
        let repo = params.get("repo").and_then(Value::as_str).unwrap_or("");
        let path = params.get("path").and_then(Value::as_str).unwrap_or("");
        let (mirror, rev) = self.main_of(ctx, repo).await?;
        let found: vcs::TreeResult = Host::new(ctx.clone()).get(vcs_methods::TREE, json!({"root": mirror, "rev": rev, "path": path})).await?;
        Ok(json!({"entries": found.entries}))
    }

    // ---- probe -----------------------------------------------------------------------------------

    /// The plugin's work that needs no model, for real: a tiny repository made in the plugin's
    /// data folder is mirrored, given a worktree, compared, read, searched and removed, all
    /// through the git plugin as a job's are.
    pub(crate) async fn probe_work(&self, ctx: &Ctx) -> ProbeReport {
        let started = std::time::Instant::now();
        let mut checks = Vec::new();
        let now = self.text(ctx, "now_edit", &[("target", "src/lib.rs")]);
        checks.push(ProbeCheck { name: "texts".into(), ok: now.contains("src/lib.rs"), detail: now, ms: started.elapsed().as_millis() as u64 });
        let work = Self::work_dir(ctx);
        let made = std::fs::create_dir_all(&work);
        checks.push(ProbeCheck { name: "work folder".into(), ok: made.is_ok(), detail: made.err().map(|error| format!("{}: {error}", work.display())).unwrap_or_default(), ms: started.elapsed().as_millis() as u64 });
        let folder = ctx.env.data_dir.join(format!("probe-{}-{}", std::process::id(), cc_protocol::contracts::now_ms()));
        let outcome = tokio::time::timeout(Duration::from_secs(40), Self::probe_repository(ctx, &folder)).await;
        let _ = std::fs::remove_dir_all(&folder);
        let (ok, detail) = match outcome {
            Ok(Ok(detail)) => (true, detail),
            // The git plugin starts beside this one: when it does not serve yet, the repository
            // work could not be tried, which is not this plugin failing.
            Ok(Err(error)) if matches!(error.code.as_str(), codes::UNAVAILABLE | codes::DRAINING | codes::CONNECTION_CLOSED) => (true, format!("not tried, the git plugin does not serve yet: {}", error.message)),
            Ok(Err(error)) => (false, error.message),
            Err(_) => (false, "the repository work did not finish in 40 s".into()),
        };
        checks.push(ProbeCheck { name: "mirror, worktree, diff, read, search, remove".into(), ok, detail, ms: started.elapsed().as_millis() as u64 });
        ProbeReport::from_checks(checks)
    }

    async fn probe_repository(ctx: &Ctx, folder: &Path) -> Result<String, RpcError> {
        std::fs::create_dir_all(folder).map_err(|error| RpcError::internal(format!("cannot create {}: {error}", folder.display())))?;
        let never = CancellationToken::new();
        let git = Self::tool("git").await?;
        let source = folder.join("source.git");
        let source_text = source.to_string_lossy().into_owned();
        // A source with one commit on `main`, made without a working tree.
        let envs: Vec<(String, String)> = [
            ("GIT_DIR", source_text.as_str()),
            ("GIT_INDEX_FILE", &folder.join("index").to_string_lossy()),
            ("GIT_AUTHOR_NAME", "runner probe"),
            ("GIT_AUTHOR_EMAIL", "runner@localhost"),
            ("GIT_COMMITTER_NAME", "runner probe"),
            ("GIT_COMMITTER_EMAIL", "runner@localhost"),
        ]
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect();
        std::fs::write(folder.join("hello.txt"), "hello from the runner's probe\n").map_err(|error| RpcError::internal(error.to_string()))?;
        let git_step = async |args: &[&str]| -> Result<String, RpcError> {
            let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
            let ran = process::run(&git, &args, folder, &envs, Duration::from_secs(20), &never).await?;
            if !ran.ok() {
                return Err(RpcError::internal(format!("git {}: {}", args.join(" "), ran.text(400))));
            }
            Ok(ran.lines.last().cloned().unwrap_or_default().trim().to_owned())
        };
        git_step(&["init", "--quiet", "--bare", source_text.as_str()]).await?;
        let blob = git_step(&["hash-object", "-w", "hello.txt"]).await?;
        git_step(&["update-index", "--add", "--cacheinfo", &format!("100644,{blob},hello.txt")]).await?;
        let tree = git_step(&["write-tree"]).await?;
        let commit = git_step(&["commit-tree", tree.as_str(), "-m", "probe"]).await?;
        git_step(&["update-ref", "refs/heads/main", commit.as_str()]).await?;

        // No retries here: a git plugin that does not serve yet is reported, not waited for.
        let call = async |method: &str, params: Value| ctx.call(method, params).await;
        let mirror = folder.join("mirror.git").to_string_lossy().into_owned();
        let made: vcs::MirrorResult = parse_params(call(vcs_methods::MIRROR, json!({"url": source_text, "path": mirror})).await?)?;
        if !made.ok {
            return Err(RpcError::internal(format!("the mirror was not fetched: {}", made.detail)));
        }
        let head: vcs::RevResult = parse_params(call(vcs_methods::REV, json!({"root": made.path, "rev": "refs/remotes/origin/main"})).await?)?;
        if head.sha.as_deref() != Some(commit.as_str()) {
            return Err(RpcError::internal("the mirror's main branch is not the source's commit"));
        }
        let worktree = folder.join("job").to_string_lossy().into_owned();
        let added: vcs::WorktreeResult = parse_params(call(vcs_methods::WORKTREE_ADD, json!({"repo": made.path, "path": worktree, "branch": "cm/probe", "from": commit})).await?)?;
        std::fs::write(Path::new(&added.path).join("new.txt"), "one line\n").map_err(|error| RpcError::internal(error.to_string()))?;
        let diff: vcs::Diff = parse_params(call(vcs_methods::DIFF, json!({"root": added.path, "scope": format!("since:{commit}")})).await?)?;
        if diff.files.len() != 1 || !unified(&diff.files[0]).contains("+one line") {
            return Err(RpcError::internal("the worktree's diff does not show the one new file"));
        }
        let shown = call(vcs_methods::SHOW, json!({"root": made.path, "rev": commit, "path": "hello.txt"})).await?;
        if !shown.get("text").and_then(Value::as_str).unwrap_or("").contains("runner's probe") {
            return Err(RpcError::internal("the file read from the mirror is not the source's"));
        }
        let found: vcs::GrepResult = parse_params(call(vcs_methods::GREP, json!({"root": made.path, "rev": commit, "text": "runner's probe"})).await?)?;
        if found.hits.first().map(|hit| hit.path.as_str()) != Some("hello.txt") {
            return Err(RpcError::internal("the search of the mirror did not find the line"));
        }
        call(vcs_methods::WORKTREE_REMOVE, json!({"repo": made.path, "path": added.path, "force": true})).await?;
        if Path::new(&added.path).exists() {
            return Err(RpcError::internal("the worktree was not removed"));
        }
        Ok(String::new())
    }
}
