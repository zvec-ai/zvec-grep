//! Stable file identities and completion-marker timestamps.

use std::{
    fs, io,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CompleteFileStamp {
    pub(super) size: u64,
    pub(super) mtime_ms: f64,
    pub(super) ctime_ms: f64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FileStamp {
    size: u64,
    modified_ns: u64,
    changed: String,
}

fn file_stamp(metadata: &fs::Metadata) -> FileStamp {
    FileStamp {
        size: metadata.len(),
        modified_ns: system_time_ns(metadata.modified().ok()),
        changed: platform_file_identity(metadata),
    }
}

pub(super) fn complete_file_stamp(metadata: &fs::Metadata) -> CompleteFileStamp {
    CompleteFileStamp {
        size: metadata.len(),
        mtime_ms: platform_mtime_ms(metadata),
        ctime_ms: platform_ctime_ms(metadata),
    }
}

pub(super) fn complete_file_stamps_match(
    actual: &CompleteFileStamp,
    expected: &CompleteFileStamp,
) -> bool {
    actual.size == expected.size
        && json_timestamp_matches(actual.mtime_ms, expected.mtime_ms)
        && json_timestamp_matches(actual.ctime_ms, expected.ctime_ms)
}

fn json_timestamp_matches(actual: f64, expected: f64) -> bool {
    actual.is_finite()
        && expected.is_finite()
        && actual.is_sign_positive() == expected.is_sign_positive()
        && actual.to_bits().abs_diff(expected.to_bits()) <= 1
}

#[cfg(unix)]
#[expect(
    clippy::cast_precision_loss,
    reason = "Node completion markers store filesystem millisecond timestamps as JSON numbers"
)]
fn platform_mtime_ms(metadata: &fs::Metadata) -> f64 {
    use std::os::unix::fs::MetadataExt;
    metadata.mtime() as f64 * 1_000.0 + metadata.mtime_nsec() as f64 / 1_000_000.0
}

#[cfg(unix)]
#[expect(
    clippy::cast_precision_loss,
    reason = "Node completion markers store filesystem millisecond timestamps as JSON numbers"
)]
fn platform_ctime_ms(metadata: &fs::Metadata) -> f64 {
    use std::os::unix::fs::MetadataExt;
    metadata.ctime() as f64 * 1_000.0 + metadata.ctime_nsec() as f64 / 1_000_000.0
}

#[cfg(windows)]
fn platform_mtime_ms(metadata: &fs::Metadata) -> f64 {
    use std::os::windows::fs::MetadataExt;
    windows_file_time_ms(metadata.last_write_time())
}

#[cfg(windows)]
fn platform_ctime_ms(metadata: &fs::Metadata) -> f64 {
    use std::os::windows::fs::MetadataExt;
    // std does not expose Windows change time. Creation time is the closest
    // stable identity field available without reopening the file.
    windows_file_time_ms(metadata.creation_time())
}

#[cfg(windows)]
#[expect(
    clippy::cast_precision_loss,
    reason = "Node completion markers store filesystem millisecond timestamps as JSON numbers"
)]
fn windows_file_time_ms(value: u64) -> f64 {
    const UNIX_EPOCH_IN_100_NS: u64 = 116_444_736_000_000_000;
    value.saturating_sub(UNIX_EPOCH_IN_100_NS) as f64 / 10_000.0
}

#[cfg(not(any(unix, windows)))]
#[expect(
    clippy::cast_precision_loss,
    reason = "Node completion markers store filesystem millisecond timestamps as JSON numbers"
)]
fn platform_mtime_ms(metadata: &fs::Metadata) -> f64 {
    system_time_ns(metadata.modified().ok()) as f64 / 1_000_000.0
}

#[cfg(not(any(unix, windows)))]
#[expect(
    clippy::cast_precision_loss,
    reason = "Node completion markers store filesystem millisecond timestamps as JSON numbers"
)]
fn platform_ctime_ms(metadata: &fs::Metadata) -> f64 {
    system_time_ns(metadata.created().ok()) as f64 / 1_000_000.0
}

fn system_time_ns(time: Option<SystemTime>) -> u64 {
    time.and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map(|duration| u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

#[cfg(unix)]
fn platform_file_identity(metadata: &fs::Metadata) -> String {
    use std::os::unix::fs::MetadataExt;
    format!(
        "{}:{}:{}:{}:{}",
        metadata.dev(),
        metadata.ino(),
        metadata.mode(),
        metadata.ctime(),
        metadata.ctime_nsec()
    )
}

#[cfg(windows)]
fn platform_file_identity(metadata: &fs::Metadata) -> String {
    use std::os::windows::fs::MetadataExt;
    format!(
        "{}:{}:{}:{}",
        metadata.file_attributes(),
        metadata.creation_time(),
        metadata.last_write_time(),
        metadata.file_size()
    )
}

#[cfg(not(any(unix, windows)))]
fn platform_file_identity(metadata: &fs::Metadata) -> String {
    format!(
        "{}:{}",
        system_time_ns(metadata.created().ok()),
        system_time_ns(metadata.modified().ok())
    )
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct FileIdentity {
    stamp: FileStamp,
    is_file: bool,
    is_symlink: bool,
}

pub(super) fn inspect_file_identity(path: &Path) -> io::Result<Option<FileIdentity>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(FileIdentity {
            stamp: file_stamp(&metadata),
            is_file: metadata.is_file(),
            is_symlink: metadata.file_type().is_symlink(),
        })),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}
