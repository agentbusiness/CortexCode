//! The plugin's state and its method dispatch.

use crate::config::Config;
use crate::deep::{Jobs, Publish, Tools};
use crate::fetch::Fetcher;
use crate::net::Net;
use crate::notes::Notes;
use crate::peers::Peers;
use crate::sources::{Engine, EnvFn};
use cc_protocol::contracts::model::{ChatParams, ChatResult};
use cc_protocol::contracts::research::{
    DeepParams, FetchParams, FetchResult, JobParams, JobsParams, JobsResult, NoteAddParams, NoteSearchParams, NotesParams, NotesResult, SearchParams,
    SearchResult, SourcesResult, methods, topics,
};
use cc_protocol::{BoxFuture, RpcError, parse_params, to_value};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub const PROVIDES: &[&str] = &[
    methods::SEARCH,
    methods::FETCH,
    methods::NOTE_ADD,
    methods::NOTE_SEARCH,
    methods::NOTES,
    methods::DEEP,
    methods::JOB_STATUS,
    methods::JOB_CANCEL,
    methods::JOBS,
    methods::SOURCES,
];

pub const PUBLISHES: &[&str] = &[topics::PROGRESS, topics::JOB];

/// Methods of other plugins this one calls.
pub const USES: &[&str] = &["model.chat", "frame.project.get", "browser.open", "browser.eval", "browser.close", "memory.note.add", "memory.note.search", "memory.note.list", "memory.knowledge.upsert"];

pub struct Core {
    pub engine: Arc<Engine>,
    pub fetcher: Arc<Fetcher>,
    pub notes: Arc<Notes>,
    pub jobs: Arc<Jobs>,
    pub config: Config,
}

/// The deep job's tools: this plugin's own search and fetch, the model and the frame through peers.
struct PluginTools {
    engine: Arc<Engine>,
    fetcher: Arc<Fetcher>,
    peers: Arc<dyn Peers>,
}

impl Tools for PluginTools {
    fn search(&self, params: SearchParams, cancel: CancellationToken) -> BoxFuture<'static, Result<SearchResult, RpcError>> {
        let engine = self.engine.clone();
        Box::pin(async move { engine.search(params, cancel).await })
    }

    fn fetch(&self, params: FetchParams) -> BoxFuture<'static, Result<FetchResult, RpcError>> {
        let fetcher = self.fetcher.clone();
        Box::pin(async move { fetcher.fetch(params).await })
    }

    fn chat(&self, params: ChatParams, cancel: CancellationToken) -> BoxFuture<'static, Result<ChatResult, RpcError>> {
        let peers = self.peers.clone();
        Box::pin(async move {
            let value = peers.call("model.chat", to_value(&params)?, cancel).await?;
            parse_params::<ChatResult>(value)
        })
    }

    fn project_root(&self, project_id: &str) -> BoxFuture<'static, Result<String, RpcError>> {
        let peers = self.peers.clone();
        let id = project_id.to_owned();
        Box::pin(async move {
            let project = peers.call("frame.project.get", json!({ "id": id }), CancellationToken::new()).await?;
            project
                .get("root")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| RpcError::internal(format!("frame.project.get gave no root for {id}")))
        })
    }
}

impl Core {
    /// `home`: CORTEX_HOME (the fetch cache lives in `research/cache`); `data`: the plugin's data
    /// directory (jobs, and the notes of an earlier version, imported into memory once).
    pub fn new(config: Config, home: PathBuf, data: PathBuf, peers: Arc<dyn Peers>, env: EnvFn) -> Arc<Self> {
        let net = Arc::new(Net::new(config.clone()));
        let engine = Arc::new(Engine::new(net.clone(), peers.clone(), env));
        let fetcher = Arc::new(Fetcher::new(net, home.join("research").join("cache")));
        let notes = Arc::new(Notes::new(peers.clone(), data.join("notes")));
        let tools: Arc<dyn Tools> = Arc::new(PluginTools { engine: engine.clone(), fetcher: fetcher.clone(), peers: peers.clone() });
        let publisher = peers.clone();
        let publish: Publish = Arc::new(move |topic: &str, data: Value| publisher.publish(topic, data));
        let jobs = Arc::new(Jobs::new(data.join("jobs"), tools, notes.clone(), config.clone(), publish));
        jobs.mark_interrupted();
        Arc::new(Self { engine, fetcher, notes, jobs, config })
    }

    pub async fn dispatch(&self, method: &str, params: Value, cancel: CancellationToken) -> Result<Value, RpcError> {
        match method {
            methods::SEARCH => to_value(&self.engine.search(parse_params(params)?, cancel).await?),
            methods::FETCH => to_value(&self.fetcher.fetch(parse_params(params)?).await?),
            methods::NOTE_ADD => to_value(&self.notes.add(parse_params::<NoteAddParams>(params)?).await?),
            methods::NOTE_SEARCH => {
                let p: NoteSearchParams = parse_params(params)?;
                let found = self.notes.search(&p.project_id, p.topic.as_deref(), &p.text, p.limit.unwrap_or(20).clamp(1, 200)).await?;
                to_value(&NotesResult { notes: found.notes, pending: found.pending })
            }
            methods::NOTES => {
                let p: NotesParams = parse_params(params)?;
                to_value(&NotesResult { notes: self.notes.list(&p.project_id, p.topic.as_deref()).await?, pending: 0 })
            }
            methods::DEEP => to_value(&self.jobs.start(parse_params::<DeepParams>(params)?)?),
            methods::JOB_STATUS => to_value(&self.jobs.status(&parse_params::<JobParams>(params)?.job_id)?),
            methods::JOB_CANCEL => to_value(&self.jobs.cancel(&parse_params::<JobParams>(params)?.job_id)?),
            methods::JOBS => {
                let p: JobsParams = parse_params(params)?;
                to_value(&JobsResult { jobs: self.jobs.list(p.project_id.as_deref()) })
            }
            methods::SOURCES => to_value(&SourcesResult { sources: self.engine.describe().await }),
            other => Err(RpcError::unknown_method(other)),
        }
    }
}
