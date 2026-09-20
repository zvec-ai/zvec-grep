//! Canonical directories, files, and entity/fragment bundles backed by zvec.
//! Each configured embedding model owns one disjoint fragment search collection,
//! containing both the shared-config full-text index and its vector index.
//!
//! File IDs are cached from source records. The existing pending-file journal
//! owns recovery; no workspace-level identity database or allocation log exists.
mod backend;
mod codec;
mod directories;
mod file_ids;
mod path;
mod pending;
pub(crate) mod spi;
mod zvec;

pub(crate) use backend::ZvecStorageFactory;

// Old-format identity data is only recognized for drop and post-publication cleanup.
pub(crate) fn workspace_identities_exist(home: &std::path::Path) -> crate::EngineResult<bool> {
    for name in ["catalog", "catalog.staging", "identity.json"] {
        if home
            .join(name)
            .try_exists()
            .map_err(|error| crate::EngineError::from_io("inspect legacy identities", &error))?
        {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn delete_workspace_identities(home: &std::path::Path) -> crate::EngineResult<()> {
    if !home
        .try_exists()
        .map_err(|error| crate::EngineError::from_io("inspect legacy identity directory", &error))?
    {
        return Ok(());
    }
    for name in ["catalog", "catalog.staging"] {
        match std::fs::remove_dir_all(home.join(name)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(crate::EngineError::from_io(
                    "remove legacy identities",
                    &error,
                ));
            }
        }
    }
    match std::fs::remove_file(home.join("identity.json")) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(crate::EngineError::from_io(
                "remove legacy identities",
                &error,
            ));
        }
    }
    crate::utils::sync_directory(home)
}
