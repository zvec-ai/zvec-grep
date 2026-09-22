//! Integrity-checked, cross-process-safe model artifact resolution.

use std::{
    collections::{BTreeMap, HashMap},
    fs::{self, FileTimes, OpenOptions},
    io,
    path::{Component, Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sysinfo::{Pid, ProcessesToUpdate, System};
use tokio::{fs as async_fs, io::AsyncWriteExt};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{
    artifacts::publish_downloaded_file,
    catalog::{ArtifactConfig, ArtifactSourceConfig},
    download_progress::{ArtifactDownloadProgress, ModelDownloadProgressReporter},
    error::ModelError,
};
use crate::utils::atomic_write;

const RESPONSE_HEADER_TIMEOUT: Duration = Duration::from_secs(10);
const READ_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(250);
const LOCK_STALE_AFTER: Duration = Duration::from_mins(10);
const LOCK_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
const MARKER_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ArtifactSourceKind {
    HuggingFace,
    ModelScope,
}

impl ArtifactSourceKind {
    const fn label(self) -> &'static str {
        match self {
            Self::HuggingFace => "huggingface",
            Self::ModelScope => "modelscope",
        }
    }

    const fn display_name(self) -> &'static str {
        match self {
            Self::HuggingFace => "Hugging Face",
            Self::ModelScope => "ModelScope",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ArtifactSource {
    kind: ArtifactSourceKind,
    repo: &'static str,
    revision: &'static str,
    cache_directory: PathBuf,
    local_paths: BTreeMap<&'static str, PathBuf>,
    base_url: Option<String>,
    response_header_timeout: Duration,
    read_idle_timeout: Duration,
}

impl ArtifactSource {
    pub(crate) fn hugging_face(config: ArtifactSourceConfig, cache_directory: PathBuf) -> Self {
        Self::new(ArtifactSourceKind::HuggingFace, config, cache_directory)
    }

    pub(crate) fn model_scope(config: ArtifactSourceConfig, cache_directory: PathBuf) -> Self {
        Self::new(ArtifactSourceKind::ModelScope, config, cache_directory)
    }

    fn new(
        kind: ArtifactSourceKind,
        config: ArtifactSourceConfig,
        cache_directory: PathBuf,
    ) -> Self {
        Self {
            kind,
            repo: config.repo,
            revision: config.revision,
            cache_directory,
            local_paths: BTreeMap::new(),
            base_url: None,
            response_header_timeout: RESPONSE_HEADER_TIMEOUT,
            read_idle_timeout: READ_IDLE_TIMEOUT,
        }
    }

    pub(crate) fn with_local_path(
        mut self,
        artifact: &'static str,
        local_path: impl Into<PathBuf>,
    ) -> Self {
        self.local_paths.insert(artifact, local_path.into());
        self
    }

    #[cfg(test)]
    fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = Some(base_url.into());
        self
    }

    #[cfg(test)]
    fn with_timeouts(mut self, response_header: Duration, read_idle: Duration) -> Self {
        self.response_header_timeout = response_header;
        self.read_idle_timeout = read_idle;
        self
    }

    fn local_path(&self, artifact: &ArtifactConfig) -> Result<PathBuf, ArtifactDownloadError> {
        safe_local_path(
            &self.cache_directory,
            self.local_paths
                .get(artifact.path)
                .map_or_else(|| Path::new(artifact.path), PathBuf::as_path),
        )
    }
}

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FailureKind {
    Http,
    Network,
    Timeout,
    Integrity,
    Filesystem,
    Cancelled,
    Callback,
    InvalidInput,
}

#[derive(Debug)]
pub(crate) struct ArtifactDownloadError {
    kind: FailureKind,
    source: Option<ArtifactSourceKind>,
    artifact: Option<String>,
    status: Option<u16>,
    message: String,
}

impl ArtifactDownloadError {
    fn new(kind: FailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            source: None,
            artifact: None,
            status: None,
            message: message.into(),
        }
    }

    fn at_source(mut self, source: ArtifactSourceKind) -> Self {
        self.source = Some(source);
        self
    }

    fn at_artifact(mut self, artifact: impl Into<String>) -> Self {
        self.artifact = Some(artifact.into());
        self
    }

    fn with_status(mut self, status: u16) -> Self {
        self.status = Some(status);
        self
    }

    fn fallback_allowed(&self) -> bool {
        matches!(
            self.kind,
            FailureKind::Network | FailureKind::Timeout | FailureKind::Integrity
        ) || (self.kind == FailureKind::Http
            && self.status.is_some_and(|status| {
                matches!(status, 403 | 404 | 408 | 429) || (500..=599).contains(&status)
            }))
    }

    fn into_model_error(self) -> ModelError {
        if self.kind == FailureKind::Cancelled {
            ModelError::cancelled(self.to_string())
        } else {
            ModelError::storage_failure(self.to_string())
        }
    }
}

impl std::fmt::Display for ArtifactDownloadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.message)?;
        if let Some(source) = self.source {
            write!(formatter, "; source={}", source.label())?;
        }
        if let Some(artifact) = &self.artifact {
            write!(formatter, "; artifact={artifact}")?;
        }
        if let Some(status) = self.status {
            write!(formatter, "; status={status}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ArtifactDownloadError {}

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
                if error.kind == FailureKind::Cancelled {
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

#[derive(Debug)]
struct Manifest {
    fingerprint: String,
    marker_path: PathBuf,
    lock_path: PathBuf,
}

impl Manifest {
    fn new(
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

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct CompleteFileStamp {
    size: u64,
    mtime_ms: f64,
    ctime_ms: f64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FileStamp {
    size: u64,
    modified_ns: u64,
    changed: String,
}

async fn validate_snapshot(
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

async fn has_valid_complete_marker(
    source: &ArtifactSource,
    artifacts: &[ArtifactConfig],
    manifest: &Manifest,
) -> Result<bool, ArtifactDownloadError> {
    let bytes = match async_fs::read(&manifest.marker_path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(filesystem_error(
                source,
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
                    source,
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

async fn validate_artifact(
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
                source,
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
                source,
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
                    source,
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

async fn download_source_snapshot(
    client: &reqwest::Client,
    request: &ResolveArtifacts<'_>,
    source: &ArtifactSource,
    manifest: &Manifest,
) -> Result<(), ArtifactDownloadError> {
    async_fs::create_dir_all(&source.cache_directory)
        .await
        .map_err(|error| filesystem_error(source, None, "create model cache", error))?;
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
            source,
            Some(artifact.path),
            "create artifact directory",
            error,
        )
    })?;
    let original = inspect_file_identity(&destination).map_err(|error| {
        filesystem_error(
            source,
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
                source,
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
            filesystem_error(source, Some(artifact.path), "write partial artifact", error)
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
        filesystem_error(source, Some(artifact.path), "flush partial artifact", error)
    })?;
    file.sync_all().await.map_err(|error| {
        filesystem_error(source, Some(artifact.path), "sync partial artifact", error)
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
            source,
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

async fn write_complete_marker(
    source: &ArtifactSource,
    artifacts: &[ArtifactConfig],
    manifest: &Manifest,
) -> Result<(), ArtifactDownloadError> {
    let mut files = BTreeMap::new();
    for artifact in artifacts {
        let path = source.local_path(artifact)?;
        let metadata = async_fs::metadata(&path).await.map_err(|error| {
            filesystem_error(
                source,
                Some(artifact.path),
                "inspect completed artifact",
                error,
            )
        })?;
        if !metadata.is_file() {
            return Err(filesystem_error(
                source,
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

async fn write_complete_marker_best_effort(
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

fn safe_local_path(base: &Path, relative: &Path) -> Result<PathBuf, ArtifactDownloadError> {
    validate_relative_path(relative)?;
    Ok(base.join(relative))
}

fn validate_relative_path(path: &Path) -> Result<(), ArtifactDownloadError> {
    let normalized = path.to_str().map(|value| value.replace('\\', "/"));
    if normalized.as_deref().is_none_or(|value| {
        value.is_empty()
            || value.contains('\0')
            || value.starts_with('/')
            || value
                .split('/')
                .any(|segment| segment.is_empty() || matches!(segment, "." | ".."))
    }) || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(ArtifactDownloadError::new(
            FailureKind::InvalidInput,
            format!("invalid relative artifact path '{}'", path.display()),
        ));
    }
    Ok(())
}

fn filesystem_error(
    source: &ArtifactSource,
    artifact: Option<&str>,
    operation: &str,
    error: impl std::fmt::Display,
) -> ArtifactDownloadError {
    let error = ArtifactDownloadError::new(
        FailureKind::Filesystem,
        format!("unable to {operation}: {error}"),
    )
    .at_source(source.kind);
    match artifact {
        Some(artifact) => error.at_artifact(artifact),
        None => error,
    }
}

fn file_stamp(metadata: &fs::Metadata) -> FileStamp {
    FileStamp {
        size: metadata.len(),
        modified_ns: system_time_ns(metadata.modified().ok()),
        changed: platform_file_identity(metadata),
    }
}

fn complete_file_stamp(metadata: &fs::Metadata) -> CompleteFileStamp {
    CompleteFileStamp {
        size: metadata.len(),
        mtime_ms: platform_mtime_ms(metadata),
        ctime_ms: platform_ctime_ms(metadata),
    }
}

fn complete_file_stamps_match(actual: &CompleteFileStamp, expected: &CompleteFileStamp) -> bool {
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
struct FileIdentity {
    stamp: FileStamp,
    is_file: bool,
    is_symlink: bool,
}

fn inspect_file_identity(path: &Path) -> io::Result<Option<FileIdentity>> {
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

struct CleanupDirectory {
    path: PathBuf,
}

impl CleanupDirectory {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl Drop for CleanupDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

struct CacheLock {
    lock_path: PathBuf,
    owner_path: PathBuf,
    last_heartbeat: std::sync::Mutex<SystemTime>,
}

#[derive(Deserialize, Serialize)]
struct LockOwner {
    token: String,
    pid: u32,
    hostname: String,
}

impl CacheLock {
    async fn acquire(
        lock_path: &Path,
        signal: Option<&CancellationToken>,
    ) -> Result<Self, ArtifactDownloadError> {
        loop {
            check_cancelled(signal)?;
            match Self::try_acquire(lock_path) {
                Ok(Some(lock)) => return Ok(lock),
                Ok(None) => {}
                Err(error) => {
                    return Err(ArtifactDownloadError::new(
                        FailureKind::Filesystem,
                        format!("unable to acquire model cache lock: {error}"),
                    ));
                }
            }
            if remove_stale_lock(lock_path).map_err(|error| {
                ArtifactDownloadError::new(
                    FailureKind::Filesystem,
                    format!("unable to recover model cache lock: {error}"),
                )
            })? {
                continue;
            }
            wait_or_cancel(signal, tokio::time::sleep(LOCK_POLL_INTERVAL)).await?;
        }
    }

    fn try_acquire(lock_path: &Path) -> io::Result<Option<Self>> {
        if fs::symlink_metadata(lock_path).is_ok() {
            return Ok(None);
        }
        let token = Uuid::new_v4().to_string();
        let parent = lock_path.parent().unwrap_or_else(|| Path::new("."));
        let name = lock_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("artifact.lock");
        let staging = parent.join(format!(".{name}.pending-{}-{token}", std::process::id()));
        fs::create_dir(&staging)?;
        let _staging_cleanup = CleanupDirectory::new(staging.clone());
        let owner_name = format!(".owner-{token}");
        let staging_owner = staging.join(&owner_name);
        let owner = LockOwner {
            token: token.clone(),
            pid: std::process::id(),
            hostname: System::host_name().unwrap_or_default(),
        };
        let mut owner_json = serde_json::to_vec(&owner).map_err(io::Error::other)?;
        owner_json.push(b'\n');
        fs::write(&staging_owner, owner_json)?;
        match fs::rename(&staging, lock_path) {
            Ok(()) => {}
            Err(error) if is_directory_conflict(&error, lock_path) => {
                return Ok(None);
            }
            Err(error) => return Err(error),
        }
        let lock = Self {
            lock_path: lock_path.to_path_buf(),
            owner_path: lock_path.join(owner_name),
            last_heartbeat: std::sync::Mutex::new(UNIX_EPOCH),
        };
        lock.finish_acquire()
    }

    fn finish_acquire(self) -> io::Result<Option<Self>> {
        match self.refresh_owner() {
            Ok(()) => Ok(Some(self)),
            // A stale-lock contender can displace us after rename succeeds but
            // before the first heartbeat. Retry instead of returning a lock we
            // no longer own or failing the entire model load.
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn touch(&self) -> Result<(), ArtifactDownloadError> {
        let mut last = self
            .last_heartbeat
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if SystemTime::now().duration_since(*last).unwrap_or_default() < LOCK_HEARTBEAT_INTERVAL {
            return Ok(());
        }
        touch_owner(&self.owner_path).map_err(|error| {
            ArtifactDownloadError::new(
                FailureKind::Filesystem,
                format!("model cache lock ownership was lost: {error}"),
            )
        })?;
        *last = SystemTime::now();
        Ok(())
    }

    fn assert_owned(&self) -> Result<(), ArtifactDownloadError> {
        self.refresh_owner().map_err(|error| {
            ArtifactDownloadError::new(
                FailureKind::Filesystem,
                format!("model cache lock ownership was lost: {error}"),
            )
        })
    }

    fn refresh_owner(&self) -> io::Result<()> {
        touch_owner(&self.owner_path)?;
        *self
            .last_heartbeat
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SystemTime::now();
        Ok(())
    }
}

impl Drop for CacheLock {
    fn drop(&mut self) {
        if self.owner_path.exists() {
            let _ = fs::remove_file(&self.owner_path);
            let _ = fs::remove_dir(&self.lock_path);
        }
    }
}

fn touch_owner(path: &Path) -> io::Result<()> {
    let file = OpenOptions::new().write(true).open(path)?;
    file.set_times(FileTimes::new().set_modified(SystemTime::now()))
}

fn remove_stale_lock(lock_path: &Path) -> io::Result<bool> {
    remove_stale_lock_at(lock_path, SystemTime::now(), LOCK_STALE_AFTER)
}

fn remove_stale_lock_at(
    lock_path: &Path,
    now: SystemTime,
    stale_after: Duration,
) -> io::Result<bool> {
    let Some(observed) = inspect_lock(lock_path)? else {
        return Ok(true);
    };
    if !lock_is_abandoned(&observed, now, stale_after) {
        return Ok(false);
    }
    let stale =
        lock_path.with_extension(format!("stale-{}-{}", std::process::id(), Uuid::new_v4()));
    match fs::rename(lock_path, &stale) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error),
    }
    let Some(moved) = inspect_lock(&stale)? else {
        return Ok(true);
    };
    if !lock_is_abandoned(&moved, now, stale_after) {
        match fs::rename(&stale, lock_path) {
            Ok(()) => {}
            Err(error) if is_directory_conflict(&error, lock_path) => {
                // A successor owns lock_path. Leave the newly refreshed displaced
                // owner intact rather than deleting another process's lease.
            }
            Err(error) => return Err(error),
        }
        return Ok(false);
    }
    match fs::remove_dir_all(stale) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error),
    }
}

struct LockObservation {
    newest_heartbeat: SystemTime,
    dead_owner: bool,
}

fn lock_is_abandoned(
    observation: &LockObservation,
    now: SystemTime,
    stale_after: Duration,
) -> bool {
    observation.dead_owner
        || now
            .duration_since(observation.newest_heartbeat)
            .unwrap_or_default()
            >= stale_after
}

fn inspect_lock(path: &Path) -> io::Result<Option<LockObservation>> {
    let Some(metadata) = lock_component(fs::metadata(path))? else {
        return Ok(None);
    };
    inspect_existing_lock(path, &metadata)
}

fn inspect_existing_lock(
    path: &Path,
    metadata: &fs::Metadata,
) -> io::Result<Option<LockObservation>> {
    let mut newest = metadata.modified().unwrap_or(UNIX_EPOCH);
    let mut owners = Vec::new();
    let Some(entries) = lock_component(fs::read_dir(path))? else {
        return Ok(None);
    };
    for entry in entries {
        let Some(entry) = lock_component(entry)? else {
            return Ok(None);
        };
        let Some(file_type) = lock_component(entry.file_type())? else {
            return Ok(None);
        };
        if file_type.is_file() && entry.file_name().to_string_lossy().starts_with(".owner-") {
            // Re-read metadata through the path instead of relying on
            // `DirEntry`'s platform-specific cache. In particular, Windows
            // may keep returning cached metadata after the owner file has
            // already been removed by the lock holder.
            let Some(metadata) = lock_entry_metadata(&entry)? else {
                return Ok(None);
            };
            newest = newest.max(metadata.modified().unwrap_or(UNIX_EPOCH));
            owners.push(entry.path());
        }
    }
    let dead_owner = owners.len() == 1 && is_known_dead_owner(&owners[0]);
    Ok(Some(LockObservation {
        newest_heartbeat: newest,
        dead_owner,
    }))
}

fn lock_component<T>(result: io::Result<T>) -> io::Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn is_directory_conflict(error: &io::Error, path: &Path) -> bool {
    match error.kind() {
        // These errors already prove that rename observed a competing target.
        // The target may be released before a follow-up metadata lookup.
        io::ErrorKind::AlreadyExists | io::ErrorKind::DirectoryNotEmpty => true,
        // Windows may report PermissionDenied when the target is an existing
        // directory. Preserve genuine permission failures when it is not.
        io::ErrorKind::PermissionDenied => {
            fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir())
        }
        _ => false,
    }
}

fn lock_entry_metadata(entry: &fs::DirEntry) -> io::Result<Option<fs::Metadata>> {
    lock_component(fs::metadata(entry.path()))
}

fn is_known_dead_owner(owner_path: &Path) -> bool {
    let Ok(bytes) = fs::read(owner_path) else {
        return false;
    };
    let Ok(owner) = serde_json::from_slice::<LockOwner>(&bytes) else {
        return false;
    };
    let Some(owner_name) = owner_path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let Some(hostname) = System::host_name() else {
        return false;
    };
    if owner.hostname.is_empty()
        || owner.hostname != hostname
        || owner.token.is_empty()
        || owner_name != format!(".owner-{}", owner.token)
        || owner.pid == 0
        || owner.pid > i32::MAX as u32
    {
        return false;
    }
    let mut system = System::new();
    let pid = Pid::from_u32(owner.pid);
    system.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    system.process(pid).is_none()
}

#[cfg(test)]
mod tests {
    use std::{
        io::{BufRead, BufReader, Write},
        net::{TcpListener, TcpStream},
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        thread,
    };

    use super::*;
    use crate::domain::model::ModelProgress;

    const BYTES: &[u8] = b"verified model artifact";
    const ARTIFACTS: &[ArtifactConfig] = &[ArtifactConfig {
        path: "onnx/model q4.onnx",
        size: 23,
        sha256: "7d5fb89e0bde2d0860867ba417c5d6809f6a38ba911715f5e5e3258c9d856813",
    }];
    const HF: ArtifactSourceConfig = ArtifactSourceConfig {
        repo: "owner/model",
        revision: "hf-revision",
    };
    const MS: ArtifactSourceConfig = ArtifactSourceConfig {
        repo: "iic/model",
        revision: "ms-revision",
    };

    fn sources(root: &Path, base_url: Option<&str>) -> [ArtifactSource; 2] {
        let hugging_face = ArtifactSource::hugging_face(HF, root.join("huggingface"));
        let model_scope = ArtifactSource::model_scope(MS, root.join("modelscope"));
        match base_url {
            Some(base_url) => [
                hugging_face.with_base_url(base_url),
                model_scope.with_base_url(base_url),
            ],
            None => [hugging_face, model_scope],
        }
    }

    fn reporter(events: &Arc<Mutex<Vec<ModelProgress>>>) -> ModelDownloadProgressReporter {
        let captured = Arc::clone(events);
        ModelDownloadProgressReporter::new(
            "local/test-model",
            Some(Arc::new(move |event| {
                captured.lock().expect("progress lock").push(event);
            })),
            ARTIFACTS.iter().map(|artifact| artifact.path.to_owned()),
        )
    }

    async fn resolve(
        root: &Path,
        base_url: Option<&str>,
        reporter: &ModelDownloadProgressReporter,
    ) -> Result<ResolvedArtifacts, ModelError> {
        resolve_model_artifacts(
            &reqwest::Client::new(),
            ResolveArtifacts {
                model: "local/test-model",
                sources: sources(root, base_url),
                artifacts: ARTIFACTS,
                reporter,
                signal: None,
            },
        )
        .await
    }

    struct TestServer {
        base_url: String,
        requests: Arc<AtomicUsize>,
        join: Option<thread::JoinHandle<()>>,
    }

    impl TestServer {
        fn spawn(
            expected_requests: usize,
            handler: impl Fn(usize, String, TcpStream) + Send + Sync + 'static,
        ) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
            let address = listener.local_addr().expect("test server address");
            let requests = Arc::new(AtomicUsize::new(0));
            let counted = Arc::clone(&requests);
            let handler = Arc::new(handler);
            let join = thread::spawn(move || {
                let mut workers = Vec::new();
                for index in 0..expected_requests {
                    let (stream, _) = listener.accept().expect("accept test request");
                    counted.fetch_add(1, Ordering::SeqCst);
                    let handler = Arc::clone(&handler);
                    workers.push(thread::spawn(move || {
                        let mut line = String::new();
                        BufReader::new(stream.try_clone().expect("clone request stream"))
                            .read_line(&mut line)
                            .expect("read request line");
                        handler(index, line, stream);
                    }));
                }
                for worker in workers {
                    worker.join().expect("request worker");
                }
            });
            Self {
                base_url: format!("http://{address}"),
                requests,
                join: Some(join),
            }
        }

        fn finish(mut self) -> usize {
            self.join
                .take()
                .expect("server join")
                .join()
                .expect("server");
            self.requests.load(Ordering::SeqCst)
        }
    }

    fn respond(mut stream: TcpStream, status: u16, body: &[u8]) {
        let reason = if status == 200 { "OK" } else { "Unavailable" };
        write!(
            stream,
            "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .expect("write response headers");
        stream.write_all(body).expect("write response body");
    }

    fn partial_files(path: &Path) -> Vec<PathBuf> {
        let Ok(entries) = fs::read_dir(path) else {
            return Vec::new();
        };
        entries
            .filter_map(Result::ok)
            .flat_map(|entry| {
                let path = entry.path();
                if path.is_dir() {
                    partial_files(&path)
                } else if path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().contains(".part-"))
                {
                    vec![path]
                } else {
                    Vec::new()
                }
            })
            .collect()
    }

    #[test]
    fn encodes_source_urls_like_node() {
        let source = ArtifactSource::hugging_face(
            ArtifactSourceConfig {
                repo: "owner/model's name",
                revision: "release/(one)!",
            },
            PathBuf::from("cache"),
        );
        assert_eq!(
            artifact_url(&source, "onnx/model q4.onnx"),
            "https://huggingface.co/owner/model's%20name/resolve/release%2F(one)!/onnx/model%20q4.onnx"
        );
    }

    #[test]
    fn fingerprints_manifests_like_node() {
        let source = ArtifactSource::hugging_face(HF, PathBuf::from("cache"));
        let manifest =
            Manifest::new("local/test-model", &source, ARTIFACTS).expect("artifact manifest");

        assert_eq!(manifest.fingerprint, "2b94620c9ab4a8539f10bf74");
    }

    #[tokio::test]
    async fn accepts_node_shaped_completion_marker() {
        let root = tempfile::tempdir().expect("cache root");
        let source = ArtifactSource::hugging_face(HF, root.path().to_owned());
        let destination = source
            .local_path(&ARTIFACTS[0])
            .expect("artifact destination");
        async_fs::create_dir_all(destination.parent().expect("artifact parent"))
            .await
            .expect("create cache");
        async_fs::write(&destination, BYTES)
            .await
            .expect("write artifact");
        let manifest =
            Manifest::new("local/test-model", &source, ARTIFACTS).expect("artifact manifest");
        let metadata = fs::metadata(&destination).expect("artifact metadata");
        let stamp = complete_file_stamp(&metadata);
        let marker = format!(
            r#"{{"version":1,"fingerprint":"{}","files":{{"{}":{{"size":{},"mtimeMs":{},"ctimeMs":{}}}}}}}"#,
            manifest.fingerprint, ARTIFACTS[0].path, stamp.size, stamp.mtime_ms, stamp.ctime_ms,
        );
        async_fs::write(&manifest.marker_path, marker)
            .await
            .expect("write Node marker");

        assert!(
            has_valid_complete_marker(&source, ARTIFACTS, &manifest)
                .await
                .expect("validate marker")
        );
    }

    #[test]
    fn completion_stamps_allow_only_json_round_trip_precision() {
        let timestamp = 1_790_048_381_253.380_1_f64;
        let stamp = CompleteFileStamp {
            size: 23,
            mtime_ms: timestamp,
            ctime_ms: timestamp,
        };
        let adjacent = CompleteFileStamp {
            size: 23,
            mtime_ms: f64::from_bits(timestamp.to_bits() - 1),
            ctime_ms: f64::from_bits(timestamp.to_bits() + 1),
        };
        assert!(complete_file_stamps_match(&stamp, &adjacent));

        let changed = CompleteFileStamp {
            size: 23,
            mtime_ms: f64::from_bits(timestamp.to_bits() - 2),
            ctime_ms: timestamp,
        };
        assert!(!complete_file_stamps_match(&stamp, &changed));
        assert!(!complete_file_stamps_match(
            &stamp,
            &CompleteFileStamp {
                size: 24,
                ..stamp.clone()
            }
        ));
    }

    #[tokio::test]
    async fn checks_modelscope_cache_before_networking_and_repairs_marker() {
        let root = tempfile::tempdir().expect("cache root");
        let destination = root.path().join("modelscope").join(ARTIFACTS[0].path);
        async_fs::create_dir_all(destination.parent().expect("artifact parent"))
            .await
            .expect("create cache");
        async_fs::write(&destination, BYTES)
            .await
            .expect("write cache");
        let events = Arc::new(Mutex::new(Vec::new()));
        let result = resolve(root.path(), None, &reporter(&events))
            .await
            .expect("offline ModelScope cache");
        assert_eq!(result.paths[ARTIFACTS[0].path], destination);
        assert!(
            fs::read_dir(root.path().join("modelscope"))
                .expect("cache entries")
                .any(|entry| entry
                    .expect("cache entry")
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".complete"))
        );
    }

    #[tokio::test]
    async fn replaces_same_size_corruption_with_verified_artifact() {
        let root = tempfile::tempdir().expect("cache root");
        let destination = root.path().join("huggingface").join(ARTIFACTS[0].path);
        async_fs::create_dir_all(destination.parent().expect("artifact parent"))
            .await
            .expect("create cache");
        async_fs::write(&destination, vec![b'x'; BYTES.len()])
            .await
            .expect("corrupt cache");
        let server = TestServer::spawn(1, |_index, request, stream| {
            assert!(request.contains("/owner/model/resolve/hf-revision/"));
            respond(stream, 200, BYTES);
        });
        let events = Arc::new(Mutex::new(Vec::new()));
        resolve(root.path(), Some(&server.base_url), &reporter(&events))
            .await
            .expect("repair cache");
        assert_eq!(server.finish(), 1);
        assert_eq!(async_fs::read(&destination).await.expect("artifact"), BYTES);
        assert!(events.lock().expect("events").iter().any(|event| matches!(
            event,
            ModelProgress::Downloading {
                downloaded_bytes: Some(0),
                total_bytes: Some(23),
                ..
            }
        )));
    }

    #[tokio::test]
    async fn falls_back_once_after_integrity_failure_and_preserves_progress_warning() {
        let root = tempfile::tempdir().expect("cache root");
        let server = TestServer::spawn(2, |_index, request, stream| {
            if request.contains("/owner/model/") {
                respond(stream, 200, &vec![b'x'; BYTES.len()]);
            } else {
                assert!(request.contains("/iic/model/resolve/ms-revision/"));
                respond(stream, 200, BYTES);
            }
        });
        let events = Arc::new(Mutex::new(Vec::new()));
        let result = resolve(root.path(), Some(&server.base_url), &reporter(&events))
            .await
            .expect("ModelScope fallback");
        assert_eq!(server.finish(), 2);
        assert!(result.paths[ARTIFACTS[0].path].starts_with(root.path().join("modelscope")));
        assert_eq!(
            events
                .lock()
                .expect("events")
                .iter()
                .filter(|event| matches!(event, ModelProgress::Warning { .. }))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn unavailable_hugging_face_falls_back_to_modelscope() {
        let root = tempfile::tempdir().expect("cache root");
        let server = TestServer::spawn(2, |_index, request, stream| {
            if request.contains("/owner/model/") {
                respond(stream, 503, b"");
            } else {
                assert!(request.contains("/iic/model/resolve/ms-revision/"));
                respond(stream, 200, BYTES);
            }
        });
        let events = Arc::new(Mutex::new(Vec::new()));
        let result = resolve(root.path(), Some(&server.base_url), &reporter(&events))
            .await
            .expect("ModelScope fallback");

        assert_eq!(server.finish(), 2);
        assert!(result.paths[ARTIFACTS[0].path].starts_with(root.path().join("modelscope")));
        assert_eq!(
            events
                .lock()
                .expect("events")
                .iter()
                .filter(|event| matches!(event, ModelProgress::Warning { .. }))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn interrupted_stream_falls_back_and_removes_partial_file() {
        let root = tempfile::tempdir().expect("cache root");
        let server = TestServer::spawn(2, |_index, request, mut stream| {
            if request.contains("/owner/model/") {
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    BYTES.len()
                )
                .expect("write response headers");
                stream.write_all(&BYTES[..4]).expect("write partial body");
            } else {
                respond(stream, 200, BYTES);
            }
        });
        let events = Arc::new(Mutex::new(Vec::new()));
        let resolved = resolve(root.path(), Some(&server.base_url), &reporter(&events))
            .await
            .expect("interrupted stream fallback");
        assert!(resolved.paths[ARTIFACTS[0].path].starts_with(root.path().join("modelscope")));
        assert_eq!(server.finish(), 2);
        assert!(partial_files(root.path()).is_empty());
    }

    #[tokio::test]
    async fn does_not_fallback_for_unauthorized_or_cancelled_requests() {
        let root = tempfile::tempdir().expect("cache root");
        let server = TestServer::spawn(1, |_index, _request, stream| respond(stream, 401, b""));
        let events = Arc::new(Mutex::new(Vec::new()));
        let error = resolve(root.path(), Some(&server.base_url), &reporter(&events))
            .await
            .expect_err("401 must fail");
        assert!(error.to_string().contains("401"));
        assert_eq!(server.finish(), 1);

        let signal = CancellationToken::new();
        signal.cancel();
        let error = resolve_model_artifacts(
            &reqwest::Client::new(),
            ResolveArtifacts {
                model: "local/test-model",
                sources: sources(root.path(), None),
                artifacts: ARTIFACTS,
                reporter: &reporter(&events),
                signal: Some(&signal),
            },
        )
        .await
        .expect_err("cancelled resolution");
        assert_eq!(error.code(), crate::EngineError::CANCELLED);
    }

    #[tokio::test]
    async fn modelscope_fallback_preserves_cancellation_code() {
        let root = tempfile::tempdir().expect("cache root");
        let server = TestServer::spawn(1, |_index, request, stream| {
            assert!(request.contains("/owner/model/"));
            respond(stream, 503, b"");
        });
        let signal = CancellationToken::new();
        let cancel_on_fallback = signal.clone();
        let reporter = ModelDownloadProgressReporter::new(
            "local/test-model",
            Some(Arc::new(move |event| {
                if matches!(event, ModelProgress::Warning { .. }) {
                    cancel_on_fallback.cancel();
                }
            })),
            ARTIFACTS.iter().map(|artifact| artifact.path.to_owned()),
        );
        let error = resolve_model_artifacts(
            &reqwest::Client::new(),
            ResolveArtifacts {
                model: "local/test-model",
                sources: sources(root.path(), Some(&server.base_url)),
                artifacts: ARTIFACTS,
                reporter: &reporter,
                signal: Some(&signal),
            },
        )
        .await
        .expect_err("ModelScope cancellation must be preserved");

        assert_eq!(error.code(), crate::EngineError::CANCELLED);
        assert_eq!(server.finish(), 1);
    }

    #[tokio::test]
    async fn response_header_timeout_falls_back_without_total_download_deadline() {
        let root = tempfile::tempdir().expect("cache root");
        let server = TestServer::spawn(2, |_index, request, stream| {
            if request.contains("/owner/model/") {
                thread::sleep(Duration::from_millis(75));
                let _ = stream.shutdown(std::net::Shutdown::Both);
            } else {
                respond(stream, 200, BYTES);
            }
        });
        let short = Duration::from_millis(20);
        let sources = sources(root.path(), Some(&server.base_url))
            .map(|source| source.with_timeouts(short, Duration::from_millis(50)));
        let events = Arc::new(Mutex::new(Vec::new()));
        let resolved = resolve_model_artifacts(
            &reqwest::Client::new(),
            ResolveArtifacts {
                model: "local/test-model",
                sources,
                artifacts: ARTIFACTS,
                reporter: &reporter(&events),
                signal: None,
            },
        )
        .await
        .expect("timeout fallback");
        assert!(resolved.paths[ARTIFACTS[0].path].starts_with(root.path().join("modelscope")));
        assert_eq!(server.finish(), 2);
    }

    #[tokio::test]
    async fn read_idle_timeout_falls_back_and_preserves_both_source_errors() {
        let root = tempfile::tempdir().expect("cache root");
        let server = TestServer::spawn(2, |_index, request, mut stream| {
            if request.contains("/owner/model/") {
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    BYTES.len()
                )
                .expect("write response headers");
                stream.flush().expect("flush response headers");
                thread::sleep(Duration::from_millis(75));
            } else {
                respond(stream, 500, b"");
            }
        });
        let short = Duration::from_millis(20);
        let sources = sources(root.path(), Some(&server.base_url))
            .map(|source| source.with_timeouts(Duration::from_millis(50), short));
        let events = Arc::new(Mutex::new(Vec::new()));
        let error = resolve_model_artifacts(
            &reqwest::Client::new(),
            ResolveArtifacts {
                model: "local/test-model",
                sources,
                artifacts: ARTIFACTS,
                reporter: &reporter(&events),
                signal: None,
            },
        )
        .await
        .expect_err("both sources must fail");
        let message = error.to_string();
        assert!(message.contains("Hugging Face"), "{message}");
        assert!(message.contains("ModelScope"), "{message}");
        assert!(message.contains("timed out"), "{message}");
        assert!(message.contains("500"), "{message}");
        assert_eq!(server.finish(), 2);
    }

    #[tokio::test]
    async fn concurrent_resolvers_share_one_download() {
        let root = tempfile::tempdir().expect("cache root");
        let server = TestServer::spawn(1, |_index, _request, stream| {
            thread::sleep(Duration::from_millis(40));
            respond(stream, 200, BYTES);
        });
        let first_events = Arc::new(Mutex::new(Vec::new()));
        let second_events = Arc::new(Mutex::new(Vec::new()));
        let first_reporter = reporter(&first_events);
        let second_reporter = reporter(&second_events);
        let (first, second) = tokio::join!(
            resolve(root.path(), Some(&server.base_url), &first_reporter),
            resolve(root.path(), Some(&server.base_url), &second_reporter),
        );
        first.expect("first resolver");
        second.expect("second resolver");
        assert_eq!(server.finish(), 1);
    }

    #[tokio::test]
    async fn completion_marker_does_not_hide_same_size_mutation() {
        let root = tempfile::tempdir().expect("cache root");
        let server = TestServer::spawn(2, |_index, _request, stream| respond(stream, 200, BYTES));
        let events = Arc::new(Mutex::new(Vec::new()));
        let reporter = reporter(&events);
        let first = resolve(root.path(), Some(&server.base_url), &reporter)
            .await
            .expect("first download");
        let path = &first.paths[ARTIFACTS[0].path];
        thread::sleep(Duration::from_millis(5));
        async_fs::write(path, vec![b'x'; BYTES.len()])
            .await
            .expect("mutate cache");
        resolve(root.path(), Some(&server.base_url), &reporter)
            .await
            .expect("repair mutation");
        assert_eq!(server.finish(), 2);
        assert_eq!(async_fs::read(path).await.expect("repaired"), BYTES);
    }

    #[test]
    fn stale_locks_are_recovered_without_allowing_old_owner_cleanup() {
        let root = tempfile::tempdir().expect("lock root");
        let lock_path = root.path().join("artifact.lock");
        let old = CacheLock::try_acquire(&lock_path)
            .expect("acquire old lock")
            .expect("old lock");
        let displaced = root.path().join("displaced.lock");
        fs::rename(&lock_path, &displaced).expect("simulate stale takeover");
        let successor = CacheLock::try_acquire(&lock_path)
            .expect("acquire successor")
            .expect("successor lock");
        drop(old);
        assert!(successor.owner_path.is_file());
        drop(successor);

        fs::create_dir(&lock_path).expect("create stale lock");
        fs::write(lock_path.join(".owner-abandoned"), b"{}").expect("write stale owner");
        let future = SystemTime::now() + LOCK_STALE_AFTER + Duration::from_secs(1);
        assert!(
            remove_stale_lock_at(&lock_path, future, LOCK_STALE_AFTER).expect("recover stale lock")
        );
        assert!(!lock_path.exists());

        fs::create_dir(&lock_path).expect("create dead-owner lock");
        let owner = LockOwner {
            token: "dead".to_owned(),
            pid: i32::MAX as u32,
            hostname: System::host_name().unwrap_or_default(),
        };
        fs::write(
            lock_path.join(".owner-dead"),
            serde_json::to_vec(&owner).expect("serialize dead owner"),
        )
        .expect("write dead owner");
        if !owner.hostname.is_empty() {
            assert!(
                remove_stale_lock_at(&lock_path, SystemTime::now(), LOCK_STALE_AFTER)
                    .expect("recover dead owner")
            );
        }
    }

    #[test]
    fn disappearing_lock_components_request_an_acquire_retry() {
        let root = tempfile::tempdir().expect("lock root");
        let lock_path = root.path().join("artifact.lock");
        fs::create_dir(&lock_path).expect("create lock");
        let metadata = fs::metadata(&lock_path).expect("lock metadata");
        fs::remove_dir(&lock_path).expect("release lock");
        assert!(
            inspect_existing_lock(&lock_path, &metadata)
                .expect("disappearing lock is not an error")
                .is_none()
        );

        fs::create_dir(&lock_path).expect("recreate lock");
        let owner_path = lock_path.join(".owner-racing");
        fs::write(&owner_path, b"{}").expect("write owner");
        let owner = fs::read_dir(&lock_path)
            .expect("read lock")
            .next()
            .expect("owner entry")
            .expect("read owner entry");
        fs::remove_file(&owner_path).expect("release owner");
        assert!(
            lock_entry_metadata(&owner)
                .expect("disappearing owner is not an error")
                .is_none()
        );
    }

    #[test]
    fn directory_conflicts_survive_release_without_hiding_permission_errors() {
        let root = tempfile::tempdir().expect("lock root");
        let lock_path = root.path().join("artifact.lock");
        let error = |kind| io::Error::new(kind, "rename fixture");

        // The competing directory may disappear after rename reports the
        // conflict, so these error kinds must not depend on a second lookup.
        assert!(is_directory_conflict(
            &error(io::ErrorKind::AlreadyExists),
            &lock_path
        ));
        assert!(is_directory_conflict(
            &error(io::ErrorKind::DirectoryNotEmpty),
            &lock_path
        ));

        // PermissionDenied is Windows' ambiguous spelling: it is contention
        // only while the target is an actual directory.
        assert!(!is_directory_conflict(
            &error(io::ErrorKind::PermissionDenied),
            &lock_path
        ));
        fs::create_dir(&lock_path).expect("competing lock directory");
        assert!(is_directory_conflict(
            &error(io::ErrorKind::PermissionDenied),
            &lock_path
        ));
        fs::remove_dir(&lock_path).expect("remove competing lock directory");
        fs::write(&lock_path, b"not a lock directory").expect("unrelated file");
        assert!(!is_directory_conflict(
            &error(io::ErrorKind::PermissionDenied),
            &lock_path
        ));
        assert!(!is_directory_conflict(
            &error(io::ErrorKind::Other),
            &lock_path
        ));
    }

    #[test]
    fn initial_owner_disappearance_requests_an_acquire_retry() {
        let root = tempfile::tempdir().expect("lock root");
        let lock_path = root.path().join("artifact.lock");
        let owner_path = lock_path.join(".owner-displaced");
        let lock = CacheLock {
            lock_path,
            owner_path,
            last_heartbeat: std::sync::Mutex::new(UNIX_EPOCH),
        };

        assert!(
            lock.finish_acquire()
                .expect("missing initial owner is not an error")
                .is_none()
        );
    }
}
