//! The plugin's configuration: `[backend.config]` in plugin.toml, overridden by the frame config's
//! `[plugin.config]`, delivered in the hello acknowledgement. Unknown keys are ignored; a key with
//! the wrong type keeps its default.

use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// Sent with every request (Reddit and arXiv ask for a descriptive one).
    pub user_agent: String,
    /// Per HTTP request.
    pub timeout_ms: u64,
    /// Most bytes downloaded per fetch (the rest is cut, `truncated`).
    pub max_download_bytes: u64,
    /// Most characters of extracted text kept per document.
    pub max_text_chars: u64,
    /// A cached fetch is fresh this long.
    pub cache_ttl_hours: u64,
    /// Let `research.fetch` reach loopback, private and link-local addresses (off: a model-chosen URL
    /// cannot read local services).
    pub allow_private_hosts: bool,
    /// `auto` (the DuckDuckGo HTML page over HTTP, then the built-in browser when blocked) | `http` |
    /// `browser`.
    pub web_search: String,
    /// `auto` (`gh` when installed and signed in, else REST) | `gh` | `rest`.
    pub github: String,
    /// The GitHub CLI.
    pub gh: String,
    pub arxiv_api: String,
    pub semantic_scholar_api: String,
    pub github_api: String,
    pub reddit: String,
    pub hackernews_api: String,
    pub duckduckgo_html: String,
    /// arXiv's API terms ask for one request every 3 seconds.
    pub arxiv_interval_ms: u64,
    /// Rate-limited (429/503) requests are retried when the server asks to wait at most this long.
    pub max_retry_wait_ms: u64,
    pub deep_depth: u32,
    pub deep_sources_per_round: u32,
    pub deep_results_per_query: u32,
    /// Characters of a source's text per model call (map step); a 32k-token context holds the
    /// prompt, one chunk and the answer.
    pub chunk_chars: u64,
    pub max_chunks_per_source: u32,
    /// Characters of notes per synthesis call (reduce step); more are condensed in batches first.
    pub synth_chars: u64,
    pub model_max_tokens: u32,
    /// `foreground` | `agent` | `background`.
    pub model_priority: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            user_agent: "CortexCode-research/0.1 (local research assistant; macOS)".into(),
            timeout_ms: 20_000,
            max_download_bytes: 25 * 1024 * 1024,
            max_text_chars: 400_000,
            cache_ttl_hours: 168,
            allow_private_hosts: false,
            web_search: "auto".into(),
            github: "auto".into(),
            gh: "gh".into(),
            arxiv_api: "https://export.arxiv.org/api/query".into(),
            semantic_scholar_api: "https://api.semanticscholar.org/graph/v1".into(),
            github_api: "https://api.github.com".into(),
            reddit: "https://www.reddit.com".into(),
            hackernews_api: "https://hn.algolia.com/api/v1".into(),
            duckduckgo_html: "https://html.duckduckgo.com/html/".into(),
            arxiv_interval_ms: 3_000,
            max_retry_wait_ms: 5_000,
            deep_depth: 2,
            deep_sources_per_round: 8,
            deep_results_per_query: 4,
            chunk_chars: 12_000,
            max_chunks_per_source: 6,
            synth_chars: 48_000,
            model_max_tokens: 2_048,
            model_priority: "agent".into(),
        }
    }
}

impl Config {
    pub fn from_value(value: &Value) -> Self {
        let mut config = Self::default();
        let Some(map) = value.as_object() else { return config };
        let text = |key: &str, slot: &mut String| {
            if let Some(found) = map.get(key).and_then(Value::as_str) {
                *slot = found.to_owned();
            }
        };
        let number = |key: &str, slot: &mut u64| {
            if let Some(found) = map.get(key).and_then(Value::as_u64) {
                *slot = found;
            }
        };
        let small = |key: &str, slot: &mut u32| {
            if let Some(found) = map.get(key).and_then(Value::as_u64) {
                *slot = u32::try_from(found).unwrap_or(u32::MAX);
            }
        };
        text("user_agent", &mut config.user_agent);
        number("timeout_ms", &mut config.timeout_ms);
        number("max_download_bytes", &mut config.max_download_bytes);
        number("max_text_chars", &mut config.max_text_chars);
        number("cache_ttl_hours", &mut config.cache_ttl_hours);
        if let Some(found) = map.get("allow_private_hosts").and_then(Value::as_bool) {
            config.allow_private_hosts = found;
        }
        text("web_search", &mut config.web_search);
        text("github", &mut config.github);
        text("gh", &mut config.gh);
        text("arxiv_api", &mut config.arxiv_api);
        text("semantic_scholar_api", &mut config.semantic_scholar_api);
        text("github_api", &mut config.github_api);
        text("reddit", &mut config.reddit);
        text("hackernews_api", &mut config.hackernews_api);
        text("duckduckgo_html", &mut config.duckduckgo_html);
        number("arxiv_interval_ms", &mut config.arxiv_interval_ms);
        number("max_retry_wait_ms", &mut config.max_retry_wait_ms);
        small("deep_depth", &mut config.deep_depth);
        small("deep_sources_per_round", &mut config.deep_sources_per_round);
        small("deep_results_per_query", &mut config.deep_results_per_query);
        number("chunk_chars", &mut config.chunk_chars);
        small("max_chunks_per_source", &mut config.max_chunks_per_source);
        number("synth_chars", &mut config.synth_chars);
        small("model_max_tokens", &mut config.model_max_tokens);
        text("model_priority", &mut config.model_priority);
        config.chunk_chars = config.chunk_chars.max(1_000);
        config.synth_chars = config.synth_chars.max(4_000);
        config.max_chunks_per_source = config.max_chunks_per_source.max(1);
        config.deep_sources_per_round = config.deep_sources_per_round.max(1);
        config.deep_results_per_query = config.deep_results_per_query.clamp(1, 25);
        config
    }
}
