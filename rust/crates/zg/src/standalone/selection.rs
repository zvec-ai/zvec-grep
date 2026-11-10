use super::Result;
use std::{fs, io, path::Path};

pub(super) fn read(root: &Path) -> Result<Option<String>> {
    let path = root.join("current");
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    #[cfg(unix)]
    let value = {
        if !metadata.file_type().is_symlink() {
            return Err(io::Error::other("current must be a version selection symlink").into());
        }
        fs::read_link(path)?
            .to_str()
            .ok_or_else(|| io::Error::other("invalid current selection"))?
            .to_owned()
    };
    #[cfg(windows)]
    let value = {
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(io::Error::other("current must be a version selection file").into());
        }
        fs::read_to_string(path)?.trim().to_owned()
    };
    super::versioned::validate_bundle_key(&value)?;
    Ok(Some(value))
}

/// Replace the selector in its own directory, keeping both package trees immutable.
pub(super) fn replace(root: &Path, expected: Option<&str>, selected: Option<&str>) -> Result<()> {
    let actual = read(root)?;
    if actual.as_deref() == selected {
        return Ok(());
    }
    if actual.as_deref() != expected {
        return Err(io::Error::other("current selection changed outside the installer").into());
    }
    let path = root.join("current");
    if let Some(selected) = selected {
        super::versioned::validate_bundle_key(selected)?;
        let temporary = super::versioned::temporary(root, "current");
        #[cfg(unix)]
        std::os::unix::fs::symlink(selected, &temporary)?;
        #[cfg(windows)]
        {
            use std::io::Write;
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(selected.as_bytes())?;
            file.sync_all()?;
        }
        let result = fs::rename(&temporary, path);
        let _ = fs::remove_file(temporary);
        result?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selecting_an_older_version_does_not_move_packages() {
        let temp = tempfile::tempdir().expect("selection fixture");
        let root = temp.path();
        let old = format!("0.2.1/{}-cpu", super::super::target());
        let new = format!("0.3.1/{}-cpu", super::super::target());
        for key in [&old, &new] {
            fs::create_dir_all(root.join(key)).expect("selection fixture");
        }
        replace(root, None, Some(&new)).expect("selection fixture");
        replace(root, Some(&new), Some(&old)).expect("selection fixture");
        assert_eq!(
            read(root).expect("selection fixture").as_deref(),
            Some(old.as_str())
        );
        assert!(root.join(old).is_dir());
        assert!(root.join(new).is_dir());
        assert_eq!(fs::read_dir(root).expect("selection fixture").count(), 3);
    }

    #[test]
    fn selection_entry_is_not_removed_between_version_switches() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let temp = tempfile::tempdir().expect("selection fixture");
        let root = temp.path();
        let old = format!("0.2.1/{}-cpu", super::super::target());
        let new = format!("0.3.1/{}-cpu", super::super::target());
        for (key, contents) in [(&old, "old resources"), (&new, "new resources")] {
            fs::create_dir_all(root.join(key)).expect("version directory");
            fs::write(root.join(key).join("marker"), contents).expect("version resources");
        }
        replace(root, None, Some(&old)).expect("initial selection");
        let running = Arc::new(AtomicBool::new(true));
        std::thread::scope(|scope| {
            let running_reader = Arc::clone(&running);
            let reader = scope.spawn(move || {
                while running_reader.load(Ordering::Relaxed) {
                    // Check rename's directory-entry guarantee without resolving the target.
                    // The stress assertion concerns entry presence, not every concurrent path lookup.
                    let metadata = fs::symlink_metadata(root.join("current"))
                        .expect("selection entry remains present");
                    assert!(if cfg!(unix) {
                        metadata.file_type().is_symlink()
                    } else {
                        metadata.is_file()
                    });
                }
            });
            let result = (|| -> Result<()> {
                for _ in 0..100 {
                    replace(root, Some(&old), Some(&new))?;
                    replace(root, Some(&new), Some(&old))?;
                }
                Ok(())
            })();
            running.store(false, Ordering::Relaxed);
            reader.join().expect("reader");
            result.expect("atomic switches");
        });
        assert_eq!(
            read(root).expect("final selection").as_deref(),
            Some(old.as_str())
        );
        assert_eq!(
            fs::read_to_string(root.join(old).join("marker")).expect("retained resources"),
            "old resources"
        );
    }

    #[test]
    fn unrelated_current_objects_are_never_replaced() {
        let temp = tempfile::tempdir().expect("selection fixture");
        fs::create_dir(temp.path().join("current")).expect("selection fixture");
        assert!(
            replace(
                temp.path(),
                None,
                Some(&format!("0.3.1/{}-cpu", super::super::target()))
            )
            .is_err()
        );
        assert!(temp.path().join("current").is_dir());
    }
}
