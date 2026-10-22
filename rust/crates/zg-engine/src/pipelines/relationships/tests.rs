use crate::storage::test_support::node;
use std::{collections::BTreeMap, fs, path::PathBuf, sync::Arc, time::Duration};

use tokio_util::sync::CancellationToken;

use crate::{
    EngineError, EngineRuntimeSnapshot, ZvecGrep,
    api::relationships::{RelationshipDirection, RelationshipOptions},
    domain::{
        CodeMetadata, Content, ContentKind, EdgeProvenance, Entity, EntityFragment, EntityId,
        EntityMetadata, FTS_CONFIG, FileEdge, FileGraphResult, FileIndexStatus, FileRecord,
        FileSnapshot, FragmentId, GraphEdgeKind, IndexDescriptor, IndexState, PendingRef,
        PendingRefStatus, Range, ScanRules, SourcePath, SymbolType, TextRange, Workspace,
        model::{EmbeddingMetric, EmbeddingModelInfo, ModelInfo},
    },
    pipelines::{indexed_search::writer::WriterSession, indexing::service::WorkspaceIndexService},
    storage::{
        IndexStore,
        graph::{OpenMode, Resolution, SqliteGraphStorage},
        types::{IndexedFragment, WorkspaceIndexStorageOptions},
    },
    workspace::{
        CURRENT_INDEX_VERSION,
        lock::{LockMode, acquire_home_lock},
        manifest::{WorkspaceManifest, read_workspace_manifest, write_workspace_manifest},
    },
};

struct Fixture {
    _directory: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    graph_path: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("workspace");
        let root = directory.path().canonicalize().expect("canonical root");
        let home = root.join(".zvec-grep");
        let mut manifest = WorkspaceManifest::new(
            Workspace {
                name: "relationship-fixture".into(),
                root: root.clone(),
                scan: ScanRules::default(),
                index: IndexState::Enabled(Box::new(
                    IndexDescriptor::new(schema(), BTreeMap::new(), FTS_CONFIG)
                        .expect("index descriptor"),
                )),
                created_epoch_ms: 1,
                updated_epoch_ms: 1,
            },
            home.clone(),
            Some(CURRENT_INDEX_VERSION),
            BTreeMap::new(),
        )
        .expect("manifest");
        manifest.storage_generation = Some(uuid::Uuid::new_v4().to_string());
        let storage_home = manifest.storage_home();
        fs::create_dir_all(&storage_home).expect("generation");
        let storage = IndexStore::open(WorkspaceIndexStorageOptions::ReadWrite {
            storage_path: storage_home.clone(),
            embeddings: vec![schema()],
        })
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
        write_workspace_manifest(&home, &manifest).expect("publish manifest");
        Self {
            _directory: directory,
            root,
            home,
            graph_path,
        }
    }

    fn storage_home(&self) -> &std::path::Path {
        self.graph_path
            .parent()
            .expect("graph")
            .parent()
            .expect("generation")
    }

    fn open_store(&self) -> IndexStore {
        crate::storage::test_support::open_store(self.storage_home(), false, schema())
    }

    fn add_symbols(
        &self,
        path: &str,
        definitions: &[(&str, Option<&str>, usize, &str)],
    ) -> Vec<Entity> {
        let store = self.open_store();
        let entities = write_symbols(&store, path, definitions);
        store.close().expect("publish symbols");
        entities
    }

    fn options(&self, symbol: &str) -> RelationshipOptions {
        RelationshipOptions {
            root: Some(self.root.clone()),
            symbol: symbol.into(),
            ..RelationshipOptions::default()
        }
    }
}

fn schema() -> EmbeddingModelInfo {
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

fn write_symbols(
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

fn edge(source: &Entity, target: &str, line: usize) -> FileEdge {
    FileEdge {
        kind: GraphEdgeKind::Calls,
        source: source.id().as_str().into(),
        target: target.into(),
        line: Some(line),
        column: Some(4),
        provenance: EdgeProvenance::FileLocal,
        metadata: BTreeMap::new(),
    }
}

fn pending(source: &Entity, name: &str, line: usize) -> PendingRef {
    PendingRef {
        owner_id: source.id().as_str().into(),
        ref_name: name.into(),
        receiver_name: None,
        ref_kind: GraphEdgeKind::Calls,
        arity: None,
        line,
        column: 8,
        status: PendingRefStatus::Pending,
        metadata: BTreeMap::new(),
    }
}

fn write_graph(path: &std::path::Path, local: &[Entity], remote: &Entity) {
    let mut graph = SqliteGraphStorage::open(path, OpenMode::ReadWrite).expect("graph writer");
    let source = &local[2];
    let missing_id = EntityId::new(
        source.file_id(),
        &Content::Text("missing".into()),
        Range::Full,
    )
    .expect("missing target identity");
    let mut edges: Vec<_> = (1..=25)
        .map(|line| edge(source, local[0].id().as_str(), line))
        .chain((30..=52).map(|line| edge(source, local[1].id().as_str(), line)))
        .collect();
    edges.push(edge(source, missing_id.as_str(), 99));
    let mut contains = edge(source, local[0].id().as_str(), 100);
    contains.kind = GraphEdgeKind::Contains;
    edges.push(contains);
    graph
        .write_file_graph(
            source.file_id().get(),
            &FileGraphResult {
                nodes: local
                    .iter()
                    .map(|entity| node(entity.id().as_str()))
                    .chain(std::iter::once(node(missing_id.as_str())))
                    .collect(),
                edges,
                pending_refs: vec![
                    pending(source, "remote", 101),
                    pending(source, "unresolved", 102),
                ],
            },
            &[],
        )
        .expect("local callsites and pending refs");
    graph
        .write_file_graph(
            remote.file_id().get(),
            &FileGraphResult {
                nodes: vec![node(remote.id().as_str())],
                ..FileGraphResult::default()
            },
            &[],
        )
        .expect("remote target");
    let reference = graph
        .list_pending_refs(10, 0)
        .expect("pending references")
        .refs
        .into_iter()
        .find(|reference| reference.reference.ref_name == "remote")
        .expect("remote reference");
    graph
        .apply_resolutions(&[Resolution {
            ref_id: reference.id,
            target_id: remote.id().as_str().into(),
            provenance: EdgeProvenance::ImportScoped,
        }])
        .expect("resolve cross-file call");
    graph.close().expect("close graph writer");
}

#[tokio::test]
async fn exact_definitions_keep_scopes_order_and_per_symbol_limits() {
    let fixture = Fixture::new();
    let engine = ZvecGrep::new();
    let groups = engine
        .callers(fixture.options(" run "))
        .await
        .expect("callers")
        .matches;
    assert_eq!(
        groups.len(),
        3,
        "exact names exclude content-only FTS matches"
    );
    assert_eq!(
        groups
            .iter()
            .map(|group| group.symbol.name.as_str())
            .collect::<Vec<_>>(),
        ["red::run", "blue::run", "run"]
    );
    assert_eq!(
        groups
            .iter()
            .map(|group| group.truncated)
            .collect::<Vec<_>>(),
        [true, true, false]
    );
    assert_eq!(
        groups
            .iter()
            .map(|group| group.edges.len())
            .collect::<Vec<_>>(),
        [20, 20, 1]
    );
    assert_eq!(groups[0].symbol.file_path, PathBuf::from("a.rs"));
    assert_eq!(
        (groups[0].symbol.start_line, groups[0].symbol.end_line),
        (2, 2)
    );
    assert_eq!(groups[1].symbol.start_line, 10);
    assert_eq!(groups[2].symbol.file_path, PathBuf::from("z.rs"));
    assert_eq!(
        groups[0].edges[0].symbol.as_ref().expect("caller").name,
        "invoke"
    );
    assert_eq!(
        (groups[0].edges[0].line, groups[0].edges[0].column),
        (Some(1), Some(4))
    );
    assert_eq!(
        groups[0].edges[19].line,
        Some(20),
        "keep distinct callsites"
    );

    engine
        .enable_read_session_cache()
        .expect("enable read cache");
    assert_eq!(
        engine
            .callers(fixture.options("run"))
            .await
            .expect("cached relationship read")
            .matches,
        groups,
    );

    let limited = engine
        .callers(RelationshipOptions {
            limit: Some(2),
            ..fixture.options("run")
        })
        .await
        .expect("limited groups")
        .matches;
    assert_eq!(
        limited
            .iter()
            .map(|group| group.edges.len())
            .collect::<Vec<_>>(),
        [2, 2, 1]
    );
    assert!(limited[0].truncated);
    let scoped = engine
        .callers(fixture.options("blue::run"))
        .await
        .expect("scoped lookup")
        .matches;
    assert_eq!(scoped.len(), 1);
    assert_eq!(scoped[0].symbol.name, "blue::run");
    assert!(scoped[0].truncated);
    assert_eq!(
        engine.runtime_snapshot(),
        EngineRuntimeSnapshot::default(),
        "relationships do not load embedding models"
    );
    engine.close();
}

#[tokio::test]
async fn callees_keep_repeated_sites_and_cross_file_edges_but_omit_missing_endpoints() {
    let fixture = Fixture::new();
    let engine = ZvecGrep::new();
    let groups = engine
        .callees(RelationshipOptions {
            limit: Some(100),
            ..fixture.options("invoke")
        })
        .await
        .expect("callees")
        .matches;
    assert_eq!(groups.len(), 1);
    let group = &groups[0];
    assert!(!group.truncated);
    assert_eq!(group.edges.len(), 49);
    assert_eq!(
        group
            .edges
            .iter()
            .filter(|edge| edge
                .symbol
                .as_ref()
                .is_some_and(|symbol| symbol.name == "red::run"))
            .count(),
        25
    );
    assert_eq!(
        group
            .edges
            .iter()
            .filter(|edge| edge
                .symbol
                .as_ref()
                .is_some_and(|symbol| symbol.name == "blue::run"))
            .count(),
        23
    );
    assert!(group.edges.iter().all(|edge| edge.line != Some(99)));
    let remote = group
        .edges
        .iter()
        .find(|edge| edge.line == Some(101))
        .expect("resolved cross-file edge");
    assert_eq!(remote.column, Some(8));
    assert_eq!(
        remote.file_path,
        PathBuf::from("a.rs"),
        "outgoing call position belongs to the caller, not the remote callee"
    );
    assert_eq!(
        remote
            .symbol
            .as_ref()
            .expect("remote description")
            .file_path,
        PathBuf::from("z.rs")
    );
    assert!(group.edges.iter().all(|edge| edge.line != Some(102)));
    engine.close();
}

#[tokio::test]
async fn missing_graph_errors_without_creating_a_database() {
    let fixture = Fixture::new();
    fs::remove_file(&fixture.graph_path).expect("remove graph fixture");
    let engine = ZvecGrep::new();
    for error in [
        engine
            .callers(fixture.options("run"))
            .await
            .expect_err("missing graph"),
        engine
            .callees(fixture.options("run"))
            .await
            .expect_err("missing graph"),
    ] {
        assert_eq!(error.code(), EngineError::NOT_FOUND);
        assert!(error.to_string().to_lowercase().contains("graph"));
        assert!(
            !fixture.graph_path.exists(),
            "relationship reads never initialize SQLite"
        );
    }
    engine.close();
}

#[tokio::test]
async fn lock_timeouts_cancellation_and_closed_engines_reject_reads() {
    let fixture = Fixture::new();
    let engine = ZvecGrep::new();
    let writer = acquire_home_lock(&fixture.home, LockMode::Write, "relationships.test")
        .expect("hold writer lock");
    let blocked = engine
        .callers(RelationshipOptions {
            lock_timeout_ms: Some(0),
            ..fixture.options("run")
        })
        .await
        .expect_err("locked read");
    assert_eq!(blocked.code(), EngineError::RESOURCE_BUSY);
    let signal = CancellationToken::new();
    let options = RelationshipOptions {
        signal: Some(signal.clone()),
        ..fixture.options("run")
    };
    let (cancelled, ()) = tokio::join!(engine.callees(options), async {
        tokio::time::sleep(Duration::from_millis(10)).await;
        signal.cancel();
    });
    assert_eq!(
        cancelled.expect_err("cancel waiting read").code(),
        EngineError::CANCELLED
    );
    drop(writer);
    assert_eq!(
        engine
            .callers(fixture.options("run"))
            .await
            .expect("reader recovers after failed admission")
            .matches
            .len(),
        3
    );
    engine.close();
    assert_eq!(
        engine
            .callers(fixture.options("run"))
            .await
            .expect_err("closed callers")
            .code(),
        EngineError::RESOURCE_CLOSED
    );
    assert_eq!(
        engine
            .callees(fixture.options("run"))
            .await
            .expect_err("closed callees")
            .code(),
        EngineError::RESOURCE_CLOSED
    );
}

#[tokio::test]
async fn invalid_queries_fail_before_opening_a_workspace() {
    let directory = tempfile::tempdir().expect("unindexed workspace");
    let engine = ZvecGrep::new();
    for (symbol, limit) in [
        (String::new(), None),
        ("   ".into(), None),
        ("scope::".into(), None),
        ("::".into(), None),
        ("x".repeat(1025), None),
        ("run".into(), Some(0)),
    ] {
        let options = RelationshipOptions {
            root: Some(directory.path().to_path_buf()),
            symbol,
            limit,
            ..RelationshipOptions::default()
        };
        assert_eq!(
            engine
                .callers(options.clone())
                .await
                .expect_err("invalid callers query")
                .code(),
            EngineError::INVALID_ARGUMENT,
        );
    }
    let missing = engine
        .callers(RelationshipOptions {
            root: Some(directory.path().to_path_buf()),
            symbol: "run".into(),
            ..RelationshipOptions::default()
        })
        .await
        .expect_err("index required");
    assert!(missing.message().contains("zg --index"));
    assert!(!directory.path().join(".zvec-grep").exists());
    engine.close();
}

#[tokio::test]
async fn unfinished_cross_file_endpoints_are_filtered_before_limit() {
    for deleting in [false, true] {
        let fixture = Fixture::new();
        let store = fixture.open_store();
        let remote = store
            .list_files()
            .expect("files")
            .into_iter()
            .find(|file| file.relative_path().to_path_buf() == std::path::Path::new("z.rs"))
            .expect("fixture value");
        store
            .set_test_file_status(
                remote.id(),
                if deleting {
                    FileIndexStatus::Deleting
                } else {
                    FileIndexStatus::NotIndexed
                },
            )
            .expect("unfinished canonical file");
        store.close().expect("persist unfinished state");
        let groups = ZvecGrep::new()
            .callees(RelationshipOptions {
                limit: Some(100),
                ..fixture.options("invoke")
            })
            .await
            .expect("query surviving edges")
            .matches;
        assert!(!groups[0].truncated);
        assert_eq!(groups[0].edges.len(), 48);
        assert!(
            groups[0]
                .edges
                .iter()
                .all(|edge| edge.line != Some(101) && edge.line != Some(99))
        );
        let groups = ZvecGrep::new()
            .callers(fixture.options("run"))
            .await
            .expect("query visible definitions")
            .matches;
        assert_eq!(
            groups.len(),
            2,
            "unfinished definition must not become a query root"
        );
    }
}

#[tokio::test]
async fn truncation_counts_only_visible_edges_and_handles_exact_limits() {
    let fixture = Fixture::new();
    let engine = ZvecGrep::new();
    for (limit, count, truncated) in [
        (1, 1, true),
        (48, 48, true),
        (49, 49, false),
        (50, 49, false),
    ] {
        let groups = engine
            .callees(RelationshipOptions {
                limit: Some(limit),
                ..fixture.options("invoke")
            })
            .await
            .expect("limit after visibility filtering")
            .matches;
        assert_eq!(
            (groups[0].edges.len(), groups[0].truncated),
            (count, truncated),
            "limit={limit}"
        );
        if count == 49 {
            assert!(groups[0].edges.iter().any(|edge| edge.line == Some(101)));
        }
    }
    engine.close();
}

#[tokio::test]
async fn paged_relationships_skip_invisible_pages_in_both_directions() {
    let fixture = Fixture::new();
    let entities = fixture.add_symbols(
        "paged.rs",
        &[
            ("paged_caller", None, 1, "fn paged_caller() {}"),
            ("paged_target", None, 2, "fn paged_target() {}"),
        ],
    );
    let graph = rusqlite::Connection::open(&fixture.graph_path).expect("graph fixture");
    // More than one full page has no visible endpoint. Query cursors must still advance.
    for (source, target) in [
        (entities[0].id().as_str(), "missing"),
        ("missing", entities[1].id().as_str()),
    ] {
        graph
            .execute(
                "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x + 1 FROM n WHERE x < 300)
             INSERT INTO edges (file_id, source, target, kind, status, provenance, line)
             SELECT ?1, ?2, ?3, 'calls', 'resolved', 'file_local', x FROM n",
                rusqlite::params![entities[0].file_id().get(), source, target],
            )
            .expect("invisible pages");
    }
    for _ in 0..3 {
        insert_call(
            &fixture,
            entities[0].file_id().get(),
            entities[0].id().as_str(),
            entities[1].id().as_str(),
        );
    }
    drop(graph);
    let engine = ZvecGrep::new();
    for (limit, count, truncated) in [(1, 1, true), (3, 3, false), (4, 3, false)] {
        for incoming in [false, true] {
            let options = RelationshipOptions {
                limit: Some(limit),
                ..fixture.options(if incoming {
                    "paged_target"
                } else {
                    "paged_caller"
                })
            };
            let reply = if incoming {
                engine.callers(options).await
            } else {
                engine.callees(options).await
            }
            .expect("advance across invisible pages");
            assert_eq!(
                (reply.matches[0].edges.len(), reply.matches[0].truncated),
                (count, truncated)
            );
            assert!(
                reply.matches[0]
                    .edges
                    .iter()
                    .all(|edge| edge.line == Some(7))
            );
        }
    }
    engine.close();
}

#[tokio::test]
async fn limited_relationships_stop_after_a_page_with_enough_visible_edges() {
    let fixture = Fixture::new();
    let graph = rusqlite::Connection::open(&fixture.graph_path).expect("graph fixture");
    graph
        .execute_batch(
            "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x + 1 FROM n WHERE x < 300)
        INSERT INTO edges (file_id, source, target, kind, status, line, col, provenance)
        SELECT e.file_id, e.source, e.target, 'calls', 'resolved', 1000 + x, 4, 'file_local'
        FROM n CROSS JOIN (SELECT file_id, source, target FROM edges
            WHERE kind = 'calls' AND provenance = 'file_local' ORDER BY id LIMIT 1) e;",
        )
        .expect("more than one page of visible edges");
    graph.execute_batch("PRAGMA ignore_check_constraints = ON;
        UPDATE edges SET metadata = 'not JSON' WHERE id = (SELECT max(id) FROM edges WHERE kind = 'calls' AND status = 'resolved');")
        .expect("corrupt an edge beyond the first page");
    drop(graph);
    let engine = ZvecGrep::new();
    let reply = engine
        .callees(RelationshipOptions {
            limit: Some(1),
            ..fixture.options("invoke")
        })
        .await
        .expect("stop after the first page supplies the visible lookahead");
    assert_eq!(reply.matches[0].edges.len(), 1);
    assert!(reply.matches[0].truncated);
    assert!(
        engine
            .callees(RelationshipOptions {
                limit: Some(1000),
                ..fixture.options("invoke")
            })
            .await
            .is_err(),
        "reading the corrupt edge still reports corruption"
    );
    engine.close();
}

#[tokio::test]
async fn limited_relationships_skip_unused_endpoint_metadata_within_a_page() {
    let fixture = Fixture::new();
    let extra = fixture.add_symbols("extra.rs", &[("extra", None, 1, "fn extra() {}")]);
    let store = fixture.open_store();
    let caller = store
        .find_symbols("invoke", None)
        .expect("caller")
        .remove(0);
    store.close().expect("close");
    insert_call(
        &fixture,
        caller.file.id().get(),
        caller.entity.id().as_str(),
        extra[0].id().as_str(),
    );
    crate::storage::test_support::corrupt_entity_identity(
        fixture.storage_home(),
        &[extra[0].id().clone()],
    );
    let engine = ZvecGrep::new();
    let reply = engine
        .callees(RelationshipOptions {
            limit: Some(1),
            ..fixture.options("invoke")
        })
        .await
        .expect("unused endpoints within the first SQL page are not loaded");
    assert_eq!(reply.matches[0].edges.len(), 1);
    assert!(reply.matches[0].truncated);
    assert!(
        engine
            .callees(RelationshipOptions {
                limit: Some(1000),
                ..fixture.options("invoke")
            })
            .await
            .is_err(),
        "reading the corrupt endpoint still fails"
    );
    engine.close();
}

fn insert_call(fixture: &Fixture, file_id: u32, source: &str, target: &str) {
    let graph = rusqlite::Connection::open(&fixture.graph_path).expect("graph fixture");
    graph.execute(
        "INSERT INTO edges (file_id, source, target, kind, status, reference_name, line, col, provenance)
         VALUES (?, ?, ?, 'calls', 'resolved', 'run', 7, 4, 'import_scoped')",
        rusqlite::params![file_id, source, target],
    ).expect("persist call fixture");
}

#[tokio::test]
async fn file_level_callers_keep_their_paths_without_symbol_metadata() {
    let fixture = Fixture::new();
    let store = fixture.open_store();
    let target = store
        .find_symbols("run", Some("red"))
        .expect("target")
        .remove(0);
    let sources: Vec<_> = store
        .list_files()
        .expect("files")
        .into_iter()
        .filter(|file| matches!(file.relative_path().to_str(), Some("c.rs" | "z.rs")))
        .collect();
    assert_eq!(sources.len(), 2);
    store.close().expect("close writer");
    for source in sources {
        insert_call(
            &fixture,
            source.id().get(),
            &format!("f{}", source.id()),
            target.entity.id().as_str(),
        );
    }
    let engine = ZvecGrep::new();
    let groups = engine
        .callers(RelationshipOptions {
            limit: Some(100),
            ..fixture.options("red::run")
        })
        .await
        .expect("incoming file calls")
        .matches;
    assert_eq!(groups[0].edges.len(), 27);
    assert!(!groups[0].truncated);
    let mut paths: Vec<_> = groups[0]
        .edges
        .iter()
        .filter(|edge| edge.symbol.is_none())
        .map(|edge| {
            assert_eq!((edge.line, edge.column), (Some(7), Some(4)));
            edge.file_path.clone()
        })
        .collect();
    paths.sort();
    assert_eq!(paths, [PathBuf::from("c.rs"), PathBuf::from("z.rs")]);
    engine.close();
}

#[tokio::test]
async fn limited_queries_do_not_decode_unselected_caller_bodies() {
    let fixture = Fixture::new();
    let storage_home = fixture.storage_home();
    let store = fixture.open_store();
    let target = store
        .find_symbols("run", Some("red"))
        .expect("target")
        .remove(0);
    let unselected = store
        .find_symbols("helper", Some("red"))
        .expect("caller")
        .remove(0);
    store.close().expect("close writer");
    insert_call(
        &fixture,
        unselected.file.id().get(),
        unselected.entity.id().as_str(),
        target.entity.id().as_str(),
    );
    crate::storage::test_support::corrupt_entity_bodies(
        storage_home,
        std::slice::from_ref(unselected.entity.id()),
    );
    let engine = ZvecGrep::new();
    let groups = engine
        .callers(RelationshipOptions {
            limit: Some(1),
            ..fixture.options("red::run")
        })
        .await
        .expect("only hydrate the selected caller")
        .matches;
    assert!(groups[0].truncated);
    assert_eq!(groups[0].edges.len(), 1);
    assert_eq!(groups[0].edges[0].file_path, PathBuf::from("a.rs"));
    assert!(
        engine
            .callers(RelationshipOptions {
                limit: Some(100),
                ..fixture.options("red::run")
            })
            .await
            .is_err(),
        "including the corrupt body still reports corruption"
    );
    engine.close();
}

#[tokio::test]
async fn common_names_are_bounded_after_scope_filtering_without_fts_fallback() {
    let fixture = Fixture::new();
    let common: Vec<_> = (1..=120)
        .map(|line| {
            (
                "new",
                Some(if line <= 100 { "Other" } else { "Rest" }),
                line,
                "fn new() {}",
            )
        })
        .collect();
    fixture.add_symbols("many.rs", &common);
    let foo = fixture.add_symbols(
        "foo.rs",
        &[
            ("helper", Some("Foo"), 5, "fn helper() { new(); }"),
            ("new", Some("Foo"), 10, "fn new() {}"),
        ],
    );
    insert_call(
        &fixture,
        foo[0].file_id().get(),
        foo[0].id().as_str(),
        foo[1].id().as_str(),
    );
    let engine = ZvecGrep::new();
    let scoped = engine
        .callers(fixture.options("Foo::new"))
        .await
        .expect("exact scoped definition")
        .matches;
    assert_eq!(scoped.len(), 1);
    assert_eq!(scoped[0].symbol.name, "Foo::new");
    assert_eq!(scoped[0].edges.len(), 1);
    assert!(!scoped[0].truncated);
    assert_eq!(
        scoped[0].edges[0].symbol.as_ref().expect("caller").name,
        "Foo::helper"
    );
    let others = engine
        .callers(fixture.options("Other::new"))
        .await
        .expect("all scoped definitions")
        .matches;
    assert_eq!(others.len(), 100);
    assert!(others.iter().all(|group| group.symbol.name == "Other::new"));
    let error = engine
        .callees(RelationshipOptions {
            limit: Some(1),
            ..fixture.options("new")
        })
        .await
        .expect_err("too many definitions even with a small edge limit");
    assert_eq!(error.code(), EngineError::INVALID_ARGUMENT);
    assert!(
        error
            .message()
            .contains("more than 100 indexed definitions")
    );
    let suggestions = engine
        .callers(fixture.options("MissingScope::new"))
        .await
        .expect("many exact-name candidates do not cause an overflow error");
    assert!(suggestions.matches.is_empty());
    assert_eq!(suggestions.suggestions.len(), 5);
    assert!(
        suggestions
            .suggestions
            .iter()
            .all(|symbol| symbol.name.ends_with("::new"))
    );
    engine.close();
}

#[tokio::test]
async fn relationship_queries_borrow_incremental_writers_even_when_registered_late() {
    let fixture = Fixture::new();
    let indexing = WorkspaceIndexService::new();
    let manifest = read_workspace_manifest(&fixture.home)
        .expect("manifest")
        .expect("index");
    let lock =
        Arc::new(acquire_home_lock(&fixture.home, LockMode::Write, "index").expect("writer lock"));
    let storage = IndexStore::open(WorkspaceIndexStorageOptions::ReadWrite {
        storage_path: manifest.storage_home(),
        embeddings: vec![schema()],
    })
    .expect("writer storage");
    let options = RelationshipOptions {
        lock_timeout_ms: Some(2_000),
        ..fixture.options("red::run")
    };
    let (groups, writer) = tokio::join!(
        super::service::query(&indexing, &options, None, RelationshipDirection::Callers),
        async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            indexing.writers.register(
                WriterSession::new(storage, Vec::new(), manifest, lock),
                true,
            )
        }
    );
    let groups = groups
        .expect("borrow newly registered writer while home lock remains held")
        .matches;
    assert_eq!(groups.len(), 1);
    assert!(groups[0].truncated);
    let outgoing = RelationshipOptions {
        lock_timeout_ms: Some(0),
        ..fixture.options("invoke")
    };
    let groups = super::service::query(&indexing, &outgoing, None, RelationshipDirection::Callees)
        .await
        .expect("borrow active writer without waiting for workspace lock")
        .matches;
    assert!(groups[0].truncated);
    let signal = CancellationToken::new();
    signal.cancel();
    let cancelled = RelationshipOptions {
        signal: Some(signal),
        ..outgoing.clone()
    };
    let error = super::service::query(&indexing, &cancelled, None, RelationshipDirection::Callees)
        .await
        .expect_err("cancel before borrowing");
    assert_eq!(error.code(), EngineError::CANCELLED);
    writer.retire().await;
    let error = super::service::query(&indexing, &outgoing, None, RelationshipDirection::Callees)
        .await
        .expect_err("retired writer cannot be borrowed");
    assert_eq!(error.code(), EngineError::RESOURCE_BUSY);
    writer.session.storage.close().expect("close writer");
}

#[tokio::test]
async fn relationship_queries_never_borrow_unpublished_generations() {
    let fixture = Fixture::new();
    let indexing = WorkspaceIndexService::new();
    let manifest = read_workspace_manifest(&fixture.home)
        .expect("manifest")
        .expect("index");
    let lock = Arc::new(
        acquire_home_lock(&fixture.home, LockMode::Write, "rebuild").expect("writer lock"),
    );
    for registered in [false, true] {
        let mut unpublished = manifest.clone();
        unpublished.storage_generation = Some(uuid::Uuid::new_v4().to_string());
        fs::create_dir_all(unpublished.storage_home()).expect("rebuild directory");
        let storage = IndexStore::open(WorkspaceIndexStorageOptions::ReadWrite {
            storage_path: unpublished.storage_home(),
            embeddings: vec![schema()],
        })
        .expect("unpublished storage");
        let writer = indexing.writers.register(
            WriterSession::new(storage, Vec::new(), unpublished, Arc::clone(&lock)),
            registered,
        );
        let options = RelationshipOptions {
            lock_timeout_ms: Some(0),
            ..fixture.options("run")
        };
        let error =
            super::service::query(&indexing, &options, None, RelationshipDirection::Callers)
                .await
                .expect_err("do not query an unpublished index even if registered");
        assert_eq!(error.code(), EngineError::RESOURCE_BUSY);
        writer.retire().await;
        writer.session.storage.close().expect("close rebuild");
    }
}

#[tokio::test]
async fn definitions_without_text_locations_do_not_suppress_suggestions() {
    let fixture = Fixture::new();
    let store = fixture.open_store();
    for (scope, range) in [
        ("Full", Range::Full),
        (
            "Bytes",
            Range::Byte(crate::domain::ByteRange::new(0, 20).expect("byte range")),
        ),
    ] {
        let mut entities = write_symbols(
            &store,
            &format!("{scope}.rs"),
            &[("unlocated", Some(scope), 1, "fn unlocated() {}")],
        );
        let entity = &mut entities[0];
        let id = EntityId::new(entity.file_id(), entity.content(), range).expect("identity");
        let fragments = vec![EntityFragment {
            id: FragmentId::new(&id, 0),
            range: Range::Full,
        }];
        *entity = Entity::new(
            id,
            entity.file_id(),
            range,
            entity.content().clone(),
            entity.metadata().cloned(),
            fragments,
        )
        .expect("unlocated entity");
        let file = store
            .list_files()
            .expect("files")
            .into_iter()
            .find(|file| file.id() == entity.file_id())
            .expect("owner");
        let fragment = IndexedFragment {
            entity_id: entity.id().clone(),
            fragment_id: entity.fragments()[0].id.clone(),
            model: schema().model().reference(),
            vector: vec![1.0, 0.0, 0.0],
            fts_text: "unlocated".into(),
        };
        store
            .replace_file(&file, &entities, &[fragment])
            .expect("definition without text coordinates");
    }
    store.close().expect("publish definitions");
    let engine = ZvecGrep::new();
    for query in ["unlocated", "Full::unlocated", "Bytes::unlocated"] {
        let result = engine
            .callers(fixture.options(query))
            .await
            .expect("definitions without text locations are valid stored data");
        assert!(result.matches.is_empty());
        assert!(result.suggestions.is_empty());
    }
    engine.close();
    fixture.add_symbols(
        "suggested.rs",
        &[(
            "unlocated_extra",
            None,
            1,
            "fn unlocated_extra() { /* unlocated */ }",
        )],
    );
    let engine = ZvecGrep::new();
    for query in [
        "unlocated",
        "Full::unlocated",
        "Bytes::unlocated",
        "Wrong::unlocated",
    ] {
        let result = engine
            .callers(fixture.options(query))
            .await
            .expect("suggest usable definitions");
        assert!(result.matches.is_empty(), "{query}");
        assert_eq!(result.suggestions.len(), 1, "{query}");
        assert_eq!(result.suggestions[0].name, "unlocated_extra", "{query}");
    }
    engine.close();

    // A usable exact definition still takes precedence over similarly named suggestions.
    fixture.add_symbols("located.rs", &[("unlocated", None, 1, "fn unlocated() {}")]);
    let engine = ZvecGrep::new();
    let result = engine
        .callers(fixture.options("unlocated"))
        .await
        .expect("usable exact match");
    assert_eq!(result.matches.len(), 1);
    assert_eq!(
        result.matches[0].symbol.file_path,
        PathBuf::from("located.rs")
    );
    assert!(result.suggestions.is_empty());
    engine.close();
}

#[tokio::test]
async fn suggestions_use_definition_names_rank_similarity_and_count_distinct_definitions() {
    let fixture = Fixture::new();
    fixture.add_symbols("other.rs", &[("new", Some("Other"), 1, "fn new() {}")]);
    fixture.add_symbols(
        "foo.rs",
        &[("helper", Some("Foo"), 1, "fn helper() { new(); }")],
    );
    let builders: Vec<_> = (1..=8)
        .map(|line| {
            (
                "newBuilder",
                Some("Foo"),
                line,
                "fn newBuilder() { /* builder */ }",
            )
        })
        .collect();
    fixture.add_symbols("builders.rs", &builders);
    let names: Vec<_> = [
        "get",
        "user",
        "name",
        "ge",
        "helper",
        "wrap_get_user_name",
        "get_user_name_extra",
        "GET_USER_NAME",
    ]
    .into_iter()
    .enumerate()
    .map(|(i, name)| (name, None, i + 1, "get_user_name ge_more"))
    .collect();
    fixture.add_symbols("names.rs", &names);
    let engine = ZvecGrep::new();
    // Prefix order is significant; equally ranked remaining candidates may vary.
    for (query, prefix, rest, count) in [
        ("needle", &[][..], &[][..], 0),
        ("red::needle", &[][..], &[][..], 0),
        ("Foo::new", &["Other::new"][..], &[][..], 1),
        ("absentneedle", &[][..], &[][..], 0),
        (
            "RED::RUN",
            &[][..],
            &["red::run", "blue::run", "run"][..],
            3,
        ),
        ("WrongScope::BUILDER", &[][..], &["Foo::newBuilder"][..], 5),
        (
            "get_user_name",
            &["GET_USER_NAME", "get_user_name_extra", "wrap_get_user_name"][..],
            &["get", "user", "name"][..],
            5,
        ),
        ("ge_more", &[][..], &[][..], 0),
    ] {
        let result = engine
            .callers(RelationshipOptions {
                limit: Some(1),
                ..fixture.options(query)
            })
            .await
            .expect("suggestions");
        assert!(result.matches.is_empty(), "{query}");
        let names: Vec<_> = result
            .suggestions
            .iter()
            .map(|symbol| symbol.name.as_str())
            .collect();
        assert_eq!(names.len(), count, "{query}");
        assert!(names.starts_with(prefix), "{query}: {names:?}");
        assert!(
            names[prefix.len()..].iter().all(|name| rest.contains(name)),
            "{query}: {names:?}"
        );
        // Every definition has two fragments: five results must still be five definitions.
        let unique: std::collections::HashSet<_> = result
            .suggestions
            .iter()
            .map(|s| (&s.file_path, s.start_line, &s.name))
            .collect();
        assert_eq!(unique.len(), count, "{query}");
        let wire = serde_json::to_value(&result).expect("wire");
        assert!(
            wire["suggestions"]
                .as_array()
                .expect("suggestions")
                .iter()
                .all(|s| s.get("edges").is_none() && s.get("truncated").is_none())
        );
        if let Some(candidate) = result.suggestions.first() {
            let retried = engine
                .callers(fixture.options(&candidate.name))
                .await
                .expect("retry exact name");
            assert!(!retried.matches.is_empty());
            assert!(retried.suggestions.is_empty());
        }
    }
    engine.close();
}

#[tokio::test]
async fn wrong_scope_suggests_exact_names_even_when_fts_is_full_of_callers() {
    let fixture = Fixture::new();
    let store = fixture.open_store();
    let callers: Vec<_> = (1..=120)
        .map(|line| ("caller", Some("Calls"), line, "fn caller() { frob(); }"))
        .collect();
    write_symbols(&store, "callers.rs", &callers);
    let body = format!("fn frob() {{ /* {} */ }}", "padding ".repeat(1000));
    let target = write_symbols(&store, "bar.rs", &[("frob", Some("Bar"), 1, &body)]);
    let hits = store.search_fts("frob", 100, None).expect("FTS candidates");
    assert_eq!(hits.len(), 100);
    assert!(
        hits.iter().all(|hit| &hit.entity_id != target[0].id()),
        "fixture reproduces FTS starvation"
    );
    store.close().expect("publish fixture");
    let engine = ZvecGrep::new();
    let result = engine
        .callers(fixture.options("Foo::frob"))
        .await
        .expect("wrong scope suggestions");
    assert!(result.matches.is_empty());
    assert_eq!(result.suggestions.len(), 1);
    assert_eq!(result.suggestions[0].name, "Bar::frob");
    engine.close();
}

#[tokio::test]
async fn symbol_spellings_select_expected_scopes() {
    let fixture = Fixture::new();
    fixture.add_symbols(
        "scopes.rs",
        &[
            ("main", None, 1, "fn main() {}"),
            ("main", Some("Foo"), 2, "fn main() {}"),
            ("f", Some("ns"), 3, "fn f() {}"),
            ("empty_scope", Some(""), 4, "fn empty_scope() {}"),
            ("bar", Some("ns::Foo"), 5, "fn bar() {}"),
            ("bar", Some("pkg.Foo"), 6, "fn bar() {}"),
        ],
    );
    let engine = ZvecGrep::new();
    for (query, expected) in [
        ("::main", &["main"][..]),
        ("main", &["main", "Foo::main"][..]),
        ("::ns::f", &["ns::f"][..]),
        ("ns::f", &["ns::f"][..]),
        ("::empty_scope", &["empty_scope"][..]),
        ("red.run", &["red::run"][..]),
        ("red::run", &["red::run"][..]),
        ("ns::Foo::bar", &["ns::Foo::bar"][..]),
        ("ns.Foo.bar", &["ns::Foo::bar"][..]),
        ("ns::Foo.bar", &["ns::Foo::bar"][..]),
        ("::ns.Foo.bar", &["ns::Foo::bar"][..]),
        ("pkg::Foo::bar", &["pkg.Foo::bar"][..]),
        ("pkg.Foo.bar", &["pkg.Foo::bar"][..]),
        ("pkg.Foo::bar", &["pkg.Foo::bar"][..]),
    ] {
        let result = engine
            .callers(fixture.options(query))
            .await
            .expect("lookup");
        assert_eq!(
            result
                .matches
                .iter()
                .map(|m| m.symbol.name.as_str())
                .collect::<Vec<_>>(),
            expected,
            "{query}"
        );
        assert!(result.suggestions.is_empty());
    }
    let error = engine
        .callers(fixture.options("::"))
        .await
        .expect_err("empty name");
    assert_eq!(error.message(), "symbol name must not be empty");
    engine.close();
}

#[tokio::test]
async fn path_filters_disambiguate_common_scoped_and_top_level_names_before_the_cap() {
    let fixture = Fixture::new();
    let store = fixture.open_store();
    for index in 0..=100 {
        write_symbols(
            &store,
            &format!("pkg{index}/main.rs"),
            &[
                ("setup", Some("tests"), 1, "fn setup() {}"),
                ("main", None, 2, "fn main() {}"),
            ],
        );
    }
    store.close().expect("close writer");
    let engine = ZvecGrep::new();
    for name in ["tests::setup", "::main"] {
        let error = engine
            .callers(fixture.options(name))
            .await
            .expect_err("ambiguous across files");
        assert!(error.message().contains("path filter"));
        for path in ["pkg1", "pkg1/main.rs", "./pkg1"] {
            let options = RelationshipOptions {
                path: Some(path.into()),
                ..fixture.options(name)
            };
            let result = engine
                .callers(options.clone())
                .await
                .expect("filtered callers");
            assert_eq!(
                result.matches.len(),
                1,
                "directory must not match pkg10 etc"
            );
            assert_eq!(
                result.matches[0].symbol.file_path,
                PathBuf::from("pkg1/main.rs")
            );
        }
    }
    let suggestions = engine
        .callers(RelationshipOptions {
            path: Some("pkg99".into()),
            ..fixture.options("wrong::setup")
        })
        .await
        .expect("path-scoped exact-name suggestions");
    assert_eq!(suggestions.suggestions.len(), 1);
    assert_eq!(
        suggestions.suggestions[0].file_path,
        PathBuf::from("pkg99/main.rs")
    );
    for path in ["", "/tmp/main.rs", "../main.rs", "pkg1/../pkg2", "."] {
        let error = engine
            .callers(RelationshipOptions {
                path: Some(path.into()),
                ..fixture.options("main")
            })
            .await
            .expect_err("invalid relative filter");
        assert_eq!(error.code(), EngineError::INVALID_ARGUMENT);
    }
    engine.close();
}

#[tokio::test]
async fn path_filters_exclude_other_files_before_loading_symbol_metadata() {
    let fixture = Fixture::new();
    fixture.add_symbols("keep/main.rs", &[("localize", None, 1, "fn localize() {}")]);
    let outside = fixture.add_symbols(
        "keepers/other.rs",
        &[("localize", None, 1, "fn localize() {}")],
    );
    crate::storage::test_support::corrupt_entity_metadata(
        fixture.storage_home(),
        &[outside[0].id().clone()],
    );
    let engine = ZvecGrep::new();
    assert!(
        engine.callers(fixture.options("localize")).await.is_err(),
        "unfiltered metadata is corrupt"
    );
    for path in ["keep", "keep/main.rs", "missing"] {
        for (name, exact) in [
            ("localize", true),
            ("wrong::localize", false),
            ("LOCALIZE", false),
        ] {
            let result = engine
                .callers(RelationshipOptions {
                    path: Some(path.into()),
                    ..fixture.options(name)
                })
                .await
                .expect("outside metadata must not be fetched");
            let count = usize::from(path != "missing");
            assert_eq!(
                result.matches.len(),
                if exact { count } else { 0 },
                "{path}: {name}"
            );
            assert_eq!(
                result.suggestions.len(),
                if exact { 0 } else { count },
                "{path}: {name}"
            );
        }
    }
    engine.close();
}

#[tokio::test]
async fn malformed_fts_and_unreadable_suggestions_do_not_fail_exact_misses() {
    let fixture = Fixture::new();
    let home = fixture.storage_home();
    let candidate = fixture.add_symbols("frob.rs", &[("frob", None, 1, "fn frob() {}")]);
    crate::storage::test_support::corrupt_entity_bodies(home, &[candidate[0].id().clone()]);
    let engine = ZvecGrep::new();
    for symbol in ["wrong::frob", "FROB", "operator()", "\"", "frob AND ("] {
        let result = engine
            .callers(fixture.options(symbol))
            .await
            .expect("optional suggestions cannot fail the query");
        assert!(result.matches.is_empty());
        assert!(result.suggestions.is_empty());
    }
    assert!(
        engine.callers(fixture.options("frob")).await.is_err(),
        "exact-match corruption remains an error"
    );
    engine.close();
}

#[tokio::test]
async fn waiting_relationship_reads_leave_the_async_worker_free_and_retain_the_writer_lease() {
    let fixture = Fixture::new();
    let indexing = Arc::new(WorkspaceIndexService::new());
    let manifest = read_workspace_manifest(&fixture.home)
        .expect("read manifest")
        .expect("indexed workspace");
    let lock =
        Arc::new(acquire_home_lock(&fixture.home, LockMode::Write, "index").expect("writer lock"));
    let storage = IndexStore::open(WorkspaceIndexStorageOptions::ReadWrite {
        storage_path: manifest.storage_home(),
        embeddings: vec![schema()],
    })
    .expect("writer storage");
    let writer = indexing.writers.register(
        WriterSession::new(storage, Vec::new(), manifest, lock),
        true,
    );
    let session = Arc::clone(&writer.session);
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let blocking_writer = std::thread::spawn(move || {
        session
            .storage
            .with_relationship_read(|_| {
                ready_tx.send(()).expect("signal locked store");
                let _ = release_rx.recv_timeout(Duration::from_secs(3));
                Ok(())
            })
            .expect("hold store lock");
    });
    ready_rx.recv().expect("writer acquired store");
    let options = fixture.options("red::run");
    let query = tokio::spawn(async move {
        super::service::query(&indexing, &options, None, RelationshipDirection::Callers).await
    });
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(
        !query.is_finished(),
        "native read is still waiting for the store"
    );
    // Cancelling the async waiter cannot release the native task's writer lease.
    query.abort();
    assert!(query.await.expect_err("aborted query").is_cancelled());
    assert!(
        tokio::time::timeout(Duration::from_millis(20), writer.retire())
            .await
            .is_err()
    );
    release_tx
        .send(())
        .expect("Tokio worker was not blocked until the writer timed out");
    blocking_writer.join().expect("writer thread");
    tokio::time::timeout(Duration::from_secs(3), writer.retire())
        .await
        .expect("native query releases lease");
    writer.session.storage.close().expect("close writer");
}
