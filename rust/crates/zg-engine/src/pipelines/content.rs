//! Read stored content without refreshing the index or opening source files.

use std::path::Path;

use crate::{
    EngineError, EngineResult,
    api::content::{ContentResult, ReadContentOptions},
    domain::EntityId,
    pipelines::indexing::service::{WorkspaceIndexService, is_indexed},
    storage::{
        IndexStore,
        read_session::{ReadSessionCache, ReadSessionLease},
    },
    workspace::{
        layout::find_nearest_workspace,
        lock::{LockWait, try_home_read},
        manifest::{WorkspaceManifest, read_workspace_manifest},
    },
};

pub(crate) async fn read_content(
    indexing: &WorkspaceIndexService,
    options: &ReadContentOptions,
    read_sessions: Option<&ReadSessionCache>,
) -> EngineResult<ContentResult> {
    let wait = LockWait::new(options.signal.as_ref(), options.lock_timeout_ms)?;
    wait.check_cancelled()?;
    if options.reference.generation.trim().is_empty()
        || options.reference.entity_id.trim().is_empty()
    {
        return Err(EngineError::invalid_argument(
            "content generation and entity ID must be non-empty",
        ));
    }
    let root = std::path::absolute(options.root.as_deref().unwrap_or_else(|| Path::new(".")))
        .map_err(|error| EngineError::from_io("resolve content workspace", &error))?;
    let location = find_nearest_workspace(&root)?
        .ok_or_else(|| EngineError::not_found("no indexed workspace was found"))?;
    let _lock = loop {
        wait.check_cancelled()?;
        if let Some(active) = read_workspace_manifest(&location.home)?
            && let Some(writer) = indexing
                .writers
                .borrow(&location.home, &active.storage_home())
        {
            let result =
                read_stored_content(&writer.session.manifest, &writer.session.storage, options)?;
            wait.check_cancelled()?;
            return Ok(result);
        }
        if let Some(lock) = try_home_read(&location.home, "read_content")? {
            break lock;
        }
        wait.retry(&location.home, "read_content").await?;
    };
    let manifest = read_workspace_manifest(&location.home)?
        .ok_or_else(|| EngineError::not_found("workspace manifest disappeared"))?;
    validate_generation(&manifest, options)?;
    let storage = match read_sessions {
        Some(cache) => cache.acquire(&location.home, &manifest.storage_home())?,
        None => ReadSessionLease::open(&manifest.storage_home())?,
    };
    let result = read_stored_content(&manifest, storage.storage(), options);
    let close = storage.close();
    let result = result?;
    close?;
    wait.check_cancelled()?;
    Ok(result)
}

fn validate_generation(
    manifest: &WorkspaceManifest,
    options: &ReadContentOptions,
) -> EngineResult<()> {
    if !is_indexed(manifest) {
        return Err(EngineError::not_found("workspace index is unavailable"));
    }
    if manifest.storage_generation.as_deref() != Some(options.reference.generation.as_str()) {
        return Err(EngineError::not_found(
            "indexed content belongs to an inactive storage generation; query the current index again",
        ));
    }
    Ok(())
}

fn read_stored_content(
    manifest: &WorkspaceManifest,
    storage: &IndexStore,
    options: &ReadContentOptions,
) -> EngineResult<ContentResult> {
    validate_generation(manifest, options)?;
    let descriptor = manifest
        .workspace
        .index
        .descriptor()
        .ok_or_else(|| EngineError::not_found("workspace index is unavailable"))?;
    storage.ensure_compatible(&descriptor.tables()?)?;
    let id = EntityId::from_string(options.reference.entity_id.clone());
    let stored = storage.read_entity(&id)?.ok_or_else(|| {
        EngineError::not_found("indexed content no longer exists; query the current index again")
    })?;
    Ok(ContentResult {
        path: manifest
            .workspace
            .root
            .join(stored.file.relative_path.as_path()),
        content: stored.entity.content,
    })
}

#[cfg(test)]
mod tests;
