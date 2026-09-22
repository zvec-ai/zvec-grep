//! Artifact manifest fingerprints, completion markers, and integrity checks.

use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::fs as async_fs;

use super::{
    cache_lock::CacheLock,
    error::{ArtifactDownloadError, FailureKind, filesystem_error},
    identity::{CompleteFileStamp, complete_file_stamp, complete_file_stamps_match},
    source::ArtifactSource,
    spec::{ArtifactConfig, ArtifactSourceKind},
};
use crate::utils::atomic_write;

pub(super) const MARKER_VERSION: u32 = 1;

pub(super) struct Manifest {
    pub(super) fingerprint: String,
    pub(super) marker_path: PathBuf,
    pub(super) lock_path: PathBuf,
}

impl Manifest {
    pub(super) fn new(
        model: &str,
        source: &ArtifactSource,
        artifacts: &[ArtifactConfig],
    ) -> Result<Self, ArtifactDownloadError> {
        let mut local_paths = std::collections::HashSet::new();
        let mut manifest_artifacts = Vec::with_capacity(artifacts.len());
        for artifact in artifacts {
            let local = source
                .local_paths
                .get(artifact.path)
                .map_or_else(|| Path::new(artifact.path), PathBuf::as_path);
            if !local_paths.insert(local) {
                return Err(ArtifactDownloadError::new(
                    FailureKind::InvalidInput,
                    format!(
                        "multiple {} artifacts map to '{}'",
                        source.kind.label(),
                        local.display()
                    ),
                ));
            }
            let local_path = local.to_str().ok_or_else(|| {
                ArtifactDownloadError::new(
                    FailureKind::InvalidInput,
                    format!("artifact local path '{}' is not UTF-8", local.display()),
                )
            })?;
            manifest_artifacts.push(FingerprintArtifact {
                path: artifact.path,
                local_path,
                size: artifact.size,
                sha256: artifact.sha256.to_ascii_lowercase(),
            });
        }
        // This serialized shape and field order intentionally match the Node.js
        // implementation so both runtimes share completion markers.
        let serialized = serde_json::to_vec(&FingerprintManifest {
            version: MARKER_VERSION,
            model,
            source: FingerprintSource {
                kind: source.kind,
                repo: source.repo,
                revision: source.revision,
            },
            artifacts: manifest_artifacts,
        })
        .map_err(|error| {
            ArtifactDownloadError::new(
                FailureKind::InvalidInput,
                format!("unable to fingerprint artifact manifest: {error}"),
            )
        })?;
        let fingerprint = hex::encode(Sha256::digest(serialized));
        let fingerprint = fingerprint[..24].to_owned();
        Ok(Self {
            marker_path: source
                .cache_directory
                .join(format!(".zvec-grep-artifacts-{fingerprint}.complete")),
            lock_path: source
                .cache_directory
                .join(format!(".zvec-grep-artifacts-{fingerprint}.lock")),
            fingerprint,
        })
    }
}

#[derive(Serialize)]
struct FingerprintManifest<'a> {
    version: u32,
    model: &'a str,
    source: FingerprintSource<'a>,
    artifacts: Vec<FingerprintArtifact<'a>>,
}

#[derive(Serialize)]
struct FingerprintSource<'a> {
    kind: ArtifactSourceKind,
    repo: &'a str,
    revision: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FingerprintArtifact<'a> {
    path: &'a str,
    local_path: &'a str,
    size: u64,
    sha256: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct CompleteMarker {
    version: u32,
    fingerprint: String,
    files: BTreeMap<String, CompleteFileStamp>,
}

pub(super) async fn validate_snapshot(
    source: &ArtifactSource,
    artifacts: &[ArtifactConfig],
    manifest: &Manifest,
    lock: Option<&CacheLock>,
) -> Result<bool, ArtifactDownloadError> {
    if has_valid_complete_marker(source, artifacts, manifest).await? {
        return Ok(true);
    }
    for artifact in artifacts {
        if !validate_artifact(source, artifact, lock).await? {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(super) async fn has_valid_complete_marker(
    source: &ArtifactSource,
    artifacts: &[ArtifactConfig],
    manifest: &Manifest,
) -> Result<bool, ArtifactDownloadError> {
    let bytes = match async_fs::read(&manifest.marker_path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(filesystem_error(
                source.kind,
                None,
                "read completion marker",
                error,
            ));
        }
    };
    let Ok(marker) = serde_json::from_slice::<CompleteMarker>(&bytes) else {
        return Ok(false);
    };
    if marker.version != MARKER_VERSION || marker.fingerprint != manifest.fingerprint {
        return Ok(false);
    }
    for artifact in artifacts {
        let Some(expected) = marker.files.get(artifact.path) else {
            return Ok(false);
        };
        let path = source.local_path(artifact)?;
        let metadata = match async_fs::metadata(&path).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(filesystem_error(
                    source.kind,
                    Some(artifact.path),
                    "inspect cached artifact",
                    error,
                ));
            }
        };
        if !metadata.is_file()
            || metadata.len() != artifact.size
            || !complete_file_stamps_match(&complete_file_stamp(&metadata), expected)
        {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(super) async fn validate_artifact(
    source: &ArtifactSource,
    artifact: &ArtifactConfig,
    lock: Option<&CacheLock>,
) -> Result<bool, ArtifactDownloadError> {
    let path = source.local_path(artifact)?;
    let metadata = match async_fs::metadata(&path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(filesystem_error(
                source.kind,
                Some(artifact.path),
                "inspect cached artifact",
                error,
            ));
        }
    };
    if !metadata.is_file() || metadata.len() != artifact.size {
        return Ok(false);
    }
    let mut file = match async_fs::File::open(&path).await {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(filesystem_error(
                source.kind,
                Some(artifact.path),
                "open cached artifact",
                error,
            ));
        }
    };
    let mut hash = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        use tokio::io::AsyncReadExt;
        let count = match file.read(&mut buffer).await {
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(filesystem_error(
                    source.kind,
                    Some(artifact.path),
                    "hash cached artifact",
                    error,
                ));
            }
        };
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
        if let Some(lock) = lock {
            lock.touch()?;
        }
    }
    Ok(hex::encode(hash.finalize()).eq_ignore_ascii_case(artifact.sha256))
}

pub(super) async fn write_complete_marker(
    source: &ArtifactSource,
    artifacts: &[ArtifactConfig],
    manifest: &Manifest,
) -> Result<(), ArtifactDownloadError> {
    let mut files = BTreeMap::new();
    for artifact in artifacts {
        let path = source.local_path(artifact)?;
        let metadata = async_fs::metadata(&path).await.map_err(|error| {
            filesystem_error(
                source.kind,
                Some(artifact.path),
                "inspect completed artifact",
                error,
            )
        })?;
        if !metadata.is_file() {
            return Err(filesystem_error(
                source.kind,
                Some(artifact.path),
                "record completion marker",
                io::Error::new(io::ErrorKind::InvalidData, "artifact is not a file"),
            ));
        }
        files.insert(artifact.path.to_owned(), complete_file_stamp(&metadata));
    }
    let bytes = serde_json::to_vec(&CompleteMarker {
        version: MARKER_VERSION,
        fingerprint: manifest.fingerprint.clone(),
        files,
    })
    .map_err(|error| {
        ArtifactDownloadError::new(
            FailureKind::Filesystem,
            format!("unable to serialize completion marker: {error}"),
        )
        .at_source(source.kind)
    })?;
    let path = manifest.marker_path.clone();
    tokio::task::spawn_blocking(move || atomic_write(&path, &bytes))
        .await
        .map_err(|error| {
            ArtifactDownloadError::new(
                FailureKind::Filesystem,
                format!("completion marker task failed: {error}"),
            )
            .at_source(source.kind)
        })?
        .map_err(|error| {
            ArtifactDownloadError::new(
                FailureKind::Filesystem,
                format!("unable to publish completion marker: {error}"),
            )
            .at_source(source.kind)
        })
}

pub(super) async fn write_complete_marker_best_effort(
    source: &ArtifactSource,
    artifacts: &[ArtifactConfig],
    manifest: &Manifest,
) {
    if !has_valid_complete_marker(source, artifacts, manifest)
        .await
        .unwrap_or(false)
    {
        let _ = async_fs::create_dir_all(&source.cache_directory).await;
        let _ = write_complete_marker(source, artifacts, manifest).await;
    }
}
