//! End-to-end resolution over real extraction output.
//!
//! Each fixture is extracted by the real walk-time collector, assembled into a
//! `FileGraphResult` the way the pipeline does, persisted, and then handed to
//! one resolver sweep. The tests assert on the sweep outcome and on the pending
//! queue, so a change in either the stored shape or the rule chain surfaces
//! here rather than only in hand-built graph fixtures.

use crate::domain::model::{EmbeddingMetric, EmbeddingModelInfo, ModelInfo};
use crate::domain::{
    ContentKind, FileId, FileIndexStatus, FileRecord, FileSnapshot, GraphEdgeKind, SourcePath,
    formats_for_path,
};
use crate::extraction::{ChunkOptions, TextSource, extract_for_indexing};
use crate::storage::IndexStore;
use crate::storage::types::{IndexedFragment, WorkspaceIndexStorageOptions};

use super::graph_assembly::assemble_file_graph;
use super::graph_resolver::resolve_pending;
use super::pipeline::bind_entities;

const MODEL: &str = "fixture/fixture-model";

fn open(path: &std::path::Path) -> IndexStore {
    IndexStore::open(WorkspaceIndexStorageOptions::ReadWrite {
        storage_path: path.to_owned(),
        embeddings: vec![
            EmbeddingModelInfo::new(
                ModelInfo::new(
                    "fixture".to_owned(),
                    "fixture-model".to_owned(),
                    [ContentKind::Text, ContentKind::Code, ContentKind::Image],
                    32,
                )
                .expect("valid model metadata"),
                3,
                EmbeddingMetric::Cosine,
                None,
                None,
            )
            .expect("valid embedding metadata"),
        ],
    })
    .expect("open storage")
}

/// Extracts, assembles, and stores one source file exactly as indexing does.
fn index_source(store: &IndexStore, path: &str, text: &str) -> FileId {
    let relative_path = SourcePath::new(path).expect("source path");
    let id = store
        .resolve_file_ids(&[relative_path.to_path_buf()])
        .expect("reserve file ID")[0];
    let formats = formats_for_path(path);
    let source = TextSource {
        relative_path: relative_path.clone(),
        formats: formats.to_vec(),
        text: text.to_owned(),
    };
    let extracted = extract_for_indexing(&source, ChunkOptions::default()).expect("extraction");
    let entities = bind_entities(id, extracted.fragments).expect("bound entities");
    let graph = extracted
        .graph
        .as_ref()
        .map(|graph| {
            let language = formats.first().copied().expect("code format");
            assemble_file_graph(&id.get().to_string(), language, graph, &entities)
        })
        .unwrap_or_default();
    let entries: Vec<IndexedFragment> = entities
        .iter()
        .flat_map(|entity| {
            entity.fragments().iter().map(|fragment| IndexedFragment {
                entity_id: entity.id().clone(),
                fragment_id: fragment.id.clone(),
                model: MODEL.to_owned(),
                vector: vec![1.0, 0.0, 0.0],
                fts_text: text.to_owned(),
            })
        })
        .collect();
    let file = FileRecord::new(
        id,
        relative_path,
        FileSnapshot {
            size_bytes: text.len() as u64,
            modified_epoch_ms: Some(1),
            content_hash: Some(crate::utils::sha256_hex(text.as_bytes())),
        },
        FileIndexStatus::NotIndexed,
    )
    .expect("file record");
    store
        .replace_file_with_graph(&file, &entities, &entries, &graph)
        .expect("store extraction output");
    id
}

#[test]
fn sweep_resolves_relative_import_and_imported_call_from_real_extraction() {
    let home = tempfile::tempdir().expect("workspace");
    let store = open(home.path());
    index_source(&store, "src/utils.ts", "export function helper() {\n}\n");
    index_source(
        &store,
        "src/main.ts",
        "import { helper } from './utils';\n\nfunction run() {\n  helper();\n}\n",
    );
    let sweep = resolve_pending(&store).expect("sweep");
    assert_eq!(sweep.failed, 0, "nothing terminates: {sweep:?}");
    assert!(
        sweep.resolved >= 2,
        "the import and the imported call both bind: {sweep:?}"
    );
    assert!(
        store
            .list_pending_refs(10, 0)
            .expect("queue")
            .refs
            .is_empty(),
        "the workspace leaves no reference pending"
    );
}

#[test]
fn sweep_fails_bare_package_import_from_real_extraction() {
    let home = tempfile::tempdir().expect("workspace");
    let store = open(home.path());
    index_source(
        &store,
        "src/app.ts",
        "import react from 'react';\n\nexport function mount() {\n  react();\n}\n",
    );
    let sweep = resolve_pending(&store).expect("sweep");
    assert_eq!(sweep.failed, 1, "the package import terminates: {sweep:?}");
    // The call through the imported binding has no workspace definition, so it
    // stays queued; only the import itself terminates.
    let page = store.list_pending_refs(10, 0).expect("queue");
    assert!(
        page.refs
            .iter()
            .all(|reference| reference.reference.ref_kind != GraphEdgeKind::Imports),
        "failed references leave the queue"
    );
}

#[test]
fn sweep_keeps_python_module_import_pending_until_its_module_is_indexed() {
    let home = tempfile::tempdir().expect("workspace");
    let store = open(home.path());
    index_source(
        &store,
        "pkg/app.py",
        "import pkg.math\n\n\ndef run():\n  return 1\n",
    );
    let first = resolve_pending(&store).expect("sweep");
    assert_eq!(first.failed, 0, "an in-workspace module path stays queued");
    assert_eq!(first.resolved, 0, "the module is not indexed yet");
    assert_eq!(store.list_pending_refs(10, 0).expect("queue").refs.len(), 1);

    index_source(&store, "pkg/math.py", "def math():\n  return 1\n");
    let second = resolve_pending(&store).expect("sweep");
    assert_eq!(second.failed, 0, "indexing the module clears the queue");
    assert_eq!(second.resolved, 1, "the import binds to the module file");
    assert!(
        store
            .list_pending_refs(10, 0)
            .expect("queue")
            .refs
            .is_empty(),
        "the import binds once its module exists"
    );
}
