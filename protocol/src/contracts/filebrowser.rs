//! `filebrowser@1`: files beneath the serving user's home. Native dialogs and local disk
//! transfers are owned by the same plugin's shell module (`filebrowser-native@1`).
use serde::{Deserialize, Serialize};

pub mod methods {
    pub const LIST: &str = "filebrowser.list";
    pub const READ: &str = "filebrowser.read";
    pub const UPLOAD_START: &str = "filebrowser.upload.start";
    pub const UPLOAD_WRITE: &str = "filebrowser.upload.write";
    pub const UPLOAD_FINISH: &str = "filebrowser.upload.finish";
    pub const UPLOAD_CANCEL: &str = "filebrowser.upload.cancel";
    pub const ALL: &[&str] = &[LIST, READ, UPLOAD_START, UPLOAD_WRITE, UPLOAD_FINISH, UPLOAD_CANCEL];
}

pub const CHUNK_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListParams {
    /// Empty or `~` starts at HOME. Returned paths are absolute.
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub show_hidden: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub name: String,
    pub path: String,
    /// `directory`, `file`, `symlink` (unavailable target), or `other`.
    pub kind: String,
    pub symlink: bool,
    pub bytes: u64,
    /// Unix milliseconds.
    pub modified: u64,
    pub accessible: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Listing {
    pub home: String,
    pub path: String,
    pub parent: Option<String>,
    pub entries: Vec<Entry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadParams {
    pub path: String,
    #[serde(default)]
    pub offset: u64,
    /// Subsequent chunks must name the first chunk's stamp; changed files are refused.
    #[serde(default)]
    pub stamp: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chunk {
    pub data: String,
    pub offset: u64,
    pub bytes: u64,
    pub stamp: String,
    pub eof: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadStart {
    pub directory: String,
    /// A single file name, never a path. Existing files are never overwritten.
    pub name: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transfer {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadWrite {
    pub id: String,
    pub offset: u64,
    /// At most CHUNK_BYTES decoded bytes.
    pub data: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Saved {
    pub path: String,
    pub bytes: u64,
}

/// The plugin's local Electron shell module: native pickers and bounded disk transfers. These
/// methods are called on the local frame even while the Files UI is scoped to a mesh machine.
pub mod native {
    use serde::{Deserialize, Serialize};
    pub const START: &str = "filebrowser-native.start";
    pub const STATUS: &str = "filebrowser-native.status";
    pub const CANCEL: &str = "filebrowser-native.cancel";
    pub const PROGRESS: &str = "filebrowser-native.progress";

    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(rename_all = "lowercase")]
    pub enum Direction {
        Upload,
        Download,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Start {
        pub id: String,
        pub node: String,
        pub direction: Direction,
        #[serde(default)]
        pub directory: Option<String>,
        #[serde(default)]
        pub path: Option<String>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(rename_all = "lowercase")]
    pub enum State {
        Choosing,
        Running,
        Done,
        Cancelled,
        Error,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Status {
        pub id: String,
        pub node: String,
        pub direction: Direction,
        pub directory: String,
        /// Monotonic per transfer; ignore a status older than the last received event.
        pub revision: u64,
        pub state: State,
        pub name: String,
        pub completed: u32,
        pub files: u32,
        pub bytes: u64,
        pub total: u64,
        pub error: String,
        pub saved: String,
    }
}
