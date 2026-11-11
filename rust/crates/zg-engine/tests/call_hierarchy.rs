//! Source-to-call-hierarchy acceptance tests adapted from upstream LSP cases.
//! Imported from liulx20/zvec-grep commit 39b65a59c56185142845f635d2c266beae1b6964.
//! These exercise production indexing and expose unsupported resolution cases.
//! Do not seed entities/edges, ignore cases, or treat missing graph data as success.
//! Expected edges contain each source-definition endpoint once. A line array allows
//! any listed call site; definition startLine still distinguishes same-name definitions.

#[allow(dead_code)] // Reuse the existing embedding stub; other storage helpers are unused here.
mod support;

use serde_json::{Value, json};
use std::{fs, path::Path};
use support::{EmbeddingServer, configure_remote_model, index_options};
use zg_engine::{
    ZvecGrep,
    api::{
        index::{IndexResult, options::WorkspaceChange},
        relationships::RelationshipOptions,
    },
};

// Each indexed workspace owns native collections and an embedding server.
// Bound memory use even when the test runner exposes hundreds of CPU threads.
static WORKSPACES: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);

struct Case {
    name: &'static str,
    files: Vec<SourceFile>,
    queries: Vec<Query>,
}

struct SourceFile {
    path: &'static str,
    source: &'static str,
}

struct Query {
    operation: &'static str,
    symbol: &'static str,
    // Key response fields only. startLine distinguishes overloads in the same file.
    expected: Value,
}

fn retain_expected_fields(actual: &mut Value, expected: &Value) {
    match (actual, expected) {
        (Value::Object(actual), Value::Object(expected)) => {
            actual.retain(|key, _| expected.contains_key(key));
            for (key, expected) in expected {
                if let Some(actual) = actual.get_mut(key) {
                    retain_expected_fields(actual, expected);
                }
            }
        }
        (Value::Array(actual), Value::Array(expected)) => {
            // Keep array lengths intact so extra/missing definitions and edges still fail.
            for (actual, expected) in actual.iter_mut().zip(expected) {
                retain_expected_fields(actual, expected);
            }
        }
        _ => {}
    }
}

fn normalize_calls(groups: &mut Value) {
    // Ignore column conventions from upstream LSP fixtures, but check call lines.
    // Preserve definition coordinates, endpoints, groups and duplicate edges.
    for group in groups.as_array_mut().expect("relationship matches array") {
        let edges = group["edges"]
            .as_array_mut()
            .expect("relationship edges array");
        for edge in edges.iter_mut() {
            let edge = edge.as_object_mut().expect("relationship edge");
            edge.remove("column");
        }
        edges.sort_by_cached_key(Value::to_string);
    }
}

fn accept_expected_line_choices(actual: &Value, expected: &mut Value) {
    // Match definitions and endpoints before allowing alternate call lines.
    // Never relax identity checks or remove duplicate actual relationships.
    let same_definition = |a: &Value, b: &Value| {
        ["name", "filePath", "startLine"]
            .iter()
            .all(|key| a[*key] == b[*key])
    };
    for group in expected.as_array_mut().expect("expected groups") {
        let Some(actual_group) = actual
            .as_array()
            .expect("actual groups")
            .iter()
            .find(|candidate| same_definition(candidate, group))
        else {
            continue;
        };
        for edge in group["edges"].as_array_mut().expect("expected edges") {
            let Some(lines) = edge["line"].as_array() else {
                continue;
            };
            if let Some(actual_edge) = actual_group["edges"]
                .as_array()
                .expect("actual edges")
                .iter()
                .find(|candidate| same_definition(&candidate["symbol"], &edge["symbol"]))
                && lines.contains(&actual_edge["line"])
            {
                edge["line"] = actual_edge["line"].clone();
            }
        }
    }
}

#[test]
fn line_choices_preserve_identity_single_sites_and_edge_counts() {
    let expected = json!([{
        "name": "caller", "filePath": "a.cc", "startLine": 1,
        "edges": [
            {"symbol": {"name": "work", "filePath": "a.cc", "startLine": 2}, "line": [7, 8]},
            {"symbol": {"name": "work", "filePath": "b.cc", "startLine": 2}, "line": 10}
        ]
    }]);
    for line in [json!(7), json!(8), json!(9), Value::Null] {
        let mut actual = expected.clone();
        actual[0]["edges"][0]["line"] = line.clone();
        let mut selected = expected.clone();
        accept_expected_line_choices(&actual, &mut selected);
        assert_eq!(actual == selected, line == 7 || line == 8);
    }
    for mutation in 0..5 {
        let mut actual = expected.clone();
        actual[0]["edges"][0]["line"] = json!(8);
        match mutation {
            0 => actual[0]["edges"][1]["line"] = json!(11),
            1 => actual[0]["edges"][0]["symbol"]["filePath"] = json!("b.cc"),
            2 => actual[0]["edges"][0]["symbol"]["startLine"] = json!(3),
            3 => actual[0]["startLine"] = json!(3),
            _ => {
                let duplicate = actual[0]["edges"][0].clone();
                actual[0]["edges"]
                    .as_array_mut()
                    .expect("relationship edges array")
                    .push(duplicate);
            }
        }
        let mut selected = expected.clone();
        accept_expected_line_choices(&actual, &mut selected);
        assert_ne!(actual, selected, "mutation {mutation} must still fail");
    }
}

async fn check_case(case: Case) {
    let _permit = WORKSPACES.acquire().await.expect("workspace permit");
    let directory = tempfile::tempdir().expect("source workspace");
    let root = directory.path().canonicalize().expect("canonical root");
    for file in &case.files {
        let path = root.join(file.path);
        fs::create_dir_all(path.parent().expect("source directory")).expect("create directory");
        fs::write(path, file.source).expect("write upstream source");
    }
    // Only embeddings are stubbed. Extraction, indexing, graph resolution and
    // relationship queries must all use production code; no graph rows are seeded.
    let server = EmbeddingServer::start().expect("local embedding server");
    configure_remote_model(&root, server.address).expect("embedding configuration");
    let engine = ZvecGrep::new();
    let mut options = index_options(&root);
    options.graph_indexing = Some(true);
    options.rebuild = true;
    options.scan.globs = Some(case.files.iter().map(|file| file.path.into()).collect());
    let indexed = engine.index(options).await.expect("index source files");
    assert_eq!(indexed.files_failed, 0, "{}: {indexed:?}", case.name);
    assert_eq!(
        indexed.files_added,
        case.files.len(),
        "{}: {indexed:?}",
        case.name
    );
    assert!(indexed.skipped.is_empty(), "{}: {indexed:?}", case.name);

    check_queries(&engine, &root, case.name, &case.queries).await;
    engine.close();
}

async fn check_queries(engine: &ZvecGrep, root: &Path, stage: &str, queries: &[Query]) {
    for query in queries {
        let options = RelationshipOptions {
            root: Some(root.to_path_buf()),
            symbol: query.symbol.into(),
            ..RelationshipOptions::default()
        };
        let result = match query.operation {
            "callers" => engine.callers(options).await,
            "callees" => engine.callees(options).await,
            _ => panic!("invalid query operation"),
        };
        let actual = result.unwrap_or_else(|error| {
            panic!("{}: {} {}: {error}", stage, query.operation, query.symbol)
        });
        let mut actual =
            serde_json::to_value(actual.matches).expect("serialize relationship response");
        let mut expected = query.expected.clone();
        accept_expected_line_choices(&actual, &mut expected);
        retain_expected_fields(&mut actual, &expected);
        normalize_calls(&mut actual);
        normalize_calls(&mut expected);
        assert_eq!(
            actual, expected,
            "{}: {} {}",
            stage, query.operation, query.symbol
        );
    }
}

// https://github.com/rust-lang/rust-analyzer/blob/3131751eaf1c3b5da6e618acdc2148ad7db65bc7/crates/ide/src/call_hierarchy.rs
// Upstream case: test_call_hierarchy_in_same_fn.
// https://github.com/rust-lang/rust-analyzer/blob/3131751eaf1c3b5da6e618acdc2148ad7db65bc7/crates/ide/src/call_hierarchy.rs
// Upstream case: test_call_hierarchy_outgoing. Strip rust-analyzer cursor/file markers. Split
// upstream grouped call ranges into one edge per site; definition ranges cover full functions.
#[tokio::test]
async fn rust_repeated_calls() {
    check_case(Case {
        name: "rust_repeated_calls",
        files: vec![SourceFile {
            path: "lib.rs",
            source: "fn callee() {}\nfn caller() {\n    callee();\n    callee();\n}\n",
        }],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "callee",
                expected: json!([
                    {"name": "callee", "filePath": "lib.rs", "startLine": 1,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "lib.rs", "startLine": 2}, "line": [3, 4], "column": 4},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([
                    {"name": "caller", "filePath": "lib.rs", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "callee", "filePath": "lib.rs", "startLine": 1}, "line": [3, 4], "column": 4},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "caller",
                expected: json!([
                    {"name": "caller", "filePath": "lib.rs", "startLine": 2,
                     "edges": []},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "callee",
                expected: json!([
                    {"name": "callee", "filePath": "lib.rs", "startLine": 1,
                     "edges": []},
                ]),
            },
        ],
    })
    .await;
}

// https://github.com/rust-lang/rust-analyzer/blob/3131751eaf1c3b5da6e618acdc2148ad7db65bc7/crates/ide/src/call_hierarchy.rs
// Upstream case: test_call_hierarchy_in_different_files.
// https://github.com/rust-lang/rust-analyzer/blob/3131751eaf1c3b5da6e618acdc2148ad7db65bc7/crates/ide/src/call_hierarchy.rs
// Upstream case: test_call_hierarchy_outgoing_in_different_files. Preserve the imported canonical
// definition and its foo scope; the import itself is not a calls edge.
#[tokio::test]
async fn rust_cross_file() {
    check_case(Case {
        name: "rust_cross_file",
        files: vec![
            SourceFile {
                path: "lib.rs",
                source: "mod foo;\nuse foo::callee;\n\nfn caller() {\n    callee();\n}\n\n",
            },
            SourceFile {
                path: "foo/mod.rs",
                source: "pub fn callee() {}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "callee",
                expected: json!([
                    {"name": "callee", "filePath": "foo/mod.rs", "startLine": 1,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "lib.rs", "startLine": 4}, "line": 5, "column": 4},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([
                    {"name": "caller", "filePath": "lib.rs", "startLine": 4,
                     "edges": [
                        {"symbol": {"name": "callee", "filePath": "foo/mod.rs", "startLine": 1}, "line": 5, "column": 4},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "foo::callee",
                expected: json!([
                    {"name": "callee", "filePath": "foo/mod.rs", "startLine": 1,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "lib.rs", "startLine": 4}, "line": 5, "column": 4},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// https://github.com/microsoft/pyright/blob/386cfa1161b63f048a9ea663d84189e69ced33b1/packages/pyright-internal/src/tests/fourslash/showcallhierarchy.outgoingCalls.aliasedFunction2.fourslash.ts
// Upstream case: outgoingCalls.aliasedFunction2. Upstream returns alias/display names and groups
// repeated calls. These expectations use the canonical declare.py::func definition once per
// endpoint. Incoming queries below invert these upstream outgoing relations. These
// queries do not request outgoing calls from func.
#[tokio::test]
async fn python_import_alias() {
    check_case(Case {
        name: "python_import_alias",
        files: vec![
            SourceFile {
                path: "declare.py",
                source: "def func():\n    func2()\n\ndef func2():\n    print(1)\n",
            },
            SourceFile {
                path: "consume.py",
                source: "from declare import func as foobar\nfrom declare import func\n\ndef callByAlias():\n    foobar()\n\ndef callByName():\n    func()\n\ndef callByBoth1():\n    func()\n    foobar()\n\ndef callByBoth2():\n    foobar()\n    func()\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "callByAlias",
                expected: json!([
                    {"name": "callByAlias", "filePath": "consume.py", "startLine": 4,
                     "edges": [
                        {"symbol": {"name": "func", "filePath": "declare.py", "startLine": 1}, "line": 5, "column": 4},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "callByName",
                expected: json!([
                    {"name": "callByName", "filePath": "consume.py", "startLine": 7,
                     "edges": [
                        {"symbol": {"name": "func", "filePath": "declare.py", "startLine": 1}, "line": 8, "column": 4},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "callByBoth1",
                expected: json!([
                    {"name": "callByBoth1", "filePath": "consume.py", "startLine": 10,
                     "edges": [
                        {"symbol": {"name": "func", "filePath": "declare.py", "startLine": 1}, "line": [11, 12], "column": 4},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "callByBoth2",
                expected: json!([
                    {"name": "callByBoth2", "filePath": "consume.py", "startLine": 14,
                     "edges": [
                        {"symbol": {"name": "func", "filePath": "declare.py", "startLine": 1}, "line": [15, 16], "column": 4},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "func",
                expected: json!([
                    {"name": "func", "filePath": "declare.py", "startLine": 1,
                     "edges": [
                        {"symbol": {"name": "callByAlias", "filePath": "consume.py", "startLine": 4}, "line": 5, "column": 4},
                        {"symbol": {"name": "callByName", "filePath": "consume.py", "startLine": 7}, "line": 8, "column": 4},
                        {"symbol": {"name": "callByBoth1", "filePath": "consume.py", "startLine": 10}, "line": [11, 12], "column": 4},
                        {"symbol": {"name": "callByBoth2", "filePath": "consume.py", "startLine": 14}, "line": [15, 16], "column": 4},
                    ]},
                ]),
            },
        ],
    }).await;
}

// https://github.com/golang/tools/blob/31c58979aa257a4ccd3da5b6102a5d77cfc8d41b/gopls/internal/test/marker/testdata/callhierarchy/issue64451.txt
// Upstream case: issue64451. Strip marker comments, preserving source lines. gopls attributes
// calls inside anonymous functions to the enclosing named function or global variable. Builtin
// print has no outgoing item. The global underscore is a value entity.
#[tokio::test]
async fn go_enclosing_callers() {
    check_case(Case {
        name: "go_enclosing_callers",
        files: vec![
            SourceFile {
                path: "a/a.go",
                source: "package a\n\nfunc Foo() {\n\tbar()\n}\n\nfunc bar() {\n\tgo func() { baz() }()\n}\n\nfunc baz() {\n\tbluh()\n}\n\nfunc bluh() {\n\tprint()\n}\n\nvar _ = func() int {\n\tbaz()\n\treturn 0\n}()\n\nfunc init() {\n\tbaz()\n}\n\n\n\n\n\n\n\n\n\n\n\n\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "baz",
                expected: json!([
                    {"name": "baz", "filePath": "a/a.go", "startLine": 11,
                     "edges": [
                        {"symbol": {"name": "bar", "filePath": "a/a.go", "startLine": 7}, "line": 8, "column": 13},
                        {"symbol": {"name": "_", "filePath": "a/a.go", "startLine": 19}, "line": 20, "column": 1},
                        {"symbol": {"name": "init", "filePath": "a/a.go", "startLine": 24}, "line": 25, "column": 1},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "bar",
                expected: json!([
                    {"name": "bar", "filePath": "a/a.go", "startLine": 7,
                     "edges": [
                        {"symbol": {"name": "baz", "filePath": "a/a.go", "startLine": 11}, "line": 8, "column": 13},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "init",
                expected: json!([
                    {"name": "init", "filePath": "a/a.go", "startLine": 24,
                     "edges": [
                        {"symbol": {"name": "baz", "filePath": "a/a.go", "startLine": 11}, "line": 25, "column": 1},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "bluh",
                expected: json!([
                    {"name": "bluh", "filePath": "a/a.go", "startLine": 15,
                     "edges": [
                        {"symbol": {"name": "baz", "filePath": "a/a.go", "startLine": 11}, "line": 12, "column": 1},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "bluh",
                expected: json!([
                    {"name": "bluh", "filePath": "a/a.go", "startLine": 15,
                     "edges": []},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Foo",
                expected: json!([
                    {"name": "Foo", "filePath": "a/a.go", "startLine": 3,
                     "edges": []},
                ]),
            },
        ],
    }).await;
}

// https://github.com/eclipse-cdt/cdt/blob/4c37839cfbe3dde7bdfa398d9f703644052f5429/core/org.eclipse.cdt.ui.tests/ui/org/eclipse/cdt/ui/tests/callhierarchy/BasicCallHierarchyTest.java
// Upstream case: testStaticFunctionsC. CDT IDE supplement. Copy the two comment-embedded C
// snippets without their comment prefixes. Assert distinct same-named static functions, global
// calls and recursion. Outgoing checks are inverses of the upstream caller tree.
#[tokio::test]
async fn c_file_local_names() {
    check_case(Case {
        name: "c_file_local_names",
        files: vec![
            SourceFile {
                path: "staticFunc1.c",
                source: "void gf();\nstatic void sf() {\n    gf();\n    sf();\n}\nvoid gf() {\n    gf();\n    sf();\n}\n",
            },
            SourceFile {
                path: "staticFunc2.c",
                source: "void gf();\nstatic void sf() {\n    gf();\n    sf();\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "sf",
                expected: json!([
                    {"name": "sf", "filePath": "staticFunc1.c", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "sf", "filePath": "staticFunc1.c", "startLine": 2}, "line": 4, "column": 4},
                        {"symbol": {"name": "gf", "filePath": "staticFunc1.c", "startLine": 6}, "line": 8, "column": 4},
                    ]},
                    {"name": "sf", "filePath": "staticFunc2.c", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "sf", "filePath": "staticFunc2.c", "startLine": 2}, "line": 4, "column": 4},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "sf",
                expected: json!([
                    {"name": "sf", "filePath": "staticFunc1.c", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "gf", "filePath": "staticFunc1.c", "startLine": 6}, "line": 3, "column": 4},
                        {"symbol": {"name": "sf", "filePath": "staticFunc1.c", "startLine": 2}, "line": 4, "column": 4},
                    ]},
                    {"name": "sf", "filePath": "staticFunc2.c", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "gf", "filePath": "staticFunc1.c", "startLine": 6}, "line": 3, "column": 4},
                        {"symbol": {"name": "sf", "filePath": "staticFunc2.c", "startLine": 2}, "line": 4, "column": 4},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "gf",
                expected: json!([
                    {"name": "gf", "filePath": "staticFunc1.c", "startLine": 6,
                     "edges": [
                        {"symbol": {"name": "sf", "filePath": "staticFunc1.c", "startLine": 2}, "line": 3, "column": 4},
                        {"symbol": {"name": "gf", "filePath": "staticFunc1.c", "startLine": 6}, "line": 7, "column": 4},
                        {"symbol": {"name": "sf", "filePath": "staticFunc2.c", "startLine": 2}, "line": 3, "column": 4},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "gf",
                expected: json!([
                    {"name": "gf", "filePath": "staticFunc1.c", "startLine": 6,
                     "edges": [
                        {"symbol": {"name": "gf", "filePath": "staticFunc1.c", "startLine": 6}, "line": 7, "column": 4},
                        {"symbol": {"name": "sf", "filePath": "staticFunc1.c", "startLine": 2}, "line": 8, "column": 4},
                    ]},
                ]),
            },
        ],
    }).await;
}

// https://github.com/typescript-language-server/typescript-language-server/blob/d11657d1b6996158a64c680516fe2c31a14d9803/src/features/call-hierarchy.test.ts
// Upstream case: outgoing calls.
// https://github.com/typescript-language-server/typescript-language-server/blob/d11657d1b6996158a64c680516fe2c31a14d9803/src/features/call-hierarchy.test.ts
// Upstream case: incoming calls. Use original two.ts/three.ts. Query the outgoing subtree rooted
// at callThreeTwice: repeated method and implicit constructor calls, then tada -> print. The
// external console.log target has no indexed definition, and main is not among these source files.
// Adapt scopes to canonical scope::name and expand grouped callsites.
#[tokio::test]
async fn typescript_methods_and_constructors() {
    check_case(Case {
        name: "typescript_methods_and_constructors",
        files: vec![
            SourceFile {
                path: "two.ts",
                source: "// comment line\nimport { Three } from './three.js';\nexport class Two {\n    callThreeTwice() {\n        new Three().tada();\n        new Three().tada();\n    }\n}\n",
            },
            SourceFile {
                path: "three.ts",
                source: "// comment line\nexport class Three {\n    tada() {\n        print('🎉');\n    }\n}\nexport function print(s: string) {\n    console.log(s);\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Two::callThreeTwice",
                expected: json!([
                    {"name": "Two::callThreeTwice", "filePath": "two.ts", "startLine": 4,
                     "edges": [
                        {"symbol": {"name": "Three", "filePath": "three.ts", "startLine": 2}, "line": [5, 6], "column": 12},
                        {"symbol": {"name": "Three::tada", "filePath": "three.ts", "startLine": 3}, "line": [5, 6], "column": 20},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Three::tada",
                expected: json!([
                    {"name": "Three::tada", "filePath": "three.ts", "startLine": 3,
                     "edges": [
                        {"symbol": {"name": "Two::callThreeTwice", "filePath": "two.ts", "startLine": 4}, "line": [5, 6], "column": 20},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Three",
                expected: json!([
                    {"name": "Three", "filePath": "three.ts", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "Two::callThreeTwice", "filePath": "two.ts", "startLine": 4}, "line": [5, 6], "column": 12},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "Three::tada",
                expected: json!([
                    {"name": "Three::tada", "filePath": "three.ts", "startLine": 3,
                     "edges": [
                        {"symbol": {"name": "print", "filePath": "three.ts", "startLine": 7}, "line": 4, "column": 8},
                    ]},
                ]),
            },
        ],
    }).await;
}

// https://github.com/eclipse-jdtls/eclipse.jdt.ls/blob/dd5d8cfaef9d0688ba4c3f9754cd79d25277ca99/org.eclipse.jdt.ls.tests/src/org/eclipse/jdt/ls/core/internal/handlers/CallHierarchyHandlerTest.java
// https://github.com/eclipse-jdtls/eclipse.jdt.ls/blob/dd5d8cfaef9d0688ba4c3f9754cd79d25277ca99/org.eclipse.jdt.ls.tests/projects/eclipse/hello/src/org/sample/CallHierarchy.java
// JDT LS incomingCalls_src / outgoingCalls_src. Preserve the source sample. Query local calls
// among main, Base and Child constructors, foo, bar and method_1. External JDK targets have no
// indexed definitions; fields are not calls. The recursive1/recursive2 component is not queried.
// Constructor expectations contain one relationship per endpoint. Inverse queries check
// the selected source relationships, including inherited methods. Definition ranges come from the
// source, not JDT call-item selection ranges.
#[tokio::test]
async fn java_methods_and_constructors() {
    const FILE: &str = "org/sample/CallHierarchy.java";

    check_case(Case {
        name: "java_methods_and_constructors",
        files: vec![
            SourceFile {
                path: FILE,
                source: "package org.sample;\n\npublic class CallHierarchy {\n\n  @Deprecated\n  public static void main(String[] args) {\n    new Base();\n    new Child().bar();\n    System.out.println(Base.STATIC_FIELD);\n  }\n\n  public static class Base {\n\n    static int STATIC_FIELD = 100;\n    protected int protectedField = 200;\n\n/*nothing*/\n\n    public Base() {\n/*should resolve to enclosing \"method/constructor/initializer\"*/\n    }\n\n    public void foo() {\n\n    }\n\n    public void bar() {\n      new Child();\n      new Child();\n      Thread.currentThread();\n      Thread.currentThread();\n    }\n\n    protected void method_1() {\n      foo();\n      bar();\n    }\n\n  }\n\n  public static class Child extends Base {\n\n    Child() {\n      super();\n      foo();\n      bar();\n      method_1();\n      protectedField = 300;\n      protectedField = 400;\n    }\n\n    protected void method_2() {\n      method_1();\n      method_1();\n      method_1();\n    }\n\n  }\n\n  public void recursive1() {\n    recursive2();\n  }\n\n  public void recursive2() {\n    recursive1();\n  }\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "CallHierarchy.Base::bar",
                expected: json!([
                    {"name": "CallHierarchy.Base::bar", "filePath": "org/sample/CallHierarchy.java", "startLine": 27,
                     "edges": [
                        {"symbol": {"name": "CallHierarchy::main", "filePath": "org/sample/CallHierarchy.java", "startLine": 6}, "line": 8, "column": 16},
                        {"symbol": {"name": "CallHierarchy.Base::method_1", "filePath": "org/sample/CallHierarchy.java", "startLine": 34}, "line": 36, "column": 6},
                        {"symbol": {"name": "CallHierarchy.Child::Child", "filePath": "org/sample/CallHierarchy.java", "startLine": 43}, "line": 46, "column": 6},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "CallHierarchy.Base::method_1",
                expected: json!([
                    {"name": "CallHierarchy.Base::method_1", "filePath": "org/sample/CallHierarchy.java", "startLine": 34,
                     "edges": [
                        {"symbol": {"name": "CallHierarchy.Base::foo", "filePath": "org/sample/CallHierarchy.java", "startLine": 23}, "line": 35, "column": 6},
                        {"symbol": {"name": "CallHierarchy.Base::bar", "filePath": "org/sample/CallHierarchy.java", "startLine": 27}, "line": 36, "column": 6},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "CallHierarchy.Base::bar",
                expected: json!([
                    {"name": "CallHierarchy.Base::bar", "filePath": "org/sample/CallHierarchy.java", "startLine": 27,
                     "edges": [
                        {"symbol": {"name": "CallHierarchy.Child::Child", "filePath": "org/sample/CallHierarchy.java", "startLine": 43}, "line": [28, 29], "column": 10},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "CallHierarchy.Child::Child",
                expected: json!([
                    {"name": "CallHierarchy.Child::Child", "filePath": "org/sample/CallHierarchy.java", "startLine": 43,
                     "edges": [
                        {"symbol": {"name": "CallHierarchy::main", "filePath": "org/sample/CallHierarchy.java", "startLine": 6}, "line": 8, "column": 8},
                        {"symbol": {"name": "CallHierarchy.Base::bar", "filePath": "org/sample/CallHierarchy.java", "startLine": 27}, "line": [28, 29], "column": 10},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "CallHierarchy.Child::Child",
                expected: json!([
                    {"name": "CallHierarchy.Child::Child", "filePath": "org/sample/CallHierarchy.java", "startLine": 43,
                     "edges": [
                        {"symbol": {"name": "CallHierarchy.Base::Base", "filePath": "org/sample/CallHierarchy.java", "startLine": 19}, "line": 44, "column": 6},
                        {"symbol": {"name": "CallHierarchy.Base::foo", "filePath": "org/sample/CallHierarchy.java", "startLine": 23}, "line": 45, "column": 6},
                        {"symbol": {"name": "CallHierarchy.Base::bar", "filePath": "org/sample/CallHierarchy.java", "startLine": 27}, "line": 46, "column": 6},
                        {"symbol": {"name": "CallHierarchy.Base::method_1", "filePath": "org/sample/CallHierarchy.java", "startLine": 34}, "line": 47, "column": 6},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "CallHierarchy.Base::foo",
                expected: json!([
                    {"name": "CallHierarchy.Base::foo", "filePath": "org/sample/CallHierarchy.java", "startLine": 23,
                     "edges": [
                        {"symbol": {"name": "CallHierarchy.Base::method_1", "filePath": "org/sample/CallHierarchy.java", "startLine": 34}, "line": 35, "column": 6},
                        {"symbol": {"name": "CallHierarchy.Child::Child", "filePath": "org/sample/CallHierarchy.java", "startLine": 43}, "line": 45, "column": 6},
                    ]},
                ]),
            },
        ],
    }).await;
}

// https://github.com/eclipse-jdtls/eclipse.jdt.ls/blob/dd5d8cfaef9d0688ba4c3f9754cd79d25277ca99/org.eclipse.jdt.ls.tests/src/org/eclipse/jdt/ls/core/internal/handlers/CallHierarchyHandlerTest.java
// https://github.com/eclipse-jdtls/eclipse.jdt.ls/blob/dd5d8cfaef9d0688ba4c3f9754cd79d25277ca99/org.eclipse.jdt.ls.tests/projects/eclipse/hello/src/org/sample/Call.java
// https://github.com/eclipse-jdtls/eclipse.jdt.ls/blob/dd5d8cfaef9d0688ba4c3f9754cd79d25277ca99/org.eclipse.jdt.ls.tests/projects/eclipse/hello/src/org/sample/Foo.java
// JDT LS testSelectionRange. Retain the two Call.main -> Foo.someMethod sites from the upstream
// caller result. JDK targets have no indexed definitions. Outgoing calls from main are not
// asserted here because the sample also constructs Foo. zvec-grep groups both sites under the
// canonical Call.main definition, while the upstream result has separate items at the two sites.
// Both qualified and unqualified symbol queries must find the same method.
#[tokio::test]
async fn java_cross_file_repeated_calls() {
    check_case(Case {
        name: "java_cross_file_repeated_calls",
        files: vec![
            SourceFile {
                path: "org/sample/Call.java",
                source: "package org.sample;\n\npublic class Call {\n    public static void main(String[] args) {\n        System.out.println(\"skip\");\n        System.out.println(\"skip\");\n        System.out.println(\"skip\");\n        new Foo().someMethod();\n        System.out.println(\"skip\");\n        System.out.println(\"skip\");\n        new Foo().someMethod();\n    }\n}\n",
            },
            SourceFile {
                path: "org/sample/Foo.java",
                source: "package org.sample;\n\npublic class Foo {\n\n\tpublic static void main(String[] args) {\n\t\tSystem.out.print(\"Hello world!\");\n\t}\n\t\n\tpublic void someMethod() {}\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "Foo::someMethod",
                expected: json!([
                    {"name": "Foo::someMethod", "filePath": "org/sample/Foo.java", "startLine": 9,
                     "edges": [
                        {"symbol": {"name": "Call::main", "filePath": "org/sample/Call.java", "startLine": 4}, "line": [8, 11], "column": 18},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "someMethod",
                expected: json!([
                    {"name": "Foo::someMethod", "filePath": "org/sample/Foo.java", "startLine": 9,
                     "edges": [
                        {"symbol": {"name": "Call::main", "filePath": "org/sample/Call.java", "startLine": 4}, "line": [8, 11], "column": 18},
                    ]},
                ]),
            },
        ],
    }).await;
}

// https://github.com/llvm/llvm-project/blob/fb2a5266d9afbbd1ac2f41a75bcda1b27eb51097/clang-tools-extra/clangd/unittests/CallHierarchyTests.cpp
// clangd IncomingQualified. Remove annotation markers, retaining source lines and indentation.
// Represent the out-of-line method definition once, with canonical ns::Waldo scope. Outgoing
// checks are inverses of the upstream incoming expectations; the declaration must not produce a
// duplicate method result.
#[tokio::test]
async fn cpp_qualified_method() {
    check_case(Case {
        name: "cpp_qualified_method",
        files: vec![
            SourceFile {
                path: "main.cc",
                source: "\n    namespace ns {\n    struct Waldo {\n      void find();\n    };\n    void Waldo::find() {}\n    void caller1(Waldo &W) {\n      W.find();\n    }\n    void caller2(Waldo &W) {\n      W.find();\n    }\n    }\n  ",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "ns::Waldo::find",
                expected: json!([
                    {"name": "ns::Waldo::find", "filePath": "main.cc", "startLine": 6,
                     "edges": [
                        {"symbol": {"name": "ns::caller1", "filePath": "main.cc", "startLine": 7}, "line": 8, "column": 8},
                        {"symbol": {"name": "ns::caller2", "filePath": "main.cc", "startLine": 10}, "line": 11, "column": 8},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "ns::caller1",
                expected: json!([
                    {"name": "ns::caller1", "filePath": "main.cc", "startLine": 7,
                     "edges": [
                        {"symbol": {"name": "ns::Waldo::find", "filePath": "main.cc", "startLine": 6}, "line": 8, "column": 8},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "ns::caller2",
                expected: json!([
                    {"name": "ns::caller2", "filePath": "main.cc", "startLine": 10,
                     "edges": [
                        {"symbol": {"name": "ns::Waldo::find", "filePath": "main.cc", "startLine": 6}, "line": 11, "column": 8},
                    ]},
                ]),
            },
        ],
    }).await;
}

// https://github.com/llvm/llvm-project/blob/fb2a5266d9afbbd1ac2f41a75bcda1b27eb51097/clang-tools-extra/clangd/unittests/CallHierarchyTests.cpp
// clangd MultiFileCpp, definition-site incoming and outgoing expectations. Remove annotation
// markers and preserve source coordinates. Query canonical implementation definitions. Header
// declarations must not duplicate results; cursor-based prepare behavior is outside the symbol-
// name query contract. Qualified calls identify distinct endpoints, and leaf/root queries return empty
// edges.
#[tokio::test]
#[allow(clippy::too_many_lines)] // Keep the cross-file fixture and its expectations together.
async fn cpp_cross_file_call_chain() {
    check_case(Case {
        name: "cpp_cross_file_call_chain",
        files: vec![
            SourceFile { path: "callee.hh", source: "\n    void callee(int);\n  " },
            SourceFile { path: "caller1.hh", source: "\n    namespace nsa {\n      void caller1();\n    }\n  " },
            SourceFile { path: "caller2.hh", source: "\n    namespace nsb {\n      void caller2();\n    }\n  " },
            SourceFile { path: "caller3.hh", source: "\n    namespace nsa {\n      void caller3();\n    }\n  " },

            SourceFile {
                path: "callee.cc",
                source: "\n    #include \"callee.hh\"\n    void callee(int) {}\n  ",
            },
            SourceFile {
                path: "caller1.cc",
                source: "\n    #include \"callee.hh\"\n    #include \"caller1.hh\"\n    namespace nsa {\n      void caller1() {\n        callee(42);\n      }\n    }\n  ",
            },
            SourceFile {
                path: "caller2.cc",
                source: "\n    #include \"caller1.hh\"\n    #include \"caller2.hh\"\n    namespace nsb {\n      void caller2() {\n        nsa::caller1();\n        nsa::caller1();\n      }\n    }\n  ",
            },
            SourceFile {
                path: "caller3.cc",
                source: "\n    #include \"caller1.hh\"\n    #include \"caller2.hh\"\n    namespace nsa {\n      void caller3() {\n        caller1();\n        nsb::caller2();\n      }\n    }\n  ",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "callee",
                expected: json!([
                    {"name": "callee", "filePath": "callee.cc", "startLine": 3,
                     "edges": [
                        {"symbol": {"name": "nsa::caller1", "filePath": "caller1.cc", "startLine": 5}, "line": 6, "column": 8},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "nsa::caller1",
                expected: json!([
                    {"name": "nsa::caller1", "filePath": "caller1.cc", "startLine": 5,
                     "edges": [
                        {"symbol": {"name": "nsb::caller2", "filePath": "caller2.cc", "startLine": 5}, "line": [6, 7], "column": 13},
                        {"symbol": {"name": "nsa::caller3", "filePath": "caller3.cc", "startLine": 5}, "line": 6, "column": 8},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "nsb::caller2",
                expected: json!([
                    {"name": "nsb::caller2", "filePath": "caller2.cc", "startLine": 5,
                     "edges": [
                        {"symbol": {"name": "nsa::caller3", "filePath": "caller3.cc", "startLine": 5}, "line": 7, "column": 13},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "nsa::caller3",
                expected: json!([
                    {"name": "nsa::caller3", "filePath": "caller3.cc", "startLine": 5,
                     "edges": []},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "nsa::caller3",
                expected: json!([
                    {"name": "nsa::caller3", "filePath": "caller3.cc", "startLine": 5,
                     "edges": [
                        {"symbol": {"name": "nsa::caller1", "filePath": "caller1.cc", "startLine": 5}, "line": 6, "column": 8},
                        {"symbol": {"name": "nsb::caller2", "filePath": "caller2.cc", "startLine": 5}, "line": 7, "column": 13},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "nsb::caller2",
                expected: json!([
                    {"name": "nsb::caller2", "filePath": "caller2.cc", "startLine": 5,
                     "edges": [
                        {"symbol": {"name": "nsa::caller1", "filePath": "caller1.cc", "startLine": 5}, "line": [6, 7], "column": 13},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "nsa::caller1",
                expected: json!([
                    {"name": "nsa::caller1", "filePath": "caller1.cc", "startLine": 5,
                     "edges": [
                        {"symbol": {"name": "callee", "filePath": "callee.cc", "startLine": 3}, "line": 6, "column": 8},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "callee",
                expected: json!([
                    {"name": "callee", "filePath": "callee.cc", "startLine": 3,
                     "edges": []},
                ]),
            },
        ],
    }).await;
}

// https://github.com/llvm/llvm-project/blob/fb2a5266d9afbbd1ac2f41a75bcda1b27eb51097/clang-tools-extra/clangd/unittests/CallHierarchyTests.cpp
// clangd IncomingIncludeOverrides. Adopt its incoming-call semantics: a call through
// Interface::Func is included among Implementation::Func callers. This explicitly requires
// override-aware resolution; lexical matching alone is insufficient. The outgoing implementation-
// to-callee check is the inverse of the first upstream incoming level.
#[tokio::test]
async fn cpp_virtual_override_callers() {
    check_case(Case {
        name: "cpp_virtual_override_callers",
        files: vec![
            SourceFile { path: "virtual.cc", source: "\n    void callee() {}\n    struct Interface {\n      virtual void Func() = 0;\n    };\n    struct Implementation : public Interface {\n      void Func() override {\n          callee();\n      }\n    };\n    void Test(Interface& cls){\n      cls.Func();\n    }\n  " },
        ],
        queries: vec![
            Query { operation: "callers", symbol: "Implementation::Func", expected: json!([
                {"name": "Implementation::Func", "filePath": "virtual.cc", "startLine": 7,
                 "edges": [
                    {"symbol": {"name": "Test", "filePath": "virtual.cc", "startLine": 11}, "line": 12, "column": 10},
                ]},
            ]) },
            Query { operation: "callers", symbol: "callee", expected: json!([
                {"name": "callee", "filePath": "virtual.cc", "startLine": 2,
                 "edges": [
                    {"symbol": {"name": "Implementation::Func", "filePath": "virtual.cc", "startLine": 7}, "line": 8, "column": 10},
                ]},
            ]) },
            Query { operation: "callees", symbol: "Implementation::Func", expected: json!([
                {"name": "Implementation::Func", "filePath": "virtual.cc", "startLine": 7,
                 "edges": [
                    {"symbol": {"name": "callee", "filePath": "virtual.cc", "startLine": 2}, "line": 8, "column": 10},
                ]},
            ]) },
        ],
    }).await;
}

// https://github.com/llvm/llvm-project/blob/fb2a5266d9afbbd1ac2f41a75bcda1b27eb51097/clang-tools-extra/clangd/unittests/CallHierarchyTests.cpp
// clangd CallInLocalVarDecl. A local variable initializer belongs to its enclosing function; a
// namespace-level initializer belongs to the global variable itself. Do not emit localVar as a
// separate caller. Outgoing checks invert these upstream incoming expectations.
#[tokio::test]
async fn cpp_initializer_caller_ownership() {
    check_case(Case {
        name: "cpp_initializer_caller_ownership",
        files: vec![
            SourceFile { path: "initializers.cc", source: "\n    int callee();\n    void caller1() {\n      callee();\n    }\n    void caller2() {\n      int localVar = callee();\n    }\n    int caller3 = callee();\n  " },
        ],
        queries: vec![
            Query { operation: "callers", symbol: "callee", expected: json!([
                {"name": "callee", "filePath": "initializers.cc", "startLine": 2,
                 "edges": [
                    {"symbol": {"name": "caller1", "filePath": "initializers.cc", "startLine": 3}, "line": 4, "column": 6},
                    {"symbol": {"name": "caller2", "filePath": "initializers.cc", "startLine": 6}, "line": 7, "column": 21},
                    {"symbol": {"name": "caller3", "filePath": "initializers.cc", "startLine": 9}, "line": 9, "column": 18},
                ]},
            ]) },
            Query { operation: "callees", symbol: "caller2", expected: json!([
                {"name": "caller2", "filePath": "initializers.cc", "startLine": 6,
                 "edges": [
                    {"symbol": {"name": "callee", "filePath": "initializers.cc", "startLine": 2}, "line": 7, "column": 21},
                ]},
            ]) },
            Query { operation: "callees", symbol: "caller3", expected: json!([
                {"name": "caller3", "filePath": "initializers.cc", "startLine": 9,
                 "edges": [
                    {"symbol": {"name": "callee", "filePath": "initializers.cc", "startLine": 2}, "line": 9, "column": 18},
                ]},
            ]) },
        ],
    }).await;
}

// https://github.com/microsoft/TypeScript/blob/fed0bf24149fb1ed36039212648bafdafc1ea10e/tsc/internal/fourslash/tests/callHierarchyFunctionAmbiguity1_test.go
// https://github.com/microsoft/TypeScript/blob/fed0bf24149fb1ed36039212648bafdafc1ea10e/tsc/testdata/baselines/reference/fourslash/callHierarchy/callHierarchyFunctionAmbiguity.1.callHierarchy.txt
// https://github.com/microsoft/TypeScript/blob/fed0bf24149fb1ed36039212648bafdafc1ea10e/tsc/testdata/baselines/reference/fourslash/callHierarchy/callHierarchyFunctionAmbiguity.5.callHierarchy.txt
// TypeScript FunctionAmbiguity1-5 share these source files and differ only in cursor selection.
// Adopt the baseline canonical targets: a.d.ts::foo and the first b.d.ts::foo declaration. The
// boolean overload belongs to the latter group. One callsite has two possible targets; retain
// both, without duplicating b.d.ts by overload. This is overload ambiguity, not unrelated same-
// name scopes.
#[tokio::test]
async fn typescript_ambiguous_overload_declarations() {
    check_case(Case {
        name: "typescript_ambiguous_overload_declarations",
        files: vec![
            SourceFile { path: "a.d.ts", source: "declare function foo(x?: number): void;\n" },
            SourceFile { path: "b.d.ts", source: "declare function foo(x?: string): void;\ndeclare function foo(x?: boolean): void;\n" },
            SourceFile { path: "main.ts", source: "function bar() {\n    foo();\n}\n" },
        ],
        queries: vec![
            Query { operation: "callers", symbol: "foo", expected: json!([
                {"name": "foo", "filePath": "a.d.ts", "startLine": 1,
                 "edges": [
                    {"symbol": {"name": "bar", "filePath": "main.ts", "startLine": 1}, "line": 2, "column": 4},
                ]},
                {"name": "foo", "filePath": "b.d.ts", "startLine": 1,
                 "edges": [
                    {"symbol": {"name": "bar", "filePath": "main.ts", "startLine": 1}, "line": 2, "column": 4},
                ]},
            ]) },
            Query { operation: "callees", symbol: "bar", expected: json!([
                {"name": "bar", "filePath": "main.ts", "startLine": 1,
                 "edges": [
                    {"symbol": {"name": "foo", "filePath": "a.d.ts", "startLine": 1}, "line": 2, "column": 4},
                    {"symbol": {"name": "foo", "filePath": "b.d.ts", "startLine": 1}, "line": 2, "column": 4},
                ]},
            ]) },
        ],
    }).await;
}

// Additional zvec-grep regression, not copied from an upstream case. Two independent namespaces
// each define work and caller. Unqualified lookup must return separate definitions, and qualified
// lookup must never attach calls from the other scope.
#[tokio::test]
async fn typescript_same_name_scopes() {
    check_case(Case {
        name: "typescript_same_name_scopes",
        files: vec![
            SourceFile { path: "scopes.ts", source: "namespace Left {\n    export function work() {}\n    export function caller() {\n        work();\n    }\n}\nnamespace Right {\n    export function work() {}\n    export function caller() {\n        work();\n    }\n}\n" },
        ],
        queries: vec![
            Query { operation: "callees", symbol: "caller", expected: json!([
                {"name": "Left::caller", "filePath": "scopes.ts", "startLine": 3,
                 "edges": [
                    {"symbol": {"name": "Left::work", "filePath": "scopes.ts", "startLine": 2}, "line": 4, "column": 8},
                ]},
                {"name": "Right::caller", "filePath": "scopes.ts", "startLine": 9,
                 "edges": [
                    {"symbol": {"name": "Right::work", "filePath": "scopes.ts", "startLine": 8}, "line": 10, "column": 8},
                ]},
            ]) },
            Query { operation: "callers", symbol: "Right::work", expected: json!([
                {"name": "Right::work", "filePath": "scopes.ts", "startLine": 8,
                 "edges": [
                    {"symbol": {"name": "Right::caller", "filePath": "scopes.ts", "startLine": 9}, "line": 10, "column": 8},
                ]},
            ]) },
            Query { operation: "callers", symbol: "work", expected: json!([
                {"name": "Left::work", "filePath": "scopes.ts", "startLine": 2,
                 "edges": [
                    {"symbol": {"name": "Left::caller", "filePath": "scopes.ts", "startLine": 3}, "line": 4, "column": 8},
                ]},
                {"name": "Right::work", "filePath": "scopes.ts", "startLine": 8,
                 "edges": [
                    {"symbol": {"name": "Right::caller", "filePath": "scopes.ts", "startLine": 9}, "line": 10, "column": 8},
                ]},
            ]) },
            Query { operation: "callers", symbol: "Left::work", expected: json!([
                {"name": "Left::work", "filePath": "scopes.ts", "startLine": 2,
                 "edges": [
                    {"symbol": {"name": "Left::caller", "filePath": "scopes.ts", "startLine": 3}, "line": 4, "column": 8},
                ]},
            ]) },
            Query { operation: "callees", symbol: "Left::caller", expected: json!([
                {"name": "Left::caller", "filePath": "scopes.ts", "startLine": 3,
                 "edges": [
                    {"symbol": {"name": "Left::work", "filePath": "scopes.ts", "startLine": 2}, "line": 4, "column": 8},
                ]},
            ]) },
            Query { operation: "callees", symbol: "Right::caller", expected: json!([
                {"name": "Right::caller", "filePath": "scopes.ts", "startLine": 9,
                 "edges": [
                    {"symbol": {"name": "Right::work", "filePath": "scopes.ts", "startLine": 8}, "line": 10, "column": 8},
                ]},
            ]) },
        ],
    }).await;
}

// https://github.com/microsoft/TypeScript/blob/fed0bf24149fb1ed36039212648bafdafc1ea10e/tsc/internal/fourslash/tests/callHierarchyConstNamedFunctionExpression_test.go
// https://github.com/microsoft/TypeScript/blob/fed0bf24149fb1ed36039212648bafdafc1ea10e/tsc/testdata/baselines/reference/fourslash/callHierarchy/callHierarchyConstNamedFunctionExpression.callHierarchy.txt
// TypeScript ConstNamedFunctionExpression. The anonymous function is named by its variable binding
// bar. Calls within it belong to bar, not a file entity or foo; this is a relationship assertion,
// not a no-crash-only regression. Additional inverse queries preserve the upstream edges.
#[tokio::test]
async fn typescript_anonymous_function_binding() {
    check_case(Case {
        name: "typescript_anonymous_function_binding",
        files: vec![
            SourceFile { path: "anonymous.ts", source: "function foo() {\n    bar();\n}\n\nconst bar = function () {\n    baz();\n}\n\nfunction baz() {\n}\n" },
        ],
        queries: vec![
            Query { operation: "callers", symbol: "bar", expected: json!([
                {"name": "bar", "filePath": "anonymous.ts", "startLine": 5,
                 "edges": [
                    {"symbol": {"name": "foo", "filePath": "anonymous.ts", "startLine": 1}, "line": 2, "column": 4},
                ]},
            ]) },
            Query { operation: "callees", symbol: "bar", expected: json!([
                {"name": "bar", "filePath": "anonymous.ts", "startLine": 5,
                 "edges": [
                    {"symbol": {"name": "baz", "filePath": "anonymous.ts", "startLine": 9}, "line": 6, "column": 4},
                ]},
            ]) },
            Query { operation: "callers", symbol: "baz", expected: json!([
                {"name": "baz", "filePath": "anonymous.ts", "startLine": 9,
                 "edges": [
                    {"symbol": {"name": "bar", "filePath": "anonymous.ts", "startLine": 5}, "line": 6, "column": 4},
                ]},
            ]) },
            Query { operation: "callees", symbol: "foo", expected: json!([
                {"name": "foo", "filePath": "anonymous.ts", "startLine": 1,
                 "edges": [
                    {"symbol": {"name": "bar", "filePath": "anonymous.ts", "startLine": 5}, "line": 2, "column": 4},
                ]},
            ]) },
        ],
    }).await;
}

// https://github.com/microsoft/TypeScript/blob/fed0bf24149fb1ed36039212648bafdafc1ea10e/tsc/internal/fourslash/tests/callHierarchyClassPropertyArrowFunction_test.go
// https://github.com/microsoft/TypeScript/blob/fed0bf24149fb1ed36039212648bafdafc1ea10e/tsc/testdata/baselines/reference/fourslash/callHierarchy/callHierarchyClassPropertyArrowFunction.callHierarchy.txt
// TypeScript ClassPropertyArrowFunction. Attribute this.callee() to the caller arrow-function
// property, and resolve this against C. Canonical scopes use C::name; definition line spans match
// the upstream arrow functions. Outgoing is the inverse of the upstream incoming edge.
#[tokio::test]
async fn typescript_class_arrow_function() {
    check_case(Case {
        name: "typescript_class_arrow_function",
        files: vec![
            SourceFile { path: "arrows.ts", source: "class C {\n    caller = () => {\n        this.callee();\n    }\n\n    callee = () => {\n    }\n}\n" },
        ],
        queries: vec![
            Query { operation: "callers", symbol: "C::callee", expected: json!([
                {"name": "C::callee", "filePath": "arrows.ts", "startLine": 6,
                 "edges": [
                    {"symbol": {"name": "C::caller", "filePath": "arrows.ts", "startLine": 2}, "line": 3, "column": 13},
                ]},
            ]) },
            Query { operation: "callees", symbol: "C::caller", expected: json!([
                {"name": "C::caller", "filePath": "arrows.ts", "startLine": 2,
                 "edges": [
                    {"symbol": {"name": "C::callee", "filePath": "arrows.ts", "startLine": 6}, "line": 3, "column": 13},
                ]},
            ]) },
            Query { operation: "callees", symbol: "C::callee", expected: json!([
                {"name": "C::callee", "filePath": "arrows.ts", "startLine": 6,
                 "edges": []},
            ]) },
        ],
    }).await;
}

// https://github.com/eclipse-jdtls/eclipse.jdt.ls/blob/dd5d8cfaef9d0688ba4c3f9754cd79d25277ca99/org.eclipse.jdt.ls.tests/src/org/eclipse/jdt/ls/core/internal/handlers/CallHierarchyHandlerTest.java
// https://github.com/eclipse-jdtls/eclipse.jdt.ls/blob/dd5d8cfaef9d0688ba4c3f9754cd79d25277ca99/org.eclipse.jdt.ls.tests/projects/eclipse/hello/src/org/sample/CallHierarchyGH2771.java
// JDT LS incomingCalls_OnInterfaceMethod. The interface method name exists but has no callers. Its
// Opt<String> return type is not a calls edge from name. The outgoing empty assertion extends the
// upstream incoming expectation to guard against misclassifying type references.
#[tokio::test]
async fn java_interface_without_callers() {
    check_case(Case {
        name: "java_interface_without_callers",
        files: vec![
            SourceFile { path: "org/sample/CallHierarchyGH2771.java", source: "package org.sample;\n\npublic interface CallHierarchyGH2771 {\n    Opt<String> name();\n\n    public static class Opt<T> {\n        public static <T> Opt<T> of(T t) {\n            return new Opt();\n        }\n    }\n}\n" },
        ],
        queries: vec![
            Query { operation: "callers", symbol: "CallHierarchyGH2771::name", expected: json!([
                {"name": "CallHierarchyGH2771::name", "filePath": "org/sample/CallHierarchyGH2771.java", "startLine": 4,
                 "edges": []},
            ]) },
            Query { operation: "callees", symbol: "CallHierarchyGH2771::name", expected: json!([
                {"name": "CallHierarchyGH2771::name", "filePath": "org/sample/CallHierarchyGH2771.java", "startLine": 4,
                 "edges": []},
            ]) },
        ],
    }).await;
}

// zvec-grep-specific incremental regressions, not upstream LSP fixtures.
// Keep one engine and its read-session cache alive across updates. Only the
// explicitly changed file is submitted to index; callers must be re-resolved
// when a target disappears or changes identity without rewriting caller source.
struct IncrementalWorkspace {
    directory: tempfile::TempDir,
    glob: &'static str,
    _embeddings: EmbeddingServer,
    engine: ZvecGrep,
    _permit: tokio::sync::SemaphorePermit<'static>,
}

impl IncrementalWorkspace {
    async fn new() -> Self {
        Self::from_sources(
            &[
                SourceFile {
                    path: "caller.py",
                    source: "from targets import first, second\ndef caller():\n    first()\n",
                },
                SourceFile {
                    path: "targets.py",
                    source: "def first():\n    pass\n\ndef second():\n    pass\n",
                },
            ],
            "*.py",
        )
        .await
    }

    async fn from_sources(files: &[SourceFile], glob: &'static str) -> Self {
        let permit = WORKSPACES.acquire().await.expect("workspace permit");
        let directory = tempfile::tempdir().expect("incremental workspace");
        let root = directory.path();
        for file in files {
            let path = root.join(file.path);
            fs::create_dir_all(path.parent().expect("source directory")).expect("source directory");
            fs::write(path, file.source).expect("source file");
        }
        let embeddings = EmbeddingServer::start().expect("embedding server");
        configure_remote_model(root, embeddings.address).expect("embedding configuration");
        let workspace = Self {
            directory,
            glob,
            _embeddings: embeddings,
            engine: ZvecGrep::new(),
            _permit: permit,
        };
        let indexed = workspace.index_with_rebuild(Vec::new(), true).await;
        assert_eq!(indexed.files_added, files.len());
        workspace
            .engine
            .enable_read_session_cache()
            .expect("read cache");
        workspace
    }

    async fn index(&self, changes: Vec<WorkspaceChange>) -> IndexResult {
        self.index_with_rebuild(changes, false).await
    }

    async fn index_with_rebuild(
        &self,
        changes: Vec<WorkspaceChange>,
        rebuild: bool,
    ) -> IndexResult {
        let mut options = index_options(self.directory.path());
        options.graph_indexing = Some(true);
        options.rebuild = rebuild;
        options.scan.globs = Some(vec![self.glob.into()]);
        options.changes = changes;
        let result = self
            .engine
            .index(options)
            .await
            .expect("index source changes");
        assert_eq!(result.files_failed, 0, "{result:?}");
        assert!(result.skipped.is_empty(), "{result:?}");
        result
    }

    async fn check(&self, stage: &str, queries: &[Query]) {
        check_queries(&self.engine, self.directory.path(), stage, queries).await;
    }

    async fn check_initial_calls(&self, stage: &str) {
        self.check(
            stage,
            &[
                Query {
                    operation: "callees",
                    symbol: "caller",
                    expected: json!([
                        {"name": "caller", "filePath": "caller.py", "startLine": 2,
                         "edges": [
                            {"symbol": {"name": "first", "filePath": "targets.py", "startLine": 1}, "line": 3, "column": 4},
                        ]},
                    ]),
                },
                Query {
                    operation: "callers",
                    symbol: "first",
                    expected: json!([
                        {"name": "first", "filePath": "targets.py", "startLine": 1,
                         "edges": [
                            {"symbol": {"name": "caller", "filePath": "caller.py", "startLine": 2}, "line": 3, "column": 4},
                        ]},
                    ]),
                },
                Query {
                    operation: "callers",
                    symbol: "second",
                    expected: json!([
                        {"name": "second", "filePath": "targets.py", "startLine": 4,
                         "edges": []},
                    ]),
                },
            ],
        )
        .await;
    }
}

impl Drop for IncrementalWorkspace {
    fn drop(&mut self) {
        self.engine.close();
    }
}

#[tokio::test]
async fn incremental_call_target_change_removes_old_edge() {
    let workspace = IncrementalWorkspace::new().await;
    workspace.check_initial_calls("before call edit").await;
    fs::write(
        workspace.directory.path().join("caller.py"),
        "from targets import first, second\ndef caller():\n    second()\n",
    )
    .expect("retarget call");
    let indexed = workspace
        .index(vec![WorkspaceChange::Upsert("caller.py".into())])
        .await;
    assert_eq!(indexed.files_modified, 1);

    workspace
        .check(
            "after call edit",
            &[
                Query {
                    operation: "callees",
                    symbol: "caller",
                    expected: json!([
                        {"name": "caller", "filePath": "caller.py", "startLine": 2,
                         "edges": [
                            {"symbol": {"name": "second", "filePath": "targets.py", "startLine": 4}, "line": 3, "column": 4},
                        ]},
                    ]),
                },
                Query {
                    operation: "callers",
                    symbol: "first",
                    expected: json!([
                        {"name": "first", "filePath": "targets.py", "startLine": 1,
                         "edges": []},
                    ]),
                },
                Query {
                    operation: "callers",
                    symbol: "second",
                    expected: json!([
                        {"name": "second", "filePath": "targets.py", "startLine": 4,
                         "edges": [
                            {"symbol": {"name": "caller", "filePath": "caller.py", "startLine": 2}, "line": 3, "column": 4},
                        ]},
                    ]),
                },
            ],
        )
        .await;
}

#[tokio::test]
async fn incremental_target_deletion_invalidates_incoming_edges() {
    let workspace = IncrementalWorkspace::new().await;
    workspace
        .check_initial_calls("before target deletion")
        .await;
    fs::remove_file(workspace.directory.path().join("targets.py")).expect("delete targets");
    let indexed = workspace
        .index(vec![WorkspaceChange::Delete("targets.py".into())])
        .await;
    assert_eq!(indexed.files_deleted, 1);

    workspace
        .check(
            "deleted target must not leave a dangling resolved edge",
            &[Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([
                    {"name": "caller", "filePath": "caller.py", "startLine": 2,
                     "edges": []},
                ]),
            }],
        )
        .await;
}

#[tokio::test]
async fn incremental_target_recreation_resolves_unchanged_caller() {
    let workspace = IncrementalWorkspace::new().await;
    workspace
        .check_initial_calls("before target replacement")
        .await;
    let target_path = workspace.directory.path().join("targets.py");
    fs::remove_file(&target_path).expect("delete targets");
    let deleted = workspace
        .index(vec![WorkspaceChange::Delete("targets.py".into())])
        .await;
    assert_eq!(deleted.files_deleted, 1);

    workspace
        .check(
            "between deletion and recreation",
            &[Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([
                    {"name": "caller", "filePath": "caller.py", "startLine": 2,
                     "edges": []},
                ]),
            }],
        )
        .await;
    // New content and definition position require a new canonical entity ID.
    fs::write(
        target_path,
        "\n\ndef first():\n    return 42\n\ndef second():\n    pass\n",
    )
    .expect("recreate target with changed identity");
    let recreated = workspace
        .index(vec![WorkspaceChange::Upsert("targets.py".into())])
        .await;
    assert_eq!(recreated.files_added, 1);
    assert_eq!(
        recreated.files_modified, 0,
        "caller source was not reindexed"
    );

    workspace
        .check(
            "recreated target rebinds the unchanged caller",
            &[
                Query {
                    operation: "callees",
                    symbol: "caller",
                    expected: json!([
                        {"name": "caller", "filePath": "caller.py", "startLine": 2,
                         "edges": [
                            {"symbol": {"name": "first", "filePath": "targets.py", "startLine": 3}, "line": 3, "column": 4},
                        ]},
                    ]),
                },
                Query {
                    operation: "callers",
                    symbol: "first",
                    expected: json!([
                        {"name": "first", "filePath": "targets.py", "startLine": 3,
                         "edges": [
                            {"symbol": {"name": "caller", "filePath": "caller.py", "startLine": 2}, "line": 3, "column": 4},
                        ]},
                    ]),
                },
            ],
        )
        .await;
}

#[tokio::test]
async fn incremental_caller_deletion_removes_outgoing_edges() {
    let workspace = IncrementalWorkspace::new().await;
    workspace
        .check_initial_calls("before caller deletion")
        .await;
    fs::remove_file(workspace.directory.path().join("caller.py")).expect("delete caller");
    let indexed = workspace
        .index(vec![WorkspaceChange::Delete("caller.py".into())])
        .await;
    assert_eq!(indexed.files_deleted, 1);

    workspace
        .check(
            "after caller deletion",
            &[
                Query {
                    operation: "callers",
                    symbol: "first",
                    expected: json!([
                        {"name": "first", "filePath": "targets.py", "startLine": 1,
                         "edges": []},
                    ]),
                },
                Query {
                    operation: "callers",
                    symbol: "second",
                    expected: json!([
                        {"name": "second", "filePath": "targets.py", "startLine": 4,
                         "edges": []},
                    ]),
                },
            ],
        )
        .await;
}

#[tokio::test]
async fn repeated_index_does_not_accumulate_call_edges() {
    let workspace = IncrementalWorkspace::new().await;
    workspace.check_initial_calls("initial index").await;
    for revision in 1..=2 {
        // Change the file outside the function so its canonical entity and
        // callsite stay identical while indexing must replace its graph.
        fs::write(
            workspace.directory.path().join("caller.py"),
            format!("from targets import first, second\ndef caller():\n    first()\n# revision {revision}\n"),
        )
        .expect("change file while preserving callsite");
        let updated = workspace
            .index(vec![WorkspaceChange::Upsert("caller.py".into())])
            .await;
        assert_eq!(updated.files_modified, 1);
        workspace
            .check_initial_calls("file replacement keeps one callsite")
            .await;
        let unchanged = workspace.index(Vec::new()).await;
        assert_eq!(unchanged.files_unchanged, 2);
        assert_eq!(
            unchanged.files_added + unchanged.files_modified + unchanged.files_deleted,
            0
        );
        workspace
            .check_initial_calls("unchanged reindex keeps one callsite")
            .await;
    }
}

// https://github.com/PHPantom-dev/phpantom_lsp/blob/1756b1f920c1290d4c4fdd4d2c902fad53a75cd3/tests/integration/call_hierarchy.rs
// Port the PHPantom recursive-function scenario to Rust, which has a configured parser. Preserve
// the self-call expectation in both directions; this is a translated scenario, not an original
// Rust upstream fixture.
#[tokio::test]
async fn rust_self_recursion() {
    check_case(Case {
        name: "rust_self_recursion",
        files: vec![SourceFile {
            path: "recursive.rs",
            source: "fn countdown(n: i32) {\n    if n > 0 { countdown(n - 1); }\n}\n",
        }],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "countdown",
                expected: json!([
                    {"name": "countdown", "filePath": "recursive.rs", "startLine": 1,
                     "edges": [
                        {"symbol": {"name": "countdown", "filePath": "recursive.rs", "startLine": 1}, "line": 2, "column": 15},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "countdown",
                expected: json!([
                    {"name": "countdown", "filePath": "recursive.rs", "startLine": 1,
                     "edges": [
                        {"symbol": {"name": "countdown", "filePath": "recursive.rs", "startLine": 1}, "line": 2, "column": 15},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// https://github.com/ycm-core/ycmd/blob/a290328810cec343fc4df23ff86936cfa7630687/ycmd/tests/javascript/subcommands_test.py
// https://github.com/ycm-core/ycmd/blob/a290328810cec343fc4df23ff86936cfa7630687/ycmd/tests/javascript/testdata/test.js
// https://github.com/ycm-core/ycmd/blob/a290328810cec343fc4df23ff86936cfa7630687/ycmd/tests/javascript/testdata/file2.js
// https://github.com/ycm-core/ycmd/blob/a290328810cec343fc4df23ff86936cfa7630687/ycmd/tests/javascript/testdata/file3.js
// Adapt ycmd/tsserver test_Subcommands_GoToCallers, a JavaScript integration supplement rather
// than an LSP protocol test. Keep its three Bar.testMethod receiver shapes and simplify unrelated
// declarations. Wrap the original top-level callsites in named functions so this case tests
// symbol-name queries without inventing a file-symbol contract. Remap positions to these source
// snippets. Outgoing assertions include implicit construction via the Bar class entity, as in the
// existing TypeScript constructor case.
#[tokio::test]
async fn javascript_cross_file_method_calls() {
    check_case(Case {
        name: "javascript_cross_file_method_calls",
        files: vec![
            SourceFile { path: "test.js", source: "class Bar {\n  testMethod() {}\n}\nfunction localCaller() {\n  var bar = new Bar();\n  bar.testMethod();\n}\n" },
            SourceFile { path: "file2.js", source: "function expressionCaller() {\n  new Bar().testMethod();\n}\n" },
            SourceFile { path: "file3.js", source: "function remoteCaller() {\n  var bar = new Bar();\n  bar.testMethod();\n}\n" },
        ],
        queries: vec![
            Query { operation: "callers", symbol: "Bar::testMethod", expected: json!([
                {"name": "Bar::testMethod", "filePath": "test.js", "startLine": 2,
                 "edges": [
                    {"symbol": {"name": "localCaller", "filePath": "test.js", "startLine": 4}, "line": 6, "column": 6},
                    {"symbol": {"name": "expressionCaller", "filePath": "file2.js", "startLine": 1}, "line": 2, "column": 12},
                    {"symbol": {"name": "remoteCaller", "filePath": "file3.js", "startLine": 1}, "line": 3, "column": 6},
                ]},
            ]) },
            Query { operation: "callees", symbol: "localCaller", expected: json!([
                {"name": "localCaller", "filePath": "test.js", "startLine": 4,
                 "edges": [
                    {"symbol": {"name": "Bar", "filePath": "test.js", "startLine": 1}, "line": 5, "column": 16},
                    {"symbol": {"name": "Bar::testMethod", "filePath": "test.js", "startLine": 2}, "line": 6, "column": 6},
                ]},
            ]) },
            Query { operation: "callees", symbol: "expressionCaller", expected: json!([
                {"name": "expressionCaller", "filePath": "file2.js", "startLine": 1,
                 "edges": [
                    {"symbol": {"name": "Bar", "filePath": "test.js", "startLine": 1}, "line": 2, "column": 6},
                    {"symbol": {"name": "Bar::testMethod", "filePath": "test.js", "startLine": 2}, "line": 2, "column": 12},
                ]},
            ]) },
            Query { operation: "callees", symbol: "remoteCaller", expected: json!([
                {"name": "remoteCaller", "filePath": "file3.js", "startLine": 1,
                 "edges": [
                    {"symbol": {"name": "Bar", "filePath": "test.js", "startLine": 1}, "line": 2, "column": 16},
                    {"symbol": {"name": "Bar::testMethod", "filePath": "test.js", "startLine": 2}, "line": 3, "column": 6},
                ]},
            ]) },
        ],
    }).await;
}

// https://github.com/microsoft/TypeScript/blob/fed0bf24149fb1ed36039212648bafdafc1ea10e/tsc/internal/fourslash/tests/callHierarchyClassStaticBlock_test.go
// https://github.com/microsoft/TypeScript/blob/fed0bf24149fb1ed36039212648bafdafc1ea10e/tsc/testdata/baselines/reference/fourslash/callHierarchy/callHierarchyClassStaticBlock.callHierarchy.txt
// https://github.com/microsoft/TypeScript/blob/fed0bf24149fb1ed36039212648bafdafc1ea10e/tsc/testdata/baselines/reference/fourslash/callHierarchy/callHierarchyClassStaticBlock2.callHierarchy.txt
// Run the TypeScript upstream static-block sample as .js: the sample contains only JavaScript
// syntax. This is a language-mode adaptation, not an independent JavaScript upstream test. Add the
// canonical C scope and name the initializer C::static {}. Calls inside nested foo/bar belong to
// those functions; only the direct foo() belongs to the static block. Preserve repeated baz sites.
#[tokio::test]
async fn javascript_static_block_call_ownership() {
    check_case(Case {
        name: "javascript_static_block_call_ownership",
        files: vec![
            SourceFile { path: "static.js", source: "class C {\n    static {\n        function foo() {\n            bar();\n        }\n\n        function bar() {\n            baz();\n            quxx();\n            baz();\n        }\n\n        foo();\n    }\n}\n\nfunction baz() {\n}\n\nfunction quxx() {\n}\n" },
        ],
        queries: vec![
            Query { operation: "callees", symbol: "C::static {}", expected: json!([
                {"name": "C::static {}", "filePath": "static.js", "startLine": 2,
                 "edges": [
                    {"symbol": {"name": "C::foo", "filePath": "static.js", "startLine": 3}, "line": 13, "column": 8},
                ]},
            ]) },
            Query { operation: "callers", symbol: "C::foo", expected: json!([
                {"name": "C::foo", "filePath": "static.js", "startLine": 3,
                 "edges": [
                    {"symbol": {"name": "C::static {}", "filePath": "static.js", "startLine": 2}, "line": 13, "column": 8},
                ]},
            ]) },
            Query { operation: "callers", symbol: "C::bar", expected: json!([
                {"name": "C::bar", "filePath": "static.js", "startLine": 7,
                 "edges": [
                    {"symbol": {"name": "C::foo", "filePath": "static.js", "startLine": 3}, "line": 4, "column": 12},
                ]},
            ]) },
            Query { operation: "callees", symbol: "C::bar", expected: json!([
                {"name": "C::bar", "filePath": "static.js", "startLine": 7,
                 "edges": [
                    {"symbol": {"name": "baz", "filePath": "static.js", "startLine": 17}, "line": [8, 10], "column": 12},
                    {"symbol": {"name": "quxx", "filePath": "static.js", "startLine": 20}, "line": 9, "column": 12},
                ]},
            ]) },
        ],
    }).await;
}

// https://github.com/golang/tools/blob/31c58979aa257a4ccd3da5b6102a5d77cfc8d41b/gopls/internal/test/marker/testdata/callhierarchy/issue75230.txt
// gopls issue75230 excludes calls to builtin error.Error, which has no package definition.
// Preserve the source after removing marker comments: outgoing A is empty, not a fabricated
// workspace match or a crash.
#[tokio::test]
async fn go_builtin_method_without_indexed_target() {
    check_case(Case {
        name: "go_builtin_method_without_indexed_target",
        files: vec![SourceFile {
            path: "a/a.go",
            source: "package a\n\nfunc A(err error) string {\n\treturn err.Error()\n}\n",
        }],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "A",
                expected: json!([
                    {"name": "A", "filePath": "a/a.go", "startLine": 3,
                     "edges": []},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "A",
                expected: json!([
                    {"name": "A", "filePath": "a/a.go", "startLine": 3,
                     "edges": []},
                ]),
            },
        ],
    })
    .await;
}

// https://github.com/golang/tools/blob/31c58979aa257a4ccd3da5b6102a5d77cfc8d41b/gopls/internal/test/marker/testdata/callhierarchy/callhierarchy.txt
// Adapt the incoming.A -> callhierarchy.D -> outgoing.B subgraph of gopls callhierarchy. Remove
// unrelated declarations and D calls, remapping source coordinates. Retain package-qualified
// imports and use a go.mod to define the module root. Public symbol names remain A, D and B;
// packages are represented by their source paths in this API.
#[tokio::test]
async fn go_cross_package_call_chain() {
    check_case(Case {
        name: "go_cross_package_call_chain",
        files: vec![
            SourceFile { path: "go.mod", source: "module golang.org/lsptests/callhierarchy\ngo 1.17\n" },
            SourceFile { path: "incoming/incoming.go", source: "package incoming\n\nimport \"golang.org/lsptests/callhierarchy\"\n\nfunc A() {\n\tcallhierarchy.D()\n}\n" },
            SourceFile { path: "hierarchy.go", source: "package callhierarchy\n\nimport \"golang.org/lsptests/callhierarchy/outgoing\"\n\nfunc D() {\n\toutgoing.B()\n}\n" },
            SourceFile { path: "outgoing/outgoing.go", source: "package outgoing\n\nfunc B() {}\n" },
        ],
        queries: vec![
            Query { operation: "callers", symbol: "D", expected: json!([
                {"name": "D", "filePath": "hierarchy.go", "startLine": 5,
                 "edges": [
                    {"symbol": {"name": "A", "filePath": "incoming/incoming.go", "startLine": 5}, "line": 6, "column": 15},
                ]},
            ]) },
            Query { operation: "callees", symbol: "D", expected: json!([
                {"name": "D", "filePath": "hierarchy.go", "startLine": 5,
                 "edges": [
                    {"symbol": {"name": "B", "filePath": "outgoing/outgoing.go", "startLine": 3}, "line": 6, "column": 10},
                ]},
            ]) },
            Query { operation: "callers", symbol: "B", expected: json!([
                {"name": "B", "filePath": "outgoing/outgoing.go", "startLine": 3,
                 "edges": [
                    {"symbol": {"name": "D", "filePath": "hierarchy.go", "startLine": 5}, "line": 6, "column": 10},
                ]},
            ]) },
            Query { operation: "callees", symbol: "A", expected: json!([
                {"name": "A", "filePath": "incoming/incoming.go", "startLine": 5,
                 "edges": [
                    {"symbol": {"name": "D", "filePath": "hierarchy.go", "startLine": 5}, "line": 6, "column": 15},
                ]},
            ]) },
        ],
    }).await;
}

// https://github.com/microsoft/TypeScript/blob/fed0bf24149fb1ed36039212648bafdafc1ea10e/tsc/internal/fourslash/tests/callHierarchyAccessor_test.go
// https://github.com/microsoft/TypeScript/blob/fed0bf24149fb1ed36039212648bafdafc1ea10e/tsc/testdata/baselines/reference/fourslash/callHierarchy/callHierarchyAccessor.callHierarchy.txt
// TypeScript Accessor: reading new C().bar invokes its getter, while the getter owns baz(). Adopt
// the upstream getter-call semantics and canonical C::bar scope; these queries avoid the separate
// constructor call from foo.
#[tokio::test]
async fn typescript_getter_calls() {
    check_case(Case {
        name: "typescript_getter_calls",
        files: vec![
            SourceFile { path: "accessor.ts", source: "function foo() {\n    new C().bar;\n}\n\nclass C {\n    get bar() {\n        return baz();\n    }\n}\n\nfunction baz() {\n}\n" },
        ],
        queries: vec![
            Query { operation: "callers", symbol: "C::bar", expected: json!([
                {"name": "C::bar", "filePath": "accessor.ts", "startLine": 6,
                 "edges": [
                    {"symbol": {"name": "foo", "filePath": "accessor.ts", "startLine": 1}, "line": 2, "column": 12},
                ]},
            ]) },
            Query { operation: "callees", symbol: "C::bar", expected: json!([
                {"name": "C::bar", "filePath": "accessor.ts", "startLine": 6,
                 "edges": [
                    {"symbol": {"name": "baz", "filePath": "accessor.ts", "startLine": 11}, "line": 7, "column": 15},
                ]},
            ]) },
            Query { operation: "callers", symbol: "baz", expected: json!([
                {"name": "baz", "filePath": "accessor.ts", "startLine": 11,
                 "edges": [
                    {"symbol": {"name": "C::bar", "filePath": "accessor.ts", "startLine": 6}, "line": 7, "column": 15},
                ]},
            ]) },
        ],
    }).await;
}

// https://github.com/microsoft/TypeScript/blob/fed0bf24149fb1ed36039212648bafdafc1ea10e/tsc/internal/fourslash/tests/callHierarchyUnclosedTemplateExprNoCrash1_test.go
// Adapt the malformed-template source from the upstream no-crash regression. Prefix an independent
// valid target and caller, and query those instead of reproducing the LSP cursor protocol. The
// added relationship assertions require error recovery to preserve valid code without inventing
// calls from the broken template.
#[tokio::test]
async fn typescript_malformed_template_preserves_valid_calls() {
    check_case(Case {
        name: "typescript_malformed_template_preserves_valid_calls",
        files: vec![
            SourceFile { path: "broken.ts", source: "function target() {}\nfunction caller() { target(); }\nfunction updateBadge() {\n    const header = `<div class=\"sub\">${format`;\n    const badge = `<div class=\"badge\">`;\n}\n" },
        ],
        queries: vec![
            Query { operation: "callers", symbol: "target", expected: json!([
                {"name": "target", "filePath": "broken.ts", "startLine": 1,
                 "edges": [
                    {"symbol": {"name": "caller", "filePath": "broken.ts", "startLine": 2}, "line": 2, "column": 20},
                ]},
            ]) },
            Query { operation: "callees", symbol: "caller", expected: json!([
                {"name": "caller", "filePath": "broken.ts", "startLine": 2,
                 "edges": [
                    {"symbol": {"name": "target", "filePath": "broken.ts", "startLine": 1}, "line": 2, "column": 20},
                ]},
            ]) },
        ],
    }).await;
}

// zvec-grep-specific regression for reference-versus-call semantics. Assigning target to alias is
// not a calls edge; alias() is. The no-call holder must not appear in target callers. This
// deliberately tests actual invocation semantics rather than copying LSP implementations that
// count bare references as calls.
#[tokio::test]
async fn python_function_reference_and_alias_call() {
    check_case(Case {
        name: "python_function_reference_and_alias_call",
        files: vec![
            SourceFile { path: "references.py", source: "def target():\n    pass\n\ndef holder():\n    alias = target\n    return alias\n\ndef caller():\n    alias = target\n    alias()\n" },
        ],
        queries: vec![
            Query { operation: "callers", symbol: "target", expected: json!([
                {"name": "target", "filePath": "references.py", "startLine": 1,
                 "edges": [
                    {"symbol": {"name": "caller", "filePath": "references.py", "startLine": 8}, "line": 10, "column": 4},
                ]},
            ]) },
            Query { operation: "callees", symbol: "holder", expected: json!([
                {"name": "holder", "filePath": "references.py", "startLine": 4,
                 "edges": []},
            ]) },
            Query { operation: "callees", symbol: "caller", expected: json!([
                {"name": "caller", "filePath": "references.py", "startLine": 8,
                 "edges": [
                    {"symbol": {"name": "target", "filePath": "references.py", "startLine": 1}, "line": 10, "column": 4},
                ]},
            ]) },
        ],
    }).await;
}

// https://github.com/eclipse-cdt/cdt/blob/4c37839cfbe3dde7bdfa398d9f703644052f5429/core/org.eclipse.cdt.ui.tests/ui/org/eclipse/cdt/ui/tests/callhierarchy/BasicCallHierarchyTest.java
// CDT IDE testFunctionsC. Preserve its prototype-only target and function definition while using
// standard int main(void) syntax. A declared external prototype is a known symbol; main has one
// call to proto and one to func. Incoming expectations come from the upstream tree, outgoing is
// their inverse.
#[tokio::test]
async fn c_prototype_and_definition_calls() {
    check_case(Case {
        name: "c_prototype_and_definition_calls",
        files: vec![
            SourceFile { path: "functions.c", source: "void proto(void);\nvoid func(void) {\n}\nint main(void) {\n   proto();\n   func();\n   return 0;\n}\n" },
        ],
        queries: vec![
            Query { operation: "callers", symbol: "proto", expected: json!([
                {"name": "proto", "filePath": "functions.c", "startLine": 1,
                 "edges": [
                    {"symbol": {"name": "main", "filePath": "functions.c", "startLine": 4}, "line": 5, "column": 3},
                ]},
            ]) },
            Query { operation: "callers", symbol: "func", expected: json!([
                {"name": "func", "filePath": "functions.c", "startLine": 2,
                 "edges": [
                    {"symbol": {"name": "main", "filePath": "functions.c", "startLine": 4}, "line": 6, "column": 3},
                ]},
            ]) },
            Query { operation: "callees", symbol: "main", expected: json!([
                {"name": "main", "filePath": "functions.c", "startLine": 4,
                 "edges": [
                    {"symbol": {"name": "proto", "filePath": "functions.c", "startLine": 1}, "line": 5, "column": 3},
                    {"symbol": {"name": "func", "filePath": "functions.c", "startLine": 2}, "line": 6, "column": 3},
                ]},
            ]) },
        ],
    }).await;
}

// https://github.com/razzmatazz/csharp-language-server/blob/72671fc7fa810c158864e72485db68e8b9b25c3d/tests/CSharpLanguageServer.Tests/CallHierarchyTests.fs
// Translate the this/base constructor-chain scenario from csharp-ls
// testCallHierarchyOutgoingCallsIncludeConstructorInitializers into Java this/super syntax. This
// is not a C# parser test or a verbatim Java upstream case. Query both same-named Derived
// constructor definitions separately; the parameterless constructor targets the sized constructor,
// which targets Base.
#[tokio::test]
async fn java_explicit_constructor_chain() {
    check_case(Case {
        name: "java_explicit_constructor_chain",
        files: vec![
            SourceFile { path: "Constructors.java", source: "class Base {\n    Base(int size) {}\n}\nclass Derived extends Base {\n    Derived() {\n        this(5);\n    }\n    Derived(int size) {\n        super(size);\n    }\n}\n" },
        ],
        queries: vec![
            Query { operation: "callees", symbol: "Derived::Derived", expected: json!([
                {"name": "Derived::Derived", "filePath": "Constructors.java", "startLine": 5,
                 "edges": [
                    {"symbol": {"name": "Derived::Derived", "filePath": "Constructors.java", "startLine": 8}, "line": 6, "column": 8},
                ]},
                {"name": "Derived::Derived", "filePath": "Constructors.java", "startLine": 8,
                 "edges": [
                    {"symbol": {"name": "Base::Base", "filePath": "Constructors.java", "startLine": 2}, "line": 9, "column": 8},
                ]},
            ]) },
            Query { operation: "callers", symbol: "Derived::Derived", expected: json!([
                {"name": "Derived::Derived", "filePath": "Constructors.java", "startLine": 5,
                 "edges": []},
                {"name": "Derived::Derived", "filePath": "Constructors.java", "startLine": 8,
                 "edges": [
                    {"symbol": {"name": "Derived::Derived", "filePath": "Constructors.java", "startLine": 5}, "line": 6, "column": 8},
                ]},
            ]) },
            Query { operation: "callers", symbol: "Base::Base", expected: json!([
                {"name": "Base::Base", "filePath": "Constructors.java", "startLine": 2,
                 "edges": [
                    {"symbol": {"name": "Derived::Derived", "filePath": "Constructors.java", "startLine": 8}, "line": 9, "column": 8},
                ]},
            ]) },
        ],
    }).await;
}

// zvec-grep-specific regression. Three overloads differ by arity or parameter type. Calls with no
// argument, integer and string must bind to the corresponding definition rather than all same-
// named functions. Unqualified callers lookup preserves three definition groups.
#[tokio::test]
async fn cpp_overload_arity_and_type() {
    check_case(Case {
        name: "cpp_overload_arity_and_type",
        files: vec![
            SourceFile { path: "overloads.cc", source: "void run() {}\nvoid run(int value) {}\nvoid run(const char *value) {}\nvoid caller() {\n    run();\n    run(1);\n    run(\"text\");\n}\n" },
        ],
        queries: vec![
            Query { operation: "callers", symbol: "run", expected: json!([
                {"name": "run", "filePath": "overloads.cc", "startLine": 1,
                 "edges": [
                    {"symbol": {"name": "caller", "filePath": "overloads.cc", "startLine": 4}, "line": 5, "column": 4},
                ]},
                {"name": "run", "filePath": "overloads.cc", "startLine": 2,
                 "edges": [
                    {"symbol": {"name": "caller", "filePath": "overloads.cc", "startLine": 4}, "line": 6, "column": 4},
                ]},
                {"name": "run", "filePath": "overloads.cc", "startLine": 3,
                 "edges": [
                    {"symbol": {"name": "caller", "filePath": "overloads.cc", "startLine": 4}, "line": 7, "column": 4},
                ]},
            ]) },
            Query { operation: "callees", symbol: "caller", expected: json!([
                {"name": "caller", "filePath": "overloads.cc", "startLine": 4,
                 "edges": [
                    {"symbol": {"name": "run", "filePath": "overloads.cc", "startLine": 1}, "line": 5, "column": 4},
                    {"symbol": {"name": "run", "filePath": "overloads.cc", "startLine": 2}, "line": 6, "column": 4},
                    {"symbol": {"name": "run", "filePath": "overloads.cc", "startLine": 3}, "line": 7, "column": 4},
                ]},
            ]) },
        ],
    }).await;
}

// zvec-grep regression: extend typescript_same_name_scopes to rust.
// Modules provide separate lexical namespaces.
// Bare-name queries preserve both groups; each group must contain only its own callsite.
#[tokio::test]
async fn rust_same_name_scopes() {
    check_case(Case {
        name: "rust_same_name_scopes",
        files: vec![SourceFile {
            path: "scopes.rs",
            source: r"mod left {
    pub fn work() {}
    pub fn caller() {
        work();
    }
}
mod right {
    pub fn work() {}
    pub fn caller() {
        work();
    }
}
",
        }],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "work",
                expected: json!([
                    {"name": "left::work", "filePath": "scopes.rs", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "left::caller", "filePath": "scopes.rs", "startLine": 3}, "line": 4, "column": 8},
                    ]},
                    {"name": "right::work", "filePath": "scopes.rs", "startLine": 8,
                     "edges": [
                        {"symbol": {"name": "right::caller", "filePath": "scopes.rs", "startLine": 9}, "line": 10, "column": 8},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([
                    {"name": "left::caller", "filePath": "scopes.rs", "startLine": 3,
                     "edges": [
                        {"symbol": {"name": "left::work", "filePath": "scopes.rs", "startLine": 2}, "line": 4, "column": 8},
                    ]},
                    {"name": "right::caller", "filePath": "scopes.rs", "startLine": 9,
                     "edges": [
                        {"symbol": {"name": "right::work", "filePath": "scopes.rs", "startLine": 8}, "line": 10, "column": 8},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "left::work",
                expected: json!([
                    {"name": "left::work", "filePath": "scopes.rs", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "left::caller", "filePath": "scopes.rs", "startLine": 3}, "line": 4, "column": 8},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "left::caller",
                expected: json!([
                    {"name": "left::caller", "filePath": "scopes.rs", "startLine": 3,
                     "edges": [
                        {"symbol": {"name": "left::work", "filePath": "scopes.rs", "startLine": 2}, "line": 4, "column": 8},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "right::work",
                expected: json!([
                    {"name": "right::work", "filePath": "scopes.rs", "startLine": 8,
                     "edges": [
                        {"symbol": {"name": "right::caller", "filePath": "scopes.rs", "startLine": 9}, "line": 10, "column": 8},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "right::caller",
                expected: json!([
                    {"name": "right::caller", "filePath": "scopes.rs", "startLine": 9,
                     "edges": [
                        {"symbol": {"name": "right::work", "filePath": "scopes.rs", "startLine": 8}, "line": 10, "column": 8},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep regression: extend typescript_same_name_scopes to cpp.
// Namespaces provide separate lexical lookup environments.
// Bare-name queries preserve both groups; each group must contain only its own callsite.
#[tokio::test]
async fn cpp_same_name_scopes() {
    check_case(Case {
        name: "cpp_same_name_scopes",
        files: vec![SourceFile {
            path: "scopes.cc",
            source: r"namespace Left {
    void work() {}
    void caller() {
        work();
    }
}
namespace Right {
    void work() {}
    void caller() {
        work();
    }
}
",
        }],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "work",
                expected: json!([
                    {"name": "Left::work", "filePath": "scopes.cc", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "Left::caller", "filePath": "scopes.cc", "startLine": 3}, "line": 4, "column": 8},
                    ]},
                    {"name": "Right::work", "filePath": "scopes.cc", "startLine": 8,
                     "edges": [
                        {"symbol": {"name": "Right::caller", "filePath": "scopes.cc", "startLine": 9}, "line": 10, "column": 8},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([
                    {"name": "Left::caller", "filePath": "scopes.cc", "startLine": 3,
                     "edges": [
                        {"symbol": {"name": "Left::work", "filePath": "scopes.cc", "startLine": 2}, "line": 4, "column": 8},
                    ]},
                    {"name": "Right::caller", "filePath": "scopes.cc", "startLine": 9,
                     "edges": [
                        {"symbol": {"name": "Right::work", "filePath": "scopes.cc", "startLine": 8}, "line": 10, "column": 8},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Left::work",
                expected: json!([
                    {"name": "Left::work", "filePath": "scopes.cc", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "Left::caller", "filePath": "scopes.cc", "startLine": 3}, "line": 4, "column": 8},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "Left::caller",
                expected: json!([
                    {"name": "Left::caller", "filePath": "scopes.cc", "startLine": 3,
                     "edges": [
                        {"symbol": {"name": "Left::work", "filePath": "scopes.cc", "startLine": 2}, "line": 4, "column": 8},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Right::work",
                expected: json!([
                    {"name": "Right::work", "filePath": "scopes.cc", "startLine": 8,
                     "edges": [
                        {"symbol": {"name": "Right::caller", "filePath": "scopes.cc", "startLine": 9}, "line": 10, "column": 8},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "Right::caller",
                expected: json!([
                    {"name": "Right::caller", "filePath": "scopes.cc", "startLine": 9,
                     "edges": [
                        {"symbol": {"name": "Right::work", "filePath": "scopes.cc", "startLine": 8}, "line": 10, "column": 8},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep regression: extend typescript_same_name_scopes to java.
// Unqualified static calls bind inside their declaring class.
// Bare-name queries preserve both groups; each group must contain only its own callsite.
#[tokio::test]
async fn java_same_name_scopes() {
    check_case(Case {
        name: "java_same_name_scopes",
        files: vec![SourceFile {
            path: "Scopes.java",
            source: r"class Left {
    static void work() {}
    static void caller() {
        work();
    }
}
class Right {
    static void work() {}
    static void caller() {
        work();
    }
}
",
        }],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "work",
                expected: json!([
                    {"name": "Left::work", "filePath": "Scopes.java", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "Left::caller", "filePath": "Scopes.java", "startLine": 3}, "line": 4, "column": 8},
                    ]},
                    {"name": "Right::work", "filePath": "Scopes.java", "startLine": 8,
                     "edges": [
                        {"symbol": {"name": "Right::caller", "filePath": "Scopes.java", "startLine": 9}, "line": 10, "column": 8},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([
                    {"name": "Left::caller", "filePath": "Scopes.java", "startLine": 3,
                     "edges": [
                        {"symbol": {"name": "Left::work", "filePath": "Scopes.java", "startLine": 2}, "line": 4, "column": 8},
                    ]},
                    {"name": "Right::caller", "filePath": "Scopes.java", "startLine": 9,
                     "edges": [
                        {"symbol": {"name": "Right::work", "filePath": "Scopes.java", "startLine": 8}, "line": 10, "column": 8},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Left::work",
                expected: json!([
                    {"name": "Left::work", "filePath": "Scopes.java", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "Left::caller", "filePath": "Scopes.java", "startLine": 3}, "line": 4, "column": 8},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "Left::caller",
                expected: json!([
                    {"name": "Left::caller", "filePath": "Scopes.java", "startLine": 3,
                     "edges": [
                        {"symbol": {"name": "Left::work", "filePath": "Scopes.java", "startLine": 2}, "line": 4, "column": 8},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Right::work",
                expected: json!([
                    {"name": "Right::work", "filePath": "Scopes.java", "startLine": 8,
                     "edges": [
                        {"symbol": {"name": "Right::caller", "filePath": "Scopes.java", "startLine": 9}, "line": 10, "column": 8},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "Right::caller",
                expected: json!([
                    {"name": "Right::caller", "filePath": "Scopes.java", "startLine": 9,
                     "edges": [
                        {"symbol": {"name": "Right::work", "filePath": "Scopes.java", "startLine": 8}, "line": 10, "column": 8},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep regression: extend typescript_same_name_scopes to javascript.
// Resolve this.work() against the enclosing receiver class.
// Bare-name queries preserve both groups; each group must contain only its own callsite.
#[tokio::test]
async fn javascript_same_name_scopes() {
    check_case(Case {
        name: "javascript_same_name_scopes",
        files: vec![SourceFile {
            path: "scopes.js",
            source: r"class Left {
    work() {}
    caller() {
        this.work();
    }
}
class Right {
    work() {}
    caller() {
        this.work();
    }
}
",
        }],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "work",
                expected: json!([
                    {"name": "Left::work", "filePath": "scopes.js", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "Left::caller", "filePath": "scopes.js", "startLine": 3}, "line": 4, "column": 13},
                    ]},
                    {"name": "Right::work", "filePath": "scopes.js", "startLine": 8,
                     "edges": [
                        {"symbol": {"name": "Right::caller", "filePath": "scopes.js", "startLine": 9}, "line": 10, "column": 13},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([
                    {"name": "Left::caller", "filePath": "scopes.js", "startLine": 3,
                     "edges": [
                        {"symbol": {"name": "Left::work", "filePath": "scopes.js", "startLine": 2}, "line": 4, "column": 13},
                    ]},
                    {"name": "Right::caller", "filePath": "scopes.js", "startLine": 9,
                     "edges": [
                        {"symbol": {"name": "Right::work", "filePath": "scopes.js", "startLine": 8}, "line": 10, "column": 13},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Left::work",
                expected: json!([
                    {"name": "Left::work", "filePath": "scopes.js", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "Left::caller", "filePath": "scopes.js", "startLine": 3}, "line": 4, "column": 13},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "Left::caller",
                expected: json!([
                    {"name": "Left::caller", "filePath": "scopes.js", "startLine": 3,
                     "edges": [
                        {"symbol": {"name": "Left::work", "filePath": "scopes.js", "startLine": 2}, "line": 4, "column": 13},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Right::work",
                expected: json!([
                    {"name": "Right::work", "filePath": "scopes.js", "startLine": 8,
                     "edges": [
                        {"symbol": {"name": "Right::caller", "filePath": "scopes.js", "startLine": 9}, "line": 10, "column": 13},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "Right::caller",
                expected: json!([
                    {"name": "Right::caller", "filePath": "scopes.js", "startLine": 9,
                     "edges": [
                        {"symbol": {"name": "Right::work", "filePath": "scopes.js", "startLine": 8}, "line": 10, "column": 13},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep regression: extend typescript_same_name_scopes to python.
// Resolve self.work() against the enclosing receiver class.
// Bare-name queries preserve both groups; each group must contain only its own callsite.
#[tokio::test]
async fn python_same_name_scopes() {
    check_case(Case {
        name: "python_same_name_scopes",
        files: vec![SourceFile {
            path: "scopes.py",
            source: r"class Left:
    def work(self):
        pass
    def caller(self):
        self.work()

class Right:
    def work(self):
        pass
    def caller(self):
        self.work()
",
        }],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "work",
                expected: json!([
                    {"name": "Left::work", "filePath": "scopes.py", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "Left::caller", "filePath": "scopes.py", "startLine": 4}, "line": 5, "column": 13},
                    ]},
                    {"name": "Right::work", "filePath": "scopes.py", "startLine": 8,
                     "edges": [
                        {"symbol": {"name": "Right::caller", "filePath": "scopes.py", "startLine": 10}, "line": 11, "column": 13},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([
                    {"name": "Left::caller", "filePath": "scopes.py", "startLine": 4,
                     "edges": [
                        {"symbol": {"name": "Left::work", "filePath": "scopes.py", "startLine": 2}, "line": 5, "column": 13},
                    ]},
                    {"name": "Right::caller", "filePath": "scopes.py", "startLine": 10,
                     "edges": [
                        {"symbol": {"name": "Right::work", "filePath": "scopes.py", "startLine": 8}, "line": 11, "column": 13},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Left::work",
                expected: json!([
                    {"name": "Left::work", "filePath": "scopes.py", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "Left::caller", "filePath": "scopes.py", "startLine": 4}, "line": 5, "column": 13},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "Left::caller",
                expected: json!([
                    {"name": "Left::caller", "filePath": "scopes.py", "startLine": 4,
                     "edges": [
                        {"symbol": {"name": "Left::work", "filePath": "scopes.py", "startLine": 2}, "line": 5, "column": 13},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Right::work",
                expected: json!([
                    {"name": "Right::work", "filePath": "scopes.py", "startLine": 8,
                     "edges": [
                        {"symbol": {"name": "Right::caller", "filePath": "scopes.py", "startLine": 10}, "line": 11, "column": 13},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "Right::caller",
                expected: json!([
                    {"name": "Right::caller", "filePath": "scopes.py", "startLine": 10,
                     "edges": [
                        {"symbol": {"name": "Right::work", "filePath": "scopes.py", "startLine": 8}, "line": 11, "column": 13},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep regression: extend typescript_same_name_scopes to go.
// Receiver types, including a pointer receiver on Right, disambiguate same-named methods.
// Bare-name queries preserve both groups; each group must contain only its own callsite.
#[tokio::test]
async fn go_same_name_scopes() {
    check_case(Case {
        name: "go_same_name_scopes",
        files: vec![SourceFile {
            path: "scopes.go",
            source: r"package scopes

type Left struct{}
func (self Left) work() {}
func (self Left) caller() {
    self.work()
}

type Right struct{}
func (self *Right) work() {}
func (self *Right) caller() {
    self.work()
}
",
        }],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "work",
                expected: json!([
                    {"name": "Left::work", "filePath": "scopes.go", "startLine": 4,
                     "edges": [
                        {"symbol": {"name": "Left::caller", "filePath": "scopes.go", "startLine": 5}, "line": 6, "column": 9},
                    ]},
                    {"name": "Right::work", "filePath": "scopes.go", "startLine": 10,
                     "edges": [
                        {"symbol": {"name": "Right::caller", "filePath": "scopes.go", "startLine": 11}, "line": 12, "column": 9},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([
                    {"name": "Left::caller", "filePath": "scopes.go", "startLine": 5,
                     "edges": [
                        {"symbol": {"name": "Left::work", "filePath": "scopes.go", "startLine": 4}, "line": 6, "column": 9},
                    ]},
                    {"name": "Right::caller", "filePath": "scopes.go", "startLine": 11,
                     "edges": [
                        {"symbol": {"name": "Right::work", "filePath": "scopes.go", "startLine": 10}, "line": 12, "column": 9},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Left::work",
                expected: json!([
                    {"name": "Left::work", "filePath": "scopes.go", "startLine": 4,
                     "edges": [
                        {"symbol": {"name": "Left::caller", "filePath": "scopes.go", "startLine": 5}, "line": 6, "column": 9},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "Left::caller",
                expected: json!([
                    {"name": "Left::caller", "filePath": "scopes.go", "startLine": 5,
                     "edges": [
                        {"symbol": {"name": "Left::work", "filePath": "scopes.go", "startLine": 4}, "line": 6, "column": 9},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Right::work",
                expected: json!([
                    {"name": "Right::work", "filePath": "scopes.go", "startLine": 10,
                     "edges": [
                        {"symbol": {"name": "Right::caller", "filePath": "scopes.go", "startLine": 11}, "line": 12, "column": 9},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "Right::caller",
                expected: json!([
                    {"name": "Right::caller", "filePath": "scopes.go", "startLine": 11,
                     "edges": [
                        {"symbol": {"name": "Right::work", "filePath": "scopes.go", "startLine": 10}, "line": 12, "column": 9},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep regression: extend typescript_same_name_scopes to c.
// C has no namespace qualifier in this API: distinguish file-local static symbols by file
// identity. Identical contents in two files must still yield separate definitions.
// Bare-name queries preserve both groups; each group must contain only its own callsite.
#[tokio::test]
async fn c_same_name_scopes() {
    check_case(Case {
        name: "c_same_name_scopes",
        files: vec![
            SourceFile {
                path: "left.c",
                source: r"static void work(void) {}
static void caller(void) {
    work();
}
",
            },
            SourceFile {
                path: "right.c",
                source: r"static void work(void) {}
static void caller(void) {
    work();
}
",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "work",
                expected: json!([
                    {"name": "work", "filePath": "left.c", "startLine": 1,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "left.c", "startLine": 2}, "line": 3, "column": 4},
                    ]},
                    {"name": "work", "filePath": "right.c", "startLine": 1,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "right.c", "startLine": 2}, "line": 3, "column": 4},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([
                    {"name": "caller", "filePath": "left.c", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "work", "filePath": "left.c", "startLine": 1}, "line": 3, "column": 4},
                    ]},
                    {"name": "caller", "filePath": "right.c", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "work", "filePath": "right.c", "startLine": 1}, "line": 3, "column": 4},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific lexical-shadowing and caller-ownership regression. The nested work shadows
// the global work. Its call to target belongs only to outer::work, not outer; outer has only the
// direct call to the nested work. Canonical nested-function names use the containing function
// scope.
#[tokio::test]
async fn rust_shadowed_nested_function() {
    check_case(Case {
        name: "rust_shadowed_nested_function",
        files: vec![SourceFile {
            path: "shadow.rs",
            source: r"fn work() {}
fn target() {}
fn outer() {
    fn work() {
        target();
    }
    work();
}
",
        }],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "work",
                expected: json!([
                    {"name": "work", "filePath": "shadow.rs", "startLine": 1,
                     "edges": []},
                    {"name": "outer::work", "filePath": "shadow.rs", "startLine": 4,
                     "edges": [
                        {"symbol": {"name": "outer", "filePath": "shadow.rs", "startLine": 3}, "line": 7, "column": 4},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "target",
                expected: json!([
                    {"name": "target", "filePath": "shadow.rs", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "outer::work", "filePath": "shadow.rs", "startLine": 4}, "line": 5, "column": 8},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "outer",
                expected: json!([
                    {"name": "outer", "filePath": "shadow.rs", "startLine": 3,
                     "edges": [
                        {"symbol": {"name": "outer::work", "filePath": "shadow.rs", "startLine": 4}, "line": 7, "column": 4},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "outer::work",
                expected: json!([
                    {"name": "outer::work", "filePath": "shadow.rs", "startLine": 4,
                     "edges": [
                        {"symbol": {"name": "target", "filePath": "shadow.rs", "startLine": 2}, "line": 5, "column": 8},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific lexical-shadowing and caller-ownership regression. The nested work shadows
// the global work. Its call to target belongs only to outer::work, not outer; outer has only the
// direct call to the nested work. Canonical nested-function names use the containing function
// scope.
#[tokio::test]
async fn javascript_shadowed_nested_function() {
    check_case(Case {
        name: "javascript_shadowed_nested_function",
        files: vec![SourceFile {
            path: "shadow.js",
            source: r"function work() {}
function target() {}
function outer() {
    function work() {
        target();
    }
    work();
}
",
        }],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "work",
                expected: json!([
                    {"name": "work", "filePath": "shadow.js", "startLine": 1,
                     "edges": []},
                    {"name": "outer::work", "filePath": "shadow.js", "startLine": 4,
                     "edges": [
                        {"symbol": {"name": "outer", "filePath": "shadow.js", "startLine": 3}, "line": 7, "column": 4},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "target",
                expected: json!([
                    {"name": "target", "filePath": "shadow.js", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "outer::work", "filePath": "shadow.js", "startLine": 4}, "line": 5, "column": 8},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "outer",
                expected: json!([
                    {"name": "outer", "filePath": "shadow.js", "startLine": 3,
                     "edges": [
                        {"symbol": {"name": "outer::work", "filePath": "shadow.js", "startLine": 4}, "line": 7, "column": 4},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "outer::work",
                expected: json!([
                    {"name": "outer::work", "filePath": "shadow.js", "startLine": 4,
                     "edges": [
                        {"symbol": {"name": "target", "filePath": "shadow.js", "startLine": 2}, "line": 5, "column": 8},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific lexical-shadowing and caller-ownership regression. The nested work shadows
// the global work. Its call to target belongs only to outer::work, not outer; outer has only the
// direct call to the nested work. Canonical nested-function names use the containing function
// scope.
#[tokio::test]
async fn python_shadowed_nested_function() {
    check_case(Case {
        name: "python_shadowed_nested_function",
        files: vec![SourceFile {
            path: "shadow.py",
            source: r"def work():
    pass
def target():
    pass
def outer():
    def work():
        target()
    work()
",
        }],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "work",
                expected: json!([
                    {"name": "work", "filePath": "shadow.py", "startLine": 1,
                     "edges": []},
                    {"name": "outer::work", "filePath": "shadow.py", "startLine": 6,
                     "edges": [
                        {"symbol": {"name": "outer", "filePath": "shadow.py", "startLine": 5}, "line": 8, "column": 4},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "target",
                expected: json!([
                    {"name": "target", "filePath": "shadow.py", "startLine": 3,
                     "edges": [
                        {"symbol": {"name": "outer::work", "filePath": "shadow.py", "startLine": 6}, "line": 7, "column": 8},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "outer",
                expected: json!([
                    {"name": "outer", "filePath": "shadow.py", "startLine": 5,
                     "edges": [
                        {"symbol": {"name": "outer::work", "filePath": "shadow.py", "startLine": 6}, "line": 8, "column": 4},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "outer::work",
                expected: json!([
                    {"name": "outer::work", "filePath": "shadow.py", "startLine": 6,
                     "edges": [
                        {"symbol": {"name": "target", "filePath": "shadow.py", "startLine": 3}, "line": 7, "column": 8},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific C shadowing regression. The local function pointer work hides the global work
// function, and its known initializer is target. Assignment is not a call; invoking work() reaches
// target, not the global same-named function.
#[tokio::test]
async fn c_local_function_pointer_shadows_global() {
    check_case(Case {
        name: "c_local_function_pointer_shadows_global",
        files: vec![SourceFile {
            path: "shadow.c",
            source: r"void work(void) {}
void target(void) {}
void caller(void) {
    void (*work)(void) = target;
    work();
}
",
        }],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "work",
                expected: json!([
                    {"name": "work", "filePath": "shadow.c", "startLine": 1,
                     "edges": []},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "target",
                expected: json!([
                    {"name": "target", "filePath": "shadow.c", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "shadow.c", "startLine": 3}, "line": 5, "column": 4},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([
                    {"name": "caller", "filePath": "shadow.c", "startLine": 3,
                     "edges": [
                        {"symbol": {"name": "target", "filePath": "shadow.c", "startLine": 2}, "line": 5, "column": 4},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific receiver-binding regression. Parameters a and b have different declared types
// with same-named run methods. Each callsite binds to its receiver type; neither target may
// inherit the other receiver call. This exercises parameters, not only self/this calls inside
// methods.
#[tokio::test]
async fn typescript_different_parameter_receivers() {
    check_case(Case {
        name: "typescript_different_parameter_receivers",
        files: vec![SourceFile {
            path: "receivers.ts",
            source: r"class Left {
    run() {}
}
class Right {
    run() {}
}
function caller(a: Left, b: Right) {
    a.run();
    b.run();
}
",
        }],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([
                    {"name": "caller", "filePath": "receivers.ts", "startLine": 7,
                     "edges": [
                        {"symbol": {"name": "Left::run", "filePath": "receivers.ts", "startLine": 2}, "line": 8, "column": 6},
                        {"symbol": {"name": "Right::run", "filePath": "receivers.ts", "startLine": 5}, "line": 9, "column": 6},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Left::run",
                expected: json!([
                    {"name": "Left::run", "filePath": "receivers.ts", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "receivers.ts", "startLine": 7}, "line": 8, "column": 6},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Right::run",
                expected: json!([
                    {"name": "Right::run", "filePath": "receivers.ts", "startLine": 5,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "receivers.ts", "startLine": 7}, "line": 9, "column": 6},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific receiver-binding regression. Parameters a and b have different declared types
// with same-named run methods. Each callsite binds to its receiver type; neither target may
// inherit the other receiver call. This exercises parameters, not only self/this calls inside
// methods.
#[tokio::test]
async fn java_different_parameter_receivers() {
    check_case(Case {
        name: "java_different_parameter_receivers",
        files: vec![SourceFile {
            path: "Receivers.java",
            source: r"class Left {
    void run() {}
}
class Right {
    void run() {}
}
class Use {
    static void caller(Left a, Right b) {
        a.run();
        b.run();
    }
}
",
        }],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Use::caller",
                expected: json!([
                    {"name": "Use::caller", "filePath": "Receivers.java", "startLine": 8,
                     "edges": [
                        {"symbol": {"name": "Left::run", "filePath": "Receivers.java", "startLine": 2}, "line": 9, "column": 10},
                        {"symbol": {"name": "Right::run", "filePath": "Receivers.java", "startLine": 5}, "line": 10, "column": 10},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Left::run",
                expected: json!([
                    {"name": "Left::run", "filePath": "Receivers.java", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "Use::caller", "filePath": "Receivers.java", "startLine": 8}, "line": 9, "column": 10},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Right::run",
                expected: json!([
                    {"name": "Right::run", "filePath": "Receivers.java", "startLine": 5,
                     "edges": [
                        {"symbol": {"name": "Use::caller", "filePath": "Receivers.java", "startLine": 8}, "line": 10, "column": 10},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific receiver-binding regression. Parameters a and b have different declared types
// with same-named run methods. Each callsite binds to its receiver type; neither target may
// inherit the other receiver call. This exercises parameters, not only self/this calls inside
// methods.
#[tokio::test]
async fn cpp_different_parameter_receivers() {
    check_case(Case {
        name: "cpp_different_parameter_receivers",
        files: vec![SourceFile {
            path: "receivers.cc",
            source: r"struct Left {
    void run() {}
};
struct Right {
    void run() {}
};
void caller(Left &a, Right &b) {
    a.run();
    b.run();
}
",
        }],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([
                    {"name": "caller", "filePath": "receivers.cc", "startLine": 7,
                     "edges": [
                        {"symbol": {"name": "Left::run", "filePath": "receivers.cc", "startLine": 2}, "line": 8, "column": 6},
                        {"symbol": {"name": "Right::run", "filePath": "receivers.cc", "startLine": 5}, "line": 9, "column": 6},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Left::run",
                expected: json!([
                    {"name": "Left::run", "filePath": "receivers.cc", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "receivers.cc", "startLine": 7}, "line": 8, "column": 6},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Right::run",
                expected: json!([
                    {"name": "Right::run", "filePath": "receivers.cc", "startLine": 5,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "receivers.cc", "startLine": 7}, "line": 9, "column": 6},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific receiver-binding regression. Parameters a and b have different declared types
// with same-named run methods. Each callsite binds to its receiver type; neither target may
// inherit the other receiver call. This exercises parameters, not only self/this calls inside
// methods.
#[tokio::test]
async fn go_different_parameter_receivers() {
    check_case(Case {
        name: "go_different_parameter_receivers",
        files: vec![SourceFile {
            path: "receivers.go",
            source: r"package receivers

type Left struct{}
func (a Left) run() {}
type Right struct{}
func (b *Right) run() {}
func caller(a Left, b *Right) {
    a.run()
    b.run()
}
",
        }],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([
                    {"name": "caller", "filePath": "receivers.go", "startLine": 7,
                     "edges": [
                        {"symbol": {"name": "Left::run", "filePath": "receivers.go", "startLine": 4}, "line": 8, "column": 6},
                        {"symbol": {"name": "Right::run", "filePath": "receivers.go", "startLine": 6}, "line": 9, "column": 6},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Left::run",
                expected: json!([
                    {"name": "Left::run", "filePath": "receivers.go", "startLine": 4,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "receivers.go", "startLine": 7}, "line": 8, "column": 6},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Right::run",
                expected: json!([
                    {"name": "Right::run", "filePath": "receivers.go", "startLine": 6,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "receivers.go", "startLine": 7}, "line": 9, "column": 6},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific import-binding regression. Import two same-named definitions through
// different aliases. Preserve their canonical definitions and source paths; aliases and import
// statements themselves are not calls. The same-named query returns two groups without exchanging
// callsites.
#[tokio::test]
async fn python_aliased_imports_of_same_name() {
    check_case(Case {
        name: "python_aliased_imports_of_same_name",
        files: vec![
            SourceFile {
                path: "left.py",
                source: r"def work():
    pass
",
            },
            SourceFile {
                path: "right.py",
                source: r"def work():
    pass
",
            },
            SourceFile {
                path: "caller.py",
                source: r"from left import work as left_work
from right import work as right_work
def caller():
    left_work()
    right_work()
",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([
                    {"name": "caller", "filePath": "caller.py", "startLine": 3,
                     "edges": [
                        {"symbol": {"name": "work", "filePath": "left.py", "startLine": 1}, "line": 4, "column": 4},
                        {"symbol": {"name": "work", "filePath": "right.py", "startLine": 1}, "line": 5, "column": 4},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "work",
                expected: json!([
                    {"name": "work", "filePath": "left.py", "startLine": 1,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "caller.py", "startLine": 3}, "line": 4, "column": 4},
                    ]},
                    {"name": "work", "filePath": "right.py", "startLine": 1,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "caller.py", "startLine": 3}, "line": 5, "column": 4},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific import-binding regression. Import two same-named definitions through
// different aliases. Preserve their canonical definitions and source paths; aliases and import
// statements themselves are not calls. The same-named query returns two groups without exchanging
// callsites.
#[tokio::test]
async fn typescript_aliased_imports_of_same_name() {
    check_case(Case {
        name: "typescript_aliased_imports_of_same_name",
        files: vec![
            SourceFile {
                path: "left.ts",
                source: r"export function work() {}
",
            },
            SourceFile {
                path: "right.ts",
                source: r"export function work() {}
",
            },
            SourceFile {
                path: "caller.ts",
                source: r"import { work as leftWork } from './left';
import { work as rightWork } from './right';
function caller() {
    leftWork();
    rightWork();
}
",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([
                    {"name": "caller", "filePath": "caller.ts", "startLine": 3,
                     "edges": [
                        {"symbol": {"name": "work", "filePath": "left.ts", "startLine": 1}, "line": 4, "column": 4},
                        {"symbol": {"name": "work", "filePath": "right.ts", "startLine": 1}, "line": 5, "column": 4},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "work",
                expected: json!([
                    {"name": "work", "filePath": "left.ts", "startLine": 1,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "caller.ts", "startLine": 3}, "line": 4, "column": 4},
                    ]},
                    {"name": "work", "filePath": "right.ts", "startLine": 1,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "caller.ts", "startLine": 3}, "line": 5, "column": 4},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific import-binding regression. Import two same-named definitions through
// different aliases. Preserve their canonical definitions and source paths; aliases and import
// statements themselves are not calls. The same-named query returns two groups without exchanging
// callsites.
#[tokio::test]
async fn rust_aliased_imports_of_same_name() {
    check_case(Case {
        name: "rust_aliased_imports_of_same_name",
        files: vec![
            SourceFile {
                path: "left.rs",
                source: r"pub fn work() {}
",
            },
            SourceFile {
                path: "right.rs",
                source: r"pub fn work() {}
",
            },
            SourceFile {
                path: "lib.rs",
                source: r"mod left;
mod right;
use left::work as left_work;
use right::work as right_work;
fn caller() {
    left_work();
    right_work();
}
",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([
                    {"name": "caller", "filePath": "lib.rs", "startLine": 5,
                     "edges": [
                        {"symbol": {"name": "left::work", "filePath": "left.rs", "startLine": 1}, "line": 6, "column": 4},
                        {"symbol": {"name": "right::work", "filePath": "right.rs", "startLine": 1}, "line": 7, "column": 4},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "work",
                expected: json!([
                    {"name": "left::work", "filePath": "left.rs", "startLine": 1,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "lib.rs", "startLine": 5}, "line": 6, "column": 4},
                    ]},
                    {"name": "right::work", "filePath": "right.rs", "startLine": 1,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "lib.rs", "startLine": 5}, "line": 7, "column": 4},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific import-binding regression. Import two same-named definitions through
// different aliases. Preserve their canonical definitions and source paths; aliases and import
// statements themselves are not calls. The same-named query returns two groups without exchanging
// callsites.
#[tokio::test]
async fn go_aliased_imports_of_same_name() {
    check_case(Case {
        name: "go_aliased_imports_of_same_name",
        files: vec![
            SourceFile {
                path: "go.mod",
                source: r"module example.com/calls
go 1.17
",
            },
            SourceFile {
                path: "left/left.go",
                source: r"package left

func Work() {}
",
            },
            SourceFile {
                path: "right/right.go",
                source: r"package right

func Work() {}
",
            },
            SourceFile {
                path: "caller.go",
                source: r#"package calls

import (
    l "example.com/calls/left"
    r "example.com/calls/right"
)

func caller() {
    l.Work()
    r.Work()
}
"#,
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([
                    {"name": "caller", "filePath": "caller.go", "startLine": 8,
                     "edges": [
                        {"symbol": {"name": "Work", "filePath": "left/left.go", "startLine": 3}, "line": 9, "column": 6},
                        {"symbol": {"name": "Work", "filePath": "right/right.go", "startLine": 3}, "line": 10, "column": 6},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Work",
                expected: json!([
                    {"name": "Work", "filePath": "left/left.go", "startLine": 3,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "caller.go", "startLine": 8}, "line": 9, "column": 6},
                    ]},
                    {"name": "Work", "filePath": "right/right.go", "startLine": 3,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "caller.go", "startLine": 8}, "line": 10, "column": 6},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific re-export regression. leaf.work is re-exported as bridge.run and imported as
// invoke. Both directions must connect caller to the original leaf definition; the export and
// import aliases must not create intermediate calls edges.
#[tokio::test]
async fn typescript_reexport_alias_reaches_original_definition() {
    check_case(Case {
        name: "typescript_reexport_alias_reaches_original_definition",
        files: vec![
            SourceFile {
                path: "leaf.ts",
                source: r"export function work() {}
",
            },
            SourceFile {
                path: "bridge.ts",
                source: r"export { work as run } from './leaf';
",
            },
            SourceFile {
                path: "caller.ts",
                source: r"import { run as invoke } from './bridge';
function caller() {
    invoke();
}
",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "work",
                expected: json!([
                    {"name": "work", "filePath": "leaf.ts", "startLine": 1,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "caller.ts", "startLine": 2}, "line": 3, "column": 4},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([
                    {"name": "caller", "filePath": "caller.ts", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "work", "filePath": "leaf.ts", "startLine": 1}, "line": 3, "column": 4},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific position regression. Two calls on one line follow Chinese characters and an
// emoji. Recorded columns are illustrative; this suite does not compare them.
// Expect one relationship even though the same target is called twice on this line.
#[tokio::test]
async fn javascript_unicode_prefix_and_repeated_same_line_calls() {
    check_case(Case {
        name: "javascript_unicode_prefix_and_repeated_same_line_calls",
        files: vec![SourceFile {
            path: "positions.js",
            source: r#"function target() {}
function caller() {
    const text = "中文😀"; target(); target();
}
"#,
        }],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "target",
                expected: json!([
                    {"name": "target", "filePath": "positions.js", "startLine": 1,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "positions.js", "startLine": 2}, "line": 3, "column": 31},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([
                    {"name": "caller", "filePath": "positions.js", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "target", "filePath": "positions.js", "startLine": 1}, "line": 3, "column": 31},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific position regression. Two calls on one line follow Chinese characters and an
// emoji. Recorded columns are illustrative; this suite does not compare them.
// Expect one relationship even though the same target is called twice on this line.
#[tokio::test]
async fn c_unicode_prefix_and_repeated_same_line_calls() {
    check_case(Case {
        name: "c_unicode_prefix_and_repeated_same_line_calls",
        files: vec![SourceFile {
            path: "positions.c",
            source: r"void target(void) {}
void caller(void) {
    /* 中文😀 */ target(); target();
}
",
        }],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "target",
                expected: json!([
                    {"name": "target", "filePath": "positions.c", "startLine": 1,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "positions.c", "startLine": 2}, "line": 3, "column": 21},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([
                    {"name": "caller", "filePath": "positions.c", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "target", "filePath": "positions.c", "startLine": 1}, "line": 3, "column": 21},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific incremental regression. Editing target content/range changes
// its canonical identity without deleting the file or changing the caller source.
#[tokio::test]
async fn incremental_target_edit_rebinds_unchanged_caller() {
    let workspace = IncrementalWorkspace::new().await;
    workspace.check_initial_calls("before target edit").await;
    fs::write(
        workspace.directory.path().join("targets.py"),
        "\n\ndef first():\n    return 42\n\ndef second():\n    pass\n",
    )
    .expect("edit target definition");
    let indexed = workspace
        .index(vec![WorkspaceChange::Upsert("targets.py".into())])
        .await;
    assert_eq!(indexed.files_modified, 1);
    assert_eq!(indexed.files_added + indexed.files_deleted, 0);

    workspace
        .check(
            "unchanged caller must point to the new target identity",
            &[
                Query {
                    operation: "callees",
                    symbol: "caller",
                    expected: json!([
                        {"name": "caller", "filePath": "caller.py", "startLine": 2,
                         "edges": [
                            {"symbol": {"name": "first", "filePath": "targets.py", "startLine": 3}, "line": 3, "column": 4},
                        ]},
                    ]),
                },
                Query {
                    operation: "callers",
                    symbol: "first",
                    expected: json!([
                        {"name": "first", "filePath": "targets.py", "startLine": 3,
                         "edges": [
                            {"symbol": {"name": "caller", "filePath": "caller.py", "startLine": 2}, "line": 3, "column": 4},
                        ]},
                    ]),
                },
            ],
        )
        .await;
}

// Moving a target must first invalidate the explicitly imported old path, even
// though a same-named function still exists elsewhere. Fixing the import then
// reconnects the caller to the moved definition, with its new path and identity.
#[tokio::test]
async fn incremental_target_move_then_import_edit_updates_path() {
    let workspace = IncrementalWorkspace::new().await;
    workspace.check_initial_calls("before target move").await;
    let root = workspace.directory.path();
    fs::rename(root.join("targets.py"), root.join("moved.py")).expect("move target");
    let moved = workspace
        .index(vec![
            WorkspaceChange::Delete("targets.py".into()),
            WorkspaceChange::Upsert("moved.py".into()),
        ])
        .await;
    assert_eq!((moved.files_added, moved.files_deleted), (1, 1));
    assert_eq!(moved.files_modified, 0);

    workspace
        .check(
            "old explicit import cannot resolve by unrelated workspace fallback",
            &[
                Query {
                    operation: "callees",
                    symbol: "caller",
                    expected: json!([
                        {"name": "caller", "filePath": "caller.py", "startLine": 2,
                         "edges": []},
                    ]),
                },
                Query {
                    operation: "callers",
                    symbol: "first",
                    expected: json!([
                        {"name": "first", "filePath": "moved.py", "startLine": 1,
                         "edges": []},
                    ]),
                },
            ],
        )
        .await;
    fs::write(
        root.join("caller.py"),
        "from moved import first, second\ndef caller():\n    first()\n",
    )
    .expect("update import");
    let updated = workspace
        .index(vec![WorkspaceChange::Upsert("caller.py".into())])
        .await;
    assert_eq!(updated.files_modified, 1);
    workspace
        .check(
            "updated import uses the moved definition and path",
            &[
                Query {
                    operation: "callees",
                    symbol: "caller",
                    expected: json!([
                        {"name": "caller", "filePath": "caller.py", "startLine": 2,
                         "edges": [
                            {"symbol": {"name": "first", "filePath": "moved.py", "startLine": 1}, "line": 3, "column": 4},
                        ]},
                    ]),
                },
                Query {
                    operation: "callers",
                    symbol: "first",
                    expected: json!([
                        {"name": "first", "filePath": "moved.py", "startLine": 1,
                         "edges": [
                            {"symbol": {"name": "caller", "filePath": "caller.py", "startLine": 2}, "line": 3, "column": 4},
                        ]},
                    ]),
                },
            ],
        )
        .await;
}

// Conservative workspace-unique fallback contract for global JavaScript scripts.
// Adding a second concrete same-named implementation removes uniqueness: leave
// the reference unresolved instead of retaining the previous arbitrary winner.
// This differs from the TypeScript overload-declaration case, whose upstream
// contract deliberately returns multiple canonical declaration candidates.
#[tokio::test]
async fn incremental_new_candidate_invalidates_unique_resolution() {
    let workspace = IncrementalWorkspace::from_sources(
        &[
            SourceFile {
                path: "one.js",
                source: "function work() {}\n",
            },
            SourceFile {
                path: "caller.js",
                source: "function caller() {\n    work();\n}\n",
            },
        ],
        "*.js",
    )
    .await;

    let unique = [
        Query {
            operation: "callees",
            symbol: "caller",
            expected: json!([
                {"name": "caller", "filePath": "caller.js", "startLine": 1,
                 "edges": [
                    {"symbol": {"name": "work", "filePath": "one.js", "startLine": 1}, "line": 2, "column": 4},
                ]},
            ]),
        },
        Query {
            operation: "callers",
            symbol: "work",
            expected: json!([
                {"name": "work", "filePath": "one.js", "startLine": 1,
                 "edges": [
                    {"symbol": {"name": "caller", "filePath": "caller.js", "startLine": 1}, "line": 2, "column": 4},
                ]},
            ]),
        },
    ];
    workspace
        .check("initial unique workspace target", &unique)
        .await;
    fs::write(
        workspace.directory.path().join("two.js"),
        "function work() {}\n",
    )
    .expect("new candidate");
    let added = workspace
        .index(vec![WorkspaceChange::Upsert("two.js".into())])
        .await;
    assert_eq!(added.files_added, 1);
    assert_eq!(added.files_modified, 0);
    workspace
        .check(
            "new candidate invalidates the old unique resolution",
            &[
                Query {
                    operation: "callees",
                    symbol: "caller",
                    expected: json!([
                        {"name": "caller", "filePath": "caller.js", "startLine": 1,
                         "edges": []},
                    ]),
                },
                Query {
                    operation: "callers",
                    symbol: "work",
                    expected: json!([
                        {"name": "work", "filePath": "one.js", "startLine": 1,
                         "edges": []},
                        {"name": "work", "filePath": "two.js", "startLine": 1,
                         "edges": []},
                    ]),
                },
            ],
        )
        .await;
    fs::remove_file(workspace.directory.path().join("two.js")).expect("remove ambiguity");
    let deleted = workspace
        .index(vec![WorkspaceChange::Delete("two.js".into())])
        .await;
    assert_eq!(deleted.files_deleted, 1);
    workspace
        .check("removing the candidate restores unique resolution", &unique)
        .await;
}

// zvec-grep-specific C++ auto receiver regression.
// Infer auto x = new X() as X*, then resolve x->foo() to X::foo. The explicitly declared X
// constructor is a separate direct outgoing call. Y::foo must have no callers; matching only the
// foo name is insufficient.
#[tokio::test]
async fn cpp_auto_new_pointer_receiver() {
    check_case(Case {
        name: "cpp_auto_new_pointer_receiver",
        files: vec![SourceFile {
            path: "auto.cc",
            source: r"struct X {
    X() {}
    void foo() {}
};
struct Y {
    void foo() {}
};
void caller() {
    auto x = new X();
    x->foo();
}
",
        }],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "foo",
                expected: json!([
                    {"name": "X::foo", "filePath": "auto.cc", "startLine": 3,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "auto.cc", "startLine": 8}, "line": 10, "column": 7},
                    ]},
                    {"name": "Y::foo", "filePath": "auto.cc", "startLine": 6,
                     "edges": []},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([
                    {"name": "caller", "filePath": "auto.cc", "startLine": 8,
                     "edges": [
                        {"symbol": {"name": "X::X", "filePath": "auto.cc", "startLine": 2}, "line": 9, "column": 17},
                        {"symbol": {"name": "X::foo", "filePath": "auto.cc", "startLine": 3}, "line": 10, "column": 7},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Y::foo",
                expected: json!([
                    {"name": "Y::foo", "filePath": "auto.cc", "startLine": 6,
                     "edges": []},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific C++ auto receiver regression.
// Infer auto x = X{} as an X value and resolve x.foo(). Preserve the explicit constructor call at
// X{}, not at the variable name. Y::foo is an unrelated same-named method.
#[tokio::test]
async fn cpp_auto_value_receiver() {
    check_case(Case {
        name: "cpp_auto_value_receiver",
        files: vec![SourceFile {
            path: "auto.cc",
            source: r"struct X {
    X() {}
    void foo() {}
};
struct Y {
    void foo() {}
};
void caller() {
    auto x = X{};
    x.foo();
}
",
        }],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "foo",
                expected: json!([
                    {"name": "X::foo", "filePath": "auto.cc", "startLine": 3,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "auto.cc", "startLine": 8}, "line": 10, "column": 6},
                    ]},
                    {"name": "Y::foo", "filePath": "auto.cc", "startLine": 6,
                     "edges": []},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([
                    {"name": "caller", "filePath": "auto.cc", "startLine": 8,
                     "edges": [
                        {"symbol": {"name": "X::X", "filePath": "auto.cc", "startLine": 2}, "line": 9, "column": 13},
                        {"symbol": {"name": "X::foo", "filePath": "auto.cc", "startLine": 3}, "line": 10, "column": 6},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Y::foo",
                expected: json!([
                    {"name": "Y::foo", "filePath": "auto.cc", "startLine": 6,
                     "edges": []},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific C++ auto receiver regression.
// Infer x from the declared X return type of makeX(), then resolve x.foo(). caller directly calls
// makeX and X::foo; construction inside makeX belongs only to makeX, not caller. The empty Y::foo
// group guards against name-only binding.
#[tokio::test]
async fn cpp_auto_factory_return_receiver() {
    check_case(Case {
        name: "cpp_auto_factory_return_receiver",
        files: vec![SourceFile {
            path: "auto.cc",
            source: r"struct X {
    X() {}
    void foo() {}
};
struct Y {
    void foo() {}
};
X makeX() {
    return X{};
}

void caller() {
    auto x = makeX();
    x.foo();
}
",
        }],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "foo",
                expected: json!([
                    {"name": "X::foo", "filePath": "auto.cc", "startLine": 3,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "auto.cc", "startLine": 12}, "line": 14, "column": 6},
                    ]},
                    {"name": "Y::foo", "filePath": "auto.cc", "startLine": 6,
                     "edges": []},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([
                    {"name": "caller", "filePath": "auto.cc", "startLine": 12,
                     "edges": [
                        {"symbol": {"name": "makeX", "filePath": "auto.cc", "startLine": 8}, "line": 13, "column": 13},
                        {"symbol": {"name": "X::foo", "filePath": "auto.cc", "startLine": 3}, "line": 14, "column": 6},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "makeX",
                expected: json!([
                    {"name": "makeX", "filePath": "auto.cc", "startLine": 8,
                     "edges": [
                        {"symbol": {"name": "X::X", "filePath": "auto.cc", "startLine": 2}, "line": 9, "column": 11},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Y::foo",
                expected: json!([
                    {"name": "Y::foo", "filePath": "auto.cc", "startLine": 6,
                     "edges": []},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific C++ auto receiver regression.
// Infer auto& x = existing as X& from the X& parameter. The reference binding is not a call and
// performs no construction. x.foo() must resolve to X::foo rather than Y::foo.
#[tokio::test]
async fn cpp_auto_reference_alias_receiver() {
    check_case(Case {
        name: "cpp_auto_reference_alias_receiver",
        files: vec![SourceFile {
            path: "auto.cc",
            source: r"struct X {
    X() {}
    void foo() {}
};
struct Y {
    void foo() {}
};
void caller(X& existing) {
    auto& x = existing;
    x.foo();
}
",
        }],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "foo",
                expected: json!([
                    {"name": "X::foo", "filePath": "auto.cc", "startLine": 3,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "auto.cc", "startLine": 8}, "line": 10, "column": 6},
                    ]},
                    {"name": "Y::foo", "filePath": "auto.cc", "startLine": 6,
                     "edges": []},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([
                    {"name": "caller", "filePath": "auto.cc", "startLine": 8,
                     "edges": [
                        {"symbol": {"name": "X::foo", "filePath": "auto.cc", "startLine": 3}, "line": 10, "column": 6},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Y::foo",
                expected: json!([
                    {"name": "Y::foo", "filePath": "auto.cc", "startLine": 6,
                     "edges": []},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific inferred receiver/callback regression.
// Java var uses the initializer type, the declared X factory return type, or a copied object
// reference. The explicit X constructor is a direct call only where new X() occurs. Copying x to y
// is not a call. Y::foo is deliberately present as a same-named decoy. Query only direct calls:
// construction inside the factory must not leak into caller.
#[tokio::test]
async fn java_inferred_creation_receiver() {
    check_case(Case {
        name: "java_inferred_creation_receiver",
        files: vec![SourceFile {
            path: "Inferred.java",
            source: r"class X {
    X() {}
    void foo() {}
}
class Y {
    void foo() {}
}
class Use {
    static X makeX() {
        return new X();
    }
    static void caller() {
        var x = new X();
        x.foo();
    }
}
",
        }],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "foo",
                expected: json!([
                    {"name": "X::foo", "filePath": "Inferred.java", "startLine": 3,
                     "edges": [
                        {"symbol": {"name": "Use::caller", "filePath": "Inferred.java", "startLine": 12}, "line": 14, "column": 10},
                    ]},
                    {"name": "Y::foo", "filePath": "Inferred.java", "startLine": 6,
                     "edges": []},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "Use::caller",
                expected: json!([
                    {"name": "Use::caller", "filePath": "Inferred.java", "startLine": 12,
                     "edges": [
                        {"symbol": {"name": "X::X", "filePath": "Inferred.java", "startLine": 2}, "line": 13, "column": 20},
                        {"symbol": {"name": "X::foo", "filePath": "Inferred.java", "startLine": 3}, "line": 14, "column": 10},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Y::foo",
                expected: json!([
                    {"name": "Y::foo", "filePath": "Inferred.java", "startLine": 6,
                     "edges": []},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific inferred receiver/callback regression.
// Java var uses the initializer type, the declared X factory return type, or a copied object
// reference. The explicit X constructor is a direct call only where new X() occurs. Copying x to y
// is not a call. Y::foo is deliberately present as a same-named decoy. Query only direct calls:
// construction inside the factory must not leak into caller.
#[tokio::test]
async fn java_inferred_factory_return_receiver() {
    check_case(Case {
        name: "java_inferred_factory_return_receiver",
        files: vec![SourceFile {
            path: "Inferred.java",
            source: r"class X {
    X() {}
    void foo() {}
}
class Y {
    void foo() {}
}
class Use {
    static X makeX() {
        return new X();
    }
    static void caller() {
        var x = makeX();
        x.foo();
    }
}
",
        }],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "foo",
                expected: json!([
                    {"name": "X::foo", "filePath": "Inferred.java", "startLine": 3,
                     "edges": [
                        {"symbol": {"name": "Use::caller", "filePath": "Inferred.java", "startLine": 12}, "line": 14, "column": 10},
                    ]},
                    {"name": "Y::foo", "filePath": "Inferred.java", "startLine": 6,
                     "edges": []},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "Use::caller",
                expected: json!([
                    {"name": "Use::caller", "filePath": "Inferred.java", "startLine": 12,
                     "edges": [
                        {"symbol": {"name": "Use::makeX", "filePath": "Inferred.java", "startLine": 9}, "line": 13, "column": 16},
                        {"symbol": {"name": "X::foo", "filePath": "Inferred.java", "startLine": 3}, "line": 14, "column": 10},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Y::foo",
                expected: json!([
                    {"name": "Y::foo", "filePath": "Inferred.java", "startLine": 6,
                     "edges": []},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "Use::makeX",
                expected: json!([
                    {"name": "Use::makeX", "filePath": "Inferred.java", "startLine": 9,
                     "edges": [
                        {"symbol": {"name": "X::X", "filePath": "Inferred.java", "startLine": 2}, "line": 10, "column": 19},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific inferred receiver/callback regression.
// JavaScript infers the receiver from new X(), factory return expressions and reference aliases.
// As in the existing implicit-constructor contract, new X() targets the X class definition;
// copying x to y creates no call. Y::foo is deliberately present as a same-named decoy. Query only
// direct calls: construction inside the factory must not leak into caller.
#[tokio::test]
async fn javascript_inferred_creation_receiver() {
    check_case(Case {
        name: "javascript_inferred_creation_receiver",
        files: vec![SourceFile {
            path: "inferred.js",
            source: r"class X {
    foo() {}
}
class Y {
    foo() {}
}
function makeX() {
    return new X();
}
function caller() {
    const x = new X();
    x.foo();
}
",
        }],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "foo",
                expected: json!([
                    {"name": "X::foo", "filePath": "inferred.js", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "inferred.js", "startLine": 10}, "line": 12, "column": 6},
                    ]},
                    {"name": "Y::foo", "filePath": "inferred.js", "startLine": 5,
                     "edges": []},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([
                    {"name": "caller", "filePath": "inferred.js", "startLine": 10,
                     "edges": [
                        {"symbol": {"name": "X", "filePath": "inferred.js", "startLine": 1}, "line": 11, "column": 18},
                        {"symbol": {"name": "X::foo", "filePath": "inferred.js", "startLine": 2}, "line": 12, "column": 6},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Y::foo",
                expected: json!([
                    {"name": "Y::foo", "filePath": "inferred.js", "startLine": 5,
                     "edges": []},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific inferred receiver/callback regression.
// JavaScript infers the receiver from new X(), factory return expressions and reference aliases.
// As in the existing implicit-constructor contract, new X() targets the X class definition;
// copying x to y creates no call. Y::foo is deliberately present as a same-named decoy. Query only
// direct calls: construction inside the factory must not leak into caller.
#[tokio::test]
async fn javascript_inferred_factory_return_receiver() {
    check_case(Case {
        name: "javascript_inferred_factory_return_receiver",
        files: vec![SourceFile {
            path: "inferred.js",
            source: r"class X {
    foo() {}
}
class Y {
    foo() {}
}
function makeX() {
    return new X();
}
function caller() {
    const x = makeX();
    x.foo();
}
",
        }],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "foo",
                expected: json!([
                    {"name": "X::foo", "filePath": "inferred.js", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "inferred.js", "startLine": 10}, "line": 12, "column": 6},
                    ]},
                    {"name": "Y::foo", "filePath": "inferred.js", "startLine": 5,
                     "edges": []},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([
                    {"name": "caller", "filePath": "inferred.js", "startLine": 10,
                     "edges": [
                        {"symbol": {"name": "makeX", "filePath": "inferred.js", "startLine": 7}, "line": 11, "column": 14},
                        {"symbol": {"name": "X::foo", "filePath": "inferred.js", "startLine": 2}, "line": 12, "column": 6},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Y::foo",
                expected: json!([
                    {"name": "Y::foo", "filePath": "inferred.js", "startLine": 5,
                     "edges": []},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "makeX",
                expected: json!([
                    {"name": "makeX", "filePath": "inferred.js", "startLine": 7,
                     "edges": [
                        {"symbol": {"name": "X", "filePath": "inferred.js", "startLine": 1}, "line": 8, "column": 15},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific inferred receiver/callback regression.
// TypeScript infers the receiver from new X(), the declared factory return type and reference
// aliases. As in the existing implicit-constructor contract, new X() targets the X class
// definition; copying x to y creates no call. Y::foo is deliberately present as a same-named
// decoy. Query only direct calls: construction inside the factory must not leak into caller.
#[tokio::test]
async fn typescript_inferred_creation_receiver() {
    check_case(Case {
        name: "typescript_inferred_creation_receiver",
        files: vec![SourceFile {
            path: "inferred.ts",
            source: r"class X {
    foo() {}
}
class Y {
    foo() {}
}
function makeX(): X {
    return new X();
}
function caller() {
    const x = new X();
    x.foo();
}
",
        }],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "foo",
                expected: json!([
                    {"name": "X::foo", "filePath": "inferred.ts", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "inferred.ts", "startLine": 10}, "line": 12, "column": 6},
                    ]},
                    {"name": "Y::foo", "filePath": "inferred.ts", "startLine": 5,
                     "edges": []},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([
                    {"name": "caller", "filePath": "inferred.ts", "startLine": 10,
                     "edges": [
                        {"symbol": {"name": "X", "filePath": "inferred.ts", "startLine": 1}, "line": 11, "column": 18},
                        {"symbol": {"name": "X::foo", "filePath": "inferred.ts", "startLine": 2}, "line": 12, "column": 6},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Y::foo",
                expected: json!([
                    {"name": "Y::foo", "filePath": "inferred.ts", "startLine": 5,
                     "edges": []},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific inferred receiver/callback regression.
// TypeScript infers the receiver from new X(), the declared factory return type and reference
// aliases. As in the existing implicit-constructor contract, new X() targets the X class
// definition; copying x to y creates no call. Y::foo is deliberately present as a same-named
// decoy. Query only direct calls: construction inside the factory must not leak into caller.
#[tokio::test]
async fn typescript_inferred_factory_return_receiver() {
    check_case(Case {
        name: "typescript_inferred_factory_return_receiver",
        files: vec![SourceFile {
            path: "inferred.ts",
            source: r"class X {
    foo() {}
}
class Y {
    foo() {}
}
function makeX(): X {
    return new X();
}
function caller() {
    const x = makeX();
    x.foo();
}
",
        }],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "foo",
                expected: json!([
                    {"name": "X::foo", "filePath": "inferred.ts", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "inferred.ts", "startLine": 10}, "line": 12, "column": 6},
                    ]},
                    {"name": "Y::foo", "filePath": "inferred.ts", "startLine": 5,
                     "edges": []},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([
                    {"name": "caller", "filePath": "inferred.ts", "startLine": 10,
                     "edges": [
                        {"symbol": {"name": "makeX", "filePath": "inferred.ts", "startLine": 7}, "line": 11, "column": 14},
                        {"symbol": {"name": "X::foo", "filePath": "inferred.ts", "startLine": 2}, "line": 12, "column": 6},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Y::foo",
                expected: json!([
                    {"name": "Y::foo", "filePath": "inferred.ts", "startLine": 5,
                     "edges": []},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "makeX",
                expected: json!([
                    {"name": "makeX", "filePath": "inferred.ts", "startLine": 7,
                     "edges": [
                        {"symbol": {"name": "X", "filePath": "inferred.ts", "startLine": 1}, "line": 8, "column": 15},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific inferred receiver/callback regression.
// Python infers the receiver from X(), the unannotated factory return expression and a local
// variable alias. X has no explicit __init__; construction targets the class definition rather
// than fabricating an initializer entity. Bare assignment is not a call. Y::foo is deliberately
// present as a same-named decoy. Query only direct calls: construction inside the factory must not
// leak into caller.
#[tokio::test]
async fn python_inferred_creation_receiver() {
    check_case(Case {
        name: "python_inferred_creation_receiver",
        files: vec![SourceFile {
            path: "inferred.py",
            source: r"class X:
    def foo(self):
        pass
class Y:
    def foo(self):
        pass

def make_x():
    return X()

def caller():
    x = X()
    x.foo()
",
        }],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "foo",
                expected: json!([
                    {"name": "X::foo", "filePath": "inferred.py", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "inferred.py", "startLine": 11}, "line": 13, "column": 6},
                    ]},
                    {"name": "Y::foo", "filePath": "inferred.py", "startLine": 5,
                     "edges": []},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([
                    {"name": "caller", "filePath": "inferred.py", "startLine": 11,
                     "edges": [
                        {"symbol": {"name": "X", "filePath": "inferred.py", "startLine": 1}, "line": 12, "column": 8},
                        {"symbol": {"name": "X::foo", "filePath": "inferred.py", "startLine": 2}, "line": 13, "column": 6},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Y::foo",
                expected: json!([
                    {"name": "Y::foo", "filePath": "inferred.py", "startLine": 5,
                     "edges": []},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific inferred receiver/callback regression.
// Python infers the receiver from X(), the unannotated factory return expression and a local
// variable alias. X has no explicit __init__; construction targets the class definition rather
// than fabricating an initializer entity. Bare assignment is not a call. Y::foo is deliberately
// present as a same-named decoy. Query only direct calls: construction inside the factory must not
// leak into caller.
#[tokio::test]
async fn python_inferred_factory_return_receiver() {
    check_case(Case {
        name: "python_inferred_factory_return_receiver",
        files: vec![SourceFile {
            path: "inferred.py",
            source: r"class X:
    def foo(self):
        pass
class Y:
    def foo(self):
        pass

def make_x():
    return X()

def caller():
    x = make_x()
    x.foo()
",
        }],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "foo",
                expected: json!([
                    {"name": "X::foo", "filePath": "inferred.py", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "inferred.py", "startLine": 11}, "line": 13, "column": 6},
                    ]},
                    {"name": "Y::foo", "filePath": "inferred.py", "startLine": 5,
                     "edges": []},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([
                    {"name": "caller", "filePath": "inferred.py", "startLine": 11,
                     "edges": [
                        {"symbol": {"name": "make_x", "filePath": "inferred.py", "startLine": 8}, "line": 12, "column": 8},
                        {"symbol": {"name": "X::foo", "filePath": "inferred.py", "startLine": 2}, "line": 13, "column": 6},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Y::foo",
                expected: json!([
                    {"name": "Y::foo", "filePath": "inferred.py", "startLine": 5,
                     "edges": []},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "make_x",
                expected: json!([
                    {"name": "make_x", "filePath": "inferred.py", "startLine": 8,
                     "edges": [
                        {"symbol": {"name": "X", "filePath": "inferred.py", "startLine": 1}, "line": 9, "column": 11},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific inferred receiver/callback regression.
// Go short declarations infer *X from new(X) or the factory signature; y := x copies that pointer.
// Builtin new has no indexed function target and is not a fabricated constructor call. Pointer-
// receiver foo must bind to X, not Y. Y::foo is deliberately present as a same-named decoy. Query
// only direct calls: construction inside the factory must not leak into caller.
#[tokio::test]
async fn go_inferred_creation_receiver() {
    check_case(Case {
        name: "go_inferred_creation_receiver",
        files: vec![SourceFile {
            path: "inferred.go",
            source: r"package inferred

type X struct{}
func (x *X) foo() {}
type Y struct{}
func (y *Y) foo() {}
func makeX() *X {
    return new(X)
}
func caller() {
    x := new(X)
    x.foo()
}
",
        }],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "foo",
                expected: json!([
                    {"name": "X::foo", "filePath": "inferred.go", "startLine": 4,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "inferred.go", "startLine": 10}, "line": 12, "column": 6},
                    ]},
                    {"name": "Y::foo", "filePath": "inferred.go", "startLine": 6,
                     "edges": []},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([
                    {"name": "caller", "filePath": "inferred.go", "startLine": 10,
                     "edges": [
                        {"symbol": {"name": "X::foo", "filePath": "inferred.go", "startLine": 4}, "line": 12, "column": 6},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Y::foo",
                expected: json!([
                    {"name": "Y::foo", "filePath": "inferred.go", "startLine": 6,
                     "edges": []},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific inferred receiver/callback regression.
// Go short declarations infer *X from new(X) or the factory signature; y := x copies that pointer.
// Builtin new has no indexed function target and is not a fabricated constructor call. Pointer-
// receiver foo must bind to X, not Y. Y::foo is deliberately present as a same-named decoy. Query
// only direct calls: construction inside the factory must not leak into caller.
#[tokio::test]
async fn go_inferred_factory_return_receiver() {
    check_case(Case {
        name: "go_inferred_factory_return_receiver",
        files: vec![SourceFile {
            path: "inferred.go",
            source: r"package inferred

type X struct{}
func (x *X) foo() {}
type Y struct{}
func (y *Y) foo() {}
func makeX() *X {
    return new(X)
}
func caller() {
    x := makeX()
    x.foo()
}
",
        }],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "foo",
                expected: json!([
                    {"name": "X::foo", "filePath": "inferred.go", "startLine": 4,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "inferred.go", "startLine": 10}, "line": 12, "column": 6},
                    ]},
                    {"name": "Y::foo", "filePath": "inferred.go", "startLine": 6,
                     "edges": []},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([
                    {"name": "caller", "filePath": "inferred.go", "startLine": 10,
                     "edges": [
                        {"symbol": {"name": "makeX", "filePath": "inferred.go", "startLine": 7}, "line": 11, "column": 9},
                        {"symbol": {"name": "X::foo", "filePath": "inferred.go", "startLine": 4}, "line": 12, "column": 6},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Y::foo",
                expected: json!([
                    {"name": "Y::foo", "filePath": "inferred.go", "startLine": 6,
                     "edges": []},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "makeX",
                expected: json!([
                    {"name": "makeX", "filePath": "inferred.go", "startLine": 7,
                     "edges": []},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific inferred receiver/callback regression.
// Rust let bindings infer X from a struct expression or the factory signature. let y = x moves the
// value without creating a call; y.foo() still binds to X. The X {} struct expression is not a
// constructor function call. Y::foo is deliberately present as a same-named decoy. Query only
// direct calls: construction inside the factory must not leak into caller.
#[tokio::test]
async fn rust_inferred_creation_receiver() {
    check_case(Case {
        name: "rust_inferred_creation_receiver",
        files: vec![SourceFile {
            path: "inferred.rs",
            source: r"struct X {}
impl X {
    fn foo(&self) {}
}
struct Y {}
impl Y {
    fn foo(&self) {}
}
fn make_x() -> X {
    X {}
}
fn caller() {
    let x = X {};
    x.foo();
}
",
        }],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "foo",
                expected: json!([
                    {"name": "X::foo", "filePath": "inferred.rs", "startLine": 3,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "inferred.rs", "startLine": 12}, "line": 14, "column": 6},
                    ]},
                    {"name": "Y::foo", "filePath": "inferred.rs", "startLine": 7,
                     "edges": []},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([
                    {"name": "caller", "filePath": "inferred.rs", "startLine": 12,
                     "edges": [
                        {"symbol": {"name": "X::foo", "filePath": "inferred.rs", "startLine": 3}, "line": 14, "column": 6},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Y::foo",
                expected: json!([
                    {"name": "Y::foo", "filePath": "inferred.rs", "startLine": 7,
                     "edges": []},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific inferred receiver/callback regression.
// Rust let bindings infer X from a struct expression or the factory signature. let y = x moves the
// value without creating a call; y.foo() still binds to X. The X {} struct expression is not a
// constructor function call. Y::foo is deliberately present as a same-named decoy. Query only
// direct calls: construction inside the factory must not leak into caller.
#[tokio::test]
async fn rust_inferred_factory_return_receiver() {
    check_case(Case {
        name: "rust_inferred_factory_return_receiver",
        files: vec![SourceFile {
            path: "inferred.rs",
            source: r"struct X {}
impl X {
    fn foo(&self) {}
}
struct Y {}
impl Y {
    fn foo(&self) {}
}
fn make_x() -> X {
    X {}
}
fn caller() {
    let x = make_x();
    x.foo();
}
",
        }],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "foo",
                expected: json!([
                    {"name": "X::foo", "filePath": "inferred.rs", "startLine": 3,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "inferred.rs", "startLine": 12}, "line": 14, "column": 6},
                    ]},
                    {"name": "Y::foo", "filePath": "inferred.rs", "startLine": 7,
                     "edges": []},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([
                    {"name": "caller", "filePath": "inferred.rs", "startLine": 12,
                     "edges": [
                        {"symbol": {"name": "make_x", "filePath": "inferred.rs", "startLine": 9}, "line": 13, "column": 12},
                        {"symbol": {"name": "X::foo", "filePath": "inferred.rs", "startLine": 3}, "line": 14, "column": 6},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Y::foo",
                expected: json!([
                    {"name": "Y::foo", "filePath": "inferred.rs", "startLine": 7,
                     "edges": []},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "make_x",
                expected: json!([
                    {"name": "make_x", "filePath": "inferred.rs", "startLine": 9,
                     "edges": []},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific inferred receiver/callback regression.
// C callback regression: makeX and makeY return different same-named static targets from x.c and
// y.c. The factory returns a function pointer value, which x() invokes. Returning the function
// name is not a call.
#[tokio::test]
async fn c_inferred_factory_function_pointer() {
    check_case(Case {
        name: "c_inferred_factory_function_pointer",
        files: vec![
            SourceFile {
                path: "callback.h",
                source: r"typedef void (*Callback)(void);
Callback makeX(void);
Callback makeY(void);
",
            },
            SourceFile {
                path: "x.c",
                source: r#"#include "callback.h"
static void foo(void) {}
Callback makeX(void) {
    return foo;
}
"#,
            },
            SourceFile {
                path: "y.c",
                source: r#"#include "callback.h"
static void foo(void) {}
Callback makeY(void) {
    return foo;
}
"#,
            },
            SourceFile {
                path: "caller.c",
                source: r#"#include "callback.h"
void caller(void) {
    Callback x = makeX();
    x();
}
"#,
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "foo",
                expected: json!([
                    {"name": "foo", "filePath": "x.c", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "caller.c", "startLine": 2}, "line": 4, "column": 4},
                    ]},
                    {"name": "foo", "filePath": "y.c", "startLine": 2,
                     "edges": []},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([
                    {"name": "caller", "filePath": "caller.c", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "makeX", "filePath": "x.c", "startLine": 3}, "line": 3, "column": 17},
                        {"symbol": {"name": "foo", "filePath": "x.c", "startLine": 2}, "line": 4, "column": 4},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "makeX",
                expected: json!([
                    {"name": "makeX", "filePath": "x.c", "startLine": 3,
                     "edges": []},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific inferred receiver/callback regression.
// C callback regression: makeX and makeY return different same-named static targets from x.c and
// y.c. Copying a known function pointer to y preserves its target; assignment itself is not a
// call.
#[tokio::test]
async fn c_inferred_function_pointer_alias() {
    check_case(Case {
        name: "c_inferred_function_pointer_alias",
        files: vec![
            SourceFile {
                path: "callback.h",
                source: r"typedef void (*Callback)(void);
Callback makeX(void);
Callback makeY(void);
",
            },
            SourceFile {
                path: "x.c",
                source: r#"#include "callback.h"
static void foo(void) {}
Callback makeX(void) {
    return foo;
}
"#,
            },
            SourceFile {
                path: "y.c",
                source: r#"#include "callback.h"
static void foo(void) {}
Callback makeY(void) {
    return foo;
}
"#,
            },
            SourceFile {
                path: "caller.c",
                source: r#"#include "callback.h"
void caller(void) {
    Callback x = makeX();
    Callback y = x;
    y();
}
"#,
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "foo",
                expected: json!([
                    {"name": "foo", "filePath": "x.c", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "caller.c", "startLine": 2}, "line": 5, "column": 4},
                    ]},
                    {"name": "foo", "filePath": "y.c", "startLine": 2,
                     "edges": []},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([
                    {"name": "caller", "filePath": "caller.c", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "makeX", "filePath": "x.c", "startLine": 3}, "line": 3, "column": 17},
                        {"symbol": {"name": "foo", "filePath": "x.c", "startLine": 2}, "line": 5, "column": 4},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "makeX",
                expected: json!([
                    {"name": "makeX", "filePath": "x.c", "startLine": 3,
                     "edges": []},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific inferred receiver/callback regression.
// C callback regression: makeX and makeY return different same-named static targets from x.c and
// y.c. Follow a returned struct value to its foo function-pointer field; the field is not itself
// the called function. Both X and Y have foo fields.
#[tokio::test]
async fn c_inferred_struct_callback_member() {
    check_case(Case {
        name: "c_inferred_struct_callback_member",
        files: vec![
            SourceFile {
                path: "callback.h",
                source: r"typedef void (*Callback)(void);
struct X { Callback foo; };
struct Y { Callback foo; };
struct X makeX(void);
struct Y makeY(void);
",
            },
            SourceFile {
                path: "x.c",
                source: r#"#include "callback.h"
static void run(void) {}
struct X makeX(void) {
    return (struct X){ .foo = run };
}
"#,
            },
            SourceFile {
                path: "y.c",
                source: r#"#include "callback.h"
static void run(void) {}
struct Y makeY(void) {
    return (struct Y){ .foo = run };
}
"#,
            },
            SourceFile {
                path: "caller.c",
                source: r#"#include "callback.h"
void caller(void) {
    struct X x = makeX();
    x.foo();
}
"#,
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "run",
                expected: json!([
                    {"name": "run", "filePath": "x.c", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "caller.c", "startLine": 2}, "line": 4, "column": 6},
                    ]},
                    {"name": "run", "filePath": "y.c", "startLine": 2,
                     "edges": []},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([
                    {"name": "caller", "filePath": "caller.c", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "makeX", "filePath": "x.c", "startLine": 3}, "line": 3, "column": 17},
                        {"symbol": {"name": "run", "filePath": "x.c", "startLine": 2}, "line": 4, "column": 6},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "makeX",
                expected: json!([
                    {"name": "makeX", "filePath": "x.c", "startLine": 3,
                     "edges": []},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific flow-sensitive receiver regression.
// Reassignment changes only subsequent x.foo() targets; earlier sites retain X.
#[tokio::test]
async fn javascript_receiver_reassignment() {
    check_case(Case {
        name: "javascript_receiver_reassignment",
        files: vec![SourceFile {
            path: "reassigned.js",
            source: r"class X {
    foo() {}
}
class Y {
    foo() {}
}
function caller() {
    let x = new X();
    x.foo();
    x = new Y();
    x.foo();
}
",
        }],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "foo",
                expected: json!([
                    {"name": "X::foo", "filePath": "reassigned.js", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "reassigned.js", "startLine": 7}, "line": 9, "column": 6},
                    ]},
                    {"name": "Y::foo", "filePath": "reassigned.js", "startLine": 5,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "reassigned.js", "startLine": 7}, "line": 11, "column": 6},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "X::foo",
                expected: json!([
                    {"name": "X::foo", "filePath": "reassigned.js", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "reassigned.js", "startLine": 7}, "line": 9, "column": 6},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Y::foo",
                expected: json!([
                    {"name": "Y::foo", "filePath": "reassigned.js", "startLine": 5,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "reassigned.js", "startLine": 7}, "line": 11, "column": 6},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([
                    {"name": "caller", "filePath": "reassigned.js", "startLine": 7,
                     "edges": [
                        {"symbol": {"name": "X", "filePath": "reassigned.js", "startLine": 1}, "line": 8, "column": 16},
                        {"symbol": {"name": "X::foo", "filePath": "reassigned.js", "startLine": 2}, "line": 9, "column": 6},
                        {"symbol": {"name": "Y", "filePath": "reassigned.js", "startLine": 4}, "line": 10, "column": 12},
                        {"symbol": {"name": "Y::foo", "filePath": "reassigned.js", "startLine": 5}, "line": 11, "column": 6},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific flow-sensitive receiver regression.
// Reassignment changes only subsequent x.foo() targets; earlier sites retain X.
// The union permits both assignments; each straight-line call has one concrete receiver.
#[tokio::test]
async fn typescript_receiver_reassignment() {
    check_case(Case {
        name: "typescript_receiver_reassignment",
        files: vec![SourceFile {
            path: "reassigned.ts",
            source: r"class X {
    foo() {}
}
class Y {
    foo() {}
}
function caller() {
    let x: X | Y = new X();
    x.foo();
    x = new Y();
    x.foo();
}
",
        }],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "foo",
                expected: json!([
                    {"name": "X::foo", "filePath": "reassigned.ts", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "reassigned.ts", "startLine": 7}, "line": 9, "column": 6},
                    ]},
                    {"name": "Y::foo", "filePath": "reassigned.ts", "startLine": 5,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "reassigned.ts", "startLine": 7}, "line": 11, "column": 6},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "X::foo",
                expected: json!([
                    {"name": "X::foo", "filePath": "reassigned.ts", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "reassigned.ts", "startLine": 7}, "line": 9, "column": 6},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Y::foo",
                expected: json!([
                    {"name": "Y::foo", "filePath": "reassigned.ts", "startLine": 5,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "reassigned.ts", "startLine": 7}, "line": 11, "column": 6},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([
                    {"name": "caller", "filePath": "reassigned.ts", "startLine": 7,
                     "edges": [
                        {"symbol": {"name": "X", "filePath": "reassigned.ts", "startLine": 1}, "line": 8, "column": 23},
                        {"symbol": {"name": "X::foo", "filePath": "reassigned.ts", "startLine": 2}, "line": 9, "column": 6},
                        {"symbol": {"name": "Y", "filePath": "reassigned.ts", "startLine": 4}, "line": 10, "column": 12},
                        {"symbol": {"name": "Y::foo", "filePath": "reassigned.ts", "startLine": 5}, "line": 11, "column": 6},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific flow-sensitive receiver regression.
// Reassignment changes only subsequent x.foo() targets; earlier sites retain X.
#[tokio::test]
async fn python_receiver_reassignment() {
    check_case(Case {
        name: "python_receiver_reassignment",
        files: vec![SourceFile {
            path: "reassigned.py",
            source: r"class X:
    def foo(self):
        pass
class Y:
    def foo(self):
        pass

def caller():
    x = X()
    x.foo()
    x = Y()
    x.foo()
",
        }],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "foo",
                expected: json!([
                    {"name": "X::foo", "filePath": "reassigned.py", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "reassigned.py", "startLine": 8}, "line": 10, "column": 6},
                    ]},
                    {"name": "Y::foo", "filePath": "reassigned.py", "startLine": 5,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "reassigned.py", "startLine": 8}, "line": 12, "column": 6},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "X::foo",
                expected: json!([
                    {"name": "X::foo", "filePath": "reassigned.py", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "reassigned.py", "startLine": 8}, "line": 10, "column": 6},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Y::foo",
                expected: json!([
                    {"name": "Y::foo", "filePath": "reassigned.py", "startLine": 5,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "reassigned.py", "startLine": 8}, "line": 12, "column": 6},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([
                    {"name": "caller", "filePath": "reassigned.py", "startLine": 8,
                     "edges": [
                        {"symbol": {"name": "X", "filePath": "reassigned.py", "startLine": 1}, "line": 9, "column": 8},
                        {"symbol": {"name": "X::foo", "filePath": "reassigned.py", "startLine": 2}, "line": 10, "column": 6},
                        {"symbol": {"name": "Y", "filePath": "reassigned.py", "startLine": 4}, "line": 11, "column": 8},
                        {"symbol": {"name": "Y::foo", "filePath": "reassigned.py", "startLine": 5}, "line": 12, "column": 6},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// https://github.com/microsoft/TypeScript/blob/fed0bf24149fb1ed36039212648bafdafc1ea10e/tsc/internal/fourslash/tests/callHierarchyExportDefaultFunction_test.go
// Upstream ExportDefaultFunction baseline: the anonymous export is named "default", not its
// import alias "bar". Strip the cursor marker and retain source lines. Reverse queries supplement
// the upstream incoming/outgoing checks; foo calls default directly, not its transitive callee baz.
#[tokio::test]
async fn typescript_lsp_default_export_call_hierarchy() {
    check_case(Case {
        name: "typescript_lsp_default_export_call_hierarchy",
        files: vec![
            SourceFile {
                path: "main.ts",
                source: r#"import bar from "./other";

function foo() {
    bar();
}
"#,
            },
            SourceFile {
                path: "other.ts",
                source: r"export default function () {
    baz();
}

function baz() {
}
",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "default",
                expected: json!([
                    {"name": "default", "filePath": "other.ts", "startLine": 1,
                     "edges": [
                        {"symbol": {"name": "foo", "filePath": "main.ts", "startLine": 3}, "line": 4, "column": 4},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "default",
                expected: json!([
                    {"name": "default", "filePath": "other.ts", "startLine": 1,
                     "edges": [
                        {"symbol": {"name": "baz", "filePath": "other.ts", "startLine": 5}, "line": 2, "column": 4},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "foo",
                expected: json!([
                    {"name": "foo", "filePath": "main.ts", "startLine": 3,
                     "edges": [
                        {"symbol": {"name": "default", "filePath": "other.ts", "startLine": 1}, "line": 4, "column": 4},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "baz",
                expected: json!([
                    {"name": "baz", "filePath": "other.ts", "startLine": 5,
                     "edges": [
                        {"symbol": {"name": "default", "filePath": "other.ts", "startLine": 1}, "line": 2, "column": 4},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// https://github.com/microsoft/TypeScript/blob/fed0bf24149fb1ed36039212648bafdafc1ea10e/tsc/internal/fourslash/tests/callHierarchyTaggedTemplate_test.go
// Upstream TaggedTemplate baseline: bar`...` invokes bar without parentheses. Strip the cursor
// marker and retain source lines. Query both directions of the two direct calls; the literal
// interpolation creates no call, and baz belongs to bar rather than foo.
#[tokio::test]
async fn typescript_lsp_tagged_template_call_hierarchy() {
    check_case(Case {
        name: "typescript_lsp_tagged_template_call_hierarchy",
        files: vec![SourceFile {
            path: "tagged.ts",
            source: r"function foo() {
    bar`a${1}b`;
}

function bar(array: TemplateStringsArray, ...args: any[]) {
    baz();
}

function baz() {
}
",
        }],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "bar",
                expected: json!([
                    {"name": "bar", "filePath": "tagged.ts", "startLine": 5,
                     "edges": [
                        {"symbol": {"name": "foo", "filePath": "tagged.ts", "startLine": 1}, "line": 2, "column": 4},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "bar",
                expected: json!([
                    {"name": "bar", "filePath": "tagged.ts", "startLine": 5,
                     "edges": [
                        {"symbol": {"name": "baz", "filePath": "tagged.ts", "startLine": 9}, "line": 6, "column": 4},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "foo",
                expected: json!([
                    {"name": "foo", "filePath": "tagged.ts", "startLine": 1,
                     "edges": [
                        {"symbol": {"name": "bar", "filePath": "tagged.ts", "startLine": 5}, "line": 2, "column": 4},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "baz",
                expected: json!([
                    {"name": "baz", "filePath": "tagged.ts", "startLine": 9,
                     "edges": [
                        {"symbol": {"name": "bar", "filePath": "tagged.ts", "startLine": 5}, "line": 6, "column": 4},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific self/this receiver regression.
// Resolve each explicit receiver against its enclosing type; the two foo definitions must not mix.
#[tokio::test]
async fn java_self_this_direct_receiver() {
    check_case(Case {
        name: "java_self_this_direct_receiver",
        files: vec![SourceFile {
            path: "receivers.java",
            source: r"class Left {
    void foo() {}
    void caller() {
        this.foo();
    }
}
class Right {
    void foo() {}
    void caller() {
        this.foo();
    }
}
",
        }],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "foo",
                expected: json!([
                    {"name": "Left::foo", "filePath": "receivers.java", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "Left::caller", "filePath": "receivers.java", "startLine": 3}, "line": 4, "column": 13},
                    ]},
                    {"name": "Right::foo", "filePath": "receivers.java", "startLine": 8,
                     "edges": [
                        {"symbol": {"name": "Right::caller", "filePath": "receivers.java", "startLine": 9}, "line": 10, "column": 13},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "Left::caller",
                expected: json!([
                    {"name": "Left::caller", "filePath": "receivers.java", "startLine": 3,
                     "edges": [
                        {"symbol": {"name": "Left::foo", "filePath": "receivers.java", "startLine": 2}, "line": 4, "column": 13},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "Right::caller",
                expected: json!([
                    {"name": "Right::caller", "filePath": "receivers.java", "startLine": 9,
                     "edges": [
                        {"symbol": {"name": "Right::foo", "filePath": "receivers.java", "startLine": 8}, "line": 10, "column": 13},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Left::foo",
                expected: json!([
                    {"name": "Left::foo", "filePath": "receivers.java", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "Left::caller", "filePath": "receivers.java", "startLine": 3}, "line": 4, "column": 13},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific self/this receiver regression.
// Resolve each explicit receiver against its enclosing type; the two foo definitions must not mix.
#[tokio::test]
async fn cpp_self_this_direct_receiver() {
    check_case(Case {
        name: "cpp_self_this_direct_receiver",
        files: vec![SourceFile {
            path: "receivers.cc",
            source: r"struct Left {
    void foo() {}
    void caller() {
        this->foo();
    }
};
struct Right {
    void foo() {}
    void caller() {
        this->foo();
    }
};
",
        }],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "foo",
                expected: json!([
                    {"name": "Left::foo", "filePath": "receivers.cc", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "Left::caller", "filePath": "receivers.cc", "startLine": 3}, "line": 4, "column": 14},
                    ]},
                    {"name": "Right::foo", "filePath": "receivers.cc", "startLine": 8,
                     "edges": [
                        {"symbol": {"name": "Right::caller", "filePath": "receivers.cc", "startLine": 9}, "line": 10, "column": 14},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "Left::caller",
                expected: json!([
                    {"name": "Left::caller", "filePath": "receivers.cc", "startLine": 3,
                     "edges": [
                        {"symbol": {"name": "Left::foo", "filePath": "receivers.cc", "startLine": 2}, "line": 4, "column": 14},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "Right::caller",
                expected: json!([
                    {"name": "Right::caller", "filePath": "receivers.cc", "startLine": 9,
                     "edges": [
                        {"symbol": {"name": "Right::foo", "filePath": "receivers.cc", "startLine": 8}, "line": 10, "column": 14},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Left::foo",
                expected: json!([
                    {"name": "Left::foo", "filePath": "receivers.cc", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "Left::caller", "filePath": "receivers.cc", "startLine": 3}, "line": 4, "column": 14},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific self/this receiver regression.
// Resolve each explicit receiver against its enclosing type; the two foo definitions must not mix.
#[tokio::test]
async fn rust_self_this_direct_receiver() {
    check_case(Case {
        name: "rust_self_this_direct_receiver",
        files: vec![SourceFile {
            path: "receivers.rs",
            source: r"struct Left;
impl Left {
    fn foo(&self) {}
    fn caller(&self) {
        self.foo();
    }
}
struct Right;
impl Right {
    fn foo(&self) {}
    fn caller(&self) {
        self.foo();
    }
}
",
        }],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "foo",
                expected: json!([
                    {"name": "Left::foo", "filePath": "receivers.rs", "startLine": 3,
                     "edges": [
                        {"symbol": {"name": "Left::caller", "filePath": "receivers.rs", "startLine": 4}, "line": 5, "column": 13},
                    ]},
                    {"name": "Right::foo", "filePath": "receivers.rs", "startLine": 10,
                     "edges": [
                        {"symbol": {"name": "Right::caller", "filePath": "receivers.rs", "startLine": 11}, "line": 12, "column": 13},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "Left::caller",
                expected: json!([
                    {"name": "Left::caller", "filePath": "receivers.rs", "startLine": 4,
                     "edges": [
                        {"symbol": {"name": "Left::foo", "filePath": "receivers.rs", "startLine": 3}, "line": 5, "column": 13},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "Right::caller",
                expected: json!([
                    {"name": "Right::caller", "filePath": "receivers.rs", "startLine": 11,
                     "edges": [
                        {"symbol": {"name": "Right::foo", "filePath": "receivers.rs", "startLine": 10}, "line": 12, "column": 13},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Left::foo",
                expected: json!([
                    {"name": "Left::foo", "filePath": "receivers.rs", "startLine": 3,
                     "edges": [
                        {"symbol": {"name": "Left::caller", "filePath": "receivers.rs", "startLine": 4}, "line": 5, "column": 13},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific self/this receiver regression.
// Resolve each explicit receiver against its enclosing type; the two foo definitions must not mix.
#[tokio::test]
async fn typescript_self_this_direct_receiver() {
    check_case(Case {
        name: "typescript_self_this_direct_receiver",
        files: vec![SourceFile {
            path: "receivers.ts",
            source: r"class Left {
    foo() {}
    caller() {
        this.foo();
    }
}
class Right {
    foo() {}
    caller() {
        this.foo();
    }
}
",
        }],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "foo",
                expected: json!([
                    {"name": "Left::foo", "filePath": "receivers.ts", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "Left::caller", "filePath": "receivers.ts", "startLine": 3}, "line": 4, "column": 13},
                    ]},
                    {"name": "Right::foo", "filePath": "receivers.ts", "startLine": 8,
                     "edges": [
                        {"symbol": {"name": "Right::caller", "filePath": "receivers.ts", "startLine": 9}, "line": 10, "column": 13},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "Left::caller",
                expected: json!([
                    {"name": "Left::caller", "filePath": "receivers.ts", "startLine": 3,
                     "edges": [
                        {"symbol": {"name": "Left::foo", "filePath": "receivers.ts", "startLine": 2}, "line": 4, "column": 13},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "Right::caller",
                expected: json!([
                    {"name": "Right::caller", "filePath": "receivers.ts", "startLine": 9,
                     "edges": [
                        {"symbol": {"name": "Right::foo", "filePath": "receivers.ts", "startLine": 8}, "line": 10, "column": 13},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Left::foo",
                expected: json!([
                    {"name": "Left::foo", "filePath": "receivers.ts", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "Left::caller", "filePath": "receivers.ts", "startLine": 3}, "line": 4, "column": 13},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific self/this receiver regression.
// Resolve each explicit receiver against its enclosing type; the two foo definitions must not mix.
// The first instance-method parameter is named me, not self; parameter binding determines the receiver.
#[tokio::test]
async fn python_self_this_named_me_receiver() {
    check_case(Case {
        name: "python_self_this_named_me_receiver",
        files: vec![SourceFile {
            path: "receivers.py",
            source: r"class Left:
    def foo(me):
        pass
    def caller(me):
        me.foo()
class Right:
    def foo(me):
        pass
    def caller(me):
        me.foo()
",
        }],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "foo",
                expected: json!([
                    {"name": "Left::foo", "filePath": "receivers.py", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "Left::caller", "filePath": "receivers.py", "startLine": 4}, "line": 5, "column": 11},
                    ]},
                    {"name": "Right::foo", "filePath": "receivers.py", "startLine": 7,
                     "edges": [
                        {"symbol": {"name": "Right::caller", "filePath": "receivers.py", "startLine": 9}, "line": 10, "column": 11},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "Left::caller",
                expected: json!([
                    {"name": "Left::caller", "filePath": "receivers.py", "startLine": 4,
                     "edges": [
                        {"symbol": {"name": "Left::foo", "filePath": "receivers.py", "startLine": 2}, "line": 5, "column": 11},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "Right::caller",
                expected: json!([
                    {"name": "Right::caller", "filePath": "receivers.py", "startLine": 9,
                     "edges": [
                        {"symbol": {"name": "Right::foo", "filePath": "receivers.py", "startLine": 7}, "line": 10, "column": 11},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Left::foo",
                expected: json!([
                    {"name": "Left::foo", "filePath": "receivers.py", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "Left::caller", "filePath": "receivers.py", "startLine": 4}, "line": 5, "column": 11},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific self/this receiver regression.
// The current type has no foo definition; self/this must resolve to inherited Base::foo.
// The unrelated Other::foo must have no incoming calls.
#[tokio::test]
async fn java_self_this_inherited_receiver() {
    check_case(Case {
        name: "java_self_this_inherited_receiver",
        files: vec![SourceFile {
            path: "inheritance.java",
            source: r"class Base {
    void foo() {}
}
class Derived extends Base {
    void caller() {
        this.foo();
    }
}
class Other {
    void foo() {}
}
",
        }],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "foo",
                expected: json!([
                    {"name": "Base::foo", "filePath": "inheritance.java", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "Derived::caller", "filePath": "inheritance.java", "startLine": 5}, "line": 6, "column": 13},
                    ]},
                    {"name": "Other::foo", "filePath": "inheritance.java", "startLine": 10,
                     "edges": []},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "Derived::caller",
                expected: json!([
                    {"name": "Derived::caller", "filePath": "inheritance.java", "startLine": 5,
                     "edges": [
                        {"symbol": {"name": "Base::foo", "filePath": "inheritance.java", "startLine": 2}, "line": 6, "column": 13},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Base::foo",
                expected: json!([
                    {"name": "Base::foo", "filePath": "inheritance.java", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "Derived::caller", "filePath": "inheritance.java", "startLine": 5}, "line": 6, "column": 13},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific self/this receiver regression.
// An explicit parent call selects Base::foo even though Derived defines foo too.
// Derived::foo and the unrelated Other::foo must have no incoming calls.
#[tokio::test]
async fn java_self_this_explicit_parent_receiver() {
    check_case(Case {
        name: "java_self_this_explicit_parent_receiver",
        files: vec![SourceFile {
            path: "inheritance.java",
            source: r"class Base {
    void foo() {}
}
class Derived extends Base {
    void foo() {}
    void caller() {
        super.foo();
    }
}
class Other {
    void foo() {}
}
",
        }],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "foo",
                expected: json!([
                    {"name": "Base::foo", "filePath": "inheritance.java", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "Derived::caller", "filePath": "inheritance.java", "startLine": 6}, "line": 7, "column": 14},
                    ]},
                    {"name": "Derived::foo", "filePath": "inheritance.java", "startLine": 5,
                     "edges": []},
                    {"name": "Other::foo", "filePath": "inheritance.java", "startLine": 11,
                     "edges": []},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "Derived::caller",
                expected: json!([
                    {"name": "Derived::caller", "filePath": "inheritance.java", "startLine": 6,
                     "edges": [
                        {"symbol": {"name": "Base::foo", "filePath": "inheritance.java", "startLine": 2}, "line": 7, "column": 14},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Base::foo",
                expected: json!([
                    {"name": "Base::foo", "filePath": "inheritance.java", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "Derived::caller", "filePath": "inheritance.java", "startLine": 6}, "line": 7, "column": 14},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific self/this receiver regression.
// The current type has no foo definition; self/this must resolve to inherited Base::foo.
// The unrelated Other::foo must have no incoming calls.
// Methods are non-virtual so this fixture does not require any speculative override expansion.
#[tokio::test]
async fn cpp_self_this_inherited_receiver() {
    check_case(Case {
        name: "cpp_self_this_inherited_receiver",
        files: vec![SourceFile {
            path: "inheritance.cc",
            source: r"struct Base {
    void foo() {}
};
struct Derived : Base {
    void caller() {
        this->foo();
    }
};
struct Other {
    void foo() {}
};
",
        }],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "foo",
                expected: json!([
                    {"name": "Base::foo", "filePath": "inheritance.cc", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "Derived::caller", "filePath": "inheritance.cc", "startLine": 5}, "line": 6, "column": 14},
                    ]},
                    {"name": "Other::foo", "filePath": "inheritance.cc", "startLine": 10,
                     "edges": []},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "Derived::caller",
                expected: json!([
                    {"name": "Derived::caller", "filePath": "inheritance.cc", "startLine": 5,
                     "edges": [
                        {"symbol": {"name": "Base::foo", "filePath": "inheritance.cc", "startLine": 2}, "line": 6, "column": 14},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Base::foo",
                expected: json!([
                    {"name": "Base::foo", "filePath": "inheritance.cc", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "Derived::caller", "filePath": "inheritance.cc", "startLine": 5}, "line": 6, "column": 14},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific self/this receiver regression.
// An explicit parent call selects Base::foo even though Derived defines foo too.
// Derived::foo and the unrelated Other::foo must have no incoming calls.
// Methods are non-virtual so this fixture does not require any speculative override expansion.
#[tokio::test]
async fn cpp_self_this_explicit_parent_receiver() {
    check_case(Case {
        name: "cpp_self_this_explicit_parent_receiver",
        files: vec![SourceFile {
            path: "inheritance.cc",
            source: r"struct Base {
    void foo() {}
};
struct Derived : Base {
    void foo() {}
    void caller() {
        Base::foo();
    }
};
struct Other {
    void foo() {}
};
",
        }],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "foo",
                expected: json!([
                    {"name": "Base::foo", "filePath": "inheritance.cc", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "Derived::caller", "filePath": "inheritance.cc", "startLine": 6}, "line": 7, "column": 14},
                    ]},
                    {"name": "Derived::foo", "filePath": "inheritance.cc", "startLine": 5,
                     "edges": []},
                    {"name": "Other::foo", "filePath": "inheritance.cc", "startLine": 11,
                     "edges": []},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "Derived::caller",
                expected: json!([
                    {"name": "Derived::caller", "filePath": "inheritance.cc", "startLine": 6,
                     "edges": [
                        {"symbol": {"name": "Base::foo", "filePath": "inheritance.cc", "startLine": 2}, "line": 7, "column": 14},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Base::foo",
                expected: json!([
                    {"name": "Base::foo", "filePath": "inheritance.cc", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "Derived::caller", "filePath": "inheritance.cc", "startLine": 6}, "line": 7, "column": 14},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific self/this receiver regression.
// The current type has no foo definition; self/this must resolve to inherited Base::foo.
// The unrelated Other::foo must have no incoming calls.
#[tokio::test]
async fn javascript_self_this_inherited_receiver() {
    check_case(Case {
        name: "javascript_self_this_inherited_receiver",
        files: vec![SourceFile {
            path: "inheritance.js",
            source: r"class Base {
    foo() {}
}
class Derived extends Base {
    caller() {
        this.foo();
    }
}
class Other {
    foo() {}
}
",
        }],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "foo",
                expected: json!([
                    {"name": "Base::foo", "filePath": "inheritance.js", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "Derived::caller", "filePath": "inheritance.js", "startLine": 5}, "line": 6, "column": 13},
                    ]},
                    {"name": "Other::foo", "filePath": "inheritance.js", "startLine": 10,
                     "edges": []},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "Derived::caller",
                expected: json!([
                    {"name": "Derived::caller", "filePath": "inheritance.js", "startLine": 5,
                     "edges": [
                        {"symbol": {"name": "Base::foo", "filePath": "inheritance.js", "startLine": 2}, "line": 6, "column": 13},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Base::foo",
                expected: json!([
                    {"name": "Base::foo", "filePath": "inheritance.js", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "Derived::caller", "filePath": "inheritance.js", "startLine": 5}, "line": 6, "column": 13},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific self/this receiver regression.
// An explicit parent call selects Base::foo even though Derived defines foo too.
// Derived::foo and the unrelated Other::foo must have no incoming calls.
#[tokio::test]
async fn javascript_self_this_explicit_parent_receiver() {
    check_case(Case {
        name: "javascript_self_this_explicit_parent_receiver",
        files: vec![SourceFile {
            path: "inheritance.js",
            source: r"class Base {
    foo() {}
}
class Derived extends Base {
    foo() {}
    caller() {
        super.foo();
    }
}
class Other {
    foo() {}
}
",
        }],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "foo",
                expected: json!([
                    {"name": "Base::foo", "filePath": "inheritance.js", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "Derived::caller", "filePath": "inheritance.js", "startLine": 6}, "line": 7, "column": 14},
                    ]},
                    {"name": "Derived::foo", "filePath": "inheritance.js", "startLine": 5,
                     "edges": []},
                    {"name": "Other::foo", "filePath": "inheritance.js", "startLine": 11,
                     "edges": []},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "Derived::caller",
                expected: json!([
                    {"name": "Derived::caller", "filePath": "inheritance.js", "startLine": 6,
                     "edges": [
                        {"symbol": {"name": "Base::foo", "filePath": "inheritance.js", "startLine": 2}, "line": 7, "column": 14},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Base::foo",
                expected: json!([
                    {"name": "Base::foo", "filePath": "inheritance.js", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "Derived::caller", "filePath": "inheritance.js", "startLine": 6}, "line": 7, "column": 14},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific self/this receiver regression.
// The current type has no foo definition; self/this must resolve to inherited Base::foo.
// The unrelated Other::foo must have no incoming calls.
#[tokio::test]
async fn typescript_self_this_inherited_receiver() {
    check_case(Case {
        name: "typescript_self_this_inherited_receiver",
        files: vec![SourceFile {
            path: "inheritance.ts",
            source: r"class Base {
    foo() {}
}
class Derived extends Base {
    caller() {
        this.foo();
    }
}
class Other {
    foo() {}
}
",
        }],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "foo",
                expected: json!([
                    {"name": "Base::foo", "filePath": "inheritance.ts", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "Derived::caller", "filePath": "inheritance.ts", "startLine": 5}, "line": 6, "column": 13},
                    ]},
                    {"name": "Other::foo", "filePath": "inheritance.ts", "startLine": 10,
                     "edges": []},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "Derived::caller",
                expected: json!([
                    {"name": "Derived::caller", "filePath": "inheritance.ts", "startLine": 5,
                     "edges": [
                        {"symbol": {"name": "Base::foo", "filePath": "inheritance.ts", "startLine": 2}, "line": 6, "column": 13},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Base::foo",
                expected: json!([
                    {"name": "Base::foo", "filePath": "inheritance.ts", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "Derived::caller", "filePath": "inheritance.ts", "startLine": 5}, "line": 6, "column": 13},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific self/this receiver regression.
// An explicit parent call selects Base::foo even though Derived defines foo too.
// Derived::foo and the unrelated Other::foo must have no incoming calls.
#[tokio::test]
async fn typescript_self_this_explicit_parent_receiver() {
    check_case(Case {
        name: "typescript_self_this_explicit_parent_receiver",
        files: vec![SourceFile {
            path: "inheritance.ts",
            source: r"class Base {
    foo() {}
}
class Derived extends Base {
    foo() {}
    caller() {
        super.foo();
    }
}
class Other {
    foo() {}
}
",
        }],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "foo",
                expected: json!([
                    {"name": "Base::foo", "filePath": "inheritance.ts", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "Derived::caller", "filePath": "inheritance.ts", "startLine": 6}, "line": 7, "column": 14},
                    ]},
                    {"name": "Derived::foo", "filePath": "inheritance.ts", "startLine": 5,
                     "edges": []},
                    {"name": "Other::foo", "filePath": "inheritance.ts", "startLine": 11,
                     "edges": []},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "Derived::caller",
                expected: json!([
                    {"name": "Derived::caller", "filePath": "inheritance.ts", "startLine": 6,
                     "edges": [
                        {"symbol": {"name": "Base::foo", "filePath": "inheritance.ts", "startLine": 2}, "line": 7, "column": 14},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Base::foo",
                expected: json!([
                    {"name": "Base::foo", "filePath": "inheritance.ts", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "Derived::caller", "filePath": "inheritance.ts", "startLine": 6}, "line": 7, "column": 14},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific self/this receiver regression.
// The current type has no foo definition; self/this must resolve to inherited Base::foo.
// The unrelated Other::foo must have no incoming calls.
#[tokio::test]
async fn python_self_this_inherited_receiver() {
    check_case(Case {
        name: "python_self_this_inherited_receiver",
        files: vec![SourceFile {
            path: "inheritance.py",
            source: r"class Base:
    def foo(self):
        pass
class Derived(Base):
    def caller(self):
        self.foo()
class Other:
    def foo(self):
        pass
",
        }],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "foo",
                expected: json!([
                    {"name": "Base::foo", "filePath": "inheritance.py", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "Derived::caller", "filePath": "inheritance.py", "startLine": 5}, "line": 6, "column": 13},
                    ]},
                    {"name": "Other::foo", "filePath": "inheritance.py", "startLine": 8,
                     "edges": []},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "Derived::caller",
                expected: json!([
                    {"name": "Derived::caller", "filePath": "inheritance.py", "startLine": 5,
                     "edges": [
                        {"symbol": {"name": "Base::foo", "filePath": "inheritance.py", "startLine": 2}, "line": 6, "column": 13},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Base::foo",
                expected: json!([
                    {"name": "Base::foo", "filePath": "inheritance.py", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "Derived::caller", "filePath": "inheritance.py", "startLine": 5}, "line": 6, "column": 13},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific self/this receiver regression.
// An explicit parent call selects Base::foo even though Derived defines foo too.
// Derived::foo and the unrelated Other::foo must have no incoming calls.
// The builtin super() has no indexed definition and must not produce a synthetic call target.
#[tokio::test]
async fn python_self_this_explicit_parent_receiver() {
    check_case(Case {
        name: "python_self_this_explicit_parent_receiver",
        files: vec![SourceFile {
            path: "inheritance.py",
            source: r"class Base:
    def foo(self):
        pass
class Derived(Base):
    def foo(self):
        pass
    def caller(self):
        super().foo()
class Other:
    def foo(self):
        pass
",
        }],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "foo",
                expected: json!([
                    {"name": "Base::foo", "filePath": "inheritance.py", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "Derived::caller", "filePath": "inheritance.py", "startLine": 7}, "line": 8, "column": 16},
                    ]},
                    {"name": "Derived::foo", "filePath": "inheritance.py", "startLine": 5,
                     "edges": []},
                    {"name": "Other::foo", "filePath": "inheritance.py", "startLine": 10,
                     "edges": []},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "Derived::caller",
                expected: json!([
                    {"name": "Derived::caller", "filePath": "inheritance.py", "startLine": 7,
                     "edges": [
                        {"symbol": {"name": "Base::foo", "filePath": "inheritance.py", "startLine": 2}, "line": 8, "column": 16},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Base::foo",
                expected: json!([
                    {"name": "Base::foo", "filePath": "inheritance.py", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "Derived::caller", "filePath": "inheritance.py", "startLine": 7}, "line": 8, "column": 16},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific module-alias regression: import module as alias binds a module namespace.
// Both modules export work and stop. Calls resolve to the original work definitions, preserve
// repeated sites and never cross modules; namespace imports do not call the unused stop exports.
#[tokio::test]
async fn python_module_alias_calls() {
    check_case(Case {
        name: "python_module_alias_calls",
        files: vec![
            SourceFile {
                path: "left.py",
                source: r"def work():
    pass

def stop():
    pass
",
            },
            SourceFile {
                path: "right.py",
                source: r"def work():
    pass

def stop():
    pass
",
            },
            SourceFile {
                path: "caller.py",
                source: r"import left as left_api
import right as right_api
def caller():
    left_api.work()
    right_api.work()
    left_api.work()
",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([
                    {"name": "caller", "filePath": "caller.py", "startLine": 3,
                     "edges": [
                        {"symbol": {"name": "work", "filePath": "left.py", "startLine": 1}, "line": [4, 6], "column": 13},
                        {"symbol": {"name": "work", "filePath": "right.py", "startLine": 1}, "line": 5, "column": 14},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "work",
                expected: json!([
                    {"name": "work", "filePath": "left.py", "startLine": 1,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "caller.py", "startLine": 3}, "line": [4, 6], "column": 13},
                    ]},
                    {"name": "work", "filePath": "right.py", "startLine": 1,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "caller.py", "startLine": 3}, "line": 5, "column": 14},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "stop",
                expected: json!([
                    {"name": "stop", "filePath": "left.py", "startLine": 4,
                     "edges": []},
                    {"name": "stop", "filePath": "right.py", "startLine": 4,
                     "edges": []},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific module-alias regression: import * as alias binds a module namespace.
// Both modules export work and stop. Calls resolve to the original work definitions, preserve
// repeated sites and never cross modules; namespace imports do not call the unused stop exports.
#[tokio::test]
async fn javascript_module_alias_calls() {
    check_case(Case {
        name: "javascript_module_alias_calls",
        files: vec![
            SourceFile {
                path: "left.js",
                source: r"export function work() {}
export function stop() {}
",
            },
            SourceFile {
                path: "right.js",
                source: r"export function work() {}
export function stop() {}
",
            },
            SourceFile {
                path: "caller.js",
                source: r#"import * as leftApi from "./left";
import * as rightApi from "./right";
function caller() {
    leftApi.work();
    rightApi.work();
    leftApi.work();
}
"#,
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([
                    {"name": "caller", "filePath": "caller.js", "startLine": 3,
                     "edges": [
                        {"symbol": {"name": "work", "filePath": "left.js", "startLine": 1}, "line": [4, 6], "column": 12},
                        {"symbol": {"name": "work", "filePath": "right.js", "startLine": 1}, "line": 5, "column": 13},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "work",
                expected: json!([
                    {"name": "work", "filePath": "left.js", "startLine": 1,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "caller.js", "startLine": 3}, "line": [4, 6], "column": 12},
                    ]},
                    {"name": "work", "filePath": "right.js", "startLine": 1,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "caller.js", "startLine": 3}, "line": 5, "column": 13},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "stop",
                expected: json!([
                    {"name": "stop", "filePath": "left.js", "startLine": 2,
                     "edges": []},
                    {"name": "stop", "filePath": "right.js", "startLine": 2,
                     "edges": []},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific module-alias regression: import * as alias binds a module namespace.
// Both modules export work and stop. Calls resolve to the original work definitions, preserve
// repeated sites and never cross modules; namespace imports do not call the unused stop exports.
#[tokio::test]
async fn typescript_module_alias_calls() {
    check_case(Case {
        name: "typescript_module_alias_calls",
        files: vec![
            SourceFile {
                path: "left.ts",
                source: r"export function work() {}
export function stop() {}
",
            },
            SourceFile {
                path: "right.ts",
                source: r"export function work() {}
export function stop() {}
",
            },
            SourceFile {
                path: "caller.ts",
                source: r#"import * as leftApi from "./left";
import * as rightApi from "./right";
function caller() {
    leftApi.work();
    rightApi.work();
    leftApi.work();
}
"#,
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([
                    {"name": "caller", "filePath": "caller.ts", "startLine": 3,
                     "edges": [
                        {"symbol": {"name": "work", "filePath": "left.ts", "startLine": 1}, "line": [4, 6], "column": 12},
                        {"symbol": {"name": "work", "filePath": "right.ts", "startLine": 1}, "line": 5, "column": 13},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "work",
                expected: json!([
                    {"name": "work", "filePath": "left.ts", "startLine": 1,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "caller.ts", "startLine": 3}, "line": [4, 6], "column": 12},
                    ]},
                    {"name": "work", "filePath": "right.ts", "startLine": 1,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "caller.ts", "startLine": 3}, "line": 5, "column": 13},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "stop",
                expected: json!([
                    {"name": "stop", "filePath": "left.ts", "startLine": 2,
                     "edges": []},
                    {"name": "stop", "filePath": "right.ts", "startLine": 2,
                     "edges": []},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific binding regression.
// The explicit import points outside the indexed workspace.
// A single same-named workspace definition is not evidence that it is the imported target.
// Keep the known direct call while leaving the unavailable dependency unresolved.
#[tokio::test]
async fn python_binding_unavailable_import_target() {
    check_case(Case {
        name: "python_binding_unavailable_import_target",
        files: vec![
            SourceFile {
                path: "decoy.py",
                source: r"def work():
    pass
",
            },
            SourceFile {
                path: "caller.py",
                source: r"from unindexed_lib import work as run
def known():
    pass
def caller():
    known()
    run()
",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([
                    {"name": "caller", "filePath": "caller.py", "startLine": 4,
                     "edges": [
                        {"symbol": {"name": "known", "filePath": "caller.py", "startLine": 2}, "line": 5, "column": 4},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "work",
                expected: json!([
                    {"name": "work", "filePath": "decoy.py", "startLine": 1,
                     "edges": []},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "known",
                expected: json!([
                    {"name": "known", "filePath": "caller.py", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "caller.py", "startLine": 4}, "line": 5, "column": 4},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific binding regression.
// The external factory is imported from a dependency absent from the workspace.
// Neither its name nor its unknown return receiver may bind to the unrelated local make/foo.
// The known direct call must still be returned; unresolved calls have no resolved query edges.
#[tokio::test]
async fn python_binding_unknown_factory_receiver() {
    check_case(Case {
        name: "python_binding_unknown_factory_receiver",
        files: vec![
            SourceFile {
                path: "decoy.py",
                source: r"def make():
    pass
class Local:
    def foo(self):
        pass
",
            },
            SourceFile {
                path: "caller.py",
                source: r"from unindexed_lib import make as external_factory
def known():
    pass
def caller():
    known()
    external_factory().foo()
",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([
                    {"name": "caller", "filePath": "caller.py", "startLine": 4,
                     "edges": [
                        {"symbol": {"name": "known", "filePath": "caller.py", "startLine": 2}, "line": 5, "column": 4},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "make",
                expected: json!([
                    {"name": "make", "filePath": "decoy.py", "startLine": 1,
                     "edges": []},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Local::foo",
                expected: json!([
                    {"name": "Local::foo", "filePath": "decoy.py", "startLine": 4,
                     "edges": []},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "known",
                expected: json!([
                    {"name": "known", "filePath": "caller.py", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "caller.py", "startLine": 4}, "line": 5, "column": 4},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific binding regression.
// The explicit import points outside the indexed workspace.
// A single same-named workspace definition is not evidence that it is the imported target.
// Keep the known direct call while leaving the unavailable dependency unresolved.
#[tokio::test]
async fn javascript_binding_unavailable_import_target() {
    check_case(Case {
        name: "javascript_binding_unavailable_import_target",
        files: vec![
            SourceFile {
                path: "decoy.js",
                source: r"export function work() {}
",
            },
            SourceFile {
                path: "caller.js",
                source: r#"import { work as run } from "unindexed-lib";
function known() {}
function caller() {
    known();
    run();
}
"#,
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([
                    {"name": "caller", "filePath": "caller.js", "startLine": 3,
                     "edges": [
                        {"symbol": {"name": "known", "filePath": "caller.js", "startLine": 2}, "line": 4, "column": 4},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "work",
                expected: json!([
                    {"name": "work", "filePath": "decoy.js", "startLine": 1,
                     "edges": []},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "known",
                expected: json!([
                    {"name": "known", "filePath": "caller.js", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "caller.js", "startLine": 3}, "line": 4, "column": 4},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific binding regression.
// The external factory is imported from a dependency absent from the workspace.
// Neither its name nor its unknown return receiver may bind to the unrelated local make/foo.
// The known direct call must still be returned; unresolved calls have no resolved query edges.
#[tokio::test]
async fn javascript_binding_unknown_factory_receiver() {
    check_case(Case {
        name: "javascript_binding_unknown_factory_receiver",
        files: vec![
            SourceFile {
                path: "decoy.js",
                source: r"export function make() {}
export class Local {
    foo() {}
}
",
            },
            SourceFile {
                path: "caller.js",
                source: r#"import { make as externalFactory } from "unindexed-lib";
function known() {}
function caller() {
    known();
    externalFactory().foo();
}
"#,
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([
                    {"name": "caller", "filePath": "caller.js", "startLine": 3,
                     "edges": [
                        {"symbol": {"name": "known", "filePath": "caller.js", "startLine": 2}, "line": 4, "column": 4},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "make",
                expected: json!([
                    {"name": "make", "filePath": "decoy.js", "startLine": 1,
                     "edges": []},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Local::foo",
                expected: json!([
                    {"name": "Local::foo", "filePath": "decoy.js", "startLine": 3,
                     "edges": []},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "known",
                expected: json!([
                    {"name": "known", "filePath": "caller.js", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "caller.js", "startLine": 3}, "line": 4, "column": 4},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific binding regression.
// The explicit import points outside the indexed workspace.
// A single same-named workspace definition is not evidence that it is the imported target.
// Keep the known direct call while leaving the unavailable dependency unresolved.
#[tokio::test]
async fn typescript_binding_unavailable_import_target() {
    check_case(Case {
        name: "typescript_binding_unavailable_import_target",
        files: vec![
            SourceFile {
                path: "decoy.ts",
                source: r"export function work() {}
",
            },
            SourceFile {
                path: "caller.ts",
                source: r#"import { work as run } from "unindexed-lib";
function known() {}
function caller() {
    known();
    run();
}
"#,
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([
                    {"name": "caller", "filePath": "caller.ts", "startLine": 3,
                     "edges": [
                        {"symbol": {"name": "known", "filePath": "caller.ts", "startLine": 2}, "line": 4, "column": 4},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "work",
                expected: json!([
                    {"name": "work", "filePath": "decoy.ts", "startLine": 1,
                     "edges": []},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "known",
                expected: json!([
                    {"name": "known", "filePath": "caller.ts", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "caller.ts", "startLine": 3}, "line": 4, "column": 4},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific binding regression.
// The external factory is imported from a dependency absent from the workspace.
// Neither its name nor its unknown return receiver may bind to the unrelated local make/foo.
// The known direct call must still be returned; unresolved calls have no resolved query edges.
#[tokio::test]
async fn typescript_binding_unknown_factory_receiver() {
    check_case(Case {
        name: "typescript_binding_unknown_factory_receiver",
        files: vec![
            SourceFile {
                path: "decoy.ts",
                source: r"export function make() {}
export class Local {
    foo() {}
}
",
            },
            SourceFile {
                path: "caller.ts",
                source: r#"import { make as externalFactory } from "unindexed-lib";
function known() {}
function caller() {
    known();
    externalFactory().foo();
}
"#,
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([
                    {"name": "caller", "filePath": "caller.ts", "startLine": 3,
                     "edges": [
                        {"symbol": {"name": "known", "filePath": "caller.ts", "startLine": 2}, "line": 4, "column": 4},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "make",
                expected: json!([
                    {"name": "make", "filePath": "decoy.ts", "startLine": 1,
                     "edges": []},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Local::foo",
                expected: json!([
                    {"name": "Local::foo", "filePath": "decoy.ts", "startLine": 3,
                     "edges": []},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "known",
                expected: json!([
                    {"name": "known", "filePath": "caller.ts", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "caller.ts", "startLine": 3}, "line": 4, "column": 4},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// https://github.com/microsoft/TypeScript/blob/fed0bf24149fb1ed36039212648bafdafc1ea10e/tsc/internal/fourslash/tests/callHierarchyConstNamedClassExpression_test.go
// Upstream ConstNamedClassExpression.
// Strip the cursor marker and retain the upstream class-level constructor hierarchy.
// The variable supplies the anonymous class name Bar; constructor calls to baz belong to Bar.
// Reverse queries supplement the upstream incoming/outgoing checks and exclude transitive edges.
#[tokio::test]
async fn typescript_upstream_class_expression() {
    check_case(Case {
        name: "typescript_upstream_class_expression",
        files: vec![SourceFile {
            path: "expression.ts",
            source: r"function foo() {
    new Bar();
}

const Bar = class {
    constructor() {
        baz();
    }
}

function baz() {
}
",
        }],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "Bar",
                expected: json!([
                    {"name": "Bar", "filePath": "expression.ts", "startLine": 5,
                     "edges": [
                        {"symbol": {"name": "foo", "filePath": "expression.ts", "startLine": 1}, "line": 2, "column": 8},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "Bar",
                expected: json!([
                    {"name": "Bar", "filePath": "expression.ts", "startLine": 5,
                     "edges": [
                        {"symbol": {"name": "baz", "filePath": "expression.ts", "startLine": 11}, "line": 7, "column": 8},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "foo",
                expected: json!([
                    {"name": "foo", "filePath": "expression.ts", "startLine": 1,
                     "edges": [
                        {"symbol": {"name": "Bar", "filePath": "expression.ts", "startLine": 5}, "line": 2, "column": 8},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "baz",
                expected: json!([
                    {"name": "baz", "filePath": "expression.ts", "startLine": 11,
                     "edges": [
                        {"symbol": {"name": "Bar", "filePath": "expression.ts", "startLine": 5}, "line": 7, "column": 8},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// https://github.com/microsoft/TypeScript/blob/fed0bf24149fb1ed36039212648bafdafc1ea10e/tsc/internal/fourslash/tests/callHierarchyExportDefaultClass_test.go
// Upstream ExportDefaultClass.
// Strip the cursor marker. The upstream canonical anonymous class name is default, not Bar.
// Preserve class-level ownership of constructor calls and add inverse queries for both edges.
#[tokio::test]
async fn typescript_upstream_default_export_class() {
    check_case(Case {
        name: "typescript_upstream_default_export_class",
        files: vec![
            SourceFile {
                path: "main.ts",
                source: r#"import Bar from "./other";

function foo() {
    new Bar();
}
"#,
            },
            SourceFile {
                path: "other.ts",
                source: r"export default class {
    constructor() {
        baz();
    }
}

function baz() {
}
",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "default",
                expected: json!([
                    {"name": "default", "filePath": "other.ts", "startLine": 1,
                     "edges": [
                        {"symbol": {"name": "foo", "filePath": "main.ts", "startLine": 3}, "line": 4, "column": 8},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "default",
                expected: json!([
                    {"name": "default", "filePath": "other.ts", "startLine": 1,
                     "edges": [
                        {"symbol": {"name": "baz", "filePath": "other.ts", "startLine": 7}, "line": 3, "column": 8},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "foo",
                expected: json!([
                    {"name": "foo", "filePath": "main.ts", "startLine": 3,
                     "edges": [
                        {"symbol": {"name": "default", "filePath": "other.ts", "startLine": 1}, "line": 4, "column": 8},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "baz",
                expected: json!([
                    {"name": "baz", "filePath": "other.ts", "startLine": 7,
                     "edges": [
                        {"symbol": {"name": "default", "filePath": "other.ts", "startLine": 1}, "line": 3, "column": 8},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// https://github.com/microsoft/TypeScript/blob/fed0bf24149fb1ed36039212648bafdafc1ea10e/tsc/internal/fourslash/tests/callHierarchyExportEqualsFunction_test.go
// Upstream ExportEqualsFunction.
// Adapt the upstream anonymous export by naming its function exported for symbol-name queries.
// The original baseline returns a module item with no incoming calls; these function-level
// binding expectations are an explicit extension, not a copy of that module baseline.
// import = require() must reach exported, while baz remains its direct callee, not foo's.
#[tokio::test]
async fn typescript_upstream_export_equals() {
    check_case(Case {
        name: "typescript_upstream_export_equals",
        files: vec![
            SourceFile {
                path: "main.ts",
                source: r#"import bar = require("./other");

function foo() {
    bar();
}
"#,
            },
            SourceFile {
                path: "other.ts",
                source: r"export = function exported() {
    baz();
}

function baz() {
}
",
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "exported",
                expected: json!([
                    {"name": "exported", "filePath": "other.ts", "startLine": 1,
                     "edges": [
                        {"symbol": {"name": "foo", "filePath": "main.ts", "startLine": 3}, "line": 4, "column": 4},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "exported",
                expected: json!([
                    {"name": "exported", "filePath": "other.ts", "startLine": 1,
                     "edges": [
                        {"symbol": {"name": "baz", "filePath": "other.ts", "startLine": 5}, "line": 2, "column": 4},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "foo",
                expected: json!([
                    {"name": "foo", "filePath": "main.ts", "startLine": 3,
                     "edges": [
                        {"symbol": {"name": "exported", "filePath": "other.ts", "startLine": 1}, "line": 4, "column": 4},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "baz",
                expected: json!([
                    {"name": "baz", "filePath": "other.ts", "startLine": 5,
                     "edges": [
                        {"symbol": {"name": "exported", "filePath": "other.ts", "startLine": 1}, "line": 2, "column": 4},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// https://github.com/microsoft/TypeScript/blob/fed0bf24149fb1ed36039212648bafdafc1ea10e/tsc/internal/fourslash/tests/callHierarchyJsxElement_test.go
// Upstream JsxElement.
// Strip the cursor marker and harness directives while preserving source lines in a TSX file.
// Adopt the upstream semantic call from JSX to the component function; no JSX runtime is indexed.
// The component owns its call to baz, which must not appear as a direct callee of foo.
#[tokio::test]
async fn typescript_upstream_jsx() {
    check_case(Case {
        name: "typescript_upstream_jsx",
        files: vec![SourceFile {
            path: "main.tsx",
            source: r"function foo() {
    return <Bar/>;
}

function Bar() {
    baz();
}

function baz() {
}
",
        }],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "Bar",
                expected: json!([
                    {"name": "Bar", "filePath": "main.tsx", "startLine": 5,
                     "edges": [
                        {"symbol": {"name": "foo", "filePath": "main.tsx", "startLine": 1}, "line": 2, "column": 12},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "Bar",
                expected: json!([
                    {"name": "Bar", "filePath": "main.tsx", "startLine": 5,
                     "edges": [
                        {"symbol": {"name": "baz", "filePath": "main.tsx", "startLine": 9}, "line": 6, "column": 4},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "foo",
                expected: json!([
                    {"name": "foo", "filePath": "main.tsx", "startLine": 1,
                     "edges": [
                        {"symbol": {"name": "Bar", "filePath": "main.tsx", "startLine": 5}, "line": 2, "column": 12},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "baz",
                expected: json!([
                    {"name": "baz", "filePath": "main.tsx", "startLine": 9,
                     "edges": [
                        {"symbol": {"name": "Bar", "filePath": "main.tsx", "startLine": 5}, "line": 6, "column": 4},
                    ]},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific C++ declaration/definition regression. Foo.h declares both methods;
// Foo.cc defines them, and a third translation unit calls bar through reference/pointer receivers.
// Canonical symbols and ranges must come from Foo.cc, with no duplicate header-declaration groups.
// Calls within the out-of-line bar definition belong to Foo::bar and resolve this against Foo.
#[tokio::test]
async fn cpp_class_header_and_implementation() {
    check_case(Case {
        name: "cpp_class_header_and_implementation",
        files: vec![
            SourceFile {
                path: "Foo.h",
                source: r"#pragma once
class Foo {
public:
    void bar();
    void baz();
};
",
            },
            SourceFile {
                path: "Foo.cc",
                source: r#"#include "Foo.h"

void Foo::bar() {
    this->baz();
}

void Foo::baz() {}
"#,
            },
            SourceFile {
                path: "caller.cc",
                source: r#"#include "Foo.h"
void caller(Foo& ref, Foo* ptr) {
    ref.bar();
    ptr->bar();
}
"#,
            },
        ],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "bar",
                expected: json!([
                    {"name": "Foo::bar", "filePath": "Foo.cc", "startLine": 3,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "caller.cc", "startLine": 2}, "line": [3, 4], "column": 8},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "Foo::bar",
                expected: json!([
                    {"name": "Foo::bar", "filePath": "Foo.cc", "startLine": 3,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "caller.cc", "startLine": 2}, "line": [3, 4], "column": 8},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([
                    {"name": "caller", "filePath": "caller.cc", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "Foo::bar", "filePath": "Foo.cc", "startLine": 3}, "line": [3, 4], "column": 8},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "Foo::bar",
                expected: json!([
                    {"name": "Foo::bar", "filePath": "Foo.cc", "startLine": 3,
                     "edges": [
                        {"symbol": {"name": "Foo::baz", "filePath": "Foo.cc", "startLine": 7}, "line": 4, "column": 10},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "baz",
                expected: json!([
                    {"name": "Foo::baz", "filePath": "Foo.cc", "startLine": 7,
                     "edges": [
                        {"symbol": {"name": "Foo::bar", "filePath": "Foo.cc", "startLine": 3}, "line": 4, "column": 10},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "Foo::baz",
                expected: json!([
                    {"name": "Foo::baz", "filePath": "Foo.cc", "startLine": 7,
                     "edges": []},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific C++ template regression. Explicit type arguments select one source template
// definition. Different instantiations must not create separate symbol groups, while repeated
// calls to the same instantiation retain their individual source sites.
#[tokio::test]
async fn cpp_template_explicit_arguments() {
    check_case(Case {
        name: "cpp_template_explicit_arguments",
        files: vec![SourceFile {
            path: "explicit.cc",
            source: r"template <typename T> void work(T value) {}
void caller() {
    work<int>(1);
    work<double>(2.0);
    work<int>(3);
}
",
        }],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "work",
                expected: json!([
                    {"name": "work", "filePath": "explicit.cc", "startLine": 1,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "explicit.cc", "startLine": 2}, "line": [3, 4, 5], "column": 4},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([
                    {"name": "caller", "filePath": "explicit.cc", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "work", "filePath": "explicit.cc", "startLine": 1}, "line": [3, 4, 5], "column": 4},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "work",
                expected: json!([
                    {"name": "work", "filePath": "explicit.cc", "startLine": 1,
                     "edges": []},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific C++ template regression. Calls with int, double and char arguments deduce T
// without explicit angle brackets. All three calls refer to the same source template definition;
// parameter and argument types are not additional call targets.
#[tokio::test]
async fn cpp_template_deduced_arguments() {
    check_case(Case {
        name: "cpp_template_deduced_arguments",
        files: vec![SourceFile {
            path: "deduced.cc",
            source: r"template <typename T> void work(T value) {}
void caller() {
    work(1);
    work(2.0);
    work('a');
}
",
        }],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "work",
                expected: json!([
                    {"name": "work", "filePath": "deduced.cc", "startLine": 1,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "deduced.cc", "startLine": 2}, "line": [3, 4, 5], "column": 4},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([
                    {"name": "caller", "filePath": "deduced.cc", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "work", "filePath": "deduced.cc", "startLine": 1}, "line": [3, 4, 5], "column": 4},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "work",
                expected: json!([
                    {"name": "work", "filePath": "deduced.cc", "startLine": 1,
                     "edges": []},
                ]),
            },
        ],
    })
    .await;
}

// zvec-grep-specific C++ template regression. Non-dependent helper calls belong to the source
// template, not its callers. Two instantiations must not multiply the template's two callsites;
// caller's direct callees remain work alone. Dependent lookup/overload selection is outside this case.
#[tokio::test]
async fn cpp_template_body_call_ownership() {
    check_case(Case {
        name: "cpp_template_body_call_ownership",
        files: vec![SourceFile {
            path: "body.cc",
            source: r"void helper() {}
template <typename T> void work(T value) {
    helper();
    helper();
}
void caller() {
    work<int>(1);
    work(2.0);
}
",
        }],
        queries: vec![
            Query {
                operation: "callers",
                symbol: "work",
                expected: json!([
                    {"name": "work", "filePath": "body.cc", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "caller", "filePath": "body.cc", "startLine": 6}, "line": [7, 8], "column": 4},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([
                    {"name": "caller", "filePath": "body.cc", "startLine": 6,
                     "edges": [
                        {"symbol": {"name": "work", "filePath": "body.cc", "startLine": 2}, "line": [7, 8], "column": 4},
                    ]},
                ]),
            },
            Query {
                operation: "callees",
                symbol: "work",
                expected: json!([
                    {"name": "work", "filePath": "body.cc", "startLine": 2,
                     "edges": [
                        {"symbol": {"name": "helper", "filePath": "body.cc", "startLine": 1}, "line": [3, 4], "column": 4},
                    ]},
                ]),
            },
            Query {
                operation: "callers",
                symbol: "helper",
                expected: json!([
                    {"name": "helper", "filePath": "body.cc", "startLine": 1,
                     "edges": [
                        {"symbol": {"name": "work", "filePath": "body.cc", "startLine": 2}, "line": [3, 4], "column": 4},
                    ]},
                ]),
            },
        ],
    })
    .await;
}
