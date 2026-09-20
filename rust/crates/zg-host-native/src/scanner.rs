use std::{
    collections::HashSet,
    fs::{self, File, Metadata},
    io::Read,
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::{Instant, UNIX_EPOCH},
};

use async_trait::async_trait;
use same_file::{Handle, is_same_file};
use tokio::sync::Semaphore;

use crate::{
    HostError,
    api::{
        DiscoveredFile, ReadBatchRequest, RootSpec, ScanDiagnostics, ScanRequest, ScanSnapshot,
        SkippedFile, SkippedFileReason, SourceFile, TaskControl, WorkspaceScannerPort,
    },
};

const MAX_SKIPPED_FILE_SAMPLES: usize = 20;

#[derive(Clone, Debug)]
pub struct NativeScanner {
    scan_slots: Arc<Semaphore>,
}

impl NativeScanner {
    #[must_use]
    pub fn new() -> Self {
        Self {
            scan_slots: Arc::new(Semaphore::new(1)),
        }
    }

    #[must_use]
    pub fn with_max_concurrent_scans(mut self, maximum: usize) -> Self {
        self.scan_slots = Arc::new(Semaphore::new(maximum.max(1)));
        self
    }
}

impl Default for NativeScanner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl WorkspaceScannerPort for NativeScanner {
    async fn discover(
        &self,
        request: &ScanRequest,
        control: &TaskControl,
    ) -> Result<ScanSnapshot, HostError> {
        let _permit = acquire_slot(&self.scan_slots, control).await?;
        let request = request.clone();
        run_blocking(control, move |blocking_control| {
            discover_sync(&request, &blocking_control)
        })
        .await
    }

    async fn read_batch(
        &self,
        request: &ReadBatchRequest,
        control: &TaskControl,
    ) -> Result<Vec<SourceFile>, HostError> {
        let _permit = acquire_slot(&self.scan_slots, control).await?;
        let request = request.clone();
        run_blocking(control, move |blocking_control| {
            read_batch_sync(&request, &blocking_control)
        })
        .await
    }
}

#[derive(Clone, Debug)]
struct BlockingControl {
    cancellation: tokio_util::sync::CancellationToken,
    deadline: Option<Instant>,
}

impl BlockingControl {
    fn check(&self) -> Result<(), HostError> {
        if self.cancellation.is_cancelled() {
            return Err(HostError::cancelled(
                "native scanner operation was cancelled",
            ));
        }
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(HostError::deadline_exceeded(
                "native scanner operation exceeded its deadline",
            ));
        }
        Ok(())
    }
}

async fn acquire_slot<'a>(
    slots: &'a Semaphore,
    control: &TaskControl,
) -> Result<tokio::sync::SemaphorePermit<'a>, HostError> {
    control_check(control)?;
    tokio::select! {
        () = control.cancellation.cancelled() => {
            Err(HostError::cancelled("native scanner operation was cancelled"))
        },
        () = deadline_wait(control.deadline) => Err(HostError::deadline_exceeded(
            "native scanner operation exceeded its deadline",
        )),
        permit = slots.acquire() => permit.map_err(|_| {
            HostError::internal("native scanner concurrency limiter closed unexpectedly")
        }),
    }
}

async fn run_blocking<T, F>(control: &TaskControl, function: F) -> Result<T, HostError>
where
    T: Send + 'static,
    F: FnOnce(BlockingControl) -> Result<T, HostError> + Send + 'static,
{
    control_check(control)?;
    let blocking_control = BlockingControl {
        cancellation: control.cancellation.clone(),
        deadline: control.deadline,
    };
    let task = tokio::task::spawn_blocking(move || function(blocking_control));
    tokio::select! {
        () = control.cancellation.cancelled() => {
            Err(HostError::cancelled("native scanner operation was cancelled"))
        },
        () = deadline_wait(control.deadline) => Err(HostError::deadline_exceeded(
            "native scanner operation exceeded its deadline",
        )),
        result = task => result
            .map_err(|error| HostError::internal(format!("native scanner worker failed: {error}")))?,
    }
}

async fn deadline_wait(deadline: Option<Instant>) {
    if let Some(deadline) = deadline {
        tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
    } else {
        std::future::pending::<()>().await;
    }
}

fn control_check(control: &TaskControl) -> Result<(), HostError> {
    if control.cancellation.is_cancelled() {
        return Err(HostError::cancelled(
            "native scanner operation was cancelled",
        ));
    }
    if control
        .deadline
        .is_some_and(|deadline| Instant::now() >= deadline)
    {
        return Err(HostError::deadline_exceeded(
            "native scanner operation exceeded its deadline",
        ));
    }
    Ok(())
}

#[derive(Debug)]
struct ScanDomain {
    root: RootSpec,
    canonical_path: PathBuf,
    metadata: Metadata,
}

fn discover_sync(
    request: &ScanRequest,
    control: &BlockingControl,
) -> Result<ScanSnapshot, HostError> {
    control.check()?;
    let domains = validate_domains(&request.roots)?;
    let scope = ScanScope::new(&request.scope_paths)?;
    let mut files = Vec::new();
    let mut diagnostics = ScanDiagnostics::default();

    for domain in domains {
        control.check()?;
        if domain.metadata.is_file() {
            scan_root_file(&domain.root, &scope, &mut files, &mut diagnostics, control)?;
        } else if domain.metadata.is_dir() {
            scan_root_directory(&domain, &scope, &mut files, &mut diagnostics, control)?;
        }
    }
    files.sort_by(|left, right| {
        left.root
            .cmp(&right.root)
            .then(left.relative_path.cmp(&right.relative_path))
    });
    files.dedup_by(|left, right| {
        left.root == right.root && left.relative_path == right.relative_path
    });
    Ok(ScanSnapshot { files, diagnostics })
}

#[derive(Debug)]
struct ScanScope {
    paths: Vec<PathBuf>,
}

impl ScanScope {
    fn new(paths: &[PathBuf]) -> Result<Self, HostError> {
        let mut normalized = paths
            .iter()
            .map(|path| {
                if !path.is_absolute() {
                    return Err(HostError::invalid_argument(format!(
                        "scan scope must be an absolute path: {}",
                        path.display()
                    )));
                }
                Ok(path.clone())
            })
            .collect::<Result<Vec<_>, _>>()?;
        normalized.sort();
        normalized.dedup();
        let mut compacted: Vec<PathBuf> = Vec::with_capacity(normalized.len());
        for path in normalized {
            if compacted.iter().any(|parent| path.starts_with(parent)) {
                continue;
            }
            compacted.push(path);
        }
        Ok(Self { paths: compacted })
    }

    fn includes_file(&self, path: &Path) -> bool {
        self.paths.is_empty() || self.paths.iter().any(|scope| path.starts_with(scope))
    }

    fn intersects_directory(&self, path: &Path) -> bool {
        self.paths.is_empty()
            || self
                .paths
                .iter()
                .any(|scope| scope.starts_with(path) || path.starts_with(scope))
    }
}

fn validate_domains(roots: &[RootSpec]) -> Result<Vec<ScanDomain>, HostError> {
    let mut domains = Vec::with_capacity(roots.len());
    for root in roots {
        if !root.path.is_absolute() {
            return Err(HostError::invalid_argument(format!(
                "workspace root must be absolute: {}",
                root.path.display()
            )));
        }
        let metadata = fs::metadata(&root.path).map_err(|error| {
            HostError::invalid_argument(format!(
                "workspace root {} could not be inspected: {error}",
                root.path.display()
            ))
        })?;
        if !metadata.is_file() && !metadata.is_dir() {
            return Err(HostError::invalid_argument(format!(
                "workspace root {} must be a file or directory",
                root.path.display()
            )));
        }
        let canonical_path = fs::canonicalize(&root.path).map_err(|error| {
            HostError::invalid_argument(format!(
                "workspace root {} could not be resolved: {error}",
                root.path.display()
            ))
        })?;
        domains.push(ScanDomain {
            root: root.clone(),
            canonical_path,
            metadata,
        });
    }
    for left_index in 0..domains.len() {
        for right_index in (left_index + 1)..domains.len() {
            if domains_overlap(&domains[left_index], &domains[right_index])? {
                return Err(HostError::invalid_argument(format!(
                    "workspace roots overlap: left={} right={}",
                    domains[left_index].root.path.display(),
                    domains[right_index].root.path.display()
                )));
            }
        }
    }
    Ok(domains)
}

fn domains_overlap(left: &ScanDomain, right: &ScanDomain) -> Result<bool, HostError> {
    if is_same_file(&left.root.path, &right.root.path).map_err(|error| {
        HostError::storage_failure(
            "native-scanner",
            format!("root identity check failed: {error}"),
        )
    })? {
        return Ok(true);
    }
    let left_file = left.metadata.is_file();
    let right_file = right.metadata.is_file();
    if left_file && right_file {
        return Ok(false);
    }
    if !left_file && !right_file {
        return Ok(
            directory_covers_directory(left, right) || directory_covers_directory(right, left)
        );
    }
    let (directory, file) = if left_file {
        (right, left)
    } else {
        (left, right)
    };
    Ok(directory_covers_file(directory, &file.canonical_path))
}

fn directory_covers_directory(directory: &ScanDomain, child: &ScanDomain) -> bool {
    directory.root.recursive && child.canonical_path.starts_with(&directory.canonical_path)
}

fn directory_covers_file(directory: &ScanDomain, file: &Path) -> bool {
    file.starts_with(&directory.canonical_path)
        && (directory.root.recursive || file.parent() == Some(&directory.canonical_path))
}

fn scan_root_file(
    root: &RootSpec,
    scope: &ScanScope,
    files: &mut Vec<DiscoveredFile>,
    diagnostics: &mut ScanDiagnostics,
    control: &BlockingControl,
) -> Result<(), HostError> {
    if !scope.includes_file(&root.path) {
        return Ok(());
    }
    let Some(name) = root.path.file_name() else {
        return Ok(());
    };
    let relative_path = PathBuf::from(name);
    if !root.policy.includes_file(&root.path)? {
        return Ok(());
    }
    if let Some(file) = read_file_info(root, &root.path, &relative_path, diagnostics, control)? {
        files.push(file);
    }
    Ok(())
}

fn scan_root_directory(
    domain: &ScanDomain,
    scope: &ScanScope,
    files: &mut Vec<DiscoveredFile>,
    diagnostics: &mut ScanDiagnostics,
    control: &BlockingControl,
) -> Result<(), HostError> {
    if !scope.intersects_directory(&domain.root.path) {
        return Ok(());
    }
    if !domain.root.policy.can_descend(&domain.root.path)? {
        return Ok(());
    }
    let mut visited = HashSet::from([domain.canonical_path.clone()]);
    walk(
        &domain.root,
        scope,
        &domain.root.path,
        0,
        &mut visited,
        files,
        diagnostics,
        control,
    )
}

#[allow(clippy::too_many_arguments)]
fn walk(
    root: &RootSpec,
    scope: &ScanScope,
    current_path: &Path,
    depth: usize,
    visited: &mut HashSet<PathBuf>,
    files: &mut Vec<DiscoveredFile>,
    diagnostics: &mut ScanDiagnostics,
    control: &BlockingControl,
) -> Result<(), HostError> {
    control.check()?;
    let Ok(read_directory) = fs::read_dir(current_path) else {
        return Ok(());
    };
    let mut entries: Vec<_> = read_directory.filter_map(Result::ok).collect();
    entries.sort_by_key(fs::DirEntry::file_name);

    for entry in entries {
        control.check()?;
        let absolute_path = entry.path();
        let relative_path = absolute_path.strip_prefix(&root.path).map_err(|error| {
            HostError::internal(format!("scanner produced an out-of-root path: {error}"))
        })?;
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let (is_directory, is_file) = if file_type.is_symlink() {
            if !root.follow {
                continue;
            }
            fs::metadata(&absolute_path).map_or((false, false), |metadata| {
                (metadata.is_dir(), metadata.is_file())
            })
        } else {
            (file_type.is_dir(), file_type.is_file())
        };

        if is_directory {
            if !scope.intersects_directory(&absolute_path) {
                continue;
            }
            if !root.recursive
                || root.max_depth.is_some_and(|maximum| depth + 1 >= maximum)
                || !root.policy.can_descend(&absolute_path)?
            {
                continue;
            }
            let Ok(canonical) = fs::canonicalize(&absolute_path) else {
                continue;
            };
            if !visited.insert(canonical) {
                continue;
            }
            walk(
                root,
                scope,
                &absolute_path,
                depth + 1,
                visited,
                files,
                diagnostics,
                control,
            )?;
            continue;
        }
        if !scope.includes_file(&absolute_path) {
            continue;
        }
        if !is_file
            || root.max_depth.is_some_and(|maximum| depth + 1 > maximum)
            || !root.policy.includes_file(&absolute_path)?
        {
            continue;
        }
        if let Some(file) =
            read_file_info(root, &absolute_path, relative_path, diagnostics, control)?
        {
            files.push(file);
        }
    }
    Ok(())
}

fn read_file_info(
    root: &RootSpec,
    absolute_path: &Path,
    relative_path: &Path,
    diagnostics: &mut ScanDiagnostics,
    control: &BlockingControl,
) -> Result<Option<DiscoveredFile>, HostError> {
    control.check()?;
    let Ok(metadata) = fs::metadata(absolute_path) else {
        return Ok(None);
    };
    if !metadata.is_file() {
        return Ok(None);
    }
    if metadata.len() == 0 {
        record_skipped(
            diagnostics,
            absolute_path,
            SkippedFileReason::Empty,
            Some(0),
            None,
        );
        return Ok(None);
    }
    if let Some(maximum) = root.max_file_size_bytes
        && metadata.len() > maximum
    {
        record_skipped(
            diagnostics,
            absolute_path,
            SkippedFileReason::TooLarge,
            Some(metadata.len()),
            Some(maximum),
        );
        return Ok(None);
    }
    let modified_epoch_ms = modified_epoch_ms(&metadata);
    let source_fingerprint = source_fingerprint(metadata.len(), modified_epoch_ms);
    Ok(Some(DiscoveredFile {
        root: root.path.clone(),
        relative_path: relative_path.to_path_buf(),
        size_bytes: metadata.len(),
        modified_epoch_ms,
        source_fingerprint,
    }))
}

fn read_batch_sync(
    request: &ReadBatchRequest,
    control: &BlockingControl,
) -> Result<Vec<SourceFile>, HostError> {
    let mut sources = Vec::with_capacity(request.files.len());
    for file in &request.files {
        control.check()?;
        validate_relative_path(&file.relative_path)?;
        let absolute_path = if fs::metadata(&file.root).is_ok_and(|metadata| metadata.is_file()) {
            file.root.clone()
        } else {
            file.root.join(&file.relative_path)
        };
        let handle = File::open(&absolute_path).map_err(|error| {
            HostError::storage_failure(
                "native-scanner",
                format!("could not open source {}: {error}", absolute_path.display()),
            )
        })?;
        let (bytes, source_fingerprint) = read_source_snapshot(&absolute_path, handle)?;
        control.check()?;
        sources.push(SourceFile {
            root: file.root.clone(),
            relative_path: file.relative_path.clone(),
            bytes,
            source_fingerprint,
        });
    }
    Ok(sources)
}

fn read_source_snapshot(path: &Path, mut file: File) -> Result<(Vec<u8>, String), HostError> {
    let inspect = |file: &File| {
        file.metadata().map_err(|error| {
            HostError::storage_failure(
                "native-scanner",
                format!("could not inspect source {}: {error}", path.display()),
            )
        })
    };
    let before = inspect(&file)?;
    if !before.is_file() {
        return Err(HostError::invalid_argument(format!(
            "source must be a regular file: {}",
            path.display()
        )));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(|error| {
        HostError::storage_failure(
            "native-scanner",
            format!("could not read source {}: {error}", path.display()),
        )
    })?;
    let after = inspect(&file)?;
    let actual_size = u64::try_from(bytes.len())
        .map_err(|_| HostError::internal("source byte length exceeds u64"))?;
    validate_source_snapshot(path, &before, &after, actual_size)?;

    let same_file = Handle::from_file(file)
        .and_then(|opened| Handle::from_path(path).map(|current| opened == current))
        .map_err(|error| {
            HostError::storage_failure(
                "native-scanner",
                format!(
                    "could not verify source identity {}: {error}",
                    path.display()
                ),
            )
        })?;
    if !same_file {
        return Err(HostError::storage_failure(
            "native-scanner",
            format!("source was replaced while reading: {}", path.display()),
        ));
    }
    Ok((
        bytes,
        source_fingerprint(after.len(), modified_epoch_ms(&after)),
    ))
}

fn validate_source_snapshot(
    path: &Path,
    before: &Metadata,
    after: &Metadata,
    actual_size: u64,
) -> Result<(), HostError> {
    // Compare the full timestamp; the public fingerprint intentionally uses milliseconds.
    if before.len() != after.len()
        || before.modified().ok() != after.modified().ok()
        || actual_size != after.len()
    {
        return Err(HostError::storage_failure(
            "native-scanner",
            format!("source changed while reading: {}", path.display()),
        ));
    }
    Ok(())
}

fn validate_relative_path(path: &Path) -> Result<(), HostError> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(HostError::invalid_argument(format!(
            "source path must be a non-empty relative path: {}",
            path.display()
        )));
    }
    Ok(())
}

fn modified_epoch_ms(metadata: &Metadata) -> Option<u64> {
    metadata
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
}

fn source_fingerprint(size_bytes: u64, modified_epoch_ms: Option<u64>) -> String {
    modified_epoch_ms.map_or_else(
        || format!("metadata-v1:{size_bytes}:unknown"),
        |modified| format!("metadata-v1:{size_bytes}:{modified}"),
    )
}

fn record_skipped(
    diagnostics: &mut ScanDiagnostics,
    path: &Path,
    reason: SkippedFileReason,
    size_bytes: Option<u64>,
    limit_bytes: Option<u64>,
) {
    diagnostics.skipped_files += 1;
    match reason {
        SkippedFileReason::Empty => diagnostics.skipped_by_reason.empty += 1,
        SkippedFileReason::TooLarge => diagnostics.skipped_by_reason.too_large += 1,
        SkippedFileReason::Unsupported => diagnostics.skipped_by_reason.unsupported += 1,
        SkippedFileReason::Binary => diagnostics.skipped_by_reason.binary += 1,
    }
    if diagnostics.skipped_samples.len() < MAX_SKIPPED_FILE_SAMPLES {
        diagnostics.skipped_samples.push(SkippedFile {
            path: path.to_path_buf(),
            reason,
            size_bytes,
            limit_bytes,
        });
    }
}

#[cfg(test)]
mod tests {
    use std::{fs::FileTimes, time::Duration};

    use super::*;

    #[test]
    fn source_snapshot_uses_metadata_from_the_read_handle() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("source.txt");
        fs::write(&path, b"source").expect("write source");
        let file = File::open(&path).expect("open source");
        let metadata = file.metadata().expect("source metadata");

        let (bytes, fingerprint) = read_source_snapshot(&path, file).expect("read source");

        assert_eq!(bytes, b"source");
        assert_eq!(
            fingerprint,
            source_fingerprint(6, modified_epoch_ms(&metadata))
        );
    }

    #[test]
    fn source_snapshot_rejects_a_replaced_path_even_with_matching_size_and_time() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("source.txt");
        let replacement = directory.path().join("replacement.txt");
        fs::write(&path, b"source").expect("write source");
        fs::write(&replacement, b"change").expect("write replacement");
        let file = File::open(&path).expect("open source");
        let modified = file
            .metadata()
            .expect("source metadata")
            .modified()
            .expect("mtime");
        File::options()
            .write(true)
            .open(&replacement)
            .expect("open replacement")
            .set_times(FileTimes::new().set_modified(modified))
            .expect("set replacement mtime");
        fs::rename(&path, directory.path().join("previous.txt")).expect("move source");
        fs::rename(&replacement, &path).expect("replace source");

        let error = read_source_snapshot(&path, file).expect_err("replacement must fail");
        assert!(error.to_string().contains("source was replaced"));
    }

    #[test]
    fn source_snapshot_rejects_changed_metadata_and_incorrect_byte_lengths() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("source.txt");
        fs::write(&path, b"source").expect("write source");
        let file = File::options()
            .write(true)
            .open(&path)
            .expect("open source");
        let before = file.metadata().expect("source metadata");

        for actual_size in [5, 7] {
            assert!(validate_source_snapshot(&path, &before, &before, actual_size).is_err());
        }
        file.set_len(7).expect("grow source");
        let longer = file.metadata().expect("larger source metadata");
        assert!(validate_source_snapshot(&path, &before, &longer, 7).is_err());

        let modified = longer.modified().expect("mtime") + Duration::from_secs(2);
        file.set_times(FileTimes::new().set_modified(modified))
            .expect("change source mtime");
        let later = file.metadata().expect("modified source metadata");
        assert!(validate_source_snapshot(&path, &longer, &later, 7).is_err());
    }
}
