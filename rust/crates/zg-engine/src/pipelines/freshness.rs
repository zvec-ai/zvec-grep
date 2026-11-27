//! Shared source freshness checks for indexed and extracted searches.
use crate::{api::context::result::ContextItemStatus, domain::FileRecord, utils::sha256_hex};
use std::{fs, path::Path};

pub(super) fn file_freshness_status(workspace_root: &Path, file: &FileRecord) -> ContextItemStatus {
    let absolute_path = workspace_root.join(file.relative_path());
    let metadata = match fs::metadata(&absolute_path) {
        Ok(metadata) => metadata,
        Err(error) => {
            // Missing sources prove drift; inaccessible metadata alone does not.
            return if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) {
                ContextItemStatus::PossiblyStale
            } else {
                ContextItemStatus::Fresh
            };
        }
    };
    if !metadata.is_file() {
        return ContextItemStatus::PossiblyStale;
    }
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|duration| duration.as_millis().try_into().ok());
    if metadata.len() == file.snapshot().size_bytes
        && modified.is_some()
        && modified == file.snapshot().modified_epoch_ms
    {
        return ContextItemStatus::Fresh;
    }
    if let Some(expected) = &file.snapshot().content_hash
        && let Ok(bytes) = fs::read(&absolute_path)
    {
        return if sha256_hex(&bytes) == *expected {
            ContextItemStatus::Fresh
        } else {
            ContextItemStatus::PossiblyStale
        };
    }
    let metadata_changed = metadata.len() != file.snapshot().size_bytes
        || modified
            .zip(file.snapshot().modified_epoch_ms)
            .is_some_and(|(current, indexed)| current != indexed);
    if metadata_changed {
        ContextItemStatus::PossiblyStale
    } else {
        // Missing index, timestamp or hash information is not proof of an update.
        ContextItemStatus::Fresh
    }
}
