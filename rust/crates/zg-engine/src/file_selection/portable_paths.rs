use std::{collections::HashMap, fs, path::Path};

use crate::{
    EngineError, EngineResult,
    domain::{FileRecord, SourcePath},
};

#[allow(clippy::unnecessary_debug_formatting)] // Escape control characters in filenames.
pub(crate) fn verify_source_paths(root: &Path, files: &[FileRecord]) -> EngineResult<()> {
    let mut resolved = HashMap::new();
    for file in files {
        if has_symlink(root, &file.relative_path)? {
            continue;
        }
        let path = root.join(&file.relative_path);
        let canonical = match fs::canonicalize(&path) {
            Ok(path) => path,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                continue;
            }
            Err(error) => {
                return Err(EngineError::from_io(
                    "cannot resolve indexed source path",
                    &error,
                ));
            }
        };
        if let Some(previous) = resolved.insert(canonical, &file.relative_path)
            && previous != &file.relative_path
        {
            return Err(EngineError::invalid_argument(format!(
                "indexed paths {:?} and {:?} refer to the same file on this filesystem; restore distinct source filenames before using this index",
                previous.as_path(),
                file.relative_path.as_path(),
            )));
        }
    }
    Ok(())
}

fn has_symlink(root: &Path, relative: &SourcePath) -> EngineResult<bool> {
    let mut path = root.to_path_buf();
    for component in relative.components() {
        path.push(component.as_os_str());
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_symlink() => return Ok(true),
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                return Ok(false);
            }
            Err(error) => {
                return Err(EngineError::from_io(
                    "cannot inspect indexed source path",
                    &error,
                ));
            }
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{FileId, FileIndexStatus, FileSnapshot};

    fn file(id: u32, path: &str) -> FileRecord {
        FileRecord {
            id: FileId::new(id),
            relative_path: SourcePath::new(path).expect("relative path"),
            snapshot: FileSnapshot {
                size_bytes: 1,
                modified_epoch_ms: None,
                content_hash: None,
            },
            index_status: FileIndexStatus::NotIndexed,
        }
    }

    #[test]
    fn destination_filesystem_decides_whether_case_distinct_names_collide() {
        let directory = tempfile::tempdir().expect("source directory");
        let root = directory.path();
        fs::write(root.join("Room.txt"), "a").expect("source file");
        let files = [
            file(0, "Room.txt"),
            file(1, "room.txt"),
            file(2, "missing.txt"),
        ];
        if root.join("room.txt").exists() {
            let error = verify_source_paths(root, &files).expect_err("case-insensitive collision");
            assert!(error.message().contains("Room.txt"));
            assert!(error.message().contains("room.txt"));
        } else {
            fs::write(root.join("room.txt"), "b").expect("distinct source file");
            verify_source_paths(root, &files).expect("case-sensitive filesystem");
        }
    }

    #[test]
    fn replaced_source_directories_allow_stale_child_records_to_be_removed() {
        let directory = tempfile::tempdir().expect("source directory");
        let root = directory.path();
        fs::write(root.join("rooms"), "replacement file").expect("replaced source directory");
        verify_source_paths(root, &[file(0, "rooms/old.txt"), file(1, "missing.txt")])
            .expect("missing old paths do not prevent the next incremental scan");
    }

    #[cfg(unix)]
    #[test]
    fn existing_file_and_directory_symlink_aliases_are_not_migration_collisions() {
        let directory = tempfile::tempdir().expect("source directory");
        let root = directory.path();
        fs::create_dir(root.join("rooms")).expect("source directory");
        fs::write(root.join("rooms/note.txt"), "a").expect("source file");
        std::os::unix::fs::symlink(root.join("rooms"), root.join("alias"))
            .expect("directory alias");
        std::os::unix::fs::symlink(root.join("rooms/note.txt"), root.join("note-link.txt"))
            .expect("file alias");
        verify_source_paths(
            root,
            &[
                file(0, "rooms/note.txt"),
                file(1, "alias/note.txt"),
                file(2, "note-link.txt"),
            ],
        )
        .expect("intentional aliases");
    }
}
