//! Open the indexed workspace and read a consistent relationship snapshot off the async workers.

use std::path::Path;

use crate::{
    EngineError, EngineResult,
    api::relationships::{RelationshipDirection, RelationshipOptions, RelationshipResult},
    pipelines::indexing::service::{WorkspaceIndexService, is_indexed},
    storage::{
        IndexStore,
        graph::Direction,
        read_session::{ReadSessionCache, ReadSessionLease},
    },
    workspace::{
        layout::find_nearest_workspace, lock::LockWait, manifest::read_workspace_manifest,
    },
};

use super::super::read_access::{ReadSource, acquire_read_source};

use super::pipeline::{normalize, query_symbols};

pub(crate) async fn query(
    indexing: &WorkspaceIndexService,
    options: &RelationshipOptions,
    read_sessions: Option<&ReadSessionCache>,
    direction: RelationshipDirection,
) -> EngineResult<RelationshipResult> {
    let (name, scope) = normalize(options)?;
    let wait = LockWait::new(options.signal.as_ref(), options.lock_timeout_ms)?;
    wait.check_cancelled()?;
    let requested = options.root.as_deref().unwrap_or_else(|| Path::new("."));
    let location = find_nearest_workspace(requested)?.ok_or_else(missing_index)?;
    let direction = match direction {
        RelationshipDirection::Callers => Direction::In,
        RelationshipDirection::Callees => Direction::Out,
    };
    let source = acquire_read_source(
        &location.home,
        &wait,
        "relationships",
        Some(&indexing.writers),
        |_| Ok(Some(())),
    )
    .await?;
    let request = options.clone();
    let read_sessions = read_sessions.cloned();
    // The blocking task owns admission and storage leases. Aborting the async
    // caller must not release them while native reads are still running.
    let result = tokio::task::spawn_blocking(move || {
        let run = |storage: &IndexStore| {
            query_symbols(storage, &name, scope.as_deref(), &request, direction)
        };
        let _lock = match source {
            ReadSource::Writer { lease, .. } => return run(&lease.session.storage),
            ReadSource::Read(lock) => lock,
        };
        let manifest = read_workspace_manifest(&location.home)?.ok_or_else(missing_index)?;
        if !is_indexed(&manifest) || !IndexStore::exists(&manifest.storage_home())? {
            return Err(missing_index());
        }
        let storage = match read_sessions {
            Some(cache) => cache.acquire(&location.home, &manifest.storage_home())?,
            None => ReadSessionLease::open(&manifest.storage_home())?,
        };
        let result = run(storage.storage());
        let close = storage.close();
        let result = result?;
        close?;
        Ok(result)
    })
    .await
    .map_err(|error| {
        EngineError::internal(format!("relationship query worker failed: {error}"))
    })??;
    wait.check_cancelled()?;
    Ok(result)
}

fn missing_index() -> EngineError {
    EngineError::not_found("workspace index is unavailable; run `zg --index` first")
}
