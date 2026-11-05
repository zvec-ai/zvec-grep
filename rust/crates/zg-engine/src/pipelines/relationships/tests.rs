//! Relationship query regressions, grouped by behavior.
use std::{collections::BTreeMap, fs, path::PathBuf, sync::Arc, time::Duration};

use tokio_util::sync::CancellationToken;

use crate::{
    EngineError, EngineRuntimeSnapshot, ZvecGrep,
    api::relationships::{RelationshipDirection, RelationshipOptions, RelationshipResult},
    domain::{
        CodeMetadata, Content, ContentKind, Entity, EntityFragment, EntityId, EntityMetadata,
        FTS_CONFIG, FileIndexStatus, FileRecord, FileSnapshot, FragmentId, IndexDescriptor,
        IndexState, Range, ScanRules, SourcePath, SymbolType, TextRange, Workspace,
        WorkspaceLayout,
        model::{EmbeddingMetric, EmbeddingModelInfo, ModelInfo},
    },
    pipelines::{indexed_search::writer::WriterSession, indexing::service::WorkspaceIndexService},
    storage::{
        IndexStore,
        types::{IndexedFragment, WorkspaceIndexStorageOptions},
    },
    workspace::{
        CURRENT_INDEX_VERSION,
        lock::{LockMode, acquire_workspace_lock},
        manifest::{read_workspace_manifest, write_workspace_manifest},
    },
};

use super::service;
use fixture::*;

mod admission;
mod fixture;
mod lookup;
mod snapshots;
mod suggestions;
mod truncation;
