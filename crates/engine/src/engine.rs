use std::{
    fs,
    sync::{Arc, Mutex},
};

use crate::{
    CancellationToken, DeterministicEmbedder, Embedder, EngineConfig, Extractor, InMemoryStorage,
    IndexRequest, IndexResult, OperationContext, ProgressSink, Result, SearchRequest, SearchResult,
    Storage, Workspace,
};

/// The standalone engine. It owns indexing/search policy and depends only on
/// replaceable embedding and storage interfaces.
pub struct Engine {
    workspace: Workspace,
    extractor: Extractor,
    embedder: Arc<dyn Embedder>,
    storage: Mutex<Box<dyn Storage>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineStatus {
    pub workspace: std::path::PathBuf,
    pub embedding: crate::EmbeddingSchema,
    pub files: usize,
    pub segments: usize,
}

impl Engine {
    pub fn new(
        config: EngineConfig,
        embedder: Arc<dyn Embedder>,
        storage: Box<dyn Storage>,
    ) -> Result<Self> {
        let max_input_chars = embedder.schema().max_input_chars;
        Ok(Self {
            workspace: Workspace::open(config.workspace)?,
            extractor: Extractor::new(max_input_chars, 3)?,
            embedder,
            storage: Mutex::new(storage),
        })
    }

    pub fn in_memory(config: EngineConfig, dimension: usize) -> Result<Self> {
        Self::new(
            config,
            Arc::new(DeterministicEmbedder::new(dimension)?),
            Box::new(InMemoryStorage::new()),
        )
    }

    /// Opens the workspace-local persistent index at `.zvec-grep/index`.
    pub fn persistent(config: EngineConfig, embedder: Arc<dyn Embedder>) -> Result<Self> {
        let embedding = embedder.schema();
        let index_path = config.workspace.root.join(".zvec-grep/index");
        let storage = crate::ZvecStorage::open(&index_path, &embedding)?;
        Self::new(config, embedder, Box::new(storage))
    }

    /// Opens a fresh persistent engine after discarding the existing derived
    /// index. Source files are never modified.
    pub fn persistent_rebuild(config: EngineConfig, embedder: Arc<dyn Embedder>) -> Result<Self> {
        let embedding = embedder.schema();
        let index_path = config.workspace.root.join(".zvec-grep/index");
        let storage = crate::ZvecStorage::recreate(&index_path, &embedding)?;
        Self::new(config, embedder, Box::new(storage))
    }

    pub fn index(&self, request: IndexRequest) -> Result<IndexResult> {
        let cancellation = CancellationToken::new();
        self.index_with(request, &cancellation, &())
    }

    pub fn index_with(
        &self,
        request: IndexRequest,
        cancellation: &CancellationToken,
        progress: &dyn ProgressSink,
    ) -> Result<IndexResult> {
        let context = OperationContext::new(cancellation, progress);
        let mut storage = self
            .storage
            .lock()
            .map_err(|_| crate::EngineError::Storage("engine storage lock poisoned".into()))?;
        crate::indexing::index(
            &self.workspace,
            &self.extractor,
            self.embedder.as_ref(),
            storage.as_mut(),
            request,
            &context,
        )
    }

    pub fn search(&self, request: SearchRequest) -> Result<SearchResult> {
        let storage = self
            .storage
            .lock()
            .map_err(|_| crate::EngineError::Storage("engine storage lock poisoned".into()))?;
        crate::search::search(self.embedder.as_ref(), storage.as_ref(), request)
    }

    /// Exhaustive lexical search that reads current workspace files without
    /// requiring or modifying a persistent index.
    pub fn search_files(&self, request: SearchRequest) -> Result<SearchResult> {
        let cancellation = CancellationToken::new();
        self.search_files_with(request, &cancellation, &())
    }

    pub fn search_files_with(
        &self,
        request: SearchRequest,
        cancellation: &CancellationToken,
        progress: &dyn ProgressSink,
    ) -> Result<SearchResult> {
        if request.mode != crate::SearchMode::Lexical {
            return Err(crate::EngineError::InvalidConfig(
                "unindexed search supports lexical mode only".into(),
            ));
        }
        progress.report(crate::ProgressEvent::Scanning);
        let files = self.workspace.scan()?;
        let mut storage = InMemoryStorage::new();
        let mut failures = Vec::new();
        for (position, file) in files.iter().enumerate() {
            cancellation.check()?;
            progress.report(crate::ProgressEvent::Searching {
                completed: position,
                total: files.len(),
            });
            let outcome = (|| -> Result<()> {
                let bytes =
                    fs::read(&file.absolute_path).map_err(|source| crate::EngineError::Io {
                        path: file.absolute_path.clone(),
                        source,
                    })?;
                let segments = self.extractor.extract(file, &bytes)?;
                let vectors = vec![Vec::new(); segments.len()];
                storage.replace_file(
                    crate::IndexedFile {
                        id: file.id.clone(),
                        relative_path: file.relative_path.clone(),
                        format: file.format,
                        size: file.size,
                        modified_ms: file.modified_ms,
                        content_hash: blake3::hash(&bytes).to_hex().to_string(),
                    },
                    segments,
                    vectors,
                )
            })();
            if let Err(error) = outcome {
                failures.push(crate::SearchFailure {
                    path: file.relative_path.clone(),
                    message: error.to_string(),
                });
            }
        }
        progress.report(crate::ProgressEvent::Searching {
            completed: files.len(),
            total: files.len(),
        });
        let mut result = crate::search::search(self.embedder.as_ref(), &storage, request)?;
        result.failures = failures;
        progress.report(crate::ProgressEvent::Complete);
        Ok(result)
    }

    pub fn workspace(&self) -> &Workspace {
        &self.workspace
    }

    pub fn status(&self) -> Result<EngineStatus> {
        let storage = self
            .storage
            .lock()
            .map_err(|_| crate::EngineError::Storage("engine storage lock poisoned".into()))?;
        let stats = storage.stats()?;
        Ok(EngineStatus {
            workspace: self.workspace.root().to_path_buf(),
            embedding: self.embedder.schema(),
            files: stats.files,
            segments: stats.segments,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::{SearchMode, WorkspaceConfig};

    use super::*;

    #[test]
    fn indexes_searches_and_incrementally_updates_a_workspace() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("parser.rs"),
            "fn parse_source() { println!(\"source parser\"); }",
        )
        .unwrap();
        fs::write(temp.path().join("fruit.txt"), "banana orchard").unwrap();
        let engine =
            Engine::in_memory(EngineConfig::new(WorkspaceConfig::new(temp.path())), 128).unwrap();

        let first = engine.index(IndexRequest::default()).unwrap();
        assert_eq!(first.added, 2);
        assert_eq!(engine.status().unwrap().files, 2);
        let mut request = SearchRequest::new("source parser");
        request.mode = SearchMode::Hybrid;
        let result = engine.search(request).unwrap();
        assert_eq!(
            result.items[0].segment.relative_path,
            std::path::Path::new("parser.rs")
        );

        let mut symbol_search = SearchRequest::new("parse_source");
        symbol_search.mode = SearchMode::Lexical;
        symbol_search.filter.symbol_names = vec!["parse_source".into()];
        let symbol_result = engine.search(symbol_search).unwrap();
        assert_eq!(
            symbol_result.items[0].segment.symbol.as_ref().unwrap().name,
            "parse_source"
        );

        let mut filtered = SearchRequest::new("source parser");
        filtered.mode = SearchMode::Lexical;
        filtered.filter.path_glob = Some("*.txt".into());
        assert!(engine.search(filtered).unwrap().items.is_empty());

        let second = engine.index(IndexRequest::default()).unwrap();
        assert_eq!(second.unchanged, 2);
        assert_eq!(second.added, 0);

        fs::remove_file(temp.path().join("fruit.txt")).unwrap();
        let third = engine.index(IndexRequest::default()).unwrap();
        assert_eq!(third.deleted, 1);
    }

    #[test]
    fn cancellation_stops_before_scanning() {
        let temp = tempfile::tempdir().unwrap();
        let engine =
            Engine::in_memory(EngineConfig::new(WorkspaceConfig::new(temp.path())), 32).unwrap();
        let token = CancellationToken::new();
        token.cancel();
        assert!(matches!(
            engine.index_with(IndexRequest::default(), &token, &()),
            Err(crate::EngineError::Cancelled)
        ));
    }

    #[test]
    fn exhaustive_lexical_search_does_not_require_an_index() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("live.txt"), "fresh workspace content").unwrap();
        let engine =
            Engine::in_memory(EngineConfig::new(WorkspaceConfig::new(temp.path())), 32).unwrap();
        let mut request = SearchRequest::new("fresh workspace");
        request.mode = SearchMode::Lexical;
        let result = engine.search_files(request).unwrap();
        assert_eq!(result.items.len(), 1);
        assert_eq!(
            result.items[0].segment.relative_path,
            std::path::Path::new("live.txt")
        );
    }

    #[test]
    fn persistent_engine_exclusively_owns_the_workspace_index() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("persistent.txt"),
            "durable semantic search",
        )
        .unwrap();
        let config = EngineConfig::new(WorkspaceConfig::new(temp.path()));
        let first = Engine::persistent(
            config.clone(),
            Arc::new(DeterministicEmbedder::new(32).unwrap()),
        )
        .unwrap();
        first.index(IndexRequest::default()).unwrap();
        assert_eq!(
            first
                .search(SearchRequest::new("durable"))
                .unwrap()
                .items
                .len(),
            1
        );
        let second = Engine::persistent(config, Arc::new(DeterministicEmbedder::new(32).unwrap()));
        assert!(matches!(second, Err(crate::EngineError::Busy)));
        drop(first);
    }
}
