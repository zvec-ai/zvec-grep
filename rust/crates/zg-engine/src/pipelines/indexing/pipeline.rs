use std::{
    borrow::Cow,
    collections::HashMap,
    future::Future,
    path::{Component, Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use futures_util::{StreamExt, stream::FuturesUnordered};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use zg_host_native::{
    DiscoveredFile, HostError, HostErrorSite, ReadBatchRequest, RootSpec, ScanRequest,
    ScanSnapshot, SourceFile as HostSource, TaskControl, WorkspaceScannerPort,
};

use crate::{
    EngineError, ErrorSite,
    api::{
        index::{
            options::WorkspaceChange,
            progress::{
                IndexEmbeddingProgress, IndexProgress, IndexProgressPhase, IndexProgressReporter,
            },
            result::{IndexResult, SkippedFile, SkippedFileReason, TimingEntry},
        },
        info::result::IndexStats,
    },
    domain::{
        Content, ContentKind, Entity, EntityFragment, EntityId, EntityMetadata, FileCategory,
        FileFormat, FileId, FileIndexStatus, FileRecord, FileSnapshot, FragmentId, Range,
        SourcePath, Workspace,
        model::{EmbeddingModelInfo, EmbeddingPurpose, EmbeddingResult},
    },
    extraction::{
        ExtractedEntity, SourceKind, TextSource, extract_for_indexing, source_kind,
        vector_content_for_fragment,
    },
    file_selection::ScanPolicy,
    models::{EmbeddingConcurrencyDefaults, EmbeddingOptions, ModelError, ModelRuntimeLease},
    storage::types::IndexedFragment,
    utils::{collapse_whitespace, decode_text, sha256_hex},
};

use super::{input_budget::index_chunk_options, model_progress, storage::IndexStorage};

const MAX_SKIPPED_FILE_SAMPLES: usize = 20;
// All default file-size limits are at least this large.
const MIN_DEFAULT_FILE_SIZE_BYTES: u64 = 1024 * 1024;
const EMBEDDING_TRANSIENT_MAX_RETRIES: usize = 3;
const EMBEDDING_RATE_LIMIT_MAX_RETRIES: usize = 6;
const EMBEDDING_TRANSIENT_RETRY_BASE_DELAY: Duration = Duration::from_millis(500);
const EMBEDDING_RATE_LIMIT_RETRY_BASE_DELAY: Duration = Duration::from_secs(2);
const EMBEDDING_TRANSIENT_RETRY_MAX_DELAY: Duration = Duration::from_secs(8);
const EMBEDDING_RATE_LIMIT_RETRY_MAX_DELAY: Duration = Duration::from_secs(30);
const EMBEDDING_RETRY_JITTER_MILLIS: u64 = 500;
const EMBEDDING_SUCCESS_STREAK_MIN: usize = 4;

#[async_trait]
pub(crate) trait IndexEmbeddingRuntime: Send + Sync {
    fn info(&self) -> &EmbeddingModelInfo;

    fn concurrency_defaults(&self) -> EmbeddingConcurrencyDefaults;

    async fn embed(
        &self,
        contents: &[Vec<Content>],
        options: EmbeddingOptions,
        progress: Option<IndexProgressReporter>,
    ) -> Result<EmbeddingResult, ModelError>;
}

#[async_trait]
impl IndexEmbeddingRuntime for ModelRuntimeLease {
    fn info(&self) -> &EmbeddingModelInfo {
        self.info()
    }

    fn concurrency_defaults(&self) -> EmbeddingConcurrencyDefaults {
        self.concurrency_defaults()
    }

    async fn embed(
        &self,
        contents: &[Vec<Content>],
        options: EmbeddingOptions,
        progress: Option<IndexProgressReporter>,
    ) -> Result<EmbeddingResult, ModelError> {
        self.embed(contents, options, progress.map(model_progress::for_index))
            .await
    }
}

pub(crate) struct IndexingContext<'context> {
    pub workspace_index: &'context Workspace,
    pub storage: &'context dyn IndexStorage,
    pub scanner: &'context dyn WorkspaceScannerPort,
    pub embedding_models: &'context [&'context dyn IndexEmbeddingRuntime],
    pub embedding_concurrency: Option<usize>,
    pub on_progress: Option<IndexProgressReporter>,
    pub signal: Option<CancellationToken>,
    pub changes: &'context [WorkspaceChange],
}

pub(crate) async fn index_workspace(
    context: &IndexingContext<'_>,
) -> Result<IndexResult, EngineError> {
    validate_context(context)?;
    let started = Instant::now();
    let mut timings = TimingCollector::default();

    let first = run_index_pass(context, &mut timings, None, &[]).await?;
    let mut passes = vec![first];
    if passes[0].stats.files_failed > 0 {
        let succeeded = passes[0].stats.files_indexed;
        let files_total = passes[0].diff.pending_count();
        report(
            context,
            IndexProgress {
                phase: IndexProgressPhase::Scanning,
                files_total: Some(files_total),
                files_indexed: Some(succeeded),
                files_failed: Some(passes[0].stats.files_failed),
                detail: Some(format!(
                    "retrying {} failed files",
                    passes[0].stats.files_failed
                )),
                embedding: None,
            },
        );
        passes.push(
            run_index_pass(
                context,
                &mut timings,
                Some(ProgressBase {
                    files_succeeded: succeeded,
                    files_total,
                }),
                &passes[0].stats.failed_files,
            )
            .await?,
        );
    }

    throw_if_cancelled(context.signal.as_ref())?;
    let final_pass = passes.last().expect("an index pass is always present");
    report(
        context,
        IndexProgress {
            phase: IndexProgressPhase::Indexing,
            files_total: Some(final_pass.diff.pending_count()),
            files_indexed: Some(final_pass.stats.files_indexed + final_pass.stats.files_failed),
            files_failed: Some(final_pass.stats.files_failed),
            detail: Some("finalizing index".to_owned()),
            embedding: None,
        },
    );
    let finalize_started = Instant::now();
    context.storage.checkpoint().map_err(|error| {
        EngineError::storage_failure(format!("failed to finalize index storage: {error}"))
    })?;
    timings.record("index_optimize", finalize_started.elapsed(), 1);

    let result = build_index_result(&passes, started.elapsed(), timings);
    if result.files_failed > 0 {
        report(
            context,
            IndexProgress {
                phase: IndexProgressPhase::Done,
                files_total: Some(final_pass.diff.pending_count()),
                files_indexed: Some(final_pass.stats.files_indexed),
                files_failed: Some(result.files_failed),
                detail: Some("indexing completed with failed files".to_owned()),
                embedding: None,
            },
        );
        return Ok(result);
    }

    report(
        context,
        IndexProgress {
            phase: IndexProgressPhase::Done,
            files_total: Some(final_pass.diff.pending_count()),
            files_indexed: Some(final_pass.stats.files_indexed),
            files_failed: Some(0),
            detail: Some("indexing complete".to_owned()),
            embedding: None,
        },
    );
    Ok(result)
}

pub(crate) async fn get_workspace_index_status(
    workspace_index: &Workspace,
    storage: &dyn IndexStorage,
    scanner: &dyn WorkspaceScannerPort,
    signal: Option<CancellationToken>,
) -> Result<IndexStats, EngineError> {
    let stored_files = storage.list_files()?;
    let control = task_control(signal);
    let snapshot = scanner
        .discover(
            &ScanRequest {
                roots: vec![host_root(workspace_index)?],
                scope_paths: Vec::new(),
            },
            &control,
        )
        .await
        .map_err(map_host_error)?;
    let (discovered_files, _) =
        classify_files(workspace_index, snapshot.files, &stored_files, &control).await?;
    let mut diff = compute_diff(discovered_files, &stored_files);
    resolve_status_modifications(scanner, &control, &mut diff).await?;

    let pending_files = stored_files
        .iter()
        .filter(|file| !file.index_status.is_indexed())
        .collect::<Vec<_>>();
    let indexed_files = stored_files
        .iter()
        .filter(|file| file.index_status.is_indexed())
        .collect::<Vec<_>>();

    Ok(IndexStats {
        files_scanned: diff.files_scanned,
        files_stored: stored_files.len(),
        files_indexed: indexed_files.len(),
        entities_indexed: indexed_files
            .iter()
            .map(|file| file.index_status.entity_count())
            .sum(),
        indexed_size_bytes: indexed_files
            .iter()
            .map(|file| file.snapshot.size_bytes)
            .sum(),
        files_pending: pending_files.len(),
        failed_files: pending_files
            .iter()
            .filter_map(|file| {
                file.index_status
                    .error()
                    .map(|reason| crate::api::info::result::FailedFile {
                        path: file.relative_path.to_path_buf(),
                        reason: reason.to_owned(),
                    })
            })
            .collect(),
        files_failed: pending_files
            .iter()
            .filter(|file| file.index_status.error().is_some())
            .count(),
        files_added: diff.added,
        files_modified: diff.modified,
        files_deleted: diff.deleted.len(),
        files_unchanged: diff.unchanged,
    })
}

#[derive(Clone, Copy)]
struct ProgressBase {
    files_succeeded: usize,
    files_total: usize,
}

struct IndexPassResult {
    files_scanned: usize,
    diff: DiffPlan,
    stats: IndexWriteStats,
    skipped: Vec<SkippedFile>,
}

#[derive(Default)]
struct IndexWriteStats {
    files_indexed: usize,
    files_failed: usize,
    entities_created: usize,
    failed_files: Vec<PathBuf>,
    failed_reasons: Vec<String>,
}

#[derive(Default)]
struct DiffPlan {
    files_scanned: usize,
    added: usize,
    modified: usize,
    pending: usize,
    unchanged: usize,
    deleted: Vec<FileRecord>,
    candidates: Vec<IndexCandidate>,
}

impl DiffPlan {
    const fn pending_count(&self) -> usize {
        self.added + self.modified + self.pending
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CandidateKind {
    Added,
    Modified,
    Pending,
}

struct IndexCandidate {
    kind: CandidateKind,
    scanned: ScannedFile,
    existing: Option<FileRecord>,
}

struct ScannedFile {
    id: Option<FileId>,
    relative_path: SourcePath,
    /// Runtime formats, absent when detection is skipped or fails.
    formats: Option<Vec<FileFormat>>,
    discovered: DiscoveredFile,
    detection_error: Option<EngineError>,
}

impl ScannedFile {
    fn to_record(&self) -> Result<FileRecord, EngineError> {
        Ok(FileRecord {
            id: self
                .id
                .ok_or_else(|| EngineError::internal("file must be registered before indexing"))?,
            relative_path: self.relative_path.clone(),
            snapshot: FileSnapshot {
                size_bytes: self.discovered.size_bytes,
                modified_epoch_ms: self.discovered.modified_epoch_ms,
                content_hash: None,
            },
            index_status: FileIndexStatus::NotIndexed,
        })
    }
}

async fn run_index_pass(
    context: &IndexingContext<'_>,
    timings: &mut TimingCollector,
    progress_base: Option<ProgressBase>,
    retry_paths: &[PathBuf],
) -> Result<IndexPassResult, EngineError> {
    throw_if_cancelled(context.signal.as_ref())?;
    report(
        context,
        IndexProgress {
            phase: IndexProgressPhase::Scanning,
            files_total: progress_base.map(|base| base.files_total),
            files_indexed: progress_base.map(|base| base.files_succeeded),
            files_failed: None,
            detail: Some(if progress_base.is_some() {
                "scanning retry candidates".to_owned()
            } else if context.changes.is_empty() {
                "scanning files".to_owned()
            } else {
                "scanning changed paths".to_owned()
            }),
            embedding: None,
        },
    );

    let mut scope = if progress_base.is_some() {
        ChangeScope::Paths(
            retry_paths
                .iter()
                .map(|path| context.workspace_index.root.join(path))
                .collect(),
        )
    } else {
        ChangeScope::from_changes(&context.workspace_index.root, context.changes)?
    };
    let all_stored = context.storage.list_files()?;
    scope.include_unfinished(&context.workspace_index.root, &all_stored);
    let existing = scope.filter_stored(&context.workspace_index.root, &all_stored);
    let scan_started = Instant::now();
    let control = task_control(context.signal.clone());
    let snapshot = context
        .scanner
        .discover(
            &ScanRequest {
                roots: vec![host_root(context.workspace_index)?],
                scope_paths: scope.scan_paths(),
            },
            &control,
        )
        .await
        .map_err(map_host_error)?;
    let mut skipped = skipped_files(&snapshot);
    let (scanned, classification_skips) = classify_files(
        context.workspace_index,
        snapshot.files,
        &all_stored,
        &control,
    )
    .await?;
    skipped.extend(classification_skips);
    skipped.truncate(MAX_SKIPPED_FILE_SAMPLES);
    let mut scanned = scope.filter_scanned(&context.workspace_index.root, scanned);
    resolve_scanned_identities(context.storage, &mut scanned)?;
    timings.record("index_scan", scan_started.elapsed(), scanned.len());

    let diff_started = Instant::now();
    let mut diff = compute_diff(scanned, &existing);
    timings.record("index_diff", diff_started.elapsed(), diff.files_scanned);
    report(
        context,
        IndexProgress {
            phase: IndexProgressPhase::Scanning,
            files_total: Some(progress_base.map_or(diff.candidates.len(), |base| base.files_total)),
            files_indexed: Some(progress_base.map_or(0, |base| base.files_succeeded)),
            files_failed: None,
            detail: Some(format!(
                "{} candidates, {} deleted, {} unchanged",
                diff.candidates.len(),
                diff.deleted.len(),
                diff.unchanged
            )),
            embedding: None,
        },
    );

    let delete_started = Instant::now();
    for file in &diff.deleted {
        throw_if_cancelled(context.signal.as_ref())?;
        context.storage.delete_file(file.id).map_err(|error| {
            EngineError::storage_failure(format!(
                "delete stale file {}: {error}",
                file.relative_path.display()
            ))
        })?;
    }
    timings.record(
        "index_delete_stale",
        delete_started.elapsed(),
        diff.deleted.len(),
    );

    let stats = index_candidates(context, &control, &mut diff, timings, progress_base).await?;
    Ok(IndexPassResult {
        files_scanned: diff.files_scanned,
        diff,
        stats,
        skipped,
    })
}

fn resolve_scanned_identities(
    storage: &dyn IndexStorage,
    scanned: &mut [ScannedFile],
) -> Result<(), EngineError> {
    let paths = scanned
        .iter()
        .filter(|scan| scan.id.is_none())
        .map(|scan| scan.relative_path.to_path_buf())
        .collect::<Vec<_>>();
    if paths.is_empty() {
        return Ok(());
    }
    let ids = storage.resolve_file_ids(&paths)?;
    if ids.len() != paths.len() {
        return Err(EngineError::storage_failure(
            "storage returned an incorrect number of file IDs",
        ));
    }
    for (scan, id) in scanned.iter_mut().filter(|scan| scan.id.is_none()).zip(ids) {
        scan.id = Some(id);
    }
    Ok(())
}

fn compute_diff(scanned: Vec<ScannedFile>, existing_files: &[FileRecord]) -> DiffPlan {
    let mut existing_by_path = existing_files
        .iter()
        .cloned()
        .map(|file| (file.relative_path.clone(), file))
        .collect::<HashMap<_, _>>();
    let mut plan = DiffPlan {
        files_scanned: scanned.len(),
        ..DiffPlan::default()
    };

    for scanned in scanned {
        let existing = existing_by_path.remove(&scanned.relative_path);
        let kind = match &existing {
            Some(file) if matches!(file.index_status, FileIndexStatus::Deleting) => {
                plan.deleted.push(file.clone());
                continue;
            }
            None => CandidateKind::Added,
            Some(existing) if !existing.index_status.is_indexed() => CandidateKind::Pending,
            Some(existing)
                if scanned.detection_error.is_none()
                    && file_is_unchanged(existing, &scanned.discovered) =>
            {
                plan.unchanged += 1;
                continue;
            }
            Some(_) => CandidateKind::Modified,
        };
        plan.increment(kind);
        plan.candidates.push(IndexCandidate {
            kind,
            scanned,
            existing,
        });
    }
    plan.deleted.extend(existing_by_path.into_values());
    plan.deleted
        .sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    plan
}

fn file_is_unchanged(existing: &FileRecord, discovered: &DiscoveredFile) -> bool {
    existing.index_status.is_indexed()
        && existing.snapshot.modified_epoch_ms.is_some()
        && existing.snapshot.size_bytes == discovered.size_bytes
        && existing.snapshot.modified_epoch_ms == discovered.modified_epoch_ms
        && existing.snapshot.content_hash.is_some()
}

impl DiffPlan {
    fn increment(&mut self, kind: CandidateKind) {
        match kind {
            CandidateKind::Added => self.added += 1,
            CandidateKind::Modified => self.modified += 1,
            CandidateKind::Pending => self.pending += 1,
        }
    }

    fn resolve_modified_as_unchanged(&mut self) {
        self.modified = self.modified.saturating_sub(1);
        self.unchanged += 1;
    }
}

async fn resolve_status_modifications(
    scanner: &dyn WorkspaceScannerPort,
    control: &TaskControl,
    diff: &mut DiffPlan,
) -> Result<(), EngineError> {
    let mut same_content = 0;
    for candidate in &diff.candidates {
        if candidate.kind != CandidateKind::Modified || candidate.scanned.detection_error.is_some()
        {
            continue;
        }
        let source = read_source(scanner, control, &candidate.scanned.discovered).await?;
        let hash = sha256_hex(&source.bytes);
        if candidate.existing.as_ref().is_some_and(|existing| {
            existing.snapshot.size_bytes == candidate.scanned.discovered.size_bytes
                && existing.snapshot.content_hash.as_deref() == Some(hash.as_str())
        }) {
            same_content += 1;
        }
    }
    diff.modified = diff.modified.saturating_sub(same_content);
    diff.unchanged += same_content;
    Ok(())
}

#[derive(Clone)]
struct PreparedFragment {
    model: String,
    entity_id: EntityId,
    fragment_id: FragmentId,
    embedding_content: Vec<Content>,
    fts_text: String,
}

struct PreparedFile {
    file: FileRecord,
    entities: Vec<Entity>,
    fragments: Vec<PreparedFragment>,
}

enum PreparedCandidate {
    File(Box<PreparedFile>),
    Unchanged,
}

type EmbeddingFuture<'context> =
    Pin<Box<dyn Future<Output = EmbeddingBatchOutcome> + Send + 'context>>;

#[expect(
    clippy::too_many_lines,
    reason = "the bounded prepare/schedule/drain loop is clearest as one linear orchestration"
)]
async fn index_candidates(
    context: &IndexingContext<'_>,
    control: &TaskControl,
    diff: &mut DiffPlan,
    timings: &mut TimingCollector,
    progress_base: Option<ProgressBase>,
) -> Result<IndexWriteStats, EngineError> {
    let policy = resolve_embedding_policy(
        context.embedding_concurrency,
        context.embedding_models[0].concurrency_defaults(),
    )?;
    let scheduler = Arc::new(EmbeddingScheduler::new(policy));
    let max_batch_size = context.embedding_models[0].info().max_batch_size;
    let mut stats = IndexWriteStats::default();
    let mut current_batch = Vec::new();
    let mut current_fragments = 0;
    let mut running: FuturesUnordered<EmbeddingFuture<'_>> = FuturesUnordered::new();

    report_indexing(
        context,
        &stats,
        diff,
        progress_base,
        None,
        Some(scheduler.snapshot()),
    );

    let candidates = std::mem::take(&mut diff.candidates);
    for mut candidate in candidates {
        throw_if_cancelled(context.signal.as_ref())?;
        report_indexing(
            context,
            &stats,
            diff,
            progress_base,
            Some(format!(
                "reading {}",
                candidate.scanned.relative_path.display()
            )),
            None,
        );
        let prepare_started = Instant::now();
        let prepared = if let Some(error) = candidate.scanned.detection_error.take() {
            Err(error)
        } else {
            prepare_candidate(context, control, &candidate).await
        };
        timings.record("index_prepare", prepare_started.elapsed(), 1);
        let prepared = match prepared {
            Ok(PreparedCandidate::Unchanged) => {
                diff.resolve_modified_as_unchanged();
                continue;
            }
            Ok(PreparedCandidate::File(prepared)) => *prepared,
            Err(error) => {
                let file = candidate.scanned.to_record()?;
                let reason = mark_file_failed(context.storage, &file, "prepare", &error)?;
                record_file_failed(&mut stats, &file, &reason);
                report_indexing(
                    context,
                    &stats,
                    diff,
                    progress_base,
                    Some(format!(
                        "failed {}",
                        candidate.scanned.relative_path.display()
                    )),
                    None,
                );
                continue;
            }
        };

        if prepared.fragments.is_empty() {
            let commit_started = Instant::now();
            commit_file(context.storage, prepared, Vec::new(), &mut stats)?;
            timings.record("index_commit", commit_started.elapsed(), 1);
            report_indexing(context, &stats, diff, progress_base, None, None);
            continue;
        }

        if prepared.fragments.len() > max_batch_size {
            if !current_batch.is_empty() {
                push_embedding(
                    &mut running,
                    std::mem::take(&mut current_batch),
                    context,
                    Arc::clone(&scheduler),
                );
                current_fragments = 0;
            }
            push_embedding(
                &mut running,
                vec![prepared],
                context,
                Arc::clone(&scheduler),
            );
        } else {
            if current_fragments > 0
                && current_fragments + prepared.fragments.len() > max_batch_size
            {
                push_embedding(
                    &mut running,
                    std::mem::take(&mut current_batch),
                    context,
                    Arc::clone(&scheduler),
                );
                current_fragments = 0;
            }
            current_fragments += prepared.fragments.len();
            current_batch.push(prepared);
            if current_fragments == max_batch_size {
                push_embedding(
                    &mut running,
                    std::mem::take(&mut current_batch),
                    context,
                    Arc::clone(&scheduler),
                );
                current_fragments = 0;
            }
        }

        if running.len() >= scheduler.task_concurrency()
            && let Some(outcome) = running.next().await
        {
            apply_embedding_outcome(context, diff, progress_base, timings, &mut stats, outcome)?;
        }
    }

    if !current_batch.is_empty() {
        push_embedding(&mut running, current_batch, context, Arc::clone(&scheduler));
    }
    while let Some(outcome) = running.next().await {
        apply_embedding_outcome(context, diff, progress_base, timings, &mut stats, outcome)?;
    }
    throw_if_cancelled(context.signal.as_ref())?;
    Ok(stats)
}

fn push_embedding<'context>(
    running: &mut FuturesUnordered<EmbeddingFuture<'context>>,
    files: Vec<PreparedFile>,
    context: &'context IndexingContext<'context>,
    scheduler: Arc<EmbeddingScheduler>,
) {
    running.push(Box::pin(embed_prepared_files(
        files,
        context.embedding_models[0],
        scheduler,
        context.signal.clone(),
        context.on_progress.clone(),
    )));
}

fn apply_embedding_outcome(
    context: &IndexingContext<'_>,
    diff: &DiffPlan,
    progress_base: Option<ProgressBase>,
    timings: &mut TimingCollector,
    stats: &mut IndexWriteStats,
    outcome: EmbeddingBatchOutcome,
) -> Result<(), EngineError> {
    timings.record("index_embedding", outcome.duration, outcome.outcomes.len());
    apply_embedding_files(
        context,
        diff,
        progress_base,
        timings,
        stats,
        outcome.outcomes,
    )
}

fn apply_embedding_files(
    context: &IndexingContext<'_>,
    diff: &DiffPlan,
    progress_base: Option<ProgressBase>,
    timings: &mut TimingCollector,
    stats: &mut IndexWriteStats,
    outcomes: Vec<EmbeddedFileOutcome>,
) -> Result<(), EngineError> {
    for outcome in outcomes {
        throw_if_cancelled(context.signal.as_ref())?;
        match outcome {
            EmbeddedFileOutcome::Success { file, vectors } => {
                let path = file.file.relative_path.clone();
                let commit_started = Instant::now();
                commit_file(context.storage, file, vectors, stats)?;
                timings.record("index_commit", commit_started.elapsed(), 1);
                report_indexing(
                    context,
                    stats,
                    diff,
                    progress_base,
                    Some(format!("indexed {}", path.display())),
                    None,
                );
            }
            EmbeddedFileOutcome::Failed { file, reason } => {
                let reason = mark_file_failed(
                    context.storage,
                    &file.file,
                    "embed",
                    &EngineError::internal(format!("embedding failed: {reason}")),
                )?;
                record_file_failed(stats, &file.file, &reason);
                report_indexing(
                    context,
                    stats,
                    diff,
                    progress_base,
                    Some(format!("failed {}", file.file.relative_path.display())),
                    None,
                );
            }
        }
    }
    Ok(())
}

fn commit_file(
    storage: &dyn IndexStorage,
    file: PreparedFile,
    vectors: Vec<Vec<f32>>,
    stats: &mut IndexWriteStats,
) -> Result<(), EngineError> {
    if file.fragments.len() != vectors.len() {
        return Err(EngineError::internal(format!(
            "entity/vector count mismatch: fragments={} vectors={}",
            file.fragments.len(),
            vectors.len()
        )));
    }
    let public_entities = file.entities.len();
    let entries = file
        .fragments
        .into_iter()
        .zip(vectors)
        .map(|(fragment, vector)| IndexedFragment {
            model: fragment.model,
            entity_id: fragment.entity_id,
            fragment_id: fragment.fragment_id,
            fts_text: fragment.fts_text,
            vector,
        })
        .collect::<Vec<_>>();
    storage.replace_file(&file.file, &file.entities, &entries)?;
    stats.files_indexed += 1;
    stats.entities_created += public_entities;
    Ok(())
}

async fn prepare_candidate(
    context: &IndexingContext<'_>,
    control: &TaskControl,
    candidate: &IndexCandidate,
) -> Result<PreparedCandidate, EngineError> {
    let source = read_source(context.scanner, control, &candidate.scanned.discovered).await?;

    let mut file = candidate.scanned.to_record()?;
    file.snapshot.size_bytes = u64::try_from(source.bytes.len())
        .map_err(|_| EngineError::invalid_argument("source byte length exceeds u64"))?;
    file.snapshot.content_hash = Some(sha256_hex(&source.bytes));
    if candidate.kind == CandidateKind::Modified
        && candidate.existing.as_ref().is_some_and(|existing| {
            existing.snapshot.size_bytes == file.snapshot.size_bytes
                && existing.snapshot.content_hash == file.snapshot.content_hash
        })
    {
        return Ok(PreparedCandidate::Unchanged);
    }

    let formats = candidate
        .scanned
        .formats
        .as_ref()
        .ok_or_else(|| EngineError::internal("index candidate must have a detected format"))?;
    if !matches!(source_kind(formats), Some(SourceKind::Text)) {
        return Err(EngineError::unsupported(format!(
            "this version only indexes text content: {}",
            file.relative_path.display()
        )));
    }
    let source_text = decode_text(&source.bytes, true).ok_or_else(|| {
        EngineError::invalid_argument(format!(
            "cannot extract text from {}: expected UTF-8 or BOM-marked UTF-16/32",
            file.relative_path.display()
        ))
    })?;
    let model = model_for_content(context, ContentKind::Text)?;
    let chunk_options = index_chunk_options(model.info().max_input_tokens, Some(&source_text));
    let text = TextSource {
        relative_path: file.relative_path.clone(),
        formats: formats.clone(),
        text: source_text.into_owned(),
    };
    let extracted = extract_for_indexing(&text, chunk_options)?;
    let entities = bind_entities(file.id, extracted)?;
    let owners = entities
        .iter()
        .map(|entity| {
            Ok((
                entity.id.clone(),
                model_for_content(context, entity.content.kind())?
                    .info()
                    .model
                    .reference(),
            ))
        })
        .collect::<Result<HashMap<_, _>, EngineError>>()?;
    let mut fragments = prepare_fragments(&entities, chunk_options.max_chunk_chars)?;
    for fragment in &mut fragments {
        fragment.model = owners[&fragment.entity_id].clone();
    }
    Ok(PreparedCandidate::File(Box::new(PreparedFile {
        file,
        entities,
        fragments,
    })))
}

fn model_for_content<'a>(
    context: &'a IndexingContext<'_>,
    kind: ContentKind,
) -> Result<&'a dyn IndexEmbeddingRuntime, EngineError> {
    if kind != ContentKind::Text {
        return Err(EngineError::unsupported(
            "this version only supports text embedding",
        ));
    }
    let index =
        context.workspace_index.index.descriptor().ok_or_else(|| {
            EngineError::invalid_argument("indexing requires an enabled workspace")
        })?;
    let reference = index.model_for(kind)?.model.reference();
    context
        .embedding_models
        .iter()
        .copied()
        .find(|model| model.info().model.reference() == reference)
        .ok_or_else(|| {
            EngineError::invalid_argument(format!("embedding runtime is missing for {reference}"))
        })
}

fn prepare_fragments(
    entities: &[Entity],
    max_chars: Option<usize>,
) -> Result<Vec<PreparedFragment>, EngineError> {
    entities
        .iter()
        .flat_map(|entity| {
            entity.fragments.iter().map(move |fragment| {
                let content = match (fragment.range, &entity.content) {
                    (Range::Full, content) => Cow::Borrowed(content),
                    (Range::Byte(range), Content::Text(text)) => {
                        let start = usize::try_from(range.start_offset()).map_err(|_| {
                            EngineError::invalid_argument("fragment start offset exceeds platform limits")
                        })?;
                        let end = usize::try_from(range.end_offset()).map_err(|_| {
                            EngineError::invalid_argument("fragment end offset exceeds platform limits")
                        })?;
                        Cow::Owned(Content::Text(crate::utils::slice_text(text, start, end)?.to_owned()))
                    }
                    _ => return Err(EngineError::invalid_argument(
                        "fragments use Full or entity-relative byte ranges for text; images and tables require Full",
                    )),
                };
                Ok(PreparedFragment {
                    model: String::new(),
                    entity_id: entity.id.clone(),
                    fragment_id: fragment.id.clone(),
                    fts_text: lexical_text(&content, entity.metadata.as_ref()),
                    embedding_content: vector_content_for_fragment(
                        &content,
                        entity.metadata.as_ref(),
                        max_chars,
                    ),
                })
            })
        })
        .collect()
}

fn lexical_text(content: &Content, metadata: Option<&EntityMetadata>) -> String {
    let mut output = String::new();
    if let Some(metadata) = metadata {
        match metadata {
            EntityMetadata::Code(code) => {
                for value in [
                    &code.symbol_name,
                    &code.scope,
                    &code.signature,
                    &code.documentation,
                ]
                .into_iter()
                .flatten()
                {
                    output.push_str(value);
                    output.push('\n');
                }
            }
            EntityMetadata::Markdown(markdown) => {
                for value in [&markdown.heading, &markdown.scope].into_iter().flatten() {
                    output.push_str(value);
                    output.push('\n');
                }
            }
        }
    }
    append_contents(&mut output, std::slice::from_ref(content));
    output
}

fn append_contents(output: &mut String, contents: &[Content]) {
    for content in contents {
        match content {
            Content::Text(text) => output.push_str(text),
            Content::Image(image) => {
                output.push_str("[image:");
                output.push_str(image.format().as_str());
                output.push(']');
            }
            Content::Table(table) => {
                for cell in &table.cells {
                    append_contents(output, &cell.contents);
                }
            }
        }
        output.push('\n');
    }
}

fn bind_entities(
    file_id: FileId,
    extracted: Vec<ExtractedEntity>,
) -> Result<Vec<Entity>, EngineError> {
    extracted
        .into_iter()
        .map(|entity| {
            let id = EntityId::new(file_id, &entity.content, entity.source_range)?;
            let fragments = entity
                .fragments
                .into_iter()
                .enumerate()
                .map(|(ordinal, fragment)| {
                    let ordinal = u32::try_from(ordinal).map_err(|_| {
                        EngineError::invalid_argument("fragment ordinal exceeds u32 limits")
                    })?;
                    Ok(EntityFragment {
                        id: FragmentId::new(&id, ordinal),
                        range: fragment.range,
                    })
                })
                .collect::<Result<Vec<_>, EngineError>>()?;
            Ok(Entity {
                id,
                file_id,
                source_range: entity.source_range,
                content: entity.content,
                metadata: entity.metadata,
                fragments,
            })
        })
        .collect()
}

fn mark_file_failed(
    storage: &dyn IndexStorage,
    file: &FileRecord,
    stage: &str,
    error: &EngineError,
) -> Result<String, EngineError> {
    let reason = collapse_whitespace(&format!("{stage}: {error}"));
    storage
        .mark_file_failed(file, &reason)
        .map_err(|mark_error| {
            EngineError::storage_failure(format!(
                "record failure for {}: {mark_error}; original={reason}",
                file.relative_path.display()
            ))
        })?;
    Ok(reason)
}

fn record_file_failed(stats: &mut IndexWriteStats, file: &FileRecord, reason: &str) {
    stats.files_failed += 1;
    stats.failed_files.push(file.relative_path.to_path_buf());
    stats.failed_reasons.push(reason.to_owned());
}

struct EmbeddingBatchOutcome {
    outcomes: Vec<EmbeddedFileOutcome>,
    duration: Duration,
}

enum EmbeddedFileOutcome {
    Success {
        file: PreparedFile,
        vectors: Vec<Vec<f32>>,
    },
    Failed {
        file: PreparedFile,
        reason: String,
    },
}

async fn embed_prepared_files(
    files: Vec<PreparedFile>,
    model: &dyn IndexEmbeddingRuntime,
    scheduler: Arc<EmbeddingScheduler>,
    signal: Option<CancellationToken>,
    progress: Option<IndexProgressReporter>,
) -> EmbeddingBatchOutcome {
    let started = Instant::now();
    if files.len() == 1 && files[0].fragments.len() > model.info().max_batch_size {
        let file = files.into_iter().next().expect("one prepared file");
        let outcome = match embed_file(
            &file.fragments,
            model,
            &scheduler,
            signal.as_ref(),
            progress,
        )
        .await
        {
            Ok(embedding) => EmbeddedFileOutcome::Success {
                file,
                vectors: embedding.vectors,
            },
            Err(error) => EmbeddedFileOutcome::Failed {
                file,
                reason: model_error_text(&error),
            },
        };
        return EmbeddingBatchOutcome {
            outcomes: vec![outcome],
            duration: started.elapsed(),
        };
    }
    let contents = files
        .iter()
        .flat_map(|file| {
            file.fragments
                .iter()
                .map(|fragment| fragment.embedding_content.clone())
        })
        .collect::<Vec<_>>();
    report_embedding_progress(progress.as_ref(), &scheduler, describe_files(&files));
    let result = embed_with_retry(
        model,
        &contents,
        &scheduler,
        signal.as_ref(),
        progress.clone(),
    )
    .await;
    let outcomes = match result {
        Ok(embedding) => split_embedding(files, embedding),
        Err(error) if classify_embedding_retry(&error).retryable => {
            let reason = model_error_text(&error);
            files
                .into_iter()
                .map(|file| EmbeddedFileOutcome::Failed {
                    file,
                    reason: reason.clone(),
                })
                .collect()
        }
        Err(_) => {
            let mut outcomes = Vec::with_capacity(files.len());
            for file in files {
                match embed_file(
                    &file.fragments,
                    model,
                    &scheduler,
                    signal.as_ref(),
                    progress.clone(),
                )
                .await
                {
                    Ok(embedding) => outcomes.push(EmbeddedFileOutcome::Success {
                        file,
                        vectors: embedding.vectors,
                    }),
                    Err(error) => outcomes.push(EmbeddedFileOutcome::Failed {
                        file,
                        reason: model_error_text(&error),
                    }),
                }
            }
            outcomes
        }
    };
    EmbeddingBatchOutcome {
        outcomes,
        duration: started.elapsed(),
    }
}

fn split_embedding(
    files: Vec<PreparedFile>,
    embedding: EmbeddingResult,
) -> Vec<EmbeddedFileOutcome> {
    if embedding.vectors.len() != files.iter().map(|file| file.fragments.len()).sum::<usize>() {
        let reason = format!(
            "embedding returned {} vectors for {} fragments",
            embedding.vectors.len(),
            files.iter().map(|file| file.fragments.len()).sum::<usize>()
        );
        return files
            .into_iter()
            .map(|file| EmbeddedFileOutcome::Failed {
                file,
                reason: reason.clone(),
            })
            .collect();
    }

    let mut vectors = embedding.vectors.into_iter();
    files
        .into_iter()
        .map(|file| {
            let count = file.fragments.len();
            let file_vectors = vectors.by_ref().take(count).collect::<Vec<_>>();
            EmbeddedFileOutcome::Success {
                file,
                vectors: file_vectors,
            }
        })
        .collect()
}

async fn embed_file(
    fragments: &[PreparedFragment],
    model: &dyn IndexEmbeddingRuntime,
    scheduler: &EmbeddingScheduler,
    signal: Option<&CancellationToken>,
    progress: Option<IndexProgressReporter>,
) -> Result<EmbeddingResult, ModelError> {
    let maximum = model.info().max_batch_size;
    let mut running = FuturesUnordered::new();
    for (batch_index, fragments) in fragments.chunks(maximum).enumerate() {
        running.push(embed_fragment_batch(
            batch_index * maximum,
            fragments,
            model,
            scheduler,
            signal,
            progress.clone(),
        ));
    }

    let mut batches = Vec::new();
    while let Some(result) = running.next().await {
        batches.push(result?);
    }
    batches.sort_by_key(|batch| batch.start);
    let mut vectors = Vec::with_capacity(fragments.len());
    let mut truncated = Vec::new();
    for batch in batches {
        vectors.extend(batch.embedding.vectors);
        truncated.extend(
            batch
                .embedding
                .truncated
                .into_iter()
                .map(|index| batch.start + index),
        );
    }
    Ok(EmbeddingResult { vectors, truncated })
}

struct FragmentBatchResult {
    start: usize,
    embedding: EmbeddingResult,
}

async fn embed_fragment_batch(
    start: usize,
    fragments: &[PreparedFragment],
    model: &dyn IndexEmbeddingRuntime,
    scheduler: &EmbeddingScheduler,
    signal: Option<&CancellationToken>,
    progress: Option<IndexProgressReporter>,
) -> Result<FragmentBatchResult, ModelError> {
    let contents = fragments
        .iter()
        .map(|fragment| fragment.embedding_content.clone())
        .collect::<Vec<_>>();
    match embed_with_retry(model, &contents, scheduler, signal, progress.clone()).await {
        Ok(embedding) => Ok(FragmentBatchResult { start, embedding }),
        Err(error) if fragments.len() == 1 || classify_embedding_retry(&error).retryable => {
            Err(error)
        }
        Err(_) => {
            let mut vectors = Vec::with_capacity(fragments.len());
            let mut truncated = Vec::new();
            for (index, fragment) in fragments.iter().enumerate() {
                let embedding = embed_with_retry(
                    model,
                    std::slice::from_ref(&fragment.embedding_content),
                    scheduler,
                    signal,
                    progress.clone(),
                )
                .await
                .map_err(|error| {
                    ModelError::internal(format!(
                        "fragment {} failed after one-by-one fallback: {}",
                        fragment.fragment_id.as_str(),
                        model_error_text(&error)
                    ))
                })?;
                let Some(vector) = embedding.vectors.into_iter().next() else {
                    return Err(ModelError::internal(
                        "embedding returned no vector for a fragment",
                    ));
                };
                vectors.push(vector);
                if !embedding.truncated.is_empty() {
                    truncated.push(index);
                }
            }
            Ok(FragmentBatchResult {
                start,
                embedding: EmbeddingResult { vectors, truncated },
            })
        }
    }
}

async fn embed_with_retry(
    model: &dyn IndexEmbeddingRuntime,
    contents: &[Vec<Content>],
    scheduler: &EmbeddingScheduler,
    signal: Option<&CancellationToken>,
    progress: Option<IndexProgressReporter>,
) -> Result<EmbeddingResult, ModelError> {
    let mut attempt = 0;
    loop {
        if signal.is_some_and(CancellationToken::is_cancelled) {
            return Err(ModelError::cancelled("embedding was cancelled"));
        }
        let permit = scheduler.acquire(signal).await?;
        let result = model
            .embed(
                contents,
                EmbeddingOptions {
                    purpose: EmbeddingPurpose::Document,
                    signal: signal.cloned(),
                    ..EmbeddingOptions::default()
                },
                progress.clone(),
            )
            .await;
        drop(permit);
        match result {
            Ok(result) => {
                scheduler.record_success();
                return Ok(result);
            }
            Err(error) => {
                let retry = classify_embedding_retry(&error);
                if !retry.retryable || attempt >= maximum_retry_attempts(retry) {
                    return Err(error);
                }
                let delay = retry_delay(attempt, retry);
                scheduler.record_retryable_failure(retry.rate_limited, delay);
                abortable_delay(delay, signal).await?;
                attempt += 1;
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct RetryClassification {
    retryable: bool,
    rate_limited: bool,
    retry_after: Option<Duration>,
}

fn classify_embedding_retry(error: &ModelError) -> RetryClassification {
    let text = model_error_text(error);
    let normalized = text.to_ascii_lowercase();
    let status = number_after(&normalized, "status=");
    let rate_limited = status == Some(429)
        || normalized.contains("rate limit")
        || normalized.contains("quota exceeded")
        || normalized.contains("too many requests")
        || normalized.contains("request rate increased too quickly");
    let server_error = status.is_some_and(|status| (500..=599).contains(&status));
    let retry_after = number_after(&normalized, "retryafterms=")
        .map(Duration::from_millis)
        .or_else(|| {
            float_after(&normalized, "retryafter=")
                .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
                .map(Duration::from_secs_f64)
        });
    RetryClassification {
        retryable: rate_limited || server_error,
        rate_limited,
        retry_after,
    }
}

fn maximum_retry_attempts(retry: RetryClassification) -> usize {
    if retry.rate_limited {
        EMBEDDING_RATE_LIMIT_MAX_RETRIES
    } else {
        EMBEDDING_TRANSIENT_MAX_RETRIES
    }
}

fn retry_delay(attempt: usize, retry: RetryClassification) -> Duration {
    if let Some(retry_after) = retry.retry_after {
        return retry_after;
    }
    let (base, maximum) = if retry.rate_limited {
        (
            EMBEDDING_RATE_LIMIT_RETRY_BASE_DELAY,
            EMBEDDING_RATE_LIMIT_RETRY_MAX_DELAY,
        )
    } else {
        (
            EMBEDDING_TRANSIENT_RETRY_BASE_DELAY,
            EMBEDDING_TRANSIENT_RETRY_MAX_DELAY,
        )
    };
    let multiplier = 1_u32
        .checked_shl(attempt.try_into().unwrap_or(u32::MAX))
        .unwrap_or(u32::MAX);
    let exponential = base.saturating_mul(multiplier);
    exponential
        .saturating_add(Duration::from_millis(pseudo_jitter()))
        .min(maximum)
}

async fn abortable_delay(
    duration: Duration,
    signal: Option<&CancellationToken>,
) -> Result<(), ModelError> {
    if duration.is_zero() {
        return Ok(());
    }
    if let Some(signal) = signal {
        tokio::select! {
            () = tokio::time::sleep(duration) => Ok(()),
            () = signal.cancelled() => Err(ModelError::cancelled("embedding was cancelled")),
        }
    } else {
        tokio::time::sleep(duration).await;
        Ok(())
    }
}

fn model_error_text(error: &ModelError) -> String {
    let mut parts = vec![error.code().to_owned()];
    parts.push(error.to_string());
    if let Some(context) = error.context() {
        parts.push(context.to_owned());
    }
    if let Some(cause) = error.cause() {
        parts.push(cause.to_owned());
    }
    parts.join(": ")
}

fn number_after(text: &str, marker: &str) -> Option<u64> {
    let start = text.find(marker)? + marker.len();
    let digits = text[start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    (!digits.is_empty()).then(|| digits.parse().ok()).flatten()
}

fn float_after(text: &str, marker: &str) -> Option<f64> {
    let start = text.find(marker)? + marker.len();
    let value = text[start..]
        .chars()
        .take_while(|character| character.is_ascii_digit() || *character == '.')
        .collect::<String>();
    (!value.is_empty()).then(|| value.parse().ok()).flatten()
}

fn pseudo_jitter() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::from(duration.subsec_nanos()) % EMBEDDING_RETRY_JITTER_MILLIS
        })
}

#[derive(Clone, Copy)]
struct EmbeddingConcurrencyPolicy {
    initial: usize,
    minimum: usize,
    maximum: usize,
    adaptive: bool,
}

fn resolve_embedding_policy(
    requested: Option<usize>,
    defaults: EmbeddingConcurrencyDefaults,
) -> Result<EmbeddingConcurrencyPolicy, EngineError> {
    if requested == Some(0) {
        return Err(EngineError::invalid_argument(
            "embedding_concurrency must be greater than zero",
        ));
    }
    if let Some(requested) = requested {
        return Ok(EmbeddingConcurrencyPolicy {
            initial: requested,
            minimum: 1,
            maximum: requested,
            adaptive: requested > 1,
        });
    }

    let initial = defaults.initial.max(1);
    let maximum = defaults.maximum.max(initial);
    Ok(EmbeddingConcurrencyPolicy {
        initial,
        minimum: initial.min(4),
        maximum,
        adaptive: maximum > 1,
    })
}

struct EmbeddingScheduler {
    policy: EmbeddingConcurrencyPolicy,
    state: Mutex<SchedulerState>,
    notify: Notify,
}

struct SchedulerState {
    active: usize,
    current: usize,
    cooldown_until: Option<Instant>,
    retryable_failures: usize,
    success_streak: usize,
}

impl EmbeddingScheduler {
    fn new(policy: EmbeddingConcurrencyPolicy) -> Self {
        Self {
            policy,
            state: Mutex::new(SchedulerState {
                active: 0,
                current: policy.initial,
                cooldown_until: None,
                retryable_failures: 0,
                success_streak: 0,
            }),
            notify: Notify::new(),
        }
    }

    const fn task_concurrency(&self) -> usize {
        self.policy.maximum
    }

    async fn acquire(
        &self,
        signal: Option<&CancellationToken>,
    ) -> Result<SchedulerPermit<'_>, ModelError> {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if signal.is_some_and(CancellationToken::is_cancelled) {
                return Err(ModelError::cancelled(
                    "embedding was cancelled while waiting for capacity",
                ));
            }
            let cooldown = {
                let state = self.lock_state();
                state
                    .cooldown_until
                    .and_then(|deadline| deadline.checked_duration_since(Instant::now()))
            };
            if let Some(duration) = cooldown {
                abortable_delay(duration, signal).await?;
                continue;
            }

            {
                let mut state = self.lock_state();
                if state.active < state.current {
                    state.active += 1;
                    return Ok(SchedulerPermit { scheduler: self });
                }
            }
            if let Some(signal) = signal {
                tokio::select! {
                    () = notified.as_mut() => {}
                    () = signal.cancelled() => {
                        return Err(ModelError::cancelled(
                            "embedding was cancelled while waiting for capacity",
                        ));
                    }
                }
            } else {
                notified.await;
            }
        }
    }

    fn release(&self) {
        let mut state = self.lock_state();
        state.active = state.active.saturating_sub(1);
        drop(state);
        self.notify.notify_waiters();
    }

    fn record_success(&self) {
        let mut state = self.lock_state();
        if !self.policy.adaptive || state.current >= self.policy.maximum {
            return;
        }
        state.success_streak += 1;
        if state.success_streak < EMBEDDING_SUCCESS_STREAK_MIN.max(state.current * 2) {
            return;
        }
        state.current += 1;
        state.success_streak = 0;
        drop(state);
        self.notify.notify_waiters();
    }

    fn record_retryable_failure(&self, rate_limited: bool, delay: Duration) {
        let mut state = self.lock_state();
        state.retryable_failures += 1;
        if rate_limited && !delay.is_zero() {
            let deadline = Instant::now() + delay;
            state.cooldown_until = Some(
                state
                    .cooldown_until
                    .map_or(deadline, |current| current.max(deadline)),
            );
        }
        if self.policy.adaptive {
            state.current = (state.current / 2).max(self.policy.minimum);
            state.success_streak = 0;
        }
    }

    fn snapshot(&self) -> IndexEmbeddingProgress {
        let state = self.lock_state();
        IndexEmbeddingProgress {
            concurrency: Some(state.current),
            max_concurrency: Some(self.policy.maximum),
            retryable_failures: Some(state.retryable_failures),
            ..IndexEmbeddingProgress::default()
        }
    }

    fn lock_state(&self) -> MutexGuard<'_, SchedulerState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

struct SchedulerPermit<'scheduler> {
    scheduler: &'scheduler EmbeddingScheduler,
}

impl Drop for SchedulerPermit<'_> {
    fn drop(&mut self) {
        self.scheduler.release();
    }
}

fn report_embedding_progress(
    reporter: Option<&IndexProgressReporter>,
    scheduler: &EmbeddingScheduler,
    detail: String,
) {
    if let Some(reporter) = reporter {
        reporter.report(IndexProgress {
            phase: IndexProgressPhase::Indexing,
            files_total: None,
            files_indexed: None,
            files_failed: None,
            detail: Some(detail),
            embedding: Some(scheduler.snapshot()),
        });
    }
}

fn report_indexing(
    context: &IndexingContext<'_>,
    stats: &IndexWriteStats,
    diff: &DiffPlan,
    progress_base: Option<ProgressBase>,
    detail: Option<String>,
    embedding: Option<IndexEmbeddingProgress>,
) {
    report(
        context,
        IndexProgress {
            phase: IndexProgressPhase::Indexing,
            files_total: Some(progress_base.map_or(diff.pending_count(), |base| base.files_total)),
            files_indexed: Some(
                progress_base.map_or(0, |base| base.files_succeeded)
                    + stats.files_indexed
                    + stats.files_failed,
            ),
            files_failed: Some(stats.files_failed),
            detail,
            embedding,
        },
    );
}

fn report(context: &IndexingContext<'_>, progress: IndexProgress) {
    if let Some(reporter) = &context.on_progress {
        reporter.report(progress);
    }
}

fn describe_files(files: &[PreparedFile]) -> String {
    match files {
        [] => "embedding 0 files".to_owned(),
        [file] => format!("embedding {}", file.file.relative_path.display()),
        [first, ..] => format!(
            "embedding {} files, starting with {}",
            files.len(),
            first.file.relative_path.display()
        ),
    }
}

fn validate_context(context: &IndexingContext<'_>) -> Result<(), EngineError> {
    if context.storage.is_read_only() {
        return Err(EngineError::invalid_argument(
            "indexing requires writable workspace storage",
        ));
    }
    context.workspace_index.validate()?;
    if !context.workspace_index.index_enabled() {
        return Err(EngineError::invalid_argument(
            "indexing requires an enabled workspace",
        ));
    }
    if context.embedding_models.len() != 1 {
        return Err(EngineError::invalid_argument(
            "indexing requires exactly one embedding model",
        ));
    }
    let index = context
        .workspace_index
        .index
        .descriptor()
        .expect("enabled workspace");
    if context.embedding_models.len() != index.embeddings.len() {
        return Err(EngineError::invalid_argument(
            "runtime models differ from workspace models",
        ));
    }
    for model in context.embedding_models {
        model.info().validate()?;
        let schema = index
            .embeddings
            .iter()
            .find(|schema| schema.model.reference() == model.info().model.reference())
            .ok_or_else(|| {
                EngineError::invalid_argument("runtime model is not in workspace index")
            })?;
        schema.ensure_index_compatible(model.info())?;
        let _ =
            resolve_embedding_policy(context.embedding_concurrency, model.concurrency_defaults())?;
    }
    Ok(())
}

fn host_root(workspace: &Workspace) -> Result<RootSpec, EngineError> {
    ScanPolicy::root_spec(&workspace.root, &workspace.scan)
}

async fn classify_files(
    workspace: &Workspace,
    files: Vec<DiscoveredFile>,
    stored: &[FileRecord],
    control: &TaskControl,
) -> Result<(Vec<ScannedFile>, Vec<SkippedFile>), EngineError> {
    let workspace = workspace.clone();
    let stored = stored.to_vec();
    let signal = control.cancellation.clone();
    tokio::task::spawn_blocking(move || {
        let mut skipped = Vec::new();
        let files = scanned_files(&workspace, files, &stored, &mut skipped, &signal)?;
        Ok((files, skipped))
    })
    .await
    .map_err(|error| EngineError::internal(format!("file classification worker failed: {error}")))?
}

fn scanned_files(
    workspace: &Workspace,
    files: Vec<DiscoveredFile>,
    stored: &[FileRecord],
    skipped: &mut Vec<SkippedFile>,
    signal: &CancellationToken,
) -> Result<Vec<ScannedFile>, EngineError> {
    let existing = stored
        .iter()
        .map(|file| (&file.relative_path, file))
        .collect::<HashMap<_, _>>();
    let mut scanned = Vec::with_capacity(files.len());
    throw_if_cancelled(Some(signal))?;
    for discovered in files {
        throw_if_cancelled(Some(signal))?;
        if discovered.root != workspace.root {
            return Err(EngineError::invalid_argument(
                "discovered file must belong to the workspace root",
            ));
        }
        let relative_path = SourcePath::new(discovered.relative_path.clone())?;
        let id = existing.get(&relative_path).map(|file| file.id);
        let absolute_path = workspace.source_path(&relative_path);
        // A saved successful index proves support, but larger files still need
        // classification when their limit depends on the format. This also
        // handles returning from an explicit size limit to the default limits.
        let known_limit = workspace
            .scan
            .max_file_size_bytes
            .unwrap_or(MIN_DEFAULT_FILE_SIZE_BYTES);
        if discovered.size_bytes <= known_limit
            && existing
                .get(&relative_path)
                .is_some_and(|file| file_is_unchanged(file, &discovered))
        {
            scanned.push(ScannedFile {
                id,
                relative_path,
                formats: None,
                discovered,
                detection_error: None,
            });
            continue;
        }
        let formats = match FileFormat::from_path(&absolute_path) {
            Ok(formats) => formats,
            Err(error) => {
                scanned.push(ScannedFile {
                    id,
                    relative_path,
                    formats: None,
                    discovered,
                    detection_error: Some(error),
                });
                continue;
            }
        };
        let explicit_maximum = workspace.scan.max_file_size_bytes;
        let maximum = explicit_maximum.unwrap_or_else(|| default_file_size_limit(&formats));
        let reason = match source_kind(&formats) {
            None | Some(SourceKind::Image(_)) => Some(
                if formats
                    .iter()
                    .any(|format| format.categories().contains(&FileCategory::Binary))
                {
                    SkippedFileReason::Binary
                } else {
                    SkippedFileReason::Unsupported
                },
            ),
            Some(_) if discovered.size_bytes > maximum => Some(SkippedFileReason::TooLarge),
            Some(_) => None,
        };
        if let Some(reason) = reason {
            if skipped.len() < MAX_SKIPPED_FILE_SAMPLES {
                skipped.push(SkippedFile {
                    path: absolute_path,
                    reason,
                    size_bytes: Some(discovered.size_bytes),
                    limit_bytes: (reason == SkippedFileReason::TooLarge).then_some(maximum),
                });
            }
            continue;
        }
        scanned.push(ScannedFile {
            id,
            relative_path,
            formats: Some(formats),
            discovered,
            detection_error: None,
        });
    }
    Ok(scanned)
}

fn default_file_size_limit(formats: &[FileFormat]) -> u64 {
    let has_category = |category| {
        formats
            .iter()
            .any(|format| format.categories().contains(&category))
    };
    if has_category(FileCategory::Image) {
        10 * 1024 * 1024
    } else if has_category(FileCategory::Data) {
        16 * 1024 * 1024
    } else if has_category(FileCategory::Code) {
        MIN_DEFAULT_FILE_SIZE_BYTES
    } else {
        256 * 1024 * 1024
    }
}

async fn read_source(
    scanner: &dyn WorkspaceScannerPort,
    control: &TaskControl,
    file: &DiscoveredFile,
) -> Result<HostSource, EngineError> {
    let mut sources = scanner
        .read_batch(
            &ReadBatchRequest {
                files: vec![file.clone()],
            },
            control,
        )
        .await
        .map_err(map_host_error)?;
    if sources.len() != 1 {
        return Err(EngineError::internal(format!(
            "native scanner returned {} sources for one requested file",
            sources.len()
        )));
    }
    let source = sources.remove(0);
    if source.root != file.root || source.relative_path != file.relative_path {
        return Err(EngineError::internal(
            "native scanner returned bytes for a different source file",
        ));
    }
    if source.source_fingerprint != file.source_fingerprint
        || u64::try_from(source.bytes.len()).ok() != Some(file.size_bytes)
    {
        return Err(EngineError::resource_busy(format!(
            "source changed while being read: {}",
            file.relative_path.display()
        )));
    }
    Ok(source)
}

fn skipped_files(snapshot: &ScanSnapshot) -> Vec<SkippedFile> {
    snapshot
        .diagnostics
        .skipped_samples
        .iter()
        .map(|skipped| SkippedFile {
            path: skipped.path.clone(),
            reason: match skipped.reason {
                zg_host_native::SkippedFileReason::Empty => SkippedFileReason::Empty,
                zg_host_native::SkippedFileReason::TooLarge => SkippedFileReason::TooLarge,
                zg_host_native::SkippedFileReason::Unsupported => SkippedFileReason::Unsupported,
                zg_host_native::SkippedFileReason::Binary => SkippedFileReason::Binary,
            },
            size_bytes: skipped.size_bytes,
            limit_bytes: skipped.limit_bytes,
        })
        .collect()
}

#[derive(Debug)]
enum ChangeScope {
    All,
    Paths(Vec<PathBuf>),
}

impl ChangeScope {
    fn from_changes(
        workspace_root: &Path,
        changes: &[WorkspaceChange],
    ) -> Result<Self, EngineError> {
        if changes.is_empty()
            || changes
                .iter()
                .any(|change| matches!(change, WorkspaceChange::Rescan))
        {
            return Ok(Self::All);
        }
        let mut paths = Vec::new();
        for change in changes {
            let path = match change {
                WorkspaceChange::Upsert(path)
                | WorkspaceChange::Delete(path)
                | WorkspaceChange::RescanDirectory(path)
                | WorkspaceChange::DeletePrefix(path) => path,
                WorkspaceChange::Rescan => continue,
            };
            let relative = if path.is_absolute() {
                path.strip_prefix(workspace_root).map_err(|_| {
                    EngineError::invalid_argument(
                        "changed path must stay within the workspace root",
                    )
                })?
            } else {
                path.as_path()
            };
            if relative
                .components()
                .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
            {
                return Err(EngineError::invalid_argument(
                    "changed path must stay within the workspace root",
                ));
            }
            paths.push(workspace_root.join(relative));
        }
        paths.sort();
        paths.dedup();
        Ok(Self::Paths(paths))
    }

    /// A scoped update also repairs interrupted writes from previous runs.
    fn include_unfinished(&mut self, root: &Path, files: &[FileRecord]) {
        if let Self::Paths(paths) = self {
            paths.extend(
                files
                    .iter()
                    .filter(|file| {
                        matches!(
                            file.index_status,
                            FileIndexStatus::NotIndexed | FileIndexStatus::Deleting
                        )
                    })
                    .map(|file| root.join(&file.relative_path)),
            );
            paths.sort();
            paths.dedup();
        }
    }

    fn contains(&self, path: &Path) -> bool {
        match self {
            Self::All => true,
            Self::Paths(paths) => paths
                .iter()
                .any(|scope| path == scope || path.starts_with(scope)),
        }
    }

    fn scan_paths(&self) -> Vec<PathBuf> {
        match self {
            Self::All => Vec::new(),
            Self::Paths(paths) => paths.clone(),
        }
    }

    fn filter_stored(&self, workspace_root: &Path, files: &[FileRecord]) -> Vec<FileRecord> {
        files
            .iter()
            .filter(|file| self.contains(&workspace_root.join(&file.relative_path)))
            .cloned()
            .collect()
    }

    fn filter_scanned(&self, workspace_root: &Path, files: Vec<ScannedFile>) -> Vec<ScannedFile> {
        files
            .into_iter()
            .filter(|file| self.contains(&workspace_root.join(&file.relative_path)))
            .collect()
    }
}

fn task_control(signal: Option<CancellationToken>) -> TaskControl {
    TaskControl::new(signal.unwrap_or_default())
}

fn throw_if_cancelled(signal: Option<&CancellationToken>) -> Result<(), EngineError> {
    if signal.is_some_and(CancellationToken::is_cancelled) {
        Err(EngineError::cancelled("indexing was cancelled"))
    } else {
        Ok(())
    }
}

pub(crate) fn map_host_error(error: HostError) -> EngineError {
    match error {
        HostError::InvalidArgument { message, origin } => {
            host_engine_error(EngineError::INVALID_ARGUMENT, message, origin)
        }
        HostError::StorageFailure {
            component,
            message,
            origin,
        } => host_engine_error(
            EngineError::STORAGE_FAILURE,
            format!("{component} failed: {message}"),
            origin,
        ),
        HostError::Cancelled { message, origin } => {
            host_engine_error(EngineError::CANCELLED, message, origin)
        }
        HostError::DeadlineExceeded { message, origin } => {
            host_engine_error(EngineError::DEADLINE_EXCEEDED, message, origin)
        }
        HostError::ResourceClosed { message, origin } => {
            host_engine_error(EngineError::RESOURCE_CLOSED, message, origin)
        }
        HostError::Internal { message, origin } => {
            host_engine_error(EngineError::INTERNAL, message, origin)
        }
    }
}

fn host_engine_error(code: &'static str, message: String, origin: HostErrorSite) -> EngineError {
    EngineError::new_at(
        code,
        message,
        ErrorSite::new(origin.file(), origin.line(), origin.column()),
    )
}

fn build_index_result(
    passes: &[IndexPassResult],
    duration: Duration,
    timings: TimingCollector,
) -> IndexResult {
    let first = &passes[0];
    let final_pass = passes.last().expect("an index pass is always present");
    IndexResult {
        files_scanned: first.files_scanned,
        files_added: passes.iter().map(|pass| pass.diff.added).sum(),
        files_modified: passes.iter().map(|pass| pass.diff.modified).sum(),
        files_pending: passes.iter().map(|pass| pass.diff.pending).sum(),
        files_deleted: passes.iter().map(|pass| pass.diff.deleted.len()).sum(),
        files_unchanged: first.diff.unchanged,
        files_failed: final_pass.stats.files_failed,
        failed_files: final_pass
            .stats
            .failed_files
            .iter()
            .zip(&final_pass.stats.failed_reasons)
            .map(|(path, reason)| crate::api::info::result::FailedFile {
                path: path.clone(),
                reason: reason.clone(),
            })
            .collect(),
        entities_created: passes.iter().map(|pass| pass.stats.entities_created).sum(),
        duration_micros: duration.as_micros().try_into().unwrap_or(u64::MAX),
        timings: timings.entries,
        skipped: first.skipped.clone(),
    }
}

#[derive(Default)]
struct TimingCollector {
    entries: Vec<TimingEntry>,
}

impl TimingCollector {
    fn record(&mut self, name: &str, duration: Duration, count: usize) {
        let duration_micros = duration.as_micros().try_into().unwrap_or(u64::MAX);
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.name == name) {
            entry.duration_micros = entry.duration_micros.saturating_add(duration_micros);
            entry.count = Some(entry.count.unwrap_or(0).saturating_add(count as u64));
        } else {
            self.entries.push(TimingEntry {
                name: name.to_owned(),
                duration_micros,
                count: Some(count.try_into().unwrap_or(u64::MAX)),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tempfile::tempdir;
    use tokio::time::sleep;
    use zg_host_native::NativeScanner;

    use crate::{
        EngineResult,
        api::index::progress::IndexProgressPhase,
        domain::{
            Content, IndexDescriptor,
            model::{EmbeddingModelInfo, Metric},
        },
    };

    use super::*;

    #[test]
    fn binding_preserves_complete_entities_and_independent_fragment_ids() {
        let source = TextSource {
            relative_path: SourcePath::new("README.md").expect("source path"),
            formats: vec![FileFormat::Markdown],
            text: format!("preamble\n\n# Heading\n\n{}", "source contents ".repeat(80)),
        };
        let options = crate::extraction::ChunkOptions {
            max_chunk_chars: Some(64),
            chunk_overlap_chars: Some(0),
        };
        let extracted = extract_for_indexing(&source, options).expect("extract markdown");
        let file_id = FileId::new(42);
        let entities = bind_entities(file_id, extracted.clone()).expect("bind entities");
        let mut reordered = extracted;
        reordered.reverse();
        for (index, entity) in reordered.iter_mut().enumerate() {
            entity.index = index + 100;
        }
        let mut rebound = bind_entities(file_id, reordered).expect("bind reordered entities");
        rebound.reverse();
        assert_eq!(entities, rebound);
        let mut ids = HashSet::new();
        for entity in &entities {
            entity.validate().expect("valid entity");
            assert!(ids.insert(entity.id.as_str()));
            for (ordinal, fragment) in entity.fragments.iter().enumerate() {
                assert!(ids.insert(fragment.id.as_str()));
                assert_eq!(
                    fragment.id.as_str(),
                    format!("{}{:08x}", entity.id.as_str(), ordinal)
                );
            }
            if let crate::domain::Range::Text(range) = entity.source_range {
                assert_eq!(
                    entity.content,
                    Content::Text(
                        crate::utils::slice_text(
                            &source.text,
                            range.start_byte_offset(),
                            range.end_byte_offset()
                        )
                        .expect("source range")
                        .into()
                    )
                );
            }
        }
        let prepared =
            prepare_fragments(&entities, options.max_chunk_chars).expect("embedding inputs");
        assert_eq!(
            prepared.len(),
            entities
                .iter()
                .map(|entity| entity.fragments.len())
                .sum::<usize>()
        );
        let section = entities
            .iter()
            .find(|entity| entity.fragments.len() > 1)
            .expect("long section");
        for item in prepared.iter().filter(|item| item.entity_id == section.id) {
            let [Content::Text(text)] = item.embedding_content.as_slice() else {
                panic!("text embedding")
            };
            assert!(text.starts_with("heading: "));
            assert!(crate::utils::utf16_len(text) <= 64);
        }
    }

    #[derive(Default)]
    struct MemoryStorage {
        files: Mutex<Vec<FileRecord>>,
        entries: Mutex<HashMap<FileId, Vec<IndexedFragment>>>,
        identities: Mutex<HashMap<PathBuf, FileId>>,
        resolved_paths: Mutex<Vec<Vec<PathBuf>>>,
        finalized: AtomicUsize,
        failed_markers: AtomicUsize,
        fail_replacements_once: Mutex<HashSet<FileId>>,
    }

    #[test]
    fn prepared_content_and_fts_stay_with_their_fragment_vectors_at_commit() {
        use crate::domain::{ByteRange, MarkdownMetadata};

        let bodies = [" 中😀\0\r\n", "尾巴\n"];
        let file_id = FileId::new(42);
        let content = Content::Text(bodies.concat());
        let entity_id = EntityId::new(file_id, &content, Range::Full).expect("entity id");
        let entity = Entity {
            id: entity_id.clone(),
            file_id,
            source_range: Range::Full,
            content,
            metadata: Some(EntityMetadata::Markdown(MarkdownMetadata {
                heading: Some("Heading".into()),
                scope: Some("Parent".into()),
                level: Some(2),
            })),
            fragments: vec![
                EntityFragment {
                    id: FragmentId::new(&entity_id, 0),
                    range: Range::Byte(ByteRange::new(0, 11).expect("first range")),
                },
                EntityFragment {
                    id: FragmentId::new(&entity_id, 1),
                    range: Range::Byte(ByteRange::new(11, 18).expect("second range")),
                },
            ],
        };
        let entities = vec![entity];
        entities[0].validate().expect("valid entity");
        let mut fragments = prepare_fragments(&entities, None).expect("prepare both projections");
        for (fragment, body) in fragments.iter_mut().zip(bodies) {
            assert_eq!(fragment.fts_text, format!("Heading\nParent\n{body}\n"));
            let [Content::Text(embedding)] = fragment.embedding_content.as_slice() else {
                panic!("text embedding");
            };
            assert!(embedding.starts_with("heading: Heading\n"));
            assert!(embedding.ends_with(body));
            fragment.model = "fixture/model".into();
        }
        let storage = MemoryStorage::default();
        let prepared = PreparedFile {
            file: FileRecord {
                id: file_id,
                relative_path: SourcePath::new("file.md").expect("path"),
                snapshot: FileSnapshot {
                    size_bytes: 18,
                    modified_epoch_ms: None,
                    content_hash: Some(sha256_hex(bodies.concat().as_bytes())),
                },
                index_status: FileIndexStatus::NotIndexed,
            },
            entities,
            fragments,
        };
        let vectors = vec![vec![1.0, 0.0], vec![0.0, 1.0]];
        commit_file(
            &storage,
            prepared,
            vectors.clone(),
            &mut IndexWriteStats::default(),
        )
        .expect("commit prepared fragments");
        let entries = storage.entries.lock().expect("stored entries");
        for (((entry, body), vector), ordinal) in entries[&file_id]
            .iter()
            .zip(bodies)
            .zip(vectors)
            .zip(0_u32..)
        {
            assert_eq!(entry.entity_id, entity_id);
            assert_eq!(entry.fragment_id, FragmentId::new(&entity_id, ordinal));
            assert_eq!(entry.model, "fixture/model");
            assert_eq!(entry.vector, vector);
            assert_eq!(entry.fts_text, format!("Heading\nParent\n{body}\n"));
        }
    }

    impl IndexStorage for MemoryStorage {
        fn is_read_only(&self) -> bool {
            false
        }

        fn resolve_file_ids(&self, paths: &[PathBuf]) -> EngineResult<Vec<FileId>> {
            self.resolved_paths
                .lock()
                .expect("record allocation")
                .push(paths.to_vec());
            let mut identities = self
                .identities
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            paths
                .iter()
                .map(|path| {
                    if let Some(id) = identities.get(path) {
                        return Ok(*id);
                    }
                    let id = FileId::new(u32::try_from(identities.len()).expect("fixture ID"));
                    identities.insert(path.clone(), id);
                    Ok(id)
                })
                .collect()
        }

        fn list_files(&self) -> EngineResult<Vec<FileRecord>> {
            Ok(self
                .files
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone())
        }

        fn replace_file(
            &self,
            file: &FileRecord,
            entities: &[Entity],
            entries: &[IndexedFragment],
        ) -> EngineResult<()> {
            if self
                .fail_replacements_once
                .lock()
                .expect("failure injection")
                .remove(&file.id)
            {
                return Err(EngineError::storage_failure("injected replacement failure"));
            }
            let mut stored = file.clone();
            stored.index_status = FileIndexStatus::Indexed {
                indexed_epoch_ms: 1,
                entity_count: entities.len() as u64,
            };
            stored.validate()?;
            self.entries
                .lock()
                .expect("stored entries")
                .insert(file.id, entries.to_vec());
            let mut files = self.files.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(existing) = files.iter_mut().find(|existing| existing.id == stored.id) {
                *existing = stored;
            } else {
                files.push(stored);
            }
            Ok(())
        }

        fn mark_file_failed(&self, file: &FileRecord, error: &str) -> EngineResult<()> {
            self.failed_markers.fetch_add(1, Ordering::AcqRel);
            self.entries
                .lock()
                .expect("stored entries")
                .remove(&file.id);
            let mut stored = file.clone();
            stored.index_status = FileIndexStatus::Failed {
                error: error.to_owned(),
            };
            let mut files = self.files.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(existing) = files.iter_mut().find(|existing| existing.id == stored.id) {
                *existing = stored;
            } else {
                files.push(stored);
            }
            Ok(())
        }

        fn delete_file(&self, file_id: FileId) -> EngineResult<()> {
            self.entries
                .lock()
                .expect("stored entries")
                .remove(&file_id);
            self.files
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .retain(|file| file.id != file_id);
            Ok(())
        }

        fn checkpoint(&self) -> EngineResult<()> {
            self.finalized.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }
    }

    struct RecordingScanner {
        inner: NativeScanner,
        requests: Mutex<Vec<ScanRequest>>,
    }

    impl RecordingScanner {
        fn new() -> Self {
            Self {
                inner: NativeScanner::default(),
                requests: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl WorkspaceScannerPort for RecordingScanner {
        async fn discover(
            &self,
            request: &ScanRequest,
            control: &TaskControl,
        ) -> Result<ScanSnapshot, HostError> {
            self.requests
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(request.clone());
            self.inner.discover(request, control).await
        }

        async fn read_batch(
            &self,
            request: &ReadBatchRequest,
            control: &TaskControl,
        ) -> Result<Vec<HostSource>, HostError> {
            self.inner.read_batch(request, control).await
        }
    }

    struct ConcurrentModel {
        info: EmbeddingModelInfo,
        calls: AtomicUsize,
        active: AtomicUsize,
        maximum_active: AtomicUsize,
        fail_embeddings: bool,
    }

    struct UnknownModifiedScanner(NativeScanner);

    #[async_trait]
    impl WorkspaceScannerPort for UnknownModifiedScanner {
        async fn discover(
            &self,
            request: &ScanRequest,
            control: &TaskControl,
        ) -> Result<ScanSnapshot, HostError> {
            let mut snapshot = self.0.discover(request, control).await?;
            for file in &mut snapshot.files {
                file.modified_epoch_ms = None;
                file.source_fingerprint = format!("unknown-mtime:{}", file.size_bytes);
            }
            Ok(snapshot)
        }

        async fn read_batch(
            &self,
            request: &ReadBatchRequest,
            control: &TaskControl,
        ) -> Result<Vec<HostSource>, HostError> {
            let mut sources = self.0.read_batch(request, control).await?;
            for file in &mut sources {
                file.source_fingerprint = format!("unknown-mtime:{}", file.bytes.len());
            }
            Ok(sources)
        }
    }

    impl ConcurrentModel {
        fn new() -> Self {
            Self {
                info: EmbeddingModelInfo {
                    model: crate::domain::model::ModelInfo {
                        provider: "local".to_owned(),
                        name: "test".to_owned(),
                        endpoint: None,
                    },
                    dimension: 2,
                    metric: Metric::Cosine,
                    max_batch_size: 1,
                    max_input_tokens: Some(64),
                    max_image_bytes: None,
                },
                calls: AtomicUsize::new(0),
                active: AtomicUsize::new(0),
                maximum_active: AtomicUsize::new(0),
                fail_embeddings: false,
            }
        }
    }

    #[async_trait]
    impl IndexEmbeddingRuntime for ConcurrentModel {
        fn info(&self) -> &EmbeddingModelInfo {
            &self.info
        }

        fn concurrency_defaults(&self) -> EmbeddingConcurrencyDefaults {
            EmbeddingConcurrencyDefaults {
                initial: 2,
                maximum: 2,
            }
        }

        async fn embed(
            &self,
            contents: &[Vec<Content>],
            _options: EmbeddingOptions,
            _progress: Option<IndexProgressReporter>,
        ) -> Result<EmbeddingResult, ModelError> {
            self.calls.fetch_add(1, Ordering::AcqRel);
            if self.fail_embeddings {
                return Err(ModelError::internal("injected embedding failure"));
            }
            let active = self.active.fetch_add(1, Ordering::AcqRel) + 1;
            self.maximum_active.fetch_max(active, Ordering::AcqRel);
            sleep(Duration::from_millis(10)).await;
            self.active.fetch_sub(1, Ordering::AcqRel);
            Ok(EmbeddingResult {
                vectors: contents.iter().map(|_| vec![1.0, 0.0]).collect(),
                truncated: Vec::new(),
            })
        }
    }

    fn workspace(root: &Path) -> Workspace {
        Workspace {
            name: "fixture".to_owned(),
            root: root.to_path_buf(),
            scan: crate::domain::ScanRules::default(),
            index: crate::domain::IndexState::Enabled(IndexDescriptor::single(
                EmbeddingModelInfo {
                    model: crate::domain::model::ModelInfo {
                        provider: "local".to_owned(),
                        name: "test".to_owned(),
                        endpoint: None,
                    },
                    dimension: 2,
                    metric: Metric::Cosine,
                    max_batch_size: 32,
                    max_input_tokens: Some(64),
                    max_image_bytes: None,
                },
            )),
            created_epoch_ms: 1,
            updated_epoch_ms: 1,
        }
    }

    #[tokio::test]
    async fn replacement_errors_abort_without_file_retry_or_checkpoint() {
        // Zero-byte files are skipped by scanning; whitespace reaches the no-fragment commit.
        for (source, embedding_calls) in [("indexable text", 1), (" \n", 0)] {
            let directory = tempdir().expect("workspace");
            std::fs::write(directory.path().join("file.txt"), source).expect("source");
            let workspace = workspace(directory.path());
            let scanner = RecordingScanner::new();
            let storage = MemoryStorage::default();
            let file_id = storage
                .resolve_file_ids(&[PathBuf::from("file.txt")])
                .expect("file identity")[0];
            storage
                .fail_replacements_once
                .lock()
                .expect("failure injection")
                .insert(file_id);
            let model = ConcurrentModel::new();

            let error = index_workspace(&IndexingContext {
                workspace_index: &workspace,
                storage: &storage,
                scanner: &scanner,
                embedding_models: &[&model],
                embedding_concurrency: None,
                on_progress: None,
                signal: None,
                changes: &[],
            })
            .await
            .expect_err("a storage failure must abort indexing");

            assert_eq!(error.code(), EngineError::STORAGE_FAILURE);
            assert!(error.to_string().contains("injected replacement failure"));
            assert_eq!(scanner.requests.lock().expect("scan requests").len(), 1);
            assert_eq!(model.calls.load(Ordering::Acquire), embedding_calls);
            // These storage operations remain usable after the replacement fails,
            // so neither failed-file recovery nor finalization may hide the error.
            assert_eq!(storage.failed_markers.load(Ordering::Acquire), 0);
            assert_eq!(storage.finalized.load(Ordering::Acquire), 0);
            assert!(storage.list_files().expect("stored files").is_empty());
        }
    }

    #[tokio::test]
    async fn embedding_errors_remain_file_failures_after_retry() {
        let directory = tempdir().expect("workspace");
        std::fs::write(directory.path().join("file.txt"), "indexable text").expect("source");
        let workspace = workspace(directory.path());
        let scanner = RecordingScanner::new();
        let storage = MemoryStorage::default();
        let mut model = ConcurrentModel::new();
        model.fail_embeddings = true;

        let result = index_workspace(&IndexingContext {
            workspace_index: &workspace,
            storage: &storage,
            scanner: &scanner,
            embedding_models: &[&model],
            embedding_concurrency: None,
            on_progress: None,
            signal: None,
            changes: &[],
        })
        .await
        .expect("embedding errors allow the index to finish");

        assert_eq!(result.files_failed, 1);
        assert_eq!(storage.failed_markers.load(Ordering::Acquire), 2);
        assert_eq!(storage.finalized.load(Ordering::Acquire), 1);
        let requests = scanner.requests.lock().expect("scan requests");
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[1].scope_paths,
            vec![directory.path().join("file.txt")]
        );
        let files = storage.list_files().expect("stored files");
        let FileIndexStatus::Failed { error } = &files[0].index_status else {
            panic!("embedding failure must remain attached to the file");
        };
        assert!(error.contains("injected embedding failure"));
    }

    #[tokio::test]
    async fn unsupported_model_schemas_fail_before_embedding_or_storage_writes() {
        use std::collections::BTreeMap;
        let directory = tempdir().expect("workspace");
        std::fs::write(directory.path().join("note.txt"), "text").expect("source");
        let first = ConcurrentModel::new();
        let mut second = ConcurrentModel::new();
        second.info.model.name = "image".into();
        second.info.max_image_bytes = Some(1024);
        let storage = MemoryStorage::default();
        let scanner = NativeScanner::default();
        for multiple in [true, false] {
            let mut workspace = workspace(directory.path());
            workspace.index = crate::domain::IndexState::Enabled(IndexDescriptor {
                fts: crate::domain::FTS_CONFIG,
                embeddings: if multiple {
                    vec![first.info.clone(), second.info.clone()]
                } else {
                    vec![second.info.clone()]
                },
                routes: if multiple {
                    BTreeMap::from([
                        (ContentKind::Text, first.info.model.reference()),
                        (ContentKind::Image, second.info.model.reference()),
                    ])
                } else {
                    BTreeMap::from([(ContentKind::Image, second.info.model.reference())])
                },
            });
            let models: Vec<&dyn IndexEmbeddingRuntime> = if multiple {
                vec![&first, &second]
            } else {
                vec![&second]
            };
            let context = IndexingContext {
                workspace_index: &workspace,
                storage: &storage,
                scanner: &scanner,
                embedding_models: &models,
                embedding_concurrency: None,
                on_progress: None,
                signal: None,
                changes: &[],
            };
            index_workspace(&context)
                .await
                .expect_err("unsupported model schema");
            assert_eq!(first.calls.load(Ordering::Acquire), 0);
            assert_eq!(second.calls.load(Ordering::Acquire), 0);
            assert!(storage.entries.lock().expect("entries").is_empty());
            assert!(storage.list_files().expect("files").is_empty());
        }
    }

    #[tokio::test]
    async fn unchanged_files_skip_allocation_and_updates_reuse_record_ids() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("file.txt");
        std::fs::write(&path, "alpha").expect("source");
        let workspace = workspace(directory.path());
        let scanner = UnknownModifiedScanner(NativeScanner::default());
        let storage = MemoryStorage::default();
        let status = get_workspace_index_status(&workspace, &storage, &scanner, None)
            .await
            .expect("status");
        assert_eq!(status.files_added, 1);
        assert!(storage.identities.lock().expect("catalog").is_empty());
        let model = ConcurrentModel::new();
        let context = IndexingContext {
            workspace_index: &workspace,
            storage: &storage,
            scanner: &scanner,
            embedding_models: &[&model],
            embedding_concurrency: None,
            on_progress: None,
            signal: None,
            changes: &[],
        };
        index_workspace(&context).await.expect("initial index");
        let id = storage.list_files().expect("files")[0].id;
        assert_eq!(storage.resolved_paths.lock().expect("calls").len(), 1);
        index_workspace(&context).await.expect("unchanged index");
        assert_eq!(storage.resolved_paths.lock().expect("calls").len(), 1);

        assert_eq!(
            storage
                .identities
                .lock()
                .expect("catalog")
                .get(Path::new("file.txt")),
            Some(&id)
        );
        std::fs::write(&path, "changed content").expect("update");
        std::fs::write(directory.path().join("new.txt"), "new source").expect("new source");
        let status = get_workspace_index_status(&workspace, &storage, &scanner, None)
            .await
            .expect("updated status");
        assert_eq!(status.files_added, 1);
        assert_eq!(status.files_modified, 1);
        assert_eq!(status.files_deleted, 0);
        assert_eq!(storage.identities.lock().expect("catalog").len(), 1);
        let result = index_workspace(&context).await.expect("incremental index");
        assert_eq!(result.files_added, 1);
        assert_eq!(result.files_modified, 1);
        let files = storage.list_files().expect("files");
        assert_eq!(
            files
                .iter()
                .find(|file| file.relative_path.as_path() == Path::new("file.txt"))
                .expect("original file")
                .id,
            id
        );
        assert_ne!(files[0].id, files[1].id);
        assert_eq!(
            *storage.resolved_paths.lock().expect("calls"),
            [
                vec![PathBuf::from("file.txt")],
                vec![PathBuf::from("new.txt")],
            ]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn classification_keeps_distinct_non_unicode_paths_and_existing_ids() {
        use std::os::unix::ffi::OsStrExt;
        let directory = tempdir().expect("temporary directory");
        let paths = [b"source-\xff.txt".as_slice(), b"source-\xfe.txt".as_slice()]
            .map(|bytes| PathBuf::from(std::ffi::OsStr::from_bytes(bytes)));
        assert_eq!(paths[0].to_string_lossy(), paths[1].to_string_lossy());
        let discovered = paths
            .iter()
            .map(|path| DiscoveredFile {
                root: directory.path().to_path_buf(),
                relative_path: path.clone(),
                size_bytes: 4,
                modified_epoch_ms: None,
                source_fingerprint: String::new(),
            })
            .collect::<Vec<_>>();
        let workspace = workspace(directory.path());
        let control = TaskControl::default();
        let (first, _) = classify_files(&workspace, discovered.clone(), &[], &control)
            .await
            .expect("classify");
        assert!(first.iter().all(|scan| scan.id.is_none()));
        assert_ne!(first[0].relative_path, first[1].relative_path);
        assert!(first[0].to_record().is_err());
        let initial = compute_diff(first, &[]);
        assert_eq!(initial.added, 2);
        assert!(initial.deleted.is_empty());
        let stored = initial
            .candidates
            .into_iter()
            .enumerate()
            .map(|(index, candidate)| {
                let mut scan = candidate.scanned;
                scan.id = Some(FileId::new(u32::try_from(index + 1).expect("fixture ID")));
                scan.to_record().expect("registered source")
            })
            .collect::<Vec<_>>();
        let (second, _) = classify_files(&workspace, discovered, &stored, &control)
            .await
            .expect("reclassify");
        for (scan, stored) in second.iter().zip(&stored) {
            assert_eq!(scan.id, Some(stored.id));
            assert_eq!(scan.relative_path, stored.relative_path);
        }
        let diff = compute_diff(second, &stored);
        assert_eq!(diff.pending, 2);
        assert_eq!(diff.added, 0);
        assert!(diff.deleted.is_empty());
    }

    #[tokio::test]
    async fn unknown_mtime_uses_content_hash_and_reports_indexed_snapshot_bytes() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("file.txt");
        std::fs::write(&path, "alpha").expect("source");
        let workspace = workspace(directory.path());
        let scanner = UnknownModifiedScanner(NativeScanner::default());
        let storage = MemoryStorage::default();
        let model = ConcurrentModel::new();
        let context = IndexingContext {
            workspace_index: &workspace,
            storage: &storage,
            scanner: &scanner,
            embedding_models: &[&model],
            embedding_concurrency: None,
            on_progress: None,
            signal: None,
            changes: &[],
        };
        index_workspace(&context).await.expect("initial index");
        let calls = model.calls.load(Ordering::Acquire);
        let unchanged = index_workspace(&context).await.expect("same bytes");
        assert_eq!(unchanged.files_unchanged, 1);
        assert_eq!(model.calls.load(Ordering::Acquire), calls);

        std::fs::write(&path, "bravo").expect("same size, different bytes");
        let status = get_workspace_index_status(&workspace, &storage, &scanner, None)
            .await
            .expect("status");
        assert_eq!(status.files_modified, 1);
        assert_eq!(status.indexed_size_bytes, 5);
        let changed = index_workspace(&context).await.expect("changed bytes");
        assert_eq!(changed.files_modified, 1);
        assert!(model.calls.load(Ordering::Acquire) > calls);
        let files = storage.list_files().expect("stored files");
        assert_eq!(files[0].snapshot.modified_epoch_ms, None);
        assert_eq!(files[0].snapshot.size_bytes, 5);
        assert_eq!(files[0].snapshot.content_hash, Some(sha256_hex(b"bravo")));
        assert!(files[0].index_status.is_indexed());

        storage
            .mark_file_failed(&files[0], "test failure")
            .expect("failed state");
        let status = get_workspace_index_status(&workspace, &storage, &scanner, None)
            .await
            .expect("failed status");
        assert_eq!(status.files_failed, 1);
        assert_eq!(status.indexed_size_bytes, 0);
        assert_eq!(status.entities_indexed, 0);
    }

    #[tokio::test]
    async fn indexes_incrementally_reuses_unchanged_files_and_honors_concurrency() {
        let directory = tempdir().expect("temporary directory");
        for index in 0..4 {
            std::fs::write(
                directory.path().join(format!("file-{index}.txt")),
                format!("content {index}"),
            )
            .expect("fixture file");
        }
        let workspace = workspace(directory.path());
        let scanner = NativeScanner::default();
        let storage = MemoryStorage::default();
        let model = ConcurrentModel::new();
        let progress = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&progress);
        let reporter = IndexProgressReporter::new(move |value| {
            captured
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(value);
        });
        let context = IndexingContext {
            workspace_index: &workspace,
            storage: &storage,
            scanner: &scanner,
            embedding_models: &[&model],
            embedding_concurrency: Some(2),
            on_progress: Some(reporter),
            signal: None,
            changes: &[],
        };

        let first = index_workspace(&context).await.expect("initial index");
        assert_eq!(first.files_added, 4);
        assert_eq!(first.entities_created, 4);
        assert_eq!(storage.list_files().expect("stored files").len(), 4);
        assert_eq!(model.maximum_active.load(Ordering::Acquire), 2);
        assert!(
            progress
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .iter()
                .any(|item| item.phase == IndexProgressPhase::Done)
        );

        let calls = model.calls.load(Ordering::Acquire);
        let second = index_workspace(&context).await.expect("unchanged index");
        assert_eq!(second.files_unchanged, 4);
        assert_eq!(second.entities_created, 0);
        assert_eq!(model.calls.load(Ordering::Acquire), calls);

        std::fs::write(directory.path().join("file-0.txt"), "changed and longer")
            .expect("modified file");
        let third = index_workspace(&context).await.expect("modified index");
        assert_eq!(third.files_modified, 1);
        assert_eq!(third.files_unchanged, 3);

        std::fs::remove_file(directory.path().join("file-3.txt")).expect("deleted file");
        let fourth = index_workspace(&context).await.expect("deleted index");
        assert_eq!(fourth.files_deleted, 1);
        assert_eq!(storage.list_files().expect("stored files").len(), 3);
        assert_eq!(storage.finalized.load(Ordering::Acquire), 4);
    }

    #[tokio::test]
    async fn changed_paths_limit_diff_and_storage_mutation_scope() {
        let directory = tempdir().expect("temporary directory");
        let first_path = directory.path().join("first.txt");
        let second_path = directory.path().join("second.txt");
        std::fs::write(&first_path, "first").expect("first fixture");
        std::fs::write(&second_path, "second").expect("second fixture");
        let workspace = workspace(directory.path());
        let scanner = RecordingScanner::new();
        let storage = MemoryStorage::default();
        let model = ConcurrentModel::new();
        index_workspace(&IndexingContext {
            workspace_index: &workspace,
            storage: &storage,
            scanner: &scanner,
            embedding_models: &[&model],
            embedding_concurrency: Some(2),
            on_progress: None,
            signal: None,
            changes: &[],
        })
        .await
        .expect("initial index");

        std::fs::write(&first_path, "first changed").expect("first modified");
        std::fs::write(&second_path, "second changed").expect("second modified");
        let changes = [WorkspaceChange::Upsert(first_path.clone())];
        let result = index_workspace(&IndexingContext {
            workspace_index: &workspace,
            storage: &storage,
            scanner: &scanner,
            embedding_models: &[&model],
            embedding_concurrency: Some(2),
            on_progress: None,
            signal: None,
            changes: &changes,
        })
        .await
        .expect("narrow index");

        let requests = scanner
            .requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        assert!(requests[0].scope_paths.is_empty());
        assert_eq!(
            requests[1].scope_paths.as_slice(),
            std::slice::from_ref(&first_path)
        );
        assert_eq!(result.files_scanned, 1);
        assert_eq!(result.files_modified, 1);
        let stored = storage.list_files().expect("stored files");
        let untouched = stored
            .iter()
            .find(|file| file.relative_path.as_path() == Path::new("second.txt"))
            .expect("second stored file");
        let changed_hash = sha256_hex(b"second changed");
        assert_ne!(
            untouched.snapshot.content_hash.as_deref(),
            Some(changed_hash.as_str())
        );
    }

    #[tokio::test]
    async fn scoped_index_repairs_unfinished_files_and_completes_deletions() {
        let directory = tempdir().expect("temporary directory");
        let root = directory.path();
        for name in [
            "requested",
            "repair",
            "missing",
            "excluded",
            "deleting",
            "failed",
        ] {
            std::fs::write(root.join(format!("{name}.txt")), name).expect("fixture file");
        }
        let mut workspace = workspace(root);
        let scanner = RecordingScanner::new();
        let storage = MemoryStorage::default();
        let model = ConcurrentModel::new();
        index_workspace(&IndexingContext {
            workspace_index: &workspace,
            storage: &storage,
            scanner: &scanner,
            embedding_models: &[&model],
            embedding_concurrency: None,
            on_progress: None,
            signal: None,
            changes: &[],
        })
        .await
        .expect("initial index");

        let repair_id = {
            let mut files = storage.files.lock().expect("stored files");
            for file in files.iter_mut() {
                file.index_status = match file
                    .relative_path
                    .as_path()
                    .file_stem()
                    .and_then(|name| name.to_str())
                {
                    Some("repair" | "missing" | "excluded") => FileIndexStatus::NotIndexed,
                    Some("deleting") => FileIndexStatus::Deleting,
                    Some("failed") => FileIndexStatus::Failed {
                        error: "previous failure".to_owned(),
                    },
                    _ => file.index_status.clone(),
                };
            }
            files
                .iter()
                .find(|file| file.relative_path.as_path() == Path::new("repair.txt"))
                .expect("repair file")
                .id
        };
        std::fs::remove_file(root.join("missing.txt")).expect("remove source");
        std::fs::write(root.join("requested.txt"), "requested changed").expect("change source");
        workspace.scan.globs.push("!excluded.txt".into());
        let changes = [WorkspaceChange::Upsert(PathBuf::from("requested.txt"))];
        let result = index_workspace(&IndexingContext {
            workspace_index: &workspace,
            storage: &storage,
            scanner: &scanner,
            embedding_models: &[&model],
            embedding_concurrency: None,
            on_progress: None,
            signal: None,
            changes: &changes,
        })
        .await
        .expect("repair during scoped update");
        assert_eq!(result.files_deleted, 3);
        assert_eq!(result.files_pending, 1);
        assert_eq!(result.files_failed, 0);
        assert_eq!(result.files_modified, 1);
        let files = storage.list_files().expect("stored files");
        assert_eq!(files.len(), 3);
        let repaired = files
            .iter()
            .find(|file| file.id == repair_id)
            .expect("identity retained");
        assert!(repaired.index_status.is_indexed());
        assert!(
            files
                .iter()
                .any(|file| matches!(file.index_status, FileIndexStatus::Failed { .. }))
        );
        assert!(
            root.join("deleting.txt").exists(),
            "delete intent must win over source existence"
        );
        let requests = scanner.requests.lock().expect("scan requests");
        assert_eq!(requests.len(), 2);
        let scope = &requests[1].scope_paths;
        assert!(scope.contains(&root.join("repair.txt")));
        assert!(!scope.contains(&root.join("failed.txt")));
    }

    #[test]
    fn changed_paths_share_the_workspace_base_and_reject_outside_paths() {
        let directory = tempdir().expect("temporary directory");
        let root = directory.path().join("workspace");
        let absolute = root.join("nested/source.rs");
        let scope = ChangeScope::from_changes(
            &root,
            &[
                WorkspaceChange::Upsert(PathBuf::from("nested/source.rs")),
                WorkspaceChange::Upsert(absolute.clone()),
            ],
        )
        .expect("workspace-relative change scope");
        assert_eq!(scope.scan_paths(), std::slice::from_ref(&absolute));
        assert!(scope.contains(&absolute));
        assert!(!scope.contains(&root.join("source.rs")));

        for path in [
            PathBuf::from("../outside.rs"),
            directory.path().join("outside.rs"),
            root.join("../outside.rs"),
        ] {
            assert!(ChangeScope::from_changes(&root, &[WorkspaceChange::Upsert(path)]).is_err());
        }
    }

    #[tokio::test]
    async fn indexes_readable_unknown_sources_and_skips_binary_or_unavailable_readers() {
        let directory = tempdir().expect("temporary directory");
        std::fs::write(directory.path().join("readable"), "ordinary text 中文 😀").expect("text");
        let mut utf16 = vec![0xff, 0xfe];
        utf16.extend("UTF-16 文本 😀".encode_utf16().flat_map(u16::to_le_bytes));
        std::fs::write(directory.path().join("encoded"), utf16).expect("UTF-16");
        std::fs::write(
            directory.path().join("database"),
            b"SQLite format 3\0payload",
        )
        .expect("database");
        std::fs::write(directory.path().join("unrecognized"), [0, 1, 2, 3]).expect("binary");
        std::fs::write(
            directory.path().join("document.pdf"),
            b"%PDF-1.7\nASCII-only syntax",
        )
        .expect("PDF");
        let workspace = workspace(directory.path());
        let scanner = NativeScanner::default();
        let storage = MemoryStorage::default();
        let model = ConcurrentModel::new();
        let result = index_workspace(&IndexingContext {
            workspace_index: &workspace,
            storage: &storage,
            scanner: &scanner,
            embedding_models: &[&model],
            embedding_concurrency: Some(1),
            on_progress: None,
            signal: None,
            changes: &[],
        })
        .await
        .expect("index supported sources");
        assert_eq!(result.files_added, 2);
        let files = storage.list_files().expect("files");
        assert_eq!(files.len(), 2);
        assert!(files.iter().all(|file| file.index_status.is_indexed()));
        assert_eq!(storage.identities.lock().expect("catalog").len(), 2);
        assert_eq!(result.skipped.len(), 3);
    }

    #[tokio::test]
    async fn classification_errors_stay_with_files_and_missing_timestamps_are_not_cached() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("fixture");
        std::fs::write(&path, "text").expect("fixture");
        let workspace = workspace(directory.path());
        let discovered = DiscoveredFile {
            root: directory.path().to_path_buf(),
            relative_path: PathBuf::from("fixture"),
            size_bytes: 4,
            modified_epoch_ms: None,
            source_fingerprint: "metadata-v1:4:unknown".to_owned(),
        };
        let control = TaskControl::default();
        let (first, _) = classify_files(&workspace, vec![discovered.clone()], &[], &control)
            .await
            .expect("classify");
        let mut scan = first.into_iter().next().expect("scanned file");
        scan.id = Some(FileId::new(1));
        let mut stored = scan.to_record().expect("registered source");
        stored.snapshot.content_hash = Some("previous hash".to_owned());
        stored.index_status = FileIndexStatus::Indexed {
            indexed_epoch_ms: 1,
            entity_count: 1,
        };
        let (second, _) = classify_files(
            &workspace,
            vec![discovered.clone()],
            &[stored.clone()],
            &control,
        )
        .await
        .expect("reclassify");
        assert_eq!(
            second[0].formats.as_deref(),
            Some([FileFormat::Text].as_slice())
        );
        assert_eq!(compute_diff(second, &[stored]).modified, 1);
        std::fs::remove_file(path).expect("remove source during scan");
        let (missing, _) = classify_files(&workspace, vec![discovered], &[], &control)
            .await
            .expect("per-file failure");
        assert_eq!(missing.len(), 1);
        assert_eq!(
            missing[0].detection_error.as_ref().expect("error").code(),
            EngineError::NOT_FOUND
        );
        control.cancellation.cancel();
        assert!(
            classify_files(&workspace, Vec::new(), &[], &control)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn unchanged_files_skip_detection_but_pending_files_are_reclassified() {
        let directory = tempdir().expect("temporary directory");
        let workspace = workspace(directory.path());
        let discovered = DiscoveredFile {
            root: directory.path().to_path_buf(),
            relative_path: PathBuf::from("no-extension"),
            size_bytes: 4,
            modified_epoch_ms: Some(1),
            source_fingerprint: String::new(),
        };
        let mut stored = FileRecord {
            id: FileId::new(1),
            relative_path: SourcePath::new("no-extension").expect("path"),
            snapshot: FileSnapshot {
                size_bytes: 4,
                modified_epoch_ms: Some(1),
                content_hash: Some("existing hash".to_owned()),
            },
            index_status: FileIndexStatus::Indexed {
                indexed_epoch_ms: 1,
                entity_count: 1,
            },
        };
        let control = TaskControl::default();
        // No source exists: a format probe would fail. Discovery and the saved
        // snapshot are sufficient to retain a successful unchanged index.
        let (scanned, skipped) = classify_files(
            &workspace,
            vec![discovered.clone()],
            std::slice::from_ref(&stored),
            &control,
        )
        .await
        .expect("unchanged classification");
        assert!(skipped.is_empty());
        assert!(scanned[0].detection_error.is_none());
        assert_eq!(
            compute_diff(scanned, std::slice::from_ref(&stored)).unchanged,
            1
        );

        for status in [
            FileIndexStatus::NotIndexed,
            FileIndexStatus::Failed {
                error: "previous failure".to_owned(),
            },
        ] {
            stored.index_status = status;
            let (pending, _) = classify_files(
                &workspace,
                vec![discovered.clone()],
                std::slice::from_ref(&stored),
                &control,
            )
            .await
            .expect("pending classification");
            assert_eq!(
                pending[0]
                    .detection_error
                    .as_ref()
                    .expect("source must be read")
                    .code(),
                EngineError::NOT_FOUND
            );
            assert_eq!(
                compute_diff(pending, std::slice::from_ref(&stored)).pending,
                1
            );
        }
    }

    #[tokio::test]
    async fn restoring_default_size_limits_rechecks_unchanged_files() {
        let directory = tempdir().expect("temporary directory");
        let mut workspace = workspace(directory.path());
        workspace.scan.max_file_size_bytes = Some(3 * MIN_DEFAULT_FILE_SIZE_BYTES);
        let discovered = DiscoveredFile {
            root: directory.path().to_path_buf(),
            relative_path: PathBuf::from("large.rs"),
            size_bytes: 2 * MIN_DEFAULT_FILE_SIZE_BYTES,
            modified_epoch_ms: Some(1),
            source_fingerprint: String::new(),
        };
        let stored = FileRecord {
            id: FileId::new(1),
            relative_path: SourcePath::new("large.rs").expect("path"),
            snapshot: FileSnapshot {
                size_bytes: discovered.size_bytes,
                modified_epoch_ms: discovered.modified_epoch_ms,
                content_hash: Some("existing hash".to_owned()),
            },
            index_status: FileIndexStatus::Indexed {
                indexed_epoch_ms: 1,
                entity_count: 1,
            },
        };
        let control = TaskControl::default();
        let (scanned, _) = classify_files(
            &workspace,
            vec![discovered.clone()],
            std::slice::from_ref(&stored),
            &control,
        )
        .await
        .expect("explicit limit");
        assert_eq!(
            compute_diff(scanned, std::slice::from_ref(&stored)).unchanged,
            1
        );

        workspace.scan.max_file_size_bytes = None;
        let (scanned, skipped) = classify_files(
            &workspace,
            vec![discovered],
            std::slice::from_ref(&stored),
            &control,
        )
        .await
        .expect("default limit");
        assert!(scanned.is_empty());
        assert_eq!(skipped[0].reason, SkippedFileReason::TooLarge);
        assert_eq!(skipped[0].limit_bytes, Some(MIN_DEFAULT_FILE_SIZE_BYTES));
        assert_eq!(
            compute_diff(scanned, std::slice::from_ref(&stored)).deleted,
            [stored]
        );
    }

    #[tokio::test]
    async fn large_unchanged_extensionless_text_keeps_its_index_after_classification() {
        let directory = tempdir().expect("temporary directory");
        let workspace = workspace(directory.path());
        let size = MIN_DEFAULT_FILE_SIZE_BYTES + 1;
        std::fs::write(
            directory.path().join("notes"),
            vec![b'x'; usize::try_from(size).expect("test size")],
        )
        .expect("extensionless text");
        let discovered = DiscoveredFile {
            root: directory.path().to_path_buf(),
            relative_path: PathBuf::from("notes"),
            size_bytes: size,
            modified_epoch_ms: Some(1),
            source_fingerprint: String::new(),
        };
        let stored = FileRecord {
            id: FileId::new(1),
            relative_path: SourcePath::new("notes").expect("path"),
            snapshot: FileSnapshot {
                size_bytes: size,
                modified_epoch_ms: Some(1),
                content_hash: Some("existing hash".to_owned()),
            },
            index_status: FileIndexStatus::Indexed {
                indexed_epoch_ms: 1,
                entity_count: 1,
            },
        };
        let (scanned, skipped) = classify_files(
            &workspace,
            vec![discovered],
            std::slice::from_ref(&stored),
            &TaskControl::default(),
        )
        .await
        .expect("text classification");
        assert!(skipped.is_empty());
        assert_eq!(
            scanned[0].formats.as_deref(),
            Some([FileFormat::Text].as_slice())
        );
        assert_eq!(compute_diff(scanned, &[stored]).unchanged, 1);
    }

    #[test]
    fn keeps_host_errors_outside_the_engine_api_boundary() {
        let origin_line = line!() + 1;
        let error = map_host_error(HostError::cancelled("native host operation was cancelled"));
        assert_eq!(error.code(), EngineError::CANCELLED);
        assert!(
            error
                .origin()
                .file
                .ends_with("src/pipelines/indexing/pipeline.rs")
        );
        assert_eq!(error.origin().line, origin_line);
        let invalid = map_host_error(HostError::invalid_argument("bad root"));
        assert_eq!(invalid.code(), EngineError::INVALID_ARGUMENT);
    }
}
