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
async fn typed_file_initializers_do_not_use_later_receiver_assignments() {
    check_case(Case { name: "typed_initializer_position", files: vec![
        SourceFile { path: "a.ts", source: "class First { run() {} }\nclass Second { run() {} }\nlet receiver = new First();\nconst result = receiver.run();\nreceiver = new Second();\nfunction deferred() { receiver.run(); }\n" },
    ], queries: vec![
        Query { operation: "callees", symbol: "result", expected: json!([{"name": "result", "edges": [{"symbol": {"name": "First::run"}}]}]), ..Query::default() },
        Query { operation: "callees", symbol: "deferred", expected: json!([{"name": "deferred", "edges": [{"symbol": {"name": "Second::run"}}]}]), ..Query::default() },
    ] }).await;
}

#[tokio::test]
async fn lexical_bindings_end_with_their_block_but_var_stays_in_the_function() {
    for path in ["scope.js", "scope.ts"] {
        check_case(Case { name: "lexical_binding_scope", files: vec![
            SourceFile { path, source: "function target() {}\nfunction other() {}\nfunction caller() { { const target = 0; } target(); }\nfunction restored() { let target = other; { let target = 0; } target(); }\nfunction hoisted() { target(); { var target = 0; } target(); }\nfunction loop() { for (let target = 0; target < 1; target++) {} target(); }\n" },
        ], queries: vec![
            Query { operation: "callees", symbol: "caller", expected: json!([{"name":"caller","edges":[{"symbol":{"name":"target"}}]}]), ..Query::default() },
            Query { operation: "callees", symbol: "restored", expected: json!([{"name":"restored","edges":[{"symbol":{"name":"other"}}]}]), ..Query::default() },
            Query { operation: "callees", symbol: "hoisted", expected: json!([{"name":"hoisted","edges":[]}]), ..Query::default() },
            Query { operation: "callees", symbol: "loop", expected: json!([{"name":"loop","edges":[{"symbol":{"name":"target"}}]}]), ..Query::default() },
        ] }).await;
    }
}

#[tokio::test]
async fn receiver_rebinding_replaces_or_invalidates_old_properties() {
    for path in ["rebind.js", "rebind.ts"] {
        check_case(Case { name: "receiver_rebinding", files: vec![
            SourceFile { path, source: "function original() {}\nfunction replacement() {}\nlet api = { run: original };\napi = { run: replacement };\nfunction invoke() { api.run(); }\nfunction removed() { let obj = { run: original }; obj = {}; obj.run(); }\nfunction local() { let obj = { run: original }; obj.run(); obj = { run: replacement }; obj.run(); }\nfunction overwritten() { let obj = { run: original }; obj = {}; obj.run = replacement; obj.run(); }\nfunction replacedMethod() { let obj = { run() { original(); } }; obj = { run() { replacement(); } }; obj.run(); }\n" },
        ], queries: vec![
            Query { operation: "callees", symbol: "invoke", expected: json!([{"name":"invoke","edges":[{"symbol":{"name":"replacement"}}]}]), ..Query::default() },
            Query { operation: "callees", symbol: "replacedMethod", expected: json!([{"name":"replacedMethod","edges":[]}]), ..Query::default() },
            Query { operation: "callees", symbol: "removed", expected: json!([{"name":"removed","edges":[]}]), ..Query::default() },
            Query { operation: "callees", symbol: "local", expected: json!([{"name":"local","edges":[{"symbol":{"name":"original"}},{"symbol":{"name":"replacement"}}]}]), ..Query::default() },
            Query { operation: "callees", symbol: "overwritten", expected: json!([{"name":"overwritten","edges":[{"symbol":{"name":"replacement"}}]}]), ..Query::default() },
        ] }).await;
    }
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
async fn class_and_object_method_parameters_stay_in_their_scope() {
    for source in [
        "function target() {}\nconst C = class { method(target) {} };\nfunction caller() { target(); }\n",
        "function target() {}\nconst obj = { method(target) {} };\nfunction caller() { target(); }\n",
    ] {
        check_case(Case {
            name: "method_parameters_stay_in_their_scope",
            files: vec![SourceFile {
                path: "a.js",
                source,
            }],
            queries: vec![Query {
                operation: "callees",
                symbol: "caller",
                expected: json!([{"name": "caller", "edges": [{"symbol": {"name": "target"}}]}]),
                ..Query::default()
            }],
        })
        .await;
    }
}

#[tokio::test]
async fn go_named_non_struct_receivers() {
    for source in [
        "package count\ntype Count func()\nfunc (c Count) Work() {}\nfunc local(c Count) { c.Work() }\n",
        "package count\ntype Count int\nfunc (c Count) Work() {}\nfunc local(c Count) { c.Work() }\n",
        "package count\ntype Count []int\nfunc (c Count) Work() {}\nfunc local(c Count) { c.Work() }\n",
        "package count\ntype Count map[string]int\nfunc (c Count) Work() {}\nfunc local(c Count) { c.Work() }\n",
    ] {
        check_case(Case { name: "go_named_non_struct_receivers", files: vec![
        SourceFile { path: "go.mod", source: "module example.com/project\n\ngo 1.23\n" },
        SourceFile { path: "count/count.go", source },
        SourceFile { path: "app/app.go", source: "package app\nimport \"example.com/project/count\"\nfunc invoke(c count.Count) { c.Work() }\n" },
    ], queries: vec![
        Query { operation: "callees", symbol: "local", expected: json!([{"name": "local", "edges": [{"symbol": {"name": "Count::Work"}}]}]), ..Query::default() },
        Query { operation: "callees", symbol: "invoke", expected: json!([{"name": "invoke", "edges": [{"symbol": {"name": "Count::Work"}}]}]), ..Query::default() },
    ] }).await;
    }
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
async fn require_destructuring_and_reexports_keep_separate_bindings() {
    for path in ["use.js", "use.ts"] {
        check_case(Case { name: "import_binding_kinds", files: vec![
            SourceFile { path: "api.js", source: "export function work() {}\n" },
            SourceFile { path: "bridge.js", source: "export { work /* name */ as\nrenamed } from './api';\nfunction missing() { renamed(); }\n" },
            SourceFile { path, source: "const { work: alias } = require /* module */ ('./api');\nconst { work: second } = require('./api');\nimport { renamed } from './bridge';\nfunction destructured() { alias(); second(); }\nfunction forwarded() { renamed(); }\n" },
        ], queries: vec![
            Query { operation: "callees", symbol: "destructured", expected: json!([{"name":"destructured","edges":[{"symbol":{"name":"work"}}, {"symbol":{"name":"work"}}]}]), ..Query::default() },
            Query { operation: "callees", symbol: "forwarded", expected: json!([{"name":"forwarded","edges":[{"symbol":{"name":"work"}}]}]), ..Query::default() },
            Query { operation: "callees", symbol: "missing", expected: json!([{"name":"missing","edges":[]}]), ..Query::default() },
        ] }).await;
    }
}

#[tokio::test]
async fn generators_do_not_publish_their_local_aliases_at_file_scope() {
    for path in ["scope.js", "scope.ts"] {
        check_case(Case { name: "generator_binding_scope", files: vec![
            SourceFile { path, source: "function target() {}\nfunction other() {}\nfunction* generator() { var target = other; yield target; }\nconst expression = function* () { var target = other; yield target; };\nfunction invoke() { target(); }\n" },
        ], queries: vec![Query { operation: "callees", symbol: "invoke", expected: json!([{"name":"invoke","edges":[{"symbol":{"name":"target"}}]}]), ..Query::default() }], }).await;
    }
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

#[tokio::test]
async fn go_subdirectory_module_and_major_version_imports() {
    check_case(Case { name: "go_subdirectory_module", files: vec![
        SourceFile { path: "nested/go.mod", source: "module example.com/service/v2\n\ngo 1.23\n" },
        SourceFile { path: "nested/service.go", source: "package service\nfunc Work() {}\ntype Client struct{}\nfunc (c Client) Run() {}\n" },
        SourceFile { path: "nested/app/app.go", source: "package app\nimport \"example.com/service/v2\"\nfunc invoke(c service.Client) { service.Work(); c.Run() }\n" },
    ], queries: vec![Query { operation: "callees", symbol: "invoke", expected: json!([{"name":"invoke","edges":[{"symbol":{"name":"Work"}}, {"symbol":{"name":"Client::Run"}}]}]), ..Query::default() }], }).await;
}

#[tokio::test]
async fn object_initializers_keep_the_container_and_member_call_owners() {
    for path in ["config.js", "config.ts"] {
        check_case(Case { name: "object_initializer_owners", files: vec![
            SourceFile { path, source: "function makeX() {}\nfunction helper() {}\nconst cfg = { x: makeX(), run() { helper(); }, arrow: () => helper() };\n" },
        ], queries: vec![
            Query { operation: "callers", symbol: "makeX", expected: json!([{"name":"makeX","edges":[{"symbol":{"name":"cfg"},"line":3}]}]), ..Query::default() },
            Query { operation: "callees", symbol: "cfg", expected: json!([{"name":"cfg","edges":[{"symbol":{"name":"makeX"}}]}]), ..Query::default() },
            Query { operation: "callers", symbol: "helper", expected: json!([{"name":"helper","edges":[{"symbol":{"name":"cfg::run"}},{"symbol":{"name":"cfg::arrow"}}]}]), ..Query::default() },
        ] }).await;
    }
}

#[tokio::test]
async fn non_relative_type_imports_keep_unique_receiver_targets() {
    for source in [
        "import { Foo } from '@/foo'; export function u(x: Foo) { x.run(); }",
        "import { Foo } from '~/foo'; export function u(x: Foo) { x.run(); }",
        "import { Foo } from '@project/lib'; export function u(x: Foo) { x.run(); }",
        "import { Foo as Imported } from '@project/lib'; export function u(x: Imported) { x.run(); }",
        "import * as types from '@project/lib'; export function u(x: types.Foo) { x.run(); }",
    ] {
        check_case(Case { name: "non_relative_type_imports", files: vec![
            SourceFile { path: "tsconfig.json", source: r#"{"compilerOptions":{"baseUrl":".","paths":{"@/*":["src/*"],"~/*":["src/*"]}}}"# },
            SourceFile { path: "src/package.json", source: r#"{"name":"@project/lib"}"# },
            SourceFile { path: "src/foo.ts", source: "export class Foo { run() {} }" },
            SourceFile { path: "src/bar.ts", source: "export class Bar { run() {} }" },
            SourceFile { path: "unrelated.ts", source: "export class Foo { run() {} }" },
            SourceFile { path: "src/use.ts", source },
        ], queries: vec![
            Query { operation: "callees", symbol: "u", expected: json!([{"name":"u","edges":[{"symbol":{"name":"Foo::run","filePath":std::path::Path::new("src").join("foo.ts")}}]}]), ..Query::default() },
        ] }).await;
    }
}

#[tokio::test]
async fn typescript_config_comments_inheritance_and_invalid_boundaries() {
    for (config, base, expected_file) in [
        (
            r#"{ // generated by tsc --init
          "compilerOptions": {"baseUrl": ".", "paths": {"@/*": ["src/*",],},},
        }"#,
            "{}",
            Some("app/src/foo.ts"),
        ),
        (
            r#"{"extends":"../configs/base"}"#,
            r#"{"compilerOptions":{"paths":{"@/*":["../app/src/*"]}}}"#,
            Some("app/src/foo.ts"),
        ),
        (
            r#"{"extends":"../configs/base.json"}"#,
            r#"{/* inherited */ "compilerOptions":{"baseUrl":"../app", "paths":{"@/*":["src/*"]},},}"#,
            Some("app/src/foo.ts"),
        ),
        (
            r#"{"extends":"../configs/base.json","compilerOptions":{"baseUrl":".","paths":{"@/*":["other/*"]}}}"#,
            r#"{"compilerOptions":{"baseUrl":"../app", "paths":{"@/*":["src/*"]}}}"#,
            Some("app/other/foo.ts"),
        ),
        (
            r#"{"extends":"@tsconfig/node20/tsconfig.json","compilerOptions":{"baseUrl":".","paths":{"@/*":["src/*"]}}}"#,
            "{}",
            Some("app/src/foo.ts"),
        ),
        (
            r#"{"extends":["../configs/base.json"]}"#,
            r#"{"compilerOptions":{"baseUrl":"../app","paths":{"@/*":["src/*"]}}}"#,
            Some("app/src/foo.ts"),
        ),
        (
            r#"{"extends":"@tsconfig/node20/tsconfig.json","compilerOptions":{"paths":{"@/*":["src/*"]}}}"#,
            "{}",
            None,
        ),
        (r#"{"compilerOptions": broken}"#, "{}", None),
        (r#"{"extends":"../configs/base.json"}"#, "broken", None),
        (r#"{"extends":"./tsconfig.json"}"#, "{}", None),
        (r#"{"extends":"./missing.json"}"#, "{}", None),
    ] {
        let edges = expected_file.map_or_else(Vec::new, |path| {
            vec![json!({
                "symbol":{"name":"Foo::run", "filePath":path.split('/').collect::<std::path::PathBuf>()}
            })]
        });
        check_case(Case {
            name: "typescript_config",
            files: vec![
                // Invalid child configs must not silently use this parent mapping.
                SourceFile {
                    path: "tsconfig.json",
                    source: r#"{"compilerOptions":{"paths":{"@/*":["app/other/*"]}}}"#,
                },
                SourceFile {
                    path: "app/tsconfig.json",
                    source: config,
                },
                SourceFile {
                    path: "configs/base.json",
                    source: base,
                },
                SourceFile {
                    path: "app/src/foo.ts",
                    source: "export class Foo { run() {} }",
                },
                SourceFile {
                    path: "app/other/foo.ts",
                    source: "export class Foo { run() {} }",
                },
                SourceFile {
                    path: "app/use.ts",
                    source: "import { Foo } from '@/foo'; export function u(x: Foo) { x.run(); }",
                },
            ],
            queries: vec![Query {
                operation: "callees",
                symbol: "u",
                expected: json!([
                    {"name":"u", "edges":edges}
                ]),
                ..Query::default()
            }],
        })
        .await;
    }
}

#[tokio::test]
async fn ambient_types_keep_receiver_targets() {
    check_case(Case { name: "ambient_types", files: vec![
        SourceFile { path: "globals.d.ts", source: "declare class Foo { run(): void; }" },
        SourceFile { path: "bar.ts", source: "export class Bar { run() {} }" },
        SourceFile { path: "use.ts", source: "export function u(x: Foo) { x.run(); }" },
    ], queries: vec![
        Query { operation: "callees", symbol: "u", expected: json!([{"name":"u","edges":[{"symbol":{"name":"Foo::run","filePath":"globals.d.ts"}}]}]), ..Query::default() },
    ] }).await;
}

#[tokio::test]
async fn type_fallback_does_not_cross_known_module_or_ambiguity_boundaries() {
    for source in [
        "import { Foo } from '@project/lib'; export function u(x: Foo) { x.run(); }",
        "import type { Foo } from 'external-pkg'; export function u(x: Foo) { x.run(); }",
        "import { Foo } from './missing'; export function u(x: Foo) { x.run(); }",
        "export function u(x: Foo) { x.run(); }",
    ] {
        let mut files = vec![
            SourceFile {
                path: "one.ts",
                source: "export class Foo { run() {} }",
            },
            SourceFile {
                path: "use.ts",
                source,
            },
        ];
        if source.contains("@project/lib") {
            files.push(SourceFile {
                path: "package.json",
                source: r#"{"name":"@project/lib"}"#,
            });
            files.push(SourceFile {
                path: "two.ts",
                source: "export class Foo { run() {} }",
            });
        }
        check_case(Case {
            name: "type_fallback_boundaries",
            files,
            queries: vec![Query {
                operation: "callees",
                symbol: "u",
                expected: json!([{"name":"u","edges":[]}]),
                ..Query::default()
            }],
        })
        .await;
    }
    check_case(Case {
        name: "declaration_module_is_not_global",
        files: vec![
            SourceFile {
                path: "types.d.ts",
                source: "declare class Foo { run(): void; } export {};",
            },
            SourceFile {
                path: "use.ts",
                source: "export function u(x: Foo) { x.run(); }",
            },
        ],
        queries: vec![Query {
            operation: "callees",
            symbol: "u",
            expected: json!([{"name":"u","edges":[]}]),
            ..Query::default()
        }],
    })
    .await;
}

#[tokio::test]
async fn java_field_receiver_keeps_its_type_after_a_shadowing_parameter() {
    check_case(Case {
        name: "java_field_receiver_keeps_its_type_after_a_shadowing_parameter",
        files: vec![SourceFile { path: "Use.java", source: "class Left { void work() {} }\nclass Right { void work() {} }\nclass Use {\n void run(Right field) { field.work(); this.field.work(); }\n Left field;\n void bare() { work(); }\n void work() {}\n}\n" }],
        queries: vec![
            Query { operation: "callees", symbol: "run", expected: json!([{"name":"Use::run","edges":[{"symbol":{"name":"Left::work","filePath":"Use.java","startLine":1,"endLine":1},"filePath":"Use.java","line":4,"column":39},{"symbol":{"name":"Right::work","filePath":"Use.java","startLine":2,"endLine":2},"filePath":"Use.java","line":4,"column":25}],"filePath":"Use.java","startLine":4,"endLine":4}]), ..Query::default() },
            Query { operation: "callees", symbol: "bare", expected: json!([{"name":"Use::bare","edges":[{"symbol":{"name":"Use::work","filePath":"Use.java","startLine":7,"endLine":7},"filePath":"Use.java","line":6,"column":15}],"filePath":"Use.java","startLine":6,"endLine":6}]), ..Query::default() },
        ],
    }).await;
}

#[tokio::test]
async fn java_generic_receivers_respect_explicit_imports() {
    check_case(Case {
        name: "java_generic_receivers_respect_explicit_imports",
        files: vec![
            SourceFile { path: "base/Pair.java", source: "package base; public class Pair<T> { public void get() {} }\n" },
            SourceFile { path: "app/Decoys.java", source: "package app; class Pair<T> { void get() {} } class Missing { void get() {} }\n" },
            SourceFile { path: "app/Use.java", source: "package app; import base.Pair; import external.Missing; class Use { void run(Pair<String> pair) { pair.get(); } void blocked(Missing value) { value.get(); } void qualified(app.Pair<String> value) { value.get(); } }\n" },
        ],
        queries: vec![
            Query { operation: "callees", symbol: "run", expected: json!([{"edges":[{"symbol":{"name":"Pair::get","filePath":std::path::Path::new("base").join("Pair.java"),"startLine":1,"endLine":1},"filePath":std::path::Path::new("app").join("Use.java"),"line":1,"column":98}],"name":"Use::run","filePath":std::path::Path::new("app").join("Use.java"),"startLine":1,"endLine":1}]), ..Query::default() },
            Query { operation: "callees", symbol: "qualified", expected: json!([{"edges":[{"symbol":{"name":"Pair::get","filePath":std::path::Path::new("app").join("Decoys.java"),"startLine":1,"endLine":1},"filePath":std::path::Path::new("app").join("Use.java"),"line":1,"column":198}],"name":"Use::qualified","filePath":std::path::Path::new("app").join("Use.java"),"startLine":1,"endLine":1}]), ..Query::default() },
            Query { operation: "callees", symbol: "blocked", expected: json!([{"edges":[],"name":"Use::blocked","filePath":std::path::Path::new("app").join("Use.java"),"startLine":1,"endLine":1}]), ..Query::default() },
        ],
    }).await;
}

#[tokio::test]
async fn rust_self_uses_the_imported_type_across_split_impls() {
    check_case(Case {
        name: "rust_self_uses_the_imported_type_across_split_impls",
        files: vec![
            SourceFile { path: "src/state.rs", source: "pub struct Target;\n" },
            SourceFile { path: "src/apply.rs", source: "use crate::state::Target; impl Target { pub fn reset(&self) {} }\n" },
            SourceFile { path: "src/run.rs", source: "use crate::state::Target; impl Target { pub fn run(&self) { self.reset(); } }\n" },
            SourceFile { path: "src/decoy.rs", source: "pub struct Target; impl Target { pub fn orphan(&self) { self.reset(); } }\n" },
            SourceFile { path: "src/params.rs", source: "struct Left; impl Left { fn work(&self) {} } struct Right; impl Right { fn work(&self) {} } fn use_left(left: &mut Left) { left.work(); }\n" },
        ],
        queries: vec![
            Query { operation: "callees", symbol: "run", expected: json!([{"edges":[{"symbol":{"name":"Target::reset","filePath":std::path::Path::new("src").join("apply.rs"),"startLine":1,"endLine":1},"filePath":std::path::Path::new("src").join("run.rs"),"line":1,"column":60}],"name":"Target::run","filePath":std::path::Path::new("src").join("run.rs"),"startLine":1,"endLine":1}]), ..Query::default() },
            Query { operation: "callees", symbol: "orphan", expected: json!([{"edges":[],"name":"Target::orphan","filePath":std::path::Path::new("src").join("decoy.rs"),"startLine":1,"endLine":1}]), ..Query::default() },
            Query { operation: "callees", symbol: "use_left", expected: json!([{"edges":[{"symbol":{"name":"Left::work","filePath":std::path::Path::new("src").join("params.rs"),"startLine":1,"endLine":1},"filePath":std::path::Path::new("src").join("params.rs"),"line":1,"column":123}],"name":"use_left","filePath":std::path::Path::new("src").join("params.rs"),"startLine":1,"endLine":1}]), ..Query::default() },
        ],
    }).await;
}

#[tokio::test]
async fn java_type_bounds_enum_members_and_loop_bindings_keep_their_scope() {
    check_case(Case {
        name: "java_type_bounds_enum_members_and_loop_bindings_keep_their_scope",
        files: vec![SourceFile { path: "Scopes.java", source: "interface Bound { void work(); }\nclass Left { void work() {} Right[] items() { return null; } }\nclass Right { void work() {} Right[] items() { return null; } }\nenum Phase { ONE; void work() {} }\nclass Use<T extends Bound> {\n T typed; Left value;\n void bounded() { typed.work(); Phase.ONE.work(); }\n void loops(Right[] values) { for (Right value : values) { value.work(); Left local = null; local.work(); } value.work(); }\n void iterable() { for (Right value : value.items()) { value.work(); } }\n}\n" }],
        queries: vec![
            Query { operation: "callees", symbol: "bounded", expected: json!([{"edges":[{"symbol":{"name":"Bound::work","filePath":"Scopes.java","startLine":1,"endLine":1},"filePath":"Scopes.java","line":7,"column":18},{"symbol":{"name":"Phase::work","filePath":"Scopes.java","startLine":4,"endLine":4},"filePath":"Scopes.java","line":7,"column":32}],"name":"Use::bounded","filePath":"Scopes.java","startLine":7,"endLine":7}]), ..Query::default() },
            Query { operation: "callees", symbol: "iterable", expected: json!([{"edges":[{"symbol":{"name":"Left::items","filePath":"Scopes.java","startLine":2,"endLine":2},"filePath":"Scopes.java","line":9,"column":38},{"symbol":{"name":"Right::work","filePath":"Scopes.java","startLine":3,"endLine":3},"filePath":"Scopes.java","line":9,"column":55}],"name":"Use::iterable","filePath":"Scopes.java","startLine":9,"endLine":9}]), ..Query::default() },
            Query { operation: "callees", symbol: "loops", expected: json!([{"edges":[{"symbol":{"name":"Right::work","filePath":"Scopes.java","startLine":3,"endLine":3},"filePath":"Scopes.java","line":8,"column":59},{"symbol":{"name":"Left::work","filePath":"Scopes.java","startLine":2,"endLine":2},"filePath":"Scopes.java","line":8,"column":92},{"symbol":{"name":"Left::work","filePath":"Scopes.java","startLine":2,"endLine":2},"filePath":"Scopes.java","line":8,"column":108}],"name":"Use::loops","filePath":"Scopes.java","startLine":8,"endLine":8}]), ..Query::default() },
        ],
    }).await;
}

#[tokio::test]
async fn array_receivers_do_not_borrow_the_element_types_methods() {
    check_case(Case {
        name: "array_receivers_do_not_borrow_the_element_types_methods",
        files: vec![
            SourceFile { path: "arrays.rs", source: "struct Item; impl Item { fn len(&self) {} } fn slice(items: &[Item]) { items.len(); }\n" },
            SourceFile { path: "Arrays.java", source: "class Item { public Item clone() { return this; } } class Use { void array(Item[] items) { items.clone(); } }\n" },
        ],
        queries: vec![
            Query { operation: "callees", symbol: "slice", expected: json!([{"edges":[],"name":"slice","filePath":"arrays.rs","startLine":1,"endLine":1}]), ..Query::default() },
            Query { operation: "callees", symbol: "array", expected: json!([{"edges":[],"name":"Use::array","filePath":"Arrays.java","startLine":1,"endLine":1}]), ..Query::default() },
        ],
    }).await;
}

#[tokio::test]
async fn java_creation_selects_constructors_without_calling_same_named_methods() {
    check_case(Case {
        name: "java_creation_selects_constructors_without_calling_same_named_methods",
        files: vec![SourceFile { path: "Create.java", source: "class Target {\n Target() {}\n Target(int n) {}\n void Target(String s) {}\n}\nclass Implicit { void Implicit() {} }\nclass Ambiguous { Ambiguous(int n) {} Ambiguous(String s) {} }\nclass Use {\n void zero() { new Target(); }\n void one() { new Target(1); }\n void implicit() { new Implicit(); }\n void ambiguous() { new Ambiguous(1); }\n}\n" }],
        queries: vec![
            Query { operation: "callees", symbol: "zero", expected: json!([
                {"name":"Use::zero", "filePath":"Create.java", "startLine":9, "endLine":9, "edges":[
                    {"filePath":"Create.java", "line":9, "column":19, "symbol":{"name":"Target::Target", "filePath":"Create.java", "startLine":2, "endLine":2}}
                ]}
            ]), ..Query::default() },
            Query { operation: "callees", symbol: "one", expected: json!([
                {"name":"Use::one", "filePath":"Create.java", "startLine":10, "endLine":10, "edges":[
                    {"filePath":"Create.java", "line":10, "column":18, "symbol":{"name":"Target::Target", "filePath":"Create.java", "startLine":3, "endLine":3}}
                ]}
            ]), ..Query::default() },
            Query { operation: "callees", symbol: "implicit", expected: json!([
                {"name":"Use::implicit", "filePath":"Create.java", "startLine":11, "endLine":11, "edges":[
                    {"filePath":"Create.java", "line":11, "column":23, "symbol":{"name":"Implicit", "filePath":"Create.java", "startLine":6, "endLine":6}}
                ]}
            ]), ..Query::default() },
            Query { operation: "callees", symbol: "ambiguous", expected: json!([
                {"name":"Use::ambiguous", "filePath":"Create.java", "startLine":12, "endLine":12, "edges":[]}
            ]), ..Query::default() },
        ],
    }).await;
}

#[tokio::test]
async fn java_this_and_super_calls_select_the_current_and_direct_base_constructors() {
    check_case(Case {
        name: "java_this_and_super_calls_select_the_current_and_direct_base_constructors",
        files: vec![
            SourceFile { path: "base/Base.java", source: "package base;\npublic class Base {\n public Base() {}\n public Base(int n) {}\n}\n" },
            SourceFile { path: "decoy/Base.java", source: "package decoy;\npublic class Base { public Base(int n) {} }\n" },
            SourceFile { path: "app/Derived.java", source: "package app;\nimport base.Base;\nclass Derived extends Base {\n Derived() { this(1); }\n Derived(int n) { super(n); }\n}\n" },
        ],
        queries: vec![
            Query { operation: "callees", symbol: "Derived::Derived", expected: json!([
                {"name":"Derived::Derived", "filePath":std::path::Path::new("app").join("Derived.java"), "startLine":4, "endLine":4, "edges":[
                    {"filePath":std::path::Path::new("app").join("Derived.java"), "line":4, "column":13, "symbol":{"name":"Derived::Derived", "filePath":std::path::Path::new("app").join("Derived.java"), "startLine":5, "endLine":5}}
                ]},
                {"name":"Derived::Derived", "filePath":std::path::Path::new("app").join("Derived.java"), "startLine":5, "endLine":5, "edges":[
                    {"filePath":std::path::Path::new("app").join("Derived.java"), "line":5, "column":18, "symbol":{"name":"Base::Base", "filePath":std::path::Path::new("base").join("Base.java"), "startLine":4, "endLine":4}}
                ]}
            ]), ..Query::default() },
            Query { operation: "callers", symbol: "Base::Base", file: Some("base/Base.java"), expected: json!([
                {"name":"Base::Base", "filePath":std::path::Path::new("base").join("Base.java"), "startLine":3, "endLine":3, "edges":[]},
                {"name":"Base::Base", "filePath":std::path::Path::new("base").join("Base.java"), "startLine":4, "endLine":4, "edges":[
                    {"filePath":std::path::Path::new("app").join("Derived.java"), "line":5, "column":18, "symbol":{"name":"Derived::Derived", "filePath":std::path::Path::new("app").join("Derived.java"), "startLine":5, "endLine":5}}
                ]}
            ]) },
        ],
    }).await;
}

#[tokio::test]
async fn java_wildcard_imports_respect_package_and_explicit_import_precedence() {
    for (source, same_package, target) in [
        (
            "package com.c; import com.a.*; class Use { void u(Foo x) { x.run(); } }",
            false,
            Some("com/a/Foo.java"),
        ),
        (
            "package com.c; import com.a.*; import com.b.*; class Use { void u(Foo x) { x.run(); } }",
            false,
            None,
        ),
        (
            "package com.c; import com.a.*; import com.b.Foo; class Use { void u(Foo x) { x.run(); } }",
            false,
            Some("com/b/Foo.java"),
        ),
        (
            "package com.c; import com.a.*; class Use { void u(Foo x) { x.run(); } }",
            true,
            Some("com/c/Foo.java"),
        ),
        (
            "package com.c; import com.a.*; import missing.Foo; class Use { void u(Foo x) { x.run(); } }",
            false,
            None,
        ),
        (
            "package com.c; import missing.*; class Use { void u(Foo x) { x.run(); } }",
            false,
            None,
        ),
    ] {
        let mut files = vec![
            SourceFile {
                path: "com/a/Foo.java",
                source: "package com.a; public class Foo { public void run() {} }",
            },
            SourceFile {
                path: "com/b/Foo.java",
                source: "package com.b; public class Foo { public void run() {} }",
            },
            SourceFile {
                path: "com/c/Use.java",
                source,
            },
        ];
        if same_package {
            files.push(SourceFile {
                path: "com/c/Foo.java",
                source: "package com.c; public class Foo { public void run() {} }",
            });
        }
        let edges = target.map(|path| json!({
            "filePath":std::path::Path::new("com").join("c").join("Use.java"), "line":1,
            "column":source.find("x.run()").expect("call site"),
            "symbol":{"name":"Foo::run", "filePath":path.split('/').collect::<std::path::PathBuf>(), "startLine":1,"endLine":1}
        })).into_iter().collect::<Vec<_>>();
        check_case(Case {
            name: "java_wildcard_imports_respect_package_and_explicit_import_precedence", files,
            queries: vec![Query { operation:"callees", symbol:"u", expected:json!([
                {"name":"Use::u","filePath":std::path::Path::new("com").join("c").join("Use.java"),"startLine":1,"endLine":1,"edges":edges}
            ]), ..Query::default() }],
        }).await;
    }
}

#[tokio::test]
async fn rust_split_impls_resolve_relative_modules_within_each_cargo_crate() {
    check_case(Case {
        name:"rust_split_impls_resolve_relative_modules_within_each_cargo_crate",
        files:vec![
            SourceFile { path:"Cargo.toml", source:"[workspace]\nmembers = [\"crates/app\", \"crates/other\"]\n" },
            SourceFile { path:"crates/app/Cargo.toml", source:"[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n" },
            SourceFile { path:"crates/app/src/lib.rs", source:"mod store;\n" },
            SourceFile { path:"crates/app/src/store/mod.rs", source:"pub struct Store;\nimpl Store { pub fn read(&self) {} }\npub struct Marker;\nmod extra; mod by_crate; mod by_alias; mod nested; mod self_call;\n" },
            SourceFile { path:"crates/app/src/store/extra.rs", source:"use super::{Store, Marker};\nimpl Store { pub fn super_go(&self) { self.read(); } }\n" },
            SourceFile { path:"crates/app/src/store/by_crate.rs", source:"use crate::store::Store;\nimpl Store { pub fn crate_go(&self) { self.read(); } }\n" },
            SourceFile { path:"crates/app/src/store/by_alias.rs", source:"use super::{Store as Shared, Marker};\nimpl Shared { pub fn alias_go(&self) { self.read(); } }\n" },
            SourceFile { path:"crates/app/src/store/nested.rs", source:"mod deeper;\n" },
            SourceFile { path:"crates/app/src/store/nested/deeper.rs", source:"use super::super::Store;\nimpl Store { pub fn deep_go(&self) { self.read(); } }\n" },
            SourceFile { path:"crates/app/src/store/self_call.rs", source:"mod actual;\nuse self::actual::Store;\nimpl Store { pub fn self_go(&self) { self.read(); } }\n" },
            SourceFile { path:"crates/app/src/store/self_call/actual.rs", source:"pub struct Store;\nimpl Store { pub fn read(&self) {} }\n" },
            SourceFile { path:"crates/other/Cargo.toml", source:"[package]\nname = \"other\"\nversion = \"0.1.0\"\nedition = \"2021\"\n" },
            SourceFile { path:"crates/other/src/lib.rs", source:"mod store; mod extra;\n" },
            SourceFile { path:"crates/other/src/store.rs", source:"pub struct Store;\nimpl Store { pub fn read(&self) {} }\nmod relative;\n" },
            SourceFile { path:"crates/other/src/extra.rs", source:"use crate::store::Store;\nimpl Store { pub fn other_go(&self) { self.read(); } }\n" },
            SourceFile { path:"crates/other/src/store/relative.rs", source:"use super::Store;\nimpl Store { pub fn relative_go(&self) { self.read(); } }\n" },
        ],
        queries:[
            ("super_go","Store::super_go","crates/app/src/store/extra.rs",2,"crates/app/src/store/mod.rs"),
            ("crate_go","Store::crate_go","crates/app/src/store/by_crate.rs",2,"crates/app/src/store/mod.rs"),
            ("alias_go","Shared::alias_go","crates/app/src/store/by_alias.rs",2,"crates/app/src/store/mod.rs"),
            ("deep_go","Store::deep_go","crates/app/src/store/nested/deeper.rs",2,"crates/app/src/store/mod.rs"),
            ("self_go","Store::self_go","crates/app/src/store/self_call.rs",3,"crates/app/src/store/self_call/actual.rs"),
            ("other_go","Store::other_go","crates/other/src/extra.rs",2,"crates/other/src/store.rs"),
            ("relative_go","Store::relative_go","crates/other/src/store/relative.rs",2,"crates/other/src/store.rs"),
        ].into_iter().map(|(query,name,path,line,target)| Query {
            operation:"callees",symbol:query,
            expected:json!([{"name":name,"filePath":path.split('/').collect::<std::path::PathBuf>(),"startLine":line,"endLine":line,"edges":[
                {"filePath":path.split('/').collect::<std::path::PathBuf>(),"line":line,"column":if query=="relative_go" {41} else if query=="alias_go" {39} else if query=="super_go" || query=="crate_go" || query=="other_go" {38} else {37},"symbol":{"name":"Store::read","filePath":target.split('/').collect::<std::path::PathBuf>(),"startLine":2,"endLine":2}}
            ]}]),..Query::default()
        }).collect(),
    }).await;
}
