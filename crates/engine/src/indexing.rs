use std::{collections::HashMap, fs, path::PathBuf, time::Instant};

use crate::{
    Embedder, Extractor, FileId, IndexedFile, OperationContext, ProgressEvent, Result, Storage,
    Workspace,
};

#[derive(Debug, Clone, Copy)]
pub struct IndexRequest {
    pub rebuild: bool,
    /// Maximum number of segments sent through one embedding call.
    pub embedding_batch_size: usize,
    /// Maximum changed files between durable storage checkpoints.
    pub commit_batch_size: usize,
}

impl Default for IndexRequest {
    fn default() -> Self {
        Self {
            rebuild: false,
            embedding_batch_size: 128,
            commit_batch_size: 4_096,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexFailure {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Debug, Clone, Default)]
pub struct IndexResult {
    pub scanned: usize,
    pub added: usize,
    pub modified: usize,
    pub deleted: usize,
    pub unchanged: usize,
    pub segments: usize,
    pub failures: Vec<IndexFailure>,
    pub elapsed_ms: u128,
}

struct PendingFile {
    file: IndexedFile,
    segments: Vec<crate::FileSegment>,
    existed: bool,
}

pub(crate) fn index(
    workspace: &Workspace,
    extractor: &Extractor,
    embedder: &dyn Embedder,
    storage: &mut dyn Storage,
    request: IndexRequest,
    context: &OperationContext<'_>,
) -> Result<IndexResult> {
    if request.embedding_batch_size == 0 || request.commit_batch_size == 0 {
        return Err(crate::EngineError::InvalidConfig(
            "embedding and commit batch sizes must be positive".into(),
        ));
    }
    let started = Instant::now();
    context.cancellation.check()?;
    context.progress.report(ProgressEvent::Scanning);
    let scanned = workspace.scan()?;
    let mut result = IndexResult {
        scanned: scanned.len(),
        ..IndexResult::default()
    };
    let existing: HashMap<FileId, IndexedFile> = storage
        .files()?
        .into_iter()
        .map(|file| (file.id.clone(), file))
        .collect();
    let current_ids: std::collections::HashSet<_> =
        scanned.iter().map(|file| file.id.clone()).collect();

    let deleted: Vec<_> = existing
        .values()
        .filter(|old| !current_ids.contains(&old.id))
        .map(|old| old.id.clone())
        .collect();
    storage.remove_files(&deleted)?;
    result.deleted = deleted.len();

    let mut pending = Vec::new();
    let mut pending_segments = 0;
    let mut uncommitted_files = 0;
    for (position, file) in scanned.iter().enumerate() {
        context.cancellation.check()?;
        context.progress.report(ProgressEvent::Indexing {
            completed: position,
            total: scanned.len(),
        });

        let previous = if request.rebuild {
            None
        } else {
            existing.get(&file.id)
        };
        if previous.is_some_and(|old| old.size == file.size && old.modified_ms == file.modified_ms)
        {
            result.unchanged += 1;
            continue;
        }

        let outcome = (|| -> Result<Option<PendingFile>> {
            let bytes = fs::read(&file.absolute_path).map_err(|source| crate::EngineError::Io {
                path: file.absolute_path.clone(),
                source,
            })?;
            let content_hash = blake3::hash(&bytes).to_hex().to_string();
            let indexed_file = IndexedFile {
                id: file.id.clone(),
                relative_path: file.relative_path.clone(),
                format: file.format,
                size: file.size,
                modified_ms: file.modified_ms,
                content_hash: content_hash.clone(),
            };

            if previous.is_some_and(|old| old.content_hash == content_hash) {
                storage.update_file(indexed_file)?;
                return Ok(None);
            }

            let segments = extractor.extract(file, &bytes)?;
            Ok(Some(PendingFile {
                file: indexed_file,
                segments,
                existed: previous.is_some(),
            }))
        })();

        match outcome {
            Ok(Some(file)) => {
                pending_segments += file.segments.len();
                pending.push(file);
                if pending_segments >= request.embedding_batch_size {
                    uncommitted_files +=
                        flush_pending(embedder, storage, &mut pending, &mut result, context)?;
                    pending_segments = 0;
                    if uncommitted_files >= request.commit_batch_size {
                        storage.commit()?;
                        uncommitted_files = 0;
                    }
                }
            }
            Ok(None) => result.unchanged += 1,
            Err(error) => result.failures.push(IndexFailure {
                path: file.relative_path.clone(),
                message: error.to_string(),
            }),
        }
    }

    flush_pending(embedder, storage, &mut pending, &mut result, context)?;
    storage.commit()?;

    context.progress.report(ProgressEvent::Indexing {
        completed: scanned.len(),
        total: scanned.len(),
    });
    context.progress.report(ProgressEvent::Complete);
    result.elapsed_ms = started.elapsed().as_millis();
    Ok(result)
}

fn flush_pending(
    embedder: &dyn Embedder,
    storage: &mut dyn Storage,
    pending: &mut Vec<PendingFile>,
    result: &mut IndexResult,
    context: &OperationContext<'_>,
) -> Result<usize> {
    if pending.is_empty() {
        return Ok(0);
    }
    let texts: Vec<_> = pending
        .iter()
        .flat_map(|file| file.segments.iter().map(|segment| segment.text.clone()))
        .collect();
    context.cancellation.check()?;
    context.progress.report(ProgressEvent::Embedding {
        segments: texts.len(),
    });
    let embedded = embedder.embed(&texts)?;
    if embedded.len() != texts.len() {
        return Err(crate::EngineError::Embedding(format!(
            "embedder returned {} vectors for {} inputs",
            embedded.len(),
            texts.len()
        )));
    }
    let mut vectors = embedded.into_iter();
    let mut writes = Vec::with_capacity(pending.len());
    let mut counts = Vec::with_capacity(pending.len());
    for file in pending.drain(..) {
        let count = file.segments.len();
        let file_vectors: Vec<_> = vectors.by_ref().take(count).collect();
        writes.push((file.file, file.segments, file_vectors));
        counts.push((count, file.existed));
    }
    context.cancellation.check()?;
    context.progress.report(ProgressEvent::Persisting {
        files: writes.len(),
    });
    let file_count = writes.len();
    storage.replace_files(writes)?;
    for (count, existed) in counts {
        result.segments += count;
        if existed {
            result.modified += 1;
        } else {
            result.added += 1;
        }
    }
    Ok(file_count)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use crate::{DeterministicEmbedder, EmbeddingSchema, InMemoryStorage, WorkspaceConfig};

    use super::*;

    struct CountingEmbedder {
        calls: AtomicUsize,
        inner: DeterministicEmbedder,
    }

    impl Embedder for CountingEmbedder {
        fn schema(&self) -> EmbeddingSchema {
            self.inner.schema()
        }

        fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.inner.embed(texts)
        }
    }

    #[test]
    fn groups_files_into_embedding_batches() {
        let temp = tempfile::tempdir().unwrap();
        for index in 0..3 {
            fs::write(
                temp.path().join(format!("{index}.txt")),
                format!("file {index}"),
            )
            .unwrap();
        }
        let workspace = Workspace::open(WorkspaceConfig::new(temp.path())).unwrap();
        let embedder = CountingEmbedder {
            calls: AtomicUsize::new(0),
            inner: DeterministicEmbedder::new(32).unwrap(),
        };
        let mut storage = InMemoryStorage::new();
        let cancellation = crate::CancellationToken::new();
        let context = OperationContext::quiet(&cancellation);
        let result = index(
            &workspace,
            &Extractor::default(),
            &embedder,
            &mut storage,
            IndexRequest {
                rebuild: false,
                embedding_batch_size: 2,
                commit_batch_size: 2,
            },
            &context,
        )
        .unwrap();
        assert_eq!(result.added, 3);
        assert_eq!(embedder.calls.load(Ordering::Relaxed), 2);
    }
}
