//! Deterministic writes at snapshot boundaries and while an edge is being hydrated.
use super::*;
use crate::{
    EngineResult,
    domain::{FileEdge, FileId, GraphEdgeKind},
    pipelines::relationships::{
        pipeline::query_symbols,
        storage::{RelationshipRead, RelationshipStorage},
    },
    storage::{
        graph::Direction,
        types::{StorageSearchFilter, StorageSearchHit},
    },
};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    sync::mpsc,
};

struct ObservedStore<'a> {
    store: &'a IndexStore,
    after_snapshot: &'a dyn Fn(usize),
    after_edges: &'a dyn Fn(&str),
    snapshots: Cell<usize>,
    endpoint_batches: Cell<usize>,
}

impl<'a> ObservedStore<'a> {
    fn new(
        store: &'a IndexStore,
        after_snapshot: &'a dyn Fn(usize),
        after_edges: &'a dyn Fn(&str),
    ) -> Self {
        Self {
            store,
            after_snapshot,
            after_edges,
            snapshots: Cell::new(0),
            endpoint_batches: Cell::new(0),
        }
    }
}

impl RelationshipStorage for ObservedStore<'_> {
    fn list_file_paths(&self) -> EngineResult<Vec<(FileId, PathBuf)>> {
        self.store.list_file_paths()
    }
    fn has_non_unicode_file_names(&self) -> EngineResult<bool> {
        self.store.has_non_unicode_file_names()
    }
    fn read_snapshot<T>(
        &self,
        operation: impl FnOnce(&dyn RelationshipRead) -> EngineResult<T>,
    ) -> EngineResult<T> {
        let result = self.store.read_snapshot(|view| {
            operation(&ObservedRead {
                view,
                observer: self,
            })
        });
        let count = self.snapshots.get() + 1;
        self.snapshots.set(count);
        (self.after_snapshot)(count);
        result
    }
}

struct ObservedRead<'a> {
    view: &'a dyn RelationshipRead,
    observer: &'a ObservedStore<'a>,
}

impl RelationshipRead for ObservedRead<'_> {
    fn has_graph(&self) -> EngineResult<bool> {
        self.view.has_graph()
    }
    fn find_symbol_entity_ids(
        &self,
        name: &str,
        filter: Option<&StorageSearchFilter>,
    ) -> EngineResult<Vec<EntityId>> {
        self.view.find_symbol_entity_ids(name, filter)
    }
    fn fetch_code_metadata(
        &self,
        ids: &[EntityId],
    ) -> EngineResult<HashMap<EntityId, (FileId, CodeMetadata)>> {
        self.view.fetch_code_metadata(ids)
    }
    fn fetch_entities(&self, ids: &[EntityId]) -> EngineResult<HashMap<EntityId, Entity>> {
        self.view.fetch_entities(ids)
    }
    fn fetch_entity_file_ids(&self, ids: &[EntityId]) -> EngineResult<HashMap<EntityId, FileId>> {
        self.observer
            .endpoint_batches
            .set(self.observer.endpoint_batches.get() + 1);
        self.view.fetch_entity_file_ids(ids)
    }
    fn fetch_files(&self, ids: &[FileId]) -> EngineResult<HashMap<FileId, FileRecord>> {
        self.view.fetch_files(ids)
    }
    fn graph_neighborhood(
        &self,
        id: &str,
        direction: Direction,
        kinds: Option<&[GraphEdgeKind]>,
    ) -> EngineResult<Vec<FileEdge>> {
        let edges = self.view.graph_neighborhood(id, direction, kinds)?;
        (self.observer.after_edges)(id);
        Ok(edges)
    }
    fn search_fts(
        &self,
        query: &str,
        limit: usize,
        filter: Option<&StorageSearchFilter>,
    ) -> EngineResult<Vec<StorageSearchHit>> {
        self.view.search_fts(query, limit, filter)
    }
}

#[test]
fn roots_changed_after_selection_are_skipped_including_same_id_rewrites() {
    for change in ["delete", "body", "metadata", "file"] {
        let fixture = Fixture::new();
        let original = fixture.find_symbol("run", Some(""));
        let store = fixture.open_store();
        let update = |snapshot| {
            if snapshot != 1 {
                return;
            } // Selection ends before the first definition starts.
            match change {
                "delete" => store.delete_file(original.file_id()).expect("delete root"),
                "body" | "metadata" => {
                    let (name, body) = if change == "body" {
                        ("run", "fn run() { changed(); }")
                    } else {
                        ("renamed", "fn run() {}")
                    };
                    let rewritten = write_symbols(&store, "z.rs", &[(name, None, 3, body)]);
                    assert_eq!(rewritten[0].id() == original.id(), change == "metadata");
                }
                "file" => {
                    let mut file = store
                        .list_files()
                        .expect("files")
                        .into_iter()
                        .find(|f| f.id() == original.file_id())
                        .expect("root file");
                    let mut snapshot = file.snapshot().clone();
                    snapshot.modified_epoch_ms = Some(123);
                    file.set_snapshot(snapshot).expect("new file revision");
                    store
                        .update_file_snapshot(&file)
                        .expect("same entity, updated file");
                }
                _ => unreachable!(),
            }
        };
        let observed = ObservedStore::new(&store, &update, &|_| {});
        let result = query_symbols(
            &observed,
            "run",
            Some(""),
            &fixture.options("::run"),
            Direction::In,
        )
        .expect("query");
        assert!(result.matches.is_empty(), "{change}");
        store.close().expect("close store");
    }
}

#[test]
fn writers_run_between_definitions_without_reusing_old_endpoint_descriptions() {
    let fixture = Fixture::new();
    let caller = fixture.find_symbol("invoke", None);
    let first = fixture.find_symbol("run", Some(""));
    let second = fixture
        .add_symbols("b.rs", &[("run", None, 3, "fn run() {}")])
        .remove(0);
    insert_call(
        &fixture,
        caller.file_id().get(),
        caller.id().as_str(),
        second.id().as_str(),
    );
    let store = fixture.open_store();
    let first_id = RefCell::new(None);
    let changed = Cell::new(false);
    let after_edges = |id: &str| {
        first_id.borrow_mut().get_or_insert_with(|| id.to_owned());
    };
    let update = |_| {
        if first_id.borrow().is_none() || changed.replace(true) {
            return;
        }
        let rewritten = write_symbols(
            &store,
            "a.rs",
            &[("renamed_invoke", None, 20, "fn invoke() { run(); }")],
        );
        assert_eq!(
            rewritten[0].id(),
            caller.id(),
            "metadata changes preserve identity"
        );
        for target in [&first, &second] {
            insert_call(
                &fixture,
                caller.file_id().get(),
                caller.id().as_str(),
                target.id().as_str(),
            );
        }
    };
    let observed = ObservedStore::new(&store, &update, &after_edges);
    let result = query_symbols(
        &observed,
        "run",
        Some(""),
        &fixture.options("::run"),
        Direction::In,
    )
    .expect("query");
    assert!(changed.get(), "writer completed between definitions");
    assert_eq!(result.matches.len(), 2);
    let old_path = if first_id.borrow().as_deref() == Some(first.id().as_str()) {
        "z.rs"
    } else {
        "b.rs"
    };
    for group in result.matches {
        let expected = if group.symbol.file_path == std::path::Path::new(old_path) {
            "invoke"
        } else {
            "renamed_invoke"
        };
        assert_eq!(
            group.edges[0]
                .symbol
                .as_ref()
                .expect("caller metadata")
                .name,
            expected
        );
    }
    store.close().expect("close store");
}

#[test]
fn writer_waits_for_all_endpoint_batches_and_next_snapshot_drops_removed_calls() {
    let fixture = Fixture::new();
    let caller = fixture.find_symbol("invoke", None);
    let target = fixture.find_symbol("run", Some(""));
    {
        let graph = rusqlite::Connection::open(&fixture.graph_path).expect("fixture graph");
        graph
            .execute("DELETE FROM edges WHERE target = ?", [target.id().as_str()])
            .expect("replace fixture edges");
        for index in 0..64 {
            // Force several endpoint batches before reaching the real caller.
            graph.execute("INSERT INTO edges (file_id, source, target, kind, status, line, col, provenance) VALUES (?, ?, ?, 'calls', 'resolved', 7, 4, 'file_local')",
                rusqlite::params![caller.file_id().get(), format!("missing-{index}"), target.id().as_str()]).expect("invisible edge");
        }
    }
    insert_call(
        &fixture,
        caller.file_id().get(),
        caller.id().as_str(),
        target.id().as_str(),
    );
    let store = Arc::new(fixture.open_store());
    let writer_store = Arc::clone(&store);
    let (start_tx, start_rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let writer = std::thread::spawn(move || {
        start_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("edges read");
        ready_tx.send(()).expect("writer starting");
        write_symbols(
            &writer_store,
            "a.rs",
            &[("replacement", None, 1, "fn replacement() {}")],
        );
        done_tx.send(()).expect("replacement committed");
    });
    let after_edges = |_: &str| {
        start_tx
            .send(())
            .expect("start writer after reading SQLite rows");
        ready_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("writer started");
        assert!(
            matches!(
                done_rx.recv_timeout(Duration::from_millis(30)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ),
            "writer must wait for endpoint hydration"
        );
    };
    let after_snapshot = |number| {
        if number == 2 {
            // The complete per-definition result has been assembled.
            done_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("writer proceeds after the definition");
        }
    };
    let observed = ObservedStore::new(&store, &after_snapshot, &after_edges);
    let options = RelationshipOptions {
        limit: Some(1),
        ..fixture.options("::run")
    };
    let result = query_symbols(&observed, "run", Some(""), &options, Direction::In)
        .expect("coherent old snapshot");
    writer.join().expect("writer thread");
    assert!(
        observed.endpoint_batches.get() > 1,
        "exercise the whole visibility loop"
    );
    let edge = &result.matches[0].edges[0];
    assert_eq!(edge.line, Some(7));
    assert_eq!(
        edge.symbol
            .as_ref()
            .expect("old edge retains old caller metadata")
            .name,
        "invoke"
    );
    let updated = query_symbols(store.as_ref(), "run", Some(""), &options, Direction::In)
        .expect("new snapshot");
    assert!(
        updated.matches[0].edges.is_empty(),
        "completed replacement must remove the old call site"
    );
    store.close().expect("close store");
}
