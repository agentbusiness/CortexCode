//! Wire envelope of the Cortex Plugin Protocol (CPP/1).
//!
//! Every frame on a connection is one JSON object with a `t` discriminator:
//! `req` (request), `res` (final response), `item` (stream item of an open request),
//! `evt` (published event) and `cancel` (cancel an open request).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

/// One protocol message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum Message {
    /// A request. `id` is unique per sender per connection.
    Req {
        id: u64,
        method: String,
        #[serde(default)]
        params: Value,
    },
    /// The final response to request `id`. Exactly one per request.
    Res {
        id: u64,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<RpcError>,
    },
    /// A stream item belonging to the still-open request `id`.
    Item { id: u64, data: Value },
    /// A published event. Topics are dot-separated; subscribers match by prefix.
    Evt { topic: String, data: Value },
    /// The sender of request `id` no longer wants its result.
    Cancel { id: u64 },
}

/// Error codes shared by every contract. Plugins may add codes, but must use these where they fit.
pub mod codes {
    pub const NOT_FOUND: &str = "not_found";
    pub const INVALID_PARAMS: &str = "invalid_params";
    pub const UNKNOWN_METHOD: &str = "unknown_method";
    /// No active plugin serves the requested category (or it is restarting).
    pub const UNAVAILABLE: &str = "unavailable";
    pub const CANCELLED: &str = "cancelled";
    pub const TIMEOUT: &str = "timeout";
    pub const INTERNAL: &str = "internal";
    pub const CONTRACT_VIOLATION: &str = "contract_violation";
    pub const BUSY: &str = "busy";
    /// Optimistic-concurrency failure, e.g. a stale hashline anchor.
    pub const CONFLICT: &str = "conflict";
    pub const DRAINING: &str = "draining";
    pub const CONNECTION_CLOSED: &str = "connection_closed";
}

/// A structured error carried in a `res` message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RpcError {
    pub code: String,
    pub message: String,
    /// Whether retrying the identical request may succeed.
    #[serde(default)]
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self { code: code.to_owned(), message: message.into(), retryable: false, data: None }
    }
    pub fn retryable(mut self) -> Self {
        self.retryable = true;
        self
    }
    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(codes::INVALID_PARAMS, message)
    }
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(codes::INTERNAL, message)
    }
    pub fn unknown_method(method: &str) -> Self {
        Self::new(codes::UNKNOWN_METHOD, format!("unknown method `{method}`"))
    }
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(codes::NOT_FOUND, message)
    }
    pub fn cancelled() -> Self {
        Self::new(codes::CANCELLED, "request cancelled")
    }
    pub fn closed() -> Self {
        Self::new(codes::CONNECTION_CLOSED, "connection closed").retryable()
    }
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for RpcError {}

impl From<serde_json::Error> for RpcError {
    fn from(error: serde_json::Error) -> Self {
        Self::invalid_params(error.to_string())
    }
}

/// Deserialize request params into a typed struct, mapping failures to `invalid_params`.
pub fn parse_params<T: serde::de::DeserializeOwned>(params: Value) -> Result<T, RpcError> {
    serde_json::from_value(params).map_err(|error| RpcError::invalid_params(error.to_string()))
}

/// Serialize a typed result, mapping failures to `internal`.
pub fn to_value<T: Serialize>(value: &T) -> Result<Value, RpcError> {
    serde_json::to_value(value).map_err(|error| RpcError::internal(error.to_string()))
}
