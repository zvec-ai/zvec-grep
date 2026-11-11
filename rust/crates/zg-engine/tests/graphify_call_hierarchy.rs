//! Source-backed caller/callee regressions adapted from Graphify.
//!
//! Each case links to its pinned upstream source and call-edge assertions.
//! Contains/Excludes preserve required/forbidden edges rather than inventing
//! exhaustive graphs. Positive and negative pairs are checked in both directions.
//! Definitions are identified by file and source line, with no ordering or column
//! requirement. Sources and expected values are inline; Graphify is not executed.
//! Fixtures asserting only Graphify's inference limitations are not correctness
//! oracles. Import-only, metadata-only and unsupported-language tests are excluded.
//! See LICENSE.graphify for attribution and licensing.

#[allow(dead_code)]
#[path = "support/relationship_cases.rs"]
mod relationship_cases;
#[allow(dead_code)]
mod support;

use relationship_cases::{Case, Check, Query, SourceFile, check_case};
use serde_json::json;

// https://github.com/safishamsi/graphify/blob/5eedffa87476a063a05bed96f17275ed63f05c1a/tests/test_python_typed_receiver_calls.py#L41
// test_annotated_parameter_receiver
#[tokio::test]
async fn graphify_python_annotated_parameter_receiver() {
    check_case(Case {
        name: "graphify_python_annotated_parameter_receiver",
        files: vec![
            SourceFile {
                path: "pkg/__init__.py",
                source: "",
            },
            SourceFile {
                path: "pkg/client.py",
                source: "class Client:\n    def send(self):\n        return 1\n",
            },
            SourceFile {
                path: "pkg/use.py",
                source: "from .client import Client\n\ndef run(c: Client):\n    return c.send()\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "run",
                expected: json!([
                    {"filePath": "pkg/use.py", "startLine": 3, "edges": [
                        {"filePath": "pkg/use.py", "symbol": {"name": "send", "filePath": "pkg/client.py", "startLine": 2}},
                    ]},
                ]),
                file: Some("pkg/use.py"),
                short_names: true,
                definition_line: Some(3),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "send",
                expected: json!([
                    {"filePath": "pkg/client.py", "startLine": 2, "edges": [
                        {"filePath": "pkg/use.py", "symbol": {"name": "run", "filePath": "pkg/use.py", "startLine": 3}},
                    ]},
                ]),
                file: Some("pkg/client.py"),
                short_names: true,
                definition_line: Some(2),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/safishamsi/graphify/blob/5eedffa87476a063a05bed96f17275ed63f05c1a/tests/test_python_typed_receiver_calls.py#L77
// test_module_qualified_typing_wrapper
#[tokio::test]
async fn graphify_python_module_qualified_typing_wrapper() {
    check_case(Case {
        name: "graphify_python_module_qualified_typing_wrapper",
        files: vec![
            SourceFile {
                path: "pkg/__init__.py",
                source: "",
            },
            SourceFile {
                path: "pkg/client.py",
                source: "class Client:\n    def send(self):\n        return 1\n",
            },
            SourceFile {
                path: "pkg/use.py",
                source: "import typing\nfrom .client import Client\n\ndef run(c: typing.Optional[Client]):\n    return c.send()\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "run",
                expected: json!([
                    {"filePath": "pkg/use.py", "startLine": 4, "edges": [
                        {"filePath": "pkg/use.py", "symbol": {"name": "send", "filePath": "pkg/client.py", "startLine": 2}},
                    ]},
                ]),
                file: Some("pkg/use.py"),
                short_names: true,
                definition_line: Some(4),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "send",
                expected: json!([
                    {"filePath": "pkg/client.py", "startLine": 2, "edges": [
                        {"filePath": "pkg/use.py", "symbol": {"name": "run", "filePath": "pkg/use.py", "startLine": 4}},
                    ]},
                ]),
                file: Some("pkg/client.py"),
                short_names: true,
                definition_line: Some(2),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/safishamsi/graphify/blob/5eedffa87476a063a05bed96f17275ed63f05c1a/tests/test_python_typed_receiver_calls.py#L52
// test_optional_string_and_union_annotations
#[tokio::test]
async fn graphify_python_optional_string_and_union_annotations() {
    check_case(Case {
        name: "graphify_python_optional_string_and_union_annotations",
        files: vec![
            SourceFile {
                path: "pkg/__init__.py",
                source: "",
            },
            SourceFile {
                path: "pkg/client.py",
                source: "class Client:\n    def send(self):\n        return 1\n",
            },
            SourceFile {
                path: "pkg/use.py",
                source: "from typing import Optional\nfrom .client import Client\n\ndef opt(c: Optional[Client]):\n    return c.send()\n\ndef fwd(c: 'Client'):\n    return c.send()\n\ndef union(c: Client | None = None):\n    return c.send()\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "opt",
                expected: json!([
                    {"filePath": "pkg/use.py", "startLine": 4, "edges": [
                        {"filePath": "pkg/use.py", "symbol": {"name": "send", "filePath": "pkg/client.py", "startLine": 2}},
                    ]},
                ]),
                file: Some("pkg/use.py"),
                short_names: true,
                definition_line: Some(4),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "send",
                expected: json!([
                    {"filePath": "pkg/client.py", "startLine": 2, "edges": [
                        {"filePath": "pkg/use.py", "symbol": {"name": "opt", "filePath": "pkg/use.py", "startLine": 4}},
                        {"filePath": "pkg/use.py", "symbol": {"name": "fwd", "filePath": "pkg/use.py", "startLine": 7}},
                        {"filePath": "pkg/use.py", "symbol": {"name": "union", "filePath": "pkg/use.py", "startLine": 10}},
                    ]},
                ]),
                file: Some("pkg/client.py"),
                short_names: true,
                definition_line: Some(2),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "fwd",
                expected: json!([
                    {"filePath": "pkg/use.py", "startLine": 7, "edges": [
                        {"filePath": "pkg/use.py", "symbol": {"name": "send", "filePath": "pkg/client.py", "startLine": 2}},
                    ]},
                ]),
                file: Some("pkg/use.py"),
                short_names: true,
                definition_line: Some(7),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "union",
                expected: json!([
                    {"filePath": "pkg/use.py", "startLine": 10, "edges": [
                        {"filePath": "pkg/use.py", "symbol": {"name": "send", "filePath": "pkg/client.py", "startLine": 2}},
                    ]},
                ]),
                file: Some("pkg/use.py"),
                short_names: true,
                definition_line: Some(10),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/safishamsi/graphify/blob/5eedffa87476a063a05bed96f17275ed63f05c1a/tests/test_python_typed_receiver_calls.py#L86
// test_same_method_name_resolves_to_annotated_class
#[tokio::test]
async fn graphify_python_same_method_name_resolves_to_annotated_class() {
    check_case(Case {
        name: "graphify_python_same_method_name_resolves_to_annotated_class",
        files: vec![
            SourceFile {
                path: "pkg/__init__.py",
                source: "",
            },
            SourceFile {
                path: "pkg/client.py",
                source: "class Client:\n    def send(self):\n        return 1\n",
            },
            SourceFile {
                path: "pkg/cache.py",
                source: "class Cache:\n    def send(self):\n        return 2\n",
            },
            SourceFile {
                path: "pkg/use.py",
                source: "from .client import Client\n\ndef run(c: Client):\n    return c.send()\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "run",
                expected: json!([
                    {"filePath": "pkg/use.py", "startLine": 3, "edges": [
                        {"filePath": "pkg/use.py", "symbol": {"name": "send", "filePath": "pkg/client.py", "startLine": 2}},
                    ]},
                ]),
                file: Some("pkg/use.py"),
                short_names: true,
                definition_line: Some(3),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "send",
                expected: json!([
                    {"filePath": "pkg/client.py", "startLine": 2, "edges": [
                        {"filePath": "pkg/use.py", "symbol": {"name": "run", "filePath": "pkg/use.py", "startLine": 3}},
                    ]},
                ]),
                file: Some("pkg/client.py"),
                short_names: true,
                definition_line: Some(2),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "run",
                expected: json!([
                    {"filePath": "pkg/use.py", "startLine": 3, "edges": [
                        {"filePath": "pkg/use.py", "symbol": {"name": "send", "filePath": "pkg/cache.py", "startLine": 2}},
                    ]},
                ]),
                file: Some("pkg/use.py"),
                short_names: true,
                definition_line: Some(3),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "send",
                expected: json!([
                    {"filePath": "pkg/cache.py", "startLine": 2, "edges": [
                        {"filePath": "pkg/use.py", "symbol": {"name": "run", "filePath": "pkg/use.py", "startLine": 3}},
                    ]},
                ]),
                file: Some("pkg/cache.py"),
                short_names: true,
                definition_line: Some(2),
                check: Check::Excludes,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/safishamsi/graphify/blob/5eedffa87476a063a05bed96f17275ed63f05c1a/tests/test_python_bare_call_not_enclosing_method.py#L34
// test_classmethod_calling_imported_function_of_same_name
// Identify the decorated method by file/name; its range may begin at @classmethod or def.
#[tokio::test]
async fn graphify_python_classmethod_calling_imported_function_of_same_name() {
    check_case(Case {
        name: "graphify_python_classmethod_calling_imported_function_of_same_name",
        files: vec![
            SourceFile {
                path: "tpl/__init__.py",
                source: "",
            },
            SourceFile {
                path: "tpl/build_api.py",
                source: "def get_build_status(c, b):\n    return 1\n",
            },
            SourceFile {
                path: "tpl/main.py",
                source: "from tpl.build_api import get_build_status\n\nclass AsyncTemplate:\n    @classmethod\n    def get_build_status(cls, b):\n        return get_build_status(None, b)\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "get_build_status",
                expected: json!([
                    {"filePath": "tpl/main.py", "edges": [
                        {"filePath": "tpl/main.py", "symbol": {"name": "get_build_status", "filePath": "tpl/build_api.py", "startLine": 1}},
                    ]},
                ]),
                file: Some("tpl/main.py"),
                short_names: true,
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "get_build_status",
                expected: json!([
                    {"filePath": "tpl/build_api.py", "startLine": 1, "edges": [
                        {"filePath": "tpl/main.py", "symbol": {"name": "get_build_status", "filePath": "tpl/main.py"}},
                    ]},
                ]),
                file: Some("tpl/build_api.py"),
                short_names: true,
                definition_line: Some(1),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "get_build_status",
                expected: json!([
                    {"filePath": "tpl/main.py", "edges": [
                        {"filePath": "tpl/main.py", "symbol": {"name": "get_build_status", "filePath": "tpl/main.py"}},
                    ]},
                ]),
                file: Some("tpl/main.py"),
                short_names: true,
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "get_build_status",
                expected: json!([
                    {"filePath": "tpl/main.py", "edges": [
                        {"filePath": "tpl/main.py", "symbol": {"name": "get_build_status", "filePath": "tpl/main.py"}},
                    ]},
                ]),
                file: Some("tpl/main.py"),
                short_names: true,
                check: Check::Excludes,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/safishamsi/graphify/blob/5eedffa87476a063a05bed96f17275ed63f05c1a/tests/test_python_bare_call_not_enclosing_method.py#L49
// test_plain_method_calling_imported_function_of_same_name
#[tokio::test]
async fn graphify_python_plain_method_calling_imported_function_of_same_name() {
    check_case(Case {
        name: "graphify_python_plain_method_calling_imported_function_of_same_name",
        files: vec![
            SourceFile {
                path: "tpl/__init__.py",
                source: "",
            },
            SourceFile {
                path: "tpl/build_api.py",
                source: "def get_build_status(c, b):\n    return 1\n",
            },
            SourceFile {
                path: "tpl/main.py",
                source: "from tpl.build_api import get_build_status\n\nclass Template:\n    def get_build_status(self, b):\n        return get_build_status(None, b)\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "get_build_status",
                expected: json!([
                    {"filePath": "tpl/main.py", "startLine": 4, "edges": [
                        {"filePath": "tpl/main.py", "symbol": {"name": "get_build_status", "filePath": "tpl/build_api.py", "startLine": 1}},
                    ]},
                ]),
                file: Some("tpl/main.py"),
                short_names: true,
                definition_line: Some(4),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "get_build_status",
                expected: json!([
                    {"filePath": "tpl/build_api.py", "startLine": 1, "edges": [
                        {"filePath": "tpl/main.py", "symbol": {"name": "get_build_status", "filePath": "tpl/main.py", "startLine": 4}},
                    ]},
                ]),
                file: Some("tpl/build_api.py"),
                short_names: true,
                definition_line: Some(1),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "get_build_status",
                expected: json!([
                    {"filePath": "tpl/main.py", "startLine": 4, "edges": [
                        {"filePath": "tpl/main.py", "symbol": {"name": "get_build_status", "filePath": "tpl/main.py", "startLine": 4}},
                    ]},
                ]),
                file: Some("tpl/main.py"),
                short_names: true,
                definition_line: Some(4),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "get_build_status",
                expected: json!([
                    {"filePath": "tpl/main.py", "startLine": 4, "edges": [
                        {"filePath": "tpl/main.py", "symbol": {"name": "get_build_status", "filePath": "tpl/main.py", "startLine": 4}},
                    ]},
                ]),
                file: Some("tpl/main.py"),
                short_names: true,
                definition_line: Some(4),
                check: Check::Excludes,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/safishamsi/graphify/blob/5eedffa87476a063a05bed96f17275ed63f05c1a/tests/test_python_bare_call_not_enclosing_method.py#L63
// test_method_calling_same_file_module_function_of_same_name
#[tokio::test]
async fn graphify_python_method_calling_same_file_module_function_of_same_name() {
    check_case(Case {
        name: "graphify_python_method_calling_same_file_module_function_of_same_name",
        files: vec![
            SourceFile {
                path: "m.py",
                source: "def helper():\n    return 1\n\nclass C:\n    def helper(self):\n        return helper()\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "helper",
                expected: json!([
                    {"filePath": "m.py", "startLine": 5, "edges": [
                        {"filePath": "m.py", "symbol": {"name": "helper", "filePath": "m.py", "startLine": 1}},
                    ]},
                ]),
                file: Some("m.py"),
                short_names: true,
                definition_line: Some(5),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "helper",
                expected: json!([
                    {"filePath": "m.py", "startLine": 1, "edges": [
                        {"filePath": "m.py", "symbol": {"name": "helper", "filePath": "m.py", "startLine": 5}},
                    ]},
                ]),
                file: Some("m.py"),
                short_names: true,
                definition_line: Some(1),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "helper",
                expected: json!([
                    {"filePath": "m.py", "startLine": 5, "edges": [
                        {"filePath": "m.py", "symbol": {"name": "helper", "filePath": "m.py", "startLine": 5}},
                    ]},
                ]),
                file: Some("m.py"),
                short_names: true,
                definition_line: Some(5),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "helper",
                expected: json!([
                    {"filePath": "m.py", "startLine": 5, "edges": [
                        {"filePath": "m.py", "symbol": {"name": "helper", "filePath": "m.py", "startLine": 5}},
                    ]},
                ]),
                file: Some("m.py"),
                short_names: true,
                definition_line: Some(5),
                check: Check::Excludes,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/safishamsi/graphify/blob/5eedffa87476a063a05bed96f17275ed63f05c1a/tests/test_python_bare_call_not_enclosing_method.py#L75
// test_nested_function_in_method_skips_the_method
#[tokio::test]
async fn graphify_python_nested_function_in_method_skips_the_method() {
    check_case(Case {
        name: "graphify_python_nested_function_in_method_skips_the_method",
        files: vec![
            SourceFile {
                path: "m.py",
                source: "def helper():\n    return 1\n\nclass C:\n    def helper(self):\n        def inner():\n            return helper()\n        return inner()\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "inner",
                expected: json!([
                    {"filePath": "m.py", "startLine": 6, "edges": [
                        {"filePath": "m.py", "symbol": {"name": "helper", "filePath": "m.py", "startLine": 1}},
                    ]},
                ]),
                file: Some("m.py"),
                short_names: true,
                definition_line: Some(6),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "helper",
                expected: json!([
                    {"filePath": "m.py", "startLine": 1, "edges": [
                        {"filePath": "m.py", "symbol": {"name": "inner", "filePath": "m.py", "startLine": 6}},
                    ]},
                ]),
                file: Some("m.py"),
                short_names: true,
                definition_line: Some(1),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "inner",
                expected: json!([
                    {"filePath": "m.py", "startLine": 6, "edges": [
                        {"filePath": "m.py", "symbol": {"name": "helper", "filePath": "m.py", "startLine": 5}},
                    ]},
                ]),
                file: Some("m.py"),
                short_names: true,
                definition_line: Some(6),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "helper",
                expected: json!([
                    {"filePath": "m.py", "startLine": 5, "edges": [
                        {"filePath": "m.py", "symbol": {"name": "inner", "filePath": "m.py", "startLine": 6}},
                    ]},
                ]),
                file: Some("m.py"),
                short_names: true,
                definition_line: Some(5),
                check: Check::Excludes,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/safishamsi/graphify/blob/5eedffa87476a063a05bed96f17275ed63f05c1a/tests/test_python_bare_call_not_enclosing_method.py#L90
// test_module_function_recursion_still_links_to_itself
#[tokio::test]
async fn graphify_python_module_function_recursion_still_links_to_itself() {
    check_case(Case {
        name: "graphify_python_module_function_recursion_still_links_to_itself",
        files: vec![
            SourceFile {
                path: "m.py",
                source: "def fact(n):\n    return 1 if n < 2 else n * fact(n - 1)\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "fact",
                expected: json!([
                    {"filePath": "m.py", "startLine": 1, "edges": [
                        {"filePath": "m.py", "symbol": {"name": "fact", "filePath": "m.py", "startLine": 1}},
                    ]},
                ]),
                file: Some("m.py"),
                short_names: true,
                definition_line: Some(1),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "fact",
                expected: json!([
                    {"filePath": "m.py", "startLine": 1, "edges": [
                        {"filePath": "m.py", "symbol": {"name": "fact", "filePath": "m.py", "startLine": 1}},
                    ]},
                ]),
                file: Some("m.py"),
                short_names: true,
                definition_line: Some(1),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/safishamsi/graphify/blob/5eedffa87476a063a05bed96f17275ed63f05c1a/tests/test_python_bare_call_not_enclosing_method.py#L97
// test_self_method_call_still_resolves
#[tokio::test]
async fn graphify_python_self_method_call_still_resolves() {
    check_case(Case {
        name: "graphify_python_self_method_call_still_resolves",
        files: vec![
            SourceFile {
                path: "m.py",
                source: "class C:\n    def a(self):\n        return self.b()\n    def b(self):\n        return 1\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "a",
                expected: json!([
                    {"filePath": "m.py", "startLine": 2, "edges": [
                        {"filePath": "m.py", "symbol": {"name": "b", "filePath": "m.py", "startLine": 4}},
                    ]},
                ]),
                file: Some("m.py"),
                short_names: true,
                definition_line: Some(2),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "b",
                expected: json!([
                    {"filePath": "m.py", "startLine": 4, "edges": [
                        {"filePath": "m.py", "symbol": {"name": "a", "filePath": "m.py", "startLine": 2}},
                    ]},
                ]),
                file: Some("m.py"),
                short_names: true,
                definition_line: Some(4),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/safishamsi/graphify/blob/5eedffa87476a063a05bed96f17275ed63f05c1a/tests/test_go_receiver_member_calls.py#L44
// test_a_typed_parameter_types_the_receiver
#[tokio::test]
async fn graphify_go_a_typed_parameter_types_the_receiver() {
    check_case(Case {
        name: "graphify_go_a_typed_parameter_types_the_receiver",
        files: vec![
            SourceFile {
                path: "svc/greeter.go",
                source: "package svc\n\ntype Greeter struct{}\n\nfunc (g *Greeter) Greet() {}\n",
            },
            SourceFile {
                path: "svc/app.go",
                source: "package svc\n\nfunc Run(g *Greeter) { g.Greet() }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Run",
                expected: json!([
                    {"filePath": "svc/app.go", "startLine": 3, "edges": [
                        {"filePath": "svc/app.go", "symbol": {"name": "Greet", "filePath": "svc/greeter.go", "startLine": 5}},
                    ]},
                ]),
                file: Some("svc/app.go"),
                short_names: true,
                definition_line: Some(3),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "Greet",
                expected: json!([
                    {"filePath": "svc/greeter.go", "startLine": 5, "edges": [
                        {"filePath": "svc/app.go", "symbol": {"name": "Run", "filePath": "svc/app.go", "startLine": 3}},
                    ]},
                ]),
                file: Some("svc/greeter.go"),
                short_names: true,
                definition_line: Some(5),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/safishamsi/graphify/blob/5eedffa87476a063a05bed96f17275ed63f05c1a/tests/test_go_receiver_member_calls.py#L66
// test_a_var_declaration_types_the_receiver
#[tokio::test]
async fn graphify_go_a_var_declaration_types_the_receiver() {
    check_case(Case {
        name: "graphify_go_a_var_declaration_types_the_receiver",
        files: vec![
            SourceFile {
                path: "svc/greeter.go",
                source: "package svc\n\ntype Greeter struct{}\n\nfunc (g *Greeter) Greet() {}\n",
            },
            SourceFile {
                path: "svc/app.go",
                source: "package svc\n\nfunc Run() {\n\tvar g Greeter\n\tg.Greet()\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Run",
                expected: json!([
                    {"filePath": "svc/app.go", "startLine": 3, "edges": [
                        {"filePath": "svc/app.go", "symbol": {"name": "Greet", "filePath": "svc/greeter.go", "startLine": 5}},
                    ]},
                ]),
                file: Some("svc/app.go"),
                short_names: true,
                definition_line: Some(3),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "Greet",
                expected: json!([
                    {"filePath": "svc/greeter.go", "startLine": 5, "edges": [
                        {"filePath": "svc/app.go", "symbol": {"name": "Run", "filePath": "svc/app.go", "startLine": 3}},
                    ]},
                ]),
                file: Some("svc/greeter.go"),
                short_names: true,
                definition_line: Some(5),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/safishamsi/graphify/blob/5eedffa87476a063a05bed96f17275ed63f05c1a/tests/test_go_receiver_member_calls.py#L74
// test_a_composite_literal_binding_types_the_receiver
#[tokio::test]
async fn graphify_go_a_composite_literal_binding_types_the_receiver() {
    check_case(Case {
        name: "graphify_go_a_composite_literal_binding_types_the_receiver",
        files: vec![
            SourceFile {
                path: "svc/greeter.go",
                source: "package svc\n\ntype Greeter struct{}\n\nfunc (g *Greeter) Greet() {}\n",
            },
            SourceFile {
                path: "svc/app.go",
                source: "package svc\n\nfunc Run() {\n\tg := Greeter{}\n\tg.Greet()\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Run",
                expected: json!([
                    {"filePath": "svc/app.go", "startLine": 3, "edges": [
                        {"filePath": "svc/app.go", "symbol": {"name": "Greet", "filePath": "svc/greeter.go", "startLine": 5}},
                    ]},
                ]),
                file: Some("svc/app.go"),
                short_names: true,
                definition_line: Some(3),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "Greet",
                expected: json!([
                    {"filePath": "svc/greeter.go", "startLine": 5, "edges": [
                        {"filePath": "svc/app.go", "symbol": {"name": "Run", "filePath": "svc/app.go", "startLine": 3}},
                    ]},
                ]),
                file: Some("svc/greeter.go"),
                short_names: true,
                definition_line: Some(5),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/safishamsi/graphify/blob/5eedffa87476a063a05bed96f17275ed63f05c1a/tests/test_go_receiver_member_calls.py#L82
// test_an_address_of_composite_literal_types_the_receiver
#[tokio::test]
async fn graphify_go_an_address_of_composite_literal_types_the_receiver() {
    check_case(Case {
        name: "graphify_go_an_address_of_composite_literal_types_the_receiver",
        files: vec![
            SourceFile {
                path: "svc/greeter.go",
                source: "package svc\n\ntype Greeter struct{}\n\nfunc (g *Greeter) Greet() {}\n",
            },
            SourceFile {
                path: "svc/app.go",
                source: "package svc\n\nfunc Run() {\n\tg := &Greeter{}\n\tg.Greet()\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Run",
                expected: json!([
                    {"filePath": "svc/app.go", "startLine": 3, "edges": [
                        {"filePath": "svc/app.go", "symbol": {"name": "Greet", "filePath": "svc/greeter.go", "startLine": 5}},
                    ]},
                ]),
                file: Some("svc/app.go"),
                short_names: true,
                definition_line: Some(3),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "Greet",
                expected: json!([
                    {"filePath": "svc/greeter.go", "startLine": 5, "edges": [
                        {"filePath": "svc/app.go", "symbol": {"name": "Run", "filePath": "svc/app.go", "startLine": 3}},
                    ]},
                ]),
                file: Some("svc/greeter.go"),
                short_names: true,
                definition_line: Some(5),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/safishamsi/graphify/blob/5eedffa87476a063a05bed96f17275ed63f05c1a/tests/test_go_receiver_member_calls.py#L90
// test_a_var_declaration_with_an_inferred_composite_literal_types_the_receiver
#[tokio::test]
async fn graphify_go_a_var_declaration_with_an_inferred_composite_literal_types_the_receiver() {
    check_case(Case {
        name: "graphify_go_a_var_declaration_with_an_inferred_composite_literal_types_the_receiver",
        files: vec![
            SourceFile {
                path: "svc/greeter.go",
                source: "package svc\n\ntype Greeter struct{}\n\nfunc (g *Greeter) Greet() {}\n",
            },
            SourceFile {
                path: "svc/app.go",
                source: "package svc\n\nfunc Run() {\n\tvar g = Greeter{}\n\tg.Greet()\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Run",
                expected: json!([
                    {"filePath": "svc/app.go", "startLine": 3, "edges": [
                        {"filePath": "svc/app.go", "symbol": {"name": "Greet", "filePath": "svc/greeter.go", "startLine": 5}},
                    ]},
                ]),
                file: Some("svc/app.go"),
                short_names: true,
                definition_line: Some(3),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "Greet",
                expected: json!([
                    {"filePath": "svc/greeter.go", "startLine": 5, "edges": [
                        {"filePath": "svc/app.go", "symbol": {"name": "Run", "filePath": "svc/app.go", "startLine": 3}},
                    ]},
                ]),
                file: Some("svc/greeter.go"),
                short_names: true,
                definition_line: Some(5),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/safishamsi/graphify/blob/5eedffa87476a063a05bed96f17275ed63f05c1a/tests/test_go_receiver_member_calls.py#L102
// test_a_var_declaration_with_an_inferred_address_of_composite_literal_types_the_receiver
#[tokio::test]
async fn graphify_go_a_var_declaration_with_an_inferred_address_of_composite_literal_types_the_receiver()
 {
    check_case(Case {
        name: "graphify_go_a_var_declaration_with_an_inferred_address_of_composite_literal_types_the_receiver",
        files: vec![
            SourceFile {
                path: "svc/greeter.go",
                source: "package svc\n\ntype Greeter struct{}\n\nfunc (g *Greeter) Greet() {}\n",
            },
            SourceFile {
                path: "svc/app.go",
                source: "package svc\n\nfunc Run() {\n\tvar g = &Greeter{}\n\tg.Greet()\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Run",
                expected: json!([
                    {"filePath": "svc/app.go", "startLine": 3, "edges": [
                        {"filePath": "svc/app.go", "symbol": {"name": "Greet", "filePath": "svc/greeter.go", "startLine": 5}},
                    ]},
                ]),
                file: Some("svc/app.go"),
                short_names: true,
                definition_line: Some(3),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "Greet",
                expected: json!([
                    {"filePath": "svc/greeter.go", "startLine": 5, "edges": [
                        {"filePath": "svc/app.go", "symbol": {"name": "Run", "filePath": "svc/app.go", "startLine": 3}},
                    ]},
                ]),
                file: Some("svc/greeter.go"),
                short_names: true,
                definition_line: Some(5),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/safishamsi/graphify/blob/5eedffa87476a063a05bed96f17275ed63f05c1a/tests/test_go_receiver_member_calls.py#L56
// test_a_struct_field_types_the_receiver_through_the_methods_own_receiver
#[tokio::test]
async fn graphify_go_a_struct_field_types_the_receiver_through_the_methods_own_receiver() {
    check_case(Case {
        name: "graphify_go_a_struct_field_types_the_receiver_through_the_methods_own_receiver",
        files: vec![
            SourceFile {
                path: "svc/greeter.go",
                source: "package svc\n\ntype Greeter struct{}\n\nfunc (g *Greeter) Greet() {}\n",
            },
            SourceFile {
                path: "svc/app.go",
                source: "package svc\n\ntype App struct {\n\tgreeter *Greeter\n}\n\nfunc (a *App) Run() { a.greeter.Greet() }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Run",
                expected: json!([
                    {"filePath": "svc/app.go", "startLine": 7, "edges": [
                        {"filePath": "svc/app.go", "symbol": {"name": "Greet", "filePath": "svc/greeter.go", "startLine": 5}},
                    ]},
                ]),
                file: Some("svc/app.go"),
                short_names: true,
                definition_line: Some(7),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "Greet",
                expected: json!([
                    {"filePath": "svc/greeter.go", "startLine": 5, "edges": [
                        {"filePath": "svc/app.go", "symbol": {"name": "Run", "filePath": "svc/app.go", "startLine": 7}},
                    ]},
                ]),
                file: Some("svc/greeter.go"),
                short_names: true,
                definition_line: Some(5),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/safishamsi/graphify/blob/5eedffa87476a063a05bed96f17275ed63f05c1a/tests/test_go_receiver_member_calls.py#L165
// test_a_same_file_method_call_resolves_once_through_its_receiver
#[tokio::test]
async fn graphify_go_a_same_file_method_call_resolves_once_through_its_receiver() {
    check_case(Case {
        name: "graphify_go_a_same_file_method_call_resolves_once_through_its_receiver",
        files: vec![
            SourceFile {
                path: "svc/app.go",
                source: "package svc\n\ntype App struct{}\n\nfunc (a *App) Greet() {}\n\nfunc Run(a *App) { a.Greet() }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Run",
                expected: json!([
                    {"filePath": "svc/app.go", "startLine": 7, "edges": [
                        {"filePath": "svc/app.go", "symbol": {"name": "Greet", "filePath": "svc/app.go", "startLine": 5}},
                    ]},
                ]),
                file: Some("svc/app.go"),
                short_names: true,
                definition_line: Some(7),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "Greet",
                expected: json!([
                    {"filePath": "svc/app.go", "startLine": 5, "edges": [
                        {"filePath": "svc/app.go", "symbol": {"name": "Run", "filePath": "svc/app.go", "startLine": 7}},
                    ]},
                ]),
                file: Some("svc/app.go"),
                short_names: true,
                definition_line: Some(5),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/safishamsi/graphify/blob/5eedffa87476a063a05bed96f17275ed63f05c1a/tests/test_go_receiver_member_calls.py#L201
// test_a_parameter_binds_only_inside_its_own_function
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "Keep the upstream source and its expected queries together"
)]
async fn graphify_go_a_parameter_binds_only_inside_its_own_function() {
    check_case(Case {
        name: "graphify_go_a_parameter_binds_only_inside_its_own_function",
        files: vec![
            SourceFile {
                path: "svc/greeter.go",
                source: "package svc\n\ntype Greeter struct{}\n\nfunc (g *Greeter) Greet() {}\n",
            },
            SourceFile {
                path: "svc/other.go",
                source: "package svc\n\ntype Other struct{}\n\nfunc (o *Other) Greet() {}\n",
            },
            SourceFile {
                path: "svc/app.go",
                source: "package svc\n\nfunc Run(g *Greeter) { g.Greet() }\n\nfunc Also(g *Other) { g.Greet() }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Run",
                expected: json!([
                    {"filePath": "svc/app.go", "startLine": 3, "edges": [
                        {"filePath": "svc/app.go", "symbol": {"name": "Greet", "filePath": "svc/greeter.go", "startLine": 5}},
                    ]},
                ]),
                file: Some("svc/app.go"),
                short_names: true,
                definition_line: Some(3),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "Greet",
                expected: json!([
                    {"filePath": "svc/greeter.go", "startLine": 5, "edges": [
                        {"filePath": "svc/app.go", "symbol": {"name": "Run", "filePath": "svc/app.go", "startLine": 3}},
                    ]},
                ]),
                file: Some("svc/greeter.go"),
                short_names: true,
                definition_line: Some(5),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Also",
                expected: json!([
                    {"filePath": "svc/app.go", "startLine": 5, "edges": [
                        {"filePath": "svc/app.go", "symbol": {"name": "Greet", "filePath": "svc/other.go", "startLine": 5}},
                    ]},
                ]),
                file: Some("svc/app.go"),
                short_names: true,
                definition_line: Some(5),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "Greet",
                expected: json!([
                    {"filePath": "svc/other.go", "startLine": 5, "edges": [
                        {"filePath": "svc/app.go", "symbol": {"name": "Also", "filePath": "svc/app.go", "startLine": 5}},
                    ]},
                ]),
                file: Some("svc/other.go"),
                short_names: true,
                definition_line: Some(5),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Run",
                expected: json!([
                    {"filePath": "svc/app.go", "startLine": 3, "edges": [
                        {"filePath": "svc/app.go", "symbol": {"name": "Greet", "filePath": "svc/other.go", "startLine": 5}},
                    ]},
                ]),
                file: Some("svc/app.go"),
                short_names: true,
                definition_line: Some(3),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "Greet",
                expected: json!([
                    {"filePath": "svc/other.go", "startLine": 5, "edges": [
                        {"filePath": "svc/app.go", "symbol": {"name": "Run", "filePath": "svc/app.go", "startLine": 3}},
                    ]},
                ]),
                file: Some("svc/other.go"),
                short_names: true,
                definition_line: Some(5),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Also",
                expected: json!([
                    {"filePath": "svc/app.go", "startLine": 5, "edges": [
                        {"filePath": "svc/app.go", "symbol": {"name": "Greet", "filePath": "svc/greeter.go", "startLine": 5}},
                    ]},
                ]),
                file: Some("svc/app.go"),
                short_names: true,
                definition_line: Some(5),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "Greet",
                expected: json!([
                    {"filePath": "svc/greeter.go", "startLine": 5, "edges": [
                        {"filePath": "svc/app.go", "symbol": {"name": "Also", "filePath": "svc/app.go", "startLine": 5}},
                    ]},
                ]),
                file: Some("svc/greeter.go"),
                short_names: true,
                definition_line: Some(5),
                check: Check::Excludes,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/safishamsi/graphify/blob/5eedffa87476a063a05bed96f17275ed63f05c1a/tests/test_go_receiver_member_calls.py#L229
// test_a_struct_field_declared_in_a_sibling_file_types_the_receiver
#[tokio::test]
async fn graphify_go_a_struct_field_declared_in_a_sibling_file_types_the_receiver() {
    check_case(Case {
        name: "graphify_go_a_struct_field_declared_in_a_sibling_file_types_the_receiver",
        files: vec![
            SourceFile {
                path: "svc/greeter.go",
                source: "package svc\n\ntype Greeter struct{}\n\nfunc (g *Greeter) Greet() {}\n",
            },
            SourceFile {
                path: "svc/types.go",
                source: "package svc\n\ntype App struct {\n\tgreeter *Greeter\n}\n",
            },
            SourceFile {
                path: "svc/run.go",
                source: "package svc\n\nfunc (a *App) Run() { a.greeter.Greet() }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Run",
                expected: json!([
                    {"filePath": "svc/run.go", "startLine": 3, "edges": [
                        {"filePath": "svc/run.go", "symbol": {"name": "Greet", "filePath": "svc/greeter.go", "startLine": 5}},
                    ]},
                ]),
                file: Some("svc/run.go"),
                short_names: true,
                definition_line: Some(3),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "Greet",
                expected: json!([
                    {"filePath": "svc/greeter.go", "startLine": 5, "edges": [
                        {"filePath": "svc/run.go", "symbol": {"name": "Run", "filePath": "svc/run.go", "startLine": 3}},
                    ]},
                ]),
                file: Some("svc/greeter.go"),
                short_names: true,
                definition_line: Some(5),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/safishamsi/graphify/blob/5eedffa87476a063a05bed96f17275ed63f05c1a/tests/test_go_receiver_member_calls.py#L290
// test_a_qualified_struct_field_types_the_receiver
#[tokio::test]
async fn graphify_go_a_qualified_struct_field_types_the_receiver() {
    check_case(Case {
        name: "graphify_go_a_qualified_struct_field_types_the_receiver",
        files: vec![
            SourceFile {
                path: "go.mod",
                source: "module example.com/app\n\ngo 1.22\n",
            },
            SourceFile {
                path: "svc/greeter.go",
                source: "package svc\n\ntype Greeter struct{}\n\nfunc (g *Greeter) Greet() {}\n",
            },
            SourceFile {
                path: "api/app.go",
                source: "package api\n\nimport \"example.com/app/svc\"\n\ntype App struct {\n\tgreeter *svc.Greeter\n}\n\nfunc (a *App) Run() { a.greeter.Greet() }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Run",
                expected: json!([
                    {"filePath": "api/app.go", "startLine": 9, "edges": [
                        {"filePath": "api/app.go", "symbol": {"name": "Greet", "filePath": "svc/greeter.go", "startLine": 5}},
                    ]},
                ]),
                file: Some("api/app.go"),
                short_names: true,
                definition_line: Some(9),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "Greet",
                expected: json!([
                    {"filePath": "svc/greeter.go", "startLine": 5, "edges": [
                        {"filePath": "api/app.go", "symbol": {"name": "Run", "filePath": "api/app.go", "startLine": 9}},
                    ]},
                ]),
                file: Some("svc/greeter.go"),
                short_names: true,
                definition_line: Some(5),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/safishamsi/graphify/blob/5eedffa87476a063a05bed96f17275ed63f05c1a/tests/test_go_receiver_member_calls.py#L303
// test_a_qualified_parameter_types_the_receiver
#[tokio::test]
async fn graphify_go_a_qualified_parameter_types_the_receiver() {
    check_case(Case {
        name: "graphify_go_a_qualified_parameter_types_the_receiver",
        files: vec![
            SourceFile {
                path: "go.mod",
                source: "module example.com/app\n\ngo 1.22\n",
            },
            SourceFile {
                path: "svc/greeter.go",
                source: "package svc\n\ntype Greeter struct{}\n\nfunc (g *Greeter) Greet() {}\n",
            },
            SourceFile {
                path: "api/app.go",
                source: "package api\n\nimport \"example.com/app/svc\"\n\nfunc Run(g *svc.Greeter) { g.Greet() }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Run",
                expected: json!([
                    {"filePath": "api/app.go", "startLine": 5, "edges": [
                        {"filePath": "api/app.go", "symbol": {"name": "Greet", "filePath": "svc/greeter.go", "startLine": 5}},
                    ]},
                ]),
                file: Some("api/app.go"),
                short_names: true,
                definition_line: Some(5),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "Greet",
                expected: json!([
                    {"filePath": "svc/greeter.go", "startLine": 5, "edges": [
                        {"filePath": "api/app.go", "symbol": {"name": "Run", "filePath": "api/app.go", "startLine": 5}},
                    ]},
                ]),
                file: Some("svc/greeter.go"),
                short_names: true,
                definition_line: Some(5),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/safishamsi/graphify/blob/5eedffa87476a063a05bed96f17275ed63f05c1a/tests/test_go_receiver_member_calls.py#L323
// test_an_aliased_import_types_the_receiver
#[tokio::test]
async fn graphify_go_an_aliased_import_types_the_receiver() {
    check_case(Case {
        name: "graphify_go_an_aliased_import_types_the_receiver",
        files: vec![
            SourceFile {
                path: "go.mod",
                source: "module example.com/app\n\ngo 1.22\n",
            },
            SourceFile {
                path: "svc/greeter.go",
                source: "package svc\n\ntype Greeter struct{}\n\nfunc (g *Greeter) Greet() {}\n",
            },
            SourceFile {
                path: "api/app.go",
                source: "package api\n\nimport greet \"example.com/app/svc\"\n\nfunc Run(g *greet.Greeter) { g.Greet() }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Run",
                expected: json!([
                    {"filePath": "api/app.go", "startLine": 5, "edges": [
                        {"filePath": "api/app.go", "symbol": {"name": "Greet", "filePath": "svc/greeter.go", "startLine": 5}},
                    ]},
                ]),
                file: Some("api/app.go"),
                short_names: true,
                definition_line: Some(5),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "Greet",
                expected: json!([
                    {"filePath": "svc/greeter.go", "startLine": 5, "edges": [
                        {"filePath": "api/app.go", "symbol": {"name": "Run", "filePath": "api/app.go", "startLine": 5}},
                    ]},
                ]),
                file: Some("svc/greeter.go"),
                short_names: true,
                definition_line: Some(5),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/safishamsi/graphify/blob/5eedffa87476a063a05bed96f17275ed63f05c1a/tests/test_go_receiver_member_calls.py#L344
// test_the_import_picks_one_of_two_packages_declaring_the_type
#[tokio::test]
async fn graphify_go_the_import_picks_one_of_two_packages_declaring_the_type() {
    check_case(Case {
        name: "graphify_go_the_import_picks_one_of_two_packages_declaring_the_type",
        files: vec![
            SourceFile {
                path: "go.mod",
                source: "module example.com/app\n\ngo 1.22\n",
            },
            SourceFile {
                path: "a/greeter.go",
                source: "package a\n\ntype Greeter struct{}\n\nfunc (g *Greeter) Greet() {}\n",
            },
            SourceFile {
                path: "b/greeter.go",
                source: "package b\n\ntype Greeter struct{}\n\nfunc (g *Greeter) Greet() {}\n",
            },
            SourceFile {
                path: "api/app.go",
                source: "package api\n\nimport \"example.com/app/b\"\n\nfunc Run(g *b.Greeter) { g.Greet() }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Run",
                expected: json!([
                    {"filePath": "api/app.go", "startLine": 5, "edges": [
                        {"filePath": "api/app.go", "symbol": {"name": "Greet", "filePath": "b/greeter.go", "startLine": 5}},
                    ]},
                ]),
                file: Some("api/app.go"),
                short_names: true,
                definition_line: Some(5),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "Greet",
                expected: json!([
                    {"filePath": "b/greeter.go", "startLine": 5, "edges": [
                        {"filePath": "api/app.go", "symbol": {"name": "Run", "filePath": "api/app.go", "startLine": 5}},
                    ]},
                ]),
                file: Some("b/greeter.go"),
                short_names: true,
                definition_line: Some(5),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "Run",
                expected: json!([
                    {"filePath": "api/app.go", "startLine": 5, "edges": [
                        {"filePath": "api/app.go", "symbol": {"name": "Greet", "filePath": "a/greeter.go", "startLine": 5}},
                    ]},
                ]),
                file: Some("api/app.go"),
                short_names: true,
                definition_line: Some(5),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "Greet",
                expected: json!([
                    {"filePath": "a/greeter.go", "startLine": 5, "edges": [
                        {"filePath": "api/app.go", "symbol": {"name": "Run", "filePath": "api/app.go", "startLine": 5}},
                    ]},
                ]),
                file: Some("a/greeter.go"),
                short_names: true,
                definition_line: Some(5),
                check: Check::Excludes,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/safishamsi/graphify/blob/5eedffa87476a063a05bed96f17275ed63f05c1a/tests/test_go_receiver_member_calls.py#L358
// test_a_qualifier_never_binds_a_same_named_type_at_another_path
#[tokio::test]
async fn graphify_go_a_qualifier_never_binds_a_same_named_type_at_another_path() {
    check_case(Case {
        name: "graphify_go_a_qualifier_never_binds_a_same_named_type_at_another_path",
        files: vec![
            SourceFile {
                path: "go.mod",
                source: "module example.com/app\n\ngo 1.22\n",
            },
            SourceFile {
                path: "svc/greeter.go",
                source: "package svc\n\ntype Greeter struct{}\n\nfunc (g *Greeter) Greet() {}\n",
            },
            SourceFile {
                path: "api/app.go",
                source: "package api\n\nimport \"example.org/other\"\n\nfunc Run(g *other.Greeter) { g.Greet() }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Run",
                expected: json!([
                    {"filePath": "api/app.go", "startLine": 5, "edges": [
                        {"filePath": "api/app.go", "symbol": {"name": "Greet", "filePath": "svc/greeter.go", "startLine": 5}},
                    ]},
                ]),
                file: Some("api/app.go"),
                short_names: true,
                definition_line: Some(5),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "Greet",
                expected: json!([
                    {"filePath": "svc/greeter.go", "startLine": 5, "edges": [
                        {"filePath": "api/app.go", "symbol": {"name": "Run", "filePath": "api/app.go", "startLine": 5}},
                    ]},
                ]),
                file: Some("svc/greeter.go"),
                short_names: true,
                definition_line: Some(5),
                check: Check::Excludes,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/safishamsi/graphify/blob/5eedffa87476a063a05bed96f17275ed63f05c1a/tests/test_java_member_calls.py#L48
// test_explicit_type_receiver_resolves_to_owned_method
#[tokio::test]
async fn graphify_java_explicit_type_receiver_resolves_to_owned_method() {
    check_case(Case {
        name: "graphify_java_explicit_type_receiver_resolves_to_owned_method",
        files: vec![
            SourceFile {
                path: "Services.java",
                source: "class PaymentGateway { static void ping() {} void charge() {} }\nclass AuditLog { static void ping() {} void charge() {} }\n",
            },
            SourceFile {
                path: "Checkout.java",
                source: "class Checkout { void run() { PaymentGateway.ping(); } }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "run",
                expected: json!([
                    {"filePath": "Checkout.java", "startLine": 1, "edges": [
                        {"filePath": "Checkout.java", "symbol": {"name": "ping", "filePath": "Services.java", "startLine": 1}},
                    ]},
                ]),
                file: Some("Checkout.java"),
                short_names: true,
                definition_line: Some(1),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "ping",
                expected: json!([
                    {"filePath": "Services.java", "startLine": 1, "edges": [
                        {"filePath": "Checkout.java", "symbol": {"name": "run", "filePath": "Checkout.java", "startLine": 1}},
                    ]},
                ]),
                file: Some("Services.java"),
                short_names: true,
                definition_line: Some(1),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "run",
                expected: json!([
                    {"filePath": "Checkout.java", "startLine": 1, "edges": [
                        {"filePath": "Checkout.java", "symbol": {"name": "ping", "filePath": "Services.java", "startLine": 2}},
                    ]},
                ]),
                file: Some("Checkout.java"),
                short_names: true,
                definition_line: Some(1),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "ping",
                expected: json!([
                    {"filePath": "Services.java", "startLine": 2, "edges": [
                        {"filePath": "Checkout.java", "symbol": {"name": "run", "filePath": "Checkout.java", "startLine": 1}},
                    ]},
                ]),
                file: Some("Services.java"),
                short_names: true,
                definition_line: Some(2),
                check: Check::Excludes,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/safishamsi/graphify/blob/5eedffa87476a063a05bed96f17275ed63f05c1a/tests/test_java_member_calls.py#L63
// test_field_receiver_resolves_to_declared_type
#[tokio::test]
async fn graphify_java_field_receiver_resolves_to_declared_type() {
    check_case(Case {
        name: "graphify_java_field_receiver_resolves_to_declared_type",
        files: vec![
            SourceFile {
                path: "Services.java",
                source: "class PaymentGateway { static void ping() {} void charge() {} }\nclass AuditLog { static void ping() {} void charge() {} }\n",
            },
            SourceFile {
                path: "Checkout.java",
                source: "class Checkout {\n    void run() { gateway.charge(); }\n    PaymentGateway gateway;\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "run",
                expected: json!([
                    {"filePath": "Checkout.java", "startLine": 2, "edges": [
                        {"filePath": "Checkout.java", "symbol": {"name": "charge", "filePath": "Services.java", "startLine": 1}},
                    ]},
                ]),
                file: Some("Checkout.java"),
                short_names: true,
                definition_line: Some(2),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "charge",
                expected: json!([
                    {"filePath": "Services.java", "startLine": 1, "edges": [
                        {"filePath": "Checkout.java", "symbol": {"name": "run", "filePath": "Checkout.java", "startLine": 2}},
                    ]},
                ]),
                file: Some("Services.java"),
                short_names: true,
                definition_line: Some(1),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "run",
                expected: json!([
                    {"filePath": "Checkout.java", "startLine": 2, "edges": [
                        {"filePath": "Checkout.java", "symbol": {"name": "charge", "filePath": "Services.java", "startLine": 2}},
                    ]},
                ]),
                file: Some("Checkout.java"),
                short_names: true,
                definition_line: Some(2),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "charge",
                expected: json!([
                    {"filePath": "Services.java", "startLine": 2, "edges": [
                        {"filePath": "Checkout.java", "symbol": {"name": "run", "filePath": "Checkout.java", "startLine": 2}},
                    ]},
                ]),
                file: Some("Services.java"),
                short_names: true,
                definition_line: Some(2),
                check: Check::Excludes,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/safishamsi/graphify/blob/5eedffa87476a063a05bed96f17275ed63f05c1a/tests/test_java_member_calls.py#L81
// test_this_field_receiver_resolves_to_declared_type
#[tokio::test]
async fn graphify_java_this_field_receiver_resolves_to_declared_type() {
    check_case(Case {
        name: "graphify_java_this_field_receiver_resolves_to_declared_type",
        files: vec![
            SourceFile {
                path: "Services.java",
                source: "class PaymentGateway { static void ping() {} void charge() {} }\nclass AuditLog { static void ping() {} void charge() {} }\n",
            },
            SourceFile {
                path: "Checkout.java",
                source: "class Checkout {\n    PaymentGateway gateway;\n    void run() { this.gateway.charge(); }\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "run",
                expected: json!([
                    {"filePath": "Checkout.java", "startLine": 3, "edges": [
                        {"filePath": "Checkout.java", "symbol": {"name": "charge", "filePath": "Services.java", "startLine": 1}},
                    ]},
                ]),
                file: Some("Checkout.java"),
                short_names: true,
                definition_line: Some(3),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "charge",
                expected: json!([
                    {"filePath": "Services.java", "startLine": 1, "edges": [
                        {"filePath": "Checkout.java", "symbol": {"name": "run", "filePath": "Checkout.java", "startLine": 3}},
                    ]},
                ]),
                file: Some("Services.java"),
                short_names: true,
                definition_line: Some(1),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/safishamsi/graphify/blob/5eedffa87476a063a05bed96f17275ed63f05c1a/tests/test_java_member_calls.py#L97
// test_this_field_uses_field_type_when_parameter_shadows_name
#[tokio::test]
async fn graphify_java_this_field_uses_field_type_when_parameter_shadows_name() {
    check_case(Case {
        name: "graphify_java_this_field_uses_field_type_when_parameter_shadows_name",
        files: vec![
            SourceFile {
                path: "Services.java",
                source: "class PaymentGateway { static void ping() {} void charge() {} }\nclass AuditLog { static void ping() {} void charge() {} }\n",
            },
            SourceFile {
                path: "Checkout.java",
                source: "class Checkout {\n    PaymentGateway service;\n    void run(AuditLog service) {\n        service.charge();\n        this.service.charge();\n    }\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "run",
                expected: json!([
                    {"filePath": "Checkout.java", "startLine": 3, "edges": [
                        {"filePath": "Checkout.java", "symbol": {"name": "charge", "filePath": "Services.java", "startLine": 1}},
                        {"filePath": "Checkout.java", "symbol": {"name": "charge", "filePath": "Services.java", "startLine": 2}},
                    ]},
                ]),
                file: Some("Checkout.java"),
                short_names: true,
                definition_line: Some(3),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "charge",
                expected: json!([
                    {"filePath": "Services.java", "startLine": 1, "edges": [
                        {"filePath": "Checkout.java", "symbol": {"name": "run", "filePath": "Checkout.java", "startLine": 3}},
                    ]},
                ]),
                file: Some("Services.java"),
                short_names: true,
                definition_line: Some(1),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "charge",
                expected: json!([
                    {"filePath": "Services.java", "startLine": 2, "edges": [
                        {"filePath": "Checkout.java", "symbol": {"name": "run", "filePath": "Checkout.java", "startLine": 3}},
                    ]},
                ]),
                file: Some("Services.java"),
                short_names: true,
                definition_line: Some(2),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/safishamsi/graphify/blob/5eedffa87476a063a05bed96f17275ed63f05c1a/tests/test_java_member_calls.py#L118
// test_parameter_and_local_receivers_resolve_per_method
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "Keep the upstream source and its expected queries together"
)]
async fn graphify_java_parameter_and_local_receivers_resolve_per_method() {
    check_case(Case {
        name: "graphify_java_parameter_and_local_receivers_resolve_per_method",
        files: vec![
            SourceFile {
                path: "Services.java",
                source: "class PaymentGateway { static void ping() {} void charge() {} }\nclass AuditLog { static void ping() {} void charge() {} }\n",
            },
            SourceFile {
                path: "Checkout.java",
                source: "class Checkout {\n    void fromParameter(PaymentGateway service) { service.charge(); }\n    void fromLocal() { AuditLog service = new AuditLog(); service.charge(); }\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "fromParameter",
                expected: json!([
                    {"filePath": "Checkout.java", "startLine": 2, "edges": [
                        {"filePath": "Checkout.java", "symbol": {"name": "charge", "filePath": "Services.java", "startLine": 1}},
                    ]},
                ]),
                file: Some("Checkout.java"),
                short_names: true,
                definition_line: Some(2),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "charge",
                expected: json!([
                    {"filePath": "Services.java", "startLine": 1, "edges": [
                        {"filePath": "Checkout.java", "symbol": {"name": "fromParameter", "filePath": "Checkout.java", "startLine": 2}},
                    ]},
                ]),
                file: Some("Services.java"),
                short_names: true,
                definition_line: Some(1),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "fromParameter",
                expected: json!([
                    {"filePath": "Checkout.java", "startLine": 2, "edges": [
                        {"filePath": "Checkout.java", "symbol": {"name": "charge", "filePath": "Services.java", "startLine": 2}},
                    ]},
                ]),
                file: Some("Checkout.java"),
                short_names: true,
                definition_line: Some(2),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "charge",
                expected: json!([
                    {"filePath": "Services.java", "startLine": 2, "edges": [
                        {"filePath": "Checkout.java", "symbol": {"name": "fromParameter", "filePath": "Checkout.java", "startLine": 2}},
                    ]},
                ]),
                file: Some("Services.java"),
                short_names: true,
                definition_line: Some(2),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "fromLocal",
                expected: json!([
                    {"filePath": "Checkout.java", "startLine": 3, "edges": [
                        {"filePath": "Checkout.java", "symbol": {"name": "charge", "filePath": "Services.java", "startLine": 2}},
                    ]},
                ]),
                file: Some("Checkout.java"),
                short_names: true,
                definition_line: Some(3),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "charge",
                expected: json!([
                    {"filePath": "Services.java", "startLine": 2, "edges": [
                        {"filePath": "Checkout.java", "symbol": {"name": "fromLocal", "filePath": "Checkout.java", "startLine": 3}},
                    ]},
                ]),
                file: Some("Services.java"),
                short_names: true,
                definition_line: Some(2),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "fromLocal",
                expected: json!([
                    {"filePath": "Checkout.java", "startLine": 3, "edges": [
                        {"filePath": "Checkout.java", "symbol": {"name": "charge", "filePath": "Services.java", "startLine": 1}},
                    ]},
                ]),
                file: Some("Checkout.java"),
                short_names: true,
                definition_line: Some(3),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "charge",
                expected: json!([
                    {"filePath": "Services.java", "startLine": 1, "edges": [
                        {"filePath": "Checkout.java", "symbol": {"name": "fromLocal", "filePath": "Checkout.java", "startLine": 3}},
                    ]},
                ]),
                file: Some("Services.java"),
                short_names: true,
                definition_line: Some(1),
                check: Check::Excludes,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/safishamsi/graphify/blob/5eedffa87476a063a05bed96f17275ed63f05c1a/tests/test_js_exported_fn_call_resolution.py#L31
// test_aliased_import_call_in_exported_function_resolves
#[tokio::test]
async fn graphify_js_aliased_import_call_in_exported_function_resolves() {
    check_case(Case {
        name: "graphify_js_aliased_import_call_in_exported_function_resolves",
        files: vec![
            SourceFile {
                path: "m.ts",
                source: "export function foo() { return 1; }\nexport function bar() { return 2; }\n",
            },
            SourceFile {
                path: "al.ts",
                source: "import { bar as baz } from './m';\nexport function useAlias() { return baz(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "useAlias",
                expected: json!([
                    {"filePath": "al.ts", "startLine": 2, "edges": [
                        {"filePath": "al.ts", "symbol": {"name": "bar", "filePath": "m.ts", "startLine": 2}},
                    ]},
                ]),
                file: Some("al.ts"),
                short_names: true,
                definition_line: Some(2),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "bar",
                expected: json!([
                    {"filePath": "m.ts", "startLine": 2, "edges": [
                        {"filePath": "al.ts", "symbol": {"name": "useAlias", "filePath": "al.ts", "startLine": 2}},
                    ]},
                ]),
                file: Some("m.ts"),
                short_names: true,
                definition_line: Some(2),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/safishamsi/graphify/blob/5eedffa87476a063a05bed96f17275ed63f05c1a/tests/test_js_exported_fn_call_resolution.py#L41
// test_aliased_import_call_in_exported_arrow_resolves
#[tokio::test]
async fn graphify_js_aliased_import_call_in_exported_arrow_resolves() {
    check_case(Case {
        name: "graphify_js_aliased_import_call_in_exported_arrow_resolves",
        files: vec![
            SourceFile {
                path: "m.ts",
                source: "export function foo() { return 1; }\nexport function bar() { return 2; }\n",
            },
            SourceFile {
                path: "al.ts",
                source: "import { bar as baz } from './m';\nexport const useArrow = () => baz();\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "useArrow",
                expected: json!([
                    {"filePath": "al.ts", "startLine": 2, "edges": [
                        {"filePath": "al.ts", "symbol": {"name": "bar", "filePath": "m.ts", "startLine": 2}},
                    ]},
                ]),
                file: Some("al.ts"),
                short_names: true,
                definition_line: Some(2),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "bar",
                expected: json!([
                    {"filePath": "m.ts", "startLine": 2, "edges": [
                        {"filePath": "al.ts", "symbol": {"name": "useArrow", "filePath": "al.ts", "startLine": 2}},
                    ]},
                ]),
                file: Some("m.ts"),
                short_names: true,
                definition_line: Some(2),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/safishamsi/graphify/blob/5eedffa87476a063a05bed96f17275ed63f05c1a/tests/test_js_exported_fn_call_resolution.py#L51
// test_plain_named_import_control_still_resolves
#[tokio::test]
async fn graphify_js_plain_named_import_control_still_resolves() {
    check_case(Case {
        name: "graphify_js_plain_named_import_control_still_resolves",
        files: vec![
            SourceFile {
                path: "m.ts",
                source: "export function foo() { return 1; }\nexport function bar() { return 2; }\n",
            },
            SourceFile {
                path: "p.ts",
                source: "import { foo } from './m';\nexport function usePlain() { return foo(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "usePlain",
                expected: json!([
                    {"filePath": "p.ts", "startLine": 2, "edges": [
                        {"filePath": "p.ts", "symbol": {"name": "foo", "filePath": "m.ts", "startLine": 1}},
                    ]},
                ]),
                file: Some("p.ts"),
                short_names: true,
                definition_line: Some(2),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "foo",
                expected: json!([
                    {"filePath": "m.ts", "startLine": 1, "edges": [
                        {"filePath": "p.ts", "symbol": {"name": "usePlain", "filePath": "p.ts", "startLine": 2}},
                    ]},
                ]),
                file: Some("m.ts"),
                short_names: true,
                definition_line: Some(1),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/safishamsi/graphify/blob/5eedffa87476a063a05bed96f17275ed63f05c1a/tests/test_js_exported_fn_call_resolution.py#L74
// test_non_exported_function_still_resolves
#[tokio::test]
async fn graphify_js_non_exported_function_still_resolves() {
    check_case(Case {
        name: "graphify_js_non_exported_function_still_resolves",
        files: vec![
            SourceFile {
                path: "m.ts",
                source: "export function foo() { return 1; }\nexport function bar() { return 2; }\n",
            },
            SourceFile {
                path: "al.ts",
                source: "import { bar as baz } from './m';\nfunction useAlias() { return baz(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "useAlias",
                expected: json!([
                    {"filePath": "al.ts", "startLine": 2, "edges": [
                        {"filePath": "al.ts", "symbol": {"name": "bar", "filePath": "m.ts", "startLine": 2}},
                    ]},
                ]),
                file: Some("al.ts"),
                short_names: true,
                definition_line: Some(2),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "bar",
                expected: json!([
                    {"filePath": "m.ts", "startLine": 2, "edges": [
                        {"filePath": "al.ts", "symbol": {"name": "useAlias", "filePath": "al.ts", "startLine": 2}},
                    ]},
                ]),
                file: Some("m.ts"),
                short_names: true,
                definition_line: Some(2),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/safishamsi/graphify/blob/5eedffa87476a063a05bed96f17275ed63f05c1a/tests/test_js_member_call_bare_name_fallback.py#L32
// test_delegating_wrapper_does_not_self_loop
#[tokio::test]
async fn graphify_js_delegating_wrapper_does_not_self_loop() {
    check_case(Case {
        name: "graphify_js_delegating_wrapper_does_not_self_loop",
        files: vec![
            SourceFile {
                path: "app.js",
                source: "import { createHistory } from \"./history.js\";\nconst history = createHistory();\n\nexport function appendFrame(entry) {\n  return history.appendFrame(entry);\n}\n\nexport function confirm(message) {\n  return window.confirm(message);\n}\n",
            },
            SourceFile {
                path: "history.js",
                source: "export function createHistory() {\n  return { appendFrame: (e) => e };\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "appendFrame",
                expected: json!([
                    {"filePath": "app.js", "startLine": 4, "edges": [
                        {"filePath": "app.js", "symbol": {"name": "appendFrame", "filePath": "app.js", "startLine": 4}},
                    ]},
                ]),
                file: Some("app.js"),
                short_names: true,
                definition_line: Some(4),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "appendFrame",
                expected: json!([
                    {"filePath": "app.js", "startLine": 4, "edges": [
                        {"filePath": "app.js", "symbol": {"name": "appendFrame", "filePath": "app.js", "startLine": 4}},
                    ]},
                ]),
                file: Some("app.js"),
                short_names: true,
                definition_line: Some(4),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "confirm",
                expected: json!([
                    {"filePath": "app.js", "startLine": 8, "edges": [
                        {"filePath": "app.js", "symbol": {"name": "confirm", "filePath": "app.js", "startLine": 8}},
                    ]},
                ]),
                file: Some("app.js"),
                short_names: true,
                definition_line: Some(8),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "confirm",
                expected: json!([
                    {"filePath": "app.js", "startLine": 8, "edges": [
                        {"filePath": "app.js", "symbol": {"name": "confirm", "filePath": "app.js", "startLine": 8}},
                    ]},
                ]),
                file: Some("app.js"),
                short_names: true,
                definition_line: Some(8),
                check: Check::Excludes,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/safishamsi/graphify/blob/5eedffa87476a063a05bed96f17275ed63f05c1a/tests/test_js_member_call_bare_name_fallback.py#L56
// test_receiver_call_does_not_bind_to_an_unrelated_sibling
#[tokio::test]
async fn graphify_js_receiver_call_does_not_bind_to_an_unrelated_sibling() {
    check_case(Case {
        name: "graphify_js_receiver_call_does_not_bind_to_an_unrelated_sibling",
        files: vec![
            SourceFile {
                path: "app.js",
                source: "function appendFrame(entry) { return entry; }\n\nfunction addFrame(history, entry) {\n  return history.appendFrame(entry);\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "addFrame",
                expected: json!([
                    {"filePath": "app.js", "startLine": 3, "edges": [
                        {"filePath": "app.js", "symbol": {"name": "appendFrame", "filePath": "app.js", "startLine": 1}},
                    ]},
                ]),
                file: Some("app.js"),
                short_names: true,
                definition_line: Some(3),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "appendFrame",
                expected: json!([
                    {"filePath": "app.js", "startLine": 1, "edges": [
                        {"filePath": "app.js", "symbol": {"name": "addFrame", "filePath": "app.js", "startLine": 3}},
                    ]},
                ]),
                file: Some("app.js"),
                short_names: true,
                definition_line: Some(1),
                check: Check::Excludes,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/safishamsi/graphify/blob/5eedffa87476a063a05bed96f17275ed63f05c1a/tests/test_js_member_call_bare_name_fallback.py#L70
// test_bare_unqualified_call_is_unaffected
#[tokio::test]
async fn graphify_js_bare_unqualified_call_is_unaffected() {
    check_case(Case {
        name: "graphify_js_bare_unqualified_call_is_unaffected",
        files: vec![
            SourceFile {
                path: "app.js",
                source: "function helper(x) { return x; }\nfunction run(x) { return helper(x); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "run",
                expected: json!([
                    {"filePath": "app.js", "startLine": 2, "edges": [
                        {"filePath": "app.js", "symbol": {"name": "helper", "filePath": "app.js", "startLine": 1}},
                    ]},
                ]),
                file: Some("app.js"),
                short_names: true,
                definition_line: Some(2),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "helper",
                expected: json!([
                    {"filePath": "app.js", "startLine": 1, "edges": [
                        {"filePath": "app.js", "symbol": {"name": "run", "filePath": "app.js", "startLine": 2}},
                    ]},
                ]),
                file: Some("app.js"),
                short_names: true,
                definition_line: Some(1),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/safishamsi/graphify/blob/5eedffa87476a063a05bed96f17275ed63f05c1a/tests/test_js_member_call_bare_name_fallback.py#L79
// test_this_method_call_is_unaffected
#[tokio::test]
async fn graphify_js_this_method_call_is_unaffected() {
    check_case(Case {
        name: "graphify_js_this_method_call_is_unaffected",
        files: vec![
            SourceFile {
                path: "app.js",
                source: "class Wrapper {\n  run(entry) { return this.appendFrame(entry); }\n  appendFrame(entry) { return entry; }\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "run",
                expected: json!([
                    {"filePath": "app.js", "startLine": 2, "edges": [
                        {"filePath": "app.js", "symbol": {"name": "appendFrame", "filePath": "app.js", "startLine": 3}},
                    ]},
                ]),
                file: Some("app.js"),
                short_names: true,
                definition_line: Some(2),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "appendFrame",
                expected: json!([
                    {"filePath": "app.js", "startLine": 3, "edges": [
                        {"filePath": "app.js", "symbol": {"name": "run", "filePath": "app.js", "startLine": 2}},
                    ]},
                ]),
                file: Some("app.js"),
                short_names: true,
                definition_line: Some(3),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/safishamsi/graphify/blob/5eedffa87476a063a05bed96f17275ed63f05c1a/tests/test_js_member_call_bare_name_fallback.py#L91
// test_ts_typed_receiver_still_resolves_through_its_own_type
#[tokio::test]
async fn graphify_js_ts_typed_receiver_still_resolves_through_its_own_type() {
    check_case(Case {
        name: "graphify_js_ts_typed_receiver_still_resolves_through_its_own_type",
        files: vec![
            SourceFile {
                path: "greeter.ts",
                source: "export class Greeter {\n  greet(): void {}\n}\n",
            },
            SourceFile {
                path: "app.ts",
                source: "import { Greeter } from \"./greeter\";\nexport function run(g: Greeter) {\n  g.greet();\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "run",
                expected: json!([
                    {"filePath": "app.ts", "startLine": 2, "edges": [
                        {"filePath": "app.ts", "symbol": {"name": "greet", "filePath": "greeter.ts", "startLine": 2}},
                    ]},
                ]),
                file: Some("app.ts"),
                short_names: true,
                definition_line: Some(2),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "greet",
                expected: json!([
                    {"filePath": "greeter.ts", "startLine": 2, "edges": [
                        {"filePath": "app.ts", "symbol": {"name": "run", "filePath": "app.ts", "startLine": 2}},
                    ]},
                ]),
                file: Some("greeter.ts"),
                short_names: true,
                definition_line: Some(2),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/safishamsi/graphify/blob/5eedffa87476a063a05bed96f17275ed63f05c1a/tests/test_ts_receiver_member_calls.py#L35
// test_local_new_binding_receiver
#[tokio::test]
async fn graphify_ts_local_new_binding_receiver() {
    check_case(Case {
        name: "graphify_ts_local_new_binding_receiver",
        files: vec![
            SourceFile {
                path: "svc.ts",
                source: "export class Svc {\n  doThing(): number { return 1; }\n}\n",
            },
            SourceFile {
                path: "direct.ts",
                source: "import { Svc } from \"./svc\";\nconst s = new Svc();\nexport function usesDirect(): number { return s.doThing(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "usesDirect",
                expected: json!([
                    {"filePath": "direct.ts", "startLine": 3, "edges": [
                        {"filePath": "direct.ts", "symbol": {"name": "doThing", "filePath": "svc.ts", "startLine": 2}},
                    ]},
                ]),
                file: Some("direct.ts"),
                short_names: true,
                definition_line: Some(3),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "doThing",
                expected: json!([
                    {"filePath": "svc.ts", "startLine": 2, "edges": [
                        {"filePath": "direct.ts", "symbol": {"name": "usesDirect", "filePath": "direct.ts", "startLine": 3}},
                    ]},
                ]),
                file: Some("svc.ts"),
                short_names: true,
                definition_line: Some(2),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/safishamsi/graphify/blob/5eedffa87476a063a05bed96f17275ed63f05c1a/tests/test_ts_receiver_member_calls.py#L54
// test_new_binding_resolves_to_correct_class_under_ambiguity
#[tokio::test]
async fn graphify_ts_new_binding_resolves_to_correct_class_under_ambiguity() {
    check_case(Case {
        name: "graphify_ts_new_binding_resolves_to_correct_class_under_ambiguity",
        files: vec![
            SourceFile {
                path: "svc.ts",
                source: "export class Svc {\n  doThing(): number { return 1; }\n}\n",
            },
            SourceFile {
                path: "cache.ts",
                source: "export class Cache {\n  doThing(): number { return 2; }\n}\n",
            },
            SourceFile {
                path: "d.ts",
                source: "import { Svc } from \"./svc\";\nconst s = new Svc();\nexport function f(): number { return s.doThing(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "f",
                expected: json!([
                    {"filePath": "d.ts", "startLine": 3, "edges": [
                        {"filePath": "d.ts", "symbol": {"name": "doThing", "filePath": "svc.ts", "startLine": 2}},
                    ]},
                ]),
                file: Some("d.ts"),
                short_names: true,
                definition_line: Some(3),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "doThing",
                expected: json!([
                    {"filePath": "svc.ts", "startLine": 2, "edges": [
                        {"filePath": "d.ts", "symbol": {"name": "f", "filePath": "d.ts", "startLine": 3}},
                    ]},
                ]),
                file: Some("svc.ts"),
                short_names: true,
                definition_line: Some(2),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "f",
                expected: json!([
                    {"filePath": "d.ts", "startLine": 3, "edges": [
                        {"filePath": "d.ts", "symbol": {"name": "doThing", "filePath": "cache.ts", "startLine": 2}},
                    ]},
                ]),
                file: Some("d.ts"),
                short_names: true,
                definition_line: Some(3),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "doThing",
                expected: json!([
                    {"filePath": "cache.ts", "startLine": 2, "edges": [
                        {"filePath": "d.ts", "symbol": {"name": "f", "filePath": "d.ts", "startLine": 3}},
                    ]},
                ]),
                file: Some("cache.ts"),
                short_names: true,
                definition_line: Some(2),
                check: Check::Excludes,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/safishamsi/graphify/blob/5eedffa87476a063a05bed96f17275ed63f05c1a/tests/test_ts_receiver_member_calls.py#L101
// test_third_party_type_does_not_fabricate_edge_to_local_class
#[tokio::test]
async fn graphify_ts_third_party_type_does_not_fabricate_edge_to_local_class() {
    check_case(Case {
        name: "graphify_ts_third_party_type_does_not_fabricate_edge_to_local_class",
        files: vec![
            SourceFile {
                path: "fileb.ts",
                source: "export class Repo {\n  save(): void {}\n  static staticSave(): void {}\n}\n",
            },
            SourceFile {
                path: "filea.ts",
                source: "import type { Repo } from 'external-pkg';\nexport class ReportService {\n  constructor(private repo: Repo) {}\n  run(): void { this.repo.save(); }\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "run",
                expected: json!([
                    {"filePath": "filea.ts", "startLine": 4, "edges": [
                        {"filePath": "filea.ts", "symbol": {"name": "save", "filePath": "fileb.ts", "startLine": 2}},
                    ]},
                ]),
                file: Some("filea.ts"),
                short_names: true,
                definition_line: Some(4),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "save",
                expected: json!([
                    {"filePath": "fileb.ts", "startLine": 2, "edges": [
                        {"filePath": "filea.ts", "symbol": {"name": "run", "filePath": "filea.ts", "startLine": 4}},
                    ]},
                ]),
                file: Some("fileb.ts"),
                short_names: true,
                definition_line: Some(2),
                check: Check::Excludes,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/safishamsi/graphify/blob/5eedffa87476a063a05bed96f17275ed63f05c1a/tests/test_ts_receiver_member_calls.py#L115
// test_genuinely_imported_type_still_resolves_inferred
#[tokio::test]
async fn graphify_ts_genuinely_imported_type_still_resolves_inferred() {
    check_case(Case {
        name: "graphify_ts_genuinely_imported_type_still_resolves_inferred",
        files: vec![
            SourceFile {
                path: "fileb.ts",
                source: "export class Repo {\n  save(): void {}\n  static staticSave(): void {}\n}\n",
            },
            SourceFile {
                path: "filea.ts",
                source: "import { Repo } from \"./fileb\";\nexport class ReportService {\n  constructor(private repo: Repo) {}\n  run(): void { this.repo.save(); }\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "run",
                expected: json!([
                    {"filePath": "filea.ts", "startLine": 4, "edges": [
                        {"filePath": "filea.ts", "symbol": {"name": "save", "filePath": "fileb.ts", "startLine": 2}},
                    ]},
                ]),
                file: Some("filea.ts"),
                short_names: true,
                definition_line: Some(4),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "save",
                expected: json!([
                    {"filePath": "fileb.ts", "startLine": 2, "edges": [
                        {"filePath": "filea.ts", "symbol": {"name": "run", "filePath": "filea.ts", "startLine": 4}},
                    ]},
                ]),
                file: Some("fileb.ts"),
                short_names: true,
                definition_line: Some(2),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/safishamsi/graphify/blob/5eedffa87476a063a05bed96f17275ed63f05c1a/tests/test_ts_receiver_member_calls.py#L133
// test_source_qualified_static_call_is_extracted
#[tokio::test]
async fn graphify_ts_source_qualified_static_call_is_extracted() {
    check_case(Case {
        name: "graphify_ts_source_qualified_static_call_is_extracted",
        files: vec![
            SourceFile {
                path: "fileb.ts",
                source: "export class Repo {\n  save(): void {}\n  static staticSave(): void {}\n}\n",
            },
            SourceFile {
                path: "filea.ts",
                source: "import { Repo } from \"./fileb\";\nexport class ReportService {\n  constructor(private repo: Repo) {}\n  run(): void { Repo.staticSave(); }\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "run",
                expected: json!([
                    {"filePath": "filea.ts", "startLine": 4, "edges": [
                        {"filePath": "filea.ts", "symbol": {"name": "staticSave", "filePath": "fileb.ts", "startLine": 3}},
                    ]},
                ]),
                file: Some("filea.ts"),
                short_names: true,
                definition_line: Some(4),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "staticSave",
                expected: json!([
                    {"filePath": "fileb.ts", "startLine": 3, "edges": [
                        {"filePath": "filea.ts", "symbol": {"name": "run", "filePath": "filea.ts", "startLine": 4}},
                    ]},
                ]),
                file: Some("fileb.ts"),
                short_names: true,
                definition_line: Some(3),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/safishamsi/graphify/blob/5eedffa87476a063a05bed96f17275ed63f05c1a/tests/test_ts_receiver_member_calls.py#L151
// test_same_file_type_still_resolves
#[tokio::test]
async fn graphify_ts_same_file_type_still_resolves() {
    check_case(Case {
        name: "graphify_ts_same_file_type_still_resolves",
        files: vec![
            SourceFile {
                path: "one.ts",
                source: "export class Repo {\n  save(): void {}\n  static staticSave(): void {}\n}\nconst r = new Repo();\nexport function runLocal(): void { r.save(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "runLocal",
                expected: json!([
                    {"filePath": "one.ts", "startLine": 6, "edges": [
                        {"filePath": "one.ts", "symbol": {"name": "save", "filePath": "one.ts", "startLine": 2}},
                    ]},
                ]),
                file: Some("one.ts"),
                short_names: true,
                definition_line: Some(6),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "save",
                expected: json!([
                    {"filePath": "one.ts", "startLine": 2, "edges": [
                        {"filePath": "one.ts", "symbol": {"name": "runLocal", "filePath": "one.ts", "startLine": 6}},
                    ]},
                ]),
                file: Some("one.ts"),
                short_names: true,
                definition_line: Some(2),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/safishamsi/graphify/blob/5eedffa87476a063a05bed96f17275ed63f05c1a/tests/test_ts_receiver_member_calls.py#L168
// test_typed_class_fields_type_this_field_calls
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "Keep the upstream source and its expected queries together"
)]
async fn graphify_ts_typed_class_fields_type_this_field_calls() {
    check_case(Case {
        name: "graphify_ts_typed_class_fields_type_this_field_calls",
        files: vec![
            SourceFile {
                path: "svc.ts",
                source: "export class Svc {\n  doThing(): number { return 1; }\n}\n",
            },
            SourceFile {
                path: "k.ts",
                source: "import { Svc } from \"./svc\";\nexport class K {\n  protected plain: Svc;\n  private maybe?: Svc | undefined;\n  #hidden: Svc = new Svc();\n  private made = new Svc();\n  declare readonly declared: Svc;\n  a(): number { return this.plain.doThing(); }\n  b(): number { return this.maybe?.doThing() ?? 0; }\n  c(): number { return this.#hidden.doThing(); }\n  d(): number { return this.made.doThing(); }\n  e(): number { return this.declared.doThing(); }\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "a",
                expected: json!([
                    {"filePath": "k.ts", "startLine": 8, "edges": [
                        {"filePath": "k.ts", "symbol": {"name": "doThing", "filePath": "svc.ts", "startLine": 2}},
                    ]},
                ]),
                file: Some("k.ts"),
                short_names: true,
                definition_line: Some(8),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "doThing",
                expected: json!([
                    {"filePath": "svc.ts", "startLine": 2, "edges": [
                        {"filePath": "k.ts", "symbol": {"name": "a", "filePath": "k.ts", "startLine": 8}},
                        {"filePath": "k.ts", "symbol": {"name": "b", "filePath": "k.ts", "startLine": 9}},
                        {"filePath": "k.ts", "symbol": {"name": "c", "filePath": "k.ts", "startLine": 10}},
                        {"filePath": "k.ts", "symbol": {"name": "d", "filePath": "k.ts", "startLine": 11}},
                        {"filePath": "k.ts", "symbol": {"name": "e", "filePath": "k.ts", "startLine": 12}},
                    ]},
                ]),
                file: Some("svc.ts"),
                short_names: true,
                definition_line: Some(2),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "b",
                expected: json!([
                    {"filePath": "k.ts", "startLine": 9, "edges": [
                        {"filePath": "k.ts", "symbol": {"name": "doThing", "filePath": "svc.ts", "startLine": 2}},
                    ]},
                ]),
                file: Some("k.ts"),
                short_names: true,
                definition_line: Some(9),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "c",
                expected: json!([
                    {"filePath": "k.ts", "startLine": 10, "edges": [
                        {"filePath": "k.ts", "symbol": {"name": "doThing", "filePath": "svc.ts", "startLine": 2}},
                    ]},
                ]),
                file: Some("k.ts"),
                short_names: true,
                definition_line: Some(10),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "d",
                expected: json!([
                    {"filePath": "k.ts", "startLine": 11, "edges": [
                        {"filePath": "k.ts", "symbol": {"name": "doThing", "filePath": "svc.ts", "startLine": 2}},
                    ]},
                ]),
                file: Some("k.ts"),
                short_names: true,
                definition_line: Some(11),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "e",
                expected: json!([
                    {"filePath": "k.ts", "startLine": 12, "edges": [
                        {"filePath": "k.ts", "symbol": {"name": "doThing", "filePath": "svc.ts", "startLine": 2}},
                    ]},
                ]),
                file: Some("k.ts"),
                short_names: true,
                definition_line: Some(12),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/safishamsi/graphify/blob/5eedffa87476a063a05bed96f17275ed63f05c1a/tests/test_cpp_member_call_by_receiver_type.py#L33
// test_base_pointer_call_reaches_the_base_method
#[tokio::test]
async fn graphify_cpp_base_pointer_call_reaches_the_base_method() {
    check_case(Case {
        name: "graphify_cpp_base_pointer_call_reaches_the_base_method",
        files: vec![
            SourceFile {
                path: "base.h",
                source: "class Base { public: virtual void draw(); void lock(); };\n",
            },
            SourceFile {
                path: "derived.cpp",
                source: "#include \"base.h\"\nclass Derived : public Base { public: void draw() override; };\nvoid Derived::draw() {}\nvoid render(Base* b) { b->draw(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "render",
                expected: json!([
                    {"filePath": "derived.cpp", "startLine": 4, "edges": [
                        {"filePath": "derived.cpp", "symbol": {"name": "draw", "filePath": "base.h", "startLine": 1}},
                    ]},
                ]),
                file: Some("derived.cpp"),
                short_names: true,
                definition_line: Some(4),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "draw",
                expected: json!([
                    {"filePath": "base.h", "startLine": 1, "edges": [
                        {"filePath": "derived.cpp", "symbol": {"name": "render", "filePath": "derived.cpp", "startLine": 4}},
                    ]},
                ]),
                file: Some("base.h"),
                short_names: true,
                definition_line: Some(1),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "render",
                expected: json!([
                    {"filePath": "derived.cpp", "startLine": 4, "edges": [
                        {"filePath": "derived.cpp", "symbol": {"name": "draw", "filePath": "derived.cpp", "startLine": 2}},
                        {"filePath": "derived.cpp", "symbol": {"name": "draw", "filePath": "derived.cpp", "startLine": 3}},
                    ]},
                ]),
                file: Some("derived.cpp"),
                short_names: true,
                definition_line: Some(4),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "draw",
                expected: json!([
                    {"filePath": "derived.cpp", "startLine": 2, "edges": [
                        {"filePath": "derived.cpp", "symbol": {"name": "render", "filePath": "derived.cpp", "startLine": 4}},
                    ]},
                ]),
                file: Some("derived.cpp"),
                short_names: true,
                definition_line: Some(2),
                check: Check::Excludes,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "draw",
                expected: json!([
                    {"filePath": "derived.cpp", "startLine": 3, "edges": [
                        {"filePath": "derived.cpp", "symbol": {"name": "render", "filePath": "derived.cpp", "startLine": 4}},
                    ]},
                ]),
                file: Some("derived.cpp"),
                short_names: true,
                definition_line: Some(3),
                check: Check::Excludes,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/safishamsi/graphify/blob/5eedffa87476a063a05bed96f17275ed63f05c1a/tests/test_cpp_member_call_by_receiver_type.py#L48
// test_typed_local_receiver_still_resolves
#[tokio::test]
async fn graphify_cpp_typed_local_receiver_still_resolves() {
    check_case(Case {
        name: "graphify_cpp_typed_local_receiver_still_resolves",
        files: vec![
            SourceFile {
                path: "widget.cpp",
                source: "class Widget { public: void paint(); };\nvoid run() { Widget w; w.paint(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "run",
                expected: json!([
                    {"filePath": "widget.cpp", "startLine": 2, "edges": [
                        {"filePath": "widget.cpp", "symbol": {"name": "paint", "filePath": "widget.cpp", "startLine": 1}},
                    ]},
                ]),
                file: Some("widget.cpp"),
                short_names: true,
                definition_line: Some(2),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "paint",
                expected: json!([
                    {"filePath": "widget.cpp", "startLine": 1, "edges": [
                        {"filePath": "widget.cpp", "symbol": {"name": "run", "filePath": "widget.cpp", "startLine": 2}},
                    ]},
                ]),
                file: Some("widget.cpp"),
                short_names: true,
                definition_line: Some(1),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/safishamsi/graphify/blob/5eedffa87476a063a05bed96f17275ed63f05c1a/tests/test_cpp_member_call_by_receiver_type.py#L56
// test_this_call_keeps_in_file_binding
#[tokio::test]
async fn graphify_cpp_this_call_keeps_in_file_binding() {
    check_case(Case {
        name: "graphify_cpp_this_call_keeps_in_file_binding",
        files: vec![
            SourceFile {
                path: "widget.cpp",
                source: "class Widget { public: void paint(); void run(); };\nvoid Widget::run() { this->paint(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "run",
                expected: json!([
                    {"filePath": "widget.cpp", "startLine": 2, "edges": [
                        {"filePath": "widget.cpp", "symbol": {"name": "paint", "filePath": "widget.cpp", "startLine": 1}},
                    ]},
                ]),
                file: Some("widget.cpp"),
                short_names: true,
                definition_line: Some(2),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "paint",
                expected: json!([
                    {"filePath": "widget.cpp", "startLine": 1, "edges": [
                        {"filePath": "widget.cpp", "symbol": {"name": "run", "filePath": "widget.cpp", "startLine": 2}},
                    ]},
                ]),
                file: Some("widget.cpp"),
                short_names: true,
                definition_line: Some(1),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/safishamsi/graphify/blob/5eedffa87476a063a05bed96f17275ed63f05c1a/tests/test_cpp_member_call_by_receiver_type.py#L64
// test_a_local_in_one_function_does_not_retype_a_parameter_in_another
#[tokio::test]
async fn graphify_cpp_a_local_in_one_function_does_not_retype_a_parameter_in_another() {
    check_case(Case {
        name: "graphify_cpp_a_local_in_one_function_does_not_retype_a_parameter_in_another",
        files: vec![
            SourceFile {
                path: "net.cpp",
                source: "class Request { public: void send(); };\nclass Response { public: void close(); };\nvoid f(Request& r) { r.send(); }\nvoid g() { Response r; r.close(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "f",
                expected: json!([
                    {"filePath": "net.cpp", "startLine": 3, "edges": [
                        {"filePath": "net.cpp", "symbol": {"name": "send", "filePath": "net.cpp", "startLine": 1}},
                    ]},
                ]),
                file: Some("net.cpp"),
                short_names: true,
                definition_line: Some(3),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "send",
                expected: json!([
                    {"filePath": "net.cpp", "startLine": 1, "edges": [
                        {"filePath": "net.cpp", "symbol": {"name": "f", "filePath": "net.cpp", "startLine": 3}},
                    ]},
                ]),
                file: Some("net.cpp"),
                short_names: true,
                definition_line: Some(1),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/safishamsi/graphify/blob/5eedffa87476a063a05bed96f17275ed63f05c1a/tests/test_cpp_member_call_by_receiver_type.py#L81
// test_auto_receiver_keeps_its_in_file_binding
#[tokio::test]
async fn graphify_cpp_auto_receiver_keeps_its_in_file_binding() {
    check_case(Case {
        name: "graphify_cpp_auto_receiver_keeps_its_in_file_binding",
        files: vec![
            SourceFile {
                path: "widget.cpp",
                source: "class Widget { public: void paint(); };\nWidget make();\nvoid run() { auto w = make(); w.paint(); }\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "run",
                expected: json!([
                    {"filePath": "widget.cpp", "startLine": 3, "edges": [
                        {"filePath": "widget.cpp", "symbol": {"name": "paint", "filePath": "widget.cpp", "startLine": 1}},
                    ]},
                ]),
                file: Some("widget.cpp"),
                short_names: true,
                definition_line: Some(3),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "paint",
                expected: json!([
                    {"filePath": "widget.cpp", "startLine": 1, "edges": [
                        {"filePath": "widget.cpp", "symbol": {"name": "run", "filePath": "widget.cpp", "startLine": 3}},
                    ]},
                ]),
                file: Some("widget.cpp"),
                short_names: true,
                definition_line: Some(1),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/safishamsi/graphify/blob/5eedffa87476a063a05bed96f17275ed63f05c1a/tests/test_rust_self_member_calls.py#L49
// test_self_call_resolves_across_files_to_a_split_impl_block
#[tokio::test]
async fn graphify_rust_self_call_resolves_across_files_to_a_split_impl_block() {
    check_case(Case {
        name: "graphify_rust_self_call_resolves_across_files_to_a_split_impl_block",
        files: vec![
            SourceFile {
                path: "state.rs",
                source: "pub struct BlockchainState { height: u64 }\n",
            },
            SourceFile {
                path: "apply.rs",
                source: "use crate::state::BlockchainState;\nimpl BlockchainState {\n    pub fn apply_block(&self) -> u64 { self.height }\n}\n",
            },
            SourceFile {
                path: "rollback.rs",
                source: "use crate::state::BlockchainState;\nimpl BlockchainState {\n    pub fn rollback_one_block(&self) -> u64 { self.apply_block() }\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "rollback_one_block",
                expected: json!([
                    {"filePath": "rollback.rs", "startLine": 3, "edges": [
                        {"filePath": "rollback.rs", "symbol": {"name": "apply_block", "filePath": "apply.rs", "startLine": 3}},
                    ]},
                ]),
                file: Some("rollback.rs"),
                short_names: true,
                definition_line: Some(3),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "apply_block",
                expected: json!([
                    {"filePath": "apply.rs", "startLine": 3, "edges": [
                        {"filePath": "rollback.rs", "symbol": {"name": "rollback_one_block", "filePath": "rollback.rs", "startLine": 3}},
                    ]},
                ]),
                file: Some("apply.rs"),
                short_names: true,
                definition_line: Some(3),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/safishamsi/graphify/blob/5eedffa87476a063a05bed96f17275ed63f05c1a/tests/test_rust_self_member_calls.py#L75
// test_self_call_same_file_control_is_unaffected
#[tokio::test]
async fn graphify_rust_self_call_same_file_control_is_unaffected() {
    check_case(Case {
        name: "graphify_rust_self_call_same_file_control_is_unaffected",
        files: vec![
            SourceFile {
                path: "lib.rs",
                source: "struct Widget { n: u32 }\nimpl Widget {\n    fn get(&self) -> u32 { self.n }\n    fn show(&self) -> u32 { self.get() }\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "show",
                expected: json!([
                    {"filePath": "lib.rs", "startLine": 4, "edges": [
                        {"filePath": "lib.rs", "symbol": {"name": "get", "filePath": "lib.rs", "startLine": 3}},
                    ]},
                ]),
                file: Some("lib.rs"),
                short_names: true,
                definition_line: Some(4),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "get",
                expected: json!([
                    {"filePath": "lib.rs", "startLine": 3, "edges": [
                        {"filePath": "lib.rs", "symbol": {"name": "show", "filePath": "lib.rs", "startLine": 4}},
                    ]},
                ]),
                file: Some("lib.rs"),
                short_names: true,
                definition_line: Some(3),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/safishamsi/graphify/blob/5eedffa87476a063a05bed96f17275ed63f05c1a/tests/test_rust_self_member_calls.py#L92
// test_generic_self_call_resolves_across_alpha_renamed_impls
#[tokio::test]
async fn graphify_rust_generic_self_call_resolves_across_alpha_renamed_impls() {
    check_case(Case {
        name: "graphify_rust_generic_self_call_resolves_across_alpha_renamed_impls",
        files: vec![
            SourceFile {
                path: "state.rs",
                source: "pub struct Bucket<T> { value: T }\n",
            },
            SourceFile {
                path: "read.rs",
                source: "use crate::state::Bucket;\nimpl<T> Bucket<T> {\n    pub fn fetch_value(&self) {}\n}\n",
            },
            SourceFile {
                path: "run.rs",
                source: "use crate::state::Bucket;\nimpl<U> Bucket<U> {\n    pub fn run(&self) { self.fetch_value(); }\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "run",
                expected: json!([
                    {"filePath": "run.rs", "startLine": 3, "edges": [
                        {"filePath": "run.rs", "symbol": {"name": "fetch_value", "filePath": "read.rs", "startLine": 3}},
                    ]},
                ]),
                file: Some("run.rs"),
                short_names: true,
                definition_line: Some(3),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "fetch_value",
                expected: json!([
                    {"filePath": "read.rs", "startLine": 3, "edges": [
                        {"filePath": "run.rs", "symbol": {"name": "run", "filePath": "run.rs", "startLine": 3}},
                    ]},
                ]),
                file: Some("read.rs"),
                short_names: true,
                definition_line: Some(3),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}
