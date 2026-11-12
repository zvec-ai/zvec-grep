use super::*;
use crate::pipelines::indexing::resolution_tests::{index_source, open};
use crate::storage::graph::ImportReference;
use serde_json::json;
use std::sync::Arc;

#[test]
fn import_names_follow_syntax_instead_of_text_separators() {
    let directory = tempfile::tempdir().expect("fixture");
    let store = open(directory.path());
    for (raw, reexport, expected) in [
        (
            "import { original as local, plain } from './api';",
            false,
            json!({"local":"original", "plain":"plain"}),
        ),
        (
            "import { type Foo as Local, /* comma, as */ run } from './api';",
            false,
            json!({"Local":"Foo", "run":"run"}),
        ),
        (
            "import Default, * as api from './api';",
            false,
            json!({"Default":"default", "api":"*"}),
        ),
        (
            "import type Default from './api';",
            false,
            json!({"Default":"default"}),
        ),
        (
            "import api = require('./api');",
            false,
            json!({"api":"default"}),
        ),
        (
            "export { original /* comment */ as\n publicName } from './api';",
            true,
            json!({"publicName":"original"}),
        ),
        (
            "{ original: local, plain } = require('./api')",
            false,
            json!({"local":"original", "plain":"plain"}),
        ),
        ("const api = require('./api');", false, json!({"api":"*"})),
        ("api = require('./api')", false, json!({"api":"*"})),
        ("import './api';", false, json!({})),
    ] {
        let mut caches = RuleCaches::default();
        caches.import_references.insert(
            1,
            Arc::from([ImportReference {
                specifier: "./api".into(),
                raw_text: Some(raw.into()),
            }]),
        );
        let imports = javascript_imports(&store, &mut caches, 1).expect("parse imports");
        assert_eq!(imports.len(), 1, "{raw}");
        assert_eq!(imports[0].reexport, reexport, "{raw}");
        assert_eq!(
            serde_json::to_value(&imports[0].names).expect("serialize import names"),
            expected,
            "{raw}"
        );
        assert!(Arc::ptr_eq(
            &imports,
            &javascript_imports(&store, &mut caches, 1).expect("cached imports")
        ));
    }
    for raw in [None, Some("import { invalid from './api'")] {
        let mut caches = RuleCaches::default();
        caches.import_references.insert(
            1,
            Arc::from([ImportReference {
                specifier: "./api".into(),
                raw_text: raw.map(str::to_owned),
            }]),
        );
        assert!(
            javascript_imports(&store, &mut caches, 1)
                .expect("parse incomplete evidence")
                .is_empty()
        );
    }
}

#[test]
fn go_import_directory_uses_aliases_and_the_nearest_module_boundary() {
    let directory = tempfile::tempdir().expect("fixture");
    let store = open(directory.path());
    let file = index_source(
        &store,
        "nested/client/use.go",
        "package client\nfunc Run() {}\n",
    )
    .get();
    let snapshot = store.resolution_snapshot(None).expect("snapshot");
    for (specifier, raw, qualifier, expected) in [
        (
            "example.com/service/v2",
            "import \"example.com/service/v2\"",
            "service",
            Some("nested"),
        ),
        (
            "example.com/service/v2/lib",
            "import \"example.com/service/v2/lib\"",
            "lib",
            Some("nested/lib"),
        ),
        (
            "example.com/service/v2/lib",
            "renamed `example.com/service/v2/lib`",
            "renamed",
            Some("nested/lib"),
        ),
        (
            "example.com/service/v2/lib",
            "renamed \"example.com/service/v2/lib\"",
            "lib",
            None,
        ),
        (
            "example.com/service/v20/lib",
            "import \"example.com/service/v20/lib\"",
            "lib",
            None,
        ),
        (
            "example.com/root/lib",
            "import \"example.com/root/lib\"",
            "lib",
            None,
        ),
        ("external/lib", "import \"external/lib\"", "lib", None),
        (
            "example.com/service/v2/lib",
            "_ \"example.com/service/v2/lib\"",
            "lib",
            None,
        ),
        (
            "example.com/service/v2/lib",
            ". \"example.com/service/v2/lib\"",
            "lib",
            None,
        ),
    ] {
        let mut caches = RuleCaches {
            source_root: Some(directory.path().into()),
            ..RuleCaches::default()
        };
        caches
            .go_modules
            .insert(String::new(), Some("example.com/root".into()));
        caches
            .go_modules
            .insert("nested".into(), Some("example.com/service/v2".into()));
        caches.import_references.insert(
            file,
            Arc::from([ImportReference {
                specifier: specifier.into(),
                raw_text: Some(raw.into()),
            }]),
        );
        assert_eq!(
            go_import_directory(&store, &snapshot, &mut caches, file, qualifier)
                .expect("resolve import directory")
                .as_deref(),
            expected,
            "{raw}: {qualifier}"
        );
    }
}

fn member_call(store: &IndexStore, snapshot: &ResolutionSnapshot, owner: &str) -> StoredPendingRef {
    let owner = &snapshot.lookup_candidates(owner).expect("indexed owner")[0].entity_id;
    store
        .list_pending_refs(100, 0, snapshot)
        .expect("pending calls")
        .refs
        .into_iter()
        .find(|reference| reference.reference.owner_id == *owner && reference.name_tail == "run")
        .expect("receiver call")
}

#[test]
fn materialized_types_keep_factory_origin_and_require_explicit_await() {
    let directory = tempfile::tempdir().expect("fixture");
    let store = open(directory.path());
    let origin = index_source(&store, "api.ts", "export class Widget { run() {} }\nexport function make(): Promise<Widget> { throw new Error(); }\nexport const create = make;\n").get();
    let file = index_source(&store, "use.ts", "import { create } from './api';\nfunction invoke(x: unknown) { x.run(); }\nfunction shadow(create: unknown, x: unknown) { x.run(); }\n").get();
    let snapshot = store
        .resolution_snapshot(None)
        .expect("resolution snapshot");
    let mut caches = RuleCaches::default();
    let stored = member_call(&store, &snapshot, "invoke");
    for (ty, expected_type, expected_file) in [
        ("Widget", "Widget", file),
        ("Promise<Widget>", "Promise<Widget>", file),
        ("Awaited<Promise<Widget>>", "Widget", file),
        ("Awaited<Widget>", "Widget", file),
        ("ReturnType<typeof create>", "Promise<Widget>", origin),
        ("Awaited<ReturnType<typeof create>>", "Widget", origin),
    ] {
        let binding = Binding {
            ty: ty.into(),
            file_id: file,
        };
        let result = materialize_type(&store, &snapshot, &stored, &mut caches, &binding)
            .expect("materialize type")
            .expect(ty);
        assert_eq!(
            (result.ty.as_str(), result.file_id),
            (expected_type, expected_file),
            "{ty}"
        );
    }
    for (context, ty) in [
        (stored, "ReturnType<typeof missing>"),
        (
            member_call(&store, &snapshot, "shadow"),
            "ReturnType<typeof create>",
        ),
    ] {
        let binding = Binding {
            ty: ty.into(),
            file_id: file,
        };
        assert!(
            materialize_type(&store, &snapshot, &context, &mut caches, &binding)
                .expect("reject unsupported type evidence")
                .is_none(),
            "{ty}"
        );
    }
}
