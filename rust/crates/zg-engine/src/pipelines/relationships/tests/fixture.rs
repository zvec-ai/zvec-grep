//! Shared workspace, canonical symbols and persisted edge fixtures.
use super::*;
use std::path::Path;

pub(super) struct Fixture {
    pub(super) engine: ZvecGrep,
    _directory: tempfile::TempDir,
    pub(super) root: PathBuf,
    pub(super) home: PathBuf,
    pub(super) graph_path: PathBuf,
}

impl Fixture {
    pub(super) fn new() -> Self {
        let directory = tempfile::tempdir().expect("workspace");
        let root = directory.path().canonicalize().expect("canonical root");
        let home = root.join(".zvec-grep");
        let layout = WorkspaceLayout::new(home.clone()).expect("layout");
        let manifest = Workspace::new(
            &root,
            layout.clone(),
            ScanRules::default(),
            Some(CURRENT_INDEX_VERSION),
            Some(uuid::Uuid::new_v4().to_string()),
            1,
            1,
            IndexState::Enabled(Box::new(
                IndexDescriptor::new(schema(), BTreeMap::new(), BTreeMap::new(), FTS_CONFIG)
                    .expect("index descriptor"),
            )),
        )
        .expect("workspace");
        let storage_home = layout.index_dir().to_path_buf();
        fs::create_dir_all(&storage_home).expect("generation");
        let storage = IndexStore::open_with_graph(
            WorkspaceIndexStorageOptions::ReadWrite {
                storage_path: storage_home.clone(),
                embeddings: vec![schema()],
            },
            true,
        )
        .expect("writable index");
        let local = write_symbols(
            &storage,
            "a.rs",
            &[
                ("run", Some("red"), 2, "fn run() {}"),
                ("run", Some("blue"), 10, "fn run() {}"),
                ("invoke", None, 20, "fn invoke() { run(); }"),
            ],
        );
        let remote = write_symbols(&storage, "z.rs", &[("run", None, 3, "fn run() {}")]);
        write_symbols(
            &storage,
            "c.rs",
            &[
                ("helper", Some("red"), 5, "fn helper() { needle(); run(); }"),
                (
                    "helper",
                    Some("blue"),
                    15,
                    "fn helper() { needle(); run(); }",
                ),
                ("", None, 25, "needle is mentioned in non-code content"),
            ],
        );
        storage.close().expect("persist index before reading");
        let graph_path = storage_home.join("graph/graph.sqlite");
        write_graph(&graph_path, &local, &remote[0]);
        write_workspace_manifest(layout.manifest_path(), &manifest).expect("publish manifest");
        Self {
            engine: ZvecGrep::new(),
            _directory: directory,
            root,
            home,
            graph_path,
        }
    }

    pub(super) fn layout(&self) -> WorkspaceLayout {
        WorkspaceLayout::new(self.home.clone()).expect("workspace layout")
    }

    pub(super) fn storage_home(&self) -> &std::path::Path {
        self.graph_path
            .parent()
            .expect("graph")
            .parent()
            .expect("generation")
    }

    pub(super) fn open_store(&self) -> IndexStore {
        IndexStore::open_with_graph(
            WorkspaceIndexStorageOptions::ReadWrite {
                storage_path: self.storage_home().to_owned(),
                embeddings: vec![schema()],
            },
            true,
        )
        .expect("open test storage")
    }

    pub(super) fn add_symbols(
        &self,
        path: &str,
        definitions: &[(&str, Option<&str>, usize, &str)],
    ) -> Vec<Entity> {
        let store = self.open_store();
        let entities = write_symbols(&store, path, definitions);
        store.close().expect("publish symbols");
        entities
    }

    pub(super) async fn callers(&self, symbol: &str) -> RelationshipResult {
        self.engine
            .callers(self.options(symbol))
            .await
            .expect("callers")
    }

    pub(super) async fn callers_with_limit(
        &self,
        symbol: &str,
        limit: usize,
    ) -> RelationshipResult {
        self.engine
            .callers(RelationshipOptions {
                limit: Some(limit),
                ..self.options(symbol)
            })
            .await
            .expect("limited callers")
    }

    pub(super) async fn callees(&self, symbol: &str, limit: usize) -> RelationshipResult {
        self.engine
            .callees(RelationshipOptions {
                limit: Some(limit),
                ..self.options(symbol)
            })
            .await
            .expect("callees")
    }

    pub(super) fn find_symbol(&self, name: &str, scope: Option<&str>) -> Entity {
        let store = self.open_store();
        let symbol = store
            .read_snapshot(|view| {
                crate::pipelines::relationships::pipeline::find_symbols(view, name, scope, None)
            })
            .expect("fixture symbol")
            .remove(0)
            .entity;
        store.close().expect("close fixture writer");
        symbol
    }

    pub(super) fn manifest(&self) -> Workspace {
        read_workspace_manifest(&self.root, &self.layout())
            .expect("manifest")
            .expect("index")
    }

    pub(super) fn writer_session(&self) -> WriterSession {
        let lock = Arc::new(
            acquire_workspace_lock(&self.layout(), LockMode::Exclusive, "test index")
                .expect("writer lock"),
        );
        WriterSession::new(self.open_store(), Vec::new(), self.manifest(), lock)
    }

    pub(super) fn options(&self, symbol: &str) -> RelationshipOptions {
        RelationshipOptions {
            root: Some(self.root.clone()),
            symbol: symbol.into(),
            ..RelationshipOptions::default()
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.engine.close();
    }
}

pub(super) fn schema() -> EmbeddingModelInfo {
    EmbeddingModelInfo::new(
        ModelInfo::new(
            "fixture",
            "relationships-without-model-runtime",
            [ContentKind::Text, ContentKind::Code],
            32,
        )
        .expect("model"),
        3,
        EmbeddingMetric::Cosine,
        None,
        None,
    )
    .expect("embedding model")
}

pub(super) fn write_symbols(
    storage: &IndexStore,
    path: &str,
    definitions: &[(&str, Option<&str>, usize, &str)],
) -> Vec<Entity> {
    let relative_path = SourcePath::new(path).expect("source path");
    let file_id = storage
        .resolve_file_ids(&[relative_path.to_path_buf()])
        .expect("file identity")[0];
    let entities: Vec<_> = definitions
        .iter()
        .map(|&(name, scope, line, text)| {
            let content = if name.is_empty() {
                Content::Text(text.into())
            } else {
                Content::Code(text.into())
            };
            let offset = (line - 1) * 100;
            let source_range = Range::Text(
                TextRange::from_coordinates(offset, offset + text.len(), line, line, 0, text.len())
                    .expect("source coordinates"),
            );
            let id = EntityId::new(file_id, &content, source_range).expect("entity identity");
            let fragments = (0..2)
                .map(|ordinal| EntityFragment {
                    id: FragmentId::new(&id, ordinal),
                    range: Range::Full,
                })
                .collect();
            Entity::new(
                id,
                file_id,
                source_range,
                content,
                (!name.is_empty()).then(|| {
                    EntityMetadata::Code(CodeMetadata {
                        symbol_name: Some(name.into()),
                        symbol_type: Some(SymbolType::Function),
                        scope: scope.map(str::to_owned),
                        signature: None,
                        documentation: None,
                        visibility: None,
                        language: None,
                    })
                }),
                fragments,
            )
            .expect("fixture entity")
        })
        .collect();
    let file = FileRecord::new(
        file_id,
        relative_path,
        FileSnapshot {
            size_bytes: 4096,
            modified_epoch_ms: None,
            content_hash: Some(crate::utils::sha256_hex(path.as_bytes())),
        },
        FileIndexStatus::NotIndexed,
    )
    .expect("fixture file");
    let entries: Vec<_> = entities
        .iter()
        .flat_map(|entity| {
            entity.fragments().iter().map(|fragment| IndexedFragment {
                entity_id: entity.id().clone(),
                fragment_id: fragment.id.clone(),
                model: schema().model().reference(),
                vector: vec![1.0, 0.0, 0.0],
                fts_text: match entity.content() {
                    Content::Text(text) | Content::Code(text) => text.clone(),
                    Content::Image(_) => unreachable!("text fixture"),
                },
            })
        })
        .collect();
    storage
        .replace_file(&file, &entities, &entries)
        .expect("index fixture symbols");
    entities
}

// Query fixtures seed the production schema directly; graph write/resolve APIs
// have their own storage tests. Include missing, unresolved and non-call edges.
fn write_graph(path: &std::path::Path, local: &[Entity], remote: &Entity) {
    let mut graph = rusqlite::Connection::open(path).expect("graph fixture");
    let tx = graph.transaction().expect("fixture transaction");
    let source = &local[2];
    let missing = EntityId::new(
        source.file_id(),
        &Content::Text("missing".into()),
        Range::Full,
    )
    .expect("missing endpoint");
    for (target, lines, kind) in [
        (local[0].id().as_str(), 1..26, "calls"),
        (local[1].id().as_str(), 30..53, "calls"),
        (missing.as_str(), 99..100, "calls"),
        (local[0].id().as_str(), 100..101, "contains"),
    ] {
        for line in lines {
            tx.execute(
                "INSERT INTO edges (file_id, source, target, kind, status, line, col, provenance)
                 VALUES (?1, ?2, ?3, ?4, 'resolved', ?5, 4, 'file_local')",
                rusqlite::params![
                    source.file_id().get(),
                    source.id().as_str(),
                    target,
                    kind,
                    line
                ],
            )
            .expect("local edge");
        }
    }
    for (target, name, line, status, provenance) in [
        (
            Some(remote.id().as_str()),
            "remote",
            101,
            "resolved",
            Some("import_scoped"),
        ),
        (None, "unresolved", 102, "pending", None),
    ] {
        tx.execute(
            "INSERT INTO edges (file_id, source, target, kind, status, reference_name, line, col, provenance)
             VALUES (?1, ?2, ?3, 'calls', ?4, ?5, ?6, 8, ?7)",
            rusqlite::params![source.file_id().get(), source.id().as_str(), target, status, name, line, provenance],
        ).expect("cross-file or pending reference");
    }
    tx.commit().expect("persist graph fixture");
}

pub(super) fn insert_call(fixture: &Fixture, file_id: u32, source: &str, target: &str) {
    let graph = rusqlite::Connection::open(&fixture.graph_path).expect("graph fixture");
    graph.execute(
        "INSERT INTO edges (file_id, source, target, kind, status, reference_name, line, col, provenance)
         VALUES (?, ?, ?, 'calls', 'resolved', 'run', 7, 4, 'import_scoped')",
        rusqlite::params![file_id, source, target],
    ).expect("persist call fixture");
}

pub(super) fn corrupt_entity_bodies(home: &Path, ids: &[EntityId]) {
    corrupt_entity_field(home, ids, "payload");
}

pub(super) fn corrupt_entity_metadata(home: &Path, ids: &[EntityId]) {
    corrupt_entity_field(home, ids, "metadata");
}

pub(super) fn corrupt_entity_identity(home: &Path, ids: &[EntityId]) {
    corrupt_entity_field(home, ids, "entity_id");
}

fn corrupt_entity_field(home: &Path, ids: &[EntityId], field: &str) {
    let path = home.join("entities");
    #[cfg(windows)]
    let path = dunce::simplified(&path);
    let entities = zvec_rust::Collection::open(path.to_str().expect("UTF-8 fixture path"), None)
        .expect("entities");
    for id in ids {
        let mut doc = entities.fetch(&[id.as_str()]).expect("entity").remove(0);
        doc.add_string(field, "not JSON").expect("corrupt field");
        assert_eq!(entities.upsert(&[&doc]).expect("write body").error_count, 0);
    }
    entities.flush().expect("persist bodies");
}
