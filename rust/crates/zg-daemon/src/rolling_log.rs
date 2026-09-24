//! Size-based log rotation for the resident daemon's tracing output.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use zg_engine::config::DaemonLogOptions;

#[derive(Clone)]
pub struct RollingLog(Arc<Mutex<LogFile>>);

struct LogFile {
    path: PathBuf,
    file: Option<File>,
    bytes: u64,
    options: DaemonLogOptions,
}

impl RollingLog {
    /// Opens the active log and removes backups beyond the configured retention.
    /// # Errors
    /// Returns directory, permission, or file I/O errors.
    pub fn open(home: &Path, options: DaemonLogOptions) -> io::Result<Self> {
        let directory = home.join("daemon").join("logs");
        fs::create_dir_all(&directory)?;
        private_directory(&directory)?;
        let path = directory.join("server.log");
        prune_backups(&path, options.keep)?;
        let file = open_active(&path)?;
        let bytes = file.metadata()?.len();
        Ok(Self(Arc::new(Mutex::new(LogFile {
            path,
            file: Some(file),
            bytes,
            options,
        }))))
    }

    /// Creates a writer buffering one complete tracing event before rotation.
    #[must_use]
    pub fn writer(&self) -> LogWriter {
        LogWriter {
            log: self.clone(),
            buffer: Vec::new(),
        }
    }
}

pub struct LogWriter {
    log: RollingLog,
    buffer: Vec<u8>,
}

impl Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.buffer.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let mut log = self
            .log
            .0
            .lock()
            .map_err(|_| io::Error::other("daemon log lock poisoned"))?;
        log.write_record(&self.buffer)?;
        self.buffer.clear();
        Ok(())
    }
}

impl Drop for LogWriter {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

impl LogFile {
    fn write_record(&mut self, record: &[u8]) -> io::Result<()> {
        if self.file.is_none() {
            let file = open_active(&self.path)?;
            self.bytes = file.metadata()?.len();
            self.file = Some(file);
        }
        if self.bytes >= self.options.max_bytes {
            self.rotate()?;
        }
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other("daemon log is closed"))?;
        file.write_all(record)?;
        self.bytes = self.bytes.saturating_add(record.len() as u64);
        if self.bytes >= self.options.max_bytes {
            self.rotate()?;
        }
        Ok(())
    }

    fn rotate(&mut self) -> io::Result<()> {
        self.file.take();
        let rotation = (|| -> io::Result<()> {
            if self.options.keep == 0 {
                fs::remove_file(&self.path)?;
            } else {
                // Move existing backups from oldest to newest without iterating a
                // potentially huge configured retention value.
                let mut backups = numbered_backups(&self.path)?;
                backups.sort_unstable_by(|left, right| right.cmp(left));
                for index in backups {
                    let source = backup_path(&self.path, index);
                    if index >= self.options.keep {
                        fs::remove_file(source)?;
                    } else {
                        fs::rename(source, backup_path(&self.path, index + 1))?;
                    }
                }
                fs::rename(&self.path, backup_path(&self.path, 1))?;
            }
            Ok(())
        })();
        let reopened = open_active(&self.path).and_then(|file| {
            self.bytes = file.metadata()?.len();
            self.file = Some(file);
            Ok(())
        });
        rotation?;
        reopened
    }
}

fn numbered_backups(path: &Path) -> io::Result<Vec<u64>> {
    let mut backups = Vec::new();
    for entry in fs::read_dir(
        path.parent()
            .ok_or_else(|| io::Error::other("log has no parent"))?,
    )? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(suffix) = name.strip_prefix("server.log.") else {
            continue;
        };
        if !suffix
            .as_bytes()
            .first()
            .is_some_and(|byte| byte.is_ascii_digit() && *byte != b'0')
            || !suffix.bytes().all(|byte| byte.is_ascii_digit())
        {
            continue;
        }
        if let Ok(index) = suffix.parse::<u64>() {
            backups.push(index);
        }
    }
    Ok(backups)
}

fn backup_path(path: &Path, index: u64) -> PathBuf {
    path.with_file_name(format!("server.log.{index}"))
}

fn prune_backups(path: &Path, keep: u64) -> io::Result<()> {
    for index in numbered_backups(path)? {
        if index > keep {
            fs::remove_file(backup_path(path, index))?;
        }
    }
    Ok(())
}

fn open_active(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

fn private_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(max_bytes: u64, keep: u64) -> DaemonLogOptions {
        DaemonLogOptions {
            max_bytes,
            keep,
            debug: false,
        }
    }

    fn write(log: &RollingLog, record: &str) {
        let mut writer = log.writer();
        writer.write_all(record.as_bytes()).expect("buffer record");
        writer.flush().expect("write record");
    }

    #[test]
    fn rotates_complete_utf8_records_and_bounds_backups() {
        let home = tempfile::tempdir().expect("home");
        let record = "日志0\n";
        let log = RollingLog::open(home.path(), options(record.len() as u64, 2)).expect("log");
        for index in 0..5 {
            write(&log, &format!("日志{index}\n"));
        }
        let path = home.path().join("daemon").join("logs").join("server.log");
        assert_eq!(fs::read(&path).expect("active log"), b"");
        assert_eq!(
            fs::read(backup_path(&path, 1)).expect("newest"),
            "日志4\n".as_bytes()
        );
        assert_eq!(
            fs::read(backup_path(&path, 2)).expect("older"),
            "日志3\n".as_bytes()
        );
        assert!(!backup_path(&path, 3).exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for path in [path.clone(), backup_path(&path, 1), backup_path(&path, 2)] {
                assert_eq!(
                    fs::metadata(path).expect("metadata").permissions().mode() & 0o777,
                    0o600
                );
            }
        }
    }

    #[test]
    fn restart_rotates_oversized_file_and_prunes_reduced_retention() {
        let home = tempfile::tempdir().expect("home");
        let path = home.path().join("daemon").join("logs").join("server.log");
        let first = RollingLog::open(home.path(), options(100, 5)).expect("first log");
        write(&first, "old record\n");
        drop(first);
        fs::write(&path, "existing oversized record\n").expect("existing log");
        for index in 1..=5 {
            fs::write(backup_path(&path, index), "backup\n").expect("backup");
        }
        let second = RollingLog::open(home.path(), options(10, 2)).expect("second log");
        assert!(!backup_path(&path, 3).exists());
        write(&second, "new\n");
        assert_eq!(fs::read_to_string(&path).expect("active"), "new\n");
        assert_eq!(
            fs::read_to_string(backup_path(&path, 1)).expect("newest"),
            "existing oversized record\n"
        );
        assert_eq!(
            fs::read_to_string(backup_path(&path, 2)).expect("older"),
            "backup\n"
        );
    }

    #[test]
    fn zero_retention_discards_old_records() {
        let home = tempfile::tempdir().expect("home");
        let log = RollingLog::open(home.path(), options(1, 0)).expect("log");
        write(&log, "old\n");
        let path = home.path().join("daemon").join("logs").join("server.log");
        assert_eq!(fs::read(&path).expect("active"), b"");
        assert!(!backup_path(&path, 1).exists());
    }
}
