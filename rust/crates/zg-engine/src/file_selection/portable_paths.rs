use std::{collections::HashMap, fs, io, path::Path};

use crate::{
    EngineError, EngineResult,
    domain::{FileRecord, SourcePath},
};

#[allow(clippy::unnecessary_debug_formatting)] // Escape control characters in filenames.
pub(crate) fn verify_source_paths(root: &Path, files: &[FileRecord]) -> EngineResult<()> {
    let mut resolved = HashMap::new();
    for file in files {
        // Only report proven collisions; scanning handles inaccessible sources.
        let Ok(Some(metadata)) = source_metadata(root, file.relative_path()) else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let path = root.join(file.relative_path());
        #[cfg(target_os = "linux")]
        let previous = {
            use std::os::unix::fs::MetadataExt;

            let paths: &mut Vec<&SourcePath> = resolved
                .entry((metadata.dev(), metadata.ino()))
                .or_default();
            let previous = paths.iter().copied().find(|previous| {
                *previous != file.relative_path()
                    && same_directory_entry(&root.join(previous), &path).unwrap_or(false)
            });
            paths.push(file.relative_path());
            previous
        };
        #[cfg(not(target_os = "linux"))]
        let previous = {
            let Ok(canonical) = fs::canonicalize(&path) else {
                continue;
            };
            resolved.insert(canonical, file.relative_path())
        };
        if let Some(previous) = previous
            && previous != file.relative_path()
        {
            return Err(EngineError::invalid_argument(format!(
                "indexed paths {:?} and {:?} refer to the same file on this filesystem; restore distinct source filenames before using this index",
                previous.as_path(),
                file.relative_path().as_path(),
            )));
        }
    }
    Ok(())
}

fn source_metadata(root: &Path, relative: &SourcePath) -> io::Result<Option<fs::Metadata>> {
    let mut path = root.to_path_buf();
    let mut result = None;
    for component in relative.components() {
        path.push(component.as_os_str());
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.is_symlink() {
            return Ok(None);
        }
        result = Some(metadata);
    }
    Ok(result)
}

#[cfg(target_os = "linux")]
fn same_directory_entry(left: &Path, right: &Path) -> io::Result<bool> {
    use std::os::fd::AsRawFd;

    use rustix::fs::{Mode, OFlags, open};

    // Keep both entries alive: casefold aliases share an entry, hard links do not.
    let left = open(left, OFlags::PATH | OFlags::CLOEXEC, Mode::empty())?;
    let right = open(right, OFlags::PATH | OFlags::CLOEXEC, Mode::empty())?;
    Ok(
        fs::read_link(format!("/proc/self/fd/{}", left.as_raw_fd()))?
            == fs::read_link(format!("/proc/self/fd/{}", right.as_raw_fd()))?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{FileId, FileIndexStatus, FileSnapshot};

    fn file(id: u32, path: &str) -> FileRecord {
        FileRecord::new(
            FileId::new(id),
            SourcePath::new(path).expect("relative path"),
            FileSnapshot {
                size_bytes: 1,
                modified_epoch_ms: None,
                content_hash: None,
            },
            FileIndexStatus::NotIndexed,
        )
        .expect("file record")
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

    #[test]
    fn hard_links_remain_distinct_but_aliases_of_either_entry_collide() {
        let directory = tempfile::tempdir().expect("source directory");
        let root = directory.path();
        fs::write(root.join("First.txt"), "a").expect("source file");
        fs::hard_link(root.join("First.txt"), root.join("Second.txt")).expect("hard link");
        let mut files = vec![file(0, "First.txt"), file(1, "Second.txt")];
        verify_source_paths(root, &files).expect("distinct hard links");

        if root.join("second.txt").exists() {
            files.push(file(2, "second.txt"));
            let error = verify_source_paths(root, &files).expect_err("second entry alias");
            assert!(error.message().contains("Second.txt"));
            assert!(error.message().contains("second.txt"));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn directory_entry_comparison_distinguishes_hard_links_from_aliases() {
        let directory = tempfile::tempdir().expect("source directory");
        let first = directory.path().join("first");
        let second = directory.path().join("second");
        let alias = directory.path().join("alias");
        fs::write(&first, "a").expect("source file");
        fs::hard_link(&first, &second).expect("hard link");
        std::os::unix::fs::symlink(&second, &alias).expect("alias of second entry");

        assert!(!same_directory_entry(&first, &second).expect("hard links"));
        assert!(!same_directory_entry(&first, &alias).expect("different entry"));
        assert!(same_directory_entry(&second, &alias).expect("same entry"));
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
