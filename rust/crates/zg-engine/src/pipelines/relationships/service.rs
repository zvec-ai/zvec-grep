//! Open the indexed workspace and read a consistent relationship snapshot off the async workers.

use crate::{
    EngineError, EngineResult,
    api::relationships::{Direction, RelationshipOptions, RelationshipResult},
    domain::IndexDescriptor,
    pipelines::{
        indexing::service::WorkspaceIndexService,
        read_access::{ReadSource, acquire_read_source},
    },
    storage::{
        IndexStore,
        read_session::{ReadSessionCache, ReadSessionLease},
    },
    utils::{directory_exists, resolve_directory},
    workspace::{find_nearest_workspace, lock::LockWait, manifest::read_workspace_manifest},
};

use super::pipeline::{normalize, query_symbols};

pub(crate) async fn query(
    indexing: &WorkspaceIndexService,
    options: &RelationshipOptions,
    read_sessions: Option<&ReadSessionCache>,
    direction: Direction,
) -> EngineResult<RelationshipResult> {
    let (name, scope) = normalize(options)?;
    let wait = LockWait::new(options.signal.as_ref(), options.lock_timeout_ms)?;
    wait.check_cancelled()?;
    let requested = resolve_directory(options.root.as_deref())?;
    let (source_root, layout) = find_nearest_workspace(&requested)?.ok_or_else(missing_index)?;
    let workspace = read_workspace_manifest(&source_root, &layout)?.ok_or_else(missing_index)?;
    if !workspace.index_enabled() {
        return Err(missing_index());
    }
    // Reject disabled workspaces before waiting for a reader or borrowing a writer.
    // Storage still uses the current configuration after admission, since a rebuild may intervene.
    if !workspace
        .index()
        .descriptor()
        .is_some_and(IndexDescriptor::graph_indexing)
    {
        return Err(EngineError::unsupported("graph support is disabled"));
    }
    let source = acquire_read_source(
        &source_root,
        &layout,
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
        let workspace =
            read_workspace_manifest(&source_root, &layout)?.ok_or_else(missing_index)?;
        if !workspace.index_enabled() || !directory_exists(workspace.layout().index_dir())? {
            return Err(missing_index());
        }
        let graph_enabled = workspace
            .index()
            .descriptor()
            .is_some_and(IndexDescriptor::graph_indexing);
        let storage = match read_sessions {
            Some(cache) => cache.acquire(
                &layout,
                workspace.layout().index_dir(),
                workspace
                    .storage_generation()
                    .expect("published index generation"),
                graph_enabled,
            )?,
            None => ReadSessionLease::open(workspace.layout().index_dir(), graph_enabled)?,
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
