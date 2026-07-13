//! Core indexing and search engine for zvec-grep.

mod config;
mod embedding;
mod engine;
mod error;
mod extraction;
mod file;
mod indexing;
mod operation;
mod search;
mod storage;
mod workspace;

pub use config::EngineConfig;
pub use embedding::{DeterministicEmbedder, Embedder, EmbeddingSchema};
#[cfg(feature = "llama-cpp")]
pub use embedding::{LlamaEmbedder, LlamaEmbeddingConfig};
pub use engine::{Engine, EngineStatus};
pub use error::{EngineError, Result};
pub use extraction::Extractor;
pub use file::{
    CodeSymbol, CodeSymbolKind, FileFormat, FileId, FileLocation, FileSegment, IndexedFile,
    SegmentKind,
};
pub use indexing::{IndexFailure, IndexRequest, IndexResult};
pub use operation::{CancellationToken, OperationContext, ProgressEvent, ProgressSink};
pub use search::{
    MatchKind, SearchEvidence, SearchFailure, SearchFilter, SearchMode, SearchRequest,
    SearchResult, SearchResultItem,
};
pub use storage::{InMemoryStorage, RecallHit, Storage, StorageStats, ZvecStorage};
pub use workspace::{Workspace, WorkspaceConfig};
