use super::{Receipt, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    path::{Path, PathBuf},
};

#[cfg(any(windows, test))]
fn publish_launcher(source: &mut impl io::Read, path: &Path, digest: &str) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("invalid command path"))?;
    let mut output = tempfile::NamedTempFile::new_in(parent)?;
    io::copy(source, output.as_file_mut())?;
    if super::versioned::file_hash(output.path())? != digest {
        return Err(io::Error::other("staged launcher checksum mismatch").into());
    }
    output.as_file().sync_all()?;
    // Publish only complete bytes, without overwriting a concurrently created command.
    output.persist_noclobber(path)?;
    Ok(())
}

/// A stable command entry is created once; version selection belongs to `current`.
#[derive(Deserialize, Serialize)]
pub(super) struct Entry {
    pub path: PathBuf,
    target: PathBuf,
    shim_sha256: Option<String>,
    existed: bool,
}

impl Entry {
    pub fn prepare(
        root: &Path,
        path: &Path,
        before: Option<&Receipt>,
        selected: Option<&Path>,
    ) -> Result<Self> {
        let path = std::path::absolute(path)?;
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("invalid command path"))?;
        fs::create_dir_all(parent)?;
        let path = parent.canonicalize()?.join(
            path.file_name()
                .ok_or_else(|| io::Error::other("missing command filename"))?,
        );
        if cfg!(unix) && path.starts_with(root) {
            return Err(io::Error::other(
                "the command symlink must be outside the version directory",
            )
            .into());
        }
        if cfg!(windows) && path != super::binary(root) {
            return Err(io::Error::other(
                "the native launcher must be installed in the installation's bin directory",
            )
            .into());
        }
        let shim_sha256 = if cfg!(windows) {
            match before.and_then(|receipt| receipt.launcher_sha256.clone()) {
                Some(digest) => Some(digest),
                None => selected
                    .map(|bundle| super::versioned::file_hash(&bundle.join("bin/zg-launcher.exe")))
                    .transpose()?,
            }
        } else {
            None
        };
        let existed = fs::symlink_metadata(&path).is_ok();
        let entry = Self {
            path,
            target: super::binary(&root.join("current")),
            shim_sha256,
            existed,
        };
        if selected.is_some() && existed && (before.is_none() || !entry.owned()?) {
            return Err(entry.conflict());
        }
        Ok(entry)
    }

    pub fn digest(&self) -> Option<String> {
        self.shim_sha256.clone()
    }

    fn conflict(&self) -> Box<dyn std::error::Error> {
        io::Error::other(format!(
            "command entry belongs to another installation: {}; move it explicitly before retrying",
            self.path.display()
        ))
        .into()
    }

    pub(super) fn owned(&self) -> Result<bool> {
        let metadata = match fs::symlink_metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        if cfg!(windows) {
            return Ok(metadata.is_file()
                && !metadata.file_type().is_symlink()
                && self.shim_sha256.as_deref()
                    == Some(super::versioned::file_hash(&self.path)?.as_str()));
        }
        Ok(metadata.file_type().is_symlink() && fs::read_link(&self.path)? == self.target)
    }

    pub fn activate(&self, bundle: &Path) -> Result<()> {
        if fs::symlink_metadata(&self.path).is_ok() {
            return if self.owned()? {
                Ok(())
            } else {
                Err(self.conflict())
            };
        }
        #[cfg(windows)]
        {
            let source = bundle.join("bin/zg-launcher.exe");
            publish_launcher(
                &mut fs::File::open(source)?,
                &self.path,
                self.shim_sha256
                    .as_deref()
                    .ok_or_else(|| io::Error::other("missing launcher digest"))?,
            )?;
        }
        #[cfg(unix)]
        {
            let _ = bundle;
            // Creation must not overwrite a command installed concurrently by another tool.
            std::os::unix::fs::symlink(&self.target, &self.path)?;
        }
        Ok(())
    }

    pub fn remove(&self) -> Result<()> {
        if self.owned()? {
            fs::remove_file(&self.path)?;
        } else if fs::symlink_metadata(&self.path).is_ok() {
            eprintln!(
                "Preserved a command entry no longer owned by this installation: {}",
                self.path.display()
            );
        }
        Ok(())
    }

    pub fn restore(&self) -> Result<()> {
        if !self.existed {
            self.remove()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launcher_partial_write_can_be_retried_without_manual_cleanup() {
        struct InterruptedCopy(bool);
        impl io::Read for InterruptedCopy {
            fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
                if self.0 {
                    return Err(io::Error::other("interrupted launcher copy"));
                }
                self.0 = true;
                b"partial".as_slice().read(bytes)
            }
        }
        let root = tempfile::tempdir().expect("fixture");
        let source = root.path().join("source.exe");
        let path = root.path().join("zg.exe");
        fs::write(&source, b"complete launcher").expect("source");
        let digest = super::super::versioned::file_hash(&source).expect("digest");
        assert!(publish_launcher(&mut InterruptedCopy(false), &path, &digest).is_err());
        assert!(
            !path.exists(),
            "failed writes must not publish a partial command"
        );
        publish_launcher(
            &mut fs::File::open(&source).expect("source"),
            &path,
            &digest,
        )
        .expect("retry installation");
        assert_eq!(
            fs::read(path).expect("published launcher"),
            b"complete launcher"
        );
    }

    #[test]
    fn launcher_rejects_unverified_bytes_before_publication() {
        let root = tempfile::tempdir().expect("fixture");
        let path = root.path().join("zg.exe");
        assert!(publish_launcher(&mut b"corrupt".as_slice(), &path, &"0".repeat(64)).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn launcher_publication_preserves_a_concurrent_command() {
        struct ConcurrentCommand<'a>(&'a Path);
        impl io::Read for ConcurrentCommand<'_> {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                fs::write(self.0, b"foreign command")?;
                Ok(0)
            }
        }
        let root = tempfile::tempdir().expect("fixture");
        let source = root.path().join("empty");
        fs::write(&source, []).expect("source");
        let digest = super::super::versioned::file_hash(&source).expect("digest");
        let path = root.path().join("zg.exe");
        assert!(publish_launcher(&mut ConcurrentCommand(&path), &path, &digest).is_err());
        assert_eq!(fs::read(path).expect("foreign entry"), b"foreign command");
    }
}
