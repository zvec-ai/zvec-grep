//! End-to-end resolution over real extraction output.
//!
//! Each fixture is extracted by the real walk-time collector, assembled into a
//! `FileGraphResult` the way the pipeline does, persisted, and then handed to
//! one resolver sweep. The tests assert on the sweep outcome and on the pending
//! queue, so a change in either the stored shape or the rule chain surfaces
//! here rather than only in hand-built graph fixtures.

use crate::EngineError;
use crate::domain::model::{EmbeddingMetric, EmbeddingModelInfo, ModelInfo};
use crate::domain::{
    ContentKind, EntityId, FileId, FileIndexStatus, FileRecord, FileSnapshot, GraphEdgeKind,
    SourcePath, formats_for_path,
};
use crate::extraction::{ChunkOptions, TextSource, extract_for_indexing};
use crate::storage::IndexStore;
use crate::storage::graph::Direction;
use crate::storage::types::{IndexedFragment, WorkspaceIndexStorageOptions};
use tokio_util::sync::CancellationToken;

use super::graph_assembly::assemble_file_graph;
use super::graph_resolver::resolve_pending;
use super::pipeline::bind_entities;

const MODEL: &str = "fixture/fixture-model";

fn open(path: &std::path::Path) -> IndexStore {
    IndexStore::open(WorkspaceIndexStorageOptions::ReadWrite {
        storage_path: path.to_owned(),
        graph_enabled: true,
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
    let formats = formats_for_path(std::path::Path::new(path));
    let source = TextSource {
        relative_path: relative_path.clone(),
        formats: formats.clone(),
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

/// Stores `path` as a `Failed` record, the way a pass that discovers a file but
/// cannot index it does: the record exists, while the path never enters the
/// snapshot's path lookup.
fn fail_source(store: &IndexStore, path: &str) {
    let relative_path = SourcePath::new(path).expect("source path");
    let id = store
        .resolve_file_ids(&[relative_path.to_path_buf()])
        .expect("reserve file ID")[0];
    let file = FileRecord::new(
        id,
        relative_path,
        FileSnapshot {
            size_bytes: 0,
            modified_epoch_ms: Some(1),
            content_hash: Some(crate::utils::sha256_hex(b"")),
        },
        FileIndexStatus::NotIndexed,
    )
    .expect("file record");
    store
        .mark_file_failed(&file, "embedding failed")
        .expect("failed record");
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
    let sweep = resolve_pending(&store, None).expect("sweep");
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

/// Files owning the targets of one owner entity's resolved call edges, read
/// back from the persisted graph.
fn call_target_files(store: &IndexStore, owner: &str) -> Vec<FileId> {
    store
        .read_snapshot(|view| {
            let targets: Vec<EntityId> = view.neighborhood(
                owner,
                Direction::Out,
                Some(&[GraphEdgeKind::Calls]),
                |rows| {
                    rows.map(|edge| edge.map(|edge| EntityId::from_string(edge.target)))
                        .collect()
                },
            )?;
            let metadata = view.fetch_code_metadata(&targets)?;
            Ok(targets
                .into_iter()
                .map(|target| {
                    metadata
                        .get(&target)
                        .expect("call target has entity metadata")
                        .0
                })
                .collect())
        })
        .expect("resolved call targets")
}

/// Owner entity of a file's queued call to `name`, captured before a sweep
/// resolves the row.
fn pending_call_owner(store: &IndexStore, file: FileId, name: &str) -> String {
    store
        .list_pending_refs(1000, 0)
        .expect("queue")
        .refs
        .iter()
        .find(|reference| {
            reference.file_id == file.get()
                && reference.reference.ref_kind == GraphEdgeKind::Calls
                && reference.reference.ref_name == name
        })
        .expect("queued call")
        .reference
        .owner_id
        .clone()
}

/// Owner entity of a file's queued call spelled with `receiver`, captured
/// before a sweep resolves the row.
fn pending_receiver_call_owner(store: &IndexStore, file: FileId, receiver: &str) -> String {
    store
        .list_pending_refs(1000, 0)
        .expect("queue")
        .refs
        .iter()
        .find(|reference| {
            reference.file_id == file.get()
                && reference.reference.ref_kind == GraphEdgeKind::Calls
                && reference.reference.receiver_name.as_deref() == Some(receiver)
        })
        .expect("queued receiver call")
        .reference
        .owner_id
        .clone()
}

/// Files owning the targets of one file's resolved import edges, read back
/// from the persisted graph.
fn import_target_files(store: &IndexStore, file: FileId) -> Vec<FileId> {
    store
        .read_snapshot(|view| {
            let targets: Vec<EntityId> = view.neighborhood(
                &format!("f{}", file.get()),
                Direction::Out,
                Some(&[GraphEdgeKind::Imports]),
                |rows| {
                    rows.map(|edge| edge.map(|edge| EntityId::from_string(edge.target)))
                        .collect()
                },
            )?;
            let metadata = view.fetch_code_metadata(&targets)?;
            Ok(targets
                .into_iter()
                .map(|target| {
                    metadata.get(&target).map_or_else(
                        || panic!("import target {target:?} has no entity metadata"),
                        |entry| entry.0,
                    )
                })
                .collect())
        })
        .expect("resolved import targets")
}

#[test]
fn sweep_resolves_relative_alias_specifiers_per_importing_directory() {
    let home = tempfile::tempdir().expect("workspace");
    let store = open(home.path());
    let left_util = index_source(&store, "a/util.ts", "export function work() {\n}\n");
    let left_main = index_source(
        &store,
        "a/main.ts",
        "import { work as run } from './util';\n\nexport function main() {\n  run();\n}\n",
    );
    let right_util = index_source(&store, "b/util.ts", "export function work() {\n}\n");
    let right_main = index_source(
        &store,
        "b/main.ts",
        "import { work as run } from './util';\n\nexport function main() {\n  run();\n}\n",
    );
    let left_owner = pending_call_owner(&store, left_main, "run");
    let right_owner = pending_call_owner(&store, right_main, "run");

    let sweep = resolve_pending(&store, None).expect("sweep");
    assert_eq!(sweep.failed, 0, "nothing terminates: {sweep:?}");
    assert!(
        store
            .list_pending_refs(10, 0)
            .expect("queue")
            .refs
            .is_empty(),
        "both imports and both alias calls bind: {sweep:?}"
    );
    // Identical relative specifiers under the same alias in two directories:
    // each call must follow the directory of the file that spells it.
    assert_eq!(call_target_files(&store, &left_owner), vec![left_util]);
    assert_eq!(call_target_files(&store, &right_owner), vec![right_util]);
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
    let sweep = resolve_pending(&store, None).expect("sweep");
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
fn sweep_binds_qualified_calls_only_when_the_receiver_spells_the_indexed_scope() {
    let home = tempfile::tempdir().expect("workspace");
    let store = open(home.path());
    index_source(
        &store,
        "src/lib.rs",
        concat!(
            "mod ns {\n",
            "    pub struct Waldo;\n\n",
            "    impl Waldo {\n",
            "        pub fn find() {}\n",
            "    }\n",
            "}\n\n",
            "pub struct Solo;\n\n",
            "impl Solo {\n",
            "    pub fn seek() {}\n",
            "}\n\n",
            "pub fn drive() {\n",
            "    Solo::seek();\n",
            "    crate::Solo::seek();\n",
            "    ns::Waldo::find();\n",
            "    crate::ns::Waldo::find();\n",
            "}\n",
        ),
    );

    let sweep = resolve_pending(&store, None).expect("sweep");
    assert_eq!(sweep.failed, 0, "no call terminates: {sweep:?}");
    // Extraction spells an entity's scope as the module/impl breadcrumb joined
    // with `::`, and a receiver keeps its `::` path minus the member name. One
    // spelling of `crate::ns::Waldo::find()` therefore cannot match: reducing
    // the receiver to its last segment yields `Waldo::find`, while the entity
    // is indexed as `ns::Waldo::find`.
    assert_eq!(
        sweep.qualified_name_match, 3,
        "only receivers the breadcrumb reproduces bind: {sweep:?}"
    );
    assert_eq!(sweep.resolved, 3, "three of the four calls bind: {sweep:?}");
    let page = store.list_pending_refs(10, 0).expect("queue");
    assert_eq!(page.refs.len(), 1, "one call stays pending: {page:?}");
    assert_eq!(page.refs[0].reference.ref_name, "find");
    assert_eq!(
        page.refs[0].reference.receiver_name.as_deref(),
        Some("crate::ns::Waldo")
    );
}

#[test]
fn sweep_binds_python_cls_receiver_to_the_enclosing_class() {
    let home = tempfile::tempdir().expect("workspace");
    let store = open(home.path());
    index_source(
        &store,
        "pkg/widget.py",
        concat!(
            "class Widget:\n",
            "    def build(self):\n",
            "        return Widget()\n\n",
            "    @classmethod\n",
            "    def make(cls):\n",
            "        return cls.build()\n",
        ),
    );

    let sweep = resolve_pending(&store, None).expect("sweep");
    assert_eq!(sweep.failed, 0, "no call terminates: {sweep:?}");
    assert_eq!(
        sweep.file_scoped, 1,
        "cls.build() binds to the enclosing class's member: {sweep:?}"
    );
    assert!(
        store
            .list_pending_refs(10, 0)
            .expect("queue")
            .refs
            .is_empty(),
        "the classmethod call leaves nothing pending"
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
    let first = resolve_pending(&store, None).expect("sweep");
    assert_eq!(first.failed, 0, "an in-workspace module path stays queued");
    assert_eq!(first.resolved, 0, "the module is not indexed yet");
    assert_eq!(store.list_pending_refs(10, 0).expect("queue").refs.len(), 1);

    index_source(&store, "pkg/math.py", "def math():\n  return 1\n");
    let second = resolve_pending(&store, None).expect("sweep");
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

#[test]
fn sweep_prefers_the_nearest_override_over_the_base_declaration() {
    let home = tempfile::tempdir().expect("workspace");
    let store = open(home.path());
    let base = index_source(
        &store,
        "base.py",
        "class Base:\n    def save(self):\n        pass\n",
    );
    let child = index_source(
        &store,
        "child.py",
        concat!(
            "from base import Base\n\n\n",
            "class Child(Base):\n",
            "    def save(self):\n",
            "        pass\n\n",
            "    def run(self):\n",
            "        self.save()\n\n",
            "    def run_super(self):\n",
            "        super().save()\n",
        ),
    );
    // One receiver per function: the call targets are read back per owner, so
    // the two receivers must not share an enclosing function.
    let own = pending_receiver_call_owner(&store, child, "self");
    let parent = pending_receiver_call_owner(&store, child, "super");

    let sweep = resolve_pending(&store, None).expect("sweep");
    assert_eq!(sweep.failed, 0, "nothing terminates: {sweep:?}");
    // `Child::save` and `Base::save` share the short name, so the hierarchy
    // has to be walked nearest first: the override answers `self`, while
    // `super` skips it and reaches the base declaration.
    assert_eq!(
        call_target_files(&store, &own),
        vec![child],
        "the enclosing class's override shadows the base method it replaces"
    );
    assert_eq!(
        call_target_files(&store, &parent),
        vec![base],
        "super() still binds the base declaration"
    );
}

#[test]
fn sweep_keeps_a_c_declaration_when_another_language_defines_the_name() {
    let home = tempfile::tempdir().expect("workspace");
    let store = open(home.path());
    // The Python definition shares only the short name: preferring it would
    // empty the C reference's candidate set, because the language gate runs
    // after the preference and would then discard the Python candidate too.
    index_source(&store, "tool.py", "def compute(x):\n  return x\n");
    let header = index_source(&store, "include/api.h", "int compute(int x);\n");
    let caller = index_source(
        &store,
        "src/app.c",
        "int main(void) {\n  return compute(1);\n}\n",
    );
    let owner = pending_call_owner(&store, caller, "compute");
    let sweep = resolve_pending(&store, None).expect("sweep");
    assert_eq!(sweep.failed, 0, "nothing terminates: {sweep:?}");
    assert_eq!(
        call_target_files(&store, &owner),
        vec![header],
        "the C call binds the only C declaration of its symbol"
    );
}

#[test]
fn sweep_prefers_a_c_definition_over_the_declaration_of_the_same_symbol() {
    let home = tempfile::tempdir().expect("workspace");
    let store = open(home.path());
    index_source(&store, "include/api.h", "int compute(int x);\n");
    let definition = index_source(
        &store,
        "src/impl.c",
        "int compute(int x) {\n  return x;\n}\n",
    );
    let caller = index_source(
        &store,
        "src/app.c",
        "int main(void) {\n  return compute(1);\n}\n",
    );
    let owner = pending_call_owner(&store, caller, "compute");
    let sweep = resolve_pending(&store, None).expect("sweep");
    assert_eq!(sweep.failed, 0, "nothing terminates: {sweep:?}");
    assert_eq!(
        call_target_files(&store, &owner),
        vec![definition],
        "the definition stands for the symbol its prototype names"
    );
}

#[test]
fn sweep_keeps_a_cpp_overload_declaration_from_a_differently_shaped_definition() {
    let home = tempfile::tempdir().expect("workspace");
    let store = open(home.path());
    // A two-parameter prototype and a one-parameter definition share the
    // qualified name `compute` but describe different overloads: letting the
    // definition hide the declaration would leave the caller a single
    // candidate, which the workspace-unique rule binds without checking the
    // argument count.
    let header = index_source(&store, "include/api.hpp", "void compute(int a, int b);\n");
    index_source(&store, "src/impl.cpp", "void compute(int a) {\n}\n");
    let caller = index_source(&store, "src/app.cpp", "void run() {\n  compute(1, 2);\n}\n");
    let owner = pending_call_owner(&store, caller, "compute");
    let sweep = resolve_pending(&store, None).expect("sweep");
    assert_eq!(sweep.failed, 0, "nothing terminates: {sweep:?}");
    assert_eq!(
        call_target_files(&store, &owner),
        vec![header],
        "arity evidence picks the two-argument declaration, not the definition"
    );
}

#[test]
fn sweep_binds_an_overload_by_the_scope_the_receiver_spells() {
    let home = tempfile::tempdir().expect("workspace");
    let store = open(home.path());
    // Two overloads of one class spell one qualified name, so the receiver
    // alone cannot pick between them; the argument count can, because the
    // candidates all belong to the scope the receiver names.
    let header = index_source(
        &store,
        "include/api.hpp",
        concat!(
            "class Api {\n",
            "public:\n",
            "    void run(int value);\n",
            "    void run(int value, int extra);\n",
            "};\n",
        ),
    );
    let caller = index_source(&store, "src/app.cpp", "void drive() {\n  Api::run(1);\n}\n");
    let owner = pending_call_owner(&store, caller, "run");
    let sweep = resolve_pending(&store, None).expect("sweep");
    assert_eq!(sweep.failed, 0, "nothing terminates: {sweep:?}");
    assert_eq!(
        call_target_files(&store, &owner),
        vec![header],
        "the count separates the receiver's own overloads: {sweep:?}"
    );
}

#[test]
fn sweep_stops_before_writing_back_when_the_pass_is_cancelled() {
    let home = tempfile::tempdir().expect("workspace");
    let store = open(home.path());
    let util = index_source(&store, "src/utils.ts", "export function helper() {\n}\n");
    let main = index_source(
        &store,
        "src/main.ts",
        "import { helper } from './utils';\n\nfunction run() {\n  helper();\n}\n",
    );
    let owner = pending_call_owner(&store, main, "helper");
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let error = resolve_pending(&store, Some(&cancelled))
        .expect_err("a cancelled pass must not drain the queue");
    assert_eq!(error.code(), EngineError::CANCELLED);
    assert_eq!(
        store.list_pending_refs(10, 0).expect("queue").refs.len(),
        2,
        "the import and its call stay queued for a later pass"
    );
    assert!(
        call_target_files(&store, &owner).is_empty(),
        "a cancelled sweep writes nothing back"
    );

    // The same sweep under a live token still resolves both references: the
    // boundary check must not change what an uncancelled pass does.
    let signal = CancellationToken::new();
    let sweep = resolve_pending(&store, Some(&signal)).expect("sweep");
    assert_eq!(sweep.failed, 0, "nothing terminates: {sweep:?}");
    assert_eq!(call_target_files(&store, &owner), vec![util]);
}

#[test]
fn sweep_resolves_java_static_import_through_the_member_path() {
    let home = tempfile::tempdir().expect("workspace");
    let store = open(home.path());
    let util = index_source(
        &store,
        "com/acme/Util.java",
        "package com.acme;\n\npublic class Util {\n  public static void work() {\n  }\n}\n",
    );
    index_source(
        &store,
        "com/acme/Helper.java",
        "package com.acme;\n\npublic class Helper {\n  public static void work() {\n  }\n}\n",
    );
    let app = index_source(
        &store,
        "com/acme/App.java",
        "package com.acme;\n\nimport static com.acme.Util.work;\n\nclass App {\n  void run() {\n    work();\n  }\n}\n",
    );
    let owner = pending_call_owner(&store, app, "work");
    let sweep = resolve_pending(&store, None).expect("sweep");
    assert_eq!(sweep.failed, 0, "nothing terminates: {sweep:?}");
    assert_eq!(
        import_target_files(&store, app),
        vec![util],
        "the specifier's member path picks Util over Helper's same-named `work`"
    );

    // The resolved import edge feeds this file's import scope on the next
    // pass, which pins the bare call that the shared name left ambiguous.
    let sweep = resolve_pending(&store, None).expect("second sweep");
    assert_eq!(sweep.failed, 0, "nothing terminates: {sweep:?}");
    assert_eq!(
        call_target_files(&store, &owner),
        vec![util],
        "the imported member's module decides the bare call"
    );
}

#[test]
fn sweep_resolves_java_type_import_to_its_class() {
    let home = tempfile::tempdir().expect("workspace");
    let store = open(home.path());
    let widget = index_source(
        &store,
        "com/acme/Widget.java",
        "package com.acme;\n\npublic class Widget {\n}\n",
    );
    let app = index_source(
        &store,
        "com/acme/App.java",
        "package com.acme;\n\nimport com.acme.Widget;\n\nclass App {\n  Widget make() {\n    return null;\n  }\n}\n",
    );
    let sweep = resolve_pending(&store, None).expect("sweep");
    assert_eq!(sweep.failed, 0, "nothing terminates: {sweep:?}");
    assert_eq!(
        import_target_files(&store, app),
        vec![widget],
        "the package prefix is no obstruction to a type import"
    );
}

#[test]
fn sweep_resolves_java_nested_type_import() {
    let home = tempfile::tempdir().expect("workspace");
    let store = open(home.path());
    let outer = index_source(
        &store,
        "com/acme/Outer.java",
        "package com.acme;\n\npublic class Outer {\n  public static class Inner {\n  }\n}\n",
    );
    let app = index_source(
        &store,
        "com/acme/App.java",
        "package com.acme;\n\nimport com.acme.Outer.Inner;\n\nclass App {\n  Inner make() {\n    return null;\n  }\n}\n",
    );
    let sweep = resolve_pending(&store, None).expect("sweep");
    assert_eq!(sweep.failed, 0, "nothing terminates: {sweep:?}");
    assert_eq!(
        import_target_files(&store, app),
        vec![outer],
        "the specifier's declaring breadcrumb spells the nested class"
    );
}

#[test]
fn sweep_keeps_java_on_demand_import_pending() {
    let home = tempfile::tempdir().expect("workspace");
    let store = open(home.path());
    index_source(
        &store,
        "com/acme/Widget.java",
        "package com.acme;\n\npublic class Widget {\n}\n",
    );
    let app = index_source(
        &store,
        "com/acme/App.java",
        "package com.acme;\n\nimport com.acme.*;\n\nclass App {\n}\n",
    );
    let sweep = resolve_pending(&store, None).expect("sweep");
    assert_eq!(sweep.failed, 0, "nothing terminates: {sweep:?}");
    assert!(
        import_target_files(&store, app).is_empty(),
        "an on-demand import names no single target"
    );
    assert!(
        store
            .list_pending_refs(10, 0)
            .expect("queue")
            .refs
            .iter()
            .any(|reference| reference.file_id == app.get()
                && reference.reference.ref_kind == GraphEdgeKind::Imports),
        "the on-demand import stays queued"
    );
}

#[test]
fn sweep_keeps_a_java_import_of_another_package_pending() {
    let home = tempfile::tempdir().expect("workspace");
    let store = open(home.path());
    // The workspace's only `List` belongs to package `my`. The indexed name
    // carries no package, so the specifier's tail alone would match it and
    // pull an unrelated file into `app`'s import scope.
    index_source(
        &store,
        "my/List.java",
        "package my;\n\npublic class List {\n}\n",
    );
    let app = index_source(
        &store,
        "app/Use.java",
        "package app;\n\nimport java.util.List;\n\nclass Use {\n  List make() {\n    return null;\n  }\n}\n",
    );
    let sweep = resolve_pending(&store, None).expect("sweep");
    assert_eq!(sweep.failed, 0, "nothing terminates: {sweep:?}");
    assert!(
        import_target_files(&store, app).is_empty(),
        "another package's type is not the workspace's List"
    );
    assert!(
        store
            .list_pending_refs(10, 0)
            .expect("queue")
            .refs
            .iter()
            .any(|reference| reference.file_id == app.get()
                && reference.reference.ref_kind == GraphEdgeKind::Imports),
        "the import stays queued until its package is confirmed"
    );
}

#[test]
fn sweep_resolves_a_java_import_below_its_source_root() {
    let home = tempfile::tempdir().expect("workspace");
    let store = open(home.path());
    // The workspace root sits above the Java source root, so the package names
    // only a trailing run of the class's directory.
    let widget = index_source(
        &store,
        "src/main/java/com/acme/Widget.java",
        "package com.acme;\n\npublic class Widget {\n}\n",
    );
    let app = index_source(
        &store,
        "src/main/java/com/acme/App.java",
        "package com.acme;\n\nimport com.acme.Widget;\n\nclass App {\n  Widget make() {\n    return null;\n  }\n}\n",
    );
    let sweep = resolve_pending(&store, None).expect("sweep");
    assert_eq!(sweep.failed, 0, "nothing terminates: {sweep:?}");
    assert_eq!(
        import_target_files(&store, app),
        vec![widget],
        "a source root above the workspace root still spells the package"
    );
}

/// Persisted targets of one file's resolved import edges. A relative
/// specifier binds to the module's file endpoint (`f{file_id}`), so the raw
/// target is the assertion, not an entity file lookup.
fn import_target_ids(store: &IndexStore, file: FileId) -> Vec<String> {
    store
        .read_snapshot(|view| {
            view.neighborhood(
                &format!("f{}", file.get()),
                Direction::Out,
                Some(&[GraphEdgeKind::Imports]),
                |rows| rows.map(|edge| edge.map(|edge| edge.target)).collect(),
            )
        })
        .expect("resolved import targets")
}

#[test]
fn incremental_added_module_requeues_import_preference() {
    let home = tempfile::tempdir().expect("workspace");
    let store = open(home.path());
    let nested = index_source(&store, "src/lib/index.ts", "export function work() {\n}\n");
    let main = index_source(
        &store,
        "src/main.ts",
        "import { work } from './lib';\n\nexport function run() {\n  work();\n}\n",
    );
    let owner = pending_call_owner(&store, main, "work");
    // The first pass has no resolved rows to revisit; it only clears what it
    // accumulated while writing.
    assert_eq!(
        store.requeue_stale_resolutions().expect("requeue"),
        0,
        "a fresh graph holds no resolved references"
    );
    resolve_pending(&store, None).expect("first sweep");
    assert_eq!(
        import_target_ids(&store, main),
        vec![format!("f{}", nested.get())]
    );
    assert_eq!(call_target_files(&store, &owner), vec![nested]);

    // Creating the sibling module makes `./lib` prefer `src/lib.ts`. Neither
    // file that already resolved is rewritten, so only the pass-level requeue
    // can reach those rows.
    let sibling = index_source(&store, "src/lib.ts", "export function work() {\n}\n");
    let requeued = store.requeue_stale_resolutions().expect("requeue");
    assert!(
        requeued >= 2,
        "the import and its scoped call return to the queue: {requeued}"
    );
    resolve_pending(&store, None).expect("second sweep");
    assert_eq!(
        import_target_ids(&store, main),
        vec![format!("f{}", sibling.get())],
        "the shorter module wins once it exists"
    );
    assert_eq!(
        call_target_files(&store, &owner),
        vec![sibling],
        "the call follows the re-resolved import"
    );
}

#[test]
fn incremental_added_duplicate_name_requeues_unique_resolution() {
    let home = tempfile::tempdir().expect("workspace");
    let store = open(home.path());
    let first = index_source(&store, "src/a.ts", "export function work() {\n}\n");
    let caller = index_source(
        &store,
        "src/b.ts",
        "export function run() {\n  work();\n}\n",
    );
    let owner = pending_call_owner(&store, caller, "work");
    store.requeue_stale_resolutions().expect("requeue");
    resolve_pending(&store, None).expect("first sweep");
    assert_eq!(call_target_files(&store, &owner), vec![first]);

    // A second definition of the same name breaks the former unique match
    // without touching any endpoint the first resolution stored.
    index_source(&store, "src/c.ts", "export function work() {\n}\n");
    let requeued = store.requeue_stale_resolutions().expect("requeue");
    assert!(
        requeued >= 1,
        "the unique call returns to the queue: {requeued}"
    );
    let sweep = resolve_pending(&store, None).expect("second sweep");
    assert!(
        call_target_files(&store, &owner).is_empty(),
        "the call no longer binds one workspace-wide definition"
    );
    assert!(
        sweep.candidates_recorded >= 1,
        "the second definition is recorded as ambiguity: {sweep:?}"
    );
}

#[test]
fn incremental_requeue_revives_failed_import() {
    let home = tempfile::tempdir().expect("workspace");
    let store = open(home.path());
    let app = index_source(
        &store,
        "src/app.ts",
        "import react from 'react';\n\nexport function mount() {\n}\n",
    );
    store.requeue_stale_resolutions().expect("requeue");
    let sweep = resolve_pending(&store, None).expect("first sweep");
    assert_eq!(
        sweep.failed, 1,
        "the bare package import terminates: {sweep:?}"
    );
    assert!(
        store
            .list_pending_refs(10, 0)
            .expect("queue")
            .refs
            .iter()
            .all(|reference| reference.reference.ref_kind != GraphEdgeKind::Imports),
        "a failed reference leaves the queue"
    );

    // A pass that changes the module path set requeues failed rows as well:
    // "no workspace module" was only true at the time it was recorded.
    index_source(&store, "src/extra.ts", "export function extra() {\n}\n");
    let requeued = store.requeue_stale_resolutions().expect("requeue");
    assert!(
        requeued >= 1,
        "the failed import returns to the queue: {requeued}"
    );
    assert!(
        store
            .list_pending_refs(10, 0)
            .expect("queue")
            .refs
            .iter()
            .any(|reference| reference.file_id == app.get()
                && reference.reference.ref_kind == GraphEdgeKind::Imports),
        "the terminated import is pending again"
    );
}

#[test]
fn sweep_keeps_an_untyped_receiver_call_pending_when_the_member_name_is_unique() {
    let home = tempfile::tempdir().expect("workspace");
    let store = open(home.path());
    let caller = index_source(
        &store,
        "caller.py",
        "def probe(expr):\n    return expr.match(1)\n",
    );
    index_source(
        &store,
        "unrelated.py",
        concat!(
            "class Other:\n",
            "    def outer(self):\n",
            "        def match(arg):\n",
            "            return arg\n",
            "        return match(0)\n",
        ),
    );

    let receiver_owner = pending_receiver_call_owner(&store, caller, "expr");
    let sweep = resolve_pending(&store, None).expect("sweep");
    // `match` is the only definition of that name in the workspace, but the
    // sweep cannot type `expr`: workspace uniqueness answers a bare name, so
    // the member call must stay pending instead of binding a guess. The
    // same-file bare call in `unrelated.py` bound during partitioning and
    // never reached the queue.
    assert_eq!(
        sweep.resolved, 0,
        "the untyped receiver call must not bind: {sweep:?}"
    );
    assert!(
        call_target_files(&store, &receiver_owner).is_empty(),
        "an untyped receiver call must not bind"
    );
    assert!(
        store
            .list_pending_refs(10, 0)
            .expect("queue")
            .refs
            .iter()
            .any(|queued| queued.file_id == caller.get()
                && queued.reference.ref_name == "match"
                && queued.reference.receiver_name.as_deref() == Some("expr")),
        "the untyped receiver call stays queued"
    );
}

#[test]
fn sweep_binds_a_bare_call_to_its_import_over_a_same_named_member() {
    let home = tempfile::tempdir().expect("workspace");
    let store = open(home.path());
    let json_module = index_source(
        &store,
        "pkg/json/__init__.py",
        "def loads(s, **kwargs):\n    return s\n",
    );
    let tag = index_source(
        &store,
        "pkg/json/tag.py",
        concat!(
            "from ..json import loads\n\n\n",
            "class TaggedJSONSerializer:\n",
            "    def loads(self, value):\n",
            "        return loads(value)\n",
        ),
    );

    let owner = pending_call_owner(&store, tag, "loads");
    let sweep = resolve_pending(&store, None).expect("sweep");
    // `loads` names both the imported module function and the enclosing
    // method. The bare call spells no receiver, so the same-file method is
    // not a shadowing definition and the import decides the target.
    assert_eq!(
        call_target_files(&store, &owner),
        vec![json_module],
        "the bare call binds the imported definition: {sweep:?}"
    );
}

#[test]
fn sweep_keeps_an_untyped_receiver_call_pending_when_several_definitions_exist() {
    let home = tempfile::tempdir().expect("workspace");
    let store = open(home.path());
    let caller = index_source(
        &store,
        "caller.py",
        "def probe(expr):\n    return expr.match(1)\n",
    );
    index_source(
        &store,
        "unrelated.py",
        concat!(
            "class Other:\n",
            "    def outer(self):\n",
            "        def match(arg):\n",
            "            return arg\n",
            "        return match(0)\n",
        ),
    );
    // A second definition leaves the name ambiguous, so the single-hit guard
    // no longer covers the call and the fallbacks are asked to answer a
    // member name without any receiver evidence.
    index_source(&store, "extra.py", "def match(a, b):\n    return a\n");

    let receiver_owner = pending_receiver_call_owner(&store, caller, "expr");
    let sweep = resolve_pending(&store, None).expect("sweep");
    assert_eq!(
        sweep.arity_bound, 0,
        "an argument count must not decide across scopes: {sweep:?}"
    );
    assert_eq!(
        sweep.resolved, 0,
        "the untyped receiver call must not bind: {sweep:?}"
    );
    assert!(
        call_target_files(&store, &receiver_owner).is_empty(),
        "an untyped receiver call must not bind"
    );
    assert!(
        store
            .list_pending_refs(10, 0)
            .expect("queue")
            .refs
            .iter()
            .any(|queued| queued.file_id == caller.get()
                && queued.reference.ref_name == "match"
                && queued.reference.receiver_name.as_deref() == Some("expr")),
        "the untyped receiver call stays queued"
    );
}

#[test]
fn sweep_keeps_a_call_pending_when_an_enclosing_function_shadows_the_import() {
    let home = tempfile::tempdir().expect("workspace");
    let store = open(home.path());
    let module = index_source(&store, "util.py", "def work(x):\n    return x\n");
    let caller = index_source(
        &store,
        "caller.py",
        concat!(
            "from util import work\n\n\n",
            "def outer():\n",
            "    def work(x):\n",
            "        return 2\n\n",
            "    def inner():\n",
            "        return work(0)\n\n",
            "    return inner()\n",
        ),
    );

    let owner = pending_call_owner(&store, caller, "work");
    let sweep = resolve_pending(&store, None).expect("sweep");
    // `outer::work` is a local binding of the enclosing function, not a
    // member reached through a receiver: the bare call inside `inner` reads
    // it, and the equal-arity pair to the import keeps the reference pending.
    assert!(
        call_target_files(&store, &owner).is_empty(),
        "the import must not win over an enclosing binding: {sweep:?}"
    );
    assert_eq!(
        sweep.import_bound, 0,
        "no import decision is taken: {sweep:?}"
    );
    assert!(
        sweep.candidates_recorded >= 1,
        "the shadowed name is recorded as ambiguity: {sweep:?}"
    );
    assert_eq!(
        import_target_ids(&store, caller),
        vec![format!("f{}", module.get())],
        "the import itself still names the module: {sweep:?}"
    );
}

#[test]
fn sweep_prefers_a_python_package_over_a_same_named_module_file() {
    let home = tempfile::tempdir().expect("workspace");
    let store = open(home.path());
    index_source(&store, "pkg.py", "def work(x):\n    return x\n");
    let package = index_source(&store, "pkg/__init__.py", "def work(x):\n    return x\n");
    let caller = index_source(
        &store,
        "main.py",
        "from pkg import work\n\n\ndef run():\n    return work(1)\n",
    );

    let owner = pending_call_owner(&store, caller, "work");
    let sweep = resolve_pending(&store, None).expect("sweep");
    // Python checks the directory package's `__init__` before a same-named
    // module file, so the import and the call behind it both name `pkg`.
    assert_eq!(
        import_target_ids(&store, caller),
        vec![format!("f{}", package.get())],
        "the package entry wins over `pkg.py`: {sweep:?}"
    );
    assert_eq!(
        call_target_files(&store, &owner),
        vec![package],
        "the call follows the import into the package: {sweep:?}"
    );
}

#[test]
fn stale_evidence_requeues_after_a_pass_stops_before_its_sweep() {
    let home = tempfile::tempdir().expect("workspace");
    let store = open(home.path());
    let nested = index_source(&store, "src/lib/index.ts", "export function work() {\n}\n");
    let main = index_source(
        &store,
        "src/main.ts",
        "import { work } from './lib';\n\nexport function run() {\n  work();\n}\n",
    );
    let owner = pending_call_owner(&store, main, "work");
    store.requeue_stale_resolutions().expect("requeue");
    resolve_pending(&store, None).expect("first sweep");
    assert_eq!(
        import_target_ids(&store, main),
        vec![format!("f{}", nested.get())]
    );

    // A pass publishes the sibling module and then fails or is cancelled
    // before its sweep. The earlier import and call keep their stale target,
    // and the file stays indexed, so no later pass rewrites it: only the
    // evidence recorded with that write can repair them.
    let sibling = index_source(&store, "src/lib.ts", "export function work() {\n}\n");
    store.close().expect("close");
    drop(store);

    // A fresh process opens the same index, as the next pass would.
    let store = open(home.path());
    let requeued = store
        .requeue_stale_resolutions()
        .expect("requeue after reopen");
    assert!(
        requeued >= 2,
        "the import and its scoped call return to the queue: {requeued}"
    );
    resolve_pending(&store, None).expect("second sweep");
    assert_eq!(
        import_target_ids(&store, main),
        vec![format!("f{}", sibling.get())],
        "the shorter module wins once it exists"
    );
    assert_eq!(call_target_files(&store, &owner), vec![sibling]);
}

#[test]
fn sweep_rebinds_an_import_after_a_failed_file_becomes_indexed() {
    let home = tempfile::tempdir().expect("workspace");
    let store = open(home.path());
    let nested = index_source(&store, "lib/index.ts", "export function work() {\n}\n");
    let main = index_source(
        &store,
        "main.ts",
        "import { work } from './lib';\n\nexport function run() {\n  work();\n}\n",
    );
    let owner = pending_call_owner(&store, main, "work");
    let sweep = resolve_pending(&store, None).expect("first sweep");
    assert_eq!(sweep.failed, 0, "nothing terminates: {sweep:?}");
    assert_eq!(
        import_target_ids(&store, main),
        vec![format!("f{}", nested.get())],
        "the directory entry is the only `./lib` module so far"
    );

    // The pass that discovers `lib.ts` cannot embed it: the record is stored as
    // `Failed`, which keeps the path out of the snapshot's path lookup while the
    // record itself already exists. Its requeue and sweep change nothing — the
    // specifier still names the directory entry.
    fail_source(&store, "lib.ts");
    store
        .requeue_stale_resolutions()
        .expect("requeue after the failure");
    resolve_pending(&store, None).expect("pass over the failed file");
    assert_eq!(
        import_target_ids(&store, main),
        vec![format!("f{}", nested.get())],
        "a failed file is no module path"
    );

    // Indexing it on a later pass enters the path set. `./lib` now names
    // `lib.ts` before `lib/index.ts`, so the import — and the call that bound
    // through its scope — must be resolved again instead of keeping the old
    // endpoints.
    let flat = index_source(&store, "lib.ts", "export function work() {\n}\n");
    let requeued = store
        .requeue_stale_resolutions()
        .expect("requeue after it is indexed");
    assert!(
        requeued >= 2,
        "the import and its scoped call return to the queue: {requeued}"
    );
    let sweep = resolve_pending(&store, None).expect("second sweep");
    assert_eq!(sweep.failed, 0, "nothing terminates: {sweep:?}");
    assert_eq!(
        import_target_ids(&store, main),
        vec![format!("f{}", flat.get())],
        "the newly indexed sibling wins the specifier"
    );
    assert_eq!(
        call_target_files(&store, &owner),
        vec![flat],
        "the call follows the import's new target"
    );
}

#[test]
fn sweep_rebinds_a_member_call_after_its_intermediate_base_changes() {
    let home = tempfile::tempdir().expect("workspace");
    let store = open(home.path());
    let base_a = index_source(
        &store,
        "a.py",
        "class A:\n    def save(self):\n        pass\n",
    );
    let base_b = index_source(
        &store,
        "b.py",
        "class B:\n    def save(self):\n        pass\n",
    );
    index_source(
        &store,
        "mid.py",
        "from a import A\n\n\nclass Mid(A):\n    pass\n",
    );
    let child = index_source(
        &store,
        "child.py",
        "from mid import Mid\n\n\nclass Child(Mid):\n    def run(self):\n        self.save()\n",
    );
    let owner = pending_receiver_call_owner(&store, child, "self");
    // Imports and inheritance clauses need their own passes before the call
    // can walk the chain.
    for _ in 0..3 {
        store.requeue_stale_resolutions().expect("requeue");
        resolve_pending(&store, None).expect("sweep");
    }
    assert_eq!(
        call_target_files(&store, &owner),
        vec![base_a],
        "the chain reaches A.save"
    );

    // Only the intermediate class moves to another base: neither `save` nor
    // `self` is written anywhere, and neither the call nor its obsolete target
    // belongs to the rewritten file, so only inheritance evidence can return
    // it to the queue.
    index_source(
        &store,
        "mid.py",
        "from b import B\n\n\nclass Mid(B):\n    pass\n",
    );
    let requeued = store
        .requeue_stale_resolutions()
        .expect("requeue after the base swap");
    assert!(
        requeued >= 1,
        "the member call returns to the queue: {requeued}"
    );
    for _ in 0..2 {
        resolve_pending(&store, None).expect("sweep after the base swap");
    }
    assert_eq!(
        call_target_files(&store, &owner),
        vec![base_b],
        "the call follows the replaced base"
    );
}

#[test]
fn sweep_keeps_a_python_member_pending_when_two_bases_declare_it() {
    let home = tempfile::tempdir().expect("workspace");
    let store = open(home.path());
    index_source(
        &store,
        "grand.py",
        "class Grand:\n    def save(self):\n        pass\n",
    );
    index_source(
        &store,
        "a.py",
        "from grand import Grand\n\n\nclass A(Grand):\n    pass\n",
    );
    index_source(
        &store,
        "b.py",
        "class B:\n    def save(self):\n        pass\n",
    );
    let child = index_source(
        &store,
        "child.py",
        "from a import A\nfrom b import B\n\n\nclass Child(A, B):\n    def run(self):\n        self.save()\n",
    );
    let owner = pending_receiver_call_owner(&store, child, "self");
    // Python linearizes the bases: `Child, A, Grand, B` reaches `Grand.save`
    // before `B.save`, while the walk offers `Child, A, B, Grand`. Both bases
    // declare the member, so the winner depends on that order and the call
    // stays pending rather than binding the base the walk reaches first.
    for _ in 0..3 {
        store.requeue_stale_resolutions().expect("requeue");
        resolve_pending(&store, None).expect("sweep");
    }
    assert!(
        call_target_files(&store, &owner).is_empty(),
        "a branched hierarchy without its lookup order binds nothing"
    );
}

#[test]
fn sweep_binds_a_python_member_only_one_base_declares() {
    let home = tempfile::tempdir().expect("workspace");
    let store = open(home.path());
    let base = index_source(
        &store,
        "b.py",
        "class B:\n    def save(self):\n        pass\n",
    );
    index_source(&store, "c.py", "class C:\n    pass\n");
    let child = index_source(
        &store,
        "child.py",
        "from b import B\nfrom c import C\n\n\nclass Child(B, C):\n    def run(self):\n        self.save()\n",
    );
    let owner = pending_receiver_call_owner(&store, child, "self");
    // Only one base declares `save`, so every linearization reaches it and the
    // walk may still bind without knowing the order.
    for _ in 0..3 {
        store.requeue_stale_resolutions().expect("requeue");
        resolve_pending(&store, None).expect("sweep");
    }
    assert_eq!(
        call_target_files(&store, &owner),
        vec![base],
        "the one declaring base wins without an ordering decision"
    );
}
