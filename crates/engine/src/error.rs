use std::path::PathBuf;

/// Errors returned by the engine.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("operation cancelled")]
    Cancelled,

    #[error("workspace index is busy in another process")]
    Busy,

    #[error("invalid configuration: {0}")]
    InvalidConfig(String),

    #[error("I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("extraction failed for {path}: {message}")]
    Extraction { path: PathBuf, message: String },

    #[error("embedding failed: {0}")]
    Embedding(String),

    #[error("storage failed: {0}")]
    Storage(String),
}

pub type Result<T> = std::result::Result<T, EngineError>;
