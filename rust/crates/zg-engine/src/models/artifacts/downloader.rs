//! Integrity-checked, cross-process-safe model artifact resolution.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use tokio::{fs as async_fs, io::AsyncWriteExt};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[cfg(test)]
use super::cache_lock::{
    LOCK_STALE_AFTER, LockOwner, inspect_existing_lock, is_directory_conflict, lock_entry_metadata,
    remove_stale_lock_at,
};
#[cfg(test)]
use super::identity::{CompleteFileStamp, complete_file_stamp, complete_file_stamps_match};
#[cfg(test)]
use super::manifest::has_valid_complete_marker;
use super::{
    cache_lock::CacheLock,
    error::{ArtifactDownloadError, FailureKind, filesystem_error},
    identity::inspect_file_identity,
    manifest::{
        Manifest, validate_artifact, validate_snapshot, write_complete_marker,
        write_complete_marker_best_effort,
    },
    progress::{ArtifactDownloadProgress, ModelDownloadProgressReporter},
    publish::publish_downloaded_file,
    source::{ArtifactSource, validate_relative_path},
    spec::{ArtifactConfig, ArtifactSourceKind},
};
use crate::models::error::ModelError;
#[cfg(test)]
use sysinfo::System;

pub(crate) struct ResolveArtifacts<'a> {
    pub(crate) model: &'static str,
    pub(crate) sources: [ArtifactSource; 2],
    pub(crate) artifacts: &'static [ArtifactConfig],
    pub(crate) reporter: &'a ModelDownloadProgressReporter,
    pub(crate) signal: Option<&'a CancellationToken>,
}

#[derive(Debug)]
pub(crate) struct ResolvedArtifacts {
    pub(crate) paths: HashMap<&'static str, PathBuf>,
}

pub(crate) async fn resolve_model_artifacts(
    client: &reqwest::Client,
    mut request: ResolveArtifacts<'_>,
) -> Result<ResolvedArtifacts, ModelError> {
    validate_request(&request).map_err(ArtifactDownloadError::into_model_error)?;
    request.sources.sort_by_key(|source| match source.kind {
        ArtifactSourceKind::HuggingFace => 0,
        ArtifactSourceKind::ModelScope => 1,
    });
    let manifests = request
        .sources
        .iter()
        .map(|source| Manifest::new(request.model, source, request.artifacts))
        .collect::<Result<Vec<_>, _>>()
        .map_err(ArtifactDownloadError::into_model_error)?;

    // Prefer any complete local snapshot over a network request, including a
    // ModelScope snapshot left by an earlier Hugging Face failure.
    for (source, manifest) in request.sources.iter().zip(&manifests) {
        check_cancelled(request.signal).map_err(ArtifactDownloadError::into_model_error)?;
        if validate_snapshot(source, request.artifacts, manifest, None)
            .await
            .map_err(ArtifactDownloadError::into_model_error)?
        {
            write_complete_marker_best_effort(source, request.artifacts, manifest).await;
            return resolved_result(source, request.artifacts);
        }
    }

    let mut primary_error = None;
    for (index, (source, manifest)) in request.sources.iter().zip(&manifests).enumerate() {
        match download_source_snapshot(client, &request, source, manifest).await {
            Ok(()) => return resolved_result(source, request.artifacts),
            Err(error) => {
                if error.is_cancelled() {
                    return Err(error.into_model_error());
                }
                if index == 0 {
                    if !error.fallback_allowed() {
                        return Err(error.into_model_error());
                    }
                    primary_error = Some(error);
                    report_fallback(request.reporter, request.model, source.kind)
                        .map_err(ArtifactDownloadError::into_model_error)?;
                    continue;
                }
                if let Some(primary) = primary_error {
                    return Err(ModelError::storage_failure(format!(
                        "Unable to download artifacts for {} from Hugging Face or ModelScope; Hugging Face: {primary}; ModelScope: {error}",
                        request.model
                    )));
                }
                return Err(error.into_model_error());
            }
        }
    }

    Err(ModelError::storage_failure(format!(
        "No artifact source is available for {}",
        request.model
    )))
}

fn validate_request(request: &ResolveArtifacts<'_>) -> Result<(), ArtifactDownloadError> {
    if request.model.trim().is_empty() || request.artifacts.is_empty() {
        return Err(ArtifactDownloadError::new(
            FailureKind::InvalidInput,
            "model and artifacts must not be empty",
        ));
    }
    let mut paths = std::collections::HashSet::new();
    for artifact in request.artifacts {
        validate_relative_path(Path::new(artifact.path))?;
        if !paths.insert(artifact.path)
            || artifact.sha256.len() != 64
            || !artifact.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(ArtifactDownloadError::new(
                FailureKind::InvalidInput,
                format!("invalid or duplicate artifact '{}'", artifact.path),
            ));
        }
    }
    if request.sources[0].kind == request.sources[1].kind {
        return Err(ArtifactDownloadError::new(
            FailureKind::InvalidInput,
            format!(
                "duplicate artifact source '{}'",
                request.sources[0].kind.label()
            ),
        ));
    }
    for source in &request.sources {
        validate_relative_path(Path::new(source.repo))?;
        if source.repo.trim().is_empty()
            || source.revision.trim().is_empty()
            || source.revision.contains('\0')
            || matches!(source.revision, "." | "..")
            || source.cache_directory.as_os_str().is_empty()
        {
            return Err(ArtifactDownloadError::new(
                FailureKind::InvalidInput,
                "artifact source repository, revision, and cache directory must be valid",
            ));
        }
        let mut local_paths = std::collections::HashSet::new();
        for (artifact, local_path) in &source.local_paths {
            if !paths.contains(artifact) {
                return Err(ArtifactDownloadError::new(
                    FailureKind::InvalidInput,
                    format!("local path maps unknown artifact '{artifact}'"),
                ));
            }
            validate_relative_path(local_path)?;
            if !local_paths.insert(local_path) {
                return Err(ArtifactDownloadError::new(
                    FailureKind::InvalidInput,
                    format!(
                        "multiple {} artifacts map to '{}'",
                        source.kind.label(),
                        local_path.display()
                    ),
                ));
            }
        }
    }
    Ok(())
}

fn resolved_result(
    source: &ArtifactSource,
    artifacts: &'static [ArtifactConfig],
) -> Result<ResolvedArtifacts, ModelError> {
    let paths = artifacts
        .iter()
        .map(|artifact| {
            source
                .local_path(artifact)
                .map(|path| (artifact.path, path))
        })
        .collect::<Result<HashMap<_, _>, _>>()
        .map_err(ArtifactDownloadError::into_model_error)?;
    Ok(ResolvedArtifacts { paths })
}

async fn download_source_snapshot(
    client: &reqwest::Client,
    request: &ResolveArtifacts<'_>,
    source: &ArtifactSource,
    manifest: &Manifest,
) -> Result<(), ArtifactDownloadError> {
    async_fs::create_dir_all(&source.cache_directory)
        .await
        .map_err(|error| filesystem_error(source.kind, None, "create model cache", error))?;
    let lock = CacheLock::acquire(&manifest.lock_path, request.signal).await?;
    if validate_snapshot(source, request.artifacts, manifest, Some(&lock)).await? {
        lock.assert_owned()?;
        return write_complete_marker(source, request.artifacts, manifest).await;
    }
    let mut missing = Vec::new();
    for artifact in request.artifacts {
        if !validate_artifact(source, artifact, Some(&lock)).await? {
            missing.push(*artifact);
        }
    }
    set_download_plan(request.reporter, &missing, source.kind)?;
    for artifact in &missing {
        download_artifact(client, request, source, artifact, &lock).await?;
    }
    lock.assert_owned()?;
    write_complete_marker(source, request.artifacts, manifest).await
}

#[expect(
    clippy::too_many_lines,
    reason = "the network stream, integrity check, and fenced publication form one transaction"
)]
async fn download_artifact(
    client: &reqwest::Client,
    request: &ResolveArtifacts<'_>,
    source: &ArtifactSource,
    artifact: &ArtifactConfig,
    lock: &CacheLock,
) -> Result<(), ArtifactDownloadError> {
    check_cancelled(request.signal)?;
    let destination = source.local_path(artifact)?;
    let parent = destination.parent().ok_or_else(|| {
        ArtifactDownloadError::new(FailureKind::InvalidInput, "artifact has no parent")
    })?;
    async_fs::create_dir_all(parent).await.map_err(|error| {
        filesystem_error(
            source.kind,
            Some(artifact.path),
            "create artifact directory",
            error,
        )
    })?;
    let original = inspect_file_identity(&destination).map_err(|error| {
        filesystem_error(
            source.kind,
            Some(artifact.path),
            "inspect artifact destination",
            error,
        )
    })?;
    let partial =
        destination.with_extension(format!("part-{}-{}", std::process::id(), Uuid::new_v4()));
    let mut cleanup = CleanupPath::new(partial.clone());
    report_progress(request.reporter, artifact, 0, source.kind)?;
    let url = artifact_url(source, artifact.path);
    let response = wait_or_cancel(
        request.signal,
        tokio::time::timeout(source.response_header_timeout, client.get(&url).send()),
    )
    .await?
    .map_err(|_| {
        ArtifactDownloadError::new(
            FailureKind::Timeout,
            "timed out waiting for model response headers",
        )
        .at_source(source.kind)
        .at_artifact(artifact.path)
    })?
    .map_err(|error| {
        ArtifactDownloadError::new(
            FailureKind::Network,
            format!("unable to request model artifact: {error}"),
        )
        .at_source(source.kind)
        .at_artifact(artifact.path)
    })?;
    if !response.status().is_success() {
        let status = response.status().as_u16();
        return Err(ArtifactDownloadError::new(
            FailureKind::Http,
            format!(
                "HTTP {} while downloading model artifact",
                response.status()
            ),
        )
        .at_source(source.kind)
        .at_artifact(artifact.path)
        .with_status(status));
    }

    let mut file = async_fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&partial)
        .await
        .map_err(|error| {
            filesystem_error(
                source.kind,
                Some(artifact.path),
                "create partial artifact",
                error,
            )
        })?;
    let mut stream = response.bytes_stream();
    let mut downloaded = 0_u64;
    let mut hash = Sha256::new();
    loop {
        let next = wait_or_cancel(
            request.signal,
            tokio::time::timeout(source.read_idle_timeout, stream.next()),
        )
        .await?
        .map_err(|_| {
            ArtifactDownloadError::new(
                FailureKind::Timeout,
                "timed out waiting for model download data",
            )
            .at_source(source.kind)
            .at_artifact(artifact.path)
        })?;
        let Some(chunk) = next else { break };
        let chunk = chunk.map_err(|error| {
            ArtifactDownloadError::new(
                FailureKind::Network,
                format!("model download stream failed: {error}"),
            )
            .at_source(source.kind)
            .at_artifact(artifact.path)
        })?;
        file.write_all(&chunk).await.map_err(|error| {
            filesystem_error(
                source.kind,
                Some(artifact.path),
                "write partial artifact",
                error,
            )
        })?;
        hash.update(&chunk);
        downloaded = downloaded.saturating_add(chunk.len() as u64);
        if downloaded > artifact.size {
            return Err(ArtifactDownloadError::new(
                FailureKind::Integrity,
                format!("artifact exceeded expected {} bytes", artifact.size),
            )
            .at_source(source.kind)
            .at_artifact(artifact.path));
        }
        lock.touch()?;
        report_progress(request.reporter, artifact, downloaded, source.kind)?;
    }
    file.flush().await.map_err(|error| {
        filesystem_error(
            source.kind,
            Some(artifact.path),
            "flush partial artifact",
            error,
        )
    })?;
    file.sync_all().await.map_err(|error| {
        filesystem_error(
            source.kind,
            Some(artifact.path),
            "sync partial artifact",
            error,
        )
    })?;
    drop(file);

    let digest = hex::encode(hash.finalize());
    if downloaded != artifact.size || !digest.eq_ignore_ascii_case(artifact.sha256) {
        return Err(ArtifactDownloadError::new(
            FailureKind::Integrity,
            format!(
                "integrity check failed: expected {} bytes/{}, received {downloaded} bytes/{digest}",
                artifact.size, artifact.sha256
            ),
        )
        .at_source(source.kind)
        .at_artifact(artifact.path));
    }

    lock.assert_owned()?;
    let current = inspect_file_identity(&destination).map_err(|error| {
        filesystem_error(
            source.kind,
            Some(artifact.path),
            "reinspect artifact destination",
            error,
        )
    })?;
    if current != original {
        if validate_artifact(source, artifact, Some(lock)).await? {
            return Ok(());
        }
        return Err(ArtifactDownloadError::new(
            FailureKind::Filesystem,
            "artifact destination changed concurrently while downloading",
        )
        .at_source(source.kind)
        .at_artifact(artifact.path));
    }
    publish_downloaded_file(&partial, &destination)
        .await
        .map_err(|error| {
            ArtifactDownloadError::new(
                FailureKind::Filesystem,
                format!("unable to publish verified artifact: {error}"),
            )
            .at_source(source.kind)
            .at_artifact(artifact.path)
        })?;
    cleanup.disarm();
    Ok(())
}

async fn wait_or_cancel<T>(
    signal: Option<&CancellationToken>,
    future: impl std::future::Future<Output = T>,
) -> Result<T, ArtifactDownloadError> {
    if let Some(signal) = signal {
        tokio::select! {
            () = signal.cancelled() => Err(cancelled()),
            output = future => Ok(output),
        }
    } else {
        Ok(future.await)
    }
}

fn check_cancelled(signal: Option<&CancellationToken>) -> Result<(), ArtifactDownloadError> {
    if signal.is_some_and(CancellationToken::is_cancelled) {
        Err(cancelled())
    } else {
        Ok(())
    }
}

fn cancelled() -> ArtifactDownloadError {
    ArtifactDownloadError::new(
        FailureKind::Cancelled,
        "model artifact download was cancelled",
    )
}

fn artifact_url(source: &ArtifactSource, artifact: &str) -> String {
    let base = source.base_url.as_deref().unwrap_or(match source.kind {
        ArtifactSourceKind::HuggingFace => "https://huggingface.co",
        ArtifactSourceKind::ModelScope => "https://modelscope.cn/models",
    });
    format!(
        "{}/{}/resolve/{}/{}",
        base.trim_end_matches('/'),
        encode_path(source.repo),
        encode_component(source.revision),
        encode_path(artifact)
    )
}

fn encode_path(value: &str) -> String {
    value
        .split('/')
        .map(encode_component)
        .collect::<Vec<_>>()
        .join("/")
}

fn encode_component(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
            )
        {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write;
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

fn set_download_plan(
    reporter: &ModelDownloadProgressReporter,
    artifacts: &[ArtifactConfig],
    source: ArtifactSourceKind,
) -> Result<(), ArtifactDownloadError> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        reporter.set_download_plan(
            artifacts
                .iter()
                .map(|artifact| (artifact.path.to_owned(), artifact.size)),
        );
    }))
    .map_err(|_| {
        ArtifactDownloadError::new(FailureKind::Callback, "download plan callback panicked")
            .at_source(source)
    })
}

fn report_progress(
    reporter: &ModelDownloadProgressReporter,
    artifact: &ArtifactConfig,
    downloaded_bytes: u64,
    source: ArtifactSourceKind,
) -> Result<(), ArtifactDownloadError> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        reporter.report(
            artifact.path,
            ArtifactDownloadProgress {
                downloaded_bytes,
                total_bytes: Some(artifact.size),
            },
        );
    }))
    .map_err(|_| {
        ArtifactDownloadError::new(FailureKind::Callback, "download progress callback panicked")
            .at_source(source)
            .at_artifact(artifact.path)
    })
}

fn report_fallback(
    reporter: &ModelDownloadProgressReporter,
    model: &str,
    source: ArtifactSourceKind,
) -> Result<(), ArtifactDownloadError> {
    let message = format!(
        "{} download failed for {model}; falling back to ModelScope.",
        source.display_name()
    );
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if !reporter.warning(message.clone()) {
            tracing::warn!(%message);
        }
    }))
    .map_err(|_| {
        ArtifactDownloadError::new(FailureKind::Callback, "fallback callback panicked")
            .at_source(source)
    })
}

struct CleanupPath {
    path: Option<PathBuf>,
}

impl CleanupPath {
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    fn disarm(&mut self) {
        self.path = None;
    }
}

impl Drop for CleanupPath {
    fn drop(&mut self) {
        if let Some(path) = &self.path {
            let _ = fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests;
