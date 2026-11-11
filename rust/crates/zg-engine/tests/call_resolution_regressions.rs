//! Regression checks for call targets across scopes, assignments and imports.
#[allow(dead_code)]
mod support;

use serde_json::{Value, json};
use std::fs;
use support::{EmbeddingServer, configure_remote_model, index_options};
use zg_engine::{ZvecGrep, api::relationships::RelationshipOptions};

#[tokio::test]
async fn go_narrowed_receiver_does_not_escape_its_branch_or_loop() {
    check_case(Case {
        name: "go_narrowed_receiver_does_not_escape_its_branch_or_loop",
        files: vec![SourceFile {
            path: "scope.go",
            source: "package scope\n\
                     type Left struct{}\n\
                     func (v Left) Work() {}\n\
                     type Right struct{}\n\
                     func (v Right) Work() {}\n\
                     func run(v Right, x any, things []Left) {\n\
                         v.Work()\n\
                         for _, v := range things { v.Work() }\n\
                         v.Work()\n\
                         switch v := x.(type) {\n\
                         case Left: v.Work()\n\
                         case Right: v.Work()\n\
                         }\n\
                         v.Work()\n\
                     }\n",
        }],
        queries: vec![Query {
            operation: "callees",
            symbol: "run",
            expected: json!([{"name": "run", "edges": [
                {"symbol": {"name": "Right::Work"}, "line": 7},
                {"symbol": {"name": "Left::Work"}, "line": 8},
                {"symbol": {"name": "Right::Work"}, "line": 9},
                {"symbol": {"name": "Left::Work"}, "line": 11},
                {"symbol": {"name": "Right::Work"}, "line": 12},
                {"symbol": {"name": "Right::Work"}, "line": 14}
            ]}]),
            ..Query::default()
        }],
    })
    .await;
}

#[tokio::test]
async fn jsx_respects_intrinsic_elements_and_parameter_shadowing() {
    check_case(Case {
        name: "jsx_respects_intrinsic_elements_and_parameter_shadowing",
        files: vec![SourceFile {
            path: "components.tsx",
            source: "function Button() { return null; }\nfunction button() { return null; }\nfunction render() { return <><button/><Button/></>; }\nfunction shadow(Button: () => any) { return <Button/>; }\n",
        }],
        queries: vec![
            Query { operation: "callees", symbol: "render", expected: json!([
                {"name": "render", "edges": [{"symbol": {"name": "Button"}}]}
            ]), ..Query::default() },
            Query { operation: "callees", symbol: "shadow", expected: json!([
                {"name": "shadow", "edges": []}
            ]), ..Query::default() },
            Query { operation: "callers", symbol: "button", expected: json!([
                {"name": "button", "edges": []}
            ]), ..Query::default() },
        ],
    }).await;
}

#[tokio::test]
async fn default_object_import_does_not_bind_properties_overwritten_by_unknown_spread() {
    check_case(Case {
        name: "default_object_import_does_not_bind_properties_overwritten_by_unknown_spread",
        files: vec![
            SourceFile { path: "api.ts", source: "export function original() {}\ndeclare const unknown: Record<string, () => void>;\nconst api = { run: original, ...unknown };\nexport default api;\n" },
            SourceFile { path: "use.ts", source: "import facade from './api';\nexport function invoke() { facade.run(); }\n" },
        ],
        queries: vec![Query { operation: "callees", symbol: "invoke", expected: json!([
            {"name": "invoke", "edges": []}
        ]), ..Query::default() }],
    }).await;
}

#[tokio::test]
async fn javascript_global_receivers_do_not_cross_module_or_parameter_boundaries() {
    check_case(Case {
        name: "javascript_global_receivers_do_not_cross_module_or_parameter_boundaries",
        files: vec![
            SourceFile { path: "global.js", source: "class Shared { run() {} }\n" },
            SourceFile { path: "module.js", source: "export class Shared { run() {} }\n" },
            SourceFile { path: "caller.js", source: "function invoke() { const x = new Shared(); x.run(); }\n" },
            SourceFile { path: "module_user.js", source: "export function moduleCaller() { const x = new Shared(); x.run(); }\n" },
            SourceFile { path: "cjs_user.js", source: "const external = require('external');\nfunction cjsCaller() { const x = new Shared(); x.run(); }\n" },
            SourceFile { path: "shadow.js", source: "function shadow(Shared) { const x = new Shared(); x.run(); }\n" },
            SourceFile { path: "commonjs.js", source: "class Private { run() {} }\nmodule.exports = Private;\n" },
            SourceFile { path: "private_user.js", source: "function noLeak() { const x = new Private(); x.run(); }\n" },
        ],
        queries: vec![
            Query { operation: "callers", symbol: "Shared::run", file: Some("global.js"), expected: json!([
                {"name": "Shared::run", "edges": [{"symbol": {"name": "invoke"}}]}
            ]) },
            Query { operation: "callers", symbol: "Shared::run", file: Some("module.js"), expected: json!([
                {"name": "Shared::run", "edges": []}
            ]) },
            Query { operation: "callers", symbol: "Private::run", expected: json!([
                {"name": "Private::run", "edges": []}
            ]), ..Query::default() },
        ],
    }).await;
}

#[tokio::test]
async fn javascript_static_initializer_keeps_nested_calls_and_repeated_sites() {
    check_case(Case {
        name: "javascript_static_initializer_keeps_nested_calls_and_repeated_sites",
        files: vec![SourceFile {
            path: "static.js",
            source: "class C {\n  static {\n    function nested() { target(); target(); }\n    nested();\n  }\n}\nfunction target() {}\n",
        }],
        queries: vec![
            Query { operation: "callees", symbol: "C::static {}", expected: json!([
                {"name": "C::static {}", "edges": [{"symbol": {"name": "C::nested"}, "line": 4}]}
            ]), ..Query::default() },
            Query { operation: "callers", symbol: "target", expected: json!([
                {"name": "target", "edges": [
                    {"symbol": {"name": "C::nested"}, "line": 3},
                    {"symbol": {"name": "C::nested"}, "line": 3}
                ]}
            ]), ..Query::default() },
        ],
    }).await;
}

struct SourceFile {
    path: &'static str,
    source: &'static str,
}

struct Case {
    name: &'static str,
    files: Vec<SourceFile>,
    queries: Vec<Query>,
}

#[derive(Default)]
struct Query {
    operation: &'static str,
    symbol: &'static str,
    file: Option<&'static str>,
    expected: Value,
}

async fn check_case(case: Case) {
    let directory = tempfile::tempdir().expect("source workspace");
    let root = directory.path().canonicalize().expect("canonical root");
    let server = EmbeddingServer::start().expect("local embedding server");
    configure_remote_model(&root, server.address).expect("embedding configuration");
    for file in &case.files {
        let path = root.join(file.path);
        fs::create_dir_all(path.parent().expect("source parent")).expect("source directory");
        fs::write(path, file.source).expect("write source");
    }
    let engine = ZvecGrep::new();
    let mut options = index_options(&root);
    options.graph_indexing = Some(true);
    options.rebuild = true;
    let indexed = engine.index(options).await;
    engine.close();
    let indexed = indexed.expect("index sources");
    assert_eq!(indexed.files_failed, 0, "{}: {indexed:?}", case.name);

    // Query a reopened store so the assertions cover persisted relationships.
    let engine = ZvecGrep::new();
    let mut errors = Vec::new();
    for query in case.queries {
        let options = RelationshipOptions {
            root: Some(root.clone()),
            symbol: query.symbol.into(),
            limit: Some(50),
            globs: query.file.into_iter().map(Into::into).collect(),
            ..RelationshipOptions::default()
        };
        let result = match query.operation {
            "callers" => engine.callers(options).await,
            "callees" => engine.callees(options).await,
            _ => panic!("unknown query operation"),
        }
        .expect("query relationships");
        assert!(result.matches.iter().all(|group| !group.truncated));
        let actual = serde_json::to_value(result.matches).expect("serialize matches");
        if !matches_expected(&actual, &query.expected) {
            errors.push(format!(
                "{} {}: expected {}, actual {actual}",
                query.operation, query.symbol, query.expected
            ));
        }
    }
    engine.close();
    assert!(errors.is_empty(), "{}: {}", case.name, errors.join("\n"));
}

// Compare the declared fields without imposing result order. Consume each
// actual item once, preserving exact definition and call-site counts.
fn matches_expected(actual: &Value, expected: &Value) -> bool {
    match (actual, expected) {
        (Value::Object(actual), Value::Object(expected)) => expected.iter().all(|(key, wanted)| {
            actual
                .get(key)
                .is_some_and(|value| matches_expected(value, wanted))
        }),
        (Value::Array(actual), Value::Array(expected)) => {
            if actual.len() != expected.len() {
                return false;
            }
            let mut used = vec![false; actual.len()];
            expected.iter().all(|wanted| {
                let Some(index) = actual
                    .iter()
                    .enumerate()
                    .position(|(index, value)| !used[index] && matches_expected(value, wanted))
                else {
                    return false;
                };
                used[index] = true;
                true
            })
        }
        _ => actual == expected,
    }
}

#[tokio::test]
async fn export_alias_is_not_local() {
    check_case(Case { name: "export_alias_is_not_local", files: vec![
        SourceFile { path: "a.js", source: "function first() {}\nfunction second() {}\nexport { first as second };\nfunction caller() { second(); }\nlet selected = first;\nexport { selected as live };\nexport default selected;\nselected = second;\n" },
        SourceFile { path: "b.js", source: "import copied, { second, live } from './a';\nfunction imported() { second(); }\nfunction copiedCaller() { copied(); }\nfunction liveCaller() { live(); }\n" },
    ], queries: vec![
        Query { operation: "callees", symbol: "caller", expected: json!([{"name": "caller", "edges": [{"symbol": {"name": "second"}}]}]), ..Query::default() },
        Query { operation: "callees", symbol: "imported", expected: json!([{"name": "imported", "edges": [{"symbol": {"name": "first"}}]}]), ..Query::default() },
        Query { operation: "callees", symbol: "copiedCaller", expected: json!([{"name": "copiedCaller", "edges": [{"symbol": {"name": "first"}}]}]), ..Query::default() },
        Query { operation: "callees", symbol: "liveCaller", expected: json!([{"name": "liveCaller", "edges": [{"symbol": {"name": "second"}}]}]), ..Query::default() },
    ] }).await;
}

#[tokio::test]
async fn property_reassignment() {
    for path in ["a.js", "a.ts"] {
        check_case(Case { name: "property_reassignment", files: vec![
        SourceFile { path, source: "function original() {}\nfunction replacement() {}\nconst api = { run: original };\napi.run = replacement;\nfunction invoke() { api.run(); }\n" },
    ], queries: vec![
        Query { operation: "callees", symbol: "invoke", expected: json!([{"name": "invoke", "edges": [{"symbol": {"name": "replacement"}}]}]), ..Query::default() },
    ] }).await;
    }
}

#[tokio::test]
async fn initializer_position() {
    check_case(Case { name: "initializer_position", files: vec![
        SourceFile { path: "a.js", source: "function first() {}\nfunction second() {}\nlet cb = first;\nconst result = cb();\ncb = second;\n" },
    ], queries: vec![
        Query { operation: "callees", symbol: "result", expected: json!([{"name": "result", "edges": [{"symbol": {"name": "first"}}]}]), ..Query::default() },
    ] }).await;
}

#[tokio::test]
async fn nested_function_shadows_file_alias() {
    check_case(Case { name: "nested_function_shadows_file_alias", files: vec![
        SourceFile { path: "a.js", source: "function target() {}\nconst work = target;\nfunction outer() { function work() {} work(); }\n" },
    ], queries: vec![
        Query { operation: "callees", symbol: "outer", expected: json!([{"name": "outer", "edges": [{"symbol": {"name": "outer::work"}}]}]), ..Query::default() },
    ] }).await;
}

#[tokio::test]
async fn alias_of_shadowed_parameter() {
    check_case(Case { name: "alias_of_shadowed_parameter", files: vec![
        SourceFile { path: "a.ts", source: "function target() {}\nfunction caller(target: () => void) { const alias = target; alias(); }\nfunction later() { const api = { target }; const target = 0; api.target(); }\n" },
    ], queries: vec![
        Query { operation: "callees", symbol: "caller", expected: json!([{"name": "caller", "edges": []}]), ..Query::default() },
        Query { operation: "callees", symbol: "later", expected: json!([{"name": "later", "edges": []}]), ..Query::default() },
    ] }).await;
}

#[tokio::test]
async fn captured_parameters_shadow_imports() {
    check_case(Case { name: "captured_parameters_shadow_imports", files: vec![
        SourceFile { path: "lib.js", source: "export default function action() {}\nexport function run() {}\n" },
        SourceFile { path: "a.js", source: "import action from './lib';\nimport * as facade from './lib';\nfunction outer(action) { function inner() { action(); } }\nfunction other(facade) { function inner() { facade.run(); } }\n" },
    ], queries: vec![
        Query { operation: "callees", symbol: "outer::inner", expected: json!([{"name": "outer::inner", "edges": []}]), ..Query::default() },
        Query { operation: "callees", symbol: "other::inner", expected: json!([{"name": "other::inner", "edges": []}]), ..Query::default() },
    ] }).await;
}

#[tokio::test]
async fn enclosing_function_shadows_import() {
    check_case(Case { name: "enclosing_function_shadows_import", files: vec![
        SourceFile { path: "lib.js", source: "export function work() {}\n" },
        SourceFile { path: "a.js", source: "import { work } from './lib';\nfunction outer() { function work() {} function inner() { work(); } }\n" },
    ], queries: vec![
        Query { operation: "callees", symbol: "outer::inner", expected: json!([{"name": "outer::inner", "edges": [{"symbol": {"name": "outer::work"}}]}]), ..Query::default() },
    ] }).await;
}

#[tokio::test]
async fn copied_alias_keeps_value() {
    check_case(Case { name: "copied_alias_keeps_value", files: vec![
        SourceFile { path: "a.js", source: "function first() {}\nfunction second() {}\nfunction invoke() { let fn = first; const api = { run: fn }; fn = second; api.run(); }\nlet value = first;\nconst copy = value;\nconst again = copy;\nvalue = second;\nfunction copied() { again(); }\n" },
    ], queries: vec![
        Query { operation: "callees", symbol: "invoke", expected: json!([{"name": "invoke", "edges": [{"symbol": {"name": "first"}}]}]), ..Query::default() },
        Query { operation: "callees", symbol: "copied", expected: json!([{"name": "copied", "edges": [{"symbol": {"name": "first"}}]}]), ..Query::default() },
    ] }).await;
}

#[tokio::test]
async fn class_expression_scope() {
    check_case(Case { name: "class_expression_scope", files: vec![
        SourceFile { path: "a.js", source: "function target() {}\nconst C = class { method(target) {} };\nfunction caller() { target(); }\n" },
    ], queries: vec![
        Query { operation: "callees", symbol: "caller", expected: json!([{"name": "caller", "edges": [{"symbol": {"name": "target"}}]}]), ..Query::default() },
    ] }).await;
}

#[tokio::test]
async fn go_named_scalar_receivers() {
    check_case(Case { name: "go_named_scalar_receivers", files: vec![
        SourceFile { path: "go.mod", source: "module example.com/project\n\ngo 1.23\n" },
        SourceFile { path: "count/count.go", source: "package count\ntype Count int\nfunc (c Count) Work() {}\nfunc local(c Count) { c.Work() }\n" },
        SourceFile { path: "app/app.go", source: "package app\nimport \"example.com/project/count\"\nfunc invoke(c count.Count) { c.Work() }\n" },
    ], queries: vec![
        Query { operation: "callees", symbol: "local", expected: json!([{"name": "local", "edges": [{"symbol": {"name": "Count::Work"}}]}]), ..Query::default() },
        Query { operation: "callees", symbol: "invoke", expected: json!([{"name": "invoke", "edges": [{"symbol": {"name": "Count::Work"}}]}]), ..Query::default() },
    ] }).await;
}

#[tokio::test]
async fn require_binding() {
    check_case(Case {
        name: "require_binding",
        files: vec![
            SourceFile {
                path: "api.js",
                source: "export function work() {}\n",
            },
            SourceFile {
                path: "a.js",
                source: "const api = require('./api');\nfunction invoke() { api.work(); }\n",
            },
        ],
        queries: vec![Query {
            operation: "callees",
            symbol: "invoke",
            expected: json!([{"name": "invoke", "edges": [{"symbol": {"name": "work"}}]}]),
            ..Query::default()
        }],
    })
    .await;
}

#[tokio::test]
async fn go_grouped_import() {
    check_case(Case { name: "go_grouped_import", files: vec![
        SourceFile { path: "go.mod", source: "module example.com/project\n\ngo 1.23\n" },
        SourceFile { path: "service/service.go", source: "package service\nfunc Work() {}\ntype Client struct{}\nfunc (c Client) Run() {}\n" },
        SourceFile { path: "app/app.go", source: "package app\nimport (\n \"example.com/project/service\"\n)\nfunc invoke(c service.Client) { service.Work(); c.Run() }\n" },
    ], queries: vec![
        Query { operation: "callees", symbol: "invoke", expected: json!([{"name": "invoke", "edges": [{"symbol": {"name": "Work"}}, {"symbol": {"name": "Client::Run"}}]}]), ..Query::default() },
    ] }).await;
}
