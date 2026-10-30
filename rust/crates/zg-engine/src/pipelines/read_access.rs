//! Shared query admission: lease a compatible writer or acquire a workspace read lock.

use std::path::Path;

use crate::{
    EngineResult,
    domain::WorkspaceLayout,
    workspace::{
        lock::{LockGuard, LockWait, try_acquire_workspace_read},
        manifest::read_workspace_manifest,
    },
};

use super::indexed_search::writer::{WriterContexts, WriterLease, WriterSession};

pub(super) enum ReadSource<T> {
    Writer { lease: WriterLease, context: T },
    Read(LockGuard),
}

pub(super) async fn acquire_read_source<T>(
    source_root: &Path,
    layout: &WorkspaceLayout,
    wait: &LockWait<'_>,
    operation: &str,
    writers: Option<&WriterContexts>,
    mut prepare: impl FnMut(&WriterSession) -> EngineResult<Option<T>>,
) -> EngineResult<ReadSource<T>> {
    loop {
        wait.check_cancelled()?;
        if let Some(writers) = writers
            && let Some(lease) = writers.borrow(layout.storage_dir())
            && let Some(active) = read_workspace_manifest(source_root, layout)?
            && active.index_enabled()
            && active.storage_generation() == lease.session.workspace.storage_generation()
            && let Some(context) = prepare(&lease.session)?
        {
            return Ok(ReadSource::Writer { lease, context });
        }
        if let Some(lock) = try_acquire_workspace_read(source_root, layout, operation)? {
            return Ok(ReadSource::Read(lock));
        }
        // A writer can become available after indexing has opened its storage.
        wait.wait_for_retry(layout.storage_dir(), operation).await?;
    }
}
